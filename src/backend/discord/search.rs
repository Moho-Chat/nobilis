//! Searching a server or a conversation, through Discord's own index.
//!
//! The filters are the ones its own search bar offers, translated from what
//! somebody typed. Dates become snowflakes because that is what the endpoint
//! takes - an id encodes its own timestamp, so a date range is an id range.

use super::*;

/// Everything a Discord search can be narrowed by.
///
/// One struct rather than a pile of arguments because the client sends them
/// as one thing - the filters somebody typed into the box - and because the
/// names here are the names Discord's own query string uses, so what is being
/// asked for stays readable at the call site.
#[derive(Default, Debug)]
pub struct SearchFilters {
    pub content: Option<String>,
    /// Names as typed. Resolved to ids here, where the conversations are.
    pub from: Option<String>,
    pub mentions: Option<String>,
    /// A channel name, for narrowing a guild-wide search to one channel.
    pub in_channel: Option<String>,
    /// link, embed, file, video, image, sound or sticker.
    pub has: Option<String>,
    /// Unix seconds. Discord takes these as snowflakes, which is a shift.
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub offset: i64,
}

/// Discord's epoch, and the shift that turns a moment into a snowflake.
///
/// Every id Discord issues carries its own timestamp in the high bits, which
/// is why a search can be bounded by date without a date parameter existing:
/// an id built from a moment sorts exactly where that moment does.
pub(super) const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

pub(super) fn snowflake_at(unix_secs: i64) -> String {
    (((unix_secs * 1000) - DISCORD_EPOCH_MS).max(0) << 22).to_string()
}

/// Searches a whole guild, or one conversation where there is no guild.
///
/// Discord's own search rather than this client's copy of one channel: the
/// point of searching a server is finding the thing somebody said in a
/// channel you were not reading, which is exactly what is not stored here.
///
/// Names are resolved to ids from what this account has already seen, because
/// that is what somebody types - "from:coty1911", not a snowflake. A name
/// nobody here has said anything under cannot be resolved, and that is
/// reported rather than quietly dropped: a filter that is ignored turns a
/// narrow search into a broad one and looks like the wrong answer.
pub async fn search_messages(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    filters: &SearchFilters,
) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    let guild_id = state.runtime.get_discord_guild(buffer_id);

    let mut query: Vec<(String, String)> = Vec::new();
    if let Some(content) = filters.content.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        query.push(("content".into(), content.to_string()));
    }
    // A guild search covers every channel in it, so "in:" is a narrowing
    // rather than the search itself; a DM has one channel and no choice.
    match &guild_id {
        Some(_) => {
            if let Some(name) = filters.in_channel.as_deref() {
                let wanted = resolve_channel(state, account_id, name)
                    .with_context(|| format!("no channel here called \"{name}\""))?;
                query.push(("channel_id".into(), wanted));
            }
        }
        None => {}
    }
    if let Some(name) = filters.from.as_deref() {
        query.push(("author_id".into(), resolve_person(state, account_id, name)?));
    }
    if let Some(name) = filters.mentions.as_deref() {
        query.push(("mentions".into(), resolve_person(state, account_id, name)?));
    }
    if let Some(has) = filters.has.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        query.push(("has".into(), has.to_lowercase()));
    }
    if let Some(before) = filters.before {
        query.push(("max_id".into(), snowflake_at(before)));
    }
    if let Some(after) = filters.after {
        query.push(("min_id".into(), snowflake_at(after)));
    }
    if filters.offset > 0 {
        query.push(("offset".into(), filters.offset.to_string()));
    }
    if query.is_empty() {
        bail!("say what to search for");
    }

    let url = match &guild_id {
        Some(guild) => format!("{API_BASE}/guilds/{guild}/messages/search"),
        None => format!("{API_BASE}/channels/{channel_id}/messages/search"),
    };
    let resp = http_client()
        .get(url)
        .query(&query)
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("searching")?;
    // Discord answers a search of a server it is still indexing with 202 and
    // an empty result, which is not an error and is worth saying out loud -
    // silence here reads as "nothing was found".
    if resp.status() == reqwest::StatusCode::ACCEPTED {
        return Ok(json!({ "results": [], "total": 0, "indexing": true, "channelNames": {} }));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "searching"));
    }
    let answer: Value = resp.json().await.context("reading the search results")?;

    // Every hit comes back with the messages either side of it for context;
    // the hit itself is the one flagged, and the rest are not results.
    let mut results = Vec::new();
    let mut channel_names = serde_json::Map::new();
    for group in answer["messages"].as_array().cloned().unwrap_or_default() {
        let Some(hit) = group
            .as_array()
            .and_then(|g| g.iter().find(|m| m["hit"].as_bool() == Some(true)).or_else(|| g.first()))
        else {
            continue;
        };
        let channel = hit["channel_id"].as_str().unwrap_or_default();
        let where_said = state
            .runtime
            .discord_buffer_for_channel(account_id, channel)
            .unwrap_or_else(|| buffer_id.to_string());
        if let Some(buffer) = state.runtime.get_buffer(&where_said) {
            channel_names.insert(where_said.clone(), json!(buffer.name));
        }
        results.push(message_summary(state, &where_said, hit, &cfg));
    }

    Ok(json!({
        "results": results,
        "total": answer["total_results"].as_i64().unwrap_or(results.len() as i64),
        "indexing": false,
        "channelNames": Value::Object(channel_names),
    }))
}

/// A name as somebody typed it, as the id Discord wants.
pub(super) fn resolve_person(state: &AppState, account_id: &str, name: &str) -> Result<String> {
    let name = name.trim().trim_start_matches('@');
    // Already an id, which is what a copied one looks like.
    if name.len() >= 17 && name.chars().all(|c| c.is_ascii_digit()) {
        return Ok(name.to_string());
    }
    state
        .store
        .sender_id_by_nick(account_id, name)
        .ok()
        .flatten()
        .with_context(|| format!("nobody called \"{name}\" has said anything moho has seen"))
}

/// A channel name as somebody typed it, as its id.
pub(super) fn resolve_channel(state: &AppState, account_id: &str, name: &str) -> Option<String> {
    let wanted = name.trim().trim_start_matches('#').to_lowercase();
    if wanted.len() >= 17 && wanted.chars().all(|c| c.is_ascii_digit()) {
        return Some(wanted);
    }
    let prefix = format!("{account_id}|");
    state
        .runtime
        .list_buffers()
        .into_iter()
        .filter(|b| b.id.starts_with(&prefix))
        .find(|b| {
            b.name
                .rsplit('#')
                .next()
                .map(|channel| channel.to_lowercase() == wanted)
                .unwrap_or(false)
        })
        .and_then(|b| state.runtime.get_discord_channel(&b.id))
}

/// One Discord message in the shape this client's own messages have.
pub(super) fn message_summary(state: &AppState, buffer_id: &str, msg: &Value, cfg: &crate::accounts::DiscordAccountConfig) -> Value {
    let author = &msg["author"];
    let from = author["global_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| author["username"].as_str())
        .unwrap_or("unknown");
    let body = resolve_mentions(&extract_body(msg).unwrap_or_default(), msg, &cfg.user_id, cfg.display_name.as_deref());
    let ts = msg["timestamp"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(0);
    let _ = state;
    json!({
        "id": msg["id"].as_str().unwrap_or_default(),
        "bufferId": buffer_id,
        "from": from,
        "body": if body.trim().is_empty() { describe_wordless(msg) } else { body },
        "ts": ts,
        "kind": "chat",
    })
}
