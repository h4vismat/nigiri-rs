use std::future::Future;

use nigiri_rs_core::{Bitcoin, NigiriClient};

use crate::{
    ContainerImage, FixtureError, RPC_PASSWORD, RPC_USER, chain::FixtureChain, deadline::Deadline,
    node::bootstrap_error,
};

/// Serializes the initial 101-block mine across concurrent fixtures.
///
/// Process-global and Bitcoin-only. Liquid does not mine to fund a wallet, so routing it through
/// this permit would serialize it against Bitcoin fixtures for nothing.
struct InitialMiningGate {
    permit: tokio::sync::Mutex<()>,
}

impl InitialMiningGate {
    const fn new() -> Self {
        Self {
            permit: tokio::sync::Mutex::const_new(()),
        }
    }

    async fn run<T, F>(&self, deadline: &Deadline, future: F) -> Result<T, FixtureError>
    where
        F: Future<Output = T>,
    {
        // Acquiring and holding the permit are reported separately so a timeout distinguishes
        // waiting for another fixture from mining that is genuinely too slow. Both are bounded by
        // the same shared budget, and dropping the guard on cancellation releases the permit.
        let _permit = deadline
            .run(
                Bitcoin::NODE_SERVICE,
                "waiting for the initial mining permit",
                self.permit.lock(),
            )
            .await?;

        deadline
            .run(
                Bitcoin::NODE_SERVICE,
                "mining the initial 101 blocks",
                future,
            )
            .await
    }
}

static INITIAL_MINING_GATE: InitialMiningGate = InitialMiningGate::new();

impl FixtureChain for Bitcoin {
    const NODE_SERVICE: &'static str = "bitcoind";
    const CHAIN_NAME: &'static str = "Bitcoin";
    const NODE_RPC_PORT: u16 = 18_443;
    const ELECTRS_HTTP_PORT: u16 = 30_000;
    const ELECTRS_ELECTRUM_PORT: u16 = 50_000;
    const NODE_NAME_PREFIX: &'static str = "nigiri-rs-bitcoind";

    fn node_image_default() -> ContainerImage {
        ContainerImage::bitcoind_default()
    }

    fn electrs_image_default() -> ContainerImage {
        ContainerImage::electrs_default()
    }

    fn node_cmd() -> Vec<String> {
        vec![
            "-regtest=1".to_owned(),
            "-server=1".to_owned(),
            "-txindex=1".to_owned(),
            format!("-rpcbind=0.0.0.0:{}", Self::NODE_RPC_PORT),
            "-rpcallowip=0.0.0.0/0".to_owned(),
            format!("-rpcuser={RPC_USER}"),
            format!("-rpcpassword={RPC_PASSWORD}"),
            "-fallbackfee=0.00001".to_owned(),
            "-printtoconsole=1".to_owned(),
        ]
    }

    fn electrs_cmd(node_container: &str) -> Vec<String> {
        vec![
            "-vvvv".to_owned(),
            "--network".to_owned(),
            "regtest".to_owned(),
            // Inert: `--jsonrpc-import` makes Electrs read blocks over JSON-RPC rather than from
            // the daemon directory, so this path is never opened.
            "--daemon-dir".to_owned(),
            "/tmp/bitcoin".to_owned(),
            "--db-dir".to_owned(),
            "/tmp/electrs".to_owned(),
            "--daemon-rpc-addr".to_owned(),
            format!("{node_container}:{}", Self::NODE_RPC_PORT),
            "--cookie".to_owned(),
            format!("{RPC_USER}:{RPC_PASSWORD}"),
            "--http-addr".to_owned(),
            format!("0.0.0.0:{}", Self::ELECTRS_HTTP_PORT),
            "--electrum-rpc-addr".to_owned(),
            format!("0.0.0.0:{}", Self::ELECTRS_ELECTRUM_PORT),
            "--cors".to_owned(),
            "*".to_owned(),
            "--jsonrpc-import".to_owned(),
        ]
    }

