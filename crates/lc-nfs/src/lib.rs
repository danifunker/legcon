//! NFS v2 and v3 for legcon (docs/PLAN.md §4.1): our own ONC RPC, portmapper, MOUNT and NFS client.
//!
//! Connecting asks the portmapper for NFS and MOUNT (v3 first, then v2; UDP first, then TCP),
//! and reads the export list. The remote pane's root is a tree of folders made from the export
//! paths (`/export/home` shows as `export` > `home`); an export is mounted when first entered and
//! unmounted (`UMNT`) on disconnect, which is NFS's clean log-off. There is no password: the server
//! trusts the AUTH_UNIX uid and gid we send, so they are per-machine settings.

pub mod rpc;
pub mod xdr;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use lc_core::{
    Charset, ConnState, Connection, Entry, EntryKind, Error, FileCaps, FileSession, PathStyle,
    RemotePath, Result, TimePrecision, Timestamp, WriteMode,
};

use rpc::{Client, Cred, MOUNT_PROG, NFS_PROG, PMAP_PORT, PMAP_PROG};
use xdr::{Dec, Enc};

pub const PORT: u16 = 2049;

#[derive(Debug, Clone)]
pub struct Options {
    pub charset: Charset,
    pub uid: u32,
    pub gid: u32,
    /// NFS version 3 or 2; `None` tries 3, then 2.
    pub version: Option<u32>,
    /// `Some(true)` for TCP only, `Some(false)` for UDP only, `None` tries UDP, then TCP.
    pub tcp: Option<bool>,
    /// Send from a port below 1024, for servers that insist (falls back if it cannot bind one).
    pub reserved_port: bool,
    /// How long one call may take, retransmits included.
    pub timeout: Duration,
    /// The machine name in the credentials; some servers log it.
    pub client_name: String,
    /// Where the portmapper listens; 111 everywhere but in tests.
    pub portmap_port: u16,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            charset: Charset::default(),
            uid: 65534,
            gid: 65534,
            version: None,
            tcp: None,
            reserved_port: true,
            timeout: Duration::from_secs(10),
            client_name: "legcon".into(),
            portmap_port: PMAP_PORT,
        }
    }
}

/// File attributes, from either version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attr {
    /// 1 regular, 2 directory, 5 symlink, others are devices and the like.
    pub ftype: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub mtime: u32,
}

impl Attr {
    fn v2(d: &mut Dec) -> Result<Attr> {
        let ftype = d.u32()?;
        let mode = d.u32()?;
        d.u32()?; // nlink
        let (uid, gid) = (d.u32()?, d.u32()?);
        let size = u64::from(d.u32()?);
        d.skip(5)?; // blocksize, rdev, blocks, fsid, fileid
        d.skip(2)?; // atime
        let mtime = d.u32()?;
        d.skip(3)?; // mtime usec, ctime
        Ok(Attr {
            ftype,
            mode,
            uid,
            gid,
            size,
            mtime,
        })
    }

    fn v3(d: &mut Dec) -> Result<Attr> {
        let ftype = d.u32()?;
        let mode = d.u32()?;
        d.u32()?; // nlink
        let (uid, gid) = (d.u32()?, d.u32()?);
        let size = d.u64()?;
        d.skip(2 + 2 + 2 + 2)?; // used, rdev, fsid, fileid
        d.skip(2)?; // atime
        let mtime = d.u32()?;
        d.skip(3)?; // mtime nsec, ctime
        Ok(Attr {
            ftype,
            mode,
            uid,
            gid,
            size,
            mtime,
        })
    }

    /// v3 `post_op_attr`: attributes if the server chose to send them.
    fn post_op(d: &mut Dec) -> Result<Option<Attr>> {
        if d.bool()? {
            Ok(Some(Attr::v3(d)?))
        } else {
            Ok(None)
        }
    }

    pub fn is_dir(&self) -> bool {
        self.ftype == 2
    }
}

