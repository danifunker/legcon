//! legcon against real FOSS servers in the lab container (lab/README.md), not pretend ones.
//! Start it first (`lab/lab.sh up`), then: cargo test --test lab_unix90s -- --ignored
//! LEGCON_LAB_HOST names the container's host (127.0.0.1); the ports are lab.sh's.
//! LEGCON_LAB_RSH=1 also runs rsh / rlogin, which need the client's reserved port to arrive intact;
//! LEGCON_LAB_NATIVE=1 uses the container's own ports, for running inside its network.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use lc_core::{Connection, FileSession, PathStyle, Pty, RemotePath, WriteMode};

const USER: &str = "lab";
const PASSWORD: &str = "lab";

fn host() -> String {
    std::env::var("LEGCON_LAB_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

/// A lab port as `lab.sh` publishes it; with LEGCON_LAB_NATIVE=1 (tests run inside the lab
/// container's network, where rsh's reserved port arrives intact) the container's own port.
fn p(published: u16) -> u16 {
    const NATIVE: [(u16, u16); 6] = [
        (10021, 21),
        (10022, 22),
        (10023, 23),
        (10512, 512),
        (10513, 513),
        (10514, 514),
    ];
    if std::env::var("LEGCON_LAB_NATIVE").is_err() {
        return published;
    }
    NATIVE
        .iter()
        .find(|(p, _)| *p == published)
        .map_or(published, |(_, n)| *n)
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
    // The keepalive and check-before-use ping: every server answers it, and still lists after.
    let ping = std::time::Duration::from_secs(5);
    fs.ping(ping)
        .unwrap_or_else(|e| panic!("{label}: ping: {e}"));
    let listed = names(fs, &home);
    fs.ping(ping)
        .unwrap_or_else(|e| panic!("{label}: second ping: {e}"));
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
fn ftp_append_and_checksum() {
    let mut s = lc_ftp::connect(&host(), p(10021), USER, PASSWORD, &Default::default()).unwrap();
    resume_round_trip(&mut s, "vsftpd", false);
    s.disconnect(Duration::from_secs(5)).unwrap();
    // proftpd refuses APPE as configured; the queue's copy then sends the file whole.
    let mut s = lc_ftp::connect(&host(), p(10121), USER, PASSWORD, &Default::default()).unwrap();
    let local = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..3000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(local.path().join("part.bin"), &data).unwrap();
    let remote = RemotePath::parse(
        PathStyle::Unix,
        &format!("/home/lab/upload/lc-part-{}.bin", std::process::id()),
    );
    {
        let mut w = s.open_write(&remote, WriteMode::Create).unwrap();
        w.write_all(&data[..1000]).unwrap();
    }
    let item = legcon::model::transfer::Item {
        dir: legcon::model::transfer::Direction::Upload,
        remote: remote.clone(),
        local: local.path().join("part.bin"),
        size: Some(3000),
        modified: None,
        folder: false,
        exists: Some(Some(1000)),
        problem: None,
    };
    let s = std::sync::Arc::new(std::sync::Mutex::new(s));
    let job = {
        let s = std::sync::Arc::clone(&s);
        legcon::model::job::Job::spawn(
            "resume",
            std::sync::Arc::new(legcon::model::job::NoNotify),
            move |ctx| {
                legcon::model::transfer::copy_item(
                    &mut *s.lock().unwrap(),
                    &item,
                    legcon::model::transfer::Mode::Resume,
                    true,
                    ctx,
                )
            },
        )
    };
    job.wait();
    let c = job.take_result().expect("copied");
    assert_eq!((c.resumed_at, c.bytes), (0, 3000), "proftpd: sent whole");
    let mut s = s.lock().unwrap();
    assert_eq!(s.stat(&remote).unwrap().size, Some(3000));
    s.remove(&remote).unwrap();
    s.disconnect(Duration::from_secs(5)).unwrap();
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
    let mut s = lc_ftp::connect(&host(), p(10121), USER, PASSWORD, &Default::default()).unwrap();
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
    nfs(3, "nfs");
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn nfs2_user_server() {
    nfs(2, "nfs2");
}

fn nfs(version: u32, tag: &str) {
    let opts = lc_nfs::Options {
        uid: 1000,
        gid: 1000,
        portmap_port: 10111,
        version: Some(version),
        ..Default::default()
    };
    let s = lc_nfs::connect(&host(), &opts).unwrap();
    assert!(
        s.describe().starts_with(&format!("NFSv{version} ")),
        "{}",
        s.describe()
    );
    exercise(
        Box::new(s),
        PathStyle::Unix,
        &["home", "lab", "upload"],
        tag,
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
        ..Default::default()
    }
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn sftp_openssh_vintage() {
    exercise(
        Box::new(ssh(10202)),
        PathStyle::Unix,
        &["home", "lab", "upload"],
        "openssh3",
    );
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn shell_commands_on_vintage_openssh() {
    // OpenSSH 3.4 without SFTP: SCP, dd and cat for resume, cksum for the check.
    let opts = lc_ssh::Options {
        sftp: false,
        ..Default::default()
    };
    let mut s = lc_ssh::connect(&host(), 10202, USER, &password_auth(), &opts, &mut |_| {
        Ok(())
    })
    .unwrap();
    resume_round_trip(&mut s, "openssh3-shell", true);
    s.disconnect(Duration::from_secs(5)).unwrap();
}

/// A 10-file tree with nested and empty folders, up through the queue's copy and back down.
fn tree_round_trip(fs: Box<dyn FileSession>, label: &str) {
    use legcon::model::transfer::{copy, Batch, Direction, Pick};
    let fs = std::sync::Arc::new(std::sync::Mutex::new(fs));
    let run = |dir: Direction, local: &std::path::Path, remote: &RemotePath, name: &str| {
        let (fs, local, remote, name) = (
            std::sync::Arc::clone(&fs),
            local.to_path_buf(),
            remote.clone(),
            name.to_string(),
        );
        let job = legcon::model::job::Job::spawn(
            "tree",
            std::sync::Arc::new(legcon::model::job::NoNotify),
            move |ctx| {
                let picks = [Pick { name, is_dir: true }];
                let b = Batch {
                    dir,
                    remote_dir: &remote,
                    local_dir: &local,
                    picks: &picks,
                    sizes: &[None],
                };
                copy(&mut **fs.lock().unwrap(), &b, true, ctx)
            },
        );
        job.wait();
        job.take_result()
            .unwrap_or_else(|| panic!("{label}: {:?}", job.snapshot().status))
    };
    let files = [
        "a1.txt",
        "a2.txt",
        "a3.txt",
        "sub/b1.txt",
        "sub/b2.txt",
        "sub/b3.txt",
        "sub/deep/c1.txt",
        "sub/deep/c2.txt",
        "other/d1.txt",
        "other/d2.txt",
    ];
    let name = format!("lc-tree-{label}-{}", std::process::id());
    let up = tempfile::tempdir().unwrap();
    for f in files {
        let p = up.path().join(&name).join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("{f} on {label}")).unwrap();
    }
    std::fs::create_dir_all(up.path().join(&name).join("empty")).unwrap();
    let upload = RemotePath::parse(PathStyle::Unix, "/home/lab/upload");
    let s = run(Direction::Upload, up.path(), &upload, &name);
    assert_eq!((s.files, s.failed.len()), (10, 0), "{label}: up {s:?}");
    let down = tempfile::tempdir().unwrap();
    let s = run(Direction::Download, down.path(), &upload, &name);
    assert_eq!((s.files, s.failed.len()), (10, 0), "{label}: down {s:?}");
    for f in files {
        assert_eq!(
            std::fs::read_to_string(down.path().join(&name).join(f)).unwrap(),
            format!("{f} on {label}"),
            "{label}: {f}"
        );
    }
    assert!(down.path().join(&name).join("empty").is_dir(), "{label}");
    let mut fs = fs.lock().unwrap();
    legcon::model::transfer::remove_tree(&mut **fs, &upload.join(&name), true).unwrap();
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn a_folder_tree_up_and_down() {
    let ftp = lc_ftp::connect(&host(), 10021, USER, PASSWORD, &Default::default()).unwrap();
    tree_round_trip(Box::new(ftp), "vsftpd");
    tree_round_trip(Box::new(ssh(10022)), "openssh");
    tree_round_trip(Box::new(ssh(10202)), "openssh3");
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
    for port in [10022, 10222, 10202] {
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
        p(10514),
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

// --- rsh trust, resume over a shell, awkward names ---

/// The client's reserved port reaches the lab's rshd intact (no port rewriting in between).
fn rsh_direct() -> bool {
    std::env::var("LEGCON_LAB_RSH").is_ok()
}

/// A command as `lab2` (csh, no .rhosts) over rexec; its output.
fn lab2(cmd: &[u8]) -> String {
    let r =
        lc_rcmd::Rexec::connect(&host(), p(10512), b"lab2", b"lab2", &Default::default()).unwrap();
    String::from_utf8_lossy(&r.run(cmd).unwrap()).into_owned()
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn rsh_trust_over_rexec_ftp_and_telnet() {
    use legcon::model::machines::{Machine, Service, ServiceKind};
    // Linux hands out ports below 1024 only to root (or with the capability); Windows to anyone.
    if !(1000..1024).any(|port| std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()) {
        eprintln!("skipped: this user cannot use a source port below 1024, which rsh needs");
        return;
    }
    for (kind, port) in [
        (ServiceKind::Ftp, p(10021)),
        (ServiceKind::Rexec, p(10512)),
        (ServiceKind::Telnet, p(10023)),
    ] {
        lab2(b"rm -f .rhosts");
        let mut m = Machine::new("lab2", &host());
        m.user = "lab2".into();
        m.services = vec![
            Service {
                kind: ServiceKind::Rsh,
                port: p(10514),
            },
            Service { kind, port },
        ];
        let run = |install: bool| {
            let job = legcon::model::rsh_trust::rsh_trust(
                &m,
                Some("lab2".into()),
                install,
                std::sync::Arc::new(legcon::model::job::NoNotify),
            );
            let snap = job.wait();
            job.take_result()
                .unwrap_or_else(|| panic!("{kind:?}: {:?}", snap.status))
        };
        let r = run(true);
        assert_eq!(r.via, Some(kind));
        assert!(
            !r.added.is_empty() && r.already.is_empty(),
            "{kind:?}: {r:?}"
        );
        let rhosts = lab2(b"cat .rhosts; ls -l .rhosts");
        for l in &r.added {
            assert!(rhosts.contains(l.as_str()), "{kind:?}: {l} not in {rhosts}");
        }
        assert!(rhosts.contains("-rw-------"), "{kind:?}: {rhosts}");
        let seen = r.seen.unwrap();
        if kind != ServiceKind::Ftp {
            assert!(
                seen.server.is_some(),
                "{kind:?}: the server did not say what it sees"
            );
        }
        if rsh_direct() {
            assert!(!seen.port_rewritten(), "{kind:?}: {seen:?}");
            assert_eq!(r.rsh, Ok(()), "{kind:?}");
        } else if seen.port_rewritten() {
            assert!(
                r.rsh.is_err(),
                "{kind:?}: rsh got in through a rewritten port?"
            );
        }
        if !rsh_direct() && kind != ServiceKind::Ftp {
            // A second setup finds its lines there and adds nothing.
            let again = run(true);
            assert!(again.added.is_empty(), "{kind:?}: {again:?}");
            assert_eq!(again.already, r.added, "{kind:?}");
        }
    }
    lab2(b"rm -f .rhosts");
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn the_machine_says_where_we_come_from() {
    let seen =
        lc_rcmd::seen_via_rexec(&host(), p(10512), b"lab", b"lab", &Default::default()).unwrap();
    let server = seen.server.expect("netstat shows our connection");
    if rsh_direct() {
        assert_eq!(server, seen.local);
    }
    eprintln!("we: {}, the machine sees: {server}", seen.local);
}

/// Upload in two parts (the second appended), then read back from several offsets.
/// Upload in two parts (the second a resume), read back from offsets, then compare the sum the
/// machine computes, which shell protocols must have (`cksum`).
fn resume_round_trip(fs: &mut dyn FileSession, label: &str, needs_sum: bool) {
    let data: Vec<u8> = (0..5000u32).map(|i| (i * 13 % 251) as u8).collect();
    let file = RemotePath::parse(
        PathStyle::Unix,
        &format!(
            "/home/lab/upload/lc-resume-{label}-{}.bin",
            std::process::id()
        ),
    );
    {
        let mut w = fs.open_write_sized(&file, WriteMode::Create, 1234).unwrap();
        w.write_all(&data[..1234]).unwrap();
    }
    {
        let mut w = fs
            .open_write_sized(&file, WriteMode::Resume(1234), (data.len() - 1234) as u64)
            .unwrap_or_else(|e| panic!("{label}: resume upload: {e}"));
        w.write_all(&data[1234..])
            .unwrap_or_else(|e| panic!("{label}: resume upload: {e}"));
    }
    assert_eq!(fs.stat(&file).unwrap().size, Some(5000), "{label}");
    match fs
        .checksum(&file)
        .unwrap_or_else(|e| panic!("{label}: checksum: {e}"))
    {
        Some(theirs) => assert_eq!(
            theirs.of(&mut &data[..]).unwrap(),
            theirs,
            "{label}: the machine's sum differs"
        ),
        None => assert!(!needs_sum, "{label}: no checksum"),
    }
    for at in [0usize, 700, 1024, 4999] {
        let mut back = Vec::new();
        fs.open_read(&file, at as u64)
            .unwrap_or_else(|e| panic!("{label}: read from {at}: {e}"))
            .read_to_end(&mut back)
            .unwrap();
        assert!(
            back == data[at..],
            "{label}: from {at}: {} bytes",
            back.len()
        );
    }
    let missing = file.parent().unwrap().join("lc-no-such-file");
    assert!(
        matches!(fs.open_read(&missing, 10), Err(lc_core::Error::NotFound(_))),
        "{label}: a missing file"
    );
    fs.remove(&file).unwrap();
}

/// Names a shell would mangle, round trip: upload, list, stat, download, rename, delete.
fn awkward_names(fs: &mut dyn FileSession, label: &str, latin1: bool) {
    let work = RemotePath::parse(
        PathStyle::Unix,
        &format!("/home/lab/upload/lc-names-{label}-{}", std::process::id()),
    );
    fs.mkdir(&work).unwrap();
    let mut names = vec![
        "it's here.txt".to_string(),
        "dollar $HOME.txt".into(),
        "back`tick`.txt".into(),
        "semi;colon & amp.txt".into(),
        "star *.txt".into(),
        "-dash.txt".into(),
        "double \"quote\".txt".into(),
        "back\\slash.txt".into(),
        "  two spaces".into(),
    ];
    if latin1 {
        names.push("caf\u{e9} 2.txt".into());
    }
    for name in &names {
        let file = work.join(name);
        {
            let mut w = fs
                .open_write_sized(&file, WriteMode::Create, 3)
                .unwrap_or_else(|e| panic!("{label}: upload {name:?}: {e}"));
            w.write_all(b"abc").unwrap();
        }
        let listed = names_in(fs, &work);
        assert!(listed.contains(name), "{label}: {name:?} not in {listed:?}");
        assert_eq!(fs.stat(&file).unwrap().size, Some(3), "{label}: {name:?}");
        let mut back = Vec::new();
        fs.open_read(&file, 0)
            .unwrap()
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, b"abc", "{label}: {name:?}");
        let moved = work.join(&format!("{name}.2"));
        fs.rename(&file, &moved)
            .unwrap_or_else(|e| panic!("{label}: rename {name:?}: {e}"));
        fs.remove(&moved)
            .unwrap_or_else(|e| panic!("{label}: delete {name:?}: {e}"));
    }
    assert!(names_in(fs, &work).is_empty(), "{label}: left behind");
    fs.remove(&work).unwrap();
}

fn names_in(fs: &mut dyn FileSession, dir: &RemotePath) -> Vec<String> {
    fs.list(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n != "." && n != "..")
        .collect()
}

#[test]
#[ignore = "needs the lab container (lab/lab.sh up)"]
fn resume_and_awkward_names_over_shell_commands() {
    let opts = lc_ssh::Options {
        sftp: false,
        ..Default::default()
    };
    let mut s = lc_ssh::connect(
        &host(),
        p(10022),
        USER,
        &password_auth(),
        &opts,
        &mut |_| Ok(()),
    )
    .unwrap();
    resume_round_trip(&mut s, "ssh-shell", true);
    awkward_names(&mut s, "ssh-shell", false);
    s.disconnect(Duration::from_secs(5)).unwrap();
    if !rsh_direct() {
        eprintln!(
            "rsh part skipped: rsh needs the client's reserved source port to reach the server"
        );
        return;
    }
    let mut r = lc_rcmd::rsh::Rsh::connect(
        &host(),
        p(10514),
        USER,
        USER,
        Default::default(),
        &Default::default(),
    )
    .unwrap();
    resume_round_trip(&mut r, "rsh", true);
    awkward_names(&mut r, "rsh", true);
    let e = lc_core::ShellSession::exec(&mut r, "echo out; echo err >&2; exit 3").unwrap();
    assert_eq!(
        (e.output.as_slice(), e.errors.as_slice()),
        (&b"out\n"[..], &b"err\n"[..])
    );
    assert_eq!(e.exit_status, Some(3));
}
