//! Scheduled events: a guild's calendar.
//!
//! Each guild lists its events in READY / GUILD_CREATE and keeps the list
//! current with GUILD_SCHEDULED_EVENT_CREATE / _UPDATE / _DELETE, and with
//! _USER_ADD / _USER_REMOVE as people mark themselves interested. What the
//! window needs - how many are interested, and whether this account is - is
//! read from REST when the list is opened, since the gateway copy carries
//! neither.
//!
//! An event happens somewhere: a stage (entity type 1), a voice channel (2),
//! or somewhere outside Discord (3), which has a written location and must
//! have an end time.

use super::*;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// Account and guild to that guild's events, as Discord sends them.
fn known() -> &'static Mutex<HashMap<(String, String), Vec<Value>>> {
    static KNOWN: OnceLock<Mutex<HashMap<(String, String), Vec<Value>>>> = OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

/// Account to the events it has marked itself interested in.
fn interested() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static INTERESTED: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    INTERESTED.get_or_init(Default::default)
}

/// Still to come or happening now. Completed (3) and cancelled (4) are gone
/// from Discord's own list, and so from this one.
fn current(e: &Value) -> bool {
    matches!(e["status"].as_u64(), Some(1) | Some(2))
}

/// A guild's whole list, from READY or GUILD_CREATE.
pub fn note_guild(account_id: &str, guild_id: &str, list: &Value) {
    let events: Vec<Value> = list.as_array().into_iter().flatten().filter(|e| current(e)).cloned().collect();
    known().lock().unwrap().insert((account_id.to_string(), guild_id.to_string()), events);
}

/// One event created, changed or removed. Says which guild, for the window.
pub fn note_change(state: &AppState, account_id: &str, dispatch: &str, d: &Value) {
    let (Some(guild_id), Some(id)) = (d["guild_id"].as_str(), d["id"].as_str()) else { return };
    {
        let mut all = known().lock().unwrap();
        let list = all.entry((account_id.to_string(), guild_id.to_string())).or_default();
        let was = list.iter().position(|e| e["id"].as_str() == Some(id));
        let kept = dispatch != "GUILD_SCHEDULED_EVENT_DELETE" && current(d);
        match (was, kept) {
            (Some(i), true) => {
                // The gateway copy has no count; keep the one REST gave.
                let count = list[i]["user_count"].clone();
                list[i] = d.clone();
                if list[i]["user_count"].is_null() {
                    list[i]["user_count"] = count;
                }
            }
            (Some(i), false) => {
                list.remove(i);
            }
            (None, true) => list.push(d.clone()),
            (None, false) => {}
        }
    }
    announce(state, account_id, guild_id);
}

/// Somebody marked an event as interesting, or stopped.
pub fn note_user(state: &AppState, account_id: &str, own_user: &str, added: bool, d: &Value) {
    let (Some(guild_id), Some(event_id)) = (d["guild_id"].as_str(), d["guild_scheduled_event_id"].as_str()) else { return };
    if d["user_id"].as_str() == Some(own_user) {
        let mut all = interested().lock().unwrap();
        let mine = all.entry(account_id.to_string()).or_default();
        if added {
            mine.insert(event_id.to_string());
        } else {
            mine.remove(event_id);
        }
    }
    if let Some(list) = known().lock().unwrap().get_mut(&(account_id.to_string(), guild_id.to_string())) {
        if let Some(e) = list.iter_mut().find(|e| e["id"].as_str() == Some(event_id)) {
            let n = e["user_count"].as_i64().unwrap_or(0) + if added { 1 } else { -1 };
            e["user_count"] = json!(n.max(0));
        }
    }
    announce(state, account_id, guild_id);
}

fn announce(state: &AppState, account_id: &str, guild_id: &str) {
    let count = known().lock().unwrap().get(&(account_id.to_string(), guild_id.to_string())).map(|l| l.len()).unwrap_or(0);
    state.events.emit("discordEvents", json!({ "accountId": account_id, "guildId": guild_id, "count": count }));
}

/// How many events a guild has coming or on, for the row at the top of its
/// channel list - which is only there when this is more than none.
pub fn count(account_id: &str, guild_id: &str) -> usize {
    known().lock().unwrap().get(&(account_id.to_string(), guild_id.to_string())).map(|l| l.len()).unwrap_or(0)
}

