//! READY as a client that asked for DEDUPE_USER_OBJECTS receives it, put back as every reader here expects it.
//!
//! With that capability Discord "dehydrates" READY (<https://docs.discord.food/topics/gateway-events#ready>): every
//! user object is moved into one `users` table and left behind as an id - `user_id` in members, presences and
//! relationships, `recipient_ids` in private channels - each guild's `members` is collapsed into READY's
//! `merged_members` (one list a guild, in the order of `guilds`), and `presences`, with each guild's, becomes
//! `merged_presences`. Everything that reads READY - the friends list, the names of conversations, who is where in a
//! voice channel - was written against the objects. So they are put back first, here, once, and nothing else has to
//! know the shape changed. A READY without a `users` table is already whole and comes back unchanged.

use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// READY with its users put back, or `None` when there is nothing to put back.
pub fn rehydrate(ready: &Value) -> Option<Value> {
    let users: HashMap<String, Value> = ready["users"]
        .as_array()?
        .iter()
        .filter_map(|u| Some((u["id"].as_str()?.to_string(), u.clone())))
        .collect();
    let mut out = ready.clone();

    // The members the guilds were relieved of, back where they were: in the guild, by its place in the list.
    if let Some(merged) = ready["merged_members"].as_array() {
        if let Some(guilds) = out["guilds"].as_array_mut() {
            for (guild, members) in guilds.iter_mut().zip(merged) {
                if guild["members"].is_null() && members.is_array() {
                    guild["members"] = members.clone();
                }
            }
        }
    }
    // And the presences: the friends' as READY's own `presences`, each guild's with the guild.
    if let Some(merged) = ready["merged_presences"].as_object() {
        if out["presences"].is_null() {
            out["presences"] = merged.get("friends").cloned().unwrap_or_else(|| json!([]));
        }
        if let (Some(per_guild), Some(guilds)) = (merged.get("guilds").and_then(Value::as_array), out["guilds"].as_array_mut()) {
            for (guild, presences) in guilds.iter_mut().zip(per_guild) {
                if guild["presences"].is_null() && presences.is_array() {
                    guild["presences"] = presences.clone();
                }
            }
        }
    }

    restore(&mut out, &users);
    Some(out)
}

/// Gives every object that names a user by id the user itself, and every conversation that lists its recipients by id
/// the recipients, wherever in the payload they are.
fn restore(value: &mut Value, users: &HashMap<String, Value>) {
    match value {
        Value::Object(object) => {
            fill(object, users);
            for child in object.values_mut() {
                restore(child, users);
            }
        }
        Value::Array(list) => {
            for child in list {
                restore(child, users);
            }
        }
        _ => {}
    }
}

fn fill(object: &mut Map<String, Value>, users: &HashMap<String, Value>) {
    if !object.contains_key("user") {
        if let Some(user) = object.get("user_id").and_then(Value::as_str).and_then(|id| users.get(id)) {
            object.insert("user".to_string(), user.clone());
        }
    }
    if !object.contains_key("recipients") {
        if let Some(ids) = object.get("recipient_ids").and_then(Value::as_array) {
            let recipients: Vec<Value> = ids.iter().filter_map(|id| users.get(id.as_str()?).cloned()).collect();
            object.insert("recipients".to_string(), Value::Array(recipients));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// READY as the documentation describes it for a client with DEDUPE_USER_OBJECTS: users in a table, ids in their
    /// place, members and presences merged.
    fn dehydrated() -> Value {
        json!({
            "user": { "id": "1", "username": "me" },
            "users": [
                { "id": "1", "username": "me" },
                { "id": "2", "username": "ada", "global_name": "Ada" },
                { "id": "3", "username": "bob" }
            ],
            "relationships": [{ "id": "2", "type": 1, "user_id": "2" }, { "id": "3", "type": 3, "user_id": "3" }],
            "private_channels": [{ "id": "20", "type": 1, "recipient_ids": ["2"], "last_message_id": null }, { "id": "21", "type": 3, "recipient_ids": ["2", "3"] }],
            "merged_presences": { "friends": [{ "user_id": "2", "status": "online", "activities": [] }], "guilds": [[{ "user_id": "3", "status": "idle" }]] },
            "guilds": [{ "id": "g", "voice_states": [{ "user_id": "3", "channel_id": "v" }] }],
            "merged_members": [[{ "user_id": "1", "roles": ["r"], "pending": false }, { "user_id": "3", "roles": [] }]]
        })
    }

    #[test]
    fn a_dehydrated_ready_is_made_whole_again() {
        let whole = rehydrate(&dehydrated()).expect("there was a users table");
        // Friends and their requests, by the user object every reader looks for.
        assert_eq!(whole["relationships"][0]["user"]["username"], "ada");
        assert_eq!(whole["relationships"][1]["user"]["id"], "3");
        // Conversations with the people in them.
        assert_eq!(whole["private_channels"][0]["recipients"][0]["global_name"], "Ada");
        assert_eq!(whole["private_channels"][1]["recipients"].as_array().unwrap().len(), 2);
        // Presences where READY used to carry them.
        assert_eq!(whole["presences"][0]["user"]["id"], "2");
        assert_eq!(whole["presences"][0]["status"], "online");
        assert_eq!(whole["guilds"][0]["presences"][0]["user"]["username"], "bob");
        // The guild's members, and our own among them as `own_member` looks for it.
        let members = whole["guilds"][0]["members"].as_array().unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0]["user"]["id"], "1");
        assert_eq!(members[0]["roles"][0], "r");
        // Voice states, which name their user the same way.
        assert_eq!(whole["guilds"][0]["voice_states"][0]["user"]["username"], "bob");
    }

    #[test]
    fn a_ready_that_was_never_dehydrated_is_left_as_it_was() {
        let whole = json!({ "user": { "id": "1" }, "relationships": [{ "id": "2", "user": { "id": "2" } }] });
        assert!(rehydrate(&whole).is_none());
    }

    #[test]
    fn what_is_already_there_is_not_overwritten_and_the_unknown_is_left_alone() {
        let mut ready = dehydrated();
        ready["relationships"][0]["user"] = json!({ "id": "2", "username": "already" });
        ready["private_channels"][0]["recipient_ids"] = json!(["2", "999"]);
        ready["guilds"][0]["members"] = json!([{ "user": { "id": "1" } }]);
        let whole = rehydrate(&ready).unwrap();
        assert_eq!(whole["relationships"][0]["user"]["username"], "already");
        assert_eq!(whole["private_channels"][0]["recipients"].as_array().unwrap().len(), 1, "an id nobody told us about is not made up");
        assert_eq!(whole["guilds"][0]["members"].as_array().unwrap().len(), 1, "a guild's own members are not replaced");
    }

    #[test]
    fn the_whole_thing_can_be_done_twice() {
        let once = rehydrate(&dehydrated()).unwrap();
        let twice = rehydrate(&once).unwrap();
        assert_eq!(once, twice);
    }
}
