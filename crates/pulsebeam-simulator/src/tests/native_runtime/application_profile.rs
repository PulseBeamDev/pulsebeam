use std::cell::RefCell;
use std::collections::BTreeMap;

use futures::future::try_join_all;
use pulsebeam_agent_core::CollectionStatus;
use pulsebeam_agent_native::ffi::{VideoCollection, VideoPublication};
use tokio::sync::{mpsc, oneshot};

use super::*;

const ROOM: &str = "application-profile";
const BUDGET: usize = pulsebeam_proto::state::validation::MAX_SELECTOR_MATCHES;
const TRACKS_PER_HOST: usize = 8;
const EXACT_WINDOW: usize = 8;
type Name = (String, String);
type Admissions = BTreeMap<Name, String>;
type Readers = BTreeMap<Name, Arc<VideoSubscription>>;

async fn agent(
    host: &NativeHost,
    server: IpAddr,
    id: &str,
    action: &str,
) -> anyhow::Result<Arc<Agent>> {
    let token = pulsebeam_server::sign_participant_token(
        &pulsebeam_server::insecure_dev_api_key().unwrap(),
        &serde_json::from_value(serde_json::json!([{"rooms":[ROOM],"allow":["join",action]}]))?,
        id,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            + 3600,
    )?
    .token;
    let host = host.clone();
    let (runtime, actor) = RuntimeAgent::managed_with_services(
        Config::new(pulsebeam_agent_core::AgentConfig {
            endpoint: format!("http://{server}:7070"),
            token,
            retry: Default::default(),
            log_level: Default::default(),
        }),
        Host::new(create_http_client()),
        move || host.runtime(),
    )?;
    tokio::task::spawn_local(actor);
    let agent = Agent::from_runtime(runtime);
    agent.join(ROOM.into())?;
    wait_connected(&agent, None)
        .await
        .with_context(|| format!("{id} did not connect"))?;
    Ok(agent)
}

enum Publish {
    Start(String, oneshot::Sender<String>),
    Stop(String, oneshot::Sender<()>),
}

struct Feed {
    name: Name,
    owner: mpsc::Sender<Publish>,
}

impl Feed {
    async fn start(&self) -> anyhow::Result<String> {
        let (reply, ack) = oneshot::channel();
        self.owner
            .send(Publish::Start(self.name.1.clone(), reply))
            .await
            .map_err(|_| anyhow::anyhow!("publisher exited"))?;
        ack.await
            .with_context(|| format!("{:?} did not publish", self.name))
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let (reply, ack) = oneshot::channel();
        self.owner
            .send(Publish::Stop(self.name.1.clone(), reply))
            .await
            .map_err(|_| anyhow::anyhow!("publisher exited"))?;
        ack.await
            .with_context(|| format!("{:?} did not close", self.name))
    }
}

async fn publish_on(
    ip: IpAddr,
    server: IpAddr,
    id: String,
    mut commands: mpsc::Receiver<Publish>,
) -> anyhow::Result<()> {
    let mut agent_instance = None;
    let mut publications: BTreeMap<String, Arc<VideoPublication>> = BTreeMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            Publish::Start(track, reply) => {
                if agent_instance.is_none() {
                    agent_instance =
                        Some(agent(&NativeHost::bind(ip)?, server, &id, "publish").await?);
                }
                let publication = agent_instance
                    .as_ref()
                    .unwrap()
                    .video(track.clone())?
                    .publish(None)?;
                let admitted = wait_publication_admitted(publication.updates()?.as_ref())
                    .await
                    .with_context(|| format!("{id}/{track}"))?;
                let publication_id = admitted
                    .admission
                    .publication_id
                    .context("missing publication ID")?;
                anyhow::ensure!(publications.insert(track, publication).is_none());
                let _ = reply.send(publication_id);
            }
            Publish::Stop(track, reply) => {
                publications
                    .remove(&track)
                    .context("track not published")?
                    .close()
                    .await?;
                if publications.is_empty() {
                    agent_instance
                        .take()
                        .context("publisher missing")?
                        .close()
                        .await?;
                }
                let _ = reply.send(());
            }
        }
    }
    Ok(())
}

fn exact(viewer: &Agent, name: &Name) -> anyhow::Result<Arc<VideoSubscription>> {
    Ok(viewer
        .room(ROOM.into())?
        .participant(name.0.clone())?
        .video(name.1.clone())?
        .subscribe(None)?)
}

async fn exact_ready(reader: &VideoSubscription) -> anyhow::Result<String> {
    let updates = reader.updates()?;
    loop {
        let state = updates.next().await?.context("exact admission closed")?;
        anyhow::ensure!(
            state.error.is_none() && state.execution_error.is_none(),
            "exact admission: {state:?}"
        );
        if state.execution == core_ffi::PublicationExecution::Installed {
            return state
                .source_track_id
                .context("installed exact publication identity");
        }
    }
}

