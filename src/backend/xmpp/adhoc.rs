//! Ad-hoc commands (XEP-0050): what a server, a component or a person's client offers to be run, as steps of forms. A server's
//! administration, a gateway's registration, a bot's menu - they are all this, which is why it is generic: a list of what
//! an address offers, and one call that starts a command or answers its current step.

use super::dataform::{self, Form};
use super::session::Handle;
use super::xml::Attr;
use super::ns;
use serde::Serialize;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;

pub const COMMANDS: &str = "http://jabber.org/protocol/commands";

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Command {
    pub node: String,
    pub name: String,
}

/// What a step of a command says.
#[derive(Debug, Serialize, Clone, Default, PartialEq)]
pub struct Step {
    /// `executing`, `completed` or `canceled`.
    pub status: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// The form to fill in, if this step asks for one (or the result, if it is a table of results).
    pub form: Option<Form>,
    /// Words the command says (`info`, `warn`, `error`).
    pub notes: Vec<(String, String)>,
    /// What can be done next: `next`, `prev`, `complete`, and which is the default.
    pub actions: Vec<String>,
}

/// The commands an address offers.
pub async fn list(handle: &Handle, to: &Jid) -> Result<Vec<Command>, String> {
    let q = Element::builder("query", ns::DISCO_ITEMS).at("node", COMMANDS).build();
    let answer = handle.request(Some(to.clone()), false, q).await.map_err(|e| e.words())?;
    Ok(answer.map(|a| a.children().filter(|c| c.name() == "item").filter_map(|c| Some(Command { node: c.attr("node")?.to_string(), name: c.attr("name").unwrap_or(c.attr("node")?).to_string() })).collect()).unwrap_or_default())
}

/// Reads the `<command>` an address answered with.
pub fn read_step(command: &Element) -> Step {
    let mut step = Step { status: command.attr("status").unwrap_or("completed").to_string(), session_id: command.attr("sessionid").unwrap_or_default().to_string(), ..Default::default() };
    step.form = command.children().find(|c| c.is("x", ns::DATA)).map(dataform::parse);
    step.notes = command.children().filter(|c| c.name() == "note").map(|n| (n.attr("type").unwrap_or("info").to_string(), n.text())).collect();
    if let Some(actions) = command.children().find(|c| c.name() == "actions") {
        step.actions = actions.children().map(|a| a.name().to_string()).collect();
    }
    step
}

/// Starts a command (`session` empty) or answers a step of one: `action` is `execute` (the default), `next`, `prev`, `complete` or `cancel`.
pub async fn run(handle: &Handle, to: &Jid, node: &str, session: &str, action: &str, form: Option<&Form>, answers: &serde_json::Value) -> Result<Step, String> {
    let mut c = Element::builder("command", COMMANDS).at("node", node);
    if !session.is_empty() {
        c = c.at("sessionid", session);
    }
    if !action.is_empty() && action != "execute" {
        c = c.at("action", action);
    }
    let mut c = c.build();
    if let (Some(form), true) = (form, action != "cancel") {
        c.append_child(dataform::submit(form, answers));
    }
    let answer = handle.request(Some(to.clone()), true, c).await.map_err(|e| e.words())?;
    answer.as_ref().map(read_step).ok_or_else(|| "the command answered with nothing".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_step_says_where_the_command_is_what_to_fill_in_and_what_can_be_done_next() {
        let c = Element::from_str("<command xmlns='http://jabber.org/protocol/commands' node='n' sessionid='s1' status='executing'><actions execute='next'><next/><prev/></actions><note type='info'>careful</note><x xmlns='jabber:x:data' type='form'><title>T</title><field var='a' type='text-single'/></x></command>").unwrap();
        let s = read_step(&c);
        assert_eq!((s.status.as_str(), s.session_id.as_str(), s.actions.clone(), s.notes.clone()), ("executing", "s1", vec!["next".to_string(), "prev".to_string()], vec![("info".to_string(), "careful".to_string())]));
        assert_eq!(s.form.unwrap().fields.len(), 1);
    }

    #[test]
    fn a_finished_command_with_no_form_is_completed() {
        let c = Element::from_str("<command xmlns='http://jabber.org/protocol/commands' node='n' sessionid='s' status='completed'><note>done</note></command>").unwrap();
        let s = read_step(&c);
        assert_eq!((s.status.as_str(), s.form.is_none(), s.notes[0].0.as_str()), ("completed", true, "info"));
    }
}
