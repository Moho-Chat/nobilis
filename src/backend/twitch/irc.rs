//! Twitch's IRC, as a line: tags, a prefix, a command and its parameters.
//!
//! Twitch speaks IRC with IRCv3 message tags - `@key=value;key=value` ahead of
//! the line - and nearly everything worth knowing about a message is in them:
//! who it was, what colour, what badges, which parts are emotes, what it was
//! a reply to. This reads one line into that shape and does nothing else; what
//! the lines mean is `chat.rs`'s. Written against lines captured from live
//! channels (the tests keep some), and tolerant of what it does not know: a tag
//! it has no use for is carried along, and a line it cannot read is nothing
//! rather than an error, because one odd line must not end a connection.

use std::collections::HashMap;

/// One line from Twitch.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Line {
    pub tags: HashMap<String, String>,
    /// Who sent it: `nick!user@host`, or a server name.
    pub prefix: Option<String>,
    pub command: String,
    /// Everything after the command; the trailing parameter (the one that may
    /// hold spaces, after `:`) is last.
    pub params: Vec<String>,
}

impl Line {
    /// A tag's value, or an empty string where it is absent or empty - which
    /// Twitch writes the same way (`emotes=` and no `emotes` tag are one thing).
    pub fn tag(&self, key: &str) -> &str {
        self.tags.get(key).map(String::as_str).unwrap_or("")
    }

    /// The nick the line is from, out of `nick!user@host`.
    pub fn nick(&self) -> Option<&str> {
        let prefix = self.prefix.as_deref()?;
        Some(prefix.split('!').next().unwrap_or(prefix)).filter(|n| !n.is_empty())
    }

    /// The channel it is about, without the `#`, lower-cased as Twitch's are.
    pub fn channel(&self) -> Option<String> {
        self.params.first().and_then(|p| p.strip_prefix('#')).map(|c| c.to_lowercase())
    }

    /// The last parameter: what was said, for a message.
    pub fn text(&self) -> &str {
        if self.params.len() > 1 { self.params.last().map(String::as_str).unwrap_or("") } else { "" }
    }
}

/// Reads a tag's value as the IRCv3 spec escapes it.
///
/// `\:` is `;`, `\s` a space, `\\` a backslash, `\r` and `\n` the controls; a
/// backslash before anything else is dropped and the character kept, and a
/// lone one at the end is dropped. A reply quotes the message it answers in a
/// tag, so this is what turns `hello\sthere` back into "hello there".
pub fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Reads one line, without its line ending. `None` for a line with no command.
pub fn parse(raw: &str) -> Option<Line> {
    let mut rest = raw.trim_end_matches(['\r', '\n']);
    let mut line = Line::default();

    if let Some(after) = rest.strip_prefix('@') {
        let (tags, tail) = after.split_once(' ')?;
        for tag in tags.split(';').filter(|t| !t.is_empty()) {
            let (key, value) = tag.split_once('=').unwrap_or((tag, ""));
            line.tags.insert(key.to_string(), unescape(value));
        }
        rest = tail.trim_start();
    }
    if let Some(after) = rest.strip_prefix(':') {
        let (prefix, tail) = after.split_once(' ').unwrap_or((after, ""));
        line.prefix = Some(prefix.to_string());
        rest = tail.trim_start();
    }
    let (command, mut tail) = rest.split_once(' ').unwrap_or((rest, ""));
    if command.is_empty() {
        return None;
    }
    line.command = command.to_string();

    while !tail.is_empty() {
        if let Some(trailing) = tail.strip_prefix(':') {
            line.params.push(trailing.to_string());
            break;
        }
        let (param, next) = tail.split_once(' ').unwrap_or((tail, ""));
        line.params.push(param.to_string());
        tail = next.trim_start();
    }
    Some(line)
}

