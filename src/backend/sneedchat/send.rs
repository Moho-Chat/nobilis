//! Everything this client puts into a room.

use super::*;

/// Sends a message, optionally as a reply to someone.
///
/// Sneedchat has no reply field: the site's own client answers somebody by
/// opening the message with an `@Name,` mention, which is what its users and
/// its own notification rules recognise as being replied to. So a reply here
/// is that mention, added by the daemon rather than left to each frontend to
/// know the convention - and skipped when the message already opens with it,
/// so replying twice to the same person does not stack them up.
pub fn send_message(state: &AppState, account_id: &str, buffer_name: &str, body: &str, reply_to: Option<&str>) -> Result<()> {
    // Typed rather than chosen from a menu, which is what somebody used to the
    // site will do. Routed through the same path either way, so it is recorded
    // as a whisper here too - passed straight through it would be a real
    // whisper that this client never saw, since the site echoes none back.
    // Told who might be meant, because a name with a space in it cannot be
    // picked out of the text any other way - see parse_whisper_command_among.
    //
    // Two sources, because neither is enough alone: the room's roster is the
    // authority on who is here, and this site does not always send one - but
    // whoever has spoken lately is in the log either way, and is who somebody
    // is usually answering.
    let buffer_id = crate::model::buffer_id(account_id, buffer_name);
    let mut roster: Vec<String> = state
        .runtime
        .get_presence(&buffer_id)
        .and_then(|members| {
            members.as_array().map(|list| {
                list.iter().filter_map(|m| m["nick"].as_str().map(str::to_string)).collect()
            })
        })
        .unwrap_or_default();
    if let Ok(spoken) = state.store.recent_senders(&buffer_id, 200) {
        for name in spoken {
            if !roster.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                roster.push(name);
            }
        }
    }
    if let Some((target, text)) = protocol::parse_whisper_command_among(body, &roster) {
        return send_whisper(state, account_id, &target, &text, Some(buffer_name));
    }
    let body = as_reply(body, reply_to);
    let Some(text) = protocol::prepare_outgoing(&body) else { return Ok(()) };
    let sender = room_sender_for_buffer(state, account_id, buffer_name)?;
    sender.send(text).map_err(|_| anyhow!("chat socket closed"))?;
    Ok(())
}

/// Sends a private message to one person.
///
/// Goes out on whichever room connection is to hand: a whisper is not part of
/// any room's conversation, so any open socket carries it.
///
/// Recorded locally as well as sent. Unlike a room message, the site does not
/// echo a whisper back to whoever sent it, so without this you would see only
/// one side of your own conversation.
///
/// Recorded from *us*, with the target written into the line. There is no
/// field on a message for "and this went to Someone", and a whisper filed
/// under the recipient's name reads as something they said - which is what it
/// used to do. The `@name` is what the site puts there too, so the line reads
/// the way the same whisper reads on the site.
pub fn send_whisper(
    state: &AppState,
    account_id: &str,
    target: &str,
    body: &str,
    // `from_buffer`: the conversation it was sent from, so it appears there
    // rather than in every room. None falls back to all of them, which is
    // right for a caller that has no room in mind.
    from_buffer: Option<&str>,
) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        bail!("no one to whisper to");
    }
    let Some(text) = protocol::prepare_outgoing(body) else { return Ok(()) };
    let sender = any_room_sender(state, account_id)?;
    let wire = protocol::prepare_whisper(target, &text);
    // The addressing, not the message: enough to tell a wrong command from a
    // wrong target without putting somebody's private message in a log.
    tracing::debug!(
        "sneedchat[{account_id}]: sending whisper addressed {:?}",
        protocol::prepare_whisper(target, "")
    );
    sender.send(wire).map_err(|_| anyhow!("chat socket closed"))?;

    let me = state
        .accounts
        .get_sneedchat(account_id)
        .map(|c| c.display_name.filter(|n| !n.is_empty()).unwrap_or(c.username))
        .unwrap_or_default();
    let at = if target.starts_with('@') { "" } else { "@" };
    let named = target.trim_end_matches(',');
    record_whisper(state, account_id, &me, &format!("{at}{named}, {text}"), None, None, false, from_buffer);
    Ok(())
}

