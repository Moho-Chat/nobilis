//! Simplified sliding sync (MSC4186), where the homeserver speaks it.
//!
//! ## Why
//!
//! Classic `/sync` sends the whole account's state and grows with it. The
//! initial one on an account in a few hundred rooms is enormous, and every
//! incremental one carries rooms nobody is looking at. Sliding sync exists to
//! ask for what is wanted and nothing else, and it is the reason Element X
//! starts in a second where Element Web takes a minute.
//!
//! ## The shape of this
//!
//! An adapter, not a second sync. The response is translated into the shape
//! `process_sync_response` already reads, and that function - two hundred and
//! eighty lines that know how a room becomes a buffer, how a timeline becomes
//! messages, how encryption and receipts and typing and spaces work - is left
//! alone.
//!
//! Writing a second processor for the second wire format is the obvious
//! alternative and would have been a mistake: the two would agree on the day
//! they were written, and every fix after that would land in one of them.
//!
//! ## What this does not do yet, and says so
//!
//! Sliding sync's real trick is the window: ask for the twenty rooms on
//! screen, and page as somebody scrolls. moho does not work that way - the
//! buffer list holds every conversation at once, and a room outside the window
//! would simply vanish from it.
//!
//! So the list here is asked for in one wide range. That still wins most of
//! what the change is for, because the saving was never only the window: the
//! server sends a room once and then only what changed in it, where classic
//! sync re-sends state on every initial sync and carries rooms nobody has
//! opened. What it does not yet win is the part that needs moho's buffer list
//! to become window-aware, which is a change to the client rather than to this.
//!
//! One thing is genuinely lost and is worth naming rather than discovering:
//! MSC4186 has no presence extension, so who is online stops arriving. That is
//! why `supported` is consulted per account rather than assumed - see
//! `PRESENCE_IS_LOST` at the call site.

use serde_json::{json, Map, Value};

/// The endpoint, which is still unstable and still carries the MSC's name.
pub const SLIDING_PATH: &str = "/_matrix/client/unstable/org.matrix.simplified_msc3575/sync";

/// The flag a homeserver sets when it speaks this. Confirmed live against
/// matrix.org, which answers `org.matrix.simplified_msc3575: true`.
pub const SLIDING_FEATURE: &str = "org.matrix.simplified_msc3575";

/// How wide a window to ask for.
///
/// Deliberately wider than a screen - see the module comment. An account with
/// more rooms than this would lose the tail, so it is set past any plausible
/// number of conversations rather than at a number somebody would hit.
pub const WINDOW: u32 = 2000;

/// How much of each room's timeline to ask for on the first pass.
pub const TIMELINE_LIMIT: u32 = 20;

/// Whether this homeserver speaks it, by its own account.
pub fn supported_in_versions(versions: &Value) -> bool {
    versions["unstable_features"][SLIDING_FEATURE].as_bool().unwrap_or(false)
}

/// The state a room needs for moho to draw it.
///
/// Named rather than taken wholesale, which is the other half of what makes
/// sliding sync cheap: classic sync sends every state event in every room, and
/// almost none of it is read. `m.room.member` is asked for lazily - the
/// senders in the timeline and nobody else - because a room of ten thousand
/// people has ten thousand membership events and a roster nobody has opened.
fn required_state() -> Value {
    json!([
        ["m.room.create", ""],
        ["m.room.name", ""],
        ["m.room.topic", ""],
        ["m.room.avatar", ""],
        ["m.room.canonical_alias", ""],
        ["m.room.encryption", ""],
        ["m.room.power_levels", ""],
        ["m.room.tombstone", ""],
        ["m.room.join_rules", ""],
        ["m.space.child", "*"],
        ["m.space.parent", "*"],
        ["m.room.member", "$LAZY"],
    ])
}

/// What to ask for.
pub fn request_body() -> Value {
    json!({
        "lists": {
            "all": {
                "ranges": [[0, WINDOW]],
                "required_state": required_state(),
                "timeline_limit": TIMELINE_LIMIT,
            }
        },
        "extensions": {
            "to_device": { "enabled": true },
            "e2ee": { "enabled": true },
            "account_data": { "enabled": true },
            "receipts": { "enabled": true },
            "typing": { "enabled": true },
        }
    })
}

fn events(list: Value) -> Value {
    json!({ "events": list })
}

fn array(v: &Value) -> Value {
    Value::Array(v.as_array().cloned().unwrap_or_default())
}

