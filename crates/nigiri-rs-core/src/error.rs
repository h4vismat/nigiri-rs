use std::{borrow::Cow, time::Duration};

use reqwest::StatusCode;

/// Error model for Nigiri HTTP and node JSON-RPC operations.
///
/// `#[non_exhaustive]`: variants are added as the crate grows — peg operations, Lightning channel
/// state, Ark — and a downstream match must carry a wildcard arm rather than being broken by each
/// addition.
///
/// Operation and method labels are [`Cow<'static, str>`] so that a
/// runtime-determined RPC method name passed to [`crate::NigiriClient::rpc`] is
/// reported accurately. Crate-owned labels stay borrowed and allocate nothing.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    /// The node returned a JSON-RPC error envelope.
    #[error("Nigiri RPC {method} failed with code {code}: {message}")]
    RpcFailed {
        /// Request method reported by the node.
        method: Cow<'static, str>,
        /// Numeric JSON-RPC error code returned by the node.
        code: i32,
        /// JSON-RPC error message returned by the node.
        message: String,
    },
    #[error("{operation} timed out after {duration:?}")]
    Timeout {
        operation: Cow<'static, str>,
        duration: Duration,
    },
    /// Caller input was rejected before any request was sent.
    ///
    /// Distinct from [`NigiriError::InvalidResponse`], which means a service
    /// returned something unusable. Covers configuration validation and RPC method
    /// name validation.
    #[error("invalid request: {detail}")]
    InvalidRequest { detail: Cow<'static, str> },
    #[error("invalid response during {operation}: {detail}")]
    InvalidResponse {
        operation: Cow<'static, str>,
        detail: String,
    },
    #[error("{operation} committed transaction {txid}, but confirmation mining failed")]
    PostTransactionMiningFailed {
        operation: Cow<'static, str>,
        txid: String,
        #[source]
        source: Box<NigiriError>,
    },
}

#[cfg(test)]
mod tests {
    use super::NigiriError;

    // Catches a regression that discards the committed transaction id or underlying mining error.
    #[test]
    fn post_transaction_error_displays_the_committed_txid() {
        let error = NigiriError::PostTransactionMiningFailed {
            operation: "faucet".into(),
            txid: "11".repeat(32),
            source: Box::new(NigiriError::InvalidResponse {
                operation: "mine confirmation".into(),
                detail: "expected a block hash list".to_owned(),
            }),
        };

        assert!(error.to_string().contains(&"11".repeat(32)));
        assert!(std::error::Error::source(&error).is_some());
    }
}
