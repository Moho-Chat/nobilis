//! The voice connection a Go Live stream gets to itself.
//!
//! Hand-rolled, because songbird has no video path and because a stream is a
//! separate connection anyway - its own endpoint, token, key, SSRCs and UDP
//! session. Nothing here touches the channel connection songbird holds, so
//! audio keeps working exactly as it did whatever this does.
//!
//! The sequence, which is Discord's voice handshake with video asked for:
//!
//! 1. open the websocket to the stream's endpoint
//! 2. **op 0** identify, naming the stream's server and this account's
//!    session
//! 3. **op 8** hello comes back with a heartbeat interval; **op 3** every
//!    interval from then on
//! 4. **op 2** ready names our SSRCs and the UDP address to send to
//! 5. UDP IP discovery: one packet out, one back, saying how the far side
//!    sees us
//! 6. **op 1** select protocol, naming that address and the encryption mode
//! 7. **op 4** session description hands over the key
//! 8. **op 5**/**op 12** say what is about to be sent, and then RTP flows
//!
//! Steps 5 and 8 are the ones with a byte layout to get wrong, so they are
//! separated out and tested. The rest is a state machine whose only real
//! verification is a live connection.

use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::rtp;
use super::videorx::{self, VideoReceiver};
use super::voicecrypto::{self, Mode, Sealer};

/// Discord's voice gateway version. 8 is what carries the video fields.
const VOICE_VERSION: u8 = 8;

/// The IP discovery request, as Discord's voice UDP expects it.
///
/// Seventy-four bytes: a two-byte type, a two-byte length of everything after
/// it, our SSRC, then sixty-four bytes for an address and two for a port,
/// which we send empty and the server fills in. The length field counts the
/// seventy bytes after itself, not the whole packet - which is the kind of
/// off-by-four that produces a packet the server drops in silence.
/// Asks for a larger receive buffer than the system's default.
///
/// A keyframe arrives as a burst of a hundred packets or more in a few
/// milliseconds, and the default buffer (about 200KB on Linux) overflows
/// under it whenever the reader is a moment late - which drops packets out of
/// the one frame that everything after it depends on. Best effort: the
/// system caps the size at its own limit, and a refusal leaves the default.
pub fn widen_receive_buffer(udp: &UdpSocket) {
    let _ = socket2::SockRef::from(udp).set_recv_buffer_size(4 * 1024 * 1024);
}

pub fn discovery_request(ssrc: u32) -> [u8; 74] {
    let mut out = [0u8; 74];
    out[0..2].copy_from_slice(&1u16.to_be_bytes());
    out[2..4].copy_from_slice(&70u16.to_be_bytes());
    out[4..8].copy_from_slice(&ssrc.to_be_bytes());
    out
}

/// Reads the answer: how the far side sees this machine.
///
/// The address is a C string in a sixty-four byte field, so it is read to the
/// first zero rather than trimmed - a name that filled the field would
/// otherwise come back with whatever followed it.
pub fn discovery_answer(packet: &[u8]) -> Result<(String, u16)> {
    if packet.len() < 74 {
        anyhow::bail!("a discovery answer is 74 bytes, not {}", packet.len());
    }
    if u16::from_be_bytes([packet[0], packet[1]]) != 2 {
        anyhow::bail!("that is not a discovery answer");
    }
    let end = packet[8..72].iter().position(|b| *b == 0).unwrap_or(64);
    let address = std::str::from_utf8(&packet[8..8 + end]).context("the address was not text")?.to_string();
    if address.is_empty() {
        anyhow::bail!("the server named no address");
    }
    let port = u16::from_be_bytes([packet[72], packet[73]]);
    Ok((address, port))
}

/// What a stream connection is for.
///
/// The handshake is the same either way - the same identify, the same
/// protocol selection, the same DAVE group - and only the two ends differ:
/// a host announces a video SSRC and then writes to the socket, a viewer
/// announces nothing and reads from it. Keeping them on one path means the
/// half that is known to work is the half the other one uses.
#[derive(Debug, Clone, Copy)]
pub enum Role {
    Host,
    /// Whose stream is being watched. Needed rather than merely useful: MLS
    /// keys every sender separately, so a frame can only be decrypted by
    /// naming who sent it.
    Viewer { owner: u64 },
}

/// A connection that is up, in whichever direction it runs.
pub enum Connected {
    Sending(Arc<StreamSender>),
    Watching(Arc<Watching>),
}

/// A stream being received.
///
/// Nothing to call on it but `stop`: frames arrive on their own task and go
/// straight to the window as events, because a viewer has nothing to ask for
/// and nothing to wait on.
pub struct Watching {
    live: Arc<AtomicBool>,
}

impl Watching {
    pub fn stop(&self) {
        self.live.store(false, Ordering::Relaxed);
    }
}

/// One stream connection, once it is sending.
pub struct StreamSender {
    udp: Arc<UdpSocket>,
    sealer: Arc<Mutex<Sealer>>,
    video_ssrc: u32,
    sequence: AtomicU32,
    /// The stream's sound goes on the SSRC the server handed this
    /// connection in `op 2`; the picture has one of its own beside it.
    audio_ssrc: u32,
    /// Frames for the websocket from outside its loop - saying that sound is
    /// coming, which the far end needs before it will play any.
    outgoing: tokio::sync::mpsc::UnboundedSender<WsMessage>,
    /// Cleared when the connection goes, so a frame arriving from the window
    /// after a hangup is dropped rather than sent into a closed socket.
    live: Arc<AtomicBool>,
    /// The end-to-end encryption, shared with the websocket loop that keeps
    /// its group up to date. Absent only if the server declined to require
    /// it, which it currently never does.
    dave: Arc<Mutex<Option<super::dave::Dave>>>,
}

