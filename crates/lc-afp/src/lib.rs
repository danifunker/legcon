//! AFP 2.x over TCP (DSI) for legcon (docs/PLAN.md §4.3, D33).
//!
//! Servers: AppleShare IP 5 and later, Mac OS 9 File Sharing, netatalk; all need Open Transport
//! on the Mac (MacTCP-only Macs have no AFP over TCP). Login tries Randnum Exchange (the password
//! never crosses the wire), then Cleartext, then guest. Paths are Mac style
//! (`Volume:Folder:File`): the root lists the volumes, and names inside are long names in the
//! machine's character set (MacRoman), sent as a path from the volume's root folder (DirID 2).
//! Resource forks and Finder info are first-class: [`FileSession::mac_extras`] reads them.

pub mod dsi;

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use lc_core::{
    Charset, ConnState, Connection, Entry, EntryKind, Error, FileCaps, FileSession, MacExtras,
    PathStyle, RemotePath, Result, TimePrecision, Timestamp, WriteMode,
};

use dsi::Dsi;

pub const PORT: u16 = 548;

/// AFP commands used here.
pub mod cmd {
    pub const CLOSE_VOL: u8 = 2;
    pub const CLOSE_FORK: u8 = 4;
    pub const CREATE_DIR: u8 = 6;
    pub const CREATE_FILE: u8 = 7;
    pub const DELETE: u8 = 8;
    pub const ENUMERATE: u8 = 9;
    pub const GET_SRVR_PARMS: u8 = 16;
    pub const LOGIN: u8 = 18;
    pub const LOGIN_CONT: u8 = 19;
    pub const LOGOUT: u8 = 20;
    pub const MOVE_AND_RENAME: u8 = 23;
    pub const OPEN_VOL: u8 = 24;
    pub const OPEN_FORK: u8 = 26;
    pub const READ: u8 = 27;
    pub const RENAME: u8 = 28;
    pub const SET_FORK_PARMS: u8 = 31;
    pub const WRITE: u8 = 33;
    pub const GET_FILE_DIR_PARMS: u8 = 34;
    pub const SET_FILE_DIR_PARMS: u8 = 35;
}

/// AFP result codes.
pub mod err {
    pub const ACCESS_DENIED: i32 = -5000;
    pub const AUTH_CONTINUE: i32 = -5001;
    pub const BAD_UAM: i32 = -5002;
    pub const BAD_VERS: i32 = -5003;
    pub const DIR_NOT_EMPTY: i32 = -5007;
    pub const DISK_FULL: i32 = -5008;
    pub const EOF: i32 = -5009;
    pub const FILE_BUSY: i32 = -5010;
    pub const OBJECT_EXISTS: i32 = -5017;
    pub const OBJECT_NOT_FOUND: i32 = -5018;
    pub const PARAM: i32 = -5019;
    pub const USER_NOT_AUTH: i32 = -5023;
    pub const CALL_NOT_SUPPORTED: i32 = -5024;
    pub const CANT_RENAME: i32 = -5028;
    pub const DIR_NOT_FOUND: i32 = -5029;
    pub const VOL_LOCKED: i32 = -5031;
    pub const OBJECT_LOCKED: i32 = -5032;
}

/// File and folder parameter bits (AFP 2.x).
mod bit {
    pub const MOD_DATE: u16 = 1 << 3;
    pub const FINDER_INFO: u16 = 1 << 5;
    pub const LONG_NAME: u16 = 1 << 6;
    pub const NODE_ID: u16 = 1 << 8;
    pub const DATA_LEN: u16 = 1 << 9;
    pub const RSRC_LEN: u16 = 1 << 10;
    pub const OFFSPRING: u16 = 1 << 9;
}

const FILE_BITMAP: u16 = bit::MOD_DATE
    | bit::FINDER_INFO
    | bit::LONG_NAME
    | bit::NODE_ID
    | bit::DATA_LEN
    | bit::RSRC_LEN;
const DIR_BITMAP: u16 =
    bit::MOD_DATE | bit::FINDER_INFO | bit::LONG_NAME | bit::NODE_ID | bit::OFFSPRING;

/// AFP dates count seconds from 2000-01-01 00:00 GMT.
pub const AFP_EPOCH_UNIX: i64 = 946_684_800;

const VERSIONS: [&str; 3] = ["AFP2.2", "AFPVersion 2.1", "AFPVersion 2.0"];

