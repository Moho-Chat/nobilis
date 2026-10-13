//! Friend invite links: the one an account gives out, and the ones it is given.
//!
//! Steam's `s.team/p/<code>/<token>`. The code is the account's number written
//! in a way that is hard to misread - its hexadecimal digits mapped onto sixteen
//! letters that cannot be mistaken for each other or for a number - with a
//! hyphen in the middle. The token is a link of the account's own making: it
//! lets whoever has it add the account even if it takes no requests from
//! strangers, lasts a month, and can be revoked.
//!
//! The links are made, listed and revoked through Steam's website, as the
//! official client does; a link received is read for the account it is from and
//! turned into the ordinary request to be friends.

use super::link::Link;
use super::web;
use serde_json::Value;

/// The sixteen letters, in order of the hexadecimal digit each stands for.
const LETTERS: &[u8; 16] = b"bcdfghjkmnpqrtvw";

/// The code an account is known by in a link.
pub fn code_of(account: u32) -> String {
    let code: String = format!("{account:x}").bytes().map(|d| LETTERS[(d as char).to_digit(16).unwrap_or(0) as usize] as char).collect();
    if code.len() > 3 {
        let (a, b) = code.split_at(code.len() / 2);
        format!("{a}-{b}")
    } else {
        code
    }
}

/// The account a code is for, or nothing for text that is not one.
pub fn account_in(code: &str) -> Option<u32> {
    let letters: String = code.chars().filter(|c| *c != '-').collect::<String>().to_ascii_lowercase();
    if letters.is_empty() || letters.len() > 8 {
        return None;
    }
    let mut value: u64 = 0;
    for c in letters.bytes() {
        let digit = LETTERS.iter().position(|l| *l == c)? as u64;
        value = value * 16 + digit;
    }
    u32::try_from(value).ok().filter(|v| *v > 0)
}

/// The account a link or a bare code is from: `s.team/p/<code>[/<token>]`, a
/// profile address in the code's form, or the code on its own.
pub fn account_of_link(text: &str) -> Option<u32> {
    let text = text.trim().trim_end_matches('/');
    let rest = text.split_once("/p/").map(|(_, r)| r).or_else(|| text.split_once("/user/").map(|(_, r)| r)).unwrap_or(text);
    let code = rest.split(['/', '?', '#']).next()?;
    account_in(code)
}

/// A link an account has given out.
#[derive(Debug, PartialEq)]
pub struct InviteLink {
    pub token: String,
    pub link: String,
    pub valid: bool,
    pub created: u64,
    pub lasts_seconds: u64,
}

fn read_link(own_code: &str, entry: &Value) -> Option<InviteLink> {
    let token = entry["invite_token"].as_str().filter(|t| !t.is_empty())?.to_string();
    Some(InviteLink {
        link: format!("https://s.team/p/{own_code}/{token}"),
        valid: entry["valid"].as_i64().unwrap_or(0) != 0,
        created: entry["time_created"].as_u64().unwrap_or(0),
        lasts_seconds: entry["invite_duration"].as_str().and_then(|d| d.parse().ok()).or_else(|| entry["invite_duration"].as_u64()).unwrap_or(0),
        token,
    })
}

/// Reads the list the website gives: the ones still good, newest first.
pub fn read_links(own_account: u32, answer: &Value) -> Vec<InviteLink> {
    let code = code_of(own_account);
    let mut links: Vec<InviteLink> = answer["tokens"].as_array().into_iter().flatten().filter_map(|t| read_link(&code, t)).filter(|l| l.valid).collect();
    links.sort_by_key(|l| std::cmp::Reverse(l.created));
    links
}

async fn website(link: &Link, key: &str, steamid: u64, refresh_token: &str) -> Result<(reqwest::Client, String, String), String> {
    web::login(link, key, steamid, refresh_token).await
}

fn plain_json(raw: &[u8]) -> Value {
    serde_json::from_str(&web::plain(raw)).unwrap_or(Value::Null)
}

/// The account's own links that are still good.
pub async fn list(link: &Link, key: &str, steamid: u64, refresh_token: &str) -> Result<Vec<InviteLink>, String> {
    let (http, cookie, session) = website(link, key, steamid, refresh_token).await?;
    let response = http
        .get("https://steamcommunity.com/invites/ajaxgetall")
        .query(&[("sessionid", session.as_str())])
        .header("Cookie", cookie)
        .send()
        .await
        .map_err(|e| format!("could not reach Steam's website: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Steam's website refused it ({})", response.status()));
    }
    let answer = plain_json(&response.bytes().await.map_err(|e| e.to_string())?);
    Ok(read_links(super::steamid::account_of(steamid).unwrap_or_default(), &answer))
}

