//! Pure-Rust Rocket.Chat End-to-End Encryption (E2EE) implementation.
//!
//! Matches Rocket.Chat's WebCrypto E2EE specification:
//! - User keypair: 2048-bit RSA-OAEP (SHA-256), exported as JWK.
//! - Master key: PBKDF2-HMAC-SHA256 (1000 iterations, salt = `user_id`) -> 256-bit AES-CBC key
//!   encrypting the private key JWK with a 16-byte random IV (`$CBC` / Rocket.Chat vector format).
//! - Per-room session key (`Ks`): 256-bit AES key exported as JWK, encrypted per member with their
//!   RSA-OAEP public key (`Ku`), prefixed with the 12-char `key_id` (`E2EKey = key_id || base64(rsa_oaep(Ks_jwk))`).
//! - Message encryption (`t == "e2e"`):
//!   - Legacy AES-CBC-256 (PKCS#7): `msg = key_id || base64(iv_16 || ciphertext)`
//!   - Modern AES-GCM-256 (`rc.v1.aes-sha2`): `content = { algorithm: "rc.v1.aes-sha2", ciphertext: base64(iv_12 || ct_tag) }`
//! - File encryption: random 256-bit AES-GCM key + 12-byte IV + SHA-256 hash metadata.
//!
//! Security: never log private keys, `Ks`, tokens, or passphrases.

