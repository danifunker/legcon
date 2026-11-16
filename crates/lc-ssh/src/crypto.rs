//! The SSH-2 algorithms legcon speaks, all pure Rust (RustCrypto): ciphers and MACs for the
//! packet layer, the key exchanges, host key checks and the signatures key login sends.
//! Old ones (`3des-cbc`, `arcfour`, `hmac-md5`, `diffie-hellman-group1-sha1`, `ssh-dss`) are
//! offered after the modern ones, so they are used only when a server has nothing newer.

use aes::cipher::{
    generic_array::GenericArray, BlockDecryptMut, BlockEncryptMut, InnerIvInit, KeyInit, KeyIvInit,
    StreamCipher, StreamCipherSeek,
};
use hmac::Hmac;
use lc_core::{Error, Result};
use rand::rngs::OsRng;
use rand::RngCore;
use rsa::BigUint;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::wire::{put_mpint, put_string, Reader};

/// Our preferences, best first.
pub const KEX: &[&str] = &[
    "curve25519-sha256",
    "curve25519-sha256@libssh.org",
    "diffie-hellman-group14-sha256",
    "diffie-hellman-group-exchange-sha256",
    "diffie-hellman-group14-sha1",
    "diffie-hellman-group-exchange-sha1",
    "diffie-hellman-group1-sha1",
];
pub const HOST_KEY: &[&str] = &[
    "ssh-ed25519",
    "rsa-sha2-512",
    "rsa-sha2-256",
    "ssh-rsa",
    "ssh-dss",
];
pub const CIPHER: &[&str] = &[
    // First, as in OpenSSH: far lighter than AES for a server without AES instructions.
    CHACHA,
    "aes128-ctr",
    "aes192-ctr",
    "aes256-ctr",
    "aes128-cbc",
    "aes192-cbc",
    "aes256-cbc",
    "3des-cbc",
    "blowfish-cbc",
    "cast128-cbc",
    "arcfour256",
    "arcfour128",
    "arcfour",
];
/// The OpenSSH AEAD cipher (PROTOCOL.chacha20poly1305): its MAC is built in.
pub const CHACHA: &str = "chacha20-poly1305@openssh.com";

pub const MAC: &[&str] = &[
    "hmac-sha2-256",
    "hmac-sha2-512",
    "hmac-sha1",
    "hmac-sha1-96",
    "hmac-md5",
    "hmac-md5-96",
];

fn bad(what: impl Into<String>) -> Error {
    Error::Protocol(what.into())
}

/// A hash a key exchange is defined over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha256,
    Sha512,
}

impl Hash {
    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => Sha1::digest(data).to_vec(),
            Hash::Sha256 => Sha256::digest(data).to_vec(),
            Hash::Sha512 => Sha512::digest(data).to_vec(),
        }
    }
}

/// One direction's cipher, keeping its state (CBC chaining, CTR counter, RC4 stream) across packets.
pub enum Cipher {
    None,
    Aes128Ctr(Box<ctr::Ctr128BE<aes::Aes128>>),
    Aes192Ctr(Box<ctr::Ctr128BE<aes::Aes192>>),
    Aes256Ctr(Box<ctr::Ctr128BE<aes::Aes256>>),
    Rc4(Box<rc4::Rc4<rc4::consts::U16>>),
    Rc4x32(Box<rc4::Rc4<rc4::consts::U32>>),
    Cbc(Box<dyn CbcCipher>),
}

/// A CBC block cipher in one direction.
pub trait CbcCipher: Send {
    fn apply(&mut self, data: &mut [u8]);
}

struct Enc<C: BlockEncryptMut>(C);
struct Dec<C: BlockDecryptMut>(C);

impl<C: BlockEncryptMut + Send> CbcCipher for Enc<C> {
    fn apply(&mut self, data: &mut [u8]) {
        for b in data.chunks_exact_mut(C::block_size()) {
            self.0.encrypt_block_mut(GenericArray::from_mut_slice(b));
        }
    }
}

impl<C: BlockDecryptMut + Send> CbcCipher for Dec<C> {
    fn apply(&mut self, data: &mut [u8]) {
        for b in data.chunks_exact_mut(C::block_size()) {
            self.0.decrypt_block_mut(GenericArray::from_mut_slice(b));
        }
    }
}

