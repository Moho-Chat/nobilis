//! What this account publishes (PEP, XEP-0163, over pubsub's owner use case, XEP-0060): each node on the account itself, who may
//! read it, how many items it holds - changed, emptied or deleted from the account's own page. Nodes moho itself keeps (the
//! bookmarks, the OMEMO keys, the avatar...) are named as such, so somebody deleting one knows what it does.

use super::dataform::{self, Form};
use super::session::Handle;
use super::xml::Attr;
use serde::Serialize;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;

pub const PUBSUB: &str = "http://jabber.org/protocol/pubsub";
pub const OWNER: &str = "http://jabber.org/protocol/pubsub#owner";

/// One node, as the page lists it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Node {
    pub node: String,
    /// What it is, in words, when it is one the common clients keep.
    pub what: String,
    /// `open`, `presence`, `roster`, `whitelist`, `authorize`; empty when the server did not say.
    pub access: String,
    pub items: usize,
    /// moho reads or writes it: deleting it changes how this client behaves.
    pub used_here: bool,
}

/// What a node is, for the ones the common clients keep, and whether moho uses it.
pub fn describe(node: &str) -> (&'static str, bool) {
    match node {
        "urn:xmpp:bookmarks:1" => ("Your rooms (bookmarks)", true),
        "storage:bookmarks" => ("Your rooms, the older list", false),
        "urn:xmpp:avatar:data" => ("Your picture", true),
        "urn:xmpp:avatar:metadata" => ("Your picture: which one is current", true),
        "urn:xmpp:vcard4" => ("Your profile (vCard)", true),
        "http://jabber.org/protocol/nick" => ("Your nickname", true),
        "urn:xmpp:mds:displayed:0" => ("What you have read, for your other devices", true),
        "eu.siacs.conversations.axolotl.devicelist" => ("Your encryption devices (OMEMO)", true),
        "urn:xmpp:omemo:2:devices" => ("Your encryption devices (OMEMO 2)", true),
        "urn:xmpp:omemo:2:bundles" => ("Your encryption keys (OMEMO 2)", true),
        "http://jabber.org/protocol/mood" => ("Your mood", false),
        "http://jabber.org/protocol/activity" => ("What you are doing", false),
        "http://jabber.org/protocol/tune" => ("What you are listening to", false),
        "http://jabber.org/protocol/geoloc" => ("Where you are", false),
        "urn:xmpp:openpgp:0:public-keys" => ("Your OpenPGP keys", false),
        n if n.starts_with("eu.siacs.conversations.axolotl.bundles:") => ("An encryption key bundle (OMEMO)", true),
        n if n.starts_with("urn:xmpp:openpgp:0:public-keys:") => ("An OpenPGP key", false),
        _ => ("", false),
    }
}

fn me(handle: &Handle) -> Option<Jid> {
    handle.own().as_str().parse().ok()
}

/// The nodes on the account, with who may read each and how many items it holds.
pub async fn list(handle: &Handle) -> Result<Vec<Node>, String> {
    let to = me(handle);
    let answer = handle.request(to.clone(), false, super::xml::empty("query", super::ns::DISCO_ITEMS)).await.map_err(|e| e.words())?;
    let names: Vec<String> = answer.map(|a| a.children().filter(|c| c.name() == "item").filter_map(|c| c.attr("node").map(str::to_string)).collect()).unwrap_or_default();
    let asks = names.into_iter().map(|node| {
        let (handle, to) = (handle.clone(), to.clone());
        async move {
            let access = config(&handle, &node).await.ok().flatten().and_then(|f| f.fields.into_iter().find(|f| f.var == "pubsub#access_model").and_then(|f| f.values.into_iter().next())).unwrap_or_default();
            let items_q = Element::builder("pubsub", PUBSUB).append(Element::builder("items", PUBSUB).at("node", node.as_str()).build()).build();
            let items = handle.request(to, false, items_q).await.ok().flatten().and_then(|p| p.children().find(|c| c.name() == "items").map(|i| i.children().filter(|c| c.name() == "item").count())).unwrap_or(0);
            let (what, used_here) = describe(&node);
            Node { what: what.to_string(), node, access, items, used_here }
        }
    });
    let mut nodes = futures::future::join_all(asks).await;
    nodes.sort_by(|a, b| (a.what.is_empty(), &a.node).cmp(&(b.what.is_empty(), &b.node)));
    Ok(nodes)
}

/// A node's configuration form, as the server gives it to its owner.
pub async fn config(handle: &Handle, node: &str) -> Result<Option<Form>, String> {
    let q = Element::builder("pubsub", OWNER).append(Element::builder("configure", OWNER).at("node", node).build()).build();
    let answer = handle.request(None, false, q).await.map_err(|e| e.words())?;
    Ok(answer.and_then(|p| p.children().find(|c| c.name() == "configure").and_then(|c| c.children().find(|x| x.is("x", super::ns::DATA)).map(dataform::parse))))
}

/// Answers a node's configuration form.
pub async fn configure(handle: &Handle, node: &str, form: &Form, answers: &serde_json::Value) -> Result<(), String> {
    let q = Element::builder("pubsub", OWNER).append(Element::builder("configure", OWNER).at("node", node).append(dataform::submit(form, answers)).build()).build();
    handle.request(None, true, q).await.map(|_| ()).map_err(|e| e.words())
}

/// Who may read a node: the one setting most people want, changed without the whole form.
pub async fn set_access(handle: &Handle, node: &str, access: &str) -> Result<(), String> {
    let form = config(handle, node).await?.ok_or("the server has no settings for that node")?;
    configure(handle, node, &form, &serde_json::json!({ "pubsub#access_model": access })).await
}

/// Takes every item out of a node, or the node itself away.
pub async fn remove(handle: &Handle, node: &str, delete: bool) -> Result<(), String> {
    let what = if delete { "delete" } else { "purge" };
    let q = Element::builder("pubsub", OWNER).append(Element::builder(what, OWNER).at("node", node).build()).build();
    handle.request(None, true, q).await.map(|_| ()).map_err(|e| e.words())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nodes_clients_keep_are_named_and_moho_s_own_are_marked() {
        assert_eq!(describe("urn:xmpp:bookmarks:1"), ("Your rooms (bookmarks)", true));
        assert!(describe("eu.siacs.conversations.axolotl.bundles:12345").1);
        assert_eq!(describe("http://jabber.org/protocol/tune"), ("What you are listening to", false));
        assert_eq!(describe("something.else"), ("", false));
    }
}
