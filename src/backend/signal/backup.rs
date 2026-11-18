//! A Signal account kept in a file of its own, sealed with a passphrase, and brought back from one: so moho put on again
//! (a reinstall, a new disk) carries on as the same linked device instead of being linked anew.
//!
//! What it holds is everything the device is: its keys and sessions, its contacts, groups and messages, and the account's
//! settings here. That is why it is sealed (Argon2id from the passphrase, then XChaCha20-Poly1305), and why it is only
//! for putting the same device back. Signal's sessions belong to one device: if the copy it was made from carries on
//! running, the two answer for the same device and both break, so the old one must be gone first. Messages that came in
//! after the file was made were taken by the old copy and are not in it.

use crate::accounts::SignalAccountConfig;
use crate::state::AppState;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::path::Path;

const MAGIC: &[u8] = b"MOHO-SIGNAL-1\n";
const SALT: usize = 16;
const NONCE: usize = 24;
/// The shortest passphrase taken: the file holds the account's keys.
pub const SHORTEST_PASSPHRASE: usize = 8;

fn key_from(passphrase: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let params = argon2::Params::new(64 * 1024, 3, 1, Some(32)).map_err(|e| e.to_string())?;
    let mut key = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| e.to_string())?;
    Ok(key)
}

/// Seals the account's settings (JSON) and its store's bytes.
pub fn seal(passphrase: &str, config: &[u8], db: &[u8]) -> Result<Vec<u8>, String> {
    if passphrase.chars().count() < SHORTEST_PASSPHRASE {
        return Err(format!("Choose a passphrase of at least {SHORTEST_PASSPHRASE} characters"));
    }
    let salt: [u8; SALT] = rand::random();
    let nonce: [u8; NONCE] = rand::random();
    let key = key_from(passphrase, &salt)?;
    let mut plain = Vec::with_capacity(4 + config.len() + db.len());
    plain.extend_from_slice(&(config.len() as u32).to_be_bytes());
    plain.extend_from_slice(config);
    plain.extend_from_slice(db);
    let sealed = XChaCha20Poly1305::new(&key.into()).encrypt(XNonce::from_slice(&nonce), Payload { msg: &plain, aad: MAGIC }).map_err(|_| "the file could not be sealed".to_string())?;
    let mut out = Vec::with_capacity(MAGIC.len() + SALT + NONCE + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// The settings and store bytes a sealed file holds.
pub fn unseal(passphrase: &str, bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let rest = bytes.strip_prefix(MAGIC).ok_or("That is not a moho Signal account file")?;
    if rest.len() < SALT + NONCE + 16 {
        return Err("That file is cut short".to_string());
    }
    let (salt, rest) = rest.split_at(SALT);
    let (nonce, sealed) = rest.split_at(NONCE);
    let key = key_from(passphrase, salt)?;
    let plain = XChaCha20Poly1305::new(&key.into()).decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad: MAGIC }).map_err(|_| "Wrong passphrase, or the file was changed".to_string())?;
    let (length, rest) = plain.split_at_checked(4).ok_or("That file is damaged")?;
    let length = u32::from_be_bytes(length.try_into().map_err(|_| "That file is damaged")?) as usize;
    let (config, db) = rest.split_at_checked(length).ok_or("That file is damaged")?;
    Ok((config.to_vec(), db.to_vec()))
}

/// Writes a sealed copy of a linked account to `path`.
pub async fn export(state: &AppState, account_id: &str, path: &str, passphrase: &str) -> Result<(), String> {
    let config = state.accounts.get_signal(account_id).ok_or("No such Signal account")?;
    let handle = state.runtime.signal_handle(account_id).ok_or("That Signal account is not connected")?;
    if path.trim().is_empty() {
        return Err("Choose where to save it".to_string());
    }
    // A consistent copy of the store as it is now, taken by SQLite itself while the account keeps running.
    let copied = super::store::folder().join(format!("export-{}.db", uuid::Uuid::new_v4().simple()));
    let target = copied.to_string_lossy().to_string();
    let made = handle.store.read(|conn| Ok(conn.execute("VACUUM INTO ?1", [&target])?)).map_err(|e| format!("copying the account's store: {e}"));
    let db = made.and_then(|_| std::fs::read(&copied).map_err(|e| e.to_string()));
    super::store::delete_files(&copied);
    let db = db?;
    let settings = serde_json::to_vec(&config).map_err(|e| e.to_string())?;
    let passphrase = passphrase.to_string();
    let sealed = tokio::task::spawn_blocking(move || seal(&passphrase, &settings, &db)).await.map_err(|e| e.to_string())??;
    write_private(Path::new(path), &sealed).map_err(|e| format!("writing {path}: {e}"))?;
    tracing::info!("signal[{account_id}]: the account was exported");
    Ok(())
}

/// Brings an account back from a sealed file and starts it. Answers its account id.
pub async fn import(state: &AppState, path: &str, passphrase: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
    let passphrase = passphrase.to_string();
    let (settings, db) = tokio::task::spawn_blocking(move || unseal(&passphrase, &bytes)).await.map_err(|e| e.to_string())??;
    let config: SignalAccountConfig = serde_json::from_slice(&settings).map_err(|_| "That file is damaged".to_string())?;
    if !config.is_signed_in() {
        return Err("That file holds no linked account".to_string());
    }
    let account_id = config.account_id();
    if state.accounts.get_signal(&account_id).is_some() {
        return Err("That account is already here. Remove it first if this file is to replace it".to_string());
    }
    let store_path = super::store::path_for(&config.aci);
    if store_path.exists() {
        return Err("A store for that account is already on disk; remove the account first".to_string());
    }
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    write_private(&store_path, &db).map_err(|e| format!("writing the account's store: {e}"))?;
    // Opened once to be sure it is a store this moho can read (and brought up to date) before the account is added.
    if let Err(e) = super::store::SignalStore::open(&store_path, presage::model::identity::OnNewIdentity::Trust) {
        super::store::delete_files(&store_path);
        return Err(format!("That store could not be opened: {e}"));
    }
    let saved = state.accounts.add_signal(config).map_err(|e| e.to_string())?;
    super::spawn(state.clone(), saved);
    tracing::info!("signal[{account_id}]: the account was imported");
    Ok(account_id)
}

/// Writes a file only its owner can read.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_account_comes_back_only_with_its_passphrase() {
        let sealed = seal("correct horse", b"{\"aci\":\"x\"}", b"SQLite format 3\0...").unwrap();
        assert!(sealed.starts_with(MAGIC));
        let (config, db) = unseal("correct horse", &sealed).unwrap();
        assert_eq!((config.as_slice(), db.as_slice()), (&b"{\"aci\":\"x\"}"[..], &b"SQLite format 3\0..."[..]));
        assert_eq!(unseal("wrong horse", &sealed).unwrap_err(), "Wrong passphrase, or the file was changed");
        let mut changed = sealed.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(unseal("correct horse", &changed).is_err());
        assert!(unseal("correct horse", b"not a backup").is_err());
        assert!(seal("short", b"{}", b"").is_err());
    }
}
