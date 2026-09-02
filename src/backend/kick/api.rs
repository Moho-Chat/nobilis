//! Kick's HTTP surface: everything that is a request rather than a stream.
//!
//! Kick has two APIs, and this uses the older one on purpose.
//!
//! The newer `api.kick.com/public/v1` is OAuth 2.1 with PKCE, which is the
//! right shape for a service integration and the wrong shape for a chat
//! client: it requires a client id registered in Kick's developer dashboard,
//! so every person running moho would have to go and create an application
//! before they could read a chat that is already public to anybody with a
//! browser. The endpoints under `kick.com/api/v2` are what kick.com's own
//! pages call, need no registration, and answer the four questions this
//! backend actually has - who is this streamer, what emotes do they have, am
//! I subscribed, and please post this line.
//!
//! Two of those four need no credential at all, which is worth stating as a
//! property rather than an accident: a channel's chat and its emote table are
//! public, so an account with no token still reads. Only sending and the
//! subscription check are authenticated, and both fail with the server's own
//! message rather than being papered over.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;

pub const API_ROOT: &str = "https://kick.com";

/// Presented on every request.
///
/// Kick sits behind Cloudflare, which answers a request with no plausible
/// browser identity with a challenge page rather than JSON. This is not an
/// attempt to hide what moho is - it identifies itself in the same string -
/// only to look like the HTTP client Cloudflare expects rather than the
/// default `reqwest/0.12`, which is filtered on sight.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 moho";

const TIMEOUT: Duration = Duration::from_secs(20);

pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build()
        .context("building the Kick HTTP client")
}

/// A streamer's channel, as much of it as this backend needs.
#[derive(Debug, Clone)]
pub struct Channel {
    pub id: u64,
    /// The room the websocket subscribes to. Not the same number as `id`
    /// in general, even though it often is for older channels.
    pub chatroom_id: u64,
    pub slug: String,
    pub username: String,
    pub avatar_url: Option<String>,
    /// Chat is restricted to subscribers right now.
    pub subscribers_only: bool,
    /// Chat is restricted to followers right now.
    pub followers_only: bool,
}

#[derive(Deserialize)]
struct ChannelJson {
    id: u64,
    slug: String,
    chatroom: ChatroomJson,
    user: Option<UserJson>,
}

#[derive(Deserialize)]
struct ChatroomJson {
    id: u64,
    #[serde(default)]
    subscribers_mode: bool,
    #[serde(default)]
    followers_mode: bool,
}

#[derive(Deserialize)]
struct UserJson {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    profile_pic: Option<String>,
}

/// Looks a streamer up by the handle somebody typed.
///
/// The handle is what appears after kick.com/, and people paste it in every
/// form they have seen it: with the URL still attached, with an @ in front
/// because that is how handles work everywhere else, in the wrong case. All of
/// those name the same channel, so all of them are accepted; see `normalise_slug`.
pub async fn channel(http: &reqwest::Client, slug: &str) -> Result<Channel> {
    let slug = normalise_slug(slug);
    if slug.is_empty() {
        bail!("that is not a Kick handle");
    }
    let url = format!("{API_ROOT}/api/v2/channels/{slug}");
    let res = http.get(&url).header("Accept", "application/json").send().await.context("asking Kick about that channel")?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("there is no Kick channel called \"{slug}\"");
    }
    if !res.status().is_success() {
        bail!("Kick answered {} for {slug}", res.status());
    }
    let json: ChannelJson = res.json().await.context("reading Kick's answer about that channel")?;
    Ok(Channel {
        id: json.id,
        chatroom_id: json.chatroom.id,
        username: json.user.as_ref().and_then(|u| u.username.clone()).unwrap_or_else(|| json.slug.clone()),
        avatar_url: json.user.and_then(|u| u.profile_pic),
        subscribers_only: json.chatroom.subscribers_mode,
        followers_only: json.chatroom.followers_mode,
        slug,
    })
}

