use crate::tests::common::client::{SimClientBuilder, VideoReceiveLog};
use crate::tests::common::{
    reserve_subnet, run_sim_or_timeout, start_sfu_node, start_sfu_node_tcp_only,
    start_sfu_node_tcp_only_multi_shard, subnet_ip,
};
use pulsebeam_agent::SimulcastLayer;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub enum Role {
    Publisher,
    Subscriber,
    /// Data-channel-only participant: no video tracks added.
    DataOnly,
}

#[derive(Clone, Debug)]
pub struct Participant {
    pub name: &'static str,
    pub role: Role,
    pub rids: Vec<&'static str>,
    /// Number of RecvOnly video slots (for multi-subscriber participants). Default 1.
    pub slots: usize,
    pub starts_disconnected: bool,
}

impl Participant {
    pub fn publisher(name: &'static str, rids: &[&'static str]) -> Self {
        Self {
            name,
            role: Role::Publisher,
            rids: rids.to_vec(),
            slots: 0,
            starts_disconnected: false,
        }
    }

    pub fn single_publisher(name: &'static str) -> Self {
        Self {
            name,
            role: Role::Publisher,
            rids: Vec::new(),
            slots: 0,
            starts_disconnected: false,
        }
    }

    pub fn subscriber(name: &'static str) -> Self {
        Self {
            name,
            role: Role::Subscriber,
            rids: Vec::new(),
            slots: 1,
            starts_disconnected: false,
        }
    }

    /// Subscriber that opens `slots` RecvOnly video tracks.
    pub fn multi_subscriber(name: &'static str, slots: usize) -> Self {
        Self {
            name,
            role: Role::Subscriber,
            rids: Vec::new(),
            slots,
            starts_disconnected: false,
        }
    }

    /// Data-channel-only participant: connects but adds no video tracks.
    pub fn data_participant(name: &'static str) -> Self {
        Self {
            name,
            role: Role::DataOnly,
            rids: Vec::new(),
            slots: 0,
            starts_disconnected: false,
        }
    }

    pub fn starts_disconnected(mut self) -> Self {
        self.starts_disconnected = true;
        self
    }
}

pub struct Room {
    pub name: &'static str,
    pub participants: Vec<Participant>,
}

impl Room {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            participants: Vec::new(),
        }
    }

    pub fn with_participant(mut self, p: Participant) -> Self {
        self.participants.push(p);
        self
    }
}

/// Builder for video quality assertions. Use the named `allow_*` methods to
/// relax specific constraints from their zero defaults.
#[derive(Clone, Debug)]
pub struct VideoQuality {
    pub min_frames: u64,
    pub max_missing_parameter_sets: u64,
    pub max_non_contiguous: u64,
}

impl VideoQuality {
    /// Require at least `n` frames. All other constraints default to zero.
    pub fn min_frames(n: u64) -> Self {
        Self {
            min_frames: n,
            max_missing_parameter_sets: 0,
            max_non_contiguous: 0,
        }
    }

    /// Allow up to `n` keyframes without preceding SPS+PPS.
    #[allow(dead_code)]
    pub fn allow_missing_parameter_sets(mut self, n: u64) -> Self {
        self.max_missing_parameter_sets = n;
        self
    }

    /// Allow up to `n` sequence-number gaps (one per simulcast switch is normal).
    pub fn allow_gaps_for_switches(mut self, n: u64) -> Self {
        self.max_non_contiguous = n;
        self
    }
}

#[allow(dead_code)]
pub enum Step {
    // ── Time ──────────────────────────────────────────────────────────────
    /// Advance simulated time. All participants run during this window.
    /// Also snapshots TX/RX byte counters for subsequent `*BytesInterval` checks.
    Run {
        description: &'static str,
        duration: Duration,
    },

    // ── Network ───────────────────────────────────────────────────────────
    Partition {
        description: &'static str,
        from: &'static str,
        to: &'static str,
    },
    Repair {
        description: &'static str,
        from: &'static str,
        to: &'static str,
    },
    Hold {
        description: &'static str,
        from: &'static str,
        to: &'static str,
    },
    Release {
        description: &'static str,
        from: &'static str,
        to: &'static str,
    },

    // ── Participant lifecycle ──────────────────────────────────────────────
    /// Connect a participant declared with `.starts_disconnected()`, or
    /// a previously disconnected one. Same as Reconnect semantically.
    Join {
        description: &'static str,
        participant: &'static str,
    },
    /// Gracefully disconnect: sends HTTP DELETE + RTC disconnect.
    Disconnect {
        description: &'static str,
        participant: &'static str,
    },
    /// Drop without signaling — models a process kill / abrupt exit.
    AbruptExit {
        description: &'static str,
        participant: &'static str,
    },
    /// Reconnect after a previous Disconnect or AbruptExit.
    Reconnect {
        description: &'static str,
        participant: &'static str,
    },

