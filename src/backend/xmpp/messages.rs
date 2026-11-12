//! What arrives in `<message>`: a person's words, and everything a client does with them.
//!
//! One path for every copy of a message - the live one, the one a carbon brings from another of this account's
//! devices, and the one the archive replays - so that all three end as the same line in the same conversation, once.
//! The line's id is the one its sender gave it (`origin-id`, XEP-0359), or the stanza's own, and the store ignores an id
//! it already holds; so a catch-up after an outage, or a message seen live and again in the archive, is not shown twice
//! and does not notify twice.
//!
//! What a message *does* besides being said is done here too: a correction replaces the text of the message it names, a
//! retraction takes one away, a reaction set replaces what its sender had, a receipt or a marker says the other side has
//! it, and a typing state says they are writing. Each is accepted only from the person who wrote the message it
//! changes: an edit or a deletion from anybody else is somebody lying.

use super::incoming::{self, ChatState, Marker, Parsed};
use super::xml::Attr;
use super::session::{Archived, Handle};
use super::{ns, roster, xml};
use crate::model::ReplyPreview;
use crate::state::AppState;
use serde_json::json;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::message::{Message, MessageType};

/// How long a "composing" stays on screen without another.
const TYPING_FOR_MS: u64 = 12_000;

/// Where a copy of a message came from.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    /// Arrived on the stream.
    Live,
    /// Copied to this device by the server because another device of the account sent or received it (XEP-0280).
    Carbon,
    /// Replayed from the archive (XEP-0313), by the archive's id and when it says it was sent.
    Archive { id: String, stamp: Option<i64> },
}

/// The element of this name in this namespace among a message's payloads.
fn payload<'a>(m: &'a Message, name: &str, ns: &str) -> Option<&'a Element> {
    m.payloads.iter().find(|p| p.is(name, ns))
}

/// A message found inside a `<forwarded>`, as the stream's own kind of message (the forwarded stanza may not have said
/// its namespace, and then it is the client one).
pub fn forwarded_message(forwarded: &Element) -> Option<(Message, Option<i64>)> {
    let stamp = forwarded.children().find(|c| c.name() == "delay").and_then(|d| d.attr("stamp")).and_then(xml::parse_stamp);
    let inner = forwarded.children().find(|c| c.name() == "message")?;
    let element = if inner.ns() == ns::CLIENT { inner.clone() } else { retag(inner) };
    Message::try_from(element).ok().map(|m| (m, stamp))
}

/// The same element, said to be in the client namespace all the way down.
fn retag(e: &Element) -> Element {
    let mut copy = Element::builder(e.name(), ns::CLIENT);
    for ((_, name), value) in e.attrs() {
        copy = copy.at(name.as_str(), value);
    }
    let mut copy = copy.build();
    for node in e.nodes() {
        match node {
            tokio_xmpp::minidom::Node::Element(child) => {
                copy.append_child(if child.ns() == e.ns() { retag(child) } else { child.clone() });
            }
            tokio_xmpp::minidom::Node::Text(t) => copy.append_text_node(t.clone()),
        }
    }
    copy
}

