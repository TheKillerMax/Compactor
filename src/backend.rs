use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, TryRecvError};
use filesize::PathExt;
use winapi::um::winbase::SetThreadExecutionState;
use winapi::um::winnt::{ES_CONTINUOUS, ES_SYSTEM_REQUIRED};

use crate::background::BackgroundHandle;
use crate::compact::{self, Compression};
use crate::compression::{BackgroundCompactor, CompressionJob, FileDisposition, FileOutcome};
use crate::config::Config;
use crate::directstorage::{is_under, DirectStorageScan};
use crate::folder::{
    compression_worker_count_for_path, validate_target_path, FileInfo, FileKind, FolderInfo,
    FolderScan,
};
use crate::gui::{CompressedViewItem, GuiRequest, GuiResponse, GuiWrapper};
use crate::persistence::{config, incompressible_key, pathdb};

const COMPRESSED_VIEW_PAGE_SIZE: usize = 100;
const AUTO_MAX_DECOMPRESSION_THREADS: usize = 8;
const WOF_PREFLIGHT_SAMPLE_FILES: usize = 8;

pub struct Backend<T> {
    gui: GuiWrapper<T>,
    msg: Receiver<GuiRequest>,
    info: Option<FolderInfo>,
    analysis_valid: bool,
}

struct SystemAwakeGuard;

impl SystemAwakeGuard {
    fn new() -> Self {
        // Keep the system awake during file transformations, but deliberately
        // do not request ES_DISPLAY_REQUIRED: the monitor may still turn off.
        unsafe {
            let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }
        Self
    }
}

impl Drop for SystemAwakeGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

fn format_size(size: u64, decimal: bool) -> String {
    use humansize::{file_size_opts as options, FileSize};

    size.file_size(if decimal {
        options::DECIMAL
    } else {
        options::BINARY
    })
    .expect("file size")
}

fn progress(done_bytes: u64, total_bytes: u64) -> f32 {
    if total_bytes == 0 {
        1.0
    } else {
        (done_bytes as f64 / total_bytes as f64).min(1.0) as f32
    }
}

fn decompression_worker_count_for_path(
    path: &Path,
    max_threads: usize,
    hdd_single_thread: bool,
) -> usize {
    let storage_workers = compression_worker_count_for_path(
        path,
        Compression::Xpress16k,
        max_threads,
        hdd_single_thread,
    );

    if max_threads == 0 {
        storage_workers.min(AUTO_MAX_DECOMPRESSION_THREADS).max(1)
    } else {
        storage_workers.max(1)
    }
}

fn protect_direct_storage_candidates(folder: &mut FolderInfo, roots: &[PathBuf]) -> usize {
    if roots.is_empty() {
        return 0;
    }

    let candidates = folder.len(FileKind::Compressible);
    let mut protected = 0usize;

    for _ in 0..candidates {
        let Some(fi) = folder.pop(FileKind::Compressible) else {
            break;
        };
        let absolute = folder.path.join(&fi.path);

        if roots.iter().any(|root| is_under(&absolute, root)) {
            protected += 1;
            folder.push(FileKind::Skipped, fi);
        } else {
            folder.push(FileKind::Compressible, fi);
        }
    }

    protected
}

fn literal_exclusion(path: &Path) -> String {
    let mut pattern = String::new();
    for ch in path.to_string_lossy().chars() {
        match ch {
            '*' => pattern.push_str("[*]"),
            '?' => pattern.push_str("[?]"),
            '[' => pattern.push_str("[[]"),
            ']' => pattern.push_str("[]]"),
            '{' => pattern.push_str("[{]"),
            '}' => pattern.push_str("[}]"),
            _ => pattern.push(ch),
        }
    }
    pattern
}

#[derive(Default)]
struct OperationTotals {
    changed: usize,
    not_beneficial: usize,
    skipped: usize,
    failed: usize,
    errors: Vec<String>,
    measured_before: u64,
    measured_after: u64,
}

impl OperationTotals {
    fn error(&mut self, path: &Path, message: impl std::fmt::Display) {
        self.failed += 1;
        // Keep memory and GUI messages bounded even for millions of errors.
        if self.errors.len() < 20 {
            self.errors.push(format!("{}: {}", path.display(), message));
        }
    }
}

