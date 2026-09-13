//! Everything this client says: messages, edits, deletions, reactions.
//!
//! The writing half of `messages`, kept apart from it because the two are
//! asymmetric in a way worth seeing - reading is parsing whatever arrives,
//! and writing is a small set of deliberate requests, each of which can be
//! refused.

use super::*;

/// How long a "typing" notice stands before it should be forgotten.
///
/// Discord sends no "stopped typing" event and its own client expires the
/// indicator after ten seconds, so this is that convention rather than a
/// number from the protocol. Matrix does carry a timeout of its own, and
/// uses this as what to ask for.
pub const TYPING_TTL_MS: u64 = 10_000;

/// Builds the `message_reference` object Discord expects for a native
/// reply - the same mechanism its own clients use, not a quoted-text
/// convention layered on top.
pub(super) fn reply_reference(reply_to_id: Option<&str>) -> Option<Value> {
    reply_to_id.map(|id| json!({ "message_id": id }))
}

/// REST message send - Discord's gateway is receive-only from the client's
/// perspective for user accounts; sending is always a plain HTTP POST.
/// Turns the names somebody typed into the mentions Discord understands.
///
/// Discord pings on `<@id>` and on nothing else: a literal "@alice" is text.
/// So every mention typed in this client reached the channel looking right
/// and notifying nobody - which is worse than not being able to mention at
/// all, because it looks like it worked.
///
/// Longest name first, because display names overlap: a channel with "Sam"
/// and "Sam Vimes" in it must not turn "@Sam Vimes" into a ping for Sam
/// followed by the word "Vimes".
///
/// `@everyone` and `@here` are left exactly as typed. They are Discord's own
/// keywords, not names, and rewriting them would be inventing an id for
/// something that has none.
pub(super) fn resolve_outgoing_mentions(body: &str, mut candidates: Vec<(String, String)>) -> String {
    candidates.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    let mut out = body.to_string();
    for (name, id) in candidates {
        if name.is_empty() || name.eq_ignore_ascii_case("everyone") || name.eq_ignore_ascii_case("here") {
            continue;
        }
        out = replace_mention(&out, &name, &id);
    }
    out
}

/// One name, everywhere it was typed as a mention.
///
/// Case-insensitive, because nobody types capitals the way a display name
/// carries them - and bounded, so "@sam" inside "email@sample.org" is left
/// alone: the "@" must start a word, and the name must end one.
pub(super) fn replace_mention(body: &str, name: &str, id: &str) -> String {
    let lower_body = body.to_lowercase();
    let needle = format!("@{}", name.to_lowercase());
    let mut out = String::with_capacity(body.len());
    let mut cursor = 0usize;

    while let Some(found) = lower_body[cursor..].find(&needle) {
        let start = cursor + found;
        let end = start + needle.len();
        let before_ok = start == 0 || body[..start].chars().next_back().is_some_and(|c| c.is_whitespace() || c == '(');
        let after_ok = body[end..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || matches!(c, ',' | '.' | '!' | '?' | ':' | ';' | ')' | '\''));
        out.push_str(&body[cursor..start]);
        if before_ok && after_ok {
            // A role id arrives with its own "&" already on it, which is
            // the only difference between the two forms Discord accepts.
            out.push_str(&format!("<@{id}>"));
        } else {
            out.push_str(&body[start..end]);
        }
        cursor = end;
    }
    out.push_str(&body[cursor..]);
    out
}

#[cfg(test)]
mod mention_tests {
    use super::resolve_outgoing_mentions;

    fn people() -> Vec<(String, String)> {
        vec![
            ("Sam".into(), "1".into()),
            ("Sam Vimes".into(), "2".into()),
            ("alice".into(), "3".into()),
        ]
    }

    /// The whole point: Discord pings on `<@id>` and treats "@alice" as text,
    /// so a mention that is not rewritten reaches the channel looking right
    /// and notifying nobody.
    #[test]
    fn a_typed_name_becomes_the_id_discord_pings_on() {
        assert_eq!(resolve_outgoing_mentions("@alice hello", people()), "<@3> hello");
        assert_eq!(resolve_outgoing_mentions("hello @alice", people()), "hello <@3>");
        assert_eq!(resolve_outgoing_mentions("(@alice)", people()), "(<@3>)");
        assert_eq!(resolve_outgoing_mentions("@alice, hello", people()), "<@3>, hello");
    }

    /// Display names overlap. Longest first, or "@Sam Vimes" becomes a ping
    /// for Sam followed by the loose word "Vimes".
    #[test]
    fn the_longer_name_wins() {
        assert_eq!(resolve_outgoing_mentions("@Sam Vimes hi", people()), "<@2> hi");
        assert_eq!(resolve_outgoing_mentions("@Sam hi", people()), "<@1> hi");
    }

