//! What a homeserver says it can do, before being asked to do it.
//!
//! Two endpoints nothing here was calling. `/versions` says which spec
//! versions and unstable features a server speaks; `/capabilities` says what
//! it will let this account change - the password, the display name, the
//! avatar - and which room versions it will create.
//!
//! Without them everything was discovered by failing, and failing in the
//! confusing direction. A homeserver that will not let you change your
//! display name answers the attempt with a 403 that a client shows as an
//! error, when the honest answer is that the field should never have been
//! offered. A room created with a version the server does not support comes
//! back as a 400 naming the version rather than saying "this server does not
//! do that".
//!
//! Read once on connect and kept. Both are properties of the server rather
//! than of the moment, and a homeserver that changes either has been
//! restarted - which reconnects this account anyway.

use super::*;

/// What one homeserver told us about itself.
#[derive(Clone, Debug, Default)]
pub struct ServerFacts {
    /// The spec versions it claims, newest last - `v1.1`, `v1.2`, and so on.
    pub versions: Vec<String>,
    /// Unstable features it has switched on, by their MSC name.
    pub unstable: Vec<String>,
    /// The room version it makes by default, where it says.
    pub default_room_version: Option<String>,
    /// Room versions it will accept, by number.
    pub room_versions: Vec<String>,
    /// Whether this account may change its own password.
    pub can_change_password: bool,
    /// Whether this account may change its own display name.
    pub can_change_displayname: bool,
    /// Whether this account may change its own avatar.
    pub can_change_avatar: bool,
}

impl ServerFacts {
    /// Whether the server claims a spec version at least this new.
    ///
    /// String comparison would say `v1.10` is older than `v1.9`, which is the
    /// kind of wrong that only shows up once a year and then looks like
    /// something else entirely - so the numbers are compared as numbers.
    pub fn speaks(&self, wanted: &str) -> bool {
        let parse = |v: &str| -> Option<(u32, u32)> {
            let (major, minor) = v.trim_start_matches('v').split_once('.')?;
            Some((major.parse().ok()?, minor.parse().ok()?))
        };
        let Some(wanted) = parse(wanted) else { return false };
        self.versions.iter().filter_map(|v| parse(v)).any(|have| have >= wanted)
    }

    /// Whether it has this unstable feature switched on.
    pub fn has_unstable(&self, feature: &str) -> bool {
        self.unstable.iter().any(|f| f == feature)
    }
}

/// Reads what a homeserver says about itself.
///
/// Neither call is fatal. `/versions` is unauthenticated and answered by every
/// homeserver ever written; `/capabilities` needs a token and is occasionally
/// absent on old ones. A server that answers neither is treated as a server
/// that permits everything, which is exactly how this client behaved before
/// either was read - so the failure mode is the old behaviour rather than a
/// new one.
pub async fn read_facts(homeserver_url: &str, access_token: &str) -> ServerFacts {
    let base = homeserver_url.trim_end_matches('/');
    let mut facts = ServerFacts {
        // Absent means permitted. A server that will not say is not a server
        // saying no, and greying out a field because a request failed would
        // take away a thing that very likely works.
        can_change_password: true,
        can_change_displayname: true,
        can_change_avatar: true,
        ..ServerFacts::default()
    };

    if let Ok(answer) = http::get_json_anonymous(&format!("{base}/_matrix/client/versions")).await {
        facts.versions = string_list(&answer["versions"]);
        if let Some(flags) = answer["unstable_features"].as_object() {
            facts.unstable = flags.iter().filter(|(_, on)| on.as_bool() == Some(true)).map(|(name, _)| name.clone()).collect();
        }
    }

    if let Ok(answer) = http::get_json(&format!("{base}/_matrix/client/v3/capabilities"), access_token).await {
        let caps = &answer["capabilities"];
        // `enabled` absent means enabled, per the specification - the field
        // exists to say no, not to say yes.
        let enabled = |name: &str| caps[name]["enabled"].as_bool().unwrap_or(true);
        facts.can_change_password = enabled("m.change_password");
        facts.can_change_displayname = enabled("m.set_displayname");
        facts.can_change_avatar = enabled("m.set_avatar_url");
        let versions = &caps["m.room_versions"];
        facts.default_room_version = versions["default"].as_str().map(str::to_string);
        if let Some(available) = versions["available"].as_object() {
            facts.room_versions = available.keys().cloned().collect();
            facts.room_versions.sort();
        }
    }

    facts
}

fn string_list(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(versions: &[&str]) -> ServerFacts {
        ServerFacts { versions: versions.iter().map(|v| v.to_string()).collect(), ..ServerFacts::default() }
    }

    /// Spec versions are compared as numbers, not as text. Sorted as strings,
    /// `v1.10` comes before `v1.9` - a mistake that lies dormant for a year
    /// and then presents as a feature mysteriously turning itself off.
    #[test]
    fn a_version_is_compared_as_numbers() {
        let server = facts(&["v1.1", "v1.9", "v1.10"]);
        assert!(server.speaks("v1.9"));
        assert!(server.speaks("v1.10"));
        assert!(!server.speaks("v1.11"));

        // The one that string comparison gets wrong.
        assert!(facts(&["v1.10"]).speaks("v1.9"));
        assert!(!facts(&["v1.9"]).speaks("v1.10"));
    }

    /// A server that says nothing is not a server saying no.
    #[test]
    fn nothing_said_is_not_a_refusal() {
        let silent = ServerFacts::default();
        assert!(!silent.speaks("v1.1"));
        assert!(!silent.has_unstable("org.matrix.msc3575"));
        // Anything unparseable is not a version rather than a panic.
        assert!(!facts(&["nonsense", "v1.1"]).speaks("v1.2"));
        assert!(!facts(&["v1.1"]).speaks("nonsense"));
    }
}
