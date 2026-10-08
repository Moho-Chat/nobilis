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

/// A message's reply fields: the reference, and - when the person turned the
/// "@ ON" switch off - the instruction not to ping the author. Discord pings
/// the one answered by default, and `allowed_mentions` is how its own client
/// says not to; everything else the message mentions stays allowed.
pub(super) fn put_reply(payload: &mut Value, reply_to_id: Option<&str>, ping_author: bool) {
    if let Some(reference) = reply_reference(reply_to_id) {
        payload["message_reference"] = reference;
        if !ping_author {
            payload["allowed_mentions"] = json!({ "parse": ["users", "roles", "everyone"], "replied_user": false });
        }
    }
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
        http_client_for(token)
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

pub async fn send_message(state: &AppState, buffer_id: &str, token: &str, body: &str, reply_to_id: Option<&str>, reply_ping: bool) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
    let content = resolve_outgoing_mentions(body, mention_candidates(state, &account_id, buffer_id));
    let mut payload = json!({ "content": content });
    put_reply(&mut payload, reply_to_id, reply_ping);
    let resp = send_write(
        http_client_for(token)
        .post(format!("{API_BASE}/channels/{channel_id}/messages"))
        .header("Authorization", token)
        .json(&payload)
        )
    .await
        .context("sending Discord message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", refusal_text(status, &text));
    }
    Ok(())
}

/// The most files Discord takes in one message.
pub const MAX_ATTACHMENTS: usize = 10;

/// Counts the bytes of an upload as they are taken from the files, and says
/// how far it has got.
///
/// The files are streamed rather than read whole, so the count is real: what
/// has been handed to the connection, which can run a little ahead of what the
/// network has carried - by about what its buffers hold - until the last chunk,
/// after which the wait for Discord's answer is said separately.
struct Meter {
    sent: std::sync::atomic::AtomicU64,
    total: u64,
    /// When the last report went out, and what it said.
    last: std::sync::Mutex<(std::time::Instant, u64)>,
    progress: Option<crate::upload::Progress>,
    /// The least time between two reports.
    gap: std::time::Duration,
}

impl Meter {
    const HOST: &'static str = "Discord";
    const GAP: std::time::Duration = std::time::Duration::from_millis(120);

    fn add(&self, n: usize) {
        use std::sync::atomic::Ordering;
        let Some(progress) = &self.progress else { return };
        let sent = self.sent.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        let mut last = self.last.lock().unwrap();
        if sent >= self.total {
            // Everything is out; what is left is Discord's answer.
            if last.1 != self.total {
                *last = (std::time::Instant::now(), self.total);
                progress.at(crate::upload::Phase::Waiting, self.total as usize, Self::HOST);
            }
        } else if last.0.elapsed() >= self.gap && sent - last.1 >= self.total / 200 {
            // Often enough to look like movement, not so often that a fast
            // link floods the socket to the window with events.
            *last = (std::time::Instant::now(), sent);
            progress.sent(sent, self.total, Self::HOST);
        }
    }
}

/// A file as one part of the upload, read from disk as it is sent and counted
/// by `meter` as it goes.
fn counted_part(path: &str, name: &str, len: u64, meter: std::sync::Arc<Meter>) -> Result<reqwest::multipart::Part> {
    let file = std::fs::File::open(path).with_context(|| format!("reading {path}"))?;
    let stream = tokio_util::io::ReaderStream::with_capacity(tokio::fs::File::from_std(file), 64 * 1024);
    let counted = futures::StreamExt::map(stream, move |chunk| {
        if let Ok(bytes) = &chunk {
            meter.add(bytes.len());
        }
        chunk
    });
    Ok(reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(counted), len).file_name(name.to_string()))
}

