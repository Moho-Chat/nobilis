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

/// One slot of the window, member or not.
///
/// The headers matter even though nothing is drawn for them. Discord's
/// incremental ops address the list *it* holds, and that list interleaves
/// group headers - "Online", "Offline", every role with its own section -
/// among the people. Dropping them on SYNC and then applying an INSERT at
/// Discord's index puts the person in somebody else's place, and the DELETE
/// that was meant to remove their old row removes a different one instead:
/// the person is now listed twice, and the drift grows with every status
/// change. So a header takes up its slot here and is left out at the end.
fn window_entry(runtime: &crate::runtime::Runtime, account_id: &str, guild_id: &str, item: &Value) -> Option<Value> {
    if let Some(group) = item.get("group") {
        return Some(json!({ "group": group["id"].as_str().unwrap_or("") }));
    }
    member_entry(runtime, account_id, guild_id, item)
}

/// Whether an entry is a header rather than somebody.
fn is_group(entry: &Value) -> bool {
    entry.get("group").is_some()
}

/// What one op did to the window.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum Applied {
    /// The window now says something different.
    Changed,
    /// Nothing to do, and nothing wrong.
    Nothing,
    /// The op could not be applied where Discord said it went, so the window
    /// no longer lines up with Discord's. Anything applied to it after this
    /// lands in the wrong place, which is how a roster grows duplicates -
    /// the only safe answer is to throw it away and ask for it again.
    Stale,
}