pub fn afp_error(code: i32, what: &str) -> Error {
    let t = |why: &str| format!("{what}: {why}");
    match code {
        err::ACCESS_DENIED => Error::Denied(t("access denied")),
        err::OBJECT_NOT_FOUND | err::DIR_NOT_FOUND => Error::NotFound(what.to_string()),
        err::CALL_NOT_SUPPORTED => Error::Unsupported(t("the server does not support that")),
        err::CANT_RENAME => Error::Protocol(t("cannot be renamed")),
        err::OBJECT_EXISTS => Error::Protocol(t("already exists")),
        err::DIR_NOT_EMPTY => Error::Protocol(t("folder is not empty")),
        err::DISK_FULL => Error::Protocol(t("the disk is full")),
        err::FILE_BUSY => Error::Protocol(t("the file is open on the server")),
        err::VOL_LOCKED | err::OBJECT_LOCKED => Error::Denied(t("locked")),
        err::USER_NOT_AUTH => Error::Auth(t("user name or password not accepted")),
        err::BAD_UAM => Error::Auth(t("the server does not offer that login method")),
        err::BAD_VERS => Error::Unsupported(t("the server speaks no AFP version legcon knows")),
        err::PARAM => Error::Protocol(t("the server did not accept the request (name too long?)")),
        n => Error::Protocol(t(&format!("AFP error {n}"))),
    }
}

/// What `FPGetSrvrInfo` (DSIGetStatus) says about the server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    /// `Macintosh`, `Netatalk`, `AppleShareIP` ...
    pub machine: String,
    pub versions: Vec<String>,
    pub uams: Vec<String>,
}

fn pstr(b: &[u8], at: usize) -> Option<&[u8]> {
    let n = *b.get(at)? as usize;
    b.get(at + 1..at + 1 + n)
}

fn pstrs(b: &[u8], at: usize) -> Vec<String> {
    let Some(&n) = b.get(at) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut p = at + 1;
    for _ in 0..n {
        let Some(s) = pstr(b, p) else {
            break;
        };
        out.push(String::from_utf8_lossy(s).into_owned());
        p += 1 + s.len();
    }
    out
}

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2)
        .map(|v| u16::from_be_bytes([v[0], v[1]]))
        .ok_or_else(|| Error::Protocol("AFP reply ends early".into()))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
        .ok_or_else(|| Error::Protocol("AFP reply ends early".into()))
}

pub fn parse_server_info(b: &[u8]) -> Result<ServerInfo> {
    let machine_off = u16_at(b, 0)? as usize;
    let vers_off = u16_at(b, 2)? as usize;
    let uam_off = u16_at(b, 4)? as usize;
    Ok(ServerInfo {
        name: pstr(b, 10)
            .map(|s| Charset::MacRoman.decode(s))
            .unwrap_or_default(),
        machine: pstr(b, machine_off)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .unwrap_or_default(),
        versions: pstrs(b, vers_off),
        uams: pstrs(b, uam_off),
    })
}

/// One file or folder's parameters, as `FPEnumerate` and `FPGetFileDirParms` give them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    pub is_dir: bool,
    pub name: Vec<u8>,
    pub mod_date: i32,
    pub finder_info: [u8; 32],
    pub id: u32,
    pub data_len: u32,
    pub rsrc_len: u32,
    pub offspring: u16,
}

impl Node {
    /// The Finder's invisible flag, in the Finder info's flags word (files and folders alike).
    pub fn invisible(&self) -> bool {
        u16::from_be_bytes([self.finder_info[8], self.finder_info[9]]) & 0x4000 != 0
    }
}

/// Parse a parameter block for the given bitmap; long names sit at an offset from its start.
pub fn parse_params(p: &[u8], is_dir: bool, bitmap: u16) -> Result<Node> {
    let mut n = Node {
        is_dir,
        ..Node::default()
    };
    let mut at = 0;
    for b in 0..16u16 {
        if bitmap & (1 << b) == 0 {
            continue;
        }
        match (b, is_dir) {
            (0, _) => at += 2,          // attributes
            (1, _) => at += 4,          // parent DirID
            (2, _) | (4, _) => at += 4, // create / backup date
            (3, _) => {
                n.mod_date = u32_at(p, at)? as i32;
                at += 4;
            }
            (5, _) => {
                let fi = p
                    .get(at..at + 32)
                    .ok_or_else(|| Error::Protocol("AFP reply ends early".into()))?;
                n.finder_info.copy_from_slice(fi);
                at += 32;
            }
            (6, _) => {
                let off = u16_at(p, at)? as usize;
                n.name = pstr(p, off).unwrap_or_default().to_vec();
                at += 2;
            }
            (7, _) => at += 2, // short name offset
            (8, _) => {
                n.id = u32_at(p, at)?;
                at += 4;
            }
            (9, false) => {
                n.data_len = u32_at(p, at)?;
                at += 4;
            }
            (9, true) => {
                n.offspring = u16_at(p, at)?;
                at += 2;
            }
            (10, false) => {
                n.rsrc_len = u32_at(p, at)?;
                at += 4;
            }
            (10, true) | (11, true) | (12, true) => at += 4, // owner, group, access rights
            _ => return Err(Error::Unsupported(format!("AFP parameter bit {b}"))),
        }
    }
    Ok(n)
}

