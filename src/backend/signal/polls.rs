//! Polls: a question with options, asked in a message; people vote (a later vote of theirs replaces the earlier one,
//! told apart by its count) and the author can end it. Drawn as the window's poll card, the one Matrix and Kick polls
//! use, and kept in the account's file so a poll outlives a restart.

use super::ids;
use super::people::Ctx;
use crate::model::Buffer;
use presage::proto::data_message::{PollCreate, PollTerminate, PollVote};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// One poll and what has been said in it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Poll {
    pub question: String,
    pub options: Vec<String>,
    pub multiple: bool,
    /// Each voter's choices and the count their vote carried (a newer vote has a higher count).
    pub votes: BTreeMap<String, (Vec<u32>, u32)>,
    pub ended: bool,
}

impl Poll {
    pub fn new(create: &PollCreate) -> Self {
        Poll { question: create.question.clone().unwrap_or_default(), options: create.options.clone(), multiple: create.allow_multiple.unwrap_or(false), ..Default::default() }
    }

    /// Takes a vote, unless an earlier one of the same person's arrives late or the poll has ended. Whether it counted.
    pub fn vote(&mut self, voter: &Uuid, choices: &[u32], count: u32) -> bool {
        if self.ended {
            return false;
        }
        let key = voter.to_string();
        if self.votes.get(&key).is_some_and(|(_, seen)| *seen >= count) {
            return false;
        }
        let mut choices: Vec<u32> = choices.iter().copied().filter(|i| (*i as usize) < self.options.len()).collect();
        if !self.multiple {
            choices.truncate(1);
        }
        self.votes.insert(key, (choices, count));
        true
    }

    /// How many people chose each option.
    pub fn tally(&self) -> Vec<usize> {
        let mut counts = vec![0; self.options.len()];
        for (choices, _) in self.votes.values() {
            for i in choices {
                if let Some(c) = counts.get_mut(*i as usize) {
                    *c += 1;
                }
            }
        }
        counts
    }

    /// The count the account's next vote carries.
    pub fn next_count(&self, voter: &Uuid) -> u32 {
        self.votes.get(&voter.to_string()).map(|(_, n)| n + 1).unwrap_or(1)
    }

    /// The window's poll card.
    pub fn card(&self, buffer_id: &str, poll_id: &str, own: &Uuid) -> serde_json::Value {
        let tally = self.tally();
        let options: Vec<serde_json::Value> = self.options.iter().enumerate().map(|(i, label)| serde_json::json!({ "id": i.to_string(), "label": label, "votes": tally[i] })).collect();
        let chosen: Vec<String> = self.votes.get(&own.to_string()).map(|(c, _)| c.iter().map(|i| i.to_string()).collect()).unwrap_or_default();
        let mine = chosen.first().cloned();
        serde_json::json!({
            "bufferId": buffer_id,
            "kind": "poll",
            "id": poll_id,
            "title": self.question,
            "options": options,
            "open": !self.ended,
            "duration": 0,
            "remaining": 0,
            "resultDisplayDuration": 0,
            "hasVoted": mine.is_some(),
            "votedOptionId": mine,
            // Signal's own: several answers where the poll allows it, all of them marked; and a vote can be changed.
            "multiple": self.multiple,
            "votedOptionIds": chosen,
            "changeable": true,
            // Only whoever asked a Signal poll can end it, so nobody else is offered the button.
            "noEnd": ids::parse_message_id(poll_id).is_none_or(|(_, author)| author != *own),
        })
    }
}

fn key(poll_id: &str) -> String {
    format!("poll:{poll_id}")
}

pub fn load(ctx: &Ctx<'_>, poll_id: &str) -> Option<Poll> {
    ctx.store.blob(&key(poll_id)).ok().flatten().and_then(|b| serde_json::from_slice(&b).ok())
}

fn save(ctx: &Ctx<'_>, poll_id: &str, poll: &Poll) {
    if let Ok(bytes) = serde_json::to_vec(poll) {
        let _ = ctx.store.set_blob(&key(poll_id), &bytes);
    }
}

/// Draws a poll and keeps it, for the conversation's past polls as well.
pub fn announce(ctx: &Ctx<'_>, buffer: &Buffer, poll_id: &str, poll: &Poll) {
    let card = poll.card(&buffer.id, poll_id, &ctx.own);
    ctx.state.runtime.set_live_card(&buffer.id, "poll", card.clone());
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    if let Err(e) = ctx.state.store.record_live_card(&buffer.id, "poll", poll_id, &poll.question, &card.to_string(), at) {
        tracing::debug!("signal: keeping a poll: {e:#}");
    }
    ctx.state.events.emit("pollCard", serde_json::json!({ "bufferId": buffer.id, "kind": "poll", "poll": card }));
}

/// A new poll, asked by `author` in the message sent at `at`.
pub fn created(ctx: &Ctx<'_>, buffer: &Buffer, author: &Uuid, at: u64, create: &PollCreate) {
    let poll_id = ids::message_id(at, author);
    let poll = load(ctx, &poll_id).unwrap_or_else(|| Poll::new(create));
    save(ctx, &poll_id, &poll);
    announce(ctx, buffer, &poll_id, &poll);
}

