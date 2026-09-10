//! Who is here, who has gone, and who is being watched for.
//!
//! IRC tells you about presence in several unrelated ways - MONITOR if the
//! server has it, ISON if it does not, netsplits as a flood of quits that are
//! not really quits - and this turns all of them into the same answer.

use super::*;

/// Starts a conversation with somebody.
///
/// IRC has no concept of opening one: a query is a client-side idea, and the
/// server only learns of it when a message is actually sent. So this creates
/// the buffer and asks once whether they are there, which is what a person
/// wants to know before typing.
pub fn open_query(state: &AppState, account_id: &str, nick: &str) -> Result<String> {
    if nick.trim().is_empty() || is_channel(nick) {
        bail!("{nick:?} is not a nickname");
    }
    let buffer = state.runtime.ensure_buffer(state, account_id, nick, "dm");
    if let Some(sender) = state.runtime.irc_sender(account_id) {
        let _ = sender.send(Command::Raw("ISON".to_string(), vec![nick.to_string()]));
    }
    Ok(buffer.id)
}

/// How often to ask the server who among our conversation partners is on.
///
/// Slow enough to be invisible traffic on any network, quick enough that the
/// warning shown before sending is rarely stale. ERR_NOSUCHNICK covers the
/// gap: it is the server's own answer at the moment of sending.
pub(super) const ISON_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Asks, repeatedly, which of the people we have conversations with are online.
pub(super) async fn poll_query_presence(state: AppState, account_id: String, sender: irc::client::Sender) {
    loop {
        tokio::time::sleep(ISON_INTERVAL).await;

        // Anything sent whose echo never came back, on a connection quiet
        // enough that no incoming message has swept it.
        sweep_pending_sends(&state);

        // Conversation partners, plus anybody on the watch list - one poll
        // answers both questions, and a name in both is asked about once.
        let watched = state.accounts.get_irc(&account_id).map(|c| notify_list(&c)).unwrap_or_default();
        let mut nicks: Vec<String> = state
            .runtime
            .list_buffers()
            .into_iter()
            .filter(|b| b.account_id == account_id && b.kind == "dm")
            .map(|b| b.name)
            .collect();
        for nick in &watched {
            if !nicks.iter().any(|n| n.eq_ignore_ascii_case(nick)) {
                nicks.push(nick.clone());
            }
        }
        // Only where the server has no MONITOR. Where it has, it is already
        // telling us, and polling on top of it would be asking a question
        // that has been answered.
        if !watched.is_empty() && !state.runtime.irc_monitors(&account_id) {
            settle_notify(&state, &account_id, &watched);
        }
        if nicks.is_empty() {
            continue;
        }
        // One request for everyone rather than one each: ISON takes a list,
        // and a server will answer a line of them in a single reply.
        for chunk in nicks.chunks(20) {
            if sender.send(Command::Raw("ISON".to_string(), chunk.to_vec())).is_err() {
                return;
            }
        }
    }
}

/// The people this account has asked to be told about.
///
/// Stored as typed, so the list reads back the way it was written; matched
/// case-insensitively, because IRC nicks are.
pub fn notify_list(config: &IrcAccountConfig) -> Vec<String> {
    config.notify.split(',').map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).collect()
}

/// Who on the watch list was last seen online, per account.
///
/// `None` for a nick nothing is known about yet, which is the whole point of
/// keeping it: an arrival is only worth announcing against a previous state.
/// A first sighting - on connect, or the moment somebody is added to the list
/// - records silently, so signing on does not print a paragraph about people
/// who were already there.
pub(super) fn notify_seen() -> &'static std::sync::Mutex<HashMap<String, HashMap<String, bool>>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<HashMap<String, HashMap<String, bool>>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
}

/// Folds one sighting in, and says whether it is news.
///
/// Pure, so the rule that matters - the first answer about somebody is never
/// an announcement - is tested rather than argued about.
pub(super) fn note_presence(seen: &mut HashMap<String, bool>, nick: &str, online: bool) -> bool {
    let key = nick.to_lowercase();
    let previous = seen.insert(key, online);
    previous.is_some_and(|was| was != online)
}

