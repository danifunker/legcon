//! legcon against real FOSS servers in the lab container (lab/README.md), not pretend ones.
//! Start it first (`lab/lab.sh up`), then: cargo test --test lab_unix90s -- --ignored
//! LEGCON_LAB_HOST names the container's host (127.0.0.1); the ports are lab.sh's.
//! LEGCON_LAB_RSH=1 also runs rsh / rlogin, which need the client's reserved port to arrive intact.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use lc_core::{Connection, FileSession, PathStyle, Pty, RemotePath, WriteMode};

const USER: &str = "lab";
const PASSWORD: &str = "lab";

fn host() -> String {
    std::env::var("LEGCON_LAB_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

fn names(fs: &mut dyn FileSession, dir: &RemotePath) -> Vec<String> {
    fs.list(dir)
        .unwrap_or_else(|e| panic!("listing {dir}: {e}"))
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// The same round trip on every writable file service: the fixtures are there; a folder, a file
/// up, its size, the file down byte for byte, a rename and a delete all work; then a clean log-off.
fn exercise(mut fs: Box<dyn FileSession>, style: PathStyle, upload: &[&str], label: &str) {
    let fs = &mut *fs;
    let dir = upload
        .iter()
        .fold(RemotePath::root(style), |p, n| p.join(n));
    let home = dir.parent().expect("upload is inside the home folder");
    let listed = names(fs, &home);
    for want in ["readme.txt", "notes", "folder with spaces"] {
        assert!(
            listed.iter().any(|n| n == want),
            "{label}: {want} not in {listed:?}"
        );
    }
    let work = dir.join(&format!("lc-{label}-{}", std::process::id()));
    fs.mkdir(&work)
        .unwrap_or_else(|e| panic!("{label}: mkdir: {e}"));
    let data: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let file = work.join("hello lab.bin");
    {
        let mut w = fs
            .open_write_sized(&file, WriteMode::Create, data.len() as u64)
            .unwrap_or_else(|e| panic!("{label}: upload: {e}"));
        w.write_all(&data).unwrap();
        w.flush().unwrap();
    }
    let st = fs
        .stat(&file)
        .unwrap_or_else(|e| panic!("{label}: stat: {e}"));
    assert_eq!(st.size, Some(data.len() as u64), "{label}: size");
    let mut back = Vec::new();
    fs.open_read(&file, 0)
        .unwrap_or_else(|e| panic!("{label}: download: {e}"))
        .read_to_end(&mut back)
        .unwrap();
    assert!(back == data, "{label}: the file came back different");
    let renamed = work.join("renamed.bin");
    fs.rename(&file, &renamed)
        .unwrap_or_else(|e| panic!("{label}: rename: {e}"));
    assert_eq!(names(fs, &work), ["renamed.bin"], "{label}: after rename");
    fs.remove(&renamed)
        .unwrap_or_else(|e| panic!("{label}: delete file: {e}"));
    fs.remove(&work)
        .unwrap_or_else(|e| panic!("{label}: delete folder: {e}"));
    assert!(
        !names(fs, &dir).iter().any(|n| n.starts_with("lc-")),
        "{label}: left behind"
    );
    fs.disconnect(Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("{label}: log-off: {e}"));
}

// --- file services ---

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn ftp_vsftpd() {
    let s = lc_ftp::connect(&host(), 10021, USER, PASSWORD, &Default::default()).unwrap();
    exercise(
        Box::new(s),
        PathStyle::Unix,
        &["home", "lab", "upload"],
        "vsftpd",
    );
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn ftp_proftpd() {
    let s = lc_ftp::connect(&host(), 10121, USER, PASSWORD, &Default::default()).unwrap();
    exercise(
        Box::new(s),
        PathStyle::Unix,
        &["home", "lab", "upload"],
        "proftpd",
    );
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn ftp_anonymous_is_read_only() {
    let mut s = lc_ftp::connect(&host(), 10021, "anonymous", "lab@", &Default::default()).unwrap();
    let pub_dir = RemotePath::parse(PathStyle::Unix, "/pub");
    assert!(names(&mut s, &pub_dir).contains(&"README".to_string()));
    assert!(s.mkdir(&pub_dir.join("nope")).is_err());
    s.disconnect(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn ftp_latin1_names() {
    let mut s = lc_ftp::connect(&host(), 10121, USER, PASSWORD, &Default::default()).unwrap();
    let home = RemotePath::parse(PathStyle::Unix, "/home/lab");
    let n = names(&mut s, &home);
    // Byte 0xE9 on disk is e-acute in ISO-8859-1, the default character set.
    assert!(n.contains(&"caf\u{e9}.txt".to_string()), "{n:?}");
    assert!(n.contains(&"Q1?Q2.txt".to_string()), "{n:?}");
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn smb_samba() {
    let s = lc_smb::connect(&host(), 10139, USER, PASSWORD, &Default::default()).unwrap();
    exercise(Box::new(s), PathStyle::Dos, &["lab", "upload"], "samba");
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn sftp_openssh() {
    let s = ssh(10022);
    exercise(
        Box::new(s),
        PathStyle::Unix,
        &["home", "lab", "upload"],
        "openssh",
    );
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn sftp_dropbear() {
    let s = ssh(10222);
    exercise(
        Box::new(s),
        PathStyle::Unix,
        &["home", "lab", "upload"],
        "dropbear",
    );
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn scp_over_ssh1() {
    let s = lc_ssh::ssh1::Ssh1Files::open(
        &host(),
        10200,
        USER,
        &password_auth(),
        &Default::default(),
        &mut |k| {
            assert_eq!(k.algorithm, "rsa1");
            Ok(())
        },
    )
    .unwrap();
    assert!(
        s.info.banner.starts_with("SSH-1.5-OpenSSH_7.3"),
        "{}",
        s.info.banner
    );
    exercise(
        Box::new(s),
        PathStyle::Unix,
        &["home", "lab", "upload"],
        "ssh1",
    );
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn nfs_unfs3() {
    let opts = lc_nfs::Options {
        uid: 1000,
        gid: 1000,
        portmap_port: 10111,
        ..Default::default()
    };
    let s = lc_nfs::connect(&host(), &opts).unwrap();
    exercise(
        Box::new(s),
        PathStyle::Unix,
        &["home", "lab", "upload"],
        "nfs",
    );
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn afp_netatalk() {
    let s = lc_afp::connect(&host(), 10548, USER, PASSWORD, &Default::default()).unwrap();
    exercise(Box::new(s), PathStyle::Mac, &["lab", "upload"], "afp");
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn http_index_pages() {
    let mut s = lc_http::http::connect(&host(), 10080, "", "", &Default::default()).unwrap();
    let root = RemotePath::root(PathStyle::Unix);
    let n = names(&mut s, &root);
    assert!(
        n.contains(&"readme.txt".to_string()) && n.contains(&"notes".to_string()),
        "{n:?}"
    );
    let mut text = String::new();
    s.open_read(&root.join("readme.txt"), 0)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "legcon lab: hello from unix90s\n");
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn gopher_menus() {
    let mut s = lc_http::gopher::connect(&host(), 10070, &Default::default()).unwrap();
    let root = RemotePath::root(PathStyle::Unix);
    let n = names(&mut s, &root);
    assert!(
        n.contains(&"readme.txt".to_string()) && n.contains(&"notes".to_string()),
        "{n:?}"
    );
    let mut text = String::new();
    s.open_read(&root.join("readme.txt"), 0)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert!(
        text.starts_with("legcon lab: hello from unix90s"),
        "{text:?}"
    );
}

// --- commands ---

fn password_auth() -> lc_ssh::Auth {
    lc_ssh::Auth {
        password: Some(PASSWORD.into()),
        key: None,
    }
}

fn ssh(port: u16) -> lc_ssh::SshSession {
    lc_ssh::connect(
        &host(),
        port,
        USER,
        &password_auth(),
        &Default::default(),
        &mut |_| Ok(()),
    )
    .unwrap()
}

/// Read a terminal until `want` shows up (or fail after a while), sending nothing.
fn until(pty: &mut Pty, want: &str) -> String {
    let end = Instant::now() + Duration::from_secs(20);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < end {
        match pty.reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }
        if String::from_utf8_lossy(&got).contains(want) {
            return String::from_utf8_lossy(&got).into_owned();
        }
    }
    panic!(
        "never saw {want:?}; got {:?}",
        String::from_utf8_lossy(&got)
    );
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn telnet_login_and_command() {
    let mut pty = lc_telnet::connect(&host(), 10023, &Default::default()).unwrap();
    until(&mut pty, "login:");
    pty.writer.send(b"lab\r").unwrap();
    until(&mut pty, "assword:");
    pty.writer.send(b"lab\r").unwrap();
    until(&mut pty, "$");
    pty.writer.send(b"echo telnet-$((6*7))\r").unwrap();
    until(&mut pty, "telnet-42");
    pty.writer.send(b"exit\r").unwrap();
    pty.writer.close().unwrap();
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn rexec_command() {
    let r = lc_rcmd::Rexec::connect(&host(), 10512, b"lab", b"lab", &Default::default()).unwrap();
    let out = r.run(b"echo rexec-$((6*7))").unwrap();
    assert!(
        String::from_utf8_lossy(&out).contains("rexec-42"),
        "{out:?}"
    );
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn ssh_exec_and_terminal() {
    for port in [10022, 10222] {
        let mut s = ssh(port);
        let r = s.run(b"echo ssh-$((6*7))").unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r.output),
            "ssh-42\n",
            "port {port}"
        );
        assert_eq!(r.exit_status, Some(0));
        let mut pty = s.into_pty("vt100", 80, 24).unwrap();
        pty.writer.send(b"echo pty-$((6*7))\r").unwrap();
        until(&mut pty, "pty-42");
        pty.writer.close().unwrap();
    }
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn ssh1_exec_and_terminal() {
    let open = || {
        lc_ssh::ssh1::Conn::open(
            &host(),
            10200,
            USER,
            &password_auth(),
            &Default::default(),
            &mut |_| Ok(()),
        )
        .unwrap()
    };
    let r = open().exec(b"echo ssh1-$((6*7))", b"").unwrap();
    assert_eq!(String::from_utf8_lossy(&r.output), "ssh1-42\n");
    let mut pty = open()
        .into_pty("vt100", 80, 24, Duration::from_millis(200))
        .unwrap();
    pty.writer.send(b"echo pty1-$((6*7))\r").unwrap();
    until(&mut pty, "pty1-42");
    pty.writer.close().unwrap();
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up) and LEGCON_LAB_RSH=1"]
fn rsh_and_rlogin() {
    if std::env::var("LEGCON_LAB_RSH").is_err() {
        eprintln!("skipped: rsh needs the client's reserved source port to reach the server");
        return;
    }
    let s = lc_rcmd::rsh::Rsh::connect(
        &host(),
        10514,
        USER,
        USER,
        Default::default(),
        &Default::default(),
    )
    .unwrap();
    exercise(
        Box::new(s),
        PathStyle::Unix,
        &["home", "lab", "upload"],
        "rsh",
    );
}
