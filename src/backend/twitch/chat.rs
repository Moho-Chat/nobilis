//! What Twitch's lines mean, and what becomes of them here.
//!
//! `irc.rs` reads a line into tags and parameters; this turns it into what the
//! rest of nobilis knows: a message in a channel's scrollback with its sender's
//! colour and badges, a card for a subscription or a raid, a message marked as
//! removed, a channel's modes in its header. The shapes are Kick's on purpose -
//! the same cards, the same removed-message marking, the same roster counted
//! without a cap - so the window draws a Twitch channel with the code it
//! already has.
//!
//! Everything here is a function of the line and the connection's `Rooms`, so
//! the tests feed it lines captured from live channels and read the store.

use super::irc::Line;
use crate::backend::kick::api::Badge;
use crate::model::{Embed, Moderated, ReplyPreview, SenderStyle};
use crate::state::AppState;
use std::collections::{HashMap, HashSet, VecDeque};

/// How many people are listed for a channel, most recent first. The count of
/// everybody who has spoken is kept apart and is not cut off - see
/// `kick::chat::SPEAKERS_REMEMBERED` for why the two differ.
const LISTED: usize = 500;

/// Gift subscriptions announced as a batch arrive again, one line each, after
/// the line that announced them. Remembered by the batch's id so those lines
/// are not drawn a second time; only the last so many matter.
const BATCHES_REMEMBERED: usize = 64;

/// What one connection knows about the channels it is in.
#[derive(Default)]
pub struct Rooms {
    /// Our own name here: the login, and how Twitch writes it for display.
    /// Empty while anonymous.
    pub me_login: String,
    pub me_display: String,
    /// Twitch's own number for us, from the account.
    pub me_id: String,
    /// What we look like when we speak: Twitch tells us on joining and after
    /// each message sent, and nothing else gives it.
    pub me_color: Option<String>,
    pub me_badges: HashMap<String, Vec<Badge>>,
    /// Twitch's number for each channel we are in, from its room state. It is
    /// what the official API is keyed by, and how a mirrored message is
    /// recognised as one from a channel we are in as well.
    pub room_ids: HashMap<String, String>,
    /// Each channel's modes, kept whole because Twitch sends only what changed.
    modes: HashMap<String, Modes>,
    speakers: HashMap<String, Vec<Speaker>>,
    chatters: HashMap<String, HashSet<String>>,
    batches: VecDeque<String>,
    /// Whether we are a moderator (or the broadcaster) in each channel, which
    /// is what decides how fast we may send.
    pub moderator_in: HashSet<String>,
}

#[derive(Default, Clone, PartialEq, Debug)]
struct Modes {
    emote_only: bool,
    /// Minutes a person must have followed for; `Some(0)` is "any follower".
    followers_only: Option<i64>,
    slow: i64,
    subs_only: bool,
    unique: bool,
}

#[derive(Clone)]
struct Speaker {
    login: String,
    display: String,
    user_id: String,
    badge: Option<String>,
}

impl Rooms {
    /// A connection's state for an account: who it is, and no channels yet.
    pub fn new(login: &str, user_id: &str) -> Self {
        Self { me_login: login.to_string(), me_id: user_id.to_string(), ..Default::default() }
    }

    pub fn forget_channel(&mut self, channel: &str) {
        self.room_ids.remove(channel);
        self.modes.remove(channel);
        self.speakers.remove(channel);
        self.chatters.remove(channel);
        self.moderator_in.remove(channel);
    }

    /// Records that somebody spoke; true when the roster changed enough to say so.
    fn heard(&mut self, channel: &str, who: Speaker) -> bool {
        self.chatters.entry(channel.to_string()).or_default().insert(who.login.clone());
        let list = self.speakers.entry(channel.to_string()).or_default();
        if let Some(existing) = list.iter_mut().find(|s| s.login == who.login) {
            let changed = existing.badge != who.badge;
            existing.badge = who.badge;
            existing.display = who.display;
            return changed;
        }
        list.insert(0, who);
        list.truncate(LISTED);
        true
    }

