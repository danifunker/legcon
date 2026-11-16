//! Against the system's own OpenSSH `sshd`, started unprivileged on a spare port (macOS ships
//! one): key login, strict key exchange, exec, SFTP and a terminal. Opt-in, as it needs `sshd`
//! and `ssh-keygen`: `cargo test -p lc-ssh --test local_openssh -- --ignored`.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lc_core::FileSession;

struct Sshd(Child);

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn keygen(args: &[&str]) {
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-N", ""])
        .args(args)
        .status()
        .expect("ssh-keygen")
        .success();
    assert!(ok, "ssh-keygen {args:?}");
}

fn start(dir: &Path, port: u16) -> Sshd {
    start_with(dir, port, None)
}

fn start_with(dir: &Path, port: u16, sftp_override: Option<&str>) -> Sshd {
    let d = dir.display();
    keygen(&["-t", "ed25519", "-f", &format!("{d}/host")]);
    keygen(&[
        "-t",
        "rsa",
        "-b",
        "2048",
        "-m",
        "PEM",
        "-f",
        &format!("{d}/user"),
    ]);
    std::fs::copy(dir.join("user.pub"), dir.join("authorized_keys")).unwrap();
    let sftp = [
        "/usr/libexec/sftp-server",
        "/usr/lib/openssh/sftp-server",
        "/usr/lib/ssh/sftp-server",
    ]
    .into_iter()
    .find(|p| Path::new(p).exists())
    .unwrap_or("internal-sftp");
    let sftp = sftp_override.unwrap_or(sftp);
    let config = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host\nPidFile {d}/sshd.pid\n\
         AuthorizedKeysFile {d}/authorized_keys\nUsePAM no\nStrictModes no\n\
         PasswordAuthentication no\nKbdInteractiveAuthentication no\nSubsystem sftp {sftp}\n"
    );
    std::fs::write(dir.join("sshd_config"), config).unwrap();
    let child = Command::new("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(dir.join("sshd_config"))
        .stderr(Stdio::null())
        .spawn()
        .expect("sshd");
    let end = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < end, "sshd did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    Sshd(child)
}

#[test]
#[ignore]
fn key_login_strict_kex_exec_sftp_and_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let port = 22990;
    let _sshd = start(dir.path(), port);
    let user = std::env::var("USER").unwrap();
    let auth = lc_ssh::Auth {
        password: None,
        key: Some(lc_ssh::KeyFiles {
            private: dir.path().join("user"),
            public: dir.path().join("user.pub"),
        }),
        ..Default::default()
    };
    let mut seen = None;
    let mut s = lc_ssh::connect(
        "127.0.0.1",
        port,
        &user,
        &auth,
        &Default::default(),
        &mut |k| {
            seen = Some(k.algorithm.clone());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(seen.as_deref(), Some("ssh-ed25519"));
    let n = s.negotiated().clone();
    assert!(
        n.strict,
        "OpenSSH 9.6 and later agree to strict key exchange: {n:?}"
    );
    assert!(!n.weak(), "{n:?}");

    let r = s.run(b"echo legcon; echo oops >&2; exit 3").unwrap();
    assert_eq!(r.output, b"legcon\n");
    assert_eq!(r.errors, b"oops\n");
    assert_eq!(r.exit_status, Some(3));

    let home = s.home().unwrap();
    std::fs::write(dir.path().join("probe.txt"), b"over sftp").unwrap();
    let listed = s
        .list(&lc_core::RemotePath::parse(
            lc_core::PathStyle::Unix,
            &dir.path().display().to_string(),
        ))
        .unwrap();
    assert!(listed.iter().any(|e| e.name == "probe.txt"), "{home}");

    let pty = s.into_pty("vt100", 80, 24).unwrap();
    let (mut reader, mut writer) = (pty.reader, pty.writer);
    writer.send(b"echo pty-$((6*7))\n").unwrap();
    let mut got = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 1024];
    while !String::from_utf8_lossy(&got).contains("pty-42") {
        assert!(Instant::now() < end, "{}", String::from_utf8_lossy(&got));
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }
    }
    writer.close().unwrap();
}