/// The guild's events as the window draws them, soonest first: refreshed
/// from REST for the counts, and this account's interest in each.
pub async fn list(state: &AppState, account_id: &str, guild_id: &str) -> Result<Vec<Value>> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = http_client_for(&cfg.token)
        .get(format!("{API_BASE}/guilds/{guild_id}/scheduled-events"))
        .query(&[("with_user_count", "true")])
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("reading the events")?;
    if resp.status().is_success() {
        let fresh: Value = resp.json().await.context("reading the events")?;
        note_guild(account_id, guild_id, &fresh);
    }
    let events = known().lock().unwrap().get(&(account_id.to_string(), guild_id.to_string())).cloned().unwrap_or_default();

    // Whether this account is interested in each, which only the event's own
    // list of interested people says. A handful of events; one read each.
    let mut mine = interested().lock().unwrap().get(account_id).cloned().unwrap_or_default();
    for e in &events {
        let Some(id) = e["id"].as_str() else { continue };
        if let Ok(resp) = http_client_for(&cfg.token)
            .get(format!("{API_BASE}/guilds/{guild_id}/scheduled-events/{id}/users"))
            .query(&[("limit", "100")])
            .header("Authorization", &cfg.token)
            .send()
            .await
        {
            if let Ok(users) = resp.json::<Value>().await {
                let me = users.as_array().into_iter().flatten().any(|u| u["user"]["id"].as_str() == Some(cfg.user_id.as_str()));
                if me {
                    mine.insert(id.to_string());
                } else {
                    mine.remove(id);
                }
            }
        }
    }
    interested().lock().unwrap().insert(account_id.to_string(), mine.clone());

    let mut out: Vec<Value> = events.iter().map(|e| shape(state, account_id, guild_id, e, &mine)).collect();
    out.sort_by(|a, b| a["start"].as_str().cmp(&b["start"].as_str()));
    Ok(out)
}

/// One event as the window wants it.
fn shape(state: &AppState, account_id: &str, guild_id: &str, e: &Value, mine: &HashSet<String>) -> Value {
    let id = e["id"].as_str().unwrap_or_default();
    let channel_id = e["channel_id"].as_str();
    let channel_name = channel_id.and_then(|c| {
        state.runtime.discord_voice_channels(account_id, guild_id).into_iter().find(|v| v.id == c).map(|v| v.name)
    });
    let creator = &e["creator"];
    let creator_name = creator["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| creator["username"].as_str());
    let creator_avatar = match (creator["id"].as_str(), creator["avatar"].as_str()) {
        (Some(uid), Some(hash)) => Some(format!("https://cdn.discordapp.com/avatars/{uid}/{hash}.png?size=64")),
        _ => None,
    };
    let image = e["image"].as_str().map(|hash| format!("https://cdn.discordapp.com/guild-events/{id}/{hash}.png?size=512"));
    json!({
        "id": id,
        "guildId": guild_id,
        "name": e["name"],
        "description": e["description"],
        "start": e["scheduled_start_time"],
        "end": e["scheduled_end_time"],
        // 1 scheduled, 2 happening now.
        "live": e["status"].as_u64() == Some(2),
        // 1 stage, 2 voice channel, 3 somewhere else.
        "where": match e["entity_type"].as_u64() { Some(1) => "stage", Some(2) => "voice", _ => "external" },
        "channelId": channel_id,
        "channelName": channel_name,
        "location": e["entity_metadata"]["location"],
        "creatorId": e["creator_id"],
        "creatorName": creator_name,
        "creatorAvatar": creator_avatar,
        "image": image,
        "userCount": e["user_count"].as_u64().unwrap_or(0),
        "interested": mine.contains(id),
        "link": format!("https://discord.com/events/{guild_id}/{id}"),
    })
}

/// What a new event is, as the window's form has it.
pub struct NewEvent<'a> {
    pub name: &'a str,
    pub description: &'a str,
    /// RFC 3339.
    pub start: &'a str,
    pub end: Option<&'a str>,
    /// "voice", "stage" or "external".
    pub kind: &'a str,
    pub channel_id: Option<&'a str>,
    pub location: Option<&'a str>,
}

/// The body Discord takes to create one, checked for what it would refuse.
pub fn creation_body(new: &NewEvent) -> Result<Value> {
    if new.name.trim().is_empty() {
        bail!("an event needs a name");
    }
    let start = chrono::DateTime::parse_from_rfc3339(new.start).context("that start time is not a time")?;
    if start <= chrono::Utc::now() {
        bail!("an event has to start in the future");
    }
    let mut body = json!({
        "name": new.name.trim(),
        "description": new.description.trim(),
        "scheduled_start_time": new.start,
        // Guild-only, the one level Discord offers.
        "privacy_level": 2,
    });
    if let Some(end) = new.end.filter(|e| !e.is_empty()) {
        let end_at = chrono::DateTime::parse_from_rfc3339(end).context("that end time is not a time")?;
        if end_at <= start {
            bail!("an event has to end after it starts");
        }
        body["scheduled_end_time"] = json!(end);
    }
    match new.kind {
        "voice" | "stage" => {
            let channel = new.channel_id.filter(|c| !c.is_empty()).context("pick the channel it happens in")?;
            body["entity_type"] = json!(if new.kind == "stage" { 1 } else { 2 });
            body["channel_id"] = json!(channel);
        }
        _ => {
            let location = new.location.map(str::trim).filter(|l| !l.is_empty()).context("say where it happens")?;
            if body["scheduled_end_time"].is_null() {
                bail!("an event somewhere else needs an end time");
            }
            body["entity_type"] = json!(3);
            body["channel_id"] = Value::Null;
            body["entity_metadata"] = json!({ "location": location });
        }
    }
    Ok(body)
}

