//! Rocket.Chat authentication flows:
//! 1. Username/email + password + optional TOTP/email 2FA (`POST /api/v1/login`).
//! 2. Personal Access Token (`X-Auth-Token` + `X-User-Id`, verified via `GET /api/v1/me`).
//! 3. OAuth / SAML / OIDC / LDAP SSO token capture (`Meteor.loginToken`, verified via `GET /api/v1/me`).
//! 4. Session logout (`POST /api/v1/logout`, `POST /api/v1/users.logoutOtherClients`).

use super::api::{Api, ApiError};
use super::{normalize_server_url, server_host};
use crate::accounts::RocketChatAccountConfig;
use serde_json::{json, Value};

#[derive(Debug)]
pub enum LoginOutcome {
    SignedIn(RocketChatAccountConfig),
    TotpRequired { method: String },
}

/// Sign in with username/email + password (and optional 2FA `code`).
pub async fn login_password(
    route_key: &str,
    raw_server_url: &str,
    user: &str,
    password: &str,
    totp_code: Option<&str>,
    use_tor: bool,
) -> Result<LoginOutcome, ApiError> {
    let server_url = normalize_server_url(raw_server_url).map_err(|e| ApiError::Network(e.to_string()))?;
    let host = server_host(&server_url).map_err(|e| ApiError::Network(e.to_string()))?;
    let api = Api::new(route_key, &server_url, None, None);

    let mut payload = json!({
        "user": user.trim(),
        "password": password,
    });
    if let Some(code) = totp_code.map(str::trim).filter(|c| !c.is_empty()) {
        payload["code"] = json!(code);
    }

    match api.post("/api/v1/login", &payload).await {
        Ok(resp) => {
            let data = &resp["data"];
            let user_id = data["userId"]
                .as_str()
                .ok_or_else(|| ApiError::Refused {
                    status: 500,
                    error_type: "invalid-response".into(),
                    message: "Rocket.Chat login response missing userId".into(),
                })?
                .to_string();
            let auth_token = data["authToken"]
                .as_str()
                .ok_or_else(|| ApiError::Refused {
                    status: 500,
                    error_type: "invalid-response".into(),
                    message: "Rocket.Chat login response missing authToken".into(),
                })?
                .to_string();
            let username = data["me"]["username"]
                .as_str()
                .unwrap_or(user.trim())
                .to_string();
            let display = data["me"]["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            Ok(LoginOutcome::SignedIn(RocketChatAccountConfig {
                server_url,
                server_host: host,
                user_id,
                username,
                auth_token,
                e2ee_passphrase: None,
                made_here: true,
                display_name: display,
                default_status: None,
                omnichannel_enabled: false,
                use_tor,
                strict_route: false,
            }))
        }
        Err(ApiError::TotpRequired { method }) => Ok(LoginOutcome::TotpRequired { method }),
        Err(e) => Err(e),
    }
}

/// Sign in with a Personal Access Token (`X-Auth-Token` + `X-User-Id`), verified via `GET /api/v1/me`.
pub async fn login_token(
    route_key: &str,
    raw_server_url: &str,
    user_id: &str,
    auth_token: &str,
    made_here: bool,
    use_tor: bool,
) -> Result<RocketChatAccountConfig, ApiError> {
    let server_url = normalize_server_url(raw_server_url).map_err(|e| ApiError::Network(e.to_string()))?;
    let host = server_host(&server_url).map_err(|e| ApiError::Network(e.to_string()))?;
    let uid = user_id.trim().to_string();
    let tok = auth_token.trim().to_string();

    let api = Api::new(route_key, &server_url, (!uid.is_empty()).then(|| uid.clone()), Some(tok.clone()));

    // If user_id wasn't supplied (e.g., SSO Meteor.loginToken capture), exchange resume token via POST /api/v1/login
    let (resolved_uid, me) = if uid.is_empty() {
        let login_res = api.post("/api/v1/login", &json!({ "resume": tok })).await?;
        let data = &login_res["data"];
        let id = data["userId"]
            .as_str()
            .ok_or(ApiError::Unauthorized)?
            .to_string();
        let new_tok = data["authToken"].as_str().unwrap_or(&tok).to_string();
        api.set_credentials(id.clone(), new_tok);
        let me = api.get("/api/v1/me", &[]).await?;
        (id, me)
    } else {
        let me = api.get("/api/v1/me", &[]).await?;
        let id = me["_id"].as_str().unwrap_or(&uid).to_string();
        (id, me)
    };

    let username = me["username"].as_str().unwrap_or("").to_string();
    let display = me["name"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let (_, final_tok) = api.credentials();

    Ok(RocketChatAccountConfig {
        server_url,
        server_host: host,
        user_id: resolved_uid,
        username,
        auth_token: final_tok.unwrap_or(tok),
        e2ee_passphrase: None,
        made_here,
        display_name: display,
        default_status: None,
        omnichannel_enabled: false,
        use_tor,
        strict_route: false,
    })
}

/// End this session on the server (`POST /api/v1/logout`).
pub async fn logout(api: &Api) -> Result<Value, ApiError> {
    api.post("/api/v1/logout", &json!({})).await
}

/// Log out all other active sessions (`POST /api/v1/users.logoutOtherClients`).
pub async fn logout_other_clients(api: &Api) -> Result<Value, ApiError> {
    api.post("/api/v1/users.logoutOtherClients", &json!({})).await
}
