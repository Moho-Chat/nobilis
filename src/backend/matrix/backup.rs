//! Server-side room key backup: a recovery key that lets a *new* session
//! decrypt history it never had Megolm sessions for, by fetching and
//! decrypting other sessions' room keys from the homeserver instead of
//! waiting to be re-shared them live.
//!
//! Uses `m.megolm_backup.v1.curve25519-aes-sha2`, the only backup
//! algorithm the Matrix spec defines - `matrix-sdk-crypto`'s own
//! `backups` module doc flags it as having known cryptographic flaws and
//! kept only for backwards compatibility, but it's also still what every
//! mainstream client (Element included) ships for "Secure Backup"/
//! recovery key, since no v2 algorithm has been ratified - the same
//! ecosystem-wide tradeoff, not a hazard specific to this backend.
//!
//! The recovery key (`BackupDecryptionKey`) already implements the
//! spec-exact base58 encoding via `to_base58()`/`from_base58()` - no
//! hand-rolled encoding needed here.
//!
//! `enable_backup_v1` only *activates* a key in the running `OlmMachine`
//! (an in-memory flag, not persisted) - a fresh process that already has
//! a backup configured from a prior session needs to re-activate it once
//! at connect time from what's saved in the crypto store, or ongoing
//! backup silently stops after every restart. See mod.rs's run_sync,
//! which does this right after opening the CryptoSession.

use super::crypto::CryptoSession;
use super::http;
use crate::state::AppState;
use anyhow::{Context, Result, bail};
use matrix_sdk_crypto::backups::MegolmV1BackupKey;
use matrix_sdk_crypto::olm::BackedUpRoomKey;
use matrix_sdk_crypto::store::types::BackupDecryptionKey;
use matrix_sdk_crypto::types::RoomKeyBackupInfo;
use ruma_client_api::backup::{KeyBackupData, RoomKeyBackup};
use ruma_common::OwnedRoomId;
use std::collections::BTreeMap;

async fn account_session(state: &AppState, account_id: &str) -> Result<(crate::accounts::MatrixAccountConfig, std::sync::Arc<CryptoSession>)> {
    let config = state.accounts.get_matrix(account_id).context("no such Matrix account")?;
    let session = state.runtime.get_matrix_machine(account_id).context("account is not connected")?;
    Ok((config, session))
}

/// Re-activates a previously-configured backup key in a freshly-opened
/// `OlmMachine`, if the crypto store has one saved. Call once, right after
/// opening the store, before the sync loop starts - see this module's own
/// doc comment on why `enable_backup_v1` alone doesn't survive a restart.
/// Returns whether a backup was found and re-activated.
pub async fn reactivate_on_connect(state: &AppState, account_id: &str, session: &CryptoSession) -> bool {
    let backup_machine = session.machine.backup_machine();
    let keys = match backup_machine.get_backup_keys().await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("matrix backup[{account_id}]: get_backup_keys failed: {e}");
            return false;
        }
    };
    let (Some(key), Some(version)) = (keys.decryption_key, keys.backup_version) else {
        return false;
    };

    let backup_key = key.megolm_v1_public_key();
    backup_key.set_version(version);
    if let Err(e) = backup_machine.enable_backup_v1(backup_key).await {
        tracing::warn!("matrix backup[{account_id}]: enable_backup_v1 failed: {e}");
        return false;
    }
    state.runtime.mark_matrix_backup_enabled(account_id);
    true
}

