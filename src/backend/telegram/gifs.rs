//! GIFs: the account's saved ones, and a search through Telegram's own GIF bot.
//!
//! Telegram does not send a GIF as a link: a GIF is a short silent video that lives on Telegram, and sending one is
//! sending that file. The account keeps a list of saved GIFs, and the search is the inline bot `@gif`, which answers
//! with files of the same kind. Both are listed as the picker's cells; the pictures and the videos are named in the
//! cache and fetched when the picker draws them, from the files remembered here (a file is fetched with its access
//! key and a reference that goes stale, so what was listed is what can be fetched).

use super::media::{download_thumb, download_to, extension_of, MEDIA};
use super::messages;
use super::send::{explain, peer_for};
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use grammers_client::media::Document;
use grammers_client::message::InputMessage;
use grammers_client::tl;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How many GIFs a list shows.
const LIMIT: usize = 100;

/// The documents that were listed, by (account, id): what the picker names is what can be fetched or sent.
type Known = Mutex<HashMap<(i64, i64), tl::types::Document>>;

fn known() -> &'static Known {
    static K: std::sync::OnceLock<Known> = std::sync::OnceLock::new();
    K.get_or_init(Default::default)
}

/// A GIF's cached file: `tgif_<account>_<id>_<thumb|full>.<ext>`.
fn name(account: i64, id: i64, thumb: bool, ext: &str) -> String {
    format!("tgif_{account}_{id}_{}.{ext}", if thumb { "thumb" } else { "full" })
}

/// What a `tgif_` name says: the account, the document and which of the two files it is.
fn parse_name(file: &str) -> Option<(i64, i64, bool)> {
    let rest = file.strip_prefix("tgif_")?;
    let stem = rest.rsplit_once('.')?.0;
    let mut parts = stem.split('_');
    let account = parts.next()?.parse().ok()?;
    let id = parts.next()?.parse().ok()?;
    let thumb = match parts.next()? {
        "thumb" => true,
        "full" => false,
        _ => return None,
    };
    parts.next().is_none().then_some((account, id, thumb))
}

fn dimensions(doc: &tl::types::Document) -> (i32, i32) {
    doc.attributes
        .iter()
        .find_map(|a| match a {
            tl::enums::DocumentAttribute::Video(v) => Some((v.w, v.h)),
            tl::enums::DocumentAttribute::ImageSize(s) => Some((s.w, s.h)),
            _ => None,
        })
        .unwrap_or((0, 0))
}

