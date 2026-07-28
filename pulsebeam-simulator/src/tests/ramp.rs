use super::common;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A simulcast sender on an unimpaired network must reach the link's
/// congestion-free capacity almost immediately, not crawl up over tens of
/// seconds. We drive a real end-to-end session (sender → SFU → receiver) and
/// assert the sender's uplink already averages near capacity within the first
/// couple of seconds and never collapses back to the base layer afterwards.
#[test]
fn fast_initial_ramp_up_on_good_network_test() {
    const OBSERVE_SECS: usize = 20;
    const RAMP_DEADLINE_SECS: usize = 2;
    // The link's congestion-free capacity in this sim sits around ~600-700 kbps.
    // A fast ramp means the first couple of seconds already average near it,
    // rather than crawling up from the 500 kbps seed over tens of seconds.
    const EARLY_MIN_BPS: u64 = 400_000;
    // Above the ~300 kbps base ("q") layer: the uplink must never collapse back
    // to base-only once ramped.
    const FLOOR_BPS: u64 = 250_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(40))
        .tick_duration(Duration::from_millis(1))
        .rng_seed(0xDEADBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();

    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("q"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("f"),
                    ]),
                )
                .connect("room1")
                .await?;

            let tx_bytes = |c: &common::client::SimClient| -> u64 {
                c.ctx
                    .driver
                    .stats()
                    .peer
                    .as_ref()
                    .map_or(0, |p| p.peer_bytes_tx)
            };

            let mut uplink_bps = Vec::with_capacity(OBSERVE_SECS);
            let mut prev = tx_bytes(&client);
            for _ in 0..OBSERVE_SECS {
                client.drive_for(Duration::from_secs(1)).await?;
                let now = tx_bytes(&client);
                uplink_bps.push((now - prev) * 8);
                prev = now;
            }
            done.cancel();

            tracing::info!(?uplink_bps, "uplink ramp");

            let early_avg = uplink_bps[..RAMP_DEADLINE_SECS].iter().sum::<u64>()
                / RAMP_DEADLINE_SECS as u64;
            assert!(
                early_avg >= EARLY_MIN_BPS,
                "slow ramp: first {RAMP_DEADLINE_SECS}s averaged only {early_avg} bps (< {EARLY_MIN_BPS}): {uplink_bps:?}"
            );

            let trough = *uplink_bps[RAMP_DEADLINE_SECS..]
                .iter()
                .min()
                .expect("post-ramp samples");
            assert!(
                trough >= FLOOR_BPS,
                "uplink collapsed to {trough} bps (< {FLOOR_BPS}) after ramping: {uplink_bps:?}"
            );

            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;
        client.drive(done).await?;
        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(120)).expect("simulation failed");
}
