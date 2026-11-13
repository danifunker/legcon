#![allow(dead_code)]
//! Pretend 1990s SMB servers on localhost (shared by the tests and `examples/fake_smbd.rs`): an NT 4 box (NT LM 0.12, user-level, NTLM) and a
//! Windows 95 box (LANMAN2.1, share-level, refuses `*SMBSERVER`). Each checks the login answers
//! byte for byte, lists shares and folders, and keeps files in memory.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lc_core::Charset;
use lc_smb::auth;
use lc_smb::netbios;
use lc_smb::wire::{self, Buf, Message};

pub const CHALLENGE: [u8; 8] = *b"legcon!!";
pub const PASSWORD: &str = "Secret";

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Nt4,
    Win95,
}

#[derive(Default)]
pub struct Disk {
    /// `\PATH` upper-cased -> contents; folders map to `None`.
    pub files: BTreeMap<String, Option<Vec<u8>>>,
    pub seen: Vec<String>,
}

fn reply(req: &Message, words: Vec<u8>, bytes: Vec<u8>) -> Message {
    Message {
        flags: wire::FLAGS_REPLY | wire::FLAGS,
        words,
        bytes,
        status: 0,
        ..req.clone()
    }
}

fn fail(req: &Message, kind: Kind, nt: u32, class: u32, code: u32) -> Message {
    let mut m = reply(req, Vec::new(), Vec::new());
    if kind == Kind::Nt4 {
        m.flags2 |= 0x4000;
        m.status = nt;
    } else {
        m.status = class | (code << 16);
    }
    m
}

fn info_standard(name: &str, data: &Option<Vec<u8>>) -> Vec<u8> {
    let date = ((1997 - 1980) << 9) | (11 << 5) | 13;
    let time = (10 << 11) | (4 << 5);
    let (size, attrs) = match data {
        Some(d) => (d.len() as u32, 0x20u16),
        None => (0, 0x10),
    };
    let raw = Charset::Windows1252.encode(name).unwrap();
    Buf::default()
        .u16(date)
        .u16(time)
        .u16(date)
        .u16(time)
        .u16(date)
        .u16(time)
        .u32(size)
        .u32(size)
        .u16(attrs)
        .u8(raw.len() as u8)
        .sz(&raw)
        .0
}

/// Reply parameters and data of a transaction, laid out as one response message.
fn trans_reply(req: &Message, params: &[u8], data: &[u8]) -> Message {
    let words_len = 10 * 2;
    let bytes_at = wire::HEADER + 1 + words_len + 2;
    let p_at = bytes_at + 1;
    let d_at = p_at + params.len() + 1;
    let words = Buf::default()
        .u16(params.len() as u16)
        .u16(data.len() as u16)
        .u16(0)
        .u16(params.len() as u16)
        .u16(p_at as u16)
        .u16(0)
        .u16(data.len() as u16)
        .u16(d_at as u16)
        .u16(0)
        .u8(0)
        .u8(0)
        .0;
    let bytes = Buf::default().u8(0).raw(params).u8(0).raw(data).0;
    reply(req, words, bytes)
}

