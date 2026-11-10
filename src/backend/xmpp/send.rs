//! Saying things: send, edit, retract, react, typing, and telling the other side a conversation was read.
//!
//! What goes out is what the other common clients read: the text as the body, with the extensions riding beside it
//! (a client that does not know one shows the body and ignores the rest, which is what makes them extensions). A
//! message is recorded in the conversation as it is sent, under the id it is sent with, so the copy that the archive or
//! another device returns is recognised and not shown again.

use super::session::Handle;
use super::xml::Attr;
use super::{ns, xml};
use crate::model::Buffer;
use crate::state::AppState;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::message::{Id, Message};

fn target(state: &AppState, buffer: &Buffer) -> Result<(Handle, BareJid), String> {
    let handle = state.runtime.xmpp_handle(&buffer.account_id).ok_or_else(|| "that XMPP account is not connected".to_string())?;
    let jid = buffer.remote_id.as_deref().and_then(|j| j.parse::<BareJid>().ok()).ok_or_else(|| "this is not an XMPP conversation".to_string())?;
    Ok((handle, jid))
}

/// A message to this person with the id it is sent under, and the markers every chat message carries.
fn chat(to: &BareJid, id: &str) -> Message {
    let mut m = Message::chat(Jid::from(to.clone()));
    m.id = Some(Id(id.to_string()));
    m.payloads.push(Element::builder("origin-id", ns::SID).at("id", id).build());
    m
}

/// The quoted text a reply carries for clients that cannot draw replies: `> who` and the lines quoted, then a blank
/// line's worth of separation. Returns the text and how many characters of it are the fallback.
pub fn reply_fallback(from: &str, quoted: &str) -> (String, usize) {
    let mut text = String::new();
    text.push_str("> ");
    text.push_str(from);
    text.push('\n');
    for line in quoted.lines().take(5) {
        text.push_str("> ");
        text.push_str(line);
        text.push('\n');
    }
    let n = text.chars().count();
    (text, n)
}

/// Sends a message.
pub async fn send(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<String>) -> Result<(), String> {
    let (handle, jid) = target(state, buffer)?;
    if body.trim().is_empty() {
        return Err("There is nothing to send".to_string());
    }
    let id = xml::new_id();
    let mut m = chat(&jid, &id);
    let mut preview = None;
    let wire_body = match reply_to.as_deref().filter(|r| !r.is_empty()) {
        Some(target_id) => {
            let quoted = state.store.get_message(&buffer.id, target_id).ok().flatten();
            let (who, said) = quoted.as_ref().map(|q| (q.from.clone(), q.body.clone())).unwrap_or_default();
            // The reply names the message by the id it was sent under, which is the one stored.
            let to = if quoted.as_ref().is_some_and(|q| q.is_own) { handle.bound.to_bare() } else { jid.clone() };
            m.payloads.push(Element::builder("reply", ns::REPLY).at("id", target_id).at("to", to.to_string()).build());
            let (fallback, n) = reply_fallback(&who, &said);
            m.payloads.push(Element::builder("fallback", ns::FALLBACK).at("for", ns::REPLY).append(Element::builder("body", ns::FALLBACK).at("start", "0").at("end", n.to_string()).build()).build());
            preview = Some(crate::model::ReplyPreview { id: target_id.to_string(), from: who, body: said, ..Default::default() });
            format!("{fallback}{body}")
        }
        None => body.to_string(),
    };
    m.bodies.insert(Default::default(), wire_body);
    m.payloads.push(xml::empty("request", ns::RECEIPTS));
    m.payloads.push(xml::empty("markable", ns::MARKERS));
    m.payloads.push(xml::empty("active", ns::CHATSTATES));
    handle.session().sent.insert(id.clone());
    let own = handle.own().to_string();
    state.runtime.record_message_at(state, &buffer.account_id, &buffer.name, "dm", &own, body, false, "chat", preview, Some(id), false, None, Vec::new(), Vec::new(), Some(own.clone()), None, None, None);
    handle.send_message(m).await;
    Ok(())
}

/// Changes the text of a message of this account's (XEP-0308).
pub async fn edit(state: &AppState, buffer: &Buffer, msg_id: &str, body: &str) -> Result<(), String> {
    let (handle, jid) = target(state, buffer)?;
    if body.trim().is_empty() {
        return Err("A message cannot be changed into nothing: delete it instead".to_string());
    }
    let original = state.store.get_message(&buffer.id, msg_id).ok().flatten().ok_or("that message is not here")?;
    if !original.is_own {
        return Err("Only a message you sent can be changed".to_string());
    }
    let id = xml::new_id();
    let mut m = chat(&jid, &id);
    m.bodies.insert(Default::default(), body.to_string());
    m.payloads.push(Element::builder("replace", ns::CORRECT).at("id", msg_id).build());
    m.payloads.push(xml::empty("request", ns::RECEIPTS));
    handle.session().sent.insert(id.clone());
    handle.session().aliases.insert(id, msg_id.to_string());
    state.runtime.update_message(state, &buffer.id, msg_id, body, &[], &[]);
    handle.send_message(m).await;
    Ok(())
}

