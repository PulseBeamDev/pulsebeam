use std::collections::VecDeque;

use ahash::HashMap;
use indexmap::IndexSet;
use pulsebeam_runtime::{
    mailbox::{self},
    net::{self, RecvPacketBatch, UnifiedSocket},
};
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, RoomId},
    participant::{
        ParticipantConfig, ParticipantCore,
        event::{
            ControlEvent, EventQueue, LifecycleEvent, ParticipantEvent, RtpEvent, TimerEvent,
            TopologyEvent,
        },
    },
    rtp::RtpPacket,
    shard::{demux::Demuxer, timer::TimerWheel},
    track::{GlobalKeyframeRequest, StreamId, StreamWriter, Track},
};
use str0m::media::MediaKind;
const MAX_PARTICIPANTS_PER_SHARD: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
}

struct Routing {
    kind: MediaKind,
    /// Local participants subscribed to this stream.
    subscribers: IndexSet<ParticipantId>,
    /// Remote shard IDs that have at least one subscriber for this stream.
    /// Only populated on the publisher's shard.
    remote_shards: IndexSet<usize>,
}

#[derive(Debug)]
pub enum ShardCommand {
    AddParticipant(ParticipantConfig),
    PublishTrack(Track, RoomId),
    RequestKeyframe(GlobalKeyframeRequest),
    RegisterParticipant {
        participant_id: ParticipantId,
        shard_id: usize,
        ufrag: String,
    },
    UnregisterParticipant {
        participant_id: ParticipantId,
    },
}

pub enum CrossShardEvent {
    /// Subscriber shard → Publisher shard: begin forwarding this stream to `from_shard_id`.
    StreamSubscribed {
        stream_id: StreamId,
        kind: MediaKind,
        from_shard_id: usize,
    },
    /// Subscriber shard → Publisher shard: no more local subscribers; stop forwarding.
    StreamUnsubscribed {
        stream_id: StreamId,
        from_shard_id: usize,
    },
    /// Publisher shard → Subscriber shards: carry an RTP packet across the shard boundary.
    RtpPublished { stream_id: StreamId, pkt: RtpPacket },
    /// Subscriber shard → Publisher shard: keyframe request.
    KeyframeRequested(GlobalKeyframeRequest),
    /// A UDP packet batch arrived on this shard but the participant lives elsewhere.
    UdpPacket {
        participant_id: ParticipantId,
        batch: RecvPacketBatch,
    },
}

#[derive(Default)]
struct RoomState {
    members: IndexSet<ParticipantId>,
}

#[derive(Debug)]
pub enum ShardEvent {
    TrackPublished(Track),
    ParticipantExited(ParticipantId),
    KeyframeRequest(GlobalKeyframeRequest),
}

struct ShardRouter {
    shard_id: usize,
    cross_shard_event_txs: Vec<mailbox::Sender<CrossShardEvent>>,
}

impl ShardRouter {
    fn send(&self, shard_id: usize, ev: CrossShardEvent) {
        debug_assert!(
            shard_id != self.shard_id,
            "ShardRouter sends a loopback event"
        );

        let _ = self.cross_shard_event_txs[shard_id].try_send(ev);
    }
}

pub struct ShardWorker {
    demuxer: Demuxer,
    participants: HashMap<ParticipantId, ParticipantCore>,
    rooms: HashMap<RoomId, RoomState>,
    routing: HashMap<StreamId, Routing>,
    participant_shards: HashMap<ParticipantId, usize>,

    recv_batch: Vec<RecvPacketBatch>,
    timers: TimerWheel,
    input_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    fanout_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    events: VecDeque<ParticipantEvent>,
    rtp_events: VecDeque<RtpEvent>,
    shard_events: VecDeque<ShardEvent>,
    /// Ufrag for participants that live on a remote shard, used to demux
    /// cross-shard UDP packets and to clean up demuxer state on unregister.
    remote_participant_ufrags: HashMap<ParticipantId, String>,

