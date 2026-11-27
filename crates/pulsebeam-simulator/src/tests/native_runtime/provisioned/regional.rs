use std::{
    cell::Cell,
    num::{NonZeroU16, NonZeroUsize},
    rc::Rc,
};

use pulsebeam::cluster::{RegionTree, SimLinkCost, SimRegionalRole};
use pulsebeam_agent_native::ffi::{DataPublication, DataSubscription};
use pulsebeam_core::auth::ApiPublicKey;
use pulsebeam_link::{PeerControl, StreamControl};
use pulsebeam_runtime::sync::mpsc;

use super::*;

pub(super) struct Fixture {
    roles: Vec<SimRegionalRole>,
    costs: Vec<[SimLinkCost; 2]>,
    streams: Vec<ControlBridge>,
    hierarchy: Option<Hierarchy>,
    pub(super) protection: Option<ProtectionFixture>,
    pub(super) foreign_registry: Option<(usize, pulsebeam_core::auth::KeyRegistry)>,
}

pub(super) struct ProtectionFixture {
    pub(super) viewer: usize,
    pub(super) wire_bps: NonZeroU64,
    pub(super) constrained_hop: Option<(usize, usize)>,
    pub(super) control: Option<pulsebeam_runtime::sync::oneshot::Sender<pulsebeam::link::Control>>,
}

impl ProtectionFixture {
    pub(super) fn egress_bps(&self, source: usize, destination: usize) -> NonZeroU64 {
        if self.constrained_hop == Some((source, destination)) {
            NonZeroU64::MIN
        } else {
            self.wire_bps
        }
    }
}

struct Hierarchy {
    regions: &'static [(&'static str, Option<&'static str>, &'static [usize])],
    borders: &'static [(usize, usize)],
    owner: usize,
}

type Port = (
    usize,
    (SimRegionalRole, SimRegionalRole),
    SimLinkCost,
    mpsc::Receiver<PeerControl>,
);

impl Fixture {
    pub(super) fn cold_path(
        seed: u64,
        root: &std::path::Path,
        tls: pulsebeam_agent_quic::transport::ServerConfig,
    ) -> anyhow::Result<(turmoil::Sim<'static>, [IpAddr; 3], Self)> {
        use SimRegionalRole::{Gateway, Router};
        let mut fixture = Self {
            roles: vec![Router, Gateway, Gateway, Router],
            costs: vec![std::array::from_fn(|_| SimLinkCost::new(NonZeroU16::MIN)); 3],
            streams: Vec::new(),
            protection: None,
            foreign_registry: None,
            hierarchy: Some(Hierarchy {
                regions: &[
                    ("parent", None, &[0, 1]),
                    ("child", Some("parent"), &[2, 3]),
                ],
                borders: &[(2, 1)],
                owner: 0,
            }),
        };
        let subnet = reserve_subnet();
        let [publisher, border_a, border_b, viewer, client] =
            [1, 2, 3, 4, 5].map(|host| subnet_ip(subnet, host));
        let latency = Duration::from_millis(5);
        let mut sim = turmoil::Builder::new()
            .min_message_latency(latency)
            .max_message_latency(latency)
            .simulation_duration(Duration::from_secs(24))
            .rng_seed(seed)
            .build();
        install_nodes(
            &mut sim,
            root,
            &[publisher, border_a, border_b, viewer],
            &[(0, 1), (1, 2), (2, 3)],
            seed,
            None,
            RoutingFixture::Regional(&mut fixture, Some(tls)),
        )?;
        Ok((sim, [publisher, viewer, client], fixture))
    }

    pub(super) fn connect(&mut self) {
        for control in &mut self.streams {
            control.connect();
        }
    }

    pub(super) fn region_tree(
        &self,
        identity: impl Fn(usize) -> (NodeId, NonZeroU64, ApiPublicKey),
    ) -> anyhow::Result<Option<RegionTree>> {
        let Some(hierarchy) = &self.hierarchy else {
            return Ok(None);
        };
        let regions: Vec<_> = hierarchy.regions.iter().map(|&(name, parent, nodes)| {
            let members: Vec<_> = nodes.iter().map(|&index| {
                let (node, generation, key) = identity(index);
                let owner = hierarchy.owner == index;
                json!({"node":node, "generation":generation, "role":self.roles[index], "catalog":owner, "media_key": owner.then_some(key) })
            }).collect();
            json!({"name":name, "parent":parent, "members":members})
        }).collect();
        let borders: Vec<_> = hierarchy
            .borders
            .iter()
            .map(|&(child, parent)| json!({"child":identity(child).0, "parent":identity(parent).0}))
            .collect();
        Ok(Some(serde_json::from_value(
            json!({"regions":regions, "borders":borders}),
        )?))
    }

    pub(super) fn ports(&mut self, count: usize, edges: &[(usize, usize)]) -> Vec<Vec<Port>> {
        let mut ports: Vec<Vec<_>> = (0..count).map(|_| Vec::new()).collect();
        for (&(left, right), costs) in edges.iter().zip(&self.costs) {
            let (left_tx, left_rx) = mpsc::channel(1);
            let (right_tx, right_rx) = mpsc::channel(1);
            ports[left].push((
                right,
                (self.roles[left], self.roles[right]),
                costs[0].clone(),
                left_rx,
            ));
            ports[right].push((
                left,
                (self.roles[right], self.roles[left]),
                costs[1].clone(),
                right_rx,
            ));
            self.streams.push(ControlBridge {
                ports: [left_tx, right_tx],
                counts: ControlCounts::default(),
                task: None,
            });
        }
        ports
    }
}

type ControlCounts = Rc<Cell<[u64; 2]>>;

