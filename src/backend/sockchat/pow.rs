//! KiwiFlare / Tartarus proof-of-work gate.
//!
//! Ported from sockchat-rs's `net/kiwiflare.rs`
//! (<https://gitgud.io/jcmoon/sockchat-rs>). The proxy in front of the
//! service answers a gated request with HTTP 203 and a challenge page whose
//! `<html>` element carries `data-ttrs-*` attributes. The client finds a
//! nonce such that `SHA-256(salt ++ nonce)` has some number of leading zero
//! bits, then POSTs it to `/.ttrs/challenge` to receive a clearance cookie.
//!
//! The gate may be multi-step: after a solution is accepted the next
//! request can be met with a fresh challenge. Callers loop until a request
//! is no longer answered with 203 rather than trusting the advertised step
//! count.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use super::http::HttpClient;

/// Status code the proxy uses to signal "solve a challenge first".
pub const GATE_STATUS: u16 = 203;
/// Path that accepts solutions.
pub const SUBMIT_PATH: &str = "/.ttrs/challenge";
/// Hard ceiling on difficulty. The nonce is a `u32`, so above 32 bits a
/// solution is not guaranteed to exist at all - grinding would be pointless
/// as well as slow. A live gate asks for ~17 bits.
const MAX_DIFFICULTY: u32 = 32;
/// The only hash algorithm implemented. An unrecognised one is fatal rather
/// than ignorable: grinding SHA-256 against a challenge that wants
/// something else would never terminate.
const SUPPORTED_ALGORITHM: &str = "sha256";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Challenge {
    pub salt: String,
    /// Required leading zero bits.
    pub difficulty: u32,
    /// Steps the server says remain. Advisory only.
    pub steps: i8,
}

#[derive(Clone, Debug)]
pub struct Solution {
    pub salt: String,
    pub nonce: u32,
    pub hash: [u8; 32],
}

/// Does `hash` begin with `diff` zero bits?
fn check_zeros(diff: u32, hash: &[u8]) -> bool {
    let whole = (diff / 8) as usize;
    let rem = diff % 8;
    if hash.len() < whole || (rem > 0 && hash.len() < whole + 1) {
        return false;
    }
    if hash[..whole].iter().any(|&b| b != 0) {
        return false;
    }
    if rem == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rem);
    hash[whole] & mask == 0
}

/// Hash one candidate. The preimage is the salt followed by the nonce in
/// decimal ASCII, with no separator.
fn hash_candidate(buf: &mut Vec<u8>, salt_len: usize, nonce: u32) -> [u8; 32] {
    buf.truncate(salt_len);
    let mut digits = [0u8; 10];
    let mut n = nonce;
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.extend_from_slice(&digits[i..]);
    Sha256::digest(&buf[..]).into()
}

impl Challenge {
    /// Extract a challenge from a gate page.
    pub fn parse(html: &str) -> Result<Challenge> {
        let salt = attr(html, "data-ttrs-challenge").context("challenge page has no data-ttrs-challenge attribute")?;
        let difficulty = attr(html, "data-ttrs-difficulty")
            .context("challenge page has no data-ttrs-difficulty attribute")?
            .trim()
            .parse::<u32>()
            .context("data-ttrs-difficulty is not a number")?;
        // Absent step count means a single step.
        let steps = attr(html, "data-ttrs-steps").and_then(|s| s.trim().parse::<i8>().ok()).unwrap_or(1);

        // Absent means the original scheme, which was SHA-256 only.
        if let Some(algorithm) = attr(html, "data-ttrs-algorithm") {
            let normalised = algorithm.to_ascii_lowercase().replace(['-', '_'], "");
            if normalised != SUPPORTED_ALGORITHM {
                bail!("gate wants unsupported algorithm {algorithm:?}; this client only implements {SUPPORTED_ALGORITHM}");
            }
        }

        if salt.is_empty() {
            bail!("challenge salt is empty");
        }
        if difficulty > MAX_DIFFICULTY {
            bail!("challenge difficulty {difficulty} exceeds the {MAX_DIFFICULTY}-bit nonce space, so no solution need exist");
        }
        Ok(Challenge { salt, difficulty, steps })
    }

    /// Brute-force a nonce. CPU-bound; call from `spawn_blocking`.
    ///
    /// Returns `None` when the whole nonce space is exhausted without a
    /// hit - the server can multiply difficulty when it believes it's under
    /// attack, and that multiplier applies to the bit count, so a base of
    /// 18 becomes 72: far beyond what any 32-bit nonce can satisfy.
    pub fn solve(&self) -> Option<Solution> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(16);
        let found = Arc::new(AtomicBool::new(false));
        let answer = Arc::new(AtomicU32::new(0));
        // Start somewhere unpredictable so repeated challenges with the
        // same salt don't all replay the same nonce sequence.
        let start = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let total_budget = u32::MAX as u64 + 1;

        std::thread::scope(|scope| {
            for t in 0..threads {
                let (found, answer) = (Arc::clone(&found), Arc::clone(&answer));
                let salt = self.salt.as_bytes().to_vec();
                let diff = self.difficulty;
                scope.spawn(move || {
                    let salt_len = salt.len();
                    let mut buf = salt;
                    let mut nonce = start.wrapping_add(t as u32);
                    let stride = threads as u32;
                    let budget = total_budget.div_ceil(stride as u64);
                    for checked in 0..budget {
                        if checked.is_multiple_of(4096) && found.load(Ordering::Relaxed) {
                            return;
                        }
                        let hash = hash_candidate(&mut buf, salt_len, nonce);
                        if check_zeros(diff, &hash) {
                            if !found.swap(true, Ordering::AcqRel) {
                                answer.store(nonce, Ordering::Release);
                            }
                            return;
                        }
                        nonce = nonce.wrapping_add(stride);
                    }
                });
            }
        });

