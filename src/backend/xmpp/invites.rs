//! Invitations (XEP-0401, with XEP-0379's pre-authorised subscriptions): a link this account hands somebody, made by its server
//! through an ad-hoc command, that adds this account as a contact without the request having to be answered (and, for a server
//! that lets its users invite people to make accounts, makes one). And the other side: a link of that kind, opened here, sends
//! its token with the request, so the inviter's server approves it.

use super::adhoc;
use super::session::Handle;
use serde::Serialize;
use tokio_xmpp::jid::Jid;

/// Somebody to add as a contact, who is asked to add this account back.
pub const CONTACT: &str = "urn:xmpp:invite#invite";
/// An account made on this server, with this account as its first contact.
pub const ACCOUNT: &str = "urn:xmpp:invite#create-account";
/// The pre-authorisation a subscription request carries (XEP-0379).
pub const PARS: &str = "urn:xmpp:pars:0";

/// An invitation the server made.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Invite {
    /// The `xmpp:` link (with its `preauth` token).
    pub uri: String,
    /// A web page to send people who have no client yet, when the server has one.
    #[serde(rename = "landingUrl")]
    pub landing_url: String,
    /// When it stops working, as the server wrote it.
    pub expires: String,
}

/// Which kinds of invitation the account's server makes for it: `contact`, `account`.
pub async fn offered(handle: &Handle) -> Vec<&'static str> {
    let Ok(server) = handle.own().domain().as_str().parse::<Jid>() else { return Vec::new() };
    let nodes: Vec<String> = adhoc::list(handle, &server).await.unwrap_or_default().into_iter().map(|c| c.node).collect();
    let mut out = Vec::new();
    if nodes.iter().any(|n| n == CONTACT) {
        out.push("contact");
    }
    if nodes.iter().any(|n| n == ACCOUNT) {
        out.push("account");
    }
    out
}

/// What a finished invitation command says.
pub fn read(step: &adhoc::Step) -> Option<Invite> {
    let form = step.form.as_ref()?;
    let value = |var: &str| form.fields.iter().find(|f| f.var == var).and_then(|f| f.values.first().cloned()).unwrap_or_default();
    let invite = Invite { uri: value("uri"), landing_url: value("landing-url"), expires: value("expire") };
    (!invite.uri.is_empty()).then_some(invite)
}

/// Makes an invitation. A command that asks something first (some servers' account invitations) is answered with what it offers.
pub async fn create(handle: &Handle, kind: &str) -> Result<Invite, String> {
    let node = if kind == "account" { ACCOUNT } else { CONTACT };
    let server: Jid = handle.own().domain().as_str().parse().map_err(|_| "the server's address is not usable".to_string())?;
    let mut step = adhoc::run(handle, &server, node, "", "execute", None, &serde_json::json!({})).await?;
    for _ in 0..3 {
        if step.status != "executing" {
            break;
        }
        let form = step.form.clone();
        step = adhoc::run(handle, &server, node, &step.session_id, "complete", form.as_ref(), &serde_json::json!({})).await?;
    }
    if let Some((_, words)) = step.notes.iter().find(|(kind, _)| kind == "error") {
        return Err(words.clone());
    }
    read(&step).ok_or_else(|| "the server made no invitation".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tokio_xmpp::minidom::Element;

    #[test]
    fn an_invitation_is_read_from_what_the_server_answers() {
        let c = Element::from_str("<command xmlns='http://jabber.org/protocol/commands' status='completed' node='urn:xmpp:invite#invite'><x xmlns='jabber:x:data' type='result'><field var='landing-url'><value>https://example.org/invite/x</value></field><field var='uri'><value>xmpp:a@example.org?roster;preauth=tok</value></field><field var='expire'><value>2026-11-19T00:04:58Z</value></field></x></command>").unwrap();
        let i = read(&adhoc::read_step(&c)).unwrap();
        assert_eq!(i, Invite { uri: "xmpp:a@example.org?roster;preauth=tok".into(), landing_url: "https://example.org/invite/x".into(), expires: "2026-11-19T00:04:58Z".into() });
    }
}
