//! The emoji this account reached for last.
//!
//! Kept in account data, so it follows the person between clients: the list
//! is the one small piece of a picker that is worth carrying, because it is
//! the part earned by use. moho's picker kept its own list in the window's
//! local storage and started the same way on every machine.
//!
//! `io.element.recent_emoji` rather than anything stable. There is no
//! standardised event for this and Element's is what every other client
//! reads, so writing something of our own would produce a list that travels
//! nowhere - which is the entire point of not keeping it locally.
//!
//! The stored shape is a list of `[emoji, count]` pairs, most recent first.
//! The count is Element's and is kept rather than used: dropping it would
//! silently reset the ordering of somebody's list in their other client.

use super::*;

pub const EVENT: &str = "io.element.recent_emoji";

/// The list, most recent first, as the picker wants it.
pub fn read(content: &Value) -> Vec<String> {
    content["recent_emoji"]
        .as_array()
        .into_iter()
        .flatten()
        // Each entry is a `[emoji, count]` pair. Anything else is somebody
        // else's idea of the format and is skipped rather than guessed at.
        .filter_map(|pair| pair.get(0).and_then(|e| e.as_str()))
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect()
}

/// How many to keep.
///
/// Element's own ceiling. A longer list is not a better one - the row is
/// read at a glance, and an emoji used once three weeks ago is noise in it.
const KEEP: usize = 24;

/// The list with one emoji moved to the front, its count carried along.
pub fn used(content: &Value, emoji: &str) -> Value {
    let mut pairs: Vec<(String, i64)> = content["recent_emoji"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pair| {
            let name = pair.get(0)?.as_str()?.to_string();
            Some((name, pair.get(1).and_then(|c| c.as_i64()).unwrap_or(1)))
        })
        .collect();

    // Taken out and put back at the front, so the count survives rather than
    // restarting at one every time somebody uses an emoji they already had.
    let count = pairs
        .iter()
        .position(|(name, _)| name == emoji)
        .map(|at| pairs.remove(at).1)
        .unwrap_or(0);
    pairs.insert(0, (emoji.to_string(), count + 1));
    pairs.truncate(KEEP);

    serde_json::json!({
        "recent_emoji": pairs.into_iter().map(|(name, count)| serde_json::json!([name, count])).collect::<Vec<_>>()
    })
}

/// Reads the account's list.
pub async fn list(state: &AppState, account_id: &str) -> Result<Vec<String>> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let content = ssss::read_account_data(&account.homeserver_url, &account.access_token, &account.user_id, EVENT)
        .await
        // An account that has never picked one has no event, which is an
        // empty list rather than a failure.
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(read(&content))
}

/// Records one, and hands back the list it leaves behind.
///
/// Read-modify-write against the server's copy rather than against anything
/// held here, for the reason every account-data write in this backend does
/// it: the event is replaced whole and another client may have written it
/// since.
pub async fn record(state: &AppState, account_id: &str, emoji: &str) -> Result<Vec<String>> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let existing = ssss::read_account_data(&account.homeserver_url, &account.access_token, &account.user_id, EVENT)
        .await
        .unwrap_or_else(|| serde_json::json!({}));
    let updated = used(&existing, emoji);
    ssss::write_account_data(&account.homeserver_url, &account.access_token, &account.user_id, EVENT, updated.clone())
        .await
        .context("recording the emoji")?;
    Ok(read(&updated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_list_comes_back_most_recent_first() {
        let content = json!({ "recent_emoji": [["\u{1F44D}", 12], ["\u{1F600}", 3]] });
        assert_eq!(read(&content), vec!["\u{1F44D}", "\u{1F600}"]);

        // An account that has never picked one, and an event somebody else's
        // client wrote in a shape this does not know.
        assert!(read(&json!({})).is_empty());
        assert!(read(&json!({ "recent_emoji": ["just a string"] })).is_empty());
        assert!(read(&json!({ "recent_emoji": "nonsense" })).is_empty());
    }

    /// The count is Element's and is carried rather than used. Restarting it
    /// at one on every pick would quietly reorder somebody's list in the
    /// client that does sort by it.
    #[test]
    fn using_one_again_moves_it_up_and_keeps_its_count() {
        let before = json!({ "recent_emoji": [["a", 12], ["b", 3], ["c", 1]] });
        let after = used(&before, "b");
        assert_eq!(after["recent_emoji"], json!([["b", 4], ["a", 12], ["c", 1]]));

        // A new one starts at one and goes to the front.
        let fresh = used(&after, "d");
        assert_eq!(fresh["recent_emoji"][0], json!(["d", 1]));
        assert_eq!(read(&fresh), vec!["d", "b", "a", "c"]);

        // From nothing at all.
        assert_eq!(used(&json!({}), "z")["recent_emoji"], json!([["z", 1]]));
    }

    /// A row read at a glance, not an archive. Element's own ceiling.
    #[test]
    fn the_list_does_not_grow_without_end() {
        let mut content = json!({});
        for i in 0..40 {
            content = used(&content, &format!("e{i}"));
        }
        assert_eq!(read(&content).len(), KEEP);
        // And it is the newest that survive.
        assert_eq!(read(&content)[0], "e39");
    }
}
