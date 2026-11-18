//! A message as moho's, and a channel's history.
//!
//! A Fluxer message is a Discord message in all but name (`http-api/messages.mdx`): an id that is a snowflake, an author,
//! content in Discord's markdown, attachments with their own addresses, embeds, a reply (`message_reference` with the
//! answered message beside it), reactions, stickers. Discord's readers for embeds, attachments and replies read it
//! unchanged and are used as they are. What differs is where pictures live - the instance's media proxy, not
//! Discord's CDN - so custom emoji, stickers and avatars are named here, in the cache, from the discovery document.

use super::media;
use super::world::World;
use super::Handle;
use crate::backend::discord::messages as discord;
use crate::model::{Attachment, Reaction};
use crate::state::AppState;
use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// How many messages a page of history asks for (Fluxer allows 1-100).
pub const PAGE: usize = 50;

/// Fluxer's epoch: a snowflake's top 42 bits are milliseconds since 2015-01-01 (`snowflakes.md`).
const EPOCH_MS: i64 = 1_420_070_400_000;

/// When a snowflake was made, in Unix seconds.
pub fn snowflake_secs(id: &str) -> Option<i64> {
    let n: u64 = id.parse().ok()?;
    Some(((n >> 22) as i64 + EPOCH_MS) / 1000)
}

fn regexes() -> &'static (Regex, Regex, Regex, Regex, Regex) {
    static R: OnceLock<(Regex, Regex, Regex, Regex, Regex)> = OnceLock::new();
    R.get_or_init(|| {
        (
            Regex::new(r"<@!?(\d{15,25})>").expect("a fixed pattern parses"),
            Regex::new(r"<#(\d{15,25})>").expect("a fixed pattern parses"),
            Regex::new(r"<@&(\d{15,25})>").expect("a fixed pattern parses"),
            Regex::new(r"<(a?):([A-Za-z0-9_]{1,32}):(\d{15,25})>").expect("a fixed pattern parses"),
            Regex::new(r"<t:(-?\d{1,12})(?::[tTdDfFR])?>").expect("a fixed pattern parses"),
        )
    })
}

/// A custom emoji, in the form the window draws one: `[fluxer:emoji:name:file://...]`, its picture named in the cache.
pub fn emoji_token(handle: &Handle, name: &str, id: &str, animated: bool) -> String {
    let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(32).collect();
    match media::emoji_url(&handle.instance, id, animated) {
        Some(url) => format!("[fluxer:emoji:{name}:{}]", media::cached(&handle.account_id, media::MEDIA, &url, None, Some(if animated { "image/gif" } else { "image/webp" }))),
        None => format!(":{name}:"),
    }
}

/// A message's words as this client shows them: who and what is named said by name, custom emoji drawn, timestamps
/// as a date. The rest of the markdown is Discord's, which the window already reads.
pub fn words(world: &World, handle: Option<&Handle>, guild: Option<&str>, content: &str) -> String {
    let (user, channel, role, emoji, time) = regexes();
    let out = user.replace_all(content, |c: &regex::Captures| format!("@{}", world.name_in(guild, &c[1])));
    let out = channel.replace_all(&out, |c: &regex::Captures| world.channels.get(&c[1]).map(|ch| format!("#{}", ch.name)).unwrap_or_else(|| "#channel".to_string()));
    let out = role.replace_all(&out, |c: &regex::Captures| {
        let name = guild.and_then(|g| world.guilds.get(g)).and_then(|g| g.roles.get(&c[1])).map(|r| r.name.clone());
        format!("@{}", name.unwrap_or_else(|| "role".to_string()))
    });
    let out = emoji.replace_all(&out, |c: &regex::Captures| match handle {
        Some(handle) => emoji_token(handle, &c[2], &c[3], !c[1].is_empty()),
        None => format!(":{}:", &c[2]),
    });
    let out = time.replace_all(&out, |c: &regex::Captures| {
        c[1].parse::<i64>().ok().and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|| c[0].to_string())
    });
    out.into_owned()
}

