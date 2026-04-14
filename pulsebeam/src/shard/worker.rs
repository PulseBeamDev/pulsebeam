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
    subscribers: IndexSet<ParticipantId>,
}

#[derive(Debug)]
pub enum ShardCommand {
    AddParticipant(ParticipantConfig),
    PublishTrack(Track, RoomId),
    RequestKeyframe(GlobalKeyframeRequest),
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

pub struct ShardWorker {
    shard_id: usize,
    demuxer: Demuxer,
    participants: HashMap<ParticipantId, ParticipantCore>,
    rooms: HashMap<RoomId, RoomState>,
    routing: HashMap<StreamId, Routing>,

    recv_batch: Vec<RecvPacketBatch>,
    timers: TimerWheel,
    input_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    fanout_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    events: VecDeque<ParticipantEvent>,
    rtp_events: VecDeque<RtpEvent>,
    shard_events: VecDeque<ShardEvent>,

    udp_socket: UnifiedSocket,

    command_rx: mailbox::Receiver<ShardCommand>,
    event_tx: mailbox::Sender<ShardEvent>,
}

impl ShardWorker {
    pub fn new(
        shard_id: usize,
        udp_socket: UnifiedSocket,
        command_rx: mailbox::Receiver<ShardCommand>,
        event_tx: mailbox::Sender<ShardEvent>,
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

        Self {
            shard_id,
            demuxer: Demuxer::default(),
            participants: HashMap::default(),
            rooms: HashMap::default(),
            routing: HashMap::default(),

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
        }
    }

    #[tracing::instrument(skip(self), fields(shard_id = self.shard_id))]
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
                _ = wait => {}
                else => break,
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
                let Some(participant) = self.participants.get_mut(&participant_id) else {
                    continue;
                };
                participant.on_ingress(batch);
                self.input_dirty.insert(participant_id);
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
                        handle_participant_topology(ev, &mut self.routing);
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
                        handle_participant_control(ev, &mut self.shard_events);
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
        }
        Some(())
    }

    fn add_participant(&mut self, participant_id: ParticipantId, cfg: ParticipantConfig) {
        self.remove_participant(&participant_id);

        let mut participant = ParticipantCore::new(cfg, self.udp_socket.max_gso_segments(), 1);
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
) -> Option<()> {
    match ev {
        TopologyEvent::StreamSubscribed {
            participant_id,
            stream_id,
            kind,
        } => {
            let routing = routing.entry(stream_id).or_insert_with(|| Routing {
                kind,
                subscribers: IndexSet::with_capacity(256),
            });
            routing.subscribers.insert(participant_id);
        }
        TopologyEvent::StreamUnsubscribed {
            participant_id,
            stream_id,
        } => {
            if let Some(routing) = routing.get_mut(&stream_id) {
                routing.subscribers.swap_remove(&participant_id);
            }
        }
    }

    Some(())
}
fn handle_participant_timer(_ev: TimerEvent) {}
fn handle_participant_lifecycle(_ev: LifecycleEvent) {}

fn handle_participant_control(ev: ControlEvent, shard_events: &mut VecDeque<ShardEvent>) {
    match ev {
        ControlEvent::TrackPublished(track) => {
            shard_events.push_back(ShardEvent::TrackPublished(track));
        }
        ControlEvent::KeyframeRequested(req) => {
            shard_events.push_back(ShardEvent::KeyframeRequest(req));
        }
    }
}