fn title_of(doc: &tl::types::Document) -> String {
    doc.attributes
        .iter()
        .find_map(|a| match a {
            tl::enums::DocumentAttribute::Filename(f) => Some(f.file_name.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "GIF".to_string())
}

pub(super) fn wrapper(doc: &tl::types::Document) -> Document {
    Document::from_raw_media(tl::types::MessageMediaDocument {
        nopremium: false,
        spoiler: false,
        video: true,
        round: false,
        voice: false,
        document: Some(tl::enums::Document::Document(doc.clone())),
        alt_documents: None,
        video_cover: None,
        video_timestamp: None,
        ttl_seconds: None,
    })
}

/// A listed document as the picker's cell: its files named in the cache, fetched when drawn.
fn entry(account: i64, doc: &tl::types::Document) -> Value {
    let (w, h) = dimensions(doc);
    let full = format!("file://{}", crate::media_cache::dir(MEDIA).join(name(account, doc.id, false, &extension_of(None, Some(&doc.mime_type)))).display());
    let thumb = format!("file://{}", crate::media_cache::dir(MEDIA).join(name(account, doc.id, true, "jpg")).display());
    json!({ "id": doc.id.to_string(), "title": title_of(doc), "url": "", "src": full, "gifSrc": full, "preview": thumb, "width": w, "height": h })
}

fn remember(account: i64, docs: Vec<tl::types::Document>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut held = known().lock().unwrap_or_else(|e| e.into_inner());
    for doc in docs.into_iter().take(LIMIT) {
        out.push(entry(account, &doc));
        held.insert((account, doc.id), doc);
    }
    out
}

fn documents(list: Vec<tl::enums::Document>) -> Vec<tl::types::Document> {
    list.into_iter()
        .filter_map(|d| match d {
            tl::enums::Document::Document(d) => Some(d),
            _ => None,
        })
        .collect()
}

/// What the picker shows: the saved GIFs when nothing is typed, else what Telegram's GIF search finds.
pub async fn list(state: &AppState, buffer: &Buffer, query: &str) -> Result<Value, String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    let account = handle.own.user_id;
    let docs = if query.trim().is_empty() {
        let answer = handle.client.invoke(&tl::functions::messages::GetSavedGifs { hash: 0 }).await.map_err(|e| explain(&e))?;
        match answer {
            tl::enums::messages::SavedGifs::Gifs(g) => documents(g.gifs),
            tl::enums::messages::SavedGifs::NotModified => Vec::new(),
        }
    } else {
        search(&handle, peer, query.trim()).await.map_err(|e| e.to_string())?
    };
    Ok(json!({ "gifs": remember(account, docs), "categories": [] }))
}

async fn search(handle: &Handle, peer: grammers_session::types::PeerRef, query: &str) -> Result<Vec<tl::types::Document>> {
    let bot = handle.client.resolve_username("gif").await.map_err(|e| anyhow!("{}", explain(&e)))?.ok_or_else(|| anyhow!("Telegram's GIF search is not available"))?;
    let bot_ref = bot.to_ref().await.ok().flatten().ok_or_else(|| anyhow!("Telegram's GIF search is not available"))?;
    let answer = handle
        .client
        .invoke(&tl::functions::messages::GetInlineBotResults { bot: bot_ref.into(), peer: peer.into(), geo_point: None, query: query.to_string(), offset: String::new() })
        .await
        .map_err(|e| anyhow!("{}", explain(&e)))?;
    let tl::enums::messages::BotResults::Results(found) = answer;
    Ok(found
        .results
        .into_iter()
        .filter_map(|r| match r {
            tl::enums::BotInlineResult::BotInlineMediaResult(m) => m.document,
            _ => None,
        })
        .filter_map(|d| match d {
            tl::enums::Document::Document(d) => Some(d),
            _ => None,
        })
        .collect())
}

/// Sends a GIF that was listed, to a conversation.
pub async fn send(state: &AppState, buffer: &Buffer, id: &str) -> Result<(), String> {
    let id: i64 = id.parse().map_err(|_| "that is not a GIF".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let doc = known().lock().unwrap_or_else(|e| e.into_inner()).get(&(handle.own.user_id, id)).cloned().ok_or_else(|| "That GIF is no longer listed: open the picker again".to_string())?;
    let media = wrapper(&doc).to_raw_input_media();
    // In a topic of a group the GIF answers the topic's first message, which is how it is sent there.
    let reply = super::topics::topic_of_buffer(buffer);
    let sent = handle.client.send_message(peer, InputMessage::new().media(tl::enums::InputMedia::Document(media)).reply_to(reply)).await.map_err(|e| explain(&e))?;
    messages::record(state, &buffer.account_id, &handle.own, &sent);
    Ok(())
}

/// Fetches one of a listed GIF's files again, for the window asking for it.
pub async fn restore(state: &AppState, file: &str, path: &Path) -> Result<Option<PathBuf>> {
    let Some((account, id, thumb)) = parse_name(file) else { return Ok(None) };
    let handle = state.runtime.telegram_handle(&format!("telegram:{account}")).context("that Telegram account is not connected")?;
    let doc = known().lock().unwrap_or_else(|e| e.into_inner()).get(&(account, id)).cloned().context("that GIF is no longer listed")?;
    let wrapped = wrapper(&doc);
    if thumb {
        download_thumb(&handle.client, &wrapped, path).await?;
    } else {
        download_to(&handle.client, &wrapped, path, false).await?;
    }
    Ok(Some(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gifs_files_are_named_and_read_back() {
        let video = name(1234567890, 5_000_000_000_001, false, "mp4");
        assert_eq!(video, "tgif_1234567890_5000000000001_full.mp4");
        assert_eq!(parse_name(&video), Some((1234567890, 5_000_000_000_001, false)));
        assert_eq!(parse_name(&name(1, 7, true, "jpg")), Some((1, 7, true)));
        assert_eq!(parse_name("tg_1_n5_2_full.jpg"), None);
        assert_eq!(parse_name("tgif_1_2_other.jpg"), None);
    }
}
