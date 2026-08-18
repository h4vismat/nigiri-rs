use std::{fmt, sync::Arc};

use crate::{
    LndConfig, LndError, NodeInfo,
    convert::node_info,
    proto::lnrpc::{GetInfoRequest, lightning_client::LightningClient},
    transport::{ClientInner, authenticated_request},
};

/// Immutable, cheaply cloneable client for an externally managed LND node.
#[derive(Clone)]
pub struct LndClient {
    pub(crate) inner: Arc<ClientInner>,
}

impl LndClient {
    /// Builds an authenticated client without opening a network connection.
    pub fn with_config(config: LndConfig) -> Result<Self, LndError> {
        Ok(Self {
            inner: ClientInner::authenticated(&config)?,
        })
    }

    /// Performs a bounded `GetInfo` call and returns once the daemon is reachable.
    pub async fn wait_ready(&self) -> Result<NodeInfo, LndError> {
        self.get_info().await
    }

    async fn get_info(&self) -> Result<NodeInfo, LndError> {
        let mut client = LightningClient::new(self.inner.channel.clone());
        let response =
            authenticated_request(&self.inner, "get info", GetInfoRequest {}, |request| {
                client.get_info(request)
            })
            .await?;
        node_info(response.into_inner())
    }
}

impl fmt::Debug for LndClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LndClient")
            .field("timeout", &self.inner.timeout)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{LndClient, LndConfig, LndError};

    #[tokio::test]
    async fn construction_is_lazy_and_wait_ready_performs_the_first_rpc() {
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem()
            .into_bytes();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let config = LndConfig::new(
            format!("https://localhost:{port}"),
            certificate,
            vec![1],
            Duration::from_millis(100),
        )
        .unwrap();

        let client = LndClient::with_config(config).unwrap();
        let error = client.wait_ready().await.unwrap_err();

        assert!(matches!(
            error,
            LndError::Status { .. } | LndError::Transport { .. } | LndError::Timeout { .. }
        ));
    }
}
