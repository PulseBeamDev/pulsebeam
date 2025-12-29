use bytes::Bytes;
use futures_lite::StreamExt;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    media::{Direction, MediaAdded, MediaKind, Mid, Pt},
    net::{Protocol, Receive},
};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc};
use tokio_stream::StreamMap;
use tokio_stream::wrappers::ReceiverStream;

use crate::MediaFrame;
use crate::signaling::{HttpSignalingClient, SignalingError};

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
    State(str0m::IceConnectionState),
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

// -----------------------------------------------------------------------------
// Builder
// -----------------------------------------------------------------------------

struct TrackRequest {
    kind: MediaKind,
    direction: Direction,
}

pub struct AgentBuilder {
    signaling: HttpSignalingClient,
    udp_socket: Option<UdpSocket>,
    candidates: Vec<Candidate>,
    tracks: Vec<TrackRequest>,
}

impl AgentBuilder {
    pub fn new(signaling: HttpSignalingClient) -> Self {
        Self {
            signaling,
            udp_socket: None,
            candidates: Vec::new(),
            tracks: Vec::new(),
        }
    }

    pub fn with_candidates(mut self, candidates: Vec<Candidate>) -> Self {
        self.candidates = candidates;
        self
    }

    pub fn with_track(mut self, kind: MediaKind, direction: Direction) -> Self {
        self.tracks.push(TrackRequest { kind, direction });
        self
    }

    pub fn with_udp_socket(mut self, socket: UdpSocket) -> Self {
        self.udp_socket = Some(socket);
        self
    }

    pub async fn join(mut self, room_id: &str) -> Result<Agent, AgentError> {
        let socket = if let Some(socket) = self.udp_socket {
            socket
        } else {
            UdpSocket::bind("0.0.0.0:0").await?
        };
        let port = socket.local_addr()?.port();

        tracing::debug!("Discovering local network interfaces...");
        let local_ips: Vec<IpAddr> = if_addrs::get_if_addrs()?
            .into_iter()
            .filter(|i| !i.is_loopback())
            .map(|i| i.ip())
            .collect();

        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_h264(true)
            .enable_opus(true)
            .build();

        let mut candidate_count = 0;
        for c in self.candidates {
            rtc.add_local_candidate(c);
            candidate_count += 1;
        }

        for ip in local_ips {
            let addr = SocketAddr::new(ip, port);
            let candidate = match Candidate::builder().udp().host(addr).build() {
                Ok(candidate) => candidate,
                Err(err) => {
                    tracing::warn!("ignored bad candidate: {:?}", err);
                    continue;
                }
            };
            rtc.add_local_candidate(candidate);
            candidate_count += 1;
        }

        if candidate_count == 0 {
            return Err(AgentError::NoCandidates);
        }

        let mut sdp = rtc.sdp_api();
        for t in &self.tracks {
            sdp.add_media(t.kind, t.direction, None, None, None);
        }

        let (offer, pending) = sdp.apply().expect("offer is required");
        let answer = self.signaling.join(room_id, offer).await?;

        rtc.sdp_api()
            .accept_answer(pending, answer)
            .map_err(AgentError::Rtc)?;

        let (event_tx, event_rx) = mpsc::channel(100);
        let shutdown = Arc::new(Notify::new());

        let actor = AgentActor {
            rtc,
            socket,
            buf: vec![0u8; 2048],
            event_tx,
            senders: StreamMap::new(),
            receivers: HashMap::new(),
            shutdown: shutdown.clone(),
        };

        tokio::spawn(async move {
            actor.run().await;
        });

        Ok(Agent {
            signaling: self.signaling,
            events: event_rx,
            _shutdown: shutdown,
        })
    }
}

pub struct Agent {
    signaling: HttpSignalingClient,
    events: mpsc::Receiver<AgentEvent>,
    _shutdown: Arc<Notify>,
}

impl Agent {
    pub fn builder(signaling: HttpSignalingClient) -> AgentBuilder {
        AgentBuilder::new(signaling)
    }

    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }

    pub async fn leave(&mut self) {
        self._shutdown.notify_waiters();
        self.signaling.leave().await;
    }
}

struct AgentActor {
    rtc: Rtc,
    socket: UdpSocket,
    buf: Vec<u8>,
    event_tx: mpsc::Sender<AgentEvent>,

    senders: StreamMap<Mid, ReceiverStream<MediaFrame>>,
    receivers: HashMap<Mid, mpsc::Sender<MediaFrame>>,

    shutdown: Arc<Notify>,
}

impl AgentActor {
    async fn run(mut self) {
        loop {
            let timeout = loop {
                match self.rtc.poll_output() {
                    Ok(Output::Transmit(tx)) => {
                        let _ = self.socket.try_send_to(&tx.contents, tx.destination);
                    }
                    Ok(Output::Event(e)) => self.handle_rtc_event(e),
                    Ok(Output::Timeout(t)) => break t,
                    Err(e) => {
                        self.emit(AgentEvent::Disconnected(format!("RTC Error: {:?}", e)));
                        return;
                    }
                }
            };

            let duration = timeout
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);

            tokio::select! {
                _ = self.shutdown.notified() => {
                    let _ = self.rtc.disconnect();
                    return;
                }

                _ = tokio::time::sleep(duration) => {
                    if let Err(_) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
                         self.emit(AgentEvent::Disconnected("RTC Timeout".into()));
                         return;
                    }
                }

                res = self.socket.recv_from(&mut self.buf) => {
                    if let Ok((n, source)) = res {
                         let _ = self.rtc.handle_input(Input::Receive(
                            Instant::now(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination: self.socket.local_addr().unwrap(),
                                contents: self.buf[..n].try_into().unwrap(),
                            }
                        ));
                    }
                }

                // Data coming from User -> Network
                Some((mid, frame)) = self.senders.next() => {
                     if let Some(writer) = self.rtc.writer(mid) {
                         let pt = writer.payload_params().nth(0).unwrap().pt();
                         let _ = writer.write(pt, Instant::now(), frame.ts, frame.data);
                     }
                }
            }
        }
    }

    fn handle_rtc_event(&mut self, event: Event) {
        match event {
            Event::MediaAdded(media) => self.handle_media_added(media),

            Event::MediaData(data) => {
                // Network -> User
                if let Some(tx) = self.receivers.get(&data.mid) {
                    let _ = tx.try_send(MediaFrame {
                        data: Bytes::from(data.data),
                        ts: data.time,
                    });
                }
            }

            Event::IceConnectionStateChange(state) => {
                self.emit(AgentEvent::State(state));
            }
            _ => {}
        }
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        let mid = media.mid;
        let pt = self
            .rtc
            .media(mid)
            .and_then(|m| m.remote_pts().first().copied());

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

    fn emit(&self, event: AgentEvent) {
        let _ = self.event_tx.try_send(event);
    }
}
