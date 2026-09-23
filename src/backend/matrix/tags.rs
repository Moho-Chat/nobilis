//! What the account has filed a room under.
//!
//! `m.tag` is per-room account data, and it is how Element sorts a room list:
//! `m.favourite` to the top, `m.lowpriority` to the bottom. Both travel with
//! the account, so somebody who has spent an evening organising their rooms
//! in one client expects to open another and find it organised.
//!
//! Only those two are acted on. The spec allows arbitrary user tags
//! (`u.holidays`), which are somebody else's filing system and mean nothing
//! to a client that cannot show them - they are left alone rather than
//! dropped, since the tag list is replaced whole and writing back only the
//! two understood here would quietly delete the rest.

use super::*;

/// The spec's two sorting tags, spelled the way the spec spells them.
///
/// `m.lowpriority` has no dot in the middle. Guessing at `m.low_priority` is
/// a tag no other client reads and a room that stays where it was.
pub const FAVOURITE: &str = "m.favourite";
pub const LOW_PRIORITY: &str = "m.lowpriority";

/// The tag that marks the server notices room.
///
/// Not a sorting tag and not something anybody sets: the homeserver puts it
/// there. It is how a client knows that a room is the server talking to this
/// account - a terms-of-service change, a quota, an account restriction - and
/// without it that room is an ordinary one from a sender with no particular
/// standing. Element marks it and refuses to let it be left, because the
/// server refuses too.
pub const SERVER_NOTICE: &str = "m.server_notice";

/// Whether a room is starred, and whether it is pushed down.
pub fn read(content: &Value) -> (bool, bool) {
    let tags = &content["tags"];
    (tags.get(FAVOURITE).is_some(), tags.get(LOW_PRIORITY).is_some())
}

/// Whether this is the room the homeserver itself talks in.
pub fn is_server_notices(content: &Value) -> bool {
    content["tags"].get(SERVER_NOTICE).is_some()
}

/// Applies a room's `m.tag` to the buffer it belongs to.
pub(super) fn apply(state: &AppState, account_id: &str, room_id: &str, content: &Value) {
    let Some(buffer_id) = state.runtime.matrix_buffer_for_room(account_id, room_id) else { return };
    let (favourite, low_priority) = read(content);
    state.runtime.set_buffer_tags(state, &buffer_id, favourite, low_priority);
    if is_server_notices(content) {
        state.runtime.set_buffer_service_room(state, &buffer_id);
    }
}

/// Reads every room's tags at connect.
///
/// The same shape as the receipt and invite fetches beside it, and for the
/// same reason: a resumed session is told only what has changed since its
/// cursor, and a tag set months ago has not changed. Without this, an account
/// that had organised its rooms elsewhere would see that organisation only
/// after somebody happened to re-tag a room.
///
/// One sync rather than a request per room - a `/user/.../rooms/.../tags` for
/// each of eighty rooms is eighty requests to learn that four of them are
/// favourites.
pub(super) async fn fetch(state: &AppState, account_id: &str, homeserver_url: &str, access_token: &str) -> Result<()> {
    let filter = serde_json::json!({
        // Nothing but the account data. A timeline limit of zero is not
        // something every homeserver will agree to, hence one.
        "room": { "timeline": { "limit": 1 }, "state": { "types": [] }, "ephemeral": { "types": [] } },
        "presence": { "types": [] }
    })
    .to_string();
    let url = format!(
        "{}/_matrix/client/v3/sync?timeout=0&filter={}",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(filter.as_bytes()).collect::<String>()
    );
    let resp = http::get_json(&url, access_token).await.context("tag sync")?;
    let Some(joined) = resp["rooms"]["join"].as_object() else { return Ok(()) };
    for (room_id, room) in joined {
        for event in room["account_data"]["events"].as_array().into_iter().flatten() {
            if event["type"].as_str() == Some("m.tag") {
                apply(state, account_id, room_id, &event["content"]);
            }
        }
    }
    Ok(())
}

/// Adds or removes one tag on one room.
///
/// One tag at a time, which is what the endpoint offers - and it is also the
/// right granularity: the tag list is shared with every other client, and
/// replacing it whole to change one entry would drop tags this client does
/// not understand.
pub async fn set(state: &AppState, account_id: &str, buffer_id: &str, tag: &str, on: bool) -> Result<()> {
    if tag != FAVOURITE && tag != LOW_PRIORITY {
        anyhow::bail!("{tag} is not a tag this client sets");
    }
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let room_id = state.runtime.get_matrix_room(buffer_id).context("no room for this conversation")?;
    let url = format!(
        "{}/_matrix/client/v3/user/{}/rooms/{}/tags/{}",
        account.homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>(),
        url::form_urlencoded::byte_serialize(tag.as_bytes()).collect::<String>(),
    );
    if on {
        // An order is optional and this client does not offer one - the rooms
        // inside a band keep the recency order they already had. Sending a
        // middling one rather than none so a client that *does* sort by it
        // has something to sort by.
        http::put_json(&url, &account.access_token, serde_json::json!({ "order": 0.5 })).await.context("adding the tag")?;
    } else {
        http::delete_json(&url, &account.access_token).await.context("removing the tag")?;
    }

    // Shown now rather than when the next sync says so. The server echoes the
    // change back as room account data, which re-applies the same answer -
    // this only means the rail moves when the menu item is clicked.
    let (mut favourite, mut low_priority) = state.runtime.buffer_tags(buffer_id);
    if tag == FAVOURITE {
        favourite = on;
    } else {
        low_priority = on;
    }
    state.runtime.set_buffer_tags(state, buffer_id, favourite, low_priority);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_room_is_read_as_starred_or_pushed_down() {
        let starred = json!({ "tags": { "m.favourite": { "order": 0.1 } } });
        assert_eq!(read(&starred), (true, false));

        let quiet = json!({ "tags": { "m.lowpriority": {} } });
        assert_eq!(read(&quiet), (false, true));

        // Somebody else's filing system is not one of ours, and is not an
        // error either.
        let theirs = json!({ "tags": { "u.holidays": { "order": 0.2 } } });
        assert_eq!(read(&theirs), (false, false));

        // A room with no tags, and content that is not what was expected.
        assert_eq!(read(&json!({ "tags": {} })), (false, false));
        assert_eq!(read(&json!({})), (false, false));
    }

    /// The spelling is the whole of this. `m.lowpriority` has no dot in the
    /// middle, and a client writing `m.low_priority` writes a tag nothing
    /// else reads.
    #[test]
    fn the_tags_are_spelled_the_way_the_spec_spells_them() {
        assert_eq!(FAVOURITE, "m.favourite");
        assert_eq!(LOW_PRIORITY, "m.lowpriority");
        assert_eq!(read(&json!({ "tags": { "m.low_priority": {} } })), (false, false));
    }

    /// The homeserver's own room, which it marks itself. Not a sorting tag
    /// and not one anybody sets.
    #[test]
    fn the_servers_own_room_is_marked_by_the_server() {
        let notices = json!({ "tags": { "m.server_notice": {} } });
        assert!(is_server_notices(&notices));
        // And it is not a favourite or a low priority by being one.
        assert_eq!(read(&notices), (false, false));

        assert!(!is_server_notices(&json!({ "tags": { "m.favourite": {} } })));
        assert!(!is_server_notices(&json!({})));
    }
}
