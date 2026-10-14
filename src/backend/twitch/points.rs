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
}

const REWARDS: &str = "query($login:String!){ user(login:$login){ channel{ id self{ communityPoints{ balance } } communityPointsSettings{ name customRewards{ id title cost prompt isEnabled isPaused isInStock isUserInputRequired backgroundColor image{ url } defaultImage{ url } } } } } }";

/// A channel's rewards, its number, and this account's balance, from one answer.
pub fn read_rewards(answer: &Value) -> Option<(String, Vec<Reward>, Points)> {
    let channel = answer.pointer("/data/user/channel").filter(|c| !c.is_null())?;
    let rewards = channel
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
                        enabled: r.get("isEnabled").and_then(|b| b.as_bool()).unwrap_or(false) && r.get("isInStock").and_then(|b| b.as_bool()).unwrap_or(true) && !r.get("isPaused").and_then(|b| b.as_bool()).unwrap_or(false),
                        paused: r.get("isPaused").and_then(|b| b.as_bool()).unwrap_or(false),
                        needs_input: r.get("isUserInputRequired").and_then(|b| b.as_bool()).unwrap_or(false),
                        color: r.get("backgroundColor").and_then(|c| c.as_str()).map(str::to_string),
                        image: r.pointer("/image/url").or_else(|| r.pointer("/defaultImage/url")).and_then(|u| u.as_str()).map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
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
pub async fn rewards(state: &AppState, http: &reqwest::Client, root: &str, login: &str, web_token: Option<&str>) -> Result<(String, Vec<Reward>, Points), String> {
    let token = token(web_token)?;
    let answer = playback::call_as_session(state, http, root, &gql::query(REWARDS, json!({ "login": login })), token).await.map_err(explain)?;
    if let Some(said) = gql::error_of(&answer) {
        return Err(said);
    }
    read_rewards(&answer).ok_or_else(|| format!("Twitch has no channel points for {login}"))
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

/// The rewards of a conversation's channel, and the balance, for the panel; the balance is also said to the window.
pub async fn rewards_in_buffer(state: &AppState, root: &str, buffer_id: &str) -> Result<(Vec<Reward>, Points), String> {
    let (cfg, channel) = session_of(state, buffer_id)?;
    let http = super::client().map_err(|e| format!("{e:#}"))?;
    let (_, rewards, points) = rewards(state, &http, root, &channel, Some(&cfg.web_token)).await?;
    state.events.emit("twitchPoints", json!({ "bufferId": buffer_id, "points": points.balance, "name": points.name }));
    Ok((rewards, points))
}

/// Redeems a reward of a conversation's channel by its id, after checking it is there, is on offer, and is affordable,
/// and answers the balance after.
pub async fn redeem_in_buffer(state: &AppState, root: &str, buffer_id: &str, reward_id: &str, input: Option<&str>) -> Result<Points, String> {
    let (cfg, channel) = session_of(state, buffer_id)?;
    let http = super::client().map_err(|e| format!("{e:#}"))?;
    let (channel_id, rewards, points) = rewards(state, &http, root, &channel, Some(&cfg.web_token)).await?;
    let reward = rewards.into_iter().find(|r| r.id == reward_id).ok_or_else(|| "That reward is no longer there".to_string())?;
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
    redeem(state, &http, root, &channel_id, &reward, input, Some(&cfg.web_token)).await?;
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
        match rewards(&state, &http, root, &channel, Some(&token)).await {
            Ok((channel_id, list, points)) => {
                println!("rewards: {} offered, balance {:?}; first: {:?}", list.len(), points.balance, list.first().map(|r| (&r.title, r.cost, r.enabled)));
                let fake = Reward { id: "00000000-0000-0000-0000-000000000000".into(), title: "nothing".into(), cost: 10, prompt: None, enabled: true, paused: false, needs_input: false, color: None, image: None };
                println!("redeem a reward that is not there: {:?}", redeem(&state, &http, root, &channel_id, &fake, None, Some(&token)).await);
            }
            Err(e) => println!("rewards: {e}"),
        }
        println!("vote in a poll that is not there: {:?}", vote(&state, &http, root, "00000000-0000-0000-0000-000000000000", "00000000-0000-0000-0000-000000000001", &user, Some(&token)).await);
        println!("bet on a prediction that is not there: {:?}", bet(&state, &http, root, "00000000-0000-0000-0000-000000000000", "00000000-0000-0000-0000-000000000001", 10, Some(&token)).await);
    }

    const REWARDS_ANSWER: &str = r##"{"data":{"user":{"channel":{"id":"71","self":{"communityPoints":{"balance":600}},"communityPointsSettings":{"name":"Sloths","customRewards":[
        {"id":"r1","title":"Drink water","cost":500,"prompt":null,"isEnabled":true,"isPaused":false,"isInStock":true,"isUserInputRequired":false,"backgroundColor":"#6BB7FA","image":null,"defaultImage":{"url":"https://img/default.png"}},
        {"id":"r2","title":"Say a name","cost":100,"prompt":"Who?","isEnabled":true,"isPaused":false,"isInStock":true,"isUserInputRequired":true,"backgroundColor":"#E37C10","image":{"url":"https://img/own.png"},"defaultImage":null},
        {"id":"r3","title":"Dance","cost":50,"prompt":null,"isEnabled":true,"isPaused":true,"isInStock":true,"isUserInputRequired":false,"backgroundColor":null,"image":null,"defaultImage":null},
        {"id":"r4","title":"Costly","cost":9000,"prompt":null,"isEnabled":true,"isPaused":false,"isInStock":true,"isUserInputRequired":false,"backgroundColor":null,"image":null,"defaultImage":null}]}}}}}"##;

    #[test]
    fn a_channels_rewards_are_read_with_what_makes_each_available() {
        let (channel, rewards, points) = read_rewards(&serde_json::from_str(REWARDS_ANSWER).unwrap()).unwrap();
        assert_eq!((channel.as_str(), points.balance, points.name.as_deref()), ("71", Some(600), Some("Sloths")));
        let ids: Vec<(&str, u64, bool, bool, bool)> = rewards.iter().map(|r| (r.id.as_str(), r.cost, r.enabled, r.paused, r.needs_input)).collect();
        assert_eq!(ids, vec![("r1", 500, true, false, false), ("r2", 100, true, false, true), ("r3", 50, false, true, false), ("r4", 9000, true, false, false)]);
        assert_eq!(rewards[0].image.as_deref(), Some("https://img/default.png"));
        assert_eq!(rewards[1].image.as_deref(), Some("https://img/own.png"));
        assert_eq!(rewards[1].prompt.as_deref(), Some("Who?"));
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
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "r3", None).await.unwrap_err(), "That reward is paused right now");
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "r4", None).await.unwrap_err(), "You have 600 of the 9000 points that costs");
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "r2", Some("  ")).await.unwrap_err(), "That reward asks you to type something");
        assert_eq!(redeem_in_buffer(&state, &root, &buffer, "nope", None).await.unwrap_err(), "That reward is no longer there");
        assert!(!seen.lock().unwrap().iter().any(|r| r.contains("redeemCommunityPointsCustomReward")), "nothing was spent");
        // One that is on offer, with what it asks for.
        let after = redeem_in_buffer(&state, &root, &buffer, "r2", Some("Greg")).await.unwrap();
        assert_eq!(after.balance, Some(500));
        let request = seen.lock().unwrap().iter().find(|r| r.contains("redeemCommunityPointsCustomReward")).cloned().unwrap();
        for part in ["\"channelID\":\"71\"", "\"rewardID\":\"r2\"", "\"cost\":100", "\"title\":\"Say a name\"", "\"prompt\":\"Who?\"", "\"textInput\":\"Greg\"", "transactionID"] {
            assert!(request.contains(part), "{part} in {request}");
        }
        assert!(request.to_lowercase().contains("authorization: oauth websession"));
    }
}
