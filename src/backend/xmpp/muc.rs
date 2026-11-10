//! Group chats (XEP-0045). Milestone 3: until then a room's messages are not shown.

use super::messages::Origin;
use super::session::Handle;
use crate::state::AppState;
use tokio_xmpp::parsers::message::Message;

pub async fn handle_groupchat(_state: &AppState, _handle: &Handle, _message: Message, _origin: Origin) {}
