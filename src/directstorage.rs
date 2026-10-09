use crate::background::{Background, ControlToken};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

const DIRECT_STORAGE_DLLS: &[&str] = &["dstorage.dll", "dstoragecore.dll"];

// These are library/container directories, not individual games. When a
// DirectStorage runtime is found below one of these, protect only the immediate
// game/install child instead of excluding the entire library.
const LIBRARY_CONTAINERS: &[&str] = &[
    "steamapps",
    "xboxgames",
    "windowsapps",
    "program files",
    "program files (x86)",
    "program files (arm)",
    "programdata",
    "epic games",
    "epicgames",
    "egs",
    "gog galaxy",
    "gog galaxy games",
    "gog games",
    "ubisoft",
    "ubisoft game launcher",
    "origin games",
    "origin",
    "ea games",
    "electronic arts",
    "battle.net",
    "riot games",
    "games",
    "my games",
    "common files",
    "amazon games",
];

const HARD_TOO_BROAD: &[&str] = &["windows", "users", "appdata"];

fn lower_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn normalized(path: &Path) -> String {
    let value = path
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();

    // All production paths come from the folder picker and are absolute, but
    // normalising the separator after a drive prefix also makes internal path
    // comparisons deterministic for PathBuf values assembled in tests/helpers.
    if value.as_bytes().get(1) == Some(&b':') && value.len() > 2 {
        let rest = value[2..].trim_start_matches('\\');
        if rest.is_empty() {
            value[..2].to_string()
        } else {
            format!("{}\\{}", &value[..2], rest)
        }
    } else {
        value
    }
}

pub fn same_path(left: &Path, right: &Path) -> bool {
    normalized(left) == normalized(right)
}

pub fn is_under(path: &Path, root: &Path) -> bool {
    let path = normalized(path);
    let root = normalized(root);
    path == root
        || path
            .strip_prefix(&root)
            .map(|rest| rest.starts_with('\\'))
            .unwrap_or(false)
}

pub fn is_direct_storage_runtime(path: &Path) -> bool {
    let name = lower_name(path);
    DIRECT_STORAGE_DLLS
        .iter()
        .any(|candidate| *candidate == name)
}

fn is_drive_root(path: &Path) -> bool {
    path.parent().map(|parent| parent == path).unwrap_or(true)
}

fn is_library_container(path: &Path) -> bool {
    let name = lower_name(path);
    if LIBRARY_CONTAINERS
        .iter()
        .any(|candidate| *candidate == name)
    {
        return true;
    }

    let parent = path.parent().map(lower_name).unwrap_or_default();
    (name == "common" && parent == "steamapps") || (name == "library" && parent == "amazon games")
}

fn too_wide_to_skip(path: &Path) -> bool {
    let name = lower_name(path);
    is_drive_root(path)
        || is_library_container(path)
        || HARD_TOO_BROAD.iter().any(|candidate| *candidate == name)
}

fn unreal_project_root(dll_parent: &Path) -> Option<PathBuf> {
    for current in dll_parent.ancestors() {
        if !lower_name(current).eq_ignore_ascii_case("directstorage") {
            continue;
        }

        let windows = current.parent()?;
        let third_party = windows.parent()?;
        let binaries = third_party.parent()?;
        let engine = binaries.parent()?;

        if lower_name(windows) == "windows"
            && lower_name(third_party) == "thirdparty"
            && lower_name(binaries) == "binaries"
            && lower_name(engine) == "engine"
        {
            return engine.parent().map(Path::to_path_buf);
        }
    }

    None
}

fn safe_root(candidate: PathBuf, dll_path: &Path, scan_root: &Path) -> PathBuf {
    if !is_under(&candidate, scan_root) || too_wide_to_skip(&candidate) {
        return dll_path
            .parent()
            .filter(|parent| is_under(parent, scan_root))
            .unwrap_or(scan_root)
            .to_path_buf();
    }

    candidate
}

fn infer_game_root(dll_path: &Path, scan_root: &Path, exe_dirs: &HashSet<String>) -> PathBuf {
    let dll_parent = dll_path.parent().unwrap_or(scan_root);

    if let Some(root) = unreal_project_root(dll_parent) {
        return safe_root(root, dll_path, scan_root);
    }

    let mut current = dll_parent.to_path_buf();
    let mut below: Option<PathBuf> = None;
    let mut nearest_exe: Option<PathBuf> = None;

    loop {
        if nearest_exe.is_none() && exe_dirs.contains(&normalized(&current)) {
            nearest_exe = Some(current.clone());
        }

        if is_library_container(&current) {
            if let Some(game) = below {
                return safe_root(game, dll_path, scan_root);
            }
            break;
        }

        if HARD_TOO_BROAD
            .iter()
            .any(|candidate| *candidate == lower_name(&current))
        {
            break;
        }

        if normalized(&current) == normalized(scan_root) {
            break;
        }

        let Some(parent) = current.parent().map(Path::to_path_buf) else {
            break;
        };
        if parent == current {
            break;
        }

        below = Some(current);
        current = parent;
    }

    // If the user selected a single game/install directory, protecting that
    // whole target is safer and clearer than guessing at a nested Binaries/bin
    // directory. For broad library roots, prefer the closest directory that
    // actually contains an executable, then fall back to the DLL's directory.
    if !too_wide_to_skip(scan_root) {
        return scan_root.to_path_buf();
    }

    if let Some(exe) = nearest_exe {
        return safe_root(exe, dll_path, scan_root);
    }

    safe_root(dll_parent.to_path_buf(), dll_path, scan_root)
}

