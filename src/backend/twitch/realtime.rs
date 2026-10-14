//! What Twitch says as it happens, beyond the chat: a channel that raids another,
//! the poll it runs, the prediction it opens.
//!
//! twitch.tv's own pages hear these over a websocket of their own (Hermes), by
//! subscribing to a topic per channel and kind of thing. It can be listened to
//! with no account at all - the subscriptions are accepted anonymously, which
//! is how a logged-out visitor sees a poll - so this works for every channel
//! every account has open. Like the GraphQL it is private and not Twitch's to
//! promise (see gql.rs and the plan, Gitea issue #55), and a socket that will
//! not open costs nothing but these cards.
//!
//! One socket per account (so it goes by that account's route), subscribed to
//! three topics per open channel: `raid.<id>`, `polls.<id>` and
//! `predictions-channel-v1.<id>`. Each message is a JSON string inside the
//! envelope; what it says is read by the plain functions below and the cards
//! are the ones Kick's polls and predictions already use.
//!
//! A poll or a prediction that began before the channel was opened is asked for
//! once, from the GraphQL, since the socket only says what changes.

use super::gql;
use crate::state::AppState;
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub const HERMES_URL: &str = "wss://hermes.twitch.tv/v1?clientId=kimne78kx3ncx6brgo4mv6wki5h1ko";

/// How long without a word (it sends a keepalive every 15 seconds) before the connection is taken for dead.
const SILENCE: Duration = Duration::from_secs(60);

/// What the connection is asked to do.
pub enum Watch {
    Add { channel: String, room_id: String },
    Remove { channel: String },
}

fn registry() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<Watch>>> {
    static REGISTRY: std::sync::OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<Watch>>>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Makes the way to tell an account's socket what to listen to. Called when
/// the account starts; the receiver is `run`'s.
pub fn register(account_id: &str) -> mpsc::UnboundedReceiver<Watch> {
    let (tx, rx) = mpsc::unbounded_channel();
    registry().lock().unwrap().insert(account_id.to_string(), tx);
    rx
}

/// Listens for a channel, now and after any reconnection.
pub fn watch(account_id: &str, channel: &str, room_id: &str) {
    if let Some(tx) = registry().lock().unwrap().get(account_id) {
        let _ = tx.send(Watch::Add { channel: channel.to_string(), room_id: room_id.to_string() });
    }
}

pub fn unwatch(account_id: &str, channel: &str) {
    if let Some(tx) = registry().lock().unwrap().get(account_id) {
        let _ = tx.send(Watch::Remove { channel: channel.to_string() });
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Raid,
    Polls,
    Predictions,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Raid, Kind::Polls, Kind::Predictions];

    fn topic(self, room_id: &str) -> String {
        match self {
            Kind::Raid => format!("raid.{room_id}"),
            Kind::Polls => format!("polls.{room_id}"),
            Kind::Predictions => format!("predictions-channel-v1.{room_id}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Reading what was said
// ---------------------------------------------------------------------------

/// What a message means.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// The channel is sending its viewers to another: who, and how many.
    Raid { id: String, login: String, display: String, viewers: Option<u64> },
    RaidCalledOff { id: String },
    /// A poll or prediction card, and whether it is the last word on it.
    Card { kind: &'static str, card: Value },
    /// The poll or prediction is over and filed away: take the card down.
    CardGone { kind: &'static str },
}

fn text<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_str())).filter(|s| !s.is_empty())
}

fn number(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_f64()))
}

/// Reads one pubsub message of the given kind. `now` is unix seconds, for the clock of a prediction (which carries
/// when it began and not how long is left).
pub fn read(kind: &str, payload: &Value, now: i64) -> Option<Event> {
    let what = payload.get("type")?.as_str()?;
    match kind {
        "raid" => {
            let raid = payload.get("raid")?;
            let id = text(raid, &["id"])?.to_string();
            match what {
                "raid_update_v2" => {
                    let login = text(raid, &["target_login"])?.to_string();
                    Some(Event::Raid {
                        display: text(raid, &["target_display_name"]).unwrap_or(&login).to_string(),
                        viewers: number(raid, &["viewer_count"]).map(|v| v as u64).filter(|v| *v > 0),
                        login,
                        id,
                    })
                }
                "raid_cancel_v2" => Some(Event::RaidCalledOff { id }),
                _ => None,
            }
        }
        "polls" => {
            let poll = payload.pointer("/data/poll")?;
            match what {
                "POLL_ARCHIVE" => Some(Event::CardGone { kind: "poll" }),
                _ => poll_card(poll, now).map(|card| Event::Card { kind: "poll", card }),
            }
        }
        "predictions" => {
            let event = payload.pointer("/data/event")?;
            let card = prediction_card(event, now)?;
            // Called off: nothing was decided, and there is nothing to show of it.
            if card["state"].as_str() == Some("canceled") {
                return Some(Event::CardGone { kind: "prediction" });
            }
            Some(Event::Card { kind: "prediction", card })
        }
        _ => None,
    }
}

fn parse_time(v: &Value, keys: &[&str]) -> Option<i64> {
    text(v, keys).and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|d| d.timestamp())
}

