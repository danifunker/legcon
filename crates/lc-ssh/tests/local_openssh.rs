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
