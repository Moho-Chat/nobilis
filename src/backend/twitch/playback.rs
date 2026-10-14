//! The address of a live channel's video.
//!
//! twitch.tv's own player asks Twitch for a signed token (a persisted GraphQL
//! query, see `gql.rs`) and then for the channel's playlist from the playlist
//! service, passing the token along. Both are reproduced here, and the second is
//! only *built* - the player in the window fetches it, as it fetches every
//! playlist and segment after it. What this returns is that address, when it
//! stops being good (the token lasts twenty minutes), and what the token says
//! about the account: whether ads are for it, which renditions it may not have.
//!
//! Anonymous works: no account, no sign-in, no browser. An account's own
//! `auth-token` (the cookie of a twitch.tv session, see Settings) is sent when
//! there is one, and is what makes a person's Turbo or subscription count.
//!
//! Twitch can demand an *integrity token*, which is proof that a real browser
//! is asking, and sometimes has, for a week at a time. It is not demanded
//! today. When it is, the daemon asks the window to make one (in a hidden page
//! on twitch.tv, which is the only place it can be made), keeps it until it is
//! nearly out, and asks again.

use super::gql::{self, Who};
use crate::state::AppState;
use anyhow::anyhow;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// Where the playlist service is.
pub const USHER_ROOT: &str = "https://usher.ttvnw.net";

/// The player's own query for the token, by the hash it is stored under at Twitch.
const TOKEN_HASH: &str = "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9";

/// The address of a stream and what is known about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Playback {
    /// The playlist, signed. Replaced before `expires`.
    pub url: String,
    /// When the signature stops being accepted, in Unix seconds.
    pub expires: i64,
    /// Whether Twitch says ads are not for this account.
    pub hide_ads: bool,
    /// Renditions this account may not have, by Twitch's names for them.
    pub restricted: Vec<String>,
}

/// Why there is no address.
#[derive(Debug)]
pub enum PlaybackError {
    /// The channel is not broadcasting.
    Offline,
    /// Twitch will not give this account the stream: subscribers only, and so on.
    Forbidden(String),
    /// Not available where this is being watched from.
    Blocked(String),
    /// Twitch wanted proof that a browser is asking, and none could be had.
    NeedsIntegrity,
    Failed(anyhow::Error),
}

impl std::fmt::Display for PlaybackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaybackError::Offline => write!(f, "this channel is not streaming"),
            PlaybackError::Forbidden(why) if why.is_empty() => write!(f, "Twitch will not play this stream for this account (it may be subscribers only)"),
            PlaybackError::Forbidden(why) => write!(f, "Twitch will not play this stream for this account: {why}"),
            PlaybackError::Blocked(why) if why.is_empty() => write!(f, "this stream is not available where you are"),
            PlaybackError::Blocked(why) => write!(f, "this stream is not available where you are ({why})"),
            PlaybackError::NeedsIntegrity => write!(f, "Twitch is asking for proof that this is a browser, and moho could not give it"),
            PlaybackError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

fn token_query(login: &str) -> Value {
    json!({
        "operationName": "PlaybackAccessToken",
        "variables": { "isLive": true, "login": login, "isVod": false, "vodID": "", "playerType": "site", "platform": "web" },
        "extensions": { "persistedQuery": { "version": 1, "sha256Hash": TOKEN_HASH } },
    })
}

/// The playlist's address, from the signed token. Codecs are limited to H.264:
/// the window's player has no use for the AV1 and HEVC renditions Twitch offers
/// big channels, and a playlist that lists only what plays is one nothing has to
/// filter.
pub fn usher_url(root: &str, login: &str, signature: &str, token: &str) -> String {
    let pass = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() % 999_999).unwrap_or(1);
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in [
        ("allow_source", "true"),
        ("allow_audio_only", "true"),
        ("fast_bread", "true"),
        ("p", &pass.to_string()),
        ("platform", "web"),
        ("player_backend", "mediaplayer"),
        ("playlist_include_framerate", "true"),
        ("reassignments_supported", "true"),
        ("sig", signature),
        ("supported_codecs", "h264"),
        ("token", token),
        ("transcode_mode", "cbr_v1"),
    ] {
        query.append_pair(k, v);
    }
    format!("{root}/api/v2/channel/hls/{}.m3u8?{}", login.to_lowercase(), query.finish())
}