/// v3 `wcc_data`: attributes before and after; only skipped here.
fn skip_wcc(d: &mut Dec) -> Result<()> {
    if d.bool()? {
        d.skip(6)?;
    }
    Attr::post_op(d)?;
    Ok(())
}

fn status_error(code: u32, what: &str) -> Error {
    let text = |why: &str| format!("{what}: {why}");
    match code {
        1 | 13 => Error::Denied(text(if code == 1 {
            "not owner"
        } else {
            "permission denied (the uid / gid sent may not have access)"
        })),
        2 => Error::NotFound(what.to_string()),
        17 => Error::Protocol(text("already exists")),
        20 => Error::Protocol(text("not a folder")),
        21 => Error::Protocol(text("is a folder")),
        28 => Error::Protocol(text("no space left on the server")),
        30 => Error::Denied(text("the export is read-only")),
        63 => Error::Protocol(text("name too long")),
        66 => Error::Protocol(text("folder is not empty")),
        69 => Error::Protocol(text("over quota")),
        70 => Error::Protocol(text(
            "stale file handle (the server restarted or the file went)",
        )),
        10004 => Error::Unsupported(text("not supported by this server")),
        n => Error::Protocol(text(&format!("NFS error {n}"))),
    }
}

/// What a path in the remote pane is.
enum Node {
    /// A folder made up from export paths, not on the server.
    Virtual(Vec<String>),
    Real(Vec<u8>),
}

/// Where a path falls: inside an export (which one, and the parts below it), or between them.
enum Located {
    Inside(String, Vec<String>),
    Between(Vec<String>),
}

/// A connection to one machine's NFS server and its exports.
pub struct NfsSession {
    nfs: Client,
    mount: Client,
    v3: bool,
    opts: Options,
    /// Export paths as the server sent them, and as shown.
    exports: Vec<(Vec<u8>, String)>,
    /// Mounted exports' root handles, by shown path.
    mounted: HashMap<String, Vec<u8>>,
    /// Handles looked up already, by full shown path.
    handles: HashMap<String, Vec<u8>>,
    chunk: u32,
    closed: bool,
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))
}

/// Connect to `host`'s NFS service through its portmapper and read the export list.
pub fn connect(host: &str, opts: &Options) -> Result<NfsSession> {
    let pmap_addr = resolve(host, opts.portmap_port)?;
    let cred = Cred {
        machine: opts.client_name.clone(),
        uid: opts.uid,
        gid: opts.gid,
        gids: vec![opts.gid],
    };
    let transports: &[bool] = match opts.tcp {
        Some(true) => &[true],
        Some(false) => &[false],
        None => &[false, true],
    };
    let versions: &[u32] = match opts.version {
        Some(3) => &[3],
        Some(_) => &[2],
        None => &[3, 2],
    };
    let mut last = Error::Unsupported("no NFS service found".into());
    for &tcp in transports {
        let mut pmap = match Client::connect(
            pmap_addr,
            tcp,
            false,
            PMAP_PROG,
            2,
            cred.clone(),
            opts.timeout,
        ) {
            Ok(p) => p,
            Err(e) => {
                last = e;
                continue;
            }
        };
        for &v in versions {
            let mount_v = if v == 3 { 3 } else { 1 };
            let ports = rpc::getport(&mut pmap, NFS_PROG, v, tcp)
                .and_then(|n| Ok((n, rpc::getport(&mut pmap, MOUNT_PROG, mount_v, tcp)?)));
            let (nfs_port, mount_port) = match ports {
                Ok((n, m)) if n != 0 && m != 0 => (n, m),
                Ok(_) => continue,
                Err(e) => {
                    last = e;
                    break;
                }
            };
            let mut at = pmap_addr;
            at.set_port(mount_port);
            let mount = Client::connect(
                at,
                tcp,
                opts.reserved_port,
                MOUNT_PROG,
                mount_v,
                cred.clone(),
                opts.timeout,
            )?;
            at.set_port(nfs_port);
            let nfs = mount.sibling(at, NFS_PROG, v)?;
            let mut s = NfsSession {
                nfs,
                mount,
                v3: v == 3,
                opts: opts.clone(),
                exports: Vec::new(),
                mounted: HashMap::new(),
                handles: HashMap::new(),
                // NFSv2 caps transfers at 8 KB; v3 over UDP keeps to it too, for old IP stacks.
                chunk: if v == 3 && tcp { 32 * 1024 } else { 8 * 1024 },
                closed: false,
            };
            s.read_exports()?;
            return Ok(s);
        }
    }
    Err(last)
}

