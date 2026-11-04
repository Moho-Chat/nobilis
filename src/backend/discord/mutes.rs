//! What this account has silenced on Discord itself.
//!
//! Muting a server or a channel in the official client is an account setting,
//! not a preference in one window: it arrives in READY as
//! `user_guild_settings` and every device signed in to the account honours
//! it. Nothing here read that, so a guild somebody deliberately silenced
//! months ago was as loud in moho as anything else - and the busiest servers
//! are exactly the ones people mute.
//!
//! Read first, and then written for a channel: muting one from here sets it on
//! the account, so it is quiet on a phone as well as in this window - which
//! is what somebody means by muting it. Discord answers with the account's
//! settings as they now stand, and those are taken in like any others.
//!
//! Applying is deliberately its own step. The settings arrive in READY, and
//! the channels they name arrive afterwards in GUILD_CREATE - so a mute
//! applied at the moment it is read would be applied to buffers that do not
//! exist yet, which is why what is read is kept and applied again whenever
//! there is something new to apply it to.

use super::*;
use crate::notify::NotifyLevel;
use crate::runtime::{DiscordChannelNotify, DiscordMute};

/// Direct messages have no guild, and Discord marks their settings entry with
/// a null guild id. `@me` is what Discord's own API calls that scope.
pub(super) const DM_SCOPE: &str = "@me";

/// Reads one `user_guild_settings` entry.
///
/// Returns the guild it is about along with what it says. A `mute_config`
/// carries an end time for a mute somebody set to expire - "mute for 8 hours" -
/// so a mute whose moment has passed is read as no mute at all rather than
/// as a permanent one.
pub(super) fn parse_entry(entry: &Value, now: i64) -> (String, DiscordMute) {
    let guild = entry["guild_id"].as_str().unwrap_or(DM_SCOPE).to_string();
    let mut channels = HashMap::new();
    for over in entry["channel_overrides"].as_array().into_iter().flatten() {
        let Some(channel_id) = over["channel_id"].as_str() else { continue };
        channels.insert(channel_id.to_string(), DiscordChannelNotify { muted: still_muted(over, now), level: over["message_notifications"].as_i64() });
    }
    (guild, DiscordMute { muted: still_muted(entry, now), level: entry["message_notifications"].as_i64(), channels })
}

/// Whether a mute flag is a mute now.
///
/// Discord keeps a timed mute as the flag plus an end time, and leaves the
/// flag set after that moment passes - the client is expected to notice. One
/// that has run out is not a mute.
fn still_muted(block: &Value, now: i64) -> bool {
    if !block["muted"].as_bool().unwrap_or(false) {
        return false;
    }
    match block["mute_config"]["end_time"].as_str() {
        Some(at) => chrono::DateTime::parse_from_rfc3339(at).map(|t| t.timestamp() > now).unwrap_or(true),
        None => true,
    }
}

/// Takes in everything READY says about muting, or one entry from an update.
pub(super) fn note_settings(state: &AppState, account_id: &str, settings: &Value) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    // READY carries a list; newer payloads wrap it in an object with the list
    // under `entries`, and an update sends a single entry with no wrapper at
    // all. All three are the same thing said three ways.
    let entries: Vec<Value> = if let Some(list) = settings.as_array() {
        list.clone()
    } else if let Some(list) = settings["entries"].as_array() {
        list.clone()
    } else {
        vec![settings.clone()]
    };
    for entry in entries {
        if !entry.is_object() {
            continue;
        }
        let (guild, mute) = parse_entry(&entry, now);
        state.runtime.set_discord_mute(account_id, &guild, mute);
    }
    apply(state, account_id);
}

/// What to send to give one channel a level: its own setting inside its
/// guild's, with no end time - a mute that does not expire.
///
/// Discord's own numbers: 0 for every message, 1 for mentions only, and 3 for
/// "use the server's", which is how its client takes a channel's override off
/// without deleting the entry. Muting is a separate switch from the number,
/// and is left alone beyond turning it on - what a channel was set to before
/// it was muted is what it goes back to.
fn override_body(channel_id: &str, level: NotifyLevel) -> Value {
    let mut over = json!({ "muted": level == NotifyLevel::Mute, "mute_config": null });
    let number = match level {
        NotifyLevel::All => Some(0),
        NotifyLevel::Mentions => Some(1),
        NotifyLevel::Default => Some(3),
        NotifyLevel::Mute => None,
    };
    if let Some(number) = number {
        over["message_notifications"] = json!(number);
    }
    json!({ "channel_overrides": { channel_id: over } })
}

