//! Matrix backend: full end-to-end encryption via `matrix-sdk-crypto` (the
//! standalone, vodozemac-backed Olm/Megolm state machine `matrix-sdk`
//! itself uses internally - not the full `matrix-sdk` crate, which would
//! fight this project's hand-rolled-per-backend convention with its own
//! opinionated HTTP/sync/state-store stack).
//!
//! Unlike Sneedchat (one websocket per room, since that protocol only lets
//! a connection join a single room) or Discord (one persistent gateway
//! connection), Matrix's `/sync` endpoint delivers events for every joined
//! room over a single long-polled HTTP connection - so this backend needs
//! exactly one connection per account.
//!
//! Module layout:
//! - `auth`    - m.login.password login, device_id persistence
//! - `http`    - thin reqwest-based Client-Server API client
//! - `protocol`- C-S API event-type constants + small extraction helpers
//! - `rooms`   - room_id <-> buffer naming/kind derivation
//! - `crypto`  - OlmMachine wrapper (added in phase 3)
//! - `verification` - interactive SAS session verification
//! - `backup`   - server-side room key backup / recovery key
//! - `roomstate` - member/room avatars + power levels
//! - `moderation` - kick/ban/unban/mute + permission checks

pub mod auth;
pub mod backup;
pub mod crypto;
pub mod http;
pub mod markup;
pub mod moderation;
pub mod polls;
pub mod protocol;
pub mod roomstate;
pub mod rooms;
pub mod verification;

use crate::accounts::MatrixAccountConfig;
use crate::model;
use crate::model::Attachment;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{Context, Result};
use futures::FutureExt;
use serde_json::Value;
use std::time::Duration;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);
const SYNC_LONG_POLL_MS: u64 = 30_000;

pub fn spawn(state: AppState, config: MatrixAccountConfig) {
    let account_id = config.account_id();
    // Same guard backend::discord::spawn/backend::irc::spawn need - see
    // Runtime::reset_connection's doc comment. Also what makes
    // disconnect()/removeAccount reliably stop the retry loop below.
    state.runtime.reset_connection(&account_id);
    let join_handle = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run_with_retry(&state, &config, &account_id).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, join_handle.abort_handle());
}

/// Keeps re-establishing the sync loop for as long as this task lives -
/// same exponential-backoff shape as backend::discord::run_gateway_with_retry
/// (3s -> 60s, no permanent give-up, including for a bad/revoked
/// password - it just keeps failing visibly). Only stops when this task
/// itself is aborted from outside.
async fn run_with_retry(state: &AppState, config: &MatrixAccountConfig, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        // Re-fetch the stored config each attempt, in case a re-login
        // inside a previous attempt updated access_token/device_id/
        // next_batch (state.accounts.set_matrix_session/next_batch) after
        // this loop's own `config` snapshot was taken.
        let current = state.accounts.get_matrix(account_id).unwrap_or_else(|| config.clone());
        let result = std::panic::AssertUnwindSafe(run_sync(state, &current, account_id)).catch_unwind().await;
        let detail = match result {
            Ok(Ok(())) => "sync loop ended".to_string(),
            Ok(Err(e)) => {
                tracing::warn!("matrix[{account_id}]: {e}");
                format!("{e:#}")
            }
            Err(_) => {
                tracing::error!("matrix[{account_id}]: connection task panicked");
                "internal error (see nobilis logs)".to_string()
            }
        };
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        state.runtime.report_progress(state, account_id, &format!("{detail} - reconnecting in {}s...", delay.as_secs()));
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
    }
}

fn is_auth_error(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("M_UNKNOWN_TOKEN") || s.contains("M_MISSING_TOKEN") || s.contains("HTTP 401")
}

/// Logs in (or re-logs-in) and persists the fresh session - `device_id` is
/// threaded through so a re-login reuses the *same* device rather than
/// minting a fresh one (see auth.rs's module doc for why that matters).
async fn ensure_login(state: &AppState, config: &MatrixAccountConfig, account_id: &str) -> Result<(String, String)> {
    let device_id = if config.device_id.is_empty() { None } else { Some(config.device_id.as_str()) };
    let login = auth::login(&config.homeserver_url, &config.user_id, &config.password, device_id)
        .await
        .context("logging in")?;
    let _ = state.accounts.set_matrix_session(account_id, &login.access_token, &login.device_id);
    Ok((login.access_token, login.device_id))
}

/// One sync-loop "session": logs in if there's no usable access_token yet,
/// then long-polls `/sync` forever, processing each response and
/// re-persisting `next_batch` (throttled). Returns Err on anything that
/// should trigger the outer backoff/retry - a fresh 401 is instead handled
/// inline via a re-login, not treated as session-ending, since that's a
/// routine occurrence (expired session) rather than a real connectivity
/// problem.
async fn run_sync(state: &AppState, config: &MatrixAccountConfig, account_id: &str) -> Result<()> {
    let mut access_token = config.access_token.clone();
    let mut device_id = config.device_id.clone();
    let mut next_batch = config.next_batch.clone();

    // A stored access_token can be empty (account added by hand, or never
    // successfully logged in) - log in up front rather than waiting for
    // the first /sync call to 401.
    if access_token.is_empty() {
        let (token, dev) = ensure_login(state, config, account_id).await?;
        access_token = token;
        device_id = dev;
    }

    // Without this, Runtime::record_message's own_nick/is_own computation
    // (used by both the DM/highlight-notification check and the frontend's
    // optimistic-send echo reconciliation)
    // silently stays blank forever, since Matrix has no irc_current_nick
    // equivalent for it to fall back on. The visible symptom, confirmed
    // live: every message this account sends shows up twice - once as the
    // never-resolved local optimistic echo, once as the real incoming
    // event, because the frontend never recognizes the real one as "ours"
    // to reconcile against.
    state.runtime.set_own_identity(account_id, &protocol::mxid_localpart(&config.user_id));

    let user_id = ruma_common::UserId::parse(&config.user_id).context("invalid own user_id")?;
    let device_id_ruma = <&ruma_common::DeviceId>::from(device_id.as_str());
    let session = std::sync::Arc::new(
        crypto::CryptoSession::open(&config_dir(), account_id, &user_id, device_id_ruma).await.context("opening crypto store")?,
    );
    state.runtime.set_matrix_machine(account_id, session.clone());
    backup::reactivate_on_connect(state, account_id, &session).await;

    // A resumed connection (next_batch already persisted) only ever gets
    // *incremental* `/sync` responses from here on - rooms.join in any
    // given response only contains rooms with new activity since that
    // cursor, not a full membership listing (see process_sync_response's
    // own doc comment: a room's full state only ever arrives the first
    // time /sync ever mentions it). Runtime's buffer list is in-memory
    // only, rebuilt from scratch every process restart - so without this,
    // a room that's had zero activity since the last persisted sync
    // simply never gets a buffer again after a restart, even though the
    // account is still very much a member of it. Confirmed live: after
    // several restarts with no new messages in most rooms, listBuffers
    // came back with zero Matrix buffers despite the account showing
    // "connected" and genuinely being in many rooms. A fresh account
    // (next_batch still None) doesn't need this - its first /sync already
    // sends full state for every joined room on its own.
    if next_batch.is_some() {
        if let Err(e) = bootstrap_joined_rooms(state, account_id, &config.user_id, &config.homeserver_url, &access_token).await {
            tracing::warn!("matrix[{account_id}]: bootstrapping joined rooms failed: {e:#}");
        }
    }

    // Read markers, once, for every room at once - see fetch_read_receipts.
    if let Err(e) = fetch_read_receipts(state, account_id, &config.user_id, &config.homeserver_url, &access_token).await {
        tracing::debug!("matrix[{account_id}]: read receipts: {e:#}");
    }

    // And the invitations already waiting - see fetch_pending_invites.
    if let Err(e) = fetch_pending_invites(state, account_id, &config.user_id, &config.homeserver_url, &access_token).await {
        tracing::debug!("matrix[{account_id}]: pending invites: {e:#}");
    }

    // And what this account has asked to be told about - see fetch_push_rules.
    fetch_push_rules(state, account_id, &config.homeserver_url, &access_token).await;
    // And who it has asked never to hear from.
    fetch_ignored_users(state, account_id, &config.homeserver_url, &access_token, &config.user_id).await;

    // No RAII drop-guard here (deliberately) - run_with_retry's own
    // catch_unwind wraps this whole function, so a plain `?` return
    // below is the only exit path apart from the infinite loop; the
    // machine entry is removed explicitly at that one site instead of
    // via Drop, since a Drop impl holding a borrowed &AppState alive
    // across every await in the loop below is exactly the shape of
    // thing that can defeat the Send-generality check tokio::spawn
    // needs elsewhere in the program.

    let mut last_persisted_batch = std::time::Instant::now();
    // Tracks "have we told the frontend Connected yet *this attempt*" -
    // deliberately not the same thing as "is this the very first /sync of
    // this account's lifetime" (next_batch.is_none()). A long-running
    // account restarts with its next_batch already persisted from a prior
    // session, so every sync from process start onward is a *resumed*
    // one, is_none() is false from the very first iteration, and a
    // Connected transition gated on is_none() would never fire at all -
    // confirmed live as the actual cause of an account showing
    // "connecting" forever despite genuinely being connected and
    // receiving messages the whole time.
    let mut announced_connected = false;

    loop {
        // Advertise E2EE capability / send anything the machine already
        // wants sent (e.g. a fresh device's initial key upload) before
        // syncing, same order the crate's own tutorial documents.
        session.process_outgoing_requests(&config.homeserver_url, &access_token).await;

        // Re-read each poll rather than captured once: a status set
        // mid-session has to take effect on the next sync, not the next
        // reconnect.
        let presence = match state.runtime.account_status(account_id).as_str() {
            "idle" => "unavailable",
            _ => "online",
        };
        let url = sync_url(&config.homeserver_url, next_batch.as_deref(), presence);
        let resp = match http::get_json(&url, &access_token).await {
            Ok(v) => v,
            Err(e) if is_auth_error(&e) => {
                tracing::info!("matrix[{account_id}]: session rejected, re-logging in");
                let (token, _device_id) = match ensure_login(state, config, account_id).await.context("re-login after 401") {
                    Ok(v) => v,
                    Err(e) => {
                        state.runtime.remove_matrix_machine(account_id);
                        return Err(e);
                    }
                };
                access_token = token;
                continue;
            }
            Err(e) => {
                state.runtime.remove_matrix_machine(account_id);
                return Err(e).context("sync request failed");
            }
        };

        if !announced_connected {
            state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
            announced_connected = true;
        }

        // Must happen before `next_batch` is persisted below - see
        // crypto::receive_sync_changes's doc comment (a room key
        // delivered in this batch can be lost on a crash between the
        // two otherwise).
        crypto::receive_sync_changes(&session, &resp).await;
        // Receiving those changes can itself produce new outgoing
        // requests (e.g. claiming one-time keys to establish a session
        // with a device that just sent us a room key) - send those too
        // before moving on, same as the tutorial's sync() sketch.
        session.process_outgoing_requests(&config.homeserver_url, &access_token).await;

        verification::tick(state, account_id, &session, &user_id, &config.homeserver_url, &access_token).await;
        session.run_pending_backup(&config.homeserver_url, &access_token).await;

        process_sync_response(state, account_id, &config.user_id, &config.homeserver_url, &access_token, &resp, &session).await;

        if let Some(nb) = resp["next_batch"].as_str() {
            next_batch = Some(nb.to_string());
            // Throttled - see MatrixAccountConfig::next_batch's doc
            // comment on why persisting on every sync would be wasteful.
            if last_persisted_batch.elapsed() >= Duration::from_secs(10) {
                let _ = state.accounts.set_matrix_next_batch(account_id, nb);
                last_persisted_batch = std::time::Instant::now();
            }
        }
    }
}

fn config_dir() -> std::path::PathBuf {
    // The one the daemon actually runs out of, rather than a second guess at
    // it - these disagreed on Windows, where the hardcoded XDG shape put the
    // crypto store somewhere the rest of the daemon was not looking.
    crate::default_data_dir()
}

fn sync_url(homeserver_url: &str, since: Option<&str>, presence: &str) -> String {
    let mut url = format!(
        // Matrix ties presence to syncing, so the status has to ride along
        // with every sync rather than being set once - "online" here would
        // quietly undo an idle or DND setting on the next poll.
        "{}/_matrix/client/v3/sync?timeout={}&set_presence={}",
        homeserver_url.trim_end_matches('/'),
        if since.is_none() { 0 } else { SYNC_LONG_POLL_MS },
        presence
    );
    if let Some(since) = since {
        url.push_str("&since=");
        url.push_str(&url::form_urlencoded::byte_serialize(since.as_bytes()).collect::<String>());
    }
    url
}

/// Ensures every room this account is currently joined to has a buffer,
/// regardless of whether any of them show up in the next incremental
/// `/sync` response - see run_sync's own call site for why this is needed
/// on a resumed connection. `/joined_rooms` gives the membership list;
/// each room's `/state` endpoint gives the same kind of full state-event
/// array `process_sync_response` already knows how to derive a name and
/// encryption flag from for a room it's seeing for the first time - reused
/// directly rather than duplicating that logic.
async fn bootstrap_joined_rooms(state: &AppState, account_id: &str, own_user_id: &str, homeserver_url: &str, access_token: &str) -> Result<()> {
    let base = homeserver_url.trim_end_matches('/');
    let resp = http::get_json(&format!("{base}/_matrix/client/v3/joined_rooms"), access_token).await.context("joined_rooms")?;
    let room_ids: Vec<String> = resp["joined_rooms"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();

    for room_id in room_ids {
        // Already known (e.g. this account never actually restarted, or a
        // prior bootstrap/first-sync already covered it this run) - skip
        // the extra request rather than re-deriving needlessly.
        if state.runtime.get_matrix_room_name(account_id, &room_id).is_some() {
            continue;
        }
        register_room(state, account_id, own_user_id, homeserver_url, access_token, &room_id, false).await;
    }
    Ok(())
}

/// Gives a room a buffer, named and furnished, without waiting for a sync to
/// mention it.
///
/// The room's own `/state` is everything needed to name it, know whether it is
/// encrypted, and build its member list - which is why both the reconnect
/// bootstrap and joining a room can use it. A sync will say all of this again
/// later; this only means the room is *there* in the meantime.
///
/// `syncing` marks the buffer as still filling in. Joining sets it, because a
/// room joined and then silent for several seconds looks like a button that
/// did nothing; the bootstrap does not, since those rooms are being restored
/// rather than waited for.
async fn register_room(
    state: &AppState,
    account_id: &str,
    own_user_id: &str,
    homeserver_url: &str,
    access_token: &str,
    room_id: &str,
    syncing: bool,
) {
    let base = homeserver_url.trim_end_matches('/');
    let already_known = state.runtime.get_matrix_room_name(account_id, room_id).is_some();
    let encoded_room_id = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let state_resp = match http::get_json(&format!("{base}/_matrix/client/v3/rooms/{encoded_room_id}/state"), access_token).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("matrix[{account_id}]: fetching state for room {room_id} failed: {e:#}");
            return;
        }
    };
    let Some(events) = state_resp.as_array() else { return };
    let event_refs: Vec<&Value> = events.iter().collect();

    // A Space is a room too, so it arrives in joined_rooms alongside real
    // ones. It gets a rail entry rather than a buffer - there is nothing
    // to say in it.
    if rooms::is_space(&event_refs) {
        register_space(state, account_id, room_id, homeserver_url, access_token, &event_refs).await;
        return;
    }

    let info = rooms::derive_room_info(room_id, own_user_id, &event_refs);
    state.runtime.set_matrix_room_name(account_id, room_id, &info.name, &info.kind);

    let buffer = state.runtime.ensure_buffer(state, account_id, &info.name, &info.kind);
    state.runtime.set_matrix_room(state, &buffer.id, room_id);
    // Only for a room this is the first sight of. A sync that has already
    // carried the room has already said what is in it, and marking it as
    // waiting afterwards would leave a spinner turning against a room that
    // arrived while we were asking - until the next thing anybody said in it.
    if syncing && !already_known {
        state.runtime.set_buffer_syncing(state, &buffer.id, true);
        stop_waiting_eventually(state.clone(), buffer.id.clone());
    }
    // The space that lists this room may already have been seen, or may
    // turn up later - register_space back-fills the other order.
    if let Some(group_id) = state.runtime.get_matrix_space_parent(account_id, room_id) {
        state.runtime.set_buffer_group(state, &buffer.id, &group_id);
    }

    state.runtime.set_matrix_room_encrypted(state, &buffer.id, rooms::is_encrypted(&event_refs));

    roomstate::process_state_events(state, account_id, room_id, homeserver_url, access_token, &event_refs).await;
    // No presence data available at bootstrap time (that's /sync-only
    // - there's no bulk "current presence for all these users"
    // endpoint) - the roster/power-level part of the userlist is
    // correct immediately, everyone just starts as offline until a
    // real presence.events update arrives post-connect.
    roomstate::emit_matrix_presence(state, account_id, room_id, own_user_id);
}

/// How long a freshly joined room is allowed to be "still arriving" before we
/// stop saying so.
const SYNC_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Gives up waiting, after a while.
///
/// Normally the next sync carries the room and clears this - joining produces
/// a membership event, so there is always something to carry. But a room that
/// was joined from somewhere else in the same breath, or one whose join event
/// arrived in a sync processed before the buffer existed, has nothing left to
/// announce it: the room is fine and the spinner would turn forever.
///
/// A room silent for half a minute is not still arriving, it is quiet - and
/// "quiet" and "still loading" look identical to somebody waiting, so the
/// wrong one of the two must not be the one that lasts.
fn stop_waiting_eventually(state: AppState, buffer_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(SYNC_WAIT_LIMIT).await;
        state.runtime.set_buffer_syncing(&state, &buffer_id, false);
    });
}

