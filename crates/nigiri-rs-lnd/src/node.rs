use std::{future::Future, str::FromStr};

use bitcoin::{Address, address::NetworkUnchecked};
use tonic::{Request, Response, Status, transport::Channel as TonicChannel};

use crate::{
    LndClient, LndError, NodeInfo, Peer, PeerAddress, WalletBalance,
    convert::{invalid_response, node_info, peer, wallet_balance as convert_wallet_balance},
    endpoint::serialize_peer_endpoint,
    proto::lnrpc::{
        AddressType, ConnectPeerRequest, ConnectPeerResponse, GetInfoRequest, GetInfoResponse,
        LightningAddress, ListPeersRequest, ListPeersResponse, NewAddressRequest,
        NewAddressResponse, WalletBalanceRequest, WalletBalanceResponse,
        lightning_client::LightningClient,
    },
    transport::{ClientInner, authenticated_request},
};

pub(crate) trait NodeRpc: Send {
    fn get_info(
        &mut self,
        request: Request<GetInfoRequest>,
    ) -> impl Future<Output = Result<Response<GetInfoResponse>, Status>> + Send;
    fn new_address(
        &mut self,
        request: Request<NewAddressRequest>,
    ) -> impl Future<Output = Result<Response<NewAddressResponse>, Status>> + Send;
    fn wallet_balance(
        &mut self,
        request: Request<WalletBalanceRequest>,
    ) -> impl Future<Output = Result<Response<WalletBalanceResponse>, Status>> + Send;
    fn connect_peer(
        &mut self,
        request: Request<ConnectPeerRequest>,
    ) -> impl Future<Output = Result<Response<ConnectPeerResponse>, Status>> + Send;
    fn list_peers(
        &mut self,
        request: Request<ListPeersRequest>,
    ) -> impl Future<Output = Result<Response<ListPeersResponse>, Status>> + Send;
}

impl NodeRpc for LightningClient<TonicChannel> {
    fn get_info(
        &mut self,
        request: Request<GetInfoRequest>,
    ) -> impl Future<Output = Result<Response<GetInfoResponse>, Status>> + Send {
        LightningClient::get_info(self, request)
    }

    fn new_address(
        &mut self,
        request: Request<NewAddressRequest>,
    ) -> impl Future<Output = Result<Response<NewAddressResponse>, Status>> + Send {
        LightningClient::new_address(self, request)
    }

    fn wallet_balance(
        &mut self,
        request: Request<WalletBalanceRequest>,
    ) -> impl Future<Output = Result<Response<WalletBalanceResponse>, Status>> + Send {
        LightningClient::wallet_balance(self, request)
    }

    fn connect_peer(
        &mut self,
        request: Request<ConnectPeerRequest>,
    ) -> impl Future<Output = Result<Response<ConnectPeerResponse>, Status>> + Send {
        LightningClient::connect_peer(self, request)
    }

    fn list_peers(
        &mut self,
        request: Request<ListPeersRequest>,
    ) -> impl Future<Output = Result<Response<ListPeersResponse>, Status>> + Send {
        LightningClient::list_peers(self, request)
    }
}

impl LndClient {
    /// Returns identity and synchronization information reported by LND.
    pub async fn get_info(&self) -> Result<NodeInfo, LndError> {
        let mut rpc = LightningClient::new(self.inner.channel().await);
        get_info_with(&self.inner, &mut rpc).await
    }

    /// Generates a native SegWit wallet address without assuming its network.
    pub async fn new_address(&self) -> Result<Address<NetworkUnchecked>, LndError> {
        let mut rpc = LightningClient::new(self.inner.channel().await);
        new_address_with(&self.inner, &mut rpc).await
    }

    /// Returns the default wallet account's on-chain balance.
    pub async fn wallet_balance(&self) -> Result<WalletBalance, LndError> {
        let mut rpc = LightningClient::new(self.inner.channel().await);
        wallet_balance_with(&self.inner, &mut rpc).await
    }

    /// Connects to a peer for the duration of the daemon process.
    pub async fn connect_peer(&self, peer: &PeerAddress) -> Result<(), LndError> {
        let mut rpc = LightningClient::new(self.inner.channel().await);
        connect_peer_with(&self.inner, &mut rpc, peer).await
    }

    /// Lists peers currently connected to the daemon.
    pub async fn list_peers(&self) -> Result<Vec<Peer>, LndError> {
        let mut rpc = LightningClient::new(self.inner.channel().await);
        list_peers_with(&self.inner, &mut rpc).await
    }
}

pub(crate) async fn get_info_with<R: NodeRpc>(
    inner: &ClientInner,
    rpc: &mut R,
) -> Result<NodeInfo, LndError> {
    let response = authenticated_request(inner, "get info", GetInfoRequest {}, |request| {
        rpc.get_info(request)
    })
    .await?;
    node_info(response.into_inner())
}