/// Reads Twitch's answer to the token query.
pub fn read_token(answer: &Value, login: &str, usher_root: &str) -> Result<Playback, PlaybackError> {
    let token = answer.pointer("/data/streamPlaybackAccessToken").filter(|t| !t.is_null()).ok_or(PlaybackError::Offline)?;
    let value = token.get("value").and_then(|v| v.as_str()).ok_or_else(|| PlaybackError::Failed(anyhow!("Twitch's token had no value")))?;
    let signature = token.get("signature").and_then(|v| v.as_str()).ok_or_else(|| PlaybackError::Failed(anyhow!("Twitch's token had no signature")))?;
    let about: Value = serde_json::from_str(value).map_err(|e| PlaybackError::Failed(anyhow!("Twitch's token was not readable: {e}")))?;
    if about.pointer("/authorization/forbidden").and_then(|f| f.as_bool()).unwrap_or(false) {
        return Err(PlaybackError::Forbidden(about.pointer("/authorization/reason").and_then(|r| r.as_str()).unwrap_or("").to_string()));
    }
    if let Some(why) = about.get("geoblock_reason").and_then(|g| g.as_str()).filter(|g| !g.is_empty()) {
        return Err(PlaybackError::Blocked(why.to_string()));
    }
    Ok(Playback {
        url: usher_url(usher_root, login, signature, value),
        expires: about.get("expires").and_then(|e| e.as_i64()).unwrap_or_else(|| chrono::Utc::now().timestamp() + 20 * 60),
        hide_ads: about.get("hide_ads").and_then(|h| h.as_bool()).unwrap_or(false),
        restricted: about
            .pointer("/chansub/restricted_bitrates")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|n| n.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// The integrity token
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Integrity {
    device_id: String,
    token: String,
    /// Unix seconds.
    expires_at: i64,
}

fn cache() -> &'static Mutex<Option<Integrity>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<Integrity>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

type Waiting = HashMap<String, tokio::sync::oneshot::Sender<Result<(String, i64), String>>>;

fn waiting() -> &'static Mutex<Waiting> {
    static WAITING: std::sync::OnceLock<Mutex<Waiting>> = std::sync::OnceLock::new();
    WAITING.get_or_init(Default::default)
}

/// How long before its end an integrity token is let go of.
const INTEGRITY_MARGIN: i64 = 600;

fn good_integrity() -> Option<Integrity> {
    let held = cache().lock().unwrap().clone()?;
    (held.expires_at - chrono::Utc::now().timestamp() > INTEGRITY_MARGIN).then_some(held)
}

/// The window's answer to a request for an integrity token: the token and when
/// it ends in Unix milliseconds, or why one could not be made.
pub fn provide_integrity(request_id: &str, outcome: Result<(String, i64), String>) -> bool {
    match waiting().lock().unwrap().remove(request_id) {
        Some(reply) => reply.send(outcome).is_ok(),
        None => false,
    }
}

/// Asks the window to make an integrity token and waits for it.
async fn obtain_integrity(state: &AppState) -> Option<Integrity> {
    if let Some(held) = good_integrity() {
        return Some(held);
    }
    let request_id = format!("integrity-{}", crate::model::next_message_id());
    let (reply, answer) = tokio::sync::oneshot::channel();
    waiting().lock().unwrap().insert(request_id.clone(), reply);
    let device_id = gql::device_id().to_string();
    state.events.emit("twitchIntegrityNeeded", json!({ "requestId": request_id, "deviceId": device_id }));
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), answer).await;
    waiting().lock().unwrap().remove(&request_id);
    match outcome {
        Ok(Ok(Ok((token, expires_ms)))) => {
            let made = Integrity { device_id, token, expires_at: expires_ms / 1000 };
            *cache().lock().unwrap() = Some(made.clone());
            Some(made)
        }
        Ok(Ok(Err(why))) => {
            tracing::debug!("twitch: no integrity token: {why}");
            None
        }
        _ => None,
    }
}