/// Takes back a message of this account's (XEP-0424), and takes it out of the conversation.
pub async fn delete(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<(), String> {
    let (handle, jid) = target(state, buffer)?;
    let original = state.store.get_message(&buffer.id, msg_id).ok().flatten().ok_or("that message is not here")?;
    if !original.is_own {
        return Err("Only a message you sent can be taken back".to_string());
    }
    let id = xml::new_id();
    let mut m = chat(&jid, &id);
    let note = "This person attempted to retract a previous message, but it's unsupported by your client.";
    m.bodies.insert(Default::default(), note.to_string());
    m.payloads.push(Element::builder("retract", ns::RETRACT).at("id", msg_id).build());
    m.payloads.push(Element::builder("fallback", ns::FALLBACK).at("for", ns::RETRACT).build());
    m.payloads.push(xml::empty("store", ns::HINTS));
    handle.session().sent.insert(id);
    state.runtime.delete_message(state, &buffer.id, msg_id);
    handle.send_message(m).await;
    Ok(())
}

/// Puts this account's reaction on a message, or takes it off (XEP-0444). The stanza carries the whole set the account
/// now has on the message, which is why the set is remembered.
pub async fn react(state: &AppState, buffer: &Buffer, msg_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    let (handle, jid) = target(state, buffer)?;
    if state.store.get_message(&buffer.id, msg_id).ok().flatten().is_none() {
        return Err("that message is not here".to_string());
    }
    let own = handle.own().to_string();
    let key = (buffer.id.clone(), msg_id.to_string(), own);
    let mut set = handle.session().reactions.get(&key).cloned().unwrap_or_default();
    let had = set.iter().any(|e| e == emoji);
    if add && !had {
        set.push(emoji.to_string());
    } else if !add && had {
        set.retain(|e| e != emoji);
    } else {
        return Ok(());
    }
    handle.session().reactions.insert(key, set.clone());
    let mut r = Element::builder("reactions", ns::REACTIONS).at("id", msg_id).build();
    for e in &set {
        r.append_child(Element::builder("reaction", ns::REACTIONS).append(e.clone()).build());
    }
    let mut m = chat(&jid, &xml::new_id());
    m.payloads.push(r);
    m.payloads.push(xml::empty("store", ns::HINTS));
    state.runtime.update_reaction(state, &buffer.id, msg_id, emoji, true, add);
    handle.send_message(m).await;
    Ok(())
}

/// Whether this account may tell this person things that are only for those it talks to (typing, reading): somebody on
/// the roster, not a stranger who wrote first.
fn trusted(handle: &Handle, jid: &BareJid) -> bool {
    handle.session().roster.get(jid).is_some_and(|c| c.subscription != "none" || c.ask)
}

/// Says that this account is writing, or has stopped (XEP-0085).
pub async fn typing(state: &AppState, buffer: &Buffer, composing: bool) -> Result<(), String> {
    let (handle, jid) = target(state, buffer)?;
    if !trusted(&handle, &jid) {
        return Ok(());
    }
    let was = handle.session().typing_to.contains(jid.as_str());
    if composing == was {
        return Ok(());
    }
    if composing {
        handle.session().typing_to.insert(jid.to_string());
    } else {
        handle.session().typing_to.remove(jid.as_str());
    }
    let mut m = Message::chat(Jid::from(jid));
    m.payloads.push(xml::empty(if composing { "composing" } else { "paused" }, ns::CHATSTATES));
    m.payloads.push(xml::empty("no-store", ns::HINTS));
    handle.send_message(m).await;
    Ok(())
}

/// Tells the other side the last message they sent has been read (XEP-0333).
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, jid) = target(state, buffer)?;
    if !trusted(&handle, &jid) {
        return Ok(());
    }
    let Some(last) = handle.session().last_incoming.get(jid.as_str()).cloned() else { return Ok(()) };
    if !handle.session().answered.insert(format!("displayed:{last}")) {
        return Ok(());
    }
    let mut m = Message::chat(Jid::from(jid));
    m.id = Some(Id(xml::new_id()));
    m.payloads.push(Element::builder("displayed", ns::MARKERS).at("id", last).build());
    m.payloads.push(xml::empty("store", ns::HINTS));
    handle.send_message(m).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_quotes_the_message_for_clients_that_cannot_draw_one_and_says_how_much_of_the_body_that_is() {
        let (text, n) = reply_fallback("Bob", "first\nsecond");
        assert_eq!(text, "> Bob\n> first\n> second\n");
        assert_eq!(n, text.chars().count());
    }

    #[test]
    fn a_long_quotation_is_cut_to_a_few_lines() {
        let quoted = (1..=20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (text, _) = reply_fallback("B", &quoted);
        assert_eq!(text.lines().count(), 6);
    }

    #[test]
    fn a_chat_message_carries_the_id_it_is_sent_under_twice_as_xep_0359_asks() {
        let to: BareJid = "bob@example.org".parse().unwrap();
        let m = chat(&to, "moho-1");
        assert_eq!(m.id.as_ref().map(|i| i.0.as_str()), Some("moho-1"));
        assert!(m.payloads.iter().any(|p| p.is("origin-id", ns::SID) && p.attr("id") == Some("moho-1")));
        assert_eq!(m.to.as_ref().map(|j| j.to_string()), Some("bob@example.org".to_string()));
    }
}
