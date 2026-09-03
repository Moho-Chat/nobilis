//! Who somebody is, in one shape, whichever service they belong to.
//!
//! Each backend answers a different subset - IRC has an idle time and no
//! account age, Discord has the day the account was made and no idleness,
//! Matrix has a power level - so this is a bag of optional facts rather than
//! a form every service has to fill in. A field nobody knows is absent, and
//! the card that draws this shows what it was given.
//!
//! Answers arrive as a `profile` event rather than as the reply to the
//! request. IRC's own answer is several numerics ending in one that says the
//! reply is over, so it could never have been synchronous; and everything
//! else needs a request over the network, which this daemon must not sit on
//! while a client waits (see rpc/mod.rs's one-at-a-time connection loop).

use crate::state::AppState;
use serde_json::{json, Value};

/// Unix milliseconds of Discord's own epoch - the zero its snowflake ids
/// count from. Every Discord id carries its own creation time in its top
/// bits, which is why account age needs no request at all.
pub const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

/// The moment an id was minted, in unix seconds.
pub fn snowflake_created(id: &str) -> Option<i64> {
    let raw: u64 = id.parse().ok()?;
    Some((((raw >> 22) as i64) + DISCORD_EPOCH_MS) / 1000)
}

/// Starts a profile with what the caller already knows, so the card has a
/// name and a face before anything is fetched.
pub fn pending(service: &str, account_id: &str, name: &str) -> Value {
    json!({ "service": service, "accountId": account_id, "name": name, "pending": true })
}

/// Sends one to whoever asked.
pub fn emit(state: &AppState, profile: Value) {
    state.events.emit("profile", profile);
}

/// Adds a fact, if there is one. Keeps the call sites free of a conditional
/// each, and keeps absent facts absent rather than null.
pub fn set(profile: &mut Value, field: &str, value: Option<Value>) {
    if let Some(value) = value {
        profile[field] = value;
    }
}

/// Adds a labelled line for something with no field of its own.
pub fn note(profile: &mut Value, label: &str, value: impl Into<String>) {
    let value = value.into();
    if value.trim().is_empty() {
        return;
    }
    let entry = json!({ "label": label, "value": value });
    match profile["extra"].as_array_mut() {
        Some(list) => list.push(entry),
        None => profile["extra"] = json!([entry]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Discord id is a timestamp with a counter on the end, so the day an
    /// account was made needs no request - which matters, because it is the
    /// one fact people actually want from a profile and the endpoint that
    /// would otherwise answer it is rate-limited.
    #[test]
    fn a_snowflake_carries_its_own_birthday() {
        // Discord's own documented example: 175928847299117063 was created
        // on 2016-04-30T11:18:25.796Z.
        let created = snowflake_created("175928847299117063").expect("should parse");
        assert_eq!(created, 1_462_015_105);
        // The very first id is the epoch itself.
        assert_eq!(snowflake_created("0"), Some(DISCORD_EPOCH_MS / 1000));
        assert_eq!(snowflake_created("not an id"), None);
    }

    #[test]
    fn absent_facts_stay_absent() {
        let mut p = pending("irc", "acct", "someone");
        set(&mut p, "idleSeconds", None);
        set(&mut p, "createdTs", Some(json!(12)));
        note(&mut p, "Server", "");
        note(&mut p, "Server", "irc.example.net");
        assert!(p.get("idleSeconds").is_none());
        assert_eq!(p["createdTs"], 12);
        assert_eq!(p["extra"].as_array().unwrap().len(), 1);
    }
}
