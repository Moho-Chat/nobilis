//! moho: the chat server's half of call links (signal.link addresses): the credentials the calling server (SFU) wants
//! before it makes, reads or lets anyone into a call link's room. The rooms themselves are RingRTC's business.

use base64::{prelude::BASE64_STANDARD, Engine};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::content::ServiceError;
use crate::push_service::response::SignalServiceResponse;

use super::{Identified, SignalWebSocket};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TemporalCredential {
    credential: String,
    redemption_time: u64,
}

impl SignalWebSocket<Identified> {
    /// The call link auth credentials (serialized `CallLinkAuthCredentialResponse`s) for the days from `start` to
    /// `end` (seconds since the epoch, day-aligned), by redemption time: they come with the group credentials
    /// (`GET /v1/certificate/auth/group`).
    pub async fn call_link_auth_credentials(
        &mut self,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, ServiceError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            #[serde(default)]
            call_link_auth_credentials: Vec<TemporalCredential>,
        }
        let path = format!(
            "/v1/certificate/auth/group?redemptionStartSeconds={start}&redemptionEndSeconds={end}&pniAsServiceId=true"
        );
        let response: Response = self
            .http_request(Method::GET, path)?
            .send()
            .await?
            .service_error_for_status()
            .await?
            .json()
            .await?;
        response
            .call_link_auth_credentials
            .into_iter()
            .map(|c| Ok((c.redemption_time, BASE64_STANDARD.decode(c.credential)?)))
            .collect()
    }

    /// Asks for the credential to make a call link's room (`POST /v1/call-link/create-auth`) from a serialized
    /// `CreateCallLinkCredentialRequest`. Answers the serialized `CreateCallLinkCredentialResponse`.
    pub async fn create_call_link_credential(
        &mut self,
        request: &[u8],
    ) -> Result<Vec<u8>, ServiceError> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request {
            create_call_link_credential_request: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            credential: String,
        }
        let response: Response = self
            .http_request(Method::POST, "/v1/call-link/create-auth")?
            .send_json(Request {
                create_call_link_credential_request: BASE64_STANDARD.encode(request),
            })
            .await?
            .service_error_for_status()
            .await?
            .json()
            .await?;
        Ok(BASE64_STANDARD.decode(response.credential)?)
    }
}
