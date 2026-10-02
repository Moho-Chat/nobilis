//! Discord stickers, sent.
//!
//! Reading them was already done (`messages::extract_stickers`); this is the
//! other half. A sticker is a message of its own - `sticker_ids` on an
//! otherwise empty message - so picking one sends it, the way it does in
//! Discord's own client.
//!
//! Two kinds are offered. Each guild's own, learned as the guild is
//! registered, which go in that guild's channels - and anywhere at all with
//! Nitro. And Discord's standard packs, which every account may send
//! anywhere, a DM included: without them an account with no Nitro had nothing
//! it could send in a DM. Where each may go is applied here, so the picker can
//! say so before the server refuses.

use super::*;
use std::sync::{Mutex, OnceLock};

/// One guild's stickers, and where they belong.
#[derive(Clone, Debug, Default)]
pub struct GuildStickers {
    pub guild_name: String,
    pub stickers: Vec<Sticker>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sticker {
    pub id: String,
    pub name: String,
    /// 1 PNG, 2 APNG, 3 Lottie, 4 GIF.
    pub format_type: u64,
}

impl Sticker {
    /// A picture of it, or none for Lottie, which is a vector animation in a
    /// format nothing here can draw. Still offered - by name - since it can
    /// still be sent.
    pub fn url(&self) -> Option<String> {
        match self.format_type {
            3 => None,
            4 => Some(format!("https://media.discordapp.net/stickers/{}.gif", self.id)),
            _ => Some(format!("https://media.discordapp.net/stickers/{}.png?size=160", self.id)),
        }
    }
}

/// Where a Lottie sticker's animation is. Only the CDN host serves it; the
/// media proxy answers 400.
pub fn lottie_url(id: &str) -> String {
    format!("https://cdn.discordapp.com/stickers/{id}.json")
}

fn lottie_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("nobilis")
        .join("discord-stickers")
}

/// Cap on the Lottie cache. An animation is tens of kilobytes, so this is
/// hundreds of stickers; one swept out is fetched again the next time it is
/// drawn, and the window keeps what it has already read.
pub const STICKER_CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024;

pub async fn sweep_sticker_cache() {
    crate::backend::sneedchat::sweep_cache_dir(&lottie_cache_dir(), STICKER_CACHE_MAX_BYTES, "discord sticker").await;
}

/// A Lottie sticker's animation on disk, fetched the first time it is asked
/// for.
///
/// Fetched here rather than by the window because the CDN sends no CORS
/// header, so a page cannot read it - and once fetched it never changes: a
/// sticker id names one animation for good.
///
/// Through the account's own client, so it goes out the way that account is
/// routed - over Tor, if the account is.
pub async fn lottie_file(token: &str, id: &str) -> Result<String> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!("that is not a sticker id");
    }
    let dir = lottie_cache_dir();
    let path = dir.join(format!("{id}.json"));
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        let resp = http_client_for(token)
            .get(lottie_url(id))
            .send()
            .await
            .context("fetching the sticker")?;
        if !resp.status().is_success() {
            bail!("Discord has no animation for that sticker ({})", resp.status());
        }
        let bytes = resp.bytes().await.context("reading the sticker")?;
        tokio::fs::create_dir_all(&dir).await.ok();
        // Written beside and renamed, so a reader never finds half a file.
        let partial = dir.join(format!("{id}.json.part"));
        tokio::fs::write(&partial, &bytes).await.context("saving the sticker")?;
        tokio::fs::rename(&partial, &path).await.context("saving the sticker")?;
    }
    Ok(format!("file://{}", path.display()))
}

/// Account to guild to its stickers.
fn known() -> &'static Mutex<HashMap<String, HashMap<String, GuildStickers>>> {
    static KNOWN: OnceLock<Mutex<HashMap<String, HashMap<String, GuildStickers>>>> = OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

/// The stickers a guild payload carries - READY's, GUILD_CREATE's, or the
/// REST guild a resync reads. Unavailable ones (a guild that lost the boost
/// that paid for the slot) are left out, as Discord's client leaves them.
pub fn read_stickers(guild: &Value) -> Vec<Sticker> {
    guild["stickers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["available"].as_bool().unwrap_or(true))
        .filter_map(|s| {
            Some(Sticker {
                id: s["id"].as_str()?.to_string(),
                name: s["name"].as_str()?.to_string(),
                format_type: s["format_type"].as_u64().unwrap_or(1),
            })
        })
        .collect()
}

/// Records a guild's stickers, replacing what was known.
pub fn note_guild(account_id: &str, guild_id: &str, entry: GuildStickers) {
    let mut all = known().lock().unwrap();
    let guilds = all.entry(account_id.to_string()).or_default();
    if entry.stickers.is_empty() {
        guilds.remove(guild_id);
    } else {
        guilds.insert(guild_id.to_string(), entry);
    }
}

/// Discord's standard packs, by pack name: the same for every account, so
/// fetched once.
type Packs = Mutex<Option<Vec<(String, Vec<Sticker>)>>>;

fn standard() -> &'static Packs {
    static STANDARD: OnceLock<Packs> = OnceLock::new();
    STANDARD.get_or_init(Default::default)
}

