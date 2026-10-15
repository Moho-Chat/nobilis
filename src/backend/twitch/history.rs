//! A channel's recent past.
//!
//! Twitch has no chat history: a client that joins a channel sees it from that
//! moment. The usual answer, Chatterino's among others, is a third-party
//! service that keeps the last few hundred lines of every channel it has been
//! asked about and hands them back as the raw IRC lines Twitch sent
//! (`recent-messages.robotty.de`). They go through the same reading as live
//! ones, recorded under Twitch's own message ids so that one already heard is
//! not shown twice, and timed by Twitch's own timestamps so they sort before
//! the present.
//!
//! It is somebody else's server being told which channel is being opened, so
//! it is off until the person turns it on (Settings, Twitch), and goes through
//! the account's route like every other request.

use super::chat::{self, Rooms};
use super::irc;
use crate::state::AppState;
use anyhow::{bail, Context, Result};

/// The service, and how many lines to ask for.
pub const ROOT: &str = "https://recent-messages.robotty.de/api/v2/recent-messages";
const LIMIT: usize = 400;

/// What the service answers.
#[derive(serde::Deserialize)]
struct Answer {
    #[serde(default)]
    messages: Vec<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Reads a channel's recent lines out of the service's answer into the
/// scrollback. Only what was said - messages, and subscriptions and raids
/// announced - and not what the room did then (a ban, a mode, a notice),
/// which is past and would be wrong to apply now.
pub async fn load(state: &AppState, account_id: &str, channel: &str, root: &str) -> Result<usize> {
    let http = super::client()?;
    let res = http
        .get(format!("{root}/{channel}?limit={LIMIT}&hide_moderated_messages=true&clearchat_to_notice=false"))
        .send()
        .await
        .context("reaching the recent-messages service")?;
    let status = res.status();
    let answer: Answer = res.json().await.unwrap_or(Answer { messages: Vec::new(), error: None });
    if !status.is_success() {
        bail!("the recent-messages service answered {status}{}", answer.error.map(|e| format!(": {e}")).unwrap_or_default());
    }
    let cfg = state.accounts.get_twitch(account_id);
    let mut rooms = Rooms::new(cfg.as_ref().map(|c| c.login.as_str()).unwrap_or(""), "");
    let mut read = 0;
    for raw in &answer.messages {
        let Some(line) = irc::parse(raw) else { continue };
        if line.channel().as_deref() != Some(channel) {
            continue;
        }
        if matches!(line.command.as_str(), "PRIVMSG" | "USERNOTICE") {
            chat::handle(state, account_id, &mut rooms, &line, true);
            read += 1;
        }
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn service(body: String, status: u16) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (log, body) = (log.clone(), body.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string());
                    let reply = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        (root, seen)
    }

    #[tokio::test]
    async fn the_past_is_read_as_the_present_was_and_what_is_already_here_is_not_doubled() {
        let state = testkit::simulated_daemon("twitch-history");
        state.accounts.add_twitch(crate::accounts::TwitchAccountConfig { anonymous: true, ..Default::default() }).unwrap();
        let lines = [
            "@display-name=Old;id=h1;room-id=1;tmi-sent-ts=1791949000000;user-id=5;historical=1 :old!old@old.tmi.twitch.tv PRIVMSG #xqc :said a while ago",
            "@display-name=Fan;id=h2;room-id=1;tmi-sent-ts=1791949001000;user-id=6;historical=1 :fan!fan@fan.tmi.twitch.tv PRIVMSG #xqc :still here",
            "@display-name=Fan;login=fan;msg-id=sub;room-id=1;tmi-sent-ts=1791949002000;user-id=6 :tmi.twitch.tv USERNOTICE #xqc",
            "@room-id=1;target-user-id=5 :tmi.twitch.tv CLEARCHAT #xqc :old",
            "@emote-only=1;room-id=1 :tmi.twitch.tv ROOMSTATE #xqc",
            "@display-name=Elsewhere;id=h9;room-id=2;tmi-sent-ts=1791949003000;user-id=7 :x!x@x.tmi.twitch.tv PRIVMSG #other :another channel",
        ];
        let body = serde_json::json!({ "messages": lines, "error": null }).to_string();
        let (root, seen) = service(body, 200).await;
        // One of them was already heard live.
        state.runtime.record_message_at(&state, "twitch:~anonymous", "xqc", "channel", "Fan", "still here", false, "chat", None, Some("h2".into()), false, None, vec![], vec![], None, Some(1791949001), None, None);

        let read = load(&state, "twitch:~anonymous", "xqc", &root).await.unwrap();
        assert_eq!(read, 3, "two messages and a subscription; not the ban, the mode or another channel");
        assert!(seen.lock().unwrap()[0].contains("/xqc?limit="), "{seen:?}");
        let log = state.store.get_backlog("twitch:~anonymous|xqc", 0, 20).unwrap();
        let bodies: Vec<&str> = log.iter().filter(|m| m.kind == "chat").map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, vec!["said a while ago", "still here"], "in the order they were said, with the one heard live not repeated");
        assert!(log.iter().all(|m| m.moderated.is_none()), "an old ban is not applied now");
        assert!(state.runtime.get_presence("twitch:~anonymous|xqc").is_none(), "the past does not make the roster");
        assert_eq!(state.runtime.get_buffer("twitch:~anonymous|xqc").unwrap().channel_modes, None, "nor the modes");
    }

    #[tokio::test]
    async fn a_service_that_will_not_answer_is_an_error_and_nothing_else() {
        let state = testkit::simulated_daemon("twitch-history-down");
        let (root, _) = service(r#"{"messages":[],"error":"Channel not joined yet"}"#.to_string(), 404).await;
        let e = load(&state, "twitch:~anonymous", "xqc", &root).await.unwrap_err();
        assert!(format!("{e}").contains("404") && format!("{e}").contains("Channel not joined yet"), "{e}");
        assert!(load(&state, "twitch:~anonymous", "xqc", "http://127.0.0.1:9").await.is_err());
    }
}
