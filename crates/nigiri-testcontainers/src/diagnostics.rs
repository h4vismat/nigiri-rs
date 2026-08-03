//! Bounded, credential-free rendering of everything this crate reports.
//!
//! Fixture text comes from Docker, Bitcoin Core, and Electrs, so every message that can reach a
//! caller passes through here: redaction happens before truncation so no boundary can expose a
//! partial credential, and every result is byte-bounded on a UTF-8 boundary.

use std::fmt;

use crate::{RPC_PASSWORD, RPC_USER};

pub(crate) const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
pub(crate) const MAX_SOURCE_BYTES: usize = 4 * 1024;
/// Slack a caller must keep in front of a truncated buffer so a credential straddling the cut is
/// still whole when [`redact`] runs.
pub(crate) const MAX_REDACTION_CONTEXT_BYTES: usize = 32;
const SOURCE_TRUNCATION_MARKER: &str = "[TRUNCATED]";
const REDACTED: &str = "[REDACTED]";
/// `base64("admin1:123")`, the form the credentials take in an HTTP basic-auth header. Asserted
/// against [`RPC_USER`]/[`RPC_PASSWORD`] by `basic_auth_pattern_matches_the_fixture_credentials`.
const BASIC_AUTH_PATTERN: &str = "YWRtaW4xOjEyMw==";
/// Deliberately as long as [`BASIC_AUTH_PATTERN`]: a replacement shorter than its pattern would
/// shrink a truncated buffer back under its bound, and the boundary slack would stop being consumed.
const REDACTED_BASIC_AUTH: &str = "[REDACTED::AUTH]";

const _: () = assert!(
    MAX_SOURCE_BYTES > SOURCE_TRUNCATION_MARKER.len(),
    "a bounded source must leave room for its truncation marker",
);

/// Replaces every spelling of the fixture credentials this crate can observe.
///
/// Patterns are derived from the credential constants rather than repeated as literals, so changing
/// the credentials cannot silently disable redaction. The bare password is deliberately not a
/// pattern: `123` occurs in ordinary log text, so the password is redacted only in credential
/// context. No replacement is shorter than its pattern, which is what lets a caller rely on the
/// boundary slack still being present after redaction.
pub(crate) fn redact(value: &str) -> String {
    let mut redacted = value.to_owned();

    for (pattern, replacement) in redaction_patterns() {
        redacted = redacted.replace(&pattern, &replacement);
    }

    redacted
}

/// The pattern/replacement pairs applied by [`redact`], longest-matching spelling first.
fn redaction_patterns() -> [(String, String); 5] {
    [
        (format!("{RPC_USER}:{RPC_PASSWORD}"), REDACTED.to_owned()),
        (
            BASIC_AUTH_PATTERN.to_owned(),
            REDACTED_BASIC_AUTH.to_owned(),
        ),
        (
            format!("rpcpassword={RPC_PASSWORD}"),
            format!("rpcpassword={REDACTED}"),
        ),
        (
            format!("rpcpassword {RPC_PASSWORD}"),
            format!("rpcpassword {REDACTED}"),
        ),
        (RPC_USER.to_owned(), REDACTED.to_owned()),
    ]
}

/// Redacts, then keeps the terminal bytes: a container log's meaning is its final output.
pub(crate) fn redacted_tail(value: &str) -> String {
    utf8_tail(
        &redact(&utf8_tail(
            value,
            MAX_DIAGNOSTIC_BYTES + MAX_REDACTION_CONTEXT_BYTES,
        )),
        MAX_DIAGNOSTIC_BYTES,
    )
}

/// Redacts, then keeps the leading bytes: an error chain's meaning is its classification.
pub(crate) fn redacted_head(value: &str, maximum_bytes: usize) -> String {
    utf8_head(
        &redact(&utf8_head(
            value,
            maximum_bytes + MAX_REDACTION_CONTEXT_BYTES,
        )),
        maximum_bytes,
    )
}

pub(crate) fn join_diagnostics(existing: &str, addition: &str) -> String {
    if existing.is_empty() {
        return redacted_tail(addition);
    }

    redacted_tail(&format!("{existing}; {addition}"))
}

pub(crate) fn utf8_tail(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }

    let mut start = value.len() - maximum_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_owned()
}

fn utf8_head(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }

    // A bound too small to hold the marker keeps text instead of announcing the cut.
    let (reserved, marker) = if maximum_bytes > SOURCE_TRUNCATION_MARKER.len() {
        (SOURCE_TRUNCATION_MARKER.len(), SOURCE_TRUNCATION_MARKER)
    } else {
        (0, "")
    };

    let mut end = maximum_bytes - reserved;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &value[..end])
}

