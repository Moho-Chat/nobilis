//! Call links: a call anyone with the address (`https://signal.link/call/#key=…`) can ask to join, kept by no group.
//!
//! The room lives on Signal's calling server (the SFU) and RingRTC, in the app, makes, reads and joins it. What it needs
//! from here is proof that this account may: zero-knowledge credentials the chat server issues, presented for the one
//! link (so the calling server learns neither who asked nor which links they hold). That is all this module does,
//! besides keeping the account's own links in the shared records, where the phone lists them too.
//!
//! - `join_info`: a presentation of today's call link auth credential, the calling server's endorsement key, and the
//!   admin passkey when the link is one of the account's;
//! - `create_info`: a presentation of a create credential for a new room, and the room's public parameters;
//! - `save`, `forget`, `list`: the account's links in the shared records.

use super::storage_write::{Change, Target};
use super::Command;
use crate::state::AppState;
use base64::Engine;
use presage::libsignal_service::configuration::{ServiceConfiguration, SignalServers};
use presage::libsignal_service::protocol::Aci;
use presage::libsignal_service::zkgroup::{
    self,
    call_links::{CallLinkAuthCredentialResponse, CallLinkSecretParams, CreateCallLinkCredentialRequestContext, CreateCallLinkCredentialResponse},
    generic_server_params::GenericServerPublicParams,
    Timestamp,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Signal's production "generic" server parameters in their legacy form, which the chat server still issues call link
/// credentials under for this libsignal's credential format (Signal's apps now also carry a "standard" set, version 1,
/// that a newer libsignal reads; Signal Desktop's config carried this one through 7.x).
const GENERIC_SERVER_PUBLIC_PARAMS: &str = "AByD873dTilmOSG0TjKrvpeaKEsUmIO8Vx9BeMmftwUs9v7ikPwM8P3OHyT0+X3EUMZrSe9VUp26Wai51Q9I8mdk0hX/yo7CeFGJyzoOqn8e/i4Ygbn5HoAyXJx5eXfIbqpc0bIxzju4H/HOQeOpt6h742qii5u/cbwOhFZCsMIbElZTaeU+BWMBQiZHIGHT5IE0qCordQKZ5iPZom0HeFa8Yq0ShuEyAl0WINBiY6xE3H/9WnvzXBbMuuk//eRxXgzO8ieCeK8FwQNxbfXqZm6Ro1cMhCOF3u7xoX83QhpN";

const DAY: u64 = 86_400;

/// Credentials by redemption day (seconds), serialized.
pub type Credentials = Vec<(u64, Vec<u8>)>;

/// The auth credentials the chat server gave, by account: a week's at a time, so a join asks it once a week at most.
fn credentials() -> &'static Mutex<HashMap<String, Credentials>> {
    static CREDENTIALS: std::sync::OnceLock<Mutex<HashMap<String, Credentials>>> = std::sync::OnceLock::new();
    CREDENTIALS.get_or_init(Default::default)
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn unb64(text: &str, what: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD.decode(text).map_err(|_| format!("that is not a {what}"))
}

fn generic_params() -> GenericServerPublicParams {
    let raw = base64::engine::general_purpose::STANDARD.decode(GENERIC_SERVER_PUBLIC_PARAMS).expect("the built-in parameters are base64");
    zkgroup::deserialize(&raw).expect("the built-in parameters are valid")
}

/// Today, as the credentials count days: midnight UTC in seconds.
fn today() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    now - now % DAY
}

/// A root key is the room's secret, as RingRTC's `CallLinkRootKey` holds it (21 bytes now; the credentials read the
/// first sixteen).
fn root_key(text: &str) -> Result<Vec<u8>, String> {
    let key = unb64(text, "call link")?;
    if !(16..=64).contains(&key.len()) {
        return Err("that is not a call link".to_string());
    }
    Ok(key)
}

fn handle_for(state: &AppState, account_id: &str) -> Result<super::Handle, String> {
    if crate::net::route::router().routed(account_id) {
        return Err("Calls are not made on an account that goes through Tor: a call's sound and picture would leave Tor".to_string());
    }
    state.runtime.signal_handle(account_id).ok_or_else(|| "that Signal account is not connected".to_string())
}

async fn ask<T>(handle: &super::Handle, make: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> Command) -> Result<T, String> {
    let (done, answer) = tokio::sync::oneshot::channel();
    handle.commands.send(make(done)).map_err(|_| "that Signal account is not connected".to_string())?;
    match tokio::time::timeout(Duration::from_secs(30), answer).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the connection ended before Signal answered".to_string()),
        Err(_) => Err("Signal did not answer in time".to_string()),
    }
}

/// Today's auth credential response, from what was kept or asked for now.
async fn todays_credential(handle: &super::Handle, account_id: &str) -> Result<(u64, Vec<u8>), String> {
    let day = today();
    let kept = credentials().lock().unwrap().get(account_id).and_then(|all| all.iter().find(|(at, _)| *at == day).cloned());
    if let Some(found) = kept {
        return Ok(found);
    }
    let fresh = ask(handle, |done| Command::CallLinkCredentials { start: day, end: day + 7 * DAY, done }).await?;
    let found = fresh.iter().find(|(at, _)| *at == day).cloned();
    credentials().lock().unwrap().insert(account_id.to_string(), fresh);
    found.ok_or_else(|| "Signal gave no call link credential for today".to_string())
}

