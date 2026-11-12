//! Group calls: Multiparty Jingle (XEP-0272, MUJI) and the Call Invites that ring people into one (XEP-0482), as Dino holds them.
//!
//! A group call is a room of its own - a fresh, private, non-anonymous room on a conference service, made for the call - whose
//! occupants say in their presence that they are in the call (`<muji/>`, first `<preparing/>` while they look around, then the
//! contents they will send). The media is a mesh: one ordinary Jingle session between each pair of participants, addressed to
//! their real addresses, started by whoever arrived later towards everybody already there. People are rung with a call invite,
//! in the chat room the call is for or to the person; whoever joins says so (`accept`) and whoever leaves says so (`left`).
//!
//! The window already holds mesh calls for Matrix (`lib/matrixcall.ts`): who is in the call is a member list, each pair is a
//! one-to-one call whose signals name the conference and the person, and joining or leaving is a membership. So the daemon
//! answers those same questions for an XMPP conversation, and each leg goes through the one-to-one translation in `mod.rs`.

use super::super::session::Handle;
use super::super::xml::{self, child, Attr};
use super::super::{muc_admin, ns};
use crate::model::Buffer;
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_xmpp::jid::{BareJid, FullJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::message::{Id, Message, MessageType};
use tokio_xmpp::parsers::presence::{Presence, Type as PresenceType};

pub const MUJI: &str = "urn:xmpp:jingle:muji:0";
pub const CIM: &str = "urn:xmpp:call-invites:0";
/// Dino's older name for the same messages, which it still sends beside them and reads.
pub const CIM_LEGACY: &str = "urn:xmpp:call-message:1";
const MUC: &str = "http://jabber.org/protocol/muc";
const MUC_USER: &str = "http://jabber.org/protocol/muc#user";

/// A call somebody rang this account into, not joined yet.
#[derive(Debug, Clone)]
pub struct Invite {
    /// The call's id, which an answer names.
    pub id: String,
    /// The call's room.
    pub room: BareJid,
    pub from: BareJid,
    /// Where answers go: the chat room (as a group message) or the person.
    pub reply_to: Jid,
    pub groupchat: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Participant {
    pub real: Option<FullJid>,
    pub preparing: bool,
    /// They were in the room before this end was (their presence came before this end's own): this end starts the session with
    /// them, whenever what they send arrives. Those who come later start it with this end.
    pub existing: bool,
}

/// The group call this account is in (one at a time, as calls are).
#[derive(Debug, Clone)]
pub struct Muji {
    pub room: BareJid,
    pub nick: String,
    pub buffer_id: String,
    pub id: String,
    pub reply_to: Jid,
    pub groupchat: bool,
    /// The room said this end is in it.
    pub joined: bool,
    /// This end said what it sends: from here on, arrivals call it.
    pub ready: bool,
    pub participants: BTreeMap<String, Participant>,
}

impl Muji {
    /// The conference id the window names the call by (`lib/matrixcall.ts`: `moho-conf-` and the room).
    pub fn conf_id(&self) -> String {
        format!("moho-conf-{}", self.room)
    }
}

/// How the window names one participant: the person and the device, as for Matrix (`memberKey`).
pub fn member_key(full: &FullJid) -> String {
    format!("{}|{}", full.to_bare(), full.resource())
}

pub fn parse_member_key(key: &str) -> Option<FullJid> {
    let (bare, resource) = key.split_once('|')?;
    format!("{bare}/{resource}").parse().ok()
}

fn random_hex() -> String {
    format!("{:08x}", rand::random::<u32>())
}

/// Whether a presence is from the call's room.
pub fn is_call_room(handle: &Handle, room: &BareJid) -> bool {
    handle.session().muji.as_ref().is_some_and(|m| m.room == *room)
}

/// The members the window is told about: everybody in the call who has finished looking around, with which of them this end
/// calls.
fn members_json(m: &Muji) -> Value {
    json!(m
        .participants
        .values()
        .filter(|p| !p.preparing)
        .filter_map(|p| p.real.as_ref().map(|r| (r, p.existing)))
        .map(|(r, offer)| json!({ "user_id": r.to_bare().to_string(), "membership": { "device_id": r.resource().to_string() }, "transports": ["moho.mesh"], "offer": offer }))
        .collect::<Vec<_>>())
}

/// Tells the window who is in a conversation's call.
pub fn announce(state: &AppState, buffer_id: &str, members: Value) {
    state.events.emit("matrixCallMembers", json!({ "bufferId": buffer_id, "members": members }));
}

fn announce_current(state: &AppState, handle: &Handle) {
    let current = handle.session().muji.as_ref().map(|m| (m.buffer_id.clone(), members_json(m)));
    if let Some((buffer_id, members)) = current {
        announce(state, &buffer_id, members);
    }
}

/// Who is in a conversation's call, as far as this end knows: the call it is in, or the person who rang it.
pub fn members(handle: &Handle, buffer_id: &str) -> Value {
    let s = handle.session();
    if let Some(m) = s.muji.as_ref().filter(|m| m.buffer_id == buffer_id) {
        return members_json(m);
    }
    match s.call_invites.get(buffer_id) {
        Some(i) => json!([{ "user_id": i.from.to_string(), "membership": { "device_id": "" }, "transports": ["moho.mesh"], "offer": false }]),
        None => json!([]),
    }
}

/// The contents a participant says it sends: audio and video, with the codecs a browser always has (Dino builds its offer from
/// what everybody lists).
fn contents() -> Element {
    let mut muji = Element::builder("muji", MUJI).build();
    for (media, id, name, clock, channels) in [("audio", "111", "opus", "48000", Some("2")), ("video", "96", "VP8", "90000", None)] {
        let mut pt = Element::builder("payload-type", super::jingle::RTP).at("id", id).at("name", name).at("clockrate", clock);
        if let Some(c) = channels {
            pt = pt.at("channels", c);
        }
        let description = Element::builder("description", super::jingle::RTP).at("media", media).append(pt.build()).build();
        muji.append_child(Element::builder("content", super::jingle::JINGLE).at("name", media).append(description).build());
    }
    muji
}

async fn presence_to_room(handle: &Handle, room: &BareJid, nick: &str, muji: Element, first: bool) {
    let Ok(to) = format!("{room}/{nick}").parse::<Jid>() else { return };
    let mut p = Presence::new(PresenceType::None).with_to(to);
    if first {
        p.payloads.push(Element::builder("x", MUC).append(Element::builder("history", MUC).at("maxstanzas", "0").build()).build());
    }
    p.payloads.push(muji);
    p.payloads.push(super::super::caps::presence_element());
    handle.send_presence(p).await;
}

/// A call-invites message, with Dino's older copy beside it and the hint that keeps it.
fn cim_message(to: Jid, groupchat: bool, tag: &str, id: &str, inner: Option<Element>, video: bool) -> Message {
    let mut m = if groupchat { Message::groupchat(to) } else { Message::chat(to) };
    m.id = Some(Id(xml::new_id()));
    let legacy_tag = if tag == "invite" { "propose" } else if tag == "left" { "finish" } else { tag };
    for (ns_, t) in [(CIM, tag), (CIM_LEGACY, legacy_tag)] {
        let mut e = Element::builder(t, ns_).at("id", id);
        if tag == "invite" {
            e = e.at("video", if video { "true" } else { "false" }).at("multi", "true");
        }
        let mut e = e.build();
        if let Some(i) = &inner {
            e.append_child(i.clone());
        }
        m.payloads.push(e);
    }
    m.payloads.push(xml::empty("store", ns::HINTS));
    m
}

fn muji_room(room: &BareJid) -> Element {
    Element::builder("muji", MUJI).at("room", room.to_string()).build()
}

/// The conference service a call's room is made on: the chat room's own, or the first the account's server offers.
async fn conference_service(handle: &Handle, buffer: &Buffer) -> Result<String, String> {
    if buffer.kind == "channel" {
        if let Some(room) = buffer.remote_id.as_deref().and_then(|r| r.parse::<BareJid>().ok()) {
            return Ok(room.domain().to_string());
        }
    }
    let server: Jid = handle.own().domain().as_str().parse().map_err(|_| "the server's address is not usable".to_string())?;
    let items = handle.request(Some(server), false, xml::empty("query", ns::DISCO_ITEMS)).await.map_err(|e| e.words())?;
    for service in items.into_iter().flat_map(|q| q.children().filter_map(|c| c.attr("jid").map(str::to_string)).collect::<Vec<_>>()) {
        let Ok(jid) = service.parse::<Jid>() else { continue };
        let info = handle.request(Some(jid), false, xml::empty("query", ns::DISCO_INFO)).await.ok().flatten();
        if info.is_some_and(|q| q.children().any(|c| c.name() == "identity" && c.attr("category") == Some("conference") && c.attr("type") == Some("text"))) {
            return Ok(service);
        }
    }
    Err("the server offers no group chat service to hold a group call on".to_string())
}

async fn until(handle: &Handle, ms: u64, test: impl Fn(&Muji) -> bool) -> bool {
    let end = tokio::time::Instant::now() + Duration::from_millis(ms);
    while tokio::time::Instant::now() < end {
        if handle.session().muji.as_ref().is_some_and(&test) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Joins the call of a conversation - the one somebody rang it into, or a new one, which rings the others. Returns who this end
/// is in it and the call's room, as the window's membership wants them.
pub async fn join(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<Value, String> {
    if handle.session().muji.is_some() {
        return Err("already in a group call".to_string());
    }
    let invite = handle.session().call_invites.get(&buffer.id).cloned();
    let (room, id, reply_to, groupchat, created) = match &invite {
        Some(i) => (i.room.clone(), i.id.clone(), i.reply_to.clone(), i.groupchat, false),
        None => {
            let service = conference_service(handle, buffer).await?;
            let room: BareJid = format!("{}@{service}", random_hex()).parse().map_err(|_| "the group chat service's address is not usable".to_string())?;
            let remote = buffer.remote_id.clone().ok_or("this conversation has nobody to call")?;
            let reply_to: Jid = remote.parse().map_err(|_| "not an address".to_string())?;
            (room, super::super::login::new_uuid(), reply_to, buffer.kind == "channel", true)
        }
    };
    let nick = random_hex();
    handle.session().muji = Some(Muji { room: room.clone(), nick: nick.clone(), buffer_id: buffer.id.clone(), id: id.clone(), reply_to: reply_to.clone(), groupchat, joined: false, ready: false, participants: BTreeMap::new() });
    let mut preparing = Element::builder("muji", MUJI).build();
    preparing.append_child(xml::empty("preparing", MUJI));
    presence_to_room(handle, &room, &nick, preparing, true).await;
    if !until(handle, 10_000, |m| m.joined).await {
        handle.session().muji = None;
        return Err("the call's room did not let this account in".to_string());
    }
    if created {
        // Private, and showing who is who (a session goes to a participant's real address): Dino's settings.
        if let Ok(form) = muc_admin::config(handle, &room).await {
            let answers = json!({ "muc#roomconfig_whois": "anyone", "muc#roomconfig_membersonly": true, "muc#roomconfig_persistentroom": false, "muc#roomconfig_allowinvites": true });
            if let Err(e) = muc_admin::set_config(handle, &room, &form, &answers).await {
                tracing::warn!("xmpp[{}]: the call's room could not be set up: {e}", handle.account_id);
            }
        }
        // Everybody who is rung may come in.
        let invited: Vec<BareJid> = if groupchat {
            let chat_room = reply_to.to_bare();
            super::super::send::room_recipients(handle, &chat_room).await.unwrap_or_default()
        } else {
            vec![reply_to.to_bare()]
        };
        for who in &invited {
            let _ = muc_admin::set_affiliation(handle, &room, who, "owner", "").await;
        }
        handle.send_message(cim_message(reply_to.clone(), groupchat, "invite", &id, Some(muji_room(&room)), true)).await;
    } else {
        handle.send_message(cim_message(reply_to.clone(), groupchat, "accept", &id, Some(muji_room(&room)), false)).await;
        handle.session().call_invites.remove(&buffer.id);
    }
    // Those still looking around are waited for (briefly): they will call this end, or be called, once they say what they send.
    until(handle, 6_000, |m| m.participants.values().all(|p| !p.preparing)).await;
    if let Some(m) = handle.session().muji.as_mut() {
        m.ready = true;
    }
    presence_to_room(handle, &room, &nick, contents(), false).await;
    announce_current(state, handle);
    Ok(json!({ "userId": handle.own().to_string(), "deviceId": handle.bound.resource().map(|r| r.to_string()).unwrap_or_default(), "roomId": room.to_string() }))
}

/// Leaves the conversation's call: out of its room, and the others told.
pub async fn leave(state: &AppState, handle: &Handle, buffer_id: &str) {
    let m = {
        let mut s = handle.session();
        if !s.muji.as_ref().is_some_and(|m| m.buffer_id == buffer_id) {
            return;
        }
        s.call_groups.clear();
        s.muji.take()
    };
    let Some(m) = m else { return };
    if let Ok(to) = format!("{}/{}", m.room, m.nick).parse::<Jid>() {
        handle.send_presence(Presence::new(PresenceType::Unavailable).with_to(to)).await;
    }
    handle.send_message(cim_message(m.reply_to.clone(), m.groupchat, "left", &m.id, None, false)).await;
    announce(state, buffer_id, json!([]));
}

/// A presence from the call's room: who is there, looking around, in, or gone.
pub async fn presence(state: &AppState, handle: &Handle, p: &Presence) {
    let Some(from) = p.from.clone() else { return };
    let Some(nick) = from.resource().map(|r| r.to_string()) else { return };
    let user = p.payloads.iter().find(|e| e.is("x", MUC_USER));
    let is_self = user.is_some_and(|u| u.children().any(|c| c.is("status", MUC_USER) && c.attr("code") == Some("110")));
    let real = user.and_then(|u| child(u, "item", MUC_USER)).and_then(|i| i.attr("jid")).and_then(|j| j.parse::<FullJid>().ok());
    let muji = p.payloads.iter().find(|e| e.is("muji", MUJI));
    let changed = {
        let mut s = handle.session();
        let Some(m) = s.muji.as_mut() else { return };
        if p.type_ == PresenceType::Error {
            m.joined = false;
            return;
        }
        if is_self || nick == m.nick {
            if p.type_ != PresenceType::Unavailable {
                m.joined = true;
            }
            return;
        }
        match (p.type_ == PresenceType::Unavailable, muji) {
            // Gone, or no longer in the call.
            (true, _) | (false, None) => m.participants.remove(&nick).is_some(),
            (false, Some(muji)) => {
                let preparing = muji.children().any(|c| c.is("preparing", MUJI));
                let joined = m.joined;
                let entry = m.participants.entry(nick.clone()).or_insert_with(|| Participant { existing: !joined, ..Default::default() });
                let was = (entry.preparing, entry.real.clone());
                entry.preparing = preparing;
                entry.real = real.or(entry.real.take());
                // Somebody who arrives after this end said what it sends calls this end.
                was != (entry.preparing, entry.real.clone())
            }
        }
    };
    if changed {
        announce_current(state, handle);
    }
}

/// A participant this end may hold a session with in the current call, by their real address.
pub fn participant(handle: &Handle, room: &str, from: &Jid) -> bool {
    let s = handle.session();
    let Some(m) = s.muji.as_ref().filter(|m| m.room.as_str() == room) else { return false };
    m.participants.values().any(|p| p.real.as_ref().is_some_and(|r| r.to_string() == from.to_string()))
}

/// The call's room, for a session-initiate.
pub fn room_element(handle: &Handle) -> Option<Element> {
    handle.session().muji.as_ref().map(|m| muji_room(&m.room))
}

/// A call-invites message (XEP-0482). Returns whether the message was one.
pub fn message(state: &AppState, handle: &Handle, message: &Message) -> bool {
    let found = message.payloads.iter().find(|e| e.ns() == CIM && matches!(e.name(), "invite" | "retract" | "accept" | "reject" | "left")).or_else(|| message.payloads.iter().find(|e| e.ns() == CIM_LEGACY && matches!(e.name(), "propose" | "retract" | "accept" | "reject" | "finish")));
    let Some(e) = found else { return false };
    let Some(from) = message.from.clone() else { return true };
    // What the archive or a room's history repeats is not a call happening now.
    if message.payloads.iter().any(|p| p.is("delay", "urn:xmpp:delay")) {
        return true;
    }
    let Some(id) = e.attr("id").map(str::to_string) else { return true };
    let groupchat = message.type_ == MessageType::Groupchat;
    let (buffer_id, sender) = if groupchat {
        let chat_room = from.to_bare();
        // Our own invite, reflected by the room.
        if handle.session().rooms.get(&chat_room).is_some_and(|r| from.resource().is_some_and(|n| n.as_str() == r.nick)) {
            return true;
        }
        let Some(buffer) = super::super::muc::find_buffer(state, handle, &chat_room) else { return true };
        let who = handle.session().rooms.get(&chat_room).and_then(|r| r.occupants.get(from.resource().map(|n| n.as_str()).unwrap_or_default()).and_then(|o| o.jid.clone()));
        (buffer.id, who.unwrap_or_else(|| chat_room.clone()))
    } else {
        let peer = from.to_bare();
        if peer == handle.own() {
            return true;
        }
        (super::super::roster::ensure_buffer(state, handle, &peer).id, peer)
    };
    let name = e.name();
    match name {
        "invite" | "propose" => {
            let Some(room) = child(e, "muji", MUJI).and_then(|m| m.attr("room")).and_then(|r| r.parse::<BareJid>().ok()) else { return true };
            let reply_to = if groupchat { Jid::from(from.to_bare()) } else { Jid::from(sender.clone()) };
            let video = e.attr("video") == Some("true");
            let invite = Invite { id, room, from: sender.clone(), reply_to, groupchat };
            handle.session().call_invites.insert(buffer_id.clone(), invite);
            announce(state, &buffer_id, members(handle, &buffer_id));
            let kind = if video { "video call" } else { "call" };
            let words = if groupchat { format!("{sender} started a group {kind} here") } else { format!("{sender} is calling you into a group {kind}") };
            line(state, handle, &buffer_id, &words);
        }
        "retract" | "left" | "finish" => {
            let gone = {
                let mut s = handle.session();
                if s.call_invites.get(&buffer_id).is_some_and(|i| i.id == id && (name == "retract" || i.from == sender)) {
                    s.call_invites.remove(&buffer_id)
                } else {
                    None
                }
            };
            if gone.is_some() {
                announce(state, &buffer_id, members(handle, &buffer_id));
            }
        }
        _ => {}
    }
    true
}

fn line(state: &AppState, handle: &Handle, buffer_id: &str, words: &str) {
    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return };
    let kind = if buffer.kind == "channel" { "channel" } else { "dm" };
    state.runtime.record_message_at(state, &handle.account_id, &buffer.name, kind, "*", words, false, "system", None, None, false, None, Vec::new(), Vec::new(), None, None, None, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_key_is_the_windows_and_reads_back() {
        let full: FullJid = "bob@example.org/laptop".parse().unwrap();
        assert_eq!(member_key(&full), "bob@example.org|laptop");
        assert_eq!(parse_member_key("bob@example.org|laptop"), Some(full));
    }

    #[test]
    fn an_invite_carries_the_id_on_its_element_with_dinos_older_copy() {
        let room: BareJid = "1a2b3c4d@conference.example.org".parse().unwrap();
        let m = cim_message("bob@example.org".parse().unwrap(), false, "invite", "call-1", Some(muji_room(&room)), true);
        let invite = m.payloads.iter().find(|e| e.is("invite", CIM)).unwrap();
        assert_eq!((invite.attr("id"), invite.attr("video"), invite.attr("multi")), (Some("call-1"), Some("true"), Some("true")));
        assert_eq!(child(invite, "muji", MUJI).and_then(|m| m.attr("room")), Some("1a2b3c4d@conference.example.org"));
        assert!(m.payloads.iter().any(|e| e.is("propose", CIM_LEGACY) && e.attr("id") == Some("call-1")));
        assert!(m.payloads.iter().any(|e| e.is("store", ns::HINTS)));
        let left = cim_message("room@conference.example.org".parse().unwrap(), true, "left", "call-1", None, false);
        assert_eq!(left.type_, MessageType::Groupchat);
        assert!(left.payloads.iter().any(|e| e.is("finish", CIM_LEGACY)));
    }
}
