//! Reactions, which IRC learned as a client tag.
//!
//! IRCv3's `+draft/react` and `+draft/unreact`: a TAGMSG with no text that
//! names the message it is about with `+reply` and carries the emoji as the
//! tag's value. Servers relay it like any other client tag, so a network
//! either has this or does not depending on two things it announces:
//!
//! - the `message-tags` capability, without which client tags are stripped and
//!   the line arrives as an empty TAGMSG saying nothing;
//! - its `CLIENTTAGDENY` ISUPPORT token, which may refuse client tags in
//!   general or this one by name.
//!
//! Both are read off the connection, and the window is told through the
//! account's `reactions` flag, so the react button exists only where the
//! network will carry what it sends.
//!
//! Reactions are live-only on IRC. History playback does not include TAGMSGs
//! unless a client asks for `draft/event-playback`, which this one does not -
//! so the record of who reacted is kept in memory for this run, and the counts
//! the store already holds are the part that survives a restart.

use super::*;

/// The tag that adds a reaction and the one that takes it back.
const REACT: &str = "+draft/react";
const UNREACT: &str = "+draft/unreact";

/// The spellings of "this is about that message".
///
/// `+reply` is what the react spec names; `+draft/reply` is what the reply
/// draft was called for years and what plenty of clients still send. Both are
/// read, and both are sent, so a reaction is understood by either generation.
const REPLY_TAGS: [&str; 2] = ["+reply", "+draft/reply"];

/// Longest reaction this client will send or believe, in bytes.
///
/// The spec puts no limit on the value and suggests clients choose one. An
/// emoji with skin tone and joiners is under 40; this leaves room for a short
/// word, which some clients react with, and stops anyone using the tag to put a
/// paragraph under somebody's message.
const MAX_REACTION: usize = 64;

/// Whether `CLIENTTAGDENY` refuses a client tag, by its name without the `+`.
///
/// The token is a comma-separated list: a name refuses that tag, `*` (first,
/// when present) refuses all of them, and `-name` exempts one from the `*`.
/// Absent or empty refuses nothing. Pure so the cases can be tested.
pub(super) fn tag_denied(deny: Option<&str>, tag: &str) -> bool {
    let Some(list) = deny else { return false };
    let mut all = false;
    let mut denied = false;
    let mut exempt = false;
    for item in list.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        if item == "*" {
            all = true;
        } else if let Some(name) = item.strip_prefix('-') {
            if name == tag {
                exempt = true;
            }
        } else if item == tag {
            denied = true;
        }
    }
    denied || (all && !exempt)
}

/// Whether this connection can carry reactions: tags are relayed, and this
/// one is not refused.
pub fn supported(state: &AppState, account_id: &str) -> bool {
    state.runtime.irc_has_cap(account_id, "message-tags")
        && !tag_denied(state.runtime.irc_isupport_value(account_id, "CLIENTTAGDENY").as_deref(), "draft/react")
}

/// What each account last told the window about reactions.
fn announced() -> &'static std::sync::Mutex<HashMap<String, bool>> {
    static SAID: std::sync::OnceLock<std::sync::Mutex<HashMap<String, bool>>> = std::sync::OnceLock::new();
    SAID.get_or_init(Default::default)
}

/// Tells the window when whether this account can react has changed.
///
/// Called when capabilities are granted or withdrawn and when ISUPPORT
/// arrives, which are the two things the answer depends on. Emits only on a
/// change, as `accountStatus`, which is how the window already learns that a
/// live fact about an account moved and it should read the list again.
pub(super) fn announce(state: &AppState, account_id: &str) {
    let now = supported(state, account_id);
    let before = announced().lock().unwrap().insert(account_id.to_string(), now);
    if before != Some(now) {
        state.events.emit("accountStatus", json!({ "accountId": account_id }));
    }
}

/// Who has reacted with what, per message: (buffer, message, emoji) -> nicks.
///
/// Kept so the same person reacting twice counts once - a server that echoes
/// our own TAGMSG sends back a reaction already counted when it was sent - and
/// so an unreact from somebody who never reacted takes nothing away.
type Reactors = HashMap<(String, String, String), Vec<String>>;

fn reactors() -> &'static std::sync::Mutex<Reactors> {
    static WHO: std::sync::OnceLock<std::sync::Mutex<Reactors>> = std::sync::OnceLock::new();
    WHO.get_or_init(Default::default)
}

