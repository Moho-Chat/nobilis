//! Secret storage - the account's own safe, on the homeserver.
//!
//! ## What it is for
//!
//! A secret kept here survives losing every device. It is encrypted with a key
//! derived from one passphrase or one recovery code, stored as account data,
//! and can be opened again by a login that has nothing else - which is exactly
//! the situation every other recovery mechanism cannot help with, because they
//! all assume something local.
//!
//! moho already had a recovery key, and it was not this: `backup.rs` makes a
//! backup decryption key, keeps it in the local crypto store and hands the
//! base58 string over to be written down. That works and is not replaced here.
//! What it cannot do is hold anything else, which is why this exists - the
//! dehydrated device's pickle key has to be readable by a login that has never
//! run before, and there was nowhere to put it.
//!
//! ## The shape on the server
//!
//! Three kinds of account data, and the indirection between them is the part
//! worth holding in mind:
//!
//! - `m.secret_storage.key.<key id>` describes a key - how it was derived, and
//!   a MAC that lets a client check a typed passphrase without being able to
//!   decrypt anything. It never contains the key.
//! - `m.secret_storage.default_key` names which of those is the current one.
//! - One event per secret, named for the secret, holding the ciphertext keyed
//!   by the id of the key that encrypted it. A secret can be encrypted under
//!   several keys at once, which is how a client rotates one without losing
//!   the others.
//!
//! The cryptography is the crate's: `SecretStorageKey` does the PBKDF2, the
//! base58 and the AES-CTR-plus-HMAC. What is here is the account data around
//! it, which is the half the crate deliberately leaves to the client.

use super::http;
use anyhow::{bail, Context, Result};
use matrix_sdk_crypto::secret_storage::SecretStorageKey;
use ruma_events::secret::request::SecretName;
use ruma_events::secret_storage::key::SecretStorageKeyEventContent;
use ruma_events::EventContentFromType;
use serde_json::{json, Value};

/// Where the current key is named.
pub const DEFAULT_KEY_EVENT: &str = "m.secret_storage.default_key";

/// The prefix a key description is filed under.
pub const KEY_EVENT_PREFIX: &str = "m.secret_storage.key.";

/// The secret this was built for. Unstable, and named for its MSC, because
/// that is what Element writes too - a dehydrated device set up there has to
/// be findable here and the other way about.
pub const DEHYDRATION_SECRET: &str = "org.matrix.msc3814";