    #[allow(
        private_interfaces,
        reason = "sealed trait; Deadline never crosses the crate boundary"
    )]
    async fn fund_wallet(
        client: &NigiriClient<Bitcoin>,
        deadline: &Deadline,
    ) -> Result<(), FixtureError> {
        let mining_address = deadline
            .run(
                Self::NODE_SERVICE,
                "creating initial mining address",
                client.new_address(),
            )
            .await?
            .map_err(|source| bootstrap_error(Self::CHAIN_NAME, "getnewaddress", source))?
            .to_string();

        INITIAL_MINING_GATE
            .run(
                deadline,
                client.generate_to_address(101, mining_address.as_str()),
            )
            .await?
            .map_err(|source| bootstrap_error(Self::CHAIN_NAME, "generatetoaddress", source))?;

        Ok(())
    }
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

    use tokio::sync::{Barrier, Notify, mpsc, oneshot};

    use super::InitialMiningGate;
    use crate::deadline::Deadline;

    struct ActiveMining {
        active: Arc<AtomicUsize>,
    }

    impl ActiveMining {
        fn enter(active: Arc<AtomicUsize>, maximum: &AtomicUsize) -> Self {
            let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(now_active, Ordering::SeqCst);
            Self { active }
        }
    }

    impl Drop for ActiveMining {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    // Catches a regression that permits concurrent initial 101-block mining or leaks a permit
    // when its owning startup task is cancelled. The watchdog only detects a deadlock; ordering
    // is proven through channels and atomics.
    #[tokio::test]
    async fn initial_mining_gate_serializes_and_releases_after_cancellation() {
        let gate = Arc::new(InitialMiningGate::new());
        let deadline = Arc::new(
            crate::deadline::Deadline::new(Duration::from_secs(10))
                .expect("a positive shared deadline is valid"),
        );
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (first_entered_tx, first_entered_rx) = oneshot::channel();
        let (first_release_tx, first_release_rx) = oneshot::channel::<()>();

        let first = {
            let gate = Arc::clone(&gate);
            let deadline = Arc::clone(&deadline);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            tokio::spawn(async move {
                gate.run(&deadline, async move {
                    let _active = ActiveMining::enter(active, &maximum);
                    first_entered_tx
                        .send(())
                        .expect("the test must await first entry");
                    let _ = first_release_rx.await;
                })
                .await
            })
        };

        first_entered_rx
            .await
            .expect("the first mining operation must acquire the permit");

        let (second_entered_tx, mut second_entered_rx) = oneshot::channel();
        let second = {
            let gate = Arc::clone(&gate);
            let deadline = Arc::clone(&deadline);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            tokio::spawn(async move {
                gate.run(&deadline, async move {
                    let _active = ActiveMining::enter(active, &maximum);
                    second_entered_tx
                        .send(())
                        .expect("the test must await second entry");
                })
                .await
            })
        };

        tokio::task::yield_now().await;
        assert!(
            matches!(
                second_entered_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "work waiting for the mining permit must not enter concurrently"
        );

        first.abort();
        let _ = first.await;
        drop(first_release_tx);

        tokio::time::timeout(Duration::from_secs(2), &mut second_entered_rx)
            .await
            .expect("cancelling the first operation must release the mining permit")
            .expect("the second mining operation must enter after cancellation");
        second
            .await
            .expect("the second task must complete")
            .expect("the gate must preserve successful operation output");
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    // Catches a regression that broadens the initial-mining permit to serialize unrelated work
    // before or after the one Bitcoin Core mining RPC.
    #[tokio::test]
    async fn initial_mining_gate_only_serializes_the_mining_future() {
        let gate = Arc::new(InitialMiningGate::new());
        let deadline =
            Arc::new(Deadline::new(Duration::from_secs(10)).expect("a positive deadline is valid"));
        let pre_barrier = Arc::new(Barrier::new(3));
        let post_barrier = Arc::new(Barrier::new(3));
        let release_first_mining = Arc::new(Notify::new());
        let mining_order = Arc::new(AtomicUsize::new(0));
        let pre_active = Arc::new(AtomicUsize::new(0));
        let pre_maximum = Arc::new(AtomicUsize::new(0));
        let mining_active = Arc::new(AtomicUsize::new(0));
        let mining_maximum = Arc::new(AtomicUsize::new(0));
        let post_active = Arc::new(AtomicUsize::new(0));
        let post_maximum = Arc::new(AtomicUsize::new(0));
        let (mining_started_tx, mut mining_started_rx) = mpsc::unbounded_channel();

        let start_worker = |mining_started_tx: mpsc::UnboundedSender<usize>| {
            let gate = Arc::clone(&gate);
            let deadline = Arc::clone(&deadline);
            let pre_barrier = Arc::clone(&pre_barrier);
            let post_barrier = Arc::clone(&post_barrier);
            let release_first_mining = Arc::clone(&release_first_mining);
            let mining_order = Arc::clone(&mining_order);
            let pre_active = Arc::clone(&pre_active);
            let pre_maximum = Arc::clone(&pre_maximum);
            let mining_active = Arc::clone(&mining_active);
            let mining_maximum = Arc::clone(&mining_maximum);
            let post_active = Arc::clone(&post_active);
            let post_maximum = Arc::clone(&post_maximum);

            tokio::spawn(async move {
                let pre_work = ActiveMining::enter(pre_active, &pre_maximum);
                pre_barrier.wait().await;
                drop(pre_work);

                gate.run(&deadline, async move {
                    let _mining_work = ActiveMining::enter(mining_active, &mining_maximum);
                    let order = mining_order.fetch_add(1, Ordering::SeqCst);
                    mining_started_tx
                        .send(order)
                        .expect("the test must observe mining entry");
                    if order == 0 {
                        release_first_mining.notified().await;
                    }
                })
                .await
                .expect("the mining gate must preserve successful work");

                let _post_work = ActiveMining::enter(post_active, &post_maximum);
                post_barrier.wait().await;
            })
        };

        let first = start_worker(mining_started_tx.clone());
        let second = start_worker(mining_started_tx);

        tokio::time::timeout(Duration::from_secs(2), pre_barrier.wait())
            .await
            .expect("both workers must reach pre-gate work concurrently");
        assert_eq!(pre_maximum.load(Ordering::SeqCst), 2);

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), mining_started_rx.recv())
                .await
                .expect("one worker must enter the mining permit")
                .expect("the mining-entry channel must remain open"),
            0
        );
        assert_eq!(mining_maximum.load(Ordering::SeqCst), 1);
        release_first_mining.notify_one();

        tokio::time::timeout(Duration::from_secs(2), post_barrier.wait())
            .await
            .expect("both workers must reach post-gate work concurrently");
        first.await.expect("the first worker must complete");
        second.await.expect("the second worker must complete");

        assert_eq!(mining_maximum.load(Ordering::SeqCst), 1);
        assert_eq!(post_maximum.load(Ordering::SeqCst), 2);
    }
}
