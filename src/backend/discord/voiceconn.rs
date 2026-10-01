//! A Discord voice connection: audio both ways, and other people's cameras.
//!
//! This replaced songbird. Songbird carried every call until the first time
//! somebody turned a camera on: its handshake never says it can receive
//! video, so Discord never sends any, and it discards whatever is not audio.
//! Nothing short of owning the connection fixes that - and the pieces already
//! existed, written for the Go Live stream connection (streamconn.rs), which
//! is the same protocol pointed at a different server.
//!
//! What is here that the stream connection does not need:
//!
//! - **Audio both ways.** Other people's Opus is decoded per speaker, held a
//!   couple of frames in a jitter buffer, mixed and played; the microphone is
//!   encoded every 20ms and sent while somebody is actually talking.
//! - **Who is who.** A packet carries an SSRC and nothing else. The server
//!   names each SSRC's owner in `op 5` (speaking) and `op 12` (video), and
//!   those names are what DAVE decrypts with - a frame from an SSRC nobody
//!   has named cannot be decrypted, and is dropped rather than guessed at.
//! - **DAVE the way songbird does it.** The group is told who is recognised
//!   (op 11 / op 13), a failed commit or welcome rebuilds the session, and
//!   transitions are acknowledged only where the protocol wants them.
//!   These rules are carried over from songbird's `ws.rs` because they are
//!   what kept calls working, not re-derived.
//!
//! Cameras arrive as `discordCameraFrame` events, the same shape as a Go Live
//! viewer's frames: the window has the decoders and the daemon has none.

use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::rtp;
use super::videorx::{self, VideoReceiver};
use super::voicecrypto::{self, Mode, Sealer};

/// Discord's voice gateway version. 8 is what carries the video fields.
const VOICE_VERSION: u8 = 8;

/// DAVE marks every frame it has encrypted with these two bytes at the end.
const DAVE_MAGIC: [u8; 2] = [0xFA, 0xFA];

/// An Opus frame of silence, which Discord asks for five of before a client
/// stops sending - so the far end's decoders fade out instead of freezing on
/// the last syllable.
const SILENT_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];

/// Twenty milliseconds of 48kHz stereo, interleaved.
const FRAME_SAMPLES: usize = rtp::OPUS_FRAME_SAMPLES * 2;

/// How often the UDP session is poked to keep the path open through NAT.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(5);

/// Where to connect, from the two halves the main gateway hands over.
#[derive(Clone, Debug)]
pub struct Handshake {
    /// The guild, or for a call with no guild, the channel.
    pub server_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
}

/// What the connection plays into and reads from.
pub struct Media {
    pub playback: Option<Arc<crate::audio::Playback>>,
    pub tracker: Arc<super::voice::SpeakingTracker>,
    /// The microphone, when this session transmits.
    pub mic: Option<crate::audio::MicSource>,
}

/// Why a connection ended.
#[derive(Debug, Clone)]
pub enum Ended {
    /// Stopped from here: hung up, or replaced by a new connection.
    Stopped,
    /// The server closed it in a way worth trying again - a crashed voice
    /// server, a network drop.
    Retry(String),
    /// The server closed it for good: kicked, the channel gone, the session
    /// or token no longer valid. Trying again would be refused the same way.
    Final(String),
}

/// A running connection. Dropping it does not end it; `stop` does.
pub struct VoiceConn {
    stop: watch::Sender<bool>,
    camera: CameraSender,
}

impl VoiceConn {
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    /// Turns this end's camera on or off, as far as the voice server is
    /// concerned. The gateway's `self_video` is the other half, and the
    /// caller's: that is what tells everybody else's client to draw a tile.
    pub fn set_camera(&self, on: bool, quality: VideoQuality) {
        let sender = &self.camera;
        sender.on.store(on, Ordering::Relaxed);
        let _ = sender.ws.send(WsMessage::Text(announce_camera(sender.audio_ssrc, sender.video_ssrc, on, quality).to_string()));
    }

    /// Whether a frame handed in now would reach anybody: the camera is on,
    /// and the group - where there is one - can be encrypted for.
    pub fn camera_ready(&self) -> bool {
        self.camera.on.load(Ordering::Relaxed) && group_ready(&self.camera.shared)
    }

    /// One encoded camera frame, as however many packets it takes.
    pub async fn send_camera_frame(&self, frame: &[u8], timestamp_micros: i64) -> Result<()> {
        self.camera.send_frame(frame, timestamp_micros).await
    }
}

/// What a picture is sent at - a camera or a stream - which the voice server
/// passes on to everybody deciding what to ask for.
#[derive(Clone, Copy, Debug)]
pub struct VideoQuality {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate: u32,
}

impl Default for VideoQuality {
    /// Discord's own client sends a camera at 720p30 whatever the account,
    /// and 720p30 is a stream's ceiling on an account without Nitro.
    fn default() -> Self {
        VideoQuality { width: 1280, height: 720, framerate: 30, bitrate: 2_500_000 }
    }
}

/// The `op 12` that says a camera is coming, or that it has gone. Turning it
/// off is the same message with the stream inactive and no video SSRC, which
/// is what Discord's own client sends - and what makes the far end drop the
/// tile rather than hold the last frame.
pub fn announce_camera(audio_ssrc: u32, video_ssrc: u32, on: bool, quality: VideoQuality) -> Value {
    json!({
        "op": 12,
        "d": {
            "audio_ssrc": audio_ssrc,
            "video_ssrc": if on { video_ssrc } else { 0 },
            "rtx_ssrc": if on { video_ssrc.wrapping_add(1) } else { 0 },
            "streams": [{
                "type": "video", "rid": "100", "ssrc": video_ssrc, "active": on,
                "quality": 100, "rtx_ssrc": video_ssrc.wrapping_add(1),
                "max_bitrate": quality.bitrate, "max_framerate": quality.framerate,
                "max_resolution": { "type": "fixed", "width": quality.width, "height": quality.height }
            }],
        }
    })
}

/// The sending half of this end's camera.
///
/// On the voice connection itself, not a connection of its own as a stream
/// is: a camera is part of being in the call, and Discord carries it on the
/// SSRC the voice server set aside for it in `op 2`.
struct CameraSender {
    udp: Arc<UdpSocket>,
    sealer: Arc<Mutex<Sealer>>,
    shared: Arc<Shared>,
    ws: mpsc::UnboundedSender<WsMessage>,
    audio_ssrc: u32,
    video_ssrc: u32,
    sequence: AtomicU32,
    on: AtomicBool,
}

