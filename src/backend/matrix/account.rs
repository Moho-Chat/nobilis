//! The account itself: the addresses that can reach it, and closing it.
//!
//! Both are user-interactive-auth gated - the server refuses the first
//! attempt with a 401 carrying a session, and the second carries the
//! password. That is deliberate on Matrix's part and it is the same dance
//! signing another device out already does (see http's
//! `post_with_password_uia`), so these reuse it rather than describing it
//! again.
//!
//! Adding an email needs the homeserver rather than an identity server:
//! since the v3 API the homeserver sends the mail itself, and binding to a
//! public identity server is a separate act nobody here has asked for. So the
//! flow is the two steps it really is - ask for a link, then say the link has
//! been followed - rather than a button that silently means both.

use super::*;

fn base(account: &crate::accounts::MatrixAccountConfig) -> String {
    account.homeserver_url.trim_end_matches('/').to_string()
}

/// Every address that can reach this account.
pub async fn third_party_ids(state: &AppState, account_id: &str) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let answer = http::get_json(&format!("{}/_matrix/client/v3/account/3pid", base(&account)), &account.access_token)
        .await
        .context("reading the account's addresses")?;
    let listed: Vec<Value> = answer["threepids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(serde_json::json!({
                "medium": entry["medium"].as_str()?,
                "address": entry["address"].as_str()?,
                // Seconds, like every other time this daemon hands out; the
                // endpoint answers in milliseconds.
                "addedAt": entry["added_at"].as_i64().map(|ms| ms / 1000),
            }))
        })
        .collect();
    Ok(serde_json::json!({ "threepids": listed }))
}

/// A secret this client makes up, tying the two halves of adding an address
/// together.
///
/// The spec's `client_secret`: the server quotes it back in the link it
/// mails, and the second half of the flow proves this is the same client that
/// asked. Random rather than derived from anything, which is the point of it.
fn client_secret() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Asks the homeserver to mail a confirmation link.
///
/// Hands back the session id and the secret, because the second half of the
/// flow needs both and neither can be re-derived.
pub async fn request_email_token(state: &AppState, account_id: &str, address: &str) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let secret = client_secret();
    let answer = http::post_json(
        &format!("{}/_matrix/client/v3/account/3pid/email/requestToken", base(&account)),
        Some(&account.access_token),
        // `send_attempt` is how the server tells "send it again" from "the
        // request was retried": the same number means the same mail, a higher
        // one means send another. One, because this is the first ask - a
        // person who wants it sent again starts the flow over.
        serde_json::json!({ "client_secret": secret, "email": address, "send_attempt": 1 }),
    )
    .await
    .context("asking the server to send a confirmation")?;
    let sid = answer["sid"].as_str().context("the server sent no session id")?;
    Ok(serde_json::json!({ "sid": sid, "clientSecret": secret }))
}

/// Finishes adding an address, once the link in the mail has been followed.
pub async fn add_third_party_id(
    state: &AppState,
    account_id: &str,
    sid: &str,
    secret: &str,
    password: &str,
) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    http::post_with_password_uia(
        &format!("{}/_matrix/client/v3/account/3pid/add", base(&account)),
        &account.access_token,
        &account.user_id,
        password,
        serde_json::json!({ "sid": sid, "client_secret": secret }),
    )
    .await
    .context("adding the address")?;
    Ok(())
}

/// Takes an address off the account.
///
/// Not user-interactive: removing an address is not the dangerous direction,
/// and the spec does not gate it. The answer says whether the *identity
/// server* also let go, which can be "no idea" - the homeserver has forgotten
/// it either way, and that is the part that governs who can sign in.
pub async fn remove_third_party_id(state: &AppState, account_id: &str, medium: &str, address: &str) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let answer = http::post_json(
        &format!("{}/_matrix/client/v3/account/3pid/delete", base(&account)),
        Some(&account.access_token),
        serde_json::json!({ "medium": medium, "address": address }),
    )
    .await
    .context("removing the address")?;
    Ok(serde_json::json!({ "idServer": answer["id_server_unbind_result"].as_str().unwrap_or("no-support") }))
}

