//! What a signed-in twitch.tv session can do in a channel that the official API cannot: see the channel
//! points balance, put points on a prediction, vote in a poll. (The bonus chest is deliberately not claimed from
//! here: it only appears to a viewer that reports watching, which moho does not do.)
//!
//! All of it is Twitch's private GraphQL (see gql.rs), and all of it as the person's own twitch.tv session - the
//! `auth-token` that signing in leaves with the account (see `playback`), since the token moho uses for chat
//! and the official API is not accepted there. Without that session these say so, in words, and do nothing.
//!
//! Nothing here runs by itself. A balance is read when the window shows the channel; a bet is placed and a vote
//! cast only when the person presses the button for it. The player's "watch time" reports,
//! which a points bot sends to earn faster, are not sent (see the plan, Gitea issue #55).

use super::gql;
use super::playback::{self, PlaybackError};
use crate::state::AppState;
use serde_json::{json, Value};

/// The balance in a channel.
#[derive(Debug, PartialEq, Clone)]
pub struct Points {
    pub balance: Option<u64>,
    /// What the channel calls its points, if it renamed them.
    pub name: Option<String>,
}

const CONTEXT: &str = "query($login:String!){ user(login:$login){ channel{ self{ communityPoints{ balance } } communityPointsSettings{ name } } } }";

pub fn read_points(answer: &Value) -> Option<Points> {
    let channel = answer.pointer("/data/user/channel").filter(|c| !c.is_null())?;
    let points = channel.pointer("/self/communityPoints").filter(|p| !p.is_null());
    Some(Points {
        balance: points.and_then(|p| p.get("balance")).and_then(|b| b.as_u64()),
        name: channel.pointer("/communityPointsSettings/name").and_then(|n| n.as_str()).filter(|n| !n.is_empty()).map(str::to_string),
    })
}

/// What spending on a reward does, which decides what it takes to redeem it.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Kind {
    /// One the streamer made: by id, with a word if it asks for one.
    Custom,
    /// Twitch's "Highlight My Message": the message is typed.
    Highlight,
    /// Twitch's "Unlock a Random Sub Emote".
    RandomEmote,
    /// "Choose an Emote to Unlock": one of the channel's subscriber emotes the account does not have.
    ChosenEmote,
    /// "Modify a Single Emote": one of the changed versions Twitch offers of a subscriber emote.
    ModifiedEmote,
    /// One that cannot be had from here (it is sent from twitch.tv's own chat box).
    Other,
}

/// One of the things a reward asks the person to pick from.
#[derive(Debug, PartialEq, Clone)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub image: Option<String>,
}

/// One thing a channel trades its points for.
#[derive(Debug, PartialEq, Clone)]
pub struct Reward {
    pub id: String,
    pub title: String,
    pub cost: u64,
    pub prompt: Option<String>,
    /// Whether it can be had right now: switched on, in stock and not paused.
    pub enabled: bool,
    pub paused: bool,
    /// It asks the person to type something.
    pub needs_input: bool,
    pub color: Option<String>,
    pub image: Option<String>,
    pub kind: Kind,
    /// What it asks the person to pick from, for the emote unlocks.
    pub choices: Vec<Choice>,
    /// Why it cannot be had from here, where that is known beforehand.
    pub unavailable: Option<String>,
}

const REWARDS: &str = "query($login:String!){ user(login:$login){ subscriptionProducts{ tier emotes{ id token } } channel{ id self{ communityPoints{ balance } } communityPointsSettings{ name customRewards{ id title cost prompt isEnabled isPaused isInStock isUserInputRequired backgroundColor image{ url } defaultImage{ url } } automaticRewards{ id type cost defaultCost isEnabled defaultBackgroundColor backgroundColor defaultImage{ url } } emoteVariants{ id modifications{ id } } } } } }";

fn emote_image(id: &str) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/1.0")
}

/// What a modification's two letters do to an emote, in words.
fn modification_name(id: &str) -> &'static str {
    match id.rsplit('_').next().unwrap_or("") {
        "BW" => "black and white",
        "HF" => "flipped",
        "SG" => "with sunglasses",
        "SQ" => "squished",
        "TK" => "thick",
        _ => "changed",
    }
}

