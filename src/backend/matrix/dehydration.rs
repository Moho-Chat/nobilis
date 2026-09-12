//! A dehydrated device: somewhere for room keys to go while you are away.
//!
//! ## The gap this closes
//!
//! Room keys are delivered to *devices that exist*. A device created today
//! cannot be sent the key for a message encrypted yesterday, because yesterday
//! it was not there to be sent to. Key backup covers most of that - a new
//! login restores history from the server - but not the window between a key
//! being shared and the new device existing, because those keys went to
//! to-device messages addressed to devices that were, and nobody re-sends them.
//!
//! A dehydrated device is a device that exists for exactly that reason. It is
//! uploaded to the homeserver in encrypted form, sits there being sent room
//! keys like any other device, and is never used to read anything. A new login
//! takes it down, decrypts it, collects everything it was sent, and puts a
//! fresh one back.
//!
//! ## Why this needed secret storage first
//!
//! The device is encrypted with a pickle key, and a new login has to be able
//! to decrypt it. A new login has no local crypto store - that is what makes
//! it new - so the key cannot be kept locally, or the feature would only ever
//! help a device that did not need helping. It goes in the account's secret
//! storage under `org.matrix.msc3814`, which is where Element puts it too, so
//! a dehydrated device set up in either is usable by the other.
//!
//! That is why `ssss.rs` exists, and why it was written before this.

use super::crypto::CryptoSession;
use super::http;
use super::ssss;
use anyhow::{bail, Context, Result};
use matrix_sdk_crypto::store::types::DehydratedDeviceKey;
use ruma_common::OwnedDeviceId;
use base64::Engine as _;
use serde_json::{json, Value};

/// The pickle key is kept in secret storage as base64 text, because account
/// data is JSON and holds no bytes - and because that is what Element writes,
/// which is what makes a device set up in either usable by the other.
fn key_from_stored(bytes: &[u8]) -> Result<DehydratedDeviceKey> {
    let text = std::str::from_utf8(bytes).context("the stored pickle key is not text")?;
    let text = text.trim();
    // Unpadded first, because that is what `to_base64` writes - vodozemac
    // encodes with STANDARD_NO_PAD, which is what caught this: decoding as
    // padded standard base64 fails with "Invalid padding" on every key this
    // daemon has ever written. Padded is still accepted, because the secret is
    // shared with whatever client set it up and not every one of them agrees.
    let raw = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(text)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(text))
        .context("the stored pickle key is not base64")?;
    DehydratedDeviceKey::from_slice(&raw).context("the stored pickle key is the wrong size")
}

/// The endpoint, still unstable and still named for its MSC.
const BASE: &str = "/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device";

/// What the device is called in somebody's device list.
const DISPLAY_NAME: &str = "moho (dehydrated)";

/// How many of the accumulated to-device messages to take at a time.
const EVENT_PAGE: u32 = 100;

/// Puts the pickle key in the account's safe, making the safe if there is none.
///
/// Answers with the recovery code where one had to be made, so it can be shown
/// once and written down. `None` means the account already had secret storage
/// and nothing new needs keeping.
pub async fn enable(
    state: &crate::state::AppState,
    account_id: &str,
    unlock_with: Option<&str>,
) -> Result<Option<String>> {
    let config = state.accounts.get_matrix(account_id).context("no such Matrix account")?;
    let session = state.runtime.get_matrix_machine(account_id).context("account is not connected")?;
    let base = config.homeserver_url.trim_end_matches('/');
    let token = &config.access_token;
    let user = &config.user_id;

    // An account that already has secret storage must be opened rather than
    // replaced: making a second one would orphan every secret in the first,
    // including a backup key somebody else's client is relying on.
    let (key, fresh_code) = match ssss::default_key_id(base, token, user).await {
        Some(_) => {
            let Some(input) = unlock_with.filter(|s| !s.is_empty()) else {
                bail!("this account already has secret storage - its passphrase or recovery code is needed to add to it")
            };
            (ssss::unlock(base, token, user, input).await?, None)
        }
        None => {
            let (key, code) = ssss::create(base, token, user, unlock_with).await?;
            (key, Some(code))
        }
    };

    // Reuse the pickle key already in the safe where there is one, so a second
    // client enabling this does not strand the device the first uploaded.
    let pickle_key = match ssss::read_secret(base, token, user, &key, ssss::DEHYDRATION_SECRET).await? {
        Some(bytes) => key_from_stored(&bytes)?,
        None => {
            let fresh = DehydratedDeviceKey::new();
            ssss::store_secret(base, token, user, &key, ssss::DEHYDRATION_SECRET, fresh.to_base64().as_bytes()).await?;
            fresh
        }
    };

    // Cached locally as well as kept in the safe. This is what lets every
    // later connect rehydrate without asking for the recovery code again -
    // and it is why the code itself is never written down here. Storing the
    // code would put the key to the account's whole safe in a config file, to
    // save typing it once on a login that by definition has nothing yet.
    session
        .machine
        .dehydrated_devices()
        .save_dehydrated_device_pickle_key(&pickle_key)
        .await
        .context("caching the pickle key")?;

    upload(&session, base, token, &pickle_key).await?;
    let _ = state.accounts.set_matrix_dehydration(account_id, true);
    Ok(fresh_code)
}

