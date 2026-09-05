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
    /// What is on air, when anything is.
    pub live: Option<Live>,
    /// How many people follow the channel - the one number about a channel
    /// that means something while it is offline.
    pub followers: Option<u64>,
}

/// A stream in progress, as the channel endpoint describes it.
#[derive(Clone, Debug, Default)]
pub struct Live {
    pub title: String,
    pub category: Option<String>,
    pub viewers: Option<u64>,
    /// Kick's own "2026-09-03 19:57:50", in unix seconds where it parses.
    pub started_ts: Option<i64>,
}

#[derive(Deserialize)]
struct ChannelJson {
    id: u64,
    slug: String,
    chatroom: ChatroomJson,
    user: Option<UserJson>,
    #[serde(default)]
    livestream: Option<LivestreamJson>,
    /// A number, or the same number written as a string - Kick sends both,
    /// and a strict `u64` here failed the *whole* channel parse, so the
    /// channels that happened to be sent that way would not connect at all.
    #[serde(default, deserialize_with = "loose_number")]
    followers_count: Option<u64>,
}

/// A count that may arrive as a number or as a string containing one.
fn loose_number<'de, D>(d: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(d)? {
        Some(serde_json::Value::Number(n)) => Ok(n.as_u64()),
        Some(serde_json::Value::String(s)) => Ok(s.parse().ok()),
        _ => Ok(None),
    }
}

#[derive(Deserialize)]
struct LivestreamJson {
    #[serde(default)]
    session_title: Option<String>,
    #[serde(default)]
    is_live: bool,
    #[serde(default, deserialize_with = "loose_number")]
    viewer_count: Option<u64>,
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default)]
    categories: Vec<CategoryJson>,
}

#[derive(Deserialize)]
struct CategoryJson {
    #[serde(default)]
    name: Option<String>,
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
/// How many times to wait and ask again when Kick says to slow down.
const CHANNEL_RETRIES: usize = 3;

/// How long Kick asked us to wait, where it said.
fn retry_after(res: &reqwest::Response) -> Option<Duration> {
    let said = res.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = said.trim().parse().ok()?;
    // Capped: a client that obeys a ten-minute Retry-After during startup has
    // hung, as far as anybody watching it can tell.
    Some(Duration::from_secs(seconds.min(10)))
}

pub async fn channel(http: &reqwest::Client, slug: &str) -> Result<Channel> {
    let slug = normalise_slug(slug);
    if slug.is_empty() {
        bail!("that is not a Kick handle");
    }
    let url = format!("{API_ROOT}/api/v2/channels/{slug}");
    // Kick rate-limits this endpoint, and connecting asks about every channel
    // an account watches - so being told to wait is an ordinary part of
    // starting up rather than a failure. Answered by waiting: a channel
    // dropped here is a channel missing from the list until the next
    // restart, which is a far worse outcome than a slower connect.
    let mut res = http.get(&url).header("Accept", "application/json").send().await.context("asking Kick about that channel")?;
    for attempt in 1..=CHANNEL_RETRIES {
        if res.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            break;
        }
        // Seconds rather than milliseconds: Kick's limit is measured in
        // requests per window, and asking again immediately only spends the
        // next window's budget on the same refusal.
        let wait = retry_after(&res).unwrap_or(Duration::from_secs(2 * attempt as u64));
        tokio::time::sleep(wait).await;
        res = http.get(&url).header("Accept", "application/json").send().await.context("asking Kick about that channel")?;
    }
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("there is no Kick channel called \"{slug}\"");
    }
    if !res.status().is_success() {
        bail!("Kick answered {} for {slug}", res.status());
    }
    let json: ChannelJson = res.json().await.context("reading Kick's answer about that channel")?;
    // `livestream` is null when the channel is offline, which is the honest
    // answer and the one the header draws as "offline" rather than as a blank.
    let live = json.livestream.filter(|l| l.is_live).map(|l| Live {
        title: l.session_title.unwrap_or_default(),
        category: l.categories.into_iter().find_map(|c| c.name),
        viewers: l.viewer_count,
        started_ts: l.start_time.as_deref().and_then(parse_kick_datetime),
    });

    Ok(Channel {
        id: json.id,
        chatroom_id: json.chatroom.id,
        username: json.user.as_ref().and_then(|u| u.username.clone()).unwrap_or_else(|| json.slug.clone()),
        avatar_url: json.user.and_then(|u| u.profile_pic),
        subscribers_only: json.chatroom.subscribers_mode,
        followers_only: json.chatroom.followers_mode,
        live,
        followers: json.followers_count,
        slug,
    })
}