/// Opens a message with the mention Sneedchat treats as a reply.
pub(super) fn as_reply(body: &str, reply_to: Option<&str>) -> String {
    match reply_to {
        Some(nick) if !nick.is_empty() && !body.trim_start().starts_with(&format!("@{nick},")) => {
            format!("@{nick}, {}", body.trim_start())
        }
        _ => body.to_string(),
    }
}

/// The "+" attachment button's SneedChat backend. Unlike Discord (whose API
/// natively accepts a file alongside a message), Sneedchat's own chat
/// protocol is text-only - there's no upload endpoint on the site itself.
/// This does what regulars already do by hand: upload the file to a
/// separate anonymous image host, then post the resulting URL as a normal
/// chat message wrapped in the [img] BBCode the site itself expects for a
/// picture to actually render there - the same `[img]...[/img]` shape
/// find_attachment_url/normalizeBBCode already know how to unwrap on the
/// receiving end. Previously qu.ax; switched to postimg.cc after qu.ax
/// stopped reliably serving uploaded images back out.
///
/// Wrapped as `[url=<page>][img]<direct>[/img][/url]` rather than a bare
/// `[img]...[/img]` - postimg.cc's own "Thumbnail for forums" BBCode
/// preset (the one shown on its own result page) uses exactly this shape,
/// giving the posted image a click-through back to the postimg.cc page
/// alongside the inline preview, which a bare [img] tag wouldn't.
///
/// Two requests, not one: the upload POST only returns the *page* URL
/// (`https://postimg.cc/<slug>/<hash>`), not a direct image link - the
/// actual `i.postimg.cc/...` link (and the server's own possibly-
/// sanitized version of the filename) only appears in that page's own
/// HTML, so it has to be fetched and scraped same as the reference
/// upload flow this was ported from does.
///
/// Deliberately `Transport::Direct` (plain clearnet), not the account's own
/// Tor transport: postimg.cc is a general image host, not part of Kiwi
/// Farms - nothing it sees is tied to the Sneedchat account or identity at
/// all (it's a plain anonymous upload, no auth). Following qu.ax's own
/// precedent of not routing this over Tor - free upload hosts commonly
/// block Tor exit traffic outright.
///
/// A fresh, throwaway HttpClient rather than the account's own logged-in
/// session: postimg.cc needs no authentication at all for an anonymous
/// upload, so there's nothing to gain from reusing the Sneedchat session's
/// cookies, and building a new client here means this doesn't need any new
/// account-level state threaded through Runtime just for this one feature.
/// Which host to put it on. `None` keeps postimg.cc, which is what this
/// always did and what the site's own regulars use.
///
/// Honouring the caller's choice is the whole point: the uploads setting
/// promised it covered "anything Sneedchat's own uploader refuses", and it
/// reached IRC only - a Sneedchat send called straight into the postimg path
/// below, whose signature had nowhere to put a host. Attaching a video there
/// failed with "postimg.cc only accepts images" and no way to pick something
/// that would take it.
pub async fn send_attachment(
    state: &AppState,
    account_id: &str,
    buffer_name: &str,
    caption: &str,
    file_path: &str,
    host: Option<crate::upload::Host>,
) -> Result<()> {
    // Only used to confirm the account is real before spending any time on
    // the upload - the transport below is deliberately unrelated to it.
    state.accounts.get_sneedchat(account_id).ok_or_else(|| anyhow!("no such account"))?;

    match host {
        None | Some(crate::upload::Host::Postimg) => {}
        Some(host) => return send_via_upload_host(state, account_id, buffer_name, caption, file_path, host).await,
    }

    let links = upload_to_postimg(file_path).await?;

    let wrapped = format!("[url={}][img]{}[/img][/url]", links.page, links.direct);
    let text = if caption.trim().is_empty() { wrapped } else { format!("{caption}\n{wrapped}") };
    // The caption already carries any mention the caller wanted; an image
    // post is not separately a reply.
    send_message(state, account_id, buffer_name, &text, None)
}

