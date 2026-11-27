//! Pure-Rust Rocket.Chat End-to-End Encryption (E2EE) implementation.
//!
//! Matches Rocket.Chat's WebCrypto E2EE specification (v6 legacy through v9 modern):
//! - User keypair: 2048-bit RSA-OAEP (SHA-256), exported as JWK.
//! - Master key (v2 default): PBKDF2-HMAC-SHA256 (100,000 iterations, salt = `v2:<user_id>:<uuid>`)
//!   -> 256-bit AES-GCM key encrypting the private key JWK with a 12-byte random IV, stored as
//!   JSON `{ "iv", "ciphertext", "salt", "iterations" }`.
//! - Master key (v1 legacy): PBKDF2-HMAC-SHA256 (1,000 iterations, salt = `user_id`) -> 256-bit
//!   AES-CBC key encrypting the private key JWK with a 16-byte IV (`{ "$binary": ... }` or raw b64).
//! - Per-room session key (`Ks`): 256-bit AES-GCM (`A256GCM`) JWK (or legacy `A128CBC`), encrypted
//!   per member with their RSA-OAEP public key (`Ku`) and prefixed with `e2eKeyId`
//!   (`PrefixedBase64`: `<kid><344-char base64 of 256-byte RSA-OAEP>`).
//! - Message encryption (`t == "e2e"`):
//!   - Modern v2 AES-GCM-256 (`rc.v2.aes-sha2`): `content = { algorithm: "rc.v2.aes-sha2", kid, iv, ciphertext }`
//!   - Legacy v1 AES-CBC (`rc.v1.aes-sha2` or `msg`): `<12-char kid><base64(iv_16 || cbc_ct)>`
//! - File encryption: random 256-bit AES-CTR (`A256CTR`, 16-byte counter with 64-bit BE counter)
//!   + lowercase hex SHA-256 of plaintext, with `content` and `fileContent` encrypted under `Ks`.
//!
//! Security: never log private keys, `Ks`, tokens, or passphrases.

use super::Handle;
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncrypt, BlockEncryptMut, KeyInit as AesKeyInit, KeyIvInit};
use aes_gcm::{
    aead::Aead,
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use base64::Engine;
use rand::RngCore;
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, Oaep, RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

const PBKDF2_ITERATIONS_V1: u32 = 1_000;
const PBKDF2_ITERATIONS_V2: u32 = 100_000;
const KEY_ID_LEN: usize = 12;
/// Base64 length of a 256-byte RSA-2048 OAEP ciphertext (`ceil(256/3)*4 = 344`).
const RSA_2048_B64_LEN: usize = 344;

#[derive(Default)]
pub struct E2eeState {
    pub unlocked: bool,
    pub private_key: Option<RsaPrivateKey>,
    pub public_key_jwk: Option<Value>,
    /// Decrypted per-room symmetric session keys: `room_id -> (key_id, key_bytes)`.
    /// `key_bytes` is 32 bytes for `A256GCM` / `A256CBC`, or 16 bytes for legacy `A128CBC`.
    pub room_keys: HashMap<String, (String, Vec<u8>)>,
}

/// Generate a fresh 2048-bit RSA-OAEP keypair.
pub fn generate_rsa_keypair() -> Result<(RsaPrivateKey, RsaPublicKey)> {
    let mut rng = rand::thread_rng();
    let priv_key = RsaPrivateKey::new(&mut rng, 2048).context("generating 2048-bit RSA keypair")?;
    let pub_key = RsaPublicKey::from(&priv_key);
    Ok((priv_key, pub_key))
}

/// Export an RSA public key as a WebCrypto-compatible JWK.
pub fn public_key_to_jwk(pub_key: &RsaPublicKey) -> Value {
    json!({
        "kty": "RSA",
        "alg": "RSA-OAEP-256",
        "ext": true,
        "key_ops": ["encrypt"],
        "n": B64URL.encode(pub_key.n().to_bytes_be()),
        "e": B64URL.encode(pub_key.e().to_bytes_be()),
    })
}

/// Parse an RSA public key from a JWK JSON object.
pub fn public_key_from_jwk(jwk: &Value) -> Result<RsaPublicKey> {
    let n_bytes = B64URL
        .decode(jwk["n"].as_str().ok_or_else(|| anyhow!("JWK missing n"))?)
        .context("decoding JWK n")?;
    let e_bytes = B64URL
        .decode(jwk["e"].as_str().ok_or_else(|| anyhow!("JWK missing e"))?)
        .context("decoding JWK e")?;
    RsaPublicKey::new(BigUint::from_bytes_be(&n_bytes), BigUint::from_bytes_be(&e_bytes))
        .context("constructing RSA public key from JWK")
}

/// Export an RSA private key as a WebCrypto-compatible JWK.
pub fn private_key_to_jwk(priv_key: &RsaPrivateKey) -> Result<Value> {
    let primes = priv_key.primes();
    if primes.len() < 2 {
        anyhow::bail!("RSA private key missing prime factors");
    }
    let p = &primes[0];
    let q = &primes[1];
    let dp = priv_key.dp().ok_or_else(|| anyhow!("missing dp"))?;
    let dq = priv_key.dq().ok_or_else(|| anyhow!("missing dq"))?;
    let qi = priv_key
        .qinv()
        .ok_or_else(|| anyhow!("missing qinv"))?
        .to_biguint()
        .ok_or_else(|| anyhow!("negative qinv"))?;
    Ok(json!({
        "kty": "RSA",
        "alg": "RSA-OAEP-256",
        "ext": true,
        "key_ops": ["decrypt"],
        "n": B64URL.encode(priv_key.n().to_bytes_be()),
        "e": B64URL.encode(priv_key.e().to_bytes_be()),
        "d": B64URL.encode(priv_key.d().to_bytes_be()),
        "p": B64URL.encode(p.to_bytes_be()),
        "q": B64URL.encode(q.to_bytes_be()),
        "dp": B64URL.encode(dp.to_bytes_be()),
        "dq": B64URL.encode(dq.to_bytes_be()),
        "qi": B64URL.encode(qi.to_bytes_be()),
    }))
}

/// Parse an RSA private key from a JWK JSON object.
pub fn private_key_from_jwk(jwk: &Value) -> Result<RsaPrivateKey> {
    let dec = |field: &str| -> Result<BigUint> {
        let s = jwk[field]
            .as_str()
            .ok_or_else(|| anyhow!("private JWK missing {field}"))?;
        let b = B64URL.decode(s).with_context(|| format!("decoding JWK {field}"))?;
        Ok(BigUint::from_bytes_be(&b))
    };
    let n = dec("n")?;
    let e = dec("e")?;
    let d = dec("d")?;
    let p = dec("p")?;
    let q = dec("q")?;
    let mut key = RsaPrivateKey::from_components(n, e, d, vec![p, q])
        .context("constructing RSA private key from JWK")?;
    key.precompute().context("precomputing RSA private key CRT")?;
    Ok(key)
}

/// Derive a 256-bit master key from the user's E2EE passphrase via PBKDF2-HMAC-SHA256.
pub fn derive_master_key(passphrase: &str, salt: &str, iterations: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt.as_bytes(), iterations, &mut out);
    out
}

