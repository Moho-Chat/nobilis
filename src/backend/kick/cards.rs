//! Polls and predictions, which are the same shape wearing two hats.
//!
//! Both are a question with options and a clock, both are shown over the chat
//! rather than in it, and both keep changing after they arrive - so both are
//! published as a card that gets refreshed rather than as a line that scrolls
//! away.

use super::*;

/// The poll in a channel, or its absence, in the shape the card reads.
///
/// Absence is a message rather than silence: a poll that has been taken down
/// has to leave the screen, and a client that only ever heard about polls
/// starting would keep showing one that ended an hour ago.
pub fn announce_poll(state: &AppState, buffer_id: &str, poll: Option<&api::Poll>) {
    let card = poll.map(|poll| {
        serde_json::json!({
            "bufferId": buffer_id,
            "kind": "poll",
            "id": card_id(state, buffer_id, "poll", &poll.title, poll.duration, poll.remaining),
            "title": poll.title,
            "options": poll.options.iter().map(|o| serde_json::json!({
                "id": o.id,
                "label": o.label,
                "votes": o.votes,
            })).collect::<Vec<_>>(),
            "duration": poll.duration,
            // Seconds left when this was written. The client counts down from
            // it rather than asking Kick every second.
            "remaining": poll.remaining,
            "resultDisplayDuration": poll.result_display_duration,
            "hasVoted": poll.has_voted,
            "votedOptionId": poll.voted_option_id,
        })
    });
    publish_card(state, buffer_id, "poll", card);
}

/// Says the prediction is over, so the card stops offering to back it.
pub fn announce_prediction_gone(state: &AppState, buffer_id: &str) {
    publish_card(state, buffer_id, "prediction", None);
}

/// The same for a prediction, which is a poll with money on it.
pub fn announce_prediction(state: &AppState, buffer_id: &str, payload: &serde_json::Value) {
    // Kick sends the whole prediction on its own broadcast channel, in the
    // same shape its REST endpoints answer with - so one parser reads both.
    let found = serde_json::from_value::<api::Prediction>(payload.get("prediction").unwrap_or(payload).clone());
    let prediction = match found {
        Ok(prediction) => prediction,
        Err(e) => {
            tracing::debug!("kick: prediction payload not understood: {e}");
            return;
        }
    };

    // The broadcast carries the prediction and nothing about you: not your
    // bet, not your points. Both were known a moment ago if this is the same
    // prediction moving, so they are carried across rather than blanked -
    // otherwise somebody else's bet would wipe yours off the card.
    let known = state.runtime.live_card(buffer_id, "prediction");
    let same = known
        .as_ref()
        .and_then(|card| card["id"].as_str().map(|id| id == format!("prediction:{}", prediction.id)))
        .unwrap_or(false);
    let mut vote = None;
    let mut points = None;
    if same {
        if let Some(card) = known.as_ref() {
            vote = card["votedOptionId"].as_str().map(|outcome| api::PredictionVote {
                outcome_id: outcome.to_string(),
                total_vote_amount: card["stake"].as_f64().unwrap_or_default(),
            });
            points = card["balance"].as_i64();
        }
    }
    announce_prediction_card(state, buffer_id, &prediction, vote.as_ref(), points);

    // A prediction this client has not seen before: ask once for the two
    // things the broadcast cannot say. Once per prediction rather than once
    // per bet, which is the difference between a request and a flood.
    // Through the handle rather than `tokio::spawn`, because this is called
    // from a plain function that the tests drive with no runtime under it -
    // and a card that draws correctly in a test is worth more than a panic
    // proving there was nowhere to send the request.
    if !same {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (state, buffer_id) = (state.clone(), buffer_id.to_string());
            runtime.spawn(async move { refresh_prediction(&state, &buffer_id).await });
        }
    }
}

/// A prediction in the shape the card reads.
///
/// The two things the card cannot work out for itself are carried alongside:
/// what this account has riding on it, and what it has left to bet with.
pub fn announce_prediction_card(
    state: &AppState,
    buffer_id: &str,
    prediction: &api::Prediction,
    vote: Option<&api::PredictionVote>,
    points: Option<i64>,
) {
    publish_card(state, buffer_id, "prediction", Some(prediction_card(prediction, vote, points)));
}

