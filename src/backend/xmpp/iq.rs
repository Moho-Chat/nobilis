//! Requests the server or another client makes of this one, and the answers RFC 6120 requires.
//!
//! An `<iq>` of type get or set **must** be answered - with a result, or with an error - or the sender waits for a
//! reply that never comes, and some servers treat a client that leaves them hanging as dead. So everything not
//! understood is answered `service-unavailable`, never ignored. What is understood grows with the milestones (version,
//! time, disco and capabilities, roster pushes, ...); for now it is the ping a server sends to check we are there.

use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::ns;
use tokio_xmpp::parsers::stanza_error::{DefinedCondition, ErrorType, StanzaError};

/// The reply to an incoming iq, or `None` when it needs none (a result or an error is itself an answer).
pub fn answer(iq: &Iq) -> Option<Iq> {
    match iq {
        Iq::Get { from, id, payload, .. } if payload.is("ping", ns::PING) => Some(result_to(from.clone(), id)),
        Iq::Get { from, id, .. } | Iq::Set { from, id, .. } => Some(unavailable(from.clone(), id)),
        Iq::Result { .. } | Iq::Error { .. } => None,
    }
}

fn result_to(to: Option<tokio_xmpp::jid::Jid>, id: &str) -> Iq {
    Iq::Result { from: None, to, id: id.to_string(), payload: None }
}

fn unavailable(to: Option<tokio_xmpp::jid::Jid>, id: &str) -> Iq {
    Iq::Error { from: None, to, id: id.to_string(), payload: None, error: StanzaError::new(ErrorType::Cancel, DefinedCondition::ServiceUnavailable, "en", "") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_xmpp::minidom::Element;

    fn get(payload: Element) -> Iq {
        Iq::Get { from: Some("example.org".parse().unwrap()), to: None, id: "q1".into(), payload }
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
}
