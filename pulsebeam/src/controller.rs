use std::{collections::HashMap, io, net::SocketAddr, sync::Arc};

use crate::{
    entity::{ParticipantId, RoomId},
    node, room,
};
use futures::stream::FuturesUnordered;
use pulsebeam_runtime::actor;
use pulsebeam_runtime::prelude::*;
use str0m::{Candidate, RtcConfig, RtcError, change::SdpOffer, error::SdpError};
use tokio::sync::oneshot;

#[derive(thiserror::Error, Debug)]
pub enum ControllerError {
    #[error("sdp offer is invalid: {0}")]
    OfferInvalid(#[from] SdpError),

    #[error("sdp offer is rejected: {0}")]
    OfferRejected(#[from] RtcError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("IO error: {0}")]
    IOError(#[from] io::Error),

    #[error("unknown error: {0}")]
    Unknown(String),
}

#[derive(Debug)]
pub enum ControllerMessage {
    Allocate(
        Arc<RoomId>,
        Arc<ParticipantId>,
        String,
        oneshot::Sender<Result<String, ControllerError>>,
    ),
    RemoveParticipant(Arc<RoomId>, Arc<ParticipantId>),
}

pub struct ControllerMessageSet;

impl actor::MessageSet for ControllerMessageSet {
    type HighPriorityMsg = ControllerMessage;
    type LowPriorityMsg = ();
    type Meta = Arc<String>;
    type ObservableState = ();
}

pub struct ControllerActor {
    node_ctx: node::NodeContext,

    id: Arc<String>,
    local_addrs: Vec<SocketAddr>,

    rooms: HashMap<Arc<RoomId>, room::RoomHandle>,
    room_tasks: FuturesUnordered<actor::JoinHandle<room::RoomMessageSet>>,
}

impl actor::Actor<ControllerMessageSet> for ControllerActor {
    fn monitor() -> Arc<tokio_metrics::TaskMonitor> {
        static MONITOR: Lazy<Arc<TaskMonitor>> = Lazy::new(|| Arc::new(TaskMonitor::new()));
        MONITOR.clone()
    }

    fn meta(&self) -> Arc<String> {
        self.id.clone()
    }

    fn get_observable_state(&self) {}

    async fn run(
        &mut self,
        ctx: &mut actor::ActorContext<ControllerMessageSet>,
    ) -> Result<(), actor::ActorError> {
        pulsebeam_runtime::actor_loop!(self, ctx, pre_select:{} ,
            select: {
                Some((room_id, _)) = self.room_tasks.next() => {
                    self.rooms.remove(&room_id);
                }
            }
        );
        Ok(())
    }

    async fn on_high_priority(
        &mut self,
        ctx: &mut actor::ActorContext<ControllerMessageSet>,
        msg: ControllerMessage,
    ) -> () {
        match msg {
            ControllerMessage::Allocate(room_id, participant_id, offer, resp) => {
                let _ = resp.send(self.allocate(ctx, room_id, participant_id, offer).await);
            }

            ControllerMessage::RemoveParticipant(room_id, participant_id) => {
                if let Some(room_handle) = self.rooms.get_mut(&room_id) {
                    // if the room has exited, the participants have already cleaned up too.
                    let _ = room_handle
                        .send_high(room::RoomMessage::RemoveParticipant(participant_id))
                        .await;
                }
            }
        }
    }
}

impl ControllerActor {
    pub async fn allocate(
        &mut self,
        _ctx: &mut actor::ActorContext<ControllerMessageSet>,
        room_id: Arc<RoomId>,
        participant_id: Arc<ParticipantId>,
        offer: String,
    ) -> Result<String, ControllerError> {
        let offer = SdpOffer::from_sdp_string(&offer)?;
        tracing::debug!("{offer}");
        let mut rtc_config = RtcConfig::new()
            .clear_codecs()
            .set_rtp_mode(true)
            // TODO: enable bwe
            .enable_bwe(Some(str0m::bwe::Bitrate::mbps(3)))
            // Uncomment this to see statistics
            // .set_stats_interval(Some(Duration::from_secs(1)))
            .set_ice_lite(false);
        let codec_config = rtc_config.codec_config();
        codec_config.enable_opus(true);
        // h264 as the lowest common denominator due to small clients like
        // embedded devices, smartphones, OBS only supports H264.
        // Baseline profile to ensure compatibility with all platforms.

        // Level 3.1 to 4.1. This is mainly to support clients that don't handle
        // level-asymmetry-allowed=true properly.
        let baseline_levels = [0x1f, 0x20, 0x28, 0x29];
        for level in &baseline_levels {
            codec_config.add_h264(
                127.into(),       // PT for video
                Some(121.into()), // RTX PT
                true,             // packetization-mode = 1
                0x42e000 | level, // Baseline
            );
            codec_config.add_h264(
                108.into(),       // PT for video
                Some(109.into()), // RTX PT
                true,             // packetization-mode = 1
                0x420000 | level, // Constrained Baseline
            );
        }

        // TODO: OBS only supports Baseline level 3.1
        // // ESP32-P4 supports up to 1080p@30fps
        // // https://components.espressif.com/components/espressif/esp_h264/versions/1.1.3/readme
        // // Baseline Level 4.0, (pt=127, rtx=121)
        // codec_config.add_h264(127.into(), Some(121.into()), true, 0x420028);
        // // Constrained Baseline Level 4.0, (pt=108, rtx=109)
        // codec_config.add_h264(108.into(), Some(109.into()), true, 0x42e028);

        let mut rtc = rtc_config.build();

        for addr in self.local_addrs.iter() {
            // TODO: add tcp and ssltcp later
            let candidate = Candidate::host(*addr, "udp").expect("a host candidate");
            rtc.add_local_candidate(candidate);
        }

        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(ControllerError::OfferRejected)?;

        let mut room_handle = self.get_or_create_room(room_id);

        // TODO: probably retry? Or, let the client to retry instead?
        // Each room will always have a graceful timeout before closing.
        // But, a data race can still occur nonetheless
        room_handle
            .send_high(room::RoomMessage::AddParticipant(participant_id, rtc))
            .await
            .map_err(|_| ControllerError::ServiceUnavailable)?;

        tracing::debug!("{answer}");
        Ok(answer.to_sdp_string())
    }

    fn get_or_create_room(&mut self, room_id: Arc<RoomId>) -> room::RoomHandle {
        if let Some(handle) = self.rooms.get(&room_id) {
            tracing::info!("get_room: {}", room_id);
            handle.clone()
        } else {
            tracing::info!("create_room: {}", room_id);
            let room_actor = room::RoomActor::new(self.node_ctx.clone(), room_id.clone());
            let (room_handle, room_join) =
                actor::spawn(room_actor, actor::RunnerConfig::default().with_hi(1024));
            self.room_tasks.push(room_join);

            self.rooms.insert(room_id.clone(), room_handle.clone());

            room_handle
        }
    }
}

impl ControllerActor {
    pub fn new(
        system_ctx: node::NodeContext,
        local_addrs: Vec<SocketAddr>,
        id: Arc<String>,
    ) -> Self {
        Self {
            id,
            node_ctx: system_ctx,
            local_addrs,
            rooms: HashMap::new(),
            room_tasks: FuturesUnordered::new(),
        }
    }
}

pub type ControllerHandle = actor::ActorHandle<ControllerMessageSet>;