/// A message that came in on the stream.
pub async fn handle(state: &AppState, handle: &Handle, message: Message) {
    // Somebody this account has blocked: the server should not deliver it, and if it did, it is not shown.
    if message.from.as_ref().is_some_and(|f| super::blocking::is_blocked(handle, f)) {
        return;
    }
    if message.type_ == MessageType::Error {
        return failed(state, handle, &message);
    }
    if invitation(state, handle, &message) {
        return;
    }
    if super::calls::message(state, handle, &message, None) {
        return;
    }
    // An archive's answer to a query in progress: kept for whoever asked.
    if let Some(result) = payload(&message, "result", ns::MAM) {
        if let (Some(query), Some(archive_id), Some(forwarded)) = (result.attr("queryid"), result.attr("id"), xml::child(result, "forwarded", ns::FORWARD)) {
            if let Some((inner, stamp)) = forwarded_message(forwarded) {
                if let Some(bucket) = handle.session().archive.get_mut(query) {
                    bucket.push(Archived { archive_id: archive_id.to_string(), message: inner, stamp });
                }
            }
        }
        return;
    }
    // A carbon: only the account's own server may send one, and it does so from the account's own address. Anything
    // else claiming to be one is somebody trying to put words in the account's mouth.
    for (name, sent) in [("received", false), ("sent", true)] {
        if let Some(carbon) = payload(&message, name, ns::CARBONS) {
            let from_us = message.from.as_ref().is_some_and(|f| f.to_bare() == handle.own());
            if !from_us {
                tracing::warn!("xmpp[{}]: a carbon from {:?}, which is not this account", handle.account_id, message.from);
                return;
            }
            if let Some((inner, _)) = xml::child(carbon, "forwarded", ns::FORWARD).and_then(forwarded_message) {
                if super::calls::message(state, handle, &inner, Some(sent)) {
                    return;
                }
                handle_one(state, handle, inner, Origin::Carbon).await;
            }
            return;
        }
    }
    handle_one(state, handle, message, Origin::Live).await;
}

/// An invitation to a room, direct (XEP-0249) or through the room (XEP-0045). Returns whether the message was one.
fn invitation(state: &AppState, handle: &Handle, message: &Message) -> bool {
    use super::session::RoomInvite;
    let mut found = None;
    if let Some(x) = payload(message, "x", "jabber:x:conference") {
        if let Some(room) = x.attr("jid") {
            found = Some(RoomInvite { room: room.to_string(), from: message.from.as_ref().map(|j| j.to_bare().to_string()).unwrap_or_default(), reason: x.attr("reason").unwrap_or_default().to_string(), password: x.attr("password").map(str::to_string) });
        }
    }
    if let Some(user) = payload(message, "x", super::muc::MUC_USER) {
        if let Some(invite) = user.children().find(|c| c.name() == "invite") {
            let room = message.from.as_ref().map(|j| j.to_bare().to_string()).unwrap_or_default();
            found = Some(RoomInvite { room, from: invite.attr("from").unwrap_or_default().to_string(), reason: invite.children().find(|c| c.name() == "reason").map(|r| r.text()).unwrap_or_default(), password: user.children().find(|c| c.name() == "password").map(|p| p.text()) });
        }
    }
    let Some(invite) = found else { return false };
    if invite.room.is_empty() {
        return true;
    }
    {
        let mut s = handle.session();
        if s.room_invites.iter().any(|i| i.room == invite.room) {
            return true;
        }
        s.room_invites.push(invite);
    }
    announce_invites(state, handle);
    true
}

/// What the window's Invitations tile lists for rooms.
pub fn invites_json(handle: &Handle) -> Vec<serde_json::Value> {
    handle.session().room_invites.iter().map(|i| json!({ "roomId": i.room, "name": i.room, "inviter": i.from, "avatarUrl": serde_json::Value::Null, "isDirect": false, "reason": i.reason })).collect()
}

pub fn announce_invites(state: &AppState, handle: &Handle) {
    state.events.emit("xmppInvites", json!({ "accountId": handle.account_id, "invites": invites_json(handle) }));
}

/// An error answering a message we sent: said in the conversation, so it is not lost.
fn failed(state: &AppState, handle: &Handle, message: &Message) {
    let Some(peer) = message.from.as_ref().map(|j| j.to_bare()) else { return };
    let Some(buffer) = roster::find_buffer(state, handle, &peer) else { return };
    let why = message.payloads.iter().find(|p| p.name() == "error").map(|e| e.children().map(|c| c.name().replace('-', " ")).next().unwrap_or_else(|| "an error".into())).unwrap_or_else(|| "an error".into());
    state.runtime.record_message_at(state, &handle.account_id, &buffer.name, "dm", "XMPP", &format!("A message to {peer} was not delivered: {why}"), false, "system", None, None, false, None, Vec::new(), Vec::new(), None, None, None, None);
}

