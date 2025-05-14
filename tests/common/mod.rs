use std::{io::ErrorKind, net::SocketAddr, sync::Arc, time::Duration};

mod net;

use console_subscriber::ConsoleLayer;
use net::{VirtualTcpListener, VirtualUdpSocket};
use pulsebeam::{
    controller::ControllerHandle,
    entity::{ExternalParticipantId, ExternalRoomId},
    message::ActorError,
    net::PacketSocket,
    rng::Rng,
    signaling,
    sink::UdpSinkHandle,
    source::UdpSourceHandle,
};
use rand::SeedableRng;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, change::SdpAnswer, net::Receive};
use tokio::{
    sync::{broadcast, mpsc},
    time::Instant,
};
use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt};
use turmoil::net::{TcpListener, UdpSocket};

pub struct Simulation {}

pub fn setup_sim(seed: u64) {
    let subscriber = Registry::default()
        .with(ConsoleLayer::builder().spawn())
        .with(EnvFilter::from_default_env())
        .with(fmt::layer().pretty());

    // Set the subscriber as the global default
    if let Err(e) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("Failed to set global default subscriber: {}", e);
        // Optionally, fall back to a simple logger if console setup fails
        // tracing_subscriber::fmt::init();
    }

    let mut sim = turmoil::Builder::new().build();

    sim.host("server", || {
        async move {
            // TODO: use preseed rng
            let socket = UdpSocket::bind("0.0.0.0:3478").await.unwrap();
            let server_addr = "192.168.1.1:3478".parse().unwrap();
            let socket = VirtualUdpSocket(Arc::new(socket));

            let rng = Rng::seed_from_u64(seed);
            let (source_handle, source_actor) = UdpSourceHandle::new(server_addr, socket.clone());
            let (sink_handle, sink_actor) = UdpSinkHandle::new(socket.clone());
            let (controller_handle, controller_actor) =
                ControllerHandle::new(rng, source_handle, sink_handle, vec![server_addr]);
            let router = signaling::router(controller_handle);
            let listener = TcpListener::bind("0.0.0.0:3000").await?;
            let signaling = async move {
                axum::serve(VirtualTcpListener(listener), router)
                    .await
                    .map_err(|err| ActorError::Unknown(err.to_string()))
            };

            let res = tokio::try_join!(
                source_actor.run(),
                sink_actor.run(),
                controller_actor.run(),
                signaling,
            );

            res.map(|_| ()).map_err(|err| err.into())
        }
    });

    let server_addr = sim.lookup("server");
    let addr = format!("{}:{}", sim.lookup("server"), 3000);
    tracing::info!("server addr: {}", server_addr);
    sim.client("client", async move {
        // TODO: add participants
        let server_addr: SocketAddr = "0.0.0.0:3478".parse().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let socket = VirtualUdpSocket(Arc::new(socket));
        let (handle, actor) = ParticipantClientHandle::connect(socket, addr, 1, 2).await;

        let join = tokio::spawn(actor.run());

        let mut event_rx = handle.subscribe();
        tracing::info!("running");
        while let Ok(event) = event_rx.recv().await {
            match event {
                ParticipantClientEvent::IceConnectionState(state) => {
                    tracing::info!("ice connection state: {:?}", state);
                }
            }
        }

        join.await?;
        Ok(())
    });

    sim.run().unwrap();
}

pub enum ParticipantClientMessage {}

#[derive(Clone, Debug)]
pub enum ParticipantClientEvent {
    IceConnectionState(IceConnectionState),
}

pub struct ParticipantClientActor<S> {
    rtc: str0m::Rtc,
    socket: S,
    room_id: Arc<ExternalRoomId>,
    participant_id: Arc<ExternalParticipantId>,
    data_rx: mpsc::Receiver<ParticipantClientMessage>,
    event_tx: broadcast::Sender<ParticipantClientEvent>,
}

impl<S: PacketSocket> ParticipantClientActor<S> {
    pub async fn run(mut self) {
        loop {
            let deadline = if let Some(deadline) = self.poll_output().await {
                deadline
            } else {
                // Rtc timeout
                break;
            };

            tokio::select! {
                _ = self.poll_input() => {}
                _ = tokio::time::sleep(deadline) => {
                    // explicit empty, next loop polls again
                }
            }
        }
    }

