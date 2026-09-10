//! The one connection an account has, and everything it brings back.
//!
//! Matrix gives a client a single long-poll: `/sync` answers with every room's
//! new events at once and a token to ask again with. So unlike a gateway or a
//! socket per room, the whole account arrives here in batches, and this module
//! is the loop that asks, the retry around it, and the walk over what comes
//! back.

use super::*;

pub(super) const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);

pub(super) const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

pub(super) const SYNC_LONG_POLL_MS: u64 = 30_000;

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
pub(super) async fn run_with_retry(state: &AppState, config: &MatrixAccountConfig, account_id: &str) {
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

pub(super) fn is_auth_error(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("M_UNKNOWN_TOKEN") || s.contains("M_MISSING_TOKEN") || s.contains("HTTP 401")
}

/// Logs in (or re-logs-in) and persists the fresh session - `device_id` is
/// threaded through so a re-login reuses the *same* device rather than
/// minting a fresh one (see auth.rs's module doc for why that matters).
pub(super) async fn ensure_login(state: &AppState, config: &MatrixAccountConfig, account_id: &str) -> Result<(String, String)> {
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
pub(super) async fn run_sync(state: &AppState, config: &MatrixAccountConfig, account_id: &str) -> Result<()> {
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
        let to_device = crypto::receive_sync_changes(&session, &resp).await;
        // A call's media keys arrive this way rather than in the room: they
        // are for the people in the call now, not for anybody who will ever
        // read it back. See calls::handle_key_event.
        for event in &to_device {
            calls::handle_key_event(state, account_id, event);
        }
        // Receiving those changes can itself produce new outgoing
        // requests (e.g. claiming one-time keys to establish a session
        // with a device that just sent us a room key) - send those too
        // before moving on, same as the tutorial's sync() sketch.
        session.process_outgoing_requests(&config.homeserver_url, &access_token).await;

        // A key that has just arrived may open messages already on screen as
        // "unable to decrypt". Here rather than inside the to-device handler
        // because a session can turn up without having been asked for - from
        // a backup restore, an import, or somebody else's device deciding to
        // share - and this is the one place all three have already landed.
        relock::retry_locked(state, &session, account_id).await;

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

pub(super) fn config_dir() -> std::path::PathBuf {
    // The one the daemon actually runs out of, rather than a second guess at
    // it - these disagreed on Windows, where the hardcoded XDG shape put the
    // crypto store somewhere the rest of the daemon was not looking.
    crate::default_data_dir()
}

pub(super) fn sync_url(homeserver_url: &str, since: Option<&str>, presence: &str) -> String {
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
pub(super) async fn bootstrap_joined_rooms(state: &AppState, account_id: &str, own_user_id: &str, homeserver_url: &str, access_token: &str) -> Result<()> {
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
pub(super) async fn register_room(
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

    // The packs this room shares with everybody in it. Keyed by state key
    // because a room may carry several, and named after the room where the
    // pack itself gives no name.
    for event in event_refs.iter().filter(|e| e["type"].as_str() == Some("im.ponies.room_emotes")) {
        state.runtime.set_matrix_sticker_pack(
            account_id,
            &format!("{room_id}|{}", event["state_key"].as_str().unwrap_or("")),
            &info.name,
            &event["content"],
        );
    }

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
pub(super) const SYNC_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

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
pub(super) fn stop_waiting_eventually(state: AppState, buffer_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(SYNC_WAIT_LIMIT).await;
        state.runtime.set_buffer_syncing(&state, &buffer_id, false);
    });
}

pub(super) async fn process_sync_response(state: &AppState, account_id: &str, own_user_id: &str, homeserver_url: &str, access_token: &str, resp: &Value, session: &crypto::CryptoSession) {
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
            // The account's own sticker pack, which travels with it between
            // clients - see backend/matrix/stickers.rs for the two places a
            // pack lives.
            if event["type"].as_str() == Some("im.ponies.user_emotes") {
                state.runtime.set_matrix_sticker_pack(account_id, "", "your stickers", &event["content"]);
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
pub(super) async fn fetch_pending_invites(
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

/// The rail entry id for one Matrix space.
pub(super) fn space_group_id(account_id: &str, room_id: &str) -> String {
    format!("{account_id}|space:{room_id}")
}

/// The room behind a space's rail entry, back out of its group id.
///
/// The client names a space by the group id it was given, which is the only
/// handle it has; the room id inside it is what the protocol wants.
pub fn space_room_id(group_id: &str) -> Option<String> {
    group_id.split_once("|space:").map(|(_, room)| room.to_string())
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
pub(super) async fn register_space(
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

#[cfg(test)]
mod space_id_tests {
    use super::{space_group_id, space_room_id};

    #[test]
    fn a_group_id_carries_its_room_id() {
        let group = space_group_id("matrix:@a:example.org", "!space:example.org");
        assert_eq!(space_room_id(&group).as_deref(), Some("!space:example.org"));
        // A Discord guild's rail entry is not a space and must not answer.
        assert_eq!(space_room_id("discord:123|guild:456"), None);
    }
}
