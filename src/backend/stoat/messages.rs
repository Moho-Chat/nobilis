//! A message as moho's, and a channel's history.
//!
//! Stoat's markdown is close to Discord's, and so is a message: an id that is a ULID (its first ten characters are
//! the time), an author, content, attachments (files on the file server), embeds, replies, reactions, and - for a
//! bridge or a webhook - a masquerade, the name and picture the message wants to be shown with. The words that name
//! somebody (`<@id>`), a channel (`<#id>`) or an emoji (`:id:`) are turned into what they say here, since the window
//! has no list to look them up in.

use super::media;
use super::world::{Channel, Instance, World};
use super::Handle;
use crate::model::{Attachment, Embed, Reaction, ReplyPreview};
use crate::state::AppState;
use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// How many messages a page of history asks for.
pub const PAGE: usize = 50;

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// When a message was sent, from its id: a ULID's first ten characters are the time in milliseconds.
pub fn ulid_ms(id: &str) -> Option<i64> {
    let head = id.get(..10)?;
    let mut n: u64 = 0;
    for c in head.bytes() {
        let v = CROCKFORD.iter().position(|x| *x == c.to_ascii_uppercase())? as u64;
        n = n.checked_mul(32)?.checked_add(v)?;
    }
    i64::try_from(n).ok()
}

fn regexes() -> &'static (Regex, Regex, Regex, Regex, Regex, Regex) {
    static R: OnceLock<(Regex, Regex, Regex, Regex, Regex, Regex)> = OnceLock::new();
    R.get_or_init(|| {
        (
            Regex::new(r"<@([0-9A-Za-z]{26})>").expect("a fixed pattern parses"),
            Regex::new(r"<#([0-9A-Za-z]{26})>").expect("a fixed pattern parses"),
            Regex::new(r"<%([0-9A-Za-z]{26})>").expect("a fixed pattern parses"),
            Regex::new(r":([0-9A-Za-z]{26}):").expect("a fixed pattern parses"),
            Regex::new(r"!!([^\n]+?)!!").expect("a fixed pattern parses"),
            Regex::new(r"<t:(\d{1,12})(?::[a-zA-Z])?>").expect("a fixed pattern parses"),
        )
    })
}

/// A message's words as this client shows them: who and what is named said by name, spoilers in Discord's marks (the
/// window draws those), timestamps as a date.
pub fn words(world: &World, files: Option<(&Instance, &str)>, server: Option<&str>, content: &str) -> String {
    let (user, channel, role, emoji, spoiler, time) = regexes();
    let out = user.replace_all(content, |c: &regex::Captures| format!("@{}", world.name_of(&c[1])));
    let out = channel.replace_all(&out, |c: &regex::Captures| world.channels.get(&c[1]).map(|ch| format!("#{}", ch.name)).unwrap_or_else(|| "#channel".to_string()));
    let out = role.replace_all(&out, |c: &regex::Captures| {
        let name = server.and_then(|s| world.servers.get(s)).and_then(|s| s.roles.get(&c[1])).map(|r| r.name.clone());
        format!("@{}", name.unwrap_or_else(|| "role".to_string()))
    });
    // A custom emoji is drawn: its picture is named in the cache and fetched when the window looks, and the line says so
    // in the form the window reads (`[stoat:emoji:name:file]`). Without a file server it is its name.
    let out = emoji.replace_all(&out, |c: &regex::Captures| match (world.emojis.get(&c[1]), files) {
        (Some(info), Some((instance, account))) => match media::file_url(instance, "emojis", &c[1]) {
            Some(url) => format!("[stoat:emoji:{}:{}]", token_name(&info.name), media::cached(account, media::MEDIA, &url, None, Some(if info.animated { "image/gif" } else { "image/png" }))),
            None => format!(":{}:", info.name),
        },
        (Some(info), None) => format!(":{}:", info.name),
        (None, _) => c[0].to_string(),
    });
    let out = spoiler.replace_all(&out, "||$1||");
    let out = time.replace_all(&out, |c: &regex::Captures| {
        c[1].parse::<i64>().ok().and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|| c[0].to_string())
    });
    out.into_owned()
}

