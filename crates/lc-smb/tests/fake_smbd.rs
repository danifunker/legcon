//! The SMB client against pretend NT 4 and Windows 95 servers (`support/fake.rs`).

#[path = "support/fake.rs"]
mod fake;

use std::io::{Read, Write};
use std::time::Duration;

use fake::{start, Kind, PASSWORD};
use lc_core::{Connection, FileSession, PathStyle, RemotePath, WriteMode};

fn exercise(port: u16, user: &str) -> lc_smb::SmbSession {
    let opts = lc_smb::Options {
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let mut s = lc_smb::connect("127.0.0.1", port, user, PASSWORD, &opts).unwrap();
    let root = s.home().unwrap();
    let shares: Vec<String> = s.list(&root).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(shares, ["C"], "printers and IPC$ are left out");
    let c = root.join("C");
    let names: Vec<String> = s.list(&c).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        [
            "AUTOEXEC.BAT",
            "CONFIG.SYS",
            "R\u{e9}sum\u{e9}.txt",
            "WINDOWS"
        ]
    );
    let mut text = String::new();
    s.open_read(&c.join("WINDOWS").join("WIN.INI"), 0)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "[windows]\r\nload=\r\n");
    {
        let mut w = s
            .open_write(&c.join("NEW.TXT"), WriteMode::Overwrite)
            .unwrap();
        w.write_all(&vec![b'x'; 10_000]).unwrap();
    }
    assert_eq!(s.stat(&c.join("new.txt")).unwrap().size, Some(10_000));
    s.mkdir(&c.join("GAMES")).unwrap();
    s.rename(&c.join("NEW.TXT"), &c.join("GAMES").join("DOOM.TXT"))
        .unwrap();
    s.remove(&c.join("GAMES").join("DOOM.TXT")).unwrap();
    s.remove(&c.join("GAMES")).unwrap();
    s
}

#[test]
fn nt4_user_level_with_ntlm() {
    let (port, disk, t) = start(Kind::Nt4);
    let mut s = exercise(port, "dani");
    assert!(s.negotiated().is_nt() && s.negotiated().user_level);
    s.disconnect(Duration::from_secs(2)).unwrap();
    t.join().unwrap();
    let seen = disk.lock().unwrap().seen.clone();
    assert!(seen.contains(&"login dani".to_string()));
    assert!(
        seen.iter().any(|l| l.starts_with("bye 0x71")),
        "trees disconnected: {seen:?}"
    );
    assert!(
        seen.last().unwrap().starts_with("bye 0x74"),
        "logged off last: {seen:?}"
    );
}

#[test]
fn nt4_refuses_a_wrong_password() {
    let (port, _disk, _t) = start(Kind::Nt4);
    let opts = lc_smb::Options {
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let r = lc_smb::connect("127.0.0.1", port, "dani", "wrong", &opts);
    assert!(matches!(r, Err(lc_core::Error::Auth(_))), "{:?}", r.err());
}

#[test]
fn win95_share_level_with_its_own_name() {
    let (port, disk, t) = start(Kind::Win95);
    let mut s = exercise(port, "");
    assert_eq!(s.negotiated().dialect, "LANMAN2.1");
    assert!(!s.negotiated().user_level);
    assert_eq!(
        s.server(),
        "127",
        "fell back to the host's first label after *SMBSERVER was refused"
    );
    s.disconnect(Duration::from_secs(2)).unwrap();
    t.join().unwrap();
    assert!(disk
        .lock()
        .unwrap()
        .seen
        .iter()
        .any(|l| l == "tree \\\\127\\C"));
    let _ = RemotePath::root(PathStyle::Dos);
}
