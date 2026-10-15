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

/// Why a call to Helix did not work.
#[derive(Debug)]
pub enum HelixError {
    /// Twitch understood and said no, in words that should reach the person.
    Refused(String),
    /// The token or what it was allowed to do will not do for this.
    Auth(String),
    Failed(anyhow::Error),
}

impl std::fmt::Display for HelixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HelixError::Refused(why) => write!(f, "{why}"),
            // A reason about the account itself (no verified phone number) is not cured by signing in again.
            HelixError::Auth(why) if why.to_lowercase().contains("verified") || why.to_lowercase().contains("phone") => write!(f, "Twitch does not allow this for this account ({why}). Verify the account on twitch.tv to enable it."),
            HelixError::Auth(why) => write!(f, "Twitch does not allow this for this account{}. Sign in again from Accounts to refresh what it may do.", if why.is_empty() { String::new() } else { format!(" ({why})") }),
            HelixError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

/// One call to Helix with the account's token: the answer's JSON, or why not.
async fn request(http: &reqwest::Client, who: &Caller<'_>, method: reqwest::Method, path: &str, query: &[(&str, &str)], body: Option<serde_json::Value>) -> Result<serde_json::Value, HelixError> {
    let mut url = format!("{}{path}", who.root);
    if !query.is_empty() {
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in query {
            q.append_pair(k, v);
        }
        url.push('?');
        url.push_str(&q.finish());
    }
    let mut req = http.request(method, &url).header("Authorization", format!("Bearer {}", who.token)).header("Client-Id", who.client_id);
    if let Some(body) = body {
        req = req.json(&body);
    }
    let res = req.send().await.context("reaching Twitch").map_err(HelixError::Failed)?;
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let said = value.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
    match status {
        200..=204 => Ok(value),
        401 => Err(HelixError::Auth(said)),
        // 403 is "you may not do that here", and is a verdict on the action when it carries Twitch's reason.
        400 | 403 | 404 | 409 | 422 | 425 => Err(HelixError::Refused(if said.is_empty() { format!("Twitch refused ({status})") } else { said })),
        429 => Err(HelixError::Refused("Twitch says you are doing that too fast. Wait a moment.".to_string())),
        other => Err(HelixError::Failed(anyhow::anyhow!("Twitch answered {other}{}", if said.is_empty() { String::new() } else { format!(": {said}") }))),
    }
}

/// Twitch's number for a login, or `None` where it has no such user.
pub async fn user_id_of(http: &reqwest::Client, who: &Caller<'_>, login: &str) -> Result<Option<String>, HelixError> {
    let answer = request(http, who, reqwest::Method::GET, "/users", &[("login", login)], None).await?;
    Ok(answer.pointer("/data/0/id").and_then(|i| i.as_str()).map(str::to_string))
}

/// Which channel and who is acting in it: what every moderation call carries.
pub struct Room<'a> {
    pub broadcaster_id: &'a str,
    pub moderator_id: &'a str,
}

/// Times somebody out for `seconds`, or bans them when there is no length.
pub async fn ban(http: &reqwest::Client, who: &Caller<'_>, room: &Room<'_>, user_id: &str, seconds: Option<u64>, reason: &str) -> Result<(), HelixError> {
    let mut data = serde_json::json!({ "user_id": user_id });
    if let Some(seconds) = seconds {
        data["duration"] = serde_json::json!(seconds);
    }
    if !reason.is_empty() {
        data["reason"] = serde_json::json!(reason);
    }
    request(http, who, reqwest::Method::POST, "/moderation/bans", &[("broadcaster_id", room.broadcaster_id), ("moderator_id", room.moderator_id)], Some(serde_json::json!({ "data": data }))).await.map(|_| ())
}

/// Lets somebody back: ends a ban, or a timeout early.
pub async fn unban(http: &reqwest::Client, who: &Caller<'_>, room: &Room<'_>, user_id: &str) -> Result<(), HelixError> {
    request(http, who, reqwest::Method::DELETE, "/moderation/bans", &[("broadcaster_id", room.broadcaster_id), ("moderator_id", room.moderator_id), ("user_id", user_id)], None).await.map(|_| ())
}