/// Applies one of Discord's lazy member-list operations.
///
/// INSERT, UPDATE and DELETE address the window by index, which is why the
/// list they are applied to is held in Discord's order - headers and all -
/// rather than the sorted one that gets displayed.
pub(super) fn apply_member_op(runtime: &crate::runtime::Runtime, account_id: &str, guild_id: &str, window: &mut Vec<Value>, op: &Value) -> Applied {
    let index = |op: &Value| op["index"].as_u64().map(|i| i as usize);
    match op["op"].as_str().unwrap_or("") {
        "SYNC" => {
            let items: Vec<Value> = op["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| window_entry(runtime, account_id, guild_id, item))
                .collect();
            // The range is the slice of the window this SYNC describes.
            // Only ever [0, 99] is asked for, so this replaces the lot -
            // but honouring the range keeps it right if that ever changes.
            let start = op["range"][0].as_u64().unwrap_or(0) as usize;
            if start == 0 {
                *window = items;
            } else if start <= window.len() {
                window.truncate(start);
                window.extend(items);
            } else {
                // A range beginning past the end of what is held would leave
                // a hole, and a hole is an index everything below is wrong by.
                return Applied::Stale;
            }
            Applied::Changed
        }
        "INSERT" => match (index(op), window_entry(runtime, account_id, guild_id, &op["item"])) {
            (Some(i), Some(entry)) if i <= window.len() => {
                let drawn = !is_group(&entry);
                window.insert(i, entry);
                // A header takes its slot but changes nothing anybody sees.
                if drawn { Applied::Changed } else { Applied::Nothing }
            }
            _ => Applied::Stale,
        },
        "UPDATE" => match (index(op), window_entry(runtime, account_id, guild_id, &op["item"])) {
            (Some(i), Some(entry)) if i < window.len() => {
                let drawn = !is_group(&entry) || !is_group(&window[i]);
                window[i] = entry;
                if drawn { Applied::Changed } else { Applied::Nothing }
            }
            _ => Applied::Stale,
        },
        "DELETE" => match index(op) {
            Some(i) if i < window.len() => {
                let drawn = !is_group(&window[i]);
                window.remove(i);
                if drawn { Applied::Changed } else { Applied::Nothing }
            }
            _ => Applied::Stale,
        },
        // INVALIDATE says a range is no longer being kept up to date, so what
        // is held for it is already behind.
        "INVALIDATE" => Applied::Stale,
        _ => Applied::Nothing,
    }
}

/// The window as a roster: headers dropped, nobody listed twice, sorted by
/// name.
///
/// The de-duplication is a belt as well as braces. Keeping Discord's headers
/// is what stops the indices drifting in the first place, but this is the
/// thing the bug was actually reported as - one person appearing over and
/// over in a channel left open all day - and a roster that cannot show
/// anybody twice cannot regress to it however the window is reached.
pub(super) fn roster(window: &[Value]) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut members: Vec<Value> = window
        .iter()
        .filter(|e| !is_group(e))
        .filter(|e| seen.insert(e["userId"].as_str().unwrap_or("").to_string()))
        .cloned()
        .collect();
    members.sort_by(|a, b| {
        let (an, bn) = (a["nick"].as_str().unwrap_or(""), b["nick"].as_str().unwrap_or(""));
        an.to_lowercase().cmp(&bn.to_lowercase()).then_with(|| an.cmp(bn))
    });
    members
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
        match apply_member_op(&state.runtime, &account_id, guild_id, &mut window, op) {
            Applied::Changed => changed = true,
            Applied::Nothing => {}
            // Everything after this op would be applied at the wrong index,
            // so the rest of the batch is abandoned along with the window
            // and a fresh SYNC is asked for. What is on screen is left alone
            // meanwhile: a stale roster reads better than an emptied one.
            Applied::Stale => {
                tracing::debug!("discord: member window for {buffer_id} is out of step; resyncing");
                state.runtime.set_discord_member_window(buffer_id, Vec::new());
                request_member_list(state, buffer_id);
                return;
            }
        }
    }

    if !changed {
        return;
    }
    state.runtime.set_discord_member_window(buffer_id, window.clone());

    let member_list = json!(roster(&window));
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

    fn member(id: &str, nick: &str, status: &str) -> Value {
        json!({ "member": { "nick": nick, "user": { "id": id, "username": nick }, "presence": { "status": status } } })
    }

    fn group(id: &str) -> Value {
        json!({ "group": { "id": id, "count": 1 } })
    }

    fn names(window: &[Value]) -> Vec<String> {
        roster(window).iter().map(|m| m["nick"].as_str().unwrap_or("").to_string()).collect()
    }

    /// Only SYNC was ever applied, so a roster froze the moment a channel
    /// was opened. The index-addressed ops are why the window is held in
    /// Discord's order rather than the sorted one that gets shown.
    #[test]
    fn a_member_window_follows_inserts_updates_and_deletes() {
        let runtime = crate::runtime::Runtime::new();
        let mut window: Vec<Value> = Vec::new();

        let sync = json!({ "op": "SYNC", "range": [0, 99], "items": [member("1", "anna", "online"), member("2", "bob", "idle")] });
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &sync), Applied::Changed);
        assert_eq!(window.len(), 2);
        assert_eq!(window[0]["nick"], "anna");

        let insert = json!({ "op": "INSERT", "index": 1, "item": member("3", "carol", "online") });
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &insert), Applied::Changed);
        assert_eq!(window.iter().map(|m| m["nick"].as_str().unwrap()).collect::<Vec<_>>(), ["anna", "carol", "bob"]);

        let update = json!({ "op": "UPDATE", "index": 0, "item": member("1", "anna", "offline") });
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &update), Applied::Changed);
        assert_eq!(window[0]["away"], true);

        let delete = json!({ "op": "DELETE", "index": 1 });
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &delete), Applied::Changed);
        assert_eq!(window.iter().map(|m| m["nick"].as_str().unwrap()).collect::<Vec<_>>(), ["anna", "bob"]);
    }

    /// The reported bug, in the shape it actually arrives in.
    ///
    /// Discord's list interleaves group headers among the people and its ops
    /// count them. Somebody's status changing is a DELETE from one section
    /// and an INSERT into another - so with the headers thrown away, every
    /// index is short by the number of headers above it: the DELETE takes
    /// out whoever happens to sit there instead, and the INSERT puts a
    /// second copy of the person who moved into a list that still holds
    /// their old row. Left open for a day, a channel accumulates one
    /// duplicate per status change, which is what the screenshot shows.
    #[test]
    fn moving_between_groups_does_not_duplicate_anybody() {
        let runtime = crate::runtime::Runtime::new();
        let mut window: Vec<Value> = Vec::new();

        // Discord's own indices: 0 header, 1 anna, 2 bob, 3 header, then
        // carol, dave, erin, finn, gail at 4..8.
        let sync = json!({ "op": "SYNC", "range": [0, 99], "items": [
            group("online"),
            member("1", "anna", "online"),
            member("2", "bob", "online"),
            group("offline"),
            member("3", "carol", "offline"),
            member("4", "dave", "offline"),
            member("5", "erin", "offline"),
            member("6", "finn", "offline"),
            member("7", "gail", "offline"),
        ]});
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &sync), Applied::Changed);
        assert_eq!(names(&window), ["anna", "bob", "carol", "dave", "erin", "finn", "gail"]);

        // finn comes online: out of the offline section at 7, into the
        // online one at 3. Both indices are well inside the list, so with
        // the headers missing these would apply - to the wrong people.
        for op in [
            json!({ "op": "DELETE", "index": 7 }),
            json!({ "op": "INSERT", "index": 3, "item": member("6", "finn", "online") }),
        ] {
            assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &op), Applied::Changed);
        }

        assert_eq!(
            names(&window),
            ["anna", "bob", "carol", "dave", "erin", "finn", "gail"],
            "finn moved; nobody was lost and nobody was listed twice"
        );
        let finn = roster(&window).into_iter().find(|m| m["userId"] == "6").expect("finn");
        assert_eq!(finn["status"], "online");
        assert_eq!(finn["away"], false);
    }

    /// A header is a real slot but nothing anybody sees, so it holds the
    /// indices straight without emitting a roster change of its own.
    #[test]
    fn a_header_takes_a_slot_and_shows_nothing() {
        let runtime = crate::runtime::Runtime::new();
        let mut window: Vec<Value> = Vec::new();

        let header = json!({ "op": "INSERT", "index": 0, "item": group("online") });
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &header), Applied::Nothing);
        assert_eq!(window.len(), 1);
        assert!(roster(&window).is_empty());

        let person = json!({ "op": "INSERT", "index": 1, "item": member("1", "anna", "online") });
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &person), Applied::Changed);
        assert_eq!(names(&window), ["anna"]);
    }

    /// An op that cannot be applied where Discord said it goes means the
    /// window has drifted. Carrying on would put people in each other's
    /// places; the answer is to say so, and let the caller ask again.
    #[test]
    fn an_op_that_does_not_fit_asks_for_a_resync() {
        let runtime = crate::runtime::Runtime::new();
        let mut window: Vec<Value> = Vec::new();

        let past_end = json!({ "op": "DELETE", "index": 7 });
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &past_end), Applied::Stale);

        let invalidate = json!({ "op": "INVALIDATE", "range": [0, 99] });
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &invalidate), Applied::Stale);

        // An item that is neither a person nor a header: nothing can be put
        // in its slot, so the slot cannot be kept straight either.
        let nonsense = json!({ "op": "INSERT", "index": 0, "item": { "thing": 1 } });
        assert_eq!(apply_member_op(&runtime, "discord:me", "guild", &mut window, &nonsense), Applied::Stale);
    }

    /// Whatever the window holds, nobody is listed twice. This is the
    /// symptom the ticket describes, and it is worth being unable to produce
    /// rather than merely fixed upstream.
    #[test]
    fn a_roster_never_shows_the_same_person_twice() {
        let runtime = crate::runtime::Runtime::new();
        let mut window: Vec<Value> = Vec::new();
        let sync = json!({ "op": "SYNC", "range": [0, 99], "items": [
            member("1", "anna", "online"),
            member("2", "bob", "online"),
            member("1", "anna", "offline"),
        ]});
        apply_member_op(&runtime, "discord:me", "guild", &mut window, &sync);
        assert_eq!(names(&window), ["anna", "bob"]);
    }
}
