//! Reading backwards: the parts of a conversation that happened earlier.
//!
//! Three shapes of the same thing - filling a buffer that was just opened,
//! catching up one that was open before the connection dropped, and jumping
//! to a message somebody went looking for.

use super::*;

/// Fires off history backfill for a batch of newly-registered buffers as a
/// background task, sequentially with a small delay between requests - a
/// guild can easily have 30+ channels (confirmed live), and firing that
/// many REST requests at once risks Discord's rate limiter; a one-time
/// startup cost taking a few extra seconds in the background is a fine
/// trade for not tripping it.
pub(super) fn spawn_backfill(state: AppState, token: String, user_id: String, display_name: Option<String>, targets: Vec<(String, String)>) {
    if targets.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for (buffer_id, channel_id) in targets {
            backfill_channel_history(&state, &token, &user_id, display_name.as_deref(), &buffer_id, &channel_id).await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

/// Backfills up to 50 recent messages for a channel/DM with no scrollback
/// yet. Unlike IRC (which has no server-side history at all - nobilis only
/// ever knows what it's personally seen live), Discord actually retains
/// and exposes history, so starting every buffer blank here would look
/// broken compared to Discord's own client. Seed-once, not sync: a buffer
/// that already has *any* stored messages (from an earlier backfill or
/// from live traffic already recorded) is left alone - there's no
/// per-message dedup against Discord's own message ids in this store, so
/// re-fetching on every reconnect would just duplicate rows.
///
/// Deliberately bypasses Runtime::record_message - that path also emits
/// the live "message" broadcast event and (for DMs/highlights) a
/// "notification" event, neither of which should fire for messages from
/// potentially weeks ago just because this is the first time nobilis has
/// seen the channel.
pub(super) async fn backfill_channel_history(state: &AppState, token: &str, user_id: &str, own_display_name: Option<&str>, buffer_id: &str, channel_id: &str) {
    match state.store.has_messages(buffer_id) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            tracing::warn!("discord: checking existing history for {buffer_id}: {e}");
            return;
        }
    }

    let resp = match http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/messages"))
        .query(&[("limit", "50")])
        .header("Authorization", token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("discord: fetching history for {buffer_id}: {e}");
            return;
        }
    };
    if !resp.status().is_success() {
        // Missing READ_MESSAGE_HISTORY, rate-limited, etc. - not worth
        // treating as an error, just leaves that buffer without backfill.
        return;
    }
    let messages: Vec<Value> = match resp.json().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("discord: parsing history for {buffer_id}: {e}");
            return;
        }
    };
    store_history_messages(state, buffer_id, &messages, user_id, own_display_name);
    state.runtime.refresh_buffer_activity(state, buffer_id);
}

/// Extends a buffer's history further back using Discord's own message
/// pagination (`before=<oldest known message id>`) - the initial backfill
/// (see backfill_channel_history) only ever seeds the most recent 50, so
/// this is what lets scrolling all the way up in a long-lived channel/DM
/// keep going instead of hitting a wall. A no-op if nothing is stored yet
/// (the initial backfill owns that case - there's no "oldest" to page
/// before) or if a request for this same buffer is already in flight
/// (getBacklog can be called concurrently by more than one connected
/// client - see the multi-screen-instance lesson from the QR login bug).
/// Re-reads a channel's recent history and stores only what is missing.
///
/// Unlike extend_history this deliberately re-reads a range already stored,
/// so it exists to repair scrollback rather than to deepen it: a message that
/// Discord sent but that was never written locally (dropped by a storage bug,
/// or missed while the daemon was down) has no other way back. Everything
/// already present is left untouched, so it is safe to run repeatedly.
///
/// Returns how many messages were recovered.
pub async fn refill_history(state: &AppState, buffer_id: &str, limit: u32) -> Result<usize> {
    let buffer = state.runtime.get_buffer(buffer_id).context("no such buffer")?;
    let config = state.accounts.get_discord(&buffer.account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this buffer")?;

    let resp = http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/messages"))
        .query(&[("limit", limit.clamp(1, 100).to_string().as_str())])
        .header("Authorization", &config.token)
        .send()
        .await
        .context("re-reading history")?;
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Honour Discord's own pacing rather than guessing at a backoff.
        let wait = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["retry_after"].as_f64())
            .unwrap_or(5.0);
        bail!("rate limited, retry after {wait:.1}s");
    }
    if !resp.status().is_success() {
        // No READ_MESSAGE_HISTORY on this channel is a normal outcome for a
        // sweep across every buffer, not a failure worth stopping for.
        bail!("Discord refused the request ({})", resp.status());
    }
    let messages: Vec<Value> = resp.json().await.context("parsing re-read history")?;

    let ids: Vec<&str> = messages.iter().filter_map(|m| m["id"].as_str()).collect();
    let known = state.store.existing_msg_ids(buffer_id, &ids)?;
    let missing: Vec<Value> = messages
        .into_iter()
        .filter(|m| m["id"].as_str().is_some_and(|id| !known.contains(id)))
        .collect();
    if missing.is_empty() {
        return Ok(0);
    }

    // Same storage path as any other history read, so recovered messages get
    // the current guard and preview caching rather than a parallel copy.
    store_history_messages(state, buffer_id, &missing, &config.user_id, config.display_name.as_deref());
    state.runtime.refresh_buffer_activity(state, buffer_id);
    Ok(missing.len())
}