/// Folds one person's reaction into the list of who reacted.
///
/// Returns whether anything changed, which is whether the count should move.
/// Nicknames compare without case, as IRC compares them.
pub(super) fn fold(who: &mut Vec<String>, nick: &str, add: bool) -> bool {
    let at = who.iter().position(|n| n.eq_ignore_ascii_case(nick));
    match (add, at) {
        (true, None) => {
            who.push(nick.to_string());
            true
        }
        (false, Some(i)) => {
            who.remove(i);
            true
        }
        _ => false,
    }
}

/// Records a reaction and moves the count, if it changed anything.
fn apply(state: &AppState, buffer_id: &str, msg_id: &str, emoji: &str, nick: &str, is_me: bool, add: bool) {
    let changed = {
        let mut all = reactors().lock().unwrap();
        let key = (buffer_id.to_string(), msg_id.to_string(), emoji.to_string());
        let who = all.entry(key.clone()).or_default();
        let changed = fold(who, nick, add);
        if who.is_empty() {
            all.remove(&key);
        }
        changed
    };
    if changed {
        state.runtime.update_reaction(state, buffer_id, msg_id, emoji, is_me, add);
    }
}

/// An id this client made up for a message the server never named.
///
/// See `model::next_message_id`: seconds, a dot, a sequence number. Nobody else
/// knows these, so a reaction pointing at one would point at nothing.
fn is_local_id(id: &str) -> bool {
    id.split_once('.').is_some_and(|(a, b)| {
        !a.is_empty() && !b.is_empty() && a.bytes().all(|c| c.is_ascii_digit()) && b.bytes().all(|c| c.is_ascii_digit())
    })
}

/// Reacts to a message, or takes a reaction back.
pub fn toggle(state: &AppState, buffer: &crate::model::Buffer, msg_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    let account_id = &buffer.account_id;
    if !supported(state, account_id) {
        return Err("this network does not carry reactions".to_string());
    }
    let emoji = emoji.trim();
    if emoji.is_empty() || emoji.len() > MAX_REACTION {
        return Err("that reaction is too long to send".to_string());
    }
    if is_local_id(msg_id) || state.store.get_message(&buffer.id, msg_id).ok().flatten().is_none() {
        return Err("the network gave that message no id to react to".to_string());
    }
    let Some(sender) = state.runtime.irc_sender(account_id) else {
        return Err("account not connected".to_string());
    };
    let mut tags: Vec<irc::proto::message::Tag> = REPLY_TAGS
        .iter()
        .map(|t| irc::proto::message::Tag(t.to_string(), Some(msg_id.to_string())))
        .collect();
    tags.push(irc::proto::message::Tag(if add { REACT } else { UNREACT }.to_string(), Some(emoji.to_string())));
    let mut line = Message::from(Command::Raw("TAGMSG".to_string(), vec![buffer.name.clone()]));
    line.tags = Some(tags);
    sender.send(line).map_err(|e| e.to_string())?;
    // Counted now rather than on the echo: not every server echoes, and the
    // fold makes the echo a no-op on those that do.
    let own = state.runtime.irc_current_nick(account_id).unwrap_or_default();
    apply(state, &buffer.id, msg_id, emoji, &own, true, add);
    Ok(())
}

/// What a TAGMSG's tags say about a reaction: the message, the emoji, and
/// whether it is being added.
pub(super) fn read_tags(tags: &[irc::proto::message::Tag]) -> Option<(String, String, bool)> {
    let value = |name: &str| tags.iter().find(|t| t.0 == name).and_then(|t| t.1.as_deref()).map(str::trim).filter(|v| !v.is_empty());
    let target = REPLY_TAGS.iter().find_map(|t| value(t))?;
    // The spec forbids both on one message; one that has both is not a guess
    // worth making in either direction.
    let (emoji, add) = match (value(REACT), value(UNREACT)) {
        (Some(e), None) => (e, true),
        (None, Some(e)) => (e, false),
        _ => return None,
    };
    if emoji.len() > MAX_REACTION {
        return None;
    }
    Some((target.to_string(), emoji.to_string(), add))
}