/// What is written in the box turned into what Stoat reads: an emoji named between colons becomes the id it is known by.
/// A name no custom emoji has is left alone, and so is an id.
pub fn outgoing(world: &World, text: &str) -> String {
    static NAMED: OnceLock<Regex> = OnceLock::new();
    let named = NAMED.get_or_init(|| Regex::new(r":([A-Za-z0-9_]{2,32}):").expect("a fixed pattern parses"));
    named
        .replace_all(text, |c: &regex::Captures| {
            let word = &c[1];
            if world.emojis.contains_key(word) {
                return c[0].to_string();
            }
            let mut found: Vec<&String> = world.emojis.iter().filter(|(_, e)| e.name == word).map(|(id, _)| id).collect();
            found.sort();
            match found.first() {
                Some(id) => format!(":{id}:"),
                None => c[0].to_string(),
            }
        })
        .into_owned()
}

/// A name as the emoji token carries it: the letters, digits and underscores a name is made of.
fn token_name(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(32).collect()
}

/// What a system message says: somebody joining, leaving, being removed, a channel renamed.
pub fn system_text(world: &World, system: &Value) -> Option<String> {
    let name = |key: &str| world.name_of(system[key].as_str().unwrap_or(""));
    Some(match system["type"].as_str()? {
        "text" => system["content"].as_str()?.to_string(),
        "user_added" => format!("{} was added by {}", name("id"), name("by")),
        "user_remove" => format!("{} was removed by {}", name("id"), name("by")),
        "user_joined" => format!("{} joined", name("id")),
        "user_left" => format!("{} left", name("id")),
        "user_kicked" => format!("{} was kicked", name("id")),
        "user_banned" => format!("{} was banned", name("id")),
        "channel_renamed" => format!("{} renamed the channel to {}", name("by"), system["name"].as_str().unwrap_or("")),
        "channel_description_changed" => format!("{} changed the channel description", name("by")),
        "channel_icon_changed" => format!("{} changed the channel icon", name("by")),
        "channel_ownership_changed" => format!("{} gave the channel to {}", name("from"), name("to")),
        "message_pinned" => format!("{} pinned a message", name("by")),
        "message_unpinned" => format!("{} unpinned a message", name("by")),
        "call_started" => format!("{} started a call", name("by")),
        other => other.replace('_', " "),
    })
}

fn colour(text: &str) -> Option<i64> {
    let hex = text.strip_prefix('#')?;
    (hex.len() == 6).then(|| i64::from_str_radix(hex, 16).ok()).flatten()
}

/// The address a picture from another site is fetched at: through the instance's link proxy where it has one, so the
/// site does not learn who is looking.
fn proxied(instance: &Instance, url: &str) -> Option<String> {
    let proxy = instance.january.as_deref()?;
    let mut out = url::Url::parse(&format!("{proxy}/proxy")).ok()?;
    out.query_pairs_mut().append_pair("url", url);
    Some(out.to_string())
}

fn attachment_of(account_id: &str, instance: &Instance, file: &Value) -> Option<Attachment> {
    let id = file["_id"].as_str()?;
    let url = media::file_url(instance, file["tag"].as_str().unwrap_or("attachments"), id)?;
    let content_type = file["content_type"].as_str();
    let filename = file["filename"].as_str();
    let metadata = &file["metadata"];
    let kind = match metadata["type"].as_str() {
        Some("Image") => "image",
        Some("Video") => "video",
        Some("Audio") => "audio",
        _ => "file",
    };
    Some(Attachment {
        kind: kind.to_string(),
        mimetype: content_type.map(str::to_string),
        filename: filename.map(str::to_string),
        size: file["size"].as_u64(),
        width: metadata["width"].as_u64().map(|w| w as u32),
        height: metadata["height"].as_u64().map(|h| h as u32),
        path: Some(media::cached(account_id, media::MEDIA, &url, filename, content_type)),
        url: Some(url),
        ..Default::default()
    })
}

