use std::{borrow::Cow, future::Future};

use bitcoin::hashes::{Hash, sha256};
use lightning_invoice::Bolt11Invoice;
use prost::Message as _;
use tonic::{Request, Response, Status, transport::Channel as TonicChannel};

use crate::{
    CreateInvoiceRequest, InvoiceRecord, LndClient, LndError, PaymentOptions, PaymentRecord,
    PaymentState,
    convert::{
        created_invoice, invalid_response_with_identifier, invoice as convert_invoice,
        payment as convert_payment,
    },
    error::bounded,
    proto::{
        lnrpc::{
            AddInvoiceResponse, Invoice as ProtoInvoice, Payment, PaymentFailureReason,
            PaymentHash, lightning_client::LightningClient,
        },
        routerrpc::{SendPaymentRequest, TrackPaymentRequest, router_client::RouterClient},
    },
    transport::{
        ClientInner, authenticated_mutating_request, authenticated_mutating_request_until,
        authenticated_request, authenticated_request_until, bounded_request_until,
        operation_deadline,
    },
};

pub(crate) trait InvoiceRpc: Send {
    fn add_invoice(
        &mut self,
        request: Request<ProtoInvoice>,
    ) -> impl Future<Output = Result<Response<AddInvoiceResponse>, Status>> + Send;
    fn lookup_invoice(
        &mut self,
        request: Request<PaymentHash>,
    ) -> impl Future<Output = Result<Response<ProtoInvoice>, Status>> + Send;
}

impl InvoiceRpc for LightningClient<TonicChannel> {
    fn add_invoice(
        &mut self,
        request: Request<ProtoInvoice>,
    ) -> impl Future<Output = Result<Response<AddInvoiceResponse>, Status>> + Send {
        LightningClient::add_invoice(self, request)
    }

    fn lookup_invoice(
        &mut self,
        request: Request<PaymentHash>,
    ) -> impl Future<Output = Result<Response<ProtoInvoice>, Status>> + Send {
        LightningClient::lookup_invoice(self, request)
    }
}

pub(crate) trait PaymentStream: Send {
    fn message(&mut self) -> impl Future<Output = Result<Option<Payment>, Status>> + Send;
}

impl PaymentStream for tonic::Streaming<Payment> {
    fn message(&mut self) -> impl Future<Output = Result<Option<Payment>, Status>> + Send {
        tonic::Streaming::message(self)
    }
}

pub(crate) trait RouterRpc: Send {
    type PaymentStream: PaymentStream;

    fn send_payment_v2(
        &mut self,
        request: Request<SendPaymentRequest>,
    ) -> impl Future<Output = Result<Response<Self::PaymentStream>, Status>> + Send;
    fn track_payment_v2(
        &mut self,
        request: Request<TrackPaymentRequest>,
    ) -> impl Future<Output = Result<Response<Self::PaymentStream>, Status>> + Send;
}

impl RouterRpc for RouterClient<TonicChannel> {
    type PaymentStream = tonic::Streaming<Payment>;

    fn send_payment_v2(
        &mut self,
        request: Request<SendPaymentRequest>,
    ) -> impl Future<Output = Result<Response<Self::PaymentStream>, Status>> + Send {
        RouterClient::send_payment_v2(self, request)
    }

    fn track_payment_v2(
        &mut self,
        request: Request<TrackPaymentRequest>,
    ) -> impl Future<Output = Result<Response<Self::PaymentStream>, Status>> + Send {
        RouterClient::track_payment_v2(self, request)
    }
}

impl LndClient {
    /// Creates a fixed-amount BOLT11 invoice.
    pub async fn create_invoice(
        &self,
        request: CreateInvoiceRequest,
    ) -> Result<InvoiceRecord, LndError> {
        let mut rpc = LightningClient::new(self.inner.channel().await);
        create_invoice_with(&self.inner, &mut rpc, request).await
    }

    /// Looks up an invoice by its payment hash.
    pub async fn lookup_invoice(
        &self,
        payment_hash: sha256::Hash,
    ) -> Result<InvoiceRecord, LndError> {
        let mut rpc = LightningClient::new(self.inner.channel().await);
        lookup_invoice_with(&self.inner, &mut rpc, payment_hash).await
    }

    /// Pays a BOLT11 invoice and returns only after terminal success.
    pub async fn pay_invoice(
        &self,
        invoice: &Bolt11Invoice,
        options: PaymentOptions,
    ) -> Result<PaymentRecord, LndError> {
        let mut rpc = RouterClient::new(self.inner.channel().await);
        pay_invoice_with(&self.inner, &mut rpc, invoice, options).await
    }

    /// Tracks a payment by hash until LND reports terminal success or failure.
    pub async fn lookup_payment(
        &self,
        payment_hash: sha256::Hash,
    ) -> Result<PaymentRecord, LndError> {
        let mut rpc = RouterClient::new(self.inner.channel().await);
        lookup_payment_with(&self.inner, &mut rpc, payment_hash).await
    }
}

pub(crate) async fn create_invoice_with<R: InvoiceRpc>(
    inner: &ClientInner,
    rpc: &mut R,
    request: CreateInvoiceRequest,
) -> Result<InvoiceRecord, LndError> {
    let expected_amount = request.amount();
    let request = proto_invoice_request(request)?;
    let expected_expiry = u64::try_from(request.expiry)
        .map_err(|_| invalid_request("validated invoice expiry unexpectedly became negative"))?;
    let response =
        authenticated_mutating_request(inner, "create invoice", None, request, |request| {
            rpc.add_invoice(request)
        })
        .await?;
    let record = created_invoice(response.into_inner())?;
    if record.amount() != expected_amount {
        return Err(invalid_response_with_identifier(
            "create invoice",
            "created invoice amount does not match the requested amount",
            Some(record.payment_hash().to_string()),
        ));
    }
    if record.invoice().expiry_time().as_secs() != expected_expiry {
        return Err(invalid_response_with_identifier(
            "create invoice",
            "created BOLT11 expiry does not match the requested expiry",
            Some(record.payment_hash().to_string()),
        ));
    }
    Ok(record)
}

