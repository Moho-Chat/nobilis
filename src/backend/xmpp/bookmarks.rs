//! The rooms an account keeps (XEP-0402): a list on the account itself, so every client of the account has the same rooms
//! and each opens them on connecting.
//!
//! Stored in PEP at `urn:xmpp:bookmarks:1`, one item per room, the item's id the room's address. Servers that do not have
//! it keep the older list (XEP-0048, a `storage:bookmarks` document in private XML storage); that is read, and written back
//! whole, when the newer one is not there. A change made by another client arrives as a PEP notification and is applied.

use super::session::Handle;
use super::xml::Attr;
use super::{muc, ns, xml};
use crate::state::AppState;
use tokio_xmpp::jid::BareJid;
use tokio_xmpp::minidom::Element;

pub const BOOKMARKS: &str = "urn:xmpp:bookmarks:1";
pub const STORAGE: &str = "storage:bookmarks";

/// One bookmarked room.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bookmark {
    pub room: String,
    pub name: Option<String>,
    pub autojoin: bool,
    pub nick: Option<String>,
    pub password: Option<String>,
}

/// The bookmarks in a pubsub `<items>` of the new form.
pub fn parse_items(items: &Element) -> Vec<Bookmark> {
    items
        .children()
        .filter(|i| i.name() == "item")
        .filter_map(|item| {
            let room = item.attr("id")?.to_string();
            let c = item.children().find(|c| c.is("conference", BOOKMARKS))?;
            Some(Bookmark {
                room,
                name: c.attr("name").map(str::to_string).filter(|n| !n.is_empty()),
                autojoin: matches!(c.attr("autojoin"), Some("true" | "1")),
                nick: c.children().find(|n| n.name() == "nick").map(|n| n.text()).filter(|n| !n.is_empty()),
                password: c.children().find(|n| n.name() == "password").map(|n| n.text()).filter(|n| !n.is_empty()),
            })
        })
        .collect()
}

/// The bookmarks in a legacy `<storage xmlns='storage:bookmarks'>`.
pub fn parse_storage(storage: &Element) -> Vec<Bookmark> {
    storage
        .children()
        .filter(|c| c.name() == "conference")
        .filter_map(|c| {
            Some(Bookmark {
                room: c.attr("jid")?.to_string(),
                name: c.attr("name").map(str::to_string).filter(|n| !n.is_empty()),
                autojoin: matches!(c.attr("autojoin"), Some("true" | "1")),
                nick: c.children().find(|n| n.name() == "nick").map(|n| n.text()).filter(|n| !n.is_empty()),
                password: c.children().find(|n| n.name() == "password").map(|n| n.text()).filter(|n| !n.is_empty()),
            })
        })
        .collect()
}

fn conference(b: &Bookmark) -> Element {
    let mut c = Element::builder("conference", BOOKMARKS).at("autojoin", if b.autojoin { "true" } else { "false" });
    if let Some(n) = &b.name {
        c = c.at("name", n.as_str());
    }
    let mut c = c.build();
    if let Some(n) = &b.nick {
        c.append_child(Element::builder("nick", BOOKMARKS).append(n.clone()).build());
    }
    if let Some(p) = &b.password {
        c.append_child(Element::builder("password", BOOKMARKS).append(p.clone()).build());
    }
    c
}

/// Reads the account's bookmarks, joins the ones marked to be, and keeps what it read.
pub async fn load(state: &AppState, handle: &Handle) {
    let items = Element::builder("items", ns::PUBSUB).at("node", BOOKMARKS).build();
    let query = Element::builder("pubsub", ns::PUBSUB).append(items).build();
    let list = match handle.request(None, false, query).await {
        Ok(Some(ps)) => ps.children().find(|c| c.name() == "items").map(parse_items),
        _ => None,
    };
    let list = match list {
        Some(l) => l,
        None => legacy(handle).await.unwrap_or_default(),
    };
    tracing::info!("xmpp[{}]: {} bookmarked rooms", handle.account_id, list.len());
    apply(state, handle, list).await;
}

async fn legacy(handle: &Handle) -> Option<Vec<Bookmark>> {
    let private = Element::builder("query", "jabber:iq:private").append(xml::empty("storage", STORAGE)).build();
    let q = handle.request(None, false, private).await.ok()??;
    q.children().find(|c| c.is("storage", STORAGE)).map(parse_storage)
}

