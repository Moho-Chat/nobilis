//! ALTCHA, the proof of work some of Fluxer's requests ask for (the password sign-in among them).
//!
//! It is not a puzzle for a person: the server sends a nonce, a salt, a cost and the first half of a key, and the client
//! counts up from nought, deriving `PBKDF2-SHA256(nonce || counter, salt, cost)` for each count, until the key starts with
//! that half. Fluxer's documentation says a client "solves it without user input" (`topics/captcha.md`); its own app
//! does exactly this, through `altcha-lib` 2.x. The answer goes back in `X-Captcha-Token` as base64 JSON holding the
//! challenge as it came and the counter and key found.
//!
//! The work is bounded: a challenge that is not solved in a minute, or by the counter's end, is given up on.

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const GIVE_UP_AFTER: Duration = Duration::from_secs(60);

/// The token for a challenge, worked out on a thread that may block. `challenge` is the error's `altcha_challenge`.
pub async fn solve(challenge: &Value) -> Result<String> {
    let challenge = challenge.clone();
    tokio::task::spawn_blocking(move || solve_blocking(&challenge, GIVE_UP_AFTER)).await.map_err(|e| anyhow!("the proof of work stopped: {e}"))?
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn solve_blocking(challenge: &Value, patience: Duration) -> Result<String> {
    let p = &challenge["parameters"];
    if p["algorithm"].as_str() != Some("PBKDF2/SHA-256") {
        bail!("Fluxer asked for a kind of proof of work moho does not know ({})", p["algorithm"]);
    }
    let field = |k: &str| p[k].as_str().and_then(|s| ::hex::decode(s).ok()).ok_or_else(|| anyhow!("the challenge has no {k}"));
    let (nonce, salt) = (field("nonce")?, field("salt")?);
    let prefix = p["keyPrefix"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("the challenge has no key prefix"))?.to_ascii_lowercase();
    let cost = p["cost"].as_u64().filter(|c| *c > 0 && *c <= 10_000_000).ok_or_else(|| anyhow!("the challenge has no cost"))? as u32;
    let length = p["keyLength"].as_u64().unwrap_or(32).clamp(1, 64) as usize;
    let started = Instant::now();
    let mut password = nonce.clone();
    password.extend_from_slice(&[0, 0, 0, 0]);
    let mut key = vec![0u8; length];
    for counter in 0..=u32::MAX {
        let at = nonce.len();
        password[at..].copy_from_slice(&counter.to_be_bytes());
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(&password, &salt, cost, &mut key);
        let derived = hex(&key);
        if derived.starts_with(&prefix) {
            let token = json!({ "challenge": { "parameters": p, "signature": challenge["signature"] }, "solution": { "counter": counter, "derivedKey": derived } });
            return Ok(base64::engine::general_purpose::STANDARD.encode(token.to_string()));
        }
        if counter % 256 == 0 && started.elapsed() > patience {
            bail!("Fluxer's proof of work took too long to finish");
        }
    }
    bail!("Fluxer's proof of work has no answer")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A challenge made the way the server makes one: the key for a chosen counter, and its first half as the prefix.
    fn made(counter: u32, cost: u32) -> Value {
        let nonce = [7u8; 16];
        let salt = [9u8; 16];
        let mut password = nonce.to_vec();
        password.extend_from_slice(&counter.to_be_bytes());
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(&password, &salt, cost, &mut key);
        json!({ "parameters": { "algorithm": "PBKDF2/SHA-256", "nonce": hex(&nonce), "salt": hex(&salt), "cost": cost, "keyLength": 32, "keyPrefix": hex(&key[..16]), "keySignature": "ab", "expiresAt": 1 }, "signature": "cd" })
    }

    #[test]
    fn the_counter_is_found_and_said_with_the_challenge_as_it_came() {
        let challenge = made(37, 10);
        let token = solve_blocking(&challenge, Duration::from_secs(10)).unwrap();
        let decoded: Value = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(token).unwrap()).unwrap();
        assert_eq!(decoded["solution"]["counter"], 37);
        assert_eq!(decoded["challenge"]["parameters"], challenge["parameters"]);
        assert_eq!(decoded["challenge"]["signature"], "cd");
        assert_eq!(decoded["solution"]["derivedKey"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn a_challenge_of_another_kind_or_with_no_fields_is_refused() {
        let mut other = made(1, 1);
        other["parameters"]["algorithm"] = json!("ARGON2ID");
        assert!(solve_blocking(&other, Duration::from_secs(1)).unwrap_err().to_string().contains("kind of proof"));
        assert!(solve_blocking(&json!({ "parameters": { "algorithm": "PBKDF2/SHA-256" } }), Duration::from_secs(1)).is_err());
    }
}
