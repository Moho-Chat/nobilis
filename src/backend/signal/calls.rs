//! Calls as far as moho can take part in them: it rings, and says what became of the call.
//!
//! The call itself (its keys, its media) is RingRTC's, Signal's own WebRTC, carried in parts of these messages no other
//! code can read; moho cannot answer one. What can be read is the offer (who, voice or video, its id), and the hangups
//! and busy signals that end it. So an incoming call rings the window (answered on the phone), stops ringing when the
//! caller gives up or one of the account's devices picks up or turns it down, and a call nobody answered is said as a
//! missed call - under the same id as the phone's own call log line for it, so the two never show twice.

use super::people::{self, Ctx};
use crate::state::AppState;
use presage::libsignal_service::protocol::ServiceId;
use presage::proto::call_message::{hangup, offer};
use presage::proto::CallMessage;
use presage::store::Thread;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

/// How long a call rings before it is taken for missed, as Signal's clients give up on one.
pub const RINGS_FOR_MS: u64 = 60_000;

#[derive(Clone)]
struct Ring {
    account_id: String,
    buffer_id: String,
    video: bool,
    started_ms: u64,
}

/// The calls ringing now, by call id.
fn ringing() -> &'static Mutex<HashMap<u64, Ring>> {
    static RINGING: OnceLock<Mutex<HashMap<u64, Ring>>> = OnceLock::new();
    RINGING.get_or_init(Default::default)
}

fn tell(state: &AppState, ring: &Ring, call_id: u64, on: bool) {
    state.events.emit(
        "incomingCall",
        serde_json::json!({
            "accountId": ring.account_id,
            "bufferId": ring.buffer_id,
            "channelId": call_id.to_string(),
            "ringing": on,
            "video": ring.video,
            // Nothing here can take the call: the window offers to silence it, and says where it is answered.
            "answerElsewhere": true,
        }),
    );
}

/// The missed-call line, as the phone's call log writes it (see `inbox::call_logged`), so whichever comes first is kept.
fn say_missed(state: &AppState, ring: &Ring, call_id: u64) {
    use presage::proto::sync_message::call_event::{Direction, Event, Type};
    let kind = if ring.video { Type::VideoCall } else { Type::AudioCall };
    let Some(words) = super::inbox::call_words(kind as i32, Direction::Incoming as i32, Event::NotAccepted as i32) else { return };
    let Some(buffer) = state.runtime.get_buffer(&ring.buffer_id) else { return };
    let id = format!("call-{call_id}-{}", Event::NotAccepted as i32);
    state.runtime.record_message_at(
        state, &ring.account_id, &buffer.name, &buffer.kind, "Signal", &format!("{words}. Calls are answered in Signal on your phone"), false, "system", None, Some(id), false, None, Vec::new(), Vec::new(), None, Some((ring.started_ms / 1000) as i64), None, None,
    );
}

/// Stops a call ringing. `missed` says it as missed.
fn stop(state: &AppState, call_id: u64, missed: bool) {
    let Some(ring) = ringing().lock().unwrap_or_else(|e| e.into_inner()).remove(&call_id) else { return };
    tell(state, &ring, call_id, false);
    if missed {
        say_missed(state, &ring, call_id);
    }
}

/// Silences a ringing call here (the window's button). Nothing is sent: the caller still hears it ring, and the phone
/// still rings, as when a ringing device is put face down.
pub fn silence(state: &AppState, buffer_id: &str) -> bool {
    let id = ringing().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(_, r)| r.buffer_id == buffer_id).map(|(id, _)| *id);
    match id {
        Some(id) => {
            stop(state, id, false);
            true
        }
        None => false,
    }
}

