//! Room id <-> buffer name/kind derivation.
//!
//! Only run once per room, the first time a buffer is created for it (see
//! mod.rs's process_sync_response) - subsequent syncs reuse the cached
//! name/kind rather than re-deriving on every event, both because most
//! incremental `/sync` responses carry no naming-relevant state at all
//! (re-deriving from an empty/partial delta would wrongly fall back to
//! the raw room id) and because live room renames aren't tracked, the
//! same accepted limitation backend/discord/guilds.rs's own channel-name-as-
//! buffer-name design already has for a renamed Discord channel.

use serde_json::Value;

pub struct RoomInfo {
    pub name: String,
    pub kind: String,
}

/// Priority: `m.room.name` -> `m.room.canonical_alias` -> (the person the
/// account's `m.direct` list says this room is with, or failing that exactly
/// 2 joined members: their display name, kind "dm") -> the raw room id, kind
/// "channel". `events` should be the union of this sync response's
/// `state.events` and any state-key-bearing `timeline.events` for the room
/// (Matrix inlines recent state changes into the timeline rather than always
/// the separate state array) - see mod.rs's call site.
///
/// `direct_peer` is the account's own answer, from `m.direct` (see the
/// `directs` module), and it outranks the member count entirely: a DM can
/// have three people in it and a two-person room need not be one. The count
/// stays as the fallback for an account whose list has not been read yet, or
/// one that has never had a DM written to it by any client.
pub fn derive_room_info(room_id: &str, own_user_id: &str, events: &[&Value], direct_peer: Option<&str>) -> RoomInfo {
    let mut room_name: Option<String> = None;
    let mut canonical_alias: Option<String> = None;
    let mut joined_count = 0usize;
    let mut other_member_name: Option<String> = None;
    let mut peer_name: Option<String> = None;

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
                // What to call the person this room is *with*, at any
                // membership: a DM opened from here is named after somebody
                // who has only been invited, and waiting for them to join
                // would leave the conversation labelled with a room id until
                // they did.
                if direct_peer.is_some() && event["state_key"].as_str() == direct_peer {
                    if let Some(display) = event["content"]["displayname"].as_str().filter(|s| !s.is_empty()) {
                        peer_name = Some(display.to_string());
                    }
                }
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

    // A DM is a DM whatever it is called. Element shows a named DM under its
    // name and still files it with the conversations, and the kind is what
    // decides where it goes in the rail - so the name and the kind are
    // decided separately rather than one falling out of the other.
    let kind = if direct_peer.is_some() || (room_name.is_none() && canonical_alias.is_none() && joined_count == 2 && other_member_name.is_some()) {
        "dm"
    } else {
        "channel"
    };

    let name = room_name
        .or(canonical_alias)
        .or_else(|| {
            // Only a DM is named after a person. An unnamed room with several
            // people in it is not named after whichever of them the state
            // happened to mention last.
            if kind != "dm" {
                return None;
            }
            peer_name
                // The mxid itself is a worse name than a display name and a
                // far better one than a room id.
                .or_else(|| direct_peer.map(str::to_string))
                .or(other_member_name)
        })
        .unwrap_or_else(|| room_id.to_string());

    RoomInfo { name, kind: kind.to_string() }
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

/// What a pending invitation looks like, out of the stripped state Matrix
/// sends with it.
///
/// An invite is not a joined room and has none of its history: the server
/// hands over a handful of state events chosen to be enough to decide with -
/// usually the room's name and avatar, and the membership event naming who
/// invited you. Everything here is best-effort for that reason, and the
/// room id is the only field guaranteed to exist.
///
/// The inviter matters more than it looks. A room with no name shows as its
/// own id, which tells nobody anything; "Salastil invited you" is the part
/// that makes the decision answerable.
pub fn invite_summary(room_id: &str, own_user_id: &str, events: &[&Value]) -> serde_json::Value {
    let mut name: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut avatar: Option<String> = None;
    let mut inviter: Option<String> = None;
    let mut is_direct = false;

    for event in events {
        match event["type"].as_str().unwrap_or("") {
            "m.room.name" => {
                if let Some(n) = event["content"]["name"].as_str().filter(|n| !n.is_empty()) {
                    name = Some(n.to_string());
                }
            }
            "m.room.canonical_alias" => {
                if let Some(a) = event["content"]["alias"].as_str().filter(|a| !a.is_empty()) {
                    alias = Some(a.to_string());
                }
            }
            "m.room.avatar" => {
                if let Some(u) = event["content"]["url"].as_str().filter(|u| !u.is_empty()) {
                    avatar = Some(u.to_string());
                }
            }
            // Ours is the one that says we were invited; its sender is who
            // did the inviting. Other members' events ride along in the same
            // list, so the state_key has to be checked.
            "m.room.member" => {
                if event["state_key"].as_str() == Some(own_user_id)
                    && event["content"]["membership"].as_str() == Some("invite")
                {
                    inviter = event["sender"].as_str().map(str::to_string);
                    is_direct = event["content"]["is_direct"].as_bool().unwrap_or(false);
                }
            }
            _ => {}
        }
    }

    // A direct message has no name of its own, so falling back to the room
    // id put a tile in the rail labelled "!vj6SZEo74ZIjCajgM..." - which is
    // both unreadable and unanswerable. Whoever sent it is the only thing
    // about an unnamed invitation worth showing.
    let display = name
        .or(alias)
        .or_else(|| if is_direct { inviter.clone() } else { None })
        .unwrap_or_else(|| room_id.to_string());
    serde_json::json!({
        "roomId": room_id,
        "name": display,
        "inviter": inviter,
        "avatarUrl": avatar,
        "isDirect": is_direct,
    })
}

#[cfg(test)]
mod space_tests {
    use super::{derive_room_info, invite_summary, is_space, room_avatar_mxc, space_children};
    use serde_json::{json, Value};

    fn refs(events: &[Value]) -> Vec<&Value> {
        events.iter().collect()
    }

    /// The inviter is the part that makes an unnamed room answerable, and it
    /// comes off our *own* membership event - other members' events ride
    /// along in the same stripped-state list.
    #[test]
    fn an_invite_names_the_room_and_who_sent_it() {
        let events = vec![
            json!({ "type": "m.room.name", "content": { "name": "Book club" } }),
            json!({ "type": "m.room.member", "state_key": "@someone:else.org", "sender": "@someone:else.org", "content": { "membership": "join" } }),
            json!({ "type": "m.room.member", "state_key": "@me:poa.st", "sender": "@salastil:poa.st", "content": { "membership": "invite" } }),
        ];
        let got = invite_summary("!abc:poa.st", "@me:poa.st", &refs(&events));
        assert_eq!(got["roomId"], "!abc:poa.st");
        assert_eq!(got["name"], "Book club");
        assert_eq!(got["inviter"], "@salastil:poa.st");
        assert_eq!(got["isDirect"], false);
    }

    /// A room with no name falls back to its alias, then to the id - which
    /// tells nobody anything on its own, which is exactly why the inviter is
    /// carried alongside it.
    #[test]
    fn an_unnamed_invite_falls_back_through_alias_to_the_id() {
        let aliased = vec![json!({ "type": "m.room.canonical_alias", "content": { "alias": "#books:poa.st" } })];
        assert_eq!(invite_summary("!abc:poa.st", "@me:poa.st", &refs(&aliased))["name"], "#books:poa.st");

        let bare: Vec<Value> = vec![];
        assert_eq!(invite_summary("!abc:poa.st", "@me:poa.st", &refs(&bare))["name"], "!abc:poa.st");
    }

    /// A direct-message invite says so, so it can be shown as a person
    /// rather than as a room.
    #[test]
    fn a_direct_invite_is_marked_as_one() {
        let events = vec![json!({
            "type": "m.room.member", "state_key": "@me:poa.st", "sender": "@friend:poa.st",
            "content": { "membership": "invite", "is_direct": true }
        })];
        let got = invite_summary("!dm:poa.st", "@me:poa.st", &refs(&events));
        assert_eq!(got["isDirect"], true);
        assert_eq!(got["inviter"], "@friend:poa.st");
    }

    fn member(mxid: &str, name: &str, membership: &str) -> Value {
        json!({ "type": "m.room.member", "state_key": mxid, "sender": mxid,
                "content": { "membership": membership, "displayname": name } })
    }

    /// The account's own m.direct list decides, and it outranks the member
    /// count in both directions: a DM can have three people in it, and an
    /// ordinary two-person room is not a conversation just because it is
    /// small.
    #[test]
    fn the_account_says_which_rooms_are_direct_messages() {
        let two = vec![member("@me:poa.st", "me", "join"), member("@anna:poa.st", "Anna", "join")];

        // Listed: a DM, named after the person, whatever else is in it.
        let listed = derive_room_info("!dm:poa.st", "@me:poa.st", &refs(&two), Some("@anna:poa.st"));
        assert_eq!(listed.kind, "dm");
        assert_eq!(listed.name, "Anna");

        // A third person in a room the account still calls a DM - which is
        // ordinary, and which the old two-member count got wrong.
        let three = vec![
            member("@me:poa.st", "me", "join"),
            member("@anna:poa.st", "Anna", "join"),
            member("@bob:poa.st", "Bob", "join"),
        ];
        let crowded = derive_room_info("!dm:poa.st", "@me:poa.st", &refs(&three), Some("@anna:poa.st"));
        assert_eq!(crowded.kind, "dm");
        assert_eq!(crowded.name, "Anna");

        // Not listed, and no name: the old heuristic still answers, because
        // an account whose list has never been written has no other answer.
        let guessed = derive_room_info("!x:poa.st", "@me:poa.st", &refs(&two), None);
        assert_eq!(guessed.kind, "dm");
        assert_eq!(guessed.name, "Anna");
    }

    /// A DM with a name is shown under its name and still filed with the
    /// conversations - the two decisions are separate, which is what Element
    /// does.
    #[test]
    fn a_named_direct_message_keeps_its_name_and_its_kind() {
        let events = vec![
            json!({ "type": "m.room.name", "content": { "name": "Holiday plans" } }),
            member("@me:poa.st", "me", "join"),
            member("@anna:poa.st", "Anna", "join"),
        ];
        let got = derive_room_info("!dm:poa.st", "@me:poa.st", &refs(&events), Some("@anna:poa.st"));
        assert_eq!(got.name, "Holiday plans");
        assert_eq!(got.kind, "dm");

        // And the same room with nobody claiming it as a DM is an ordinary
        // room, as it always was.
        let plain = derive_room_info("!dm:poa.st", "@me:poa.st", &refs(&events), None);
        assert_eq!(plain.kind, "channel");
    }

    /// A conversation opened from here is named after somebody who has only
    /// been invited. Waiting for them to join would leave it labelled with a
    /// room id until they did - which on an unanswered invitation is for ever.
    #[test]
    fn a_direct_message_is_named_before_the_other_person_arrives() {
        let events = vec![member("@me:poa.st", "me", "join"), member("@anna:poa.st", "Anna", "invite")];
        let got = derive_room_info("!new:poa.st", "@me:poa.st", &refs(&events), Some("@anna:poa.st"));
        assert_eq!(got.kind, "dm");
        assert_eq!(got.name, "Anna");

        // With no display name anywhere, the mxid - a worse name than a
        // display name and a far better one than a room id.
        let nameless = vec![json!({ "type": "m.room.member", "state_key": "@anna:poa.st", "content": { "membership": "invite" } })];
        let got = derive_room_info("!new:poa.st", "@me:poa.st", &refs(&nameless), Some("@anna:poa.st"));
        assert_eq!(got.name, "@anna:poa.st");
    }

    /// An unnamed room with several people in it keeps its id rather than
    /// taking the name of whichever member the state happened to mention
    /// last - which would be arbitrary and would change between syncs.
    #[test]
    fn an_unnamed_group_room_is_not_named_after_somebody() {
        let three = vec![
            member("@me:poa.st", "me", "join"),
            member("@anna:poa.st", "Anna", "join"),
            member("@bob:poa.st", "Bob", "join"),
        ];
        let got = derive_room_info("!x:poa.st", "@me:poa.st", &refs(&three), None);
        assert_eq!(got.kind, "channel");
        assert_eq!(got.name, "!x:poa.st");
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