async fn process_sync_response(state: &AppState, account_id: &str, own_user_id: &str, homeserver_url: &str, access_token: &str, resp: &Value, session: &crypto::CryptoSession) {
    // Global presence updates, handled before the per-room loop below since
    // presence.events is a top-level sibling of rooms, not scoped to any
    // one of them - a user going online/offline with no other room
    // activity this cycle wouldn't otherwise show up in rooms.join at all,
    // so this can't just be folded into the per-room pass.
    // The account's own settings, which arrive at the top level rather than in
    // any room. Only the ignore list is read: it changes from other clients,
    // and a block that only applies where it was made is not a block.
    if let Some(events) = resp["account_data"]["events"].as_array() {
        for event in events {
            if event["type"].as_str() == Some("m.ignored_user_list") {
                apply_ignored_users(state, account_id, &event["content"]);
            }
        }
    }

    if let Some(events) = resp["presence"]["events"].as_array() {
        for event in events {
            let Some(sender) = event["sender"].as_str() else { continue };
            let online = event["content"]["presence"].as_str() == Some("online");
            state.runtime.set_matrix_presence(account_id, sender, online);
            for room_id in state.runtime.matrix_rooms_containing_member(account_id, sender) {
                roomstate::emit_matrix_presence(state, account_id, &room_id, own_user_id);
            }
        }
    }

    // Invitations, which are not rooms you are in. Read before the joined
    // pass because that one returns early when there is nothing joined -
    // and an account whose only news this cycle is an invite has exactly
    // that shape.
    //
    // Until this existed, being invited to a room was something the client
    // could not perceive at all: rooms.invite was never read, so the invite
    // never appeared, and there was no way to accept or decline one.
    let invites: Vec<serde_json::Value> = resp["rooms"]["invite"]
        .as_object()
        .map(|rooms| {
            rooms
                .iter()
                .map(|(room_id, room)| {
                    let events: Vec<&Value> = room["invite_state"]["events"].as_array().into_iter().flatten().collect();
                    rooms::invite_summary(room_id, own_user_id, &events)
                })
                .collect()
        })
        .unwrap_or_default();
    // An invite is taken down by being answered, not by a sync failing to
    // mention it: the rooms this response reports as joined or left are the
    // ones that have been, here or in another client.
    let resolved: std::collections::HashSet<String> = ["join", "leave"]
        .iter()
        .flat_map(|kind| resp["rooms"][kind].as_object().into_iter().flatten().map(|(room_id, _)| room_id.clone()))
        .collect();
    if state.runtime.merge_matrix_invites(account_id, invites, &resolved) {
        let invites = state.runtime.matrix_invites(account_id);
        state.events.emit("matrixInvites", serde_json::json!({ "accountId": account_id, "invites": invites }));
    }

    // Rooms this account is no longer in - left from another client, or
    // kicked. Their buffers would otherwise sit there looking joined.
    if let Some(left) = resp["rooms"]["leave"].as_object() {
        for room_id in left.keys() {
            if let Some((buffer_name, _)) = state.runtime.get_matrix_room_name(account_id, room_id) {
                let buffer_id = crate::model::buffer_id(account_id, &buffer_name);
                state.runtime.remove_buffer(state, &buffer_id);
            }
        }
    }

    let Some(joined) = resp["rooms"]["join"].as_object() else { return };
    for (room_id, room) in joined {
        // The anchor for reading older history. Only the first one seen is
        // kept: every later sync's prev_batch points at newer history, and
        // taking one would skip everything between.
        if let Some(token) = room["timeline"]["prev_batch"].as_str() {
            state.runtime.set_matrix_back_token(account_id, room_id, token, true);
        }

        // Who is composing in this room, if anybody. Matrix sends the whole
        // set each time rather than one event per person, and an empty list
        // is how it says everybody stopped - so this is a replace, and the
        // empty case has to be emitted rather than skipped or the last
        // indicator would never clear.
        if let Some(events) = room["ephemeral"]["events"].as_array() {
            for event in events.iter().filter(|e| e["type"].as_str() == Some("m.receipt")) {
                take_read_receipts(state, account_id, room_id, own_user_id, &event["content"]);
            }
            for event in events.iter().filter(|e| e["type"].as_str() == Some("m.typing")) {
                let members = state.runtime.get_matrix_room_members(account_id, room_id);
                let nicks: Vec<String> = event["content"]["user_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|u| u.as_str())
                    .filter(|u| *u != own_user_id)
                    .map(|u| members.get(u).cloned().unwrap_or_else(|| u.to_string()))
                    .collect();
                if let Some((buffer_name, _)) = state.runtime.get_matrix_room_name(account_id, room_id) {
                    state.events.emit(
                        "typing",
                        serde_json::json!({
                            "accountId": account_id,
                            "bufferId": crate::model::buffer_id(account_id, &buffer_name),
                            "nicks": nicks,
                            "expiresInMs": crate::backend::discord::TYPING_TTL_MS,
                        }),
                    );
                }
            }
        }

        // The room has been heard from, so it is no longer waiting to be. Set
        // on join and cleared here, which is the first moment there is
        // anything true to say about what is in it.
        if let Some(buffer_id) = state.runtime.matrix_buffer_for_room(account_id, room_id) {
            state.runtime.set_buffer_syncing(state, &buffer_id, false);
        }

        let timeline_events: Vec<&Value> = room["timeline"]["events"].as_array().into_iter().flatten().collect();

        let (buffer_name, buffer_kind) = match state.runtime.get_matrix_room_name(account_id, room_id) {
            Some(cached) => cached,
            None => {
                // First time seeing this room - either the account's true
                // first /sync (a full state snapshot) or a room newly
                // joined mid-session (Matrix sends that room's full state
                // the first time it appears too) - either way this
                // response's state section for THIS room is a real
                // snapshot, not a partial delta, so deriving from it once
                // here is reliable. See rooms.rs's doc comment for why
                // this is deliberately *not* re-derived on later syncs.
                let mut naming_events: Vec<&Value> = room["state"]["events"].as_array().into_iter().flatten().collect();
                naming_events.extend(timeline_events.iter().filter(|e| e["state_key"].is_string()));

                // A space joined mid-session arrives here exactly like a room
                // would. It becomes a rail entry, not a buffer - without this
                // it would appear in the room list as somewhere to talk.
                // Bootstrap and this loop run concurrently, and a first
                // sync does not always carry m.room.create - so a space can
                // reach here looking like an ordinary room. Whichever side
                // identified it first is the answer.
                if rooms::is_space(&naming_events)
                    || state.runtime.has_buffer_group(&space_group_id(account_id, room_id))
                {
                    register_space(state, account_id, room_id, homeserver_url, access_token, &naming_events).await;
                    continue;
                }

                let info = rooms::derive_room_info(room_id, own_user_id, &naming_events);
                state.runtime.set_matrix_room_name(account_id, room_id, &info.name, &info.kind);
                // Same full-state snapshot naming just derived from - also
                // seeds member avatars/room avatar/power levels the first
                // time this room is seen, same reasoning as naming above
                // (see roomstate.rs's own doc comment).
                roomstate::process_state_events(state, account_id, room_id, homeserver_url, access_token, &naming_events).await;
                roomstate::emit_matrix_presence(state, account_id, room_id, own_user_id);
                (info.name, info.kind)
            }
        };

        let buffer = state.runtime.ensure_buffer(state, account_id, &buffer_name, &buffer_kind);
        state.runtime.set_matrix_room(state, &buffer.id, room_id);

        // A room can be added to a space at any time, and a space seen after
        // this room was first synced only records the mapping - so this is
        // checked every sync rather than only when the buffer is created.
        if let Some(group_id) = state.runtime.get_matrix_space_parent(account_id, room_id) {
            state.runtime.set_buffer_group(state, &buffer.id, &group_id);
        }

        // A room avatar discovered above (in the first-seen branch, before
        // this buffer existed) never actually reached it - set_matrix_room_
        // avatar's own buffer lookup silently no-ops when there's no
        // buffer yet to update. Harmless to re-apply unconditionally here
        // (a no-op if it was already applied, e.g. via bootstrap_joined_
        // rooms, which resolves the avatar *after* the buffer exists).
        if let Some(avatar) = state.runtime.get_matrix_room_avatar(account_id, room_id) {
            state.runtime.set_matrix_room_avatar(state, account_id, room_id, &avatar);
        }

        // Checked every sync (not just first-seen, unlike naming above) -
        // a room can turn encryption on mid-session, and it's sticky once
        // set (the spec forbids un-encrypting), so this only ever needs
        // to move from unset to set, never back.
        if !state.runtime.is_matrix_room_encrypted(&buffer.id) {
            let mut state_events: Vec<&Value> = room["state"]["events"].as_array().into_iter().flatten().collect();
            state_events.extend(timeline_events.iter().copied());
            state.runtime.set_matrix_room_encrypted(state, &buffer.id, rooms::is_encrypted(&state_events));
        }

        // Unlike the full-state seeding above, this runs every sync
        // regardless of whether the room was already known - a member's
        // avatar, the room's own avatar, or its power levels can all
        // change mid-session, each showing up as an ordinary
        // state-carrying timeline event rather than a fresh full state
        // snapshot.
        let mid_session_state_events: Vec<&Value> = timeline_events.iter().filter(|e| e["state_key"].is_string()).copied().collect();
        if !mid_session_state_events.is_empty() {
            roomstate::process_state_events(state, account_id, room_id, homeserver_url, access_token, &mid_session_state_events).await;
            // A membership join/leave or power-level change mid-session
            // both change this room's userlist - re-broadcast unconditionally
            // here rather than trying to detect which specific event types
            // were in this batch (cheap either way, since it's just a
            // rebuild from already-cached data, not a fresh request).
            roomstate::emit_matrix_presence(state, account_id, room_id, own_user_id);
            // Only ever called against this genuinely-live batch, never
            // the full-state dumps first-seen-room/bootstrap use above -
            // see announce_membership_changes's own doc comment for why.
            roomstate::announce_membership_changes(state, account_id, &buffer_name, &buffer_kind, &mid_session_state_events);
        }

        for event in &timeline_events {
            handle_timeline_event(state, account_id, &buffer.id, &buffer_name, &buffer_kind, event, own_user_id, room_id, session, homeserver_url, access_token).await;
        }
    }

    // Rooms that were replaced while this response was being read. Followed
    // here rather than where the tombstone was seen, because joining one
    // registers it, which processes its state, which is where they are
    // noticed - see Runtime::note_matrix_upgrade.
    for successor in state.runtime.take_matrix_upgrades(account_id) {
        if let Err(e) = join_room(state, account_id, &successor, &[]).await {
            tracing::warn!("matrix[{account_id}]: following a room upgrade into {successor}: {e:#}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// What a message is answering, and whether that is a thread.
///
/// A threaded message names its thread rather than the message before it:
/// Matrix sends both, and the second is a fallback for clients that cannot
/// read threads, so following it gives a chain of one-line replies where a
/// conversation was.
///
/// The preview is filled in from local scrollback where the target is there.
/// Where it is not, the relation is still recorded without one - knowing a
/// message belongs to a thread matters even when the thread's own first
/// message has not been read yet.
fn relation_preview(state: &AppState, buffer_id: &str, content: &Value) -> Option<crate::model::ReplyPreview> {
    let thread = protocol::thread_root(content);
    let in_thread = thread.is_some();
    let target = thread.or_else(|| protocol::reply_target(content))?;
    match state.store.get_message(buffer_id, target) {
        Ok(Some(m)) => Some(crate::model::ReplyPreview { id: target.to_string(), from: m.from, body: m.body, thread: in_thread }),
        _ => Some(crate::model::ReplyPreview { id: target.to_string(), thread: in_thread, ..Default::default() }),
    }
}

async fn handle_timeline_event(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    buffer_name: &str,
    buffer_kind: &str,
    event: &Value,
    own_user_id: &str,
    room_id: &str,
    session: &crypto::CryptoSession,
    homeserver_url: &str,
    access_token: &str,
) {
    let outer_type = protocol::event_type(event);

    // Redactions are always sent in cleartext, never wrapped in
    // m.room.encrypted (per spec) - handled up front, independent of the
    // decrypt path below.
    if outer_type == protocol::EVENT_REDACTION {
        if let Some(target) = protocol::redaction_target(event) {
            handle_redaction(state, buffer_id, target);
        }
        return;
    }

    if outer_type != protocol::EVENT_ROOM_MESSAGE
        && outer_type != protocol::EVENT_ROOM_ENCRYPTED
        && outer_type != protocol::EVENT_REACTION
        && outer_type != protocol::EVENT_STICKER
        // A poll is three kinds of event and none of them is a message.
        && !polls::is_poll_event(outer_type)
        // And a verification with another person travels through the room.
        && !outer_type.starts_with("m.key.verification.")
    {
        // Membership/name changes were already folded into naming in
        // process_sync_response; anything else (typing, receipts, other
        // state events) isn't rendered.
        return;
    }

    let sender = protocol::sender(event);
    // Somebody this account has asked never to hear from. Synapse filters
    // them out of sync before they get here; not every homeserver does, and a
    // block honoured only by some servers is not one worth having.
    if sender != own_user_id && state.runtime.matrix_is_ignored(account_id, sender) {
        return;
    }
    let from = if sender == own_user_id {
        state.runtime.own_identity(account_id).unwrap_or_else(|| protocol::short_sender(event))
    } else {
        protocol::short_sender(event)
    };
    // The *outer* envelope's event id is what edits/reactions/redactions
    // reference, even for an event that turns out to be encrypted -
    // never the decrypted inner event's own id (Megolm-encrypted content
    // has none of its own; the outer id is authoritative either way).
    let Some(event_id) = protocol::event_id(event) else { return };

    // For an encrypted event, decryption reveals the *real* type (a
    // reaction can be encrypted too, not just messages) - so the
    // effective type/content used below come from the decrypted envelope
    // when there is one, the outer event otherwise.
    // Whether the sender's device has been verified, which is only a question
    // an encrypted message can answer: in a plain room there is no device
    // claim to judge, so there is nothing to say rather than something bad.
    let mut sender_verified: Option<bool> = None;
    let (effective_type, mut content, undecryptable): (String, Value, bool) = if outer_type == protocol::EVENT_ROOM_ENCRYPTED {
        match decrypt_event(session, event, room_id).await {
            Ok((decrypted, verified)) => {
                sender_verified = Some(verified);
                let t = decrypted["type"].as_str().unwrap_or(protocol::EVENT_ROOM_MESSAGE).to_string();
                (t, decrypted["content"].clone(), false)
            }
            Err(e) => {
                // A real, common race: the Megolm session for this
                // message hasn't arrived yet (key-share to-device events
                // can lag the timeline event referencing them). v1 shows
                // an honest placeholder rather than retry-on-later-key-
                // arrival (request_room_key) - see the plan's Open
                // decisions for why that's deferred to a later pass.
                tracing::debug!("matrix[{account_id}]: failed to decrypt event {event_id} in {room_id}: {e:#}");
                (protocol::EVENT_ROOM_MESSAGE.to_string(), Value::Null, true)
            }
        }
    } else {
        (outer_type.to_string(), event["content"].clone(), false)
    };

    if effective_type == protocol::EVENT_REACTION {
        let Some((target_event, emoji)) = protocol::reaction_target(&content) else { return };
        let is_me = sender == own_user_id;
        state.runtime.record_matrix_reaction_event(buffer_id, target_event, emoji, event_id, is_me);
        state.runtime.update_reaction(state, buffer_id, target_event, emoji, is_me, true);
        return;
    }

    // Verifying another person: the whole flow travels through the room the
    // two of you share, because until you have agreed which devices you are
    // talking about there is no device to address it to. None of it is chat,
    // so it goes to the machine and stops here.
    if effective_type.starts_with("m.key.verification.")
        || content["msgtype"].as_str() == Some("m.key.verification.request")
    {
        if sender != own_user_id {
            state.runtime.note_matrix_verification_peer(account_id, sender);
        }
        let mut event = event.clone();
        // The decrypted content where there was any, so an encrypted room's
        // verification reads the same as a plain one's.
        if outer_type == protocol::EVENT_ROOM_ENCRYPTED && !undecryptable {
            if let Some(object) = event.as_object_mut() {
                object.insert("type".to_string(), Value::from(effective_type.clone()));
                object.insert("content".to_string(), content.clone());
            }
        }
        if let Err(e) = session.receive_room_verification(&event, room_id).await {
            tracing::debug!("matrix[{account_id}]: verification event {event_id}: {e:#}");
        }
        return;
    }

    // A poll: the question, a vote, or the end of the counting. Handled after
    // decryption like everything else, because a poll in an encrypted room
    // arrives as ciphertext the same way a message does.
    if polls::is_poll_event(&effective_type)
        && polls::handle(state, account_id, buffer_id, own_user_id, &effective_type, event_id, sender, &content)
    {
        return;
    }

    // A sticker is an image that arrived under its own event type rather
    // than as a message with a msgtype. Given the msgtype it is missing, the
    // whole media path below - fetch, cache, describe - reads it as what it
    // is instead of dropping it for having the wrong envelope.
    if effective_type == protocol::EVENT_STICKER {
        if let Some(object) = content.as_object_mut() {
            object.entry("msgtype").or_insert_with(|| Value::from("m.image"));
        }
    } else if effective_type != protocol::EVENT_ROOM_MESSAGE {
        return;
    }

    // An edit: apply to the *original* message rather than recording a
    // new one - the outer event_id here is the edit event's own id, not
    // useful as a message id (edits aren't shown as separate messages,
    // matching every other backend's own edit handling).
    if let Some(target_event) = protocol::edit_target(&content) {
        if undecryptable {
            return;
        }
        let (new_body, _) = protocol::message_body(protocol::edit_new_content(&content));
        if !new_body.is_empty() {
            state.runtime.update_message(state, buffer_id, target_event, &new_body, &[], &[]);
        }
        return;
    }

    // Media travels as a described attachment, not as text: Matrix already
    // separates the two (`body` is the filename, the bytes are behind an mxc
    // URI plus an `info` block), and every Matrix client from Element down
    // keeps them separate. Fetching is still ours to do - a frontend has no
    // access token, and for an encrypted room the server only holds
    // ciphertext - so the bytes land in the local cache and the attachment
    // reports that path, the same shape matrix-rust-sdk hands a client.
    let mut attachments: Vec<Attachment> = Vec::new();
    let (mut body, is_action) = if undecryptable {
        ("[unable to decrypt message]".to_string(), false)
    } else if let Some(mxc) = protocol::media_mxc_uri(&content) {
        let ext = extension_for_mimetype(content["info"]["mimetype"].as_str().unwrap_or(""));
        let path = cached_media_path(homeserver_url, access_token, mxc, ext).await;
        attachments.push(build_attachment(&content, path, thumbnail_for(&content, homeserver_url, access_token).await));
        (protocol::message_body(&content).0, false)
    } else if let Some(file) = protocol::encrypted_media_file(&content) {
        let ext = extension_for_mimetype(content["info"]["mimetype"].as_str().unwrap_or(""));
        let path = cached_encrypted_media_path(homeserver_url, access_token, file, ext).await;
        attachments.push(build_attachment(&content, path, thumbnail_for(&content, homeserver_url, access_token).await));
        (protocol::message_body(&content).0, false)
    } else if let Some(place) = protocol::location_of(&content) {
        // A place, as the one thing a chat client can honestly do with one:
        // what the sender called it, and a link to a map that can show it.
        // Drawing the map here would mean shipping tiles from somebody's
        // server on every message that mentions a street corner.
        (place, false)
    } else {
        protocol::message_body(&content)
    };
    // An attachment with no caption legitimately has an empty body; dropping
    // the message then would lose the media entirely.
    if body.is_empty() && attachments.is_empty() {
        return;
    }

    // What this message is answering. A threaded message names its thread
    // rather than the message before it: Matrix sends both, and the second is
    // a fallback for clients that cannot read threads, so following it gives a
    // chain of one-line replies where a conversation was.
    //
    // Threads are not their own buffers here, and this does not make them one.
    // It is the difference between a threaded message arriving with no context
    // at all and one that says which conversation it belongs to.
    let reply_to = relation_preview(state, buffer_id, &content);

    // Whether this was aimed at us, as the sender said rather than as this
    // client guesses. Matrix used to leave every client to scan every message
    // for its own name, which is why the same mention could be seen by one
    // client and missed by another.
    let mentioned = content["m.mentions"]["user_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|id| id.as_str() == Some(own_user_id))
        || content["m.mentions"]["room"].as_bool() == Some(true);

    // What this account told its server it wants to hear about. A keyword set
    // on another client notifies here; a room muted there stays quiet here,
    // including for the nick match this client makes on its own - which is
    // why the mute is put on the buffer rather than folded into this one
    // message's answer.
    let (muted, keyword) = match state.runtime.matrix_push_rules(account_id) {
        Some(rules) => push_rule_verdict(&rules, room_id, &body),
        None => (false, false),
    };
    state.runtime.set_silenced(buffer_id, muted);
    let mentioned = mentioned || keyword;

    if reply_to.is_some() {
        // The raw body Matrix sends for a reply includes a quoted `> `
        // fallback block ahead of the real text, for clients that don't
        // understand m.relates_to - this project already renders its own
        // reply preview (reply_to above), so strip Matrix's redundant
        // quoted-fallback lines rather than showing the quote twice.
        if let Some(after) = body.rsplit("\n\n").last() {
            if body.contains("\n\n") && body.lines().next().is_some_and(|l| l.starts_with('>')) {
                body = after.to_string();
            }
        }
    }

    // A voice message is an audio file with a note on it saying it was
    // spoken rather than sent. Element shows it as a waveform; this at least
    // stops it reading as somebody attaching "Voice message.ogg".
    if protocol::is_voice_message(&content) {
        body = "Voice message".to_string();
    }

    let sender_avatar_url = state.runtime.get_matrix_member_avatar(account_id, sender);

    // The time the server recorded, not the time this arrived. They are the
    // same thing for a message read as it is said and very different for one
    // that arrives in the burst after a reconnect, which would otherwise all
    // land at the reconnect. It also keeps live messages and backfilled ones
    // on the same clock, so scrollback does not step sideways at the join.
    let sent_at = event["origin_server_ts"].as_i64().map(|ms| ms / 1000);

    state.runtime.record_message_at(
        state,
        account_id,
        buffer_name,
        buffer_kind,
        &from,
        &body,
        is_action,
        "message",
        reply_to,
        Some(event_id.to_string()),
        mentioned,
        sender_avatar_url,
        Vec::new(),
        attachments,
        Some(sender.to_string()),
        sent_at,
        // The sender's own formatting, where they sent any. Passed on rather
        // than rendered here: it is somebody else's markup and the frontend
        // is where the sanitiser lives.
        protocol::formatted_body(&content).map(str::to_string),
        // A mark beside the name where the message came from a device this
        // account has never verified. Only where there was something to
        // check - an encrypted message from somebody else - and only when the
        // answer is no: Element marks the doubtful ones rather than ticking
        // every ordinary message, and a badge on all of them would say
        // nothing while taking room from the ones that matter.
        match sender_verified {
            Some(false) if sender != own_user_id => Some(model::SenderStyle {
                color: None,
                badges: vec![crate::backend::kick::api::Badge {
                    kind: "unverified".to_string(),
                    text: "Sent from a device you have not verified".to_string(),
                    count: None,
                }],
            }),
            _ => None,
        },
    );
}

/// Applies an incoming `m.room.redaction`: either a real message delete
/// (target is a message we have) or a reaction removal (target is a
/// reaction event we've seen - see Runtime::take_matrix_reaction_target).
/// Tries the reaction path first since it's a cheap map lookup+remove;
/// falls back to treating it as a message delete otherwise, matching the
/// same "try, then fall back" precedent backend/sneedchat/mod.rs uses for
/// its own ambiguous edit-vs-insert wire signal.
fn handle_redaction(state: &AppState, buffer_id: &str, target_event: &str) {
    if let Some((target_buffer, msg_id, emoji, is_me)) = state.runtime.take_matrix_reaction_target(target_event) {
        state.runtime.update_reaction(state, &target_buffer, &msg_id, &emoji, is_me, false);
        return;
    }
    state.runtime.delete_message(state, buffer_id, target_event);
}

/// The plaintext, and whether the device that sent it has been verified.
async fn decrypt_event(session: &crypto::CryptoSession, event: &Value, room_id: &str) -> Result<(Value, bool)> {
    let room_id = ruma_common::RoomId::parse(room_id).context("invalid room id")?;
    crypto::decrypt_room_event_with_trust(session, event, &room_id).await
}

/// Turns a media event's `content` into an Attachment, carrying across the
/// `info` block Matrix already provides - mimetype, size, intrinsic
/// dimensions and blurhash - rather than reducing all of it to a file path.
/// The dimensions in particular let a frontend reserve layout space before
/// any bytes arrive, which is why Element's timeline doesn't reflow as
/// images load.
fn build_attachment(content: &Value, path: Option<String>, thumbnail_path: Option<String>) -> Attachment {
    let info = &content["info"];
    let mimetype = info["mimetype"].as_str().map(str::to_string);
    Attachment {
        kind: match content["msgtype"].as_str().unwrap_or("") {
            "m.image" => "image",
            "m.video" => "video",
            "m.audio" => "audio",
            _ => "file",
        }
        .to_string(),
        filename: content["body"].as_str().map(str::to_string),
        size: info["size"].as_u64(),
        width: info["w"].as_u64().map(|v| v as u32),
        height: info["h"].as_u64().map(|v| v as u32),
        // MSC2448, still under its unstable prefix on most servers.
        blurhash: info["xyz.amorgan.blurhash"].as_str().or_else(|| info["blurhash"].as_str()).map(str::to_string),
        mimetype,
        path,
        thumbnail_path,
        url: None,
    }
}

/// Fetches the server-generated thumbnail a media event points at, when it
/// has one. Preferring this for previews is what keeps a timeline from
/// pulling full-size originals off the homeserver just to draw something
/// small - the same reason Element requests the thumbnail endpoint.
async fn thumbnail_for(content: &Value, homeserver_url: &str, access_token: &str) -> Option<String> {
    let info = &content["info"];
    // Encrypted rooms carry the thumbnail as its own EncryptedFile rather
    // than a plain mxc URI, and it needs the same decrypt path as the body.
    if let Some(file) = info.get("thumbnail_file").filter(|v| v.is_object()) {
        return cached_encrypted_media_path(homeserver_url, access_token, file, "").await;
    }
    let mxc = info["thumbnail_url"].as_str()?;
    cached_media_path(homeserver_url, access_token, mxc, "").await
}

/// Matrix's own mxc:// media ids carry no file extension - unlike
/// backend/sneedchat/mod.rs's cached attachments, which keep whatever
/// extension the original filename already had. The cached file still gets a
/// real extension so that anything reading it off disk (an image viewer the
/// user opens it in, a frontend sniffing by name) sees the right type - but
/// this is now a detail of how the cache is named, not something the wire
/// contract depends on: the mimetype travels on the attachment itself.
/// Historically this existed because the client classified media by URL
/// pattern - the
/// same class of bug already fixed once for Discord/klipy's own
/// extensionless CDN URLs (see opaqueImageHosts there), just hit again
/// here from a different direction (no extension at all, rather than an
/// extension-less but known host). Only the handful of types actually
/// worth embedding inline are mapped; anything else falls back to no
/// extension, same as before this existed - it just won't auto-embed,
/// same as it wouldn't have anyway.
fn extension_for_mimetype(mimetype: &str) -> &'static str {
    match mimetype {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "video/x-matroska" => "mkv",
        "video/ogg" => "ogv",
        _ => "",
    }
}

/// Resolves an `mxc://` URI to a local `file://` path, downloading and
/// caching it first if needed - a plain QML `Image`/media element has no
/// route to send the `Authorization: Bearer` header Matrix's media
/// endpoint requires, so the raw remote URL would simply never load (same
/// reasoning backend/sneedchat/mod.rs's own avatar caching already
/// documents). Cached permanently per media id for this daemon's
/// lifetime - see MEDIA_CACHE_MAX_BYTES/sweep_media_cache for the size
/// cap that keeps that bounded. `extension` (from extension_for_mimetype,
/// empty string if unknown) is appended to the cache filename - see that
/// function's own doc comment on why this can't just be left off.
pub(crate) async fn cached_media_path(homeserver_url: &str, access_token: &str, mxc_uri: &str, extension: &str) -> Option<String> {
    let rest = mxc_uri.strip_prefix("mxc://")?;
    let (server_name, media_id) = rest.split_once('/')?;
    let dir = media_cache_dir();
    let filename = if extension.is_empty() { format!("{server_name}_{media_id}") } else { format!("{server_name}_{media_id}.{extension}") };
    let path = dir.join(filename);

    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Some(format!("file://{}", path.display()));
    }

    let url = format!(
        "{}/_matrix/client/v1/media/download/{}/{}",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(server_name.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(media_id.as_bytes()).collect::<String>(),
    );
    let fetch = tokio::time::timeout(std::time::Duration::from_secs(20), http::get_bytes(&url, access_token)).await;
    let (status, bytes) = match fetch {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::debug!("matrix: media fetch for {mxc_uri} failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::debug!("matrix: media fetch for {mxc_uri} timed out");
            return None;
        }
    };
    if !(200..300).contains(&status) || bytes.is_empty() {
        tracing::debug!("matrix: media fetch for {mxc_uri} returned HTTP {status}");
        return None;
    }
    if tokio::fs::create_dir_all(&dir).await.is_err() || tokio::fs::write(&path, &bytes).await.is_err() {
        return None;
    }
    Some(format!("file://{}", path.display()))
}

/// Same as cached_media_path, but for E2EE media: `file` is the message's
/// `content.file` object (mxc URI plus the per-file AES key/iv/hash - see
/// protocol::encrypted_media_file) - the downloaded ciphertext is
/// AES-256-CTR decrypted (matrix-sdk-crypto's AttachmentDecryptor, the
/// same primitive send_message's upload_encrypted_media_message uses in
/// reverse) before being written to the same on-disk cache, so the cached
/// file is real plaintext by the time a plain QML Image/MediaPlayer opens
/// it - same reasoning as the unencrypted path needing to route around a
/// plain Image having no way to send a Bearer header, plus here also no
/// way to AES-decrypt on the fly.
pub(crate) async fn cached_encrypted_media_path(homeserver_url: &str, access_token: &str, file: &Value, extension: &str) -> Option<String> {
    use matrix_sdk_crypto::{AttachmentDecryptor, MediaEncryptionInfo};
    use std::io::{Cursor, Read};

    let mxc_uri = file["url"].as_str()?;
    let rest = mxc_uri.strip_prefix("mxc://")?;
    let (server_name, media_id) = rest.split_once('/')?;
    let dir = media_cache_dir();
    let filename = if extension.is_empty() { format!("{server_name}_{media_id}") } else { format!("{server_name}_{media_id}.{extension}") };
    let path = dir.join(filename);

    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Some(format!("file://{}", path.display()));
    }

    // Strip "url" before deserializing - MediaEncryptionInfo's shape is
    // exactly this object minus the mxc URI (see upload_encrypted_media_
    // message, which builds it the same way in reverse), and its custom
    // Deserialize impl expects no extra fields.
    let mut encryption_info = file.clone();
    if let Some(obj) = encryption_info.as_object_mut() {
        obj.remove("url");
    }
    let encryption_info: MediaEncryptionInfo = match serde_json::from_value(encryption_info) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("matrix: invalid encrypted media info for {mxc_uri}: {e}");
            return None;
        }
    };

    let download_url = format!(
        "{}/_matrix/client/v1/media/download/{}/{}",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(server_name.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(media_id.as_bytes()).collect::<String>(),
    );
    let fetch = tokio::time::timeout(std::time::Duration::from_secs(20), http::get_bytes(&download_url, access_token)).await;
    let (status, ciphertext) = match fetch {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::debug!("matrix: encrypted media fetch for {mxc_uri} failed: {e}");
            return None;
        }
        Err(_) => {
            tracing::debug!("matrix: encrypted media fetch for {mxc_uri} timed out");
            return None;
        }
    };
    if !(200..300).contains(&status) || ciphertext.is_empty() {
        tracing::debug!("matrix: encrypted media fetch for {mxc_uri} returned HTTP {status}");
        return None;
    }

    let mut cursor = Cursor::new(ciphertext);
    let mut decryptor = match AttachmentDecryptor::new(&mut cursor, encryption_info) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!("matrix: building decryptor for {mxc_uri} failed: {e}");
            return None;
        }
    };
    let mut plaintext = Vec::new();
    if let Err(e) = decryptor.read_to_end(&mut plaintext) {
        tracing::debug!("matrix: decrypting media {mxc_uri} failed: {e}");
        return None;
    }

    if tokio::fs::create_dir_all(&dir).await.is_err() || tokio::fs::write(&path, &plaintext).await.is_err() {
        return None;
    }
    Some(format!("file://{}", path.display()))
}

