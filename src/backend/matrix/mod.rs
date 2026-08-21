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
pub mod moderation;
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

        let url = sync_url(&config.homeserver_url, next_batch.as_deref());
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
    dirs::home_dir().unwrap_or_default().join(".config").join("nobilis")
}

fn sync_url(homeserver_url: &str, since: Option<&str>) -> String {
    let mut url = format!(
        "{}/_matrix/client/v3/sync?timeout={}&set_presence=online",
        homeserver_url.trim_end_matches('/'),
        if since.is_none() { 0 } else { SYNC_LONG_POLL_MS }
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

        let encoded_room_id = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
        let state_resp = match http::get_json(&format!("{base}/_matrix/client/v3/rooms/{encoded_room_id}/state"), access_token).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("matrix[{account_id}]: fetching state for room {room_id} failed: {e:#}");
                continue;
            }
        };
        let Some(events) = state_resp.as_array() else { continue };
        let event_refs: Vec<&Value> = events.iter().collect();

        let info = rooms::derive_room_info(&room_id, own_user_id, &event_refs);
        state.runtime.set_matrix_room_name(account_id, &room_id, &info.name, &info.kind);

        let buffer = state.runtime.ensure_buffer(state, account_id, &info.name, &info.kind);
        state.runtime.set_matrix_room(&buffer.id, &room_id);

        state.runtime.set_matrix_room_encrypted(state, &buffer.id, rooms::is_encrypted(&event_refs));

        roomstate::process_state_events(state, account_id, &room_id, homeserver_url, access_token, &event_refs).await;
        // No presence data available at bootstrap time (that's /sync-only
        // - there's no bulk "current presence for all these users"
        // endpoint) - the roster/power-level part of the userlist is
        // correct immediately, everyone just starts as offline until a
        // real presence.events update arrives post-connect.
        roomstate::emit_matrix_presence(state, account_id, &room_id, own_user_id);
    }
    Ok(())
}