/// Key and IV lengths of a cipher, and its block size (8 for a stream cipher).
pub fn cipher_sizes(name: &str) -> (usize, usize, usize) {
    match name {
        "aes128-ctr" | "aes128-cbc" => (16, 16, 16),
        "aes192-ctr" | "aes192-cbc" => (24, 16, 16),
        "aes256-ctr" | "aes256-cbc" => (32, 16, 16),
        "3des-cbc" => (24, 8, 8),
        "blowfish-cbc" | "cast128-cbc" => (16, 8, 8),
        "arcfour" | "arcfour128" => (16, 0, 8),
        "arcfour256" => (32, 0, 8),
        CHACHA => (64, 0, 8),
        _ => (0, 0, 8),
    }
}

fn cbc<C>(key: &[u8], iv: &[u8], encrypt: bool) -> Result<Box<dyn CbcCipher>>
where
    C: aes::cipher::BlockCipher + aes::cipher::BlockEncrypt + aes::cipher::BlockDecrypt,
    C: aes::cipher::KeyInit + Send + 'static,
{
    let c = C::new_from_slice(key).map_err(|_| bad("bad cipher key length"))?;
    Ok(if encrypt {
        Box::new(Enc(
            cbc::Encryptor::<C>::inner_iv_slice_init(c, iv).map_err(|_| bad("bad IV length"))?
        ))
    } else {
        Box::new(Dec(
            cbc::Decryptor::<C>::inner_iv_slice_init(c, iv).map_err(|_| bad("bad IV length"))?
        ))
    })
}

impl Cipher {
    pub fn new(name: &str, key: &[u8], iv: &[u8], encrypt: bool) -> Result<Cipher> {
        let ctr_err = |_| bad("bad cipher key length");
        Ok(match name {
            "aes128-ctr" => Cipher::Aes128Ctr(Box::new(
                ctr::Ctr128BE::new_from_slices(key, iv).map_err(ctr_err)?,
            )),
            "aes192-ctr" => Cipher::Aes192Ctr(Box::new(
                ctr::Ctr128BE::new_from_slices(key, iv).map_err(ctr_err)?,
            )),
            "aes256-ctr" => Cipher::Aes256Ctr(Box::new(
                ctr::Ctr128BE::new_from_slices(key, iv).map_err(ctr_err)?,
            )),
            "aes128-cbc" => Cipher::Cbc(cbc::<aes::Aes128>(key, iv, encrypt)?),
            "aes192-cbc" => Cipher::Cbc(cbc::<aes::Aes192>(key, iv, encrypt)?),
            "aes256-cbc" => Cipher::Cbc(cbc::<aes::Aes256>(key, iv, encrypt)?),
            "3des-cbc" => Cipher::Cbc(cbc::<des::TdesEde3>(key, iv, encrypt)?),
            "blowfish-cbc" => Cipher::Cbc(cbc::<blowfish::Blowfish>(key, iv, encrypt)?),
            "cast128-cbc" => Cipher::Cbc(cbc::<cast5::Cast5>(key, iv, encrypt)?),
            "arcfour" | "arcfour128" => {
                let mut c = rc4::Rc4::<rc4::consts::U16>::new_from_slice(key)
                    .map_err(|_| bad("bad cipher key length"))?;
                if name == "arcfour128" {
                    // RFC 4345: drop the first 1536 bytes of the key stream.
                    c.apply_keystream(&mut [0u8; 1536]);
                }
                Cipher::Rc4(Box::new(c))
            }
            "arcfour256" => {
                let mut c = rc4::Rc4::<rc4::consts::U32>::new_from_slice(key)
                    .map_err(|_| bad("bad cipher key length"))?;
                c.apply_keystream(&mut [0u8; 1536]);
                Cipher::Rc4x32(Box::new(c))
            }
            other => return Err(bad(format!("cipher {other} is not supported"))),
        })
    }

    /// Encrypt or decrypt `data` in place (a whole number of blocks).
    pub fn apply(&mut self, data: &mut [u8]) {
        match self {
            Cipher::None => {}
            Cipher::Aes128Ctr(c) => c.apply_keystream(data),
            Cipher::Aes192Ctr(c) => c.apply_keystream(data),
            Cipher::Aes256Ctr(c) => c.apply_keystream(data),
            Cipher::Rc4(c) => c.apply_keystream(data),
            Cipher::Rc4x32(c) => c.apply_keystream(data),
            Cipher::Cbc(c) => c.apply(data),
        }
    }
}

