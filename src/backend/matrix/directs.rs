//! Which rooms this account calls direct messages.
//!
//! Matrix keeps that list in one place - the `m.direct` account data event,
//! a map of user id to the rooms shared with them - and it is the only
//! answer that travels. A room is a DM because the account says so, not
//! because of anything visible in the room itself: a DM can have three
//! people in it, a name, and a topic, and an ordinary two-person room is not
//! a DM at all.
//!
//! Before this, a DM was guessed at: two joined members and no name, or an
//! `is_direct` flag read off an invitation. That guess disagrees with every
//! other client in both directions - a conversation started in Element
//! arrived here as an ordinary room named after its id, and one started here
//! was an ordinary room everywhere else, because nothing wrote the list
//! back. The guess is kept as a fallback for an account whose list has not
//! been read yet; where the list says anything, the list wins.

use super::*;
use serde_json::{json, Map};

/// The list, flattened to the way a room is asked about: room id -> the
/// person it is with.
///
/// Inverted on the way in because every question here is "is *this room* a
/// DM, and who with" - the account data is keyed the other way, by person,
/// since one person can have several rooms with you.
pub fn read(content: &Value) -> Vec<(String, String)> {
    let Some(by_user) = content.as_object() else { return Vec::new() };
    let mut pairs = Vec::new();
    for (user_id, rooms) in by_user {
        for room in rooms.as_array().into_iter().flatten() {
            if let Some(room_id) = room.as_str().filter(|r| !r.is_empty()) {
                pairs.push((room_id.to_string(), user_id.clone()));
            }
        }
    }
    pairs
}

/// The list with one more room in it.
///
/// Read-modify-write, because account data is replaced whole and this client
/// is not the only thing writing it: dropping the other entries would un-DM
/// every other conversation the account has, on every device. Adding a room
/// already listed changes nothing rather than listing it twice.
pub fn with(content: &Value, user_id: &str, room_id: &str) -> Value {
    let mut by_user = content.as_object().cloned().unwrap_or_else(Map::new);
    let rooms = by_user.entry(user_id.to_string()).or_insert_with(|| json!([]));
    let already = rooms.as_array().into_iter().flatten().any(|r| r.as_str() == Some(room_id));
    if !already {
        if let Some(list) = rooms.as_array_mut() {
            list.push(json!(room_id));
        } else {
            *rooms = json!([room_id]);
        }
    }
    Value::Object(by_user)
}

/// The list without a room, wherever it appears.
///
/// A person is dropped entirely when their last room goes: an empty array
/// left behind is a person the account claims to have a DM list for and no
/// DMs with, which other clients render as an empty conversation.
pub fn without(content: &Value, room_id: &str) -> Value {
    let Some(by_user) = content.as_object() else { return json!({}) };
    let mut out = Map::new();
    for (user_id, rooms) in by_user {
        let kept: Vec<Value> = rooms
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r.as_str() != Some(room_id))
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.insert(user_id.clone(), Value::Array(kept));
        }
    }
    Value::Object(out)
}

/// Reads the account's list once, at connect.
///
/// Asked for directly rather than waited for: a resumed connection gets only
/// incremental syncs, and account data that has not changed since the last
/// cursor never arrives again - so an account that had not reopened a DM
/// since before this existed would sit there with every conversation filed as
/// an ordinary room.
pub(super) async fn fetch(state: &AppState, account_id: &str, homeserver_url: &str, access_token: &str, user_id: &str) {
    let base = homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/user/{}/account_data/m.direct",
        url::form_urlencoded::byte_serialize(user_id.as_bytes()).collect::<String>()
    );
    match http::get_json(&url, access_token).await {
        Ok(content) => apply(state, account_id, &content),
        // A 404 is an account that has never had a DM, which is ordinary.
        Err(e) => tracing::debug!("matrix[{account_id}]: reading the direct list: {e:#}"),
    }
}

