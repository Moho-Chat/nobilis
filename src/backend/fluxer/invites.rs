//! A link to a community in what somebody said, drawn as what it leads to: the guild's picture, its name, how many are
//! in it and online, and a button to join. Fluxer's own app does the same from the invite's public description
//! (`GET /v1/invites/{code}`), so this asks it as the account, once for a code in a while, and only for links to this
//! instance's invite address or web app.

use super::{media, people, Handle};
use crate::model::{Embed, EmbedLink};
use crate::state::AppState;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How many links in one message are drawn.
const MOST: usize = 3;
/// How long what Fluxer said of an invite is kept.
const KEEP: Duration = Duration::from_secs(600);

type Previews = HashMap<String, (Instant, Option<Value>)>;
static PREVIEWS: LazyLock<Mutex<Previews>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn host_of(address: &str) -> Option<String> {
    url::Url::parse(address).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_ascii_lowercase()))
}

/// The invite codes in a message's words, in order, each once: links to the instance's invite address
/// (`fluxer.gg/Code`) or its web app (`fluxer.com/invite/Code`).
pub fn codes_in(content: &str, invite: &str, webapp: &str) -> Vec<(String, String)> {
    static LINK: OnceLock<Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| Regex::new(r#"https?://[^\s<>()\[\]"']+"#).expect("a fixed pattern parses"));
    let (invite_host, app_host) = (host_of(invite), host_of(webapp));
    let mut out: Vec<(String, String)> = Vec::new();
    for m in link.find_iter(content) {
        let text = m.as_str().trim_end_matches(['.', ',', '!', '?', ';', ':']);
        let Ok(url) = url::Url::parse(text) else { continue };
        let host = url.host_str().unwrap_or("").trim_start_matches("www.").to_ascii_lowercase();
        let parts: Vec<&str> = url.path_segments().map(|p| p.filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let ours = (invite_host.as_deref() == Some(host.as_str()) && (parts.len() == 1 || parts.first() == Some(&"invite"))) || (app_host.as_deref() == Some(host.as_str()) && parts.first() == Some(&"invite"));
        if !ours {
            continue;
        }
        if let Some(code) = people::invite_code(text) {
            if !out.iter().any(|(c, _)| *c == code) {
                out.push((code, text.to_string()));
            }
        }
        if out.len() >= MOST {
            break;
        }
    }
    out
}

/// The card for what Fluxer says an invite leads to.
pub fn card(handle: &Handle, preview: &Value, link: &str) -> Option<Embed> {
    let count = preview["member_count"].as_u64();
    let online = preview["presence_count"].as_u64();
    let people_in = |n: u64| format!("{n} {}", if n == 1 { "member" } else { "members" });
    match preview["guild"]["id"].as_str() {
        Some(guild) => {
            let joined = handle.world.lock().unwrap_or_else(|e| e.into_inner()).guilds.contains_key(guild);
            let icon = preview["guild"]["icon"].as_str().and_then(|h| media::asset_url(&handle.instance, "icons", guild, h)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp")));
            let description = match (count, online) {
                (Some(n), Some(o)) => Some(format!("{}, {o} online", people_in(n))),
                (Some(n), None) => Some(people_in(n)),
                _ => None,
            };
            Some(Embed {
                kind: Some("invite".into()),
                title: Some(preview["guild"]["name"].as_str()?.to_string()),
                description,
                image_url: icon,
                url: Some(link.to_string()),
                target: Some(guild.to_string()),
                cta: Some(EmbedLink { label: if joined { "Joined" } else { "Join" }.into(), url: Some(link.to_string()) }),
                ..Default::default()
            })
        }
        None if preview["channel"].is_object() => Some(Embed {
            kind: Some("invite".into()),
            title: Some(preview["channel"]["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("A group").to_string()),
            description: Some(match count {
                Some(n) => format!("A group, {}", people_in(n)),
                None => "A group".to_string(),
            }),
            url: Some(link.to_string()),
            cta: Some(EmbedLink { label: "Join".into(), url: Some(link.to_string()) }),
            ..Default::default()
        }),
        None => None,
    }
}

async fn preview(handle: &Handle, code: &str) -> Option<Value> {
    if let Some((at, known)) = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner()).get(code) {
        if at.elapsed() < KEEP {
            return known.clone();
        }
    }
    let found = handle.api.get(&format!("/invites/{code}"), &[]).await.ok();
    let mut held = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner());
    if held.len() > 512 {
        held.clear();
    }
    held.insert(code.to_string(), (Instant::now(), found.clone()));
    found
}

/// Puts a card under a message for each community it links to, once they have been asked about.
pub fn cards_later(state: &AppState, handle: &Handle, buffer_id: &str, message_id: &str, content: &str) {
    let codes = codes_in(content, &handle.instance.invite, &handle.instance.webapp);
    if codes.is_empty() {
        return;
    }
    let (state, handle, buffer_id, message_id) = (state.clone(), handle.clone(), buffer_id.to_string(), message_id.to_string());
    tokio::spawn(async move {
        for (code, link) in codes {
            let Some(found) = preview(&handle, &code).await else { continue };
            if let Some(embed) = card(&handle, &found, &link) {
                crate::unfurl::add_embed(&state, &buffer_id, &message_id, embed);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_to_this_instance_s_invites_are_found_and_nothing_else() {
        let said = "come to https://fluxer.gg/QGEnmqo9, or https://fluxer.com/invite/AbC123! not https://example.com/invite/zzz \
                    nor https://fluxer.com/channels/@me and again https://fluxer.gg/QGEnmqo9";
        let found: Vec<String> = codes_in(said, "https://fluxer.gg", "https://fluxer.com").into_iter().map(|(c, _)| c).collect();
        assert_eq!(found, vec!["QGEnmqo9", "AbC123"]);
        let local: Vec<String> = codes_in("http://localhost:8890/invite/Zed9", "http://localhost:8890/invite", "http://localhost:8890").into_iter().map(|(c, _)| c).collect();
        assert_eq!(local, vec!["Zed9"]);
        assert_eq!(codes_in("https://fluxer.gg/a1 https://fluxer.gg/a2 https://fluxer.gg/a3 https://fluxer.gg/a4", "https://fluxer.gg", "").len(), MOST);
    }
}