async fn add_exact(viewer: &Agent, readers: &mut Readers, feeds: &[Feed]) -> anyhow::Result<()> {
    for window in feeds.chunks(EXACT_WINDOW) {
        let mut new = Vec::new();
        for feed in window {
            if !readers.contains_key(&feed.name) {
                let reader =
                    exact(viewer, &feed.name).with_context(|| format!("claim {:?}", feed.name))?;
                readers.insert(feed.name.clone(), reader.clone());
                new.push(reader);
            }
        }
        tokio::time::timeout(
            Duration::from_secs(5),
            try_join_all(new.iter().map(|reader| exact_ready(reader))),
        )
        .await
        .with_context(|| format!("exact window timed out at {} registrations", readers.len()))??;
    }
    Ok(())
}

async fn set_ready(
    set: &VideoCollection,
    count: usize,
    limited: bool,
    retired: Option<&Name>,
) -> anyhow::Result<Admissions> {
    let updates = set.updates()?;
    let status = if limited {
        CollectionStatus::Limited
    } else {
        CollectionStatus::Live
    };
    loop {
        let state = updates.next().await?.context("collection closed")?;
        anyhow::ensure!(state.error.is_none(), "collection error: {:?}", state.error);
        if state.status == status
            && state.tracks.len() == count
            && !state.tracks.iter().any(|entry| {
                retired.is_some_and(|(participant, track)| {
                    entry.participant_id == *participant && entry.track_id == *track
                })
            })
        {
            return Ok(state
                .tracks
                .into_iter()
                .map(|entry| ((entry.participant_id, entry.track_id), entry.publication_id))
                .collect());
        }
    }
}

async fn measure(
    set: Option<&VideoCollection>,
    readers: &Readers,
    overlap: &VideoSubscription,
    count: usize,
    limited: bool,
    retired: Option<&Name>,
) -> anyhow::Result<Admissions> {
    let run = async {
        let admitted: Admissions = if let Some(set) = set {
            set_ready(set, count, limited, retired).await?
        } else {
            try_join_all(readers.iter().map(|(name, reader)| async {
                anyhow::Ok((name.clone(), exact_ready(reader).await?))
            }))
            .await?
            .into_iter()
            .collect()
        };
        let sibling = exact_ready(overlap).await?;
        let selector = overlap.state().selector;
        anyhow::ensure!(
            admitted.get(&(selector.participant_id, selector.track_id)) == Some(&sibling),
            "overlap did not share the admitted identity"
        );
        anyhow::ensure!(admitted.len() == count);
        Ok(admitted)
    };
    tokio::time::timeout(Duration::from_secs(12), run)
        .await
        .with_context(|| {
            let installed = readers
                .values()
                .filter(|reader| {
                    reader.state().execution == core_ffi::PublicationExecution::Installed
                })
                .count();
            let collection = set.map(|set| {
                let state = set.state();
                (state.status, state.tracks.len())
            });
            format!(
                "admission deadline: {installed}/{count} exact installed; collection {collection:?}"
            )
        })?
}

async fn workload(
    host: NativeHost,
    server: IpAddr,
    mut feeds: Vec<Feed>,
    replacement: Option<Feed>,
    use_set: bool,
) -> anyhow::Result<()> {
    let offered = feeds.len();
    let trace = std::env::var_os("PULSEBEAM_PROFILE_TRACE").is_some();
    let mut expected = Admissions::new();
    for (index, feed) in feeds.iter().enumerate() {
        if trace && index % 16 == 0 {
            eprintln!("profile setup: {index}/{offered} publications ready");
        }
        expected.insert(feed.name.clone(), feed.start().await?);
    }
    if trace {
        eprintln!("profile setup: {offered} publications ready, joining viewer");
    }
    let viewer = agent(&host, server, "viewer", "subscribe").await?;
    let initial = tokio::time::Instant::now();
    let mut readers = Readers::new();
    let count = offered.min(BUDGET);
    let limited = offered > BUDGET;
    let mut overlap = exact(&viewer, &feeds[0].name)?;
    let set = if use_set {
        Some(viewer.room(ROOM.into())?.video(None).subscribe()?)
    } else {
        add_exact(&viewer, &mut readers, &feeds[..count]).await?;
        None
    };
    let quiet = measure(set.as_deref(), &readers, &overlap, count, limited, None).await?;
    let initial_us = initial.elapsed().as_micros();
    anyhow::ensure!(
        quiet.iter().all(|(key, id)| expected.get(key) == Some(id)),
        "wrong publisher identity"
    );
    if trace {
        eprintln!("profile viewer: initial admissions ready; starting churn");
    }

    let churn = tokio::time::Instant::now();
    let single = offered == 1;
    let index = if single {
        0
    } else {
        (1..feeds.len())
            .find(|&i| quiet.contains_key(&feeds[i].name))
            .context("no non-overlapping admission to retire")?
    };
    let retired = feeds.remove(index);
    if single {
        overlap.close().await?;
    }
    if let Some(reader) = readers.remove(&retired.name) {
        reader.close().await?;
    }
    retired.stop().await?;
    expected.remove(&retired.name);
    if let Some(replacement) = replacement {
        expected.insert(replacement.name.clone(), replacement.start().await?);
        feeds.push(replacement);
    }
    if single {
        overlap = exact(&viewer, &feeds[0].name)?;
    }
    if !use_set {
        add_exact(&viewer, &mut readers, &feeds).await?;
    }
    let active = measure(
        set.as_deref(),
        &readers,
        &overlap,
        count,
        false,
        Some(&retired.name),
    )
    .await?;
    let churn_us = churn.elapsed().as_micros();
    anyhow::ensure!(active == expected, "wrong membership after churn");
    for (name, id) in quiet.iter().filter(|(name, _)| **name != retired.name) {
        anyhow::ensure!(active.get(name) == Some(id));
    }
    if trace {
        eprintln!("profile viewer: churn settled; closing viewer and publishers");
    }
    overlap.close().await?;
    if let Some(set) = set {
        set.close();
    }
    for reader in readers.values() {
        reader.close().await?;
    }
    viewer.close().await?;
    for feed in &feeds {
        feed.stop().await?;
    }
    println!(
        "application-profile {}",
        serde_json::json!({
            "offered_publications": offered, "mode": if use_set { "set" } else { "exact" },
            "initial_admitted": quiet.len(), "initial_admission_us": initial_us,
            "after_churn_admitted": active.len(), "churn_us": churn_us,
            "publisher_connections": offered.div_ceil(TRACKS_PER_HOST),
            "admission_budget": BUDGET, "decoder_attachments": 0,
        })
    );
    Ok(())
}

