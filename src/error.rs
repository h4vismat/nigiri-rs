use std::{io, time::Duration};

use reqwest::StatusCode;

/// Closed error model for Nigiri HTTP and CLI operations.
#[derive(Debug, thiserror::Error)]
pub enum NigiriError {
    #[error("HTTP transport failed during {operation}")]
    HttpTransport {
        operation: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("HTTP status {status} during {operation}: {body}")]
    HttpStatus {
        operation: &'static str,
        status: StatusCode,
        body: String,
    },
    #[error("failed to execute Nigiri during {operation}")]
    ProcessSpawn {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("Nigiri RPC {method} failed with exit code {exit_code:?}: {stderr}")]
    RpcFailed {
        method: &'static str,
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error("{operation} timed out after {duration:?}")]
    Timeout {
        operation: &'static str,
        duration: Duration,
    },
    #[error("invalid response during {operation}: {detail}")]
    InvalidResponse {
        operation: &'static str,
        detail: String,
    },
}