/// chacha20-poly1305@openssh.com for one direction: the length sealed with one key, the
/// packet with the other, and a Poly1305 tag over both.
pub struct ChaChaPoly {
    main: [u8; 32],
    header: [u8; 32],
}

impl ChaChaPoly {
    pub fn new(key: &[u8]) -> Result<ChaChaPoly> {
        if key.len() != 64 {
            return Err(bad("bad cipher key length"));
        }
        let mut c = ChaChaPoly {
            main: [0; 32],
            header: [0; 32],
        };
        c.main.copy_from_slice(&key[..32]);
        c.header.copy_from_slice(&key[32..]);
        Ok(c)
    }

    fn stream(key: &[u8; 32], seq: u32) -> chacha20::ChaCha20Legacy {
        let nonce = u64::from(seq).to_be_bytes();
        chacha20::ChaCha20Legacy::new(key.into(), &nonce.into())
    }

    /// The packet length from its first four (sealed) bytes.
    pub fn length(&self, seq: u32, sealed: &[u8]) -> u32 {
        let mut len = [sealed[0], sealed[1], sealed[2], sealed[3]];
        Self::stream(&self.header, seq).apply_keystream(&mut len);
        u32::from_be_bytes(len)
    }

    fn tag(&self, seq: u32, data: &[u8]) -> [u8; 16] {
        let mut key = [0u8; 32];
        Self::stream(&self.main, seq).apply_keystream(&mut key);
        poly1305::Poly1305::new((&key).into())
            .compute_unpadded(data)
            .into()
    }

    /// Encrypt a whole packet (length first) in place and append its tag.
    pub fn seal(&self, seq: u32, pkt: &mut Vec<u8>) {
        Self::stream(&self.header, seq).apply_keystream(&mut pkt[..4]);
        let mut s = Self::stream(&self.main, seq);
        s.seek(64u64);
        s.apply_keystream(&mut pkt[4..]);
        let tag = self.tag(seq, pkt);
        pkt.extend_from_slice(&tag);
    }

    /// Check a sealed packet's tag, then decrypt all but its length in place.
    pub fn open(&self, seq: u32, pkt: &mut [u8], tag: &[u8]) -> bool {
        use subtle::ConstantTimeEq;
        if !bool::from(self.tag(seq, pkt).ct_eq(tag)) {
            return false;
        }
        let mut s = Self::stream(&self.main, seq);
        s.seek(64u64);
        s.apply_keystream(&mut pkt[4..]);
        true
    }
}

/// One direction's MAC.
pub struct Mac {
    kind: MacKind,
    key: Vec<u8>,
    /// Bytes sent: the `-96` MACs send the first 12.
    len: usize,
}

#[derive(Clone, Copy)]
enum MacKind {
    Sha1,
    Md5,
    Sha256,
    Sha512,
}

/// Key length of a MAC.
pub fn mac_key_len(name: &str) -> usize {
    match name {
        "hmac-sha1" | "hmac-sha1-96" => 20,
        "hmac-md5" | "hmac-md5-96" => 16,
        "hmac-sha2-256" => 32,
        "hmac-sha2-512" => 64,
        _ => 0,
    }
}

impl Mac {
    pub fn new(name: &str, key: &[u8]) -> Result<Mac> {
        let (kind, len) = match name {
            "hmac-sha1" => (MacKind::Sha1, 20),
            "hmac-sha1-96" => (MacKind::Sha1, 12),
            "hmac-md5" => (MacKind::Md5, 16),
            "hmac-md5-96" => (MacKind::Md5, 12),
            "hmac-sha2-256" => (MacKind::Sha256, 32),
            "hmac-sha2-512" => (MacKind::Sha512, 64),
            other => return Err(bad(format!("MAC {other} is not supported"))),
        };
        Ok(Mac {
            kind,
            key: key.to_vec(),
            len,
        })
    }

    /// How many bytes of MAC follow each packet.
    pub fn size(&self) -> usize {
        self.len
    }

    /// The MAC of packet `seq`, cut to its length.
    pub fn sign(&self, seq: u32, packet: &[u8]) -> Vec<u8> {
        fn run<M: hmac::Mac + KeyInit>(key: &[u8], seq: u32, packet: &[u8]) -> Vec<u8> {
            let mut m = <M as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
            m.update(&seq.to_be_bytes());
            m.update(packet);
            m.finalize().into_bytes().to_vec()
        }
        let mut out = match self.kind {
            MacKind::Sha1 => run::<Hmac<Sha1>>(&self.key, seq, packet),
            MacKind::Md5 => run::<Hmac<md5::Md5>>(&self.key, seq, packet),
            MacKind::Sha256 => run::<Hmac<Sha256>>(&self.key, seq, packet),
            MacKind::Sha512 => run::<Hmac<Sha512>>(&self.key, seq, packet),
        };
        out.truncate(self.len);
        out
    }
}

