//! `OlmMachine` wrapper: the E2EE state machine glue between the
//! hand-rolled Client-Server API client (see http.rs/mod.rs) and
//! `matrix-sdk-crypto`'s Olm/Megolm implementation.
//!
//! `ruma_client_api`'s Request/Response types (`upload_keys::v3::Request`
//! etc.) do NOT implement plain `serde::Serialize`/`Deserialize` on the
//! outer struct - the `#[request]`/`#[response]` macros instead generate
//! `OutgoingRequest`/`IncomingResponse` trait impls tied to real
//! `http::Request`/`http::Response` types, for clients that want ruma to
//! own the whole HTTP transport. This backend doesn't (see mod.rs's doc
//! comment on why - it hand-rolls its own reqwest-based transport like
//! every other backend here), so this module instead reads/writes these
//! types' plain `pub` fields directly and hand-assembles the JSON bodies
//! itself, matching the wire format directly (confirmed against the C-S
//! API spec and the actual ruma-client-api 0.24.0 source, not guessed).
//! Individual field types (`Raw<T>`, `BTreeMap<OwnedXId, Raw<T>>`, etc.)
//! *do* implement real Serialize, so this is a legitimate, spec-faithful
//! approach, not a hack.
//!
//! Trust settings are hardcoded per the plan's Open decisions: no
//! cross-signing/device-verification UX exists in this project (out of
//! scope for v1), so both settings below are explicit overrides of the
//! crate's own cross-signing-assuming production defaults, not just
//! "left on default":
//! - `TrustRequirement::Untrusted` - decrypt from any device, verified or
//!   not.
//! - `CollectStrategy::AllDevices` - share room keys with every device,
//!   not just cross-signed ones (the crate's own `Default`,
//!   `IdentityBasedStrategy`, would silently fail to share with most real
//!   devices - including the user's own other clients - without
//!   cross-signing set up).

use super::http;
use anyhow::{Context, Result};
use js_int::UInt;
use matrix_sdk_crypto::olm::EncryptionSettings;
use matrix_sdk_crypto::types::requests::AnyOutgoingRequest;
use matrix_sdk_crypto::{
    CollectStrategy, DecryptionSettings, EncryptionSyncChanges, OlmMachine, TrustRequirement,
};
use matrix_sdk_sqlite::SqliteCryptoStore;
use ruma_client_api::keys::{claim_keys, get_keys, upload_keys};
use ruma_client_api::sync::sync_events::DeviceLists;
use ruma_client_api::to_device::send_event_to_device;
use ruma_common::serde::Raw;
use ruma_common::{DeviceId, OneTimeKeyAlgorithm, OwnedDeviceId, OwnedOneTimeKeyId, OwnedUserId, RoomId, UserId};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use tokio::sync::Mutex as AsyncMutex;

pub fn decryption_settings() -> DecryptionSettings {
    DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted }
}

pub fn encryption_settings() -> EncryptionSettings {
    EncryptionSettings { sharing_strategy: CollectStrategy::AllDevices, ..Default::default() }
}

/// One account's running `OlmMachine` plus the lock the crate's own
/// tutorial documents as required: `outgoing_requests()`/
/// `get_missing_sessions()`/`share_room_key()` can each return duplicate/
/// overlapping requests if called concurrently, since they're all
/// read-then-produce over the same underlying state.
pub struct CryptoSession {
    pub machine: OlmMachine,
    outgoing_lock: AsyncMutex<()>,
}

/// Turns an account id (`matrix:@alice:example.org`) into a filesystem-safe
/// directory name - `@`/`:` aren't valid on every filesystem this could
/// run on.
fn sanitize_account_id(account_id: &str) -> String {
    account_id.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect()
}

impl CryptoSession {
    /// Opens (or creates) this account's own crypto store directory under
    /// `<data_dir>/matrix-crypto/<sanitized account id>/` - mirrors the
    /// "each feature gets its own subdirectory" convention already
    /// established by the Tor cache/state dirs and the Sneedchat avatar
    /// cache. If the store already has Olm identity keys for this user/
    /// device pair, those are reused (this is what makes device_id reuse
    /// across restarts actually matter - see auth.rs's module doc).
    pub async fn open(data_dir: &Path, account_id: &str, user_id: &UserId, device_id: &DeviceId) -> Result<Self> {
        let dir = data_dir.join("matrix-crypto").join(sanitize_account_id(account_id));
        tokio::fs::create_dir_all(&dir).await.context("creating matrix crypto store directory")?;
        let store = SqliteCryptoStore::open(&dir, None).await.context("opening matrix crypto store")?;
        let machine = OlmMachine::with_store(user_id, device_id, store, None).await.context("initializing OlmMachine")?;
        Ok(Self { machine, outgoing_lock: AsyncMutex::new(()) })
    }