/// The "+" attachment button's backend: a single multipart POST carrying
/// both the message JSON (as a `payload_json` part) and the files (as
/// `files[0]`, `files[1]`...) - Discord's documented way to send attachments
/// inline with a message in one request, no separate upload-then-attach step
/// needed for files under the account's size limit. Up to ten files make one
/// message, which is what Discord's own client does with several chosen at
/// once.
pub async fn send_attachments(
    state: &AppState,
    buffer_id: &str,
    token: &str,
    body: &str,
    attachment_paths: &[String],
    reply: (Option<&str>, bool),
    progress: Option<&crate::upload::Progress>,
) -> Result<()> {
    use crate::upload::Phase;
    // Who is answered, and whether they are told.
    let (reply_to_id, reply_ping) = reply;
    if attachment_paths.is_empty() {
        bail!("no file to send");
    }
    if attachment_paths.len() > MAX_ATTACHMENTS {
        bail!("Discord takes at most {MAX_ATTACHMENTS} files in one message");
    }
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    if let Some(p) = progress {
        p.at(Phase::Preparing, 0, Meter::HOST);
    }
    // Sizes first, from the files themselves, so the ring has its whole
    // length before the first byte goes.
    let mut files = Vec::with_capacity(attachment_paths.len());
    let mut total = 0u64;
    for path in attachment_paths {
        let len = tokio::fs::metadata(path).await.with_context(|| format!("reading {path}"))?.len();
        let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        total += len;
        files.push((path.clone(), name, len));
    }
    // A caption is a message like any other, so a name typed in one is a
    // mention like any other.
    let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
    let content = resolve_outgoing_mentions(body, mention_candidates(state, &account_id, buffer_id));
    let mut payload = json!({ "content": content });
    put_reply(&mut payload, reply_to_id, reply_ping);
    if let Some(p) = progress {
        p.sent(0, total, Meter::HOST);
    }

    let client = http_client_for(token);
    let url = format!("{API_BASE}/channels/{channel_id}/messages");
    let resp = send_write_rebuilt(|| {
        // Built afresh for every try: a streamed body is used up by sending
        // it, so a rate-limited request has to open its files again.
        let meter = std::sync::Arc::new(Meter {
            sent: std::sync::atomic::AtomicU64::new(0),
            total,
            last: std::sync::Mutex::new((std::time::Instant::now(), 0)),
            progress: progress.cloned(),
            gap: Meter::GAP,
        });
        let mut form = reqwest::multipart::Form::new().text("payload_json", payload.to_string());
        for (i, (path, name, len)) in files.iter().enumerate() {
            form = form.part(format!("files[{i}]"), counted_part(path, name, *len, meter.clone())?);
        }
        Ok(client.post(&url).header("Authorization", token).multipart(form))
    })
    .await
    .context("uploading Discord attachment")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", refusal_text(status, &text));
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
        http_client_for(token)
            .post(format!("{API_BASE}/channels/{channel_id}/messages"))
            .header("Authorization", token)
            .multipart(form),
    )
    .await
    .context("uploading the voice message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", refusal_text(status, &text));
    }
    Ok(())
}

#[cfg(test)]
mod reply_tests {
    use super::*;

    #[test]
    fn a_reply_pings_its_author_unless_told_not_to() {
        let mut ping = json!({ "content": "hi" });
        put_reply(&mut ping, Some("42"), true);
        assert_eq!(ping["message_reference"]["message_id"], "42");
        assert!(ping.get("allowed_mentions").is_none(), "Discord's own default stands");

        let mut quiet = json!({ "content": "hi" });
        put_reply(&mut quiet, Some("42"), false);
        assert_eq!(quiet["allowed_mentions"]["replied_user"], false);
        // Whatever else the message mentions is still allowed.
        assert_eq!(quiet["allowed_mentions"]["parse"], json!(["users", "roles", "everyone"]));
    }

    #[test]
    fn a_message_that_answers_nothing_has_neither() {
        let mut plain = json!({ "content": "hi" });
        put_reply(&mut plain, None, false);
        assert!(plain.get("message_reference").is_none() && plain.get("allowed_mentions").is_none());
    }
}

#[cfg(test)]
mod upload_tests {
    //! Several files in one request, counted as they go - against a server of
    //! our own, since Discord's address is not ours to point at.
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn several_files_make_one_counted_request() {
        let dir = std::env::temp_dir().join(format!("nobilis-multi-{}", crate::model::next_message_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = vec![b'a'; 700_000];
        let second = vec![b'b'; 500_000];
        std::fs::write(dir.join("one.png"), &first).unwrap();
        std::fs::write(dir.join("two.png"), &second).unwrap();

        // A server that reads one whole request and says what it saw.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut chunk = vec![0u8; 65536];
            let mut header_end = None;
            let mut want = usize::MAX;
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(at + 4);
                        let head = String::from_utf8_lossy(&seen[..at]).to_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .map(|v| v.trim().parse::<usize>().unwrap());
                        want = at + 4 + length.expect("a known content-length, not chunked");
                    }
                }
                if seen.len() >= want {
                    break;
                }
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await.unwrap();
            seen
        });

        let bus = crate::events::EventBus::new();
        let mut rx = bus.subscribe();
        let total = (first.len() + second.len()) as u64;
        let meter = std::sync::Arc::new(Meter {
            sent: std::sync::atomic::AtomicU64::new(0),
            total,
            last: std::sync::Mutex::new((std::time::Instant::now() - std::time::Duration::from_secs(1), 0)),
            progress: Some(crate::upload::Progress::new(bus, "up-1")),
            gap: std::time::Duration::ZERO,
        });
        let form = reqwest::multipart::Form::new()
            .text("payload_json", "{}")
            .part("files[0]", counted_part(dir.join("one.png").to_str().unwrap(), "one.png", first.len() as u64, meter.clone()).unwrap())
            .part("files[1]", counted_part(dir.join("two.png").to_str().unwrap(), "two.png", second.len() as u64, meter.clone()).unwrap());
        let resp = reqwest::Client::new().post(format!("http://{addr}/")).multipart(form).send().await.unwrap();
        assert!(resp.status().is_success());

        let seen = server.await.unwrap();
        let text = String::from_utf8_lossy(&seen);
        assert!(text.contains("name=\"files[0]\"") && text.contains("filename=\"one.png\""));
        assert!(text.contains("name=\"files[1]\"") && text.contains("filename=\"two.png\""));
        assert!(seen.windows(1000).any(|w| w.iter().all(|&b| b == b'a')), "the first file's bytes arrived");
        assert!(seen.windows(1000).any(|w| w.iter().all(|&b| b == b'b')), "the second file's bytes arrived");

        // Reported as it went: counts that only rise, then the wait for the answer.
        let mut counts = Vec::new();
        let mut last_phase = String::new();
        while let Ok(event) = rx.try_recv() {
            last_phase = event.data["phase"].as_str().unwrap_or_default().to_string();
            if let Some(sent) = event.data["sent"].as_u64() {
                assert_eq!(event.data["total"].as_u64(), Some(total));
                counts.push(sent);
            }
        }
        assert!(counts.len() >= 3, "progress was reported along the way: {counts:?}");
        assert!(counts.windows(2).all(|w| w[0] <= w[1]), "counts only rise: {counts:?}");
        assert!(counts.iter().all(|&c| c < total), "the last of it is the wait, not more sending: {counts:?}");
        assert_eq!(last_phase, "waiting");
        let _ = std::fs::remove_dir_all(&dir);
    }
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
        let resp = super::http_client_for(&token)
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

        let messages: serde_json::Value = super::http_client_for(&token)
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
        http_client_for(token)
        .patch(format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}"))
        .header("Authorization", token)
        .json(&json!({ "content": body }))
        )
    .await
        .context("editing Discord message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", refusal_text(status, &text));
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
        http_client_for(token)
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
        http_client_for(token)
        .post(format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}/ack"))
        .header("Authorization", token)
        .json(&json!({ "token": serde_json::Value::Null }))
        )
    .await
        .context("acking Discord read state")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", refusal_text(status, &text));
    }
    Ok(())
}