fn record_file_result(
    folder: &mut FolderInfo,
    mut file: FileInfo,
    path: &Path,
    result: io::Result<FileOutcome>,
    totals: &mut OperationTotals,
    cache: &mut hashfilter::HashFilter,
    settings: &Config,
) {
    file.estimate_valid = false;
    match result {
        Ok(outcome) => {
            totals.measured_before = totals
                .measured_before
                .saturating_add(outcome.original_physical_size);
            totals.measured_after = totals.measured_after.saturating_add(outcome.physical_size);
            file.content_len = outcome.content_len;
            file.logical_size = outcome.content_len;
            file.modified_time = outcome.modified_time;
            file.physical_size = outcome.physical_size;
            file.compression = outcome.compression;
            match outcome.disposition {
                FileDisposition::Compressed | FileDisposition::Decompressed => totals.changed += 1,
                FileDisposition::NotBeneficial => {
                    totals.not_beneficial += 1;
                    // Cache only a measured outcome and the fingerprint captured
                    // under the protected handle, never later metadata by path.
                    cache.insert(incompressible_key(
                        path,
                        outcome.content_len,
                        outcome.modified_time,
                        settings.compression,
                        settings.analysis_mode,
                        settings.min_savings_percent,
                        folder.cluster_size,
                    ));
                }
                FileDisposition::Skipped => totals.skipped += 1,
            }
        }
        Err(error) => {
            totals.error(path, error);
            // A failed rollback may leave valid WOF backing. Refresh for the
            // viewer/decompress action rather than silently losing that file.
            if let Ok(metadata) = std::fs::metadata(path) {
                use std::os::windows::fs::MetadataExt;
                file.content_len = metadata.len();
                file.logical_size = metadata.len();
                file.modified_time = metadata.last_write_time();
            }
            if let Ok(size) = path.size_on_disk() {
                file.physical_size = size;
            }
            if let Ok(backing) = compact::detect_compression(path) {
                file.compression = backing;
            }
        }
    }
    file.estimated_physical_size = file.physical_size;
    folder.push(
        if file.compression.is_some() {
            FileKind::Compressed
        } else {
            FileKind::Skipped
        },
        file,
    );
}

fn take_queued_file(
    folder: &mut FolderInfo,
    remaining: &mut usize,
    kind: FileKind,
) -> Option<FileInfo> {
    if *remaining == 0 {
        return None;
    }
    *remaining -= 1;
    folder.pop(kind)
}

fn add_folder_exclusion(path: &Path) -> Result<bool, String> {
    let display = literal_exclusion(path);
    let c = config();
    let mut c = c.write().unwrap();
    let mut current = c.current();

    if current
        .excludes
        .iter()
        .any(|existing| existing.trim().eq_ignore_ascii_case(&display))
    {
        return Ok(false);
    }

    current.excludes.push(display);
    current.validate()?;
    c.update(current)
        .map_err(|err| format!("Unable to save exclusions: {}", err))?;
    Ok(true)
}

fn move_compressible_to_skipped(folder: &mut FolderInfo) {
    let count = folder.len(FileKind::Compressible);
    for _ in 0..count {
        if let Some(fi) = folder.pop(FileKind::Compressible) {
            folder.push(FileKind::Skipped, fi);
        }
    }
}

#[derive(Default)]
struct CompressedFolderTotals {
    count: usize,
    logical_size: u64,
    physical_size: u64,
}

fn path_matches_query(path: &Path, query: &str) -> bool {
    query.is_empty() || path.to_string_lossy().to_lowercase().contains(query)
}

fn compressed_file_items(folder: &FolderInfo, query: &str) -> Vec<CompressedViewItem> {
    let mut files: Vec<&FileInfo> = folder
        .compressed
        .files
        .iter()
        .filter(|fi| path_matches_query(&fi.path, query))
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));

    files
        .into_iter()
        .map(|fi| CompressedViewItem::File {
            path: fi.path.clone(),
            logical_size: fi.logical_size,
            physical_size: fi.physical_size,
        })
        .collect()
}

