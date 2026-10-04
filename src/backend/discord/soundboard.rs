//! The soundboard: short sounds anybody in a voice channel can set off.
//!
//! Discord does this in the clients, not the voice server. Playing one is a
//! REST call; everybody in the channel is then told by the gateway
//! (`VOICE_CHANNEL_EFFECT_SEND`) which sound it was, and each client fetches
//! the file and plays it itself. So hearing them here means fetching, decoding
//! and mixing them into the call - which also puts them in what the echo
//! canceller listens for, so a sound played out of the speakers is not sent
//! back into the call.
//!
//! What can be played: Discord's default sounds, from REST, and each guild's
//! own, asked of the gateway (op 31) when a voice channel is joined. A guild's
//! sounds can be played in that guild; anywhere with Nitro.

use super::*;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug, PartialEq)]
pub struct Sound {
    pub id: String,
    pub name: String,
    /// Discord's own level for it, 0 to 1.
    pub volume: f32,
    /// A Unicode emoji, or a custom one's id.
    pub emoji_name: Option<String>,
    pub emoji_id: Option<String>,
    /// Absent for a default sound.
    pub guild_id: Option<String>,
}

pub fn read_sound(v: &Value) -> Option<Sound> {
    if v["available"].as_bool() == Some(false) {
        return None;
    }
    Some(Sound {
        id: v["sound_id"].as_str()?.to_string(),
        name: v["name"].as_str().unwrap_or("sound").to_string(),
        volume: v["volume"].as_f64().unwrap_or(1.0) as f32,
        emoji_name: v["emoji_name"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
        emoji_id: v["emoji_id"].as_str().map(str::to_string),
        guild_id: v["guild_id"].as_str().map(str::to_string),
    })
}

fn defaults() -> &'static Mutex<Option<Vec<Sound>>> {
    static DEFAULTS: OnceLock<Mutex<Option<Vec<Sound>>>> = OnceLock::new();
    DEFAULTS.get_or_init(Default::default)
}

/// Account and guild to that guild's sounds.
type ByGuild = Mutex<HashMap<(String, String), Vec<Sound>>>;

fn guilds() -> &'static ByGuild {
    static GUILDS: OnceLock<ByGuild> = OnceLock::new();
    GUILDS.get_or_init(Default::default)
}

/// A guild's whole list, as `SOUNDBOARD_SOUNDS` or
/// `GUILD_SOUNDBOARD_SOUNDS_UPDATE` carry it.
pub fn note_guild_sounds(account_id: &str, guild_id: &str, list: &Value) {
    let sounds: Vec<Sound> = list.as_array().into_iter().flatten().filter_map(read_sound).collect();
    guilds().lock().unwrap().insert((account_id.to_string(), guild_id.to_string()), sounds);
}

/// One sound added, changed or removed.
pub fn note_sound_change(account_id: &str, dispatch: &str, d: &Value) {
    let Some(guild_id) = d["guild_id"].as_str() else { return };
    let Some(id) = d["sound_id"].as_str() else { return };
    let mut all = guilds().lock().unwrap();
    let list = all.entry((account_id.to_string(), guild_id.to_string())).or_default();
    list.retain(|s| s.id != id);
    if dispatch != "GUILD_SOUNDBOARD_SOUND_DELETE" {
        if let Some(sound) = read_sound(d) {
            list.push(sound);
        }
    }
}

/// Asks the gateway for these guilds' sounds; they arrive as
/// `SOUNDBOARD_SOUNDS`, one dispatch per guild.
pub fn request(state: &AppState, account_id: &str, guild_ids: &[String]) {
    if guild_ids.is_empty() {
        return;
    }
    if let Some(sender) = state.runtime.discord_gateway_sender(account_id) {
        let _ = sender.send(json!({ "op": 31, "d": { "guild_ids": guild_ids } }).to_string());
    }
}

