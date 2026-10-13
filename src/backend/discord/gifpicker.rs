//! The GIFs Discord offers, for browsing.
//!
//! Discord's own client gets its GIFs from the same two requests - what is
//! trending, and a search - and shows what comes back; this asks for the same
//! and does no more than the official client does: reads, and only when the
//! picker is open and somebody has stopped typing.
//!
//! Any Discord account will do, whichever conversation the picker is open in,
//! since what it borrows is the account's way of asking and not the account's
//! conversations: a GIF found this way can be sent anywhere.
//!
//! Answers are kept for a few minutes. Opening the picker twice, or typing a
//! word and deleting it, is not a reason to ask again.

use super::*;
use std::sync::LazyLock;
use std::time::Instant;

static CACHE: LazyLock<std::sync::Mutex<HashMap<String, (Instant, Value)>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
const TRENDING_FOR: Duration = Duration::from_secs(10 * 60);
const SEARCH_FOR: Duration = Duration::from_secs(5 * 60);
const CACHE_LIMIT: usize = 64;

/// One GIF as the picker wants it: where it is, what it is called, and what to
/// send. Discord's own words for these are `src` for the video, `gif_src` for
/// the file, `url` for the page - kept, since each is used for something.
fn one(item: &Value) -> Option<Value> {
    let url = item["url"].as_str().filter(|u| !u.is_empty())?;
    let src = item["src"].as_str().filter(|u| !u.is_empty());
    let gif = item["gif_src"].as_str().filter(|u| !u.is_empty());
    // Something to play, or there is nothing to show.
    let play = src.or(gif)?;
    Some(json!({
        "id": item["id"].as_str().map(str::to_string).unwrap_or_else(|| url.to_string()),
        "title": item["title"].as_str().unwrap_or(""),
        "url": url,
        "src": play,
        "gifSrc": gif.unwrap_or(play),
        "preview": item["preview"].as_str().unwrap_or(""),
        "width": item["width"].as_u64().unwrap_or(0),
        "height": item["height"].as_u64().unwrap_or(0),
    }))
}

/// Discord's two shapes of answer: a search is a list of GIFs, and trending is
/// an object holding them with the categories beside.
pub(super) fn parse_gifs(answer: &Value) -> Value {
    let list = answer
        .as_array()
        .or_else(|| answer["gifs"].as_array())
        .or_else(|| answer["results"].as_array());
    let gifs: Vec<Value> = list.into_iter().flatten().filter_map(one).collect();
    let categories: Vec<Value> = answer["categories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = c["name"].as_str().filter(|n| !n.is_empty())?;
            Some(json!({ "name": name, "src": c["src"].as_str().unwrap_or("") }))
        })
        .collect();
    json!({ "gifs": gifs, "categories": categories })
}

/// What is trending, or what matches `query`.
pub async fn list_gifs(state: &AppState, query: &str) -> Result<Value> {
    let query = query.trim();
    let key = query.to_lowercase();
    let ttl = if key.is_empty() {
        TRENDING_FOR
    } else {
        SEARCH_FOR
    };
    if let Some((at, answer)) = CACHE.lock().unwrap().get(&key) {
        if at.elapsed() < ttl {
            return Ok(answer.clone());
        }
    }

    let config = state
        .accounts
        .all_discord()
        .into_iter()
        .next()
        .context("GIFs are found through a Discord account, and none is set up")?;
    let mut url = url::Url::parse(&format!(
        "{API_BASE}/gifs/{}",
        if query.is_empty() {
            "trending"
        } else {
            "search"
        }
    ))
    .context("building the GIF request")?;
    {
        let mut pairs = url.query_pairs_mut();
        if !query.is_empty() {
            pairs.append_pair("q", &query.chars().take(100).collect::<String>());
        }
        pairs
            .append_pair("media_format", "mp4")
            .append_pair("locale", "en-US");
    }
    let resp = http_client_for(&config.token)
        .get(url)
        .header("Authorization", &config.token)
        .send()
        .await
        .context("asking Discord for GIFs")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "{}",
            discord_error_text(status, &text, "asking Discord for GIFs")
        );
    }
    let answer: Value = resp.json().await.context("reading Discord's GIFs")?;
    let parsed = parse_gifs(&answer);

    let mut cache = CACHE.lock().unwrap();
    if cache.len() >= CACHE_LIMIT {
        cache.clear();
    }
    cache.insert(key, (Instant::now(), parsed.clone()));
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gif(id: &str) -> Value {
        json!({
            "id": id, "title": "cat", "url": format!("https://tenor.com/view/cat-gif-{id}"),
            "src": format!("https://media.tenor.com/{id}.mp4"), "gif_src": format!("https://media.tenor.com/{id}.gif"),
            "preview": format!("https://media.tenor.com/{id}.png"), "width": 480, "height": 270
        })
    }

    #[test]
    fn a_search_is_a_list_of_gifs() {
        let out = parse_gifs(&json!([gif("1"), gif("2")]));
        assert_eq!(out["gifs"].as_array().unwrap().len(), 2);
        assert_eq!(out["gifs"][0]["url"], "https://tenor.com/view/cat-gif-1");
        assert_eq!(out["gifs"][0]["src"], "https://media.tenor.com/1.mp4");
        assert_eq!(out["gifs"][0]["gifSrc"], "https://media.tenor.com/1.gif");
        assert_eq!(
            (
                out["gifs"][0]["width"].as_u64(),
                out["gifs"][0]["height"].as_u64()
            ),
            (Some(480), Some(270))
        );
    }

    #[test]
    fn trending_holds_its_gifs_beside_its_categories() {
        let out = parse_gifs(&json!({
            "categories": [{ "name": "reaction", "src": "https://media.tenor.com/c.mp4" }, { "name": "" }],
            "gifs": [gif("1")]
        }));
        assert_eq!(out["gifs"].as_array().unwrap().len(), 1);
        assert_eq!(
            out["categories"].as_array().unwrap().len(),
            1,
            "a category with no name is not one"
        );
        assert_eq!(out["categories"][0]["name"], "reaction");
    }

    #[test]
    fn one_with_nothing_to_play_or_no_page_is_left_out() {
        let out = parse_gifs(&json!([
            { "url": "https://tenor.com/view/x" },
            { "src": "https://media.tenor.com/y.mp4" },
            gif("3")
        ]));
        assert_eq!(out["gifs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn without_a_file_the_video_is_what_there_is() {
        let out = parse_gifs(
            &json!([{ "url": "https://tenor.com/view/x", "src": "https://media.tenor.com/x.mp4" }]),
        );
        assert_eq!(out["gifs"][0]["gifSrc"], "https://media.tenor.com/x.mp4");
    }

    #[test]
    fn nothing_recognisable_is_nothing() {
        let out = parse_gifs(&json!({ "message": "nope" }));
        assert!(out["gifs"].as_array().unwrap().is_empty());
        assert!(out["categories"].as_array().unwrap().is_empty());
    }
}