    // ── Subscriptions ─────────────────────────────────────────────────────
    /// Call driver.set_subscriptions(). Processed on next drive tick.
    SetSubscriptions {
        description: &'static str,
        participant: &'static str,
        subscriptions: Vec<VideoSubscription>,
    },
    /// Subscribe the participant to ALL currently discovered video tracks.
    /// `heights` is applied round-robin to discovered tracks (sorted ascending).
    /// Example: heights=&[720, 180] with 2 tracks → track[0]@720, track[1]@180.
    SubscribeAll {
        description: &'static str,
        participant: &'static str,
        heights: &'static [u32],
    },

    // ── Data channels ─────────────────────────────────────────────────────
    /// Call driver.declare_publish_topic().
    DeclarePublishTopic {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
    },
    /// Call driver.declare_subscribe_topic(). When `scoped_to` is set, the harness
    /// resolves that participant's runtime participant_id automatically.
    DeclareSubscribeTopic {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        scoped_to: Option<&'static str>,
    },
    /// Queue a payload to be sent on the next drive tick.
    PublishData {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        data: &'static [u8],
    },
    DeclareOrderedPublisher {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
    },
    DeclareOrderedSubscriber {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
    },
    PublishOrdered {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        data: &'static [u8],
    },

    // ── Assertions ─────────────────────────────────────────────────────────
    /// Deep QoE check: frames are renderable, SPS/PPS present before keyframes,
    /// no backward timestamp jumps, bounded sequence gaps.
    CheckVideoQuality {
        description: &'static str,
        participant: &'static str,
        quality: VideoQuality,
    },
    /// Assert the participant has an active peer connection.
    CheckConnected {
        description: &'static str,
        participant: &'static str,
    },
    /// Assert the participant has NO active peer connection.
    CheckNotConnected {
        description: &'static str,
        participant: &'static str,
    },
    /// Cumulative bytes received ≥ min_bytes.
    CheckRxBytes {
        description: &'static str,
        participant: &'static str,
        min_bytes: u64,
    },
    /// Cumulative bytes sent ≥ min_bytes.
    CheckTxBytes {
        description: &'static str,
        participant: &'static str,
        min_bytes: u64,
    },
    /// Bytes received since the last Step::Run ≥ min_bytes (per-window rate check).
    CheckRxBytesInterval {
        description: &'static str,
        participant: &'static str,
        min_bytes: u64,
    },
    /// Bytes sent since the last Step::Run ≥ min_bytes (per-window rate check).
    CheckTxBytesInterval {
        description: &'static str,
        participant: &'static str,
        min_bytes: u64,
    },
    /// Assert ≥1 payload matching `expected` was received on the topic.
    CheckDataReceived {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        expected: &'static [u8],
    },
    /// Assert NO payload matching `excluded` was received on the topic.
    CheckDataNotReceived {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        excluded: &'static [u8],
    },
    CheckDataSequence {
        description: &'static str,
        participant: &'static str,
        topic: &'static str,
        expected: &'static [&'static [u8]],
    },
}

// ── Internal lifecycle commands ─────────────────────────────────────────────

enum ParticipantCmd {
    /// Graceful shutdown (HTTP DELETE + RTC disconnect), then wait for Reconnect.
    Shutdown,
    /// Drop the client without signaling (AbruptExit), then wait for Reconnect.
    Drop,
    /// Reconnect (after Shutdown or Drop).
    Reconnect,
    /// Test over — exit the participant loop.
    Done,
}

// ── Pending driver operations (queued by coordinator, drained on drive tick) ─

enum PendingDriverOp {
    SetSubscriptions(Vec<VideoSubscription>),
    DeclarePublishTopic(String),
    /// (topic, scoped_participant_id)
    DeclareSubscribeTopic(String, Option<String>),
    PublishData(String, Vec<u8>),
    DeclareOrderedPublisher(String),
    DeclareOrderedSubscriber(String),
    PublishOrdered(String, Vec<u8>),
}

#[derive(Clone)]
pub struct VideoSubscription {
    pub participant_id: String,
    pub height: u32,
    pub min_height: u32,
    pub priority: u32,
}

// ── Per-participant shared state ────────────────────────────────────────────

struct ParticipantShared {
    video_rx: Arc<Mutex<VideoReceiveLog>>,
    tx_bytes: Mutex<u64>,
    rx_bytes: Mutex<u64>,
    connected: Mutex<bool>,
    /// Set to Some(...) once the participant has connected for the first time.
    participant_id: Mutex<Option<String>>,
    /// Operations queued by the coordinator; drained on next drive tick.
    pending_ops: Mutex<Vec<PendingDriverOp>>,
    /// Data received per topic, accumulated across all drive ticks.
    data_received: Mutex<HashMap<String, Vec<Vec<u8>>>>,
    /// Remote video tracks discovered via signaling (track IDs).
    discovered_tracks: Mutex<HashSet<String>>,
}