fn handle(
    kind: Kind,
    req: &Message,
    raw: &[u8],
    disk: &Mutex<Disk>,
    next_fid: &mut u16,
    open: &mut BTreeMap<u16, String>,
) -> Message {
    let cs = Charset::Windows1252;
    let path_of = |b: &[u8], at: usize| cs.decode(wire::cstr(b, at).0).to_ascii_uppercase();
    match req.command {
        wire::NEGOTIATE => {
            let dialects: Vec<String> = req
                .bytes
                .split(|&b| b == 0)
                .filter(|d| d.len() > 1)
                .map(|d| String::from_utf8_lossy(&d[1..]).into_owned())
                .collect();
            let want = if kind == Kind::Nt4 {
                "NT LM 0.12"
            } else {
                "LANMAN2.1"
            };
            let idx = dialects.iter().position(|d| d == want).unwrap() as u16;
            if kind == Kind::Nt4 {
                let words = Buf::default()
                    .u16(idx)
                    .u8(0x03)
                    .u16(50)
                    .u16(1)
                    .u32(4356)
                    .u32(65536)
                    .u32(0)
                    .u32(0)
                    .u32(0)
                    .u32(0)
                    .u16(0)
                    .u8(8)
                    .0;
                reply(
                    req,
                    words,
                    Buf::default().raw(&CHALLENGE).sz(b"WORKGROUP").0,
                )
            } else {
                let words = Buf::default()
                    .u16(idx)
                    .u16(0x02)
                    .u16(2920)
                    .u16(50)
                    .u16(1)
                    .u16(0)
                    .u32(0)
                    .u16(0)
                    .u16(0)
                    .u16(0)
                    .u16(8)
                    .u16(0)
                    .0;
                reply(
                    req,
                    words,
                    Buf::default().raw(&CHALLENGE).sz(b"WORKGROUP").0,
                )
            }
        }
        wire::SESSION_SETUP_ANDX => {
            if kind == Kind::Nt4 {
                let (lm_len, nt_len) = (usize::from(req.word(7)), usize::from(req.word(8)));
                let nt = &req.bytes[lm_len..lm_len + nt_len];
                let account = cs.decode(wire::cstr(&req.bytes, lm_len + nt_len).0);
                disk.lock().unwrap().seen.push(format!("login {account}"));
                if account != "dani"
                    || nt != auth::v1_response(&auth::nt_hash(PASSWORD), &CHALLENGE)
                {
                    return fail(req, kind, 0xc000_006d, 2, 2);
                }
            }
            let mut m = reply(
                req,
                Buf::default().no_andx().u16(0).0,
                Buf::default()
                    .sz(b"Windows NT 4.0")
                    .sz(b"NT LAN Manager 4.0")
                    .sz(b"WORKGROUP")
                    .0,
            );
            m.uid = 100;
            m
        }
        wire::TREE_CONNECT_ANDX => {
            let pw_len = usize::from(req.word(3));
            let path = path_of(&req.bytes, pw_len);
            let share = path.rsplit('\\').next().unwrap().to_string();
            disk.lock().unwrap().seen.push(format!("tree {path}"));
            if kind == Kind::Win95 && share != "IPC$" {
                let want = auth::v1_response(&auth::lm_hash(PASSWORD.as_bytes()), &CHALLENGE);
                if req.bytes[..pw_len] != want {
                    return fail(req, kind, 0, 2, 2);
                }
            }
            if share != "IPC$" && share != "C" {
                return fail(req, kind, 0xc000_00cc, 2, 6);
            }
            let mut m = reply(
                req,
                Buf::default().no_andx().u16(0).0,
                Buf::default().sz(b"A:").0,
            );
            m.tid = if share == "C" { 2 } else { 1 };
            m
        }
        wire::TRANSACTION => {
            // RAP NetShareEnum, level 1: name[13], pad, type, remark pointer.
            let mut data = Vec::new();
            for (name, ty) in [("C", 0u16), ("PRINTER", 1), ("IPC$", 3)] {
                let mut n = [0u8; 14];
                n[..name.len()].copy_from_slice(name.as_bytes());
                data.extend(Buf::default().raw(&n).u16(ty).u32(0).0);
            }
            trans_reply(req, &Buf::default().u16(0).u16(0).u16(3).u16(3).0, &data)
        }
        wire::TRANSACTION2 => {
            let p_count = usize::from(req.word(9));
            let p_at = usize::from(req.word(10));
            let params = &raw[p_at..p_at + p_count];
            assert_eq!(req.word(14), 1, "FIND_FIRST2");
            assert_eq!(
                u16::from_le_bytes([params[6], params[7]]),
                1,
                "info level 1"
            );
            let pattern = path_of(params, 12);
            let dir = pattern
                .trim_end_matches('*')
                .trim_end_matches('\\')
                .to_string();
            let d = disk.lock().unwrap();
            let mut data = Vec::new();
            let mut n = 0u16;
            for (k, v) in &d.files {
                if let Some(rest) = k.strip_prefix(&format!("{dir}\\")) {
                    if !rest.contains('\\') {
                        let name = d
                            .seen
                            .iter()
                            .find_map(|s| s.strip_prefix(&format!("name {k}=")).map(str::to_string))
                            .unwrap_or_else(|| rest.to_string());
                        data.extend(info_standard(&name, v));
                        n += 1;
                    }
                }
            }
            if n == 0 {
                return fail(req, kind, 0x8000_0006, 1, 18);
            }
            trans_reply(
                req,
                &Buf::default().u16(1).u16(n).u16(1).u16(0).u16(0).0,
                &data,
            )
        }
        wire::OPEN_ANDX => {
            let path = path_of(&req.bytes, 0);
            let mode = req.word(8);
            let mut d = disk.lock().unwrap();
            d.seen.push(format!("open {path} {mode:#x}"));
            let exists = d.files.contains_key(&path);
            if !exists && mode & 0x10 == 0 {
                return fail(req, kind, 0xc000_0034, 1, 2);
            }
            if mode & 0x03 == 2 || !exists {
                d.files.insert(path.clone(), Some(Vec::new()));
            }
            *next_fid += 1;
            open.insert(*next_fid, path);
            reply(
                req,
                Buf::default()
                    .no_andx()
                    .u16(*next_fid)
                    .u16(0x20)
                    .u32(0)
                    .u32(0)
                    .u16(0)
                    .u16(0)
                    .u16(0)
                    .u16(1)
                    .u16(0)
                    .u32(0)
                    .0,
                Vec::new(),
            )
        }
        wire::READ_ANDX => {
            let (fid, off, max) = (req.word(2), req.dword(3) as usize, usize::from(req.word(5)));
            let d = disk.lock().unwrap();
            let data = d.files[&open[&fid]].as_ref().unwrap();
            let chunk = &data[off.min(data.len())..(off + max).min(data.len())];
            let data_at = wire::HEADER + 1 + 24 + 2 + 1;
            let words = Buf::default()
                .no_andx()
                .u16(0)
                .u16(0)
                .u16(0)
                .u16(chunk.len() as u16)
                .u16(data_at as u16)
                .u32(0)
                .u32(0)
                .u16(0)
                .0;
            reply(req, words, Buf::default().u8(0).raw(chunk).0)
        }
        wire::WRITE_ANDX => {
            let (fid, off, len, at) = (
                req.word(2),
                req.dword(3) as usize,
                usize::from(req.word(10)),
                usize::from(req.word(11)),
            );
            let mut d = disk.lock().unwrap();
            let path = open[&fid].clone();
            let f = d.files.get_mut(&path).unwrap().as_mut().unwrap();
            f.resize(f.len().max(off + len), 0);
            f[off..off + len].copy_from_slice(&raw[at..at + len]);
            reply(
                req,
                Buf::default().no_andx().u16(len as u16).u16(0).u32(0).0,
                Vec::new(),
            )
        }
        wire::CLOSE => {
            open.remove(&req.word(0));
            reply(req, Vec::new(), Vec::new())
        }
        wire::CREATE_DIRECTORY => {
            disk.lock()
                .unwrap()
                .files
                .insert(path_of(&req.bytes, 1), None);
            reply(req, Vec::new(), Vec::new())
        }
        wire::DELETE | wire::DELETE_DIRECTORY => {
            let path = path_of(&req.bytes, 1);
            let mut d = disk.lock().unwrap();
            let is_dir = matches!(d.files.get(&path), Some(None));
            if (req.command == wire::DELETE) == is_dir || d.files.remove(&path).is_none() {
                return fail(req, kind, 0xc000_0022, 1, 5);
            }
            reply(req, Vec::new(), Vec::new())
        }
        wire::RENAME => {
            let (old, next) = wire::cstr(&req.bytes, 1);
            let old = cs.decode(old).to_ascii_uppercase();
            let new = path_of(&req.bytes, next + 1);
            let mut d = disk.lock().unwrap();
            let v = d.files.remove(&old).unwrap();
            d.files.insert(new, v);
            reply(req, Vec::new(), Vec::new())
        }
        wire::TREE_DISCONNECT | wire::LOGOFF_ANDX => {
            disk.lock()
                .unwrap()
                .seen
                .push(format!("bye {:#x} tid {}", req.command, req.tid));
            reply(
                req,
                if req.command == wire::LOGOFF_ANDX {
                    Buf::default().no_andx().0
                } else {
                    Vec::new()
                },
                Vec::new(),
            )
        }
        other => panic!("unexpected SMB command {other:#x}"),
    }
}

