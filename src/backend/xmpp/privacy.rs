//! Privacy lists (XEP-0016): rules the server applies to what reaches this account and what leaves it - by address, by roster
//! group, by subscription, or for everybody; allow or deny; for messages, incoming or outgoing presence, requests, or all of it.
//! Superseded by blocking (XEP-0191, which moho uses for "block"), and gone from Prosody; ejabberd still keeps them, and a list
//! somebody made elsewhere still applies. So they can be read here, named as default or active, added to and taken away.

use super::session::Handle;
use super::xml::Attr;
use serde::{Deserialize, Serialize};
use tokio_xmpp::minidom::Element;

pub const NS: &str = "jabber:iq:privacy";

/// One rule of a list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    /// `jid`, `group`, `subscription`, or empty for everybody.
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub value: String,
    /// `allow` or `deny`.
    pub action: String,
    pub order: u32,
    /// What it covers: `message`, `presence-in`, `presence-out`, `iq`; none for everything.
    #[serde(default)]
    pub what: Vec<String>,
}

/// The lists, and which is the default and which is active for this connection.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Lists {
    pub names: Vec<String>,
    pub default: String,
    pub active: String,
}

pub fn read_rules(list: &Element) -> Vec<Rule> {
    let mut rules: Vec<Rule> = list
        .children()
        .filter(|c| c.name() == "item")
        .map(|i| Rule {
            kind: i.attr("type").unwrap_or_default().to_string(),
            value: i.attr("value").unwrap_or_default().to_string(),
            action: i.attr("action").unwrap_or("deny").to_string(),
            order: i.attr("order").and_then(|o| o.parse().ok()).unwrap_or(0),
            what: i.children().map(|c| c.name().to_string()).collect(),
        })
        .collect();
    rules.sort_by_key(|r| r.order);
    rules
}

/// A rule in words: "Deny messages from spam@example.org".
pub fn words(r: &Rule) -> String {
    let verb = if r.action == "allow" { "Allow" } else { "Deny" };
    let what = if r.what.is_empty() {
        "everything".to_string()
    } else {
        r.what.iter().map(|w| match w.as_str() {
            "message" => "messages",
            "presence-in" => "their presence",
            "presence-out" => "your presence to them",
            "iq" => "requests",
            other => other,
        }).collect::<Vec<_>>().join(", ")
    };
    let who = match r.kind.as_str() {
        "jid" => r.value.clone(),
        "group" => format!("your group {:?}", r.value),
        "subscription" => format!("people whose subscription is {}", r.value),
        _ => "everybody".to_string(),
    };
    format!("{verb} {what} - {who}")
}

pub async fn lists(handle: &Handle) -> Result<Lists, String> {
    let answer = handle.request(None, false, super::xml::empty("query", NS)).await.map_err(|e| e.words())?;
    let Some(q) = answer else { return Ok(Lists::default()) };
    let name_of = |tag: &str| q.children().find(|c| c.name() == tag).and_then(|c| c.attr("name")).unwrap_or_default().to_string();
    Ok(Lists { names: q.children().filter(|c| c.name() == "list").filter_map(|l| l.attr("name").map(str::to_string)).collect(), default: name_of("default"), active: name_of("active") })
}

pub async fn rules(handle: &Handle, name: &str) -> Result<Vec<Rule>, String> {
    let q = Element::builder("query", NS).append(Element::builder("list", NS).at("name", name).build()).build();
    let answer = handle.request(None, false, q).await.map_err(|e| e.words())?;
    Ok(answer.and_then(|q| q.children().find(|c| c.name() == "list").map(read_rules)).unwrap_or_default())
}

/// Writes a list whole (an empty one deletes it, RFC-wise: XEP-0016 2.6).
pub async fn set(handle: &Handle, name: &str, rules: &[Rule]) -> Result<(), String> {
    let mut list = Element::builder("list", NS).at("name", name).build();
    for r in rules {
        if !matches!(r.action.as_str(), "allow" | "deny") || !matches!(r.kind.as_str(), "" | "jid" | "group" | "subscription") {
            return Err(format!("not a rule: {r:?}"));
        }
        let mut item = Element::builder("item", NS).at("action", r.action.as_str()).at("order", r.order.to_string());
        if !r.kind.is_empty() {
            item = item.at("type", r.kind.as_str()).at("value", r.value.as_str());
        }
        let mut item = item.build();
        for w in &r.what {
            if matches!(w.as_str(), "message" | "presence-in" | "presence-out" | "iq") {
                item.append_child(super::xml::empty(w, NS));
            }
        }
        list.append_child(item);
    }
    let q = Element::builder("query", NS).append(list).build();
    handle.request(None, true, q).await.map(|_| ()).map_err(|e| e.words())
}

/// Makes a list the default or the active one (an empty name: none).
pub async fn choose(handle: &Handle, which: &str, name: &str) -> Result<(), String> {
    let tag = if which == "active" { "active" } else { "default" };
    let mut e = Element::builder(tag, NS).build();
    if !name.is_empty() {
        e = Element::builder(tag, NS).at("name", name).build();
    }
    let q = Element::builder("query", NS).append(e).build();
    handle.request(None, true, q).await.map(|_| ()).map_err(|e| e.words())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_list_is_read_in_its_order_and_said_in_words() {
        let l = Element::from_str("<list xmlns='jabber:iq:privacy' name='public'><item type='jid' value='tybalt@example.com' action='deny' order='2'><message/></item><item action='allow' order='3'/><item type='subscription' value='none' action='deny' order='1'><presence-out/></item></list>").unwrap();
        let rules = read_rules(&l);
        assert_eq!(rules.iter().map(|r| r.order).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(words(&rules[0]), "Deny your presence to them - people whose subscription is none");
        assert_eq!(words(&rules[1]), "Deny messages - tybalt@example.com");
        assert_eq!(words(&rules[2]), "Allow everything - everybody");
    }
}
