//! Signing in the way a homeserver with no password does.
//!
//! A growing number of homeservers - matrix.org among them since it moved to
//! its own authentication service - delegate their accounts to an OAuth 2.0
//! provider. They keep a compatibility `m.login.password` for now, but the
//! real account lives elsewhere, and the flows that go with it are the ones
//! their own clients use.
//!
//! This is the **device authorization grant** (RFC 8628): the client asks for
//! a short code, the person types that code into a page they are already
//! signed in to, and the client polls until it is approved. No password is
//! typed into moho, no browser is embedded, and nothing here ever sees a
//! credential.
//!
//! Not MSC4108, which is the QR flow Element X leads with - that carries this
//! same grant over a rendezvous channel, and no homeserver reachable from
//! here serves a rendezvous endpoint (see #222). The grant underneath it is
//! available today, and is most of what somebody wants from that flow.

use super::*;
use std::time::Duration;

/// What the provider says about itself.
///
/// Read from the homeserver rather than the provider, because the homeserver
/// is the only name the person typed: `/auth_metadata` is how a homeserver
/// points at whoever holds its accounts.
#[derive(Debug, Clone)]
pub struct AuthMetadata {
    pub issuer: String,
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
    /// Where somebody manages the account itself. Worth keeping: it is the
    /// answer to "change my password", which this client can no longer do.
    pub account_management_uri: Option<String>,
}

/// Where the metadata lives, newest spelling first.
///
/// The stable path arrived in Matrix 1.15; before that it was MSC2965's
/// unstable one, which servers still serve and some serve only. Both are
/// tried because a client that asks for only the new one calls a working
/// homeserver password-only.
const METADATA_PATHS: [&str; 2] =
    ["/_matrix/client/v1/auth_metadata", "/_matrix/client/unstable/org.matrix.msc2965/auth_metadata"];

/// Whether this homeserver delegates its accounts, and to whom.
///
/// `Ok(None)` for a homeserver that does not - which is not an error and must
/// not read as one, since it is the ordinary case for most of Matrix.
pub async fn auth_metadata(homeserver_url: &str) -> Result<Option<AuthMetadata>> {
    let base = homeserver_url.trim_end_matches('/');
    for path in METADATA_PATHS {
        let Ok(v) = http::get_json_anonymous(&format!("{base}{path}")).await else { continue };
        let field = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let issuer = field("issuer");
        let device = field("device_authorization_endpoint");
        let token = field("token_endpoint");
        let registration = field("registration_endpoint");
        // All four or none. A provider advertising itself without a device
        // endpoint cannot do this flow, and saying "sign in with a code" to
        // somebody it will then fail for is worse than not offering it.
        if issuer.is_empty() || device.is_empty() || token.is_empty() || registration.is_empty() {
            continue;
        }
        return Ok(Some(AuthMetadata {
            issuer,
            device_authorization_endpoint: device,
            token_endpoint: token,
            registration_endpoint: registration,
            account_management_uri: v["account_management_uri"].as_str().map(str::to_string),
        }));
    }
    Ok(None)
}

/// Gets moho a client id from the provider.
///
/// Registered afresh rather than shipped as a constant, because there is no
/// constant to ship: every provider issues its own, and a client id is not a
/// secret - it names the application, and this application is one anybody can
/// build. `token_endpoint_auth_method: none` says so outright.
pub async fn register_client(metadata: &AuthMetadata) -> Result<String> {
    let body = serde_json::json!({
        "client_name": "moho",
        "client_uri": "https://github.com/Moho-Chat/moho",
        "application_type": "native",
        "token_endpoint_auth_method": "none",
        "grant_types": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
        // Empty on purpose: this flow never sends anybody back to a URL, so
        // registering one would be claiming a capability this does not use.
        "response_types": [],
        "redirect_uris": [],
    });
    let resp = http::post_json_anonymous(&metadata.registration_endpoint, body)
        .await
        .context("registering with the account provider")?;
    resp["client_id"].as_str().map(str::to_string).context("the provider issued no client id")
}

