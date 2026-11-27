//! A post as moho's message, a channel's history, and a thread.
//!
//! A post is: an id, when it was made (milliseconds), its author, its channel, the post it answers (`root_id`: replies
//! are threads, never quotes), the words in Markdown, the files it carries, and `metadata` the server adds - reactions,
//! link previews, the files' details, the priority. A post whose `type` begins `system_` is the channel speaking.
//! A webhook or bot may post under another name and picture (`props.override_username`/`override_icon_url`), and carry
//! the old-style cards of an integration (`props.attachments`).
//!
//! Words that name somebody (`@username`) are left as they are - the window draws a mention of the account - and
//! `:emoji:` names become the emoji. Markdown that the window's own formatting does not draw (tables, task lists,
//! headings, lists) is drawn here as HTML, which the window cleans as it does a Matrix message's.

use super::world::World;
use super::{emoji, media, Handle};
use crate::model::{Attachment, Embed, EmbedField, EmbedLink, Reaction, ReplyPreview};
use crate::state::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// How many posts a page of history asks for.
pub const PAGE: usize = 60;

/// The buffer a channel's conversation is, if the account has one for it.
pub fn buffer_of(state: &AppState, account_id: &str, channel_id: &str) -> Option<crate::model::Buffer> {
    state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(channel_id))
}

/// What a system post says, where the server left its words empty.
pub fn system_text(world: &World, style: &str, post: &Value) -> String {
    let said = post["message"].as_str().unwrap_or("").trim();
    if post["type"].as_str().is_some_and(|t| t.starts_with("custom_calls")) {
        let who = world.name_of(post["user_id"].as_str().unwrap_or(""), style);
        let ended = post["props"]["end_at"].as_i64().unwrap_or(0) > 0;
        return match (said, ended) {
            ("", false) => format!("{who} started a call"),
            ("", true) => "The call ended".to_string(),
            (s, _) => s.to_string(),
        };
    }
    if !said.is_empty() {
        return said.to_string();
    }
    let props = &post["props"];
    let who = world.name_of(post["user_id"].as_str().unwrap_or(""), style);
    let named = |k: &str| props[k].as_str().unwrap_or("someone").to_string();
    match post["type"].as_str().unwrap_or("") {
        "system_join_channel" => format!("{who} joined the channel"),
        "system_leave_channel" => format!("{who} left the channel"),
        "system_join_team" => format!("{who} joined the team"),
        "system_leave_team" => format!("{who} left the team"),
        "system_add_to_channel" => format!("{} was added to the channel by {who}", named("addedUsername")),
        "system_remove_from_channel" => format!("{} was removed from the channel", named("removedUsername")),
        "system_header_change" => format!("{who} changed the channel header to: {}", named("new_header")),
        "system_purpose_change" => format!("{who} changed the channel purpose to: {}", named("new_purpose")),
        "system_displayname_change" => format!("{who} renamed the channel to {}", named("new_displayname")),
        "system_channel_deleted" => format!("{who} archived the channel"),
        "system_channel_restored" => format!("{who} restored the channel"),
        other => other.trim_start_matches("system_").replace('_', " "),
    }
}

/// Whether the words hold Markdown the window does not draw by itself: a table, a task list, a heading, a list.
pub fn needs_html(text: &str) -> bool {
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        if t.starts_with("```") {
            // A fence is the window's own to draw; what is inside it is not Markdown.
            for inner in lines.by_ref() {
                if inner.trim_start().starts_with("```") {
                    break;
                }
            }
            continue;
        }
        let table_rule = |l: &str| {
            let l = l.trim();
            l.contains('-') && l.contains('|') && l.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
        };
        if t.starts_with('|') && lines.peek().is_some_and(|next| table_rule(next)) {
            return true;
        }
        if t.starts_with("- [ ] ") || t.starts_with("- [x] ") || t.starts_with("* [ ] ") || t.starts_with("* [x] ") {
            return true;
        }
        if (t.starts_with("# ") || t.starts_with("## ") || t.starts_with("### ") || t.starts_with("#### ")) && t.len() > 2 {
            return true;
        }
        if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") || t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit())) {
            return true;
        }
    }
    false
}

