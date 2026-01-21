use crate::api::{ApiError, CreateParticipantRequest, DeleteParticipantRequest, HttpApiClient};
use crate::{MediaFrame, TransceiverDirection};
use futures_lite::StreamExt;
use pulsebeam_core::net::UdpSocket;
use pulsebeam_proto::prelude::*;
use pulsebeam_proto::signaling::Track;
use pulsebeam_proto::{namespace, signaling};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use str0m::IceConnectionState;
use str0m::channel::{ChannelData, ChannelId};
use str0m::media::{Rid, Simulcast, SimulcastLayer};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    media::{Direction, MediaAdded, MediaKind, Mid},
    net::{Protocol, Receive},
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_stream::StreamMap;
use tokio_stream::wrappers::ReceiverStream;

const MIN_QUANTA: Duration = Duration::from_millis(1);

pub type TrackId = String;
pub type ParticipantId = String;

#[derive(Debug, Default, Clone)]
pub struct AgentStats {
    pub peer: Option<str0m::stats::PeerStats>,
    pub tracks: HashMap<Mid, TrackStats>,
}

#[derive(Debug, Default, Clone)]
pub struct TrackStats {
    pub rx_layers: HashMap<Option<Rid>, str0m::stats::MediaIngressStats>,
    pub tx_layers: HashMap<Option<Rid>, str0m::stats::MediaEgressStats>,
}

#[derive(Debug)]
pub struct LocalTrack {
    pub mid: Mid,
    tx: mpsc::Sender<MediaFrame>,
}

impl LocalTrack {
    pub fn try_send(&self, frame: MediaFrame) {
        let _ = self.tx.try_send(frame);
    }

    pub async fn send(&self, frame: MediaFrame) {
        let _ = self.tx.send(frame).await;
    }
}

fn new_remote_track(track: Arc<Track>) -> (RemoteTrackTx, RemoteTrackRx) {
    let (ch_tx, ch_rx) = mpsc::channel(128);
    let tx = RemoteTrackTx {
        track: track.clone(),
        tx: ch_tx,
    };
    let rx = RemoteTrackRx { track, rx: ch_rx };

    (tx, rx)
}

#[derive(Debug, Clone)]
pub struct RemoteTrackTx {
    pub track: Arc<Track>,
    tx: mpsc::Sender<MediaFrame>,
}

impl RemoteTrackTx {
    pub fn try_send(&self, frame: MediaFrame) {
        let _ = self.tx.try_send(frame);
    }

    pub async fn send(&self, frame: MediaFrame) {
        let _ = self.tx.send(frame).await;
    }
}

#[derive(Debug)]
pub struct RemoteTrackRx {
    pub track: Arc<Track>,
    rx: mpsc::Receiver<MediaFrame>,
}

