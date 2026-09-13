//! Everything this client puts into a room.
//!
//! Messages and their edits, reactions, redactions, stickers, locations,
//! poll votes, pins. All of them are room events with different types, which
//! is why they end at the same two functions.

use super::*;

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

/// Accepting an invitation, which is a join and one thing more.
///
/// `is_direct` on the invitation is how the inviter said "this is a
/// conversation, not a room". It is a hint carried on one event and nothing
/// keeps it: the account's own `m.direct` list is where the fact lives, and
/// the invitee is the one who has to write it - the inviter cannot write to
/// somebody else's account data. Without this, accepting a DM from Element
/// gave an ordinary room here and stayed one.
pub async fn accept_invite(state: &AppState, account_id: &str, room_id: &str) -> Result<()> {
    // Read before the join, because the invitation stops being listed once
    // it has been accepted.
    let direct_with = state
        .runtime
        .matrix_invites(account_id)
        .into_iter()
        .find(|invite| invite["roomId"].as_str() == Some(room_id))
        .filter(|invite| invite["isDirect"].as_bool() == Some(true))
        .and_then(|invite| invite["inviter"].as_str().map(str::to_string));

    join_room(state, account_id, room_id, &[]).await?;

    if let Some(peer) = direct_with {
        directs::record(state, account_id, &peer, room_id).await;
    }
    Ok(())
}

/// Sends one of the account's stickers.
///
/// Its own event type rather than a message with an image in it, which is what
/// makes a sticker a sticker: clients draw it without a filename, without a
/// download button, and inline at its own size. moho has read them since
/// stickers were supported and could send none.
pub async fn send_sticker(state: &AppState, account_id: &str, buffer_id: &str, mxc: &str, body: &str) -> Result<()> {
    let sticker = stickers::Sticker {
        name: body.to_string(),
        pack: String::new(),
        mxc: mxc.to_string(),
        body: body.to_string(),
    };
    send_typed_event(state, account_id, buffer_id, protocol::EVENT_STICKER, stickers::sticker_event(&sticker)).await
}

/// Sends a place.
///
/// A pin rather than live location sharing, which is a different feature with
/// a different set of promises attached - this says "here is somewhere",
/// once, and stops being true about nothing when the window closes.
pub async fn send_location(state: &AppState, account_id: &str, buffer_id: &str, place: &str, label: &str) -> Result<()> {
    let Some((latitude, longitude)) = stickers::parse_place(place) else {
        anyhow::bail!("that does not look like a place - try \"51.5, -0.12\" or a map link");
    };
    send_typed_event(
        state,
        account_id,
        buffer_id,
        protocol::EVENT_ROOM_MESSAGE,
        stickers::location_event(latitude, longitude, label),
    )
    .await
}

/// Asks to be let in, where a room is asked rather than entered.
///
/// A knock room is one whose join rule says "ask first": joining it outright
/// is refused, and the way in is to knock and wait for somebody inside to
/// answer with an invitation. Without this a room like that was simply a dead
/// end here - the join failed with the server's refusal and there was nothing
/// else to try.
///
/// The reason travels with the knock and is what the people inside see, so it
/// is worth writing: "asking to join" with no name attached is what most
/// knocks look like, and most of those are ignored.
pub async fn knock_room(state: &AppState, account_id: &str, room_id_or_alias: &str, via: &[String], reason: &str) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded = url::form_urlencoded::byte_serialize(room_id_or_alias.trim().as_bytes()).collect::<String>();
    let mut url = format!("{base}/_matrix/client/v3/knock/{encoded}");
    // Same routing problem a join by room id has: the id names the room and
    // not who has it.
    for (i, server) in via.iter().filter(|s| !s.trim().is_empty()).enumerate() {
        url.push(if i == 0 { '?' } else { '&' });
        url.push_str("server_name=");
        url.push_str(&url::form_urlencoded::byte_serialize(server.trim().as_bytes()).collect::<String>());
    }
    let body = if reason.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({ "reason": reason.trim() })
    };
    http::post_json(&url, Some(&account.access_token), body).await.context("knocking on that room")?;
    // Deliberately no buffer: a knock is not a join, and putting the room in
    // the list would say you are in somewhere you have only asked about. The
    // answer arrives as an invitation, which the invite list already shows.
    Ok(())
}