/// Kick's stream start time, which is not the format the rest of its API
/// uses: "2026-09-03 19:57:50", a space rather than a T and no zone at all.
/// Read as UTC, which is what it is.
pub fn parse_kick_datetime(text: &str) -> Option<i64> {
    let text = text.trim();
    // Kick writes a stream's start time as "2026-09-03 19:57:50" and a
    // prediction's as "2026-09-04T17:45:16Z". Appending a zone to the second
    // one produced "...ZZ", which parses as nothing - and a prediction whose
    // start cannot be read is one whose clock never runs down.
    let iso = if text.ends_with('Z') || text.contains('+') {
        text.to_string()
    } else {
        format!("{}Z", text.replacen(' ', "T", 1))
    };
    chrono::DateTime::parse_from_rfc3339(&iso).ok().map(|dt| dt.timestamp())
}

#[cfg(test)]
mod live_tests {
    use super::{parse_kick_datetime, ChannelJson};

    /// The one timestamp Kick writes differently from all its others - a
    /// space instead of a T, and no zone.
    #[test]
    fn reads_the_start_time_kick_actually_sends() {
        assert_eq!(parse_kick_datetime("2026-09-03 19:57:50"), Some(1788465470));
        // And the one it writes the ordinary way, which predictions use.
        assert_eq!(parse_kick_datetime("2026-09-04T17:45:16Z"), Some(1788543916));
        assert_eq!(parse_kick_datetime("not a time"), None);
    }

    /// Kick sends a follower count as a number for some channels and as a
    /// string for others. A strict u64 failed the *whole* channel parse, so
    /// the channels sent the second way did not connect at all - which is a
    /// far worse thing than a missing number.
    #[test]
    fn a_count_is_read_whichever_way_kick_writes_it() {
        let numeric: ChannelJson = serde_json::from_value(serde_json::json!({
            "id": 1, "slug": "a", "chatroom": { "id": 2 }, "followers_count": 4123
        }))
        .expect("numeric");
        assert_eq!(numeric.followers_count, Some(4123));

        let stringly: ChannelJson = serde_json::from_value(serde_json::json!({
            "id": 1, "slug": "a", "chatroom": { "id": 2 }, "followers_count": "4123"
        }))
        .expect("string");
        assert_eq!(stringly.followers_count, Some(4123));

        // Absent, null, or something else entirely is simply no answer - not
        // a reason to lose the channel.
        let missing: ChannelJson = serde_json::from_value(serde_json::json!({
            "id": 1, "slug": "a", "chatroom": { "id": 2 }, "followers_count": null
        }))
        .expect("null");
        assert_eq!(missing.followers_count, None);
    }
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
    walk_follows(http, token, path, |body| {
        for slug in slugs_in(body) {
            if !out.contains(&slug) {
                out.push(slug);
            }
        }
    })
    .await
}

/// Walks one paginated follow list, handing each page's body to the caller.
///
/// The paging - cursor, stall guard, page cap, the two ways Kick spells the
/// cursor - is the same whether the caller wants handles or live state, so it
/// lives here once rather than in each of them.
async fn walk_follows(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    mut each: impl FnMut(&serde_json::Value),
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
        each(&body);
        match next_cursor(&body) {
            // A cursor that has not moved would walk the same page forever.
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => return Ok(()),
        }
    }
    Ok(())
}

