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

/// Whether this conversation is a room: what is sent in it is `groupchat`, and is recorded when the room says it has it.
fn in_room(buffer: &Buffer) -> bool {
    buffer.kind == "channel"
}

/// The id of a message as the other side must name it: in a room, the room's own stamp on it.
fn named(handle: &Handle, buffer: &Buffer, local: &str) -> String {
    if in_room(buffer) {
        handle.session().room_ids.get(local).cloned().unwrap_or_else(|| local.to_string())
    } else {
        local.to_string()
    }
}

/// A message to this person with the id it is sent under, and the markers every chat message carries.
fn chat(to: &BareJid, id: &str, room: bool) -> Message {
    let mut m = if room { Message::groupchat(Jid::from(to.clone())) } else { Message::chat(Jid::from(to.clone())) };
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

/// Whether this conversation is encrypted.
fn encrypting(handle: &Handle, jid: &BareJid) -> bool {
    handle.session().omemo.as_ref().is_some_and(|o| o.store().state.enabled.contains(jid.as_str()))
}

/// Who an encrypted message to a room is for: everybody in it whose address it shows, and its members who are not there. Only a room
/// that shows addresses works: OMEMO needs to know whose devices to write to.
pub async fn room_recipients(handle: &Handle, room: &BareJid) -> Result<Vec<BareJid>, String> {
    super::muc::refresh_features(handle, room).await;
    let (hidden, mut people): (bool, Vec<BareJid>) = {
        let s = handle.session();
        match s.rooms.get(room) {
            Some(r) => (r.anonymous(), r.occupants.values().filter_map(|o| o.jid.clone()).collect()),
            None => return Err("you are not in that room".to_string()),
        }
    };
    if hidden {
        return Err("This room does not show people's addresses, so it cannot be encrypted: OMEMO needs to know whose devices to write to".to_string());
    }
    for standing in ["owner", "admin", "member"] {
        if let Ok(list) = super::muc_admin::affiliates(handle, room, standing).await {
            people.extend(list.into_iter().filter_map(|(jid, _)| jid.parse::<BareJid>().ok()));
        }
    }
    people.retain(|p| *p != handle.own());
    people.sort();
    people.dedup();
    Ok(people)
}

/// What an encrypted message carries in place of its body: the encrypted element, the note that says what it is for clients
/// that cannot read it, and the words they show. Fails if there is no device of the person to encrypt to.
async fn encrypt_for(handle: &Handle, jid: &BareJid, plaintext: &str, room: bool) -> Result<Vec<Element>, String> {
    let omemo = handle.session().omemo.clone().ok_or("OMEMO is not ready on this account yet")?;
    let mut targets = Vec::new();
    if room {
        for person in room_recipients(handle, jid).await? {
            targets.extend(super::omemo::usable_devices(handle, &omemo, &person, false).await);
        }
        if targets.is_empty() {
            return Err("None of the people in this room have a device that can be encrypted to. Nothing was sent".to_string());
        }
    } else {
        targets = super::omemo::usable_devices(handle, &omemo, jid, false).await;
        if targets.is_empty() {
            return Err(format!("None of {jid}'s devices can be encrypted to (they may use none that speak OMEMO, or have no device you trust). Nothing was sent"));
        }
    }
    targets.extend(super::omemo::usable_devices(handle, &omemo, &handle.own(), true).await);
    let mut out = omemo.encrypt_all(&targets, plaintext, handle.own().as_str()).map_err(|e| e.to_string())?;
    // The note names the namespace most of the recipients read; a client that reads either finds its own element.
    let namespace = if targets.iter().all(|t| t.v2) { super::omemo::v2::NS } else { super::omemo::NS };
    out.push(Element::builder("encryption", ns::EME).at("namespace", namespace).at("name", "OMEMO").build());
    out.push(xml::empty("store", ns::HINTS));
    Ok(out)
}

/// Sends a message.
pub async fn send(state: &AppState, buffer: &Buffer, body: &str, reply_to: Option<String>) -> Result<(), String> {
    let (handle, jid) = target(state, buffer)?;
    if body.trim().is_empty() {
        return Err("There is nothing to send".to_string());
    }
    let id = xml::new_id();
    let mut m = chat(&jid, &id, in_room(buffer));
    let mut preview = None;
    let wire_body = match reply_to.as_deref().filter(|r| !r.is_empty()) {
        Some(target_id) => {
            let quoted = state.store.get_message(&buffer.id, target_id).ok().flatten();
            let (who, said) = quoted.as_ref().map(|q| (q.from.clone(), q.body.clone())).unwrap_or_default();
            // The reply names the message by the id it was sent under, which is the one stored.
            let to = if in_room(buffer) { format!("{jid}/{who}") } else if quoted.as_ref().is_some_and(|q| q.is_own) { handle.bound.to_bare().to_string() } else { jid.to_string() };
            m.payloads.push(Element::builder("reply", ns::REPLY).at("id", named(&handle, buffer, target_id)).at("to", to).build());
            let (fallback, n) = reply_fallback(&who, &said);
            m.payloads.push(Element::builder("fallback", ns::FALLBACK).at("for", ns::REPLY).append(Element::builder("body", ns::FALLBACK).at("start", "0").at("end", n.to_string()).build()).build());
            preview = Some(crate::model::ReplyPreview { id: target_id.to_string(), from: who, body: said, ..Default::default() });
            format!("{fallback}{body}")
        }
        None => body.to_string(),
    };
    // A message that is wholly a spoiler goes as one (XEP-0382): the words, and a mark that says to hide them.
    let spoiler = if reply_to.as_deref().is_none_or(str::is_empty) { super::messages::unspoilered(&wire_body) } else { None };
    let wire_body = match spoiler {
        Some(words) => {
            m.payloads.push(xml::empty("spoiler", ns::SPOILER));
            words
        }
        None => wire_body,
    };
    let encrypted = encrypting(&handle, &jid);
    // Who a message to a room names is said in words and, besides, by reference (XEP-0372), as the clients that draw mentions send it.
    if in_room(buffer) && !encrypted {
        let skip = if reply_to.as_deref().is_some_and(|r| !r.is_empty()) { wire_body.chars().count().saturating_sub(body.chars().count()) } else { 0 };
        m.payloads.extend(super::muc::mention_references(&handle, &jid, &wire_body, skip));
    }
    if encrypted {
        m.payloads.extend(encrypt_for(&handle, &jid, &wire_body, in_room(buffer)).await?);
        m.bodies.insert(Default::default(), super::omemo::FALLBACK_BODY.to_string());
    } else {
        m.bodies.insert(Default::default(), wire_body);
    }
    if !in_room(buffer) {
        m.payloads.push(xml::empty("request", ns::RECEIPTS));
    }
    m.payloads.push(xml::empty("markable", ns::MARKERS));
    m.payloads.push(xml::empty("active", ns::CHATSTATES));
    if in_room(buffer) && !encrypted {
        // The room sends it back to everybody including its sender, and that copy is what is shown: a message the room
        // refused (no voice, a ban) is then not shown as if it had been said.
        handle.send_message(m).await;
        return Ok(());
    }
    // A direct message is shown at once; so is one encrypted into a room, whose copy from the room has no key for this device.
    handle.session().sent.insert(id.clone());
    let own = handle.own().to_string();
    let kind = if in_room(buffer) { "channel" } else { "dm" };
    let (is_action, shown) = super::messages::action(body);
    state.runtime.record_message_at(state, &buffer.account_id, &buffer.name, kind, &own, &shown, is_action, "chat", preview, Some(id), false, None, Vec::new(), Vec::new(), Some(own.clone()), None, None, None);
    handle.send_message(m).await;
    Ok(())
}

/// Sends files: each goes to the server's upload service and is then sent as a message holding its address. The caption, if
/// there is one, is a message of its own before them, as the other clients do it.
/// `voice`: the files are one recording made in the window, this long (seconds) - sent as an audio file that says its length,
/// which is how XMPP clients send voice messages, and shown here as a voice message.
pub async fn send_files(state: &AppState, buffer: &Buffer, caption: &str, files: &[(String, bool)], reply_to: Option<&str>, progress: Option<&crate::upload::Progress>, voice: Option<f64>) -> Result<(), String> {
    if files.is_empty() {
        return send(state, buffer, caption, reply_to.map(str::to_string)).await;
    }
    let (handle, jid) = target(state, buffer)?;
    let encrypted = encrypting(&handle, &jid);
    let (service, most) = match super::upload::service(&handle).await {
        Ok(found) => found,
        // A server with no HTTP Upload: a person's file goes to their device directly (XEP-0234), if one takes it.
        Err(no_upload) if !in_room(buffer) && !encrypted && voice.is_none() => {
            if !caption.trim().is_empty() {
                send(state, buffer, caption, reply_to.map(str::to_string)).await?;
            }
            if super::filetransfer::device_for(&handle, &jid).await.is_none() {
                return Err(format!("This server has no file upload ({no_upload}), and no device of {jid} that is online takes files sent directly"));
            }
            for (path, _) in files {
                super::filetransfer::send_later(state, &handle, buffer, path);
            }
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let ca = state.accounts.get_xmpp(&buffer.account_id).map(|c| c.ca_pem).unwrap_or_default();
    let total: u64 = files.iter().filter_map(|(p, _)| std::fs::metadata(p).ok().map(|m| m.len())).sum();
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Preparing, total as usize, "XMPP");
    }
    for (path, _) in files {
        let size = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?.len();
        if most.is_some_and(|m| size > m) {
            return Err(format!("{} is {} MB and this server takes files of at most {} MB", path.rsplit('/').next().unwrap_or(path), size / 1_000_000, most.unwrap_or(0) / 1_000_000));
        }
    }
    if !caption.trim().is_empty() {
        send(state, buffer, caption, reply_to.map(str::to_string)).await?;
    }
    let mut done = 0u64;
    for (path, _) in files {
        let size = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?.len();
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".to_string());
        let content_type = super::upload::content_type_of(&name);
        if let Some(p) = progress {
            p.sent(done, total, "XMPP");
        }
        let dims = if content_type.starts_with("image/") { imagesize::size(path).ok().map(|d| (d.width as u32, d.height as u32)) } else { None };
        let id = xml::new_id();
        let mut m = chat(&jid, &id, in_room(buffer));
        // What others are told the file is: its address (an encrypted file's carries the key, and is itself encrypted).
        let link = if encrypted {
            // The server sees only ciphertext, of a kind it cannot name.
            let plain = tokio::fs::read(path).await.map_err(|e| format!("{path}: {e}"))?;
            let (cipher, key, iv) = super::omemo::encrypt_file(&plain).map_err(|e| e.to_string())?;
            let slot = super::upload::request_slot(&handle, &service, &name, cipher.len() as u64, "application/octet-stream").await?;
            super::upload::put_bytes(&buffer.account_id, &ca, &slot, cipher, "application/octet-stream").await?;
            let link = super::omemo::aesgcm_url(&slot.get, &key, &iv);
            m.payloads.extend(encrypt_for(&handle, &jid, &link, in_room(buffer)).await?);
            m.bodies.insert(Default::default(), super::omemo::FALLBACK_BODY.to_string());
            link
        } else {
            let slot = super::upload::request_slot(&handle, &service, &name, size, content_type).await?;
            super::upload::put(&buffer.account_id, &ca, &slot, path, content_type, size).await?;
            m.bodies.insert(Default::default(), slot.get.clone());
            m.payloads.extend(super::upload::file_payloads(&slot.get, &name, content_type, size, dims, None, voice));
            slot.get
        };
        done += size;
        let slot = super::upload::Slot { put: String::new(), get: link, headers: Vec::new() };
        if !in_room(buffer) {
            m.payloads.push(xml::empty("request", ns::RECEIPTS));
        }
        m.payloads.push(xml::empty("markable", ns::MARKERS));
        if !in_room(buffer) || encrypted {
            handle.session().sent.insert(id.clone());
            let own = handle.own().to_string();
            let attachment = crate::model::Attachment {
                kind: super::media::kind_of(Some(content_type), Some(&name)).to_string(),
                mimetype: Some(content_type.to_string()),
                filename: Some(name.clone()),
                size: Some(size),
                width: dims.map(|d| d.0),
                height: dims.map(|d| d.1),
                blurhash: None,
                duration_secs: voice,
                waveform: None,
                voice: voice.is_some(),
                path: Some(format!("file://{path}")),
                thumbnail_path: None,
                url: Some(slot.get.clone()),
            };
            let kind = if in_room(buffer) { "channel" } else { "dm" };
            state.runtime.record_message_at(state, &buffer.account_id, &buffer.name, kind, &own, "", false, "chat", None, Some(id), false, None, Vec::new(), vec![attachment], Some(own.clone()), None, None, None);
        }
        handle.send_message(m).await;
    }
    if let Some(p) = progress {
        p.at(crate::upload::Phase::Waiting, total as usize, "XMPP");
    }
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
    let mut m = chat(&jid, &id, in_room(buffer));
    let encrypted = encrypting(&handle, &jid);
    if encrypted {
        m.payloads.extend(encrypt_for(&handle, &jid, body, in_room(buffer)).await?);
        m.bodies.insert(Default::default(), super::omemo::FALLBACK_BODY.to_string());
    } else {
        m.bodies.insert(Default::default(), body.to_string());
    }
    m.payloads.push(Element::builder("replace", ns::CORRECT).at("id", msg_id).build());
    handle.session().sent.insert(id.clone());
    handle.session().aliases.insert(id, msg_id.to_string());
    // In a room the correction is shown when the room says it, like the message was (an encrypted room's copy has no key for this device).
    if !in_room(buffer) || encrypted {
        state.runtime.update_message(state, &buffer.id, msg_id, body, &[], &[]);
    }
    handle.send_message(m).await;
    Ok(())
}

/// Takes back a message of this account's (XEP-0424), and takes it out of the conversation.
pub async fn delete(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<(), String> {
    let (handle, jid) = target(state, buffer)?;
    let original = state.store.get_message(&buffer.id, msg_id).ok().flatten().ok_or("that message is not here")?;
    if !original.is_own {
        // In a room a moderator may remove what somebody else said; anywhere else there is no such thing.
        if in_room(buffer) {
            let stanza_id = handle.session().room_ids.get(msg_id).cloned().ok_or("the room has not said what it calls that message")?;
            return super::muc_admin::moderate(&handle, &jid, &stanza_id, "").await;
        }
        return Err("Only a message you sent can be taken back".to_string());
    }
    let id = xml::new_id();
    let mut m = chat(&jid, &id, in_room(buffer));
    let note = "This person attempted to retract a previous message, but it's unsupported by your client.";
    m.bodies.insert(Default::default(), note.to_string());
    m.payloads.push(Element::builder("retract", ns::RETRACT).at("id", named(&handle, buffer, msg_id)).build());
    m.payloads.push(Element::builder("fallback", ns::FALLBACK).at("for", ns::RETRACT).build());
    m.payloads.push(xml::empty("store", ns::HINTS));
    handle.session().sent.insert(id);
    if !in_room(buffer) {
        state.runtime.delete_message(state, &buffer.id, msg_id);
    }
    handle.send_message(m).await;
    if in_room(buffer) {
        // A room does not send a retraction back to its sender in every implementation: it is taken out here as well.
        state.runtime.delete_message(state, &buffer.id, msg_id);
    }
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
    let mut r = Element::builder("reactions", ns::REACTIONS).at("id", named(&handle, buffer, msg_id)).build();
    for e in &set {
        r.append_child(Element::builder("reaction", ns::REACTIONS).append(e.clone()).build());
    }
    let mut m = chat(&jid, &xml::new_id(), in_room(buffer));
    m.payloads.push(r);
    m.payloads.push(xml::empty("store", ns::HINTS));
    state.runtime.update_reaction(state, &buffer.id, msg_id, emoji, true, add);
    handle.send_message(m).await;
    Ok(())
}

/// Whether this account may tell this person things that are only for those it talks to (typing, reading): somebody on
/// the roster, not a stranger who wrote first.
fn trusted(handle: &Handle, jid: &BareJid) -> bool {
    super::muc::is_room(handle, jid) || handle.session().roster.get(jid).is_some_and(|c| c.subscription != "none" || c.ask)
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
    let mut m = if in_room(buffer) { Message::groupchat(Jid::from(jid)) } else { Message::chat(Jid::from(jid)) };
    m.payloads.push(xml::empty(if composing { "composing" } else { "paused" }, ns::CHATSTATES));
    m.payloads.push(xml::empty("no-store", ns::HINTS));
    handle.send_message(m).await;
    Ok(())
}

/// Tells the other side the last message they sent has been read (XEP-0333).
pub async fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, jid) = target(state, buffer)?;
    // The account's other devices are told, whoever the conversation is with: it is about this account, not about them.
    let stamped = {
        let s = handle.session();
        s.last_incoming.get(jid.as_str()).and_then(|l| s.archive_ids.get(l)).cloned()
    };
    if let Some((by, id)) = stamped {
        let h = handle.clone();
        let conversation = jid.to_string();
        tokio::spawn(async move { super::mds::publish(&h, &conversation, &by, &id).await });
    }
    if !trusted(&handle, &jid) {
        return Ok(());
    }
    let Some(last) = handle.session().last_incoming.get(jid.as_str()).cloned() else { return Ok(()) };
    if !handle.session().answered.insert(format!("displayed:{last}")) {
        return Ok(());
    }
    let mut m = if in_room(buffer) { Message::groupchat(Jid::from(jid)) } else { Message::chat(Jid::from(jid)) };
    m.id = Some(Id(xml::new_id()));
    m.payloads.push(Element::builder("displayed", ns::MARKERS).at("id", named(&handle, buffer, &last)).build());
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
        let m = chat(&to, "moho-1", false);
        assert_eq!(m.id.as_ref().map(|i| i.0.as_str()), Some("moho-1"));
        assert!(m.payloads.iter().any(|p| p.is("origin-id", ns::SID) && p.attr("id") == Some("moho-1")));
        assert_eq!(m.to.as_ref().map(|j| j.to_string()), Some("bob@example.org".to_string()));
    }
}