impl NfsSession {
    /// `NFSv3 over UDP`, for the header and the log.
    pub fn describe(&self) -> String {
        format!(
            "NFSv{} over {}{}",
            if self.v3 { 3 } else { 2 },
            if self.nfs.is_tcp() { "TCP" } else { "UDP" },
            if self.nfs.reserved {
                ", reserved port"
            } else {
                ""
            }
        )
    }

    /// The exports as shown, such as `/export/home`.
    pub fn exports(&self) -> Vec<String> {
        self.exports.iter().map(|(_, s)| s.clone()).collect()
    }

    fn read_exports(&mut self) -> Result<()> {
        let r = self.mount.call(5, &[])?;
        let mut d = Dec::new(&r);
        let mut out = Vec::new();
        while d.bool()? {
            let dir = d.opaque()?.to_vec();
            while d.bool()? {
                d.opaque()?; // group names allowed to mount it
            }
            let shown = self.opts.charset.decode(&dir);
            out.push((dir, shown));
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        self.exports = out;
        Ok(())
    }

    fn enc_fh(&self, e: &mut Enc, fh: &[u8]) {
        if self.v3 {
            e.opaque(fh);
        } else {
            e.fixed(fh);
        }
    }

    fn dec_fh(&self, d: &mut Dec) -> Result<Vec<u8>> {
        Ok(if self.v3 {
            d.opaque()?.to_vec()
        } else {
            d.fixed(32)?.to_vec()
        })
    }

    fn name(&self, n: &str) -> Vec<u8> {
        self.opts
            .charset
            .encode(n)
            .unwrap_or_else(|| n.as_bytes().to_vec())
    }

    /// Call NFS and check its status word; the rest of the reply.
    fn call(&mut self, procedure: u32, args: &Enc, what: &str) -> Result<Vec<u8>> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        let r = self.nfs.call(procedure, &args.0)?;
        let status = Dec::new(&r).u32()?;
        if status != 0 {
            if status == 70 {
                self.handles.clear();
            }
            return Err(status_error(status, what));
        }
        Ok(r[4..].to_vec())
    }

    fn mount_export(&mut self, shown: &str) -> Result<Vec<u8>> {
        if let Some(fh) = self.mounted.get(shown) {
            return Ok(fh.clone());
        }
        let raw = self
            .exports
            .iter()
            .find(|(_, s)| s == shown)
            .map(|(r, _)| r.clone())
            .ok_or_else(|| Error::NotFound(shown.to_string()))?;
        let mut e = Enc::new();
        e.opaque(&raw);
        let r = self.mount.call(1, &e.0)?;
        let mut d = Dec::new(&r);
        let status = d.u32()?;
        if status != 0 {
            return Err(match status {
                1 | 13 => Error::Denied(format!(
                    "{shown}: the server will not let this computer mount it (check its exports)"
                )),
                2 => Error::NotFound(shown.to_string()),
                n => status_error(n, shown),
            });
        }
        let fh = self.dec_fh(&mut d)?;
        self.mounted.insert(shown.to_string(), fh.clone());
        Ok(fh)
    }

