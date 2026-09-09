//! Whether a channel is live, and who is following one that just went live.
//!
//! Kick's own site answers this by polling, and so does this - there is no
//! event for "somebody you follow started streaming" that arrives on a socket
//! nobody is subscribed to yet.

use super::*;

/// Tells the client what is on air in a channel, and remembers it so a window
/// opening later can be told the same thing.
///
/// Kick's own channel endpoint carries all of it and this backend was already
/// calling it - the numbers were parsed away and dropped.
pub(super) fn announce_stream(state: &AppState, buffer_id: &str, channel: &api::Channel, following: Option<bool>) {
    let stream = serde_json::json!({
        "bufferId": buffer_id,
        "live": channel.live.is_some(),
        "title": channel.live.as_ref().map(|l| l.title.clone()),
        "category": channel.live.as_ref().and_then(|l| l.category.clone()),
        "viewers": channel.live.as_ref().and_then(|l| l.viewers),
        "startedTs": channel.live.as_ref().and_then(|l| l.started_ts),
        "followers": channel.followers,
        // Where the picture is. On the stream event rather than fetched by
        // whoever wants to watch, because every frontend that shows video
        // needs it and the refresh already running is what replaces the
        // signed token before it expires.
        "playbackUrl": channel.playback_url,
        // Absent rather than false for an account that is not signed in:
        // "not following" and "cannot say" are different, and a button that
        // offers to follow when it cannot is a button that fails when pressed.
        "following": following,
    });
    state.runtime.set_kick_stream(buffer_id, stream.clone());
    state.events.emit("kickStream", stream);
}

/// Whether this account follows the channel, where it can say.
pub(super) async fn following_now(
    state: &AppState,
    http: &reqwest::Client,
    account_id: &str,
    slug: &str,
    known: Option<bool>,
) -> Option<bool> {
    // Nothing to ask with. This is the one honest "cannot say", and the
    // client draws it as an account that is not signed in.
    let Some(token) = state.accounts.get_kick(account_id).and_then(|c| c.token).filter(|t| !t.is_empty()) else {
        return None;
    };
    match api::standing(http, &token, slug).await {
        Ok(standing) => Some(standing.following),
        // A question that could not be asked is not an answer of "no idea".
        //
        // It used to be: any failure here became None, which is the value
        // that means signed out - so one rate-limited request drew the whole
        // account as logged out of Kick. Kick rate-limits this endpoint
        // readily, and every open window asking on its own minute is how that
        // happens, so this is a state the client reached routinely.
        Err(e) => {
            tracing::debug!("kick[{account_id}]: reading standing in {slug}: {e:#}");
            known
        }
    }
}

/// Re-reads whether this account follows a channel and says so.
///
/// Its own function because following is the one thing here a person changes
/// from this client - the rest of what a channel says about itself changes
/// because the streamer did something.
pub async fn refresh_standing(state: &AppState, buffer_id: &str) {
    let Some(channel) = state.runtime.kick_channel(buffer_id) else { return };
    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return };
    let Ok(http) = api::client() else { return };
    let known = state.runtime.kick_stream(buffer_id).and_then(|s| s["following"].as_bool());
    let following = following_now(state, &http, &buffer.account_id, &channel.slug, known).await;
    if let Some(mut stream) = state.runtime.kick_stream(buffer_id) {
        stream["following"] = serde_json::json!(following);
        state.runtime.set_kick_stream(buffer_id, stream.clone());
        state.events.emit("kickStream", stream);
    }
}

/// How recently a channel must have been asked about directly for a second ask
/// to be pointless.
///
/// Opening a client with thirty Kick channels subscribes to thirty buffers at
/// once, and each subscription used to become its own channel fetch; the poll
/// above covers all of them for the price of one request.
/// Just under the minute each open window asks on, so two windows watching
/// the same channel cost what one does rather than twice what one does.
pub(super) const DIRECT_REFRESH: Duration = Duration::from_secs(50);

