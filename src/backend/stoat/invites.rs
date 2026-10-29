//! A link to a server in what somebody said, drawn as what it leads to: the server's picture, its name, how many are in
//! it, and a button to join. Stoat's own app does the same, from the invite's public preview (`GET /invites/{code}`),
//! which anybody may ask - so this asks it as the account, once for a code in a while, and only for links to Stoat.

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
/// How long what Stoat said of an invite is kept: a server's name and count change slowly.
const KEEP: Duration = Duration::from_secs(600);

/// What Stoat said of an invite, and when; nothing, where it said there is nothing.
type Previews = HashMap<String, (Instant, Option<Value>)>;

static PREVIEWS: LazyLock<Mutex<Previews>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The invite codes in a message's words, in order, each once: links to the official addresses and to this instance's own.
pub fn codes_in(content: &str, app: Option<&str>) -> Vec<(String, String)> {
    static LINK: OnceLock<Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| Regex::new(r#"https?://[^\s<>()\[\]"']+"#).expect("a fixed pattern parses"));
    let own_host = app.and_then(|a| url::Url::parse(a).ok()).and_then(|u| u.host_str().map(str::to_string));
    let mut out: Vec<(String, String)> = Vec::new();
    for m in link.find_iter(content) {
        let text = m.as_str().trim_end_matches(['.', ',', '!', '?', ';', ':']);
        let Ok(url) = url::Url::parse(text) else { continue };
        let host = url.host_str().unwrap_or("").trim_start_matches("www.").to_ascii_lowercase();
        let known = ["stoat.chat", "app.stoat.chat", "revolt.chat", "app.revolt.chat", "rvlt.gg", "stt.gg", "stoat.gg"].contains(&host.as_str()) || own_host.as_deref() == Some(host.as_str());
        if !known {
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

/// The card for what Stoat says an invite leads to.
pub fn card(handle: &Handle, preview: &Value, link: &str) -> Option<Embed> {
    let icon = |file: &Value, tag: &str| file["_id"].as_str().and_then(|id| media::file_url(&handle.instance, tag, id)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, None));
    let count = preview["member_count"].as_u64();
    let people_in = |n: u64| format!("{n} {}", if n == 1 { "member" } else { "members" });
    let joined_already = |server: &str| handle.world.lock().unwrap_or_else(|e| e.into_inner()).servers.contains_key(server);
    match preview["type"].as_str()? {
        "Server" => {
            let server = preview["server_id"].as_str()?;
            Some(Embed {
                kind: Some("invite".into()),
                title: Some(preview["server_name"].as_str()?.to_string()),
                description: count.map(people_in),
                image_url: icon(&preview["server_icon"], "icons"),
                url: Some(link.to_string()),
                target: Some(server.to_string()),
                cta: Some(EmbedLink { label: if joined_already(server) { "Joined" } else { "Join" }.into(), url: Some(link.to_string()) }),
                ..Default::default()
            })
        }
        "Group" => Some(Embed {
            kind: Some("invite".into()),
            title: Some(preview["channel_name"].as_str()?.to_string()),
            description: Some(match count {
                Some(n) => format!("A group, {}", people_in(n)),
                None => "A group".to_string(),
            }),
            url: Some(link.to_string()),
            cta: Some(EmbedLink { label: "Join".into(), url: Some(link.to_string()) }),
            ..Default::default()
        }),
        _ => None,
    }
}

/// Asks about an invite, or remembers what it was told lately (and that there was nothing to tell).
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

/// Puts a card under a message for each Stoat server it links to, when they have been asked about.
pub fn cards_later(state: &AppState, handle: &Handle, buffer_id: &str, message_id: &str, content: &str) {
    let codes = codes_in(content, handle.instance.app.as_deref());
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
    use serde_json::json;

    #[test]
    fn links_to_a_stoat_server_are_found_in_what_was_said_and_nothing_else() {
        let said = "come to https://stt.gg/C9K0vVxd, or https://stoat.chat/invite/AbC123! not https://example.com/invite/zzz nor https://stt.gg/discover/servers \
                    and again https://stt.gg/C9K0vVxd";
        let found: Vec<String> = codes_in(said, None).into_iter().map(|(c, _)| c).collect();
        assert_eq!(found, vec!["C9K0vVxd", "AbC123"]);
        let own: Vec<String> = codes_in("https://chat.example.org/invite/Zed9", Some("https://chat.example.org")).into_iter().map(|(c, _)| c).collect();
        assert_eq!(own, vec!["Zed9"], "an instance's own web app is believed too");
        assert!(codes_in("https://chat.example.org/invite/Zed9", None).is_empty());
        assert_eq!(codes_in("https://rvlt.gg/a1 https://rvlt.gg/a2 https://rvlt.gg/a3 https://rvlt.gg/a4", None).len(), MOST);
    }

    #[test]
    fn a_server_s_invite_becomes_a_card_that_says_whether_it_is_joined_already() {
        let h = crate::backend::stoat::tests::bare_handle("stoat:ME");
        let preview = json!({ "type": "Server", "code": "C9", "server_id": "S1", "server_name": "kittens", "member_count": 5, "channel_name": "announcements" });
        let card1 = card(&h, &preview, "https://stt.gg/C9").unwrap();
        assert_eq!((card1.kind.as_deref(), card1.title.as_deref(), card1.description.as_deref(), card1.target.as_deref()), (Some("invite"), Some("kittens"), Some("5 members"), Some("S1")));
        assert_eq!(card1.cta.as_ref().map(|c| c.label.as_str()), Some("Join"));
        h.world.lock().unwrap().servers.insert("S1".into(), crate::backend::stoat::world::parse_server(&json!({ "_id": "S1", "owner": "X", "name": "kittens" })).unwrap());
        assert_eq!(card(&h, &preview, "https://stt.gg/C9").unwrap().cta.map(|c| c.label), Some("Joined".to_string()));
        let one = card(&h, &json!({ "type": "Server", "server_id": "S2", "server_name": "tiny", "member_count": 1 }), "l").unwrap();
        assert_eq!(one.description.as_deref(), Some("1 member"));
        assert!(card(&h, &json!({ "type": "Bot" }), "l").is_none());
    }
}
