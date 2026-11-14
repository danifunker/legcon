//! rsh and rlogin against pretend servers on localhost that check the reserved source port.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use lc_core::{Charset, Error, FileSession, PathStyle, RemotePath};
use lc_rcmd::rsh::{self, Rsh};
use lc_rcmd::Options;

fn field(s: &mut impl Read) -> Vec<u8> {
    let mut v = Vec::new();
    let mut b = [0u8; 1];
    while s.read(&mut b).unwrap() == 1 && b[0] != 0 {
        v.push(b[0]);
    }
    v
}

/// Serves `n` rsh connections; `dani` trusts us, anyone else is refused as rshd does.
fn rshd(n: usize) -> (u16, thread::JoinHandle<Vec<String>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let h = thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..n {
            let (mut s, peer) = l.accept().unwrap();
            if peer.port() >= 1024 {
                s.write_all(b"\x01Permission denied.\n").unwrap();
                continue;
            }
            assert_eq!(field(&mut s), b"0");
            let (local, remote) = (field(&mut s), field(&mut s));
            let cmd = String::from_utf8(field(&mut s)).unwrap();
            seen.push(cmd.clone());
            if local != b"dani" || remote != b"dani" {
                s.write_all(b"\x01Permission denied.\n").unwrap();
                continue;
            }
            s.write_all(b"\0").unwrap();
            let reply: &[u8] = if cmd.contains("pwd") {
                b"/home/dani\n@legcon-status:0\n"
            } else if cmd.contains("mkdir") {
                b"mkdir: src: File exists\n@legcon-status:1\n"
            } else if cmd.contains("ls -laL") {
                b"total 2\ndrwxr-xr-x   2 dani  staff   512 Nov 13  1997 src\n-rw-r--r--   1 dani  staff    12 Nov 13  1997 caf\xe9.txt\n@legcon-status:0\n"
            } else {
                b""
            };
            s.write_all(reply).unwrap();
        }
        seen
    });
    (port, h)
}

fn opts() -> Options {
    Options {
        read_timeout: Duration::from_secs(5),
        ..Options::default()
    }
}

#[test]
fn rsh_from_a_reserved_port_runs_file_commands() {
    let (port, h) = rshd(4);
    let mut r = Rsh::connect("127.0.0.1", port, "dani", "dani", Charset::Latin1, &opts()).unwrap();
    assert_eq!(r.home().unwrap().to_string(), "/home/dani");
    let names: Vec<_> = r
        .list(&RemotePath::parse(PathStyle::Unix, "/home/dani"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["src", "caf\u{e9}.txt"]);
    let e = r
        .mkdir(&RemotePath::parse(PathStyle::Unix, "/home/dani/src"))
        .unwrap_err();
    assert!(e.to_string().contains("File exists"), "{e}");
    let seen = h.join().unwrap();
    assert_eq!(seen[0], "true");
    assert!(
        seen[1].starts_with("exec /bin/sh -c 'LC_ALL=C"),
        "{}",
        seen[1]
    );
}

#[test]
fn an_untrusted_user_is_told_about_rhosts() {
    let (port, _h) = rshd(1);
    let e = Rsh::connect("127.0.0.1", port, "eve", "eve", Charset::Latin1, &opts())
        .err()
        .unwrap();
    assert!(
        matches!(e, Error::Auth(ref m) if m.contains("Permission denied") && m.contains(".rhosts")),
        "{e}"
    );
}

#[test]
fn rlogin_sends_its_request_and_the_window_size() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let h = thread::spawn(move || {
        let (mut s, peer) = l.accept().unwrap();
        assert!(peer.port() < 1024);
        let mut z = [0u8; 1];
        s.read_exact(&mut z).unwrap();
        let req = (field(&mut s), field(&mut s), field(&mut s));
        s.write_all(b"\0SunOS 5.8\r\n$ ").unwrap();
        let mut ws = [0u8; 12];
        s.read_exact(&mut ws).unwrap();
        (z[0], req, ws)
    });
    let mut pty = rsh::rlogin(
        "127.0.0.1",
        port,
        b"dani",
        b"dani",
        "vt100",
        100,
        30,
        Duration::from_millis(200),
        &opts(),
    )
    .unwrap();
    let (z, req, ws) = h.join().unwrap();
    assert_eq!(z, 0);
    assert_eq!(
        req,
        (b"dani".to_vec(), b"dani".to_vec(), b"vt100/38400".to_vec())
    );
    assert_eq!(ws, rsh::window_size(100, 30));
    let mut got = vec![0u8; 64];
    let n = pty.reader.read(&mut got).unwrap();
    assert_eq!(&got[..n], b"SunOS 5.8\r\n$ ");
}
