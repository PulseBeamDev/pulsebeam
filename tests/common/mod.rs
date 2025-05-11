use std::{net::SocketAddr, sync::Arc};

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
    Candidate,
    change::{SdpOffer, SdpPendingOffer},
};

pub fn setup_sim<'a>(seed: u64) -> turmoil::Sim<'a> {
    // TODO: use preseed rng
    let mut sim = turmoil::Builder::new().build();

    sim.host("server", move || {
        async move {
            // TODO: turmoil doesn't support other addresses other than localhost
            let local_addr: SocketAddr = "0.0.0.0:3478".parse().unwrap();
            let socket = UdpSocket::bind(local_addr).await.unwrap();
            let local_addr: SocketAddr = "1.2.3.4:3478".parse().unwrap();
            let socket = Arc::new(socket);

            let rng = Rng::seed_from_u64(seed);
            let (source_handle, source_actor) = UdpSourceHandle::new(local_addr, socket.clone());
            let (sink_handle, sink_actor) = UdpSinkHandle::new(socket.clone());
            let (controller_handle, controller_actor) =
                ControllerHandle::new(rng, source_handle, sink_handle, vec![local_addr]);

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
        // TODO: add participants
        let participant = SimulatedParticipant::new(1, 2);
        Ok(())
    });

    sim
}

pub struct SimulatedParticipant {
    rtc: str0m::Rtc,
    room_id: ExternalRoomId,
    participant_id: ExternalParticipantId,
}

impl SimulatedParticipant {
    pub fn new(send_streams: usize, recv_streams: usize) -> Self {
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
        let participant_id = ExternalParticipantId::new("alice".to_string()).unwrap();

        Self {
            rtc,
            room_id,
            participant_id,
        }
    }

    pub fn create_offer(&mut self) -> (SdpOffer, SdpPendingOffer) {
        self.rtc.sdp_api().apply().unwrap()
    }
}
