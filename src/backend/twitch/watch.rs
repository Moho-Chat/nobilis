//! Telling Twitch a channel is being watched.
//!
//! Twitch gives a viewer channel points for time spent watching, and counts that time from what the website's
//! player reports: once a minute, a small "minute-watched" event naming the channel, the broadcast and the viewer,
//! sent to Twitch's telemetry address. Moho plays the video itself, so without this Twitch never hears that anyone
//! is watching and no points build up. This sends the same report, and only while the window is really playing
//! the channel's video: the window asks once a minute while the picture is playing and visible, and stops when it
//! is paused, hidden, closed or minimised.
//!
//! What it does not do: it claims nothing (the bonus chest is out of scope), it reports no faster than once a
//! minute for a channel however many windows ask, it reports nothing for a channel that is not live or an
//! account with no twitch.tv session, and it can be turned off in Settings -> Twitch.

use super::stream;
use crate::state::AppState;
use base64::Engine;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Twitch's own page, which says where its player reports to.
pub const PAGE_ROOT: &str = "https://www.twitch.tv";

/// Just under the minute the window asks on, so two windows on one channel cost one report.
const MIN_GAP: Duration = Duration::from_secs(50);

fn last_reports() -> &'static Mutex<HashMap<String, Instant>> {
    static LAST: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    LAST.get_or_init(Default::default)
}

fn known_address() -> &'static Mutex<Option<String>> {
    static ADDRESS: std::sync::OnceLock<Mutex<Option<String>>> = std::sync::OnceLock::new();
    ADDRESS.get_or_init(Default::default)
}

