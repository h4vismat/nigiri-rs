use std::{borrow::Cow, error::Error, io, path::PathBuf, time::Duration};

use bitcoin::hashes::sha256;

/// Maximum retained length for daemon-provided diagnostic text.
pub(crate) const MAX_ERROR_TEXT_BYTES: usize = 1_024;

/// Errors returned by the LND Lightning boundary.
#[derive(thiserror::Error)]
#[non_exhaustive]
pub enum LndError {
    /// Caller input or immutable client configuration was rejected before an RPC was sent.
    #[error("invalid request: {detail}")]
    InvalidRequest { detail: Cow<'static, str> },
    /// Reading a configured credential file failed.
    #[error("failed to read LND credential file {path}")]
    CredentialRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The gRPC transport could not be established or maintained.
    #[error("transport failed during {operation}: {detail}")]
    Transport {
        operation: Cow<'static, str>,
        detail: Cow<'static, str>,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// LND rejected the configured macaroon.
    #[error("authentication failed during {operation}: {detail}")]
    Authentication {
        operation: Cow<'static, str>,
        detail: Cow<'static, str>,
    },
    /// LND returned a non-authentication gRPC status.
    #[error("LND status failed during {operation}: {detail}")]
    Status {
        operation: Cow<'static, str>,
        detail: Cow<'static, str>,
    },
    /// An operation did not complete before its configured deadline.
    #[error("{operation} timed out after {duration:?}")]
    Timeout {
        operation: Cow<'static, str>,
        duration: Duration,
    },
    /// LND supplied a response that cannot be converted into the public domain model.
    #[error("invalid response during {operation}: {detail}")]
    InvalidResponse {
        operation: Cow<'static, str>,
        detail: Cow<'static, str>,
        identifier: Option<String>,
    },
    /// LND reached a terminal failed payment state.
    #[error("payment {payment_hash} failed: {reason}")]
    PaymentFailed {
        payment_hash: sha256::Hash,
        reason: Cow<'static, str>,
    },
    /// LND may have committed an operation, but its final outcome was not observed.
    #[error("outcome unknown for {operation}")]
    OutcomeUnknown {
        operation: Cow<'static, str>,
        identifier: Option<String>,
    },
}

impl std::fmt::Debug for LndError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LndError")
            .field("display", &self.to_string())
            .finish()
    }
}

#[allow(dead_code)]
pub(crate) fn bounded(value: impl Into<Cow<'static, str>>) -> Cow<'static, str> {
    let value = value.into();
    if value.len() <= MAX_ERROR_TEXT_BYTES {
        return value;
    }

    let mut end = MAX_ERROR_TEXT_BYTES - '…'.len_utf8();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}…", &value[..end]))
}
