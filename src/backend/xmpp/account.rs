//! The account on the server: its password, its removal, and what the server it lives on says about itself.

use super::register::REGISTER;
use super::session::Handle;
use super::{ns, xml};
use serde_json::{json, Value};
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;

/// Changes the password (XEP-0077). The server checks that it is this account asking: the connection is the proof.
pub async fn change_password(handle: &Handle, new: &str) -> Result<(), String> {
    if new.is_empty() {
        return Err("A password cannot be empty".to_string());
    }
    let username = handle.own().node().map(|n| n.to_string()).unwrap_or_default();
    let mut q = Element::builder("query", REGISTER).build();
    q.append_child(Element::builder("username", REGISTER).append(username).build());
    q.append_child(Element::builder("password", REGISTER).append(new).build());
    handle.request(None, true, q).await.map(|_| ()).map_err(|e| e.words())
}

/// Removes the account from the server, for good.
pub async fn delete(handle: &Handle) -> Result<(), String> {
    let q = Element::builder("query", REGISTER).append(xml::empty("remove", REGISTER)).build();
    handle.request(None, true, q).await.map(|_| ()).map_err(|e| e.words())
}

/// What a server says about itself: its software, the contact addresses its operators give, and what it can do.
pub async fn server_info(handle: &Handle) -> Result<Value, String> {
    let domain: Jid = handle.own().domain().as_str().parse().map_err(|_| "the server's address is not usable".to_string())?;
    let version = handle.request(Some(domain.clone()), false, xml::empty("query", ns::VERSION)).await.ok().flatten();
    let (name, v) = version.map(|q| (q.children().find(|c| c.name() == "name").map(|c| c.text()), q.children().find(|c| c.name() == "version").map(|c| c.text()))).unwrap_or((None, None));
    let info = handle.request(Some(domain), false, xml::empty("query", ns::DISCO_INFO)).await.map_err(|e| e.words())?;
    let mut contacts = serde_json::Map::new();
    let mut features = Vec::new();
    if let Some(q) = &info {
        features = q.children().filter(|c| c.name() == "feature").filter_map(|c| c.attr("var").map(str::to_string)).collect();
        for x in q.children().filter(|c| c.is("x", ns::DATA)) {
            let form = super::dataform::parse(x);
            if form.fields.iter().any(|f| f.var == "FORM_TYPE" && f.values.iter().any(|v| v == "http://jabber.org/network/serverinfo")) {
                for f in form.fields.iter().filter(|f| f.var.ends_with("-addresses")) {
                    contacts.insert(f.var.trim_end_matches("-addresses").to_string(), json!(f.values));
                }
            }
        }
    }
    features.sort();
    let named: Vec<Value> = features.iter().map(|f| json!({ "var": f, "xep": xep_of(f) })).collect();
    Ok(json!({ "software": name, "version": v, "contacts": contacts, "features": named }))
}

/// The number of the specification a namespace is from, where it is one this client knows by name.
pub fn xep_of(namespace: &str) -> Option<&'static str> {
    Some(match namespace {
        "urn:xmpp:mam:2" => "XEP-0313 message archive",
        "urn:xmpp:carbons:2" => "XEP-0280 carbons",
        "urn:xmpp:sm:3" => "XEP-0198 stream management",
        "urn:xmpp:csi:0" => "XEP-0352 client state indication",
        "urn:xmpp:http:upload:0" => "XEP-0363 file upload",
        "urn:xmpp:blocking" => "XEP-0191 blocking",
        "urn:xmpp:bookmarks:1" | "urn:xmpp:bookmarks:1#compat" => "XEP-0402 bookmarks",
        "urn:xmpp:sasl:2" => "XEP-0388 SASL2",
        "urn:xmpp:bind:0" => "XEP-0386 bind 2",
        "urn:xmpp:fast:0" => "XEP-0484 fast",
        "urn:xmpp:push:0" => "XEP-0357 push",
        "jabber:iq:register" => "XEP-0077 registration",
        "urn:xmpp:extdisco:2" => "XEP-0215 external services",
        "urn:xmpp:avatar:metadata" | "urn:xmpp:avatar:data" => "XEP-0084 avatars",
        "http://jabber.org/protocol/pubsub" => "XEP-0060 publish-subscribe",
        "http://jabber.org/protocol/commands" => "XEP-0050 commands",
        "http://jabber.org/protocol/muc" => "XEP-0045 group chat",
        "urn:xmpp:invite" => "XEP-0401 invitations",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feature_is_named_by_its_specification_where_it_is_known() {
        assert_eq!(xep_of("urn:xmpp:mam:2"), Some("XEP-0313 message archive"));
        assert_eq!(xep_of("urn:example:nothing"), None);
    }
}
