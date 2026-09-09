use reqwest::RequestBuilder;

use crate::{NigiriError, NigiriNetwork};

pub(crate) async fn send_bounded<N: NigiriNetwork>(
    client: &crate::NigiriClient<N>,
    operation: &'static str,
    request: RequestBuilder,
    sensitive: &[&str],
) -> Result<Vec<u8>, NigiriError> {
    let response = send(client, operation, request).await?;
    if !response.status.is_success() {
        return Err(response.status_error(operation, sensitive));
    }
    response.into_body(operation)
}

pub(crate) struct BoundedResponse {
    pub(crate) status: reqwest::StatusCode,
    pub(crate) body: Vec<u8>,
    exceeded: bool,
}

impl BoundedResponse {
    pub(crate) fn status_error(&self, operation: &str, sensitive: &[&str]) -> NigiriError {
        NigiriError::HttpStatus {
            operation: operation.to_owned().into(),
            status: self.status,
            body: bounded_error_text(&self.body, self.exceeded, sensitive),
        }
    }

    pub(crate) fn into_body(self, operation: &str) -> Result<Vec<u8>, NigiriError> {
        if self.exceeded {
            return Err(NigiriError::InvalidResponse {
                operation: operation.to_owned().into(),
                detail: "response body exceeded the configured safety limit".to_owned(),
            });
        }
        Ok(self.body)
    }
}

pub(crate) async fn send<N: NigiriNetwork>(
    client: &crate::NigiriClient<N>,
    operation: &str,
    request: RequestBuilder,
) -> Result<BoundedResponse, NigiriError> {
    let transport_error = |source: reqwest::Error| {
        if source.is_timeout() {
            NigiriError::Timeout {
                operation: operation.to_owned().into(),
                duration: client.config.timeout,
            }
        } else {
            NigiriError::HttpTransport {
                operation: operation.to_owned().into(),
                source: source.without_url(),
            }
        }
    };
    let mut response = request.send().await.map_err(transport_error)?;
    let status = response.status();
    let mut body = Vec::new();
    let mut exceeded = false;
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        let remaining = client.config.max_response_bytes.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            exceeded = true;
            break;
        }
    }
    Ok(BoundedResponse {
        status,
        body,
        exceeded,
    })
}

fn bounded_error_text(body: &[u8], exceeded: bool, sensitive: &[&str]) -> String {
    let mut text = redact_sensitive(String::from_utf8_lossy(body).into_owned(), sensitive);
    if exceeded {
        text.push_str("…[truncated]");
    }
    text
}

/// How many leading bytes of a sensitive value are enough to recognise an echo of it.
///
/// A node is free to quote only part of an argument it rejected — Bitcoin Core reports the offending
/// portion of a bad address rather than the whole string — so matching the value in full would leave
/// the fragment it did echo in the clear.
const SENSITIVE_ANCHOR_BYTES: usize = 12;

/// Removes caller-supplied sensitive values, including fragments of them.
///
/// Matching is anchored on each value's leading bytes and then extended over as much of the value as
/// actually follows, so a partial echo is redacted along with a whole one. Matching stays
/// case-sensitive: transaction hex and addresses are echoed verbatim, and folding case would risk
/// redacting unrelated text.
pub(crate) fn redact_sensitive(text: String, sensitive: &[&str]) -> String {
    sensitive
        .iter()
        .filter(|value| !value.is_empty())
        .fold(text, |text, value| redact_value(&text, value))
}

fn redact_value(text: &str, value: &str) -> String {
    let anchor = anchor_of(value);
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(offset) = rest.find(anchor) {
        redacted.push_str(&rest[..offset]);
        let echoed = &rest[offset..];
        // The echo is however much of the value is actually present, never less than the anchor.
        let matched = value
            .chars()
            .zip(echoed.chars())
            .take_while(|(expected, actual)| expected == actual)
            .map(|(character, _)| character.len_utf8())
            .sum::<usize>();

        redacted.push_str("[redacted]");
        rest = &echoed[matched..];
    }

    redacted.push_str(rest);
    redacted
}

