//! A pretend AppleShare IP server: DSI on TCP, AFP 2.1 / 2.2, Randnum Exchange and Cleartext
//! logins (dani / pw), one volume held in memory with resource forks and Finder info.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use lc_afp::dsi::{self, Header};
use lc_afp::{cmd, err};

type Key = Vec<Vec<u8>>;

#[derive(Clone, Default)]
pub struct Item {
    pub dir: bool,
    pub data: Vec<u8>,
    pub rsrc: Vec<u8>,
    pub finfo: [u8; 32],
    pub id: u32,
}

#[derive(Default)]
pub struct Store {
    pub items: HashMap<Key, Item>,
    next_id: u32,
    /// What happened, for the tests: `login Randnum Exchange`, `logout`, ...
    pub log: Vec<String>,
}

impl Store {
    pub fn add(&mut self, path: &str, dir: bool, data: &[u8], rsrc: &[u8], tc: &[u8; 8]) {
        self.next_id += 1;
        let mut finfo = [0u8; 32];
        finfo[..8].copy_from_slice(tc);
        let key: Key = path.split(':').map(mac).collect();
        self.items.insert(
            key,
            Item {
                dir,
                data: data.to_vec(),
                rsrc: rsrc.to_vec(),
                finfo,
                id: self.next_id + 16,
            },
        );
    }
}

/// UTF-8 test names to MacRoman bytes (only the few characters the tests use).
pub fn mac(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| match c {
            '\u{e9}' => 0x8e,
            '\u{2122}' => 0xaa,
            c => c as u8,
        })
        .collect()
}

pub fn sample() -> Store {
    let mut s = Store::default();
    s.add("Macintosh HD", true, b"", b"", &[0; 8]);
    s.add("Macintosh HD:System Folder", true, b"", b"", &[0; 8]);
    s.add(
        "Macintosh HD:Read Me",
        false,
        b"Welcome to Mac OS 9\r",
        b"",
        b"TEXTttxt",
    );
    s.add(
        "Macintosh HD:SimpleText\u{2122}",
        false,
        b"",
        b"\0\0\x01\0resource fork bytes",
        b"APPLttxt",
    );
    s.add("Macintosh HD:Caf\u{e9}", true, b"", b"", &[0; 8]);
    // The Finder's invisible flag: listings leave it out.
    s.add("Macintosh HD:Desktop DB", false, b"db", b"", b"BTFLDMGR");
    let key: Key = vec![mac("Macintosh HD"), mac("Desktop DB")];
    s.items.get_mut(&key).unwrap().finfo[8] = 0x40;
    s
}

pub struct Fake {
    pub port: u16,
    pub store: Arc<Mutex<Store>>,
}

pub fn start(store: Store) -> Fake {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let store = Arc::new(Mutex::new(store));
    let st = Arc::clone(&store);
    thread::spawn(move || {
        for s in l.incoming() {
            let st = Arc::clone(&st);
            thread::spawn(move || serve(s.unwrap(), st));
        }
    });
    Fake { port, store }
}

fn pascal(v: &mut Vec<u8>, s: &[u8]) {
    v.push(s.len() as u8);
    v.extend_from_slice(s);
}

