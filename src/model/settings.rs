//! App-wide settings (`settings.json`): the home folder and the default layout.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The share of the height the files area gets by default (the terminal area gets the rest).
pub const DEFAULT_FILES_SHARE: f32 = 0.75;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The local folder every connection starts in; `None` until the first run asks.
    pub home_folder: Option<PathBuf>,
    pub files_share: f32,
    /// Local folders files were last copied to or from (or gone to), newest first.
    pub recent_places: Vec<PathBuf>,
    /// Local folders the user pinned to the Places menu, in the order pinned.
    pub pinned_places: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            home_folder: None,
            files_share: DEFAULT_FILES_SHARE,
            recent_places: Vec::new(),
            pinned_places: Vec::new(),
        }
    }
}

/// How many recent places are kept (locally, and per machine).
pub const RECENT_MAX: usize = 8;

impl Settings {
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }

    /// Read `settings.json` from `dir`; a missing file gives the defaults.
    pub fn load(dir: &Path) -> anyhow::Result<Settings> {
        match std::fs::read(Self::path_in(dir)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        super::config::write_atomic(&Self::path_in(dir), &serde_json::to_vec_pretty(self)?)
    }

    /// Remember a place files were copied to or from, newest first, without duplicates.
    pub fn add_recent(&mut self, place: PathBuf) {
        add_recent(&mut self.recent_places, place);
    }

    /// Pin a place to the Places menu, or unpin it.
    pub fn set_pinned(&mut self, place: PathBuf, pinned: bool) {
        self.pinned_places.retain(|p| p != &place);
        if pinned {
            self.pinned_places.push(place);
        }
    }
}

/// Put `item` first in a recent list, without duplicates, keeping at most `RECENT_MAX`.
pub fn add_recent<T: PartialEq>(list: &mut Vec<T>, item: T) {
    list.retain(|p| p != &item);
    list.insert(0, item);
    list.truncate(RECENT_MAX);
}

impl Settings {
    /// For tests and the CLI: whether `place` is pinned.
    pub fn is_pinned(&self, place: &Path) -> bool {
        self.pinned_places.iter().any(|p| p == place)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_recent_places() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), Settings::default());
        let mut s = Settings {
            home_folder: Some(PathBuf::from("D:/Retro")),
            ..Settings::default()
        };
        for p in ["a", "b", "a"] {
            s.add_recent(PathBuf::from(p));
        }
        assert_eq!(s.recent_places, [PathBuf::from("a"), PathBuf::from("b")]);
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()).unwrap(), s);
    }
}
