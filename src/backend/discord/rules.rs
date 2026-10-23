//! What Discord's own rules say a message needs before it is worth sending - checked here, from what login and the
//! guild already told us, so a message that is bound to be turned down is not sent at all. A refused send is one more
//! request from an account that is being watched, and says nothing that a sentence cannot say sooner.
//!
//! The server rules are Discord's documented ones (and Concord's reading of them, `discord/verification.rs`): a
//! server's verification level asks for a verified email (low), an account older than five minutes (medium), a member
//! of ten minutes' standing (high) or a verified phone (very high); membership screening and onboarding must be done;
//! and a member who is timed out may not speak. The owner, anybody with every permission, anybody with a role (for
//! the levels) and anybody the server exempts (`BYPASSES_VERIFICATION`) are not held to the levels.

use std::time::{Duration, Instant};

/// Member flags (the `flags` of a guild member).
const FLAG_COMPLETED_ONBOARDING: u64 = 1 << 1;
const FLAG_BYPASSES_VERIFICATION: u64 = 1 << 2;

/// How long a member of a server wears Discord's green "new member" badge by their name: the first week.
pub const NEW_MEMBER_SECS: i64 = 7 * 24 * 60 * 60;

const ACCOUNT_AGE_SECS: i64 = 5 * 60;
const MEMBER_AGE_SECS: i64 = 10 * 60;

/// What a server asks of everyone, as its payload says.
#[derive(Debug, Clone, Default)]
pub struct GuildFacts {
    /// 0 none, 1 low, 2 medium, 3 high, 4 very high.
    pub verification: u8,
    pub community: bool,
    pub owner: bool,
}

/// This account's membership of it.
#[derive(Debug, Clone, Default)]
pub struct MemberFacts {
    pub pending: Option<bool>,
    pub flags: Option<u64>,
    pub has_roles: bool,
    /// When it joined, Unix seconds.
    pub joined_at: Option<i64>,
    /// Until when it may not speak (a timeout), Unix seconds.
    pub timed_out_until: Option<i64>,
}

/// And the account itself.
#[derive(Debug, Clone, Default)]
pub struct AccountFacts {
    pub email_verified: Option<bool>,
    pub phone_verified: Option<bool>,
    /// When it was made, Unix seconds (from its id).
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restriction {
    Screening,
    Onboarding,
    EmailRequired,
    PhoneRequired,
    AccountTooNew(i64),
    MemberTooNew(i64),
    TimedOut(i64),
    /// Wearing the green new-member badge: for as many more seconds.
    NewMember(i64),
}

impl Restriction {
    /// The sentence a refused send gives.
    pub fn sentence(&self) -> String {
        match self {
            Restriction::Screening => "This server asks you to accept its rules first. Do that from the notice above the conversation, or in Discord's own app.".into(),
            Restriction::Onboarding => "This server asks you to finish its onboarding first - do that in Discord's own app.".into(),
            Restriction::EmailRequired => "This server needs a verified email on your Discord account before you can talk here.".into(),
            Restriction::PhoneRequired => "This server needs a verified phone number on your Discord account before you can talk here.".into(),
            Restriction::AccountTooNew(s) => format!("This server asks for an account more than five minutes old: {}.", wait(*s)),
            Restriction::MemberTooNew(s) => format!("This server asks you to have been a member for ten minutes before you talk: {}.", wait(*s)),
            Restriction::TimedOut(s) => format!("You are timed out in this server: {}.", wait(*s)),
            Restriction::NewMember(s) => format!(
                "You joined this server recently, so Discord shows the green new-member badge by your name here, for about {} more. Messages from another client while it shows are treated as spam: use Discord's own app until it is gone.",
                long_wait(*s)
            ),
        }
    }

    /// Whether it is the kind that stays until the person does something, and so is worth a lock over the box. The
    /// others pass by themselves, and are said when a send is tried. (Screening has a notice of its own.)
    pub fn banner(&self) -> Option<&'static str> {
        match self {
            Restriction::Onboarding => Some("This server asks you to finish its onboarding first - do that in Discord's own app."),
            Restriction::EmailRequired => Some("This server needs a verified email on your Discord account before you can talk here."),
            Restriction::PhoneRequired => Some("This server needs a verified phone number on your Discord account before you can talk here."),
            Restriction::NewMember(_) => Some("You are a new member of this server: Discord shows a green badge by your name for the first week, and treats messages from another client as spam while it shows. Use Discord's own app until it is gone; this opens again then."),
            _ => None,
        }
    }
}

