//! What a person does to a room, and to the people in it: a subject, a nickname, who may speak, who may not come in,
//! the room's settings, an invitation. Each is one iq or one stanza, and each can be refused, and then the room says why.

use super::dataform::{self, Form};
use super::muc::{MUC_ADMIN, MUC_OWNER};
use super::session::Handle;
use super::xml::Attr;
use super::{ns, xml};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::message::Message;
use tokio_xmpp::parsers::presence::{Presence, Type as PresenceType};

fn room_jid(room: &BareJid) -> Jid {
    Jid::from(room.clone())
}

/// A role (`moderator`, `participant`, `visitor`, `none` to remove) for the occupant with this nickname.
pub async fn set_role(handle: &Handle, room: &BareJid, nick: &str, role: &str, reason: &str) -> Result<(), String> {
    item(handle, room, Element::builder("item", MUC_ADMIN).at("nick", nick).at("role", role), reason).await
}

/// An affiliation (`owner`, `admin`, `member`, `outcast` to ban, `none`) for an address.
pub async fn set_affiliation(handle: &Handle, room: &BareJid, jid: &BareJid, affiliation: &str, reason: &str) -> Result<(), String> {
    item(handle, room, Element::builder("item", MUC_ADMIN).at("jid", jid.to_string()).at("affiliation", affiliation), reason).await
}

async fn item(handle: &Handle, room: &BareJid, builder: tokio_xmpp::minidom::ElementBuilder, reason: &str) -> Result<(), String> {
    let mut item = builder.build();
    if !reason.is_empty() {
        item.append_child(Element::builder("reason", MUC_ADMIN).append(reason).build());
    }
    let query = Element::builder("query", MUC_ADMIN).append(item).build();
    handle.request(Some(room_jid(room)), true, query).await.map(|_| ()).map_err(|e| e.words())
}

/// Who holds an affiliation in the room: (address, nickname if shown, reason).
pub async fn affiliates(handle: &Handle, room: &BareJid, affiliation: &str) -> Result<Vec<(String, String)>, String> {
    let query = Element::builder("query", MUC_ADMIN).append(Element::builder("item", MUC_ADMIN).at("affiliation", affiliation).build()).build();
    let q = handle.request(Some(room_jid(room)), false, query).await.map_err(|e| e.words())?;
    Ok(q.map(|q| q.children().filter(|c| c.name() == "item").filter_map(|c| Some((c.attr("jid")?.to_string(), c.children().find(|r| r.name() == "reason").map(|r| r.text()).unwrap_or_default()))).collect()).unwrap_or_default())
}

/// Sets the room's subject.
pub async fn set_subject(handle: &Handle, room: &BareJid, subject: &str) {
    let mut m = Message::groupchat(room_jid(room));
    m.subjects.insert(Default::default(), subject.to_string());
    handle.send_message(m).await;
}

/// Asks for another nickname: a presence to the room under the new one.
pub async fn change_nick(handle: &Handle, room: &BareJid, nick: &str) -> Result<(), String> {
    let to: Jid = format!("{room}/{nick}").parse().map_err(|_| format!("{nick:?} is not a usable nickname"))?;
    handle.send_presence(Presence::new(PresenceType::None).with_to(to)).await;
    Ok(())
}

/// Invites somebody to a room: directly (XEP-0249), which works whether or not the room is members-only and does not
/// depend on the room's service.
pub async fn invite(handle: &Handle, room: &BareJid, who: &BareJid, reason: &str, password: Option<&str>) {
    let mut x = Element::builder("x", "jabber:x:conference").at("jid", room.to_string());
    if !reason.is_empty() {
        x = x.at("reason", reason);
    }
    if let Some(p) = password {
        x = x.at("password", p);
    }
    let mut m = Message::normal(Jid::from(who.clone()));
    m.payloads.push(x.build());
    handle.send_message(m).await;
}

/// The room's settings form, for its owner.
pub async fn config(handle: &Handle, room: &BareJid) -> Result<Form, String> {
    let q = super::muc::config_form(handle, room).await.map_err(|e| e.words())?;
    let x = q.as_ref().and_then(|q| xml::child(q, "x", ns::DATA)).ok_or("the room has no settings to change, or this account may not change them")?;
    Ok(dataform::parse(x))
}

/// Sends the settings back.
pub async fn set_config(handle: &Handle, room: &BareJid, form: &Form, answers: &serde_json::Value) -> Result<(), String> {
    let query = Element::builder("query", MUC_OWNER).append(dataform::submit(form, answers)).build();
    handle.request(Some(room_jid(room)), true, query).await.map(|_| ()).map_err(|e| e.words())
}

/// Removes somebody else's message from a room, for everyone (XEP-0425). `stanza_id` is the room's own id for it.
pub async fn moderate(handle: &Handle, room: &BareJid, stanza_id: &str, reason: &str) -> Result<(), String> {
    let mut m = Element::builder("moderate", ns::MODERATED).at("id", stanza_id).build();
    m.append_child(xml::empty("retract", ns::RETRACT));
    if !reason.is_empty() {
        m.append_child(Element::builder("reason", ns::MODERATED).append(reason).build());
    }
    handle.request(Some(room_jid(room)), true, m).await.map(|_| ()).map_err(|e| e.words())
}

/// Takes the room away (owners only).
pub async fn destroy(handle: &Handle, room: &BareJid, reason: &str) -> Result<(), String> {
    let mut d = Element::builder("destroy", MUC_OWNER).build();
    if !reason.is_empty() {
        d.append_child(Element::builder("reason", MUC_OWNER).append(reason).build());
    }
    let query = Element::builder("query", MUC_OWNER).append(d).build();
    handle.request(Some(room_jid(room)), true, query).await.map(|_| ()).map_err(|e| e.words())
}