    /// Sends every request the machine currently wants sent (device key
    /// upload, key queries/claims, to-device messages) and feeds each
    /// response back in. Per the crate's own tutorial: a transient send
    /// failure for one request is logged and skipped rather than
    /// propagated - outgoing_requests() will just return it again next
    /// time this is called.
    pub async fn process_outgoing_requests(&self, homeserver_url: &str, access_token: &str) {
        let _guard = self.outgoing_lock.lock().await;
        let requests = match self.machine.outgoing_requests().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("matrix crypto: outgoing_requests() failed: {e}");
                return;
            }
        };
        for request in requests {
            if let Err(e) = self.send_one(homeserver_url, access_token, &request).await {
                tracing::warn!("matrix crypto: outgoing request failed (will retry next cycle): {e:#}");
            }
        }
    }

    /// Dispatches one `OutgoingVerificationRequest` - the kind returned
    /// directly by imperative verification calls (`request_verification()`,
    /// `VerificationRequest::accept()`/`start_sas()`, `Sas::confirm()`/
    /// `cancel()` - see verification.rs), as opposed to `KeysUpload`/
    /// `KeysQuery`/etc., which arrive via the queued `outgoing_requests()`
    /// path above. Reuses the exact same send_one dispatch (and, critically,
    /// its `mark_request_as_sent` call - required for the verification
    /// machine's own internal state to advance past "waiting to send", not
    /// just bookkeeping) rather than a separate code path.
    pub async fn send_verification_request(
        &self,
        homeserver_url: &str,
        access_token: &str,
        request: matrix_sdk_crypto::types::requests::OutgoingVerificationRequest,
    ) -> Result<()> {
        let outgoing: matrix_sdk_crypto::types::requests::OutgoingRequest = request.into();
        self.send_one(homeserver_url, access_token, &outgoing).await
    }

    /// Uploads any room keys that still need backing up, if this account
    /// has key backup enabled (see backup.rs's setup_recovery_key/
    /// restore_from_recovery_key). Unlike KeysUpload/KeysQuery/ToDevice/
    /// etc., a pending backup upload isn't part of the queued
    /// outgoing_requests() flow above - matrix-sdk-crypto's BackupMachine
    /// tracks it entirely separately (there's no `AnyOutgoingRequest`
    /// variant for it) - so this is its own dispatch call, not a new arm
    /// in send_one. A no-op (returns immediately) when backup isn't
    /// enabled or there's nothing new to back up.
    pub async fn run_pending_backup(&self, homeserver_url: &str, access_token: &str) {
        let backup_machine = self.machine.backup_machine();
        let pending = match backup_machine.backup().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("matrix crypto: backup_machine().backup() failed: {e}");
                return;
            }
        };
        let Some((request_id, request)) = pending else { return };

        let base = homeserver_url.trim_end_matches('/');
        let url = format!(
            "{base}/_matrix/client/v3/room_keys/keys?version={}",
            url::form_urlencoded::byte_serialize(request.version.as_bytes()).collect::<String>()
        );
        let body = serde_json::json!({ "rooms": request.rooms });
        let resp = match http::put_json(&url, access_token, body).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("matrix crypto: uploading backup keys failed (will retry next cycle): {e:#}");
                return;
            }
        };
        let etag = resp["etag"].as_str().unwrap_or_default().to_string();
        let count = resp["count"].as_u64().and_then(|n| UInt::try_from(n).ok()).unwrap_or_default();
        let response = ruma_client_api::backup::add_backup_keys::v3::Response::new(etag, count);
        if let Err(e) = self.machine.mark_request_as_sent(&request_id, &response).await {
            tracing::warn!("matrix crypto: mark_request_as_sent(KeysBackup) failed: {e}");
        }
    }

    async fn send_one(&self, homeserver_url: &str, access_token: &str, request: &matrix_sdk_crypto::types::requests::OutgoingRequest) -> Result<()> {
        let base = homeserver_url.trim_end_matches('/');
        match request.request() {
            AnyOutgoingRequest::KeysUpload(req) => {
                let body = serde_json::json!({
                    "device_keys": req.device_keys,
                    "one_time_keys": req.one_time_keys,
                    "fallback_keys": req.fallback_keys,
                });
                let resp = http::post_json(&format!("{base}/_matrix/client/v3/keys/upload"), Some(access_token), body).await.context("keys/upload")?;
                let mut one_time_key_counts: BTreeMap<OneTimeKeyAlgorithm, UInt> = BTreeMap::new();
                if let Some(obj) = resp["one_time_key_counts"].as_object() {
                    for (k, v) in obj {
                        if let Some(n) = v.as_u64() {
                            one_time_key_counts.insert(OneTimeKeyAlgorithm::from(k.as_str()), UInt::try_from(n).unwrap_or_default());
                        }
                    }
                }
                let response = upload_keys::v3::Response::new(one_time_key_counts);
                self.machine.mark_request_as_sent(request.request_id(), &response).await.context("mark_request_as_sent(KeysUpload)")?;
            }
            AnyOutgoingRequest::KeysQuery(req) => {
                let body = serde_json::json!({ "device_keys": req.device_keys });
                let resp = http::post_json(&format!("{base}/_matrix/client/v3/keys/query"), Some(access_token), body).await.context("keys/query")?;
                let mut response = get_keys::v3::Response::new();
                response.device_keys = raw_nested_map(&resp["device_keys"]);
                response.master_keys = raw_map(&resp["master_keys"]);
                response.self_signing_keys = raw_map(&resp["self_signing_keys"]);
                response.user_signing_keys = raw_map(&resp["user_signing_keys"]);
                if let Some(obj) = resp["failures"].as_object() {
                    for (k, v) in obj {
                        response.failures.insert(k.clone(), v.clone());
                    }
                }
                self.machine.mark_request_as_sent(request.request_id(), &response).await.context("mark_request_as_sent(KeysQuery)")?;
            }
            AnyOutgoingRequest::KeysClaim(req) => {
                self.send_keys_claim(homeserver_url, access_token, request.request_id(), req).await?;
            }
            AnyOutgoingRequest::ToDeviceRequest(req) => {
                self.send_to_device_request(homeserver_url, access_token, request.request_id(), req).await?;
            }
            // Signature upload / room message: never actually produced by
            // anything this backend calls - session verification here is
            // self-verification only (see verification.rs's module doc),
            // which always resolves to a to-device request, never an
            // in-room one, and this project's trust model doesn't upload
            // cross-signing signatures (see this module's doc comment).
            // Skip defensively rather than erroring.
            _ => {}
        }
        Ok(())
    }

    /// Sends a `/keys/claim` request and feeds the response back in.
    /// Shared by the general outgoing-request dispatch above (KeysClaim
    /// arrives via `outgoing_requests()` in the ordinary flow) and
    /// `share_and_encrypt` below (a fresh claim requested directly by
    /// `get_missing_sessions()`, which - unlike upload/query - hands back
    /// its request immediately rather than queuing it for the next
    /// `outgoing_requests()` call, per the crate's own API shape).
    async fn send_keys_claim(&self, homeserver_url: &str, access_token: &str, request_id: &ruma_common::TransactionId, req: &claim_keys::v3::Request) -> Result<()> {
        let base = homeserver_url.trim_end_matches('/');
        let body = serde_json::json!({ "one_time_keys": req.one_time_keys });
        let resp = http::post_json(&format!("{base}/_matrix/client/v3/keys/claim"), Some(access_token), body).await.context("keys/claim")?;
        let one_time_keys = parse_claimed_otks(&resp["one_time_keys"]);
        let response = claim_keys::v3::Response::new(one_time_keys);
        self.machine.mark_request_as_sent(request_id, &response).await.context("mark_request_as_sent(KeysClaim)")?;
        Ok(())
    }

    /// Sends one `/sendToDevice` request and feeds the (empty) response
    /// back in. Shared the same way send_keys_claim is - `share_room_key`
    /// below hands back its `ToDeviceRequest`s directly rather than
    /// queuing them.
    async fn send_to_device_request(&self, homeserver_url: &str, access_token: &str, request_id: &ruma_common::TransactionId, req: &matrix_sdk_crypto::types::requests::ToDeviceRequest) -> Result<()> {
        let base = homeserver_url.trim_end_matches('/');
        let mut messages_json = serde_json::Map::new();
        for (user_id, devices) in &req.messages {
            let mut device_map = serde_json::Map::new();
            for (device_id_or_all, content) in devices {
                let key = match device_id_or_all {
                    ruma_common::to_device::DeviceIdOrAllDevices::DeviceId(id) => id.to_string(),
                    ruma_common::to_device::DeviceIdOrAllDevices::AllDevices => "*".to_string(),
                };
                let content_value: Value = serde_json::from_str(content.json().get()).unwrap_or(Value::Null);
                device_map.insert(key, content_value);
            }
            messages_json.insert(user_id.to_string(), Value::Object(device_map));
        }
        let url = format!(
            "{base}/_matrix/client/v3/sendToDevice/{}/{}",
            url::form_urlencoded::byte_serialize(req.event_type.to_string().as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(req.txn_id.to_string().as_bytes()).collect::<String>(),
        );
        let body = serde_json::json!({ "messages": Value::Object(messages_json) });
        http::put_json(&url, access_token, body).await.context("sendToDevice")?;
        let response = send_event_to_device::v3::Response::new();
        self.machine.mark_request_as_sent(request_id, &response).await.context("mark_request_as_sent(ToDevice)")?;
        Ok(())
    }

    /// Ensures we have Olm sessions with, and have shared this room's
    /// current Megolm session with, every given member - establishing
    /// sessions/sharing keys as needed. Call this before encrypt_raw for
    /// the same room/member set. Split out from encryption itself so
    /// edits/reactions (encrypt_raw with a different event_type/content)
    /// can reuse the same key-sharing step as a plain message send.
    async fn ensure_keys_shared(&self, homeserver_url: &str, access_token: &str, room_id: &RoomId, member_ids: &[OwnedUserId]) -> Result<()> {
        let members: Vec<&UserId> = member_ids.iter().map(|u| u.as_ref()).collect();

        // Queues a key query for this room's members if any are newly
        // tracked - picked up by the main sync loop's own regular
        // process_outgoing_requests() calls (every /sync cycle), not
        // flushed synchronously here: doing so would need to re-enter the
        // same lock process_outgoing_requests() takes below, and Rust's
        // Mutex isn't reentrant. Slightly stale device-list data in the
        // rare case of sending to a brand-new member within the same
        // sync cycle they were first seen in is an acceptable edge case,
        // not a correctness problem - get_missing_sessions/share_room_key
        // still work correctly against whatever device list is current.
        //
        // Passed as a concrete `Vec<&UserId>` (cloned - cheap, just
        // pointer copies), not an `.iter().map(...)` adaptor - unlike
        // get_missing_sessions/share_room_key below (which take `impl
        // Iterator`), update_tracked_users takes `impl IntoIterator`, and
        // feeding it a `Map<Iter<...>, Closure>` adaptor type instead of
        // a concrete collection tripped a real rustc HRTB/Send-inference
        // limitation once this whole call chain was nested inside
        // tokio::spawn (confirmed by bisection - swapping only this one
        // call's argument shape was what fixed a "Send is not general
        // enough" build failure reported all the way up at rpc/mod.rs's
        // connection-handler spawn).
        self.machine.update_tracked_users(members.clone()).await.context("update_tracked_users")?;

        let _guard = self.outgoing_lock.lock().await;
        if let Some((request_id, claim_request)) = self.machine.get_missing_sessions(members.iter().copied()).await.context("get_missing_sessions")? {
            self.send_keys_claim(homeserver_url, access_token, &request_id, &claim_request).await.context("establishing missing Olm sessions")?;
        }

        let to_device_requests =
            self.machine.share_room_key(room_id, members.iter().copied(), encryption_settings()).await.context("share_room_key")?;
        for req in &to_device_requests {
            self.send_to_device_request(homeserver_url, access_token, &req.txn_id, req).await.context("sharing room key")?;
        }
        Ok(())
    }

    /// Encrypts an arbitrary event (any type - `m.room.message` for a
    /// plain send/edit, `m.reaction` for a reaction) for the room, once
    /// ensure_keys_shared has already run for the same room/members.
    /// Returns the ready-to-PUT `m.room.encrypted` content JSON.
    async fn encrypt_raw(&self, room_id: &RoomId, event_type: &str, content: Value) -> Result<Value> {
        let raw = Raw::new(&content).context("serializing content for encryption")?.cast_unchecked();
        let encrypted = self.machine.encrypt_room_event_raw(room_id, event_type, &raw).await.context("encrypt_room_event_raw")?;
        serde_json::to_value(&encrypted.content).context("serializing encrypted content")
    }

    /// Ensures keys are shared, then encrypts an arbitrary event
    /// (`event_type`/`content`) for the room. The general form behind
    /// share_and_encrypt/share_and_encrypt_edit/share_and_encrypt_reaction
    /// below - exposed directly too for callers (e.g. a reply, which
    /// needs a plain `m.room.message` but with its own `m.relates_to`)
    /// that don't fit any of those three shapes exactly.
    pub async fn share_and_encrypt_content(&self, homeserver_url: &str, access_token: &str, room_id: &RoomId, member_ids: Vec<OwnedUserId>, event_type: &str, content: Value) -> Result<Value> {
        self.ensure_keys_shared(homeserver_url, access_token, room_id, &member_ids).await?;
        self.encrypt_raw(room_id, event_type, content).await
    }

    /// Ensures keys are shared, then encrypts `body` as a plain
    /// `m.room.message` for the room. Returns the ready-to-PUT
    /// `m.room.encrypted` content JSON. See `CollectStrategy::AllDevices`
    /// in `encryption_settings()` above for why every device (not just
    /// cross-signed ones) gets the key.
    pub async fn share_and_encrypt(&self, homeserver_url: &str, access_token: &str, room_id: &RoomId, member_ids: Vec<OwnedUserId>, body: &str) -> Result<Value> {
        self.share_and_encrypt_content(homeserver_url, access_token, room_id, member_ids, "m.room.message", serde_json::json!({ "msgtype": "m.text", "body": body })).await
    }

    /// Like share_and_encrypt, but for an edit: `body` becomes the new
    /// `m.room.message` content under `m.new_content`, with a plain-text
    /// fallback body (`* body`) for clients that don't understand edits,
    /// matching the wire shape every other Matrix client sends.
    pub async fn share_and_encrypt_edit(&self, homeserver_url: &str, access_token: &str, room_id: &RoomId, member_ids: Vec<OwnedUserId>, target_event_id: &str, body: &str) -> Result<Value> {
        self.ensure_keys_shared(homeserver_url, access_token, room_id, &member_ids).await?;
        let content = serde_json::json!({
            "msgtype": "m.text",
            "body": format!("* {body}"),
            "m.new_content": { "msgtype": "m.text", "body": body },
            "m.relates_to": { "rel_type": "m.replace", "event_id": target_event_id },
        });
        self.encrypt_raw(room_id, "m.room.message", content).await
    }

    /// Like share_and_encrypt, but for a reaction (`m.reaction`) - no
    /// plain-text fallback body needed, reactions have no unencrypted
    /// rendering to fall back to.
    pub async fn share_and_encrypt_reaction(&self, homeserver_url: &str, access_token: &str, room_id: &RoomId, member_ids: Vec<OwnedUserId>, target_event_id: &str, emoji: &str) -> Result<Value> {
        self.ensure_keys_shared(homeserver_url, access_token, room_id, &member_ids).await?;
        let content = serde_json::json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": target_event_id, "key": emoji } });
        self.encrypt_raw(room_id, "m.reaction", content).await
    }
}

