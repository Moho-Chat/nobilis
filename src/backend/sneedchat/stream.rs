//! A listed room's featured stream, and watching it together.
//!
//! A moderator features one stream in a room - YouTube, Twitch, Kick or
//! Rumble - as the room's pin (`stream {chat_room_uuid, stream_url}`, a URL or
//! null to take it down). The room says which in its `joined`
//! (`chat_room_stream`, absent when there is none) and in a
//! `chat_room_stream_set` entry when it changes (the same three fields at the
//! top level, all null on a clear). Each member opts in to watch; while a
//! player is open the client tells the chat so (`stream_watch`), again after
//! every rejoin since the chat starts each subscription unwatched, and the
//! room's presence then marks them `watching`.
//!
//! The chat only ever names a provider and a source id; the address is
//! rebuilt from those two, whatever else the frame says, and anything that
//! does not fit the provider's grammar is no stream at all.

use serde_json::{json, Value};

/// A stream the chat features: where it is, and the address to watch it at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub provider: String,
    pub source: String,
}

/// What the site's own client accepts for each provider's source id.
fn valid(provider: &str, source: &str) -> bool {
    let all = |ok: fn(char) -> bool| source.chars().all(ok);
    let len = source.chars().count();
    match provider {
        "youtube" => len == 11 && all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "twitch" => (4..=25).contains(&len) && all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
        "kick" => (1..=32).contains(&len) && all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
        // A Rumble embed id: `v` and 2-12 more.
        "rumble" => source.starts_with('v') && (3..=13).contains(&len) && all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
        _ => false,
    }
}

impl Stream {
    /// The stream a `joined` (`chat_room_stream`) or a `chat_room_stream_set`
    /// entry names, or None - also for an unknown provider or a source that
    /// does not fit it.
    pub fn of(v: &Value) -> Option<Self> {
        let v = if v["t"] == "joined" { &v["chat_room_stream"] } else { v };
        let provider = v["chat_room_stream_provider"].as_str()?;
        let source = v["chat_room_stream_source"].as_str()?;
        valid(provider, source).then(|| Self { provider: provider.to_string(), source: source.to_string() })
    }

    /// Its page on the provider's own site.
    pub fn url(&self) -> String {
        match self.provider.as_str() {
            "youtube" => format!("https://www.youtube.com/watch?v={}", self.source),
            "twitch" => format!("https://www.twitch.tv/{}", self.source),
            "kick" => format!("https://kick.com/{}", self.source),
            _ => format!("https://rumble.com/embed/{}/", self.source),
        }
    }

    /// The provider's own player, for a frame in the window - or None where
    /// it cannot play there: Twitch refuses without the embedding page's web
    /// address, and the window has none.
    pub fn player(&self) -> Option<String> {
        match self.provider.as_str() {
            "youtube" => Some(format!("https://www.youtube-nocookie.com/embed/{}?autoplay=1&rel=0", self.source)),
            "kick" => Some(format!("https://player.kick.com/{}?autoplay=true", self.source)),
            "rumble" => Some(format!("https://rumble.com/embed/{}/?autoplay=2", self.source)),
            _ => None,
        }
    }

    /// How the provider is written for people.
    pub fn provider_name(&self) -> &'static str {
        match self.provider.as_str() {
            "youtube" => "YouTube",
            "twitch" => "Twitch",
            "kick" => "Kick",
            _ => "Rumble",
        }
    }
}

/// The stream as the window is told of it (`sneedchatStream`, `getSneedchatStream`).
pub fn stream_json(stream: Option<&Stream>, watching: bool) -> Value {
    match stream {
        Some(s) => json!({
            "provider": s.provider,
            "providerName": s.provider_name(),
            "source": s.source,
            "url": s.url(),
            "player": s.player(),
            "watching": watching,
        }),
        None => Value::Null,
    }
}

/// The node's cap on a submitted stream address, in bytes.
pub const STREAM_URL_MAX_BYTES: usize = 512;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_is_read_from_either_frame_and_checked() {
        let joined = json!({ "t": "joined", "chat_room_stream": { "chat_room_stream_provider": "kick", "chat_room_stream_source": "some_one", "chat_room_stream_url": "https://evil.example/" } });
        let s = Stream::of(&joined).unwrap();
        // The address is rebuilt, never taken from the frame.
        assert_eq!(s.url(), "https://kick.com/some_one");
        assert_eq!(s.player().as_deref(), Some("https://player.kick.com/some_one?autoplay=true"));
        let set = json!({ "kind": "chat_room_stream_set", "chat_room_stream_provider": "youtube", "chat_room_stream_source": "dQw4w9WgXcQ" });
        assert_eq!(Stream::of(&set).unwrap().url(), "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        // A clear, a room with none, and sources that do not fit.
        assert_eq!(Stream::of(&json!({ "kind": "chat_room_stream_set", "chat_room_stream_provider": null, "chat_room_stream_source": null })), None);
        assert_eq!(Stream::of(&json!({ "t": "joined" })), None);
        assert_eq!(Stream::of(&json!({ "chat_room_stream_provider": "youtube", "chat_room_stream_source": "short" })), None);
        assert_eq!(Stream::of(&json!({ "chat_room_stream_provider": "twitch", "chat_room_stream_source": "Upper" })), None);
        assert_eq!(Stream::of(&json!({ "chat_room_stream_provider": "vimeo", "chat_room_stream_source": "12345" })), None);
        assert!(Stream::of(&json!({ "chat_room_stream_provider": "rumble", "chat_room_stream_source": "v5abc12" })).is_some());
        // Twitch has no player that plays in a window without a web address.
        assert_eq!(Stream { provider: "twitch".into(), source: "abcd".into() }.player(), None);
    }

    #[test]
    fn the_window_is_told_the_stream_or_null() {
        assert_eq!(stream_json(None, true), Value::Null);
        let v = stream_json(Some(&Stream { provider: "twitch".into(), source: "abcd".into() }), true);
        assert_eq!(v["url"], "https://www.twitch.tv/abcd");
        assert_eq!(v["player"], Value::Null);
        assert_eq!(v["watching"], true);
    }
}
