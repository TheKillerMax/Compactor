use directories::ProjectDirs;
use hashfilter::HashFilter;
use lazy_static::lazy_static;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::compact::Compression;
use crate::config::{AnalysisMode, ConfigFile};

lazy_static! {
    static ref PATHDB: RwLock<HashFilter> = RwLock::new(HashFilter::default());
    static ref CONFIG: RwLock<ConfigFile> = RwLock::new(ConfigFile::default());
}

pub fn init() {
    if let Some(dirs) = ProjectDirs::from("", "Freaky", "Compactor") {
        // V3 keys also include algorithm, analysis mode, threshold and allocation
        // unit. A rejected XPRESS attempt must not suppress a later LZX attempt.
        pathdb()
            .write()
            .unwrap()
            .set_backing(dirs.cache_dir().join("incompressible-v3.dat"));
        *config().write().unwrap() = ConfigFile::new(dirs.config_dir().join("config.json"));
    }
}

pub fn incompressible_key(
    path: &Path,
    content_len: u64,
    modified_time: u64,
    compression: Compression,
    analysis_mode: AnalysisMode,
    min_savings_percent: f32,
    cluster_size: u64,
) -> (PathBuf, u64, u64, Compression, AnalysisMode, u32, u64) {
    (
        path.to_path_buf(),
        content_len,
        modified_time,
        compression,
        analysis_mode,
        min_savings_percent.to_bits(),
        cluster_size,
    )
}

pub fn config() -> &'static RwLock<ConfigFile> {
    &CONFIG
}

pub fn pathdb() -> &'static RwLock<HashFilter> {
    &PATHDB
}

#[test]
fn cached_results_are_scoped_to_the_operation_that_measured_them() {
    let path = Path::new(r"C:\Games\data.bin");
    let key = |len, modified, algorithm, mode, minimum, cluster| {
        incompressible_key(path, len, modified, algorithm, mode, minimum, cluster)
    };
    let original = key(
        8192,
        12,
        Compression::Xpress4k,
        AnalysisMode::Sampled,
        1.0,
        4096,
    );
    assert_eq!(
        original,
        key(
            8192,
            12,
            Compression::Xpress4k,
            AnalysisMode::Sampled,
            1.0,
            4096
        )
    );
    for changed in [
        key(
            8192,
            13,
            Compression::Xpress4k,
            AnalysisMode::Sampled,
            1.0,
            4096,
        ),
        key(
            16384,
            12,
            Compression::Xpress4k,
            AnalysisMode::Sampled,
            1.0,
            4096,
        ),
        key(8192, 12, Compression::Lzx, AnalysisMode::Sampled, 1.0, 4096),
        key(
            8192,
            12,
            Compression::Xpress4k,
            AnalysisMode::Thorough,
            1.0,
            4096,
        ),
        key(
            8192,
            12,
            Compression::Xpress4k,
            AnalysisMode::Sampled,
            0.0,
            4096,
        ),
        key(
            8192,
            12,
            Compression::Xpress4k,
            AnalysisMode::Sampled,
            1.0,
            65536,
        ),
    ] {
        assert_ne!(original, changed);
    }
}
