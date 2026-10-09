use std::collections::VecDeque;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use std::thread;
use std::time::{Duration, Instant};

use compresstimator::Compresstimator;
use crossbeam_channel::{unbounded, RecvTimeoutError};
use filesize::PathExt;
use globset::GlobSet;
use serde_derive::Serialize;
use walkdir::WalkDir;
use winapi::shared::minwindef::DWORD;
use winapi::shared::ntdef::PVOID;
use winapi::um::fileapi::{GetDiskFreeSpaceW, GetVolumeInformationW};
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::sysinfoapi::GetLogicalProcessorInformation;
use winapi::um::winioctl::{
    PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, IOCTL_STORAGE_QUERY_PROPERTY,
    STORAGE_PROPERTY_QUERY,
};
use winapi::um::winnt::{
    RelationProcessorCore, BOOLEAN, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_ENCRYPTED,
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SPARSE_FILE, FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, HANDLE, SYSTEM_LOGICAL_PROCESSOR_INFORMATION,
};

use crate::background::{Background, ControlToken};
use crate::compact::{self, Compression};
use crate::config::{AnalysisMode, Config};
use crate::persistence::{incompressible_key, pathdb};
use crate::policy::{estimated_allocation, sample_is_candidate};

const AUTO_MAX_ANALYSIS_THREADS: usize = 8;
const AUTO_MAX_COMPRESSION_THREADS: usize = 16;
const AUTO_MAX_LZX_THREADS: usize = 2;
const MAX_CONFIGURED_THREADS: usize = 16;
const ESTIMATE_BATCH_FILES: usize = 4096;
const HDD_SAMPLE_WINDOWS: u64 = 4;
const HDD_SAMPLE_CHUNK: usize = 128 * 1024;
const ESTIMATOR_BLOCK_SIZE: usize = 8192;
const FALLBACK_CLUSTER_SIZE: u64 = 4096;

