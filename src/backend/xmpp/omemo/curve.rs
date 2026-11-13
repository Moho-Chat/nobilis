//! Curve25519 as the Signal protocol uses it: key pairs whose public half is written with a `0x05` in front, agreement
//! (X25519), and signatures made with the same key (XEdDSA), so that one identity key both agrees and signs.

use curve25519_dalek::constants::ED25519_BASEPOINT_POINT;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::montgomery::MontgomeryPoint;
use curve25519_dalek::scalar::Scalar;
use rand::RngCore;
use sha2::{Digest, Sha512};

#[derive(Clone, PartialEq, Eq)]
pub struct KeyPair {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyPair({})", hex(&self.public))
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn clamp(mut k: [u8; 32]) -> [u8; 32] {
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    k
}

impl KeyPair {
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        Self::from_private(seed)
    }

    pub fn from_private(seed: [u8; 32]) -> Self {
        let private = clamp(seed);
        let public = x25519_dalek::x25519(private, x25519_dalek::X25519_BASEPOINT_BYTES);
        Self { private, public }
    }

    /// The public key as the protocol writes it: a type byte (5) and the 32 bytes.
    pub fn public_serialized(&self) -> Vec<u8> {
        serialize_public(&self.public)
    }
}

pub fn serialize_public(public: &[u8; 32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(33);
    v.push(0x05);
    v.extend_from_slice(public);
    v
}

/// A public key from its 33-byte form (or the bare 32, which some clients send).
pub fn parse_public(bytes: &[u8]) -> Option<[u8; 32]> {
    let raw: &[u8] = match bytes.len() {
        33 if bytes[0] == 0x05 => &bytes[1..],
        32 => bytes,
        _ => return None,
    };
    raw.try_into().ok()
}

/// What two keys agree on.
pub fn agree(private: &[u8; 32], public: &[u8; 32]) -> [u8; 32] {
    x25519_dalek::x25519(*private, *public)
}

/// Signs with a Curve25519 private key (XEdDSA). The Edwards key's sign bit is made zero by negating the scalar, which is
/// what the specification says and what the other implementations verify.
pub fn sign(private: &[u8; 32], message: &[u8]) -> [u8; 64] {
    let mut a = Scalar::from_bytes_mod_order(clamp(*private));
    let mut public = (ED25519_BASEPOINT_POINT * a).compress().to_bytes();
    if public[31] & 0x80 != 0 {
        a = -a;
        public[31] &= 0x7f;
    }
    let mut z = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut z);
    let mut prefix = [0xFFu8; 32];
    prefix[0] = 0xFE;
    let r = Scalar::from_hash(Sha512::new().chain_update(prefix).chain_update(a.to_bytes()).chain_update(message).chain_update(z));
    let big_r = (ED25519_BASEPOINT_POINT * r).compress();
    let h = Scalar::from_hash(Sha512::new().chain_update(big_r.as_bytes()).chain_update(public).chain_update(message));
    let s = r + h * a;
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(big_r.as_bytes());
    out[32..].copy_from_slice(s.as_bytes());
    out
}

/// Checks a signature. Accepts both ways the sign bit is carried: not at all (zero, as above) or in the top bit of the second
/// half (the way the original Signal library wrote it).
pub fn verify(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let mut s_bytes: [u8; 32] = signature[32..].try_into().expect("32 bytes");
    let sign = (s_bytes[31] >> 7) & 1;
    s_bytes[31] &= 0x7f;
    let Some(a) = MontgomeryPoint(*public).to_edwards(sign) else { return false };
    let s: Option<Scalar> = Scalar::from_canonical_bytes(s_bytes).into();
    let Some(s) = s else { return false };
    let compressed: CompressedEdwardsY = a.compress();
    let h = Scalar::from_hash(Sha512::new().chain_update(&signature[..32]).chain_update(compressed.as_bytes()).chain_update(message));
    let check: EdwardsPoint = EdwardsPoint::vartime_double_scalar_mul_basepoint(&(-h), &a, &s);
    check.compress().as_bytes() == &signature[..32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_keys_agree_on_the_same_secret_from_either_side() {
        let (a, b) = (KeyPair::generate(), KeyPair::generate());
        assert_eq!(agree(&a.private, &b.public), agree(&b.private, &a.public));
        assert_ne!(agree(&a.private, &b.public), agree(&a.private, &KeyPair::generate().public));
    }

    #[test]
    fn a_public_key_is_written_with_its_type_and_read_back_either_way() {
        let k = KeyPair::generate();
        let w = k.public_serialized();
        assert_eq!((w.len(), w[0]), (33, 5));
        assert_eq!(parse_public(&w), Some(k.public));
        assert_eq!(parse_public(&k.public), Some(k.public));
        assert_eq!(parse_public(&w[..20]), None);
        assert_eq!(parse_public(&[0x06; 33]), None);
    }

    #[test]
    fn a_signature_made_with_a_key_that_agrees_is_checked_by_its_public_half() {
        for _ in 0..20 {
            let k = KeyPair::generate();
            let sig = sign(&k.private, b"a signed prekey");
            assert!(verify(&k.public, b"a signed prekey", &sig));
            assert!(!verify(&k.public, b"another", &sig), "not for another message");
            let mut bad = sig;
            bad[3] ^= 1;
            assert!(!verify(&k.public, b"a signed prekey", &bad), "not for a changed signature");
            assert!(!verify(&KeyPair::generate().public, b"a signed prekey", &sig), "not for another key");
        }
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(unhex(&hex(&[0, 1, 254, 255])), Some(vec![0, 1, 254, 255]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }
}
