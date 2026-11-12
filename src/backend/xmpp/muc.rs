//! Group chats (XEP-0045), with the extensions that make them usable: occupant ids (XEP-0421), room archives (MAM on the
//! room), the room's own subject, and everything the common clients show about who is in a room and what they may do.
//!
//! A room is a conversation of kind `channel` named by its disco name (or the local part of its address) and filed under
//! "Rooms"; its occupants are the member list. Joining is a presence to `room/nick`; the room answers with the occupants'
//! presences (each carrying a role and an affiliation, and, in a room that shows them, the real address), then the subject,
//! and `110` in a presence marks our own, which is when we are in. A refusal is an error presence, and says why.
//!
//! What a message from the room says about who sent it is the occupant's nickname, which can change; the sender id kept
//! with the message is the occupant id when the room gives one, so a correction or a retraction is still accepted from
//! the same person after a nickname change, and from nobody else.

use super::messages::{self, Conv, Origin};
use super::session::{Failure, Handle};
use super::xml::Attr;
use super::{incoming, ns, roster};
use crate::model::Buffer;
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::message::Message;
use tokio_xmpp::parsers::presence::{Presence, Type as PresenceType};

pub const MUC: &str = "http://jabber.org/protocol/muc";
pub const MUC_USER: &str = "http://jabber.org/protocol/muc#user";
pub const MUC_ADMIN: &str = "http://jabber.org/protocol/muc#admin";
pub const MUC_OWNER: &str = "http://jabber.org/protocol/muc#owner";
pub const OCCUPANT_ID: &str = "urn:xmpp:occupant-id:0";
pub const CATEGORY: &str = "Rooms";

/// Somebody in a room.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Occupant {
    /// Their address, if the room says (a room that is not anonymous).
    pub jid: Option<BareJid>,
    pub role: String,
    pub affiliation: String,
    pub show: String,
    pub status: String,
    pub occupant_id: Option<String>,
    /// The titles of the hats the room gave them (XEP-0317): what they are in this room, in its own words.
    pub hats: Vec<String>,
}

/// A room this account is in, or is joining.
#[derive(Debug, Clone, Default)]
pub struct Room {
    pub nick: String,
    pub joined: bool,
    pub password: Option<String>,
    pub subject: Option<String>,
    pub name: Option<String>,
    pub occupants: BTreeMap<String, Occupant>,
    pub features: HashSet<String>,
    pub own_role: String,
    pub own_affiliation: String,
    /// Our own occupant id in this room, which marks our messages whatever we are called.
    pub own_occupant_id: Option<String>,
    /// The first subject has been seen (what comes with the history is the room's state, not a change).
    pub subject_seen: bool,
    /// What the room is like has been said in its conversation, once.
    pub announced: bool,
}

impl Room {
    pub fn anonymous(&self) -> bool {
        !self.features.contains("muc_nonanonymous")
    }

    pub fn has_archive(&self) -> bool {
        self.features.contains(ns::MAM)
    }
}

/// The codes a room's presence carries (`<status code='110'/>`).
pub fn status_codes(user: &Element) -> Vec<u16> {
    user.children().filter(|c| c.name() == "status").filter_map(|c| c.attr("code").and_then(|v| v.parse().ok())).collect()
}

fn user_element(p_payloads: &[Element]) -> Option<&Element> {
    p_payloads.iter().find(|e| e.is("x", MUC_USER))
}

/// The occupant id a stanza carries, when it was stamped by `room`.
pub fn occupant_id_of(payloads: &[Element], room: &BareJid) -> Option<String> {
    payloads.iter().filter(|e| e.is("occupant-id", OCCUPANT_ID)).find(|e| e.attr("by").and_then(|b| b.parse::<Jid>().ok()).is_some_and(|b| &b.to_bare() == room)).and_then(|e| e.attr("id").map(str::to_string))
}

/// The hats on a presence: the titles, which are what a person reads (the URI behind each is for software).
pub fn hats_of(payloads: &[Element]) -> Vec<String> {
    payloads.iter().filter(|e| e.is("hats", "urn:xmpp:hats:0")).flat_map(|h| h.children()).filter(|h| h.name() == "hat").filter_map(|h| h.attr("title").or_else(|| h.attr("uri")).map(|t| t.trim().to_string())).filter(|t| !t.is_empty()).take(5).collect()
}

pub fn is_room(handle: &Handle, jid: &BareJid) -> bool {
    handle.session().rooms.contains_key(jid)
}

/// The buffer of a room, if it has one.
pub fn find_buffer(state: &AppState, handle: &Handle, room: &BareJid) -> Option<Buffer> {
    let remote = room.to_string();
    state.runtime.list_buffers().into_iter().find(|b| b.account_id == handle.account_id && b.kind == "channel" && b.remote_id.as_deref() == Some(remote.as_str()))
}

