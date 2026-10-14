//! Twitch's official API, Helix: only what moho uses of it.
//!
//! Sending a chat message is the first use. Over IRC a message that Twitch
//! drops (a banned word, slow mode, a link it will not have) is dropped
//! silently, or explained by a NOTICE that nothing ties to the message; Helix
//! answers each message with whether it was sent, and if not, why. It also
//! answers with the message's id, which IRC never does for one's own.
//!
//! Every call carries the account's access token and moho's own Client ID.
//! The functions take the server's address, so the tests can answer them.

use anyhow::Context;
use serde::Deserialize;

pub const API_ROOT: &str = "https://api.twitch.tv/helix";

/// What Twitch said about a message sent.
#[derive(Debug, PartialEq)]
pub struct Sent {
    /// Twitch's id for the message: what a reply to it, or its removal, names.
    pub message_id: String,
    /// Why it was not sent, when it was not: Twitch's words.
    pub dropped: Option<String>,
}

/// Why a message could not be sent by Helix at all.
#[derive(Debug)]
pub enum SendError {
    /// Twitch understood and said no, in words that should reach the person.
    Refused(String),
    /// It never got a hearing - the network, a server error, a token or a
    /// scope Twitch will not take for this call. Sending can fall back to IRC.
    Unavailable(anyhow::Error),
}

#[derive(Deserialize)]
struct SendAnswer {
    #[serde(default)]
    data: Vec<SendResult>,
}

#[derive(Deserialize)]
struct SendResult {
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    is_sent: bool,
    #[serde(default)]
    drop_reason: Option<DropReason>,
}

#[derive(Deserialize)]
struct DropReason {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

/// Where Helix is and who is asking it.
pub struct Caller<'a> {
    pub root: &'a str,
    pub client_id: &'a str,
    pub token: &'a str,
}

/// A message to send: to which channel, as whom, saying what.
pub struct Outgoing<'a> {
    pub broadcaster_id: &'a str,
    pub sender_id: &'a str,
    pub message: &'a str,
    /// The id of the message this answers.
    pub reply_parent: Option<&'a str>,
}

/// Sends a message as the account, optionally as a reply to one.
pub async fn send_chat_message(http: &reqwest::Client, who: &Caller<'_>, out: &Outgoing<'_>) -> std::result::Result<Sent, SendError> {
    let mut body = serde_json::json!({ "broadcaster_id": out.broadcaster_id, "sender_id": out.sender_id, "message": out.message });
    if let Some(parent) = out.reply_parent {
        body["reply_parent_message_id"] = serde_json::json!(parent);
    }
    let res = http
        .post(format!("{}/chat/messages", who.root))
        .header("Authorization", format!("Bearer {}", who.token))
        .header("Client-Id", who.client_id)
        .json(&body)
        .send()
        .await
        .context("reaching Twitch")
        .map_err(SendError::Unavailable)?;
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    let said = || serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string)).unwrap_or_default();
    match status {
        200 => {
            let answer: SendAnswer = serde_json::from_str(&text).context("reading Twitch's answer to a message").map_err(SendError::Unavailable)?;
            let result = answer.data.into_iter().next().ok_or_else(|| SendError::Unavailable(anyhow::anyhow!("Twitch answered a message with nothing")))?;
            let dropped = (!result.is_sent).then(|| match result.drop_reason {
                Some(r) if !r.message.is_empty() => r.message,
                Some(r) if !r.code.is_empty() => format!("Twitch did not send it ({})", r.code),
                _ => "Twitch did not send it".to_string(),
            });
            Ok(Sent { message_id: result.message_id, dropped })
        }
        // Twitch understood and said no: banned, not allowed to talk, a bad id.
        400 | 403 | 422 => Err(SendError::Refused(match said() {
            m if m.is_empty() => format!("Twitch refused the message ({status})"),
            m => m,
        })),
        // The token or what it may do is the problem, which is not a verdict on the message.
        401 => Err(SendError::Unavailable(anyhow::anyhow!("Twitch does not accept this sign-in for sending: {}", said()))),
        429 => Err(SendError::Refused("Twitch says you are sending too fast. Wait a moment.".to_string())),
        other => Err(SendError::Unavailable(anyhow::anyhow!("Twitch answered {other} to a message: {}", said()))),
    }
}

