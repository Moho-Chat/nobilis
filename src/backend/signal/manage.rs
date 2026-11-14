//! Changing a group: its name, description, picture-less settings, timer, members, admins, link and rules, and leaving.
//!
//! Each edit is turned into the group server's change actions here, from the group as the account knows it; the
//! library (patched presage) sends them, saves the new state and tells the members. What Signal would refuse is said
//! before asking: a non-admin changing what only admins may, the last admin leaving a group that has others in it.

use presage::libsignal_service::{
    groups_v2::{GroupOperations, Role, Timer},
    proto::{access_control::AccessRequired, group_change::actions, group_change::Actions, member::Role as WireRole},
    protocol::Aci,
};
use presage::model::groups::Group;
use uuid::Uuid;

/// Who may do something in a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    Everyone,
    Admins,
}

impl Who {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "all" | "members" | "everyone" => Some(Self::Everyone),
            "admins" => Some(Self::Admins),
            _ => None,
        }
    }

    fn wire(self) -> i32 {
        match self {
            Self::Everyone => AccessRequired::Member as i32,
            Self::Admins => AccessRequired::Administrator as i32,
        }
    }
}

/// What the group link does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Off,
    On,
    /// On, with an admin approving each person who asks to join.
    Approval,
}

/// One change to a group.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    Rename(String),
    Describe(String),
    /// The group's picture: the CDN key it was uploaded under (`upload_group_avatar`), or empty to take it away.
    Picture(String),
    Timer(u32),
    /// People to put in the group; each is added directly when the server will take it, invited otherwise. The
    /// credentials are fetched before building (see `commands`), `None` meaning "invite".
    Add(Vec<(Uuid, bool)>),
    Remove(Uuid),
    Admin(Uuid, bool),
    /// Takes back an invitation not yet accepted.
    Uninvite(Uuid),
    /// Lets in, or turns down, somebody who asked through the link.
    Approve(Uuid, bool),
    Leave,
    Link(Link),
    ResetLink,
    DetailsRights(Who),
    AddRights(Who),
    AnnouncementsOnly(bool),
}

fn is_admin(group: &Group, who: &Aci) -> bool {
    group.members.iter().any(|m| m.aci == *who && matches!(m.role, Role::Administrator))
}

fn wire_role(admin: bool) -> i32 {
    if admin {
        WireRole::Administrator as i32
    } else {
        WireRole::Default as i32
    }
}

/// Whether the account may make this edit at all, in words when it may not: Signal's server would refuse it, and a
/// refusal in a person's words is better than one in the server's.
pub fn allowed(group: &Group, own: &Aci, edit: &Edit) -> Result<(), String> {
    if !group.is_member(own) {
        return Err("This account is not in that group".to_string());
    }
    let admin = is_admin(group, own);
    let attributes_by_admins = group.access_control.as_ref().is_some_and(|a| matches!(a.attributes, presage::libsignal_service::groups_v2::AccessRequired::Administrator));
    let members_by_admins = group.access_control.as_ref().is_some_and(|a| matches!(a.members, presage::libsignal_service::groups_v2::AccessRequired::Administrator));
    match edit {
        Edit::Rename(_) | Edit::Describe(_) | Edit::Picture(_) | Edit::Timer(_) if attributes_by_admins && !admin => Err("Only an admin can change this group's details".to_string()),
        Edit::Add(_) if members_by_admins && !admin => Err("Only an admin can add people to this group".to_string()),
        Edit::Remove(_) | Edit::Admin(..) | Edit::Approve(..) | Edit::Link(_) | Edit::ResetLink | Edit::DetailsRights(_) | Edit::AddRights(_) | Edit::AnnouncementsOnly(_) if !admin => {
            Err("Only an admin can do that".to_string())
        }
        Edit::Uninvite(who) => {
            let mine = group.pending_members.iter().any(|p| p.uuid == *who && p.added_by_aci == *own);
            if admin || mine {
                Ok(())
            } else {
                Err("Only an admin, or whoever sent it, can take back an invitation".to_string())
            }
        }
        Edit::Leave => {
            let others = group.members.iter().filter(|m| m.aci != *own).count();
            let other_admins = group.members.iter().filter(|m| m.aci != *own && matches!(m.role, Role::Administrator)).count();
            if admin && others > 0 && other_admins == 0 {
                Err("You are this group's only admin: make somebody else an admin before leaving".to_string())
            } else {
                Ok(())
            }
        }
        Edit::Rename(title) if title.trim().is_empty() => Err("A group needs a name".to_string()),
        Edit::Remove(who) if Aci::from(*who) == *own => Err("Leave the group instead".to_string()),
        _ => Ok(()),
    }
}

