use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::PathBuf;

use compresstimator::Compresstimator;
use crossbeam_channel::{Receiver, Sender};
use filetime::FileTime;
use winapi::um::processthreadsapi::{GetCurrentThread, SetThreadPriority};
use winapi::um::winbase::{
    THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_HIGHEST,
    THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL,
};
use winapi::um::winnt::{
    FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_ENCRYPTED, FILE_ATTRIBUTE_OFFLINE,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE,
    FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY,
};

use crate::background::{Background, ControlToken};
use crate::compact::{self, BackingState, Compression};
use crate::config::{AnalysisMode, CompressionPriority};
use crate::persistence::config;
use crate::policy::{round_up_to_cluster, sample_is_candidate, savings_worthwhile};

#[derive(Debug, Clone)]
pub struct CompressionJob {
    pub path: PathBuf,
    pub root: PathBuf,
    pub expected_path: PathBuf,
    pub content_len: u64,
    pub modified_time: u64,
    pub estimate_valid: bool,
    pub estimated_ratio: f32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FileDisposition {
    Compressed,
    Decompressed,
    NotBeneficial,
    Skipped,
}

/// Measurements and the final fingerprint come from the protected file handle.
/// In particular, the caller must not attach a failed attempt to a replacement
/// file by rereading its path after this worker releases the handle.
#[derive(Debug, Clone)]
pub struct FileOutcome {
    pub disposition: FileDisposition,
    pub original_physical_size: u64,
    pub physical_size: u64,
    pub content_len: u64,
    pub modified_time: u64,
    pub compression: Option<Compression>,
}

#[derive(Debug)]
pub struct BackgroundCompactor {
    compression: Option<Compression>,
    min_savings_percent: f32,
    analysis_mode: AnalysisMode,
    cluster_size: u64,
    files_in: Receiver<CompressionJob>,
    files_out: Sender<(PathBuf, io::Result<FileOutcome>)>,
}

impl BackgroundCompactor {
    pub fn new(
        compression: Option<Compression>,
        min_savings_percent: f32,
        analysis_mode: AnalysisMode,
        cluster_size: u64,
        files_in: Receiver<CompressionJob>,
        files_out: Sender<(PathBuf, io::Result<FileOutcome>)>,
    ) -> Self {
        Self {
            compression,
            min_savings_percent,
            analysis_mode,
            cluster_size: cluster_size.max(1),
            files_in,
            files_out,
        }
    }
}

fn thread_priority_value(priority: CompressionPriority) -> i32 {
    match priority {
        CompressionPriority::Lowest => THREAD_PRIORITY_LOWEST as i32,
        CompressionPriority::BelowNormal => THREAD_PRIORITY_BELOW_NORMAL as i32,
        CompressionPriority::Normal => THREAD_PRIORITY_NORMAL as i32,
        CompressionPriority::AboveNormal => THREAD_PRIORITY_ABOVE_NORMAL as i32,
        CompressionPriority::Highest => THREAD_PRIORITY_HIGHEST as i32,
    }
}

fn reuse_estimate(job: &CompressionJob, content_len: u64, modified_time: u64) -> Option<f32> {
    if job.estimate_valid
        && job.content_len == content_len
        && job.modified_time == modified_time
        && job.estimated_ratio.is_finite()
        && (0.0..=1.0).contains(&job.estimated_ratio)
    {
        Some(job.estimated_ratio)
    } else {
        None
    }
}

fn unsafe_file(attributes: u32, is_file: bool, links: u32, backing: BackingState) -> bool {
    let always_excluded = FILE_ATTRIBUTE_COMPRESSED
        | FILE_ATTRIBUTE_ENCRYPTED
        | FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_SYSTEM
        | FILE_ATTRIBUTE_TEMPORARY;
    !is_file
        || links != 1
        || attributes & always_excluded != 0
        || backing == BackingState::Unsupported
        || (backing == BackingState::None
            && attributes & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_REPARSE_POINT) != 0)
}