/// The code to show somebody, and what to poll with.
#[derive(Debug, Clone)]
pub struct DeviceGrant {
    pub device_code: String,
    /// The short code a person types. Shown, never logged.
    pub user_code: String,
    pub verification_uri: String,
    /// The same page with the code already in it, where the provider offers
    /// one - which turns typing a code into following a link.
    pub verification_uri_complete: Option<String>,
    pub interval: Duration,
    pub expires_in: Duration,
}

/// The Matrix scopes a client needs, and the device it is asking to become.
///
/// The device id is chosen here rather than by the server, because it is part
/// of the scope string: the grant is asking to be *that* device, and the
/// access token that comes back is bound to it.
fn scope(device_id: &str) -> String {
    format!("urn:matrix:org.matrix.msc2967.client:api:* urn:matrix:org.matrix.msc2967.client:device:{device_id}")
}

pub async fn request_device_code(metadata: &AuthMetadata, client_id: &str, device_id: &str) -> Result<DeviceGrant> {
    let resp = http::post_form_anonymous(
        &metadata.device_authorization_endpoint,
        &[("client_id", client_id), ("scope", &scope(device_id))],
    )
    .await
    .context("asking for a sign-in code")?;

    Ok(DeviceGrant {
        device_code: resp["device_code"].as_str().context("no device code came back")?.to_string(),
        user_code: resp["user_code"].as_str().context("no user code came back")?.to_string(),
        verification_uri: resp["verification_uri"].as_str().unwrap_or_default().to_string(),
        verification_uri_complete: resp["verification_uri_complete"].as_str().map(str::to_string),
        // Defaults from RFC 8628 where the provider does not say. Polling
        // faster than told is what earns a `slow_down`.
        interval: Duration::from_secs(resp["interval"].as_u64().unwrap_or(5).clamp(1, 60)),
        expires_in: Duration::from_secs(resp["expires_in"].as_u64().unwrap_or(600).clamp(30, 1800)),
    })
}

/// Where a poll got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// Nobody has approved it yet. Keep waiting.
    Pending,
    /// Polling too fast; the interval goes up by five seconds and stays up.
    SlowDown,
    /// The code ran out, or was refused. Either way this attempt is over.
    Stopped(String),
    /// Signed in. The refresh token matters as much as the access one: the
    /// access token expires in minutes on matrix.org, and an account signed
    /// in this way has no password to fall back on.
    Approved { access_token: String, refresh_token: String, expires_in: u64 },
}

/// Reads one poll of the token endpoint.
///
/// Separate from the polling loop so the classification can be tested without
/// a provider: every branch here is a different thing to do next, and getting
/// one wrong means either a sign-in that never completes or one that spins.
pub fn classify(status: u16, body: &Value) -> Poll {
    if let Some(token) = body["access_token"].as_str() {
        if !token.is_empty() {
            return Poll::Approved {
                access_token: token.to_string(),
                refresh_token: body["refresh_token"].as_str().unwrap_or_default().to_string(),
                expires_in: body["expires_in"].as_u64().unwrap_or(0),
            };
        }
    }
    match body["error"].as_str().unwrap_or_default() {
        "authorization_pending" => Poll::Pending,
        "slow_down" => Poll::SlowDown,
        "expired_token" => Poll::Stopped("the sign-in code expired".to_string()),
        "access_denied" => Poll::Stopped("the sign-in was refused".to_string()),
        other if !other.is_empty() => Poll::Stopped(
            body["error_description"].as_str().filter(|d| !d.is_empty()).unwrap_or(other).to_string(),
        ),
        // A status with no error code in it: report the status rather than
        // silently waiting for a provider that has stopped talking sense.
        _ => Poll::Stopped(format!("the provider answered HTTP {status} with no explanation")),
    }
}

