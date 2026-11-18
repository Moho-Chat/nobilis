//! What is done to or about a person, or to a message on its way somewhere else: opening a conversation with
//! somebody met in a group, blocking them, seeing who reacted, forwarding a message, and reporting one.
//!
//! Each is one request to Telegram. Blocking is the account's own list, kept by Telegram, so it holds on the phone
//! as well; the daemon's local ignore list is filled too, because a blocked person's words in a group still arrive.

use super::messages;
use super::peers;
use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::tl;
use grammers_session::types::PeerId;
use grammers_session::Session;
use serde_json::{json, Value};

/// A person's number on Telegram, from the id the window holds for them.
fn user_number(user_id: &str) -> Result<i64, String> {
    user_id.trim().parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(|| "that person cannot be told apart from the others".to_string())
}

/// Opens (makes if need be) the private conversation with somebody, and answers its buffer id.
pub async fn open_dm(state: &AppState, account_id: &str, user_id: &str) -> Result<String, String> {
    let handle = state.runtime.telegram_handle(account_id).ok_or_else(|| "that Telegram account is not connected".to_string())?;
    let id = user_number(user_id)?;
    let peer_ref = handle.session.peer_ref(PeerId::user_unchecked(id)).await.ok().flatten().ok_or_else(|| "Telegram has not told this account about that person yet".to_string())?;
    let peer = handle.client.resolve_peer(peer_ref).await.map_err(|e| explain(&e))?;
    let buffer = peers::buffer_for(state, account_id, &peer);
    if let Some(avatar) = super::media::avatar_url(handle.own.user_id, &peer) {
        state.runtime.set_buffer_avatar(state, &buffer.id, &avatar);
    }
    Ok(buffer.id)
}

/// Blocks somebody on the account, or lets them back.
pub async fn block(state: &AppState, account_id: &str, user_id: &str, blocked: bool) -> Result<(), String> {
    let handle = state.runtime.telegram_handle(account_id).ok_or_else(|| "that Telegram account is not connected".to_string())?;
    let id = user_number(user_id)?;
    let peer = handle.session.peer_ref(PeerId::user_unchecked(id)).await.ok().flatten().ok_or_else(|| "Telegram has not told this account about that person yet".to_string())?;
    let done = if blocked {
        handle.client.invoke(&tl::functions::contacts::Block { my_stories_from: false, id: peer.into() }).await
    } else {
        handle.client.invoke(&tl::functions::contacts::Unblock { my_stories_from: false, id: peer.into() }).await
    };
    done.map(drop).map_err(|e| explain(&e))
}

/// A person's name, from Telegram's own record of them.
fn user_name(user: &tl::enums::User) -> Option<(i64, String)> {
    let tl::enums::User::User(u) = user else { return None };
    let name = [u.first_name.as_deref(), u.last_name.as_deref()].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    let name = if name.is_empty() { u.username.clone().unwrap_or_else(|| "Deleted account".to_string()) } else { name };
    Some((u.id, name))
}

/// A chat's name, for a reaction sent as a channel.
fn chat_name(chat: &tl::enums::Chat) -> Option<(i64, String)> {
    match chat {
        tl::enums::Chat::Chat(c) => Some((c.id, c.title.clone())),
        tl::enums::Chat::Channel(c) => Some((c.id, c.title.clone())),
        _ => None,
    }
}

/// The number Telegram gives a peer, whichever kind it is.
fn peer_number(peer: &tl::enums::Peer) -> i64 {
    match peer {
        tl::enums::Peer::User(p) => p.user_id,
        tl::enums::Peer::Chat(p) => p.chat_id,
        tl::enums::Peer::Channel(p) => p.channel_id,
    }
}

/// Who put one emoji on a message, as far as Telegram shows it (in a big channel it shows nobody).
pub async fn reactors(state: &AppState, buffer: &Buffer, msg_id: &str, emoji: &str) -> Result<Value, String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message has no reactions on Telegram".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let found = handle
        .client
        .invoke(&tl::functions::messages::GetMessageReactionsList {
            peer: peer.into(),
            id,
            reaction: Some(tl::enums::Reaction::Emoji(tl::types::ReactionEmoji { emoticon: emoji.to_string() })),
            offset: None,
            limit: 50,
        })
        .await;
    let list = match found {
        Ok(tl::enums::messages::MessageReactionsList::List(list)) => list,
        // A broadcast channel keeps who reacted to itself; the count is all anybody else may see.
        Err(grammers_client::InvocationError::Rpc(r)) if r.name == "BROADCAST_FORBIDDEN" || r.name == "CHAT_ADMIN_REQUIRED" => return Ok(json!({ "users": [] })),
        Err(e) => return Err(explain(&e)),
    };
    Ok(json!({ "users": reactor_names(&list) }))
}

