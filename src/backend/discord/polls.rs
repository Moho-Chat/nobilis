//! Discord polls: the card, and the one thing a poll exists for.
//!
//! A Discord poll is part of a message rather than an event of its own -
//! question, answers, an expiry, and a `results` block Discord recounts as
//! votes arrive. `extract_poll` in `messages` writes it into the log as text,
//! which is the record of what was asked; this is the thing that can be
//! answered.
//!
//! It lands on the same card a Kick poll or a Matrix one does. One question
//! with a set of answers and a tally is one shape, whoever is asking, and the
//! renderer already draws it.
//!
//! One poll per conversation is showing at a time, which is the card's own
//! rule rather than Discord's: a channel can hold several open polls, and the
//! most recent one to move is the one on screen. The log keeps the rest.

use super::*;

/// Whether this message carries a poll at all.
pub(super) fn has_poll(d: &Value) -> bool {
    d.get("poll").filter(|p| p.is_object()).is_some()
}

/// Builds the card for a Discord poll.
///
/// The tally comes from `results.answer_counts`, which Discord omits entirely
/// until the first vote - an unanswered poll has no counts rather than a row
/// of zeroes, so a missing entry is nought and not missing data.
pub(super) fn card(buffer_id: &str, message_id: &str, poll: &Value) -> Option<Value> {
    let question = poll["question"]["text"].as_str().unwrap_or("Poll");
    let counts = &poll["results"]["answer_counts"];
    let count_for = |id: i64| -> (u64, bool) {
        counts
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["id"].as_i64() == Some(id))
            .map(|c| (c["count"].as_u64().unwrap_or(0), c["me_voted"].as_bool().unwrap_or(false)))
            .unwrap_or((0, false))
    };

    let mut options: Vec<Value> = Vec::new();
    let mut voted_for: Option<i64> = None;
    for answer in poll["answers"].as_array().into_iter().flatten() {
        let Some(id) = answer["answer_id"].as_i64() else { continue };
        let text = answer["poll_media"]["text"].as_str().unwrap_or("");
        let emoji = answer["poll_media"]["emoji"]["name"].as_str().unwrap_or("");
        // An answer can be an emoji and nothing else, which is a label.
        let label = match (emoji.is_empty(), text.is_empty()) {
            (true, true) => continue,
            (true, false) => text.to_string(),
            (false, true) => emoji.to_string(),
            (false, false) => format!("{emoji} {text}"),
        };
        let (votes, mine) = count_for(id);
        if mine {
            voted_for = Some(id);
        }
        options.push(json!({ "id": id, "label": label, "votes": votes }));
    }
    if options.is_empty() {
        return None;
    }

    // Discord gives an absolute moment rather than a countdown, so the
    // seconds left are worked out here and stamped with when that was true -
    // the card counts down from the pair.
    let now = now_secs();
    let expiry = poll["expiry"].as_str().and_then(parse_expiry);
    let remaining = expiry.map(|at| (at - now).max(0)).unwrap_or(0);
    // `is_finalized` means the counting is over. A poll past its expiry that
    // Discord has not finalised yet is closed too: the clock is what people
    // read, and offering a vote that would be refused is worse than saying so.
    let finalized = poll["results"]["is_finalized"].as_bool().unwrap_or(false);
    let open = !finalized && expiry.map(|at| at > now).unwrap_or(true);

    Some(json!({
        "bufferId": buffer_id,
        "kind": "poll",
        // The message is the poll: it is what a vote is addressed to.
        "id": message_id,
        "title": question,
        "options": options,
        "open": open,
        "duration": 0,
        "remaining": remaining,
        // Discord leaves a finished poll in the channel forever, so there is
        // no window after which the result stops being shown.
        "resultDisplayDuration": 0,
        "hasVoted": voted_for.is_some(),
        "votedOptionId": voted_for,
        "asOf": now,
    }))
}

/// This moment, in unix seconds.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Discord's expiry is an ISO 8601 instant; this wants a unix second.
fn parse_expiry(at: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(at).ok().map(|t| t.timestamp())
}

/// Draws the poll in a message, and writes it down.
///
/// Called for every message carrying one, arriving or updated - a vote by
/// anybody comes back as a MESSAGE_UPDATE with a new tally in it, which is
/// how the counts move without asking.
pub(super) fn announce(state: &AppState, buffer_id: &str, message_id: &str, d: &Value) {
    let Some(card) = card(buffer_id, message_id, &d["poll"]) else { return };
    state.runtime.set_live_card(buffer_id, "poll", card.clone());
    // Kept as well as shown, so a channel's past polls are readable from the
    // history panel the way a Kick channel's or a Matrix room's are.
    let title = card["title"].as_str().unwrap_or_default().to_string();
    let ts = now_secs();
    if let Err(e) = state.store.record_live_card(buffer_id, "poll", message_id, &title, &card.to_string(), ts) {
        tracing::debug!("discord: keeping the poll: {e:#}");
    }
    state
        .events
        .emit("pollCard", json!({ "bufferId": buffer_id, "kind": "poll", "poll": card }));
}

