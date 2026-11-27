//! Rocket.Chat server discovery via `GET /api/info` and `GET /api/v1/settings.public`.
//!
//! Discovers server version (and v6/v7/v8 API compatibility flags), site name,
//! E2EE enablement, OAuth/SSO providers, file upload limits, Teams, Matrix Federation,
//! read receipts, OTR, and video conferencing flags.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// Parsed semver for Rocket.Chat version-gated API behavior (v6 / v7 / v8).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerVersion {
    pub raw: String,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ServerVersion {
    pub fn parse(raw: &str) -> Self {
        let clean = raw.trim().trim_start_matches('v');
        let core = clean.split(['-', '+']).next().unwrap_or("");
        let mut parts = core.split('.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Self { raw: raw.to_string(), major, minor, patch }
    }

    /// Rocket.Chat 7+ prefers modern AES-GCM E2EE (`rc.v1.aes-sha2`) and deprecated some v6 fields.
    pub fn supports_aes_gcm_e2ee(&self) -> bool {
        self.major >= 7 || (self.major == 6 && self.minor >= 10)
    }

    /// Rocket.Chat 6+ supports Teams and Discussions natively.
    pub fn supports_teams(&self) -> bool {
        self.major == 0 || self.major >= 4
    }
}

/// Discovered OAuth / SAML / OIDC / LDAP SSO provider on the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SsoProvider {
    pub id: String,
    pub name: String,
    pub kind: String, // "oauth" | "saml" | "ldap" | "cas"
}

/// Public capabilities and limits discovered from a Rocket.Chat server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub server_url: String,
    pub version: ServerVersion,
    pub site_name: String,
    pub e2ee_enabled: bool,
    pub e2ee_default_dm: bool,
    pub e2ee_default_private: bool,
    pub max_upload_bytes: u64,
    pub file_upload_enabled: bool,
    pub read_receipts_enabled: bool,
    pub read_receipts_store_users: bool,
    pub threads_enabled: bool,
    pub discussions_enabled: bool,
    pub federation_enabled: bool,
    pub otr_enabled: bool,
    pub katex_enabled: bool,
    pub webrtc_enabled: bool,
    pub livechat_enabled: bool,
    pub message_allow_editing: bool,
    pub message_allow_deleting: bool,
    pub message_allow_pinning: bool,
    pub message_allow_starring: bool,
    pub sso_providers: Vec<SsoProvider>,
}

impl Default for ServerInfo {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            version: ServerVersion::default(),
            site_name: "Rocket.Chat".to_string(),
            e2ee_enabled: false,
            e2ee_default_dm: false,
            e2ee_default_private: false,
            max_upload_bytes: 100 * 1024 * 1024,
            file_upload_enabled: true,
            read_receipts_enabled: false,
            read_receipts_store_users: false,
            threads_enabled: true,
            discussions_enabled: true,
            federation_enabled: false,
            otr_enabled: false,
            katex_enabled: false,
            webrtc_enabled: false,
            livechat_enabled: false,
            message_allow_editing: true,
            message_allow_deleting: true,
            message_allow_pinning: true,
            message_allow_starring: true,
            sso_providers: Vec::new(),
        }
    }
}