/// What is written in the box turned into what Fluxer reads: `:name:` of a custom emoji the account can use becomes
/// `<:name:id>`. A name no custom emoji has is left alone.
pub fn outgoing(world: &World, guild: Option<&str>, text: &str) -> String {
    static NAMED: OnceLock<Regex> = OnceLock::new();
    let named = NAMED.get_or_init(|| Regex::new(r"(^|[^<a]):([A-Za-z0-9_]{2,32}):").expect("a fixed pattern parses"));
    named
        .replace_all(text, |c: &regex::Captures| {
            let word = &c[2];
            // The guild's own first, then any other the account is in.
            let mut found: Vec<(&String, &super::world::Emoji)> = world.emojis.iter().filter(|(_, e)| e.name == word).collect();
            found.sort_by_key(|(id, e)| (Some(e.guild.as_str()) != guild, (*id).clone()));
            match found.first() {
                Some((id, e)) => format!("{}<{}:{}:{}>", &c[1], if e.animated { "a" } else { "" }, e.name, id),
                None => c[0].to_string(),
            }
        })
        .into_owned()
}

/// What a system message says.
pub fn system_text(world: &World, guild: Option<&str>, message: &Value) -> Option<String> {
    let author = message["author"]["id"].as_str().unwrap_or("");
    let who = world.name_in(guild, author);
    let other = message["mentions"].as_array().and_then(|m| m.first()).and_then(|u| u["id"].as_str()).map(|id| world.name_in(guild, id)).unwrap_or_else(|| "someone".to_string());
    Some(match message["type"].as_i64()? {
        1 => format!("{who} added {other}"),
        2 if other == who || message["mentions"].as_array().is_none_or(|m| m.is_empty()) => format!("{who} left"),
        2 => format!("{who} removed {other}"),
        3 => format!("{who} started a call"),
        4 => format!("{who} renamed the conversation to {}", message["content"].as_str().unwrap_or("")),
        5 => format!("{who} changed the conversation's picture"),
        6 => format!("{who} pinned a message"),
        7 => format!("{who} joined"),
        12 => format!("{who} followed an announcement channel here: {}", message["content"].as_str().unwrap_or("")),
        18 => format!("{who} started a thread: {}", message["content"].as_str().unwrap_or("")),
        _ => return None,
    })
}

/// Stickers, as pictures from the media proxy.
fn stickers(handle: &Handle, message: &Value) -> Vec<Attachment> {
    let list = message["stickers"].as_array().or_else(|| message["sticker_items"].as_array());
    list.into_iter()
        .flatten()
        .filter_map(|s| {
            let id = s["id"].as_str()?;
            let animated = s["animated"].as_bool().unwrap_or(false) || s["format_type"].as_i64() == Some(4);
            let url = media::sticker_url(&handle.instance, id, animated)?;
            let name = s["name"].as_str().unwrap_or("sticker");
            Some(Attachment {
                kind: "image".to_string(),
                filename: Some(format!("{name}.webp")),
                path: Some(media::cached(&handle.account_id, media::MEDIA, &url, None, Some("image/webp"))),
                url: Some(url),
                mimetype: Some("image/webp".to_string()),
                width: Some(160),
                height: Some(160),
                ..Default::default()
            })
        })
        .collect()
}

/// A reaction's emoji as it is held and shown: a custom one by its picture, a unicode one as it is.
pub fn reaction_key(handle: &Handle, emoji: &Value) -> String {
    match emoji["id"].as_str() {
        Some(id) => emoji_token(handle, emoji["name"].as_str().unwrap_or("emoji"), id, emoji["animated"].as_bool().unwrap_or(false)),
        None => emoji["name"].as_str().unwrap_or("?").to_string(),
    }
}

/// What a held reaction is called on the wire: a unicode emoji as it is (escaped for the path), a custom one as
/// `name:id`, read back from the picture's form or from `<:name:id>`.
pub fn reaction_wire(world: &World, held: &str) -> String {
    if let Some(rest) = held.strip_prefix("[fluxer:emoji:") {
        let name = rest.split(':').next().unwrap_or("");
        if let Some((id, _)) = world.emojis.iter().find(|(_, e)| e.name == name) {
            return format!("{name}:{id}");
        }
    }
    if let Some(c) = regexes().3.captures(held) {
        return format!("{}:{}", &c[2], &c[3]);
    }
    held.to_string()
}

pub fn reactions_of(handle: &Handle, message: &Value) -> Vec<Reaction> {
    message["reactions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Reaction { emoji: reaction_key(handle, &r["emoji"]), count: r["count"].as_i64()?, me: r["me"].as_bool().unwrap_or(false), animated: false })
        })
        .collect()
}

