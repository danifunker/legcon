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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_folders_exist_and_drives_include_the_system_one() {
        assert!(system_places().iter().all(|p| p.path.is_dir()));
        let d = drives();
        assert!(!d.is_empty() && d.iter().all(|p| p.is_dir()));
    }
}