/// The handle out of whatever was typed, or empty if there was none in there.
///
/// Deliberately permissive about the wrapping and strict about the result: a
/// Kick handle is letters, digits, underscores and hyphens, so anything else
/// surviving this is not one and is better refused here than sent.
pub fn normalise_slug(raw: &str) -> String {
    let mut s = raw.trim();
    for prefix in ["https://", "http://"] {
        s = s.strip_prefix(prefix).unwrap_or(s);
    }
    s = s.strip_prefix("www.").unwrap_or(s);
    s = s.strip_prefix("kick.com/").unwrap_or(s);
    // A channel URL can carry a tab or a query - kick.com/name/videos.
    s = s.split(['/', '?', '#']).next().unwrap_or("");
    s = s.strip_prefix('@').unwrap_or(s);
    let s = s.trim().to_lowercase();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return String::new();
    }
    s
}

/// One emote somebody can put in a message.
#[derive(Debug, Clone, Serialize)]
pub struct Emote {
    pub id: String,
    pub name: String,
    /// Only subscribers to this channel may use it. Everybody can still *see*
    /// it - see the module doc on `super::emotes`.
    #[serde(rename = "subscribersOnly")]
    pub subscribers_only: bool,
    /// Which set it came from: the channel's own, or one of Kick's global
    /// ones. Shown as a heading in the picker, so an unfamiliar name is
    /// attributable to whoever supplied it.
    pub set: String,
    pub url: String,
}

use serde::Serialize;

#[derive(Deserialize)]
struct EmoteSetJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    emotes: Vec<EmoteJson>,
}

#[derive(Deserialize)]
struct EmoteJson {
    id: serde_json::Value,
    name: String,
    #[serde(default)]
    subscribers_only: bool,
}

/// The picture for an emote, by id.
///
/// Only "fullsize" exists: the obvious `/default` and `/small` variants that
/// other emote hosts serve are a 403 here, which would be a broken image in
/// every message rather than a smaller one.
pub fn emote_url(id: &str) -> String {
    format!("https://files.kick.com/emotes/{id}/fullsize")
}

/// Every emote usable in a channel: the streamer's own, plus Kick's global
/// sets.
///
/// One request for all three, because that is how Kick serves it and because
/// they are wanted together - the picker shows them in one list under
/// headings, and a message can carry any of them.
pub async fn emotes(http: &reqwest::Client, slug: &str) -> Result<Vec<Emote>> {
    let url = format!("{API_ROOT}/emotes/{slug}");
    let res = http.get(&url).header("Accept", "application/json").send().await.context("fetching the channel's emotes")?;
    if !res.status().is_success() {
        bail!("Kick answered {} for {slug}'s emotes", res.status());
    }
    let sets: Vec<EmoteSetJson> = res.json().await.context("reading the channel's emotes")?;

    let mut out = Vec::new();
    for set in sets {
        // The channel's own set is the one identified by slug rather than by
        // name; Kick's global sets are named ("Global", "Emojis") and carry
        // no slug. Naming it for the streamer is what makes the picker's
        // heading useful.
        let label = match (&set.name, &set.slug) {
            (_, Some(slug)) => slug.clone(),
            (Some(name), None) => name.clone(),
            (None, None) => "Kick".to_string(),
        };
        for e in set.emotes {
            // Kick sends ids as numbers in some sets and strings in others.
            let id = match &e.id {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                _ => continue,
            };
            out.push(Emote {
                url: emote_url(&id),
                id,
                name: e.name,
                subscribers_only: e.subscribers_only,
                set: label.clone(),
            });
        }
    }
    Ok(out)
}

/// The credential itself, out of however it was captured.
///
/// A sign-in window reads Kick's session off a request header, where it is
/// written the way HTTP writes credentials - `Bearer <token>`. Everything here
/// adds that scheme itself, so a token still carrying one would be sent as
/// `Bearer Bearer ...` and rejected with a message about nothing in
/// particular. Stripped once, at the boundary, rather than guarded at each of
/// the four call sites.
pub fn bare_token(raw: &str) -> String {
    let t = raw.trim();
    let t = if t.len() > 7 && t[..7].eq_ignore_ascii_case("bearer ") { &t[7..] } else { t };
    t.trim().to_string()
}

/// Who the token belongs to.
#[derive(Deserialize, Debug, Clone)]
pub struct Identity {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub id: Option<u64>,
}

