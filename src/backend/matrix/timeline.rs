//! One event, turned into whatever it means.
//!
//! Everything `/sync` puts in a room's timeline arrives here: a message, an
//! edit of one, a reaction to one, a redaction of one, somebody joining, the
//! room being renamed. Decryption happens at the top of it, because an
//! encrypted event is only a shape until it is opened.

use super::*;

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
pub(super) fn relation_preview(state: &AppState, buffer_id: &str, content: &Value) -> Option<crate::model::ReplyPreview> {
    let thread = protocol::thread_root(content);
    let in_thread = thread.is_some();
    let target = thread.or_else(|| protocol::reply_target(content))?;
    match state.store.get_message(buffer_id, target) {
        Ok(Some(m)) => Some(crate::model::ReplyPreview { id: target.to_string(), from: m.from, body: m.body, thread: in_thread, forwarded: false }),
        _ => Some(crate::model::ReplyPreview { id: target.to_string(), thread: in_thread, ..Default::default() }),
    }
}

pub(super) async fn handle_timeline_event(
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
        // A call is signalled through the room like anything else: the offer,
        // the answer and the network candidates are all events in it.
        && !outer_type.starts_with("m.call.")
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

    // A call: somebody ringing, answering, hanging up, or telling us how to
    // reach them. Handed to the client rather than rendered, because the
    // media is the client's - it has a WebRTC stack and this daemon does not.
    // See calls::handle.
    if effective_type.starts_with("m.call.") {
        calls::handle(state, account_id, buffer_id, own_user_id, &effective_type, sender, &content);
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
pub(super) fn handle_redaction(state: &AppState, buffer_id: &str, target_event: &str) {
    if let Some((target_buffer, msg_id, emoji, is_me)) = state.runtime.take_matrix_reaction_target(target_event) {
        state.runtime.update_reaction(state, &target_buffer, &msg_id, &emoji, is_me, false);
        return;
    }
    state.runtime.delete_message(state, buffer_id, target_event);
}

/// The plaintext, and whether the device that sent it has been verified.
pub(super) async fn decrypt_event(session: &crypto::CryptoSession, event: &Value, room_id: &str) -> Result<(Value, bool)> {
    let room_id = ruma_common::RoomId::parse(room_id).context("invalid room id")?;
    crypto::decrypt_room_event_with_trust(session, event, &room_id).await
}

/// One event from a thread, decrypted if it needs to be, into the store.
/// Deliberately quiet about anything it cannot use: a thread is read whole,
/// and one unreadable reply should not cost the rest of it.
pub(super) async fn store_thread_event(
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
pub(super) async fn store_history_event(
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

/// An event as text, decrypted where it needs to be and where this session
/// can. Shared by the thread list and the pinned list, which both read events
/// the sync loop never handed to a buffer.
pub(super) async fn read_event(session: &Option<std::sync::Arc<crypto::CryptoSession>>, room: Option<&ruma_common::RoomId>, event: &Value) -> Value {
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
