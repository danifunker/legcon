//! Reading login keys made elsewhere (traditional PEM, PKCS#8, OpenSSH, PuTTY), and what a public
//! key says about itself. Only `ssh-rsa` and `ssh-dss`: the keys 1990s servers take.

use base64::Engine;
use lc_core::{Error, Result};

use crate::keys::{KeyKind, KeyPair};
use crate::sftp::Cursor;

fn bad(why: impl Into<String>) -> Error {
    Error::Unsupported(why.into())
}

fn b64(s: &str) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(clean)
        .map_err(|e| bad(format!("bad base64: {e}")))
}

/// The algorithm, key size in bits, and fingerprints of a public key blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicInfo {
    pub algorithm: String,
    pub bits: usize,
    /// `SHA256:...` as modern OpenSSH prints it.
    pub sha256: String,
    /// `aa:bb:...` as an old server's admin sees it.
    pub md5: String,
}

fn bits_of(m: &[u8]) -> usize {
    let m = &m[m.iter().take_while(|&&b| b == 0).count()..];
    m.first()
        .map_or(0, |b| (m.len() - 1) * 8 + (8 - b.leading_zeros() as usize))
}

pub fn describe(blob: &[u8]) -> Result<PublicInfo> {
    use md5::Digest as _;
    let mut c = Cursor::new(blob);
    let algorithm = String::from_utf8_lossy(c.bytes()?).into_owned();
    let bits = match algorithm.as_str() {
        // ssh-rsa: e, n; ssh-dss: p, q, g, y.
        "ssh-rsa" => {
            c.bytes()?;
            bits_of(c.bytes()?)
        }
        "ssh-dss" => bits_of(c.bytes()?),
        _ => 0,
    };
    let sha = sha2::Sha256::digest(blob);
    let md5 = md5::Md5::digest(blob);
    Ok(PublicInfo {
        algorithm,
        bits,
        sha256: format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(sha)
        ),
        md5: md5
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    })
}

/// The blob in an OpenSSH public key line (`ssh-rsa AAAA... comment`), and its comment.
pub fn parse_public_line(line: &str) -> Result<(Vec<u8>, String)> {
    let mut parts = line.split_whitespace();
    let alg = parts.next().ok_or_else(|| bad("an empty public key"))?;
    let blob = b64(parts
        .next()
        .ok_or_else(|| bad("no key after the algorithm"))?)?;
    let inner = String::from_utf8_lossy(Cursor::new(&blob).bytes()?).into_owned();
    if inner != alg {
        return Err(bad("the key does not match its algorithm"));
    }
    Ok((blob, parts.collect::<Vec<_>>().join(" ")))
}

/// An SSH-1 public line (`bits e n comment`): its numbers as bytes, and its comment.
pub fn parse_ssh1_line(line: &str) -> Result<(Vec<u8>, Vec<u8>, String)> {
    let mut parts = line.split_whitespace();
    let mut num = |what: &str| -> Result<rsa::BigUint> {
        parts
            .next()
            .and_then(|t| rsa::BigUint::parse_bytes(t.as_bytes(), 10))
            .ok_or_else(|| bad(format!("an SSH-1 public key without its {what}")))
    };
    let _bits = num("size")?;
    let (e, n) = (num("exponent")?, num("modulus")?);
    Ok((
        e.to_bytes_be(),
        n.to_bytes_be(),
        parts.collect::<Vec<_>>().join(" "),
    ))
}

/// Size and fingerprints of an SSH-1 key, as ssh-keygen prints them (over n then e).
pub fn describe_ssh1(e: &[u8], n: &[u8]) -> PublicInfo {
    use md5::Digest as _;
    let mut both = n.to_vec();
    both.extend_from_slice(e);
    PublicInfo {
        algorithm: "rsa1".into(),
        bits: bits_of(n),
        sha256: format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(sha2::Sha256::digest(&both))
        ),
        md5: md5::Md5::digest(&both)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    }
}

const SSH1_ID: &[u8] = b"SSH PRIVATE KEY FILE FORMAT 1.1\n\0";

