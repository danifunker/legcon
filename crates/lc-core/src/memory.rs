//! An in-memory machine: a [`FileSession`] over a tree held in RAM, for tests and the demo.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::session::{ConnState, Connection, FileCaps, FileSession, WriteMode};
use crate::{Entry, EntryKind, Error, PathStyle, RemotePath, Result};

#[derive(Debug, Clone)]
enum Node {
    File(Vec<u8>),
    Folder,
}

/// A tree of folders and files, keyed by the path's display form.
#[derive(Debug, Default)]
struct Tree {
    nodes: BTreeMap<String, Node>,
}

/// A connected in-memory machine; [`MemoryFs::disconnect`] records that it logged off.
#[derive(Debug, Clone)]
pub struct MemoryFs {
    style: PathStyle,
    home: RemotePath,
    tree: Arc<Mutex<Tree>>,
    state: ConnState,
    pub logged_off: Arc<Mutex<bool>>,
    /// Set to act as a server that closed the link: every call, ping included, fails.
    pub dropped: Arc<std::sync::atomic::AtomicBool>,
    /// How many pings the server has answered.
    pub pings: Arc<std::sync::atomic::AtomicUsize>,
}

impl MemoryFs {
    /// An empty machine whose login folder is `home` (created with its parents).
    pub fn new(style: PathStyle, home: &str) -> Self {
        let fs = MemoryFs {
            style,
            home: RemotePath::parse(style, home),
            tree: Arc::default(),
            state: ConnState::Connected,
            logged_off: Arc::default(),
            dropped: Arc::default(),
            pings: Arc::default(),
        };
        fs.add_folder(home);
        fs
    }

    /// Add a folder and its parents.
    pub fn add_folder(&self, path: &str) {
        let p = RemotePath::parse(self.style, path);
        let mut tree = self.tree.lock().unwrap();
        let mut at = RemotePath::root(self.style);
        for part in p.parts() {
            at = at.join(part);
            tree.nodes.entry(at.to_string()).or_insert(Node::Folder);
        }
    }

    /// Add a file (and its folder).
    pub fn add_file(&self, path: &str, data: &[u8]) {
        let p = RemotePath::parse(self.style, path);
        if let Some(parent) = p.parent() {
            self.add_folder(&parent.to_string());
        }
        self.tree
            .lock()
            .unwrap()
            .nodes
            .insert(p.to_string(), Node::File(data.to_vec()));
    }

    fn live(&self) -> Result<()> {
        if self.dropped.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Lost("the server closed the connection".into()));
        }
        match self.state {
            ConnState::Connected | ConnState::Busy => Ok(()),
            _ => Err(Error::NotConnected),
        }
    }
}

impl Connection for MemoryFs {
    fn state(&self) -> ConnState {
        self.state
    }

    fn disconnect(&mut self, _timeout: Duration) -> Result<()> {
        *self.logged_off.lock().unwrap() = true;
        self.state = ConnState::Disconnected;
        Ok(())
    }

    fn ping(&mut self, _timeout: Duration) -> Result<()> {
        self.live()?;
        self.pings
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// Collects an upload and stores it in the tree when dropped.
struct Upload {
    tree: Arc<Mutex<Tree>>,
    key: String,
    data: Vec<u8>,
}

impl Write for Upload {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        let data = std::mem::take(&mut self.data);
        if let Ok(mut tree) = self.tree.lock() {
            tree.nodes.insert(self.key.clone(), Node::File(data));
        }
    }
}

impl FileSession for MemoryFs {
    fn caps(&self) -> FileCaps {
        FileCaps {
            random_read: true,
            random_write: false,
            resume_upload: true,
            rename: true,
            delete: true,
            mkdir: true,
            set_times: false,
            set_mode: false,
        }
    }