pub(crate) async fn new_address_with<R: NodeRpc>(
    inner: &ClientInner,
    rpc: &mut R,
) -> Result<Address<NetworkUnchecked>, LndError> {
    let response = authenticated_request(inner, "new address", new_address_request(), |request| {
        rpc.new_address(request)
    })
    .await?
    .into_inner();
    wallet_address(response)
}

fn wallet_address(response: NewAddressResponse) -> Result<Address<NetworkUnchecked>, LndError> {
    Address::from_str(&response.address)
        .map_err(|_| invalid_response("new address", "wallet address is malformed"))
}

pub(crate) async fn wallet_balance_with<R: NodeRpc>(
    inner: &ClientInner,
    rpc: &mut R,
) -> Result<WalletBalance, LndError> {
    let response = authenticated_request(
        inner,
        "wallet balance",
        wallet_balance_request(),
        |request| rpc.wallet_balance(request),
    )
    .await?;
    convert_wallet_balance(response.into_inner())
}

pub(crate) async fn connect_peer_with<R: NodeRpc>(
    inner: &ClientInner,
    rpc: &mut R,
    peer: &PeerAddress,
) -> Result<(), LndError> {
    authenticated_request(
        inner,
        "connect peer",
        connect_peer_request(peer),
        |request| rpc.connect_peer(request),
    )
    .await?;
    Ok(())
}

fn connect_peer_request(peer: &PeerAddress) -> ConnectPeerRequest {
    ConnectPeerRequest {
        addr: Some(LightningAddress {
            pubkey: peer.public_key().to_string(),
            host: serialize_peer_endpoint(peer.host(), peer.port()),
        }),
        perm: false,
        timeout: 0,
    }
}

pub(crate) async fn list_peers_with<R: NodeRpc>(
    inner: &ClientInner,
    rpc: &mut R,
) -> Result<Vec<Peer>, LndError> {
    let response = authenticated_request(inner, "list peers", list_peers_request(), |request| {
        rpc.list_peers(request)
    })
    .await?
    .into_inner();
    response.peers.into_iter().map(peer).collect()
}

fn new_address_request() -> NewAddressRequest {
    NewAddressRequest {
        r#type: AddressType::WitnessPubkeyHash as i32,
        account: String::new(),
    }
}

fn wallet_balance_request() -> WalletBalanceRequest {
    WalletBalanceRequest {
        account: String::new(),
        min_confs: 0,
    }
}