/// The conversation of a room, made if it is not there.
pub fn ensure_buffer(state: &AppState, handle: &Handle, room: &BareJid) -> Buffer {
    let known = handle.session().rooms.get(room).and_then(|r| r.name.clone()).filter(|n| !n.trim().is_empty());
    // A room whose name is not known yet keeps the name its conversation already has, rather than being renamed to its
    // address and back as the name turns up.
    if known.is_none() {
        if let Some(found) = find_buffer(state, handle, room) {
            return found;
        }
    }
    let display = known.unwrap_or_else(|| room.node().map(|n| n.to_string()).unwrap_or_else(|| room.to_string()));
    let taken = |name: &str| state.runtime.list_buffers().into_iter().any(|b| b.account_id == handle.account_id && b.name == name && b.remote_id.as_deref() != Some(room.as_str()));
    let name = if taken(&display) { format!("{display} ({room})") } else { display };
    let buffer = match find_buffer(state, handle, room) {
        Some(found) if found.name == name => found,
        Some(found) => {
            state.runtime.rename_buffer(state, &handle.account_id, &found.name, &name, "channel");
            state.runtime.get_buffer(&crate::model::buffer_id(&handle.account_id, &name)).unwrap_or(found)
        }
        None => state.runtime.ensure_buffer(state, &handle.account_id, &name, "channel"),
    };
    state.runtime.set_buffer_remote_id(state, &buffer.id, room.as_str());
    state.runtime.set_buffer_category(state, &buffer.id, Some(CATEGORY), 0);
    state.runtime.get_buffer(&buffer.id).unwrap_or(buffer)
}

/// A system line in a room: somebody came, went, was removed, changed the subject.
fn card(state: &AppState, handle: &Handle, buffer: &Buffer, kind: &str, text: &str) {
    state.runtime.record_message_at(state, &handle.account_id, &buffer.name, "channel", "*", text, false, kind, None, None, false, None, Vec::new(), Vec::new(), None, None, None, None);
}

// --- joining -------------------------------------------------------------------------------------------------------

/// What a change of somebody's role or affiliation is, in words; `None` when nothing that is shown changed.
pub fn standing_change(who: &str, role: &str, affiliation: &str, new_role: &str, new_affiliation: &str) -> Option<String> {
    let you = who == "You";
    let (are, have) = if you { ("are", "have") } else { ("is", "has") };
    if affiliation != new_affiliation {
        return Some(match new_affiliation {
            "owner" => format!("{who} {are} now an owner of the room"),
            "admin" => format!("{who} {are} now an admin of the room"),
            "member" if affiliation == "none" || affiliation == "outcast" => format!("{who} {are} now a member of the room"),
            "member" => format!("{who} {are} no longer an {affiliation} of the room, only a member"),
            "none" => format!("{who} {are} no longer {} of the room", if affiliation == "member" { "a member".to_string() } else { format!("an {affiliation}") }),
            other => format!("{who} {are} now {other} in the room"),
        });
    }
    if role != new_role {
        return Some(match (role, new_role) {
            (_, "moderator") => format!("{who} {are} now a moderator"),
            ("moderator", "participant") => format!("{who} {are} no longer a moderator"),
            ("visitor", "participant") => format!("{who} can speak now ({have} been given a voice)"),
            (_, "visitor") => format!("{who} can no longer speak in the room"),
            (_, other) => format!("{who} {are} now a {other}"),
        });
    }
    None
}

/// What a room's status codes say about a change to its configuration (XEP-0045 10.2.1), in words.
pub fn config_words(codes: &[u16]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for c in codes {
        out.push(match c {
            170 => "The room now keeps what is said in it (it is logged)",
            171 => "The room no longer keeps what is said in it",
            172 => "The room now shows everyone's address to everyone in it",
            173 => "The room now shows addresses to its moderators only",
            174 => "The room no longer shows anybody's address",
            104 => "The room's settings were changed",
            _ => continue,
        });
    }
    out
}

/// What a room says about itself: its name and what it supports.
async fn disco(handle: &Handle, room: &BareJid) -> (Option<String>, HashSet<String>, Option<String>) {
    let to = Jid::from(room.clone());
    match handle.request(Some(to), false, Element::builder("query", ns::DISCO_INFO).build()).await {
        Ok(Some(q)) => {
            let name = q.children().find(|c| c.name() == "identity" && c.attr("category") == Some("conference")).and_then(|c| c.attr("name").map(str::to_string));
            let features = q.children().filter(|c| c.name() == "feature").filter_map(|c| c.attr("var").map(str::to_string)).collect();
            (name, features, avatar_hash(&q))
        }
        _ => (None, HashSet::new(), None),
    }
}

/// The room's picture, as its disco#info gives it (XEP-0486: `muc#roominfo_avatarhash`, the SHA-1 of the picture in its vCard).
pub fn avatar_hash(info: &Element) -> Option<String> {
    info.children()
        .filter(|c| c.is("x", ns::DATA))
        .flat_map(|x| x.children())
        .find(|f| f.name() == "field" && f.attr("var") == Some("muc#roominfo_avatarhash"))
        .and_then(|f| f.children().find(|v| v.name() == "value"))
        .map(|v| v.text().trim().to_string())
        .filter(|h| !h.is_empty())
}

/// Asks the room again what it supports and how it is set up (not its picture: see [`refresh`]).
pub async fn refresh_features(handle: &Handle, room: &BareJid) {
    let (name, features, _) = disco(handle, room).await;
    if let Some(r) = handle.session().rooms.get_mut(room) {
        r.features = features;
        r.name = name.or(r.name.take());
    }
}

/// Asks the room again for its picture (XEP-0486), after it said its configuration changed. Only the picture: the name the
/// room's conversation goes by is not changed under it.
pub async fn refresh_avatar(state: &AppState, handle: &Handle, room: &BareJid) {
    let (_, _, avatar) = disco(handle, room).await;
    super::avatars::room(state, handle, room, avatar.as_deref()).await;
}