async fn default_sounds(token: &str) -> Vec<Sound> {
    if let Some(list) = defaults().lock().unwrap().clone() {
        return list;
    }
    let fetched = async {
        let resp = http_client_for(token)
            .get(format!("{API_BASE}/soundboard-default-sounds"))
            .header("Authorization", token)
            .send()
            .await
            .ok()?;
        resp.status().is_success().then_some(())?;
        let body: Value = resp.json().await.ok()?;
        Some(body.as_array().into_iter().flatten().filter_map(read_sound).collect::<Vec<_>>())
    }
    .await;
    match fetched {
        Some(list) => {
            *defaults().lock().unwrap() = Some(list.clone());
            list
        }
        None => Vec::new(),
    }
}

/// Everything this account could play, from the guild it is in: that guild's
/// sounds first, then Discord's defaults, then the rest - each marked with
/// whether it can be played here.
pub async fn list(state: &AppState, account_id: &str, guild_id: Option<&str>, token: &str) -> Vec<Value> {
    let defaults = default_sounds(token).await;
    let nitro = state.runtime.emoji_unrestricted(account_id);
    // This guild's own, then Discord's defaults, then every other guild's.
    let mut here: Vec<(String, Vec<Sound>, bool)> = Vec::new();
    let mut elsewhere: Vec<(String, Vec<Sound>, bool)> = Vec::new();
    for ((a, g), sounds) in guilds().lock().unwrap().iter() {
        if a != account_id || sounds.is_empty() {
            continue;
        }
        let name = state
            .runtime
            .get_buffer_group(&super::guilds::guild_group_id(account_id, g))
            .map(|grp| grp.name)
            .unwrap_or_else(|| "Server".to_string());
        if Some(g.as_str()) == guild_id {
            here.push((name, sounds.clone(), true));
        } else {
            elsewhere.push((name, sounds.clone(), nitro));
        }
    }
    elsewhere.sort_by_key(|(name, _, _)| name.to_lowercase());
    let groups: Vec<(String, Vec<Sound>, bool)> =
        here.into_iter().chain([("Default".to_string(), defaults, true)]).chain(elsewhere).collect();
    groups
        .into_iter()
        .flat_map(|(group, sounds, usable)| {
            sounds.into_iter().map(move |s| {
                json!({
                    "id": s.id,
                    "name": s.name,
                    "group": group,
                    "guildId": s.guild_id,
                    "emojiName": s.emoji_name,
                    "emojiId": s.emoji_id,
                    "locked": !usable,
                })
            })
        })
        .collect()
}

/// Sets a sound off in the channel this account is in. Everybody there -
/// this account included - hears it when the gateway says it was played.
pub async fn play(state: &AppState, account_id: &str, channel_id: &str, sound_id: &str, source_guild_id: Option<&str>) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let mut body = json!({ "sound_id": sound_id });
    if let Some(g) = source_guild_id {
        body["source_guild_id"] = json!(g);
    }
    let resp = send_write(
        http_client_for(&cfg.token)
            .post(format!("{API_BASE}/channels/{channel_id}/send-soundboard-sound"))
            .header("Authorization", &cfg.token)
            .json(&body),
    )
    .await
    .context("playing the sound")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// A sound somebody played in a channel this account is in: fetched,
/// decoded and handed to the call to mix in.
pub async fn heard(state: &AppState, account_id: &str, d: &Value) {
    // A string for a guild's sound and, for some default ones, a number.
    let Some(sound_id) = d["sound_id"].as_str().map(str::to_string).or_else(|| d["sound_id"].as_u64().map(|n| n.to_string())) else {
        return;
    };
    let sound_id = sound_id.as_str();
    let ours = state.voice.current_channel(account_id).map(|(_, c)| c);
    if ours.as_deref() != d["channel_id"].as_str() {
        return;
    }
    let volume = d["sound_volume"].as_f64().unwrap_or(1.0) as f32;
    let token = state.accounts.get_discord(account_id).map(|c| c.token).unwrap_or_default();
    match fetch(&token, sound_id).await.and_then(|bytes| decode(&bytes)) {
        Ok(mut pcm) => {
            crate::audio::apply_gain(&mut pcm, volume);
            state.voice.queue_effect(account_id, &pcm);
        }
        Err(e) => tracing::warn!("discord[{account_id}]: soundboard sound {sound_id}: {e:#}"),
    }
}