/// Creates a device and puts it on the server, replacing any already there.
async fn upload(session: &CryptoSession, base: &str, token: &str, pickle_key: &DehydratedDeviceKey) -> Result<()> {
    let devices = session.machine.dehydrated_devices();
    let device = devices.create().await.context("creating a dehydrated device")?;
    let request = device
        .keys_for_upload(DISPLAY_NAME.to_string(), pickle_key)
        .await
        .context("preparing the dehydrated device for upload")?;

    // Sent as plain JSON rather than through the crypto machine's own request
    // queue: this one is not an OutgoingRequest, and the endpoint is unstable
    // enough that ruma's own path is the only part worth borrowing.
    let body = json!({
        "device_id": request.device_id,
        "device_data": request.device_data,
        "initial_device_display_name": request.initial_device_display_name,
        "device_keys": request.device_keys,
        "one_time_keys": request.one_time_keys,
        "fallback_keys": request.fallback_keys,
    });
    http::put_json(&format!("{base}{BASE}"), token, body).await.context("uploading the dehydrated device")?;
    Ok(())
}

/// Opens the safe with a recovery code, caches the pickle key, and collects.
///
/// The one moment the code is needed: a login that has never run before has no
/// cached key, and this is how it gets one. Every connect after this uses the
/// cache, so the code is typed once rather than kept.
pub async fn rehydrate_with_code(state: &crate::state::AppState, account_id: &str, code: &str) -> Result<()> {
    let config = state.accounts.get_matrix(account_id).context("no such Matrix account")?;
    let session = state.runtime.get_matrix_machine(account_id).context("account is not connected")?;
    let base = config.homeserver_url.trim_end_matches('/');

    let key = ssss::unlock(base, &config.access_token, &config.user_id, code).await?;
    let Some(bytes) = ssss::read_secret(base, &config.access_token, &config.user_id, &key, ssss::DEHYDRATION_SECRET).await? else {
        bail!("this account's secret storage holds no dehydrated device key")
    };
    let pickle_key = key_from_stored(&bytes)?;
    session
        .machine
        .dehydrated_devices()
        .save_dehydrated_device_pickle_key(&pickle_key)
        .await
        .context("caching the pickle key")?;
    let _ = state.accounts.set_matrix_dehydration(account_id, true);

    rehydrate(state, account_id, &session, &config).await
}

/// Takes down whatever was left last time, collects what it was sent, and puts
/// a fresh one back.
///
/// Run on connect. Quiet about an account that has none - most do not, and a
/// line about it on every connect would be noise.
pub async fn rehydrate_on_connect(state: &crate::state::AppState, account_id: &str, session: &CryptoSession) {
    let Some(config) = state.accounts.get_matrix(account_id) else { return };
    if !config.dehydration_enabled {
        return;
    }
    if let Err(e) = rehydrate(state, account_id, session, &config).await {
        // Never fatal. A dehydrated device that cannot be collected costs the
        // keys it was holding; failing the connection over it would cost the
        // whole account.
        tracing::warn!("matrix[{account_id}]: collecting the dehydrated device: {e:#}");
    }
}

