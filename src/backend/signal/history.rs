//! The message history a phone can hand a newly linked device ("Transfer message history" when linking).
//!
//! The link says moho can take it (`capabilities` in the link address); a phone that offers it gives the new device a
//! key with the link, uploads its history as a backup archive, and says where (`/v1/devices/transfer_archive`). The
//! archive is Signal's backup format: libsignal's own reader checks it and takes off its encryption and compression,
//! and its frames - who each recipient is, which chat is whose, and each message - are read here and written into the
//! conversations under the ids a live message has, so nothing is shown twice. Text is brought over; pictures and files
//! are named, not fetched (their copies are the phone's to keep).

use super::people::{self, Ctx};
use super::ids;
use libsignal_message_backup::proto::backup::{chat_item, frame, recipient, BackupInfo, Frame};
use presage::libsignal_service::protocol::{Aci, ServiceId};
use presage::store::Thread;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Where the link's key waits for the archive, until it is imported (or given up on).
pub const KEY_BLOB: &str = "link-history-key";

/// Who a recipient in the archive is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Who {
    Person(Uuid),
    Group([u8; 32]),
    Me,
}

/// One message to bring over.
#[derive(Debug, Clone, PartialEq)]
pub struct Old {
    thread: Who,
    author: Who,
    sent_ms: u64,
    body: String,
}

/// Takes off the archive's encryption and compression (libsignal's reader, which checks it too) and reads its frames.
pub async fn read(archive: &[u8], key: &[u8; 32], own: &Uuid) -> Result<Vec<Frame>, String> {
    use futures::io::AsyncReadExt;
    use libsignal_account_keys::BackupKey;
    use libsignal_message_backup::frame::{CursorFactory, FramesReader, VerifyHmac};
    use libsignal_message_backup::key::MessageBackupKey;
    let backup_key: BackupKey = BackupKey(*key);
    let backup_id = backup_key.derive_backup_id(&Aci::from(*own));
    let message_key = MessageBackupKey::derive(&backup_key, &backup_id, None);
    let mut reader = FramesReader::new(&message_key, CursorFactory::new(&archive)).await.map_err(|e| format!("the history could not be opened: {e}"))?;
    let mut plain = Vec::new();
    reader.read_to_end(&mut plain).await.map_err(|e| format!("the history could not be read: {e}"))?;
    reader.verify_hmac().await.map_err(|e| format!("the history did not check out: {e}"))?;
    frames(&plain)
}

/// The frames of a decrypted archive: a `BackupInfo`, then each frame, each with its length before it.
pub fn frames(plain: &[u8]) -> Result<Vec<Frame>, String> {
    use protobuf::Message;
    let mut at = 0;
    let mut next = || -> Result<Option<&[u8]>, String> {
        if at >= plain.len() {
            return Ok(None);
        }
        let mut len = 0u64;
        let mut shift = 0;
        loop {
            let b = *plain.get(at).ok_or("the history ends inside a frame")?;
            at += 1;
            len |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return Err("the history is not in frames".to_string());
            }
        }
        let end = at.checked_add(len as usize).filter(|&e| e <= plain.len()).ok_or("the history ends inside a frame")?;
        let bytes = &plain[at..end];
        at = end;
        Ok(Some(bytes))
    };
    let info = next()?.ok_or("the history is empty")?;
    BackupInfo::parse_from_bytes(info).map_err(|_| "the history does not start as a backup does".to_string())?;
    let mut out = Vec::new();
    while let Some(bytes) = next()? {
        // A frame of a kind this copy does not know is passed over, as Signal's apps do.
        if let Ok(frame) = Frame::parse_from_bytes(bytes) {
            out.push(frame);
        }
    }
    Ok(out)
}

/// The messages the frames hold, in the order they are in.
pub fn messages(frames: &[Frame]) -> Vec<Old> {
    let mut recipients: HashMap<u64, Who> = HashMap::new();
    let mut chats: HashMap<u64, Who> = HashMap::new();
    let mut out = Vec::new();
    for f in frames {
        match &f.item {
            Some(frame::Item::Recipient(r)) => {
                let who = match &r.destination {
                    Some(recipient::Destination::Contact(c)) => c.aci.as_deref().and_then(|a| Uuid::from_slice(a).ok()).map(Who::Person),
                    Some(recipient::Destination::Group(g)) => <[u8; 32]>::try_from(g.masterKey.as_slice()).ok().map(Who::Group),
                    Some(recipient::Destination::Self_(_)) => Some(Who::Me),
                    _ => None,
                };
                if let Some(who) = who {
                    recipients.insert(r.id, who);
                }
            }
            Some(frame::Item::Chat(c)) => {
                if let Some(who) = recipients.get(&c.recipientId) {
                    chats.insert(c.id, *who);
                }
            }
            Some(frame::Item::ChatItem(item)) => {
                let (Some(thread), Some(author)) = (chats.get(&item.chatId), recipients.get(&item.authorId)) else { continue };
                let Some(body) = words(item) else { continue };
                out.push(Old { thread: *thread, author: *author, sent_ms: item.dateSent, body });
            }
            _ => {}
        }
    }
    out
}

