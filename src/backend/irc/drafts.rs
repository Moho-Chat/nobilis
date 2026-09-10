//! The IRCv3 extensions that are still drafts, and that clients ship anyway.
//!
//! Four of the twelve drafts are implemented in WeeChat, Halloy and bIRC
//! today, which is the line this backend draws: a draft nobody has built is a
//! specification that can still change underneath an implementation, and a
//! draft three clients interoperate on is a de facto protocol.
//!
//! - `multiline` - one message that is longer than a line
//! - `message-redaction` - taking a message back, which IRC never had
//! - `read-marker` - where you had read up to, kept by the server
//! - `channel-rename` - a channel changing its name without becoming a new one
//!
//! They are together in one file because they are the same shape of work -
//! a capability, something to send, and a line to read - and because they
//! share the one property worth stating in a single place: every one of them
//! degrades to exactly the old behaviour on a network that does not offer it.

use super::*;

/// How long a single IRC line may be, in bytes, including the trailing CRLF.
///
/// The 512 of RFC 1459, minus room for the parts the server prepends that
/// this client cannot see: our own `nick!user@host` and the command and
/// target in front of the text. 400 is what other clients settle on for the
/// same reason - it is a conservative guess at a number the server knows and
/// we do not.
pub(super) const SAFE_LINE: usize = 400;

/// Splits a message into pieces that will each fit on the wire.
///
/// Split on character boundaries, never inside one: a UTF-8 sequence cut in
/// half arrives as replacement characters at both ends of the join, which is
/// worse than the truncation this exists to avoid.
///
/// Prefers to break at a space near the limit, because a line broken
/// mid-word reads as a fault even when it is reassembled correctly at the
/// other end - and on a client without the capability, mid-word is exactly
/// what everybody sees.
pub(super) fn split_for_wire(body: &str, limit: usize) -> Vec<String> {
    if body.len() <= limit {
        return vec![body.to_string()];
    }
    let mut pieces = Vec::new();
    let mut rest = body;
    while rest.len() > limit {
        // The last byte index at or below the limit that starts a character.
        let mut cut = limit;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        // Back up to a space if there is one reasonably close, so words stay
        // whole. "Reasonably" is a quarter of the line: further back than
        // that and the pieces get lopsided enough to look like a bug of
        // their own.
        if let Some(space) = rest[..cut].rfind(' ') {
            if space > cut.saturating_sub(limit / 4) {
                // After the space, not at it. The pieces are rejoined with
                // nothing between them, so a separator dropped here is a
                // separator gone: a 659-byte message came back as 658 with
                // two words run together at the seam.
                cut = space + 1;
            }
        }
        if cut == 0 {
            // One character longer than the limit, which cannot be split any
            // further. Send it whole and let the server truncate; there is
            // no better answer.
            break;
        }
        pieces.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        pieces.push(rest.to_string());
    }
    pieces
}

/// Sends a message as a multiline batch.
///
/// The batch is opened, each piece goes out as an ordinary PRIVMSG carrying
/// the batch tag, and the batch is closed. A client that understands the
/// batch sees one message; one that does not sees the pieces, which is the
/// same thing every client sees today and therefore no worse.
///
/// The pieces carry `draft/multiline-concat` so a receiver joins them without
/// inserting the newline that would otherwise separate them - this splits a
/// long paragraph, not a poem.
pub(super) fn send_multiline(sender: &Sender, target: &str, pieces: &[String]) -> Result<()> {
    // A reference this client picked, unique for as long as the batch is
    // open. The clock is enough: batches do not overlap here, because a send
    // completes before the next one starts.
    let reference = format!(
        "ml{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() % 100_000
    );
    sender.send(Command::BATCH(
        format!("+{reference}"),
        Some(irc::proto::BatchSubCommand::CUSTOM("draft/multiline".to_string())),
        Some(vec![target.to_string()]),
    ))?;
    for piece in pieces {
        let mut line = Message::new(None, "PRIVMSG", vec![target, piece])?;
        line.tags = Some(vec![
            irc::proto::message::Tag("batch".to_string(), Some(reference.clone())),
            irc::proto::message::Tag("draft/multiline-concat".to_string(), None),
        ]);
        sender.send(line)?;
    }
    sender.send(Command::BATCH(format!("-{reference}"), None, None))?;
    Ok(())
}