/// Parse an `FPEnumerate` reply: bitmaps, count, then length-prefixed records.
pub fn parse_enumerate(b: &[u8]) -> Result<Vec<Node>> {
    let fbm = u16_at(b, 0)?;
    let dbm = u16_at(b, 2)?;
    let count = u16_at(b, 4)?;
    let mut at = 6;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let len = *b
            .get(at)
            .ok_or_else(|| Error::Protocol("AFP reply ends early".into()))?
            as usize;
        let flag = *b.get(at + 1).unwrap_or(&0);
        let rec = b
            .get(at + 2..at + len)
            .ok_or_else(|| Error::Protocol("AFP record ends early".into()))?;
        let is_dir = flag & 0x80 != 0;
        out.push(parse_params(rec, is_dir, if is_dir { dbm } else { fbm })?);
        at += len.max(2);
    }
    Ok(out)
}

/// The DES step of Randnum Exchange: the server's random number, encrypted with the password.
pub fn randnum_answer(password: &[u8], random: &[u8; 8]) -> [u8; 8] {
    use des::cipher::{BlockEncrypt, KeyInit};
    let mut key = [0u8; 8];
    for (k, p) in key.iter_mut().zip(password) {
        *k = *p;
    }
    let c = des::Des::new(&key.into());
    let mut block = (*random).into();
    c.encrypt_block(&mut block);
    block.into()
}

#[derive(Debug, Clone)]
pub struct Options {
    pub charset: Charset,
    pub connect_timeout: Duration,
    pub timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            charset: Charset::MacRoman,
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
struct Volume {
    raw: Vec<u8>,
    shown: String,
}

/// A logged-in AFP session.
pub struct AfpSession {
    dsi: Dsi,
    opts: Options,
    pub info: ServerInfo,
    /// The AFP version and login method agreed on.
    pub version: String,
    pub uam: String,
    volumes: Vec<Volume>,
    open: HashMap<String, u16>,
    /// The largest request the server takes, from DSIOpenSession.
    quantum: u32,
    closed: bool,
}

/// The server's info (`DSIGetStatus`), without a session or a login.
pub fn server_info(host: &str, port: u16, opts: &Options) -> Result<ServerInfo> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))?;
    let tcp = TcpStream::connect_timeout(&addr, opts.connect_timeout)?;
    let mut probe = Dsi::new(tcp, opts.timeout)?;
    let (_, status) = probe.request(dsi::GET_STATUS, &[], 0)?;
    parse_server_info(&status)
}

/// Connect, read the server's info, open a DSI session and log in.
///
/// An empty `user` logs in as guest.
pub fn connect(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    opts: &Options,
) -> Result<AfpSession> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::NotFound(format!("no address for {host}")))?;
    let dial = || {
        TcpStream::connect_timeout(&addr, opts.connect_timeout).map_err(|e| {
            if e.kind() == ErrorKind::TimedOut {
                Error::Timeout(opts.connect_timeout)
            } else {
                Error::Io(e)
            }
        })
    };
    // GetStatus answers on a connection of its own, before any session.
    let mut probe = Dsi::new(dial()?, opts.timeout)?;
    let (_, status) = probe.request(dsi::GET_STATUS, &[], 0)?;
    drop(probe);
    let info = parse_server_info(&status)?;
    let version = VERSIONS
        .iter()
        .find(|v| info.versions.iter().any(|s| s == *v))
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "the server speaks {:?}; legcon speaks AFP 2.0 to 2.2",
                info.versions
            ))
        })?
        .to_string();
    let mut dsi = Dsi::new(dial()?, opts.timeout)?;
    // Our attention quantum, as the Mac client sends it.
    let (code, opt) = dsi.request(dsi::OPEN_SESSION, &[1, 4, 0, 0, 4, 0], 0)?;
    if code != 0 {
        return Err(afp_error(code, "opening the session"));
    }
    let mut quantum = 0x2000;
    let mut i = 0;
    while i + 2 <= opt.len() {
        let (t, l) = (opt[i], opt[i + 1] as usize);
        if t == 0 && l == 4 {
            quantum = u32_at(&opt, i + 2)?;
        }
        i += 2 + l;
    }
    let mut s = AfpSession {
        dsi,
        opts: opts.clone(),
        info,
        version,
        uam: String::new(),
        volumes: Vec::new(),
        open: HashMap::new(),
        quantum,
        closed: false,
    };
    s.login(user, password)?;
    s.read_volumes()?;
    Ok(s)
}

