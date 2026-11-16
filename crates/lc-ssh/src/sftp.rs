//! SFTP version 3 (draft-ietf-secsh-filexfer-02), the version every pre-2005 server speaks.
//!
//! Written here rather than using ssh2's SFTP so names stay bytes: ssh2 turns them into `Path`s,
//! which on Windows must be UTF-8, and 1990s machines name files in Latin-1 or MacRoman. The
//! client works over any `Read + Write`, so tests drive it against a pretend server in memory.

use std::collections::HashMap;
use std::io::{Read, Write};

use lc_core::{Error, Result};

pub const VERSION: u32 = 3;

pub mod op {
    pub const INIT: u8 = 1;
    pub const VERSION: u8 = 2;
    pub const OPEN: u8 = 3;
    pub const CLOSE: u8 = 4;
    pub const READ: u8 = 5;
    pub const WRITE: u8 = 6;
    pub const LSTAT: u8 = 7;
    pub const OPENDIR: u8 = 11;
    pub const READDIR: u8 = 12;
    pub const REMOVE: u8 = 13;
    pub const MKDIR: u8 = 14;
    pub const RMDIR: u8 = 15;
    pub const REALPATH: u8 = 16;
    pub const STAT: u8 = 17;
    pub const RENAME: u8 = 18;
    pub const STATUS: u8 = 101;
    pub const HANDLE: u8 = 102;
    pub const DATA: u8 = 103;
    pub const NAME: u8 = 104;
    pub const ATTRS: u8 = 105;
}

pub mod status {
    pub const OK: u32 = 0;
    pub const EOF: u32 = 1;
    pub const NO_SUCH_FILE: u32 = 2;
    pub const PERMISSION_DENIED: u32 = 3;
    pub const OP_UNSUPPORTED: u32 = 8;
}

/// `open` flags.
pub mod flag {
    pub const READ: u32 = 0x01;
    pub const WRITE: u32 = 0x02;
    pub const APPEND: u32 = 0x04;
    pub const CREAT: u32 = 0x08;
    pub const TRUNC: u32 = 0x10;
    pub const EXCL: u32 = 0x20;
}

const A_SIZE: u32 = 0x1;
const A_UIDGID: u32 = 0x2;
const A_PERMISSIONS: u32 = 0x4;
const A_ACMODTIME: u32 = 0x8;
const A_EXTENDED: u32 = 0x8000_0000;

/// The largest read asked for; OpenSSH allows more, but SSH.com 2.x and others cap near 32 KB.
pub const CHUNK: u32 = 32 * 1024;

/// File attributes; absent fields were not sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub uid_gid: Option<(u32, u32)>,
    pub permissions: Option<u32>,
    pub atime_mtime: Option<(u32, u32)>,
}

impl Attrs {
    pub fn is_dir(&self) -> bool {
        self.permissions.is_some_and(|p| p & 0o170000 == 0o040000)
    }

    pub fn is_link(&self) -> bool {
        self.permissions.is_some_and(|p| p & 0o170000 == 0o120000)
    }

    pub fn is_file(&self) -> bool {
        self.permissions.is_none_or(|p| p & 0o170000 == 0o100000)
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut flags = 0;
        if self.size.is_some() {
            flags |= A_SIZE;
        }
        if self.uid_gid.is_some() {
            flags |= A_UIDGID;
        }
        if self.permissions.is_some() {
            flags |= A_PERMISSIONS;
        }
        if self.atime_mtime.is_some() {
            flags |= A_ACMODTIME;
        }
        put_u32(out, flags);
        if let Some(s) = self.size {
            out.extend_from_slice(&s.to_be_bytes());
        }
        if let Some((u, g)) = self.uid_gid {
            put_u32(out, u);
            put_u32(out, g);
        }
        if let Some(p) = self.permissions {
            put_u32(out, p);
        }
        if let Some((a, m)) = self.atime_mtime {
            put_u32(out, a);
            put_u32(out, m);
        }
    }
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s);
}

/// Reads fields off a packet body.
pub struct Cursor<'a> {
    b: &'a [u8],
}

