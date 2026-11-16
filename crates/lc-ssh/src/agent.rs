//! An ssh-agent client (draft-miller-ssh-agent): the keys the user's agent holds, and signatures
//! made by it, so legcon logs in with the same keys `ssh` does, passphrase-protected ones too.
//! The agent is `$SSH_AUTH_SOCK` on Unix and macOS, the OpenSSH agent's pipe on Windows.

use std::io::{Read, Write};

use lc_core::{Error, Result};

use crate::wire::{put_string, put_u32, Reader};

const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;
/// Sign flags: SHA-2 RSA signatures.
const RSA_SHA2_256: u32 = 2;
const RSA_SHA2_512: u32 = 4;

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

pub struct Agent {
    stream: Box<dyn Stream>,
}

/// A key the agent holds: its public blob and comment (usually the file it came from).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub blob: Vec<u8>,
    pub comment: String,
}

impl Identity {
    /// The key's type, such as `ssh-ed25519`.
    pub fn kind(&self) -> String {
        Reader::new(&self.blob)
            .string()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .unwrap_or_default()
    }

    /// The signature algorithm to ask for: SHA-2 RSA when the server takes it.
    pub fn algorithm(&self, server_sig_algs: &[String]) -> (String, u32) {
        let kind = self.kind();
        if kind != "ssh-rsa" {
            return (kind, 0);
        }
        let has = |a: &str| server_sig_algs.iter().any(|s| s == a);
        if has("rsa-sha2-512") {
            ("rsa-sha2-512".into(), RSA_SHA2_512)
        } else if has("rsa-sha2-256") {
            ("rsa-sha2-256".into(), RSA_SHA2_256)
        } else {
            ("ssh-rsa".into(), 0)
        }
    }
}

fn bad(what: &str) -> Error {
    Error::Protocol(format!("ssh-agent: {what}"))
}

impl Agent {
    /// The user's running agent, if there is one.
    pub fn connect() -> Option<Agent> {
        #[cfg(unix)]
        {
            let path = std::env::var_os("SSH_AUTH_SOCK")?;
            let s = std::os::unix::net::UnixStream::connect(path).ok()?;
            let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(30)));
            Some(Agent {
                stream: Box::new(s),
            })
        }
        #[cfg(windows)]
        {
            let pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(r"\\.\pipe\openssh-ssh-agent")
                .ok()?;
            Some(Agent {
                stream: Box::new(pipe),
            })
        }
        #[cfg(not(any(unix, windows)))]
        None
    }

    fn call(&mut self, body: &[u8]) -> Result<Vec<u8>> {
        let mut msg = Vec::new();
        put_string(&mut msg, body);
        self.stream.write_all(&msg)?;
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len)?;
        let n = u32::from_be_bytes(len) as usize;
        if n == 0 || n > 256 * 1024 {
            return Err(bad("a malformed answer"));
        }
        let mut reply = vec![0u8; n];
        self.stream.read_exact(&mut reply)?;
        Ok(reply)
    }

    pub fn identities(&mut self) -> Result<Vec<Identity>> {
        let reply = self.call(&[REQUEST_IDENTITIES])?;
        if reply[0] != IDENTITIES_ANSWER {
            return Err(bad("it would not list its keys"));
        }
        let mut r = Reader::new(&reply[1..]);
        let n = r.u32()?;
        let mut out = Vec::new();
        for _ in 0..n.min(64) {
            let blob = r.string()?.to_vec();
            let comment = String::from_utf8_lossy(r.string()?).into_owned();
            out.push(Identity { blob, comment });
        }
        Ok(out)
    }

    /// The agent's signature blob over `data` with key `blob`.
    pub fn sign(&mut self, blob: &[u8], data: &[u8], flags: u32) -> Result<Vec<u8>> {
        let mut body = vec![SIGN_REQUEST];
        put_string(&mut body, blob);
        put_string(&mut body, data);
        put_u32(&mut body, flags);
        let reply = self.call(&body)?;
        if reply[0] != SIGN_RESPONSE {
            return Err(bad(
                "it refused to sign (a key needing confirmation, or locked)",
            ));
        }
        Ok(Reader::new(&reply[1..]).string()?.to_vec())
    }
}