/// How this account can be closed, as a client can tell before trying.
///
/// A homeserver that keeps its own accounts takes the request here, with the
/// password. One that hands its accounts to an OAuth provider (Matrix
/// Authentication Service - matrix.org's, among others) does not: its
/// homeserver answers the request with M_UNRECOGNIZED, and the account is the
/// provider's to close, on its own account page. That page may say outright
/// that it can (MSC4191's `org.matrix.account_deactivate`), in which case the
/// link opens that action directly; it may not, and then all a client can do
/// is send the person to the page and say so.
pub async fn deactivation_route(state: &AppState, account_id: &str) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("no such Matrix account")?;
    let base = base(&account);
    let mut metadata = None;
    for path in ["/_matrix/client/v1/auth_metadata", "/_matrix/client/unstable/org.matrix.msc2965/auth_metadata"] {
        if let Ok(v) = http::get_json_anonymous(&format!("{base}{path}")).await {
            if v["issuer"].as_str().is_some_and(|i| !i.is_empty()) {
                metadata = Some(v);
                break;
            }
        }
    }
    Ok(route_from_metadata(metadata.as_ref()))
}

/// The decision behind `deactivation_route`, given what the homeserver said
/// about who holds its accounts - or nothing, where it holds its own.
pub(super) fn route_from_metadata(metadata: Option<&Value>) -> Value {
    let Some(m) = metadata else {
        return serde_json::json!({ "route": "password" });
    };
    let Some(page) = m["account_management_uri"].as_str().filter(|u| !u.is_empty()) else {
        return serde_json::json!({ "route": "none" });
    };
    let direct = m["account_management_actions_supported"]
        .as_array()
        .is_some_and(|a| a.iter().any(|x| x == "org.matrix.account_deactivate"));
    let url = if direct {
        let separator = if page.contains('?') { '&' } else { '?' };
        format!("{page}{separator}action=org.matrix.account_deactivate")
    } else {
        page.to_string()
    };
    serde_json::json!({ "route": "page", "url": url, "direct": direct })
}

/// Closes the account on the homeserver.
///
/// Irreversible, and the client's job here is to be honest about that rather
/// than to soften it: the account cannot be signed in to again, its device
/// keys are gone, and its encrypted messages become unreadable to everybody
/// who did not already have the keys.
///
/// `erase` is the spec's request that the homeserver redact this account's
/// messages as well. Off unless asked, because it is a separate decision and
/// a much larger one - and because a server is allowed to ignore it, so a
/// client promising it would be promising something it cannot deliver.
pub async fn deactivate(state: &AppState, account_id: &str, password: &str, erase: bool) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    http::post_with_password_uia(
        &format!("{}/_matrix/client/v3/account/deactivate", base(&account)),
        &account.access_token,
        &account.user_id,
        password,
        serde_json::json!({ "erase": erase }),
    )
    .await
    .map_err(|e| {
        // What a homeserver whose accounts belong to an OAuth provider says
        // to this request. Translated, because "unrecognized request" reads
        // like moho sent something malformed, when the truth is that this
        // account is closed somewhere else - see deactivation_route.
        if format!("{e:#}").contains("M_UNRECOGNIZED") {
            anyhow::anyhow!("this homeserver closes accounts on its own account page, not through a client")
        } else {
            e.context("closing the account")
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod deactivation_tests {
    use super::route_from_metadata;
    use serde_json::json;

    #[test]
    fn a_server_that_keeps_its_own_accounts_takes_the_password() {
        assert_eq!(route_from_metadata(None)["route"], "password");
    }

    /// matrix.org's answer: a provider, an account page, and no
    /// deactivation among the actions it lists.
    #[test]
    fn a_provider_without_the_action_is_sent_to_its_page_and_said_so() {
        let m = json!({
            "issuer": "https://account.matrix.org/",
            "account_management_uri": "https://account.matrix.org/account/",
            "account_management_actions_supported": ["org.matrix.profile", "org.matrix.sessions_list"]
        });
        let route = route_from_metadata(Some(&m));
        assert_eq!(route["route"], "page");
        assert_eq!(route["direct"], false);
        assert_eq!(route["url"], "https://account.matrix.org/account/");
    }

    #[test]
    fn a_provider_with_the_action_opens_it_directly() {
        let m = json!({
            "issuer": "https://mas.example/",
            "account_management_uri": "https://mas.example/account/",
            "account_management_actions_supported": ["org.matrix.account_deactivate"]
        });
        let route = route_from_metadata(Some(&m));
        assert_eq!(route["direct"], true);
        assert_eq!(route["url"], "https://mas.example/account/?action=org.matrix.account_deactivate");
    }

    #[test]
    fn a_provider_with_no_account_page_leaves_nothing_to_offer() {
        assert_eq!(route_from_metadata(Some(&json!({ "issuer": "https://mas.example/" })))["route"], "none");
    }
}
