//! Disappearing messages: noting which will go, and taking them away when their time comes.

use super::ids;
use super::people::Ctx;
use super::store::{Expiry, SignalStore};
use crate::state::AppState;
use presage::store::{ContentsStore, Thread};

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Notes a message that was sent (by this account, from here or from its phone): its time runs from when it was sent.
pub fn from_sent(ctx: &Ctx<'_>, buffer_id: &str, thread: &Thread, msg_id: &str, sent_ms: u64, seconds: u32) {
    note(ctx, buffer_id, thread, msg_id, sent_ms, seconds, Some((sent_ms / 1000) as i64 + seconds as i64));
}

/// Notes a message that was received: its time starts when the conversation is looked at.
pub fn from_received(ctx: &Ctx<'_>, buffer_id: &str, thread: &Thread, msg_id: &str, sent_ms: u64, seconds: u32) {
    note(ctx, buffer_id, thread, msg_id, sent_ms, seconds, None);
}

fn note(ctx: &Ctx<'_>, buffer_id: &str, thread: &Thread, msg_id: &str, sent_ms: u64, seconds: u32, deadline: Option<i64>) {
    if seconds == 0 {
        return;
    }
    let row = Expiry { buffer_id: buffer_id.to_string(), msg_id: msg_id.to_string(), thread_remote: ids::remote_id(thread), sent_ms, seconds, deadline };
    if let Err(e) = ctx.store.expiry_add(&row) {
        tracing::warn!("signal[{}]: could not note a disappearing message: {e}", ctx.account_id);
    }
}

/// The conversation was looked at: what was waiting starts to run down.
pub fn started(store: &SignalStore, buffer_id: &str) {
    if let Err(e) = store.expiry_start(buffer_id, now()) {
        tracing::warn!("signal: could not start disappearing timers: {e}");
    }
}

/// Removes what is due, from the conversation and from the account's store.
pub async fn sweep(state: &AppState, store: &SignalStore) -> usize {
    let due = match store.expiry_due(now()) {
        Ok(due) => due,
        Err(e) => {
            tracing::warn!("signal: could not read what is due to disappear: {e}");
            return 0;
        }
    };
    let mut removed = 0;
    for e in due {
        state.runtime.delete_message(state, &e.buffer_id, &e.msg_id);
        if let Some(thread) = ids::parse_remote_id(&e.thread_remote) {
            let mut writer = store.clone();
            let _ = writer.delete_message(&thread, e.sent_ms).await;
        }
        let _ = store.expiry_done(&e.buffer_id, &e.msg_id);
        removed += 1;
    }
    removed
}
