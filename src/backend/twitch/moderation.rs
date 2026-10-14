//! What a person can do to a channel's chat, and to themselves, through
//! Twitch's official API: remove a message, time somebody out or ban them (and
//! let them back), change the room's modes, announce, change a name colour,
//! whisper.
//!
//! Twitch no longer reads these as chat text (`/ban` typed into IRC does
//! nothing), so they are API calls, and this reads the slash commands people
//! are used to typing and makes the calls. The window's own menus - a message's
//! Delete, a name's Time out - make the same ones.
//!
//! Only for accounts that were given the rights: signing in asks for no more
//! than chatting needs, and moderation is turned on from the account's card,
//! which signs in again with these added (see `auth::MODERATION_SCOPES`).

use super::helix::{self, Caller, Room};
use crate::state::AppState;

/// One thing to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A person, by login, for this many seconds.
    Timeout { user: String, seconds: u64, reason: String },
    Ban { user: String, reason: String },
    Unban { user: String },
    /// One message by id, or all of them.
    Delete { message_id: Option<String> },
    Slow(Option<u32>),
    /// Followers-only, with the minutes a person must have followed for.
    Followers(Option<u32>),
    Subscribers(bool),
    EmoteOnly(bool),
    Unique(bool),
    Announce { text: String, color: String },
    Color(String),
    Whisper { to: String, text: String },
}

impl Action {
    /// Whether it is something a moderator does to the room, which needs the
    /// right to, as against what a person does as themselves.
    pub fn needs_moderator(&self) -> bool {
        !matches!(self, Action::Color(_) | Action::Whisper { .. })
    }
}

/// Twitch's longest timeout is fourteen days.
const LONGEST_TIMEOUT: u64 = 14 * 24 * 3600;

fn login_of(word: &str) -> Option<String> {
    let login = word.trim().trim_start_matches('@').to_lowercase();
    (!login.is_empty() && login.len() <= 25 && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some(login)
}

/// A length of time as people type it: `600`, `10m`, `2h`, `1d`, `2w`, in seconds.
pub fn duration_seconds(word: &str) -> Option<u64> {
    let word = word.trim().to_lowercase();
    let digits: String = word.chars().take_while(|c| c.is_ascii_digit()).collect();
    let unit = &word[digits.len()..];
    let n: u64 = digits.parse().ok()?;
    let per = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    };
    n.checked_mul(per)
}

