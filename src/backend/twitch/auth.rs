//! Signing in to Twitch: the device code flow, and the tokens it leaves.
//!
//! The person never types a password into moho. Moho asks Twitch for a short
//! code, shows it, and the person approves it on twitch.tv in their own
//! browser, signed in however they like; moho polls until Twitch says yes and
//! then holds two tokens:
//!
//! - an **access token**, which every call carries and which lasts four hours
//! - a **refresh token**, single use, which buys the next pair and which
//!   Twitch lets go of after thirty days without one
//!
//! moho is a *public* client (it is a program on somebody's own computer, and
//! cannot keep a secret), so there is no client secret anywhere in this: the
//! device flow and the refresh both go without one, and the Client ID below
//! is not a secret either.
//!
//! The functions take the server's address, so the tests can answer them.
//! Everything here is checked against a real Twitch account in the plan's
//! round 1; what the answers look like is from the docs
//! (dev.twitch.tv/docs/authentication) and from watching them.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The application registered at dev.twitch.tv for moho: a *public* client.
pub const CLIENT_ID: &str = "ybpi52doogxn2gzsa7415i18bq0vlb";

/// Where Twitch's sign-in lives.
pub const ID_ROOT: &str = "https://id.twitch.tv";

/// What moho asks to be allowed, and what each is for.
///
/// No more than the features need, and said beside each in the account form.
/// Moderation and whispers are asked for when somebody turns them on, not
/// here: a scope nobody can see a use for is a reason to refuse.
pub const SCOPES: &[(&str, &str)] = &[
    ("chat:read", "read chat"),
    ("chat:edit", "send messages"),
    ("user:write:chat", "hear why Twitch turned a message down"),
    ("user:read:follows", "list the channels you follow"),
    ("user:read:emotes", "show the emotes you can use"),
];

/// The scopes as Twitch wants them in a request: one string, space between.
pub fn scope_string() -> String {
    SCOPES.iter().map(|(s, _)| *s).collect::<Vec<_>>().join(" ")
}

/// The code to show, and where to enter it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    /// Twitch's own page, with the code already in it.
    pub verification_uri: String,
    /// How long the code can be used, in seconds.
    pub expires_in: u64,
    /// How often to ask whether it has been approved, in seconds.
    pub interval: u64,
}

/// A pair of tokens, as Twitch hands them out.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds from now until the access token stops working.
    pub expires_in: i64,
    #[serde(default, deserialize_with = "scopes_either_way")]
    pub scope: Vec<String>,
}

/// Twitch says the scopes as a list; some of its endpoints have said them as
/// one string. Both are read.
fn scopes_either_way<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        List(Vec<String>),
        Text(String),
    }
    Ok(match Option::<Either>::deserialize(d)? {
        Some(Either::List(list)) => list,
        Some(Either::Text(text)) => text.split_whitespace().map(str::to_string).collect(),
        None => Vec::new(),
    })
}

/// What asking whether the code was approved can answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Poll {
    /// Not yet. Ask again after the interval.
    Pending,
    /// Twitch wants asking less often.
    SlowDown,
    /// The person said no.
    Denied,
    /// The code ran out before anyone approved it.
    Expired,
    Approved(Tokens),
}

/// Who a token belongs to and what it may do.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Validation {
    pub client_id: String,
    pub login: String,
    pub user_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Seconds the token has left.
    pub expires_in: i64,
}

/// Why a token could not be renewed.
#[derive(Debug)]
pub enum Renewal {
    /// Twitch will not accept it any more: revoked, used twice, or thirty
    /// days old. The only answer is to sign in again.
    Refused(String),
    /// Anything else - the network, a server having a bad moment. The token
    /// may well be fine; try again later.
    Failed(anyhow::Error),
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        out.append_pair(k, v);
    }
    out.finish()
}