impl RemoteTrackRx {
    pub async fn recv(&mut self) -> Option<MediaFrame> {
        self.rx.recv().await
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AgentError {
    #[error("API call failed: {0}")]
    Api(#[from] ApiError),
    #[error("RTC Error: {0}")]
    Rtc(#[from] str0m::RtcError),
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Comand Error: {0}")]
    Command(#[from] mpsc::error::SendError<AgentCommand>),
    #[error("No valid network candidates found")]
    NoCandidates,
}

struct TrackRequest {
    kind: MediaKind,
    direction: TransceiverDirection,
    simulcast_layers: Option<Vec<SimulcastLayer>>,
}

pub struct AgentBuilder {
    api: HttpApiClient,
    udp_socket: UdpSocket,
    tracks: Vec<TrackRequest>,
    local_ips: Vec<IpAddr>,
}

impl AgentBuilder {
    pub fn new(api: HttpApiClient, udp_socket: UdpSocket) -> AgentBuilder {
        Self {
            api,
            udp_socket,
            tracks: Vec::new(),
            local_ips: Vec::new(),
        }
    }

    pub fn with_track(
        mut self,
        kind: MediaKind,
        direction: TransceiverDirection,
        simulcast_layers: Option<Vec<SimulcastLayer>>,
    ) -> Self {
        self.tracks.push(TrackRequest {
            kind,
            direction,
            simulcast_layers,
        });
        self
    }

    pub fn with_local_ip(mut self, ip: IpAddr) -> Self {
        self.local_ips.push(ip);
        self
    }

    pub async fn connect(mut self, room_id: &str) -> Result<Agent, AgentError> {
        let port = self.udp_socket.local_addr()?.port();

        if self.local_ips.is_empty() {
            self.local_ips.extend(
                if_addrs::get_if_addrs()?
                    .into_iter()
                    .filter(|i| !i.is_loopback())
                    .map(|i| i.ip()),
            )
        }

        tracing::info!("local ips: {:?}", self.local_ips);

        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_h264(true)
            .enable_opus(true)
            // .enable_bwe(Some(Bitrate::kbps(2000)))
            .set_stats_interval(Some(Duration::from_millis(200)))
            .build();

        let mut candidate_count = 0;
        let mut maybe_addr = None;
        for ip in self.local_ips {
            let addr = SocketAddr::new(ip, port);
            let candidate = match Candidate::builder().udp().host(addr).build() {
                Ok(candidate) => candidate,
                Err(err) => {
                    tracing::warn!("ignored bad candidate: {:?}", err);
                    continue;
                }
            };
            rtc.add_local_candidate(candidate);
            maybe_addr = Some(addr);
            candidate_count += 1;
        }

        if candidate_count == 0 {
            return Err(AgentError::NoCandidates);
        }

        // TODO: map multiple addresses?
        let Some(addr) = maybe_addr else {
            return Err(AgentError::NoCandidates);
        };

        let mut sdp = rtc.sdp_api();
        let mut medias = Vec::new();
        for track in self.tracks {
            let (dir, simulcast) = match track.direction {
                TransceiverDirection::SendOnly => (
                    Direction::SendOnly,
                    track.simulcast_layers.map(|layers| Simulcast {
                        send: layers,
                        recv: Vec::new(),
                    }),
                ),
                TransceiverDirection::RecvOnly => (
                    Direction::RecvOnly,
                    track.simulcast_layers.map(|layers| Simulcast {
                        send: Vec::new(),
                        recv: layers,
                    }),
                ),
            };
            let mid = sdp.add_media(track.kind, dir, None, None, simulcast.clone());
            // TODO: why do we need to emit manually here?
            medias.push(MediaAdded {
                mid,
                kind: track.kind,
                direction: dir,
                simulcast,
            });
        }

        let (offer, pending) = sdp.apply().expect("offer is required");
        let resp = self
            .api
            .create_participant(CreateParticipantRequest {
                room_id: room_id.to_string(),
                offer,
            })
            .await?;

        rtc.sdp_api()
            .accept_answer(pending, resp.answer)
            .map_err(AgentError::Rtc)?;

        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        let (event_tx, event_rx) = mpsc::channel(100);

        let actor = AgentActor {
            addr,
            rtc,
            stats: AgentStats::default(),
            socket: self.udp_socket,
            buf: vec![0u8; 2048],
            cmd_rx,
            event_tx,
            senders: StreamMap::new(),
            slot_manager: SlotManager::new(),
            disconnected_reason: None,
            signaling_cid: None,
        };

        tokio::spawn(async move {
            actor.run(medias).await;
        });

        Ok(Agent {
            room_id: room_id.to_string(),
            participant_id: resp.participant_id,
            api: self.api,
            cmd_tx,
            event_rx,
        })
    }
}

#[derive(Debug)]
pub enum AgentCommand {
    Disconnect,
    GetStats(oneshot::Sender<AgentStats>),

    /// Assigns a specific Track to a specific Receiver Slot (MID) at a target resolution.
    ///
    /// - `mid`: The ID of the `TrackReceiver` you want to use.
    /// - `track_id`: The remote track to subscribe to.
    /// - `height`: Desired resolution (e.g. 720, 1080).
    ///   Use `0` to pause the stream while keeping the assignment.
    Subscribe {
        mid: Mid,
        track_id: String,
        height: u32,
    },

    /// Stops receiving media on the specified slot and clears the assignment.
    ///
    /// The Actor will look up the `track_id` currently assigned to this `mid`
    /// and send a `VideoRequest` with `height: 0`.
    Unsubscribe {
        mid: Mid,
    },
}

#[derive(Debug)]
pub enum AgentEvent {
    LocalTrackAdded(LocalTrack),
    RemoteTrackAdded(RemoteTrackRx),
    Connected,
    Disconnected(String),
}

pub struct Agent {
    room_id: String,
    participant_id: String,
    api: HttpApiClient,
    cmd_tx: mpsc::Sender<AgentCommand>,
    event_rx: mpsc::Receiver<AgentEvent>,
}

impl Agent {
    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        self.event_rx.recv().await
    }

    pub async fn get_stats(&self) -> Option<AgentStats> {
        let (stats_tx, stats_rx) = oneshot::channel();
        let _ = self.cmd_tx.send(AgentCommand::GetStats(stats_tx)).await;
        stats_rx.await.ok()
    }

    pub async fn send(&self, cmd: AgentCommand) -> Result<(), AgentError> {
        self.cmd_tx.send(cmd).await?;
        Ok(())
    }

    pub async fn disconnect(&mut self) -> Result<(), AgentError> {
        let _ = self.cmd_tx.send(AgentCommand::Disconnect).await;
        self.api
            .delete_participant(DeleteParticipantRequest {
                room_id: self.room_id.clone(),
                participant_id: self.participant_id.clone(),
            })
            .await?;

        Ok(())
    }
}

struct AgentActor {
    addr: SocketAddr,
    rtc: Rtc,
    socket: UdpSocket,
    buf: Vec<u8>,
    stats: AgentStats,
    cmd_rx: mpsc::Receiver<AgentCommand>,
    event_tx: mpsc::Sender<AgentEvent>,

    senders: StreamMap<Mid, ReceiverStream<MediaFrame>>,
    pending: PendingState,
    slot_manager: SlotManager,

    signaling_cid: Option<ChannelId>,
    disconnected_reason: Option<String>,
}

impl AgentActor {
    async fn run(mut self, medias: Vec<MediaAdded>) {
        for media in medias {
            self.handle_media_added(media);
        }

        let sleep = tokio::time::sleep(MIN_QUANTA);
        tokio::pin!(sleep);

        while let Some(deadline) = self.poll_rtc().await {
            let now = Instant::now();

            // If the deadline is 'now' or in the past, we must not busy-wait.
            // We enforce a minimum 1ms "quanta" to prevent CPU starvation.
            let adjusted_deadline = if deadline <= now {
                now + MIN_QUANTA
            } else {
                deadline
            };

            if sleep.deadline() != adjusted_deadline {
                sleep.as_mut().reset(adjusted_deadline);
            }

            tokio::select! {
                biased;
                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd)
                }
                res = self.socket.recv_from(&mut self.buf) => {
                    if let Ok((n, source)) = res {
                         let _ = self.rtc.handle_input(Input::Receive(
                            Instant::now().into(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination: self.addr,
                                contents: self.buf[..n].try_into().unwrap(),
                            }
                        ));
                    }
                }

                // Data coming from User -> Network
                Some((mid, frame)) = self.senders.next() => {
                     if let Some(writer) = self.rtc.writer(mid) {
                         let pt = writer.payload_params().nth(0).unwrap().pt();
                         let _ = writer.write(pt, frame.capture_time.into(), frame.ts, frame.data);
                     }
                }

                 _ = &mut sleep => {
                    if let Err(_) = self.rtc.handle_input(Input::Timeout(Instant::now().into())) {
                         self.emit(AgentEvent::Disconnected("RTC Timeout".into()));
                         return;
                    }
                }
            }
        }
    }

    async fn poll_rtc(&mut self) -> Option<Instant> {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(tx)) => {
                    let _ = self.socket.send_to(&tx.contents, tx.destination).await;
                }
                Ok(Output::Event(e)) => {
                    match e {
                        Event::ChannelOpen(cid, label) => {
                            if label == namespace::Signaling::Reliable.as_str() {
                                self.signaling_cid.replace(cid);
                            }
                        }
                        Event::ChannelData(data) => {
                            if Some(data.id) == self.signaling_cid {
                                self.handle_signaling_data(data);
                            }
                        }
                        Event::MediaAdded(media) => self.handle_media_added(media),

                        Event::MediaData(data) => {
                            // Network -> User
                            if let Some(tx) = self.slot_manager.get_sender(&data.mid) {
                                let _ = tx.try_send(data.into());
                            }
                        }

                        Event::IceConnectionStateChange(state) => {
                            tracing::info!("connection state changed: {:?}", state);
                            if state == IceConnectionState::Disconnected {
                                let reason = self
                                    .disconnected_reason
                                    .clone()
                                    .unwrap_or("unknown reason".to_string());
                                self.emit(AgentEvent::Disconnected(reason));
                                return None;
                            }
                        }
                        Event::Connected => {
                            self.emit(AgentEvent::Connected);
                        }
                        Event::PeerStats(stats) => {
                            self.stats.peer = Some(stats);
                        }
                        Event::MediaIngressStats(stats) => {
                            let track_stats = self.stats.tracks.entry(stats.mid).or_default();
                            track_stats.rx_layers.insert(stats.rid, stats);
                        }
                        Event::MediaEgressStats(stats) => {
                            let track_stats = self.stats.tracks.entry(stats.mid).or_default();
                            track_stats.tx_layers.insert(stats.rid, stats);
                        }
                        e => {
                            tracing::trace!("unhandled event: {:?}", e);
                        }
                    }
                }
                Ok(Output::Timeout(t)) => return Some(t.into()),
                Err(e) => {
                    self.disconnected_reason = Some(format!("RTC Error: {:?}", e));
                    self.rtc.disconnect();
                }
            }
        }
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        let mid = media.mid;
        tracing::info!("new media added: {:?}", media);
        match media.direction {
            Direction::SendOnly => {
                // User wants to send. We create a channel: User(tx) -> Actor(rx)
                let (tx, rx) = mpsc::channel(128);
                self.senders.insert(mid, ReceiverStream::new(rx));

                self.emit(AgentEvent::LocalTrackAdded(LocalTrack { mid, tx }));
            }
            Direction::RecvOnly => {
                // Remote wants to send. We create a channel: Actor(tx) -> User(rx)
                self.slot_manager.register(mid);
            }
            dir => {
                tracing::warn!("{} transceiver direction is not supported", dir);
            }
        }
    }

    fn handle_command(&mut self, cmd: AgentCommand) {
        match cmd {
            AgentCommand::Disconnect => self.rtc.disconnect(),
            AgentCommand::GetStats(stats_tx) => {
                let _ = stats_tx.send(self.stats.clone());
            }
            AgentCommand::Subscribe {
                mid,
                track_id,
                height,
            } => {
                self.pending.assign(mid, track_id, height);
            }
            AgentCommand::Unsubscribe { mid } => {
                self.pending.unassign(mid);
            }
        }
    }

    fn handle_signaling_data(&mut self, cd: ChannelData) {
        let Ok(msg) = signaling::ServerMessage::decode(cd.data.as_slice()) else {
            tracing::warn!("Invalid Protobuf");
            return;
        };

        let Some(payload) = msg.payload else {
            tracing::warn!("empty protobuf");
            return;
        };

        match payload {
            signaling::server_message::Payload::Update(update) => {
                self.slot_manager.sync(update);
            }
            signaling::server_message::Payload::Error(err) => {
                tracing::warn!("signaling error: {}", err);
            }
        }
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.event_tx.try_send(event);
    }
}