fn embed_of(account_id: &str, instance: &Instance, embed: &Value) -> Option<Embed> {
    let picture = |v: &Value| v["url"].as_str().and_then(|u| proxied(instance, u)).map(|u| media::cached(account_id, media::MEDIA, &u, None, None));
    match embed["type"].as_str()? {
        "Website" => Some(Embed {
            title: embed["title"].as_str().map(str::to_string),
            description: embed["description"].as_str().map(str::to_string),
            url: embed["url"].as_str().or(embed["original_url"].as_str()).map(str::to_string),
            provider: embed["site_name"].as_str().map(str::to_string),
            color: embed["colour"].as_str().and_then(colour),
            image_url: picture(&embed["image"]),
            ..Default::default()
        }),
        "Text" => Some(Embed {
            title: embed["title"].as_str().map(str::to_string),
            description: embed["description"].as_str().map(str::to_string),
            url: embed["url"].as_str().map(str::to_string),
            color: embed["colour"].as_str().and_then(colour),
            ..Default::default()
        }),
        "Image" => embed["url"].as_str().and_then(|u| proxied(instance, u)).map(|u| Embed {
            url: embed["url"].as_str().map(str::to_string),
            image_url: Some(media::cached(account_id, media::MEDIA, &u, None, None)),
            ..Default::default()
        }),
        _ => None,
    }
}

/// The reactions on a message as moho's: `{ emoji: [user ids] }`.
pub fn reactions_of(world: &World, reactions: &Value) -> Vec<Reaction> {
    let mut out: Vec<Reaction> = reactions
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(emoji, who)| {
                    let who = who.as_array()?;
                    // A custom emoji is named by its id, which means nothing to a reader.
                    let shown = if emoji.len() == 26 && emoji.chars().all(|c| c.is_ascii_alphanumeric()) { world.emojis.get(emoji).map(|e| format!(":{}:", e.name)).unwrap_or_else(|| ":emoji:".to_string()) } else { emoji.clone() };
                    Some(Reaction { emoji: shown, count: who.len() as i64, me: who.iter().any(|u| u.as_str() == Some(world.own_id.as_str())), animated: false })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| b.count.cmp(&a.count).then(a.emoji.cmp(&b.emoji)));
    out
}

/// The buffer a channel's conversation is, if the account has one for it.
pub fn buffer_of(state: &AppState, account_id: &str, channel_id: &str) -> Option<crate::model::Buffer> {
    state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(channel_id))
}

/// Keeps a message's author, which the message often brings with it.
fn absorb_author(world: &mut World, message: &Value) {
    if let Some((id, user)) = super::world::parse_user(&message["user"]) {
        world.users.entry(id).and_modify(|u| u.name = user.name.clone()).or_insert(user);
    }
}

