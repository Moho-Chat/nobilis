//! Sticker packs added from moho: from a sticker somebody sent (its message offers the pack while it is not installed),
//! or from a `signal.art/addstickers` link. The library downloads the pack and tells the account's other devices, as
//! Signal's apps do, so it is on the phone too; a pack can be taken off the same way.

use super::people::Ctx;
use super::{Command, Handle};
use crate::model::Component;
use crate::state::AppState;

/// The id of the button on a sticker whose pack is not installed.
pub const BUTTON: &str = "signal:stickerpack";

fn pack_key(msg_id: &str) -> String {
    format!("sticker-pack:{msg_id}")
}

/// A pack's id and key from a `https://signal.art/addstickers/#pack_id=<hex>&pack_key=<hex>` link.
pub fn parse_link(link: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let fragment = link.trim().split_once('#')?.1;
    let mut id = None;
    let mut key = None;
    for pair in fragment.split('&') {
        match pair.split_once('=')? {
            ("pack_id", v) => id = hex(v),
            ("pack_key", v) => key = hex(v),
            _ => {}
        }
    }
    Some((id.filter(|i| i.len() == 16)?, key.filter(|k| k.len() == 32)?))
}

fn hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

/// A sticker arrived in message `msg_id`: if its pack is not installed, the message offers it.
pub async fn offer(ctx: &Ctx<'_>, buffer_id: &str, msg_id: &str, sticker: &presage::proto::data_message::Sticker) {
    use presage::store::ContentsStore;
    let (Some(id), Some(key)) = (sticker.pack_id.as_ref(), sticker.pack_key.as_ref()) else { return };
    if ctx.store.sticker_pack(id).await.ok().flatten().is_some() {
        return;
    }
    let mut kept = id.clone();
    kept.extend_from_slice(key);
    let _ = ctx.store.set_blob(&pack_key(msg_id), &kept);
    let components = vec![Component {
        kind: "button".to_string(),
        custom_id: Some(BUTTON.to_string()),
        label: Some("Add this sticker pack".to_string()),
        style: Some("secondary".to_string()),
        url: None,
        disabled: false,
        emoji: None,
        options: Vec::new(),
        placeholder: None,
        row: 0,
    }];
    if ctx.state.store.set_components(buffer_id, msg_id, &components).is_ok() {
        ctx.state.events.emit("messageComponents", serde_json::json!({ "bufferId": buffer_id, "messageId": msg_id, "components": components }));
    }
}

async fn ask(handle: &Handle, id: Vec<u8>, key: Vec<u8>, install: bool) -> Result<(), String> {
    let (done, answer) = tokio::sync::oneshot::channel();
    handle.commands.send(Command::StickerPack { id, key, install, done }).map_err(|_| "that Signal account is not connected".to_string())?;
    match tokio::time::timeout(std::time::Duration::from_secs(120), answer).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("the connection ended before Signal answered".to_string()),
        Err(_) => Err("Signal did not answer in time".to_string()),
    }
}

/// Installs the pack a sticker message offered, and takes the offer off the message.
pub async fn install_from_message(state: &AppState, buffer_id: &str, msg_id: &str) -> Result<(), String> {
    let buffer = state.runtime.get_buffer(buffer_id).ok_or("no such conversation")?;
    let handle = state.runtime.signal_handle(&buffer.account_id).ok_or("that Signal account is not connected")?;
    let kept = handle.store.blob(&pack_key(msg_id)).ok().flatten().filter(|k| k.len() == 48).ok_or("That sticker's pack is not known")?;
    let (id, key) = kept.split_at(16);
    ask(&handle, id.to_vec(), key.to_vec(), true).await?;
    let _ = handle.store.set_blob(&pack_key(msg_id), &[]);
    if state.store.clear_components(buffer_id, msg_id).is_ok() {
        state.events.emit("messageComponents", serde_json::json!({ "bufferId": buffer_id, "messageId": msg_id, "components": [] }));
    }
    Ok(())
}

/// Installs (or removes) the pack a `signal.art/addstickers` link names, or a pack by its id (hex) for removing.
pub async fn from_link(state: &AppState, account_id: &str, link: &str, install: bool) -> Result<(), String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let (id, key) = parse_link(link).ok_or("That is not a Signal sticker pack link (signal.art/addstickers/#pack_id=…&pack_key=…)")?;
    ask(&handle, id, key, install).await
}

/// Removes an installed pack, by its id (hex).
pub async fn remove(state: &AppState, account_id: &str, pack_id_hex: &str) -> Result<(), String> {
    use presage::store::ContentsStore;
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let id = hex(pack_id_hex).ok_or("that is not a sticker pack")?;
    let pack = handle.store.sticker_pack(&id).await.ok().flatten().ok_or("that sticker pack is not installed")?;
    ask(&handle, pack.id.clone(), pack.key.clone(), false).await
}

/// The installed packs: id (hex), title, how many stickers.
pub async fn list(state: &AppState, account_id: &str) -> Result<serde_json::Value, String> {
    use presage::store::ContentsStore;
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let packs = handle.store.sticker_packs().await.map_err(|e| e.to_string())?;
    Ok(serde_json::Value::Array(
        packs
            .filter_map(Result::ok)
            .map(|p| serde_json::json!({ "packId": super::ids::hex(&p.id), "title": p.manifest.title, "count": p.manifest.stickers.len() }))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sticker_link_gives_the_pack() {
        let link = format!("https://signal.art/addstickers/#pack_id={}&pack_key={}", "ab".repeat(16), "cd".repeat(32));
        let (id, key) = parse_link(&link).unwrap();
        assert_eq!((id.len(), key.len(), id[0], key[0]), (16, 32, 0xab, 0xcd));
        assert!(parse_link("https://signal.art/addstickers/#pack_id=ab").is_none());
        assert!(parse_link("https://example.org/").is_none());
    }
}