/// What RingRTC needs to read or join a call link's room (see the module's notes).
pub async fn join_info(state: &AppState, account_id: &str, key: &str) -> Result<serde_json::Value, String> {
    let handle = handle_for(state, account_id)?;
    let key = root_key(key)?;
    let (day, response) = todays_credential(&handle, account_id).await?;
    let aci = Aci::from(handle.own);
    let generic = generic_params();
    let response: CallLinkAuthCredentialResponse = zkgroup::deserialize(&response).map_err(|_| "Signal's call link credential could not be read".to_string())?;
    let at = Timestamp::from_epoch_seconds(day);
    let credential = response.receive(aci, at, &generic).map_err(|_| "Signal's call link credential did not check out".to_string())?;
    let params = CallLinkSecretParams::derive_from_root_key(&key);
    let presentation = credential.present(aci, at, &generic, &params, rand::random());
    let endorsement = ServiceConfiguration::from(SignalServers::Production).zkgroup_server_public_params.get_endorsement_public_key();
    let admin = super::storage::current(account_id, &handle.store).call_links.iter().find(|(k, _)| *k == key).map(|(_, admin)| admin.clone()).filter(|a| !a.is_empty());
    let device = state.accounts.get_signal(account_id).map(|c| c.device_id).unwrap_or(0);
    Ok(serde_json::json!({
        "accountId": account_id,
        "presentation": b64(&zkgroup::serialize(&presentation)),
        "endorsementPublicKey": b64(&zkgroup::serialize(&endorsement)),
        "adminPasskey": admin.as_deref().map(b64),
        "sfuUrl": super::calls::SFU_URL,
        "sfuCa": super::calls::SFU_CA,
        "ownAci": handle.own.to_string(),
        "deviceId": device,
    }))
}

/// What RingRTC needs to make a new room: `room` is the id RingRTC derived from the new root key.
pub async fn create_info(state: &AppState, account_id: &str, key: &str, room: &str) -> Result<serde_json::Value, String> {
    let handle = handle_for(state, account_id)?;
    let key = root_key(key)?;
    let room = unb64(room, "call link room")?;
    let aci = Aci::from(handle.own);
    let generic = generic_params();
    let context = CreateCallLinkCredentialRequestContext::new(&room, rand::random());
    let request = zkgroup::serialize(&context.get_request());
    let response = ask(&handle, |done| Command::CreateCallLinkCredential { request, done }).await?;
    let response: CreateCallLinkCredentialResponse = zkgroup::deserialize(&response).map_err(|_| "Signal's call link credential could not be read".to_string())?;
    let credential = context.receive(response, aci, &generic).map_err(|_| "Signal's call link credential did not check out".to_string())?;
    let params = CallLinkSecretParams::derive_from_root_key(&key);
    let presentation = credential.present(&room, aci, &generic, &params, rand::random());
    Ok(serde_json::json!({
        "presentation": b64(&zkgroup::serialize(&presentation)),
        "publicParams": b64(&zkgroup::serialize(&params.get_public_params())),
    }))
}

/// Keeps a link made here (with its admin passkey) in the shared records, for the account's other devices.
pub async fn save(state: &AppState, account_id: &str, key: &str, admin: &str) -> Result<(), String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let root_key = root_key(key)?;
    let admin_passkey = unb64(admin, "call link passkey")?;
    if super::storage::current(account_id, &handle.store).call_links.iter().any(|(k, _)| *k == root_key) {
        return Ok(());
    }
    ask(&handle, |done| Command::SaveCallLink { root_key, admin_passkey, done }).await
}

/// Takes a link off the account's list (marked deleted in the shared records). Its room, if this account made it, is
/// taken down by RingRTC first.
pub async fn forget(state: &AppState, account_id: &str, key: &str) -> Result<(), String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let key = root_key(key)?;
    ask(&handle, |done| Command::SharedChange { target: Target::CallLink(key), change: Change::CallLinkDelete, done }).await
}

/// The account's call links: each root key, and whether this account can manage it (it holds the admin passkey).
pub fn list(state: &AppState, account_id: &str) -> Result<serde_json::Value, String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let kept = super::storage::current(account_id, &handle.store);
    Ok(serde_json::Value::Array(
        kept.call_links.iter().map(|(key, admin)| serde_json::json!({ "rootKey": b64(key), "admin": !admin.is_empty() })).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_parameters_read() {
        let _ = generic_params();
        let endorsement = ServiceConfiguration::from(SignalServers::Production).zkgroup_server_public_params.get_endorsement_public_key();
        assert!(!zkgroup::serialize(&endorsement).is_empty());
    }

    #[test]
    fn a_root_key_is_at_least_sixteen_bytes() {
        assert!(root_key(&b64(&[7; 16])).is_ok());
        assert!(root_key(&b64(&[7; 21])).is_ok());
        assert!(root_key(&b64(&[7; 8])).is_err());
        assert!(root_key("not base64!").is_err());
    }

    #[test]
    fn today_is_midnight() {
        assert_eq!(today() % DAY, 0);
    }

    #[test]
    fn a_create_request_round_trips_through_the_wire_form() {
        let context = CreateCallLinkCredentialRequestContext::new(&[1; 32], [2; 32]);
        let request = zkgroup::serialize(&context.get_request());
        let back: zkgroup::call_links::CreateCallLinkCredentialRequest = zkgroup::deserialize(&request).unwrap();
        assert_eq!(zkgroup::serialize(&back), request);
    }
}
