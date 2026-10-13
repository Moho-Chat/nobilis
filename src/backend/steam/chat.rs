//! Direct messages: reading what arrives, and sending.
//!
//! A message to a friend is a call to Steam's `FriendMessages` service, and a
//! message from one arrives as a notification from it. Steam stamps each with
//! the second it was sent and an ordinal that orders messages sharing a
//! second, and the pair is the message's identity here: the same message seen
//! twice - once as the answer to our own send, once as the echo to another of
//! our sessions, or replayed after a reconnect - is stored once.
//!
//! Text is shown as Steam holds it with its formatting codes taken off (it
//! sends both). Drawing the formatting itself is a later piece of work; until
//! then a link keeps its text and loses its address.

use super::eresult;
use super::friends::Roster;
use super::link::{CallError, Link};
use super::proto::*;
use super::publish::Conversations;
use super::steamid;
use crate::state::AppState;

/// `EChatEntryType`: an ordinary message. Typing, invitations and the other
/// kinds share the channel and are not messages.
pub const CHAT_MESSAGE: i32 = 1;

/// How long a message may be. Steam's own limit is 2,000 characters per
/// message in its client; longer is refused by the server with an error that
/// says nothing of why, so it is said here first.
pub const MAX_LENGTH: usize = 2000;

/// What a message is called here: when Steam stamped it, and where it came
/// among those stamped the same second.
pub fn message_id(timestamp: u32, ordinal: u32) -> String {
    format!("{timestamp}:{ordinal}")
}

/// A message read out of a notification.
#[derive(Debug, PartialEq)]
pub struct Incoming {
    /// The other person in the conversation.
    pub account: u32,
    pub text: String,
    pub timestamp: u32,
    pub id: String,
    /// Sent by us, from another of our sessions.
    pub from_us: bool,
    /// The web pages Steam's client described in it, which are cards.
    pub embeds: Vec<crate::model::Embed>,
}

/// Reads a notification, or says it is not a message to show.
pub fn read_incoming(n: &FriendMessagesIncoming) -> Option<Incoming> {
    if n.chat_entry_type != Some(CHAT_MESSAGE) {
        return None;
    }
    let account = n.steamid_friend.and_then(steamid::account_of)?;
    // The message as Steam formatted it, read into what moho draws; the plain
    // copy only if that leaves nothing, which is a message made of tags alone.
    let rendered = super::bbcode::render_message(n.message.as_deref().unwrap_or_default());
    let embeds = rendered.cards;
    let text = Some(rendered.text)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| n.message_no_bbcode.as_deref().map(|t| t.trim_end().to_string()))
        .unwrap_or_default();
    if text.trim().is_empty() {
        return None;
    }
    let timestamp = n.rtime32_server_timestamp.unwrap_or_default();
    Some(Incoming { account, text, timestamp, id: message_id(timestamp, n.ordinal.unwrap_or_default()), from_us: n.local_echo.unwrap_or(false), embeds })
}

/// Who is typing to us, from a notification that says so.
pub fn read_typing(n: &FriendMessagesIncoming) -> Option<u32> {
    (n.chat_entry_type == Some(TYPING) && n.local_echo != Some(true)).then_some(n.steamid_friend).flatten().and_then(steamid::account_of)
}

