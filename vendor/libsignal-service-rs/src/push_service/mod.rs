use std::{sync::LazyLock, time::Duration};

use crate::{
    configuration::{Endpoint, ServiceCredentials, SignalServers},
    prelude::ServiceConfiguration,
    utils::serde_device_id_vec,
    websocket::{SignalWebSocket, WebSocketType},
};

use libsignal_core::DeviceId;
use protobuf::ProtobufResponseExt;
use reqwest::{Method, RequestBuilder};
use reqwest_websocket::Upgrade;
use serde::{Deserialize, Serialize};
use tracing::{debug_span, Instrument};

pub const KEEPALIVE_TIMEOUT_SECONDS: Duration = Duration::from_secs(55);
pub static DEFAULT_DEVICE_ID: LazyLock<libsignal_core::DeviceId> =
    LazyLock::new(|| libsignal_core::DeviceId::try_from(1).unwrap());

mod account;
mod cdn;
mod error;
pub mod linking;
pub(crate) mod response;

pub use account::*;
pub use cdn::*;
pub use error::*;
pub(crate) use response::SignalServiceResponse;

#[derive(Debug, Serialize, Deserialize)]
pub struct ProofRequired {
    pub token: String,
    pub options: Vec<String>,
}

#[derive(derive_more::Debug, Clone, Serialize, Deserialize)]
pub struct HttpAuth {
    pub username: String,
    #[debug(ignore)]
    pub password: String,
}