impl StreamSender {
    /// Sends one encoded frame, as however many packets it takes.
    pub async fn send_frame(&self, frame: &[u8], timestamp_micros: i64) -> Result<()> {
        if !self.live.load(Ordering::Relaxed) {
            return Ok(());
        }
        let timestamp = rtp::timestamp_from_micros(timestamp_micros);

        // Encrypted for the group before it is cut into packets, and sealed
        // for the wire after. Two different promises to two different
        // parties: DAVE hides the picture from Discord, and the packet
        // sealing hides it from everybody between here and Discord.
        //
        // A frame sent before the group is ready would be one nobody can
        // read, so it is dropped instead - the next keyframe is two seconds
        // away at most and a viewer arriving into a group that is still
        // forming is expected to wait.
        let encrypted;
        let frame = {
            let mut held = self.dave.lock().await;
            match held.as_mut() {
                Some(dave) if dave.ready() => {
                    encrypted = dave.encrypt_video(frame)?;
                    &encrypted[..]
                }
                Some(_) => return Ok(()),
                None => frame,
            }
        };
        // The range is reserved before anything is built, so two frames
        // handed in at once cannot interleave their sequence numbers - which
        // the far end would read as loss and answer by asking for a keyframe
        // over and over. Counted rather than measured after the fact,
        // because the count has to be known before the first number is taken.
        let needed = rtp::packet_count_vp8(frame);
        if needed == 0 {
            return Ok(());
        }
        let first = self.sequence.fetch_add(needed as u32, Ordering::Relaxed) as u16;
        let packets = rtp::packetise_vp8(frame, self.video_ssrc, first, timestamp);
        for packet in packets {
            let header = rtp::header(&packet);
            let sealed = {
                let mut sealer = self.sealer.lock().await;
                sealer.seal(&header, &packet.payload)?
            };
            self.udp.send(&sealed).await.context("sending a video packet")?;
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.live.store(false, Ordering::Relaxed);
    }

    /// Whether the connection is still up, ready or not.
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    /// Sends what the computer is playing along with the picture, until the
    /// stream ends.
    ///
    /// Recorded by this process rather than the window: Chromium cannot record
    /// a Linux desktop's sound at all, and doing it here is one path for every
    /// desktop the recording works on (see `audio::start_system_capture`).
    /// Encoded for music rather than speech, and sent continuously rather than
    /// gated - a quiet passage in a film is still part of the film.
    pub fn start_audio(self: Arc<Self>, account_id: String) {
        tokio::spawn(async move {
            let (source, sink) = crate::audio::MicSource::new();
            let capture = match crate::audio::start_system_capture(sink) {
                Ok(capture) => capture,
                Err(e) => {
                    tracing::warn!("discord[{account_id}]: the stream goes without sound: {e:#}");
                    return;
                }
            };
            let mut encoder = match opus2::Encoder::new(48_000, opus2::Channels::Stereo, opus2::Application::Audio) {
                Ok(encoder) => encoder,
                Err(e) => {
                    tracing::warn!("discord[{account_id}]: no Opus encoder for the stream's sound: {e:?}");
                    return;
                }
            };
            let _ = encoder.set_bitrate(opus2::Bitrate::Bits(128_000));
            let _ = self.outgoing.send(WsMessage::Text(
                // Speaking flag 2 is "soundshare": a stream's sound, not somebody's
                // microphone. Flag 1 is what a voice call sends, and a viewer
                // does not play a stream's audio marked as that - which is
                // exactly what the phone's own stream sends, read off the wire.
                json!({ "op": 5, "d": { "speaking": 2, "delay": 0, "ssrc": self.audio_ssrc } }).to_string(),
            ));
            tracing::info!("discord[{account_id}]: the stream is sending sound");

            let mut tick = tokio::time::interval(std::time::Duration::from_millis(20));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut sequence: u16 = rand::random();
            let mut timestamp: u32 = rand::random();
            let mut sent = 0u64;
            while self.live.load(Ordering::Relaxed) {
                tick.tick().await;
                timestamp = timestamp.wrapping_add(rtp::OPUS_FRAME_SAMPLES as u32);
                let Some(pcm) = source.take(rtp::OPUS_FRAME_SAMPLES * 2) else { continue };
                let mut opus = vec![0u8; 1275];
                let Ok(length) = encoder.encode_float(&pcm, &mut opus) else { continue };
                opus.truncate(length);
                // As for the picture: nothing until the group can read it.
                let body = {
                    let mut held = self.dave.lock().await;
                    match held.as_mut() {
                        Some(dave) if dave.ready() => match dave.encrypt_opus(&opus) {
                            Ok(body) => body,
                            Err(_) => continue,
                        },
                        Some(_) => continue,
                        None => opus,
                    }
                };
                let header = rtp::header(&rtp::Packet {
                    payload_type: rtp::PAYLOAD_TYPE_OPUS,
                    sequence,
                    timestamp,
                    ssrc: self.audio_ssrc,
                    marker: false,
                    payload: Vec::new(),
                });
                sequence = sequence.wrapping_add(1);
                let sealed = {
                    let mut sealer = self.sealer.lock().await;
                    sealer.seal(&header, &body)
                };
                if let Ok(packet) = sealed {
                    if self.udp.send(&packet).await.is_ok() {
                        sent += 1;
                    }
                }
            }
            drop(capture);
            tracing::info!("discord[{account_id}]: the stream's sound stopped after {sent} frames");
        });
    }
}

/// Opens the connection and runs it until it ends.
///
/// Returns the sender as soon as there is something to send with; the
/// websocket keeps running behind it, heart-beating and listening, until the
/// stream ends or the socket does.
#[allow(clippy::too_many_arguments)]
pub async fn connect(
    state: &AppState,
    account_id: &str,
    endpoint: &str,
    token: &str,
    server_id: &str,
    // The channel the *stream* is on, which STREAM_CREATE names separately
    // from the conversation's. The DAVE group is built from it, and a group
    // named after the wrong channel is one nobody else is in.
    rtc_channel_id: &str,
    session_id: &str,
    user_id: &str,
    role: Role,
    // Named for the events a viewer's frames arrive under, so the window can
    // tell two streams apart. Unused by a host, which has nothing to report.
    stream_key: &str,
) -> Result<Connected> {
    // Voice and video travel over UDP, which neither Tor nor a SOCKS5 proxy
    // carries. Refused rather than sent directly: an account routed through
    // Tor that quietly called out on its real address would be worse than
    // one that says it cannot call.
    if crate::net::route::router().routed(account_id) {
        anyhow::bail!("calls and screen sharing need UDP, which Tor and SOCKS5 proxies can't carry - turn off Tor for this account to use them");
    }
    let url = format!("wss://{}/?v={VOICE_VERSION}", endpoint.trim_end_matches(":443"));
    let (socket, _) = tokio_tungstenite::connect_async(url.as_str()).await.context("opening the stream socket")?;
    let (mut write, mut read) = socket.split();

    tracing::info!(
        "discord[{account_id}]: identifying to {endpoint} as server {server_id}, session {}…, user {user_id}",
        session_id.chars().take(6).collect::<String>()
    );
    write
        .send(WsMessage::Text(
            json!({
                "op": 0,
                "d": {
                    "server_id": server_id,
                    "user_id": user_id,
                    "session_id": session_id,
                    "token": token,
                    // The whole point of this connection. A stream connection
                    // that identifies without it is given audio SSRCs and no
                    // video one, and there is then nowhere to put a picture.
                    "video": true,
                    // Which version of DAVE this end speaks. Declaring zero
                    // - "not this one" - was refused with 4017 just as
                    // firmly as declaring nothing, so a Go Live stream
                    // genuinely has to be end-to-end encrypted.
                    "max_dave_protocol_version": davey::DAVE_PROTOCOL_VERSION,
                    "streams": [{ "type": "video", "rid": "100", "quality": 100 }],
                }
            })
            .to_string(),
        ))
        .await
        .context("identifying")?;

    let mut ssrc = 0u32;
    let mut video_ssrc = 0u32;
    let mut address = String::new();
    let mut port = 0u16;
    let mut modes: Vec<String> = Vec::new();
    let mut beat_every = std::time::Duration::from_millis(13_750);
    // Which opcodes came, so a failure can say how far it got rather than
    // only that it did not finish.
    let mut saw: Vec<u64> = Vec::new();

    // Up to the point there is a key, the handshake is a short sequence with
    // a definite end; after it, the socket is a loop. Read it as the former
    // first.
    while let Some(frame) = read.next().await {
        let frame = frame.context("the stream socket failed")?;
        // The whole of why a handshake failed is in here. A voice gateway
        // refuses in numbers - 4004 is a token it would not take, 4011 a
        // server it could not find, 4016 an encryption mode it does not
        // know - and each points at a different mistake.
        if let WsMessage::Close(reason) = &frame {
            let said = reason
                .as_ref()
                .map(|r| format!("{} {}", u16::from(r.code), r.reason))
                .unwrap_or_else(|| "with no reason given".to_string());
            // 4017 is worth translating. "E2EE protocol required" names a
            // protocol nobody has heard of; what it means is that this
            // conversation will not carry a picture that the server itself
            // could watch.
            if reason.as_ref().map(|r| u16::from(r.code)) == Some(4017) {
                anyhow::bail!(
                    "this call requires Discord's end-to-end encryption (DAVE), which moho's stream connection does not speak yet"
                );
            }
            anyhow::bail!("the stream server closed the connection: {said}");
        }
        let WsMessage::Text(text) = frame else { continue };
        let message: Value = serde_json::from_str(&text).context("the stream server sent something odd")?;
        // Every frame of the handshake, at a level somebody is running with.
        // Four of them, once per stream: saying them costs nothing, and not
        // saying them cost two live attempts that could report only that
        // nothing had arrived.
        saw.push(message["op"].as_u64().unwrap_or(u64::MAX));
        tracing::info!("discord[{account_id}]: stream op {} {}", message["op"], super::gateway::brief(&message["d"]));
        match message["op"].as_u64() {
            // Hello. The heartbeat starts from here rather than after the
            // handshake: a voice gateway expects one from this moment, and a
            // handshake that pauses on a UDP round trip is one the server is
            // entitled to give up on.
            Some(8) => {
                let interval = message["d"]["heartbeat_interval"].as_f64().unwrap_or(41_250.0);
                beat_every = std::time::Duration::from_millis(interval.max(500.0) as u64);
            }
            // Ready: our SSRCs, and where to send.
            Some(2) => {
                let d = &message["d"];
                ssrc = d["ssrc"].as_u64().unwrap_or(0) as u32;
                address = d["ip"].as_str().unwrap_or_default().to_string();
                port = d["port"].as_u64().unwrap_or(0) as u16;
                modes = d["modes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m.as_str().map(str::to_string))
                    .collect();
                // The video SSRC is in the streams list rather than beside
                // the audio one; a connection that used the audio SSRC for
                // video would be ignored packet for packet.
                video_ssrc = d["streams"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|s| s["type"].as_str() == Some("video"))
                    .and_then(|s| s["ssrc"].as_u64())
                    .unwrap_or(ssrc.wrapping_add(1) as u64) as u32;
                break;
            }
            _ => {}
        }
    }

    if ssrc == 0 || address.is_empty() || port == 0 {
        // How far it got, which is the difference between a rejected
        // identify and a handshake that went wrong later. An empty list is a
        // socket that opened and said nothing at all; a list with 8 in it and
        // no 2 is Discord accepting the connection and then refusing what we
        // identified as.
        return Err(anyhow!(
            "the stream server never said where to send - it sent {}",
            if saw.is_empty() { "nothing at all".to_string() } else { format!("op(s) {saw:?}") }
        ));
    }
    let mode = Mode::negotiate(&modes).ok_or_else(|| anyhow!("no encryption mode this client speaks: {modes:?}"))?;

    // The UDP session, and the round trip that says how the far side sees us.
    let udp = UdpSocket::bind("0.0.0.0:0").await.context("opening a socket")?;
    widen_receive_buffer(&udp);
    udp.connect((address.as_str(), port)).await.context("pointing the socket at the stream server")?;
    udp.send(&discovery_request(ssrc)).await.context("asking how we are seen")?;
    let mut answer = [0u8; 74];
    let read_len = tokio::time::timeout(std::time::Duration::from_secs(5), udp.recv(&mut answer))
        .await
        .context("the stream server did not answer the discovery packet")??;
    let (public_address, public_port) = discovery_answer(&answer[..read_len])?;

    write
        .send(WsMessage::Text(
            json!({
                "op": 1,
                "d": {
                    "protocol": "udp",
                    "data": { "address": public_address, "port": public_port, "mode": mode.wire_name() },
                    "codecs": [
                        { "name": "opus", "type": "audio", "priority": 1000, "payload_type": 120 },
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
        let frame = frame.context("the stream socket failed")?;
        if let WsMessage::Close(reason) = &frame {
            let said = reason
                .as_ref()
                .map(|r| format!("{} {}", u16::from(r.code), r.reason))
                .unwrap_or_else(|| "with no reason given".to_string());
            anyhow::bail!("the stream server closed after being told how to reach us: {said}");
        }
        let WsMessage::Text(text) = frame else { continue };
        let message: Value = serde_json::from_str(&text).unwrap_or_default();
        tracing::info!("discord[{account_id}]: stream op {} {}", message["op"], super::gateway::brief(&message["d"]));
        if message["op"].as_u64() == Some(4) {
            key = message["d"]["secret_key"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b.as_u64().map(|b| b as u8))
                .collect();
            // The version actually in force, which is the server's decision
            // rather than what the identify asked for.
            dave_version = message["d"]["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
            break;
        }
    }
    if key.len() != 32 {
        return Err(anyhow!("the stream server sent no usable key"));
    }

    // The group this stream's media belongs to. Built from the stream's own
    // channel rather than the conversation's: a Go Live session is its own
    // group, and naming the wrong channel builds one nobody else is in.
    let dave = Arc::new(Mutex::new(super::dave::Dave::new(dave_version, user_id, rtc_channel_id)?));
    if let Some(package) = {
        let mut held = dave.lock().await;
        match held.as_mut() {
            Some(session) => Some(session.key_package()?),
            None => None,
        }
    } {
        tracing::info!(
            "discord[{account_id}]: DAVE v{dave_version}, asking to join the group for channel {rtc_channel_id} with a {}-byte key package",
            package.len()
        );
        write
            .send(WsMessage::Binary(super::dave::write_binary(super::dave::OP_KEY_PACKAGE, &package)))
            .await
            .context("sending the key package")?;
    }

    // What is about to arrive, so the far end expects a picture rather than
    // discarding packets on an SSRC it was never told about.
    //
    // Held until the encrypted group exists. Announcing video on a
    // connection that cannot yet encrypt any is announcing something that
    // will not come, and the ordering is the one thing left that could be
    // provoking the server into dropping us.
    // A viewer sends no media, so it claims no SSRCs. Announcing a video
    // SSRC it will never put a packet on would tell the server to expect a
    // second picture in a conversation that has one.
    // At whatever the share was started at - the window encodes to the same
    // numbers, so what the server tells viewers to expect is what arrives.
    let quality = super::golive::quality(account_id);
    let announce_video = json!({
        "op": 12,
        "d": {
            // The sound, if any is sent, arrives on this connection's own
            // SSRC. Announced whether or not it comes: a stream announced
            // with no audio SSRC has nowhere for its sound to go.
            "audio_ssrc": ssrc,
            "video_ssrc": video_ssrc,
            "rtx_ssrc": video_ssrc.wrapping_add(1),
            "streams": [{
                "type": "video", "rid": "100", "ssrc": video_ssrc, "active": true,
                "quality": 100, "rtx_ssrc": video_ssrc.wrapping_add(1),
                "max_bitrate": quality.bitrate, "max_framerate": quality.framerate,
                "max_resolution": { "type": "fixed", "width": quality.width, "height": quality.height }
            }],
        }
    })
    .to_string();

    let live = Arc::new(AtomicBool::new(true));
    // Held back from the tasks below, which each take their own handle: the
    // value returned has to be able to stop them after they have started.
    let watching = live.clone();
    let udp = Arc::new(udp);
    // One sealer for everything this connection sends - the picture, the
    // sound, and a viewer's requests for resends - because they share a key,
    // and two counters under one key could one day send the same nonce twice.
    let sealer = Arc::new(Mutex::new(Sealer::new(mode, &key)?));
    let (outgoing, mut to_socket) = tokio::sync::mpsc::unbounded_channel::<WsMessage>();
    let sender = Arc::new(StreamSender {
        udp: udp.clone(),
        sealer: sealer.clone(),
        video_ssrc,
        sequence: AtomicU32::new(rand::random::<u16>() as u32),
        audio_ssrc: ssrc,
        outgoing,
        live: live.clone(),
        dave: dave.clone(),
    });

    // Who is sending on which SSRC, learned from the server rather than
    // assumed. The server describes other people's streams in its own op 12
    // frames, and a viewer needs the mapping both to know which packets are
    // the picture and to name the sender when decrypting.
    let senders: Arc<Mutex<HashMap<u32, u64>>> = Arc::new(Mutex::new(HashMap::new()));
    // Retransmission SSRCs, to the picture they resend for.
    let resends: Arc<Mutex<HashMap<u32, u32>>> = Arc::new(Mutex::new(HashMap::new()));

    if let Role::Viewer { owner } = role {
        tokio::spawn(watch_socket(
            state.clone(),
            account_id.to_string(),
            stream_key.to_string(),
            udp.clone(),
            mode,
            key.clone(),
            dave.clone(),
            senders.clone(),
            resends.clone(),
            sealer.clone(),
            ssrc,
            owner,
            live.clone(),
        ));
    }

    // The socket has to keep being read and heart-beaten for the connection
    // to stay up; nothing else is waiting on it, so it runs on its own.
    let account = account_id.to_string();
    tokio::spawn(async move {
        let mut beat = tokio::time::interval(beat_every);
        // The last sequence number the server sent, which every v8 heartbeat
        // has to acknowledge. Binary frames carry it in their first two
        // bytes; JSON ones carry it as `seq`.
        let seq_ack = Arc::new(AtomicU32::new(0));
        // Sent once, the first moment there is a group to encrypt for.
        let mut announced = false;
        loop {
            tokio::select! {
                _ = beat.tick() => {
                    // Voice gateway v8's heartbeat, which is not v4's.
                    //
                    // v4 takes a bare nonce: {"op":3,"d":<number>}. v8 takes
                    // an object carrying the nonce and the last sequence
                    // number seen, because v8 added buffered resume and the
                    // server needs to know how far this client got. Sending
                    // v4's shape on a v8 socket is a protocol violation the
                    // server answers by invalidating the session - which is
                    // the 4006 that arrived a few seconds into every
                    // connection, whatever else had just been sent.
                    //
                    // songbird speaks v4, so its heartbeat was the wrong
                    // thing to copy onto this connection.
                    let beat_frame = json!({
                        "op": 3,
                        "d": { "t": rand::random::<u32>(), "seq_ack": seq_ack.load(Ordering::Relaxed) }
                    });
                    if write.send(WsMessage::Text(beat_frame.to_string())).await.is_err() {
                        break;
                    }
                }
                Some(message) = to_socket.recv() => {
                    if write.send(message).await.is_err() {
                        break;
                    }
                }
                frame = read.next() => {
                    match frame {
                        // Why the connection ended, which the handshake
                        // loops above say and this one used to swallow - it
                        // passed a Close to the DAVE handler, got nothing
                        // back, and read again until the stream ended. So a
                        // refusal after the group had started forming looked
                        // exactly like a socket quietly going away.
                        Some(Ok(WsMessage::Close(reason))) => {
                            let said = reason
                                .as_ref()
                                .map(|r| format!("{} {}", u16::from(r.code), r.reason))
                                .unwrap_or_else(|| "with no reason given".to_string());
                            tracing::warn!("discord[{account}]: the stream server closed the connection: {said}");
                            break;
                        }
                        // DAVE is server-driven: the group is rebuilt as
                        // people come and go, and a client's whole job is to
                        // answer correctly and promptly. A connection that
                        // stops answering keeps its socket and stops being
                        // able to encrypt anything.
                        Some(Ok(incoming)) => {
                            note_sequence(&incoming, &seq_ack);
                            note_senders(&incoming, &senders, &resends).await;
                            if let Some(reply) = answer_dave(&dave, &incoming, &account).await {
                                if write.send(reply).await.is_err() {
                                    break;
                                }
                            }
                            // The moment the group exists, say what is about
                            // to be sent on it. A viewer has nothing to say:
                            // it is here to receive.
                            if matches!(role, Role::Host) && !announced && group_ready(&dave).await {
                                announced = true;
                                let epoch = match dave.lock().await.as_ref() {
                                    Some(d) => d.epoch(),
                                    None => None,
                                };
                                tracing::info!(
                                    "discord[{account}]: the group is ready at epoch {epoch:?}; announcing the video stream"
                                );
                                if write.send(WsMessage::Text(announce_video.clone())).await.is_err() {
                                    break;
                                }
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
        tracing::info!("discord[{account}]: the stream connection closed");
        live.store(false, Ordering::Relaxed);
    });

    Ok(match role {
        Role::Host => Connected::Sending(sender),
        Role::Viewer { .. } => Connected::Watching(Arc::new(Watching { live: watching })),
    })
}

/// Reads the server's own op 12 frames for who is sending on which SSRC.
///
/// The server describes every other participant's streams this way, and it is
/// the only place the mapping exists: a packet carries an SSRC and nothing
/// else identifying, so without this a viewer knows a picture is arriving and
/// not whose it is - which under MLS means it cannot be decrypted at all.
async fn note_senders(
    incoming: &WsMessage,
    senders: &Arc<Mutex<HashMap<u32, u64>>>,
    resends: &Arc<Mutex<HashMap<u32, u32>>>,
) {
    let WsMessage::Text(text) = incoming else { return };
    let Ok(message) = serde_json::from_str::<Value>(text) else { return };
    if message["op"].as_u64() != Some(12) {
        return;
    }
    let d = &message["d"];
    // Discord writes ids as strings everywhere else and as numbers here, so
    // both are read rather than the one that happened to arrive first.
    let Some(user) = d["user_id"].as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| d["user_id"].as_u64()) else {
        return;
    };
    resends.lock().await.extend(videorx::rtx_pairs(d));
    let mut held = senders.lock().await;
    if let Some(ssrc) = d["video_ssrc"].as_u64().filter(|s| *s != 0) {
        held.insert(ssrc as u32, user);
    }
    // A stream can be re-announced on a new SSRC when quality changes, and
    // the entries in `streams` are where that arrives first.
    for stream in d["streams"].as_array().into_iter().flatten() {
        if let Some(ssrc) = stream["ssrc"].as_u64().filter(|s| *s != 0) {
            held.insert(ssrc as u32, user);
        }
    }
}

/// Reads the socket for as long as the stream lasts, and hands the window
/// whole frames.
///
/// The exact mirror of `StreamSender::send_frame`, run backwards: open each
/// packet, reassemble the frame it belongs to, decrypt the frame for the
/// group. Doing the two decryptions in the wrong order produces nothing that
/// looks like an error - the packet opens, the bytes are the right length,
/// and the decoder is handed noise - so the order here is the same one the
/// sender used, read bottom to top.
#[allow(clippy::too_many_arguments)]
async fn watch_socket(
    state: AppState,
    account_id: String,
    stream_key: String,
    udp: Arc<UdpSocket>,
    mode: Mode,
    key: Vec<u8>,
    dave: Arc<Mutex<Option<super::dave::Dave>>>,
    senders: Arc<Mutex<HashMap<u32, u64>>>,
    resends: Arc<Mutex<HashMap<u32, u32>>>,
    sealer: Arc<Mutex<Sealer>>,
    ssrc: u32,
    owner: u64,
    live: Arc<AtomicBool>,
) {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    // One per SSRC. Two people sending into the same connection do not share
    // a sequence space, and treating them as if they did would read every
    // other packet as a gap.
    let mut pictures: HashMap<u32, VideoReceiver> = HashMap::new();
    // A datagram is at most an MTU, and the sender caps its payload well
    // under one; this is roomy rather than tight.
    let mut buffer = vec![0u8; 4096];
    // Counted, not logged one by one. A stream that cannot be decrypted
    // produces one of these per packet - thousands a second - and a log line
    // each is how a client stops responding entirely.
    let mut unopened = 0u64;
    let mut undecrypted = 0u64;
    let mut frames = 0u64;
    let mut seen = 0u32;
    let mut unparsed = 0u64;
    let mut types: HashMap<u8, u64> = HashMap::new();
    let mut sources: HashMap<u32, u64> = HashMap::new();

    let send_rtcp = |packet: Vec<u8>| {
        let udp = udp.clone();
        let sealer = sealer.clone();
        async move {
            let sealed = videorx::seal_rtcp(&mut *sealer.lock().await, &packet);
            if let Ok(sealed) = sealed {
                let _ = udp.send(&sealed).await;
            }
        }
    };

    tracing::info!("discord[{account_id}]: watching {stream_key}, owned by {owner}");
    while live.load(Ordering::Relaxed) {
        let read = tokio::time::timeout(std::time::Duration::from_secs(30), udp.recv(&mut buffer)).await;
        let length = match read {
            Ok(Ok(length)) => length,
            // Not an error worth ending on by itself: a paused stream sends
            // nothing, and the websocket is what says whether it is still
            // there.
            Err(_) => {
                tracing::debug!("discord[{account_id}]: nothing on {stream_key} for thirty seconds");
                continue;
            }
            Ok(Err(e)) => {
                tracing::warn!("discord[{account_id}]: the stream socket failed: {e}");
                break;
            }
        };
        let datagram = &buffer[..length];
        if seen < 3 {
            seen += 1;
            let head: String = datagram.iter().take(24).map(|b| format!("{b:02x}")).collect();
            tracing::info!(
                "discord[{account_id}]: datagram {seen} on {stream_key}: {length} bytes, first 24: {head}"
            );
        }
        if videorx::is_rtcp(datagram) {
            *types.entry(datagram[1] & 0x7f).or_insert(0u64) += 1;
            continue;
        }
        // Not everything arriving here is a packet. The discovery answer and
        // whatever else the server sends would otherwise be fed to a decoder.
        let Some(parsed) = rtp::parse_header(datagram) else {
            unparsed += 1;
            continue;
        };
        // Counted by type, so a stream arriving under a payload type this
        // does not expect is visible rather than silently discarded - which
        // is indistinguishable, from the outside, from no stream at all.
        *types.entry(parsed.payload_type).or_insert(0u64) += 1;
        *sources.entry(parsed.ssrc).or_insert(0u64) += 1;
        if parsed.payload_type != rtp::PAYLOAD_TYPE_VP8 && parsed.payload_type != videorx::PAYLOAD_TYPE_RTX {
            continue;
        }
        // Discord's own rule for where the clear header ends, the same one a
        // voice call's camera arrives under. The three guesses this used to
        // try between all left the extension's preamble out of one span or
        // the other, and a phone's stream - which carries an extension on
        // every packet - opened under none of them.
        let Ok(payload) = voicecrypto::open_rtp(mode, &key, datagram, &parsed) else {
            unopened += 1;
            continue;
        };
        let (video_ssrc, sequence, payload, resent) = if parsed.payload_type == videorx::PAYLOAD_TYPE_RTX {
            let known = resends.lock().await.get(&parsed.ssrc).copied();
            let Some(media) = known.or_else(|| {
                let guess = parsed.ssrc.wrapping_sub(1);
                pictures.contains_key(&guess).then_some(guess)
            }) else {
                continue;
            };
            let Some((sequence, original)) = videorx::unwrap_rtx(&payload) else { continue };
            (media, sequence, original, true)
        } else {
            (parsed.ssrc, parsed.sequence, payload, false)
        };
        let now = std::time::Instant::now();
        let picture_rx = pictures.entry(video_ssrc).or_default();
        let whole = picture_rx.push(
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
        let lost = picture_rx.due_for_asking(now);
        if !lost.is_empty() {
            send_rtcp(videorx::nack(ssrc, video_ssrc, &lost)).await;
        }
        for frame in whole {
            // Whoever the server said owns this SSRC, and the stream's owner
            // if it has not said yet - which is right for a viewer
            // connection, since it carries one person's stream.
            let from = senders.lock().await.get(&video_ssrc).copied().unwrap_or(owner);
            let picture = {
                let mut held = dave.lock().await;
                match held.as_mut() {
                    Some(session) => match session.decrypt_video(from, &frame.data) {
                        Ok(picture) => picture,
                        Err(_) => {
                            undecrypted += 1;
                            continue;
                        }
                    },
                    None => frame.data.clone(),
                }
            };
            let keyframe = super::reassemble::is_keyframe(&picture);
            let Some(picture_rx) = pictures.get_mut(&video_ssrc) else { continue };
            let first = picture_rx.stats.frames == 0;
            let admitted = picture_rx.admit(&frame, keyframe);
            if picture_rx.wants_keyframe(now) {
                send_rtcp(videorx::pli(ssrc, video_ssrc)).await;
            }
            if !admitted {
                continue;
            }
            if first {
                tracing::info!("discord[{account_id}]: the first keyframe of {stream_key} arrived");
            }
            frames += 1;
            if frames % 900 == 1 {
                tracing::info!(
                    "discord[{account_id}]: {stream_key}: {frames} frames, {unopened} packets that would not open, {undecrypted} frames that would not decrypt; {:?}",
                    picture_rx.stats
                );
            }
            state.events.emit(
                "discordStreamFrame",
                json!({
                    "accountId": account_id,
                    "streamKey": stream_key,
                    "keyframe": keyframe,
                    // Microseconds, which is what a WebCodecs decoder takes.
                    // RTP counts video in 90kHz ticks.
                    "timestampMicros": (frame.timestamp as u64 * 1_000_000) / rtp::VIDEO_CLOCK_HZ,
                    "frame": STANDARD.encode(&picture),
                }),
            );
        }
    }
    for (source, picture_rx) in &pictures {
        tracing::info!("discord[{account_id}]: {stream_key}: picture on {source}: {:?}", picture_rx.stats);
    }
    let mut by_type: Vec<_> = types.into_iter().collect();
    by_type.sort();
    let mut by_source: Vec<_> = sources.into_iter().collect();
    by_source.sort();
    tracing::info!(
        "discord[{account_id}]: stopped watching {stream_key} after {frames} frames ({unopened} unopened, {undecrypted} undecrypted, {unparsed} not RTP); payload types {by_type:?}; ssrcs {by_source:?}"
    );
    live.store(false, Ordering::Relaxed);
    state.events.emit(
        "discordStreamFrame",
        json!({ "accountId": account_id, "streamKey": stream_key, "ended": true }),
    );
}

/// Remembers how far the server has got, for the next heartbeat to
/// acknowledge.
///
/// Only ever forward. Frames are read on one task and the heartbeat fires on
/// the same one, but an out-of-order acknowledgement would tell the server
/// this client had lost ground it has not lost.
fn note_sequence(incoming: &WsMessage, seq_ack: &Arc<AtomicU32>) {
    let seq = match incoming {
        WsMessage::Binary(bytes) if bytes.len() >= 2 => u32::from(u16::from_be_bytes([bytes[0], bytes[1]])),
        WsMessage::Text(text) => match serde_json::from_str::<Value>(text) {
            Ok(message) => match message["seq"].as_u64() {
                Some(seq) => seq as u32,
                None => return,
            },
            Err(_) => return,
        },
        _ => return,
    };
    seq_ack.fetch_max(seq, Ordering::Relaxed);
}

/// Whether the end-to-end encrypted group has formed.
async fn group_ready(dave: &Arc<Mutex<Option<super::dave::Dave>>>) -> bool {
    match dave.lock().await.as_ref() {
        Some(dave) => dave.ready(),
        // No group asked for, so there is nothing to wait on.
        None => true,
    }
}

/// Takes in one frame on a running stream connection and says what, if
/// anything, has to go back.
///
/// Separate from the loop so that the lock on the group is held for exactly
/// as long as it takes to process a frame, and never across a socket write -
/// the sender takes the same lock for every frame of video, and a write that
/// blocked while holding it would stall the picture.
async fn answer_dave(
    dave: &Arc<Mutex<Option<super::dave::Dave>>>,
    incoming: &WsMessage,
    account_id: &str,
) -> Option<WsMessage> {
    use super::dave::Reply;

    let reply = {
        let mut held = dave.lock().await;
        let session = held.as_mut()?;
        match incoming {
            WsMessage::Binary(bytes) => {
                let frame = super::dave::read_binary(bytes)?;
                // Every DAVE frame, by name, at a level somebody runs with.
                // Whether Discord answers a key package at all is the
                // difference between a group that is forming and one that
                // was refused, and nothing else in the log distinguishes
                // them.
                tracing::info!(
                    "discord[{account_id}]: DAVE in: op {} ({} bytes), ready={}",
                    frame.opcode,
                    frame.payload.len(),
                    session.ready()
                );
                // Nobody is named: this client does not track who is
                // watching a stream, and an empty set would mean "nobody
                // belongs in this group" rather than "no opinion".
                session.take_binary(&frame, None)
            }
            WsMessage::Text(text) => {
                let message: Value = serde_json::from_str(text).ok()?;
                let op = message["op"].as_u64()?;
                session.take_json(op, &message["d"])
            }
            _ => Reply::Nothing,
        }
    };

    match reply {
        Reply::Nothing => None,
        Reply::Binary(opcode, payload) => {
            tracing::debug!("discord[{account_id}]: DAVE answering with binary op {opcode}");
            Some(WsMessage::Binary(super::dave::write_binary(opcode, &payload)))
        }
        Reply::Json(value) => {
            tracing::debug!("discord[{account_id}]: DAVE answering with {}", value["op"]);
            Some(WsMessage::Text(value.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seventy-four bytes, and a length field counting the seventy after
    /// itself rather than the whole packet - the off-by-four that produces a
    /// packet the server drops without a word.
    #[test]
    fn a_discovery_request_is_the_shape_the_server_expects() {
        let packet = discovery_request(0xDEADBEEF);
        assert_eq!(packet.len(), 74);
        assert_eq!(&packet[0..2], &[0, 1], "type 1 is a request");
        assert_eq!(&packet[2..4], &[0, 70], "the length counts what follows it");
        assert_eq!(&packet[4..8], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(packet[8..].iter().all(|b| *b == 0), "the rest is for the server to fill in");
    }

    /// The address is a C string in a 64-byte field. Trimming instead of
    /// reading to the first zero gives an address with rubbish on the end.
    #[test]
    fn a_discovery_answer_reads_the_address_to_its_terminator() {
        let mut packet = [0u8; 74];
        packet[0..2].copy_from_slice(&2u16.to_be_bytes());
        packet[2..4].copy_from_slice(&70u16.to_be_bytes());
        packet[4..8].copy_from_slice(&7u32.to_be_bytes());
        packet[8..8 + 11].copy_from_slice(b"203.0.113.7");
        // Whatever the server left in the rest of the field.
        packet[8 + 12] = b'x';
        packet[72..74].copy_from_slice(&50_001u16.to_be_bytes());

        let (address, port) = discovery_answer(&packet).expect("an answer");
        assert_eq!(address, "203.0.113.7");
        assert_eq!(port, 50_001);
    }

    #[test]
    fn anything_that_is_not_an_answer_is_refused() {
        assert!(discovery_answer(&[0u8; 20]).is_err(), "too short");

        // A request echoed back is not an answer.
        assert!(discovery_answer(&discovery_request(1)).is_err());

        // An answer naming nothing is not one either: an empty address would
        // otherwise be sent back to the server as where to reach us.
        let mut empty = [0u8; 74];
        empty[0..2].copy_from_slice(&2u16.to_be_bytes());
        assert!(discovery_answer(&empty).is_err());
    }
}