/// A genuine cache (every file here is re-fetchable from the homeserver
/// given its message/state event, and encrypted media additionally needs
/// the room's Megolm session either way) - belongs under XDG_CACHE_HOME,
/// not alongside accounts.toml/the crypto store/scrollback in
/// config_dir()'s `~/.config/nobilis` (see main.rs's own
/// migrate_caches_to_xdg_cache_dir, which moves any pre-existing directory
/// here on first startup after this changed).
fn media_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache")).join("nobilis").join("matrix-media")
}

/// Cap on the media cache's total size on disk - same unbounded-growth
/// concern (and same fix) as backend/sneedchat/mod.rs's avatar/attachment
/// caches: every distinct piece of media ever seen would otherwise
/// accumulate its own permanently-cached file forever.
pub const MEDIA_CACHE_MAX_BYTES: u64 = 250 * 1024 * 1024;
/// Evicts the oldest-written files in the media cache until it's back
/// under MEDIA_CACHE_MAX_BYTES - oldest-by-mtime, same simplification
/// backend/sneedchat/mod.rs's own sweep_cache_dir documents (not true LRU,
/// but a reasonable approximation without an extra dependency).
pub async fn sweep_media_cache() {
    let dir = media_cache_dir();
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else { return };

    let mut files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        total += meta.len();
        files.push((entry.path(), meta.len(), mtime));
    }
    if total <= MEDIA_CACHE_MAX_BYTES {
        return;
    }

    files.sort_by_key(|(_, _, mtime)| *mtime);
    let mut to_free = total - MEDIA_CACHE_MAX_BYTES;
    let mut removed = 0usize;
    for (path, size, _) in files {
        if to_free == 0 {
            break;
        }
        if tokio::fs::remove_file(&path).await.is_ok() {
            to_free = to_free.saturating_sub(size);
            removed += 1;
        }
    }
    tracing::info!("matrix: media cache was over its {}MB cap, evicted {removed} oldest file(s)", MEDIA_CACHE_MAX_BYTES / 1024 / 1024);
}

/// Joins an existing room (or space - a Space is just a room with an
/// `m.space` creation type under the hood, joined through this exact same
/// endpoint, so there's no separate "join a space" mechanism to build) by
/// id (`!opaque:server`) or alias (`#room:server`) - whichever the caller
/// gives, the API accepts both identically. The joined room shows up as a
/// buffer the normal way once the next `/sync` poll sees it in
/// `rooms.join` for the first time (process_sync_response's own first-
/// seen-room handling) - nothing extra needed here.
pub async fn join_room(state: &AppState, account_id: &str, room_id_or_alias: &str, via: &[String]) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded = url::form_urlencoded::byte_serialize(room_id_or_alias.trim().as_bytes()).collect::<String>();
    let mut url = format!("{base}/_matrix/client/v3/join/{encoded}");
    // Where to ask. A room id names a room and says nothing about who has it,
    // so joining one by id needs somebody already in it to route through -
    // which is why a room found in another server's directory could be listed
    // and not joinable. An alias carries its server in itself and needs none
    // of this.
    for (i, server) in via.iter().filter(|s| !s.trim().is_empty()).enumerate() {
        url.push(if i == 0 { '?' } else { '&' });
        url.push_str("server_name=");
        url.push_str(&url::form_urlencoded::byte_serialize(server.trim().as_bytes()).collect::<String>());
    }
    let resp = http::post_json(&url, Some(&account.access_token), serde_json::json!({})).await.context("joining room")?;

    // Give the room a buffer now rather than when the next sync happens to
    // mention it. A join is somebody waiting: on a large room the server can
    // take many seconds to say anything about it, and until it did there was
    // no sign anywhere in the client that anything had happened at all.
    //
    // Marked as still syncing, which is what the list draws a spinner against
    // and what the empty room says instead of looking like a room with nothing
    // in it. Cleared by the first sync that carries the room - see
    // process_sync_response.
    if let Some(room_id) = resp["room_id"].as_str() {
        register_room(state, account_id, &account.user_id, &account.homeserver_url, &account.access_token, room_id, true).await;
    }
    Ok(())
}

/// Searches the homeserver's own copy of the conversation.
///
/// The local scrollback is this window's copy of what it happened to be
/// present for; the server has everything the room has, including what was
/// said before this account joined and what this client has never downloaded.
/// Element searches both, and searching only one of them is why something
/// said last year in a room joined last week could not be found here.
///
/// One room or the whole account: a search worth doing is often "where did
/// somebody say that", and the room is exactly what the person has forgotten.
///
/// An encrypted room returns nothing from this, and honestly so - the server
/// holds ciphertext and cannot read it. That is not a failure to report as
/// one, and the caller is told the room is encrypted so it can say which kind
/// of nothing this is.
pub async fn search_messages(
    state: &AppState,
    account_id: &str,
    room_id: Option<&str>,
    term: &str,
    limit: u32,
) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let mut filter = serde_json::Map::new();
    filter.insert("limit".into(), Value::from(limit.clamp(1, 100)));
    if let Some(room_id) = room_id.filter(|r| !r.is_empty()) {
        filter.insert("rooms".into(), serde_json::json!([room_id]));
    }
    let body = serde_json::json!({
        "search_categories": {
            "room_events": {
                "search_term": term,
                // The message text, which is what somebody searching means.
                // Searching over topics and names as well turns "did anyone
                // mention the deploy" into a list of rooms with deploy in
                // their name.
                "keys": ["content.body"],
                "order_by": "recent",
                "filter": Value::Object(filter),
                // Names for the senders, so a result reads as a message from
                // a person rather than from an mxid.
                "event_context": { "before_limit": 0, "after_limit": 0, "include_profile": true },
            }
        }
    });

    let resp = http::post_json(&format!("{base}/_matrix/client/v3/search"), Some(&account.access_token), body)
        .await
        .context("searching the server")?;
    let events = resp["search_categories"]["room_events"]["results"].as_array().cloned().unwrap_or_default();

    let mut results = Vec::new();
    let mut room_names = serde_json::Map::new();
    for hit in events {
        let event = &hit["result"];
        let Some(event_id) = event["event_id"].as_str() else { continue };
        let Some(room) = event["room_id"].as_str() else { continue };
        let sender = event["sender"].as_str().unwrap_or_default();
        // The display name the server sent alongside the hit, where it did.
        let profile = hit["context"]["profile_info"][sender]["displayname"].as_str();
        let body = event["content"]["body"].as_str().unwrap_or_default();
        if body.is_empty() {
            continue;
        }
        let buffer_id = state.runtime.matrix_buffer_for_room(account_id, room).unwrap_or_default();
        if !buffer_id.is_empty() {
            if let Some((name, _kind)) = state.runtime.get_matrix_room_name(account_id, room) {
                room_names.insert(buffer_id.clone(), Value::from(name));
            }
        }
        results.push(serde_json::json!({
            "id": event_id,
            "bufferId": buffer_id,
            "from": profile.map(|name| name.to_string()).unwrap_or_else(|| protocol::mxid_localpart(sender)),
            "body": body,
            // Matrix counts in milliseconds and everything here counts in
            // seconds.
            "ts": event["origin_server_ts"].as_i64().unwrap_or_default() / 1000,
            "isAction": false,
            "isHighlight": false,
            "kind": "chat",
            "edited": false,
            "isOwn": sender == account.user_id,
        }));
    }

    Ok(serde_json::json!({
        "results": results,
        "roomNames": Value::Object(room_names),
        "count": resp["search_categories"]["room_events"]["count"].as_i64().unwrap_or(results.len() as i64),
    }))
}

/// Leaves a room, for real, on the server.
///
/// Closing a conversation used to remove the buffer and nothing else, so it
/// came back on the next sync and the account was still in the room as far as
/// everyone else in it was concerned - the client had hidden it rather than
/// left it. Also forgets the room afterwards: leaving alone keeps it in the
/// account's `rooms.leave` forever, which every client shows as a room you
/// have left rather than one that is gone. Forgetting is best-effort, since a
/// server may refuse it and the leave is the part that matters.
/// Reads a page of older history and writes it into local scrollback.
///
/// Before this there was no `/messages` call at all, so scrollback was only
/// ever what the daemon had watched happen live: joining a room with years
/// behind it showed nothing above the first sync.
///
/// Returns how many messages were added. Zero means the room has been read
/// back to its beginning, or as far as the server will serve.
///
/// Written straight to the store rather than through record_message: these
/// are old, and the notification path would announce every one of them.
/// Searches the public room directories, the way Element's room explorer does
/// - except across every homeserver at once rather than one at a time.
///
/// A directory is per homeserver: ours lists what our server has been told
/// about, and finding a room on a server we have never spoken to means asking
/// that server directly. Element makes you pick one from a dropdown and shows
/// its results alone, so finding something means knowing where to look first.
/// Here every server is asked together and the answers become one list, which
/// is what somebody searching for a room by name actually wants.
///
/// Which servers: ours, wherever this account already has rooms, and anything
/// the caller names. The middle one is what makes this useful without being
/// told anything - the servers somebody's rooms are on are the servers their
/// community lives on.
///
/// One server being slow, dead, or refusing federation does not fail the
/// search: each is a separate request and the answers are merged from
/// whichever came back, because a partial list is worth having and a failed
/// search is not.
pub async fn search_public_rooms(
    state: &AppState,
    account_id: &str,
    query: &str,
    servers: &[String],
    since: &serde_json::Map<String, Value>,
    limit: u32,
) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/').to_string();
    let token = account.access_token.clone();

    // Paging asks only the servers that gave us a place to continue from.
    // Asking them all would hand a server with no token its *first* page
    // again, so "show more" would append rooms already on screen - which is
    // exactly what it did.
    let targets: Vec<String> = if since.is_empty() {
        directory_targets(state, account_id, &account.user_id, &base, &token, servers).await
    } else {
        since.keys().cloned().collect()
    };
    let requests = targets.iter().map(|server| {
        let base = base.clone();
        let token = token.clone();
        let since = since.get(server).and_then(|v| v.as_str()).unwrap_or("").to_string();
        async move { (server.clone(), directory_page(&base, &token, server, query, &since, limit).await) }
    });
    let answers = futures::future::join_all(requests).await;

    let joined = state.runtime.matrix_joined_rooms(account_id);
    // Our own server has no name in a request - the spec spells "ask locally"
    // as the absence of the parameter - but it very much has one to a reader,
    // and a list of servers with a blank in it says less than nothing.
    let own_server = account.user_id.rsplit(':').next().unwrap_or("").to_string();
    let named = |server: &str| if server.is_empty() { own_server.clone() } else { server.to_string() };

    let mut rooms: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut next = serde_json::Map::new();
    let mut answered: Vec<Value> = Vec::new();

    for (server, answer) in answers {
        let resp = match answer {
            Ok(resp) => resp,
            // Why, not just that: "did not answer" covers a server that is
            // down, one that will not federate its directory, and one that
            // rate-limited us, and those are three different things to do
            // next about it.
            Err(e) => {
                answered.push(serde_json::json!({ "server": named(&server), "error": format!("{e:#}"), "rooms": 0 }));
                continue;
            }
        };
        if let Some(token) = resp["next_batch"].as_str() {
            next.insert(server.clone(), Value::String(token.to_string()));
        }
        let before = rooms.len();
        for room in resp["chunk"].as_array().into_iter().flatten() {
            let room_id = room["room_id"].as_str().unwrap_or("").to_string();
            // The same room is listed by every server that knows it. Whoever
            // answered first keeps it, which is arbitrary and does not matter:
            // the entries describe one room and differ only in how stale each
            // server's copy of its member count is.
            if room_id.is_empty() || !seen.insert(room_id.clone()) {
                continue;
            }
            rooms.push(serde_json::json!({
                "roomId": room_id,
                "name": room["name"].as_str().unwrap_or(""),
                "alias": room["canonical_alias"].as_str().unwrap_or(""),
                "topic": room["topic"].as_str().unwrap_or(""),
                "members": room["num_joined_members"].as_i64().unwrap_or(0),
                // Deliberately no icon. A directory hands back `mxc://` URIs,
                // which name media on a homeserver and are not URLs anything
                // can load - fetching one needs the access token. Passing them
                // through drew a broken image against every room that had an
                // icon and the coloured initial only against the rooms that
                // had none, which is precisely the wrong way round.
                //
                // Downloading them here was tried and measured: fifty rooms
                // spread across as many media servers did not finish inside a
                // six second budget, so a search that was fast became one that
                // was slow *and* still showed no icons. A room's initial costs
                // nothing and is what every room without an icon shows anyway.
                // Which directory answered. Shown, because in one merged list
                // the server a room lives on is the thing that says what kind
                // of place it is - and it is the routing hint a room with no
                // published alias needs to be joined at all.
                "via": named(&server),
                "joined": joined.contains(&room_id),
            }));
        }
        // What this server actually contributed, after the rooms every other
        // server had already listed were dropped. That is the honest number:
        // a server whose whole page was rooms somebody else had listed added
        // nothing to what is on screen, however many it returned.
        answered.push(serde_json::json!({ "server": named(&server), "rooms": rooms.len() - before }));
    }

    // Busiest first, across all of them. Each server returns its own list in
    // its own order, so concatenating without this would sort by which server
    // happened to answer rather than by anything about the rooms.
    rooms.sort_by(|a, b| b["members"].as_i64().unwrap_or(0).cmp(&a["members"].as_i64().unwrap_or(0)));

    // By name, which does not change. Sorting by what each contributed put
    // them in a different order after every search, so a switch somebody was
    // reaching for moved as they reached - and the count beside it is what
    // says which gave the most anyway.
    answered.sort_by(|a, b| a["server"].as_str().unwrap_or("").cmp(b["server"].as_str().unwrap_or("")));

    Ok(serde_json::json!({
        "rooms": rooms,
        "next": next,
        // One entry per homeserver asked, named, with what it contributed or
        // why it contributed nothing. A count of servers said none of this,
        // and "3 homeservers" is not something anybody can check.
        "servers": answered,
    }))
}

/// One homeserver's directory page, or an error that only costs that server.
async fn directory_page(base: &str, token: &str, server: &str, query: &str, since: &str, limit: u32) -> Result<Value> {
    let mut url = format!("{base}/_matrix/client/v3/publicRooms");
    if !server.is_empty() {
        url.push_str("?server=");
        url.push_str(&url::form_urlencoded::byte_serialize(server.as_bytes()).collect::<String>());
    }
    let mut body = serde_json::json!({ "limit": limit });
    if !query.trim().is_empty() {
        body["filter"] = serde_json::json!({ "generic_search_term": query.trim() });
    }
    if !since.is_empty() {
        body["since"] = Value::String(since.to_string());
    }
    // POST rather than GET: only the POST form takes a search term at all, and
    // the GET form's absence of one is why a directory browser without this
    // could only ever show the first page of the whole server.
    http::post_json(&url, Some(token), body).await.context("searching the room directory")
}