/// Puts a message in the conversation it belongs to, starting the
/// conversation if this is the first word from somebody.
///
/// Returns whether anything was stored, and whether the person is new to the
/// roster and so unknown - the caller asks Steam who they are.
pub fn record_incoming(state: &AppState, account_id: &str, roster: &mut Roster, shown: &mut Conversations, own_name: &str, incoming: &Incoming) -> (bool, bool) {
    let stranger = roster.note_conversation(incoming.account) || (roster.persona(incoming.account).is_none() && !roster.is_friend(incoming.account));
    shown.publish(state, roster);
    let Some(conversation) = shown.name_of(incoming.account).map(str::to_string) else { return (false, stranger) };
    let sender = if incoming.from_us {
        own_name.to_string()
    } else {
        roster.persona(incoming.account).and_then(|p| p.name.clone()).unwrap_or_else(|| conversation.clone())
    };
    let stored = state.runtime.record_message_at(
        state,
        account_id,
        &conversation,
        "dm",
        &sender,
        // Read through the account's text filter, if it has one on: as Steam's own client does.
        &(if incoming.from_us { incoming.text.clone() } else { super::account::filtered(account_id, roster.is_friend(incoming.account), &incoming.text) }),
        false,
        "chat",
        None,
        Some(incoming.id.clone()),
        false,
        roster.persona(incoming.account).filter(|_| !incoming.from_us).and_then(|p| p.avatar.clone()),
        incoming.embeds.clone(),
        Vec::new(),
        (!incoming.from_us).then(|| steamid::text_of(incoming.account)),
        Some(i64::from(incoming.timestamp)).filter(|t| *t > 0),
        None,
        None,
    );
    (stored, stranger)
}

/// Why a message could not be sent, in words for a person.
pub fn refusal(error: &CallError) -> String {
    match error {
        CallError::Refused(code, _) => eresult::describe(*code),
        other => other.to_string(),
    }
}

/// Sends a message to the person a conversation is with.
///
/// `recipient` is their 64-bit id as the conversation holds it. Returns the
/// id Steam gave the message and the second it stamped it, which the caller
/// stores under so that an echo of it is recognised.
pub async fn send(link: &Link, recipient: &str, text: &str) -> Result<Sent, String> {
    let steamid: u64 = recipient.parse().ok().filter(|id| steamid::account_of(*id).is_some()).ok_or("this conversation is not with a Steam account")?;
    let text = text.trim_end();
    if text.trim().is_empty() {
        return Err("there is nothing to send".to_string());
    }
    if text.chars().count() > MAX_LENGTH {
        return Err(format!("Steam takes messages of up to {MAX_LENGTH} characters; this one is {}", text.chars().count()));
    }
    let request = FriendMessagesSendRequest {
        steamid: Some(steamid),
        chat_entry_type: Some(CHAT_MESSAGE),
        message: Some(text.to_string()),
        // The text is BBCode, as Steam's own client sends it: the composer
        // writes the formatting as tags and escapes any bracket somebody typed
        // as `\[`, so a "[b]" they meant literally stays what they meant.
        contains_bbcode: Some(true),
        echo_to_sender: Some(false),
        low_priority: Some(false),
        client_message_id: Some(client_message_id()),
    };
    let response: FriendMessagesSendResponse = link
        .call("FriendMessages.SendMessage#1", &request, true)
        .await
        .map_err(|e| {
            tracing::info!("steam: SendMessage was answered with {e:?}");
            refusal(&e)
        })?;
    tracing::debug!("steam: sent {text:?}; Steam stored {:?} ({:?} without tags)", response.modified_message, response.message_without_bb_code);
    let timestamp = response.server_timestamp.unwrap_or_default();
    // What Steam stored, which is the text with its emoticons found: ":steamhappy:"
    // comes back as a tag, and the log should hold what the other side will see.
    let said = response.modified_message.as_deref().filter(|m| !m.trim().is_empty()).unwrap_or(text);
    let rendered = super::bbcode::render_message(said);
    Ok(Sent { id: message_id(timestamp, response.ordinal.unwrap_or_default()), timestamp, shown: rendered.text, embeds: rendered.cards })
}

/// A message that Steam took.
#[derive(Debug)]
pub struct Sent {
    /// The id Steam gave it, which an echo of it is recognised by.
    pub id: String,
    /// The second Steam stamped it.
    pub timestamp: u32,
    /// What to put in the log: the message as moho draws it.
    pub shown: String,
    /// The cards Steam made of any links in it, which the sender's own copy
    /// holds as the other side's does.
    pub embeds: Vec<crate::model::Embed>,
}

/// What a typing notice is, in `chat_entry_type`.
pub const TYPING: i32 = 2;