impl<'a> Cursor<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Cursor { b }
    }

    fn short() -> Error {
        Error::Protocol("SFTP packet ends early".into())
    }

    pub fn is_empty(&self) -> bool {
        self.b.is_empty()
    }

    pub fn u8(&mut self) -> Result<u8> {
        let (&v, rest) = self.b.split_first().ok_or_else(Self::short)?;
        self.b = rest;
        Ok(v)
    }

    pub fn u32(&mut self) -> Result<u32> {
        let v = self.b.get(..4).ok_or_else(Self::short)?;
        let v = u32::from_be_bytes(v.try_into().unwrap());
        self.b = &self.b[4..];
        Ok(v)
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok((u64::from(self.u32()?) << 32) | u64::from(self.u32()?))
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        let v = self.b.get(..n).ok_or_else(Self::short)?;
        self.b = &self.b[n..];
        Ok(v)
    }

    pub fn attrs(&mut self) -> Result<Attrs> {
        let flags = self.u32()?;
        let mut a = Attrs::default();
        if flags & A_SIZE != 0 {
            a.size = Some(self.u64()?);
        }
        if flags & A_UIDGID != 0 {
            a.uid_gid = Some((self.u32()?, self.u32()?));
        }
        if flags & A_PERMISSIONS != 0 {
            a.permissions = Some(self.u32()?);
        }
        if flags & A_ACMODTIME != 0 {
            a.atime_mtime = Some((self.u32()?, self.u32()?));
        }
        if flags & A_EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(a)
    }
}

/// One packet off the wire: its type and body.
pub fn read_packet(r: &mut impl Read) -> Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > 1 << 20 {
        return Err(Error::Protocol(format!("SFTP packet of {len} bytes")));
    }
    let mut kind = [0u8];
    r.read_exact(&mut kind)?;
    let mut body = vec![0u8; len - 1];
    r.read_exact(&mut body)?;
    Ok((kind[0], body))
}

pub fn write_packet(w: &mut impl Write, kind: u8, body: &[u8]) -> Result<()> {
    let mut p = Vec::with_capacity(body.len() + 5);
    put_u32(&mut p, body.len() as u32 + 1);
    p.push(kind);
    p.extend_from_slice(body);
    w.write_all(&p)?;
    w.flush()?;
    Ok(())
}

/// One name from a listing: the raw name, `ls -l` style long name, and attributes.
#[derive(Debug, Clone)]
pub struct Name {
    pub name: Vec<u8>,
    pub long: Vec<u8>,
    pub attrs: Attrs,
}

/// An SFTP client over a channel.
pub struct Sftp<S> {
    io: S,
    next: u32,
    /// Replies that came ahead of the one being waited for, by request id.
    early: HashMap<u32, (u8, Vec<u8>)>,
    pub version: u32,
}

/// Turn a STATUS that is not OK into the error the user sees, naming `what`.
fn status_error(code: u32, msg: &[u8], what: &[u8]) -> Error {
    let what = String::from_utf8_lossy(what).into_owned();
    let msg = String::from_utf8_lossy(msg).into_owned();
    let text = if msg.is_empty() {
        what
    } else {
        format!("{what}: {msg}")
    };
    match code {
        status::NO_SUCH_FILE => Error::NotFound(text),
        status::PERMISSION_DENIED => Error::Denied(text),
        status::OP_UNSUPPORTED => Error::Unsupported(text),
        _ => Error::Protocol(text),
    }
}

enum Reply {
    Status(u32, Vec<u8>),
    Handle(Vec<u8>),
    Data(Vec<u8>),
    Names(Vec<Name>),
    Attrs(Attrs),
}

impl<S: Read + Write> Sftp<S> {
    /// Say hello; the server answers with the version it speaks.
    pub fn start(mut io: S) -> Result<Self> {
        let mut b = Vec::new();
        put_u32(&mut b, VERSION);
        write_packet(&mut io, op::INIT, &b)?;
        let (kind, body) = read_packet(&mut io)?;
        if kind != op::VERSION {
            return Err(Error::Protocol(format!(
                "SFTP server answered INIT with {kind}"
            )));
        }
        let version = Cursor::new(&body).u32()?;
        Ok(Sftp {
            io,
            next: 1,
            early: HashMap::new(),
            version,
        })
    }

