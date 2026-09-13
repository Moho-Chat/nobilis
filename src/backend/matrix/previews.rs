//! What a link in a message turns out to be.
//!
//! The homeserver does the fetching, which is the part worth having. Element
//! works this way and so does this: the preview costs the reader no request
//! to a stranger's server, which is the opposite trade from this client's own
//! content sniffing - there, the reader's own machine fetches the page.
//!
//! Never in an encrypted room. Asking the homeserver to unfurl a link handed
//! to it in plaintext gives it a URL that room went to some trouble to keep
//! from it, and no setting here turns that back on: a room is encrypted
//! because its contents are nobody else's business, and a preview is not
//! worth undoing that for. Element defaults the same way; this does not
//! offer the override.
//!
//! Two switches are honoured beside that, both spelled the Matrix way: the
//! account's `org.matrix.preview_urls`, and the room's own
//! `m.room.preview_urls` state - a room that has turned previews off must not
//! have them, whatever the account says.

use super::*;

/// The room state event that turns previews off for everybody in a room.
pub const ROOM_SETTING: &str = "m.room.preview_urls";

/// The account's own switch. Still under its `org.matrix.` prefix, which is
/// where every client that reads it looks.
pub const ACCOUNT_SETTING: &str = "org.matrix.preview_urls";

/// Whether a `disable` flag says to stop.
///
/// The events say "disable" rather than "enable", so an absent or unreadable
/// setting means previews are on - which is what every client does with one.
pub fn disabled_by(content: &Value) -> bool {
    content["disable"].as_bool().unwrap_or(false)
}

/// Whether a link in this room should be unfurled at all.
pub fn allowed(state: &AppState, account_id: &str, buffer_id: &str, room_id: &str) -> bool {
    if state.runtime.is_matrix_room_encrypted(buffer_id) {
        return false;
    }
    state.runtime.matrix_previews_allowed(account_id, room_id)
}

/// Reads the account's own switch at connect.
///
/// Asked for directly rather than waited for, like every other piece of
/// account data here: a resumed session is told only what changed since its
/// cursor, and a preference set a year ago has not changed.
pub(super) async fn fetch_account_setting(state: &AppState, account_id: &str, homeserver_url: &str, access_token: &str, user_id: &str) {
    let content = ssss::read_account_data(homeserver_url, access_token, user_id, ACCOUNT_SETTING)
        .await
        .unwrap_or_else(|| serde_json::json!({}));
    state.runtime.set_matrix_previews_off(account_id, "", disabled_by(&content));
}

/// The first link in a message body, if it has one.
///
/// One rather than all of them. A message with six links would mean six
/// requests and six cards under one line, and the card is a glance at what
/// somebody posted rather than an index of it - Element shows one too.
pub fn first_link(body: &str) -> Option<&str> {
    body.split_whitespace()
        .find(|word| word.starts_with("https://") || word.starts_with("http://"))
        // Trailing punctuation belongs to the sentence, not to the URL:
        // "look at https://example.org." is a link to example.org.
        .map(|word| word.trim_end_matches(|c| matches!(c, '.' | ',' | ')' | ']' | '!' | '?' | ';' | ':' | '"' | '\'')))
        .filter(|url| url.len() > "https://".len())
}

/// Asks the homeserver what a link is.
///
/// Quiet on every failure. A server with previews switched off, a page that
/// does not answer, a link to something that is not a page - none of those
/// are worth telling a reader about, because the link itself is still there
/// and still works. A missing card says "no preview"; an error message where
/// a card should be says "this client is broken".
pub async fn fetch(state: &AppState, account_id: &str, url: &str, ts_ms: i64) -> Option<crate::model::Embed> {
    let account = state.accounts.get_matrix(account_id)?;
    let asked = format!(
        "{}/_matrix/client/v1/media/preview_url?url={}&ts={ts_ms}",
        account.homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>()
    );
    let answer = http::get_json(&asked, &account.access_token).await.ok()?;

    let text = |key: &str| answer[key].as_str().filter(|v| !v.is_empty()).map(str::to_string);
    let title = text("og:title");
    let description = text("og:description");
    // The picture comes back as an mxc URI - the homeserver fetched it and
    // kept a copy, so it needs the same token-bearing download everything
    // else here does and cannot be handed to a frontend as it stands.
    let image = match answer["og:image"].as_str().filter(|v| v.starts_with("mxc://")) {
        Some(mxc) => {
            let ext = extension_for_mimetype(answer["og:image:type"].as_str().unwrap_or(""));
            cached_media_path(&account.homeserver_url, &account.access_token, mxc, ext).await
        }
        None => None,
    };

    // A card with nothing on it is not a card. Servers answer `{}` for a page
    // they could not read, and drawing an empty box under the message would
    // be worse than drawing nothing.
    if title.is_none() && description.is_none() && image.is_none() {
        return None;
    }
    Some(crate::model::Embed {
        title,
        description,
        color: None,
        timestamp: None,
        url: Some(url.to_string()),
        image_url: image,
    })
}

/// Unfurls a message's link and puts the card on it, if there is one.
///
/// After the message is stored rather than before: the line is what somebody
/// is waiting for, and a homeserver fetching somebody else's slow page is not
/// something to hold a conversation up for. Spawned for the same reason.
pub(super) fn unfurl_later(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    room_id: &str,
    event_id: &str,
    body: &str,
    ts_ms: i64,
) {
    if !allowed(state, account_id, buffer_id, room_id) {
        return;
    }
    let Some(link) = first_link(body) else { return };
    let (state, account_id, buffer_id, event_id, body, link) = (
        state.clone(),
        account_id.to_string(),
        buffer_id.to_string(),
        event_id.to_string(),
        body.to_string(),
        link.to_string(),
    );
    tokio::spawn(async move {
        let Some(embed) = fetch(&state, &account_id, &link, ts_ms).await else { return };
        // The body again, unchanged: update_message takes the whole message
        // and this only means to add the card to it.
        state.runtime.update_message(&state, &buffer_id, &event_id, &body, &[embed], &[]);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One link, and the sentence's punctuation left out of it.
    #[test]
    fn the_first_link_is_the_one_that_gets_a_card() {
        assert_eq!(first_link("look at https://example.org/page"), Some("https://example.org/page"));
        // The full stop ends the sentence, not the address.
        assert_eq!(first_link("see https://example.org."), Some("https://example.org"));
        assert_eq!(first_link("(https://example.org/a)"), Some("(https://example.org/a"));
        // First of several: a card is a glance at what somebody posted, not
        // an index of it.
        assert_eq!(first_link("https://one.example https://two.example"), Some("https://one.example"));

        assert_eq!(first_link("nothing here"), None);
        assert_eq!(first_link("mailto:someone@example.org"), None);
        // A bare scheme is not a link.
        assert_eq!(first_link("https://"), None);
    }

    /// The events say "disable", so silence means previews are on - which is
    /// what every client does with one.
    #[test]
    fn nothing_said_leaves_previews_on() {
        assert!(!disabled_by(&serde_json::json!({})));
        assert!(!disabled_by(&serde_json::json!({ "disable": false })));
        assert!(disabled_by(&serde_json::json!({ "disable": true })));
        // Something that is not a boolean is not a refusal.
        assert!(!disabled_by(&serde_json::json!({ "disable": "yes" })));
    }
}