    fn roster(&self, channel: &str) -> Vec<serde_json::Value> {
        self.speakers
            .get(channel)
            .map(|list| {
                list.iter()
                    .map(|s| {
                        serde_json::json!({
                            "nick": s.display,
                            "userId": s.user_id,
                            "prefix": s.badge.clone().unwrap_or_default(),
                            "away": false,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// How many different people have spoken in a channel since it was opened.
    pub fn chatter_count(&self, channel: &str) -> usize {
        self.chatters.get(channel).map_or(0, |set| set.len())
    }

    fn batch_seen(&mut self, id: &str) -> bool {
        if self.batches.iter().any(|b| b == id) {
            return true;
        }
        self.batches.push_back(id.to_string());
        while self.batches.len() > BATCHES_REMEMBERED {
            self.batches.pop_front();
        }
        false
    }
}

/// Where a channel's conversation is: its name, which is its login.
fn buffer_id(account_id: &str, channel: &str) -> String {
    crate::model::buffer_id(account_id, channel)
}

// ---------------------------------------------------------------------------
// Pieces of a message
// ---------------------------------------------------------------------------

/// The text with each of Twitch's own emotes replaced by the token the window
/// draws a picture for: `[twitch:emote:<id>:<name>]`.
///
/// The `emotes` tag is `<id>:<from>-<to>,<from>-<to>/<id>:...`, with positions
/// counted in **characters** (Unicode code points) of the text as sent - not
/// bytes and not UTF-16 units, which is why an emoji in front of an emote is
/// the usual way this goes wrong. A range that does not fit the text is
/// skipped rather than trusted.
pub fn emote_tokens(text: &str, tag: &str) -> String {
    if tag.is_empty() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<(usize, usize, &str)> = Vec::new();
    for emote in tag.split('/') {
        let Some((id, ranges)) = emote.split_once(':') else { continue };
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        for range in ranges.split(',') {
            let Some((from, to)) = range.split_once('-') else { continue };
            let (Ok(from), Ok(to)) = (from.parse::<usize>(), to.parse::<usize>()) else { continue };
            if from <= to && to < chars.len() {
                spans.push((from, to, id));
            }
        }
    }
    spans.sort_by_key(|s| s.0);
    let mut out = String::with_capacity(text.len() + spans.len() * 24);
    let mut at = 0;
    for (from, to, id) in spans {
        if from < at {
            continue; // overlapping: the first one stands
        }
        out.extend(&chars[at..from]);
        let name: String = chars[from..=to].iter().map(|c| if *c == ']' { '_' } else { *c }).collect();
        out.push_str(&format!("[twitch:emote:{id}:{name}]"));
        at = to + 1;
    }
    out.extend(&chars[at..]);
    out
}

/// The badges that say something about a person, from the `badges` and
/// `badge-info` tags, in the order Twitch ranks them.
///
/// Drawn as words for now (the pictures come with the channel's own badge
/// sets): the roles worth looking for in a busy chat, and a mark on a first
/// message. A set nobody here has a word for - a campaign's, an event's - is
/// left out rather than drawn as three meaningless letters.
pub fn badges_of(line: &Line) -> Vec<Badge> {
    let info: HashMap<&str, &str> = line.tag("badge-info").split(',').filter_map(|b| b.split_once('/')).collect();
    let mut out: Vec<Badge> = Vec::new();
    for badge in line.tag("badges").split(',') {
        let Some((set, _version)) = badge.split_once('/') else { continue };
        let (text, rank) = match set {
            "broadcaster" => ("Broadcaster", 100),
            "staff" => ("Twitch staff", 95),
            "admin" => ("Twitch admin", 94),
            "global_mod" => ("Global moderator", 93),
            "moderator" => ("Moderator", 80),
            "lead_moderator" => ("Lead moderator", 81),
            "vip" => ("VIP", 70),
            "founder" => ("Founder", 62),
            "subscriber" => ("Subscriber", 60),
            "partner" => ("Verified", 50),
            _ => continue,
        };
        // Months, for the badges that count them: a subscriber's `badge-info` says how long.
        let months = info.get(set).and_then(|m| m.parse::<u32>().ok());
        out.push(Badge {
            kind: if set == "partner" { "verified".to_string() } else if set == "lead_moderator" { "moderator".to_string() } else { set.to_string() },
            text: text.to_string(),
            count: months,
            image: None,
            sort_order: Some(rank),
        });
    }
    if line.tag("first-msg") == "1" {
        out.push(Badge { kind: "first-message".into(), text: "First message in this channel".into(), count: None, image: None, sort_order: Some(0) });
    }
    if !line.tag("source-room-id").is_empty() && line.tag("source-room-id") != line.tag("room-id") {
        out.push(Badge { kind: "shared-chat".into(), text: "From another channel in a shared chat".into(), count: None, image: None, sort_order: Some(1) });
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.sort_order.unwrap_or(0)));
    out
}

/// The word that marks somebody in the roster: their strongest badge.
fn strongest(badges: &[Badge]) -> Option<String> {
    badges.iter().find(|b| matches!(b.kind.as_str(), "broadcaster" | "moderator" | "vip" | "founder" | "subscriber")).map(|b| b.kind.clone())
}

/// What a reply was answering, from the `reply-*` tags, and the text with the
/// `@name ` Twitch puts on the front of a reply taken off: it is there for the
/// clients that cannot show a reply, and doubles what the quote already says.
pub fn reply_of(line: &Line, text: &str) -> (Option<ReplyPreview>, String) {
    let parent = line.tag("reply-parent-msg-id");
    if parent.is_empty() {
        return (None, text.to_string());
    }
    let from = Some(line.tag("reply-parent-display-name")).filter(|s| !s.is_empty()).unwrap_or_else(|| line.tag("reply-parent-user-login"));
    let root = Some(line.tag("reply-thread-parent-msg-id")).filter(|s| !s.is_empty()).unwrap_or(parent);
    let login = line.tag("reply-parent-user-login");
    let body = match text.strip_prefix('@') {
        Some(rest) if !login.is_empty() => {
            let (name, tail) = rest.split_once(' ').unwrap_or((rest, ""));
            if name.eq_ignore_ascii_case(login) { tail.to_string() } else { text.to_string() }
        }
        _ => text.to_string(),
    };
    (
        Some(ReplyPreview {
            id: parent.to_string(),
            from: from.to_string(),
            body: line.tag("reply-parent-msg-body").to_string(),
            thread: false,
            forwarded: false,
            thread_root: Some(root.to_string()),
        }),
        body,
    )
}

/// Splits Twitch's `/me` marker off a message: (is an action, the text).
pub fn action_of(text: &str) -> (bool, &str) {
    match text.strip_prefix("\u{1}ACTION ").and_then(|t| t.strip_suffix('\u{1}')) {
        Some(inner) => (true, inner),
        None => (false, text),
    }
}

fn millis_to_secs(ms: &str) -> Option<i64> {
    ms.parse::<i64>().ok().map(|m| m / 1000)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// What reading a line asks of the connection.
#[derive(Debug, PartialEq, Default)]
pub struct Outcome {
    /// Twitch said to reconnect.
    pub reconnect: bool,
    /// Twitch refused the sign-in.
    pub refused: Option<String>,
    /// A channel the account is now in, learned from its room state.
    pub joined: Option<String>,
    /// A channel Twitch will not have us in, with why.
    pub barred: Option<(String, String)>,
}

/// Reads one line. `now` is when it is being read, for a line with no time of its own.
pub fn handle(state: &AppState, account_id: &str, rooms: &mut Rooms, line: &Line, historical: bool) -> Outcome {
    let mut outcome = Outcome::default();
    match line.command.as_str() {
        "PRIVMSG" => privmsg(state, account_id, rooms, line, historical),
        "USERNOTICE" => usernotice(state, account_id, rooms, line),
        "CLEARCHAT" => clearchat(state, account_id, rooms, line),
        "CLEARMSG" => clearmsg(state, account_id, line),
        "ROOMSTATE" => {
            if let Some(channel) = roomstate(state, account_id, rooms, line) {
                outcome.joined = Some(channel);
            }
        }
        "NOTICE" => {
            if let Some(refused) = notice(state, account_id, line) {
                outcome.refused = Some(refused);
            } else if let Some(barred) = barred_channel(line) {
                outcome.barred = Some(barred);
            }
        }
        "GLOBALUSERSTATE" => globaluserstate(state, account_id, rooms, line),
        "USERSTATE" => userstate(rooms, line),
        "RECONNECT" => outcome.reconnect = true,
        _ => {}
    }
    outcome
}

fn privmsg(state: &AppState, account_id: &str, rooms: &mut Rooms, line: &Line, historical: bool) {
    let Some(channel) = line.channel() else { return };
    let login = line.nick().unwrap_or_default().to_lowercase();
    if login.is_empty() {
        return;
    }
    // A shared chat shows every message in every channel taking part, as that
    // channel's own copy. One drawn in a channel we are also in is the same
    // message twice.
    let source = line.tag("source-room-id");
    if !source.is_empty() && source != line.tag("room-id") && rooms.room_ids.values().any(|id| id == source) {
        return;
    }
    let display = Some(line.tag("display-name")).filter(|d| !d.is_empty()).map(str::to_string).unwrap_or_else(|| login.clone());
    let (reply, plain) = reply_of(line, line.text());
    let (action, plain) = action_of(&plain);
    let body = emote_tokens(plain, if reply.is_some() { "" } else { line.tag("emotes") });
    // A reply's text lost its first word, and the emote positions were counted
    // before that: they are no use against what is left.
    let badges = badges_of(line);
    let style = SenderStyle { color: Some(line.tag("color").to_string()).filter(|c| !c.is_empty()), badges: badges.clone() };

    let buffer = buffer_id(account_id, &channel);
    if rooms.heard(&channel, Speaker { login: login.clone(), display: display.clone(), user_id: line.tag("user-id").to_string(), badge: strongest(&badges) }) {
        let roster = serde_json::json!(rooms.roster(&channel));
        let total = rooms.chatter_count(&channel);
        state.runtime.set_presence(&buffer, roster.clone());
        state.runtime.set_presence_total(&buffer, total);
        state.events.emit("presenceChange", serde_json::json!({ "bufferId": buffer, "members": roster, "total": total }));
    }
    // Being named is being highlighted, by login: the display name can be in
    // another script, and what people type after an @ is the login.
    let mentioned = !rooms.me_login.is_empty() && login != rooms.me_login && mentions(plain, &rooms.me_login);
    state.runtime.record_message_at(
        state,
        account_id,
        &channel,
        "channel",
        &display,
        &body,
        action,
        "chat",
        reply,
        Some(line.tag("id").to_string()).filter(|i| !i.is_empty()),
        mentioned && !historical,
        None,
        Vec::new(),
        Vec::new(),
        Some(line.tag("user-id").to_string()).filter(|i| !i.is_empty()),
        millis_to_secs(line.tag("tmi-sent-ts")),
        None,
        Some(style),
    );
}

/// Whether a message names a login: as a word, with or without the `@`.
fn mentions(text: &str, login: &str) -> bool {
    let login = login.to_lowercase();
    text.to_lowercase().split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|word| word == login)
}

fn usernotice(state: &AppState, account_id: &str, rooms: &mut Rooms, line: &Line) {
    let Some(channel) = line.channel() else { return };
    let mut kind = line.tag("msg-id").to_string();
    // A notice mirrored from another channel of a shared chat is that channel's
    // business, and only its announcements are worth repeating here.
    if kind == "sharedchatnotice" {
        let source = line.tag("source-msg-id").to_string();
        if source != "announcement" {
            return;
        }
        kind = source;
    }
    let who_login = line.tag("login");
    let who = Some(line.tag("display-name")).filter(|d| !d.is_empty()).unwrap_or(who_login).to_string();
    let message = line.text().trim();
    let said = |what: &str, k: &str, card: Embed| {
        state.runtime.record_message(state, account_id, &channel, "channel", &channel, what, false, k, None, None, false, None, vec![card], Vec::new(), None);
    };
    // What the person typed with their sub, if they typed anything, is an
    // ordinary message from them after the card.
    let typed = |rooms: &mut Rooms| {
        if message.is_empty() {
            return;
        }
        let user_id = line.tag("user-id").to_string();
        let badges = badges_of(line);
        let style = SenderStyle { color: Some(line.tag("color").to_string()).filter(|c| !c.is_empty()), badges: badges.clone() };
        rooms.heard(&channel, Speaker { login: who_login.to_string(), display: who.clone(), user_id: user_id.clone(), badge: strongest(&badges) });
        state.runtime.record_message_at(
            state,
            account_id,
            &channel,
            "channel",
            &who,
            &emote_tokens(message, line.tag("emotes")),
            false,
            "chat",
            None,
            // Not the notice's id as it is: the card carries that.
            (!line.tag("id").is_empty()).then(|| format!("{}:message", line.tag("id"))),
            false,
            None,
            Vec::new(),
            Vec::new(),
            Some(user_id).filter(|i| !i.is_empty()),
            millis_to_secs(line.tag("tmi-sent-ts")),
            None,
            Some(style),
        );
    };

    match kind.as_str() {
        "sub" | "resub" => {
            let months = line.tag("msg-param-cumulative-months").parse::<u64>().ok().filter(|m| *m > 1);
            said(
                &format!("{who} subscribed"),
                "sub",
                Embed { kind: Some("subscribe".into()), author: Some(who.clone()), title: months.map(|m| m.to_string()), ..Default::default() },
            );
            typed(rooms);
        }
        "subgift" | "anonsubgift" => {
            // One of a batch: the batch's own line already said how many.
            let origin = line.tag("msg-param-origin-id");
            // A batch we heard announced is told once, by that: these are its lines.
            if !origin.is_empty() && rooms.batches.iter().any(|b| *b == format!("mass:{origin}")) {
                return;
            }
            let recipient = Some(line.tag("msg-param-recipient-display-name")).filter(|r| !r.is_empty()).unwrap_or(line.tag("msg-param-recipient-user-name")).to_string();
            let gifter = if kind == "anonsubgift" { "An anonymous gifter".to_string() } else { who.clone() };
            said(
                &format!("{gifter} gifted a sub to {recipient}"),
                "sub",
                Embed { kind: Some("gift-sub".into()), author: Some(gifter), title: Some("1".into()), description: Some(recipient), ..Default::default() },
            );
        }
        "submysterygift" | "anonsubmysterygift" => {
            let count = line.tag("msg-param-mass-gift-count").parse::<u64>().unwrap_or(1).max(1);
            let origin = line.tag("msg-param-origin-id");
            if !origin.is_empty() {
                rooms.batch_seen(&format!("mass:{origin}"));
            }
            let gifter = if kind == "anonsubmysterygift" { "An anonymous gifter".to_string() } else { who.clone() };
            said(
                &format!("{gifter} gifted {count} subs"),
                "sub",
                Embed { kind: Some("gift-sub".into()), author: Some(gifter), title: Some(count.to_string()), ..Default::default() },
            );
        }
        "giftpaidupgrade" | "anongiftpaidupgrade" => {
            let from = line.tag("msg-param-sender-name");
            let text = if from.is_empty() || kind == "anongiftpaidupgrade" { format!("{who} is continuing the gift sub they got") } else { format!("{who} is continuing the gift sub they got from {from}") };
            state.runtime.record_message(state, account_id, &channel, "channel", &channel, &text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
        }
        "raid" => {
            let raider = Some(line.tag("msg-param-displayName")).filter(|r| !r.is_empty()).unwrap_or(line.tag("msg-param-login")).to_string();
            let viewers = line.tag("msg-param-viewerCount").parse::<u64>().ok().filter(|v| *v > 0);
            said(
                &format!("{raider} raided"),
                "raid",
                Embed { kind: Some("raid".into()), author: Some(raider), title: viewers.map(|v| v.to_string()), ..Default::default() },
            );
        }
        "unraid" => {
            state.runtime.record_message(state, account_id, &channel, "channel", &channel, "The raid was called off", false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
        }
        "ritual" => {
            if line.tag("msg-param-ritual-name") == "new_chatter" {
                state.runtime.record_message(state, account_id, &channel, "channel", &channel, &format!("{who} is new to the channel"), false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                typed(rooms);
            }
        }
        "announcement" => {
            // The colour Twitch's own chat draws the bar in.
            let colour = match line.tag("msg-param-color") {
                "BLUE" => Some(0x2E8BFF),
                "GREEN" => Some(0x00C877),
                "ORANGE" => Some(0xFF8C00),
                "PURPLE" => Some(0x9146FF),
                _ => Some(0x9146FF),
            };
            said(
                &format!("Announcement from {who}: {message}"),
                "announcement",
                Embed { kind: Some("notice".into()), title: Some(format!("Announcement from {who}")), description: Some(emote_tokens(message, line.tag("emotes"))), color: colour, ..Default::default() },
            );
        }
        "viewermilestone" => {
            let value = line.tag("msg-param-value");
            let text = if line.tag("msg-param-category") == "watch-streak" {
                format!("{who} has watched {value} streams in a row")
            } else {
                format!("{who} reached a milestone")
            };
            state.runtime.record_message(state, account_id, &channel, "channel", &channel, &text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            typed(rooms);
        }
        "bitsbadgetier" => {
            let tier = line.tag("msg-param-threshold");
            state.runtime.record_message(state, account_id, &channel, "channel", &channel, &format!("{who} unlocked the {tier} bits badge"), false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            typed(rooms);
        }
        _ => {}
    }
}

fn clearchat(state: &AppState, account_id: &str, rooms: &mut Rooms, line: &Line) {
    let Some(channel) = line.channel() else { return };
    let buffer = buffer_id(account_id, &channel);
    let target = line.text().trim();
    if target.is_empty() {
        state.runtime.record_message(state, account_id, &channel, "channel", &channel, "Chat was cleared by a moderator", false, "moderation", None, None, false, None, Vec::new(), Vec::new(), None);
        return;
    }
    // Everything they said is gone from Twitch's chat, and drawn as removed
    // here - kept, so what they said can still be read.
    let user_id = line.tag("target-user-id");
    if !user_id.is_empty() {
        state.runtime.moderate_sender(state, &buffer, user_id, &Moderated::default());
    }
    let duration = line.tag("ban-duration").parse::<u64>().ok();
    let what = match duration {
        Some(seconds) => format!("was timed out for {}", span(seconds)),
        None => "was banned".to_string(),
    };
    let _ = rooms;
    state.runtime.record_message(
        state,
        account_id,
        &channel,
        "channel",
        &channel,
        &format!("{target} {what}"),
        false,
        "moderation",
        None,
        None,
        false,
        None,
        vec![Embed { kind: Some("moderation".into()), author: Some(target.to_string()), title: Some(what), ..Default::default() }],
        Vec::new(),
        None,
    );
}

/// A length of time as people say it: "10 minutes", "2 hours", "30 seconds".
pub fn span(seconds: u64) -> String {
    let (n, unit) = match seconds {
        s if s >= 86_400 && s % 86_400 == 0 => (s / 86_400, "day"),
        s if s >= 3_600 && s % 3_600 == 0 => (s / 3_600, "hour"),
        s if s >= 60 && s % 60 == 0 => (s / 60, "minute"),
        s => (s, "second"),
    };
    if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") }
}

fn clearmsg(state: &AppState, account_id: &str, line: &Line) {
    let Some(channel) = line.channel() else { return };
    let target = line.tag("target-msg-id");
    if target.is_empty() {
        return;
    }
    state.runtime.moderate_message(state, &buffer_id(account_id, &channel), target, &Moderated::default());
}

/// A channel's modes, said in words. `None` for a channel with none.
fn words(modes: &Modes) -> Option<String> {
    let mut said: Vec<String> = Vec::new();
    if modes.subs_only {
        said.push("subscribers only".into());
    }
    match modes.followers_only {
        Some(0) => said.push("followers only".into()),
        Some(minutes) if minutes > 0 => said.push(format!("followers only ({})", span(minutes as u64 * 60))),
        _ => {}
    }
    if modes.slow > 0 {
        said.push(format!("slow mode {}s", modes.slow));
    }
    if modes.emote_only {
        said.push("emote only".into());
    }
    if modes.unique {
        said.push("unique messages only".into());
    }
    (!said.is_empty()).then(|| said.join(", "))
}

/// Applies a room state and answers which channel it was, when it is one we have just joined.
fn roomstate(state: &AppState, account_id: &str, rooms: &mut Rooms, line: &Line) -> Option<String> {
    let channel = line.channel()?;
    let first = !rooms.room_ids.contains_key(&channel);
    if !line.tag("room-id").is_empty() {
        rooms.room_ids.insert(channel.clone(), line.tag("room-id").to_string());
    }
    let mut modes = rooms.modes.get(&channel).cloned().unwrap_or_default();
    // Twitch sends every mode on joining and only the one that changed after,
    // so a tag that is absent is unchanged, not off.
    let flag = |key: &str| line.tags.get(key).map(|v| v == "1");
    if let Some(v) = flag("emote-only") {
        modes.emote_only = v;
    }
    if let Some(v) = flag("subs-only") {
        modes.subs_only = v;
    }
    if let Some(v) = flag("r9k") {
        modes.unique = v;
    }
    if let Some(v) = line.tags.get("slow").and_then(|v| v.parse::<i64>().ok()) {
        modes.slow = v;
    }
    if let Some(v) = line.tags.get("followers-only").and_then(|v| v.parse::<i64>().ok()) {
        modes.followers_only = if v < 0 { None } else { Some(v) };
    }
    rooms.modes.insert(channel.clone(), modes.clone());
    state.runtime.set_channel_modes(state, &buffer_id(account_id, &channel), words(&modes));
    first.then_some(channel)
}

/// A NOTICE, said in the channel it is about (or in none, for the server's),
/// and, when it is the one that says the sign-in failed, as a refusal.
fn notice(state: &AppState, account_id: &str, line: &Line) -> Option<String> {
    let text = line.text();
    let lower = text.to_lowercase();
    if line.channel().is_none() && (lower.contains("authentication failed") || lower.contains("improperly formatted auth") || lower.contains("invalid nick")) {
        return Some(text.to_string());
    }
    if let Some(channel) = line.channel() {
        if !text.is_empty() {
            state.runtime.record_message(state, account_id, &channel, "channel", &channel, text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
        }
    }
    None
}

/// A channel Twitch says we cannot be in: suspended, or one we are banned from.
fn barred_channel(line: &Line) -> Option<(String, String)> {
    let channel = line.channel()?;
    match line.tag("msg-id") {
        "msg_channel_suspended" | "msg_banned" | "msg_channel_blocked" => Some((channel, line.text().to_string())),
        _ => None,
    }
}

fn globaluserstate(state: &AppState, account_id: &str, rooms: &mut Rooms, line: &Line) {
    let display = line.tag("display-name");
    if !display.is_empty() {
        rooms.me_display = display.to_string();
        // Our own messages are written under the name Twitch writes it, so the
        // window can tell which are ours.
        state.runtime.set_own_identity(account_id, display);
    }
    if !line.tag("user-id").is_empty() {
        rooms.me_id = line.tag("user-id").to_string();
    }
    if !line.tag("color").is_empty() {
        rooms.me_color = Some(line.tag("color").to_string());
    }
}

fn userstate(rooms: &mut Rooms, line: &Line) {
    let Some(channel) = line.channel() else { return };
    if !line.tag("color").is_empty() {
        rooms.me_color = Some(line.tag("color").to_string());
    }
    let badges = badges_of(line);
    if badges.iter().any(|b| b.kind == "broadcaster" || b.kind == "moderator") || line.tag("mod") == "1" {
        rooms.moderator_in.insert(channel.clone());
    } else {
        rooms.moderator_in.remove(&channel);
    }
    rooms.me_badges.insert(channel, badges.into_iter().filter(|b| b.kind != "first-message" && b.kind != "shared-chat").collect());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit;
    use crate::backend::twitch::irc;

    const ACCOUNT: &str = "twitch:~anonymous";

    fn feed(state: &AppState, rooms: &mut Rooms, raw: &str) -> Outcome {
        handle(state, ACCOUNT, rooms, &irc::parse(raw).expect("a line"), false)
    }

    fn log(state: &AppState, channel: &str) -> Vec<crate::model::Message> {
        state.store.get_backlog(&buffer_id(ACCOUNT, channel), 0, 100).expect("backlog")
    }

    #[test]
    fn emotes_are_found_by_character_not_by_byte() {
        // The ranges are in code points: the emoji in front is one character, not four bytes or two units.
        let text = "😀 Kappa and Kappa";
        assert_eq!(emote_tokens(text, "25:2-6,12-16"), "😀 [twitch:emote:25:Kappa] and [twitch:emote:25:Kappa]");
        assert_eq!(emote_tokens("xqcPeepo  eyebrow busta", "301445381:0-7"), "[twitch:emote:301445381:xqcPeepo]  eyebrow busta");
        // Out of order, overlapping, past the end, or not numbers: taken as far as they make sense.
        assert_eq!(emote_tokens("Kappa Kappa", "25:6-10,0-4"), "[twitch:emote:25:Kappa] [twitch:emote:25:Kappa]");
        assert_eq!(emote_tokens("hello", "25:0-99"), "hello");
        assert_eq!(emote_tokens("hello", "25:a-b"), "hello");
        assert_eq!(emote_tokens("hello", ""), "hello");
        // A name with a bracket in it cannot close its own token.
        assert_eq!(emote_tokens("[o_o]", "1:0-4"), "[twitch:emote:1:[o_o_]");
    }

    #[test]
    fn a_message_is_recorded_with_its_sender_colour_badges_and_emotes() {
        let state = testkit::simulated_daemon("twitch-privmsg");
        let mut rooms = Rooms::default();
        feed(&state, &mut rooms, "@badge-info=subscriber/37;badges=moderator/1,subscriber/36,premium/1;color=#FF69B4;display-name=Zakktheking13;emotes=301445381:0-7;first-msg=0;id=6a349c82;room-id=71092938;tmi-sent-ts=1791949381404;user-id=494821572 :zakktheking13!zakktheking13@zakktheking13.tmi.twitch.tv PRIVMSG #xqc :xqcPeepo  eyebrow busta");
        let m = &log(&state, "xqc")[0];
        assert_eq!((m.id.as_str(), m.from.as_str(), m.ts), ("6a349c82", "Zakktheking13", 1791949381));
        assert_eq!(m.body, "[twitch:emote:301445381:xqcPeepo]  eyebrow busta");
        assert_eq!(m.sender_color.as_deref(), Some("#FF69B4"));
        assert_eq!(m.sender_id.as_deref(), Some("494821572"));
        // Moderator first, then the subscriber with its months; premium is nobody's business here.
        assert_eq!(m.badges.iter().map(|b| b.kind.as_str()).collect::<Vec<_>>(), vec!["moderator", "subscriber"]);
        assert_eq!(m.badges[1].count, Some(37));
        assert_eq!(rooms.chatter_count("xqc"), 1);
    }

    #[test]
    fn an_action_is_an_action_and_a_first_message_says_so() {
        let state = testkit::simulated_daemon("twitch-action");
        let mut rooms = Rooms::default();
        feed(&state, &mut rooms, "@display-name=Ann;first-msg=1;id=a1;room-id=1;user-id=2 :ann!ann@ann.tmi.twitch.tv PRIVMSG #xqc :\u{1}ACTION waves\u{1}");
        let m = &log(&state, "xqc")[0];
        assert!(m.is_action);
        assert_eq!(m.body, "waves");
        assert!(m.badges.iter().any(|b| b.kind == "first-message"));
    }

    #[test]
    fn a_reply_quotes_what_it_answers_loses_the_at_and_keeps_its_thread() {
        let state = testkit::simulated_daemon("twitch-reply");
        let mut rooms = Rooms::default();
        feed(&state, &mut rooms, "@display-name=A_T_M_0_S;emotes=;id=b4d3;reply-parent-display-name=deathsashes112;reply-parent-msg-body=what\\sdoes\\sno\\seyebrows\\smean;reply-parent-msg-id=bc4a;reply-parent-user-login=deathsashes112;reply-thread-parent-msg-id=root1;room-id=1;user-id=3 :a_t_m_0_s!a_t_m_0_s@a_t_m_0_s.tmi.twitch.tv PRIVMSG #xqc :@deathsashes112 it means nothing");
        let m = &log(&state, "xqc")[0];
        assert_eq!(m.body, "it means nothing");
        let r = m.reply_to.clone().expect("a reply");
        assert_eq!((r.id.as_str(), r.from.as_str(), r.body.as_str(), r.thread_root.as_deref()), ("bc4a", "deathsashes112", "what does no eyebrows mean", Some("root1")));
        // A reply to somebody else's name in front is left as it was written.
        let (_, kept) = reply_of(&irc::parse("@reply-parent-msg-id=x;reply-parent-user-login=bob :a!a@a PRIVMSG #c :@alice hi").unwrap(), "@alice hi");
        assert_eq!(kept, "@alice hi");
    }

    #[test]
    fn being_named_is_a_highlight_by_login_even_when_the_display_name_differs() {
        let state = testkit::simulated_daemon("twitch-mention");
        let mut rooms = Rooms { me_login: "someone".into(), ..Default::default() };
        feed(&state, &mut rooms, "@display-name=Other;id=m1;room-id=1;user-id=3 :other!other@other.tmi.twitch.tv PRIVMSG #xqc :hey @Someone look");
        feed(&state, &mut rooms, "@display-name=Other;id=m2;room-id=1;user-id=3 :other!other@other.tmi.twitch.tv PRIVMSG #xqc :someonelse is not you");
        let l = log(&state, "xqc");
        assert!(l[0].is_highlight);
        assert!(!l[1].is_highlight, "a longer name is not yours");
    }

    #[test]
    fn a_message_deleted_stays_and_is_marked_and_a_ban_marks_everything_the_person_said() {
        let state = testkit::simulated_daemon("twitch-removed");
        let mut rooms = Rooms::default();
        for (id, user) in [("m1", "7"), ("m2", "7"), ("m3", "8")] {
            feed(&state, &mut rooms, &format!("@display-name=P{user};id={id};room-id=1;user-id={user} :p{user}!p{user}@p{user}.tmi.twitch.tv PRIVMSG #xqc :text {id}"));
        }
        feed(&state, &mut rooms, "@login=p8;target-msg-id=m3 :tmi.twitch.tv CLEARMSG #xqc :text m3");
        feed(&state, &mut rooms, "@ban-duration=600;room-id=1;target-user-id=7 :tmi.twitch.tv CLEARCHAT #xqc :p7");
        let l = log(&state, "xqc");
        let removed: Vec<bool> = l.iter().filter(|m| m.kind == "chat").map(|m| m.moderated.is_some()).collect();
        assert_eq!(removed, vec![true, true, true]);
        let card = l.iter().find(|m| m.kind == "moderation").expect("a line about the timeout");
        assert_eq!(card.embeds[0].title.as_deref(), Some("was timed out for 10 minutes"));
        assert_eq!(card.embeds[0].author.as_deref(), Some("p7"));
        // A ban with no length, and a cleared room.
        feed(&state, &mut rooms, "@room-id=1;target-user-id=9 :tmi.twitch.tv CLEARCHAT #xqc :p9");
        feed(&state, &mut rooms, "@room-id=1 :tmi.twitch.tv CLEARCHAT #xqc");
        let bodies: Vec<String> = log(&state, "xqc").iter().filter(|m| m.kind == "moderation").map(|m| m.body.clone()).collect();
        assert_eq!(bodies[1], "p9 was banned");
        assert_eq!(bodies[2], "Chat was cleared by a moderator");
    }

    #[test]
    fn a_rooms_modes_are_kept_whole_and_said_in_words() {
        let state = testkit::simulated_daemon("twitch-roomstate");
        let mut rooms = Rooms::default();
        let first = feed(&state, &mut rooms, "@emote-only=0;followers-only=10;r9k=0;room-id=71092938;slow=0;subs-only=0 :tmi.twitch.tv ROOMSTATE #xqc");
        assert_eq!(first.joined.as_deref(), Some("xqc"));
        assert_eq!(rooms.room_ids.get("xqc").map(String::as_str), Some("71092938"));
        let modes = |rooms: &Rooms| words(&rooms.modes["xqc"]);
        assert_eq!(modes(&rooms).as_deref(), Some("followers only (10 minutes)"));
        // Only the one that changed is sent; the rest stand.
        let again = feed(&state, &mut rooms, "@slow=30 :tmi.twitch.tv ROOMSTATE #xqc");
        assert_eq!(again.joined, None, "not a join the second time");
        assert_eq!(modes(&rooms).as_deref(), Some("followers only (10 minutes), slow mode 30s"));
        feed(&state, &mut rooms, "@followers-only=-1;slow=0;emote-only=1;subs-only=1 :tmi.twitch.tv ROOMSTATE #xqc");
        assert_eq!(modes(&rooms).as_deref(), Some("subscribers only, emote only"));
        feed(&state, &mut rooms, "@followers-only=0 :tmi.twitch.tv ROOMSTATE #xqc");
        assert!(modes(&rooms).unwrap().contains("followers only,") || modes(&rooms).unwrap().contains("followers only"));
    }

    #[test]
    fn subscriptions_gifts_and_raids_are_cards_and_a_batch_is_told_once() {
        let state = testkit::simulated_daemon("twitch-cards");
        let mut rooms = Rooms::default();
        feed(&state, &mut rooms, "@display-name=Fan;login=fan;msg-id=resub;msg-param-cumulative-months=14;room-id=1;user-id=5 :tmi.twitch.tv USERNOTICE #xqc :still here");
        feed(&state, &mut rooms, "@display-name=Gifter;login=gifter;msg-id=submysterygift;msg-param-mass-gift-count=3;msg-param-origin-id=abc;room-id=1 :tmi.twitch.tv USERNOTICE #xqc");
        for r in ["One", "Two", "Three"] {
            feed(&state, &mut rooms, &format!("@display-name=Gifter;login=gifter;msg-id=subgift;msg-param-origin-id=abc;msg-param-recipient-display-name={r};room-id=1 :tmi.twitch.tv USERNOTICE #xqc"));
        }
        feed(&state, &mut rooms, "@display-name=Solo;login=solo;msg-id=subgift;msg-param-recipient-display-name=Lucky;room-id=1 :tmi.twitch.tv USERNOTICE #xqc");
        feed(&state, &mut rooms, "@msg-id=raid;msg-param-displayName=Raider;msg-param-login=raider;msg-param-viewerCount=1234;room-id=1 :tmi.twitch.tv USERNOTICE #xqc");
        let l = log(&state, "xqc");
        let kinds: Vec<(&str, Option<&str>, Option<&str>)> = l.iter().filter_map(|m| m.embeds.first().map(|e| (e.kind.as_deref().unwrap_or(""), e.author.as_deref(), e.title.as_deref()))).collect();
        assert_eq!(kinds, vec![("subscribe", Some("Fan"), Some("14")), ("gift-sub", Some("Gifter"), Some("3")), ("gift-sub", Some("Solo"), Some("1")), ("raid", Some("Raider"), Some("1234"))]);
        // What the subscriber typed is a message of theirs, after the card.
        assert!(l.iter().any(|m| m.kind == "chat" && m.body == "still here" && m.from == "Fan"));
    }

    #[test]
    fn an_announcement_is_a_notice_and_a_mirrored_notice_is_not_repeated() {
        let state = testkit::simulated_daemon("twitch-announce");
        let mut rooms = Rooms::default();
        feed(&state, &mut rooms, "@display-name=Mod;login=mod;msg-id=announcement;msg-param-color=GREEN;room-id=1 :tmi.twitch.tv USERNOTICE #xqc :Giveaway at noon");
        feed(&state, &mut rooms, "@display-name=Mod;login=mod;msg-id=sharedchatnotice;source-msg-id=sub;room-id=1 :tmi.twitch.tv USERNOTICE #xqc :hi");
        let l = log(&state, "xqc");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].embeds[0].kind.as_deref(), Some("notice"));
        assert_eq!(l[0].embeds[0].description.as_deref(), Some("Giveaway at noon"));
        assert_eq!(l[0].embeds[0].color, Some(0x00C877));
    }

    #[test]
    fn a_shared_chat_message_in_a_channel_we_are_also_in_is_not_drawn_twice() {
        let state = testkit::simulated_daemon("twitch-shared");
        let mut rooms = Rooms::default();
        rooms.room_ids.insert("alpha".into(), "100".into());
        // Alpha's copy of a message said in alpha, shown in beta as well.
        feed(&state, &mut rooms, "@display-name=A;id=x1;room-id=200;source-room-id=100;user-id=1 :a!a@a.tmi.twitch.tv PRIVMSG #beta :hello both");
        assert!(log(&state, "beta").is_empty(), "alpha is joined, so alpha's own copy is the one");
        // From a channel we are not in, it is kept and says where it is from.
        feed(&state, &mut rooms, "@display-name=B;id=x2;room-id=200;source-room-id=999;user-id=2 :b!b@b.tmi.twitch.tv PRIVMSG #beta :hello from afar");
        let l = log(&state, "beta");
        assert_eq!(l.len(), 1);
        assert!(l[0].badges.iter().any(|b| b.kind == "shared-chat"));
    }

    #[test]
    fn a_notice_is_said_in_its_channel_and_a_failed_login_is_a_refusal() {
        let state = testkit::simulated_daemon("twitch-notice");
        let mut rooms = Rooms::default();
        feed(&state, &mut rooms, "@msg-id=msg_slowmode :tmi.twitch.tv NOTICE #xqc :This room is in slow mode and you are sending messages too quickly. You will be able to talk again in 29 seconds.");
        assert_eq!(log(&state, "xqc")[0].kind, "system");
        assert!(log(&state, "xqc")[0].body.contains("slow mode"));
        let refused = feed(&state, &mut rooms, ":tmi.twitch.tv NOTICE * :Login authentication failed");
        assert_eq!(refused.refused.as_deref(), Some("Login authentication failed"));
        let barred = feed(&state, &mut rooms, "@msg-id=msg_banned :tmi.twitch.tv NOTICE #xqc :You are permanently banned from talking in xqc.");
        assert_eq!(barred.barred.as_ref().map(|b| b.0.as_str()), Some("xqc"));
        assert!(feed(&state, &mut rooms, "RECONNECT").reconnect);
    }

    #[test]
    fn who_we_are_is_learned_from_the_global_state() {
        let state = testkit::simulated_daemon("twitch-me");
        let mut rooms = Rooms::default();
        feed(&state, &mut rooms, "@badge-info=;badges=;color=#8A2BE2;display-name=Someone;emote-sets=0,300374282;user-id=42 :tmi.twitch.tv GLOBALUSERSTATE");
        assert_eq!((rooms.me_display.as_str(), rooms.me_id.as_str(), rooms.me_color.as_deref()), ("Someone", "42", Some("#8A2BE2")));
        assert_eq!(state.runtime.own_identity(ACCOUNT).as_deref(), Some("Someone"));
        feed(&state, &mut rooms, "@badge-info=;badges=moderator/1;color=#8A2BE2;display-name=Someone;mod=1 :tmi.twitch.tv USERSTATE #xqc");
        assert!(rooms.moderator_in.contains("xqc"));
        assert_eq!(rooms.me_badges["xqc"][0].kind, "moderator");
    }

    #[test]
    fn the_roster_counts_everybody_and_lists_the_most_recent() {
        let state = testkit::simulated_daemon("twitch-roster");
        let mut rooms = Rooms::default();
        for i in 0..(LISTED + 20) {
            feed(&state, &mut rooms, &format!("@display-name=U{i};id=i{i};room-id=1;user-id={i} :u{i}!u{i}@u{i}.tmi.twitch.tv PRIVMSG #xqc :hi"));
        }
        assert_eq!(rooms.roster("xqc").len(), LISTED);
        assert_eq!(rooms.chatter_count("xqc"), LISTED + 20);
        assert_eq!(state.runtime.get_presence_total(&buffer_id(ACCOUNT, "xqc")), Some(LISTED + 20));
    }

    #[test]
    fn spans_are_said_as_people_say_them() {
        assert_eq!([span(30), span(600), span(3600), span(7200), span(86400), span(90)], ["30 seconds", "10 minutes", "1 hour", "2 hours", "1 day", "90 seconds"]);
    }
}
