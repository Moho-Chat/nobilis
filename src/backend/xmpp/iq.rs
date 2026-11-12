//! Requests the server or another client makes of this one, and the answers RFC 6120 requires.
//!
//! An `<iq>` of type get or set **must** be answered - with a result, or with an error - or the sender waits for a
//! reply that never comes, and some servers treat a client that leaves them hanging as dead. So everything not
//! understood is answered `service-unavailable`, never ignored. What is understood: a ping; service discovery of this
//! client (so peers know what it does); its name and version (XEP-0092), which says no more than that; and the time
//! (XEP-0202), in UTC, so it does not say where the machine is. A roster push is the roster's (`roster.rs`).
//!
//! Not answered, on purpose: last activity (XEP-0012), which would tell anyone who asks how long the person has been
//! away from the keyboard.

use super::{caps, ns};
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::stanza_error::{DefinedCondition, ErrorType, StanzaError};

/// The reply to an incoming iq, or `None` when it needs none (a result or an error is itself an answer).
pub fn answer(iq: &Iq) -> Option<Iq> {
    match iq {
        Iq::Get { from, id, payload, .. } => Some(match get(payload) {
            Ok(result) => Iq::Result { from: None, to: from.clone(), id: id.clone(), payload: result },
            Err(condition) => refused(from.clone(), id, condition),
        }),
        Iq::Set { from, id, .. } => Some(refused(from.clone(), id, DefinedCondition::ServiceUnavailable)),
        Iq::Result { .. } | Iq::Error { .. } => None,
    }
}

fn get(payload: &Element) -> Result<Option<Element>, DefinedCondition> {
    if payload.is("ping", ns::PING) {
        return Ok(None);
    }
    if payload.is("query", ns::DISCO_INFO) {
        return caps::disco_info(payload.attr("node")).map(Some).ok_or(DefinedCondition::ItemNotFound);
    }
    if payload.is("query", ns::DISCO_ITEMS) {
        return Ok(Some(Element::builder("query", ns::DISCO_ITEMS).build()));
    }
    if payload.is("query", ns::VERSION) {
        let mut q = Element::builder("query", ns::VERSION).build();
        q.append_child(Element::builder("name", ns::VERSION).append(caps::NAME).build());
        q.append_child(Element::builder("version", ns::VERSION).append(env!("CARGO_PKG_VERSION")).build());
        return Ok(Some(q));
    }
    if payload.is("time", ns::TIME) {
        let mut t = Element::builder("time", ns::TIME).build();
        t.append_child(Element::builder("tzo", ns::TIME).append("+00:00").build());
        t.append_child(Element::builder("utc", ns::TIME).append(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()).build());
        return Ok(Some(t));
    }
    Err(DefinedCondition::ServiceUnavailable)
}

fn refused(to: Option<Jid>, id: &str, condition: DefinedCondition) -> Iq {
    Iq::Error { from: None, to, id: id.to_string(), payload: None, error: StanzaError::new(ErrorType::Cancel, condition, "en", "") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::xmpp::xml::Attr;

    fn get(payload: Element) -> Iq {
        Iq::Get { from: Some("example.org".parse().unwrap()), to: None, id: "q1".into(), payload }
    }

    fn result_payload(iq: Iq) -> Option<Element> {
        match iq {
            Iq::Result { payload, .. } => payload,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_ping_is_answered_with_an_empty_result_to_whoever_sent_it() {
        let reply = answer(&get(Element::builder("ping", ns::PING).build())).expect("answered");
        match reply {
            Iq::Result { to, id, payload, .. } => assert_eq!((to.map(|j| j.to_string()), id.as_str(), payload.is_none()), (Some("example.org".into()), "q1", true)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn what_is_not_understood_is_refused_not_ignored() {
        for iq in [get(Element::builder("query", "urn:example:unknown").build()), Iq::Set { from: None, to: None, id: "s1".into(), payload: Element::builder("x", "urn:example:unknown").build() }] {
            let id = iq.id().to_string();
            match answer(&iq).expect("a reply") {
                Iq::Error { id: reply_id, error, .. } => assert_eq!((reply_id, error.defined_condition), (id, DefinedCondition::ServiceUnavailable)),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn an_answer_is_not_answered() {
        assert!(answer(&Iq::Result { from: None, to: None, id: "r".into(), payload: None }).is_none());
    }

    #[test]
    fn discovery_says_what_this_client_does_and_a_node_that_is_not_ours_is_not_found() {
        let info = result_payload(answer(&get(Element::builder("query", ns::DISCO_INFO).build())).unwrap()).unwrap();
        assert!(info.children().any(|c| c.name() == "feature" && c.attr("var") == Some(ns::RECEIPTS)));
        match answer(&get(Element::builder("query", ns::DISCO_INFO).at("node", "urn:other").build())).unwrap() {
            Iq::Error { error, .. } => assert_eq!(error.defined_condition, DefinedCondition::ItemNotFound),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_name_and_version_are_given_and_nothing_about_the_machine() {
        let q = result_payload(answer(&get(Element::builder("query", ns::VERSION).build())).unwrap()).unwrap();
        let names: Vec<String> = q.children().map(|c| c.name().to_string()).collect();
        assert_eq!(names, vec!["name", "version"]);
    }

    #[test]
    fn the_time_is_utc_and_last_activity_is_not_answered() {
        let t = result_payload(answer(&get(Element::builder("time", ns::TIME).build())).unwrap()).unwrap();
        assert_eq!(t.children().find(|c| c.name() == "tzo").map(|c| c.text()).as_deref(), Some("+00:00"));
        match answer(&get(Element::builder("query", "jabber:iq:last").build())).unwrap() {
            Iq::Error { error, .. } => assert_eq!(error.defined_condition, DefinedCondition::ServiceUnavailable),
            other => panic!("{other:?}"),
        }
    }
}