/// An author's picture, from the media proxy.
pub fn avatar_of(handle: &Handle, user: &Value) -> Option<String> {
    let id = user["id"].as_str()?;
    let hash = user["avatar"].as_str()?;
    media::asset_url(&handle.instance, "avatars", id, hash).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp")))
}

/// The buffer a channel's conversation is, if the account has one for it.
pub fn buffer_of(state: &AppState, account_id: &str, channel_id: &str) -> Option<crate::model::Buffer> {
    state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(channel_id))
}

/// Records a message, as heard live or read from history. Answers whether it was new.
pub fn record(state: &AppState, handle: &Handle, message: &Value) -> bool {
    let (Some(id), Some(channel_id)) = (message["id"].as_str(), message["channel_id"].as_str()) else { return false };
    let account_id = &handle.account_id;
    let Some(buffer) = buffer_of(state, account_id, channel_id) else {
        tracing::debug!("fluxer[{account_id}]: a message in a channel with no conversation was not shown");
        return false;
    };
    let ts = message["timestamp"].as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|t| t.timestamp()).or_else(|| snowflake_secs(id)).unwrap_or_else(|| chrono::Utc::now().timestamp());
    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((uid, user)) = super::world::parse_user(&message["author"]) {
        world.users.insert(uid, user);
    }
    for m in message["mentions"].as_array().into_iter().flatten() {
        if let Some((uid, user)) = super::world::parse_user(m) {
            world.users.entry(uid).or_insert(user);
        }
    }
    let guild = world.channels.get(channel_id).and_then(|c| c.guild.clone()).or_else(|| message["guild_id"].as_str().map(str::to_string));
    if let (Some(g), true) = (guild.as_deref(), message["member"].is_object()) {
        let mut member = message["member"].clone();
        member["user"] = message["author"].clone();
        world.put_member(g, &member);
    }
    let author = message["author"]["id"].as_str().unwrap_or("").to_string();
    let own = world.own_id.clone();
    let kind = message["type"].as_i64().unwrap_or(0);
    if !matches!(kind, 0 | 19 | 20 | 21 | 23) {
        let text = system_text(&world, guild.as_deref(), message);
        drop(world);
        let Some(text) = text else { return false };
        return state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, "Fluxer", &text, false, "system", None, Some(id.to_string()), false, None, Vec::new(), Vec::new(), None, Some(ts), None, None);
    }
    let from = if message["webhook_id"].is_string() { message["author"]["username"].as_str().unwrap_or("Webhook").to_string() } else { world.name_in(guild.as_deref(), &author) };
    let content = message["content"].as_str().unwrap_or("");
    // Discord's reader gathers the content, a forward's words and the media of embeds; the names in it are then said.
    let mut gathered = message.clone();
    gathered["content"] = json!(content);
    let body = discord::extract_body(&gathered).unwrap_or_default();
    let body = words(&world, Some(handle), guild.as_deref(), &body);
    let highlight = author != own
        && (message["mentions"].as_array().is_some_and(|m| m.iter().any(|u| u["id"].as_str() == Some(own.as_str())))
            || message["mention_everyone"].as_bool() == Some(true)
            || guild.as_ref().and_then(|g| world.guilds.get(g)).and_then(|g| g.members.get(&own)).zip(message["mention_roles"].as_array()).is_some_and(|(m, roles)| roles.iter().any(|r| r.as_str().is_some_and(|r| m.roles.iter().any(|mine| mine == r)))));
    let bot = message["author"]["bot"].as_bool() == Some(true);
    drop(world);
    let avatar = avatar_of(handle, &message["author"]);
    let mut attachments = discord::extract_attachments(message);
    attachments.extend(discord::forwarded_attachments(message));
    attachments.extend(stickers(handle, message));
    for a in attachments.iter_mut() {
        if a.path.is_none() {
            if let Some(url) = a.url.clone() {
                a.path = Some(media::cached(account_id, media::MEDIA, &url, a.filename.as_deref(), a.mimetype.as_deref()));
            }
        }
    }
    let embeds = discord::extract_embeds(message);
    let reactions = reactions_of(handle, message);
    let mut reply = discord::extract_reply(message);
    if let Some(r) = reply.as_mut() {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        r.body = words(&world, Some(handle), guild.as_deref(), &r.body);
    }
    let badges = if message["webhook_id"].is_string() {
        vec![crate::backend::kick::api::Badge { kind: "webhook".into(), text: "Webhook".into(), ..Default::default() }]
    } else if bot {
        vec![crate::backend::kick::api::Badge { kind: "app".into(), text: "Bot".into(), ..Default::default() }]
    } else {
        Vec::new()
    };
    let style = (!badges.is_empty()).then_some(crate::model::SenderStyle { color: None, badges });
    let recorded = state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, &from, &body, false, "chat", reply, Some(id.to_string()), highlight, avatar, embeds, attachments, Some(author), Some(ts), None, style);
    if recorded && !reactions.is_empty() && state.store.set_reactions(&buffer.id, id, &reactions).unwrap_or(false) {
        state.events.emit("reactionsChanged", json!({ "bufferId": buffer.id, "id": id, "reactions": reactions }));
    }
    recorded
}

