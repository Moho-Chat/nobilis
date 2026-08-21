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