impl AfpSession {
    fn call(&mut self, data: &[u8], what: &str) -> Result<Vec<u8>> {
        if self.closed {
            return Err(Error::NotConnected);
        }
        let (code, reply) = self.dsi.request(dsi::COMMAND, data, 0)?;
        if code != 0 {
            return Err(afp_error(code, what));
        }
        Ok(reply)
    }

    fn enc(&self, t: &str) -> Vec<u8> {
        self.opts
            .charset
            .encode(t)
            .unwrap_or_else(|| t.as_bytes().to_vec())
    }

    fn login(&mut self, user: &str, password: &str) -> Result<()> {
        let offered = |u: &str| self.info.uams.iter().any(|x| x.eq_ignore_ascii_case(u));
        let mut head = vec![cmd::LOGIN];
        let push_p = |v: &mut Vec<u8>, s: &[u8]| {
            v.push(s.len() as u8);
            v.extend_from_slice(s);
        };
        push_p(&mut head, self.version.as_bytes());
        let uname = self.enc(user);
        let pw = self.enc(password);
        if user.is_empty() {
            push_p(&mut head, b"No User Authent");
            self.call(&head, "guest login")?;
            self.uam = "guest".into();
            return Ok(());
        }
        if offered("Randnum Exchange") && pw.len() <= 8 {
            let mut m = head.clone();
            push_p(&mut m, b"Randnum Exchange");
            push_p(&mut m, &uname);
            let (code, reply) = self.dsi.request(dsi::COMMAND, &m, 0)?;
            if code != err::AUTH_CONTINUE {
                return Err(afp_error(code, "logging in"));
            }
            let id = u16_at(&reply, 0)?;
            let random: [u8; 8] = reply
                .get(2..10)
                .and_then(|r| r.try_into().ok())
                .ok_or_else(|| Error::Protocol("Randnum reply ends early".into()))?;
            let mut cont = vec![cmd::LOGIN_CONT, 0];
            cont.extend_from_slice(&id.to_be_bytes());
            cont.extend_from_slice(&randnum_answer(&pw, &random));
            let (code, _) = self.dsi.request(dsi::COMMAND, &cont, 0)?;
            if code != 0 {
                return Err(afp_error(code, "logging in"));
            }
            self.uam = "Randnum Exchange".into();
            return Ok(());
        }
        if offered("Cleartxt Passwrd") {
            let mut m = head;
            push_p(&mut m, b"Cleartxt Passwrd");
            push_p(&mut m, &uname);
            if m.len() % 2 == 1 {
                m.push(0);
            }
            let mut p8 = [0u8; 8];
            for (d, s) in p8.iter_mut().zip(&pw) {
                *d = *s;
            }
            m.extend_from_slice(&p8);
            self.call(&m, "logging in")?;
            self.uam = "Cleartxt Passwrd".into();
            return Ok(());
        }
        Err(Error::Auth(format!(
            "the server offers no login legcon can use ({:?})",
            self.info.uams
        )))
    }

    fn read_volumes(&mut self) -> Result<()> {
        let r = self.call(&[cmd::GET_SRVR_PARMS, 0], "listing volumes")?;
        let count = *r.get(4).unwrap_or(&0);
        let mut at = 5;
        let mut vols = Vec::new();
        for _ in 0..count {
            let Some(name) = pstr(&r, at + 1) else {
                break;
            };
            at += 2 + name.len();
            vols.push(Volume {
                shown: self.opts.charset.decode(name).replace(':', "-"),
                raw: name.to_vec(),
            });
        }
        self.volumes = vols;
        Ok(())
    }

    /// The volumes shown at the root.
    pub fn volumes(&self) -> Vec<String> {
        self.volumes.iter().map(|v| v.shown.clone()).collect()
    }