/// Encrypt a private key JWK in Rocket.Chat's v2 Keychain format:
/// PBKDF2-HMAC-SHA256 (100,000 iterations, salt = `v2:<user_id>:<uuid>`) + AES-256-GCM (12-byte IV),
/// serialized as JSON `{ "iv", "ciphertext", "salt", "iterations" }`.
pub fn wrap_private_key(priv_jwk: &Value, passphrase: &str, user_id: &str) -> Result<String> {
    let salt = format!("v2:{user_id}:{}", uuid::Uuid::new_v4());
    let master = derive_master_key(passphrase, &salt, PBKDF2_ITERATIONS_V2);
    let plaintext = serde_json::to_vec(priv_jwk)?;
    let mut iv = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut iv);
    let cipher = Aes256Gcm::new(&master.into());
    let ct = cipher
        .encrypt(Nonce::from_slice(&iv), plaintext.as_ref())
        .map_err(|_| anyhow!("AES-GCM private key encryption failed"))?;
    Ok(json!({
        "iv": B64.encode(iv),
        "ciphertext": B64.encode(ct),
        "salt": salt,
        "iterations": PBKDF2_ITERATIONS_V2,
    })
    .to_string())
}

/// Decrypt a server-stored encrypted private key JWK with the user's E2EE passphrase.
/// Supports Rocket.Chat v2 JSON (`{ iv, ciphertext, salt, iterations }` with AES-GCM or AES-CBC),
/// v1 JSON (`{ "$binary": base64(iv_16 || ct) }`), and raw/legacy `$CBC$` base64.
pub fn unwrap_private_key(encrypted: &str, passphrase: &str, user_id: &str) -> Result<RsaPrivateKey> {
    let trimmed = encrypted.trim();
    let pt = if trimmed.starts_with('{') {
        let obj: Value = serde_json::from_str(trimmed).context("parsing encrypted private key JSON")?;
        if let (Some(iv_b64), Some(ct_b64), Some(salt), Some(iters)) = (
            obj["iv"].as_str(),
            obj["ciphertext"].as_str(),
            obj["salt"].as_str(),
            obj["iterations"].as_u64(),
        ) {
            let iv = B64.decode(iv_b64).context("decoding private key iv")?;
            let ct = B64.decode(ct_b64).context("decoding private key ciphertext")?;
            let master = derive_master_key(passphrase, salt, iters as u32);
            if iv.len() == 16 {
                Aes256CbcDec::new(&master.into(), iv.as_slice().into())
                    .decrypt_padded_vec_mut::<Pkcs7>(&ct)
                    .map_err(|_| anyhow!("wrong E2EE passphrase or corrupted private key"))?
            } else if iv.len() == 12 {
                let cipher = Aes256Gcm::new(&master.into());
                cipher
                    .decrypt(Nonce::from_slice(&iv), ct.as_ref())
                    .map_err(|_| anyhow!("wrong E2EE passphrase or corrupted private key"))?
            } else {
                anyhow::bail!("unsupported private key IV length {}", iv.len());
            }
        } else if let Some(bin_b64) = obj["$binary"].as_str() {
            let raw = B64.decode(bin_b64).context("decoding v1 $binary private key")?;
            if raw.len() < 32 {
                anyhow::bail!("v1 encrypted private key too short");
            }
            let (iv, ct) = raw.split_at(16);
            let master = derive_master_key(passphrase, user_id, PBKDF2_ITERATIONS_V1);
            Aes256CbcDec::new(&master.into(), iv.into())
                .decrypt_padded_vec_mut::<Pkcs7>(ct)
                .map_err(|_| anyhow!("wrong E2EE passphrase or corrupted private key"))?
        } else {
            anyhow::bail!("unrecognized encrypted private key JSON schema");
        }
    } else {
        let clean = trimmed.trim_start_matches("$CBC$");
        let raw = B64.decode(clean).context("decoding encrypted private key base64")?;
        if raw.len() < 32 {
            anyhow::bail!("encrypted private key too short");
        }
        let (iv, ct) = raw.split_at(16);
        let master = derive_master_key(passphrase, user_id, PBKDF2_ITERATIONS_V1);
        Aes256CbcDec::new(&master.into(), iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(ct)
            .map_err(|_| anyhow!("wrong E2EE passphrase or corrupted private key"))?
    };

    let jwk: Value = serde_json::from_slice(&pt).context("parsing decrypted private key JWK")?;
    private_key_from_jwk(&jwk)
}

/// Compute a 12-character fallback `key_id` for a symmetric room key `Ks`.
pub fn room_key_id(ks: &[u8]) -> String {
    let digest = Sha256::digest(ks);
    let b64 = B64.encode(digest);
    b64.chars().take(KEY_ID_LEN).collect()
}

/// Wrap a 256-bit room session key `Ks` with a recipient's RSA-OAEP public key (`Ku`).
/// Uses Rocket.Chat v8/v9 `A256GCM` JWK and `PrefixedBase64` (`<key_id><344-char base64>`).
pub fn wrap_room_key(ks: &[u8], key_id: &str, recipient_pub: &RsaPublicKey) -> Result<String> {
    let alg = if ks.len() == 16 { "A128CBC" } else { "A256GCM" };
    let jwk = json!({
        "kty": "oct",
        "alg": alg,
        "ext": true,
        "key_ops": ["encrypt", "decrypt"],
        "k": B64URL.encode(ks),
    });
    let mut rng = rand::thread_rng();
    let enc = recipient_pub
        .encrypt(&mut rng, Oaep::new::<Sha256>(), serde_json::to_vec(&jwk)?.as_slice())
        .context("RSA-OAEP encrypting room key")?;
    Ok(format!("{key_id}{}", B64.encode(enc)))
}

/// Unwrap a subscription's `E2EKey` or `E2ESuggestedKey` using our RSA-OAEP private key.
/// Follows Rocket.Chat's `PrefixedBase64` spec: the last 344 characters are the Base64-encoded
/// 256-byte RSA-2048 OAEP ciphertext, and any leading prefix (12-char legacy id or 36-char UUID)
/// is the `key_id`. Also accepts bare unprefixed Base64.
pub fn unwrap_room_key(e2e_key: &str, priv_key: &RsaPrivateKey) -> Result<(String, Vec<u8>)> {
    let try_decrypt = |b64_str: &str| -> Result<Vec<u8>> {
        let enc = B64.decode(b64_str.trim()).context("decoding E2EKey base64")?;
        let pt = priv_key
            .decrypt(Oaep::new::<Sha256>(), &enc)
            .context("RSA-OAEP decrypting room key")?;
        let jwk: Value = serde_json::from_slice(&pt).context("parsing room key JWK")?;
        let k_bytes = B64URL
            .decode(jwk["k"].as_str().ok_or_else(|| anyhow!("room key JWK missing k"))?)
            .context("decoding room key k")?;
        if k_bytes.len() != 16 && k_bytes.len() != 32 {
            anyhow::bail!("unexpected room key length {}", k_bytes.len());
        }
        Ok(k_bytes)
    };

    let trimmed = e2e_key.trim();
    if trimmed.len() > RSA_2048_B64_LEN {
        let split_at = trimmed.len() - RSA_2048_B64_LEN;
        let (kid, b64_part) = trimmed.split_at(split_at);
        if let Ok(ks) = try_decrypt(b64_part) {
            return Ok((kid.to_string(), ks));
        }
    }
    if trimmed.len() > KEY_ID_LEN {
        if let Ok(ks) = try_decrypt(&trimmed[KEY_ID_LEN..]) {
            return Ok((trimmed[..KEY_ID_LEN].to_string(), ks));
        }
    }
    let ks = try_decrypt(trimmed)?;
    Ok((room_key_id(&ks), ks))
}

/// Encrypt a plaintext message in legacy AES-256-CBC (PKCS#7) mode (`<key_id><base64(iv || ct)>`).
pub fn encrypt_message_cbc(ks: &[u8], key_id: &str, payload: &Value) -> Result<String> {
    let ks32: [u8; 32] = ks.try_into().map_err(|_| anyhow!("AES-256-CBC requires 32-byte key"))?;
    let pt = serde_json::to_vec(payload)?;
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    let ct = Aes256CbcEnc::new(&ks32.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&pt);
    let mut combined = Vec::with_capacity(16 + ct.len());
    combined.extend_from_slice(&iv);
    combined.extend_from_slice(&ct);
    Ok(format!("{key_id}{}", B64.encode(combined)))
}

/// Decrypt a legacy `rc.v1.aes-sha2` / AES-CBC message (`<12-char key_id><base64(iv_16 || ct)>`).
/// Supports both 128-bit (`A128CBC`, Rocket.Chat's original v1) and 256-bit (`A256CBC`) keys.
pub fn decrypt_message_cbc(ks: &[u8], ciphertext_with_key_id: &str) -> Result<Value> {
    if ciphertext_with_key_id.len() <= KEY_ID_LEN {
        anyhow::bail!("AES-CBC E2EE message too short");
    }
    let raw = B64
        .decode(&ciphertext_with_key_id[KEY_ID_LEN..])
        .context("decoding AES-CBC message base64")?;
    if raw.len() < 32 {
        anyhow::bail!("AES-CBC ciphertext too short");
    }
    let (iv, ct) = raw.split_at(16);
    let pt = if ks.len() == 16 {
        let ks16: [u8; 16] = ks.try_into().unwrap();
        Aes128CbcDec::new(&ks16.into(), iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(ct)
            .map_err(|_| anyhow!("AES-128-CBC decryption failed"))?
    } else if ks.len() == 32 {
        let ks32: [u8; 32] = ks.try_into().unwrap();
        Aes256CbcDec::new(&ks32.into(), iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(ct)
            .map_err(|_| anyhow!("AES-256-CBC decryption failed"))?
    } else {
        anyhow::bail!("unsupported AES-CBC key length {}", ks.len());
    };
    Ok(serde_json::from_slice(&pt).context("parsing decrypted AES-CBC JSON")?)
}

/// Encrypt a plaintext payload in Rocket.Chat's modern AES-256-GCM mode (`rc.v2.aes-sha2`).
pub fn encrypt_message_v2(ks: &[u8], key_id: &str, payload: &Value) -> Result<Value> {
    let ks32: [u8; 32] = ks.try_into().map_err(|_| anyhow!("AES-256-GCM requires 32-byte key"))?;
    let cipher = Aes256Gcm::new(&ks32.into());
    let mut iv = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut iv);
    let pt = serde_json::to_vec(payload)?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&iv), pt.as_ref())
        .map_err(|_| anyhow!("AES-GCM encryption failed"))?;
    Ok(json!({
        "algorithm": "rc.v2.aes-sha2",
        "kid": key_id,
        "iv": B64.encode(iv),
        "ciphertext": B64.encode(ct),
    }))
}

