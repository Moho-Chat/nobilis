//! Who people and groups are called, and the conversations they make.
//!
//! Signal knows a person by an ACI (a UUID) and says nothing of a name: the name is whatever the phone's address book
//! calls them (the contact sync), or what they call themselves (their profile, which needs their profile key), or, when
//! neither is known, a number or a stub of the id. A group has a title in its state.

use super::ids;
use super::store::SignalStore;
use crate::model::Buffer;
use crate::state::AppState;
use presage::{
    libsignal_service::protocol::{Aci, ServiceId},
    store::{ContentsStore, Thread},
};
use uuid::Uuid;

/// What the inbox and the sending code need to look things up: the account, the daemon, and the account's store.
pub struct Ctx<'a> {
    pub state: &'a AppState,
    pub account_id: &'a str,
    pub store: SignalStore,
    pub own: Uuid,
}

impl<'a> Ctx<'a> {
    pub fn new(state: &'a AppState, account_id: &'a str, store: SignalStore, own: Uuid) -> Self {
        Self { state, account_id, store, own }
    }

    pub fn is_own(&self, who: &Uuid) -> bool {
        *who == self.own
    }

    /// What this account's own messages are signed with: the name the daemon was told at connect.
    pub fn own_name(&self) -> String {
        self.state.runtime.own_identity(self.account_id).unwrap_or_else(|| "Me".to_string())
    }
}

/// The last four characters of an id, enough to tell two unnamed people apart.
fn stub(id: &Uuid) -> String {
    let text = id.simple().to_string();
    text[text.len() - 4..].to_string()
}

/// A person's name for a conversation or a message.
pub async fn person(ctx: &Ctx<'_>, who: &Uuid) -> String {
    if ctx.is_own(who) {
        return ctx.own_name();
    }
    // A nickname given in Signal, or the name the phone's address book has, as the storage service says.
    if let Some(name) = super::storage::current(ctx.account_id, &ctx.store).standing(&format!("p:{who}")).and_then(|s| s.name.clone()) {
        return name;
    }
    let id = ServiceId::Aci(Aci::from(*who));
    if let Ok(Some(contact)) = ctx.store.contact_by_id(&id).await {
        if !contact.name.trim().is_empty() {
            return contact.name;
        }
        if let Ok(Some(key)) = ctx.store.profile_key(&id).await {
            if let Ok(Some(profile)) = ctx.store.profile(*who, key).await {
                if let Some(name) = profile.name {
                    return name.to_string();
                }
            }
        }
        if let Some(number) = contact.phone_number {
            return number.to_string();
        }
    } else if let Ok(Some(key)) = ctx.store.profile_key(&id).await {
        if let Ok(Some(profile)) = ctx.store.profile(*who, key).await {
            if let Some(name) = profile.name {
                return name.to_string();
            }
        }
    }
    format!("Signal user ·{}", stub(who))
}

/// Whether the account knows what to call this person: a contact name, or a profile it has fetched.
pub async fn has_name(ctx: &Ctx<'_>, who: &Uuid) -> bool {
    if ctx.is_own(who) {
        return true;
    }
    if super::storage::current(ctx.account_id, &ctx.store).standing(&format!("p:{who}")).is_some_and(|s| s.name.is_some()) {
        return true;
    }
    let id = ServiceId::Aci(Aci::from(*who));
    if let Ok(Some(contact)) = ctx.store.contact_by_id(&id).await {
        if !contact.name.trim().is_empty() {
            return true;
        }
    }
    if let Ok(Some(key)) = ctx.store.profile_key(&id).await {
        if let Ok(Some(profile)) = ctx.store.profile(*who, key).await {
            return profile.name.is_some();
        }
    }
    false
}

/// A group's title.
pub async fn group_title(ctx: &Ctx<'_>, key: &[u8; 32]) -> String {
    match ctx.store.group(*key).await {
        Ok(Some(group)) if !group.title.trim().is_empty() => group.title,
        _ => format!("Group ·{}", &ids::hex(key)[..4]),
    }
}