/// Which conversation a message belongs to and who is speaking in it, worked out by whoever received it (a person's
/// address for a one-to-one chat, a room's for a group chat) so that everything that comes after is the same.
pub struct Conv {
    pub buffer: crate::model::Buffer,
    /// `dm` or `channel`.
    pub kind: &'static str,
    /// What the speaker is called in this conversation.
    pub from_name: String,
    /// Who the speaker is, in a form that stays the same for the same person: their address, or the room's occupant id.
    pub sender_id: String,
    pub is_own: bool,
    /// The person or room, as an address: what typing, receipts and the like are keyed by.
    pub key: String,
    /// A mention of this account by name in the message, which the room decides (its nickname there).
    pub mentioned: bool,
    /// The speaker's own address, when it is known: what an encrypted message's keys are found by. A room that hides addresses
    /// does not say.
    pub real_jid: Option<String>,
}

/// One message, from wherever. `Archive` copies are replays: they are never answered with a receipt and never notify.
pub async fn handle_one(state: &AppState, handle: &Handle, message: Message, origin: Origin) {
    let own = handle.own();
    let Some(from) = message.from.clone().map(|j| j.to_bare()) else { return };
    // A message sent by this account, from here or another device, is in the conversation with whoever it was sent to.
    let is_own = from == own;
    let peer = if is_own {
        match message.to.as_ref().map(|j| j.to_bare()) {
            Some(to) => to,
            None => return,
        }
    } else {
        from.clone()
    };
    // Whatever a room says, whatever its type, is the room's: a conversation with a room is never a conversation with a person.
    if message.type_ == MessageType::Groupchat || (message.from.as_ref().is_some_and(|f| f.resource().is_none()) && super::muc::is_room(handle, &peer)) {
        tracing::debug!("xmpp[{}]: a message of type {:?} from room {peer}", handle.account_id, message.type_);
        return super::muc::handle_groupchat(state, handle, message, origin).await;
    }
    if !incoming::is_conversational(&message.type_) && message.type_ != MessageType::Headline {
        return;
    }
    let p = incoming::parse(&message);
    // A message addressed to a room's occupant is a private message in the room, a conversation of its own.
    if let Some(room_conv) = super::muc::private_conversation(state, handle, &message, is_own) {
        return deliver(state, handle, &room_conv, p, origin).await;
    }
    let buffer = roster::ensure_buffer(state, handle, &peer);
    let name = if is_own { own.to_string() } else { handle.session().display_name(&from) };
    let conv = Conv { buffer, kind: "dm", from_name: name, sender_id: from.to_string(), is_own, key: peer.to_string(), mentioned: false, real_jid: Some(from.to_string()) };
    deliver(state, handle, &conv, p, origin).await;
}

