//! The limits an instance states, kept to before a request is made rather than learned from a refusal.
//!
//! Each is a question with a plain answer from what the account already holds - how many servers it is in, how many
//! requests it has outstanding, how many different reactions a message has - so none costs a request, and the
//! answer is said in words that name the number. Stoat still checks, and its own refusal still passes through.

use super::world::{Instance, UserLimits, World};
use super::Handle;

/// Whether one more server may be joined.
pub fn may_join(limits: &UserLimits, servers_held: usize) -> Result<(), String> {
    if servers_held >= limits.servers {
        return Err(format!("This account is in {servers_held} servers, which is the most Stoat lets it join ({}). Leave one first", limits.servers));
    }
    Ok(())
}

/// Whether one more friend request may be sent: those still waiting for an answer count.
pub fn may_ask(limits: &UserLimits, world: &World) -> Result<(), String> {
    let waiting = world.users.values().filter(|u| u.relationship == "Outgoing").count();
    if waiting >= limits.outgoing_friend_requests {
        return Err(format!("This account has {waiting} friend requests waiting for an answer, which is the most Stoat allows at once ({}). Wait for some to be answered", limits.outgoing_friend_requests));
    }
    Ok(())
}

/// Whether a reaction may be put on a message that already carries these (a new kind counts against the limit; one
/// that is already there does not).
pub fn may_react(instance: &Instance, already: &[String], emoji: &str) -> Result<(), String> {
    if !already.iter().any(|e| e == emoji) && already.len() >= instance.message_reactions {
        return Err(format!("A message may carry at most {} different reactions on this instance", instance.message_reactions));
    }
    Ok(())
}

/// Whether a file may be a picture of one kind.
pub fn may_use_picture(kind: &str, bytes: u64, limits: &UserLimits) -> Result<(), String> {
    let most = match kind {
        "avatar" => limits.avatar_bytes,
        "icon" => limits.icon_bytes,
        "banner" => limits.banner_bytes,
        _ => limits.emoji_bytes,
    };
    if bytes > most {
        return Err(format!("That picture is {} KB and Stoat takes {kind}s of at most {} KB", bytes / 1000, most / 1000));
    }
    Ok(())
}

/// What a call is held to for this account, said once on joining: nothing where it is as good as anybody's.
pub fn call_note(handle: &Handle) -> Option<String> {
    let (mine, best) = (handle.limits(), &handle.instance.standing);
    if !handle.is_new(chrono::Utc::now().timestamp_millis()) || mine.voice_quality >= best.voice_quality && mine.video == best.video {
        return None;
    }
    let mut parts = Vec::new();
    if mine.voice_quality < best.voice_quality {
        parts.push(format!("sound is limited to {} kHz", mine.voice_quality / 1000));
    }
    if !mine.video && best.video {
        parts.push("video is off".to_string());
    }
    Some(format!("While this account is new, {} (for the first {} hours).", parts.join(" and "), handle.instance.new_user_hours))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::world::User;

    fn limits() -> UserLimits {
        UserLimits { servers: 2, outgoing_friend_requests: 1, avatar_bytes: 4_000_000, ..Default::default() }
    }

    #[test]
    fn a_server_is_joined_only_below_the_limit() {
        assert!(may_join(&limits(), 1).is_ok());
        assert!(may_join(&limits(), 2).unwrap_err().contains("(2)"));
    }

    #[test]
    fn requests_waiting_count_against_asking() {
        let mut w = World::default();
        assert!(may_ask(&limits(), &w).is_ok());
        w.users.insert("U".into(), User { relationship: "Incoming".into(), ..Default::default() });
        assert!(may_ask(&limits(), &w).is_ok(), "one asked of us is not one of ours");
        w.users.insert("V".into(), User { relationship: "Outgoing".into(), ..Default::default() });
        assert!(may_ask(&limits(), &w).unwrap_err().contains("most Stoat allows"));
    }

    #[test]
    fn a_reaction_already_there_is_not_a_new_kind() {
        let instance = Instance { message_reactions: 2, ..Default::default() };
        let held = vec!["a".to_string(), "b".to_string()];
        assert!(may_react(&instance, &held, "a").is_ok());
        assert!(may_react(&instance, &held, "c").unwrap_err().contains("at most 2"));
        assert!(may_react(&instance, &held[..1], "c").is_ok());
    }

    #[test]
    fn a_picture_is_held_to_its_kind_s_size() {
        assert!(may_use_picture("avatar", 3_999_999, &limits()).is_ok());
        assert!(may_use_picture("avatar", 5_000_000, &limits()).unwrap_err().contains("at most 4000 KB"));
    }

    #[test]
    fn a_call_says_what_a_new_account_is_held_to_and_an_old_one_says_nothing() {
        let h = crate::backend::stoat::tests::bare_handle("stoat:ME");
        let instance = Instance { new_user_hours: 72, standing: UserLimits { voice_quality: 48_000, video: true, ..Default::default() }, fresh: UserLimits { voice_quality: 16_000, video: true, ..Default::default() }, ..Default::default() };
        let h = Handle { instance: std::sync::Arc::new(instance), ..h };
        h.world.lock().unwrap().own_id = format!("{:0>10}{}", "", "0".repeat(16));
        assert_eq!(call_note(&h), None, "an account made long ago");
        // An id made now.
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let head: String = (0..10).rev().map(|i| alphabet[((now >> (5 * i)) & 31) as usize] as char).collect();
        h.world.lock().unwrap().own_id = format!("{head}{}", "0".repeat(16));
        let note = call_note(&h).unwrap();
        assert!(note.contains("16 kHz") && note.contains("72 hours"), "{note}");
    }
}
