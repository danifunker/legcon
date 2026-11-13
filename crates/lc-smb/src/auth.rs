//! Challenge / response logins of the 1990s: LM, NTLM (v1), and NTLMv2 for Samba setups that
//! refuse v1 (MS-NLMP 3.3). Windows 9x knows only LM; NT 4 before SP4 only LM and NTLM.

use des::cipher::{BlockEncrypt, KeyInit};
use des::Des;
use hmac::{Hmac, Mac};
use md4::{Digest, Md4};
use md5::Md5;

/// Spread 7 key bytes over 8, leaving the DES parity bits (ignored by the cipher) clear.
fn des_key(k7: &[u8]) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[0] = k7[0];
    k[1] = (k7[0] << 7) | (k7[1] >> 1);
    k[2] = (k7[1] << 6) | (k7[2] >> 2);
    k[3] = (k7[2] << 5) | (k7[3] >> 3);
    k[4] = (k7[3] << 4) | (k7[4] >> 4);
    k[5] = (k7[4] << 3) | (k7[5] >> 5);
    k[6] = (k7[5] << 2) | (k7[6] >> 6);
    k[7] = k7[6] << 1;
    k
}

fn des(k7: &[u8], block: &[u8; 8]) -> [u8; 8] {
    let cipher = Des::new_from_slice(&des_key(k7)).expect("an 8-byte DES key");
    let mut b = (*block).into();
    cipher.encrypt_block(&mut b);
    b.into()
}

/// The LM hash: the password upper-cased in the OEM set, cut to 14 bytes, as two DES keys.
pub fn lm_hash(oem_password: &[u8]) -> [u8; 16] {
    let mut p = [0u8; 14];
    for (i, b) in oem_password.iter().take(14).enumerate() {
        p[i] = b.to_ascii_uppercase();
    }
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&des(&p[..7], b"KGS!@#$%"));
    out[8..].copy_from_slice(&des(&p[7..], b"KGS!@#$%"));
    out
}

/// The NT hash: MD4 of the password in UTF-16LE.
pub fn nt_hash(password: &str) -> [u8; 16] {
    let utf16: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    Md4::digest(&utf16).into()
}

/// A v1 response: the 16-byte hash padded to 21 and used as three DES keys on the challenge.
pub fn v1_response(hash: &[u8; 16], challenge: &[u8; 8]) -> [u8; 24] {
    let mut k = [0u8; 21];
    k[..16].copy_from_slice(hash);
    let mut out = [0u8; 24];
    for i in 0..3 {
        out[i * 8..i * 8 + 8].copy_from_slice(&des(&k[i * 7..i * 7 + 7], challenge));
    }
    out
}

fn hmac_md5(key: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut m = <Hmac<Md5> as Mac>::new_from_slice(key).expect("HMAC takes any key");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// NTLMv2 responses (LMv2, NTv2) for `user` in `domain`, with this client challenge and time.
pub fn v2_responses(
    password: &str,
    user: &str,
    domain: &str,
    challenge: &[u8; 8],
    client: &[u8; 8],
    filetime: u64,
) -> ([u8; 24], Vec<u8>) {
    let who: Vec<u8> = format!("{}{domain}", user.to_uppercase())
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let key = hmac_md5(&nt_hash(password), &[&who]);
    let mut lm = [0u8; 24];
    lm[..16].copy_from_slice(&hmac_md5(&key, &[challenge, client]));
    lm[16..].copy_from_slice(client);
    let mut blob = vec![1, 1, 0, 0, 0, 0, 0, 0];
    blob.extend(filetime.to_le_bytes());
    blob.extend(client);
    blob.extend([0u8; 8]);
    let proof = hmac_md5(&key, &[challenge, &blob]);
    let mut nt = proof.to_vec();
    nt.extend(blob);
    (lm, nt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // MS-NLMP 4.2.2 test vectors: password "Password", challenge 0123456789abcdef.
    const CHALLENGE: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];

    #[test]
    fn lm_and_nt_hashes_match_ms_nlmp() {
        assert_eq!(
            hex(&lm_hash(b"Password")),
            "e52cac67419a9a224a3b108f3fa6cb6d"
        );
        assert_eq!(
            hex(&nt_hash("Password")),
            "a4f49c406510bdcab6824ee7c30fd852"
        );
    }

    #[test]
    fn v1_responses_match_ms_nlmp() {
        assert_eq!(
            hex(&v1_response(&nt_hash("Password"), &CHALLENGE)),
            "67c43011f30298a2ad35ece64f16331c44bdbed927841f94"
        );
        assert_eq!(
            hex(&v1_response(&lm_hash(b"Password"), &CHALLENGE)),
            "98def7b87f88aa5dafe2df779688a172def11c7d5ccdef13"
        );
    }

    #[test]
    fn lmv2_matches_ms_nlmp() {
        let client = [0xaa; 8];
        let (lm, nt) = v2_responses("Password", "User", "Domain", &CHALLENGE, &client, 0);
        assert_eq!(hex(&lm), "86c35097ac9cec102554764a57cccc19aaaaaaaaaaaaaaaa");
        assert_eq!(nt.len(), 16 + 8 + 8 + 8 + 8);
    }
}