/// The names in a list of reactions, in Telegram's order.
fn reactor_names(list: &tl::types::messages::MessageReactionsList) -> Vec<Value> {
    let users: std::collections::HashMap<i64, String> = list.users.iter().filter_map(user_name).collect();
    let chats: std::collections::HashMap<i64, String> = list.chats.iter().filter_map(chat_name).collect();
    list.reactions
        .iter()
        .filter_map(|r| {
            let tl::enums::MessagePeerReaction::Reaction(r) = r;
            let n = peer_number(&r.peer_id);
            let name = match r.peer_id {
                tl::enums::Peer::User(_) => users.get(&n),
                _ => chats.get(&n),
            }?;
            Some(json!({ "name": name, "userId": n.to_string() }))
        })
        .collect()
}

/// Sends a message on from one conversation to another of the same account, as Telegram forwards: it says where it
/// came from. Into a forum topic it goes to that topic.
pub async fn forward(state: &AppState, from: &Buffer, msg_id: &str, to: &Buffer) -> Result<(), String> {
    if from.account_id != to.account_id {
        return Err("Telegram forwards within one account".to_string());
    }
    let id: i32 = msg_id.parse().map_err(|_| "that message cannot be forwarded".to_string())?;
    let (handle, source) = peer_for(state, from).await?;
    let (_, target) = peer_for(state, to).await?;
    let topic = super::topics::topic_of_buffer(to);
    let random_id: i64 = rand::random();
    let updates = handle
        .client
        .invoke(&tl::functions::messages::ForwardMessages {
            silent: false,
            background: false,
            with_my_score: false,
            drop_author: false,
            drop_media_captions: false,
            noforwards: false,
            allow_paid_floodskip: false,
            from_ephemeral: false,
            from_peer: source.into(),
            id: vec![id],
            random_id: vec![random_id],
            to_peer: target.into(),
            top_msg_id: topic,
            reply_to: None,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            video_timestamp: None,
            allow_paid_stars: None,
            suggested_post: None,
        })
        .await
        .map_err(|e| match &e {
            grammers_client::InvocationError::Rpc(r) if r.name == "CHAT_FORWARDS_RESTRICTED" => "This chat does not allow its messages to be forwarded".to_string(),
            _ => explain(&e),
        })?;
    // Telegram does not send the account its own message back as an update, so it is read and recorded here.
    if let Some(new_id) = super::send::sent_id(&updates, random_id) {
        if let Ok(found) = handle.client.get_messages_by_id(target, &[new_id]).await {
            if let Some(Some(sent)) = found.into_iter().next() {
                messages::record(state, &to.account_id, &handle.own, &sent);
            }
        }
    }
    Ok(())
}

/// What reporting a message comes to at each step: Telegram asks for a reason from its own list (in the account's
/// language), maybe for a comment, and then says it was reported.
pub fn report_answer(result: &tl::enums::ReportResult) -> Value {
    match result {
        tl::enums::ReportResult::ChooseOption(c) => json!({
            "title": c.title,
            "options": c.options.iter().map(|o| {
                let tl::enums::MessageReportOption::Option(o) = o;
                json!({ "label": o.text, "option": hex::encode(&o.option) })
            }).collect::<Vec<_>>(),
        }),
        tl::enums::ReportResult::AddComment(c) => json!({ "comment": true, "optional": c.optional, "option": hex::encode(&c.option) }),
        tl::enums::ReportResult::Reported => json!({ "done": true }),
    }
}

