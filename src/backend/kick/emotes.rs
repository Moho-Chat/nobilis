//! Which of a channel's emotes this account may send, and how one is written.
//!
//! The distinction this module exists to hold is between *seeing* an emote and
//! *using* one, because they are not the same permission and treating them as
//! one would be wrong in both directions.
//!
//! Everybody sees everything. A subscriber emote in somebody else's message
//! renders for every reader, subscribed or not - the picture is public, the id
//! is right there in the message, and a chat where half the lines showed as
//! `[emote:1082364:xqcAM]` to non-subscribers would be unreadable for exactly
//! the people most likely to be new. Nothing here gates rendering; the client's
//! formatter turns the token into a picture with no lookup at all.
//!
//! Only sending is gated, and only as a courtesy. Kick decides for itself
//! whether a message is allowed, and it is the one with the authority - what
//! this does is stop the picker from offering something that would come back
//! refused, which is a better experience than finding out after pressing send.
//! That is why "we could not ask" resolves to locked rather than to an error:
//! the cost of being wrong is a greyed-out emote, and the message still sends
//! if it is typed out.

use super::api::Emote;

/// One emote as the client sees it, with the verdict already reached.
#[derive(serde::Serialize, Debug, Clone)]
pub struct Offered {
    pub id: String,
    pub name: String,
    pub url: String,
    /// The set it came from, as a heading.
    pub set: String,
    /// Subscriber-only *and* this account is not subscribed. Shown, not
    /// hidden: knowing what subscribing would get you is most of the reason
    /// the tier exists.
    pub locked: bool,
    /// Kick emotes are static PNGs. Present so the field means the same thing
    /// here as it does for Discord's, whose client does need it.
    pub animated: bool,
}

/// The channel's emotes, each marked with whether this account may use it.
pub fn offer(emotes: &[Emote], subscribed: bool) -> Vec<Offered> {
    emotes
        .iter()
        .map(|e| Offered {
            id: e.id.clone(),
            name: e.name.clone(),
            url: e.url.clone(),
            set: e.set.clone(),
            locked: e.subscribers_only && !subscribed,
            animated: false,
        })
        .collect()
}

/// How an emote is written inside a message.
///
/// Kick's own format, which every other Kick client reads and writes: the id
/// carries the picture and the name is there so the line still says something
/// where the picture cannot be drawn.
pub fn token(id: &str, name: &str) -> String {
    format!("[emote:{id}:{name}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emote(name: &str, subs_only: bool) -> Emote {
        Emote {
            id: "1".into(),
            name: name.into(),
            subscribers_only: subs_only,
            set: "xqc".into(),
            url: "https://files.kick.com/emotes/1/fullsize".into(),
        }
    }

    #[test]
    fn a_subscriber_may_use_everything() {
        let offered = offer(&[emote("free", false), emote("paid", true)], true);
        assert!(offered.iter().all(|o| !o.locked));
    }

    #[test]
    fn everybody_else_may_use_the_free_ones() {
        let offered = offer(&[emote("free", false), emote("paid", true)], false);
        assert!(!offered[0].locked);
        assert!(offered[1].locked);
        // Locked, not absent: the picker shows what subscribing would buy.
        assert_eq!(offered.len(), 2);
    }

    #[test]
    fn writes_kicks_own_token() {
        assert_eq!(token("1082364", "xqcAM"), "[emote:1082364:xqcAM]");
    }
}