/// A name for a buffer that no other conversation of the account has. One of the name that is no conversation's (no
/// remote id: left so by an older moho's rename, or put back from a layout) is not another conversation's, and is taken
/// over rather than worked around - otherwise a group renamed from moho came back after a restart as a second
/// conversation, "Name ·abcd", beside its own history.
pub fn unique_name(taken: &[(String, Option<String>)], remote_id: &str, title: &str) -> String {
    let used = |name: &str| taken.iter().any(|(n, r)| n == name && r.as_deref().is_some_and(|r| r != remote_id));
    if !used(title) {
        return title.to_string();
    }
    let tail: String = remote_id.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{title} ·{tail}")
}

/// The conversation's buffer, made if it is not there yet.
pub async fn buffer_for(ctx: &Ctx<'_>, thread: &Thread) -> Buffer {
    let remote = ids::remote_id(thread);
    let mine: Vec<Buffer> = ctx.state.runtime.list_buffers().into_iter().filter(|b| b.account_id == ctx.account_id).collect();
    if let Some(found) = mine.iter().find(|b| b.remote_id.as_deref() == Some(remote.as_str())) {
        // Through ensure_buffer, which confirms one put back from the last run's layout: found here without it, a
        // restored conversation was taken away again half a minute after connecting, as one the service no longer has.
        return ctx.state.runtime.ensure_buffer(ctx.state, ctx.account_id, &found.name, &found.kind);
    }
    let (title, kind) = match thread {
        Thread::Contact(id) if ids::uuid_of(id) == ctx.own => ("Note to Self".to_string(), "dm"),
        Thread::Contact(id) => (person(ctx, &ids::uuid_of(id)).await, "dm"),
        Thread::Group(key) => (group_title(ctx, key).await, "channel"),
    };
    let taken: Vec<(String, Option<String>)> = mine.iter().map(|b| (b.name.clone(), b.remote_id.clone())).collect();
    let name = unique_name(&taken, &remote, &title);
    let buffer = ctx.state.runtime.ensure_buffer(ctx.state, ctx.account_id, &name, kind);
    ctx.state.runtime.set_buffer_remote_id(ctx.state, &buffer.id, &remote);
    ctx.state.runtime.get_buffer(&buffer.id).unwrap_or(buffer)
}

/// Words for a disappearing-message timer.
pub fn timer_words(seconds: u32) -> String {
    let (n, unit) = match seconds {
        0 => return "off".to_string(),
        s if s % 604_800 == 0 => (s / 604_800, "week"),
        s if s % 86_400 == 0 => (s / 86_400, "day"),
        s if s % 3_600 == 0 => (s / 3_600, "hour"),
        s if s % 60 == 0 => (s / 60, "minute"),
        s => (s, "second"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timer_is_said_in_its_largest_whole_unit() {
        assert_eq!(timer_words(0), "off");
        assert_eq!(timer_words(30), "30 seconds");
        assert_eq!(timer_words(1), "1 second");
        assert_eq!(timer_words(60), "1 minute");
        assert_eq!(timer_words(3600), "1 hour");
        assert_eq!(timer_words(86_400), "1 day");
        assert_eq!(timer_words(7 * 86_400), "1 week");
        assert_eq!(timer_words(14 * 86_400), "2 weeks");
        assert_eq!(timer_words(90), "90 seconds");
    }

    #[test]
    fn two_conversations_with_one_name_are_told_apart() {
        let taken = vec![("Sam".to_string(), Some("p:aaaa1111".to_string()))];
        assert_eq!(unique_name(&taken, "p:aaaa1111", "Sam"), "Sam");
        assert_eq!(unique_name(&taken, "p:bbbb2222", "Sam"), "Sam ·2222");
        assert_eq!(unique_name(&taken, "p:bbbb2222", "Alex"), "Alex");
        // One of the name that is no conversation's is taken over, not worked around (seen on the first real account: a
        // renamed group came back as "Name ·abcd" beside its history).
        let unlinked = vec![("Book club".to_string(), None)];
        assert_eq!(unique_name(&unlinked, "g:cccc3333", "Book club"), "Book club");
    }
}