/// An SSH-1 identity file's public line, when it is one and has no passphrase.
pub fn ssh1_identity(data: &[u8]) -> Result<Option<String>> {
    let Some(rest) = data.strip_prefix(SSH1_ID) else {
        return Ok(None);
    };
    let mut c = Cursor::new(rest);
    if c.u8()? != 0 {
        return Err(passphrase());
    }
    c.u32()?;
    let bits = c.u32()?;
    let mut mp = || -> Result<rsa::BigUint> {
        let b = u16::from_be_bytes([c.u8()?, c.u8()?]) as usize;
        let mut v = Vec::with_capacity(b.div_ceil(8));
        for _ in 0..b.div_ceil(8) {
            v.push(c.u8()?);
        }
        Ok(rsa::BigUint::from_bytes_be(&v))
    };
    let (n, e) = (mp()?, mp()?);
    let comment = String::from_utf8_lossy(c.bytes()?).into_owned();
    Ok(Some(
        format!("{bits} {e} {n} {comment}").trim_end().to_string(),
    ))
}

/// The private key in an unencrypted SSH-1 identity file.
pub fn ssh1_private(data: &[u8]) -> Result<rsa::RsaPrivateKey> {
    let rest = data
        .strip_prefix(SSH1_ID)
        .ok_or_else(|| bad("not an SSH-1 key file; SSH-1 logins need an SSH-1 key"))?;
    let mut c = Cursor::new(rest);
    if c.u8()? != 0 {
        return Err(passphrase());
    }
    c.u32()?;
    c.u32()?;
    let mp = |c: &mut Cursor| -> Result<rsa::BigUint> {
        let b = u16::from_be_bytes([c.u8()?, c.u8()?]) as usize;
        let mut v = Vec::with_capacity(b.div_ceil(8));
        for _ in 0..b.div_ceil(8) {
            v.push(c.u8()?);
        }
        Ok(rsa::BigUint::from_bytes_be(&v))
    };
    let (n, e) = (mp(&mut c)?, mp(&mut c)?);
    c.bytes()?;
    let check = [c.u8()?, c.u8()?, c.u8()?, c.u8()?];
    if check[..2] != check[2..] {
        return Err(bad("the SSH-1 key file is damaged"));
    }
    let (d, _iqmp, q, p) = (mp(&mut c)?, mp(&mut c)?, mp(&mut c)?, mp(&mut c)?);
    rsa::RsaPrivateKey::from_components(n, e, d, vec![p, q])
        .map_err(|e| bad(format!("not a valid SSH-1 key: {e}")))
}

fn rsa_pair(k: rsa::RsaPrivateKey) -> Result<KeyPair> {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::traits::PublicKeyParts;
    let bits = k.n().bits();
    if !(1024..=4096).contains(&bits) {
        return Err(bad(format!(
            "an RSA key of {bits} bits; old servers take 1024 to 4096"
        )));
    }
    let private_pem = k
        .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
        .map_err(|e| bad(e.to_string()))?
        .to_string();
    let mut blob = Vec::new();
    crate::keys::put_str(&mut blob, b"ssh-rsa");
    crate::keys::put_mpint(&mut blob, &k.e().to_bytes_be());
    crate::keys::put_mpint(&mut blob, &k.n().to_bytes_be());
    Ok(KeyPair {
        kind: KeyKind::Rsa2048,
        private_pem,
        blob,
    })
}

fn rsa_from(n: &[u8], e: &[u8], d: &[u8], p: &[u8], q: &[u8]) -> Result<KeyPair> {
    use rsa::BigUint;
    let k = rsa::RsaPrivateKey::from_components(
        BigUint::from_bytes_be(n),
        BigUint::from_bytes_be(e),
        BigUint::from_bytes_be(d),
        vec![BigUint::from_bytes_be(p), BigUint::from_bytes_be(q)],
    )
    .map_err(|e| bad(format!("not a valid RSA key: {e}")))?;
    rsa_pair(k)
}