pub async fn poll_once(metadata: &AuthMetadata, client_id: &str, device_code: &str) -> Poll {
    let (status, body) = match http::post_form_anonymous_raw(
        &metadata.token_endpoint,
        &[
            ("client_id", client_id),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
        ],
    )
    .await
    {
        Ok(v) => v,
        // A single failed request is not a failed sign-in - the code is good
        // for twenty minutes and a network blip inside that is ordinary.
        Err(e) => {
            tracing::debug!("matrix device login: a poll failed: {e:#}");
            return Poll::Pending;
        }
    };
    classify(status, &body)
}

/// A fresh access token, from the refresh token stored at sign-in.
///
/// The other half of the device grant, and the half without which it is a
/// sign-in that stops working: matrix.org's access tokens last minutes, and
/// an account signed in by code has no password to try again with. Without
/// this the account simply went dead - `M_UNKNOWN_TOKEN`, a re-login attempt
/// against an empty password, and a reconnect loop.
///
/// Returns the new pair. Providers may or may not rotate the refresh token;
/// where the answer carries a new one it replaces the old, and where it does
/// not the old one stays valid.
pub async fn refresh(metadata: &AuthMetadata, client_id: &str, refresh_token: &str) -> Result<(String, String)> {
    let (status, body) = http::post_form_anonymous_raw(
        &metadata.token_endpoint,
        &[("client_id", client_id), ("grant_type", "refresh_token"), ("refresh_token", refresh_token)],
    )
    .await
    .context("asking for a fresh access token")?;
    let access = body["access_token"].as_str().unwrap_or_default();
    if access.is_empty() {
        let why = body["error_description"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("the provider gave no reason");
        anyhow::bail!("refusing to refresh the session (HTTP {status}): {why}");
    }
    let rotated = body["refresh_token"].as_str().filter(|t| !t.is_empty()).unwrap_or(refresh_token);
    Ok((access.to_string(), rotated.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn waiting_is_not_failing() {
        assert_eq!(classify(403, &json!({ "error": "authorization_pending" })), Poll::Pending);
        assert_eq!(classify(400, &json!({ "error": "slow_down" })), Poll::SlowDown);
    }

    /// The refresh token is part of the approval, not an extra. Dropping it
    /// is what made a code sign-in work until the first expiry and then
    /// never again, with no password to fall back on.
    #[test]
    fn an_approval_carries_both_tokens() {
        let p = classify(200, &json!({
            "access_token": "syt_x", "token_type": "Bearer",
            "refresh_token": "mar_y", "expires_in": 300
        }));
        assert_eq!(p, Poll::Approved {
            access_token: "syt_x".to_string(),
            refresh_token: "mar_y".to_string(),
            expires_in: 300,
        });
    }

    /// The two ends of the flow, which must stop it rather than spin.
    #[test]
    fn expiry_and_refusal_both_stop() {
        assert!(matches!(classify(400, &json!({ "error": "expired_token" })), Poll::Stopped(m) if m.contains("expired")));
        assert!(matches!(classify(400, &json!({ "error": "access_denied" })), Poll::Stopped(m) if m.contains("refused")));
    }

    /// An error nobody anticipated is still an end, and says what it was
    /// rather than "something went wrong".
    #[test]
    fn an_unknown_error_reports_itself() {
        let p = classify(400, &json!({ "error": "invalid_client", "error_description": "no such client" }));
        assert!(matches!(p, Poll::Stopped(m) if m == "no such client"));
        let bare = classify(400, &json!({ "error": "invalid_grant" }));
        assert!(matches!(bare, Poll::Stopped(m) if m == "invalid_grant"));
    }

    /// An empty body must not read as approval - the token field is absent,
    /// and treating that as success would store an empty access token.
    #[test]
    fn nothing_at_all_is_not_approval() {
        assert!(matches!(classify(500, &json!({})), Poll::Stopped(m) if m.contains("500")));
        assert!(matches!(classify(200, &json!({ "access_token": "" })), Poll::Stopped(_)));
    }

    #[test]
    fn the_scope_names_the_device_it_is_asking_to_be() {
        let s = scope("ABCD");
        assert!(s.contains("client:api:*"));
        assert!(s.ends_with("client:device:ABCD"));
    }
}