pub async fn create(state: &AppState, account_id: &str, guild_id: &str, new: &NewEvent<'_>) -> Result<()> {
    let body = creation_body(new)?;
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = send_write(
        http_client_for(&cfg.token)
            .post(format!("{API_BASE}/guilds/{guild_id}/scheduled-events"))
            .header("Authorization", &cfg.token)
            .json(&body),
    )
    .await
    .context("creating the event")?;
    refused_says(resp, "create events here").await
}

/// Start it now, end it, or cancel it - which are its status going to 2, 3
/// or 4. Discord allows each only from the right state, and says so.
pub async fn set_status(state: &AppState, account_id: &str, guild_id: &str, event_id: &str, status: u8) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let resp = send_write(
        http_client_for(&cfg.token)
            .patch(format!("{API_BASE}/guilds/{guild_id}/scheduled-events/{event_id}"))
            .header("Authorization", &cfg.token)
            .json(&json!({ "status": status })),
    )
    .await
    .context("changing the event")?;
    refused_says(resp, "manage this event").await
}

/// Marks this account interested, or not.
pub async fn set_interested(state: &AppState, account_id: &str, guild_id: &str, event_id: &str, on: bool) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let url = format!("{API_BASE}/guilds/{guild_id}/scheduled-events/{event_id}/users/@me");
    let request = if on { http_client_for(&cfg.token).put(url) } else { http_client_for(&cfg.token).delete(url) };
    let resp = send_write(request.header("Authorization", &cfg.token)).await.context("changing your interest")?;
    refused_says(resp, "mark interest in this event").await
}

async fn refused_says(resp: reqwest::Response, what: &str) -> Result<()> {
    if resp.status().is_success() {
        return Ok(());
    }
    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN {
        bail!("this account is not allowed to {what}");
    }
    let text = resp.text().await.unwrap_or_default();
    bail!("Discord API error {status}: {text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn later(hours: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::hours(hours)).to_rfc3339()
    }

    #[test]
    fn a_voice_event_names_its_channel() {
        let start = later(24);
        let body = creation_body(&NewEvent {
            name: " Testing ",
            description: "The test event",
            start: &start,
            end: None,
            kind: "voice",
            channel_id: Some("42"),
            location: None,
        })
        .unwrap();
        assert_eq!(body["name"], "Testing");
        assert_eq!(body["entity_type"], 2);
        assert_eq!(body["channel_id"], "42");
        assert_eq!(body["privacy_level"], 2);
    }

    #[test]
    fn an_event_elsewhere_needs_a_place_and_an_end() {
        let start = later(24);
        let end = later(26);
        let mut new = NewEvent { name: "Meetup", description: "", start: &start, end: None, kind: "external", channel_id: None, location: Some("The park") };
        assert!(creation_body(&new).is_err());
        new.end = Some(&end);
        let body = creation_body(&new).unwrap();
        assert_eq!(body["entity_type"], 3);
        assert_eq!(body["entity_metadata"]["location"], "The park");
        new.location = Some("  ");
        assert!(creation_body(&new).is_err());
    }

    #[test]
    fn an_event_cannot_start_in_the_past_or_end_before_it_starts() {
        let past = later(-1);
        let start = later(5);
        let before = later(4);
        let voice = |start: &str, end: Option<&str>| -> bool {
            creation_body(&NewEvent { name: "x", description: "", start, end, kind: "voice", channel_id: Some("1"), location: None }).is_ok()
        };
        assert!(!voice(&past, None));
        assert!(!voice(&start, Some(&before)));
        assert!(voice(&start, None));
    }

    #[test]
    fn finished_and_cancelled_events_are_not_listed() {
        note_guild("a", "g", &json!([{ "id": "1", "status": 1 }, { "id": "2", "status": 3 }, { "id": "3", "status": 4 }, { "id": "4", "status": 2 }]));
        assert_eq!(count("a", "g"), 2);
    }
}