/// Reads a slash command. `None` for text that is not one (and for `/me`,
/// which Twitch reads itself); an error for one that is wrong or unknown, in
/// words, so a mistyped command is never sent as a message.
pub fn parse(text: &str) -> Option<Result<Action, String>> {
    let text = text.trim();
    let body = text.strip_prefix('/')?;
    // Twitch's own: sent as typed.
    if body == "me" || body.starts_with("me ") || body.starts_with('/') || body.is_empty() {
        return None;
    }
    let (name, rest) = body.split_once(char::is_whitespace).map(|(n, r)| (n, r.trim())).unwrap_or((body, ""));
    let mut words = rest.split_whitespace();
    let user = |w: Option<&str>| w.and_then(login_of).ok_or_else(|| format!("/{name} needs a user: /{name} <name>"));
    Some(match name.to_lowercase().as_str() {
        "timeout" => (|| {
            let who = user(words.next())?;
            let mut seconds = 600;
            let mut reason: Vec<&str> = Vec::new();
            if let Some(first) = words.next() {
                match duration_seconds(first) {
                    Some(s) => seconds = s,
                    None => reason.push(first),
                }
            }
            reason.extend(words);
            if seconds == 0 || seconds > LONGEST_TIMEOUT {
                return Err("A timeout is from 1 second to 14 days".to_string());
            }
            Ok(Action::Timeout { user: who, seconds, reason: reason.join(" ") })
        })(),
        "ban" => user(words.next()).map(|who| Action::Ban { user: who, reason: words.collect::<Vec<_>>().join(" ") }),
        "unban" | "untimeout" => user(words.next()).map(|who| Action::Unban { user: who }),
        "delete" => match words.next() {
            Some(id) => Ok(Action::Delete { message_id: Some(id.to_string()) }),
            None => Err("/delete needs the id of a message; use a message's own menu to delete one".to_string()),
        },
        "clear" => Ok(Action::Delete { message_id: None }),
        "slow" => match words.next().map(|w| w.parse::<u32>()) {
            None => Ok(Action::Slow(Some(30))),
            Some(Ok(s)) if (3..=120).contains(&s) => Ok(Action::Slow(Some(s))),
            _ => Err("Slow mode is 3 to 120 seconds: /slow <seconds>".to_string()),
        },
        "slowoff" => Ok(Action::Slow(None)),
        "followers" => match words.next() {
            None => Ok(Action::Followers(Some(0))),
            Some(w) => match duration_seconds(&if w.chars().all(|c| c.is_ascii_digit()) { format!("{w}m") } else { w.to_string() }) {
                Some(s) if s / 60 <= 129_600 => Ok(Action::Followers(Some((s / 60) as u32))),
                _ => Err("Followers-only takes a length up to 3 months: /followers 30m, /followers 2h, /followers 1d".to_string()),
            },
        },
        "followersoff" => Ok(Action::Followers(None)),
        "subscribers" | "subscriberson" | "subs" => Ok(Action::Subscribers(true)),
        "subscribersoff" | "subsoff" => Ok(Action::Subscribers(false)),
        "emoteonly" | "emoteonlyon" => Ok(Action::EmoteOnly(true)),
        "emoteonlyoff" => Ok(Action::EmoteOnly(false)),
        "uniquechat" | "r9kbeta" => Ok(Action::Unique(true)),
        "uniquechatoff" | "r9kbetaoff" => Ok(Action::Unique(false)),
        "announce" | "announceblue" | "announcegreen" | "announceorange" | "announcepurple" => {
            let color = name.to_lowercase().trim_start_matches("announce").to_string();
            let color = if color.is_empty() { "primary".to_string() } else { color };
            if rest.is_empty() { Err(format!("/{name} needs something to announce")) } else { Ok(Action::Announce { text: rest.to_string(), color }) }
        }
        "color" => {
            if rest.is_empty() { Err("/color needs a colour: a name like coral or blue_violet, or #rrggbb with Twitch Turbo or Prime".to_string()) } else { Ok(Action::Color(rest.split_whitespace().next().unwrap_or("").to_string())) }
        }
        "w" | "whisper" => match (words.next().and_then(login_of), words.collect::<Vec<_>>().join(" ")) {
            (Some(to), text) if !text.is_empty() => Ok(Action::Whisper { to, text }),
            _ => Err("/w needs a name and a message: /w <name> <message>".to_string()),
        },
        other => Err(format!("Twitch has no /{other} command here. Type // to send a message that starts with a slash.")),
    })
}

/// What is needed to act: who and where.
pub struct Ctx<'a> {
    pub token: &'a str,
    pub my_id: &'a str,
    /// Twitch's number for the channel, from its room state.
    pub room_id: &'a str,
    /// Whether this account is a moderator (or the broadcaster) there.
    pub moderator: bool,
    pub api_root: &'a str,
    pub client_id: &'a str,
}

fn said(r: Result<(), helix::HelixError>, done: String) -> Result<String, String> {
    r.map(|_| done).map_err(|e| e.to_string())
}