impl ParticipantShared {
    fn new() -> Self {
        Self {
            video_rx: Arc::new(Mutex::new(VideoReceiveLog::default())),
            tx_bytes: Mutex::new(0),
            rx_bytes: Mutex::new(0),
            connected: Mutex::new(false),
            participant_id: Mutex::new(None),
            pending_ops: Mutex::new(Vec::new()),
            data_received: Mutex::new(HashMap::new()),
            discovered_tracks: Mutex::new(HashSet::new()),
        }
    }
}

struct ParticipantHandle {
    shared: Arc<ParticipantShared>,
    cmd_tx: mpsc::Sender<ParticipantCmd>,
    /// TX bytes at the start of the most recent Step::Run (for interval checks).
    interval_tx_baseline: u64,
    /// RX bytes at the start of the most recent Step::Run (for interval checks).
    interval_rx_baseline: u64,
}

impl ParticipantHandle {
    fn send_command(&self, command: ParticipantCmd) {
        // Best-effort: a participant that has abruptly exited has dropped its
        // command receiver, so a send can legitimately fail during teardown
        // races. That is not a test failure.
        let _ = self.cmd_tx.try_send(command);
    }

    fn tx_bytes(&self) -> u64 {
        *self.shared.tx_bytes.lock().unwrap()
    }
    fn rx_bytes(&self) -> u64 {
        *self.shared.rx_bytes.lock().unwrap()
    }
    fn connected(&self) -> bool {
        *self.shared.connected.lock().unwrap()
    }
    fn video_rx(&self) -> VideoReceiveLog {
        self.shared.video_rx.lock().unwrap().clone()
    }

    fn snapshot_interval(&mut self) {
        self.interval_tx_baseline = self.tx_bytes();
        self.interval_rx_baseline = self.rx_bytes();
    }
}

// ── Participant task ────────────────────────────────────────────────────────

