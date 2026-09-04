//! Interactive session verification (SAS - "compare emoji") between two of
//! *our own* logged-in sessions. Self-verification only - no cross-user
//! identity verification (a materially bigger feature, not what was asked
//! for). Doesn't change the existing decrypt/share trust model
//! (`TrustRequirement::Untrusted`/`CollectStrategy::AllDevices` in
//! crypto.rs stay as-is) - verification here is purely informational/
//! trust-building for the user.
//!
//! Runs entirely over `m.key.verification.*` to-device events, which
//! already flow through the existing `crypto::receive_sync_changes` call
//! in mod.rs's sync loop - no new receive-side plumbing, only new code to
//! *query* state and *drive* the flow. `tick`, called once per sync
//! iteration (see mod.rs's run_sync), is that driving code: it notices
//! incoming requests and state changes the *other* session produced (ours
//! are driven directly by the RPC handlers below) and reflects them out as
//! events. Reusing the sync loop's own cadence this way - rather than
//! spawning a dedicated per-verification poll task - is simpler and still
//! plenty responsive: `/sync`'s long-poll returns as soon as a new
//! to-device event arrives server-side, same as any other Matrix event.

use super::crypto::CryptoSession;
use super::http;
use crate::accounts::MatrixAccountConfig;
use crate::state::AppState;
use anyhow::{Context, Result, bail};
use matrix_sdk_crypto::{Sas, SasState, VerificationRequest, VerificationRequestState};
use ruma_common::{DeviceId, UserId};
use std::sync::Arc;

/// One in-progress verification flow, keyed in Runtime by a generated id
/// (see runtime.rs's matrix_verifications doc comment). `request`/`sas`
/// are cheap Arc-backed clones (matrix-sdk-crypto's own design) - cloning
/// out of Runtime's map and calling methods on the clone still mutates the
/// one shared underlying flow, so most calls below don't need to write the
/// entry back; storing a *new* `Sas` once `start_sas()` produces one is the
/// one case that does.
#[derive(Clone)]
pub struct ActiveVerification {
    pub account_id: String,
    pub flow_id: String,
    pub request: VerificationRequest,
    pub sas: Option<Sas>,
    emoji_emitted: bool,
    done: bool,
}

async fn account_session(state: &AppState, account_id: &str) -> Result<(MatrixAccountConfig, Arc<CryptoSession>)> {
    let config = state.accounts.get_matrix(account_id).context("no such Matrix account")?;
    let session = state.runtime.get_matrix_machine(account_id).context("account is not connected")?;
    Ok((config, session))
}

/// This account's other known devices (from the last `/keys/query`) -
/// candidates to start a verification with. Excludes our own current
/// device - verifying "yourself" isn't a meaningful flow.
pub async fn list_own_devices(state: &AppState, account_id: &str) -> Result<Vec<serde_json::Value>> {
    let (config, session) = account_session(state, account_id).await?;
    let own_user_id = UserId::parse(&config.user_id).context("invalid own user_id")?;
    let own_device_id = session.machine.device_id();
    let devices = session.machine.get_user_devices(&own_user_id, None).await.context("get_user_devices")?;
    Ok(devices
        .devices()
        .filter(|d| d.device_id() != own_device_id)
        .map(|d| {
            serde_json::json!({
                "deviceId": d.device_id().to_string(),
                "displayName": d.display_name(),
                "verified": d.is_verified(),
            })
        })
        .collect())
}

/// Deletes (logs out) one of this account's own *other* sessions.
/// Requires the account password - deleting a device is UIA-gated by the
/// Matrix spec (see http.rs's delete_with_password_uia), unlike most
/// authenticated C-S API calls which only need a valid access token.
pub async fn delete_device(state: &AppState, account_id: &str, device_id: &str, password: &str) -> Result<()> {
    let (config, _session) = account_session(state, account_id).await?;
    let base = config.homeserver_url.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/devices/{}", url::form_urlencoded::byte_serialize(device_id.as_bytes()).collect::<String>());
    http::delete_with_password_uia(&url, &config.access_token, &config.user_id, password).await.context("deleting device")?;
    Ok(())
}