/// What a follow list says about one channel right now.
///
/// Deliberately not a `Channel`: this comes from a list endpoint that carries
/// live state and nothing else useful, and pretending otherwise would invite
/// code to read ids off it that are not there.
#[derive(Debug, Clone, PartialEq)]
pub struct FollowedLive {
    pub slug: String,
    pub live: bool,
    pub title: Option<String>,
    pub category: Option<String>,
    pub viewers: Option<u64>,
}

/// Live state and viewer counts for everything this account follows, in bulk.
///
/// This is what Kick's own Following panel does, and it is the difference
/// between one request a minute and one request per channel per minute: an
/// account following thirty channels is thirty channel fetches otherwise,
/// which is how a client earns a rate limit.
///
/// Both lists again, for the same reason `followed` reads both: the live list
/// carries the stream title and the full list carries the channels that are
/// not live, and a channel missing from the answer is not the same as a
/// channel that is off air.
pub async fn followed_live(http: &reqwest::Client, token: &str) -> Result<Vec<FollowedLive>> {
    let mut out: Vec<FollowedLive> = Vec::new();
    let mut collect = |body: &serde_json::Value| {
        for row in live_rows(body) {
            match out.iter_mut().find(|c| c.slug == row.slug) {
                // First writer wins on presence, but a later page still fills
                // in what the earlier one did not carry - the full list has no
                // session title, so an entry from it would otherwise erase the
                // title the live list already provided.
                Some(existing) => {
                    existing.live |= row.live;
                    if existing.title.is_none() {
                        existing.title = row.title;
                    }
                    if existing.category.is_none() {
                        existing.category = row.category;
                    }
                    if existing.viewers.is_none() {
                        existing.viewers = row.viewers;
                    }
                }
                None => out.push(row),
            }
        }
    };
    let live = walk_follows(http, token, "/api/v2/channels/followed", &mut collect).await;
    let all = walk_follows(http, token, "/api/v2/channels/followed-page", &mut collect).await;
    match (live, all) {
        (Err(e), Err(_)) => Err(e),
        _ => Ok(out),
    }
}

/// The live state rows in one page of a follow list.
fn live_rows(body: &serde_json::Value) -> Vec<FollowedLive> {
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
        let slug = normalise_slug(raw);
        if slug.is_empty() {
            continue;
        }
        let live = item.get("is_live").and_then(|v| v.as_bool()).unwrap_or(false);
        let text = |key: &str| {
            item.get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        out.push(FollowedLive {
            slug,
            live,
            title: text("session_title"),
            category: text("category_name"),
            // Zero on an off-air channel is Kick saying nothing rather than
            // saying nobody is watching, and a "0 viewers" chip on a stream
            // that is not running reads as a bug.
            viewers: item
                .get("viewer_count")
                .and_then(|v| v.as_u64())
                .filter(|_| live),
        });
    }
    out
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
/// What a channel knows about one of its viewers.
///
/// `/channels/{slug}/users/{username}` is what the site's own moderation
/// popup asks: the day they started following, their badges here, and whether
/// they are currently banned. Unauthenticated it answers the public half,
/// which is why this takes an optional token rather than requiring one.
pub async fn channel_user(http: &reqwest::Client, token: Option<&str>, slug: &str, username: &str) -> Result<serde_json::Value> {
    let encoded = url::form_urlencoded::byte_serialize(username.as_bytes()).collect::<String>();
    let mut request = http
        .get(format!("{API_ROOT}/api/v2/channels/{slug}/users/{encoded}"))
        .header("Accept", "application/json");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let res = request.send().await.context("asking Kick about a viewer")?;
    if !res.status().is_success() {
        bail!("Kick answered {} about that viewer", res.status());
    }
    res.json().await.context("reading Kick's answer")
}

/// Follows a channel, or stops.
///
/// The same endpoint both ways, POST to follow and DELETE to stop, which is
/// how kick.com's own button works. Following is the one of these actions
/// worth having in a chat client: it is what decides whether a channel is in
/// the list this account syncs, so doing it here keeps that list somewhere a
/// person can edit it.
pub async fn set_following(http: &reqwest::Client, token: &str, slug: &str, follow: bool) -> Result<()> {
    let slug = normalise_slug(slug);
    if slug.is_empty() {
        bail!("that is not a Kick handle");
    }
    let url = format!("{API_ROOT}/api/v2/channels/{slug}/follow");
    let request = if follow { http.post(&url) } else { http.delete(&url) };
    let res = request
        .header("Accept", "application/json")
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| if follow { format!("following {slug}") } else { format!("unfollowing {slug}") })?;

    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("Kick no longer accepts this sign-in - sign in again from Accounts");
    }
    // Kick rate-limits following hard, and for minutes rather than seconds -
    // found by following and unfollowing one channel twice in a row. Said in
    // words, because "429" tells somebody nothing about what to do, and what
    // to do is wait.
    if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!("Kick is rate-limiting follows right now - try again in a few minutes");
    }
    if !res.status().is_success() {
        bail!("Kick answered {} to that", res.status());
    }
    Ok(())
}

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