    fn lookup(&mut self, dir: &[u8], name: &str, what: &str) -> Result<(Vec<u8>, Option<Attr>)> {
        let mut e = Enc::new();
        self.enc_fh(&mut e, dir);
        e.opaque(&self.name(name));
        let r = self.call(if self.v3 { 3 } else { 4 }, &e, what)?;
        let mut d = Dec::new(&r);
        let fh = self.dec_fh(&mut d)?;
        let attr = if self.v3 {
            Attr::post_op(&mut d)?
        } else {
            Some(Attr::v2(&mut d)?)
        };
        Ok((fh, attr))
    }

    fn getattr(&mut self, fh: &[u8], what: &str) -> Result<Attr> {
        let mut e = Enc::new();
        self.enc_fh(&mut e, fh);
        let r = self.call(1, &e, what)?;
        let mut d = Dec::new(&r);
        if self.v3 {
            Attr::v3(&mut d)
        } else {
            Attr::v2(&mut d)
        }
    }

    /// Which export holds `path`, and the path inside it; or the folders the exports make up.
    fn locate(&self, path: &RemotePath) -> Result<Located> {
        let p = path.to_string();
        let best = self
            .exports
            .iter()
            .map(|(_, s)| s)
            .filter(|s| s.as_str() == "/" || p == **s || p.starts_with(&format!("{s}/")))
            .max_by_key(|s| s.len());
        if let Some(export) = best {
            let inside = RemotePath::parse(PathStyle::Unix, &p[export.len().min(p.len())..]);
            return Ok(Located::Inside(export.clone(), inside.parts().to_vec()));
        }
        let prefix = if p == "/" {
            "/".to_string()
        } else {
            format!("{p}/")
        };
        let mut kids: Vec<String> = self
            .exports
            .iter()
            .filter_map(|(_, s)| s.strip_prefix(&prefix))
            .filter_map(|rest| rest.split('/').next().map(str::to_string))
            .filter(|k| !k.is_empty())
            .collect();
        kids.sort();
        kids.dedup();
        if kids.is_empty() {
            return Err(Error::NotFound(format!("{p} (not inside any export)")));
        }
        Ok(Located::Between(kids))
    }

    fn node(&mut self, path: &RemotePath) -> Result<Node> {
        let (export, parts) = match self.locate(path)? {
            Located::Inside(e, parts) => (e, parts),
            Located::Between(kids) => return Ok(Node::Virtual(kids)),
        };
        let key = path.to_string();
        if let Some(fh) = self.handles.get(&key) {
            return Ok(Node::Real(fh.clone()));
        }
        let mut fh = self.mount_export(&export)?;
        let mut so_far = RemotePath::parse(PathStyle::Unix, &export);
        for part in &parts {
            so_far = so_far.join(part);
            let k = so_far.to_string();
            fh = match self.handles.get(&k) {
                Some(h) => h.clone(),
                None => {
                    let (h, _) = self.lookup(&fh, part, &k)?;
                    self.handles.insert(k, h.clone());
                    h
                }
            };
        }
        Ok(Node::Real(fh))
    }

    fn real(&mut self, path: &RemotePath) -> Result<Vec<u8>> {
        match self.node(path)? {
            Node::Real(fh) => Ok(fh),
            Node::Virtual(_) => Err(Error::Unsupported(format!(
                "{path} is not inside an export"
            ))),
        }
    }

    fn entry(&self, raw: &[u8], a: Option<Attr>) -> Entry {
        let a = a.unwrap_or_default();
        let mut e = Entry::new(
            self.opts.charset.decode(raw),
            match a.ftype {
                1 => EntryKind::File,
                2 => EntryKind::Folder,
                5 => EntryKind::Link,
                0 => EntryKind::File,
                _ => EntryKind::Other,
            },
        );
        e.raw_name = raw.to_vec();
        if a.ftype != 0 {
            e.size = Some(a.size);
            e.mode = Some(a.mode & 0o7777);
            e.owner = Some(a.uid.to_string());
            e.group = Some(a.gid.to_string());
            e.modified = Some(Timestamp {
                unix: i64::from(a.mtime),
                precision: TimePrecision::Exact,
            });
        }
        e
    }

