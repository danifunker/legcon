//! Paths on a remote machine, kept in the remote's own style so nothing above guesses.

/// How a machine spells paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PathStyle {
    /// `/export/home/dani`, case-sensitive.
    #[default]
    Unix,
    /// `\WINDOWS\SYSTEM` on a share, case-insensitive (DOS, Windows, OS/2 over SMB or FTP).
    Dos,
    /// `Macintosh HD:System Folder`, case-insensitive (classic Mac OS over AFP or FTP).
    Mac,
}

impl PathStyle {
    pub fn separator(self) -> char {
        match self {
            PathStyle::Unix => '/',
            PathStyle::Dos => '\\',
            PathStyle::Mac => ':',
        }
    }

    pub fn case_sensitive(self) -> bool {
        matches!(self, PathStyle::Unix)
    }
}

/// An absolute path on a remote machine: its components and the style they are joined with.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct RemotePath {
    pub style: PathStyle,
    parts: Vec<String>,
}

impl RemotePath {
    /// The root (`/`, `\`, or the top of a Mac volume list).
    pub fn root(style: PathStyle) -> Self {
        RemotePath {
            style,
            parts: Vec::new(),
        }
    }

    /// Parse `text` in `style`; empty components and `.` are dropped, `..` steps up.
    pub fn parse(style: PathStyle, text: &str) -> Self {
        let mut p = RemotePath::root(style);
        for part in text.split(style.separator()) {
            match part {
                "" | "." => {}
                ".." => {
                    p.parts.pop();
                }
                other => p.parts.push(other.to_string()),
            }
        }
        p
    }

    pub fn parts(&self) -> &[String] {
        &self.parts
    }

    pub fn is_root(&self) -> bool {
        self.parts.is_empty()
    }

    /// The last component, or empty at the root.
    pub fn name(&self) -> &str {
        self.parts.last().map(String::as_str).unwrap_or("")
    }

    pub fn join(&self, name: &str) -> Self {
        let mut p = self.clone();
        p.parts.push(name.to_string());
        p
    }

    pub fn parent(&self) -> Option<Self> {
        if self.parts.is_empty() {
            return None;
        }
        let mut p = self.clone();
        p.parts.pop();
        Some(p)
    }

    /// Whether `self` is `other` or below it, by the style's case rule.
    pub fn starts_with(&self, other: &RemotePath) -> bool {
        other.parts.len() <= self.parts.len()
            && other.parts.iter().zip(&self.parts).all(|(a, b)| {
                if self.style.case_sensitive() {
                    a == b
                } else {
                    a.eq_ignore_ascii_case(b)
                }
            })
    }
}

impl std::fmt::Display for RemotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sep = self.style.separator();
        match self.style {
            PathStyle::Mac => write!(f, "{}", self.parts.join(":")),
            _ => {
                if self.parts.is_empty() {
                    return write!(f, "{sep}");
                }
                for part in &self.parts {
                    write!(f, "{sep}{part}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_parse_display_and_dotdot() {
        let p = RemotePath::parse(PathStyle::Unix, "/export//home/./dani/../dani/src");
        assert_eq!(p.to_string(), "/export/home/dani/src");
        assert_eq!(p.name(), "src");
        assert_eq!(p.parent().unwrap().to_string(), "/export/home/dani");
        assert_eq!(RemotePath::root(PathStyle::Unix).to_string(), "/");
    }

    #[test]
    fn dos_paths_compare_without_case() {
        let home = RemotePath::parse(PathStyle::Dos, r"\WINDOWS");
        let sys = RemotePath::parse(PathStyle::Dos, r"\windows\SYSTEM");
        assert!(sys.starts_with(&home));
        assert_eq!(sys.to_string(), r"\windows\SYSTEM");
        let unix = RemotePath::parse(PathStyle::Unix, "/Usr/local");
        assert!(!unix.starts_with(&RemotePath::parse(PathStyle::Unix, "/usr")));
    }
}