/// Joins a room (or rejoins it). The answer is the room's presences, handled as they arrive.
pub async fn join(state: &AppState, handle: &Handle, room: &BareJid, nick: Option<&str>, password: Option<&str>) -> Result<(), String> {
    let (name, features, avatar) = disco(handle, room).await;
    let nick = nick.map(str::to_string).or_else(|| handle.session().rooms.get(room).map(|r| r.nick.clone()).filter(|n| !n.is_empty())).unwrap_or_else(|| handle.own().node().map(|n| n.to_string()).unwrap_or_else(|| "moho".to_string()));
    {
        let mut s = handle.session();
        let entry = s.rooms.entry(room.clone()).or_default();
        entry.nick = nick.clone();
        entry.joined = false;
        entry.password = password.map(str::to_string).or(entry.password.take());
        entry.name = name.or(entry.name.take());
        entry.features = features.clone();
    }
    ensure_buffer(state, handle, room);
    if let Some(hash) = avatar {
        let (s, h, r) = (state.clone(), handle.clone(), room.clone());
        tokio::spawn(async move { super::avatars::room(&s, &h, &r, Some(&hash)).await });
    }
    let Ok(to) = format!("{room}/{nick}").parse::<Jid>() else { return Err(format!("{nick:?} is not a usable nickname")) };
    let mut x = Element::builder("x", MUC).build();
    if let Some(pw) = handle.session().rooms.get(room).and_then(|r| r.password.clone()) {
        x.append_child(Element::builder("password", MUC).append(pw).build());
    }
    // A room with an archive is read from the archive; one without sends what it has on joining.
    if features.contains(ns::MAM) {
        x.append_child(Element::builder("history", MUC).at("maxchars", "0").build());
    }
    let mut p = Presence::new(PresenceType::None).with_to(to);
    p.payloads.push(x);
    p.payloads.push(super::caps::presence_element());
    handle.send_presence(p).await;
    Ok(())
}

/// MUC self-ping (XEP-0410): is the account still in its rooms? A room can drop somebody silently (it restarted; the server
/// lost the link to it while the connection was resumed). Each joined room is asked by pinging the account's own occupant
/// address; one that answers the account is not there (`not-acceptable`) is rejoined. Answers that say nothing of it (the room
/// does not answer pings for its occupants, or is not reachable for now) change nothing.
pub async fn self_ping_all(state: &AppState, handle: &Handle) {
    let rooms: Vec<(BareJid, String)> = handle.session().rooms.iter().filter(|(_, r)| r.joined && !r.nick.is_empty()).map(|(j, r)| (j.clone(), r.nick.clone())).collect();
    tracing::debug!("xmpp[{}]: self-ping of {} rooms (of {})", handle.account_id, rooms.len(), handle.session().rooms.len());
    for (room, nick) in rooms {
        let Ok(me) = format!("{room}/{nick}").parse::<Jid>() else { continue };
        let answer = handle.request(Some(me), false, super::xml::empty("ping", "urn:xmpp:ping")).await;
        tracing::debug!("xmpp[{}]: self-ping of {room}: {:?}", handle.account_id, answer.as_ref().map(|_| ()).map_err(|f| &f.condition));
        if let Some(why) = still_there(&answer) {
            tracing::info!("xmpp[{}]: no longer in {room} ({why}); joining again", handle.account_id);
            if let Some(r) = handle.session().rooms.get_mut(&room) {
                r.joined = false;
            }
            let _ = join(state, handle, &room, Some(&nick), None).await;
        }
    }
}

/// Whether a self-ping's answer says the account has left the room: `Some(why)` when it should rejoin.
pub fn still_there(answer: &Result<Option<Element>, super::session::Failure>) -> Option<String> {
    match answer {
        Ok(_) => None,
        Err(f) => {
            let c = f.condition.to_ascii_lowercase().replace(['-', '_'], "");
            // not-acceptable: not an occupant. item-not-found: the nickname is not there (some rooms say this).
            (c == "notacceptable" || c == "itemnotfound").then(|| f.condition.clone())
        }
    }
}

/// Leaves a room. The conversation stays, with what was said in it.
pub async fn leave(state: &AppState, handle: &Handle, room: &BareJid) {
    let nick = handle.session().rooms.get(room).map(|r| r.nick.clone()).unwrap_or_default();
    if let Ok(to) = format!("{room}/{nick}").parse::<Jid>() {
        handle.send_presence(Presence::new(PresenceType::Unavailable).with_to(to)).await;
    }
    handle.session().rooms.remove(room);
    if let Some(b) = find_buffer(state, handle, room) {
        state.runtime.set_buffer_read_only(state, &b.id, Some("You are not in this room"));
        state.runtime.set_presence(&b.id, json!([]));
        state.events.emit("presenceChange", json!({ "bufferId": b.id, "members": [], "total": 0 }));
    }
}

// --- presence --------------------------------------------------------------------------------------------------------

fn why_join_failed(error: &Element) -> String {
    let condition = error.children().map(|c| c.name().to_string()).find(|n| n != "text").unwrap_or_default();
    match condition.as_str() {
        "conflict" => "That nickname is taken in this room".into(),
        "forbidden" => "You are banned from this room".into(),
        "not-authorized" => "This room needs a password".into(),
        "registration-required" => "This room is for members only".into(),
        "service-unavailable" => "This room is full".into(),
        "item-not-found" => "This room does not exist, or is locked".into(),
        "not-allowed" => "You may not create rooms here".into(),
        "jid-malformed" => "That is not a usable room address or nickname".into(),
        "remote-server-not-found" | "remote-server-timeout" => "The room's server cannot be reached".into(),
        _ => format!("The room refused: {}", condition.replace('-', " ")),
    }
}

