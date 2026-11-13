//! What happened to a group, in words: a group message that changes the group carries the change (encrypted with the
//! group's key), and Signal's clients draw each part of it as a line - who joined, who was added or removed, a new
//! name, a new timer, who may do what. The library decrypts it; this says it.

use super::ids;
use super::people::{self, Ctx};
use presage::libsignal_service::{
    groups_v2::{AccessRequired, GroupChange, GroupChanges, GroupOperations, Role},
    proto::GroupContextV2,
    protocol::ServiceId,
    zkgroup::groups::{GroupMasterKey, GroupSecretParams},
};
use std::collections::HashMap;
use uuid::Uuid;

/// The change a group message carries, decrypted; `None` when it carries none (an ordinary message) or it cannot be
/// read.
pub fn change_of(context: &GroupContextV2) -> Option<GroupChanges> {
    let key: [u8; 32] = context.master_key.as_deref()?.try_into().ok()?;
    let bytes = context.group_change.as_deref()?;
    let encrypted: presage::libsignal_service::proto::GroupChange = prost::Message::decode(bytes).ok()?;
    let params = GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key));
    match GroupOperations::new(params).decrypt_group_change(encrypted) {
        Ok(changes) => Some(changes),
        Err(e) => {
            tracing::debug!("signal: a group change could not be read: {e:?}");
            None
        }
    }
}

fn who_may(access: &AccessRequired) -> &'static str {
    match access {
        AccessRequired::Administrator => "only admins",
        AccessRequired::Any | AccessRequired::Member => "all members",
        AccessRequired::Unsatisfiable => "nobody",
        AccessRequired::Unknown => "somebody",
    }
}

/// Every person a change names, so their names can be looked up before the words are made.
fn people_in(changes: &GroupChanges) -> Vec<Uuid> {
    let mut out = vec![Uuid::from(changes.editor)];
    let mut add = |id: &ServiceId| {
        if let ServiceId::Aci(aci) = id {
            out.push(Uuid::from(*aci));
        }
    };
    for change in &changes.changes {
        match change {
            GroupChange::NewMember(m) => add(&ServiceId::Aci(m.aci)),
            GroupChange::DeleteMember(aci) | GroupChange::DeleteRequestingMember(aci) => add(&ServiceId::Aci(*aci)),
            GroupChange::ModifyMemberRole { aci, .. } | GroupChange::PromoteRequestingMember { aci, .. } => add(&ServiceId::Aci(*aci)),
            GroupChange::NewPendingMember(p) => add(&p.address),
            GroupChange::DeletePendingMember(id) | GroupChange::PromotePendingMember { address: id, .. } => add(id),
            GroupChange::NewRequestingMember(r) => add(&ServiceId::Aci(r.aci)),
            _ => {}
        }
    }
    out
}