#[derive(Debug, Clone, Serialize)]
pub struct FileInfo {
    pub path: PathBuf,
    pub content_len: u64,
    pub modified_time: u64,
    pub estimate_valid: bool,
    pub estimated_ratio: f32,
    pub logical_size: u64,
    pub physical_size: u64,
    pub estimated_physical_size: u64,
    pub compression: Option<Compression>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GroupInfo {
    pub files: VecDeque<FileInfo>,
    pub count: usize,
    pub logical_size: u64,
    pub physical_size: u64,
    pub estimated_physical_size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderInfo {
    pub path: PathBuf,
    pub canonical_root: PathBuf,
    pub logical_size: u64,
    pub physical_size: u64,
    pub compressible: GroupInfo,
    pub compressed: GroupInfo,
    pub skipped: GroupInfo,
    pub direct_storage: bool,
    pub cluster_size: u64,
    pub scan_errors: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct FolderSummary {
    pub logical_size: u64,
    pub physical_size: u64,
    pub compressible: GroupSummary,
    pub compressed: GroupSummary,
    pub skipped: GroupSummary,
    pub direct_storage: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GroupSummary {
    pub count: usize,
    pub logical_size: u64,
    pub physical_size: u64,
    pub estimated_physical_size: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum FileKind {
    Compressed,
    Compressible,
    Skipped,
}

impl FolderInfo {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_owned(),
            canonical_root: path.as_ref().to_owned(),
            logical_size: 0,
            physical_size: 0,
            compressible: GroupInfo::default(),
            compressed: GroupInfo::default(),
            skipped: GroupInfo::default(),
            direct_storage: false,
            cluster_size: FALLBACK_CLUSTER_SIZE,
            scan_errors: 0,
        }
    }

    pub fn summary(&self) -> FolderSummary {
        FolderSummary {
            logical_size: self.logical_size,
            physical_size: self.physical_size,
            compressible: self.compressible.summary(),
            compressed: self.compressed.summary(),
            skipped: self.skipped.summary(),
            direct_storage: self.direct_storage,
        }
    }

    pub fn len(&self, kind: FileKind) -> usize {
        match kind {
            FileKind::Compressible => self.compressible.count,
            FileKind::Compressed => self.compressed.count,
            FileKind::Skipped => self.skipped.count,
        }
    }

    pub fn pop(&mut self, kind: FileKind) -> Option<FileInfo> {
        let ret = match kind {
            FileKind::Compressible => self.compressible.pop(),
            FileKind::Compressed => self.compressed.pop(),
            FileKind::Skipped => None,
        };

        if let Some(fi) = ret {
            self.logical_size = self.logical_size.saturating_sub(fi.logical_size);
            self.physical_size = self.physical_size.saturating_sub(fi.physical_size);
            Some(fi)
        } else {
            None
        }
    }

    pub fn push(&mut self, kind: FileKind, fi: FileInfo) {
        self.logical_size = self.logical_size.saturating_add(fi.logical_size);
        self.physical_size = self.physical_size.saturating_add(fi.physical_size);

        match kind {
            FileKind::Compressible => self.compressible.push(fi, true),
            FileKind::Compressed => self.compressed.push(fi, true),
            // Skipped paths are not needed for any operation or viewer. Keeping
            // only their counters/totals prevents huge excluded trees from
            // consuming memory merely to remember every filename.
            FileKind::Skipped => self.skipped.push(fi, false),
        }
    }
}

impl GroupInfo {
    pub fn summary(&self) -> GroupSummary {
        GroupSummary {
            count: self.count,
            logical_size: self.logical_size,
            physical_size: self.physical_size,
            estimated_physical_size: self.estimated_physical_size,
        }
    }

    fn pop(&mut self) -> Option<FileInfo> {
        let ret = self.files.pop_front();

        if let Some(fi) = ret {
            self.count = self.count.saturating_sub(1);
            self.logical_size = self.logical_size.saturating_sub(fi.logical_size);
            self.physical_size = self.physical_size.saturating_sub(fi.physical_size);
            self.estimated_physical_size = self
                .estimated_physical_size
                .saturating_sub(fi.estimated_physical_size);
            Some(fi)
        } else {
            None
        }
    }

    fn push(&mut self, fi: FileInfo, retain: bool) {
        self.count = self.count.saturating_add(1);
        self.logical_size = self.logical_size.saturating_add(fi.logical_size);
        self.physical_size = self.physical_size.saturating_add(fi.physical_size);
        self.estimated_physical_size = self
            .estimated_physical_size
            .saturating_add(fi.estimated_physical_size);
        if retain {
            self.files.push_back(fi);
        }
    }
}

#[derive(Debug)]
pub struct FolderScan {
    path: PathBuf,
    canonical_root: PathBuf,
    excludes: GlobSet,
    config: Config,
}

impl FolderScan {
    pub fn new<P: AsRef<Path>>(path: P, config: &Config) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            path: path.as_ref().to_path_buf(),
            canonical_root: std::fs::canonicalize(path.as_ref())
                .map_err(|error| format!("Unable to resolve scan root: {}", error))?,
            excludes: config.globset()?,
            config: config.clone(),
        })
    }
}

#[derive(Debug)]
struct EstimateCandidate {
    file: FileInfo,
    content_len: u64,
}

#[repr(C)]
struct DeviceSeekPenaltyDescriptor {
    version: DWORD,
    size: DWORD,
    incurs_seek_penalty: BOOLEAN,
}

fn wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn volume_root_path(path: &Path) -> Option<PathBuf> {
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some(PathBuf::from(format!(r"{}:\", letter as char)))
            }
            _ => None,
        },
        _ => None,
    }
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn path_is_same_or_child(path: &Path, parent: &Path) -> bool {
    let path = normalized_path(path);
    let parent = normalized_path(parent);
    path == parent
        || path
            .strip_prefix(&parent)
            .map(|rest| rest.starts_with('\\'))
            .unwrap_or(false)
}

fn volume_filesystem(path: &Path) -> io::Result<String> {
    let root = volume_root_path(path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a local drive path"))?;
    let root = wide_null(&root);
    let mut filesystem = [0u16; 32];

    let ret = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as DWORD,
        )
    };

    if ret == 0 {
        return Err(io::Error::last_os_error());
    }

    let len = filesystem
        .iter()
        .position(|&value| value == 0)
        .unwrap_or(filesystem.len());
    Ok(String::from_utf16_lossy(&filesystem[..len]))
}