/// Removes one message, or - with no id - everything in the chat.
pub async fn delete_chat(http: &reqwest::Client, who: &Caller<'_>, room: &Room<'_>, message_id: Option<&str>) -> Result<(), HelixError> {
    let mut query = vec![("broadcaster_id", room.broadcaster_id), ("moderator_id", room.moderator_id)];
    if let Some(id) = message_id {
        query.push(("message_id", id));
    }
    request(http, who, reqwest::Method::DELETE, "/moderation/chat", &query, None).await.map(|_| ())
}

/// Changes the chat's modes: only the fields given are changed.
pub async fn chat_settings(http: &reqwest::Client, who: &Caller<'_>, room: &Room<'_>, patch: serde_json::Value) -> Result<(), HelixError> {
    request(http, who, reqwest::Method::PATCH, "/chat/settings", &[("broadcaster_id", room.broadcaster_id), ("moderator_id", room.moderator_id)], Some(patch)).await.map(|_| ())
}

/// Makes an announcement in the channel, in one of Twitch's colours.
pub async fn announce(http: &reqwest::Client, who: &Caller<'_>, room: &Room<'_>, message: &str, color: &str) -> Result<(), HelixError> {
    request(http, who, reqwest::Method::POST, "/chat/announcements", &[("broadcaster_id", room.broadcaster_id), ("moderator_id", room.moderator_id)], Some(serde_json::json!({ "message": message, "color": color }))).await.map(|_| ())
}

/// Changes the account's own name colour: one of Twitch's names, or a `#rrggbb` where the account may.
pub async fn set_color(http: &reqwest::Client, who: &Caller<'_>, user_id: &str, color: &str) -> Result<(), HelixError> {
    request(http, who, reqwest::Method::PUT, "/chat/color", &[("user_id", user_id), ("color", color)], None).await.map(|_| ())
}

/// Blocks somebody (for the account, on Twitch's side, everywhere).
pub async fn block(http: &reqwest::Client, who: &Caller<'_>, user_id: &str) -> Result<(), HelixError> {
    request(http, who, reqwest::Method::PUT, "/users/blocks", &[("target_user_id", user_id)], None).await.map(|_| ())
}

/// Lets somebody through again.
pub async fn unblock(http: &reqwest::Client, who: &Caller<'_>, user_id: &str) -> Result<(), HelixError> {
    request(http, who, reqwest::Method::DELETE, "/users/blocks", &[("target_user_id", user_id)], None).await.map(|_| ())
}

/// Points the channel's viewers at another channel, in chat.
pub async fn shoutout(http: &reqwest::Client, who: &Caller<'_>, room: &Room<'_>, to_id: &str) -> Result<(), HelixError> {
    request(http, who, reqwest::Method::POST, "/chat/shoutouts", &[("from_broadcaster_id", room.broadcaster_id), ("to_broadcaster_id", to_id), ("moderator_id", room.moderator_id)], None).await.map(|_| ())
}

/// Sends a whisper.
pub async fn whisper(http: &reqwest::Client, who: &Caller<'_>, from_id: &str, to_id: &str, message: &str) -> Result<(), HelixError> {
    request(http, who, reqwest::Method::POST, "/whispers", &[("from_user_id", from_id), ("to_user_id", to_id)], Some(serde_json::json!({ "message": message }))).await.map(|_| ())
}

/// One emote an account can use in a channel.
#[derive(Debug, PartialEq)]
pub struct UserEmote {
    /// The broadcaster who owns it: a channel's number, or empty / `twitch` for Twitch's own.
    pub owner: String,
    pub id: String,
    pub name: String,
    pub url: String,
    pub animated: bool,
}