/// Turns a simplified-sliding-sync response into the classic shape.
///
/// Every field `process_sync_response` and `receive_sync_changes` read is
/// filled in, and nothing else is - a field neither of them looks at would be
/// translation written for nobody.
pub fn to_classic(resp: &Value) -> Value {
    let ext = &resp["extensions"];

    // Room account data and the two ephemeral extensions arrive keyed by room
    // rather than inside it, so they are gathered first and folded in below.
    let room_account_data = &ext["account_data"]["rooms"];
    let receipts = &ext["receipts"]["rooms"];
    let typing = &ext["typing"]["rooms"];

    let mut join = Map::new();
    let mut invite = Map::new();
    let mut leave = Map::new();

    for (room_id, room) in resp["rooms"].as_object().into_iter().flatten() {
        // An invitation carries its stripped state and nothing else - no
        // timeline to read and no membership to act on until it is answered.
        if room["invite_state"].is_array() {
            invite.insert(room_id.clone(), json!({ "invite_state": events(array(&room["invite_state"])) }));
            continue;
        }

        let mut ephemeral: Vec<Value> = Vec::new();
        if let Some(r) = receipts.get(room_id) {
            ephemeral.push(r.clone());
        }
        if let Some(t) = typing.get(room_id) {
            ephemeral.push(t.clone());
        }

        let mut entry = json!({
            // A flat array here, an object with an `events` key there.
            "timeline": {
                "events": array(&room["timeline"]),
                "limited": room["limited"].as_bool().unwrap_or(false),
            },
            "state": events(array(&room["required_state"])),
            "ephemeral": events(Value::Array(ephemeral)),
            "account_data": events(array(room_account_data.get(room_id).unwrap_or(&Value::Null))),
        });
        // Only when the server gave one: the anchor for reading older history
        // is kept from the first sync that mentions a room, and writing a null
        // over it would lose the place.
        if let Some(prev) = room["prev_batch"].as_str() {
            entry["timeline"]["prev_batch"] = json!(prev);
        }

        // A room this account has left arrives with that membership in its
        // state. Sorted here rather than in the processor, which reads the
        // three buckets and trusts them.
        if left_in(&room["required_state"]) {
            leave.insert(room_id.clone(), entry);
        } else {
            join.insert(room_id.clone(), entry);
        }
    }

    json!({
        // The token is called something else and means the same thing.
        "next_batch": resp["pos"],
        "account_data": events(array(&ext["account_data"]["global"])),
        // MSC4186 has no presence extension. Empty rather than absent, so the
        // processor's own "nothing to say about presence" path runs instead of
        // it reading a null.
        "presence": events(json!([])),
        "to_device": events(array(&ext["to_device"]["events"])),
        "device_lists": ext["e2ee"]["device_lists"],
        "device_one_time_keys_count": ext["e2ee"]["device_one_time_keys_count"],
        "device_unused_fallback_key_types": ext["e2ee"]["device_unused_fallback_key_types"],
        "rooms": { "join": join, "invite": invite, "leave": leave },
    })
}