/// Live state for everything this account follows, in one request.
///
/// Only channels this client actually has open are updated: the follow list
/// can be longer than the buffers, and a stream state for a buffer that does
/// not exist has nobody to tell.
pub(super) async fn refresh_followed_live(state: &AppState, http: &reqwest::Client, account_id: &str, token: &str) {
    let rows = match api::followed_live(http, token).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("kick[{account_id}]: polling live channels: {e:#}");
            return;
        }
    };
    for row in rows {
        let buffer_id = crate::model::buffer_id(account_id, &row.slug);
        // A channel with no chat connection is one this client does not have
        // open, whatever the follow list says.
        if state.runtime.kick_channel(&buffer_id).is_none() {
            continue;
        }
        let mut stream = state.runtime.kick_stream(&buffer_id).unwrap_or_else(|| {
            serde_json::json!({ "bufferId": buffer_id, "live": false })
        });
        let before = stream.clone();
        stream["live"] = serde_json::json!(row.live);
        stream["viewers"] = serde_json::json!(row.viewers);
        if row.live {
            // The full follow list carries no session title, so a title
            // already known from the channel itself is better than none.
            if let Some(title) = row.title {
                stream["title"] = serde_json::json!(title);
            }
            if let Some(category) = row.category {
                stream["category"] = serde_json::json!(category);
            }
        } else {
            stream["title"] = serde_json::Value::Null;
            stream["category"] = serde_json::Value::Null;
            stream["startedTs"] = serde_json::Value::Null;
        }
        // This list says nothing about whether the account follows the
        // channel - it is the follow list, so it does, and anything already
        // known stays as it is.
        if stream["following"].is_null() {
            stream["following"] = serde_json::json!(true);
        }
        if stream != before {
            state.runtime.set_kick_stream(&buffer_id, stream.clone());
            state.events.emit("kickStream", stream);
        }
    }
}

/// Asks Kick what a channel is doing now and says so.
///
/// Used when the answer has just changed - a stream starting or ending - and
/// when somebody opens the channel, since a viewer count from an hour ago is
/// worse than none.
pub async fn refresh_stream(state: &AppState, buffer_id: &str) {
    if !state.runtime.kick_stream_due(buffer_id, DIRECT_REFRESH) {
        return;
    }
    refresh_stream_now(state, buffer_id).await
}

/// The same, for a caller that has already decided the answer is stale - a
/// stream going on or off air, or the channel somebody is looking at.
pub async fn refresh_stream_now(state: &AppState, buffer_id: &str) {
    let Some(channel) = state.runtime.kick_channel(buffer_id) else { return };
    let Ok(http) = api::client() else { return };
    let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
    let known = state.runtime.kick_stream(buffer_id).and_then(|s| s["following"].as_bool());
    match api::channel(&http, &channel.slug).await {
        Ok(fresh) => {
            let following = following_now(state, &http, &account_id, &channel.slug, known).await;
            announce_stream(state, buffer_id, &fresh, following);
        }
        Err(e) => tracing::debug!("kick: refreshing {}: {e:#}", channel.slug),
    }
}

/// One line that keeps being rewritten rather than repeated.
///
/// A poll or a prediction is one thing happening over a minute or two, not a
/// stream of events - so it gets one message that changes, the way an edited
/// message does, and the chat around it stays readable.
pub(super) fn live_line(state: &AppState, account_id: &str, slug: &str, msg_id: &str, what: &str, kind: &str) {
    let buffer_id = crate::model::buffer_id(account_id, slug);
    if state.runtime.update_message(state, &buffer_id, msg_id, what, &[], &[]) {
        return;
    }
    state.runtime.record_message(
        state, account_id, slug, "channel", slug, what, false, kind, None, Some(msg_id.to_string()),
        false, None, Vec::new(), Vec::new(), None,
    );
}