/// A poll in the shape the card reads, from either of the shapes Twitch has it in: the socket's
/// (`poll_id`, `choice_id`, `duration_seconds`) and the GraphQL's (`id`, `durationSeconds`).
pub fn poll_card(poll: &Value, _now: i64) -> Option<Value> {
    let id = text(poll, &["poll_id", "id"])?;
    let status = text(poll, &["status"]).unwrap_or("ACTIVE").to_uppercase();
    let open = status == "ACTIVE";
    let duration = number(poll, &["duration_seconds", "durationSeconds"]).unwrap_or(0.0).max(0.0) as u32;
    let remaining = if open { (number(poll, &["remaining_duration_milliseconds", "remainingDurationMilliseconds"]).unwrap_or(duration as f64 * 1000.0) / 1000.0).max(0.0).ceil() as u32 } else { 0 };
    let choices = poll.get("choices")?.as_array()?;
    let votes_of = |c: &Value| c.pointer("/votes/total").and_then(|t| t.as_f64()).unwrap_or(0.0);
    let top = choices.iter().map(votes_of).fold(0.0_f64, f64::max);
    let options: Vec<Value> = choices
        .iter()
        .filter_map(|c| {
            let id = text(c, &["choice_id", "id"])?;
            Some(json!({
                "id": id,
                "label": text(c, &["title"]).unwrap_or(""),
                "votes": votes_of(c),
                "backers": number(c, &["total_voters", "totalVoters"]),
                // The one with the most, once it is decided.
                "winner": !open && top > 0.0 && votes_of(c) == top,
            }))
        })
        .collect();
    Some(json!({
        "kind": "poll",
        "id": format!("poll:{id}"),
        "title": text(poll, &["title"]).unwrap_or(""),
        "options": options,
        "duration": duration,
        "remaining": remaining,
        // The result stays up a while after it closes, as Twitch's own does.
        "resultDisplayDuration": 20,
        "hasVoted": false,
        "votedOptionId": null,
        "state": status.to_lowercase(),
        "open": open,
        // Twitch's polls are ended on twitch.tv by the streamer or a moderator, not from a viewer's card.
        "noEnd": true,
    }))
}

