//! Try an SSH-1 terminal by hand: `cargo run -p lc-ssh --example ssh1_try -- HOST USER KEYFILE [COMMAND]`.
//! Logs in with the SSH-1 identity file, types the command and Enter, prints what comes back.

use std::io::Read;
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (host, user, key) = (&a[1], &a[2], &a[3]);
    let cmd = a
        .get(4)
        .cloned()
        .unwrap_or_else(|| "echo hi-from-legcon".into());
    let auth = lc_ssh::Auth {
        password: None,
        key: Some(lc_ssh::KeyFiles {
            private: key.into(),
            public: lc_ssh::keys::public_path(std::path::Path::new(key)),
        }),
        ..Default::default()
    };
    let opts = lc_ssh::Options {
        timeout: Duration::from_secs(30),
        ..lc_ssh::Options::default()
    };
    let conn = lc_ssh::ssh1::Conn::open(host, 22, user, &auth, &opts, &mut |_| Ok(())).unwrap();
    println!("logged in: {:?}", conn.info);
    let mut pty = conn
        .into_pty("vt100", 80, 24, Duration::from_millis(300))
        .unwrap();
    let mut buf = [0u8; 4096];
    let mut got = Vec::new();
    let mut read_for = |secs: u64, got: &mut Vec<u8>| {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            match pty.reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }
        }
    };
    read_for(6, &mut got);
    println!("--- greeting:\n{}", String::from_utf8_lossy(&got));
    got.clear();
    pty.writer.send(cmd.as_bytes()).unwrap();
    pty.writer.send(b"\r").unwrap();
    read_for(6, &mut got);
    println!(
        "--- after the command (raw): {:?}",
        String::from_utf8_lossy(&got)
    );
    pty.writer.send(b"exit\r").unwrap();
    got.clear();
    read_for(4, &mut got);
    println!("--- after exit: {:?}", String::from_utf8_lossy(&got));
    let _ = pty.writer.close();
}