fn cluster_size_for_path(path: &Path) -> u64 {
    let Some(root) = volume_root_path(path) else {
        return FALLBACK_CLUSTER_SIZE;
    };
    let root = wide_null(&root);
    let mut sectors_per_cluster = 0u32;
    let mut bytes_per_sector = 0u32;
    let mut free_clusters = 0u32;
    let mut total_clusters = 0u32;

    let ret = unsafe {
        GetDiskFreeSpaceW(
            root.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };

    if ret == 0 {
        FALLBACK_CLUSTER_SIZE
    } else {
        (sectors_per_cluster as u64)
            .saturating_mul(bytes_per_sector as u64)
            .max(1)
    }
}

pub fn validate_target_path(path: &Path) -> Result<(), String> {
    // Refuse aliases through junctions/mount points, including ancestor aliases.
    // A textual C:\Games path must not redirect to Windows or another volume.
    if !path.is_absolute() {
        return Err("Select an absolute local folder path.".to_string());
    }
    for ancestor in path.ancestors() {
        if let Ok(metadata) = std::fs::symlink_metadata(ancestor) {
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err("Select the actual folder instead of a junction, symbolic link or mounted-folder alias.".to_string());
            }
        }
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("Unable to resolve the selected folder: {}", error))?;
    let path = canonical.as_path();
    if !path.exists() {
        return Err("The selected folder no longer exists.".to_string());
    }
    if !path.is_dir() {
        return Err("The selected path is not a folder.".to_string());
    }

    let root = volume_root_path(path).ok_or_else(|| {
        "Only local drive folders are supported. Network/UNC paths are not valid WOF targets."
            .to_string()
    })?;

    if normalized_path(path) == normalized_path(&root) {
        return Err("Select a folder rather than an entire drive root.".to_string());
    }

    let filesystem = volume_filesystem(path)
        .map_err(|err| format!("Unable to query the target filesystem: {}", err))?;
    if !filesystem.eq_ignore_ascii_case("NTFS") {
        return Err(format!(
            "Windows WOF compression requires NTFS. This folder is on {}.",
            if filesystem.is_empty() {
                "an unknown filesystem"
            } else {
                &filesystem
            }
        ));
    }

    if let Some(system_root) = std::env::var_os("SystemRoot") {
        if path_is_same_or_child(path, &PathBuf::from(system_root)) {
            return Err("The active Windows directory is protected from compression.".to_string());
        }
    }

    let root_normalized = normalized_path(&root);
    let path_normalized = normalized_path(path);
    if let Some(relative) = path_normalized
        .strip_prefix(&root_normalized)
        .map(|value| value.trim_start_matches('\\'))
    {
        let first = relative.split('\\').next().unwrap_or_default();
        if first.eq_ignore_ascii_case("system volume information")
            || first.eq_ignore_ascii_case("recovery")
            || first.starts_with('$')
        {
            return Err("The selected folder is a protected Windows-managed path.".to_string());
        }
    }

    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.file_attributes() & FILE_ATTRIBUTE_COMPRESSED != 0 {
            return Err(
                "The selected folder uses legacy NTFS (LZNT1) compression. Disable 'Compress contents to save disk space' on the folder before using WOF compression."
                    .to_string(),
            );
        }
    }

    Ok(())
}

fn volume_device_path(path: &Path) -> Option<PathBuf> {
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some(PathBuf::from(format!(r"\\.\{}:", letter as char)))
            }
            _ => None,
        },
        _ => None,
    }
}

