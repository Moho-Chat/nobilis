//! Stickers: the packs an account has installed, the ones it uses most, and sending one.
//!
//! A sticker is a document that lives on Telegram, so sending one is sending that file, the way a GIF is. The account
//! keeps three lists the picker draws: the stickers it sent lately, the ones it marked as favourites, and its
//! installed packs. A pack is asked for on its own, which is a request each, so they are read a few at a time and
//! kept for a while. Each sticker is drawn from the still picture Telegram sends with it (an animated or video
//! sticker has one too), named in the cache and fetched when the picker draws it, from the document remembered
//! here: a file is fetched with its access key and a reference that goes stale, so what was listed is what can be
//! fetched.

use super::gifs::wrapper;
use super::media::{download_thumb, download_to, MEDIA};
use super::messages;
use super::send::{explain, peer_for};
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{Context, Result};
use futures::StreamExt;
use grammers_client::message::InputMessage;
use grammers_client::tl;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many of the most recent stickers are listed.
const RECENT: usize = 40;

/// How many installed packs are read. An account can have hundreds; the picker is for the ones in daily use, which
/// Telegram lists first.
const PACKS: usize = 40;

/// How many packs are asked for at once.
const AT_ONCE: usize = 4;

/// How long a pack's stickers are kept before it is asked for again.
const KEEP: Duration = Duration::from_secs(15 * 60);

/// The documents that were listed, by (account, id): what the picker names is what can be fetched or sent.
type Known = Mutex<HashMap<(i64, i64), tl::types::Document>>;

fn known() -> &'static Known {
    static K: std::sync::OnceLock<Known> = std::sync::OnceLock::new();
    K.get_or_init(Default::default)
}

/// A pack's stickers, when they were read.
type Packs = Mutex<HashMap<(i64, i64), (Instant, Vec<tl::types::Document>)>>;

fn packs() -> &'static Packs {
    static P: std::sync::OnceLock<Packs> = std::sync::OnceLock::new();
    P.get_or_init(Default::default)
}

/// A sticker's cached picture: `tstk_<account>_<id>.webp`.
fn name(account: i64, id: i64) -> String {
    format!("tstk_{account}_{id}.webp")
}

/// What a `tstk_` name says: the account and the document.
fn parse_name(file: &str) -> Option<(i64, i64)> {
    let stem = file.strip_prefix("tstk_")?.strip_suffix(".webp")?;
    let (account, id) = stem.split_once('_')?;
    Some((account.parse().ok()?, id.parse().ok()?))
}

/// The emoji a sticker stands for, which is also what it is called.
fn emoji_of(doc: &tl::types::Document) -> String {
    doc.attributes
        .iter()
        .find_map(|a| match a {
            tl::enums::DocumentAttribute::Sticker(s) => Some(s.alt.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// A listed document as the picker's cell, its picture named in the cache and fetched when drawn.
fn entry(account: i64, pack: &str, doc: &tl::types::Document) -> Value {
    let emoji = emoji_of(doc);
    let url = format!("file://{}", crate::media_cache::dir(MEDIA).join(name(account, doc.id)).display());
    json!({ "id": doc.id.to_string(), "name": if emoji.is_empty() { "Sticker".to_string() } else { emoji.clone() }, "pack": pack, "body": emoji, "url": url })
}

fn documents(list: Vec<tl::enums::Document>) -> Vec<tl::types::Document> {
    list.into_iter()
        .filter_map(|d| match d {
            tl::enums::Document::Document(d) => Some(d),
            _ => None,
        })
        .collect()
}

/// Adds a heading's stickers to the list and remembers the documents.
fn push(out: &mut Vec<Value>, account: i64, pack: &str, docs: Vec<tl::types::Document>) {
    let mut held = known().lock().unwrap_or_else(|e| e.into_inner());
    for doc in docs {
        // A sticker can be in the recent list and in its pack; the picker shows it under each heading, since the
        // heading is how it is found, but it is remembered and fetched once.
        out.push(entry(account, pack, &doc));
        held.insert((account, doc.id), doc);
    }
}

/// One installed pack's stickers, from what was kept or from Telegram.
async fn pack_documents(handle: Handle, account: i64, id: i64, access_hash: i64) -> Result<Vec<tl::types::Document>> {
    let key = (account, id);
    if let Some((when, docs)) = packs().lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if when.elapsed() < KEEP {
            return Ok(docs.clone());
        }
    }
    let answer = handle
        .client
        .invoke(&tl::functions::messages::GetStickerSet { stickerset: tl::enums::InputStickerSet::Id(tl::types::InputStickerSetId { id, access_hash }), hash: 0 })
        .await
        .map_err(|e| anyhow::anyhow!("{}", explain(&e)))?;
    let docs = match answer {
        tl::enums::messages::StickerSet::Set(s) => documents(s.documents),
        tl::enums::messages::StickerSet::NotModified => Vec::new(),
    };
    packs().lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), docs.clone()));
    Ok(docs)
}

