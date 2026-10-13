//! The groups a Steam account has sorted its friends into.
//!
//! Steam keeps them for the account - "Raiders", "Work" - and tells every
//! client of it, so a friend filed in one place is filed in all. A friend can be
//! in several. The window files each conversation under the one heading its own
//! category system has for it and mirrors changes both ways; this is the
//! daemon's half: what Steam says, and the calls that change it.

use super::link::Link;
use super::proto::*;
use super::steamid;
use super::wire::emsg;
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};

/// The groups and who is in each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Groups {
    names: BTreeMap<i32, String>,
    members: BTreeMap<i32, BTreeSet<u64>>,
}

impl Groups {
    /// Takes in what Steam said: all of it, or what changed. A list marked as a
    /// removal names things taken away. Returns whether anything is different.
    pub fn apply(&mut self, list: &ClientFriendsGroupsList) -> bool {
        let before = self.clone();
        let incremental = list.bincremental.unwrap_or(false);
        let removal = list.bremoval.unwrap_or(false);
        if !incremental {
            self.names.clear();
            self.members.clear();
        }
        for group in &list.friendGroups {
            let Some(id) = group.nGroupID else { continue };
            if removal {
                self.names.remove(&id);
                self.members.remove(&id);
            } else {
                self.names
                    .insert(id, group.strGroupName.clone().unwrap_or_default());
                self.members.entry(id).or_default();
            }
        }
        for membership in &list.memberships {
            let (Some(friend), Some(id)) = (membership.ulSteamID, membership.nGroupID) else {
                continue;
            };
            if removal {
                if let Some(members) = self.members.get_mut(&id) {
                    members.remove(&friend);
                }
            } else if self.names.contains_key(&id) {
                self.members.entry(id).or_default().insert(friend);
            }
        }
        *self != before
    }

    /// The groups by number and name: for the live check, which has to find its own.
    #[cfg(test)]
    pub fn names_for_probe(&self) -> Vec<(i32, String)> {
        self.names.iter().map(|(i, n)| (*i, n.clone())).collect()
    }

    pub fn put_member(&mut self, group: i32, friend: u64, present: bool) {
        if !self.names.contains_key(&group) {
            return;
        }
        let members = self.members.entry(group).or_default();
        if present {
            members.insert(friend);
        } else {
            members.remove(&friend);
        }
    }

    pub fn put_group(&mut self, group: i32, name: &str, friends: &[u64]) {
        self.names.insert(group, name.to_string());
        self.members
            .insert(group, friends.iter().copied().collect());
    }

    pub fn rename(&mut self, group: i32, name: &str) {
        if let Some(held) = self.names.get_mut(&group) {
            *held = name.to_string();
        }
    }

    pub fn remove(&mut self, group: i32) {
        self.names.remove(&group);
        self.members.remove(&group);
    }

    /// As the window reads it: each group with its name and the friends in it
    /// by their 64-bit ids, which is how a conversation names who it is with.
    pub fn to_json(&self) -> Value {
        Value::Array(
            self.names
                .iter()
                .map(|(id, name)| json!({ "id": id, "name": name, "members": self.members.get(id).map(|m| m.iter().map(|f| f.to_string()).collect::<Vec<_>>()).unwrap_or_default() }))
                .collect(),
        )
    }
}

/// What each signed-in account's groups are now, for the window to ask for.
static HELD: LazyLock<Mutex<HashMap<String, Groups>>> = LazyLock::new(Default::default);

pub fn held(account_id: &str) -> Groups {
    HELD.lock()
        .unwrap()
        .get(account_id)
        .cloned()
        .unwrap_or_default()
}

pub fn clear(account_id: &str) {
    HELD.lock().unwrap().remove(account_id);
}

/// Keeps what Steam says and tells the window, if it is different.
pub fn heard(state: &AppState, account_id: &str, list: &ClientFriendsGroupsList) {
    let groups = {
        let mut all = HELD.lock().unwrap();
        let groups = all.entry(account_id.to_string()).or_default();
        if !groups.apply(list) {
            return;
        }
        groups.clone()
    };
    tell(state, account_id, &groups);
}

fn tell(state: &AppState, account_id: &str, groups: &Groups) {
    state.events.emit(
        "steamFriendGroups",
        json!({ "accountId": account_id, "groups": groups.to_json() }),
    );
}

/// What a change is, for the one call that makes it.
#[derive(Debug, PartialEq)]
pub enum Change {
    Add { group: i32, friend: u64 },
    Remove { group: i32, friend: u64 },
    Create { name: String, friends: Vec<u64> },
    Rename { group: i32, name: String },
    Delete { group: i32 },
}