    #[test]
    fn case_does_not_matter_but_word_boundaries_do() {
        assert_eq!(resolve_outgoing_mentions("@ALICE hi", people()), "<@3> hi");
        // Inside a word: an address, not a mention.
        assert_eq!(resolve_outgoing_mentions("mail@alice.example", people()), "mail@alice.example");
        // A longer name that merely starts with a known one.
        assert_eq!(resolve_outgoing_mentions("@alicent hi", people()), "@alicent hi");
    }

    /// Discord's own keywords are not names and have no id to become.
    #[test]
    fn everyone_and_here_are_left_alone() {
        let mut roster = people();
        roster.push(("everyone".into(), "9".into()));
        roster.push(("here".into(), "10".into()));
        assert_eq!(resolve_outgoing_mentions("@everyone look", roster.clone()), "@everyone look");
        assert_eq!(resolve_outgoing_mentions("@here look", roster), "@here look");
    }

    /// Roles are tagged the same way, with an ampersand in the id - which is
    /// carried on the id itself so one rewrite handles both.
    #[test]
    fn a_role_becomes_a_role_mention() {
        let roster = vec![("Gaymer Word User".to_string(), "&42".to_string())];
        assert_eq!(resolve_outgoing_mentions("@Gaymer Word User look", roster), "<@&42> look");
    }

    #[test]
    fn text_with_no_mentions_is_untouched() {
        assert_eq!(resolve_outgoing_mentions("just talking", people()), "just talking");
        assert_eq!(resolve_outgoing_mentions("", people()), "");
    }
}

/// Everybody this conversation could be talking about: the channel's own
/// member list first, then anybody this account has seen elsewhere.
///
/// The roster is what a person is looking at when they type a name, so it is
/// the authority; the wider cache catches somebody who has left the visible
/// window but is still in the conversation.
pub(super) fn mention_candidates(state: &AppState, account_id: &str, buffer_id: &str) -> Vec<(String, String)> {
    let mut candidates: Vec<(String, String)> = Vec::new();
    if let Some(members) = state.runtime.get_presence(buffer_id) {
        for member in members.as_array().into_iter().flatten() {
            if let (Some(nick), Some(id)) = (member["nick"].as_str(), member["userId"].as_str()) {
                candidates.push((nick.to_string(), id.to_string()));
            }
        }
    }
    // Roles are mentioned as `<@&id>` - the same shape with an ampersand -
    // and a guild's mentionable roles are as much a thing people tag as its
    // members are.
    if let Some(guild) = state
        .runtime
        .get_buffer(buffer_id)
        .and_then(|b| b.group_id)
        .and_then(|g| g.rsplit_once("guild:").map(|(_, id)| id.to_string()))
    {
        for role in state.runtime.discord_mentionable_roles(&guild) {
            if let (Some(name), Some(id)) = (role["name"].as_str(), role["id"].as_str()) {
                candidates.push((name.to_string(), format!("&{id}")));
            }
        }
    }
    candidates.extend(state.runtime.discord_known_names(account_id));
    candidates
}

/// Sends somebody else's message on, as a forward.
///
/// Not a copy with quotation marks round it: Discord has a forward of its own,
/// and it is a message whose entire content is a reference to another one. The
/// server takes the snapshot, so what arrives at the far end is the original -
/// which is how a forward carries pictures and embeds this client never
/// re-uploads.
///
/// The source channel is named as well as the message, because a forward
/// crosses rooms and servers, and that is the whole point of it.
pub async fn forward_message(
    state: &AppState,
    from_buffer: &str,
    message_id: &str,
    to_buffer: &str,
    token: &str,
) -> Result<()> {
    let source_channel = state
        .runtime
        .get_discord_channel(from_buffer)
        .ok_or_else(|| anyhow!("that message is not in a Discord conversation"))?;
    let target_channel = state
        .runtime
        .get_discord_channel(to_buffer)
        .ok_or_else(|| anyhow!("there is no Discord conversation to send it to"))?;
    let mut reference = json!({
        // 1 is Discord's own number for a forward, as against 0 for a reply.
        "type": 1,
        "channel_id": source_channel,
        "message_id": message_id,
    });
    // Where it is from, when it is from a server. A direct message has no
    // guild, and naming one would be a claim the server would refuse.
    if let Some(guild_id) = state.runtime.get_discord_guild(from_buffer) {
        reference["guild_id"] = json!(guild_id);
    }
    let resp = send_write(
        http_client()
            .post(format!("{API_BASE}/channels/{target_channel}/messages"))
            .header("Authorization", token)
            .json(&json!({ "content": "", "message_reference": reference })),
    )
    .await
    .context("forwarding the message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "forwarding that message"));
    }
    Ok(())
}

