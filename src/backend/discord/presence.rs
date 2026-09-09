//! Who is here, and what they are shown as.
//!
//! Two different things that both answer that question: this account's own
//! status, pushed onto the gateway connection, and everybody else's, which
//! arrives as a member-list window addressed by index rather than by name.

use super::*;

/// Discord's presence shape for one of our statuses.
///
/// Discord's own vocabulary matches ours for both values, so they pass
/// through unchanged - the mapping only exists so an unrecognised value
/// cannot put the account into some unintended state.
/// The four Discord actually has, from whatever it was asked for.
///
/// Invisible is the one people want from a client that is not Discord's own:
/// it is how you read a server without being counted as present. It is a real
/// status to Discord rather than a disconnection - the account stays
/// connected and keeps receiving everything, and only the presence others see
/// says offline.
pub(super) fn discord_status(status: &str) -> &'static str {
    match status {
        "idle" => "idle",
        "dnd" | "do_not_disturb" | "busy" => "dnd",
        "invisible" | "offline" => "invisible",
        _ => "online",
    }
}

pub(super) fn presence_payload(status: &str) -> serde_json::Value {
    json!({
        "status": discord_status(status),
        "since": 0,
        "activities": [],
        // Only idle means away. Do-not-disturb is somebody who is here and
        // does not want to be interrupted, and invisible is somebody who is
        // here and would rather nobody knew.
        "afk": status == "idle",
    })
}

/// Sets an account's status.
///
/// Two places have to agree. The gateway opcode changes how this *session*
/// presents right now, which is what other people see immediately; the account
/// setting is what Discord treats as the user's chosen status, and is what
/// every new session starts from. Setting only the opcode leaves the account
/// still holding its old choice - which is how an account left on "invisible"
/// keeps reverting - and setting only the account is slow to show.
pub async fn apply_status(state: &AppState, account_id: &str, status: &str) -> bool {
    if let Some(sender) = state.runtime.discord_gateway_sender(account_id) {
        // Opcode 3 is presence update.
        let _ = sender.send(json!({ "op": 3, "d": presence_payload(status) }).to_string());
    }

    let Some(config) = state.accounts.get_discord(account_id) else { return false };
    match send_write(
        http_client()
        .patch(format!("{API_BASE}/users/@me/settings"))
        .header("Authorization", &config.token)
        .json(&json!({ "status": discord_status(status) }))
        )
    .await
    {
        Ok(resp) if resp.status().is_success() => true,
        Ok(resp) => {
            tracing::debug!("discord: setting status returned HTTP {}", resp.status());
            false
        }
        Err(e) => {
            tracing::debug!("discord: setting status failed: {e}");
            false
        }
    }
}

/// Asks Discord for a channel's member list.
///
/// A user token cannot use REQUEST_GUILD_MEMBERS the way a bot does - the
/// member list is instead a "lazy guild" subscription (opcode 14) naming the
/// channel and the ranges of the list to send, which the server answers with
/// GUILD_MEMBER_LIST_UPDATE dispatches. This is what Discord's own client
/// does when you open a channel, which is also why the roster only exists for
/// channels somebody is actually looking at.
pub fn request_member_list(state: &AppState, buffer_id: &str) -> bool {
    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return false };
    let account_id = buffer.account_id;
    let Some(sender) = state.runtime.discord_gateway_sender(&account_id) else { return false };
    let Some(channel_id) = state.runtime.get_discord_channel(buffer_id) else { return false };
    let Some(guild_id) = state.runtime.get_discord_guild(buffer_id) else {
        // A DM has no guild and no member list to subscribe to; its
        // participants are already known from the channel itself.
        return false;
    };

    // The reply names the guild and a permissions-derived list id, never the
    // channel, so remember which buffer this was for.
    state.runtime.set_discord_member_list_target(&account_id, &guild_id, buffer_id);

    // Ranges are 100-member windows; one covers any channel we would show.
    sender
        .send(
            json!({
                "op": 14,
                "d": {
                    "guild_id": guild_id,
                    "typing": true,
                    "threads": false,
                    "activities": true,
                    "channels": { channel_id: [[0, 99]] }
                }
            })
            .to_string(),
        )
        .is_ok()
}