/// What a parsed message does in its conversation: says something, changes something said, or says how things stand.
pub async fn deliver(state: &AppState, handle: &Handle, conv: &Conv, mut p: Parsed, origin: Origin) {
    // What this resource sent into a room it encrypted for is not read again from the room's copy: that copy has no key for it.
    if conv.is_own && conv.kind == "channel" {
        if let Some(id) = incoming::local_id(&p) {
            if handle.session().sent.contains(&id) {
                return;
            }
        }
    }
    decrypt_in_place(state, handle, conv, &mut p).await;
    let replay = matches!(origin, Origin::Archive { .. });
    let sent_at = match &origin {
        Origin::Archive { stamp, .. } => p.sent_at.or(*stamp),
        _ => p.sent_at,
    };
    // A message the server delivered late (a delay stamp on a live stanza) is history, not news.
    let replay = replay || (origin == Origin::Live && p.sent_at.is_some() && conv.kind == "channel");
    let buffer = &conv.buffer;

    // What the message says about other messages, whether or not it has words of its own.
    if let Some((target, set)) = &p.reactions {
        return reacted(state, handle, buffer, &conv.sender_id, target, set, conv.is_own);
    }
    if let Some(id) = &p.retracts {
        return retracted(state, handle, buffer, &conv.sender_id, id);
    }
    if !replay && !conv.is_own {
        if let Some(state_) = p.chat_state {
            if p.body.is_none() {
                return typing(state, handle, buffer, &conv.from_name, state_);
            }
        }
        if let Some(id) = &p.receipt_for {
            tracing::debug!("xmpp[{}]: {} has message {id}", handle.account_id, conv.key);
        }
        if let Some(Marker::Displayed(id)) = &p.marker {
            tracing::debug!("xmpp[{}]: {} has read up to {id}", handle.account_id, conv.key);
        }
    }
    let Some(raw) = p.body.clone().or_else(|| p.encrypted.as_ref().map(|_| String::new())) else { return };

    let local = incoming::local_id(&p).or_else(|| match &origin {
        Origin::Archive { id, .. } => Some(id.clone()),
        _ => None,
    });
    let local = local.unwrap_or_else(xml::new_id);
    // A copy of something this resource sent is already in the conversation, under the id it was sent with.
    if conv.is_own && conv.kind == "dm" && handle.session().sent.contains(&local) {
        return;
    }
    // Other names for it, so a later correction or reaction that points at one of them finds it.
    {
        let mut s = handle.session();
        if let Some(id) = &p.id {
            s.aliases.insert(id.clone(), local.clone());
        }
        for sid in &p.stanza_ids {
            s.aliases.insert(sid.id.clone(), local.clone());
            s.archive_ids.entry(local.clone()).or_insert((sid.by.clone(), sid.id.clone()));
        }
        if let Origin::Archive { id, .. } = &origin {
            s.aliases.insert(id.clone(), local.clone());
            // The archive's id is the result's, which not every server also writes on the message: it is stamped by the account's
            // own archive for a person, and by the room for a room.
            let by = if conv.kind == "channel" { conv.key.clone() } else { handle.own().to_string() };
            s.archive_ids.entry(local.clone()).or_insert((by, id.clone()));
        }
    }

    // A correction: the same sender, the same conversation, a new text.
    if let Some(target) = &p.replaces {
        let target = handle.session().aliases.get(target).cloned().unwrap_or_else(|| target.clone());
        let theirs = state.store.get_message(&buffer.id, &target).ok().flatten();
        let allowed = theirs.as_ref().is_some_and(|m| m.sender_id.as_deref() == Some(conv.sender_id.as_str()));
        if allowed {
            let text = readable(&p, &raw);
            state.runtime.update_message(state, &buffer.id, &target, &text, &[], &[]);
            // What the correction was itself called is also a name for the message.
            handle.session().aliases.insert(local, target);
        } else {
            tracing::debug!("xmpp[{}]: a correction from {} of a message that is not theirs, or not here", handle.account_id, conv.sender_id);
        }
        return;
    }

    let mut text = readable(&p, &raw);
    // An encrypted file arrives as a link whose fragment is its key: the whole of the (decrypted) message.
    if let Some((url, ..)) = super::omemo::parse_aesgcm(text.trim()).filter(|_| p.files.is_empty()) {
        let name = url.split('#').next().and_then(|u| u.rsplit('/').next()).map(str::to_string);
        p.files.push(incoming::FileRef { url: text.trim().to_string(), media_type: name.as_deref().map(|n| super::upload::content_type_of(n).to_string()).filter(|t| t != "application/octet-stream"), name, ..Default::default() });
        p.oob.clear();
    }
    // Files the sender shared: the stated description when there is one, else the address they sent as a link (XEP-0066).
    let mut attachments: Vec<crate::model::Attachment> = p.files.iter().map(|f| super::media::attachment(&handle.account_id, f)).collect();
    for (url, desc) in &p.oob {
        if url.starts_with("https://") && !attachments.iter().any(|a| a.url.as_deref() == Some(url.as_str())) {
            let f = incoming::FileRef { url: url.clone(), description: desc.clone(), name: url.rsplit('/').next().map(str::to_string), ..Default::default() };
            attachments.push(super::media::attachment(&handle.account_id, &f));
        }
    }
    // The address that was the whole of the message is not shown twice.
    let shared: Vec<&str> = attachments.iter().filter_map(|a| a.url.as_deref()).collect();
    if shared.iter().any(|u| text.trim() == *u) {
        text = p.files.first().and_then(|f| f.description.clone()).or_else(|| p.oob.first().and_then(|(_, d)| d.clone())).unwrap_or_default();
    }
    if text.trim().is_empty() && attachments.is_empty() {
        return;
    }
    let reply = p.reply.as_ref().map(|r| {
        let id = handle.session().aliases.get(&r.id).cloned().unwrap_or_else(|| r.id.clone());
        let target = state.store.get_message(&buffer.id, &id).ok().flatten();
        ReplyPreview { id, from: target.as_ref().map(|m| m.from.clone()).unwrap_or_default(), body: target.map(|m| m.body).unwrap_or_default(), ..Default::default() }
    });
    if replay {
        state.runtime.set_replaying(&buffer.id, true);
    }
    let embeds = link_cards(&p.links, &text);
    // What a server or a component says (an address with no user part: an announcement, a gateway's notice) is a card, not
    // somebody speaking.
    let line_kind = if conv.kind == "dm" && !conv.is_own && !conv.key.contains('@') { "system" } else { "chat" };
    let (is_action, text) = action(&text);
    let recorded = state.runtime.record_message_at(state, &handle.account_id, &buffer.name, conv.kind, &conv.from_name, &text, is_action, line_kind, reply, Some(local.clone()), conv.mentioned, None, embeds, attachments, Some(conv.sender_id.clone()), sent_at, None, None);
    if replay {
        state.runtime.set_replaying(&buffer.id, false);
    }
    if !recorded {
        return;
    }
    if !conv.is_own {
        if p.marker == Some(Marker::Markable) || p.body.is_some() {
            handle.session().last_incoming.insert(conv.key.clone(), p.id.clone().unwrap_or_else(|| local.clone()));
        }
        // Somebody speaking is no longer typing.
        state.events.emit("typing", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "nicks": [], "expiresInMs": 0 }));
        if conv.kind == "dm" && !replay && origin == Origin::Live && p.wants_receipt {
            if let Ok(peer) = conv.key.parse::<BareJid>() {
                acknowledge(handle, &peer, &p).await;
            }
        }
    }
}

