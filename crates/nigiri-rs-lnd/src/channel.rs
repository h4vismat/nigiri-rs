use std::{borrow::Cow, future::Future};

use bitcoin::OutPoint;
use tonic::{Request, Response, Status, transport::Channel as TonicChannel};

use crate::{
    Channel, LndClient, LndError, OpenChannelRequest,
    convert::{channel, channel_point, invalid_response},
    proto::lnrpc::{
        ChannelPoint, ListChannelsRequest, ListChannelsResponse,
        OpenChannelRequest as ProtoOpenChannelRequest, OpenStatusUpdate,
        channel_point as proto_channel_point, lightning_client::LightningClient,
        open_status_update,
    },
    transport::{ClientInner, authenticated_request, bounded_request_until},
};

pub(crate) trait OpenStatusStream: Send {
    fn message(&mut self) -> impl Future<Output = Result<Option<OpenStatusUpdate>, Status>> + Send;
}

impl OpenStatusStream for tonic::Streaming<OpenStatusUpdate> {
    fn message(&mut self) -> impl Future<Output = Result<Option<OpenStatusUpdate>, Status>> + Send {
        tonic::Streaming::message(self)
    }
}

pub(crate) trait ChannelRpc: Send {
    type OpenStatusStream: OpenStatusStream;

    fn open_channel(
        &mut self,
        request: Request<ProtoOpenChannelRequest>,
    ) -> impl Future<Output = Result<Response<Self::OpenStatusStream>, Status>> + Send;
    fn list_channels(
        &mut self,
        request: Request<ListChannelsRequest>,
    ) -> impl Future<Output = Result<Response<ListChannelsResponse>, Status>> + Send;
}

impl ChannelRpc for LightningClient<TonicChannel> {
    type OpenStatusStream = tonic::Streaming<OpenStatusUpdate>;

    fn open_channel(
        &mut self,
        request: Request<ProtoOpenChannelRequest>,
    ) -> impl Future<Output = Result<Response<Self::OpenStatusStream>, Status>> + Send {
        LightningClient::open_channel(self, request)
    }

    fn list_channels(
        &mut self,
        request: Request<ListChannelsRequest>,
    ) -> impl Future<Output = Result<Response<ListChannelsResponse>, Status>> + Send {
        LightningClient::list_channels(self, request)
    }
}

impl LndClient {
    /// Opens a channel and waits until LND reports its confirmed funding outpoint.
    pub async fn open_channel(&self, request: OpenChannelRequest) -> Result<OutPoint, LndError> {
        let mut rpc = LightningClient::new(self.inner.channel().await);
        open_channel_with(&self.inner, &mut rpc, request).await
    }

    /// Lists the node's open channels.
    pub async fn list_channels(&self) -> Result<Vec<Channel>, LndError> {
        let mut rpc = LightningClient::new(self.inner.channel().await);
        list_channels_with(&self.inner, &mut rpc).await
    }
}