fn known_compression(backing: BackingState) -> Option<Compression> {
    match backing {
        BackingState::Wof(algorithm) => Some(algorithm),
        _ => None,
    }
}

/// A WOF expansion can need room for the entire uncompressed stream before the
/// compressed backing is released. This conservative check avoids starting an
/// expansion already known not to fit. It is a snapshot, not a reservation;
/// unrelated processes can still exhaust the volume while Windows works.
fn check_expansion_space(
    file: &std::fs::File,
    content_len: u64,
    cluster_size: u64,
) -> io::Result<()> {
    let path = compact::final_path_handle(file)?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unable to locate the file's volume",
        )
    })?;
    let available = fs2::available_space(parent)?;
    let required = round_up_to_cluster(content_len, cluster_size);
    if available < required {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "Restoring the uncompressed file needs up to {} free bytes; Windows reports {}. The file remains WOF compressed",
                required, available
            ),
        ));
    }
    Ok(())
}

fn restore_uncompressed(
    file: &std::fs::File,
    content_len: u64,
    cluster_size: u64,
) -> io::Result<u64> {
    match compact::detect_backing_handle(file)? {
        BackingState::Wof(_) => {
            check_expansion_space(file, content_len, cluster_size)?;
            compact::uncompress_file_handle(file)?;
        }
        BackingState::None => {}
        BackingState::Unsupported => {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Unexpected external backing; restoration was not attempted",
            ));
        }
    }
    if compact::detect_backing_handle(file)? != BackingState::None {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Windows did not remove WOF backing during restoration",
        ));
    }
    if file.metadata()?.len() != content_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "The logical file length changed during restoration; inspect this file before retrying",
        ));
    }
    compact::allocated_size_handle(file)
}

fn failed_with_restoration(
    file: &std::fs::File,
    content_len: u64,
    cluster_size: u64,
    cause: io::Error,
) -> io::Error {
    match restore_uncompressed(file, content_len, cluster_size) {
        Ok(_) => io::Error::new(cause.kind(), format!("{}; the file was restored to uncompressed form", cause)),
        Err(restore_error) => io::Error::new(
            io::ErrorKind::Other,
            format!(
                "{}; restoring the uncompressed file also failed: {}. The file may still be WOF compressed; rescan before retrying",
                cause, restore_error
            ),
        ),
    }
}

fn compact_and_measure(
    file: &std::fs::File,
    compression: Compression,
    before: u64,
    content_len: u64,
    min_savings_percent: f32,
    cluster_size: u64,
) -> io::Result<(FileDisposition, u64, Option<Compression>)> {
    let attempt = compact::compress_file_handle(file, compression);
    // Inspect the state even when the API reports failure. Returning a bare
    // error must not hide a representation that Windows already changed.
    let backing = compact::detect_backing_handle(file).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("Unable to verify file backing after compression: {}. Its state is uncertain; rescan before retrying", error),
        )
    })?;
    match backing {
        BackingState::None => {
            return match attempt {
                Ok(false) => Ok((
                    FileDisposition::NotBeneficial,
                    compact::allocated_size_handle(file)?,
                    None,
                )),
                Ok(true) => Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Windows reported compression success but the file has no WOF backing",
                )),
                Err(error) => Err(error),
            };
        }
        BackingState::Unsupported => {
            return Err(io::Error::new(io::ErrorKind::Other, "Unexpected external backing after compression; the file was left untouched for inspection"));
        }
        BackingState::Wof(actual) if actual != compression => {
            let error = io::Error::new(
                io::ErrorKind::Other,
                "Windows applied a different compression algorithm than requested",
            );
            return Err(failed_with_restoration(
                file,
                content_len,
                cluster_size,
                error,
            ));
        }
        BackingState::Wof(_) => {}
    }
    if let Err(error) = attempt {
        return Err(failed_with_restoration(
            file,
            content_len,
            cluster_size,
            error,
        ));
    }
    let after = match compact::allocated_size_handle(file) {
        Ok(size) => size,
        Err(error) => {
            return Err(failed_with_restoration(
                file,
                content_len,
                cluster_size,
                error,
            ))
        }
    };
    let after_metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(failed_with_restoration(
                file,
                content_len,
                cluster_size,
                error,
            ))
        }
    };
    if after_metadata.len() != content_len {
        let error = io::Error::new(
            io::ErrorKind::InvalidData,
            "The logical file length changed during compression",
        );
        return Err(failed_with_restoration(
            file,
            content_len,
            cluster_size,
            error,
        ));
    }
    if savings_worthwhile(before, after, min_savings_percent, cluster_size) {
        Ok((FileDisposition::Compressed, after, Some(compression)))
    } else {
        match restore_uncompressed(file, content_len, cluster_size) {
            Ok(restored) => Ok((FileDisposition::NotBeneficial, restored, None)),
            Err(error) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "The measured saving ({} to {} bytes) did not meet the retention threshold, but restoration failed: {}. The file may still be WOF compressed; rescan before retrying",
                    before, after, error
                ),
            )),
        }
    }
}