/// The address of the website's settings script, which a channel's page names.
pub fn settings_script(page: &str) -> Option<String> {
    let found = regex::Regex::new(r#"https?://[A-Za-z0-9.:/_-]*?/config/settings\.[A-Za-z0-9]+\.js"#).ok()?.find(page)?;
    Some(found.as_str().to_string())
}

/// Where the player reports to, from the settings script.
pub fn spade_address(script: &str) -> Option<String> {
    let found = regex::Regex::new(r#""spade_url":"([^"]+)""#).ok()?.captures(script)?;
    Some(found[1].replace("\\/", "/"))
}

/// The report, as the player writes it: a list with one event, as JSON, in base64.
pub fn minute_watched(channel_id: &str, broadcast_id: u64, user_id: &str) -> String {
    let events = serde_json::json!([{
        "event": "minute-watched",
        "properties": { "channel_id": channel_id, "broadcast_id": broadcast_id.to_string(), "player": "site", "user_id": user_id }
    }]);
    base64::engine::general_purpose::STANDARD.encode(events.to_string())
}

async fn address(http: &reqwest::Client, page_root: &str, channel: &str, fresh: bool) -> Result<String, String> {
    if !fresh {
        if let Some(known) = known_address().lock().unwrap().clone() {
            return Ok(known);
        }
    }
    let ask = |url: String| async move { http.get(url).send().await.map_err(|e| format!("Twitch's page would not load: {e}"))?.text().await.map_err(|e| format!("Twitch's page would not load: {e}")) };
    let page = ask(format!("{page_root}/{channel}")).await?;
    let script_url = settings_script(&page).ok_or("Twitch's page does not say where its player reports")?;
    let script = ask(script_url).await?;
    let found = spade_address(&script).ok_or("Twitch's settings do not say where its player reports")?;
    *known_address().lock().unwrap() = Some(found.clone());
    Ok(found)
}

/// Reports that the account is watching the channel of this conversation, if it is live and the account has a
/// twitch.tv session and this channel was not reported in the last minute. True when a report was sent.
pub async fn report(state: &AppState, gql_root: &str, page_root: &str, buffer_id: &str) -> Result<bool, String> {
    let (cfg, channel) = super::points::session_of(state, buffer_id)?;
    if cfg.web_token.is_empty() || cfg.user_id.is_empty() {
        return Ok(false);
    }
    {
        let mut last = last_reports().lock().unwrap();
        let key = format!("{}|{channel}", cfg.user_id);
        if last.get(&key).is_some_and(|at| at.elapsed() < MIN_GAP) {
            return Ok(false);
        }
        last.insert(key, Instant::now());
    }
    let http = super::client().map_err(|e| format!("{e:#}"))?;
    let found = stream::channels(&http, gql_root, std::slice::from_ref(&channel)).await.map_err(|e| format!("{e:#}"))?;
    let Some((id, live)) = found.into_iter().next().flatten().and_then(|c| c.live.map(|l| (c.id, l))) else { return Ok(false) };
    let body = format!("data={}", minute_watched(&id, live.id, &cfg.user_id));
    for fresh in [false, true] {
        let to = address(&http, page_root, &channel, fresh).await?;
        let sent = http.post(&to).header("Content-Type", "application/x-www-form-urlencoded").body(body.clone()).send().await;
        match sent {
            Ok(res) if res.status().is_success() => return Ok(true),
            // The address moved with a new script: look again once.
            Ok(res) if fresh => return Err(format!("Twitch did not take the report ({})", res.status())),
            Err(e) if fresh => return Err(format!("Twitch's reports are not answering: {e}")),
            _ => {}
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_is_the_players_own_shape() {
        let encoded = minute_watched("71", 1234, "9");
        let decoded: serde_json::Value = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(encoded).unwrap()).unwrap();
        assert_eq!(decoded[0]["event"], "minute-watched");
        assert_eq!(decoded[0]["properties"]["channel_id"], "71");
        assert_eq!(decoded[0]["properties"]["broadcast_id"], "1234");
        assert_eq!(decoded[0]["properties"]["user_id"], "9");
        assert_eq!(decoded[0]["properties"]["player"], "site");
    }

    #[tokio::test]
    async fn a_live_channel_is_reported_once_a_minute() {
        use super::super::points::tests::{channel, fake_gql};
        *known_address().lock().unwrap() = None;
        last_reports().lock().unwrap().clear();
        let (state, buffer) = channel("twitch-watch", "websession");
        let (root, seen) = fake_gql(|req| {
            let line = req.lines().next().unwrap_or("").to_string();
            if line.starts_with("POST /gql") || req.contains("users(logins") {
                r#"{"data":{"users":[{"id":"71","login":"xqc","displayName":"xQc","stream":{"id":"555","title":"t","viewersCount":5,"createdAt":"2026-10-14T10:00:00Z","type":"live"}}]}}"#.to_string()
            } else if line.starts_with("GET /xqc ") {
                // The page names the script on the same fake server.
                let host = req.lines().find_map(|l| l.strip_prefix("host: ").or_else(|| l.strip_prefix("Host: "))).unwrap_or("").trim().to_string();
                format!(r#"<script src="http://{host}/config/settings.abc.js"></script>"#)
            } else if line.starts_with("GET /config/") {
                let host = req.lines().find_map(|l| l.strip_prefix("host: ").or_else(|| l.strip_prefix("Host: "))).unwrap_or("").trim().to_string();
                format!(r#"x={{"spade_url":"http://{host}/track"}}"#)
            } else {
                String::new()
            }
        })
        .await;
        assert!(report(&state, &format!("{root}/gql"), &root, &buffer).await.unwrap());
        let requests = seen.lock().unwrap().clone();
        let posted = requests.iter().find(|r| r.starts_with("POST /track")).expect("a report was sent");
        assert!(posted.contains("data="), "{posted}");
        // Asked again at once: nothing more goes out.
        assert!(!report(&state, &format!("{root}/gql"), &root, &buffer).await.unwrap());
        assert_eq!(seen.lock().unwrap().iter().filter(|r| r.starts_with("POST /track")).count(), 1);
    }

    #[test]
    fn where_to_report_is_read_from_the_page_and_its_script() {
        let page = r#"<script src="https://assets.twitch.tv/config/settings.abc123.js"></script>"#;
        assert_eq!(settings_script(page).as_deref(), Some("https://assets.twitch.tv/config/settings.abc123.js"));
        assert_eq!(spade_address(r#"x={"spade_url":"https:\/\/spade.example\/track","a":1}"#).as_deref(), Some("https://spade.example/track"));
        assert_eq!(settings_script("<html></html>"), None);
        assert_eq!(spade_address("nothing"), None);
    }
}
