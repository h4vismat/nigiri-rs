use std::{fmt, time::Duration};

use nigiri_rs_core::NigiriClient;

use crate::{
    ElectrumEndpoint, FixtureError,
    chain::FixtureChain,
    deadline::Deadline,
    diagnostics::{MAX_SOURCE_BYTES, redacted_head},
    electrum,
};

const SERVICE: &str = "fixture";
/// Shared by every fixture readiness loop, so polling cannot drift between them.
pub(crate) const RETRY_DELAY: Duration = Duration::from_millis(100);

/// The three heights that must agree before a fixture is queryable.
///
/// The node mines; Esplora indexes what the node mined; Electrum serves what Esplora indexed. A
/// caller that queries before all three agree sees a wallet whose funds do not exist yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Heights {
    pub(crate) node: u64,
    pub(crate) esplora: u64,
    pub(crate) electrum: u64,
}

impl Heights {
    pub(crate) fn agree(&self) -> bool {
        self.node == self.esplora && self.node == self.electrum
    }
}

impl fmt::Display for Heights {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "node={} esplora={} electrum={}",
            self.node, self.esplora, self.electrum
        )
    }
}

/// Waits until the node, Esplora, and Electrum report the same tip height.
///
/// Every operation and every pause is charged to the same shared budget, so readiness cannot outlive
/// the deadline it was given. A service that is merely not up yet is retried rather than reported:
/// only the budget running out ends this loop with an error.
pub(crate) async fn wait_for_sync<C: FixtureChain>(
    client: &NigiriClient<C>,
    endpoint: &ElectrumEndpoint,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut observation = "waiting for node, Esplora, and Electrum tip synchronization".to_owned();

    loop {
        match observe_heights::<C>(client, endpoint, deadline, &observation).await? {
            Ok(heights) if heights.agree() => return Ok(()),
            Ok(heights) => observation = heights.to_string(),
            Err(unavailable) => observation = unavailable,
        }

        deadline
            .run(SERVICE, &observation, tokio::time::sleep(RETRY_DELAY))
            .await?;
    }
}

/// One round of the three heights, or a bounded description of the service that was not ready.
///
/// The outer error is only ever the shared deadline expiring; a service failure is an inner `Err`, so
/// the loop above cannot mistake "not up yet" for "will never be ready".
async fn observe_heights<C: FixtureChain>(
    client: &NigiriClient<C>,
    endpoint: &ElectrumEndpoint,
    deadline: &Deadline,
    observation: &str,
) -> Result<Result<Heights, String>, FixtureError> {
    let node = match deadline
        .run(
            SERVICE,
            observation,
            client.rpc::<u64, _>("getblockcount", ()),
        )
        .await?
    {
        Ok(height) => height,
        Err(error) => return Ok(Err(transient_observation("node", &error.into()))),
    };

    let esplora = match deadline
        .run(SERVICE, observation, client.block_height())
        .await?
    {
        Ok(height) => height,
        Err(error) => return Ok(Err(transient_observation("esplora", &error.into()))),
    };

    // The probe is already bounded by this deadline, so its expiry must propagate rather than be
    // retried; anything else about it is transient.
    let electrum = match electrum::tip_height(endpoint, deadline).await {
        Ok(height) => height,
        Err(error @ FixtureError::ReadinessTimeout { .. }) => return Err(error),
        Err(error) => return Ok(Err(transient_observation("electrum", &error))),
    };

    Ok(Ok(Heights {
        node,
        esplora,
        electrum,
    }))
}