/// Decrypt a Rocket.Chat `rc.v2.aes-sha2` message (`{ algorithm, kid, iv, ciphertext }`).
pub fn decrypt_message_v2(ks: &[u8], iv_b64: &str, ciphertext_b64: &str) -> Result<Value> {
    let ks32: [u8; 32] = ks.try_into().map_err(|_| anyhow!("AES-256-GCM requires 32-byte key"))?;
    let iv = B64.decode(iv_b64).context("decoding AES-GCM iv base64")?;
    if iv.len() != 12 {
        anyhow::bail!("expected 12-byte AES-GCM iv, got {}", iv.len());
    }
    let ct = B64.decode(ciphertext_b64).context("decoding AES-GCM ciphertext base64")?;
    let cipher = Aes256Gcm::new(&ks32.into());
    let pt = cipher
        .decrypt(Nonce::from_slice(&iv), ct.as_ref())
        .map_err(|_| anyhow!("AES-GCM v2 decryption failed"))?;
    Ok(serde_json::from_slice(&pt).context("parsing decrypted AES-GCM v2 JSON")?)
}

/// Decrypt a combined `base64(iv_12 || ct_tag)` AES-GCM ciphertext (compat fallback).
pub fn decrypt_message_gcm(ks: &[u8], ciphertext_b64: &str) -> Result<Value> {
    let ks32: [u8; 32] = ks.try_into().map_err(|_| anyhow!("AES-256-GCM requires 32-byte key"))?;
    let raw = B64
        .decode(ciphertext_b64)
        .context("decoding AES-GCM ciphertext base64")?;
    if raw.len() < 28 {
        anyhow::bail!("AES-GCM ciphertext too short");
    }
    let (iv, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new(&ks32.into());
    let pt = cipher
        .decrypt(Nonce::from_slice(iv), ct)
        .map_err(|_| anyhow!("AES-GCM decryption failed"))?;
    Ok(serde_json::from_slice(&pt).context("parsing decrypted AES-GCM JSON")?)
}

/// WebCrypto-compatible AES-256-CTR (`length: 64`: first 8 bytes fixed nonce, last 8 bytes big-endian counter).
/// Encryption and decryption are the same XOR keystream operation.
fn aes256_ctr_64_xor(key: &[u8; 32], initial_counter: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = aes::Aes256::new(key.into());
    let mut out = Vec::with_capacity(data.len());
    let nonce_hi = &initial_counter[..8];
    let mut counter_lo = u64::from_be_bytes(initial_counter[8..16].try_into().unwrap());
    let mut block = aes::Block::default();
    for chunk in data.chunks(16) {
        block[..8].copy_from_slice(nonce_hi);
        block[8..16].copy_from_slice(&counter_lo.to_be_bytes());
        cipher.encrypt_block(&mut block);
        for (i, &b) in chunk.iter().enumerate() {
            out.push(b ^ block[i]);
        }
        counter_lo = counter_lo.wrapping_add(1);
    }
    out
}

/// Encrypt file bytes for an E2EE room attachment matching Rocket.Chat WebCrypto:
/// AES-256-CTR (`length: 64`, 16-byte random counter) + lowercase hex SHA-256 of the plaintext.
pub fn encrypt_file_bytes(plaintext: &[u8]) -> Result<(Vec<u8>, Value)> {
    let mut key = [0u8; 32];
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut key);
    rand::thread_rng().fill_bytes(&mut iv);
    let ct = aes256_ctr_64_xor(&key, &iv, plaintext);
    let sha256_hex = hex::encode(Sha256::digest(plaintext));
    let meta = json!({
        "encryption": {
            "key": {
                "kty": "oct",
                "alg": "A256CTR",
                "ext": true,
                "key_ops": ["encrypt", "decrypt"],
                "k": B64URL.encode(key),
            },
            "iv": B64.encode(iv),
        },
        "hashes": { "sha256": sha256_hex },
    });
    Ok((ct, meta))
}