/// Which homeservers to ask: ours, the ones this account has rooms on, and
/// whatever the caller added - deduplicated, and with our own written as the
/// empty string because that is how the spec spells "no server parameter, ask
/// locally".
async fn directory_targets(
    state: &AppState,
    account_id: &str,
    own_user_id: &str,
    base: &str,
    access_token: &str,
    extra: &[String],
) -> Vec<String> {
    let mut targets: Vec<String> = vec![String::new()];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Our own server by name as well as locally would ask the same directory
    // twice and list every one of its rooms twice with it.
    if let Some(own) = own_user_id.rsplit(':').next() {
        seen.insert(own.to_string());
    }

    // Asked of the server rather than read off the buffers this window has
    // built, because those appear over the first minute of a session: a
    // search run before a room's buffer existed silently left that room's
    // homeserver out, and the results looked like the server had nothing.
    let mut from_rooms: Vec<String> = state.runtime.matrix_joined_rooms(account_id).into_iter().collect();
    if let Ok(resp) = http::get_json(&format!("{base}/_matrix/client/v3/joined_rooms"), access_token).await {
        from_rooms.extend(resp["joined_rooms"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)));
    }

    let named = from_rooms.iter().filter_map(|id| server_of_room(id).map(str::to_string));
    for server in named.chain(extra.iter().map(|s| server_name(s))) {
        if !server.is_empty() && seen.insert(server.clone()) {
            targets.push(server);
        }
    }
    targets
}

/// The homeserver named inside a room id, where there is one.
///
/// Room ids used to be `!opaque:server.example` and this could be taken as
/// "everything after the colon". Room version 12 ids are the hash of the
/// create event and carry no server at all - `!phpQp7HD_h1IuRlU61Vop1...` and
/// nothing else - so splitting on a colon that is not there returned the whole
/// room id as if it were a hostname. That was then asked to search its own
/// directory, and the server answered M_BAD_JSON, which is exactly what it
/// should say about `?server=!phpQp7HD...`.
///
/// A room whose id names no server is not a lost cause elsewhere - it is
/// reachable through the people in it - but it has nothing to contribute to a
/// list of directories to search, so it is skipped here.
fn server_of_room(room_id: &str) -> Option<&str> {
    let (_, server) = room_id.split_once(':')?;
    // A hostname, not merely "text after a colon": the point is to catch
    // anything that would be sent as ?server= and be nonsense there.
    let plausible = !server.is_empty()
        && server.contains('.')
        && server.chars().all(|c| c.is_ascii_alphanumeric() || "-._:[]".contains(c));
    plausible.then_some(server)
}

/// A homeserver's name, from whatever somebody typed. A bare hostname, not a
/// URL: this names a server in the Matrix sense, and one typed with a scheme
/// would be rejected by ours.
fn server_name(typed: &str) -> String {
    typed.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string()
}

#[cfg(test)]
mod directory_tests {
    use super::{server_name, server_of_room};

    /// Room version 12 ids are a hash and nothing else. Reading a server out
    /// of one gave the whole room id as a hostname, which was then asked to
    /// search its own directory and answered M_BAD_JSON - a real error in the
    /// server list, against a "server" that was a room.
    #[test]
    fn a_room_id_only_names_a_server_when_it_has_one() {
        assert_eq!(server_of_room("!abc:matrix.org"), Some("matrix.org"));
        assert_eq!(server_of_room("!phpQp7HD_h1IuRlU61Vop1-1RL5GxApK3Foo8E6KHxM"), None);
        assert_eq!(server_of_room("!abc:"), None);
        // A hostname has a dot in it; "localhost" is not something to go
        // asking a public directory of.
        assert_eq!(server_of_room("!abc:localhost"), None);
        assert_eq!(server_of_room("!abc:matrix.example.com:8448"), Some("matrix.example.com:8448"));
    }

    #[test]
    fn a_server_is_named_however_somebody_typed_it() {
        assert_eq!(server_name("matrix.org"), "matrix.org");
        assert_eq!(server_name("  https://matrix.org/  "), "matrix.org");
        assert_eq!(server_name("http://glowers.club"), "glowers.club");
        assert_eq!(server_name(""), "");
    }
}

/// Who somebody is, as far as Matrix will say.
///
/// Three sources, because Matrix keeps them apart: the profile endpoint has
/// the name and the picture, the room's power levels have their standing in
/// this room, and presence has when they were last seen. Presence is often
/// refused - a server may not federate it, or may have it switched off - and
/// that is not an error, just an absence.
pub async fn profile(state: &AppState, account_id: &str, buffer_id: &str, user_id: &str) -> Value {
    let mut profile = crate::profile::pending("matrix", account_id, user_id);
    profile["pending"] = serde_json::json!(false);
    profile["id"] = Value::String(user_id.to_string());
    profile["handle"] = Value::String(user_id.to_string());
    profile["name"] = Value::String(protocol::mxid_localpart(user_id));

    let Some(account) = state.accounts.get_matrix(account_id) else { return profile };
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded_user = url::form_urlencoded::byte_serialize(user_id.as_bytes()).collect::<String>();

    if let Ok(resp) = http::get_json(&format!("{base}/_matrix/client/v3/profile/{encoded_user}"), &account.access_token).await {
        if let Some(name) = resp["displayname"].as_str().filter(|n| !n.is_empty()) {
            profile["name"] = Value::String(name.to_string());
        }
        if let Some(mxc) = resp["avatar_url"].as_str() {
            if let Some(path) = cached_media_path(&account.homeserver_url, &account.access_token, mxc, "").await {
                profile["avatarUrl"] = Value::String(path);
            }
        }
    }

    // Their standing in this room, from what the sync already carries.
    if let Some(room_id) = state.runtime.get_matrix_room(buffer_id) {
        if let Some(levels) = state.runtime.get_matrix_power_levels(account_id, &room_id) {
            let level = moderation::user_power_level(&levels, user_id);
            // Matrix's own conventional names for the two ranks anybody
            // recognises. A room can set any number, so a level that is
            // neither is reported as itself rather than rounded to a word.
            let role = match level {
                100 => Some("Admin".to_string()),
                50 => Some("Moderator".to_string()),
                0 => None,
                other => Some(format!("Power level {other}")),
            };
            if let Some(role) = role {
                profile["roles"] = serde_json::json!([role]);
            }
            profile["isModerator"] = serde_json::json!(level >= 50);
        }
    }

    // When they were last seen. `last_active_ago` is milliseconds, and is the
    // only "last seen" Matrix has - there is no join date to be had without
    // walking the room's whole state history.
    if let Ok(resp) = http::get_json(&format!("{base}/_matrix/client/v3/presence/{encoded_user}/status"), &account.access_token).await {
        if let Some(status) = resp["presence"].as_str() {
            profile["status"] = Value::String(status.to_string());
        }
        if let Some(ago) = resp["last_active_ago"].as_i64() {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            profile["lastActiveTs"] = serde_json::json!(now - ago / 1000);
        }
        if let Some(message) = resp["status_msg"].as_str().filter(|m| !m.is_empty()) {
            crate::profile::note(&mut profile, "Status", message);
        }
    }

    profile
}

/// Reads a thread from the server, then hands back everything known about it.
///
/// `/relations` is the only way to see a thread whole: its replies are
/// ordinary timeline events, so a room read backwards would find them only by
/// paging back far enough to have crossed all of them, and a thread that has
/// been quiet for a month is arbitrarily far back.
///
/// What comes back is read out of the store rather than built here, so a
/// thread shows the same messages in the same shape whether they arrived
/// live, in room history, or through this - and so anything already stored
/// keeps its edits and reactions instead of being replaced by a plainer copy.
pub async fn fetch_thread(state: &AppState, account_id: &str, buffer_id: &str, root_id: &str) -> Result<Vec<crate::model::Message>> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let root = url::form_urlencoded::byte_serialize(root_id.as_bytes()).collect::<String>();
    // v1, not v3: relations were added to the spec after the v3 client API was
    // frozen, and live under /client/v1 for every server that has them.
    let url = format!("{base}/_matrix/client/v1/rooms/{room}/relations/{root}/m.thread?dir=f&limit=100");

    match http::get_json(&url, &account.access_token).await {
        Ok(resp) => {
            let session = state.runtime.get_matrix_machine(account_id);
            for event in resp["chunk"].as_array().into_iter().flatten() {
                store_thread_event(state, account_id, buffer_id, &room_id, &account.user_id, session.as_deref(), event).await;
            }
        }
        // A thread nobody has added to, a server that does not implement
        // relations, or a root that has been redacted. What is already stored
        // is still worth showing, so this is not fatal.
        Err(e) => tracing::debug!("matrix: reading thread {root_id}: {e:#}"),
    }

    state.store.thread_messages(buffer_id, root_id).map_err(Into::into)
}

/// One event from a thread, decrypted if it needs to be, into the store.
/// Deliberately quiet about anything it cannot use: a thread is read whole,
/// and one unreadable reply should not cost the rest of it.
async fn store_thread_event(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    room_id: &str,
    own_user_id: &str,
    session: Option<&crypto::CryptoSession>,
    event: &Value,
) {
    let decrypted;
    let event = if protocol::event_type(event) == protocol::EVENT_ROOM_ENCRYPTED {
        match session {
            // The trust half is for the timeline, where a message is drawn
            // beside a name; here only the plaintext matters.
            Some(session) => match decrypt_event(session, event, room_id).await.ok().map(|(plain, _)| plain) {
                Some(plain) => {
                    decrypted = plain;
                    &decrypted
                }
                None => return,
            },
            None => return,
        }
    } else {
        event
    };
    if protocol::event_type(event) != protocol::EVENT_ROOM_MESSAGE {
        return;
    }
    let content = &event["content"];
    if protocol::edit_target(content).is_some() {
        return;
    }
    let Some(event_id) = protocol::event_id(event) else { return };
    let (body, is_action) = protocol::message_body(content);
    if body.is_empty() {
        return;
    }
    let sender_mxid = protocol::sender(event);
    let ts = event["origin_server_ts"].as_i64().map(|ms| ms / 1000).unwrap_or(0);
    let reply_to = relation_preview(state, buffer_id, content);
    let _ = state.store.append_message(
        buffer_id,
        event_id,
        &protocol::short_sender(event),
        &body,
        ts,
        is_action,
        false,
        "message",
        reply_to.as_ref(),
        &[],
        sender_mxid == own_user_id,
        state.runtime.get_matrix_member_avatar(account_id, sender_mxid).as_deref(),
        &[],
        &[],
        Some(sender_mxid),
        protocol::formatted_body(content),
        &state.runtime.buffer_kind_of(buffer_id),
        None,
        &[],
    );
}

/// Stores one event read back from the server, decrypting it first if it
/// arrived encrypted.
///
/// Extracted from `backfill` so that reading history and reaching one
/// particular message - a pinned one, a search result - put the same thing in
/// the store. Two loops that stored "almost the same" message would drift, and
/// the one used less often would be the one that drifted.
///
/// Returns whether anything was stored: an event that is not a message, an
/// edit, or one that will not decrypt is skipped rather than stored as a
/// placeholder, because a screenful of "unable to decrypt" is worse than a
/// shorter page of what can be read.
async fn store_history_event(
    state: &AppState,
    account: &crate::accounts::MatrixAccountConfig,
    account_id: &str,
    buffer_id: &str,
    room_id: &str,
    session: Option<&std::sync::Arc<crypto::CryptoSession>>,
    event: &Value,
) -> bool {
    let decrypted;
    let event = if protocol::event_type(event) == "m.room.encrypted" {
        let plain = match session {
            Some(session) => decrypt_event(session, event, room_id).await.ok().map(|(plain, _)| plain),
            None => None,
        };
        match plain {
            Some(plain) => {
                decrypted = plain;
                &decrypted
            }
            None => return false,
        }
    } else {
        event
    };
    if protocol::event_type(event) != "m.room.message" {
        return false;
    }
    let Some(event_id) = protocol::event_id(event) else { return false };
    let content = &event["content"];
    // An edit carries the replacement rather than a message of its own.
    if protocol::edit_target(content).is_some() {
        return false;
    }
    let (body, is_action) = protocol::message_body(content);
    if body.is_empty() {
        return false;
    }
    let html = protocol::formatted_body(content).map(str::to_string);
    let from = protocol::short_sender(event);
    let sender_mxid = protocol::sender(event);
    let is_own = sender_mxid == account.user_id;
    // Real send time, unlike live messages before server-time existed
    // elsewhere: history dated to the moment it was fetched would put
    // years-old conversation at today.
    let ts = event["origin_server_ts"].as_i64().map(|ms| ms / 1000).unwrap_or(0);
    let avatar = state.runtime.get_matrix_member_avatar(account_id, sender_mxid);
    // History carries its relations like anything else - without this a
    // thread read back from the server arrived as loose messages.
    let reply_to = relation_preview(state, buffer_id, content);
    if let Err(e) = state.store.append_message(
        buffer_id, event_id, &from, &body, ts, is_action, false, "message", reply_to.as_ref(), &[], is_own,
        avatar.as_deref(), &[], &[], Some(sender_mxid), html.as_deref(),
        &state.runtime.buffer_kind_of(buffer_id),
        // Matrix has no per-sender colour or badges of its own.
        None,
        &[],
    ) {
        tracing::warn!("matrix: storing history message: {e}");
        return false;
    }
    true
}

/// Fetches the conversation around one event and stores it.
///
/// What makes a pinned message or a search result reachable: both name a
/// message that may be years older than anything this client has, and paging
/// backwards to it would mean reading the whole room in between. The server
/// will hand over that one moment directly, so this asks for it - and stores
/// the messages either side of it too, because arriving at a line with no
/// conversation around it is arriving nowhere.
///
/// Returns when the message was sent, which is what a client needs to go and
/// read it out of the store.
/// Reads forward from an event, for a reader working back towards the present
/// from somewhere they jumped to.
///
/// Two requests rather than one: `/messages` pages from a token rather than
/// from an event, and the only place to get a token pointing at one
/// particular moment is `/context` for that event - which is what `end` is.
///
/// Returns how many messages were stored, so a caller can tell "here is more"
/// from "there is no more".
pub async fn load_newer(state: &AppState, account_id: &str, buffer_id: &str, event_id: &str) -> Result<usize> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let event = url::form_urlencoded::byte_serialize(event_id.as_bytes()).collect::<String>();

    let anchored = http::get_json(&format!("{base}/_matrix/client/v3/rooms/{room}/context/{event}?limit=1"), &account.access_token)
        .await
        .context("finding that part of the room")?;
    let end = anchored["end"].as_str().context("the server gave no way to read forward from there")?;
    let from = url::form_urlencoded::byte_serialize(end.as_bytes()).collect::<String>();
    let resp = http::get_json(
        &format!("{base}/_matrix/client/v3/rooms/{room}/messages?dir=f&limit=50&from={from}"),
        &account.access_token,
    )
    .await
    .context("reading the rest of the room")?;

    let session = state.runtime.get_matrix_machine(account_id);
    let mut added = 0usize;
    // dir=f already reads oldest first, unlike the backward page.
    for event in resp["chunk"].as_array().cloned().unwrap_or_default() {
        if store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), &event).await {
            added += 1;
        }
    }
    Ok(added)
}

pub async fn load_context(state: &AppState, account_id: &str, buffer_id: &str, event_id: &str) -> Result<i64> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let event = url::form_urlencoded::byte_serialize(event_id.as_bytes()).collect::<String>();
    let url = format!("{base}/_matrix/client/v3/rooms/{room}/context/{event}?limit=30");
    let resp = http::get_json(&url, &account.access_token).await.context("reading that part of the room")?;

    let session = state.runtime.get_matrix_machine(account_id);
    // Oldest first, so the store reads in the order it was said: the events
    // before this one arrive newest-first from the server.
    let before: Vec<Value> = resp["events_before"].as_array().cloned().unwrap_or_default();
    for event in before.iter().rev() {
        store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), event).await;
    }
    let target = resp["event"].clone();
    store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), &target).await;
    for event in resp["events_after"].as_array().cloned().unwrap_or_default() {
        store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), &event).await;
    }

    // From the event itself rather than from the store: an event that would
    // not decrypt is not in the store, and the client still needs to know
    // where in the room it was to read what surrounds it.
    target["origin_server_ts"]
        .as_i64()
        .map(|ms| ms / 1000)
        .context("the server did not say when that message was sent")
}

pub async fn backfill(state: &AppState, account_id: &str, buffer_id: &str, limit: u32) -> Result<usize> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let Some(from) = state.runtime.matrix_back_token(account_id, &room_id) else { return Ok(0) };

    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let from_enc = url::form_urlencoded::byte_serialize(from.as_bytes()).collect::<String>();
    let url = format!("{base}/_matrix/client/v3/rooms/{room}/messages?dir=b&limit={limit}&from={from_enc}");
    let resp = http::get_json(&url, &account.access_token).await?;

    // The sync loop's own session, not a second one: opening the crypto
    // store twice would mean two writers to the same SQLite file for no
    // gain. Absent only when the account is not connected, in which case
    // encrypted history is skipped rather than waited for.
    let session = state.runtime.get_matrix_machine(account_id);
    let mut added = 0usize;
    // dir=b returns newest first; stored oldest first so scrollback reads in
    // the order it was said.
    let chunk: Vec<Value> = resp["chunk"].as_array().cloned().unwrap_or_default();
    for event in chunk.iter().rev() {
        if store_history_event(state, &account, account_id, buffer_id, &room_id, session.as_ref(), event).await {
            added += 1;
        }
    }

    // Where the next page starts. Absent means the room has been read to its
    // beginning, and the token is left alone so a later call does not loop
    // over the same page forever.
    if let Some(end) = resp["end"].as_str() {
        state.runtime.set_matrix_back_token(account_id, &room_id, end, false);
    }
    Ok(added)
}

/// Says that this account is composing something in a room.
///
/// Matrix wants a timeout with the notice and cancels with `typing: false`,
/// unlike Discord's single fire-and-expire - so a caller that stops typing
/// can actually say so rather than waiting the notice out.
/// Reads this account's push rules, once, at connect.
///
/// Matrix keeps them on the server so every client agrees: a room muted on a
/// phone is muted here, and a keyword added on one client notifies on all of
/// them. moho read none of them, so it agreed with nothing.
///
/// Stored whole and interpreted at the point of use - see notify_decision.
/// Only a subset of the rule language is honoured, and honestly: the two
/// kinds people actually set are a per-room mute and a keyword.
/// Reads `m.ignored_user_list` - the account's own block list.
///
/// Account data rather than a local preference, which is the whole point:
/// blocking somebody on one machine and hearing from them on the next is not
/// blocking them. A server that filters them out of sync is doing the same
/// thing from its end; this client honours the list either way, since not
/// every homeserver does.
async fn fetch_ignored_users(state: &AppState, account_id: &str, homeserver_url: &str, access_token: &str, user_id: &str) {
    let base = homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/user/{}/account_data/m.ignored_user_list",
        url::form_urlencoded::byte_serialize(user_id.as_bytes()).collect::<String>()
    );
    match http::get_json(&url, access_token).await {
        Ok(content) => apply_ignored_users(state, account_id, &content),
        // A 404 is an account that has never ignored anybody, which is the
        // ordinary case and not worth a word.
        Err(e) => tracing::debug!("matrix[{account_id}]: reading the ignore list: {e:#}"),
    }
}

/// Takes an `m.ignored_user_list` content and makes it this account's list.
pub fn apply_ignored_users(state: &AppState, account_id: &str, content: &Value) {
    let users: std::collections::HashSet<String> = content["ignored_users"]
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    state.runtime.set_matrix_ignored(account_id, users.clone());
    let mut listed: Vec<&String> = users.iter().collect();
    listed.sort();
    state.events.emit(
        "matrixIgnored",
        serde_json::json!({ "accountId": account_id, "users": listed }),
    );
}

/// Adds somebody to the ignore list, or takes them off it.
///
/// The list is read back from the server first for the same reason pinning is:
/// this is a whole-map replacement, and writing a stale copy would un-ignore
/// whoever was added from another client since this one last looked.
pub async fn set_ignored_user(state: &AppState, account_id: &str, user_id: &str, ignored: bool) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/user/{}/account_data/m.ignored_user_list",
        url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>()
    );
    let mut users: std::collections::BTreeMap<String, Value> = match http::get_json(&url, &account.access_token).await {
        Ok(content) => content["ignored_users"]
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        Err(_) => Default::default(),
    };
    if ignored {
        // The value is an empty object by spec - the key is the whole
        // statement.
        users.insert(user_id.to_string(), serde_json::json!({}));
    } else {
        users.remove(user_id);
    }
    let content = serde_json::json!({ "ignored_users": users });
    http::put_json(&url, &account.access_token, content.clone()).await.context("writing the ignore list")?;
    apply_ignored_users(state, account_id, &content);
    Ok(())
}

async fn fetch_push_rules(state: &AppState, account_id: &str, homeserver_url: &str, access_token: &str) {
    let base = homeserver_url.trim_end_matches('/');
    match http::get_json(&format!("{base}/_matrix/client/v3/pushrules/"), access_token).await {
        Ok(rules) => state.runtime.set_matrix_push_rules(account_id, rules),
        Err(e) => tracing::debug!("matrix[{account_id}]: reading push rules: {e:#}"),
    }
}

/// Mutes a room for this account, everywhere it is signed in - or stops.
///
/// A per-room push rule, which is what Element writes and what a phone reads.
/// Muting only locally was the old behaviour and is still what the client
/// does for services with no such idea; on Matrix the account itself can hold
/// the answer, so it should.
pub async fn set_room_muted(state: &AppState, account_id: &str, buffer_id: &str, muted: bool) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let url = format!("{base}/_matrix/client/v3/pushrules/global/room/{encoded}");

    if muted {
        // An empty action list is how the current spec spells "notify about
        // nothing"; `dont_notify` is the older word for it and servers still
        // accept both. The new one is sent, since that is what a current
        // client reading it back expects to find.
        http::put_json(&url, &account.access_token, serde_json::json!({ "actions": [] })).await.context("muting the room")?;
    } else {
        http::delete_json(&url, &account.access_token).await.context("unmuting the room")?;
    }
    state.runtime.set_silenced(buffer_id, muted);

    // The rules are cached; re-read rather than patch the copy, so what is
    // held is what the server actually has.
    fetch_push_rules(state, account_id, &account.homeserver_url, &account.access_token).await;
    Ok(())
}

