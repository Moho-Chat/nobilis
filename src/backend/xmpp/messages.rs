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
    if message.type_ == MessageType::Error {
        return failed(state, handle, &message);
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
                // The copy of a message this very resource sent is not news.
                let _ = sent;
                handle_one(state, handle, inner, Origin::Carbon).await;
            }
            return;
        }
    }
    handle_one(state, handle, message, Origin::Live).await;
}

/// An error answering a message we sent: said in the conversation, so it is not lost.
fn failed(state: &AppState, handle: &Handle, message: &Message) {
    let Some(peer) = message.from.as_ref().map(|j| j.to_bare()) else { return };
    let Some(buffer) = roster::find_buffer(state, handle, &peer) else { return };
    let why = message.payloads.iter().find(|p| p.name() == "error").map(|e| e.children().map(|c| c.name().replace('-', " ")).next().unwrap_or_else(|| "an error".into())).unwrap_or_else(|| "an error".into());
    state.runtime.record_message_at(state, &handle.account_id, &buffer.name, "dm", "XMPP", &format!("A message to {peer} was not delivered: {why}"), false, "system", None, None, false, None, Vec::new(), Vec::new(), None, None, None, None);
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
    if message.type_ == MessageType::Groupchat {
        return super::muc::handle_groupchat(state, handle, message, origin).await;
    }
    if !incoming::is_conversational(&message.type_) && message.type_ != MessageType::Headline {
        return;
    }
    let p = incoming::parse(&message);
    let replay = matches!(origin, Origin::Archive { .. });
    let sent_at = match &origin {
        Origin::Archive { stamp, .. } => p.sent_at.or(*stamp),
        _ => p.sent_at,
    };

    // What the message says about other messages, whether or not it has words of its own.
    if let Some((target, set)) = &p.reactions {
        return reacted(state, handle, &peer, &from, target, set, is_own);
    }
    if let Some(id) = &p.retracts {
        return retracted(state, handle, &peer, &from, id);
    }
    if !replay && !is_own {
        if let Some(state_) = p.chat_state {
            if p.body.is_none() {
                return typing(state, handle, &peer, state_);
            }
        }
        if let Some(id) = &p.receipt_for {
            tracing::debug!("xmpp[{}]: {peer} has message {id}", handle.account_id);
        }
        if let Some(Marker::Displayed(id)) = &p.marker {
            tracing::debug!("xmpp[{}]: {peer} has read up to {id}", handle.account_id);
        }
    }
    let Some(raw) = p.body.clone().or_else(|| p.encrypted.as_ref().map(|_| String::new())) else { return };

    let local = incoming::local_id(&p).or_else(|| match &origin {
        Origin::Archive { id, .. } => Some(id.clone()),
        _ => None,
    });
    let local = local.unwrap_or_else(xml::new_id);
    // A copy of something this resource sent is already in the conversation, under the id it was sent with.
    if is_own && handle.session().sent.contains(&local) {
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
        }
        if let Origin::Archive { id, .. } = &origin {
            s.aliases.insert(id.clone(), local.clone());
        }
    }

    let buffer = roster::ensure_buffer(state, handle, &peer);
    // A correction: the same sender, the same conversation, a new text.
    if let Some(target) = &p.replaces {
        let target = handle.session().aliases.get(target).cloned().unwrap_or_else(|| target.clone());
        let theirs = state.store.get_message(&buffer.id, &target).ok().flatten();
        let allowed = theirs.as_ref().is_some_and(|m| m.sender_id.as_deref() == Some(from.as_str()));
        if allowed {
            let text = readable(&p, &raw);
            state.runtime.update_message(state, &buffer.id, &target, &text, &[], &[]);
            // What the correction was itself called is also a name for the message.
            handle.session().aliases.insert(local, target);
        } else {
            tracing::debug!("xmpp[{}]: a correction from {from} of a message that is not theirs, or not here", handle.account_id);
        }
        return;
    }

    let text = readable(&p, &raw);
    if text.trim().is_empty() {
        return;
    }
    let reply = p.reply.as_ref().map(|r| {
        let id = handle.session().aliases.get(&r.id).cloned().unwrap_or_else(|| r.id.clone());
        let target = state.store.get_message(&buffer.id, &id).ok().flatten();
        ReplyPreview { id, from: target.as_ref().map(|m| m.from.clone()).unwrap_or_default(), body: target.map(|m| m.body).unwrap_or_default(), ..Default::default() }
    });
    let name = if is_own { own.to_string() } else { handle.session().display_name(&from) };
    let is_dm = true;
    let _ = is_dm;
    if replay {
        state.runtime.set_replaying(&buffer.id, true);
    }
    let recorded = state.runtime.record_message_at(state, &handle.account_id, &buffer.name, "dm", &name, &text, false, "chat", reply, Some(local.clone()), false, None, Vec::new(), Vec::new(), Some(from.to_string()), sent_at, None, None);
    if replay {
        state.runtime.set_replaying(&buffer.id, false);
    }
    if !recorded {
        return;
    }
    if !is_own {
        if p.marker == Some(Marker::Markable) || p.body.is_some() {
            handle.session().last_incoming.insert(peer.to_string(), p.id.clone().unwrap_or_else(|| local.clone()));
        }
        // Somebody speaking is no longer typing.
        state.events.emit("typing", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "nicks": [], "expiresInMs": 0 }));
        if !replay && origin == Origin::Live && p.wants_receipt {
            acknowledge(handle, &peer, &p).await;
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
    incoming::strip_fallbacks(raw, &p.fallbacks, &understood)
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

fn typing(state: &AppState, handle: &Handle, peer: &BareJid, s: ChatState) {
    let Some(buffer) = roster::find_buffer(state, handle, peer) else { return };
    let nicks = match s {
        ChatState::Composing => vec![handle.session().display_name(peer)],
        _ => Vec::new(),
    };
    state.events.emit("typing", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "nicks": nicks, "expiresInMs": if nicks.is_empty() { 0 } else { TYPING_FOR_MS } }));
}