/// The lines a change makes. `name` says what a person is called; an id it cannot name (an invitation sent to a
/// number's id) is "somebody".
pub fn lines(changes: &GroupChanges, name: &dyn Fn(&ServiceId) -> String) -> Vec<String> {
    let editor_id = ServiceId::Aci(changes.editor);
    let editor = name(&editor_id);
    let is_editor = |id: &ServiceId| *id == editor_id;
    let mut out = Vec::new();
    for change in &changes.changes {
        let line = match change {
            GroupChange::NewMember(m) => {
                let id = ServiceId::Aci(m.aci);
                if is_editor(&id) {
                    format!("{} joined the group", name(&id))
                } else {
                    format!("{editor} added {}", name(&id))
                }
            }
            GroupChange::DeleteMember(aci) => {
                let id = ServiceId::Aci(*aci);
                if is_editor(&id) {
                    format!("{} left the group", name(&id))
                } else {
                    format!("{editor} removed {}", name(&id))
                }
            }
            GroupChange::ModifyMemberRole { aci, role } => match role {
                Role::Administrator => format!("{editor} made {} an admin", name(&ServiceId::Aci(*aci))),
                _ => format!("{editor} removed {} as an admin", name(&ServiceId::Aci(*aci))),
            },
            GroupChange::NewPendingMember(p) => format!("{editor} invited {}", name(&p.address)),
            GroupChange::DeletePendingMember(id) => {
                if is_editor(id) {
                    format!("{} declined the invitation to the group", name(id))
                } else {
                    format!("{editor} took back an invitation to {}", name(id))
                }
            }
            GroupChange::PromotePendingMember { address, .. } => format!("{} accepted the invitation and joined", name(address)),
            GroupChange::PromotePendingPniAciMemberProfileKey(_) => format!("{editor} accepted the invitation and joined"),
            GroupChange::Title(title) => format!("{editor} changed the group name to “{}”", title.trim()),
            GroupChange::Avatar(path) if path.is_empty() => format!("{editor} removed the group picture"),
            GroupChange::Avatar(_) => format!("{editor} changed the group picture"),
            GroupChange::Timer(timer) => match timer.as_ref().map(|t| t.duration).unwrap_or(0) {
                0 => format!("{editor} turned off disappearing messages"),
                seconds => format!("{editor} set disappearing messages to {}", people::timer_words(seconds)),
            },
            GroupChange::AttributeAccess(access) => format!("{editor} changed who can edit the group's details to {}", who_may(access)),
            GroupChange::MemberAccess(access) => format!("{editor} changed who can add members to {}", who_may(access)),
            GroupChange::InviteLinkAccess(access) => match access {
                AccessRequired::Any => format!("{editor} turned on the group link"),
                AccessRequired::Administrator => format!("{editor} turned on the group link, with an admin approving who joins"),
                _ => format!("{editor} turned off the group link"),
            },
            GroupChange::InviteLinkPassword(_) => format!("{editor} reset the group link"),
            GroupChange::NewRequestingMember(r) => format!("{} asked to join through the group link", name(&ServiceId::Aci(r.aci))),
            GroupChange::DeleteRequestingMember(aci) => {
                let id = ServiceId::Aci(*aci);
                if is_editor(&id) {
                    format!("{} took back their request to join", name(&id))
                } else {
                    format!("{editor} turned down {}'s request to join", name(&id))
                }
            }
            GroupChange::PromoteRequestingMember { aci, .. } => format!("{editor} let {} in", name(&ServiceId::Aci(*aci))),
            GroupChange::Description(Some(text)) if !text.trim().is_empty() => format!("{editor} changed the group description"),
            GroupChange::Description(_) => format!("{editor} removed the group description"),
            GroupChange::AnnouncementOnly(true) => format!("{editor} changed the group so only admins can send messages"),
            GroupChange::AnnouncementOnly(false) => format!("{editor} changed the group so all members can send messages"),
            GroupChange::AddBannedMember(b) => format!("{editor} banned {} from the group", name(&b.user_id)),
            GroupChange::DeleteBannedMember(id) => format!("{editor} lifted the ban on {}", name(id)),
            GroupChange::TerminateGroup => format!("{editor} ended the group"),
            // Profile keys, labels and their rights are housekeeping, not news.
            _ => continue,
        };
        out.push(line);
    }
    out
}

/// The lines a group message's change makes, with everybody named.
pub async fn lines_for(ctx: &Ctx<'_>, changes: &GroupChanges) -> Vec<String> {
    let mut names: HashMap<Uuid, String> = HashMap::new();
    for who in people_in(changes) {
        if let std::collections::hash_map::Entry::Vacant(slot) = names.entry(who) {
            slot.insert(people::person(ctx, &who).await);
        }
    }
    lines(changes, &|id| match id {
        ServiceId::Aci(_) => names.get(&ids::uuid_of(id)).cloned().unwrap_or_else(|| "somebody".to_string()),
        _ => "somebody".to_string(),
    })
}