struct PendingState {
    requests: Vec<pulsebeam_proto::signaling::VideoRequest>,
}

impl PendingState {
    fn assign(&mut self, mid: Mid, track_id: TrackId, height: u32) -> Option<()> {
        if let Some(req) = self
            .requests
            .iter_mut()
            .find(|r| r.mid.as_bytes() == mid.as_bytes())
        {
            req.track_id = track_id;
            req.height = height;
        } else {
            self.requests
                .push(pulsebeam_proto::signaling::VideoRequest {
                    mid: mid.to_string(),
                    track_id,
                    height,
                });
        }
        Some(())
    }

    fn unassign(&mut self, mid: Mid) {
        self.requests.retain(|r| r.mid.as_bytes() != mid.as_bytes());
    }

    fn take(&mut self) -> Vec<pulsebeam_proto::signaling::VideoRequest> {
        let replacement = Vec::with_capacity(self.requests.len());
        std::mem::replace(&mut self.requests, replacement)
    }
}

struct ReceiverSlot {
    mid: Mid,
    track_id: Option<TrackId>,
}

struct SlotManager {
    remote_tracks: HashMap<TrackId, RemoteTrackTx>,
    // The fixed set of Receive Transceivers (MIDs) available to this Agent.
    slots: Vec<ReceiverSlot>,
}