struct ControlBridge {
    ports: [mpsc::Sender<PeerControl>; 2],
    counts: ControlCounts,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ControlBridge {
    fn connect(&mut self) {
        let [left, right] = self.ports.each_ref().map(|port| {
            let (peer, stream) =
                StreamControl::new_authenticated(NonZeroUsize::new(64 * 1024).unwrap());
            assert!(port.try_send(peer).is_ok());
            stream
        });
        self.task = Some(tokio::task::spawn_local(bridge(
            left,
            right,
            self.counts.clone(),
        )));
    }

    async fn disconnect(&mut self) {
        let task = self.task.take().unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
}

async fn bridge(mut left: StreamControl, mut right: StreamControl, counts: ControlCounts) {
    let forwarded = |direction: usize| {
        let mut next = counts.get();
        next[direction] += 1;
        counts.set(next);
    };
    let (from_left, to_left) = left.directions();
    let (from_right, to_right) = right.directions();
    tokio::select! {
        () = async { while let Some((bytes, _)) = from_left.recv().await {
            if to_right.send(bytes).await.is_err() { break; }
            forwarded(0);
        } } => {},
        () = async { while let Some((bytes, _)) = from_right.recv().await {
            if to_left.send(bytes).await.is_err() { break; }
            forwarded(1);
        } } => {},
    }
}

const EDGES: [(usize, usize); 5] = [(0, 2), (0, 3), (0, 1), (1, 2), (1, 3)];
const ROOM: &str = "regional-weighted";

#[test]
fn regional_weighted_routes_aggregate_child_demand() -> anyhow::Result<()> {
    use SimRegionalRole::{Edge, Router};

    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(NATIVE_RUNTIME_SEED))?;
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    let subnet = reserve_subnet();
    let ips = [1, 2, 3, 4].map(|host| subnet_ip(subnet, host));
    let client = subnet_ip(subnet, 5);
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(65))
        .rng_seed(seed)
        .build();
    let root = tempfile::tempdir()?;
    let mut fixture = Fixture {
        roles: vec![Router, Router, Edge, Edge],
        costs: [100, 100, 80, 80, 80]
            .map(|cost| std::array::from_fn(|_| SimLinkCost::new(NonZeroU16::new(cost).unwrap())))
            .into(),
        streams: Vec::new(),
        protection: None,
        foreign_registry: None,
        hierarchy: None,
    };
    install_nodes(
        &mut sim,
        root.path(),
        &ips,
        &EDGES,
        seed,
        None,
        RoutingFixture::Regional(&mut fixture, None),
    )?;
    sim.client(client, async move {
        fixture.connect();
        prove_weighted(client, ips, fixture.costs, &mut fixture.streams)
            .await
            .map_err(Into::into)
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(15))
        .map_err(|error| anyhow::anyhow!("regional weighted routes: {error:#}"))
}

#[test]
fn regional_wan_does_not_route_foreign_project_demand_for_the_same_names() -> anyhow::Result<()> {
    use SimRegionalRole::{Edge, Router};
    use pulsebeam_core::{
        auth::{DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_PUBLIC_KEY, KeyRegistry, KeyRegistryEntry},
        identity::ProjectId,
    };

    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(NATIVE_RUNTIME_SEED))?;
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    let subnet = reserve_subnet();
    let ips = [1, 2, 3].map(|host| subnet_ip(subnet, host));
    let client = subnet_ip(subnet, 4);
    let root = tempfile::tempdir()?;
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(20))
        .rng_seed(seed)
        .build();
    let project = ProjectId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 55]);
    let dev_key = pulsebeam_server::insecure_dev_api_key()?;
    let (_, secret) = dev_key.rsplit_once('.').context("development key format")?;
    let foreign_key = format!("{project}.{}.{secret}", *DEVELOPMENT_API_KEY_ID);
    let registry = KeyRegistry::new(
        project,
        vec![KeyRegistryEntry {
            key_id: *DEVELOPMENT_API_KEY_ID,
            public_key: *DEVELOPMENT_API_PUBLIC_KEY,
        }],
    )?;
    let mut fixture = Fixture {
        roles: vec![Router, Edge, Edge],
        costs: vec![std::array::from_fn(|_| SimLinkCost::new(NonZeroU16::MIN)); 2],
        streams: Vec::new(),
        hierarchy: Some(Hierarchy {
            regions: &[("region", None, &[0, 1, 2])],
            borders: &[],
            owner: 0,
        }),
        protection: None,
        foreign_registry: Some((2, registry)),
    };
    install_nodes(
        &mut sim,
        root.path(),
        &ips,
        &[(0, 1), (0, 2)],
        seed,
        None,
        RoutingFixture::Regional(&mut fixture, None),
    )?;
    sim.client(client, async move {
        fixture.connect();
        let observe = async {
            let (publisher, _) = regional_agent(client, ips[0], "alice", &["join", "publish"])?;
            let source = publish_feed(&publisher).await?;
            let (foreign, runtime) =
                regional_agent_with_key(client, ips[2], "viewer", &["subscribe"], &foreign_key)?;
            let foreign_feed = subscribe_feed(&foreign)?;
            runtime.connect().await?;
            tokio::time::timeout(Duration::from_secs(5), wait_connected(&foreign, None))
                .await
                .context("foreign viewer did not connect")??;
            let (local, runtime) = regional_agent(client, ips[1], "viewer", &["subscribe"])?;
            let local_feed = subscribe_feed(&local)?;
            runtime.connect().await?;
            warm(&source, &local_feed, b"same-project-wan").await?;
            anyhow::ensure!(
                foreign_feed.state().source_track_id.is_none(),
                "foreign demand bound the other project's publication"
            );
            let counts = || {
                [ips[1], ips[2]].map(|viewer| pulsebeam::sim_metrics::wan_live_data(ips[0], viewer))
            };
            anyhow::ensure!(
                counts()[1].0 == 0,
                "foreign demand created a WAN data branch"
            );

            // Same application names and API verification key, but a different
            // project. Show the foreign viewer can receive its own source.
            let (foreign_publisher, _) = regional_agent_with_key(
                client,
                ips[2],
                "alice",
                &["join", "publish"],
                &foreign_key,
            )?;
            let foreign_source = publish_feed(&foreign_publisher).await?;
            warm(&foreign_source, &foreign_feed, b"foreign-project-local").await?;
            anyhow::ensure!(
                local_feed.state().source_track_id != foreign_feed.state().source_track_id
            );
            probe(
                &source,
                &[&local_feed],
                b"after-foreign-publish",
                counts,
                [1, 0],
            )
            .await
        };
        observe.await.map_err(Into::into)
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(10))
        .map_err(|error| anyhow::anyhow!("regional project isolation: {error:#}"))
}

