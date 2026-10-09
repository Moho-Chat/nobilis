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

/// How long Discord asks to wait when its search index is not built yet, if
/// that is what an answer says: code 110000, with `retry_after` in seconds.
fn index_not_ready(answer: &Value) -> Option<std::time::Duration> {
    if answer["code"].as_u64() != Some(110_000) {
        return None;
    }
    let secs = answer["retry_after"].as_f64().unwrap_or(2.0).clamp(0.5, 6.0);
    Some(std::time::Duration::from_secs_f64(secs))
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

    // Discord builds a search index the first time one is asked for, and says so
    // with a success status and an answer that holds no threads - "not yet
    // available", with how long to wait. That looked like an empty forum. So it is
    // asked again after the wait it names, a few times, before giving up on it.
    let mut status = reqwest::StatusCode::OK;
    for attempt in 0..3 {
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
        status = resp.status();
        if !status.is_success() {
            break;
        }
        let answer: Value = resp.json().await.context("reading the forum")?;
        if let Some(wait) = index_not_ready(&answer) {
            tracing::debug!("discord: the forum's search index is not ready (attempt {attempt}); waiting {wait:?}");
            tokio::time::sleep(wait).await;
            continue;
        }
        let threads = answer["threads"].as_array().cloned().unwrap_or_default();
        // A forum with nothing found by a search that answered properly is empty
        // - but only the first page can say so; the plain lists below are asked
        // too, so that a search that quietly knows nothing is not believed.
        if threads.is_empty() && offset == 0 {
            // Logged, so what a forum's search says when it says nothing is on record.
            let said = answer.to_string();
            tracing::info!("discord: the search of forum {channel_id} found no posts: {}", said.chars().take(400).collect::<String>());
            break;
        }
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
    tracing::debug!("discord: thread search gave nothing usable ({status}); using the plain lists");

    // The fallback: the plain lists, and no paging.
    let listed = list_threads(state, account_id, buffer_id).await?;
    let threads: Vec<Value> = listed["threads"].as_array().into_iter().flatten().filter_map(|t| t["thread"].as_object().map(|_| t["thread"].clone())).collect();
    let key = |t: &Value| -> i64 {
        let last = t["last_message_id"].as_str().map(snowflake_secs).unwrap_or(0);
        if sort == "created" { t["id"].as_str().map(snowflake_secs).unwrap_or(0) } else { last.max(t["id"].as_str().map(snowflake_secs).unwrap_or(0)) }
    };
    let mut threads = threads;
    threads.sort_by_key(|t| std::cmp::Reverse(key(t)));
    // The words that began each of the first few, which the lists do not carry:
    // one request each for the posts at the top, which is what is on screen.
    let http = http_client_for(&cfg.token);
    let reads = threads.iter().take(10).map(|t| {
        let id = t["id"].as_str().unwrap_or_default().to_string();
        let http = http.clone();
        let token = cfg.token.clone();
        async move {
            let resp = http
                .get(format!("{API_BASE}/channels/{id}/messages"))
                .query(&[("limit", "1"), ("after", "0")])
                .header("Authorization", token)
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let list: Vec<Value> = resp.json().await.ok()?;
            list.into_iter().next()
        }
    });
    let firsts: Vec<Option<Value>> = futures::future::join_all(reads).await;
    let posts: Vec<Value> = threads
        .iter()
        .enumerate()
        .map(|(i, t)| post_card(t, firsts.get(i).and_then(|f| f.as_ref()), &tags, &cfg.user_id, cfg.display_name.as_deref()))
        .collect();
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
    fn an_index_still_being_built_is_waited_for_not_read_as_an_empty_forum() {
        let building = json!({ "message": "Index not yet available. Try again later", "code": 110000, "documents_indexed": 0, "retry_after": 3 });
        assert_eq!(index_not_ready(&building), Some(std::time::Duration::from_secs(3)));
        assert_eq!(index_not_ready(&json!({ "threads": [], "total_results": 0 })), None);
        // A wait that is absurdly long is not taken.
        assert_eq!(index_not_ready(&json!({ "code": 110000, "retry_after": 600 })), Some(std::time::Duration::from_secs(6)));
    }

    #[test]
    fn a_post_with_no_first_message_still_has_a_card() {
        let card = post_card(&json!({ "id": "5", "name": "bare" }), None, &[], "me", None);
        assert_eq!(card["name"], "bare");
        assert_eq!(card["author"], "");
        assert!(card["thumbnail"].is_null());
    }
}