/// Times somebody out, or bans them outright.
///
/// `minutes` of None is a ban with no end. Kick treats the two as one endpoint
/// with a flag, and the difference is the whole of what the moderator meant.
pub async fn ban(
    http: &reqwest::Client,
    token: &str,
    slug: &str,
    username: &str,
    minutes: Option<u32>,
) -> Result<()> {
    let body = match minutes {
        Some(duration) => serde_json::json!({ "banned_username": username, "duration": duration, "permanent": false }),
        None => serde_json::json!({ "banned_username": username, "permanent": true }),
    };
    moderate(http, token, reqwest::Method::POST, &format!("/api/v2/channels/{slug}/bans"), Some(body)).await
}

pub async fn unban(http: &reqwest::Client, token: &str, slug: &str, username: &str) -> Result<()> {
    moderate(http, token, reqwest::Method::DELETE, &format!("/api/v2/channels/{slug}/bans/{username}"), None).await
}

/// Takes one message down.
///
/// Keyed by the chatroom rather than the channel, which is the same split the
/// websocket uses: messages belong to the room, bans belong to the channel.
pub async fn delete_message(http: &reqwest::Client, token: &str, chatroom_id: u64, message_id: &str) -> Result<()> {
    moderate(
        http,
        token,
        reqwest::Method::DELETE,
        &format!("/api/v2/chatrooms/{chatroom_id}/messages/{message_id}"),
        None,
    )
    .await
}

/// How the chat is restricted: followers only, subscribers only, slow mode.
///
/// Sent whole rather than as a patch, because that is what the endpoint takes
/// - so a caller changing one setting must pass the others as they are, and
/// the RPC reads the current values before sending.
pub async fn set_chat_mode(
    http: &reqwest::Client,
    token: &str,
    slug: &str,
    followers_only: bool,
    subscribers_only: bool,
    slow_seconds: Option<u32>,
) -> Result<()> {
    let body = serde_json::json!({
        "followers_mode": followers_only,
        "subscribers_mode": subscribers_only,
        "slow_mode": slow_seconds.is_some(),
        "message_interval": slow_seconds.unwrap_or(0),
    });
    moderate(http, token, reqwest::Method::PUT, &format!("/api/v2/channels/{slug}/chatroom"), Some(body)).await
}