/// A wait counted in days or hours, for one that is long.
fn long_wait(seconds: i64) -> String {
    let hours = (seconds.max(0) + 3599) / 3600;
    if hours >= 48 {
        format!("{} days", (hours + 23) / 24)
    } else if hours > 1 {
        format!("{hours} hours")
    } else {
        "an hour".to_string()
    }
}

fn wait(seconds: i64) -> String {
    if seconds >= 120 {
        format!("wait {} more minutes", (seconds + 59) / 60)
    } else {
        format!("wait {} more seconds", seconds.max(1))
    }
}

/// Whether the account may speak in a server now, and if not why. `full` is every permission (the owner, an
/// administrator); `onboarding_enabled` is what the server's onboarding says of itself, where it was asked.
pub fn participation(
    guild: &GuildFacts,
    member: Option<&MemberFacts>,
    account: &AccountFacts,
    full: bool,
    onboarding_enabled: Option<bool>,
    now: i64,
) -> Option<Restriction> {
    if guild.owner {
        return None;
    }
    // Not known is not blocked: a check that stops everything for want of data is a lock nobody can open.
    let member = member?;
    if let Some(until) = member.timed_out_until.filter(|u| *u > now) {
        return Some(Restriction::TimedOut(until - now));
    }
    if member.pending == Some(true) {
        return Some(Restriction::Screening);
    }
    if full {
        return None;
    }
    let flags = member.flags.unwrap_or(0);
    if flags & FLAG_BYPASSES_VERIFICATION != 0 {
        return None;
    }
    if onboarding_enabled == Some(true) && flags & FLAG_COMPLETED_ONBOARDING == 0 {
        return Some(Restriction::Onboarding);
    }
    if guild.verification == 0 || member.has_roles || account.phone_verified == Some(true) {
        return None;
    }
    if guild.verification >= 4 {
        return (account.phone_verified == Some(false)).then_some(Restriction::PhoneRequired);
    }
    if account.email_verified == Some(false) {
        return Some(Restriction::EmailRequired);
    }
    if guild.verification >= 2 {
        let age = now - account.created_at;
        if account.created_at > 0 && age < ACCOUNT_AGE_SECS {
            return Some(Restriction::AccountTooNew(ACCOUNT_AGE_SECS - age));
        }
    }
    if guild.verification >= 3 {
        if let Some(joined) = member.joined_at {
            let age = now - joined;
            if age < MEMBER_AGE_SECS {
                return Some(Restriction::MemberTooNew(MEMBER_AGE_SECS - age));
            }
        }
    }
    None
}

/// Whether the account still wears the green new-member badge: it joined the server in the last week. Discord treats
/// what a person says from another client in that time as spam, so it is a reason not to speak from here, whatever
/// else the server allows. The owner has no such badge; `joined` is when the account joined if the member does not say.
pub fn new_member(guild: &GuildFacts, member: Option<&MemberFacts>, joined: Option<i64>, now: i64) -> Option<Restriction> {
    if guild.owner {
        return None;
    }
    let joined = member.and_then(|m| m.joined_at).or(joined)?;
    let age = now - joined;
    // A join time in the future is a clock that is wrong, and not a reason to lock anything.
    (0..NEW_MEMBER_SECS).contains(&age).then_some(Restriction::NewMember(NEW_MEMBER_SECS - age))
}

/// How long until a channel's slowmode lets the account speak again: `seconds` is the channel's setting, `last` the
/// account's last message there, `bypass` the permissions that are not held to it (manage messages, manage channel).
pub fn slowmode_remaining(seconds: u64, last: Option<Instant>, bypass: bool, now: Instant) -> Option<Duration> {
    if seconds == 0 || bypass {
        return None;
    }
    let ends = last? + Duration::from_secs(seconds);
    (ends > now).then(|| ends - now)
}

/// Own messages (not sent from here) a direct message needs before it counts as a conversation that is begun.
pub const ESTABLISHED_DM_MESSAGES: usize = 5;

