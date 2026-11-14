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
/// The construction is libsignal's own (`Fingerprint`, version 2, 5200 iterations, each side's ACI bytes), as Signal's
/// apps make it; the same pair's scannable form is `scannable`.
pub async fn safety_number(store: &SignalStore, own: &Uuid, who: &Uuid) -> Option<String> {
    use presage::libsignal_service::protocol::Fingerprint;
    let local = store.own_identity().ok().flatten()?;
    let remote = store.identity_of(&ServiceId::Aci(Aci::from(*who)).service_id_string()).ok().flatten()?;
    let fingerprint = Fingerprint::new(2, 5200, own.as_bytes(), &local, who.as_bytes(), &remote).ok()?;
    fingerprint.display_string().ok().map(|digits| grouped(&digits))
}

/// The same pair's fingerprint as Signal's apps put it in a QR code (the scannable form), for comparing by camera.
pub fn scannable(store: &SignalStore, own: &Uuid, who: &Uuid) -> Option<Vec<u8>> {
    use presage::libsignal_service::protocol::Fingerprint;
    let local = store.own_identity().ok().flatten()?;
    let remote = store.identity_of(&ServiceId::Aci(Aci::from(*who)).service_id_string()).ok().flatten()?;
    let fingerprint = Fingerprint::new(2, 5200, own.as_bytes(), &local, who.as_bytes(), &remote).ok()?;
    fingerprint.scannable.serialize().ok()
}

/// A QR code of `bytes` (byte mode, as Signal's apps encode a fingerprint), as a PNG.
pub fn qr_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let code = qrcode::QrCode::new(bytes).map_err(|e| e.to_string())?;
    let picture = code.render::<image::Luma<u8>>().min_dimensions(320, 320).quiet_zone(true).build();
    let mut out = std::io::Cursor::new(Vec::new());
    picture.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// The raw contents of every QR code in a picture.
pub fn read_qr(picture: &[u8]) -> Vec<Vec<u8>> {
    let Ok(image) = image::load_from_memory(picture) else { return Vec::new() };
    let grey = image.to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare(grey);
    prepared
        .detect_grids()
        .into_iter()
        .filter_map(|grid| {
            let mut bytes = Vec::new();
            grid.decode_to(&mut bytes).ok().map(|_| bytes)
        })
        .collect()
}

/// What comparing a scanned code with the safety number here says.
#[derive(Debug, PartialEq, Eq)]
pub enum Compared {
    Same,
    Different,
    /// No code was found in the picture.
    NoCode,
    /// A code that is not a Signal safety number (or of another version).
    NotASafetyNumber,
}

/// Compares the codes in a picture with this pair's fingerprint.
pub fn compare_picture(mine: &[u8], picture: &[u8]) -> Compared {
    let codes = read_qr(picture);
    if codes.is_empty() {
        return Compared::NoCode;
    }
    let Ok(local) = presage::libsignal_service::protocol::ScannableFingerprint::deserialize(mine) else { return Compared::NotASafetyNumber };
    let mut any = false;
    for code in codes {
        match local.compare(&code) {
            Ok(true) => return Compared::Same,
            Ok(false) => any = true,
            Err(_) => {}
        }
    }
    if any {
        Compared::Different
    } else {
        Compared::NotASafetyNumber
    }
}

/// The QR code of the safety number with a person, as a data address the window shows.
pub fn safety_qr(state: &AppState, account_id: &str, user_id: &str) -> Result<serde_json::Value, String> {
    use base64::Engine;
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let who = Uuid::parse_str(user_id.trim_start_matches("p:")).map_err(|_| "that is not a Signal person".to_string())?;
    let bytes = scannable(&handle.store, &handle.own, &who).ok_or("Their key is not known yet: exchange a message with them first")?;
    let png = qr_png(&bytes)?;
    Ok(json!({ "qr": format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)) }))
}

/// Compares a picture of the other side's code (a camera frame or a chosen file, as base64) with the safety number here.
pub fn compare_safety_qr(state: &AppState, account_id: &str, user_id: &str, picture_b64: &str) -> Result<serde_json::Value, String> {
    use base64::Engine;
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let who = Uuid::parse_str(user_id.trim_start_matches("p:")).map_err(|_| "that is not a Signal person".to_string())?;
    let mine = scannable(&handle.store, &handle.own, &who).ok_or("Their key is not known yet: exchange a message with them first")?;
    // A camera frame comes as base64; a chosen picture (a photo of their phone, say) by its path.
    let picture = if let Some(path) = picture_b64.strip_prefix("path:") {
        let size = std::fs::metadata(path).map_err(|e| format!("that picture could not be read: {e}"))?.len();
        if size > 20 * 1024 * 1024 {
            return Err("That picture is too big to look for a code in".to_string());
        }
        std::fs::read(path).map_err(|e| format!("that picture could not be read: {e}"))?
    } else {
        base64::engine::general_purpose::STANDARD.decode(picture_b64.split_once(',').map(|(_, b)| b).unwrap_or(picture_b64)).map_err(|_| "that picture could not be read".to_string())?
    };
    let result = match compare_picture(&mine, &picture) {
        Compared::Same => "same",
        Compared::Different => "different",
        Compared::NoCode => "none",
        Compared::NotASafetyNumber => "other",
    };
    Ok(json!({ "result": result }))
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

    #[tokio::test]
    async fn a_safety_number_qr_from_one_side_matches_the_other_and_not_a_stranger() {
        use presage::libsignal_service::protocol::{Fingerprint, IdentityKeyPair};
        let mut rng = rand09::rng();
        let (a, b, c) = (IdentityKeyPair::generate(&mut rng), IdentityKeyPair::generate(&mut rng), IdentityKeyPair::generate(&mut rng));
        let (a_id, b_id) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let from_a = Fingerprint::new(2, 5200, a_id.as_bytes(), a.identity_key(), b_id.as_bytes(), b.identity_key()).unwrap();
        let from_b = Fingerprint::new(2, 5200, b_id.as_bytes(), b.identity_key(), a_id.as_bytes(), a.identity_key()).unwrap();
        let wrong = Fingerprint::new(2, 5200, b_id.as_bytes(), c.identity_key(), a_id.as_bytes(), a.identity_key()).unwrap();
        let mine = from_a.scannable.serialize().unwrap();
        // Their phone shows its code; moho reads it from a picture of it.
        let theirs = qr_png(&from_b.scannable.serialize().unwrap()).unwrap();
        assert_eq!(compare_picture(&mine, &theirs), Compared::Same);
        assert_eq!(compare_picture(&mine, &qr_png(&wrong.scannable.serialize().unwrap()).unwrap()), Compared::Different);
        assert_eq!(compare_picture(&mine, &qr_png(b"https://example.org").unwrap()), Compared::NotASafetyNumber);
        let blank = {
            let mut out = std::io::Cursor::new(Vec::new());
            image::GrayImage::from_pixel(64, 64, image::Luma([255])).write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        assert_eq!(compare_picture(&mine, &blank), Compared::NoCode);
    }

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
