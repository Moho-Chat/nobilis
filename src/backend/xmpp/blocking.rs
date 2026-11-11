//! Blocking (XEP-0191): people (and whole servers) whose messages the server is told not to deliver, kept on the account so
//! every client has the same list. A change made elsewhere arrives as a push and is applied.

use super::session::Handle;
use super::xml::Attr;
use tokio_xmpp::minidom::Element;

pub const BLOCKING: &str = "urn:xmpp:blocking";

/// The addresses in a `<blocklist/>`, `<block/>` or `<unblock/>`.
pub fn items(e: &Element) -> Vec<String> {
    e.children().filter(|c| c.name() == "item").filter_map(|c| c.attr("jid").map(str::to_string)).collect()
}

pub async fn load(handle: &Handle) {
    if let Ok(Some(list)) = handle.request(None, false, super::xml::empty("blocklist", BLOCKING)).await {
        handle.session().blocked = items(&list).into_iter().collect();
    }
}

/// Blocks or unblocks an address (a person, or a domain).
pub async fn set(handle: &Handle, jid: &str, block: bool) -> Result<(), String> {
    let item = Element::builder("item", BLOCKING).at("jid", jid).build();
    let q = Element::builder(if block { "block" } else { "unblock" }, BLOCKING).append(item).build();
    handle.request(None, true, q).await.map(|_| ()).map_err(|e| e.words())?;
    let mut s = handle.session();
    if block {
        s.blocked.insert(jid.to_string());
    } else {
        s.blocked.remove(jid);
    }
    Ok(())
}

/// A push from the server: somebody (maybe another of this account's clients) changed the list.
pub fn pushed(handle: &Handle, e: &Element) {
    let mut s = handle.session();
    match e.name() {
        "block" => s.blocked.extend(items(e)),
        "unblock" => {
            let list = items(e);
            if list.is_empty() {
                s.blocked.clear();
            }
            for j in list {
                s.blocked.remove(&j);
            }
        }
        _ => {}
    }
}

/// Whether a sender is blocked: their address, their bare address, or their server.
pub fn is_blocked(handle: &Handle, from: &tokio_xmpp::jid::Jid) -> bool {
    let s = handle.session();
    s.blocked.contains(&from.to_string()) || s.blocked.contains(from.to_bare().as_str()) || s.blocked.contains(from.domain().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_list_is_the_addresses_of_its_items() {
        let e = Element::from_str("<blocklist xmlns='urn:xmpp:blocking'><item jid='a@example.org'/><item jid='spam.example'/></blocklist>").unwrap();
        assert_eq!(items(&e), vec!["a@example.org", "spam.example"]);
    }
}