    fn home(&mut self) -> Result<RemotePath> {
        self.live()?;
        Ok(self.home.clone())
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        self.live()?;
        let tree = self.tree.lock().unwrap();
        if !dir.is_root() && !matches!(tree.nodes.get(&dir.to_string()), Some(Node::Folder)) {
            return Err(Error::NotFound(dir.to_string()));
        }
        let mut out = Vec::new();
        for (key, node) in &tree.nodes {
            let p = RemotePath::parse(self.style, key);
            if p.parent().as_ref() == Some(dir) {
                let mut e = match node {
                    Node::Folder => Entry::new(p.name(), EntryKind::Folder),
                    Node::File(d) => {
                        let mut e = Entry::new(p.name(), EntryKind::File);
                        e.size = Some(d.len() as u64);
                        e
                    }
                };
                e.mode = Some(if e.is_folder() { 0o755 } else { 0o644 });
                out.push(e);
            }
        }
        Ok(out)
    }

    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        let parent = path.parent().ok_or_else(|| Error::NotFound("/".into()))?;
        self.list(&parent)?
            .into_iter()
            .find(|e| e.name == path.name())
            .ok_or_else(|| Error::NotFound(path.to_string()))
    }

    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        self.live()?;
        match self.tree.lock().unwrap().nodes.get(&path.to_string()) {
            Some(Node::File(d)) => {
                let start = (at as usize).min(d.len());
                Ok(Box::new(Cursor::new(d[start..].to_vec())))
            }
            _ => Err(Error::NotFound(path.to_string())),
        }
    }

    fn open_write(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>> {
        self.live()?;
        let key = path.to_string();
        let exists = self.tree.lock().unwrap().nodes.contains_key(&key);
        // As a real server: no file in a folder that is not there.
        if let Some(parent) = path.parent().filter(|p| !p.is_root()) {
            let tree = self.tree.lock().unwrap();
            if !matches!(tree.nodes.get(&parent.to_string()), Some(Node::Folder)) {
                return Err(Error::NotFound(parent.to_string()));
            }
        }
        match mode {
            WriteMode::Create if exists => return Err(Error::Denied(format!("{key} exists"))),
            _ => {}
        }
        let data = match (mode, self.tree.lock().unwrap().nodes.get(&key)) {
            (WriteMode::Resume(at), Some(Node::File(d))) => {
                d[..(at as usize).min(d.len())].to_vec()
            }
            _ => Vec::new(),
        };
        Ok(Box::new(Upload {
            tree: Arc::clone(&self.tree),
            key,
            data,
        }))
    }

    fn checksum(&mut self, path: &RemotePath) -> Result<Option<crate::Checksum>> {
        self.live()?;
        match self.tree.lock().unwrap().nodes.get(&path.to_string()) {
            Some(Node::File(d)) => Ok(Some(crate::Checksum::Crc32(
                crate::checksum::crc32(&mut &d[..]).map_err(Error::Io)?,
            ))),
            _ => Err(Error::NotFound(path.to_string())),
        }
    }

    fn mkdir(&mut self, path: &RemotePath) -> Result<()> {
        self.live()?;
        self.add_folder(&path.to_string());
        Ok(())
    }

    fn remove(&mut self, path: &RemotePath) -> Result<()> {
        self.live()?;
        let key = path.to_string();
        let mut tree = self.tree.lock().unwrap();
        let below = format!("{key}{}", self.style.separator());
        if tree.nodes.keys().any(|k| k.starts_with(&below)) {
            return Err(Error::Denied(format!("{key} is not empty")));
        }
        tree.nodes
            .remove(&key)
            .map(|_| ())
            .ok_or(Error::NotFound(key))
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        self.live()?;
        let mut tree = self.tree.lock().unwrap();
        let node = tree
            .nodes
            .remove(&from.to_string())
            .ok_or_else(|| Error::NotFound(from.to_string()))?;
        tree.nodes.insert(to.to_string(), node);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_read_write_and_log_off() {
        let mut fs = MemoryFs::new(PathStyle::Unix, "/export/home/dani");
        fs.add_file("/export/home/dani/notes.txt", b"hello");
        let home = fs.home().unwrap();
        let names: Vec<_> = fs
            .list(&home)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["notes.txt"]);
        let mut s = String::new();
        fs.open_read(&home.join("notes.txt"), 2)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "llo");
        {
            let mut w = fs
                .open_write(&home.join("new.txt"), WriteMode::Create)
                .unwrap();
            w.write_all(b"abc").unwrap();
        }
        assert_eq!(fs.stat(&home.join("new.txt")).unwrap().size, Some(3));
        fs.disconnect(Duration::from_secs(1)).unwrap();
        assert!(*fs.logged_off.lock().unwrap());
        assert!(matches!(fs.list(&home), Err(Error::NotConnected)));
    }
}