/// An sshd that takes the sftp subsystem but has no sftp-server (embedded Linux often): files fall
/// back to SCP and shell commands.
#[test]
#[ignore]
fn a_missing_sftp_server_falls_back_to_scp() {
    let dir = tempfile::tempdir().unwrap();
    let port = 22991;
    let _sshd = start_with(dir.path(), port, Some("/no/such/sftp-server"));
    let user = std::env::var("USER").unwrap();
    let auth = lc_ssh::Auth {
        password: None,
        key: Some(lc_ssh::KeyFiles {
            private: dir.path().join("user"),
            public: dir.path().join("user.pub"),
        }),
        ..Default::default()
    };
    let log = lc_core::TraceLog::default();
    let opts = lc_ssh::Options {
        trace: Some(log.clone()),
        ..Default::default()
    };
    let mut s = lc_ssh::connect("127.0.0.1", port, &user, &auth, &opts, &mut |_| Ok(())).unwrap();
    let home = s.home().expect("home through the shell");
    assert!(s.uses_scp());
    // The Log tab can say what happened at each step.
    let notes: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .map(|t| format!("{t:?}"))
        .collect();
    for want in [
        "Server: SSH-2.0-OpenSSH",
        "Host key accepted",
        "The key was accepted",
        "Logged in as",
        "No SFTP here",
    ] {
        assert!(
            notes.iter().any(|n| n.contains(want)),
            "{want} not in {notes:#?}"
        );
    }
    std::fs::write(dir.path().join("probe.txt"), b"x").unwrap();
    let listed = s
        .list(&lc_core::RemotePath::parse(
            lc_core::PathStyle::Unix,
            &dir.path().display().to_string(),
        ))
        .unwrap();
    assert!(listed.iter().any(|e| e.name == "probe.txt"), "{home}");
}

/// Logging in as `ssh` does: an ed25519 key held by a running ssh-agent, then one read from a
/// file, as `~/.ssh/id_ed25519` would be.
#[test]
#[ignore]
fn agent_and_ed25519_file_logins() {
    let dir = tempfile::tempdir().unwrap();
    let port = 22992;
    let _sshd = start(dir.path(), port);
    let d = dir.path().display().to_string();
    keygen(&["-t", "ed25519", "-f", &format!("{d}/ed")]);
    let mut keys = std::fs::read_to_string(dir.path().join("authorized_keys")).unwrap();
    keys.push_str(&std::fs::read_to_string(dir.path().join("ed.pub")).unwrap());
    std::fs::write(dir.path().join("authorized_keys"), keys).unwrap();
    let user = std::env::var("USER").unwrap();

    // The agent alone: its key is the only way in.
    let sock = dir.path().join("agent.sock");
    let mut agent = Command::new("ssh-agent")
        .args(["-D", "-a"])
        .arg(&sock)
        .stdout(Stdio::null())
        .spawn()
        .expect("ssh-agent");
    let end = Instant::now() + Duration::from_secs(5);
    while !sock.exists() {
        assert!(Instant::now() < end, "ssh-agent did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let added = Command::new("ssh-add")
        .arg(dir.path().join("ed"))
        .env("SSH_AUTH_SOCK", &sock)
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(added.success());
    std::env::set_var("SSH_AUTH_SOCK", &sock);
    let log = lc_core::TraceLog::default();
    let opts = lc_ssh::Options {
        trace: Some(log.clone()),
        ..Default::default()
    };
    let auth = lc_ssh::Auth {
        agent: true,
        ..Default::default()
    };
    let r = lc_ssh::connect("127.0.0.1", port, &user, &auth, &opts, &mut |_| Ok(()));
    let _ = agent.kill();
    let _ = agent.wait();
    let notes = format!("{:#?}", log.lock().unwrap());
    let mut s = r.unwrap_or_else(|e| panic!("{e}: {notes}"));
    assert!(notes.contains("The agent's key"), "{notes}");
    assert_eq!(s.run(b"echo agent").unwrap().output, b"agent\n");

    // The file alone, with no agent and no password.
    std::env::remove_var("SSH_AUTH_SOCK");
    let auth = lc_ssh::Auth {
        user_keys: vec![dir.path().join("ed")],
        ..Default::default()
    };
    let mut s = lc_ssh::connect(
        "127.0.0.1",
        port,
        &user,
        &auth,
        &Default::default(),
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(s.run(b"echo file").unwrap().output, b"file\n");

    // Nothing that gets in: the error says so in the words the app asks for a password on.
    let auth = lc_ssh::Auth::default();
    let e = lc_ssh::connect(
        "127.0.0.1",
        port,
        &user,
        &auth,
        &Default::default(),
        &mut |_| Ok(()),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(e.contains("no password and no key to log in with"), "{e}");
}