/// One moderation request, with the CSRF token and Kick's own refusal.
///
/// Shared because every one of these fails the same interesting ways - not a
/// moderator, not signed in any more, the person is not there - and Kick
/// explains each of them better than a status code can.
async fn moderate(
    http: &reqwest::Client,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<()> {
    let mut req = http
        .request(method, format!("{API_ROOT}{path}"))
        .header("Accept", "application/json")
        .bearer_auth(token);
    if let Some(xsrf) = xsrf_token(http, token).await {
        req = req.header("X-XSRF-TOKEN", xsrf);
    }
    if let Some(body) = body {
        req = req.json(&body);
    }
    let res = req.send().await.context("asking Kick to do that")?;
    let status = res.status();
    if status.is_success() {
        return Ok(());
    }
    let detail = res
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty());
    match detail {
        Some(message) => Err(anyhow!("{message}")),
        None if status == reqwest::StatusCode::FORBIDDEN => {
            Err(anyhow!("Kick refused - this account is not a moderator of that channel"))
        }
        None => Err(anyhow!("Kick refused ({status})")),
    }
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
    // Whatever went wrong, a refusal is a reason to stop trusting the CSRF
    // token: it costs one request to replace and a stale one fails every send
    // until it is.
    forget_xsrf();
    match detail {
        Some(msg) => Err(anyhow!("{msg}")),
        // Said apart from the sign-in being dead, because the two look
        // identical from here and only one of them is worth acting on. Being
        // told to slow down is not being logged out, and telling somebody to
        // sign in again - which is what this used to say - sends them to
        // re-authorise an account that was never deauthorised.
        None if status == reqwest::StatusCode::TOO_MANY_REQUESTS => {
            Err(anyhow!("Kick is rate-limiting this account - wait a moment and send it again"))
        }
        None if status == reqwest::StatusCode::UNAUTHORIZED => {
            Err(anyhow!("Kick no longer accepts this sign-in - sign in again from Accounts"))
        }
        None => Err(anyhow!("Kick refused the message ({status})")),
    }
}

/// How long a CSRF token is reused before being asked for again.
///
/// It used to be fetched for every single message, which doubled the requests
/// a conversation costs - and did it on the endpoint least worth spending
/// them on. Kick rate-limits this account readily enough that the extra call
/// was a real cause of messages failing, and a failed fetch means the send
/// goes without the header and is refused.
const XSRF_TTL: Duration = Duration::from_secs(600);

/// The last CSRF token seen for a session, and when.
static XSRF_CACHE: std::sync::Mutex<Option<(String, String, std::time::Instant)>> = std::sync::Mutex::new(None);

/// Throws the cached token away, so the next send goes and gets a fresh one.
///
/// Called when a send is refused: a stale CSRF token is one of the things
/// Kick refuses for, and it is the one thing here worth retrying differently.
fn forget_xsrf() {
    *XSRF_CACHE.lock().unwrap() = None;
}

/// The CSRF token Kick last handed this client, if it has handed one over.
///
/// Best-effort by design: a missing token is not worth refusing to send over,
/// because the request without it fails with Kick's own explanation, which is
/// more useful than a guess made here about why.
async fn xsrf_token(http: &reqwest::Client, token: &str) -> Option<String> {
    if let Some((session, value, when)) = XSRF_CACHE.lock().unwrap().clone() {
        if session == token && when.elapsed() < XSRF_TTL {
            return Some(value);
        }
    }
    let fresh = fetch_xsrf_token(http, token).await?;
    *XSRF_CACHE.lock().unwrap() = Some((token.to_string(), fresh.clone(), std::time::Instant::now()));
    Some(fresh)
}