#[test]
fn regional_h264_protected_pair_withdraws_on_standby_loss() -> anyhow::Result<()> {
    prove_h264_protection(ProtectionCase::StandbyLoss)
}

#[test]
fn regional_h264_protected_playback_survives_abrupt_primary_loss() -> anyhow::Result<()> {
    prove_h264_protection(ProtectionCase::PrimaryLoss)
}

#[test]
fn regional_h264_protected_playback_survives_planned_primary_drain() -> anyhow::Result<()> {
    prove_h264_protection(ProtectionCase::PlannedDrain)
}

#[test]
fn regional_h264_protected_repair_uses_an_independent_relay() -> anyhow::Result<()> {
    prove_h264_protection(ProtectionCase::IndependentRepair)
}

#[test]
fn regional_h264_protected_pair_reports_isolation_and_recovers_degraded() -> anyhow::Result<()> {
    prove_h264_protection(ProtectionCase::TotalIsolation)
}

#[test]
fn regional_h264_protected_pair_refuses_a_constrained_hop() -> anyhow::Result<()> {
    prove_h264_protection(ProtectionCase::InsufficientCapacity)
}

#[test]
fn regional_h264_protected_late_viewer_decodes_without_periodic_idr() -> anyhow::Result<()> {
    prove_h264_protection(ProtectionCase::LateViewer)
}

#[derive(Clone, Copy, Debug)]
enum ProtectionCase {
    LateViewer,
    StandbyLoss,
    PrimaryLoss,
    PlannedDrain,
    IndependentRepair,
    TotalIsolation,
    InsufficientCapacity,
}

