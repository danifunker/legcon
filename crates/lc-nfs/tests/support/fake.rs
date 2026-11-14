//! A pretend NFS server on one UDP port: portmapper, MOUNT v1 / v3 and NFS v2 / v3, backed by
//! `MemoryFs`. Handles are 32 bytes: an index into a path table. Used by the tests and the
//! `fake_nfsd` example.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;

use lc_core::memory::MemoryFs;
use lc_core::{Entry, Error, FileSession, PathStyle, RemotePath, WriteMode};
use lc_nfs::xdr::{Dec, Enc};

pub struct Fake {
    pub port: u16,
    /// Exports unmounted so far, in order.
    pub unmounted: Arc<Mutex<Vec<String>>>,
    /// The uid of every NFS call seen.
    pub uids: Arc<Mutex<Vec<u32>>>,
}

pub const EXPORTS: [&str; 2] = ["/export/home", "/usr/local"];

pub fn sample_fs() -> MemoryFs {
    let fs = MemoryFs::new(PathStyle::Unix, "/export/home");
    fs.add_file("/export/home/dani/.profile", b"PATH=/usr/bin:/bin\n");
    fs.add_file("/export/home/dani/caf\u{e9}.txt", b"latin-1 name\n");
    fs.add_folder("/export/home/dani/src");
    fs.add_file("/usr/local/bin/gcc", &[0x7f, b'E', b'L', b'F']);
    fs
}

struct State {
    fs: MemoryFs,
    paths: Vec<String>,
    v3: bool,
}

impl State {
    fn handle(&mut self, path: &str) -> [u8; 32] {
        let i = match self.paths.iter().position(|p| p == path) {
            Some(i) => i,
            None => {
                self.paths.push(path.to_string());
                self.paths.len() - 1
            }
        };
        let mut h = [0u8; 32];
        h[..4].copy_from_slice(&(i as u32).to_be_bytes());
        h[4] = 0xfe;
        h
    }

    fn path_of(&self, fh: &[u8]) -> Option<String> {
        let i = u32::from_be_bytes(fh.get(..4)?.try_into().ok()?) as usize;
        self.paths.get(i).cloned()
    }

    fn fh(&self, d: &mut Dec) -> Vec<u8> {
        if self.v3 {
            d.opaque().unwrap().to_vec()
        } else {
            d.fixed(32).unwrap().to_vec()
        }
    }

    fn put_fh(&self, e: &mut Enc, h: &[u8]) {
        if self.v3 {
            e.opaque(h);
        } else {
            e.fixed(h);
        }
    }

    fn attr(&self, e: &mut Enc, ent: &Entry) {
        let dir = ent.is_folder();
        let (ftype, mode) = if dir { (2, 0o40755) } else { (1, 0o100644) };
        let size = ent.size.unwrap_or(if dir { 512 } else { 0 });
        e.u32(ftype).u32(mode).u32(1).u32(100).u32(10);
        if self.v3 {
            e.u64(size).u64(size).u32(0).u32(0).u64(1).u64(7);
            for t in [1_000_000, 879_000_000, 879_000_000] {
                e.u32(t).u32(0);
            }
        } else {
            e.u32(size as u32).u32(512).u32(0).u32(1).u32(1).u32(7);
            for t in [1_000_000, 879_000_000, 879_000_000] {
                e.u32(t).u32(0);
            }
        }
    }

    fn post_attr(&self, e: &mut Enc, path: &str) {
        match self.fs.clone().stat(&rp(path)) {
            Ok(ent) => {
                e.bool(true);
                self.attr(e, &ent);
            }
            Err(_) => {
                e.bool(false);
            }
        }
    }

    fn read_all(&self, path: &str) -> Vec<u8> {
        let mut v = Vec::new();
        let mut fs = self.fs.clone();
        fs.open_read(&rp(path), 0)
            .unwrap()
            .read_to_end(&mut v)
            .unwrap();
        v
    }

    fn write_all(&self, path: &str, data: &[u8]) {
        let mut fs = self.fs.clone();
        fs.open_write(&rp(path), WriteMode::Overwrite)
            .unwrap()
            .write_all(data)
            .unwrap();
    }
}

fn rp(p: &str) -> RemotePath {
    RemotePath::parse(PathStyle::Unix, p)
}

fn join(dir: &str, name: &str) -> String {
    rp(dir).join(name).to_string()
}

fn status(e: Error) -> u32 {
    match e {
        Error::NotFound(_) => 2,
        Error::Denied(_) => 13,
        _ => 5,
    }
}