#[cfg(test)]
mod tests {
    use super::*;

    fn guild(level: u8) -> GuildFacts {
        GuildFacts { verification: level, community: false, owner: false }
    }
    fn account() -> AccountFacts {
        AccountFacts { email_verified: Some(true), phone_verified: Some(false), created_at: 1_000_000 }
    }
    const NOW: i64 = 2_000_000;

    #[test]
    fn an_open_server_asks_nothing_of_a_member_in_good_standing() {
        let member = MemberFacts { pending: Some(false), flags: Some(0), ..Default::default() };
        assert_eq!(participation(&guild(0), Some(&member), &account(), false, None, NOW), None);
    }

    #[test]
    fn each_level_asks_for_what_it_says() {
        let member = MemberFacts { pending: Some(false), flags: Some(0), joined_at: Some(NOW - 60), ..Default::default() };
        let mut a = account();
        a.email_verified = Some(false);
        assert_eq!(participation(&guild(1), Some(&member), &a, false, None, NOW), Some(Restriction::EmailRequired));
        a.email_verified = Some(true);
        assert_eq!(participation(&guild(1), Some(&member), &a, false, None, NOW), None);
        // Medium: the account must be over five minutes old.
        a.created_at = NOW - 100;
        assert_eq!(participation(&guild(2), Some(&member), &a, false, None, NOW), Some(Restriction::AccountTooNew(200)));
        a.created_at = 1_000_000;
        assert_eq!(participation(&guild(2), Some(&member), &a, false, None, NOW), None);
        // High: and the membership over ten minutes.
        assert_eq!(participation(&guild(3), Some(&member), &a, false, None, NOW), Some(Restriction::MemberTooNew(540)));
        let old = MemberFacts { joined_at: Some(NOW - 700), ..member.clone() };
        assert_eq!(participation(&guild(3), Some(&old), &a, false, None, NOW), None);
        // Very high: a phone, and nothing else stands in for it but a role or an exemption.
        assert_eq!(participation(&guild(4), Some(&old), &a, false, None, NOW), Some(Restriction::PhoneRequired));
        a.phone_verified = Some(true);
        assert_eq!(participation(&guild(4), Some(&old), &a, false, None, NOW), None);
    }

    #[test]
    fn the_owner_a_role_and_an_exemption_are_not_held_to_the_levels() {
        let mut a = account();
        a.phone_verified = Some(false);
        let plain = MemberFacts { pending: Some(false), flags: Some(0), joined_at: Some(NOW - 5), ..Default::default() };
        assert!(participation(&guild(4), Some(&plain), &a, false, None, NOW).is_some());
        let owner = GuildFacts { owner: true, ..guild(4) };
        assert_eq!(participation(&owner, Some(&plain), &a, false, None, NOW), None);
        let roled = MemberFacts { has_roles: true, ..plain.clone() };
        assert_eq!(participation(&guild(4), Some(&roled), &a, false, None, NOW), None);
        let exempt = MemberFacts { flags: Some(1 << 2), ..plain.clone() };
        assert_eq!(participation(&guild(4), Some(&exempt), &a, false, None, NOW), None);
        assert_eq!(participation(&guild(4), Some(&plain), &a, true, None, NOW), None, "every permission");
    }

    #[test]
    fn screening_onboarding_and_a_timeout_come_before_the_levels() {
        let a = account();
        let pending = MemberFacts { pending: Some(true), ..Default::default() };
        assert_eq!(participation(&guild(0), Some(&pending), &a, false, None, NOW), Some(Restriction::Screening));
        let member = MemberFacts { pending: Some(false), flags: Some(0), ..Default::default() };
        assert_eq!(participation(&guild(0), Some(&member), &a, false, Some(true), NOW), Some(Restriction::Onboarding));
        assert_eq!(participation(&guild(0), Some(&member), &a, false, Some(false), NOW), None);
        let done = MemberFacts { flags: Some(1 << 1), ..member.clone() };
        assert_eq!(participation(&guild(0), Some(&done), &a, false, Some(true), NOW), None);
        let timed = MemberFacts { timed_out_until: Some(NOW + 300), ..member };
        assert_eq!(participation(&guild(0), Some(&timed), &a, true, None, NOW), Some(Restriction::TimedOut(300)), "even with every permission");
        let over = MemberFacts { timed_out_until: Some(NOW - 5), ..timed };
        assert_eq!(participation(&guild(0), Some(&over), &a, false, None, NOW), None);
    }