async fn fetch_xsrf_token(http: &reqwest::Client, token: &str) -> Option<String> {
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

    /// The two follow lists have different shapes - the live one carries the
    /// session title, the full one does not - and this is the parsing that
    /// keeps a live channel from being reported as off air by the second one.
    #[test]
    fn reads_live_state_out_of_a_follow_page() {
        let body = serde_json::json!({
            "channels": [
                {
                    "channel_slug": "shoovy",
                    "is_live": true,
                    "viewer_count": 292,
                    "session_title": " sleeping ",
                    "category_name": "Just Chatting"
                },
                { "channel_slug": "winnerdog", "is_live": false, "viewer_count": 0 }
            ],
            "nextCursor": 20
        });
        let rows = live_rows(&body);
        assert_eq!(
            rows,
            vec![
                FollowedLive {
                    slug: "shoovy".into(),
                    live: true,
                    title: Some("sleeping".into()),
                    category: Some("Just Chatting".into()),
                    viewers: Some(292),
                },
                FollowedLive {
                    slug: "winnerdog".into(),
                    live: false,
                    title: None,
                    category: None,
                    // Not Some(0): off air, Kick sends zero for "no answer".
                    viewers: None,
                },
            ]
        );
        assert_eq!(next_cursor(&body).as_deref(), Some("20"));
    }

    /// Kick answers 200 and puts the refusal inside the body, so this is the
    /// difference between "your vote counted" and "you already voted".
    #[test]
    fn a_refusal_inside_a_success_is_still_a_refusal() {
        let refused: PollEnvelope = serde_json::from_str(
            r#"{"status":{"code":400,"message":"User has already voted","error":true},"data":null}"#,
        )
        .expect("parse");
        assert_eq!(refused.complaint(), Some((400, "User has already voted".to_string())));

        let empty: PollEnvelope =
            serde_json::from_str(r#"{"status":{"code":404,"message":"Poll not found","error":true},"data":null}"#)
                .expect("parse");
        assert_eq!(empty.complaint().map(|(code, _)| code), Some(404));

        let fine: PollEnvelope = serde_json::from_str(
            r#"{"status":{"code":200,"message":"Poll retrieved successfully","error":false},
                "data":{"poll":{"title":"next game?","options":[{"id":0,"label":"chess","votes":2}],
                "duration":60,"remaining":41,"result_display_duration":30,"has_voted":true,"voted_option_id":0}}}"#,
        )
        .expect("parse");
        assert_eq!(fine.complaint(), None);
        let poll = fine.data.and_then(|d| d.poll).expect("a poll");
        assert_eq!(poll.remaining, 41);
        assert_eq!(poll.voted_option_id, Some(0));
        assert_eq!(poll.options[0].votes, 2);
    }

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

/// A poll, as Kick describes one.
///
/// There is no id anywhere in it: a channel has at most one poll running, and
/// Kick identifies it by the channel it is in. `remaining` is the seconds left
/// when the answer was written, so a client counts down from it rather than
/// asking again every second.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Poll {
    pub title: String,
    #[serde(default)]
    pub options: Vec<PollOption>,
    #[serde(default)]
    pub duration: u32,
    #[serde(default)]
    pub remaining: u32,
    /// How long the result stays up after the voting stops.
    #[serde(default)]
    pub result_display_duration: u32,
    #[serde(default)]
    pub has_voted: bool,
    #[serde(default)]
    pub voted_option_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PollOption {
    pub id: i64,
    pub label: String,
    #[serde(default)]
    pub votes: u64,
}

/// Kick's answer about a poll, envelope and all.
///
/// The envelope is the whole point: this API answers 200 and puts the failure
/// *inside* the body - "User has already voted" arrives as an HTTP success
/// with `status.error` set - so a client reading only the status code reports
/// a refused vote as a vote that went through.
#[derive(Deserialize)]
struct PollEnvelope {
    #[serde(default)]
    status: Option<PollStatus>,
    #[serde(default)]
    data: Option<PollData>,
}

#[derive(Deserialize)]
struct PollStatus {
    #[serde(default)]
    code: u32,
    #[serde(default)]
    message: String,
    #[serde(default)]
    error: bool,
}

impl PollEnvelope {
    /// What went wrong inside a 200, in Kick's own words where it gave any.
    fn complaint(&self) -> Option<(u32, String)> {
        let status = self.status.as_ref().filter(|s| s.error)?;
        let said = status.message.trim();
        let words = if said.is_empty() { "Kick would not do that".to_string() } else { said.to_string() };
        Some((status.code, words))
    }
}

#[derive(Deserialize)]
struct PollData {
    #[serde(default)]
    poll: Option<Poll>,
}

/// The poll running in a channel now, if there is one.
///
/// `None` rather than an error for "no poll": Kick answers 200 with a 404
/// *inside* the envelope for a channel with nothing running, which is a
/// normal state and not a failure.
///
/// Signed in where possible, because two fields of the answer - whether this
/// account has voted and for what - only exist for somebody Kick can identify.
pub async fn poll(http: &reqwest::Client, token: Option<&str>, slug: &str) -> Result<Option<Poll>> {
    let mut req = http
        .get(format!("{API_ROOT}/api/v2/channels/{slug}/polls"))
        .header("Accept", "application/json");
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(token);
    }
    let res = req.send().await.context("asking Kick about this channel's poll")?;
    if !res.status().is_success() {
        bail!("Kick answered {} about this channel's poll", res.status());
    }
    let body: PollEnvelope = res.json().await.context("reading Kick's answer about the poll")?;
    match body.complaint() {
        // "Poll not found" is the ordinary answer for a channel with nothing
        // running, and not a failure to report.
        Some((404, _)) | None => Ok(body.data.and_then(|d| d.poll)),
        Some((_, said)) => bail!("{said}"),
    }
}

