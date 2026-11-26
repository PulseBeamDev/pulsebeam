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
        hierarchy: None,
    };
    install_nodes(
        &mut sim,
        root.path(),
        &ips,
        &EDGES,
        seed,
        None,
        RoutingFixture::Regional(&mut fixture),
    )?;
    sim.client(client, async move {
        for control in &mut fixture.streams {
            control.connect();
        }
        prove_weighted(client, ips, fixture.costs, &mut fixture.streams)
            .await
            .map_err(Into::into)
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(15))
        .map_err(|error| anyhow::anyhow!("regional weighted routes: {error:#}"))
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
        RoutingFixture::Regional(&mut fixture),
    )?;
    sim.client(client, async move {
        for control in &mut fixture.streams {
            control.connect();
        }
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
    let key = pulsebeam_server::insecure_dev_api_key()?;
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let signed = pulsebeam_server::sign_participant_token(
        &key,
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
        .subscribe(Some(core_ffi::DataSubscribeOptions {
            delivery: Some(core_ffi::DataDelivery::Latest),
        }))?)
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
