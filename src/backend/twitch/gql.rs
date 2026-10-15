//! Twitch's own GraphQL, the one twitch.tv's pages are built on.
//!
//! It is private - Twitch does not offer it to anybody else, and it has been
//! changed before under the clients that use it (the plan, Gitea issue #55,
//! says why moho uses it anyway and what happens if it breaks). Three things
//! are asked of it, none of which Twitch's official API will answer without an
//! account: whether a channel is live and what it is called, the signed token
//! a stream's playlist needs, and the badges a channel's chat draws.
//!
//! It is spoken as twitch.tv speaks it: the site's own Client ID, and queries
//! that are either plain GraphQL or, for the player's token, the *persisted*
//! query the player sends by hash. Answers are checked with care - a response
//! that is not what was expected is an error to say, never a crash.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub const ROOT: &str = "https://gql.twitch.tv/gql";

/// The Client ID twitch.tv's own pages send. Public (it is in every page),
/// and the only one the GraphQL endpoint accepts: it answers 400 "The
/// Client-ID header is invalid" to moho's own.
pub const SITE_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";

/// A device id for this run, the one the site makes up for a browser: random
/// and sent with every request, so the token Twitch signs is for one device.
pub fn device_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let a = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(1);
        let b = std::process::id() as u128;
        format!("{:032x}", a.wrapping_mul(6364136223846793005).wrapping_add(b))
    })
}

/// Extra things a call may carry: who is asking, and proof that it is a
/// person's browser asking.
#[derive(Default, Clone)]
pub struct Who<'a> {
    /// The `auth-token` of a twitch.tv session, for a call that should be
    /// answered for that person (their Turbo, their subscriptions).
    pub web_token: Option<&'a str>,
    /// An integrity token Twitch demanded, and the device id it was minted for.
    pub integrity: Option<(&'a str, &'a str)>,
}

/// Sends one query or persisted query and answers with its JSON, or says what
/// Twitch said when it refused or reported errors.
pub async fn call(http: &reqwest::Client, root: &str, body: &Value, who: &Who<'_>) -> Result<Value> {
    let mut req = http
        .post(root)
        .header("Client-ID", SITE_CLIENT_ID)
        // The site sends its JSON as text/plain, and so does this.
        .header("Content-Type", "text/plain;charset=UTF-8")
        .header("X-Device-Id", who.integrity.map(|(device, _)| device).unwrap_or_else(|| device_id()))
        .body(body.to_string());
    if let Some(token) = who.web_token.filter(|t| !t.is_empty()) {
        req = req.header("Authorization", format!("OAuth {token}"));
    }
    if let Some((_, token)) = who.integrity {
        req = req.header("Client-Integrity", token);
    }
    let res = req.send().await.context("reaching Twitch")?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let said = value.get("message").and_then(|m| m.as_str()).unwrap_or("");
        bail!("Twitch answered {status}{}", if said.is_empty() { String::new() } else { format!(": {said}") });
    }
    Ok(value)
}

/// The first error message Twitch put in an answer, if it put any.
pub fn error_of(answer: &Value) -> Option<String> {
    answer.get("errors")?.as_array()?.first()?.get("message")?.as_str().map(str::to_string)
}

/// A plain GraphQL query with variables.
pub fn query(text: &str, variables: Value) -> Value {
    json!({ "query": text, "variables": variables })
}
