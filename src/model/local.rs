//! This computer's files, as the local pane lists them.

use std::path::Path;
use std::time::UNIX_EPOCH;

/// One entry of a local folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since 1970, when the file system gives it.
    pub modified: Option<i64>,
}

/// List `dir`: folders first, then files, each by name ignoring case.
pub fn list_dir(dir: &Path) -> std::io::Result<Vec<LocalEntry>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        // An entry that vanished or can't be read is skipped, not fatal for the whole folder.
        let Ok(meta) = item.metadata() else { continue };
        out.push(LocalEntry {
            name: item.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_first_then_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), b"12").unwrap();
        std::fs::write(dir.path().join("A.txt"), b"1").unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        let names: Vec<_> = list_dir(dir.path())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["zdir", "A.txt", "b.txt"]);
    }
}