        if !found.load(Ordering::Acquire) {
            return None;
        }
        let nonce = answer.load(Ordering::Acquire);
        let mut buf = self.salt.as_bytes().to_vec();
        let salt_len = buf.len();
        Some(Solution { salt: self.salt.clone(), nonce, hash: hash_candidate(&mut buf, salt_len, nonce) })
    }
}

/// Read an HTML attribute value, handling double, single and unquoted
/// forms. A dependency-free scan is enough here: three attributes off one
/// known element, not a conformant parser.
fn attr(html: &str, name: &str) -> Option<String> {
    let mut offset = 0;
    while let Some(rel) = html[offset..].find(name) {
        let start = offset + rel;
        let end = start + name.len();
        offset = end;

        let starts_cleanly = html[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_');
        if !starts_cleanly {
            continue;
        }

        let Some(value) = html[end..].trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim_start();
        let extracted = if let Some(v) = value.strip_prefix('"') {
            v.split('"').next()
        } else if let Some(v) = value.strip_prefix('\'') {
            v.split('\'').next()
        } else {
            value.split([' ', '>', '\n', '\t', '\r']).next()
        };
        return extracted.map(str::to_string);
    }
    None
}

/// Solve the gate if the given URL is gated, repeating for multi-step
/// gates. Returns the number of challenges solved; zero means the URL
/// wasn't gated.
pub async fn clear(http: &HttpClient, url: &str, max_steps: usize) -> Result<usize> {
    let mut solved = 0;
    for _ in 0..max_steps {
        let resp = http.get(url).await?;
        if resp.status != GATE_STATUS {
            return Ok(solved);
        }

        let challenge = Challenge::parse(&resp.body)?;
        tracing::info!(salt = %challenge.salt, difficulty = challenge.difficulty, steps = challenge.steps, "solving KiwiFlare challenge");

        let c = challenge.clone();
        let solution = tokio::task::spawn_blocking(move || c.solve())
            .await
            .context("solver task panicked")?
            .with_context(|| format!("no nonce in the 32-bit space satisfies a difficulty-{} challenge; the server may have raised difficulty under attack", challenge.difficulty))?;

        submit(http, url, &solution).await?;
        solved += 1;

        if challenge.steps <= 1 {
            let resp = http.get(url).await?;
            if resp.status != GATE_STATUS {
                return Ok(solved);
            }
        }
    }
    bail!("gate still challenging us after {max_steps} solutions")
}

/// POST a solution. The clearance cookie arrives via `Set-Cookie` and is
/// absorbed into the caller's cookie jar.
async fn submit(http: &HttpClient, url: &str, sol: &Solution) -> Result<()> {
    let base = url::Url::parse(url)?;
    let submit_url = format!("{}://{}{SUBMIT_PATH}", base.scheme(), base.host_str().context("URL has no host")?);

    tracing::debug!(nonce = sol.nonce, hash = %hex::encode(sol.hash), "submitting gate solution");
    let resp = http.post_form(&submit_url, &[("salt", sol.salt.as_str()), ("nonce", &sol.nonce.to_string())]).await?;
    if !(200..300).contains(&resp.status) {
        bail!("gate solution rejected with HTTP {}", resp.status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_check_matches_full_and_partial_bytes() {
        assert!(check_zeros(0, &[0xff, 0xff]));
        assert!(check_zeros(8, &[0x00, 0xff]));
        assert!(!check_zeros(8, &[0x01, 0xff]));
        assert!(check_zeros(12, &[0x00, 0x0f]));
        assert!(!check_zeros(12, &[0x00, 0x1f]));
        assert!(!check_zeros(16, &[0x00]));
    }

    #[test]
    fn parses_a_challenge_page() {
        let html = r#"<html data-ttrs-challenge="abc123" data-ttrs-difficulty="8" data-ttrs-steps="2">"#;
        let c = Challenge::parse(html).unwrap();
        assert_eq!(c.salt, "abc123");
        assert_eq!(c.difficulty, 8);
        assert_eq!(c.steps, 2);
    }

    #[test]
    fn defaults_steps_to_one_when_absent() {
        let html = r#"<html data-ttrs-challenge="s" data-ttrs-difficulty="4">"#;
        assert_eq!(Challenge::parse(html).unwrap().steps, 1);
    }

    #[test]
    fn rejects_unsupported_algorithms() {
        let html = r#"<html data-ttrs-challenge="s" data-ttrs-difficulty="4" data-ttrs-algorithm="md5">"#;
        assert!(Challenge::parse(html).is_err());
    }

    #[test]
    fn rejects_excessive_difficulty() {
        let html = r#"<html data-ttrs-challenge="s" data-ttrs-difficulty="99">"#;
        assert!(Challenge::parse(html).is_err());
    }

    #[test]
    fn solves_a_low_difficulty_challenge() {
        let c = Challenge { salt: "test-salt".to_string(), difficulty: 8, steps: 1 };
        let sol = c.solve().expect("difficulty 8 should always be solvable quickly");
        assert!(check_zeros(8, &sol.hash));
        assert_eq!(sol.salt, "test-salt");
    }
}