fn status() -> Vec<u8> {
    let mut b = vec![0u8; 10];
    pascal(&mut b, b"Pretend Power Mac");
    if b.len() % 2 == 1 {
        b.push(0);
    }
    let machine = b.len() as u16;
    pascal(&mut b, b"Macintosh");
    let vers = b.len() as u16;
    b.push(2);
    pascal(&mut b, b"AFPVersion 2.1");
    pascal(&mut b, b"AFP2.2");
    let uams = b.len() as u16;
    b.push(2);
    pascal(&mut b, b"Cleartxt Passwrd");
    pascal(&mut b, b"Randnum Exchange");
    b[0..2].copy_from_slice(&machine.to_be_bytes());
    b[2..4].copy_from_slice(&vers.to_be_bytes());
    b[4..6].copy_from_slice(&uams.to_be_bytes());
    b
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// A type-2 pathname at `at`: its components, and where the next field starts.
fn pathname(b: &[u8], at: usize) -> (Vec<Vec<u8>>, usize) {
    let n = b[at + 1] as usize;
    let p = &b[at + 2..at + 2 + n];
    let parts = if p.is_empty() {
        Vec::new()
    } else {
        p.split(|&c| c == 0).map(<[u8]>::to_vec).collect()
    };
    (parts, at + 2 + n)
}

/// The parameter block for `bitmap`, names after the fixed part.
fn params(key: &Key, it: &Item, bitmap: u16, kids: u16) -> Vec<u8> {
    let mut fixed = Vec::new();
    let mut name_at = None;
    for b in 0..16u16 {
        if bitmap & (1 << b) == 0 {
            continue;
        }
        match (b, it.dir) {
            (0, _) => fixed.extend_from_slice(&[0, 0]),
            (1, _) | (2, _) | (4, _) => fixed.extend_from_slice(&[0, 0, 0, 0]),
            (3, _) => fixed.extend_from_slice(&(-68_000_000i32).to_be_bytes()),
            (5, _) => fixed.extend_from_slice(&it.finfo),
            (6, _) => {
                name_at = Some(fixed.len());
                fixed.extend_from_slice(&[0, 0]);
            }
            (7, _) => fixed.extend_from_slice(&[0, 0]),
            (8, _) => fixed.extend_from_slice(&it.id.to_be_bytes()),
            (9, false) => fixed.extend_from_slice(&(it.data.len() as u32).to_be_bytes()),
            (9, true) => fixed.extend_from_slice(&kids.to_be_bytes()),
            (10, false) => fixed.extend_from_slice(&(it.rsrc.len() as u32).to_be_bytes()),
            _ => fixed.extend_from_slice(&[0, 0, 0, 0]),
        }
    }
    if let Some(at) = name_at {
        let off = fixed.len() as u16;
        fixed[at..at + 2].copy_from_slice(&off.to_be_bytes());
        pascal(&mut fixed, key.last().unwrap());
    }
    fixed
}

fn children(st: &Store, dir: &Key) -> Vec<Key> {
    let mut v: Vec<Key> = st
        .items
        .keys()
        .filter(|k| k.len() == dir.len() + 1 && k.starts_with(dir))
        .cloned()
        .collect();
    v.sort();
    v
}

fn serve(mut s: TcpStream, st: Arc<Mutex<Store>>) {
    let mut forks: HashMap<u16, (Key, bool)> = HashMap::new();
    let mut volume = Vec::new();
    let mut random = [0u8; 8];
    let mut next_fork = 1u16;
    loop {
        let Ok((h, data)) = dsi::read_message(&mut s) else {
            return;
        };
        let reply = |s: &mut TcpStream, code: i32, body: &[u8]| {
            let r = Header {
                flags: 1,
                command: h.command,
                id: h.id,
                code,
                len: body.len() as u32,
            };
            dsi::write_message(s, r, body).unwrap();
        };
        match h.command {
            dsi::GET_STATUS => {
                reply(&mut s, 0, &status());
                continue;
            }
            dsi::OPEN_SESSION => {
                reply(&mut s, 0, &[0, 4, 0, 0, 0x20, 0]);
                continue;
            }
            dsi::CLOSE_SESSION => {
                st.lock().unwrap().log.push("close session".into());
                return;
            }
            dsi::TICKLE => continue,
            _ => {}
        }
        let mut st = st.lock().unwrap();
        let op = data[0];
        let resolve = |vol: &[u8], parts: Vec<Vec<u8>>| -> Key {
            let mut k = vec![vol.to_vec()];
            k.extend(parts);
            k
        };
        let (code, body): (i32, Vec<u8>) = match op {
            cmd::LOGIN => {
                let vers_len = data[1] as usize;
                let at = 2 + vers_len;
                let uam = data[at + 1..at + 1 + data[at] as usize].to_vec();
                let at = at + 1 + uam.len();
                let user = data[at + 1..at + 1 + data[at] as usize].to_vec();
                match (uam.as_slice(), user.as_slice()) {
                    (b"Randnum Exchange", b"dani") => {
                        random = [7, 1, 9, 9, 7, 0, 4, 2];
                        let mut b = 1u16.to_be_bytes().to_vec();
                        b.extend_from_slice(&random);
                        (err::AUTH_CONTINUE, b)
                    }
                    (b"Cleartxt Passwrd", b"dani") => {
                        let mut at = at + 1 + user.len();
                        if at % 2 == 1 {
                            at += 1;
                        }
                        if &data[at..at + 8] == b"pw\0\0\0\0\0\0" {
                            st.log.push("login Cleartxt Passwrd".into());
                            (0, Vec::new())
                        } else {
                            (err::USER_NOT_AUTH, Vec::new())
                        }
                    }
                    _ => (err::USER_NOT_AUTH, Vec::new()),
                }
            }
            cmd::LOGIN_CONT => {
                if data[4..12] == lc_afp::randnum_answer(b"pw", &random) {
                    st.log.push("login Randnum Exchange".into());
                    (0, Vec::new())
                } else {
                    (err::USER_NOT_AUTH, Vec::new())
                }
            }
            cmd::GET_SRVR_PARMS => {
                let mut b = 0u32.to_be_bytes().to_vec();
                let vols: Vec<Key> = st.items.keys().filter(|k| k.len() == 1).cloned().collect();
                b.push(vols.len() as u8);
                for v in vols {
                    b.push(0);
                    pascal(&mut b, &v[0]);
                }
                (0, b)
            }
            cmd::OPEN_VOL => {
                let n = data[4] as usize;
                volume = data[5..5 + n].to_vec();
                let mut b = (1u16 << 5).to_be_bytes().to_vec();
                b.extend_from_slice(&3u16.to_be_bytes());
                (0, b)
            }
            cmd::CLOSE_VOL => (0, Vec::new()),
            cmd::LOGOUT => {
                st.log.push("logout".into());
                (0, Vec::new())
            }
            cmd::ENUMERATE => {
                let (fbm, dbm) = (u16_at(&data, 8), u16_at(&data, 10));
                let (count, start) = (u16_at(&data, 12) as usize, u16_at(&data, 14) as usize);
                let (parts, _) = pathname(&data, 18);
                let dir = resolve(&volume, parts);
                let kids = children(&st, &dir);
                if !st.items.contains_key(&dir) || start > kids.len() {
                    (err::OBJECT_NOT_FOUND, Vec::new())
                } else {
                    let page: Vec<&Key> = kids.iter().skip(start - 1).take(count).collect();
                    let mut b = fbm.to_be_bytes().to_vec();
                    b.extend_from_slice(&dbm.to_be_bytes());
                    b.extend_from_slice(&(page.len() as u16).to_be_bytes());
                    for k in page {
                        let it = &st.items[k];
                        let n = children(&st, k).len() as u16;
                        let p = params(k, it, if it.dir { dbm } else { fbm }, n);
                        let mut len = p.len() + 2;
                        if len % 2 == 1 {
                            len += 1;
                        }
                        b.push(len as u8);
                        b.push(if it.dir { 0x80 } else { 0 });
                        b.extend_from_slice(&p);
                        if (p.len() + 2) % 2 == 1 {
                            b.push(0);
                        }
                    }
                    (0, b)
                }
            }
            cmd::GET_FILE_DIR_PARMS => {
                let (fbm, dbm) = (u16_at(&data, 8), u16_at(&data, 10));
                let (parts, _) = pathname(&data, 12);
                let k = resolve(&volume, parts);
                match st.items.get(&k) {
                    Some(it) => {
                        let n = children(&st, &k).len() as u16;
                        let mut b = fbm.to_be_bytes().to_vec();
                        b.extend_from_slice(&dbm.to_be_bytes());
                        b.push(if it.dir { 0x80 } else { 0 });
                        b.push(0);
                        b.extend_from_slice(&params(&k, it, if it.dir { dbm } else { fbm }, n));
                        (0, b)
                    }
                    None => (err::OBJECT_NOT_FOUND, Vec::new()),
                }
            }
            cmd::SET_FILE_DIR_PARMS => {
                let (parts, mut at) = pathname(&data, 10);
                if at % 2 == 1 {
                    at += 1;
                }
                let k = resolve(&volume, parts);
                match st.items.get_mut(&k) {
                    Some(it) => {
                        it.finfo.copy_from_slice(&data[at..at + 32]);
                        (0, Vec::new())
                    }
                    None => (err::OBJECT_NOT_FOUND, Vec::new()),
                }
            }
            cmd::OPEN_FORK => {
                let rsrc = data[1] & 0x80 != 0;
                let (parts, _) = pathname(&data, 12);
                let k = resolve(&volume, parts);
                match st.items.get(&k) {
                    Some(it) if !it.dir => {
                        let r = next_fork;
                        next_fork += 1;
                        forks.insert(r, (k, rsrc));
                        let mut b = 0u16.to_be_bytes().to_vec();
                        b.extend_from_slice(&r.to_be_bytes());
                        (0, b)
                    }
                    Some(_) => (err::ACCESS_DENIED, Vec::new()),
                    None => (err::OBJECT_NOT_FOUND, Vec::new()),
                }
            }
            cmd::CLOSE_FORK => {
                forks.remove(&u16_at(&data, 2));
                (0, Vec::new())
            }
            cmd::READ => {
                let (k, rsrc) = forks[&u16_at(&data, 2)].clone();
                let (at, n) = (u32_at(&data, 4) as usize, u32_at(&data, 8) as usize);
                let it = &st.items[&k];
                let all = if rsrc { &it.rsrc } else { &it.data };
                let from = at.min(all.len());
                let to = (at + n).min(all.len());
                (
                    if to == all.len() { err::EOF } else { 0 },
                    all[from..to].to_vec(),
                )
            }
            cmd::WRITE => {
                let (k, rsrc) = forks[&u16_at(&data, 2)].clone();
                let at = u32_at(&data, 4) as usize;
                let bytes = &data[12..];
                let it = st.items.get_mut(&k).unwrap();
                let all = if rsrc { &mut it.rsrc } else { &mut it.data };
                all.resize(all.len().max(at + bytes.len()), 0);
                all[at..at + bytes.len()].copy_from_slice(bytes);
                (0, ((at + bytes.len()) as u32).to_be_bytes().to_vec())
            }
            cmd::SET_FORK_PARMS => {
                let (k, rsrc) = forks[&u16_at(&data, 2)].clone();
                let len = u32_at(&data, 6) as usize;
                let it = st.items.get_mut(&k).unwrap();
                if rsrc {
                    it.rsrc.truncate(len);
                } else {
                    it.data.truncate(len);
                }
                (0, Vec::new())
            }
            cmd::CREATE_FILE | cmd::CREATE_DIR => {
                let (parts, _) = pathname(&data, 8);
                let k = resolve(&volume, parts);
                if st.items.contains_key(&k) {
                    (err::OBJECT_EXISTS, Vec::new())
                } else {
                    st.next_id += 1;
                    let id = st.next_id + 16;
                    st.items.insert(
                        k,
                        Item {
                            dir: op == cmd::CREATE_DIR,
                            id,
                            ..Item::default()
                        },
                    );
                    (
                        0,
                        if op == cmd::CREATE_DIR {
                            id.to_be_bytes().to_vec()
                        } else {
                            Vec::new()
                        },
                    )
                }
            }
            cmd::DELETE => {
                let (parts, _) = pathname(&data, 8);
                let k = resolve(&volume, parts);
                if !children(&st, &k).is_empty() {
                    (err::DIR_NOT_EMPTY, Vec::new())
                } else if st.items.remove(&k).is_some() {
                    (0, Vec::new())
                } else {
                    (err::OBJECT_NOT_FOUND, Vec::new())
                }
            }
            cmd::RENAME | cmd::MOVE_AND_RENAME => {
                let (src, dst_dir, name) = if op == cmd::RENAME {
                    let (src, at) = pathname(&data, 8);
                    let (name, _) = pathname(&data, at);
                    let mut dir = src.clone();
                    dir.pop();
                    (src, dir, name)
                } else {
                    let (src, at) = pathname(&data, 12);
                    let (dir, at) = pathname(&data, at);
                    let (name, _) = pathname(&data, at);
                    (src, dir, name)
                };
                let from = resolve(&volume, src);
                let mut to = resolve(&volume, dst_dir);
                to.push(name.concat());
                match st.items.remove(&from) {
                    Some(it) => {
                        st.items.insert(to, it);
                        (0, Vec::new())
                    }
                    None => (err::OBJECT_NOT_FOUND, Vec::new()),
                }
            }
            _ => (err::PARAM, Vec::new()),
        };
        drop(st);
        reply(&mut s, code, &body);
        let _ = s.flush();
    }
}