/// What the picker shows: the recent stickers, the favourites, then the installed packs in the account's order.
pub async fn list(state: &AppState, buffer: &Buffer) -> Result<Value, String> {
    let (handle, _) = peer_for(state, buffer).await?;
    let account = handle.own.user_id;
    let mut out = Vec::new();

    // The two short lists are a nicety: a failure reading them leaves the packs to be listed.
    match handle.client.invoke(&tl::functions::messages::GetRecentStickers { attached: false, hash: 0 }).await {
        Ok(tl::enums::messages::RecentStickers::Stickers(r)) => {
            let docs: Vec<_> = documents(r.stickers).into_iter().take(RECENT).collect();
            push(&mut out, account, "Recent", docs);
        }
        Ok(_) => {}
        Err(e) => tracing::debug!("telegram: recent stickers: {}", explain(&e)),
    }
    match handle.client.invoke(&tl::functions::messages::GetFavedStickers { hash: 0 }).await {
        Ok(tl::enums::messages::FavedStickers::Stickers(f)) => push(&mut out, account, "Favourites", documents(f.stickers)),
        Ok(_) => {}
        Err(e) => tracing::debug!("telegram: favourite stickers: {}", explain(&e)),
    }

    let sets = match handle.client.invoke(&tl::functions::messages::GetAllStickers { hash: 0 }).await.map_err(|e| explain(&e))? {
        tl::enums::messages::AllStickers::Stickers(s) => s.sets,
        tl::enums::messages::AllStickers::NotModified => Vec::new(),
    };
    let sets: Vec<tl::types::StickerSet> = sets
        .into_iter()
        .map(|s| {
            let tl::enums::StickerSet::Set(s) = s;
            s
        })
        // Masks are for putting on photos and custom emoji are typed into text: neither is sent as a sticker.
        .filter(|s| !s.masks && !s.emojis && !s.archived)
        .take(PACKS)
        .collect();
    let fetched: Vec<_> = futures::stream::iter(sets.into_iter().map(|set| {
        let handle = handle.clone();
        async move { (set.title, pack_documents(handle, account, set.id, set.access_hash).await) }
    }))
    .buffered(AT_ONCE)
    .collect()
    .await;
    for (title, result) in fetched {
        match result {
            Ok(docs) => push(&mut out, account, &title, docs),
            Err(e) => tracing::debug!("telegram: sticker pack {title}: {e}"),
        }
    }
    Ok(Value::Array(out))
}

/// Sends a sticker that was listed, to a conversation.
pub async fn send(state: &AppState, buffer: &Buffer, id: &str) -> Result<(), String> {
    let id: i64 = id.parse().map_err(|_| "that is not a sticker".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let doc = known().lock().unwrap_or_else(|e| e.into_inner()).get(&(handle.own.user_id, id)).cloned().ok_or_else(|| "That sticker is no longer listed: open the picker again".to_string())?;
    let media = wrapper(&doc).to_raw_input_media();
    // In a topic of a group the sticker answers the topic's first message, which is how it is sent there.
    let reply = super::topics::topic_of_buffer(buffer);
    let sent = handle.client.send_message(peer, InputMessage::new().media(tl::enums::InputMedia::Document(media)).reply_to(reply)).await.map_err(|e| explain(&e))?;
    messages::record(state, &buffer.account_id, &handle.own, &sent);
    Ok(())
}

/// Fetches a listed sticker's picture again, for the window asking for it.
pub async fn restore(state: &AppState, file: &str, path: &Path) -> Result<Option<PathBuf>> {
    let Some((account, id)) = parse_name(file) else { return Ok(None) };
    let handle = state.runtime.telegram_handle(&format!("telegram:{account}")).context("that Telegram account is not connected")?;
    let doc = known().lock().unwrap_or_else(|e| e.into_inner()).get(&(account, id)).cloned().context("that sticker is no longer listed")?;
    let wrapped = wrapper(&doc);
    // A still sticker is its own picture; everything else is drawn from the preview that comes with it.
    if wrapped.thumbs().is_empty() && doc.mime_type == "image/webp" {
        download_to(&handle.client, &wrapped, path, false).await?;
    } else {
        download_thumb(&handle.client, &wrapped, path).await?;
    }
    Ok(Some(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sticker(id: i64, alt: &str) -> tl::types::Document {
        tl::types::Document {
            id,
            access_hash: 9,
            file_reference: vec![1],
            date: 0,
            mime_type: "image/webp".into(),
            size: 10,
            thumbs: None,
            video_thumbs: None,
            dc_id: 2,
            attributes: vec![tl::enums::DocumentAttribute::Sticker(tl::types::DocumentAttributeSticker {
                mask: false,
                alt: alt.into(),
                stickerset: tl::enums::InputStickerSet::Empty,
                mask_coords: None,
            })],
        }
    }

    #[test]
    fn a_stickers_picture_is_named_and_read_back() {
        let file = name(1234567890, 5_000_000_000_001);
        assert_eq!(file, "tstk_1234567890_5000000000001.webp");
        assert_eq!(parse_name(&file), Some((1234567890, 5_000_000_000_001)));
        assert_eq!(parse_name("tgif_1_2_full.mp4"), None);
        assert_eq!(parse_name("tstk_1_x.webp"), None);
    }

    #[test]
    fn a_sticker_is_called_by_its_emoji() {
        let cell = entry(5, "Cats", &sticker(77, "😺"));
        assert_eq!(cell["id"], "77");
        assert_eq!(cell["name"], "😺");
        assert_eq!(cell["body"], "😺");
        assert_eq!(cell["pack"], "Cats");
        assert!(cell["url"].as_str().unwrap().ends_with("tstk_5_77.webp"));
        // One without an emoji is still a cell that can be told from a missing one.
        assert_eq!(entry(5, "Cats", &sticker(78, ""))["name"], "Sticker");
    }

    #[test]
    fn a_sticker_listed_twice_is_remembered_once_and_shown_under_each_heading() {
        let mut out = Vec::new();
            push(&mut out, 424242, "Recent", vec![sticker(1, "a")]);
        push(&mut out, 424242, "Cats", vec![sticker(1, "a"), sticker(2, "b")]);
        assert_eq!(out.len(), 3);
        assert!(known().lock().unwrap().contains_key(&(424242, 2)));
    }
}
