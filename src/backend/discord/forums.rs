//! Forums: a channel that is a list of posts, each post a thread.
//!
//! Discord's own client reads them from the thread-search endpoint, which hands
//! back each post with its first message in the same answer - the title, the
//! words under it, a picture, the reactions - so one request draws a page of the
//! list. A post is opened as the thread it is (see `open_thread`), and posting is
//! one request that makes the thread and its first message together.

use super::*;

/// How many posts one page holds.
const PAGE: usize = 25;

/// Discord's epoch for snowflake ids, in milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// When a snowflake was made, in Unix seconds. A thread's id is the id of its
/// first message, so this is when the post was made.
fn snowflake_secs(id: &str) -> i64 {
    id.parse::<u64>().map(|n| (((n >> 22) + DISCORD_EPOCH_MS) / 1000) as i64).unwrap_or(0)
}

/// The words of a first message, cut to what a card shows.
fn teaser(content: &str) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(300).collect();
    if flat.chars().count() > 300 {
        out.push('…');
    }
    out
}

/// A picture to put on the post's card: the first image attached to its first
/// message, else the picture of the first link in it.
fn post_thumbnail(first: &Value) -> Option<String> {
    let attached = extract_attachments(first).into_iter().find(|a| a.kind == "image" || a.kind == "video");
    if let Some(a) = attached {
        // A video has no still of its own here; Discord's proxy serves one.
        if a.kind == "image" {
            return a.url;
        }
    }
    first["embeds"]
        .as_array()?
        .iter()
        .find_map(|e| e["thumbnail"]["url"].as_str().or_else(|| e["image"]["url"].as_str()).map(str::to_string))
}

