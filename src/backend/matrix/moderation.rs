//! Room moderation: power-level permission checks, plus kick/ban/unban/
//! mute themselves. Redacting *others'* messages needs no new function
//! here at all - mod.rs's existing delete_message already redacts
//! whatever event_id it's given with no ownership check of its own (the
//! homeserver is what actually enforces who's allowed to redact what);
//! this module's own job is just telling the frontend when to *offer*
//! that (and kick/ban/mute) in the first place.
//!
//! Power levels are handled as raw `serde_json::Value`, matching this
//! backend's existing convention (see crypto.rs's own module doc comment
//! on why) - not worth a dedicated struct for a handful of int lookups.
//!
//! Every permission check here is advisory/UI-gating only: the
//! homeserver is the real authority and will reject an unauthorized
//! action regardless of what's cached locally in Runtime (see
//! roomstate.rs) - a stale or never-populated power_levels cache just
//! means the UI might offer (or hide) an action it shouldn't, not a
//! security boundary.

use super::http;
use crate::state::AppState;
use anyhow::{Context, Result};
use serde_json::Value;

fn threshold(pl: &Value, key: &str, default: i64) -> i64 {
    pl[key].as_i64().unwrap_or(default)
}

/// What the power levels say about somebody, taken literally.
///
/// Not the whole answer since room version 12 - see `effective_power`.
pub fn user_power_level(pl: &Value, user_id: &str) -> i64 {
    pl["users"][user_id].as_i64().unwrap_or_else(|| threshold(pl, "users_default", 0))
}

/// Whether this room's version gives its creators power of their own.
///
/// Room version 12 moved the creator out of the power levels entirely: they
/// are named in the create event, they outrank every number, and they cannot
/// be demoted - so `users` is empty in a room whose owner has full control,
/// which reads as "nobody has any power" to anything that only looks there.
pub fn creators_outrank_everybody(room_version: &str) -> bool {
    room_version.trim().parse::<u32>().map(|v| v >= 12).unwrap_or(false)
}

/// The rank a creator holds: above every number a room can name.
///
/// `i64::MAX` rather than 101 or 150, because the point of it is that no
/// power level can be set high enough to match it.
pub const CREATOR_POWER: i64 = i64::MAX;

/// What somebody can actually do here, creators included.
///
/// The power levels are the whole story in a room of version 11 or earlier,
/// where a creator is simply whoever was given 100 at the start and can be
/// demoted like anybody else. From version 12 the creator is named in the
/// create event instead, and outranks everything - so a room created by this
/// account with an empty `users` map is a room this account owns, not one
/// where it has no power at all.
pub fn effective_power(pl: &Value, user_id: &str, creators: &[String], room_version: &str) -> i64 {
    if creators_outrank_everybody(room_version) && creators.iter().any(|c| c == user_id) {
        return CREATOR_POWER;
    }
    user_power_level(pl, user_id)
}

/// Who a room's creators are, out of its `m.room.create` event.
///
/// The sender made it; `additional_creators` is version 12's way of saying
/// somebody else owns it equally. Both outrank everybody.
/// Reads either shape a create event arrives in.
///
/// A sync carries the whole event - sender, content and all - while asking
/// the server for one piece of state hands back the *content* alone. The
/// creator is the sender, which the second shape does not have, so both are
/// looked at and neither is assumed.
pub fn creators_of(create_event: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |id: Option<&str>| {
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            if !out.iter().any(|c| c == id) {
                out.push(id.to_string());
            }
        }
    };
    add(create_event["sender"].as_str());
    // Rooms before version 11 named the creator in the content; from 11 the
    // sender is it, and from 12 there may be more than one.
    add(create_event["content"]["creator"].as_str());
    add(create_event["creator"].as_str());
    for extra in create_event["content"]["additional_creators"]
        .as_array()
        .or_else(|| create_event["additional_creators"].as_array())
        .into_iter()
        .flatten()
    {
        add(extra.as_str());
    }
    out
}

fn message_send_threshold(pl: &Value) -> i64 {
    pl["events"]["m.room.message"].as_i64().unwrap_or_else(|| threshold(pl, "events_default", 0))
}

fn power_levels_change_threshold(pl: &Value) -> i64 {
    pl["events"]["m.room.power_levels"].as_i64().unwrap_or_else(|| threshold(pl, "state_default", 50))
}

/// `{canRedactOthers, canKick, canBan, canMute}` for `own_user_id` in a
/// room, given its current cached power levels (`{}` - every default -
/// for a room where no one's ever customized permissions, or where
/// nothing's been cached yet; both parse through the same defaults above
/// without error). Room-level only, not per-target: the actual spec rules
/// for kick/ban/mute also require the acting user's power to exceed the
/// *target's*, which this can't check without knowing who - the mutation
/// itself still enforces that server-side regardless, this is only ever
/// used to decide whether to show the option at all.
pub fn permissions_json(pl: &Value, own_user_id: &str, creators: &[String], room_version: &str) -> Value {
    let own_level = effective_power(pl, own_user_id, creators, room_version);
    serde_json::json!({
        "canRedactOthers": own_level >= threshold(pl, "redact", 50),
        "canKick": own_level >= threshold(pl, "kick", 50),
        "canBan": own_level >= threshold(pl, "ban", 50),
        "canMute": own_level >= power_levels_change_threshold(pl),
    })
}