/// Reads a page of a channel's history and records it, oldest first so a reply finds what it answers already kept.
/// `before` is the id of the oldest message already held. Answers how many came.
pub async fn history(state: &AppState, handle: &Handle, buffer: &crate::model::Buffer, before: Option<&str>) -> Result<usize> {
    let channel_id = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Fluxer conversation"))?;
    super::connect::activate_for(handle, channel_id);
    let mut query = vec![("limit", PAGE.to_string())];
    if let Some(before) = before.filter(|b| b.parse::<u64>().is_ok()) {
        query.push(("before", before.to_string()));
    }
    let answer = handle.api.get(&format!("/channels/{channel_id}/messages"), &query).await.map_err(|e| anyhow!("{e}"))?;
    let list = answer.as_array().cloned().unwrap_or_default();
    let count = list.len();
    for message in list.iter().rev() {
        record(state, handle, message);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut w = World { own_id: "1".into(), ..Default::default() };
        w.put_guild(&json!({ "id": "100000000000000001", "properties": { "name": "G", "owner_id": "1" }, "roles": [{ "id": "300000000000000001", "name": "Mods", "permissions": "0" }], "channels": [{ "id": "400000000000000001", "type": 0, "name": "general" }], "emojis": [{ "id": "500000000000000001", "name": "party", "animated": false }] }));
        w.users.insert("200000000000000001".into(), super::super::world::User { username: "ada".into(), global_name: Some("Ada".into()), ..Default::default() });
        w
    }

    #[test]
    fn names_channels_roles_and_times_are_said_in_words() {
        let w = world();
        let g = Some("100000000000000001");
        assert_eq!(words(&w, None, g, "hi <@200000000000000001> and <@!200000000000000001>"), "hi @Ada and @Ada");
        assert_eq!(words(&w, None, g, "see <#400000000000000001>"), "see #general");
        assert_eq!(words(&w, None, g, "<@&300000000000000001> look"), "@Mods look");
        assert_eq!(words(&w, None, g, "<:party:500000000000000001>"), ":party:");
        assert_eq!(words(&w, None, g, "<t:0:R>"), "1970-01-01 00:00 UTC");
    }

    #[test]
    fn a_custom_emoji_written_by_name_goes_out_by_id() {
        let w = world();
        assert_eq!(outgoing(&w, Some("100000000000000001"), "yay :party: :nope:"), "yay <:party:500000000000000001> :nope:");
        assert_eq!(outgoing(&w, None, "<:party:500000000000000001>"), "<:party:500000000000000001>", "already an id");
    }

    #[test]
    fn a_held_reaction_is_named_on_the_wire_as_fluxer_wants() {
        let w = world();
        assert_eq!(reaction_wire(&w, "👍"), "👍");
        assert_eq!(reaction_wire(&w, "[fluxer:emoji:party:file:///x/fluxer-media/a.webp]"), "party:500000000000000001");
        assert_eq!(reaction_wire(&w, "<:party:500000000000000001>"), "party:500000000000000001");
    }

    #[test]
    fn a_snowflake_says_when_it_was_made_and_a_system_message_is_said() {
        // 2026-08-31 in the docs' example is about 1501320000000000000.
        let secs = snowflake_secs("1501320000000000000").unwrap();
        assert!(secs > 1_700_000_000 && secs < 1_900_000_000, "{secs}");
        let w = world();
        let joined = json!({ "type": 7, "author": { "id": "200000000000000001" } });
        assert_eq!(system_text(&w, None, &joined).as_deref(), Some("Ada joined"));
        assert_eq!(system_text(&w, None, &json!({ "type": 0 })), None);
    }
}