/// The card, from Kick's own prediction object.
pub(super) fn prediction_card(
    prediction: &api::Prediction,
    vote: Option<&api::PredictionVote>,
    points: Option<i64>,
) -> serde_json::Value {
    let open = prediction.state.eq_ignore_ascii_case("ACTIVE");
    let started = prediction.created_at.as_deref().and_then(api::parse_kick_datetime);
    // How long is left, from when it started rather than from when this
    // arrived: a prediction event carries no clock of its own.
    let remaining = match (open, started) {
        (true, Some(started)) => {
            let gone = now_secs().saturating_sub(started);
            (prediction.duration as i64 - gone).max(0) as u32
        }
        (true, None) => prediction.duration,
        // Locked, resolved or cancelled: the betting is over whatever the
        // clock says.
        _ => 0,
    };
    let staked: f64 = prediction.outcomes.iter().map(|o| o.total_vote_amount).sum();
    let options: Vec<serde_json::Value> = prediction
        .outcomes
        .iter()
        .map(|outcome| {
            serde_json::json!({
                "id": outcome.id,
                "label": outcome.title,
                // Points where a poll counts votes: the same bar, measuring
                // the thing this service measures.
                "votes": outcome.total_vote_amount,
                "backers": outcome.vote_count,
                // Kick's own page writes the rate this way rather than
                // sending it as a phrase.
                "odds": (outcome.return_rate > 0.0).then(|| format!("1:{:.1}", outcome.return_rate)),
                "winner": prediction.winning_outcome_id.as_deref() == Some(outcome.id.as_str()),
            })
        })
        .collect();

    // What this bet would come back as, at the rate the outcome is paying
    // now. Kick shows the same number and marks it as an estimate while the
    // betting is open, because every later bet moves it.
    let your_return = vote.and_then(|vote| {
        prediction
            .outcomes
            .iter()
            .find(|o| o.id == vote.outcome_id)
            .map(|o| vote.total_vote_amount * o.return_rate)
    });

    serde_json::json!({
        "kind": "prediction",
        // Kick names these, so the card takes its name rather than inventing
        // one from the clock the way a poll has to.
        "id": format!("prediction:{}", prediction.id),
        "title": prediction.title,
        "options": options,
        "duration": prediction.duration,
        "remaining": remaining,
        "resultDisplayDuration": 0,
        "hasVoted": vote.is_some(),
        "votedOptionId": vote.map(|v| v.outcome_id.clone()),
        "stake": vote.map(|v| v.total_vote_amount),
        "total": staked,
        "yourReturn": your_return,
        "state": prediction.state,
        "balance": points,
        "minBet": api::MIN_PREDICTION_BET,
    })
}

/// Reads the prediction a channel has going, for somebody opening it.
///
/// The events say when one starts and changes; this is how a window that
/// arrives mid-way learns there is one at all - and the only place the two
/// things the card cannot compute come from: this account's own bet, and the
/// points it has left.
pub async fn refresh_prediction(state: &AppState, buffer_id: &str) {
    let Some(channel) = channel_when_ready(state, buffer_id).await else { return };
    let Ok(http) = api::client() else { return };
    let token = state
        .runtime
        .get_buffer(buffer_id)
        .and_then(|b| state.accounts.get_kick(&b.account_id))
        .and_then(|c| c.token)
        .filter(|t| !t.is_empty());
    match api::prediction_latest(&http, token.as_deref(), &channel.slug).await {
        Ok(Some((prediction, vote))) => {
            let points = match token.as_deref() {
                Some(token) => api::points(&http, token, &channel.slug).await.ok(),
                None => None,
            };
            announce_prediction_card(state, buffer_id, &prediction, vote.as_ref(), points);
        }
        // Nothing running, and nothing to show.
        Ok(None) => publish_card(state, buffer_id, "prediction", None),
        Err(e) => tracing::debug!("kick: reading {}'s prediction: {e:#}", channel.slug),
    }
}

