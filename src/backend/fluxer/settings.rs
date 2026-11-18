//! What this account has silenced on Fluxer itself.
//!
//! Fluxer keeps notification settings on the account, one object per guild (and one, with no guild, for direct
//! messages): `READY`'s `user_guild_settings`, then `USER_GUILD_SETTINGS_UPDATE` whenever any app changes them
//! (`http-api/users/settings.mdx`). Each has the guild's level and mute, and `channel_overrides`, a map of channel to
//! its own level and mute. A mute may carry an end time (`mute_config.end_time`), after which it no longer stands.
//! Changing a conversation's level here writes its channel override, so it is quiet on a phone as well.

use super::messages;
use super::Handle;
use crate::model::Buffer;
use crate::notify::NotifyLevel;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Fluxer's notification levels: 0 everything, 1 mentions, 2 nothing, 3 as the parent says.
fn level_of(n: Option<i64>) -> Option<NotifyLevel> {
    match n? {
        0 => Some(NotifyLevel::All),
        1 => Some(NotifyLevel::Mentions),
        2 => Some(NotifyLevel::Mute),
        _ => None,
    }
}

/// Whether a mute stands: `muted`, and no end time or one that has not come.
fn mute_stands(entry: &Value, now: i64) -> bool {
    if entry["muted"].as_bool() != Some(true) {
        return false;
    }
    match entry["mute_config"]["end_time"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        Some(end) => end.timestamp() > now,
        None => true,
    }
}

/// What a conversation is set to (what a menu ticks), and what that comes to once its guild is heard: the guild only
/// counts where it is quieter than the default, as an untouched guild says "everything" or "mentions".
pub fn levels(guild: &Value, channel: &str, now: i64) -> (NotifyLevel, NotifyLevel) {
    let over = &guild["channel_overrides"][channel];
    let shown = if mute_stands(over, now) { NotifyLevel::Mute } else { level_of(over["message_notifications"].as_i64()).unwrap_or_default() };
    let guild_quiet = mute_stands(guild, now) || level_of(guild["message_notifications"].as_i64()) == Some(NotifyLevel::Mute);
    let applies = if shown == NotifyLevel::Default && guild_quiet { NotifyLevel::Mute } else { shown };
    (shown, applies)
}

/// The body that sets one channel's level. Fluxer's `channel_overrides` replaces the whole map and wants each entry
/// whole, so every override already held is sent again as it was, and this channel's is written over its own.
pub fn override_body(held: &Value, channel: &str, level: NotifyLevel) -> Value {
    let (notifications, muted) = match level {
        NotifyLevel::All => (0, false),
        NotifyLevel::Mentions => (1, false),
        NotifyLevel::Mute => (1, true),
        _ => (3, false),
    };
    let mut map = serde_json::Map::new();
    for (id, over) in held["channel_overrides"].as_object().into_iter().flatten() {
        map.insert(id.clone(), whole(over));
    }
    let mut mine = whole(&held["channel_overrides"][channel]);
    mine["message_notifications"] = json!(notifications);
    mine["muted"] = json!(muted);
    mine["mute_config"] = Value::Null;
    map.insert(channel.to_string(), mine);
    json!({ "channel_overrides": map })
}

/// An override in the input form, every field present.
fn whole(over: &Value) -> Value {
    let mut out = json!({
        "collapsed": over["collapsed"].as_bool().unwrap_or(false),
        "message_notifications": over["message_notifications"].as_i64().unwrap_or(3),
        "muted": over["muted"].as_bool().unwrap_or(false),
        "mute_config": Value::Null,
    });
    if let Some(end) = over["mute_config"]["end_time"].as_str() {
        out["mute_config"] = json!({ "end_time": end, "selected_time_window": over["mute_config"]["selected_time_window"].as_i64().unwrap_or(0) });
    }
    if let Some(badges) = over["unread_badges"].as_i64() {
        out["unread_badges"] = json!(badges);
    }
    out
}