/// Reports a message to the people who run the homeserver.
///
/// The other answers to somebody behaving badly act on the person - ignoring
/// them, or leaving. This is the one that reaches whoever can actually do
/// something about it, and it is the only one a server's moderators can act
/// on. It needs no power in the room: reporting is a thing anybody may do,
/// which is the point of it.
pub async fn report_message(state: &AppState, account_id: &str, buffer_id: &str, event_id: &str, reason: &str) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/report/{}",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(event_id.as_bytes()).collect::<String>()
    );
    // The score is a severity from -100 to 0 that no server this client has
    // met does anything with, and a number nobody chose is worse than no
    // number: the reason is what a moderator reads.
    let body = if reason.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({ "reason": reason.trim() })
    };
    http::post_json(&url, Some(&account.access_token), body).await.context("reporting that message")?;
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
pub(super) fn intentional_mentions(state: &AppState, account_id: &str, room_id: &str, body: &str) -> Value {
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
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known Matrix room for this buffer")?;
    put_room_state(state, account_id, &room_id, event_type, "", content).await
}

/// One state event in a room, by type and key.
///
/// The key is the half `set_room_state` above never had: a name or a topic is
/// the room's only one of its kind and needs none, but the events that make a
/// space a space are keyed by the room they are about - one `m.space.child`
/// per child - and without a key every child written would overwrite the last.
pub async fn put_room_state(
    state: &AppState,
    account_id: &str,
    room_id: &str,
    event_type: &str,
    state_key: &str,
    content: serde_json::Value,
) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/state/{event_type}/{}",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(state_key.as_bytes()).collect::<String>()
    );
    http::put_json(&url, &account.access_token, content).await.context("setting room state")?;
    Ok(())
}

/// Puts a room in a space, or takes it out of one.
///
/// A space is an ordinary room whose contents are `m.space.child` state
/// events, one per child, keyed by the child's room id. moho has read those
/// since spaces were supported and never written one, so a space made here
/// stayed empty for ever and one made elsewhere could be looked at and not
/// rearranged.
///
/// Removing sends empty content rather than deleting the event, because Matrix
/// has no delete: an entry with nothing in it is how the protocol spells "no
/// longer a child", and `rooms::space_children` already skips those.
///
/// The child is told about its parent as well, where this account may say so.
/// That is what makes the relationship visible from the room rather than only
/// from the space - and it is allowed to fail: setting `m.space.parent` needs
/// power in the *child*, which somebody adding their own room to somebody
/// else's space will not have.
pub async fn set_space_child(state: &AppState, account_id: &str, space_id: &str, child_room_id: &str, child: bool) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let via = account.user_id.split(':').nth(1).unwrap_or_default().to_string();
    let content = if child {
        // `via` is not decoration: without a server to route through, a client
        // that has never met this room cannot join it from the space listing.
        serde_json::json!({ "via": [via], "suggested": false })
    } else {
        serde_json::json!({})
    };
    put_room_state(state, account_id, space_id, "m.space.child", child_room_id, content).await?;

    let parent = if child {
        serde_json::json!({ "via": [via], "canonical": true })
    } else {
        serde_json::json!({})
    };
    if let Err(e) = put_room_state(state, account_id, child_room_id, "m.space.parent", space_id, parent).await {
        tracing::debug!("matrix[{account_id}]: {child_room_id} keeps no parent for {space_id}: {e:#}");
    }
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

    // And told to the account, so the conversation is a conversation in
    // Element too. `is_direct` on the invitation is a hint to the person
    // being invited; `m.direct` is where the fact is actually kept, and
    // without this a DM opened here was an ordinary room everywhere else.
    directs::record(state, account_id, target_user_id, &room_id).await;

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
        // Before the upload rather than after it. A server states its limit
        // in /config, and finding out a video is too big by watching it
        // upload for two minutes and then fail is what this replaces - the
        // failure was the same either way, but it arrived after the wait and
        // in the server's words rather than in a sentence.
        media::check_upload_size(state, account_id, path).await?;
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
        // Somewhere, rather than something said. Handled here because it is
        // typed in the same box and is a message like any other once sent -
        // and refused rather than posted as text when what follows is not a
        // place, since "/location where are you" said out loud to a room is
        // not what anybody meant.
        if let Some(rest) = body.strip_prefix("/location ").or_else(|| body.strip_prefix("/place ")) {
            let (place, label) = stickers::split_place_and_label(rest)
                .unwrap_or_else(|| (rest.to_string(), String::new()));
            return send_location(state, account_id, buffer_id, &place, &label).await;
        }
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
        // The formatting somebody typed, plus any emoji they picked. The two
        // together, because an emoticon is only an emoticon in the formatted
        // half: `:shortcode:` in the plain body is exactly the fallback a
        // client with no images should show, and replacing it there would
        // leave those clients reading an `<img>` tag as words.
        let typed = markup::to_html(body);
        let emoji = state.runtime.matrix_emoticons(account_id);
        let html = match typed {
            Some(html) => Some(stickers::inline_emoticons(&html, &emoji)),
            // Nothing was typed in markup, but an emoticon still needs a
            // formatted body to live in - so one is made only when there is
            // actually an emoticon to put in it.
            None => {
                let inlined = stickers::inline_emoticons(&stickers::escape_html(body), &emoji);
                (inlined != stickers::escape_html(body)).then_some(inlined)
            }
        };
        if let Some(html) = html {
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

/// Sends a recording as a voice message rather than as a sound file.
///
/// Three additions to what an ordinary audio attachment carries, and all of
/// them are what make a client draw a waveform instead of a file row:
/// `org.matrix.msc3245.voice` as the marker, `org.matrix.msc1767.audio` with
/// the length and the picture of the sound, and the length again in `info`
/// where clients that predate all of this look for it.
///
/// The waveform is converted back on the way out. This program carries one
/// shape internally - Discord's bytes - so the samples are scaled up into the
/// 0-1024 integers Matrix uses, rather than every caller learning two formats.
///
/// Encrypted where the room is, through exactly the same two paths an
/// attachment already uses: a voice message is a file, and there is no reason
/// for it to be the one attachment that leaks.
pub async fn send_voice_message(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    access_token: &str,
    path: &std::path::Path,
    duration_secs: f64,
    waveform: &[u8],
) -> Result<()> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let is_encrypted = state.runtime.is_matrix_room_encrypted(buffer_id);
    let file = path.to_str().context("the recording has an unreadable path")?;

    // What Element calls one, so a room's history reads consistently.
    let body = "Voice message";
    let mut content = if is_encrypted {
        upload_encrypted_media_message(base, access_token, file, body).await?
    } else {
        upload_media_message(base, access_token, file, body).await?
    };

    media::mark_as_voice(&mut content, duration_secs, waveform);

    let txn_id = model::next_message_id();
    let encoded_room_id = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let encoded_txn_id = url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>();
    let (event_type, body_json) = if is_encrypted {
        let session = state.runtime.get_matrix_machine(account_id).context("crypto session not ready yet")?;
        let member_ids = joined_member_ids(base, access_token, &room_id).await?;
        let room_id_ruma = ruma_common::RoomId::parse(&room_id).context("invalid room id")?;
        let encrypted = session
            .share_and_encrypt_content(&account.homeserver_url, access_token, &room_id_ruma, member_ids, protocol::EVENT_ROOM_MESSAGE, content)
            .await
            .context("encrypting the voice message")?;
        (protocol::EVENT_ROOM_ENCRYPTED, encrypted)
    } else {
        (protocol::EVENT_ROOM_MESSAGE, content)
    };
    let url = format!("{base}/_matrix/client/v3/rooms/{encoded_room_id}/send/{event_type}/{encoded_txn_id}");
    http::put_json(&url, access_token, body_json).await.context("sending the voice message")?;
    Ok(())
}

