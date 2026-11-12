//! Reading on one device is reading on all of them (XEP-0490): when a conversation is read here, the id of the last message
//! seen is published on the account (PEP); when another device does, the notification arrives and the conversation is read here
//! too, with its unread count cleared.

use super::session::Handle;
use super::xml::Attr;
use super::{muc, ns, roster};
use crate::state::AppState;
use serde_json::json;
use tokio_xmpp::jid::BareJid;
use tokio_xmpp::minidom::Element;

pub const NODE: &str = "urn:xmpp:mds:displayed:0";

fn item(conversation: &str, by: &str, stanza_id: &str) -> Element {
    let displayed = Element::builder("displayed", NODE).append(Element::builder("stanza-id", ns::SID).at("by", by).at("id", stanza_id).build()).build();
    Element::builder("item", ns::PUBSUB).at("id", conversation).append(displayed).build()
}

/// The stanza id and who stamped it, from an item.
pub fn read_item(item: &Element) -> Option<(String, String, String)> {
    let conversation = item.attr("id")?.to_string();
    let sid = item.children().find(|c| c.is("displayed", NODE))?.children().find(|c| c.is("stanza-id", ns::SID))?;
    Some((conversation, sid.attr("by")?.to_string(), sid.attr("id")?.to_string()))
}

/// Says that a conversation was read up to the message with this archive id.
pub async fn publish(handle: &Handle, conversation: &str, by: &str, stanza_id: &str) {
    let publish = Element::builder("publish", ns::PUBSUB).at("node", NODE).append(item(conversation, by, stanza_id)).build();
    let mut options = Element::builder("x", ns::DATA).at("type", "submit").build();
    for (var, value) in [("FORM_TYPE", "http://jabber.org/protocol/pubsub#publish-options"), ("pubsub#persist_items", "true"), ("pubsub#max_items", "max"), ("pubsub#access_model", "whitelist")] {
        let mut f = Element::builder("field", ns::DATA).at("var", var).build();
        f.append_child(Element::builder("value", ns::DATA).append(value).build());
        options.append_child(f);
    }
    let q = Element::builder("pubsub", ns::PUBSUB).append(publish).append(Element::builder("publish-options", ns::PUBSUB).append(options).build()).build();
    if let Err(e) = handle.request(None, true, q).await {
        tracing::debug!("xmpp[{}]: telling the account's other devices what was read: {e}", handle.account_id);
    }
}

/// Another device read a conversation: here it is read too, if what it read is something this device has.
pub fn apply(state: &AppState, handle: &Handle, item: &Element) {
    let Some((conversation, _by, stanza_id)) = read_item(item) else { return };
    let Ok(jid) = conversation.parse::<BareJid>() else { return };
    let known = {
        let s = handle.session();
        let local = s.aliases.get(&stanza_id).cloned();
        local.is_some_and(|l| s.last_incoming.get(&conversation).is_none_or(|last| *last == l || s.aliases.get(last).is_none_or(|x| *x == l)))
    };
    // Whatever the other device read, a conversation this device knows nothing newer than is read.
    let buffer = if muc::is_room(handle, &jid) { muc::find_buffer(state, handle, &jid) } else { roster::find_buffer(state, handle, &jid) };
    if let (true, Some(b)) = (known, buffer) {
        state.events.emit("bufferRead", json!({ "accountId": handle.account_id, "bufferId": b.id }));
    }
}

/// After connecting: what the account's other devices have read.
pub async fn load(state: &AppState, handle: &Handle) {
    let items = Element::builder("items", ns::PUBSUB).at("node", NODE).build();
    let q = Element::builder("pubsub", ns::PUBSUB).append(items).build();
    if let Ok(Some(ps)) = handle.request(None, false, q).await {
        for i in ps.children().filter(|c| c.name() == "items").flat_map(|i| i.children()).filter(|c| c.name() == "item") {
            apply(state, handle, i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn an_item_names_the_conversation_and_the_message_read_up_to() {
        let e = Element::from_str("<item xmlns='http://jabber.org/protocol/pubsub' id='bob@example.org'><displayed xmlns='urn:xmpp:mds:displayed:0'><stanza-id xmlns='urn:xmpp:sid:0' by='alice@example.org' id='A42'/></displayed></item>").unwrap();
        assert_eq!(read_item(&e), Some(("bob@example.org".into(), "alice@example.org".into(), "A42".into())));
        let built = item("bob@example.org", "alice@example.org", "A42");
        assert_eq!(read_item(&built), read_item(&e));
    }
}