/// Asks Steam to make a change, and keeps what it answers. Returns the number
/// of the group made, for a creation.
pub async fn change(
    state: &AppState,
    link: &Link,
    account_id: &str,
    own: u64,
    what: Change,
) -> Result<Option<i32>, String> {
    let refused = |code: Option<u32>| match code.unwrap_or(2) {
        1 => None,
        other => Some(super::eresult::describe(other as i32)),
    };
    let asked = |e: super::link::CallError| super::chat::refusal(&e);
    let mut made = None;
    match &what {
        Change::Add { group, friend } => {
            let answer: ClientAddFriendToGroupResponse = link
                .ask(
                    emsg::CLIENT_ADD_FRIEND_TO_GROUP,
                    &ClientAddFriendToGroup {
                        groupid: Some(*group),
                        steamiduser: Some(*friend),
                    },
                )
                .await
                .map_err(asked)?;
            if let Some(why) = refused(answer.eresult) {
                return Err(why);
            }
        }
        Change::Remove { group, friend } => {
            let answer: ClientRemoveFriendFromGroupResponse = link
                .ask(
                    emsg::CLIENT_REMOVE_FRIEND_FROM_GROUP,
                    &ClientRemoveFriendFromGroup {
                        groupid: Some(*group),
                        steamiduser: Some(*friend),
                    },
                )
                .await
                .map_err(asked)?;
            if let Some(why) = refused(answer.eresult) {
                return Err(why);
            }
        }
        Change::Create { name, friends } => {
            let answer: ClientCreateFriendsGroupResponse = link
                .ask(
                    emsg::CLIENT_CREATE_FRIENDS_GROUP,
                    &ClientCreateFriendsGroup {
                        steamid: Some(own),
                        groupname: Some(name.clone()),
                        steamid_friends: friends.clone(),
                    },
                )
                .await
                .map_err(asked)?;
            if let Some(why) = refused(answer.eresult) {
                return Err(why);
            }
            made = answer.groupid;
        }
        Change::Rename { group, name } => {
            let answer: ClientManageFriendsGroupResponse = link
                .ask(
                    emsg::CLIENT_MANAGE_FRIENDS_GROUP,
                    &ClientManageFriendsGroup {
                        groupid: Some(*group),
                        groupname: Some(name.clone()),
                        ..Default::default()
                    },
                )
                .await
                .map_err(asked)?;
            if let Some(why) = refused(answer.eresult) {
                return Err(why);
            }
        }
        Change::Delete { group } => {
            let answer: ClientDeleteFriendsGroupResponse = link
                .ask(
                    emsg::CLIENT_DELETE_FRIENDS_GROUP,
                    &ClientDeleteFriendsGroup {
                        steamid: Some(own),
                        groupid: Some(*group),
                    },
                )
                .await
                .map_err(asked)?;
            if let Some(why) = refused(answer.eresult) {
                return Err(why);
            }
        }
    }
    // What Steam has just agreed to, held now; if it also says so, it changes nothing.
    let groups = {
        let mut all = HELD.lock().unwrap();
        let groups = all.entry(account_id.to_string()).or_default();
        match what {
            Change::Add { group, friend } => groups.put_member(group, friend, true),
            Change::Remove { group, friend } => groups.put_member(group, friend, false),
            Change::Create { name, friends } => {
                if let Some(id) = made {
                    groups.put_group(id, &name, &friends);
                }
            }
            Change::Rename { group, name } => groups.rename(group, &name),
            Change::Delete { group } => groups.remove(group),
        }
        groups.clone()
    };
    tell(state, account_id, &groups);
    Ok(made)
}

