//! IRCv3 metadata - the avatar, display name and status a network lets
//! somebody publish about themselves.
//!
//! IRC has no profile of its own. A nick and a realname is the whole of what
//! the protocol carries, which is why moho can draw a face for everyone on
//! Discord and Matrix and nobody on IRC. Metadata is the mechanism IRC grew
//! for exactly that, and on a network that offers it the facts are there for
//! the asking.
//!
//! Two spellings, and both are asked for. `draft/metadata` is what the
//! networks that shipped it first still advertise, and `metadata-2` is the
//! name it was given on the way to being standardised. A network offering
//! either is a network this understands; one offering both is answered the
//! same way, because the wire format below is the same in both.
//!
//! Values arrive two ways and both are handled. `METADATA` as a command is
//! how a change reaches a subscriber after the fact, and `RPL_KEYVALUE`
//! (761) is how the same thing comes back from an explicit `GET`. They carry
//! the same four fields in the same order, so they are read by one parser.
//!
//! A key with no value has been *cleared*, and that is not the same as a key
//! that was never set - the network is saying somebody took their avatar
//! down. Both end up absent here, which is right, but the distinction is why
//! an empty value removes the entry instead of storing an empty string.

use std::collections::BTreeMap;

/// What is worth subscribing to.
///
/// Deliberately short. A subscription is a standing request for every change
/// to these keys for everybody visible, so asking for keys nothing renders
/// would be traffic spent on nothing. These are the ones with somewhere to
/// go: the first two onto the profile card as a face and a name, and the
/// rest as the lines beneath it.
pub const WANTED_KEYS: &[&str] = &["avatar", "display-name", "homepage", "status"];

/// One person's published facts, by key.
pub type Metadata = BTreeMap<String, String>;

/// Reads the `<target> <key> <visibility> [:<value>]` shape that both
/// `METADATA` and `RPL_KEYVALUE` use.
///
/// Returns the target, the key, and the value - `None` for a key that has
/// been cleared. Visibility is parsed past rather than kept: it says who else
/// can see the value, which is the network's business and not a fact about
/// the person.
///
/// The numeric form is prefixed with the client's own nick, the way every
/// numeric is. That is stripped by the caller, which knows whether it is
/// reading a command or a reply, rather than being guessed at here.
pub fn parse_keyvalue(args: &[String]) -> Option<(String, String, Option<String>)> {
    let target = args.first()?.clone();
    let key = args.get(1)?.clone();
    // Three args is a cleared key: target, key, visibility and nothing after.
    let value = args.get(3).filter(|v| !v.is_empty()).cloned();
    Some((target, key, value))
}

/// Whether a capability name is one of the two spellings of metadata.
pub fn is_metadata_cap(cap: &str) -> bool {
    matches!(cap, "draft/metadata" | "metadata-2")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_a_value_somebody_published() {
        let got = parse_keyvalue(&args(&["alice", "avatar", "*", "https://example.net/a.png"]));
        assert_eq!(got, Some(("alice".into(), "avatar".into(), Some("https://example.net/a.png".into()))));
    }

    /// A cleared key is the network saying somebody took their avatar down,
    /// which has to be distinguishable from never having had one.
    #[test]
    fn a_key_with_no_value_has_been_cleared() {
        assert_eq!(parse_keyvalue(&args(&["alice", "avatar", "*"])), Some(("alice".into(), "avatar".into(), None)));
        // An explicitly empty trailing parameter means the same thing.
        assert_eq!(parse_keyvalue(&args(&["alice", "avatar", "*", ""])), Some(("alice".into(), "avatar".into(), None)));
    }

    #[test]
    fn a_line_too_short_to_mean_anything_is_not_read() {
        assert_eq!(parse_keyvalue(&args(&["alice"])), None);
        assert_eq!(parse_keyvalue(&[]), None);
    }

    #[test]
    fn both_spellings_of_the_capability_count() {
        assert!(is_metadata_cap("draft/metadata"));
        assert!(is_metadata_cap("metadata-2"));
        assert!(!is_metadata_cap("metadata"));
        assert!(!is_metadata_cap("away-notify"));
    }
}