/// Rebuilds a channel's roster from a GUILD_MEMBER_LIST_UPDATE.
///
/// The list arrives as a series of ops over a windowed view: SYNC carries a
/// whole range of entries, while INSERT/UPDATE/DELETE adjust it as people come
/// and go. Entries are either a group header - a role name, or the online and
/// offline buckets - or a member.
///
/// Only SYNC is acted on. The incremental ops move members between roles and
/// buckets by position within a list this client does not otherwise model, and
/// applying them half-understood would corrupt the roster; re-opening the
/// channel asks for a fresh SYNC, which is what Discord's own client does when
/// its view changes.
/// One entry of Discord's member window, or nothing if the item is a role
/// header rather than a person.
pub(super) fn member_entry(runtime: &crate::runtime::Runtime, account_id: &str, guild_id: &str, item: &Value) -> Option<Value> {
    let member = item.get("member")?;
    let user = &member["user"];
    let user_id = user["id"].as_str()?;
    // Server nickname first, then the account's chosen display name, then
    // the raw username - the same order Discord itself shows.
    let nick = member["nick"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| user["global_name"].as_str().filter(|s| !s.is_empty()))
        .or_else(|| user["username"].as_str())
        .unwrap_or("unknown");
    let status = member["presence"]["status"].as_str().unwrap_or("offline");
    // Names are learned here and remembered for anywhere they are needed.
    // Voice is the case that matters: Discord attaches a member to a voice
    // state only when someone moves, so anyone already sitting in a channel
    // when we connect would otherwise be shown as a raw snowflake forever.
    runtime.remember_discord_name(account_id, user_id, nick);
    // And their membership of this guild, which arrives here and nowhere else
    // a user token can reach: /users/{id}/profile answers "Unknown User" for
    // anybody this account has no relationship with, and /guilds/../members
    // is closed to user tokens entirely. The member window is how Discord's
    // own client knows when somebody joined, so it is how this does too.
    if !guild_id.is_empty() {
        runtime.remember_discord_member(guild_id, user_id, member);
    }
    // The roles they hold here, by name, so a menu offering to take one away
    // knows which they have - the member object carries ids, and an id is not
    // something to put in front of anybody.
    let roles = if guild_id.is_empty() {
        Vec::new()
    } else {
        runtime.discord_role_names(guild_id, member["roles"].as_array().unwrap_or(&Vec::new()))
    };
    Some(json!({
        "nick": nick,
        "userId": user_id,
        "prefix": "",
        // Only actually offline counts as away. Idle and do-not-disturb are
        // still connected - Discord lists them with everyone else who is
        // present, and their own status word says the rest.
        "away": status == "offline",
        "status": status,
        "roles": roles
    }))
}

