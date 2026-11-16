//! The SFTP v3 client against a pretend server in memory, backed by `MemoryFs`.

use std::io::{self, Read, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use lc_core::memory::MemoryFs;
use lc_core::{Error, FileSession, PathStyle, RemotePath, WriteMode};
use lc_ssh::sftp::{self, flag, op, put_str, put_u32, status, Attrs, Cursor, Sftp};

/// One end of an in-memory pipe pair.
struct End {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
}

impl Read for End {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(v) => self.buf = v,
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Ok(n)
    }
}

impl Write for End {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.tx
            .send(b.to_vec())
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn pipe() -> (End, End) {
    let (a_tx, b_rx) = channel();
    let (b_tx, a_rx) = channel();
    (
        End {
            tx: a_tx,
            rx: a_rx,
            buf: Vec::new(),
        },
        End {
            tx: b_tx,
            rx: b_rx,
            buf: Vec::new(),
        },
    )
}

fn path(b: &[u8]) -> RemotePath {
    RemotePath::parse(PathStyle::Unix, &String::from_utf8_lossy(b))
}

fn attrs_of(e: &lc_core::Entry) -> Attrs {
    Attrs {
        size: e.size,
        permissions: Some(if e.is_folder() { 0o040755 } else { 0o100644 }),
        ..Attrs::default()
    }
}

/// Serves SFTP v3 from `fs` until the client goes away. Handles are paths with a type prefix.
fn serve(mut io: End, mut fs: MemoryFs) {
    let (k, _) = sftp::read_packet(&mut io).unwrap();
    assert_eq!(k, op::INIT);
    let mut v = Vec::new();
    put_u32(&mut v, 3);
    sftp::write_packet(&mut io, op::VERSION, &v).unwrap();
    let mut open_dirs: Vec<Vec<u8>> = Vec::new();
    while let Ok((kind, body)) = sftp::read_packet(&mut io) {
        let mut c = Cursor::new(&body);
        let id = c.u32().unwrap();
        let mut out = Vec::new();
        put_u32(&mut out, id);
        let st = |out: &mut Vec<u8>, code: u32, msg: &str| {
            put_u32(out, code);
            put_str(out, msg.as_bytes());
            put_str(out, b"");
            op::STATUS
        };
        let err = |out: &mut Vec<u8>, e: Error| match e {
            Error::NotFound(m) => st(out, status::NO_SUCH_FILE, &m),
            other => st(out, 4, &other.to_string()),
        };
        let reply = match kind {
            op::REALPATH => {
                let p = c.bytes().unwrap();
                put_u32(&mut out, 1);
                let full: &[u8] = if p == b"." { b"/home/dani" } else { p };
                put_str(&mut out, full);
                put_str(&mut out, full);
                Attrs::default().encode(&mut out);
                op::NAME
            }
            op::STAT | op::LSTAT => match fs.stat(&path(c.bytes().unwrap())) {
                Ok(e) => {
                    attrs_of(&e).encode(&mut out);
                    op::ATTRS
                }
                Err(e) => err(&mut out, e),
            },
            op::OPENDIR => {
                let p = c.bytes().unwrap().to_vec();
                open_dirs.push(p.clone());
                let mut h = b"D".to_vec();
                h.extend_from_slice(&p);
                put_str(&mut out, &h);
                op::HANDLE
            }
            op::READDIR => {
                let h = c.bytes().unwrap();
                let dir = &h[1..];
                if let Some(i) = open_dirs.iter().position(|d| d == dir) {
                    open_dirs.remove(i);
                    let list = fs.list(&path(dir)).unwrap();
                    put_u32(&mut out, list.len() as u32 + 1);
                    put_str(&mut out, b".");
                    put_str(
                        &mut out,
                        b"drwxr-xr-x   2 dani     staff         512 Nov 13 10:04 .",
                    );
                    attrs_of(&lc_core::Entry::new(".", lc_core::EntryKind::Folder))
                        .encode(&mut out);
                    for e in list {
                        put_str(&mut out, &e.raw_name);
                        let long = format!(
                            "-rw-r--r--   1 dani     staff  {:>10} Nov 13 10:04 {}",
                            e.size.unwrap_or(0),
                            e.name
                        );
                        put_str(&mut out, long.as_bytes());
                        attrs_of(&e).encode(&mut out);
                    }
                    op::NAME
                } else {
                    st(&mut out, status::EOF, "")
                }
            }
            op::OPEN => {
                let p = c.bytes().unwrap().to_vec();
                let flags = c.u32().unwrap();
                let r = if flags & flag::WRITE != 0 {
                    fs.open_write(&path(&p), WriteMode::Overwrite).map(drop)
                } else {
                    fs.stat(&path(&p)).map(drop)
                };
                match r {
                    Ok(()) => {
                        let mut h = b"F".to_vec();
                        h.extend_from_slice(&p);
                        put_str(&mut out, &h);
                        op::HANDLE
                    }
                    Err(e) => err(&mut out, e),
                }
            }
            op::READ => {
                let h = c.bytes().unwrap().to_vec();
                let at = c.u64().unwrap();
                let len = c.u32().unwrap() as usize;
                let mut all = Vec::new();
                fs.open_read(&path(&h[1..]), 0)
                    .unwrap()
                    .read_to_end(&mut all)
                    .unwrap();
                if at as usize >= all.len() {
                    st(&mut out, status::EOF, "")
                } else {
                    let end = all.len().min(at as usize + len);
                    put_str(&mut out, &all[at as usize..end]);
                    op::DATA
                }
            }
            op::WRITE => {
                let h = c.bytes().unwrap().to_vec();
                let at = c.u64().unwrap() as usize;
                let data = c.bytes().unwrap();
                let p = path(&h[1..]);
                let mut all = Vec::new();
                fs.open_read(&p, 0).unwrap().read_to_end(&mut all).unwrap();
                all.resize(all.len().max(at + data.len()), 0);
                all[at..at + data.len()].copy_from_slice(data);
                fs.open_write(&p, WriteMode::Overwrite)
                    .unwrap()
                    .write_all(&all)
                    .unwrap();
                st(&mut out, status::OK, "")
            }
            op::CLOSE => st(&mut out, status::OK, ""),
            op::MKDIR => match fs.mkdir(&path(c.bytes().unwrap())) {
                Ok(()) => st(&mut out, status::OK, ""),
                Err(e) => err(&mut out, e),
            },
            op::REMOVE | op::RMDIR => match fs.remove(&path(c.bytes().unwrap())) {
                Ok(()) => st(&mut out, status::OK, ""),
                Err(e) => err(&mut out, e),
            },
            op::RENAME => {
                let (a, b) = (path(c.bytes().unwrap()), path(c.bytes().unwrap()));
                match fs.rename(&a, &b) {
                    Ok(()) => st(&mut out, status::OK, ""),
                    Err(e) => err(&mut out, e),
                }
            }
            _ => st(&mut out, status::OP_UNSUPPORTED, "unsupported"),
        };
        sftp::write_packet(&mut io, reply, &out).unwrap();
    }
}

fn start() -> Sftp<End> {
    let fs = MemoryFs::new(PathStyle::Unix, "/home/dani");
    fs.add_file("/home/dani/readme.txt", b"hello from 1997\n");
    fs.add_file("/home/dani/caf\u{e9}.txt", b"x");
    fs.add_folder("/home/dani/src");
    let (client, server) = pipe();
    thread::spawn(move || serve(server, fs));
    Sftp::start(client).unwrap()
}

#[test]
fn version_home_and_listing() {
    let mut s = start();
    assert_eq!(s.version, 3);
    assert_eq!(s.realpath(b".").unwrap(), b"/home/dani");
    let mut names: Vec<_> = s
        .list(b"/home/dani")
        .unwrap()
        .into_iter()
        .map(|n| {
            (
                String::from_utf8_lossy(&n.name).into_owned(),
                n.attrs.is_dir(),
            )
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ("caf\u{e9}.txt".to_string(), false),
            ("readme.txt".to_string(), false),
            ("src".to_string(), true)
        ]
    );
}

#[test]
fn read_write_rename_remove() {
    let mut s = start();
    let h = s.open(b"/home/dani/readme.txt", flag::READ).unwrap();
    assert_eq!(s.read(&h, 5, 100).unwrap(), b" from 1997\n");
    assert_eq!(s.read(&h, 100, 100).unwrap(), b"");
    s.close(&h).unwrap();

    let body: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let h = s
        .open(
            b"/home/dani/new.bin",
            flag::WRITE | flag::CREAT | flag::TRUNC,
        )
        .unwrap();
    for (i, c) in body.chunks(sftp::CHUNK as usize).enumerate() {
        s.write(&h, (i * sftp::CHUNK as usize) as u64, c).unwrap();
    }
    s.close(&h).unwrap();
    assert_eq!(s.stat(b"/home/dani/new.bin").unwrap().size, Some(70_000));

    s.mkdir(b"/home/dani/old").unwrap();
    s.rename(b"/home/dani/new.bin", b"/home/dani/old/new.bin")
        .unwrap();
    s.remove(b"/home/dani/old/new.bin").unwrap();
    s.rmdir(b"/home/dani/old").unwrap();
    let e = s.stat(b"/home/dani/old").unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "{e}");
}

#[test]
fn requests_in_flight_answer_in_any_order() {
    let mut s = start();
    let body: Vec<u8> = (0..100_000u32).map(|i| (i % 249) as u8).collect();
    let h = s
        .open(b"/home/dani/big.bin", flag::WRITE | flag::CREAT)
        .unwrap();
    let ids: Vec<u32> = body
        .chunks(10_000)
        .enumerate()
        .map(|(i, c)| s.send_write(&h, i as u64 * 10_000, c).unwrap())
        .collect();
    for id in ids {
        s.write_done(id).unwrap();
    }
    s.close(&h).unwrap();

    let h = s.open(b"/home/dani/big.bin", flag::READ).unwrap();
    let ids: Vec<u32> = (0..11)
        .map(|i| s.send_read(&h, i * 10_000, 10_000).unwrap())
        .collect();
    // Collected last first: the earlier replies wait their turn.
    let mut parts: Vec<Vec<u8>> = ids
        .iter()
        .rev()
        .map(|&id| s.read_data(id).unwrap())
        .collect();
    parts.reverse();
    assert!(parts[10].is_empty());
    assert_eq!(parts.concat(), body);
    s.close(&h).unwrap();
}
