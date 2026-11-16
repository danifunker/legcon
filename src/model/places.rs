//! Places on this computer for the Places menu: the user's known folders and the drives.

use std::path::{Path, PathBuf};

/// A named folder, such as Desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
}

/// The user folder, Desktop, Documents and Downloads, where they exist (the OS says where).
pub fn system_places() -> Vec<Place> {
    [
        ("User folder", dirs::home_dir()),
        ("Desktop", dirs::desktop_dir()),
        ("Documents", dirs::document_dir()),
        ("Downloads", dirs::download_dir()),
    ]
    .into_iter()
    .filter_map(|(label, path)| {
        let path = path?;
        path.is_dir().then(|| Place {
            label: label.to_string(),
            path,
        })
    })
    .collect()
}

/// The drives (`C:\` and so on) on Windows, the root elsewhere; may be slow, so run it as a job.
pub fn drives() -> Vec<PathBuf> {
    if cfg!(windows) {
        // A and B are floppy drives: asking an empty one can stall for seconds.
        (b'C'..=b'Z')
            .map(|c| PathBuf::from(format!("{}:\\", c as char)))
            .filter(|p| p.is_dir())
            .collect()
    } else {
        vec![Path::new("/").to_path_buf()]
    }
}

/// Removable media and other volumes mounted now: `/Volumes` on macOS (not the startup disk),
/// `/media/$USER`, `/run/media/$USER` and `/mnt` on Linux. Windows drives are in [`drives`].
pub fn volumes() -> Vec<Place> {
    let user = std::env::var("USER").unwrap_or_default();
    let roots: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![PathBuf::from("/Volumes")]
    } else if cfg!(unix) {
        vec![
            PathBuf::from("/media").join(&user),
            PathBuf::from("/run/media").join(&user),
            PathBuf::from("/mnt"),
        ]
    } else {
        Vec::new()
    };
    let root = std::fs::canonicalize("/").ok();
    let mut out = Vec::new();
    for r in roots {
        let Ok(rd) = std::fs::read_dir(&r) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            // The startup disk appears in /Volumes as a link to /.
            if !path.is_dir() || std::fs::canonicalize(&path).ok() == root {
                continue;
            }
            out.push(Place {
                label: e.file_name().to_string_lossy().into_owned(),
                path,
            });
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// The folders classic Mac emulators share with the Mac inside them: Basilisk II's and
/// SheepShaver's `extfs` setting, where it names a folder that exists.
pub fn emulator_folders() -> Vec<Place> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    [
        ("Basilisk II shared folder", ".basilisk_ii_prefs"),
        ("SheepShaver shared folder", ".sheepshaver_prefs"),
    ]
    .into_iter()
    .filter_map(|(label, file)| {
        let text = std::fs::read_to_string(home.join(file)).ok()?;
        extfs(&text).map(|path| Place {
            label: label.to_string(),
            path,
        })
    })
    .collect()
}

/// The folder an emulator prefs file's `extfs` line names, if it exists.
fn extfs(prefs: &str) -> Option<PathBuf> {
    let line = prefs
        .lines()
        .find_map(|l| l.trim().strip_prefix("extfs "))?;
    let path = PathBuf::from(line.trim());
    path.is_dir().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_folders_exist_and_drives_include_the_system_one() {
        assert!(system_places().iter().all(|p| p.path.is_dir()));
        let d = drives();
        assert!(!d.is_empty() && d.iter().all(|p| p.is_dir()));
        let root = std::fs::canonicalize("/").ok();
        assert!(volumes()
            .iter()
            .all(|v| v.path.is_dir() && std::fs::canonicalize(&v.path).ok() != root));
    }

    #[test]
    fn emulator_prefs_name_their_shared_folder() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = format!(
            "disk /x.hfv\nextfs {}\nramsize 8388608\n",
            dir.path().display()
        );
        assert_eq!(extfs(&prefs), Some(dir.path().to_path_buf()));
        assert_eq!(extfs("extfs /no/such/folder\n"), None);
        assert_eq!(extfs("rom /x.rom\n"), None);
    }
}