/// The DER INTEGERs of a SEQUENCE (a traditional DSA key).
fn der_ints(der: &[u8]) -> Result<Vec<Vec<u8>>> {
    fn len(d: &[u8], i: &mut usize) -> Result<usize> {
        let first = *d.get(*i).ok_or_else(|| bad("short key"))?;
        *i += 1;
        if first < 0x80 {
            return Ok(first as usize);
        }
        let n = (first & 0x7f) as usize;
        let mut v = 0usize;
        for _ in 0..n {
            v = (v << 8) | *d.get(*i).ok_or_else(|| bad("short key"))? as usize;
            *i += 1;
        }
        Ok(v)
    }
    let mut i = 0;
    if der.first() != Some(&0x30) {
        return Err(bad("not a DER sequence"));
    }
    i += 1;
    let end = len(der, &mut i)? + i;
    let mut out = Vec::new();
    while i < end.min(der.len()) {
        if der[i] != 0x02 {
            return Err(bad("not a DER integer"));
        }
        i += 1;
        let n = len(der, &mut i)?;
        out.push(der.get(i..i + n).ok_or_else(|| bad("short key"))?.to_vec());
        i += n;
    }
    Ok(out)
}

fn dsa_from(p: &[u8], q: &[u8], g: &[u8], y: &[u8], x: &[u8]) -> KeyPair {
    let mut body = Vec::new();
    for n in [&[0u8][..], p, q, g, y, x] {
        crate::keys::der_int(&mut body, n);
    }
    let mut der = vec![0x30];
    crate::keys::der_len(&mut der, body.len());
    der.extend_from_slice(&body);
    let mut blob = Vec::new();
    crate::keys::put_str(&mut blob, b"ssh-dss");
    for n in [p, q, g, y] {
        crate::keys::put_mpint(&mut blob, n);
    }
    KeyPair {
        kind: KeyKind::Dsa1024,
        private_pem: crate::keys::pem("DSA PRIVATE KEY", &der),
        blob,
    }
}

fn pem_body(text: &str, label: &str) -> Result<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text.find(&begin).ok_or_else(|| bad("no key"))? + begin.len();
    let stop = text[start..]
        .find(&end)
        .ok_or_else(|| bad("the key is cut short"))?
        + start;
    let body = &text[start..stop];
    if body.contains("ENCRYPTED") {
        return Err(passphrase());
    }
    // Header lines (`Proc-Type: ...`) carry a colon; the base64 does not.
    b64(&body
        .lines()
        .filter(|l| !l.contains(':'))
        .collect::<Vec<_>>()
        .join(""))
}

fn passphrase() -> Error {
    bad("the key has a passphrase, which legcon cannot use yet; remove it with `ssh-keygen -p` (or PuTTYgen) on a copy and import that")
}

fn unsupported(alg: &str) -> Error {
    bad(format!(
        "{alg} keys; 1990s servers only take ssh-rsa and ssh-dss keys"
    ))
}

fn openssh_v1(text: &str) -> Result<KeyPair> {
    let data = pem_body(text, "OPENSSH PRIVATE KEY")?;
    let magic = b"openssh-key-v1\0";
    let rest = data
        .strip_prefix(&magic[..])
        .ok_or_else(|| bad("not an OpenSSH key"))?;
    let mut c = Cursor::new(rest);
    let cipher = c.bytes()?;
    c.bytes()?; // kdf name
    c.bytes()?; // kdf options
    if cipher != b"none" {
        return Err(passphrase());
    }
    if c.u32()? != 1 {
        return Err(bad("an OpenSSH file with several keys"));
    }
    c.bytes()?; // public key
    let private = c.bytes()?;
    let mut p = Cursor::new(private);
    if p.u32()? != p.u32()? {
        return Err(bad("the OpenSSH key is damaged"));
    }
    let alg = String::from_utf8_lossy(p.bytes()?).into_owned();
    match alg.as_str() {
        "ssh-rsa" => {
            let (n, e, d, _iqmp, pp, q) = (
                p.bytes()?,
                p.bytes()?,
                p.bytes()?,
                p.bytes()?,
                p.bytes()?,
                p.bytes()?,
            );
            rsa_from(n, e, d, pp, q)
        }
        "ssh-dss" => {
            let (pp, q, g, y, x) = (p.bytes()?, p.bytes()?, p.bytes()?, p.bytes()?, p.bytes()?);
            Ok(dsa_from(pp, q, g, y, x))
        }
        other => Err(unsupported(other)),
    }
}