/// Generates a new recovery key and registers a fresh backup version on
/// the homeserver for it. Returns the recovery key string - show it to
/// the user *once*; nothing here persists it in plaintext (the key
/// material itself is persisted via save_decryption_key, into the same
/// crypto store as everything else in this backend - not as a plaintext
/// string anywhere).
///
/// Ongoing backup (new room keys uploaded automatically from here on)
/// piggybacks on the sync loop's existing per-cycle dispatch - see
/// crypto.rs's run_pending_backup, called from mod.rs's run_sync.
///
/// v1 limitation: always creates a brand new backup version rather than
/// checking for/reusing an existing one. Calling this twice for the same
/// account leaves the older backup version on the server, still valid
/// under the *old* recovery key but no longer receiving this device's
/// future keys - deleting old versions explicitly is a follow-up, not
/// handled here.
pub async fn setup_recovery_key(state: &AppState, account_id: &str) -> Result<String> {
    let (config, session) = account_session(state, account_id).await?;
    let base = config.homeserver_url.trim_end_matches('/');

    let key = BackupDecryptionKey::new();
    let recovery_key = key.to_base58();

    let mut backup_info = key.to_backup_info();
    let backup_machine = session.machine.backup_machine();
    backup_machine.sign_backup(&mut backup_info).await.context("signing backup auth data")?;

    let body = serde_json::to_value(&backup_info).context("serializing backup auth data")?;
    let resp =
        http::post_json(&format!("{base}/_matrix/client/v3/room_keys/version"), Some(&config.access_token), body).await.context("room_keys/version")?;
    let version = resp["version"].as_str().context("no version in room_keys/version response")?.to_string();

    let backup_key: MegolmV1BackupKey = key.megolm_v1_public_key();
    backup_key.set_version(version.clone());
    backup_machine.enable_backup_v1(backup_key).await.context("enable_backup_v1")?;
    backup_machine.save_decryption_key(Some(key), Some(version)).await.context("save_decryption_key")?;
    state.runtime.mark_matrix_backup_enabled(account_id);

    Ok(recovery_key)
}

pub struct RestoreSummary {
    pub imported_keys: usize,
    pub total_keys: usize,
}