pub async fn send_message(state: &AppState, buffer_id: &str, token: &str, body: &str, reply_to_id: Option<&str>) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
    let content = resolve_outgoing_mentions(body, mention_candidates(state, &account_id, buffer_id));
    let mut payload = json!({ "content": content });
    if let Some(reference) = reply_reference(reply_to_id) {
        payload["message_reference"] = reference;
    }
    let resp = send_write(
        http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/messages"))
        .header("Authorization", token)
        .json(&payload)
        )
    .await
        .context("sending Discord message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// The "+" attachment button's backend: a single multipart POST carrying
/// both the message JSON (as a `payload_json` part) and the file bytes
/// (as a `files[0]` part) - Discord's documented way to send an attachment
/// inline with a message in one request, no separate upload-then-attach
/// step needed for files under the account's size limit.
pub async fn send_attachment(state: &AppState, buffer_id: &str, token: &str, body: &str, attachment_path: &str, reply_to_id: Option<&str>) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let path = std::path::Path::new(attachment_path);
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    let bytes = tokio::fs::read(path).await.with_context(|| format!("reading {attachment_path}"))?;
    // A caption is a message like any other, so a name typed in one is a
    // mention like any other.
    let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
    let content = resolve_outgoing_mentions(body, mention_candidates(state, &account_id, buffer_id));
    let mut payload = json!({ "content": content });
    if let Some(reference) = reply_reference(reply_to_id) {
        payload["message_reference"] = reference;
    }
    let form = reqwest::multipart::Form::new()
        .text("payload_json", payload.to_string())
        .part("files[0]", reqwest::multipart::Part::bytes(bytes).file_name(file_name));
    let resp = send_write(
        http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/messages"))
        .header("Authorization", token)
        .multipart(form)
        )
    .await
        .context("uploading Discord attachment")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// Sending a recording as a voice message rather than as a sound file.
///
/// Three things make Discord treat it as one, and all three are required: the
/// IS_VOICE_MESSAGE flag on the message, and `duration_secs` and `waveform` on
/// the attachment. Without them the same bytes arrive as an .ogg somebody
/// attached - which is what every other client would then show, because that
/// is what it would be.
///
/// The attachment metadata travels in `payload_json` alongside the file, keyed
/// by the index of the file part. That is the same single multipart POST the
/// ordinary attachment path uses; the separate upload-then-attach flow exists
/// for files too large for one request, which a voice message is not.
///
/// A voice message carries no text: Discord rejects one with content, and
/// there is nowhere in its own client to type any.
pub async fn send_voice_message(
    state: &AppState,
    buffer_id: &str,
    token: &str,
    file_path: &std::path::Path,
    duration_secs: f64,
    waveform: &str,
) -> Result<()> {
    const IS_VOICE_MESSAGE: u64 = 1 << 13;

    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let bytes = tokio::fs::read(file_path)
        .await
        .with_context(|| format!("reading {}", file_path.display()))?;
    // The name Discord's own client uses. Its clients key on the flag rather
    // than on this, but a file that arrives anywhere else should say what it
    // is - and an unnamed part is rejected outright.
    let file_name = "voice-message.ogg";
    let payload = json!({
        "content": "",
        "flags": IS_VOICE_MESSAGE,
        "attachments": [{
            "id": "0",
            "filename": file_name,
            "duration_secs": duration_secs,
            "waveform": waveform,
        }],
    });
    let form = reqwest::multipart::Form::new()
        .text("payload_json", payload.to_string())
        .part(
            "files[0]",
            reqwest::multipart::Part::bytes(bytes)
                .file_name(file_name)
                .mime_str("audio/ogg")
                .context("audio/ogg is a valid mime type")?,
        );
    let resp = send_write(
        http_client()
            .post(format!("{API_BASE}/channels/{channel_id}/messages"))
            .header("Authorization", token)
            .multipart(form),
    )
    .await
    .context("uploading the voice message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

#[cfg(test)]
mod live_probe {
    //! Sending a real voice message to a real conversation, run by hand.
    //!
    //! The encoder can be checked offline and is - ffprobe agrees the file is
    //! Ogg/Opus of the right length. What cannot be checked offline is whether
    //! Discord accepts it *as a voice message* rather than as a sound file
    //! somebody attached, because that answer only exists on their side.
    //!
    //! `#[ignore]`d, and it takes the token and the channel from the
    //! environment rather than from anywhere in this repository:
    //!   MOHO_DISCORD_TOKEN=… MOHO_DISCORD_CHANNEL=… \
    //!     cargo test --release -- --ignored --nocapture voice_message_probe

    #[tokio::test]
    #[ignore]
    async fn voice_message_probe() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let token = std::env::var("MOHO_DISCORD_TOKEN").expect("MOHO_DISCORD_TOKEN");
        let channel = std::env::var("MOHO_DISCORD_CHANNEL").expect("MOHO_DISCORD_CHANNEL");

        // Two seconds of a tone that rises and falls, so the waveform that
        // arrives is one a person can recognise as this recording rather than
        // as a flat bar.
        let samples: Vec<f32> = (0..crate::oggopus::RATE * 2)
            .map(|i| {
                let t = i as f32 / crate::oggopus::RATE as f32;
                (t * 440.0 * std::f32::consts::TAU).sin() * (t * std::f32::consts::PI / 2.0).sin() * 0.4
            })
            .collect();
        let duration = crate::oggopus::duration_secs(&samples);
        let waveform = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(crate::oggopus::waveform(&samples, 256))
        };
        let bytes = crate::oggopus::encode(&samples).expect("encoding");
        println!("{} bytes of ogg, {duration:.2}s", bytes.len());

        const IS_VOICE_MESSAGE: u64 = 1 << 13;
        let payload = serde_json::json!({
            "content": "",
            "flags": IS_VOICE_MESSAGE,
            "attachments": [{ "id": "0", "filename": "voice-message.ogg", "duration_secs": duration, "waveform": waveform }],
        });
        let form = reqwest::multipart::Form::new()
            .text("payload_json", payload.to_string())
            .part(
                "files[0]",
                reqwest::multipart::Part::bytes(bytes)
                    .file_name("voice-message.ogg")
                    .mime_str("audio/ogg")
                    .unwrap(),
            );
        let resp = super::http_client()
            .post(format!("{}/channels/{channel}/messages", super::API_BASE))
            .header("Authorization", &token)
            .multipart(form)
            .send()
            .await
            .expect("posting");
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.expect("a JSON answer");
        println!("HTTP {status}");
        assert!(status.is_success(), "Discord refused it: {body}");

        // The answer is the message Discord stored, so this is its own
        // verification: the flag it kept, and what it did with the metadata.
        println!("flags={} attachments={}", body["flags"], serde_json::to_string_pretty(&body["attachments"]).unwrap());
        assert_eq!(
            body["flags"].as_u64().unwrap_or(0) & IS_VOICE_MESSAGE,
            IS_VOICE_MESSAGE,
            "stored without the voice-message flag, so it is a sound file: {body}"
        );
        let att = &body["attachments"][0];
        assert!(att["duration_secs"].as_f64().is_some(), "no duration came back: {att}");
        assert!(att["waveform"].as_str().is_some(), "no waveform came back: {att}");
    }
}

#[cfg(test)]
mod receive_probe {
    //! The other half of the round trip: reading back what Discord stored and
    //! running it through the parser the window draws from.
    //!
    //! Read-only, and separate from the send probe so it can be pointed at any
    //! conversation that already has a voice message in it:
    //!   MOHO_DISCORD_TOKEN=… MOHO_DISCORD_CHANNEL=… \
    //!     cargo test --release -- --ignored --nocapture voice_message_read_probe

    #[tokio::test]
    #[ignore]
    async fn voice_message_read_probe() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let token = std::env::var("MOHO_DISCORD_TOKEN").expect("MOHO_DISCORD_TOKEN");
        let channel = std::env::var("MOHO_DISCORD_CHANNEL").expect("MOHO_DISCORD_CHANNEL");

        let messages: serde_json::Value = super::http_client()
            .get(format!("{}/channels/{channel}/messages?limit=10", super::API_BASE))
            .header("Authorization", &token)
            .send()
            .await
            .expect("fetching")
            .json()
            .await
            .expect("a JSON answer");

        let spoken = messages
            .as_array()
            .expect("a list of messages")
            .iter()
            .find(|m| m["flags"].as_u64().unwrap_or(0) & (1 << 13) != 0)
            .expect("no voice message in the last ten - send one first");

        let attachments = crate::backend::discord::messages::extract_attachments(spoken);
        let att = attachments.first().expect("a voice message has an attachment");
        println!(
            "kind={} voice={} duration={:?} waveform={} bytes",
            att.kind,
            att.voice,
            att.duration_secs,
            att.waveform.as_deref().map(str::len).unwrap_or(0)
        );
        assert!(att.voice, "the parser did not read the flag off a real message");
        assert!(att.duration_secs.unwrap_or(0.0) > 0.0, "no length came through");
        assert!(att.waveform.is_some(), "no waveform came through");
        assert_eq!(att.kind, "audio");
    }
}

