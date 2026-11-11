//! Small helpers over `minidom`, for the stanza parts that the typed parsers do not cover (and for reading the ones
//! they do the same way everywhere).

use tokio_xmpp::minidom::rxml::NcName;
use tokio_xmpp::minidom::{Element, ElementBuilder, IntoAttributeValue};

/// `attr` for builders with the name given as text: every name used here is a literal, so one that is not a valid XML name
/// is a mistake in this code and not in anyone's data.
pub trait Attr {
    fn at<V: IntoAttributeValue>(self, name: &str, value: V) -> Self;
}

impl Attr for ElementBuilder {
    fn at<V: IntoAttributeValue>(self, name: &str, value: V) -> Self {
        self.attr(NcName::try_from(name.to_owned()).expect("a literal attribute name"), value)
    }
}

/// The first child with this name in this namespace.
pub fn child<'a>(e: &'a Element, name: &str, ns: &str) -> Option<&'a Element> {
    e.children().find(|c| c.is(name, ns))
}

/// The text of the first child with this name in this namespace.
pub fn child_text(e: &Element, name: &str, ns: &str) -> Option<String> {
    child(e, name, ns).map(|c| c.text())
}

/// An empty element with just a namespace: `<name xmlns=ns/>`.
pub fn empty(name: &str, ns: &str) -> Element {
    Element::builder(name, ns).build()
}

/// A fresh identifier for a stanza or a message: random, URL-safe, and not guessable (a message id is also what
/// corrections and reactions point at, and what another device would see).
pub fn new_id() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let id: String = (0..20).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect();
    format!("moho-{id}")
}

/// An ISO 8601 / XEP-0082 timestamp, UTC, as `2026-11-09T12:00:00Z`, to unix seconds.
pub fn parse_stamp(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s.trim()).ok().map(|d| d.timestamp())
}

/// Unix seconds as an XEP-0082 timestamp.
pub fn stamp(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default()
}

/// Text made safe to put in an attribute value or between tags.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\'', "&apos;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip_and_read_the_forms_servers_write() {
        assert_eq!(parse_stamp("2026-11-09T12:00:00Z"), Some(1794225600));
        assert_eq!(parse_stamp("2026-11-09T12:00:00.123+00:00"), Some(1794225600));
        assert_eq!(parse_stamp("2026-11-09T14:00:00+02:00"), Some(1794225600));
        assert_eq!(stamp(1794225600), "2026-11-09T12:00:00Z");
        assert_eq!(parse_stamp("yesterday"), None);
    }

    #[test]
    fn ids_are_different_every_time_and_say_who_made_them() {
        let (a, b) = (new_id(), new_id());
        assert_ne!(a, b);
        assert!(a.starts_with("moho-") && a.len() == 25);
    }
}