/// Parse `/api/info` and `/api/v1/settings.public` responses into `ServerInfo`.
pub fn parse_server_info(server_url: &str, info_json: &Value, settings_json: &Value) -> ServerInfo {
    let mut out = ServerInfo {
        server_url: server_url.to_string(),
        ..Default::default()
    };

    let version_str = info_json["version"]
        .as_str()
        .or_else(|| info_json["info"]["version"].as_str())
        .unwrap_or("");
    out.version = ServerVersion::parse(version_str);

    if let Some(arr) = settings_json["settings"].as_array() {
        for item in arr {
            let id = item["_id"].as_str().unwrap_or("");
            let val = &item["value"];
            match id {
                "Site_Name" => {
                    if let Some(s) = val.as_str().filter(|s| !s.is_empty()) {
                        out.site_name = s.to_string();
                    }
                }
                "E2E_Enable" => out.e2ee_enabled = val.as_bool().unwrap_or(false),
                "E2E_Enabled_Default_DirectRooms" => out.e2ee_default_dm = val.as_bool().unwrap_or(false),
                "E2E_Enabled_Default_PrivateRooms" => out.e2ee_default_private = val.as_bool().unwrap_or(false),
                "FileUpload_MaxFileSize" => {
                    if let Some(n) = val.as_i64() {
                        if n > 0 {
                            out.max_upload_bytes = n as u64;
                        }
                    }
                }
                "FileUpload_Enabled" => out.file_upload_enabled = val.as_bool().unwrap_or(true),
                "Message_Read_Receipt_Enabled" => out.read_receipts_enabled = val.as_bool().unwrap_or(false),
                "Message_Read_Receipt_Store_Users" => out.read_receipts_store_users = val.as_bool().unwrap_or(false),
                "Threads_enabled" => out.threads_enabled = val.as_bool().unwrap_or(true),
                "Discussion_enabled" => out.discussions_enabled = val.as_bool().unwrap_or(true),
                "Federation_Matrix_enabled" | "FEDERATION_Enabled" => {
                    if val.as_bool().unwrap_or(false) {
                        out.federation_enabled = true;
                    }
                }
                "OTR_Enable" => out.otr_enabled = val.as_bool().unwrap_or(false),
                "Katex_Enabled" => out.katex_enabled = val.as_bool().unwrap_or(false),
                "WebRTC_Enable_Direct" | "WebRTC_Enable_Channel" | "WebRTC_Enable_Private" => {
                    if val.as_bool().unwrap_or(false) {
                        out.webrtc_enabled = true;
                    }
                }
                "Livechat_enabled" => out.livechat_enabled = val.as_bool().unwrap_or(false),
                "Message_AllowEditing" => out.message_allow_editing = val.as_bool().unwrap_or(true),
                "Message_AllowDeleting" => out.message_allow_deleting = val.as_bool().unwrap_or(true),
                "Message_AllowPinning" => out.message_allow_pinning = val.as_bool().unwrap_or(true),
                "Message_AllowStarring" => out.message_allow_starring = val.as_bool().unwrap_or(true),
                "LDAP_Enable" if val.as_bool().unwrap_or(false) => {
                    out.sso_providers.push(SsoProvider {
                        id: "ldap".to_string(),
                        name: "LDAP".to_string(),
                        kind: "ldap".to_string(),
                    });
                }
                "CAS_enabled" if val.as_bool().unwrap_or(false) => {
                    out.sso_providers.push(SsoProvider {
                        id: "cas".to_string(),
                        name: "CAS".to_string(),
                        kind: "cas".to_string(),
                    });
                }
                other if other.starts_with("Accounts_OAuth_") && !other.ends_with("_id") && !other.ends_with("_secret") => {
                    if val.as_bool().unwrap_or(false) {
                        let name = other.trim_start_matches("Accounts_OAuth_").to_string();
                        if !name.is_empty() && !name.contains('_') {
                            out.sso_providers.push(SsoProvider {
                                id: name.to_ascii_lowercase(),
                                name,
                                kind: "oauth".to_string(),
                            });
                        }
                    }
                }
                other if other.starts_with("SAML_Custom_") && other.ends_with("_enabled") => {
                    if val.as_bool().unwrap_or(false) {
                        let name = other
                            .trim_start_matches("SAML_Custom_")
                            .trim_end_matches("_enabled")
                            .to_string();
                        out.sso_providers.push(SsoProvider {
                            id: format!("saml:{name}"),
                            name: format!("SAML ({name})"),
                            kind: "saml".to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    out
}

/// Fetch `/api/info` and `/api/v1/settings.public` from a Rocket.Chat server.
pub async fn discover(route_key: &str, raw_server_url: &str) -> anyhow::Result<ServerInfo> {
    let base = super::normalize_server_url(raw_server_url)?;
    let router = crate::net::route::router();
    let http = router.client_if("rocketchat", router.routed(route_key), |b| {
        b.user_agent(super::user_agent()).timeout(Duration::from_secs(20))
    });

    let info_resp = http
        .get(format!("{base}/api/info"))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach Rocket.Chat server at {base}: {e}"))?;
    if !info_resp.status().is_success() {
        anyhow::bail!("Rocket.Chat /api/info returned HTTP {}", info_resp.status().as_u16());
    }
    let info_json: Value = info_resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("invalid JSON from /api/info: {e}"))?;

    // settings.public is best-effort (older or locked-down servers may paginate or restrict it)
    let settings_json = match http
        .get(format!("{base}/api/v1/settings.public"))
        .query(&[("count", "0")])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp.json::<Value>().await.unwrap_or(Value::Null),
        _ => Value::Null,
    };

    Ok(parse_server_info(&base, &info_json, &settings_json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_version_and_public_settings() {
        let info = json!({ "version": "7.2.1-rc.1", "success": true });
        let settings = json!({
            "settings": [
                { "_id": "Site_Name", "value": "Acme Chat" },
                { "_id": "E2E_Enable", "value": true },
                { "_id": "E2E_Enabled_Default_DirectRooms", "value": true },
                { "_id": "FileUpload_MaxFileSize", "value": 52428800 },
                { "_id": "Message_Read_Receipt_Enabled", "value": true },
                { "_id": "Federation_Matrix_enabled", "value": true },
                { "_id": "Accounts_OAuth_Google", "value": true },
                { "_id": "LDAP_Enable", "value": true },
                { "_id": "Katex_Enabled", "value": true },
                { "_id": "Livechat_enabled", "value": true },
            ]
        });
        let parsed = parse_server_info("https://chat.acme.test", &info, &settings);
        assert_eq!(parsed.version.major, 7);
        assert_eq!(parsed.version.minor, 2);
        assert_eq!(parsed.version.patch, 1);
        assert!(parsed.version.supports_aes_gcm_e2ee());
        assert_eq!(parsed.site_name, "Acme Chat");
        assert!(parsed.e2ee_enabled);
        assert!(parsed.e2ee_default_dm);
        assert_eq!(parsed.max_upload_bytes, 52_428_800);
        assert!(parsed.read_receipts_enabled);
        assert!(parsed.federation_enabled);
        assert!(parsed.katex_enabled);
        assert!(parsed.livechat_enabled);
        assert_eq!(parsed.sso_providers.len(), 2);
    }
}