    fn volume(&mut self, shown: &str) -> Result<u16> {
        if let Some(&id) = self.open.get(shown) {
            return Ok(id);
        }
        let v = self
            .volumes
            .iter()
            .find(|v| v.shown.eq_ignore_ascii_case(shown))
            .cloned()
            .ok_or_else(|| Error::NotFound(shown.to_string()))?;
        let mut m = vec![cmd::OPEN_VOL, 0];
        m.extend_from_slice(&(1u16 << 5).to_be_bytes()); // volume ID
        m.push(v.raw.len() as u8);
        m.extend_from_slice(&v.raw);
        let r = self.call(&m, shown)?;
        let id = u16_at(&r, 2)?;
        self.open.insert(v.shown.clone(), id);
        Ok(id)
    }

    /// The volume a path is on, and the long-name pathname (`\0`-separated) inside it.
    fn locate(&mut self, path: &RemotePath) -> Result<(u16, Vec<u8>)> {
        let parts = path.parts();
        let vol = parts
            .first()
            .ok_or_else(|| Error::Unsupported("the server itself is not a folder".into()))?;
        let id = self.volume(vol)?;
        let mut p = Vec::new();
        for (i, part) in parts[1..].iter().enumerate() {
            if i > 0 {
                p.push(0);
            }
            p.extend_from_slice(&self.enc(part));
        }
        if p.len() > 255 {
            return Err(Error::Unsupported(format!(
                "{path} is too deep for AFP 2.x paths"
            )));
        }
        Ok((id, p))
    }

    fn pathname(m: &mut Vec<u8>, p: &[u8]) {
        m.push(2); // long names
        m.push(p.len() as u8);
        m.extend_from_slice(p);
    }

    fn head(op: u8, flag: u8, vol: u16, dir: u32) -> Vec<u8> {
        let mut m = vec![op, flag];
        m.extend_from_slice(&vol.to_be_bytes());
        m.extend_from_slice(&dir.to_be_bytes());
        m
    }

    fn params(&mut self, path: &RemotePath) -> Result<Node> {
        let (vol, p) = self.locate(path)?;
        let mut m = Self::head(cmd::GET_FILE_DIR_PARMS, 0, vol, 2);
        m.extend_from_slice(&FILE_BITMAP.to_be_bytes());
        m.extend_from_slice(&DIR_BITMAP.to_be_bytes());
        Self::pathname(&mut m, &p);
        let r = self.call(&m, &path.to_string())?;
        let (fbm, dbm) = (u16_at(&r, 0)?, u16_at(&r, 2)?);
        let is_dir = r.get(4).is_some_and(|f| f & 0x80 != 0);
        parse_params(
            r.get(6..).unwrap_or(&[]),
            is_dir,
            if is_dir { dbm } else { fbm },
        )
    }

    fn entry(&self, n: &Node) -> Entry {
        let mut e = Entry::new(
            self.opts.charset.decode(&n.name).replace(':', "-"),
            if n.is_dir {
                EntryKind::Folder
            } else {
                EntryKind::File
            },
        );
        e.raw_name = n.name.clone();
        if !n.is_dir {
            e.size = Some(u64::from(n.data_len));
            e.has_resource_fork = n.rsrc_len > 0;
            let (t, c): ([u8; 4], [u8; 4]) = (
                n.finder_info[..4].try_into().unwrap(),
                n.finder_info[4..8].try_into().unwrap(),
            );
            if t != [0; 4] || c != [0; 4] {
                e.mac_type_creator = Some((t, c));
            }
        }
        e.modified = Some(Timestamp {
            unix: AFP_EPOCH_UNIX + i64::from(n.mod_date),
            precision: TimePrecision::Exact,
        });
        e
    }

    fn open_fork(&mut self, path: &RemotePath, rsrc: bool, write: bool) -> Result<u16> {
        let (vol, p) = self.locate(path)?;
        let mut m = Self::head(cmd::OPEN_FORK, if rsrc { 0x80 } else { 0 }, vol, 2);
        m.extend_from_slice(&0u16.to_be_bytes());
        m.extend_from_slice(&(if write { 3u16 } else { 1u16 }).to_be_bytes());
        Self::pathname(&mut m, &p);
        let r = self.call(&m, &path.to_string())?;
        u16_at(&r, 2)
    }

    fn close_fork(&mut self, fork: u16) -> Result<()> {
        let mut m = vec![cmd::CLOSE_FORK, 0];
        m.extend_from_slice(&fork.to_be_bytes());
        self.call(&m, "closing a file").map(drop)
    }

