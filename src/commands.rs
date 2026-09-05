//! The slash commands this daemon understands, and how to find one.
//!
//! Here rather than in a window because the commands are the daemon's: each
//! backend parses the ones it implements out of a message that starts with a
//! slash (see `backend/irc.rs`'s `send_message` and `backend/matrix/mod.rs`),
//! so this table is the same list read from the other end. A client that held
//! its own copy would be a second list to keep in step, and it would be the
//! one that drifted.
//!
//! Application commands - Discord's, belonging to whatever bots a server has
//! installed - are not here. Those come from the service, per channel, and are
//! merged with these by the RPC that answers the question.

/// One command somebody can type, as a menu needs to describe it.
pub struct Builtin {
    pub name: &'static str,
    /// What goes after the name, in the usual notation: `<required>` and
    /// `[optional]`. Empty for a command that takes nothing.
    pub usage: &'static str,
    pub description: &'static str,
    /// Which services understand it. Every one of these is implemented by the
    /// backend named here; a command listed for a service that cannot run it
    /// would be worse than no menu at all.
    pub services: &'static [&'static str],
}

const IRC: &[&str] = &["irc"];
const IRC_AND_MATRIX: &[&str] = &["irc", "matrix"];

/// Ordered roughly by how often each is wanted, since a fuzzy match that ties
/// keeps this order.
pub const BUILTINS: &[Builtin] = &[
    Builtin { name: "me", usage: "<action>", description: "Say something as an action", services: IRC_AND_MATRIX },
    Builtin { name: "join", usage: "<#channel>", description: "Join a channel", services: IRC },
    Builtin { name: "part", usage: "", description: "Leave this channel", services: IRC },
    Builtin { name: "msg", usage: "<nick> <message>", description: "Send somebody a private message", services: IRC },
    Builtin { name: "nick", usage: "<newnick>", description: "Change your nick", services: IRC },
    Builtin { name: "topic", usage: "<text>", description: "Set the channel topic", services: IRC },
    Builtin { name: "away", usage: "[reason]", description: "Mark yourself away", services: IRC },
    Builtin { name: "back", usage: "", description: "Stop being away", services: IRC },
    Builtin { name: "whois", usage: "<nick>", description: "Look somebody up", services: IRC },
    Builtin { name: "whowas", usage: "<nick>", description: "Look up somebody who has left", services: IRC },
    Builtin { name: "notice", usage: "<target> <message>", description: "Send a notice", services: IRC },
    Builtin { name: "ctcp", usage: "<nick> <request>", description: "Send a CTCP request", services: IRC },
    Builtin { name: "invite", usage: "<nick>", description: "Invite somebody to this channel", services: IRC },
    Builtin { name: "list", usage: "[pattern]", description: "List the network's channels", services: IRC },
    Builtin { name: "kick", usage: "<nick> [reason]", description: "Remove somebody from the channel", services: IRC },
    Builtin { name: "ban", usage: "<nick>", description: "Ban somebody from the channel", services: IRC },
    Builtin { name: "unban", usage: "<mask>", description: "Lift a ban", services: IRC },
    Builtin { name: "op", usage: "<nick>", description: "Give somebody operator status", services: IRC },
    Builtin { name: "deop", usage: "<nick>", description: "Take operator status away", services: IRC },
    Builtin { name: "voice", usage: "<nick>", description: "Give somebody voice", services: IRC },
    Builtin { name: "devoice", usage: "<nick>", description: "Take voice away", services: IRC },
    Builtin { name: "mode", usage: "[target] <modes>", description: "Set or read channel modes", services: IRC },
];

/// How well a typed fragment matches a command name, or nothing if it does
/// not match at all.
///
/// A subsequence rather than a prefix: people type the letters they remember,
/// and "dv" should find "devoice" the way it does in every editor. Higher is
/// better, and the three things that make a match better are all things
/// somebody typing has in mind - the letters being together, being at the
/// start, and the name being short enough that there is not much else it
/// could have been.
pub fn score(query: &str, name: &str) -> Option<i32> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let name_lower = name.to_lowercase();
    let mut points = 0;
    let mut at = 0usize;
    let name_bytes: Vec<char> = name_lower.chars().collect();
    let mut last_hit: Option<usize> = None;
    for wanted in query.chars() {
        let found = name_bytes.iter().skip(at).position(|c| *c == wanted)? + at;
        // Letters that follow one another read as the word being typed out.
        if last_hit == Some(found.wrapping_sub(1)) {
            points += 5;
        }
        if found == 0 {
            points += 10;
        }
        last_hit = Some(found);
        at = found + 1;
    }
    // A whole-word match beats a scattered one of the same length, and a
    // shorter name beats a longer one that merely contains the letters.
    if name_lower.starts_with(&query) {
        points += 20;
    }
    points += 10i32.saturating_sub(name_bytes.len() as i32);
    Some(points)
}

/// The built-in commands a conversation on this service understands, best
/// match first.
pub fn matching(service: &str, query: &str) -> Vec<&'static Builtin> {
    let mut hits: Vec<(i32, &'static Builtin)> = BUILTINS
        .iter()
        .filter(|c| c.services.contains(&service))
        .filter_map(|c| score(query, c.name).map(|s| (s, c)))
        .collect();
    // Stable, so an exact tie keeps the order the table is written in - which
    // is roughly how often each is wanted.
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::{matching, score};

    #[test]
    fn a_prefix_finds_the_command() {
        let hits = matching("irc", "ki");
        assert_eq!(hits.first().map(|c| c.name), Some("kick"));
    }

    /// The thing a prefix search cannot do, and the reason this is a fuzzy
    /// one: the letters somebody remembers are not always the first ones.
    #[test]
    fn scattered_letters_still_find_it() {
        let hits = matching("irc", "dv");
        assert!(hits.iter().any(|c| c.name == "devoice"), "got {:?}", hits.iter().map(|c| c.name).collect::<Vec<_>>());
    }

    #[test]
    fn a_better_match_comes_first() {
        // "ban" is the word; "unban" merely contains it.
        let hits = matching("irc", "ban");
        assert_eq!(hits[0].name, "ban");
        assert!(hits.iter().any(|c| c.name == "unban"));
    }

    #[test]
    fn nothing_typed_offers_everything_this_service_has() {
        let irc = matching("irc", "");
        assert!(irc.len() > 10);
        // And a service with no commands of its own offers none rather than
        // offering somebody else's.
        assert!(matching("kick", "").is_empty());
        assert!(matching("discord", "").is_empty());
    }

    #[test]
    fn matrix_gets_the_ones_it_actually_implements() {
        let names: Vec<&str> = matching("matrix", "").iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["me"]);
    }

    #[test]
    fn letters_that_are_not_in_it_do_not_match() {
        assert!(score("zzz", "kick").is_none());
        assert!(score("kx", "kick").is_none());
    }
}
