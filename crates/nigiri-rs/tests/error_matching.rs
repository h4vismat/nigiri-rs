//! Matching `NigiriError` from outside the crate that defines it.
//!
//! This is documentation of an intended shape rather than a regression gate: `#[non_exhaustive]`
//! only constrains code in another crate, and its removal would make this file compile *more*
//! freely, not less. It is here so the requirement is written down where a reader of the public
//! surface will find it, and it is in the facade crate because that is the nearest thing to an
//! external consumer the workspace has.

use nigiri_rs::NigiriError;

// Catches nothing by itself; states that a downstream match must carry a wildcard arm, and that the
// variants a consumer is expected to branch on are reachable by name from the facade.
#[test]
fn a_downstream_match_needs_a_wildcard_arm() {
    let error = NigiriError::InvalidRequest {
        detail: "startup timeout must be greater than zero".into(),
    };

    let described = match &error {
        NigiriError::InvalidRequest { detail } => format!("caller error: {detail}"),
        NigiriError::InvalidResponse { operation, .. } => format!("bad response from {operation}"),
        NigiriError::RpcFailed { method, code, .. } => format!("{method} failed with {code}"),
        // Required by `#[non_exhaustive]`, and the reason it is there: peg, Lightning and Ark each
        // add variants, and a consumer must not have to be recompiled against each one.
        _ => "other".to_owned(),
    };

    assert_eq!(
        described,
        "caller error: startup timeout must be greater than zero"
    );
}