    fn read_fork(&mut self, fork: u16, at: u32, count: u32) -> Result<(Vec<u8>, bool)> {
        let mut m = vec![cmd::READ, 0];
        m.extend_from_slice(&fork.to_be_bytes());
        m.extend_from_slice(&at.to_be_bytes());
        m.extend_from_slice(&count.to_be_bytes());
        m.extend_from_slice(&[0, 0]);
        let (code, data) = self.dsi.request(dsi::COMMAND, &m, 0)?;
        match code {
            0 => Ok((data, false)),
            err::EOF => Ok((data, true)),
            c => Err(afp_error(c, "reading")),
        }
    }

    fn write_fork(&mut self, fork: u16, at: u32, data: &[u8]) -> Result<()> {
        let mut m = vec![cmd::WRITE, 0];
        m.extend_from_slice(&fork.to_be_bytes());
        m.extend_from_slice(&at.to_be_bytes());
        m.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let header = m.len() as u32;
        m.extend_from_slice(data);
        let (code, _) = self.dsi.request(dsi::WRITE, &m, header)?;
        if code != 0 {
            return Err(afp_error(code, "writing"));
        }
        Ok(())
    }

    fn truncate(&mut self, fork: u16, rsrc: bool) -> Result<()> {
        let mut m = vec![cmd::SET_FORK_PARMS, 0];
        m.extend_from_slice(&fork.to_be_bytes());
        m.extend_from_slice(&(if rsrc { bit::RSRC_LEN } else { bit::DATA_LEN }).to_be_bytes());
        m.extend_from_slice(&0u32.to_be_bytes());
        self.call(&m, "emptying a file").map(drop)
    }

    /// The largest read or write asked for at once.
    fn chunk(&self) -> u32 {
        self.quantum.clamp(4096, 128 * 1024) - 64
    }

    fn read_whole(&mut self, fork: u16) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let (d, eof) = self.read_fork(fork, out.len() as u32, self.chunk())?;
            let empty = d.is_empty();
            out.extend_from_slice(&d);
            if eof || empty {
                return Ok(out);
            }
        }
    }
}

struct ForkReader<'a> {
    s: &'a mut AfpSession,
    fork: u16,
    at: u32,
    eof: bool,
}

impl Read for ForkReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.eof || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u32).min(self.s.chunk());
        let (d, eof) = self
            .s
            .read_fork(self.fork, self.at, want)
            .map_err(std::io::Error::other)?;
        let n = d.len().min(buf.len());
        buf[..n].copy_from_slice(&d[..n]);
        self.at += n as u32;
        self.eof = eof || n == 0;
        Ok(n)
    }
}

impl Drop for ForkReader<'_> {
    fn drop(&mut self) {
        let _ = self.s.close_fork(self.fork);
    }
}

struct ForkWriter<'a> {
    s: &'a mut AfpSession,
    fork: u16,
    at: u32,
}

impl Write for ForkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.s.chunk() as usize);
        self.s
            .write_fork(self.fork, self.at, &buf[..n])
            .map_err(std::io::Error::other)?;
        self.at += n as u32;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for ForkWriter<'_> {
    fn drop(&mut self) {
        let _ = self.s.close_fork(self.fork);
    }
}

impl Connection for AfpSession {
    fn state(&self) -> ConnState {
        if self.closed {
            ConnState::Disconnected
        } else {
            ConnState::Connected
        }
    }

    /// Close the volumes, `FPLogout`, then `DSICloseSession`: the server's user list stays truthful.
    fn disconnect(&mut self, _timeout: Duration) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let vols: Vec<u16> = self.open.values().copied().collect();
        for v in vols {
            let mut m = vec![cmd::CLOSE_VOL, 0];
            m.extend_from_slice(&v.to_be_bytes());
            let _ = self.call(&m, "closing a volume");
        }
        let out = self.call(&[cmd::LOGOUT, 0], "logging out").map(drop);
        let h = dsi::Header {
            flags: 0,
            command: dsi::CLOSE_SESSION,
            id: 0xffff,
            code: 0,
            len: 0,
        };
        let _ = dsi::write_message(&mut self.dsi.stream, h, &[]);
        let _ = self.dsi.stream.shutdown(std::net::Shutdown::Both);
        self.closed = true;
        out
    }
}

impl FileSession for AfpSession {
    fn charset(&self) -> lc_core::Charset {
        self.opts.charset
    }