async fn post_form(http: &reqwest::Client, url: &str, pairs: &[(&str, &str)]) -> Result<(u16, serde_json::Value)> {
    let res = http
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form(pairs))
        .send()
        .await
        .with_context(|| format!("reaching Twitch at {url}"))?;
    let status = res.status().as_u16();
    // An error answer is JSON too, and says why; one that is not is read as nothing.
    let body = res.json::<serde_json::Value>().await.unwrap_or(serde_json::Value::Null);
    Ok((status, body))
}

fn message_of(body: &serde_json::Value) -> String {
    body.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string()
}

/// Asks Twitch for a code for somebody to approve.
pub async fn start(http: &reqwest::Client, root: &str) -> Result<DeviceCode> {
    let (status, body) = post_form(http, &format!("{root}/oauth2/device"), &[("client_id", CLIENT_ID), ("scopes", &scope_string())]).await?;
    if status != 200 {
        bail!("Twitch would not start a sign-in ({status}): {}", message_of(&body));
    }
    serde_json::from_value(body).context("reading Twitch's sign-in code")
}

/// Asks whether the code has been approved.
pub async fn poll(http: &reqwest::Client, root: &str, device_code: &str) -> Result<Poll> {
    let (status, body) = post_form(
        http,
        &format!("{root}/oauth2/token"),
        &[
            ("client_id", CLIENT_ID),
            ("scopes", &scope_string()),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ],
    )
    .await?;
    if status == 200 {
        return Ok(Poll::Approved(serde_json::from_value(body).context("reading the tokens Twitch gave")?));
    }
    // The refusals are the ordinary course of a sign-in, said in a message
    // rather than a status, and spelled as the standard does.
    let message = message_of(&body).to_lowercase();
    Ok(match message.as_str() {
        "authorization_pending" => Poll::Pending,
        m if m.contains("slow_down") => Poll::SlowDown,
        m if m.contains("access_denied") || m.contains("denied") => Poll::Denied,
        m if m.contains("expired") || m.contains("invalid device code") || m.contains("invalid_device_code") => Poll::Expired,
        _ => bail!("Twitch refused the sign-in ({status}): {}", message_of(&body)),
    })
}

/// Buys a new pair of tokens with the refresh token. The old pair is spent
/// whether or not this works out, so what comes back must be kept.
pub async fn refresh(http: &reqwest::Client, root: &str, refresh_token: &str) -> std::result::Result<Tokens, Renewal> {
    let (status, body) = match post_form(
        http,
        &format!("{root}/oauth2/token"),
        &[("client_id", CLIENT_ID), ("grant_type", "refresh_token"), ("refresh_token", refresh_token)],
    )
    .await
    {
        Ok(answer) => answer,
        Err(e) => return Err(Renewal::Failed(e)),
    };
    match status {
        200 => serde_json::from_value(body).map_err(|e| Renewal::Failed(anyhow::Error::new(e).context("reading renewed tokens"))),
        // Twitch says 400 "Invalid refresh token" for one that is used, revoked or old, and 401 for the same family.
        400 | 401 => Err(Renewal::Refused(message_of(&body))),
        other => Err(Renewal::Failed(anyhow::anyhow!("Twitch answered {other} to a renewal: {}", message_of(&body)))),
    }
}

/// Who a token belongs to, or `None` when Twitch no longer accepts it.
///
/// Twitch asks apps to do this when they start and from time to time, and it
/// is how the login, the id and the scopes are read: the sign-in does not say.
pub async fn validate(http: &reqwest::Client, root: &str, access_token: &str) -> Result<Option<Validation>> {
    let res = http
        .get(format!("{root}/oauth2/validate"))
        .header("Authorization", format!("OAuth {access_token}"))
        .send()
        .await
        .context("reaching Twitch to check a sign-in")?;
    match res.status().as_u16() {
        200 => Ok(Some(res.json().await.context("reading who the token belongs to")?)),
        401 => Ok(None),
        other => bail!("Twitch answered {other} when checking a sign-in"),
    }
}

