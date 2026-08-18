//! Private LND protocol bindings and Lightning client APIs.

mod proto;

/// The pinned upstream LND protobuf release.
pub const LND_PROTO_VERSION: &str = "v0.21.1-beta";

/// The pinned upstream LND protobuf commit.
pub const LND_PROTO_COMMIT: &str = "2b87887";
