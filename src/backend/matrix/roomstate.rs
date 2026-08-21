//! Room state tracking beyond naming (see rooms.rs): member avatars, room
//! avatars, power levels, and the joined-member roster - all derived from
//! `m.room.*` state events, processed at the same points rooms.rs's own
//! naming derivation already runs (a room's full state the first time
//! /sync ever mentions it, mid-session state-carrying timeline events, and
//! bootstrap_joined_rooms's own explicit `/state` fetch on a resumed
//! connection - see mod.rs's call sites).
//!
//! Avatars are resolved eagerly (downloaded+cached to a local file://
//! path) as soon as they're seen, same as message media - a plain QML
//! Image has no route to send the Bearer auth header Matrix's media
//! endpoint requires, so a raw mxc:// URI would never load directly. No
//! extension hint is needed here the way message media needs one (see
//! mod.rs's extension_for_mimetype) - avatars are bound directly to an
//! image element's source, which sniffs the real format from file
//! content, not extension; only the client's URL-pattern-based chat
//! media detector needed that trick.
//!
//! emit_matrix_presence builds and broadcasts a room's current userlist
//! (see runtime.rs's presence/get_presence, the same wire mechanism
//! backend/irc.rs's emit_presence already uses) from whatever's currently
//! cached: the joined-member roster, power levels (sort key), and presence
//! (online/offline split) - called any time one of those three changes.

use super::{cached_media_path, moderation, protocol};
use crate::model;
use crate::state::AppState;
use serde_json::Value;

/// Scans a batch of state events for anything this module tracks. Safe to
/// call with the same events more than once - re-resolving an
/// already-cached avatar is a cheap no-op (cached_media_path checks
/// try_exists first), and re-storing identical power_levels/avatar
/// content is harmless.
pub async fn process_state_events(state: &AppState, account_id: &str, room_id: &str, homeserver_url: &str, access_token: &str, events: &[&Value]) {
    for event in events {
        match event["type"].as_str().unwrap_or("") {
            "m.room.power_levels" => {
                state.runtime.set_matrix_power_levels(account_id, room_id, event["content"].clone());
            }
            "m.room.avatar" => {
                if let Some(mxc) = event["content"]["url"].as_str() {
                    if let Some(path) = cached_media_path(homeserver_url, access_token, mxc, "").await {
                        state.runtime.set_matrix_room_avatar(state, account_id, room_id, &path);
                    }
                }
            }
            "m.room.member" => {
                // Avatar cached even for a non-"join" membership content (a
                // leave/ban event still carries the member's last-known
                // profile) - their historical messages should keep showing
                // whatever avatar they had, not suddenly go blank once
                // they're gone. The roster entry itself, unlike the avatar,
                // is removed outright on anything but "join" - a userlist
                // should never show someone who isn't actually in the room.
                let Some(user_id) = event["state_key"].as_str() else { continue };
                if let Some(mxc) = event["content"]["avatar_url"].as_str() {
                    if let Some(path) = cached_media_path(homeserver_url, access_token, mxc, "").await {
                        state.runtime.set_matrix_member_avatar(account_id, user_id, &path);
                    }
                }
                if event["content"]["membership"].as_str() == Some("join") {
                    // Same fallback rooms.rs's own name derivation uses for
                    // a sender with no displayname set - the full mxid, not
                    // a parsed localpart (no such helper exists elsewhere
                    // in this backend, so this doesn't invent one either).
                    let display_name = event["content"]["displayname"].as_str().filter(|s| !s.is_empty()).unwrap_or(user_id);
                    state.runtime.set_matrix_member(account_id, room_id, user_id, display_name);
                } else {
                    state.runtime.remove_matrix_member(account_id, room_id, user_id);
                }
            }
            _ => {}
        }
    }
}