/// Votes for one option, and reads back the poll that vote landed in.
pub async fn vote_poll(http: &reqwest::Client, token: &str, slug: &str, option_id: i64) -> Result<Poll> {
    let res = http
        .post(format!("{API_ROOT}/api/v2/channels/{slug}/polls/vote"))
        .header("Accept", "application/json")
        .bearer_auth(token)
        .json(&serde_json::json!({ "id": option_id }))
        .send()
        .await
        .context("sending your vote to Kick")?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("sign in to Kick from Accounts to vote");
    }
    if !res.status().is_success() {
        bail!("Kick would not take that vote ({})", res.status());
    }
    let body: PollEnvelope = res.json().await.context("reading Kick's answer to the vote")?;
    if let Some((_, said)) = body.complaint() {
        bail!("{said}");
    }
    body.data
        .and_then(|d| d.poll)
        .ok_or_else(|| anyhow::anyhow!("Kick took the vote but said nothing about the poll"))
}

/// Takes the poll down. The streamer's own button, and Kick refuses it for
/// anybody else - which is the right place for that check to live.
pub async fn delete_poll(http: &reqwest::Client, token: &str, slug: &str) -> Result<()> {
    let res = http
        .delete(format!("{API_ROOT}/api/v2/channels/{slug}/polls"))
        .header("Accept", "application/json")
        .bearer_auth(token)
        .send()
        .await
        .context("asking Kick to end the poll")?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED || res.status() == reqwest::StatusCode::FORBIDDEN {
        bail!("only the streamer and their moderators can end a poll");
    }
    if !res.status().is_success() {
        bail!("Kick answered {} to ending the poll", res.status());
    }
    Ok(())
}

/// A prediction, as Kick's own viewer panel reads one.
///
/// Read from the site's own calls rather than guessed: the ids are ULID
/// strings rather than numbers, the stake is `total_vote_amount`, and the
/// payout is a rate rather than a phrase - "1:2.8" is how the page writes
/// `return_rate` 2.8, not something the API sends.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Prediction {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub outcomes: Vec<PredictionOutcome>,
    #[serde(default)]
    pub duration: u32,
    #[serde(default)]
    pub created_at: Option<String>,
    /// ACTIVE while it takes bets, LOCKED once it stops, then RESOLVED or
    /// CANCELLED.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub winning_outcome_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PredictionOutcome {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub total_vote_amount: f64,
    #[serde(default)]
    pub vote_count: u64,
    #[serde(default)]
    pub return_rate: f64,
}

/// What this account has riding on the prediction, where it has anything.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PredictionVote {
    pub outcome_id: String,
    #[serde(default)]
    pub total_vote_amount: f64,
}

#[derive(Deserialize)]
struct PredictionEnvelope {
    #[serde(default)]
    data: Option<PredictionData>,
}

#[derive(Deserialize)]
struct PredictionData {
    #[serde(default)]
    prediction: Option<Prediction>,
    #[serde(default)]
    user_vote: Option<PredictionVote>,
}

#[derive(Deserialize)]
struct PointsEnvelope {
    #[serde(default)]
    data: Option<PointsData>,
}

#[derive(Deserialize)]
struct PointsData {
    #[serde(default)]
    points: f64,
}

/// The prediction a channel has going, and this account's stake in it.
///
/// Signed in where possible for the same reason the poll is: the stake only
/// exists for somebody Kick can identify.
pub async fn prediction_latest(
    http: &reqwest::Client,
    token: Option<&str>,
    slug: &str,
) -> Result<Option<(Prediction, Option<PredictionVote>)>> {
    let body = prediction_call(http, token, &format!("/api/v2/channels/{slug}/predictions/latest"), None).await?;
    Ok(body.data.and_then(|d| d.prediction.map(|p| (p, d.user_vote))))
}