/// Tells Twitch to forget a token, so signing out here ends it there too
/// rather than leaving it good for the rest of its four hours.
pub async fn revoke(http: &reqwest::Client, root: &str, token: &str) -> Result<()> {
    let (status, body) = post_form(http, &format!("{root}/oauth2/revoke"), &[("client_id", CLIENT_ID), ("token", token)]).await?;
    // 400 is a token that was already gone, which is what was wanted.
    if status != 200 && status != 400 {
        bail!("Twitch answered {status} when ending a sign-in: {}", message_of(&body));
    }
    Ok(())
}

/// When a token handed out `expires_in` seconds from `now` should be renewed:
/// a little before it stops, so a call made at the edge does not meet a dead one.
pub fn renew_at(now: i64, expires_in: i64) -> i64 {
    now + (expires_in - RENEW_BEFORE).max(0)
}

/// How long before its end a token is renewed, in seconds.
pub const RENEW_BEFORE: i64 = 300;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A Twitch that answers each request with the next canned answer, and
    /// remembers what it was asked: the request line and the body.
    async fn twitch(answers: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<(String, String)>>> = Default::default();
        let log = seen.clone();
        let answers = Arc::new(Mutex::new(answers.into_iter().collect::<std::collections::VecDeque<_>>()));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (log, answers) = (log.clone(), answers.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut got = 0;
                    // Until the headers and as much body as it says there is.
                    loop {
                        let n = sock.read(&mut buf[got..]).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        got += n;
                        let text = String::from_utf8_lossy(&buf[..got]).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text[..end]
                                .lines()
                                .find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                                .unwrap_or(0);
                            if got >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let text = String::from_utf8_lossy(&buf[..got]).to_string();
                    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                    log.lock().unwrap().push((head.lines().next().unwrap_or("").to_string(), body.to_string()));
                    let (status, answer) = answers.lock().unwrap().pop_front().unwrap_or((500, "{}"));
                    let reply = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        (root, seen)
    }

    fn http() -> reqwest::Client {
        reqwest::Client::new()
    }

    const CODE: &str = r#"{"device_code":"dev123","expires_in":1800,"interval":5,"user_code":"ABCDEFGH","verification_uri":"https://www.twitch.tv/activate?device-code=ABCDEFGH"}"#;
    const TOKENS: &str = r#"{"access_token":"acc","expires_in":14344,"refresh_token":"ref","scope":["chat:read","chat:edit"],"token_type":"bearer"}"#;

    #[tokio::test]
    async fn a_sign_in_asks_for_the_scopes_moho_needs_with_its_own_client_id() {
        let (root, seen) = twitch(vec![(200, CODE)]).await;
        let code = start(&http(), &root).await.unwrap();
        assert_eq!((code.user_code.as_str(), code.interval, code.expires_in), ("ABCDEFGH", 5, 1800));
        assert!(code.verification_uri.ends_with("device-code=ABCDEFGH"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "POST /oauth2/device HTTP/1.1");
        assert!(seen[0].1.contains(&format!("client_id={CLIENT_ID}")), "{}", seen[0].1);
        // Space is a plus in a form, which is how Twitch reads a list of scopes.
        assert!(seen[0].1.contains("scopes=chat%3Aread+chat%3Aedit"), "{}", seen[0].1);
        // And no secret anywhere: moho is a public client.
        assert!(!seen[0].1.contains("secret"));
    }

    #[tokio::test]
    async fn the_poll_tells_waiting_from_a_no_from_an_old_code() {
        let (root, _) = twitch(vec![
            (400, r#"{"status":400,"message":"authorization_pending"}"#),
            (400, r#"{"status":400,"message":"slow_down"}"#),
            (400, r#"{"status":400,"message":"access_denied"}"#),
            (400, r#"{"status":400,"message":"invalid device code"}"#),
            (200, TOKENS),
        ])
        .await;
        let client = http();
        let ask = || poll(&client, &root, "dev123");
        assert_eq!(ask().await.unwrap(), Poll::Pending);
        assert_eq!(ask().await.unwrap(), Poll::SlowDown);
        assert_eq!(ask().await.unwrap(), Poll::Denied);
        assert_eq!(ask().await.unwrap(), Poll::Expired);
        match ask().await.unwrap() {
            Poll::Approved(t) => assert_eq!((t.access_token.as_str(), t.refresh_token.as_str(), t.scope.len()), ("acc", "ref", 2)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn the_poll_names_the_grant_and_the_code() {
        let (root, seen) = twitch(vec![(400, r#"{"message":"authorization_pending"}"#)]).await;
        let _ = poll(&http(), &root, "dev123").await;
        let seen = seen.lock().unwrap();
        assert!(seen[0].1.contains("device_code=dev123"), "{}", seen[0].1);
        assert!(seen[0].1.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"), "{}", seen[0].1);
    }

    #[tokio::test]
    async fn an_answer_nobody_expected_is_an_error_with_twitchs_words() {
        let (root, _) = twitch(vec![(500, r#"{"message":"the sky fell"}"#)]).await;
        let e = poll(&http(), &root, "d").await.unwrap_err();
        assert!(format!("{e}").contains("the sky fell"), "{e}");
    }

    #[tokio::test]
    async fn a_refresh_returns_the_new_pair_and_a_used_one_is_refused() {
        let (root, seen) = twitch(vec![(200, TOKENS), (400, r#"{"status":400,"message":"Invalid refresh token"}"#), (503, "{}")]).await;
        let t = refresh(&http(), &root, "old").await.unwrap();
        assert_eq!(t.access_token, "acc");
        assert!(seen.lock().unwrap()[0].1.contains("grant_type=refresh_token"));
        assert!(seen.lock().unwrap()[0].1.contains("refresh_token=old"));
        match refresh(&http(), &root, "old").await {
            Err(Renewal::Refused(why)) => assert_eq!(why, "Invalid refresh token"),
            other => panic!("{other:?}"),
        }
        // A server having a bad moment is not a verdict on the token.
        assert!(matches!(refresh(&http(), &root, "old").await, Err(Renewal::Failed(_))));
    }

    #[tokio::test]
    async fn validating_reads_who_a_token_belongs_to_and_a_dead_one_is_none() {
        let (root, seen) = twitch(vec![
            (200, r#"{"client_id":"c","login":"someone","scopes":["chat:read"],"user_id":"42","expires_in":3600}"#),
            (401, r#"{"status":401,"message":"invalid access token"}"#),
        ])
        .await;
        let v = validate(&http(), &root, "tok").await.unwrap().expect("accepted");
        assert_eq!((v.login.as_str(), v.user_id.as_str(), v.scopes.len()), ("someone", "42", 1));
        assert!(seen.lock().unwrap()[0].0.starts_with("GET /oauth2/validate"));
        assert!(validate(&http(), &root, "tok").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn revoking_a_token_that_is_already_gone_is_fine() {
        let (root, _) = twitch(vec![(200, "{}"), (400, r#"{"message":"Invalid token"}"#), (500, "{}")]).await;
        revoke(&http(), &root, "t").await.unwrap();
        revoke(&http(), &root, "t").await.unwrap();
        assert!(revoke(&http(), &root, "t").await.is_err());
    }

    #[test]
    fn scopes_are_read_as_a_list_or_as_text() {
        let list: Tokens = serde_json::from_str(r#"{"access_token":"a","refresh_token":"r","expires_in":1,"scope":["a","b"]}"#).unwrap();
        let text: Tokens = serde_json::from_str(r#"{"access_token":"a","refresh_token":"r","expires_in":1,"scope":"a b"}"#).unwrap();
        let none: Tokens = serde_json::from_str(r#"{"access_token":"a","refresh_token":"r","expires_in":1}"#).unwrap();
        assert_eq!((list.scope, text.scope, none.scope), (vec!["a".to_string(), "b".into()], vec!["a".to_string(), "b".into()], vec![]));
    }

    #[test]
    fn a_token_is_renewed_before_it_runs_out() {
        assert_eq!(renew_at(1000, 14400), 1000 + 14400 - RENEW_BEFORE);
        assert_eq!(renew_at(1000, 60), 1000, "a token with less time than the margin is renewed at once");
    }
}