/// Decrypt E2EE file attachment bytes using the message's `encryption` metadata.
/// Supports Rocket.Chat's official `A256CTR` (16-byte counter, hex SHA-256 of plaintext)
/// and fallback `A256GCM` (12-byte IV, base64 SHA-256 of ciphertext).
pub fn decrypt_file_bytes(ciphertext: &[u8], meta: &Value) -> Result<Vec<u8>> {
    let k_str = meta["encryption"]["key"]["k"]
        .as_str()
        .ok_or_else(|| anyhow!("file encryption metadata missing key.k"))?;
    let iv_str = meta["encryption"]["iv"]
        .as_str()
        .ok_or_else(|| anyhow!("file encryption metadata missing iv"))?;
    let key_bytes: [u8; 32] = B64URL
        .decode(k_str)
        .context("decoding file key")?
        .try_into()
        .map_err(|_| anyhow!("file key not 256-bit"))?;
    let iv_bytes = B64.decode(iv_str).context("decoding file iv")?;

    let alg = meta["encryption"]["key"]["alg"].as_str().unwrap_or("");
    if alg == "A256CTR" || iv_bytes.len() == 16 {
        let iv16: [u8; 16] = iv_bytes
            .try_into()
            .map_err(|_| anyhow!("expected 16-byte AES-CTR counter"))?;
        let pt = aes256_ctr_64_xor(&key_bytes, &iv16, ciphertext);
        if let Some(expected_hash) = meta["hashes"]["sha256"].as_str() {
            let actual_hex = hex::encode(Sha256::digest(&pt));
            let actual_b64 = B64.encode(Sha256::digest(&pt));
            if !actual_hex.eq_ignore_ascii_case(expected_hash) && actual_b64 != expected_hash {
                anyhow::bail!("E2EE file SHA-256 mismatch");
            }
        }
        return Ok(pt);
    }

    if let Some(expected_hash) = meta["hashes"]["sha256"].as_str() {
        let actual = B64.encode(Sha256::digest(ciphertext));
        if actual != expected_hash {
            anyhow::bail!("E2EE file SHA-256 mismatch");
        }
    }
    if iv_bytes.len() != 12 {
        anyhow::bail!("expected 12-byte AES-GCM IV");
    }
    let cipher = Aes256Gcm::new(&key_bytes.into());
    cipher
        .decrypt(Nonce::from_slice(&iv_bytes), ciphertext)
        .map_err(|_| anyhow!("E2EE file decryption failed"))
}