/// PATCH .../messages/{id} - editing your own message. Discord scopes
/// this to the message author (enforced server-side; there's no separate
/// permission check needed here).
pub async fn edit_message(state: &AppState, buffer_id: &str, token: &str, msg_id: &str, body: &str) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let resp = send_write(
        http_client()
        .patch(format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}"))
        .header("Authorization", token)
        .json(&json!({ "content": body }))
        )
    .await
        .context("editing Discord message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// DELETE .../messages/{id} - deleting your own message. Same
/// author-only enforcement as edit; also works for a moderator with
/// MANAGE_MESSAGES, which Discord itself decides, not this code.
/// Says that this account is composing something.
///
/// One POST covers about ten seconds, so a caller repeats it while somebody
/// keeps typing rather than sending one per keystroke. Failure is not worth
/// reporting: a missing typing indicator is not something to interrupt
/// somebody mid-sentence about.
pub async fn send_typing(state: &AppState, buffer_id: &str, token: &str) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    send_write(
        http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/typing"))
        .header("Authorization", token)
        .header("Content-Length", "0")
        )
    .await
        .context("sending a Discord typing notice")?;
    Ok(())
}

/// Tells Discord this conversation has been read up to its newest message.
///
/// Unread was purely local before this, so reading a channel here did not
/// clear it on a phone and reading it there did not clear it here - the gap
/// most visible to anyone who uses Discord on more than one device.
///
/// The newest message this client actually holds is what gets acked, not the
/// newest that exists: acking past something never seen would mark it read
/// on every device on the strength of a message this one never showed.
///
/// Quietly does nothing for a buffer with no messages, which is an ordinary
/// state for a channel opened and never scrolled.
pub async fn ack_read(state: &AppState, buffer_id: &str, token: &str) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let Some(msg_id) = state.store.newest_msg_id(buffer_id)? else { return Ok(()) };
    let resp = send_write(
        http_client()
        .post(format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}/ack"))
        .header("Authorization", token)
        .json(&json!({ "token": serde_json::Value::Null }))
        )
    .await
        .context("acking Discord read state")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

