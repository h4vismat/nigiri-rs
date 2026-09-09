# Changelog

## 0.6.0 — 2026-09-08

This release publishes the following independently versioned crates:

| Crate | Version |
| --- | --- |
| nigiri-rs | 0.6.0 |
| nigiri-rs-core | 0.4.1 |
| nigiri-rs-lnd | 0.2.0 |
| nigiri-rs-fixtures | 0.3.0 |
| nigiri-rs-macros | 0.3.0 |

### Migration

- Enable `lightning-fixtures` for facade `LndPair`, or `lnd` with the direct fixtures crate.
- `LndError::Status` now contains a typed `code` field.
- Fixture parameters cannot be combined with `#[should_panic]`; assert expected failures inside the body.
- Repository Docker tests require `docker-tests`; ordinary workspace tests need no Docker.

### Fixes and improvements

- Validate JSON-RPC envelopes and IDs, distinguish missing results from null, and reject HTTP failures.
- Bound response reads and polling deadlines; tolerate transaction-indexing delays and redact sensitive values in linear time.
- Retain asset issuance identifiers when a mint transfer fails so callers can recover without issuing again.
- Validate peg-out asset identity and output ambiguity; retry only the supported peg-in maturity rejection. Simulated release remains stateless, with confirmation and replay prevention owned by callers.
- Preserve fixture ownership through cancellation and late create responses; coordinate dependent teardown and retain startup diagnostics. Bound log sampling and reject unsupported remote Docker endpoints.
- Correct LND HTTPS port 443 bootstrap, expose validated response constructors, and simplify startup orchestration.
- Support macro consumers without a direct Tokio dependency and renamed facade dependencies through `crate = "::regtest"`.
- Keep Lightning dependencies optional for Bitcoin/Liquid fixtures and check both default and all-feature builds in CI.