    async fn poll_input(&mut self) {
        let mut buf = vec![0; 2000];

        // Try to receive. Because we have a timeout on the socket,
        // we will either receive a packet, or timeout.
        // This is where having an async loop shines. We can await multiple things to
        // happen such as outgoing media data, the timeout and incoming network traffic.
        // When using async there is no need to set timeout on the socket.
        let input = match self.socket.recv_from(&mut buf).await {
            Ok((n, source)) => {
                // UDP data received.
                buf.truncate(n);
                Input::Receive(
                    Instant::now().into_std(),
                    Receive {
                        proto: str0m::net::Protocol::Udp,
                        source,
                        destination: self.socket.local_addr().unwrap(),
                        contents: buf.as_slice().try_into().unwrap(),
                    },
                )
            }

            Err(e) => match e.kind() {
                // Expected error for set_read_timeout().
                // One for windows, one for the rest.
                ErrorKind::WouldBlock | ErrorKind::TimedOut => {
                    Input::Timeout(Instant::now().into_std())
                }

                e => {
                    eprintln!("Error: {:?}", e);
                    return; // abort
                }
            },
        };

        // Input is either a Timeout or Receive of data. Both drive the state forward.
        self.rtc.handle_input(input).unwrap();
    }

    async fn poll_output(&mut self) -> Option<Duration> {
        while self.rtc.is_alive() {
            // Poll output until we get a timeout. The timeout means we
            // are either awaiting UDP socket input or the timeout to happen.
            match self.rtc.poll_output().unwrap() {
                // Stop polling when we get the timeout.
                Output::Timeout(deadline) => {
                    let now = Instant::now().into_std();
                    let duration = deadline - now;
                    if !duration.is_zero() {
                        return Some(duration);
                    }

                    // forward clock never fails
                    self.rtc.handle_input(Input::Timeout(now)).unwrap();
                }

                // Transmit this data to the remote peer. Typically via
                // a UDP socket. The destination IP comes from the ICE
                // agent. It might change during the session.
                Output::Transmit(v) => {
                    self.socket
                        .send_to(&v.contents, v.destination)
                        .await
                        .unwrap();
                }

                // Events are mainly incoming media data from the remote
                // peer, but also data channel data and statistics.
                Output::Event(v) => {
                    match v {
                        Event::IceConnectionStateChange(state) => {
                            // Abort if we disconnect.
                            if state == str0m::IceConnectionState::Disconnected {
                                self.rtc.disconnect();
                            }

                            let _ = self
                                .event_tx
                                .send(ParticipantClientEvent::IceConnectionState(state));
                        }
                        _ => {}
                    }

                    // TODO: handle more cases of v here, such as incoming media data.
                }
            };
        }
        None
    }
}

#[derive(Clone)]
pub struct ParticipantClientHandle {
    data_tx: mpsc::Sender<ParticipantClientMessage>,
    event_tx: broadcast::Sender<ParticipantClientEvent>,
    room_id: Arc<ExternalRoomId>,
    participant_id: Arc<ExternalParticipantId>,
}

impl ParticipantClientHandle {
    pub async fn connect<S: PacketSocket>(
        socket: S,
        server_addr: String,
        send_streams: usize,
        recv_streams: usize,
    ) -> (Self, ParticipantClientActor<S>) {
        let mut rtc = str0m::Rtc::new();
        rtc.add_local_candidate(Candidate::host("127.0.0.1:3478".parse().unwrap(), "udp").unwrap());
        let mut change = rtc.sdp_api();

        for _ in 0..send_streams {
            change.add_media(
                str0m::media::MediaKind::Video,
                str0m::media::Direction::SendOnly,
                None,
                None,
                None,
            );
            change.add_media(
                str0m::media::MediaKind::Audio,
                str0m::media::Direction::SendOnly,
                None,
                None,
                None,
            );
        }

        for _ in 0..recv_streams {
            change.add_media(
                str0m::media::MediaKind::Video,
                str0m::media::Direction::RecvOnly,
                None,
                None,
                None,
            );
            change.add_media(
                str0m::media::MediaKind::Audio,
                str0m::media::Direction::RecvOnly,
                None,
                None,
                None,
            );
        }

        let room_id = ExternalRoomId::new("simulation".to_string()).unwrap();
        let participant_id = ExternalParticipantId::new("alice".to_string()).unwrap();

        let (offer, pending) = change.apply().unwrap();
        let answer = net::offer(
            format!("http://{}/?room=simulation&participant=alice", server_addr),
            offer.to_sdp_string(),
        )
        .await
        .unwrap();

        let answer = SdpAnswer::from_sdp_string(&answer).unwrap();
        rtc.sdp_api().accept_answer(pending, answer).unwrap();

        let room_id = Arc::new(room_id);
        let participant_id = Arc::new(participant_id);

        let (data_tx, data_rx) = mpsc::channel(1);
        let (event_tx, _) = broadcast::channel(1);
        let handle = ParticipantClientHandle {
            data_tx,
            event_tx: event_tx.clone(),
            room_id: room_id.clone(),
            participant_id: participant_id.clone(),
        };
        let actor = ParticipantClientActor {
            rtc,
            socket,
            room_id,
            participant_id,
            data_rx,
            event_tx,
        };

        (handle, actor)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ParticipantClientEvent> {
        self.event_tx.subscribe()
    }
}
