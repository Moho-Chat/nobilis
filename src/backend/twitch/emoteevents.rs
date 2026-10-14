//! A channel's 7TV and BetterTTV emotes kept current while it is open.
//!
//! Both services announce changes over a websocket of their own, which is how
//! their browser extensions notice an emote added a minute ago. Without this a
//! channel's tables are as they were when it was joined and a new emote is
//! plain text until it is rejoined. With it, they are updated as announced
//! (see `emotes::apply`). FrankerFaceZ has nothing of the kind to listen to.
//!
//! Like the fetches in `emotes`, each socket is the service being told which
//! channel is open, so each is used only while its provider's switch is on
//! (Settings → Twitch), and neither is opened until a channel needs it.

use super::emotes::{self, Change, Provider};
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub const SEVENTV_URL: &str = "wss://events.7tv.io/v3";
pub const BTTV_URL: &str = "wss://sockets.betterttv.net/ws";

#[derive(Clone)]
pub enum Watch {
    Add { channel: String, room_id: String },
    Remove { channel: String },
}

fn registry() -> &'static Mutex<HashMap<String, broadcast::Sender<Watch>>> {
    static REGISTRY: std::sync::OnceLock<Mutex<HashMap<String, broadcast::Sender<Watch>>>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Makes the way to tell an account's sockets what to listen to; the receiver is `run`'s.
pub fn register(account_id: &str) -> broadcast::Receiver<Watch> {
    let (tx, rx) = broadcast::channel(64);
    registry().lock().unwrap().insert(account_id.to_string(), tx);
    rx
}

/// Listens for a channel's emote changes, once its tables are fetched (the 7TV set it uses is learned there).
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

/// Runs both sockets for as long as the account does.
pub async fn run(rx: broadcast::Receiver<Watch>, seventv_url: String, bttv_url: String) {
    let (a, b) = (rx.resubscribe(), rx);
    tokio::join!(keep_going(Provider::SevenTv, a, seventv_url), keep_going(Provider::Bttv, b, bttv_url));
}