    /// Every name in a folder with its attributes.
    fn readdir(&mut self, fh: &[u8], what: &str) -> Result<Vec<Entry>> {
        if self.v3 {
            match self.readdirplus(fh, what) {
                Err(Error::Unsupported(_)) => {}
                r => return r,
            }
        }
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut cookie = 0u64;
        let mut verf = [0u8; 8];
        loop {
            let mut e = Enc::new();
            self.enc_fh(&mut e, fh);
            if self.v3 {
                e.u64(cookie).fixed(&verf).u32(self.chunk);
            } else {
                e.fixed(&(cookie as u32).to_be_bytes()).u32(self.chunk);
            }
            let r = self.call(16, &e, what)?;
            let mut d = Dec::new(&r);
            if self.v3 {
                Attr::post_op(&mut d)?;
                verf.copy_from_slice(d.fixed(8)?);
            }
            let mut any = false;
            while d.bool()? {
                any = true;
                if self.v3 {
                    d.u64()?;
                } else {
                    d.u32()?;
                }
                names.push(d.opaque()?.to_vec());
                cookie = if self.v3 {
                    d.u64()?
                } else {
                    u64::from(u32::from_be_bytes(d.fixed(4)?.try_into().unwrap()))
                };
            }
            if d.bool()? || !any {
                break;
            }
        }
        let mut out = Vec::new();
        for n in names.into_iter().filter(|n| n != b"." && n != b"..") {
            let shown = self.opts.charset.decode(&n);
            let attr = self
                .lookup(fh, &shown, &format!("{what}/{shown}"))
                .ok()
                .and_then(|(_, a)| a);
            out.push(self.entry(&n, attr));
        }
        Ok(out)
    }

