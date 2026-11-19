//! What an instance says of itself at `/.well-known/fluxer`: where its API, gateway and files are, what it has turned
//! on, and the rules its limits are worked out from.
//!
//! Fluxer asks a client never to build an address from the instance's own name or to write a host down: every address
//! comes from this document. An account therefore keeps only its instance's origin, and reads this again on each start.

use serde_json::Value;

/// The official instance.
pub const DEFAULT_ORIGIN: &str = "https://fluxer.com";

/// The limits an account is held to, worked out from the instance's rules (see `resolve`).
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub message_length: usize,
    pub attachments_per_message: usize,
    /// The most each attached file may weigh, in bytes.
    pub attachment_bytes: u64,
    pub reactions_per_message: usize,
    pub guilds: usize,
    pub bio_length: usize,
}

impl Default for Limits {
    /// The release defaults (`packages/limits/src/LimitDefaults.ts`), which the document does not carry.
    fn default() -> Self {
        Self { message_length: 2000, attachments_per_message: 10, attachment_bytes: 25 * 1024 * 1024, reactions_per_message: 30, guilds: 100, bio_length: 320 }
    }
}

/// An instance, as its discovery document describes it.
#[derive(Debug, Clone, Default)]
pub struct Instance {
    /// The origin the account was added with.
    pub origin: String,
    /// The public API's base, ending in `/v1`.
    pub api: String,
    pub gateway: String,
    /// The media proxy, where avatars, icons, emoji and attachments are read from.
    pub media: String,
    /// Where an invite link points (`https://fluxer.gg`).
    pub invite: String,
    pub webapp: String,
    /// Whether gated requests (the password sign-in among them) want an ALTCHA proof of work.
    pub altcha: bool,
    pub voice: bool,
    pub presigned_uploads: bool,
    /// `email` or `username`: what a person signs in with.
    pub identity: String,
    /// The rules, kept to work out an account's limits once its traits are known.
    pub rules: Value,
}

impl Instance {
    pub fn from_document(origin: &str, doc: &Value) -> Option<Self> {
        let endpoints = &doc["endpoints"];
        let api = endpoints["api_public"].as_str().or_else(|| endpoints["api"].as_str())?.trim_end_matches('/');
        let gateway = endpoints["gateway"].as_str()?.to_string();
        let features = &doc["features"];
        Some(Self {
            origin: origin.to_string(),
            api: format!("{api}/v1"),
            gateway,
            media: endpoints["media"].as_str().unwrap_or("").trim_end_matches('/').to_string(),
            invite: endpoints["invite"].as_str().unwrap_or("").trim_end_matches('/').to_string(),
            webapp: endpoints["webapp"].as_str().unwrap_or(origin).trim_end_matches('/').to_string(),
            altcha: doc["captcha"]["provider"].as_str() == Some("altcha"),
            voice: features["voice_enabled"].as_bool().unwrap_or(false),
            presigned_uploads: features["presigned_attachment_uploads"].as_bool().unwrap_or(false),
            identity: features["account_identity"].as_str().unwrap_or("email").to_string(),
            rules: doc["limits"].clone(),
        })
    }

    /// The limits for an account with these traits (`premium` for a paying one).
    pub fn limits(&self, traits: &[&str]) -> Limits {
        resolve(&self.rules, traits)
    }
}

/// Works out an account's limits as the document says to: from the defaults, the rules sorted by how many conditions
/// they have, each applied when every trait it names is the account's; a rule with no conditions sets a value, and
/// one with conditions only raises it. Rules about guild features are a guild's, not an account's, and are left out.
pub fn resolve(config: &Value, traits: &[&str]) -> Limits {
    let mut out = Limits::default();
    let Some(rules) = config["rules"].as_array() else { return out };
    let mut rules: Vec<&Value> = rules.iter().collect();
    let conditions = |r: &Value| r["filters"]["traits"].as_array().map_or(0, Vec::len) + r["filters"]["guildFeatures"].as_array().map_or(0, Vec::len);
    rules.sort_by_key(|r| conditions(r));
    for rule in rules {
        if rule["filters"]["guildFeatures"].as_array().is_some_and(|f| !f.is_empty()) {
            continue;
        }
        let wanted = rule["filters"]["traits"].as_array().cloned().unwrap_or_default();
        if !wanted.iter().all(|t| t.as_str().is_some_and(|t| traits.contains(&t))) {
            continue;
        }
        let raise = !wanted.is_empty();
        let o = &rule["overrides"];
        let set = |key: &str, slot: &mut u64| {
            if let Some(v) = o[key].as_i64().filter(|v| *v >= 0) {
                let v = v as u64;
                *slot = if raise { (*slot).max(v) } else { v };
            }
        };
        let (mut length, mut count, mut bytes, mut reactions, mut guilds, mut bio) = (out.message_length as u64, out.attachments_per_message as u64, out.attachment_bytes, out.reactions_per_message as u64, out.guilds as u64, out.bio_length as u64);
        set("max_message_length", &mut length);
        set("max_attachments_per_message", &mut count);
        set("max_attachment_file_size", &mut bytes);
        set("max_reactions_per_message", &mut reactions);
        set("max_guilds", &mut guilds);
        set("max_bio_length", &mut bio);
        out = Limits { message_length: length as usize, attachments_per_message: count as usize, attachment_bytes: bytes, reactions_per_message: reactions as usize, guilds: guilds as usize, bio_length: bio as usize };
    }
    out
}