fn parse_claimed_otks(value: &Value) -> BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, BTreeMap<OwnedOneTimeKeyId, Raw<ruma_common::encryption::OneTimeKey>>>> {
    let mut one_time_keys = BTreeMap::new();
    if let Some(users) = value.as_object() {
        for (user_id, devices) in users {
            let Ok(user_id) = OwnedUserId::try_from(user_id.as_str()) else { continue };
            let mut device_map = BTreeMap::new();
            if let Some(devices) = devices.as_object() {
                for (device_id, keys) in devices {
                    let device_id: OwnedDeviceId = device_id.as_str().into();
                    let mut key_map = BTreeMap::new();
                    if let Some(keys) = keys.as_object() {
                        for (key_id, key_value) in keys {
                            let Ok(key_id) = OwnedOneTimeKeyId::try_from(key_id.as_str()) else { continue };
                            if let Ok(raw) = Raw::from_json_string(key_value.to_string()) {
                                key_map.insert(key_id, raw);
                            }
                        }
                    }
                    device_map.insert(device_id, key_map);
                }
            }
            one_time_keys.insert(user_id, device_map);
        }
    }
    one_time_keys
}

fn raw_map<T>(value: &Value) -> BTreeMap<OwnedUserId, Raw<T>> {
    let mut out = BTreeMap::new();
    if let Some(obj) = value.as_object() {
        for (user_id, v) in obj {
            let Ok(user_id) = OwnedUserId::try_from(user_id.as_str()) else { continue };
            if let Ok(raw) = Raw::from_json_string(v.to_string()) {
                out.insert(user_id, raw);
            }
        }
    }
    out
}

