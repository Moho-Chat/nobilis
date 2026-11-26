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
        None if preview["channel"].is_object() => {
            let channel = preview["channel"]["id"].as_str();
            let joined = channel.is_some_and(|c| handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.contains_key(c));
            Some(Embed {
                kind: Some("invite".into()),
                title: Some(preview["channel"]["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("A group").to_string()),
                description: Some(match count {
                    Some(n) => format!("A group, {}", people_in(n)),
                    None => "A group".to_string(),
                }),
                url: Some(link.to_string()),
                target: channel.map(str::to_string),
                cta: Some(EmbedLink { label: if joined { "Joined" } else { "Join" }.into(), url: Some(link.to_string()) }),
                ..Default::default()
            })
        }
        None => None,
    }
}

/// Drops any cached invite preview for a guild that was renamed or had its icon changed.
pub fn invalidate_guild(guild_id: &str) {
    PREVIEWS.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, (_, v)| {
        v.as_ref().and_then(|p| p["guild"]["id"].as_str()) != Some(guild_id)
    });
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

/// Who sent a link in a private conversation, which makes it an invitation to this account: their id and name.
pub type Sender = (String, String);

/// Puts a card under a message for each community it links to, once they have been asked about. A link somebody else
/// sent this account in a direct message or a group is also an invitation, offered in the invitations tile until it is
/// answered (see `offers`).
pub fn cards_later(state: &AppState, handle: &Handle, buffer_id: &str, message_id: &str, content: &str, sender: Option<Sender>) {
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
            if let Some((from, name)) = sender.as_ref() {
                offer(&state, &handle, &code, &found, from, name, &message_id);
            }
        }
    });
}

/// An invitation somebody sent this account as a link: to a community, or to a group.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    pub code: String,
    /// The guild's id, or the group's channel id.
    pub target: String,
    pub group: bool,
    pub name: String,
    pub icon: Option<String>,
    pub members: Option<u64>,
    pub from: String,
    pub from_name: String,
    /// The message it came in, so a later one with the same link does not offer it twice over.
    pub message: String,
}

type Offers = HashMap<String, Vec<Offer>>;
static OFFERS: LazyLock<Mutex<Offers>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn answered_path() -> std::path::PathBuf {
    crate::default_data_dir().join("fluxer-invites-answered.json")
}

/// Codes this person has already answered, by account: kept on disk so an old link is not offered again after a
/// restart when its conversation is read once more.
fn answered() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static ANSWERED: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    ANSWERED.get_or_init(|| Mutex::new(std::fs::read(answered_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()))
}

fn is_answered(account: &str, code: &str) -> bool {
    answered().lock().unwrap_or_else(|e| e.into_inner()).get(account).is_some_and(|l| l.iter().any(|c| c == code))
}

fn mark_answered(account: &str, code: &str) {
    let mut all = answered().lock().unwrap_or_else(|e| e.into_inner());
    let list = all.entry(account.to_string()).or_default();
    if !list.iter().any(|c| c == code) {
        list.push(code.to_string());
        // The oldest go first: a list of every link ever answered is not worth keeping.
        if list.len() > 500 {
            list.remove(0);
        }
    }
    if let Ok(bytes) = serde_json::to_vec(&*all) {
        let _ = std::fs::create_dir_all(crate::default_data_dir());
        let _ = std::fs::write(answered_path(), bytes);
    }
}

/// What a preview offers this account, if anything: nothing for a community or group it is already in, a link it
/// answered before, or one it sent itself.
fn offer_of(handle: &Handle, code: &str, preview: &Value, from: &str, from_name: &str, message: &str) -> Option<Offer> {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    if from == world.own_id || is_answered(&handle.account_id, code) {
        return None;
    }
    let members = preview["member_count"].as_u64();
    match preview["guild"]["id"].as_str() {
        Some(guild) => (!world.guilds.contains_key(guild)).then(|| Offer {
            code: code.to_string(),
            target: guild.to_string(),
            group: false,
            name: preview["guild"]["name"].as_str().unwrap_or("A community").to_string(),
            icon: preview["guild"]["icon"].as_str().and_then(|h| media::asset_url(&handle.instance, "icons", guild, h)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp"))),
            members,
            from: from.to_string(),
            from_name: from_name.to_string(),
            message: message.to_string(),
        }),
        None => {
            let channel = preview["channel"]["id"].as_str()?;
            (!world.channels.contains_key(channel)).then(|| Offer {
                code: code.to_string(),
                target: channel.to_string(),
                group: true,
                name: preview["channel"]["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("A group").to_string(),
                icon: None,
                members,
                from: from.to_string(),
                from_name: from_name.to_string(),
                message: message.to_string(),
            })
        }
    }
}

fn offer(state: &AppState, handle: &Handle, code: &str, preview: &Value, from: &str, from_name: &str, message: &str) {
    let Some(offer) = offer_of(handle, code, preview, from, from_name, message) else { return };
    {
        let mut all = OFFERS.lock().unwrap_or_else(|e| e.into_inner());
        let list = all.entry(handle.account_id.clone()).or_default();
        if list.iter().any(|o| o.code == offer.code || o.target == offer.target) {
            return;
        }
        list.push(offer);
    }
    tell(state, &handle.account_id);
}

fn as_json(o: &Offer) -> Value {
    serde_json::json!({
        "code": o.code, "target": o.target, "group": o.group, "name": o.name, "iconUrl": o.icon,
        "members": o.members, "fromId": o.from, "fromName": o.from_name,
    })
}

/// The invitations waiting on this account, as the window lists them.
pub fn offers(handle: &Handle) -> Value {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = OFFERS.lock().unwrap_or_else(|e| e.into_inner());
    let list = all.entry(handle.account_id.clone()).or_default();
    // One joined some other way since is no longer an invitation.
    list.retain(|o| if o.group { !world.channels.contains_key(&o.target) } else { !world.guilds.contains_key(&o.target) });
    Value::Array(list.iter().map(as_json).collect())
}

/// Says this account's invitations again, for the tile.
pub fn tell(state: &AppState, account_id: &str) {
    let list: Vec<Value> = OFFERS.lock().unwrap_or_else(|e| e.into_inner()).get(account_id).map(|l| l.iter().map(as_json).collect()).unwrap_or_default();
    state.events.emit("fluxerInviteOffers", serde_json::json!({ "accountId": account_id, "offers": list }));
}

/// Joins what an invitation leads to, or turns it down; either way it is answered and not offered again.
pub async fn answer(state: &AppState, handle: &Handle, code: &str, accept: bool) -> anyhow::Result<()> {
    if accept {
        people::join_invite(state, handle, code).await?;
    }
    mark_answered(&handle.account_id, code);
    if let Some(list) = OFFERS.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&handle.account_id) {
        list.retain(|o| o.code != code);
    }
    tell(state, &handle.account_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_answered_is_not_offered_again() {
        // In memory only: the file is written in the data directory, which a test does not set up here.
        let mut all = answered().lock().unwrap();
        all.entry("fluxer:test".into()).or_default().push("Abc".into());
        drop(all);
        assert!(is_answered("fluxer:test", "Abc"));
        assert!(!is_answered("fluxer:test", "Other"));
        assert!(!is_answered("fluxer:else", "Abc"));
    }

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