async fn run_participant(
    ip: IpAddr,
    server_ip: IpAddr,
    config: Participant,
    room_name: &'static str,
    shared: Arc<ParticipantShared>,
    mut cmd_rx: mpsc::Receiver<ParticipantCmd>,
    tcp_only: bool,
) -> anyhow::Result<()> {
    // Participants declared starts_disconnected wait for a Join/Reconnect command.
    if config.starts_disconnected {
        match cmd_rx.recv().await {
            Some(ParticipantCmd::Reconnect) => {}
            _ => return Ok(()),
        }
    }

    loop {
        let mut builder = if tcp_only {
            SimClientBuilder::bind_tcp(ip, server_ip).await?
        } else {
            SimClientBuilder::bind(ip, server_ip).await?
        };

        match config.role {
            Role::Publisher => {
                let layers = if config.rids.is_empty() {
                    None
                } else {
                    Some(config.rids.iter().map(|r| SimulcastLayer::new(r)).collect())
                };
                builder = builder.publish_video(layers);
            }
            Role::Subscriber => {
                builder = builder.receive_video(config.slots.max(1));
            }
            Role::DataOnly => {
                // No tracks; data channels only.
            }
        }

        let auto_subscribe = matches!(config.role, Role::Subscriber);
        let shared_clone = shared.clone();
        let mut client = builder
            .with_video_rx(shared.video_rx.clone())
            .connect(room_name)
            .await?;

        // Capture participant_id after first connect.
        {
            let mut id_guard = shared.participant_id.lock().unwrap();
            if id_guard.is_none() {
                *id_guard = Some(client.ctx.agent.participant_id().clone());
            }
        }
        *shared.connected.lock().unwrap() = true;

        // Drive until cancelled or a lifecycle command arrives.
        let token = CancellationToken::new();
        let cmd = {
            let mut drive_fut = Box::pin(client.drive_until_cancelled(token.clone(), move |ctx| {
                // 1. Drain pending ops.
                let ops: Vec<PendingDriverOp> =
                    shared_clone.pending_ops.lock().unwrap().drain(..).collect();
                let mut retry_ops: Vec<PendingDriverOp> = Vec::new();
                for op in ops {
                    match op {
                        PendingDriverOp::SetSubscriptions(subs) => {
                            let incoming_tracks = ctx.incoming_track_tx.clone();
                            let new_subscriptions: Vec<_> = subs
                                .iter()
                                .filter(|subscription| {
                                    ctx.requested_tracks
                                        .insert(subscription.participant_id.clone())
                                })
                                .cloned()
                                .collect();
                            for subscription in new_subscriptions {
                                let agent = ctx.agent.clone();
                                let incoming_tracks = incoming_tracks.clone();
                                tokio::spawn(async move {
                                    let track = agent
                                        .participant(subscription.participant_id)
                                        .video()
                                        .subscribe()
                                        .target_height(subscription.height)
                                        .minimum_height(subscription.min_height)
                                        .priority(subscription.priority)
                                        .await
                                        .expect("failed to subscribe to publication");
                                    incoming_tracks
                                        .send(track)
                                        .await
                                        .expect("client media inbox closed");
                                });
                            }
                        }
                        PendingDriverOp::DeclarePublishTopic(t) => {
                            let agent = ctx.agent.clone();
                            let publishers = ctx.published_topics.clone();
                            tokio::spawn(async move {
                                let publisher = agent
                                    .topic(t.clone())
                                    .expect("invalid topic")
                                    .publisher()
                                    .latest()
                                    .await
                                    .expect("failed to declare publisher");
                                publishers.lock().unwrap().insert(t, publisher);
                            });
                        }
                        PendingDriverOp::DeclareSubscribeTopic(t, pub_id) => {
                            let agent = ctx.agent.clone();
                            let subscribers = ctx.subscribed_topics.clone();
                            tokio::spawn(async move {
                                let builder = agent
                                    .topic(t.clone())
                                    .expect("invalid topic")
                                    .subscriber()
                                    .latest();
                                let builder = match pub_id.as_deref() {
                                    Some(publisher_id) => builder.from_publisher(publisher_id),
                                    None => builder,
                                };
                                let subscriber =
                                    builder.await.expect("failed to declare subscriber");
                                subscribers.lock().unwrap().insert((t, pub_id), subscriber);
                            });
                        }
                        PendingDriverOp::PublishData(ref topic, ref data) => {
                            if let Some(publisher) = ctx.published_topics.lock().unwrap().get(topic)
                            {
                                if publisher.try_send(data.clone()).is_err() {
                                    retry_ops.push(op);
                                }
                            } else {
                                retry_ops.push(op);
                            }
                        }
                        PendingDriverOp::DeclareOrderedPublisher(t) => {
                            let agent = ctx.agent.clone();
                            let publishers = ctx.ordered_publishers.clone();
                            tokio::spawn(async move {
                                let publisher = agent
                                    .topic(t.clone())
                                    .expect("invalid topic")
                                    .publisher()
                                    .ordered()
                                    .await
                                    .expect("failed to declare ordered publisher");
                                publishers.lock().unwrap().insert(t, publisher);
                            });
                        }
                        PendingDriverOp::DeclareOrderedSubscriber(t) => {
                            let agent = ctx.agent.clone();
                            let subscribers = ctx.ordered_subscribers.clone();
                            tokio::spawn(async move {
                                let subscriber = agent
                                    .topic(t.clone())
                                    .expect("invalid topic")
                                    .subscriber()
                                    .ordered()
                                    .await
                                    .expect("failed to declare ordered subscriber");
                                subscribers.lock().unwrap().insert(t, subscriber);
                            });
                        }
                        PendingDriverOp::PublishOrdered(ref topic, ref data) => {
                            if let Some(publisher) =
                                ctx.ordered_publishers.lock().unwrap().get(topic)
                            {
                                if publisher.try_send(data.clone()).is_err() {
                                    retry_ops.push(op);
                                }
                            } else {
                                retry_ops.push(op);
                            }
                        }
                    }
                }
                if !retry_ops.is_empty() {
                    let mut guard = shared_clone.pending_ops.lock().unwrap();
                    // Prepend so retries are tried first next tick.
                    retry_ops.extend(guard.drain(..));
                    *guard = retry_ops;
                }

                // 1b. Auto-subscribe: for subscriber participants, subscribe to any
                // newly-discovered tracks so tests that have no explicit SubscribeAll
                // step still receive video.
                if auto_subscribe {
                    let incoming_tracks = ctx.incoming_track_tx.clone();
                    let new_track_ids: Vec<String> = ctx
                        .discovered_tracks
                        .iter()
                        .filter(|id| !ctx.requested_tracks.contains(*id))
                        .cloned()
                        .collect();
                    for participant_id in new_track_ids {
                        ctx.requested_tracks.insert(participant_id.clone());
                        let agent = ctx.agent.clone();
                        let incoming_tracks = incoming_tracks.clone();
                        tokio::spawn(async move {
                            // Subscribing can legitimately fail when the publisher
                            // has already left (churn, abrupt exit). That is not a
                            // test failure — just skip it.
                            let Ok(track) = agent
                                .participant(participant_id)
                                .video()
                                .subscribe()
                                .target_height(720)
                                .minimum_height(0)
                                .priority(0)
                                .await
                            else {
                                return;
                            };
                            let _ = incoming_tracks.send(track).await;
                        });
                    }
                }

                // 2. Drain received data from all known subscribers.
                {
                    let mut data_received = shared_clone.data_received.lock().unwrap();
                    for ((topic, _scope), subscriber) in
                        ctx.subscribed_topics.lock().unwrap().iter_mut()
                    {
                        while let Ok(payload) = subscriber.try_recv() {
                            data_received
                                .entry(topic.clone())
                                .or_default()
                                .push(payload);
                        }
                    }
                    for (topic, subscriber) in ctx.ordered_subscribers.lock().unwrap().iter_mut() {
                        while let Ok(delivery) = subscriber.try_recv() {
                            if let pulsebeam_agent::agent::OrderedTopicDelivery::Message(message) =
                                delivery
                            {
                                data_received
                                    .entry(topic.clone())
                                    .or_default()
                                    .push(message.payload);
                            }
                        }
                    }
                }

                // 3. Snapshot discovered tracks.
                {
                    *shared_clone.discovered_tracks.lock().unwrap() = ctx.discovered_tracks.clone();
                }

                // 4. Update stats.
                let stats = ctx.agent.stats().current();
                *shared_clone.tx_bytes.lock().unwrap() = stats.total_tx_bytes();
                *shared_clone.rx_bytes.lock().unwrap() = stats.total_rx_bytes();
                *shared_clone.connected.lock().unwrap() = stats.is_connected();
                false
            }));

            let received = tokio::select! {
                _ = drive_fut.as_mut() => None,
                c = cmd_rx.recv() => { token.cancel(); c }
            };
            drop(drive_fut); // release &mut client borrow
            received
        };

        // Update stats one final time before handling the command.
        {
            let stats = client.ctx.agent.stats().current();
            *shared.tx_bytes.lock().unwrap() = stats.total_tx_bytes();
            *shared.rx_bytes.lock().unwrap() = stats.total_rx_bytes();
        }
        *shared.connected.lock().unwrap() = false;

        match cmd {
            None | Some(ParticipantCmd::Done) => break,
            Some(ParticipantCmd::Shutdown) => {
                client.ctx.agent.close().await?;
                drop(client);
                match cmd_rx.recv().await {
                    Some(ParticipantCmd::Reconnect) => continue,
                    _ => break,
                }
            }
            Some(ParticipantCmd::Drop) => {
                drop(client);
                match cmd_rx.recv().await {
                    Some(ParticipantCmd::Reconnect) => continue,
                    _ => break,
                }
            }
            Some(ParticipantCmd::Reconnect) => continue,
        }
    }

    Ok(())
}

