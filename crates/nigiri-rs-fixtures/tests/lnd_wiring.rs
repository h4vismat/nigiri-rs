//! Real topology guarantees for the four-container LND fixture.

use nigiri_rs_fixtures::LndPair;
use nigiri_rs_lnd::Millisats;
use serde_json::Value;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::test]
async fn pair_reports_one_funded_active_channel_on_the_same_regtest_tip() -> Result<(), BoxError> {
    let pair = LndPair::start().await?;
    let point = pair.channel_point();
    let bitcoin_height = pair.bitcoin().rpc::<u64, _>("getblockcount", ()).await?;
    let (alice_info, bob_info, alice_channels, bob_channels) = tokio::try_join!(
        pair.alice().get_info(),
        pair.bob().get_info(),
        pair.alice().list_channels(),
        pair.bob().list_channels(),
    )?;

    for info in [&alice_info, &bob_info] {
        assert_eq!(info.network(), "regtest");
        assert_eq!(u64::from(info.block_height()), bitcoin_height);
        assert!(info.synced_to_chain());
        assert!(info.synced_to_graph());
    }

    let alice = alice_channels
        .iter()
        .find(|channel| channel.channel_point() == point)
        .expect("Alice must report the fixture channel point");
    let bob = bob_channels
        .iter()
        .find(|channel| channel.channel_point() == point)
        .expect("Bob must report the fixture channel point");
    assert!(alice.active() && bob.active());
    assert_eq!(alice.remote_public_key(), bob_info.public_key());
    assert_eq!(bob.remote_public_key(), alice_info.public_key());
    assert!(alice.local_balance() > Millisats::new(1_000));
    assert!(bob.local_balance() > Millisats::new(1_000));

    let funding_output = pair
        .bitcoin()
        .rpc::<Value, _>(
            "gettxout",
            (point.txid.to_string(), u64::from(point.vout), true),
        )
        .await?;
    assert!(
        !funding_output.is_null(),
        "the exposed channel point must name an unspent funding output"
    );

    pair.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn parallel_pairs_have_independent_channels_and_runtime_names() -> Result<(), BoxError> {
    let (left, right) = tokio::try_join!(LndPair::start(), LndPair::start())?;

    assert_ne!(
        left.channel_point(),
        right.channel_point(),
        "parallel fixtures must not share a funding transaction or channel"
    );
    assert_eq!(left.alice().get_info().await?.network(), "regtest");
    assert_eq!(right.alice().get_info().await?.network(), "regtest");

    let (left_shutdown, right_shutdown) = tokio::join!(left.shutdown(), right.shutdown());
    left_shutdown?;
    right_shutdown?;
    Ok(())
}
