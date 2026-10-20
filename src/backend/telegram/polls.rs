//! Polls: a question with answers, shown as the card over the conversation and voted in from it.
//!
//! A poll is part of a message. When one that is still open is read (live or from history) it is announced as the
//! conversation's poll card; Telegram sends an update whenever the tally moves, and the card is drawn again. Only the
//! newest open poll has the card (a conversation has one slot for it); a closed poll stays in the log as its label.

use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::tl;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// Which message each poll is, by Telegram's id for the poll, for an update that names only the poll.
fn registry() -> &'static Mutex<HashMap<i64, (String, i32)>> {
    static R: std::sync::OnceLock<Mutex<HashMap<i64, (String, i32)>>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

fn text_of(t: &tl::enums::TextWithEntities) -> String {
    let tl::enums::TextWithEntities::Entities(t) = t;
    t.text.clone()
}

/// An answer's id as text: Telegram names an answer by a few bytes.
pub fn option_id(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// The card for a poll, from its definition and its tally.
pub fn card(buffer_id: &str, msg_id: i32, poll: &tl::types::Poll, results: &tl::types::PollResults) -> Value {
    let tally: HashMap<Vec<u8>, (i32, bool)> = results
        .results
        .iter()
        .flatten()
        .map(|r| {
            let tl::enums::PollAnswerVoters::Voters(v) = r;
            (v.option.clone(), (v.voters.unwrap_or(0), v.chosen))
        })
        .collect();
    let mut voted: Option<String> = None;
    let options: Vec<Value> = poll
        .answers
        .iter()
        .map(|a| {
            let tl::enums::PollAnswer::Answer(a) = a else { return json!({}) };
            let (votes, chosen) = tally.get(&a.option).copied().unwrap_or((0, false));
            if chosen {
                voted = Some(option_id(&a.option));
            }
            json!({ "id": option_id(&a.option), "label": text_of(&a.text), "votes": votes })
        })
        .collect();
    json!({
        "bufferId": buffer_id,
        "kind": "poll",
        "id": msg_id.to_string(),
        "title": text_of(&poll.question),
        "options": options,
        // A poll has no clock the card could count: it is open until it is closed.
        "open": !poll.closed,
        "duration": 0,
        "remaining": 0,
        "resultDisplayDuration": 0,
        "hasVoted": voted.is_some(),
        "votedOptionId": voted,
        "noEnd": true,
    })
}

/// Shows a poll as the conversation's card, and remembers which message it is.
pub fn announce(state: &AppState, buffer_id: &str, msg_id: i32, poll: &tl::types::Poll, results: &tl::types::PollResults) {
    registry().lock().unwrap_or_else(|e| e.into_inner()).insert(poll.id, (buffer_id.to_string(), msg_id));
    if poll.closed {
        return;
    }
    let card = card(buffer_id, msg_id, poll, results);
    state.runtime.set_live_card(buffer_id, "poll", card.clone());
    let ts = chrono::Utc::now().timestamp();
    if let Err(e) = state.store.record_live_card(buffer_id, "poll", &msg_id.to_string(), card["title"].as_str().unwrap_or(""), &card.to_string(), ts) {
        tracing::debug!("telegram: keeping the poll: {e:#}");
    }
    state.events.emit("pollCard", json!({ "bufferId": buffer_id, "kind": "poll", "poll": card }));
}

/// The tally of a poll moved.
pub fn update(state: &AppState, update: &tl::types::UpdateMessagePoll) {
    let Some((buffer_id, msg_id)) = registry().lock().unwrap_or_else(|e| e.into_inner()).get(&update.poll_id).cloned() else { return };
    let tl::enums::PollResults::Results(results) = &update.results;
    // A poll that closed is drawn as it ended; one whose definition came with the update is redrawn from it.
    match &update.poll {
        Some(tl::enums::Poll::Poll(poll)) => announce_any(state, &buffer_id, msg_id, poll, results),
        None => {
            if let Some(mut live) = state.runtime.live_card(&buffer_id, "poll").filter(|c| c["id"].as_str() == Some(&msg_id.to_string())) {
                let mut chosen_id: Option<String> = None;
                if let Some(options) = live["options"].as_array_mut() {
                    for option in options.iter_mut() {
                        let id = option["id"].as_str().unwrap_or("").to_string();
                        let found = results.results.iter().flatten().find_map(|r| {
                            let tl::enums::PollAnswerVoters::Voters(v) = r;
                            (option_id(&v.option) == id).then_some((v.voters.unwrap_or(0), v.chosen))
                        });
                        if let Some((votes, chosen)) = found {
                            option["votes"] = json!(votes);
                            if chosen {
                                chosen_id = Some(id);
                            }
                        }
                    }
                }
                if let Some(id) = chosen_id {
                    live["votedOptionId"] = json!(id);
                    live["hasVoted"] = json!(true);
                }
                state.runtime.set_live_card(&buffer_id, "poll", live.clone());
                state.events.emit("pollCard", json!({ "bufferId": buffer_id, "kind": "poll", "poll": live }));
            }
        }
    }
}

/// Redraws a poll that may have closed: the card says so rather than vanishing.
fn announce_any(state: &AppState, buffer_id: &str, msg_id: i32, poll: &tl::types::Poll, results: &tl::types::PollResults) {
    let card = card(buffer_id, msg_id, poll, results);
    state.runtime.set_live_card(buffer_id, "poll", card.clone());
    state.events.emit("pollCard", json!({ "bufferId": buffer_id, "kind": "poll", "poll": card }));
}

/// Votes for an answer in the poll the conversation's card shows.
pub async fn vote(state: &AppState, buffer: &Buffer, answer: &str) -> Result<(), String> {
    let card = state.runtime.live_card(&buffer.id, "poll").ok_or_else(|| "there is no poll open here".to_string())?;
    let msg_id: i32 = card["id"].as_str().and_then(|i| i.parse().ok()).ok_or_else(|| "that poll cannot be voted in".to_string())?;
    let option = hex::decode(answer).map_err(|_| "that is not one of the poll's answers".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    handle.client.invoke(&tl::functions::messages::SendVote { peer: peer.into(), msg_id, options: vec![option] }).await.map_err(|e| explain(&e))?;
    // Said at once; Telegram's own update with the new tally follows.
    let mut live = card;
    live["hasVoted"] = json!(true);
    live["votedOptionId"] = json!(answer);
    if let Some(options) = live["options"].as_array_mut() {
        for o in options.iter_mut() {
            if o["id"].as_str() == Some(answer) {
                o["votes"] = json!(o["votes"].as_i64().unwrap_or(0) + 1);
            }
        }
    }
    state.runtime.set_live_card(&buffer.id, "poll", live.clone());
    state.events.emit("pollCard", json!({ "bufferId": buffer.id, "kind": "poll", "poll": live }));
    Ok(())
}

/// Telegram's limits on a poll: a question of 255 characters, answers of 100, two to ten of them.
const QUESTION_MAX: usize = 255;
const ANSWER_MAX: usize = 100;
const ANSWERS: std::ops::RangeInclusive<usize> = 2..=10;

/// What a poll somebody is starting is, checked and as Telegram's own type. `disclosed` is whether the running
/// count shows before it closes.
pub fn build(question: &str, answers: &[String], disclosed: bool) -> Result<tl::types::Poll, String> {
    let question = question.trim();
    let answers: Vec<&str> = answers.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
    if question.is_empty() {
        return Err("A poll needs a question".to_string());
    }
    if question.chars().count() > QUESTION_MAX {
        return Err(format!("The question can be at most {QUESTION_MAX} characters"));
    }
    if !ANSWERS.contains(&answers.len()) {
        return Err("Telegram polls have between 2 and 10 answers".to_string());
    }
    if answers.iter().any(|a| a.chars().count() > ANSWER_MAX) {
        return Err(format!("An answer can be at most {ANSWER_MAX} characters"));
    }
    let plain = |t: &str| tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities { text: t.to_string(), entities: Vec::new() });
    Ok(tl::types::Poll {
        id: 0,
        closed: false,
        // Anonymous, as Telegram's own apps start one.
        public_voters: false,
        multiple_choice: false,
        quiz: false,
        open_answers: false,
        revoting_disabled: false,
        shuffle_answers: false,
        hide_results_until_close: !disclosed,
        creator: false,
        subscribers_only: false,
        question: plain(question),
        answers: answers.iter().enumerate().map(|(i, a)| tl::enums::PollAnswer::Answer(tl::types::PollAnswer { text: plain(a), option: vec![i as u8], media: None, added_by: None, date: None })).collect(),
        close_period: None,
        close_date: None,
        hash: 0,
        countries_iso2: None,
    })
}

/// Starts a poll in a conversation.
pub async fn start(state: &AppState, buffer: &Buffer, question: &str, answers: &[String], disclosed: bool) -> Result<(), String> {
    let poll = build(question, answers, disclosed)?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let media = tl::types::InputMediaPoll { poll: tl::enums::Poll::Poll(poll), correct_answers: None, attached_media: None, solution: None, solution_entities: None, solution_media: None };
    let input = grammers_client::message::InputMessage::new().media(tl::enums::InputMedia::Poll(Box::new(media)));
    let sent = handle.client.send_message(peer, input).await.map_err(|e| explain(&e))?;
    super::messages::record(state, &buffer.account_id, &handle.own, &sent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> tl::enums::TextWithEntities {
        tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities { text: s.into(), entities: vec![] })
    }

    fn answer(label: &str, option: &[u8]) -> tl::enums::PollAnswer {
        tl::enums::PollAnswer::Answer(tl::types::PollAnswer { text: text(label), option: option.to_vec(), media: None, added_by: None, date: None })
    }

    fn poll(closed: bool) -> tl::types::Poll {
        tl::types::Poll {
            id: 77,
            closed,
            public_voters: false,
            multiple_choice: false,
            quiz: false,
            open_answers: false,
            revoting_disabled: false,
            shuffle_answers: false,
            hide_results_until_close: false,
            creator: false,
            subscribers_only: false,
            question: text("Lunch?"),
            answers: vec![answer("Pizza", b"0"), answer("Soup", b"1")],
            close_period: None,
            close_date: None,
            hash: 0,
            countries_iso2: None,
        }
    }

    fn voters(option: &[u8], n: i32, chosen: bool) -> tl::enums::PollAnswerVoters {
        tl::enums::PollAnswerVoters::Voters(tl::types::PollAnswerVoters { chosen, correct: false, option: option.to_vec(), voters: Some(n), recent_voters: None })
    }

    #[test]
    fn a_poll_that_is_started_is_checked() {
        let a = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ok = build(" Lunch? ", &a(&["Pizza", " Soup ", ""]), false).unwrap();
        assert_eq!(ok.answers.len(), 2, "an empty answer is not one");
        assert!(ok.hide_results_until_close);
        assert!(!ok.public_voters);
        assert!(build("", &a(&["a", "b"]), true).is_err());
        assert!(build("Q", &a(&["only"]), true).is_err());
        assert!(build("Q", &a(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"]), true).is_err());
        assert!(build(&"x".repeat(256), &a(&["a", "b"]), true).is_err());
        assert!(!build("Q", &a(&["a", "b"]), true).unwrap().hide_results_until_close);
    }

    #[test]
    fn a_poll_becomes_a_card_with_counts_and_the_accounts_own_choice() {
        let results = tl::types::PollResults { min: false, has_unread_votes: false, can_view_stats: false, results: Some(vec![voters(b"0", 3, false), voters(b"1", 5, true)]), total_voters: Some(8), recent_voters: None, solution: None, solution_entities: None, solution_media: None };
        let card = card("telegram:1|Lunch", 42, &poll(false), &results);
        assert_eq!((card["title"].as_str(), card["id"].as_str(), card["open"].as_bool()), (Some("Lunch?"), Some("42"), Some(true)));
        assert_eq!(card["options"][0]["votes"], 3);
        assert_eq!(card["options"][1]["votes"], 5);
        assert_eq!((card["hasVoted"].as_bool(), card["votedOptionId"].as_str()), (Some(true), Some(option_id(b"1").as_str())));
        let closed = super::card("b", 1, &poll(true), &results);
        assert_eq!(closed["open"], false);
    }

    #[test]
    fn a_poll_nobody_voted_in_has_zeros_and_no_choice() {
        let results = tl::types::PollResults { min: false, has_unread_votes: false, can_view_stats: false, results: None, total_voters: None, recent_voters: None, solution: None, solution_entities: None, solution_media: None };
        let card = card("b", 1, &poll(false), &results);
        assert_eq!(card["options"][0]["votes"], 0);
        assert_eq!(card["hasVoted"], false);
    }
}