/// Posting a file that went to one of the shared upload hosts.
///
/// The link still has to be wrapped in `[img]` for a picture to render
/// rather than sit there as a URL - that is the site's own markup, and what
/// find_attachment_url already unwraps on the way back in. Anything that is
/// not a picture goes as a bare link, because `[img]` around a video would
/// render as a broken image instead of something clickable.
///
/// No `[url=]` wrapper here, unlike the postimg path: these hosts serve the
/// file itself rather than a page about it, so there is nothing to click
/// through to that the image is not already showing.
pub(super) async fn send_via_upload_host(
    state: &AppState,
    account_id: &str,
    buffer_name: &str,
    caption: &str,
    file_path: &str,
    host: crate::upload::Host,
) -> Result<()> {
    let link = crate::upload::upload(host, file_path, None).await?;
    let file_name = std::path::Path::new(file_path).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let posted = posted_markup(file_name, &link);
    let text = if caption.trim().is_empty() { posted } else { format!("{caption}\n{posted}") };
    send_message(state, account_id, buffer_name, &text, None)
}

/// How a finished upload is written into a message.
pub(super) fn posted_markup(file_name: &str, link: &str) -> String {
    if guess_postimg_content_type(file_name).is_some() {
        format!("[img]{link}[/img]")
    } else {
        link.to_string()
    }
}

/// `editMessage`'s SneedChat branch (see rpc/methods.rs). The server has no
/// direct "edit accepted" reply - the edited message just comes back
/// through the normal live stream with a bumped message_edit_date, which
/// handle_frame already detects and applies via Runtime::update_message.
pub fn edit_message(state: &AppState, account_id: &str, buffer_name: &str, msg_id: &str, body: &str) -> Result<()> {
    let sender = room_sender_for_buffer(state, account_id, buffer_name)?;
    sender.send(protocol::prepare_edit(msg_id, body)).map_err(|_| anyhow!("chat socket closed"))?;
    Ok(())
}

/// `deleteMessage`'s SneedChat branch - same "no direct reply" story as
/// edit_message; the deletion gets applied locally once it's echoed back
/// (either as a top-level `delete` batch or a `deleted`/`is_deleted` flag).
pub fn delete_message(state: &AppState, account_id: &str, buffer_name: &str, msg_id: &str) -> Result<()> {
    let sender = room_sender_for_buffer(state, account_id, buffer_name)?;
    sender.send(protocol::prepare_delete(msg_id)).map_err(|_| anyhow!("chat socket closed"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture has to be wrapped for the site to render it; a video must
    /// not be, because [img] around an .mp4 draws a broken image instead of
    /// something you can click.
    #[test]
    fn only_pictures_are_wrapped_for_display() {
        assert_eq!(posted_markup("cat.png", "https://files.catbox.moe/a.png"), "[img]https://files.catbox.moe/a.png[/img]");
        assert_eq!(posted_markup("cat.JPEG", "https://x/a.jpeg"), "[img]https://x/a.jpeg[/img]");
        assert_eq!(posted_markup("clip.mp4", "https://files.catbox.moe/b.mp4"), "https://files.catbox.moe/b.mp4");
        assert_eq!(posted_markup("notes.pdf", "https://x/c.pdf"), "https://x/c.pdf");
        // No extension at all is not a picture.
        assert_eq!(posted_markup("README", "https://x/d"), "https://x/d");
    }

    #[test]
    fn a_reply_opens_with_the_mention_sneedchat_understands() {
        // Sneedchat has no reply field; answering somebody by name is what
        // the site's own client does and what its notifications look for.
        assert_eq!(super::as_reply("sure", Some("Alexcellence")), "@Alexcellence, sure");
        // Leading space would otherwise land between the comma and the text.
        assert_eq!(super::as_reply("   sure", Some("Bob")), "@Bob, sure");
    }

    #[test]
    fn replying_twice_does_not_stack_mentions() {
        // Somebody who types the mention themselves, or replies again to the
        // same person, should not end up with "@Bob, @Bob, ...".
        assert_eq!(super::as_reply("@Bob, already there", Some("Bob")), "@Bob, already there");
    }

    #[test]
    fn a_message_that_is_not_a_reply_is_untouched() {
        assert_eq!(super::as_reply("plain", None), "plain");
        // A reply to somebody whose name is unknown - dropped from scrollback -
        // still sends rather than being lost to a missing mention.
        assert_eq!(super::as_reply("plain", Some("")), "plain");
    }
}
