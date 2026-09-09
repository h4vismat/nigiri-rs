use super::*;

pub(super) async fn bootstrap_ready_channel<C: LndNodeConnector, B: BitcoinTip>(
    connector: &C,
    alice: &C::Client,
    bob: &C::Client,
    bitcoin: &B,
    bob_host: &str,
    allocation: ChannelAllocation,
    deadline: &Deadline,
) -> Result<OutPoint, FixtureError> {
    let alice_info = lnd_operation(
        deadline,
        "lnd-alice",
        "query Alice identity for channel bootstrap",
        connector.get_info(alice),
    )
    .await?;
    let bob_info = lnd_operation(
        deadline,
        "lnd-bob",
        "query Bob identity for channel bootstrap",
        connector.get_info(bob),
    )
    .await?;

    let address = lnd_operation(
        deadline,
        "lnd-alice",
        "request Alice P2WPKH funding address",
        connector.new_address(alice),
    )
    .await?;
    deadline
        .run(
            "lightning-channel",
            "fund Alice on-chain wallet and mine its confirmation",
            bitcoin.fund_address(&address, allocation.funding_amount),
        )
        .await??;
    wait_for_confirmed_balance(connector, alice, allocation.capacity, deadline).await?;

    ensure_peer_connected(connector, alice, bob_info.public_key, bob_host, deadline).await?;
    let request =
        OpenChannelRequest::new(bob_info.public_key, allocation.capacity, allocation.push)
            .map_err(|error| lightning_bootstrap_error("configure channel opening", error))?;
    let channel_point =
        open_and_confirm_channel(connector, alice, bitcoin, request, deadline).await?;

    wait_for_active_channel(
        connector,
        alice,
        bob,
        bitcoin,
        channel_point,
        alice_info.public_key,
        bob_info.public_key,
        bob_host,
        deadline,
    )
    .await?;
    prove_readiness_payment(connector, alice, bob, deadline).await?;
    Box::pin(prove_reverse_readiness_with_retry(
        connector,
        alice,
        bob,
        bitcoin,
        channel_point,
        alice_info.public_key,
        bob_info.public_key,
        bob_host,
        deadline,
    ))
    .await?;
    Ok(channel_point)
}

pub(super) async fn wait_for_confirmed_balance<C: LndNodeConnector>(
    connector: &C,
    alice: &C::Client,
    required: Sats,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut observation = format!(
        "waiting for Alice confirmed balance to reach {} satoshis",
        required.as_u64()
    );
    loop {
        match deadline
            .run(
                "lnd-alice",
                &observation,
                connector.confirmed_balance(alice),
            )
            .await?
        {
            Ok(balance) if balance >= required => return Ok(()),
            Ok(balance) => {
                observation = format!(
                    "Alice confirmed balance={} required={}",
                    balance.as_u64(),
                    required.as_u64()
                );
            }
            Err(error) if is_transient_lnd_readiness(&error) => {
                observation = redacted_tail(&format!("Alice wallet balance is not ready: {error}"));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error(
                    "query Alice confirmed balance",
                    error,
                ));
            }
        }
        wait_before_retry(deadline, "lnd-alice", &observation).await?;
    }
}

pub(super) async fn ensure_peer_connected<C: LndNodeConnector>(
    connector: &C,
    alice: &C::Client,
    bob_public_key: PublicKey,
    bob_host: &str,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let peer = PeerAddress::new(bob_public_key, bob_host, LND_PEER_PORT)
        .map_err(|error| lightning_bootstrap_error("configure Bob peer address", error))?;
    let mut observation = "waiting for Alice to connect to Bob over the private network".to_owned();
    loop {
        match deadline
            .run(
                "lightning-channel",
                &observation,
                connector.peer_connected(alice, bob_public_key),
            )
            .await?
        {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if is_transient_lnd_readiness(&error) => {
                observation = redacted_tail(&format!("peer listing is not ready: {error}"));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error(
                    "query Alice peer connection",
                    error,
                ));
            }
        }

        match deadline
            .run(
                "lightning-channel",
                &observation,
                connector.connect_peer(alice, peer.clone()),
            )
            .await?
        {
            Ok(()) => {}
            Err(error) if is_transient_lnd_readiness(&error) || is_already_connected(&error) => {
                observation = redacted_tail(&format!("peer connection is converging: {error}"));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error("connect Alice to Bob", error));
            }
        }
        wait_before_retry(deadline, "lightning-channel", &observation).await?;
    }
}