/// Whether a room's messages should announce themselves, per the account's
/// own push rules.
///
/// Two questions are asked of them, because two are all anybody sets: is this
/// room muted, and does this message contain a word somebody asked to be told
/// about. Everything else in the rule language - conditions on message counts,
/// sender display names, arbitrary event fields - is left to the server, whose
/// job it is, and to the clients that edit it.
fn push_rule_verdict(rules: &Value, room_id: &str, body: &str) -> (bool, bool) {
    let muted = rules["global"]["room"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rule| rule["rule_id"].as_str() == Some(room_id))
        .filter(|rule| rule["enabled"].as_bool().unwrap_or(true))
        .any(|rule| silences(&rule["actions"]));

    let lower = body.to_lowercase();
    let keyword = rules["global"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rule| rule["enabled"].as_bool().unwrap_or(true))
        // The default content rule is the account's own name, which this
        // client already matches for itself; skipping it keeps one mechanism
        // rather than two disagreeing about the same thing.
        .filter(|rule| rule["rule_id"].as_str() != Some(".m.rule.contains_user_name"))
        .filter_map(|rule| rule["pattern"].as_str())
        .any(|pattern| matches_keyword(&lower, &pattern.to_lowercase()));

    (muted, keyword)
}

/// Whether a rule's actions amount to "say nothing".
///
/// Both spellings: the old `dont_notify` action, and the newer form where an
/// empty action list means the same thing.
fn silences(actions: &Value) -> bool {
    let Some(actions) = actions.as_array() else { return false };
    actions.is_empty() || actions.iter().any(|a| a.as_str() == Some("dont_notify"))
}

/// A keyword match, on word boundaries - Matrix's own globbing allows `*`,
/// and a bare word must not match inside a longer one.
fn matches_keyword(body: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if pattern.contains('*') {
        let mut cursor = 0usize;
        for part in pattern.split('*').filter(|p| !p.is_empty()) {
            match body[cursor..].find(part) {
                Some(at) => cursor += at + part.len(),
                None => return false,
            }
        }
        return true;
    }
    body.split(|c: char| !c.is_alphanumeric()).any(|word| word == pattern)
}

#[cfg(test)]
mod push_rule_tests {
    use super::{matches_keyword, push_rule_verdict};
    use serde_json::json;

    #[test]
    fn a_muted_room_is_muted_here_too() {
        let rules = json!({ "global": { "room": [{ "rule_id": "!quiet:example.org", "actions": ["dont_notify"] }] } });
        assert_eq!(push_rule_verdict(&rules, "!quiet:example.org", "anything").0, true);
        assert_eq!(push_rule_verdict(&rules, "!other:example.org", "anything").0, false);
        // The newer spelling of the same thing.
        let empty = json!({ "global": { "room": [{ "rule_id": "!quiet:example.org", "actions": [] }] } });
        assert_eq!(push_rule_verdict(&empty, "!quiet:example.org", "x").0, true);
        // A rule somebody switched off says nothing.
        let off = json!({ "global": { "room": [{ "rule_id": "!quiet:example.org", "actions": [], "enabled": false }] } });
        assert_eq!(push_rule_verdict(&off, "!quiet:example.org", "x").0, false);
    }

    #[test]
    fn a_keyword_notifies() {
        let rules = json!({ "global": { "content": [{ "rule_id": "moho", "pattern": "moho", "actions": ["notify"] }] } });
        assert_eq!(push_rule_verdict(&rules, "!r:example.org", "look at MOHO today").1, true);
        assert_eq!(push_rule_verdict(&rules, "!r:example.org", "nothing here").1, false);
        // Inside a longer word is not the word.
        assert_eq!(push_rule_verdict(&rules, "!r:example.org", "mohoism").1, false);
    }

    /// The default rule is the account's own name, which this client already
    /// matches for itself - honouring it too would be two mechanisms
    /// disagreeing about one thing.
    #[test]
    fn the_built_in_name_rule_is_left_to_the_client() {
        let rules = json!({ "global": { "content": [
            { "rule_id": ".m.rule.contains_user_name", "pattern": "someone", "actions": ["notify"] }
        ]}});
        assert_eq!(push_rule_verdict(&rules, "!r:example.org", "hello someone").1, false);
    }

    #[test]
    fn globs_match_the_way_matrix_writes_them() {
        assert!(matches_keyword("deploying the release now", "deploy*"));
        assert!(matches_keyword("a build failed", "*failed"));
        assert!(!matches_keyword("a build passed", "*failed"));
    }
}

/// Asks for everybody's read markers once, at connect.
///
/// Receipts only ever arrive through `/sync`, and a session resuming from a
/// stored token is told only what has *changed* since then - so a client that
/// waited for them would show an empty gutter until somebody happened to read
/// something, which on a quiet room is never.
///
/// So this is a second sync from scratch, filtered down to nothing but the
/// receipts: no state, no presence, and one timeline message per room because
/// zero is not something every homeserver will agree to. The `next_batch` it
/// comes back with is deliberately thrown away - the real sync loop keeps its
/// own place, and taking this one would skip everything in between.
/// The invitations already waiting when this client signed in.
///
/// Same reason the receipts above need their own sync, and the same shape: a
/// session resuming from a stored token is told what changed, and an invite
/// that arrived while the client was closed has already changed - so it is
/// mentioned once, if at all, and never again. An invite nobody has answered
/// is not news that expires, so it is asked for outright at connect.
async fn fetch_pending_invites(
    state: &AppState,
    account_id: &str,
    own_user_id: &str,
    homeserver_url: &str,
    access_token: &str,
) -> Result<()> {
    let filter = serde_json::json!({
        "room": { "timeline": { "limit": 1 }, "ephemeral": { "types": [] } },
        "presence": { "types": [] }
    })
    .to_string();
    let url = format!(
        "{}/_matrix/client/v3/sync?timeout=0&filter={}",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(filter.as_bytes()).collect::<String>()
    );
    let resp = http::get_json(&url, access_token).await.context("invite sync")?;
    let invites: Vec<Value> = resp["rooms"]["invite"]
        .as_object()
        .map(|rooms| {
            rooms
                .iter()
                .map(|(room_id, room)| {
                    let events: Vec<&Value> = room["invite_state"]["events"].as_array().into_iter().flatten().collect();
                    rooms::invite_summary(room_id, own_user_id, &events)
                })
                .collect()
        })
        .unwrap_or_default();
    if state.runtime.merge_matrix_invites(account_id, invites, &Default::default()) {
        let invites = state.runtime.matrix_invites(account_id);
        state.events.emit("matrixInvites", serde_json::json!({ "accountId": account_id, "invites": invites }));
    }
    Ok(())
}

async fn fetch_read_receipts(
    state: &AppState,
    account_id: &str,
    own_user_id: &str,
    homeserver_url: &str,
    access_token: &str,
) -> Result<()> {
    let filter = serde_json::json!({
        "room": { "timeline": { "limit": 1 }, "state": { "types": [] }, "ephemeral": { "limit": 100 } },
        "presence": { "types": [] }
    })
    .to_string();
    let url = format!(
        "{}/_matrix/client/v3/sync?timeout=0&filter={}",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(filter.as_bytes()).collect::<String>()
    );
    let resp = http::get_json(&url, access_token).await.context("receipt sync")?;
    let Some(joined) = resp["rooms"]["join"].as_object() else { return Ok(()) };
    for (room_id, room) in joined {
        for event in room["ephemeral"]["events"].as_array().into_iter().flatten() {
            if event["type"].as_str() == Some("m.receipt") {
                take_read_receipts(state, account_id, room_id, own_user_id, &event["content"]);
            }
        }
    }
    Ok(())
}

/// Takes in who else has read how far, from a room's `m.receipt`.
///
/// Matrix says this as a map of event id -> receipt type -> user, which is
/// the inverse of what a client draws: the marker belongs beside the message
/// somebody has read up to, and a person only ever has one of those. So this
/// flattens to one entry per person, newest wins, and emits the room's whole
/// set rather than a delta - a set is what the frontend draws, and rebuilding
/// one from deltas is how a marker gets stuck against a message somebody has
/// long since read past.
///
/// Only `m.read` appears here. `m.read.private` is by definition never
/// federated to anybody else, so a receipt reaching this function is one its
/// sender chose to publish; see mark_read for the other side of that choice.
///
/// Our own receipts are dropped - every client hides your own marker, because
/// the message you have read up to is the one you are looking at.
fn take_read_receipts(state: &AppState, account_id: &str, room_id: &str, own_user_id: &str, content: &Value) {
    let Some(by_event) = content.as_object() else { return };
    let mut changed = false;
    for (event_id, receipts) in by_event {
        let Some(readers) = receipts["m.read"].as_object() else { continue };
        for (user_id, receipt) in readers {
            if user_id == own_user_id {
                continue;
            }
            // Threads are not drawn as their own place yet, so a receipt
            // against one would put somebody's marker beside a message they
            // have not necessarily reached in the room itself.
            match receipt["thread_id"].as_str() {
                None | Some("main") => {}
                Some(_) => continue,
            }
            changed |= state.runtime.set_matrix_read_receipt(account_id, room_id, user_id, event_id);
        }
    }
    if changed {
        emit_read_receipts(state, account_id, room_id);
    }
}

/// Re-sends a buffer's read markers, for a frontend that has just started
/// caring about it. A no-op for anything that is not a Matrix room, so the
/// caller need not first ask which protocol a buffer is.
pub fn replay_read_receipts(state: &AppState, buffer_id: &str) {
    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return };
    if !buffer.account_id.starts_with("matrix:") {
        return;
    }
    let Some(room_id) = state.runtime.get_matrix_room(buffer_id) else { return };
    emit_read_receipts(state, &buffer.account_id, &room_id);
}

/// Sends the room's whole set of read markers to the frontend.
fn emit_read_receipts(state: &AppState, account_id: &str, room_id: &str) {
    let Some((buffer_name, _)) = state.runtime.get_matrix_room_name(account_id, room_id) else { return };
    let members = state.runtime.get_matrix_room_members(account_id, room_id);
    let readers: Vec<Value> = state
        .runtime
        .get_matrix_read_receipts(account_id, room_id)
        .into_iter()
        // Somebody who has left still has a receipt on the server. Drawing it
        // would claim a person is in a room they are not in.
        .filter(|(user_id, _)| members.contains_key(user_id))
        .map(|(user_id, event_id)| {
            serde_json::json!({
                "userId": user_id,
                "nick": members.get(&user_id).cloned().unwrap_or_else(|| protocol::mxid_localpart(&user_id)),
                "avatarUrl": state.runtime.get_matrix_member_avatar(account_id, &user_id),
                "messageId": event_id,
            })
        })
        .collect();
    state.events.emit(
        "readReceipts",
        serde_json::json!({
            "accountId": account_id,
            "bufferId": crate::model::buffer_id(account_id, &buffer_name),
            "readers": readers,
        }),
    );
}

/// Says this room has been read, as far as its newest message.
///
/// Two markers, because Matrix has two and they answer different questions.
/// `m.read` is public - it is what puts your avatar against a message in
/// somebody else's client - and `m.fully_read` is private, and is what your
/// own other clients use to stop showing the room as unread.
///
/// Sent together through the one endpoint that takes both, so reading a room
/// here stops it being bold on your phone. Before this, `markBufferRead` was
/// a no-op for Matrix and the two never agreed.
///
/// `publicly` is what the privacy toggle turns off, and turning it off swaps
/// `m.read` for `m.read.private` rather than dropping it. Both settle the
/// room's unread count on the server; only the public one is federated, so
/// the difference is whether other people are told - not whether your own
/// devices agree. Sending nothing at all would have bought the same privacy
/// by making the room unread again on every other device you own.
pub async fn mark_read(state: &AppState, account_id: &str, buffer_id: &str, publicly: bool) -> Result<()> {
    let Some(config) = state.accounts.get_matrix(account_id) else { anyhow::bail!("no such account") };
    let Some(room_id) = state.runtime.get_matrix_room(buffer_id) else {
        anyhow::bail!("no known Matrix room for this buffer")
    };
    // The newest message this client actually holds. Nothing to say if the
    // room has never had one - and claiming to have read a room that is empty
    // would be a receipt pointing at nothing.
    let Some(event_id) = state.store.newest_message_id(buffer_id).ok().flatten().filter(|id| id.starts_with('$')) else {
        return Ok(());
    };
    let base = config.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/read_markers",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>()
    );
    let mut body = serde_json::json!({ "m.fully_read": event_id });
    body[if publicly { "m.read" } else { "m.read.private" }] = Value::String(event_id);
    http::post_json(&url, Some(&config.access_token), body).await.context("sending read markers")?;
    Ok(())
}

/// Who a message is aimed at, in the form the spec calls intentional
/// mentions.
///
/// Names are matched against the room's own roster, longest first, so
/// "@Sam Vimes" is one person rather than Sam and a loose word. "@room" is the
/// whole-room mention, which is a flag rather than a name.
///
/// Absent - a null - when nobody was named, because an empty `m.mentions` is
/// a positive statement that a message mentions nobody, and that is only
/// worth sending when it is a correction.
fn intentional_mentions(state: &AppState, account_id: &str, room_id: &str, body: &str) -> Value {
    let mut named: Vec<String> = Vec::new();
    let members = state.runtime.get_matrix_room_members(account_id, room_id);

    let mut by_length: Vec<(&String, &String)> = members.iter().collect();
    by_length.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));
    let lower = body.to_lowercase();
    for (user_id, display) in by_length {
        if display.is_empty() {
            continue;
        }
        if lower.contains(&format!("@{}", display.to_lowercase())) && !named.contains(user_id) {
            named.push(user_id.clone());
        }
    }

    let room_wide = lower.contains("@room");
    if named.is_empty() && !room_wide {
        return Value::Null;
    }
    let mut mentions = serde_json::Map::new();
    if !named.is_empty() {
        mentions.insert("user_ids".into(), serde_json::json!(named));
    }
    if room_wide {
        mentions.insert("room".into(), serde_json::json!(true));
    }
    Value::Object(mentions)
}

pub async fn send_typing(state: &AppState, account_id: &str, room_id: &str, typing: bool) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.trim().as_bytes()).collect::<String>();
    let user = url::form_urlencoded::byte_serialize(account.user_id.trim().as_bytes()).collect::<String>();
    let body = if typing {
        serde_json::json!({ "typing": true, "timeout": crate::backend::discord::TYPING_TTL_MS })
    } else {
        serde_json::json!({ "typing": false })
    };
    http::put_json(&format!("{base}/_matrix/client/v3/rooms/{room}/typing/{user}"), &account.access_token, body).await?;
    Ok(())
}

pub async fn leave_room(state: &AppState, account_id: &str, room_id: &str) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded = url::form_urlencoded::byte_serialize(room_id.trim().as_bytes()).collect::<String>();
    http::post_json(
        &format!("{base}/_matrix/client/v3/rooms/{encoded}/leave"),
        Some(&account.access_token),
        serde_json::json!({}),
    )
    .await
    .context("leaving room")?;
    let _ = http::post_json(
        &format!("{base}/_matrix/client/v3/rooms/{encoded}/forget"),
        Some(&account.access_token),
        serde_json::json!({}),
    )
    .await;
    Ok(())
}

/// Opens (creating if necessary) a 1:1 DM room with `target_user_id` -
/// reuses an already-known DM room with them if one exists (see Runtime::
/// find_matrix_dm_room), otherwise creates a fresh one via `createRoom`
/// with `is_direct: true` (the flag every Matrix client uses to recognize
/// a room as a DM rather than a group chat) and a `trusted_private_chat`
/// preset (invited member gets a private, encrypted-by-default room with
/// symmetric power levels - matches what every other Matrix client offers
/// for "message this person").
///
/// `target_display_name` comes from the caller (the userlist entry the
/// "Open DM" action was invoked from already has it - see roomstate.rs's
/// emit_matrix_presence) rather than being re-derived here: the room we
/// just created only has the *invited*, not yet *joined*, target member,
/// so rooms.rs's derive_room_info - whose DM heuristic counts joined
/// members - wouldn't recognize this as a DM at all yet. The buffer this
/// creates is reconciled the normal way once the room's own first real
/// `/sync` response arrives (process_sync_response's cached-name check
/// already no-ops once a name/kind is set, same as any other room).
/// Makes a room, or a space.
///
/// A space is a room with `m.space` as its creation type and nothing else
/// different, which is why one call makes both - inventing a second path for
/// it would be inventing a distinction the protocol does not have.
///
/// Encryption is offered and is off by default. Turning it on afterwards is
/// possible and turning it off never is, so the default is the one that can
/// still be changed - and a room somebody meant to be private is a room they
/// will say so about.
///
/// The buffer is not created here. The room arrives through the next sync
/// like any other, with the name and kind the server settled on, and creating
/// one now would mean guessing at both and reconciling later.
pub async fn create_room(
    state: &AppState,
    account_id: &str,
    name: &str,
    topic: &str,
    is_space: bool,
    is_public: bool,
    encrypted: bool,
) -> Result<String> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');

    let mut body = serde_json::json!({
        "name": name,
        // A public room is one anybody can find and join; a private one is
        // invite-only. Both are ordinary presets rather than a pile of state
        // events, which is what every other client sends too.
        "preset": if is_public { "public_chat" } else { "private_chat" },
        "visibility": if is_public { "public" } else { "private" },
    });
    if !topic.trim().is_empty() {
        body["topic"] = serde_json::Value::String(topic.trim().to_string());
    }
    if is_space {
        body["creation_content"] = serde_json::json!({ "type": "m.space" });
    }
    if encrypted {
        // The one piece of initial state worth sending: a room encrypted from
        // its first message has no plaintext history to leak, and one turned
        // on later always does.
        body["initial_state"] = serde_json::json!([{
            "type": "m.room.encryption",
            "state_key": "",
            "content": { "algorithm": "m.megolm.v1.aes-sha2" }
        }]);
    }

    let resp = http::post_json(&format!("{base}/_matrix/client/v3/createRoom"), Some(&account.access_token), body)
        .await
        .context("creating room")?;
    resp["room_id"].as_str().map(str::to_string).context("createRoom response missing room_id")
}

/// Changes one of a room's own state events - its name or its topic.
///
/// Both are the same shape of call with a different type, and both are
/// refused by the server if this account's power level is too low, which is
/// The threads a room has, newest activity first.
///
/// A thread can be opened from its root message and continued, but a root
/// that has scrolled past is unreachable without this: the room knows its
/// threads and only the server can list them.
///
/// Each one carries what the panel needs to be worth opening - who started
/// it, how many replies it has, whether this account has said anything in it
/// - which is exactly what the server sends alongside the root event.
pub async fn list_threads(state: &AppState, account_id: &str, buffer_id: &str, limit: u32) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded_room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    // Client v1, not v3: threads arrived after v3 was frozen, the same way
    // /relations did.
    //
    // `include=all` is sent rather than left to the default, which the spec
    // says is that anyway: Conduit's deserializer treats the parameter as
    // required and answers M_BAD_JSON without it, so two of the four rooms
    // tested here could not list their threads at all. Synapse accepts it
    // either way, so saying it costs nothing and fixes a whole homeserver.
    let url = format!(
        "{base}/_matrix/client/v1/rooms/{encoded_room}/threads?include=all&limit={}",
        limit.clamp(1, 100)
    );
    let resp = http::get_json(&url, &account.access_token).await.context("listing threads")?;

    let session = state.runtime.get_matrix_machine(account_id);
    let room = ruma_common::RoomId::parse(&room_id).ok();
    let members = state.runtime.get_matrix_room_members(account_id, &room_id);
    let mut threads = Vec::new();
    for root in resp["chunk"].as_array().into_iter().flatten() {
        let Some(root_id) = root["event_id"].as_str() else { continue };
        let root = read_event(&session, room.as_deref(), root).await;
        let sender = root["sender"].as_str().unwrap_or_default();
        let relation = &root["unsigned"]["m.relations"]["m.thread"];
        // The most recent reply, which is what somebody scanning a list of
        // threads is actually looking at.
        let latest = read_event(&session, room.as_deref(), &relation["latest_event"]).await;
        let name_of = |user: &str| {
            members.get(user).cloned().unwrap_or_else(|| protocol::mxid_localpart(user))
        };
        threads.push(serde_json::json!({
            "rootId": root_id,
            "from": name_of(sender),
            "body": root["content"]["body"].as_str().unwrap_or("This message cannot be read here."),
            "ts": root["origin_server_ts"].as_i64().unwrap_or_default() / 1000,
            "replies": relation["count"].as_i64().unwrap_or(0),
            // Whether this account has said anything in it - Element sorts
            // its own list by this, and it is the difference between "a
            // thread happened" and "a thread you are in happened".
            "joined": relation["current_user_participated"].as_bool().unwrap_or(false),
            "lastFrom": latest["sender"].as_str().map(|u| name_of(u)),
            "lastBody": latest["content"]["body"].as_str(),
            "lastTs": latest["origin_server_ts"].as_i64().map(|ms| ms / 1000),
        }));
    }
    Ok(serde_json::json!({ "bufferId": buffer_id, "threads": threads, "next": resp["next_batch"].clone() }))
}

/// An event as text, decrypted where it needs to be and where this session
/// can. Shared by the thread list and the pinned list, which both read events
/// the sync loop never handed to a buffer.
async fn read_event(session: &Option<std::sync::Arc<crypto::CryptoSession>>, room: Option<&ruma_common::RoomId>, event: &Value) -> Value {
    if event["type"].as_str() != Some("m.room.encrypted") {
        return event.clone();
    }
    if let (Some(session), Some(room)) = (session, room) {
        if let Ok(plain) = crypto::decrypt_room_event(session, event, room).await {
            return plain;
        }
    }
    event.clone()
}