fn collapse_roots(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort_by_key(|path| path.components().count());
    let mut kept: Vec<PathBuf> = Vec::new();

    for root in roots {
        if kept.iter().any(|existing| is_under(&root, existing)) {
            continue;
        }
        kept.push(root);
    }

    kept
}

pub struct DirectStorageScan {
    pub path: PathBuf,
}

impl Background for DirectStorageScan {
    type Output = io::Result<Vec<PathBuf>>;
    type Status = ();

    fn run(self, control: &ControlToken<()>) -> Self::Output {
        discover_direct_storage_roots(&self.path, || control.is_cancelled_with_pause())
    }
}

fn discover_direct_storage_roots(
    scan_root: &Path,
    mut cancelled: impl FnMut() -> bool,
) -> io::Result<Vec<PathBuf>> {
    let mut dlls = Vec::new();
    let mut exe_dirs = HashSet::new();

    for entry in WalkDir::new(scan_root).follow_links(false).into_iter() {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "DirectStorage check stopped",
            ));
        }
        // An incomplete detection cannot establish that an unreadable subtree
        // is safe. Report the failure before transforming any files.
        let entry = entry.map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
        let path = entry.path();
        // A named runtime link is still evidence of DirectStorage. Record its
        // name without following it into an unselected tree.
        if is_direct_storage_runtime(path) {
            dlls.push(path.to_path_buf());
        }
        if !entry.file_type().is_file() {
            continue;
        }

        if path
            .extension()
            .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
        {
            if let Some(parent) = path.parent() {
                exe_dirs.insert(normalized(parent));
            }
        }
    }

    Ok(collapse_roots(
        dlls.iter()
            .map(|dll| infer_game_root(dll, scan_root, &exe_dirs))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_is_cancellable_and_does_not_use_compression_exclusions() {
        let dir = tempdir::TempDir::new("compactor-directstorage").unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("dstorage.dll"), []).unwrap();
        assert_eq!(
            vec![dir.path().to_path_buf()],
            discover_direct_storage_roots(dir.path(), || false).unwrap()
        );
        assert_eq!(
            io::ErrorKind::Interrupted,
            discover_direct_storage_roots(dir.path(), || true)
                .unwrap_err()
                .kind()
        );
    }

    #[test]
    fn steam_unreal_runtime_maps_to_game_root() {
        let scan = Path::new(r"D:\SteamLibrary\steamapps\common");
        let dll = Path::new(
            r"D:\SteamLibrary\steamapps\common\ExampleGame\Engine\Binaries\ThirdParty\Windows\DirectStorage\x64\dstorage.dll",
        );
        let root = infer_game_root(dll, scan, &HashSet::new());
        assert_eq!(
            normalized(&root),
            normalized(Path::new(r"D:\SteamLibrary\steamapps\common\ExampleGame"))
        );
    }

    #[test]
    fn epic_library_runtime_maps_to_immediate_game_child() {
        let scan = Path::new(r"D:\Epic Games");
        let dll = Path::new(r"D:\Epic Games\ExampleGame\bin\dstorage.dll");
        let root = infer_game_root(dll, scan, &HashSet::new());
        assert_eq!(
            normalized(&root),
            normalized(Path::new(r"D:\Epic Games\ExampleGame"))
        );
    }

    #[test]
    fn single_game_target_protects_the_selected_game() {
        let scan = Path::new(r"D:\Games\ExampleGame");
        let dll = Path::new(r"D:\Games\ExampleGame\bin\dstoragecore.dll");
        let root = infer_game_root(dll, scan, &HashSet::new());
        assert_eq!(normalized(&root), normalized(scan));
    }

    #[test]
    fn scope_comparison_accepts_verbatim_prefix_but_respects_directory_boundaries() {
        assert!(is_under(
            Path::new(r"\\?\C:\Games\A\data.bin"),
            Path::new(r"c:\games\a")
        ));
        assert!(!is_under(
            Path::new(r"C:\Games\AB\data.bin"),
            Path::new(r"C:\Games\A")
        ));
        assert!(!is_under(
            Path::new(r"D:\Games\A\data.bin"),
            Path::new(r"C:\Games\A")
        ));
    }

    #[test]
    fn nested_roots_are_collapsed() {
        let roots = collapse_roots(vec![
            PathBuf::from(r"D:\Games\Foo\Engine"),
            PathBuf::from(r"D:\Games\Foo"),
            PathBuf::from(r"D:\Games\Bar"),
        ]);
        assert_eq!(2, roots.len());
        assert!(roots
            .iter()
            .any(|path| normalized(path) == normalized(Path::new(r"D:\Games\Foo"))));
        assert!(roots
            .iter()
            .any(|path| normalized(path) == normalized(Path::new(r"D:\Games\Bar"))));
    }
}