/// A prediction in the shape the card reads, from the socket's event or the GraphQL's.
pub fn prediction_card(event: &Value, now: i64) -> Option<Value> {
    let id = text(event, &["id"])?;
    let status = text(event, &["status"]).unwrap_or("ACTIVE").to_uppercase();
    let open = status == "ACTIVE";
    let window = number(event, &["prediction_window_seconds", "predictionWindowSeconds"]).unwrap_or(0.0).max(0.0) as u32;
    let started = parse_time(event, &["created_at", "createdAt"]);
    let remaining = match (open, started) {
        (true, Some(started)) => (window as i64 - (now - started)).max(0) as u32,
        (true, None) => window,
        _ => 0,
    };
    let outcomes = event.get("outcomes")?.as_array()?;
    let points_of = |o: &Value| number(o, &["total_points", "totalPoints"]).unwrap_or(0.0);
    let pot: f64 = outcomes.iter().map(points_of).sum();
    let winner = text(event, &["winning_outcome_id"]).map(str::to_string).or_else(|| event.pointer("/winningOutcome/id").and_then(|w| w.as_str()).map(str::to_string));
    let options: Vec<Value> = outcomes
        .iter()
        .filter_map(|o| {
            let oid = text(o, &["id"])?;
            let points = points_of(o);
            Some(json!({
                "id": oid,
                "label": text(o, &["title"]).unwrap_or(""),
                "votes": points,
                "backers": number(o, &["total_users", "totalUsers"]),
                // What a point put on it pays back, written the way Kick's page writes it.
                "odds": (points > 0.0 && pot > 0.0).then(|| format!("1:{:.1}", pot / points)),
                "winner": winner.as_deref() == Some(oid),
            }))
        })
        .collect();
    let state = match status.as_str() {
        "ACTIVE" => "open",
        "LOCKED" => "locked",
        "RESOLVE_PENDING" | "RESOLVED" => "resolved",
        "CANCEL_PENDING" | "CANCELED" => "canceled",
        other => return Some(json!({ "kind": "prediction", "id": format!("prediction:{id}"), "state": other.to_lowercase(), "options": options, "title": text(event, &["title"]).unwrap_or(""), "duration": window, "remaining": 0, "resultDisplayDuration": 0, "hasVoted": false, "votedOptionId": null, "total": pot, "minBet": MIN_BET, "noEnd": true })),
    };
    Some(json!({
        "kind": "prediction",
        "id": format!("prediction:{id}"),
        "title": text(event, &["title"]).unwrap_or(""),
        "options": options,
        "duration": window,
        "remaining": remaining,
        "resultDisplayDuration": 0,
        "hasVoted": false,
        "votedOptionId": null,
        "total": pot,
        "state": state,
        "minBet": MIN_BET,
        "noEnd": true,
    }))
}

/// The smallest bet Twitch takes.
pub const MIN_BET: u32 = 10;

// ---------------------------------------------------------------------------
// Saying it
// ---------------------------------------------------------------------------

/// Whether this account can answer a poll or a prediction: it has the twitch.tv session that Twitch takes votes
/// and bets from.
fn answerable(state: &AppState, account_id: &str) -> bool {
    state.accounts.get_twitch(account_id).is_some_and(|a| !a.web_token.is_empty())
}

/// Publishes a card, carrying across what the socket cannot know: this account's own vote and bet and what it has to spend.
pub fn publish(state: &AppState, account_id: &str, buffer_id: &str, kind: &str, mut card: Value) {
    card["readOnly"] = json!(!answerable(state, account_id));
    if let Some(known) = state.runtime.live_card(buffer_id, kind) {
        if known["id"] == card["id"] {
            for key in ["hasVoted", "votedOptionId", "stake", "balance"] {
                if !known[key].is_null() {
                    card[key] = known[key].clone();
                }
            }
        }
    }
    crate::backend::kick::cards::publish_card(state, buffer_id, kind, Some(card));
}

fn gone(state: &AppState, buffer_id: &str, kind: &str) {
    crate::backend::kick::cards::publish_card(state, buffer_id, kind, None);
}

