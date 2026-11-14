//! The SSH-1 client against a pretend SSH 1.2.27 server: key exchange, password and RSA key
//! logins, commands, SCP both ways and a terminal.

use std::io::{Read, Write};
use std::time::Duration;

use lc_core::{FileSession, PathStyle, RemotePath, WriteMode};
use lc_ssh::ssh1::{Conn, Ssh1Files};
use lc_ssh::{Auth, KeyFiles, Options};
use rsa::BigUint;

#[path = "support/ssh1_server.rs"]
mod ssh1_server;
use ssh1_server::start;

fn opts() -> Options {
    Options {
        timeout: Duration::from_secs(10),
        ..Options::default()
    }
}

fn password(p: &str) -> Auth {
    Auth {
        password: Some(p.into()),
        key: None,
    }
}

#[test]
fn files_over_ssh1_with_a_password() {
    let (port, server) = start(None);
    let mut seen = Vec::new();
    let mut f = Ssh1Files::open(
        "127.0.0.1",
        port,
        "dani",
        &password("secret"),
        &opts(),
        &mut |k| {
            seen.push(k.algorithm.clone());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(seen, ["rsa1"]);
    assert_eq!(f.info.banner, "SSH-1.5-1.2.27");
    assert_eq!((f.info.host_bits, f.info.server_bits), (1024, 768));

    let home = f.home().unwrap();
    assert_eq!(home.to_string(), "/home/dani");
    let names: Vec<String> = f.list(&home).unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"hello.txt".to_string()) && names.contains(&"src".to_string()));

    let file = RemotePath::parse(PathStyle::Unix, "/home/dani/hello.txt");
    let mut text = String::new();
    f.open_read(&file, 0)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "hello");

    let up = RemotePath::parse(PathStyle::Unix, "/home/dani/new.txt");
    {
        let mut w = f.open_write_sized(&up, WriteMode::Create, 11).unwrap();
        w.write_all(b"from legcon").unwrap();
    }
    assert_eq!(*server.uploaded.lock().unwrap(), b"from legcon");

    // Commands go through /bin/sh whatever the login shell; failures carry what the shell said.
    let r = f.run(b"false").unwrap();
    assert_eq!(r.exit_status, Some(1));
    assert!(String::from_utf8_lossy(&r.errors).contains("not found"));
    let cmds = server.commands.lock().unwrap();
    assert!(cmds
        .iter()
        .filter(|c| c.contains("pwd") || c.contains("ls -laL"))
        .all(|c| c.starts_with("/bin/sh -c '")));
}

#[test]
fn a_wrong_password_or_refused_host_key_stops_the_login() {
    let (port, _server) = start(None);
    let e = Conn::open(
        "127.0.0.1",
        port,
        "dani",
        &password("guess"),
        &opts(),
        &mut |_| Ok(()),
    )
    .err()
    .unwrap();
    assert!(e.to_string().contains("password refused"), "{e}");
    let e = Conn::open(
        "127.0.0.1",
        port,
        "dani",
        &password("secret"),
        &opts(),
        &mut |_| Err(lc_core::Error::Denied("changed".into())),
    )
    .err()
    .unwrap();
    assert!(e.to_string().contains("changed"));
}

#[test]
fn an_ssh1_key_logs_in_without_a_password() {
    let dir = tempfile::tempdir().unwrap();
    let (identity, line) =
        lc_ssh::keys::generate_ssh1(lc_ssh::keys::KeyKind::Ssh1Rsa1024, "dani@legcon").unwrap();
    let path = dir.path().join("id_ssh1");
    lc_ssh::keys::save_ssh1(&identity, &line, &path).unwrap();
    let (_, n, _) = lc_ssh::keyfile::parse_ssh1_line(&line).unwrap();
    let (port, _server) = start(Some(BigUint::from_bytes_be(&n)));
    let auth = Auth {
        password: None,
        key: Some(KeyFiles {
            public: lc_ssh::keys::public_path(&path),
            private: path,
        }),
    };
    let r = Conn::open("127.0.0.1", port, "dani", &auth, &opts(), &mut |_| Ok(()))
        .unwrap()
        .exec(b"pwd", b"")
        .unwrap();
    assert_eq!(r.output, b"/home/dani\n");
}

#[test]
fn a_terminal_echoes_and_resizes() {
    let (port, _server) = start(None);
    let conn = Conn::open(
        "127.0.0.1",
        port,
        "dani",
        &password("secret"),
        &opts(),
        &mut |_| Ok(()),
    )
    .unwrap();
    let mut pty = conn
        .into_pty("vt100", 80, 24, Duration::from_millis(200))
        .unwrap();
    pty.writer.send(b"hi there").unwrap();
    pty.writer.resize(100, 30).unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 64];
    while !got.ends_with(b"[resized]") {
        match pty.reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(got, b"hi there[resized]");
    pty.writer.send(b"exit\r").unwrap();
    while !matches!(pty.reader.read(&mut buf), Ok(0)) {}
    pty.writer.close().unwrap();
}