/// Sets how much a conversation may interrupt, on the account itself.
///
/// A muted channel still counts what was said to its reader - Discord keeps
/// the mention badge on one - and stops sounding on every device signed in to
/// the account. Direct messages are the `@me` scope.
pub async fn set_level(state: &AppState, account_id: &str, buffer_id: &str, level: NotifyLevel) -> anyhow::Result<()> {
    use anyhow::Context;
    let config = state.accounts.get_discord(account_id).context("no such account")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("not a Discord conversation")?;
    let guild = state.runtime.get_discord_guild(buffer_id).unwrap_or_else(|| DM_SCOPE.to_string());
    let response = send_write(
        http_client_for(&config.token)
            .patch(format!("{API_BASE}/users/@me/guilds/{guild}/settings"))
            .header("Authorization", &config.token)
            .json(&override_body(&channel_id, level)),
    )
    .await?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("Discord refused to set it to {} (HTTP {})", level.as_str(), status.as_u16());
    }
    // The account's settings as they now are. The gateway says the same a
    // moment later; taking it here as well is what makes the change show
    // without waiting on that.
    if let Ok(now) = response.json::<Value>().await {
        note_settings(state, account_id, &now);
    }
    // And judged by what Discord says, not by what was asked: a 200 with the
    // channel unchanged is not a channel that was set.
    let (shown, _) = state.runtime.discord_notify(account_id, &guild, &channel_id);
    if shown != level {
        anyhow::bail!("Discord accepted the change but still has this conversation on {}", shown.as_str());
    }
    Ok(())
}