/// What a message says, as moho draws one of its kind; `None` for what is not a message (group changes, deletions).
fn words(item: &libsignal_message_backup::proto::backup::ChatItem) -> Option<String> {
    use chat_item::Item;
    Some(match item.item.as_ref()? {
        Item::StandardMessage(m) => {
            let text = m.text.as_ref().map(|t| t.body.clone()).unwrap_or_default();
            let files = m.attachments.len();
            let named = match files {
                0 => String::new(),
                1 => "[An attachment, kept on the phone]".to_string(),
                n => format!("[{n} attachments, kept on the phone]"),
            };
            [text, named].into_iter().filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n")
        }
        Item::ContactMessage(_) => "[Contact]".to_string(),
        Item::StickerMessage(_) => "[Sticker]".to_string(),
        Item::ViewOnceMessage(_) => "[View-once media]".to_string(),
        Item::Poll(p) => format!("[Poll: {}]", p.question),
        Item::DirectStoryReplyMessage(_) => "[Reply to a story]".to_string(),
        Item::GiftBadge(_) => "[Gift badge: not supported in moho]".to_string(),
        Item::PaymentNotification(_) => "[Payment: not supported in moho]".to_string(),
        _ => return None,
    })
    .filter(|s: &String| !s.trim().is_empty())
}

/// Writes the messages into their conversations (made where there is none yet) without a notification for any:
/// they are old. Answers how many were new here.
pub async fn import(ctx: &Ctx<'_>, old: &[Old]) -> usize {
    let mut names: HashMap<Uuid, String> = HashMap::new();
    let mut touched = HashSet::new();
    let mut brought = 0;
    for m in old {
        let thread = match m.thread {
            Who::Person(who) => Thread::Contact(ServiceId::Aci(Aci::from(who))),
            Who::Group(key) => Thread::Group(key),
            Who::Me => Thread::Contact(ServiceId::Aci(Aci::from(ctx.own))),
        };
        let author = match m.author {
            Who::Person(who) => who,
            Who::Me => ctx.own,
            Who::Group(_) => continue,
        };
        let buffer = people::buffer_for(ctx, &thread).await;
        let from = match names.get(&author) {
            Some(name) => name.clone(),
            None => {
                let name = people::person(ctx, &author).await;
                names.insert(author, name.clone());
                name
            }
        };
        let id = ids::message_id(m.sent_ms, &author);
        let own = author == ctx.own;
        let fresh = ctx.state.store.append_message(&buffer.id, &id, &from, &m.body, (m.sent_ms / 1000) as i64, false, false, "chat", None, &[], own, None, &[], &[], Some(&author.to_string()), None, &buffer.kind, None, &[]);
        if matches!(fresh, Ok(true)) {
            brought += 1;
            touched.insert(buffer.id.clone());
        }
    }
    for buffer_id in touched {
        ctx.state.runtime.refresh_buffer_activity(ctx.state, &buffer_id);
    }
    brought
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsignal_message_backup::proto::backup::{Chat, ChatItem, Contact, Recipient, Self_, StandardMessage, Text};
    use protobuf::Message;

    fn framed(messages: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for m in messages {
            let mut len = m.len() as u64;
            loop {
                let b = (len & 0x7f) as u8;
                len >>= 7;
                if len == 0 {
                    out.push(b);
                    break;
                }
                out.push(b | 0x80);
            }
            out.extend_from_slice(m);
        }
        out
    }

    #[test]
    fn the_frames_say_who_said_what_where() {
        let alice = Uuid::from_u128(7);
        let frame = |item: frame::Item| Frame { item: Some(item), ..Default::default() }.write_to_bytes().unwrap();
        let contact = Recipient { id: 1, destination: Some(recipient::Destination::Contact(Contact { aci: Some(alice.as_bytes().to_vec()), ..Default::default() })), ..Default::default() };
        let me = Recipient { id: 2, destination: Some(recipient::Destination::Self_(Self_::default())), ..Default::default() };
        let chat = Chat { id: 10, recipientId: 1, ..Default::default() };
        let said = |author: u64, at: u64, body: &str| ChatItem {
            chatId: 10,
            authorId: author,
            dateSent: at,
            item: Some(chat_item::Item::StandardMessage(StandardMessage { text: protobuf::MessageField::some(Text { body: body.into(), ..Default::default() }), ..Default::default() })),
            ..Default::default()
        };
        let plain = framed(&[
            BackupInfo { version: 1, ..Default::default() }.write_to_bytes().unwrap(),
            frame(frame::Item::Recipient(contact)),
            frame(frame::Item::Recipient(me)),
            frame(frame::Item::Chat(chat)),
            frame(frame::Item::ChatItem(said(1, 1_700_000_000_000, "hello"))),
            frame(frame::Item::ChatItem(said(2, 1_700_000_001_000, "hi back"))),
            // A message in a chat nobody named is passed over.
            frame(frame::Item::ChatItem(ChatItem { chatId: 99, ..said(1, 5, "lost") })),
        ]);
        let got = messages(&frames(&plain).unwrap());
        assert_eq!(
            got,
            vec![
                Old { thread: Who::Person(alice), author: Who::Person(alice), sent_ms: 1_700_000_000_000, body: "hello".into() },
                Old { thread: Who::Person(alice), author: Who::Me, sent_ms: 1_700_000_001_000, body: "hi back".into() },
            ]
        );
        assert!(frames(&[0x05, 1, 2]).is_err(), "cut short");
    }
}