/// An OMEMO message becomes the message it holds. One that cannot be read says so and why, rather than showing nothing or the
/// words every client puts in for clients that cannot read it.
async fn decrypt_in_place(state: &AppState, handle: &Handle, conv: &Conv, p: &mut Parsed) {
    let Some(envelope) = p.omemo.clone() else { return };
    let omemo = handle.session().omemo.clone();
    let result = match &omemo {
        Some(o) => match &conv.real_jid {
            Some(jid) => o.decrypt(jid, &envelope),
            None => Err(super::omemo::Error::NoRealJid),
        },
        None => Err(super::omemo::Error::NoSession),
    };
    match result {
        Ok(done) => {
            p.encrypted = None;
            match done.text {
                Some(text) => p.body = Some(text),
                // A key sent to set a session up: there is nothing to show.
                None => p.body = None,
            }
            state.runtime.set_buffer_encrypted(state, &conv.buffer.id, true);
            if done.seen == super::omemo::store::Seen::Changed {
                state.runtime.record_message_at(state, &handle.account_id, &conv.buffer.name, conv.kind, "*", &format!("The encryption key of a device of {} changed. Messages to it are held until you decide whether to trust it", conv.key), false, "system", None, None, false, None, Vec::new(), Vec::new(), None, None, None, None);
            }
            if done.republish {
                if let Some(o) = omemo {
                    let h = handle.clone();
                    tokio::spawn(async move { super::omemo::publish_device(&h, &o).await });
                }
            }
        }
        Err(e) => {
            tracing::debug!("xmpp[{}]: an encrypted message from {} could not be read: {e}", handle.account_id, conv.key);
            let why = match e {
                super::omemo::Error::NotForUs => "it was not encrypted for this device".to_string(),
                super::omemo::Error::NoSession => "there is no session with the device that sent it yet".to_string(),
                super::omemo::Error::NoRealJid => "this room does not show who sent it".to_string(),
                other => other.to_string(),
            };
            p.body = Some(format!("This message is encrypted with OMEMO and could not be read here: {why}"));
            p.encrypted = None;
        }
    }
}

