//! What a bot offers: the commands in its menu.
//!
//! Telegram keeps a bot's list of commands (`/help`, `/weblink`, ...) with the bot, and its own apps show it as a
//! Menu button beside the message box and as suggestions while typing a slash. It is read once from the bot's profile
//! and kept for a while, since it changes when the bot's author changes it and not otherwise.

use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::tl;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a bot's list is kept before it is asked for again.
const KEEP: Duration = Duration::from_secs(15 * 60);

/// One command in a bot's menu.
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub name: String,
    pub description: String,
}

/// When each conversation's list was read, and what it said.
type Kept = Mutex<HashMap<String, (Instant, Vec<Command>)>>;

fn kept() -> &'static Kept {
    static K: std::sync::OnceLock<Kept> = std::sync::OnceLock::new();
    K.get_or_init(Default::default)
}

/// The commands a bot's profile lists.
fn from_info(info: &tl::enums::BotInfo) -> Vec<Command> {
    let tl::enums::BotInfo::Info(info) = info;
    info.commands
        .iter()
        .flatten()
        .map(|c| {
            let tl::enums::BotCommand::Command(c) = c;
            Command { name: c.command.trim_start_matches('/').to_string(), description: c.description.clone() }
        })
        .filter(|c| !c.name.is_empty())
        .collect()
}

/// The commands of the bot a conversation is with; nothing for a conversation that is not with a bot.
pub async fn commands(state: &AppState, buffer: &Buffer) -> Result<Vec<Command>, String> {
    if buffer.kind != "dm" || buffer.name_icon.as_deref() != Some("smart_toy") {
        return Ok(Vec::new());
    }
    if let Some((when, list)) = kept().lock().unwrap_or_else(|e| e.into_inner()).get(&buffer.id) {
        if when.elapsed() < KEEP {
            return Ok(list.clone());
        }
    }
    let (handle, peer) = peer_for(state, buffer).await?;
    let answer = handle.client.invoke(&tl::functions::users::GetFullUser { id: peer.into() }).await.map_err(|e| explain(&e))?;
    let tl::enums::users::UserFull::Full(full) = answer;
    let tl::enums::UserFull::Full(user) = full.full_user;
    let list = user.bot_info.as_ref().map(from_info).unwrap_or_default();
    kept().lock().unwrap_or_else(|e| e.into_inner()).insert(buffer.id.clone(), (Instant::now(), list.clone()));
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bots_menu_is_read_from_its_profile() {
        let info = tl::enums::BotInfo::Info(tl::types::BotInfo {
            has_preview_medias: false,
            user_id: Some(1),
            description: None,
            description_photo: None,
            description_document: None,
            commands: Some(vec![
                tl::enums::BotCommand::Command(tl::types::BotCommand { ephemeral: false, command: "help".into(), description: "Show help for bot commands".into() }),
                tl::enums::BotCommand::Command(tl::types::BotCommand { ephemeral: false, command: "/weblink".into(), description: "Get available link to ZLibrary".into() }),
                tl::enums::BotCommand::Command(tl::types::BotCommand { ephemeral: false, command: "".into(), description: "nothing".into() }),
            ]),
            menu_button: None,
            privacy_policy_url: None,
            app_settings: None,
            verifier_settings: None,
        });
        assert_eq!(
            from_info(&info),
            vec![
                Command { name: "help".into(), description: "Show help for bot commands".into() },
                Command { name: "weblink".into(), description: "Get available link to ZLibrary".into() },
            ]
        );
    }
}
