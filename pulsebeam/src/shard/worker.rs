use std::collections::VecDeque;

use ahash::HashMap;
use indexmap::IndexSet;
use pulsebeam_runtime::{
    mailbox::{self},
    net::{self, RecvPacketBatch, UnifiedSocket},
};
use tokio::time::Instant;

use crate::{
    entity::{CohortId, ParticipantId},
    participant::{
        ParticipantConfig, ParticipantCore,
        event::{
            ControlEvent, EventQueue, LifecycleEvent, MediaEvent, ParticipantEvent, TimerEvent,
            TopologyEvent,
        },
    },
    shard::{demux::Demuxer, timer::TimerWheel},
    track::{GlobalKeyframeRequest, StreamId, StreamWriter, Track},
};

const MAX_PARTICIPANTS_PER_SHARD: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
}

#[derive(Default)]
struct Routing {
    subscribers: IndexSet<ParticipantId>,
}

#[derive(Debug)]
pub enum ShardCommand {
    AddParticipant(ParticipantConfig),
    PublishTrack(Track, CohortId),
    RequestKeyframe(GlobalKeyframeRequest),
}

/// Per-cohort state on a shard. A shard may host participants from many cohorts;
/// each cohort has its own independent membership set.
#[derive(Default)]
struct CohortState {
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
    cohorts: HashMap<CohortId, CohortState>,
    routing: HashMap<StreamId, Routing>,

    recv_batch: Vec<RecvPacketBatch>,
    timers: TimerWheel,
    input_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    fanout_dirty: IndexSet<ParticipantId, ahash::RandomState>,
    events: VecDeque<ParticipantEvent>,
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
        let shard_events = VecDeque::with_capacity(MAX_PARTICIPANTS_PER_SHARD);

        Self {
            shard_id,
            demuxer: Demuxer::default(),
            participants: HashMap::default(),
            cohorts: HashMap::default(),
            routing: HashMap::default(),

            recv_batch,
            timers,
            input_dirty,
            fanout_dirty,
            events,
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
            );

            // Drain all events produced this tick before flushing egress,
            // so RTP forwards from this tick are batched into the same flush.
            while let Some(event) = self.events.pop_front() {
                match event {
                    ParticipantEvent::Media(ev) => {
                        handle_participant_media(
                            ev,
                            &self.routing,
                            &mut self.participants,
                            &mut self.fanout_dirty,
                        );
                    }
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

            poll_participants(
                now,
                &self.fanout_dirty,
                &mut self.participants,
                &mut self.events,
            );

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

    fn on_command(&mut self, cmd: ShardCommand) {
        match cmd {
            ShardCommand::AddParticipant(cfg) => {
                let participant_id = cfg.participant_id;
                let cohort_id = cfg.cohort_id;
                self.add_participant(participant_id, cfg);
                self.cohorts
                    .entry(cohort_id)
                    .or_default()
                    .members
                    .insert(participant_id);
                self.input_dirty.insert(participant_id);
            }
            ShardCommand::PublishTrack(track, cohort_id) => {
                let publisher = track.meta.origin;
                let tracks = &[track];
                let Some(cohort) = self.cohorts.get(&cohort_id) else {
                    return;
                };
                for &participant_id in &cohort.members {
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
                let Some(p) = self.participants.get_mut(&req.origin) else {
                    tracing::warn!(%req.origin, ?req.stream_id, "RequestKeyframe: publisher participant not on this shard");
                    return;
                };
                p.handle_remote_keyframe_request(req.stream_id, req.kind);
                self.input_dirty.insert(req.origin);
            }
        }
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
        if let Some(cohort) = self.cohorts.get_mut(&participant.cohort_id) {
            cohort.members.swap_remove(participant_id);
            if cohort.members.is_empty() {
                self.cohorts.remove(&participant.cohort_id);
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
) {
    for participant_id in dirty {
        let Some(participant) = participants.get_mut(participant_id) else {
            continue;
        };
        let mut queue = EventQueue::new(participant_id, events);
        participant.poll(now, &mut queue);
    }
}

fn handle_participant_media(
    ev: MediaEvent,
    routing: &HashMap<StreamId, Routing>,

    participants: &mut HashMap<ParticipantId, ParticipantCore>,
    dirty: &mut IndexSet<ParticipantId, ahash::RandomState>,
) -> Option<()> {
    match ev {
        MediaEvent::RtpPublished { stream_id, pkt } => {
            let route = routing.get(&stream_id)?;

            for participant_id in &route.subscribers {
                let Some(sub) = participants.get_mut(participant_id) else {
                    continue;
                };
                let mut writer = StreamWriter(&mut sub.rtc);
                sub.downstream.on_forward_rtp(&stream_id, &pkt, &mut writer);
                dirty.insert(*participant_id);
            }
        }
    }
    Some(())
}

fn handle_participant_topology(ev: TopologyEvent, routing: &mut HashMap<StreamId, Routing>) {
    match ev {
        TopologyEvent::StreamSubscribed {
            participant_id,
            stream_id,
        } => {
            let routing = routing.entry(stream_id).or_default();
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