/// What to show for a message: its body without what is only there for a client that cannot draw a reply, or a note when
/// it is encrypted in a way this client cannot read.
fn readable(p: &Parsed, raw: &str) -> String {
    if let Some(what) = &p.encrypted {
        if raw.trim().is_empty() || p.body.as_deref().is_some_and(|b| b.contains("encrypted")) {
            return format!("This message is encrypted with {what}, which moho cannot read yet");
        }
    }
    let understood = [ns::REPLY, ns::RETRACT, ns::RETRACT_OLD];
    let text = incoming::strip_fallbacks(raw, &p.fallbacks, &understood);
    match &p.spoiler {
        Some(hint) if !text.trim().is_empty() => spoilered(hint, &text),
        _ => text,
    }
}

/// A spoiler as the window draws one: the hint in the open and the words behind a bar (the `||` convention every service here uses).
pub fn spoilered(hint: &str, text: &str) -> String {
    let hidden = format!("||{}||", text.trim().replace("||", "| |"));
    if hint.is_empty() { hidden } else { format!("{hint}: {hidden}") }
}

/// What is sent for a message that is wholly a spoiler: the words, and the marks taken off them.
pub fn unspoilered(body: &str) -> Option<String> {
    let t = body.trim();
    let inner = t.strip_prefix("||")?.strip_suffix("||")?;
    (!inner.is_empty() && !inner.contains("||")).then(|| inner.to_string())
}

/// Tells a person their message arrived, if they asked and they are somebody this account has a relationship with:
/// a receipt to a stranger tells them the account is there.
async fn acknowledge(handle: &Handle, peer: &BareJid, p: &Parsed) {
    let Some(id) = p.id.clone() else { return };
    let known = handle.session().roster.get(peer).is_some_and(|c| c.subscription != "none" || c.ask);
    if !known || !handle.session().answered.insert(id.clone()) {
        return;
    }
    let mut reply = Message::chat(Jid::from(peer.clone()));
    reply.id = Some(tokio_xmpp::parsers::message::Id(xml::new_id()));
    reply.payloads.push(Element::builder("received", ns::RECEIPTS).at("id", id).build());
    reply.payloads.push(xml::empty("store", ns::HINTS));
    handle.send_message(reply).await;
}

fn typing(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, name: &str, s: ChatState) {
    let nicks = match s {
        ChatState::Composing => vec![name.to_string()],
        _ => Vec::new(),
    };
    state.events.emit("typing", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "nicks": nicks, "expiresInMs": if nicks.is_empty() { 0 } else { TYPING_FOR_MS } }));
}

/// A reaction set: what `sender` now has on the message, against what they had.
fn reacted(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, sender: &str, target: &str, now: &[String], is_own: bool) {
    let target = handle.session().aliases.get(target).cloned().unwrap_or_else(|| target.to_string());
    if state.store.get_message(&buffer.id, &target).ok().flatten().is_none() {
        return;
    }
    let key = (buffer.id.clone(), target.clone(), sender.to_string());
    let before = handle.session().reactions.insert(key, now.to_vec()).unwrap_or_default();
    for e in now.iter().filter(|e| !before.contains(e)) {
        state.runtime.update_reaction(state, &buffer.id, &target, e, is_own, true);
    }
    for e in before.iter().filter(|e| !now.contains(e)) {
        state.runtime.update_reaction(state, &buffer.id, &target, e, is_own, false);
    }
}

