use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::hash::Hash;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::path::PathBuf;

use fs2::FileExt;
use siphasher::sip128::{Hasher128, SipHasher};

#[derive(Debug, Default)]
pub struct HashFilter {
    path: Option<PathBuf>,
    last_offset: u64,
    filter: HashSet<u128>,
    pending: Vec<u128>,
}

impl HashFilter {
    pub fn open<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: Some(path.as_ref().to_owned()),
            ..Self::default()
        }
    }

    pub fn set_backing<P: AsRef<Path>>(&mut self, path: P) {
        self.path = Some(path.as_ref().to_owned());
        self.last_offset = 0;
        self.filter.clear();
        self.pending.clear();
    }

    fn reset_loaded(&mut self) {
        self.last_offset = 0;
        self.filter.clear();
        // Local inserts are still useful and must survive replacement or
        // truncation of this disposable, append-only cache.
        self.filter.extend(self.pending.iter().copied());
    }

    pub fn load(&mut self) -> io::Result<()> {
        if self.path.is_none() {
            return Ok(());
        }

        let mut file = match File::open(self.path.as_ref().unwrap()) {
            Ok(file) => file,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
                self.reset_loaded();
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        FileExt::lock_shared(&file)?;

        let end = file.metadata()?.len();
        let complete_end = end - end % 16;
        if complete_end < self.last_offset {
            self.reset_loaded();
        }

        if self.last_offset > 0 {
            file.seek(SeekFrom::Start(self.last_offset))?;
        }

        let mut file = BufReader::new(file);
        let mut buf = [0; 16];
        // A crashed append can leave an incomplete final record. Do not read
        // it, but propagate an unexpected EOF within the known complete data.
        while self.last_offset < complete_end {
            file.read_exact(&mut buf)?;
            self.filter.insert(u128::from_le_bytes(buf));
            self.last_offset += 16;
        }

        Ok(())
    }

    pub fn save(&mut self) -> io::Result<()> {
        if self.path.is_none() || self.pending.is_empty() {
            return Ok(());
        }

        if let Some(dir) = self.path.as_ref().and_then(|p| p.parent()) {
            std::fs::create_dir_all(dir)?;
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.as_ref().unwrap())?;
        FileExt::lock_exclusive(&file)?;

        let end = file.metadata()?.len();
        let complete_end = end - end % 16;
        if complete_end != end {
            file.set_len(complete_end)?;
        }
        if complete_end < self.last_offset {
            self.reset_loaded();
        }
        file.seek(SeekFrom::End(0))?;

        let new_end = (self.pending.len() as u64)
            .checked_mul(16)
            .and_then(|bytes| complete_end.checked_add(bytes))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Hash cache is too large")
            })?;
        let can_advance = complete_end == self.last_offset;
        let mut file = BufWriter::new(file);

        self.write_pending(&mut file, |writer| writer.get_ref().sync_all())?;

        if can_advance {
            self.last_offset = new_end;
        }

        Ok(())
    }

    fn write_pending<W, F>(&mut self, writer: &mut W, sync: F) -> io::Result<()>
    where
        W: Write,
        F: FnOnce(&mut W) -> io::Result<()>,
    {
        for key in &self.pending {
            writer.write_all(&key.to_le_bytes())?;
        }
        writer.flush()?;
        sync(writer)?;
        // Do not forget records until they reach durable storage. A failed
        // append may leave valid duplicate records on retry; HashSet loading
        // makes those harmless, and save() repairs a partial trailing record.
        self.pending.clear();

        Ok(())
    }

    pub fn insert<H: Hash>(&mut self, data: H) -> bool {
        let key = Self::key_for(data);

        if self.filter.insert(key) {
            self.pending.push(key);
            return true;
        }

        false
    }

    pub fn contains<H: Hash>(&self, data: H) -> bool {
        self.filter.contains(&Self::key_for(data))
    }

    fn key_for<H: Hash>(data: H) -> u128 {
        let mut hash = SipHasher::new();
        data.hash(&mut hash);
        let h = hash.finish128();
        (u128::from(h.h1) << 64) | u128::from(h.h2)
    }
}