/// This account's own cached permissions in a buffer's room - `{}` (every
/// default) if nothing's been cached yet, same reasoning as
/// permissions_json's own doc comment.
pub fn permissions_for_buffer(state: &AppState, account_id: &str, buffer_id: &str) -> Result<Value> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("no such Matrix account")?;
    let pl = state.runtime.get_matrix_power_levels(account_id, &room_id).unwrap_or_else(|| serde_json::json!({}));
    let creators = state.runtime.matrix_room_creators(account_id, &room_id);
    let version = state.runtime.matrix_room_version(account_id, &room_id).unwrap_or_default();
    Ok(permissions_json(&pl, &account.user_id, &creators, &version))
}

async fn homeserver_token_room(state: &AppState, account_id: &str, buffer_id: &str) -> Result<(String, String, String)> {
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no known room id for this buffer")?;
    let account = state.accounts.get_matrix(account_id).context("no such Matrix account")?;
    Ok((account.homeserver_url, account.access_token, room_id))
}

pub async fn kick_member(state: &AppState, account_id: &str, buffer_id: &str, target_user_id: &str, reason: Option<&str>) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/rooms/{}/kick", url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>());
    let mut body = serde_json::json!({ "user_id": target_user_id });
    if let Some(reason) = reason {
        body["reason"] = Value::String(reason.to_string());
    }
    http::post_json(&url, Some(&access_token), body).await.context("kicking member")?;
    Ok(())
}

/// Asks somebody into a room.
///
/// Lives here beside kick and ban because it is the same shape of call and
/// the same authority decides it - a room's power levels say who may invite
/// exactly as they say who may remove. It is the opposite action, which is
/// why its absence was odd: a room made here could gain no second member
/// from here.
pub async fn invite_member(state: &AppState, account_id: &str, buffer_id: &str, target_user_id: &str) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/rooms/{}/invite", url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>());
    http::post_json(&url, Some(&access_token), serde_json::json!({ "user_id": target_user_id }))
        .await
        .context("inviting member")?;
    Ok(())
}

pub async fn ban_member(state: &AppState, account_id: &str, buffer_id: &str, target_user_id: &str, reason: Option<&str>) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/rooms/{}/ban", url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>());
    let mut body = serde_json::json!({ "user_id": target_user_id });
    if let Some(reason) = reason {
        body["reason"] = Value::String(reason.to_string());
    }
    http::post_json(&url, Some(&access_token), body).await.context("banning member")?;
    Ok(())
}

pub async fn unban_member(state: &AppState, account_id: &str, buffer_id: &str, target_user_id: &str) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/rooms/{}/unban", url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>());
    http::post_json(&url, Some(&access_token), serde_json::json!({ "user_id": target_user_id })).await.context("unbanning member")?;
    Ok(())
}

/// Sets a member's power level outright - promoting or demoting them.
///
/// The same read-modify-write as `mute_member` and for the same reason: a
/// power-levels event is always a full replace, so PUTting only the one user
/// would silently wipe every other rank and threshold the room had.
///
/// Refusing to set a level above our own is this client's own rule, not the
/// server's - the server refuses it too, but with an error about event
/// authorisation rather than about what was actually attempted. Somebody
/// making another person an admin equal to themselves is allowed and is
/// deliberately not blocked; it is a real thing people do.
pub async fn set_power_level(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    target_user_id: &str,
    level: i64,
) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let encoded_room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let state_url = format!("{base}/_matrix/client/v3/rooms/{encoded_room}/state/m.room.power_levels/");

    let mut pl = http::get_json(&state_url, &access_token).await.context("fetching current power levels")?;
    // The account id is "matrix:@user:server"; the power levels are keyed by
    // the Matrix id itself.
    let own_mxid = account_id.strip_prefix("matrix:").unwrap_or(account_id);
    // Asked of the room rather than of the cache, and including who made it:
    // from room version 12 a creator holds no entry in `users` at all, so
    // reading power levels alone says the room's own owner has none - which
    // refused, here, the very changes the server was happy to accept.
    //
    // From the sync's own copy first, because that is the whole event and
    // carries the sender - the one field that says who made a version 12
    // room, and the one field asking the server for a single piece of state
    // does not return.
    let cached_creators = state.runtime.matrix_room_creators(account_id, &room_id);
    let cached_version = state.runtime.matrix_room_version(account_id, &room_id);
    let (creators, version) = match (cached_creators.is_empty(), cached_version) {
        (false, Some(version)) => (cached_creators, version),
        _ => {
            let create_url = format!("{base}/_matrix/client/v3/rooms/{encoded_room}/state/m.room.create/");
            let create = http::get_json(&create_url, &access_token).await.unwrap_or(Value::Null);
            let version = create["room_version"]
                .as_str()
                .or_else(|| create["content"]["room_version"].as_str())
                .unwrap_or_default()
                .to_string();
            (creators_of(&create), version)
        }
    };
    let own = effective_power(&pl, own_mxid, &creators, &version);
    if level > own {
        anyhow::bail!("you cannot give somebody a rank above your own ({own})");
    }
    pl["users"][target_user_id] = Value::from(level);
    http::put_json(&state_url, &access_token, pl).await.context("setting power level")?;
    Ok(())
}