/// A retraction: the message goes, if it was the retracter's own.
fn retracted(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, sender: &str, id: &str) {
    let target = handle.session().aliases.get(id).cloned().unwrap_or_else(|| id.to_string());
    let theirs = state.store.get_message(&buffer.id, &target).ok().flatten();
    if theirs.is_some_and(|m| m.sender_id.as_deref() == Some(sender)) {
        state.runtime.delete_message(state, &buffer.id, &target);
    } else {
        tracing::debug!("xmpp[{}]: {sender} retracted a message that is not theirs", handle.account_id);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_spoiler_is_drawn_behind_its_bar_with_the_hint_beside_it() {
        assert_eq!(super::spoilered("", "he was the killer"), "||he was the killer||");
        assert_eq!(super::spoilered("film ending", "the ship sinks"), "film ending: ||the ship sinks||");
        assert_eq!(super::spoilered("", "a || b"), "||a | | b||");
    }

    #[test]
    fn a_message_that_is_all_spoiler_is_sent_as_one_and_part_of_one_is_left_alone() {
        assert_eq!(super::unspoilered(" ||the ship sinks|| ").as_deref(), Some("the ship sinks"));
        assert_eq!(super::unspoilered("||a|| and ||b||"), None);
        assert_eq!(super::unspoilered("plain"), None);
        assert_eq!(super::unspoilered("||||"), None);
    }

    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_forwarded_message_is_read_with_the_time_the_archive_gives() {
        let f = Element::from_str("<forwarded xmlns='urn:xmpp:forward:0'><delay xmlns='urn:xmpp:delay' stamp='2026-11-09T12:00:00Z'/><message xmlns='jabber:client' from='bob@example.org/x' to='alice@example.org' type='chat' id='m1'><body>old</body></message></forwarded>").unwrap();
        let (m, stamp) = forwarded_message(&f).expect("a message");
        assert_eq!((m.id.map(|i| i.0), stamp), (Some("m1".to_string()), Some(1794225600)));
        assert_eq!(m.bodies.values().next().map(String::as_str), Some("old"));
    }

    #[test]
    fn a_forwarded_message_that_left_its_namespace_out_is_still_a_client_message() {
        let f = Element::from_str("<forwarded xmlns='urn:xmpp:forward:0'><message from='bob@example.org' to='alice@example.org' type='chat'><body xmlns='urn:xmpp:forward:0'>hi</body></message></forwarded>").unwrap();
        let (m, _) = forwarded_message(&f).expect("a message");
        assert_eq!(m.bodies.values().next().map(String::as_str), Some("hi"));
    }

    #[test]
    fn what_cannot_be_read_says_so_and_a_reply_loses_its_quoted_text() {
        let p = Parsed { body: Some("I sent you an OMEMO encrypted message".into()), encrypted: Some("OMEMO".into()), ..Default::default() };
        assert!(readable(&p, p.body.as_deref().unwrap()).contains("encrypted with OMEMO"));
        let r = Parsed { body: Some("> hi\nyes".into()), fallbacks: vec![incoming::Fallback { for_ns: ns::REPLY.into(), start: 0, end: 5 }], ..Default::default() };
        assert_eq!(readable(&r, "> hi\nyes"), "yes");
    }
}

/// The sender's descriptions of links (XEP-0511) as cards: only for a link that is in the message (a card for an address it
/// does not show would put words in a page's mouth), only http(s), and the picture only when it came inside the message -
/// at most 512 KiB, and a picture by its own first bytes (an SVG is not one). Nothing is fetched.
pub fn link_cards(links: &[incoming::LinkMeta], text: &str) -> Vec<crate::model::Embed> {
    let mut out: Vec<crate::model::Embed> = Vec::new();
    for l in links {
        let scheme_ok = l.about.starts_with("https://") || l.about.starts_with("http://");
        if !scheme_ok || !text.contains(&l.about) || out.iter().any(|e| e.url.as_deref() == Some(l.about.as_str())) {
            continue;
        }
        let image = l.image_data.as_deref().and_then(store_data_image);
        if l.title.is_none() && l.description.is_none() && image.is_none() {
            continue;
        }
        let host = url::Url::parse(&l.about).ok().and_then(|u| u.host_str().map(str::to_string));
        out.push(crate::model::Embed {
            title: l.title.clone(),
            description: l.description.clone(),
            url: Some(l.about.clone()),
            image_url: image,
            // Who serves it, as the address says - not as the sender says (the site name is theirs to make up).
            provider: host.map(|h| match &l.site_name {
                Some(name) if !name.eq_ignore_ascii_case(&h) => format!("{h} · \u{201c}{name}\u{201d}"),
                _ => h,
            }),
            ..Default::default()
        });
    }
    out
}

/// A `data:` picture, decoded and kept in the media cache by its hash. `file://` path, or `None`.
fn store_data_image(uri: &str) -> Option<String> {
    use base64::Engine;
    let (head, data) = uri.strip_prefix("data:")?.split_once(',')?;
    if !head.ends_with(";base64") || data.len() > 700_000 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    if bytes.len() > 512 * 1024 {
        return None;
    }
    let kind = super::avatars::sniff(&bytes)?;
    let ext = kind.rsplit('/').next().unwrap_or("png").replace("jpeg", "jpg");
    let path = crate::media_cache::dir(super::media::MEDIA).join(format!("card-{}.{ext}", crate::backend::xmpp::transport::sha256_hex(&bytes)));
    if !path.exists() {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, &bytes).ok()?;
    }
    Some(format!("file://{}", path.display()))
}

