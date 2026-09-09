//! The captcha the forum now puts on its login form.
//!
//! Not a picture of a bus. "Tartarus Captcha" is the same vendor as the
//! proof-of-work gate in `pow.rs` - the one that answers a request with HTTP
//! 203 - but a different product at a different endpoint: where the gate
//! gives a clearance cookie for the whole site, this hands back one token for
//! one form. Its widget is a checkbox that grinds a hash and ticks itself, so
//! there is nothing in it that needs eyes.
//!
//! The work is the same shape as the gate's and the numbers come off the
//! wire: several rounds, each asking for a nonce whose hash starts with some
//! number of zero bits, each answered before the next is issued. The site
//! currently asks for argon2id rather than SHA-256, which is why this can
//! not simply call into `pow.rs`.
//!
//! What it cannot do is the other branch. The service can decide mid-run that
//! it wants a "Monocle" assessment instead - a fingerprint of the browser,
//! collected by a third-party script - and no HTTP client can produce one. It
//! is off for this site (`monocle_enabled: false`), and if it is ever turned
//! on this says so plainly rather than pretending the login merely failed.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::http::HttpClient;

/// Where the captcha service lives, under the site's own host.
const API_BASE: &str = "/.ttrs/captcha";
/// The hidden input the widget fills in, and the site reads.
pub const TOKEN_FIELD: &str = "ttrs-captcha-token";
/// Argon2's salt for this proof of work: a constant of the scheme rather
/// than a per-challenge value, which is the challenge's own `salt` and goes
/// into the password instead. Taken from the widget verbatim.
const ARGON_SALT: &[u8] = b"tartarus-pow-v1!";
/// The digest length the widget asks for; the check reads the first 4 bytes.
const HASH_LEN: usize = 32;
/// Above this a solution is not guaranteed to exist in a 32-bit window, and
/// the site has no reason to ask for it - the live gate asks for 8.
const MAX_DIFFICULTY: u32 = 32;
/// The session token the service issues expires two minutes after it is
/// handed out, so there is no point grinding past that - and every round has
/// to fit inside it, round trips over Tor included.
const BUDGET: Duration = Duration::from_secs(105);

/// How a round wants its nonce hashed.
#[derive(Clone, Debug, PartialEq)]
pub enum Work {
    Sha256,
    /// Costs are the service's own: memory in KiB, then passes and lanes.
    Argon2id { m_cost: u32, t_cost: u32, p_cost: u32 },
}

impl Work {
    /// Reads the algorithm out of a `/start` or `/verify` answer.
    ///
    /// Absent means SHA-256, which is the widget's own default and the shape
    /// the gate already speaks.
    fn read(answer: &Value, previous: &Work) -> Work {
        match answer["algorithm"].as_str() {
            None => previous.clone(),
            Some("sha256") => Work::Sha256,
            Some(_) => Work::Argon2id {
                m_cost: answer["argon2_m_cost"].as_u64().unwrap_or(256) as u32,
                t_cost: answer["argon2_t_cost"].as_u64().unwrap_or(1) as u32,
                p_cost: answer["argon2_p_cost"].as_u64().unwrap_or(1) as u32,
            },
        }
    }

    /// The hash of one candidate, of which only the first four bytes matter.
    fn hash(&self, password: &[u8]) -> Result<[u8; HASH_LEN]> {
        let mut out = [0u8; HASH_LEN];
        match self {
            Work::Sha256 => {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(password);
                out.copy_from_slice(&hasher.finalize());
            }
            Work::Argon2id { m_cost, t_cost, p_cost } => {
                let params = argon2::Params::new(*m_cost, *t_cost, *p_cost, Some(HASH_LEN))
                    .map_err(|e| anyhow::anyhow!("the captcha asked for argon2 settings that are not usable: {e}"))?;
                argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
                    .hash_password_into(password, ARGON_SALT, &mut out)
                    .map_err(|e| anyhow::anyhow!("hashing the captcha's challenge: {e}"))?;
            }
        }
        Ok(out)
    }
}

