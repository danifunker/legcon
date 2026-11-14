//! AFP against a pretend AppleShare IP server on localhost.

mod support;

use std::io::{Read, Write};
use std::time::Duration;

use lc_core::{Connection, Entry, Error, FileSession, MacExtras, PathStyle, RemotePath, WriteMode};
use support::fake;

fn mp(p: &str) -> RemotePath {
    RemotePath::parse(PathStyle::Mac, p)
}

fn open(port: u16, user: &str, pw: &str) -> lc_afp::AfpSession {
    lc_afp::connect("127.0.0.1", port, user, pw, &lc_afp::Options::default()).unwrap()
}

fn by_name<'a>(v: &'a [Entry], n: &str) -> &'a Entry {
    v.iter()
        .find(|e| e.name == n)
        .unwrap_or_else(|| panic!("no {n} in {v:?}"))
}

#[test]
fn logs_in_without_sending_the_password_and_browses() {
    let f = fake::start(fake::sample());
    let mut s = open(f.port, "dani", "pw");
    assert_eq!(s.uam, "Randnum Exchange");
    assert_eq!(s.version, "AFP2.2");
    assert_eq!(s.info.name, "Pretend Power Mac");
    assert_eq!(s.home().unwrap().to_string(), "Macintosh HD");
    let root = s.list(&RemotePath::root(PathStyle::Mac)).unwrap();
    assert_eq!(root.len(), 1);
    let hd = s.list(&mp("Macintosh HD")).unwrap();
    let names: Vec<_> = hd.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Caf\u{e9}",
            "Read Me",
            "SimpleText\u{2122}",
            "System Folder"
        ]
    );
    let app = by_name(&hd, "SimpleText\u{2122}");
    assert!(app.has_resource_fork);
    assert_eq!(app.mac_type_creator, Some((*b"APPL", *b"ttxt")));
    assert!(by_name(&hd, "Caf\u{e9}").is_folder());
    let mut text = String::new();
    s.open_read(&mp("Macintosh HD:Read Me"), 11)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "Mac OS 9\r");
    let x = s
        .mac_extras(&mp("Macintosh HD:SimpleText\u{2122}"))
        .unwrap()
        .unwrap();
    assert_eq!(x.resource_fork, b"\0\0\x01\0resource fork bytes");
    assert_eq!(x.type_code(), *b"APPL");
    s.disconnect(Duration::from_secs(1)).unwrap();
    // The server notes the DSI close on its own thread, a moment later.
    for _ in 0..50 {
        if f.store.lock().unwrap().log.len() == 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let log = f.store.lock().unwrap().log.clone();
    assert_eq!(log, ["login Randnum Exchange", "logout", "close session"]);
}

#[test]
fn writes_forks_and_moves_things() {
    let f = fake::start(fake::sample());
    let mut s = open(f.port, "dani", "pw");
    let body: Vec<u8> = (0..70_000u32).map(|i| (i % 249) as u8).collect();
    let file = mp("Macintosh HD:System Folder:Note");
    s.open_write(&file, WriteMode::Overwrite)
        .unwrap()
        .write_all(&body)
        .unwrap();
    assert_eq!(s.stat(&file).unwrap().size, Some(70_000));
    s.open_write(&file, WriteMode::Overwrite)
        .unwrap()
        .write_all(b"short")
        .unwrap();
    assert_eq!(s.stat(&file).unwrap().size, Some(5));
    let mut fi = [0u8; 32];
    fi[..8].copy_from_slice(b"TEXTttxt");
    let x = MacExtras {
        finder_info: fi,
        resource_fork: vec![9; 3000],
    };
    s.set_mac_extras(&file, &x).unwrap();
    assert_eq!(s.mac_extras(&file).unwrap(), Some(x));

    s.mkdir(&mp("Macintosh HD:New Folder")).unwrap();
    s.rename(&file, &mp("Macintosh HD:New Folder:Note 2"))
        .unwrap();
    s.rename(
        &mp("Macintosh HD:New Folder:Note 2"),
        &mp("Macintosh HD:New Folder:Note 3"),
    )
    .unwrap();
    let inside: Vec<_> = s
        .list(&mp("Macintosh HD:New Folder"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(inside, ["Note 3"]);
    assert!(matches!(
        s.remove(&mp("Macintosh HD:New Folder")),
        Err(Error::Protocol(ref m)) if m.contains("not empty")
    ));
    s.remove(&mp("Macintosh HD:New Folder:Note 3")).unwrap();
    s.remove(&mp("Macintosh HD:New Folder")).unwrap();
    assert!(matches!(
        s.stat(&mp("Macintosh HD:New Folder")),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn cleartext_and_a_wrong_password() {
    let f = fake::start(fake::sample());
    let e = lc_afp::connect(
        "127.0.0.1",
        f.port,
        "dani",
        "nope",
        &lc_afp::Options::default(),
    )
    .err()
    .unwrap();
    assert!(matches!(e, Error::Auth(_)), "{e}");
    // A password over 8 characters cannot go through Randnum, so Cleartext is used (and refused).
    let e = lc_afp::connect(
        "127.0.0.1",
        f.port,
        "dani",
        "much-too-long",
        &lc_afp::Options::default(),
    )
    .err()
    .unwrap();
    assert!(matches!(e, Error::Auth(_)), "{e}");
    assert!(f.store.lock().unwrap().log.is_empty());
}