fn compressed_folder_items(folder: &FolderInfo, query: &str) -> Vec<CompressedViewItem> {
    let mut folders: BTreeMap<PathBuf, CompressedFolderTotals> = BTreeMap::new();

    for fi in &folder.compressed.files {
        let parent = fi
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let totals = folders.entry(parent).or_default();
        totals.count += 1;
        totals.logical_size = totals.logical_size.saturating_add(fi.logical_size);
        totals.physical_size = totals.physical_size.saturating_add(fi.physical_size);
    }

    folders
        .into_iter()
        .filter(|(path, _)| path_matches_query(path, query))
        .map(|(path, totals)| CompressedViewItem::Folder {
            path,
            count: totals.count,
            logical_size: totals.logical_size,
            physical_size: totals.physical_size,
        })
        .collect()
}

fn paginate_compressed_items(
    items: Vec<CompressedViewItem>,
    page: usize,
) -> (usize, usize, usize, Vec<CompressedViewItem>) {
    let total = items.len();
    let pages = if total == 0 {
        1
    } else {
        (total + COMPRESSED_VIEW_PAGE_SIZE - 1) / COMPRESSED_VIEW_PAGE_SIZE
    };
    let page = page.min(pages - 1);
    let start = page.saturating_mul(COMPRESSED_VIEW_PAGE_SIZE);
    let page_items = items
        .into_iter()
        .skip(start)
        .take(COMPRESSED_VIEW_PAGE_SIZE)
        .collect();

    (page, pages, total, page_items)
}

impl<T> Backend<T> {
    pub fn new(gui: GuiWrapper<T>, msg: Receiver<GuiRequest>) -> Self {
        Self {
            gui,
            msg,
            info: None,
            analysis_valid: false,
        }
    }

    fn target_is_valid(&self, path: &Path) -> bool {
        match validate_target_path(path) {
            Ok(()) => true,
            Err(message) => {
                self.gui.error("Folder not supported", &message);
                false
            }
        }
    }

    fn wof_preflight(&self, folder: &FolderInfo, kind: FileKind) -> bool {
        match compact::system_supports_compression() {
            Ok(true) => {}
            Ok(false) => {
                self.gui.error(
                    "WOF compression unavailable",
                    "This version of Windows does not report WOF filesystem compression support.",
                );
                return false;
            }
            Err(err) => {
                self.gui.error(
                    "WOF compression unavailable",
                    format!("Unable to initialise Windows WOF support: {}", err),
                );
                return false;
            }
        }

        let files = match kind {
            FileKind::Compressible => &folder.compressible.files,
            FileKind::Compressed => &folder.compressed.files,
            FileKind::Skipped => return true,
        };

        // Locked files can make an individual probe fail. Try a handful and
        // only block the operation when Windows explicitly says WOF is not
        // attached/supported on an accessible file from this volume.
        for fi in files.iter().take(WOF_PREFLIGHT_SAMPLE_FILES) {
            let path = folder.path.join(&fi.path);
            match compact::file_supports_compression(&path) {
                Ok(true) => return true,
                Ok(false) => {
                    self.gui.error(
                        "WOF unavailable on this drive",
                        "Windows Overlay Filter (Wof.sys) is not available for this NTFS volume, so WOF compression cannot be used here.",
                    );
                    return false;
                }
                Err(_) => continue,
            }
        }

        // If every sample happened to be locked, let the normal per-file path
        // handle it rather than refusing the whole operation on an uncertain
        // preflight result.
        true
    }