/// Where this account stands on cross-signing.
///
/// Three separate questions, and the client needs all three to say anything
/// useful: whether an identity exists at all, whether this device holds the
/// private keys that identity is made of, and whether this device has been
/// signed by it. A client holding no keys can still be *verified by* another
/// session; a client holding all three can verify anybody.
pub async fn cross_signing_status(state: &AppState, account_id: &str) -> Result<serde_json::Value> {
    let (config, session) = account_session(state, account_id).await?;
    let own_user_id = UserId::parse(&config.user_id).context("invalid own user_id")?;
    let status = session.cross_signing_status().await;
    let identity = session.machine.get_identity(&own_user_id, None).await.ok().flatten();
    let own_device = session
        .machine
        .get_device(&own_user_id, session.machine.device_id(), None)
        .await
        .ok()
        .flatten();
    Ok(serde_json::json!({
        "accountId": account_id,
        // Published on the homeserver, by this client or any other.
        "hasIdentity": identity.is_some(),
        "hasMaster": status.has_master,
        // What this device can do: sign your own devices, and sign other
        // people.
        "canSignDevices": status.has_self_signing,
        "canSignOthers": status.has_user_signing,
        // Whether this session itself is signed by that identity, which is
        // what every other client draws its shield from.
        "thisDeviceSigned": own_device.map(|d| d.is_cross_signed_by_owner()).unwrap_or(false),
        // A password is what the homeserver asks for before it will publish
        // signing keys, and an account signed in by token has none stored.
        "canBootstrap": !config.password.is_empty(),
    }))
}

/// Creates this account's cross-signing identity, where it has none.
pub async fn bootstrap_cross_signing(state: &AppState, account_id: &str, password: &str) -> Result<()> {
    let (config, session) = account_session(state, account_id).await?;
    let password = if password.is_empty() { config.password.clone() } else { password.to_string() };
    if password.is_empty() {
        bail!("your homeserver asks for your password before it will publish signing keys");
    }
    session
        .bootstrap_cross_signing(&config.homeserver_url, &config.access_token, &config.user_id, &password)
        .await?;
    state.events.emit(
        "matrixCrossSigning",
        cross_signing_status(state, account_id).await.unwrap_or_else(|_| serde_json::json!({ "accountId": account_id })),
    );
    Ok(())
}

fn status_event(v: &ActiveVerification, state: &str) -> serde_json::Value {
    serde_json::json!({
        "accountId": v.account_id,
        "verificationId": v.flow_id,
        "state": state,
    })
}

/// Starts verifying one of this account's own other devices. Only one
/// active verification per account for v1 - starting a new one cancels
/// any existing one first (best-effort - a cancel failing to send doesn't
/// block starting the new flow, it'll just time out on its own).
pub async fn start_verification(
    state: &AppState,
    account_id: &str,
    user_id: Option<&str>,
    device_id: &str,
) -> Result<String> {
    let (config, session) = account_session(state, account_id).await?;

    for old_id in state.runtime.matrix_verification_ids_for_account(account_id) {
        let _ = cancel(state, account_id, &old_id).await;
    }

    // Somebody else's device, or one of ours where none is named. The flow is
    // the same either way - emoji over to-device events - and what differs is
    // which key ends up signing the result: your self-signing key for your
    // own devices, your user-signing key for anybody else's.
    let whose = user_id.filter(|id| !id.is_empty()).unwrap_or(&config.user_id);
    let whose_user_id = UserId::parse(whose).context("invalid user id")?;
    let device_id_ruma = <&DeviceId>::from(device_id);
    let device = session
        .machine
        .get_device(&whose_user_id, device_id_ruma, None)
        .await
        .context("get_device")?
        .with_context(|| format!("unknown device {device_id}"))?;

    let (request, outgoing) = device.request_verification();
    let flow_id = request.flow_id().as_str().to_string();
    session
        .send_verification_request(&config.homeserver_url, &config.access_token, outgoing)
        .await
        .context("sending verification request")?;

    let verification_id = flow_id.clone();
    let v = ActiveVerification { account_id: account_id.to_string(), flow_id, request, sas: None, emoji_emitted: false, done: false };
    state.events.emit("matrixVerificationStatus", status_event(&v, "requested"));
    state.runtime.insert_matrix_verification(&verification_id, v);
    Ok(verification_id)
}

/// Accepts or declines an incoming verification request (one the *other*
/// session started - see tick's incoming-request scan below).
pub async fn respond_to_request(state: &AppState, account_id: &str, verification_id: &str, accept: bool) -> Result<()> {
    let (config, session) = account_session(state, account_id).await?;
    let v = state.runtime.get_matrix_verification(verification_id).context("no such verification")?;
    if v.account_id != account_id {
        bail!("verification does not belong to this account");
    }

    let outgoing = if accept { v.request.accept() } else { v.request.cancel() };
    if let Some(outgoing) = outgoing {
        session.send_verification_request(&config.homeserver_url, &config.access_token, outgoing).await.context("sending verification response")?;
    }

    if accept {
        state.events.emit("matrixVerificationStatus", status_event(&v, "ready"));
    } else {
        state.events.emit(
            "matrixVerificationResult",
            serde_json::json!({ "accountId": account_id, "verificationId": verification_id, "success": false, "error": "declined" }),
        );
        state.runtime.remove_matrix_verification(verification_id);
    }
    Ok(())
}