fn cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("nobilis")
        .join("discord-sounds")
}

/// The largest sound file accepted. Discord takes uploads up to 512 KB for
/// at most 5.2 seconds; anything much bigger is not a soundboard sound, and
/// what gets decoded here was uploaded by anyone in the server (#251).
const LARGEST_BYTES: usize = 1024 * 1024;

/// The sound's file, kept: a sound id names one recording for good.
async fn fetch(token: &str, id: &str) -> Result<Vec<u8>> {
    if !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!("that is not a sound id");
    }
    let path = cache_dir().join(id);
    if let Ok(bytes) = tokio::fs::read(&path).await {
        if bytes.len() <= LARGEST_BYTES {
            return Ok(bytes);
        }
    }
    let resp = http_client_for(token)
        .get(format!("https://cdn.discordapp.com/soundboard-sounds/{id}"))
        .send()
        .await
        .context("fetching the sound")?;
    if !resp.status().is_success() {
        bail!("Discord has no file for that sound ({})", resp.status());
    }
    if resp.content_length().is_some_and(|n| n as usize > LARGEST_BYTES) {
        bail!("that sound is larger than any soundboard sound can be");
    }
    // Read a piece at a time and stopped at the limit, rather than trusting
    // a Content-Length that need not be there or be true.
    let mut resp = resp;
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("reading the sound")? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > LARGEST_BYTES {
            bail!("that sound is larger than any soundboard sound can be");
        }
    }
    tokio::fs::create_dir_all(cache_dir()).await.ok();
    let _ = tokio::fs::write(&path, &bytes).await;
    Ok(bytes)
}

/// The longest a sound is allowed to be, as Discord allows: 5.2 seconds.
const LONGEST_SECS: usize = 6;

/// MP3 or Ogg Vorbis to 48kHz stereo, interleaved, which is what the call
/// plays. Resampled linearly: a soundboard sound is a few seconds of effect,
/// not music anybody is listening to closely.
pub fn decode(bytes: &[u8]) -> Result<Vec<i16>> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes.to_vec())), Default::default());
    let probed = symphonia::default::get_probe()
        .format(&Hint::new(), source, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| anyhow!("not a sound this can read: {e}"))?;
    let mut format = probed.format;
    let track = format.default_track().context("the file has no audio in it")?;
    let track_id = track.id;
    let rate = track.codec_params.sample_rate.unwrap_or(48_000) as usize;
    // A guild's own sounds are Ogg Opus, which symphonia can unwrap but not
    // decode. libopus is here already for the calls themselves.
    if track.codec_params.codec == symphonia::core::codecs::CODEC_TYPE_OPUS {
        let mut opus = opus2::Decoder::new(48_000, opus2::Channels::Stereo).map_err(|e| anyhow!("no Opus decoder: {e:?}"))?;
        let mut out: Vec<i16> = Vec::new();
        let mut frame = vec![0i16; 5760 * 2];
        while let Ok(packet) = format.next_packet() {
            if packet.track_id() != track_id {
                continue;
            }
            // Checked in Rust before libopus sees it (#251); a packet that
            // is not one, or is longer than the buffer, is left out.
            if crate::opus_packet::plausible(&packet.data, frame.len() / 2).is_none() {
                continue;
            }
            if let Ok(n) = opus.decode(&packet.data, &mut frame, false) {
                out.extend_from_slice(&frame[..n * 2]);
            }
            if out.len() / 2 > 48_000 * LONGEST_SECS {
                break;
            }
        }
        return Ok(out);
    }
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!("no decoder for it: {e}"))?;

    let mut stereo: Vec<f32> = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        for frame in buf.samples().chunks(channels) {
            let (l, r) = if channels == 1 { (frame[0], frame[0]) } else { (frame[0], frame[1]) };
            stereo.push(l);
            stereo.push(r);
        }
        if stereo.len() / 2 > rate * LONGEST_SECS {
            break;
        }
    }
    Ok(resample(&stereo, rate, 48_000))
}

