use crate::signaling::{HttpSignalingClient, SignalingError};
use crate::{MediaFrame, TransceiverDirection};
use futures_lite::StreamExt;
use http::Uri;
use pulsebeam_core::net::UdpSocket;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use str0m::IceConnectionState;
use str0m::bwe::Bitrate;
use str0m::media::{Rid, Simulcast, SimulcastLayer};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    media::{Direction, MediaAdded, MediaKind, Mid, Pt},
    net::{Protocol, Receive},
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_stream::StreamMap;
use tokio_stream::wrappers::ReceiverStream;

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
pub struct TrackSender {
    pub mid: Mid,
    tx: mpsc::Sender<MediaFrame>,
}

impl TrackSender {
    pub fn try_send(&self, frame: MediaFrame) {
        let _ = self.tx.try_send(frame);
    }

    pub async fn send(&self, frame: MediaFrame) {
        let _ = self.tx.send(frame).await;
    }
}

#[derive(Debug)]
pub struct TrackReceiver {
    pub mid: Mid,
    pub pt: Option<Pt>,
    rx: mpsc::Receiver<MediaFrame>,
}

impl TrackReceiver {
    pub async fn recv(&mut self) -> Option<MediaFrame> {
        self.rx.recv().await
    }
}

#[derive(Debug)]
pub enum AgentEvent {
    SenderAdded(TrackSender),
    ReceiverAdded(TrackReceiver),
    Connected,
    Disconnected(String),
}

#[derive(thiserror::Error, Debug)]
pub enum AgentError {
    #[error("Signaling failed: {0}")]
    Signaling(#[from] SignalingError),
    #[error("RTC Error: {0}")]
    Rtc(#[from] str0m::RtcError),
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("No valid network candidates found")]
    NoCandidates,
}

struct TrackRequest {
    kind: MediaKind,
    direction: TransceiverDirection,
    simulcast_layers: Option<Vec<SimulcastLayer>>,
}

pub struct AgentBuilder {
    signaling: HttpSignalingClient,
    udp_socket: UdpSocket,
    tracks: Vec<TrackRequest>,
    local_ips: Vec<IpAddr>,
}

impl AgentBuilder {
    pub fn new(signaling: HttpSignalingClient, udp_socket: UdpSocket) -> AgentBuilder {
        Self {
            signaling,
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
            .enable_bwe(Some(Bitrate::kbps(2000)))
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
        let (answer, resource_uri) = self.signaling.connect(room_id, offer).await?;

        rtc.sdp_api()
            .accept_answer(pending, answer)
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
            receivers: HashMap::new(),
            disconnected_reason: None,
        };

        tokio::spawn(async move {
            actor.run(medias).await;
        });

        Ok(Agent {
            resource_uri,
            signaling: self.signaling,
            cmd_tx,
            event_rx,
        })
    }
}

#[derive(Debug)]
enum AgentCommand {
    Disconnect,
    GetStats(oneshot::Sender<AgentStats>),
}

pub struct Agent {
    resource_uri: Option<Uri>,
    signaling: HttpSignalingClient,
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

    pub async fn disconnect(&mut self) -> Result<(), AgentError> {
        let _ = self.cmd_tx.send(AgentCommand::Disconnect).await;
        if let Some(resource_uri) = self.resource_uri.take() {
            self.signaling.disconnect(resource_uri).await?;
        }

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
    receivers: HashMap<Mid, mpsc::Sender<MediaFrame>>,

    disconnected_reason: Option<String>,
}

impl AgentActor {
    async fn run(mut self, medias: Vec<MediaAdded>) {
        for media in medias {
            self.handle_media_added(media);
        }

        let sleep = tokio::time::sleep(Duration::from_millis(1));
        tokio::pin!(sleep);

        while let Some(deadline) = self.poll_rtc().await {
            let now = Instant::now();

            // If the deadline is 'now' or in the past, we must not busy-wait.
            // We enforce a minimum 1ms "quanta" to prevent CPU starvation.
            let adjusted_deadline = if deadline <= now {
                now + Duration::from_millis(1)
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
                    // Update str0m (Time has now advanced by at least 1us)
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
                        Event::MediaAdded(media) => self.handle_media_added(media),

                        Event::MediaData(data) => {
                            // Network -> User
                            if let Some(tx) = self.receivers.get(&data.mid) {
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
                            tracing::debug!("unhandled event: {:?}", e);
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
        let pt = self
            .rtc
            .media(mid)
            .and_then(|m| m.remote_pts().first().copied());

        tracing::info!("new media added: {:?}", media);
        match media.direction {
            Direction::SendOnly => {
                // User wants to send. We create a channel: User(tx) -> Actor(rx)
                let (tx, rx) = mpsc::channel(128);
                self.senders.insert(mid, ReceiverStream::new(rx));

                self.emit(AgentEvent::SenderAdded(TrackSender { mid, tx }));
            }
            Direction::RecvOnly => {
                // Remote wants to send. We create a channel: Actor(tx) -> User(rx)
                let (tx, rx) = mpsc::channel(128);
                self.receivers.insert(mid, tx);

                self.emit(AgentEvent::ReceiverAdded(TrackReceiver { mid, pt, rx }));
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
        }
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.event_tx.try_send(event);
    }
}