#[test]
#[ignore = "manual application subscription profile"]
fn application_subscription_profile() {
    let offered: usize = std::env::var("PULSEBEAM_PROFILE_OFFERED")
        .unwrap()
        .parse()
        .unwrap();
    assert!(matches!(offered, 1 | 16 | 100 | 101));
    let mode = std::env::var("PULSEBEAM_PROFILE_MODE").unwrap();
    assert!(matches!(mode.as_str(), "exact" | "set"));
    let seed = NATIVE_RUNTIME_SEED;
    let _clocks = crate::sim_clock::SimClocksGuard::init();
    crate::sim_rand::set_thread_rng(seed);
    fastrand::seed(seed);
    pulsebeam_runtime::net::shaper::seed_impairments(seed);
    let cluster = tempfile::tempdir().unwrap();
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(150))
        .rng_seed(seed)
        .build();
    let subnet = reserve_subnet();
    let server = subnet_ip(subnet, 1);
    let client = subnet_ip(subnet, 2);
    let data_dir = cluster.path().to_owned();
    sim.host(server, move || {
        let data_dir = data_dir.clone();
        async move {
            let gossip = "127.0.0.1:7946".parse().unwrap();
            sfu_node_builder(server, pulsebeam_runtime::rand::seeded_rng(seed), 1)
                .with_cluster(pulsebeam::cluster::Options {
                    ownership_peers: Vec::new(),
                    region_tree: None,
                    cluster_id: "application-profile".into(),
                    topology: pulsebeam::cluster::Topology::SingleNode,
                    listen: gossip,
                    advertise: gossip,
                    seeds: Vec::new(),
                    data_dir,
                })
                .run(tokio_util::sync::CancellationToken::new())
                .await
                .map_err(Into::into)
        }
    });
    let has_replacement = offered <= BUDGET;
    let feed_count = offered + usize::from(has_replacement);
    let mut feeds = Vec::with_capacity(feed_count);
    for host in 0..feed_count.div_ceil(TRACKS_PER_HOST) {
        let ip = subnet_ip(subnet, (host + 3).try_into().unwrap());
        let id = format!("publisher-{host:03}");
        let (owner, commands) = mpsc::channel(1);
        let commands = RefCell::new(Some(commands));
        let host_id = id.clone();
        sim.host(ip, move || {
            let commands = commands.borrow_mut().take().expect("no publisher restart");
            let id = host_id.clone();
            async move {
                publish_on(ip, server, id, commands)
                    .await
                    .map_err(Into::into)
            }
        });
        for track in host * TRACKS_PER_HOST..((host + 1) * TRACKS_PER_HOST).min(feed_count) {
            feeds.push(Feed {
                name: (id.clone(), format!("camera-{track:03}")),
                owner: owner.clone(),
            });
        }
    }
    let replacement = if has_replacement { feeds.pop() } else { None };
    sim.client(client, async move {
        workload(
            NativeHost::bind(client)?,
            server,
            feeds,
            replacement,
            mode == "set",
        )
        .await
        .map_err(Into::into)
    });
    run_sim_or_timeout(&mut sim, Duration::from_secs(60)).unwrap();
}