/// The change's actions. `credentials` are the members' presentations for `Edit::Add` (made by `commands`).
pub fn actions(operations: &GroupOperations, group: &Group, own: &Aci, edit: &Edit, added: Vec<actions::AddMemberAction>) -> Result<Actions, String> {
    let fail = |e: presage::libsignal_service::groups_v2::GroupDecodingError| format!("the change could not be made: {e:?}");
    let mut change = Actions::default();
    let mut rng = rand09::rng();
    match edit {
        Edit::Rename(title) => {
            change.modify_title = Some(actions::ModifyTitleAction { title: operations.encrypt_title(title.trim(), &mut rng) });
        }
        Edit::Picture(key) => change.modify_avatar = Some(actions::ModifyAvatarAction { avatar: key.clone() }),
        Edit::Describe(text) => {
            change.modify_description = Some(actions::ModifyDescriptionAction { description: operations.encrypt_description(Some(text.trim()).filter(|t| !t.is_empty()), &mut rng) });
        }
        Edit::Timer(seconds) => {
            let timer = Timer { duration: *seconds };
            change.modify_disappearing_message_timer = Some(actions::ModifyDisappearingMessageTimerAction { timer: operations.encrypt_disappearing_messages_timer(Some(&timer).filter(|t| t.duration > 0), &mut rng) });
        }
        Edit::Add(people) => {
            change.add_members = added;
            for (who, direct) in people {
                if *direct {
                    continue;
                }
                let invite = operations.build_add_pending_member_action(Aci::from(*who).into(), *own, Role::Default).map_err(fail)?;
                change.add_members_pending_profile_key.push(invite);
            }
        }
        Edit::Remove(who) => change.delete_members.push(operations.build_remove_member_action(Aci::from(*who)).map_err(fail)?),
        Edit::Leave => change.delete_members.push(operations.build_remove_member_action(*own).map_err(fail)?),
        Edit::Admin(who, admin) => change.modify_member_roles.push(actions::ModifyMemberRoleAction { user_id: operations.encrypt_aci(Aci::from(*who)).map_err(fail)?, role: wire_role(*admin) }),
        Edit::Uninvite(who) => change.delete_members_pending_profile_key.push(operations.build_remove_pending_member_action(Aci::from(*who).into()).map_err(fail)?),
        Edit::Approve(who, true) => change.promote_members_pending_admin_approval.push(actions::PromoteMemberPendingAdminApprovalAction { user_id: operations.encrypt_aci(Aci::from(*who)).map_err(fail)?, role: wire_role(false) }),
        Edit::Approve(who, false) => change.delete_members_pending_admin_approval.push(actions::DeleteMemberPendingAdminApprovalAction { deleted_user_id: operations.encrypt_aci(Aci::from(*who)).map_err(fail)? }),
        Edit::Link(link) => {
            let access = match link {
                Link::Off => AccessRequired::Unsatisfiable,
                Link::On => AccessRequired::Any,
                Link::Approval => AccessRequired::Administrator,
            };
            change.modify_add_from_invite_link_access = Some(actions::ModifyAddFromInviteLinkAccessControlAction { add_from_invite_link_access: access as i32 });
            // A link turned on for the first time needs a password; one that had one keeps it.
            if *link != Link::Off && group.invite_link_password.is_empty() {
                change.modify_invite_link_password = Some(actions::ModifyInviteLinkPasswordAction { invite_link_password: rand::random::<[u8; 16]>().to_vec() });
            }
        }
        Edit::ResetLink => change.modify_invite_link_password = Some(actions::ModifyInviteLinkPasswordAction { invite_link_password: rand::random::<[u8; 16]>().to_vec() }),
        Edit::DetailsRights(who) => change.modify_attributes_access = Some(actions::ModifyAttributesAccessControlAction { attributes_access: who.wire() }),
        Edit::AddRights(who) => change.modify_member_access = Some(actions::ModifyMembersAccessControlAction { members_access: who.wire() }),
        Edit::AnnouncementsOnly(on) => change.modify_announcements_only = Some(actions::ModifyAnnouncementsOnlyAction { announcements_only: *on }),
    }
    Ok(change)
}