/// Says somebody on the watch list arrived or left, in the server buffer.
///
/// The server buffer because it is about the network rather than about any
/// conversation - the same place a WHOIS answer and a network notice go.
pub(super) fn announce_presence(state: &AppState, account_id: &str, nick: &str, online: bool) {
    let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
    let body = if online { format!("{nick} is online") } else { format!("{nick} is offline") };
    state.runtime.record_message(state, account_id, host, "server", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
}

/// Starts watching, the good way where the server has it.
///
/// MONITOR is the server keeping the list and telling us when it changes,
/// which is one line at sign-on and nothing at all afterwards until somebody
/// moves. Where it is missing the ISON poll below covers the same ground more
/// expensively, so this is tried without asking whether it will work: a server
/// that has never heard of MONITOR answers 421, which costs one line.
pub(super) fn start_monitor(sender: &Sender, nicks: &[String]) {
    if nicks.is_empty() {
        return;
    }
    // Chunked, because the list goes on one line and a line has a length.
    for chunk in nicks.chunks(20) {
        let raw = format!("MONITOR + {}", chunk.join(","));
        if let Ok(msg) = raw.parse::<Message>() {
            let _ = sender.send(msg);
        }
    }
}

/// Turns a poll's worth of ISON answers into arrivals and departures.
///
/// ISON says only who *is* on, so absence is the answer for everybody asked
/// about - which is why this runs a tick later than the asking: the replies to
/// the previous poll have all arrived by then, and what they did not mention
/// is offline.
pub(super) fn settle_notify(state: &AppState, account_id: &str, watched: &[String]) {
    let replied = {
        let mut all = ison_replies().lock().unwrap();
        all.remove(account_id).unwrap_or_default()
    };
    // Nothing came back at all: a connection that has not answered yet, which
    // is not the same as everybody being offline.
    if replied.is_empty() && !ison_asked().lock().unwrap().contains(account_id) {
        return;
    }
    // Decided under the lock, announced outside it: recording a message
    // reaches the store and the event bus, which is not somewhere to go while
    // holding a mutex this small.
    let news: Vec<(String, bool)> = {
        let mut all = notify_seen().lock().unwrap();
        let seen = all.entry(account_id.to_string()).or_default();
        watched
            .iter()
            .filter_map(|nick| {
                let online = replied.contains(&nick.to_lowercase());
                note_presence(seen, nick, online).then(|| (nick.clone(), online))
            })
            .collect()
    };
    for (nick, online) in news {
        announce_presence(state, account_id, &nick, online);
    }
}

/// Nicks named by ISON replies since the last poll settled.
pub(super) fn ison_replies() -> &'static std::sync::Mutex<HashMap<String, std::collections::HashSet<String>>> {
    static REPLIES: std::sync::OnceLock<std::sync::Mutex<HashMap<String, std::collections::HashSet<String>>>> =
        std::sync::OnceLock::new();
    REPLIES.get_or_init(Default::default)
}

/// Accounts that have been asked at least once, so "no reply yet" and
/// "everybody is offline" can be told apart.
pub(super) fn ison_asked() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static ASKED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    ASKED.get_or_init(Default::default)
}

/// MONITOR's own answer: these people are on, or these have gone.
pub(super) fn apply_monitor(state: &AppState, account_id: &str, targets: &str, online: bool) {
    state.runtime.set_irc_monitors(account_id, true);
    for target in targets.split(',') {
        // 730 sends full masks, 731 bare nicks. The nick is the part before
        // the "!" either way.
        let nick = target.trim().split('!').next().unwrap_or("").trim();
        if nick.is_empty() {
            continue;
        }
        let news = {
            let mut all = notify_seen().lock().unwrap();
            note_presence(all.entry(account_id.to_string()).or_default(), nick, online)
        };
        if news {
            announce_presence(state, account_id, nick, online);
        }
    }
}

/// Records which conversation partners the server just said are online.
///
/// ISON answers with only the nicks that *are* on, so anyone asked about and
/// missing from the reply is offline - which is the answer this exists to get.
pub(super) fn apply_ison(state: &AppState, account_id: &str, online: &str) {
    let online: Vec<String> = online.split_whitespace().map(|n| n.to_lowercase()).collect();
    for buffer in state.runtime.list_buffers() {
        if buffer.account_id != account_id || buffer.kind != "dm" {
            continue;
        }
        let here = online.contains(&buffer.name.to_lowercase());
        let members = json!([{
            "nick": buffer.name,
            "userId": buffer.name,
            "prefix": "",
            "away": !here,
            "status": if here { "online" } else { "offline" },
        }]);
        state.runtime.set_presence(&buffer.id, members.clone());
        state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": members }));
    }
}

