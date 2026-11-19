//! Everything this client puts into a room.
//!
//! Over the account's one socket (chat.rs). A post waits for its own echo,
//! which carries the nonce it was sent with; an edit or a removal waits for
//! the chat's `command_ack`. Either way the call answers with what the chat
//! said, so a refused message is said to be refused rather than left grey.

use super::*;
use serde_json::json;

/// The message a reply answers: who wrote it, its id, and what it said.
#[derive(Clone, Debug)]
pub struct ReplyTo {
    pub from: String,
    pub id: String,
    pub body: String,
}

/// The body that goes out: the reply's quote, as the site's own client
/// writes one, then what was typed.
pub(super) fn compose(body: &str, reply: Option<&ReplyTo>) -> String {
    let text = body.trim();
    match reply {
        Some(r) => format!("{}{text}", protocol::reply_quote(&r.from, &r.id, &r.body)),
        None => text.to_string(),
    }
}

/// Sends a message to the room a buffer shows, optionally as a reply.
pub async fn send_message(_state: &AppState, account_id: &str, buffer_name: &str, body: &str, reply_to: Option<ReplyTo>) -> Result<()> {
    let text = compose(body, reply_to.as_ref());
    if text.is_empty() {
        return Ok(());
    }
    if text.len() > 15 * 1024 {
        bail!("that's longer than the chat takes");
    }
    let chat = super::chat::chat(account_id);
    let room = chat.room_of_buffer(buffer_name).ok_or_else(|| anyhow!("\"{buffer_name}\" isn't one of this account's rooms"))?;
    if !chat.joined(&room) {
        bail!("not in this room right now - it is still joining");
    }
    let nonce = protocol::post_nonce();
    chat.command(&nonce, json!({ "t": "send", "chat_room_uuid": room, "body": text, "client_nonce": nonce })).await
}

/// Whispers are side rooms on the new chat - not built yet.
pub fn send_whisper(_state: &AppState, _account_id: &str, _target: &str, _body: &str, _from_buffer: Option<&str>) -> Result<()> {
    bail!("whispers aren't supported on the new Sneedchat yet")
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
    progress: Option<&crate::upload::Progress>,
) -> Result<()> {
    // Only used to confirm the account is real before spending any time on
    // the upload - the transport below is deliberately unrelated to it.
    state.accounts.get_sneedchat(account_id).ok_or_else(|| anyhow!("no such account"))?;

    match host {
        None | Some(crate::upload::Host::Postimg) => {}
        Some(host) => {
            return send_via_upload_host(state, account_id, buffer_name, caption, file_path, host, progress).await
        }
    }

    let links = match upload_to_postimg_reporting(file_path, progress).await {
        Ok(links) => {
            if let Some(progress) = progress {
                progress.done(None);
            }
            links
        }
        Err(e) => {
            if let Some(progress) = progress {
                progress.done(Some(&e.to_string()));
            }
            return Err(e);
        }
    };

    let wrapped = format!("[url={}][img]{}[/img][/url]", links.page, links.direct);
    let text = if caption.trim().is_empty() { wrapped } else { format!("{caption}\n{wrapped}") };
    // The caption already carries any mention the caller wanted; an image
    // post is not separately a reply.
    send_message(state, account_id, buffer_name, &text, None).await
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
    progress: Option<&crate::upload::Progress>,
) -> Result<()> {
    // Refused here as well as left out of the menu, because the menu is only
    // one client's: a link from a host the forum does not embed is a picture
    // nobody on the other end sees.
    if !host.offered_to("sneedchat") {
        bail!("{} links don't show as pictures on Sneedchat - choose another image host for it", host.id());
    }
    let link = crate::upload::upload_reporting(host, file_path, None, progress).await?;
    let file_name = std::path::Path::new(file_path).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let posted = posted_markup(file_name, &link);
    let text = if caption.trim().is_empty() { posted } else { format!("{caption}\n{posted}") };
    send_message(state, account_id, buffer_name, &text, None).await
}

/// How a finished upload is written into a message.
pub(super) fn posted_markup(file_name: &str, link: &str) -> String {
    if crate::upload::is_web_picture(file_name) {
        format!("[img]{link}[/img]")
    } else {
        link.to_string()
    }
}

/// Changes what a message says (`revise`).
pub async fn edit_message(state: &AppState, account_id: &str, _buffer_name: &str, msg_id: &str, body: &str) -> Result<()> {
    let text = body.trim();
    if text.is_empty() {
        bail!("there's nothing to change it to");
    }
    // An edit keeps the reply it was: the quote stays at its head.
    let stored = state.store.get_message(&crate::model::buffer_id(account_id, _buffer_name), msg_id).ok().flatten();
    let reply = stored.and_then(|m| m.reply_to).map(|r| ReplyTo { from: r.from, id: r.id, body: r.body });
    let text = compose(text, reply.as_ref());
    operate(account_id, msg_id, "revise", Some(&text)).await
}

/// Takes a message down (`tombstone`).
pub async fn delete_message(_state: &AppState, account_id: &str, _buffer_name: &str, msg_id: &str) -> Result<()> {
    operate(account_id, msg_id, "tombstone", None).await
}

/// A change to a message: it names the state the message is in, which the
/// chat checks so that two changes cannot cross.
async fn operate(account_id: &str, msg_id: &str, action: &str, body: Option<&str>) -> Result<()> {
    let chat = super::chat::chat(account_id);
    let known = match chat.known(msg_id) {
        Some(k) => k.state_seq,
        None => message_state(&chat, msg_id).await?,
    };
    let op = protocol::operation_id();
    let mut frame = json!({
        "t": action,
        "chat_message_uuid": msg_id.to_lowercase(),
        "expected_message_state_seq": known,
        "reason": null,
        "notify_author": false,
        "chat_operation_uuid": op,
    });
    if let Some(b) = body {
        frame["body"] = json!(b);
    }
    chat.command(&op, frame).await
}

/// A message's current state, asked of the chat - for one this connection
/// has not seen.
async fn message_state(chat: &super::chat::Chat, msg_id: &str) -> Result<u64> {
    let session = chat.session.lock().unwrap().clone().ok_or_else(|| anyhow!("not connected to the chat right now"))?;
    let host = chat.host.lock().unwrap().clone();
    let v = super::chat::api_get(&session, &host, &format!("messages/{}", msg_id.to_lowercase())).await?;
    v["message_state_seq"]
        .as_u64()
        .or_else(|| v["message"]["message_state_seq"].as_u64())
        .ok_or_else(|| anyhow!("the chat didn't say what state that message is in"))
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
    fn a_reply_opens_with_the_quote_the_site_writes() {
        let reply = ReplyTo { from: "Ryo Ma".into(), id: "01A1".into(), body: "what's up".into() };
        assert_eq!(compose("not much", Some(&reply)), "[quote=\"Ryo Ma, chat_message: 01a1\"]what's up[/quote]\nnot much");
        assert_eq!(compose("  plain  ", None), "plain");
        // Nothing of their own to quote: sent without a quote rather than not at all.
        let empty = ReplyTo { from: "Ryo".into(), id: "x".into(), body: "".into() };
        assert_eq!(compose("hi", Some(&empty)), "hi");
    }
}