/// Start the server; `v3` false makes it a v2-only server (as SunOS 4 or IRIX 5 were).
pub fn start(fs: MemoryFs, v3: bool) -> Fake {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    let unmounted = Arc::new(Mutex::new(Vec::new()));
    let uids = Arc::new(Mutex::new(Vec::new()));
    let (un, ids) = (Arc::clone(&unmounted), Arc::clone(&uids));
    thread::spawn(move || {
        let mut st = State {
            fs,
            paths: Vec::new(),
            v3: false,
        };
        let mut buf = vec![0u8; 65536];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                return;
            };
            let mut d = Dec::new(&buf[..n]);
            let xid = d.u32().unwrap();
            d.u32().unwrap(); // CALL
            d.u32().unwrap(); // rpcvers
            let (prog, vers, procedure) = (d.u32().unwrap(), d.u32().unwrap(), d.u32().unwrap());
            let flavor = d.u32().unwrap();
            let cred = d.opaque().unwrap().to_vec();
            d.u32().unwrap();
            d.opaque().unwrap();
            if flavor == 1 && prog == 100003 {
                let mut c = Dec::new(&cred);
                c.u32().unwrap();
                c.opaque().unwrap();
                ids.lock().unwrap().push(c.u32().unwrap());
            }
            st.v3 = vers == 3;
            let mut r = Enc::new();
            r.u32(xid).u32(1).u32(0).u32(0).u32(0).u32(0);
            match (prog, procedure) {
                (100000, 3) => {
                    let (p, v, prot) = (d.u32().unwrap(), d.u32().unwrap(), d.u32().unwrap());
                    let ok = prot == 17
                        && match p {
                            100003 => v == 2 || (v == 3 && v3),
                            100005 => v == 1 || (v == 3 && v3),
                            _ => false,
                        };
                    r.u32(if ok { u32::from(port) } else { 0 });
                }
                (100005, 1) => {
                    let p = String::from_utf8_lossy(d.opaque().unwrap()).into_owned();
                    if EXPORTS.contains(&p.as_str()) {
                        let h = st.handle(&p);
                        r.u32(0);
                        st.put_fh(&mut r, &h);
                        if st.v3 {
                            r.u32(1).u32(1);
                        }
                    } else {
                        r.u32(13);
                    }
                }
                (100005, 3) => {
                    un.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(d.opaque().unwrap()).into_owned());
                }
                (100005, 5) => {
                    for x in EXPORTS {
                        r.bool(true).opaque(x.as_bytes()).bool(false);
                    }
                    r.bool(false);
                }
                (100003, p) => nfs(&mut st, p, &mut d, &mut r),
                _ => {
                    r.0.truncate(r.0.len() - 4);
                    r.u32(3);
                }
            }
            let _ = sock.send_to(&r.0, from);
        }
    });
    Fake {
        port,
        unmounted,
        uids,
    }
}

