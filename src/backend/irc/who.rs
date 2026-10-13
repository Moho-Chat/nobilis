//! Asking the server who is actually in a channel.
//!
//! Until this existed there was no `WHO` anywhere in the backend, which meant
//! a hostmask only ever arrived from an explicit WHOIS about one person. That
//! is the wrong shape for the two things that want one: ignoring somebody and
//! banning them both act on a mask, and an ignore keyed on a nick is undone
//! the moment they change it.
//!
//! Two shapes, because servers answer in two.
//!
//! **WHOX** is the ratified extension, and it is the one with no capability
//! of its own - a server advertises `WHOX` in RPL_ISUPPORT and nowhere else,
//! which is why this reads ISUPPORT rather than the CAP list. It lets the
//! client name the fields it wants and get exactly those back, including the
//! services account, which plain WHO has nowhere to put.
//!
//! **Plain WHO** is the fallback every server has had since RFC 1459. It
//! carries the hostmask but not the account, so on those networks the account
//! comes from `account-notify`, `extended-join` and `account-tag` instead.

use super::*;

/// The fields asked for, in the order WHOX answers them.
///
/// `t` is the token that marks the reply as ours, and the rest are what a
/// roster wants: channel, user, host, nick, flags, account. Deliberately not
/// `r` (realname) - it is the one field that is free text of arbitrary
/// length, and nothing here shows it.
const WHOX_FIELDS: &str = "%tcuhnfa";

/// Marks a WHOX reply as an answer to us rather than to something a person
/// typed. Any number would do; this one is stable so a reply arriving after a
/// reconnect is still recognisable.
pub(super) const WHOX_TOKEN: &str = "042";

/// Asks who is in a channel, in whichever dialect this server speaks.
pub(super) fn ask(state: &AppState, account_id: &str, channel: &str) {
    let Some(sender) = state.runtime.irc_sender(account_id) else { return };
    let line = if state.runtime.irc_has_isupport(account_id, "WHOX") {
        format!("WHO {channel} {WHOX_FIELDS},{WHOX_TOKEN}")
    } else {
        format!("WHO {channel}")
    };
    if let Err(e) = sender.send(Command::Raw(line.clone(), Vec::new())) {
        tracing::debug!("irc[{account_id}]: asking {line}: {e}");
    }
}

/// Asks for a channel's roster: who is in it, and who they are.
///
/// Both questions, because they are two commands and one answer is half of
/// what a member list needs - NAMES gives names and ranks, WHO gives
/// hostmasks, accounts and who is away.
///
/// Asked when a conversation is opened rather than when it is joined. An
/// account that autojoins twenty channels was fetching twenty member lists at
/// connect, every one about a room nobody had looked at; `no-implicit-names`
/// is the capability that stops the *server* volunteering the first half, and
/// this is the client half of the same decision.
///
/// Once per channel per connection. After that the roster keeps itself
/// current from the joins, parts and nick changes that arrive anyway.
pub fn ask_roster(state: &AppState, account_id: &str, channel: &str) {
    // Before the "asked" mark, not after. A conversation can be opened while
    // its account is still connecting - the buffer exists from scrollback and
    // the socket does not - and marking that as asked would mean the roster
    // was never fetched at all: the one chance to ask spent on a connection
    // that could not carry it.
    let Some(sender) = state.runtime.irc_sender(account_id) else { return };
    if !state.runtime.irc_roster_needed(&crate::model::buffer_id(account_id, channel)) {
        return;
    }
    if let Err(e) = sender.send(Command::Raw("NAMES".to_string(), vec![channel.to_string()])) {
        tracing::debug!("irc[{account_id}]: asking who is in {channel}: {e}");
    }
    ask(state, account_id, channel);
}

/// What one WHO reply says about one person.
pub(super) struct Seen {
    pub channel: String,
    pub nick: String,
    pub host: String,
    pub account: Option<String>,
    pub away: bool,
    pub bot: bool,
}