/// A raid, said in the channel with a way to follow it.
fn raid_said(state: &AppState, account_id: &str, channel: &str, event: &Event) {
    match event {
        Event::Raid { login, display, viewers, .. } => {
            let embed = crate::model::Embed { kind: Some("raid-out".into()), author: Some(display.clone()), title: viewers.map(|v| v.to_string()), description: Some(login.clone()), ..Default::default() };
            state.runtime.record_message(state, account_id, channel, "channel", channel, &format!("{channel} is raiding {display}"), false, "raid", None, None, false, None, vec![embed], Vec::new(), None);
        }
        Event::RaidCalledOff { .. } => {
            state.runtime.record_message(state, account_id, channel, "channel", channel, "The raid was called off", false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
        }
        _ => {}
    }
}

/// What has to be known to act on a message: which channel a subscription was for, and what kind it was.
type Subscriptions = HashMap<String, (String, Kind)>;

/// Acts on one pubsub message.
fn act(state: &AppState, account_id: &str, channel: &str, kind: Kind, payload: &Value, raids: &mut Vec<String>) {
    let now = crate::backend::kick::cards::now_secs();
    let name = match kind {
        Kind::Raid => "raid",
        Kind::Polls => "polls",
        Kind::Predictions => "predictions",
    };
    let buffer_id = crate::model::buffer_id(account_id, channel);
    match read(name, payload, now) {
        Some(event @ Event::Raid { .. }) => {
            if let Event::Raid { id, .. } = &event {
                // The same raid is described again as its clock runs: said once.
                if raids.contains(id) {
                    return;
                }
                raids.push(id.clone());
                if raids.len() > 32 {
                    raids.remove(0);
                }
            }
            raid_said(state, account_id, channel, &event);
        }
        Some(event @ Event::RaidCalledOff { .. }) => raid_said(state, account_id, channel, &event),
        Some(Event::Card { kind, card }) => {
            let fresh = kind == "prediction" && state.runtime.live_card(&buffer_id, kind).is_none_or(|known| known["id"] != card["id"]);
            publish(state, account_id, &buffer_id, kind, card);
            // A new prediction asks what this account has to bet with, once.
            if fresh && answerable(state, account_id) {
                let (state, account_id, channel) = (state.clone(), account_id.to_string(), channel.to_string());
                tokio::spawn(async move { refresh_balance(&state, gql::ROOT, &account_id, &channel).await });
            }
        }
        Some(Event::CardGone { kind }) => gone(state, &buffer_id, kind),
        None => {}
    }
}

/// Pulls out the pubsub payload a message carries: the topic's subscription id and the JSON inside.
pub fn notification(frame: &Value) -> Option<(String, Value)> {
    let n = frame.get("notification")?;
    let sub = n.pointer("/subscription/id")?.as_str()?.to_string();
    let payload: Value = serde_json::from_str(n.get("pubsub")?.as_str()?).ok()?;
    Some((sub, payload))
}

/// Reads what this account has to bet with in a channel and puts it on the prediction card that is up, if any.
pub async fn refresh_balance(state: &AppState, root: &str, account_id: &str, channel: &str) {
    let Some(cfg) = state.accounts.get_twitch(account_id) else { return };
    let Ok(http) = super::client() else { return };
    let Ok(points) = super::points::context(state, &http, root, channel, Some(&cfg.web_token)).await else { return };
    let (buffer_id, Some(balance)) = (crate::model::buffer_id(account_id, channel), points.balance) else { return };
    if let Some(mut card) = state.runtime.live_card(&buffer_id, "prediction") {
        card["balance"] = json!(balance);
        // Directly: `publish` would carry the old balance back over the new one.
        crate::backend::kick::cards::publish_card(state, &buffer_id, "prediction", Some(card));
    }
    state.events.emit("twitchPoints", json!({ "bufferId": buffer_id, "points": balance, "name": points.name }));
}

/// A poll or prediction the channel already has going, asked for from the GraphQL when it is opened.
pub async fn current(state: &AppState, account_id: &str, channel: &str, gql_root: &str) {
    let Ok(http) = super::client() else { return };
    const ASK: &str = "query($login:String!){ user(login:$login){ viewablePoll{ id title status durationSeconds remainingDurationMilliseconds choices{ id title votes{ total } totalVoters } } channel{ activePredictionEvents{ id title status createdAt predictionWindowSeconds winningOutcome{ id } outcomes{ id title totalPoints totalUsers } } } } }";
    let answer = match gql::call(&http, gql_root, &gql::query(ASK, json!({ "login": channel })), &gql::Who::default()).await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!("twitch[{account_id}]: {channel}'s poll and prediction: {e:#}");
            return;
        }
    };
    let now = crate::backend::kick::cards::now_secs();
    let buffer_id = crate::model::buffer_id(account_id, channel);
    if let Some(card) = answer.pointer("/data/user/viewablePoll").filter(|p| !p.is_null()).and_then(|p| poll_card(p, now)) {
        publish(state, account_id, &buffer_id, "poll", card);
    }
    if let Some(card) = answer.pointer("/data/user/channel/activePredictionEvents/0").filter(|p| !p.is_null()).and_then(|p| prediction_card(p, now)) {
        publish(state, account_id, &buffer_id, "prediction", card);
        if answerable(state, account_id) {
            refresh_balance(state, gql_root, account_id, channel).await;
        }
    }
}

