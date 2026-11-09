//! How Signal's things are named here.
//!
//! Signal has no message ids. A message is the person who sent it and the time they say they sent it, in milliseconds;
//! a reply, a reaction, an edit and a delete all point at it that way. A conversation is a person (their ACI) or a group
//! (its 32-byte master key). The daemon wants one string for each, so these are those strings, and the way back.

use presage::{
    libsignal_service::protocol::{Aci, Pni, ServiceId},
    store::Thread,
};
use uuid::Uuid;

/// The conversation's id, as the buffer carries it.
pub fn remote_id(thread: &Thread) -> String {
    match thread {
        Thread::Contact(ServiceId::Aci(aci)) => format!("p:{}", Uuid::from(*aci)),
        Thread::Contact(ServiceId::Pni(pni)) => format!("n:{}", Uuid::from(*pni)),
        Thread::Group(key) => format!("g:{}", hex(key)),
    }
}

/// The conversation an id names.
pub fn parse_remote_id(id: &str) -> Option<Thread> {
    let (kind, rest) = id.split_once(':')?;
    match kind {
        "p" => Some(Thread::Contact(ServiceId::Aci(Aci::from(Uuid::parse_str(rest).ok()?)))),
        "n" => Some(Thread::Contact(ServiceId::Pni(Pni::from(Uuid::parse_str(rest).ok()?)))),
        "g" => {
            let bytes = unhex(rest)?;
            Some(Thread::Group(bytes.try_into().ok()?))
        }
        _ => None,
    }
}

/// The id of the message `author` sent at `sent_ms`.
pub fn message_id(sent_ms: u64, author: &Uuid) -> String {
    format!("{sent_ms}-{author}")
}

/// The time and the author a message id names.
pub fn parse_message_id(id: &str) -> Option<(u64, Uuid)> {
    let (ms, author) = id.split_once('-')?;
    Some((ms.parse().ok()?, Uuid::parse_str(author).ok()?))
}

/// The person a service id is, whichever kind of id it is.
pub fn uuid_of(id: &ServiceId) -> Uuid {
    match id {
        ServiceId::Aci(aci) => Uuid::from(*aci),
        ServiceId::Pni(pni) => Uuid::from(*pni),
    }
}

/// An ACI as the protobufs carry it: a string in older messages, sixteen bytes in newer ones.
pub fn aci_of(text: Option<&str>, binary: Option<&[u8]>) -> Option<Uuid> {
    binary.and_then(|b| Uuid::from_slice(b).ok()).or_else(|| text.and_then(|t| Uuid::parse_str(t).ok()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-1111-4111-8111-111111111111";

    #[test]
    fn a_conversation_id_goes_there_and_back() {
        let person = Thread::Contact(ServiceId::Aci(Aci::from(Uuid::parse_str(A).unwrap())));
        assert_eq!(remote_id(&person), format!("p:{A}"));
        assert_eq!(parse_remote_id(&remote_id(&person)), Some(person));
        let group = Thread::Group([7u8; 32]);
        let id = remote_id(&group);
        assert_eq!(id.len(), 2 + 64);
        assert_eq!(parse_remote_id(&id), Some(group));
        assert_eq!(parse_remote_id("g:abc"), None);
        assert_eq!(parse_remote_id("x:abc"), None);
        assert_eq!(parse_remote_id("nonsense"), None);
    }

    #[test]
    fn a_message_id_is_the_time_and_the_author() {
        let author = Uuid::parse_str(A).unwrap();
        let id = message_id(1_700_000_000_123, &author);
        assert_eq!(parse_message_id(&id), Some((1_700_000_000_123, author)));
        assert_eq!(parse_message_id("12-notauuid"), None);
        assert_eq!(parse_message_id("abc"), None);
    }

    #[test]
    fn an_aci_is_read_from_either_form() {
        let author = Uuid::parse_str(A).unwrap();
        assert_eq!(aci_of(Some(A), None), Some(author));
        assert_eq!(aci_of(None, Some(author.as_bytes())), Some(author));
        assert_eq!(aci_of(Some("garbage"), None), None);
        assert_eq!(aci_of(None, Some(&[1, 2, 3])), None);
    }
}