/// Makes a new link, good for a month.
pub async fn create(link: &Link, key: &str, steamid: u64, refresh_token: &str) -> Result<InviteLink, String> {
    let (http, cookie, session) = website(link, key, steamid, refresh_token).await?;
    let form = reqwest::multipart::Form::new().text("sessionid", session).text("steamid_user", steamid.to_string()).text("duration", "2592000");
    let response = http.post("https://steamcommunity.com/invites/ajaxcreate").header("Cookie", cookie).multipart(form).send().await.map_err(|e| format!("could not reach Steam's website: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Steam's website refused it ({})", response.status()));
    }
    let answer = plain_json(&response.bytes().await.map_err(|e| e.to_string())?);
    read_link(&code_of(super::steamid::account_of(steamid).unwrap_or_default()), &answer["invite"]).ok_or_else(|| "Steam did not make a link".to_string())
}

/// Takes a link back, so that it is no use to whoever has it.
pub async fn revoke(link: &Link, key: &str, steamid: u64, refresh_token: &str, token: &str) -> Result<(), String> {
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("that is not an invite link".to_string());
    }
    let (http, cookie, session) = website(link, key, steamid, refresh_token).await?;
    let form = reqwest::multipart::Form::new().text("sessionid", session).text("invite_token", token.to_string()).text("steamid_user", steamid.to_string());
    let response = http.post("https://steamcommunity.com/invites/ajaxrevoke").header("Cookie", cookie).multipart(form).send().await.map_err(|e| format!("could not reach Steam's website: {e}"))?;
    let answer = plain_json(&response.bytes().await.map_err(|e| e.to_string())?);
    if answer["success"].as_i64() == Some(1) {
        Ok(())
    } else {
        Err("Steam would not revoke that link".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_code_is_the_account_number_in_letters() {
        // Checked against Steam itself: these two are the codes of two real accounts.
        assert_eq!(code_of(664476208), "dknq-cpfb");
        assert_eq!(code_of(17167656), "cbh-whdm");
        assert_eq!(account_in("dknq-cpfb"), Some(664476208));
        assert_eq!(account_in("CBH-WHDM"), Some(17167656));
        assert_eq!(account_in("dknqcpfb"), Some(664476208), "the hyphen is for reading");
        for account in [1u32, 255, 4095, 22202, 664476208, u32::MAX] {
            assert_eq!(account_in(&code_of(account)), Some(account), "{account}");
        }
        assert_eq!(account_in(""), None);
        assert_eq!(account_in("abcdefgh"), None, "a, e and i are not among the letters");
        assert_eq!(account_in("bcdfghjkmnpq-rtvw1"), None, "too long to be a number of this kind");
    }

    #[test]
    fn a_link_is_read_for_the_account_it_is_from() {
        assert_eq!(account_of_link("https://s.team/p/dknq-cpfb/QWERTYUI"), Some(664476208));
        assert_eq!(account_of_link("https://s.team/p/dknq-cpfb"), Some(664476208));
        assert_eq!(account_of_link("http://s.team/p/dknq-cpfb/QWERTYUI/"), Some(664476208));
        assert_eq!(account_of_link("https://steamcommunity.com/user/cbh-whdm/"), Some(17167656));
        assert_eq!(account_of_link("dknq-cpfb"), Some(664476208));
        assert_eq!(account_of_link("https://example.com/not/this"), None);
    }

    #[test]
    fn the_links_an_account_has_are_read_newest_first_and_only_the_good_ones() {
        let answer = json!({ "success": 1, "tokens": [
            { "invite_token": "AAAAAAAA", "valid": 1, "time_created": 100, "invite_duration": "2592000" },
            { "invite_token": "BBBBBBBB", "valid": 0, "time_created": 300, "invite_duration": "2592000" },
            { "invite_token": "CCCCCCCC", "valid": 1, "time_created": 200, "invite_duration": "2592000" },
            { "valid": 1 }
        ] });
        let links = read_links(664476208, &answer);
        assert_eq!(links.iter().map(|l| l.token.as_str()).collect::<Vec<_>>(), ["CCCCCCCC", "AAAAAAAA"]);
        assert_eq!(links[0].link, "https://s.team/p/dknq-cpfb/CCCCCCCC");
        assert_eq!(links[0].lasts_seconds, 2592000);
    }
}