/// Does it, and says what was done in words.
pub async fn run(http: &reqwest::Client, cx: &Ctx<'_>, action: &Action, target_id: Option<&str>) -> Result<String, String> {
    if action.needs_moderator() && !cx.moderator {
        return Err("You are not a moderator in this channel".to_string());
    }
    let who = Caller { root: cx.api_root, client_id: cx.client_id, token: cx.token };
    let room = Room { broadcaster_id: cx.room_id, moderator_id: cx.my_id };
    let resolve = |login: &str| {
        let (who, login) = (&who, login.to_string());
        let known = target_id.map(str::to_string);
        async move {
            if let Some(id) = known {
                return Ok(id);
            }
            match helix::user_id_of(http, who, &login).await {
                Ok(Some(id)) => Ok(id),
                Ok(None) => Err(format!("Twitch has no user called {login}")),
                Err(e) => Err(e.to_string()),
            }
        }
    };
    match action {
        Action::Timeout { user, seconds, reason } => {
            let id = resolve(user).await?;
            said(helix::ban(http, &who, &room, &id, Some(*seconds), reason).await, format!("{user} was timed out for {}", super::chat::span(*seconds)))
        }
        Action::Ban { user, reason } => {
            let id = resolve(user).await?;
            said(helix::ban(http, &who, &room, &id, None, reason).await, format!("{user} was banned"))
        }
        Action::Unban { user } => {
            let id = resolve(user).await?;
            said(helix::unban(http, &who, &room, &id).await, format!("{user} is allowed back"))
        }
        Action::Delete { message_id } => said(
            helix::delete_chat(http, &who, &room, message_id.as_deref()).await,
            if message_id.is_some() { "The message was deleted".to_string() } else { "Chat was cleared".to_string() },
        ),
        Action::Slow(Some(s)) => said(helix::chat_settings(http, &who, &room, serde_json::json!({ "slow_mode": true, "slow_mode_wait_time": s })).await, format!("Slow mode is on: {s} seconds")),
        Action::Slow(None) => said(helix::chat_settings(http, &who, &room, serde_json::json!({ "slow_mode": false })).await, "Slow mode is off".to_string()),
        Action::Followers(Some(minutes)) => said(
            helix::chat_settings(http, &who, &room, serde_json::json!({ "follower_mode": true, "follower_mode_duration": minutes })).await,
            if *minutes == 0 { "Followers-only mode is on".to_string() } else { format!("Followers-only mode is on: followers of {}", super::chat::span(*minutes as u64 * 60)) },
        ),
        Action::Followers(None) => said(helix::chat_settings(http, &who, &room, serde_json::json!({ "follower_mode": false })).await, "Followers-only mode is off".to_string()),
        Action::Subscribers(on) => said(helix::chat_settings(http, &who, &room, serde_json::json!({ "subscriber_mode": on })).await, format!("Subscribers-only mode is {}", if *on { "on" } else { "off" })),
        Action::EmoteOnly(on) => said(helix::chat_settings(http, &who, &room, serde_json::json!({ "emote_mode": on })).await, format!("Emote-only mode is {}", if *on { "on" } else { "off" })),
        Action::Unique(on) => said(helix::chat_settings(http, &who, &room, serde_json::json!({ "unique_chat_mode": on })).await, format!("Unique-chat mode is {}", if *on { "on" } else { "off" })),
        Action::Announce { text, color } => said(helix::announce(http, &who, &room, text, color).await, "Announced".to_string()),
        Action::Color(color) => said(helix::set_color(http, &who, cx.my_id, color).await, format!("Your name is {color}")),
        Action::Whisper { to, text } => {
            let id = resolve(to).await?;
            said(helix::whisper(http, &who, cx.my_id, &id, text).await, format!("Whisper sent to {to}"))
        }
    }
}