/// The friend a window names by their id as text.
pub fn friend_of(text: &str) -> Option<u64> {
    text.parse::<u64>()
        .ok()
        .filter(|id| steamid::account_of(*id).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u64 = 76561197960287930;
    const B: u64 = 76561197960265729;

    fn list(
        groups: &[(i32, &str)],
        members: &[(u64, i32)],
        incremental: bool,
        removal: bool,
    ) -> ClientFriendsGroupsList {
        ClientFriendsGroupsList {
            bremoval: Some(removal),
            bincremental: Some(incremental),
            friendGroups: groups
                .iter()
                .map(|(id, name)| FriendGroup {
                    nGroupID: Some(*id),
                    strGroupName: Some(name.to_string()),
                })
                .collect(),
            memberships: members
                .iter()
                .map(|(f, g)| FriendGroupMembership {
                    ulSteamID: Some(*f),
                    nGroupID: Some(*g),
                })
                .collect(),
        }
    }

    #[test]
    fn what_steam_says_is_the_groups_and_who_is_in_them() {
        let mut groups = Groups::default();
        assert!(groups.apply(&list(
            &[(7, "Raiders"), (8, "Work")],
            &[(A, 7), (B, 7), (B, 8)],
            false,
            false
        )));
        assert_eq!(
            groups.to_json(),
            json!([{ "id": 7, "name": "Raiders", "members": [B.to_string(), A.to_string()] }, { "id": 8, "name": "Work", "members": [B.to_string()] }])
        );
        assert!(
            !groups.apply(&list(&[(7, "Raiders")], &[(A, 7)], true, false)),
            "the same said again changes nothing"
        );
    }

    #[test]
    fn a_change_is_one_thing_and_a_full_list_is_everything() {
        let mut groups = Groups::default();
        groups.apply(&list(
            &[(7, "Raiders"), (8, "Work")],
            &[(A, 7), (B, 8)],
            false,
            false,
        ));
        // Incremental: a rename, somebody moved.
        assert!(groups.apply(&list(&[(7, "Raid Team")], &[(B, 7)], true, false)));
        assert_eq!(groups.names[&7], "Raid Team");
        assert!(groups.members[&7].contains(&B) && groups.members[&7].contains(&A));
        // A removal of a person from a group, and of a group.
        assert!(groups.apply(&list(&[], &[(A, 7)], true, true)));
        assert!(!groups.members[&7].contains(&A));
        assert!(groups.apply(&list(&[(8, "")], &[], true, true)));
        assert!(!groups.names.contains_key(&8) && !groups.members.contains_key(&8));
        // A full list replaces what was held.
        assert!(groups.apply(&list(&[(9, "New")], &[], false, false)));
        assert_eq!(groups.names.keys().copied().collect::<Vec<_>>(), vec![9]);
        // Somebody in a group nobody has named is in no group.
        assert!(!groups.apply(&list(&[], &[(A, 55)], true, false)));
    }

    #[tokio::test]
    async fn changes_are_asked_of_steam_and_kept_when_it_agrees() {
        use crate::backend::steam::link::{self, fake::*};
        use prost::Message;
        let state = crate::backend::kick::testkit::simulated_daemon("steam-groups-change");
        let url = serve(Box::new(|p| match p.emsg {
            emsg::CLIENT_CREATE_FRIENDS_GROUP => {
                let ask = ClientCreateFriendsGroup::decode(p.body.as_slice()).unwrap();
                assert_eq!(
                    (
                        ask.steamid,
                        ask.groupname.as_deref(),
                        ask.steamid_friends.clone()
                    ),
                    (Some(A), Some("Raiders"), vec![B])
                );
                Some(vec![answer(
                    &p,
                    1,
                    ClientCreateFriendsGroupResponse {
                        eresult: Some(1),
                        groupid: Some(7),
                    }
                    .encode_to_vec(),
                )])
            }
            emsg::CLIENT_ADD_FRIEND_TO_GROUP => Some(vec![answer(
                &p,
                1,
                ClientAddFriendToGroupResponse { eresult: Some(1) }.encode_to_vec(),
            )]),
            emsg::CLIENT_REMOVE_FRIEND_FROM_GROUP => Some(vec![answer(
                &p,
                1,
                ClientRemoveFriendFromGroupResponse { eresult: Some(15) }.encode_to_vec(),
            )]),
            _ => Some(vec![]),
        }))
        .await;
        let (link, _told) = link::open_with("test", &url, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        let account = "steam:groups-test";
        let made = change(
            &state,
            &link,
            account,
            A,
            Change::Create {
                name: "Raiders".into(),
                friends: vec![B],
            },
        )
        .await
        .unwrap();
        assert_eq!(made, Some(7));
        assert_eq!(
            held(account).to_json(),
            json!([{ "id": 7, "name": "Raiders", "members": [B.to_string()] }])
        );
        change(
            &state,
            &link,
            account,
            A,
            Change::Add {
                group: 7,
                friend: A,
            },
        )
        .await
        .unwrap();
        assert!(held(account).members[&7].contains(&A));
        // Steam refusing leaves what was held as it was, and says why.
        assert!(change(
            &state,
            &link,
            account,
            A,
            Change::Remove {
                group: 7,
                friend: A
            }
        )
        .await
        .is_err());
        assert!(held(account).members[&7].contains(&A));
        clear(account);
        assert_eq!(held(account), Groups::default());
    }
}
