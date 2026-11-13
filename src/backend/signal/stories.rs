//! Stories: pictures, videos and text people post for a day. They are drawn in one conversation per account, called
//! Stories, each as a message from whoever posted it, and taken away 24 hours after it was posted. A reply or a
//! reaction made on one there goes to its author as Signal's clients send it: a message to them that names the story.
//!
//! Posting a story is not built: it goes to "My Story" or a custom list, which live in the storage service's
//! distribution lists, and those are not written from here.

use super::ids;
use super::people::{self, Ctx};
use crate::model::Buffer;
use presage::libsignal_service::{prelude::ProtobufMessage, protocol::ServiceId};
use presage::proto::{AttachmentPointer, StoryMessage};
use presage::store::Thread;
use uuid::Uuid;

/// What the Stories conversation is called in the account's list, and its id among conversations.
pub const NAME: &str = "Stories";
pub const REMOTE: &str = "stories";

/// How long a story is up.
pub const LIFE_SECONDS: u32 = 24 * 60 * 60;

/// The Stories conversation, made if it is not there yet.
pub fn buffer(ctx: &Ctx<'_>) -> Buffer {
    if let Some(found) = ctx.state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id && b.remote_id.as_deref() == Some(REMOTE)) {
        return found;
    }
    let buffer = ctx.state.runtime.ensure_buffer(ctx.state, ctx.account_id, NAME, "dm");
    ctx.state.runtime.set_buffer_remote_id(ctx.state, &buffer.id, REMOTE);
    ctx.state.runtime.get_buffer(&buffer.id).unwrap_or(buffer)
}

fn pointer_key(ts: u64, author: &Uuid) -> String {
    format!("story:{}", ids::message_id(ts, author))
}

/// The picture or video of a story, kept by moho because the library does not keep stories.
pub fn pointer(store: &super::store::SignalStore, ts: u64, author: &Uuid) -> Option<AttachmentPointer> {
    let bytes = store.blob(&pointer_key(ts, author)).ok().flatten()?;
    AttachmentPointer::decode(bytes.as_slice()).ok()
}

/// The words a story shows as a line: its text, and who it was posted to when that was a group.
pub fn words(story: &StoryMessage, group: Option<&str>, text: Option<&str>) -> String {
    use presage::proto::story_message::Attachment;
    let mut parts = Vec::new();
    if let Some(group) = group {
        parts.push(format!("[Story in {group}]"));
    } else {
        parts.push("[Story]".to_string());
    }
    match &story.attachment {
        Some(Attachment::TextAttachment(t)) => {
            let body = t.text.clone().unwrap_or_default();
            if !body.trim().is_empty() {
                parts.push(body);
            }
            if let Some(url) = t.preview.as_ref().and_then(|p| p.url.clone()) {
                parts.push(url);
            }
        }
        Some(Attachment::FileAttachment(p)) => {
            if let Some(caption) = p.caption.clone().filter(|c| !c.trim().is_empty()) {
                parts.push(caption);
            }
        }
        None => {}
    }
    if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
        parts.push(text.to_string());
    }
    if story.allows_replies == Some(false) {
        parts.push("(replies are off)".to_string());
    }
    parts.join(" ")
}

/// A story posted by `author` at `at` (ms).
pub async fn arrived(ctx: &Ctx<'_>, author: Uuid, at: u64, story: &StoryMessage) {
    use presage::proto::story_message::Attachment;
    // A story older than a day is not news (a device that was away for a while catches up on it).
    let now = super::commands::now_ms();
    if now.saturating_sub(at) > u64::from(LIFE_SECONDS) * 1000 {
        return;
    }
    let group = match story.group.as_ref().and_then(|g| g.master_key.as_deref()).and_then(|k| <[u8; 32]>::try_from(k).ok()) {
        Some(key) => Some(people::group_title(ctx, &key).await),
        None => None,
    };
    let buffer = buffer(ctx);
    let id = ids::message_id(at, &author);
    let from = people::person(ctx, &author).await;
    let thread = Thread::Contact(ServiceId::Aci(author.into()));
    let attachments = match &story.attachment {
        Some(Attachment::FileAttachment(pointer)) => {
            let _ = ctx.store.set_blob(&pointer_key(at, &author), &pointer.encode_to_vec());
            super::media::attachments_of(ctx.account_id, &thread, &author, at, std::slice::from_ref(pointer))
        }
        _ => Vec::new(),
    };
    let body = words(story, group.as_deref(), None);
    let recorded = ctx.state.runtime.record_message_at(
        ctx.state,
        ctx.account_id,
        &buffer.name,
        &buffer.kind,
        &from,
        &body,
        false,
        "chat",
        None,
        Some(id.clone()),
        false,
        super::media::avatar_of(ctx.account_id, &author),
        Vec::new(),
        attachments,
        Some(author.to_string()),
        Some((at / 1000) as i64),
        None,
        None,
    );
    if recorded {
        // Gone a day after it was posted, from the window and from moho's own copy of its picture.
        super::expiry::from_sent(ctx, &buffer.id, &thread, &id, at, LIFE_SECONDS);
    }
}

/// The story a message in the Stories conversation is, as a reply names it.
pub fn context_of(message_id: &str) -> Option<(Thread, presage::proto::data_message::StoryContext)> {
    let (ts, author) = ids::parse_message_id(message_id)?;
    let context = presage::proto::data_message::StoryContext { author_aci: Some(author.to_string()), sent_timestamp: Some(ts), author_aci_binary: Some(author.as_bytes().to_vec()) };
    Some((Thread::Contact(ServiceId::Aci(author.into())), context))
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::proto::{story_message::Attachment, TextAttachment};

    #[test]
    fn a_story_is_said_with_its_text_where_it_was_posted_and_whether_it_can_be_answered() {
        let text = StoryMessage { attachment: Some(Attachment::TextAttachment(TextAttachment { text: Some("Beach day".into()), ..Default::default() })), allows_replies: Some(false), ..Default::default() };
        assert_eq!(words(&text, None, None), "[Story] Beach day (replies are off)");
        let picture = StoryMessage { attachment: Some(Attachment::FileAttachment(AttachmentPointer { caption: Some("sunset".into()), ..Default::default() })), allows_replies: Some(true), ..Default::default() };
        assert_eq!(words(&picture, Some("Book club"), None), "[Story in Book club] sunset");
    }

    #[test]
    fn a_reply_names_the_story_by_its_author_and_time() {
        let author = Uuid::from_u128(5);
        let (thread, context) = context_of(&ids::message_id(1_700_000_000_000, &author)).unwrap();
        assert!(matches!(thread, Thread::Contact(ServiceId::Aci(a)) if Uuid::from(a) == author));
        assert_eq!((context.sent_timestamp, context.author_aci.as_deref()), (Some(1_700_000_000_000), Some(author.to_string().as_str())));
    }

    #[test]
    fn a_kept_picture_comes_back() {
        let store = super::super::store::SignalStore::open_in_memory(presage::model::identity::OnNewIdentity::Trust).unwrap();
        let author = Uuid::from_u128(9);
        let pointer = AttachmentPointer { content_type: Some("image/jpeg".into()), size: Some(10), ..Default::default() };
        store.set_blob(&pointer_key(42, &author), &pointer.encode_to_vec()).unwrap();
        assert_eq!(super::pointer(&store, 42, &author), Some(pointer));
        assert_eq!(super::pointer(&store, 43, &author), None);
    }
}