/// The messages a room has pinned, as messages rather than as ids.
///
/// Looked up locally first, because most pins point at something this window
/// already has; anything else is fetched from the server and decrypted the
/// same way a scrollback message is. A pin whose message is gone is still
/// listed - the room is saying something is pinned, and silently dropping it
/// would be this client disagreeing with every other one.
pub async fn list_pinned(state: &AppState, account_id: &str, buffer_id: &str) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded_room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let session = state.runtime.get_matrix_machine(account_id);
    let room = ruma_common::RoomId::parse(&room_id).ok();

    let mut out = Vec::new();
    for event_id in state.runtime.get_matrix_pinned(account_id, &room_id) {
        if let Ok(Some(message)) = state.store.get_message(buffer_id, &event_id) {
            out.push(serde_json::to_value(message)?);
            continue;
        }
        let url = format!(
            "{base}/_matrix/client/v3/rooms/{encoded_room}/event/{}",
            url::form_urlencoded::byte_serialize(event_id.as_bytes()).collect::<String>()
        );
        let mut event = match http::get_json(&url, &account.access_token).await {
            Ok(event) => event,
            Err(e) => {
                tracing::debug!("matrix: pinned event {event_id} could not be read: {e:#}");
                out.push(serde_json::json!({
                    "id": event_id,
                    "bufferId": buffer_id,
                    "from": "",
                    "body": "This pinned message is no longer available.",
                    "ts": 0,
                    "kind": "system",
                }));
                continue;
            }
        };
        if event["type"].as_str() == Some("m.room.encrypted") {
            if let (Some(session), Some(room)) = (&session, &room) {
                match crypto::decrypt_room_event(session, &event, room).await {
                    Ok(plain) => event = plain,
                    Err(e) => tracing::debug!("matrix: pinned event {event_id} would not decrypt: {e:#}"),
                }
            }
        }
        let sender = event["sender"].as_str().unwrap_or_default();
        out.push(serde_json::json!({
            "id": event_id,
            "bufferId": buffer_id,
            "from": state
                .runtime
                .get_matrix_room_members(account_id, &room_id)
                .get(sender)
                .cloned()
                .unwrap_or_else(|| protocol::mxid_localpart(sender)),
            "body": event["content"]["body"].as_str().unwrap_or("This pinned message cannot be read here."),
            "ts": event["origin_server_ts"].as_i64().unwrap_or_default() / 1000,
            "kind": "chat",
        }));
    }
    Ok(serde_json::json!({ "bufferId": buffer_id, "pinned": out }))
}

/// Pins a message, or takes the pin off.
///
/// The list is read back from the server before it is written rather than
/// taken from this client's copy: pinning is a whole-list replacement, and
/// writing a stale list would unpin whatever somebody else pinned while this
/// window was not looking. Whether this account may do it at all is the
/// server's decision, and its refusal is reported in its own words.
pub async fn set_pinned(state: &AppState, account_id: &str, buffer_id: &str, event_id: &str, pinned: bool) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded_room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();

    let url = format!("{base}/_matrix/client/v3/rooms/{encoded_room}/state/m.room.pinned_events/");
    // A room that has never pinned anything has no such state event at all,
    // which is a 404 and an empty list rather than a failure.
    let current: Vec<String> = match http::get_json(&url, &account.access_token).await {
        Ok(content) => content["pinned"]
            .as_array()
            .map(|ids| ids.iter().filter_map(|id| id.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    let mut next: Vec<String> = current.into_iter().filter(|id| id != event_id).collect();
    if pinned {
        next.push(event_id.to_string());
    }
    set_room_state(state, account_id, buffer_id, "m.room.pinned_events", serde_json::json!({ "pinned": next })).await?;
    // Locally too, so the list is right before the next sync arrives.
    state.runtime.set_matrix_pinned(account_id, &room_id, next.clone());
    state.events.emit("pinnedMessages", serde_json::json!({ "bufferId": buffer_id, "pinned": next }));
    Ok(())
}


/// where that question belongs.
pub async fn set_room_state(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    event_type: &str,
    content: serde_json::Value,
) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/state/{event_type}/",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>()
    );
    http::put_json(&url, &account.access_token, content).await.context("setting room state")?;
    Ok(())
}

pub async fn open_dm(state: &AppState, account_id: &str, target_user_id: &str, target_display_name: &str) -> Result<String> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');

    if let Some(room_id) = state.runtime.find_matrix_dm_room(account_id, target_user_id) {
        if let Some((name, kind)) = state.runtime.get_matrix_room_name(account_id, &room_id) {
            let buffer = state.runtime.ensure_buffer(state, account_id, &name, &kind);
            return Ok(buffer.id);
        }
    }

    let create_resp = http::post_json(
        &format!("{base}/_matrix/client/v3/createRoom"),
        Some(&account.access_token),
        serde_json::json!({ "invite": [target_user_id], "is_direct": true, "preset": "trusted_private_chat" }),
    )
    .await
    .context("creating DM room")?;
    let room_id = create_resp["room_id"].as_str().context("createRoom response missing room_id")?.to_string();

    // The background sync loop runs concurrently and can plausibly have
    // already discovered and buffered this exact room by the time we get
    // here - a homeserver commonly flushes an in-flight long-poll
    // immediately for a room its own user just created. If it won that
    // race, it would have gone through rooms.rs's derive_room_info, which
    // can't yet recognize this as a DM (the invited peer hasn't joined,
    // so its "2 joined members" heuristic only sees us) and so falls back
    // to the raw room id under kind "channel" - and since a room's name/
    // kind is only ever derived once (see rooms.rs's own doc comment),
    // that would stick permanently. Reconcile it by discarding whatever
    // stray buffer exists for this room id and always (re)creating the
    // correctly-named "dm" one ourselves, regardless of which side got
    // here first.
    if let Some(stray_buffer_id) = state.runtime.get_buffer_id_for_matrix_room(account_id, &room_id) {
        state.runtime.remove_buffer(state, &stray_buffer_id);
    }

    let name = if target_display_name.is_empty() { target_user_id.to_string() } else { target_display_name.to_string() };
    state.runtime.set_matrix_room_name(account_id, &room_id, &name, "dm");
    let buffer = state.runtime.ensure_buffer(state, account_id, &name, "dm");
    state.runtime.set_matrix_room(state, &buffer.id, &room_id);

    Ok(buffer.id)
}

/// Sends a message - encrypted if the room is (see Runtime::
/// is_matrix_room_encrypted, kept current by process_sync_response), plain
/// otherwise. `buffer_id` must already have a known room id (see Runtime::
/// set_matrix_room, populated the moment a room's buffer is created).
/// `attachment_path`, when set, is uploaded first and sent as media - via
/// upload_media_message for a plain room or upload_encrypted_media_message
/// for an E2EE one, chosen the same way the text-message path already
/// picks share_and_encrypt_content or not below.
pub async fn send_message(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    access_token: &str,
    body: &str,
    reply_to_id: Option<&str>,
    thread: bool,
    attachment_path: Option<&str>,
) -> Result<()> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let is_encrypted = state.runtime.is_matrix_room_encrypted(buffer_id);

    let mut plain_content = if let Some(path) = attachment_path {
        if is_encrypted {
            upload_encrypted_media_message(base, access_token, path, body).await?
        } else {
            upload_media_message(base, access_token, path, body).await?
        }
    } else {
        // "/me waves" is an action, the same convention IRC uses and the one
        // Matrix spells m.emote. Inbound emotes were already understood;
        // typing one here sent the literal text. "//" escapes a leading
        // slash, matching how the IRC backend reads the same box.
        let (msgtype, body) = match body.strip_prefix('/') {
            Some(literal) if literal.starts_with('/') => ("m.text", literal),
            Some(rest) => match rest.strip_prefix("me ") {
                Some(action) => ("m.emote", action),
                None => ("m.text", body),
            },
            None => ("m.text", body),
        };
        let mut content = serde_json::json!({ "msgtype": msgtype, "body": body });
        // Who this is aimed at, said outright rather than left to be guessed
        // from the text. Matrix used to work by every client scanning every
        // message for its own name, which is why a mention could be missed by
        // one client and seen by another; `m.mentions` is the answer to that,
        // and it is what Element sends.
        let mentions = intentional_mentions(state, account_id, &room_id, body);
        if !mentions.is_null() {
            content["m.mentions"] = mentions;
        }
        // The formatting somebody typed, where they typed any. Sent
        // alongside the plain text rather than instead of it: body stays the
        // fallback for a client that will not render HTML.
        if let Some(html) = markup::to_html(body) {
            content["format"] = serde_json::json!("org.matrix.custom.html");
            content["formatted_body"] = serde_json::json!(html);
        }
        content
    };
    if let Some(target) = reply_to_id {
        plain_content["m.relates_to"] = if thread {
            // The in_reply_to alongside it is the fallback a client that does
            // not understand threads reads instead, and is marked as such so
            // one that does knows not to draw a quotation nobody wrote. Both
            // point at the thread's root here rather than at the last message
            // in it, which is a simplification: a client showing the fallback
            // sees the thread's opening quoted rather than the message being
            // answered.
            serde_json::json!({
                "rel_type": "m.thread",
                "event_id": target,
                "is_falling_back": true,
                "m.in_reply_to": { "event_id": target }
            })
        } else {
            serde_json::json!({ "m.in_reply_to": { "event_id": target } })
        };
    }

    let txn_id = model::next_message_id();
    let encoded_room_id = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let encoded_txn_id = url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>();

    let (event_type, body_json) = if is_encrypted {
        let session = state.runtime.get_matrix_machine(account_id).context("crypto session not ready yet")?;
        let member_ids = joined_member_ids(base, access_token, &room_id).await?;
        let room_id_ruma = ruma_common::RoomId::parse(&room_id).context("invalid room id")?;
        let content = session
            .share_and_encrypt_content(&account.homeserver_url, access_token, &room_id_ruma, member_ids, protocol::EVENT_ROOM_MESSAGE, plain_content)
            .await
            .context("encrypting message")?;
        (protocol::EVENT_ROOM_ENCRYPTED, content)
    } else {
        (protocol::EVENT_ROOM_MESSAGE, plain_content)
    };

    let url = format!("{base}/_matrix/client/v3/rooms/{encoded_room_id}/send/{event_type}/{encoded_txn_id}");
    http::put_json(&url, access_token, body_json).await.context("sending message")?;
    Ok(())
}

/// `m.room.message` msgtype + upload Content-Type for a local file, chosen
/// from its extension - a reasonable guess without needing to sniff file
/// contents, same convention backend/sneedchat/mod.rs's own attachment
/// handling uses. Shared by both the plaintext and encrypted upload paths
/// below - only the mime half is unused by the encrypted one (see its own
/// doc comment on why the upload itself always goes out as opaque bytes
/// regardless of the real content type).
fn media_msgtype_and_mime(ext: &str) -> (&'static str, &'static str) {
    match ext {
        "png" => ("m.image", "image/png"),
        "jpg" | "jpeg" => ("m.image", "image/jpeg"),
        "gif" => ("m.image", "image/gif"),
        "webp" => ("m.image", "image/webp"),
        "mp4" | "webm" => ("m.video", "video/mp4"),
        "mp3" | "ogg" | "wav" | "flac" => ("m.audio", "audio/mpeg"),
        _ => ("m.file", "application/octet-stream"),
    }
}

fn file_extension(path: &str) -> (String, String) {
    let filename = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    (filename, ext)
}

/// Uploads a local file to the homeserver's media repository and builds
/// the corresponding `m.room.message` content (`m.image`/`m.video`/
/// `m.audio`/`m.file`) - unencrypted rooms only, see
/// Changes what this account is called, for everyone.
///
/// The homeserver's copy rather than the local rename beside it in the
/// account panel: one is how moho refers to you and the other is what every
/// room you are in shows. Both exist on purpose, and only this one leaves the
/// machine.
pub async fn set_own_display_name(state: &AppState, account_id: &str, name: &str) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/profile/{}/displayname",
        url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>()
    );
    http::put_json(&url, &account.access_token, serde_json::json!({ "displayname": name }))
        .await
        .context("changing your display name")?;
    Ok(())
}

/// Changes this account's picture, for everyone.
///
/// Uploaded unencrypted on purpose, unlike an attachment: a profile picture
/// is shown to anybody who can see the account at all, including in rooms
/// this client has never been in, and there is nobody to share a key with.
pub async fn set_own_avatar(state: &AppState, account_id: &str, path: &str) -> Result<String> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let bytes = tokio::fs::read(path).await.context("reading the picture")?;
    let (filename, ext) = file_extension(path);
    let (_msgtype, mime) = media_msgtype_and_mime(&ext);
    let upload_url = format!(
        "{base}/_matrix/media/v3/upload?filename={}",
        url::form_urlencoded::byte_serialize(filename.as_bytes()).collect::<String>()
    );
    let resp = http_client_post_bytes(&upload_url, &account.access_token, mime, bytes)
        .await
        .context("uploading the picture")?;
    let content_uri = resp["content_uri"].as_str().context("the server took the picture but did not say where")?;

    let url = format!(
        "{base}/_matrix/client/v3/profile/{}/avatar_url",
        url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>()
    );
    http::put_json(&url, &account.access_token, serde_json::json!({ "avatar_url": content_uri }))
        .await
        .context("setting your picture")?;
    Ok(content_uri.to_string())
}

/// What the homeserver currently says this account is called and looks like.
pub async fn own_profile(state: &AppState, account_id: &str) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/profile/{}",
        url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>()
    );
    let profile = http::get_json(&url, &account.access_token).await.context("reading your profile")?;
    let avatar = match profile["avatar_url"].as_str() {
        Some(mxc) => cached_media_path(&account.homeserver_url, &account.access_token, mxc, "").await,
        None => None,
    };
    Ok(serde_json::json!({
        "accountId": account_id,
        "userId": account.user_id,
        "displayName": profile["displayname"].as_str().unwrap_or_default(),
        "avatarUrl": avatar,
    }))
}

/// upload_encrypted_media_message for the E2EE counterpart.
async fn upload_media_message(base: &str, access_token: &str, path: &str, body: &str) -> Result<Value> {
    let bytes = tokio::fs::read(path).await.context("reading attachment")?;
    let (filename, ext) = file_extension(path);
    let (msgtype, mime) = media_msgtype_and_mime(&ext);

    let upload_url = format!("{base}/_matrix/media/v3/upload?filename={}", url::form_urlencoded::byte_serialize(filename.as_bytes()).collect::<String>());
    let resp = http_client_post_bytes(&upload_url, access_token, mime, bytes).await.context("uploading media")?;
    let content_uri = resp["content_uri"].as_str().context("upload response missing content_uri")?;

    // "info.mimetype" matters on the *receive* side too, not just here -
    // this same content comes back through /sync as our own echo (and to
    // every other member), and extension_for_mimetype (see
    // handle_timeline_event's media branch) needs it to give the cached
    // file a real extension; without it the file lands in the local cache
    // with no extension at all and the client's URL-pattern-based
    // embed detector silently never recognizes it as media (see this
    // project's own earlier "media isn't embedding" fix for the exact same
    // failure mode with media authored by *other* clients).
    Ok(serde_json::json!({ "msgtype": msgtype, "body": if body.is_empty() { filename } else { body.to_string() }, "url": content_uri, "info": { "mimetype": mime } }))
}

/// Same as upload_media_message, but for an E2EE room: the file itself is
/// AES-256-CTR encrypted with a fresh one-time key (matrix-sdk-crypto's
/// AttachmentEncryptor - the same primitive/format `matrix-sdk` itself
/// uses, not hand-rolled here) *before* upload, so the homeserver only
/// ever stores ciphertext. The resulting per-file key/iv/hash go into
/// `content.file` (an `EncryptedFile`, spec-shaped, in place of the plain
/// `content.url` the unencrypted path uses) rather than a bare mxc URI -
/// that whole content object, key included, still passes through the
/// caller's normal share_and_encrypt_content step afterward like any other
/// message, so the per-file key itself is *also* only ever visible to
/// actual room members via Megolm, not just whoever can reach the media
/// repo.
///
/// The upload itself always goes out as opaque `application/octet-stream`
/// bytes with no `?filename=` query param - the ciphertext reveals nothing
/// about the real content either way, but the real filename/mimetype
/// (carried in this same event's `body`/`info` instead) has no reason to
/// ever reach the server in the clear.
async fn upload_encrypted_media_message(base: &str, access_token: &str, path: &str, body: &str) -> Result<Value> {
    use matrix_sdk_crypto::AttachmentEncryptor;
    use std::io::{Cursor, Read};

    let bytes = tokio::fs::read(path).await.context("reading attachment")?;
    let (filename, ext) = file_extension(path);
    let (msgtype, mime) = media_msgtype_and_mime(&ext);

    let mut cursor = Cursor::new(bytes);
    let mut encryptor = AttachmentEncryptor::new(&mut cursor);
    let mut ciphertext = Vec::new();
    encryptor.read_to_end(&mut ciphertext).context("encrypting attachment")?;
    let media_info = encryptor.finish();

    let upload_url = format!("{base}/_matrix/media/v3/upload");
    let resp = http_client_post_bytes(&upload_url, access_token, "application/octet-stream", ciphertext).await.context("uploading encrypted media")?;
    let content_uri = resp["content_uri"].as_str().context("upload response missing content_uri")?;

    let mut file = serde_json::to_value(&media_info).context("serializing encryption info")?;
    file["url"] = serde_json::json!(content_uri);

    // See upload_media_message's own comment on why "info.mimetype" (the
    // real type - unrelated to the octet-stream Content-Type the upload
    // itself went out as) matters for the receive-side cache extension.
    Ok(serde_json::json!({ "msgtype": msgtype, "body": if body.is_empty() { filename } else { body.to_string() }, "file": file, "info": { "mimetype": mime } }))
}

async fn http_client_post_bytes(url: &str, access_token: &str, content_type: &str, bytes: Vec<u8>) -> Result<Value> {
    let resp = http::http_client()
        .post(url)
        .bearer_auth(access_token)
        .header("Content-Type", content_type)
        .body(bytes)
        .send()
        .await
        .context("upload request failed")?;
    let status = resp.status();
    let body: Value = resp.json().await.context("invalid JSON response")?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {body}");
    }
    Ok(body)
}

/// The `m.replace` an edit is sent as.
///
/// The replacement carries the sender's formatting the same way a new
/// message does - editing a formatted message should not quietly flatten it
/// - while the outer body keeps the plain "* text" form that clients without
/// edit support fall back to showing.
fn edit_content(target_event_id: &str, body: &str) -> serde_json::Value {
    let mut new_content = serde_json::json!({ "msgtype": "m.text", "body": body });
    if let Some(html) = markup::to_html(body) {
        new_content["format"] = serde_json::json!("org.matrix.custom.html");
        new_content["formatted_body"] = serde_json::json!(html);
    }
    serde_json::json!({
        "msgtype": "m.text",
        "body": format!("* {body}"),
        "m.new_content": new_content,
        "m.relates_to": { "rel_type": "m.replace", "event_id": target_event_id },
    })
}

/// Applies a real edit. `buffer_id` must already have a known room id.
pub async fn edit_message(state: &AppState, account_id: &str, buffer_id: &str, access_token: &str, msg_id: &str, body: &str) -> Result<()> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');

    // Built once for both kinds of room. An edit reads the same either way,
    // and building it twice is how a formatted body would reach one and not
    // the other.
    let content = edit_content(msg_id, body);

    let (event_type, body_json) = if state.runtime.is_matrix_room_encrypted(buffer_id) {
        let session = state.runtime.get_matrix_machine(account_id).context("crypto session not ready yet")?;
        let member_ids = joined_member_ids(base, access_token, &room_id).await?;
        let room_id_ruma = ruma_common::RoomId::parse(&room_id).context("invalid room id")?;
        let content = session
            .share_and_encrypt_edit(&account.homeserver_url, access_token, &room_id_ruma, member_ids, content)
            .await
            .context("encrypting edit")?;
        (protocol::EVENT_ROOM_ENCRYPTED, content)
    } else {
        (protocol::EVENT_ROOM_MESSAGE, content)
    };

    let txn_id = model::next_message_id();
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/send/{event_type}/{}",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>(),
    );
    http::put_json(&url, access_token, body_json).await.context("sending edit")?;
    Ok(())
}

/// Sends one message-shaped event into a room and says what id it got.
///
/// Shared by the things that are messages without being chat - so far the
/// request that opens a verification with somebody. Encrypted where the room
/// is, for the same reason everything else is: a room that hides what is said
/// in it should not make an exception for this.
///
/// The event id is the return value because these events are referred to
/// afterwards - a verification is identified by the id of the message that
/// asked for it.
pub async fn send_room_event(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    room_id: &str,
    content: Value,
) -> Result<String> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let access_token = account.access_token.clone();

    let (event_type, body_json) = if state.runtime.is_matrix_room_encrypted(buffer_id) {
        let session = state.runtime.get_matrix_machine(account_id).context("crypto session not ready yet")?;
        let member_ids = joined_member_ids(base, &access_token, room_id).await?;
        let room_id_ruma = ruma_common::RoomId::parse(room_id).context("invalid room id")?;
        let encrypted = session
            .share_and_encrypt_content(&account.homeserver_url, &access_token, &room_id_ruma, member_ids, protocol::EVENT_ROOM_MESSAGE, content)
            .await
            .context("encrypting the message")?;
        (protocol::EVENT_ROOM_ENCRYPTED.to_string(), encrypted)
    } else {
        (protocol::EVENT_ROOM_MESSAGE.to_string(), content)
    };

    let txn_id = model::next_message_id();
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/send/{event_type}/{}",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>(),
    );
    let resp = http::put_json(&url, &access_token, body_json).await.context("sending the message")?;
    resp["event_id"]
        .as_str()
        .map(|id| id.to_string())
        .context("the server took the message but did not say what id it got")
}

