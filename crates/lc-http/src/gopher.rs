//! Gopher (RFC 1436): menus are folders, documents and binaries are files.
//!
//! A menu line is a type character, the display name, a tab, the selector, the host and the port.
//! A path in the remote pane is a chain of display names; each is found in its parent menu to get
//! the selector to ask for. Items on other hosts, info lines, searches and Telnet links are left out.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::time::Duration;

use lc_core::{
    ConnState, Connection, Entry, EntryKind, Error, FileCaps, FileSession, PathStyle, RemotePath,
    Result, WriteMode,
};

use crate::{dial, part_name, read_only, Options};

pub const PORT: u16 = 70;

/// One menu line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: u8,
    pub display: Vec<u8>,
    pub selector: Vec<u8>,
    pub host: String,
    pub port: u16,
}

/// Parse a menu, up to its closing `.` line.
pub fn parse_menu(data: &[u8]) -> Vec<Item> {
    let mut out = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line == b"." {
            break;
        }
        let Some((&kind, rest)) = line.split_first() else {
            continue;
        };
        let f: Vec<&[u8]> = rest.split(|&b| b == b'\t').collect();
        if f.len() < 4 {
            continue;
        }
        out.push(Item {
            kind,
            display: listing_name(f[0], f[1]),
            selector: f[1].to_vec(),
            host: String::from_utf8_lossy(f[2]).into_owned(),
            port: String::from_utf8_lossy(f[3]).trim().parse().unwrap_or(PORT),
        });
    }
    out
}

/// The name in a directory listing's display text: Gophernicus and others pad it with date and
/// size columns, so the selector's last part followed by two spaces is the name.
fn listing_name(display: &[u8], selector: &[u8]) -> Vec<u8> {
    let path = selector.strip_suffix(b"/").unwrap_or(selector);
    let base = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let padded = display.len() > base.len() && display[base.len()..].starts_with(b"  ");
    if !base.is_empty() && display.starts_with(base) && padded {
        base.to_vec()
    } else {
        display.to_vec()
    }
}

/// Menus and documents shown; info, errors, searches, Telnet and CSO lines are not.
fn shown(kind: u8) -> Option<EntryKind> {
    match kind {
        b'1' => Some(EntryKind::Folder),
        b'0' | b'4' | b'5' | b'6' | b'9' | b'g' | b'I' | b'h' | b's' | b'd' | b'p' | b'c'
        | b';' | b'P' => Some(EntryKind::File),
        _ => None,
    }
}

/// A Gopher hole: its menus, fetched as they are opened.
pub struct GopherSession {
    host: String,
    port: u16,
    opts: Options,
    menus: HashMap<String, Vec<Item>>,
    closed: bool,
}

/// Fetch the root menu to check the server answers.
pub fn connect(host: &str, port: u16, opts: &Options) -> Result<GopherSession> {
    let mut s = GopherSession {
        host: host.to_string(),
        port,
        opts: opts.clone(),
        menus: HashMap::new(),
        closed: false,
    };
    s.menu(&RemotePath::root(PathStyle::Unix))?;
    Ok(s)
}

impl GopherSession {
    fn fetch(&self, selector: &[u8]) -> Result<Vec<u8>> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        let mut s = dial(&self.host, self.port, &self.opts)?;
        let mut req = selector.to_vec();
        req.extend_from_slice(b"\r\n");
        s.write_all(&req)?;
        let mut out = Vec::new();
        s.read_to_end(&mut out)?;
        Ok(out)
    }

    fn here(&self, i: &Item) -> bool {
        i.port == self.port && i.host.eq_ignore_ascii_case(&self.host)
    }

    fn name(&self, i: &Item) -> String {
        part_name(&self.opts.charset.decode(&i.display))
    }

    /// The item a path names, looked up in its parent menu.
    fn item(&mut self, path: &RemotePath) -> Result<Item> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        let items = self.menu(&parent)?;
        items
            .into_iter()
            .find(|i| shown(i.kind).is_some() && self.here(i) && self.name(i) == path.name())
            .ok_or_else(|| Error::NotFound(path.to_string()))
    }

    fn menu(&mut self, dir: &RemotePath) -> Result<Vec<Item>> {
        let key = dir.to_string();
        if let Some(m) = self.menus.get(&key) {
            return Ok(m.clone());
        }
        let selector = if dir.is_root() {
            Vec::new()
        } else {
            let i = self.item(dir)?;
            if i.kind != b'1' {
                return Err(Error::Unsupported(format!("{dir} is not a menu")));
            }
            i.selector
        };
        let items = parse_menu(&self.fetch(&selector)?);
        if let Some(e) = items.iter().find(|i| i.kind == b'3') {
            if items.len() == 1 {
                return Err(Error::NotFound(format!(
                    "{dir}: {}",
                    self.opts.charset.decode(&e.display)
                )));
            }
        }
        self.menus.insert(key, items.clone());
        Ok(items)
    }
}