/// Confirms ("they match") or rejects ("they don't") the emoji comparison.
pub async fn confirm_sas(state: &AppState, account_id: &str, verification_id: &str, matches: bool) -> Result<()> {
    let (config, session) = account_session(state, account_id).await?;
    let v = state.runtime.get_matrix_verification(verification_id).context("no such verification")?;
    if v.account_id != account_id {
        bail!("verification does not belong to this account");
    }
    let sas = v.sas.clone().context("verification has not reached the emoji stage yet")?;

    if matches {
        let (requests, signature_upload) = sas.confirm().await.context("confirm")?;
        for req in requests {
            session.send_verification_request(&config.homeserver_url, &config.access_token, req).await.context("sending confirmation")?;
        }
        // The half that makes it mean something anywhere else. Both sides
        // agreeing is a private fact until this is published: without it a
        // device verified here still reads as unverified in Element, which is
        // exactly the complaint this path was written to answer.
        if let Some(upload) = signature_upload {
            session
                .post_signatures(&config.homeserver_url, &config.access_token, &upload)
                .await
                .context("publishing the signature for this verification")?;
        }
        if sas.is_done() {
            // Now that the other session trusts this one, ask it for the
            // cross-signing keys this client does not hold. Nothing to send
            // and nothing to wait for: the answer arrives as a to-device
            // secret through the sync loop already running.
            if let Err(e) = session.machine.query_missing_secrets_from_other_sessions().await {
                tracing::warn!("matrix: could not ask for the cross-signing keys: {e}");
            }
            state.events.emit(
                "matrixVerificationResult",
                serde_json::json!({ "accountId": account_id, "verificationId": verification_id, "success": true }),
            );
            state.runtime.remove_matrix_verification(verification_id);
        }
    } else {
        cancel(state, account_id, verification_id).await?;
    }
    Ok(())
}

/// Cancels an in-progress verification (any stage) from our side.
pub async fn cancel(state: &AppState, account_id: &str, verification_id: &str) -> Result<()> {
    let (config, session) = account_session(state, account_id).await?;
    let v = state.runtime.get_matrix_verification(verification_id).context("no such verification")?;
    if v.account_id != account_id {
        bail!("verification does not belong to this account");
    }

    let outgoing = match &v.sas {
        Some(sas) => sas.cancel(),
        None => v.request.cancel(),
    };
    if let Some(outgoing) = outgoing {
        session.send_verification_request(&config.homeserver_url, &config.access_token, outgoing).await.context("sending cancellation")?;
    }

    state.events.emit(
        "matrixVerificationResult",
        serde_json::json!({ "accountId": account_id, "verificationId": verification_id, "success": false, "error": "cancelled" }),
    );
    state.runtime.remove_matrix_verification(verification_id);
    Ok(())
}