/// Answers a poll./// Answers a poll.
///
/// The stable spelling is sent, which is what a current Element writes;
/// everything on the way in is read either way, because a room with older
/// clients in it has both. Encrypted where the room is - a vote is an event
/// like any other, and a room that hides its messages hides its votes.
///
/// The answer is recorded here as well as sent: the echo arrives on the next
/// sync, and a card that does not move when pressed reads as a card that did
/// not take the press.
pub async fn vote_in_poll(state: &AppState, account_id: &str, buffer_id: &str, poll_id: &str, answer: &str) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let access_token = account.access_token.clone();

    let content = serde_json::json!({
        "m.relates_to": { "rel_type": "m.reference", "event_id": poll_id },
        "m.selections": [answer],
    });

    let (event_type, body_json) = if state.runtime.is_matrix_room_encrypted(buffer_id) {
        let session = state.runtime.get_matrix_machine(account_id).context("crypto session not ready yet")?;
        let member_ids = joined_member_ids(base, &access_token, &room_id).await?;
        let room_id_ruma = ruma_common::RoomId::parse(&room_id).context("invalid room id")?;
        let encrypted = session
            .share_and_encrypt_content(&account.homeserver_url, &access_token, &room_id_ruma, member_ids, polls::POLL_RESPONSE[0], content)
            .await
            .context("encrypting the vote")?;
        (protocol::EVENT_ROOM_ENCRYPTED.to_string(), encrypted)
    } else {
        (polls::POLL_RESPONSE[0].to_string(), content)
    };

    let txn_id = model::next_message_id();
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/send/{event_type}/{}",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>(),
    );
    http::put_json(&url, &access_token, body_json).await.context("sending your vote")?;

    if !state.runtime.matrix_poll_ended(buffer_id, poll_id) {
        state.runtime.set_matrix_poll_vote(buffer_id, poll_id, &account.user_id, answer);
        polls::republish(state, account_id, buffer_id, poll_id, &account.user_id);
    }
    Ok(())
}

/// Redacts (deletes) a message. Redactions are always sent in cleartext,
/// even in an encrypted room (per the C-S API spec) - no crypto involved.
pub async fn delete_message(state: &AppState, account_id: &str, buffer_id: &str, access_token: &str, msg_id: &str) -> Result<()> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    redact_event(&account.homeserver_url, access_token, &room_id, msg_id).await
}

/// Adds or removes our own reaction. Adding encrypts (in an encrypted
/// room) the same way a message does; removing is a plain redaction of
/// the specific `m.reaction` event we sent (see Runtime::
/// get_matrix_own_reaction_event) - Matrix has no toggle-by-name endpoint
/// like Discord's.
pub async fn toggle_reaction(state: &AppState, account_id: &str, buffer_id: &str, access_token: &str, msg_id: &str, emoji: &str, add: bool) -> Result<()> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');

    if !add {
        let Some(event_id) = state.runtime.get_matrix_own_reaction_event(buffer_id, msg_id, emoji) else {
            anyhow::bail!("no known reaction event to remove");
        };
        return redact_event(&account.homeserver_url, access_token, &room_id, &event_id).await;
    }

    let (event_type, body_json) = if state.runtime.is_matrix_room_encrypted(buffer_id) {
        let session = state.runtime.get_matrix_machine(account_id).context("crypto session not ready yet")?;
        let member_ids = joined_member_ids(base, access_token, &room_id).await?;
        let room_id_ruma = ruma_common::RoomId::parse(&room_id).context("invalid room id")?;
        let content = session
            .share_and_encrypt_reaction(&account.homeserver_url, access_token, &room_id_ruma, member_ids, msg_id, emoji)
            .await
            .context("encrypting reaction")?;
        (protocol::EVENT_ROOM_ENCRYPTED, content)
    } else {
        let content = serde_json::json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": msg_id, "key": emoji } });
        (protocol::EVENT_REACTION, content)
    };

    let txn_id = model::next_message_id();
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/send/{event_type}/{}",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>(),
    );
    let resp = http::put_json(&url, access_token, body_json).await.context("sending reaction")?;
    // The reaction we just sent won't come back to us until the next
    // /sync (record_matrix_reaction_event normally runs from the receive
    // path) - record it locally right away too, so an immediate un-react
    // (before that sync arrives) can still find its event id.
    if let Some(event_id) = resp["event_id"].as_str() {
        state.runtime.record_matrix_reaction_event(buffer_id, msg_id, emoji, event_id, true);
    }
    Ok(())
}

async fn redact_event(homeserver_url: &str, access_token: &str, room_id: &str, target_event_id: &str) -> Result<()> {
    let base = homeserver_url.trim_end_matches('/');
    let txn_id = model::next_message_id();
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/redact/{}/{}",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(target_event_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>(),
    );
    http::put_json(&url, access_token, serde_json::json!({})).await.context("redacting event")?;
    Ok(())
}

/// The full member list of a room, needed to know who to establish Olm
/// sessions with / share the Megolm session with before sending into an
/// encrypted room. Fetched fresh per send rather than tracked incrementally
/// from `m.room.member` timeline events - simpler, and an extra round trip
/// per encrypted send is an acceptable cost for v1 (this project's other
/// backends make comparable per-send round trips already, e.g. Discord's
/// own REST send call).
async fn joined_member_ids(base: &str, access_token: &str, room_id: &str) -> Result<Vec<ruma_common::OwnedUserId>> {
    let encoded_room_id = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let resp = http::get_json(&format!("{base}/_matrix/client/v3/rooms/{encoded_room_id}/joined_members"), access_token)
        .await
        .context("fetching joined_members")?;
    let members = resp["joined"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.keys())
        .filter_map(|id| ruma_common::OwnedUserId::try_from(id.as_str()).ok())
        .collect();
    Ok(members)
}

/// Kicks off a login attempt in the background - see backend/discord.rs's
/// start_qr_login/backend/sneedchat's start_login for the identical
/// async-kickoff shape. Progress/result arrive via matrixLoginStatus/
/// matrixLoginResult events tagged with `login_id`, not the RPC response
/// (see rpc/methods.rs's addMatrixAccount).
/// Which ways a homeserver will let somebody sign in, resolving delegation
/// first - the address somebody types is often not where the API lives.
pub async fn login_flows(homeserver_url: &str) -> Result<Vec<String>> {
    let resolved = http::resolve_homeserver(homeserver_url).await?;
    auth::login_flows(&resolved).await
}

/// Signs in through the homeserver's own web login.
///
/// The shape is fixed by the spec and by what a desktop app can do: the
/// homeserver sends the browser back to a URL of our choosing with a one-time
/// token, and that token is exchanged for a real one. So this listens on
/// loopback for exactly one request, hands the browser a page saying it can
/// be closed, and finishes the login with what it carried.
///
/// Loopback rather than a custom scheme because it needs no registration with
/// the desktop, works the same on every platform, and cannot be claimed by
/// another application: the port is chosen by the kernel a moment before it
/// is used, and nothing else knows it.
pub fn start_sso_login(state: AppState, login_id: String, homeserver_url: String, provider: Option<String>) {
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(try_sso_login(&state, &login_id, &homeserver_url, provider.as_deref())).catch_unwind().await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("matrix sso login[{login_id}]: {error}");
        state.events.emit("matrixLoginResult", serde_json::json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

/// How long to hold the loopback listener open waiting for the browser.
const SSO_TIMEOUT: Duration = Duration::from_secs(300);

async fn try_sso_login(state: &AppState, login_id: &str, homeserver_url: &str, provider: Option<&str>) -> Result<()> {
    state.events.emit("matrixLoginStatus", serde_json::json!({ "loginId": login_id, "detail": "finding the server..." }));
    let homeserver_url = http::resolve_homeserver(homeserver_url).await?;
    let base = homeserver_url.trim_end_matches('/');

    let flows = auth::login_flows(&homeserver_url).await.unwrap_or_default();
    if !flows.iter().any(|flow| flow == "m.login.sso" || flow == "m.login.token") {
        anyhow::bail!("this homeserver does not offer single sign-on");
    }

    // Bound before the URL is built, because the URL has to contain the port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("opening a port for the sign-in to come back to")?;
    let port = listener.local_addr().context("reading the port")?.port();
    let redirect = format!("http://127.0.0.1:{port}/");
    let url = match provider.filter(|p| !p.is_empty()) {
        Some(provider) => format!(
            "{base}/_matrix/client/v3/login/sso/redirect/{}?redirectUrl={}",
            url::form_urlencoded::byte_serialize(provider.as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(redirect.as_bytes()).collect::<String>()
        ),
        None => format!(
            "{base}/_matrix/client/v3/login/sso/redirect?redirectUrl={}",
            url::form_urlencoded::byte_serialize(redirect.as_bytes()).collect::<String>()
        ),
    };

    // The client opens this; the daemon has no browser and should not pretend
    // to. Sent as a status rather than returned, because the RPC that started
    // this answered long before the person had finished typing a password
    // into somebody else's login page.
    state.events.emit(
        "matrixLoginStatus",
        serde_json::json!({ "loginId": login_id, "detail": "waiting for the browser...", "url": url }),
    );

    let token = tokio::time::timeout(SSO_TIMEOUT, wait_for_sso_token(listener))
        .await
        .map_err(|_| anyhow::anyhow!("the sign-in was not finished in time"))?
        .context("waiting for the browser to come back")?;

    state.events.emit("matrixLoginStatus", serde_json::json!({ "loginId": login_id, "detail": "logging in..." }));
    let login = auth::login_with_token(&homeserver_url, &token, None).await.context("finishing the sign-in")?;

    let config = MatrixAccountConfig {
        homeserver_url: homeserver_url.to_string(),
        user_id: login.user_id,
        // No password to keep, and that is a real difference rather than an
        // omission: anything that needs one - publishing cross-signing keys,
        // removing a device - will have to ask the homeserver for its own
        // interactive auth instead.
        password: String::new(),
        access_token: login.access_token,
        device_id: login.device_id,
        next_batch: None,
        display_name: None,
    };
    let saved = state.accounts.add_matrix(config)?;
    let account = crate::accounts::matrix_account_to_json(&saved, "connecting", false);
    spawn(state.clone(), saved);
    state.events.emit("matrixLoginResult", serde_json::json!({ "loginId": login_id, "success": true, "account": account }));
    Ok(())
}

/// Waits for the browser to arrive with a token, and tells the person they
/// can close the tab.
async fn wait_for_sso_token(listener: tokio::net::TcpListener) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut socket, _) = listener.accept().await.context("accepting the browser's request")?;
        let mut buffer = [0u8; 2048];
        let read = socket.read(&mut buffer).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
        let token = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|target| {
                let query = target.split_once('?')?.1;
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(key, _)| key == "loginToken")
                    .map(|(_, value)| value.into_owned())
            });

        let page = match &token {
            Some(_) => "<!doctype html><meta charset=utf-8><title>Signed in</title><body style=\"font-family:sans-serif;padding:2rem\"><h1>Signed in</h1><p>You can close this tab and go back to moho.</p>",
            None => "<!doctype html><meta charset=utf-8><title>Nothing to sign in with</title><body style=\"font-family:sans-serif;padding:2rem\"><h1>Nothing to sign in with</h1><p>The homeserver sent the browser back without a login token.</p>",
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;

        // A browser asking for /favicon.ico is not the answer; keep waiting
        // for the one that carries a token.
        if let Some(token) = token {
            return Ok(token);
        }
    }
}

