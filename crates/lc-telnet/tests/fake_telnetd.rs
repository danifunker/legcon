//! A pretend 1990s `telnetd` on localhost: negotiation, a login prompt, then hang up.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use lc_telnet::nvt::{DO, ECHO, IAC, SB, SE, TTYPE, WILL};

fn read_until(r: &mut dyn Read, want: &[u8], secs: u64) -> Vec<u8> {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    while !got.windows(want.len()).any(|w| w == want) {
        assert!(
            Instant::now() < end,
            "timed out waiting for {want:?}; got {got:?}"
        );
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => panic!("{e}"),
        }
    }
    got
}

#[test]
fn login_prompt_terminal_type_and_hang_up() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(&[IAC, WILL, ECHO, IAC, DO, TTYPE, IAC, SB, TTYPE, 1, IAC, SE])
            .unwrap();
        s.write_all(b"\r\nSunOS 5.8\r\n\r\nlogin: ").unwrap();
        let mut ttype = b"\xff\xfa\x18\x00VT100".to_vec();
        ttype.extend([IAC, SE]);
        let heard = read_until(&mut s, &ttype, 5);
        assert!(heard.windows(3).any(|w| w == [IAC, DO, ECHO]), "{heard:?}");
        let typed = read_until(&mut s, b"dani\r\n", 5);
        assert!(typed.ends_with(b"dani\r\n"));
        s.write_all(b"dani\r\nPassword:").unwrap();
    });
    let pty = lc_telnet::connect("127.0.0.1", port, &lc_telnet::Options::default()).unwrap();
    let (mut reader, mut writer) = (pty.reader, pty.writer);
    let shown = read_until(&mut reader, b"login: ", 5);
    assert!(
        !shown.contains(&IAC),
        "negotiation never reaches the screen"
    );
    writer.send(b"dani\r").unwrap();
    let shown = read_until(&mut reader, b"Password:", 5);
    assert!(shown.starts_with(b"dani"));
    server.join().unwrap();
    let mut buf = [0u8; 16];
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                assert!(Instant::now() < end, "no end of session");
            }
            Err(_) => break,
        }
    }
    writer.close().unwrap();
}