/// Oakley group 2 (RFC 2409), `diffie-hellman-group1-sha1`.
const GROUP1: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE65381FFFFFFFFFFFFFFFF";
/// The 2048-bit MODP group (RFC 3526), `diffie-hellman-group14-*`.
const GROUP14: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AACAA68FFFFFFFFFFFFFFFF";

pub fn group1() -> BigUint {
    BigUint::parse_bytes(GROUP1.as_bytes(), 16).expect("a valid prime")
}

pub fn group14() -> BigUint {
    BigUint::parse_bytes(GROUP14.as_bytes(), 16).expect("a valid prime")
}

/// One Diffie-Hellman exchange: our secret and public value in group (p, g).
pub struct Dh {
    p: BigUint,
    x: BigUint,
    pub e: BigUint,
}

impl Dh {
    pub fn new(p: BigUint, g: &BigUint) -> Dh {
        // A 512-bit exponent: well over twice the strength of the largest group used here.
        let mut b = [0u8; 64];
        OsRng.fill_bytes(&mut b);
        let x = BigUint::from_bytes_be(&b) | BigUint::from(1u8);
        let e = g.modpow(&x, &p);
        Dh { p, x, e }
    }

    /// The shared secret from the server's `f`, which must lie strictly between 1 and p - 1.
    pub fn shared(&self, f: &BigUint) -> Result<BigUint> {
        let one = BigUint::from(1u8);
        if *f <= one || *f >= &self.p - &one {
            return Err(bad("the server sent an invalid key exchange value"));
        }
        Ok(f.modpow(&self.x, &self.p))
    }
}

/// A curve25519 exchange: our secret and our public point.
pub struct Curve {
    secret: x25519_dalek::StaticSecret,
    pub public: [u8; 32],
}

impl Default for Curve {
    fn default() -> Self {
        Curve::new()
    }
}

impl Curve {
    pub fn new() -> Curve {
        let secret = x25519_dalek::StaticSecret::random_from_rng(OsRng);
        let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
        Curve { secret, public }
    }

    /// The shared secret as the big-endian number RFC 8731 makes of it.
    pub fn shared(&self, server: &[u8]) -> Result<BigUint> {
        let q: [u8; 32] = server
            .try_into()
            .map_err(|_| bad("the server's curve25519 value has the wrong length"))?;
        let k = self
            .secret
            .diffie_hellman(&x25519_dalek::PublicKey::from(q));
        if k.as_bytes().iter().all(|b| *b == 0) {
            return Err(bad("the server sent an invalid curve25519 value"));
        }
        Ok(BigUint::from_bytes_be(k.as_bytes()))
    }
}

fn mp(r: &mut Reader) -> Result<BigUint> {
    Ok(BigUint::from_bytes_be(r.string()?))
}