fn raw_nested_map<T>(value: &Value) -> BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, Raw<T>>> {
    let mut out = BTreeMap::new();
    if let Some(users) = value.as_object() {
        for (user_id, devices) in users {
            let Ok(user_id) = OwnedUserId::try_from(user_id.as_str()) else { continue };
            let mut device_map = BTreeMap::new();
            if let Some(devices) = devices.as_object() {
                for (device_id, v) in devices {
                    let device_id: OwnedDeviceId = device_id.as_str().into();
                    if let Ok(raw) = Raw::from_json_string(v.to_string()) {
                        device_map.insert(device_id, raw);
                    }
                }
            }
            out.insert(user_id, device_map);
        }
    }
    out
}

/// Feeds this sync response's to-device events + device-list/one-time-key
/// changes into the machine, per the crate's documented ordering
/// requirement: this MUST happen before the sync's `next_batch` token is
/// persisted, or a room key delivered in this batch can be lost on a
/// crash between the two (see mod.rs's run_sync).
pub async fn receive_sync_changes(session: &CryptoSession, sync_response: &Value) {
    let to_device_events = sync_response["to_device"]["events"].as_array().cloned().unwrap_or_default();
    let to_device_events: Vec<Raw<ruma_events::AnyToDeviceEvent>> =
        to_device_events.into_iter().filter_map(|e| Raw::from_json_string(e.to_string()).ok()).collect();

    let device_lists = parse_device_lists(&sync_response["device_lists"]);
    let one_time_keys_counts = parse_otk_counts(&sync_response["device_one_time_keys_count"]);
    let unused_fallback_keys = sync_response["device_unused_fallback_key_types"].as_array().map(|arr| {
        arr.iter().filter_map(|v| v.as_str()).map(OneTimeKeyAlgorithm::from).collect::<Vec<_>>()
    });

    let changes = EncryptionSyncChanges {
        to_device_events,
        changed_devices: &device_lists,
        one_time_keys_counts: &one_time_keys_counts,
        unused_fallback_keys: unused_fallback_keys.as_deref(),
        next_batch_token: sync_response["next_batch"].as_str().map(String::from),
    };
    if let Err(e) = session.machine.receive_sync_changes(changes, &decryption_settings()).await {
        tracing::warn!("matrix crypto: receive_sync_changes failed: {e}");
    }
}