fn prove_h264_protection(scenario: ProtectionCase) -> anyhow::Result<()> {
    use crate::tests::{
        native_state::spatial_vp9::{expect_height, open_logical_h264},
        quic_data::{
            dd_video::{
                Graph, TRACK, connect_dd_publisher, replay_graph, replay_graph_on_feedback,
            },
            sim_quic_tls,
        },
    };
    use SimRegionalRole::{Edge, Router};
    use pulsebeam::entity::{ProjectRoomKey, PublicationKey, RoomId, TrackKind};
    use pulsebeam_agent_quic::media::units::Codec;

    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(NATIVE_RUNTIME_SEED))?;
    let _clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    let subnet = reserve_subnet();
    let ips = [1, 2, 3, 4].map(|host| subnet_ip(subnet, host));
    let client = subnet_ip(subnet, 5);
    let latency = Duration::from_millis(5);
    let mut sim = turmoil::Builder::new()
        .min_message_latency(latency)
        .max_message_latency(latency)
        .simulation_duration(Duration::from_secs(24))
        .rng_seed(seed)
        .build();
    let root = tempfile::tempdir()?;
    let (tls, roots) = sim_quic_tls()?;
    let (control, inspect) = pulsebeam_runtime::sync::oneshot::channel();
    let mut fixture = Fixture {
        roles: vec![Router, Router, Router, Edge],
        costs: [30, 60, 30, 60]
            .map(|cost| std::array::from_fn(|_| SimLinkCost::new(NonZeroU16::new(cost).unwrap())))
            .into(),
        streams: Vec::new(),
        hierarchy: Some(Hierarchy {
            regions: &[("region", None, &[0, 1, 2, 3])],
            borders: &[],
            owner: 0,
        }),
        foreign_registry: None,
        protection: Some(ProtectionFixture {
            viewer: 3,
            wire_bps: NonZeroU64::new(2_000_000).unwrap(),
            constrained_hop: matches!(scenario, ProtectionCase::InsufficientCapacity)
                .then_some((2, 3)),
            control: Some(control),
        }),
    };
    let nodes = install_nodes(
        &mut sim,
        root.path(),
        &ips,
        &[(0, 1), (0, 2), (1, 3), (2, 3)],
        seed,
        None,
        RoutingFixture::Regional(&mut fixture, Some(tls)),
    )?;
    let key = pulsebeam_server::insecure_dev_api_key()?;
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        + 60;
    let token = |subject, allow: &[&str]| {
        pulsebeam_server::sign_participant_token(
            &key,
            &serde_json::from_value(json!([{"rooms":["stage"],"allow":allow}]))?,
            subject,
            expires,
        )
    };
    let publisher_name = "video-publisher";
    let publish = token(publisher_name, &["join", "publish"])?;
    sim.client(client, async move {
        fixture.connect();
        let inspect = inspect.await?;
        let (mut publisher, source) = connect_dd_publisher(
            (ips[0], 7070).into(),
            client,
            (ips[0], 3478).into(),
            roots,
            &publish.token,
            Codec::H264,
        )
        .await?;
        let id = source.publication_id();
        let room = ProjectRoomKey {
            project: *pulsebeam_core::auth::DEVELOPMENT_PROJECT_ID,
            room: RoomId::new("stage")?,
        };
        let publication = PublicationKey::new(TrackKind::Video, id.parse()?);
        let graph = Graph::H264Temporal;
        let send = async {
            if matches!(scenario, ProtectionCase::LateViewer) {
                replay_graph_on_feedback(&mut publisher, &source, graph).await
            } else {
                replay_graph(&mut publisher, &source, graph).await
            }
        };
        let observe = async {
            anyhow::ensure!(
                !inspect
                    .media_protection(room, publication)
                    .await
                    .map_err(relay_error)?
            );
            let (mut viewer, sink) = open_logical_h264(
                (ips[3], 7070).into(),
                NativeHost::bind(client)?,
                "video-viewer",
                publisher_name,
                TRACK,
                id,
            )
            .await?;
            expect_height(&sink, "H264 primary", 720, Duration::from_secs(4), true).await?;
            if matches!(scenario, ProtectionCase::LateViewer) {
                let refreshes = || pulsebeam::sim_metrics::routing_counter("fabric_wan_h264_refresh");
                let before = refreshes();
                let (_late, late_sink) = open_logical_h264(
                    (ips[3], 7070).into(),
                    NativeHost::bind(client)?,
                    "late-video-viewer",
                    publisher_name,
                    TRACK,
                    id,
                )
                .await?;
                expect_height(&late_sink, "feedback-only H264 late viewer", 720, Duration::from_secs(4), true).await?;
                anyhow::ensure!(refreshes() > before, "late viewer did not refresh the remote publisher");
                return Ok(());
            }
            let wait = |expected| {
                let inspect = &inspect;
                async move {
                    loop {
                        if inspect
                            .media_protection(room, publication)
                            .await
                            .map_err(relay_error)?
                            == expected
                        {
                            return anyhow::Ok(());
                        }
                        pulsebeam_runtime::time::sleep(Duration::from_millis(25)).await;
                    }
                }
            };
            use pulsebeam_runtime::time::{Instant, sleep, timeout};
            if !matches!(scenario, ProtectionCase::InsufficientCapacity) {
                timeout(Duration::from_secs(4), wait(true))
                    .await.context("independent H264 paths never became warm")??;
            }
            let frame = || timeout(Duration::from_millis(350), async {
                loop {
                    if let Some(frame) = sink.try_next_frame() {
                        return frame;
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            });
            match scenario {
                ProtectionCase::LateViewer => unreachable!("handled before warming the pair"),
                ProtectionCase::InsufficientCapacity => {
                    anyhow::ensure!(
                        !inspect.drain_media_relay(room, publication, nodes[1]).await.map_err(relay_error)?,
                        "drained a live primary with no qualified replacement"
                    );
                    for _ in 0..40 {
                        anyhow::ensure!(
                            !inspect.media_protection(room, publication).await.map_err(relay_error)?,
                            "underprovisioned hop admitted protection"
                        );
                        sleep(Duration::from_millis(25)).await;
                    }
                }
                ProtectionCase::IndependentRepair => {
                    use pulsebeam::sim_metrics::wan_repair;
                    wan_repair::arm_protected_h264(
                        ((ips[1], 7445).into(), (ips[3], 7445).into()),
                        ((ips[2], 7446).into(), (ips[3], 7446).into()),
                    );
                    timeout(Duration::from_secs(3), async {
                        loop {
                            if let Some(report) = wan_repair::protected_report()
                                && report.independent_offers != 0
                            {
                                break report;
                            }
                            sleep(Duration::from_millis(5)).await;
                        }
                    }).await.map_err(|_| anyhow::anyhow!(
                        "exact H264 repair did not arrive over the independent relay: {:?}",
                        wan_repair::protected_report(),
                    )).and_then(|report| {
                        anyhow::ensure!(report.dropped_live[0] >= 2 && report.dropped_live[1] == 1,
                            "all copies of the exact position must be lost: {report:?}");
                        anyhow::ensure!(report.independent_offers == 1 && report.repaired_timestamp == Some(report.timestamp),
                            "repair did not fill the original position exactly once: {report:?}");
                        Ok(())
                    })?;
                }
                ProtectionCase::TotalIsolation => {
                    for relay in [ips[1], ips[2]] {
                        turmoil::partition(relay, ips[3]);
                    }
                    timeout(Duration::from_secs(2), wait(false))
                        .await.context("protection remained admitted in isolation")??;
                    while sink.try_next_frame().is_some() {}
                    sleep(Duration::from_millis(400)).await;
                    anyhow::ensure!(sink.try_next_frame().is_none(), "viewer decoded during total WAN isolation");
                    // Restore only the primary. Playback may recover, but the
                    // still-isolated standby cannot justify protection again.
                    turmoil::repair(ips[1], ips[3]);
                }
                ProtectionCase::StandbyLoss => {
                    while sink.try_next_frame().is_some() {}
                    let mut previous = frame().await.context("no H264 picture before churn")?.rtp_timestamp;
                    for control_cut in [true, false, false] {
                        let transition = async {
                            if control_cut {
                                fixture.streams[3].disconnect().await;
                            } else {
                                turmoil::partition_oneway(ips[2], ips[3]);
                            }
                            timeout(Duration::from_secs(2), wait(false))
                                .await.context("protection stayed admitted without a fresh standby")??;
                            if control_cut {
                                fixture.streams[3].connect();
                            } else {
                                turmoil::repair_oneway(ips[2], ips[3]);
                            }
                            timeout(Duration::from_secs(4), wait(true))
                                .await.context("protection did not rewarm after standby rejoined")??;
                            anyhow::Ok(())
                        };
                        let playback = async {
                            loop {
                                let picture = frame().await.context("H264 decode deadline during standby churn")?;
                                let delta = picture.rtp_timestamp.wrapping_sub(previous);
                                anyhow::ensure!(picture.height == 720 && matches!(delta, 3000 | 6000 | 9000 | 12000),
                                    "H264 playback missed or rewound a base interval during churn: delta={delta}");
                                previous = picture.rtp_timestamp;
                            }
                        };
                        pulsebeam_runtime::select! { result = transition => result, result = playback => result }?;
                    }
                }
                ProtectionCase::PrimaryLoss | ProtectionCase::PlannedDrain => {
                        while sink.try_next_frame().is_some() {}
                        let mut timestamp = frame().await.context("no H264 picture before primary loss")?.rtp_timestamp;
                        let cut = Instant::now();
                        if matches!(scenario, ProtectionCase::PlannedDrain) {
                            anyhow::ensure!(inspect.drain_media_relay(room, publication, nodes[1]).await.map_err(relay_error)?,
                                "refused to drain a primary with a qualified replacement");
                            anyhow::ensure!(!inspect.media_protection(room, publication).await.map_err(relay_error)?,
                                "drained leg was still admitted");
                            timestamp = frame().await.context("H264 playback stalled while the primary relay was draining")?.rtp_timestamp;
                        }
                        tracing::debug!(target: "pulsebeam_protection", timestamp, "cutting primary WAN path");
                        turmoil::partition(ips[1], ips[3]);
                        let mut control_disconnected = false;
                        while Instant::now() - cut < Duration::from_secs(2) {
                            // Hold the ordinary route on the failed relay long
                            // enough that only the preinstalled copy can bridge it.
                            if !control_disconnected && Instant::now() - cut >= Duration::from_millis(500) {
                                fixture.streams[2].disconnect().await;
                                control_disconnected = true;
                            }
                            let picture = frame().await.context("H264 playback stalled after primary loss")?;
                            let delta = picture.rtp_timestamp.wrapping_sub(timestamp);
                            tracing::debug!(target: "pulsebeam_protection", timestamp = picture.rtp_timestamp, delta, "decoded picture after the cut");
                            anyhow::ensure!(picture.height == 720 && matches!(delta, 3000 | 6000 | 9000 | 12000),
                                "H264 playback skipped a base interval, duplicated or rewound: delta={delta}");
                            timestamp = picture.rtp_timestamp;
                        }
                    timeout(Duration::from_secs(2), wait(false))
                        .await.context("protection remained admitted after path loss")??;
                }
            }
            while sink.try_next_frame().is_some() {}
            expect_height(
                &sink,
                "H264 playback after the protection scenario",
                720,
                Duration::from_secs(3),
                true,
            )
            .await?;
            if matches!(scenario, ProtectionCase::TotalIsolation) {
                anyhow::ensure!(!inspect.media_protection(room, publication).await.map_err(relay_error)?,
                    "a single restored path readmitted protection");
            }
            if matches!(scenario, ProtectionCase::StandbyLoss) {
                let traffic = || pulsebeam::sim_metrics::routing_counter("fabric_wan_protected_sent");
                anyhow::ensure!(traffic() > 0, "the warmed pair sent no protected packets");
                timeout(Duration::from_secs(2), viewer.remove_owned(1, 3))
                    .await.context("subscriber withdrawal was not confirmed")??;
                timeout(Duration::from_secs(4), wait(false))
                    .await.context("protection remained admitted without a subscriber")??;
                timeout(Duration::from_secs(4), async {
                    let mut counters = traffic();
                    let mut quiet_since = Instant::now();
                    while quiet_since.elapsed() < Duration::from_millis(500) {
                        sleep(Duration::from_millis(25)).await;
                        let current = traffic();
                        if current != counters {
                            quiet_since = Instant::now();
                            counters = current;
                        }
                    }
                }).await.context("WAN traffic did not quiesce after subscriber withdrawal")?;
            }
            Ok(())
        };
        pulsebeam_runtime::select! { result = observe => result, result = send => result }
            .map_err(Into::into)
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(20))
        .map_err(|error| anyhow::anyhow!("regional H264 protection ({scenario:?}): {error:#}"))
}

const SIBLING_EDGES: [(usize, usize); 7] = [(0, 1), (1, 2), (2, 3), (2, 4), (3, 5), (4, 5), (5, 6)];

#[test]
fn sibling_regions_share_a_wan_branch_and_reroute_without_replacing_the_source()
-> anyhow::Result<()> {
    use SimRegionalRole::{Edge, Gateway, Router};

    let seed = std::env::var("PULSEBEAM_SIM_SEED")
        .map(|value| value.parse::<u64>())
        .unwrap_or(Ok(NATIVE_RUNTIME_SEED))?;
    let _sim_clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    let subnet = reserve_subnet();
    let ips = [1, 2, 3, 4, 5, 6, 7].map(|host| subnet_ip(subnet, host));
    let client = subnet_ip(subnet, 8);
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(48))
        .rng_seed(seed)
        .build();
    let root = tempfile::tempdir()?;
    let mut fixture = Fixture {
        roles: vec![Router, Gateway, Gateway, Gateway, Gateway, Router, Edge],
        costs: (0..SIBLING_EDGES.len())
            .map(|_| std::array::from_fn(|_| SimLinkCost::new(NonZeroU16::MIN)))
            .collect(),
        streams: Vec::new(),
        protection: None,
        foreign_registry: None,
        hierarchy: Some(Hierarchy {
            regions: &[
                ("parent", None, &[2]),
                ("left", Some("parent"), &[0, 1]),
                ("right", Some("parent"), &[3, 4, 5, 6]),
            ],
            borders: &[(1, 2), (3, 2), (4, 2)],
            owner: 0,
        }),
    };
    install_nodes(
        &mut sim,
        root.path(),
        &ips,
        &SIBLING_EDGES,
        seed,
        None,
        RoutingFixture::Regional(&mut fixture, None),
    )?;
    sim.client(client, async move {
        fixture.connect();
        prove_siblings(client, ips, &mut fixture.streams)
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}").into())
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(15))
        .map_err(|error| anyhow::anyhow!("sibling media: {error:#}"))
}

async fn prove_siblings(
    client: IpAddr,
    ips: [IpAddr; 7],
    controls: &mut [ControlBridge],
) -> anyhow::Result<()> {
    let counters = || {
        SIBLING_EDGES.map(|(from, to)| pulsebeam::sim_metrics::wan_live_data(ips[from], ips[to]))
    };
    let (publisher, _) = regional_agent(client, ips[0], "alice", &["join", "publish"])?;
    let source = publish_feed(&publisher).await?;
    probe(&source, &[], b"no-demand", counters, [0; 7]).await?;

    let (viewer_a, runtime_a) = regional_agent(client, ips[5], "viewer-a", &["subscribe"])?;
    let (viewer_b, runtime_b) = regional_agent(client, ips[6], "viewer-b", &["subscribe"])?;
    let a = subscribe_feed(&viewer_a)?;
    let b = subscribe_feed(&viewer_b)?;
    let empty = viewer_a
        .room(ROOM.into())?
        .video(Some(pulsebeam_agent_core::TrackFilter {
            participant_id: Some("alice".into()),
            track_id: Some("no-camera".into()),
        }))
        .subscribe()?;
    anyhow::ensure!(empty.state().status == pulsebeam_agent_core::CollectionStatus::Waiting);
    let empty_updates = empty.updates()?;
    runtime_a.connect().await?;
    runtime_b.connect().await?;
    warm(&source, &a, b"ready-a").await?;
    warm(&source, &b, b"ready-b").await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(state) = empty_updates.next().await? {
            anyhow::ensure!(state.tracks.is_empty() && state.error.is_none());
            if state.status == pulsebeam_agent_core::CollectionStatus::Live {
                return anyhow::Ok(());
            }
        }
        anyhow::bail!("empty collection closed before regional confirmation")
    })
    .await
    .context("confirm authorized empty regional scope")??;
    empty_updates.close();
    empty.close();
    let before = counters();
    let primary = match (before[2].0 > 0, before[3].0 > 0) {
        (true, false) => 2,
        (false, true) => 3,
        _ => anyhow::bail!("expected just one active sibling border: {before:?}"),
    };
    let pattern = |border, edge_viewer| {
        let mut sends = [1, 1, 0, 0, 0, 0, edge_viewer];
        sends[border] = 1;
        sends[border + 2] = 1;
        sends
    };
    probe(
        &source,
        &[&a, &b],
        b"shared-upstream",
        counters,
        pattern(primary, 1),
    )
    .await?;
    let generations = (
        viewer_a.snapshot().generation,
        viewer_b.snapshot().generation,
    );
    let (local_viewer, local_runtime) =
        regional_agent(client, ips[0], "local-video-viewer", &["subscribe"])?;
    local_runtime.connect().await?;
    prove_video_recovery(&publisher, &local_viewer, &viewer_a, ips, controls, primary).await?;
    warm(&source, &a, b"failover-a").await?;
    warm(&source, &b, b"failover-b").await?;
    let alternate = 5 - primary;
    probe(
        &source,
        &[&a, &b],
        b"alternate-border",
        counters,
        pattern(alternate, 1),
    )
    .await?;
    anyhow::ensure!(
        (
            viewer_a.snapshot().generation,
            viewer_b.snapshot().generation
        ) == generations
    );
    b.close().await?;
    tokio::time::sleep(Duration::from_millis(1400)).await;
    probe(
        &source,
        &[&a],
        b"remaining-viewer",
        counters,
        pattern(alternate, 0),
    )
    .await?;
    a.close().await?;
    tokio::time::sleep(Duration::from_millis(1400)).await;
    probe(&source, &[], b"all-withdrawn", counters, [0; 7]).await
}