/// The standard packs out of `GET /sticker-packs`.
pub fn read_packs(body: &Value) -> Vec<(String, Vec<Sticker>)> {
    body["sticker_packs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pack| {
            let name = pack["name"].as_str()?.to_string();
            let stickers = read_stickers(pack);
            (!stickers.is_empty()).then_some((name, stickers))
        })
        .collect()
}

async fn standard_packs(token: &str) -> Vec<(String, Vec<Sticker>)> {
    if let Some(packs) = standard().lock().unwrap().clone() {
        return packs;
    }
    let fetched = async {
        let resp = http_client_for(token)
            .get(format!("{API_BASE}/sticker-packs"))
            .header("Authorization", token)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        Some(read_packs(&resp.json::<Value>().await.ok()?))
    }
    .await;
    match fetched {
        Some(packs) => {
            *standard().lock().unwrap() = Some(packs.clone());
            packs
        }
        // Not remembered, so the next time the picker opens asks again.
        None => Vec::new(),
    }
}

/// Every sticker the account has, and whether each can go where it is being
/// sent from.
pub async fn list(state: &AppState, account_id: &str, buffer_id: &str, token: &str) -> Vec<Value> {
    let standard = standard_packs(token).await;
    let mut out = guild_list(state, account_id, buffer_id);
    // After this conversation's own guild and before the others: they can go
    // anywhere, so they are the next most likely to be usable here.
    let own = state.runtime.get_discord_guild(buffer_id);
    let at = out
        .iter()
        .position(|s| own.is_none() || s["guildId"].as_str() != own.as_deref())
        .unwrap_or(out.len());
    let extra: Vec<Value> = standard
        .iter()
        .flat_map(|(pack, stickers)| {
            stickers.iter().map(move |sticker| {
                json!({
                    "id": sticker.id,
                    "name": sticker.name,
                    "pack": pack,
                    "body": sticker.name,
                    "url": sticker.url(),
                    "lottie": sticker.format_type == 3,
                    "locked": false,
                })
            })
        })
        .collect();
    out.splice(at..at, extra);
    out
}

fn guild_list(state: &AppState, account_id: &str, buffer_id: &str) -> Vec<Value> {
    let anywhere = state.runtime.emoji_unrestricted(account_id);
    // Absent in a DM, which belongs to no guild - and where, without Nitro,
    // no guild's sticker can go.
    let here = state.runtime.get_discord_guild(buffer_id);
    let all = known().lock().unwrap();
    let Some(guilds) = all.get(account_id) else { return Vec::new() };
    let mut out = Vec::new();
    // This conversation's own guild first: what is most likely wanted, and
    // the only ones usable without Nitro.
    let mut ordered: Vec<(&String, &GuildStickers)> = guilds.iter().collect();
    ordered.sort_by_key(|(id, g)| (here.as_deref() != Some(id.as_str()), g.guild_name.to_lowercase()));
    for (guild_id, guild) in ordered {
        let usable = anywhere || here.as_deref() == Some(guild_id.as_str());
        for sticker in &guild.stickers {
            out.push(json!({
                "id": sticker.id,
                "name": sticker.name,
                "pack": guild.guild_name,
                "body": sticker.name,
                "url": sticker.url(),
                "lottie": sticker.format_type == 3,
                "locked": !usable,
                "guildId": guild_id,
            }));
        }
    }
    out
}

/// Sends a sticker as a message of its own.
pub async fn send(state: &AppState, buffer_id: &str, token: &str, sticker_id: &str) -> Result<()> {
    let channel_id = state.runtime.get_discord_channel(buffer_id).ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let resp = send_write(
        http_client_for(token)
            .post(format!("{API_BASE}/channels/{channel_id}/messages"))
            .header("Authorization", token)
            .json(&json!({ "sticker_ids": [sticker_id] })),
    )
    .await
    .context("sending a Discord sticker")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guild_offers_its_available_stickers() {
        let guild = json!({ "stickers": [
            { "id": "1", "name": "wave", "format_type": 1 },
            { "id": "2", "name": "gone", "format_type": 1, "available": false },
            { "id": "3", "name": "dance", "format_type": 4 },
            { "id": "4", "name": "vector", "format_type": 3 },
        ]});
        let stickers = read_stickers(&guild);
        assert_eq!(stickers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["wave", "dance", "vector"]);
        assert!(stickers[1].url().unwrap().ends_with("3.gif"));
        // Offered by name: it can be sent, just not drawn here.
        assert_eq!(stickers[2].url(), None);
    }

    #[test]
    fn standard_packs_are_read_by_name() {
        let body = json!({ "sticker_packs": [
            { "name": "Wumpus Beyond", "stickers": [{ "id": "5", "name": "wave", "format_type": 3 }] },
            { "name": "Empty", "stickers": [] },
        ]});
        let packs = read_packs(&body);
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].0, "Wumpus Beyond");
        assert_eq!(packs[0].1[0].id, "5");
    }

    #[test]
    fn a_guild_with_no_stickers_has_none() {
        assert!(read_stickers(&json!({})).is_empty());
    }
}
