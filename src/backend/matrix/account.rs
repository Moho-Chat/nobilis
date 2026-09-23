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
    .context("closing the account")?;
    Ok(())
}