/// A multiline message being assembled as its pieces arrive.
///
/// The pieces are ordinary PRIVMSGs carrying a `batch` tag, so without this
/// they are ordinary messages: a paragraph somebody sent as one thing arrives
/// as four lines, and our own echo of it arrives the same way - which is how
/// this was found, a 659-byte send coming back as its first 395 bytes.
pub(super) struct Assembling {
    pub target: String,
    /// Each piece, and whether it joins the previous one without a newline.
    pub pieces: Vec<(String, bool)>,
}

impl Assembling {
    /// The message the pieces make.
    ///
    /// `draft/multiline-concat` on a piece means it continues the one before
    /// rather than starting a line of its own - the difference between a long
    /// paragraph that had to be split and a message somebody wrote with line
    /// breaks in it.
    pub(super) fn finish(&self) -> String {
        let mut body = String::new();
        for (index, (piece, concat)) in self.pieces.iter().enumerate() {
            if index > 0 && !concat {
                body.push('\n');
            }
            body.push_str(piece);
        }
        body
    }
}

/// Batches being assembled, keyed by account and by the reference the server
/// gave them.
///
/// Per process rather than per connection, for the same reason `pending_sends`
/// is: the dispatch that sees the pieces has no place to keep state of its own,
/// and a reference is unique for as long as the batch is open.
pub(super) fn open_batches() -> &'static std::sync::Mutex<HashMap<(String, String), Assembling>> {
    static OPEN: std::sync::OnceLock<std::sync::Mutex<HashMap<(String, String), Assembling>>> = std::sync::OnceLock::new();
    OPEN.get_or_init(Default::default)
}

/// Reads a BATCH line: which reference, whether it is opening or closing, and
/// what kind it is.
///
/// `BATCH +<ref> <type> [params]` opens one and `BATCH -<ref>` closes it. Only
/// multiline batches are collected - a chathistory batch is a container for
/// messages that are each their own message, and folding those into one would
/// turn a replayed conversation into a wall of text.
pub(super) fn read_batch_tag(tag: &str) -> Option<(bool, String)> {
    match tag.split_at_checked(1)? {
        ("+", rest) => Some((true, rest.to_string())),
        ("-", rest) => Some((false, rest.to_string())),
        _ => None,
    }
}

/// Asks the server to take a message back.
///
/// `REDACT <target> <msgid> [:reason]`. Whether it is allowed is the server's
/// decision - your own message, or somebody else's if you have the rank - and
/// the answer comes back as a standard reply, which is already handled.
pub fn redact(sender: &Sender, target: &str, msg_id: &str, reason: Option<&str>) -> Result<()> {
    let mut args = vec![target.to_string(), msg_id.to_string()];
    if let Some(reason) = reason.filter(|r| !r.is_empty()) {
        args.push(reason.to_string());
    }
    sender.send(Command::Raw("REDACT".to_string(), args))?;
    Ok(())
}

/// Tells the server where this conversation has been read up to.
///
/// `MARKREAD <target> timestamp=<iso8601>`. The point is other clients: the
/// same account open on a phone starts where you left off here, which every
/// other protocol in this daemon has done for a while and IRC could not.
pub fn mark_read(sender: &Sender, target: &str, at: i64) -> Result<()> {
    let stamp = chrono::DateTime::from_timestamp(at, 0)
        .unwrap_or_else(chrono::Utc::now)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    sender.send(Command::Raw("MARKREAD".to_string(), vec![target.to_string(), format!("timestamp={stamp}")]))?;
    Ok(())
}