pub(crate) async fn lookup_invoice_with<R: InvoiceRpc>(
    inner: &ClientInner,
    rpc: &mut R,
    payment_hash: sha256::Hash,
) -> Result<InvoiceRecord, LndError> {
    #[allow(deprecated)]
    let request = PaymentHash {
        r_hash_str: String::new(),
        r_hash: payment_hash.to_byte_array().to_vec(),
    };
    let response = authenticated_request(inner, "lookup invoice", request, |request| {
        rpc.lookup_invoice(request)
    })
    .await?;
    let record = convert_invoice(response.into_inner())?;
    if record.payment_hash() != payment_hash {
        return Err(invalid_response_with_identifier(
            "lookup invoice",
            "response payment hash does not match the requested hash",
            Some(payment_hash.to_string()),
        ));
    }
    Ok(record)
}

pub(crate) async fn pay_invoice_with<R: RouterRpc>(
    inner: &ClientInner,
    rpc: &mut R,
    invoice: &Bolt11Invoice,
    options: PaymentOptions,
) -> Result<PaymentRecord, LndError> {
    let expected_hash = *invoice.payment_hash();
    let request = proto_send_payment_request(invoice, options)?;
    let deadline = operation_deadline(inner.timeout)?;
    let response = authenticated_mutating_request_until(
        inner,
        deadline,
        "pay invoice",
        Some(expected_hash.to_string()),
        request,
        |request| rpc.send_payment_v2(request),
    )
    .await?;
    consume_payment_stream(
        response.into_inner(),
        deadline,
        inner.timeout,
        "pay invoice",
        expected_hash,
        invoice.amount_milli_satoshis(),
        true,
    )
    .await
}

pub(crate) async fn lookup_payment_with<R: RouterRpc>(
    inner: &ClientInner,
    rpc: &mut R,
    payment_hash: sha256::Hash,
) -> Result<PaymentRecord, LndError> {
    let deadline = operation_deadline(inner.timeout)?;
    let response = authenticated_request_until(
        inner,
        deadline,
        inner.timeout,
        "lookup payment",
        TrackPaymentRequest {
            payment_hash: payment_hash.to_byte_array().to_vec(),
            no_inflight_updates: true,
        },
        |request| rpc.track_payment_v2(request),
    )
    .await
    .map_err(|error| match error {
        LndError::Timeout { .. } => unknown_payment_outcome("lookup payment", payment_hash),
        other => other,
    })?;
    consume_payment_stream(
        response.into_inner(),
        deadline,
        inner.timeout,
        "lookup payment",
        payment_hash,
        None,
        true,
    )
    .await
}

#[derive(Clone, Eq, PartialEq)]
enum TerminalPayment {
    Succeeded(PaymentRecord),
    Failed {
        payment_hash: sha256::Hash,
        reason: Cow<'static, str>,
    },
}

impl TerminalPayment {
    fn into_result(self) -> Result<PaymentRecord, LndError> {
        match self {
            Self::Succeeded(record) => Ok(record),
            Self::Failed {
                payment_hash,
                reason,
            } => Err(LndError::PaymentFailed {
                payment_hash,
                reason,
            }),
        }
    }
}

async fn consume_payment_stream<S: PaymentStream>(
    mut stream: S,
    deadline: tokio::time::Instant,
    configured_duration: std::time::Duration,
    operation: &'static str,
    expected_hash: sha256::Hash,
    expected_amount: Option<u64>,
    uncertain_from_start: bool,
) -> Result<PaymentRecord, LndError> {
    let mut observed_hash = None;
    let mut terminal: Option<(Vec<u8>, TerminalPayment)> = None;

    loop {
        let next = bounded_request_until(deadline, configured_duration, operation, async {
            stream.message().await.map(Response::new)
        })
        .await;
        let update = match next {
            Ok(response) => response.into_inner(),
            Err(error) => {
                if let Some((_, terminal)) = terminal {
                    return terminal.into_result();
                }
                return match observed_hash.or(uncertain_from_start.then_some(expected_hash)) {
                    Some(hash) => Err(unknown_payment_outcome(operation, hash)),
                    None => Err(error),
                };
            }
        };
        let Some(update) = update else {
            return match terminal {
                Some((_, terminal)) => terminal.into_result(),
                None => Err(invalid_response_with_identifier(
                    operation,
                    "payment stream ended before a terminal state",
                    Some(expected_hash.to_string()),
                )),
            };
        };

        let wire = update.encode_to_vec();
        let failure_reason = update.failure_reason;
        let record = convert_payment(update).map_err(|mut error| {
            if let LndError::InvalidResponse { identifier, .. } = &mut error
                && identifier.is_none()
            {
                *identifier = Some(expected_hash.to_string());
            }
            error
        })?;
        if record.payment_hash() != expected_hash {
            return Err(invalid_response_with_identifier(
                operation,
                "payment update hash does not match the requested hash",
                Some(expected_hash.to_string()),
            ));
        }
        if expected_amount.is_some_and(|amount| record.value().as_u64() != amount) {
            return Err(invalid_response_with_identifier(
                operation,
                "payment value does not match the BOLT11 invoice amount",
                Some(expected_hash.to_string()),
            ));
        }
        observed_hash = Some(record.payment_hash());

        let next_terminal = match record.state() {
            PaymentState::Succeeded => Some(TerminalPayment::Succeeded(record)),
            PaymentState::Failed => Some(TerminalPayment::Failed {
                payment_hash: record.payment_hash(),
                reason: payment_failure_reason(failure_reason),
            }),
            PaymentState::InFlight | PaymentState::Unknown(_) => None,
        };
        match (&terminal, next_terminal) {
            (None, Some(next)) => terminal = Some((wire, next)),
            (Some((first_wire, _)), Some(_)) if first_wire == &wire => {}
            (Some(_), Some(_)) => {
                return Err(invalid_response_with_identifier(
                    operation,
                    "payment stream contained contradictory terminal updates",
                    Some(expected_hash.to_string()),
                ));
            }
            (Some(_), None) => {
                return Err(invalid_response_with_identifier(
                    operation,
                    "payment stream returned to a nonterminal state",
                    Some(expected_hash.to_string()),
                ));
            }
            (None, None) => {}
        }
    }
}

