//! Turning a Discord message object into one of ours.
//!
//! Everything a message can carry and this client can draw: its body, the
//! things attached to it, the message it answers, the message it forwards,
//! its stickers, its poll, its reactions, its buttons. All reading, no
//! writing - what goes the other way lives in `send`.
//!
//! The pin list belongs here for the same reason: a pin is a message, and
//! what a pinned one needs is a way to describe itself in a list.

use super::*;

/// The messages a channel has pinned, as this client's own message shape.
///
/// Asked of Discord every time rather than kept: a pin is set by whoever is
/// running the channel, often while nobody here is looking, and a cached list
/// would be a list of what was pinned when this window last happened to ask.
/// The same messages are usually already in scrollback, but not always - a
/// channel's rules are typically pinned years before anybody reads them.
pub async fn list_pinned(state: &AppState, account_id: &str, buffer_id: &str) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    let resp = http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/pins"))
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("reading the pins")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "reading the pins"));
    }
    let messages: Vec<Value> = resp.json().await.context("reading the pins")?;

    // Discord returns newest first, which is the order a pin list is read in:
    // the thing pinned most recently is the thing being talked about now.
    let own_display_name = cfg.display_name.clone();
    let pinned: Vec<Value> = messages
        .iter()
        .filter_map(|msg| {
            let id = msg["id"].as_str()?;
            let author = &msg["author"];
            let from = author["global_name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| author["username"].as_str())
                .unwrap_or("unknown");
            let body = resolve_mentions(&extract_body(msg).unwrap_or_default(), msg, &cfg.user_id, own_display_name.as_deref());
            let ts = msg["timestamp"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.timestamp())
                .unwrap_or(0);
            Some(json!({
                "id": id,
                "bufferId": buffer_id,
                "from": from,
                // An attachment or an embed with no words is a real pin and a
                // common one - a picture everybody is meant to have seen - so
                // it is listed saying what it is rather than as a blank row.
                "body": if body.trim().is_empty() { describe_wordless(msg) } else { body },
                "ts": ts,
                "kind": "chat",
            }))
        })
        .collect();
    let ids: Vec<&str> = pinned.iter().filter_map(|m| m["id"].as_str()).collect();
    // Told as well as answered, so the header's count is right for every
    // window watching this conversation rather than only the one that asked.
    state.events.emit("pinnedMessages", json!({ "bufferId": buffer_id, "pinned": ids }));
    Ok(json!({ "bufferId": buffer_id, "pinned": pinned }))
}

/// What to call a message that has no words in it.
pub(super) fn describe_wordless(msg: &Value) -> String {
    let attachments = msg["attachments"].as_array().map(|a| a.len()).unwrap_or(0);
    let embeds = msg["embeds"].as_array().map(|a| a.len()).unwrap_or(0);
    match (attachments, embeds) {
        (0, 0) => "(no text)".to_string(),
        (1, _) => "(an attachment)".to_string(),
        (n, _) if n > 1 => format!("({n} attachments)"),
        _ => "(a link)".to_string(),
    }
}

/// Pins a message, or takes the pin off.
///
/// Whether this account may is Discord's decision - MANAGE_MESSAGES in a
/// guild, and anybody in a DM - so it is asked rather than worked out here,
/// and a refusal is passed through in Discord's own words.
pub async fn set_pinned(state: &AppState, account_id: &str, buffer_id: &str, message_id: &str, pinned: bool) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    let url = format!("{API_BASE}/channels/{channel_id}/pins/{message_id}");
    let http = http_client();
    let request = if pinned { http.put(url) } else { http.delete(url) };
    let doing = if pinned { "pinning the message" } else { "unpinning the message" };
    let resp = request.header("Authorization", &cfg.token).send().await.context(doing)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, doing));
    }
    // Read back, so the header's count and the menu's wording agree with what
    // just happened. Discord sends no pin event to a user client, so nothing
    // else would tell this window - or any other one - that the list changed.
    if let Err(e) = list_pinned(state, account_id, buffer_id).await {
        tracing::debug!("discord: re-reading the pins: {e:#}");
    }
    Ok(())
}

/// Builds a `https://discord.com/channels/...` deep link to a specific
/// message - the fallback for an attachment link whose signature has
/// expired (see runtime.rs's discord_guild_id doc comment for why: every
/// `cdn.discordapp.com`/`media.discordapp.net` link Discord hands out is
/// signed with an `ex=`/`is=`/`hm=` query string that lapses roughly 24h
/// after issue, and Discord's own `/attachments/refresh-urls` endpoint -
/// which a real client uses to silently re-sign one on demand - hard-
/// rejects this backend's user-token requests with a blanket 401 even for
/// a URL that hasn't expired yet, confirmed live against both a genuinely
/// expired and a genuinely fresh URL; it isn't a missing-header problem,
/// it's a client fingerprint gate this backend has no way to pass). Opening
/// the real message in an actual Discord client instead always works,
/// since that client gets its own freshly-signed link. `guild_id` is
/// `None` for a DM (no guild at all - `@me` is Discord's own link
/// convention there).
pub fn message_link(channel_id: &str, guild_id: Option<&str>, message_id: &str) -> String {
    format!("https://discord.com/channels/{}/{channel_id}/{message_id}", guild_id.unwrap_or("@me"))
}