/// A presence from a room (`room/nick`) or from the room itself.
pub async fn handle_presence(state: &AppState, handle: &Handle, p: &Presence) {
    let Some(from) = p.from.clone() else { return };
    let room = from.to_bare();
    let nick = from.resource().map(|r| r.to_string()).unwrap_or_default();
    // A presence from the room itself, with no occupant behind it, is not somebody in the room.
    if nick.is_empty() && p.type_ != PresenceType::Error {
        return;
    }
    let buffer = ensure_buffer(state, handle, &room);
    if p.type_ == PresenceType::Error {
        let reason = p.payloads.iter().find(|e| e.name() == "error").map(why_join_failed).unwrap_or_else(|| "The room refused".to_string());
        let was_joined = handle.session().rooms.get(&room).is_some_and(|r| r.joined);
        if !was_joined {
            handle.session().rooms.remove(&room);
            state.runtime.set_buffer_read_only(state, &buffer.id, Some(&reason));
        }
        card(state, handle, &buffer, "system", &reason);
        return;
    }
    let user = user_element(&p.payloads).cloned().unwrap_or_else(|| Element::builder("x", MUC_USER).build());
    let codes = status_codes(&user);
    let item = user.children().find(|c| c.name() == "item").cloned();
    let attr = |name: &str| item.as_ref().and_then(|i| i.attr(name)).map(str::to_string);
    let is_self = codes.contains(&110) || handle.session().rooms.get(&room).is_some_and(|r| !r.nick.is_empty() && r.nick == nick);
    let reason = item.as_ref().and_then(|i| i.children().find(|c| c.name() == "reason")).map(|r| r.text()).filter(|r| !r.is_empty());

    if p.type_ == PresenceType::Unavailable {
        let gone = handle.session().rooms.get_mut(&room).and_then(|r| r.occupants.remove(&nick));
        let who = nick.clone();
        if codes.contains(&303) {
            // A nickname change: the room says unavailable for the old one and then available for the new one.
            let new = attr("nick").unwrap_or_default();
            if let Some(o) = gone {
                if let Some(r) = handle.session().rooms.get_mut(&room) {
                    r.occupants.insert(new.clone(), o);
                    if is_self {
                        r.nick = new.clone();
                    }
                }
            }
            card(state, handle, &buffer, "nick", &format!("{who} is now known as {new}"));
        } else if is_self {
            let words = if codes.contains(&307) {
                format!("You were kicked from the room{}", reason.map(|r| format!(": {r}")).unwrap_or_default())
            } else if codes.contains(&301) {
                format!("You were banned from the room{}", reason.map(|r| format!(": {r}")).unwrap_or_default())
            } else if codes.contains(&321) {
                "You were removed from the room because its membership changed".into()
            } else if codes.contains(&322) {
                "You were removed from the room because it became members-only".into()
            } else if codes.contains(&332) {
                "You were removed from the room because its service is shutting down".into()
            } else if let Some(d) = user.children().find(|c| c.name() == "destroy") {
                // The room was closed by its owner: why, and where it went, if they said.
                let why = d.children().find(|c| c.name() == "reason").map(|r| r.text()).filter(|r| !r.trim().is_empty());
                let moved = d.attr("jid").filter(|j| !j.is_empty());
                format!("The room was closed{}{}", why.map(|r| format!(": {}", r.trim())).unwrap_or_default(), moved.map(|j| format!(". It moved to {j}")).unwrap_or_default())
            } else {
                "You left the room".into()
            };
            if let Some(r) = handle.session().rooms.get_mut(&room) {
                r.joined = false;
                r.occupants.clear();
            }
            // A room whose service is restarting (332) is gone for a moment, not for good: it is joined again when the service
            // is back - tried a few times, a little longer apart each time. A kick or a ban is not undone this way.
            if codes.contains(&332) {
                let (s, h, r, n) = (state.clone(), handle.clone(), room.clone(), nick.clone());
                tokio::spawn(async move {
                    for wait in [5u64, 15, 30, 60] {
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                        if h.session().rooms.get(&r).is_none_or(|x| x.joined) {
                            return;
                        }
                        tracing::info!("xmpp[{}]: joining {r} again after its service restarted", h.account_id);
                        let _ = join(&s, &h, &r, Some(&n), None).await;
                    }
                });
            }
            state.runtime.set_buffer_read_only(state, &buffer.id, Some(&words));
            card(state, handle, &buffer, if codes.contains(&307) || codes.contains(&301) { "kick" } else { "part" }, &words);
        } else if gone.is_some() {
            let words = if codes.contains(&307) {
                format!("{who} was kicked{}", reason.map(|r| format!(": {r}")).unwrap_or_default())
            } else if codes.contains(&301) {
                format!("{who} was banned{}", reason.map(|r| format!(": {r}")).unwrap_or_default())
            } else {
                format!("{who} left")
            };
            card(state, handle, &buffer, if codes.contains(&307) || codes.contains(&301) { "kick" } else { "part" }, &words);
        }
        publish_occupants(state, handle, &room);
        return;
    }

    // Somebody is here: new, or changed.
    let show = match p.show {
        Some(tokio_xmpp::parsers::presence::Show::Away) => "away",
        Some(tokio_xmpp::parsers::presence::Show::Xa) => "xa",
        Some(tokio_xmpp::parsers::presence::Show::Dnd) => "dnd",
        _ => "online",
    };
    let occupant = Occupant {
        jid: attr("jid").and_then(|j| j.parse::<Jid>().ok()).map(|j| j.to_bare()),
        role: attr("role").unwrap_or_else(|| "participant".into()),
        affiliation: attr("affiliation").unwrap_or_else(|| "none".into()),
        show: show.into(),
        status: p.statuses.values().next().cloned().unwrap_or_default(),
        occupant_id: occupant_id_of(&p.payloads, &room),
        hats: hats_of(&p.payloads),
    };
    let (first_time, before) = {
        let mut s = handle.session();
        let Some(r) = s.rooms.get_mut(&room) else { return };
        let before = r.occupants.get(&nick).map(|o| (o.role.clone(), o.affiliation.clone()));
        let new = before.is_none();
        if is_self {
            r.own_role = occupant.role.clone();
            r.own_affiliation = occupant.affiliation.clone();
            r.own_occupant_id = occupant.occupant_id.clone();
        }
        r.occupants.insert(nick.clone(), occupant.clone());
        (new, before)
    };
    // Somebody's standing changed while they were here: said once, as the room says it.
    if let Some((role, affiliation)) = &before {
        if let Some(words) = standing_change(if is_self { "You" } else { &nick }, role, affiliation, &occupant.role, &occupant.affiliation) {
            card(state, handle, &buffer, "mode", &words);
        }
    }
    if is_self && !handle.session().rooms.get(&room).is_some_and(|r| r.joined) {
        if let Some(r) = handle.session().rooms.get_mut(&room) {
            r.joined = true;
        }
        state.runtime.set_buffer_read_only(state, &buffer.id, None);
        // Who can see whom: said once, on coming in, since it decides what anybody should write here.
        let (hidden, said) = handle.session().rooms.get_mut(&room).map(|r| (r.anonymous(), std::mem::replace(&mut r.announced, true))).unwrap_or((true, true));
        if !said {
            card(state, handle, &buffer, "system", if hidden { "This room does not show people's addresses" } else { "This room shows everyone's address to everyone in it" });
        }
        if codes.contains(&201) {
            // A room that did not exist is created locked, and is opened with its default settings, which the owner
            // can change from the room's settings afterwards.
            let form = Element::builder("x", ns::DATA).at("type", "submit").build();
            let query = Element::builder("query", MUC_OWNER).append(form).build();
            let (h, to) = (handle.clone(), Jid::from(room.clone()));
            tokio::spawn(async move {
                let _ = h.request(Some(to), true, query).await;
            });
            card(state, handle, &buffer, "system", "You created this room");
        }
        let h = handle.clone();
        let s = state.clone();
        let r = room.clone();
        tokio::spawn(async move { super::mam::room_catch_up(&s, &h, &r).await });
        if codes.contains(&104) || codes.contains(&170) {
            tracing::debug!("xmpp[{}]: room {room} changed its configuration", handle.account_id);
        }
    } else if first_time && !is_self && handle.session().rooms.get(&room).is_some_and(|r| r.joined) {
        card(state, handle, &buffer, "join", &format!("{nick} joined"));
    }
    publish_occupants(state, handle, &room);
}

