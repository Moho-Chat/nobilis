//! A bot's inline keyboard: the buttons under one of its messages.
//!
//! Telegram sends them as rows of buttons with the message. Most are "callback" buttons, which tell the bot something
//! when pressed and are answered with a short notice (or nothing, the bot editing its message instead); the rest are
//! links. They are drawn with the same shape Discord's buttons use, and an edit to the message that changes or removes
//! them says so.

use super::send::{explain, peer_for};
use crate::model::{Buffer, Component};
use crate::state::AppState;
use grammers_client::tl;
use serde_json::json;
use tl::enums::InlineButtonType as T;

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

/// Only an address a window may open in a browser.
fn web(url: &str) -> Option<String> {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then(|| url.trim().to_string())
}

/// The buttons of an inline keyboard, in rows, as this client draws buttons. Nothing for any other kind of markup.
pub fn components_of(markup: &tl::enums::ReplyMarkup) -> Vec<Component> {
    let tl::enums::ReplyMarkup::ReplyInlineMarkup(inline) = markup else { return Vec::new() };
    let mut out = Vec::new();
    for (row_index, row) in inline.rows.iter().enumerate() {
        let tl::enums::KeyboardInlineButtonRow::Row(row) = row;
        for button in &row.buttons {
            let tl::enums::KeyboardInlineButton::Button(button) = button;
            let style = button.style.as_ref().map(|s| {
                let tl::enums::KeyboardButtonStyle::Style(s) = s;
                if s.bg_danger {
                    "danger"
                } else if s.bg_success {
                    "success"
                } else if s.bg_primary {
                    "primary"
                } else {
                    "secondary"
                }
            });
            let make = |custom_id: Option<String>, style: Option<&str>, url: Option<String>, disabled: bool| Component {
                kind: "button".to_string(),
                custom_id,
                label: Some(button.text.clone()).filter(|t| !t.is_empty()),
                style: style.map(str::to_string),
                url,
                disabled,
                emoji: None,
                options: Vec::new(),
                placeholder: None,
                row: row_index as i64,
            };
            match &button.r#type {
                T::Callback(c) => out.push(make(Some(hex(&c.data)), style.or(Some("secondary")), None, c.requires_password)),
                T::Url(u) => out.extend(web(&u.url).map(|url| make(None, Some("link"), Some(url), false))),
                T::UrlAuth(u) => out.extend(web(&u.url).map(|url| make(None, Some("link"), Some(url), false))),
                T::WebView(w) => out.extend(web(&w.url).map(|url| make(None, Some("link"), Some(url), false))),
                T::Disabled => out.push(make(None, Some("secondary"), None, true)),
                // Switching to inline mode, games, payments, profile links and copy buttons are not done here.
                _ => {}
            }
        }
    }
    out
}

/// Keeps the buttons on a message and says so. With none, an edit that took them away takes them off the row too.
pub fn note(state: &AppState, buffer_id: &str, msg_id: &str, raw: &tl::enums::Message, edited: bool) {
    let tl::enums::Message::Message(m) = raw else { return };
    let components = m.reply_markup.as_ref().map(components_of).unwrap_or_default();
    if components.is_empty() && !edited {
        return;
    }
    let kept = if components.is_empty() { state.store.clear_components(buffer_id, msg_id) } else { state.store.set_components(buffer_id, msg_id, &components) };
    match kept {
        Ok(()) => state.events.emit("messageComponents", json!({ "bufferId": buffer_id, "messageId": msg_id, "components": components })),
        Err(e) => tracing::debug!("telegram: keeping the buttons on {msg_id}: {e:#}"),
    }
}

/// What the bot said back to a press: a notice to show (as a pop-up alert when it asked for one), and an address to open.
pub async fn press(state: &AppState, buffer: &Buffer, msg_id: &str, custom_id: &str) -> Result<serde_json::Value, String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message has no buttons".to_string())?;
    let data = unhex(custom_id).ok_or_else(|| "that button is not one this client can press".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let answer = handle
        .client
        .invoke(&tl::functions::messages::GetBotCallbackAnswer { game: false, peer: peer.into(), msg_id: id, data: Some(data), password: None })
        .await
        .map_err(|e| match &e {
            grammers_client::InvocationError::Rpc(r) if r.name == "BOT_RESPONSE_TIMEOUT" => "The bot did not answer".to_string(),
            grammers_client::InvocationError::Rpc(r) if r.name == "DATA_INVALID" || r.name == "BUTTON_DATA_INVALID" => "That button is no longer valid".to_string(),
            _ => explain(&e),
        })?;
    let tl::enums::messages::BotCallbackAnswer::Answer(a) = answer;
    Ok(json!({ "message": a.message, "alert": a.alert, "url": a.url.as_deref().and_then(web) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(text: &str, kind: T) -> tl::enums::KeyboardInlineButton {
        tl::enums::KeyboardInlineButton::Button(tl::types::KeyboardInlineButton { style: None, text: text.into(), r#type: kind })
    }

    fn keyboard(rows: Vec<Vec<tl::enums::KeyboardInlineButton>>) -> tl::enums::ReplyMarkup {
        tl::enums::ReplyMarkup::ReplyInlineMarkup(tl::types::ReplyInlineMarkup {
            force_reply: false,
            rows: rows.into_iter().map(|buttons| tl::enums::KeyboardInlineButtonRow::Row(tl::types::KeyboardInlineButtonRow { buttons })).collect(),
        })
    }

    #[test]
    fn data_survives_the_trip_through_a_button() {
        assert_eq!(unhex(&hex(&[0, 1, 0xab, 0xff])), Some(vec![0, 1, 0xab, 0xff]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }

    #[test]
    fn rows_keep_their_places_and_links_are_links() {
        let markup = keyboard(vec![
            vec![button("Next", T::Callback(tl::types::InlineButtonTypeCallback { requires_password: false, data: b"next:2".to_vec() })), button("Close", T::Callback(tl::types::InlineButtonTypeCallback { requires_password: false, data: b"x".to_vec() }))],
            vec![button("Open site", T::Url(tl::types::InlineButtonTypeUrl { url: "https://example.com/a".into() }))],
        ]);
        let got = components_of(&markup);
        assert_eq!(got.iter().map(|c| (c.label.as_deref().unwrap(), c.row)).collect::<Vec<_>>(), vec![("Next", 0), ("Close", 0), ("Open site", 1)]);
        assert_eq!(got[0].custom_id.as_deref(), Some(hex(b"next:2").as_str()));
        assert_eq!(got[2].url.as_deref(), Some("https://example.com/a"));
        assert_eq!(got[2].style.as_deref(), Some("link"));
    }

    #[test]
    fn what_cannot_be_opened_or_pressed_is_left_out() {
        let markup = keyboard(vec![vec![
            button("App", T::Url(tl::types::InlineButtonTypeUrl { url: "tg://resolve?domain=x".into() })),
            button("Pay", T::Buy),
            button("Locked", T::Callback(tl::types::InlineButtonTypeCallback { requires_password: true, data: b"x".to_vec() })),
        ]]);
        let got = components_of(&markup);
        assert_eq!(got.len(), 1, "only the callback is kept, and it cannot be pressed");
        assert!(got[0].disabled);
        assert!(components_of(&tl::enums::ReplyMarkup::ReplyKeyboardHide(tl::types::ReplyKeyboardHide { selective: false })).is_empty());
    }
}