    #[test]
    fn what_is_not_known_is_not_a_block() {
        assert_eq!(participation(&guild(4), None, &account(), false, None, NOW), None);
        let unknown = AccountFacts { email_verified: None, phone_verified: None, created_at: 0 };
        let member = MemberFacts { pending: Some(false), flags: Some(0), joined_at: Some(NOW - 5), ..Default::default() };
        assert_eq!(participation(&guild(4), Some(&member), &unknown, false, None, NOW), None);
        assert_eq!(participation(&guild(1), Some(&member), &unknown, false, None, NOW), None);
    }

    #[test]
    fn a_member_of_under_a_week_wears_the_badge_until_the_week_is_up() {
        let day = 24 * 60 * 60;
        let member = |ago: i64| MemberFacts { joined_at: Some(NOW - ago), ..Default::default() };
        assert_eq!(new_member(&guild(0), Some(&member(60)), None, NOW), Some(Restriction::NewMember(7 * day - 60)));
        assert_eq!(new_member(&guild(0), Some(&member(6 * day)), None, NOW), Some(Restriction::NewMember(day)));
        assert_eq!(new_member(&guild(0), Some(&member(7 * day)), None, NOW), None, "gone at a week");
        assert_eq!(new_member(&guild(0), Some(&member(90 * day)), None, NOW), None);
    }

    #[test]
    fn the_badge_is_not_worn_by_the_owner_or_by_a_member_nothing_is_known_of() {
        let fresh = MemberFacts { joined_at: Some(NOW - 5), ..Default::default() };
        assert_eq!(new_member(&GuildFacts { owner: true, ..guild(0) }, Some(&fresh), None, NOW), None);
        assert_eq!(new_member(&guild(0), None, None, NOW), None, "not known is not blocked");
        assert_eq!(new_member(&guild(0), Some(&MemberFacts::default()), None, NOW), None);
        // The server's own join time stands in for a member object that does not carry one.
        assert!(new_member(&guild(0), Some(&MemberFacts::default()), Some(NOW - 5), NOW).is_some());
        assert_eq!(new_member(&guild(0), Some(&MemberFacts { joined_at: Some(NOW + 500), ..Default::default() }), None, NOW), None, "a join in the future is a wrong clock");
    }

    #[test]
    fn the_badge_says_how_long_in_days_and_is_a_lock_over_the_box() {
        let day = 24 * 60 * 60;
        assert!(Restriction::NewMember(3 * day).sentence().contains("3 days"));
        assert!(Restriction::NewMember(5 * 3600).sentence().contains("5 hours"));
        assert!(Restriction::NewMember(600).sentence().contains("an hour"));
        assert!(Restriction::NewMember(day).banner().is_some(), "it lasts days, so it is a lock and not a thing said when a send is tried");
    }

    #[test]
    fn slowmode_lets_the_account_speak_when_it_is_over() {
        let t = Instant::now();
        let s = Duration::from_secs;
        assert_eq!(slowmode_remaining(0, Some(t), false, t), None);
        assert_eq!(slowmode_remaining(10, None, false, t), None, "nothing said yet");
        assert_eq!(slowmode_remaining(10, Some(t), false, t + s(3)), Some(s(7)));
        assert_eq!(slowmode_remaining(10, Some(t), false, t + s(10)), None);
        assert_eq!(slowmode_remaining(10, Some(t), true, t + s(3)), None, "manage messages is not held to it");
    }

    #[test]
    fn the_sentences_say_how_long_to_wait() {
        assert!(Restriction::AccountTooNew(200).sentence().contains("4 more minutes"));
        assert!(Restriction::MemberTooNew(30).sentence().contains("30 more seconds"));
        assert!(Restriction::TimedOut(1).sentence().contains("1 more seconds") || Restriction::TimedOut(1).sentence().contains("wait"));
        assert!(Restriction::Onboarding.banner().is_some() && Restriction::AccountTooNew(5).banner().is_none());
    }
}