    udp_socket: UnifiedSocket,

    command_rx: mailbox::Receiver<ShardCommand>,
    event_tx: mailbox::Sender<ShardEvent>,
    cross_shard_event_rx: mailbox::Receiver<CrossShardEvent>,
    router: ShardRouter,
}

impl ShardWorker {
    pub fn new(
        shard_id: usize,
        udp_socket: UnifiedSocket,
        command_rx: mailbox::Receiver<ShardCommand>,
        event_tx: mailbox::Sender<ShardEvent>,
        cross_shard_event_rx: mailbox::Receiver<CrossShardEvent>,
        cross_shard_event_txs: Vec<mailbox::Sender<CrossShardEvent>>,
    ) -> Self {
        let recv_batch = Vec::with_capacity(net::BATCH_SIZE);
        let timers = TimerWheel::new(MAX_PARTICIPANTS_PER_SHARD);
        let input_dirty: IndexSet<ParticipantId, ahash::RandomState> =
            IndexSet::with_capacity_and_hasher(
                MAX_PARTICIPANTS_PER_SHARD,
                ahash::RandomState::default(),
            );
        let fanout_dirty: IndexSet<ParticipantId, ahash::RandomState> =
            IndexSet::with_capacity_and_hasher(
                MAX_PARTICIPANTS_PER_SHARD,
                ahash::RandomState::default(),
            );
        let events = VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD);
        let rtp_events = VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD);
        let shard_events = VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD);
        let router = ShardRouter {
            shard_id,
            cross_shard_event_txs,
        };

        Self {
            router,
            demuxer: Demuxer::default(),
            participants: HashMap::default(),
            rooms: HashMap::default(),
            routing: HashMap::default(),
            participant_shards: HashMap::default(),
            remote_participant_ufrags: HashMap::default(),

            recv_batch,
            timers,
            input_dirty,
            fanout_dirty,
            events,
            rtp_events,
            shard_events,

            udp_socket,
            command_rx,
            event_tx,
            cross_shard_event_rx,
        }
    }

    #[tracing::instrument(skip(self), fields(shard_id = self.router.shard_id))]
    pub async fn run(self) {
        let res = self.run_inner().await;
        tracing::info!("shard exited: {:?}", res);
    }

    async fn run_inner(mut self) -> Result<(), ShardError> {
        loop {
            let wait = async {
                match self.timers.next_deadline() {
                    Some(d) => tokio::time::sleep_until(d).await,
                    // No pending timers: park forever until socket wakes us.
                    None => std::future::pending::<()>().await,
                }
            };

            // Block until at least one source is ready.
            tokio::select! {
                biased;
                Ok(_) = self.udp_socket.readable() => {}
                Some(cmd) = self.command_rx.recv() => {
                    self.on_command(cmd);
                }
                Some(ev) = self.cross_shard_event_rx.recv() => {
                    self.on_cross_shard_event(ev);
                }
                _ = wait => {}
                else => break,
            }

            while let Ok(cmd) = self.command_rx.try_recv() {
                self.on_command(cmd);
            }

            while let Ok(ev) = self.cross_shard_event_rx.try_recv() {
                self.on_cross_shard_event(ev);
            }

            let now = Instant::now();

            self.timers.drain_expired(now, |participant_id| {
                if let Some(participant) = self.participants.get_mut(&participant_id) {
                    participant.on_timeout(now);
                    self.input_dirty.insert(participant_id);
                }
            });

            let count = self
                .udp_socket
                .try_recv_batch(&mut self.recv_batch)
                .unwrap_or_default();
            for batch in self.recv_batch.drain(..count) {
                let Some(participant_id) = self.demuxer.demux(&batch) else {
                    continue;
                };
                if let Some(participant) = self.participants.get_mut(&participant_id) {
                    participant.on_ingress(batch);
                    self.input_dirty.insert(participant_id);
                } else if let Some(&shard_id) = self.participant_shards.get(&participant_id) {
                    self.router.send(
                        shard_id,
                        CrossShardEvent::UdpPacket {
                            participant_id,
                            batch,
                        },
                    );
                }
            }

            poll_participants(
                now,
                &self.input_dirty,
                &mut self.participants,
                &mut self.events,
                &mut self.rtp_events,
            );

            while let Some(ev) = self.rtp_events.pop_front() {
                handle_rtp(
                    ev,
                    &self.routing,
                    &mut self.participants,
                    &mut self.fanout_dirty,
                    &self.router,
                );
            }

            poll_participants(
                now,
                &self.fanout_dirty,
                &mut self.participants,
                &mut self.events,
                &mut self.rtp_events,
            );

            // Drain all events produced this tick before flushing egress,
            // so RTP forwards from this tick are batched into the same flush.
            while let Some(event) = self.events.pop_front() {
                match event {
                    ParticipantEvent::Topology(ev) => {
                        handle_participant_topology(ev, &mut self.routing, &self.router);
                    }
                    ParticipantEvent::Timer(TimerEvent::DeadlineUpdated { at, participant_id }) => {
                        self.timers.schedule(participant_id, at);
                    }
                    ParticipantEvent::Lifecycle(LifecycleEvent::Exited { participant_id }) => {
                        self.remove_participant(&participant_id);
                        self.timers.cancel(&participant_id);
                        self.input_dirty.swap_remove(&participant_id);
                        self.shard_events
                            .push_back(ShardEvent::ParticipantExited(participant_id));
                    }
                    ParticipantEvent::Control(ev) => {
                        handle_participant_control(ev, &mut self.shard_events, &self.router);
                    }
                }
            }

            // Flush egress for all dirty participants in one pass.
            // Exited participants were swap_removed above so this is safe.
            for participant_id in self
                .input_dirty
                .drain(..)
                .chain(self.fanout_dirty.drain(..))
            {
                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };
                participant.udp_batcher.flush(&self.udp_socket);
                // TODO: TCP
            }

            while let Some(event) = self.shard_events.pop_front() {
                match self.event_tx.try_send(event) {
                    Err(mailbox::TrySendError::Full(e)) => {
                        tracing::warn!("shard event channel is full, piling up shard events");
                        self.shard_events.push_front(e);
                        break;
                    }
                    Err(mailbox::TrySendError::Closed(e)) => {
                        tracing::warn!("shard event channel is closed, piling up shard events");
                        self.shard_events.push_front(e);
                        break;
                    }
                    Ok(_) => {}
                }
            }
        }

        Ok(())
    }

    fn on_cross_shard_event(&mut self, ev: CrossShardEvent) {
        match ev {
            CrossShardEvent::StreamSubscribed {
                stream_id,
                kind,
                from_shard_id,
            } => {
                let routing = self.routing.entry(stream_id).or_insert_with(|| Routing {
                    kind,
                    subscribers: IndexSet::new(),
                    remote_shards: IndexSet::new(),
                });
                routing.remote_shards.insert(from_shard_id);
            }
            CrossShardEvent::StreamUnsubscribed {
                stream_id,
                from_shard_id,
            } => {
                if let Some(routing) = self.routing.get_mut(&stream_id) {
                    routing.remote_shards.swap_remove(&from_shard_id);
                    if routing.subscribers.is_empty() && routing.remote_shards.is_empty() {
                        self.routing.remove(&stream_id);
                    }
                }
            }
            CrossShardEvent::RtpPublished { stream_id, pkt } => {
                let ev = RtpEvent { stream_id, pkt };
                handle_rtp(
                    ev,
                    &self.routing,
                    &mut self.participants,
                    &mut self.fanout_dirty,
                    &self.router,
                );
            }
            CrossShardEvent::UdpPacket {
                participant_id,
                batch,
            } => {
                if let Some(participant) = self.participants.get_mut(&participant_id) {
                    participant.on_ingress(batch);
                    self.input_dirty.insert(participant_id);
                }
            }
            CrossShardEvent::KeyframeRequested(req) => {
                if let Some(p) = self.participants.get_mut(&req.origin) {
                    p.handle_remote_keyframe_request(req.stream_id, req.kind);
                    self.input_dirty.insert(req.origin);
                }
            }
        }
    }

    fn on_command(&mut self, cmd: ShardCommand) -> Option<()> {
        match cmd {
            ShardCommand::AddParticipant(cfg) => {
                let participant_id = cfg.participant_id;
                let room_id = cfg.room_id;
                self.add_participant(participant_id, cfg);
                self.rooms
                    .entry(room_id)
                    .or_default()
                    .members
                    .insert(participant_id);
                self.input_dirty.insert(participant_id);
            }
            ShardCommand::PublishTrack(track, room_id) => {
                let publisher = track.meta.origin;
                let tracks = &[track];
                let room = self.rooms.get(&room_id)?;
                for &participant_id in &room.members {
                    if participant_id == publisher {
                        continue;
                    }
                    let Some(p) = self.participants.get_mut(&participant_id) else {
                        continue;
                    };
                    p.on_tracks_published(tracks);
                    self.input_dirty.insert(participant_id);
                }
            }
            ShardCommand::RequestKeyframe(req) => {
                let p = self.participants.get_mut(&req.origin)?;
                p.handle_remote_keyframe_request(req.stream_id, req.kind);
                self.input_dirty.insert(req.origin);
            }
            ShardCommand::RegisterParticipant {
                participant_id,
                shard_id,
                ufrag,
            } => {
                self.participant_shards.insert(participant_id, shard_id);
                // Register the ufrag in this shard's demuxer so that STUN packets
                // arriving on the wrong shard can still be identified and forwarded.
                if shard_id != self.router.shard_id {
                    self.demuxer
                        .register_ice_ufrag(ufrag.as_bytes(), participant_id);
                    self.remote_participant_ufrags.insert(participant_id, ufrag);
                }
            }
            ShardCommand::UnregisterParticipant { participant_id } => {
                self.participant_shards.remove(&participant_id);
                if let Some(ufrag) = self.remote_participant_ufrags.remove(&participant_id) {
                    let addrs = self.demuxer.unregister(ufrag.as_bytes());
                    for addr in &addrs {
                        self.udp_socket.close_peer(addr);
                    }
                }
            }
        }
        Some(())
    }

    fn add_participant(&mut self, participant_id: ParticipantId, cfg: ParticipantConfig) {
        self.remove_participant(&participant_id);

        let mut participant = ParticipantCore::new(
            cfg,
            self.router.shard_id,
            self.udp_socket.max_gso_segments(),
            1,
        );
        self.demuxer
            .register_ice_ufrag(participant.ufrag().as_bytes(), participant_id);

        self.participants.insert(participant_id, participant);
        tracing::info!(%participant_id, "participant added to shard");
    }

    fn remove_participant(&mut self, participant_id: &ParticipantId) -> Option<ParticipantCore> {
        let mut participant = self.participants.remove(participant_id)?;
        if let Some(room) = self.rooms.get_mut(&participant.room_id) {
            room.members.swap_remove(participant_id);
            if room.members.is_empty() {
                self.rooms.remove(&participant.room_id);
            }
        }
        // Clean up the shard routing table before teardown.
        // participant is already removed from self.participants so there is no aliasing.
        participant.downstream.unsubscribe_all();
        let addrs = self.demuxer.unregister(participant.ufrag().as_bytes());
        for addr in &addrs {
            self.udp_socket.close_peer(addr);
        }
        Some(participant)
    }
}