async fn rehydrate(
    state: &crate::state::AppState,
    account_id: &str,
    session: &CryptoSession,
    config: &crate::accounts::MatrixAccountConfig,
) -> Result<()> {
    let base = config.homeserver_url.trim_end_matches('/');
    let token = &config.access_token;
    let user = &config.user_id;

    // The locally cached key, which every connect after the first has. A
    // login that has never been given the recovery code has none, and that is
    // not an error - it is the state `rehydrate_with_code` exists to leave.
    let Some(pickle_key) = session
        .machine
        .dehydrated_devices()
        .get_dehydrated_device_pickle_key()
        .await
        .context("reading the cached pickle key")?
    else {
        tracing::debug!("matrix[{account_id}]: dehydration is on but this login has no pickle key yet");
        return Ok(());
    };
    let _ = (user, &ssss::DEHYDRATION_SECRET);

    // What is on the server now, if anything.
    let existing = http::get_json(&format!("{base}{BASE}"), token).await.ok();
    if let Some(existing) = existing {
        let device_id: OwnedDeviceId = existing["device_id"]
            .as_str()
            .context("the server's dehydrated device has no id")?
            .into();
        let data = serde_json::from_value(existing["device_data"].clone()).context("unreadable dehydrated device")?;

        let devices = session.machine.dehydrated_devices();
        let rehydrated = devices.rehydrate(&pickle_key, &device_id, data).await.context("rehydrating")?;

        let mut next: Option<String> = None;
        let mut collected = 0usize;
        loop {
            let url = match &next {
                Some(t) => format!(
                    "{base}{BASE}/{device_id}/events?limit={EVENT_PAGE}&next_batch={}",
                    url::form_urlencoded::byte_serialize(t.as_bytes()).collect::<String>()
                ),
                None => format!("{base}{BASE}/{device_id}/events?limit={EVENT_PAGE}"),
            };
            let page: Value = http::post_json(&url, Some(token), json!({})).await.context("reading its events")?;
            let events: Vec<_> = page["events"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| ruma_common::serde::Raw::from_json_string(e.to_string()).ok())
                .collect();
            if events.is_empty() {
                break;
            }
            collected += events.len();
            rehydrated
                .receive_events(events, &super::crypto::decryption_settings())
                .await
                .context("taking the keys out of its events")?;
            match page["next_batch"].as_str() {
                // The server repeats the token when there is nothing more.
                Some(t) if Some(t.to_string()) != next => next = Some(t.to_string()),
                _ => break,
            }
        }
        if collected > 0 {
            tracing::info!("matrix[{account_id}]: collected {collected} event(s) from the dehydrated device");
        }
    }

    // A fresh one, always: the old device's one-time keys are spent, and a
    // device left in place stops being able to receive after enough traffic.
    upload(session, base, token, &pickle_key).await?;
    let _ = account_id;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pickle key survives the round trip through secret storage as text.
    ///
    /// Account data is JSON and holds no bytes, so the key goes in as base64 -
    /// and has to come back out through the decode, not straight into
    /// `from_slice`. Writing this test is what caught that: passing the base64
    /// text to `from_slice` hands it 43 bytes where it wants 32, and it was
    /// wrong in the live path as well as in the test.
    #[test]
    fn a_pickle_key_survives_being_written_down_and_read_back() {
        let key = DehydratedDeviceKey::new();
        let written = key.to_base64();
        let read = key_from_stored(written.as_bytes()).expect("should read back");
        assert_eq!(read.to_base64(), written);

        // And the padded spelling of the same key, since the secret is shared
        // with whatever client wrote it.
        let padded = base64::engine::general_purpose::STANDARD
            .encode(base64::engine::general_purpose::STANDARD_NO_PAD.decode(&written).unwrap());
        assert_eq!(key_from_stored(padded.as_bytes()).unwrap().to_base64(), written);
    }

    /// Anything that is not a key is refused rather than producing one that
    /// silently decrypts nothing.
    #[test]
    fn something_that_is_not_a_key_is_refused() {
        assert!(key_from_stored(b"not base64 at all !!").is_err());
        // Valid base64, wrong length.
        assert!(key_from_stored(b"aGVsbG8").is_err());
    }

    /// The endpoint is the one the MSC names, which is what Element talks to.
    #[test]
    fn the_endpoint_is_the_one_the_msc_names() {
        assert_eq!(BASE, "/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device");
    }
}
