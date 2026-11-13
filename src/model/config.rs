//! Where legcon keeps its files, and writing them so a crash never leaves half a file.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Overrides the config folder (tests, portable installs).
pub const ENV: &str = "LEGCON_CONFIG_DIR";

/// The config folder: `%APPDATA%\legcon`, `~/Library/Application Support/legcon`, `~/.config/legcon`.
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os(ENV).filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("legcon")
}

/// Write `data` to `path` through a temporary file in the same folder, then rename it over.
pub fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