/// Whether a hash clears the bar: `difficulty` zero bits at the front.
///
/// The widget reads the first four bytes as one big-endian number and counts
/// its leading zeros, so that is what this counts - the same answer as
/// counting across the whole digest for any difficulty that fits in 32 bits,
/// and the same arithmetic as the widget for the ones that do not.
fn clears(hash: &[u8; HASH_LEN], difficulty: u32) -> bool {
    u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]).leading_zeros() >= difficulty
}

/// Grinds one round: the nonce that, appended to the salt, hashes small
/// enough.
///
/// The candidate is the salt and the nonce written out in decimal and joined
/// with nothing between them, which is what `salt + nonce` means in the
/// widget's JavaScript.
pub fn find_nonce(salt: &str, difficulty: u32, work: &Work, deadline: Instant) -> Result<u64> {
    if difficulty > MAX_DIFFICULTY {
        bail!("the captcha asked for {difficulty} zero bits, which is more work than it can be owed");
    }
    let mut nonce: u64 = 0;
    loop {
        let hash = work.hash(format!("{salt}{nonce}").as_bytes())?;
        if clears(&hash, difficulty) {
            return Ok(nonce);
        }
        nonce += 1;
        // Checked rarely: a difficulty of 8 is answered in a few hundred
        // tries, and the clock is here for the case where the site raises it
        // far enough that finishing would outlive the session anyway.
        if nonce % 512 == 0 && Instant::now() > deadline {
            bail!("ran out of time answering the captcha ({difficulty} zero bits, {nonce} tries)");
        }
    }
}

/// The site key on a page that carries the widget, if it carries one.
///
/// A page without one needs no token, which is the ordinary case for every
/// form on the site that is not the login.
pub fn site_key(html: &str) -> Option<String> {
    let at = html.find("tartarus-captcha")?;
    // The attribute may sit either side of the one that named the widget.
    let window = &html[at.saturating_sub(400)..(at + 400).min(html.len())];
    let key = window.split("data-sitekey=\"").nth(1)?.split('"').next()?;
    (!key.trim().is_empty()).then(|| key.to_string())
}

/// Answers the captcha, handing back the token its form field wants.
pub async fn solve(http: &HttpClient, base: &str, site_key: &str) -> Result<String> {
    let deadline = Instant::now() + BUDGET;

    let start = http
        .get(&format!("{base}{API_BASE}/start?key={site_key}"))
        .await
        .context("asking the captcha for a challenge")?;
    let answer: Value = serde_json::from_str(&start.body)
        .with_context(|| format!("the captcha answered with something that is not JSON (HTTP {})", start.status))?;
    if answer["status"].as_str() != Some("challenge") {
        bail!("the captcha would not start: {}", message(&answer));
    }

    let rounds = answer["total_rounds"].as_u64().unwrap_or(1);
    let mut work = Work::read(&answer, &Work::Sha256);
    let mut session = string(&answer, "session_token");
    let mut salt = string(&answer, "salt");
    let mut difficulty = answer["difficulty"].as_u64().unwrap_or(0) as u32;
    tracing::info!("sneedchat: answering the login captcha, {rounds} round(s) at {difficulty} bits of {work:?}");

    // One more than the site said, because the count is what it advertised
    // rather than a promise - the loop ends on the answer that carries a
    // token, and this only stops it running forever if one never comes.
    for _ in 0..rounds + 1 {
        let nonce = find_nonce(&salt, difficulty, &work, deadline)?;
        let resp = http
            .post_form(
                &format!("{base}{API_BASE}/verify"),
                &[
                    ("session_token", session.as_str()),
                    ("salt", salt.as_str()),
                    ("nonce", &nonce.to_string()),
                    ("difficulty", &difficulty.to_string()),
                ],
            )
            .await
            .context("handing the captcha a solved round")?;
        let answer: Value = serde_json::from_str(&resp.body)
            .with_context(|| format!("the captcha answered a round with something that is not JSON (HTTP {})", resp.status))?;

        match answer["status"].as_str() {
            Some("complete") | Some("verified") => {
                let token = string(&answer, "verification_token");
                if token.is_empty() {
                    bail!("the captcha said it was satisfied but handed over no token");
                }
                return Ok(token);
            }
            Some("next_round") => {
                session = string(&answer, "session_token");
                salt = string(&answer, "salt");
                difficulty = answer["difficulty"].as_u64().unwrap_or(difficulty as u64) as u32;
                work = Work::read(&answer, &work);
            }
            // The branch nothing headless can take. Said plainly: this is not
            // a wrong password and not a broken client, it is the site asking
            // for something only a browser can give it.
            Some("monocle") => bail!(
                "the captcha asked for a browser fingerprint check (Monocle), which nothing but a real browser can produce"
            ),
            _ => bail!("the captcha rejected a round: {}", message(&answer)),
        }
        if Instant::now() > deadline {
            bail!("the captcha's challenge expired before its rounds were finished");
        }
    }
    bail!("the captcha never finished: it kept asking for more rounds")
}

