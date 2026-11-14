//! Login keys for old SSH servers (docs/PLAN.md §5.1): making one, its public forms, and the
//! shell commands that install it the way each server family expects.
//!
//! Private keys are written as traditional unencrypted PEM (`RSA PRIVATE KEY`, `DSA PRIVATE KEY`),
//! the one form every libssh2 backend reads. RSA 2048 suits OpenSSH 2.5 and later and SSH.com 2.x;
//! DSA 1024 is for servers that only take `ssh-dss` user keys (OpenSSH before 2.5, early SSH.com).

use std::path::Path;

use lc_core::{Error, Result};
use rand::rngs::OsRng;

/// Which kind of key to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Rsa2048,
    Dsa1024,
}

impl KeyKind {
    pub fn algorithm(self) -> &'static str {
        match self {
            KeyKind::Rsa2048 => "ssh-rsa",
            KeyKind::Dsa1024 => "ssh-dss",
        }
    }

    /// The key an old server takes, judging by its version line.
    pub fn for_banner(banner: &str) -> KeyKind {
        let v = banner.split('-').nth(2).unwrap_or("");
        let old_openssh = ["OpenSSH_2.0", "OpenSSH_2.1", "OpenSSH_2.2", "OpenSSH_2.3"]
            .iter()
            .any(|p| v.starts_with(p));
        let old_sshcom = ["2.0.", "2.1.", "2.2.", "2.3."]
            .iter()
            .any(|p| v.starts_with(p));
        if old_openssh || old_sshcom {
            KeyKind::Dsa1024
        } else {
            KeyKind::Rsa2048
        }
    }
}

/// A new key pair: the private key as PEM, and the public key blob (SSH wire form).
pub struct KeyPair {
    pub kind: KeyKind,
    pub private_pem: String,
    pub blob: Vec<u8>,
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// An SSH `mpint` from big-endian magnitude bytes.
fn put_mpint(out: &mut Vec<u8>, be: &[u8]) {
    let be = &be[be.iter().take_while(|&&b| b == 0).count()..];
    let mut v = Vec::with_capacity(be.len() + 1);
    if be.first().is_some_and(|&b| b & 0x80 != 0) {
        v.push(0);
    }
    v.extend_from_slice(be);
    put_str(out, &v);
}

/// A DER INTEGER from big-endian magnitude bytes.
fn der_int(out: &mut Vec<u8>, be: &[u8]) {
    let be = &be[be
        .iter()
        .take_while(|&&b| b == 0)
        .count()
        .min(be.len().saturating_sub(1))..];
    let mut v = Vec::new();
    if be.is_empty() || be[0] & 0x80 != 0 {
        v.push(0);
    }
    v.extend_from_slice(be);
    out.push(0x02);
    der_len(out, v.len());
    out.extend_from_slice(&v);
}

fn der_len(out: &mut Vec<u8>, n: usize) {
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let b = (n as u32).to_be_bytes();
        let skip = b.iter().take_while(|&&x| x == 0).count();
        out.push(0x80 | (4 - skip) as u8);
        out.extend_from_slice(&b[skip..]);
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn pem(label: &str, der: &[u8]) -> String {
    let b = base64(der);
    let mut s = format!("-----BEGIN {label}-----\n");
    for line in b.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(line).unwrap());
        s.push('\n');
    }
    s.push_str(&format!("-----END {label}-----\n"));
    s
}

