#![cfg_attr(not(test), allow(dead_code))]

use std::io;

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

use crate::{
    ElectrumEndpoint, FixtureError,
    deadline::Deadline,
    diagnostics::{MAX_SOURCE_BYTES, redacted_head, redacted_source},
};

const SERVICE: &str = "electrs";
const PROBE_ID: &str = "nigiri-testcontainers";
pub(crate) const PROBE_OPERATION: &str = "blockchain.headers.subscribe";
/// The largest response line accepted before parsing. Electrs answers this method in well under a
/// kilobyte, so anything larger is a misbehaving or wrong service rather than a tip.
pub(crate) const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// The exact request Electrs answers, as one line.
///
/// Built from the same constants the response is attributed to, and asserted byte-for-byte by
/// `the_probe_request_is_exactly_one_headers_subscribe_line`, because a stray space or a missing
/// newline leaves Electrs waiting instead of answering.
pub(crate) fn request_line() -> String {
    format!("{{\"id\":\"{PROBE_ID}\",\"method\":\"{PROBE_OPERATION}\",\"params\":[]}}\n")
}

#[derive(serde::Deserialize)]
struct ElectrumResponse {
    result: ElectrumTip,
}

#[derive(serde::Deserialize)]
struct ElectrumTip {
    height: u64,
}

/// Reads the Electrum tip height, bounded by the shared startup budget.
pub(crate) async fn tip_height(
    endpoint: &ElectrumEndpoint,
    deadline: &Deadline,
) -> Result<u64, FixtureError> {
    let probed = deadline
        .run(
            SERVICE,
            "probing the Electrum tip height",
            probe(endpoint.host(), endpoint.port()),
        )
        .await?;

    probed.map_err(probe_error)
}

async fn probe(host: &str, port: u16) -> Result<u64, ProbeFailure> {
    let mut stream = TcpStream::connect((host, port))
        .await
        .map_err(|source| ProbeFailure::transport("connecting to the Electrum port", source))?;
    stream
        .write_all(request_line().as_bytes())
        .await
        .map_err(|source| ProbeFailure::transport("sending the Electrum request", source))?;
    stream
        .flush()
        .await
        .map_err(|source| ProbeFailure::transport("sending the Electrum request", source))?;

    // One byte past the bound is read on purpose: it is how an oversized line is recognised without
    // ever buffering the rest of it.
    let mut reader = BufReader::new(stream).take(MAX_RESPONSE_BYTES as u64 + 1);
    let mut response = Vec::new();
    reader
        .read_until(b'\n', &mut response)
        .await
        .map_err(|source| ProbeFailure::transport("reading the Electrum response", source))?;

    if response.len() > MAX_RESPONSE_BYTES {
        return Err(ProbeFailure::Oversized {
            read_bytes: response.len(),
        });
    }
    if response.is_empty() {
        return Err(ProbeFailure::Closed);
    }

    // Only `result.height` is deserialized, so nothing else in the response can reach a caller.
    let parsed: ElectrumResponse =
        serde_json::from_slice(&response).map_err(|source| ProbeFailure::Unusable {
            read_bytes: response.len(),
            // Only the category is kept: a serde message quotes the input it rejected, which is the
            // one thing a probe diagnostic must never carry.
            category: match source.classify() {
                serde_json::error::Category::Io => "transport",
                serde_json::error::Category::Syntax => "malformed JSON",
                serde_json::error::Category::Data => "unexpected shape",
                serde_json::error::Category::Eof => "truncated",
            },
        })?;

    Ok(parsed.result.height)
}

/// Why a probe failed, in terms that carry no part of the response body.
#[derive(Debug, thiserror::Error)]
enum ProbeFailure {
    #[error("{operation} failed")]
    Transport {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("the Electrum response exceeded {MAX_RESPONSE_BYTES} bytes before a newline")]
    Oversized { read_bytes: usize },
    #[error("the Electrum connection closed without answering")]
    Closed,
    #[error("the {read_bytes}-byte Electrum response carried no usable tip height: {category}")]
    Unusable {
        read_bytes: usize,
        category: &'static str,
    },
}