/// What a client sends to get in: the capabilities that put the tags on the
/// lines, then the password and the nick. In that order - Twitch wants the
/// capability request first.
pub fn greeting(login: &str, token: Option<&str>) -> Vec<String> {
    // Any password does for an anonymous reader, and a `justinfan` nick marks it.
    let (nick, pass) = match token {
        Some(token) => (login.to_string(), format!("oauth:{token}")),
        None => (format!("justinfan{}", 10_000 + (std::process::id() % 80_000)), "SCHMOOPIIE".to_string()),
    };
    vec!["CAP REQ :twitch.tv/tags twitch.tv/commands".to_string(), format!("PASS {pass}"), format!("NICK {nick}")]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line captured from xqc's chat, tags and all.
    const MESSAGE: &str = "@badge-info=subscriber/37;badges=subscriber/36,premium/1;client-nonce=0c55da2c71da4ff5af5bb3a8ee0ef46d;color=#FF69B4;display-name=Zakktheking13;emotes=301445381:0-7;first-msg=0;flags=;id=6a349c82-69f2-4ed6-ad45-30b2d3e57102;mod=0;returning-chatter=0;room-id=71092938;subscriber=1;tmi-sent-ts=1791949381404;turbo=0;user-id=494821572;user-type= :zakktheking13!zakktheking13@zakktheking13.tmi.twitch.tv PRIVMSG #xqc :xqcPeepo  eyebrow busta";

    #[test]
    fn a_message_is_read_with_its_tags_prefix_channel_and_text() {
        let line = parse(MESSAGE).unwrap();
        assert_eq!(line.command, "PRIVMSG");
        assert_eq!(line.nick(), Some("zakktheking13"));
        assert_eq!(line.channel().as_deref(), Some("xqc"));
        // The two spaces inside the text are the sender's, and are kept.
        assert_eq!(line.text(), "xqcPeepo  eyebrow busta");
        assert_eq!(line.tag("display-name"), "Zakktheking13");
        assert_eq!(line.tag("emotes"), "301445381:0-7");
        assert_eq!(line.tag("color"), "#FF69B4");
        assert_eq!(line.tag("tmi-sent-ts"), "1791949381404");
        // An empty tag and an absent one are the same thing.
        assert_eq!(line.tag("flags"), "");
        assert_eq!(line.tag("no-such-tag"), "");
    }

    #[test]
    fn tag_values_are_unescaped() {
        assert_eq!(unescape(r"what\sdoes\sno\seyebrows\smean\sbru"), "what does no eyebrows mean bru");
        assert_eq!(unescape(r"a\:b\\c\nd\re"), "a;b\\c\nd\re");
        assert_eq!(unescape(r"trailing\"), "trailing");
        assert_eq!(unescape(r"odd\qescape"), "oddqescape");
    }

    #[test]
    fn a_reply_names_what_it_answers_in_its_tags() {
        let line = parse("@reply-parent-display-name=deathsashes112;reply-parent-msg-body=what\\sdoes\\sno\\seyebrows\\smean\\sbru;reply-parent-msg-id=bc4a0663-4e5d-49c4-8d5b-0c78ec73d901;reply-parent-user-login=deathsashes112;reply-thread-parent-msg-id=bc4a0663-4e5d-49c4-8d5b-0c78ec73d901;id=b4d3 :a_t_m_0_s!a_t_m_0_s@a_t_m_0_s.tmi.twitch.tv PRIVMSG #xqc :@deathsashes112 it means nothing").unwrap();
        assert_eq!(line.tag("reply-parent-msg-body"), "what does no eyebrows mean bru");
        assert_eq!(line.tag("reply-thread-parent-msg-id"), "bc4a0663-4e5d-49c4-8d5b-0c78ec73d901");
    }

    #[test]
    fn the_lines_with_no_tags_are_read_too() {
        let line = parse(":tmi.twitch.tv 001 justinfan26983 :Welcome, GLHF!").unwrap();
        assert_eq!((line.command.as_str(), line.prefix.as_deref(), line.params.clone()), ("001", Some("tmi.twitch.tv"), vec!["justinfan26983".to_string(), "Welcome, GLHF!".to_string()]));
        let ping = parse("PING :tmi.twitch.tv\r\n").unwrap();
        assert_eq!((ping.command.as_str(), ping.text()), ("PING", ""));
        assert_eq!(ping.params, vec!["tmi.twitch.tv".to_string()]);
        let cap = parse(":tmi.twitch.tv CAP * ACK :twitch.tv/tags twitch.tv/commands").unwrap();
        assert_eq!((cap.command.as_str(), cap.params.len()), ("CAP", 3));
    }

    #[test]
    fn a_roomstate_is_just_tags() {
        let line = parse("@emote-only=0;followers-only=10;r9k=0;room-id=71092938;slow=0;subs-only=0 :tmi.twitch.tv ROOMSTATE #xqc").unwrap();
        assert_eq!((line.command.as_str(), line.channel().as_deref(), line.tag("followers-only"), line.tag("room-id")), ("ROOMSTATE", Some("xqc"), "10", "71092938"));
    }

    #[test]
    fn an_action_keeps_its_markers_for_the_caller_to_read() {
        let line = parse(":someone!someone@someone.tmi.twitch.tv PRIVMSG #xqc :\u{1}ACTION waves\u{1}").unwrap();
        assert_eq!(line.text(), "\u{1}ACTION waves\u{1}");
    }

    #[test]
    fn what_is_not_a_line_is_nothing() {
        assert!(parse("").is_none());
        assert!(parse("@tags-and-nothing-else").is_none());
        assert!(parse("\r\n").is_none());
    }

    #[test]
    fn a_greeting_asks_for_the_tags_first_and_is_anonymous_without_a_token() {
        let signed = greeting("someone", Some("abc"));
        assert_eq!(signed, vec!["CAP REQ :twitch.tv/tags twitch.tv/commands", "PASS oauth:abc", "NICK someone"]);
        let anon = greeting("", None);
        assert!(anon[2].starts_with("NICK justinfan"), "{anon:?}");
        assert!(anon[1].starts_with("PASS "));
    }
}