use super::Handle;
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use aes_gcm::{
    aead::{Aead, KeyInit},
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

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

const PBKDF2_ITERATIONS: u32 = 1_000;
const KEY_ID_LEN: usize = 12;

#[derive(Default)]
pub struct E2eeState {
    pub unlocked: bool,
    pub private_key: Option<RsaPrivateKey>,
    pub public_key_jwk: Option<Value>,
    /// Decrypted per-room symmetric session keys: `room_id -> (key_id, [u8; 32])`.
    pub room_keys: HashMap<String, (String, [u8; 32])>,
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
pub fn derive_master_key(passphrase: &str, salt: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt.as_bytes(), PBKDF2_ITERATIONS, &mut out);
    out
}

/// Encrypt a private key JWK with the PBKDF2 master key using AES-256-CBC (PKCS#7),
/// returning Rocket.Chat's `base64(iv_16 || ciphertext)` string.
pub fn wrap_private_key(priv_jwk: &Value, passphrase: &str, user_id: &str) -> Result<String> {
    let master = derive_master_key(passphrase, user_id);
    let plaintext = serde_json::to_vec(priv_jwk)?;
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    let ct = Aes256CbcEnc::new(&master.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&plaintext);
    let mut combined = Vec::with_capacity(16 + ct.len());
    combined.extend_from_slice(&iv);
    combined.extend_from_slice(&ct);
    Ok(B64.encode(combined))
}

/// Decrypt a server-stored encrypted private key JWK with the user's E2EE passphrase.
pub fn unwrap_private_key(encrypted_b64: &str, passphrase: &str, user_id: &str) -> Result<RsaPrivateKey> {
    // Rocket.Chat sometimes prefixes with `$CBC$` or JSON-wraps `{ "$binary": ... }`
    let clean = encrypted_b64.trim().trim_start_matches("$CBC$");
    let raw = B64.decode(clean).context("decoding encrypted private key base64")?;
    if raw.len() < 32 {
        anyhow::bail!("encrypted private key too short");
    }
    let (iv, ct) = raw.split_at(16);
    let master = derive_master_key(passphrase, user_id);
    let pt = Aes256CbcDec::new(&master.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ct)
        .map_err(|_| anyhow!("wrong E2EE passphrase or corrupted private key"))?;
    let jwk: Value = serde_json::from_slice(&pt).context("parsing decrypted private key JWK")?;
    private_key_from_jwk(&jwk)
}

/// Compute Rocket.Chat's 12-character `key_id` for a symmetric room key `Ks`.
pub fn room_key_id(ks: &[u8; 32]) -> String {
    let digest = Sha256::digest(ks);
    let b64 = B64.encode(digest);
    b64.chars().take(KEY_ID_LEN).collect()
}

/// Wrap a 256-bit room session key `Ks` with a recipient's RSA-OAEP public key (`Ku`).
/// Returns `E2EKey = key_id || base64(rsa_oaep(Ks_jwk))`.
pub fn wrap_room_key(ks: &[u8; 32], key_id: &str, recipient_pub: &RsaPublicKey) -> Result<String> {
    let jwk = json!({
        "kty": "oct",
        "alg": "A256CBC",
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

/// Unwrap a subscription's `E2EKey` using our RSA-OAEP private key.
pub fn unwrap_room_key(e2e_key: &str, priv_key: &RsaPrivateKey) -> Result<(String, [u8; 32])> {
    if e2e_key.len() <= KEY_ID_LEN {
        anyhow::bail!("E2EKey too short");
    }
    let (key_id, rest) = e2e_key.split_at(KEY_ID_LEN);
    let enc = B64.decode(rest).context("decoding E2EKey base64")?;
    let pt = priv_key
        .decrypt(Oaep::new::<Sha256>(), &enc)
        .context("RSA-OAEP decrypting room key")?;
    let jwk: Value = serde_json::from_slice(&pt).context("parsing room key JWK")?;
    let k_bytes = B64URL
        .decode(jwk["k"].as_str().ok_or_else(|| anyhow!("room key JWK missing k"))?)
        .context("decoding room key k")?;
    let ks: [u8; 32] = k_bytes
        .try_into()
        .map_err(|_| anyhow!("room key is not 256-bit"))?;
    Ok((key_id.to_string(), ks))
}

/// Encrypt a plaintext message in legacy AES-256-CBC (PKCS#7) mode (`<key_id><base64(iv || ct)>`).
pub fn encrypt_message_cbc(ks: &[u8; 32], key_id: &str, payload: &Value) -> Result<String> {
    let pt = serde_json::to_vec(payload)?;
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    let ct = Aes256CbcEnc::new(ks.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&pt);
    let mut combined = Vec::with_capacity(16 + ct.len());
    combined.extend_from_slice(&iv);
    combined.extend_from_slice(&ct);
    Ok(format!("{key_id}{}", B64.encode(combined)))
}

/// Decrypt a legacy AES-256-CBC message (`<key_id><base64(iv || ct)>`).
pub fn decrypt_message_cbc(ks: &[u8; 32], ciphertext_with_key_id: &str) -> Result<Value> {
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
    let pt = Aes256CbcDec::new(ks.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ct)
        .map_err(|_| anyhow!("AES-CBC decryption failed"))?;
    Ok(serde_json::from_slice(&pt).context("parsing decrypted AES-CBC JSON")?)
}

/// Encrypt a plaintext message in modern Rocket.Chat AES-256-GCM mode (`rc.v1.aes-sha2`).
pub fn encrypt_message_gcm(ks: &[u8; 32], payload: &Value) -> Result<Value> {
    let cipher = Aes256Gcm::new(ks.into());
    let mut iv = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut iv);
    let pt = serde_json::to_vec(payload)?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&iv), pt.as_ref())
        .map_err(|_| anyhow!("AES-GCM encryption failed"))?;
    let mut combined = Vec::with_capacity(12 + ct.len());
    combined.extend_from_slice(&iv);
    combined.extend_from_slice(&ct);
    Ok(json!({
        "algorithm": "rc.v1.aes-sha2",
        "ciphertext": B64.encode(combined),
    }))
}

/// Decrypt a modern Rocket.Chat AES-256-GCM message (`content.ciphertext`).
pub fn decrypt_message_gcm(ks: &[u8; 32], ciphertext_b64: &str) -> Result<Value> {
    let raw = B64
        .decode(ciphertext_b64)
        .context("decoding AES-GCM ciphertext base64")?;
    if raw.len() < 28 {
        anyhow::bail!("AES-GCM ciphertext too short");
    }
    let (iv, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new(ks.into());
    let pt = cipher
        .decrypt(Nonce::from_slice(iv), ct)
        .map_err(|_| anyhow!("AES-GCM decryption failed"))?;
    Ok(serde_json::from_slice(&pt).context("parsing decrypted AES-GCM JSON")?)
}

/// Encrypt file bytes for an E2EE room attachment (AES-256-GCM + SHA-256 integrity hash).
pub fn encrypt_file_bytes(plaintext: &[u8]) -> Result<(Vec<u8>, Value)> {
    let mut key = [0u8; 32];
    let mut iv = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut key);
    rand::thread_rng().fill_bytes(&mut iv);
    let cipher = Aes256Gcm::new(&key.into());
    let ct = cipher
        .encrypt(Nonce::from_slice(&iv), plaintext)
        .map_err(|_| anyhow!("E2EE file encryption failed"))?;
    let sha256 = B64.encode(Sha256::digest(&ct));
    let meta = json!({
        "encryption": {
            "key": { "kty": "oct", "alg": "A256GCM", "k": B64URL.encode(key) },
            "iv": B64.encode(iv),
        },
        "hashes": { "sha256": sha256 },
    });
    Ok((ct, meta))
}

/// Decrypt E2EE file attachment bytes using the message's `encryption` metadata.
pub fn decrypt_file_bytes(ciphertext: &[u8], meta: &Value) -> Result<Vec<u8>> {
    if let Some(expected_hash) = meta["hashes"]["sha256"].as_str() {
        let actual = B64.encode(Sha256::digest(ciphertext));
        if actual != expected_hash {
            anyhow::bail!("E2EE file SHA-256 mismatch");
        }
    }
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

    let inner = json!({
        "_id": uuid::Uuid::new_v4().to_string(),
        "text": text,
        "ts": chrono::Utc::now().to_rfc3339(),
    });

    if handle.server.version.supports_aes_gcm_e2ee() {
        let content = encrypt_message_gcm(&ks, &inner)?;
        Ok(json!({
            "rid": room_id,
            "t": "e2e",
            "e2e": "pending",
            "msg": "",
            "content": content,
        }))
    } else {
        let msg = encrypt_message_cbc(&ks, &key_id, &inner)?;
        Ok(json!({
            "rid": room_id,
            "t": "e2e",
            "e2e": "pending",
            "msg": msg,
        }))
    }
}

/// Decrypt an incoming Rocket.Chat E2EE message (supports both AES-GCM `content` and legacy AES-CBC `msg`).
pub fn decrypt_incoming(handle: &Handle, room_id: &str, msg: &Value) -> Option<String> {
    let ks = {
        let e2ee = handle.e2ee.lock().unwrap_or_else(|e| e.into_inner());
        e2ee.room_keys.get(room_id).map(|(_, k)| *k)?
    };
    if let Some(ct) = msg["content"]["ciphertext"].as_str().filter(|s| !s.is_empty()) {
        if let Ok(v) = decrypt_message_gcm(&ks, ct) {
            return v["text"].as_str().or_else(|| v["msg"].as_str()).map(str::to_string);
        }
    }
    if let Some(raw) = msg["msg"].as_str().filter(|s| s.len() > KEY_ID_LEN) {
        if let Ok(v) = decrypt_message_cbc(&ks, raw) {
            return v["text"].as_str().or_else(|| v["msg"].as_str()).map(str::to_string);
        }
    }
    None
}

/// Distribute `Ks` to newly joined members of an E2EE room (`e2e.getUsersOfRoomWithoutKey`,
/// `e2e.updateGroupKey`, `e2e.provideUsersSuggestedGroupKeys`).
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
    let mut suggested = serde_json::Map::new();

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
            suggested.insert(uid.to_string(), json!(wrapped));
            distributed += 1;
        }
    }

    if !suggested.is_empty() {
        let _ = handle
            .api
            .post(
                "/api/v1/e2e.provideUsersSuggestedGroupKeys",
                &json!({ "usersSuggestedGroupKeys": { room_id: Value::Object(suggested) } }),
            )
            .await;
    }

    Ok(distributed)
}

