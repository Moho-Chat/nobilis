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
// For `event_type()` on a verification step's content - the wire needs the
// name of the event, and only the trait knows it.
use ruma_events::MessageLikeEventContent;
use ruma_client_api::keys::{claim_keys, get_keys, upload_keys, upload_signatures};
use ruma_client_api::sync::sync_events::DeviceLists;
use ruma_client_api::to_device::send_event_to_device;
use ruma_common::serde::Raw;
use ruma_common::{DeviceId, OneTimeKeyAlgorithm, OwnedDeviceId, OwnedOneTimeKeyId, OwnedUserId, RoomId, UserId};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use tokio::sync::Mutex as AsyncMutex;

/// PBKDF2 rounds for a key export, matching what Element writes.
const EXPORT_ROUNDS: u32 = 500_000;

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
        // Asked before backing up rather than after, because the crypto
        // machine warns every time it is asked to back up without a key -
        // once per sync cycle, per account, forever. That was 451 lines in
        // one session, which is how a log stops being somewhere anybody
        // looks for the warnings that matter.
        if !backup_machine.enabled().await {
            return;
        }
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
                // Built field by field rather than as one literal, because a
                // literal turns an absent value into an explicit `null` and
                // the specification says these fields are *absent* when there
                // is nothing to send.
                //
                // Not a nicety. Synapse accepts `"device_keys": null` and
                // ignores it; a stricter homeserver validates the body and
                // refuses the whole request - poast.org answers
                // `M_INVALID_PARAM: device_keys must not be null`, on every
                // sync cycle, forever, because the retry never changes. The
                // consequence is that this device's identity is never
                // published at all: nobody else can start an Olm session with
                // it, so messages encrypted to it may be undecryptable and the
                // device is not properly announced to any room it is in.
                let mut body = serde_json::Map::new();
                if let Some(device_keys) = &req.device_keys {
                    body.insert("device_keys".into(), serde_json::to_value(device_keys)?);
                }
                // Empty maps are omitted for the same reason: "here are no
                // keys" and "I am not uploading keys" are different requests,
                // and only the second one is what an empty upload means.
                if !req.one_time_keys.is_empty() {
                    body.insert("one_time_keys".into(), serde_json::to_value(&req.one_time_keys)?);
                }
                if !req.fallback_keys.is_empty() {
                    body.insert("fallback_keys".into(), serde_json::to_value(&req.fallback_keys)?);
                }
                let body = serde_json::Value::Object(body);
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
            // The signature that makes a verification mean something outside
            // this client. Skipped here for a long time, which is why a
            // device verified in moho still read as unverified in Element:
            // the emoji matched, both sides agreed, and nobody ever told the
            // homeserver.
            AnyOutgoingRequest::SignatureUpload(req) => {
                self.post_signatures(homeserver_url, access_token, req).await?;
                let response = upload_signatures::v3::Response::new();
                self.machine.mark_request_as_sent(request.request_id(), &response).await.context("mark_request_as_sent(SignatureUpload)")?;
            }
            // A verification step sent as a room event rather than to-device.
            //
            // This is what verifying *another person* looks like: Element and
            // every other client carry a cross-user verification in the room
            // the two of you share, because there is no device to address it
            // to until you have agreed which devices you are talking about.
            AnyOutgoingRequest::RoomMessage(req) => {
                let event_type = req.content.event_type().to_string();
                let url = format!(
                    "{base}/_matrix/client/v3/rooms/{}/send/{event_type}/{}",
                    url::form_urlencoded::byte_serialize(req.room_id.as_str().as_bytes()).collect::<String>(),
                    url::form_urlencoded::byte_serialize(req.txn_id.as_str().as_bytes()).collect::<String>(),
                );
                let body = serde_json::to_value(&req.content).context("serialising a verification step")?;
                let resp = http::put_json(&url, access_token, body).await.context("sending a verification step")?;
                let response = ruma_client_api::message::send_message_event::v3::Response::new(
                    ruma_common::OwnedEventId::try_from(resp["event_id"].as_str().unwrap_or("$unknown"))
                        .unwrap_or_else(|_| ruma_common::OwnedEventId::try_from("$unknown").expect("static event id")),
                );
                self.machine.mark_request_as_sent(request.request_id(), &response).await.context("mark_request_as_sent(RoomMessage)")?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Hands the machine a verification step that arrived as a room event.
    ///
    /// In-room verification is how one person verifies another: the events
    /// travel through the room the two of them share rather than to-device,
    /// so nothing in the ordinary to-device path ever sees them. The room id
    /// is put back on the event first - a sync timeline event does not carry
    /// the room it came from, and the machine needs it to know which flow
    /// this belongs to.
    pub async fn receive_room_verification(&self, event: &Value, room_id: &str) -> Result<()> {
        let mut event = event.clone();
        if let Some(object) = event.as_object_mut() {
            object.insert("room_id".to_string(), Value::from(room_id));
        }
        let parsed: ruma_events::AnyMessageLikeEvent =
            serde_json::from_value(event).context("reading a verification event")?;
        self.machine.receive_verification_event(&parsed).await.context("receive_verification_event")?;
        Ok(())
    }

    /// Publishes signatures over somebody's keys.
    ///
    /// This is what a verification is *for*: having agreed that a device is
    /// what it claims to be, this account signs it with its own cross-signing
    /// key so every other client of yours, and everyone who trusts you, can
    /// see that agreement without repeating it.
    ///
    /// Failures come back inside a 200 - per-key, per-user - and are reported
    /// rather than swallowed: a signature the server rejected is a device
    /// that will keep showing as unverified, and silence there is the bug
    /// this whole path exists to fix.
    pub async fn post_signatures(
        &self,
        homeserver_url: &str,
        access_token: &str,
        req: &upload_signatures::v3::Request,
    ) -> Result<()> {
        if req.signed_keys.is_empty() {
            return Ok(());
        }
        let base = homeserver_url.trim_end_matches('/');
        let body = serde_json::to_value(&req.signed_keys).context("serialising signatures")?;
        let resp = http::post_json(&format!("{base}/_matrix/client/v3/keys/signatures/upload"), Some(access_token), body)
            .await
            .context("keys/signatures/upload")?;
        if let Some(failures) = resp["failures"].as_object().filter(|f| !f.is_empty()) {
            tracing::warn!("matrix: the homeserver refused some signatures: {failures:?}");
        }
        Ok(())
    }

    /// Whether this account holds each of the three cross-signing keys.
    ///
    /// The master key signs the other two; the self-signing key is what signs
    /// your own devices; the user-signing key is what signs other people. A
    /// client holding none of them can verify nothing beyond its own screen.
    pub async fn cross_signing_status(&self) -> matrix_sdk_crypto::olm::CrossSigningStatus {
        self.machine.cross_signing_status().await
    }

    /// Creates this account's cross-signing identity and publishes it.
    ///
    /// Only where there is none: `bootstrap_cross_signing(false)` uploads the
    /// existing identity again rather than replacing it, and replacing one is
    /// destructive in a way no client should do quietly - every device you
    /// have verified anywhere becomes unverified, for everyone.
    ///
    /// Password-gated because the homeserver gates it: publishing signing keys
    /// is user-interactive auth, the same as removing a device.
    pub async fn bootstrap_cross_signing(
        &self,
        homeserver_url: &str,
        access_token: &str,
        user_id: &str,
        password: &str,
    ) -> Result<()> {
        let base = homeserver_url.trim_end_matches('/');

        // The guard that matters, and the reason this is not simply a call to
        // the crate's own bootstrap: `bootstrap_cross_signing(false)` decides
        // whether to create keys by looking at what *this* client holds, and
        // an identity created by Element sits on the homeserver with its
        // private half on Element's machine. This client holds nothing, so
        // the crate would happily mint a second identity - and publishing
        // that replaces the first, un-verifying every device the account has,
        // everywhere, for everyone. The way back into an existing identity is
        // to verify this session against one that holds the keys.
        let own = UserId::parse(user_id).context("invalid own user id")?;
        let published = self.machine.get_identity(&own, None).await.context("get_identity")?;
        let held = self.machine.cross_signing_status().await;
        if published.is_some() && !held.has_master {
            anyhow::bail!(
                "this account already has a cross-signing identity - verify this session against                  one that has the keys rather than replacing it"
            );
        }

        let requests = self.machine.bootstrap_cross_signing(false).await.context("bootstrap_cross_signing")?;

        // In the order the crate documents, which is the order the server
        // needs: the device's own keys, then the signing keys, then the
        // signatures that tie them together.
        if let Some(upload) = &requests.upload_keys_req {
            self.send_one(homeserver_url, access_token, upload).await.context("uploading device keys")?;
        }

        let mut body = serde_json::Map::new();
        let keys = &requests.upload_signing_keys_req;
        if let Some(master) = &keys.master_key {
            body.insert("master_key".into(), serde_json::to_value(master)?);
        }
        if let Some(self_signing) = &keys.self_signing_key {
            body.insert("self_signing_key".into(), serde_json::to_value(self_signing)?);
        }
        if let Some(user_signing) = &keys.user_signing_key {
            body.insert("user_signing_key".into(), serde_json::to_value(user_signing)?);
        }
        http::post_with_password_uia(
            &format!("{base}/_matrix/client/v3/keys/device_signing/upload"),
            access_token,
            user_id,
            password,
            serde_json::Value::Object(body),
        )
        .await
        .context("publishing cross-signing keys")?;

        self.post_signatures(homeserver_url, access_token, &requests.upload_signatures_req)
            .await
            .context("signing this device with the new identity")?;
        Ok(())
    }

    /// Writes every room key this session holds into Element's own encrypted
    /// export format.
    ///
    /// The point of a file, next to the server-side backup already here, is
    /// that it does not depend on the homeserver being up or reachable - it
    /// is how history moves to a client that cannot see the backup, and how
    /// somebody keeps a copy of their own.
    ///
    /// The passphrase is the whole of the protection: the file is readable by
    /// anybody who has it and the words, and by nobody else.
    pub async fn export_room_keys(&self, passphrase: &str) -> Result<(String, usize)> {
        if passphrase.is_empty() {
            anyhow::bail!("an export with no passphrase is a plain copy of your keys - choose one");
        }
        let keys = self.machine.store().export_room_keys(|_| true).await.context("reading this session's room keys")?;
        let count = keys.len();
        // The same work factor Element writes, so a file from here costs an
        // attacker what a file from there does.
        let text = matrix_sdk_crypto::encrypt_room_key_export(&keys, passphrase, EXPORT_ROUNDS)
            .context("encrypting the export")?;
        Ok((text, count))
    }

    /// Reads one back in.
    ///
    /// Returns how many keys the file held and how many were new, because
    /// those are different numbers and the second is the one that answers
    /// "did that do anything".
    pub async fn import_room_keys(&self, text: &str, passphrase: &str) -> Result<(usize, usize)> {
        let keys = matrix_sdk_crypto::decrypt_room_key_export(std::io::Cursor::new(text), passphrase)
            .map_err(|e| anyhow::anyhow!("that file would not open - wrong passphrase, or not a key export ({e})"))?;
        let total = keys.len();
        let result = self
            .machine
            .store()
            .import_exported_room_keys(keys, |_, _| {})
            .await
            .context("importing the keys")?;
        Ok((result.imported_count, total))
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
    /// Takes the content ready-made, like share_and_encrypt_content does.
    /// An edit's shape is the same encrypted or not, and building it twice
    /// meant a formatted body reaching one kind of room and not the other.
    pub async fn share_and_encrypt_edit(&self, homeserver_url: &str, access_token: &str, room_id: &RoomId, member_ids: Vec<OwnedUserId>, content: Value) -> Result<Value> {
        self.ensure_keys_shared(homeserver_url, access_token, room_id, &member_ids).await?;
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