/// Names the service that was not ready, with its cause bounded and redacted.
fn transient_observation(service: &str, error: &FixtureError) -> String {
    redacted_head(&format!("{service}: {error}"), MAX_SOURCE_BYTES)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use nigiri_rs_core::{Bitcoin, NigiriClient, NigiriConfig, NigiriError};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };
    use url::Url;

    use super::{Heights, SERVICE, transient_observation, wait_for_sync};
    use crate::{ElectrumEndpoint, FixtureError, deadline::Deadline};

    /// One scripted round of the three heights `wait_for_sync` compares.
    #[derive(Clone, Copy)]
    struct Round {
        node: Option<u64>,
        esplora: Option<u64>,
        electrum: Option<u64>,
    }

    impl Round {
        const fn all(height: u64) -> Self {
            Self {
                node: Some(height),
                esplora: Some(height),
                electrum: Some(height),
            }
        }

        const fn heights(node: u64, esplora: u64, electrum: u64) -> Self {
            Self {
                node: Some(node),
                esplora: Some(esplora),
                electrum: Some(electrum),
            }
        }

        const fn unavailable() -> Self {
            Self {
                node: None,
                esplora: None,
                electrum: None,
            }
        }

        /// The node is up but the indexer's HTTP half is not answering yet.
        const fn esplora_unavailable(node: u64) -> Self {
            Self {
                node: Some(node),
                esplora: None,
                electrum: None,
            }
        }

        /// The node and Esplora are up but the Electrum port is not serving yet.
        const fn electrum_unavailable(node: u64, esplora: u64) -> Self {
            Self {
                node: Some(node),
                esplora: Some(esplora),
                electrum: None,
            }
        }
    }

    /// Serves the node RPC, the Esplora tip, and the Electrum tip from one scripted script, so a
    /// readiness round can be driven without Docker or a real node.
    struct SyncStub {
        client: NigiriClient<Bitcoin>,
        endpoint: ElectrumEndpoint,
        rounds: Arc<AtomicUsize>,
        servers: Vec<JoinHandle<()>>,
    }

    impl SyncStub {
        /// Replaces the Electrum half with a listener that accepts and then says nothing, so the probe
        /// can only end on the shared deadline.
        async fn with_silent_electrum(mut self) -> Self {
            let silent = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback listener is available");
            let port = silent
                .local_addr()
                .expect("a bound listener has an address")
                .port();

            self.servers.push(tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok((stream, _)) = silent.accept().await {
                    held.push(stream);
                }
            }));
            self.endpoint =
                ElectrumEndpoint::new("127.0.0.1", port).expect("a loopback endpoint is valid");
            self
        }

        async fn start(script: Vec<Round>) -> Self {
            let script = Arc::new(script);
            let rounds = Arc::new(AtomicUsize::new(0));

            let http = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback listener is available");
            let http_port = http
                .local_addr()
                .expect("a bound listener has an address")
                .port();
            let electrum = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback listener is available");
            let electrum_port = electrum
                .local_addr()
                .expect("a bound listener has an address")
                .port();

            let http_server = {
                let script = Arc::clone(&script);
                let rounds = Arc::clone(&rounds);
                tokio::spawn(async move {
                    loop {
                        let Ok((mut stream, _)) = http.accept().await else {
                            return;
                        };
                        let script = Arc::clone(&script);
                        let rounds = Arc::clone(&rounds);
                        tokio::spawn(async move {
                            let mut request = vec![0_u8; 8 * 1024];
                            let read = stream.read(&mut request).await.unwrap_or(0);
                            let request = String::from_utf8_lossy(&request[..read]).into_owned();

                            // The node RPC opens each readiness round, so it is what advances the
                            // script; the Esplora and Electrum reads that follow report the same
                            // round, and one loop iteration therefore sees one consistent triplet.
                            let response = if request.starts_with("POST") {
                                let round = advance_round(&script, &rounds);
                                match round.node {
                                    Some(height) => json_response(&format!(
                                        "{{\"result\":{height},\"error\":null,\"id\":\"1\"}}"
                                    )),
                                    None => status_response(503, "node warming up"),
                                }
                            } else {
                                match current_round(&script, &rounds).esplora {
                                    Some(height) => text_response(&height.to_string()),
                                    None => status_response(503, "esplora warming up"),
                                }
                            };
                            let _ = stream.write_all(response.as_bytes()).await;
                            let _ = stream.flush().await;
                        });
                    }
                })
            };

            let electrum_server = {
                let script = Arc::clone(&script);
                let rounds = Arc::clone(&rounds);
                tokio::spawn(async move {
                    loop {
                        let Ok((mut stream, _)) = electrum.accept().await else {
                            return;
                        };
                        let script = Arc::clone(&script);
                        let rounds = Arc::clone(&rounds);
                        tokio::spawn(async move {
                            let mut request = vec![0_u8; 1024];
                            let _ = stream.read(&mut request).await;

                            match current_round(&script, &rounds).electrum {
                                Some(height) => {
                                    let _ = stream
                                        .write_all(
                                            format!("{{\"result\":{{\"height\":{height}}}}}\n")
                                                .as_bytes(),
                                        )
                                        .await;
                                    let _ = stream.flush().await;
                                }
                                // A closed connection is how an unready Electrs behaves.
                                None => drop(stream),
                            }
                        });
                    }
                })
            };

            let base = Url::parse(&format!("http://127.0.0.1:{http_port}/"))
                .expect("a loopback URL is valid");
            let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
                esplora_url: base.clone(),
                node_rpc_url: base,
                node_rpc_user: "admin1".to_owned(),
                node_rpc_password: "123".to_owned(),
                timeout: Duration::from_secs(5),
                ..Default::default()
            })
            .expect("a loopback fixture configuration is valid");

            Self {
                client,
                endpoint: ElectrumEndpoint::new("127.0.0.1", electrum_port)
                    .expect("a loopback endpoint is valid"),
                rounds,
                servers: vec![http_server, electrum_server],
            }
        }

        fn attempts(&self) -> usize {
            self.rounds.load(Ordering::SeqCst)
        }
    }

    impl Drop for SyncStub {
        fn drop(&mut self) {
            for server in &self.servers {
                server.abort();
            }
        }
    }

    /// Opens the next round and returns it. The last scripted round repeats, so a script can describe
    /// a fixture that never converges.
    fn advance_round(script: &[Round], rounds: &AtomicUsize) -> Round {
        let index = rounds.fetch_add(1, Ordering::SeqCst).min(script.len() - 1);
        script[index]
    }

    /// The round already opened by this iteration's node RPC.
    fn current_round(script: &[Round], rounds: &AtomicUsize) -> Round {
        let index = rounds
            .load(Ordering::SeqCst)
            .saturating_sub(1)
            .min(script.len() - 1);
        script[index]
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn text_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn status_response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    // Catches a regression that renders the observed triplet in a form a reader cannot compare, or
    // that reports synchronization before all three services agree.
    #[test]
    fn a_triplet_renders_compactly_and_only_agrees_when_all_three_match() {
        assert_eq!(
            Heights {
                node: 101,
                esplora: 100,
                electrum: 100,
            }
            .to_string(),
            "node=101 esplora=100 electrum=100"
        );

        assert!(
            Heights {
                node: 101,
                esplora: 101,
                electrum: 101,
            }
            .agree()
        );
        for disagreeing in [
            Heights {
                node: 101,
                esplora: 100,
                electrum: 101,
            },
            Heights {
                node: 101,
                esplora: 101,
                electrum: 100,
            },
            Heights {
                node: 100,
                esplora: 101,
                electrum: 101,
            },
        ] {
            assert!(!disagreeing.agree(), "{disagreeing}");
        }
    }

    // Catches a regression that folds a credential-bearing or unbounded service error into the
    // observation a readiness timeout reports.
    #[test]
    fn a_transient_observation_is_bounded_and_redacted() {
        let observation = transient_observation(
            "node",
            &FixtureError::Client(NigiriError::InvalidResponse {
                operation: "block height".into(),
                detail: format!("{} admin1:123", "node-error-".repeat(2_000)),
            }),
        );

        assert!(observation.len() <= 4 * 1024);
        assert!(!observation.contains("admin1"));
        assert!(
            observation.starts_with("node: "),
            "the observation must name the service that failed: {observation:.64}"
        );
    }

    // Catches a regression that reports readiness before Esplora and Electrum have caught up with the
    // node, which is what makes a freshly mined fixture queryable.
    #[tokio::test]
    async fn synchronization_waits_until_all_three_heights_agree() {
        let stub = SyncStub::start(vec![
            Round::heights(101, 0, 0),
            Round::heights(101, 100, 0),
            Round::all(101),
        ])
        .await;
        let deadline =
            Deadline::new(Duration::from_secs(30)).expect("a positive deadline is valid");

        wait_for_sync(&stub.client, &stub.endpoint, &deadline)
            .await
            .expect("a converging fixture must become ready");

        assert_eq!(
            stub.attempts(),
            3,
            "each disagreeing round must be retried, not accepted"
        );
    }

    // Catches a regression that surfaces an unready service as a fixture failure instead of retrying
    // it, which would make startup fail on the ordinary case of Electrs still opening its port.
    #[tokio::test]
    async fn an_unavailable_service_is_retried_rather_than_reported() {
        let stub = SyncStub::start(vec![
            Round::unavailable(),
            Round::heights(101, 101, 0),
            Round::all(101),
        ])
        .await;
        let deadline =
            Deadline::new(Duration::from_secs(30)).expect("a positive deadline is valid");

        wait_for_sync(&stub.client, &stub.endpoint, &deadline)
            .await
            .expect("a transiently unavailable service must be retried");

        assert_eq!(stub.attempts(), 3);
    }

    // Catches a regression in the two transient paths a node-only failure never reaches. An indexer
    // that has not opened its port yet is the ordinary case this loop exists to tolerate.
    #[tokio::test]
    async fn an_indexer_that_is_not_serving_yet_is_retried_and_named() {
        let stub = SyncStub::start(vec![
            Round::esplora_unavailable(101),
            Round::electrum_unavailable(101, 101),
            Round::all(101),
        ])
        .await;
        let deadline =
            Deadline::new(Duration::from_secs(30)).expect("a positive deadline is valid");

        wait_for_sync(&stub.client, &stub.endpoint, &deadline)
            .await
            .expect("an indexer that is still starting must be retried");

        assert_eq!(
            stub.attempts(),
            3,
            "each half of the indexer must be retried, not reported"
        );
    }

    // Catches a regression that names the wrong service when an indexer half fails: the observation a
    // readiness timeout carries is the only thing that says where to look.
    #[tokio::test]
    async fn a_failing_indexer_half_is_named_in_the_observation() {
        for (round, expected) in [
            (Round::esplora_unavailable(101), "esplora: "),
            (Round::electrum_unavailable(101, 101), "electrum: "),
        ] {
            let stub = SyncStub::start(vec![round]).await;
            let deadline =
                Deadline::new(Duration::from_secs(1)).expect("a positive deadline is valid");

            let error = wait_for_sync(&stub.client, &stub.endpoint, &deadline)
                .await
                .expect_err("a permanently unavailable indexer must expire");

            let FixtureError::ReadinessTimeout {
                last_observation, ..
            } = error
            else {
                panic!("an exhausted readiness budget must be a readiness timeout");
            };
            assert!(
                last_observation.starts_with(expected),
                "expected {expected:?} to open the observation: {last_observation}"
            );
        }
    }

    // Catches a regression that retries an Electrum probe whose own expiry already spent the shared
    // budget. Retrying it would report the failure as the fixture's rather than the indexer's, sending
    // a reader to the wrong service.
    #[tokio::test]
    async fn an_electrum_probe_expiry_is_reported_as_electrs_rather_than_retried() {
        let stub = SyncStub::start(vec![Round::all(101)])
            .await
            .with_silent_electrum()
            .await;
        let deadline = Deadline::new(Duration::from_secs(1)).expect("a positive deadline is valid");

        let error = wait_for_sync(&stub.client, &stub.endpoint, &deadline)
            .await
            .expect_err("a probe that is never answered must expire");

        assert!(
            matches!(
                error,
                FixtureError::ReadinessTimeout {
                    service: "electrs",
                    ..
                }
            ),
            "{error}"
        );
    }

    // Catches a regression that returns a synthetic error instead of the readiness timeout, or that
    // loses the last thing observed before the shared budget ran out.
    #[tokio::test]
    async fn an_unconverging_fixture_expires_with_its_last_observation() {
        let stub = SyncStub::start(vec![Round::heights(101, 100, 100)]).await;
        let deadline = Deadline::new(Duration::from_secs(1)).expect("a positive deadline is valid");

        let error = wait_for_sync(&stub.client, &stub.endpoint, &deadline)
            .await
            .expect_err("a fixture that never converges must expire");

        assert!(
            stub.attempts() > 1,
            "the loop must have retried rather than reported the first disagreement"
        );

        let FixtureError::ReadinessTimeout {
            service,
            last_observation,
            ..
        } = error
        else {
            panic!("an exhausted readiness budget must be a readiness timeout");
        };
        // The shared budget can run out at either of two legitimate points: in this loop, which
        // reports the triplet it last observed, or inside the Electrum probe, which reports itself.
        // Both are correct, and which one wins is a matter of where the clock happens to stop, so
        // asserting one of them specifically is what made this test flaky.
        assert!(
            [SERVICE, crate::electrs::SERVICE].contains(&service),
            "an expiry must name the fixture or the service it was waiting on, got {service}"
        );
        assert!(
            last_observation.contains("node=101 esplora=100 electrum=100")
                || last_observation.contains("Electrum"),
            "the expiry must carry what was last observed: {last_observation}"
        );
    }
}