pub async fn delete_message(state: &AppState, buffer_id: &str, token: &str, msg_id: &str) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let resp = send_write(
        http_client()
        .delete(format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}"))
        .header("Authorization", token)
        )
    .await
        .context("deleting Discord message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Discord API error {status}: {text}");
    }
    Ok(())
}

/// Discord's reaction endpoint wants a custom emoji as `name:id` (no
/// angle brackets, no leading `a:` animated marker), but every reaction
/// this app already tracks (extract_reactions above, the live
/// MESSAGE_REACTION_ADD/REMOVE handling) stores it wrapped as `<:name:id>`
/// - that's the one place besides here needing the raw form, so it's
/// unwrapped here rather than changing the stored shape everywhere else.
/// A plain Unicode emoji (no wrapper) passes through unchanged.
pub(super) fn reaction_path_segment(emoji: &str) -> &str {
    emoji.strip_prefix("<:").or_else(|| emoji.strip_prefix("<a:")).and_then(|s| s.strip_suffix('>')).unwrap_or(emoji)
}

/// PUT/DELETE .../messages/{id}/reactions/{emoji}/@me - adding or
/// removing *our own* reaction (Discord's reaction endpoints are
/// per-reactor; there's no "set the count directly" concept). Built via
/// `Url::path_segments_mut` rather than hand-rolled string formatting so
/// the emoji segment - raw Unicode bytes for a standard emoji, a `:`
/// inside a custom one - gets properly percent-encoded rather than
/// landing in the URL unescaped.
pub async fn toggle_reaction(state: &AppState, buffer_id: &str, token: &str, msg_id: &str, emoji: &str, add: bool) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let mut url = url::Url::parse(&format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}/reactions")).context("building reaction URL")?;
    url.path_segments_mut().map_err(|_| anyhow!("reaction URL cannot be a base"))?.push(reaction_path_segment(emoji)).push("@me");

    let client = http_client();
    let req = if add { client.put(url) } else { client.delete(url) };
    let resp = req.header("Authorization", token).send().await.context("toggling Discord reaction")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // Reactions are rate-limited tightly, and clicking again is exactly
        // what somebody does when one appears not to have worked - so this
        // is a refusal people meet, and "You are being rate limited" is a
        // great deal more use than the object carrying it.
        bail!("{}", discord_error_text(status, &text, "reacting"));
    }
    Ok(())
}