/// A reaction set: what `from` now has on the message, against what they had.
fn reacted(state: &AppState, handle: &Handle, peer: &BareJid, from: &BareJid, target: &str, now: &[String], is_own: bool) {
    let Some(buffer) = roster::find_buffer(state, handle, peer) else { return };
    let target = handle.session().aliases.get(target).cloned().unwrap_or_else(|| target.to_string());
    if state.store.get_message(&buffer.id, &target).ok().flatten().is_none() {
        return;
    }
    let key = (buffer.id.clone(), target.clone(), from.to_string());
    let before = handle.session().reactions.insert(key, now.to_vec()).unwrap_or_default();
    for e in now.iter().filter(|e| !before.contains(e)) {
        state.runtime.update_reaction(state, &buffer.id, &target, e, is_own, true);
    }
    for e in before.iter().filter(|e| !now.contains(e)) {
        state.runtime.update_reaction(state, &buffer.id, &target, e, is_own, false);
    }
}

/// A retraction: the message goes, if it was the retracter's own.
fn retracted(state: &AppState, handle: &Handle, peer: &BareJid, from: &BareJid, id: &str) {
    let Some(buffer) = roster::find_buffer(state, handle, peer) else { return };
    let target = handle.session().aliases.get(id).cloned().unwrap_or_else(|| id.to_string());
    let theirs = state.store.get_message(&buffer.id, &target).ok().flatten();
    if theirs.is_some_and(|m| m.sender_id.as_deref() == Some(from.as_str())) {
        state.runtime.delete_message(state, &buffer.id, &target);
    } else {
        tracing::debug!("xmpp[{}]: {from} retracted a message that is not theirs", handle.account_id);
    }
}

#[cfg(test)]
mod tests {
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