async fn prove_video_recovery(
    publisher: &Agent,
    local_viewer: &Agent,
    viewer: &Agent,
    ips: [IpAddr; 7],
    controls: &mut [ControlBridge],
    cut: usize,
) -> anyhow::Result<()> {
    use pulsebeam_testdata::QualityVideoSource::{One, Zero};

    let publication = publisher.video("camera".into())?.publish(None)?;
    let input = publisher.create_video_input(Default::default()).await?;
    let _pipe = input.pipe_to(&publication)?;
    let subscribe = |viewer: &Agent| -> anyhow::Result<_> {
        Ok(viewer
            .room(ROOM.into())?
            .participant("alice".into())?
            .video("camera".into())?
            .subscribe(None)?)
    };
    let local = subscribe(local_viewer)?;
    let generation = viewer.snapshot().generation.context("viewer generation")?;
    let pictures = [StandbyPicture::new(Zero)?, StandbyPicture::new(One)?];
    let selected = Cell::new(0);
    let pump = async {
        loop {
            input
                .submit(pictures[selected.get()].input(input.capture_time_us().await?))
                .await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let scenario = async {
        tokio::time::timeout(Duration::from_secs(5), publication.ready())
            .await
            .context("install sibling video sender")??;
        wait_mixed_video_installed(&local)
            .await
            .context("install local camera demand")?;
        let local_generation = local_viewer
            .snapshot()
            .generation
            .context("local video viewer generation")?;
        let local_frames = local.frames()?;
        decode_standby_picture(|| local_frames.next(), local_generation, &pictures, 0)
            .await
            .context("establish live publisher video before remote demand")?;
        let collection = viewer
            .room(ROOM.into())?
            .video(Some(pulsebeam_agent_core::TrackFilter {
                participant_id: Some("alice".into()),
                track_id: Some("camera".into()),
            }))
            .subscribe()?;
        let updates = collection.updates()?;
        let source = publication
            .state()
            .admission
            .publication_id
            .context("camera publication ID")?;
        let next_camera = async || {
            while let Some(state) = updates.next().await? {
                anyhow::ensure!(state.error.is_none() && state.tracks.len() <= 1);
                if let Some(camera) = state.tracks.into_iter().next() {
                    anyhow::ensure!(camera.publication_id == source);
                    return Ok(camera.track.frames()?);
                }
            }
            anyhow::bail!("collection closed before camera admission")
        };
        let frames = tokio::time::timeout(Duration::from_secs(5), next_camera())
            .await
            .context("install remote sibling camera collection")??;
        let before = decode_standby_picture(|| frames.next(), generation, &pictures, 0)
            .await
            .context("decode before border failover")?;
        let (from, to) = SIBLING_EDGES[cut];
        turmoil::partition(ips[from], ips[to]);
        controls[cut].disconnect().await;
        // Only post-fault submissions contain this picture; buffered pre-fault output cannot pass.
        selected.set(1);
        let mut rebound: Option<Arc<pulsebeam_agent_native::ffi::VideoFrames>> = None;
        let after = decode_picture_within(
            async || {
                if let Some(reader) = &rebound {
                    return reader.next().await;
                }
                if let Some(picture) = frames.next().await? {
                    return Ok(Some(picture));
                }
                // A retired reader cannot reacquire on its old admission.
                let reader = next_camera().await.map_err(|error| {
                    pulsebeam_agent_native::ffi::NativeError::Operation {
                        info: pulsebeam_agent_core::ffi::ErrorInfo::new(
                            pulsebeam_agent_core::ffi::ErrorCode::Internal,
                            &format!("collection readmission: {error:#}"),
                        ),
                    }
                })?;
                rebound.insert(reader).next().await
            },
            generation,
            &pictures,
            1,
            Duration::from_secs(8),
        )
        .await
        .context("decode fresh video on the surviving border")?;
        if rebound.is_none() {
            anyhow::ensure!(before == after, "active frame stream changed receivers");
        }
        let state = collection.state();
        anyhow::ensure!(state.tracks.len() == 1 && state.tracks[0].publication_id == source);
        collection.close();
        let active = rebound.as_ref().unwrap_or(&frames);
        anyhow::ensure!(
            tokio::time::timeout(Duration::from_secs(1), active.next())
                .await??
                .is_none()
        );
        updates.close();
        anyhow::Ok(())
    };
    tokio::select! { result = pump => result, result = scenario => result }
}

fn regional_agent(
    client: IpAddr,
    server: IpAddr,
    subject: &str,
    allow: &[&str],
) -> anyhow::Result<(Arc<Agent>, RuntimeAgent)> {
    regional_agent_with_key(
        client,
        server,
        subject,
        allow,
        &pulsebeam_server::insecure_dev_api_key()?,
    )
}

fn regional_agent_with_key(
    client: IpAddr,
    server: IpAddr,
    subject: &str,
    allow: &[&str],
    key: &str,
) -> anyhow::Result<(Arc<Agent>, RuntimeAgent)> {
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let signed = pulsebeam_server::sign_participant_token(
        key,
        &serde_json::from_value(json!([{ "rooms": [ROOM], "allow": allow }]))?,
        subject,
        wall + 60,
    )?;
    let mut config = Config::new(pulsebeam_agent_core::AgentConfig {
        endpoint: format!("http://{server}:7070"),
        token: signed.token,
        retry: Default::default(),
        log_level: Default::default(),
    });
    if allow.contains(&"publish") {
        config.media.video_input = VideoInput::EncodedH264;
    }
    let host = NativeHost::bind(client)?;
    let (runtime, actor) =
        RuntimeAgent::managed_with_services(config, Host::new(create_http_client()), move || {
            host.runtime()
        })?;
    tokio::task::spawn_local(actor);
    Ok((Agent::from_runtime(runtime.clone()), runtime))
}

async fn publish_feed(publisher: &Agent) -> anyhow::Result<Arc<DataPublication>> {
    let source = publisher
        .data("feed".into())?
        .publish(Some(core_ffi::DataPublishOptions {
            delivery: Some(core_ffi::DataDelivery::Latest),
            ..Default::default()
        }))?;
    publisher.join(ROOM.into())?;
    wait_connected(&publisher, None).await?;
    let updates = source.updates()?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(state) = updates.next().await? {
            if state.execution == core_ffi::PublicationExecution::Installed {
                return Ok(());
            }
        }
        anyhow::bail!("publisher closed")
    })
    .await
    .context("publisher never installed")??;
    Ok(source)
}