/// Makes the rooms of a list conversations, and joins the ones marked.
async fn apply(state: &AppState, handle: &Handle, list: Vec<Bookmark>) {
    for b in list {
        let Ok(room) = b.room.parse::<BareJid>() else { continue };
        {
            let mut s = handle.session();
            let entry = s.rooms.entry(room.clone()).or_default();
            if entry.name.is_none() {
                entry.name = b.name.clone();
            }
            if entry.nick.is_empty() {
                entry.nick = b.nick.clone().unwrap_or_default();
            }
            if entry.password.is_none() {
                entry.password = b.password.clone();
            }
            // A bookmarked room that is not joined is a conversation to read, not a room the account is in.
            let joined = entry.joined;
            let keep = b.autojoin || joined;
            if !keep {
                s.rooms.remove(&room);
            }
        }
        let buffer = muc::ensure_buffer(state, handle, &room);
        if b.autojoin {
            let _ = muc::join(state, handle, &room, b.nick.as_deref(), b.password.as_deref()).await;
        } else {
            state.runtime.set_buffer_read_only(state, &buffer.id, Some("You are not in this room"));
        }
    }
}

/// Saves a room in the account's bookmarks (or updates it).
pub async fn save(handle: &Handle, b: &Bookmark) -> Result<(), String> {
    let item = Element::builder("item", ns::PUBSUB).at("id", b.room.as_str()).append(conference(b)).build();
    let publish = Element::builder("publish", ns::PUBSUB).at("node", BOOKMARKS).append(item).build();
    let mut options = Element::builder("publish-options", ns::PUBSUB).build();
    let mut form = Element::builder("x", ns::DATA).at("type", "submit").build();
    for (var, value) in [("FORM_TYPE", "http://jabber.org/protocol/pubsub#publish-options"), ("pubsub#persist_items", "true"), ("pubsub#max_items", "max"), ("pubsub#send_last_published_item", "never"), ("pubsub#access_model", "whitelist")] {
        let mut f = Element::builder("field", ns::DATA).at("var", var).build();
        f.append_child(Element::builder("value", ns::DATA).append(value).build());
        form.append_child(f);
    }
    options.append_child(form);
    let query = Element::builder("pubsub", ns::PUBSUB).append(publish).append(options).build();
    handle.request(None, true, query).await.map(|_| ()).map_err(|e| e.words())
}

/// Takes a room out of the bookmarks.
pub async fn remove(handle: &Handle, room: &str) -> Result<(), String> {
    let item = Element::builder("item", ns::PUBSUB).at("id", room).build();
    let retract = Element::builder("retract", ns::PUBSUB).at("node", BOOKMARKS).at("notify", "true").append(item).build();
    let query = Element::builder("pubsub", ns::PUBSUB).append(retract).build();
    handle.request(None, true, query).await.map(|_| ()).map_err(|e| e.words())
}

/// A change another client made to the list, as a notification (`<event>` with `<items>` of our node).
pub async fn notified(state: &AppState, handle: &Handle, event: &Element) {
    let Some(items) = event.children().find(|c| c.name() == "items" && c.attr("node") == Some(BOOKMARKS)) else { return };
    let gone: Vec<String> = items.children().filter(|c| c.name() == "retract").filter_map(|r| r.attr("id").map(str::to_string)).collect();
    for room in gone {
        if let Ok(room) = room.parse::<BareJid>() {
            if muc::is_room(handle, &room) {
                muc::leave(state, handle, &room).await;
            }
        }
    }
    apply(state, handle, parse_items(items)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn the_new_form_names_the_room_by_the_items_id_and_the_rest_by_attributes_and_children() {
        let items = Element::from_str("<items xmlns='http://jabber.org/protocol/pubsub' node='urn:xmpp:bookmarks:1'><item id='room@muc.example.org'><conference xmlns='urn:xmpp:bookmarks:1' name='The room' autojoin='true'><nick>alice</nick><password>pw</password></conference></item><item id='b@muc.example.org'><conference xmlns='urn:xmpp:bookmarks:1'/></item></items>").unwrap();
        let list = parse_items(&items);
        assert_eq!(list[0], Bookmark { room: "room@muc.example.org".into(), name: Some("The room".into()), autojoin: true, nick: Some("alice".into()), password: Some("pw".into()) });
        assert_eq!((list[1].room.as_str(), list[1].autojoin, list[1].nick.clone()), ("b@muc.example.org", false, None));
    }

    #[test]
    fn the_old_form_keeps_the_address_in_an_attribute() {
        let storage = Element::from_str("<storage xmlns='storage:bookmarks'><conference name='Old' autojoin='1' jid='old@conference.example.org'><nick>me</nick></conference><url name='x' url='http://e.org'/></storage>").unwrap();
        let list = parse_storage(&storage);
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].room.as_str(), list[0].autojoin, list[0].nick.as_deref()), ("old@conference.example.org", true, Some("me")));
    }

    #[test]
    fn a_bookmark_is_written_in_the_form_the_other_clients_read() {
        let c = conference(&Bookmark { room: "r@m.example.org".into(), name: Some("R".into()), autojoin: true, nick: Some("n".into()), password: None });
        assert_eq!((c.attr("name"), c.attr("autojoin")), (Some("R"), Some("true")));
        assert_eq!(c.children().map(|n| n.name().to_string()).collect::<Vec<_>>(), vec!["nick"]);
    }
}