/// Rebuilds and broadcasts a room's current userlist from whatever's
/// presently cached in Runtime (roster + power levels + presence) - see
/// this module's own doc comment for when to call this. A no-op if the
/// room's buffer name isn't known yet (nothing to key the presenceChange
/// event's bufferId on) - callers only ever reach this after the room's
/// name has already been derived, so this only guards against
/// out-of-order calls, not an expected case.
pub fn emit_matrix_presence(state: &AppState, account_id: &str, room_id: &str, own_user_id: &str) {
    let Some((name, _kind)) = state.runtime.get_matrix_room_name(account_id, room_id) else { return };
    let buffer_id = model::buffer_id(account_id, &name);

    let members = state.runtime.get_matrix_room_members(account_id, room_id);
    let power_levels = state.runtime.get_matrix_power_levels(account_id, room_id).unwrap_or_else(|| serde_json::json!({}));

    let member_list: Vec<Value> = members
        .iter()
        .map(|(user_id, display_name)| {
            // Our own session is always "online" from its own point of
            // view - /sync's presence.events never reports the logged-in
            // user's own status back to itself, only other users', so
            // without this override our own entry would otherwise sit
            // permanently unset (treated as offline).
            let online = user_id == own_user_id || state.runtime.is_matrix_user_online(account_id, user_id);
            serde_json::json!({
                "nick": display_name,
                "userId": user_id,
                "prefix": "",
                "away": !online,
                "powerLevel": moderation::user_power_level(&power_levels, user_id),
            })
        })
        .collect();

    let member_list = serde_json::json!(member_list);
    state.runtime.set_presence(&buffer_id, member_list.clone());
    state.events.emit("presenceChange", serde_json::json!({ "bufferId": buffer_id, "members": member_list }));
}

/// Membership-change chat announcements ("X joined the room", "X was
/// kicked by Y", ...). Unlike process_state_events's own roster/avatar
/// bookkeeping above (silent, and safe to call against a room's *entire*
/// historical state dump - see its own doc comment), this only makes
/// sense against genuinely live, just-happened timeline events: call it
/// only from mod.rs's mid_session_state_events pass, never bootstrap_
/// joined_rooms's or process_sync_response's first-seen-room full-state
/// fetch, or opening a room for the first time would replay its entire
/// join/leave history as chat messages. Always recorded regardless of the
/// account's own display settings - same "backend always emits, frontend
/// decides whether to render" convention every other message-kind filter
/// (IRC's join/part/topic/mode toggles) already uses; the client's Matrix
/// settings panel carries the actual on/off toggles.
pub fn announce_membership_changes(state: &AppState, account_id: &str, buffer_name: &str, buffer_kind: &str, events: &[&Value]) {
    for event in events {
        if event["type"].as_str() != Some("m.room.member") {
            continue;
        }
        let Some((kind, body)) = classify_membership_change(event) else { continue };
        state.runtime.record_message(state, account_id, buffer_name, buffer_kind, "*", &body, false, kind, None, None, false, None, Vec::new(), None);
    }
}