/// Check the host key's signature `sig` over the exchange hash `h`.
pub fn verify_host(blob: &[u8], sig: &[u8], h: &[u8]) -> Result<()> {
    let mut k = Reader::new(blob);
    let kind = k.string()?.to_vec();
    let mut s = Reader::new(sig);
    let sig_kind = s.string()?.to_vec();
    let sig = s.string()?;
    let wrong = || bad("the server's host key signature does not match");
    match kind.as_slice() {
        b"ssh-ed25519" => {
            let key: [u8; 32] = k.string()?.try_into().map_err(|_| wrong())?;
            let key = ed25519_dalek::VerifyingKey::from_bytes(&key).map_err(|_| wrong())?;
            let sig = ed25519_dalek::Signature::from_slice(sig).map_err(|_| wrong())?;
            key.verify_strict(h, &sig).map_err(|_| wrong())
        }
        b"ssh-rsa" => {
            let (e, n) = (mp(&mut k)?, mp(&mut k)?);
            let key = rsa::RsaPublicKey::new_with_max_size(n, e, 16384).map_err(|_| wrong())?;
            let (scheme, digest) = match sig_kind.as_slice() {
                b"ssh-rsa" => (rsa::Pkcs1v15Sign::new::<Sha1>(), Hash::Sha1.digest(h)),
                b"rsa-sha2-256" => (rsa::Pkcs1v15Sign::new::<Sha256>(), Hash::Sha256.digest(h)),
                b"rsa-sha2-512" => (rsa::Pkcs1v15Sign::new::<Sha512>(), Hash::Sha512.digest(h)),
                _ => return Err(wrong()),
            };
            key.verify(scheme, &digest, sig).map_err(|_| wrong())
        }
        b"ssh-dss" => {
            use dsa::signature::hazmat::PrehashVerifier;
            let (p, q, g, y) = (mp(&mut k)?, mp(&mut k)?, mp(&mut k)?, mp(&mut k)?);
            if sig.len() != 40 {
                return Err(wrong());
            }
            let c = dsa::Components::from_components(p, q, g).map_err(|_| wrong())?;
            let key = dsa::VerifyingKey::from_components(c, y).map_err(|_| wrong())?;
            let sig = dsa::Signature::from_components(
                BigUint::from_bytes_be(&sig[..20]),
                BigUint::from_bytes_be(&sig[20..]),
            )
            .map_err(|_| wrong())?;
            key.verify_prehash(&Hash::Sha1.digest(h), &sig)
                .map_err(|_| wrong())
        }
        other => Err(bad(format!(
            "host key type {} is not supported",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// A key legcon logs in with, from the traditional PEM file `keys` writes.
pub enum Signer {
    Rsa(Box<rsa::RsaPrivateKey>),
    Dsa(Box<dsa::SigningKey>),
    /// An ed25519 key from `~/.ssh` (legcon makes none itself).
    Ed25519(Box<ed25519_dalek::SigningKey>),
}

impl Signer {
    /// The public key blob the server knows the key by.
    pub fn blob(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Signer::Rsa(k) => {
                use rsa::traits::PublicKeyParts;
                put_string(&mut b, b"ssh-rsa");
                put_mpint(&mut b, &k.e().to_bytes_be());
                put_mpint(&mut b, &k.n().to_bytes_be());
            }
            Signer::Dsa(k) => {
                let v = k.verifying_key();
                let c = v.components();
                put_string(&mut b, b"ssh-dss");
                for n in [c.p(), c.q(), c.g(), v.y()] {
                    put_mpint(&mut b, &n.to_bytes_be());
                }
            }
            Signer::Ed25519(k) => {
                put_string(&mut b, b"ssh-ed25519");
                put_string(&mut b, k.verifying_key().as_bytes());
            }
        }
        b
    }

    /// The signature algorithm to use: SHA-2 RSA when the server says it takes it.
    pub fn algorithm(&self, server_sig_algs: &[String]) -> &'static str {
        match self {
            Signer::Dsa(_) => "ssh-dss",
            Signer::Ed25519(_) => "ssh-ed25519",
            Signer::Rsa(_) if server_sig_algs.iter().any(|a| a == "rsa-sha2-512") => "rsa-sha2-512",
            Signer::Rsa(_) if server_sig_algs.iter().any(|a| a == "rsa-sha2-256") => "rsa-sha2-256",
            Signer::Rsa(_) => "ssh-rsa",
        }
    }

    /// The signature blob over `data` with `algorithm`.
    pub fn sign(&self, algorithm: &str, data: &[u8]) -> Result<Vec<u8>> {
        let fail = |e: String| bad(format!("could not sign with the key: {e}"));
        let raw = match self {
            Signer::Rsa(k) => {
                let (scheme, digest) = match algorithm {
                    "rsa-sha2-512" => (
                        rsa::Pkcs1v15Sign::new::<Sha512>(),
                        Hash::Sha512.digest(data),
                    ),
                    "rsa-sha2-256" => (
                        rsa::Pkcs1v15Sign::new::<Sha256>(),
                        Hash::Sha256.digest(data),
                    ),
                    _ => (rsa::Pkcs1v15Sign::new::<Sha1>(), Hash::Sha1.digest(data)),
                };
                k.sign(scheme, &digest).map_err(|e| fail(e.to_string()))?
            }
            Signer::Dsa(k) => {
                use dsa::signature::hazmat::PrehashSigner;
                let s: dsa::Signature = k
                    .sign_prehash(&Hash::Sha1.digest(data))
                    .map_err(|e| fail(e.to_string()))?;
                // r and s as two 20-byte numbers, left-padded.
                let mut out = vec![0u8; 40];
                let (r, sv) = (s.r().to_bytes_be(), s.s().to_bytes_be());
                out[20 - r.len()..20].copy_from_slice(&r);
                out[40 - sv.len()..].copy_from_slice(&sv);
                out
            }
            Signer::Ed25519(k) => {
                use ed25519_dalek::Signer as _;
                k.sign(data).to_bytes().to_vec()
            }
        };
        let mut b = Vec::new();
        put_string(&mut b, algorithm.as_bytes());
        put_string(&mut b, &raw);
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciphers_round_trip_across_packets() {
        for name in CIPHER.iter().filter(|c| **c != CHACHA) {
            let (k, iv, block) = cipher_sizes(name);
            let key = vec![7u8; k];
            let iv = vec![9u8; iv];
            let mut enc = Cipher::new(name, &key, &iv, true).unwrap();
            let mut dec = Cipher::new(name, &key, &iv, false).unwrap();
            for round in 0..3u8 {
                let plain: Vec<u8> = (0..block * 4).map(|i| i as u8 ^ round).collect();
                let mut data = plain.clone();
                enc.apply(&mut data);
                assert_ne!(data, plain, "{name}");
                dec.apply(&mut data);
                assert_eq!(data, plain, "{name} round {round}");
            }
        }
    }

    #[test]
    fn chacha20_poly1305_seals_opens_and_catches_tampering() {
        let key: Vec<u8> = (0..64).collect();
        let c = ChaChaPoly::new(&key).unwrap();
        let plain: Vec<u8> = [&28u32.to_be_bytes()[..], &[4u8], &[1u8; 23], &[0u8; 4]].concat();
        let mut pkt = plain.clone();
        c.seal(5, &mut pkt);
        assert_eq!(pkt.len(), plain.len() + 16);
        assert_eq!(c.length(5, &pkt), 28);
        let (body, tag) = pkt.split_at(plain.len());
        let mut wrong = body.to_vec();
        assert!(!c.open(6, &mut wrong, tag), "another sequence number");
        wrong[10] ^= 1;
        assert!(!c.open(5, &mut wrong, tag), "a changed byte");
        let mut good = body.to_vec();
        assert!(c.open(5, &mut good, tag));
        assert_eq!(good[4..], plain[4..]);
    }

    #[test]
    fn hmac_sha1_matches_rfc_2202() {
        use hmac::Mac as _;
        // Test case 2: key "Jefe", data "what do ya want for nothing?" (here after a zero seq).
        let m = Mac::new("hmac-sha1", b"Jefe").unwrap();
        let mut h = <Hmac<Sha1> as hmac::Mac>::new_from_slice(b"Jefe").unwrap();
        h.update(&0u32.to_be_bytes());
        h.update(b"what do ya want for nothing?");
        assert_eq!(
            m.sign(0, b"what do ya want for nothing?"),
            h.finalize().into_bytes().to_vec()
        );
        assert_eq!(
            Mac::new("hmac-md5-96", b"k").unwrap().sign(1, b"x").len(),
            12
        );
    }

    #[test]
    fn dh_both_sides_agree() {
        let p = group14();
        let g = BigUint::from(2u8);
        let (a, b) = (Dh::new(p.clone(), &g), Dh::new(p, &g));
        assert_eq!(a.shared(&b.e).unwrap(), b.shared(&a.e).unwrap());
        assert!(a.shared(&BigUint::from(1u8)).is_err());
    }

    #[test]
    fn rsa_and_dsa_signatures_verify_as_host_keys() {
        let k = rsa::RsaPrivateKey::new(&mut OsRng, 1024).unwrap();
        let s = Signer::Rsa(Box::new(k));
        for alg in ["ssh-rsa", "rsa-sha2-256", "rsa-sha2-512"] {
            let sig = s.sign(alg, b"exchange hash").unwrap();
            verify_host(&s.blob(), &sig, b"exchange hash").unwrap();
            assert!(verify_host(&s.blob(), &sig, b"another hash").is_err());
        }
        #[allow(deprecated)]
        let c = dsa::Components::generate(&mut OsRng, dsa::KeySize::DSA_1024_160);
        let s = Signer::Dsa(Box::new(dsa::SigningKey::generate(&mut OsRng, c)));
        let sig = s.sign("ssh-dss", b"exchange hash").unwrap();
        verify_host(&s.blob(), &sig, b"exchange hash").unwrap();
    }
}