/// Make a new key pair; RSA 2048 takes a few seconds.
pub fn generate(kind: KeyKind) -> Result<KeyPair> {
    let fail = |e: String| Error::Protocol(format!("could not make the key: {e}"));
    match kind {
        KeyKind::Rsa2048 => {
            use rsa::pkcs1::EncodeRsaPrivateKey;
            use rsa::traits::PublicKeyParts;
            let k = rsa::RsaPrivateKey::new(&mut OsRng, 2048).map_err(|e| fail(e.to_string()))?;
            let private_pem = k
                .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
                .map_err(|e| fail(e.to_string()))?
                .to_string();
            let mut blob = Vec::new();
            put_str(&mut blob, b"ssh-rsa");
            put_mpint(&mut blob, &k.e().to_bytes_be());
            put_mpint(&mut blob, &k.n().to_bytes_be());
            Ok(KeyPair {
                kind,
                private_pem,
                blob,
            })
        }
        KeyKind::Dsa1024 => {
            // 1024 bits is the only size these servers take; weak is the point here.
            #[allow(deprecated)]
            let c = dsa::Components::generate(&mut OsRng, dsa::KeySize::DSA_1024_160);
            let k = dsa::SigningKey::generate(&mut OsRng, c);
            let v = k.verifying_key();
            let c = v.components();
            let (p, q, g) = (
                c.p().to_bytes_be(),
                c.q().to_bytes_be(),
                c.g().to_bytes_be(),
            );
            let (y, x) = (v.y().to_bytes_be(), k.x().to_bytes_be());
            let mut body = Vec::new();
            for n in [&[0u8][..], &p, &q, &g, &y, &x] {
                der_int(&mut body, n);
            }
            let mut der = vec![0x30];
            der_len(&mut der, body.len());
            der.extend_from_slice(&body);
            let mut blob = Vec::new();
            put_str(&mut blob, b"ssh-dss");
            for n in [&p, &q, &g, &y] {
                put_mpint(&mut blob, n);
            }
            Ok(KeyPair {
                kind,
                private_pem: pem("DSA PRIVATE KEY", &der),
                blob,
            })
        }
    }
}

/// The OpenSSH `authorized_keys` line: `ssh-rsa AAAA... comment`.
pub fn openssh_line(blob: &[u8], comment: &str) -> String {
    let alg = crate::sftp::Cursor::new(blob)
        .bytes()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .unwrap_or_default();
    format!("{alg} {} {comment}", base64(blob))
}

/// The SECSH public key file SSH.com / F-Secure servers read (RFC 4716).
pub fn secsh_text(blob: &[u8], comment: &str) -> String {
    let mut s = String::from("---- BEGIN SSH2 PUBLIC KEY ----\n");
    s.push_str(&format!("Comment: \"{comment}\"\n"));
    for line in base64(blob).as_bytes().chunks(70) {
        s.push_str(std::str::from_utf8(line).unwrap());
        s.push('\n');
    }
    s.push_str("---- END SSH2 PUBLIC KEY ----\n");
    s
}

/// Which server family a version line belongs to, for where its keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// OpenSSH before 3.0 reads SSH-2 keys from `authorized_keys2` only.
    OpenSshOld,
    /// OpenSSH 3.0 and later, Dropbear, Sun SSH: `authorized_keys`.
    OpenSsh,
    /// SSH.com / F-Secure: `~/.ssh2/authorization` names a key file.
    SshCom,
}

impl Family {
    pub fn of(banner: &str) -> Family {
        let v = banner.splitn(3, '-').nth(2).unwrap_or("");
        if let Some(rest) = v.strip_prefix("OpenSSH_") {
            if rest.starts_with('2') || rest.starts_with("1.") {
                return Family::OpenSshOld;
            }
            return Family::OpenSsh;
        }
        let sshcom = v.contains("SSH Secure Shell")
            || v.contains("F-Secure")
            || v.starts_with("2.")
            || v.starts_with("3.");
        if sshcom {
            Family::SshCom
        } else {
            Family::OpenSsh
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Family::OpenSshOld => "~/.ssh/authorized_keys2",
            Family::OpenSsh => "~/.ssh/authorized_keys",
            Family::SshCom => "~/.ssh2/authorization",
        }
    }
}

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A Bourne shell command that installs the key once (run again, it changes nothing) and sets
/// the permissions these servers insist on. `name` names the key file for SSH.com.
pub fn install_command(family: Family, blob: &[u8], name: &str) -> String {
    let b64 = base64(blob);
    match family {
        Family::OpenSsh | Family::OpenSshOld => {
            let file = if family == Family::OpenSshOld {
                ".ssh/authorized_keys2"
            } else {
                ".ssh/authorized_keys"
            };
            format!(
                "cd && umask 077 && (test -d .ssh || mkdir .ssh) && chmod 700 .ssh && \
                 (grep {b64q} {file} >/dev/null 2>&1 || echo {line} >> {file}) && chmod 600 {file}",
                b64q = sq(&b64),
                line = sq(&openssh_line(blob, name)),
            )
        }
        Family::SshCom => {
            let file = format!("{name}.pub");
            let lines: Vec<String> = secsh_text(blob, name).lines().map(sq).collect();
            format!(
                "cd && umask 077 && (test -d .ssh2 || mkdir .ssh2) && chmod 700 .ssh2 && \
                 printf '%s\\n' {lines} > .ssh2/{file} && \
                 (grep {keyline} .ssh2/authorization >/dev/null 2>&1 || echo {keyline} >> .ssh2/authorization) && \
                 chmod 600 .ssh2/authorization .ssh2/{file}",
                lines = lines.join(" "),
                keyline = sq(&format!("Key {file}")),
            )
        }
    }
}