fn handle_file(
    job: &CompressionJob,
    compression: Option<Compression>,
    min_savings_percent: f32,
    analysis_mode: AnalysisMode,
    cluster_size: u64,
) -> io::Result<FileOutcome> {
    if !min_savings_percent.is_finite() || !(0.0..=100.0).contains(&min_savings_percent) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Minimum savings must be between 0 and 100%",
        ));
    }
    let cluster_size = cluster_size.max(1);
    // OPEN_REPARSE_POINT prevents following a last-component link. Denying
    // FILE_SHARE_WRITE and FILE_SHARE_DELETE excludes writers and replacements
    // while we inspect and transform this identity. An additional Windows
    // byte-range lock would also block our own second handle to the WOF view.
    let handle = compact::open_for_wof(&job.path)?;
    // A parent directory can become a junction after analysis. Validate the
    // opened identity against the canonical root and exact path captured during
    // that scan. A redirect within the same root can still lead to an excluded
    // or DirectStorage-protected subtree, so containment alone is insufficient.
    let final_path = compact::final_path_handle(&handle)?;
    if !crate::directstorage::is_under(&final_path, &job.root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The opened file is outside the folder that was analysed; no compression change was attempted",
        ));
    }
    if !crate::directstorage::same_path(&final_path, &job.expected_path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The opened file no longer matches the path that was analysed; no compression change was attempted",
        ));
    }
    let identity = compact::file_identity_handle(&handle)?;
    let _operation_lock = compact::lock_wof_identity(&identity)?;
    // Link count is metadata too; refresh it after cooperative exclusion.
    let identity = compact::file_identity_handle(&handle)?;
    let meta = handle.metadata()?;
    let backing = compact::detect_backing_handle(&handle)?;
    let before = compact::allocated_size_handle(&handle)?;
    let ratio_limit = 1.0 - min_savings_percent / 100.0;

    let operation = (|| -> io::Result<(FileDisposition, u64, Option<Compression>)> {
        if unsafe_file(
            meta.file_attributes(),
            meta.is_file(),
            identity.links,
            backing,
        ) {
            return Ok((FileDisposition::Skipped, before, known_compression(backing)));
        }
        match compression {
            Some(algorithm) => {
                if backing != BackingState::None
                    || meta.len() <= cluster_size
                    || before <= cluster_size
                {
                    return Ok((FileDisposition::Skipped, before, known_compression(backing)));
                }
                if analysis_mode == AnalysisMode::Sampled {
                    let ratio = match reuse_estimate(job, meta.len(), meta.last_write_time()) {
                        Some(ratio) => ratio,
                        None => Compresstimator::with_block_size(8192)
                            .compresstimate(&handle, meta.len())?,
                    };
                    if !sample_is_candidate(ratio, ratio_limit) {
                        // A sample is not proof that the selected WOF algorithm
                        // cannot save space. Do not persist this as a failed WOF
                        // attempt, especially when the file changed after scan.
                        return Ok((FileDisposition::Skipped, before, None));
                    }
                }
                compact_and_measure(
                    &handle,
                    algorithm,
                    before,
                    meta.len(),
                    min_savings_percent,
                    cluster_size,
                )
            }
            None => {
                if !matches!(backing, BackingState::Wof(_)) {
                    return Ok((FileDisposition::Skipped, before, None));
                }
                let restored = restore_uncompressed(&handle, meta.len(), cluster_size)?;
                Ok((FileDisposition::Decompressed, restored, None))
            }
        }
    })();

    // Restore timestamps through the same protected identity, including failed
    // attempts and rollbacks. A failure here is visible instead of silently
    // claiming that metadata preservation succeeded.
    let restored_times = filetime::set_file_handle_times(
        &handle,
        Some(FileTime::from_last_access_time(&meta)),
        Some(FileTime::from_last_modification_time(&meta)),
    );
    let (disposition, physical_size, final_compression) = match (operation, restored_times) {
        (Ok(value), Ok(())) => value,
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(error)) => {
            return Err(io::Error::new(error.kind(), format!("The file operation finished, but restoring timestamps failed: {}. Rescan to inspect its current state", error)));
        }
        (Err(error), Err(time_error)) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "{}; restoring timestamps also failed: {}",
                    error, time_error
                ),
            ));
        }
    };
    let final_meta = handle.metadata()?;
    Ok(FileOutcome {
        disposition,
        original_physical_size: before,
        physical_size,
        content_len: final_meta.len(),
        modified_time: final_meta.last_write_time(),
        compression: final_compression,
    })
}

