use std::fmt;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde_derive::{Deserialize, Serialize};

use crate::compact::Compression;

/// Sampling is fast, while thorough analysis lets Windows measure every
/// eligible file. Neither mode changes the selected WOF compression format.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AnalysisMode {
    #[default]
    Sampled,
    Thorough,
}

impl fmt::Display for AnalysisMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Sampled => "Sampled",
            Self::Thorough => "Thorough",
        })
    }
}

impl FromStr for AnalysisMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sampled" => Ok(Self::Sampled),
            "thorough" => Ok(Self::Thorough),
            _ => Err(()),
        }
    }
}

fn default_min_savings_percent() -> f32 {
    1.0
}

fn default_max_threads() -> usize {
    0
}

fn default_hdd_single_thread() -> bool {
    true
}

fn default_protect_direct_storage() -> bool {
    true
}

fn default_compression_priority() -> CompressionPriority {
    CompressionPriority::BelowNormal
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CompressionPriority {
    Lowest,
    BelowNormal,
    Normal,
    AboveNormal,
    Highest,
}

impl Default for CompressionPriority {
    fn default() -> Self {
        Self::BelowNormal
    }
}

impl fmt::Display for CompressionPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lowest => "Lowest",
            Self::BelowNormal => "BelowNormal",
            Self::Normal => "Normal",
            Self::AboveNormal => "AboveNormal",
            Self::Highest => "Highest",
        })
    }
}

impl FromStr for CompressionPriority {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "lowest" => Ok(Self::Lowest),
            "belownormal" | "below normal" | "below-normal" => Ok(Self::BelowNormal),
            "normal" => Ok(Self::Normal),
            "abovenormal" | "above normal" | "above-normal" => Ok(Self::AboveNormal),
            "highest" => Ok(Self::Highest),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Default)]
pub struct ConfigFile {
    backing: Option<PathBuf>,
    config: Config,
    load_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub decimal: bool,
    pub compression: Compression,
    #[serde(default)]
    pub analysis_mode: AnalysisMode,
    #[serde(default = "default_min_savings_percent")]
    pub min_savings_percent: f32,
    #[serde(default = "default_max_threads")]
    pub max_threads: usize,
    #[serde(default = "default_hdd_single_thread")]
    pub hdd_single_thread: bool,
    #[serde(default = "default_protect_direct_storage")]
    pub protect_direct_storage: bool,
    #[serde(default = "default_compression_priority")]
    pub compression_priority: CompressionPriority,
    pub excludes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            decimal: false,
            compression: Compression::Lzx,
            analysis_mode: AnalysisMode::default(),
            min_savings_percent: default_min_savings_percent(),
            max_threads: default_max_threads(),
            hdd_single_thread: default_hdd_single_thread(),
            protect_direct_storage: default_protect_direct_storage(),
            compression_priority: default_compression_priority(),
            excludes: vec!["*:\\Windows*", "*:\\System Volume Information*", "*:\\$*"]
                .into_iter()
                .map(String::from)
                .collect(),
        }
    }
}

impl ConfigFile {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let path = path.as_ref().to_owned();
        let loaded = std::fs::read(&path).and_then(|data| {
            let config = serde_json::from_slice::<Config>(&data)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            config
                .validate()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            Ok(config)
        });
        let (config, load_error) = match loaded {
            Ok(config) => (config, None),
            Err(err) if err.kind() == io::ErrorKind::NotFound => (Config::default(), None),
            Err(err) => (
                Config::default(),
                Some(format!("Unable to load {}: {}", path.display(), err)),
            ),
        };

        Self {
            backing: Some(path),
            config,
            load_error,
        }
    }

    fn write_config(&self, config: &Config) -> io::Result<()> {
        config
            .validate()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        match &self.backing {
            Some(path) => {
                let data = serde_json::to_vec_pretty(config)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                atomic_write(path, &data)
            }
            None => Ok(()),
        }
    }

    /// Persist first, so a failed write cannot silently change active settings.
    pub fn update(&mut self, config: Config) -> io::Result<()> {
        self.write_config(&config)?;
        self.config = config;
        self.load_error = None;
        Ok(())
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn current(&self) -> Config {
        self.config.clone()
    }
}

fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid settings filename"))?;

    // A sibling temp file guarantees the final rename stays on one volume.
    // create_new prevents accidental reuse of another process's pending write.
    for _ in 0..100 {
        let mut temporary_name = std::ffi::OsString::from(".");
        temporary_name.push(filename);
        temporary_name.push(format!(
            ".{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = parent.join(temporary_name);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };

        let result = (|| {
            file.write_all(data)?;
            file.sync_all()?;
            drop(file);
            replace_file(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        return result;
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Unable to create a temporary settings file",
    ))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winbase::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let succeeded = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

impl Config {
    pub fn ratio_limit(&self) -> f32 {
        1.0 - (self.min_savings_percent.clamp(0.0, 100.0) / 100.0)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.min_savings_percent.is_finite()
            || self.min_savings_percent < 0.0
            || self.min_savings_percent > 100.0
        {
            return Err("Minimum savings must be between 0 and 100%.".to_string());
        }

        if self.max_threads > 16 {
            return Err("Maximum threads must be between 0 (Auto) and 16.".to_string());
        }

        self.globset().map(|_| ())
    }

    pub fn globset(&self) -> Result<GlobSet, String> {
        let mut globs = GlobSetBuilder::new();

        for pattern in self
            .excludes
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            let glob = GlobBuilder::new(pattern)
                .case_insensitive(true)
                .backslash_escape(false)
                .build()
                .map_err(|e| e.to_string())?;
            globs.add(glob);
        }

        globs.build().map_err(|e| e.to_string())
    }
}

#[test]
fn test_config() {
    let s = Config::default();

    assert!(s.globset().is_ok());
    assert_eq!(s.compression, Compression::Lzx);
    assert_eq!(s.analysis_mode, AnalysisMode::Sampled);
    assert_eq!(s.min_savings_percent, 1.0);
    assert_eq!(s.max_threads, 0);
    assert!(s.hdd_single_thread);
    assert!(s.protect_direct_storage);
    assert_eq!(s.compression_priority, CompressionPriority::BelowNormal);
    assert!((s.ratio_limit() - 0.99).abs() < f32::EPSILON);

    let gs = s.globset().unwrap();
    assert!(gs.is_match("C:\\Windows\\System32\\floop\\bla.txt"));
    assert!(gs.is_match("C:\\System Volume Information\\tracking.log"));
    assert!(gs.is_match("C:\\$Recycle.Bin\\example.bin"));

    // File extensions are not excluded by default. The sampled estimator and
    // configured savings threshold decide whether compression is worthwhile.
    assert!(!gs.is_match("C:\\foo\\archive.rar"));
    assert!(!gs.is_match("C:\\foo\\data.lz4"));
    assert!(!gs.is_match("C:\\foo\\PHOTO.JPG"));
}

#[test]
fn compression_priority_parses_ui_values() {
    assert_eq!(
        CompressionPriority::BelowNormal,
        "BelowNormal".parse::<CompressionPriority>().unwrap()
    );
    assert_eq!(
        CompressionPriority::AboveNormal,
        "Above Normal".parse::<CompressionPriority>().unwrap()
    );
    assert!("Realtime".parse::<CompressionPriority>().is_err());
}

#[test]
fn blank_excludes_are_ignored() {
    let mut s = Config::default();
    s.excludes.push(String::new());
    s.excludes.push("   ".to_string());

    let gs = s.globset().unwrap();
    assert!(!gs.is_match("C:\\foo\\ordinary.txt"));
}

#[test]
fn invalid_threshold_is_rejected() {
    let mut s = Config::default();
    s.min_savings_percent = 101.0;
    assert!(s.validate().is_err());
}

#[test]
fn invalid_thread_limit_is_rejected() {
    let mut s = Config::default();
    s.max_threads = 17;
    assert!(s.validate().is_err());
}

#[test]
fn legacy_settings_keep_sampled_analysis() {
    let legacy = r#"{"decimal":false,"compression":"Xpress8k","excludes":[]}"#;
    let settings: Config = serde_json::from_str(legacy).unwrap();
    assert_eq!(settings.analysis_mode, AnalysisMode::Sampled);
    assert_eq!(settings.compression, Compression::Xpress8k);
    assert!(settings.validate().is_ok());
}

#[test]
fn analysis_modes_round_trip_without_changing_the_algorithm() {
    let mut settings = Config::default();
    settings.analysis_mode = "Thorough".parse().unwrap();
    let json = serde_json::to_string(&settings).unwrap();
    let restored: Config = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.analysis_mode, AnalysisMode::Thorough);
    assert_eq!(restored.analysis_mode.to_string(), "Thorough");
    assert_eq!(restored.compression, Compression::Lzx);
    assert!("Automatic".parse::<AnalysisMode>().is_err());
}

#[test]
fn invalid_settings_on_disk_are_reported_and_left_unchanged() {
    let directory = tempdir::TempDir::new("compactor-config-invalid").unwrap();
    let path = directory.path().join("config.json");
    let invalid = r#"{"decimal":false,"compression":"Lzx","max_threads":17,"excludes":[]}"#;
    std::fs::write(&path, invalid).unwrap();

    let settings = ConfigFile::new(&path);
    assert!(settings.load_error().is_some());
    assert_eq!(settings.current().max_threads, 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
}

#[test]
fn malformed_exclusion_on_disk_does_not_reach_the_scanner() {
    let directory = tempdir::TempDir::new("compactor-config-glob").unwrap();
    let path = directory.path().join("config.json");
    let invalid = r#"{"decimal":false,"compression":"Lzx","excludes":["["]}"#;
    std::fs::write(&path, invalid).unwrap();

    let settings = ConfigFile::new(&path);
    assert!(settings.load_error().is_some());
    assert!(settings.current().globset().is_ok());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
}

#[test]
fn settings_update_persists_before_changing_memory() {
    let directory = tempdir::TempDir::new("compactor-config-update").unwrap();
    let path = directory.path().join("config.json");
    let mut file = ConfigFile::new(&path);
    assert!(file.load_error().is_none());
    file.update(Config::default()).unwrap();

    let mut updated = file.current();
    updated.analysis_mode = AnalysisMode::Thorough;
    updated.min_savings_percent = 7.5;
    file.update(updated).unwrap();

    let restored = ConfigFile::new(&path);
    assert!(restored.load_error().is_none());
    assert_eq!(restored.current().analysis_mode, AnalysisMode::Thorough);
    assert_eq!(restored.current().min_savings_percent, 7.5);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn failed_settings_update_preserves_the_previous_file_and_memory() {
    let directory = tempdir::TempDir::new("compactor-config-failed").unwrap();
    let path = directory.path().join("config.json");
    let mut file = ConfigFile::new(&path);
    file.update(Config::default()).unwrap();
    let original = std::fs::read(&path).unwrap();

    let mut invalid = file.current();
    invalid.min_savings_percent = f32::NAN;
    assert!(file.update(invalid).is_err());
    assert_eq!(file.current().min_savings_percent, 1.0);
    assert_eq!(std::fs::read(&path).unwrap(), original);

    // Force a rename failure without relying on permissions (tests may run as
    // administrator/root). A destination directory must remain untouched.
    let blocked_path = directory.path().join("blocked.json");
    std::fs::create_dir(&blocked_path).unwrap();
    std::fs::write(blocked_path.join("sentinel"), b"keep").unwrap();
    let mut blocked = ConfigFile::new(&blocked_path);
    let mut candidate = blocked.current();
    candidate.analysis_mode = AnalysisMode::Thorough;
    assert!(blocked.update(candidate).is_err());
    assert_eq!(blocked.current().analysis_mode, AnalysisMode::Sampled);
    assert_eq!(
        std::fs::read(blocked_path.join("sentinel")).unwrap(),
        b"keep"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn windows_path_separators_do_not_escape_glob_characters() {
    let mut settings = Config::default();
    settings.excludes = vec![r"C:\Games\*\cache\*".to_string()];
    let exclusions = settings.globset().unwrap();
    assert!(exclusions.is_match(r"C:\Games\Example\cache\data.bin"));
    assert!(!exclusions.is_match(r"C:\Games\Example\content\data.bin"));
}
