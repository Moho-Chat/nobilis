//! The account's own presence: the status chosen in moho, published on the gateway (op 3, `gateway/commands.md`) with
//! the line beside it as the custom status. Fluxer keeps five of these per twenty seconds per connection and drops the
//! rest, so a change is sent once, not repeated.

use super::Handle;
use serde_json::{json, Value};

/// Fluxer's word for a status chosen in moho.
pub fn status_of(status: &str) -> &'static str {
    match status {
        "idle" | "away" => "idle",
        "dnd" | "busy" => "dnd",
        "invisible" | "offline" => "invisible",
        _ => "online",
    }
}

/// The presence command: the status, and the custom status when a line is given (an empty line clears it).
pub fn command(status: &str, text: Option<&str>) -> Value {
    let mut d = json!({ "status": status_of(status), "afk": false, "mobile": false });
    if let Some(text) = text {
        let text = text.trim();
        d["custom_status"] = if text.is_empty() { Value::Null } else { json!({ "text": text.chars().take(128).collect::<String>() }) };
    }
    json!({ "op": 3, "d": d })
}

pub async fn apply_status(handle: &Handle, status: &str, text: Option<&str>) -> bool {
    handle.gateway.send(command(status, text).to_string()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_is_said_in_fluxers_words_and_an_empty_line_clears_the_custom_status() {
        assert_eq!(command("dnd", None), json!({ "op": 3, "d": { "status": "dnd", "afk": false, "mobile": false } }));
        assert_eq!(command("invisible", Some(""))["d"]["custom_status"], Value::Null);
        assert_eq!(command("online", Some(" lunch "))["d"]["custom_status"]["text"], "lunch");
        assert_eq!(status_of("whatever"), "online");
    }
}
