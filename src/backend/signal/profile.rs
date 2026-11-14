//! The card for a person: their name, the number or username the account knows them by, what they wrote about themselves,
//! and the safety number that says whether this account is talking to the same person's keys as they are.

use super::people::{self, Ctx};
use super::store::SignalStore;
use super::{ids, media};
use crate::profile;
use crate::state::AppState;
use presage::{
    libsignal_service::protocol::{Aci, ServiceId},
    store::ContentsStore,
};
use serde_json::{json, Value};
use uuid::Uuid;

/// A safety number is sixty digits, read in twelve groups of five.
pub fn grouped(digits: &str) -> String {
    digits.as_bytes().chunks(5).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

/// The safety number between this account and `who`, if both identity keys are known. Version 2 of Signal's fingerprint,
/// which names each side by its ACI (version 1 used the phone number).
///
/// **Not yet compared with another Signal client's number for the same pair** (no account to do it with); the
/// construction is libsignal's own, and what is assumed is the stable identifier (the ACI's sixteen bytes).
pub async fn safety_number(store: &SignalStore, own: &Uuid, who: &Uuid) -> Option<String> {
    use presage::libsignal_service::protocol::Fingerprint;
    let local = store.own_identity().ok().flatten()?;
    let remote = store.identity_of(&ServiceId::Aci(Aci::from(*who)).service_id_string()).ok().flatten()?;
    let fingerprint = Fingerprint::new(2, 5200, own.as_bytes(), &local, who.as_bytes(), &remote).ok()?;
    fingerprint.display_string().ok().map(|digits| grouped(&digits))
}

/// The person a card is for: the other one in a direct message, or the member the window named by id.
fn target_of(state: &AppState, buffer_id: &str, user_id: &str) -> Option<Uuid> {
    if let Some(buffer) = state.runtime.get_buffer(buffer_id) {
        if buffer.kind == "dm" {
            if let Some(presage::store::Thread::Contact(id)) = buffer.remote_id.as_deref().and_then(ids::parse_remote_id) {
                return Some(ids::uuid_of(&id));
            }
        }
    }
    Uuid::parse_str(user_id).ok()
}

/// Whether the account had marked this person verified.
pub fn was_verified(store: &SignalStore, who: &Uuid) -> bool {
    store.blob(&format!("verified:{who}")).ok().flatten().is_some_and(|b| b == b"1")
}

/// A verified person's key changed: no longer verified, and sends to them wait for the account to look.
pub fn hold_for_confirmation(store: &SignalStore, who: &Uuid) {
    let _ = store.set_blob(&format!("verified:{who}"), b"0");
    let _ = store.set_blob(&format!("unconfirmed:{who}"), b"1");
}

/// Whether sends to this person wait for the account to look at their changed safety number.
pub fn unconfirmed(store: &SignalStore, who: &Uuid) -> bool {
    store.blob(&format!("unconfirmed:{who}")).ok().flatten().is_some_and(|b| b == b"1")
}

/// The account has looked (marked them verified again, cleared it, or said to send anyway).
pub fn confirm(store: &SignalStore, who: &Uuid) {
    let _ = store.set_blob(&format!("unconfirmed:{who}"), b"0");
}

/// The titles of the groups the account and this person are both in.
pub async fn groups_in_common(store: &SignalStore, who: &Uuid) -> Vec<String> {
    use presage::store::ContentsStore;
    let aci = Aci::from(*who);
    let Ok(groups) = store.groups().await else { return Vec::new() };
    let mut titles: Vec<String> = groups.filter_map(Result::ok).filter(|(key, g)| g.is_member(&aci) && !store.has_left(key)).map(|(_, g)| g.title).collect();
    titles.sort_by_key(|t| t.to_lowercase());
    titles
}

pub async fn build(state: &AppState, account_id: &str, buffer_id: &str, who: &str, user_id: &str) -> Value {
    let mut card = profile::pending("signal", account_id, who);
    card["pending"] = json!(false);
    let Some(handle) = state.runtime.signal_handle(account_id) else { return card };
    let Some(target) = target_of(state, buffer_id, user_id) else { return card };
    card["id"] = json!(target.to_string());
    let ctx = Ctx::new(state, account_id, handle.store.clone(), handle.own);
    card["name"] = json!(people::person(&ctx, &target).await);
    profile::set(&mut card, "avatarUrl", media::avatar_of(account_id, &target).map(|u| json!(u)));
    let id = ServiceId::Aci(Aci::from(target));
    if let Ok(Some(contact)) = ctx.store.contact_by_id(&id).await {
        if let Some(number) = contact.phone_number {
            profile::note(&mut card, "Number", number.to_string());
        }
    }
    if let Ok(Some(key)) = ctx.store.profile_key(&id).await {
        if let Ok(Some(found)) = ctx.store.profile(target, key).await {
            if let Some(about) = found.about.filter(|a| !a.trim().is_empty()) {
                let emoji = found.about_emoji.unwrap_or_default();
                profile::note(&mut card, "About", format!("{emoji} {about}").trim().to_string());
            }
        }
    }
    let kept = super::storage::current(account_id, &ctx.store);
    if let Some(standing) = kept.standing(&format!("p:{target}")) {
        if let Some(username) = &standing.username {
            profile::note(&mut card, "Username", username.clone());
        }
        if let Some(note) = &standing.note {
            profile::note(&mut card, "Your note", note.clone());
        }
    }
    if target != handle.own {
        if let Some(number) = safety_number(&ctx.store, &handle.own, &target).await {
            profile::note(&mut card, "Safety number (compare with theirs in Signal; not yet checked against another client)", number);
        }
        let common = groups_in_common(&ctx.store, &target).await;
        if !common.is_empty() {
            profile::note(&mut card, "Groups in common", common.join(", "));
        }
        card["signalVerified"] = json!(was_verified(&ctx.store, &target));
        card["signalUnconfirmed"] = json!(unconfirmed(&ctx.store, &target));
        card["signalBlocked"] = json!(kept.blocked(&format!("p:{target}")));
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixty_digits_are_read_in_twelve_groups_of_five() {
        let digits = "300354477692869396892869876765458257569162576843440918079131";
        assert_eq!(grouped(digits), "30035 44776 92869 39689 28698 76765 45825 75691 62576 84344 09180 79131");
        assert_eq!(grouped(digits).split(' ').count(), 12);
    }

    #[tokio::test]
    async fn nothing_is_said_of_a_person_whose_key_is_not_known() {
        let store = SignalStore::open_in_memory(presage::model::identity::OnNewIdentity::Trust).unwrap();
        assert_eq!(safety_number(&store, &Uuid::nil(), &Uuid::from_u128(7)).await, None);
    }

    #[tokio::test]
    async fn both_sides_read_the_same_number_and_a_changed_key_changes_it() {
        use presage::libsignal_service::protocol::{DeviceId, IdentityKeyPair, IdentityKeyStore, ProtocolAddress};
        use presage::store::{StateStore, Store};
        let (a_id, b_id) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let mut rng = rand09::rng();
        let (a_pair, b_pair) = (IdentityKeyPair::generate(&mut rng), IdentityKeyPair::generate(&mut rng));
        let make = |own: &IdentityKeyPair, other: &Uuid, other_key: presage::libsignal_service::protocol::IdentityKey| {
            let store = SignalStore::open_in_memory(presage::model::identity::OnNewIdentity::Trust).unwrap();
            let own = *own;
            let name = ServiceId::Aci(Aci::from(*other)).service_id_string();
            async move {
                store.set_aci_identity_key_pair(own).await.unwrap();
                let mut protocol = store.aci_protocol_store();
                protocol.save_identity(&ProtocolAddress::new(name, DeviceId::new(1).unwrap()), &other_key).await.unwrap();
                store
            }
        };
        let a = make(&a_pair, &b_id, *b_pair.identity_key()).await;
        let b = make(&b_pair, &a_id, *a_pair.identity_key()).await;
        let from_a = safety_number(&a, &a_id, &b_id).await.expect("a number");
        let from_b = safety_number(&b, &b_id, &a_id).await.expect("a number");
        assert_eq!(from_a, from_b, "both ends of the conversation read the same sixty digits");
        assert_eq!(from_a.split(' ').count(), 12);
        // Somebody else's key under the same name is a different number.
        let other = IdentityKeyPair::generate(&mut rng);
        let c = make(&a_pair, &b_id, *other.identity_key()).await;
        assert_ne!(safety_number(&c, &a_id, &b_id).await.unwrap(), from_a);
    }
}