/// The WHOIS answer, as a profile.
///
/// IRC's numerics are the oldest form of this in chat, and most of what they
/// carry maps straight across: the mask, the real name, the server, how long
/// they have been idle and when they connected. What has no equivalent -
/// there is no account age on IRC, because there are no accounts - is simply
/// absent rather than guessed at.
pub(super) fn irc_profile(
    state: &AppState,
    account_id: &str,
    nick: &str,
    whois: serde_json::Value,
    channels: &HashMap<String, HashMap<String, Who>>,
) -> serde_json::Value {
    let mut profile = crate::profile::pending("irc", account_id, nick);
    profile["pending"] = json!(false);
    crate::profile::set(&mut profile, "handle", whois.get("mask").cloned());
    crate::profile::set(&mut profile, "idleSeconds", whois.get("idleSeconds").cloned());
    // The signon time is when this connection started, which is the closest
    // thing IRC has to "joined" - and is what every client labels as such.
    crate::profile::set(&mut profile, "joinedTs", whois.get("signOnTs").cloned());
    crate::profile::set(&mut profile, "away", whois.get("away").cloned());
    crate::profile::set(&mut profile, "channels", whois.get("channels").cloned());

    if let Some(real) = whois.get("realName").and_then(serde_json::Value::as_str) {
        crate::profile::note(&mut profile, "Name", real);
    }
    if let Some(server) = whois.get("server").and_then(serde_json::Value::as_str) {
        crate::profile::note(&mut profile, "Server", server);
    }
    if let Some(oper) = whois.get("operator").and_then(serde_json::Value::as_str) {
        crate::profile::note(&mut profile, "Operator", oper);
    }

    // Their standing where we can see them. A rank is per channel on IRC, so
    // this reports every channel we share where they hold one - which is the
    // honest form of "are they a moderator" on a protocol with no global
    // answer to it.
    let mut roles: Vec<String> = Vec::new();
    let mut moderator = false;
    for (channel, members) in channels {
        if let Some(who) = members.get(nick) {
            if let Some(word) = who.rank.title() {
                roles.push(format!("{word} in {channel}"));
                moderator |= who.rank.can_moderate();
            }
        }
    }
    if !roles.is_empty() {
        profile["roles"] = json!(roles);
    }
    profile["isModerator"] = json!(moderator);
    profile["status"] = json!(if state.runtime.is_irc_away(account_id, nick) { "idle" } else { "online" });
    profile
}

/// Whether a quit message is a netsplit rather than something somebody typed.
///
/// A split's quit message is the two servers that stopped talking to each
/// other and nothing else - "irc.example.net hub.example.net". That shape is
/// the only signal there is, so it is matched conservatively: exactly two
/// words, both looking like hostnames, neither of them a sentence.
pub(super) fn is_netsplit(reason: &str) -> bool {
    let mut words = reason.split_whitespace();
    let (Some(a), Some(b), None) = (words.next(), words.next(), words.next()) else { return false };
    let hostish = |w: &str| w.contains('.') && !w.contains(':') && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    hostish(a) && hostish(b)
}

/// How long to keep gathering before reporting a split. Long enough that the
/// server has finished sending the quits, short enough that the line is still
/// about something that just happened.
pub(super) const SPLIT_REPORT_DELAY: Duration = Duration::from_secs(3);

/// Reports one netsplit, once, after the quits have stopped arriving.
pub(super) fn schedule_split_report(state: AppState, account_id: String, channel: String, reason: String) {
    tokio::spawn(async move {
        tokio::time::sleep(SPLIT_REPORT_DELAY).await;
        let gone = state.runtime.take_irc_split(&account_id, &channel, &reason);
        if gone.is_empty() {
            return;
        }
        let line = if gone.len() == 1 {
            format!("{} has quit ({reason})", gone[0])
        } else {
            format!("{} people split from the network ({reason})", gone.len())
        };
        state.runtime.record_message(&state, &account_id, &channel, "channel", "*", &line, false, "part", None, None, false, None, Vec::new(), Vec::new(), None);
    });
}