fn volume_incurs_seek_penalty(path: &Path) -> Option<bool> {
    let volume_path = volume_device_path(path)?;
    let volume = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(volume_path)
        .ok()?;

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut descriptor = DeviceSeekPenaltyDescriptor {
        version: 0,
        size: 0,
        incurs_seek_penalty: 0,
    };
    let mut bytes_returned: DWORD = 0;

    let ret = unsafe {
        DeviceIoControl(
            volume.as_raw_handle() as HANDLE,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as PVOID,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as DWORD,
            &mut descriptor as *mut _ as PVOID,
            std::mem::size_of::<DeviceSeekPenaltyDescriptor>() as DWORD,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    if ret != 0 {
        Some(descriptor.incurs_seek_penalty != 0)
    } else {
        None
    }
}

fn physical_core_count() -> Option<usize> {
    let entry_size = std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION>();
    if entry_size == 0 {
        return None;
    }

    let mut bytes: DWORD = 0;
    unsafe {
        // The first call is expected to fail with an insufficient-buffer result
        // while returning the required byte count.
        let _ = GetLogicalProcessorInformation(std::ptr::null_mut(), &mut bytes);
    }
    if bytes == 0 {
        return None;
    }

    loop {
        let capacity = ((bytes as usize + entry_size - 1) / entry_size).max(1);
        let mut entries: Vec<SYSTEM_LOGICAL_PROCESSOR_INFORMATION> = Vec::with_capacity(capacity);
        let mut buffer_bytes = (capacity * entry_size) as DWORD;

        let ret =
            unsafe { GetLogicalProcessorInformation(entries.as_mut_ptr(), &mut buffer_bytes) };

        if ret != 0 {
            let len = (buffer_bytes as usize / entry_size).min(capacity);
            unsafe {
                entries.set_len(len);
            }
            let cores = entries
                .iter()
                .filter(|entry| entry.Relationship == RelationProcessorCore)
                .count();
            return if cores == 0 { None } else { Some(cores) };
        }

        // Processor topology can theoretically change between calls. Retry only
        // when Windows reports that the required buffer grew; otherwise fall
        // back to the logical count rather than blocking compression.
        if buffer_bytes as usize <= capacity * entry_size {
            return None;
        }
        bytes = buffer_bytes;
    }
}

fn configured_thread_count(cpus: usize, max_threads: usize, auto_max_threads: usize) -> usize {
    if max_threads == 0 {
        cpus.max(1).min(auto_max_threads)
    } else {
        max_threads.clamp(1, MAX_CONFIGURED_THREADS)
    }
}

fn worker_count_for_storage(
    cpus: usize,
    incurs_seek_penalty: Option<bool>,
    max_threads: usize,
    hdd_single_thread: bool,
    auto_max_threads: usize,
) -> usize {
    let configured = configured_thread_count(cpus, max_threads, auto_max_threads);

    match incurs_seek_penalty {
        Some(true) if hdd_single_thread => 1,
        Some(_) => configured,
        None => 1,
    }
}

fn compression_worker_count(
    logical_cpus: usize,
    physical_cpus: usize,
    incurs_seek_penalty: Option<bool>,
    compression: Compression,
    max_threads: usize,
    hdd_single_thread: bool,
) -> usize {
    let workers = worker_count_for_storage(
        logical_cpus,
        incurs_seek_penalty,
        max_threads,
        hdd_single_thread,
        AUTO_MAX_COMPRESSION_THREADS,
    );

    // Auto is conservative for LZX because a single WOF/LZX operation can be
    // CPU-heavy. An explicit manual limit is treated as an intentional override
    // so high-end systems can trade responsiveness for throughput if desired.
    if compression == Compression::Lzx && max_threads == 0 {
        if workers <= 1 || physical_cpus <= 4 {
            1
        } else {
            workers.min(AUTO_MAX_LZX_THREADS)
        }
    } else {
        workers
    }
}

pub fn compression_worker_count_for_path(
    path: &Path,
    compression: Compression,
    max_threads: usize,
    hdd_single_thread: bool,
) -> usize {
    let logical_cpus = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let physical_cpus = physical_core_count().unwrap_or(logical_cpus);
    compression_worker_count(
        logical_cpus,
        physical_cpus,
        volume_incurs_seek_penalty(path),
        compression,
        max_threads,
        hdd_single_thread,
    )
}

fn compresstimate_hdd(
    estimator: &Compresstimator,
    handle: &mut std::fs::File,
    len: u64,
    buffer: &mut Vec<u8>,
) -> io::Result<f32> {
    let chunk = len.min(HDD_SAMPLE_CHUNK as u64) as usize;

    if chunk == 0 {
        return Ok(1.0);
    }

    if len <= (HDD_SAMPLE_CHUNK as u64) * HDD_SAMPLE_WINDOWS {
        return estimator.compresstimate(handle, len);
    }

    buffer.resize(chunk, 0);
    let max_start = len.saturating_sub(chunk as u64);
    let mut weighted_ratio = 0.0f64;
    let mut total_sampled = 0u64;

    for sample in 0..HDD_SAMPLE_WINDOWS {
        let mut offset = max_start.saturating_mul(sample) / (HDD_SAMPLE_WINDOWS - 1);
        offset = (offset / ESTIMATOR_BLOCK_SIZE as u64) * ESTIMATOR_BLOCK_SIZE as u64;

        handle.seek(SeekFrom::Start(offset))?;
        handle.read_exact(&mut buffer[..chunk])?;

        let ratio = estimator.compresstimate(Cursor::new(&buffer[..chunk]), chunk as u64)?;
        weighted_ratio += ratio as f64 * chunk as f64;
        total_sampled = total_sampled.saturating_add(chunk as u64);
    }

    if total_sampled == 0 {
        Ok(1.0)
    } else {
        Ok((weighted_ratio / total_sampled as f64) as f32)
    }
}

fn estimate_file(
    estimator: &Compresstimator,
    path: &Path,
    len: u64,
    hdd: bool,
    buffer: &mut Vec<u8>,
) -> io::Result<f32> {
    let mut handle = std::fs::File::open(path)?;

    if hdd {
        compresstimate_hdd(estimator, &mut handle, len, buffer)
    } else {
        estimator.compresstimate(&mut handle, len)
    }
}

fn apply_estimate(
    ds: &mut FolderInfo,
    mut fi: FileInfo,
    result: io::Result<f32>,
    ratio_limit: f32,
    analysis_mode: AnalysisMode,
    cluster_size: u64,
) {
    match result {
        Ok(ratio) => {
            fi.estimate_valid = ratio.is_finite() && (0.0..=1.0).contains(&ratio);
            fi.estimated_ratio = if fi.estimate_valid { ratio } else { 1.0 };
            fi.estimated_physical_size =
                estimated_allocation(fi.logical_size, ratio, cluster_size, fi.physical_size);
            if analysis_mode == AnalysisMode::Thorough || sample_is_candidate(ratio, ratio_limit) {
                ds.push(FileKind::Compressible, fi);
            } else {
                ds.push(FileKind::Skipped, fi);
            }
        }
        Err(_) if analysis_mode == AnalysisMode::Thorough => {
            // Analysis is read-only and sampled; a transient read error is not
            // proof of incompressibility. The protected worker will retry once.
            ds.scan_errors += 1;
            ds.push(FileKind::Compressible, fi);
        }
        Err(_) => {
            ds.scan_errors += 1;
            ds.push(FileKind::Skipped, fi);
        }
    }
}

fn estimate_candidates(
    path: &Path,
    mut candidates: VecDeque<EstimateCandidate>,
    workers: usize,
    hdd: bool,
    ratio_limit: f32,
    analysis_mode: AnalysisMode,
    cluster_size: u64,
    control: &ControlToken<(PathBuf, FolderSummary)>,
    ds: &mut FolderInfo,
    last_status: &mut Instant,
) -> bool {
    if candidates.is_empty() {
        return true;
    }

    let workers = workers.min(candidates.len()).max(1);
    let (job_tx, job_rx) = unbounded::<EstimateCandidate>();
    let (result_tx, result_rx) = unbounded::<(FileInfo, io::Result<f32>)>();
    let mut handles = Vec::with_capacity(workers);

    for _ in 0..workers {
        let jobs = job_rx.clone();
        let results = result_tx.clone();
        let root = path.to_path_buf();

        handles.push(thread::spawn(move || {
            let estimator = Compresstimator::with_block_size(ESTIMATOR_BLOCK_SIZE);
            let mut buffer = Vec::new();

            while let Ok(candidate) = jobs.recv() {
                let file_path = root.join(&candidate.file.path);
                let result = estimate_file(
                    &estimator,
                    &file_path,
                    candidate.content_len,
                    hdd,
                    &mut buffer,
                );

                if results.send((candidate.file, result)).is_err() {
                    break;
                }
            }
        }));
    }

    drop(job_rx);
    drop(result_tx);

    let mut in_flight = 0usize;
    for _ in 0..workers {
        if let Some(candidate) = candidates.pop_front() {
            if job_tx.send(candidate).is_err() {
                break;
            }
            in_flight += 1;
        }
    }

    let mut cancelled = false;

    while in_flight > 0 {
        if control.is_cancelled_with_pause() {
            cancelled = true;
            break;
        }

        match result_rx.recv_timeout(Duration::from_millis(25)) {
            Ok((fi, result)) => {
                in_flight -= 1;
                let status_path = fi.path.clone();
                apply_estimate(ds, fi, result, ratio_limit, analysis_mode, cluster_size);

                if last_status.elapsed() >= Duration::from_millis(50) {
                    *last_status = Instant::now();
                    control.set_status((status_path, ds.summary()));
                }

                if let Some(candidate) = candidates.pop_front() {
                    if job_tx.send(candidate).is_ok() {
                        in_flight += 1;
                    } else {
                        cancelled = true;
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                cancelled = true;
                break;
            }
        }
    }

    drop(job_tx);

    for handle in handles {
        let _ = handle.join();
    }

    if cancelled {
        for (fi, result) in result_rx.try_iter() {
            apply_estimate(ds, fi, result, ratio_limit, analysis_mode, cluster_size);
        }

        for candidate in candidates {
            ds.push(FileKind::Skipped, candidate.file);
        }

        false
    } else {
        true
    }
}

fn is_direct_storage_runtime(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.eq_ignore_ascii_case("dstorage.dll")
                || name.eq_ignore_ascii_case("dstoragecore.dll")
        })
        .unwrap_or(false)
}

impl Background for FolderScan {
    type Output = Result<FolderInfo, FolderInfo>;
    type Status = (PathBuf, FolderSummary);

    fn run(self, control: &ControlToken<Self::Status>) -> Self::Output {
        let FolderScan {
            path,
            canonical_root,
            excludes,
            config,
        } = self;
        let ratio_limit = config.ratio_limit();
        let analysis_mode = config.analysis_mode;
        let mut ds = FolderInfo::new(&path);
        ds.canonical_root = canonical_root;
        let cpus = thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        let seek_penalty = volume_incurs_seek_penalty(&path);
        let analysis_workers = worker_count_for_storage(
            cpus,
            seek_penalty,
            config.max_threads,
            config.hdd_single_thread,
            AUTO_MAX_ANALYSIS_THREADS,
        );
        let is_hdd = matches!(seek_penalty, Some(true));
        let cluster_size = cluster_size_for_path(&path);
        ds.cluster_size = cluster_size;
        let mut candidates: VecDeque<EstimateCandidate> = VecDeque::new();
        let inline_estimator = if analysis_workers == 1 {
            Some(Compresstimator::with_block_size(ESTIMATOR_BLOCK_SIZE))
        } else {
            None
        };
        let mut inline_buffer = Vec::new();
        let incompressible = pathdb();
        let mut incompressible = incompressible.write().unwrap();
        let _ = incompressible.load();

        let mut last_status = Instant::now();

        let walker = WalkDir::new(&path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                // Reparse directories can redirect an otherwise safe target to
                // another volume or protected tree. Never descend into them.
                if e.file_type().is_dir() {
                    return e
                        .metadata()
                        .map(|md| {
                            md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
                                && !excludes.is_match(e.path())
                        })
                        .unwrap_or(false);
                }
                true
            });

        for (count, result) in walker.enumerate() {
            if control.is_cancelled_with_pause() {
                for candidate in candidates {
                    ds.push(FileKind::Skipped, candidate.file);
                }
                return Err(ds);
            }
            let entry = match result {
                Ok(entry) if entry.file_type().is_file() => entry,
                Ok(_) => continue,
                Err(_) => {
                    ds.scan_errors += 1;
                    continue;
                }
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    ds.scan_errors += 1;
                    continue;
                }
            };
            let physical = match entry.path().size_on_disk() {
                Ok(size) => size,
                Err(_) => {
                    ds.scan_errors += 1;
                    continue;
                }
            };
            let shortname = entry
                .path()
                .strip_prefix(&path)
                .unwrap_or_else(|_e| entry.path())
                .to_path_buf();

            if is_direct_storage_runtime(entry.path()) {
                ds.direct_storage = true;
            }

            let logical_size = metadata.len();
            let mut fi = FileInfo {
                path: shortname,
                content_len: metadata.len(),
                modified_time: metadata.last_write_time(),
                estimate_valid: false,
                estimated_ratio: 1.0,
                logical_size,
                physical_size: physical,
                estimated_physical_size: physical,
                compression: None,
            };

            if count % 8 == 0 {
                if control.is_cancelled_with_pause() {
                    for candidate in candidates {
                        ds.push(FileKind::Skipped, candidate.file);
                    }
                    return Err(ds);
                }

                if last_status.elapsed() >= Duration::from_millis(50) {
                    last_status = Instant::now();
                    control.set_status((fi.path.clone(), ds.summary()));
                }
            }

            let attributes = metadata.file_attributes();
            let forbidden = FILE_ATTRIBUTE_COMPRESSED
                | FILE_ATTRIBUTE_ENCRYPTED
                | FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_SYSTEM
                | FILE_ATTRIBUTE_TEMPORARY;
            if attributes & forbidden != 0 {
                ds.push(FileKind::Skipped, fi);
                continue;
            }
            // WOF itself uses sparse/reparse metadata. Detect the actual provider
            // before excluding these flags; size_on_disk < len is not a format.
            let backing = compact::detect_compression(entry.path());
            match backing {
                Ok(Some(compression)) => {
                    fi.compression = Some(compression);
                    ds.push(FileKind::Compressed, fi);
                    continue;
                }
                Err(_) => {
                    ds.scan_errors += 1;
                    ds.push(FileKind::Skipped, fi);
                    continue;
                }
                Ok(None) => {}
            }
            if fi.content_len <= cluster_size
                || attributes
                    & (FILE_ATTRIBUTE_READONLY
                        | FILE_ATTRIBUTE_SPARSE_FILE
                        | FILE_ATTRIBUTE_REPARSE_POINT)
                    != 0
                || incompressible.contains(incompressible_key(
                    entry.path(),
                    metadata.len(),
                    metadata.last_write_time(),
                    config.compression,
                    config.analysis_mode,
                    config.min_savings_percent,
                    cluster_size,
                ))
                || excludes.is_match(entry.path())
            {
                ds.push(FileKind::Skipped, fi);
            } else if let Some(estimator) = inline_estimator.as_ref() {
                let result = estimate_file(
                    estimator,
                    entry.path(),
                    metadata.len(),
                    is_hdd,
                    &mut inline_buffer,
                );
                apply_estimate(
                    &mut ds,
                    fi,
                    result,
                    ratio_limit,
                    analysis_mode,
                    cluster_size,
                );
            } else {
                candidates.push_back(EstimateCandidate {
                    file: fi,
                    content_len: metadata.len(),
                });

                if candidates.len() >= ESTIMATE_BATCH_FILES {
                    let batch = std::mem::take(&mut candidates);
                    if !estimate_candidates(
                        &path,
                        batch,
                        analysis_workers,
                        is_hdd,
                        ratio_limit,
                        analysis_mode,
                        cluster_size,
                        control,
                        &mut ds,
                        &mut last_status,
                    ) {
                        return Err(ds);
                    }
                }
            }
        }

        drop(incompressible);

        if analysis_workers == 1
            || estimate_candidates(
                &path,
                candidates,
                analysis_workers,
                is_hdd,
                ratio_limit,
                analysis_mode,
                cluster_size,
                control,
                &mut ds,
                &mut last_status,
            )
        {
            Ok(ds)
        } else {
            Err(ds)
        }
    }
}