fn poll_participants(
    now: Instant,
    dirty: &IndexSet<ParticipantId, ahash::RandomState>,
    participants: &mut HashMap<ParticipantId, ParticipantCore>,
    events: &mut VecDeque<ParticipantEvent>,
    rtp_events: &mut VecDeque<RtpEvent>,
) {
    for participant_id in dirty {
        let Some(participant) = participants.get_mut(participant_id) else {
            continue;
        };
        let mut queue = EventQueue::new(participant_id, events, rtp_events);
        participant.poll(now, &mut queue);
    }
}

fn handle_rtp(
    ev: RtpEvent,
    routing: &HashMap<StreamId, Routing>,
    participants: &mut HashMap<ParticipantId, ParticipantCore>,
    dirty: &mut IndexSet<ParticipantId, ahash::RandomState>,
    router: &ShardRouter,
) -> Option<()> {
    let route = routing.get(&ev.stream_id)?;
    match route.kind {
        MediaKind::Video => {
            for participant_id in &route.subscribers {
                let Some(sub) = participants.get_mut(participant_id) else {
                    continue;
                };
                let mut writer = StreamWriter(&mut sub.rtc);
                sub.downstream
                    .on_forward_rtp(&ev.stream_id, &ev.pkt, &mut writer);
                dirty.insert(*participant_id);
            }
            // Cross-shard fanout: send to remote shards that have subscribers.
            for &shard_id in &route.remote_shards {
                router.send(
                    shard_id,
                    CrossShardEvent::RtpPublished {
                        stream_id: ev.stream_id,
                        pkt: ev.pkt.clone(),
                    },
                );
            }
        }
        MediaKind::Audio => {
            // TODO: audio forwarding
        }
    }
    Some(())
}

