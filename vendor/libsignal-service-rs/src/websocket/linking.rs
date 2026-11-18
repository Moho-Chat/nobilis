use libsignal_core::DeviceId;
use reqwest::Method;

use crate::push_service::response::SignalServiceResponse;
use crate::websocket::{self, SignalWebSocket};

use super::ServiceError;

impl SignalWebSocket<websocket::Identified> {
    pub async fn unlink_device(
        &mut self,
        id: DeviceId,
    ) -> Result<(), ServiceError> {
        self.http_request(Method::DELETE, format!("/v1/devices/{}", id))?
            .send()
            .await?
            .service_error_for_status()
            .await?;

        Ok(())
    }
}

/// moho: where the phone put the message history it transfers to a newly linked device, or why there is none.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum TransferArchive {
    Ready { cdn: u32, key: String },
    /// `RELINK_REQUESTED` (link again) or `CONTINUE_WITHOUT_UPLOAD` (no history after all).
    Failed { error: String },
}

impl SignalWebSocket<websocket::Identified> {
    /// moho: waits up to `timeout_secs` for the phone to say where the transferred history is
    /// (`GET /v1/devices/transfer_archive`). `None`: nothing yet, ask again.
    pub async fn transfer_archive(
        &mut self,
        timeout_secs: u32,
    ) -> Result<Option<TransferArchive>, ServiceError> {
        let response = self
            .http_request(
                Method::GET,
                format!("/v1/devices/transfer_archive?timeout={timeout_secs}"),
            )?
            .send()
            .await?;
        if response.status_code().map(|s| s.as_u16()) == Some(204) {
            return Ok(None);
        }
        Ok(Some(response.service_error_for_status().await?.json().await?))
    }
}