/// The window's member list for a room.
pub fn publish_occupants(state: &AppState, handle: &Handle, room: &BareJid) {
    let Some(buffer) = find_buffer(state, handle, room) else { return };
    let list: Vec<Value> = {
        let s = handle.session();
        let Some(r) = s.rooms.get(room) else { return };
        r.occupants
            .iter()
            .map(|(nick, o)| {
                // The room's own words, highest standing first: a lasting
                // affiliation outranks the role of this visit. A participant -
                // the ordinary occupant with voice - is given nothing, and a
                // visitor is somebody the room will not let speak.
                let prefix = match (o.affiliation.as_str(), o.role.as_str()) {
                    ("owner", _) => "owner",
                    ("admin", _) => "admin",
                    (_, "moderator") => "moderator",
                    (_, "visitor") => "visitor",
                    _ => "",
                };
                let status = roster::status_word(&o.show);
                json!({
                    "nick": nick,
                    "userId": o.jid.as_ref().map(|j| j.to_string()).unwrap_or_else(|| format!("{room}/{nick}")),
                    "prefix": prefix,
                    "away": false,
                    "status": status,
                    "activity": if o.status.is_empty() { Value::Null } else { json!({ "kind": "custom", "text": o.status }) },
                    "bot": false,
                    "relationship": "",
                    "avatarUrl": Value::Null,
                    "roles": [o.affiliation, o.role],
                    "hats": o.hats,
                })
            })
            .collect()
    };
    let total = list.len();
    let list = Value::Array(list);
    state.runtime.set_presence(&buffer.id, list.clone());
    state.runtime.set_presence_total(&buffer.id, total);
    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list, "total": total }));
}

// --- messages --------------------------------------------------------------------------------------------------------