/// Reads a WHOX reply (`354`).
///
/// `<client> <token> <channel> <user> <host> <nick> <flags> <account>` - the
/// fields asked for in `WHOX_FIELDS`, in that order, which is fixed by the
/// spec rather than by the request. A reply carrying somebody else's token is
/// an answer to something a person typed and is left alone.
pub(super) fn read_whox(args: &[String]) -> Option<Seen> {
    if args.len() < 8 || args[1] != WHOX_TOKEN {
        return None;
    }
    Some(Seen {
        channel: args[2].clone(),
        nick: args[5].clone(),
        host: format!("{}@{}", args[3], args[4]),
        account: Some(args[7].clone()),
        away: args[6].starts_with('G'),
        bot: args[6].contains('B'),
    })
}

/// Reads a plain WHO reply (`352`).
///
/// `<client> <channel> <user> <host> <server> <nick> <flags> :<hops> <real>`.
/// No account: RFC WHO has nowhere to put one, which is the whole reason WHOX
/// exists.
pub(super) fn read_who(args: &[String]) -> Option<Seen> {
    if args.len() < 7 {
        return None;
    }
    Some(Seen {
        channel: args[1].clone(),
        nick: args[5].clone(),
        host: format!("{}@{}", args[2], args[3]),
        account: None,
        away: args[6].starts_with('G'),
        bot: args[6].contains('B'),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &[&str]) -> Vec<String> {
        line.iter().map(|s| s.to_string()).collect()
    }

    /// The shape that carries an account, which is the reason for asking in
    /// this dialect at all.
    #[test]
    fn a_whox_reply_carries_the_account() {
        let seen = read_whox(&args(&["me", WHOX_TOKEN, "#chat", "ada", "example.org", "Ada", "H", "adalovelace"]))
            .expect("a reply");
        assert_eq!(seen.channel, "#chat");
        assert_eq!(seen.nick, "Ada");
        assert_eq!(seen.host, "ada@example.org");
        assert_eq!(seen.account.as_deref(), Some("adalovelace"));
        assert!(!seen.away);
        assert!(!seen.bot);
    }

    /// Somebody else's WHOX. A person typing `/quote WHO ... %tcuhnfa,999`
    /// gets an answer this client must not fold into a roster, because the
    /// fields may be in a different order entirely.
    #[test]
    fn a_reply_to_somebody_elses_question_is_left_alone() {
        assert!(read_whox(&args(&["me", "999", "#chat", "ada", "example.org", "Ada", "H", "acct"])).is_none());
        // And a truncated one is not read as if the missing fields were empty.
        assert!(read_whox(&args(&["me", WHOX_TOKEN, "#chat"])).is_none());
    }

    /// Away and bot both live in the flags field, and both are read from it.
    /// `G` is gone, `H` is here - the letters are the wrong way round from
    /// what anybody would guess, which is exactly why it is tested.
    #[test]
    fn the_flags_say_away_and_bot() {
        let away = read_whox(&args(&["me", WHOX_TOKEN, "#chat", "u", "h", "Ada", "G", "acct"])).expect("a reply");
        assert!(away.away);

        let bot = read_whox(&args(&["me", WHOX_TOKEN, "#chat", "u", "h", "Hal", "HB", "acct"])).expect("a reply");
        assert!(bot.bot);
        assert!(!bot.away);

        // An operator's star and a channel prefix ride in the same field and
        // must not be read as either.
        let op = read_whox(&args(&["me", WHOX_TOKEN, "#chat", "u", "h", "Ada", "H*@", "acct"])).expect("a reply");
        assert!(!op.away);
        assert!(!op.bot);
    }

    /// The fallback every server has. Fewer fields, in a different order, and
    /// no account at all.
    #[test]
    fn a_plain_reply_carries_everything_but_the_account() {
        let seen = read_who(&args(&["me", "#chat", "ada", "example.org", "irc.example.org", "Ada", "H", "0 Ada Lovelace"]))
            .expect("a reply");
        assert_eq!(seen.channel, "#chat");
        assert_eq!(seen.nick, "Ada");
        assert_eq!(seen.host, "ada@example.org");
        assert!(seen.account.is_none());
        assert!(!seen.away);

        assert!(read_who(&args(&["me", "#chat"])).is_none());
    }
}