/// The emotes an account can use in a channel: Twitch's global ones, the
/// channel's, and any the account has from elsewhere (a subscription to
/// another streamer's that carries over, a follower emote). Needs the
/// `user:read:emotes` scope.
pub async fn user_emotes(http: &reqwest::Client, who: &Caller<'_>, user_id: &str, broadcaster_id: &str, cap: usize) -> anyhow::Result<Vec<UserEmote>> {
    let mut out: Vec<UserEmote> = Vec::new();
    let mut after = String::new();
    loop {
        let mut url = format!("{}/chat/emotes/user?user_id={user_id}&broadcaster_id={broadcaster_id}", who.root);
        if !after.is_empty() {
            url.push_str(&format!("&after={after}"));
        }
        let res = http
            .get(&url)
            .header("Authorization", format!("Bearer {}", who.token))
            .header("Client-Id", who.client_id)
            .send()
            .await
            .context("reaching Twitch for the emotes you can use")?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!("Twitch would not list your emotes ({status})");
        }
        let template = body.get("template").and_then(|t| t.as_str()).unwrap_or("https://static-cdn.jtvnw.net/emoticons/v2/{{id}}/{{format}}/{{theme_mode}}/{{scale}}");
        for e in body.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
            let (Some(id), Some(name)) = (e.get("id").and_then(|i| i.as_str()), e.get("name").and_then(|n| n.as_str())) else { continue };
            let animated = e.get("format").and_then(|f| f.as_array()).is_some_and(|f| f.iter().any(|x| x == "animated"));
            // The still form at twice the size: a cell in the picker is small, and the window animates by token.
            let url = template.replace("{{id}}", id).replace("{{format}}", "static").replace("{{theme_mode}}", "dark").replace("{{scale}}", "2.0");
            let owner = e.get("owner_id").and_then(|o| o.as_str()).unwrap_or("").to_string();
            out.push(UserEmote { owner, id: id.to_string(), name: name.to_string(), url, animated });
        }
        after = body.pointer("/pagination/cursor").and_then(|c| c.as_str()).unwrap_or("").to_string();
        if after.is_empty() || out.len() >= cap {
            break;
        }
    }
    out.truncate(cap);
    Ok(out)
}

/// Every emote a channel has of its own (subscriber tiers, Bits tiers, follower emotes), whether or not an account
/// can use them: what the picker greys out is what is in here and not in `user_emotes`.
pub async fn channel_emotes(http: &reqwest::Client, who: &Caller<'_>, broadcaster_id: &str) -> anyhow::Result<Vec<UserEmote>> {
    let res = http
        .get(format!("{}/chat/emotes?broadcaster_id={broadcaster_id}", who.root))
        .header("Authorization", format!("Bearer {}", who.token))
        .header("Client-Id", who.client_id)
        .send()
        .await
        .context("reaching Twitch for the channel's emotes")?;
    let status = res.status();
    let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        anyhow::bail!("Twitch would not list the channel's emotes ({status})");
    }
    let template = body.get("template").and_then(|t| t.as_str()).unwrap_or("https://static-cdn.jtvnw.net/emoticons/v2/{{id}}/{{format}}/{{theme_mode}}/{{scale}}");
    Ok(body
        .get("data")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let (id, name) = (e.get("id")?.as_str()?, e.get("name")?.as_str()?);
            let animated = e.get("format").and_then(|f| f.as_array()).is_some_and(|f| f.iter().any(|x| x == "animated"));
            let url = template.replace("{{id}}", id).replace("{{format}}", "static").replace("{{theme_mode}}", "dark").replace("{{scale}}", "2.0");
            let owner = e.get("owner_id").and_then(|o| o.as_str()).unwrap_or("").to_string();
            Some(UserEmote { owner, id: id.to_string(), name: name.to_string(), url, animated })
        })
        .collect())
}