/// Called once per sync iteration (see mod.rs's run_sync, right after
/// crypto::receive_sync_changes) to notice everything the *other* session
/// produced since the last tick: a brand-new incoming request, the other
/// side becoming ready (upgrading our own request to SAS, if we're the one
/// who started it), the SAS keys being exchanged (time to show emoji), or
/// the flow finishing/being cancelled from the other end.
pub async fn tick(state: &AppState, account_id: &str, session: &CryptoSession, own_user_id: &UserId, homeserver_url: &str, access_token: &str) {
    let known = state.runtime.matrix_known_flow_ids(account_id);
    for request in session.machine.get_verification_requests(own_user_id) {
        if request.we_started() {
            continue;
        }
        let flow_id = request.flow_id().as_str().to_string();
        if known.contains(&flow_id) {
            continue;
        }
        let other_device = request.other_device_id().map(|d| d.to_string()).unwrap_or_default();
        let v = ActiveVerification {
            account_id: account_id.to_string(),
            flow_id: flow_id.clone(),
            request,
            sas: None,
            emoji_emitted: false,
            done: false,
        };
        state.events.emit(
            "matrixVerificationIncoming",
            serde_json::json!({ "accountId": account_id, "verificationId": flow_id, "fromDevice": other_device }),
        );
        state.runtime.insert_matrix_verification(&flow_id, v);
    }

    for id in state.runtime.matrix_verification_ids_for_account(account_id) {
        let Some(mut v) = state.runtime.get_matrix_verification(&id) else { continue };
        if v.done {
            continue;
        }

        if v.sas.is_none() && v.request.we_started() && v.request.is_ready() {
            // We're the side that requested verification, and the other
            // device is now ready - upgrade to SAS ourselves and send the
            // resulting `m.key.verification.start` to-device event.
            match v.request.start_sas().await {
                Ok(Some((sas, outgoing))) => {
                    if let Err(e) = session.send_verification_request(homeserver_url, access_token, outgoing).await {
                        tracing::warn!("matrix verification[{id}]: sending SAS start failed: {e:#}");
                    }
                    v.sas = Some(sas);
                    state.events.emit("matrixVerificationStatus", status_event(&v, "started"));
                    state.runtime.update_matrix_verification(&id, v.clone());
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("matrix verification[{id}]: start_sas failed: {e:#}"),
            }
        } else if v.sas.is_none() && !v.request.we_started() {
            // We're the side that received the request. The other device's
            // `start` event (sent by the branch above) is processed
            // automatically by receive_sync_changes - the request's own
            // state has already transitioned - but the SAS proposal itself
            // still needs an explicit accept() (a separate step from the
            // request-level accept() in respond_to_request above) before
            // key exchange proceeds and emoji become available. Auto-accept
            // immediately here rather than exposing this as a second user
            // decision - real clients don't ask "accept this verification
            // method" separately from "verify this device", only the
            // eventual emoji match/no-match is a real user choice.
            if let VerificationRequestState::Transitioned { verification, .. } = v.request.state() {
                if let Some(sas) = verification.sas_v1() {
                    let sas = *sas;
                    if let Some(outgoing) = sas.accept() {
                        if let Err(e) = session.send_verification_request(homeserver_url, access_token, outgoing).await {
                            tracing::warn!("matrix verification[{id}]: sending SAS accept failed: {e:#}");
                        }
                    }
                    v.sas = Some(sas);
                    state.events.emit("matrixVerificationStatus", status_event(&v, "started"));
                    state.runtime.update_matrix_verification(&id, v.clone());
                }
            }
        }

        if v.request.is_cancelled() && v.sas.is_none() {
            let reason = v.request.cancel_info().map(|c| c.reason().to_string()).unwrap_or_default();
            state.events.emit(
                "matrixVerificationResult",
                serde_json::json!({ "accountId": account_id, "verificationId": id, "success": false, "error": reason }),
            );
            state.runtime.remove_matrix_verification(&id);
            continue;
        }

        if let Some(sas) = v.sas.clone() {
            match sas.state() {
                SasState::KeysExchanged { .. } if !v.emoji_emitted => {
                    if let Some(emoji) = sas.emoji() {
                        let emoji_json: Vec<serde_json::Value> =
                            emoji.iter().map(|e| serde_json::json!({ "symbol": e.symbol, "description": e.description })).collect();
                        state.events.emit(
                            "matrixVerificationEmoji",
                            serde_json::json!({ "accountId": account_id, "verificationId": id, "emoji": emoji_json }),
                        );
                    }
                    v.emoji_emitted = true;
                    state.runtime.update_matrix_verification(&id, v);
                }
                SasState::Done { .. } => {
                    state.events.emit(
                        "matrixVerificationResult",
                        serde_json::json!({ "accountId": account_id, "verificationId": id, "success": true }),
                    );
                    state.runtime.remove_matrix_verification(&id);
                }
                SasState::Cancelled(info) => {
                    state.events.emit(
                        "matrixVerificationResult",
                        serde_json::json!({ "accountId": account_id, "verificationId": id, "success": false, "error": info.reason() }),
                    );
                    state.runtime.remove_matrix_verification(&id);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::MatrixAccountConfig;
    use std::time::Duration;

    struct TestDevice {
        state: AppState,
        account_id: String,
        device_id: String,
        sync_task: tokio::task::JoinHandle<()>,
    }

    /// Logs in fresh (a real `/login` call, not a stored session - so this
    /// always creates a genuinely new device_id/access_token server-side)
    /// and opens its own crypto store under this test's own temp dir, then
    /// drives a minimal sync loop against the real homeserver: just enough
    /// to exercise receive_sync_changes/process_outgoing_requests/tick,
    /// without the rest of run_sync's message/room handling this test
    /// doesn't need.
    ///
    /// Deliberately does NOT call backend::matrix::spawn - that opens its
    /// crypto store under the real `~/.config/nobilis` (see mod.rs's
    /// config_dir()), keyed only by account_id, which is just `matrix:
    /// <user_id>` (see accounts.rs's MatrixAccountConfig::account_id) -
    /// no device_id component. Two sessions of the *same* real account
    /// would therefore collide on the exact same on-disk store, which
    /// defeats the entire point of this test (two genuinely independent
    /// devices, each with their own Olm identity) - so each test device
    /// gets its own store under its own temp dir instead.
    async fn spawn_test_device(homeserver: &str, username: &str, password: &str, tag: &str) -> TestDevice {
        let login = super::super::auth::login(homeserver, username, password, None).await.expect("login failed");
        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-verify-probe-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: Arc::new(crate::runtime::Runtime::new()),
            tor: Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            voice: Arc::new(crate::backend::discord_voice::VoiceState::new()),
            voice_prefs: Arc::new(crate::backend::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: Arc::new(crate::backend::irc_dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
        };
        let config = MatrixAccountConfig {
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

        let user_id = UserId::parse(&login.user_id).expect("invalid user_id");
        let device_id_ruma = <&DeviceId>::from(login.device_id.as_str());
        let session = Arc::new(CryptoSession::open(&data_dir, &account_id, &user_id, device_id_ruma).await.expect("opening crypto store"));
        state.runtime.set_matrix_machine(&account_id, session.clone());

        let sync_state = state.clone();
        let sync_account_id = account_id.clone();
        let sync_user_id = user_id.clone();
        let sync_homeserver = homeserver.to_string();
        let sync_token = login.access_token.clone();
        let tag = tag.to_string();
        let sync_task = tokio::spawn(async move {
            let mut since: Option<String> = None;
            loop {
                session.process_outgoing_requests(&sync_homeserver, &sync_token).await;
                let mut url = format!("{}/_matrix/client/v3/sync?timeout=3000&set_presence=online", sync_homeserver.trim_end_matches('/'));
                if let Some(s) = &since {
                    url.push_str("&since=");
                    url.push_str(&url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>());
                }
                let resp = match super::super::http::get_json(&url, &sync_token).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[{tag}] sync error: {e:#}");
                        continue;
                    }
                };
                super::super::crypto::receive_sync_changes(&session, &resp).await;
                session.process_outgoing_requests(&sync_homeserver, &sync_token).await;
                tick(&sync_state, &sync_account_id, &session, &sync_user_id, &sync_homeserver, &sync_token).await;
                if let Some(nb) = resp["next_batch"].as_str() {
                    since = Some(nb.to_string());
                }
            }
        });

        TestDevice { state, account_id, device_id: login.device_id, sync_task }
    }

    /// Waits until `target_device_id` specifically shows up in
    /// list_own_devices - not just "any" other device. A real account
    /// this test has been run against before (including a prior failed
    /// run that panicked before reaching its own logout cleanup) can have
    /// other leftover sessions besides the two this run just created, so
    /// picking "first" from the list isn't reliable - it's what produced
    /// this test's first failure (both sides picked the same stale
    /// leftover device instead of each other).
    async fn wait_for_device(state: &AppState, account_id: &str, target_device_id: &str) -> String {
        for _ in 0..60 {
            if let Ok(devices) = list_own_devices(state, account_id).await {
                if devices.iter().any(|d| d["deviceId"].as_str() == Some(target_device_id)) {
                    return target_device_id.to_string();
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("device {target_device_id} never became visible within 30s");
    }

    async fn wait_for_emoji(state: &AppState, verification_id: &str) -> Option<Sas> {
        for _ in 0..60 {
            if let Some(v) = state.runtime.get_matrix_verification(verification_id) {
                if let Some(sas) = v.sas {
                    if sas.emoji().is_some() {
                        return Some(sas);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        None
    }

    /// Two genuinely independent logins of the same real account (two
    /// separate OlmMachines/crypto stores - see spawn_test_device's own
    /// doc comment on why production spawn() can't be reused here) drive
    /// a full SAS verification round-trip against each other end to end,
    /// confirming both sides land on the exact same 7-emoji sequence, then
    /// that both sides consider the other's device verified afterward -
    /// the real proof, not just that the RPC-equivalent calls returned Ok.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_verification_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_verification_probe() {
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

        let a = spawn_test_device(&homeserver, &username, &password, "a").await;
        let b = spawn_test_device(&homeserver, &username, &password, "b").await;
        println!("account A: {}\naccount B: {}", a.account_id, b.account_id);

        let own_user_id = UserId::parse(&a.state.accounts.get_matrix(&a.account_id).unwrap().user_id).unwrap();
        let session_a = a.state.runtime.get_matrix_machine(&a.account_id).unwrap();
        let session_b = b.state.runtime.get_matrix_machine(&b.account_id).unwrap();

        // Ordinarily a device starts tracking (and querying keys for) its
        // own user id as a side effect of sending into a room it's a
        // member of (see crypto.rs's ensure_keys_shared) - this test never
        // sends a message, so it's done explicitly here instead.
        session_a.machine.update_tracked_users(std::iter::once(own_user_id.as_ref())).await.expect("update_tracked_users A");
        session_b.machine.update_tracked_users(std::iter::once(own_user_id.as_ref())).await.expect("update_tracked_users B");

        let device_b_id = wait_for_device(&a.state, &a.account_id, &b.device_id).await;
        let device_a_id = wait_for_device(&b.state, &b.account_id, &a.device_id).await;
        println!("A sees B's device: {device_b_id}\nB sees A's device: {device_a_id}");

        let verification_id = start_verification(&a.state, &a.account_id, None, &device_b_id).await.expect("start_verification");
        println!("A started verification {verification_id}");

        let mut b_saw_request = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if b.state.runtime.get_matrix_verification(&verification_id).is_some() {
                b_saw_request = true;
                break;
            }
        }
        assert!(b_saw_request, "B never saw the incoming verification request within 30s");

        respond_to_request(&b.state, &b.account_id, &verification_id, true).await.expect("respond_to_request");
        println!("B accepted");

        let sas_a = wait_for_emoji(&a.state, &verification_id).await.expect("A never reached the emoji stage within 30s");
        let sas_b = wait_for_emoji(&b.state, &verification_id).await.expect("B never reached the emoji stage within 30s");

        let emoji_a: Vec<&str> = sas_a.emoji().unwrap().iter().map(|e| e.symbol).collect();
        let emoji_b: Vec<&str> = sas_b.emoji().unwrap().iter().map(|e| e.symbol).collect();
        assert_eq!(emoji_a, emoji_b, "independently-computed emoji sequences did not match");
        println!("emoji match confirmed: {emoji_a:?}");

        confirm_sas(&a.state, &a.account_id, &verification_id, true).await.expect("confirm A");
        confirm_sas(&b.state, &b.account_id, &verification_id, true).await.expect("confirm B");

        let mut a_done = false;
        let mut b_done = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            a_done = a_done || a.state.runtime.get_matrix_verification(&verification_id).is_none();
            b_done = b_done || b.state.runtime.get_matrix_verification(&verification_id).is_none();
            if a_done && b_done {
                break;
            }
        }
        assert!(a_done && b_done, "verification never completed on both sides within 30s");

        let device_id_b_ruma = <&DeviceId>::from(device_b_id.as_str());
        let device_id_a_ruma = <&DeviceId>::from(device_a_id.as_str());
        let device_b_from_a =
            session_a.machine.get_device(&own_user_id, device_id_b_ruma, None).await.expect("get_device").expect("device B not found from A");
        let device_a_from_b =
            session_b.machine.get_device(&own_user_id, device_id_a_ruma, None).await.expect("get_device").expect("device A not found from B");
        assert!(device_b_from_a.is_verified(), "A does not consider B's device verified after SAS confirm");
        assert!(device_a_from_b.is_verified(), "B does not consider A's device verified after SAS confirm");
        println!("mutual device verification confirmed.");

        a.sync_task.abort();
        b.sync_task.abort();

        // This test logs into a real account twice, each a genuine new
        // device server-side (see spawn_test_device's own doc comment) -
        // clean those up rather than leaving two permanent extra sessions
        // on the real account this test happened to be run against.
        logout(&a.state, &a.account_id).await;
        logout(&b.state, &b.account_id).await;
    }

    async fn logout(state: &AppState, account_id: &str) {
        let Some(config) = state.accounts.get_matrix(account_id) else { return };
        let url = format!("{}/_matrix/client/v3/logout", config.homeserver_url.trim_end_matches('/'));
        if let Err(e) = super::super::http::post_json(&url, Some(&config.access_token), serde_json::json!({})).await {
            eprintln!("warning: failed to log out test session for {account_id}: {e:#}");
        }
    }
}