/// Reset own E2EE keypair on server (`POST /api/v1/e2e.resetOwnE2EKey`).
/// Clears local E2EE state; user will lose access to previous encrypted rooms until re-invited.
pub async fn reset_own_key(handle: &Handle) -> Result<Value> {
    let resp = handle.api.post("/api/v1/e2e.resetOwnE2EKey", &json!({})).await?;
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

        // 3. Per-room symmetric key Ks wrap/unwrap via RSA-OAEP
        let mut ks = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut ks);
        let kid = room_key_id(&ks);
        assert_eq!(kid.len(), KEY_ID_LEN);
        let e2e_key = wrap_room_key(&ks, &kid, &pub_parsed).unwrap();
        let (unwrapped_kid, unwrapped_ks) = unwrap_room_key(&e2e_key, &unwrapped_priv).unwrap();
        assert_eq!(unwrapped_kid, kid);
        assert_eq!(unwrapped_ks, ks);

        // 4. Legacy AES-CBC message encryption/decryption
        let payload = json!({ "_id": "m1", "text": "hello CBC rocket!" });
        let cbc_ct = encrypt_message_cbc(&ks, &kid, &payload).unwrap();
        assert!(cbc_ct.starts_with(&kid));
        let cbc_pt = decrypt_message_cbc(&ks, &cbc_ct).unwrap();
        assert_eq!(cbc_pt["text"], "hello CBC rocket!");

        // 5. Modern AES-GCM (`rc.v1.aes-sha2`) message encryption/decryption
        let gcm_obj = encrypt_message_gcm(&ks, &payload).unwrap();
        assert_eq!(gcm_obj["algorithm"], "rc.v1.aes-sha2");
        let gcm_pt = decrypt_message_gcm(&ks, gcm_obj["ciphertext"].as_str().unwrap()).unwrap();
        assert_eq!(gcm_pt["text"], "hello CBC rocket!");

        // 6. E2EE file encryption/decryption
        let file_bytes = b"secret image payload bytes 1234567890";
        let (enc_file, meta) = encrypt_file_bytes(file_bytes).unwrap();
        assert_ne!(enc_file.as_slice(), file_bytes);
        let dec_file = decrypt_file_bytes(&enc_file, &meta).unwrap();
        assert_eq!(dec_file.as_slice(), file_bytes);
    }
}