/// Discord sometimes puts the actually-useful content in `embeds` rather
/// than `content`/`attachments`, and treating those two fields as the
/// whole message (as this used to) silently drops real content in two
/// confirmed-live cases:
///
/// - A plain link (e.g. a klipy.com GIF page) gets server-side resolved
///   into a rich embed carrying the *actual* playable media URL (often
///   with a real file extension, unlike the original link) in
///   `embed.video.url` / `embed.image.url` - our own extension-based
///   embed detection has nothing to match against the original bare link,
///   but matches these resolved URLs fine.
/// - Some bot/webhook messages (bridges, log relays) ship an empty
///   `content` with their entire payload in an embed's `description`
///   (confirmed against a real Sneedchat IRC-bridge webhook message) -
///   without this, those messages showed up as blank.
///
/// Attachments beyond the first are also now included (previously only
/// `attachments[0]` was ever read, silently dropping additional files).
/// Returns None only when there is truly nothing renderable at all.
pub(super) fn extract_body(d: &Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let content = d["content"].as_str().unwrap_or("");
    if !content.is_empty() {
        parts.push(content.to_string());
    }
    // A forwarded message carries nothing of its own: the thing forwarded is
    // in `message_snapshots`, and `content` is empty or a line somebody added
    // above it. Reading only `content` is why a forward arrived as a name
    // with nothing under it.
    for quoted in forwarded_text(d) {
        parts.push(quoted);
    }
    // A poll is the message when there is one, and a sticker this client
    // cannot draw is at least a message that arrived.
    if let Some(poll) = extract_poll(d) {
        parts.push(poll);
    }
    for name in undrawable_sticker_names(d) {
        parts.push(format!("sent a sticker: {name}"));
    }
    if let Some(embeds) = d["embeds"].as_array() {
        for embed in embeds {
            // What Discord unfurled to get this embed. When that link is
            // already in the message, adding the embed's own media URL says
            // the same thing twice: a posted YouTube link arrives as the
            // watch URL in the content and the /embed/ URL here, and a client
            // that unfurls links sees two videos where a person posted one.
            //
            // Only skipped when the source link is actually present. An embed
            // with no `url`, or one whose source is not in the text, is the
            // only thing carrying that media and still has to be passed on.
            if let Some(source) = embed["url"].as_str() {
                if content.contains(source) {
                    continue;
                }
            }
            if let Some(url) = embed["video"]["url"].as_str() {
                parts.push(url.to_string());
            } else if let Some(url) = embed["image"]["url"].as_str() {
                parts.push(url.to_string());
            } else if embed["type"].as_str() == Some("gifv") {
                if let Some(url) = embed["thumbnail"]["url"].as_str() {
                    parts.push(url.to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Discord message flag 1 << 13: this message is somebody speaking.
///
/// Named rather than written as 8192 at the point of use, because the number
/// says nothing and this is the only thing that distinguishes a voice message
/// from an ordinary audio attachment.
const IS_VOICE_MESSAGE: u64 = 1 << 13;

/// Discord's own `attachments` array, kept structured instead of being
/// flattened into body text. Discord already reports filename, size,
/// dimensions and content type per file, so there is nothing to infer - and
/// unlike Matrix or Sneedchat these URLs are directly loadable by a frontend,
/// so no local cache copy is needed and `path` stays unset.
/// Whether a Discord message carries nothing worth storing.
///
/// Discord emits messages that are pure protocol noise (a pin notice, a
/// thread-created marker), and those genuinely have nothing to show. An
/// uncaptioned picture is not one of them: its body is empty because the
/// content is the attachment. Attachment URLs were once appended to the body,
/// which hid this distinction - testing the body alone now drops the most
/// ordinary kind of image post there is.
pub(super) fn is_empty_message(body: &str, embeds: &[Embed], attachments: &[Attachment]) -> bool {
    body.is_empty() && embeds.is_empty() && attachments.is_empty()
}

#[cfg(test)]
mod voice_tests {
    use super::extract_attachments;
    use serde_json::json;

    /// A voice message as Discord actually sends one, taken from the shape in
    /// this account's own scrollback: an ogg named voice-message.ogg, with the
    /// length and the waveform on the attachment and the flag on the message.
    fn spoken() -> serde_json::Value {
        json!({
            "flags": 8192,
            "attachments": [{
                "url": "https://cdn.discordapp.com/attachments/1/2/voice-message.ogg",
                "filename": "voice-message.ogg",
                "content_type": "audio/ogg",
                "size": 453003,
                "duration_secs": 12.4,
                "waveform": "AAAICBAQGBggIA=="
            }]
        })
    }

    #[test]
    fn a_voice_message_says_that_it_is_one() {
        let att = &extract_attachments(&spoken())[0];
        assert!(att.voice, "the flag was not read");
        assert_eq!(att.duration_secs, Some(12.4));
        assert_eq!(att.waveform.as_deref(), Some("AAAICBAQGBggIA=="));
        // Still an audio attachment underneath, so everything that already
        // knew how to play one still can.
        assert_eq!(att.kind, "audio");
    }

    /// The distinction that has to survive: an .ogg somebody attached on
    /// purpose is a file they sent, and drawing it as a voice message would
    /// claim they spoke it.
    #[test]
    fn an_ordinary_sound_file_is_not_a_voice_message() {
        let mut ordinary = spoken();
        ordinary["flags"] = json!(0);
        let att = &extract_attachments(&ordinary)[0];
        assert!(!att.voice);
        assert_eq!(att.kind, "audio");
    }

    /// Discord sets the flag on the message, so every attachment on it is part
    /// of the same recording - but a message with no flag and no waveform must
    /// come through exactly as it always did.
    #[test]
    fn a_picture_is_untouched_by_any_of_this() {
        let picture = json!({
            "attachments": [{
                "url": "https://cdn.discordapp.com/attachments/1/2/cat.png",
                "filename": "cat.png",
                "content_type": "image/png",
                "size": 100,
                "width": 800,
                "height": 600
            }]
        });
        let att = &extract_attachments(&picture)[0];
        assert_eq!(att.kind, "image");
        assert!(!att.voice);
        assert_eq!(att.duration_secs, None);
        assert_eq!(att.waveform, None);
    }
}

#[cfg(test)]
mod forwarded_tests {
    use super::{extract_attachments, extract_body, extract_reply, forwarded_attachments};
    use serde_json::json;

    /// A forward as Discord actually sends one: nothing in `content`, and
    /// the message itself under `message_snapshots`.
    fn forward(content: &str) -> serde_json::Value {
        json!({
            "content": content,
            "message_reference": { "type": 1, "channel_id": "1", "message_id": "2" },
            "message_snapshots": [{
                "message": {
                    "content": "the thing that was forwarded",
                    "attachments": [],
                    "embeds": []
                }
            }]
        })
    }

    #[test]
    fn a_forward_carries_the_message_it_brought() {
        // The bug: this arrived as an author with nothing under them.
        let body = extract_body(&forward("")).expect("a forward is not an empty message");
        assert_eq!(body, "> the thing that was forwarded");
    }

    #[test]
    fn a_comment_above_a_forward_keeps_its_place() {
        let body = extract_body(&forward("look at this")).unwrap();
        assert_eq!(body, "look at this\n> the thing that was forwarded");
    }

    #[test]
    fn a_forward_says_that_it_is_one() {
        // Said in the row above the message rather than in the body, which is
        // where a reply's own line goes - and with its own word, because a
        // forward is not an answer to anything.
        let preview = extract_reply(&forward("")).expect("a forward names what it brought");
        assert!(preview.forwarded);
        assert_eq!(preview.from, "Forwarded");
    }

    #[test]
    fn a_forwarded_picture_arrives_as_a_picture() {
        let d = json!({
            "content": "",
            "message_snapshots": [{
                "message": {
                    "content": "",
                    "attachments": [{
                        "url": "https://cdn.discordapp.com/a.png",
                        "filename": "a.png",
                        "content_type": "image/png",
                        "size": 100
                    }]
                }
            }]
        });
        let atts = forwarded_attachments(&d);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].kind, "image");
        // The message is not left blank either, since a client that draws no
        // attachment would otherwise show nothing at all.
        assert_eq!(extract_body(&d).as_deref(), Some("> (forwarded 1 attachment)"));
        // And the message's own attachments are untouched by this.
        assert!(extract_attachments(&d).is_empty());
    }

    #[test]
    fn a_forward_is_not_drawn_as_a_reply() {
        // What produced the arrow: a forward has a message_reference, so it
        // was read as a reply to a message Discord never resolves - drawn as
        // "replying to" with no author and no text beside it.
        let d = json!({
            "content": "",
            "message_reference": { "type": 1, "channel_id": "1", "message_id": "2" },
            "message_snapshots": [{ "message": { "content": "brought from elsewhere", "attachments": [] } }]
        });
        let forwarded = extract_reply(&d).expect("a forward still names what it brought");
        assert!(forwarded.forwarded);
        assert!(forwarded.body.is_empty());
        // An actual reply still is one.
        let reply = json!({
            "content": "yes",
            "message_reference": { "type": 0, "message_id": "2" },
            "referenced_message": { "content": "the question", "author": { "username": "asker" } }
        });
        let preview = extract_reply(&reply).expect("a reply is still a reply");
        assert_eq!(preview.from, "asker");
    }

    #[test]
    fn a_message_that_forwards_nothing_is_unchanged() {
        let plain = json!({ "content": "hello", "attachments": [], "embeds": [] });
        assert_eq!(extract_body(&plain).as_deref(), Some("hello"));
        assert!(forwarded_attachments(&plain).is_empty());
    }
}

#[cfg(test)]
mod sticker_and_poll_tests {
    use super::{extract_body, extract_poll, extract_stickers, is_empty_message};
    use serde_json::json;

    /// A sticker-only message carries no content, no embed and no attachment,
    /// so it used to be dropped whole - it arrived as nothing at all, which
    /// is the worst way for a message to go missing: nobody knows to look.
    #[test]
    fn a_sticker_only_message_is_a_message() {
        let d = json!({ "content": "", "sticker_items": [{ "id": "42", "name": "sad cat", "format_type": 1 }] });
        let stickers = extract_stickers(&d);
        assert_eq!(stickers.len(), 1);
        assert_eq!(stickers[0].filename.as_deref(), Some("sad cat.png"));
        assert_eq!(stickers[0].url.as_deref(), Some("https://media.discordapp.net/stickers/42.png"));
        assert!(!is_empty_message("", &[], &stickers));
    }

    #[test]
    fn an_animated_sticker_keeps_its_own_format() {
        let d = json!({ "sticker_items": [{ "id": "7", "name": "dance", "format_type": 4 }] });
        assert_eq!(extract_stickers(&d)[0].url.as_deref(), Some("https://media.discordapp.net/stickers/7.gif"));
    }

    /// Lottie is a vector animation format nothing here can draw, so the
    /// sticker becomes its name - not the sticker, but a message that
    /// arrived, which is the whole point.
    #[test]
    fn a_sticker_we_cannot_draw_becomes_its_name() {
        let d = json!({ "content": "", "sticker_items": [{ "id": "9", "name": "wave", "format_type": 3 }] });
        assert!(extract_stickers(&d).is_empty());
        assert_eq!(extract_body(&d).as_deref(), Some("sent a sticker: wave"));
    }

    /// A poll-only message used to arrive as nothing, so a channel went quiet
    /// in the middle of a decision being made.
    #[test]
    fn a_poll_arrives_as_its_question_and_options() {
        let d = json!({
            "content": "",
            "poll": {
                "question": { "text": "Pizza?" },
                "answers": [
                    { "poll_media": { "text": "Yes" } },
                    { "poll_media": { "text": "No", "emoji": { "name": "🙅" } } }
                ]
            }
        });
        let body = extract_body(&d).expect("a poll is a message");
        assert!(body.contains("Pizza?"), "{body}");
        assert!(body.contains("• Yes"), "{body}");
        assert!(body.contains("• 🙅 No"), "{body}");
    }

    /// A marker alone on a line says less than nothing.
    #[test]
    fn a_poll_with_no_answers_is_not_shown() {
        assert_eq!(extract_poll(&json!({ "poll": { "question": { "text": "?" }, "answers": [] } })), None);
        assert_eq!(extract_poll(&json!({ "content": "hi" })), None);
    }
}

/// The stickers on a message, as pictures.
///
/// A sticker-only message carries no content, no embed and no attachment, so
/// it used to be dropped whole by is_empty_message - it arrived as nothing at
/// all, which is the worst way for a message to go missing: nobody knows to
/// look for it.
///
/// Lottie stickers are vector animations in a JSON format nothing here can
/// draw, so those become their name in the body instead. A named sticker is
/// not the sticker, but it is a message that arrived.
pub(super) fn extract_stickers(d: &Value) -> Vec<Attachment> {
    d["sticker_items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sticker| {
            let id = sticker["id"].as_str()?;
            let name = sticker["name"].as_str().unwrap_or("sticker");
            // 1 PNG, 2 APNG, 3 Lottie, 4 GIF - Discord's own numbering.
            let (extension, mimetype) = match sticker["format_type"].as_i64().unwrap_or(1) {
                4 => ("gif", "image/gif"),
                3 => return None,
                _ => ("png", "image/png"),
            };
            Some(Attachment {
                kind: "image".to_string(),
                filename: Some(format!("{name}.{extension}")),
                url: Some(format!("https://media.discordapp.net/stickers/{id}.{extension}")),
                mimetype: Some(mimetype.to_string()),
                ..Default::default()
            })
        })
        .collect()
}

/// A poll, as the text of the question and its options.
///
/// This is the record of what was asked, written into the log. Answering one
/// is a separate thing in `polls.rs`, which draws the same poll on a card -
/// the log keeps what was asked, the card is the part that can be acted on.
///
/// Worth having on its own even so: a poll-only message used to arrive as
/// nothing, so a channel would go quiet in the middle of a decision being
/// made.
pub(super) fn extract_poll(d: &Value) -> Option<String> {
    let poll = d.get("poll").filter(|p| p.is_object())?;
    let question = poll["question"]["text"].as_str().unwrap_or("Poll");
    let mut out = format!("📊 {question}");
    for answer in poll["answers"].as_array().into_iter().flatten() {
        let text = answer["poll_media"]["text"].as_str().unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let emoji = answer["poll_media"]["emoji"]["name"].as_str().unwrap_or("");
        out.push_str(&format!("\n• {emoji}{}{text}", if emoji.is_empty() { "" } else { " " }));
    }
    // A Lottie sticker or an empty poll would leave the marker alone on a
    // line, which says less than nothing.
    (out.lines().count() > 1).then_some(out)
}

/// The name of a sticker nothing here can draw - a Lottie animation.
pub(super) fn undrawable_sticker_names(d: &Value) -> Vec<String> {
    d["sticker_items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["format_type"].as_i64() == Some(3))
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect()
}

/// What was forwarded, as lines quoting it.
///
/// Discord puts the forwarded message in `message_snapshots` - a copy of it
/// taken at the moment of forwarding, deliberately frozen, which is why it
/// carries no author: a forward is the *message*, not the person. So it is
/// rendered as a quote rather than attributed to somebody who is not named.
///
/// Recursive in the protocol and not here: a forward of a forward carries
/// its own snapshot, and one level is what Discord itself draws.
pub(super) fn forwarded_text(d: &Value) -> Vec<String> {
    let Some(snapshots) = d["message_snapshots"].as_array() else { return Vec::new() };
    snapshots
        .iter()
        .filter_map(|snapshot| {
            let message = &snapshot["message"];
            // Just the message. That it is a forward, and whose it was, is
            // said in the row above it - see extract_reply and name_forward -
            // rather than as a line of body text pretending to be one.
            let mut parts: Vec<String> = Vec::new();
            if let Some(text) = message["content"].as_str().filter(|t| !t.is_empty()) {
                // Quoted, because that is what it is: something said
                // elsewhere, brought here.
                parts.extend(text.lines().map(|line| format!("> {line}")));
            }
            // What came with it. The pictures themselves are lifted into the
            // message's own attachments below; this is for the case where a
            // forward is *only* a picture, so the line is not empty.
            let files = message["attachments"].as_array().map(|a| a.len()).unwrap_or(0);
            if parts.is_empty() && files > 0 {
                parts.push(format!("> (forwarded {files} attachment{})", if files == 1 { "" } else { "s" }));
            }
            if parts.is_empty() {
                return None;
            }
            Some(parts.join("\n"))
        })
        .collect()
}

/// What a forward says before anybody has looked up who wrote it.
pub const FORWARD_MARK: &str = "Forwarded";

/// Fills in who wrote the forwarded message, and where from.
///
/// Discord's snapshot carries the message and not its author - so the only
/// way to say whose words these are is to read the original, which this
/// account can do exactly when it can see where the message came from. Where
/// it cannot, the line stays as it was: "Forwarded" and no claim about who.
///
/// Done afterwards rather than before, so a forward appears at once and gains
/// its attribution a moment later, the same way a late link preview does.
pub async fn name_forward(state: &AppState, account_id: &str, buffer_id: &str, msg_id: &str, reference: &Value) {
    let (Some(source_channel), Some(source_message)) =
        (reference["channel_id"].as_str(), reference["message_id"].as_str())
    else {
        return;
    };
    let Some(cfg) = state.accounts.get_discord(account_id) else { return };
    let resp = http_client()
        .get(format!("{API_BASE}/channels/{source_channel}/messages?limit=1&around={source_message}"))
        .header("Authorization", &cfg.token)
        .send()
        .await;
    // A source this account cannot see is the ordinary case for a forward
    // out of somebody else's server, and not worth a word to anybody.
    let Ok(resp) = resp else { return };
    if !resp.status().is_success() {
        return;
    }
    let Ok(found) = resp.json::<Value>().await else { return };
    let original = found
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["id"].as_str() == Some(source_message));
    let Some(original) = original else { return };
    let author = &original["author"];
    let Some(name) = author["global_name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .or_else(|| author["username"].as_str())
        .filter(|n| !n.is_empty())
    else {
        return;
    };
    // And where from, where that is a place with a name. A direct message is
    // named as one rather than by its id, which would mean nothing to anybody.
    // Where from, where that is somewhere this client knows the name of. A
    // buffer's name is what a person calls the place; a channel id is not.
    let place = match state
        .runtime
        .discord_buffer_for_channel(account_id, source_channel)
        .and_then(|buffer| state.runtime.get_buffer(&buffer))
        .map(|buffer| buffer.name)
    {
        Some(name) => format!(" in {}", name.rsplit('/').next().unwrap_or(&name).to_string()),
        None if original["guild_id"].is_null() => " from a direct message".to_string(),
        None => String::new(),
    };
    state.runtime.rename_reply_preview(state, buffer_id, msg_id, &format!("{FORWARD_MARK} from {name}{place}"));
}

/// The files inside a forwarded message, as attachments of this one.
///
/// A forwarded picture is a picture: it belongs in the message the way any
/// other attachment does, rather than being described in words while the
/// image itself is dropped.
pub(super) fn forwarded_attachments(d: &Value) -> Vec<Attachment> {
    d["message_snapshots"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|snapshot| extract_attachments(&snapshot["message"]))
        .collect()
}

pub(super) fn extract_attachments(d: &Value) -> Vec<Attachment> {
    let Some(atts) = d["attachments"].as_array() else { return Vec::new() };
    // Whether somebody spoke this message rather than attaching a sound file.
    // The flag is on the message, the recording is on the attachment, so it is
    // read once here and carried down to each.
    let spoken = d["flags"].as_u64().unwrap_or(0) & IS_VOICE_MESSAGE != 0;
    atts.iter()
        .filter_map(|att| {
            let url = att["url"].as_str()?;
            let mimetype = att["content_type"].as_str().map(str::to_string);
            let kind = match mimetype.as_deref().unwrap_or("") {
                m if m.starts_with("image/") => "image",
                m if m.starts_with("video/") => "video",
                m if m.starts_with("audio/") => "audio",
                _ => "file",
            };
            Some(Attachment {
                kind: kind.to_string(),
                filename: att["filename"].as_str().map(str::to_string),
                size: att["size"].as_u64(),
                width: att["width"].as_u64().map(|v| v as u32),
                height: att["height"].as_u64().map(|v| v as u32),
                url: Some(url.to_string()),
                mimetype,
                // Both only ever appear on a voice message, and both were being
                // dropped: what arrived was an audio file with no indication of
                // what it was, no length, and no picture of the sound.
                duration_secs: att["duration_secs"].as_f64(),
                waveform: att["waveform"].as_str().map(str::to_string),
                // The flag decides, not the shape of the attachment. An .ogg
                // somebody uploaded on purpose is a file they sent, and calling
                // it a voice message would draw it as something it is not.
                voice: spoken,
                ..Default::default()
            })
        })
        .collect()
}

/// A rich embed's title/description/color/timestamp/url, structured
/// rather than flattened into plain body text (see model.rs's Embed doc
/// comment - this replaced extract_body's old title+description dump).
/// Only embeds that actually carry a title or description become one of
/// these; a pure image/video/gifv embed has neither and is already fully
/// represented by extract_body's own media-URL handling above, so
/// including it here too would just render an empty box under the media.
pub(super) fn extract_embeds(d: &Value) -> Vec<Embed> {
    let Some(embeds) = d["embeds"].as_array() else { return Vec::new() };
    embeds
        .iter()
        .filter_map(|embed| {
            let title = embed["title"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string());
            let description = embed["description"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string());
            if title.is_none() && description.is_none() {
                return None;
            }
            Some(Embed {
                title,
                description,
                color: embed["color"].as_i64(),
                timestamp: embed["timestamp"].as_str().map(|s| s.to_string()),
                url: embed["url"].as_str().map(|s| s.to_string()),
                // Discord's own thumbnails are already reached through the
                // message's media sniffing, which pairs them with the embed
                // in the frontend. Nothing to fetch here.
                image_url: None,
            })
        })
        .collect()
}

/// Discord-native replies: `message_reference.message_id` names what's
/// Discord's raw `<@id>`/`<@!id>` mention tokens resolved to `@name` using
/// the message's own `mentions` array (already carries id/username/
/// global_name for everyone pinged - no extra lookup needed). When the
/// mentioned id is *this* account's own user, the local display-name
/// override (config.display_name, see accounts.rs's set_display_name) is
/// substituted instead of Discord's real name - purely a local rendering
/// choice, never sent anywhere.
/// Builds a message author's real avatar CDN URL from their `author`
/// object (works identically whether that author is someone else or this
/// account's own user - Discord's gateway echo of a self-sent message
/// carries the same full `author` object as any other message). `None`
/// when the user has no avatar hash set (a legacy/never-customized
/// account) - the frontend falls back to a colored initial in that case,
/// same as it already does for IRC.
pub(super) fn author_avatar_url(author: &Value) -> Option<String> {
    let id = author["id"].as_str()?;
    let hash = author["avatar"].as_str()?;
    let ext = if hash.starts_with("a_") { "gif" } else { "png" };
    Some(format!("https://cdn.discordapp.com/avatars/{id}/{hash}.{ext}"))
}

pub(super) fn resolve_mentions(body: &str, d: &Value, own_user_id: &str, own_display_name: Option<&str>) -> String {
    let Some(mentions) = d["mentions"].as_array() else { return body.to_string() };
    let mut out = body.to_string();
    for m in mentions {
        let Some(id) = m["id"].as_str() else { continue };
        let name = if id == own_user_id {
            own_display_name
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| m["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| m["username"].as_str()).unwrap_or("you").to_string())
        } else {
            m["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| m["username"].as_str()).unwrap_or("someone").to_string()
        };
        out = out.replace(&format!("<@{id}>"), &format!("@{name}"));
        out = out.replace(&format!("<@!{id}>"), &format!("@{name}"));
    }
    out
}

/// Whether this account's own user is directly pinged in the message -
/// read straight from Discord's own resolved `mentions` array rather than
/// reconstructed via substring matching, which is both more reliable and
/// the only way this would ever fire at all: the raw body still contains
/// `<@id>` tokens (not the account's nick) at the point highlight
/// detection needs an answer, so a generic nick-substring check (fine for
/// IRC, which has no structured mention data) can never match a Discord
/// mention.
pub(super) fn mentions_own_user(d: &Value, own_user_id: &str) -> bool {
    d["mentions"].as_array().map(|arr| arr.iter().any(|m| m["id"].as_str() == Some(own_user_id))).unwrap_or(false)
}

/// A cached snapshot of the message being replied to, taken at receive
/// time - not a live reference. Discord hands us the referenced message's
/// author/content inline with the reply itself (`referenced_message`), so
/// there's no need to look anything up, and the preview still means
/// something even if the original later scrolls out of local history or
/// gets deleted. `id` is what the frontend's "jump to" click targets if
/// the original happens to already be loaded.
pub(super) fn extract_reply(d: &Value) -> Option<ReplyPreview> {
    let reply_id = d["message_reference"]["message_id"].as_str()?;
    // A forward carries a reference too - to the message it brought here -
    // and it is not a reply to it. Same row on screen, different word in it:
    // read as a reply it drew the arrow a reply gets and nothing beside it,
    // since a forward resolves no referenced message. Type 1 is Discord's own
    // word for the difference.
    if d["message_reference"]["type"].as_i64() == Some(1) || d["message_snapshots"].is_array() {
        return Some(ReplyPreview {
            id: reply_id.to_string(),
            // Whose message it was is not in what Discord sent - the snapshot
            // has no author - so it is read afterwards and filled in here by
            // name_forward.
            from: FORWARD_MARK.to_string(),
            body: String::new(),
            thread: false,
            forwarded: true,
        });
    }
    let referenced = &d["referenced_message"];
    if referenced.is_null() {
        // Reference exists but Discord didn't resolve it - still worth a
        // "replying to a message" placeholder rather than nothing at all.
        return Some(ReplyPreview { id: reply_id.to_string(), ..Default::default() });
    }
    let author = &referenced["author"];
    let from = author["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| author["username"].as_str()).unwrap_or("unknown").to_string();
    let body = extract_body(referenced).unwrap_or_default();
    Some(ReplyPreview { id: reply_id.to_string(), from, body, thread: false, forwarded: false })
}

/// Shared by both the initial backfill and extend_history - Discord
/// returns messages newest-first; both store oldest-first so scrollback
/// reads top-to-bottom chronologically, matching getBacklog's contract.
pub(super) fn store_history_messages(state: &AppState, buffer_id: &str, messages: &[Value], user_id: &str, own_display_name: Option<&str>) {
    for msg in messages.iter().rev() {
        let Some(msg_id) = msg["id"].as_str() else { continue };
        let author = &msg["author"];
        let from = author["global_name"].as_str().filter(|s| !s.is_empty()).or_else(|| author["username"].as_str()).unwrap_or("unknown");
        let is_own = author["id"].as_str() == Some(user_id);
        let embeds = extract_embeds(msg);
        let mut attachments = extract_attachments(msg);
        attachments.extend(extract_stickers(msg));
        // A forwarded picture is a picture, and belongs in the message the
        // way any other attachment does.
        attachments.extend(forwarded_attachments(msg));
        let body = extract_body(msg).unwrap_or_default();
        if is_empty_message(&body, &embeds, &attachments) {
            continue;
        }
        let body = resolve_mentions(&body, msg, user_id, own_display_name);
        let reply_to = extract_reply(msg);
        let reactions = extract_reactions(msg);
        let avatar_url = author_avatar_url(author);
        let ts = msg["timestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        if let Err(e) = state.store.append_message(buffer_id, msg_id, from, &body, ts, false, false, "chat", reply_to.as_ref(), &reactions, is_own, avatar_url.as_deref(), &embeds, &attachments, author["id"].as_str(), None, &state.runtime.buffer_kind_of(buffer_id), None, &[]) {
            tracing::warn!("discord: storing history message: {e}");
            continue;
        }
        note_components(state, buffer_id, msg_id, msg);
        // A poll in the history may still be running - Discord's last hours
        // or days, so a channel opened for the first time is the commonest
        // place to meet one. Read oldest first, so the newest still-open poll
        // is the one left on the card.
        if polls::has_poll(msg) {
            polls::announce_if_open(state, buffer_id, msg_id, msg);
        }
        // Who wrote a message somebody forwarded, which the snapshot does not
        // carry. Backfill needs this as much as the live path: a conversation
        // read for the first time is all history, and a forward in it would
        // otherwise never say whose words it holds.
        if msg["message_snapshots"].is_array() {
            let state = state.clone();
            let account_id = buffer_id.split('|').next().unwrap_or_default().to_string();
            let buffer = buffer_id.to_string();
            let msg_id = msg_id.to_string();
            let reference = msg["message_reference"].clone();
            tokio::spawn(async move {
                name_forward(&state, &account_id, &buffer, &msg_id, &reference).await;
            });
        }
        // Backfilled messages need previews as much as live ones do - more so,
        // since a channel read for the first time is all history and none of it
        // would otherwise survive its links expiring.
        cache_thumbnails(state.clone(), buffer_id.to_string(), msg_id.to_string(), attachments);
    }
}

/// Same `<:name:id>` / plain-unicode emoji key convention as the live
/// REACTION_ADD/REMOVE handling - Discord already reports a snapshot
/// count and whether *we* are among the reactors, so this is a much
/// simpler direct read rather than reconstructing state incrementally.
pub(super) fn extract_reactions(msg: &Value) -> Vec<Reaction> {
    let Some(arr) = msg["reactions"].as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|r| {
            let count = r["count"].as_i64()?;
            let name = r["emoji"]["name"].as_str().unwrap_or("?");
            let is_custom = r["emoji"]["id"].as_str().is_some();
            let emoji = match r["emoji"]["id"].as_str() {
                Some(id) => format!("<:{name}:{id}>"),
                None => name.to_string(),
            };
            let me = r["me"].as_bool().unwrap_or(false);
            let animated = is_custom && r["emoji"]["animated"].as_bool().unwrap_or(false);
            Some(Reaction { emoji, count, me, animated })
        })
        .collect()
}

/// The buttons and menus on a message, in this client's own shape.
///
/// Discord lays them out in rows of up to five; the row is kept so a client
/// can draw them as they were arranged rather than as one long line, and the
/// type numbers are turned into words on the way through - a frontend should
/// not have to know that 2 means button.
pub(super) fn extract_components(msg: &Value) -> Vec<model::Component> {
    let Some(rows) = msg["components"].as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        // A row is itself a component (type 1) holding the real ones. A
        // message whose top level is already a control is not a shape Discord
        // sends, but reading it either way costs nothing.
        let children = row["components"].as_array().cloned().unwrap_or_else(|| vec![row.clone()]);
        for child in children {
            let kind = match child["type"].as_i64() {
                Some(2) => "button",
                // 3 is the plain string select; 5 through 8 are the ones that
                // pick users, roles, mentionables and channels. They are all
                // menus, and all of them answer the same way.
                Some(3) | Some(5) | Some(6) | Some(7) | Some(8) => "select",
                _ => continue,
            };
            let style = child["style"].as_i64().map(|s| match s {
                1 => "primary",
                2 => "secondary",
                3 => "success",
                4 => "danger",
                5 => "link",
                _ => "secondary",
            });
            let options = child["options"]
                .as_array()
                .map(|opts| {
                    opts.iter()
                        .filter_map(|o| {
                            Some(model::ComponentOption {
                                value: o["value"].as_str()?.to_string(),
                                label: o["label"].as_str().unwrap_or_default().to_string(),
                                description: o["description"].as_str().map(str::to_string),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(model::Component {
                kind: kind.to_string(),
                custom_id: child["custom_id"].as_str().map(str::to_string),
                label: child["label"].as_str().filter(|l| !l.is_empty()).map(str::to_string),
                style: style.map(str::to_string),
                url: child["url"].as_str().map(str::to_string),
                disabled: child["disabled"].as_bool().unwrap_or(false),
                // Written the way this client writes emoji everywhere else, so
                // a button's picture renders like one in a message does.
                emoji: match (child["emoji"]["name"].as_str(), child["emoji"]["id"].as_str()) {
                    (Some(name), Some(id)) => Some(format!("<:{name}:{id}>")),
                    (Some(name), None) => Some(name.to_string()),
                    _ => None,
                },
                options,
                placeholder: child["placeholder"].as_str().map(str::to_string),
                row: row_index as i64,
            });
        }
    }
    out
}

/// Stores a message's buttons and tells the client, if it has any.
///
/// Separate from the message itself because they arrive with it and are
/// written a moment later - see Store::set_components - and because a message
/// that gains or loses its buttons in an edit has to be able to say so.
pub(super) fn note_components(state: &AppState, buffer_id: &str, msg_id: &str, msg: &Value) {
    let components = extract_components(msg);
    if components.is_empty() {
        return;
    }
    if let Err(e) = state.store.set_components(buffer_id, msg_id, &components) {
        tracing::debug!("discord: keeping the buttons on {msg_id}: {e:#}");
        return;
    }
    state.events.emit(
        "messageComponents",
        json!({ "bufferId": buffer_id, "messageId": msg_id, "components": components }),
    );
}

#[cfg(test)]
mod component_tests {
    use super::extract_components;
    use serde_json::json;

    /// A message as Discord sends one: rows holding the controls, with the
    /// type numbers and style numbers this client turns into words.
    fn message() -> serde_json::Value {
        json!({
            "components": [
                { "type": 1, "components": [
                    { "type": 2, "style": 1, "label": "Accept", "custom_id": "accept" },
                    { "type": 2, "style": 4, "label": "Decline", "custom_id": "decline", "disabled": true },
                    { "type": 2, "style": 5, "label": "Read the rules", "url": "https://example.com/rules" }
                ]},
                { "type": 1, "components": [
                    { "type": 3, "custom_id": "role", "placeholder": "Pick a role", "options": [
                        { "label": "Red", "value": "red", "description": "the red one" },
                        { "label": "Blue", "value": "blue" }
                    ]}
                ]}
            ]
        })
    }

    #[test]
    fn buttons_and_menus_come_out_named_rather_than_numbered() {
        let out = extract_components(&message());
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].kind, "button");
        assert_eq!(out[0].label.as_deref(), Some("Accept"));
        assert_eq!(out[0].style.as_deref(), Some("primary"));
        assert_eq!(out[0].custom_id.as_deref(), Some("accept"));
        assert_eq!(out[1].style.as_deref(), Some("danger"));
        assert!(out[1].disabled);
        assert_eq!(out[2].style.as_deref(), Some("link"));
        assert_eq!(out[2].url.as_deref(), Some("https://example.com/rules"));
        assert_eq!(out[3].kind, "select");
        assert_eq!(out[3].placeholder.as_deref(), Some("Pick a role"));
        assert_eq!(out[3].options.len(), 2);
        assert_eq!(out[3].options[0].value, "red");
        assert_eq!(out[3].options[0].description.as_deref(), Some("the red one"));
    }

    /// The rows are kept, because how they were laid out is something the bot
    /// meant: five on one line and one below is not the same as six in a row.
    #[test]
    fn the_rows_survive() {
        let out = extract_components(&message());
        assert_eq!(out[0].row, 0);
        assert_eq!(out[2].row, 0);
        assert_eq!(out[3].row, 1);
    }

    #[test]
    fn a_custom_emoji_is_written_the_way_this_client_writes_them() {
        let msg = json!({ "components": [{ "type": 1, "components": [
            { "type": 2, "style": 2, "custom_id": "a", "emoji": { "name": "pepe", "id": "12345" } },
            { "type": 2, "style": 2, "custom_id": "b", "emoji": { "name": "👍" } }
        ]}]});
        let out = extract_components(&msg);
        assert_eq!(out[0].emoji.as_deref(), Some("<:pepe:12345>"));
        assert_eq!(out[1].emoji.as_deref(), Some("👍"));
    }

    /// Discord keeps adding component types. One this client has never heard
    /// of is skipped rather than drawn as an empty button.
    #[test]
    fn a_kind_this_does_not_know_is_left_alone() {
        let msg = json!({ "components": [{ "type": 1, "components": [
            { "type": 4, "custom_id": "text-input" },
            { "type": 2, "style": 2, "label": "Real", "custom_id": "real" }
        ]}]});
        let out = extract_components(&msg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label.as_deref(), Some("Real"));
    }

    /// The menus that pick people, roles and channels are menus too - they
    /// carry no options of their own, since the service fills them in.
    #[test]
    fn the_other_kinds_of_menu_are_menus() {
        for kind in [5, 6, 7, 8] {
            let msg = json!({ "components": [{ "type": 1, "components": [
                { "type": kind, "custom_id": "pick", "placeholder": "Choose" }
            ]}]});
            let out = extract_components(&msg);
            assert_eq!(out.len(), 1, "type {kind}");
            assert_eq!(out[0].kind, "select", "type {kind}");
        }
    }

    #[test]
    fn a_message_with_nothing_on_it_has_nothing_on_it() {
        assert!(extract_components(&json!({})).is_empty());
        assert!(extract_components(&json!({ "components": [] })).is_empty());
    }
}

#[cfg(test)]
mod embed_body_tests {
    use super::extract_body;
    use serde_json::json;

    /// A posted YouTube link comes back with an embed whose video URL is the
    /// /embed/ form of the same video. Appending it puts the same video in the
    /// message twice, and a client that unfurls links shows two of them.
    #[test]
    fn an_embed_for_a_link_already_in_the_message_adds_nothing() {
        let msg = json!({
            "content": "https://www.youtube.com/watch?v=g1Sq1Nr58hM",
            "embeds": [{
                "url": "https://www.youtube.com/watch?v=g1Sq1Nr58hM",
                "video": { "url": "https://www.youtube.com/embed/g1Sq1Nr58hM" }
            }]
        });
        assert_eq!(extract_body(&msg).as_deref(), Some("https://www.youtube.com/watch?v=g1Sq1Nr58hM"));
    }

    /// An embed nobody linked - Discord attaches these to its own system
    /// messages - is the only thing carrying that media, so it still comes
    /// through.
    #[test]
    fn an_embed_with_no_link_in_the_message_still_carries_its_media() {
        let msg = json!({
            "content": "look at this",
            "embeds": [{ "image": { "url": "https://cdn.example/pic.png" } }]
        });
        assert_eq!(extract_body(&msg).as_deref(), Some("look at this\nhttps://cdn.example/pic.png"));
    }

    /// The source link being absent from the text is the same situation: the
    /// embed is the only reference to it.
    #[test]
    fn an_embed_whose_source_is_not_in_the_text_is_kept() {
        let msg = json!({
            "content": "no links here",
            "embeds": [{
                "url": "https://example.com/article",
                "image": { "url": "https://example.com/hero.png" }
            }]
        });
        assert_eq!(extract_body(&msg).as_deref(), Some("no links here\nhttps://example.com/hero.png"));
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_discord_attachment_metadata_instead_of_flattening_it_to_a_url() {
        let d = json!({
            "content": "look at this",
            "attachments": [{
                "url": "https://cdn.discordapp.com/attachments/1/2/cat.png",
                "filename": "cat.png",
                "size": 12345,
                "width": 800,
                "height": 600,
                "content_type": "image/png"
            }]
        });
        let atts = extract_attachments(&d);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].kind, "image");
        assert_eq!(atts[0].filename.as_deref(), Some("cat.png"));
        assert_eq!(atts[0].mimetype.as_deref(), Some("image/png"));
        assert_eq!(atts[0].size, Some(12345));
        assert_eq!((atts[0].width, atts[0].height), (Some(800), Some(600)));
        // Directly loadable, so no local copy is made.
        assert!(atts[0].path.is_none());

        // The body keeps what the user actually typed - the attachment URL is
        // no longer appended to it.
        assert_eq!(extract_body(&d).as_deref(), Some("look at this"));
    }

    #[test]
    fn classifies_non_image_attachments_by_content_type() {
        let d = json!({ "attachments": [
            { "url": "https://cdn/x.mp4", "content_type": "video/mp4" },
            { "url": "https://cdn/x.ogg", "content_type": "audio/ogg" },
            { "url": "https://cdn/x.zip", "content_type": "application/zip" },
            { "url": "https://cdn/x.bin" }
        ]});
        let atts = extract_attachments(&d);
        let kinds: Vec<&str> = atts.iter().map(|a| a.kind.as_str()).collect();
        assert_eq!(kinds, ["video", "audio", "file", "file"]);
    }

    #[test]
    fn an_attachment_only_message_still_has_a_renderable_body_or_attachments() {
        // No text at all: the body is now empty rather than being the URL, so
        // the attachment list is the only thing carrying the message.
        let d = json!({ "content": "", "attachments": [{ "url": "https://cdn/a.png", "content_type": "image/png" }] });
        assert!(extract_body(&d).is_none());
        assert_eq!(extract_attachments(&d).len(), 1);
    }

    #[test]
    fn an_uncaptioned_picture_is_not_an_empty_message() {
        // The regression this guards: moving attachment URLs out of the body
        // made an uncaptioned image look empty, and both the live and history
        // paths dropped it instead of storing it.
        let d = json!({ "content": "", "attachments": [{ "url": "https://cdn/a.png", "content_type": "image/png" }] });
        let body = extract_body(&d).unwrap_or_default();
        assert!(!is_empty_message(&body, &extract_embeds(&d), &extract_attachments(&d)));
    }

    #[test]
    fn a_message_with_no_text_embeds_or_attachments_is_empty() {
        // Pin notices and thread markers really do have nothing to show.
        let d = json!({ "content": "", "attachments": [], "embeds": [] });
        let body = extract_body(&d).unwrap_or_default();
        assert!(is_empty_message(&body, &extract_embeds(&d), &extract_attachments(&d)));
    }

    #[test]
    fn unwraps_a_static_custom_emoji() {
        assert_eq!(reaction_path_segment("<:pepege:123456789>"), "pepege:123456789");
    }

    #[test]
    fn unwraps_an_animated_custom_emoji() {
        assert_eq!(reaction_path_segment("<a:vibing:987654321>"), "vibing:987654321");
    }

    #[test]
    fn leaves_a_plain_unicode_emoji_unchanged() {
        assert_eq!(reaction_path_segment("🔥"), "🔥");
    }
}