/// Asks every open conversation what it missed, one at a time.
///
/// Spaced out deliberately: this runs right after connecting, alongside
/// everything else a fresh connection does, and a burst of requests is the
/// quickest way to be rate-limited by a service that has just let us in.
pub(super) fn spawn_dm_catch_up(state: AppState, config: DiscordAccountConfig, channels: HashMap<String, (String, String)>) {
    tokio::spawn(async move {
        let account_id = config.account_id();
        let dms: Vec<(String, String)> = channels
            .iter()
            .filter(|(_, (_, kind))| kind == "dm")
            .filter_map(|(channel_id, (name, _))| {
                let buffer = state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && &b.name == name)?;
                Some((buffer.id, channel_id.clone()))
            })
            .collect();

        for (buffer_id, channel_id) in dms {
            catch_up_channel(&state, &config.token, &config.user_id, config.display_name.as_deref(), &buffer_id, &channel_id).await;
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
    });
}

/// Fetches what was said in a channel while nobody was listening.
///
/// The daemon only runs while the client does, so every exit is a gap: the
/// gateway replays nothing on reconnect, and the existing backfill deliberately
/// skips any channel that already has scrollback. Without this, messages that
/// arrived overnight simply never appeared - the channel looked exactly as it
/// did when the app was closed.
///
/// Asks for what came *after* the newest message already stored, which is
/// bounded by how long the gap was rather than by how much history exists.
pub async fn catch_up_channel(
    state: &AppState,
    token: &str,
    user_id: &str,
    own_display_name: Option<&str>,
    buffer_id: &str,
    channel_id: &str,
) {
    if !state.runtime.try_start_discord_history_fetch(buffer_id) {
        return;
    }
    let result: Result<()> = async {
        // Nothing stored means this is a first sight, which the initial
        // backfill already covers.
        let Some(after_id) = state.store.newest_msg_id(buffer_id)? else { return Ok(()) };
        let resp = http_client()
            .get(format!("{API_BASE}/channels/{channel_id}/messages"))
            .query(&[("limit", "50"), ("after", after_id.as_str())])
            .header("Authorization", token)
            .send()
            .await
            .context("fetching missed messages")?;
        if resp.status().is_success() {
            let messages: Vec<Value> = resp.json().await.context("parsing missed messages")?;
            if !messages.is_empty() {
                tracing::info!("discord: {} message(s) missed in {buffer_id}", messages.len());
            }
            store_history_messages(state, buffer_id, &messages, user_id, own_display_name);
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!("discord: catching up {buffer_id}: {e}");
    }
    state.runtime.finish_discord_history_fetch(buffer_id);
}

pub async fn extend_history(state: &AppState, token: &str, user_id: &str, own_display_name: Option<&str>, buffer_id: &str, channel_id: &str) {
    if !state.runtime.try_start_discord_history_fetch(buffer_id) {
        return;
    }
    let result: Result<()> = async {
        let Some(before_id) = state.store.oldest_msg_id(buffer_id)? else { return Ok(()) };
        let resp = http_client()
            .get(format!("{API_BASE}/channels/{channel_id}/messages"))
            .query(&[("limit", "50"), ("before", before_id.as_str())])
            .header("Authorization", token)
            .send()
            .await
            .context("fetching more history")?;
        if resp.status().is_success() {
            let messages: Vec<Value> = resp.json().await.context("parsing more history")?;
            store_history_messages(state, buffer_id, &messages, user_id, own_display_name);
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!("discord: extending history for {buffer_id}: {e}");
    }
    state.runtime.finish_discord_history_fetch(buffer_id);
}

/// Reads forward from a message, for a reader who arrived in the middle of a
/// conversation and is now working their way back towards the present.
///
/// The counterpart of `extend_history`, which only ever reads the other way.
/// Both exist because a jump leaves a hole: what was fetched around the
/// message ends somewhere, the recent conversation begins somewhere later,
/// and scrolling down from the first to the second used to cross that hole
/// without saying so and without filling it.
///
/// Returns how many messages were stored, so a caller can tell the difference
/// between "here is more" and "there is no more".
pub async fn load_newer(state: &AppState, account_id: &str, buffer_id: &str, message_id: &str) -> Result<usize> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    let resp = http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/messages"))
        .query(&[("limit", "50"), ("after", message_id)])
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("reading the rest of the channel")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "reading the rest of the channel"));
    }
    let messages: Vec<Value> = resp.json().await.context("reading the rest of the channel")?;
    store_history_messages(state, buffer_id, &messages, &cfg.user_id, cfg.display_name.as_deref());
    Ok(messages.len())
}

pub async fn load_context(state: &AppState, account_id: &str, buffer_id: &str, message_id: &str) -> Result<i64> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    let resp = http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/messages"))
        .query(&[("limit", "50"), ("around", message_id)])
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("reading that part of the channel")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "reading that part of the channel"));
    }
    let messages: Vec<Value> = resp.json().await.context("reading that part of the channel")?;
    store_history_messages(state, buffer_id, &messages, &cfg.user_id, cfg.display_name.as_deref());

    messages
        .iter()
        .find(|m| m["id"].as_str() == Some(message_id))
        .and_then(|m| m["timestamp"].as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
        .context("Discord did not return that message")
}