    pub fn run(&mut self) {
        loop {
            match self.msg.recv() {
                Ok(GuiRequest::ChooseFolder) => {
                    let path = self.gui.choose_folder().recv().ok().flatten();

                    if let Some(path) = path {
                        if self.target_is_valid(&path) {
                            self.gui.folder(&path);
                            self.scan_loop(path);
                            continue;
                        }
                    }
                    self.gui.scanned();
                    if !self.analysis_valid {
                        self.gui.send(&GuiResponse::AnalysisInvalidated);
                    }
                }
                Ok(GuiRequest::Analyse) if self.info.is_some() => {
                    let path = self.info.as_ref().unwrap().path.clone();
                    if self.target_is_valid(&path) {
                        self.info = None;
                        self.gui.folder(&path);
                        self.scan_loop(path);
                    }
                }
                Ok(GuiRequest::ViewCompressed { view, query, page }) if self.info.is_some() => {
                    self.view_compressed(view, query, page);
                }
                Ok(GuiRequest::SettingsChanged) => {
                    self.analysis_valid = false;
                    if self.info.is_some() {
                        self.gui.send(&GuiResponse::AnalysisInvalidated);
                    }
                }
                Ok(GuiRequest::Quit) => break,
                Ok(GuiRequest::Compress) if self.info.is_some() => {
                    if !self.analysis_valid {
                        self.gui.send(&GuiResponse::AnalysisInvalidated);
                        self.gui.error(
                            "Analysis required",
                            "Analyse the folder with the current settings before compressing.",
                        );
                        continue;
                    }
                    let path = self.info.as_ref().unwrap().path.clone();
                    if self.target_is_valid(&path) {
                        self.compress_loop();
                    }
                }
                Ok(GuiRequest::Decompress) if self.info.is_some() => {
                    let path = self.info.as_ref().unwrap().path.clone();
                    if self.target_is_valid(&path) {
                        self.uncompress_loop(false);
                    }
                }
                Ok(GuiRequest::DecompressAndExclude) if self.info.is_some() => {
                    let path = self.info.as_ref().unwrap().path.clone();
                    if self.target_is_valid(&path) {
                        self.uncompress_loop(true);
                    }
                }
                Ok(GuiRequest::Analyse)
                | Ok(GuiRequest::Compress)
                | Ok(GuiRequest::Decompress)
                | Ok(GuiRequest::DecompressAndExclude) => {
                    self.gui
                        .error("Folder required", "Choose and analyse a folder first.");
                    self.gui.scanned();
                    self.gui.send(&GuiResponse::AnalysisInvalidated);
                }
                Ok(msg) => {
                    eprintln!("Backend: Ignored message: {:?}", msg);
                }
                Err(_) => {
                    eprintln!("Backend: exit run loop");
                    break;
                }
            }
        }
    }

    fn scan_loop(&mut self, path: PathBuf) {
        self.info = None;
        self.analysis_valid = false;
        let current = config().read().unwrap().current();
        let scanner = match FolderScan::new(&path, &current) {
            Ok(scanner) => scanner,
            Err(message) => {
                self.gui.error("Settings error", message);
                self.gui.stopped();
                return;
            }
        };
        let task = BackgroundHandle::spawn(scanner);
        let start = Instant::now();
        let mut cancelled = false;

        self.gui.status("Analysing", None);
        loop {
            let msg = self.msg.recv_timeout(Duration::from_millis(25));

            match msg {
                Ok(GuiRequest::Pause) => {
                    task.pause();
                    self.gui.status("Paused", Some(0.5));
                    self.gui.paused();
                }
                Ok(GuiRequest::Resume) => {
                    task.resume();
                    self.gui.status("Analysing", None);
                    self.gui.resumed();
                }
                Ok(GuiRequest::Stop)
                | Ok(GuiRequest::Quit)
                | Ok(GuiRequest::SettingsChanged)
                | Err(RecvTimeoutError::Disconnected) => {
                    cancelled = true;
                    task.cancel();
                }
                Ok(msg) => {
                    eprintln!("Ignored message: {:?}", msg);
                }
                Err(RecvTimeoutError::Timeout) => (),
            }

            match task.wait_timeout(Duration::from_millis(25)) {
                Some(result) => {
                    let complete = result.is_ok() && !cancelled;
                    let info = match result {
                        Ok(info) | Err(info) => info,
                    };
                    self.analysis_valid = complete;
                    if complete {
                        self.gui.status(
                            format!(
                                "Analysed in {:.2?}; {} scan/read errors",
                                start.elapsed(),
                                info.scan_errors
                            ),
                            Some(1.0),
                        );
                        self.gui.summary(info.summary());
                        self.gui.scanned();
                    } else {
                        self.gui.status(
                            format!(
                                "Analysis stopped after {:.2?}; analyse again before compressing",
                                start.elapsed()
                            ),
                            Some(0.0),
                        );
                        self.gui.summary(info.summary());
                        self.gui.stopped();
                        self.gui.send(&GuiResponse::AnalysisInvalidated);
                    }
                    self.info = Some(info);
                    break;
                }
                None => {
                    if let Some(status) = task.status() {
                        self.gui
                            .status(format!("Analysing: {}", status.0.display()), None);
                        self.gui.summary(status.1);
                    }
                }
            }
        }
    }

