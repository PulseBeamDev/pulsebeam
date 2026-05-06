use super::common;
use crate::tests::scenario::{
    AssertAllDisconnectedStage, ChurnStage, ConnectPeersStage, ConnectPeersTcpOnlyStage,
    DisconnectStage, HoldStage, PartitionStage, Scenario, StartSfuStage,
    StartSfuTcpOnlyMultiShardStage, StartSfuTcpOnlyStage,
};
use std::time::Duration;

fn run_connection_scenario(
    name: &'static str,
    peers: usize,
    min_rx_bytes: u64,
    enable_partition: bool,
    enable_churn: bool,
) {
    common::setup_tracing();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(90))
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0xDEADBEEF)
        .build();

    let mut scenario = Scenario::new(name, server_ip, sim)
        .add_stage(StartSfuStage)
        .add_stage(ConnectPeersStage {
            peers,
            min_tx_bytes: 0,
            min_rx_bytes,
            max_wait: Duration::from_secs(20),
        });

    if enable_partition {
        scenario = scenario.add_stage(PartitionStage {
            client_index: 0,
            delay: Duration::from_secs(5),
            duration: Duration::from_secs(10),
        });

        // Optional hold (buffering) test to validate packet flush behavior.
        scenario = scenario.add_stage(HoldStage {
            client_index: 1.min(peers - 1),
            delay: Duration::from_secs(10),
            duration: Duration::from_secs(8),
        });
    }

    if enable_churn {
        scenario = scenario.add_stage(ChurnStage {
            cycles: 2,
            num_peers: 2,
            join_duration: Duration::from_secs(6),
            pause_between: Duration::from_secs(4),
        });
    }

    scenario
        .add_stage(DisconnectStage {
            after: Duration::from_secs(40),
        })
        .add_stage(AssertAllDisconnectedStage {
            after: Duration::from_secs(60),
        })
        .run()
        .expect("Simulation failed");
}

#[test]
fn simulation_test() {
    // Deterministic parameters to validate the full connection lifecycle.
    // We use low byte-received thresholds to keep the test stable on CI.
    run_connection_scenario("connection_test", 3, 1, true, true);
}

#[test]
fn tcp_simulation_test() {
    common::setup_tracing();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(90))
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0xDEADBEEF)
        .build();

    Scenario::new("tcp_connection_test", server_ip, sim)
        // Start a server that only advertises a TCP passive candidate
        .add_stage(StartSfuTcpOnlyStage)
        // Connect two peers using the TCP active path
        .add_stage(ConnectPeersTcpOnlyStage {
            peers: 2,
            min_tx_bytes: 0,
            min_rx_bytes: 1,
            max_wait: Duration::from_secs(25),
        })
        .add_stage(DisconnectStage {
            after: Duration::from_secs(40),
        })
        .add_stage(AssertAllDisconnectedStage {
            after: Duration::from_secs(60),
        })
        .run()
        .expect("TCP simulation failed");
}

/// Reproduces the Chrome-with-UDP-disabled failure: with two shards the hash of
/// a client's `peer_addr` and the hash of `room_id` can land on different shards,
/// causing TCP egress to be silently dropped.
///
/// The fix routes egress cross-shard via `CrossShardEvent::TcpEgressForward`.
#[test]
fn tcp_multi_shard_simulation_test() {
    common::setup_tracing();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(90))
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0xDEADBEEF)
        .build();

    Scenario::new("tcp_multi_shard_test", server_ip, sim)
        .add_stage(StartSfuTcpOnlyMultiShardStage)
        .add_stage(ConnectPeersTcpOnlyStage {
            peers: 4,
            min_tx_bytes: 0,
            min_rx_bytes: 1,
            max_wait: Duration::from_secs(30),
        })
        .add_stage(DisconnectStage {
            after: Duration::from_secs(50),
        })
        .add_stage(AssertAllDisconnectedStage {
            after: Duration::from_secs(70),
        })
        .run()
        .expect("TCP multi-shard simulation failed");
}