/// Records a message, as heard live or read from history. Answers whether it was new.
pub fn record(state: &AppState, handle: &Handle, message: &Value) -> bool {
    let (Some(id), Some(channel_id)) = (message["_id"].as_str(), message["channel"].as_str()) else { return false };
    let account_id = &handle.account_id;
    let Some(buffer) = buffer_of(state, account_id, channel_id) else {
        tracing::debug!("stoat[{account_id}]: a message in a channel with no conversation was not shown");
        return false;
    };
    let ts = ulid_ms(id).map(|ms| ms / 1000).unwrap_or_else(|| chrono::Utc::now().timestamp());
    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    absorb_author(&mut world, message);
    let channel: Option<Channel> = world.channels.get(channel_id).cloned();
    let server = channel.as_ref().and_then(|c| c.server.clone());
    let author = message["author"].as_str().unwrap_or("");
    let own = world.own_id.clone();

    // A system message is the room speaking, not a person.
    if let Some(system) = message.get("system").filter(|s| s.is_object()) {
        let text = system_text(&world, system).unwrap_or_default();
        drop(world);
        return state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, "Stoat", &text, false, "system", None, Some(id.to_string()), false, None, Vec::new(), Vec::new(), None, Some(ts), None, None);
    }

    let masquerade = &message["masquerade"];
    let from = masquerade["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| message["webhook"]["name"].as_str().filter(|n| !n.is_empty()).map(str::to_string))
        .or_else(|| message["member"]["nickname"].as_str().filter(|n| !n.is_empty()).map(str::to_string))
        .unwrap_or_else(|| world.name_of(author));
    let avatar = masquerade["avatar"]
        .as_str()
        .map(|u| media::cached(account_id, media::AVATARS, u, None, None))
        .or_else(|| message["webhook"]["avatar"].as_str().and_then(|f| media::file_url(&handle.instance, "avatars", f)).map(|u| media::cached(account_id, media::AVATARS, &u, None, None)))
        .or_else(|| world.users.get(author).and_then(|u| u.avatar.clone()).and_then(|f| media::file_url(&handle.instance, "avatars", &f)).map(|u| media::cached(account_id, media::AVATARS, &u, None, None)));
    let content = message["content"].as_str().unwrap_or("");
    let body = words(&world, Some((&handle.instance, account_id)), server.as_deref(), content);
    let highlight = message["mentions"].as_array().is_some_and(|m| m.iter().any(|u| u.as_str() == Some(own.as_str())))
        || server.as_ref().and_then(|s| world.members.get(s)).zip(message["role_mentions"].as_array()).is_some_and(|(m, roles)| roles.iter().any(|r| r.as_str().is_some_and(|r| m.roles.iter().any(|mine| mine == r))));
    let attachments: Vec<Attachment> = message["attachments"].as_array().into_iter().flatten().filter_map(|f| attachment_of(account_id, &handle.instance, f)).collect();
    let embeds: Vec<Embed> = message["embeds"].as_array().into_iter().flatten().filter_map(|e| embed_of(account_id, &handle.instance, e)).collect();
    let reactions = reactions_of(&world, &message["reactions"]);
    let reply_id = message["replies"].as_array().and_then(|r| r.first()).and_then(|r| r.as_str()).map(str::to_string);
    drop(world);

    let reply = reply_id.as_ref().map(|reply_id| {
        let stored = state.store.get_message(&buffer.id, reply_id).ok().flatten();
        ReplyPreview {
            id: reply_id.clone(),
            from: stored.as_ref().map(|m| m.from.clone()).unwrap_or_default(),
            body: stored.map(|m| if m.body.trim().is_empty() { "Attachment".to_string() } else { m.body }).unwrap_or_default(),
            thread: false,
            forwarded: false,
            thread_root: None,
            chat: None,
        }
    });
    let needs_fill = reply.as_ref().is_some_and(|r| r.body.is_empty());
    let recorded = state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, &from, &body, false, "chat", reply, Some(id.to_string()), highlight, avatar, embeds, attachments, Some(author.to_string()), Some(ts), None, None);
    if recorded && !reactions.is_empty() && state.store.set_reactions(&buffer.id, id, &reactions).unwrap_or(false) {
        state.events.emit("reactionsChanged", json!({ "bufferId": buffer.id, "id": id, "reactions": reactions }));
    }
    if needs_fill {
        if let Some(reply_id) = reply_id {
            fill_reply_later(state, handle, &buffer.id, channel_id, id, &reply_id);
        }
    }
    recorded
}