/// Mattermost's Markdown as HTML (GitHub's flavour: tables, task lists, strikethrough). Raw HTML in the words is shown
/// as text, never passed on as markup.
pub fn to_html(text: &str) -> String {
    use pulldown_cmark::{html, Event, Options, Parser};
    let options = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(text, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        // A task list's box is drawn as a character: a form control is not something a message has.
        Event::TaskListMarker(done) => Event::Text(if done { "☑ ".into() } else { "☐ ".into() }),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// The cards of an integration's post (`props.attachments`): each a card with its colour, title, words and fields.
pub fn cards_of(account_id: &str, props: &Value) -> Vec<Embed> {
    props["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            let text = [a["pretext"].as_str(), a["text"].as_str()].into_iter().flatten().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n");
            let text = if text.is_empty() { a["fallback"].as_str().unwrap_or("").to_string() } else { text };
            let colour = a["color"].as_str().and_then(|c| {
                let hex = c.strip_prefix('#')?;
                (hex.len() == 6).then(|| i64::from_str_radix(hex, 16).ok()).flatten()
            });
            let colour = colour.or(match a["color"].as_str() {
                Some("good") => Some(0x2eb886),
                Some("warning") => Some(0xdaa038),
                Some("danger") => Some(0xa30200),
                _ => None,
            });
            // The buttons and menus of an interactive post act through the integration's own server; they are named so the
            // reader knows they are there, and pressing them is left to Mattermost's app.
            let actions: Vec<String> = a["actions"].as_array().into_iter().flatten().filter_map(|x| x["name"].as_str().map(str::to_string)).collect();
            let footer = [a["footer"].as_str().map(str::to_string), (!actions.is_empty()).then(|| format!("Actions: {} (open in Mattermost to use them)", actions.join(", ")))].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            Embed {
                title: a["title"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
                url: a["title_link"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
                description: (!text.is_empty()).then_some(text),
                color: colour,
                author: a["author_name"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
                fields: a["fields"].as_array().into_iter().flatten().map(|f| EmbedField { name: f["title"].as_str().unwrap_or("").to_string(), value: f["value"].as_str().map(str::to_string).unwrap_or_else(|| f["value"].to_string()) }).collect(),
                image_url: a["image_url"].as_str().and_then(|u| media::external(account_id, u)),
                footer: (!footer.is_empty()).then_some(footer),
                cta: a["author_link"].as_str().filter(|t| !t.is_empty()).map(|u| EmbedLink { label: a["author_name"].as_str().unwrap_or("Open").to_string(), url: Some(u.to_string()) }),
                ..Default::default()
            }
        })
        .collect()
}

/// The cards the server made for links (`metadata.embeds`): a page's preview, a picture, a quoted post.
pub fn previews_of(account_id: &str, metadata: &Value) -> Vec<Embed> {
    metadata["embeds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let url = e["url"].as_str().map(str::to_string);
            match e["type"].as_str()? {
                "opengraph" => {
                    let d = &e["data"];
                    let image = d["images"][0]["secure_url"].as_str().filter(|u| !u.is_empty()).or(d["images"][0]["url"].as_str());
                    Some(Embed {
                        title: d["title"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
                        description: d["description"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
                        provider: d["site_name"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
                        url: url.or_else(|| d["url"].as_str().map(str::to_string)),
                        image_url: image.and_then(|u| media::external(account_id, u)),
                        ..Default::default()
                    })
                }
                "image" => url.as_deref().and_then(|u| media::external(account_id, u)).map(|picture| Embed { url: url.clone(), image_url: Some(picture), ..Default::default() }),
                "permalink" => {
                    let d = &e["data"];
                    let quoted = d["post"]["message"].as_str().unwrap_or("").to_string();
                    Some(Embed {
                        kind: Some("quote".into()),
                        author: d["post"]["props"]["override_username"].as_str().map(str::to_string),
                        title: Some(format!("Message in {}", d["channel_display_name"].as_str().unwrap_or("a channel"))),
                        description: (!quoted.is_empty()).then_some(quoted),
                        target: d["post_id"].as_str().map(str::to_string),
                        ..Default::default()
                    })
                }
                _ => None,
            }
        })
        .collect()
}

/// A file a post carries, as the server described it.
pub fn attachment_of(account_id: &str, info: &Value) -> Option<Attachment> {
    let id = info["id"].as_str()?;
    let mime = info["mime_type"].as_str().map(str::to_string);
    let name = info["name"].as_str().map(str::to_string);
    let kind = match mime.as_deref().unwrap_or("") {
        m if m.starts_with("image/") => "image",
        m if m.starts_with("video/") => "video",
        m if m.starts_with("audio/") => "audio",
        _ => "file",
    };
    // A picture is drawn from its preview (smaller, and always a picture the window can show), anything else is the file.
    let path = if kind == "image" && info["has_preview_image"].as_bool().unwrap_or(false) && mime.as_deref() != Some("image/gif") {
        format!("/api/v4/files/{id}/preview")
    } else {
        format!("/api/v4/files/{id}")
    };
    let content_type = if path.ends_with("/preview") { Some("image/jpeg") } else { mime.as_deref() };
    let shown_name = if path.ends_with("/preview") { None } else { name.as_deref() };
    Some(Attachment {
        kind: kind.to_string(),
        mimetype: mime.clone(),
        filename: name.clone(),
        size: info["size"].as_u64(),
        width: info["width"].as_u64().map(|w| w as u32),
        height: info["height"].as_u64().map(|h| h as u32),
        path: Some(media::cached(account_id, media::MEDIA, &path, shown_name, content_type)),
        ..Default::default()
    })
}

/// The reactions on a post as moho's: grouped by emoji, the account's own marked.
pub fn reactions_of(handle: &Handle, list: &Value) -> Vec<Reaction> {
    let own = handle.own_id();
    let mut grouped: Vec<(String, i64, bool)> = Vec::new();
    for r in list.as_array().into_iter().flatten() {
        let Some(name) = r["emoji_name"].as_str() else { continue };
        let mine = r["user_id"].as_str() == Some(own.as_str());
        match grouped.iter_mut().find(|(n, _, _)| n == name) {
            Some(entry) => {
                entry.1 += 1;
                entry.2 |= mine;
            }
            None => grouped.push((name.to_string(), 1, mine)),
        }
    }
    grouped.into_iter().map(|(name, count, me)| Reaction { emoji: emoji::shown(handle, &name), count, me, animated: false }).collect()
}

/// The marks beside a sender's name: a bot, a webhook, somebody posting under another name.
pub fn badges_of(world: &World, style: &str, post: &Value) -> Vec<crate::backend::kick::api::Badge> {
    use crate::backend::kick::api::Badge;
    let props = &post["props"];
    let mut out = Vec::new();
    let truthy = |v: &Value| v.as_str() == Some("true") || v.as_bool() == Some(true);
    if truthy(&props["from_webhook"]) {
        out.push(Badge { kind: "webhook".into(), text: "Webhook".into(), ..Default::default() });
    } else if props["override_username"].as_str().is_some_and(|n| !n.is_empty()) {
        out.push(Badge { kind: "via".into(), text: format!("Via {}: it is sent under another name", world.name_of(post["user_id"].as_str().unwrap_or(""), style)), ..Default::default() });
    }
    if truthy(&props["from_bot"]) || world.users.get(post["user_id"].as_str().unwrap_or("")).is_some_and(|u| u.bot) {
        out.push(Badge { kind: "app".into(), text: "Bot".into(), ..Default::default() });
    }
    out
}

/// The priority the sender gave a post, said as a line above it.
pub fn priority_of(metadata: &Value) -> Option<String> {
    let p = &metadata["priority"];
    let level = match p["priority"].as_str().unwrap_or("") {
        "important" => Some("Important"),
        "urgent" => Some("Urgent"),
        _ => None,
    };
    let ack = p["requested_ack"].as_bool().unwrap_or(false);
    match (level, ack) {
        (Some(l), true) => Some(format!("{l} · acknowledgement requested")),
        (Some(l), false) => Some(l.to_string()),
        (None, true) => Some("Acknowledgement requested".to_string()),
        (None, false) => None,
    }
}

/// Keeps who a post names, so its author's name and picture can be said.
pub fn absorb_users(handle: &Handle, users: &[Value]) {
    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    for u in users {
        if let Some(user) = super::world::parse_user(u) {
            let status = world.users.get(&user.id).map(|u| u.status.clone()).unwrap_or_default();
            world.users.insert(user.id.clone(), super::world::User { status, ..user });
        }
    }
}

/// Asks for the people a set of posts names that are not known yet.
pub async fn learn_authors(handle: &Handle, posts: &[&Value]) {
    let missing: Vec<String> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let mut ids: Vec<String> = posts.iter().filter_map(|p| p["user_id"].as_str()).filter(|u| !world.users.contains_key(*u)).map(str::to_string).collect();
        ids.sort();
        ids.dedup();
        ids
    };
    if missing.is_empty() {
        return;
    }
    if let Ok(list) = handle.api.post("/api/v4/users/ids", &json!(missing)).await {
        absorb_users(handle, list.as_array().map(Vec::as_slice).unwrap_or(&[]));
    }
}

/// What a post looks like as moho's message, without recording it: the words, the sender's name and picture, files,
/// cards, reactions, and what it answers.
pub struct Shaped {
    pub from: String,
    pub avatar: Option<String>,
    pub body: String,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
    pub embeds: Vec<Embed>,
    pub reactions: Vec<Reaction>,
    pub badges: Vec<crate::backend::kick::api::Badge>,
    pub highlight: bool,
    pub system: bool,
}

pub fn shape(handle: &Handle, post: &Value) -> Shaped {
    let account_id = &handle.account_id;
    let style = handle.server.teammate_name.clone();
    let author = post["user_id"].as_str().unwrap_or("");
    let props = &post["props"];
    let metadata = &post["metadata"];
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    // The channel speaking, and the Calls plugin's own posts (a call started, ended, recorded), which carry a card that only
    // repeats their words.
    let system = post["type"].as_str().is_some_and(|t| t.starts_with("system_") || t.starts_with("custom_calls"));
    if system {
        let text = system_text(&world, &style, post);
        return Shaped { from: "Mattermost".into(), avatar: None, body: text, html: None, attachments: Vec::new(), embeds: Vec::new(), reactions: Vec::new(), badges: Vec::new(), highlight: false, system: true };
    }
    let overridden = props["override_username"].as_str().filter(|n| !n.is_empty());
    let from = overridden.map(str::to_string).unwrap_or_else(|| world.name_of(author, &style));
    let avatar = props["override_icon_url"]
        .as_str()
        .filter(|u| !u.is_empty())
        .and_then(|u| if u.starts_with('/') { Some(media::cached(account_id, media::AVATARS, u, None, None)) } else { media::external(account_id, u) })
        .or_else(|| (!author.is_empty()).then(|| media::avatar(account_id, author, world.users.get(author).map(|u| u.picture).unwrap_or(0))));
    let own_name = world.username_of(&world.own_id).unwrap_or_default();
    let badges = badges_of(&world, &style, post);
    drop(world);
    let raw = post["message"].as_str().unwrap_or("");
    let mut body = emoji::words(handle, raw);
    if let Some(line) = priority_of(metadata) {
        body = format!("**{line}**\n{body}");
    }
    let html = needs_html(raw).then(|| to_html(&body));
    // A mention of the account by name, or of everybody in the channel: Mattermost's own `mentions` come only with the live
    // event, so the words are read here.
    let lowered = raw.to_lowercase();
    let highlight = !own_name.is_empty() && (lowered.contains(&format!("@{}", own_name.to_lowercase())) || ["@channel", "@all", "@here"].iter().any(|m| lowered.contains(m)));
    let attachments: Vec<Attachment> = metadata["files"].as_array().into_iter().flatten().filter_map(|f| attachment_of(account_id, f)).collect();
    let mut embeds = cards_of(account_id, props);
    embeds.extend(previews_of(account_id, metadata));
    let reactions = reactions_of(handle, &metadata["reactions"]);
    Shaped { from, avatar, body, html, attachments, embeds, reactions, badges, highlight, system: false }
}

/// What a reply's quote says: the post it answers, where it is kept.
fn thread_preview(state: &AppState, buffer_id: &str, root: &str) -> ReplyPreview {
    let stored = state.store.get_message(buffer_id, root).ok().flatten();
    ReplyPreview {
        id: root.to_string(),
        from: stored.as_ref().map(|m| m.from.clone()).unwrap_or_default(),
        body: stored.map(|m| if m.body.trim().is_empty() { "Attachment".to_string() } else { m.body }).unwrap_or_default(),
        thread: true,
        forwarded: false,
        thread_root: None,
        chat: None,
    }
}

/// Records a post, as heard live or read from history. Answers whether it was new.
pub fn record(state: &AppState, handle: &Handle, post: &Value) -> bool {
    let (Some(id), Some(channel_id)) = (post["id"].as_str(), post["channel_id"].as_str()) else { return false };
    if post["delete_at"].as_i64().unwrap_or(0) > 0 {
        return false;
    }
    let account_id = &handle.account_id;
    let Some(buffer) = buffer_of(state, account_id, channel_id) else {
        tracing::debug!("mattermost[{account_id}]: a post in a channel with no conversation was not shown");
        return false;
    };
    let ts = post["create_at"].as_i64().map(|ms| ms / 1000).unwrap_or_else(|| chrono::Utc::now().timestamp());
    let shaped = shape(handle, post);
    if shaped.system {
        return state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, "Mattermost", &shaped.body, false, "system", None, Some(id.to_string()), false, None, Vec::new(), Vec::new(), None, Some(ts), None, None);
    }
    let root = post["root_id"].as_str().filter(|r| !r.is_empty());
    let reply = root.map(|r| thread_preview(state, &buffer.id, r));
    let author = post["user_id"].as_str().map(str::to_string);
    let style = (!shaped.badges.is_empty()).then_some(crate::model::SenderStyle { color: None, badges: shaped.badges });
    let recorded = state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, &shaped.from, &shaped.body, false, "chat", reply, Some(id.to_string()), shaped.highlight, shaped.avatar, shaped.embeds, shaped.attachments, author, Some(ts), shaped.html, style);
    if recorded {
        quote_links_later(state, handle, &buffer.id, id, post["message"].as_str().unwrap_or(""));
    }
    if recorded && !shaped.reactions.is_empty() && state.store.set_reactions(&buffer.id, id, &shaped.reactions).unwrap_or(false) {
        state.events.emit("reactionsChanged", json!({ "bufferId": buffer.id, "id": id, "reactions": shaped.reactions }));
    }
    // How many replies a post has, as the server counts them: with threads collapsed they are not in the channel to be
    // counted, and the window's "3 replies" is drawn from this.
    if root.is_none() {
        if let Some(count) = post["reply_count"].as_i64().filter(|n| *n > 0) {
            handle.world.lock().unwrap_or_else(|e| e.into_inner()).replies.insert(id.to_string(), count);
            state.events.emit("threadCount", json!({ "bufferId": buffer.id, "id": id, "count": count }));
        }
    }
    if recorded && post["edit_at"].as_i64().unwrap_or(0) > 0 {
        state.runtime.update_message_body_only(state, &buffer.id, id, &shape(handle, post).body);
    }
    recorded
}

/// The post ids a post's words link to on this server (`<server>/<team>/pl/<post id>`).
pub fn permalinks(server: &str, text: &str) -> Vec<String> {
    let base = server.trim_end_matches('/');
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(base) {
        let after = &rest[at + base.len()..];
        // `/<team>/pl/<id>`
        let mut parts = after.trim_start_matches('/').splitn(3, '/');
        if let (Some(_team), Some("pl"), Some(tail)) = (parts.next(), parts.next(), parts.next()) {
            let id: String = tail.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            if super::is_id(&id) && !out.contains(&id) {
                out.push(id);
            }
        }
        rest = &after[..];
        if rest.is_empty() {
            break;
        }
    }
    out
}

/// The address the window reads as "go to this message", for a quote's button.
pub fn jump_link(buffer_id: &str, post: &str) -> String {
    let mut url = url::Url::parse("moho:jump").expect("a fixed address parses");
    url.query_pairs_mut().append_pair("buffer", buffer_id).append_pair("message", post);
    url.to_string()
}

/// Draws the posts a post links to as quotes under it, read from the server: a permalink's preview is the server's only
/// where it knows its own address, which many servers do not say.
pub fn quote_links_later(state: &AppState, handle: &Handle, buffer_id: &str, post_id: &str, text: &str) {
    let ids = permalinks(handle.api.base(), text);
    if ids.is_empty() {
        return;
    }
    let (state, handle, buffer_id, post_id) = (state.clone(), handle.clone(), buffer_id.to_string(), post_id.to_string());
    tokio::spawn(async move {
        let mut cards = state.store.get_message(&buffer_id, &post_id).ok().flatten().map(|m| m.embeds).unwrap_or_default();
        let before = cards.len();
        for id in ids {
            if cards.iter().any(|c| c.target.as_deref() == Some(id.as_str())) {
                continue;
            }
            let Ok(quoted) = handle.api.get(&format!("/api/v4/posts/{id}"), &[]).await else { continue };
            learn_authors(&handle, &[&quoted]).await;
            let shaped = shape(&handle, &quoted);
            let channel = quoted["channel_id"].as_str().unwrap_or("");
            let where_ = buffer_of(&state, &handle.account_id, channel);
            let title = where_.as_ref().map(|b| format!("Message in {}", b.name.rsplit('/').next().unwrap_or(&b.name))).unwrap_or_else(|| "Message".to_string());
            cards.push(Embed {
                kind: Some("quote".into()),
                author: Some(shaped.from),
                title: Some(title),
                description: Some(shaped.body),
                target: Some(id.clone()),
                cta: where_.map(|b| EmbedLink { label: "Go to the message".into(), url: Some(jump_link(&b.id, &id)) }),
                ..Default::default()
            });
        }
        if cards.len() > before {
            state.runtime.set_message_embeds(&state, &buffer_id, &post_id, &cards);
        }
    });
}

/// A reply heard live moves its thread's count on.
pub fn count_reply(state: &AppState, handle: &Handle, post: &Value) {
    let (Some(root), Some(channel)) = (post["root_id"].as_str().filter(|r| !r.is_empty()), post["channel_id"].as_str()) else { return };
    let Some(buffer) = buffer_of(state, &handle.account_id, channel) else { return };
    let count = {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let n = world.replies.entry(root.to_string()).or_insert(0);
        *n += 1;
        *n
    };
    state.events.emit("threadCount", json!({ "bufferId": buffer.id, "id": root, "count": count }));
}

/// An edited post brought up to date: its words, files, cards and reactions.
pub fn update(state: &AppState, handle: &Handle, post: &Value) {
    let (Some(id), Some(channel_id)) = (post["id"].as_str(), post["channel_id"].as_str()) else { return };
    let Some(buffer) = buffer_of(state, &handle.account_id, channel_id) else { return };
    if state.store.get_message(&buffer.id, id).ok().flatten().is_none() {
        record(state, handle, post);
        return;
    }
    let shaped = shape(handle, post);
    state.runtime.update_message(state, &buffer.id, id, &shaped.body, &shaped.embeds, &shaped.attachments);
    if state.store.set_reactions(&buffer.id, id, &shaped.reactions).unwrap_or(false) {
        state.events.emit("reactionsChanged", json!({ "bufferId": buffer.id, "id": id, "reactions": shaped.reactions }));
    }
}

/// The posts of a page (`{order, posts}`), oldest first.
pub fn ordered(page: &Value) -> Vec<Value> {
    let posts = &page["posts"];
    let mut list: Vec<Value> = page["order"].as_array().into_iter().flatten().filter_map(|id| id.as_str()).filter_map(|id| posts.get(id).cloned()).collect();
    if list.is_empty() {
        list = posts.as_object().map(|o| o.values().cloned().collect()).unwrap_or_default();
    }
    list.sort_by_key(|p| p["create_at"].as_i64().unwrap_or(0));
    list
}

/// Reads a page of a channel's history and records it, oldest first so a reply finds what it answers already kept.
/// `before` is the id of the oldest post already held. Answers how many came.
pub async fn history(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, before: Option<&str>) -> Result<usize> {
    let channel_id = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Mattermost conversation"))?;
    let mut query = vec![("per_page", PAGE.to_string()), ("page", "0".to_string())];
    if let Some(before) = before.filter(|b| super::is_id(b)) {
        query.push(("before", before.to_string()));
    }
    // With threads collapsed the channel is its posts, and a thread's replies are read in the thread.
    if collapsed(handle) {
        query.push(("collapsedThreads", "true".to_string()));
        query.push(("collapsedThreadsExtended", "false".to_string()));
    }
    let page = handle.api.get(&format!("/api/v4/channels/{channel_id}/posts"), &query).await.map_err(|e| anyhow!("{e}"))?;
    let posts = ordered(&page);
    learn_authors(handle, &posts.iter().collect::<Vec<_>>()).await;
    let count = posts.len();
    for post in &posts {
        record(state, handle, post);
    }
    Ok(count)
}

/// Whether this account reads replies as threads apart from the channel: the server's setting, and the account's own
/// choice where the server leaves it to the person.
pub fn collapsed(handle: &Handle) -> bool {
    match handle.server.collapsed_threads.as_str() {
        "always_on" => true,
        "default_on" => handle.world.lock().unwrap_or_else(|e| e.into_inner()).collapsed_choice.unwrap_or(true),
        "default_off" => handle.world.lock().unwrap_or_else(|e| e.into_inner()).collapsed_choice.unwrap_or(false),
        _ => false,
    }
}

/// A thread, whole: its first post and every reply, as the thread panel wants them.
pub async fn thread(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, root: &str) -> Result<Value> {
    let page = handle.api.get(&format!("/api/v4/posts/{root}/thread"), &[("perPage", "200".into()), ("direction", "down".into())]).await.map_err(|e| anyhow!("{e}"))?;
    let posts = ordered(&page);
    learn_authors(handle, &posts.iter().collect::<Vec<_>>()).await;
    for post in &posts {
        record(state, handle, post);
    }
    let messages: Vec<Value> = posts.iter().filter_map(|p| p["id"].as_str()).filter_map(|id| state.store.get_message(&buffer.id, id).ok().flatten()).filter_map(|m| serde_json::to_value(m).ok()).collect();
    Ok(json!({ "messages": messages }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_the_window_draws_is_left_to_it_and_the_rest_is_drawn_here() {
        assert!(!needs_html("**bold** and `code` and ~~gone~~"));
        assert!(!needs_html("```\n- not a list\n| a | b |\n|---|---|\n```"));
        assert!(needs_html("| a | b |\n|---|---|\n| 1 | 2 |"));
        assert!(needs_html("- [ ] buy milk\n- [x] done"));
        assert!(needs_html("# Heading"));
        assert!(needs_html("1. first\n2. second"));
        assert!(!needs_html("#hashtag is not a heading"));
        let html = to_html("| a | b |\n|---|---|\n| 1 | 2 |\n\n- [x] done\n\n<script>x</script>");
        assert!(html.contains("<table>") && html.contains("<td>1</td>"), "{html}");
        assert!(html.contains("☑ done"), "{html}");
        assert!(!html.contains("<script>"), "raw HTML is text: {html}");
    }

    #[test]
    fn an_integrations_card_has_its_colour_title_words_and_fields() {
        let cards = cards_of(
            "mattermost:h:u",
            &json!({ "attachments": [{ "color": "#ff0000", "pretext": "Build", "text": "Failed", "title": "CI #12", "title_link": "https://ci.example/12", "fields": [{ "title": "Branch", "value": "main", "short": true }], "actions": [{ "name": "Retry" }] }, { "color": "good", "fallback": "ok" }] }),
        );
        assert_eq!(cards.len(), 2);
        assert_eq!((cards[0].color, cards[0].title.as_deref(), cards[0].url.as_deref(), cards[0].description.as_deref()), (Some(0xff0000), Some("CI #12"), Some("https://ci.example/12"), Some("Build\n\nFailed")));
        assert_eq!(cards[0].fields[0].name, "Branch");
        assert!(cards[0].footer.as_deref().is_some_and(|f| f.contains("Retry")));
        assert_eq!((cards[1].color, cards[1].description.as_deref()), (Some(0x2eb886), Some("ok")));
    }

    #[test]
    fn a_link_preview_is_a_card_with_its_picture_through_the_server() {
        let got = previews_of("mattermost:h:u", &json!({ "embeds": [{ "type": "opengraph", "url": "https://x.example/a", "data": { "title": "T", "site_name": "X", "images": [{ "secure_url": "https://x.example/i.png" }] } }, { "type": "image", "url": "https://x.example/p.png" }, { "type": "nothing" }] }));
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].title.as_deref(), got[0].provider.as_deref()), (Some("T"), Some("X")));
        assert!(got[0].image_url.as_deref().is_some_and(|p| p.contains("mattermost-media")));
    }

    #[test]
    fn a_picture_is_drawn_from_its_preview_and_a_file_is_the_file() {
        let pic = attachment_of("mattermost:h:u", &json!({ "id": "f1", "name": "cat.png", "mime_type": "image/png", "has_preview_image": true, "width": 10, "height": 5 })).unwrap();
        assert_eq!((pic.kind.as_str(), pic.width, pic.filename.as_deref()), ("image", Some(10), Some("cat.png")));
        assert!(pic.path.as_deref().is_some_and(|p| p.ends_with(".jpg")));
        let doc = attachment_of("mattermost:h:u", &json!({ "id": "f2", "name": "a.pdf", "mime_type": "application/pdf" })).unwrap();
        assert!(doc.kind == "file" && doc.path.as_deref().is_some_and(|p| p.ends_with(".pdf")));
    }

    #[test]
    fn a_priority_is_said_above_the_post() {
        assert_eq!(priority_of(&json!({ "priority": { "priority": "urgent", "requested_ack": true } })).as_deref(), Some("Urgent · acknowledgement requested"));
        assert_eq!(priority_of(&json!({ "priority": { "priority": "", "requested_ack": false } })), None);
        assert_eq!(priority_of(&json!({})), None);
    }

    #[test]
    fn a_link_to_a_post_on_this_server_is_found() {
        let id = "bx955xywntf83q76gx1ost4pkr";
        assert_eq!(permalinks("https://chat.example.org", &format!("see https://chat.example.org/alpha/pl/{id}, and https://elsewhere.org/alpha/pl/{id}")), vec![id.to_string()]);
        assert!(permalinks("https://chat.example.org", "https://chat.example.org/alpha/channels/town-square").is_empty());
        assert!(jump_link("mattermost:h:u|Alpha/Town", id).starts_with("moho:jump?buffer=mattermost%3Ah%3Au%7CAlpha%2FTown&message="));
    }

    #[test]
    fn a_page_is_read_oldest_first() {
        let page = json!({ "order": ["b", "a"], "posts": { "a": { "id": "a", "create_at": 1 }, "b": { "id": "b", "create_at": 2 } } });
        assert_eq!(ordered(&page).iter().map(|p| p["id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn a_system_post_with_no_words_is_said() {
        let mut w = World::default();
        w.users.insert("u".into(), super::super::world::User { id: "u".into(), username: "ann".into(), ..Default::default() });
        assert_eq!(system_text(&w, "username", &json!({ "type": "system_join_channel", "user_id": "u", "message": "" })), "ann joined the channel");
        assert_eq!(system_text(&w, "username", &json!({ "type": "system_join_channel", "user_id": "u", "message": "ann joined the channel." })), "ann joined the channel.");
        assert_eq!(system_text(&w, "username", &json!({ "type": "system_something_new", "message": "" })), "something new");
    }
}