/// Restores room keys from the account's existing backup using a
/// previously-created recovery key string, then also enables ongoing
/// backup on this session (restoring implies this device now has the
/// key, so it can participate in future uploads too - same as a fresh
/// setup_recovery_key would).
pub async fn restore_from_recovery_key(state: &AppState, account_id: &str, recovery_key: &str) -> Result<RestoreSummary> {
    let (config, session) = account_session(state, account_id).await?;
    let base = config.homeserver_url.trim_end_matches('/');

    let key = BackupDecryptionKey::from_base58(recovery_key).context("invalid recovery key")?;

    let version_resp =
        http::get_json(&format!("{base}/_matrix/client/v3/room_keys/version"), &config.access_token).await.context("fetching current backup version")?;
    let info: RoomKeyBackupInfo = serde_json::from_value(version_resp.clone()).context("parsing backup auth data")?;
    if !key.backup_key_matches(&info) {
        bail!("this recovery key doesn't match the account's current backup");
    }
    let version = version_resp["version"].as_str().context("no version in room_keys/version response")?.to_string();

    let keys_resp = http::get_json(
        &format!("{base}/_matrix/client/v3/room_keys/keys?version={}", url::form_urlencoded::byte_serialize(version.as_bytes()).collect::<String>()),
        &config.access_token,
    )
    .await
    .context("fetching backed-up keys")?;
    let rooms: BTreeMap<OwnedRoomId, RoomKeyBackup> = serde_json::from_value(keys_resp["rooms"].clone()).context("parsing backed-up room keys")?;

    let mut room_keys: BTreeMap<OwnedRoomId, BTreeMap<String, BackedUpRoomKey>> = BTreeMap::new();
    let mut total_keys = 0usize;
    for (room_id, backup) in rooms {
        let mut sessions = BTreeMap::new();
        for (session_id, raw) in backup.sessions {
            total_keys += 1;
            let data: KeyBackupData = match raw.deserialize() {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("matrix backup restore[{account_id}]: skipping unparseable key backup entry: {e:#}");
                    continue;
                }
            };
            match key.decrypt_session_data(data.session_data) {
                Ok(backed_up) => {
                    sessions.insert(session_id, backed_up);
                }
                Err(e) => tracing::warn!("matrix backup restore[{account_id}]: skipping a session that failed to decrypt: {e:#}"),
            }
        }
        if !sessions.is_empty() {
            room_keys.insert(room_id, sessions);
        }
    }

    let backup_machine = session.machine.backup_machine();
    #[allow(deprecated)]
    let result = backup_machine.import_backed_up_room_keys(room_keys, |_, _| {}).await.context("importing backed-up room keys")?;

    let backup_key = key.megolm_v1_public_key();
    backup_key.set_version(version.clone());
    backup_machine.enable_backup_v1(backup_key).await.context("enable_backup_v1")?;
    backup_machine.save_decryption_key(Some(key), Some(version)).await.context("save_decryption_key")?;
    state.runtime.mark_matrix_backup_enabled(account_id);

    Ok(RestoreSummary { imported_keys: result.imported_count, total_keys })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    struct TestDevice {
        state: AppState,
        account_id: String,
        session: std::sync::Arc<CryptoSession>,
        access_token: String,
        homeserver_url: String,
        user_id: String,
    }

    /// Logs in fresh (a real `/login` call - always a genuinely new
    /// device_id/access_token server-side) and opens its own crypto store
    /// under this test's own temp dir - same reasoning as verification.rs's
    /// own spawn_test_device: production spawn()'s crypto store lives
    /// under the real `~/.config/nobilis`, keyed only by account_id (no
    /// device_id component), so two sessions of the same real account
    /// would collide on the same on-disk store there.
    async fn login_device(homeserver: &str, username: &str, password: &str, tag: &str) -> TestDevice {
        let login = super::super::auth::login(homeserver, username, password, None).await.expect("login failed");
        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-backup-probe-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord_voice::VoiceState::new()),
        };
        let config = crate::accounts::MatrixAccountConfig {
            homeserver_url: homeserver.to_string(),
            user_id: login.user_id.clone(),
            password: password.to_string(),
            access_token: login.access_token.clone(),
            device_id: login.device_id.clone(),
            next_batch: None,
            display_name: None,
        };
        let saved = state.accounts.add_matrix(config).expect("add_matrix failed");
        let account_id = saved.account_id();

        let user_id = ruma_common::UserId::parse(&login.user_id).expect("invalid user_id");
        let device_id_ruma = <&ruma_common::DeviceId>::from(login.device_id.as_str());
        let session = std::sync::Arc::new(CryptoSession::open(&data_dir, &account_id, &user_id, device_id_ruma).await.expect("opening crypto store"));
        state.runtime.set_matrix_machine(&account_id, session.clone());

        TestDevice { state, account_id, session, access_token: login.access_token, homeserver_url: homeserver.to_string(), user_id: login.user_id }
    }

    async fn logout(homeserver_url: &str, access_token: &str) {
        let base = homeserver_url.trim_end_matches('/');
        let _ = http::post_json(&format!("{base}/_matrix/client/v3/logout"), Some(access_token), serde_json::json!({})).await;
    }

    /// Device A creates a private encrypted room, sends one message, sets
    /// up a recovery key, and force-uploads the resulting room key.
    /// Confirms the backup exists server-side with a matching public key.
    /// Then a fully independent device C - a fresh login, its own crypto
    /// store, never a member of the room's live key-sharing, no prior
    /// Megolm session for it whatsoever - restores using *only* the
    /// recovery key string and successfully decrypts the exact event A
    /// sent. That decrypt succeeding is the real proof (mirroring this
    /// project's established "prove it against a genuinely independent
    /// party" bar) - a wrong or absent recovery key would leave C with no
    /// session for the room and decrypt_room_event would simply fail.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_backup_restore_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_backup_restore_probe() {
        let Ok(username) = std::env::var("MATRIX_USERNAME") else {
            println!("MATRIX_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("MATRIX_PASSWORD") else {
            println!("MATRIX_PASSWORD not set, skipping");
            return;
        };
        let homeserver = std::env::var("MATRIX_HOMESERVER").unwrap_or_else(|_| "https://matrix.org".to_string());
        let _ = rustls::crypto::ring::default_provider().install_default();

        let a = login_device(&homeserver, &username, &password, "a").await;
        let base = homeserver.trim_end_matches('/');

        let create_resp = http::post_json(
            &format!("{base}/_matrix/client/v3/createRoom"),
            Some(&a.access_token),
            serde_json::json!({
                "preset": "private_chat",
                "name": "nobilis-matrix-backup-probe",
                "initial_state": [{"type": "m.room.encryption", "state_key": "", "content": {"algorithm": "m.megolm.v1.aes-sha2"}}],
            }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id in createRoom response").to_string();
        println!("created encrypted test room {room_id}");

        let own_user_id = ruma_common::UserId::parse(&a.user_id).expect("invalid user_id");
        a.session.machine.update_tracked_users(std::iter::once(own_user_id.as_ref())).await.expect("update_tracked_users");

        let room_id_ruma = ruma_common::RoomId::parse(&room_id).expect("invalid room id");
        let member_ids = super::super::joined_member_ids(base, &a.access_token, &room_id).await.expect("joined_member_ids");

        let probe_body = format!("backup-probe-{}", crate::model::next_message_id());
        let content =
            a.session.share_and_encrypt(&a.homeserver_url, &a.access_token, &room_id_ruma, member_ids, &probe_body).await.expect("share_and_encrypt");

        let txn_id = crate::model::next_message_id();
        let send_url = format!(
            "{base}/_matrix/client/v3/rooms/{}/send/m.room.encrypted/{}",
            url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>()
        );
        let send_resp = http::put_json(&send_url, &a.access_token, content).await.expect("sending encrypted message");
        let event_id = send_resp["event_id"].as_str().expect("no event_id in send response").to_string();
        println!("sent encrypted probe message, event {event_id}");

        // The exact raw m.room.encrypted envelope, as any client would see
        // it in a timeline - reused below for device C's decrypt call.
        let event_url = format!(
            "{base}/_matrix/client/v3/rooms/{}/event/{}",
            url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(event_id.as_bytes()).collect::<String>()
        );
        let raw_event = http::get_json(&event_url, &a.access_token).await.expect("fetching sent event");

        let recovery_key = setup_recovery_key(&a.state, &a.account_id).await.expect("setup_recovery_key");

        // Force the just-created room key to actually upload now, rather
        // than waiting for a sync cycle - this test doesn't run a's sync
        // loop (see login_device's own doc comment on why not).
        a.session.run_pending_backup(&a.homeserver_url, &a.access_token).await;

        let version_resp = http::get_json(&format!("{base}/_matrix/client/v3/room_keys/version"), &a.access_token).await.expect("room_keys/version");
        let info: RoomKeyBackupInfo = serde_json::from_value(version_resp.clone()).expect("parsing backup auth data");
        let recovery_key_obj = BackupDecryptionKey::from_base58(&recovery_key).expect("parsing our own recovery key");
        assert!(recovery_key_obj.backup_key_matches(&info), "server's backup public key doesn't match the recovery key we were given");
        println!("server-side backup version confirmed: {}", version_resp["version"]);

        let c = login_device(&homeserver, &username, &password, "c").await;

        let summary = restore_from_recovery_key(&c.state, &c.account_id, &recovery_key).await.expect("restore_from_recovery_key");
        println!("restore summary: imported {}/{} keys", summary.imported_keys, summary.total_keys);
        assert!(summary.imported_keys >= 1, "expected at least the probe message's room key to be imported");

        let decrypted = super::super::crypto::decrypt_room_event(&c.session, &raw_event, &room_id_ruma).await.expect("decrypt_room_event on device C");
        let decrypted_body = decrypted["content"]["body"].as_str().unwrap_or_default();
        assert_eq!(decrypted_body, probe_body, "device C decrypted the wrong content (or decryption silently produced garbage)");
        println!("independent restore+decrypt confirmed: {decrypted_body}");

        logout(&a.homeserver_url, &a.access_token).await;
        logout(&c.homeserver_url, &c.access_token).await;
    }
}
