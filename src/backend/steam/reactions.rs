//! Reacting to a message with an emoticon or a sticker, and seeing others do.
//!
//! A reaction on Steam is an emoticon or sticker named by a word, held by a list
//! of people. moho keeps them as it keeps everyone's: a count, and whether it
//! is ours, under a token - `[steam:emote:steamhappy]` - that the window draws
//! as the picture. The message is found by the id it is stored under, the
//! second Steam stamped it and its place in that second, which is also how
//! Steam names it.

use super::proto::{
    MessageReactionNotice, PastReaction, UpdateMessageReactionRequest,
    UpdateMessageReactionResponse,
};
use super::steamid;
use crate::model::{Buffer, Reaction};
use crate::state::AppState;
use serde_json::json;

/// `EMessageReactionType`.
pub const EMOTICON: i32 = 1;
pub const STICKER: i32 = 2;

fn valid_name(kind: i32, name: &str) -> bool {
    if kind == STICKER {
        return super::emoticons::valid_sticker(name);
    }
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// How a reaction is held here: the token the window draws as a picture.
pub fn token(kind: i32, name: &str) -> Option<String> {
    let name = name.trim().trim_matches(':');
    valid_name(kind, name).then(|| {
        format!(
            "[steam:{}:{name}]",
            if kind == STICKER { "sticker" } else { "emote" }
        )
    })
}

/// What a token held here is, as Steam names it: its kind and its word.
pub fn parse_token(emoji: &str) -> Option<(i32, String)> {
    let inner = emoji.strip_prefix("[steam:")?.strip_suffix(']')?;
    let (kind, name) = inner.split_once(':')?;
    let kind = match kind {
        "emote" => EMOTICON,
        "sticker" => STICKER,
        _ => return None,
    };
    valid_name(kind, name).then(|| (kind, name.to_string()))
}

/// How Steam spells a reaction: an emoticon between colons, as in a message, and a
/// sticker by its bare name. (Found in Steam's own client, and refused as
/// anything else.)
fn on_the_wire(kind: i32, name: &str) -> String {
    if kind == EMOTICON {
        format!(":{name}:")
    } else {
        name.to_string()
    }
}

/// A message's id here, as the second and ordinal Steam names it by.
pub fn parse_message_id(id: &str) -> Option<(u32, u32)> {
    let (ts, ord) = id.split_once(':')?;
    Some((ts.parse().ok()?, ord.parse().ok()?))
}

/// The reactions out of a history entry, as held here.
pub fn from_history(own: u32, list: &[PastReaction]) -> Vec<Reaction> {
    list.iter()
        .filter_map(|r| {
            let emoji = token(r.reaction_type.unwrap_or(EMOTICON), r.reaction.as_deref()?)?;
            (!r.reactors.is_empty()).then(|| Reaction {
                emoji,
                count: r.reactors.len() as i64,
                me: r.reactors.contains(&own),
                animated: false,
            })
        })
        .collect()
}

/// Puts the list of people who hold one reaction on a message, replacing what
/// was held: Steam's answer to a change is who holds it now, which is truer
/// than adding one to a count.
fn set_reactors(
    state: &AppState,
    buffer_id: &str,
    msg_id: &str,
    emoji: &str,
    reactors: &[u32],
    own: u32,
) -> Result<(), String> {
    let message = state
        .store
        .get_message(buffer_id, msg_id)
        .map_err(|e| e.to_string())?
        .ok_or("that message is not in the log")?;
    let mut held: Vec<Reaction> = message.reactions;
    held.retain(|r| r.emoji != emoji);
    if !reactors.is_empty() {
        held.push(Reaction {
            emoji: emoji.to_string(),
            count: reactors.len() as i64,
            me: reactors.contains(&own),
            animated: false,
        });
    }
    state
        .store
        .set_reactions(buffer_id, msg_id, &held)
        .map_err(|e| e.to_string())?;
    state.events.emit(
        "reactionsChanged",
        json!({ "bufferId": buffer_id, "id": msg_id, "reactions": held }),
    );
    Ok(())
}

/// Adds or takes off our reaction on a message.
pub async fn toggle(
    state: &AppState,
    buffer: &Buffer,
    msg_id: &str,
    emoji: &str,
    add: bool,
) -> Result<(), String> {
    let link = state
        .runtime
        .steam_link(&buffer.account_id)
        .ok_or("this Steam account is not connected")?;
    let (kind, name) =
        parse_token(emoji).ok_or("Steam reactions are its own emoticons and stickers")?;
    let (timestamp, ordinal) =
        parse_message_id(msg_id).ok_or("that message has no place in Steam's log")?;
    let friend: u64 = buffer
        .remote_id
        .as_deref()
        .and_then(|id| id.parse().ok())
        .filter(|id| steamid::account_of(*id).is_some())
        .ok_or("this conversation is not with a Steam account")?;
    let own = state
        .accounts
        .get_steam(&buffer.account_id)
        .and_then(|c| steamid::account_of(c.steamid))
        .ok_or("this Steam account is not set up")?;
    let request = UpdateMessageReactionRequest {
        steamid: Some(friend),
        server_timestamp: Some(timestamp),
        ordinal: Some(ordinal),
        reaction_type: Some(kind),
        reaction: Some(on_the_wire(kind, &name)),
        is_add: Some(add),
    };
    let answer: UpdateMessageReactionResponse = link
        .call("FriendMessages.UpdateMessageReaction#1", &request, true)
        .await
        .map_err(|e| super::chat::refusal(&e))?;
    set_reactors(state, &buffer.id, msg_id, emoji, &answer.reactors, own)
}

/// Somebody reacted, or took it back, in a conversation that exists.
pub fn heard(state: &AppState, buffer_id: &str, own: u32, notice: &MessageReactionNotice) {
    let (Some(ts), Some(ord)) = (notice.server_timestamp, notice.ordinal) else {
        return;
    };
    let Some(emoji) = token(
        notice.reaction_type.unwrap_or(EMOTICON),
        notice.reaction.as_deref().unwrap_or_default(),
    ) else {
        return;
    };
    let by_us = notice.reactor.and_then(steamid::account_of) == Some(own);
    state.runtime.update_reaction(
        state,
        buffer_id,
        &super::chat::message_id(ts, ord),
        &emoji,
        by_us,
        notice.is_add.unwrap_or(true),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::simulated_daemon;
    use crate::backend::steam::link::{self, fake::*};
    use prost::Message;
    use std::time::Duration;

    const ACCOUNT: &str = "steam:tester";
    const ME: u32 = 22202;
    const FRIEND: u32 = 100;

    #[test]
    fn a_reaction_is_a_token_for_a_picture_and_back() {
        assert_eq!(
            token(EMOTICON, "steamhappy").as_deref(),
            Some("[steam:emote:steamhappy]")
        );
        assert_eq!(
            token(EMOTICON, ":steamhappy:").as_deref(),
            Some("[steam:emote:steamhappy]"),
            "colons are not part of it"
        );
        assert_eq!(
            token(STICKER, "CatSurprise").as_deref(),
            Some("[steam:sticker:CatSurprise]")
        );
        assert_eq!(token(EMOTICON, "a b"), None);
        assert_eq!(token(EMOTICON, ""), None);
        assert_eq!(
            parse_token("[steam:emote:steamhappy]"),
            Some((EMOTICON, "steamhappy".to_string()))
        );
        assert_eq!(
            parse_token("[steam:sticker:CatSurprise]"),
            Some((STICKER, "CatSurprise".to_string()))
        );
        assert_eq!(
            token(STICKER, "Nyan GLaDOS").as_deref(),
            Some("[steam:sticker:Nyan GLaDOS]"),
            "a sticker's name has spaces in it"
        );
        assert_eq!(
            parse_token("[steam:sticker:Nyan GLaDOS]"),
            Some((STICKER, "Nyan GLaDOS".to_string()))
        );
        for not in [
            "👍",
            "<:name:123>",
            "[steam:other:x]",
            "[steam:emote:a b]",
            "[steam:emote:]",
            "steam:emote:x",
        ] {
            assert_eq!(parse_token(not), None, "{not}");
        }
    }

    #[test]
    fn steam_spells_an_emoticon_with_colons_and_a_sticker_without() {
        assert_eq!(on_the_wire(EMOTICON, "steamhappy"), ":steamhappy:");
        assert_eq!(on_the_wire(STICKER, "CatSurprise"), "CatSurprise");
    }

    #[test]
    fn a_message_is_named_by_its_second_and_ordinal() {
        assert_eq!(parse_message_id("1790000000:3"), Some((1790000000, 3)));
        for not in ["", "1790000000", "a:b", "1:", ":1", "-1:0", "1:2:3"] {
            assert_eq!(parse_message_id(not), None, "{not:?}");
        }
    }

    #[test]
    fn history_reactions_count_who_and_whether_it_is_ours() {
        let list = vec![
            PastReaction {
                reaction_type: Some(1),
                reaction: Some("steamhappy".into()),
                reactors: vec![FRIEND, ME],
            },
            PastReaction {
                reaction_type: Some(2),
                reaction: Some("CatSurprise".into()),
                reactors: vec![FRIEND],
            },
            PastReaction {
                reaction_type: Some(1),
                reaction: Some("empty".into()),
                reactors: vec![],
            },
            PastReaction {
                reaction_type: Some(1),
                reaction: Some("bad name".into()),
                reactors: vec![FRIEND],
            },
        ];
        let got = from_history(ME, &list);
        assert_eq!(
            got.iter()
                .map(|r| (r.emoji.as_str(), r.count, r.me))
                .collect::<Vec<_>>(),
            [
                ("[steam:emote:steamhappy]", 2, true),
                ("[steam:sticker:CatSurprise]", 1, false)
            ]
        );
    }

    fn conversation(name: &str) -> (AppState, Buffer, String) {
        let state = simulated_daemon(name);
        state
            .accounts
            .add_steam(crate::accounts::SteamAccountConfig {
                username: "tester".into(),
                steamid: steamid::steamid_of(ME),
                ..Default::default()
            })
            .unwrap();
        let buffer = state.runtime.ensure_buffer(&state, ACCOUNT, "Gabe", "dm");
        state
            .runtime
            .set_buffer_remote_id(&state, &buffer.id, &steamid::text_of(FRIEND));
        let buffer = state.runtime.get_buffer(&buffer.id).unwrap();
        state.runtime.record_message_at(
            &state,
            ACCOUNT,
            "Gabe",
            "dm",
            "Gabe",
            "hello",
            false,
            "chat",
            None,
            Some("1790000000:1".into()),
            false,
            None,
            vec![],
            vec![],
            None,
            Some(1790000000),
            None,
            None,
        );
        (state, buffer, "1790000000:1".to_string())
    }

    #[tokio::test]
    async fn reacting_asks_steam_and_holds_who_it_says_now_has_it() {
        let (state, buffer, msg) = conversation("steam-react-toggle");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = serve({
            let seen = seen.clone();
            Box::new(move |p| {
                seen.lock()
                    .unwrap()
                    .push(UpdateMessageReactionRequest::decode(p.body.as_slice()).unwrap());
                let is_add = seen.lock().unwrap().last().unwrap().is_add == Some(true);
                // Steam answers who holds it now: the friend always, and us while added.
                let reactors = if is_add {
                    vec![FRIEND, ME]
                } else {
                    vec![FRIEND]
                };
                Some(vec![answer(
                    &p,
                    1,
                    UpdateMessageReactionResponse { reactors }.encode_to_vec(),
                )])
            })
        })
        .await;
        state.runtime.set_steam_link(
            ACCOUNT,
            link::open_with("test", &url, Duration::from_secs(5))
                .await
                .unwrap()
                .0,
        );
        let mut events = state.events.subscribe();

        toggle(&state, &buffer, &msg, "[steam:emote:steamhappy]", true)
            .await
            .unwrap();
        let held = state
            .store
            .get_message(&buffer.id, &msg)
            .unwrap()
            .unwrap()
            .reactions;
        assert_eq!(
            held.iter()
                .map(|r| (r.emoji.as_str(), r.count, r.me))
                .collect::<Vec<_>>(),
            [("[steam:emote:steamhappy]", 2, true)]
        );
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            (
                sent.steamid,
                sent.server_timestamp,
                sent.ordinal,
                sent.reaction_type,
                sent.reaction.as_deref(),
                sent.is_add
            ),
            (
                Some(steamid::steamid_of(FRIEND)),
                Some(1790000000),
                Some(1),
                Some(1),
                Some(":steamhappy:"),
                Some(true)
            )
        );
        assert!(
            std::iter::from_fn(|| events.try_recv().ok()).any(|e| e.name == "reactionsChanged"),
            "the window is told"
        );

        toggle(&state, &buffer, &msg, "[steam:emote:steamhappy]", false)
            .await
            .unwrap();
        let held = state
            .store
            .get_message(&buffer.id, &msg)
            .unwrap()
            .unwrap()
            .reactions;
        assert_eq!(
            held.iter().map(|r| (r.count, r.me)).collect::<Vec<_>>(),
            [(1, false)],
            "ours is off, theirs stays"
        );
    }

    #[tokio::test]
    async fn what_is_not_one_of_steams_is_refused_before_it_is_asked() {
        let (state, buffer, msg) = conversation("steam-react-refused");
        let url = serve(Box::new(|_| panic!("nothing should be asked"))).await;
        state.runtime.set_steam_link(
            ACCOUNT,
            link::open_with("test", &url, Duration::from_secs(5))
                .await
                .unwrap()
                .0,
        );
        assert!(toggle(&state, &buffer, &msg, "👍", true)
            .await
            .unwrap_err()
            .contains("emoticons"));
        assert!(toggle(&state, &buffer, "nonsense", "[steam:emote:x]", true)
            .await
            .is_err());
        // A message that is not in the log has nothing to put it on.
        let url = serve(Box::new(|p| {
            Some(vec![answer(
                &p,
                1,
                UpdateMessageReactionResponse { reactors: vec![ME] }.encode_to_vec(),
            )])
        }))
        .await;
        state.runtime.set_steam_link(
            ACCOUNT,
            link::open_with("test", &url, Duration::from_secs(5))
                .await
                .unwrap()
                .0,
        );
        assert!(toggle(&state, &buffer, "5:5", "[steam:emote:x]", true)
            .await
            .unwrap_err()
            .contains("not in the log"));
    }

    #[test]
    fn somebody_else_reacting_adds_to_the_count_and_taking_it_back_removes_it() {
        let (state, buffer, msg) = conversation("steam-react-heard");
        let notice = |reactor: u32, add: bool| MessageReactionNotice {
            steamid_friend: Some(steamid::steamid_of(FRIEND)),
            server_timestamp: Some(1790000000),
            ordinal: Some(1),
            reactor: Some(steamid::steamid_of(reactor)),
            reaction_type: Some(EMOTICON),
            reaction: Some("steamhappy".into()),
            is_add: Some(add),
        };
        heard(&state, &buffer.id, ME, &notice(FRIEND, true));
        heard(&state, &buffer.id, ME, &notice(ME, true));
        let held = state
            .store
            .get_message(&buffer.id, &msg)
            .unwrap()
            .unwrap()
            .reactions;
        assert_eq!(
            held.iter().map(|r| (r.count, r.me)).collect::<Vec<_>>(),
            [(2, true)],
            "a friend's and ours, from another client"
        );
        heard(&state, &buffer.id, ME, &notice(FRIEND, false));
        heard(&state, &buffer.id, ME, &notice(ME, false));
        assert!(
            state
                .store
                .get_message(&buffer.id, &msg)
                .unwrap()
                .unwrap()
                .reactions
                .is_empty(),
            "none left, none shown"
        );
        // Nonsense is nothing.
        heard(
            &state,
            &buffer.id,
            ME,
            &MessageReactionNotice {
                reaction: Some("bad name".into()),
                ..notice(FRIEND, true)
            },
        );
        heard(
            &state,
            &buffer.id,
            ME,
            &MessageReactionNotice {
                server_timestamp: None,
                ..notice(FRIEND, true)
            },
        );
        assert!(state
            .store
            .get_message(&buffer.id, &msg)
            .unwrap()
            .unwrap()
            .reactions
            .is_empty());
    }
}