// ── Coordinator task ────────────────────────────────────────────────────────

fn step_name(step: &Step) -> &'static str {
    match step {
        Step::Run { .. } => "Run",
        Step::Partition { .. } => "Partition",
        Step::Repair { .. } => "Repair",
        Step::Hold { .. } => "Hold",
        Step::Release { .. } => "Release",
        Step::Join { .. } => "Join",
        Step::Disconnect { .. } => "Disconnect",
        Step::AbruptExit { .. } => "AbruptExit",
        Step::Reconnect { .. } => "Reconnect",
        Step::SetSubscriptions { .. } => "SetSubscriptions",
        Step::SubscribeAll { .. } => "SubscribeAll",
        Step::DeclarePublishTopic { .. } => "DeclarePublishTopic",
        Step::DeclareSubscribeTopic { .. } => "DeclareSubscribeTopic",
        Step::PublishData { .. } => "PublishData",
        Step::DeclareOrderedPublisher { .. } => "DeclareOrderedPublisher",
        Step::DeclareOrderedSubscriber { .. } => "DeclareOrderedSubscriber",
        Step::PublishOrdered { .. } => "PublishOrdered",
        Step::CheckVideoQuality { .. } => "CheckVideoQuality",
        Step::CheckConnected { .. } => "CheckConnected",
        Step::CheckNotConnected { .. } => "CheckNotConnected",
        Step::CheckRxBytes { .. } => "CheckRxBytes",
        Step::CheckTxBytes { .. } => "CheckTxBytes",
        Step::CheckRxBytesInterval { .. } => "CheckRxBytesInterval",
        Step::CheckTxBytesInterval { .. } => "CheckTxBytesInterval",
        Step::CheckDataReceived { .. } => "CheckDataReceived",
        Step::CheckDataNotReceived { .. } => "CheckDataNotReceived",
        Step::CheckDataSequence { .. } => "CheckDataSequence",
    }
}