fn subscribe_feed(viewer: &Agent) -> anyhow::Result<Arc<DataSubscription>> {
    Ok(viewer
        .room(ROOM.into())?
        .participant("alice".into())?
        .data("feed".into())?
        .subscribe()?)
}

async fn prove_weighted(
    client: IpAddr,
    ips: [IpAddr; 4],
    costs: Vec<[SimLinkCost; 2]>,
    control: &mut [ControlBridge],
) -> anyhow::Result<()> {
    let agent_for =
        |index: usize, subject, allow| regional_agent(client, ips[index], subject, allow);
    let (publisher, _) = agent_for(0, "alice", &["join", "publish"][..])?;
    let source = publish_feed(&publisher).await?;
    let counters =
        || EDGES.map(|(from, to)| pulsebeam::sim_metrics::wan_live_data(ips[from], ips[to]));
    let idle = counters();
    source.send(b"idle".to_vec(), None).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    anyhow::ensure!(
        counters() == idle,
        "metadata alone must not install a WAN branch"
    );

    let (a, a_runtime) = agent_for(2, "viewer-a", &["subscribe"])?;
    let feed_a = subscribe_feed(&a)?;
    a_runtime.connect().await?;
    warm(&source, &feed_a, b"direct-ready").await?;
    probe(&source, &[&feed_a], b"direct", counters, [1, 0, 0, 0, 0]).await?;

    for cost in costs.iter().skip(2).flatten() {
        cost.set(NonZeroU16::new(30).unwrap());
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        for sequence in 0u8.. {
            let payload = [b'h', sequence];
            let before = counters();
            source.send(payload.to_vec(), None).await?;
            tokio::time::timeout(Duration::from_millis(250), expect(&feed_a, &payload))
                .await
                .context("viewer lost media during the same-source handoff")??;
            tokio::time::sleep(Duration::from_millis(80)).await;
            let after = counters();
            if after[3].0 > before[3].0 {
                anyhow::ensure!(
                    after[0].0 == before[0].0 + 1,
                    "the first relay packet must overlap the direct path"
                );
                anyhow::ensure!(
                    after[3].1 == after[0].1,
                    "overlap must carry the same source position"
                );
                anyhow::ensure!(
                    tokio::time::timeout(Duration::from_millis(80), expect(&feed_a, &payload))
                        .await
                        .is_err(),
                    "overlap decoded twice"
                );
                return Ok::<_, anyhow::Error>(());
            }
        }
        unreachable!()
    })
    .await
    .context("replacement WAN path did not overlap the incumbent")??;
    // Let withdrawal reach the old sender before measuring steady-state fanout.
    tokio::time::sleep(Duration::from_millis(800)).await;
    warm(&source, &feed_a, b"relay-ready").await?;
    probe(&source, &[&feed_a], b"relay-a", counters, [0, 0, 1, 1, 0]).await?;

    let (b, b_runtime) = agent_for(3, "viewer-b", &["subscribe"])?;
    let feed_b = subscribe_feed(&b)?;
    b_runtime.connect().await?;
    warm(&source, &feed_b, b"second-ready").await?;
    probe(
        &source,
        &[&feed_a, &feed_b],
        b"both",
        counters,
        [0, 0, 1, 1, 1],
    )
    .await?;

    feed_a.close().await?;
    // Observe child withdrawal before the independently expiring WAN windows roll over.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    probe(&source, &[&feed_b], b"relay-b", counters, [0, 0, 1, 0, 1]).await?;

    // Also verify forwarding recovers after the original upstream receive lease expires.
    tokio::time::sleep(Duration::from_secs(4)).await;
    warm(&source, &feed_b, b"refresh-ready").await?;
    probe(&source, &[&feed_b], b"refreshed", counters, [0, 0, 1, 0, 1]).await?;

    let upstream_control = &control[2].counts;
    let control_before = upstream_control.get();
    let viewer_generation = b.snapshot().generation;
    turmoil::partition(ips[0], ips[1]);
    tokio::time::sleep(Duration::from_secs(7)).await;
    probe(&source, &[], b"blackholed", counters, [0; 5]).await?;
    anyhow::ensure!(
        upstream_control
            .get()
            .into_iter()
            .zip(control_before)
            .all(|(after, before)| after > before),
        "the same regional streams must stay active when the media path is blackholed"
    );
    turmoil::repair(ips[0], ips[1]);
    warm(&source, &feed_b, b"path-restored").await?;
    probe(
        &source,
        &[&feed_b],
        b"after-blackhole",
        counters,
        [0, 0, 1, 0, 1],
    )
    .await?;
    anyhow::ensure!(b.snapshot().generation == viewer_generation);

    // Close only the regional stream. Installed media can drain until its
    // existing deadline, but the stream must reconnect to renew authority.
    let downstream_control = &mut control[4];
    downstream_control.disconnect().await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    probe(
        &source,
        &[&feed_b],
        b"control-disconnected",
        counters,
        [0, 0, 1, 0, 1],
    )
    .await?;
    let control_before = downstream_control.counts.get();
    downstream_control.connect();
    tokio::time::sleep(Duration::from_secs(2)).await;
    anyhow::ensure!(
        downstream_control
            .counts
            .get()
            .into_iter()
            .zip(control_before)
            .all(|(after, before)| after > before)
    );
    warm(&source, &feed_b, b"control-reconnected").await?;
    probe(
        &source,
        &[&feed_b],
        b"control-restored",
        counters,
        [0, 0, 1, 0, 1],
    )
    .await?;
    anyhow::ensure!(b.snapshot().generation == viewer_generation);

    feed_b.close().await?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    probe(&source, &[], b"retired", counters, [0; 5]).await
}