/// The leading bytes matched when looking for an echo, on a character boundary.
fn anchor_of(value: &str) -> &str {
    if value.len() <= SENSITIVE_ANCHOR_BYTES {
        return value;
    }

    let mut end = SENSITIVE_ANCHOR_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub(crate) fn endpoint(
    base: &url::Url,
    operation: &'static str,
    segments: &[&str],
) -> Result<url::Url, NigiriError> {
    let mut url = base.clone();
    let mut path = url
        .path_segments_mut()
        .map_err(|_| NigiriError::InvalidResponse {
            operation: operation.into(),
            detail: "configured endpoint cannot accept path segments".to_owned(),
        })?;
    path.pop_if_empty();
    path.extend(segments);
    drop(path);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::redact_sensitive;

    #[test]
    fn unicode_partial_echo_stops_on_a_character_boundary() {
        let value = "秘密の認証文字列";
        assert_eq!(
            redact_sensitive("秘密の認証文外 and 秘密の認証文字列".into(), &[value]),
            "[redacted]外 and [redacted]"
        );
    }

    #[test]
    fn long_repeated_sensitive_values_are_redacted() {
        let value = "a".repeat(100_000);
        assert_eq!(
            redact_sensitive(format!("{value} {value}"), &[&value]),
            "[redacted] [redacted]"
        );
    }

    const TXID: &str = "9f2b7c1d4e6a8b0c2d4f6a8b0c2d4e6f8a0b2c4d6e8f0a1b3c5d7e9f1a3b5c7d";

    // Catches a regression back to whole-value matching. A node that quotes only part of a rejected
    // argument would then leave that fragment in the clear, which is worse than not redacting at all
    // because the error reads as though it had been sanitized.
    #[test]
    fn a_partial_echo_of_a_sensitive_value_is_redacted() {
        let echoed = &TXID[..20];

        let redacted = redact_sensitive(format!("transaction {echoed} rejected"), &[TXID]);

        assert_eq!(redacted, "transaction [redacted] rejected");
    }

    // Catches a regression that stops redacting the whole value once anchored matching exists.
    #[test]
    fn a_whole_sensitive_value_is_still_redacted() {
        let redacted = redact_sensitive(format!("transaction {TXID} rejected"), &[TXID]);

        assert_eq!(redacted, "transaction [redacted] rejected");
        assert!(!redacted.contains(&TXID[..12]));
    }

    // Catches a regression that leaves a later echo behind after redacting the first.
    #[test]
    fn every_echo_of_a_value_is_redacted() {
        let redacted = redact_sensitive(
            format!("{TXID} conflicts with {} in the mempool", &TXID[..24]),
            &[TXID],
        );

        assert_eq!(
            redacted,
            "[redacted] conflicts with [redacted] in the mempool"
        );
    }

    // Catches a regression that redacts values shorter than the anchor by prefix, which for an amount
    // like `1.0` would swallow unrelated numbers.
    #[test]
    fn a_value_shorter_than_the_anchor_matches_only_in_full() {
        let redacted = redact_sensitive(
            "amount 1.00000000 exceeds balance 1.5".to_owned(),
            &["1.00000000"],
        );

        assert_eq!(redacted, "amount [redacted] exceeds balance 1.5");
    }

    // Catches a regression that redacts text merely resembling a sensitive value, which would hide the
    // node's actual complaint.
    #[test]
    fn unrelated_text_is_left_alone() {
        let redacted = redact_sensitive(
            "insufficient funds for the requested send".to_owned(),
            &[TXID],
        );

        assert_eq!(redacted, "insufficient funds for the requested send");
    }

    // Catches a regression that drops later values once an earlier one matched.
    #[test]
    fn each_sensitive_value_is_redacted_independently() {
        let redacted = redact_sensitive(
            "wallet rejected address bcrt1qexampleaddress0000 and amount 0.001".to_owned(),
            &["bcrt1qexampleaddress0000", "0.001"],
        );

        assert_eq!(
            redacted,
            "wallet rejected address [redacted] and amount [redacted]"
        );
    }
}