pub(crate) async fn open_channel_with<R: ChannelRpc>(
    inner: &ClientInner,
    rpc: &mut R,
    request: OpenChannelRequest,
) -> Result<OutPoint, LndError> {
    let request = proto_open_channel_request(request)?;
    let deadline = tokio::time::Instant::now() + inner.timeout;
    let response = authenticated_request(inner, "open channel", request, |request| {
        rpc.open_channel(request)
    })
    .await?;
    let mut stream = response.into_inner();
    let mut pending_point = None;

    loop {
        let next = bounded_request_until(deadline, inner.timeout, "open channel", async {
            stream.message().await.map(Response::new)
        })
        .await;
        let update = match next {
            Ok(response) => response.into_inner(),
            Err(error) => {
                return match pending_point {
                    Some(point) => Err(unknown_open_outcome(point)),
                    None => Err(error),
                };
            }
        };
        let Some(update) = update else {
            return Err(invalid_open_response(
                "channel stream ended before the channel opened",
                pending_point,
            ));
        };
        match update.update {
            Some(open_status_update::Update::ChanPending(pending)) => {
                let point = channel_point(ChannelPoint {
                    output_index: pending.output_index,
                    funding_txid: Some(proto_channel_point::FundingTxid::FundingTxidBytes(
                        pending.txid,
                    )),
                })?;
                if pending_point.is_some_and(|existing| existing != point) {
                    return Err(invalid_open_response(
                        "pending channel point changed during channel opening",
                        pending_point,
                    ));
                }
                pending_point = Some(point);
            }
            Some(open_status_update::Update::ChanOpen(opened)) => {
                let point = opened
                    .channel_point
                    .ok_or_else(|| {
                        invalid_open_response("opened channel point is missing", pending_point)
                    })
                    .and_then(channel_point)?;
                if pending_point.is_some_and(|pending| pending != point) {
                    return Err(invalid_open_response(
                        "pending and opened channel points do not match",
                        pending_point,
                    ));
                }
                return Ok(point);
            }
            Some(open_status_update::Update::PsbtFund(_)) => {
                return Err(invalid_open_response(
                    "unexpected PSBT funding state for a wallet-funded channel",
                    pending_point,
                ));
            }
            None => {
                return Err(invalid_open_response(
                    "channel update has no state",
                    pending_point,
                ));
            }
        }
    }
}

pub(crate) async fn list_channels_with<R: ChannelRpc>(
    inner: &ClientInner,
    rpc: &mut R,
) -> Result<Vec<Channel>, LndError> {
    let response = authenticated_request(
        inner,
        "list channels",
        ListChannelsRequest::default(),
        |request| rpc.list_channels(request),
    )
    .await?
    .into_inner();
    response.channels.into_iter().map(channel).collect()
}

fn proto_open_channel_request(
    request: OpenChannelRequest,
) -> Result<ProtoOpenChannelRequest, LndError> {
    let local_funding_amount =
        i64::try_from(request.capacity().as_u64()).map_err(|_| LndError::InvalidRequest {
            detail: Cow::Borrowed("channel capacity exceeds LND's signed amount range"),
        })?;
    let push_sat =
        i64::try_from(request.push_amount().as_u64()).map_err(|_| LndError::InvalidRequest {
            detail: Cow::Borrowed("channel push amount exceeds LND's signed amount range"),
        })?;
    Ok(ProtoOpenChannelRequest {
        node_pubkey: request.peer_public_key().serialize().to_vec(),
        local_funding_amount,
        push_sat,
        private: false,
        ..Default::default()
    })
}

fn invalid_open_response(detail: &'static str, point: Option<OutPoint>) -> LndError {
    let mut error = invalid_response("open channel", detail);
    if let LndError::InvalidResponse { identifier, .. } = &mut error {
        *identifier = point.map(|point| point.to_string());
    }
    error
}