fn parse_device_lists(value: &Value) -> DeviceLists {
    // DeviceLists (unlike the request/response wrapper types elsewhere in
    // this file) has real #[derive(Deserialize)] - it's a plain nested
    // body type, not one of the macro-generated HTTP-transport-tied
    // Request/Response structs (see this module's doc comment).
    serde_json::from_value(value.clone()).unwrap_or_default()
}

fn parse_otk_counts(value: &Value) -> BTreeMap<OneTimeKeyAlgorithm, UInt> {
    let mut out = BTreeMap::new();
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            if let Some(n) = v.as_u64() {
                out.insert(OneTimeKeyAlgorithm::from(k.as_str()), UInt::try_from(n).unwrap_or_default());
            }
        }
    }
    out
}

/// Decrypts one `m.room.encrypted` timeline event. `event` is the raw
/// event JSON exactly as it appeared in the `/sync` timeline (including
/// `type`/`event_id`/`sender`/`origin_server_ts`/`content` - the machine
/// needs the whole envelope, not just `content`). Returns the decrypted
/// event's own JSON on success.
pub async fn decrypt_room_event(session: &CryptoSession, event: &Value, room_id: &RoomId) -> Result<Value> {
    // matrix-sdk-crypto uses its own lightweight `Event<C>` wrapper here
    // (types::events::room::encrypted::EncryptedEvent), not a ruma_events
    // type - Raw<T>'s phantom marker doesn't validate/parse eagerly, so
    // this is just a type-level relabeling of the same raw JSON bytes.
    let raw: Raw<matrix_sdk_crypto::types::events::room::encrypted::EncryptedEvent> =
        Raw::from_json_string(event.to_string()).context("re-serializing event for decryption")?;
    let decrypted = session.machine.decrypt_room_event(&raw, room_id, &decryption_settings()).await.context("decrypt_room_event")?;
    let value: Value = serde_json::from_str(decrypted.event.json().get()).context("parsing decrypted event JSON")?;
    Ok(value)
}