/// Draws a poll read back out of history, if it is still running.
///
/// A poll posted before this conversation was opened is the one a channel
/// read for the first time most often has open in it - Discord's run for
/// hours or days, so the live path alone would never show it. A finished one
/// is history and stays in the log where it was written.
pub(super) fn announce_if_open(state: &AppState, buffer_id: &str, message_id: &str, d: &Value) {
    if card(buffer_id, message_id, &d["poll"]).is_some_and(|c| c["open"] == json!(true)) {
        announce(state, buffer_id, message_id, d);
    }
}

/// Answers a poll.
///
/// `PUT .../answers/@me` is what the official client sends, and it takes a
/// list because a poll may allow several answers. This client offers one, so
/// it sends one - a multi-select poll is read correctly and answered singly,
/// the same compromise the Matrix side makes.
///
/// Through `send_write`, so it is paced and waits out a rate limit like every
/// other thing this account does rather than sends.
pub async fn vote_in_poll(state: &AppState, account_id: &str, buffer_id: &str, message_id: &str, answer_id: i64) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .context("no known Discord channel for this conversation")?;
    let resp = send_write(
        http_client()
            .put(format!("{API_BASE}/channels/{channel_id}/polls/{message_id}/answers/@me"))
            .header("Authorization", &cfg.token)
            .json(&json!({ "answer_ids": [answer_id.to_string()] })),
    )
    .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "voting in the poll"));
    }
    // Nothing comes back but a 204. The new tally arrives as a MESSAGE_UPDATE
    // moments later and redraws the card, the same as anybody else's vote.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A poll nobody has answered: Discord sends no `answer_counts` at all,
    /// and every answer is nought votes rather than an empty card.
    #[test]
    fn a_poll_with_no_votes_is_still_a_poll() {
        let poll = json!({
            "question": { "text": "Where for lunch?" },
            "answers": [
                { "answer_id": 1, "poll_media": { "text": "Pub" } },
                { "answer_id": 2, "poll_media": { "text": "Chips", "emoji": { "name": "🍟" } } }
            ],
            "expiry": "2099-01-01T00:00:00+00:00"
        });
        let card = card("discord:me|#lunch", "700", &poll).expect("a card");
        assert_eq!(card["title"], "Where for lunch?");
        assert_eq!(card["options"][0]["votes"], 0);
        assert_eq!(card["options"][1]["label"], "🍟 Chips");
        assert_eq!(card["hasVoted"], false);
        assert_eq!(card["open"], true);
        assert!(card["remaining"].as_i64().unwrap_or(0) > 0);
    }

    /// The tally, and which answer is this account's - `me_voted` is the only
    /// thing that says so, and it is what stops the card offering a vote
    /// somebody has already cast.
    #[test]
    fn the_tally_and_your_own_answer_are_read() {
        let poll = json!({
            "question": { "text": "Tabs or spaces?" },
            "answers": [
                { "answer_id": 1, "poll_media": { "text": "Tabs" } },
                { "answer_id": 2, "poll_media": { "text": "Spaces" } }
            ],
            "results": { "is_finalized": false, "answer_counts": [
                { "id": 1, "count": 3, "me_voted": false },
                { "id": 2, "count": 9, "me_voted": true }
            ]}
        });
        let card = card("discord:me|#dev", "701", &poll).expect("a card");
        assert_eq!(card["options"][0]["votes"], 3);
        assert_eq!(card["options"][1]["votes"], 9);
        assert_eq!(card["hasVoted"], true);
        assert_eq!(card["votedOptionId"], 2);
    }

    /// Closed two ways: Discord saying the counting is over, and a clock that
    /// has run out while Discord has not got round to saying so. Offering a
    /// vote that would be refused is worse than saying it is shut.
    #[test]
    fn a_finished_poll_is_shut_however_it_finished() {
        let base = json!({
            "question": { "text": "Done?" },
            "answers": [{ "answer_id": 1, "poll_media": { "text": "Yes" } }]
        });

        let mut finalized = base.clone();
        finalized["results"] = json!({ "is_finalized": true });
        finalized["expiry"] = json!("2099-01-01T00:00:00+00:00");
        assert_eq!(card("b", "1", &finalized).expect("a card")["open"], false);

        let mut expired = base.clone();
        expired["expiry"] = json!("2020-01-01T00:00:00+00:00");
        let card = card("b", "1", &expired).expect("a card");
        assert_eq!(card["open"], false);
        assert_eq!(card["remaining"], 0);
    }

    /// An answer that is nothing but an emoji is an answer, and a poll with
    /// no answers at all is no card rather than an empty one.
    #[test]
    fn an_emoji_is_a_label_and_nothing_is_no_card() {
        let emoji_only = json!({
            "question": { "text": "?" },
            "answers": [{ "answer_id": 1, "poll_media": { "emoji": { "name": "👍" } } }]
        });
        assert_eq!(card("b", "1", &emoji_only).expect("a card")["options"][0]["label"], "👍");

        assert!(card("b", "1", &json!({ "question": { "text": "?" }, "answers": [] })).is_none());
        assert!(card("b", "1", &json!({})).is_none());
    }
}