async fn execute_plan(
    plan: Vec<Step>,
    handles: &mut HashMap<&'static str, ParticipantHandle>,
    name_to_ip: &HashMap<&'static str, IpAddr>,
) -> anyhow::Result<()> {
    let total = plan.len();

    for (idx, step) in plan.iter().enumerate() {
        let n = idx + 1;
        let kind = step_name(step);

        match step {
            Step::Run {
                description,
                duration,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({duration:?})");
                for handle in handles.values_mut() {
                    handle.snapshot_interval();
                }
                tokio::time::sleep(*duration).await;
            }

            Step::Partition {
                description,
                from,
                to,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({from} ↔ {to})");
                let from_ip = resolve(name_to_ip, from, description)?;
                let to_ip = resolve(name_to_ip, to, description)?;
                turmoil::partition(from_ip, to_ip);
            }

            Step::Repair {
                description,
                from,
                to,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({from} ↔ {to})");
                let from_ip = resolve(name_to_ip, from, description)?;
                let to_ip = resolve(name_to_ip, to, description)?;
                turmoil::repair(from_ip, to_ip);
            }

            Step::Hold {
                description,
                from,
                to,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({from} ↔ {to})");
                let from_ip = resolve(name_to_ip, from, description)?;
                let to_ip = resolve(name_to_ip, to, description)?;
                turmoil::hold(from_ip, to_ip);
            }

            Step::Release {
                description,
                from,
                to,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({from} ↔ {to})");
                let from_ip = resolve(name_to_ip, from, description)?;
                let to_ip = resolve(name_to_ip, to, description)?;
                turmoil::release(from_ip, to_ip);
            }

            Step::Join {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                handle.send_command(ParticipantCmd::Reconnect);
            }

            Step::Disconnect {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                handle.send_command(ParticipantCmd::Shutdown);
            }

            Step::AbruptExit {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                handle.send_command(ParticipantCmd::Drop);
            }

            Step::Reconnect {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                handle.send_command(ParticipantCmd::Reconnect);
            }

            Step::SetSubscriptions {
                description,
                participant,
                subscriptions,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::SetSubscriptions(subscriptions.clone()));
            }

            Step::SubscribeAll {
                description,
                participant,
                heights,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, heights={heights:?})"
                );
                let handle = get_handle(handles, participant, description)?;
                let mut tracks: Vec<String> = handle
                    .shared
                    .discovered_tracks
                    .lock()
                    .unwrap()
                    .iter()
                    .cloned()
                    .collect();
                tracks.sort();
                if tracks.is_empty() {
                    anyhow::bail!(
                        "step \"{description}\": SubscribeAll on \"{participant}\" but no tracks discovered yet; add a Step::Run before this step"
                    );
                }
                let heights_slice = if heights.is_empty() {
                    &[720u32][..]
                } else {
                    *heights
                };
                let subs: Vec<VideoSubscription> = tracks
                    .iter()
                    .enumerate()
                    .map(|(i, participant_id)| VideoSubscription {
                        participant_id: participant_id.clone(),
                        height: heights_slice[i % heights_slice.len()],
                        min_height: 0,
                        priority: 0,
                    })
                    .collect();
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::SetSubscriptions(subs));
            }

            Step::DeclarePublishTopic {
                description,
                participant,
                topic,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic})"
                );
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::DeclarePublishTopic(topic.to_string()));
            }

            Step::DeclareSubscribeTopic {
                description,
                participant,
                topic,
                scoped_to,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic}, scoped_to={scoped_to:?})"
                );
                let resolved_id: Option<String> = match scoped_to {
                    None => None,
                    Some(pub_name) => {
                        let pub_handle = handles.get(*pub_name).ok_or_else(|| {
                            anyhow::anyhow!("step \"{description}\": unknown publisher participant \"{pub_name}\"")
                        })?;
                        let id = pub_handle.shared.participant_id.lock().unwrap().clone();
                        match id {
                            Some(id) => Some(id),
                            None => anyhow::bail!(
                                "step \"{description}\": DeclareSubscribeTopic scoped_to \"{pub_name}\" \
                                 but that participant has not connected yet; add a Step::Run before this step"
                            ),
                        }
                    }
                };
                let handle = get_handle(handles, participant, description)?;
                handle.shared.pending_ops.lock().unwrap().push(
                    PendingDriverOp::DeclareSubscribeTopic(topic.to_string(), resolved_id),
                );
            }

            Step::PublishData {
                description,
                participant,
                topic,
                data,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic}, {} bytes)",
                    data.len()
                );
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::PublishData(
                        topic.to_string(),
                        data.to_vec(),
                    ));
            }

            Step::DeclareOrderedPublisher {
                description,
                participant,
                topic,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::DeclareOrderedPublisher(topic.to_string()));
            }

            Step::DeclareOrderedSubscriber {
                description,
                participant,
                topic,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::DeclareOrderedSubscriber(topic.to_string()));
            }

            Step::PublishOrdered {
                description,
                participant,
                topic,
                data,
            } => {
                let handle = get_handle(handles, participant, description)?;
                handle
                    .shared
                    .pending_ops
                    .lock()
                    .unwrap()
                    .push(PendingDriverOp::PublishOrdered(
                        topic.to_string(),
                        data.to_vec(),
                    ));
            }

            Step::CheckVideoQuality {
                description,
                participant,
                quality,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                let log = handle.video_rx();
                assert_video_quality(n, total, description, participant, quality, &log);
            }

            Step::CheckConnected {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                assert!(
                    handle.connected(),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     peer connection active\n  actual:       not connected"
                );
            }

            Step::CheckNotConnected {
                description,
                participant,
            } => {
                tracing::info!("[step {n}/{total}: {kind}] \"{description}\" ({participant})");
                let handle = get_handle(handles, participant, description)?;
                assert!(
                    !handle.connected(),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     no peer connection\n  actual:       connected"
                );
            }

            Step::CheckRxBytes {
                description,
                participant,
                min_bytes,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min_bytes} B)"
                );
                let handle = get_handle(handles, participant, description)?;
                let actual = handle.rx_bytes();
                assert!(
                    actual >= *min_bytes,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {min_bytes} bytes (cumulative)\n  actual:       {actual} bytes"
                );
            }

            Step::CheckTxBytes {
                description,
                participant,
                min_bytes,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min_bytes} B)"
                );
                let handle = get_handle(handles, participant, description)?;
                let actual = handle.tx_bytes();
                assert!(
                    actual >= *min_bytes,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {min_bytes} bytes (cumulative)\n  actual:       {actual} bytes"
                );
            }

            Step::CheckRxBytesInterval {
                description,
                participant,
                min_bytes,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min_bytes} B)"
                );
                let handle = get_handle(handles, participant, description)?;
                let baseline = handle.interval_rx_baseline;
                let actual = handle.rx_bytes().saturating_sub(baseline);
                assert!(
                    actual >= *min_bytes,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {min_bytes} bytes in last interval\n  actual:       {actual} bytes"
                );
            }

            Step::CheckTxBytesInterval {
                description,
                participant,
                min_bytes,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, min {min_bytes} B)"
                );
                let handle = get_handle(handles, participant, description)?;
                let baseline = handle.interval_tx_baseline;
                let actual = handle.tx_bytes().saturating_sub(baseline);
                assert!(
                    actual >= *min_bytes,
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {min_bytes} bytes in last interval\n  actual:       {actual} bytes"
                );
            }

            Step::CheckDataReceived {
                description,
                participant,
                topic,
                expected,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic})"
                );
                let handle = get_handle(handles, participant, description)?;
                let data_received = handle.shared.data_received.lock().unwrap();
                let received = data_received
                    .get(*topic)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let expected_vec = expected.to_vec();
                assert!(
                    received.contains(&expected_vec),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  topic:        {topic}\n  expected:     payload {:?} in received list\n  actual:       {received:?}",
                    expected
                );
            }

            Step::CheckDataNotReceived {
                description,
                participant,
                topic,
                excluded,
            } => {
                tracing::info!(
                    "[step {n}/{total}: {kind}] \"{description}\" ({participant}, topic={topic})"
                );
                let handle = get_handle(handles, participant, description)?;
                let data_received = handle.shared.data_received.lock().unwrap();
                let received = data_received
                    .get(*topic)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let excluded_vec = excluded.to_vec();
                assert!(
                    !received.contains(&excluded_vec),
                    "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  topic:        {topic}\n  expected:     payload {:?} NOT in received list\n  actual:       {received:?}",
                    excluded
                );
            }

            Step::CheckDataSequence {
                description,
                participant,
                topic,
                expected,
            } => {
                let handle = get_handle(handles, participant, description)?;
                let data_received = handle.shared.data_received.lock().unwrap();
                let received = data_received
                    .get(*topic)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let expected: Vec<Vec<u8>> =
                    expected.iter().map(|payload| payload.to_vec()).collect();
                assert_eq!(
                    received, expected,
                    "step {n}/{total} {kind}: {description} ({participant}, {topic})"
                );
            }
        }
    }

    // Signal all participants to stop.
    for handle in handles.values() {
        handle.send_command(ParticipantCmd::Done);
    }

    Ok(())
}

