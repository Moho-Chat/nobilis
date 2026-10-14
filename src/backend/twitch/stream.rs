//! What is on air in a channel: whether it is live, what it is called, how many
//! are watching, and the address of its video.
//!
//! The answer comes from Twitch's GraphQL (see `gql.rs`) because it can be
//! asked anonymously and in one request for many channels at once, where the
//! official API wants a signed-in account and one call per kind of thing. It is
//! said to the window as the same `kickStream` event Kick's streams use, with
//! the same fields, so the header's live chip, the Watch button and the player
//! are the code Kick already has - the event is named for the service that had
//! it first and carries any service's stream.

use super::gql;
use super::playback::{self, Playback, PlaybackError};
use crate::state::AppState;
use anyhow::{Context, Result};
use serde_json::{json, Value};

/// A channel as Twitch describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    /// Twitch's number for it, which the official API and a shared chat are keyed by.
    pub id: String,
    pub login: String,
    pub display_name: String,
    /// The streamer's own picture.
    pub avatar: Option<String>,
    pub followers: Option<u64>,
    pub live: Option<Live>,
}

/// A broadcast in progress.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    /// The broadcast's own id: a different one is a different stream, even in the same channel.
    pub id: u64,
    pub title: String,
    pub category: Option<String>,
    pub viewers: Option<u64>,
    pub started_ts: Option<i64>,
}

const USERS: &str = "query($logins:[String!]!){ users(logins:$logins){ id login displayName profileImageURL(width:300) followers{ totalCount } broadcastSettings{ title game{ displayName } } stream{ id title viewersCount createdAt type game{ displayName } } } }";

/// How many channels one question asks about. The poll covers every channel an
/// account has open, and a list longer than this is asked in pieces.
const BATCH: usize = 30;

fn read_channel(v: &Value) -> Option<Channel> {
    let login = v.get("login")?.as_str()?.to_lowercase();
    let live = v.get("stream").filter(|s| !s.is_null()).map(|s| Live {
        id: s.get("id").and_then(|i| i.as_str()).and_then(|i| i.parse().ok()).unwrap_or(0),
        title: s.get("title").and_then(|t| t.as_str()).filter(|t| !t.is_empty()).or_else(|| v.pointer("/broadcastSettings/title").and_then(|t| t.as_str())).unwrap_or("").to_string(),
        category: s.pointer("/game/displayName").and_then(|g| g.as_str()).map(str::to_string),
        viewers: s.get("viewersCount").and_then(|n| n.as_u64()),
        started_ts: s.get("createdAt").and_then(|d| d.as_str()).and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok()).map(|d| d.timestamp()),
    });
    Some(Channel {
        id: v.get("id")?.as_str()?.to_string(),
        display_name: v.get("displayName").and_then(|d| d.as_str()).filter(|d| !d.is_empty()).unwrap_or(&login).to_string(),
        avatar: v.get("profileImageURL").and_then(|a| a.as_str()).map(str::to_string),
        followers: v.pointer("/followers/totalCount").and_then(|n| n.as_u64()),
        login,
        live,
    })
}

/// Asks about several channels at once. A login Twitch has no channel for comes
/// back as `None` in its place, so the answers line up with the question.
pub async fn channels(http: &reqwest::Client, root: &str, logins: &[String]) -> Result<Vec<Option<Channel>>> {
    let mut out = Vec::with_capacity(logins.len());
    for chunk in logins.chunks(BATCH) {
        let answer = gql::call(http, root, &gql::query(USERS, json!({ "logins": chunk })), &gql::Who::default()).await?;
        if let Some(why) = gql::error_of(&answer) {
            anyhow::bail!("Twitch would not say what is on air: {why}");
        }
        let users = answer.pointer("/data/users").and_then(|u| u.as_array()).context("Twitch's answer about channels had no list of them")?;
        for (i, login) in chunk.iter().enumerate() {
            out.push(users.get(i).filter(|u| !u.is_null()).and_then(read_channel).filter(|c| c.login == login.to_lowercase()));
        }
    }
    Ok(out)
}


