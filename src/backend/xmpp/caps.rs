//! What this client says it can do (service discovery, XEP-0030) and the hash of it that rides on presence so that the
//! people we talk to need not ask (entity capabilities, XEP-0115).
//!
//! The list is what is actually built, and only that: a feature advertised and not honoured is worse than one left out,
//! because other clients rely on it (a peer that sees receipts advertised waits for one). It grows with the milestones.

use super::ns;
use super::xml::Attr;
use base64::Engine;
use sha1::{Digest, Sha1};
use tokio_xmpp::minidom::Element;

/// Where this client's capabilities are named. The node is a name, not an address anything is fetched from.
pub const NODE: &str = "https://github.com/Moho-Chat/moho";
pub const NAME: &str = "moho";

/// Every feature this client answers for, as the namespaces service discovery names them.
pub const FEATURES: &[&str] = &[
    ns::DISCO_INFO,
    ns::DISCO_ITEMS,
    ns::CAPS,
    ns::PING,
    ns::VERSION,
    ns::TIME,
    ns::CARBONS,
    ns::RECEIPTS,
    ns::MARKERS,
    ns::CHATSTATES,
    ns::CORRECT,
    ns::RETRACT,
    ns::REACTIONS,
    ns::REPLY,
    ns::FALLBACK,
    ns::HINTS,
    ns::SID,
    ns::SFS,
    ns::SPOILER,
    ns::REFERENCE,
    "urn:xmpp:hats:0",
    "urn:xmpp:avatar:metadata+notify",
    "urn:xmpp:mds:displayed:0+notify",
    "eu.siacs.conversations.axolotl.devicelist+notify",
    "eu.siacs.conversations.axolotl",
    "urn:xmpp:bookmarks:1+notify",
    ns::OOB,
    ns::DELAY,
    "urn:xmpp:csi:0",
];

fn sorted_features() -> Vec<&'static str> {
    let mut f = FEATURES.to_vec();
    f.extend_from_slice(super::calls::FEATURES);
    f.extend_from_slice(super::pep::NOTIFY);
    f.extend_from_slice(super::filetransfer::FEATURES);
    f.sort_unstable();
    f.dedup();
    f
}

/// The verification string of XEP-0115 section 5.1: one identity and the features, sorted, each ended with `<`.
pub fn ver() -> String {
    let mut s = format!("client/pc//{NAME}<");
    for f in sorted_features() {
        s.push_str(f);
        s.push('<');
    }
    base64::engine::general_purpose::STANDARD.encode(Sha1::digest(s.as_bytes()))
}

/// The `<c/>` that goes on presence.
pub fn presence_element() -> Element {
    Element::builder("c", ns::CAPS).at("hash", "sha-1").at("node", NODE).at("ver", ver()).build()
}

/// The answer to a disco#info query for this client. `None` for a node that is not ours.
pub fn disco_info(node: Option<&str>) -> Option<Element> {
    let ours = format!("{NODE}#{}", ver());
    if node.is_some_and(|n| n != ours && n != NODE) {
        return None;
    }
    let mut q = Element::builder("query", ns::DISCO_INFO);
    if let Some(n) = node {
        q = q.at("node", n);
    }
    let mut q = q.build();
    q.append_child(Element::builder("identity", ns::DISCO_INFO).at("category", "client").at("type", "pc").at("name", NAME).build());
    for f in sorted_features() {
        q.append_child(Element::builder("feature", ns::DISCO_INFO).at("var", f).build());
    }
    Some(q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_the_same_every_time_and_covers_the_whole_list() {
        assert_eq!(ver(), ver());
        assert_eq!(ver().len(), 28, "a base64 SHA-1");
        let q = disco_info(None).unwrap();
        assert_eq!(q.children().filter(|c| c.name() == "feature").count(), sorted_features().len());
        assert_eq!(q.children().filter(|c| c.name() == "identity").count(), 1);
    }

    #[test]
    fn a_query_for_a_node_that_is_not_ours_is_not_answered_as_if_it_were() {
        assert!(disco_info(Some(&format!("{NODE}#{}", ver()))).is_some());
        assert!(disco_info(Some("urn:other:node")).is_none());
    }

    #[test]
    fn the_hash_follows_the_algorithm_of_the_specification() {
        // The example of XEP-0115 5.2 (one identity, a short list), to be sure the string is put together right.
        let s = "client/pc//Exodus 0.9.1<http://jabber.org/protocol/caps<http://jabber.org/protocol/disco#info<http://jabber.org/protocol/disco#items<http://jabber.org/protocol/muc<";
        assert_eq!(base64::engine::general_purpose::STANDARD.encode(Sha1::digest(s.as_bytes())), "QgayPKawpkPSDYmwT/WM94uAlu0=");
    }
}