impl ProbeFailure {
    fn transport(operation: &'static str, source: io::Error) -> Self {
        Self::Transport { operation, source }
    }
}

fn probe_error(failure: ProbeFailure) -> FixtureError {
    // `ProbeFailure`'s own rendering is the diagnostic: it describes the shape of the failure and, for
    // a body that could not be used, only how large it was. The serde cause is deliberately not
    // rendered here, because its message can quote the input it rejected.
    FixtureError::Probe {
        service: SERVICE,
        operation: PROBE_OPERATION,
        diagnostics: redacted_head(&failure.to_string(), MAX_SOURCE_BYTES),
        source: redacted_source(failure),
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, time::Duration};

    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
    };

    use super::{MAX_RESPONSE_BYTES, PROBE_OPERATION, request_line, tip_height};
    use crate::{ElectrumEndpoint, FixtureError, deadline::Deadline};

    /// A loopback stand-in for Electrs that records the request it was sent.
    async fn electrum_stub(response: Vec<u8>) -> (ElectrumEndpoint, JoinHandle<Vec<u8>>) {
        stub_with(move |_| response.clone()).await
    }

    async fn stub_with(
        respond: impl Fn(&[u8]) -> Vec<u8> + Send + 'static,
    ) -> (ElectrumEndpoint, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener is available");
        let port = listener
            .local_addr()
            .expect("a bound listener has an address")
            .port();
        let endpoint =
            ElectrumEndpoint::new("127.0.0.1", port).expect("a loopback endpoint is valid");

        let served = tokio::spawn(async move {
            let (stream, _) = listener
                .accept()
                .await
                .expect("the probe must connect to the stub");
            let mut reader = BufReader::new(stream);
            let mut request = Vec::new();
            reader
                .read_until(b'\n', &mut request)
                .await
                .expect("the probe must send one line");

            let response = respond(&request);
            let mut stream = reader.into_inner();
            if response.is_empty() {
                // An empty response models Electrs closing the connection without answering.
                drop(stream);
            } else {
                stream
                    .write_all(&response)
                    .await
                    .expect("the stub must answer");
                stream.flush().await.expect("the stub must flush");
            }

            request
        });

        (endpoint, served)
    }

    fn deadline() -> Deadline {
        Deadline::new(Duration::from_secs(10)).expect("a positive deadline is valid")
    }

    fn assert_bounded_probe_failure(error: FixtureError, forbidden: &[&str]) {
        let FixtureError::Probe {
            service,
            operation,
            diagnostics,
            ..
        } = error
        else {
            panic!("an Electrum probe failure must be reported as a probe error");
        };
        assert_eq!(service, "electrs");
        assert_eq!(operation, PROBE_OPERATION);
        assert!(diagnostics.len() <= 4 * 1024, "{diagnostics:.64}");
        for fragment in forbidden {
            assert!(
                !diagnostics.contains(fragment),
                "diagnostics must not echo the response body: {diagnostics:.128}"
            );
        }
    }

    // Catches a regression that changes the exact Electrum request Electrs answers, including its
    // single trailing newline.
    #[test]
    fn the_probe_request_is_exactly_one_headers_subscribe_line() {
        assert_eq!(
            request_line(),
            "{\"id\":\"nigiri-testcontainers\",\"method\":\"blockchain.headers.subscribe\",\"params\":[]}\n"
        );
    }

    // Catches a regression that sends a different request on the wire than the one asserted above, or
    // that fails to read the tip height out of a well-formed result.
    #[tokio::test]
    async fn a_well_formed_result_yields_its_tip_height() {
        let (endpoint, served) = electrum_stub(
            b"{\"result\":{\"height\":101},\"id\":\"nigiri-testcontainers\"}\n".to_vec(),
        )
        .await;

        let height = tip_height(&endpoint, &deadline())
            .await
            .expect("a well-formed Electrum result must parse");

        assert_eq!(height, 101);
        let request = served.await.expect("the stub must finish");
        assert_eq!(
            String::from_utf8(request).expect("the request is UTF-8"),
            request_line()
        );
    }

    // Catches a regression that accepts a response whose result carries no height, or that treats
    // malformed JSON as a tip of zero.
    #[tokio::test]
    async fn a_response_without_a_usable_height_is_a_bounded_probe_failure() {
        for response in [
            b"{\"result\":{}}\n".to_vec(),
            b"{\"result\":{\"height\":\"one-hundred-one\"}}\n".to_vec(),
            b"{\"error\":{\"code\":-32601,\"message\":\"unknown method\"}}\n".to_vec(),
            b"not json at all\n".to_vec(),
        ] {
            let (endpoint, served) = electrum_stub(response).await;

            let error = tip_height(&endpoint, &deadline())
                .await
                .expect_err("a response without a usable height must fail");

            assert_bounded_probe_failure(error, &["one-hundred-one", "unknown method", "not json"]);
            let _ = served.await;
        }
    }

    // Catches a regression that buffers an unbounded Electrum response, or that echoes the oversized
    // body it refused into the fixture's diagnostics.
    #[tokio::test]
    async fn an_oversized_response_is_refused_before_it_is_parsed() {
        const MARKER: &str = "unbounded-electrum-body";

        let mut response =
            format!("{{\"result\":{{\"height\":101,\"padding\":\"{MARKER}").into_bytes();
        response.extend(std::iter::repeat_n(b'x', MAX_RESPONSE_BYTES + 1));
        response.extend_from_slice(b"\"}}\n");
        let (endpoint, served) = electrum_stub(response).await;

        let error = tip_height(&endpoint, &deadline())
            .await
            .expect_err("a response larger than the read bound must fail");

        assert_bounded_probe_failure(error, &[MARKER, "xxxxxxxx"]);
        let _ = served.await;
    }

    // Catches a regression that reports an unreachable or silent Electrs as a parse failure, or that
    // hangs instead of failing when the connection closes with no answer.
    #[tokio::test]
    async fn a_connection_that_answers_nothing_is_a_bounded_probe_failure() {
        let (endpoint, served) = electrum_stub(Vec::new()).await;

        let error = tip_height(&endpoint, &deadline())
            .await
            .expect_err("a closed connection must fail rather than hang");

        assert_bounded_probe_failure(error, &[]);
        let _ = served.await;
    }

    // Catches a regression that lets an unreachable endpoint surface a raw transport error, losing the
    // probed service and operation a caller needs to act on.
    #[tokio::test]
    async fn an_unreachable_endpoint_reports_the_probed_service() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener is available");
        let port = listener
            .local_addr()
            .expect("a bound listener has an address")
            .port();
        drop(listener);
        let endpoint =
            ElectrumEndpoint::new("127.0.0.1", port).expect("a loopback endpoint is valid");

        let error = tip_height(&endpoint, &deadline())
            .await
            .expect_err("an unreachable endpoint must fail");

        assert!(
            Error::source(&error).is_some(),
            "a transport failure must keep its cause"
        );
        assert_bounded_probe_failure(error, &[]);
    }

    // Catches a regression that gives the probe its own timeout instead of charging the shared startup
    // budget, which would let readiness outlive the deadline it was given.
    #[tokio::test(start_paused = true)]
    async fn the_probe_is_bounded_by_the_shared_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener is available");
        let endpoint = ElectrumEndpoint::new(
            "127.0.0.1",
            listener
                .local_addr()
                .expect("a bound listener has an address")
                .port(),
        )
        .expect("a loopback endpoint is valid");
        // Accepts the probe and then answers nothing, so only the deadline can end the read.
        let _silent = tokio::spawn(async move {
            let held: Vec<TcpStream> =
                vec![listener.accept().await.expect("the probe must connect").0];
            std::future::pending::<()>().await;
            drop(held);
        });

        let probe = tokio::spawn(async move {
            let deadline =
                Deadline::new(Duration::from_secs(5)).expect("a positive deadline is valid");
            tip_height(&endpoint, &deadline).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;

        let error = probe
            .await
            .expect("the probe task must not panic")
            .expect_err("an unanswered probe must expire with its deadline");
        assert!(matches!(
            error,
            FixtureError::ReadinessTimeout {
                service: "electrs",
                ..
            }
        ));
    }
}