fn proto_invoice_request(request: CreateInvoiceRequest) -> Result<ProtoInvoice, LndError> {
    let value_msat = signed_i64(request.amount().as_u64(), "invoice amount")?;
    let expiry = signed_i64(
        nonzero_whole_seconds(request.expiry(), "invoice expiry")?,
        "invoice expiry",
    )?;
    Ok(ProtoInvoice {
        memo: request.memo().to_owned(),
        value_msat,
        expiry,
        ..Default::default()
    })
}

fn proto_send_payment_request(
    invoice: &Bolt11Invoice,
    options: PaymentOptions,
) -> Result<SendPaymentRequest, LndError> {
    if invoice.amount_milli_satoshis().is_none() {
        return Err(invalid_request(
            "BOLT11 invoice must include an amount because no amount override is available",
        ));
    }
    let fee_limit_msat = signed_i64(options.fee_limit().as_u64(), "payment fee limit")?;
    let timeout_seconds =
        i32::try_from(nonzero_whole_seconds(options.timeout(), "payment timeout")?)
            .map_err(|_| invalid_request("payment timeout exceeds LND's integer range"))?;
    Ok(SendPaymentRequest {
        payment_request: invoice.to_string(),
        fee_limit_msat,
        timeout_seconds,
        no_inflight_updates: false,
        ..Default::default()
    })
}

fn signed_i64(value: u64, field: &'static str) -> Result<i64, LndError> {
    i64::try_from(value).map_err(|_| {
        invalid_request(match field {
            "invoice amount" => "invoice amount exceeds LND's signed integer range",
            "invoice expiry" => "invoice expiry exceeds LND's signed integer range",
            "payment fee limit" => "payment fee limit exceeds LND's signed integer range",
            _ => "request value exceeds LND's signed integer range",
        })
    })
}

fn nonzero_whole_seconds(
    duration: std::time::Duration,
    field: &'static str,
) -> Result<u64, LndError> {
    if duration.subsec_nanos() != 0 {
        return Err(invalid_request(match field {
            "invoice expiry" => "invoice expiry must use whole seconds",
            "payment timeout" => "payment timeout must use whole seconds",
            _ => "duration must use whole seconds",
        }));
    }
    let seconds = duration.as_secs();
    if seconds == 0 {
        return Err(invalid_request(match field {
            "invoice expiry" => "invoice expiry must include at least one whole second",
            "payment timeout" => "payment timeout must include at least one whole second",
            _ => "duration must include at least one whole second",
        }));
    }
    Ok(seconds)
}

fn payment_failure_reason(value: i32) -> Cow<'static, str> {
    let reason = match PaymentFailureReason::try_from(value) {
        Ok(PaymentFailureReason::FailureReasonNone) => Cow::Borrowed("failure reason not reported"),
        Ok(PaymentFailureReason::FailureReasonTimeout) => Cow::Borrowed("payment timeout"),
        Ok(PaymentFailureReason::FailureReasonNoRoute) => Cow::Borrowed("no route"),
        Ok(PaymentFailureReason::FailureReasonError) => Cow::Borrowed("payment error"),
        Ok(PaymentFailureReason::FailureReasonIncorrectPaymentDetails) => {
            Cow::Borrowed("incorrect payment details")
        }
        Ok(PaymentFailureReason::FailureReasonInsufficientBalance) => {
            Cow::Borrowed("insufficient balance")
        }
        Ok(PaymentFailureReason::FailureReasonCanceled) => Cow::Borrowed("payment canceled"),
        Err(_) => Cow::Owned(format!("unknown failure reason {value}")),
    };
    bounded(reason)
}

fn invalid_request(detail: &'static str) -> LndError {
    LndError::InvalidRequest {
        detail: Cow::Borrowed(detail),
    }
}