async fn process_sync_response(state: &AppState, account_id: &str, own_user_id: &str, homeserver_url: &str, access_token: &str, resp: &Value, session: &crypto::CryptoSession) {
    // Global presence updates, handled before the per-room loop below since
    // presence.events is a top-level sibling of rooms, not scoped to any
    // one of them - a user going online/offline with no other room
    // activity this cycle wouldn't otherwise show up in rooms.join at all,
    // so this can't just be folded into the per-room pass.
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

    let Some(joined) = resp["rooms"]["join"].as_object() else { return };
    for (room_id, room) in joined {
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
        state.runtime.set_matrix_room(&buffer.id, room_id);

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
}

#[allow(clippy::too_many_arguments)]
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

    if outer_type != protocol::EVENT_ROOM_MESSAGE && outer_type != protocol::EVENT_ROOM_ENCRYPTED && outer_type != protocol::EVENT_REACTION {
        // Membership/name changes were already folded into naming in
        // process_sync_response; anything else (typing, receipts, other
        // state events) isn't rendered.
        return;
    }

    let sender = protocol::sender(event);
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
    let (effective_type, content, undecryptable): (String, Value, bool) = if outer_type == protocol::EVENT_ROOM_ENCRYPTED {
        match decrypt_event(session, event, room_id).await {
            Ok(decrypted) => {
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

    if effective_type != protocol::EVENT_ROOM_MESSAGE {
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
    } else {
        protocol::message_body(&content)
    };
    // An attachment with no caption legitimately has an empty body; dropping
    // the message then would lose the media entirely.
    if body.is_empty() && attachments.is_empty() {
        return;
    }

    let reply_to = match protocol::reply_target(&content) {
        Some(target_event) => match state.store.get_message(buffer_id, target_event) {
            Ok(Some(m)) => Some(crate::model::ReplyPreview { id: target_event.to_string(), from: m.from, body: m.body }),
            // Not (or no longer) in local scrollback - still record that
            // this is a reply, just without a preview to show.
            _ => Some(crate::model::ReplyPreview { id: target_event.to_string(), from: String::new(), body: String::new() }),
        },
        None => None,
    };

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

    let sender_avatar_url = state.runtime.get_matrix_member_avatar(account_id, sender);

    state.runtime.record_message(
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
        false,
        sender_avatar_url,
        Vec::new(),
        attachments,
        Some(sender.to_string()),
    );
}

/// Applies an incoming `m.room.redaction`: either a real message delete
/// (target is a message we have) or a reaction removal (target is a
/// reaction event we've seen - see Runtime::take_matrix_reaction_target).
/// Tries the reaction path first since it's a cheap map lookup+remove;
/// falls back to treating it as a message delete otherwise, matching the
/// same "try, then fall back" precedent backend/sockchat/mod.rs uses for
/// its own ambiguous edit-vs-insert wire signal.
fn handle_redaction(state: &AppState, buffer_id: &str, target_event: &str) {
    if let Some((target_buffer, msg_id, emoji, is_me)) = state.runtime.take_matrix_reaction_target(target_event) {
        state.runtime.update_reaction(state, &target_buffer, &msg_id, &emoji, is_me, false);
        return;
    }
    state.runtime.delete_message(state, buffer_id, target_event);
}

async fn decrypt_event(session: &crypto::CryptoSession, event: &Value, room_id: &str) -> Result<Value> {
    let room_id = ruma_common::RoomId::parse(room_id).context("invalid room id")?;
    crypto::decrypt_room_event(session, event, &room_id).await
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
/// backend/sockchat/mod.rs's cached attachments, which keep whatever
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
/// reasoning backend/sockchat/mod.rs's own avatar caching already
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
/// concern (and same fix) as backend/sockchat/mod.rs's avatar/attachment
/// caches: every distinct piece of media ever seen would otherwise
/// accumulate its own permanently-cached file forever.
pub const MEDIA_CACHE_MAX_BYTES: u64 = 250 * 1024 * 1024;
/// Evicts the oldest-written files in the media cache until it's back
/// under MEDIA_CACHE_MAX_BYTES - oldest-by-mtime, same simplification
/// backend/sockchat/mod.rs's own sweep_cache_dir documents (not true LRU,
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
pub async fn join_room(state: &AppState, account_id: &str, room_id_or_alias: &str) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded = url::form_urlencoded::byte_serialize(room_id_or_alias.trim().as_bytes()).collect::<String>();
    let url = format!("{base}/_matrix/client/v3/join/{encoded}");
    http::post_json(&url, Some(&account.access_token), serde_json::json!({})).await.context("joining room")?;
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
    if let Some(stray_buffer_id) = state.runtime.get_buffer_id_for_matrix_room(&room_id) {
        state.runtime.remove_buffer(state, &stray_buffer_id);
    }

    let name = if target_display_name.is_empty() { target_user_id.to_string() } else { target_display_name.to_string() };
    state.runtime.set_matrix_room_name(account_id, &room_id, &name, "dm");
    let buffer = state.runtime.ensure_buffer(state, account_id, &name, "dm");
    state.runtime.set_matrix_room(&buffer.id, &room_id);

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
        serde_json::json!({ "msgtype": "m.text", "body": body })
    };
    if let Some(target) = reply_to_id {
        plain_content["m.relates_to"] = serde_json::json!({ "m.in_reply_to": { "event_id": target } });
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
/// contents, same convention backend/sockchat/mod.rs's own attachment
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

/// Applies a real edit. `buffer_id` must already have a known room id.
pub async fn edit_message(state: &AppState, account_id: &str, buffer_id: &str, access_token: &str, msg_id: &str, body: &str) -> Result<()> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');

    let (event_type, body_json) = if state.runtime.is_matrix_room_encrypted(buffer_id) {
        let session = state.runtime.get_matrix_machine(account_id).context("crypto session not ready yet")?;
        let member_ids = joined_member_ids(base, access_token, &room_id).await?;
        let room_id_ruma = ruma_common::RoomId::parse(&room_id).context("invalid room id")?;
        let content = session
            .share_and_encrypt_edit(&account.homeserver_url, access_token, &room_id_ruma, member_ids, msg_id, body)
            .await
            .context("encrypting edit")?;
        (protocol::EVENT_ROOM_ENCRYPTED, content)
    } else {
        let content = serde_json::json!({
            "msgtype": "m.text",
            "body": format!("* {body}"),
            "m.new_content": { "msgtype": "m.text", "body": body },
            "m.relates_to": { "rel_type": "m.replace", "event_id": msg_id },
        });
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
/// start_qr_login/backend/sockchat's start_login for the identical
/// async-kickoff shape. Progress/result arrive via matrixLoginStatus/
/// matrixLoginResult events tagged with `login_id`, not the RPC response
/// (see rpc/methods.rs's addMatrixAccount).
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
    state.events.emit("matrixLoginStatus", serde_json::json!({ "loginId": login_id, "detail": "logging in..." }));

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
    let account = crate::accounts::matrix_account_to_json(&saved, "connecting", false);
    spawn(state.clone(), saved);

    state.events.emit("matrixLoginResult", serde_json::json!({ "loginId": login_id, "success": true, "account": account }));
    Ok(())
}

#[cfg(test)]
mod tests {
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
        send_message(&state, &account_id, &buffer_id, &login.access_token, &sent_body, None, None).await.expect("send_message failed");
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
        send_message(&state, &account_id, &buffer_id, &login.access_token, &original_body, None, None).await.expect("send failed");
        let original_id = poll_for_message(&state, &buffer_id, |m| m.body == original_body, 30).await.expect("original message never arrived").id;
        println!("sent original: {original_id}");

        // --- reply ---
        let reply_body = format!("phase5-reply-{}", model::next_message_id());
        send_message(&state, &account_id, &buffer_id, &login.access_token, &reply_body, Some(&original_id), None).await.expect("reply send failed");
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

        send_message(&state, &account_id, &buffer_id, &login.access_token, "", None, Some(&image_path)).await.expect("media send failed");
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
