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
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::rtp;
use super::voicecrypto::{Mode, Sealer};

/// Discord's voice gateway version. 8 is what carries the video fields.
const VOICE_VERSION: u8 = 8;

/// The IP discovery request, as Discord's voice UDP expects it.
///
/// Seventy-four bytes: a two-byte type, a two-byte length of everything after
/// it, our SSRC, then sixty-four bytes for an address and two for a port,
/// which we send empty and the server fills in. The length field counts the
/// seventy bytes after itself, not the whole packet - which is the kind of
/// off-by-four that produces a packet the server drops in silence.
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

/// One stream connection, once it is sending.
pub struct StreamSender {
    udp: Arc<UdpSocket>,
    sealer: Mutex<Sealer>,
    video_ssrc: u32,
    sequence: AtomicU32,
    /// Cleared when the connection goes, so a frame arriving from the window
    /// after a hangup is dropped rather than sent into a closed socket.
    live: Arc<AtomicBool>,
}

impl StreamSender {
    /// Sends one encoded frame, as however many packets it takes.
    pub async fn send_frame(&self, frame: &[u8], timestamp_micros: i64) -> Result<()> {
        if !self.live.load(Ordering::Relaxed) {
            return Ok(());
        }
        let timestamp = rtp::timestamp_from_micros(timestamp_micros);
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
}

/// Opens the connection and runs it until it ends.
///
/// Returns the sender as soon as there is something to send with; the
/// websocket keeps running behind it, heart-beating and listening, until the
/// stream ends or the socket does.
pub async fn connect(
    state: &AppState,
    account_id: &str,
    endpoint: &str,
    token: &str,
    server_id: &str,
    session_id: &str,
    user_id: &str,
) -> Result<Arc<StreamSender>> {
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
            break;
        }
    }
    if key.len() != 32 {
        return Err(anyhow!("the stream server sent no usable key"));
    }

    // What is about to arrive, so the far end expects a picture rather than
    // discarding packets on an SSRC it was never told about.
    write
        .send(WsMessage::Text(
            json!({
                "op": 12,
                "d": {
                    "audio_ssrc": 0,
                    "video_ssrc": video_ssrc,
                    "rtx_ssrc": video_ssrc.wrapping_add(1),
                    "streams": [{
                        "type": "video", "rid": "100", "ssrc": video_ssrc, "active": true,
                        "quality": 100, "rtx_ssrc": video_ssrc.wrapping_add(1),
                        "max_bitrate": 2_500_000, "max_framerate": 30,
                        "max_resolution": { "type": "fixed", "width": 1280, "height": 720 }
                    }],
                }
            })
            .to_string(),
        ))
        .await
        .context("announcing the video stream")?;

    let live = Arc::new(AtomicBool::new(true));
    let sender = Arc::new(StreamSender {
        udp: Arc::new(udp),
        sealer: Mutex::new(Sealer::new(mode, &key)?),
        video_ssrc,
        sequence: AtomicU32::new(rand::random::<u16>() as u32),
        live: live.clone(),
    });

    // The socket has to keep being read and heart-beaten for the connection
    // to stay up; nothing else is waiting on it, so it runs on its own.
    let account = account_id.to_string();
    tokio::spawn(async move {
        let mut beat = tokio::time::interval(beat_every);
        loop {
            tokio::select! {
                _ = beat.tick() => {
                    let nonce = rand::random::<u32>();
                    if write.send(WsMessage::Text(json!({ "op": 3, "d": nonce }).to_string())).await.is_err() {
                        break;
                    }
                }
                frame = read.next() => {
                    match frame {
                        Some(Ok(_)) => {}
                        _ => break,
                    }
                }
            }
        }
        tracing::info!("discord[{account}]: the stream connection closed");
        live.store(false, Ordering::Relaxed);
    });

    let _ = state;
    Ok(sender)
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