    fn readdirplus(&mut self, fh: &[u8], what: &str) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut cookie = 0u64;
        let mut verf = [0u8; 8];
        loop {
            let mut e = Enc::new();
            self.enc_fh(&mut e, fh);
            e.u64(cookie)
                .fixed(&verf)
                .u32(self.chunk)
                .u32(self.chunk * 4);
            let r = self.call(17, &e, what)?;
            let mut d = Dec::new(&r);
            Attr::post_op(&mut d)?;
            verf.copy_from_slice(d.fixed(8)?);
            let mut any = false;
            while d.bool()? {
                any = true;
                d.u64()?; // fileid
                let name = d.opaque()?.to_vec();
                cookie = d.u64()?;
                let attr = Attr::post_op(&mut d)?;
                if d.bool()? {
                    d.opaque()?; // handle; looked up again when the folder is entered
                }
                if name != b"." && name != b".." {
                    out.push(self.entry(&name, attr));
                }
            }
            if d.bool()? || !any {
                break;
            }
        }
        Ok(out)
    }

    fn parent_and_name(&mut self, path: &RemotePath) -> Result<(Vec<u8>, String)> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::Unsupported("the root cannot be changed".into()))?;
        Ok((self.real(&parent)?, path.name().to_string()))
    }

    fn dirop(&self, e: &mut Enc, dir: &[u8], name: &str) {
        self.enc_fh(e, dir);
        e.opaque(&self.name(name));
    }

    /// v2 `sattr` / v3 `sattr3` that sets only the mode (and optionally size 0).
    fn sattr(&self, e: &mut Enc, mode: u32, truncate: bool) {
        if self.v3 {
            e.bool(true).u32(mode);
            e.bool(false).bool(false); // uid, gid
            e.bool(truncate);
            if truncate {
                e.u64(0);
            }
            e.u32(1).u32(1); // atime and mtime: the server's clock
        } else {
            let none = u32::MAX;
            e.u32(mode).u32(none).u32(none);
            e.u32(if truncate { 0 } else { none });
            e.u32(none).u32(none).u32(none).u32(none);
        }
    }

    fn create(&mut self, path: &RemotePath, exclusive: bool) -> Result<Vec<u8>> {
        let what = path.to_string();
        let (dir, name) = self.parent_and_name(path)?;
        if !exclusive {
            // An existing file is cut to nothing and reused, as `>` does.
            if let Ok((fh, _)) = self.lookup(&dir, &name, &what) {
                let mut e = Enc::new();
                self.enc_fh(&mut e, &fh);
                self.sattr(&mut e, 0o644, true);
                if self.v3 {
                    e.bool(false); // no guard
                }
                self.call(2, &e, &what)?;
                return Ok(fh);
            }
        }
        let mut e = Enc::new();
        self.dirop(&mut e, &dir, &name);
        if self.v3 {
            e.u32(if exclusive { 1 } else { 0 });
        }
        self.sattr(&mut e, 0o644, false);
        let r = self.call(if self.v3 { 8 } else { 9 }, &e, &what)?;
        let mut d = Dec::new(&r);
        let fh = if self.v3 {
            if d.bool()? {
                self.dec_fh(&mut d)?
            } else {
                self.lookup(&dir, &name, &what)?.0
            }
        } else {
            self.dec_fh(&mut d)?
        };
        self.handles.insert(what, fh.clone());
        Ok(fh)
    }

    fn read_at(&mut self, fh: &[u8], at: u64, count: u32) -> Result<(Vec<u8>, bool)> {
        let mut e = Enc::new();
        self.enc_fh(&mut e, fh);
        if self.v3 {
            e.u64(at).u32(count);
        } else {
            e.u32(at as u32).u32(count).u32(count);
        }
        let r = self.call(6, &e, "read")?;
        let mut d = Dec::new(&r);
        if self.v3 {
            Attr::post_op(&mut d)?;
            d.u32()?;
            let eof = d.bool()?;
            Ok((d.opaque()?.to_vec(), eof))
        } else {
            let a = Attr::v2(&mut d)?;
            let data = d.opaque()?.to_vec();
            let eof = data.is_empty() || at + data.len() as u64 >= a.size;
            Ok((data, eof))
        }
    }

    fn write_at(&mut self, fh: &[u8], at: u64, data: &[u8]) -> Result<usize> {
        let mut e = Enc::new();
        self.enc_fh(&mut e, fh);
        if self.v3 {
            // FILE_SYNC: on disk before the reply, so nothing needs a COMMIT afterwards.
            e.u64(at).u32(data.len() as u32).u32(2).opaque(data);
        } else {
            if at > u64::from(u32::MAX) {
                return Err(Error::Unsupported("NFSv2 files end at 4 GB".into()));
            }
            e.u32(0).u32(at as u32).u32(0).opaque(data);
        }
        let r = self.call(if self.v3 { 7 } else { 8 }, &e, "write")?;
        if self.v3 {
            let mut d = Dec::new(&r);
            skip_wcc(&mut d)?;
            return Ok(d.u32()? as usize);
        }
        Ok(data.len())
    }
}

struct NfsReader<'a> {
    s: &'a mut NfsSession,
    fh: Vec<u8>,
    at: u64,
    eof: bool,
}

impl Read for NfsReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.eof || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u32).min(self.s.chunk);
        let (data, eof) = self
            .s
            .read_at(&self.fh, self.at, want)
            .map_err(std::io::Error::other)?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        self.at += n as u64;
        self.eof = eof || n == 0;
        Ok(n)
    }
}

struct NfsWriter<'a> {
    s: &'a mut NfsSession,
    fh: Vec<u8>,
    at: u64,
}

impl Write for NfsWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.s.chunk as usize);
        let done = self
            .s
            .write_at(&self.fh, self.at, &buf[..n])
            .map_err(std::io::Error::other)?;
        self.at += done as u64;
        Ok(done)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Connection for NfsSession {
    fn state(&self) -> ConnState {
        if self.closed {
            ConnState::Disconnected
        } else {
            ConnState::Connected
        }
    }

    /// `UMNT` every export mounted here, so the server's mount list stays truthful.
    fn disconnect(&mut self, _timeout: Duration) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let mut first = None;
        let mounted: Vec<String> = self.mounted.keys().cloned().collect();
        for shown in mounted {
            let raw = self
                .exports
                .iter()
                .find(|(_, s)| *s == shown)
                .map(|(r, _)| r.clone())
                .unwrap_or_else(|| shown.as_bytes().to_vec());
            let mut e = Enc::new();
            e.opaque(&raw);
            if let Err(err) = self.mount.call(3, &e.0) {
                first.get_or_insert(err);
            }
        }
        self.closed = true;
        self.mounted.clear();
        self.handles.clear();
        first.map_or(Ok(()), Err)
    }
}