fn account_data_url(homeserver_url: &str, user_id: &str, kind: &str) -> String {
    format!(
        "{}/_matrix/client/v3/user/{}/account_data/{}",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(user_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(kind.as_bytes()).collect::<String>()
    )
}

/// Reads one global account data event, or `None` where the server has none.
///
/// A missing event answers 404 and that is an ordinary answer rather than a
/// failure: an account that has never used secret storage has none of these.
pub async fn read_account_data(homeserver_url: &str, token: &str, user_id: &str, kind: &str) -> Option<Value> {
    http::get_json(&account_data_url(homeserver_url, user_id, kind), token).await.ok()
}

pub async fn write_account_data(homeserver_url: &str, token: &str, user_id: &str, kind: &str, body: Value) -> Result<()> {
    http::put_json(&account_data_url(homeserver_url, user_id, kind), token, body)
        .await
        .with_context(|| format!("writing {kind}"))?;
    Ok(())
}

/// Which key this account's secrets are encrypted under, if any.
pub async fn default_key_id(homeserver_url: &str, token: &str, user_id: &str) -> Option<String> {
    read_account_data(homeserver_url, token, user_id, DEFAULT_KEY_EVENT)
        .await?
        .get("key")?
        .as_str()
        .map(str::to_string)
}

/// Opens the account's safe with a passphrase or a recovery code.
///
/// The description on the server says which of the two it is expecting and
/// carries a MAC, so a wrong answer is refused here rather than producing a
/// key that silently decrypts nothing.
pub async fn unlock(homeserver_url: &str, token: &str, user_id: &str, input: &str) -> Result<SecretStorageKey> {
    let Some(key_id) = default_key_id(homeserver_url, token, user_id).await else {
        bail!("this account has no secret storage yet")
    };
    let content = read_account_data(homeserver_url, token, user_id, &format!("{KEY_EVENT_PREFIX}{key_id}"))
        .await
        .context("the account names a secret storage key it does not describe")?;
    // Rebuilt from the event type as well as the body, because the key's own
    // id lives in the type - `m.secret_storage.key.<id>` - and not in the
    // content. That is why this cannot simply be deserialised.
    let raw = serde_json::value::to_raw_value(&content).context("unreadable secret storage key description")?;
    let content = SecretStorageKeyEventContent::from_parts(&format!("{KEY_EVENT_PREFIX}{key_id}"), &raw)
        .context("unreadable secret storage key description")?;
    SecretStorageKey::from_account_data(input, content)
        .map_err(|e| anyhow::anyhow!("that passphrase or recovery code did not open secret storage: {e}"))
}

/// Makes an account's secret storage, and makes it the default.
///
/// Answers with the recovery code to write down. A passphrase, where one was
/// given, is a second way in rather than a replacement - the code is what
/// works when the passphrase is forgotten, which is the case this exists for.
pub async fn create(homeserver_url: &str, token: &str, user_id: &str, passphrase: Option<&str>) -> Result<(SecretStorageKey, String)> {
    let key = match passphrase.filter(|p| !p.is_empty()) {
        Some(p) => SecretStorageKey::new_from_passphrase(p),
        None => SecretStorageKey::new(),
    };
    let recovery_code = key.to_base58();

    // The description first, then the pointer at it. In that order on purpose:
    // a default_key naming a description that does not exist is an account
    // nothing can open, while a description nothing points at is merely unused.
    write_account_data(
        homeserver_url,
        token,
        user_id,
        &format!("{KEY_EVENT_PREFIX}{}", key.key_id()),
        serde_json::to_value(key.event_content()).context("serialising the key description")?,
    )
    .await?;
    write_account_data(homeserver_url, token, user_id, DEFAULT_KEY_EVENT, json!({ "key": key.key_id() })).await?;

    Ok((key, recovery_code))
}

/// Puts a secret in the safe.
///
/// Written alongside whatever is already there rather than over it: a secret
/// may be encrypted under several keys at once, and dropping the others would
/// lock out every client holding one of them.
pub async fn store_secret(
    homeserver_url: &str,
    token: &str,
    user_id: &str,
    key: &SecretStorageKey,
    name: &str,
    plaintext: &[u8],
) -> Result<()> {
    let secret_name = SecretName::from(name);
    let encrypted = key.encrypt(plaintext.to_vec(), &secret_name);

    let mut event = read_account_data(homeserver_url, token, user_id, name).await.unwrap_or_else(|| json!({}));
    if !event["encrypted"].is_object() {
        event["encrypted"] = json!({});
    }
    event["encrypted"][key.key_id()] = serde_json::to_value(&encrypted).context("serialising the secret")?;
    write_account_data(homeserver_url, token, user_id, name, event).await
}

/// Takes a secret back out, or `None` where this key did not encrypt it.
pub async fn read_secret(
    homeserver_url: &str,
    token: &str,
    user_id: &str,
    key: &SecretStorageKey,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    let Some(event) = read_account_data(homeserver_url, token, user_id, name).await else {
        return Ok(None);
    };
    let Some(mine) = event["encrypted"].get(key.key_id()) else {
        // Stored under somebody else's key. Not an error - it means this
        // recovery code is not the one that safe was locked with.
        return Ok(None);
    };
    let data = serde_json::from_value(mine.clone()).context("unreadable stored secret")?;
    let plaintext = key
        .decrypt(&data, &SecretName::from(name))
        .map_err(|e| anyhow::anyhow!("the stored secret did not verify: {e}"))?;
    Ok(Some(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip this module exists for, without a server in it: what is
    /// put in comes back out, and only with the right key.
    #[test]
    fn a_secret_survives_being_locked_and_opened() {
        let key = SecretStorageKey::new();
        let name = SecretName::from(DEHYDRATION_SECRET);
        let secret = b"a pickle key, as it happens".to_vec();

        let encrypted = key.encrypt(secret.clone(), &name);
        assert_eq!(key.decrypt(&encrypted, &name).unwrap(), secret);
    }

    /// A different key must not open it, which is the whole security claim.
    #[test]
    fn another_key_does_not_open_it() {
        let name = SecretName::from(DEHYDRATION_SECRET);
        let encrypted = SecretStorageKey::new().encrypt(b"secret".to_vec(), &name);
        assert!(SecretStorageKey::new().decrypt(&encrypted, &name).is_err());
    }

    /// The secret's own name is part of the key derivation, so a ciphertext
    /// cannot be lifted from one secret and read as another.
    #[test]
    fn a_secret_cannot_be_read_under_a_different_name() {
        let key = SecretStorageKey::new();
        let encrypted = key.encrypt(b"secret".to_vec(), &SecretName::from(DEHYDRATION_SECRET));
        assert!(key.decrypt(&encrypted, &SecretName::from("m.megolm_backup.v1")).is_err());
    }

    /// A recovery code opens what it locked, which is what makes this worth
    /// having: the code is the thing somebody writes down.
    #[test]
    fn a_recovery_code_reopens_the_safe() {
        let key = SecretStorageKey::new();
        let code = key.to_base58();
        let content = key.event_content().clone();

        let reopened = SecretStorageKey::from_account_data(&code, content).expect("the code should open it");
        assert_eq!(reopened.key_id(), key.key_id());

        let name = SecretName::from(DEHYDRATION_SECRET);
        let encrypted = key.encrypt(b"pickle".to_vec(), &name);
        assert_eq!(reopened.decrypt(&encrypted, &name).unwrap(), b"pickle");
    }

    /// And a passphrase does too, where one was set - the description carries
    /// what is needed to derive the same key again.
    #[test]
    fn a_passphrase_reopens_the_safe() {
        let key = SecretStorageKey::new_from_passphrase("correct horse battery staple");
        let content = key.event_content().clone();
        let reopened = SecretStorageKey::from_account_data("correct horse battery staple", content.clone())
            .expect("the passphrase should open it");
        assert_eq!(reopened.key_id(), key.key_id());
        // And the wrong one is refused rather than producing a key that
        // decrypts nothing.
        assert!(SecretStorageKey::from_account_data("hunter2", content).is_err());
    }

    #[test]
    fn account_data_urls_escape_what_goes_in_them() {
        let url = account_data_url("https://h.example.org/", "@a:b.org", "m.secret_storage.key.abc");
        assert!(url.contains("%40a%3Ab.org"), "{url}");
        assert!(url.ends_with("m.secret_storage.key.abc"), "{url}");
        // The trailing slash on the homeserver must not double up.
        assert!(!url.contains("org//_matrix"), "{url}");
    }
}