/// Fetch or generate RSA keypair and unlock all E2EE rooms using `passphrase`.
pub async fn unlock_with_passphrase(handle: &Handle, passphrase: &str) -> Result<Value> {
    let own_id = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .own_id
        .clone();

    let fetched = handle.api.get("/api/v1/e2e.fetchMyKeys", &[]).await?;
    let (priv_key, pub_jwk, created) = if let (Some(pub_str), Some(priv_str)) = (
        fetched["public_key"].as_str().filter(|s| !s.is_empty()),
        fetched["private_key"].as_str().filter(|s| !s.is_empty()),
    ) {
        let priv_key = unwrap_private_key(priv_str, passphrase, &own_id)?;
        let pub_jwk: Value = serde_json::from_str(pub_str).unwrap_or_else(|_| public_key_to_jwk(&RsaPublicKey::from(&priv_key)));
        (priv_key, pub_jwk, false)
    } else {
        let (priv_key, pub_key) = generate_rsa_keypair()?;
        let pub_jwk = public_key_to_jwk(&pub_key);
        let priv_jwk = private_key_to_jwk(&priv_key)?;
        let enc_priv = wrap_private_key(&priv_jwk, passphrase, &own_id)?;
        handle
            .api
            .post(
                "/api/v1/e2e.setUserPublicAndPrivateKeys",
                &json!({
                    "public_key": pub_jwk.to_string(),
                    "private_key": enc_priv,
                }),
            )
            .await?;
        (priv_key, pub_jwk, true)
    };

    // Refresh subscriptions so newly distributed E2EKey / E2ESuggestedKey values are picked up.
    if let Ok(subs) = handle.api.get("/api/v1/subscriptions.get", &[]).await {
        let mut suggested_rids = Vec::new();
        {
            let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            for s in subs["update"].as_array().into_iter().flatten() {
                if s["E2ESuggestedKey"].as_str().is_some_and(|k| !k.is_empty()) {
                    if let Some(rid) = s["rid"].as_str() {
                        suggested_rids.push(rid.to_string());
                    }
                }
                world.merge_subscription_json(s);
            }
        }
        // Accept suggested group keys on the server (Rocket.Chat v7/v8/v9 protocol)
        for rid in suggested_rids {
            let _ = handle
                .api
                .post("/api/v1/e2e.acceptSuggestedGroupKey", &json!({ "rid": rid }))
                .await;
        }
    }

    // Unwrap every subscription's E2EKey we already have
    let room_e2e_keys: Vec<(String, String)> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world
            .rooms
            .values()
            .filter_map(|r| r.e2e_key.as_ref().map(|k| (r.id.clone(), k.clone())))
            .collect()
    };

    let mut decrypted_count = 0usize;
    {
        let mut e2ee = handle.e2ee.lock().unwrap_or_else(|e| e.into_inner());
        e2ee.unlocked = true;
        e2ee.public_key_jwk = Some(pub_jwk);
        for (rid, enc_key) in room_e2e_keys {
            if let Ok((kid, ks)) = unwrap_room_key(&enc_key, &priv_key) {
                e2ee.room_keys.insert(rid, (kid, ks));
                decrypted_count += 1;
            }
        }
        e2ee.private_key = Some(priv_key);
    }

    Ok(json!({
        "unlocked": true,
        "generatedKeypair": created,
        "roomsUnlocked": decrypted_count,
    }))
}

