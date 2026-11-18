use presage::libsignal_service::{
    prelude::phonenumber,
    protocol::{InvalidDeviceId, SignalProtocolError},
};
use presage::store::StoreError;
use tracing::error;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SignalStoreError {
    #[error("the Signal store: {0}")]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    PhoneNumber(#[from] phonenumber::ParseError),
    #[error("a record in the Signal store is not in the form it should be")]
    InvalidFormat,
    #[error("this Signal store was written by a newer moho (schema {found}, this one reads up to {known})")]
    NewerSchema { found: i64, known: i64 },
    #[error(transparent)]
    Protocol(#[from] SignalProtocolError),
    #[error("invalid device ID: {0}")]
    InvalidDeviceId(#[from] InvalidDeviceId),
    #[error(transparent)]
    Setup(#[from] anyhow::Error),
}

impl StoreError for SignalStoreError {}

/// What the protocol layer is told when the store fails. The library has no variant for "the disk said no", so it is
/// reported as an invalid state; the detail goes to the log, never the text of a message or a key.
impl From<SignalStoreError> for SignalProtocolError {
    fn from(error: SignalStoreError) -> Self {
        match error {
            // A protocol error that only passed through the store (an id that is not there, a replayed handshake) keeps
            // its meaning: the protocol acts on which one it is.
            SignalStoreError::Protocol(inner) => inner,
            other => {
                error!(error = %other, "signal store error");
                Self::InvalidState("signal store", other.to_string())
            }
        }
    }
}
