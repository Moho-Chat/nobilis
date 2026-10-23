//! What a bot offers: the commands in its menu.
//!
//! Telegram keeps a bot's list of commands (`/help`, `/weblink`, ...) with the bot, and its own apps show it as a
//! Menu button beside the message box and as suggestions while typing a slash. It is read once from the bot's profile
//! and kept for a while, since it changes when the bot's author changes it and not otherwise.
//!
//! In a group it is the commands of every bot that is in the group, which Telegram lists with the group itself (and
//! scopes: a bot can offer other commands to a group's administrators than to its members). Two bots can both have a
//! `/help`, and a group says which one is meant by writing the bot's name after it, `/help@somebot`: a command that
//! more than one bot has is offered once for each bot, in that form.

use super::peers;
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
    /// Whose it is, where that is not the conversation itself: a bot in a group, as `@name`.
    pub source: Option<String>,
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
            Command { name: c.command.trim_start_matches('/').to_string(), description: c.description.clone(), source: None }
        })
        .filter(|c| !c.name.is_empty())
        .collect()
}

/// The commands of a bot's conversation, or of the bots in a group; nothing for any other conversation.
pub async fn commands(state: &AppState, buffer: &Buffer) -> Result<Vec<Command>, String> {
    let with_bot = buffer.kind == "dm" && buffer.name_icon.as_deref() == Some("smart_toy");
    // A channel's posts are not commands, and a person is not a bot. A group (or a topic of one) has bots.
    let in_group = buffer.kind != "dm" && buffer.category.as_deref() != Some(peers::CHANNELS);
    if !with_bot && !in_group {
        return Ok(Vec::new());
    }
    // A group's topics are one group: they share a list.
    let key = format!("{}|{}", buffer.account_id, buffer.remote_id.as_deref().map(|r| r.split(':').next().unwrap_or(r)).unwrap_or(&buffer.id));
    if let Some((when, list)) = kept().lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if when.elapsed() < KEEP {
            return Ok(list.clone());
        }
    }
    let (handle, peer) = peer_for(state, buffer).await?;
    let list = if with_bot {
        let answer = handle.client.invoke(&tl::functions::users::GetFullUser { id: peer.into() }).await.map_err(|e| explain(&e))?;
        let tl::enums::users::UserFull::Full(full) = answer;
        let tl::enums::UserFull::Full(user) = full.full_user;
        user.bot_info.as_ref().map(from_info).unwrap_or_default()
    } else {
        let (infos, users) = if peer.id.kind() == grammers_session::types::PeerKind::Channel {
            let answer = handle.client.invoke(&tl::functions::channels::GetFullChannel { channel: peer.into() }).await.map_err(|e| explain(&e))?;
            let tl::enums::messages::ChatFull::Full(full) = answer;
            let tl::enums::ChatFull::ChannelFull(group) = full.full_chat else { return Ok(Vec::new()) };
            (group.bot_info, full.users)
        } else {
            let answer = handle.client.invoke(&tl::functions::messages::GetFullChat { chat_id: peer.id.bare_id_unchecked() }).await.map_err(|e| explain(&e))?;
            let tl::enums::messages::ChatFull::Full(full) = answer;
            let tl::enums::ChatFull::Full(group) = full.full_chat else { return Ok(Vec::new()) };
            (group.bot_info.unwrap_or_default(), full.users)
        };
        merge(&infos, &users)
    };
    kept().lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), list.clone()));
    Ok(list)
}

/// The @name a user is reached by.
fn username_of(user: &tl::enums::User) -> Option<String> {
    let tl::enums::User::User(user) = user else { return None };
    user.username.clone().or_else(|| user.usernames.as_ref()?.iter().find_map(|u| {
        let tl::enums::Username::Username(u) = u;
        u.active.then(|| u.username.clone())
    }))
}

/// The commands of the bots in a group, each named for its bot when more than one bot has it.
fn merge(infos: &[tl::enums::BotInfo], users: &[tl::enums::User]) -> Vec<Command> {
    let mut per_bot: Vec<(String, Vec<Command>)> = Vec::new();
    for info in infos {
        let tl::enums::BotInfo::Info(raw) = info;
        let Some(id) = raw.user_id else { continue };
        let Some(name) = users.iter().find(|u| matches!(u, tl::enums::User::User(u) if u.id == id)).and_then(username_of) else { continue };
        per_bot.push((name, from_info(info)));
    }
    label(&per_bot)
}

/// Each bot's commands as the group's menu, a name that two bots share written with the bot's after it.
fn label(per_bot: &[(String, Vec<Command>)]) -> Vec<Command> {
    let mut taken: HashMap<&str, usize> = HashMap::new();
    for (_, list) in per_bot {
        for c in list {
            *taken.entry(c.name.as_str()).or_default() += 1;
        }
    }
    let mut out = Vec::new();
    for (bot, list) in per_bot {
        for c in list {
            let name = if taken.get(c.name.as_str()).copied().unwrap_or(0) > 1 { format!("{}@{bot}", c.name) } else { c.name.clone() };
            out.push(Command { name, description: c.description.clone(), source: Some(format!("@{bot}")) });
        }
    }
    out
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
                Command { name: "help".into(), description: "Show help for bot commands".into(), source: None },
                Command { name: "weblink".into(), description: "Get available link to ZLibrary".into(), source: None },
            ]
        );
    }

    fn cmd(name: &str, description: &str) -> Command {
        Command { name: name.into(), description: description.into(), source: None }
    }

    #[test]
    fn a_group_offers_each_bots_commands_and_names_the_bot_only_where_two_share_one() {
        let per_bot = vec![
            ("rose_bot".to_string(), vec![cmd("help", "Rose help"), cmd("warn", "Warn a user")]),
            ("music_bot".to_string(), vec![cmd("help", "Music help"), cmd("play", "Play a song")]),
        ];
        let got: Vec<(String, Option<String>)> = label(&per_bot).into_iter().map(|c| (c.name, c.source)).collect();
        assert_eq!(
            got,
            vec![
                ("help@rose_bot".to_string(), Some("@rose_bot".to_string())),
                ("warn".to_string(), Some("@rose_bot".to_string())),
                ("help@music_bot".to_string(), Some("@music_bot".to_string())),
                ("play".to_string(), Some("@music_bot".to_string())),
            ]
        );
    }

    #[test]
    fn a_lone_bots_commands_are_not_named_for_it() {
        let got = label(&[("only_bot".to_string(), vec![cmd("start", "Begin")])]);
        assert_eq!(got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["start"]);
        assert!(label(&[]).is_empty());
    }

    #[test]
    fn a_bot_the_group_does_not_name_cannot_be_written_to_and_is_left_out() {
        let info = tl::enums::BotInfo::Info(tl::types::BotInfo {
            has_preview_medias: false,
            user_id: Some(3),
            description: None,
            description_photo: None,
            description_document: None,
            commands: Some(vec![tl::enums::BotCommand::Command(tl::types::BotCommand { ephemeral: false, command: "ghost".into(), description: "Nobody".into() })]),
            menu_button: None,
            privacy_policy_url: None,
            app_settings: None,
            verifier_settings: None,
        });
        let users = vec![tl::enums::User::Empty(tl::types::UserEmpty { id: 3 })];
        assert!(merge(&[info], &users).is_empty());
    }
}