impl CameraSender {
    async fn send_frame(&self, frame: &[u8], timestamp_micros: i64) -> Result<()> {
        if !self.on.load(Ordering::Relaxed) {
            return Ok(());
        }
        // As for a stream: encrypted for the group before it is cut into
        // packets, and dropped while the group is forming, because a frame
        // nobody can decrypt is worse than none - the next keyframe is two
        // seconds away.
        let frame = if self.shared.dave_version.load(Ordering::Relaxed) == 0 {
            frame.to_vec()
        } else {
            let mut group = self.shared.group.lock().unwrap();
            match group.session.as_mut() {
                Some(session) if session.is_ready() => session
                    .encrypt(davey::MediaType::VIDEO, davey::Codec::VP8, frame)
                    .map_err(|e| anyhow!("could not encrypt a camera frame: {e:?}"))?
                    .into_owned(),
                _ => return Ok(()),
            }
        };
        let needed = rtp::packet_count_vp8(&frame);
        if needed == 0 {
            return Ok(());
        }
        // Reserved before anything is built, so two frames in flight cannot
        // interleave their sequence numbers.
        let first = self.sequence.fetch_add(needed as u32, Ordering::Relaxed) as u16;
        let timestamp = rtp::timestamp_from_micros(timestamp_micros);
        for packet in rtp::packetise_vp8(&frame, self.video_ssrc, first, timestamp) {
            let header = rtp::header(&packet);
            let sealed = self.sealer.lock().unwrap().seal(&header, &packet.payload)?;
            self.udp.send(&sealed).await.context("sending a camera packet")?;
        }
        Ok(())
    }
}

fn group_ready(shared: &Shared) -> bool {
    if shared.dave_version.load(Ordering::Relaxed) == 0 {
        return true;
    }
    shared.group.lock().unwrap().session.as_ref().is_some_and(|s| s.is_ready())
}

/// Which close codes mean "do not come back".
///
/// 4004 a token it will not take, 4006 a session it no longer knows, 4011 a
/// server it cannot find, 4014 disconnected on purpose (kicked, moved, the
/// channel deleted), 4016 an encryption mode it does not speak, 4017 DAVE
/// required and not spoken, 4021 and 4022 the call itself ended. Everything
/// else - 4015 a voice server that crashed, 1006 a network that dropped - is
/// the kind of thing a second attempt gets past.
pub fn close_is_final(code: u16) -> bool {
    matches!(code, 4004 | 4006 | 4011 | 4014 | 4016 | 4017 | 4021 | 4022)
}