/// The window's member list for a group: its members, admins first by their mark, and those invited and not yet in.
/// Signal has no presence, so nobody is drawn online or away.
pub async fn publish_members(ctx: &Ctx<'_>, key: &[u8; 32]) {
    use presage::store::ContentsStore;
    let Ok(Some(group)) = ctx.store.group(*key).await else { return };
    let remote = format!("g:{}", ids::hex(key));
    let Some(buffer) = ctx.state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id && b.remote_id.as_deref() == Some(remote.as_str())) else { return };
    let mut list = Vec::new();
    for member in &group.members {
        let who = Uuid::from(member.aci);
        let admin = matches!(member.role, Role::Administrator);
        list.push(serde_json::json!({
            "nick": people::person(ctx, &who).await,
            "userId": who.to_string(),
            "prefix": if admin { "admin" } else { "" },
            "away": false,
            "bot": false,
            "relationship": "",
            "avatarUrl": super::media::avatar_of(ctx.account_id, &who),
            "roles": [if admin { "admin" } else { "member" }],
        }));
    }
    for pending in &group.pending_members {
        // One invited by number (their PNI) has no name this account can know.
        if !matches!(pending.service_id_type, presage::model::ServiceIdType::AccountIdentity) {
            continue;
        }
        let who = pending.uuid;
        list.push(serde_json::json!({
            "nick": people::person(ctx, &who).await,
            "userId": who.to_string(),
            "prefix": "invited",
            "away": false,
            "bot": false,
            "relationship": "",
            "avatarUrl": super::media::avatar_of(ctx.account_id, &who),
            "roles": ["invited"],
        }));
    }
    let total = group.members.len();
    let list = serde_json::Value::Array(list);
    ctx.state.runtime.set_presence(&buffer.id, list.clone());
    ctx.state.runtime.set_presence_total(&buffer.id, total);
    ctx.state.events.emit("presenceChange", serde_json::json!({ "bufferId": buffer.id, "members": list, "total": total }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::groups_v2::{Member, Timer};
    use presage::libsignal_service::protocol::Aci;
    use presage::libsignal_service::zkgroup::profiles::ProfileKey;

    fn aci(n: u8) -> Aci {
        Aci::from(Uuid::from_bytes([n; 16]))
    }

    fn changes(editor: u8, list: Vec<GroupChange>) -> GroupChanges {
        GroupChanges { group_id: [0u8; 32], editor: aci(editor), version: 2, changes: list, change_epoch: 0 }
    }

    fn name(id: &ServiceId) -> String {
        match id {
            ServiceId::Aci(a) if *a == aci(1) => "Sam".into(),
            ServiceId::Aci(a) if *a == aci(2) => "Alex".into(),
            _ => "somebody".into(),
        }
    }

    fn member(n: u8) -> Member {
        Member { aci: aci(n), role: Role::Default, profile_key: ProfileKey::create([0u8; 32]), joined_at_version: 2, label: None, label_emoji: None }
    }

    #[test]
    fn joining_and_being_added_leaving_and_being_removed_are_told_apart() {
        assert_eq!(lines(&changes(1, vec![GroupChange::NewMember(member(2))]), &name), vec!["Sam added Alex"]);
        assert_eq!(lines(&changes(2, vec![GroupChange::NewMember(member(2))]), &name), vec!["Alex joined the group"]);
        assert_eq!(lines(&changes(2, vec![GroupChange::DeleteMember(aci(2))]), &name), vec!["Alex left the group"]);
        assert_eq!(lines(&changes(1, vec![GroupChange::DeleteMember(aci(2))]), &name), vec!["Sam removed Alex"]);
    }

    #[test]
    fn a_name_a_timer_and_the_rules_are_said_and_housekeeping_is_not() {
        let said = lines(
            &changes(
                1,
                vec![
                    GroupChange::Title(" Book club ".into()),
                    GroupChange::Timer(Some(Timer { duration: 86_400 })),
                    GroupChange::ModifyMemberRole { aci: aci(2), role: Role::Administrator },
                    GroupChange::MemberAccess(AccessRequired::Administrator),
                    GroupChange::InviteLinkAccess(AccessRequired::Unsatisfiable),
                    GroupChange::AnnouncementOnly(true),
                    GroupChange::ModifyMemberProfileKey { aci: aci(2), profile_key: ProfileKey::create([1u8; 32]) },
                    GroupChange::Timer(None),
                ],
            ),
            &name,
        );
        assert_eq!(
            said,
            vec![
                "Sam changed the group name to “Book club”",
                "Sam set disappearing messages to 1 day",
                "Sam made Alex an admin",
                "Sam changed who can add members to only admins",
                "Sam turned off the group link",
                "Sam changed the group so only admins can send messages",
                "Sam turned off disappearing messages",
            ]
        );
    }

    #[test]
    fn a_message_with_no_change_or_an_unreadable_one_makes_no_lines() {
        assert!(change_of(&GroupContextV2 { master_key: Some(vec![1; 32]), revision: Some(3), group_change: None }).is_none());
        assert!(change_of(&GroupContextV2 { master_key: Some(vec![1; 32]), revision: Some(3), group_change: Some(vec![9, 9, 9]) }).is_none());
        assert!(change_of(&GroupContextV2 { master_key: Some(vec![1; 3]), revision: Some(3), group_change: Some(vec![]) }).is_none());
    }
}
