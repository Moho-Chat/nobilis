//! The roster (RFC 6121): the people this account knows, and who of them is here.
//!
//! The server keeps the list. It is asked for once per connection (with the version last seen, so an unchanged list
//! costs one empty reply), and changes arrive as pushes. Each person on it is a direct-message conversation, filed under
//! their roster group; their presence is what the window shows by the name.

use super::session::{Contact, Handle, Presence_};
use super::xml::Attr;
use super::ns;
use crate::model::Buffer;
use crate::state::AppState;
use serde_json::{json, Value};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::presence::{Presence, Show, Type as PresenceType};

/// The heading a contact is filed under when the roster puts it in no group.
pub const DEFAULT_CATEGORY: &str = "Direct Messages";

/// What a `show` is, in the window's words.
pub fn status_word(show: &str) -> &'static str {
    match show {
        "away" | "xa" => "idle",
        "dnd" => "dnd",
        "online" => "online",
        _ => "offline",
    }
}

/// The roster items of a query, as (address, contact); `remove` ones are returned with subscription `remove`.
pub fn parse_items(query: &Element) -> Vec<(BareJid, Contact)> {
    let mut out = Vec::new();
    for item in query.children().filter(|c| c.is("item", ns::ROSTER)) {
        let Some(jid) = item.attr("jid").and_then(|j| j.parse::<Jid>().ok()).map(|j| j.to_bare()) else { continue };
        let groups = item.children().filter(|g| g.is("group", ns::ROSTER)).map(|g| g.text()).filter(|g| !g.is_empty()).collect();
        out.push((jid, Contact { name: item.attr("name").map(str::to_string).filter(|n| !n.is_empty()), subscription: item.attr("subscription").unwrap_or("none").to_string(), ask: item.attr("ask") == Some("subscribe"), groups }));
    }
    out
}

/// Asks for the roster and keeps it.
pub async fn fetch(state: &AppState, handle: &Handle) {
    let mut query = Element::builder("query", ns::ROSTER);
    let ver = handle.session().roster_version.clone();
    // Versioning is only worth asking for if there is a list to compare with; and a server that does not offer it
    // ignores the attribute.
    if ver.is_some() && handle.session().server_features.contains(ns::ROSTER_VER) {
        query = query.at("ver", ver.unwrap_or_default());
    }
    match handle.request(None, false, query.build()).await {
        Ok(Some(q)) => {
            let items = parse_items(&q);
            {
                let mut s = handle.session();
                s.roster.clear();
                for (jid, contact) in &items {
                    s.roster.insert(jid.clone(), contact.clone());
                }
                s.roster_version = q.attr("ver").map(str::to_string);
            }
            for (jid, _) in &items {
                ensure_buffer(state, handle, jid);
            }
            tracing::info!("xmpp[{}]: roster of {}", handle.account_id, items.len());
        }
        Ok(None) => tracing::info!("xmpp[{}]: the roster has not changed", handle.account_id),
        Err(e) => tracing::warn!("xmpp[{}]: roster: {e}", handle.account_id),
    }
}

/// A roster push, or the answer to one of ours. Returns the reply the server is owed.
pub fn push(state: &AppState, handle: &Handle, query: &Element) {
    for (jid, contact) in parse_items(query) {
        if contact.subscription == "remove" {
            handle.session().roster.remove(&jid);
            handle.session().presence.remove(&jid);
            // The conversation stays: what was said does not go away because somebody was taken off a list.
            if let Some(b) = find_buffer(state, handle, &jid) {
                state.runtime.set_buffer_category(state, &b.id, Some(DEFAULT_CATEGORY), 0);
                publish_presence(state, handle, &jid);
            }
            continue;
        }
        handle.session().roster.insert(jid.clone(), contact);
        ensure_buffer(state, handle, &jid);
    }
    if let Some(ver) = query.attr("ver") {
        handle.session().roster_version = Some(ver.to_string());
    }
}

/// The conversation with this person, if it exists.
pub fn find_buffer(state: &AppState, handle: &Handle, jid: &BareJid) -> Option<Buffer> {
    let remote = jid.to_string();
    state.runtime.list_buffers().into_iter().find(|b| b.account_id == handle.account_id && b.kind == "dm" && b.remote_id.as_deref() == Some(remote.as_str()))
}

/// The conversation with this person, made (and named, and filed) if it is not there yet.
pub fn ensure_buffer(state: &AppState, handle: &Handle, jid: &BareJid) -> Buffer {
    let (display, group) = {
        let s = handle.session();
        (s.display_name(jid), s.roster.get(jid).and_then(|c| c.groups.first().cloned()))
    };
    let taken = |name: &str| state.runtime.list_buffers().into_iter().any(|b| b.account_id == handle.account_id && b.name == name && b.remote_id.as_deref() != Some(jid.as_str()));
    let name = if taken(&display) { format!("{display} ({jid})") } else { display };
    let existing = find_buffer(state, handle, jid);
    let buffer = match existing {
        Some(found) if found.name == name => found,
        Some(found) => {
            state.runtime.rename_buffer(state, &handle.account_id, &found.name, &name, "dm");
            state.runtime.get_buffer(&crate::model::buffer_id(&handle.account_id, &name)).unwrap_or(found)
        }
        None => state.runtime.ensure_buffer(state, &handle.account_id, &name, "dm"),
    };
    state.runtime.set_buffer_remote_id(state, &buffer.id, jid.as_str());
    state.runtime.set_buffer_category(state, &buffer.id, Some(group.as_deref().unwrap_or(DEFAULT_CATEGORY)), 0);
    publish_presence(state, handle, jid);
    state.runtime.get_buffer(&buffer.id).unwrap_or(buffer)
}