#[test]
fn analysis_workers_are_storage_aware() {
    assert_eq!(
        1,
        worker_count_for_storage(16, Some(true), 0, true, AUTO_MAX_ANALYSIS_THREADS)
    );
    assert_eq!(
        1,
        worker_count_for_storage(16, None, 0, true, AUTO_MAX_ANALYSIS_THREADS)
    );
    assert_eq!(
        1,
        worker_count_for_storage(1, Some(false), 0, true, AUTO_MAX_ANALYSIS_THREADS)
    );
    assert_eq!(
        8,
        worker_count_for_storage(16, Some(false), 0, true, AUTO_MAX_ANALYSIS_THREADS)
    );
    assert_eq!(
        4,
        worker_count_for_storage(16, Some(false), 4, true, AUTO_MAX_ANALYSIS_THREADS)
    );
    assert_eq!(
        8,
        worker_count_for_storage(16, Some(true), 0, false, AUTO_MAX_ANALYSIS_THREADS)
    );
    assert_eq!(
        16,
        worker_count_for_storage(32, Some(false), 0, true, AUTO_MAX_COMPRESSION_THREADS)
    );
    assert_eq!(
        16,
        worker_count_for_storage(4, Some(false), 16, true, AUTO_MAX_ANALYSIS_THREADS)
    );
}