fn list_peers_request() -> ListPeersRequest {
    ListPeersRequest {
        latest_error: false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]

    use std::{collections::HashMap, future::Future, time::Duration};

    use bitcoin::{Address, Network, address::NetworkUnchecked, secp256k1::PublicKey};
    use tonic::{Request, Response, Status};

    use crate::{
        LndClient, LndConfig, LndError, PeerAddress, Sats,
        convert::{node_info, peer, wallet_balance},
        proto::lnrpc::{
            AddressType, Chain, ConnectPeerRequest, ConnectPeerResponse, GetInfoRequest,
            GetInfoResponse, ListPeersRequest, ListPeersResponse, NewAddressRequest,
            NewAddressResponse, Peer as ProtoPeer, WalletBalanceRequest, WalletBalanceResponse,
        },
    };

    use super::{
        NodeRpc, connect_peer_request, connect_peer_with, get_info_with, list_peers_with,
        new_address_with, wallet_address, wallet_balance_with,
    };

    const NODE_KEY: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    #[derive(Default)]
    struct FakeNodeRpc {
        get_info_response: Option<Result<GetInfoResponse, Status>>,
        new_address_response: Option<Result<NewAddressResponse, Status>>,
        wallet_balance_response: Option<Result<WalletBalanceResponse, Status>>,
        connect_peer_response: Option<Result<ConnectPeerResponse, Status>>,
        list_peers_response: Option<Result<ListPeersResponse, Status>>,
        get_info_request: Option<GetInfoRequest>,
        new_address_request: Option<NewAddressRequest>,
        wallet_balance_request: Option<WalletBalanceRequest>,
        connect_peer_request: Option<ConnectPeerRequest>,
        list_peers_request: Option<ListPeersRequest>,
    }

    impl NodeRpc for FakeNodeRpc {
        fn get_info(
            &mut self,
            request: Request<GetInfoRequest>,
        ) -> impl Future<Output = Result<Response<GetInfoResponse>, Status>> + Send {
            self.get_info_request = Some(request.into_inner());
            let response = self.get_info_response.take().unwrap();
            async move { response.map(Response::new) }
        }

        fn new_address(
            &mut self,
            request: Request<NewAddressRequest>,
        ) -> impl Future<Output = Result<Response<NewAddressResponse>, Status>> + Send {
            self.new_address_request = Some(request.into_inner());
            let response = self.new_address_response.take().unwrap();
            async move { response.map(Response::new) }
        }

        fn wallet_balance(
            &mut self,
            request: Request<WalletBalanceRequest>,
        ) -> impl Future<Output = Result<Response<WalletBalanceResponse>, Status>> + Send {
            self.wallet_balance_request = Some(request.into_inner());
            let response = self.wallet_balance_response.take().unwrap();
            async move { response.map(Response::new) }
        }

        fn connect_peer(
            &mut self,
            request: Request<ConnectPeerRequest>,
        ) -> impl Future<Output = Result<Response<ConnectPeerResponse>, Status>> + Send {
            self.connect_peer_request = Some(request.into_inner());
            let response = self.connect_peer_response.take().unwrap();
            async move { response.map(Response::new) }
        }

        fn list_peers(
            &mut self,
            request: Request<ListPeersRequest>,
        ) -> impl Future<Output = Result<Response<ListPeersResponse>, Status>> + Send {
            self.list_peers_request = Some(request.into_inner());
            let response = self.list_peers_response.take().unwrap();
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

    fn info_response() -> GetInfoResponse {
        GetInfoResponse {
            version: "0.21.1-beta".into(),
            identity_pubkey: NODE_KEY.into(),
            alias: "alice".into(),
            block_height: 321,
            synced_to_chain: true,
            synced_to_graph: true,
            chains: vec![Chain {
                chain: "bitcoin".into(),
                network: "regtest".into(),
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn node_operations_emit_exact_requests_and_convert_responses() {
        let public_key = NODE_KEY.parse::<PublicKey>().unwrap();
        let address = Address::p2wpkh(&bitcoin::CompressedPublicKey(public_key), Network::Regtest);
        let mut rpc = FakeNodeRpc {
            get_info_response: Some(Ok(info_response())),
            new_address_response: Some(Ok(NewAddressResponse {
                address: address.to_string(),
            })),
            wallet_balance_response: Some(Ok(WalletBalanceResponse {
                total_balance: 30,
                confirmed_balance: 20,
                unconfirmed_balance: 10,
                locked_balance: 0,
                reserved_balance_anchor_chan: 0,
                account_balance: HashMap::new(),
            })),
            connect_peer_response: Some(Ok(ConnectPeerResponse {
                status: String::new(),
            })),
            list_peers_response: Some(Ok(ListPeersResponse {
                peers: vec![ProtoPeer {
                    pub_key: NODE_KEY.into(),
                    address: "bob.internal:9735".into(),
                    ..Default::default()
                }],
            })),
            ..Default::default()
        };
        let client = client(Duration::from_secs(1));

        let info = get_info_with(&client.inner, &mut rpc).await.unwrap();
        let generated_address: Address<NetworkUnchecked> =
            new_address_with(&client.inner, &mut rpc).await.unwrap();
        let balance = wallet_balance_with(&client.inner, &mut rpc).await.unwrap();
        let peer_address = PeerAddress::new(public_key, "bob.internal", 9735).unwrap();
        connect_peer_with(&client.inner, &mut rpc, &peer_address)
            .await
            .unwrap();
        let peers = list_peers_with(&client.inner, &mut rpc).await.unwrap();

        assert_eq!(info.public_key(), public_key);
        assert_eq!(info.network(), "regtest");
        assert_eq!(
            generated_address.require_network(Network::Regtest).unwrap(),
            address
        );
        assert_eq!(balance.total(), Sats::new(30));
        assert_eq!(peers[0].public_key(), public_key);
        assert_eq!(rpc.get_info_request, Some(GetInfoRequest {}));
        let captured_new_address = rpc.new_address_request.unwrap();
        assert_eq!(
            captured_new_address.r#type,
            AddressType::WitnessPubkeyHash as i32
        );
        assert_eq!(captured_new_address.account, "");
        assert_eq!(
            rpc.wallet_balance_request,
            Some(WalletBalanceRequest {
                account: String::new(),
                min_confs: 0,
            })
        );
        let captured_connect = rpc.connect_peer_request.unwrap();
        assert!(!captured_connect.perm);
        assert_eq!(captured_connect.timeout, 0);
        let captured_addr = captured_connect.addr.unwrap();
        assert_eq!(captured_addr.pubkey, public_key.to_string());
        assert_eq!(captured_addr.host, "bob.internal:9735");
        assert_eq!(
            rpc.list_peers_request,
            Some(ListPeersRequest {
                latest_error: false
            })
        );
    }

    #[test]
    fn malformed_node_public_key_is_an_invalid_response() {
        let mut response = info_response();
        response.identity_pubkey = "not-a-public-key".into();

        let error = node_info(response).unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[test]
    fn invalid_node_chain_combinations_are_rejected() {
        for chains in [
            vec![],
            vec![Chain {
                chain: "litecoin".into(),
                network: "regtest".into(),
            }],
            vec![
                Chain {
                    chain: "bitcoin".into(),
                    network: "regtest".into(),
                },
                Chain {
                    chain: "bitcoin".into(),
                    network: "testnet".into(),
                },
            ],
        ] {
            let mut response = info_response();
            response.chains = chains;
            let error = node_info(response).unwrap_err();

            assert!(matches!(error, LndError::InvalidResponse { .. }));
        }
    }

    #[test]
    fn malformed_wallet_address_is_an_invalid_response() {
        let error = wallet_address(NewAddressResponse {
            address: "not-an-address".into(),
        })
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[test]
    fn negative_wallet_amounts_are_invalid_responses() {
        for (total, confirmed, unconfirmed) in [(-1, 0, 0), (0, -1, 0), (0, 0, -1)] {
            let error = wallet_balance(WalletBalanceResponse {
                total_balance: total,
                confirmed_balance: confirmed,
                unconfirmed_balance: unconfirmed,
                ..Default::default()
            })
            .unwrap_err();

            assert!(matches!(error, LndError::InvalidResponse { .. }));
        }
    }

    #[test]
    fn inconsistent_wallet_total_is_an_invalid_response() {
        let error = wallet_balance(WalletBalanceResponse {
            total_balance: 3,
            confirmed_balance: 1,
            unconfirmed_balance: 1,
            ..Default::default()
        })
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[test]
    fn malformed_peer_public_key_is_an_invalid_response() {
        let error = peer(ProtoPeer {
            pub_key: "bad-key".into(),
            address: "bob.internal:9735".into(),
            ..Default::default()
        })
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[test]
    fn connected_peer_without_an_address_is_an_invalid_response() {
        let error = peer(ProtoPeer {
            pub_key: NODE_KEY.into(),
            address: String::new(),
            ..Default::default()
        })
        .unwrap_err();

        assert!(matches!(error, LndError::InvalidResponse { .. }));
    }

    #[test]
    fn peer_hosts_serialize_to_unambiguous_lnd_endpoints() {
        const ONION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.onion";
        let public_key = NODE_KEY.parse::<PublicKey>().unwrap();

        for (host, expected) in [
            ("bob.internal", "bob.internal:9735"),
            ("127.0.0.1", "127.0.0.1:9735"),
            ("2001:db8::1", "[2001:db8::1]:9735"),
            ("[2001:db8::1]", "[2001:db8::1]:9735"),
            (
                ONION,
                concat!(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.onion",
                    ":9735"
                ),
            ),
        ] {
            let peer = PeerAddress::new(public_key, host, 9735).unwrap();

            let request = connect_peer_request(&peer);

            assert_eq!(request.addr.unwrap().host, expected);
        }
    }

    #[test]
    fn peer_address_rejects_malformed_hosts_and_embedded_ports() {
        let public_key = NODE_KEY.parse::<PublicKey>().unwrap();

        for host in [
            "example.com:9735",
            "127.0.0.1:9735",
            "[::1]:9735",
            "[::1",
            "::1]",
            "user@example.com",
            "example..com",
            "-bad.example",
            "bad-.example",
            "exa mple.com",
            "999.1.1.1",
            "short.onion",
            "9999999999999999.ONION",
        ] {
            assert!(
                matches!(
                    PeerAddress::new(public_key, host, 9735),
                    Err(LndError::InvalidRequest { .. })
                ),
                "accepted malformed peer host {host:?}"
            );
        }
    }

    #[test]
    fn peer_response_accepts_valid_endpoint_authorities() {
        const ONION_ENDPOINT: &str = concat!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.onion",
            ":9735"
        );

        for endpoint in [
            "bob.internal:9735",
            "127.0.0.1:9735",
            "[2001:db8::1]:9735",
            ONION_ENDPOINT,
        ] {
            let converted = peer(ProtoPeer {
                pub_key: NODE_KEY.into(),
                address: endpoint.into(),
                ..Default::default()
            })
            .unwrap();

            assert_eq!(converted.address(), endpoint);
        }
    }

    #[test]
    fn peer_response_rejects_malformed_endpoint_authorities() {
        for endpoint in [
            "not-an-endpoint",
            "example.com",
            "::1:9735",
            "[::1]",
            "[::1]:0",
            "example.com:0",
            "example.com:70000",
            "example.com:9735:1234",
            "user@example.com:9735",
            "short.onion:9735",
            "9999999999999999.ONION:9735",
            " bob.internal:9735 ",
        ] {
            let error = peer(ProtoPeer {
                pub_key: NODE_KEY.into(),
                address: endpoint.into(),
                ..Default::default()
            })
            .unwrap_err();

            assert!(
                matches!(error, LndError::InvalidResponse { .. }),
                "accepted malformed peer endpoint {endpoint:?}"
            );
        }
    }
}