/// What a stream event says, built from what is known. Public for the tests.
pub fn stream_event(buffer_id: &str, channel: &Channel, playback: Option<&Playback>, why_not: Option<&str>) -> Value {
    json!({
        "bufferId": buffer_id,
        "service": "twitch",
        "live": channel.live.is_some(),
        "title": channel.live.as_ref().map(|l| l.title.clone()),
        "category": channel.live.as_ref().and_then(|l| l.category.clone()),
        "livestreamId": channel.live.as_ref().map(|l| l.id).filter(|id| *id != 0),
        "viewers": channel.live.as_ref().and_then(|l| l.viewers),
        "startedTs": channel.live.as_ref().and_then(|l| l.started_ts),
        "followers": channel.followers,
        "playbackUrl": playback.map(|p| p.url.clone()),
        "playbackExpires": playback.map(|p| p.expires),
        // Whether this account's own token says ads are not for it, which is
        // the one thing a client does not choose: a subscriber's or Turbo's.
        "adFree": playback.map(|p| p.hide_ads),
        // Why there is no picture, in Twitch's words or ours, for a stream that
        // is live: sub-only, blocked in this country, and so on.
        "playbackError": why_not,
    })
}


/// How long before an address stops being accepted a new one is made: the
/// token is good for twenty minutes, and the player can be several minutes
/// into a playlist that carries it.
pub const RENEW_BEFORE: i64 = 300;

/// The address for a live channel: the one already held while it is good for
/// a while yet, else a new one.
async fn playback_for(state: &AppState, account_id: &str, buffer_id: &str, http: &reqwest::Client, login: &str) -> (Option<Playback>, Option<String>) {
    let now = chrono::Utc::now().timestamp();
    if let Some(have) = state.runtime.kick_stream(buffer_id) {
        if let (Some(url), Some(expires)) = (have["playbackUrl"].as_str(), have["playbackExpires"].as_i64()) {
            if expires - now > RENEW_BEFORE {
                return (Some(Playback { url: url.to_string(), expires, hide_ads: have["adFree"].as_bool().unwrap_or(false), restricted: Vec::new() }), None);
            }
        }
    }
    let web = state.accounts.get_twitch(account_id).map(|a| a.web_token).filter(|t| !t.is_empty());
    match playback::playback(state, http, gql::ROOT, playback::USHER_ROOT, login, web.as_deref()).await {
        Ok(p) => (Some(p), None),
        Err(PlaybackError::Offline) => (None, None),
        Err(e) => {
            tracing::debug!("twitch: the address for {login}: {e}");
            (None, Some(e.to_string()))
        }
    }
}

/// Reads a batch of channels' state into the stream events, saying going live
/// and going off the air in the channel when that is what changed.
///
/// Each live channel is made sure to have a good address too. That costs a
/// call only for a channel whose address is not yet held or is running out:
/// one that is good for a while yet is kept as it is.
pub async fn refresh(state: &AppState, account_id: &str, logins: &[String]) {
    let Ok(http) = super::client() else { return };
    let rows = match channels(&http, gql::ROOT, logins).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("twitch[{account_id}]: asking what is on air: {e:#}");
            return;
        }
    };
    for (login, row) in logins.iter().zip(rows) {
        let buffer_id = crate::model::buffer_id(account_id, login);
        let Some(channel) = row else { continue };
        let before = state.runtime.kick_stream(&buffer_id);
        let was_live = before.as_ref().map(|s| s["live"].as_bool().unwrap_or(false));
        let (playback, why_not) = if channel.live.is_some() { playback_for(state, account_id, &buffer_id, &http, login).await } else { (None, None) };
        let next = stream_event(&buffer_id, &channel, playback.as_ref(), why_not.as_deref());
        if before.as_ref() != Some(&next) {
            state.runtime.set_kick_stream(&buffer_id, next.clone());
            state.events.emit("kickStream", next);
        }
        if let Some(avatar) = channel.avatar.as_deref() {
            state.runtime.set_buffer_avatar(state, &buffer_id, avatar);
        }
        // Said in the channel when it changes, never on first sight of a
        // channel (which would announce every channel as it is joined).
        if let (Some(was), now) = (was_live, channel.live.is_some()) {
            if was != now {
                say_live_change(state, account_id, &channel, now);
            }
        }
    }
}

