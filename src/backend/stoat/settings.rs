//! What this account has silenced on Stoat itself.
//!
//! Stoat keeps an account's settings on the account (`/sync/settings`), so every app signed in to it hears the same.
//! The one this reads is `notifications`, which the official web app writes as a JSON string:
//!
//! ```text
//! { "server": { id: "all" | "mention" | "none" | "muted" }, "channel": { ... },
//!   "server_mutes": { id: { "until": ms } }, "channel_mutes": { id: { } } }
//! ```
//!
//! A mute with no `until` lasts for ever; one with a time ends then (the app expects the client to look at the
//! clock). The older form said `"muted"` in the first two. Muting a conversation from here sets it on the account, so
//! it is quiet on a phone as well, and what is not understood in the settings is written back as it was found.

use super::messages;
use super::world::Kind;
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

/// The settings' key for notifications.
const KEY: &str = "notifications";

/// Whether a server's or a channel's entry silences it now.
fn silenced_in(settings: &Value, level: &str, mutes: &str, id: &str, now_ms: i64) -> bool {
    if settings[level][id].as_str() == Some("muted") {
        return true;
    }
    match settings[mutes].get(id).filter(|m| m.is_object()) {
        Some(mute) => mute["until"].as_i64().map(|until| until > now_ms).unwrap_or(true),
        None => false,
    }
}

/// Whether a conversation is silenced: itself, or the server holding it.
pub fn is_muted(settings: &Value, channel: &str, server: Option<&str>, now_ms: i64) -> bool {
    silenced_in(settings, "channel", "channel_mutes", channel, now_ms) || server.is_some_and(|s| silenced_in(settings, "server", "server_mutes", s, now_ms))
}

/// The settings with one channel muted for ever, or let be heard again.
pub fn with_channel_muted(settings: &Value, channel: &str, muted: bool) -> Value {
    let mut out = if settings.is_object() { settings.clone() } else { json!({}) };
    let object = out.as_object_mut().expect("it was made an object");
    let mutes = object.entry("channel_mutes").or_insert_with(|| json!({}));
    if !mutes.is_object() {
        *mutes = json!({});
    }
    let mutes = mutes.as_object_mut().expect("it was made an object");
    if muted {
        mutes.insert(channel.to_string(), json!({}));
    } else {
        mutes.remove(channel);
        // The older form of the same thing.
        if let Some(levels) = object.get_mut("channel").and_then(Value::as_object_mut) {
            if levels.get(channel).and_then(Value::as_str) == Some("muted") {
                levels.remove(channel);
            }
        }
    }
    out
}

/// The settings as Stoat sends them: a pair of when and a JSON string.
fn parse(entry: &Value) -> Option<Value> {
    let text = entry.as_array().and_then(|a| a.get(1)).and_then(Value::as_str).or_else(|| entry.as_str())?;
    serde_json::from_str::<Value>(text).ok().filter(Value::is_object)
}

/// Puts what is silenced onto the conversations this account has.
pub fn apply(state: &AppState, handle: &Handle) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let settings = handle.world.lock().unwrap_or_else(|e| e.into_inner()).notifications.clone();
    let wanted: Vec<(String, bool)> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.channels.values().filter(|c| c.kind != Kind::Voice || c.voice).map(|c| (c.id.clone(), is_muted(&settings, &c.id, c.server.as_deref(), now_ms))).collect()
    };
    for (channel, muted) in wanted {
        if let Some(buffer) = messages::buffer_of(state, &handle.account_id, &channel) {
            if buffer.server_muted != muted {
                state.runtime.set_silenced(state, &buffer.id, muted);
            }
        }
    }
}

/// Reads the account's notification settings and applies them.
pub async fn load(state: &AppState, handle: &Handle) {
    let answer = match handle.api.post("/sync/settings/fetch", &json!({ "keys": [KEY] })).await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!("stoat[{}]: the settings were not read: {e}", handle.account_id);
            return;
        }
    };
    if let Some(settings) = parse(&answer[KEY]) {
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).notifications = settings;
    }
    apply(state, handle);
}

/// A settings change another app made: `UserSettingsUpdate`.
pub fn changed(state: &AppState, handle: &Handle, update: &Value) {
    if let Some(settings) = parse(&update[KEY]) {
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).notifications = settings;
        apply(state, handle);
    }
}

/// Mutes a conversation on the account, or lets it be heard again.
pub async fn set_muted(state: &AppState, handle: &Handle, buffer: &Buffer, muted: bool) -> Result<()> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    // Read again just before, so a change made on a phone a moment ago is not written over.
    load(state, handle).await;
    let next = with_channel_muted(&handle.world.lock().unwrap_or_else(|e| e.into_inner()).notifications, channel, muted);
    let text = serde_json::to_string(&next)?;
    let mut body = Map::new();
    body.insert(KEY.to_string(), Value::String(text));
    handle.api.post(&format!("/sync/settings/set?timestamp={}", chrono::Utc::now().timestamp_millis()), &Value::Object(body)).await.map_err(|e| anyhow!("{e}"))?;
    handle.world.lock().unwrap_or_else(|e| e.into_inner()).notifications = next;
    apply(state, handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn a_channel_or_its_server_can_silence_it_and_a_timed_mute_ends() {
        let s = json!({ "channel_mutes": { "C1": {}, "C2": { "until": NOW + 1000 }, "C3": { "until": NOW - 1000 } }, "server_mutes": { "S1": {} }, "channel": { "C4": "muted" }, "server": { "S2": "mention" } });
        assert!(is_muted(&s, "C1", None, NOW));
        assert!(is_muted(&s, "C2", None, NOW), "still running");
        assert!(!is_muted(&s, "C3", None, NOW), "ran out");
        assert!(is_muted(&s, "C4", None, NOW), "the older form");
        assert!(is_muted(&s, "C9", Some("S1"), NOW), "everything in a muted server");
        assert!(!is_muted(&s, "C9", Some("S2"), NOW), "mentions only is not a mute");
        assert!(!is_muted(&json!({}), "C1", Some("S1"), NOW));
    }

    #[test]
    fn muting_one_channel_leaves_the_rest_of_the_settings_as_they_were() {
        let s = json!({ "server": { "S": "mention" }, "channel_mutes": { "A": {} }, "channel": { "B": "muted", "Z": "all" }, "later": 1 });
        let on = with_channel_muted(&s, "C", true);
        assert_eq!(on["channel_mutes"], json!({ "A": {}, "C": {} }));
        assert_eq!(on["later"], 1);
        let off = with_channel_muted(&on, "B", false);
        assert_eq!(off["channel"], json!({ "Z": "all" }), "the older form is let go too");
        assert_eq!(with_channel_muted(&Value::Null, "C", true), json!({ "channel_mutes": { "C": {} } }));
    }

    #[test]
    fn the_settings_come_as_a_time_and_a_string() {
        assert_eq!(parse(&json!([1700, "{\"channel_mutes\":{\"A\":{}}}"])).unwrap()["channel_mutes"]["A"], json!({}));
        assert!(parse(&json!([1700, "not json"])).is_none());
        assert!(parse(&Value::Null).is_none());
    }
}