/// Asks Stoat what a reply answered, and fills the quote in when it comes.
fn fill_reply_later(state: &AppState, handle: &Handle, buffer_id: &str, channel_id: &str, msg_id: &str, reply_id: &str) {
    let (state, handle) = (state.clone(), handle.clone());
    let (buffer_id, channel_id, msg_id, reply_id) = (buffer_id.to_string(), channel_id.to_string(), msg_id.to_string(), reply_id.to_string());
    tokio::spawn(async move {
        let Ok(original) = handle.api.get(&format!("/channels/{channel_id}/messages/{reply_id}"), &[]).await else { return };
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let from = original["masquerade"]["name"].as_str().map(str::to_string).unwrap_or_else(|| world.name_of(original["author"].as_str().unwrap_or("")));
        let server = world.channels.get(&channel_id).and_then(|c| c.server.clone());
        let body = words(&world, Some((&handle.instance, &handle.account_id)), server.as_deref(), original["content"].as_str().unwrap_or(""));
        drop(world);
        let body = if body.trim().is_empty() { "Attachment".to_string() } else { body };
        state.runtime.fill_reply_preview(&state, &buffer_id, &msg_id, &from, &body);
    });
}

/// Reads a page of a channel's history and records it, oldest first so a reply finds what it answers already kept.
/// `before` is the id of the oldest message already held. Answers how many came.
pub async fn history(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, before: Option<&str>) -> Result<usize> {
    let channel_id = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    let mut query = vec![("limit", PAGE.to_string()), ("include_users", "true".to_string())];
    if let Some(before) = before.filter(|b| b.len() == 26) {
        query.push(("before", before.to_string()));
    }
    let answer = handle.api.get(&format!("/channels/{channel_id}/messages"), &query).await.map_err(|e| anyhow!("{e}"))?;
    let (messages, users) = match &answer {
        Value::Array(list) => (list.clone(), Vec::new()),
        other => (other["messages"].as_array().cloned().unwrap_or_default(), other["users"].as_array().cloned().unwrap_or_default()),
    };
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        for u in &users {
            if let Some((id, user)) = super::world::parse_user(u) {
                world.users.entry(id).or_insert(user);
            }
        }
    }
    let mut ordered = messages;
    ordered.sort_by(|a, b| a["_id"].as_str().cmp(&b["_id"].as_str()));
    let count = ordered.len();
    for message in &ordered {
        record(state, handle, message);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "01ME000000000000000000000A";

    fn world() -> World {
        let mut w = World::default();
        w.absorb_ready(
            ME,
            &json!({
                "users": [{ "_id": ME, "username": "me", "display_name": "Me" }, { "_id": "01PAL00000000000000000000A", "username": "pal", "display_name": "Pal" }],
                "servers": [{ "_id": "S1", "owner": ME, "name": "Chasm", "channels": ["C1"], "roles": { "01ROLE0000000000000000000A": { "name": "Mods", "rank": 1, "permissions": { "a": 0, "d": 0 } } } }],
                "channels": [{ "_id": "01CHAN0000000000000000000A", "channel_type": "TextChannel", "server": "S1", "name": "general" }],
                "emojis": [{ "_id": "01EMOJ000000000000000000AA", "name": "party" }]
            }),
        );
        w
    }

    #[test]
    fn a_ulid_says_when_it_was_made() {
        // The example in the ULID specification, made on 2016-07-30 at 23:54:10.259 UTC.
        assert_eq!(ulid_ms("01ARZ3NDEKTSV4RRFFQ69G5FAV"), Some(1_469_922_850_259));
        assert_eq!(ulid_ms("not an id"), None);
        assert_eq!(ulid_ms("01ARZ3"), None);
        assert!(ulid_ms("01ARZ3NDEKTSV4RRFFQ69G5FAV") < ulid_ms("01ARZ3NDEMTSV4RRFFQ69G5FAV"), "a later id is a later time");
    }

    #[test]
    fn who_and_what_is_named_is_said_by_name() {
        let w = world();
        assert_eq!(words(&w, None, Some("S1"), "hi <@01PAL00000000000000000000A>, see <#01CHAN0000000000000000000A>"), "hi @Pal, see #general");
        assert_eq!(words(&w, None, Some("S1"), "calling <%01ROLE0000000000000000000A>"), "calling @Mods");
        assert_eq!(words(&w, None, None, "yay :01EMOJ000000000000000000AA: and :01NONE000000000000000000AA:"), "yay :party: and :01NONE000000000000000000AA:");
        assert_eq!(words(&w, None, None, "nobody <@01GONE0000000000000000000>"), "nobody <@01GONE0000000000000000000>", "an id that is not 26 characters is not a mention");
    }

    #[test]
    fn an_emoji_named_in_the_box_goes_out_as_its_id_and_what_is_not_one_is_left_alone() {
        let w = world();
        assert_eq!(outgoing(&w, "yay :party: :nobody: 12:30:45 and :01EMOJ000000000000000000AA:"), "yay :01EMOJ000000000000000000AA: :nobody: 12:30:45 and :01EMOJ000000000000000000AA:");
        assert_eq!(outgoing(&w, "no colons"), "no colons");
    }

    #[test]
    fn an_emoji_in_a_message_is_a_picture_named_in_the_cache_where_there_is_a_file_server() {
        let w = world();
        let instance = Instance { autumn: Some("https://cdn.example".into()), ..Default::default() };
        let said = words(&w, Some((&instance, "stoat:me")), None, "yay :01EMOJ000000000000000000AA:");
        assert!(said.starts_with("yay [stoat:emoji:party:file://") && said.contains("/stoat-media/") && said.ends_with(".png]"), "{said}");
        assert_eq!(words(&w, Some((&Instance::default(), "stoat:me")), None, "yay :01EMOJ000000000000000000AA:"), "yay :party:", "without a file server it is its name");
    }

    #[test]
    fn spoilers_and_times_are_written_as_the_window_reads_them() {
        let w = world();
        assert_eq!(words(&w, None, None, "the end is !!a twist!! really"), "the end is ||a twist|| really");
        assert_eq!(words(&w, None, None, "at <t:1469918176:R> sharp"), "at 2016-07-30 22:36 UTC sharp");
    }

    #[test]
    fn what_happened_in_a_room_is_said_in_words() {
        let w = world();
        let say = |v: Value| system_text(&w, &v);
        assert_eq!(say(json!({ "type": "user_joined", "id": "01PAL00000000000000000000A" })).as_deref(), Some("Pal joined"));
        assert_eq!(say(json!({ "type": "user_kicked", "id": ME })).as_deref(), Some("Me was kicked"));
        assert_eq!(say(json!({ "type": "channel_renamed", "by": ME, "name": "lobby" })).as_deref(), Some("Me renamed the channel to lobby"));
        assert_eq!(say(json!({ "type": "text", "content": "Welcome" })).as_deref(), Some("Welcome"));
        assert_eq!(say(json!({ "type": "something_new" })).as_deref(), Some("something new"));
        assert_eq!(say(json!({})), None);
    }

    #[test]
    fn reactions_are_counted_and_the_accounts_own_is_marked() {
        let w = world();
        let got = reactions_of(&w, &json!({ "🔥": ["01PAL00000000000000000000A"], "👍": [ME, "01PAL00000000000000000000A"], "01EMOJ000000000000000000AA": [ME] }));
        assert_eq!(got.iter().map(|r| (r.emoji.as_str(), r.count, r.me)).collect::<Vec<_>>(), vec![("👍", 2, true), (":party:", 1, true), ("🔥", 1, false)]);
        assert!(reactions_of(&w, &Value::Null).is_empty());
    }

    #[test]
    fn a_colour_is_read_as_a_number() {
        assert_eq!(colour("#ff8800"), Some(0xff8800));
        assert_eq!(colour("red"), None);
        assert_eq!(colour("#fff"), None);
    }

    #[test]
    fn a_picture_from_another_site_is_asked_for_through_the_proxy() {
        let i = Instance { january: Some("https://proxy.example".into()), ..Default::default() };
        assert_eq!(proxied(&i, "https://site.example/a b.png").as_deref(), Some("https://proxy.example/proxy?url=https%3A%2F%2Fsite.example%2Fa+b.png"));
        assert_eq!(proxied(&Instance::default(), "https://site.example/a.png"), None, "with no proxy it is not fetched at all");
    }
}
