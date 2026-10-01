//! Discord stickers, sent.
//!
//! Reading them was already done (`messages::extract_stickers`); this is the
//! other half. A sticker is a message of its own - `sticker_ids` on an
//! otherwise empty message - so picking one sends it, the way it does in
//! Discord's own client.
//!
//! The ones offered are each guild's own, learned as the guild is registered.
//! Where they may be sent is Discord's rule, applied here so the picker can
//! say so before the server refuses: a guild's stickers in that guild's
//! channels, and anywhere at all with Nitro.

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

/// Every sticker the account has, and whether each can go where it is being
/// sent from.
pub fn list(state: &AppState, account_id: &str, buffer_id: &str) -> Vec<Value> {
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
                "locked": !usable,
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
    fn a_guild_with_no_stickers_has_none() {
        assert!(read_stickers(&json!({})).is_empty());
    }
}
