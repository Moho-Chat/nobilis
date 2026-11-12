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
use crate::notify::NotifyLevel;
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

/// The settings' key for notifications.
const KEY: &str = "notifications";

/// What a server's or channel's own entry says it is told of: the level the
/// settings name, or none.
///
/// Stoat spells them `all`, `mention`, `none` and the older `muted`. Told of
/// nothing and muted are the same thing from where a person sits, and are read
/// as one.
fn named_level(settings: &Value, level: &str, id: &str) -> Option<NotifyLevel> {
    match settings[level][id].as_str()? {
        "all" => Some(NotifyLevel::All),
        "mention" => Some(NotifyLevel::Mentions),
        "none" | "muted" => Some(NotifyLevel::Mute),
        _ => None,
    }
}

/// Whether a mute stands: an entry in the mutes map, with no end time or one
/// that has not come.
fn mute_stands(settings: &Value, mutes: &str, id: &str, now_ms: i64) -> bool {
    match settings[mutes].get(id).filter(|m| m.is_object()) {
        Some(mute) => mute["until"].as_i64().map(|until| until > now_ms).unwrap_or(true),
        None => false,
    }
}

/// What a conversation is set to, and what that comes to once the server
/// holding it is taken into account.
///
/// The first is what a menu ticks - the channel's own entry, a mute standing
/// against it, or nothing. The second also hears the server, but only where it
/// is quiet: a server set to everything is how Stoat reads a server nobody
/// changed, and carrying that down would make every channel in it announce.
pub fn levels(settings: &Value, channel: &str, server: Option<&str>, now_ms: i64) -> (NotifyLevel, NotifyLevel) {
    let shown = if mute_stands(settings, "channel_mutes", channel, now_ms) { NotifyLevel::Mute } else { named_level(settings, "channel", channel).unwrap_or_default() };
    let server_quiet = server.is_some_and(|s| mute_stands(settings, "server_mutes", s, now_ms) || named_level(settings, "server", s) == Some(NotifyLevel::Mute));
    let applies = if server_quiet && shown == NotifyLevel::Default { NotifyLevel::Mute } else { shown };
    (shown, applies)
}