/// A channel's rewards, its number, and this account's balance, from one answer. `owned` is the names of the
/// emotes the account can already use there, which an unlock does not offer again.
pub fn read_rewards(answer: &Value, owned: &std::collections::HashSet<String>) -> Option<(String, Vec<Reward>, Points)> {
    let channel = answer.pointer("/data/user/channel").filter(|c| !c.is_null())?;
    let flag = |r: &Value, k: &str, default: bool| r.get(k).and_then(|b| b.as_bool()).unwrap_or(default);
    let mut rewards: Vec<Reward> = channel
        .pointer("/communityPointsSettings/customRewards")
        .and_then(|r| r.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|r| {
                    Some(Reward {
                        id: r.get("id")?.as_str()?.to_string(),
                        title: r.get("title")?.as_str()?.to_string(),
                        cost: r.get("cost")?.as_u64()?,
                        prompt: r.get("prompt").and_then(|p| p.as_str()).filter(|p| !p.is_empty()).map(str::to_string),
                        enabled: flag(r, "isEnabled", false) && flag(r, "isInStock", true) && !flag(r, "isPaused", false),
                        paused: flag(r, "isPaused", false),
                        needs_input: flag(r, "isUserInputRequired", false),
                        color: r.get("backgroundColor").and_then(|c| c.as_str()).map(str::to_string),
                        image: r.pointer("/image/url").or_else(|| r.pointer("/defaultImage/url")).and_then(|u| u.as_str()).map(str::to_string),
                        kind: Kind::Custom,
                        choices: Vec::new(),
                        unavailable: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // The subscriber emotes of the channel, by id, for the unlocks that choose one.
    let sub_emotes: Vec<(String, String)> = answer
        .pointer("/data/user/subscriptionProducts")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .flat_map(|product| product.get("emotes").and_then(|e| e.as_array()).into_iter().flatten())
        .filter_map(|e| Some((e.get("id")?.as_str()?.to_string(), e.get("token")?.as_str()?.to_string())))
        .collect();
    let unowned: Vec<Choice> = sub_emotes.iter().filter(|(_, token)| !owned.contains(token)).map(|(id, token)| Choice { id: id.clone(), label: token.clone(), image: Some(emote_image(id)) }).collect();
    let variants: Vec<Choice> = channel
        .pointer("/communityPointsSettings/emoteVariants")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .flat_map(|v| {
            let base = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let token = sub_emotes.iter().find(|(id, _)| id == base).map(|(_, t)| t.clone()).unwrap_or_else(|| base.to_string());
            v.get("modifications").and_then(|m| m.as_array()).into_iter().flatten().filter_map(move |m| {
                let id = m.get("id")?.as_str()?.to_string();
                Some(Choice { label: format!("{token}, {}", modification_name(&id)), image: Some(emote_image(&id)), id })
            })
        })
        .collect();

    for r in channel.pointer("/communityPointsSettings/automaticRewards").and_then(|r| r.as_array()).into_iter().flatten() {
        let (Some(id), Some(kind)) = (r.get("id").and_then(|i| i.as_str()), r.get("type").and_then(|t| t.as_str())) else { continue };
        let cost = r.get("cost").and_then(|c| c.as_u64()).or_else(|| r.get("defaultCost").and_then(|c| c.as_u64())).unwrap_or(0);
        let (title, prompt, kind, needs_input, choices) = match kind {
            "SEND_HIGHLIGHTED_MESSAGE" => ("Highlight My Message", "Write something special and give it some glitz in chat", Kind::Highlight, true, Vec::new()),
            "RANDOM_SUB_EMOTE_UNLOCK" => ("Unlock a Random Sub Emote", "Unlocks a random subscriber emote you do not have", Kind::RandomEmote, false, Vec::new()),
            "CHOSEN_SUB_EMOTE_UNLOCK" => ("Choose an Emote to Unlock", "Pick a subscriber emote to unlock for 24 hours", Kind::ChosenEmote, false, unowned.clone()),
            "CHOSEN_MODIFIED_SUB_EMOTE_UNLOCK" => ("Modify a Single Emote", "Unlock a changed version of a subscriber emote", Kind::ModifiedEmote, false, variants.clone()),
            "SINGLE_MESSAGE_BYPASS_SUB_MODE" => ("Send a Message in Sub-Only Mode", "Send one message past subscribers-only mode", Kind::Other, false, Vec::new()),
            // Paid for in Bits and not in points: not for here.
            _ => continue,
        };
        let mut unavailable = None;
        if kind == Kind::Other {
            unavailable = Some("Send this one from twitch.tv's chat box".to_string());
        } else if (matches!(kind, Kind::ChosenEmote | Kind::ModifiedEmote) && choices.is_empty()) || (kind == Kind::RandomEmote && unowned.is_empty()) {
            unavailable = Some("There is nothing left here to unlock".to_string());
        }
        rewards.push(Reward {
            id: id.to_string(),
            title: title.to_string(),
            cost,
            prompt: Some(prompt.to_string()),
            enabled: flag(r, "isEnabled", false),
            paused: false,
            needs_input,
            color: r.get("backgroundColor").and_then(|c| c.as_str()).or_else(|| r.get("defaultBackgroundColor").and_then(|c| c.as_str())).map(str::to_string),
            image: r.pointer("/defaultImage/url").and_then(|u| u.as_str()).map(str::to_string),
            kind,
            choices,
            unavailable,
        });
    }
    Some((channel.get("id")?.as_str()?.to_string(), rewards, read_points(answer)?))
}

fn explain(e: PlaybackError) -> String {
    match e {
        PlaybackError::NeedsIntegrity => "Twitch wants proof this is a browser and moho could not make it. Try again in a moment".to_string(),
        other => other.to_string(),
    }
}

const NEEDS_SESSION: &str = "Sign in to Twitch from Accounts again: channel points are only available to the twitch.tv session a sign-in brings";

/// A fresh id for one vote or bet, so a request that is repeated is the same one and not a second. In the form
/// of a UUID, which is what Twitch's own page sends.
fn fresh_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// A mutation's own refusal, in words. Twitch answers with a code in the payload, not an HTTP error.
fn refusal(answer: &Value, field: &str) -> Option<String> {
    if let Some(said) = gql::error_of(answer) {
        return Some(match said.as_str() {
            "unauthenticated" => "Twitch no longer accepts the twitch.tv sign-in. Sign in to Twitch again from Accounts".to_string(),
            other => other.to_string(),
        });
    }
    let code = answer.pointer(&format!("/data/{field}/error/code")).and_then(|c| c.as_str())?;
    Some(match code {
        "NOT_ENOUGH_POINTS" | "INSUFFICIENT_FUNDS" => "Not enough channel points".to_string(),
        "ALREADY_VOTED" | "ALREADY_PREDICTED" => "You have already answered that".to_string(),
        "CANNOT_PREDICT_ON_BOTH_OUTCOMES" | "OTHER_OUTCOME_ALREADY_PREDICTED" => "You already put points on the other outcome".to_string(),
        "EVENT_NOT_ACTIVE" | "POLL_NOT_ACTIVE" | "PREDICTION_LOCKED" => "That is closed".to_string(),
        "GLOBAL_COOLDOWN" | "COOLDOWN" => "That reward is cooling down".to_string(),
        "MAX_PER_STREAM" | "MAX_PER_USER_PER_STREAM" | "MAX_REDEMPTIONS" => "That reward has been had as many times as this stream allows".to_string(),
        "REWARD_DISABLED" | "REWARD_NOT_ENABLED" | "REWARD_PAUSED" | "REWARD_OUT_OF_STOCK" => "That reward is not available right now".to_string(),
        "CHANNEL_NOT_LIVE" | "STREAM_NOT_LIVE" => "That reward can be had while the channel is live".to_string(),
        "NOT_FOUND" | "POLL_NOT_FOUND" | "EVENT_NOT_FOUND" | "CLAIM_NOT_FOUND" => "That is no longer there".to_string(),
        "MAX_POINTS_EXCEEDED" => "That is more than the channel allows on one prediction".to_string(),
        other => format!("Twitch refused: {}", other.to_lowercase().replace('_', " ")),
    })
}

fn token(web_token: Option<&str>) -> Result<&str, String> {
    web_token.filter(|t| !t.is_empty()).ok_or_else(|| NEEDS_SESSION.to_string())
}

/// The balance and any bonus waiting, for a channel by login.
pub async fn context(state: &AppState, http: &reqwest::Client, root: &str, login: &str, web_token: Option<&str>) -> Result<Points, String> {
    let token = token(web_token)?;
    let answer = playback::call_as_session(state, http, root, &gql::query(CONTEXT, json!({ "login": login })), token).await.map_err(explain)?;
    if let Some(said) = gql::error_of(&answer) {
        return Err(said);
    }
    read_points(&answer).ok_or_else(|| format!("Twitch has no channel points for {login}"))
}

/// Puts points on an outcome of a prediction.
pub async fn bet(state: &AppState, http: &reqwest::Client, root: &str, event_id: &str, outcome_id: &str, points: u64, web_token: Option<&str>) -> Result<(), String> {
    let token = token(web_token)?;
    const BET: &str = "mutation($i:MakePredictionInput!){ makePrediction(input:$i){ error{ code } } }";
    let body = gql::query(BET, json!({ "i": { "eventID": event_id, "outcomeID": outcome_id, "points": points, "transactionID": fresh_id() } }));
    let answer = playback::call_as_session(state, http, root, &body, token).await.map_err(explain)?;
    refusal(&answer, "makePrediction").map_or(Ok(()), Err)
}

/// Votes in a poll with the free vote everybody has.
pub async fn vote(state: &AppState, http: &reqwest::Client, root: &str, poll_id: &str, choice_id: &str, user_id: &str, web_token: Option<&str>) -> Result<(), String> {
    let token = token(web_token)?;
    const VOTE: &str = "mutation($i:VoteInPollInput!){ voteInPoll(input:$i){ error{ code } } }";
    let body = gql::query(VOTE, json!({ "i": { "pollID": poll_id, "choiceID": choice_id, "userID": user_id, "voteID": fresh_id() } }));
    let answer = playback::call_as_session(state, http, root, &body, token).await.map_err(explain)?;
    refusal(&answer, "voteInPoll").map_or(Ok(()), Err)
}

/// A channel's rewards and this account's balance: the panel over the chat lists them.
pub async fn rewards(state: &AppState, http: &reqwest::Client, root: &str, login: &str, owned: &std::collections::HashSet<String>, web_token: Option<&str>) -> Result<(String, Vec<Reward>, Points), String> {
    let token = token(web_token)?;
    let answer = playback::call_as_session(state, http, root, &gql::query(REWARDS, json!({ "login": login })), token).await.map_err(explain)?;
    if let Some(said) = gql::error_of(&answer) {
        return Err(said);
    }
    read_rewards(&answer, owned).ok_or_else(|| format!("Twitch has no channel points for {login}"))
}

/// Spends points on a reward. `input` is what a reward that asks for something was given.
pub async fn redeem(state: &AppState, http: &reqwest::Client, root: &str, channel_id: &str, reward: &Reward, input: Option<&str>, web_token: Option<&str>) -> Result<(), String> {
    let token = token(web_token)?;
    const REDEEM: &str = "mutation($i:RedeemCommunityPointsCustomRewardInput!){ redeemCommunityPointsCustomReward(input:$i){ error{ code } } }";
    let mut i = json!({ "channelID": channel_id, "cost": reward.cost, "prompt": reward.prompt, "rewardID": reward.id, "title": reward.title, "transactionID": fresh_id() });
    if let Some(text) = input.map(str::trim).filter(|t| !t.is_empty()) {
        i["textInput"] = json!(text);
    }
    let answer = playback::call_as_session(state, http, root, &gql::query(REDEEM, json!({ "i": i })), token).await.map_err(explain)?;
    refusal(&answer, "redeemCommunityPointsCustomReward").map_or(Ok(()), Err)
}

/// What the person gave a reward: the words typed for it, the emote picked for it.
#[derive(Default, Clone, Copy)]
pub struct Given<'a> {
    pub input: Option<&'a str>,
    pub choice: Option<&'a str>,
}

/// Spends points on one of Twitch's own rewards (a highlighted message, an emote unlock).
pub async fn redeem_automatic(state: &AppState, http: &reqwest::Client, root: &str, channel_id: &str, reward: &Reward, given: Given<'_>, web_token: Option<&str>) -> Result<(), String> {
    let (input, choice) = (given.input, given.choice);
    let token = token(web_token)?;
    let base = json!({ "channelID": channel_id, "cost": reward.cost, "transactionID": fresh_id() });
    let (name, input_type, mut i) = match reward.kind {
        Kind::Highlight => {
            let text = input.map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| "That reward asks you to type something".to_string())?;
            let mut i = base;
            i["message"] = json!(text);
            ("sendHighlightedChatMessage", "SendHighlightedChatMessageInput", i)
        }
        Kind::RandomEmote => ("unlockRandomSubscriberEmote", "UnlockRandomSubscriberEmoteInput", base),
        Kind::ChosenEmote | Kind::ModifiedEmote => {
            let emote = choice.filter(|c| reward.choices.iter().any(|o| o.id == *c)).ok_or_else(|| "Pick an emote first".to_string())?;
            let mut i = base;
            i["emoteID"] = json!(emote);
            if reward.kind == Kind::ChosenEmote {
                ("unlockChosenSubscriberEmote", "UnlockChosenSubscriberEmoteInput", i)
            } else {
                ("unlockChosenModifiedSubscriberEmote", "UnlockChosenModifiedSubscriberEmoteInput", i)
            }
        }
        _ => return Err(reward.unavailable.clone().unwrap_or_else(|| "That reward cannot be had from here".to_string())),
    };
    let mutation = format!("mutation($i:{input_type}!){{ {name}(input:$i){{ error{{ code }} }} }}");
    i["cost"] = json!(reward.cost);
    let answer = playback::call_as_session(state, http, root, &gql::query(&mutation, json!({ "i": i })), token).await.map_err(explain)?;
    refusal(&answer, name).map_or(Ok(()), Err)
}

// ---------------------------------------------------------------------------
// From a conversation: what the window's buttons call
// ---------------------------------------------------------------------------

/// The account behind a Twitch channel's conversation, and the channel's login.
fn session_of(state: &AppState, buffer_id: &str) -> Result<(crate::accounts::TwitchAccountConfig, String), String> {
    let buffer = state.runtime.get_buffer(buffer_id).filter(|b| b.account_id.starts_with("twitch:")).ok_or_else(|| "that is not a Twitch channel".to_string())?;
    let cfg = state.accounts.get_twitch(&buffer.account_id).ok_or_else(|| "no such Twitch account".to_string())?;
    Ok((cfg, buffer.name))
}

/// The id Twitch knows a card by, from the one the window knows it by (`poll:<id>`).
fn card_id(state: &AppState, buffer_id: &str, kind: &str) -> Result<(String, serde_json::Value), String> {
    let card = state.runtime.live_card(buffer_id, kind).ok_or_else(|| format!("there is no {kind} open here"))?;
    let id = card["id"].as_str().and_then(|i| i.strip_prefix(&format!("{kind}:"))).ok_or_else(|| format!("that {kind} has no id to answer"))?.to_string();
    Ok((id, card))
}

/// Votes in the poll on screen, and shows the vote on its card at once (the tally follows from Twitch).
pub async fn vote_in_buffer(state: &AppState, root: &str, buffer_id: &str, choice_id: &str) -> Result<(), String> {
    let (cfg, _) = session_of(state, buffer_id)?;
    let (poll_id, mut card) = card_id(state, buffer_id, "poll")?;
    let http = super::client().map_err(|e| format!("{e:#}"))?;
    vote(state, &http, root, &poll_id, choice_id, &cfg.user_id, Some(&cfg.web_token)).await?;
    card["hasVoted"] = json!(true);
    card["votedOptionId"] = json!(choice_id);
    crate::backend::kick::cards::publish_card(state, buffer_id, "poll", Some(card));
    Ok(())
}

/// Puts points on the prediction on screen, and shows the bet on its card at once.
pub async fn bet_in_buffer(state: &AppState, root: &str, buffer_id: &str, outcome_id: &str, amount: u64) -> Result<(), String> {
    let (cfg, channel) = session_of(state, buffer_id)?;
    let (event_id, mut card) = card_id(state, buffer_id, "prediction")?;
    if amount < super::realtime::MIN_BET as u64 {
        return Err(format!("Bets are {} points or more", super::realtime::MIN_BET));
    }
    if let Some(balance) = card["balance"].as_u64() {
        if amount > balance {
            return Err("Not enough channel points".to_string());
        }
    }
    let http = super::client().map_err(|e| format!("{e:#}"))?;
    bet(state, &http, root, &event_id, outcome_id, amount, Some(&cfg.web_token)).await?;
    let stake = card["stake"].as_f64().unwrap_or(0.0) + amount as f64;
    card["hasVoted"] = json!(true);
    card["votedOptionId"] = json!(outcome_id);
    card["stake"] = json!(stake);
    crate::backend::kick::cards::publish_card(state, buffer_id, "prediction", Some(card));
    // What is left, from Twitch rather than this one minus the bet: a refund or a cap would make that wrong.
    super::realtime::refresh_balance(state, root, &cfg.account_id(), &channel).await;
    Ok(())
}

/// The balance and any bonus waiting in a conversation's channel; also said to the window as `twitchPoints`.
pub async fn points_in_buffer(state: &AppState, root: &str, buffer_id: &str) -> Result<Points, String> {
    let (cfg, channel) = session_of(state, buffer_id)?;
    let http = super::client().map_err(|e| format!("{e:#}"))?;
    let points = context(state, &http, root, &channel, Some(&cfg.web_token)).await?;
    state.events.emit("twitchPoints", json!({ "bufferId": buffer_id, "points": points.balance, "name": points.name }));
    Ok(points)
}

/// The names of the emotes this account can already use in a conversation's channel.
fn owned_emotes(state: &AppState, account_id: &str, buffer_id: &str) -> std::collections::HashSet<String> {
    state.runtime.emoji_catalogue(account_id).into_iter().filter(|s| s.buffers.iter().any(|b| b == buffer_id)).flat_map(|s| s.emoji).map(|e| e.name).collect()
}

/// The rewards of a conversation's channel, and the balance, for the panel; the balance is also said to the window.
pub async fn rewards_in_buffer(state: &AppState, root: &str, buffer_id: &str) -> Result<(Vec<Reward>, Points), String> {
    let (cfg, channel) = session_of(state, buffer_id)?;
    let http = super::client().map_err(|e| format!("{e:#}"))?;
    let owned = owned_emotes(state, &cfg.account_id(), buffer_id);
    let (_, rewards, points) = rewards(state, &http, root, &channel, &owned, Some(&cfg.web_token)).await?;
    state.events.emit("twitchPoints", json!({ "bufferId": buffer_id, "points": points.balance, "name": points.name }));
    Ok((rewards, points))
}

/// Redeems a reward of a conversation's channel by its id, after checking it is there, is on offer, and is affordable,
/// and answers the balance after. An emote unlock also reads the account's emotes again, so the new one is in the picker.
pub async fn redeem_in_buffer(state: &AppState, root: &str, buffer_id: &str, reward_id: &str, input: Option<&str>, choice: Option<&str>) -> Result<Points, String> {
    let (cfg, channel) = session_of(state, buffer_id)?;
    let http = super::client().map_err(|e| format!("{e:#}"))?;
    let owned = owned_emotes(state, &cfg.account_id(), buffer_id);
    let (channel_id, rewards, points) = rewards(state, &http, root, &channel, &owned, Some(&cfg.web_token)).await?;
    let reward = rewards.into_iter().find(|r| r.id == reward_id).ok_or_else(|| "That reward is no longer there".to_string())?;
    if let Some(why) = &reward.unavailable {
        return Err(why.clone());
    }
    if !reward.enabled {
        return Err(if reward.paused { "That reward is paused right now".to_string() } else { "That reward is not available right now".to_string() });
    }
    if reward.needs_input && input.map(str::trim).unwrap_or("").is_empty() {
        return Err("That reward asks you to type something".to_string());
    }
    if let Some(balance) = points.balance {
        if balance < reward.cost {
            return Err(format!("You have {balance} of the {} points that costs", reward.cost));
        }
    }
    match reward.kind {
        Kind::Custom => redeem(state, &http, root, &channel_id, &reward, input, Some(&cfg.web_token)).await?,
        _ => redeem_automatic(state, &http, root, &channel_id, &reward, Given { input, choice }, Some(&cfg.web_token)).await?,
    }
    if matches!(reward.kind, Kind::RandomEmote | Kind::ChosenEmote | Kind::ModifiedEmote) {
        super::connect::refresh_user_emotes(state, &cfg.account_id(), &channel, &channel_id).await;
    }
    points_in_buffer(state, root, buffer_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_balance_and_the_bonus_are_read_and_a_signed_out_answer_has_neither() {
        let signed_in = json!({ "data": { "user": { "channel": { "self": { "communityPoints": { "balance": 4200 } }, "communityPointsSettings": { "name": "Sloths" } } } } });
        assert_eq!(read_points(&signed_in), Some(Points { balance: Some(4200), name: Some("Sloths".into()) }));
        let anonymous = json!({ "data": { "user": { "channel": { "self": { "communityPoints": null }, "communityPointsSettings": { "name": null } } } } });
        assert_eq!(read_points(&anonymous), Some(Points { balance: None, name: None }));
        assert_eq!(read_points(&json!({ "data": { "user": null } })), None);
    }

    #[test]
    fn a_refusal_is_said_in_words_whether_it_came_as_an_error_or_in_the_payload() {
        let payload = json!({ "data": { "makePrediction": { "error": { "code": "NOT_ENOUGH_POINTS" } } } });
        assert_eq!(refusal(&payload, "makePrediction").as_deref(), Some("Not enough channel points"));
        let auth = json!({ "errors": [{ "message": "unauthenticated" }], "data": { "makePrediction": null } });
        assert!(refusal(&auth, "makePrediction").unwrap().contains("Sign in to Twitch again"));
        let fine = json!({ "data": { "makePrediction": { "error": null } } });
        assert_eq!(refusal(&fine, "makePrediction"), None);
        let odd = json!({ "data": { "voteInPoll": { "error": { "code": "SOMETHING_NEW" } } } });
        assert_eq!(refusal(&odd, "voteInPoll").as_deref(), Some("Twitch refused: something new"));
    }

    #[test]
    fn an_id_for_a_vote_is_a_uuid_and_is_new_each_time() {
        let a = fresh_id();
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
        assert_eq!(&a[14..15], "4");
        assert_ne!(a, fresh_id());
    }

    use crate::accounts::TwitchAccountConfig;
    use crate::backend::kick::testkit;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A GraphQL that answers by what was asked, and keeps what it was sent (headers and body).
    async fn fake_gql(answer: impl Fn(&str) -> String + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let (log, answer) = (seen.clone(), Arc::new(answer));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (log, answer) = (log.clone(), answer.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let mut got = 0;
                    loop {
                        let n = sock.read(&mut buf[got..]).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        got += n;
                        let text = String::from_utf8_lossy(&buf[..got]).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text[..end].lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                            if got >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let request = String::from_utf8_lossy(&buf[..got]).to_string();
                    let body = answer(&request);
                    log.lock().unwrap().push(request);
                    let reply = format!("HTTP/1.1 200 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        (root, seen)
    }

    fn channel(name: &str, web_token: &str) -> (AppState, String) {
        let state = testkit::simulated_daemon(name);
        state.accounts.add_twitch(TwitchAccountConfig { login: "me".into(), user_id: "42".into(), access_token: "t".into(), refresh_token: "r".into(), expires_at: chrono::Utc::now().timestamp() + 9000, web_token: web_token.into(), ..Default::default() }).unwrap();
        state.runtime.ensure_buffer(&state, "twitch:me", "xqc", "channel");
        (state, "twitch:me|xqc".to_string())
    }

    #[tokio::test]
    async fn a_vote_is_sent_as_the_twitch_session_and_shows_on_the_card() {
        let (state, buffer) = channel("twitch-points-vote", "websession");
        state.runtime.set_live_card(&buffer, "poll", json!({ "id": "poll:q1", "kind": "poll", "options": [] }));
        let (root, seen) = fake_gql(|_| r#"{"data":{"voteInPoll":{"error":null}}}"#.to_string()).await;
        vote_in_buffer(&state, &root, &buffer, "choice-x").await.unwrap();
        let request = seen.lock().unwrap()[0].clone();
        assert!(request.to_lowercase().contains("authorization: oauth websession"), "{request}");
        assert!(request.contains("\"pollID\":\"q1\"") && request.contains("\"choiceID\":\"choice-x\"") && request.contains("\"userID\":\"42\""), "{request}");
        assert!(request.contains("voteID"), "{request}");
        let card = state.runtime.live_card(&buffer, "poll").unwrap();
        assert_eq!((card["hasVoted"].as_bool(), card["votedOptionId"].as_str()), (Some(true), Some("choice-x")));
    }

    #[tokio::test]
    async fn a_refusal_is_said_and_the_card_is_left_alone() {
        let (state, buffer) = channel("twitch-points-refused", "websession");
        state.runtime.set_live_card(&buffer, "poll", json!({ "id": "poll:q1", "kind": "poll", "options": [] }));
        let (root, _) = fake_gql(|_| r#"{"data":{"voteInPoll":{"error":{"code":"ALREADY_VOTED"}}}}"#.to_string()).await;
        assert_eq!(vote_in_buffer(&state, &root, &buffer, "x").await.unwrap_err(), "You have already answered that");
        assert!(state.runtime.live_card(&buffer, "poll").unwrap()["hasVoted"].is_null());
        // And no session, no call.
        let (state, buffer) = channel("twitch-points-nosession", "");
        state.runtime.set_live_card(&buffer, "poll", json!({ "id": "poll:q1", "kind": "poll" }));
        let e = vote_in_buffer(&state, &root, &buffer, "x").await.unwrap_err();
        assert!(e.contains("Sign in to Twitch from Accounts again"), "{e}");
    }

    #[tokio::test]
    async fn a_bet_checks_its_size_places_it_and_asks_what_is_left() {
        let (state, buffer) = channel("twitch-points-bet", "websession");
        state.runtime.set_live_card(&buffer, "prediction", json!({ "id": "prediction:e1", "kind": "prediction", "balance": 500, "options": [] }));
        let (root, seen) = fake_gql(|req| {
            if req.contains("makePrediction") {
                r#"{"data":{"makePrediction":{"error":null}}}"#.to_string()
            } else {
                r#"{"data":{"user":{"channel":{"self":{"communityPoints":{"balance":400}},"communityPointsSettings":{"name":null}}}}}"#.to_string()
            }
        })
        .await;
        assert!(bet_in_buffer(&state, &root, &buffer, "o1", 5).await.unwrap_err().contains("10 points or more"));
        assert_eq!(bet_in_buffer(&state, &root, &buffer, "o1", 900).await.unwrap_err(), "Not enough channel points");
        assert!(seen.lock().unwrap().is_empty(), "a bet that cannot be placed is not sent");
        bet_in_buffer(&state, &root, &buffer, "o1", 100).await.unwrap();
        let requests = seen.lock().unwrap().clone();
        assert!(requests[0].contains("\"eventID\":\"e1\"") && requests[0].contains("\"outcomeID\":\"o1\"") && requests[0].contains("\"points\":100") && requests[0].contains("transactionID"), "{}", requests[0]);
        let card = state.runtime.live_card(&buffer, "prediction").unwrap();
        assert_eq!((card["votedOptionId"].as_str(), card["stake"].as_f64(), card["balance"].as_u64()), (Some("o1"), Some(100.0), Some(400)));
    }

    /// Against the real Twitch, with nothing at stake: reads a balance, and sends a vote and a bet that
    /// name things that do not exist, which Twitch refuses in words. Shows the session is accepted and the
    /// requests are shaped as it wants. `TWITCH_LIVE_WEB_TOKEN=... TWITCH_LIVE_USER_ID=... TWITCH_LIVE_CHANNEL=...
    /// cargo test --release live_ -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_the_session_reads_a_balance_and_twitch_refuses_what_does_not_exist() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (token, user, channel) = (std::env::var("TWITCH_LIVE_WEB_TOKEN").unwrap(), std::env::var("TWITCH_LIVE_USER_ID").unwrap(), std::env::var("TWITCH_LIVE_CHANNEL").unwrap());
        let state = testkit::simulated_daemon("twitch-points-live");
        let http = crate::backend::twitch::client().unwrap();
        let root = gql::ROOT;
        // A real prediction and outcome (from `TWITCH_LIVE_EVENT` and `TWITCH_LIVE_OUTCOME`), with more points than
        // the account has: Twitch refuses it for want of points, which proves the bet is understood and spends nothing.
        if let (Ok(event), Ok(outcome)) = (std::env::var("TWITCH_LIVE_EVENT"), std::env::var("TWITCH_LIVE_OUTCOME")) {
            let amount: u64 = std::env::var("TWITCH_LIVE_BET").ok().and_then(|a| a.parse().ok()).unwrap_or(10);
            println!("a bet of {amount} on a real prediction: {:?}", bet(&state, &http, root, &event, &outcome, amount, Some(&token)).await);
        }
        println!("balance: {:?}", context(&state, &http, root, &channel, Some(&token)).await);
        // The channel's rewards, and a redeem of one that does not exist (nothing is spent).
        match rewards(&state, &http, root, &channel, &Default::default(), Some(&token)).await {
            Ok((channel_id, list, points)) => {
                println!("rewards: {} offered, balance {:?}; first: {:?}", list.len(), points.balance, list.first().map(|r| (&r.title, r.cost, r.enabled)));
                for r in list.iter().filter(|r| r.kind != Kind::Custom) {
                    println!("  own reward: {} | {} | enabled {} | {} choices | {:?}", r.title, r.cost, r.enabled, r.choices.len(), r.unavailable);
                }
                let fake = Reward { id: "00000000-0000-0000-0000-000000000000".into(), title: "nothing".into(), cost: 10, prompt: None, enabled: true, paused: false, needs_input: false, color: None, image: None, kind: Kind::Custom, choices: Vec::new(), unavailable: None };
                println!("redeem a reward that is not there: {:?}", redeem(&state, &http, root, &channel_id, &fake, None, Some(&token)).await);
            }
            Err(e) => println!("rewards: {e}"),
        }
        println!("vote in a poll that is not there: {:?}", vote(&state, &http, root, "00000000-0000-0000-0000-000000000000", "00000000-0000-0000-0000-000000000001", &user, Some(&token)).await);
        println!("bet on a prediction that is not there: {:?}", bet(&state, &http, root, "00000000-0000-0000-0000-000000000000", "00000000-0000-0000-0000-000000000001", 10, Some(&token)).await);
    }

    const REWARDS_ANSWER: &str = r##"{"data":{"user":{"subscriptionProducts":[{"tier":"1000","emotes":[{"id":"e1","token":"binxbWut"},{"id":"e2","token":"binxbYAP"},{"id":"e3","token":"binxbDrama"}]}],"channel":{"id":"71","self":{"communityPoints":{"balance":600}},"communityPointsSettings":{"name":"Sloths","customRewards":[
        {"id":"r1","title":"Drink water","cost":500,"prompt":null,"isEnabled":true,"isPaused":false,"isInStock":true,"isUserInputRequired":false,"backgroundColor":"#6BB7FA","image":null,"defaultImage":{"url":"https://img/default.png"}},
        {"id":"r2","title":"Say a name","cost":100,"prompt":"Who?","isEnabled":true,"isPaused":false,"isInStock":true,"isUserInputRequired":true,"backgroundColor":"#E37C10","image":{"url":"https://img/own.png"},"defaultImage":null},
        {"id":"r3","title":"Dance","cost":50,"prompt":null,"isEnabled":true,"isPaused":true,"isInStock":true,"isUserInputRequired":false,"backgroundColor":null,"image":null,"defaultImage":null},
        {"id":"r4","title":"Costly","cost":9000,"prompt":null,"isEnabled":true,"isPaused":false,"isInStock":true,"isUserInputRequired":false,"backgroundColor":null,"image":null,"defaultImage":null}],"automaticRewards":[
        {"id":"71:SEND_HIGHLIGHTED_MESSAGE","type":"SEND_HIGHLIGHTED_MESSAGE","cost":null,"defaultCost":100,"isEnabled":true,"defaultBackgroundColor":"#FF6905","backgroundColor":null,"defaultImage":{"url":"https://img/hl.png"}},
        {"id":"71:RANDOM_SUB_EMOTE_UNLOCK","type":"RANDOM_SUB_EMOTE_UNLOCK","cost":null,"defaultCost":320,"isEnabled":true,"defaultBackgroundColor":"#8205B4","backgroundColor":null,"defaultImage":null},
        {"id":"71:CHOSEN_SUB_EMOTE_UNLOCK","type":"CHOSEN_SUB_EMOTE_UNLOCK","cost":null,"defaultCost":480,"isEnabled":true,"defaultBackgroundColor":"#00C8AF","backgroundColor":null,"defaultImage":null},
        {"id":"71:CHOSEN_MODIFIED_SUB_EMOTE_UNLOCK","type":"CHOSEN_MODIFIED_SUB_EMOTE_UNLOCK","cost":900,"defaultCost":900,"isEnabled":true,"defaultBackgroundColor":"#00FA05","backgroundColor":null,"defaultImage":null},
        {"id":"71:SINGLE_MESSAGE_BYPASS_SUB_MODE","type":"SINGLE_MESSAGE_BYPASS_SUB_MODE","cost":350,"defaultCost":350,"isEnabled":true,"defaultBackgroundColor":"#9146FF","backgroundColor":null,"defaultImage":null},
        {"id":"71:SEND_GIGANTIFIED_EMOTE","type":"SEND_GIGANTIFIED_EMOTE","cost":null,"defaultCost":0,"isEnabled":true,"defaultBackgroundColor":"#69B6FF","backgroundColor":null,"defaultImage":null}],
        "emoteVariants":[{"id":"e3","modifications":[{"id":"e3_BW"},{"id":"e3_SG"}]}]}}}}}"##;

    #[test]
    fn a_channels_rewards_are_read_with_what_makes_each_available() {
        let (channel, rewards, points) = read_rewards(&serde_json::from_str(REWARDS_ANSWER).unwrap(), &Default::default()).unwrap();
        assert_eq!((channel.as_str(), points.balance, points.name.as_deref()), ("71", Some(600), Some("Sloths")));
        let custom: Vec<(&str, u64, bool, bool, bool)> = rewards.iter().filter(|r| r.kind == Kind::Custom).map(|r| (r.id.as_str(), r.cost, r.enabled, r.paused, r.needs_input)).collect();
        assert_eq!(custom, vec![("r1", 500, true, false, false), ("r2", 100, true, false, true), ("r3", 50, false, true, false), ("r4", 9000, true, false, false)]);
        assert_eq!(rewards[0].image.as_deref(), Some("https://img/default.png"));
        assert_eq!(rewards[1].image.as_deref(), Some("https://img/own.png"));
        assert_eq!(rewards[1].prompt.as_deref(), Some("Who?"));
    }

    #[test]
    fn twitchs_own_rewards_are_listed_with_what_each_takes_and_the_bits_ones_are_left_out() {
        let owned: std::collections::HashSet<String> = ["binxbWut".to_string()].into_iter().collect();
        let (_, rewards, _) = read_rewards(&serde_json::from_str(REWARDS_ANSWER).unwrap(), &owned).unwrap();
        let auto: Vec<&Reward> = rewards.iter().filter(|r| r.kind != Kind::Custom).collect();
        let kinds: Vec<(Kind, u64, bool)> = auto.iter().map(|r| (r.kind, r.cost, r.needs_input)).collect();
        // Highlight (typed), random, chosen, modified, and the one that is sent from twitch.tv; the Bits one is not here.
        assert_eq!(kinds, vec![(Kind::Highlight, 100, true), (Kind::RandomEmote, 320, false), (Kind::ChosenEmote, 480, false), (Kind::ModifiedEmote, 900, false), (Kind::Other, 350, false)]);
        // What the account already has is not offered again; a change is offered for each version.
        let chosen: Vec<&str> = auto[2].choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(chosen, vec!["binxbYAP", "binxbDrama"]);
        let modified: Vec<&str> = auto[3].choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(modified, vec!["binxbDrama, black and white", "binxbDrama, with sunglasses"]);
        assert_eq!(auto[4].unavailable.as_deref(), Some("Send this one from twitch.tv's chat box"));
        // With every emote owned there is nothing to unlock, which is said.
        let all: std::collections::HashSet<String> = ["binxbWut", "binxbYAP", "binxbDrama"].iter().map(|s| s.to_string()).collect();
        let (_, none_left, _) = read_rewards(&serde_json::from_str(REWARDS_ANSWER).unwrap(), &all).unwrap();
        let chosen_none = none_left.iter().find(|r| r.kind == Kind::ChosenEmote).unwrap();
        assert_eq!(chosen_none.unavailable.as_deref(), Some("There is nothing left here to unlock"));
    }

    #[tokio::test]
    async fn the_automatic_rewards_are_redeemed_through_their_own_calls_with_what_each_needs() {
        let (state, buffer) = channel("twitch-points-auto", "websession");
        let (root, seen) = fake_gql(|req| {
            if req.contains("\"mutation(") || req.contains("mutation($i") {
                r#"{"data":{"x":{"error":null}}}"#.replace("\"x\"", if req.contains("sendHighlightedChatMessage") { "\"sendHighlightedChatMessage\"" } else if req.contains("unlockRandomSubscriberEmote") { "\"unlockRandomSubscriberEmote\"" } else if req.contains("unlockChosenModifiedSubscriberEmote") { "\"unlockChosenModifiedSubscriberEmote\"" } else { "\"unlockChosenSubscriberEmote\"" })
            } else {
                REWARDS_ANSWER.to_string()
            }
        })
        .await;
        redeem_in_buffer(&state, &root, &buffer, "71:SEND_HIGHLIGHTED_MESSAGE", Some("look at me"), None).await.unwrap();
        redeem_in_buffer(&state, &root, &buffer, "71:RANDOM_SUB_EMOTE_UNLOCK", None, None).await.unwrap();
        redeem_in_buffer(&state, &root, &buffer, "71:CHOSEN_SUB_EMOTE_UNLOCK", None, Some("e2")).await.unwrap();
        redeem_in_buffer(&state, &root, &buffer, "71:CHOSEN_MODIFIED_SUB_EMOTE_UNLOCK", None, Some("e3_SG")).await.unwrap_err();
        let requests = seen.lock().unwrap().clone();
        let sent = |name: &str| requests.iter().find(|r| r.contains(name) && r.contains("transactionID")).cloned().unwrap_or_default();
        let highlight = sent("sendHighlightedChatMessage");
        for part in ["\"channelID\":\"71\"", "\"cost\":100", "\"message\":\"look at me\"", "SendHighlightedChatMessageInput"] {
            assert!(highlight.contains(part), "{part} in {highlight}");
        }
        assert!(sent("unlockRandomSubscriberEmote").contains("\"cost\":320"));
        let chosen = sent("unlockChosenSubscriberEmote");
        assert!(chosen.contains("\"emoteID\":\"e2\"") && chosen.contains("\"cost\":480"), "{chosen}");
        // An emote that was not offered, and a message that was not typed, are stopped before anything is sent.
        let before = requests.iter().filter(|r| r.contains("transactionID")).count();
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "71:CHOSEN_SUB_EMOTE_UNLOCK", None, Some("not-offered")).await.unwrap_err(), "Pick an emote first");
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "71:SEND_HIGHLIGHTED_MESSAGE", Some(" "), None).await.unwrap_err(), "That reward asks you to type something");
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "71:SINGLE_MESSAGE_BYPASS_SUB_MODE", None, None).await.unwrap_err(), "Send this one from twitch.tv's chat box");
        assert_eq!(seen.lock().unwrap().iter().filter(|r| r.contains("transactionID")).count(), before);
    }

    #[tokio::test]
    async fn a_reward_is_redeemed_with_what_twitch_needs_and_a_refusal_is_stopped_before_it_is_sent() {
        let (state, buffer) = channel("twitch-points-redeem", "websession");
        let asked = Arc::new(Mutex::new(0));
        let counter = asked.clone();
        let (root, seen) = fake_gql(move |req| {
            if req.contains("redeemCommunityPointsCustomReward") {
                return r#"{"data":{"redeemCommunityPointsCustomReward":{"error":null}}}"#.to_string();
            }
            let mut n = counter.lock().unwrap();
            *n += 1;
            if *n <= 4 { REWARDS_ANSWER.to_string() } else { REWARDS_ANSWER.replace("\"balance\":600", "\"balance\":500") }
        })
        .await;
        // Paused, too dear, asks for a word and was not given one, and not there: each said, none sent.
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "r3", None, None).await.unwrap_err(), "That reward is paused right now");
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "r4", None, None).await.unwrap_err(), "You have 600 of the 9000 points that costs");
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "r2", Some("  "), None).await.unwrap_err(), "That reward asks you to type something");
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "nope", None, None).await.unwrap_err(), "That reward is no longer there");
        assert!(!seen.lock().unwrap().iter().any(|r| r.contains("redeemCommunityPointsCustomReward")), "nothing was spent");
        // One that is on offer, with what it asks for.
        let after = redeem_in_buffer(&state, &root, &buffer, "r2", Some("Greg"), None).await.unwrap();
        assert_eq!(after.balance, Some(500));
        let request = seen.lock().unwrap().iter().find(|r| r.contains("redeemCommunityPointsCustomReward")).cloned().unwrap();
        for part in ["\"channelID\":\"71\"", "\"rewardID\":\"r2\"", "\"cost\":100", "\"title\":\"Say a name\"", "\"prompt\":\"Who?\"", "\"textInput\":\"Greg\"", "transactionID"] {
            assert!(request.contains(part), "{part} in {request}");
        }
        assert!(request.to_lowercase().contains("authorization: oauth websession"));
    }
}