/// Somebody's reaction, arriving as a TAGMSG.
pub(super) fn note(state: &AppState, account_id: &str, from: &str, target: &str, tags: &[irc::proto::message::Tag]) {
    let Some((msg_id, emoji, add)) = read_tags(tags) else { return };
    let own = state.runtime.irc_current_nick(account_id).unwrap_or_default();
    let is_me = from.eq_ignore_ascii_case(&own);
    // A channel's reactions are on the channel; a private one is on the
    // conversation with whoever is not us.
    let buffer_name = if is_channel(target) || is_me { target } else { from };
    // Only where the conversation already exists, and only on a message it
    // holds: a reaction is not a reason to open a window, and one about a
    // message from before this client was listening has nothing to sit under.
    let Some(buffer) = state
        .runtime
        .list_buffers()
        .into_iter()
        .find(|b| b.account_id == account_id && b.name.eq_ignore_ascii_case(buffer_name))
    else {
        return;
    };
    if state.store.get_message(&buffer.id, &msg_id).ok().flatten().is_none() {
        return;
    }
    apply(state, &buffer.id, &msg_id, &emoji, from, is_me, add);
}

/// Who reacted with one emoji, for the tooltip over it.
pub fn list_reactors(buffer_id: &str, msg_id: &str, emoji: &str) -> serde_json::Value {
    let who = reactors()
        .lock()
        .unwrap()
        .get(&(buffer_id.to_string(), msg_id.to_string(), emoji.to_string()))
        .cloned()
        .unwrap_or_default();
    let users: Vec<serde_json::Value> = who.iter().map(|n| json!({ "id": n, "name": n })).collect();
    json!({ "users": users })
}

#[cfg(test)]
mod reaction_tests {
    use super::{fold, is_local_id, read_tags, tag_denied};
    use irc::proto::message::Tag;

    fn tag(name: &str, value: &str) -> Tag {
        Tag(name.to_string(), Some(value.to_string()))
    }

    #[test]
    fn clienttagdeny_as_the_spec_writes_it() {
        assert!(!tag_denied(None, "draft/react"));
        assert!(!tag_denied(Some(""), "draft/react"));
        assert!(tag_denied(Some("*"), "draft/react"));
        // Everything refused except the ones named.
        assert!(!tag_denied(Some("*,-draft/react,-typing"), "draft/react"));
        assert!(tag_denied(Some("*,-typing"), "draft/react"));
        // Refused by name, and only that name.
        assert!(tag_denied(Some("typing,draft/react"), "draft/react"));
        assert!(!tag_denied(Some("typing"), "draft/react"));
        // No prefix matching: "draft" is not "draft/react".
        assert!(!tag_denied(Some("draft"), "draft/react"));
    }

    #[test]
    fn a_react_and_an_unreact_are_read_with_either_reply_spelling() {
        let r = [tag("+reply", "abc"), tag("+draft/react", "👍")];
        assert_eq!(read_tags(&r), Some(("abc".into(), "👍".into(), true)));
        let u = [tag("+draft/reply", "abc"), tag("+draft/unreact", "👍")];
        assert_eq!(read_tags(&u), Some(("abc".into(), "👍".into(), false)));
    }

    #[test]
    fn what_is_not_a_reaction_is_ignored() {
        // Typing, with no reply and no react.
        assert_eq!(read_tags(&[tag("+typing", "active")]), None);
        // A reaction about nothing.
        assert_eq!(read_tags(&[tag("+draft/react", "👍")]), None);
        // Both at once, which the spec forbids.
        assert_eq!(read_tags(&[tag("+reply", "a"), tag("+draft/react", "x"), tag("+draft/unreact", "y")]), None);
        // A paragraph dressed as a reaction.
        assert_eq!(read_tags(&[tag("+reply", "a"), tag("+draft/react", &"x".repeat(200))]), None);
    }

    #[test]
    fn one_person_counts_once() {
        let mut who = Vec::new();
        assert!(fold(&mut who, "Ada", true));
        // The echo of our own reaction, or a client sending it twice.
        assert!(!fold(&mut who, "ada", true));
        assert!(fold(&mut who, "grace", true));
        // Taking back what was never given changes nothing.
        assert!(!fold(&mut who, "linus", false));
        assert!(fold(&mut who, "ADA", false));
        assert_eq!(who, vec!["grace"]);
    }

    #[test]
    fn our_own_made_up_ids_are_known() {
        assert!(is_local_id("1731940000.12"));
        assert!(!is_local_id("01HZX3K9ABCDEF"));
        assert!(!is_local_id("abc.def"));
        assert!(!is_local_id("12345"));
    }
}