/// One step of reporting a message to Telegram's moderators. With no `option` it asks what the reasons are.
pub async fn report(state: &AppState, buffer: &Buffer, msg_id: &str, option: &str, comment: &str) -> Result<Value, String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message cannot be reported".to_string())?;
    let option = hex::decode(option.trim()).map_err(|_| "that is not one of Telegram's reasons".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let result = handle
        .client
        .invoke(&tl::functions::messages::Report { peer: peer.into(), id: vec![id], option, message: comment.trim().to_string() })
        .await
        .map_err(|e| explain(&e))?;
    Ok(report_answer(&result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: i64, first: &str, last: Option<&str>, username: Option<&str>) -> tl::enums::User {
        let mut u = tl::types::User {
            is_self: false,
            contact: false,
            mutual_contact: false,
            deleted: false,
            bot: false,
            bot_chat_history: false,
            bot_nochats: false,
            verified: false,
            restricted: false,
            min: false,
            bot_inline_geo: false,
            support: false,
            scam: false,
            apply_min_photo: false,
            fake: false,
            bot_attach_menu: false,
            premium: false,
            attach_menu_enabled: false,
            bot_can_edit: false,
            close_friend: false,
            stories_hidden: false,
            stories_unavailable: false,
            contact_require_premium: false,
            bot_business: false,
            bot_has_main_app: false,
            bot_forum_view: false,
            bot_forum_can_manage_topics: false,
            bot_can_manage_bots: false,
            bot_guestchat: false,
            bot_guard: false,
            linked_community_id: None,
            id,
            access_hash: None,
            first_name: None,
            last_name: last.map(str::to_string),
            username: username.map(str::to_string),
            phone: None,
            photo: None,
            status: None,
            bot_info_version: None,
            restriction_reason: None,
            bot_inline_placeholder: None,
            lang_code: None,
            emoji_status: None,
            usernames: None,
            stories_max_id: None,
            color: None,
            profile_color: None,
            bot_active_users: None,
            bot_verification_icon: None,
            send_paid_messages_stars: None,
        };
        if !first.is_empty() {
            u.first_name = Some(first.to_string());
        }
        tl::enums::User::User(u)
    }

    #[test]
    fn a_persons_name_is_first_and_last_or_their_username() {
        assert_eq!(user_name(&user(1, "Ada", Some("Lovelace"), None)), Some((1, "Ada Lovelace".to_string())));
        assert_eq!(user_name(&user(2, "Ada", None, None)), Some((2, "Ada".to_string())));
        assert_eq!(user_name(&user(3, "", None, Some("ada"))), Some((3, "ada".to_string())));
        assert_eq!(user_name(&user(4, "", None, None)), Some((4, "Deleted account".to_string())));
    }

    #[test]
    fn who_reacted_is_named_in_telegrams_order() {
        let reaction = |peer: tl::enums::Peer| {
            tl::enums::MessagePeerReaction::Reaction(tl::types::MessagePeerReaction {
                big: false,
                unread: false,
                my: false,
                peer_id: peer,
                date: 0,
                reaction: tl::enums::Reaction::Emoji(tl::types::ReactionEmoji { emoticon: "👍".into() }),
            })
        };
        let list = tl::types::messages::MessageReactionsList {
            count: 3,
            reactions: vec![
                reaction(tl::types::PeerUser { user_id: 7 }.into()),
                reaction(tl::types::PeerUser { user_id: 99 }.into()),
                reaction(tl::types::PeerUser { user_id: 5 }.into()),
            ],
            chats: vec![],
            users: vec![user(5, "Bo", None, None), user(7, "Al", None, None)],
            next_offset: None,
        };
        let names: Vec<String> = reactor_names(&list).iter().map(|v| v["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["Al", "Bo"], "somebody Telegram did not describe is left out rather than named by number");
    }

    #[test]
    fn each_step_of_a_report_says_what_comes_next() {
        let choose = tl::enums::ReportResult::ChooseOption(tl::types::ReportResultChooseOption {
            title: "Report".into(),
            options: vec![tl::enums::MessageReportOption::Option(tl::types::MessageReportOption { text: "Spam".into(), option: vec![1, 2] })],
        });
        let a = report_answer(&choose);
        assert_eq!((a["options"][0]["label"].as_str(), a["options"][0]["option"].as_str()), (Some("Spam"), Some("0102")));
        let comment = report_answer(&tl::enums::ReportResult::AddComment(tl::types::ReportResultAddComment { optional: true, option: vec![9] }));
        assert_eq!((comment["comment"].as_bool(), comment["optional"].as_bool(), comment["option"].as_str()), (Some(true), Some(true), Some("09")));
        assert_eq!(report_answer(&tl::enums::ReportResult::Reported)["done"], true);
    }

    #[test]
    fn a_person_is_named_by_a_positive_number() {
        assert_eq!(user_number(" 42 "), Ok(42));
        assert!(user_number("Ada").is_err());
        assert!(user_number("-100").is_err());
    }
}