#[test]
fn persists_and_merges_appends_from_other_instances() {
    let dir = tempdir::TempDir::new("hashfilter-test").unwrap();
    let db = dir.path().join("test.dat");
    let mut hf = HashFilter::open(&db);
    let _ = hf.load();

    let paths = vec![
        PathBuf::from("/path/to/some/file0"),
        PathBuf::from("/path/to/some/file1"),
        PathBuf::from("/path/to/some/file2"),
        PathBuf::from("/path/to/some/file3"),
        PathBuf::from("/path/to/some/file4"),
        PathBuf::from("/path/to/some/file5"),
        PathBuf::from("/path/to/some/file6"),
        PathBuf::from("/path/to/some/file7"),
        PathBuf::from("/path/to/some/file8"),
        PathBuf::from("/path/to/some/file9"),
    ];

    for p in &paths {
        hf.insert(p);
    }

    hf.save().unwrap();

    let mut hf2 = HashFilter::open(&db);
    hf2.load().unwrap();
    hf2.insert(PathBuf::from("/path/to/some/file10"));
    hf2.save().unwrap();

    for p in &paths {
        assert!(hf2.contains(p));
    }

    hf.load().unwrap();
    assert!(hf.contains(PathBuf::from("/path/to/some/file10")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn switching_backing_drops_keys_from_the_previous_cache() {
        let dir = tempdir::TempDir::new("hashfilter-switch").unwrap();
        let first = dir.path().join("first.dat");
        let second = dir.path().join("second.dat");
        let mut filter = HashFilter::open(&first);
        filter.insert("persisted in first");
        filter.save().unwrap();
        filter.insert("pending in first");

        let mut other = HashFilter::open(&second);
        other.insert("persisted in second");
        other.save().unwrap();

        filter.set_backing(&second);
        filter.load().unwrap();
        assert!(!filter.contains("persisted in first"));
        assert!(!filter.contains("pending in first"));
        assert!(filter.contains("persisted in second"));
        assert!(filter.pending.is_empty());
    }

    #[test]
    fn truncation_reloads_retained_records_and_preserves_local_inserts() {
        let dir = tempdir::TempDir::new("hashfilter-truncate").unwrap();
        let path = dir.path().join("cache.dat");
        let mut filter = HashFilter::open(&path);
        filter.insert("retained");
        filter.insert("discarded");
        filter.save().unwrap();
        filter.insert("pending");

        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(16)
            .unwrap();
        filter.load().unwrap();
        assert!(filter.contains("retained"));
        assert!(!filter.contains("discarded"));
        assert!(filter.contains("pending"));
        filter.save().unwrap();

        let mut reloaded = HashFilter::open(&path);
        reloaded.load().unwrap();
        assert!(reloaded.contains("retained"));
        assert!(reloaded.contains("pending"));
        assert!(!reloaded.contains("discarded"));
    }

    #[test]
    fn save_also_detects_truncation_without_an_intervening_load() {
        let dir = tempdir::TempDir::new("hashfilter-save-truncate").unwrap();
        let path = dir.path().join("cache.dat");
        let mut filter = HashFilter::open(&path);
        filter.insert("old");
        filter.save().unwrap();
        filter.insert("pending");
        File::create(&path).unwrap();

        filter.save().unwrap();
        assert!(!filter.contains("old"));
        assert!(filter.contains("pending"));
        assert_eq!(16, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn removed_cache_does_not_leave_stale_persisted_keys() {
        let dir = tempdir::TempDir::new("hashfilter-removed").unwrap();
        let path = dir.path().join("cache.dat");
        let mut filter = HashFilter::open(&path);
        filter.insert("old");
        filter.save().unwrap();
        filter.insert("pending");
        std::fs::remove_file(&path).unwrap();

        filter.load().unwrap();
        assert!(!filter.contains("old"));
        assert!(filter.contains("pending"));
        filter.save().unwrap();
        let mut reloaded = HashFilter::open(&path);
        reloaded.load().unwrap();
        assert!(reloaded.contains("pending"));
        assert!(!reloaded.contains("old"));
    }

    #[test]
    fn partial_trailing_record_is_ignored_then_repaired() {
        let dir = tempdir::TempDir::new("hashfilter-partial").unwrap();
        let path = dir.path().join("cache.dat");
        let mut filter = HashFilter::open(&path);
        filter.insert("complete");
        filter.save().unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&HashFilter::key_for("partial").to_le_bytes()[..7])
            .unwrap();

        let mut reloaded = HashFilter::open(&path);
        reloaded.load().unwrap();
        assert!(reloaded.contains("complete"));
        assert!(!reloaded.contains("partial"));
        reloaded.insert("after partial");
        reloaded.save().unwrap();
        assert_eq!(32, std::fs::metadata(&path).unwrap().len());
        filter.load().unwrap();
        assert!(filter.contains("after partial"));
        assert!(!filter.contains("partial"));
    }

    struct FailingWriter {
        bytes: Vec<u8>,
        fail_after: Option<usize>,
        fail_flush: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let count = match self.fail_after {
                Some(limit) if self.bytes.len() >= limit => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Injected write failure",
                    ));
                }
                Some(limit) => bytes.len().min(limit - self.bytes.len()),
                None => bytes.len(),
            };
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Injected flush failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn write_flush_and_sync_failures_all_preserve_pending_for_retry() {
        for failure in 0..3 {
            let mut filter = HashFilter::default();
            filter.insert("first");
            filter.insert("second");
            let mut writer = FailingWriter {
                bytes: Vec::new(),
                fail_after: if failure == 0 { Some(19) } else { None },
                fail_flush: failure == 1,
            };
            let result = filter.write_pending(&mut writer, |_| {
                if failure == 2 {
                    Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Injected sync failure",
                    ))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());
            assert_eq!(2, filter.pending.len());
            assert!(filter.contains("first"));
            assert!(filter.contains("second"));

            let mut retry = Cursor::new(Vec::new());
            filter.write_pending(&mut retry, |_| Ok(())).unwrap();
            assert!(filter.pending.is_empty());
            let expected: Vec<u8> = ["first", "second"]
                .iter()
                .flat_map(|value| HashFilter::key_for(value).to_le_bytes().to_vec())
                .collect();
            assert_eq!(expected, retry.into_inner());
        }
    }

    #[test]
    fn failed_save_can_be_retried_after_fixing_the_backing_path() {
        let dir = tempdir::TempDir::new("hashfilter-retry").unwrap();
        let path = dir.path().join("cache.dat");
        std::fs::create_dir(&path).unwrap();
        let mut filter = HashFilter::open(&path);
        filter.insert("pending");
        assert!(filter.save().is_err());
        assert_eq!(1, filter.pending.len());

        std::fs::remove_dir(&path).unwrap();
        filter.save().unwrap();
        let mut reloaded = HashFilter::open(&path);
        reloaded.load().unwrap();
        assert!(reloaded.contains("pending"));
    }
}