/// Stores a card, writes it down, and tells the client - or says it is gone.
pub(super) fn publish_card(state: &AppState, buffer_id: &str, kind: &str, card: Option<serde_json::Value>) {
    // When this was true, stamped here so every card carries one.
    //
    // The clock is the whole point: `remaining` is a number of seconds that
    // was accurate at one moment, and a client told only the number counts
    // down from whenever it happened to hear it. Replay a stored card into a
    // window opened an hour later and the poll appears to have its full time
    // left - which is exactly what a poll timer must never do.
    let card = card.map(|mut card| {
        card["asOf"] = serde_json::json!(now_secs());
        // Which conversation it belongs to, stamped here rather than by each
        // builder: a card read back out of storage is matched against the
        // open channel by this field, and a prediction that forgot to carry
        // one was recalled from the menu into nothing.
        card["bufferId"] = serde_json::json!(buffer_id);
        card
    });
    match &card {
        None => state.runtime.forget_live_card(buffer_id, kind),
        Some(card) => {
            state.runtime.set_live_card(buffer_id, kind, card.clone());
            // Written down as it changes, so what is read back later is how
            // it finished rather than how it opened.
            let id = card["id"].as_str().unwrap_or_default().to_string();
            let title = card["title"].as_str().unwrap_or_default().to_string();
            let ts = now_secs();
            if let Err(e) = state.store.record_live_card(buffer_id, kind, &id, &title, &card.to_string(), ts) {
                tracing::debug!("kick: keeping the {kind}: {e:#}");
            }
        }
    }
    state.events.emit("pollCard", serde_json::json!({
        "bufferId": buffer_id,
        "kind": kind,
        "poll": card.unwrap_or(serde_json::Value::Null),
    }));
}

/// Whether a stored card still has anything to say.
///
/// Its own function because two places ask: the replay into a window that has
/// just opened, and the tests. A card is current while its clock is running
/// and for as long as the service leaves the result up afterwards.
pub fn card_is_current(card: &serde_json::Value) -> bool {
    let seconds = |key: &str| card[key].as_i64().unwrap_or(0);
    let as_of = seconds("asOf");
    if as_of == 0 {
        // Stamped by every card this backend makes; anything without one is
        // from a version that did not, and its clock cannot be trusted.
        return false;
    }
    let alive = seconds("remaining") + seconds("resultDisplayDuration");
    now_secs() - as_of <= alive
}

/// The wall clock, in seconds, as everything here writes it down.
pub(super) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// What identifies one poll or prediction for as long as it runs.
///
/// Kick names neither of them, so the moment it started does: the same card
/// keeps its id while the votes come in, and the next one - even with the
/// same question - gets its own. The id already in flight wins where the
/// title matches, since a second of drift in `remaining` must not split one
/// poll into two.
pub(super) fn card_id(state: &AppState, buffer_id: &str, kind: &str, title: &str, duration: u32, remaining: u32) -> String {
    if let Some(open) = state.runtime.live_card(buffer_id, kind) {
        if open["title"].as_str() == Some(title) {
            if let Some(id) = open["id"].as_str() {
                return id.to_string();
            }
        }
    }
    format!("{kind}:{}", now_secs() - (duration.saturating_sub(remaining)) as i64)
}

/// Reads the poll a channel has running, for somebody who has just opened it./// Reads the poll a channel has running, for somebody who has just opened it.
///
/// A poll that started before you arrived is the common case - they run for a
/// minute and a chat is opened at any moment in it - and the events only tell
/// you about the ones that change while you are watching.
pub async fn refresh_poll(state: &AppState, buffer_id: &str) {
    let Some(channel) = channel_when_ready(state, buffer_id).await else { return };
    let Ok(http) = api::client() else { return };
    let token = state
        .runtime
        .get_buffer(buffer_id)
        .and_then(|b| state.accounts.get_kick(&b.account_id))
        .and_then(|c| c.token)
        .filter(|t| !t.is_empty());
    match api::poll(&http, token.as_deref(), &channel.slug).await {
        Ok(poll) => announce_poll(state, buffer_id, poll.as_ref()),
        Err(e) => tracing::debug!("kick: reading {}'s poll: {e:#}", channel.slug),
    }
}

/// How often the followed channels are asked about as a group.
pub(super) const LIVE_POLL: Duration = Duration::from_secs(60);

/// The id the line for this poll keeps, so later updates find it.
///
/// Kick's own poll id where there is one; the channel otherwise, since a
/// channel runs one poll at a time and a stable-but-approximate id is better
/// than a fresh line per vote.
pub(super) fn poll_message_id(slug: &str, payload: &serde_json::Value) -> String {
    let poll = payload.get("poll").unwrap_or(payload);
    match poll.get("id").and_then(|v| v.as_u64()) {
        Some(id) => format!("kick-poll-{slug}-{id}"),
        None => format!("kick-poll-{slug}"),
    }
}

