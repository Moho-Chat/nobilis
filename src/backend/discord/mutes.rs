//! What this account has silenced on Discord itself.
//!
//! Muting a server or a channel in the official client is an account setting,
//! not a preference in one window: it arrives in READY as
//! `user_guild_settings` and every device signed in to the account honours
//! it. Nothing here read that, so a guild somebody deliberately silenced
//! months ago was as loud in moho as anything else - and the busiest servers
//! are exactly the ones people mute.
//!
//! Reading it, not writing it. Being *told* is the half that matters and the
//! half that can go wrong quietly; setting it from here is a separate thing
//! and can follow.
//!
//! Applying is deliberately its own step. The settings arrive in READY, and
//! the channels they name arrive afterwards in GUILD_CREATE - so a mute
//! applied at the moment it is read would be applied to buffers that do not
//! exist yet, which is why what is read is kept and applied again whenever
//! there is something new to apply it to.

use super::*;
use crate::runtime::DiscordMute;

/// Direct messages have no guild, and Discord marks their settings entry with
/// a null guild id. `@me` is what Discord's own API calls that scope.
pub(super) const DM_SCOPE: &str = "@me";

/// Reads one `user_guild_settings` entry.
///
/// Returns the guild it is about along with what it says. A `mute_config`
/// carries an end time for a mute somebody set to expire - "mute for 8 hours"
/// - so a mute whose moment has passed is read as no mute at all rather than
/// as a permanent one.
pub(super) fn parse_entry(entry: &Value, now: i64) -> (String, DiscordMute) {
    let guild = entry["guild_id"].as_str().unwrap_or(DM_SCOPE).to_string();
    let mut channels = HashMap::new();
    for over in entry["channel_overrides"].as_array().into_iter().flatten() {
        let Some(channel_id) = over["channel_id"].as_str() else { continue };
        channels.insert(channel_id.to_string(), still_muted(over, now));
    }
    (guild, DiscordMute { muted: still_muted(entry, now), channels })
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

/// Puts what is known onto the buffers that exist.
///
/// Cheap enough to run whenever anything might have changed - one pass over
/// this account's conversations - and `set_silenced` only tells the client
/// about the ones that actually moved.
pub(super) fn apply(state: &AppState, account_id: &str) {
    let prefix = format!("{account_id}|");
    for buffer in state.runtime.list_buffers() {
        if !buffer.id.starts_with(&prefix) {
            continue;
        }
        let Some(channel_id) = state.runtime.get_discord_channel(&buffer.id) else { continue };
        let guild = state.runtime.get_discord_guild(&buffer.id).unwrap_or_else(|| DM_SCOPE.to_string());
        let muted = state.runtime.discord_muted(account_id, &guild, &channel_id);
        // A local mute is still a local mute. This only ever says what
        // Discord says, so a channel muted here and not there goes back to
        // being quiet only because this window says so - which is the
        // distinction the buffer carries and a client draws.
        if muted != buffer.server_muted {
            state.runtime.set_silenced(state, &buffer.id, muted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

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
        assert_eq!(mute.channels.get("100"), Some(&true));
        assert_eq!(mute.channels.get("101"), Some(&false));
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
        assert_eq!(mute.channels.get("55"), Some(&true));
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
}