/// Puts what is known onto the buffers that exist.
///
/// Cheap enough to run whenever anything might have changed - one pass over
/// this account's conversations - and `set_notify_level` only tells the client
/// about the ones that actually moved.
pub(super) fn apply(state: &AppState, account_id: &str) {
    let prefix = format!("{account_id}|");
    for buffer in state.runtime.list_buffers() {
        if !buffer.id.starts_with(&prefix) {
            continue;
        }
        let Some(channel_id) = state.runtime.get_discord_channel(&buffer.id) else { continue };
        let guild = state.runtime.get_discord_guild(&buffer.id).unwrap_or_else(|| DM_SCOPE.to_string());
        // What Discord says, and only that: `set_notify_level` tells the
        // clients about the conversations that actually moved.
        let (shown, applies) = state.runtime.discord_notify(account_id, &guild, &channel_id);
        state.runtime.set_notify_level(state, &buffer.id, shown, applies);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn muting_a_channel_is_its_own_setting_with_no_end() {
        assert_eq!(
            override_body("100", NotifyLevel::Mute),
            json!({ "channel_overrides": { "100": { "muted": true, "mute_config": null } } })
        );
        assert_eq!(override_body("100", NotifyLevel::Default)["channel_overrides"]["100"]["muted"], false);
    }

    /// Discord's own numbers, and "use the server's" for taking an override off.
    #[test]
    fn each_level_is_written_as_discord_numbers_it() {
        let number = |level| override_body("1", level)["channel_overrides"]["1"]["message_notifications"].clone();
        assert_eq!(number(NotifyLevel::All), json!(0));
        assert_eq!(number(NotifyLevel::Mentions), json!(1));
        assert_eq!(number(NotifyLevel::Default), json!(3));
        // A mute leaves the number where it was.
        assert_eq!(number(NotifyLevel::Mute), json!(null));
        // And never leaves a mute behind on the way to something else.
        assert_eq!(override_body("1", NotifyLevel::All)["channel_overrides"]["1"]["muted"], json!(false));
    }

    /// What the menu ticks is the channel's own setting; what announces also
    /// hears the server.
    #[test]
    fn a_channel_is_read_as_set_and_as_it_comes_to() {
        let entry = json!({
            "guild_id": "9", "message_notifications": 1,
            "channel_overrides": [
                { "channel_id": "a", "message_notifications": 0 },
                { "channel_id": "b", "message_notifications": 1 },
                { "channel_id": "c", "muted": true },
                { "channel_id": "d", "message_notifications": 3 },
                { "channel_id": "e", "message_notifications": 2 }
            ]
        });
        let (_, mute) = parse_entry(&entry, NOW);
        assert_eq!(mute.levels("a"), (NotifyLevel::All, NotifyLevel::All));
        assert_eq!(mute.levels("b"), (NotifyLevel::Mentions, NotifyLevel::Mentions));
        assert_eq!(mute.levels("c"), (NotifyLevel::Mute, NotifyLevel::Mute));
        assert_eq!(mute.levels("d"), (NotifyLevel::Default, NotifyLevel::Default), "inherits, and the server only says mentions");
        assert_eq!(mute.levels("e"), (NotifyLevel::Mute, NotifyLevel::Mute), "told of nothing");
        assert_eq!(mute.levels("never-set"), (NotifyLevel::Default, NotifyLevel::Default));
    }

    /// A server that says everything is how Discord stores a server nobody
    /// changed, so it must not turn a quiet client loud everywhere.
    #[test]
    fn a_server_set_to_everything_does_not_make_its_channels_loud() {
        let (_, mute) = parse_entry(&json!({ "guild_id": "9", "message_notifications": 0 }), NOW);
        assert_eq!(mute.levels("any"), (NotifyLevel::Default, NotifyLevel::Default));
    }

    /// A channel left on its default inside a server that is muted, or told
    /// of nothing, is silent - and says so, because the row it is drawn on
    /// cannot show it as an ordinary channel.
    #[test]
    fn a_server_that_is_quiet_quietens_a_channel_that_has_no_say() {
        let (_, muted) = parse_entry(&json!({ "guild_id": "9", "muted": true }), NOW);
        assert_eq!(muted.levels("any"), (NotifyLevel::Default, NotifyLevel::Mute));
        let (_, nothing) = parse_entry(&json!({ "guild_id": "9", "message_notifications": 2 }), NOW);
        assert_eq!(nothing.levels("any"), (NotifyLevel::Default, NotifyLevel::Mute));
        // A channel that asked for mentions in a server told of nothing is
        // still the channel's own say.
        let (_, spoken) = parse_entry(&json!({ "guild_id": "9", "message_notifications": 2, "channel_overrides": [{ "channel_id": "a", "message_notifications": 1 }] }), NOW);
        assert_eq!(spoken.levels("a"), (NotifyLevel::Mentions, NotifyLevel::Mentions));
    }

    /// A muted guild and, inside it, one channel muted on its own. Both are
    /// read; neither is inferred from the other.
    #[test]
    fn a_guild_and_a_channel_are_both_read() {
        let entry = json!({
            "guild_id": "9",
            "muted": true,
            "channel_overrides": [
                { "channel_id": "100", "muted": true },
                { "channel_id": "101", "muted": false }
            ]
        });
        let (guild, mute) = parse_entry(&entry, NOW);
        assert_eq!(guild, "9");
        assert!(mute.muted);
        assert!(mute.channels["100"].muted);
        assert!(!mute.channels["101"].muted);
    }

    /// Direct messages arrive with a null guild id, which is a scope of its
    /// own rather than a malformed entry.
    #[test]
    fn direct_messages_are_their_own_scope() {
        let entry = json!({
            "guild_id": null,
            "muted": false,
            "channel_overrides": [{ "channel_id": "55", "muted": true }]
        });
        let (guild, mute) = parse_entry(&entry, NOW);
        assert_eq!(guild, DM_SCOPE);
        assert!(!mute.muted);
        assert!(mute.channels["55"].muted);
    }

    /// "Mute for 8 hours" leaves the flag set after the eight hours are up -
    /// Discord expects the client to look at the clock. One that did not
    /// would silence a server permanently because somebody once silenced it
    /// for an evening.
    #[test]
    fn a_mute_that_has_run_out_is_not_a_mute() {
        let expired = json!({
            "guild_id": "9",
            "muted": true,
            "mute_config": { "end_time": "2020-01-01T00:00:00+00:00" }
        });
        assert!(!parse_entry(&expired, NOW).1.muted);

        let running = json!({
            "guild_id": "9",
            "muted": true,
            "mute_config": { "end_time": "2099-01-01T00:00:00+00:00" }
        });
        assert!(parse_entry(&running, NOW).1.muted);

        // No end time at all is Discord's ordinary indefinite mute.
        let forever = json!({ "guild_id": "9", "muted": true });
        assert!(parse_entry(&forever, NOW).1.muted);
    }

    /// A timed mute ends at its end time, to the second: at that moment it is
    /// over, and a second before it is not.
    #[test]
    fn a_mute_is_over_at_its_end_time() {
        let ends = |at: i64| {
            let end = chrono::DateTime::from_timestamp(at, 0).unwrap().to_rfc3339();
            json!({ "muted": true, "mute_config": { "end_time": end } })
        };
        assert!(still_muted(&ends(NOW + 1), NOW), "a second to go");
        assert!(!still_muted(&ends(NOW), NOW), "its moment");
        assert!(!still_muted(&ends(NOW - 1), NOW), "past");
        // An end time nobody can read is not a reason to say it is over.
        assert!(still_muted(&json!({ "muted": true, "mute_config": { "end_time": "someday" } }), NOW));
        assert!(!still_muted(&json!({ "muted": false, "mute_config": null }), NOW), "never muted");
    }

    /// Settings in the three shapes, and an entry that is not an object at all
    /// is skipped rather than read as an empty one.
    #[test]
    fn what_is_read_is_kept_and_what_is_not_an_entry_is_skipped() {
        let state = crate::backend::kick::testkit::simulated_daemon("discord-mutes");
        let acct = "discord:me";
        note_settings(&state, acct, &json!([
            { "guild_id": "9", "muted": true },
            "not an entry",
            42,
            { "guild_id": null, "muted": false, "channel_overrides": [{ "channel_id": "5", "muted": true }] }
        ]));
        let silent = |guild: &str, channel: &str| state.runtime.discord_notify(acct, guild, channel).1 == NotifyLevel::Mute;
        assert!(silent("9", "anything"), "a muted guild mutes what is in it");
        assert!(silent(DM_SCOPE, "5"), "a muted direct message");
        assert!(!silent(DM_SCOPE, "6"));
        // One entry on its own, as an update sends it, and the wrapped form.
        note_settings(&state, acct, &json!({ "guild_id": "9", "muted": false }));
        assert!(!silent("9", "anything"));
        note_settings(&state, acct, &json!({ "entries": [{ "guild_id": "9", "muted": true }] }));
        assert!(silent("9", "anything"));
    }
}
