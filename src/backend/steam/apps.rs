//! What a game is called.
//!
//! Steam tells a friend's client which game somebody is in by its number, and
//! names it only for a game that is not on Steam. The name of one that is is
//! Steam's own to say - the store has it - so it is asked for the first time a
//! number is seen and kept for as long as moho runs. A game is not asked about
//! twice, and one the store does not know stays a number: shown as "In game",
//! which is true, rather than as a guess.
//!
//! It is the account's request and goes the account's way: through its route.

use serde_json::Value;

/// How the store is asked.
const STORE: &str = "https://store.steampowered.com/api/appdetails";

/// The name out of the store's answer, which is keyed by the number asked for.
pub fn name_in(answer: &Value, app: u64) -> Option<String> {
    let entry = answer.get(app.to_string())?;
    if entry.get("success").and_then(|s| s.as_bool()) != Some(true) {
        return None;
    }
    let name = entry.get("data")?.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.chars().take(80).collect())
}

/// Asks the store what a game is called.
pub async fn lookup(key: &str, app: u64) -> Option<String> {
    let router = crate::net::route::router();
    if router.routed(key) && router.ready_for_general().await.is_err() {
        return None;
    }
    let http = router.client_if("steam", router.routed(key), |b| b.timeout(std::time::Duration::from_secs(15)).user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"))));
    let answer: Value = http.get(STORE).query(&[("appids", app.to_string()), ("filters", "basic".to_string())]).send().await.ok()?.json().await.ok()?;
    name_in(&answer, app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_name_is_read_out_of_the_stores_answer() {
        let said = json!({ "730": { "success": true, "data": { "type": "game", "name": "Counter-Strike 2", "steam_appid": 730 } } });
        assert_eq!(name_in(&said, 730).as_deref(), Some("Counter-Strike 2"));
        assert_eq!(name_in(&said, 731), None, "another game is not this one");
        assert_eq!(name_in(&json!({ "99": { "success": false } }), 99), None, "a game the store does not know has no name");
        assert_eq!(name_in(&json!({ "5": { "success": true, "data": { "name": "  " } } }), 5), None);
        assert_eq!(name_in(&json!([]), 5), None);
    }
}