/// A boxed error source that any error-chain formatter can render without exposing what bounded
/// diagnostics deliberately withhold.
///
/// The cause chain is flattened into this single terminal source, so `Display` and `Debug` keep the
/// original chain's meaning while `source` cannot walk back into raw, credential-bearing errors.
pub(crate) struct RedactedSource {
    rendered: String,
}

impl fmt::Display for RedactedSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

// `Debug` mirrors `Display` because a derived struct rendering escapes the message and grows past
// the byte bound this wrapper exists to hold, and `{:?}` is how most error chains are printed.
impl fmt::Debug for RedactedSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

impl std::error::Error for RedactedSource {}

pub(crate) fn redacted_source(
    error: impl std::error::Error + Send + Sync + 'static,
) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(RedactedSource {
        rendered: redacted_head(&flattened_chain(&error), MAX_SOURCE_BYTES),
    })
}

/// Renders an error and every cause below it, bounding each link so a single oversized cause cannot
/// force an unbounded intermediate allocation.
fn flattened_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let link_bound = MAX_SOURCE_BYTES + MAX_REDACTION_CONTEXT_BYTES;
    let mut rendered = utf8_head(&error.to_string(), link_bound);
    let mut cause = error.source();

    while let Some(source) = cause {
        if rendered.len() >= link_bound {
            break;
        }
        rendered.push_str(": ");
        rendered.push_str(&utf8_head(&source.to_string(), link_bound));
        cause = source.source();
    }

    rendered
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fmt, io};

    use super::{
        BASIC_AUTH_PATTERN, MAX_DIAGNOSTIC_BYTES, MAX_REDACTION_CONTEXT_BYTES, MAX_SOURCE_BYTES,
        join_diagnostics, redact, redacted_head, redacted_source, redacted_tail,
        redaction_patterns, utf8_head, utf8_tail,
    };
    use crate::{RPC_PASSWORD, RPC_USER};

    fn base64(value: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

        let bytes = value.as_bytes();
        let mut encoded = String::new();

        for chunk in bytes.chunks(3) {
            let mut buffer = [0_u8; 3];
            buffer[..chunk.len()].copy_from_slice(chunk);
            let packed =
                u32::from(buffer[0]) << 16 | u32::from(buffer[1]) << 8 | u32::from(buffer[2]);

            for offset in 0..4 {
                if offset <= chunk.len() {
                    let index = (packed >> (18 - offset * 6)) & 0b11_1111;
                    encoded.push(char::from(ALPHABET[index as usize]));
                } else {
                    encoded.push('=');
                }
            }
        }

        encoded
    }

    // Catches a regression that leaves the basic-auth redaction pattern behind after the fixture
    // credentials change, silently letting an `Authorization` header through.
    #[test]
    fn basic_auth_pattern_matches_the_fixture_credentials() {
        assert_eq!(
            base64(&format!("{RPC_USER}:{RPC_PASSWORD}")),
            BASIC_AUTH_PATTERN
        );
    }

    // Catches a regression that adds a redaction pattern longer than the slack callers keep in front
    // of a truncated buffer, or a replacement shorter than its pattern. Either one lets a partial
    // credential survive a boundary: an over-long pattern is cut before it can match, and a shrinking
    // replacement pulls the buffer back under its bound so the slack is never dropped.
    #[test]
    fn redaction_patterns_fit_the_boundary_slack_and_never_shrink_their_input() {
        for (pattern, replacement) in redaction_patterns() {
            assert!(
                pattern.len() <= MAX_REDACTION_CONTEXT_BYTES,
                "the {}-byte pattern {pattern} exceeds {MAX_REDACTION_CONTEXT_BYTES} bytes of slack",
                pattern.len()
            );
            assert!(
                replacement.len() >= pattern.len(),
                "replacing {pattern} with {replacement} shrinks the retained buffer"
            );
        }
    }

    // Catches a regression that lets a shrinking redaction leave a partial credential at the front of
    // a retained tail: enough basic-auth blobs in one window would pull the redacted text back under
    // the bound, so the second cut would stop dropping the boundary slack.
    #[test]
    fn a_shrinking_window_still_drops_its_leading_partial_credential() {
        let split_credential = "n1:123";
        let blobs = format!(" {BASIC_AUTH_PATTERN}").repeat(64);
        let filler = "f".repeat(
            MAX_DIAGNOSTIC_BYTES + MAX_REDACTION_CONTEXT_BYTES
                - split_credential.len()
                - blobs.len(),
        );
        let window = format!("{split_credential}{blobs}{filler}");

        let rendered = redacted_tail(&window);

        assert!(!rendered.starts_with(split_credential), "{rendered:.32}");
        assert!(!rendered.contains(":123"));
    }

    // Catches a regression that underflows the head bound when it cannot hold the truncation marker.
    #[test]
    fn head_bounds_smaller_than_the_truncation_marker_stay_within_bound() {
        for maximum_bytes in 0..16 {
            let rendered = utf8_head("早early-log-text", maximum_bytes);
            assert!(
                rendered.len() <= maximum_bytes,
                "{maximum_bytes} bytes produced {rendered:?}"
            );
        }
    }

    // Catches a regression that only redacts the exact command-line spellings, leaving the
    // credentials readable in daemon log text, a cookie argument, or a basic-auth header.
    #[test]
    fn redaction_covers_every_observed_credential_spelling() {
        for spelling in [
            "-rpcuser=admin1",
            "rpcuser=admin1",
            "-rpcpassword=123",
            "rpcpassword=123",
            "rpcpassword 123",
            "admin1:123",
            "--cookie admin1:123",
            "Authorization: Basic YWRtaW4xOjEyMw==",
            "Command-line arg: rpcuser=admin1",
        ] {
            let redacted = redact(spelling);
            assert!(!redacted.contains("admin1"), "{spelling} -> {redacted}");
            assert!(
                !redacted.contains("=123") && !redacted.contains(" 123"),
                "{spelling} -> {redacted}"
            );
        }
    }

    // Catches a regression that truncates before redacting, which would leave a partial credential
    // visible at the retained boundary.
    #[test]
    fn bounded_rendering_redacts_before_it_truncates() {
        let leading = format!("admin1:123 {}", "log-".repeat(8 * 1024));
        let trailing = format!("{} admin1:123", "log-".repeat(8 * 1024));

        for rendered in [
            redacted_tail(&leading),
            redacted_tail(&trailing),
            redacted_head(&leading, MAX_SOURCE_BYTES),
            redacted_head(&trailing, MAX_SOURCE_BYTES),
        ] {
            assert!(!rendered.contains("admin1"));
            assert!(!rendered.contains(":123"));
        }
    }

    // Catches a regression that keeps a diagnostic's leading text instead of the terminal failure,
    // or that loses the retained text's first whole character.
    #[test]
    fn diagnostic_tails_are_bounded_to_a_whole_character_boundary() {
        let value = format!("{}terminal-marker", "早".repeat(16 * 1024));

        let rendered = redacted_tail(&value);

        assert!(rendered.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(rendered.ends_with("terminal-marker"));
        assert!(
            rendered.starts_with('早'),
            "the retained tail must begin with a whole character"
        );
        assert_eq!(utf8_tail(&rendered, MAX_DIAGNOSTIC_BYTES), rendered);
    }

    // Catches a regression that keeps an oversized cause's tail and loses the leading classification
    // an error chain exists to report.
    #[test]
    fn bounded_heads_keep_their_leading_classification_and_mark_the_cut() {
        let value = format!("leading classification {}", "早".repeat(8 * 1024));

        let rendered = redacted_head(&value, MAX_SOURCE_BYTES);

        assert!(rendered.starts_with("leading classification 早"));
        assert!(rendered.ends_with("[TRUNCATED]"));
        assert!(rendered.len() <= MAX_SOURCE_BYTES);
    }

    // Catches a regression that drops nested causes from a redacted source, or that lets `source`
    // walk back into the raw, credential-bearing chain.
    #[test]
    fn redacted_sources_flatten_their_causes_into_one_terminal_source() {
        #[derive(Debug)]
        struct Nested {
            cause: io::Error,
        }

        impl fmt::Display for Nested {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("outer classification")
            }
        }

        impl Error for Nested {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.cause)
            }
        }

        let source = redacted_source(Nested {
            cause: io::Error::other(format!("nested cause admin1:123 {}", "x".repeat(8 * 1024))),
        });

        let rendered = source.to_string();
        assert!(
            rendered.starts_with("outer classification: nested cause [REDACTED] x"),
            "{rendered:.64}"
        );
        assert!(rendered.len() <= MAX_SOURCE_BYTES);
        assert_eq!(format!("{source:?}"), rendered);
        assert!(rendered.ends_with("[TRUNCATED]"));
        assert!(source.source().is_none());
    }

    // Catches a regression that overwrites existing diagnostics when context is appended, or that
    // lets the joined result exceed the diagnostic bound.
    #[test]
    fn joined_diagnostics_keep_both_parts_and_stay_bounded() {
        assert_eq!(join_diagnostics("", "removed"), "removed");
        assert_eq!(
            join_diagnostics("start failed", "removed"),
            "start failed; removed"
        );

        let joined = join_diagnostics(&"log-".repeat(8 * 1024), "removed admin1:123");
        assert!(joined.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(joined.ends_with("removed [REDACTED]"));
    }
}
