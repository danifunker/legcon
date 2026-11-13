//! rexec against a pretend rexecd on localhost.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use lc_core::Error;
use lc_rcmd::{Options, Rexec};

fn field(s: &mut impl Read) -> Vec<u8> {
    let mut v = Vec::new();
    let mut b = [0u8; 1];
    while s.read(&mut b).unwrap() == 1 && b[0] != 0 {
        v.push(b[0]);
    }
    v
}

/// Serves `n` connections: user `dani` / `pw` may run `echo X` and `true`.
fn rexecd(n: usize) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        for _ in 0..n {
            let (mut s, _) = l.accept().unwrap();
            assert_eq!(field(&mut s), b"0");
            let (user, pw, cmd) = (field(&mut s), field(&mut s), field(&mut s));
            if user != b"dani" || pw != b"pw" {
                s.write_all(b"\x01Login incorrect.\n").unwrap();
                continue;
            }
            s.write_all(b"\0").unwrap();
            if let Some(rest) = cmd.strip_prefix(b"echo ") {
                s.write_all(rest).unwrap();
                s.write_all(b"\n").unwrap();
            }
        }
    });
    port
}

#[test]
fn runs_commands_and_returns_output() {
    let port = rexecd(3);
    let r = Rexec::connect("127.0.0.1", port, b"dani", b"pw", &Options::default()).unwrap();
    assert_eq!(r.run(b"echo hello").unwrap(), b"hello\n");
    assert_eq!(r.run(b"echo caf\xe9").unwrap(), b"caf\xe9\n");
}

#[test]
fn a_wrong_password_is_a_login_error() {
    let port = rexecd(1);
    let e = Rexec::connect("127.0.0.1", port, b"dani", b"nope", &Options::default())
        .err()
        .unwrap();
    assert!(
        matches!(e, Error::Auth(ref m) if m == "Login incorrect."),
        "{e}"
    );
}