    fn view_compressed(&self, view: String, query: String, page: usize) {
        let Some(folder) = self.info.as_ref() else {
            return;
        };

        let query = query.trim().to_string();
        let query_match = query.to_lowercase();
        let view = if view.eq_ignore_ascii_case("folders") {
            "folders"
        } else {
            "files"
        };
        let items = if view == "folders" {
            compressed_folder_items(folder, &query_match)
        } else {
            compressed_file_items(folder, &query_match)
        };
        let (page, pages, total, items) = paginate_compressed_items(items, page);
        let summary = folder.compressed.summary();

        self.gui.send(&GuiResponse::CompressedView {
            root: folder.path.clone(),
            view: view.to_string(),
            query,
            page,
            pages,
            total,
            compressed_count: summary.count,
            logical_size: summary.logical_size,
            physical_size: summary.physical_size,
            items,
        });
    }

    fn compress_loop(&mut self) {
        self.transform_loop(false, false);
    }

    fn uncompress_loop(&mut self, exclude_after: bool) {
        self.transform_loop(true, exclude_after);
    }

    fn direct_storage_roots(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.gui.status("Checking DirectStorage protection", None);
        let task = BackgroundHandle::spawn(DirectStorageScan {
            path: path.to_path_buf(),
        });
        let mut cancelled = false;
        loop {
            match self.msg.recv_timeout(Duration::from_millis(25)) {
                Ok(GuiRequest::Pause) => {
                    task.pause();
                    self.gui.paused();
                }
                Ok(GuiRequest::Resume) => {
                    task.resume();
                    self.gui.resumed();
                }
                Ok(GuiRequest::Stop)
                | Ok(GuiRequest::Quit)
                | Ok(GuiRequest::SettingsChanged)
                | Err(RecvTimeoutError::Disconnected) => {
                    cancelled = true;
                    task.cancel();
                }
                _ => {}
            }
            if let Some(result) = task.wait_timeout(Duration::from_millis(25)) {
                return if cancelled {
                    Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "DirectStorage check stopped",
                    ))
                } else {
                    result
                };
            }
        }
    }

    fn transform_loop(&mut self, decompress: bool, exclude_after: bool) {
        let current = config().read().unwrap().current();
        let mut folder = self.info.take().expect("fileinfo");
        let kind = if decompress {
            FileKind::Compressed
        } else {
            FileKind::Compressible
        };
        let operation = if decompress {
            "Decompression"
        } else {
            "Compression"
        };
        let verb = if decompress {
            "Expanding"
        } else {
            "Compacting"
        };
        let mut totals = OperationTotals::default();
        self.gui.compacting();

        // Run independently of compression exclusions: a runtime DLL hidden in
        // an excluded bin directory must still protect the rest of its game.
        if !decompress && current.protect_direct_storage {
            match self.direct_storage_roots(&folder.path) {
                Ok(roots) => {
                    folder.direct_storage = !roots.is_empty();
                    totals.skipped += protect_direct_storage_candidates(&mut folder, &roots);
                }
                Err(error) => {
                    self.analysis_valid = false;
                    self.gui
                        .status(format!("Compression not started: {}", error), Some(0.0));
                    if error.kind() != io::ErrorKind::Interrupted {
                        self.gui.error("DirectStorage check incomplete", format!(
                            "No files were compressed. {}. Review access to the selected folder and analyse again.", error,
                        ));
                    }
                    self.gui.summary(folder.summary());
                    self.gui.stopped();
                    self.gui.send(&GuiResponse::AnalysisInvalidated);
                    self.info = Some(folder);
                    return;
                }
            }
        }

        let total_files = folder.len(kind);
        if total_files > 0 && !self.wof_preflight(&folder, kind) {
            self.gui.summary(folder.summary());
            self.gui.scanned();
            self.info = Some(folder);
            return;
        }
        let _awake = SystemAwakeGuard::new();
        let worker_count = if decompress {
            decompression_worker_count_for_path(
                &folder.path,
                current.max_threads,
                current.hdd_single_thread,
            )
        } else {
            compression_worker_count_for_path(
                &folder.path,
                current.compression,
                current.max_threads,
                current.hdd_single_thread,
            )
        }
        .min(total_files.max(1));
        let (send_file, jobs) = bounded::<CompressionJob>(worker_count);
        let (results, recv_result) = bounded::<(PathBuf, io::Result<FileOutcome>)>(worker_count);
        let mut tasks = Vec::with_capacity(worker_count);
        if total_files > 0 {
            for _ in 0..worker_count {
                tasks.push(BackgroundHandle::spawn(BackgroundCompactor::new(
                    if decompress {
                        None
                    } else {
                        Some(current.compression)
                    },
                    current.min_savings_percent,
                    current.analysis_mode,
                    folder.cluster_size,
                    jobs.clone(),
                    results.clone(),
                )));
            }
        }
        drop(jobs);
        drop(results);

        let start = Instant::now();
        let total_bytes = if decompress {
            folder.compressed.logical_size
        } else {
            folder.compressible.logical_size
        };
        let mut done_files = 0usize;
        let mut done_bytes = 0u64;
        let mut pending: HashMap<PathBuf, FileInfo> = HashMap::with_capacity(worker_count);
        let mut paused = false;
        let mut stopped = false;
        let mut exhausted = total_files == 0;
        let mut remaining_to_dispatch = total_files;
        let mut last_update = Instant::now();
        let mut last_write = Instant::now();
        let mut cache = pathdb().write().unwrap();
        let _ = cache.load();
        self.gui.status(verb, Some(0.0));

        loop {
            // Consume controls before dispatching so closing a paused or idle
            // window stops scheduling work instead of leaving a stranded loop.
            loop {
                match self.msg.try_recv() {
                    Ok(GuiRequest::Pause) if !stopped => {
                        paused = true;
                        self.gui.paused();
                        self.gui.status(
                            "Pausing after active files finish",
                            Some(progress(done_bytes, total_bytes)),
                        );
                    }
                    Ok(GuiRequest::Resume) if !stopped => {
                        paused = false;
                        self.gui.resumed();
                    }
                    Ok(GuiRequest::SettingsChanged) => {
                        self.analysis_valid = false;
                        stopped = true;
                    }
                    Ok(GuiRequest::Stop) | Ok(GuiRequest::Quit) => stopped = true,
                    Err(TryRecvError::Disconnected) => {
                        stopped = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    _ => {}
                }
            }

            while !paused && !stopped && !exhausted && pending.len() < worker_count {
                match take_queued_file(&mut folder, &mut remaining_to_dispatch, kind) {
                    Some(file) => {
                        let path = folder.path.join(&file.path);
                        let job = CompressionJob {
                            path: path.clone(),
                            root: folder.canonical_root.clone(),
                            expected_path: folder.canonical_root.join(&file.path),
                            content_len: file.content_len,
                            modified_time: file.modified_time,
                            estimate_valid: file.estimate_valid,
                            estimated_ratio: file.estimated_ratio,
                        };
                        if send_file.send(job).is_err() {
                            folder.push(kind, file);
                            stopped = true;
                            break;
                        }
                        pending.insert(path, file);
                    }
                    None => exhausted = true,
                }
            }
            if (exhausted || stopped) && pending.is_empty() {
                break;
            }

            match recv_result.recv_timeout(Duration::from_millis(25)) {
                Ok((path, result)) => {
                    if let Some(file) = pending.remove(&path) {
                        done_files += 1;
                        done_bytes = done_bytes.saturating_add(file.logical_size);
                        record_file_result(
                            &mut folder,
                            file,
                            &path,
                            result,
                            &mut totals,
                            &mut cache,
                            &current,
                        );
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    stopped = true;
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            // A panicked worker cannot report its active file. Do not wait
            // forever for that missing result; drain the other workers below.
            if tasks.iter().any(|task| task.is_finished()) {
                stopped = true;
                break;
            }
            if last_update.elapsed() >= Duration::from_millis(100) {
                let state = if stopped {
                    "Stopping after active files finish"
                } else if paused && pending.is_empty() {
                    "Paused"
                } else if paused {
                    "Pausing after active files finish"
                } else {
                    verb
                };
                self.gui.status(
                    format!(
                        "{}: {} / {} files; {} errors",
                        state, done_files, total_files, totals.failed
                    ),
                    Some(progress(done_bytes, total_bytes)),
                );
                if pending.is_empty() {
                    self.gui.summary(folder.summary());
                }
                last_update = Instant::now();
            }
            if last_write.elapsed() >= Duration::from_secs(60) {
                if let Err(error) = cache.save() {
                    eprintln!("Unable to save compression cache: {}", error);
                }
                last_write = Instant::now();
            }
        }

        drop(send_file);
        for task in tasks {
            if task.wait_result().is_err() {
                stopped = true;
            }
        }
        // At most worker_count files were active; the equally-sized result
        // channel can hold all remaining responses while the workers finish.
        for (path, result) in recv_result.try_iter() {
            if let Some(file) = pending.remove(&path) {
                done_bytes = done_bytes.saturating_add(file.logical_size);
                record_file_result(
                    &mut folder,
                    file,
                    &path,
                    result,
                    &mut totals,
                    &mut cache,
                    &current,
                );
            }
        }
        for (path, file) in pending {
            record_file_result(
                &mut folder,
                file,
                &path,
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Worker exited before reporting this file; analyse again",
                )),
                &mut totals,
                &mut cache,
                &current,
            );
        }
        if let Err(error) = cache.save() {
            eprintln!("Unable to save compression cache: {}", error);
        }
        drop(cache);

        let mut excluded = false;
        if exclude_after && !stopped && totals.failed == 0 && folder.compressed.count == 0 {
            match add_folder_exclusion(&folder.path) {
                Ok(_) => {
                    excluded = true;
                    move_compressible_to_skipped(&mut folder);
                    self.gui.config();
                }
                Err(error) => self.gui.error("Unable to add exclusion", error),
            }
        } else if exclude_after && !stopped {
            self.gui.error("Folder was not excluded", "Some files could not be decompressed. Review the operation report, fix the errors and try again.");
        }

        let decimal = current.decimal;
        let size_change = if decompress {
            totals.measured_after.saturating_sub(totals.measured_before)
        } else {
            totals.measured_before.saturating_sub(totals.measured_after)
        };
        self.gui.status(
            format!(
                "{}{}: {} changed, {} below threshold, {} skipped, {} errors; {} {} in {:.2?}{}",
                if stopped { "Stopped — " } else { "" },
                operation,
                totals.changed,
                totals.not_beneficial,
                totals.skipped,
                totals.failed,
                format_size(size_change, decimal),
                if decompress {
                    "additional space in completed files"
                } else {
                    "saved in completed files"
                },
                start.elapsed(),
                if excluded { "; folder excluded" } else { "" },
            ),
            Some(progress(done_bytes, total_bytes)),
        );
        self.gui.send(&GuiResponse::OperationSummary {
            operation: operation.to_string(),
            changed: totals.changed,
            not_beneficial: totals.not_beneficial,
            skipped: totals.skipped,
            failed: totals.failed,
            errors: totals.errors,
        });
        self.gui.summary(folder.summary());
        if stopped {
            self.gui.stopped();
        } else {
            self.gui.scanned();
        }
        // Expanded files have not been analysed against exclusions or the
        // current algorithm. Errors can also leave changed backing metadata.
        if decompress || totals.failed > 0 || !self.analysis_valid {
            self.analysis_valid = false;
            self.gui.send(&GuiResponse::AnalysisInvalidated);
        }
        self.info = Some(folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_compressed_folder() -> FolderInfo {
        let mut folder = FolderInfo::new(PathBuf::from("C:").join("Games"));

        for (path, logical_size, physical_size) in [
            (PathBuf::from("Data").join("one.bin"), 8192, 4096),
            (PathBuf::from("Data").join("two.bin"), 16384, 8192),
            (
                PathBuf::from("Data").join("Sub").join("three.bin"),
                32768,
                12288,
            ),
        ] {
            folder.push(
                FileKind::Compressed,
                FileInfo {
                    path,
                    content_len: logical_size,
                    modified_time: 0,
                    estimate_valid: false,
                    estimated_ratio: 1.0,
                    logical_size,
                    physical_size,
                    estimated_physical_size: physical_size,
                    compression: Some(Compression::Lzx),
                },
            );
        }

        folder
    }

    #[test]
    fn automatically_excluded_folder_names_are_literal_globs() {
        let path = Path::new(r"C:\Games\Title [2024] {demo}");
        let mut config = Config::default();
        config.excludes = vec![literal_exclusion(path)];
        let excludes = config.globset().unwrap();
        assert!(excludes.is_match(path));
        assert!(!excludes.is_match(r"C:\Games\Title 2 demo"));
    }

    #[test]
    fn failed_decompression_files_are_retained_without_requeueing_in_the_same_run() {
        let mut folder = sample_compressed_folder();
        let mut remaining = folder.compressed.count;
        let mut attempted = std::collections::HashSet::new();
        while let Some(file) = take_queued_file(&mut folder, &mut remaining, FileKind::Compressed) {
            assert!(
                attempted.insert(file.path.clone()),
                "file retried in the same run"
            );
            // Failure keeps WOF backing in the viewer and available to retry
            // during a later operation, never in this operation's work budget.
            folder.push(FileKind::Compressed, file);
        }
        assert_eq!(3, attempted.len());
        assert_eq!(3, folder.compressed.count);
        assert_eq!(0, remaining);
    }

    #[test]
    fn operation_errors_are_counted_but_details_are_bounded() {
        let mut totals = OperationTotals::default();
        for _ in 0..100 {
            totals.error(Path::new("test.bin"), "denied");
        }
        assert_eq!(100, totals.failed);
        assert_eq!(20, totals.errors.len());
    }

    #[test]
    fn compressed_file_view_filters_paths() {
        let folder = sample_compressed_folder();
        let items = compressed_file_items(&folder, "sub");

        assert_eq!(1, items.len());
        match &items[0] {
            CompressedViewItem::File { path, .. } => {
                assert_eq!(&PathBuf::from("Data").join("Sub").join("three.bin"), path);
            }
            _ => panic!("expected file item"),
        }
    }

    #[test]
    fn compressed_folder_view_groups_files() {
        let folder = sample_compressed_folder();
        let items = compressed_folder_items(&folder, "");

        assert_eq!(2, items.len());
        let data = items
            .iter()
            .find(|item| match item {
                CompressedViewItem::Folder { path, .. } => path == &PathBuf::from("Data"),
                _ => false,
            })
            .expect("Data folder");

        match data {
            CompressedViewItem::Folder {
                count,
                logical_size,
                physical_size,
                ..
            } => {
                assert_eq!(2, *count);
                assert_eq!(24576, *logical_size);
                assert_eq!(12288, *physical_size);
            }
            _ => panic!("expected folder item"),
        }
    }

    #[test]
    fn compressed_view_pagination_clamps_page() {
        let items = (0..3)
            .map(|index| CompressedViewItem::File {
                path: PathBuf::from(format!("{}.bin", index)),
                logical_size: 8192,
                physical_size: 4096,
            })
            .collect();

        let (page, pages, total, page_items) = paginate_compressed_items(items, usize::MAX);
        assert_eq!(0, page);
        assert_eq!(1, pages);
        assert_eq!(3, total);
        assert_eq!(3, page_items.len());
    }

    #[test]
    fn direct_storage_protection_moves_only_matching_candidates() {
        let mut folder = FolderInfo::new(PathBuf::from(r"D:\Games"));
        for path in ["Foo\\data.bin", "Bar\\data.bin"] {
            folder.push(
                FileKind::Compressible,
                FileInfo {
                    path: PathBuf::from(path),
                    content_len: 8192,
                    modified_time: 0,
                    estimate_valid: true,
                    estimated_ratio: 0.5,
                    logical_size: 8192,
                    physical_size: 8192,
                    estimated_physical_size: 4096,
                    compression: None,
                },
            );
        }

        let protected =
            protect_direct_storage_candidates(&mut folder, &[PathBuf::from(r"D:\Games\Foo")]);
        assert_eq!(1, protected);
        assert_eq!(1, folder.compressible.count);
        assert_eq!(1, folder.skipped.count);
        assert_eq!(
            PathBuf::from(r"Bar\data.bin"),
            folder.compressible.files[0].path
        );
    }

    #[test]
    fn decompression_auto_is_capped_but_manual_is_respected() {
        // The exact storage result is exercised in folder.rs. This helper only
        // adds the decompression-specific Auto cap.
        assert_eq!(
            1,
            decompression_worker_count_for_path(Path::new(r"Z:\unknown"), 0, true)
        );
    }
}