/// `(message kind, body)` for an `m.room.member` event worth announcing in
/// chat, or `None` for one that isn't (a profile update while staying
/// joined, a declined/retracted invite, etc). `unsigned.prev_content` -
/// present on state-carrying timeline events for a room the account has
/// fully joined - is what distinguishes a genuinely new transition (e.g.
/// invite -> join) from a same-state no-op event (join -> join, just a
/// displayname/avatar change).
fn classify_membership_change(event: &Value) -> Option<(&'static str, String)> {
    let membership = event["content"]["membership"].as_str().unwrap_or("");
    let prev_membership = event["unsigned"]["prev_content"]["membership"].as_str().unwrap_or("");
    let target = event["state_key"].as_str().unwrap_or("");
    let sender = event["sender"].as_str().unwrap_or("");
    let display_name = event["content"]["displayname"].as_str().filter(|s| !s.is_empty()).unwrap_or(target);
    let actor = protocol::mxid_localpart(sender);
    let reason = event["content"]["reason"].as_str();

    match membership {
        "join" if prev_membership != "join" => Some(("matrixJoin", format!("{display_name} joined the room"))),
        "invite" => Some(("matrixInvite", format!("{display_name} was invited by {actor}"))),
        "leave" if prev_membership == "join" && sender == target => Some(("matrixQuit", format!("{display_name} left the room"))),
        "leave" if prev_membership == "join" => {
            let mut body = format!("{display_name} was kicked by {actor}");
            if let Some(r) = reason {
                body.push_str(&format!(": {r}"));
            }
            Some(("matrixKick", body))
        }
        "ban" => {
            let mut body = format!("{display_name} was banned by {actor}");
            if let Some(r) = reason {
                body.push_str(&format!(": {r}"));
            }
            Some(("matrixKick", body))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member_event(sender: &str, target: &str, membership: &str, prev_membership: Option<&str>, displayname: Option<&str>, reason: Option<&str>) -> Value {
        let mut content = serde_json::json!({ "membership": membership });
        if let Some(d) = displayname {
            content["displayname"] = serde_json::json!(d);
        }
        if let Some(r) = reason {
            content["reason"] = serde_json::json!(r);
        }
        let mut event = serde_json::json!({
            "type": "m.room.member",
            "sender": sender,
            "state_key": target,
            "content": content,
        });
        if let Some(p) = prev_membership {
            event["unsigned"]["prev_content"] = serde_json::json!({ "membership": p });
        }
        event
    }

    #[test]
    fn a_fresh_join_announces() {
        let event = member_event("@alice:example.org", "@alice:example.org", "join", Some("invite"), Some("Alice"), None);
        assert_eq!(classify_membership_change(&event), Some(("matrixJoin", "Alice joined the room".to_string())));
    }

    #[test]
    fn a_profile_update_while_still_joined_does_not_announce() {
        let event = member_event("@alice:example.org", "@alice:example.org", "join", Some("join"), Some("Alice"), None);
        assert_eq!(classify_membership_change(&event), None);
    }

    #[test]
    fn an_invite_announces_with_the_inviter() {
        let event = member_event("@bob:example.org", "@carol:example.org", "invite", None, Some("Carol"), None);
        assert_eq!(classify_membership_change(&event), Some(("matrixInvite", "Carol was invited by bob".to_string())));
    }

    #[test]
    fn a_self_leave_is_a_quit_not_a_kick() {
        let event = member_event("@alice:example.org", "@alice:example.org", "leave", Some("join"), Some("Alice"), None);
        assert_eq!(classify_membership_change(&event), Some(("matrixQuit", "Alice left the room".to_string())));
    }

    #[test]
    fn a_leave_by_someone_else_is_a_kick_with_reason() {
        let event = member_event("@mod:example.org", "@alice:example.org", "leave", Some("join"), Some("Alice"), Some("spamming"));
        assert_eq!(classify_membership_change(&event), Some(("matrixKick", "Alice was kicked by mod: spamming".to_string())));
    }

    #[test]
    fn a_ban_announces_as_a_kick_kind_with_reason() {
        let event = member_event("@mod:example.org", "@alice:example.org", "ban", Some("join"), Some("Alice"), Some("rule 3"));
        assert_eq!(classify_membership_change(&event), Some(("matrixKick", "Alice was banned by mod: rule 3".to_string())));
    }

    #[test]
    fn revoking_a_pending_invite_is_not_announced_as_a_kick() {
        // Confirmed against a real homeserver: "kicking" someone who was
        // only ever invited (never actually joined) is a real, valid
        // action - it revokes the invite - but produces a "leave" event
        // with prev_membership "invite", not "join". Nobody who was
        // actually *in* the room got removed, so this correctly isn't a
        // "kick" announcement at all.
        let event = member_event("@mod:example.org", "@alice:example.org", "leave", Some("invite"), Some("Alice"), Some("changed my mind"));
        assert_eq!(classify_membership_change(&event), None);
    }

    #[test]
    fn no_displayname_falls_back_to_the_bare_mxid() {
        let event = member_event("@alice:example.org", "@dave:example.org", "join", Some("invite"), None, None);
        assert_eq!(classify_membership_change(&event), Some(("matrixJoin", "@dave:example.org joined the room".to_string())));
    }
}