// ---------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------

/// Listens for as long as the account runs, reconnecting with a growing wait.
pub async fn run(state: AppState, account_id: String, mut rx: mpsc::UnboundedReceiver<Watch>, url: String, gql_root: String) {
    let mut watched: HashMap<String, String> = HashMap::new();
    let mut wait = 3u64;
    loop {
        match session(&state, &account_id, &mut rx, &mut watched, &url, &gql_root).await {
            Ok(()) => return,
            Err(e) => {
                tracing::debug!("twitch[{account_id}]: realtime ended: {e:#}");
                // Anything asked for while it was down is kept in `watched` by the drain below.
                while let Ok(w) = rx.try_recv() {
                    apply(&mut watched, w);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
        wait = (wait * 2).min(120);
    }
}

fn apply(watched: &mut HashMap<String, String>, w: Watch) {
    match w {
        Watch::Add { channel, room_id } => {
            watched.insert(channel, room_id);
        }
        Watch::Remove { channel } => {
            watched.remove(&channel);
        }
    }
}

fn subscribe_frame(id: &str, topic: &str) -> String {
    json!({
        "type": "subscribe",
        "id": id,
        "subscribe": { "id": id, "type": "pubsub", "pubsub": { "topic": topic } },
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
    .to_string()
}

async fn session(state: &AppState, account_id: &str, rx: &mut mpsc::UnboundedReceiver<Watch>, watched: &mut HashMap<String, String>, url: &str, gql_root: &str) -> Result<()> {
    let (mut socket, _) = crate::net::route::websocket(super::SHARED_KEY, url).await.context("connecting to Twitch's realtime service")?;
    let mut subs: Subscriptions = HashMap::new();
    let mut next = 0u64;
    let mut raids: Vec<String> = Vec::new();
    // Nothing to listen to is nothing to hold open: the socket waits for the first channel.
    let mut listening: Vec<String> = Vec::new();
    let mut last_heard = tokio::time::Instant::now();
    let mut check = tokio::time::interval(Duration::from_secs(15));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    macro_rules! listen {
        ($channel:expr, $room:expr) => {{
            let (channel, room): (String, String) = ($channel, $room);
            if !room.is_empty() && !listening.contains(&channel) {
                listening.push(channel.clone());
                for kind in Kind::ALL {
                    let id = format!("s{next}");
                    next += 1;
                    socket.send(WsMessage::Text(subscribe_frame(&id, &kind.topic(&room)))).await?;
                    subs.insert(id, (channel.clone(), kind));
                }
                let (state, account_id, gql_root) = (state.clone(), account_id.to_string(), gql_root.to_string());
                tokio::spawn(async move { current(&state, &account_id, &channel, &gql_root).await });
            }
        }};
    }

    for (channel, room) in watched.clone() {
        listen!(channel, room);
    }

    loop {
        tokio::select! {
            frame = socket.next() => {
                let Some(frame) = frame else { anyhow::bail!("the connection closed") };
                match frame.context("reading from Twitch's realtime service")? {
                    WsMessage::Text(text) => {
                        last_heard = tokio::time::Instant::now();
                        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                        if value.get("type").and_then(|t| t.as_str()) != Some("notification") {
                            continue;
                        }
                        let Some((sub, payload)) = notification(&value) else { continue };
                        let Some((channel, kind)) = subs.get(&sub).cloned() else { continue };
                        // A channel closed since is no longer listened for.
                        if !watched.contains_key(&channel) {
                            continue;
                        }
                        act(state, account_id, &channel, kind, &payload, &mut raids);
                    }
                    WsMessage::Ping(data) => socket.send(WsMessage::Pong(data)).await?,
                    WsMessage::Close(_) => anyhow::bail!("Twitch's realtime service closed the connection"),
                    _ => {}
                }
            }
            command = rx.recv() => {
                let Some(command) = command else { return Ok(()) };
                match command {
                    Watch::Add { channel, room_id } => {
                        watched.insert(channel.clone(), room_id.clone());
                        listen!(channel, room_id);
                    }
                    Watch::Remove { channel } => {
                        watched.remove(&channel);
                        listening.retain(|c| *c != channel);
                    }
                }
            }
            _ = check.tick() => {
                if !listening.is_empty() && last_heard.elapsed() > SILENCE {
                    anyhow::bail!("nothing heard from Twitch's realtime service for {} seconds", SILENCE.as_secs());
                }
                if listening.is_empty() {
                    last_heard = tokio::time::Instant::now();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prediction(status: &str) -> Value {
        json!({ "type": "event-updated", "data": { "event": {
            "id": "p1", "title": "Map 2", "status": status, "created_at": "2026-10-14T13:00:00Z", "prediction_window_seconds": 600,
            "winning_outcome_id": if status == "RESOLVED" { json!("b") } else { Value::Null },
            "outcomes": [{ "id": "a", "title": "Falcons", "total_points": 750, "total_users": 7 }, { "id": "b", "title": "NIP", "total_points": 250, "total_users": 3 }]
        } } })
    }

    #[test]
    fn a_prediction_counts_down_from_when_it_began_and_pays_by_its_pot() {
        let began = chrono::DateTime::parse_from_rfc3339("2026-10-14T13:00:00Z").unwrap().timestamp();
        let Some(Event::Card { kind, card }) = read("predictions", &prediction("ACTIVE"), began + 100) else { panic!("a card") };
        assert_eq!(kind, "prediction");
        assert_eq!(card["id"], "prediction:p1");
        assert_eq!(card["remaining"], 500);
        assert_eq!(card["state"], "open");
        assert_eq!(card["total"], 1000.0);
        assert_eq!(card["options"][0]["odds"], "1:1.3");
        assert_eq!(card["options"][1]["odds"], "1:4.0");
        assert_eq!(card["options"][1]["backers"], 3.0);
    }

    #[test]
    fn a_locked_or_decided_prediction_has_no_time_left_and_names_its_winner() {
        let began = chrono::DateTime::parse_from_rfc3339("2026-10-14T13:00:00Z").unwrap().timestamp();
        let Some(Event::Card { card, .. }) = read("predictions", &prediction("LOCKED"), began + 100) else { panic!() };
        assert_eq!((card["remaining"].as_u64(), card["state"].as_str()), (Some(0), Some("locked")));
        let Some(Event::Card { card, .. }) = read("predictions", &prediction("RESOLVED"), began + 700) else { panic!() };
        assert_eq!(card["state"], "resolved");
        assert_eq!((card["options"][0]["winner"].as_bool(), card["options"][1]["winner"].as_bool()), (Some(false), Some(true)));
    }

    #[test]
    fn a_called_off_prediction_is_taken_down() {
        assert_eq!(read("predictions", &prediction("CANCELED"), 0), Some(Event::CardGone { kind: "prediction" }));
    }

    #[test]
    fn a_poll_is_read_in_the_sockets_shape_and_in_the_graphqls() {
        let socket = json!({ "type": "POLL_UPDATE", "data": { "poll": { "poll_id": "q1", "title": "Next?", "status": "ACTIVE", "duration_seconds": 60, "remaining_duration_milliseconds": 41_500,
            "choices": [{ "choice_id": "x", "title": "Chess", "votes": { "total": 3 }, "total_voters": 3 }, { "choice_id": "y", "title": "Go", "votes": { "total": 1 }, "total_voters": 1 }] } } });
        let Some(Event::Card { kind, card }) = read("polls", &socket, 0) else { panic!() };
        assert_eq!((kind, card["id"].as_str(), card["remaining"].as_u64(), card["open"].as_bool()), ("poll", Some("poll:q1"), Some(42), Some(true)));
        assert_eq!(card["options"][0], json!({ "id": "x", "label": "Chess", "votes": 3.0, "backers": 3.0, "winner": false }));

        let graphql = json!({ "id": "q1", "title": "Next?", "status": "ACTIVE", "durationSeconds": 60, "remainingDurationMilliseconds": 10_000, "choices": [{ "id": "x", "title": "Chess", "votes": { "total": 3 }, "totalVoters": 3 }] });
        assert_eq!(poll_card(&graphql, 0).unwrap()["remaining"], 10);
    }

    #[test]
    fn a_finished_poll_shows_how_it_went_and_an_archived_one_goes() {
        let done = json!({ "type": "POLL_COMPLETE", "data": { "poll": { "poll_id": "q1", "title": "Next?", "status": "COMPLETED", "duration_seconds": 60,
            "choices": [{ "choice_id": "x", "title": "Chess", "votes": { "total": 3 } }, { "choice_id": "y", "title": "Go", "votes": { "total": 1 } }] } } });
        let Some(Event::Card { card, .. }) = read("polls", &done, 0) else { panic!() };
        assert_eq!((card["open"].as_bool(), card["remaining"].as_u64()), (Some(false), Some(0)));
        assert_eq!((card["options"][0]["winner"].as_bool(), card["options"][1]["winner"].as_bool()), (Some(true), Some(false)));
        let archived = json!({ "type": "POLL_ARCHIVE", "data": { "poll": { "poll_id": "q1" } } });
        assert_eq!(read("polls", &archived, 0), Some(Event::CardGone { kind: "poll" }));
    }

    #[test]
    fn a_raid_names_where_it_is_going_and_can_be_called_off() {
        let raid = json!({ "type": "raid_update_v2", "raid": { "id": "r1", "target_login": "friend", "target_display_name": "Friend", "viewer_count": 120 } });
        assert_eq!(read("raid", &raid, 0), Some(Event::Raid { id: "r1".into(), login: "friend".into(), display: "Friend".into(), viewers: Some(120) }));
        let off = json!({ "type": "raid_cancel_v2", "raid": { "id": "r1" } });
        assert_eq!(read("raid", &off, 0), Some(Event::RaidCalledOff { id: "r1".into() }));
        assert_eq!(read("raid", &json!({ "type": "raid_go_v2", "raid": { "id": "r1" } }), 0), None);
    }

    #[test]
    fn the_envelope_gives_up_the_payload_inside_it() {
        let frame = json!({ "type": "notification", "notification": { "subscription": { "id": "s3" }, "type": "pubsub", "pubsub": prediction("ACTIVE").to_string() } });
        let (sub, payload) = notification(&frame).unwrap();
        assert_eq!(sub, "s3");
        assert_eq!(payload["data"]["event"]["id"], "p1");
        assert!(notification(&json!({ "type": "keepalive" })).is_none());
    }

    use crate::backend::kick::testkit;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message as Ws;

    /// A Hermes that, once a client has subscribed to its three topics, announces a prediction, a raid (twice) and a poll.
    async fn fake_hermes(subscribed: Arc<Mutex<Vec<String>>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = ws.send(Ws::Text(json!({ "type": "welcome", "welcome": { "keepaliveSec": 15 } }).to_string())).await;
            let mut ids: Vec<(String, String)> = Vec::new();
            while let Some(Ok(Ws::Text(text))) = ws.next().await {
                let v: Value = serde_json::from_str(&text).unwrap();
                if v["type"] != "subscribe" {
                    continue;
                }
                let (id, topic) = (v["subscribe"]["id"].as_str().unwrap().to_string(), v["subscribe"]["pubsub"]["topic"].as_str().unwrap().to_string());
                subscribed.lock().unwrap().push(topic.clone());
                let _ = ws.send(Ws::Text(json!({ "type": "subscribeResponse", "subscribeResponse": { "subscription": { "id": id }, "result": "ok" } }).to_string())).await;
                ids.push((id, topic));
                if ids.len() == 3 {
                    let say = |topic: &str, payload: Value| {
                        let id = ids.iter().find(|(_, t)| t.starts_with(topic)).unwrap().0.clone();
                        Ws::Text(json!({ "type": "notification", "notification": { "subscription": { "id": id }, "type": "pubsub", "pubsub": payload.to_string() } }).to_string())
                    };
                    let began = chrono::Utc::now().to_rfc3339();
                    let _ = ws.send(say("predictions-channel-v1", json!({ "type": "event-created", "data": { "event": { "id": "p1", "title": "Map 2", "status": "ACTIVE", "created_at": began, "prediction_window_seconds": 600,
                        "outcomes": [{ "id": "a", "title": "Falcons", "total_points": 0, "total_users": 0 }, { "id": "b", "title": "NIP", "total_points": 0, "total_users": 0 }] } } }))).await;
                    let raid = json!({ "type": "raid_update_v2", "raid": { "id": "r1", "target_login": "friend", "target_display_name": "Friend", "viewer_count": 120 } });
                    let _ = ws.send(say("raid", raid.clone())).await;
                    // Described again as its clock runs: said once.
                    let _ = ws.send(say("raid", raid)).await;
                    let _ = ws.send(say("polls", json!({ "type": "POLL_CREATE", "data": { "poll": { "poll_id": "q1", "title": "Next?", "status": "ACTIVE", "duration_seconds": 60, "choices": [{ "choice_id": "x", "title": "Chess", "votes": { "total": 0 } }] } } }))).await;
                }
            }
        });
        url
    }

    #[tokio::test]
    async fn a_channels_prediction_poll_and_raid_are_heard_from_the_realtime_service_and_shown() {
        let state = testkit::simulated_daemon("twitch-realtime");
        let account = "twitch:~anonymous";
        let buffer = crate::model::buffer_id(account, "xqc");
        state.runtime.ensure_buffer(&state, account, "xqc", "channel");
        let subscribed = Arc::new(Mutex::new(Vec::new()));
        let url = fake_hermes(subscribed.clone()).await;
        let rx = register("twitch:realtime-test");
        // Told about the channel before the socket is up, as a join is.
        watch("twitch:realtime-test", "xqc", "100");
        let task = tokio::spawn(run(state.clone(), account.to_string(), rx, url, "http://127.0.0.1:9".to_string()));

        for _ in 0..100 {
            if state.runtime.live_card(&buffer, "prediction").is_some() && state.runtime.live_card(&buffer, "poll").is_some() && !state.store.get_backlog(&buffer, 0, 10).unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let mut topics = subscribed.lock().unwrap().clone();
        topics.sort();
        assert_eq!(topics, vec!["polls.100", "predictions-channel-v1.100", "raid.100"]);
        let prediction = state.runtime.live_card(&buffer, "prediction").expect("the prediction is on screen");
        assert_eq!((prediction["title"].as_str(), prediction["state"].as_str()), (Some("Map 2"), Some("open")));
        assert!(prediction["remaining"].as_u64().unwrap() > 590);
        assert_eq!(state.runtime.live_card(&buffer, "poll").unwrap()["options"][0]["label"], "Chess");
        let log = state.store.get_backlog(&buffer, 0, 10).unwrap();
        let raids: Vec<_> = log.iter().filter(|m| m.body.contains("raiding")).collect();
        assert_eq!(raids.len(), 1, "a raid is said once: {log:?}");
        assert_eq!(raids[0].embeds[0].kind.as_deref(), Some("raid-out"));
        assert_eq!(raids[0].embeds[0].description.as_deref(), Some("friend"));
        task.abort();
    }

    /// Against the real services: `TWITCH_LIVE_CHANNEL=<login> cargo test --release live_ -- --ignored --nocapture`.
    /// Opens the channel the way a join does and prints what Twitch says about its poll and prediction.
    #[tokio::test]
    #[ignore]
    async fn live_a_real_channels_prediction_is_read_from_the_graphql_and_the_socket() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let channel = std::env::var("TWITCH_LIVE_CHANNEL").expect("TWITCH_LIVE_CHANNEL=<login>");
        let state = testkit::simulated_daemon("twitch-realtime-live");
        let account = "twitch:~anonymous";
        let buffer = crate::model::buffer_id(account, &channel);
        state.runtime.ensure_buffer(&state, account, &channel, "channel");
        let http = crate::backend::twitch::client().unwrap();
        let id = crate::backend::twitch::stream::channels(&http, gql::ROOT, std::slice::from_ref(&channel)).await.unwrap()[0].clone().unwrap().id;
        let rx = register("twitch:live");
        watch("twitch:live", &channel, &id);
        let task = tokio::spawn(run(state.clone(), account.to_string(), rx, HERMES_URL.to_string(), gql::ROOT.to_string()));
        tokio::time::sleep(Duration::from_secs(20)).await;
        println!("prediction: {:?}", state.runtime.live_card(&buffer, "prediction"));
        println!("poll: {:?}", state.runtime.live_card(&buffer, "poll"));
        task.abort();
    }
}