impl SlotManager {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            remote_tracks: HashMap::new(),
        }
    }

    /// Registers a permanent receiver slot (called during Agent init).
    fn register(&mut self, mid: Mid) {
        self.slots.push(ReceiverSlot {
            mid,
            track_id: None,
        });
    }

    fn sync(&mut self, update: pulsebeam_proto::signaling::StateUpdate) -> Vec<RemoteTrackRx> {
        let mut new_remote_tracks = Vec::new();

        for t in update.tracks_remove {
            self.remote_tracks.remove(&t);
        }

        for t in update.tracks_upsert {
            if self.remote_tracks.contains_key(&t.id) {
                tracing::warn!("detected a track entry duplicate: {}", t.id);
                continue;
            }

            let t = Arc::new(t);
            let (tx, rx) = new_remote_track(t.clone());
            self.remote_tracks.insert(t.id.clone(), tx);
            new_remote_tracks.push(rx);
        }

        for a in update.assignments_remove {
            let Some(idx) = self
                .slots
                .iter()
                .position(|s| s.mid.as_bytes() == a.as_bytes())
            else {
                continue;
            };

            self.slots.remove(idx);
        }

        for a in update.assignments_upsert {
            if !self.remote_tracks.contains_key(&a.track_id) {
                tracing::warn!(
                    "remote_track doesn't exist, ignore assignment update: {}",
                    a.track_id
                );
                continue;
            }

            for s in &mut self.slots {
                if s.mid.as_bytes() == a.mid.as_bytes() {
                    s.track_id.replace(a.track_id);
                    break;
                }
            }
        }

        new_remote_tracks
    }

    fn get_sender(&self, mid: &Mid) -> Option<&RemoteTrackTx> {
        let slot = self.slots.iter().find(|s| s.mid == *mid)?;
        let track_id = slot.track_id.as_ref()?;
        self.remote_tracks.get(track_id)
    }
}
