use std::{io::ErrorKind, net::SocketAddr, ptr::copy_nonoverlapping, sync::Arc, time::Duration};

mod net;

use futures::{StreamExt, TryStreamExt};
use futures_concurrency::stream::StreamGroup;
use net::{VirtualNetwork, VirtualSocket};
use pulsebeam::{
    controller::ControllerHandle,
    entity::{ExternalParticipantId, ExternalRoomId},
    net::PacketSocket,
    rng::Rng,
    sink::UdpSinkHandle,
    source::UdpSourceHandle,
};
use rand::SeedableRng;
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output,
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    net::Receive,
};
use tokio::{
    runtime::RngSeed,
    sync::{broadcast, mpsc},
    task::JoinSet,
    time::Instant,
};

pub struct Simulation {}

pub fn new_rt(seed: u64) -> tokio::runtime::Runtime {
    let rng = RngSeed::from_bytes(&seed.to_be_bytes());
    tracing_subscriber::fmt().init();
    tokio::runtime::Builder::new_current_thread()
        .rng_seed(rng)
        .start_paused(true)
        .build()
        .unwrap()
}

pub async fn setup_sim(seed: u64) {
    // TODO: use preseed rng
    let server_addr: SocketAddr = "1.2.3.4:3478".parse().unwrap();
    let vnet = VirtualNetwork::new(Duration::from_millis(0));
    let socket = VirtualSocket::register(vnet, server_addr).await;

    let rng = Rng::seed_from_u64(seed);
    let (source_handle, source_actor) = UdpSourceHandle::new(server_addr, socket.clone());
    let (sink_handle, sink_actor) = UdpSinkHandle::new(socket.clone());
    let (controller_handle, controller_actor) =
        ControllerHandle::new(rng, source_handle, sink_handle, vec![server_addr]);

    let mut server_set = JoinSet::new();
    server_set.spawn(source_actor.run());
    server_set.spawn(sink_actor.run());
    server_set.spawn(controller_actor.run());

    let mut client_set = JoinSet::new();
    let mut event_group = StreamGroup::new();

    // TODO: add participants
    let (handle, actor) =
        ParticipantClientHandle::connect(socket.clone(), controller_handle.clone(), 1, 2).await;

    let event_rx = handle.subscribe();
    let event_rx =
        tokio_stream::wrappers::BroadcastStream::new(event_rx).map_ok(move |v| (handle.clone(), v));

    event_group.insert(event_rx);
    client_set.spawn(actor.run());

    tracing::info!("running");
    loop {
        let event = event_group.next().await.unwrap().unwrap();
        match event {
            (handle, ParticipantClientEvent::IceConnectionState(state)) => {
                tracing::info!("ice connection state: {:?}", state);
            }
        }
    }
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
    pub async fn connect<S: PacketSocket>(
        socket: S,
        controller: ControllerHandle,
        send_streams: usize,
        recv_streams: usize,
    ) -> (Self, ParticipantClientActor<S>) {
        let mut rtc = str0m::Rtc::new();
        rtc.add_local_candidate(Candidate::host("1.1.1.1:8000".parse().unwrap(), "udp").unwrap());
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
        let answer = controller
            .allocate(
                room_id.clone(),
                participant_id.clone(),
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