/// Build an encrypted `t == "e2e"` message payload for `chat.sendMessage`.
pub fn build_encrypted_message_payload(handle: &Handle, room_id: &str, text: &str) -> Result<Value> {
    let (key_id, ks) = {
        let e2ee = handle.e2ee.lock().unwrap_or_else(|e| e.into_inner());
        e2ee.room_keys
            .get(room_id)
            .cloned()
            .ok_or_else(|| anyhow!("E2EE key for room {room_id} is not unlocked"))?
    };

    if handle.server.version.supports_aes_gcm_e2ee() && ks.len() == 32 {
        let inner = json!({
            "msg": text,
        });
        let content = encrypt_message_v2(&ks, &key_id, &inner)?;
        Ok(json!({
            "rid": room_id,
            "t": "e2e",
            "e2e": "pending",
            "msg": "",
            "content": content,
        }))
    } else {
        let inner = json!({
            "_id": uuid::Uuid::new_v4().to_string(),
            "text": text,
            "ts": chrono::Utc::now().to_rfc3339(),
        });
        let msg = encrypt_message_cbc(&ks, &key_id, &inner)?;
        Ok(json!({
            "rid": room_id,
            "t": "e2e",
            "e2e": "pending",
            "msg": msg,
        }))
    }
}

fn extract_decrypted_text(v: &Value) -> Option<String> {
    if let Some(s) = v["msg"].as_str().or_else(|| v["text"].as_str()) {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(att) = v["attachments"].as_array().and_then(|a| a.first()) {
        if let Some(title) = att["title"].as_str().filter(|s| !s.is_empty()) {
            return Some(format!("[encrypted file: {title}]"));
        }
    }
    v["msg"].as_str().or_else(|| v["text"].as_str()).map(str::to_string)
}

/// Decrypt an incoming Rocket.Chat E2EE message (supports `rc.v2.aes-sha2` AES-GCM,
/// `rc.v1.aes-sha2` AES-CBC, and legacy `msg` AES-CBC).
pub fn decrypt_incoming(handle: &Handle, room_id: &str, msg: &Value) -> Option<String> {
    let ks = {
        let e2ee = handle.e2ee.lock().unwrap_or_else(|e| e.into_inner());
        e2ee.room_keys.get(room_id).map(|(_, k)| k.clone())?
    };
    let algo = msg["content"]["algorithm"].as_str().unwrap_or("");
    if algo == "rc.v2.aes-sha2" {
        if let (Some(iv), Some(ct)) = (
            msg["content"]["iv"].as_str().filter(|s| !s.is_empty()),
            msg["content"]["ciphertext"].as_str().filter(|s| !s.is_empty()),
        ) {
            if let Ok(v) = decrypt_message_v2(&ks, iv, ct) {
                return extract_decrypted_text(&v);
            }
        }
    }
    if let Some(ct) = msg["content"]["ciphertext"].as_str().filter(|s| !s.is_empty()) {
        if let Ok(v) = decrypt_message_cbc(&ks, ct) {
            return extract_decrypted_text(&v);
        }
        if let Ok(v) = decrypt_message_gcm(&ks, ct) {
            return extract_decrypted_text(&v);
        }
    }
    if let Some(raw) = msg["msg"].as_str().filter(|s| s.len() > KEY_ID_LEN) {
        if let Ok(v) = decrypt_message_cbc(&ks, raw) {
            return extract_decrypted_text(&v);
        }
    }
    None
}

/// Distribute `Ks` to newly joined members of an E2EE room (`e2e.getUsersOfRoomWithoutKey`,
/// `e2e.provideUsersSuggestedGroupKeys`, and legacy `e2e.updateGroupKey`).
pub async fn distribute_room_keys(handle: &Handle, room_id: &str) -> Result<usize> {
    let Some((key_id, ks)) = handle
        .e2ee
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .room_keys
        .get(room_id)
        .cloned()
    else {
        return Ok(0);
    };

    let resp = handle
        .api
        .get("/api/v1/e2e.getUsersOfRoomWithoutKey", &[("rid", room_id.to_string())])
        .await?;
    let users = resp["users"].as_array().cloned().unwrap_or_default();
    let mut distributed = 0usize;
    let mut suggested_list = Vec::new();

    for u in users {
        let Some(uid) = u["_id"].as_str() else { continue };
        let Some(pub_str) = u["e2e"]["public_key"].as_str() else { continue };
        let Ok(pub_jwk) = serde_json::from_str::<Value>(pub_str) else { continue };
        let Ok(pub_key) = public_key_from_jwk(&pub_jwk) else { continue };
        if let Ok(wrapped) = wrap_room_key(&ks, &key_id, &pub_key) {
            // Update individual key & collect batch suggested keys
            let _ = handle
                .api
                .post(
                    "/api/v1/e2e.updateGroupKey",
                    &json!({ "uid": uid, "rid": room_id, "key": wrapped }),
                )
                .await;
            suggested_list.push(json!({ "_id": uid, "key": wrapped }));
            distributed += 1;
        }
    }

    if !suggested_list.is_empty() {
        let _ = handle
            .api
            .post(
                "/api/v1/e2e.provideUsersSuggestedGroupKeys",
                &json!({ "usersSuggestedGroupKeys": { room_id: suggested_list } }),
            )
            .await;
    }

    Ok(distributed)
}

/// Reset own E2EE keypair on server (`POST /api/v1/users.resetE2EKey` or legacy `e2e.resetOwnE2EKey`).
/// Clears local E2EE state; user will lose access to previous encrypted rooms until re-invited.
pub async fn reset_own_key(handle: &Handle) -> Result<Value> {
    let own_id = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .own_id
        .clone();
    let resp = match handle
        .api
        .post("/api/v1/users.resetE2EKey", &json!({ "userId": own_id }))
        .await
    {
        Ok(v) => v,
        Err(_) => {
            // Fallback for older servers or non-admin self-reset via DDP method
            let method_payload = json!({
                "message": json!({
                    "msg": "method",
                    "id": super::ddp::next_id("e2e-reset"),
                    "method": "e2e.resetOwnE2EKey",
                    "params": [],
                }).to_string()
            });
            match handle
                .api
                .post("/api/v1/method.call/e2e.resetOwnE2EKey", &method_payload)
                .await
            {
                Ok(v) => v,
                Err(_) => handle.api.post("/api/v1/e2e.resetOwnE2EKey", &json!({})).await?,
            }
        }
    };
    let mut e2ee = handle.e2ee.lock().unwrap_or_else(|e| e.into_inner());
    *e2ee = E2eeState::default();
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_e2ee_roundtrip_rsa_pbkdf2_cbc_gcm_and_files() {
        // 1. RSA-OAEP 2048-bit keypair + JWK round-trip
        let (priv_key, pub_key) = generate_rsa_keypair().unwrap();
        let pub_jwk = public_key_to_jwk(&pub_key);
        let priv_jwk = private_key_to_jwk(&priv_key).unwrap();
        let pub_parsed = public_key_from_jwk(&pub_jwk).unwrap();

        // 2. PBKDF2-SHA256 + AES-CBC private key wrap/unwrap
        let wrapped_priv = wrap_private_key(&priv_jwk, "correct horse battery staple", "user123").unwrap();
        let unwrapped_priv = unwrap_private_key(&wrapped_priv, "correct horse battery staple", "user123").unwrap();
        assert!(unwrap_private_key(&wrapped_priv, "wrong passphrase", "user123").is_err());

        // 3. Per-room symmetric key Ks wrap/unwrap via RSA-OAEP (both 36-char UUID kid & 12-char legacy kid)
        let mut ks = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut ks);
        let uuid_kid = uuid::Uuid::new_v4().to_string();
        let e2e_key = wrap_room_key(&ks, &uuid_kid, &pub_parsed).unwrap();
        let (unwrapped_kid, unwrapped_ks) = unwrap_room_key(&e2e_key, &unwrapped_priv).unwrap();
        assert_eq!(unwrapped_kid, uuid_kid);
        assert_eq!(unwrapped_ks.as_slice(), &ks);

        // 4. Legacy AES-CBC message encryption/decryption
        let kid = room_key_id(&ks);
        let payload = json!({ "_id": "m1", "msg": "hello CBC rocket!" });
        let cbc_ct = encrypt_message_cbc(&ks, &kid, &payload).unwrap();
        assert!(cbc_ct.starts_with(&kid));
        let cbc_pt = decrypt_message_cbc(&ks, &cbc_ct).unwrap();
        assert_eq!(cbc_pt["msg"], "hello CBC rocket!");

        // 5. Modern AES-GCM (`rc.v2.aes-sha2`) message encryption/decryption
        let v2_obj = encrypt_message_v2(&ks, &uuid_kid, &payload).unwrap();
        assert_eq!(v2_obj["algorithm"], "rc.v2.aes-sha2");
        assert_eq!(v2_obj["kid"], uuid_kid);
        let v2_pt = decrypt_message_v2(
            &ks,
            v2_obj["iv"].as_str().unwrap(),
            v2_obj["ciphertext"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(v2_pt["msg"], "hello CBC rocket!");

        // 6. E2EE file encryption/decryption (AES-256-CTR + hex SHA-256 of plaintext)
        let file_bytes = b"secret image payload bytes 1234567890";
        let (enc_file, meta) = encrypt_file_bytes(file_bytes).unwrap();
        assert_ne!(enc_file.as_slice(), file_bytes);
        assert_eq!(meta["encryption"]["key"]["alg"], "A256CTR");
        let dec_file = decrypt_file_bytes(&enc_file, &meta).unwrap();
        assert_eq!(dec_file.as_slice(), file_bytes);
    }
}