fn resolve<'a>(
    map: &'a HashMap<&'static str, IpAddr>,
    name: &str,
    step_desc: &str,
) -> anyhow::Result<IpAddr> {
    map.get(name).copied().ok_or_else(|| {
        anyhow::anyhow!("step \"{step_desc}\": unknown participant/endpoint name \"{name}\"")
    })
}

fn get_handle<'a>(
    handles: &'a mut HashMap<&'static str, ParticipantHandle>,
    name: &str,
    step_desc: &str,
) -> anyhow::Result<&'a mut ParticipantHandle> {
    handles
        .get_mut(name)
        .ok_or_else(|| anyhow::anyhow!("step \"{step_desc}\": unknown participant \"{name}\""))
}

fn assert_video_quality(
    n: usize,
    total: usize,
    description: &str,
    participant: &str,
    quality: &VideoQuality,
    log: &VideoReceiveLog,
) {
    let kind = "CheckVideoQuality";
    assert!(
        log.frames >= quality.min_frames,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≥ {} frames\n  actual:       frames={}, keyframes={}, missing_parameter_sets={}, ts_regression_count={}, non_contiguous={}",
        quality.min_frames,
        log.frames,
        log.keyframes,
        log.missing_parameter_sets,
        log.ts_regression_count,
        log.non_contiguous,
    );
    assert!(
        log.missing_parameter_sets <= quality.max_missing_parameter_sets,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≤ {} keyframes missing parameter sets (decodable)\n  actual:       frames={}, keyframes={}, missing_parameter_sets={}, ts_regression_count={}, non_contiguous={}",
        quality.max_missing_parameter_sets,
        log.frames,
        log.keyframes,
        log.missing_parameter_sets,
        log.ts_regression_count,
        log.non_contiguous,
    );
    assert!(
        log.non_contiguous <= quality.max_non_contiguous,
        "\nassertion failed\n  plan step:   {n}/{total} {kind}\n  description: \"{description}\"\n  participant:  {participant}\n  expected:     ≤ {} non-contiguous frames (gap budget)\n  actual:       frames={}, keyframes={}, missing_parameter_sets={}, ts_regression_count={}, non_contiguous={}",
        quality.max_non_contiguous,
        log.frames,
        log.keyframes,
        log.missing_parameter_sets,
        log.ts_regression_count,
        log.non_contiguous,
    );
}