fn putty(text: &str) -> Result<KeyPair> {
    let mut alg = "";
    let mut public = String::new();
    let mut private = String::new();
    let mut lines = text.lines();
    while let Some(l) = lines.next() {
        let (k, v) = l.split_once(':').map_or((l, ""), |(k, v)| (k, v.trim()));
        let take = |lines: &mut std::str::Lines, n: &str| -> String {
            let n: usize = n.parse().unwrap_or(0);
            lines.take(n).collect::<Vec<_>>().join("")
        };
        match k {
            _ if k.starts_with("PuTTY-User-Key-File-") => alg = v,
            "Encryption" if v != "none" => return Err(passphrase()),
            "Public-Lines" => public = take(&mut lines, v),
            "Private-Lines" => private = take(&mut lines, v),
            _ => {}
        }
    }
    let blob = b64(&public)?;
    let priv_blob = b64(&private)?;
    let mut pb = Cursor::new(&blob);
    let mut pr = Cursor::new(&priv_blob);
    pb.bytes()?;
    match alg {
        "ssh-rsa" => {
            let (e, n) = (pb.bytes()?, pb.bytes()?);
            let (d, p, q) = (pr.bytes()?, pr.bytes()?, pr.bytes()?);
            rsa_from(n, e, d, p, q)
        }
        "ssh-dss" => {
            let (p, q, g, y) = (pb.bytes()?, pb.bytes()?, pb.bytes()?, pb.bytes()?);
            Ok(dsa_from(p, q, g, y, pr.bytes()?))
        }
        other => Err(unsupported(other)),
    }
}