/// Whether this account's own membership in the room's state says it has left.
fn left_in(state: &Value) -> bool {
    state
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"].as_str() == Some("m.room.member"))
        .any(|e| matches!(e["content"]["membership"].as_str(), Some("leave") | Some("ban")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({
            "pos": "s99",
            "lists": { "all": { "count": 2 } },
            "rooms": {
                "!a:example.org": {
                    "required_state": [
                        { "type": "m.room.name", "state_key": "", "content": { "name": "General" } }
                    ],
                    "timeline": [
                        { "type": "m.room.message", "event_id": "$1", "content": { "body": "hi" } }
                    ],
                    "prev_batch": "t1",
                    "limited": true
                },
                "!b:example.org": { "invite_state": [{ "type": "m.room.name" }] }
            },
            "extensions": {
                "to_device": { "events": [{ "type": "m.room_key" }] },
                "e2ee": {
                    "device_lists": { "changed": ["@x:example.org"], "left": [] },
                    "device_one_time_keys_count": { "signed_curve25519": 42 }
                },
                "account_data": {
                    "global": [{ "type": "m.ignored_user_list" }],
                    "rooms": { "!a:example.org": [{ "type": "m.marked_unread" }] }
                },
                "receipts": { "rooms": { "!a:example.org": { "type": "m.receipt" } } },
                "typing": { "rooms": { "!a:example.org": { "type": "m.typing" } } }
            }
        })
    }

    #[test]
    fn the_token_is_carried_under_the_name_the_processor_reads() {
        assert_eq!(to_classic(&sample())["next_batch"], json!("s99"));
    }

    /// A flat array there, an object with an `events` key here.
    #[test]
    fn a_timeline_becomes_the_shape_the_processor_expects() {
        let c = to_classic(&sample());
        let room = &c["rooms"]["join"]["!a:example.org"];
        assert_eq!(room["timeline"]["events"].as_array().unwrap().len(), 1);
        assert_eq!(room["timeline"]["prev_batch"], json!("t1"));
        assert_eq!(room["timeline"]["limited"], json!(true));
        assert_eq!(room["state"]["events"][0]["type"], json!("m.room.name"));
    }

    /// Receipts and typing arrive keyed by room rather than inside it, and
    /// both land in the one ephemeral bucket the processor reads.
    #[test]
    fn the_two_ephemeral_extensions_are_folded_back_into_their_rooms() {
        let c = to_classic(&sample());
        let kinds: Vec<&str> = c["rooms"]["join"]["!a:example.org"]["ephemeral"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["type"].as_str())
            .collect();
        assert!(kinds.contains(&"m.receipt"));
        assert!(kinds.contains(&"m.typing"));
    }

    #[test]
    fn room_account_data_is_folded_back_in_and_global_stays_global() {
        let c = to_classic(&sample());
        assert_eq!(
            c["rooms"]["join"]["!a:example.org"]["account_data"]["events"][0]["type"],
            json!("m.marked_unread")
        );
        assert_eq!(c["account_data"]["events"][0]["type"], json!("m.ignored_user_list"));
    }

    /// The crypto machine reads these three off the top level, so the e2ee
    /// extension has to be unwrapped rather than passed along.
    #[test]
    fn the_crypto_fields_land_where_the_machine_looks_for_them() {
        let c = to_classic(&sample());
        assert_eq!(c["to_device"]["events"][0]["type"], json!("m.room_key"));
        assert_eq!(c["device_lists"]["changed"][0], json!("@x:example.org"));
        assert_eq!(c["device_one_time_keys_count"]["signed_curve25519"], json!(42));
    }

    #[test]
    fn an_invitation_goes_in_the_invite_bucket_with_its_stripped_state() {
        let c = to_classic(&sample());
        assert!(c["rooms"]["invite"]["!b:example.org"]["invite_state"]["events"].is_array());
        assert!(c["rooms"]["join"].get("!b:example.org").is_none());
    }

    /// A room whose own membership says it has been left is sorted out, so the
    /// processor's leave handling runs rather than it being treated as joined.
    #[test]
    fn a_left_room_is_sorted_into_leave() {
        let resp = json!({
            "pos": "s1",
            "rooms": { "!c:example.org": {
                "required_state": [{ "type": "m.room.member", "content": { "membership": "leave" } }],
                "timeline": []
            }}
        });
        let c = to_classic(&resp);
        assert!(c["rooms"]["leave"].get("!c:example.org").is_some());
        assert!(c["rooms"]["join"].get("!c:example.org").is_none());
    }

    /// An empty response must still have every bucket, or the processor reads
    /// a null where it expects an object.
    #[test]
    fn an_empty_response_still_has_every_shape_the_processor_reads() {
        let c = to_classic(&json!({ "pos": "s1" }));
        assert!(c["rooms"]["join"].is_object());
        assert!(c["rooms"]["invite"].is_object());
        assert!(c["rooms"]["leave"].is_object());
        assert!(c["account_data"]["events"].is_array());
        assert!(c["presence"]["events"].is_array());
        assert!(c["to_device"]["events"].is_array());
    }

    /// Writing a null over a prev_batch would lose the anchor for reading
    /// older history, so a response that does not carry one says nothing.
    #[test]
    fn a_missing_prev_batch_is_left_out_rather_than_written_as_null() {
        let resp = json!({ "pos": "s1", "rooms": { "!a:x": { "timeline": [] } } });
        let c = to_classic(&resp);
        assert!(c["rooms"]["join"]["!a:x"]["timeline"].get("prev_batch").is_none());
    }

    #[test]
    fn the_feature_flag_is_read_from_the_versions_answer() {
        assert!(supported_in_versions(&json!({ "unstable_features": { SLIDING_FEATURE: true } })));
        assert!(!supported_in_versions(&json!({ "unstable_features": { SLIDING_FEATURE: false } })));
        assert!(!supported_in_versions(&json!({ "unstable_features": {} })));
        assert!(!supported_in_versions(&json!({})));
    }

    #[test]
    fn the_request_asks_for_the_state_a_room_needs_and_lazy_members() {
        let body = request_body();
        let state = &body["lists"]["all"]["required_state"];
        let pairs: Vec<String> = state.as_array().unwrap().iter().map(|p| p[0].as_str().unwrap().to_string()).collect();
        for wanted in ["m.room.name", "m.room.encryption", "m.room.power_levels", "m.space.child"] {
            assert!(pairs.contains(&wanted.to_string()), "{wanted} should be asked for");
        }
        // The roster is the expensive one and is asked for lazily.
        let members = state.as_array().unwrap().iter().find(|p| p[0] == "m.room.member").unwrap();
        assert_eq!(members[1], json!("$LAZY"));
        for ext in ["to_device", "e2ee", "account_data", "receipts", "typing"] {
            assert_eq!(body["extensions"][ext]["enabled"], json!(true), "{ext} should be enabled");
        }
    }
}