/// Serve until one session logs off.
pub fn serve(kind: Kind, listener: &TcpListener, disk: &Arc<Mutex<Disk>>) {
    loop {
        let Ok((mut s, _)) = listener.accept() else {
            return;
        };
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let Ok((t, payload)) = netbios::recv(&mut s) else {
            continue;
        };
        assert_eq!(t, netbios::SESSION_REQUEST);
        let any = payload[..34] == netbios::encode_name(netbios::ANY_SERVER, 0x20)[..];
        if kind == Kind::Win95 && any {
            netbios::send(&mut s, netbios::NEGATIVE_RESPONSE, &[0x82]).unwrap();
            continue;
        }
        netbios::send(&mut s, netbios::POSITIVE_RESPONSE, &[]).unwrap();
        let (mut fid, mut open) = (0u16, BTreeMap::new());
        while let Ok((_, raw)) = netbios::recv(&mut s) {
            let req = Message::decode(&raw).unwrap();
            let r = handle(kind, &req, &raw, disk, &mut fid, &mut open);
            netbios::send(&mut s, netbios::SESSION_MESSAGE, &r.encode()).unwrap();
            if req.command == wire::LOGOFF_ANDX {
                return;
            }
        }
        return;
    }
}

pub fn start(kind: Kind) -> (u16, Arc<Mutex<Disk>>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let disk = Arc::new(Mutex::new(Disk::default()));
    seed(&disk);
    let d2 = Arc::clone(&disk);
    let t = std::thread::spawn(move || serve(kind, &listener, &d2));
    (port, disk, t)
}

/// The files every fake server starts with.
pub fn seed(disk: &Arc<Mutex<Disk>>) {
    let mut d = disk.lock().unwrap();
    d.files.insert("\\WINDOWS".into(), None);
    d.files.insert(
        "\\AUTOEXEC.BAT".into(),
        Some(b"@ECHO OFF\r\nPROMPT $P$G\r\n".to_vec()),
    );
    d.files.insert(
        "\\CONFIG.SYS".into(),
        Some(b"DEVICE=C:\\WINDOWS\\HIMEM.SYS\r\n".to_vec()),
    );
    d.files.insert(
        "\\WINDOWS\\WIN.INI".into(),
        Some(b"[windows]\r\nload=\r\n".to_vec()),
    );
    d.files.insert("\\WINDOWS\\SYSTEM".into(), None);
    d.files.insert(
        "\\WINDOWS\\SYSTEM\\VMM32.VXD".into(),
        Some(vec![0x4d; 4096]),
    );
    d.files
        .insert("\\R\u{c9}SUM\u{c9}.TXT".into(), Some(b"cv".to_vec()));
    d.seen
        .push("name \\R\u{c9}SUM\u{c9}.TXT=R\u{e9}sum\u{e9}.txt".into());
}