fn string(value: &Value, field: &str) -> String {
    value[field].as_str().unwrap_or_default().to_string()
}

/// Whatever the service said about itself, for an error a person will read.
fn message(answer: &Value) -> String {
    answer["message"]
        .as_str()
        .or_else(|| answer["status"].as_str())
        .unwrap_or("no reason given")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_clears_the_bar_by_its_leading_zeros() {
        let mut hash = [0xffu8; HASH_LEN];
        hash[0] = 0x00;
        hash[1] = 0x7f;
        // 0x007f… is nine leading zeros: eight from the first byte and one more.
        assert!(clears(&hash, 9));
        assert!(!clears(&hash, 10));
        assert!(clears(&[0u8; HASH_LEN], 32));
    }

    #[test]
    fn a_nonce_is_found_for_a_real_challenge() {
        // SHA-256 at a difficulty the test can afford, checked by hashing the
        // answer again the way the site would.
        let deadline = Instant::now() + Duration::from_secs(30);
        let nonce = find_nonce("some-salt_abcd", 12, &Work::Sha256, deadline).expect("should find a nonce");
        let hash = Work::Sha256.hash(format!("some-salt_abcd{nonce}").as_bytes()).unwrap();
        assert!(clears(&hash, 12), "the nonce it found does not actually clear the bar");
    }

    #[test]
    fn argon2_answers_the_shape_the_site_asks_for() {
        // The live settings, at a difficulty low enough to stay quick: this
        // is here to catch the parameters being rejected outright, which is
        // what a wrong m_cost/p_cost pairing does.
        let work = Work::Argon2id { m_cost: 256, t_cost: 1, p_cost: 1 };
        let deadline = Instant::now() + Duration::from_secs(60);
        let nonce = find_nonce("5ef58a71decaea9a_6aa099c3", 8, &work, deadline).expect("should find a nonce");
        let hash = work.hash(format!("5ef58a71decaea9a_6aa099c3{nonce}").as_bytes()).unwrap();
        assert!(clears(&hash, 8));
    }

    #[test]
    fn the_algorithm_comes_off_the_wire_and_defaults_to_what_came_before() {
        let start = serde_json::json!({ "algorithm": "argon2id", "argon2_m_cost": 512, "argon2_t_cost": 2, "argon2_p_cost": 1 });
        assert_eq!(Work::read(&start, &Work::Sha256), Work::Argon2id { m_cost: 512, t_cost: 2, p_cost: 1 });
        // A round that says nothing keeps whatever the last one asked for.
        let quiet = serde_json::json!({ "status": "next_round" });
        assert_eq!(Work::read(&quiet, &Work::Argon2id { m_cost: 256, t_cost: 1, p_cost: 1 }), Work::Argon2id { m_cost: 256, t_cost: 1, p_cost: 1 });
        assert_eq!(Work::read(&serde_json::json!({ "algorithm": "sha256" }), &Work::Sha256), Work::Sha256);
    }

    #[test]
    fn a_site_key_is_found_only_where_a_widget_is() {
        let page = r#"<div class="block"><div data-xf-init="tartarus-captcha" data-sitekey="09a19d2b-a47f" data-captcha-host=""></div></div>"#;
        assert_eq!(site_key(page).as_deref(), Some("09a19d2b-a47f"));
        // The other order, since the attributes are the site's to arrange.
        let swapped = r#"<div data-sitekey="abc-123" data-xf-init="tartarus-captcha"></div>"#;
        assert_eq!(site_key(swapped).as_deref(), Some("abc-123"));
        assert_eq!(site_key("<form><input name=\"login\"></form>"), None);
    }
}