pub(super) async fn open_and_confirm_channel<C: LndNodeConnector, B: BitcoinTip>(
    connector: &C,
    alice: &C::Client,
    bitcoin: &B,
    request: OpenChannelRequest,
    deadline: &Deadline,
) -> Result<OutPoint, FixtureError> {
    let baseline = deadline
        .run(
            "lightning-channel",
            "capture mempool before opening Alice-to-Bob channel",
            bitcoin.mempool_transactions(),
        )
        .await??;
    let open = deadline.run(
        "lightning-channel",
        "open Alice-to-Bob channel",
        connector.open_channel(alice, request),
    );
    tokio::pin!(open);
    let mut opened = None;
    let mut observation = "waiting for the channel funding transaction in mempool".to_owned();

    let trigger_transactions = loop {
        let current = {
            let mempool = deadline.run(
                "lightning-channel",
                &observation,
                bitcoin.mempool_transactions(),
            );
            tokio::pin!(mempool);
            tokio::select! {
                biased;
                result = &mut open, if opened.is_none() => {
                    opened = Some(flatten_lnd_operation("open channel", result)?);
                    None
                },
                result = &mut mempool => Some(result??),
            }
        };
        let Some(current) = current else {
            continue;
        };
        let newly_observed = current
            .difference(&baseline)
            .copied()
            .collect::<BTreeSet<_>>();
        if !newly_observed.is_empty() {
            break newly_observed;
        }
        observation = "channel funding transaction is not yet in mempool".to_owned();
        wait_before_retry(deadline, "lightning-channel", &observation).await?;
    };

    deadline
        .run(
            "lightning-channel",
            "mine six channel funding confirmations",
            bitcoin.mine_blocks(CHANNEL_CONFIRMATIONS),
        )
        .await??;
    let channel_point = match opened {
        Some(channel_point) => channel_point,
        None => flatten_lnd_operation("open channel", open.await)?,
    };
    // This fixture owns an isolated regtest mempool. If callers inject simultaneous transactions,
    // the trigger set may contain more than one txid, but the final funding txid must still be one
    // of the transactions whose appearance caused this fixture to mine exactly once.
    if !trigger_transactions.contains(&channel_point.txid) {
        return Err(lightning_bootstrap_error(
            "verify channel funding transaction",
            LndError::InvalidResponse {
                operation: "open channel".into(),
                detail: "funding transaction was not observed before confirmation mining".into(),
                identifier: Some(channel_point.txid.to_string()),
            },
        ));
    }
    Ok(channel_point)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn wait_for_active_channel<C: LndNodeConnector, B: BitcoinTip>(
    connector: &C,
    alice: &C::Client,
    bob: &C::Client,
    bitcoin: &B,
    channel_point: OutPoint,
    alice_public_key: PublicKey,
    bob_public_key: PublicKey,
    bob_host: &str,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut observation = format!("waiting for active channel {channel_point}");
    loop {
        ensure_peer_connected(connector, alice, bob_public_key, bob_host, deadline).await?;
        let alice_channel = async {
            let result = deadline
                .run(
                    "lnd-alice",
                    &observation,
                    connector.channel_readiness(alice, channel_point, bob_public_key),
                )
                .await?;
            channel_observation("query Alice channel", result)
        };
        let bob_channel = async {
            let result = deadline
                .run(
                    "lnd-bob",
                    &observation,
                    connector.channel_readiness(bob, channel_point, alice_public_key),
                )
                .await?;
            channel_observation("query Bob channel", result)
        };
        let alice_info = async {
            let result = deadline
                .run("lnd-alice", &observation, connector.get_info(alice))
                .await?;
            info_observation("query Alice graph synchronization", result)
        };
        let bob_info = async {
            let result = deadline
                .run("lnd-bob", &observation, connector.get_info(bob))
                .await?;
            info_observation("query Bob graph synchronization", result)
        };
        let bitcoin_height = async {
            deadline
                .run("bitcoind", &observation, bitcoin.block_height())
                .await?
        };
        let (alice_channel, bob_channel, alice_info, bob_info, bitcoin_height) = tokio::try_join!(
            alice_channel,
            bob_channel,
            alice_info,
            bob_info,
            bitcoin_height,
        )?;
        if channel_is_spendable(alice_channel)
            && channel_is_spendable(bob_channel)
            && alice_info.is_some_and(|info| graph_synchronized(info, bitcoin_height))
            && bob_info.is_some_and(|info| graph_synchronized(info, bitcoin_height))
        {
            return Ok(());
        }
        observation = format!(
            "channel {channel_point} at Bitcoin height {bitcoin_height}: Alice={alice_channel:?} info={alice_info:?}; Bob={bob_channel:?} info={bob_info:?}"
        );
        wait_before_retry(deadline, "lightning-channel", &observation).await?;
    }
}

pub(super) fn info_observation(
    operation: &'static str,
    result: Result<LndSyncStatus, LndError>,
) -> Result<Option<LndSyncStatus>, FixtureError> {
    match result {
        Ok(info) => Ok(Some(info)),
        Err(error) if is_transient_lnd_readiness(&error) => Ok(None),
        Err(error) => Err(lightning_bootstrap_error(operation, error)),
    }
}

pub(super) fn channel_observation(
    operation: &'static str,
    result: Result<Option<ChannelReadiness>, LndError>,
) -> Result<Option<ChannelReadiness>, FixtureError> {
    match result {
        Ok(channel) => Ok(channel),
        Err(error) if is_transient_lnd_readiness(&error) => Ok(None),
        Err(error) => Err(lightning_bootstrap_error(operation, error)),
    }
}

pub(super) fn channel_is_spendable(channel: Option<ChannelReadiness>) -> bool {
    matches!(
        channel,
        Some(ChannelReadiness { active: true, local_balance })
            if local_balance > READINESS_PAYMENT
    )
}

pub(super) async fn prove_readiness_payment<C: LndNodeConnector>(
    connector: &C,
    alice: &C::Client,
    bob: &C::Client,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let (invoice, payment_hash) =
        create_readiness_invoice(connector, bob, "lnd-bob", deadline).await?;
    let payment = lnd_operation(
        deadline,
        "lnd-alice",
        "pay Alice-to-Bob 1000-msat readiness invoice",
        connector.pay_readiness_invoice(alice, &invoice),
    )
    .await?;
    verify_readiness_payment(payment_hash, payment)?;
    wait_for_settled_invoice(connector, bob, "lnd-bob", payment_hash, deadline).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn prove_reverse_readiness_with_retry<C: LndNodeConnector, B: BitcoinTip>(
    connector: &C,
    alice: &C::Client,
    bob: &C::Client,
    bitcoin: &B,
    channel_point: OutPoint,
    alice_public_key: PublicKey,
    bob_public_key: PublicKey,
    bob_host: &str,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut observation = "waiting for Bob-to-Alice routing policy propagation".to_owned();
    loop {
        wait_for_active_channel(
            connector,
            alice,
            bob,
            bitcoin,
            channel_point,
            alice_public_key,
            bob_public_key,
            bob_host,
            deadline,
        )
        .await?;
        let (invoice, payment_hash) =
            create_readiness_invoice(connector, alice, "lnd-alice", deadline).await?;
        let payment = deadline
            .run(
                "lnd-bob",
                &observation,
                connector.pay_readiness_invoice(bob, &invoice),
            )
            .await?;
        match payment {
            Ok(payment) => {
                verify_readiness_payment(payment_hash, payment)?;
                return wait_for_settled_invoice(
                    connector,
                    alice,
                    "lnd-alice",
                    payment_hash,
                    deadline,
                )
                .await;
            }
            Err(error) if is_transient_routing_failure(&error) => {
                observation = redacted_tail(&format!(
                    "Bob-to-Alice route is not ready for invoice {payment_hash}: {error}"
                ));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error(
                    "pay Bob-to-Alice readiness invoice",
                    error,
                ));
            }
        }
        wait_before_retry(deadline, "lightning-channel", &observation).await?;
    }
}

pub(super) async fn create_readiness_invoice<C: LndNodeConnector>(
    connector: &C,
    receiver: &C::Client,
    service: &'static str,
    deadline: &Deadline,
) -> Result<(C::Invoice, sha256::Hash), FixtureError> {
    lnd_operation(
        deadline,
        service,
        "create 1000-msat readiness invoice",
        connector.create_readiness_invoice(receiver),
    )
    .await
}

pub(super) fn verify_readiness_payment(
    payment_hash: sha256::Hash,
    payment: PaymentReadiness,
) -> Result<(), FixtureError> {
    if payment.payment_hash != payment_hash || payment.state != PaymentState::Succeeded {
        return Err(lightning_bootstrap_error(
            "verify readiness payment",
            LndError::InvalidResponse {
                operation: "readiness payment".into(),
                detail: "payment did not succeed with the readiness invoice hash".into(),
                identifier: Some(payment_hash.to_string()),
            },
        ));
    }
    Ok(())
}

pub(super) async fn wait_for_settled_invoice<C: LndNodeConnector>(
    connector: &C,
    receiver: &C::Client,
    service: &'static str,
    payment_hash: sha256::Hash,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut observation = format!("waiting for readiness invoice {payment_hash} to settle");
    loop {
        match deadline
            .run(
                service,
                &observation,
                connector.invoice_state(receiver, payment_hash),
            )
            .await?
        {
            Ok(InvoiceState::Settled) => return Ok(()),
            Ok(InvoiceState::Open | InvoiceState::Accepted) => {}
            Ok(state) => {
                return Err(lightning_bootstrap_error(
                    "verify readiness invoice",
                    LndError::InvalidResponse {
                        operation: "readiness invoice".into(),
                        detail: format!("invoice reached non-settled state {state:?}").into(),
                        identifier: Some(payment_hash.to_string()),
                    },
                ));
            }
            Err(error) if is_transient_lnd_readiness(&error) => {
                observation = redacted_tail(&format!("readiness invoice is not ready: {error}"));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error("lookup readiness invoice", error));
            }
        }
        wait_before_retry(deadline, service, &observation).await?;
    }
}

pub(super) fn is_transient_routing_failure(error: &LndError) -> bool {
    matches!(
        error,
        LndError::PaymentFailed { reason, .. }
            if matches!(reason.as_ref(), "no route" | "insufficient balance")
    )
}

pub(super) async fn lnd_operation<T>(
    deadline: &Deadline,
    service: &'static str,
    operation: &'static str,
    future: impl Future<Output = Result<T, LndError>>,
) -> Result<T, FixtureError> {
    let result = deadline.run(service, operation, future).await?;
    result.map_err(|error| lightning_bootstrap_error(operation, error))
}

pub(super) fn flatten_lnd_operation<T>(
    operation: &'static str,
    result: Result<Result<T, LndError>, FixtureError>,
) -> Result<T, FixtureError> {
    result?.map_err(|error| lightning_bootstrap_error(operation, error))
}

pub(super) fn is_already_connected(error: &LndError) -> bool {
    matches!(
        error,
        LndError::Status {
            code: LndStatusCode::AlreadyExists,
            ..
        }
    )
}
