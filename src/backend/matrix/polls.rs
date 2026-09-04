//! Matrix polls - `m.poll.start`, `m.poll.response`, `m.poll.end`.
//!
//! A poll arrives as three kinds of event and no running total: the start
//! carries the question and the answers, every vote is its own event, and the
//! end is a fourth party saying the counting is over. Only the most recent
//! vote from each person counts, which is why the tally is kept by sender
//! rather than as a number - a changed vote must replace, not double.
//!
//! Two spellings of everything, and both are read. The stable names arrived
//! with Matrix 1.10 and the `org.matrix.msc3381` ones are what clients sent
//! for years before that; a room with a mix of clients in it has both, and a
//! poll that renders in Element and not here is exactly the complaint this
//! answers.
//!
//! The card this produces is the same card a Kick poll or prediction lands
//! on. That is not a coincidence being exploited - it is one question with a
//! set of answers and a tally, whoever is asking.

use crate::state::AppState;
use serde_json::Value;

/// Event types, stable first and unstable second, in the order a `starts_with`
/// test should try them.
pub const POLL_START: [&str; 2] = ["m.poll.start", "org.matrix.msc3381.poll.start"];
pub const POLL_RESPONSE: [&str; 2] = ["m.poll.response", "org.matrix.msc3381.poll.response"];
pub const POLL_END: [&str; 2] = ["m.poll.end", "org.matrix.msc3381.poll.end"];

/// Whether an event type is any of the poll ones.
pub fn is_poll_event(event_type: &str) -> bool {
    POLL_START.contains(&event_type) || POLL_RESPONSE.contains(&event_type) || POLL_END.contains(&event_type)
}

/// Reads whichever spelling of a text block this event used.
///
/// MSC1767 wrapped text in an object, the stable form kept it, and plenty of
/// events carry a plain `body` beside it as the fallback for clients that
/// understand none of this.
fn text_of(value: &Value) -> Option<String> {
    for key in ["m.text", "org.matrix.msc1767.text", "body"] {
        if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
            return Some(text.to_string());
        }
    }
    // The stable form allows a list of representations; the first one that is
    // text is the one to show.
    value
        .get("m.text")
        .and_then(|v| v.as_array())
        .and_then(|items| items.iter().find_map(|i| i.get("body").and_then(|b| b.as_str())))
        .map(|s| s.to_string())
}