/// Makes an `m.direct` content this account's list, and moves any room
/// already on screen that it disagrees with.
///
/// The second half is the part that matters when the list changes rather than
/// when it is first read: a room's name and kind are derived once and cached
/// (see `rooms`), so a conversation that was already a buffer when Element
/// made it a DM would otherwise stay an ordinary room until a restart.
pub(super) fn apply(state: &AppState, account_id: &str, content: &Value) {
    let pairs = read(content);
    state.runtime.set_matrix_directs(account_id, pairs.clone());
    for (room_id, _) in pairs {
        let Some((name, kind)) = state.runtime.get_matrix_room_name(account_id, &room_id) else { continue };
        if kind == "dm" {
            continue;
        }
        state.runtime.set_matrix_room_name(account_id, &room_id, &name, "dm");
        if let Some(buffer_id) = state.runtime.matrix_buffer_for_room(account_id, &room_id) {
            state.runtime.set_buffer_kind(state, &buffer_id, "dm");
        }
    }
}

/// Tells the account - and therefore every other client it signs in from -
/// that this room is a direct message with this person.
///
/// Read-modify-write against the server's own copy rather than against what
/// is held here, because the list is replaced whole and another client may
/// have written it since. Best-effort: failing to record a DM is worth a
/// line in the log and not worth failing the conversation over.
pub(super) async fn record(state: &AppState, account_id: &str, peer_user_id: &str, room_id: &str) {
    let Some(account) = state.accounts.get_matrix(account_id) else { return };
    let existing = ssss::read_account_data(&account.homeserver_url, &account.access_token, &account.user_id, "m.direct")
        .await
        .unwrap_or_else(|| json!({}));
    let updated = with(&existing, peer_user_id, room_id);
    match ssss::write_account_data(&account.homeserver_url, &account.access_token, &account.user_id, "m.direct", updated.clone()).await {
        Ok(()) => apply(state, account_id, &updated),
        Err(e) => tracing::warn!("matrix[{account_id}]: recording {room_id} as a direct message: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_read_the_way_a_room_is_asked_about() {
        let content = json!({
            "@anna:poa.st": ["!one:poa.st", "!two:poa.st"],
            "@bob:poa.st": ["!three:poa.st"],
            // A person with no rooms is not a DM with anybody.
            "@carol:poa.st": [],
        });
        let mut pairs = read(&content);
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("!one:poa.st".to_string(), "@anna:poa.st".to_string()),
                ("!three:poa.st".to_string(), "@bob:poa.st".to_string()),
                ("!two:poa.st".to_string(), "@anna:poa.st".to_string()),
            ]
        );
        // An account that has never had one, and a server answering with
        // something else entirely, both mean "no DMs" rather than an error.
        assert!(read(&json!({})).is_empty());
        assert!(read(&json!("nonsense")).is_empty());
    }

    /// The list is replaced whole, so writing back only what this client
    /// knows would un-DM every other conversation on every other device.
    #[test]
    fn adding_a_room_keeps_everybody_elses() {
        let before = json!({ "@anna:poa.st": ["!one:poa.st"], "@bob:poa.st": ["!three:poa.st"] });
        let after = with(&before, "@anna:poa.st", "!new:poa.st");
        assert_eq!(after["@anna:poa.st"], json!(["!one:poa.st", "!new:poa.st"]));
        assert_eq!(after["@bob:poa.st"], json!(["!three:poa.st"]));

        // Somebody with no entry yet gets one.
        let fresh = with(&json!({}), "@carol:poa.st", "!c:poa.st");
        assert_eq!(fresh["@carol:poa.st"], json!(["!c:poa.st"]));

        // And a room already listed is not listed twice - which is what
        // happens every time "Open DM" is clicked on an existing
        // conversation.
        let again = with(&after, "@anna:poa.st", "!new:poa.st");
        assert_eq!(again["@anna:poa.st"], json!(["!one:poa.st", "!new:poa.st"]));
    }

    #[test]
    fn removing_a_room_takes_the_person_with_it_when_it_was_the_last_one() {
        let before = json!({ "@anna:poa.st": ["!one:poa.st", "!two:poa.st"], "@bob:poa.st": ["!three:poa.st"] });
        let after = without(&before, "!three:poa.st");
        assert_eq!(after["@anna:poa.st"], json!(["!one:poa.st", "!two:poa.st"]));
        // Not an empty list: a person with no rooms reads to other clients
        // as a conversation with nothing in it.
        assert!(after.get("@bob:poa.st").is_none());

        let one_left = without(&before, "!one:poa.st");
        assert_eq!(one_left["@anna:poa.st"], json!(["!two:poa.st"]));
    }
}