/// A private key file's text as a key pair legcon can log in with (traditional PEM).
pub fn import(text: &str) -> Result<KeyPair> {
    if text.starts_with("PuTTY-User-Key-File-") {
        return putty(text);
    }
    if text.contains("-----BEGIN OPENSSH PRIVATE KEY-----") {
        return openssh_v1(text);
    }
    if text.contains("-----BEGIN RSA PRIVATE KEY-----") {
        use rsa::pkcs1::DecodeRsaPrivateKey;
        let der = pem_body(text, "RSA PRIVATE KEY")?;
        let k = rsa::RsaPrivateKey::from_pkcs1_der(&der)
            .map_err(|e| bad(format!("not a valid RSA key: {e}")))?;
        return rsa_pair(k);
    }
    if text.contains("-----BEGIN DSA PRIVATE KEY-----") {
        let v = der_ints(&pem_body(text, "DSA PRIVATE KEY")?)?;
        if v.len() < 6 {
            return Err(bad("not a valid DSA key"));
        }
        return Ok(dsa_from(&v[1], &v[2], &v[3], &v[4], &v[5]));
    }
    if text.contains("-----BEGIN PRIVATE KEY-----") {
        use rsa::pkcs8::DecodePrivateKey;
        let der = pem_body(text, "PRIVATE KEY")?;
        let k = rsa::RsaPrivateKey::from_pkcs8_der(&der)
            .map_err(|_| bad("a PKCS#8 key that is not RSA; convert it with ssh-keygen first"))?;
        return rsa_pair(k);
    }
    if text.contains("ENCRYPTED PRIVATE KEY") {
        return Err(passphrase());
    }
    if text.contains("BEGIN EC PRIVATE KEY") {
        return Err(unsupported("ECDSA"));
    }
    if text.trim_start().starts_with("ssh-") || text.contains("BEGIN SSH2 PUBLIC KEY") {
        return Err(bad(
            "this is a public key; import the private half (the file without .pub)",
        ));
    }
    Err(bad(
        "not a private key legcon knows (PEM, OpenSSH or PuTTY)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{generate, openssh_line};

    #[test]
    fn generated_keys_import_back_unchanged() {
        for kind in [KeyKind::Dsa1024, KeyKind::Rsa2048] {
            let k = generate(kind).unwrap();
            let back = import(&k.private_pem).unwrap();
            assert_eq!(back.blob, k.blob);
            let info = describe(&k.blob).unwrap();
            assert_eq!(
                info.bits,
                if kind == KeyKind::Dsa1024 { 1024 } else { 2048 }
            );
            assert!(info.sha256.starts_with("SHA256:"));
            assert_eq!(info.md5.len(), 47);
            let (blob, comment) = parse_public_line(&openssh_line(&k.blob, "a b")).unwrap();
            assert_eq!((blob, comment.as_str()), (k.blob.clone(), "a b"));
        }
    }

    #[test]
    fn an_openssh_format_dsa_key_converts() {
        let k = generate(KeyKind::Dsa1024).unwrap();
        let v = der_ints(&pem_body(&k.private_pem, "DSA PRIVATE KEY").unwrap()).unwrap();
        // Build an unencrypted openssh-key-v1 file around the same numbers.
        let mut private = Vec::new();
        private.extend_from_slice(&7u32.to_be_bytes());
        private.extend_from_slice(&7u32.to_be_bytes());
        crate::keys::put_str(&mut private, b"ssh-dss");
        for n in &v[1..6] {
            crate::keys::put_mpint(&mut private, n);
        }
        crate::keys::put_str(&mut private, b"comment");
        let mut body = b"openssh-key-v1\0".to_vec();
        for s in [&b"none"[..], b"none", b""] {
            crate::keys::put_str(&mut body, s);
        }
        body.extend_from_slice(&1u32.to_be_bytes());
        crate::keys::put_str(&mut body, &k.blob);
        crate::keys::put_str(&mut body, &private);
        let text = crate::keys::pem("OPENSSH PRIVATE KEY", &body);
        assert_eq!(import(&text).unwrap().blob, k.blob);
    }

    #[test]
    fn ssh1_identity_files_hold_together() {
        use rsa::BigUint;
        let (file, line) = crate::keys::generate_ssh1(KeyKind::Ssh1Rsa1024, "dani@sun").unwrap();
        assert_eq!(ssh1_identity(&file).unwrap().unwrap(), line);
        let (e, n, comment) = parse_ssh1_line(&line).unwrap();
        assert_eq!(comment, "dani@sun");
        assert_eq!(describe_ssh1(&e, &n).bits, 1024);
        // The secret part: check bytes twice, then d, iqmp, q, p with n = p * q.
        let mut c = Cursor::new(&file[SSH1_ID.len() + 9..]);
        let mut mp = || {
            let b = u16::from_be_bytes([c.u8().unwrap(), c.u8().unwrap()]) as usize;
            BigUint::from_bytes_be(
                &(0..b.div_ceil(8))
                    .map(|_| c.u8().unwrap())
                    .collect::<Vec<_>>(),
            )
        };
        let (n2, _e2) = (mp(), mp());
        assert_eq!(n2.to_bytes_be(), n);
        let rest_at = SSH1_ID.len() + 9 + 2 + n.len() + 2 + e.len() + 4 + comment.len();
        let mut c = Cursor::new(&file[rest_at..]);
        let check: Vec<u8> = (0..4).map(|_| c.u8().unwrap()).collect();
        assert_eq!((check[0], check[1]), (check[2], check[3]));
        let mut mp = || {
            let b = u16::from_be_bytes([c.u8().unwrap(), c.u8().unwrap()]) as usize;
            BigUint::from_bytes_be(
                &(0..b.div_ceil(8))
                    .map(|_| c.u8().unwrap())
                    .collect::<Vec<_>>(),
            )
        };
        let (_d, iqmp, q, p) = (mp(), mp(), mp(), mp());
        assert_eq!(&p * &q, BigUint::from_bytes_be(&n));
        assert_eq!((&q * &iqmp) % &p, BigUint::from(1u8));
        assert_eq!((file.len() - rest_at) % 8, 0);
    }

    #[test]
    fn what_cannot_be_used_says_why() {
        let enc = "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\nAAAA\n-----END RSA PRIVATE KEY-----\n";
        assert!(import(enc)
            .err()
            .unwrap()
            .to_string()
            .contains("passphrase"));
        assert!(import("ssh-rsa AAAA x")
            .err()
            .unwrap()
            .to_string()
            .contains("public key"));
        assert!(import("hello").is_err());
    }
}