/// The `m.replace` an edit is sent as.
///
/// The replacement carries the sender's formatting the same way a new
/// message does - editing a formatted message should not quietly flatten it
/// - while the outer body keeps the plain "* text" form that clients without
/// edit support fall back to showing.
pub(super) fn edit_content(target_event_id: &str, body: &str) -> serde_json::Value {
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

/// Sends an event of whatever type into a room.
///
/// Unlike `send_room_event` below - which is for the things that are messages
/// without being chat, and always sends `m.room.message` - this carries its
/// own type all the way through, including into the encryption, where the
/// type is part of what gets encrypted. That distinction is not academic: a
/// call event sent as a message would arrive as a blank line in the log
/// rather than as a ringing telephone.
///
/// Encrypted where the room is, for the reason everything else here is: a
/// room that hides what is said in it should not make an exception.
pub async fn send_typed_event(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    event_type: &str,
    content: Value,
) -> Result<()> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let access_token = account.access_token.clone();

    let (sent_type, body_json) = if state.runtime.is_matrix_room_encrypted(buffer_id) {
        let session = state.runtime.get_matrix_machine(account_id).context("crypto session not ready yet")?;
        let member_ids = joined_member_ids(base, &access_token, &room_id).await?;
        let room_id_ruma = ruma_common::RoomId::parse(&room_id).context("invalid room id")?;
        let encrypted = session
            .share_and_encrypt_content(&account.homeserver_url, &access_token, &room_id_ruma, member_ids, event_type, content)
            .await
            .context("encrypting the event")?;
        (protocol::EVENT_ROOM_ENCRYPTED.to_string(), encrypted)
    } else {
        (event_type.to_string(), content)
    };

    let txn_id = model::next_message_id();
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/send/{sent_type}/{}",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(txn_id.as_bytes()).collect::<String>(),
    );
    http::put_json(&url, &access_token, body_json).await.context("sending the event")?;
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

pub(super) async fn redact_event(homeserver_url: &str, access_token: &str, room_id: &str, target_event_id: &str) -> Result<()> {
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

#[cfg(test)]
mod edit_tests {
    use super::*;

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
}