fn handle_participant_topology(
    ev: TopologyEvent,
    routing: &mut HashMap<StreamId, Routing>,
    router: &ShardRouter,
) -> Option<()> {
    match ev {
        TopologyEvent::StreamSubscribed {
            shard_id,
            participant_id,
            stream_id,
            kind,
        } => {
            let entry = routing.entry(stream_id).or_insert_with(|| Routing {
                kind,
                subscribers: IndexSet::with_capacity(256),
                remote_shards: IndexSet::new(),
            });
            let was_empty = entry.subscribers.is_empty();
            entry.subscribers.insert(participant_id);
            // First local subscriber: tell the publisher shard to forward RTP to us.
            if was_empty {
                router.send(
                    shard_id,
                    CrossShardEvent::StreamSubscribed {
                        stream_id,
                        kind,
                        from_shard_id: router.shard_id,
                    },
                );
            }
        }
        TopologyEvent::StreamUnsubscribed {
            shard_id,
            participant_id,
            stream_id,
        } => {
            if let Some(entry) = routing.get_mut(&stream_id) {
                entry.subscribers.swap_remove(&participant_id);
                // Last local subscriber: stop forwarding from the publisher shard.
                if entry.subscribers.is_empty() {
                    router.send(
                        shard_id,
                        CrossShardEvent::StreamUnsubscribed {
                            stream_id,
                            from_shard_id: router.shard_id,
                        },
                    );
                    if entry.remote_shards.is_empty() {
                        routing.remove(&stream_id);
                    }
                }
            }
        }
    }

    Some(())
}
fn handle_participant_timer(_ev: TimerEvent) {}
fn handle_participant_lifecycle(_ev: LifecycleEvent) {}

fn handle_participant_control(
    ev: ControlEvent,
    shard_events: &mut VecDeque<ShardEvent>,
    router: &ShardRouter,
) {
    match ev {
        ControlEvent::TrackPublished(track) => {
            shard_events.push_back(ShardEvent::TrackPublished(track));
        }
        ControlEvent::KeyframeRequested(req) => {
            if req.shard_id != router.shard_id {
                // Publisher is on a remote shard: bypass the controller.
                router.send(req.shard_id, CrossShardEvent::KeyframeRequested(req));
            } else {
                // Publisher is local or unknown: let the controller route it.
                shard_events.push_back(ShardEvent::KeyframeRequest(req));
            }
        }
    }
}
