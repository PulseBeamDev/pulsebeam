use std::{io::ErrorKind, net::SocketAddr, sync::Arc};

use futures::{StreamExt, TryStreamExt};
use futures_concurrency::stream::StreamGroup;
use pulsebeam::{
    controller::ControllerHandle,
    entity::{ExternalParticipantId, ExternalRoomId},
    message::ActorError,
    net::{TcpListener, UdpSocket},
    rng::Rng,
    signaling,
    sink::UdpSinkHandle,
    source::UdpSourceHandle,
};
use rand::SeedableRng;
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output,
    change::{SdpOffer, SdpPendingOffer},
    net::Receive,
};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinSet,
    time::Instant,
};

pub fn setup_sim<'a>(seed: u64) -> turmoil::Sim<'a> {
    // TODO: use preseed rng
    let mut sim = turmoil::Builder::new().build();
    let server_addr: SocketAddr = "1.2.3.4:3478".parse().unwrap();

    sim.host("server", move || {
        async move {
            // TODO: turmoil doesn't support other addresses other than localhost
            let socket = UdpSocket::bind("0.0.0.0:3478").await.unwrap();
            let socket = Arc::new(socket);

            let rng = Rng::seed_from_u64(seed);
            let (source_handle, source_actor) = UdpSourceHandle::new(server_addr, socket.clone());
            let (sink_handle, sink_actor) = UdpSinkHandle::new(socket.clone());
            let (controller_handle, controller_actor) =
                ControllerHandle::new(rng, source_handle, sink_handle, vec![server_addr]);

            let router = signaling::router(controller_handle);
            let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
            let signaling = async move {
                axum::serve(listener, router)
                    .await
                    .map_err(|err| ActorError::Unknown(err.to_string()))
            };

            let res = tokio::try_join!(
                source_actor.run(),
                sink_actor.run(),
                controller_actor.run(),
                signaling
            );
            if let Err(err) = res {
                tracing::error!("pipeline ended with an error: {err}")
            }
            Ok(())
        }
    });

    sim.client("client", async move {
        let mut join_set = JoinSet::new();
        let mut event_group = StreamGroup::new();

        // TODO: add participants
        let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        let (handle, actor) = ParticipantClientHandle::new(socket, 1, 2);

        let event_rx = handle.subscribe();
        let event_rx = tokio_stream::wrappers::BroadcastStream::new(event_rx)
            .map_ok(move |v| (handle.clone(), v));

        event_group.insert(event_rx);
        join_set.spawn(actor.run());

        loop {
            let event = event_group.next().await.unwrap().unwrap();
            match event {
                (handle, ParticipantClientEvent::IceConnectionState(state)) => {
                    println!("ice connection state: {:?}", state);
                }
            }
        }

        Ok(())
    });

    sim
}

pub enum ParticipantClientMessage {}

#[derive(Clone, Debug)]
pub enum ParticipantClientEvent {
    IceConnectionState(IceConnectionState),
}

pub struct ParticipantClientActor {
    rtc: str0m::Rtc,
    socket: UdpSocket,
    room_id: Arc<ExternalRoomId>,
    participant_id: Arc<ExternalParticipantId>,
    data_rx: mpsc::Receiver<ParticipantClientMessage>,
    event_tx: broadcast::Sender<ParticipantClientEvent>,
}

impl ParticipantClientActor {
    pub fn create_offer(&mut self) -> (SdpOffer, SdpPendingOffer) {
        self.rtc.sdp_api().apply().unwrap()
    }

    pub async fn run(mut self) {
        let mut buf = vec![0; 2000];

        loop {
            // Poll output until we get a timeout. The timeout means we
            // are either awaiting UDP socket input or the timeout to happen.
            let timeout = match self.rtc.poll_output().unwrap() {
                // Stop polling when we get the timeout.
                Output::Timeout(v) => Instant::from_std(v),

                // Transmit this data to the remote peer. Typically via
                // a UDP socket. The destination IP comes from the ICE
                // agent. It might change during the session.
                Output::Transmit(v) => {
                    self.socket
                        .send_to(&v.contents, v.destination)
                        .await
                        .unwrap();
                    continue;
                }

                // Events are mainly incoming media data from the remote
                // peer, but also data channel data and statistics.
                Output::Event(v) => {
                    match v {
                        Event::IceConnectionStateChange(state) => {
                            // Abort if we disconnect.
                            if state == str0m::IceConnectionState::Disconnected {
                                return;
                            }

                            let _ = self
                                .event_tx
                                .send(ParticipantClientEvent::IceConnectionState(state));
                        }
                        _ => {}
                    }

                    // TODO: handle more cases of v here, such as incoming media data.

                    continue;
                }
            };

            // Duration until timeout.
            let duration = timeout - Instant::now();

            // socket.set_read_timeout(Some(0)) is not ok
            if duration.is_zero() {
                // Drive time forwards in rtc straight away.
                self.rtc
                    .handle_input(Input::Timeout(Instant::now().into_std()))
                    .unwrap();
                continue;
            }

            tokio::time::sleep(duration).await;

            // Scale up buffer to receive an entire UDP packet.
            buf.resize(2000, 0);

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
    pub fn new(
        socket: UdpSocket,
        send_streams: usize,
        recv_streams: usize,
    ) -> (Self, ParticipantClientActor) {
        let mut rtc = str0m::Rtc::new();
        rtc.add_local_candidate(Candidate::host("1.1.1.1:8000".parse().unwrap(), "udp").unwrap());
        let mut sdp = rtc.sdp_api();

        for _ in 0..send_streams {
            sdp.add_media(
                str0m::media::MediaKind::Video,
                str0m::media::Direction::SendOnly,
                None,
                None,
                None,
            );
            sdp.add_media(
                str0m::media::MediaKind::Audio,
                str0m::media::Direction::SendOnly,
                None,
                None,
                None,
            );
        }

        for _ in 0..recv_streams {
            sdp.add_media(
                str0m::media::MediaKind::Video,
                str0m::media::Direction::RecvOnly,
                None,
                None,
                None,
            );
            sdp.add_media(
                str0m::media::MediaKind::Audio,
                str0m::media::Direction::RecvOnly,
                None,
                None,
                None,
            );
        }

        let room_id = ExternalRoomId::new("simulation".to_string()).unwrap();
        let room_id = Arc::new(room_id);
        let participant_id = ExternalParticipantId::new("alice".to_string()).unwrap();
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
