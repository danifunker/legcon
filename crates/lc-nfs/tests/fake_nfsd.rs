//! NFS v2 and v3 against a pretend server on localhost (UDP).

mod support;

use std::io::{Read, Write};
use std::time::Duration;

use lc_core::{Charset, Connection, Error, FileSession, PathStyle, RemotePath, WriteMode};
use support::fake;

fn rp(p: &str) -> RemotePath {
    RemotePath::parse(PathStyle::Unix, p)
}

fn open(port: u16, version: Option<u32>) -> lc_nfs::NfsSession {
    let opts = lc_nfs::Options {
        charset: Charset::Utf8,
        uid: 100,
        gid: 10,
        version,
        portmap_port: port,
        timeout: Duration::from_secs(3),
        ..lc_nfs::Options::default()
    };
    lc_nfs::connect("127.0.0.1", &opts).unwrap()
}

fn names(s: &mut lc_nfs::NfsSession, p: &str) -> Vec<String> {
    let mut v: Vec<String> = s
        .list(&rp(p))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    v.sort();
    v
}

fn exercise(s: &mut lc_nfs::NfsSession) {
    assert_eq!(s.exports(), ["/export/home", "/usr/local"]);
    assert_eq!(s.home().unwrap().to_string(), "/");
    // The root and /export are made up from the export paths.
    assert_eq!(names(s, "/"), ["export", "usr"]);
    assert_eq!(names(s, "/export"), ["home"]);
    assert_eq!(
        names(s, "/export/home/dani"),
        [".profile", "caf\u{e9}.txt", "src"]
    );
    let e = s.list(&rp("/export/home/dani")).unwrap();
    let profile = e.iter().find(|e| e.name == ".profile").unwrap();
    assert_eq!(profile.size, Some(19));
    assert_eq!(profile.mode, Some(0o644));
    assert_eq!(profile.owner.as_deref(), Some("100"));
    assert!(e.iter().any(|e| e.name == "src" && e.is_folder()));

    let mut text = String::new();
    s.open_read(&rp("/export/home/dani/.profile"), 5)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "/usr/bin:/bin\n");

    let body: Vec<u8> = (0..50_000u32).map(|i| (i % 253) as u8).collect();
    let f = rp("/export/home/dani/src/blob.bin");
    s.open_write(&f, WriteMode::Overwrite)
        .unwrap()
        .write_all(&body)
        .unwrap();
    assert_eq!(s.stat(&f).unwrap().size, Some(50_000));
    let mut back = Vec::new();
    s.open_read(&f, 0).unwrap().read_to_end(&mut back).unwrap();
    assert!(back == body, "read back differs ({} bytes)", back.len());
    // Overwriting with less cuts the old tail.
    s.open_write(&f, WriteMode::Overwrite)
        .unwrap()
        .write_all(b"short")
        .unwrap();
    assert_eq!(s.stat(&f).unwrap().size, Some(5));

    s.mkdir(&rp("/export/home/dani/old")).unwrap();
    s.rename(&f, &rp("/export/home/dani/old/blob.bin")).unwrap();
    assert_eq!(names(s, "/export/home/dani/old"), ["blob.bin"]);
    s.remove(&rp("/export/home/dani/old/blob.bin")).unwrap();
    s.remove(&rp("/export/home/dani/old")).unwrap();
    assert!(matches!(
        s.stat(&rp("/export/home/dani/old")),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(s.list(&rp("/opt")), Err(Error::NotFound(_))));
    assert_eq!(names(s, "/usr/local/bin"), ["gcc"]);
}

#[test]
fn version_3_browses_reads_writes_and_unmounts() {
    let f = fake::start(fake::sample_fs(), true);
    let mut s = open(f.port, None);
    assert!(
        s.describe().starts_with("NFSv3 over UDP"),
        "{}",
        s.describe()
    );
    exercise(&mut s);
    s.disconnect(Duration::from_secs(1)).unwrap();
    let mut un = f.unmounted.lock().unwrap().clone();
    un.sort();
    assert_eq!(un, ["/export/home", "/usr/local"]);
    assert!(f.uids.lock().unwrap().iter().all(|&u| u == 100));
}

#[test]
fn a_v2_only_server_gets_version_2() {
    let f = fake::start(fake::sample_fs(), false);
    let mut s = open(f.port, None);
    assert!(
        s.describe().starts_with("NFSv2 over UDP"),
        "{}",
        s.describe()
    );
    exercise(&mut s);
    s.disconnect(Duration::from_secs(1)).unwrap();
}

#[test]
fn asking_for_v3_from_a_v2_server_says_so() {
    let f = fake::start(fake::sample_fs(), false);
    let opts = lc_nfs::Options {
        version: Some(3),
        tcp: Some(false),
        portmap_port: f.port,
        timeout: Duration::from_secs(2),
        ..lc_nfs::Options::default()
    };
    assert!(lc_nfs::connect("127.0.0.1", &opts).is_err());
}