impl Background for BackgroundCompactor {
    type Output = ();
    type Status = ();

    fn run(self, control: &ControlToken<Self::Status>) -> Self::Output {
        // Only this worker changes priority; analysis and the GUI stay at their
        // normal priority. Failure to change scheduling is non-fatal.
        let priority = config().read().unwrap().current().compression_priority;
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), thread_priority_value(priority));
        }
        for job in &self.files_in {
            if control.is_cancelled_with_pause() {
                break;
            }
            let path = job.path.clone();
            let ret = handle_file(
                &job,
                self.compression,
                self.min_savings_percent,
                self.analysis_mode,
                self.cluster_size,
            );
            if self.files_out.send((path, ret)).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_for(path: PathBuf, ratio: f32) -> CompressionJob {
        let meta = std::fs::metadata(&path).unwrap();
        CompressionJob {
            root: path.parent().unwrap().canonicalize().unwrap(),
            expected_path: path.canonicalize().unwrap(),
            path,
            content_len: meta.len(),
            modified_time: meta.last_write_time(),
            estimate_valid: true,
            estimated_ratio: ratio,
        }
    }

    #[test]
    fn estimate_reuse_requires_matching_fingerprint_and_valid_ratio() {
        let mut job = CompressionJob {
            path: PathBuf::from("test.bin"),
            root: PathBuf::from(r"C:\Games"),
            expected_path: PathBuf::from("test.bin"),
            content_len: 8192,
            modified_time: 1234,
            estimate_valid: true,
            estimated_ratio: 0.5,
        };
        assert_eq!(Some(0.5), reuse_estimate(&job, 8192, 1234));
        assert_eq!(None, reuse_estimate(&job, 4096, 1234));
        assert_eq!(None, reuse_estimate(&job, 8192, 5678));
        job.estimate_valid = false;
        assert_eq!(None, reuse_estimate(&job, 8192, 1234));
        job.estimate_valid = true;
        for ratio in [f32::NAN, f32::INFINITY, -0.5, 1.01] {
            job.estimated_ratio = ratio;
            assert_eq!(None, reuse_estimate(&job, 8192, 1234));
        }
    }

    #[test]
    fn native_wof_is_distinguished_from_other_special_files_and_hardlinks() {
        let wof = BackingState::Wof(Compression::Lzx);
        let sparse_reparse = FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_REPARSE_POINT;
        assert!(!unsafe_file(sparse_reparse, true, 1, wof));
        assert!(unsafe_file(sparse_reparse, true, 1, BackingState::None));
        assert!(unsafe_file(
            sparse_reparse,
            true,
            1,
            BackingState::Unsupported
        ));
        assert!(unsafe_file(0, false, 1, BackingState::None));
        assert!(unsafe_file(0, true, 2, BackingState::None));
        assert!(unsafe_file(0, true, 0, BackingState::None));
        for attribute in [
            FILE_ATTRIBUTE_COMPRESSED,
            FILE_ATTRIBUTE_ENCRYPTED,
            FILE_ATTRIBUTE_OFFLINE,
            FILE_ATTRIBUTE_READONLY,
            FILE_ATTRIBUTE_SYSTEM,
            FILE_ATTRIBUTE_TEMPORARY,
        ] {
            assert!(unsafe_file(attribute, true, 1, BackingState::None));
            assert!(unsafe_file(attribute | sparse_reparse, true, 1, wof));
        }
    }

    #[test]
    fn compression_priority_maps_to_safe_windows_levels() {
        for (priority, expected) in [
            (CompressionPriority::Lowest, THREAD_PRIORITY_LOWEST),
            (
                CompressionPriority::BelowNormal,
                THREAD_PRIORITY_BELOW_NORMAL,
            ),
            (CompressionPriority::Normal, THREAD_PRIORITY_NORMAL),
            (
                CompressionPriority::AboveNormal,
                THREAD_PRIORITY_ABOVE_NORMAL,
            ),
            (CompressionPriority::Highest, THREAD_PRIORITY_HIGHEST),
        ] {
            assert_eq!(expected as i32, thread_priority_value(priority));
        }
    }

    #[test]
    #[ignore = "Requires Windows 10+ and a local NTFS temporary directory; run with --ignored"]
    fn thorough_mode_bypasses_the_sample_and_retention_uses_measured_savings() {
        let dir = tempdir::TempDir::new("compactor-measured-retention").unwrap();
        let path = dir.path().join("fixture.bin");
        let data =
            b"A repeatable WOF compression fixture with enough repeated content.\n".repeat(32768);
        std::fs::write(&path, &data).unwrap();
        assert!(compact::file_supports_compression(&path).unwrap());
        let time = FileTime::from_unix_time(1_600_000_000, 123_400_000);
        filetime::set_file_mtime(&path, time).unwrap();
        let job = job_for(path.clone(), 1.0);
        let before =
            compact::allocated_size_handle(&compact::open_for_wof(&path).unwrap()).unwrap();

        let sampled = handle_file(
            &job,
            Some(Compression::Lzx),
            1.0,
            AnalysisMode::Sampled,
            4096,
        )
        .unwrap();
        assert_eq!(FileDisposition::Skipped, sampled.disposition);
        assert_eq!(None, sampled.compression);
        assert_eq!(before, sampled.physical_size);

        let thorough = handle_file(
            &job,
            Some(Compression::Lzx),
            1.0,
            AnalysisMode::Thorough,
            4096,
        )
        .unwrap();
        assert_eq!(FileDisposition::Compressed, thorough.disposition);
        assert_eq!(Some(Compression::Lzx), thorough.compression);
        assert!(savings_worthwhile(
            before,
            thorough.physical_size,
            1.0,
            4096
        ));
        assert_eq!(job.modified_time, thorough.modified_time);
        assert_eq!(data, std::fs::read(&path).unwrap());

        let existing = handle_file(
            &job_for(path.clone(), 0.1),
            Some(Compression::Xpress4k),
            0.0,
            AnalysisMode::Thorough,
            4096,
        )
        .unwrap();
        assert_eq!(FileDisposition::Skipped, existing.disposition);
        assert_eq!(Some(Compression::Lzx), existing.compression);
        assert_eq!(thorough.physical_size, existing.physical_size);

        let restored = handle_file(
            &job_for(path.clone(), 0.1),
            None,
            1.0,
            AnalysisMode::Thorough,
            4096,
        )
        .unwrap();
        assert_eq!(FileDisposition::Decompressed, restored.disposition);
        assert_eq!(before, restored.physical_size);
        assert_eq!(data, std::fs::read(&path).unwrap());
        assert_eq!(job.modified_time, restored.modified_time);

        // No nonempty file can save 100%. Force the real compress/measure/
        // restore path and prove it returns to the original allocation.
        let rejected = handle_file(
            &job_for(path.clone(), 0.0),
            Some(Compression::Lzx),
            100.0,
            AnalysisMode::Thorough,
            4096,
        )
        .unwrap();
        assert_eq!(FileDisposition::NotBeneficial, rejected.disposition);
        assert_eq!(None, rejected.compression);
        assert_eq!(before, rejected.physical_size);
        assert_eq!(job.modified_time, rejected.modified_time);
        assert_eq!(data, std::fs::read(&path).unwrap());
    }

    #[test]
    #[ignore = "Requires Windows 10+ and a local NTFS temporary directory; run with --ignored"]
    fn out_of_scope_files_active_writers_and_hardlinks_are_not_transformed() {
        let dir = tempdir::TempDir::new("compactor-protected-files").unwrap();
        let path = dir.path().join("fixture.bin");
        let data = vec![b'x'; 65536];
        std::fs::write(&path, &data).unwrap();
        assert!(compact::file_supports_compression(&path).unwrap());
        let job = job_for(path.clone(), 0.0);
        let other_root = tempdir::TempDir::new("compactor-unrelated-root").unwrap();
        let mut outside = job.clone();
        outside.root = other_root.path().canonicalize().unwrap();
        let scope_error = handle_file(
            &outside,
            Some(Compression::Lzx),
            1.0,
            AnalysisMode::Thorough,
            4096,
        )
        .unwrap_err();
        assert_eq!(io::ErrorKind::PermissionDenied, scope_error.kind());
        assert_eq!(None, compact::detect_compression(path.as_os_str()).unwrap());
        assert_eq!(data, std::fs::read(&path).unwrap());
        let other_path = dir.path().join("another-subtree-file.bin");
        std::fs::write(&other_path, b"preserve the other path").unwrap();
        let mut redirected_within_root = job.clone();
        redirected_within_root.expected_path = other_path.canonicalize().unwrap();
        let redirect_error = handle_file(
            &redirected_within_root,
            Some(Compression::Lzx),
            1.0,
            AnalysisMode::Thorough,
            4096,
        )
        .unwrap_err();
        assert_eq!(io::ErrorKind::PermissionDenied, redirect_error.kind());
        assert_eq!(None, compact::detect_compression(path.as_os_str()).unwrap());
        assert_eq!(data, std::fs::read(&path).unwrap());
        assert_eq!(
            b"preserve the other path".to_vec(),
            std::fs::read(&other_path).unwrap()
        );
        let writer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(handle_file(
            &job,
            Some(Compression::Lzx),
            1.0,
            AnalysisMode::Thorough,
            4096
        )
        .is_err());
        drop(writer);
        let alias = dir.path().join("alias.bin");
        std::fs::hard_link(&path, &alias).unwrap();
        let skipped = handle_file(
            &job,
            Some(Compression::Lzx),
            1.0,
            AnalysisMode::Thorough,
            4096,
        )
        .unwrap();
        assert_eq!(FileDisposition::Skipped, skipped.disposition);
        assert_eq!(None, skipped.compression);
        assert_eq!(data, std::fs::read(&path).unwrap());
        assert_eq!(data, std::fs::read(&alias).unwrap());
    }
}