fn nfs(st: &mut State, procedure: u32, d: &mut Dec, r: &mut Enc) {
    let v3 = st.v3;
    let fh = st.fh(d);
    let Some(path) = st.path_of(&fh) else {
        r.u32(70);
        return;
    };
    let mut fs = st.fs.clone();
    // Procedure numbers differ between the versions; map v2's onto names.
    let op = match (v3, procedure) {
        (_, 1) => "getattr",
        (_, 2) => "setattr",
        (true, 3) | (false, 4) => "lookup",
        (_, 6) => "read",
        (true, 7) | (false, 8) => "write",
        (true, 8) | (false, 9) => "create",
        (true, 9) | (false, 14) => "mkdir",
        (true, 12) | (false, 10) => "remove",
        (true, 13) | (false, 15) => "rmdir",
        (true, 14) | (false, 11) => "rename",
        (_, 16) => "readdir",
        (true, 17) => "readdirplus",
        _ => "",
    };
    match op {
        "getattr" => match fs.stat(&rp(&path)) {
            Ok(e) => {
                r.u32(0);
                st.attr(r, &e);
            }
            Err(e) => {
                r.u32(status(e));
            }
        },
        "setattr" => {
            // Only "size 0" is used: truncate.
            st.write_all(&path, b"");
            r.u32(0);
            if v3 {
                r.bool(false);
                st.post_attr(r, &path);
            } else {
                let e = fs.stat(&rp(&path)).unwrap();
                st.attr(r, &e);
            }
        }
        "lookup" => {
            let name = String::from_utf8_lossy(d.opaque().unwrap()).into_owned();
            let full = join(&path, &name);
            match fs.stat(&rp(&full)) {
                Ok(e) => {
                    let h = st.handle(&full);
                    r.u32(0);
                    st.put_fh(r, &h);
                    if v3 {
                        r.bool(true);
                        st.attr(r, &e);
                        st.post_attr(r, &path);
                    } else {
                        st.attr(r, &e);
                    }
                }
                Err(e) => {
                    r.u32(status(e));
                    if v3 {
                        st.post_attr(r, &path);
                    }
                }
            }
        }
        "read" => {
            let (at, count) = if v3 {
                (d.u64().unwrap(), d.u32().unwrap())
            } else {
                (u64::from(d.u32().unwrap()), d.u32().unwrap())
            };
            let all = st.read_all(&path);
            let from = (at as usize).min(all.len());
            let to = (from + count as usize).min(all.len());
            r.u32(0);
            if v3 {
                st.post_attr(r, &path);
                r.u32((to - from) as u32).bool(to == all.len());
            } else {
                let e = fs.stat(&rp(&path)).unwrap();
                st.attr(r, &e);
            }
            r.opaque(&all[from..to]);
        }
        "write" => {
            let at = if v3 {
                let at = d.u64().unwrap();
                d.u32().unwrap();
                d.u32().unwrap();
                at
            } else {
                d.u32().unwrap();
                let at = d.u32().unwrap();
                d.u32().unwrap();
                u64::from(at)
            } as usize;
            let data = d.opaque().unwrap().to_vec();
            let mut all = st.read_all(&path);
            all.resize(all.len().max(at + data.len()), 0);
            all[at..at + data.len()].copy_from_slice(&data);
            st.write_all(&path, &all);
            r.u32(0);
            if v3 {
                r.bool(false);
                st.post_attr(r, &path);
                r.u32(data.len() as u32).u32(2).fixed(&[0; 8]);
            } else {
                let e = fs.stat(&rp(&path)).unwrap();
                st.attr(r, &e);
            }
        }
        "create" | "mkdir" => {
            let name = String::from_utf8_lossy(d.opaque().unwrap()).into_owned();
            let full = join(&path, &name);
            let res = if op == "mkdir" {
                fs.mkdir(&rp(&full))
            } else {
                fs.open_write(&rp(&full), WriteMode::Overwrite).map(drop)
            };
            match res {
                Ok(()) => {
                    let h = st.handle(&full);
                    let e = fs.stat(&rp(&full)).unwrap();
                    r.u32(0);
                    if v3 {
                        r.bool(true);
                        st.put_fh(r, &h);
                        r.bool(true);
                        st.attr(r, &e);
                        r.bool(false).bool(false);
                    } else {
                        st.put_fh(r, &h);
                        st.attr(r, &e);
                    }
                }
                Err(e) => {
                    r.u32(status(e));
                    if v3 {
                        r.bool(false).bool(false);
                    }
                }
            }
        }
        "remove" | "rmdir" => {
            let name = String::from_utf8_lossy(d.opaque().unwrap()).into_owned();
            match fs.remove(&rp(&join(&path, &name))) {
                Ok(()) => r.u32(0),
                Err(e) => r.u32(status(e)),
            };
            if v3 {
                r.bool(false).bool(false);
            }
        }
        "rename" => {
            let name = String::from_utf8_lossy(d.opaque().unwrap()).into_owned();
            let to_fh = st.fh(d);
            let to_name = String::from_utf8_lossy(d.opaque().unwrap()).into_owned();
            let to_dir = st.path_of(&to_fh).unwrap();
            match fs.rename(&rp(&join(&path, &name)), &rp(&join(&to_dir, &to_name))) {
                Ok(()) => r.u32(0),
                Err(e) => r.u32(status(e)),
            };
            if v3 {
                r.bool(false).bool(false).bool(false).bool(false);
            }
        }
        "readdir" | "readdirplus" => {
            let list = fs.list(&rp(&path)).unwrap();
            r.u32(0);
            if v3 {
                st.post_attr(r, &path);
                r.fixed(&[0; 8]);
            }
            let names: Vec<(Vec<u8>, Entry)> = [
                (b".".to_vec(), Entry::new(".", lc_core::EntryKind::Folder)),
                (b"..".to_vec(), Entry::new("..", lc_core::EntryKind::Folder)),
            ]
            .into_iter()
            .chain(list.into_iter().map(|e| (e.raw_name.clone(), e)))
            .collect();
            for (i, (raw, e)) in names.iter().enumerate() {
                r.bool(true);
                if v3 {
                    r.u64(i as u64 + 10).opaque(raw).u64(i as u64 + 1);
                    if op == "readdirplus" {
                        r.bool(true);
                        st.attr(r, e);
                        let h = st.handle(&join(&path, &String::from_utf8_lossy(raw)));
                        r.bool(true);
                        st.put_fh(r, &h);
                    }
                } else {
                    r.u32(i as u32 + 10)
                        .opaque(raw)
                        .fixed(&(i as u32 + 1).to_be_bytes());
                }
            }
            r.bool(false).bool(true);
        }
        _ => {
            r.u32(10004);
        }
    }
}