/// The predictions this channel has run before, newest first.
pub async fn predictions_recent(http: &reqwest::Client, token: Option<&str>, slug: &str) -> Result<Vec<Prediction>> {
    #[derive(Deserialize)]
    struct RecentEnvelope {
        #[serde(default)]
        data: Option<RecentData>,
    }
    #[derive(Deserialize)]
    struct RecentData {
        #[serde(default)]
        predictions: Vec<Prediction>,
    }
    let url = format!("{API_ROOT}/api/v2/channels/{slug}/predictions/recent");
    let mut req = http.get(&url).header("Accept", "application/json");
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(token);
    }
    let res = req.send().await.context("asking Kick about past predictions")?;
    if !res.status().is_success() {
        bail!("Kick answered {} about past predictions", res.status());
    }
    let body: RecentEnvelope = res.json().await.context("reading Kick's answer about past predictions")?;
    Ok(body.data.map(|d| d.predictions).unwrap_or_default())
}

/// Puts points on an outcome.
///
/// Ten is Kick's own floor and the client says so before sending, because
/// "amount must be at least 10" is a better answer than a refusal that
/// arrives a round trip later.
pub async fn vote_prediction(
    http: &reqwest::Client,
    token: &str,
    slug: &str,
    outcome_id: &str,
    amount: i64,
) -> Result<Option<(Prediction, Option<PredictionVote>)>> {
    if amount < MIN_PREDICTION_BET {
        bail!("Kick takes bets of {MIN_PREDICTION_BET} points or more");
    }
    let payload = serde_json::json!({ "amount": amount, "outcome_id": outcome_id });
    let body = prediction_call(
        http,
        Some(token),
        &format!("/api/v2/channels/{slug}/predictions/vote"),
        Some(payload),
    )
    .await?;
    Ok(body.data.and_then(|d| d.prediction.map(|p| (p, d.user_vote))))
}

/// The smallest bet Kick accepts, which its own form enforces.
pub const MIN_PREDICTION_BET: i64 = 10;

/// This account's channel points in a channel, which is what a bet spends.
pub async fn points(http: &reqwest::Client, token: &str, slug: &str) -> Result<i64> {
    let res = http
        .get(format!("{API_ROOT}/api/v2/channels/{slug}/points"))
        .header("Accept", "application/json")
        .bearer_auth(token)
        .send()
        .await
        .context("asking Kick about this account's points")?;
    if !res.status().is_success() {
        bail!("Kick answered {} about this account's points", res.status());
    }
    let body: PointsEnvelope = res.json().await.context("reading Kick's answer about points")?;
    Ok(body.data.map(|d| d.points as i64).unwrap_or_default())
}

/// One request to the prediction endpoints, GET or POST, with Kick's own
/// wording carried out of a refusal.
async fn prediction_call(
    http: &reqwest::Client,
    token: Option<&str>,
    path: &str,
    payload: Option<serde_json::Value>,
) -> Result<PredictionEnvelope> {
    let url = format!("{API_ROOT}{path}");
    let mut req = match &payload {
        Some(body) => http.post(&url).json(body),
        None => http.get(&url),
    };
    req = req.header("Accept", "application/json");
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(token);
    }
    let res = req.send().await.context("asking Kick about the prediction")?;
    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!("sign in to Kick from Accounts to bet on predictions");
    }
    // A channel that has never run one answers 404, which is an absence
    // rather than a failure.
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(PredictionEnvelope { data: None });
    }
    let text = res.text().await.context("reading Kick's answer about the prediction")?;
    if !status.is_success() {
        // Kick words these well - "not enough points", "prediction is locked"
        // - and its own words beat a status code every time.
        let said = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["message"].as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty());
        match said {
            Some(said) => bail!("{said}"),
            None => bail!("Kick answered {status} about the prediction"),
        }
    }
    serde_json::from_str(&text).context("reading Kick's answer about the prediction")
}