/// Lowers a member's power level to one below whatever's needed to send
/// `m.room.message` in this room, without removing them from it - "mute"
/// as a power-level floor rather than a membership change. Read-modify-
/// write of the full `m.room.power_levels` state event, since Matrix
/// state events are always a full replace, never a partial patch - a
/// naive "just PUT {users: {target: level}}" would silently wipe every
/// other customization the room's power levels already had.
pub async fn mute_member(state: &AppState, account_id: &str, buffer_id: &str, target_user_id: &str) -> Result<()> {
    let (homeserver_url, access_token, room_id) = homeserver_token_room(state, account_id, buffer_id).await?;
    let base = homeserver_url.trim_end_matches('/');
    let encoded_room = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let state_url = format!("{base}/_matrix/client/v3/rooms/{encoded_room}/state/m.room.power_levels/");

    let mut pl = http::get_json(&state_url, &access_token).await.context("fetching current power levels")?;
    let muted_level = message_send_threshold(&pl) - 1;
    pl["users"][target_user_id] = serde_json::json!(muted_level);

    http::put_json(&state_url, &access_token, pl).await.context("updating power levels")?;
    Ok(())
}

#[cfg(test)]
mod creator_tests {
    use super::*;
    use serde_json::json;

    fn create(version: &str, sender: &str, extra: Vec<&str>) -> Value {
        json!({
            "type": "m.room.create",
            "sender": sender,
            "content": {
                "room_version": version,
                "additional_creators": extra,
            }
        })
    }

    #[test]
    fn a_version_12_room_has_an_owner_the_power_levels_never_mention() {
        let create = create("12", "@me:example.org", vec![]);
        let creators = creators_of(&create);
        // Exactly what a freshly made v12 room looks like: nobody in `users`.
        let levels = json!({ "users": {}, "users_default": 0, "state_default": 50 });
        assert_eq!(effective_power(&levels, "@me:example.org", &creators, "12"), CREATOR_POWER);
        // And it is above anything a room could name.
        assert!(effective_power(&levels, "@me:example.org", &creators, "12") > 100);
        // Somebody else in that same room is still nobody.
        assert_eq!(effective_power(&levels, "@them:example.org", &creators, "12"), 0);
    }

    #[test]
    fn an_older_room_says_what_it_always_said() {
        let create = create("10", "@me:example.org", vec![]);
        let creators = creators_of(&create);
        // Before version 12 the creator is whoever holds 100, and can be
        // demoted like anybody - so an empty users map really does mean the
        // room has no admin left, and inventing one would be wrong.
        let levels = json!({ "users": {}, "users_default": 0 });
        assert_eq!(effective_power(&levels, "@me:example.org", &creators, "10"), 0);
        let with_admin = json!({ "users": { "@me:example.org": 100 } });
        assert_eq!(effective_power(&with_admin, "@me:example.org", &creators, "10"), 100);
    }

    #[test]
    fn a_room_can_be_owned_by_more_than_one_person() {
        let create = create("12", "@me:example.org", vec!["@partner:example.org"]);
        let creators = creators_of(&create);
        assert_eq!(creators.len(), 2);
        let levels = json!({ "users": {} });
        assert_eq!(effective_power(&levels, "@partner:example.org", &creators, "12"), CREATOR_POWER);
    }

    #[test]
    fn the_permission_gates_follow() {
        let create = create("12", "@me:example.org", vec![]);
        let creators = creators_of(&create);
        let levels = json!({ "users": {}, "state_default": 50, "kick": 50, "ban": 50, "redact": 50 });
        let perms = permissions_json(&levels, "@me:example.org", &creators, "12");
        // The bug this fixes: the owner of a room being offered nothing.
        assert_eq!(perms["canKick"], true);
        assert_eq!(perms["canBan"], true);
        assert_eq!(perms["canMute"], true);
        assert_eq!(perms["canRedactOthers"], true);
        let theirs = permissions_json(&levels, "@them:example.org", &creators, "12");
        assert_eq!(theirs["canKick"], false);
    }

    #[test]
    fn only_versions_that_have_creator_power_get_it() {
        assert!(creators_outrank_everybody("12"));
        assert!(creators_outrank_everybody("13"));
        assert!(!creators_outrank_everybody("11"));
        assert!(!creators_outrank_everybody("10"));
        // An unstable room version is not one this rule is known to hold in.
        assert!(!creators_outrank_everybody("org.matrix.msc3757.12"));
    }
}