#[test]
fn lzx_auto_is_conservatively_capped_and_manual_is_respected() {
    assert_eq!(
        1,
        compression_worker_count(8, 4, Some(false), Compression::Lzx, 0, true)
    );
    assert_eq!(
        2,
        compression_worker_count(8, 6, Some(false), Compression::Lzx, 0, true)
    );
    assert_eq!(
        2,
        compression_worker_count(16, 8, Some(false), Compression::Lzx, 0, true)
    );
    assert_eq!(
        1,
        compression_worker_count(16, 8, Some(true), Compression::Lzx, 0, true)
    );
    assert_eq!(
        1,
        compression_worker_count(16, 8, Some(false), Compression::Lzx, 1, true)
    );
    assert_eq!(
        8,
        compression_worker_count(16, 8, Some(false), Compression::Lzx, 8, true)
    );
    assert_eq!(
        16,
        compression_worker_count(32, 16, Some(false), Compression::Xpress8k, 0, true)
    );
}

#[test]
fn thorough_analysis_keeps_candidates_rejected_by_a_generic_sample() {
    let file = || FileInfo {
        path: PathBuf::from("fixture.bin"),
        content_len: 65536,
        modified_time: 0,
        estimate_valid: false,
        estimated_ratio: 1.0,
        logical_size: 65536,
        physical_size: 65536,
        estimated_physical_size: 65536,
        compression: None,
    };
    let mut sampled = FolderInfo::new("fixture");
    apply_estimate(
        &mut sampled,
        file(),
        Ok(1.0),
        0.99,
        AnalysisMode::Sampled,
        4096,
    );
    assert_eq!(1, sampled.skipped.count);
    let mut thorough = FolderInfo::new("fixture");
    apply_estimate(
        &mut thorough,
        file(),
        Ok(1.0),
        0.99,
        AnalysisMode::Thorough,
        4096,
    );
    assert_eq!(1, thorough.compressible.count);
    assert_eq!(65536, thorough.compressible.estimated_physical_size);
}

#[test]
fn skipped_files_keep_totals_without_retaining_paths() {
    let mut folder = FolderInfo::new("C:\\Games");
    folder.push(
        FileKind::Skipped,
        FileInfo {
            path: PathBuf::from("skip.bin"),
            content_len: 8192,
            modified_time: 0,
            estimate_valid: false,
            estimated_ratio: 1.0,
            logical_size: 8192,
            physical_size: 8192,
            estimated_physical_size: 8192,
            compression: None,
        },
    );

    assert_eq!(1, folder.skipped.count);
    assert!(folder.skipped.files.is_empty());
    assert_eq!(8192, folder.skipped.logical_size);
}

#[test]
fn scan_uses_an_isolated_fixture_and_accounts_for_empty_files() {
    let dir = tempdir::TempDir::new("compactor-scan").unwrap();
    std::fs::write(dir.path().join("empty.bin"), []).unwrap();
    let scanner = FolderScan::new(dir.path(), &Config::default()).unwrap();
    let result = scanner.run(&ControlToken::new()).unwrap();
    assert_eq!(1, result.skipped.count);
    assert_eq!(0, result.logical_size);
    assert_eq!(0, result.compressible.count);
}