/// The settings with one channel set to a level.
///
/// A mute is its own map, with no end, and the other three are the channel's
/// own entry; whichever is not being set is taken out, so a channel is never
/// both muted and told of everything. What is not understood is written back as
/// it was found.
pub fn with_channel_level(settings: &Value, channel: &str, level: NotifyLevel) -> Value {
    let mut out = if settings.is_object() { settings.clone() } else { json!({}) };
    let object = out.as_object_mut().expect("it was made an object");
    for key in ["channel_mutes", "channel"] {
        let entry = object.entry(key).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
    }
    object.get_mut("channel_mutes").and_then(Value::as_object_mut).expect("made above").remove(channel);
    let levels = object.get_mut("channel").and_then(Value::as_object_mut).expect("made above");
    levels.remove(channel);
    match level {
        NotifyLevel::Mute => {
            object.get_mut("channel_mutes").and_then(Value::as_object_mut).expect("made above").insert(channel.to_string(), json!({}));
        }
        NotifyLevel::All => {
            levels.insert(channel.to_string(), json!("all"));
        }
        NotifyLevel::Mentions => {
            levels.insert(channel.to_string(), json!("mention"));
        }
        NotifyLevel::Default => {}
    }
    // An emptied map is no map: the settings are shared with the official
    // app, and a row it never wrote is one more thing for it to carry.
    for key in ["channel_mutes", "channel"] {
        if object.get(key).and_then(Value::as_object).is_some_and(|m| m.is_empty()) {
            object.remove(key);
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
    let wanted: Vec<(String, (NotifyLevel, NotifyLevel))> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.channels.values().filter(|c| c.kind != Kind::Voice || c.voice).map(|c| (c.id.clone(), levels(&settings, &c.id, c.server.as_deref(), now_ms))).collect()
    };
    for (channel, (shown, applies)) in wanted {
        if let Some(buffer) = messages::buffer_of(state, &handle.account_id, &channel) {
            state.runtime.set_notify_level(state, &buffer.id, shown, applies);
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

/// Sets how much a conversation may interrupt, on the account.
pub async fn set_level(state: &AppState, handle: &Handle, buffer: &Buffer, level: NotifyLevel) -> Result<()> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    // Read again just before, so a change made on a phone a moment ago is not written over.
    load(state, handle).await;
    let next = with_channel_level(&handle.world.lock().unwrap_or_else(|e| e.into_inner()).notifications, channel, level);
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
        let s = json!({ "channel_mutes": { "C1": {}, "C2": { "until": NOW + 1000 }, "C3": { "until": NOW - 1000 } }, "server_mutes": { "S1": {} }, "channel": { "C4": "muted", "C5": "none" }, "server": { "S2": "mention", "S3": "none" } });
        let muted = |c: &str, server: Option<&str>| levels(&s, c, server, NOW).1 == NotifyLevel::Mute;
        assert!(muted("C1", None));
        assert!(muted("C2", None), "still running");
        assert!(!muted("C3", None), "ran out");
        assert!(muted("C4", None), "the older form");
        assert!(muted("C5", None), "told of nothing");
        assert!(muted("C9", Some("S1")), "everything in a muted server");
        assert!(muted("C9", Some("S3")), "a server told of nothing");
        assert!(!muted("C9", Some("S2")), "mentions only is not a mute");
        assert!(!levels(&json!({}), "C1", Some("S1"), NOW).1.eq(&NotifyLevel::Mute));
    }

    #[test]
    fn a_channel_is_read_as_set_and_as_it_comes_to() {
        let s = json!({ "channel": { "A": "all", "B": "mention" }, "server_mutes": { "S": {} }, "server": { "L": "all" } });
        assert_eq!(levels(&s, "A", None, NOW), (NotifyLevel::All, NotifyLevel::All));
        assert_eq!(levels(&s, "B", None, NOW), (NotifyLevel::Mentions, NotifyLevel::Mentions));
        assert_eq!(levels(&s, "C", None, NOW), (NotifyLevel::Default, NotifyLevel::Default));
        // Left on its default inside a muted server: ticked as default, silent.
        assert_eq!(levels(&s, "C", Some("S"), NOW), (NotifyLevel::Default, NotifyLevel::Mute));
        // A channel with its own say is not overruled by the server's quiet.
        assert_eq!(levels(&s, "A", Some("S"), NOW), (NotifyLevel::All, NotifyLevel::All));
        // A server set to everything is not carried down.
        assert_eq!(levels(&s, "C", Some("L"), NOW), (NotifyLevel::Default, NotifyLevel::Default));
    }

    #[test]
    fn setting_one_channel_leaves_the_rest_of_the_settings_as_they_were() {
        let s = json!({ "server": { "S": "mention" }, "channel_mutes": { "A": {} }, "channel": { "B": "muted", "Z": "all" }, "later": 1 });
        let on = with_channel_level(&s, "C", NotifyLevel::Mute);
        assert_eq!(on["channel_mutes"], json!({ "A": {}, "C": {} }));
        assert_eq!(on["later"], 1);
        let off = with_channel_level(&on, "B", NotifyLevel::Default);
        assert_eq!(off["channel"], json!({ "Z": "all" }), "the older form is let go too");
        assert_eq!(with_channel_level(&Value::Null, "C", NotifyLevel::Mute), json!({ "channel_mutes": { "C": {} } }));
    }

    #[test]
    fn a_channel_is_never_muted_and_told_of_everything_at_once() {
        let muted = with_channel_level(&json!({}), "C", NotifyLevel::Mute);
        let all = with_channel_level(&muted, "C", NotifyLevel::All);
        assert_eq!(all, json!({ "channel": { "C": "all" } }), "the mute is gone, and the emptied map with it");
        let mentions = with_channel_level(&all, "C", NotifyLevel::Mentions);
        assert_eq!(mentions["channel"]["C"], "mention");
        let default = with_channel_level(&mentions, "C", NotifyLevel::Default);
        assert_eq!(default, json!({}), "default is absence");
    }

    #[test]
    fn the_settings_come_as_a_time_and_a_string() {
        assert_eq!(parse(&json!([1700, "{\"channel_mutes\":{\"A\":{}}}"])).unwrap()["channel_mutes"]["A"], json!({}));
        assert!(parse(&json!([1700, "not json"])).is_none());
        assert!(parse(&Value::Null).is_none());
    }
}