/// Opens the connection. Returns once media can flow; `on_end` is called once
/// when it stops, for whatever reason.
pub async fn connect(
    state: &AppState,
    account_id: &str,
    handshake: &Handshake,
    media: Media,
    on_end: impl FnOnce(Ended) + Send + 'static,
) -> Result<Arc<VoiceConn>> {
    // Voice and video travel over UDP, which neither Tor nor a SOCKS5 proxy
    // carries. Refused rather than sent directly: an account routed through
    // Tor that quietly called out on its real address would be worse than
    // one that says it cannot call.
    if crate::net::route::router().routed(account_id) {
        anyhow::bail!("calls and screen sharing need UDP, which Tor and SOCKS5 proxies can't carry - turn off Tor for this account to use them");
    }
    let url = format!("wss://{}/?v={VOICE_VERSION}", handshake.endpoint.trim_end_matches(":443"));
    let (socket, _) = tokio_tungstenite::connect_async(url.as_str()).await.context("opening the voice socket")?;
    let (mut write, mut read) = socket.split();

    write
        .send(WsMessage::Text(
            json!({
                "op": 0,
                "d": {
                    "server_id": handshake.server_id,
                    "user_id": handshake.user_id,
                    "session_id": handshake.session_id,
                    "token": handshake.token,
                    // The reason this module exists. Without it the server
                    // never sends anybody's camera.
                    "video": true,
                    "streams": [{ "type": "video", "rid": "100", "quality": 100 }],
                    "max_dave_protocol_version": davey::DAVE_PROTOCOL_VERSION,
                }
            })
            .to_string(),
        ))
        .await
        .context("identifying")?;

    // Everything the server says during the handshake that is not the
    // handshake - who is here, who is on which SSRC, the first DAVE frames -
    // is kept and dealt with once the loop below is running. Dropping it
    // loses the names of everybody already in the call.
    let mut early: Vec<WsMessage> = Vec::new();
    let mut ssrc = 0u32;
    let mut video_ssrc = 0u32;
    let mut address = String::new();
    let mut port = 0u16;
    let mut modes: Vec<String> = Vec::new();
    let mut beat_every = Duration::from_millis(13_750);
    let mut got_hello = false;
    while let Some(frame) = read.next().await {
        let frame = frame.context("the voice socket failed")?;
        match &frame {
            WsMessage::Close(reason) => return Err(anyhow!("the voice server refused the connection: {}", describe_close(reason))),
            WsMessage::Text(text) => {
                let message: Value = serde_json::from_str(text).context("the voice server sent something odd")?;
                match message["op"].as_u64() {
                    Some(8) => {
                        let interval = message["d"]["heartbeat_interval"].as_f64().unwrap_or(41_250.0);
                        beat_every = Duration::from_millis(interval.max(500.0) as u64);
                        got_hello = true;
                    }
                    Some(2) => {
                        let d = &message["d"];
                        ssrc = d["ssrc"].as_u64().unwrap_or(0) as u32;
                        address = d["ip"].as_str().unwrap_or_default().to_string();
                        port = d["port"].as_u64().unwrap_or(0) as u16;
                        modes = d["modes"].as_array().into_iter().flatten().filter_map(|m| m.as_str().map(str::to_string)).collect();
                        // This end's camera SSRC, set aside in the streams
                        // list rather than beside the audio one.
                        video_ssrc = d["streams"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|s| s["type"].as_str() == Some("video"))
                            .and_then(|s| s["ssrc"].as_u64())
                            .unwrap_or(ssrc.wrapping_add(1) as u64) as u32;
                    }
                    _ => early.push(frame.clone()),
                }
            }
            _ => early.push(frame.clone()),
        }
        if got_hello && ssrc != 0 {
            break;
        }
    }
    if ssrc == 0 || address.is_empty() || port == 0 {
        return Err(anyhow!("the voice server never said where to send"));
    }
    let mode = Mode::negotiate(&modes).ok_or_else(|| anyhow!("no encryption mode this client speaks: {modes:?}"))?;

    let udp = UdpSocket::bind("0.0.0.0:0").await.context("opening a socket")?;
    super::streamconn::widen_receive_buffer(&udp);
    udp.connect((address.as_str(), port)).await.context("pointing the socket at the voice server")?;
    udp.send(&super::streamconn::discovery_request(ssrc)).await.context("asking how we are seen")?;
    let mut answer = [0u8; 74];
    let length = tokio::time::timeout(Duration::from_secs(5), udp.recv(&mut answer))
        .await
        .context("the voice server did not answer the discovery packet")??;
    let (public_address, public_port) = super::streamconn::discovery_answer(&answer[..length])?;

    write
        .send(WsMessage::Text(
            json!({
                "op": 1,
                "d": {
                    "protocol": "udp",
                    "data": { "address": public_address, "port": public_port, "mode": mode.wire_name() },
                    // VP8 only, as for Go Live: every Discord client can send
                    // it and every machine can decode it in software, and the
                    // server picks a codec everybody in the call has offered.
                    "codecs": [
                        { "name": "opus", "type": "audio", "priority": 1000, "payload_type": rtp::PAYLOAD_TYPE_OPUS },
                        { "name": "VP8", "type": "video", "priority": 1000, "payload_type": rtp::PAYLOAD_TYPE_VP8, "rtx_payload_type": rtp::PAYLOAD_TYPE_VP8 + 1 },
                    ],
                }
            })
            .to_string(),
        ))
        .await
        .context("choosing a protocol")?;

    let mut key: Vec<u8> = Vec::new();
    let mut dave_version = 0u16;
    while let Some(frame) = read.next().await {
        let frame = frame.context("the voice socket failed")?;
        if let WsMessage::Close(reason) = &frame {
            return Err(anyhow!("the voice server closed after being told how to reach us: {}", describe_close(reason)));
        }
        if let WsMessage::Text(text) = &frame {
            let message: Value = serde_json::from_str(text).unwrap_or_default();
            if message["op"].as_u64() == Some(4) {
                key = message["d"]["secret_key"].as_array().into_iter().flatten().filter_map(|b| b.as_u64().map(|b| b as u8)).collect();
                dave_version = message["d"]["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
                break;
            }
        }
        early.push(frame);
    }
    if key.len() != 32 {
        return Err(anyhow!("the voice server sent no usable key"));
    }

    let user_id: u64 = handshake.user_id.parse().context("that is not a user id")?;
    let channel_id: u64 = handshake.channel_id.parse().context("that is not a channel id")?;
    let shared = Arc::new(Shared {
        state: state.clone(),
        account: account_id.to_string(),
        user_id,
        channel_id,
        owners: Mutex::new(HashMap::new()),
        cameras: Mutex::new(HashMap::new()),
        rtx: Mutex::new(HashMap::new()),
        group: Mutex::new(Group::default()),
        dave_version: AtomicU16::new(dave_version),
        speakers: Mutex::new(HashMap::new()),
        tracker: media.tracker.clone(),
    });

    // The group, where the server wants one. A key package is this device
    // asking to be let in.
    if let Some(package) = shared.reinit_group()? {
        write
            .send(WsMessage::Binary(super::dave::write_binary(super::dave::OP_KEY_PACKAGE, &package).into()))
            .await
            .context("sending the key package")?;
    }
    // Every camera in the call, at full quality. Without saying what it wants
    // a client can be sent nothing at all.
    write
        .send(WsMessage::Text(json!({ "op": 15, "d": { "any": 100 } }).to_string()))
        .await
        .context("asking for video")?;

    tracing::info!(
        "discord[{account_id}]: voice connected: ssrc {ssrc}, {}, DAVE v{dave_version}",
        mode.wire_name()
    );

    let (stop_tx, stop_rx) = watch::channel(false);
    let (ws_tx, ws_rx) = mpsc::unbounded_channel::<WsMessage>();
    let udp = Arc::new(udp);
    let sealer = Arc::new(Mutex::new(Sealer::new(mode, &key)?));

    tokio::spawn(receive(shared.clone(), udp.clone(), sealer.clone(), ssrc, mode, key.clone(), stop_rx.clone()));
    tokio::spawn(clock(shared.clone(), udp.clone(), sealer.clone(), ssrc, media.playback, media.mic, ws_tx.clone(), stop_rx.clone()));
    let camera = CameraSender {
        udp: udp.clone(),
        sealer,
        shared: shared.clone(),
        ws: ws_tx.clone(),
        audio_ssrc: ssrc,
        video_ssrc,
        sequence: AtomicU32::new(rand::random::<u16>() as u32),
        on: AtomicBool::new(false),
    };
    tokio::spawn(async move {
        let ended = socket_loop(shared.clone(), write, read, early, beat_every, ws_rx, stop_rx).await;
        // Whatever cameras were showing are not any more.
        shared.end_all_cameras();
        on_end(ended);
    });

    Ok(Arc::new(VoiceConn { stop: stop_tx, camera }))
}

fn describe_close(reason: &Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> String {
    reason
        .as_ref()
        .map(|r| format!("{} {}", u16::from(r.code), r.reason))
        .unwrap_or_else(|| "with no reason given".to_string())
}

/// What the three tasks of one connection share.
struct Shared {
    state: AppState,
    account: String,
    user_id: u64,
    channel_id: u64,
    /// SSRC to the user it belongs to, audio and video alike.
    owners: Mutex<HashMap<u32, u64>>,
    /// Cameras being received: SSRC to their reassembly and keyframe state.
    cameras: Mutex<HashMap<u32, Camera>>,
    /// Retransmission SSRC to the camera SSRC it resends for.
    rtx: Mutex<HashMap<u32, u32>>,
    group: Mutex<Group>,
    /// The DAVE version in force. Zero means no end-to-end encryption.
    dave_version: AtomicU16,
    /// Everybody being heard, by SSRC.
    speakers: Mutex<HashMap<u32, Jitter>>,
    tracker: Arc<super::voice::SpeakingTracker>,
}

/// The DAVE group, and what the protocol needs remembered around it.
#[derive(Default)]
struct Group {
    session: Option<davey::DaveSession>,
    /// Transition id to the protocol version it moves to.
    pending: HashMap<u16, u16>,
    /// Everybody the server has said is in the call, which is who may be let
    /// into the group.
    recognised: HashSet<u64>,
}

struct Camera {
    user: u64,
    rx: VideoReceiver,
}

impl Shared {
    /// Creates or rebuilds the DAVE session for the version in force, and
    /// returns the key package to send - or none, where there is no group.
    ///
    /// songbird's `reinit_dave_session`: a version of zero resets any session
    /// and lets media through in the clear for a moment while the far ends
    /// catch up.
    fn reinit_group(&self) -> Result<Option<Vec<u8>>> {
        let version = self.dave_version.load(Ordering::Relaxed);
        let mut group = self.group.lock().unwrap();
        match NonZeroU16::new(version) {
            Some(version) => {
                let package = match group.session.as_mut() {
                    Some(session) => {
                        session
                            .reinit(version, self.user_id, self.channel_id, None)
                            .map_err(|e| anyhow!("could not restart DAVE: {e:?}"))?;
                        session.create_key_package().map_err(|e| anyhow!("could not make a key package: {e:?}"))?
                    }
                    None => {
                        let mut session = davey::DaveSession::new(version, self.user_id, self.channel_id, None)
                            .map_err(|e| anyhow!("could not start DAVE: {e:?}"))?;
                        let package = session.create_key_package().map_err(|e| anyhow!("could not make a key package: {e:?}"))?;
                        group.session = Some(session);
                        package
                    }
                };
                Ok(Some(package))
            }
            None => {
                if let Some(session) = group.session.as_mut() {
                    let _ = session.reset();
                    session.set_passthrough_mode(true, Some(10));
                }
                Ok(None)
            }
        }
    }

    fn transition_ready(&self, transition: u16) -> WsMessage {
        let version = self.dave_version.load(Ordering::Relaxed);
        WsMessage::Text(json!({ "op": 23, "d": { "transition_id": transition, "protocol_version": version } }).to_string())
    }

    /// Takes one JSON frame from the server; returns what, if anything, has to
    /// go back.
    fn take_json(&self, op: u64, d: &Value) -> Vec<WsMessage> {
        let mut replies = Vec::new();
        match op {
            // Speaking: who is on which audio SSRC.
            5 => {
                if let (Some(user), Some(ssrc)) = (user_of(d), d["ssrc"].as_u64()) {
                    self.owners.lock().unwrap().insert(ssrc as u32, user);
                    self.tracker.learn(ssrc as u32, user.to_string());
                }
            }
            // Video: whose camera is on which SSRC - or, with a zero SSRC,
            // that it has been switched off.
            12 => {
                let Some(user) = user_of(d) else { return replies };
                let (audio, video) = video_ssrcs(d);
                self.rtx.lock().unwrap().extend(super::videorx::rtx_pairs(d));
                let mut owners = self.owners.lock().unwrap();
                if let Some(audio) = audio {
                    owners.insert(audio, user);
                }
                if video.is_empty() {
                    drop(owners);
                    self.end_camera(user);
                } else {
                    for ssrc in video {
                        owners.insert(ssrc, user);
                    }
                }
            }
            // Clients connect: who is in the call, for the DAVE group.
            11 => {
                let mut group = self.group.lock().unwrap();
                for id in d["user_ids"].as_array().into_iter().flatten() {
                    if let Some(id) = id.as_str().and_then(|s| s.parse().ok()).or_else(|| id.as_u64()) {
                        group.recognised.insert(id);
                    }
                }
            }
            // Client disconnect: forget their SSRCs, their camera, and their
            // place in the group.
            13 => {
                let Some(user) = user_of(d) else { return replies };
                self.group.lock().unwrap().recognised.remove(&user);
                let gone: Vec<u32> = self.owners.lock().unwrap().iter().filter(|(_, u)| **u == user).map(|(s, _)| *s).collect();
                for ssrc in &gone {
                    self.owners.lock().unwrap().remove(ssrc);
                    self.speakers.lock().unwrap().remove(ssrc);
                    self.tracker.forget(*ssrc);
                }
                self.end_camera(user);
            }
            // Prepare transition.
            21 => {
                let transition = d["transition_id"].as_u64().unwrap_or(0) as u16;
                let version = d["protocol_version"].as_u64().unwrap_or(0) as u16;
                self.group.lock().unwrap().pending.insert(transition, version);
                if transition == 0 {
                    self.execute_transition(transition);
                } else if version == 0 {
                    if let Some(session) = self.group.lock().unwrap().session.as_mut() {
                        session.set_passthrough_mode(true, Some(120));
                    }
                    replies.push(self.transition_ready(transition));
                }
            }
            // Execute transition.
            22 => {
                self.execute_transition(d["transition_id"].as_u64().unwrap_or(0) as u16);
            }
            // Prepare epoch: epoch 1 is a group starting again from nothing.
            24 => {
                if d["epoch"].as_u64() == Some(1) {
                    self.dave_version.store(d["protocol_version"].as_u64().unwrap_or(0) as u16, Ordering::Relaxed);
                    match self.reinit_group() {
                        Ok(Some(package)) => replies.push(WsMessage::Binary(
                            super::dave::write_binary(super::dave::OP_KEY_PACKAGE, &package).into(),
                        )),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("discord[{}]: DAVE could not restart: {e:#}", self.account),
                    }
                }
            }
            _ => {}
        }
        replies
    }

    fn execute_transition(&self, transition: u16) {
        let mut group = self.group.lock().unwrap();
        let Some(new_version) = group.pending.remove(&transition) else {
            tracing::debug!("discord[{}]: DAVE execute for unknown transition {transition}", self.account);
            return;
        };
        let old_version = self.dave_version.swap(new_version, Ordering::Relaxed);
        // Upgrading from nothing: let clear media through briefly while the
        // far ends switch over.
        if transition > 0 && old_version == 0 && new_version != 0 {
            if let Some(session) = group.session.as_mut() {
                session.set_passthrough_mode(true, Some(10));
            }
        }
    }

    /// Takes one binary DAVE frame from the server.
    fn take_binary(&self, bytes: &[u8]) -> Vec<WsMessage> {
        use super::dave::{OP_ANNOUNCE_COMMIT_TRANSITION, OP_COMMIT_WELCOME, OP_EXTERNAL_SENDER, OP_PROPOSALS, OP_WELCOME};
        let mut replies = Vec::new();
        let Some(frame) = super::dave::read_binary(bytes) else { return replies };
        let mut failed_transition: Option<u16> = None;
        {
            let mut group = self.group.lock().unwrap();
            let recognised: Vec<u64> = group.recognised.iter().copied().collect();
            let Some(session) = group.session.as_mut() else { return replies };
            match frame.opcode {
                OP_EXTERNAL_SENDER => {
                    if let Err(e) = session.set_external_sender(frame.payload) {
                        tracing::warn!("discord[{}]: DAVE external sender refused: {e:?}", self.account);
                    }
                }
                OP_PROPOSALS => {
                    let Some((kind, proposals)) = frame.payload.split_first() else { return replies };
                    let operation = match kind {
                        0 => davey::ProposalsOperationType::APPEND,
                        1 => davey::ProposalsOperationType::REVOKE,
                        _ => return replies,
                    };
                    match session.process_proposals(operation, proposals, Some(&recognised)) {
                        Ok(Some(cw)) => {
                            let mut payload = cw.commit;
                            if let Some(welcome) = cw.welcome {
                                payload.extend_from_slice(&welcome);
                            }
                            replies.push(WsMessage::Binary(super::dave::write_binary(OP_COMMIT_WELCOME, &payload).into()));
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!("discord[{}]: DAVE proposals failed: {e:?}", self.account),
                    }
                }
                OP_ANNOUNCE_COMMIT_TRANSITION | OP_WELCOME => {
                    let Some((transition, message)) = split_transition(frame.payload) else { return replies };
                    let result = if frame.opcode == OP_WELCOME {
                        session.process_welcome(message).map_err(|e| format!("{e:?}"))
                    } else {
                        session.process_commit(message).map_err(|e| format!("{e:?}"))
                    };
                    match result {
                        Ok(()) if transition != 0 => {
                            let version = self.dave_version.load(Ordering::Relaxed);
                            group.pending.insert(transition, version);
                            replies.push(self.transition_ready(transition));
                        }
                        Ok(()) => {}
                        Err(e) => {
                            tracing::warn!("discord[{}]: DAVE {} failed: {e}", self.account, if frame.opcode == OP_WELCOME { "welcome" } else { "commit" });
                            failed_transition = Some(transition);
                        }
                    }
                }
                _ => {}
            }
        }
        // A commit or welcome that would not apply: say so, and start the
        // group again - which is what songbird does and what the server then
        // expects.
        if let Some(transition) = failed_transition {
            replies.push(WsMessage::Text(json!({ "op": 31, "d": { "transition_id": transition } }).to_string()));
            match self.reinit_group() {
                Ok(Some(package)) => replies.push(WsMessage::Binary(super::dave::write_binary(super::dave::OP_KEY_PACKAGE, &package).into())),
                Ok(None) => {}
                Err(e) => tracing::warn!("discord[{}]: DAVE could not restart: {e:#}", self.account),
            }
        }
        replies
    }

    /// Decrypts a DAVE frame from `ssrc`'s owner, or passes a clear one through.
    ///
    /// None where it cannot be read: nobody has said whose SSRC it is, the
    /// group is not ready, or it simply will not decrypt - all of which are
    /// dropped rather than handed to a decoder as noise.
    fn open_media(&self, ssrc: u32, media: davey::MediaType, payload: Vec<u8>) -> Option<Vec<u8>> {
        if self.dave_version.load(Ordering::Relaxed) == 0 || !is_dave_frame(&payload) {
            return Some(payload);
        }
        let user = *self.owners.lock().unwrap().get(&ssrc)?;
        let mut group = self.group.lock().unwrap();
        let session = group.session.as_mut()?;
        if !session.is_ready() {
            return None;
        }
        session.decrypt(user, media, &payload).ok()
    }

    fn end_camera(&self, user: u64) {
        let ended: Vec<u32> = {
            let mut cameras = self.cameras.lock().unwrap();
            let ssrcs: Vec<u32> = cameras.iter().filter(|(_, c)| c.user == user).map(|(s, _)| *s).collect();
            for ssrc in &ssrcs {
                if let Some(camera) = cameras.remove(ssrc) {
                    tracing::info!("discord[{}]: camera from {user} ended: {:?}", self.account, camera.rx.stats);
                }
            }
            ssrcs
        };
        if !ended.is_empty() {
            self.state.events.emit(
                "discordCameraFrame",
                json!({ "accountId": self.account, "userId": user.to_string(), "ended": true }),
            );
        }
    }

    fn end_all_cameras(&self) {
        let users: HashSet<u64> = self.cameras.lock().unwrap().values().map(|c| c.user).collect();
        for user in users {
            self.end_camera(user);
        }
    }
}

/// A user id as Discord writes it: a string everywhere but here, sometimes a
/// number here.
fn user_of(d: &Value) -> Option<u64> {
    d["user_id"].as_str().and_then(|s| s.parse().ok()).or_else(|| d["user_id"].as_u64())
}

/// The audio SSRC and every active video SSRC an `op 12` names.
///
/// A camera switched off arrives as a video SSRC of zero, or as streams all
/// marked inactive - both read here as "no video".
pub fn video_ssrcs(d: &Value) -> (Option<u32>, Vec<u32>) {
    let audio = d["audio_ssrc"].as_u64().filter(|s| *s != 0).map(|s| s as u32);
    let mut video: Vec<u32> = Vec::new();
    let streams: Vec<&Value> = d["streams"].as_array().into_iter().flatten().collect();
    if streams.is_empty() {
        if let Some(ssrc) = d["video_ssrc"].as_u64().filter(|s| *s != 0) {
            video.push(ssrc as u32);
        }
    } else {
        for stream in streams {
            let active = stream["active"].as_bool().unwrap_or(true);
            if let Some(ssrc) = stream["ssrc"].as_u64().filter(|s| *s != 0) {
                if active {
                    video.push(ssrc as u32);
                }
            }
        }
    }
    (audio, video)
}

fn is_dave_frame(payload: &[u8]) -> bool {
    payload.len() >= 11 && payload[payload.len() - 2..] == DAVE_MAGIC
}

fn split_transition(payload: &[u8]) -> Option<(u16, &[u8])> {
    if payload.len() < 2 {
        return None;
    }
    Some((u16::from_be_bytes([payload[0], payload[1]]), &payload[2..]))
}

/// The websocket: heartbeats, the server's news, and DAVE.
async fn socket_loop(
    shared: Arc<Shared>,
    mut write: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        WsMessage,
    >,
    mut read: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    early: Vec<WsMessage>,
    beat_every: Duration,
    mut outgoing: mpsc::UnboundedReceiver<WsMessage>,
    mut stop: watch::Receiver<bool>,
) -> Ended {
    let account = shared.account.clone();
    let mut seq_ack: u64 = 0;
    let handle = |message: &WsMessage, seq_ack: &mut u64| -> Vec<WsMessage> {
        match message {
            WsMessage::Binary(bytes) => {
                if bytes.len() >= 2 {
                    *seq_ack = (*seq_ack).max(u16::from_be_bytes([bytes[0], bytes[1]]) as u64);
                }
                shared.take_binary(bytes)
            }
            WsMessage::Text(text) => {
                let Ok(value) = serde_json::from_str::<Value>(text) else { return Vec::new() };
                if let Some(seq) = value["seq"].as_u64() {
                    *seq_ack = (*seq_ack).max(seq);
                }
                match value["op"].as_u64() {
                    Some(op) => shared.take_json(op, &value["d"]),
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    };

    for message in &early {
        for reply in handle(message, &mut seq_ack) {
            if write.send(reply).await.is_err() {
                return Ended::Retry("the voice socket went away during the handshake".into());
            }
        }
    }

    let mut beat = tokio::time::interval(beat_every);
    loop {
        tokio::select! {
            _ = stop.changed() => {
                let _ = write.send(WsMessage::Close(None)).await;
                return Ended::Stopped;
            }
            _ = beat.tick() => {
                // v8's heartbeat carries the last sequence number seen.
                let frame = json!({ "op": 3, "d": { "t": rand::random::<u32>(), "seq_ack": seq_ack } });
                if write.send(WsMessage::Text(frame.to_string())).await.is_err() {
                    return Ended::Retry("the voice socket would not take a heartbeat".into());
                }
            }
            Some(message) = outgoing.recv() => {
                if write.send(message).await.is_err() {
                    return Ended::Retry("the voice socket went away".into());
                }
            }
            frame = read.next() => {
                match frame {
                    Some(Ok(WsMessage::Close(reason))) => {
                        let said = describe_close(&reason);
                        tracing::warn!("discord[{account}]: the voice server closed the connection: {said}");
                        let code = reason.as_ref().map(|r| u16::from(r.code)).unwrap_or(1006);
                        return if close_is_final(code) { Ended::Final(said) } else { Ended::Retry(said) };
                    }
                    Some(Ok(message)) => {
                        for reply in handle(&message, &mut seq_ack) {
                            if write.send(reply).await.is_err() {
                                return Ended::Retry("the voice socket went away".into());
                            }
                        }
                    }
                    Some(Err(e)) => return Ended::Retry(format!("the voice socket failed: {e}")),
                    None => return Ended::Retry("the voice socket closed".into()),
                }
            }
        }
    }
}

/// Everything arriving on UDP: other people's voices and cameras.
async fn receive(
    shared: Arc<Shared>,
    udp: Arc<UdpSocket>,
    sealer: Arc<Mutex<Sealer>>,
    ssrc: u32,
    mode: Mode,
    key: Vec<u8>,
    mut stop: watch::Receiver<bool>,
) {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let mut buffer = vec![0u8; 4096];
    let mut unopened = 0u64;
    let mut rtcp_checked = false;
    loop {
        let length = tokio::select! {
            _ = stop.changed() => break,
            read = udp.recv(&mut buffer) => match read {
                Ok(length) => length,
                Err(e) => {
                    tracing::warn!("discord[{}]: the voice socket failed: {e}", shared.account);
                    break;
                }
            },
        };
        let datagram = &buffer[..length];
        // RTCP shares the socket. Not used, but the first one is opened the
        // way ours are sealed, which says whether the relay can read the
        // retransmission and keyframe requests this sends.
        if videorx::is_rtcp(datagram) {
            if !rtcp_checked {
                rtcp_checked = true;
                let opens = voicecrypto::open_at(mode, &key, datagram, videorx::RTCP_CLEAR, videorx::RTCP_CLEAR).is_ok();
                tracing::info!("discord[{}]: RTCP from the relay {} with its header in the clear", shared.account, if opens { "opens" } else { "does not open" });
            }
            continue;
        }
        let Some(parsed) = rtp::parse_header(datagram) else { continue };
        let payload = match voicecrypto::open_rtp(mode, &key, datagram, &parsed) {
            Ok(payload) => payload,
            Err(_) => {
                unopened += 1;
                if unopened == 1 || unopened % 1000 == 0 {
                    tracing::warn!("discord[{}]: {unopened} voice packets would not open", shared.account);
                }
                continue;
            }
        };
        // A retransmission is the packet it replaces, on the camera's SSRC.
        let (video_ssrc, sequence, payload, resent) = match parsed.payload_type {
            rtp::PAYLOAD_TYPE_OPUS => {
                let Some(opus) = shared.open_media(parsed.ssrc, davey::MediaType::AUDIO, payload) else { continue };
                shared
                    .speakers
                    .lock()
                    .unwrap()
                    .entry(parsed.ssrc)
                    .or_insert_with(Jitter::new)
                    .push(parsed.sequence, opus);
                continue;
            }
            rtp::PAYLOAD_TYPE_VP8 => (parsed.ssrc, parsed.sequence, payload, false),
            videorx::PAYLOAD_TYPE_RTX => {
                let known = shared.rtx.lock().unwrap().get(&parsed.ssrc).copied();
                let Some(media) = known.or_else(|| {
                    let guess = parsed.ssrc.wrapping_sub(1);
                    shared.cameras.lock().unwrap().contains_key(&guess).then_some(guess)
                }) else {
                    continue;
                };
                let Some((sequence, original)) = videorx::unwrap_rtx(&payload) else { continue };
                (media, sequence, original, true)
            }
            _ => continue,
        };
        let Some(user) = shared.owners.lock().unwrap().get(&video_ssrc).copied() else { continue };
        let now = Instant::now();
        let (frames, lost) = {
            let mut cameras = shared.cameras.lock().unwrap();
            let camera = cameras.entry(video_ssrc).or_insert_with(|| Camera { user, rx: VideoReceiver::new() });
            camera.user = user;
            let frames = camera.rx.push(
                rtp::Packet {
                    payload_type: rtp::PAYLOAD_TYPE_VP8,
                    sequence,
                    timestamp: parsed.timestamp,
                    ssrc: video_ssrc,
                    marker: parsed.marker,
                    payload,
                },
                resent,
                now,
            );
            (frames, camera.rx.to_ask_for(now))
        };
        if !lost.is_empty() {
            send_rtcp(&udp, &sealer, &videorx::nack(ssrc, video_ssrc, &lost)).await;
        }
        for frame in frames {
            let Some(picture) = shared.open_media(video_ssrc, davey::MediaType::VIDEO, frame.data.clone()) else { continue };
            let keyframe = super::reassemble::is_keyframe(&picture);
            let (admitted, want_key, first) = {
                let mut cameras = shared.cameras.lock().unwrap();
                let Some(camera) = cameras.get_mut(&video_ssrc) else { continue };
                let first = camera.rx.stats.frames == 0;
                let admitted = camera.rx.admit(&frame, keyframe);
                let want_key = camera.rx.wants_keyframe(now);
                if admitted && camera.rx.stats.frames % 900 == 0 {
                    tracing::info!("discord[{}]: camera from {user}: {:?}", shared.account, camera.rx.stats);
                }
                (admitted, want_key, first && admitted)
            };
            if want_key {
                send_rtcp(&udp, &sealer, &videorx::pli(ssrc, video_ssrc)).await;
            }
            if !admitted {
                continue;
            }
            if first {
                tracing::info!("discord[{}]: a camera from {user} is arriving", shared.account);
            }
            shared.state.events.emit(
                "discordCameraFrame",
                json!({
                    "accountId": shared.account,
                    "userId": user.to_string(),
                    "keyframe": keyframe,
                    "timestampMicros": (frame.timestamp as u64 * 1_000_000) / rtp::VIDEO_CLOCK_HZ,
                    "frame": STANDARD.encode(&picture),
                }),
            );
        }
    }
}

/// Seals and sends one RTCP packet; a failure is the next packet's problem.
async fn send_rtcp(udp: &UdpSocket, sealer: &Mutex<Sealer>, packet: &[u8]) {
    let sealed = videorx::seal_rtcp(&mut sealer.lock().unwrap(), packet);
    if let Ok(sealed) = sealed {
        let _ = udp.send(&sealed).await;
    }
}

/// The 20ms clock: play what everybody said, send what we said, keep the
/// path open.
#[allow(clippy::too_many_arguments)]
async fn clock(
    shared: Arc<Shared>,
    udp: Arc<UdpSocket>,
    sealer: Arc<Mutex<Sealer>>,
    ssrc: u32,
    playback: Option<Arc<crate::audio::Playback>>,
    mic: Option<crate::audio::MicSource>,
    ws: mpsc::UnboundedSender<WsMessage>,
    mut stop: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut encoder = match opus2::Encoder::new(48_000, opus2::Channels::Stereo, opus2::Application::Voip) {
        Ok(encoder) => Some(encoder),
        Err(e) => {
            tracing::warn!("discord[{}]: no Opus encoder, so nothing can be sent: {e:?}", shared.account);
            None
        }
    };
    if let Some(encoder) = encoder.as_mut() {
        let _ = encoder.set_bitrate(opus2::Bitrate::Bits(64_000));
    }
    let mut gate = Gate::default();
    let mut sequence: u16 = rand::random();
    let mut timestamp: u32 = rand::random();
    let mut next_keepalive = Instant::now();
    let mut last_sweep = Instant::now();

    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = tick.tick() => {}
        }

        // Everybody else, mixed.
        let voices: Vec<(u32, Vec<i16>)> = {
            let mut speakers = shared.speakers.lock().unwrap();
            speakers.iter_mut().filter_map(|(ssrc, jitter)| jitter.pop().map(|pcm| (*ssrc, pcm))).collect()
        };
        for (ssrc, pcm) in &voices {
            shared.tracker.heard_from(*ssrc, super::voice::peak_of(pcm));
        }
        if let Some(playback) = playback.as_ref() {
            // Each person at the volume they were set to, before the mix, and
            // the whole call at its own after it. The speaking rings above
            // were fed before either: turning somebody down is not the same
            // as them going quiet.
            let prefs = shared.state.voice_prefs.get();
            let mut voices = voices;
            if !prefs.user_volumes.is_empty() {
                let owners = shared.owners.lock().unwrap();
                for (ssrc, pcm) in voices.iter_mut() {
                    if let Some(user) = owners.get(ssrc) {
                        crate::audio::apply_gain(pcm, prefs.user_volume(&user.to_string()));
                    }
                }
            }
            let refs: Vec<&[i16]> = voices.iter().map(|(_, pcm)| pcm.as_slice()).collect();
            let mut mixed = super::voice::mix(&refs);
            crate::audio::apply_gain(&mut mixed, prefs.output_volume);
            if !mixed.is_empty() {
                playback.push(&mixed);
            }
        }
        // Decoders for people who have gone quiet for good.
        if last_sweep.elapsed() > Duration::from_secs(30) {
            last_sweep = Instant::now();
            shared.speakers.lock().unwrap().retain(|_, jitter| jitter.last_arrival.elapsed() < Duration::from_secs(60));
        }

        // Us.
        let frame = mic.as_ref().and_then(|mic| mic.take(FRAME_SAMPLES));
        let peak = frame.as_ref().map(|f| f.iter().fold(0.0f32, |m, s| m.max(s.abs()))).unwrap_or(0.0);
        let action = gate.step(peak);
        if let Some(speaking) = action.announce {
            let _ = ws.send(WsMessage::Text(
                json!({ "op": 5, "d": { "speaking": if speaking { 1 } else { 0 }, "delay": 0, "ssrc": ssrc } }).to_string(),
            ));
        }
        let opus: Option<Vec<u8>> = match action.send {
            Out::Nothing => None,
            Out::Silence => Some(SILENT_FRAME.to_vec()),
            Out::Voice => match (encoder.as_mut(), frame.as_ref()) {
                (Some(encoder), Some(frame)) => {
                    let mut out = vec![0u8; 1275];
                    match encoder.encode_float(frame, &mut out) {
                        Ok(length) => {
                            out.truncate(length);
                            Some(out)
                        }
                        Err(_) => None,
                    }
                }
                _ => Some(SILENT_FRAME.to_vec()),
            },
        };
        timestamp = timestamp.wrapping_add(rtp::OPUS_FRAME_SAMPLES as u32);
        if let Some(opus) = opus {
            let body = seal_for_group(&shared, opus);
            let header = rtp::header(&rtp::Packet {
                payload_type: rtp::PAYLOAD_TYPE_OPUS,
                sequence,
                timestamp,
                ssrc,
                marker: false,
                payload: Vec::new(),
            });
            sequence = sequence.wrapping_add(1);
            let sealed = sealer.lock().unwrap().seal(&header, &body);
            if let Ok(packet) = sealed {
                let _ = udp.send(&packet).await;
            }
        }

        if Instant::now() >= next_keepalive {
            next_keepalive = Instant::now() + KEEPALIVE_EVERY;
            let _ = udp.send(&ssrc.to_be_bytes()).await;
        }
    }
}

/// Encrypts a frame for the group where there is one ready, and leaves it
/// clear otherwise - which is what songbird sends while a group forms.
fn seal_for_group(shared: &Shared, opus: Vec<u8>) -> Vec<u8> {
    if shared.dave_version.load(Ordering::Relaxed) == 0 {
        return opus;
    }
    let mut group = shared.group.lock().unwrap();
    match group.session.as_mut() {
        Some(session) if session.is_ready() => match session.encrypt_opus(&opus) {
            Ok(sealed) => sealed.into_owned(),
            Err(_) => opus,
        },
        _ => opus,
    }
}

/// When to send, and when to say so.
///
/// Voice activity rather than a constant stream: a microphone that is always
/// sending is a green ring that is always lit, on everybody else's screen.
/// Speech opens the gate at once; it stays open for half a second of quiet
/// so the ends of words are not clipped, then five frames of silence are sent
/// - which Discord asks for, so the far end fades rather than freezes - and
/// only then does speaking stop.
#[derive(Default)]
pub struct Gate {
    /// Ticks the gate stays open for after the last sound.
    hold: u32,
    /// Silence frames still to send after closing.
    trailing: u32,
    speaking: bool,
}

/// What one tick should put on the wire.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    Nothing,
    Silence,
    Voice,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Action {
    pub send: Out,
    /// Some(true) to announce speaking, Some(false) to announce silence.
    pub announce: Option<bool>,
}

/// About -48dBFS: well under speech, well over a quiet room's hiss.
const GATE_THRESHOLD: f32 = 0.004;
/// Half a second of 20ms ticks.
const GATE_HOLD_TICKS: u32 = 25;

impl Gate {
    pub fn step(&mut self, peak: f32) -> Action {
        if peak > GATE_THRESHOLD {
            self.hold = GATE_HOLD_TICKS;
            self.trailing = 5;
            let announce = if self.speaking { None } else { Some(true) };
            self.speaking = true;
            return Action { send: Out::Voice, announce };
        }
        if self.hold > 0 {
            self.hold -= 1;
            return Action { send: Out::Voice, announce: None };
        }
        if self.trailing > 0 {
            self.trailing -= 1;
            let announce = if self.trailing == 0 && self.speaking {
                self.speaking = false;
                Some(false)
            } else {
                None
            };
            return Action { send: Out::Silence, announce };
        }
        Action { send: Out::Nothing, announce: None }
    }
}

/// One speaker's packets, held just long enough to put them in order.
///
/// Two frames of delay before playing starts: enough to absorb ordinary
/// reordering on a home connection, little enough that nobody hears it. A
/// missing frame is concealed by the decoder rather than skipped - Opus is
/// good at that for a frame or two - and a speaker who has fallen silent is
/// left silent rather than concealed into a drone.
pub struct Jitter {
    packets: HashMap<u16, Vec<u8>>,
    next: Option<u16>,
    started: bool,
    concealed: u32,
    pub last_arrival: Instant,
    decoder: Option<opus2::Decoder>,
}

const JITTER_PRIME: usize = 2;
const JITTER_MAX: usize = 25;

impl Jitter {
    pub fn new() -> Self {
        Self {
            packets: HashMap::new(),
            next: None,
            started: false,
            concealed: 0,
            last_arrival: Instant::now(),
            decoder: opus2::Decoder::new(48_000, opus2::Channels::Stereo).ok(),
        }
    }

    pub fn push(&mut self, sequence: u16, opus: Vec<u8>) {
        self.last_arrival = Instant::now();
        if let Some(next) = self.next {
            // Behind what has already been played: too late to be any use.
            if (sequence.wrapping_sub(next) as i16) < 0 {
                return;
            }
        }
        self.packets.insert(sequence, opus);
        // Far too much held means the clocks have drifted or a burst arrived;
        // start again from what is newest rather than fall ever further
        // behind.
        if self.packets.len() > JITTER_MAX {
            let newest = self.newest();
            self.packets.retain(|seq, _| (newest.wrapping_sub(*seq) as i16) < JITTER_PRIME as i16);
            self.next = None;
            self.started = false;
        }
    }

    fn oldest(&self) -> u16 {
        let any = *self.packets.keys().next().unwrap_or(&0);
        self.packets.keys().copied().fold(any, |oldest, seq| if (seq.wrapping_sub(oldest) as i16) < 0 { seq } else { oldest })
    }

    fn newest(&self) -> u16 {
        let any = *self.packets.keys().next().unwrap_or(&0);
        self.packets.keys().copied().fold(any, |newest, seq| if (seq.wrapping_sub(newest) as i16) > 0 { seq } else { newest })
    }

    /// The next 20ms of this speaker, or None for silence.
    pub fn pop(&mut self) -> Option<Vec<i16>> {
        if !self.started {
            if self.packets.len() < JITTER_PRIME {
                return None;
            }
            self.started = true;
            self.next = Some(self.oldest());
        }
        let next = self.next?;
        let opus = self.packets.remove(&next);
        self.next = Some(next.wrapping_add(1));
        match opus {
            Some(opus) => {
                self.concealed = 0;
                self.decode(&opus)
            }
            None if self.packets.is_empty() => {
                // Nothing held: they have stopped. Wait for the next burst.
                self.started = false;
                self.next = None;
                None
            }
            None => {
                self.concealed += 1;
                if self.concealed > 5 {
                    // A long gap with packets beyond it: jump to them.
                    self.concealed = 0;
                    self.next = Some(self.oldest());
                }
                self.decode(&[])
            }
        }
    }

    fn decode(&mut self, opus: &[u8]) -> Option<Vec<i16>> {
        let decoder = self.decoder.as_mut()?;
        let mut pcm = vec![0i16; FRAME_SAMPLES * 3];
        let per_channel = decoder.decode(opus, &mut pcm, false).ok()?;
        pcm.truncate(per_channel * 2);
        Some(pcm)
    }
}

impl Default for Jitter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    /// Turning a camera off is the same message with the stream inactive and
    /// no video SSRC - what makes the far end drop the tile rather than hold
    /// the last frame.
    #[test]
    fn a_camera_is_announced_on_and_off() {
        let on = announce_camera(10, 11, true, VideoQuality::default());
        assert_eq!(on["d"]["video_ssrc"], 11);
        assert_eq!(on["d"]["streams"][0]["active"], true);
        assert_eq!(on["d"]["streams"][0]["max_resolution"]["height"], 720);
        let off = announce_camera(10, 11, false, VideoQuality::default());
        assert_eq!(off["d"]["video_ssrc"], 0);
        assert_eq!(off["d"]["streams"][0]["active"], false);
        assert_eq!(off["d"]["audio_ssrc"], 10);
    }

    use super::*;

    #[test]
    fn only_the_codes_that_mean_it_are_final() {
        for code in [4004, 4006, 4011, 4014, 4016, 4017, 4021, 4022] {
            assert!(close_is_final(code), "{code} should not be retried");
        }
        for code in [1000, 1001, 1006, 4000, 4009, 4015] {
            assert!(!close_is_final(code), "{code} is worth another attempt");
        }
    }

    #[test]
    fn a_camera_is_read_from_its_streams() {
        let d = json!({
            "user_id": "42", "audio_ssrc": 100, "video_ssrc": 101,
            "streams": [{ "ssrc": 101, "active": true, "rid": "100" }, { "ssrc": 103, "active": false, "rid": "50" }]
        });
        assert_eq!(video_ssrcs(&d), (Some(100), vec![101]));
        assert_eq!(user_of(&d), Some(42));
    }

    #[test]
    fn a_camera_switched_off_reads_as_no_video() {
        let off = json!({ "user_id": "42", "audio_ssrc": 100, "video_ssrc": 0, "streams": [{ "ssrc": 101, "active": false }] });
        assert_eq!(video_ssrcs(&off).1, Vec::<u32>::new());
        let bare = json!({ "user_id": 42, "audio_ssrc": 100, "video_ssrc": 0 });
        assert_eq!(video_ssrcs(&bare).1, Vec::<u32>::new());
    }

    #[test]
    fn a_dave_frame_is_known_by_its_marker() {
        let mut frame = vec![0u8; 20];
        assert!(!is_dave_frame(&frame));
        frame[18] = 0xFA;
        frame[19] = 0xFA;
        assert!(is_dave_frame(&frame));
        assert!(!is_dave_frame(&[0xFA, 0xFA]), "too short to be one");
    }

    #[test]
    fn the_gate_opens_on_speech_holds_then_trails_off_in_silence() {
        let mut gate = Gate::default();
        assert_eq!(gate.step(0.0), Action { send: Out::Nothing, announce: None }, "a quiet room sends nothing");
        assert_eq!(gate.step(0.2), Action { send: Out::Voice, announce: Some(true) });
        assert_eq!(gate.step(0.2), Action { send: Out::Voice, announce: None }, "announced once");
        for _ in 0..GATE_HOLD_TICKS {
            assert_eq!(gate.step(0.0).send, Out::Voice, "the ends of words are kept");
        }
        for i in 0..5 {
            let action = gate.step(0.0);
            assert_eq!(action.send, Out::Silence);
            assert_eq!(action.announce, if i == 4 { Some(false) } else { None });
        }
        assert_eq!(gate.step(0.0), Action { send: Out::Nothing, announce: None });
    }

    fn opus_silence() -> Vec<u8> {
        SILENT_FRAME.to_vec()
    }

    #[test]
    fn packets_are_played_in_order_after_a_short_wait() {
        let mut jitter = Jitter::new();
        jitter.push(11, opus_silence());
        assert!(jitter.pop().is_none(), "one packet is not enough to start");
        jitter.push(10, opus_silence());
        assert_eq!(jitter.pop().map(|p| p.len()), Some(FRAME_SAMPLES), "starts from the earliest");
        assert_eq!(jitter.next, Some(11));
        assert!(jitter.pop().is_some());
        assert!(jitter.pop().is_none(), "and stops when they do");
    }

    #[test]
    fn a_late_packet_is_dropped_and_sequence_numbers_wrap() {
        let mut jitter = Jitter::new();
        jitter.push(65_535, opus_silence());
        jitter.push(0, opus_silence());
        assert!(jitter.pop().is_some());
        assert_eq!(jitter.next, Some(0), "wrapped");
        jitter.push(65_534, opus_silence());
        assert!(!jitter.packets.contains_key(&65_534), "already behind what was played");
    }

    #[test]
    fn a_lost_packet_is_concealed_rather_than_skipped() {
        let mut jitter = Jitter::new();
        jitter.push(1, opus_silence());
        jitter.push(3, opus_silence());
        assert!(jitter.pop().is_some(), "1");
        assert!(jitter.pop().is_some(), "2 is missing and concealed");
        assert!(jitter.pop().is_some(), "3");
    }

    #[test]
    fn a_backlog_starts_again_from_the_newest() {
        let mut jitter = Jitter::new();
        for seq in 0..(JITTER_MAX as u16 + 1) {
            jitter.push(seq, opus_silence());
        }
        assert!(jitter.packets.len() <= JITTER_PRIME, "held {}", jitter.packets.len());
    }
}