/// A message of type groupchat: live from the room, or out of its archive.
pub async fn handle_groupchat(state: &AppState, handle: &Handle, message: Message, origin: Origin) {
    let Some(from) = message.from.clone() else { return };
    let room = from.to_bare();
    let nick = from.resource().map(|r| r.to_string()).unwrap_or_default();
    // A room this account is not in (and has never been in) is not talking to it.
    if !is_room(handle, &room) && !matches!(origin, Origin::Archive { .. }) {
        return;
    }
    let buffer = ensure_buffer(state, handle, &room);
    // What the room itself says (no nickname): its configuration changed (status codes), or an invitation was declined.
    if nick.is_empty() && matches!(origin, Origin::Live) {
        if let Some(x) = message.payloads.iter().find(|e| e.is("x", MUC_USER)) {
            let codes = status_codes(x);
            if codes.contains(&104) {
                // Asked again, its picture included (XEP-0486).
                let (s, h, r) = (state.clone(), handle.clone(), room.clone());
                tokio::spawn(async move { refresh_avatar(&s, &h, &r).await });
            }
            for words in config_words(&codes) {
                card(state, handle, &buffer, "system", words);
            }
            if let Some(d) = x.children().find(|c| c.name() == "decline") {
                let who = d.attr("from").unwrap_or("Somebody");
                let why = d.children().find(|c| c.name() == "reason").map(|r| r.text()).filter(|r| !r.trim().is_empty());
                card(state, handle, &buffer, "system", &format!("{who} declined your invitation{}", why.map(|r| format!(": {}", r.trim())).unwrap_or_default()));
            }
        }
    }
    let p = incoming::parse(&message);
    // The room's subject: who set it, and what it is.
    if message.bodies.is_empty() {
        if let Some(subject) = &p.subject {
            let first = {
                let mut s = handle.session();
                match s.rooms.get_mut(&room) {
                    Some(r) => {
                        let first = !r.subject_seen;
                        r.subject_seen = true;
                        r.subject = Some(subject.clone());
                        first
                    }
                    None => true,
                }
            };
            state.runtime.set_buffer_topic(state, &buffer.id, subject);
            if !first && !nick.is_empty() && !matches!(origin, Origin::Archive { .. }) {
                card(state, handle, &buffer, "topic", &format!("{nick} changed the subject to: {subject}"));
            }
            return;
        }
    }
    // A moderator took a message away: the room says so, in its own name. Only the room itself may say it.
    if nick.is_empty() {
        if let (Some(target), Some(reason)) = (&p.retracts, &p.moderated) {
            let target = handle.session().aliases.get(target).cloned().unwrap_or_else(|| target.clone());
            let had = state.store.get_message(&buffer.id, &target).ok().flatten().is_some();
            state.runtime.delete_message(state, &buffer.id, &target);
            if had && !matches!(origin, Origin::Archive { .. }) {
                card(state, handle, &buffer, "system", &format!("A moderator removed a message{}", reason.as_ref().filter(|r| !r.is_empty()).map(|r| format!(": {r}")).unwrap_or_default()));
            }
            return;
        }
    }
    // The room itself speaking (no nickname): its notices, shown as what they are.
    if nick.is_empty() {
        if let Some(body) = &p.body {
            card(state, handle, &buffer, "system", body);
        }
        return;
    }
    let (own_nick, own_oid) = {
        let s = handle.session();
        s.rooms.get(&room).map(|r| (r.nick.clone(), r.own_occupant_id.clone())).unwrap_or_default()
    };
    let oid = occupant_id_of(&message.payloads, &room);
    let is_own = nick == own_nick || (oid.is_some() && oid == own_oid);
    let sender_id = match &oid {
        Some(oid) => format!("{room}#{oid}"),
        None => format!("{room}/{nick}"),
    };
    // The room's own id for the message is the one later reactions and retractions of it may name.
    if let Some(sid) = p.stanza_ids.iter().find(|s| s.by == room.as_str()) {
        if let Some(local) = incoming::local_id(&p) {
            handle.session().room_ids.insert(local, sid.id.clone());
        }
    }
    let own_identity = handle.own().to_string();
    let mentioned = !is_own && mentions(&p, &own_nick, &room, &handle.own());
    let real_jid = if is_own { Some(handle.own().to_string()) } else { handle.session().rooms.get(&room).and_then(|r| r.occupants.get(&nick)).and_then(|o| o.jid.as_ref()).map(|j| j.to_string()) };
    let conv = Conv { buffer, kind: "channel", from_name: if is_own { own_identity } else { nick.clone() }, sender_id, is_own, key: room.to_string(), mentioned, real_jid };
    messages::deliver(state, handle, &conv, p, origin).await;
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The references (XEP-0372) for the people a message to a room names, by their nicknames as whole words: the address where the
/// room shows it, else the nickname in the room. `skip` is how many characters at the start are not the sender's (a reply's quote).
pub fn mention_references(handle: &Handle, room: &BareJid, body: &str, skip: usize) -> Vec<Element> {
    let chars: Vec<char> = body.chars().collect();
    let (own_nick, occupants): (String, Vec<(String, Option<BareJid>)>) = {
        let s = handle.session();
        let Some(r) = s.rooms.get(room) else { return Vec::new() };
        (r.nick.clone(), r.occupants.iter().map(|(n, o)| (n.clone(), o.jid.clone())).collect())
    };
    let mut found: Vec<(usize, usize, String)> = Vec::new();
    for (nick, jid) in occupants.into_iter().filter(|(n, _)| !n.is_empty() && *n != own_nick) {
        let needle: Vec<char> = nick.chars().collect();
        let mut i = skip;
        while i + needle.len() <= chars.len() {
            if chars[i..i + needle.len()] == needle[..] {
                let edge = |c: Option<&char>| c.is_none_or(|c| !c.is_alphanumeric() && *c != '_');
                let before = if i == 0 { None } else { chars.get(i - 1) };
                if edge(before) && edge(chars.get(i + needle.len())) {
                    let uri = match &jid {
                        Some(j) => format!("xmpp:{j}"),
                        None => format!("xmpp:{room}/{nick}"),
                    };
                    found.push((i, i + needle.len(), uri));
                    i += needle.len();
                    continue;
                }
            }
            i += 1;
        }
    }
    found.sort();
    found.into_iter().map(|(b, e, uri)| Element::builder("reference", ns::REFERENCE).at("type", "mention").at("begin", b.to_string()).at("end", e.to_string()).at("uri", uri).build()).collect()
}

/// Whether a message is addressed to this account in a room: it names the nickname, or answers one of its messages.
pub fn mentions(p: &incoming::Parsed, nick: &str, room: &BareJid, own: &BareJid) -> bool {
    if nick.is_empty() {
        return false;
    }
    // Named by a reference (XEP-0372): by the address of the account, or by the nickname in the room.
    let by_nick = format!("xmpp:{room}/{nick}");
    let by_jid = format!("xmpp:{own}");
    if p.mentions.iter().any(|u| {
        let u = u.split('?').next().unwrap_or(u);
        let decoded = percent_decode(u);
        decoded == by_nick || decoded.eq_ignore_ascii_case(&by_jid)
    }) {
        return true;
    }
    if p.reply.as_ref().and_then(|r| r.to.as_deref()).is_some_and(|to| to == format!("{room}/{nick}")) {
        return true;
    }
    let Some(body) = &p.body else { return false };
    let body = incoming::strip_fallbacks(body, &p.fallbacks, &[ns::REPLY]).to_lowercase();
    let nick = nick.to_lowercase();
    let mut from = 0;
    while let Some(at) = body[from..].find(&nick) {
        let start = from + at;
        let end = start + nick.len();
        let before = body[..start].chars().next_back();
        let after = body[end..].chars().next();
        let edge = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if edge(before) && edge(after) {
            return true;
        }
        from = end;
    }
    false
}

/// A message to one occupant of a room (type chat from `room/nick`): a conversation of its own, named for them.
pub fn private_conversation(state: &AppState, handle: &Handle, message: &Message, is_own: bool) -> Option<Conv> {
    let counterpart = if is_own { message.to.clone()? } else { message.from.clone()? };
    let room = counterpart.to_bare();
    if !is_room(handle, &room) {
        return None;
    }
    let nick = counterpart.resource().map(|r| r.to_string())?;
    let name = format!("{nick} in {}", handle.session().rooms.get(&room).and_then(|r| r.name.clone()).unwrap_or_else(|| room.to_string()));
    let buffer = state.runtime.ensure_buffer(state, &handle.account_id, &name, "dm");
    state.runtime.set_buffer_remote_id(state, &buffer.id, &counterpart.to_string());
    state.runtime.set_buffer_category(state, &buffer.id, Some(roster::DEFAULT_CATEGORY), 0);
    let own = handle.own().to_string();
    Some(Conv { buffer, kind: "dm", from_name: if is_own { own } else { nick.clone() }, sender_id: format!("{room}/{nick}"), is_own, key: counterpart.to_string(), mentioned: false, real_jid: None })
}

// --- asking ----------------------------------------------------------------------------------------------------------

/// What a room's owner or admin set up, as a form (XEP-0004), to be shown and sent back.
pub async fn config_form(handle: &Handle, room: &BareJid) -> Result<Option<Element>, Failure> {
    handle.request(Some(Jid::from(room.clone())), false, Element::builder("query", MUC_OWNER).build()).await
}

/// The addresses of the rooms an account's service offers (disco#items of the conference service).
pub async fn list_rooms(handle: &Handle, service: &Jid) -> Result<Vec<(String, String)>, Failure> {
    let q = handle.request(Some(service.clone()), false, Element::builder("query", ns::DISCO_ITEMS).build()).await?;
    Ok(q.map(|q| q.children().filter(|c| c.name() == "item").filter_map(|c| Some((c.attr("jid")?.to_string(), c.attr("name").unwrap_or_default().to_string()))).collect()).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    #[test]
    fn hats_are_read_as_the_titles_a_room_gave() {
        let presence: Element = "<presence xmlns='jabber:client'><hats xmlns='urn:xmpp:hats:0'><hat uri='http://example.org/hats#admin' title='Administrator'/><hat uri='http://example.org/hats#vip'/><hat title=' '/></hats></presence>".parse().unwrap();
        let payloads: Vec<Element> = presence.children().cloned().collect();
        assert_eq!(super::hats_of(&payloads), vec!["Administrator".to_string(), "http://example.org/hats#vip".to_string()]);
        assert!(super::hats_of(&[]).is_empty());
    }

    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_rooms_presence_codes_are_read() {
        let x = Element::from_str("<x xmlns='http://jabber.org/protocol/muc#user'><item affiliation='member' role='participant'/><status code='110'/><status code='201'/></x>").unwrap();
        assert_eq!(status_codes(&x), vec![110, 201]);
    }

    #[test]
    fn a_stamp_from_another_service_is_not_an_occupant_id() {
        let room: BareJid = "room@muc.example.org".parse().unwrap();
        let good = Element::from_str("<occupant-id xmlns='urn:xmpp:occupant-id:0' id='abc' by='room@muc.example.org'/>").unwrap();
        let forged = Element::from_str("<occupant-id xmlns='urn:xmpp:occupant-id:0' id='abc' by='evil@muc.example.org'/>").unwrap();
        assert_eq!(occupant_id_of(&[good], &room).as_deref(), Some("abc"));
        assert_eq!(occupant_id_of(&[forged], &room), None);
    }

    fn parsed(body: &str, reply_to: Option<&str>) -> incoming::Parsed {
        incoming::Parsed { body: Some(body.into()), reply: reply_to.map(|t| incoming::Reply { id: "x".into(), to: Some(t.into()) }), ..Default::default() }
    }

    #[test]
    fn being_named_in_a_room_is_a_mention_only_as_a_whole_word() {
        let room: BareJid = "room@muc.example.org".parse().unwrap();
        let me: BareJid = "me@example.org".parse().unwrap();
        assert!(mentions(&parsed("hey Alice, look", None), "alice", &room, &me));
        assert!(mentions(&parsed("alice: look", None), "alice", &room, &me));
        assert!(mentions(&parsed("look @alice", None), "alice", &room, &me));
        assert!(mentions(&parsed("malice is not alice-like", None), "alice", &room, &me));
        assert!(!mentions(&parsed("malicetown", None), "alice", &room, &me));
        assert!(!mentions(&parsed("alices", None), "alice", &room, &me));
        assert!(mentions(&parsed("yes", Some("room@muc.example.org/alice")), "alice", &room, &me), "an answer to one of our messages counts");
        assert!(!mentions(&parsed("alice", None), "", &room, &me));
    }

    #[test]
    fn a_reference_names_this_account_by_its_address_or_by_its_nickname_in_the_room() {
        let room: BareJid = "room@muc.example.org".parse().unwrap();
        let me: BareJid = "me@example.org".parse().unwrap();
        let by = |uri: &str| incoming::Parsed { body: Some("hello everybody".into()), mentions: vec![uri.to_string()], ..Default::default() };
        assert!(mentions(&by("xmpp:me@example.org"), "alice", &room, &me));
        assert!(mentions(&by("xmpp:ME@example.org"), "alice", &room, &me));
        assert!(mentions(&by("xmpp:room@muc.example.org/alice"), "alice", &room, &me));
        assert!(mentions(&by("xmpp:room@muc.example.org/al%69ce"), "alice", &room, &me));
        assert!(!mentions(&by("xmpp:other@example.org"), "alice", &room, &me));
        assert!(!mentions(&by("xmpp:room@muc.example.org/bob"), "alice", &room, &me));
        assert_eq!(percent_decode("a%2Fb%zz%"), "a/b%zz%");
    }

    #[test]
    fn what_a_room_refused_is_said_in_words() {
        let e = |c: &str| Element::from_str(&format!("<error xmlns='jabber:client' type='cancel'><{c} xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error>")).unwrap();
        assert_eq!(why_join_failed(&e("conflict")), "That nickname is taken in this room");
        assert_eq!(why_join_failed(&e("not-authorized")), "This room needs a password");
        assert_eq!(why_join_failed(&e("registration-required")), "This room is for members only");
        assert!(why_join_failed(&e("gone")).contains("gone"));
    }

    #[test]
    fn a_room_that_does_not_say_it_shows_addresses_is_anonymous() {
        let mut r = Room::default();
        assert!(r.anonymous());
        r.features.insert("muc_nonanonymous".into());
        assert!(!r.anonymous());
        assert!(!r.has_archive());
        r.features.insert(ns::MAM.into());
        assert!(r.has_archive());
    }
}

#[cfg(test)]
mod card_tests {
    use super::*;

    #[test]
    fn a_self_ping_rejoins_only_when_the_room_says_the_account_is_gone() {
        use super::super::session::Failure;
        let f = |c: &str| Err(Failure { condition: c.into(), text: String::new() });
        assert!(still_there(&Ok(None)).is_none());
        assert!(still_there(&f("NotAcceptable")).is_some());
        assert!(still_there(&f("ItemNotFound")).is_some());
        assert!(still_there(&f("ServiceUnavailable")).is_none(), "a room that does not answer pings: still there");
        assert!(still_there(&f("FeatureNotImplemented")).is_none());
        assert!(still_there(&f("remote-server-timeout")).is_none(), "cannot tell: nothing done");
    }

    #[test]
    fn a_change_of_standing_is_said_in_words() {
        assert_eq!(standing_change("pat", "participant", "none", "moderator", "none").as_deref(), Some("pat is now a moderator"));
        assert_eq!(standing_change("pat", "visitor", "none", "participant", "none").as_deref(), Some("pat can speak now (has been given a voice)"));
        assert_eq!(standing_change("pat", "participant", "none", "visitor", "none").as_deref(), Some("pat can no longer speak in the room"));
        assert_eq!(standing_change("You", "participant", "none", "participant", "member").as_deref(), Some("You are now a member of the room"));
        assert_eq!(standing_change("pat", "moderator", "admin", "participant", "none").as_deref(), Some("pat is no longer an admin of the room"));
        assert_eq!(standing_change("pat", "participant", "member", "participant", "member"), None, "a presence that changes nothing shown says nothing");
    }

    #[test]
    fn a_rooms_configuration_change_is_said() {
        assert_eq!(config_words(&[104, 172]), vec!["The room's settings were changed", "The room now shows everyone's address to everyone in it"]);
        assert!(config_words(&[110, 201]).is_empty());
    }
}

#[cfg(test)]
mod avatar_tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_rooms_picture_is_named_in_its_disco_info() {
        let q = Element::from_str("<query xmlns='http://jabber.org/protocol/disco#info'><feature var='vcard-temp'/><x xmlns='jabber:x:data' type='result'><field var='FORM_TYPE' type='hidden'><value>http://jabber.org/protocol/muc#roominfo</value></field><field var='muc#roominfo_avatarhash' type='text-multi'><value>a31c4bd04de69663cfd7f424a8453f4674da37ff</value></field></x></query>").unwrap();
        assert_eq!(avatar_hash(&q).as_deref(), Some("a31c4bd04de69663cfd7f424a8453f4674da37ff"));
        assert_eq!(avatar_hash(&Element::from_str("<query xmlns='http://jabber.org/protocol/disco#info'/>").unwrap()), None);
    }
}