async fn expect(feed: &DataSubscription, payload: &[u8]) -> anyhow::Result<()> {
    use pulsebeam_agent_core::DataEvent;
    while let Some(event) = feed.next().await? {
        if let DataEvent::Message {
            participant_id,
            data,
            ..
        } = event
            && data == payload
        {
            anyhow::ensure!(participant_id == "alice");
            return Ok(());
        }
    }
    anyhow::bail!("regional subscription closed")
}

async fn warm(
    source: &DataPublication,
    feed: &DataSubscription,
    payload: &[u8],
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(9), async {
        let mut ticks = tokio::time::interval(Duration::from_millis(75));
        let delivered = expect(feed, payload);
        tokio::pin!(delivered);
        loop {
            tokio::select! {
                result = &mut delivered => return result,
                _ = ticks.tick() => { source.send(payload.to_vec(), None).await?; }
            }
        }
    })
    .await
    .with_context(|| {
        format!(
            "regional branch did not become ready for {}: {:?}",
            String::from_utf8_lossy(payload),
            feed.state()
        )
    })??;
    tokio::time::sleep(Duration::from_millis(250)).await;
    Ok(())
}

async fn probe<const N: usize>(
    source: &DataPublication,
    feeds: &[&DataSubscription],
    payload: &[u8],
    counters: impl Fn() -> [(u64, u64, u32); N],
    expected: [u64; N],
) -> anyhow::Result<()> {
    let before = counters();
    source.send(payload.to_vec(), None).await?;
    for feed in feeds {
        tokio::time::timeout(Duration::from_secs(2), expect(feed, payload))
            .await
            .with_context(|| {
                format!(
                    "regional probe {} was not delivered; handoffs before={before:?} after={:?}; subscription={:?}",
                    String::from_utf8_lossy(payload), counters(), feed.state()
                )
            })??;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    let after = counters();
    let actual = std::array::from_fn(|i| after[i].0 - before[i].0);
    anyhow::ensure!(
        actual == expected,
        "WAN handoffs for {}: expected {expected:?}, got {actual:?}",
        String::from_utf8_lossy(payload)
    );
    let mut traversed =
        after
            .into_iter()
            .zip(expected)
            .filter_map(|((_, position, timestamp), count)| {
                (count != 0).then_some((position, timestamp))
            });
    if let Some(first) = traversed.next() {
        anyhow::ensure!(
            traversed.all(|unit| unit == first),
            "relay must preserve position and timestamp"
        );
    }
    Ok(())
}