fn unknown_payment_outcome(operation: &'static str, payment_hash: sha256::Hash) -> LndError {
    LndError::OutcomeUnknown {
        operation: Cow::Borrowed(operation),
        identifier: Some(payment_hash.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]
    use std::{collections::VecDeque, future::Future, time::Duration};

    use bitcoin::{
        hashes::{Hash, sha256},
        secp256k1::{Secp256k1, SecretKey},
    };
    use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder, PaymentSecret};
    use tonic::{Request, Response, Status, transport::Endpoint};

    use crate::{
        CreateInvoiceRequest, InvoiceState, LndClient, LndConfig, LndError, Millisats,
        PaymentOptions, PaymentState,
        convert::{created_invoice, invoice as convert_invoice, payment as convert_payment},
        proto::{
            lnrpc::{
                AddInvoiceResponse, Invoice as ProtoInvoice, Payment, PaymentFailureReason,
                PaymentHash, payment,
            },
            routerrpc::{SendPaymentRequest, TrackPaymentRequest},
        },
    };

    use super::{
        InvoiceRpc, PaymentStream, RouterRpc, create_invoice_with, lookup_invoice_with,
        lookup_payment_with, pay_invoice_with, proto_invoice_request,
    };

    const PREIMAGE_BYTES: [u8; 32] = [9; 32];

    enum StreamItem {
        Ready(Result<Option<Payment>, Status>),
        Delayed(Duration, Result<Option<Payment>, Status>),
    }

    struct FakePaymentStream(VecDeque<StreamItem>);

    impl PaymentStream for FakePaymentStream {
        fn message(&mut self) -> impl Future<Output = Result<Option<Payment>, Status>> + Send {
            let item = self.0.pop_front().unwrap_or(StreamItem::Ready(Ok(None)));
            async move {
                match item {
                    StreamItem::Ready(result) => result,
                    StreamItem::Delayed(delay, result) => {
                        tokio::time::sleep(delay).await;
                        result
                    }
                }
            }
        }
    }

    #[derive(Default)]
    struct FakeInvoiceRpc {
        add_response: Option<Result<AddInvoiceResponse, Status>>,
        lookup_response: Option<Result<ProtoInvoice, Status>>,
        add_delay: Duration,
        add_request: Option<ProtoInvoice>,
        lookup_request: Option<PaymentHash>,
    }

    impl InvoiceRpc for FakeInvoiceRpc {
        fn add_invoice(
            &mut self,
            request: Request<ProtoInvoice>,
        ) -> impl Future<Output = Result<Response<AddInvoiceResponse>, Status>> + Send {
            self.add_request = Some(request.into_inner());
            let response = self.add_response.take().unwrap();
            let delay = self.add_delay;
            async move {
                tokio::time::sleep(delay).await;
                response.map(Response::new)
            }
        }

        fn lookup_invoice(
            &mut self,
            request: Request<PaymentHash>,
        ) -> impl Future<Output = Result<Response<ProtoInvoice>, Status>> + Send {
            self.lookup_request = Some(request.into_inner());
            let response = self.lookup_response.take().unwrap();
            async move { response.map(Response::new) }
        }
    }

    #[derive(Default)]
    struct FakeRouterRpc {
        send_response: Option<Result<FakePaymentStream, Status>>,
        track_response: Option<Result<FakePaymentStream, Status>>,
        send_delay: Duration,
        track_delay: Duration,
        send_request: Option<SendPaymentRequest>,
        track_request: Option<TrackPaymentRequest>,
    }

    impl RouterRpc for FakeRouterRpc {
        type PaymentStream = FakePaymentStream;

        fn send_payment_v2(
            &mut self,
            request: Request<SendPaymentRequest>,
        ) -> impl Future<Output = Result<Response<Self::PaymentStream>, Status>> + Send {
            self.send_request = Some(request.into_inner());
            let response = self.send_response.take().unwrap();
            let delay = self.send_delay;
            async move {
                tokio::time::sleep(delay).await;
                response.map(Response::new)
            }
        }

        fn track_payment_v2(
            &mut self,
            request: Request<TrackPaymentRequest>,
        ) -> impl Future<Output = Result<Response<Self::PaymentStream>, Status>> + Send {
            self.track_request = Some(request.into_inner());
            let response = self.track_response.take().unwrap();
            let delay = self.track_delay;
            async move {
                tokio::time::sleep(delay).await;
                response.map(Response::new)
            }
        }
    }

    fn client(timeout: Duration) -> LndClient {
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem()
            .into_bytes();
        LndClient::with_config(
            LndConfig::new("https://localhost:10009", certificate, vec![1], timeout).unwrap(),
        )
        .unwrap()
    }

    fn invoice() -> Bolt11Invoice {
        invoice_with(payment_hash(), Some(25_000), 1_700_000_000, 60)
    }

    fn invoice_with(
        payment_hash: sha256::Hash,
        amount_msat: Option<u64>,
        timestamp: u64,
        expiry: u64,
    ) -> Bolt11Invoice {
        let secret_key = SecretKey::from_slice(&[42; 32]).unwrap();
        let builder = InvoiceBuilder::new(Currency::Regtest)
            .description("nigiri-rs payment test".into())
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([21; 32]))
            .duration_since_epoch(Duration::from_secs(timestamp))
            .expiry_time(Duration::from_secs(expiry))
            .min_final_cltv_expiry_delta(18);
        let builder = match amount_msat {
            Some(amount) => builder.amount_milli_satoshis(amount),
            None => builder,
        };
        builder
            .build_signed(|message| Secp256k1::new().sign_ecdsa_recoverable(message, &secret_key))
            .unwrap()
    }

    fn payment_hash() -> sha256::Hash {
        sha256::Hash::hash(&PREIMAGE_BYTES)
    }

    fn proto_invoice(invoice: &Bolt11Invoice, value_msat: i64, state: i32) -> ProtoInvoice {
        ProtoInvoice {
            r_hash: invoice.payment_hash().to_byte_array().to_vec(),
            payment_request: invoice.to_string(),
            value_msat,
            creation_date: i64::try_from(invoice.duration_since_epoch().as_secs()).unwrap(),
            expiry: i64::try_from(invoice.expiry_time().as_secs()).unwrap(),
            state,
            ..Default::default()
        }
    }

    fn payment_update(status: payment::PaymentStatus) -> Payment {
        Payment {
            payment_hash: hex(&payment_hash().to_byte_array()),
            payment_preimage: if status == payment::PaymentStatus::Succeeded {
                hex(&PREIMAGE_BYTES)
            } else {
                String::new()
            },
            value_msat: 25_000,
            fee_msat: 1_250,
            status: status as i32,
            ..Default::default()
        }
    }

    fn stream(items: impl IntoIterator<Item = StreamItem>) -> FakePaymentStream {
        FakePaymentStream(items.into_iter().collect())
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn connection_loss_status() -> Status {
        let error = Endpoint::from_shared("https://[".to_owned())
            .expect_err("the malformed URI must produce a transport error");
        Status::from_error(Box::new(error))
    }

    #[tokio::test]
    async fn inflight_then_succeeded_returns_terminal_value_and_fee() {
        let invoice = invoice();
        let mut rpc = FakeRouterRpc {
            send_response: Some(Ok(stream([
                StreamItem::Ready(Ok(Some(payment_update(payment::PaymentStatus::InFlight)))),
                StreamItem::Ready(Ok(Some(payment_update(payment::PaymentStatus::Succeeded)))),
                StreamItem::Ready(Ok(None)),
            ]))),
            ..Default::default()
        };
        let options = PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap();

        let paid = pay_invoice_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            &invoice,
            options,
        )
        .await
        .unwrap();

        assert_eq!(paid.state(), PaymentState::Succeeded);
        assert_eq!(paid.payment_hash(), *invoice.payment_hash());
        assert_eq!(paid.preimage(), Some(PREIMAGE_BYTES));
        assert_eq!(paid.value(), Millisats::new(25_000));
        assert_eq!(paid.fee(), Millisats::new(1_250));
        let request = rpc.send_request.unwrap();
        assert_eq!(request.payment_request, invoice.to_string());
        assert_eq!(request.fee_limit_msat, 10_000);
        assert_eq!(request.timeout_seconds, 5);
        assert!(!request.no_inflight_updates);
        assert!(!request.cancelable);
        assert_eq!(request.amt_msat, 0);
        assert!(request.payment_hash.is_empty());
    }

    #[tokio::test]
    async fn immediate_failed_returns_bounded_reason_and_payment_hash() {
        let mut failed = payment_update(payment::PaymentStatus::Failed);
        failed.failure_reason = PaymentFailureReason::FailureReasonNoRoute as i32;
        let mut rpc = FakeRouterRpc {
            send_response: Some(Ok(stream([
                StreamItem::Ready(Ok(Some(failed))),
                StreamItem::Ready(Ok(None)),
            ]))),
            ..Default::default()
        };

        let error = pay_invoice_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            &invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::PaymentFailed { payment_hash: actual, reason }
                if actual == payment_hash() && reason == "no route"
        ));
    }

    #[tokio::test]
    async fn stream_end_while_inflight_is_invalid_and_preserves_hash_identifier() {
        let mut rpc = FakeRouterRpc {
            track_response: Some(Ok(stream([
                StreamItem::Ready(Ok(Some(payment_update(payment::PaymentStatus::InFlight)))),
                StreamItem::Ready(Ok(None)),
            ]))),
            ..Default::default()
        };

        let error = lookup_payment_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            payment_hash(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::InvalidResponse { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
        assert_eq!(
            rpc.track_request,
            Some(TrackPaymentRequest {
                payment_hash: payment_hash().to_byte_array().to_vec(),
                no_inflight_updates: true,
            })
        );
    }

    #[test]
    fn malformed_31_byte_hash_and_terminal_preimage_are_invalid() {
        assert!(matches!(
            created_invoice(AddInvoiceResponse {
                r_hash: payment_hash().to_byte_array()[..31].to_vec(),
                payment_request: invoice().to_string(),
                ..Default::default()
            }),
            Err(LndError::InvalidResponse { .. })
        ));

        let mut malformed_hash = payment_update(payment::PaymentStatus::InFlight);
        malformed_hash.payment_hash = hex(&payment_hash().to_byte_array()[..31]);
        assert!(matches!(
            convert_payment(malformed_hash),
            Err(LndError::InvalidResponse { .. })
        ));

        let mut malformed_preimage = payment_update(payment::PaymentStatus::Succeeded);
        malformed_preimage.payment_preimage = hex(&[9; 31]);
        assert!(matches!(
            convert_payment(malformed_preimage),
            Err(LndError::InvalidResponse { .. })
        ));
    }

    #[test]
    fn succeeded_payment_rejects_a_preimage_that_does_not_prove_its_hash() {
        let mut mismatched = payment_update(payment::PaymentStatus::Succeeded);
        mismatched.payment_hash = hex(&sha256::Hash::hash(&[8; 32]).to_byte_array());

        let error = convert_payment(mismatched).unwrap_err();

        assert!(matches!(
            error,
            LndError::InvalidResponse { identifier: Some(identifier), .. }
                if identifier == sha256::Hash::hash(&[8; 32]).to_string()
        ));
    }

    #[tokio::test]
    async fn identical_duplicate_terminal_updates_are_accepted() {
        let terminal = payment_update(payment::PaymentStatus::Succeeded);
        let mut rpc = FakeRouterRpc {
            track_response: Some(Ok(stream([
                StreamItem::Ready(Ok(Some(terminal.clone()))),
                StreamItem::Ready(Ok(Some(terminal))),
                StreamItem::Ready(Ok(None)),
            ]))),
            ..Default::default()
        };

        let paid = lookup_payment_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            payment_hash(),
        )
        .await
        .unwrap();

        assert_eq!(paid.state(), PaymentState::Succeeded);
    }

    #[tokio::test]
    async fn contradictory_duplicate_terminal_update_is_invalid() {
        let first = payment_update(payment::PaymentStatus::Succeeded);
        let mut second = first.clone();
        second.fee_msat += 1;
        let mut rpc = FakeRouterRpc {
            track_response: Some(Ok(stream([
                StreamItem::Ready(Ok(Some(first))),
                StreamItem::Ready(Ok(Some(second))),
                StreamItem::Ready(Ok(None)),
            ]))),
            ..Default::default()
        };

        let error = lookup_payment_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            payment_hash(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[tokio::test]
    async fn timeout_after_hash_discovery_is_outcome_unknown_with_hash() {
        let mut rpc = FakeRouterRpc {
            track_response: Some(Ok(stream([
                StreamItem::Ready(Ok(Some(payment_update(payment::PaymentStatus::InFlight)))),
                StreamItem::Delayed(Duration::from_secs(1), Ok(None)),
            ]))),
            ..Default::default()
        };

        let error = lookup_payment_with(
            &client(Duration::from_millis(5)).inner,
            &mut rpc,
            payment_hash(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn lookup_request_timeout_preserves_the_requested_hash() {
        let mut rpc = FakeRouterRpc {
            track_response: Some(Ok(stream([]))),
            track_delay: Duration::from_secs(1),
            ..Default::default()
        };

        let error = lookup_payment_with(
            &client(Duration::from_millis(5)).inner,
            &mut rpc,
            payment_hash(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn lookup_timeout_before_first_terminal_update_preserves_requested_hash() {
        let mut rpc = FakeRouterRpc {
            track_response: Some(Ok(stream([StreamItem::Delayed(
                Duration::from_secs(1),
                Ok(None),
            )]))),
            ..Default::default()
        };

        let error = lookup_payment_with(
            &client(Duration::from_millis(5)).inner,
            &mut rpc,
            payment_hash(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
        assert!(rpc.track_request.unwrap().no_inflight_updates);
    }

    #[tokio::test]
    async fn lookup_stream_status_before_first_update_preserves_requested_hash() {
        let mut rpc = FakeRouterRpc {
            track_response: Some(Ok(stream([StreamItem::Ready(Err(Status::unavailable(
                "stream lost",
            )))]))),
            ..Default::default()
        };

        let error = lookup_payment_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            payment_hash(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn lookup_setup_not_found_remains_a_definitive_status() {
        let mut rpc = FakeRouterRpc {
            track_response: Some(Err(Status::not_found("payment not initiated"))),
            ..Default::default()
        };

        let error = lookup_payment_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            payment_hash(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, LndError::Status { .. }));
    }

    #[tokio::test]
    async fn pay_request_timeout_preserves_the_invoice_hash_as_outcome_unknown() {
        let mut rpc = FakeRouterRpc {
            send_response: Some(Ok(stream([]))),
            send_delay: Duration::from_secs(1),
            ..Default::default()
        };

        let error = pay_invoice_with(
            &client(Duration::from_millis(5)).inner,
            &mut rpc,
            &invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn pay_stream_status_before_first_update_preserves_the_invoice_hash() {
        let mut rpc = FakeRouterRpc {
            send_response: Some(Ok(stream([StreamItem::Ready(Err(Status::unavailable(
                "stream lost",
            )))]))),
            ..Default::default()
        };

        let error = pay_invoice_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            &invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn status_after_hash_discovery_is_outcome_unknown_with_hash() {
        let mut rpc = FakeRouterRpc {
            track_response: Some(Ok(stream([
                StreamItem::Ready(Ok(Some(payment_update(payment::PaymentStatus::InFlight)))),
                StreamItem::Ready(Err(Status::unavailable("stream lost"))),
            ]))),
            ..Default::default()
        };

        let error = lookup_payment_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            payment_hash(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn request_and_stream_share_one_absolute_deadline() {
        let mut rpc = FakeRouterRpc {
            send_response: Some(Ok(stream([
                StreamItem::Delayed(
                    Duration::from_millis(40),
                    Ok(Some(payment_update(payment::PaymentStatus::InFlight))),
                ),
                StreamItem::Delayed(
                    Duration::from_millis(40),
                    Ok(Some(payment_update(payment::PaymentStatus::Succeeded))),
                ),
            ]))),
            send_delay: Duration::from_millis(40),
            ..Default::default()
        };

        let error = pay_invoice_with(
            &client(Duration::from_millis(100)).inner,
            &mut rpc,
            &invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[test]
    fn invoice_conversions_validate_hash_amount_and_preserve_unknown_state() {
        let invoice = invoice();
        let created = created_invoice(AddInvoiceResponse {
            r_hash: payment_hash().to_byte_array().to_vec(),
            payment_request: invoice.to_string(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(created.payment_hash(), payment_hash());
        assert_eq!(created.amount(), Millisats::new(25_000));
        assert_eq!(created.state(), InvoiceState::Open);

        let looked_up = convert_invoice(proto_invoice(&invoice, 25_000, 91)).unwrap();
        assert_eq!(looked_up.state(), InvoiceState::Unknown(91));

        let error = convert_invoice(proto_invoice(&invoice, -1, 0)).unwrap_err();
        assert!(matches!(
            error,
            LndError::InvalidResponse { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));

        for (raw, expected) in [
            (0, InvoiceState::Open),
            (1, InvoiceState::Settled),
            (2, InvoiceState::Canceled),
            (3, InvoiceState::Accepted),
        ] {
            let converted = convert_invoice(proto_invoice(&invoice, 25_000, raw)).unwrap();
            assert_eq!(converted.state(), expected);
        }
    }

    #[test]
    fn lookup_invoice_rejects_negative_or_mismatched_time_metadata() {
        let invoice = invoice();
        let mut cases = Vec::new();

        let mut negative_creation = proto_invoice(&invoice, 25_000, 0);
        negative_creation.creation_date = -1;
        cases.push(negative_creation);

        let mut negative_expiry = proto_invoice(&invoice, 25_000, 0);
        negative_expiry.expiry = -1;
        cases.push(negative_expiry);

        let mut mismatched_creation = proto_invoice(&invoice, 25_000, 0);
        mismatched_creation.creation_date += 1;
        cases.push(mismatched_creation);

        let mut mismatched_expiry = proto_invoice(&invoice, 25_000, 0);
        mismatched_expiry.expiry += 1;
        cases.push(mismatched_expiry);

        let mut maximum_creation = proto_invoice(&invoice, 25_000, 0);
        maximum_creation.creation_date = i64::MAX;
        cases.push(maximum_creation);

        let mut maximum_expiry = proto_invoice(&invoice, 25_000, 0);
        maximum_expiry.expiry = i64::MAX;
        cases.push(maximum_expiry);

        for response in cases {
            let error = convert_invoice(response).unwrap_err();
            assert!(matches!(
                error,
                LndError::InvalidResponse { identifier: Some(identifier), .. }
                    if identifier == payment_hash().to_string()
            ));
        }
    }

    #[test]
    fn payment_conversion_maps_known_and_unknown_states_and_rejects_negative_amounts() {
        let initiated = convert_payment(payment_update(payment::PaymentStatus::Initiated)).unwrap();
        assert_eq!(initiated.state(), PaymentState::InFlight);

        let mut unknown = payment_update(payment::PaymentStatus::InFlight);
        unknown.status = 91;
        assert_eq!(
            convert_payment(unknown).unwrap().state(),
            PaymentState::Unknown(91)
        );

        for (value_msat, fee_msat) in [(-1, 0), (0, -1)] {
            let mut invalid = payment_update(payment::PaymentStatus::InFlight);
            invalid.value_msat = value_msat;
            invalid.fee_msat = fee_msat;
            assert!(matches!(
                convert_payment(invalid),
                Err(LndError::InvalidResponse { .. })
            ));
        }

        for (raw, expected) in [
            (0, PaymentState::Unknown(0)),
            (1, PaymentState::InFlight),
            (2, PaymentState::Succeeded),
            (3, PaymentState::Failed),
            (4, PaymentState::InFlight),
        ] {
            let mut update = payment_update(payment::PaymentStatus::InFlight);
            update.status = raw;
            if expected == PaymentState::Succeeded {
                update.payment_preimage = hex(&PREIMAGE_BYTES);
            }
            assert_eq!(convert_payment(update).unwrap().state(), expected);
        }

        let mut missing_preimage = payment_update(payment::PaymentStatus::Succeeded);
        missing_preimage.payment_preimage.clear();
        assert!(matches!(
            convert_payment(missing_preimage),
            Err(LndError::InvalidResponse { .. })
        ));
    }

    #[tokio::test]
    async fn invoice_operations_emit_exact_requests_and_convert_responses() {
        let invoice = invoice();
        let mut rpc = FakeInvoiceRpc {
            add_response: Some(Ok(AddInvoiceResponse {
                r_hash: payment_hash().to_byte_array().to_vec(),
                payment_request: invoice.to_string(),
                ..Default::default()
            })),
            lookup_response: Some(Ok(proto_invoice(&invoice, 25_000, 1))),
            ..Default::default()
        };
        let client = client(Duration::from_secs(1));
        let request = CreateInvoiceRequest::new(
            Millisats::new(25_000),
            "nigiri-rs readiness probe",
            Duration::from_secs(60),
        )
        .unwrap();

        let created = create_invoice_with(&client.inner, &mut rpc, request)
            .await
            .unwrap();
        let looked_up = lookup_invoice_with(&client.inner, &mut rpc, payment_hash())
            .await
            .unwrap();

        assert_eq!(created.payment_hash(), payment_hash());
        assert_eq!(looked_up.state(), InvoiceState::Settled);
        let add = rpc.add_request.unwrap();
        assert_eq!(add.value_msat, 25_000);
        assert_eq!(add.memo, "nigiri-rs readiness probe");
        assert_eq!(add.expiry, 60);
        assert_eq!(add.value, 0);
        assert_eq!(
            rpc.lookup_request,
            Some(PaymentHash {
                r_hash_str: String::new(),
                r_hash: payment_hash().to_byte_array().to_vec(),
            })
        );
    }

    #[tokio::test]
    async fn create_invoice_timeout_is_an_uncertain_committed_operation() {
        let mut rpc = FakeInvoiceRpc {
            add_response: Some(Ok(AddInvoiceResponse::default())),
            add_delay: Duration::from_secs(1),
            ..Default::default()
        };
        let request =
            CreateInvoiceRequest::new(Millisats::new(25_000), "memo", Duration::from_secs(60))
                .unwrap();

        let error = create_invoice_with(&client(Duration::from_millis(5)).inner, &mut rpc, request)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown {
                identifier: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn create_invoice_connection_loss_is_outcome_unknown_without_identifier() {
        let mut rpc = FakeInvoiceRpc {
            add_response: Some(Err(connection_loss_status())),
            ..Default::default()
        };
        let request =
            CreateInvoiceRequest::new(Millisats::new(25_000), "memo", Duration::from_secs(60))
                .unwrap();

        let error = create_invoice_with(&client(Duration::from_secs(1)).inner, &mut rpc, request)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown {
                identifier: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn create_invoice_ambiguous_status_is_outcome_unknown_without_identifier() {
        let mut rpc = FakeInvoiceRpc {
            add_response: Some(Err(Status::deadline_exceeded("commit not observable"))),
            ..Default::default()
        };
        let request =
            CreateInvoiceRequest::new(Millisats::new(25_000), "memo", Duration::from_secs(60))
                .unwrap();

        let error = create_invoice_with(&client(Duration::from_secs(1)).inner, &mut rpc, request)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown {
                identifier: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn create_invoice_definitive_validation_status_remains_typed() {
        let mut rpc = FakeInvoiceRpc {
            add_response: Some(Err(Status::invalid_argument("invalid invoice"))),
            ..Default::default()
        };
        let request =
            CreateInvoiceRequest::new(Millisats::new(25_000), "memo", Duration::from_secs(60))
                .unwrap();

        let error = create_invoice_with(&client(Duration::from_secs(1)).inner, &mut rpc, request)
            .await
            .unwrap_err();

        assert!(matches!(error, LndError::Status { .. }));
    }

    #[tokio::test]
    async fn pay_invoice_connection_loss_is_outcome_unknown_with_invoice_hash() {
        let mut rpc = FakeRouterRpc {
            send_response: Some(Err(connection_loss_status())),
            ..Default::default()
        };

        let error = pay_invoice_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            &invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn pay_invoice_ambiguous_status_is_outcome_unknown_with_invoice_hash() {
        let mut rpc = FakeRouterRpc {
            send_response: Some(Err(Status::deadline_exceeded("commit not observable"))),
            ..Default::default()
        };

        let error = pay_invoice_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            &invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::OutcomeUnknown { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn pay_invoice_definitive_authentication_status_remains_typed() {
        let mut rpc = FakeRouterRpc {
            send_response: Some(Err(Status::unauthenticated("bad macaroon"))),
            ..Default::default()
        };

        let error = pay_invoice_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            &invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, LndError::Authentication { .. }));
    }

    #[tokio::test]
    async fn create_invoice_rejects_a_response_with_a_different_amount() {
        let invoice = invoice();
        let mut rpc = FakeInvoiceRpc {
            add_response: Some(Ok(AddInvoiceResponse {
                r_hash: payment_hash().to_byte_array().to_vec(),
                payment_request: invoice.to_string(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let request =
            CreateInvoiceRequest::new(Millisats::new(24_000), "memo", Duration::from_secs(60))
                .unwrap();

        let error = create_invoice_with(&client(Duration::from_secs(1)).inner, &mut rpc, request)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            LndError::InvalidResponse { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn create_invoice_rejects_a_response_with_a_different_expiry() {
        let response_invoice = invoice_with(payment_hash(), Some(25_000), 1_700_000_000, 61);
        let mut rpc = FakeInvoiceRpc {
            add_response: Some(Ok(AddInvoiceResponse {
                r_hash: payment_hash().to_byte_array().to_vec(),
                payment_request: response_invoice.to_string(),
                ..Default::default()
            })),
            ..Default::default()
        };
        let request =
            CreateInvoiceRequest::new(Millisats::new(25_000), "memo", Duration::from_secs(60))
                .unwrap();

        let error = create_invoice_with(&client(Duration::from_secs(1)).inner, &mut rpc, request)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            LndError::InvalidResponse { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[tokio::test]
    async fn fractional_or_overflowing_invoice_expiry_is_rejected_before_rpc() {
        for expiry in [
            Duration::from_millis(1_500),
            Duration::from_secs(i64::MAX as u64 + 1),
        ] {
            let mut rpc = FakeInvoiceRpc::default();
            let request =
                CreateInvoiceRequest::new(Millisats::new(25_000), "memo", expiry).unwrap();

            let error =
                create_invoice_with(&client(Duration::from_secs(1)).inner, &mut rpc, request)
                    .await
                    .unwrap_err();

            assert!(matches!(error, LndError::InvalidRequest { .. }));
            assert!(rpc.add_request.is_none());
        }
    }

    #[test]
    fn maximum_signed_invoice_expiry_is_preserved_without_truncation() {
        let request = CreateInvoiceRequest::new(
            Millisats::new(25_000),
            "memo",
            Duration::from_secs(i64::MAX as u64),
        )
        .unwrap();

        let proto = proto_invoice_request(request).unwrap();

        assert_eq!(proto.expiry, i64::MAX);
    }

    #[tokio::test]
    async fn amountless_invoice_is_rejected_before_send_payment_rpc() {
        let amountless = invoice_with(payment_hash(), None, 1_700_000_000, 60);
        let mut rpc = FakeRouterRpc {
            send_response: Some(Ok(stream([]))),
            ..Default::default()
        };

        let error = pay_invoice_with(
            &client(Duration::from_secs(1)).inner,
            &mut rpc,
            &amountless,
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidRequest { .. }));
        assert!(rpc.send_request.is_none());
    }

    #[test]
    fn malformed_created_invoice_preserves_a_valid_hash_identifier() {
        let error = created_invoice(AddInvoiceResponse {
            r_hash: payment_hash().to_byte_array().to_vec(),
            payment_request: "not-a-bolt11-invoice".into(),
            ..Default::default()
        })
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::InvalidResponse { identifier: Some(identifier), .. }
                if identifier == payment_hash().to_string()
        ));
    }

    #[test]
    fn created_invoice_rejects_a_payment_request_with_a_different_hash() {
        let error = created_invoice(AddInvoiceResponse {
            r_hash: [8; 32].to_vec(),
            payment_request: invoice().to_string(),
            ..Default::default()
        })
        .unwrap_err();

        assert!(matches!(
            error,
            LndError::InvalidResponse { identifier: Some(identifier), .. }
                if identifier == sha256::Hash::from_byte_array([8; 32]).to_string()
        ));
    }

    #[test]
    fn invoice_record_debug_omits_the_payment_request() {
        let payment_request = invoice().to_string();
        let record = created_invoice(AddInvoiceResponse {
            r_hash: payment_hash().to_byte_array().to_vec(),
            payment_request: payment_request.clone(),
            ..Default::default()
        })
        .unwrap();

        let debug = format!("{record:?}");

        assert!(!debug.contains(&payment_request));
        assert!(!debug.contains("PaymentSecret"));
        assert!(debug.contains(&payment_hash().to_string()));
    }
}