impl Connection for GopherSession {
    fn state(&self) -> ConnState {
        if self.closed {
            ConnState::Disconnected
        } else {
            ConnState::Connected
        }
    }

    fn disconnect(&mut self, _timeout: Duration) -> Result<()> {
        self.closed = true;
        Ok(())
    }
}

impl FileSession for GopherSession {
    fn caps(&self) -> FileCaps {
        FileCaps::default()
    }

    fn home(&mut self) -> Result<RemotePath> {
        Ok(RemotePath::root(PathStyle::Unix))
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        let items = self.menu(dir)?;
        let mut out: Vec<Entry> = Vec::new();
        for i in items.iter().filter(|i| self.here(i)) {
            let Some(kind) = shown(i.kind) else {
                continue;
            };
            let name = self.name(i);
            if out.iter().any(|e| e.name == name) {
                continue;
            }
            let mut e = Entry::new(name, kind);
            e.raw_name = i.display.clone();
            out.push(e);
        }
        Ok(out)
    }

    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        if path.is_root() {
            return Ok(Entry::new("/", EntryKind::Folder));
        }
        let i = self.item(path)?;
        let mut e = Entry::new(self.name(&i), shown(i.kind).unwrap_or(EntryKind::Other));
        e.raw_name = i.display;
        Ok(e)
    }

    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        let i = self.item(path)?;
        if i.kind == b'1' {
            return Err(Error::Unsupported(format!("{path} is a menu")));
        }
        let mut data = self.fetch(&i.selector)?;
        // Text ends with a lone `.` line, which is not part of the document.
        if i.kind == b'0' {
            for end in [&b"\r\n.\r\n"[..], b"\n.\n", b"\r\n.", b"\n."] {
                if data.ends_with(end) {
                    let keep =
                        data.len() - end.len() + if end.starts_with(b"\r\n") { 2 } else { 1 };
                    data.truncate(keep);
                    break;
                }
            }
        }
        let mut c = Cursor::new(data);
        c.set_position(at);
        Ok(Box::new(c))
    }

    fn open_write(
        &mut self,
        _path: &RemotePath,
        _mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>> {
        Err(read_only())
    }

    fn mkdir(&mut self, _path: &RemotePath) -> Result<()> {
        Err(read_only())
    }

    fn remove(&mut self, _path: &RemotePath) -> Result<()> {
        Err(read_only())
    }

    fn rename(&mut self, _from: &RemotePath, _to: &RemotePath) -> Result<()> {
        Err(read_only())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_lines() {
        let m = parse_menu(
            b"iWelcome\t\terror.host\t1\r\n1Software\t/software\tgopher.example\t70\r\n0About/this\t/about.txt\tgopher.example\t70\r\n.\r\n",
        );
        assert_eq!(m.len(), 3);
        assert_eq!(m[1].kind, b'1');
        assert_eq!(m[1].selector, b"/software");
        assert_eq!(
            part_name(&String::from_utf8_lossy(&m[2].display)),
            "About-this"
        );
    }

    #[test]
    fn listing_columns_are_not_part_of_the_name() {
        // As Gophernicus lists a folder (seen in the lab, 2026-11-14).
        let m = parse_menu(
            b"1notes                                 2026-Nov-14 21:45   --------/\t/notes/\th\t70\r\n0readme.txt                            2026-Nov-14 21:45     0.0 KB\t/readme.txt\th\t70\r\n0Read me first\t/readme.txt\th\t70\r\n",
        );
        assert_eq!(m[0].display, b"notes");
        assert_eq!(m[1].display, b"readme.txt");
        // A real title that is not padded stays as it is.
        assert_eq!(m[2].display, b"Read me first");
    }
}