pub(super) fn prediction_message_id(slug: &str, payload: &serde_json::Value) -> String {
    let prediction = payload.get("prediction").unwrap_or(payload);
    match prediction.get("id").and_then(|v| v.as_u64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_string))) {
        Some(id) => format!("kick-prediction-{slug}-{id}"),
        None => format!("kick-prediction-{slug}"),
    }
}

/// A prediction, as one readable line.
///
/// Written from the shape a prediction has rather than from a payload anybody
/// has seen: a title, and outcomes that carry a name and some count of what
/// has been staked on them. Kick does not document this and this client has
/// never received one, so every field is optional and a payload that does not
/// match produces nothing - which shows the chat as it was rather than a line
/// of empty brackets.
pub(super) fn describe_prediction(payload: &serde_json::Value) -> Option<String> {
    let prediction = payload.get("prediction").unwrap_or(payload);
    let title = prediction
        .get("title")
        .or_else(|| prediction.get("question"))
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())?;

    let outcomes: Vec<String> = prediction
        .get("outcomes")
        .or_else(|| prediction.get("options"))
        .and_then(|v| v.as_array())
        .map(|outcomes| {
            outcomes
                .iter()
                .filter_map(|o| {
                    let label = o.get("label").or_else(|| o.get("title")).or_else(|| o.get("name"))?.as_str()?;
                    let staked = ["votes", "points", "total", "amount"]
                        .iter()
                        .find_map(|field| o.get(field).and_then(|v| v.as_u64()));
                    Some(match staked {
                        Some(n) => format!("{label} ({n})"),
                        None => label.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(if outcomes.is_empty() {
        format!("prediction: {title}")
    } else {
        format!("prediction: {title} — {}", outcomes.join(", "))
    })
}

/// A poll, as one readable line.
///
/// Tolerant about where the fields sit: Kick nests this under `poll` on the
/// update event and has sent it flat, and an event whose shape has moved on
/// should cost the line rather than the connection.
pub(super) fn describe_poll(payload: &serde_json::Value) -> Option<String> {
    let poll = payload.get("poll").unwrap_or(payload);
    let title = poll.get("title").and_then(|v| v.as_str()).filter(|t| !t.is_empty())?;
    let options: Vec<String> = poll
        .get("options")
        .and_then(|v| v.as_array())
        .map(|options| {
            options
                .iter()
                .filter_map(|o| {
                    let label = o.get("label").and_then(|v| v.as_str())?;
                    // The running tally where there is one, since a poll with
                    // no numbers is only half the thing being watched.
                    Some(match o.get("votes").and_then(|v| v.as_u64()) {
                        Some(votes) => format!("{label} ({votes})"),
                        None => label.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(if options.is_empty() {
        format!("poll: {title}")
    } else {
        format!("poll: {title} — {}", options.join(", "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::*;

    /// A poll heard about an hour ago has no time left, whatever the number
    /// it was carrying said - the number was true at a moment, and the moment
    /// is part of it.
    #[test]
    fn a_card_is_only_current_while_its_clock_is() {
        let running = serde_json::json!({ "asOf": now_secs() - 5, "remaining": 60, "resultDisplayDuration": 30 });
        assert!(card_is_current(&running));

        // Voting is over but the result is still up.
        let showing = serde_json::json!({ "asOf": now_secs() - 70, "remaining": 60, "resultDisplayDuration": 30 });
        assert!(card_is_current(&showing));

        let gone = serde_json::json!({ "asOf": now_secs() - 3600, "remaining": 60, "resultDisplayDuration": 30 });
        assert!(!card_is_current(&gone));

        // No stamp at all: written by a version that did not carry one, and
        // its clock cannot be believed.
        assert!(!card_is_current(&serde_json::json!({ "remaining": 60 })));
    }

    /// The card is built from Kick's own prediction object, copied field
    /// for field off the endpoint its viewer panel reads - ULID ids, points
    /// where a poll counts votes, and a payout rate rather than a phrase.
    #[test]
    fn a_prediction_becomes_the_same_card_a_poll_does() {
        let state = simulated_daemon("prediction-card");
        let buffer_id = crate::model::buffer_id("kick:tester", "odablock");
        announce_prediction(&state, &buffer_id, &serde_json::json!({
            "prediction": {
                "id": "01M1PQXD465ZFG2RJJ2FE4166R",
                "channel_id": 54194893,
                "title": "who will win?",
                "outcomes": [
                    { "id": "OUT1", "title": "guy (mike)", "total_vote_amount": 23800, "vote_count": 12, "return_rate": 1.5 },
                    { "id": "OUT2", "title": "dude (brandon)", "total_vote_amount": 13100, "vote_count": 7, "return_rate": 2.8 }
                ],
                "duration": 120,
                "created_at": "2026-09-04T17:36:55Z",
                "state": "RESOLVED",
                "winning_outcome_id": "OUT2"
            }
        }));
        let card = state.runtime.live_card(&buffer_id, "prediction").expect("a card");
        assert_eq!(card["kind"], "prediction");
        assert_eq!(card["id"], "prediction:01M1PQXD465ZFG2RJJ2FE4166R");
        assert_eq!(card["title"], "who will win?");
        assert_eq!(card["total"], 36900.0);
        assert_eq!(card["options"][0]["label"], "guy (mike)");
        assert_eq!(card["options"][0]["votes"], 23800.0);
        assert_eq!(card["options"][0]["backers"], 12);
        assert_eq!(card["options"][0]["odds"], "1:1.5");
        assert_eq!(card["options"][1]["winner"], true);
        // Resolved, so nothing is left to bet on however long it ran.
        assert_eq!(card["remaining"], 0);

        let kept = state.store.live_cards(&buffer_id, "prediction", 10).expect("history");
        assert_eq!(kept.len(), 1);
        assert!(kept[0].0.contains("who will win?"));
    }

    /// What this account has on it, and what it stands to get back - the two
    /// things no broadcast carries and the card cannot work out alone.
    #[test]
    fn a_bet_of_your_own_shows_what_it_would_return() {
        let state = simulated_daemon("prediction-bet");
        let buffer_id = crate::model::buffer_id("kick:tester", "odablock");
        let prediction = api::Prediction {
            id: "P1".into(),
            title: "who will win?".into(),
            outcomes: vec![api::PredictionOutcome {
                id: "OUT1".into(),
                title: "guy".into(),
                total_vote_amount: 500.0,
                vote_count: 2,
                return_rate: 2.0,
            }],
            duration: 120,
            created_at: None,
            state: "ACTIVE".into(),
            winning_outcome_id: None,
        };
        let vote = api::PredictionVote { outcome_id: "OUT1".into(), total_vote_amount: 250.0 };
        announce_prediction_card(&state, &buffer_id, &prediction, Some(&vote), Some(9_000));
        let card = state.runtime.live_card(&buffer_id, "prediction").expect("a card");
        assert_eq!(card["hasVoted"], true);
        assert_eq!(card["votedOptionId"], "OUT1");
        assert_eq!(card["stake"], 250.0);
        assert_eq!(card["yourReturn"], 500.0);
        assert_eq!(card["balance"], 9_000);
        assert_eq!(card["minBet"], 10);
    }

    /// Predictions arrive on a broadcast channel of their own, named with
    /// hyphens where the chat's are named with dots. Reading the channel id
    /// back out of it is what tells the card which conversation it belongs
    /// to - and getting it wrong is how a prediction goes unnoticed.
    #[test]
    fn a_prediction_subscription_names_its_channel() {
        assert!(matches!(id_in_subscription("predictions-channel-54194893"), Some(Subscribed::Channel(54194893))));
        assert!(matches!(id_in_subscription("channel.54194893"), Some(Subscribed::Channel(54194893))));
        assert!(matches!(id_in_subscription("chatrooms.53906513.v2"), Some(Subscribed::Chatroom(53906513))));
    }

    /// A poll is one thing happening over a minute    /// A poll is one thing happening over a minute, not a stream of events -
    /// Kick sends its update on every vote, and a line each would bury the
    /// conversation the poll is about.
    #[test]
    fn a_poll_is_one_line_that_keeps_up_with_the_votes() {
        let state = simulated_daemon("poll-votes");
        let mut watched = watching_odablock();

        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 5, "title": "next game?", "options": [
                { "label": "runescape", "votes": 3 },
                { "label": "chess", "votes": 1 }
            ]}
        }));
        assert_eq!(lines(&state, "odablock"), vec!["poll: next game? — runescape (3), chess (1)"]);

        // Somebody votes. Same poll, same line.
        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 5, "title": "next game?", "options": [
                { "label": "runescape", "votes": 9 },
                { "label": "chess", "votes": 1 }
            ]}
        }));
        assert_eq!(lines(&state, "odablock"), vec!["poll: next game? — runescape (9), chess (1)"]);
    }

    /// The line stays when the poll ends - what was asked and how it went is
    /// worth keeping - but stops claiming to be running.
    #[test]
    fn a_finished_poll_says_so() {
        let state = simulated_daemon("poll-ended");
        let mut watched = watching_odablock();
        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 5, "title": "next game?", "options": [{ "label": "runescape", "votes": 9 }] }
        }));
        feed(&state, &mut watched, "App\\Events\\PollDeleteEvent", serde_json::json!({ "poll": { "id": 5 } }));
        assert_eq!(lines(&state, "odablock"), vec!["poll ended: next game? — runescape (9)"]);
    }

    /// Two polls in a row are two lines: the id is part of what identifies
    /// the message, so the second does not overwrite the first.
    #[test]
    fn a_second_poll_does_not_overwrite_the_first() {
        let state = simulated_daemon("two-polls");
        let mut watched = watching_odablock();
        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 1, "title": "first", "options": [] }
        }));
        feed(&state, &mut watched, "App\\Events\\PollUpdateEvent", serde_json::json!({
            "poll": { "id": 2, "title": "second", "options": [] }
        }));
        assert_eq!(lines(&state, "odablock"), vec!["poll: first", "poll: second"]);
    }

    /// Predictions have never been seen by this client and Kick documents
    /// nothing, so the parser reads a title and named outcomes wherever they
    /// sit - and produces nothing at all rather than a line of empty brackets
    /// when the shape is not what it expects.
    #[test]
    fn a_prediction_reads_whichever_way_kick_writes_it() {
        let state = simulated_daemon("prediction");
        let mut watched = watching_odablock();

        feed(&state, &mut watched, "App\\Events\\PredictionUpdateEvent", serde_json::json!({
            "prediction": { "id": 3, "title": "will he win?", "outcomes": [
                { "label": "yes", "points": 400 },
                { "label": "no", "points": 120 }
            ]}
        }));
        assert_eq!(lines(&state, "odablock"), vec!["prediction: will he win? — yes (400), no (120)"]);

        feed(&state, &mut watched, "App\\Events\\PredictionDeleteEvent", serde_json::json!({ "prediction": { "id": 3 } }));
        assert_eq!(lines(&state, "odablock"), vec!["prediction closed: will he win? — yes (400), no (120)"]);
    }

    #[test]
    fn a_payload_that_is_not_a_prediction_produces_nothing() {
        let state = simulated_daemon("prediction-empty");
        let mut watched = watching_odablock();
        feed(&state, &mut watched, "App\\Events\\PredictionUpdateEvent", serde_json::json!({ "prediction": { "id": 8 } }));
        assert!(lines(&state, "odablock").is_empty());
    }

    #[test]
    fn reads_a_poll_wherever_kick_puts_it() {
        let nested = serde_json::json!({
            "poll": {
                "title": "what next",
                "options": [
                    { "label": "keep going", "votes": 12 },
                    { "label": "stop", "votes": 3 }
                ]
            }
        });
        assert_eq!(
            describe_poll(&nested).as_deref(),
            Some("poll: what next — keep going (12), stop (3)")
        );

        // Flat, which Kick has also sent.
        let flat = serde_json::json!({ "title": "yes or no", "options": [{ "label": "yes" }] });
        assert_eq!(describe_poll(&flat).as_deref(), Some("poll: yes or no — yes"));

        // A poll with no options is still worth announcing; one with no title
        // is not a poll anybody can read.
        assert_eq!(describe_poll(&serde_json::json!({ "title": "hm" })).as_deref(), Some("poll: hm"));
        assert_eq!(describe_poll(&serde_json::json!({ "options": [] })), None);
        assert_eq!(describe_poll(&serde_json::json!({})), None);
    }
}