impl FileSession for NfsSession {
    fn caps(&self) -> FileCaps {
        FileCaps {
            random_read: true,
            random_write: true,
            resume_upload: true,
            rename: true,
            delete: true,
            mkdir: true,
            set_times: false,
            set_mode: false,
        }
    }

    fn home(&mut self) -> Result<RemotePath> {
        if self.exports.is_empty() {
            return Err(Error::NotFound(
                "the server exports nothing to this computer".into(),
            ));
        }
        Ok(match self.exports.as_slice() {
            [(_, only)] => RemotePath::parse(PathStyle::Unix, only),
            _ => RemotePath::root(PathStyle::Unix),
        })
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        match self.node(dir)? {
            Node::Virtual(kids) => Ok(kids
                .into_iter()
                .map(|k| Entry::new(k, EntryKind::Folder))
                .collect()),
            Node::Real(fh) => self.readdir(&fh, &dir.to_string()),
        }
    }

    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        match self.node(path)? {
            Node::Virtual(_) => Ok(Entry::new(path.name(), EntryKind::Folder)),
            Node::Real(fh) => {
                let a = self.getattr(&fh, &path.to_string())?;
                let raw = self.name(path.name());
                Ok(self.entry(&raw, Some(a)))
            }
        }
    }

    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        let fh = self.real(path)?;
        Ok(Box::new(NfsReader {
            s: self,
            fh,
            at,
            eof: false,
        }))
    }

    fn open_write(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>> {
        let (fh, at) = match mode {
            WriteMode::Create => (self.create(path, true)?, 0),
            WriteMode::Overwrite => (self.create(path, false)?, 0),
            WriteMode::Resume(n) => (self.real(path)?, n),
        };
        Ok(Box::new(NfsWriter { s: self, fh, at }))
    }

    fn mkdir(&mut self, path: &RemotePath) -> Result<()> {
        let what = path.to_string();
        let (dir, name) = self.parent_and_name(path)?;
        let mut e = Enc::new();
        self.dirop(&mut e, &dir, &name);
        self.sattr(&mut e, 0o755, false);
        self.call(if self.v3 { 9 } else { 14 }, &e, &what)?;
        Ok(())
    }

    fn remove(&mut self, path: &RemotePath) -> Result<()> {
        let what = path.to_string();
        let fh = self.real(path)?;
        let is_dir = self.getattr(&fh, &what)?.is_dir();
        let (dir, name) = self.parent_and_name(path)?;
        let mut e = Enc::new();
        self.dirop(&mut e, &dir, &name);
        let procedure = match (self.v3, is_dir) {
            (true, true) => 13,
            (true, false) => 12,
            (false, true) => 15,
            (false, false) => 10,
        };
        self.call(procedure, &e, &what)?;
        self.handles
            .retain(|k, _| k != &what && !k.starts_with(&format!("{what}/")));
        Ok(())
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        let what = from.to_string();
        let (fdir, fname) = self.parent_and_name(from)?;
        let (tdir, tname) = self.parent_and_name(to)?;
        let mut e = Enc::new();
        self.dirop(&mut e, &fdir, &fname);
        self.dirop(&mut e, &tdir, &tname);
        self.call(if self.v3 { 14 } else { 11 }, &e, &what)?;
        self.handles
            .retain(|k, _| k != &what && !k.starts_with(&format!("{what}/")));
        Ok(())
    }
}