async fn keep_going(provider: Provider, mut rx: broadcast::Receiver<Watch>, url: String) {
    // Channel -> room id, what is wanted whether or not the socket is up.
    let mut wanted: HashMap<String, String> = HashMap::new();
    let mut wait = 3u64;
    loop {
        // Nothing is opened for nobody: wait for the first channel.
        while wanted.is_empty() || !emotes::enabled(provider) {
            match rx.recv().await {
                Ok(w) => apply(&mut wanted, w),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
        match session(provider, &mut rx, &mut wanted, &url).await {
            // Nothing left to listen for (or the provider switched off): back to waiting for a channel.
            Ok(()) => continue,
            Err(e) => tracing::debug!("twitch: {} live changes ended: {e:#}", provider.name()),
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
        wait = (wait * 2).min(120);
    }
}

fn apply(wanted: &mut HashMap<String, String>, w: Watch) {
    match w {
        Watch::Add { channel, room_id } => {
            wanted.insert(channel, room_id);
        }
        Watch::Remove { channel } => {
            wanted.remove(&channel);
        }
    }
}

/// 7TV's subscription for a set, and BetterTTV's for a channel.
fn seventv_subscribe(set: &str) -> String {
    json!({ "op": 35, "d": { "type": "emote_set.update", "condition": { "object_id": set } } }).to_string()
}

fn bttv_join(room_id: &str) -> String {
    json!({ "name": "join_channel", "data": { "name": format!("twitch:{room_id}") } }).to_string()
}

/// The channel (by login) a 7TV set belongs to, among those wanted.
fn channel_of_set(wanted: &HashMap<String, String>, set: &str) -> Option<String> {
    wanted.keys().find(|c| emotes::seventv_set(c).as_deref() == Some(set)).cloned()
}

/// What a 7TV message is: a dispatch with changes for a set, or something else.
pub fn seventv_dispatch(frame: &Value) -> Option<(String, Vec<Change>)> {
    if frame.get("op")?.as_u64()? != 0 {
        return None;
    }
    let d = frame.get("d")?;
    if d.get("type")?.as_str()? != "emote_set.update" {
        return None;
    }
    let body = d.get("body")?;
    Some((body.get("id")?.as_str()?.to_string(), emotes::read_seventv_changes(body)))
}

/// The channel's login, by BetterTTV's room name ("twitch:<id>").
fn bttv_channel(wanted: &HashMap<String, String>, event: &Value) -> Option<String> {
    let room = event.pointer("/data/channel")?.as_str()?.strip_prefix("twitch:")?;
    wanted.iter().find(|(_, id)| id.as_str() == room).map(|(c, _)| c.clone())
}

async fn session(provider: Provider, rx: &mut broadcast::Receiver<Watch>, wanted: &mut HashMap<String, String>, url: &str) -> Result<()> {
    let (mut socket, _) = crate::net::route::websocket(super::SHARED_KEY, url).await.with_context(|| format!("connecting to {}'s live changes", provider.name()))?;
    let mut sent: Vec<String> = Vec::new();
    let mut hello = provider == Provider::Bttv;
    let mut last_heard = tokio::time::Instant::now();
    let mut check = tokio::time::interval(Duration::from_secs(30));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // 7TV speaks first and is spoken to only after it has; BetterTTV is spoken to at once.
        if hello {
            for (channel, room) in wanted.clone() {
                if sent.contains(&channel) || !emotes::enabled(provider) {
                    continue;
                }
                let frame = match provider {
                    Provider::Bttv => Some(bttv_join(&room)),
                    // The set is known once the channel's tables are in; until then it is tried again.
                    _ => emotes::seventv_set(&channel).map(|set| seventv_subscribe(&set)),
                };
                if let Some(frame) = frame {
                    socket.send(WsMessage::Text(frame)).await?;
                    sent.push(channel);
                }
            }
        }
        tokio::select! {
            frame = socket.next() => {
                let Some(frame) = frame else { anyhow::bail!("the connection closed") };
                match frame.context("reading live emote changes")? {
                    WsMessage::Text(text) => {
                        last_heard = tokio::time::Instant::now();
                        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                        match provider {
                            Provider::SevenTv => {
                                if value.get("op").and_then(|o| o.as_u64()) == Some(1) {
                                    hello = true;
                                } else if let Some((set, changes)) = seventv_dispatch(&value) {
                                    if let Some(channel) = channel_of_set(wanted, &set) {
                                        emotes::apply(Provider::SevenTv, &channel, &changes);
                                    }
                                }
                            }
                            _ => {
                                if let (Some(channel), Some(change)) = (bttv_channel(wanted, &value), emotes::read_bttv_change(&value)) {
                                    emotes::apply(Provider::Bttv, &channel, &[change]);
                                }
                            }
                        }
                    }
                    WsMessage::Ping(data) => socket.send(WsMessage::Pong(data)).await?,
                    WsMessage::Pong(_) => last_heard = tokio::time::Instant::now(),
                    WsMessage::Close(_) => anyhow::bail!("{} closed the connection", provider.name()),
                    _ => {}
                }
            }
            command = rx.recv() => {
                match command {
                    Ok(w) => {
                        if let Watch::Remove { channel } = &w {
                            sent.retain(|c| c != channel);
                        }
                        apply(wanted, w);
                        if wanted.is_empty() {
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            _ = check.tick() => {
                // 7TV's heartbeat comes about every 45 seconds, BetterTTV says nothing: a ping tells a dead socket from a quiet one.
                if provider == Provider::Bttv {
                    socket.send(WsMessage::Ping(Vec::new())).await?;
                }
                if last_heard.elapsed() > Duration::from_secs(150) {
                    anyhow::bail!("nothing heard from {} for {} seconds", provider.name(), last_heard.elapsed().as_secs());
                }
                if !emotes::enabled(provider) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::twitch::emotes::{Emote, Table};

    fn table(names: &[(&str, &str)]) -> Table {
        let mut t = Table::default();
        for (id, name) in names {
            t.add_for_test(Emote { provider: Provider::SevenTv, id: id.to_string(), name: name.to_string(), animated: false });
        }
        t
    }

    #[test]
    fn a_7tv_dispatch_says_what_was_added_removed_and_renamed() {
        let frame = json!({ "op": 0, "d": { "type": "emote_set.update", "body": { "id": "SET",
            "pushed": [{ "key": "emotes", "value": { "id": "E1", "name": "Wow", "data": { "animated": true } } }],
            "pulled": [{ "key": "emotes", "old_value": { "id": "E2", "name": "Gone" } }],
            "updated": [{ "key": "emotes", "old_value": { "id": "E3", "name": "Old" }, "value": { "id": "E3", "name": "New" } }] } } });
        let (set, changes) = seventv_dispatch(&frame).unwrap();
        assert_eq!(set, "SET");
        assert_eq!(
            changes,
            vec![
                Change::RemoveName("Gone".into()),
                Change::RemoveName("Old".into()),
                Change::Add(Emote { provider: Provider::SevenTv, id: "E3".into(), name: "New".into(), animated: false }),
                Change::Add(Emote { provider: Provider::SevenTv, id: "E1".into(), name: "Wow".into(), animated: true }),
            ]
        );
        assert!(seventv_dispatch(&json!({ "op": 2, "d": {} })).is_none(), "a heartbeat is not a change");
        assert!(seventv_dispatch(&json!({ "op": 0, "d": { "type": "user.update", "body": { "id": "x" } } })).is_none());
    }

    #[test]
    fn a_bttv_event_is_an_add_or_a_removal_by_id() {
        let create = json!({ "name": "emote_create", "data": { "channel": "twitch:71", "emote": { "id": "b1", "code": "Pog", "imageType": "gif" } } });
        assert_eq!(emotes::read_bttv_change(&create), Some(Change::Add(Emote { provider: Provider::Bttv, id: "b1".into(), name: "Pog".into(), animated: true })));
        let delete = json!({ "name": "emote_delete", "data": { "channel": "twitch:71", "emoteId": "b1" } });
        assert_eq!(emotes::read_bttv_change(&delete), Some(Change::RemoveId("b1".into())));
        assert_eq!(emotes::read_bttv_change(&json!({ "name": "lookup_user", "data": {} })), None);
        let wanted = HashMap::from([("xqc".to_string(), "71".to_string())]);
        assert_eq!(bttv_channel(&wanted, &create).as_deref(), Some("xqc"));
        assert_eq!(bttv_channel(&wanted, &json!({ "name": "emote_create", "data": { "channel": "twitch:99" } })), None);
    }

    #[test]
    fn changes_reach_the_held_table_and_leave_a_closed_channel_alone() {
        emotes::keep(Provider::SevenTv, Some("live-test"), table(&[("E2", "Gone"), ("E3", "Old")]));
        emotes::apply(Provider::SevenTv, "live-test", &[Change::RemoveName("Gone".into()), Change::Add(Emote { provider: Provider::SevenTv, id: "E1".into(), name: "Wow".into(), animated: false })]);
        assert!(emotes::find_in("live-test", "Gone").is_none());
        assert_eq!(emotes::find_in("live-test", "Wow").map(|e| e.id), Some("E1".to_string()));
        assert!(emotes::find_in("live-test", "Old").is_some(), "what was not mentioned stays");
        emotes::forget("live-test");
        emotes::apply(Provider::SevenTv, "live-test", &[Change::Add(Emote { provider: Provider::SevenTv, id: "E9".into(), name: "Late".into(), animated: false })]);
        assert!(emotes::find_in("live-test", "Late").is_none(), "a closed channel is not brought back by a late change");
    }
}