/// Write the pair as `path` (private) and `path.pub` (OpenSSH public line).
pub fn save(pair: &KeyPair, path: &Path, comment: &str) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, &pair.private_pem)?;
    std::fs::write(public_path(path), openssh_line(&pair.blob, comment) + "\n")?;
    Ok(())
}

/// Where a private key's public half is kept.
pub fn public_path(private: &Path) -> std::path::PathBuf {
    let mut p = private.as_os_str().to_owned();
    p.push(".pub");
    p.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_from_version_lines() {
        assert_eq!(Family::of("SSH-2.0-OpenSSH_2.9p2"), Family::OpenSshOld);
        assert_eq!(Family::of("SSH-1.99-OpenSSH_3.9p1"), Family::OpenSsh);
        assert_eq!(Family::of("SSH-2.0-dropbear_0.45"), Family::OpenSsh);
        assert_eq!(
            Family::of("SSH-2.0-3.2.9.1 SSH Secure Shell (non-commercial)"),
            Family::SshCom
        );
        assert_eq!(
            KeyKind::for_banner("SSH-2.0-OpenSSH_2.3.0p1"),
            KeyKind::Dsa1024
        );
        assert_eq!(
            KeyKind::for_banner("SSH-2.0-OpenSSH_3.4p1"),
            KeyKind::Rsa2048
        );
    }

    #[test]
    fn integers_in_both_encodings() {
        let mut v = Vec::new();
        put_mpint(&mut v, &[0, 0x80, 1]);
        assert_eq!(v, [0, 0, 0, 3, 0, 0x80, 1]);
        let mut d = Vec::new();
        der_int(&mut d, &[0]);
        der_int(&mut d, &[0x7f]);
        der_int(&mut d, &[0x80]);
        assert_eq!(d, [2, 1, 0, 2, 1, 0x7f, 2, 2, 0, 0x80]);
        let mut l = Vec::new();
        der_len(&mut l, 300);
        assert_eq!(l, [0x82, 1, 44]);
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn new_keys_have_pem_and_a_matching_blob() {
        for kind in [KeyKind::Rsa2048, KeyKind::Dsa1024] {
            let k = generate(kind).unwrap();
            let label = if kind == KeyKind::Rsa2048 {
                "RSA"
            } else {
                "DSA"
            };
            assert!(k.private_pem.starts_with(&format!(
                "-----BEGIN {label} PRIVATE KEY-----
"
            )));
            assert!(openssh_line(&k.blob, "x").starts_with(&format!("{} AAAA", kind.algorithm())));
        }
    }

    #[test]
    fn install_commands_quote_and_name_the_right_file() {
        let blob = {
            let mut b = Vec::new();
            put_str(&mut b, b"ssh-dss");
            b
        };
        let c = install_command(Family::OpenSshOld, &blob, "legcon-sun");
        assert!(
            c.contains("authorized_keys2") && c.contains("'ssh-dss AAAAB3NzaC1kc3M= legcon-sun'")
        );
        let c = install_command(Family::SshCom, &blob, "legcon-sun");
        assert!(c.contains(".ssh2/legcon-sun.pub") && c.contains("'Key legcon-sun.pub'"));
    }
}