/// The origin of an instance as it was typed: nothing means the official one; the official web app's other names are
/// the official one; a path is dropped (the document is at the origin's root).
pub fn normalize_origin(typed: &str) -> Option<String> {
    let text = typed.trim().trim_end_matches('/');
    if text.is_empty() {
        return Some(DEFAULT_ORIGIN.to_string());
    }
    let with_scheme = if text.contains("://") { text.to_string() } else { format!("https://{text}") };
    let url = url::Url::parse(&with_scheme).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    if !host.contains('.') && host != "localhost" {
        return None;
    }
    if matches!(host, "fluxer.com" | "www.fluxer.com" | "fluxer.app" | "web.fluxer.app" | "canary.fluxer.app") {
        return Some(DEFAULT_ORIGIN.to_string());
    }
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{}://{host}{port}", url.scheme()))
}

/// Reads an instance's discovery document, through the account's route.
pub async fn fetch(route_key: &str, origin: &str) -> anyhow::Result<Instance> {
    let router = crate::net::route::router();
    let client = router.client_if("fluxer", router.routed(route_key), |b| b.user_agent(super::user_agent()).timeout(std::time::Duration::from_secs(20)));
    let response = client.get(format!("{origin}/.well-known/fluxer")).header("Accept", "application/json").send().await.map_err(|e| anyhow::anyhow!("could not reach {origin}: {e}"))?;
    if !response.status().is_success() {
        anyhow::bail!("{origin} did not answer like a Fluxer instance ({})", response.status());
    }
    let doc: Value = response.json().await.map_err(|_| anyhow::anyhow!("{origin} did not answer like a Fluxer instance"))?;
    Instance::from_document(origin, &doc).ok_or_else(|| anyhow::anyhow!("{origin} did not say where its API and gateway are"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_origin_is_made_from_what_was_typed() {
        assert_eq!(normalize_origin("").as_deref(), Some("https://fluxer.com"));
        assert_eq!(normalize_origin("web.fluxer.app/channels/@me").as_deref(), Some("https://fluxer.com"));
        assert_eq!(normalize_origin("chat.example.org/").as_deref(), Some("https://chat.example.org"));
        assert_eq!(normalize_origin("http://localhost:8890").as_deref(), Some("http://localhost:8890"));
        assert_eq!(normalize_origin("https://chat.example.org/some/path").as_deref(), Some("https://chat.example.org"));
        assert_eq!(normalize_origin("ftp://example.org"), None);
        assert_eq!(normalize_origin("not a host"), None);
    }

    #[test]
    fn every_address_comes_from_the_document() {
        let doc = json!({
            "endpoints": { "api": "https://web.example/api", "api_public": "https://example.com/api", "gateway": "wss://gateway.example.com", "media": "https://media.example/", "invite": "https://ex.gg", "webapp": "https://example.com" },
            "captcha": { "provider": "altcha" },
            "features": { "voice_enabled": true, "presigned_attachment_uploads": true, "account_identity": "username" }
        });
        let i = Instance::from_document("https://example.com", &doc).unwrap();
        assert_eq!(i.api, "https://example.com/api/v1", "the public API, not the web app's");
        assert_eq!((i.gateway.as_str(), i.media.as_str(), i.invite.as_str()), ("wss://gateway.example.com", "https://media.example", "https://ex.gg"));
        assert!(i.altcha && i.voice && i.presigned_uploads);
        assert_eq!(i.identity, "username");
        assert!(Instance::from_document("https://x", &json!({ "endpoints": {} })).is_none(), "no API, no instance");
    }

    #[test]
    fn limits_follow_the_rules_as_the_document_says() {
        // fluxer.com's own rules, abridged.
        let rules = json!({ "version": 2, "rules": [
            { "id": "premium", "filters": { "traits": ["premium"] }, "overrides": { "max_message_length": 4000, "max_attachment_file_size": 524288000, "max_guilds": 200 } },
            { "id": "default", "overrides": { "max_guild_emojis": 500 } },
            { "id": "very_large_guild", "filters": { "guildFeatures": ["VERY_LARGE_GUILD"] }, "overrides": { "max_message_length": 99999 } }
        ]});
        let plain = resolve(&rules, &[]);
        assert_eq!(plain, Limits::default(), "a rule that does not name a limit leaves the default");
        let paying = resolve(&rules, &["premium"]);
        assert_eq!((paying.message_length, paying.attachment_bytes, paying.guilds), (4000, 524288000, 200));
        // A rule with no conditions sets a value, lower or not; one with conditions only raises.
        let lowered = resolve(&json!({ "rules": [{ "id": "a", "overrides": { "max_message_length": 500 } }, { "id": "b", "filters": { "traits": ["x"] }, "overrides": { "max_message_length": 100 } }] }), &["x"]);
        assert_eq!(lowered.message_length, 500);
        assert_eq!(resolve(&json!({ "rules": [{ "id": "a", "overrides": { "max_message_length": -1 } }] }), &[]).message_length, 2000, "a negative value is ignored");
    }
}