/// Applies one of Discord's lazy member-list operations.
///
/// Only SYNC was applied before, so a roster was correct at the moment a
/// channel was opened and then stood still: somebody joining, leaving, or
/// going offline changed nothing until the channel was reopened.
///
/// INSERT, UPDATE and DELETE address the window by index, which is why the
/// list they are applied to is held in Discord's order rather than the
/// sorted one that gets displayed.
pub(super) fn apply_member_op(runtime: &crate::runtime::Runtime, account_id: &str, guild_id: &str, window: &mut Vec<Value>, op: &Value) -> bool {
    let index = |op: &Value| op["index"].as_u64().map(|i| i as usize);
    match op["op"].as_str().unwrap_or("") {
        "SYNC" => {
            let items: Vec<Value> = op["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| member_entry(runtime, account_id, guild_id, item))
                .collect();
            // The range is the slice of the window this SYNC describes.
            // Only ever [0, 99] is asked for, so this replaces the lot -
            // but honouring the range keeps it right if that ever changes.
            let start = op["range"][0].as_u64().unwrap_or(0) as usize;
            if start == 0 {
                *window = items;
            } else {
                window.truncate(start);
                window.extend(items);
            }
            true
        }
        "INSERT" => match (index(op), member_entry(runtime, account_id, guild_id, &op["item"])) {
            (Some(i), Some(entry)) if i <= window.len() => {
                window.insert(i, entry);
                true
            }
            // A role header being inserted shifts everyone below it, and
            // there is nothing to show for it - so the window is refreshed
            // by the next SYNC rather than being left subtly misaligned.
            _ => false,
        },
        "UPDATE" => match (index(op), member_entry(runtime, account_id, guild_id, &op["item"])) {
            (Some(i), Some(entry)) if i < window.len() => {
                window[i] = entry;
                true
            }
            _ => false,
        },
        "DELETE" => match index(op) {
            Some(i) if i < window.len() => {
                window.remove(i);
                true
            }
            _ => false,
        },
        // INVALIDATE says a range is stale; the next SYNC replaces it.
        _ => false,
    }
}

pub(super) fn update_member_list(state: &AppState, buffer_id: &str, d: &Value) {
    let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
    let mut window = state.runtime.discord_member_window(buffer_id);
    let mut changed = false;

    // The guild the window belongs to, so each member's own membership of it
    // - when they joined, which roles they hold - can be remembered as it
    // goes past. It is on the dispatch rather than on the entries.
    let guild_id = d["guild_id"].as_str().unwrap_or("");
    for op in d["ops"].as_array().into_iter().flatten() {
        changed |= apply_member_op(&state.runtime, &account_id, guild_id, &mut window, op);
    }

    if !changed {
        return;
    }
    state.runtime.set_discord_member_window(buffer_id, window.clone());

    let mut members = window;
    members.sort_by(|a, b| {
        let (an, bn) = (a["nick"].as_str().unwrap_or(""), b["nick"].as_str().unwrap_or(""));
        an.to_lowercase().cmp(&bn.to_lowercase()).then_with(|| an.cmp(bn))
    });
    let member_list = json!(members);
    state.runtime.set_presence(buffer_id, member_list.clone());
    state.events.emit("presenceChange", json!({ "bufferId": buffer_id, "members": member_list }));
}

/// Updates one person's status wherever they are currently listed.
///
/// PRESENCE_UPDATE arrives for anyone the account can see, which is far more
/// people than are in any open roster - so this only touches buffers that
/// already list them, and says nothing otherwise.
pub(super) fn update_presence_in_rosters(state: &AppState, user_id: &str, status: &str) {
    for buffer in state.runtime.list_buffers() {
        let Some(existing) = state.runtime.get_presence(&buffer.id) else { continue };
        let Some(members) = existing.as_array() else { continue };
        if !members.iter().any(|m| m["userId"].as_str() == Some(user_id)) {
            continue;
        }
        let updated: Vec<Value> = members
            .iter()
            .map(|m| {
                if m["userId"].as_str() != Some(user_id) {
                    return m.clone();
                }
                let mut m = m.clone();
                m["status"] = json!(status);
                // Same rule the initial sync uses; the two disagreeing would
                // move someone between groups on their next presence change.
                m["away"] = json!(status == "offline");
                m
            })
            .collect();
        let member_list = json!(updated);
        state.runtime.set_presence(&buffer.id, member_list.clone());
        state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": member_list }));
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use serde_json::json;

    /// Only SYNC was ever applied, so a roster froze the moment a channel
    /// was opened. The index-addressed ops are why the window is held in
    /// Discord's order rather than the sorted one that gets shown.
    #[test]
    fn a_member_window_follows_inserts_updates_and_deletes() {
        let runtime = crate::runtime::Runtime::new();
        let member = |id: &str, nick: &str, status: &str| {
            json!({ "member": { "nick": nick, "user": { "id": id, "username": nick }, "presence": { "status": status } } })
        };
        let mut window: Vec<serde_json::Value> = Vec::new();

        let sync = json!({ "op": "SYNC", "range": [0, 99], "items": [member("1", "anna", "online"), member("2", "bob", "idle")] });
        assert!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &sync));
        assert_eq!(window.len(), 2);
        assert_eq!(window[0]["nick"], "anna");

        let insert = json!({ "op": "INSERT", "index": 1, "item": member("3", "carol", "online") });
        assert!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &insert));
        assert_eq!(window.iter().map(|m| m["nick"].as_str().unwrap()).collect::<Vec<_>>(), ["anna", "carol", "bob"]);

        let update = json!({ "op": "UPDATE", "index": 0, "item": member("1", "anna", "offline") });
        assert!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &update));
        assert_eq!(window[0]["away"], true);

        let delete = json!({ "op": "DELETE", "index": 1 });
        assert!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &delete));
        assert_eq!(window.iter().map(|m| m["nick"].as_str().unwrap()).collect::<Vec<_>>(), ["anna", "bob"]);
    }

    /// An op that cannot be applied cleanly - a role header with no member,
    /// an index past the end - reports no change rather than corrupting the
    /// window, leaving the next SYNC to put it right.
    #[test]
    fn an_op_that_does_not_fit_changes_nothing() {
        let runtime = crate::runtime::Runtime::new();
        let mut window: Vec<serde_json::Value> = Vec::new();

        let header = json!({ "op": "INSERT", "index": 0, "item": { "group": { "id": "online", "count": 4 } } });
        assert!(!apply_member_op(&runtime, "discord:me", "guild", &mut window, &header));
        assert!(window.is_empty());

        let past_end = json!({ "op": "DELETE", "index": 7 });
        assert!(!apply_member_op(&runtime, "discord:me", "guild", &mut window, &past_end));

        let invalidate = json!({ "op": "INVALIDATE", "range": [0, 99] });
        assert!(!apply_member_op(&runtime, "discord:me", "guild", &mut window, &invalidate));
    }
}
