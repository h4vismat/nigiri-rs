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
    /// A Liquid transaction expected to carry a peg-out carried none.
    #[error("no peg-out output in Liquid transaction {liquid_txid}")]
    PegOutputNotFound { liquid_txid: String },
    /// A peg-out output was present but could not be decoded.
    #[error("malformed peg-out output in Liquid transaction {liquid_txid}: {detail}")]
    PegOutputMalformed { liquid_txid: String, detail: String },
    /// A peg-in deposit has not reached the sidechain's required confirmation depth.
    #[error("peg-in deposit has {have} confirmations, needs {need}")]
    PegInImmature { have: u64, need: u64 },
    /// The Bitcoin and Liquid nodes are not a usable peg pair.
    #[error("peg is not configured: {detail}")]
    PegNotConfigured { detail: Cow<'static, str> },
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

    // Catches a regression that drops the transaction id from a peg-out decode failure, which
    // would leave a caller unable to say which transaction was rejected.
    #[test]
    fn peg_errors_name_what_failed() {
        let not_found = NigiriError::PegOutputNotFound {
            liquid_txid: "aa".repeat(32),
        };
        assert!(not_found.to_string().contains(&"aa".repeat(32)));

        let malformed = NigiriError::PegOutputMalformed {
            liquid_txid: "bb".repeat(32),
            detail: "expected a 32-byte parent genesis hash push".to_owned(),
        };
        assert!(malformed.to_string().contains(&"bb".repeat(32)));
        assert!(malformed.to_string().contains("32-byte parent genesis"));

        let immature = NigiriError::PegInImmature { have: 3, need: 8 };
        assert!(immature.to_string().contains('3'));
        assert!(immature.to_string().contains('8'));

        let unconfigured = NigiriError::PegNotConfigured {
            detail: "peg-out wallet has not been initialized".into(),
        };
        assert!(unconfigured.to_string().contains("not been initialized"));
    }
}