    pub fn into_inner(self) -> S {
        self.io
    }

    fn call(&mut self, kind: u8, args: &[u8]) -> Result<Reply> {
        let id = self.request(kind, args)?;
        self.reply(id)
    }

    /// Send a request without waiting; its id, for `reply`.
    fn request(&mut self, kind: u8, args: &[u8]) -> Result<u32> {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        let mut b = Vec::with_capacity(args.len() + 4);
        put_u32(&mut b, id);
        b.extend_from_slice(args);
        write_packet(&mut self.io, kind, &b)?;
        Ok(id)
    }

    /// The reply to request `id`; others that arrive first are kept for their turn.
    fn reply(&mut self, id: u32) -> Result<Reply> {
        let (kind, body) = loop {
            if let Some(r) = self.early.remove(&id) {
                break r;
            }
            let (kind, body) = read_packet(&mut self.io)?;
            let got = Cursor::new(&body).u32()?;
            if got == id {
                break (kind, body);
            }
            if self.early.len() >= 1024 {
                return Err(Error::Protocol(format!("SFTP reply {got} to request {id}")));
            }
            self.early.insert(got, (kind, body));
        };
        let mut c = Cursor::new(&body);
        c.u32()?;
        Ok(match kind {
            op::STATUS => {
                let code = c.u32()?;
                // Some early servers send the code alone.
                let msg = if c.is_empty() { &[][..] } else { c.bytes()? };
                Reply::Status(code, msg.to_vec())
            }
            op::HANDLE => Reply::Handle(c.bytes()?.to_vec()),
            op::DATA => Reply::Data(c.bytes()?.to_vec()),
            op::ATTRS => Reply::Attrs(c.attrs()?),
            op::NAME => {
                let n = c.u32()?;
                let mut v = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    v.push(Name {
                        name: c.bytes()?.to_vec(),
                        long: c.bytes()?.to_vec(),
                        attrs: c.attrs()?,
                    });
                }
                Reply::Names(v)
            }
            other => return Err(Error::Protocol(format!("unknown SFTP reply type {other}"))),
        })
    }

    fn expect_ok(&mut self, kind: u8, args: &[u8], what: &[u8]) -> Result<()> {
        match self.call(kind, args)? {
            Reply::Status(status::OK, _) => Ok(()),
            Reply::Status(code, msg) => Err(status_error(code, &msg, what)),
            _ => Err(Error::Protocol("SFTP: expected a status".into())),
        }
    }

    fn handle(&mut self, kind: u8, args: &[u8], what: &[u8]) -> Result<Vec<u8>> {
        match self.call(kind, args)? {
            Reply::Handle(h) => Ok(h),
            Reply::Status(code, msg) => Err(status_error(code, &msg, what)),
            _ => Err(Error::Protocol("SFTP: expected a handle".into())),
        }
    }

    fn path_arg(path: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        put_str(&mut b, path);
        b
    }

    /// The absolute form of `path` (`.` is the login folder).
    pub fn realpath(&mut self, path: &[u8]) -> Result<Vec<u8>> {
        match self.call(op::REALPATH, &Self::path_arg(path))? {
            Reply::Names(mut v) if !v.is_empty() => Ok(v.swap_remove(0).name),
            Reply::Status(code, msg) => Err(status_error(code, &msg, path)),
            _ => Err(Error::Protocol("SFTP: REALPATH gave no name".into())),
        }
    }

    pub fn stat(&mut self, path: &[u8]) -> Result<Attrs> {
        match self.call(op::STAT, &Self::path_arg(path))? {
            Reply::Attrs(a) => Ok(a),
            Reply::Status(code, msg) => Err(status_error(code, &msg, path)),
            _ => Err(Error::Protocol("SFTP: expected attributes".into())),
        }
    }

    pub fn lstat(&mut self, path: &[u8]) -> Result<Attrs> {
        match self.call(op::LSTAT, &Self::path_arg(path))? {
            Reply::Attrs(a) => Ok(a),
            Reply::Status(code, msg) => Err(status_error(code, &msg, path)),
            _ => Err(Error::Protocol("SFTP: expected attributes".into())),
        }
    }

    /// Every name in a folder, `.` and `..` left out.
    pub fn list(&mut self, dir: &[u8]) -> Result<Vec<Name>> {
        let h = self.handle(op::OPENDIR, &Self::path_arg(dir), dir)?;
        let mut out = Vec::new();
        let result = loop {
            match self.call(op::READDIR, &Self::path_arg(&h)) {
                Ok(Reply::Names(v)) => out.extend(v),
                Ok(Reply::Status(status::EOF, _)) => break Ok(()),
                Ok(Reply::Status(code, msg)) => break Err(status_error(code, &msg, dir)),
                Ok(_) => break Err(Error::Protocol("SFTP: READDIR gave no names".into())),
                Err(e) => break Err(e),
            }
        };
        let closed = self.close(&h);
        result?;
        closed?;
        out.retain(|n| n.name != b"." && n.name != b"..");
        Ok(out)
    }

    pub fn open(&mut self, path: &[u8], flags: u32) -> Result<Vec<u8>> {
        let mut b = Self::path_arg(path);
        put_u32(&mut b, flags);
        Attrs::default().encode(&mut b);
        self.handle(op::OPEN, &b, path)
    }

    pub fn close(&mut self, handle: &[u8]) -> Result<()> {
        self.expect_ok(op::CLOSE, &Self::path_arg(handle), b"close")
    }

    /// Up to `len` bytes at `offset`; empty at the end of the file.
    pub fn read(&mut self, handle: &[u8], offset: u64, len: u32) -> Result<Vec<u8>> {
        let id = self.send_read(handle, offset, len)?;
        self.read_data(id)
    }

    pub fn write(&mut self, handle: &[u8], offset: u64, data: &[u8]) -> Result<()> {
        let id = self.send_write(handle, offset, data)?;
        self.write_done(id)
    }

    /// Ask for a read without waiting; `read_data` collects it.
    pub fn send_read(&mut self, handle: &[u8], offset: u64, len: u32) -> Result<u32> {
        let mut b = Self::path_arg(handle);
        b.extend_from_slice(&offset.to_be_bytes());
        put_u32(&mut b, len);
        self.request(op::READ, &b)
    }

    /// The data of read `id`; empty at the end of the file.
    pub fn read_data(&mut self, id: u32) -> Result<Vec<u8>> {
        match self.reply(id)? {
            Reply::Data(d) => Ok(d),
            Reply::Status(status::EOF, _) => Ok(Vec::new()),
            Reply::Status(code, msg) => Err(status_error(code, &msg, b"read")),
            _ => Err(Error::Protocol("SFTP: expected data".into())),
        }
    }

    /// Send a write without waiting; `write_done` collects its status.
    pub fn send_write(&mut self, handle: &[u8], offset: u64, data: &[u8]) -> Result<u32> {
        let mut b = Self::path_arg(handle);
        b.extend_from_slice(&offset.to_be_bytes());
        put_str(&mut b, data);
        self.request(op::WRITE, &b)
    }

    pub fn write_done(&mut self, id: u32) -> Result<()> {
        match self.reply(id)? {
            Reply::Status(status::OK, _) => Ok(()),
            Reply::Status(code, msg) => Err(status_error(code, &msg, b"write")),
            _ => Err(Error::Protocol("SFTP: expected a status".into())),
        }
    }

    pub fn remove(&mut self, path: &[u8]) -> Result<()> {
        self.expect_ok(op::REMOVE, &Self::path_arg(path), path)
    }

    pub fn mkdir(&mut self, path: &[u8]) -> Result<()> {
        let mut b = Self::path_arg(path);
        Attrs::default().encode(&mut b);
        self.expect_ok(op::MKDIR, &b, path)
    }

    pub fn rmdir(&mut self, path: &[u8]) -> Result<()> {
        self.expect_ok(op::RMDIR, &Self::path_arg(path), path)
    }

    pub fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<()> {
        let mut b = Self::path_arg(from);
        put_str(&mut b, to);
        self.expect_ok(op::RENAME, &b, from)
    }
}