/// One post, from the thread and the first message that came with it.
fn post_card(thread: &Value, first: Option<&Value>, tags: &[Value], own_user_id: &str, own_name: Option<&str>) -> Value {
    let id = thread["id"].as_str().unwrap_or_default();
    let applied: Vec<&str> = thread["applied_tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).collect();
    let tag_names: Vec<Value> = applied
        .iter()
        .filter_map(|id| tags.iter().find(|t| t["id"].as_str() == Some(id)))
        .map(|t| {
            json!({
                "name": t["name"].as_str().unwrap_or(""),
                "emoji": t["emoji_name"].as_str(),
            })
        })
        .collect();
    let author = first
        .and_then(|m| m["author"]["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| m["author"]["username"].as_str()))
        .unwrap_or("");
    let content = first
        .map(|m| resolve_mentions(&extract_body(m).unwrap_or_default(), m, own_user_id, own_name))
        .unwrap_or_default();
    let reaction = first.and_then(|m| extract_reactions(m).into_iter().max_by_key(|r| r.count));
    let last = thread["last_message_id"].as_str().map(snowflake_secs).filter(|&t| t > 0).unwrap_or_else(|| snowflake_secs(id));
    json!({
        "id": id,
        "name": thread["name"].as_str().unwrap_or("post"),
        "author": author,
        "avatarUrl": first.and_then(|m| author_avatar_url(&m["author"])),
        "content": teaser(&content),
        "createdTs": snowflake_secs(id),
        "lastTs": last,
        "messageCount": thread["message_count"].as_i64().unwrap_or(0),
        "archived": thread["thread_metadata"]["archived"].as_bool().unwrap_or(false),
        // Flag 2: kept at the top of the forum by the people who run it.
        "pinned": thread["flags"].as_u64().unwrap_or(0) & 2 != 0,
        "tags": tag_names,
        "thumbnail": first.and_then(post_thumbnail),
        "reaction": reaction,
    })
}

/// A page of a forum's posts, newest or most recently active first.
///
/// `offset` is how many have been read already. Where the search endpoint will
/// not answer - some servers and some accounts are refused it - the plain lists
/// of live and archived threads stand in, with what they carry: a title, a
/// count and a time, and no first message.
pub async fn list_forum_posts(state: &AppState, account_id: &str, buffer_id: &str, sort: &str, offset: usize) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this forum")?;
    let tags = state.runtime.discord_forum_tags(account_id, &channel_id);
    let sort_by = if sort == "created" { "creation_time" } else { "last_message_time" };

    let resp = http_client_for(&cfg.token)
        .get(format!("{API_BASE}/channels/{channel_id}/threads/search"))
        .query(&[
            ("archived", "true"),
            ("sort_by", sort_by),
            ("sort_order", "desc"),
            ("limit", &PAGE.to_string()),
            ("offset", &offset.to_string()),
            ("tag_setting", "match_some"),
        ])
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("reading the forum")?;
    if resp.status().is_success() {
        let answer: Value = resp.json().await.context("reading the forum")?;
        let threads = answer["threads"].as_array().cloned().unwrap_or_default();
        let firsts = answer["first_messages"].as_array().cloned().unwrap_or_default();
        let posts: Vec<Value> = threads
            .iter()
            .map(|t| {
                let id = t["id"].as_str().unwrap_or_default();
                let first = firsts.iter().find(|m| m["channel_id"].as_str() == Some(id) || m["id"].as_str() == Some(id));
                post_card(t, first, &tags, &cfg.user_id, cfg.display_name.as_deref())
            })
            .collect();
        let has_more = answer["has_more"].as_bool().unwrap_or(posts.len() >= PAGE);
        return Ok(json!({ "posts": posts, "hasMore": has_more, "total": answer["total_results"], "tags": tag_list(&tags) }));
    }
    tracing::debug!("discord: thread search answered {}; using the plain lists", resp.status());

    // The fallback: no first messages, no paging.
    let listed = list_threads(state, account_id, buffer_id).await?;
    let mut posts: Vec<Value> = listed["threads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["thread"].as_object().map(|_| post_card(&t["thread"], None, &tags, &cfg.user_id, cfg.display_name.as_deref())))
        .collect();
    posts.sort_by_key(|p| std::cmp::Reverse(if sort == "created" { p["createdTs"].as_i64() } else { p["lastTs"].as_i64() }));
    Ok(json!({ "posts": posts, "hasMore": false, "total": posts.len(), "tags": tag_list(&tags) }))
}

/// The tags a forum offers, as the window needs them for a new post.
fn tag_list(tags: &[Value]) -> Vec<Value> {
    tags.iter()
        .map(|t| json!({ "id": t["id"], "name": t["name"], "emoji": t["emoji_name"], "moderated": t["moderated"].as_bool().unwrap_or(false) }))
        .collect()
}

/// Makes a post: a thread and its first message in one request.
///
/// The words go through the same mention resolution as a message does, so a
/// name typed in one is a ping like any other. Returns the thread's buffer, so
/// the window can open what it just made.
pub async fn create_forum_post(state: &AppState, account_id: &str, buffer_id: &str, title: &str, body: &str, tags: &[String]) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this forum")?;
    let title = title.trim();
    if title.is_empty() {
        bail!("a post needs a title");
    }
    if body.trim().is_empty() {
        bail!("a post needs something to say");
    }
    let content = resolve_outgoing_mentions(body, mention_candidates(state, account_id, buffer_id));
    let mut payload = json!({
        "name": title.chars().take(100).collect::<String>(),
        "auto_archive_duration": 4320,
        "message": { "content": content },
    });
    if !tags.is_empty() {
        payload["applied_tags"] = json!(tags);
    }
    let resp = send_write(
        http_client_for(&cfg.token)
            .post(format!("{API_BASE}/channels/{channel_id}/threads"))
            .header("Authorization", &cfg.token)
            .json(&payload),
    )
    .await
    .context("making the post")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "making the post"));
    }
    let thread: Value = resp.json().await.context("reading the new post")?;
    let thread_id = thread["id"].as_str().context("Discord made a post with no id")?;
    // Opened the way any post is, which files it under the forum and reads its
    // first page.
    open_thread(state, account_id, buffer_id, thread_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snowflake_says_when_it_was_made() {
        // 2020-01-01T00:00:00Z as a Discord id: (ms since the Discord epoch) << 22.
        let ms = 1_577_836_800_000u64 - DISCORD_EPOCH_MS;
        assert_eq!(snowflake_secs(&(ms << 22).to_string()), 1_577_836_800);
        assert_eq!(snowflake_secs("not an id"), 0);
    }

    #[test]
    fn a_long_first_message_is_cut_and_flattened() {
        assert_eq!(teaser("a\n\n  b   c"), "a b c");
        assert!(teaser(&"word ".repeat(200)).ends_with('…'));
    }

    #[test]
    fn a_post_is_read_off_its_thread_and_first_message() {
        let thread = json!({
            "id": "100", "name": "n64 test suite", "message_count": 3, "flags": 2, "applied_tags": ["t1"],
            "thread_metadata": { "archived": true }
        });
        let first = json!({
            "id": "100", "channel_id": "100", "content": "https://github.com/x/y  look",
            "author": { "id": "9", "username": "nemonic", "global_name": "Nemonic" },
            "attachments": [{ "url": "https://cdn.discordapp.com/a.png", "content_type": "image/png", "filename": "a.png" }],
            "reactions": [{ "count": 1, "emoji": { "name": "❤️", "id": null }, "me": false }]
        });
        let tags = vec![json!({ "id": "t1", "name": "Tools", "emoji_name": "🔧" })];
        let card = post_card(&thread, Some(&first), &tags, "me", None);
        assert_eq!(card["name"], "n64 test suite");
        assert_eq!(card["author"], "Nemonic");
        assert_eq!(card["content"], "https://github.com/x/y look");
        assert_eq!(card["messageCount"], 3);
        assert_eq!(card["pinned"], true);
        assert_eq!(card["archived"], true);
        assert_eq!(card["tags"][0]["name"], "Tools");
        assert_eq!(card["thumbnail"], "https://cdn.discordapp.com/a.png");
        assert_eq!(card["reaction"]["count"], 1);
    }

    #[test]
    fn a_post_with_no_first_message_still_has_a_card() {
        let card = post_card(&json!({ "id": "5", "name": "bare" }), None, &[], "me", None);
        assert_eq!(card["name"], "bare");
        assert_eq!(card["author"], "");
        assert!(card["thumbnail"].is_null());
    }
}