/// Who some accounts are: (id, login, name shown, picture), as many as are found. A hundred to a request.
pub async fn users_by_id(http: &reqwest::Client, who: &Caller<'_>, ids: &[String]) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    for chunk in ids.chunks(100) {
        let query: Vec<(&str, &str)> = chunk.iter().map(|i| ("id", i.as_str())).collect();
        let Ok(answer) = request(http, who, reqwest::Method::GET, "/users", &query, None).await else { continue };
        for u in answer.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
            let text = |k: &str| u.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            out.push((text("id"), text("login"), text("display_name"), text("profile_image_url")));
        }
    }
    out
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

    #[tokio::test]
    async fn the_emotes_somebody_may_use_in_a_channel_come_with_their_pictures_and_whether_they_move() {
        let (root, seen) = twitch(vec![
            (200, r#"{"data":[{"id":"25","name":"Kappa","emote_type":"globals","format":["static"]},{"id":"emotesv2_abc","name":"Dance","emote_type":"subscriptions","format":["static","animated"]}],"template":"https://static-cdn.jtvnw.net/emoticons/v2/{{id}}/{{format}}/{{theme_mode}}/{{scale}}","pagination":{"cursor":"p2"}}"#),
            (200, r#"{"data":[{"id":"9","name":"Last","format":["static"]}],"pagination":{}}"#),
            (401, "{}"),
        ])
        .await;
        let who = Caller { root: &root, client_id: "cid", token: "tok" };
        let client = reqwest::Client::new();
        let got = user_emotes(&client, &who, "42", "100", 1000).await.unwrap();
        assert_eq!(got.iter().map(|e| (e.name.as_str(), e.animated)).collect::<Vec<_>>(), vec![("Kappa", false), ("Dance", true), ("Last", false)]);
        assert_eq!(got[0].url, "https://static-cdn.jtvnw.net/emoticons/v2/25/static/dark/2.0");
        assert!(seen.lock().unwrap()[0].starts_with("GET /chat/emotes/user?user_id=42&broadcaster_id=100 "));
        assert!(seen.lock().unwrap()[1].contains("after=p2"));
        assert!(user_emotes(&client, &who, "42", "100", 1000).await.is_err());
    }

    #[tokio::test]
    async fn the_owners_of_emotes_are_named_with_their_pictures() {
        let (root, seen) = twitch(vec![(200, r#"{"data":[{"id":"7","login":"skitzy_va","display_name":"Skitzy_VA","profile_image_url":"https://img/s.png"}]}"#)]).await;
        let who = Caller { root: &root, client_id: "cid", token: "tok" };
        let got = users_by_id(&reqwest::Client::new(), &who, &["7".to_string(), "8".to_string()]).await;
        assert_eq!(got, vec![("7".to_string(), "skitzy_va".to_string(), "Skitzy_VA".to_string(), "https://img/s.png".to_string())]);
        assert!(seen.lock().unwrap()[0].starts_with("GET /users?id=7&id=8 "), "{}", seen.lock().unwrap()[0]);
    }

    #[tokio::test]
    async fn a_channels_own_emotes_are_listed_whether_or_not_they_can_be_used() {
        let (root, seen) = twitch(vec![
            (200, r#"{"data":[{"id":"emotesv2_a","name":"binxSub","emote_type":"subscriptions","format":["static","animated"]},{"id":"7","name":"binxFollow","emote_type":"follower","format":["static"]}],"template":"https://static-cdn.jtvnw.net/emoticons/v2/{{id}}/{{format}}/{{theme_mode}}/{{scale}}"}"#),
            (401, "{}"),
        ])
        .await;
        let who = Caller { root: &root, client_id: "cid", token: "tok" };
        let client = reqwest::Client::new();
        let got = channel_emotes(&client, &who, "100").await.unwrap();
        assert_eq!(got.iter().map(|e| (e.name.as_str(), e.animated)).collect::<Vec<_>>(), vec![("binxSub", true), ("binxFollow", false)]);
        assert_eq!(got[1].url, "https://static-cdn.jtvnw.net/emoticons/v2/7/static/dark/2.0");
        assert!(seen.lock().unwrap()[0].starts_with("GET /chat/emotes?broadcaster_id=100 "));
        assert!(channel_emotes(&client, &who, "100").await.is_err());
    }
}