pub async fn identity(http: &reqwest::Client, token: &str) -> Result<Identity> {
    let res = http
        .get(format!("{API_ROOT}/api/v1/user"))
        .header("Accept", "application/json")
        .bearer_auth(token)
        .send()
        .await
        .context("asking Kick who this account is")?;
    if !res.status().is_success() {
        bail!("Kick did not accept that sign-in ({})", res.status());
    }
    let who: Identity = res.json().await.context("reading Kick's answer")?;
    // An unauthenticated request to this endpoint is answered `{}` with a 200
    // rather than a 401, so "did it parse" is not the same question as "is
    // this signed in" and the empty answer has to be caught here.
    if who.username.as_deref().unwrap_or("").is_empty() {
        bail!("Kick did not accept that sign-in");
    }
    Ok(who)
}

/// The channels this account follows, as handles, the live ones first.
///
/// Two endpoints, because Kick's own site uses two and they answer different
/// questions - taken from its own frontend rather than guessed:
///
///   - `/api/v2/channels/followed` is what the sidebar shows: the follows that
///     are **live right now**, and nothing else. Asking only this is why a
///     first connect brought in four channels for an account following
///     twenty-nine, and it looked correct because four channels did appear.
///   - `/api/v2/channels/followed-page` is the Following page: all of them.
///
/// Both are cursor-paginated. Live first and then the rest, deduplicated, so
/// the channels worth opening first are the ones with something happening in
/// them - which is also the order they appear in.
///
/// A third path, `/api/v1/channels/followed`, is a trap: it answers 200 to an
/// anonymous request because there is a real streamer whose handle is literally
/// "followed", so it returns one stranger's channel object instead of anybody's
/// follows.
pub async fn followed(http: &reqwest::Client, token: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    // The live list first, and its failure is survivable: the full list below
    // contains these too, so losing it costs the ordering rather than the
    // channels.
    let live = collect_follows(http, token, "/api/v2/channels/followed", &mut out).await;
    let all = collect_follows(http, token, "/api/v2/channels/followed-page", &mut out).await;
    // Only a complete failure is an error. Having one of the two is a usable
    // answer, and reporting it as a failure would throw away what was fetched.
    match (live, all) {
        (Err(e), Err(_)) => Err(e),
        _ => Ok(out),
    }
}

/// How many pages to walk before stopping.
///
/// A cursor that never changes, or a list longer than anybody's attention,
/// should end the walk rather than spin - and fifty channels is the cap
/// upstream anyway.
const MAX_FOLLOW_PAGES: usize = 10;

/// Walks one paginated follow list, appending handles not already collected.
async fn collect_follows(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_FOLLOW_PAGES {
        let url = match &cursor {
            Some(c) => format!("{API_ROOT}{path}?cursor={}", urlencode(c)),
            None => format!("{API_ROOT}{path}"),
        };
        let res = http
            .get(&url)
            .header("Accept", "application/json")
            .bearer_auth(token)
            .send()
            .await
            .context("asking Kick which channels this account follows")?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("Kick no longer accepts this sign-in - sign in again from Accounts");
        }
        if !res.status().is_success() {
            bail!("Kick answered {} for this account's follows", res.status());
        }
        let body: serde_json::Value = res.json().await.context("reading this account's follows")?;
        for slug in slugs_in(&body) {
            if !out.contains(&slug) {
                out.push(slug);
            }
        }
        match next_cursor(&body) {
            // A cursor that has not moved would walk the same page forever.
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => return Ok(()),
        }
    }
    Ok(())
}