// ── LocalNodeSim ────────────────────────────────────────────────────────────

pub struct LocalNodeSim {
    rooms: Vec<Room>,
    tick_duration: Duration,
    rng_seed: u64,
    tcp_only: bool,
    num_shards: usize,
}

impl Default for LocalNodeSim {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalNodeSim {
    pub fn new() -> Self {
        Self {
            rooms: Vec::new(),
            tick_duration: Duration::from_millis(1),
            rng_seed: 0xDEAD_BEEF,
            tcp_only: false,
            num_shards: 1,
        }
    }

    pub fn with_room(mut self, r: Room) -> Self {
        self.rooms.push(r);
        self
    }

    pub fn with_tick(mut self, d: Duration) -> Self {
        self.tick_duration = d;
        self
    }

    #[allow(unused)]
    pub fn with_rng_seed(mut self, seed: u64) -> Self {
        self.rng_seed = seed;
        self
    }

    /// Use TCP-only SFU + TCP client connections.
    pub fn with_tcp_only(mut self) -> Self {
        self.tcp_only = true;
        self
    }

    /// Use N worker shards (implies tcp_only).
    pub fn with_shards(mut self, n: usize) -> Self {
        self.num_shards = n;
        self.tcp_only = true;
        self
    }

    pub fn run(self, plan: Vec<Step>) {
        let sim_duration = plan
            .iter()
            .filter_map(|s| match s {
                Step::Run { duration, .. } => Some(*duration),
                _ => None,
            })
            .sum::<Duration>()
            + Duration::from_secs(60);

        let mut sim = turmoil::Builder::new()
            .simulation_duration(sim_duration)
            .tick_duration(self.tick_duration)
            .rng_seed(self.rng_seed)
            .build();

        let subnet = reserve_subnet();
        let server_ip = subnet_ip(subnet, 1);
        let coordinator_ip = subnet_ip(subnet, 254);
        let seed = self.rng_seed;
        let tcp_only = self.tcp_only;
        let num_shards = self.num_shards;

        sim.host(server_ip, move || async move {
            let rng = pulsebeam_runtime::rand::seeded_rng(seed);
            if tcp_only && num_shards > 1 {
                start_sfu_node_tcp_only_multi_shard(server_ip, rng)
                    .await
                    .map_err(Into::into)
            } else if tcp_only {
                start_sfu_node_tcp_only(server_ip, rng)
                    .await
                    .map_err(Into::into)
            } else {
                start_sfu_node(server_ip, rng).await.map_err(Into::into)
            }
        });

        let mut handles: HashMap<&'static str, ParticipantHandle> = HashMap::new();
        let mut name_to_ip: HashMap<&'static str, IpAddr> = HashMap::new();
        name_to_ip.insert("server", server_ip);

        let mut ip_counter = 2u8;
        for room in &self.rooms {
            for participant in &room.participants {
                let ip = subnet_ip(subnet, ip_counter);
                ip_counter += 1;

                name_to_ip.insert(participant.name, ip);

                let shared = Arc::new(ParticipantShared::new());
                let (cmd_tx, cmd_rx) = mpsc::channel::<ParticipantCmd>(16);

                handles.insert(
                    participant.name,
                    ParticipantHandle {
                        shared: shared.clone(),
                        cmd_tx,
                        interval_tx_baseline: 0,
                        interval_rx_baseline: 0,
                    },
                );

                let config = participant.clone();
                let room_name = room.name;

                sim.client(ip, async move {
                    run_participant(ip, server_ip, config, room_name, shared, cmd_rx, tcp_only)
                        .await
                        .map_err(Into::into)
                });
            }
        }

        sim.client(coordinator_ip, async move {
            let mut handles = handles;
            execute_plan(plan, &mut handles, &name_to_ip)
                .await
                .map_err(Into::into)
        });

        let wall_budget = sim_duration * 3 + Duration::from_secs(120);
        run_sim_or_timeout(&mut sim, wall_budget).expect("simulation failed");
    }
}