/// The address of a channel's video, or why there is none.
pub async fn playback(state: &AppState, http: &reqwest::Client, gql_root: &str, usher_root: &str, login: &str, web_token: Option<&str>) -> Result<Playback, PlaybackError> {
    let mut integrity = good_integrity();
    for attempt in 0..2 {
        let who = Who { web_token, integrity: integrity.as_ref().map(|i| (i.device_id.as_str(), i.token.as_str())) };
        let answer = gql::call(http, gql_root, &token_query(login), &who).await.map_err(PlaybackError::Failed)?;
        if let Some(said) = gql::error_of(&answer) {
            if said.to_lowercase().contains("integrity") {
                if attempt == 0 {
                    integrity = obtain_integrity(state).await;
                    if integrity.is_some() {
                        continue;
                    }
                }
                return Err(PlaybackError::NeedsIntegrity);
            }
            return Err(PlaybackError::Failed(anyhow!("Twitch would not give the stream a token: {said}")));
        }
        return read_token(&answer, login, usher_root);
    }
    Err(PlaybackError::NeedsIntegrity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// What the token's JSON says, as captured from a live answer, with the fields varied.
    fn token(extra: Value) -> Value {
        let mut about = json!({
            "adblock": false, "authorization": { "forbidden": false, "reason": "" }, "channel": "tylorturner", "channel_id": 106959846,
            "chansub": { "restricted_bitrates": [], "view_until": 1924905600 }, "device_id": "abc", "expires": 1791950724,
            "hide_ads": false, "https_required": true, "platform": "web", "player_type": "site", "server_ads": true, "show_ads": true
        });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            about[k] = v;
        }
        json!({ "data": { "streamPlaybackAccessToken": { "value": about.to_string(), "signature": "deadbeef" } } })
    }

    #[test]
    fn a_token_becomes_a_playlist_address_that_names_the_channel_and_the_signature() {
        let p = read_token(&token(json!({})), "TylorTurner", USHER_ROOT).unwrap();
        assert!(p.url.starts_with("https://usher.ttvnw.net/api/v2/channel/hls/tylorturner.m3u8?"), "{}", p.url);
        assert!(p.url.contains("sig=deadbeef") && p.url.contains("allow_source=true") && p.url.contains("supported_codecs=h264"), "{}", p.url);
        assert!(p.url.contains("token=%7B%22adblock%22"), "the token is the JSON, escaped: {}", p.url);
        assert_eq!((p.expires, p.hide_ads), (1791950724, false));
    }

    #[test]
    fn what_the_token_says_about_the_account_is_kept() {
        let p = read_token(&token(json!({ "hide_ads": true, "chansub": { "restricted_bitrates": ["chunked", "720p60"] } })), "x", USHER_ROOT).unwrap();
        assert!(p.hide_ads);
        assert_eq!(p.restricted, vec!["chunked", "720p60"]);
    }

    #[test]
    fn a_channel_that_is_not_streaming_has_no_token_and_a_stream_that_may_not_be_had_says_why() {
        assert!(matches!(read_token(&json!({ "data": { "streamPlaybackAccessToken": null } }), "x", USHER_ROOT), Err(PlaybackError::Offline)));
        match read_token(&token(json!({ "authorization": { "forbidden": true, "reason": "This stream is for subscribers only" } })), "x", USHER_ROOT) {
            Err(PlaybackError::Forbidden(why)) => assert!(why.contains("subscribers only")),
            other => panic!("{other:?}"),
        }
        match read_token(&token(json!({ "geoblock_reason": "blocked in your country" })), "x", USHER_ROOT) {
            Err(PlaybackError::Blocked(why)) => assert_eq!(why, "blocked in your country"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(read_token(&json!({ "data": { "streamPlaybackAccessToken": { "value": "not json", "signature": "s" } } }), "x", USHER_ROOT), Err(PlaybackError::Failed(_))));
        let said = PlaybackError::Forbidden(String::new()).to_string();
        assert!(said.contains("subscribers only"), "{said}");
    }

    /// A GraphQL that answers each call with the next canned answer, and remembers the headers it was sent.
    async fn graphql(answers: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
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
                    let mut buf = vec![0u8; 16384];
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
                    let (status, body) = answers.lock().unwrap().pop_front().unwrap_or((500, "{}".to_string()));
                    let reply = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        (root, seen)
    }

    #[tokio::test]
    async fn the_call_is_the_players_own_and_carries_the_accounts_token_only_when_there_is_one() {
        let state = testkit::simulated_daemon("twitch-playback");
        let (root, seen) = graphql(vec![(200, token(json!({})).to_string()), (200, token(json!({ "hide_ads": true })).to_string())]).await;
        let http = reqwest::Client::new();
        let anonymous = playback(&state, &http, &root, USHER_ROOT, "xqc", None).await.unwrap();
        assert!(!anonymous.hide_ads);
        let signed = playback(&state, &http, &root, USHER_ROOT, "xqc", Some("web-session")).await.unwrap();
        assert!(signed.hide_ads);
        let asked = seen.lock().unwrap().clone();
        let (first, second) = (asked[0].to_lowercase(), asked[1].to_lowercase());
        assert!(first.contains("client-id: kimne78kx3ncx6brgo4mv6wki5h1ko") && first.contains("x-device-id: "), "{first}");
        assert!(first.contains(r#""operationname":"playbackaccesstoken""#) && first.contains(&TOKEN_HASH.to_lowercase()), "{first}");
        assert!(first.contains(r#""playertype":"site""#) && first.contains(r#""login":"xqc""#));
        assert!(!first.contains("authorization:"), "an anonymous request carries no account");
        assert!(second.contains("authorization: oauth web-session"), "{second}");
    }

    #[tokio::test]
    async fn an_error_that_is_not_about_integrity_is_said_and_a_refusal_is_said_with_its_status() {
        let state = testkit::simulated_daemon("twitch-playback-errors");
        let (root, _) = graphql(vec![
            (200, json!({ "errors": [{ "message": "service timeout" }] }).to_string()),
            (400, json!({ "message": "The \"Client-ID\" header is invalid." }).to_string()),
        ])
        .await;
        let http = reqwest::Client::new();
        let e = playback(&state, &http, &root, USHER_ROOT, "xqc", None).await.unwrap_err();
        assert!(e.to_string().contains("service timeout"), "{e}");
        let e = playback(&state, &http, &root, USHER_ROOT, "xqc", None).await.unwrap_err();
        assert!(e.to_string().contains("400") && e.to_string().contains("Client-ID"), "{e}");
    }

    #[tokio::test]
    async fn when_twitch_wants_an_integrity_token_the_window_is_asked_and_the_call_is_made_again_with_it() {
        let state = testkit::simulated_daemon("twitch-playback-integrity");
        *cache().lock().unwrap() = None;
        let (root, seen) = graphql(vec![(200, json!({ "errors": [{ "message": "failed integrity check" }] }).to_string()), (200, token(json!({})).to_string())]).await;
        let mut events = state.events.subscribe();
        let answerer = tokio::spawn(async move {
            loop {
                let event = events.recv().await.expect("the bus is open");
                if event.name == "twitchIntegrityNeeded" {
                    let id = event.data["requestId"].as_str().unwrap().to_string();
                    assert_eq!(event.data["deviceId"], gql::device_id());
                    let ends = (chrono::Utc::now().timestamp() + 16 * 3600) * 1000;
                    assert!(provide_integrity(&id, Ok(("v4.local.made".to_string(), ends))));
                    return;
                }
            }
        });
        let got = playback(&state, &reqwest::Client::new(), &root, USHER_ROOT, "xqc", None).await.expect("works the second time");
        answerer.await.unwrap();
        assert!(got.url.contains("xqc.m3u8"));
        let second = seen.lock().unwrap()[1].to_lowercase();
        assert!(second.contains("client-integrity: v4.local.made"), "{second}");
        // Kept: the next call carries it without asking again.
        assert!(good_integrity().is_some());
        *cache().lock().unwrap() = None;
    }

    #[tokio::test]
    async fn an_integrity_token_that_cannot_be_made_is_a_reason_not_a_hang() {
        let state = testkit::simulated_daemon("twitch-playback-integrity-fails");
        *cache().lock().unwrap() = None;
        let (root, _) = graphql(vec![(200, json!({ "errors": [{ "message": "failed integrity check" }] }).to_string())]).await;
        let mut events = state.events.subscribe();
        tokio::spawn(async move {
            loop {
                let event = events.recv().await.expect("the bus is open");
                if event.name == "twitchIntegrityNeeded" {
                    provide_integrity(event.data["requestId"].as_str().unwrap(), Err("the page would not load".to_string()));
                    return;
                }
            }
        });
        let e = playback(&state, &reqwest::Client::new(), &root, USHER_ROOT, "xqc", None).await.unwrap_err();
        assert!(matches!(e, PlaybackError::NeedsIntegrity), "{e}");
        assert!(!provide_integrity("nobody-asked", Err("late".into())), "a late answer to nothing is refused");
    }
}