/// Where the next page starts, if there is one.
fn next_cursor(body: &serde_json::Value) -> Option<String> {
    let value = body.get("nextCursor").or_else(|| body.get("next_cursor"))?;
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Enough escaping for a cursor in a query string.
///
/// Kick's cursors have been plain so far, but this one is handed straight back
/// from a response into a URL, and "it has always been alphanumeric" is not a
/// property of somebody else's API.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Every handle in a follow list, whatever shape the list arrived in.
fn slugs_in(body: &serde_json::Value) -> Vec<String> {
    let items = body
        .as_array()
        .or_else(|| body.get("data").and_then(|v| v.as_array()))
        .or_else(|| body.get("channels").and_then(|v| v.as_array()))
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for item in items {
        let raw = item
            .get("slug")
            .or_else(|| item.get("channel").and_then(|c| c.get("slug")))
            .or_else(|| item.get("channel_slug"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Through the same gate a typed handle goes through, so nothing from
        // the network reaches a URL without being a handle first.
        let slug = normalise_slug(raw);
        if !slug.is_empty() && !out.contains(&slug) {
            out.push(slug);
        }
    }
    out
}

/// One message out of a channel's history.
#[derive(Deserialize, Debug)]
pub struct HistoryMessage {
    pub id: String,
    #[serde(default)]
    pub content: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub created_at: Option<String>,
    pub sender: HistorySender,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
pub struct HistorySender {
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub identity: Option<ChatIdentity>,
}

/// How somebody appears in chat: their colour, and what they have earned.
///
/// Both arrive on every message and were both being dropped at the struct
/// boundary. Kick chat is substantially about who is talking - a moderator, a
/// subscriber of two years, the streamer - and without this every line looks
/// the same.
///
/// Named for what it describes rather than `Identity`, which in this module
/// already means "who the signed-in account is".
#[derive(Deserialize, Debug, Clone, Default)]
pub struct ChatIdentity {
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub badges: Vec<Badge>,
}

#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct Badge {
    /// `moderator`, `subscriber`, `verified`, `og`, `founder`, `vip`...
    #[serde(rename = "type")]
    pub kind: String,
    /// What Kick calls it, which is what a tooltip should say.
    #[serde(default)]
    pub text: String,
    /// Months subscribed, for the badges that count.
    #[serde(default)]
    pub count: Option<u32>,
}

#[derive(Deserialize)]
struct HistoryEnvelope {
    #[serde(default)]
    data: Option<HistoryPage>,
}

#[derive(Deserialize)]
struct HistoryPage {
    #[serde(default)]
    messages: Vec<HistoryMessage>,
    #[serde(default)]
    cursor: Option<serde_json::Value>,
}

/// A page of a channel's past, newest first.
///
/// Keyed by the numeric channel id rather than the handle: the same path with
/// a slug answers 500, which is the kind of difference that only shows up by
/// trying it.
///
/// `cursor` continues an earlier page; the returned one continues this page,
/// and is absent at the end of what the server will give.
pub async fn history(
    http: &reqwest::Client,
    channel_id: u64,
    cursor: Option<&str>,
) -> Result<(Vec<HistoryMessage>, Option<String>)> {
    let mut url = format!("{API_ROOT}/api/v2/channels/{channel_id}/messages");
    if let Some(cursor) = cursor {
        url.push_str(&format!("?cursor={}", urlencode(cursor)));
    }
    let res = http.get(&url).header("Accept", "application/json").send().await.context("fetching the channel's history")?;
    if !res.status().is_success() {
        bail!("Kick answered {} for that channel's history", res.status());
    }
    let envelope: HistoryEnvelope = res.json().await.context("reading the channel's history")?;
    let page = envelope.data.unwrap_or(HistoryPage { messages: Vec::new(), cursor: None });
    // Kick has sent this as a number and as a string; both mean the same
    // thing to the next request.
    let cursor = match page.cursor {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    Ok((page.messages, cursor))
}

/// What this account is to one channel.
#[derive(Debug, Clone, Copy, Default)]
pub struct Standing {
    pub subscribed: bool,
    pub following: bool,
}

#[derive(Deserialize)]
struct MeJson {
    #[serde(default)]
    subscription: Option<serde_json::Value>,
    #[serde(default)]
    is_following: bool,
}

/// Whether this account is subscribed to a channel.
///
/// The answer decides only which emotes the picker will let you *send*. It is
/// asked once when a channel is opened rather than per message: a subscription
/// starting mid-session is worth a reconnect to notice, and asking Kick on
/// every keystroke to gate a picker would be absurd.
///
/// An error here is not fatal anywhere it is called. Not knowing means being
/// treated as not subscribed, which costs a locked emote rather than a
/// conversation.
pub async fn standing(http: &reqwest::Client, token: &str, slug: &str) -> Result<Standing> {
    let res = http
        .get(format!("{API_ROOT}/api/v2/channels/{slug}/me"))
        .header("Accept", "application/json")
        .bearer_auth(token)
        .send()
        .await
        .context("asking Kick about this account's standing in the channel")?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("Kick no longer accepts this sign-in - sign in again from Accounts");
    }
    if !res.status().is_success() {
        bail!("Kick answered {} about this account's standing", res.status());
    }
    let me: MeJson = res.json().await.context("reading Kick's answer")?;
    Ok(Standing {
        // Present-and-not-null rather than truthy: Kick sends the whole
        // subscription object, and its fields are none of this client's
        // business beyond whether there is one.
        subscribed: !matches!(me.subscription, None | Some(serde_json::Value::Null)),
        following: me.is_following,
    })
}

/// What a message is answering, as Kick's send endpoint wants it.
///
/// The original's text travels with the reply rather than only its id, which
/// is Kick's design and not this client's choice: it is what makes a reply
/// quotable in everybody's chat window without each of them looking the
/// original up, including people who joined after it was said.
#[derive(Debug, Clone)]
pub struct ReplyTo {
    pub message_id: String,
    pub body: String,
    pub sender_id: Option<u64>,
    pub sender_name: String,
}

/// Posts a line to a channel's chat.
///
/// The CSRF token is asked for at send time rather than stored. Kick's API is
/// Laravel, which wants its `XSRF-TOKEN` cookie echoed back in a header on a
/// POST; fetching it immediately before means it cannot be stale, and means
/// the sign-in window has only one thing to capture rather than two that could
/// expire independently.
pub async fn send_message(
    http: &reqwest::Client,
    token: &str,
    chatroom_id: u64,
    body: &str,
    reply_to: Option<&ReplyTo>,
) -> Result<()> {
    // A reply is its own message type carrying the original, not a mention
    // pasted on the front. Sent as Kick's own client sends it, so it threads
    // in everybody's chat window rather than only reading like a reply here.
    let payload = match reply_to {
        None => serde_json::json!({ "content": body, "type": "message" }),
        Some(r) => serde_json::json!({
            "content": body,
            "type": "reply",
            "metadata": {
                "original_message": { "id": r.message_id, "content": r.body },
                "original_sender": { "id": r.sender_id, "username": r.sender_name }
            }
        }),
    };
    let mut req = http
        .post(format!("{API_ROOT}/api/v2/messages/send/{chatroom_id}"))
        .header("Accept", "application/json")
        .bearer_auth(token)
        .json(&payload);
    if let Some(xsrf) = xsrf_token(http, token).await {
        req = req.header("X-XSRF-TOKEN", xsrf);
    }
    let res = req.send().await.context("sending the message to Kick")?;
    let status = res.status();
    if status.is_success() {
        return Ok(());
    }
    // Kick explains refusals properly - slow mode, followers-only, a timeout,
    // an expired session - and its sentence is worth far more than "HTTP 403".
    let detail = res
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty());
    match detail {
        Some(msg) => Err(anyhow!("{msg}")),
        None if status == reqwest::StatusCode::UNAUTHORIZED => {
            Err(anyhow!("Kick no longer accepts this sign-in - sign in again from Accounts"))
        }
        None => Err(anyhow!("Kick refused the message ({status})")),
    }
}

/// The CSRF token Kick last handed this client, if it has handed one over.
///
/// Best-effort by design: a missing token is not worth refusing to send over,
/// because the request without it fails with Kick's own explanation, which is
/// more useful than a guess made here about why.
async fn xsrf_token(http: &reqwest::Client, token: &str) -> Option<String> {
    // Any authenticated GET hands one over; this is the cheapest one. Read
    // straight off the response rather than through a cookie jar - the session
    // itself travels as a bearer header, so a jar would exist for this one
    // value and hold a credential for the rest of the process's life.
    let res = http.get(format!("{API_ROOT}/api/v1/user")).bearer_auth(token).send().await.ok()?;
    let cookie = res
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.strip_prefix("XSRF-TOKEN="))?
        .split(';')
        .next()?
        .to_string();
    // It arrives percent-encoded and must be sent decoded.
    Some(percent_decode(&cookie))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_handle_out_of_whatever_was_typed() {
        for input in [
            "xqc",
            "XQC",
            " xqc ",
            "@xqc",
            "kick.com/xqc",
            "https://kick.com/xqc",
            "http://www.kick.com/xqc",
            "https://kick.com/xqc/videos",
            "https://kick.com/xqc?foo=1",
        ] {
            assert_eq!(normalise_slug(input), "xqc", "for {input:?}");
        }
    }

    #[test]
    fn keeps_the_punctuation_a_handle_may_contain() {
        assert_eq!(normalise_slug("some_streamer-2"), "some_streamer-2");
    }

    #[test]
    fn refuses_what_is_not_a_handle() {
        // Nothing here should ever reach a URL, so each is empty rather than
        // escaped: a "handle" containing a slash or a space is somebody
        // pasting the wrong thing, and guessing at what they meant is how a
        // client ends up requesting a path it was never given.
        for input in ["", "  ", "@", "https://kick.com/", "not a handle", "x;y", "../admin", "a b"] {
            assert_eq!(normalise_slug(input), "", "for {input:?}");
        }
    }

    #[test]
    fn a_path_is_read_as_a_handle_and_a_tail() {
        // The same rule that turns kick.com/xqc/videos into "xqc", applied to
        // a bare path. Taking the first segment rather than refusing is the
        // useful reading of somebody's paste, and it is safe for the reason
        // that matters: whatever comes out is checked character by character
        // afterwards, so a segment that is not a handle still becomes nothing.
        assert_eq!(normalise_slug("xqc/videos"), "xqc");
        assert_eq!(normalise_slug("../admin"), "");
    }

    #[test]
    fn takes_the_scheme_off_a_captured_credential() {
        assert_eq!(bare_token("Bearer abc123"), "abc123");
        assert_eq!(bare_token("bearer abc123"), "abc123");
        assert_eq!(bare_token("  Bearer   abc123  "), "abc123");
        assert_eq!(bare_token("abc123"), "abc123");
        // A token that merely starts with those letters is not a scheme.
        assert_eq!(bare_token("bearerish"), "bearerish");
    }

    #[test]
    fn reads_a_follow_list_in_any_of_its_shapes() {
        let bare = serde_json::json!([{ "slug": "tayl31gh" }, { "slug": "odablock" }]);
        assert_eq!(slugs_in(&bare), vec!["tayl31gh", "odablock"]);

        let wrapped = serde_json::json!({ "data": [{ "slug": "tayl31gh" }] });
        assert_eq!(slugs_in(&wrapped), vec!["tayl31gh"]);

        let named = serde_json::json!({ "channels": [{ "slug": "tayl31gh" }] });
        assert_eq!(slugs_in(&named), vec!["tayl31gh"]);

        // The handle one level down, which is how a list of livestreams reads.
        let nested = serde_json::json!({ "data": [{ "channel": { "slug": "odablock" } }] });
        assert_eq!(slugs_in(&nested), vec!["odablock"]);
    }

    #[test]
    fn finds_where_the_next_page_starts() {
        assert_eq!(next_cursor(&serde_json::json!({ "nextCursor": "abc" })).as_deref(), Some("abc"));
        assert_eq!(next_cursor(&serde_json::json!({ "nextCursor": 20 })).as_deref(), Some("20"));
        assert_eq!(next_cursor(&serde_json::json!({ "next_cursor": "abc" })).as_deref(), Some("abc"));
        // The end of the list, in each of the ways it is said.
        assert_eq!(next_cursor(&serde_json::json!({ "nextCursor": null })), None);
        assert_eq!(next_cursor(&serde_json::json!({ "nextCursor": "" })), None);
        assert_eq!(next_cursor(&serde_json::json!({ "channels": [] })), None);
    }

    #[test]
    fn escapes_a_cursor_before_putting_it_in_a_url() {
        assert_eq!(urlencode("abc123"), "abc123");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("eyJ0eXAiOiJKV1Qi"), "eyJ0eXAiOiJKV1Qi");
    }

    #[test]
    fn a_follow_list_that_is_not_one_yields_nothing() {
        assert!(slugs_in(&serde_json::json!({ "message": "Unauthenticated." })).is_empty());
        assert!(slugs_in(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn one_unusable_entry_does_not_cost_the_others() {
        let mixed = serde_json::json!([
            { "slug": "good" },
            { "nothing": "here" },
            { "slug": "../admin" },
            { "slug": "also_good" },
            { "slug": "good" }
        ]);
        // Skipped, deduplicated, and every survivor is a real handle - the
        // same gate a typed one goes through, so nothing off the network
        // reaches a URL unchecked.
        assert_eq!(slugs_in(&mixed), vec!["good", "also_good"]);
    }

    #[test]
    fn decodes_a_percent_encoded_csrf_token() {
        assert_eq!(percent_decode("abc%3D%3D"), "abc==");
        assert_eq!(percent_decode("plain"), "plain");
        // A stray percent is left alone rather than eating the next two bytes.
        assert_eq!(percent_decode("100%"), "100%");
    }
}