#[derive(Debug, Clone)]
pub enum HttpAuthOverride {
    NoOverride,
    Unidentified,
    Identified(HttpAuth),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AvatarWrite<C> {
    NewAvatar(C),
    RetainAvatar,
    NoAvatar,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MismatchedDevices {
    #[serde(with = "serde_device_id_vec")]
    pub missing_devices: Vec<DeviceId>,
    #[serde(with = "serde_device_id_vec")]
    pub extra_devices: Vec<DeviceId>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleDevices {
    #[serde(with = "serde_device_id_vec")]
    pub stale_devices: Vec<DeviceId>,
}

/// moho's patch: how the `PushService`s made on this thread reach Signal.
///
/// Every request this crate makes - REST, the attachment CDNs and both websockets - goes through the one `reqwest`
/// client `PushService::new` builds, and that constructor is called deep inside presage with no way to hand it a
/// proxy or a user agent. moho runs each account's library on a thread of its own (the stores are not `Send`), so the
/// setting is per thread: set once when the thread starts, before any `PushService` exists. A proxy that cannot be
/// read fails closed - requests go to an address that refuses them - rather than going out directly.
pub mod network {
    use std::cell::RefCell;

    #[derive(Clone, Debug, Default)]
    pub struct Network {
        /// `socks5h://host:port` (or `socks5://`), or `None` to connect directly.
        pub proxy: Option<String>,
        /// What the client calls itself, instead of the one the caller passed.
        pub user_agent: Option<String>,
        /// Whether presage may fetch a link's page to build a preview for a message being sent. Off unless set:
        /// fetching shows the page's server where the person is.
        pub link_previews: bool,
    }

    thread_local! {
        static NETWORK: RefCell<Network> = RefCell::new(Network::default());
    }

    /// Sets how the `PushService`s made on this thread from now on connect.
    pub fn set_for_this_thread(network: Network) {
        NETWORK.with(|n| *n.borrow_mut() = network);
    }

    pub fn for_this_thread() -> Network {
        NETWORK.with(|n| n.borrow().clone())
    }

    /// Where a proxy that could not be read sends requests: nothing listens on the discard port.
    pub const NOWHERE: &str = "socks5h://127.0.0.1:9";
}

#[derive(Clone)]
pub struct PushService {
    pub(crate) servers: SignalServers,
    cfg: ServiceConfiguration,
    credentials: Option<HttpAuth>,
    client: reqwest::Client,
}

impl PushService {
    pub fn new(
        env: SignalServers,
        credentials: Option<ServiceCredentials>,
        user_agent: impl AsRef<str>,
    ) -> Self {
        let cfg: ServiceConfiguration = env.into();

        // Use the ring provider except if the application already installed one.
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }

        let network = network::for_this_thread();
        let user_agent = network
            .user_agent
            .clone()
            .unwrap_or_else(|| user_agent.as_ref().to_string());
        let mut builder = reqwest::ClientBuilder::new()
            .tls_certs_only([reqwest::Certificate::from_pem(
                cfg.certificate_authority.as_bytes(),
            )
            .unwrap()])
            .connect_timeout(Duration::from_secs(10))
            // Not a total timeout: attachments take as long as they take to transfer.
            .read_timeout(Duration::from_secs(65))
            .user_agent(user_agent)
            .http1_only();
        builder = match network.proxy.as_deref() {
            // A routed account never falls back to the environment's proxy or a direct connection.
            Some(proxy) => builder.no_proxy().proxy(
                reqwest::Proxy::all(proxy).unwrap_or_else(|_| {
                    reqwest::Proxy::all(network::NOWHERE)
                        .expect("a fixed proxy address")
                }),
            ),
            None => builder,
        };
        let client = builder.build().unwrap();

        Self {
            servers: env,
            cfg,
            credentials: credentials.and_then(|c| c.authorization()),
            client,
        }
    }

    #[tracing::instrument(skip(self), fields(endpoint = %endpoint))]
    pub fn request(
        &self,
        method: Method,
        endpoint: Endpoint,
        auth_override: HttpAuthOverride,
    ) -> Result<RequestBuilder, ServiceError> {
        let url = endpoint.into_url(&self.cfg)?;
        let mut builder = self.client.request(method, url);

        builder = match auth_override {
            HttpAuthOverride::NoOverride => {
                if let Some(HttpAuth { username, password }) =
                    self.credentials.as_ref()
                {
                    builder.basic_auth(username, Some(password))
                } else {
                    builder
                }
            },
            HttpAuthOverride::Identified(HttpAuth { username, password }) => {
                builder.basic_auth(username, Some(password))
            },
            HttpAuthOverride::Unidentified => builder,
        };

        Ok(builder)
    }

    pub async fn ws<C: WebSocketType>(
        &mut self,
        path: &str,
        keepalive_path: &str,
        additional_headers: &[(&'static str, &str)],
        credentials: Option<ServiceCredentials>,
    ) -> Result<SignalWebSocket<C>, ServiceError> {
        let span = debug_span!("websocket");

        let mut url = Endpoint::service(path).into_url(&self.cfg)?;
        url.set_scheme("wss").expect("valid https base url");

        let mut builder = self.client.get(url);
        for (key, value) in additional_headers {
            builder = builder.header(*key, *value);
        }

        if let Some(credentials) = credentials {
            builder =
                builder.basic_auth(credentials.login(), credentials.password);
        }

        let ws = builder
            .upgrade()
            .send()
            .await?
            .into_websocket()
            .instrument(span.clone())
            .await?;

        let unidentified_push_service = PushService {
            servers: self.servers,
            cfg: self.cfg.clone(),
            credentials: None,
            client: self.client.clone(),
        };
        let (ws, task) = SignalWebSocket::new(
            ws,
            keepalive_path.to_owned(),
            unidentified_push_service,
        );
        let task = task.instrument(span);
        tokio::task::spawn(task);
        Ok(ws)
    }

    pub(crate) async fn get_group(
        &mut self,
        credentials: HttpAuth,
    ) -> Result<crate::proto::Group, ServiceError> {
        self.request(
            Method::GET,
            Endpoint::storage("/v1/groups/"),
            HttpAuthOverride::Identified(credentials),
        )?
        .send()
        .await?
        .service_error_for_status()
        .await?
        .protobuf()
        .await
    }
}

/// moho's patch: the requests that change a group, make one, and read a group's link, which upstream does not have yet.
/// Each is signed in with the group's zero-knowledge authorization (`GroupsManager::get_authorization_for_today`).
impl PushService {
    /// Sends a change to a group (`PATCH /v2/groups/`), and answers the change as the server signed it: that is what
    /// the members are sent. `invite_link_password` is for joining through a link.
    pub async fn patch_group(
        &mut self,
        credentials: HttpAuth,
        actions: crate::proto::group_change::Actions,
        invite_link_password: Option<&[u8]>,
    ) -> Result<crate::proto::GroupChange, ServiceError> {
        use base64::Engine;
        use protobuf::ProtobufRequestBuilderExt;
        let path = match invite_link_password {
            Some(password) => format!(
                "/v2/groups/?inviteLinkPassword={}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(password)
            ),
            None => "/v2/groups/".to_string(),
        };
        let response: crate::proto::GroupChangeResponse = self
            .request(
                Method::PATCH,
                Endpoint::storage(path),
                HttpAuthOverride::Identified(credentials),
            )?
            .protobuf(actions).map_err(|_| ServiceError::InvalidFrame { reason: "a group request could not be encoded" })?
            .send()
            .await?
            .service_error_for_status()
            .await?
            .protobuf()
            .await?;
        response.group_change.ok_or(ServiceError::InvalidFrame {
            reason: "the group server answered a change with no change",
        })
    }

    /// Makes a group (`PUT /v2/groups/`).
    pub async fn create_group(
        &mut self,
        credentials: HttpAuth,
        group: crate::proto::Group,
    ) -> Result<(), ServiceError> {
        use protobuf::ProtobufRequestBuilderExt;
        self.request(
            Method::PUT,
            Endpoint::storage("/v2/groups/"),
            HttpAuthOverride::Identified(credentials),
        )?
        .protobuf(group).map_err(|_| ServiceError::InvalidFrame { reason: "a group request could not be encoded" })?
        .send()
        .await?
        .service_error_for_status()
        .await?;
        Ok(())
    }

    /// What a group link shows before joining (`GET /v1/groups/join/<password>`): the group's title, how many are in it,
    /// whether an admin must approve.
    pub async fn group_join_info(
        &mut self,
        credentials: HttpAuth,
        invite_link_password: &[u8],
    ) -> Result<crate::proto::GroupJoinInfo, ServiceError> {
        use base64::Engine;
        let path = format!(
            "/v1/groups/join/{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(invite_link_password)
        );
        self.request(
            Method::GET,
            Endpoint::storage(path),
            HttpAuthOverride::Identified(credentials),
        )?
        .send()
        .await?
        .service_error_for_status()
        .await?
        .protobuf()
        .await
    }
}

pub(crate) mod protobuf {
    use async_trait::async_trait;
    use prost::{EncodeError, Message};
    use reqwest::{header, RequestBuilder, Response};

    use super::ServiceError;

    pub(crate) trait ProtobufRequestBuilderExt
    where
        Self: Sized,
    {
        /// Set the request payload encoded as protobuf.
        /// Sets the `Content-Type` header to `application/x-protobuf`
        fn protobuf<T: Message + Default>(
            self,
            value: T,
        ) -> Result<Self, EncodeError>;
    }

    #[async_trait::async_trait]
    pub(crate) trait ProtobufResponseExt {
        /// Get the response body decoded from Protobuf
        async fn protobuf<T>(self) -> Result<T, ServiceError>
        where
            T: prost::Message + Default;
    }

    impl ProtobufRequestBuilderExt for RequestBuilder {
        fn protobuf<T: Message + Default>(
            self,
            value: T,
        ) -> Result<Self, EncodeError> {
            let mut buf = Vec::new();
            value.encode(&mut buf)?;
            let this =
                self.header(header::CONTENT_TYPE, "application/x-protobuf");
            Ok(this.body(buf))
        }
    }

    #[async_trait]
    impl ProtobufResponseExt for Response {
        async fn protobuf<T>(self) -> Result<T, ServiceError>
        where
            T: Message + Default,
        {
            let body = self.bytes().await?;
            let decoded = T::decode(body)?;
            Ok(decoded)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::configuration::SignalServers;
    use bytes::{Buf, Bytes};

    #[test]
    fn create_clients() {
        let environments = &[SignalServers::Staging, SignalServers::Production];

        for env in environments {
            let _ =
                super::PushService::new(*env, None, "libsignal-service test");
        }
    }

    #[test]
    fn serde_json_from_empty_reader() {
        // This fails, so we have handle empty response body separately in HyperPushService::json()
        let bytes: Bytes = "".into();
        assert!(
            serde_json::from_reader::<bytes::buf::Reader<Bytes>, String>(
                bytes.reader()
            )
            .is_err()
        );
    }

    #[test]
    fn serde_json_form_empty_vec() {
        // If we're trying to send and empty payload, serde_json must be able to make a Vec out of it
        assert!(serde_json::to_vec(b"").is_ok());
    }
}