/// The channels an account follows, by login: only the ones live now when
/// `live_only` (one request), otherwise all of them up to `cap`, a hundred a page.
///
/// Needs the `user:read:follows` scope. A 401 means the token will not do
/// for it and is said as such, since the likely cause is an account that
/// signed in before the scope was asked for.
pub async fn followed(http: &reqwest::Client, who: &Caller<'_>, user_id: &str, live_only: bool, cap: usize) -> anyhow::Result<Vec<String>> {
    let mut logins: Vec<String> = Vec::new();
    let mut after = String::new();
    loop {
        let mut url = format!("{}/{}?user_id={user_id}&first=100", who.root, if live_only { "streams/followed" } else { "channels/followed" });
        if !after.is_empty() {
            url.push_str(&format!("&after={after}"));
        }
        let res = http
            .get(&url)
            .header("Authorization", format!("Bearer {}", who.token))
            .header("Client-Id", who.client_id)
            .send()
            .await
            .context("reaching Twitch for the channels you follow")?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            let said = body.get("message").and_then(|m| m.as_str()).unwrap_or("");
            anyhow::bail!("Twitch would not list the channels you follow ({status}){}. Signing in to Twitch again from Accounts may fix it.", if said.is_empty() { String::new() } else { format!(": {said}") });
        }
        for row in body.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
            if let Some(login) = row.get("broadcaster_login").or_else(|| row.get("user_login")).and_then(|l| l.as_str()) {
                logins.push(login.to_lowercase());
            }
        }
        after = body.pointer("/pagination/cursor").and_then(|c| c.as_str()).unwrap_or("").to_string();
        // The live list is one page of a hundred, which is every channel a person follows that is on air at once.
        if live_only || after.is_empty() || logins.len() >= cap {
            break;
        }
    }
    logins.truncate(cap);
    Ok(logins)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn twitch(answers: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
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

    async fn send(root: &str, reply: Option<&str>) -> std::result::Result<Sent, SendError> {
        let who = Caller { root, client_id: "cid", token: "tok" };
        let out = Outgoing { broadcaster_id: "100", sender_id: "42", message: "hello", reply_parent: reply };
        send_chat_message(&reqwest::Client::new(), &who, &out).await
    }

    #[tokio::test]
    async fn a_sent_message_comes_back_with_its_id_and_carries_the_account_and_client() {
        let (root, seen) = twitch(vec![(200, r#"{"data":[{"message_id":"abc","is_sent":true}]}"#)]).await;
        let sent = send(&root, Some("parent-1")).await.unwrap();
        assert_eq!(sent, Sent { message_id: "abc".into(), dropped: None });
        let request = seen.lock().unwrap()[0].clone();
        assert!(request.starts_with("POST /chat/messages"), "{request}");
        assert!(request.to_lowercase().contains("authorization: bearer tok"));
        assert!(request.to_lowercase().contains("client-id: cid"));
        assert!(request.contains(r#""broadcaster_id":"100""#) && request.contains(r#""sender_id":"42""#) && request.contains(r#""reply_parent_message_id":"parent-1""#), "{request}");
    }

    #[tokio::test]
    async fn a_message_twitch_drops_says_why() {
        let (root, _) = twitch(vec![
            (200, r#"{"data":[{"message_id":"","is_sent":false,"drop_reason":{"code":"msg_duplicate","message":"Your message was not sent because it is identical to the previous one you sent, less than 30 seconds ago."}}]}"#),
            (200, r#"{"data":[{"message_id":"","is_sent":false,"drop_reason":{"code":"x","message":""}}]}"#),
        ])
        .await;
        assert!(send(&root, None).await.unwrap().dropped.unwrap().contains("identical to the previous one"));
        assert_eq!(send(&root, None).await.unwrap().dropped.as_deref(), Some("Twitch did not send it (x)"));
    }

    #[tokio::test]
    async fn a_no_is_a_refusal_and_a_bad_token_is_a_reason_to_try_another_way() {
        let (root, _) = twitch(vec![
            (403, r#"{"error":"Forbidden","status":403,"message":"The sender is not permitted to send chat messages in this channel."}"#),
            (401, r#"{"message":"Missing scope: user:write:chat"}"#),
            (429, "{}"),
            (500, "{}"),
        ])
        .await;
        match send(&root, None).await {
            Err(SendError::Refused(why)) => assert!(why.contains("not permitted"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(send(&root, None).await, Err(SendError::Unavailable(_))));
        assert!(matches!(send(&root, None).await, Err(SendError::Refused(_))));
        assert!(matches!(send(&root, None).await, Err(SendError::Unavailable(_))));
        assert!(matches!(send("http://127.0.0.1:9", None).await, Err(SendError::Unavailable(_))));
    }

    #[tokio::test]
    async fn the_channels_somebody_follows_are_listed_page_by_page_up_to_a_cap_and_the_live_ones_in_one_request() {
        let (root, seen) = twitch(vec![
            (200, r#"{"data":[{"broadcaster_login":"Alpha"},{"broadcaster_login":"beta"}],"pagination":{"cursor":"next-page"}}"#),
            (200, r#"{"data":[{"broadcaster_login":"gamma"}],"pagination":{}}"#),
            (200, r#"{"data":[{"user_login":"Live1"},{"user_login":"live2"}],"pagination":{"cursor":"ignored"}}"#),
            (200, r#"{"data":[{"broadcaster_login":"a"},{"broadcaster_login":"b"},{"broadcaster_login":"c"}],"pagination":{"cursor":"more"}}"#),
            (401, r#"{"message":"Missing scope: user:read:follows"}"#),
        ])
        .await;
        let who = Caller { root: &root, client_id: "cid", token: "tok" };
        let client = reqwest::Client::new();
        assert_eq!(followed(&client, &who, "42", false, 200).await.unwrap(), vec!["alpha", "beta", "gamma"]);
        let asked = seen.lock().unwrap().clone();
        assert!(asked[0].starts_with("GET /channels/followed?user_id=42&first=100 "), "{}", asked[0]);
        assert!(asked[1].contains("after=next-page"), "{}", asked[1]);
        assert_eq!(followed(&client, &who, "42", true, 200).await.unwrap(), vec!["live1", "live2"]);
        assert!(seen.lock().unwrap()[2].starts_with("GET /streams/followed?"), "the live list is its own call, once");
        assert_eq!(followed(&client, &who, "42", false, 2).await.unwrap(), vec!["a", "b"], "cut at the cap, and no further page asked for");
        let e = followed(&client, &who, "42", false, 200).await.unwrap_err();
        assert!(e.to_string().contains("Missing scope") && e.to_string().contains("Signing in"), "{e}");
    }
}
