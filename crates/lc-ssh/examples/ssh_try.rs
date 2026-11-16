//! Try SSH against a real (or pretend) server, cleaning up after itself:
//! `cargo run -p lc-ssh --example ssh_try -- HOST PORT USER PASSWORD [FOLDER]`.
//! It lists FOLDER (the login folder by default), writes, renames, reads back and deletes
//! `legcon-try.txt` there, runs a command, and opens a terminal for a moment.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use lc_core::{Connection, FileSession, PathStyle, RemotePath, WriteMode};

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (host, port, user, pw) = (&a[0], a[1].parse().unwrap(), &a[2], &a[3]);
    let opts = lc_ssh::Options {
        sftp: std::env::var_os("NO_SFTP").is_none(),
        ..lc_ssh::Options::default()
    };
    let auth = lc_ssh::Auth {
        password: Some(pw.clone()),
        key: None,
        ..Default::default()
    };
    let mut seen = None;
    let t = Instant::now();
    let mut s = lc_ssh::connect(host, port, user, &auth, &opts, &mut |k| {
        seen = Some(k.clone());
        Ok(())
    })
    .expect("connect and log in");
    let k = seen.unwrap();
    println!("host key {} md5 {} {}", k.algorithm, k.md5, k.sha256);
    println!(
        "{:?} weak {} ({:?})",
        s.negotiated(),
        s.negotiated().weak(),
        t.elapsed()
    );
    let dir = match a.get(4) {
        Some(f) => RemotePath::parse(PathStyle::Unix, f),
        None => s.home().expect("home"),
    };
    let names: Vec<_> = s
        .list(&dir)
        .expect("list")
        .into_iter()
        .map(|e| e.name)
        .collect();
    println!("{dir}: {names:?}");
    let file = dir.join("legcon-try.txt");
    let body: Vec<u8> = (0..100_000u32)
        .map(|i| b"legcon SSH test\n"[(i % 16) as usize])
        .collect();
    let t = Instant::now();
    s.open_write_sized(&file, WriteMode::Overwrite, body.len() as u64)
        .expect("open to write")
        .write_all(&body)
        .expect("write");
    println!("wrote {} bytes in {:?}", body.len(), t.elapsed());
    let moved = dir.join("legcon-try-moved.txt");
    s.rename(&file, &moved).expect("rename");
    let mut back = Vec::new();
    s.open_read(&moved, 0)
        .expect("open to read")
        .read_to_end(&mut back)
        .expect("read");
    println!("read back {} bytes, equal {}", back.len(), back == body);
    s.remove(&moved).expect("delete");
    println!("console: {:?}", s.quote(&dir, "pwd").expect("command"));
    s.disconnect(Duration::from_secs(5)).expect("log off");
    println!("file session logged off");

    let s =
        lc_ssh::connect(host, port, user, &auth, &opts, &mut |_| Ok(())).expect("connect again");
    let mut pty = s.into_pty("vt100", 80, 24).expect("terminal");
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    let read_for = |pty: &mut lc_core::Pty, got: &mut Vec<u8>, buf: &mut [u8], ms: u64| {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            match pty.reader.read(buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
        }
    };
    read_for(&mut pty, &mut got, &mut buf, 1000);
    pty.writer.resize(100, 30).expect("resize");
    pty.writer.send(b"size\r").expect("send");
    read_for(&mut pty, &mut got, &mut buf, 1000);
    println!("terminal: {:?}", String::from_utf8_lossy(&got));
    pty.writer.send(b"exit\r").expect("send");
    read_for(&mut pty, &mut got, &mut buf, 500);
    pty.writer.close().expect("close");
    println!("terminal closed");
}