/// One person as the window's member list wants them.
fn entry(handle: &Handle, jid: &BareJid) -> Value {
    let s = handle.session();
    let present = s.best_presence(jid);
    let status = present.map(|p| status_word(&p.show)).unwrap_or("offline");
    let text = present.map(|p| p.status.clone()).filter(|t| !t.is_empty());
    // Their own words first; else what they publish - a tune, a mood, an activity.
    let activity = text.map(|t| json!({ "kind": "custom", "text": t })).or_else(|| s.published.get(jid).and_then(|p| p.line())).unwrap_or(Value::Null);
    json!({
        "nick": s.shown_name(jid),
        "userId": jid.to_string(),
        "prefix": "",
        "away": status == "offline",
        "status": status,
        "activity": activity,
        "bot": false,
        "relationship": s.roster.get(jid).map(|c| c.subscription.clone()).unwrap_or_default(),
        "avatarUrl": s.avatars.get(jid).cloned().map(Value::String).unwrap_or(Value::Null),
        "roles": [],
    })
}

/// Tells the window who is in this conversation and whether they are here.
pub fn publish_presence(state: &AppState, handle: &Handle, jid: &BareJid) {
    let Some(buffer) = find_buffer(state, handle, jid) else { return };
    let own = handle.own();
    let own_entry = {
        let s = handle.session();
        json!({
            "nick": own.to_string(), "userId": own.to_string(), "prefix": "", "away": false,
            "status": if s.present { status_word(&s.own_show) } else { "offline" }, "activity": Value::Null,
            "bot": false, "relationship": "", "avatarUrl": Value::Null, "roles": [],
        })
    };
    let list = Value::Array(vec![entry(handle, jid), own_entry]);
    state.runtime.set_presence(&buffer.id, list.clone());
    state.runtime.set_presence_total(&buffer.id, 2);
    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list, "total": 2 }));
}

/// A `<presence>` from somebody.
pub async fn handle_presence(state: &AppState, handle: &Handle, p: &Presence) {
    let Some(from) = p.from.clone() else { return };
    let bare = from.to_bare();
    // A room's occupants are not contacts.
    if super::muc::is_room(handle, &bare) {
        return super::muc::handle_presence(state, handle, p).await;
    }
    match p.type_ {
        PresenceType::None | PresenceType::Unavailable => {
            if bare == handle.own() {
                return;
            }
            if let Some(x) = p.payloads.iter().find(|e| e.is("x", ns::VCARD_UPDATE)) {
                let (s, h, b) = (state.clone(), handle.clone(), bare.clone());
                let x = x.clone();
                tokio::spawn(async move { super::avatars::presence_hash(&s, &h, &b, &x).await });
            }
            let resource = from.resource().map(|r| r.to_string()).unwrap_or_default();
            {
                let mut s = handle.session();
                if p.type_ == PresenceType::Unavailable {
                    if let Some(rs) = s.presence.get_mut(&bare) {
                        rs.remove(&resource);
                        if rs.is_empty() {
                            s.presence.remove(&bare);
                        }
                    }
                } else {
                    let show = match p.show {
                        Some(Show::Away) => "away",
                        Some(Show::Xa) => "xa",
                        Some(Show::Dnd) => "dnd",
                        _ => "online",
                    };
                    let status = p.statuses.values().next().cloned().unwrap_or_default();
                    s.presence.entry(bare.clone()).or_default().insert(resource, Presence_ { show: show.into(), status, priority: p.priority.0 });
                }
            }
            publish_presence(state, handle, &bare);
        }
        PresenceType::Subscribe => {
            let status = p.statuses.values().next().cloned().unwrap_or_default();
            // Somebody we asked first, asking back: the same wish, answered by the one we made.
            let asked = handle.session().roster.get(&bare).is_some_and(|c| c.ask || c.we_see_them());
            if asked {
                answer_subscription(handle, &bare, true).await;
            } else {
                handle.session().requests.insert(bare, status);
                announce_requests(state, handle);
            }
        }
        PresenceType::Unsubscribe => {
            // They stopped seeing us; the roster push says so. Acknowledged, as the RFC asks.
            handle.send_presence(Presence::new(PresenceType::Unsubscribed).with_to(Jid::from(bare))).await;
        }
        _ => {}
    }
}