/// A vote by `voter`.
pub fn voted(ctx: &Ctx<'_>, buffer: &Buffer, voter: &Uuid, vote: &PollVote) {
    let (Some(author), Some(ts)) = (vote.target_author_aci_binary.as_deref().and_then(|b| Uuid::from_slice(b).ok()), vote.target_sent_timestamp) else { return };
    let poll_id = ids::message_id(ts, &author);
    let Some(mut poll) = load(ctx, &poll_id) else { return };
    if poll.vote(voter, &vote.option_indexes, vote.vote_count.unwrap_or(1)) {
        save(ctx, &poll_id, &poll);
        announce(ctx, buffer, &poll_id, &poll);
    }
}

/// The poll's author ended it.
pub fn ended(ctx: &Ctx<'_>, buffer: &Buffer, author: &Uuid, end: &PollTerminate) {
    let Some(ts) = end.target_sent_timestamp else { return };
    let poll_id = ids::message_id(ts, author);
    let Some(mut poll) = load(ctx, &poll_id) else { return };
    poll.ended = true;
    save(ctx, &poll_id, &poll);
    announce(ctx, buffer, &poll_id, &poll);
}

/// The vote the account casts for option `choice` of the poll `poll_id`, and the poll as it will be once sent.
pub fn own_vote(ctx: &Ctx<'_>, poll_id: &str, choices: &[u32]) -> Result<(PollVote, Poll), String> {
    let mut poll = load(ctx, poll_id).ok_or("that poll is not known here")?;
    if poll.ended {
        return Err("That poll has ended".to_string());
    }
    let (ts, author) = ids::parse_message_id(poll_id).ok_or("that is not a Signal poll")?;
    let count = poll.next_count(&ctx.own);
    if choices.len() > 1 && !poll.multiple {
        return Err("That poll takes one answer".to_string());
    }
    if choices.iter().any(|c| *c as usize >= poll.options.len()) {
        return Err("That poll has no such answer".to_string());
    }
    poll.vote(&ctx.own, choices, count);
    let vote = PollVote { target_author_aci_binary: Some(author.as_bytes().to_vec()), target_sent_timestamp: Some(ts), option_indexes: choices.to_vec(), vote_count: Some(count) };
    Ok((vote, poll))
}

/// Keeps the account's own vote once it was sent, and draws it.
pub fn keep_own(ctx: &Ctx<'_>, buffer: &Buffer, poll_id: &str, poll: &Poll) {
    save(ctx, poll_id, poll);
    announce(ctx, buffer, poll_id, poll);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll(multiple: bool) -> Poll {
        Poll::new(&PollCreate { question: Some("Lunch?".into()), allow_multiple: Some(multiple), options: vec!["Pizza".into(), "Soup".into(), "Salad".into()] })
    }

    #[test]
    fn a_later_vote_replaces_an_earlier_one_and_a_late_old_one_does_not() {
        let mut p = poll(false);
        let sam = Uuid::from_u128(1);
        assert!(p.vote(&sam, &[0], 1));
        assert!(p.vote(&sam, &[2], 2));
        assert!(!p.vote(&sam, &[1], 1), "an older vote arriving late");
        assert_eq!(p.tally(), vec![0, 0, 1]);
        assert_eq!(p.next_count(&sam), 3);
    }

    #[test]
    fn one_choice_unless_the_poll_allows_more_and_nothing_after_it_ends() {
        let mut single = poll(false);
        single.vote(&Uuid::from_u128(1), &[0, 1, 9], 1);
        assert_eq!(single.tally(), vec![1, 0, 0]);
        let mut many = poll(true);
        many.vote(&Uuid::from_u128(1), &[0, 1, 9], 1);
        many.vote(&Uuid::from_u128(2), &[1], 1);
        assert_eq!(many.tally(), vec![1, 2, 0]);
        many.ended = true;
        assert!(!many.vote(&Uuid::from_u128(3), &[2], 1));
    }

    #[test]
    fn the_card_says_what_the_account_chose_and_whether_it_is_open() {
        let mut p = poll(false);
        let me = Uuid::from_u128(7);
        p.vote(&me, &[1], 1);
        let card = p.card("buf", "1-x", &me);
        assert_eq!((card["title"].as_str(), card["hasVoted"].as_bool(), card["votedOptionId"].as_str(), card["open"].as_bool()), (Some("Lunch?"), Some(true), Some("1"), Some(true)));
        assert_eq!(card["options"][1]["votes"], 1);
    }

    #[test]
    fn a_poll_that_allows_several_answers_marks_each_and_a_vote_can_be_taken_back() {
        let mut p = poll(true);
        let me = Uuid::from_u128(7);
        p.vote(&me, &[0, 2], 1);
        let card = p.card("buf", "1-x", &me);
        assert_eq!((card["multiple"].as_bool(), card["changeable"].as_bool()), (Some(true), Some(true)));
        assert_eq!(card["votedOptionIds"], serde_json::json!(["0", "2"]));
        assert_eq!(p.tally(), vec![1, 0, 1]);
        // Every answer unticked: the vote is taken back.
        p.vote(&me, &[], 2);
        assert_eq!(p.tally(), vec![0, 0, 0]);
        assert_eq!(p.card("buf", "1-x", &me)["votedOptionIds"], serde_json::json!([]));
    }

    #[test]
    fn only_the_account_s_own_poll_offers_ending_it() {
        let p = poll(false);
        let me = Uuid::from_u128(7);
        let mine = ids::message_id(1_700_000_000_000, &me);
        let theirs = ids::message_id(1_700_000_000_000, &Uuid::from_u128(8));
        assert_eq!(p.card("buf", &mine, &me)["noEnd"], false);
        assert_eq!(p.card("buf", &theirs, &me)["noEnd"], true);
    }
}