/// Reads the timestamp out of a MARKREAD line.
///
/// `MARKREAD <target> timestamp=<iso8601>` or `MARKREAD <target> *`, the
/// second meaning nothing has been read. The star is not an error and not a
/// time: it is answered with None and left alone, because "never read" is not
/// a position to scroll to.
pub(super) fn read_marker(args: &[String]) -> Option<(String, i64)> {
    let target = args.first()?.clone();
    let value = args.get(1)?;
    let stamp = value.strip_prefix("timestamp=")?;
    let at = chrono::DateTime::parse_from_rfc3339(stamp).ok()?.timestamp();
    Some((target, at))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limit is in bytes and the text is in characters, which is the
    /// whole difficulty: a cut inside a multi-byte character produces
    /// replacement characters at both ends of the join.
    #[test]
    fn a_split_never_cuts_a_character_in_half() {
        // Every one of these is three bytes and one character.
        let body = "日".repeat(300);
        let pieces = split_for_wire(&body, 100);
        assert!(pieces.len() > 1);
        for piece in &pieces {
            assert!(piece.len() <= 100, "piece was {} bytes", piece.len());
            // If a character had been cut the string would not round-trip.
            assert!(piece.chars().all(|c| c == '日'));
        }
        assert_eq!(pieces.concat().chars().count(), 300);
    }

    /// A line broken mid-word reads as a fault, and on a client without the
    /// capability mid-word is what everybody sees - so a space near the limit
    /// wins over the limit itself.
    #[test]
    fn a_split_prefers_to_break_between_words() {
        let body = format!("{}tail", "word ".repeat(30));
        let pieces = split_for_wire(&body, 60);
        assert!(pieces.len() > 1);
        for piece in &pieces {
            // Whole words, with the separator kept on the end of the piece
            // it was cut from rather than thrown away.
            assert!(piece.trim_end().split(' ').all(|w| w == "word" || w == "tail"), "got {piece:?}");
        }
    }

    /// The invariant that matters more than any of the preferences above: the
    /// pieces put back together are the message. Found the hard way - the
    /// split used to break *at* a space and trim both sides, so a 659-byte
    /// message arrived as 658 with two words run together at the seam.
    #[test]
    fn the_pieces_are_exactly_the_message() {
        for body in [
            "word ".repeat(200),
            "日".repeat(400),
            format!("{}{}", "x".repeat(500), " and some words after it"),
            "nospacesatallinthiswholeverylongmessage".repeat(20),
        ] {
            let pieces = split_for_wire(&body, 100);
            assert_eq!(pieces.concat(), body, "{} pieces did not rejoin", pieces.len());
        }
    }

    /// Short enough to fit is one piece and no batch at all - the common
    /// case, and the one that must not grow a wrapper it does not need.
    #[test]
    fn something_that_fits_is_left_alone() {
        assert_eq!(split_for_wire("hello", 400), vec!["hello".to_string()]);
        let exact = "x".repeat(400);
        assert_eq!(split_for_wire(&exact, 400), vec![exact]);
    }

    /// The difference between a paragraph that had to be split and a message
    /// somebody wrote with line breaks in it. Getting this wrong turns one
    /// into the other, in both directions.
    #[test]
    fn concat_decides_whether_a_piece_starts_a_line() {
        let split = Assembling {
            target: "#chat".into(),
            pieces: vec![("a long ".into(), false), ("paragraph".into(), true)],
        };
        assert_eq!(split.finish(), "a long paragraph");

        let written = Assembling {
            target: "#chat".into(),
            pieces: vec![("first".into(), false), ("second".into(), false)],
        };
        assert_eq!(written.finish(), "first\nsecond");

        // The first piece never gets a newline in front of it, whatever it
        // says about itself.
        let one = Assembling { target: "#chat".into(), pieces: vec![("only".into(), true)] };
        assert_eq!(one.finish(), "only");
    }

    /// Which batch, and which way it goes.
    #[test]
    fn a_batch_tag_says_which_way_it_goes() {
        assert_eq!(read_batch_tag("+ml42"), Some((true, "ml42".to_string())));
        assert_eq!(read_batch_tag("-ml42"), Some((false, "ml42".to_string())));

        // Anything without the sign is not a batch reference.
        assert!(read_batch_tag("ml42").is_none());
        assert!(read_batch_tag("").is_none());
    }

    /// A time, and the two things that are not one.
    #[test]
    fn a_read_marker_is_a_time_or_it_is_nothing() {
        let args = ["#chat".to_string(), "timestamp=2026-09-10T12:00:00.000Z".to_string()];
        let (target, at) = read_marker(&args).expect("a marker");
        assert_eq!(target, "#chat");
        assert!(at > 1_700_000_000);

        // "Never read" is not a position to scroll to.
        assert!(read_marker(&["#chat".to_string(), "*".to_string()]).is_none());
        // And neither is a malformed one.
        assert!(read_marker(&["#chat".to_string(), "timestamp=yesterday".to_string()]).is_none());
        assert!(read_marker(&["#chat".to_string()]).is_none());
    }
}