/// The question and answers, out of whichever shape the sender used.
pub fn parse_start(content: &Value) -> Option<(String, Vec<(String, String)>)> {
    let poll = ["m.poll", "org.matrix.msc3381.poll.start"]
        .iter()
        .find_map(|key| content.get(*key))
        .unwrap_or(content);
    let question = poll.get("question").and_then(text_of).or_else(|| text_of(content))?;
    let answers = poll.get("answers")?.as_array()?;
    let options: Vec<(String, String)> = answers
        .iter()
        .enumerate()
        .filter_map(|(index, answer)| {
            let id = answer
                .get("m.id")
                .or_else(|| answer.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                // An answer with no id of its own is still an answer; its
                // position is the only handle anybody has on it.
                .unwrap_or_else(|| index.to_string());
            let label = text_of(answer).unwrap_or_else(|| format!("Option {}", index + 1));
            Some((id, label))
        })
        .collect();
    if options.is_empty() {
        return None;
    }
    Some((question, options))
}

/// Which poll a vote or an end belongs to.
pub fn related_poll(content: &Value) -> Option<String> {
    content["m.relates_to"]["event_id"].as_str().map(|s| s.to_string())
}

/// The answer somebody picked. One answer, because this client offers one:
/// a multi-select poll is read correctly and answered singly.
pub fn parse_response(content: &Value) -> Option<String> {
    for key in ["m.selections", "org.matrix.msc3381.poll.response"] {
        // `continue`, not `?`: the whole point of the loop is that an event
        // carries one spelling or the other, and giving up on the first
        // missing key would only ever read the stable one.
        let Some(block) = content.get(key) else { continue };
        // Stable puts the ids in a list at the key; unstable wraps them in an
        // object under `answers`.
        let list = block.as_array().cloned().or_else(|| block.get("answers").and_then(|a| a.as_array()).cloned());
        if let Some(list) = list {
            if let Some(first) = list.first().and_then(|v| v.as_str()) {
                return Some(first.to_string());
            }
        }
    }
    None
}

/// Builds the card for a poll, from its question, its answers and every vote
/// this client has seen.
pub fn card(
    buffer_id: &str,
    poll_id: &str,
    question: &str,
    answers: &[(String, String)],
    votes: &std::collections::HashMap<String, String>,
    own_user_id: &str,
    ended: bool,
) -> Value {
    let options: Vec<Value> = answers
        .iter()
        .map(|(id, label)| {
            serde_json::json!({
                "id": id,
                "label": label,
                "votes": votes.values().filter(|answer| *answer == id).count(),
            })
        })
        .collect();
    serde_json::json!({
        "bufferId": buffer_id,
        "kind": "poll",
        "id": poll_id,
        "title": question,
        "options": options,
        // Matrix polls have no clock: they run until somebody ends them, so
        // the card is told plainly rather than left to work it out from a
        // countdown that would never start.
        "open": !ended,
        "duration": 0,
        "remaining": 0,
        "resultDisplayDuration": 0,
        "hasVoted": votes.contains_key(own_user_id),
        "votedOptionId": votes.get(own_user_id),
    })
}

/// Handles one poll event, updating the tally and telling the client.
///
/// Returns whether the event was one of ours, so the caller can stop.
pub fn handle(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    own_user_id: &str,
    event_type: &str,
    event_id: &str,
    sender: &str,
    content: &Value,
) -> bool {
    if POLL_START.contains(&event_type) {
        let Some((question, answers)) = parse_start(content) else { return true };
        state.runtime.set_matrix_poll(buffer_id, event_id, &question, &answers);
        announce(state, account_id, buffer_id, event_id, own_user_id);
        return true;
    }

    if POLL_RESPONSE.contains(&event_type) {
        let (Some(poll_id), Some(answer)) = (related_poll(content), parse_response(content)) else {
            return true;
        };
        // A vote after the poll ended does not count, which is the server's
        // rule and this client's too.
        if !state.runtime.matrix_poll_ended(buffer_id, &poll_id) {
            state.runtime.set_matrix_poll_vote(buffer_id, &poll_id, sender, &answer);
            announce(state, account_id, buffer_id, &poll_id, own_user_id);
        }
        return true;
    }

    if POLL_END.contains(&event_type) {
        if let Some(poll_id) = related_poll(content) {
            state.runtime.end_matrix_poll(buffer_id, &poll_id);
            announce(state, account_id, buffer_id, &poll_id, own_user_id);
        }
        return true;
    }

    false
}

/// Draws the poll again after a change made here rather than received.
pub fn republish(state: &AppState, account_id: &str, buffer_id: &str, poll_id: &str, own_user_id: &str) {
    announce(state, account_id, buffer_id, poll_id, own_user_id);
}

/// Draws the current state of one poll and sends it to the client.
fn announce(state: &AppState, _account_id: &str, buffer_id: &str, poll_id: &str, own_user_id: &str) {
    let Some((question, answers, ended)) = state.runtime.matrix_poll(buffer_id, poll_id) else { return };
    let votes = state.runtime.matrix_poll_votes(buffer_id, poll_id);
    let card = card(buffer_id, poll_id, &question, &answers, &votes, own_user_id, ended);
    state.runtime.set_live_card(buffer_id, "poll", card.clone());
    // Written down as well as shown, so the room's past polls are readable
    // from the history panel the same way a Kick channel's are - and so a
    // poll answered last week survives a restart.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if let Err(e) = state.store.record_live_card(buffer_id, "poll", poll_id, &question, &card.to_string(), ts) {
        tracing::debug!("matrix: keeping the poll: {e:#}");
    }
    state.events.emit(
        "pollCard",
        serde_json::json!({ "bufferId": buffer_id, "kind": "poll", "poll": card }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both spellings, because a room with a mix of clients in it has both.
    #[test]
    fn reads_a_poll_however_it_was_written() {
        let stable = serde_json::json!({
            "m.poll": {
                "question": { "m.text": "Where for lunch?" },
                "kind": "m.poll.disclosed",
                "answers": [
                    { "m.id": "a", "m.text": "Pub" },
                    { "m.id": "b", "m.text": "Chippy" }
                ]
            }
        });
        let (question, answers) = parse_start(&stable).expect("stable poll");
        assert_eq!(question, "Where for lunch?");
        assert_eq!(answers, vec![("a".into(), "Pub".into()), ("b".into(), "Chippy".into())]);

        let unstable = serde_json::json!({
            "org.matrix.msc3381.poll.start": {
                "question": { "org.matrix.msc1767.text": "Where for lunch?" },
                "answers": [
                    { "id": "a", "org.matrix.msc1767.text": "Pub" },
                    { "id": "b", "org.matrix.msc1767.text": "Chippy" }
                ]
            }
        });
        let (question, answers) = parse_start(&unstable).expect("unstable poll");
        assert_eq!(question, "Where for lunch?");
        assert_eq!(answers.len(), 2);
    }

    #[test]
    fn reads_a_vote_however_it_was_written() {
        let stable = serde_json::json!({ "m.selections": ["b"], "m.relates_to": { "event_id": "$poll" } });
        assert_eq!(parse_response(&stable).as_deref(), Some("b"));
        assert_eq!(related_poll(&stable).as_deref(), Some("$poll"));

        let unstable = serde_json::json!({ "org.matrix.msc3381.poll.response": { "answers": ["a"] } });
        assert_eq!(parse_response(&unstable).as_deref(), Some("a"));

        // Nothing to read is not an answer of "".
        assert_eq!(parse_response(&serde_json::json!({})), None);
    }

    /// Only the last vote from each person counts, and the card says whether
    /// this account has voted at all.
    #[test]
    fn counts_one_vote_each() {
        let answers = vec![("a".to_string(), "Pub".to_string()), ("b".to_string(), "Chippy".to_string())];
        let mut votes = std::collections::HashMap::new();
        votes.insert("@alice:example.org".to_string(), "a".to_string());
        votes.insert("@bob:example.org".to_string(), "a".to_string());
        // Bob changes his mind, which replaces rather than adds.
        votes.insert("@bob:example.org".to_string(), "b".to_string());
        let card = card("buf", "$poll", "Where for lunch?", &answers, &votes, "@alice:example.org", false);
        assert_eq!(card["options"][0]["votes"], 1);
        assert_eq!(card["options"][1]["votes"], 1);
        assert_eq!(card["hasVoted"], true);
        assert_eq!(card["votedOptionId"], "a");
        assert_eq!(card["open"], true);

        let ended = card_ended(&answers, &votes);
        assert_eq!(ended["open"], false);
    }

    fn card_ended(answers: &[(String, String)], votes: &std::collections::HashMap<String, String>) -> Value {
        card("buf", "$poll", "Where for lunch?", answers, votes, "@nobody:example.org", true)
    }
}