/// A group link, as Signal's clients write it: `https://signal.group/#<base64url of GroupInviteLink>`.
pub fn link_of(master_key: &[u8; 32], password: &[u8]) -> String {
    use base64::Engine;
    use presage::libsignal_service::{prelude::ProtobufMessage, proto::group_invite_link};
    let contents = group_invite_link::GroupInviteLinkContentsV1 { group_master_key: master_key.to_vec(), invite_link_password: password.to_vec() };
    let link = presage::libsignal_service::proto::GroupInviteLink { contents: Some(group_invite_link::Contents::ContentsV1(contents)) };
    format!("https://signal.group/#{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(link.encode_to_vec()))
}

/// The group's key and the link's password from a group link (`https://signal.group/#...` or `sgnl://signal.group/#...`).
pub fn parse_link(link: &str) -> Option<([u8; 32], Vec<u8>)> {
    use base64::Engine;
    use presage::libsignal_service::{prelude::ProtobufMessage, proto::group_invite_link};
    let fragment = link.trim().split_once('#')?.1;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(fragment.trim_end_matches('=')).ok()?;
    let parsed = presage::libsignal_service::proto::GroupInviteLink::decode(bytes.as_slice()).ok()?;
    match parsed.contents? {
        group_invite_link::Contents::ContentsV1(v1) => Some((v1.group_master_key.try_into().ok()?, v1.invite_link_password)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::model::groups::Member;
    use presage::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};
    use presage::libsignal_service::zkgroup::profiles::ProfileKey;

    fn aci(n: u8) -> Aci {
        Aci::from(Uuid::from_bytes([n; 16]))
    }

    fn member(n: u8, admin: bool) -> Member {
        Member { aci: aci(n), role: if admin { Role::Administrator } else { Role::Default }, profile_key: ProfileKey::create([n; 32]), joined_at_revision: 0, label: None, label_emoji: None }
    }

    fn group(members: Vec<Member>) -> Group {
        Group { title: "G".into(), avatar: String::new(), disappearing_messages_timer: None, access_control: None, revision: 4, members, pending_members: Vec::new(), requesting_members: Vec::new(), invite_link_password: Vec::new(), description: None }
    }

    fn operations() -> GroupOperations {
        GroupOperations::new(GroupSecretParams::derive_from_master_key(GroupMasterKey::new([9u8; 32])))
    }

    #[test]
    fn what_only_an_admin_may_do_is_refused_in_words_first() {
        let g = group(vec![member(1, true), member(2, false)]);
        assert!(allowed(&g, &aci(1), &Edit::Remove(Uuid::from(aci(2)))).is_ok());
        assert_eq!(allowed(&g, &aci(2), &Edit::Remove(Uuid::from(aci(1)))).unwrap_err(), "Only an admin can do that");
        assert!(allowed(&g, &aci(2), &Edit::Rename("New".into())).is_ok(), "anybody may, unless the group says admins");
        assert!(allowed(&g, &aci(3), &Edit::Rename("New".into())).is_err(), "not in the group");
        assert!(allowed(&g, &aci(1), &Edit::Leave).unwrap_err().contains("only admin"));
        assert!(allowed(&g, &aci(2), &Edit::Leave).is_ok());
        assert!(allowed(&group(vec![member(1, true)]), &aci(1), &Edit::Leave).is_ok(), "alone, the last admin may go");
        assert!(allowed(&g, &aci(1), &Edit::Rename("  ".into())).is_err());
    }

    #[test]
    fn edits_become_the_servers_actions() {
        let ops = operations();
        let g = group(vec![member(1, true), member(2, false)]);
        let rename = actions(&ops, &g, &aci(1), &Edit::Rename("Book club".into()), Vec::new()).unwrap();
        assert_eq!(ops.decrypt_title(&rename.modify_title.unwrap().title), "Book club");
        let admin = actions(&ops, &g, &aci(1), &Edit::Admin(Uuid::from(aci(2)), true), Vec::new()).unwrap();
        assert_eq!(admin.modify_member_roles[0].role, WireRole::Administrator as i32);
        let leave = actions(&ops, &g, &aci(2), &Edit::Leave, Vec::new()).unwrap();
        assert_eq!(leave.delete_members.len(), 1);
        let link = actions(&ops, &g, &aci(1), &Edit::Link(Link::Approval), Vec::new()).unwrap();
        assert_eq!(link.modify_add_from_invite_link_access.unwrap().add_from_invite_link_access, AccessRequired::Administrator as i32);
        assert_eq!(link.modify_invite_link_password.unwrap().invite_link_password.len(), 16, "a first link gets a password");
        let invite = actions(&ops, &g, &aci(1), &Edit::Add(vec![(Uuid::from(aci(3)), false)]), Vec::new()).unwrap();
        assert_eq!(invite.add_members_pending_profile_key.len(), 1);
        let timer = actions(&ops, &g, &aci(1), &Edit::Timer(3600), Vec::new()).unwrap();
        assert!(timer.modify_disappearing_message_timer.is_some());
    }

    #[test]
    fn a_group_link_goes_there_and_back() {
        let key = [5u8; 32];
        let link = link_of(&key, &[1, 2, 3, 4]);
        assert!(link.starts_with("https://signal.group/#"));
        assert_eq!(parse_link(&link), Some((key, vec![1, 2, 3, 4])));
        assert_eq!(parse_link(&link.replace("https://", "sgnl://")), Some((key, vec![1, 2, 3, 4])));
        assert_eq!(parse_link("https://signal.group/#nonsense"), None);
        assert_eq!(parse_link("https://example.com/"), None);
    }
}