#[cfg(test)]
mod link_card_tests {
    use super::*;

    fn meta(about: &str) -> incoming::LinkMeta {
        incoming::LinkMeta { about: about.into(), title: Some("T".into()), site_name: Some("Totally Your Bank".into()), ..Default::default() }
    }

    #[test]
    fn a_card_is_only_for_a_link_the_message_shows() {
        let cards = link_cards(&[meta("https://example.org/a"), meta("https://elsewhere.example/b"), meta("javascript:alert(1)")], "look https://example.org/a");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].url.as_deref(), Some("https://example.org/a"));
        let provider = cards[0].provider.clone().unwrap_or_default();
        assert!(provider.starts_with("example.org"), "the host comes first, whatever the sender calls it: {provider}");
    }

    #[test]
    fn a_picture_inside_the_message_is_kept_and_anything_else_is_not() {
        assert!(store_data_image("data:image/svg+xml;base64,PHN2Zy8+").is_none(), "an SVG is not a picture here");
        assert!(store_data_image("data:image/png,notbase64").is_none());
        assert!(store_data_image("https://example.org/x.png").is_none());
    }
}

/// XEP-0245: a body that begins `/me ` is an action ("* pat waves"), drawn as one; the words are what follows it.
pub fn action(text: &str) -> (bool, String) {
    match text.strip_prefix("/me ") {
        Some(rest) if !rest.trim().is_empty() => (true, rest.to_string()),
        _ => (false, text.to_string()),
    }
}

#[cfg(test)]
mod action_tests {
    #[test]
    fn a_me_line_is_an_action() {
        assert_eq!(super::action("/me waves"), (true, "waves".to_string()));
        assert_eq!(super::action("/meh"), (false, "/meh".to_string()));
        assert_eq!(super::action("say /me waves"), (false, "say /me waves".to_string()));
        assert_eq!(super::action("/me "), (false, "/me ".to_string()));
    }
}
