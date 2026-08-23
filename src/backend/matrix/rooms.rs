//! Room id <-> buffer name/kind derivation.
//!
//! Only run once per room, the first time a buffer is created for it (see
//! mod.rs's process_sync_response) - subsequent syncs reuse the cached
//! name/kind rather than re-deriving on every event, both because most
//! incremental `/sync` responses carry no naming-relevant state at all
//! (re-deriving from an empty/partial delta would wrongly fall back to
//! the raw room id) and because live room renames aren't tracked, the
//! same accepted limitation backend/discord.rs's own channel-name-as-
//! buffer-name design already has for a renamed Discord channel.

use serde_json::Value;

pub struct RoomInfo {
    pub name: String,
    pub kind: String,
}

/// Priority: `m.room.name` -> `m.room.canonical_alias` -> (exactly 2
/// joined members: the other member's display name, kind "dm") -> the raw
/// room id, kind "channel". `events` should be the union of this sync
/// response's `state.events` and any state-key-bearing `timeline.events`
/// for the room (Matrix inlines recent state changes into the timeline
/// rather than always the separate state array) - see mod.rs's call site.
pub fn derive_room_info(room_id: &str, own_user_id: &str, events: &[&Value]) -> RoomInfo {
    let mut room_name: Option<String> = None;
    let mut canonical_alias: Option<String> = None;
    let mut joined_count = 0usize;
    let mut other_member_name: Option<String> = None;

    for event in events {
        match event["type"].as_str().unwrap_or("") {
            "m.room.name" => {
                if let Some(n) = event["content"]["name"].as_str() {
                    if !n.is_empty() {
                        room_name = Some(n.to_string());
                    }
                }
            }
            "m.room.canonical_alias" => {
                if let Some(a) = event["content"]["alias"].as_str() {
                    canonical_alias = Some(a.to_string());
                }
            }
            "m.room.member" => {
                if event["content"]["membership"].as_str() == Some("join") {
                    joined_count += 1;
                    let sender = event["sender"].as_str().unwrap_or("");
                    if sender != own_user_id {
                        let display = event["content"]["displayname"].as_str().filter(|s| !s.is_empty()).unwrap_or(sender);
                        other_member_name = Some(display.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(name) = room_name.or(canonical_alias) {
        return RoomInfo { name, kind: "channel".to_string() };
    }
    if joined_count == 2 {
        if let Some(name) = other_member_name {
            return RoomInfo { name, kind: "dm".to_string() };
        }
    }
    RoomInfo { name: room_id.to_string(), kind: "channel".to_string() }
}

pub fn is_encrypted(events: &[&Value]) -> bool {
    events.iter().any(|e| e["type"].as_str() == Some("m.room.encryption"))
}

/// Whether this room is a Space rather than somewhere you talk.
///
/// A Space is an ordinary room carrying `type: "m.space"` in its creation
/// event - there is no separate object for it, which is why a client that
/// doesn't check ends up showing spaces in its room list as if they were
/// chats.
pub fn is_space(events: &[&Value]) -> bool {
    events
        .iter()
        .find(|e| e["type"].as_str() == Some("m.room.create"))
        .and_then(|e| e["content"]["type"].as_str())
        == Some("m.space")
}

/// The rooms a space lists as its children.
///
/// Each child is an `m.space.child` state event whose state key is the child's
/// room id. Removing a child leaves the event in place with empty content
/// rather than deleting it, so an entry with nothing in it means "no longer a
/// child" and must be skipped - otherwise rooms reappear in a space long after
/// they were taken out of it.
pub fn space_children(events: &[&Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"].as_str() == Some("m.space.child"))
        .filter(|e| e["content"].as_object().is_some_and(|c| !c.is_empty()))
        .filter_map(|e| e["state_key"].as_str().map(String::from))
        .collect()
}

/// The room's own avatar, as an unresolved mxc URI.
pub fn room_avatar_mxc(events: &[&Value]) -> Option<String> {
    events
        .iter()
        .find(|e| e["type"].as_str() == Some("m.room.avatar"))
        .and_then(|e| e["content"]["url"].as_str())
        .map(String::from)
}

#[cfg(test)]
mod space_tests {
    use super::{is_space, room_avatar_mxc, space_children};
    use serde_json::{json, Value};

    fn refs(events: &[Value]) -> Vec<&Value> {
        events.iter().collect()
    }

    #[test]
    fn a_space_is_told_apart_by_its_creation_type() {
        let space = vec![json!({ "type": "m.room.create", "content": { "type": "m.space" } })];
        assert!(is_space(&refs(&space)));

        let room = vec![json!({ "type": "m.room.create", "content": { "room_version": "10" } })];
        assert!(!is_space(&refs(&room)));
        assert!(!is_space(&[]));
    }

    #[test]
    fn children_come_from_the_state_keys() {
        let events = vec![
            json!({ "type": "m.space.child", "state_key": "!a:example.org", "content": { "via": ["example.org"] } }),
            json!({ "type": "m.space.child", "state_key": "!b:example.org", "content": { "via": ["example.org"] } }),
            json!({ "type": "m.room.name", "state_key": "", "content": { "name": "not a child" } }),
        ];
        assert_eq!(space_children(&refs(&events)), vec!["!a:example.org", "!b:example.org"]);
    }

    #[test]
    fn a_removed_child_is_not_a_child() {
        // Removal empties the content rather than deleting the event, so
        // taking every m.space.child at face value resurrects old members.
        let events = vec![
            json!({ "type": "m.space.child", "state_key": "!gone:example.org", "content": {} }),
            json!({ "type": "m.space.child", "state_key": "!here:example.org", "content": { "via": ["example.org"] } }),
        ];
        assert_eq!(space_children(&refs(&events)), vec!["!here:example.org"]);
    }

    #[test]
    fn the_avatar_is_read_as_an_mxc_uri() {
        let events = vec![json!({ "type": "m.room.avatar", "content": { "url": "mxc://example.org/abc" } })];
        assert_eq!(room_avatar_mxc(&refs(&events)).as_deref(), Some("mxc://example.org/abc"));
        assert_eq!(room_avatar_mxc(&[]), None);
    }
}
