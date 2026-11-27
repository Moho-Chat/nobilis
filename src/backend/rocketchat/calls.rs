//! Rocket.Chat Audio/Video Calls & Group Conferencing:
//! - Provider capabilities (`GET /api/v1/video-conference.capabilities`: Jitsi, BigBlueButton, Google Meet, Pexip, native WebRTC)
//! - Start, join, cancel, info (`video-conference.start`, `join`, `cancel`, `info`)
//! - Native 1:1 WebRTC SDP/ICE signaling via DDP `stream-notify-user` (`{targetUserId}/webrtc`)

use super::ddp;
use super::Handle;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Detect server video-conferencing provider (`GET /api/v1/video-conference.capabilities`).
pub async fn capabilities(handle: &Handle) -> Result<Value> {
    match handle.api.get("/api/v1/video-conference.capabilities", &[]).await {
        Ok(v) => Ok(v),
        Err(_) => Ok(json!({
            "providerName": if handle.server.webrtc_enabled { "webrtc" } else { "none" },
            "capabilities": {
                "mic": handle.server.webrtc_enabled,
                "cam": handle.server.webrtc_enabled,
                "title": false,
            }
        })),
    }
}

/// Start a video conference in a room (`POST /api/v1/video-conference.start`).
pub async fn start(handle: &Handle, room_id: &str, title: Option<&str>, allow_ringing: bool) -> Result<Value> {
    let mut body = json!({ "roomId": room_id, "allowRinging": allow_ringing });
    if let Some(t) = title.filter(|s| !s.is_empty()) {
        body["title"] = json!(t);
    }
    Ok(handle.api.post("/api/v1/video-conference.start", &body).await?)
}

/// Join an active video conference (`POST /api/v1/video-conference.join`).
pub async fn join(handle: &Handle, call_id: &str, mic: bool, cam: bool) -> Result<Value> {
    Ok(handle
        .api
        .post(
            "/api/v1/video-conference.join",
            &json!({ "callId": call_id, "state": { "mic": mic, "cam": cam } }),
        )
        .await?)
}

/// Cancel / decline a ringing video conference (`POST /api/v1/video-conference.cancel`).
pub async fn cancel(handle: &Handle, call_id: &str) -> Result<Value> {
    Ok(handle
        .api
        .post("/api/v1/video-conference.cancel", &json!({ "callId": call_id }))
        .await?)
}

/// Fetch video conference info & participants (`GET /api/v1/video-conference.info`).
pub async fn info(handle: &Handle, call_id: &str) -> Result<Value> {
    Ok(handle
        .api
        .get("/api/v1/video-conference.info", &[("callId", call_id.to_string())])
        .await?)
}

/// Send a native 1:1 WebRTC signaling message (`offer`, `answer`, `candidate`, `call`, `join`, `hangup`)
/// over DDP `stream-notify-user` (`{targetUserId}/webrtc`).
pub fn send_webrtc_signal(handle: &Handle, target_user_id: &str, signal_type: &str, payload: Value) -> Result<()> {
    let id = ddp::next_id("webrtc");
    let frame = ddp::method_frame(
        &id,
        "stream-notify-user",
        json!([format!("{target_user_id}/webrtc"), signal_type, payload]),
    );
    handle
        .ddp
        .send(frame)
        .map_err(|e| anyhow!("failed to send Rocket.Chat WebRTC signal: {e}"))
}