pub fn start_login(state: AppState, login_id: String, homeserver_url: String, username: String, password: String) {
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(try_login(&state, &login_id, &homeserver_url, &username, &password)).catch_unwind().await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("matrix login[{login_id}]: {error}");
        state.events.emit("matrixLoginResult", serde_json::json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

async fn try_login(state: &AppState, login_id: &str, homeserver_url: &str, username: &str, password: &str) -> Result<()> {
    // Where the client API actually is, before anything is stored or tried.
    //
    // Resolved here rather than at every call site, and the *resolved* address
    // is what gets saved - so a delegated server is asked once at sign-in
    // rather than on every request forever, and an account that works keeps
    // working if the well-known later goes away.
    state.events.emit("matrixLoginStatus", serde_json::json!({ "loginId": login_id, "detail": "finding the server..." }));
    let homeserver_url = &http::resolve_homeserver(homeserver_url).await?;
    state.events.emit("matrixLoginStatus", serde_json::json!({ "loginId": login_id, "detail": "logging in..." }));

    // Saved before the login is attempted, so a failure leaves an account to
    // correct and retry rather than an empty form. Same reasoning as the
    // Sneedchat path: the moment somebody is told their password might be
    // wrong is the worst moment to also make them retype the server address.
    //
    // An empty access token is already a state this backend understands -
    // ensure_login treats it as "never successfully logged in" and
    // authenticates from the stored password - so a retry needs nothing that
    // is not here, and a corrected password overwrites in place because the
    // account id comes from the user id.
    let pending = MatrixAccountConfig {
        homeserver_url: homeserver_url.to_string(),
        user_id: username.to_string(),
        password: password.to_string(),
        access_token: String::new(),
        device_id: String::new(),
        next_batch: None,
        display_name: None,
    };
    state.accounts.add_matrix(pending.clone())?;

    let login = auth::login(homeserver_url, username, password, None).await.context("logging in")?;

    let config = MatrixAccountConfig {
        homeserver_url: homeserver_url.to_string(),
        user_id: login.user_id,
        password: password.to_string(),
        access_token: login.access_token,
        device_id: login.device_id,
        next_batch: None,
        display_name: None,
    };
    let saved = state.accounts.add_matrix(config)?;
    // The placeholder above was keyed on whatever was typed, and the server
    // answers with the canonical user id - "salastil" against "@salastil:
    // poa.st". When they differ, the placeholder is a second account for the
    // same person, so it goes now that the real one exists.
    let pending_id = pending.account_id();
    if pending_id != saved.account_id() {
        // No event: removeAccount does not emit one either, and the frontend
        // re-reads the account list when the login result arrives a few lines
        // below. Inventing a name nothing listens for would only look like it
        // did something.
        let _ = state.accounts.remove(&pending_id);
    }
    let account = crate::accounts::matrix_account_to_json(&saved, "connecting", false);
    spawn(state.clone(), saved);

    state.events.emit("matrixLoginResult", serde_json::json!({ "loginId": login_id, "success": true, "account": account }));
    Ok(())
}

#[cfg(test)]
mod sso_tests {
    /// The browser comes back to a port this process opened a moment ago,
    /// carrying a one-time token in the query string. Everything about the
    /// sign-in hangs off reading that correctly, and a hand-rolled HTTP
    /// handler is exactly the place to get it wrong.
    #[tokio::test]
    async fn reads_the_token_the_browser_brings_back() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let waiting = tokio::spawn(super::wait_for_sso_token(listener));

        // A browser asking for the icon first, which must not be mistaken for
        // the answer - this is why the handler loops rather than taking the
        // first request it sees.
        let mut noise = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        noise.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n").await.expect("write");
        drop(noise);

        let mut browser = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        browser
            .write_all(b"GET /?loginToken=syt_abc123 HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write");

        // And the page it is answered with, so nobody is left looking at a
        // browser error after a sign-in that worked.
        let mut answer = Vec::new();
        browser.read_to_end(&mut answer).await.expect("read");
        let answer = String::from_utf8_lossy(&answer);
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert!(answer.contains("You can close this tab"), "{answer}");

        let token = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("in time")
            .expect("task")
            .expect("token");
        assert_eq!(token, "syt_abc123");
    }
}

#[cfg(test)]
mod tests {

    /// An edit carries the sender's formatting the same way a new message
    /// does. Building this in two places is how a formatted body reached
    /// unencrypted rooms and not encrypted ones.
    #[test]
    fn an_edit_keeps_the_formatting_it_was_given() {
        let c = super::edit_content("$abc", "**bold** now");
        assert_eq!(c["m.relates_to"]["rel_type"], "m.replace");
        assert_eq!(c["m.relates_to"]["event_id"], "$abc");
        // The outer body is the fallback a client without edit support shows.
        assert_eq!(c["body"], "* **bold** now");
        assert_eq!(c["m.new_content"]["body"], "**bold** now");
        assert_eq!(c["m.new_content"]["format"], "org.matrix.custom.html");
        assert_eq!(c["m.new_content"]["formatted_body"], "<strong>bold</strong> now");
    }

    /// A plain edit stays plain - no format keys at all, rather than a
    /// formatted body restating the text.
    #[test]
    fn a_plain_edit_carries_no_format() {
        let c = super::edit_content("$abc", "just words");
        assert!(c["m.new_content"].get("format").is_none());
        assert!(c["m.new_content"].get("formatted_body").is_none());
    }
    use super::*;
    use crate::state::AppState;

    #[test]
    fn carries_the_matrix_info_block_onto_the_attachment() {
        // Matrix already describes media properly; the point of build_attachment
        // is to stop throwing that description away.
        let content = serde_json::json!({
            "msgtype": "m.image",
            "body": "holiday.jpg",
            "url": "mxc://example.org/abc",
            "info": {
                "mimetype": "image/jpeg",
                "size": 148213,
                "w": 1920,
                "h": 1080,
                "xyz.amorgan.blurhash": "LEHV6nWB2yk8"
            }
        });
        let a = build_attachment(&content, Some("file:///cache/abc.jpg".into()), None);
        assert_eq!(a.kind, "image");
        assert_eq!(a.mimetype.as_deref(), Some("image/jpeg"));
        // `body` is the filename in Matrix, which is exactly what it is here.
        assert_eq!(a.filename.as_deref(), Some("holiday.jpg"));
        assert_eq!(a.size, Some(148213));
        assert_eq!((a.width, a.height), (Some(1920), Some(1080)));
        assert_eq!(a.blurhash.as_deref(), Some("LEHV6nWB2yk8"));
        assert_eq!(a.path.as_deref(), Some("file:///cache/abc.jpg"));
    }

    #[test]
    fn maps_every_media_msgtype_to_a_renderer_kind() {
        for (msgtype, want) in [
            ("m.image", "image"),
            ("m.video", "video"),
            ("m.audio", "audio"),
            ("m.file", "file"),
        ] {
            let content = serde_json::json!({ "msgtype": msgtype, "body": "x", "info": {} });
            assert_eq!(build_attachment(&content, None, None).kind, want, "for {msgtype}");
        }
    }

    #[test]
    fn an_unfetched_attachment_still_describes_itself() {
        // A failed or pending download must not lose the metadata - a frontend
        // can still show a placeholder of the right size and offer a retry.
        let content = serde_json::json!({
            "msgtype": "m.image", "body": "big.png",
            "info": { "mimetype": "image/png", "w": 640, "h": 480 }
        });
        let a = build_attachment(&content, None, None);
        assert!(a.path.is_none());
        assert_eq!((a.width, a.height), (Some(640), Some(480)));
        assert_eq!(a.filename.as_deref(), Some("big.png"));
    }

    /// Phase-2 live check: real login, a fresh private room created for
    /// the test (a throwaway account has no rooms of its own, and posting
    /// into a real public room isn't appropriate for an automated test),
    /// then the full connect-sync-receive-send loop against it. Reads
    /// credentials from the environment - see auth::tests::matrix_login_probe's
    /// doc comment for the invocation pattern (same env vars, this test
    /// name instead):
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_end_to_end_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_end_to_end_probe() {
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

        let login = auth::login(&homeserver, &username, &password, None).await.expect("login failed");
        println!("logged in as {}", login.user_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login.access_token),
            serde_json::json!({ "preset": "private_chat", "name": "nobilis-matrix-phase2-probe" }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id in createRoom response").to_string();
        println!("created test room {room_id}");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-probe-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord_voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::backend::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc_dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
        };

        let config = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login.user_id.clone(),
            password: password.clone(),
            access_token: login.access_token.clone(),
            device_id: login.device_id.clone(),
            next_batch: None,
            display_name: None,
        };
        let saved = state.accounts.add_matrix(config).expect("add_matrix failed");
        let account_id = saved.account_id();
        spawn(state.clone(), saved);

        let buffer_name = "nobilis-matrix-phase2-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer for the test room never appeared within 30s - first sync/buffer creation failed");
        println!("buffer created: {buffer_id}");
        assert_eq!(state.runtime.get_matrix_room(&buffer_id).as_deref(), Some(room_id.as_str()), "buffer's room id mapping is wrong");

        let sent_body = format!("phase2-probe-{}", model::next_message_id());
        send_message(&state, &account_id, &buffer_id, &login.access_token, &sent_body, None, false, None).await.expect("send_message failed");
        println!("sent: {sent_body}");

        let mut received = false;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(msgs) = state.store.get_backlog(&buffer_id, 0, 20) {
                if msgs.iter().any(|m| m.body == sent_body) {
                    received = true;
                    break;
                }
            }
        }
        assert!(received, "sent message never came back through the sync loop within 30s");
        println!("send/receive round-trip confirmed.");

        // Clean up: leave (and best-effort forget) the test room rather
        // than leaving it behind on the account indefinitely.
        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login.access_token),
            serde_json::json!({}),
        )
        .await;
    }

    /// Phase-3 manual live check: creates a real *encrypted* room, starts
    /// nobilis's own sync loop against it (as one "device" of the test
    /// account), then idles for up to 3 minutes printing every message
    /// this backend receives and decrypts. Meant to be run in the
    /// background while a second, independent Matrix client (e.g. Element
    /// Web, logged into the same account as a genuinely different device)
    /// joins the room and sends a real message into it - the room id is
    /// printed up front so it can be found there. This is the actual hard
    /// gate for "full E2EE from day one": a message this backend never
    /// touched the encryption of, decrypted correctly, proves the whole
    /// device-key-upload + key-claim + Megolm-session path works against
    /// a real second device, not just against itself.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_e2ee_read_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_e2ee_read_probe() {
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

        let login = auth::login(&homeserver, &username, &password, None).await.expect("login failed");
        println!("logged in as {} (device {})", login.user_id, login.device_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login.access_token),
            serde_json::json!({
                "preset": "private_chat",
                "name": "nobilis-matrix-phase3-probe",
                "initial_state": [
                    { "type": "m.room.encryption", "state_key": "", "content": { "algorithm": "m.megolm.v1.aes-sha2" } }
                ],
            }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id in createRoom response").to_string();
        println!("created ENCRYPTED test room {room_id} - join it with a second client (same account, different device) and send a message now.");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-e2ee-probe-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord_voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::backend::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc_dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
        };

        let config = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login.user_id.clone(),
            password: password.clone(),
            access_token: login.access_token.clone(),
            device_id: login.device_id.clone(),
            next_batch: None,
            display_name: None,
        };
        let saved = state.accounts.add_matrix(config).expect("add_matrix failed");
        let account_id = saved.account_id();
        spawn(state.clone(), saved);

        let buffer_name = "nobilis-matrix-phase3-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer for the test room never appeared within 30s");
        println!("buffer created: {buffer_id} - waiting up to 180s for a message from a second device...");

        let mut seen_bodies: std::collections::HashSet<String> = std::collections::HashSet::new();
        for i in 0..180 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(msgs) = state.store.get_backlog(&buffer_id, 0, 20) {
                for m in &msgs {
                    if seen_bodies.insert(m.id.clone()) {
                        println!("[{i}s] received from {}: {}", m.from, m.body);
                    }
                }
            }
        }

        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login.access_token),
            serde_json::json!({}),
        )
        .await;
    }

    /// Phase-3/phase-4 combined live check, fully automated (no manual
    /// second client needed): "device A" is nobilis's own real production
    /// connect loop (spawn/run_sync, same code path a real account uses);
    /// "device B" is a second, genuinely independent login of the same
    /// account (a real second `OlmMachine`/crypto store, its own device_id)
    /// that encrypts and sends a message using the exact same production
    /// `CryptoSession::share_and_encrypt` path `send_message`'s encrypted
    /// branch uses. Device A decrypting what device B encrypted - neither
    /// having touched the other's key material directly - is the real
    /// gate: device-key upload, key claim, Megolm session establishment,
    /// and decryption all have to work correctly end to end for this to
    /// pass, exactly the "nobilis sends, a different real client decrypts"
    /// (and the reverse) gates the plan calls for.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_e2ee_two_device_roundtrip
    #[tokio::test]
    #[ignore]
    async fn matrix_e2ee_two_device_roundtrip() {
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

        // --- Device A: nobilis's own real connect loop ---
        let login_a = auth::login(&homeserver, &username, &password, None).await.expect("device A login failed");
        println!("device A logged in as {} (device {})", login_a.user_id, login_a.device_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login_a.access_token),
            serde_json::json!({
                "preset": "private_chat",
                "name": "nobilis-matrix-phase34-probe",
                "initial_state": [
                    { "type": "m.room.encryption", "state_key": "", "content": { "algorithm": "m.megolm.v1.aes-sha2" } }
                ],
            }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id in createRoom response").to_string();
        println!("created ENCRYPTED test room {room_id}");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-e2ee-roundtrip-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord_voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::backend::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc_dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
        };

        let config_a = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login_a.user_id.clone(),
            password: password.clone(),
            access_token: login_a.access_token.clone(),
            device_id: login_a.device_id.clone(),
            next_batch: None,
            display_name: None,
        };
        let saved_a = state.accounts.add_matrix(config_a).expect("add_matrix failed");
        let account_id_a = saved_a.account_id();
        // Wipe any stale crypto store left over from an earlier run of
        // this test - run_sync's own crypto::CryptoSession::open() reads
        // from the real ~/.config/nobilis (not this test's isolated
        // temp data_dir; only the account/scrollback stores are
        // redirected there), so a leftover store from a previous run
        // still has identity keys for a *different* device_id than the
        // fresh one auth::login(..., None) just minted above -
        // OlmMachine::with_store() rejects that mismatch outright, which
        // silently wedges run_with_retry in an invisible (no tracing
        // subscriber in tests) backoff loop that never creates a buffer.
        let crypto_dir = dirs::home_dir().unwrap_or_default().join(".config").join("nobilis").join("matrix-crypto");
        let sanitized = account_id_a.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect::<String>();
        let _ = std::fs::remove_dir_all(crypto_dir.join(&sanitized));
        spawn(state.clone(), saved_a);

        let buffer_name = "nobilis-matrix-phase34-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer for the test room never appeared within 30s");
        println!("device A buffer created: {buffer_id}");

        // --- Device B: a second, independent login + OlmMachine ---
        let login_b = auth::login(&homeserver, &username, &password, None).await.expect("device B login failed");
        assert_ne!(login_b.device_id, login_a.device_id, "device B unexpectedly got the same device_id as device A");
        println!("device B logged in as {} (device {})", login_b.user_id, login_b.device_id);

        let user_id = ruma_common::UserId::parse(&login_b.user_id).expect("invalid user id");
        let device_id_b = <&ruma_common::DeviceId>::from(login_b.device_id.as_str());
        let account_id_b = format!("{account_id_a}-deviceB-test");
        let session_b = crypto::CryptoSession::open(&data_dir, &account_id_b, &user_id, device_id_b).await.expect("opening device B crypto store");

        // Let both devices exchange device-key info: device B uploads its
        // own keys, device A's already-running sync loop uploads its own
        // on its next cycle and will pick up device B's via device_lists
        // "changed" on a future /sync. A few rounds with short waits gives
        // both directions time to settle before device B tries to encrypt
        // for a member list that includes device A.
        for _ in 0..10 {
            session_b.process_outgoing_requests(&homeserver, &login_b.access_token).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let room_id_ruma = ruma_common::RoomId::parse(&room_id).expect("invalid room id");
        let sent_body = format!("phase34-roundtrip-{}", model::next_message_id());
        println!("device B encrypting and sending: {sent_body}");
        let encrypted_content = session_b
            .share_and_encrypt(&homeserver, &login_b.access_token, &room_id_ruma, vec![user_id.clone()], &sent_body)
            .await
            .expect("device B failed to encrypt/share room key");

        let txn_id = model::next_message_id();
        let send_url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/{}/{}",
            homeserver.trim_end_matches('/'),
            url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
            protocol::EVENT_ROOM_ENCRYPTED,
            url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>(),
        );
        http::put_json(&send_url, &login_b.access_token, encrypted_content).await.expect("device B failed to send encrypted event");
        println!("device B sent the encrypted event - waiting up to 60s for device A to decrypt it...");

        let mut received_body = None;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(msgs) = state.store.get_backlog(&buffer_id, 0, 20) {
                if let Some(m) = msgs.iter().find(|m| m.body == sent_body) {
                    received_body = Some(m.body.clone());
                    break;
                }
                // Also surface anything that came through as an
                // undecryptable placeholder, to distinguish "message
                // never arrived" from "arrived but failed to decrypt" in
                // the failure output.
                if let Some(m) = msgs.iter().find(|m| m.body.contains("unable to decrypt")) {
                    println!("saw an undecrypted placeholder: {}", m.body);
                }
            }
        }

        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login_a.access_token),
            serde_json::json!({}),
        )
        .await;

        assert_eq!(received_body.as_deref(), Some(sent_body.as_str()), "device A never decrypted the message device B sent");
        println!("E2EE round trip confirmed: device A correctly decrypted a message it never encrypted itself.");
    }

    /// Phase-5 live check: reply/edit/react/delete round-trip against a
    /// real (unencrypted) room, each verified via the same
    /// get_backlog-polling pattern the earlier phase probes use.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_phase5_polish_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_phase5_polish_probe() {
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

        let login = auth::login(&homeserver, &username, &password, None).await.expect("login failed");
        println!("logged in as {}", login.user_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login.access_token),
            serde_json::json!({ "preset": "private_chat", "name": "nobilis-matrix-phase5-probe" }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id").to_string();
        println!("created test room {room_id}");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-phase5-probe-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord_voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::backend::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc_dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
        };

        let config = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login.user_id.clone(),
            password: password.clone(),
            access_token: login.access_token.clone(),
            device_id: login.device_id.clone(),
            next_batch: None,
            display_name: None,
        };
        let saved = state.accounts.add_matrix(config).expect("add_matrix failed");
        let account_id = saved.account_id();
        let crypto_dir = dirs::home_dir().unwrap_or_default().join(".config").join("nobilis").join("matrix-crypto");
        let sanitized = account_id.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect::<String>();
        let _ = std::fs::remove_dir_all(crypto_dir.join(&sanitized));
        spawn(state.clone(), saved);

        let buffer_name = "nobilis-matrix-phase5-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer never appeared within 30s");
        println!("buffer created: {buffer_id}");

        // --- send the original message ---
        let original_body = format!("phase5-original-{}", model::next_message_id());
        send_message(&state, &account_id, &buffer_id, &login.access_token, &original_body, None, false, None).await.expect("send failed");
        let original_id = poll_for_message(&state, &buffer_id, |m| m.body == original_body, 30).await.expect("original message never arrived").id;
        println!("sent original: {original_id}");

        // --- reply ---
        let reply_body = format!("phase5-reply-{}", model::next_message_id());
        send_message(&state, &account_id, &buffer_id, &login.access_token, &reply_body, Some(&original_id), false, None).await.expect("reply send failed");
        let reply_msg = poll_for_message(&state, &buffer_id, |m| m.body == reply_body, 30).await.expect("reply never arrived");
        assert_eq!(reply_msg.reply_to.as_ref().map(|r| r.id.as_str()), Some(original_id.as_str()), "reply didn't record the right target");
        println!("reply confirmed, targets {}", original_id);

        // --- edit ---
        let edited_body = format!("phase5-edited-{}", model::next_message_id());
        edit_message(&state, &account_id, &buffer_id, &login.access_token, &original_id, &edited_body).await.expect("edit failed");
        let edited = poll_for_message(&state, &buffer_id, |m| m.id == original_id && m.body == edited_body, 30).await;
        assert!(edited.is_some(), "edit never applied");
        println!("edit confirmed");

        // --- react, then un-react ---
        toggle_reaction(&state, &account_id, &buffer_id, &login.access_token, &original_id, "👍", true).await.expect("react failed");
        let reacted = poll_until(30, || {
            state.store.get_message(&buffer_id, &original_id).ok().flatten().is_some_and(|m| m.reactions.iter().any(|r| r.emoji == "👍" && r.me))
        })
        .await;
        assert!(reacted, "reaction never showed up");
        println!("reaction confirmed");

        toggle_reaction(&state, &account_id, &buffer_id, &login.access_token, &original_id, "👍", false).await.expect("un-react failed");
        let unreacted = poll_until(30, || {
            state.store.get_message(&buffer_id, &original_id).ok().flatten().is_none_or(|m| !m.reactions.iter().any(|r| r.emoji == "👍"))
        })
        .await;
        assert!(unreacted, "reaction was never removed");
        println!("un-react confirmed");

        // --- delete ---
        delete_message(&state, &account_id, &buffer_id, &login.access_token, &reply_msg.id).await.expect("delete failed");
        let deleted = poll_until(30, || state.store.get_message(&buffer_id, &reply_msg.id).ok().flatten().is_none()).await;
        assert!(deleted, "message was never deleted");
        println!("delete confirmed");

        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login.access_token),
            serde_json::json!({}),
        )
        .await;
        println!("phase 5 polish probe passed: reply/edit/react/unreact/delete all confirmed.");
    }

    async fn poll_for_message(state: &AppState, buffer_id: &str, pred: impl Fn(&crate::model::Message) -> bool, secs: u32) -> Option<crate::model::Message> {
        for _ in 0..secs {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Ok(msgs) = state.store.get_backlog(buffer_id, 0, 20) {
                if let Some(m) = msgs.into_iter().find(|m| pred(m)) {
                    return Some(m);
                }
            }
        }
        None
    }

    async fn poll_until(secs: u32, mut pred: impl FnMut() -> bool) -> bool {
        for _ in 0..secs {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        pred()
    }

    /// Phase-5 live check: media upload + receive-side caching, against a
    /// real (unencrypted) room - a distinct code path (upload_media_message/
    /// cached_media_path) the other phase-5 probe doesn't touch.
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... MATRIX_TEST_IMAGE=/path/to.png \
    ///     cargo test --release -- --ignored --nocapture matrix_media_probe
    #[tokio::test]
    #[ignore]
    async fn matrix_media_probe() {
        let Ok(username) = std::env::var("MATRIX_USERNAME") else {
            println!("MATRIX_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("MATRIX_PASSWORD") else {
            println!("MATRIX_PASSWORD not set, skipping");
            return;
        };
        let Ok(image_path) = std::env::var("MATRIX_TEST_IMAGE") else {
            println!("MATRIX_TEST_IMAGE not set, skipping");
            return;
        };
        let homeserver = std::env::var("MATRIX_HOMESERVER").unwrap_or_else(|_| "https://matrix.org".to_string());
        let _ = rustls::crypto::ring::default_provider().install_default();

        let login = auth::login(&homeserver, &username, &password, None).await.expect("login failed");
        println!("logged in as {}", login.user_id);

        let create_resp = http::post_json(
            &format!("{}/_matrix/client/v3/createRoom", homeserver.trim_end_matches('/')),
            Some(&login.access_token),
            serde_json::json!({ "preset": "private_chat", "name": "nobilis-matrix-media-probe" }),
        )
        .await
        .expect("createRoom failed");
        let room_id = create_resp["room_id"].as_str().expect("no room_id").to_string();
        println!("created test room {room_id}");

        let data_dir = std::env::temp_dir().join(format!("nobilis-matrix-media-probe-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::store::Store::open(&data_dir.join("scrollback.db")).expect("opening store")),
            accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(data_dir.join("accounts.toml")).expect("opening accounts")),
            events: crate::events::EventBus::new(),
            runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
            tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&data_dir)),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
            voice: std::sync::Arc::new(crate::backend::discord_voice::VoiceState::new()),
            voice_prefs: std::sync::Arc::new(crate::backend::audio::VoicePrefsStore::open(data_dir.join("voice.toml"))),
            dcc_prefs: std::sync::Arc::new(crate::backend::irc_dcc::DccPrefsStore::open(data_dir.join("dcc.toml"))),
            highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(data_dir.join("highlights.toml"))),
        };

        let config = MatrixAccountConfig {
            homeserver_url: homeserver.clone(),
            user_id: login.user_id.clone(),
            password: password.clone(),
            access_token: login.access_token.clone(),
            device_id: login.device_id.clone(),
            next_batch: None,
            display_name: None,
        };
        let saved = state.accounts.add_matrix(config).expect("add_matrix failed");
        let account_id = saved.account_id();
        let crypto_dir = dirs::home_dir().unwrap_or_default().join(".config").join("nobilis").join("matrix-crypto");
        let sanitized = account_id.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect::<String>();
        let _ = std::fs::remove_dir_all(crypto_dir.join(&sanitized));
        spawn(state.clone(), saved);

        let buffer_name = "nobilis-matrix-media-probe";
        let mut buffer_id = None;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(b) = state.runtime.list_buffers().into_iter().find(|b| b.name == buffer_name) {
                buffer_id = Some(b.id);
                break;
            }
        }
        let buffer_id = buffer_id.expect("buffer never appeared within 30s");
        println!("buffer created: {buffer_id}");

        send_message(&state, &account_id, &buffer_id, &login.access_token, "", None, false, Some(&image_path)).await.expect("media send failed");
        println!("uploaded and sent {image_path}");

        let received = poll_for_message(&state, &buffer_id, |m| m.body.starts_with("file://"), 30).await;
        let msg = received.expect("media message never arrived as a cached file:// path");
        println!("received body: {}", msg.body);
        let local_path = msg.body.strip_prefix("file://").unwrap();
        assert!(std::path::Path::new(local_path).is_file(), "cached media file doesn't actually exist on disk: {local_path}");
        let cached_bytes = std::fs::read(local_path).expect("reading cached media file");
        let original_bytes = std::fs::read(&image_path).expect("reading original test image");
        assert_eq!(cached_bytes, original_bytes, "cached media content doesn't match what was uploaded");

        let _ = http::post_json(
            &format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver.trim_end_matches('/'), room_id),
            Some(&login.access_token),
            serde_json::json!({}),
        )
        .await;
        println!("media probe passed: upload -> receive -> local cache all confirmed, bytes match exactly.");
    }

    /// Unlike the two live probes above, this needs no network/homeserver
    /// at all - it isolates exactly the part of the encrypted-media path
    /// that's actually new/risky (upload_encrypted_media_message's encrypt-
    /// then-serialize-then-stash-url dance, and cached_encrypted_media_
    /// path's exact mirror of it on the way back down) from the
    /// unencrypted-path-reusing upload/download HTTP calls around it,
    /// which the media probe above already exercises for real. A real
    /// AES-256-CTR encrypt/decrypt round trip, through the exact same
    /// serde_json::Value shape both of those functions build/consume.
    #[test]
    fn matrix_encrypted_media_json_round_trip() {
        use matrix_sdk_crypto::{AttachmentDecryptor, AttachmentEncryptor, MediaEncryptionInfo};
        use std::io::{Cursor, Read};

        let plaintext = b"nobilis encrypted attachment round-trip probe".to_vec();

        // --- upload_encrypted_media_message's half ---
        let mut src = Cursor::new(plaintext.clone());
        let mut encryptor = AttachmentEncryptor::new(&mut src);
        let mut ciphertext = Vec::new();
        encryptor.read_to_end(&mut ciphertext).expect("encrypting");
        let media_info = encryptor.finish();

        let mut file = serde_json::to_value(&media_info).expect("serializing encryption info");
        file["url"] = serde_json::json!("mxc://example.org/fake-media-id");
        // A real server response would also be missing "hashes"/"key"
        // ordering guarantees etc - round-tripping through an actual
        // serde_json::Value (not the original struct) is the point, since
        // that's genuinely what flows over the wire as this message's
        // content.file.
        let file_over_the_wire: Value = serde_json::from_str(&file.to_string()).expect("re-parsing as if received fresh over the wire");

        // --- cached_encrypted_media_path's half ---
        let mut encryption_info = file_over_the_wire.clone();
        encryption_info.as_object_mut().expect("file is an object").remove("url");
        let encryption_info: MediaEncryptionInfo = serde_json::from_value(encryption_info).expect("deserializing encryption info back out");

        let mut ciphertext_cursor = Cursor::new(ciphertext);
        let mut decryptor = AttachmentDecryptor::new(&mut ciphertext_cursor, encryption_info).expect("building decryptor");
        let mut decrypted = Vec::new();
        decryptor.read_to_end(&mut decrypted).expect("decrypting");

        assert_eq!(decrypted, plaintext, "decrypted bytes don't match the original plaintext");
    }
}

/// The rail entry id for one Matrix space.
fn space_group_id(account_id: &str, room_id: &str) -> String {
    format!("{account_id}|space:{room_id}")
}

/// Turns a joined Space into a rail entry and files its rooms under it.
///
/// Spaces and their rooms arrive in whatever order joined_rooms happens to
/// list them, so this works both ways: children seen already are moved now,
/// and children seen later find the mapping waiting for them.
///
/// A room can be listed by more than one space; the last one processed wins,
/// since a buffer belongs to exactly one rail entry. That is a real Matrix
/// arrangement rather than an error, and picking one beats showing the room
/// twice.
async fn register_space(
    state: &AppState,
    account_id: &str,
    room_id: &str,
    homeserver_url: &str,
    access_token: &str,
    events: &[&Value],
) {
    let name = rooms::derive_room_info(room_id, "", events).name;
    let group_id = space_group_id(account_id, room_id);

    // Resolved to a local file for the same reason room avatars are: the mxc
    // URI needs an access token no frontend holds.
    let icon_url = match rooms::room_avatar_mxc(events) {
        Some(mxc) => cached_media_path(homeserver_url, access_token, &mxc, "").await,
        None => None,
    };

    state.runtime.upsert_buffer_group(
        state,
        crate::model::BufferGroup {
            id: group_id.clone(),
            account_id: account_id.to_string(),
            service: "matrix".to_string(),
            kind: "space".to_string(),
            name,
            icon_url,
            // Matrix gives spaces no ordering of its own, so they sort by name
            // among themselves - after Discord's guilds, which do carry one.
            position: 500,
            pending: false,
        },
    );

    // If the other side got here first and made a buffer for this space,
    // discard it: a space is not somewhere you talk, and leaving it in the
    // room list is how spaces end up looking like empty chats.
    if let Some(buffer_id) = state.runtime.matrix_buffer_for_room(account_id, room_id) {
        tracing::debug!("matrix[{account_id}]: {room_id} is a space, dropping the buffer made for it");
        state.runtime.remove_buffer(state, &buffer_id);
    }

    for child in rooms::space_children(events) {
        state.runtime.set_matrix_space_parent(account_id, &child, &group_id);
        if let Some(buffer_id) = state.runtime.matrix_buffer_for_room(account_id, &child) {
            state.runtime.set_buffer_group(state, &buffer_id, &group_id);
        }
    }
}

/// Pushes a status to the homeserver.
///
/// Matrix has three presence values - online, unavailable and offline. Idle
/// maps to unavailable, which is the closest honest answer to "am I here".
pub async fn apply_status(state: &AppState, config: &MatrixAccountConfig, status: &str) -> Result<()> {
    // Re-read rather than trusting the config passed in: a re-login rotates
    // the token, and a stale one fails with a bare 401.
    let account = state
        .accounts
        .get_matrix(&config.account_id())
        .context("account is no longer configured")?;
    let access_token = account.access_token;
    // Matrix has three, and they do not line up one for one. Do-not-disturb
    // is somebody present who does not want interrupting, which is closest to
    // unavailable; invisible has no equivalent at all, and offline is the
    // honest answer - it is what invisible means to everybody looking.
    let presence = match status {
        "idle" | "dnd" => "unavailable",
        "invisible" => "offline",
        _ => "online",
    };
    let user = url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>();
    let base = account.homeserver_url.trim_end_matches('/');
    http::put_json(
        &format!("{base}/_matrix/client/v3/presence/{user}/status"),
        &access_token,
        serde_json::json!({ "presence": presence }),
    )
    .await
    .context("setting presence")?;
    Ok(())
}