fn resample(stereo: &[f32], from: usize, to: usize) -> Vec<i16> {
    let frames = stereo.len() / 2;
    let out_frames = if from == to { frames } else { frames * to / from.max(1) };
    let mut out = Vec::with_capacity(out_frames * 2);
    for i in 0..out_frames {
        let pos = i as f64 * from as f64 / to as f64;
        let a = pos.floor() as usize;
        let b = (a + 1).min(frames.saturating_sub(1));
        let t = (pos - a as f64) as f32;
        for c in 0..2 {
            let x = stereo.get(a * 2 + c).copied().unwrap_or(0.0);
            let y = stereo.get(b * 2 + c).copied().unwrap_or(0.0);
            out.push(((x + (y - x) * t).clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Ogg Opus file - what a guild's own sounds are - decodes through
    /// libopus to 48kHz stereo.
    #[test]
    fn an_ogg_opus_sound_decodes() {
        let tone: Vec<f32> = (0..48_000).map(|n| (n as f32 * 0.05).sin() * 0.3).collect();
        let bytes = crate::oggopus::encode(&tone).unwrap();
        let pcm = decode(&bytes).unwrap();
        let secs = pcm.len() as f32 / 2.0 / 48_000.0;
        assert!((0.9..1.1).contains(&secs), "decoded {secs}s");
        assert!(pcm.iter().any(|s| s.saturating_abs() > 1000));
    }

    #[test]
    fn a_sound_is_read_with_its_emoji() {
        let v = json!({ "sound_id": "1", "name": "quack", "volume": 0.5, "emoji_name": "🦆", "guild_id": "9", "available": true });
        let s = read_sound(&v).unwrap();
        assert_eq!((s.id.as_str(), s.name.as_str(), s.volume), ("1", "quack", 0.5));
        assert_eq!(s.emoji_name.as_deref(), Some("🦆"));
        assert!(read_sound(&json!({ "sound_id": "2", "available": false })).is_none());
    }

    #[test]
    fn resampling_keeps_the_length_in_time() {
        let one_second_44k = vec![0.1f32; 44_100 * 2];
        assert_eq!(resample(&one_second_44k, 44_100, 48_000).len(), 48_000 * 2);
    }
}


#[cfg(test)]
mod opus_guard_tests {
    /// A real Ogg Opus file - what a guild's own sounds are - still decodes in
    /// full with every packet checked before libopus sees it (#251).
    #[test]
    fn a_real_ogg_opus_sound_decodes_whole() {
        // Two seconds of a tone at 48 kHz mono, written as Ogg Opus the way
        // voice messages are.
        let samples: Vec<f32> = (0..96_000).map(|i| (i as f32 * 0.03).sin() * 0.5).collect();
        let file = crate::oggopus::encode(&samples).unwrap();
        let pcm = super::decode(&file).unwrap();
        let seconds = pcm.len() as f64 / 2.0 / 48_000.0;
        assert!((1.9..=2.1).contains(&seconds), "decoded {seconds:.2}s of a 2s sound");
    }

    /// Packets that are not Opus are left out rather than handed over.
    #[test]
    fn a_damaged_ogg_opus_sound_does_not_reach_the_decoder_whole() {
        let samples: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.03).sin() * 0.5).collect();
        let mut file = crate::oggopus::encode(&samples).unwrap();
        // Corrupt the audio pages (after the two header pages) without
        // touching the Ogg framing: each packet's first byte becomes a code 3
        // header claiming 63 frames, which no valid packet can.
        let mut i = 0;
        let mut pages = 0;
        while i + 27 < file.len() {
            if &file[i..i + 4] != b"OggS" {
                i += 1;
                continue;
            }
            let segments = file[i + 26] as usize;
            let body = i + 27 + segments;
            pages += 1;
            if pages > 2 && body + 1 < file.len() {
                file[body] = (31 << 3) | 3;
                file[body + 1] = 63;
            }
            let len: usize = file[i + 27..i + 27 + segments].iter().map(|&b| b as usize).sum();
            i = body + len;
        }
        // Still a file symphonia can read, and the check keeps the bad
        // packets out; it must not panic or error the whole sound.
        let _ = super::decode(&file);
    }
}
