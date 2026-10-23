//! Telegram links: what a `t.me` address or a `tg://` link points at, and taking the window there.
//!
//! A link names a public chat (`t.me/disclosetv`, `tg://resolve?domain=disclosetv`), a message in one
//! (`t.me/disclosetv/21953`, `...&post=21953`), a message in a chat the account is already in
//! (`t.me/c/1234567/89`, `tg://privatepost?channel=1234567&post=89`) or a private invite (`t.me/+hash`,
//! `tg://join?invite=hash`); in a group with topics the topic comes before the message
//! (`t.me/group/5/21953`). Opening one makes the conversation if the account has none (a chat it is not in is
//! joined, which is what a link to a channel asks for) and answers where it is, and which message to go to.
//!
//! The web page Telegram shows for a post has a "View in Telegram" button that opens `tg://resolve?domain=..&post=..`,
//! which is how a click in a browser comes here once moho is the handler for `tg:` links.

use super::peers;
use super::send::explain;
use super::{dialogs, topics, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_session::types::PeerId;
use grammers_session::Session;
use serde_json::{json, Value};

/// What a link points at.
#[derive(Debug, PartialEq)]
pub enum Link {
    /// A chat named by its public @name.
    Public { name: String, topic: Option<i32>, post: Option<i32> },
    /// A chat named by its number, which only an account that is in it can open.
    Private { channel: i64, topic: Option<i32>, post: Option<i32> },
    /// A private invite.
    Invite(String),
}

fn number(text: &str) -> Option<i32> {
    text.parse::<i32>().ok().filter(|n| *n > 0)
}

fn name_ok(name: &str) -> bool {
    (5..=32).contains(&name.len()) && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn invite_ok(hash: &str) -> bool {
    !hash.is_empty() && hash.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Reads a `t.me` / `telegram.me` / `telegram.dog` address or a `tg://` link; `None` for anything else, including the
/// pages on those hosts that are not a chat (sticker packs, proxies, the web preview).
pub fn parse(input: &str) -> Option<Link> {
    let url = url::Url::parse(input.trim()).ok()?;
    let query = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    match url.scheme() {
        "tg" => match url.host_str()? {
            "resolve" => {
                let name = query("domain")?;
                if name == "telegrampassport" || !name_ok(&name) {
                    return None;
                }
                let topic = query("thread").or_else(|| query("topic")).and_then(|t| number(&t));
                Some(Link::Public { name, topic, post: query("post").and_then(|p| number(&p)) })
            }
            "privatepost" => {
                let channel: i64 = query("channel")?.parse().ok().filter(|c| *c > 0)?;
                Some(Link::Private { channel, topic: query("thread").or_else(|| query("topic")).and_then(|t| number(&t)), post: query("post").and_then(|p| number(&p)) })
            }
            "join" => Some(Link::Invite(query("invite").filter(|h| invite_ok(h))?)),
            _ => None,
        },
        "http" | "https" => {
            if !matches!(url.host_str()?.trim_start_matches("www."), "t.me" | "telegram.me" | "telegram.dog") {
                return None;
            }
            let parts: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
            let first = *parts.first()?;
            if let Some(hash) = first.strip_prefix('+') {
                return invite_ok(hash).then(|| Link::Invite(hash.to_string()));
            }
            match first {
                "joinchat" => parts.get(1).filter(|h| invite_ok(h)).map(|h| Link::Invite((*h).to_string())),
                "c" => {
                    let channel: i64 = parts.get(1)?.parse().ok().filter(|c| *c > 0)?;
                    let (topic, post) = match (parts.get(2).and_then(|p| number(p)), parts.get(3).and_then(|p| number(p))) {
                        (Some(topic), Some(post)) => (Some(topic), Some(post)),
                        (Some(post), None) => (None, Some(post)),
                        _ => (None, None),
                    };
                    Some(Link::Private { channel, topic, post })
                }
                // Pages that are about something other than a chat.
                "s" | "addstickers" | "addemoji" | "setlanguage" | "proxy" | "socks" | "share" | "login" | "iv" | "addtheme" | "bg" | "boost" | "m" | "addlist" | "invoice" | "giftcode" | "contact" => None,
                name if name_ok(name) => {
                    let (topic, post) = match (parts.get(1).and_then(|p| number(p)), parts.get(2).and_then(|p| number(p))) {
                        (Some(topic), Some(post)) => (Some(topic), Some(post)),
                        (Some(post), None) => (None, Some(post)),
                        _ => (None, None),
                    };
                    Some(Link::Public { name: name.to_string(), topic, post })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Where a link led.
pub struct Opened {
    pub account_id: String,
    pub buffer: Buffer,
    pub message_id: Option<String>,
}

/// The first connected Telegram account, when the window did not say which.
fn account_for(state: &AppState, wanted: Option<&str>) -> Result<(String, Handle), String> {
    if let Some(id) = wanted {
        return state.runtime.telegram_handle(id).map(|h| (id.to_string(), h)).ok_or_else(|| "that Telegram account is not connected".to_string());
    }
    state
        .accounts
        .all_telegram()
        .into_iter()
        .find_map(|a| {
            let id = a.account_id();
            state.runtime.telegram_handle(&id).map(|h| (id, h))
        })
        .ok_or_else(|| "Add a Telegram account, and wait for it to connect, to open that link".to_string())
}

/// Whether the chat is one this account has a conversation for.
fn known(state: &AppState, account_id: &str, peer: &Peer) -> bool {
    let remote = peers::remote_id(peer.id());
    state.runtime.list_buffers().iter().any(|b| b.account_id == account_id && b.remote_id.as_deref().is_some_and(|r| r == remote || r.strip_prefix(remote.as_str()).is_some_and(|rest| rest.starts_with(':'))))
}

fn chats_of(updates: &tl::enums::Updates) -> Vec<tl::enums::Chat> {
    match updates {
        tl::enums::Updates::Updates(u) => u.chats.clone(),
        tl::enums::Updates::Combined(u) => u.chats.clone(),
        _ => Vec::new(),
    }
}

/// Joins a chat by its invite, and says which chat it was.
async fn join_invite(state: &AppState, account_id: &str, handle: &Handle, hash: &str) -> Result<Peer, String> {
    let checked = handle.client.invoke(&tl::functions::messages::CheckChatInvite { hash: hash.to_string() }).await.map_err(|e| invite_words(&e))?;
    // Already in it: the invite names a chat the account has.
    if let tl::enums::ChatInvite::Already(already) = checked {
        return Ok(Peer::from_raw(&handle.client, already.chat));
    }
    let answer = handle.client.invoke(&tl::functions::messages::ImportChatInvite { hash: hash.to_string() }).await.map_err(|e| invite_words(&e))?;
    let tl::enums::messages::ChatInviteJoinResult::Ok(joined) = answer else {
        return Err("That chat has to be joined from Telegram's own app first".to_string());
    };
    let chat = chats_of(&joined.updates).into_iter().next().ok_or("Telegram did not say which chat that was")?;
    let peer = Peer::from_raw(&handle.client, chat);
    let _ = dialogs::sync(state, account_id, handle).await;
    Ok(peer)
}

fn invite_words(e: &grammers_client::InvocationError) -> String {
    match e {
        grammers_client::InvocationError::Rpc(r) if r.name == "INVITE_HASH_EXPIRED" || r.name == "INVITE_HASH_INVALID" => "That invite link is no longer valid".to_string(),
        grammers_client::InvocationError::Rpc(r) if r.name == "INVITE_REQUEST_SENT" => "Your request to join was sent: an administrator has to accept it".to_string(),
        other => explain(other),
    }
}

/// The conversation a message of a chat is in: the chat's own, or in a group with topics the topic's.
fn buffer_in(state: &AppState, account_id: &str, peer: &Peer, topic: Option<i32>, post: Option<i32>) -> Buffer {
    if topics::is_forum(peer) {
        let remote = peers::remote_id(peer.id());
        return match (topic, post) {
            (Some(topic), _) => topics::buffer_for_topic(state, account_id, peer, topic, None),
            (None, Some(post)) => topics::holding(state, account_id, &remote, post).unwrap_or_else(|| topics::buffer_for_topic(state, account_id, peer, topics::GENERAL, None)),
            (None, None) => topics::buffer_for_topic(state, account_id, peer, topics::GENERAL, None),
        };
    }
    peers::buffer_for(state, account_id, peer)
}

/// Opens a link: finds or makes the conversation it points into, joining the chat if the account is not in it.
pub async fn open(state: &AppState, account: Option<&str>, input: &str) -> Result<Opened, String> {
    let link = parse(input).ok_or_else(|| "That is not a Telegram link moho can open".to_string())?;
    let (account_id, handle) = account_for(state, account)?;
    let (peer, topic, post) = match link {
        Link::Invite(hash) => (join_invite(state, &account_id, &handle, &hash).await?, None, None),
        Link::Public { name, topic, post } => {
            let peer = handle.client.resolve_username(&name).await.map_err(|e| explain(&e))?.ok_or_else(|| format!("Nobody on Telegram is called @{name}"))?;
            if !matches!(peer, Peer::User(_)) && !known(state, &account_id, &peer) {
                // A link to a channel asks to be in it.
                let peer_ref = peer.to_ref().await.ok().flatten().ok_or("Telegram did not say how to reach that chat")?;
                handle.client.join_chat(peer_ref).await.map_err(|e| invite_words(&e))?;
                let _ = dialogs::sync(state, &account_id, &handle).await;
            }
            (peer, topic, post)
        }
        Link::Private { channel, topic, post } => {
            let peer_ref = handle.session.peer_ref(PeerId::channel_unchecked(channel)).await.ok().flatten().ok_or("This account is not in that chat, so a link by its number cannot open it")?;
            (handle.client.resolve_peer(peer_ref).await.map_err(|e| explain(&e))?, topic, post)
        }
    };
    let buffer = buffer_in(state, &account_id, &peer, topic, post);
    Ok(Opened { account_id, buffer, message_id: post.map(|p| p.to_string()) })
}

/// What the window is told.
pub fn answer(opened: &Opened) -> Value {
    json!({ "accountId": opened.account_id, "bufferId": opened.buffer.id, "messageId": opened.message_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(name: &str, topic: Option<i32>, post: Option<i32>) -> Option<Link> {
        Some(Link::Public { name: name.into(), topic, post })
    }

    #[test]
    fn a_web_link_to_a_post_names_the_channel_and_the_post() {
        assert_eq!(parse("https://t.me/disclosetv/21953"), public("disclosetv", None, Some(21953)));
        assert_eq!(parse("https://t.me/disclosetv/21953?single"), public("disclosetv", None, Some(21953)));
        assert_eq!(parse("http://telegram.me/disclosetv"), public("disclosetv", None, None));
        assert_eq!(parse("https://www.t.me/disclosetv/5"), public("disclosetv", None, Some(5)));
    }

    #[test]
    fn in_a_group_with_topics_the_topic_comes_before_the_message() {
        assert_eq!(parse("https://t.me/somegroup/7/21953"), public("somegroup", Some(7), Some(21953)));
        assert_eq!(parse("tg://resolve?domain=somegroup&post=21953&thread=7"), public("somegroup", Some(7), Some(21953)));
    }

    #[test]
    fn the_app_link_a_browsers_view_in_telegram_button_opens_is_read() {
        assert_eq!(parse("tg://resolve?domain=disclosetv&post=21953"), public("disclosetv", None, Some(21953)));
        assert_eq!(parse("tg://resolve?domain=disclosetv"), public("disclosetv", None, None));
        // A bot started with a parameter is the bot's chat.
        assert_eq!(parse("tg://resolve?domain=zlibrary_bot&start=abc"), public("zlibrary_bot", None, None));
        assert_eq!(parse("tg://resolve?domain=telegrampassport&bot_id=1"), None);
    }

    #[test]
    fn a_chat_named_by_its_number_is_private() {
        assert_eq!(parse("https://t.me/c/1234567/89"), Some(Link::Private { channel: 1234567, topic: None, post: Some(89) }));
        assert_eq!(parse("https://t.me/c/1234567/5/89"), Some(Link::Private { channel: 1234567, topic: Some(5), post: Some(89) }));
        assert_eq!(parse("tg://privatepost?channel=1234567&post=89"), Some(Link::Private { channel: 1234567, topic: None, post: Some(89) }));
        assert_eq!(parse("https://t.me/c/notanumber/89"), None);
    }

    #[test]
    fn invites_are_read_in_every_form() {
        let invite = |h: &str| Some(Link::Invite(h.to_string()));
        assert_eq!(parse("https://t.me/+AbC_d-123"), invite("AbC_d-123"));
        assert_eq!(parse("https://t.me/joinchat/AbC_d-123"), invite("AbC_d-123"));
        assert_eq!(parse("tg://join?invite=AbC_d-123"), invite("AbC_d-123"));
        assert_eq!(parse("tg://join?invite="), None);
    }

    #[test]
    fn pages_that_are_not_a_chat_and_other_hosts_are_left_alone() {
        assert_eq!(parse("https://t.me/addstickers/SomePack"), None);
        assert_eq!(parse("https://t.me/s/disclosetv"), None);
        assert_eq!(parse("https://t.me/proxy?server=x&port=1"), None);
        assert_eq!(parse("https://t.me/"), None);
        assert_eq!(parse("https://t.me/ab"), None, "too short to be a name");
        assert_eq!(parse("https://example.com/disclosetv/1"), None);
        assert_eq!(parse("https://t.me.evil.example/disclosetv/1"), None);
        assert_eq!(parse("irc://irc.libera.chat/moho"), None);
        assert_eq!(parse("not a link"), None);
    }
}