/// Tells somebody we are writing to them. A message of the other kind with no
/// text, which is how Steam's own client says it; sent without waiting, since
/// nothing depends on it arriving.
pub fn send_typing(link: &Link, recipient: &str) {
    let Some(steamid) = recipient.parse::<u64>().ok().filter(|id| steamid::account_of(*id).is_some()) else { return };
    let link = link.clone();
    tokio::spawn(async move {
        let request = FriendMessagesSendRequest { steamid: Some(steamid), chat_entry_type: Some(TYPING), ..Default::default() };
        let _: Result<FriendMessagesSendResponse, _> = link.call("FriendMessages.SendMessage#1", &request, true).await;
    });
}

/// A fresh name for one send, so that sending it twice cannot say it twice.
fn client_message_id() -> String {
    use rand::Rng;
    // Sixteen hex digits, as Steam's own client makes them.
    let n: u64 = rand::thread_rng().gen();
    format!("{n:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn typing_is_a_message_of_the_other_kind_with_no_words_and_not_a_message() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = serve({
            let seen = seen.clone();
            Box::new(move |p| {
                seen.lock().unwrap().push(p.clone());
                Some(vec![answer(&p, 1, FriendMessagesSendResponse::default().encode_to_vec())])
            })
        })
        .await;
        let (link, _told) = link::open_with("test", &url, Duration::from_secs(5)).await.unwrap();
        send_typing(&link, "76561197960265729");
        // Nothing to send to when it is not somebody's id.
        send_typing(&link, "not an id");
        for _ in 0..100 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].header.target_job_name.as_deref(), Some("FriendMessages.SendMessage#1"));
        let body = FriendMessagesSendRequest::decode(seen[0].body.as_slice()).unwrap();
        assert_eq!((body.steamid, body.chat_entry_type, body.message), (Some(76561197960265729), Some(2), None));
    }

    #[test]
    fn an_incoming_message_is_read_from_its_tags() {
        let said = |message: &str, plain: &str| FriendMessagesIncoming {
            steamid_friend: Some(steamid::steamid_of(7)),
            chat_entry_type: Some(1),
            message: Some(message.into()),
            message_no_bbcode: Some(plain.into()),
            rtime32_server_timestamp: Some(5),
            ..Default::default()
        };
        assert_eq!(read_incoming(&said("look [b]here[/b] [emoticon]steamhappy[/emoticon]", "look here :steamhappy:")).unwrap().text, "look **here** [steam:emote:steamhappy]");
        // Nothing but tags: the plain copy is what there is to show.
        assert_eq!(read_incoming(&said("[roomeffect]x[/roomeffect]", "sends an effect")).unwrap().text, "sends an effect");
        assert_eq!(read_incoming(&said("   ", "  ")), None, "and nothing at all is not a message");
    }

    #[test]
    fn somebody_typing_is_told_from_a_message_and_from_ourselves_elsewhere() {
        let from = |kind, echo| FriendMessagesIncoming { steamid_friend: Some(steamid::steamid_of(7)), chat_entry_type: Some(kind), local_echo: Some(echo), ..Default::default() };
        assert_eq!(read_typing(&from(2, false)), Some(7));
        assert_eq!(read_typing(&from(1, false)), None, "a message is not typing");
        assert_eq!(read_typing(&from(2, true)), None, "our own typing, from another client, is not somebody else's");
        assert_eq!(read_typing(&FriendMessagesIncoming { chat_entry_type: Some(2), ..Default::default() }), None, "from nobody");
    }

    /// Steam refuses a message whose id is the wrong shape - as "not an
    /// account", which says nothing of the id - and the shape that is accepted
    /// is what its own client makes: sixteen hex digits. Found against a live
    /// account, where thirty-two were refused and sixteen sent.
    #[test]
    fn a_message_id_is_sixteen_hex_digits_and_not_the_same_twice() {
        let a = client_message_id();
        let b = client_message_id();
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
    use crate::backend::kick::testkit::simulated_daemon;
    use crate::backend::steam::friends::REL_FRIEND;
    use crate::backend::steam::link::{self, fake::*};
    use crate::backend::steam::wire::emsg;
    use std::time::Duration;
    use prost::Message;

    const ACCOUNT: &str = "steam:tester";

    fn notice(friend: u32, text: &str, ts: u32, ordinal: u32) -> FriendMessagesIncoming {
        FriendMessagesIncoming {
            steamid_friend: Some(steamid::steamid_of(friend)),
            chat_entry_type: Some(1),
            message: Some(text.to_string()),
            message_no_bbcode: Some(text.to_string()),
            rtime32_server_timestamp: Some(ts),
            ordinal: Some(ordinal),
            ..Default::default()
        }
    }

    #[test]
    fn a_message_is_named_by_when_steam_stamped_it_and_its_place_in_that_second() {
        assert_eq!(message_id(1790000000, 0), "1790000000:0");
        assert_ne!(message_id(1790000000, 0), message_id(1790000000, 1), "two in the same second are two messages");
        let read = read_incoming(&notice(22202, "hello", 1790000000, 3)).unwrap();
        assert_eq!((read.account, read.text.as_str(), read.id.as_str(), read.from_us), (22202, "hello", "1790000000:3", false));
    }

    #[test]
    fn what_is_not_a_message_to_show_is_not_one() {
        let mut typing = notice(22202, "x", 1, 0);
        typing.chat_entry_type = Some(2);
        assert_eq!(read_incoming(&typing), None, "typing is not a message");
        let mut other = notice(22202, "x", 1, 0);
        other.chat_entry_type = None;
        assert_eq!(read_incoming(&other), None);
        let mut empty = notice(22202, "   ", 1, 0);
        empty.message = Some("   ".into());
        assert_eq!(read_incoming(&empty), None);
        let mut group = notice(22202, "x", 1, 0);
        group.steamid_friend = Some((1u64 << 56) | (7 << 52) | (1 << 32) | 5);
        assert_eq!(read_incoming(&group), None, "not from a person");
        let mut nobody = notice(22202, "x", 1, 0);
        nobody.steamid_friend = None;
        assert_eq!(read_incoming(&nobody), None);
    }

    #[test]
    fn the_text_is_the_message_and_trailing_space_is_dropped() {
        assert_eq!(read_incoming(&notice(1, "plain words", 1, 0)).unwrap().text, "plain words");
        let mut raw_only = notice(1, "", 1, 0);
        raw_only.message_no_bbcode = None;
        raw_only.message = Some("raw words  ".into());
        assert_eq!(read_incoming(&raw_only).unwrap().text, "raw words");
    }

    #[test]
    fn a_message_we_sent_from_another_session_is_ours() {
        let mut echo = notice(22202, "sent from my phone", 5, 0);
        echo.local_echo = Some(true);
        assert!(read_incoming(&echo).unwrap().from_us);
    }

    #[test]
    fn a_page_steam_described_arrives_as_a_message_with_a_card() {
        let state = simulated_daemon("steam-chat-card");
        let mut roster = Roster::default();
        roster.apply_friends_list(&ClientFriendsList {
            friends: vec![FriendsListEntry { ulfriendid: Some(steamid::steamid_of(22202)), efriendrelationship: Some(REL_FRIEND) }],
            ..Default::default()
        });
        roster.apply_persona(&ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(22202)), player_name: Some("Gabe".into()), persona_state: Some(1), ..Default::default() }], ..Default::default() });
        let mut shown = Conversations::new(ACCOUNT);
        let raw = "[og url=\"http://x.salastil.com\" title=\"Home\" desc=\"A privacy-focused Twitter/X frontend\"]x.salastil.com[/og]";
        let incoming = read_incoming(&notice(22202, raw, 1790000100, 0)).unwrap();
        assert_eq!(incoming.text, "[x.salastil.com](http://x.salastil.com)");
        assert!(record_incoming(&state, ACCOUNT, &mut roster, &mut shown, "Me", &incoming).0);
        let history = state.store.get_backlog(&crate::model::buffer_id(ACCOUNT, "Gabe"), 0, 10).unwrap();
        let card = &history[0].embeds[0];
        assert_eq!((card.url.as_deref(), card.title.as_deref(), card.provider.as_deref()), (Some("http://x.salastil.com"), Some("Home"), Some("x.salastil.com")));
    }

    #[test]
    fn a_message_lands_in_the_friends_conversation_once() {
        let state = simulated_daemon("steam-chat-record");
        let mut roster = Roster::default();
        roster.apply_friends_list(&ClientFriendsList {
            friends: vec![FriendsListEntry { ulfriendid: Some(steamid::steamid_of(22202)), efriendrelationship: Some(REL_FRIEND) }],
            ..Default::default()
        });
        roster.apply_persona(&ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(22202)), player_name: Some("Gabe".into()), persona_state: Some(1), ..Default::default() }], ..Default::default() });
        let mut shown = Conversations::new(ACCOUNT);

        let incoming = read_incoming(&notice(22202, "hello there", 1790000000, 0)).unwrap();
        let (stored, stranger) = record_incoming(&state, ACCOUNT, &mut roster, &mut shown, "Me", &incoming);
        assert!(stored && !stranger);
        let (again, _) = record_incoming(&state, ACCOUNT, &mut roster, &mut shown, "Me", &incoming);
        assert!(!again, "the same message twice is one message");

        let history = state.store.get_backlog(&crate::model::buffer_id(ACCOUNT, "Gabe"), 0, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!((history[0].from.as_str(), history[0].body.as_str(), history[0].ts), ("Gabe", "hello there", 1790000000));
        assert_eq!(history[0].id, "1790000000:0");
    }

    #[test]
    fn what_we_sent_from_elsewhere_is_filed_under_the_friend_and_signed_by_us() {
        let state = simulated_daemon("steam-chat-echo");
        let mut roster = Roster::default();
        roster.apply_friends_list(&ClientFriendsList {
            friends: vec![FriendsListEntry { ulfriendid: Some(steamid::steamid_of(22202)), efriendrelationship: Some(REL_FRIEND) }],
            ..Default::default()
        });
        roster.apply_persona(&ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(22202)), player_name: Some("Gabe".into()), persona_state: Some(1), ..Default::default() }], ..Default::default() });
        let mut shown = Conversations::new(ACCOUNT);
        state.runtime.set_own_identity(ACCOUNT, "Me");

        let mut echo = notice(22202, "from my phone", 1790000001, 0);
        echo.local_echo = Some(true);
        record_incoming(&state, ACCOUNT, &mut roster, &mut shown, "Me", &read_incoming(&echo).unwrap());
        let history = state.store.get_backlog(&crate::model::buffer_id(ACCOUNT, "Gabe"), 0, 10).unwrap();
        assert_eq!(history.len(), 1, "in Gabe's conversation, not a new one");
        assert_eq!(history[0].from, "Me");
        assert!(history[0].is_own);
    }

    #[test]
    fn somebody_who_writes_first_gets_a_conversation_and_is_asked_after() {
        let state = simulated_daemon("steam-chat-stranger");
        let mut roster = Roster::default();
        let mut shown = Conversations::new(ACCOUNT);
        let incoming = read_incoming(&notice(777, "hi, we have not met", 1790000002, 0)).unwrap();
        let (stored, stranger) = record_incoming(&state, ACCOUNT, &mut roster, &mut shown, "Me", &incoming);
        assert!(stored, "a message from a stranger is still a message");
        assert!(stranger, "and the caller should find out who they are");
        let name = shown.name_of(777).unwrap().to_string();
        assert_eq!(name, steamid::text_of(777));
        assert_eq!(state.store.get_backlog(&crate::model::buffer_id(ACCOUNT, &name), 0, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sending_names_the_recipient_sends_the_tags_and_logs_what_steam_stored() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let url = serve({
            let seen = seen.clone();
            Box::new(move |req| {
                *seen.lock().unwrap() = Some(req.clone());
                Some(vec![answer(&req, 1, FriendMessagesSendResponse { modified_message: Some("hi [b]there[/b] [emoticon]steamhappy[/emoticon]".into()), server_timestamp: Some(1790000003), ordinal: Some(2), message_without_bb_code: Some("hi there".into()) }.encode_to_vec())])
            })
        })
        .await;
        let (link, _told) = link::open("steam:test", &url).await.unwrap();
        link.set_identity(link::Identity { steamid: 76561197960287930, session: 4 });

        let sent = send(&link, "76561197960265729", "hi [b]there[/b] :steamhappy:  \n").await.unwrap();
        assert_eq!((sent.id.as_str(), sent.timestamp), ("1790000003:2", 1790000003));
        assert_eq!(sent.shown, "hi **there** [steam:emote:steamhappy]", "what is logged is what Steam stored, drawn as moho draws it");
        let req = seen.lock().unwrap().clone().unwrap();
        assert_eq!(req.emsg, emsg::SERVICE_METHOD_CALL_FROM_CLIENT, "as the signed-in account");
        assert_eq!(req.header.target_job_name.as_deref(), Some("FriendMessages.SendMessage#1"));
        let body = FriendMessagesSendRequest::decode(req.body.as_slice()).unwrap();
        assert_eq!(body.steamid, Some(76561197960265729));
        assert_eq!(body.message.as_deref(), Some("hi [b]there[/b] :steamhappy:"), "trailing space is dropped; the tags are sent as they came");
        assert_eq!((body.chat_entry_type, body.contains_bbcode), (Some(1), Some(true)));
        assert_eq!(body.client_message_id.as_deref().map(str::len), Some(16));
    }

    #[tokio::test]
    async fn a_link_sent_keeps_the_card_steam_made_of_it() {
        let url = serve(Box::new(|req| {
            let stored = "[og url=\"https://x.test/p\" img=\"https://community.steamstatic.com/chat/image/a/share_image\" title=\"A page\" desc=\"About it\"]https://x.test/p[/og]";
            Some(vec![answer(&req, 1, FriendMessagesSendResponse { modified_message: Some(stored.into()), server_timestamp: Some(1790000004), ordinal: Some(0), message_without_bb_code: None }.encode_to_vec())])
        }))
        .await;
        let (link, _told) = link::open("steam:test", &url).await.unwrap();
        link.set_identity(link::Identity { steamid: 76561197960287930, session: 4 });
        let sent = send(&link, "76561197960265729", "https://x.test/p").await.unwrap();
        assert_eq!(sent.shown, "https://x.test/p", "the words are what was sent");
        assert_eq!(sent.embeds.len(), 1, "and the card is the sender's too");
        assert_eq!((sent.embeds[0].title.as_deref(), sent.embeds[0].description.as_deref()), (Some("A page"), Some("About it")));
        assert!(sent.embeds[0].image_url.is_some());
    }

    #[tokio::test]
    async fn what_cannot_be_sent_is_said_before_it_is_tried() {
        let heard = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let url = serve({
            let heard = heard.clone();
            Box::new(move |req| {
                heard.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(vec![answer(&req, 1, FriendMessagesSendResponse::default().encode_to_vec())])
            })
        })
        .await;
        let (link, _told) = link::open("steam:test", &url).await.unwrap();
        for (to, text, why) in [
            ("not a number", "hi", "not with a Steam account"),
            ("5", "hi", "not with a Steam account"),
            ("76561197960265729", "   ", "nothing to send"),
        ] {
            let err = send(&link, to, text).await.unwrap_err();
            assert!(err.contains(why), "{to:?} {text:?}: {err}");
        }
        let err = send(&link, "76561197960265729", &"x".repeat(2001)).await.unwrap_err();
        assert!(err.contains("2000"), "{err}");
        assert_eq!(heard.load(std::sync::atomic::Ordering::SeqCst), 0, "none of those reached Steam");

        // Exactly the limit is fine.
        send(&link, "76561197960265729", &"x".repeat(2000)).await.unwrap();
        assert_eq!(heard.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn steams_refusals_are_said_in_words() {
        let url = serve(Box::new(|req| Some(vec![answer(&req, eresult::ACCOUNT_NOT_FRIENDS, Vec::new())]))).await;
        let (link, _told) = link::open("steam:test", &url).await.unwrap();
        let err = send(&link, "76561197960265729", "hello").await.unwrap_err();
        assert!(err.contains("friends"), "{err}");
    }
}