    /// AFP 2.x long names are at most 31 bytes.
    fn max_name_len(&self) -> Option<usize> {
        Some(31)
    }
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
        Ok(match self.volumes.as_slice() {
            [only] => RemotePath::parse(PathStyle::Mac, &only.shown.clone()),
            _ => RemotePath::root(PathStyle::Mac),
        })
    }

    fn list(&mut self, dir: &RemotePath) -> Result<Vec<Entry>> {
        if dir.is_root() {
            return Ok(self
                .volumes
                .iter()
                .map(|v| Entry::new(v.shown.clone(), EntryKind::Folder))
                .collect());
        }
        let (vol, p) = self.locate(dir)?;
        let what = dir.to_string();
        let mut out = Vec::new();
        let mut start: u16 = 1;
        loop {
            let mut m = Self::head(cmd::ENUMERATE, 0, vol, 2);
            m.extend_from_slice(&FILE_BITMAP.to_be_bytes());
            m.extend_from_slice(&DIR_BITMAP.to_be_bytes());
            m.extend_from_slice(&64u16.to_be_bytes());
            m.extend_from_slice(&start.to_be_bytes());
            m.extend_from_slice(&(self.chunk().min(0xfff0) as u16).to_be_bytes());
            Self::pathname(&mut m, &p);
            let (code, r) = self.dsi.request(dsi::COMMAND, &m, 0)?;
            match code {
                0 => {}
                // Past the last item: the listing is complete.
                err::OBJECT_NOT_FOUND if start > 1 => break,
                // An empty folder answers the first request the same way (netatalk, Mac OS).
                err::OBJECT_NOT_FOUND if self.params(dir).is_ok() => break,
                c => return Err(afp_error(c, &what)),
            }
            let nodes = parse_enumerate(&r)?;
            if nodes.is_empty() {
                break;
            }
            start += nodes.len() as u16;
            // The Finder hides what has the invisible flag (Desktop DB, Trash, ...); so do we.
            out.extend(
                nodes
                    .iter()
                    .filter(|n| !n.invisible())
                    .map(|n| self.entry(n)),
            );
        }
        Ok(out)
    }

    fn stat(&mut self, path: &RemotePath) -> Result<Entry> {
        if path.parts().len() == 1 {
            let v = self.volume(path.name())?;
            let _ = v;
            return Ok(Entry::new(path.name(), EntryKind::Folder));
        }
        let mut n = self.params(path)?;
        if n.name.is_empty() {
            n.name = self.enc(path.name());
        }
        Ok(self.entry(&n))
    }

    fn open_read(&mut self, path: &RemotePath, at: u64) -> Result<Box<dyn Read + Send + '_>> {
        let fork = self.open_fork(path, false, false)?;
        Ok(Box::new(ForkReader {
            s: self,
            fork,
            at: u32::try_from(at)
                .map_err(|_| Error::Unsupported("AFP 2.x files end at 4 GB".into()))?,
            eof: false,
        }))
    }

    fn open_write(
        &mut self,
        path: &RemotePath,
        mode: WriteMode,
    ) -> Result<Box<dyn Write + Send + '_>> {
        let (vol, p) = self.locate(path)?;
        if !matches!(mode, WriteMode::Resume(_)) {
            // A soft create fails on an existing file, which Create wants; Overwrite empties it below.
            let mut m = Self::head(cmd::CREATE_FILE, 0, vol, 2);
            Self::pathname(&mut m, &p);
            let (code, _) = self.dsi.request(dsi::COMMAND, &m, 0)?;
            match (code, mode) {
                (0, _) | (err::OBJECT_EXISTS, WriteMode::Overwrite) => {}
                (c, _) => return Err(afp_error(c, &path.to_string())),
            }
        }
        let fork = self.open_fork(path, false, true)?;
        let at = match mode {
            WriteMode::Resume(n) => n as u32,
            _ => {
                self.truncate(fork, false)?;
                0
            }
        };
        Ok(Box::new(ForkWriter { s: self, fork, at }))
    }

    fn mkdir(&mut self, path: &RemotePath) -> Result<()> {
        let (vol, p) = self.locate(path)?;
        let mut m = Self::head(cmd::CREATE_DIR, 0, vol, 2);
        Self::pathname(&mut m, &p);
        self.call(&m, &path.to_string()).map(drop)
    }

    fn remove(&mut self, path: &RemotePath) -> Result<()> {
        let (vol, p) = self.locate(path)?;
        let mut m = Self::head(cmd::DELETE, 0, vol, 2);
        Self::pathname(&mut m, &p);
        self.call(&m, &path.to_string()).map(drop)
    }

    fn rename(&mut self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        let what = from.to_string();
        let (vol, src) = self.locate(from)?;
        let to_dir = to
            .parent()
            .ok_or_else(|| Error::Unsupported("cannot move to the root".into()))?;
        let (tvol, dst_dir) = self.locate(&to_dir)?;
        if tvol != vol {
            return Err(Error::Unsupported("AFP cannot move between volumes".into()));
        }
        let new_name = self.enc(to.name());
        if from.parent() == Some(to_dir) {
            let mut m = Self::head(cmd::RENAME, 0, vol, 2);
            Self::pathname(&mut m, &src);
            Self::pathname(&mut m, &new_name);
            return self.call(&m, &what).map(drop);
        }
        let mut m = vec![cmd::MOVE_AND_RENAME, 0];
        m.extend_from_slice(&vol.to_be_bytes());
        m.extend_from_slice(&2u32.to_be_bytes());
        m.extend_from_slice(&2u32.to_be_bytes());
        Self::pathname(&mut m, &src);
        Self::pathname(&mut m, &dst_dir);
        Self::pathname(&mut m, &new_name);
        self.call(&m, &what).map(drop)
    }

    fn keeps_mac_extras(&self) -> bool {
        true
    }

    fn mac_extras(&mut self, path: &RemotePath) -> Result<Option<MacExtras>> {
        let n = self.params(path)?;
        if n.is_dir {
            return Ok(None);
        }
        let resource_fork = if n.rsrc_len > 0 {
            let fork = self.open_fork(path, true, false)?;
            let r = self.read_whole(fork);
            let _ = self.close_fork(fork);
            r?
        } else {
            Vec::new()
        };
        let x = MacExtras {
            finder_info: n.finder_info,
            resource_fork,
        };
        Ok((!x.is_empty()).then_some(x))
    }

    fn set_mac_extras(&mut self, path: &RemotePath, extras: &MacExtras) -> Result<()> {
        let (vol, p) = self.locate(path)?;
        let mut m = Self::head(cmd::SET_FILE_DIR_PARMS, 0, vol, 2);
        m.extend_from_slice(&bit::FINDER_INFO.to_be_bytes());
        Self::pathname(&mut m, &p);
        if m.len() % 2 == 1 {
            m.push(0);
        }
        m.extend_from_slice(&extras.finder_info);
        self.call(&m, &path.to_string())?;
        let fork = self.open_fork(path, true, true)?;
        let result = (|| {
            self.truncate(fork, true)?;
            let mut at = 0;
            for c in extras.resource_fork.chunks(self.chunk() as usize) {
                self.write_fork(fork, at, c)?;
                at += c.len() as u32;
            }
            Ok(())
        })();
        let _ = self.close_fork(fork);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_from_an_appleshare_ip_reply() {
        let mut b = vec![0u8; 10];
        b.push(9);
        b.extend_from_slice(b"Power Mac");
        let machine = b.len() as u16;
        b.extend_from_slice(b"\x09Macintosh");
        let vers = b.len() as u16;
        b.extend_from_slice(b"\x02\x0eAFPVersion 2.1\x06AFP2.2");
        let uams = b.len() as u16;
        b.extend_from_slice(b"\x02\x10Cleartxt Passwrd\x10Randnum Exchange");
        b[0..2].copy_from_slice(&machine.to_be_bytes());
        b[2..4].copy_from_slice(&vers.to_be_bytes());
        b[4..6].copy_from_slice(&uams.to_be_bytes());
        let i = parse_server_info(&b).unwrap();
        assert_eq!(i.name, "Power Mac");
        assert_eq!(i.machine, "Macintosh");
        assert_eq!(i.versions, ["AFPVersion 2.1", "AFP2.2"]);
        assert_eq!(i.uams, ["Cleartxt Passwrd", "Randnum Exchange"]);
    }

    #[test]
    fn randnum_is_des_with_the_password_as_key() {
        // FIPS 81 / the classic DES test vector: key 0x133457799BBCDFF1, plain 0x0123456789ABCDEF.
        let key = [0x13, 0x34, 0x57, 0x79, 0x9b, 0xbc, 0xdf, 0xf1];
        let plain = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        assert_eq!(
            randnum_answer(&key, &plain),
            [0x85, 0xe8, 0x13, 0x54, 0x0f, 0x0a, 0xb4, 0x05]
        );
    }
}
