use reqwest::RequestBuilder;

use crate::{NigiriError, NigiriNetwork};

pub(crate) async fn send_bounded<N: NigiriNetwork>(
    client: &crate::NigiriClient<N>,
    operation: &'static str,
    request: RequestBuilder,
    sensitive: &[&str],
) -> Result<Vec<u8>, NigiriError> {
    let response = request
        .send()
        .await
        .map_err(|source| NigiriError::HttpTransport {
            operation: operation.into(),
            source: source.without_url(),
        })?;
    read_bounded(
        operation,
        response,
        sensitive,
        client.config.max_response_bytes,
    )
    .await
}

async fn read_bounded(
    operation: &'static str,
    mut response: reqwest::Response,
    sensitive: &[&str],
    limit: usize,
) -> Result<Vec<u8>, NigiriError> {
    let status = response.status();
    let mut body = Vec::new();
    let mut exceeded = false;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|source| NigiriError::HttpTransport {
            operation: operation.into(),
            source: source.without_url(),
        })?
    {
        let remaining = limit.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            exceeded = true;
        }
    }

    if !status.is_success() {
        return Err(NigiriError::HttpStatus {
            operation: operation.into(),
            status,
            body: bounded_error_text(&body, exceeded, sensitive),
        });
    }
    if exceeded {
        return Err(NigiriError::InvalidResponse {
            operation: operation.into(),
            detail: "response body exceeded the configured safety limit".to_owned(),
        });
    }
    Ok(body)
}

fn bounded_error_text(body: &[u8], exceeded: bool, sensitive: &[&str]) -> String {
    let mut text = String::from_utf8_lossy(body).into_owned();
    for value in sensitive.iter().filter(|value| !value.is_empty()) {
        text = text.replace(value, "[redacted]");
    }
    if exceeded {
        text.push_str("…[truncated]");
    }
    text
}

pub(crate) fn parse_txid<N: NigiriNetwork>(
    operation: &'static str,
    body: &[u8],
) -> Result<N::Txid, NigiriError> {
    let text = std::str::from_utf8(body).map_err(|_| NigiriError::InvalidResponse {
        operation: operation.into(),
        detail: "expected a UTF-8 transaction identifier".to_owned(),
    })?;
    N::parse_txid(operation, text)
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