pub async fn delete_message(state: &AppState, buffer_id: &str, token: &str, msg_id: &str) -> Result<()> {
    let channel_id = state
        .runtime
        .get_discord_channel(buffer_id)
        .ok_or_else(|| anyhow!("no known Discord channel for this buffer"))?;
    let resp = send_write(
        http_client_for(token)
        .delete(format!("{API_BASE}/channels/{channel_id}/messages/{msg_id}"))
        .header("Authorization", token)
        )
    .await
        .context("deleting Discord message")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", refusal_text(status, &text));
    }
    Ok(())
}

/// Discord's reaction endpoint wants a custom emoji as `name:id` (no
/// angle brackets, no leading `a:` animated marker), but every reaction
/// this app already tracks (extract_reactions above, the live
/// MESSAGE_REACTION_ADD/REMOVE handling) stores it wrapped as `<:name:id>` -
/// that's the one place besides here needing the raw form, so it's
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

    let client = http_client_for(token);
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

/// What a refused write says, with AutoMod's refusal put in words: the
/// message was not sent because a rule in this server stopped it, which is a
/// different thing to do something about than a network error.
fn refusal_text(status: reqwest::StatusCode, text: &str) -> String {
    let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    match parsed["code"].as_u64() {
        // 200000 a message, 200001 a forum post's title.
        Some(200_000) | Some(200_001) => {
            let what = if parsed["code"].as_u64() == Some(200_001) { "title" } else { "message" };
            let said = parsed["message"].as_str().filter(|m| !m.is_empty() && !m.contains("blocked by AutoMod"));
            match said {
                Some(reason) => format!("AutoMod in this server blocked the {what}: {reason}"),
                None => format!("AutoMod in this server blocked the {what}"),
            }
        }
        _ => format!("Discord API error {status}: {text}"),
    }
}

/// A duration as a person says it: "10 minutes", "1 hour", "45 seconds".
pub(super) fn describe_seconds(secs: u64) -> String {
    let (n, unit) = if secs.is_multiple_of(86_400) {
        (secs / 86_400, "day")
    } else if secs.is_multiple_of(3600) {
        (secs / 3600, "hour")
    } else if secs.is_multiple_of(60) {
        (secs / 60, "minute")
    } else {
        (secs, "second")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod refusal_tests {
    use super::*;

    #[test]
    fn an_automod_block_is_said_as_one() {
        let body = r#"{"message": "Message was blocked by AutoMod", "code": 200000}"#;
        assert_eq!(refusal_text(reqwest::StatusCode::BAD_REQUEST, body), "AutoMod in this server blocked the message");
        let other = r#"{"message": "Missing Permissions", "code": 50013}"#;
        assert!(refusal_text(reqwest::StatusCode::FORBIDDEN, other).starts_with("Discord API error 403"));
        assert_eq!(describe_seconds(600), "10 minutes");
        assert_eq!(describe_seconds(3600), "1 hour");
        assert_eq!(describe_seconds(45), "45 seconds");
    }
}