/// A call message from `sender`, as the server stamped it at `server_ms`.
pub async fn arrived(ctx: &Ctx<'_>, sender: Uuid, server_ms: u64, call: &CallMessage) {
    if let Some(offer) = &call.offer {
        if ctx.is_own(&sender) {
            return;
        }
        let Some(call_id) = offer.id else { return };
        // An offer that waited on the server while moho was away is a call already over.
        if super::commands::now_ms().saturating_sub(server_ms) > RINGS_FOR_MS {
            return;
        }
        let thread = Thread::Contact(ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(sender)));
        let buffer = people::buffer_for(ctx, &thread).await;
        let video = offer.r#type == Some(offer::Type::OfferVideoCall as i32);
        let ring = Ring { account_id: ctx.account_id.to_string(), buffer_id: buffer.id.clone(), video, started_ms: server_ms };
        ringing().lock().unwrap_or_else(|e| e.into_inner()).insert(call_id, ring.clone());
        tell(ctx.state, &ring, call_id, true);
        tracing::info!("signal[{}]: a {} call is ringing", ctx.account_id, if video { "video" } else { "voice" });
        // Nobody answered in time: missed, unless something else ended it first.
        let state = ctx.state.clone();
        let wait = RINGS_FOR_MS.saturating_sub(super::commands::now_ms().saturating_sub(server_ms));
        tokio::task::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            stop(&state, call_id, true);
        });
        return;
    }
    if let Some(hangup) = &call.hangup {
        let Some(call_id) = hangup.id else { return };
        // From another of the account's devices: picked up or turned down there; its call log says which.
        if ctx.is_own(&sender) {
            stop(ctx.state, call_id, false);
            return;
        }
        // From the caller: given up before anybody answered.
        let accepted = hangup.r#type == Some(hangup::Type::HangupAccepted as i32);
        stop(ctx.state, call_id, !accepted);
        return;
    }
    // The account's other device was busy: it is not ringing there. Answers and connection details are RingRTC's, for a
    // device in the call.
    if let Some(call_id) = call.busy.as_ref().and_then(|b| b.id) {
        stop(ctx.state, call_id, false);
    }
}

#[cfg(test)]
pub(super) fn is_ringing(call_id: u64) -> bool {
    ringing().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&call_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::proto::call_message::{Busy, Hangup, Offer};

    #[tokio::test]
    async fn a_call_rings_and_a_caller_who_gives_up_leaves_a_missed_call() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (state, store) = super::super::inbox::tests::world_for_calls("calls").await;
                let me = Uuid::parse_str(super::super::inbox::tests::ME_FOR_CALLS).unwrap();
                let ctx = Ctx::new(&state, super::super::inbox::tests::ACCOUNT_FOR_CALLS, store, me);
                let alice = Uuid::from_u128(2);
                let mut events = state.events.subscribe();
                let now = super::super::commands::now_ms();
                let offer = CallMessage { offer: Some(Offer { id: Some(77), r#type: Some(offer::Type::OfferVideoCall as i32), ..Default::default() }), ..Default::default() };
                arrived(&ctx, alice, now, &offer).await;
                assert!(is_ringing(77));
                let rang = std::iter::from_fn(|| events.try_recv().ok()).find(|e| e.name == "incomingCall").expect("a ring");
                assert_eq!((rang.data["ringing"].as_bool(), rang.data["video"].as_bool()), (Some(true), Some(true)));
                // The caller hangs up: no longer ringing, and said as missed.
                let hangup = CallMessage { hangup: Some(Hangup { id: Some(77), r#type: Some(hangup::Type::HangupNormal as i32), ..Default::default() }), ..Default::default() };
                arrived(&ctx, alice, now, &hangup).await;
                assert!(!is_ringing(77));
                let buffer = state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id).unwrap();
                let rows = state.store.get_backlog(&buffer.id, i64::MAX, 10).unwrap();
                assert!(rows.iter().any(|m| m.body.starts_with("Missed video call")), "{:?}", rows.iter().map(|m| &m.body).collect::<Vec<_>>());
                // An offer from long ago does not ring; one taken on another device stops without a missed line.
                arrived(&ctx, alice, now - RINGS_FOR_MS - 1, &CallMessage { offer: Some(Offer { id: Some(78), ..Default::default() }), ..Default::default() }).await;
                assert!(!is_ringing(78));
                arrived(&ctx, alice, now, &CallMessage { offer: Some(Offer { id: Some(79), ..Default::default() }), ..Default::default() }).await;
                arrived(&ctx, me, now, &CallMessage { hangup: Some(Hangup { id: Some(79), r#type: Some(hangup::Type::HangupAccepted as i32), ..Default::default() }), ..Default::default() }).await;
                assert!(!is_ringing(79));
                arrived(&ctx, alice, now, &CallMessage { offer: Some(Offer { id: Some(80), ..Default::default() }), ..Default::default() }).await;
                assert!(silence(&state, &buffer.id));
                assert!(!is_ringing(80));
                let _ = CallMessage { busy: Some(Busy { id: Some(1) }), ..Default::default() };
            })
            .await;
    }
}