/// Keeps a settings object, newer versions only, under its guild (or `@me` for direct messages).
pub fn keep(handle: &Handle, entry: &Value) {
    let key = entry["guild_id"].as_str().unwrap_or("@me").to_string();
    let mut held = handle.notify.lock().unwrap_or_else(|e| e.into_inner());
    let older = held.get(&key).and_then(|h| h["version"].as_i64()).unwrap_or(-1);
    if entry["version"].as_i64().unwrap_or(0) >= older {
        held.insert(key, entry.clone());
    }
}

/// Puts what is silenced onto the conversations this account has.
pub fn apply(state: &AppState, handle: &Handle) {
    let now = chrono::Utc::now().timestamp();
    let channels: Vec<(String, String)> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.values().map(|c| (c.id.clone(), c.guild.clone().unwrap_or_else(|| "@me".to_string()))).collect();
    let held = handle.notify.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for (channel, guild) in channels {
        let Some(settings) = held.get(&guild) else { continue };
        if let Some(buffer) = messages::buffer_of(state, &handle.account_id, &channel) {
            let (shown, applies) = levels(settings, &channel, now);
            state.runtime.set_notify_level(state, &buffer.id, shown, applies);
        }
    }
}

/// Sets how much a conversation may interrupt, on the account.
pub async fn set_level(state: &AppState, handle: &Handle, buffer: &Buffer, level: NotifyLevel) -> Result<()> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Fluxer conversation"))?;
    let guild = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel).and_then(|c| c.guild.clone()).unwrap_or_else(|| "@me".to_string());
    let held = handle.notify.lock().unwrap_or_else(|e| e.into_inner()).get(&guild).cloned().unwrap_or(Value::Null);
    let updated = handle.api.patch(&format!("/users/@me/guilds/{guild}/settings"), &override_body(&held, channel, level)).await.map_err(|e| anyhow!("{e}"))?;
    keep(handle, &updated);
    apply(state, handle);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn a_channel_or_its_guild_can_silence_it_and_a_timed_mute_ends() {
        let g = json!({ "message_notifications": 1, "muted": false, "channel_overrides": {
            "A": { "message_notifications": 0, "muted": false },
            "B": { "message_notifications": 3, "muted": true, "mute_config": { "end_time": "2028-01-15T08:00:00Z" } },
            "C": { "message_notifications": 3, "muted": true, "mute_config": { "end_time": "2020-01-01T00:00:00Z" } }
        } });
        assert_eq!(levels(&g, "A", NOW), (NotifyLevel::All, NotifyLevel::All));
        assert_eq!(levels(&g, "B", NOW), (NotifyLevel::Mute, NotifyLevel::Mute), "still running");
        assert_eq!(levels(&g, "C", NOW), (NotifyLevel::Default, NotifyLevel::Default), "ran out");
        assert_eq!(levels(&g, "D", NOW), (NotifyLevel::Default, NotifyLevel::Default), "an untouched guild set to mentions is not carried down");
        let muted = json!({ "message_notifications": 0, "muted": true, "channel_overrides": { "A": { "message_notifications": 0, "muted": false } } });
        assert_eq!(levels(&muted, "D", NOW), (NotifyLevel::Default, NotifyLevel::Mute), "left on its default in a muted guild");
        assert_eq!(levels(&muted, "A", NOW), (NotifyLevel::All, NotifyLevel::All), "a channel with its own say keeps it");
    }

    #[test]
    fn a_level_is_written_as_the_channels_override_and_the_others_are_kept_whole() {
        let held = json!({ "channel_overrides": { "B": { "collapsed": true, "message_notifications": 2, "muted": false } } });
        let body = override_body(&held, "A", NotifyLevel::Mute);
        assert_eq!(body["channel_overrides"]["A"], json!({ "collapsed": false, "message_notifications": 1, "muted": true, "mute_config": null }));
        assert_eq!(body["channel_overrides"]["B"], json!({ "collapsed": true, "message_notifications": 2, "muted": false, "mute_config": null }), "the map replaces every override, so B goes again");
        assert_eq!(override_body(&Value::Null, "A", NotifyLevel::All)["channel_overrides"]["A"]["message_notifications"], 0);
        assert_eq!(override_body(&Value::Null, "A", NotifyLevel::Default)["channel_overrides"]["A"]["message_notifications"], 3);
    }
}
