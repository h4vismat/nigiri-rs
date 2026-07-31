use std::{borrow::Cow, io, time::Duration};

use reqwest::StatusCode;

/// Closed error model for Nigiri HTTP and CLI operations.
///
/// Operation and method labels are [`Cow<'static, str>`] so that a
/// runtime-determined RPC method name passed to [`crate::NigiriClient::rpc`] is
/// reported accurately. Crate-owned labels stay borrowed and allocate nothing.
#[derive(Debug, thiserror::Error)]
pub enum NigiriError {
    #[error("HTTP transport failed during {operation}")]
    HttpTransport {
        operation: Cow<'static, str>,
        #[source]
        source: reqwest::Error,
    },
    #[error("HTTP status {status} during {operation}: {body}")]
    HttpStatus {
        operation: Cow<'static, str>,
        status: StatusCode,
        body: String,
    },
    #[error("failed to execute Nigiri during {operation}")]
    ProcessSpawn {
        operation: Cow<'static, str>,
        #[source]
        source: io::Error,
    },
    #[error("Nigiri RPC {method} failed with exit code {exit_code:?}: {stderr}")]
    RpcFailed {
        method: Cow<'static, str>,
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error("{operation} timed out after {duration:?}")]
    Timeout {
        operation: Cow<'static, str>,
        duration: Duration,
    },
    /// Caller input was rejected before any Nigiri process was spawned.
    ///
    /// Distinct from [`NigiriError::InvalidResponse`], which means Nigiri ran and
    /// returned something unusable. Covers configuration validation and RPC method
    /// name validation.
    #[error("invalid request: {detail}")]
    InvalidRequest { detail: Cow<'static, str> },
    #[error("invalid response during {operation}: {detail}")]
    InvalidResponse {
        operation: Cow<'static, str>,
        detail: String,
    },
}