/// Said in the channel, as a line of its own, so the one who acted - and any
/// other window of theirs - sees what was done.
pub fn say_done(state: &AppState, account_id: &str, channel: &str, what: &str) {
    state.runtime.record_message(state, account_id, channel, "channel", channel, what, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Action {
        parse(text).expect("a command").expect("a good one")
    }

    #[test]
    fn what_is_not_a_command_is_not_read_as_one_and_me_is_twitchs_own() {
        assert!(parse("hello").is_none());
        assert!(parse("/me waves").is_none());
        assert!(parse("//not a command").is_none(), "a doubled slash is how a message starting with one is sent");
        assert!(parse("/").is_none());
    }

    #[test]
    fn a_timeout_has_a_length_and_a_reason_either_of_which_may_be_left_out() {
        assert_eq!(ok("/timeout @Bob"), Action::Timeout { user: "bob".into(), seconds: 600, reason: String::new() });
        assert_eq!(ok("/timeout bob 30"), Action::Timeout { user: "bob".into(), seconds: 30, reason: String::new() });
        assert_eq!(ok("/timeout bob 2h being rude"), Action::Timeout { user: "bob".into(), seconds: 7200, reason: "being rude".into() });
        assert_eq!(ok("/timeout bob spamming links"), Action::Timeout { user: "bob".into(), seconds: 600, reason: "spamming links".into() });
        assert!(parse("/timeout").unwrap().unwrap_err().contains("needs a user"));
        assert!(parse("/timeout bob 99w").unwrap().unwrap_err().contains("14 days"));
    }

    #[test]
    fn bans_unbans_and_deletes() {
        assert_eq!(ok("/ban bob spam"), Action::Ban { user: "bob".into(), reason: "spam".into() });
        assert_eq!(ok("/unban bob"), Action::Unban { user: "bob".into() });
        assert_eq!(ok("/untimeout bob"), Action::Unban { user: "bob".into() });
        assert_eq!(ok("/delete 6a349c82"), Action::Delete { message_id: Some("6a349c82".into()) });
        assert_eq!(ok("/clear"), Action::Delete { message_id: None });
        assert!(parse("/delete").unwrap().is_err());
    }

    #[test]
    fn the_rooms_modes() {
        assert_eq!(ok("/slow"), Action::Slow(Some(30)));
        assert_eq!(ok("/slow 10"), Action::Slow(Some(10)));
        assert!(parse("/slow 1").unwrap().is_err());
        assert_eq!(ok("/slowoff"), Action::Slow(None));
        assert_eq!(ok("/followers"), Action::Followers(Some(0)));
        assert_eq!(ok("/followers 30"), Action::Followers(Some(30)));
        assert_eq!(ok("/followers 2h"), Action::Followers(Some(120)));
        assert_eq!(ok("/followers 1d"), Action::Followers(Some(1440)));
        assert!(parse("/followers soon").unwrap().is_err());
        assert_eq!(ok("/followersoff"), Action::Followers(None));
        assert_eq!((ok("/subscribers"), ok("/subscribersoff")), (Action::Subscribers(true), Action::Subscribers(false)));
        assert_eq!((ok("/emoteonly"), ok("/emoteonlyoff")), (Action::EmoteOnly(true), Action::EmoteOnly(false)));
        assert_eq!((ok("/uniquechat"), ok("/uniquechatoff")), (Action::Unique(true), Action::Unique(false)));
    }

    #[test]
    fn announcements_colours_and_whispers() {
        assert_eq!(ok("/announce Giveaway at noon"), Action::Announce { text: "Giveaway at noon".into(), color: "primary".into() });
        assert_eq!(ok("/announcegreen Hello"), Action::Announce { text: "Hello".into(), color: "green".into() });
        assert!(parse("/announce").unwrap().is_err());
        assert_eq!(ok("/color coral"), Action::Color("coral".into()));
        assert_eq!(ok("/w @Bob hello there"), Action::Whisper { to: "bob".into(), text: "hello there".into() });
        assert!(parse("/w bob").unwrap().is_err());
        assert!(!Action::Color("x".into()).needs_moderator() && Action::Slow(None).needs_moderator());
    }

    #[test]
    fn an_unknown_command_is_said_and_never_sent_as_a_message() {
        let e = parse("/frobnicate now").unwrap().unwrap_err();
        assert!(e.contains("no /frobnicate command") && e.contains("//"), "{e}");
    }

    #[test]
    fn lengths_are_read_as_people_write_them() {
        for (typed, seconds) in [("90", 90), ("30s", 30), ("10m", 600), ("2H", 7200), ("1d", 86_400), ("1w", 604_800)] {
            assert_eq!(duration_seconds(typed), Some(seconds), "{typed}");
        }
        for typed in ["", "m", "1y", "ten"] {
            assert_eq!(duration_seconds(typed), None, "{typed}");
        }
    }

    // The calls themselves, against a Twitch that is a server in a test.
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn helix_server(answers: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        let answers = Arc::new(Mutex::new(answers.into_iter().collect::<std::collections::VecDeque<_>>()));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (log, answers) = (log.clone(), answers.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut got = 0;
                    loop {
                        let n = sock.read(&mut buf[got..]).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        got += n;
                        let text = String::from_utf8_lossy(&buf[..got]).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text[..end].lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                            if got >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    log.lock().unwrap().push(String::from_utf8_lossy(&buf[..got]).to_string());
                    let (status, body) = answers.lock().unwrap().pop_front().unwrap_or((500, "{}"));
                    let reply = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        (root, seen)
    }

    fn cx<'a>(root: &'a str, moderator: bool) -> Ctx<'a> {
        Ctx { token: "tok", my_id: "42", room_id: "100", moderator, api_root: root, client_id: "cid" }
    }

    #[tokio::test]
    async fn a_timeout_looks_the_person_up_and_asks_twitch_for_it_in_the_rooms_name() {
        let (root, seen) = helix_server(vec![(200, r#"{"data":[{"id":"7","login":"bob"}]}"#), (200, r#"{"data":[{}]}"#)]).await;
        let done = run(&reqwest::Client::new(), &cx(&root, true), &Action::Timeout { user: "bob".into(), seconds: 600, reason: "spam".into() }, None).await.unwrap();
        assert_eq!(done, "bob was timed out for 10 minutes");
        let asked = seen.lock().unwrap().clone();
        assert!(asked[0].starts_with("GET /users?login=bob "), "{}", asked[0]);
        assert!(asked[1].starts_with("POST /moderation/bans?broadcaster_id=100&moderator_id=42 "), "{}", asked[1]);
        assert!(asked[1].contains(r#""user_id":"7""#) && asked[1].contains(r#""duration":600"#) && asked[1].contains(r#""reason":"spam""#), "{}", asked[1]);
        assert!(asked[1].to_lowercase().contains("authorization: bearer tok") && asked[1].to_lowercase().contains("client-id: cid"));
    }

    #[tokio::test]
    async fn a_person_whose_id_is_known_is_not_looked_up_and_one_who_does_not_exist_is_said() {
        let (root, seen) = helix_server(vec![(200, "{}"), (200, r#"{"data":[]}"#)]).await;
        let client = reqwest::Client::new();
        run(&client, &cx(&root, true), &Action::Ban { user: "bob".into(), reason: String::new() }, Some("7")).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1, "the id was given, so only the ban was asked for");
        let e = run(&client, &cx(&root, true), &Action::Unban { user: "ghost".into() }, None).await.unwrap_err();
        assert!(e.contains("no user called ghost"), "{e}");
    }

    #[tokio::test]
    async fn the_rooms_modes_are_patched_and_deleting_names_the_message() {
        let (root, seen) = helix_server(vec![(200, "{}"), (200, "{}"), (204, "{}")]).await;
        let client = reqwest::Client::new();
        run(&client, &cx(&root, true), &Action::Slow(Some(30)), None).await.unwrap();
        run(&client, &cx(&root, true), &Action::Followers(Some(60)), None).await.unwrap();
        run(&client, &cx(&root, true), &Action::Delete { message_id: Some("m1".into()) }, None).await.unwrap();
        let asked = seen.lock().unwrap().clone();
        assert!(asked[0].starts_with("PATCH /chat/settings?broadcaster_id=100&moderator_id=42 ") && asked[0].contains(r#""slow_mode_wait_time":30"#), "{}", asked[0]);
        assert!(asked[1].contains(r#""follower_mode":true"#) && asked[1].contains(r#""follower_mode_duration":60"#), "{}", asked[1]);
        assert!(asked[2].starts_with("DELETE /moderation/chat?broadcaster_id=100&moderator_id=42&message_id=m1 "), "{}", asked[2]);
    }

    #[tokio::test]
    async fn not_being_a_moderator_stops_it_before_twitch_is_asked_and_twitchs_no_is_said() {
        let (root, seen) = helix_server(vec![(400, r#"{"message":"The user specified in user_id is already banned."}"#), (401, r#"{"message":"Missing scope: moderator:manage:banned_users"}"#)]).await;
        let client = reqwest::Client::new();
        let e = run(&client, &cx(&root, false), &Action::Slow(None), None).await.unwrap_err();
        assert!(e.contains("not a moderator") && seen.lock().unwrap().is_empty());
        let e = run(&client, &cx(&root, true), &Action::Ban { user: "bob".into(), reason: String::new() }, Some("7")).await.unwrap_err();
        assert!(e.contains("already banned"), "{e}");
        let e = run(&client, &cx(&root, true), &Action::Ban { user: "bob".into(), reason: String::new() }, Some("7")).await.unwrap_err();
        assert!(e.contains("Turn moderation on"), "{e}");
        // Whispers and colours are what a person does as themselves, with no moderator needed.
        let (root, _) = helix_server(vec![(204, "{}"), (204, "{}")]).await;
        run(&client, &cx(&root, false), &Action::Color("coral".into()), None).await.unwrap();
        run(&client, &cx(&root, false), &Action::Whisper { to: "bob".into(), text: "hi".into() }, Some("7")).await.unwrap();
    }
}
