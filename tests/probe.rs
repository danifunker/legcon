//! The probe against pretend servers on localhost.

#[path = "../crates/lc-afp/tests/support/fake.rs"]
mod fake;

use std::io::Write;
use std::net::TcpListener;
use std::thread;

use legcon::model::machines::ServiceKind;
use legcon::model::probe::{probe_ports, Ports};

/// A port that says `hello` to whoever connects, then closes.
fn greeter(hello: &'static [u8]) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        for s in l.incoming() {
            let _ = s.unwrap().write_all(hello);
        }
    });
    port
}

/// A port nothing listens on.
fn closed() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[test]
fn finds_what_answers_and_guesses_the_system() {
    let afp = fake::start(fake::sample());
    let ftp = greeter(b"220 sun FTP server (SunOS 5.8) ready.\r\n");
    let ssh = greeter(b"SSH-1.99-OpenSSH_2.9p2\r\n");
    let rsh = greeter(b"");
    let c = closed();
    let ports = Ports {
        ftp,
        ssh,
        telnet: c,
        gopher: c,
        http: c,
        smb: [c, c],
        portmap: c,
        rexec: c,
        rlogin: c,
        rsh,
        afp: afp.port,
    };
    let r = probe_ports("127.0.0.1", &ports, None).unwrap();
    let kinds: Vec<_> = r.services().into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        kinds,
        [
            ServiceKind::Afp,
            ServiceKind::Ssh,
            ServiceKind::Ftp,
            ServiceKind::Rsh
        ]
    );
    let lines = r.lines().join("\n");
    assert!(
        lines.contains("sun FTP server (SunOS 5.8) ready."),
        "{lines}"
    );
    assert!(
        lines.contains("Pretend Power Mac (Macintosh), AFP2.2"),
        "{lines}"
    );
    assert!(lines.contains("SSH-1.99-OpenSSH_2.9p2"), "{lines}");
    // An AFP server means a Mac, and Mac names are MacRoman.
    assert_eq!(r.system.as_deref(), Some("Mac OS"));
    assert_eq!(r.charset, "MacRoman");
}