/// What is known about somebody in a channel.
///
/// A rank on its own was enough while a roster was a list of names with
/// prefixes on them. Four ratified capabilities each add a fact about the
/// *person* rather than about their standing in the room, so the entry became
/// a record: `userhost-in-names` and `whox` give a hostmask, `account-tag`
/// and `extended-join` give the services account, and `bot-mode` says this
/// one is a program.
///
/// Every field but the rank is optional and stays that way. A network that
/// grants none of those capabilities produces exactly the roster it always
/// did, and a field nobody filled in is absent rather than guessed at.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Who {
    pub rank: MemberRank,
    /// `user@host` as the network writes it. What an ignore or a ban wants,
    /// and the reason there is a WHO at all.
    pub host: Option<String>,
    /// The services account this person is identified to.
    pub account: Option<String>,
    /// The network says this one is a program.
    pub bot: bool,
}

impl Who {
    /// Somebody who has just arrived and is so far only a rank.
    pub(super) fn ranked(rank: MemberRank) -> Self {
        Self { rank, ..Self::default() }
    }

    /// Fills in what an answer carried, leaving alone what it did not.
    ///
    /// Answers arrive in any order and from several sources - NAMES on join,
    /// a WHO reply moments later, an `account` tag on the first thing
    /// somebody says - so this merges rather than replaces. A later answer
    /// that knows less must not erase what an earlier one knew.
    pub(super) fn learn(&mut self, host: Option<&str>, account: Option<&str>, bot: Option<bool>) {
        if let Some(host) = host.filter(|h| !h.is_empty()) {
            self.host = Some(host.to_string());
        }
        // "0" and "*" are how the wire says "signed in to nothing", which is
        // knowledge rather than absence: it clears an account rather than
        // leaving a stale one in place.
        match account {
            Some("0") | Some("*") => self.account = None,
            Some(account) if !account.is_empty() => self.account = Some(account.to_string()),
            _ => {}
        }
        if let Some(bot) = bot {
            self.bot = bot;
        }
    }
}

/// Redraws every roster this person appears in.
///
/// Away is a property of the person rather than of a channel, so one AWAY
/// line moves them in all of them at once - and a roster that was not redrawn
/// keeps showing them as present, which is precisely the thing away exists to
/// correct.
pub(super) fn refresh_rosters_containing(state: &AppState, account_id: &str, nick: &str, channels: &HashMap<String, HashMap<String, Who>>) {
    for (channel, members) in channels.iter() {
        if members.contains_key(nick) {
            emit_presence(state, account_id, channel, members);
        }
    }
}

pub(super) fn emit_presence(state: &AppState, account_id: &str, channel: &str, members: &HashMap<String, Who>) {
    let buffer_id = crate::model::buffer_id(account_id, channel);
    let member_list: Vec<_> = members
        .iter()
        .map(|(nick, who)| json!({
            "nick": nick,
            "prefix": who.rank.prefix(),
            // Absent rather than null where the network never said, so a
            // client can tell "not known" from "known to be nothing".
            "host": who.host,
            "account": who.account,
            "bot": who.bot,
            // Was hardcoded false for everybody, which made the field a
            // decoration rather than a fact. It is the server's answer now -
            // from away-notify where the network has it, and from a WHOIS or
            // a bounced message where it does not.
            "away": state.runtime.is_irc_away(account_id, nick),
        }))
        .collect();
    let member_list = json!(member_list);
    // Persisted so a client subscribing after this point (reopening the
    // buffer, or a fresh UI session) can get the current roster immediately
    // via subscribe's replay instead of waiting for the next incremental
    // change - see Runtime::get_presence and rpc/methods.rs's subscribe.
    state.runtime.set_presence(&buffer_id, member_list.clone());
    state.events.emit("presenceChange", json!({ "bufferId": buffer_id, "members": member_list }));
}

#[cfg(test)]
mod who_tests {
    use super::*;