/// Says yes or no to somebody who asked to see this account's presence.
pub async fn answer_subscription(handle: &Handle, jid: &BareJid, accept: bool) {
    let kind = if accept { PresenceType::Subscribed } else { PresenceType::Unsubscribed };
    handle.send_presence(Presence::new(kind).with_to(Jid::from(jid.clone()))).await;
    handle.session().requests.remove(jid);
}

/// What is waiting to be answered, for the window's Invitations tile.
pub fn requests(handle: &Handle) -> Vec<Value> {
    handle.session().requests.iter().map(|(jid, text)| json!({ "jid": jid.to_string(), "text": text })).collect()
}

pub fn announce_requests(state: &AppState, handle: &Handle) {
    state.events.emit("xmppRequests", json!({ "accountId": handle.account_id, "requests": requests(handle) }));
}

/// Asks to see somebody's presence, putting them on the roster if they are not there. `name` and `group` are ours.
pub async fn add_contact(handle: &Handle, jid: &BareJid, name: Option<&str>, group: Option<&str>) -> Result<(), String> {
    set_item(handle, jid, name, group.map(|g| vec![g.to_string()])).await?;
    handle.send_presence(Presence::new(PresenceType::Subscribe).with_to(Jid::from(jid.clone()))).await;
    Ok(())
}

/// Changes a roster entry: its name, its groups (`None` leaves them alone).
pub async fn set_item(handle: &Handle, jid: &BareJid, name: Option<&str>, groups: Option<Vec<String>>) -> Result<(), String> {
    let mut item = Element::builder("item", ns::ROSTER).at("jid", jid.to_string());
    if let Some(n) = name {
        item = item.at("name", n);
    }
    let mut item = item.build();
    for g in groups.unwrap_or_else(|| handle.session().roster.get(jid).map(|c| c.groups.clone()).unwrap_or_default()) {
        item.append_child(Element::builder("group", ns::ROSTER).append(g).build());
    }
    let query = Element::builder("query", ns::ROSTER).append(item).build();
    handle.request(None, true, query).await.map(|_| ()).map_err(|e| e.words())
}

/// Takes somebody off the roster, which also ends the presence each sees of the other.
pub async fn remove_contact(handle: &Handle, jid: &BareJid) -> Result<(), String> {
    let item = Element::builder("item", ns::ROSTER).at("jid", jid.to_string()).at("subscription", "remove").build();
    let query = Element::builder("query", ns::ROSTER).append(item).build();
    handle.request(None, true, query).await.map(|_| ()).map_err(|e| e.words())
}

/// What the account shows: `online`, `away`, `xa` or `dnd`, and the words beside it.
pub async fn set_own(handle: &Handle, show: &str, status: &str) {
    {
        let mut s = handle.session();
        s.present = true;
        s.own_show = show.to_string();
        s.own_status = status.to_string();
    }
    handle.send_presence(own_presence(show, status)).await;
}

pub fn own_presence(show: &str, status: &str) -> Presence {
    let mut p = Presence::new(PresenceType::None);
    p.payloads.push(super::caps::presence_element());
    p.show = match show {
        "away" => Some(Show::Away),
        "xa" => Some(Show::Xa),
        "dnd" => Some(Show::Dnd),
        _ => None,
    };
    if !status.is_empty() {
        p.statuses.insert(Default::default(), status.to_string());
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_roster_query_is_read_with_its_names_groups_and_subscription_states() {
        let q = Element::from_str("<query xmlns='jabber:iq:roster' ver='7'><item jid='Bob@Example.org' name='Bob' subscription='both'><group>Friends</group><group>Work</group></item><item jid='carol@example.org' subscription='none' ask='subscribe'/><item jid='gone@example.org' subscription='remove'/></query>").unwrap();
        let items = parse_items(&q);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].0.as_str(), "bob@example.org");
        assert_eq!((items[0].1.name.as_deref(), items[0].1.subscription.as_str(), items[0].1.groups.clone()), (Some("Bob"), "both", vec!["Friends".to_string(), "Work".to_string()]));
        assert!(items[0].1.we_see_them());
        assert_eq!((items[1].1.name.clone(), items[1].1.ask, items[1].1.we_see_them()), (None, true, false));
        assert_eq!(items[2].1.subscription, "remove");
    }

    #[test]
    fn what_a_resource_shows_becomes_the_words_the_window_draws() {
        assert_eq!(status_word("online"), "online");
        assert_eq!(status_word("away"), "idle");
        assert_eq!(status_word("xa"), "idle");
        assert_eq!(status_word("dnd"), "dnd");
        assert_eq!(status_word(""), "offline");
    }

    #[test]
    fn own_presence_carries_the_show_and_the_words_only_when_there_are_any() {
        let p = own_presence("dnd", "in a meeting");
        assert_eq!((p.show, p.statuses.values().next().cloned()), (Some(Show::Dnd), Some("in a meeting".to_string())));
        let q = own_presence("online", "");
        assert_eq!((q.show, q.statuses.len()), (None, 0));
    }
}
