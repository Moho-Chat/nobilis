//! Typing notifications, which IRC only recently learned.
//!
//! A client tag on a message nobody is meant to see as a message. Both
//! directions are here: what this client sends while somebody types, and the
//! short-lived record of who else is typing, which expires on its own because
//! the protocol has no way of saying "stopped".

use super::*;

/// How long a typing notice stands before the person is assumed to have
/// stopped.
///
/// The `+typing` tag's own guidance: refresh while still composing, and forget
/// a notice that has not been refreshed. Six seconds against the client's
/// four-second refresh leaves room for one lost message before somebody stops
/// appearing to type - which is the right way round, since a name stuck on
/// screen is worse than one that flickers off a moment early.
pub(super) const TYPING_TTL: Duration = Duration::from_secs(6);

/// Who is composing, and where.
///
/// Kept here rather than in `Runtime` because nothing outside this file asks:
/// it exists so that one person going quiet removes one name from a list
/// rather than clearing the whole list, which is what emitting a bare "nobody
/// is typing" would do to the other people still writing.
pub(super) fn typers() -> &'static std::sync::Mutex<HashMap<String, HashMap<String, std::time::Instant>>> {
    static TYPERS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, HashMap<String, std::time::Instant>>>> =
        std::sync::OnceLock::new();
    TYPERS.get_or_init(Default::default)
}

/// The names still composing in a buffer after this one's news is folded in.
///
/// Pure so it can be tested: the state, the person, what they said, and now.
/// Returns the list to announce, already free of anyone whose notice ran out.
pub(super) fn fold_typing(
    room: &mut HashMap<String, std::time::Instant>,
    nick: &str,
    state: TypingState,
    now: std::time::Instant,
) -> Vec<String> {
    match state {
        // "paused" is somebody who stopped mid-sentence with a half-written
        // line still in the box. They are still writing to anybody watching,
        // so it refreshes the notice like "active" does.
        TypingState::Active | TypingState::Paused => {
            room.insert(nick.to_string(), now);
        }
        TypingState::Done => {
            room.remove(nick);
        }
    }
    room.retain(|_, seen| now.duration_since(*seen) < TYPING_TTL);
    let mut names: Vec<String> = room.keys().cloned().collect();
    // Stable, because this list is drawn as a sentence and a sentence whose
    // words swap places every few seconds is unreadable.
    names.sort_unstable();
    names
}

/// The three things `+typing` can say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TypingState {
    Active,
    Paused,
    Done,
}

impl TypingState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "done" => Some(Self::Done),
            // An unknown value is not a reason to guess. The tag is versioned
            // by its values, and a client that treated anything unrecognised
            // as "typing" would be wrong in the direction that shows.
            _ => None,
        }
    }
}

/// Says this end is composing, where the network can carry it.
///
/// A TAGMSG with a client-only tag and no text: servers relay it to the target
/// exactly as they relay a message, and clients that do not understand it drop
/// it. Gated on `message-tags` because without that capability the server
/// strips tags from what it forwards, so the line would arrive as an empty
/// TAGMSG saying nothing at all.
pub fn send_typing(state: &AppState, account_id: &str, target: &str, typing: bool) {
    if !state.runtime.irc_has_cap(account_id, "message-tags") {
        return;
    }
    let Some(sender) = state.runtime.irc_sender(account_id) else {
        return;
    };
    let mut msg = Message::from(Command::Raw("TAGMSG".to_string(), vec![target.to_string()]));
    msg.tags = Some(vec![irc::proto::message::Tag(
        "+typing".to_string(),
        Some(if typing { "active" } else { "done" }.to_string()),
    )]);
    let _ = sender.send(msg);
}

/// Somebody else composing, arriving as a TAGMSG.
///
/// The same event every other backend emits for this, so the window needs to
/// know nothing about how IRC says it.
pub(super) fn note_typing(state: &AppState, account_id: &str, from: &str, target: &str, value: &str) {
    let Some(said) = TypingState::parse(value) else {
        return;
    };
    // Our own typing, reflected back by a server that echoes what we send.
    // Announcing it would put this account's own name in its own "somebody is
    // typing" line.
    if from.eq_ignore_ascii_case(&state.runtime.irc_current_nick(account_id).unwrap_or_default()) {
        return;
    }
    let buffer_name = if is_channel(target) { target } else { from };
    // Only where the conversation already exists. A notice that somebody is
    // composing is not a reason to open a window: anybody on the network can
    // send one of these, and a stranger typing at you should not put a new
    // conversation on screen before they have said anything.
    let Some(buffer) = state
        .runtime
        .list_buffers()
        .into_iter()
        .find(|b| b.account_id == account_id && b.name.eq_ignore_ascii_case(buffer_name))
    else {
        return;
    };
    let names = {
        let mut all = typers().lock().unwrap();
        let room = all.entry(buffer.id.clone()).or_default();
        fold_typing(room, from, said, std::time::Instant::now())
    };
    state.events.emit(
        "typing",
        json!({
            "accountId": account_id,
            "bufferId": buffer.id,
            "nicks": names,
            "expiresInMs": TYPING_TTL.as_millis() as u64,
        }),
    );
}

#[cfg(test)]
mod typing_tests {
    use super::{fold_typing, TypingState, TYPING_TTL};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn active_adds_and_done_removes_only_that_person() {
        let now = Instant::now();
        let mut room = HashMap::new();
        assert_eq!(fold_typing(&mut room, "ada", TypingState::Active, now), vec!["ada"]);
        assert_eq!(fold_typing(&mut room, "grace", TypingState::Active, now), vec!["ada", "grace"]);
        // The whole point of keeping the set: one person stopping does not
        // clear the other.
        assert_eq!(fold_typing(&mut room, "ada", TypingState::Done, now), vec!["grace"]);
    }

    #[test]
    fn paused_still_counts_as_composing() {
        let now = Instant::now();
        let mut room = HashMap::new();
        fold_typing(&mut room, "ada", TypingState::Active, now);
        // A half-written line left in the box is still a line being written.
        assert_eq!(fold_typing(&mut room, "ada", TypingState::Paused, now), vec!["ada"]);
    }

    #[test]
    fn a_notice_that_was_never_refreshed_expires() {
        let now = Instant::now();
        let mut room = HashMap::new();
        fold_typing(&mut room, "ada", TypingState::Active, now);
        let later = now + TYPING_TTL + Duration::from_secs(1);
        // Somebody who closed their client mid-sentence never sends "done".
        assert!(fold_typing(&mut room, "grace", TypingState::Active, later) == vec!["grace"]);
    }

    #[test]
    fn only_the_three_known_values_are_believed() {
        assert_eq!(TypingState::parse("active"), Some(TypingState::Active));
        assert_eq!(TypingState::parse("paused"), Some(TypingState::Paused));
        assert_eq!(TypingState::parse("done"), Some(TypingState::Done));
        // A value from a later version of the tag is not a guess to make in
        // the direction that shows on screen.
        assert_eq!(TypingState::parse("thinking"), None);
        assert_eq!(TypingState::parse(""), None);
    }
}