    /// Answers arrive from several places in no fixed order - NAMES on join,
    /// a WHO reply moments later, an `account` tag on the first thing
    /// somebody says. A later answer that knows less must not erase what an
    /// earlier one knew, or a roster would flicker between complete and bare.
    #[test]
    fn a_later_answer_does_not_erase_an_earlier_one() {
        let mut who = Who::ranked(MemberRank::Op);
        who.learn(Some("ada@example.org"), Some("adalovelace"), Some(false));
        // A plain WHO reply carries no account at all.
        who.learn(Some("ada@example.org"), None, None);
        assert_eq!(who.account.as_deref(), Some("adalovelace"));
        assert_eq!(who.host.as_deref(), Some("ada@example.org"));
        assert_eq!(who.rank, MemberRank::Op);
    }

    /// Signing out of services is a fact, not an absence: the wire says so
    /// with "0" or "*", and a client that treated those as "no answer" would
    /// keep showing an account somebody has just left.
    #[test]
    fn signing_out_clears_the_account() {
        let mut who = Who::default();
        who.learn(None, Some("adalovelace"), None);
        assert_eq!(who.account.as_deref(), Some("adalovelace"));

        who.learn(None, Some("0"), None);
        assert!(who.account.is_none());

        who.learn(None, Some("adalovelace"), None);
        who.learn(None, Some("*"), None);
        assert!(who.account.is_none());
    }

    /// An empty string is the server having nothing to say, which is not the
    /// same as it saying there is nothing.
    #[test]
    fn nothing_said_changes_nothing() {
        let mut who = Who::default();
        who.learn(Some("ada@example.org"), None, None);
        who.learn(Some(""), Some(""), None);
        assert_eq!(who.host.as_deref(), Some("ada@example.org"));
        assert!(who.account.is_none());
        assert!(!who.bot);
    }
}

#[cfg(test)]
mod notify_tests {
    use super::{note_presence, notify_list};
    use std::collections::HashMap;

    fn config(notify: &str) -> crate::accounts::IrcAccountConfig {
        let mut c = crate::accounts::IrcAccountConfig {
            nick: "me".into(),
            host: "example.org".into(),
            ..Default::default()
        };
        c.notify = notify.to_string();
        c
    }

    #[test]
    fn the_list_reads_back_as_written() {
        assert_eq!(notify_list(&config("ada, grace ,,")), vec!["ada", "grace"]);
        assert!(notify_list(&config("")).is_empty());
    }

    #[test]
    fn the_first_answer_about_somebody_is_never_news() {
        let mut seen = HashMap::new();
        // Signing on and finding somebody already there is not an arrival.
        assert!(!note_presence(&mut seen, "ada", true));
        assert!(!note_presence(&mut seen, "ada", true));
        // Going is.
        assert!(note_presence(&mut seen, "ada", false));
        assert!(note_presence(&mut seen, "ada", true));
    }

    #[test]
    fn a_nick_is_the_same_nick_in_any_case() {
        let mut seen = HashMap::new();
        assert!(!note_presence(&mut seen, "Ada", true));
        // The server may answer in a different case than the list was
        // written in; announcing that as a second person would be wrong.
        assert!(note_presence(&mut seen, "ada", false));
    }
}

#[cfg(test)]
mod away_and_split_tests {
    use super::is_netsplit;

    /// The only thing that marks a netsplit is the shape of its quit message:
    /// two server names and nothing else. Anything a person could have typed
    /// has to fall the other way, because a quit wrongly folded into a split
    /// is a quit nobody ever sees.
    #[test]
    fn a_split_is_two_servers_and_nothing_else() {
        assert!(is_netsplit("irc.example.net hub.example.net"));
        assert!(is_netsplit("card.freenode.net orwell.freenode.net"));

        assert!(!is_netsplit("Leaving"));
        assert!(!is_netsplit("brb food"));
        assert!(!is_netsplit(""));
        // Three words is somebody talking, not a split.
        assert!(!is_netsplit("a.example b.example c.example"));
        // A quit message that happens to name one server is still a quit.
        assert!(!is_netsplit("irc.example.net"));
        // Quit messages carrying a URL are common and are not splits.
        assert!(!is_netsplit("https://example.com hexchat.example"));
    }
}