fn say_live_change(state: &AppState, account_id: &str, channel: &Channel, live: bool) {
    let what = if live { "went live" } else { "ended the stream" };
    state.runtime.record_message(
        state,
        account_id,
        &channel.login,
        "channel",
        &channel.login,
        what,
        false,
        "stream",
        None,
        None,
        false,
        None,
        vec![crate::model::Embed {
            kind: Some("stream".into()),
            author: Some(channel.display_name.clone()),
            title: Some(what.to_string()),
            icon: Some(if live { "live" } else { "offline" }.to_string()),
            ..Default::default()
        }],
        Vec::new(),
        None,
    );
}

/// One channel, now: used when it has just been joined and when the window
/// asks on its own minute for the one on screen.
pub async fn refresh_one(state: &AppState, buffer_id: &str) {
    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return };
    if !buffer.account_id.starts_with("twitch:") {
        return;
    }
    refresh(state, &buffer.account_id, std::slice::from_ref(&buffer.name)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer() -> Value {
        json!({ "data": { "users": [
            { "id": "71092938", "login": "xqc", "displayName": "xQc", "profileImageURL": "https://static-cdn.jtvnw.net/x.jpeg", "followers": { "totalCount": 12641353 },
              "broadcastSettings": { "title": "old title", "game": { "displayName": "Just Chatting" } },
              "stream": { "id": "321500000001", "title": "LOCK IN", "viewersCount": 25551, "createdAt": "2026-10-14T01:02:03Z", "type": "live", "game": { "displayName": "GTA V" } } },
            { "id": "43001749", "login": "kotbarsik_twt", "displayName": "kotbarsik_twt", "profileImageURL": null, "followers": { "totalCount": 0 }, "broadcastSettings": { "title": "Afk cloaking", "game": { "displayName": "EVE Online" } }, "stream": null },
            null
        ] } })
    }

    #[test]
    fn a_live_channel_is_read_with_its_broadcast_and_an_offline_one_without() {
        let users = answer()["data"]["users"].as_array().unwrap().clone();
        let live = read_channel(&users[0]).unwrap();
        assert_eq!((live.login.as_str(), live.display_name.as_str(), live.followers), ("xqc", "xQc", Some(12641353)));
        let on = live.live.unwrap();
        assert_eq!((on.id, on.title.as_str(), on.category.as_deref(), on.viewers), (321500000001, "LOCK IN", Some("GTA V"), Some(25551)));
        assert_eq!(on.started_ts, Some(1791939723));
        let off = read_channel(&users[1]).unwrap();
        assert!(off.live.is_none());
        assert_eq!(off.avatar, None);
        assert!(read_channel(&users[2]).is_none(), "a login Twitch has no channel for comes back as null");
    }

    #[test]
    fn the_event_is_the_shape_the_window_already_draws_for_kick() {
        let users = answer()["data"]["users"].as_array().unwrap().clone();
        let channel = read_channel(&users[0]).unwrap();
        let playback = Playback { url: "https://usher.ttvnw.net/x".into(), expires: 1800000000, hide_ads: false, restricted: vec![] };
        let e = stream_event("twitch:~anonymous|xqc", &channel, Some(&playback), None);
        assert_eq!(e["bufferId"], "twitch:~anonymous|xqc");
        assert_eq!((e["live"].clone(), e["title"].clone(), e["category"].clone(), e["viewers"].clone()), (json!(true), json!("LOCK IN"), json!("GTA V"), json!(25551)));
        assert_eq!((e["playbackUrl"].clone(), e["playbackExpires"].clone(), e["adFree"].clone()), (json!("https://usher.ttvnw.net/x"), json!(1800000000), json!(false)));
        assert_eq!(e["livestreamId"], json!(321500000001u64));
        // An offline channel has no broadcast and no address, and no `following` for a button to offer.
        let off = stream_event("b", &read_channel(&users[1]).unwrap(), None, None);
        assert_eq!((off["live"].clone(), off["playbackUrl"].clone(), off["livestreamId"].clone()), (json!(false), Value::Null, Value::Null));
        assert!(e.get("following").is_none());
        // Live but with nothing to play, and why.
        let blocked = stream_event("b", &channel, None, Some("subscribers only"));
        assert_eq!((blocked["live"].clone(), blocked["playbackError"].clone()), (json!(true), json!("subscribers only")));
    }
}