fn unknown_open_outcome(point: OutPoint) -> LndError {
    LndError::OutcomeUnknown {
        operation: Cow::Borrowed("open channel"),
        identifier: Some(point.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, future::Future, time::Duration};

    use bitcoin::{OutPoint, Txid, hashes::Hash, secp256k1::PublicKey};
    use tonic::{Request, Response, Status};

    use crate::{
        LndClient, LndConfig, LndError, OpenChannelRequest, Sats,
        convert::channel,
        proto::lnrpc::{
            Channel as ProtoChannel, ChannelOpenUpdate, ChannelPoint, ListChannelsRequest,
            ListChannelsResponse, OpenChannelRequest as ProtoOpenChannelRequest, OpenStatusUpdate,
            PendingUpdate, channel_point, open_status_update,
        },
    };

    use super::{ChannelRpc, OpenStatusStream, list_channels_with, open_channel_with};

    const NODE_KEY: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    enum StreamItem {
        Ready(Result<Option<OpenStatusUpdate>, Status>),
        Delayed(Duration, Result<Option<OpenStatusUpdate>, Status>),
    }

    struct FakeStream(VecDeque<StreamItem>);

    impl OpenStatusStream for FakeStream {
        fn message(
            &mut self,
        ) -> impl Future<Output = Result<Option<OpenStatusUpdate>, Status>> + Send {
            let item = self.0.pop_front().unwrap_or(StreamItem::Ready(Ok(None)));
            async move {
                match item {
                    StreamItem::Ready(result) => result,
                    StreamItem::Delayed(duration, result) => {
                        tokio::time::sleep(duration).await;
                        result
                    }
                }
            }
        }
    }

    #[derive(Default)]
    struct FakeChannelRpc {
        open_response: Option<Result<FakeStream, Status>>,
        list_response: Option<Result<ListChannelsResponse, Status>>,
        open_request: Option<ProtoOpenChannelRequest>,
        list_request: Option<ListChannelsRequest>,
    }

    impl ChannelRpc for FakeChannelRpc {
        type OpenStatusStream = FakeStream;

        fn open_channel(
            &mut self,
            request: Request<ProtoOpenChannelRequest>,
        ) -> impl Future<Output = Result<Response<Self::OpenStatusStream>, Status>> + Send {
            self.open_request = Some(request.into_inner());
            let response = self.open_response.take().unwrap();
            async move { response.map(Response::new) }
        }

        fn list_channels(
            &mut self,
            request: Request<ListChannelsRequest>,
        ) -> impl Future<Output = Result<Response<ListChannelsResponse>, Status>> + Send {
            self.list_request = Some(request.into_inner());
            let response = self.list_response.take().unwrap();
            async move { response.map(Response::new) }
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

    fn point(byte: u8, output_index: u32) -> OutPoint {
        OutPoint::new(Txid::from_byte_array([byte; 32]), output_index)
    }

    fn pending(point: OutPoint) -> OpenStatusUpdate {
        OpenStatusUpdate {
            pending_chan_id: vec![2; 32],
            update: Some(open_status_update::Update::ChanPending(PendingUpdate {
                txid: point.txid.to_byte_array().to_vec(),
                output_index: point.vout,
                fee_per_vbyte: 1,
                local_close_tx: false,
            })),
        }
    }

    fn opened(point: OutPoint) -> OpenStatusUpdate {
        OpenStatusUpdate {
            pending_chan_id: vec![2; 32],
            update: Some(open_status_update::Update::ChanOpen(ChannelOpenUpdate {
                channel_point: Some(ChannelPoint {
                    output_index: point.vout,
                    funding_txid: Some(channel_point::FundingTxid::FundingTxidBytes(
                        point.txid.to_byte_array().to_vec(),
                    )),
                }),
            })),
        }
    }

    fn open_request() -> OpenChannelRequest {
        OpenChannelRequest::new(
            NODE_KEY.parse::<PublicKey>().unwrap(),
            Sats::new(2_000_000),
            Sats::new(1_000_000),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn channel_operations_emit_exact_requests_and_convert_responses() {
        let expected_point = point(1, 3);
        let mut rpc = FakeChannelRpc {
            open_response: Some(Ok(FakeStream(VecDeque::from([
                StreamItem::Ready(Ok(Some(pending(expected_point)))),
                StreamItem::Ready(Ok(Some(opened(expected_point)))),
            ])))),
            list_response: Some(Ok(ListChannelsResponse {
                channels: vec![ProtoChannel {
                    active: true,
                    remote_pubkey: NODE_KEY.into(),
                    channel_point: expected_point.to_string(),
                    capacity: 2_000_000,
                    local_balance: 900_000,
                    remote_balance: 1_000_000,
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };
        let client = client(Duration::from_secs(1));

        let actual_point = open_channel_with(&client.inner, &mut rpc, open_request())
            .await
            .unwrap();
        let channels = list_channels_with(&client.inner, &mut rpc).await.unwrap();

        assert_eq!(actual_point, expected_point);
        assert_eq!(channels[0].channel_point(), expected_point);
        let captured_open = rpc.open_request.unwrap();
        assert_eq!(
            captured_open.node_pubkey,
            NODE_KEY.parse::<PublicKey>().unwrap().serialize()
        );
        assert_eq!(captured_open.local_funding_amount, 2_000_000);
        assert_eq!(captured_open.push_sat, 1_000_000);
        assert!(!captured_open.private);
        assert_eq!(rpc.list_request, Some(ListChannelsRequest::default()));
    }

    #[test]
    fn malformed_channel_public_key_is_an_invalid_response() {
        let error = channel(ProtoChannel {
            remote_pubkey: "bad-key".into(),
            channel_point: point(1, 0).to_string(),
            capacity: 1,
            ..Default::default()
        })
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[test]
    fn malformed_channel_txid_is_an_invalid_response() {
        let error = channel(ProtoChannel {
            remote_pubkey: NODE_KEY.into(),
            channel_point: "not-a-txid:0".into(),
            capacity: 1,
            ..Default::default()
        })
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[test]
    fn negative_channel_amounts_are_invalid_responses() {
        for (capacity, local, remote) in [(-1, 0, 0), (1, -1, 0), (1, 0, -1)] {
            let error = channel(ProtoChannel {
                remote_pubkey: NODE_KEY.into(),
                channel_point: point(1, 0).to_string(),
                capacity,
                local_balance: local,
                remote_balance: remote,
                ..Default::default()
            })
            .unwrap_err();

            assert!(matches!(error, LndError::InvalidResponse { .. }));
        }
    }

    #[test]
    fn overflowing_channel_millisatoshis_are_an_invalid_response() {
        let error = channel(ProtoChannel {
            remote_pubkey: NODE_KEY.into(),
            channel_point: point(1, 0).to_string(),
            capacity: i64::MAX,
            local_balance: i64::MAX,
            ..Default::default()
        })
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[test]
    fn channel_balances_above_capacity_are_an_invalid_response() {
        let error = channel(ProtoChannel {
            remote_pubkey: NODE_KEY.into(),
            channel_point: point(1, 0).to_string(),
            capacity: 10,
            local_balance: 6,
            remote_balance: 5,
            ..Default::default()
        })
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[tokio::test]
    async fn oversized_open_amount_is_rejected_before_rpc() {
        let request = OpenChannelRequest::new(
            NODE_KEY.parse::<PublicKey>().unwrap(),
            Sats::new(i64::MAX as u64 + 1),
            Sats::new(1),
        )
        .unwrap();
        let mut rpc = FakeChannelRpc::default();
        let client = client(Duration::from_secs(1));

        let error = open_channel_with(&client.inner, &mut rpc, request)
            .await
            .unwrap_err();

        assert!(matches!(error, LndError::InvalidRequest { .. }));
        assert!(rpc.open_request.is_none());
    }

    #[tokio::test]
    async fn malformed_pending_channel_txid_is_an_invalid_response() {
        let mut update = pending(point(1, 0));
        let Some(open_status_update::Update::ChanPending(pending)) = &mut update.update else {
            unreachable!()
        };
        pending.txid.pop();
        let mut rpc = FakeChannelRpc {
            open_response: Some(Ok(FakeStream(VecDeque::from([StreamItem::Ready(Ok(
                Some(update),
            ))])))),
            ..Default::default()
        };
        let client = client(Duration::from_secs(1));

        let error = open_channel_with(&client.inner, &mut rpc, open_request())
            .await
            .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[tokio::test]
    async fn malformed_open_channel_point_is_an_invalid_response() {
        let mut update = opened(point(1, 0));
        let Some(open_status_update::Update::ChanOpen(opened)) = &mut update.update else {
            unreachable!()
        };
        let point = opened.channel_point.as_mut().unwrap();
        point.funding_txid = Some(channel_point::FundingTxid::FundingTxidStr(
            "bad-txid".into(),
        ));
        let mut rpc = FakeChannelRpc {
            open_response: Some(Ok(FakeStream(VecDeque::from([StreamItem::Ready(Ok(
                Some(update),
            ))])))),
            ..Default::default()
        };
        let client = client(Duration::from_secs(1));

        let error = open_channel_with(&client.inner, &mut rpc, open_request())
            .await
            .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[tokio::test]
    async fn mismatched_pending_and_open_channel_points_are_invalid() {
        let mut rpc = FakeChannelRpc {
            open_response: Some(Ok(FakeStream(VecDeque::from([
                StreamItem::Ready(Ok(Some(pending(point(1, 0))))),
                StreamItem::Ready(Ok(Some(opened(point(2, 0))))),
            ])))),
            ..Default::default()
        };
        let client = client(Duration::from_secs(1));

        let error = open_channel_with(&client.inner, &mut rpc, open_request())
            .await
            .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[tokio::test]
    async fn stream_end_before_open_is_an_invalid_response() {
        let expected = point(1, 0);
        let mut rpc = FakeChannelRpc {
            open_response: Some(Ok(FakeStream(VecDeque::from([
                StreamItem::Ready(Ok(Some(pending(expected)))),
                StreamItem::Ready(Ok(None)),
            ])))),
            ..Default::default()
        };
        let client = client(Duration::from_secs(1));

        let error = open_channel_with(&client.inner, &mut rpc, open_request())
            .await
            .unwrap_err();

        assert!(
            matches!(error, LndError::InvalidResponse { identifier: Some(identifier), .. } if identifier == expected.to_string())
        );
    }

    #[tokio::test]
    async fn status_after_pending_point_preserves_it_as_outcome_unknown() {
        let expected = point(1, 0);
        let mut rpc = FakeChannelRpc {
            open_response: Some(Ok(FakeStream(VecDeque::from([
                StreamItem::Ready(Ok(Some(pending(expected)))),
                StreamItem::Ready(Err(Status::unavailable("lost stream"))),
            ])))),
            ..Default::default()
        };
        let client = client(Duration::from_secs(1));

        let error = open_channel_with(&client.inner, &mut rpc, open_request())
            .await
            .unwrap_err();

        assert!(
            matches!(error, LndError::OutcomeUnknown { identifier: Some(identifier), .. } if identifier == expected.to_string())
        );
    }

    #[tokio::test]
    async fn timeout_after_pending_point_preserves_it_as_outcome_unknown() {
        let expected = point(1, 0);
        let mut rpc = FakeChannelRpc {
            open_response: Some(Ok(FakeStream(VecDeque::from([
                StreamItem::Ready(Ok(Some(pending(expected)))),
                StreamItem::Delayed(Duration::from_secs(1), Ok(None)),
            ])))),
            ..Default::default()
        };
        let client = client(Duration::from_millis(5));

        let error = open_channel_with(&client.inner, &mut rpc, open_request())
            .await
            .unwrap_err();

        assert!(
            matches!(error, LndError::OutcomeUnknown { identifier: Some(identifier), .. } if identifier == expected.to_string())
        );
    }

    #[tokio::test]
    async fn psbt_update_without_a_funding_shim_is_an_invalid_state() {
        let update = OpenStatusUpdate {
            pending_chan_id: vec![2; 32],
            update: Some(open_status_update::Update::PsbtFund(Default::default())),
        };
        let mut rpc = FakeChannelRpc {
            open_response: Some(Ok(FakeStream(VecDeque::from([StreamItem::Ready(Ok(
                Some(update),
            ))])))),
            ..Default::default()
        };
        let client = client(Duration::from_secs(1));

        let error = open_channel_with(&client.inner, &mut rpc, open_request())
            .await
            .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }
}
