use ahash::HashMap;
use pulsebeam_runtime::sync::Arc;
use std::collections::VecDeque;
use std::time::Duration;

use crate::participant::batcher::Batcher;
use crate::participant::core::{CoreEvent, ParticipantCore};
use crate::{audio_selector::AudioSelectorSubscription, entity, room, track};
use pulsebeam_runtime::net::UnifiedSocketWriter;
use str0m::{Rtc, RtcError, error::SdpError};
use tokio::time::Instant;

pub use crate::participant::core::TrackMapping;

const MIN_QUANTA: Duration = Duration::from_millis(1);

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("Invalid SDP format: {0}")]
    InvalidSdpFormat(#[from] SdpError),
    #[error("Offer rejected: {0}")]
    OfferRejected(#[from] RtcError),
}

#[derive(Debug, Clone)]
pub enum ParticipantControlMessage {
    TracksSnapshot(HashMap<entity::TrackId, track::TrackReceiver>),
    TracksPublished(Arc<HashMap<entity::TrackId, track::TrackReceiver>>),
    TracksUnpublished(Arc<HashMap<entity::TrackId, track::TrackReceiver>>),
    TrackPublishRejected(track::TrackReceiver),
    /// Initial audio subscription from the room-level Top-N selector.
    /// The participant's `AudioAllocator` uses this to receive the pre-ranked
    /// audio streams without any per-participant Top-N logic.
    AudioSubscription(AudioSelectorSubscription),
}

pub struct Participant {
    pub core: ParticipantCore,
    pub room_handle: room::RoomHandle,
    pub udp_egress: UnifiedSocketWriter,
    pub tcp_egress: UnifiedSocketWriter,
    pub control_queue: VecDeque<ParticipantControlMessage>,
    pub disconnected: bool,
}


impl Participant {
    pub fn new(
        room_handle: room::RoomHandle,
        udp_egress: UnifiedSocketWriter,
        tcp_egress: UnifiedSocketWriter,
        participant_id: entity::ParticipantId,
        rtc: Rtc,
        manual_sub: bool,
    ) -> Self {
        let udp_batcher = Batcher::with_capacity(udp_egress.max_gso_segments());
        let tcp_batcher = Batcher::with_capacity(tcp_egress.max_gso_segments());
        let core = ParticipantCore::new(manual_sub, participant_id, rtc, udp_batcher, tcp_batcher);

        Self {
            core,
            room_handle,
            udp_egress,
            tcp_egress,
            control_queue: VecDeque::new(),
            disconnected: false,
        }
    }

    pub fn participant_id(&self) -> entity::ParticipantId {
        self.core.participant_id
    }

    pub fn on_udp_batch(&mut self, batch: pulsebeam_runtime::net::RecvPacketBatch) {
        if self.disconnected {
            return;
        }

        let now = Instant::now();
        let _ = self.core.handle_udp_packet_batch(batch, now);
    }

    pub fn on_control_message(&mut self, msg: ParticipantControlMessage) {
        if self.disconnected {
            return;
        }
        self.control_queue.push_back(msg);
    }

    pub fn poll(&mut self) {
        if self.disconnected {
            return;
        }

        while let Some(msg) = self.control_queue.pop_front() {
            self.apply_control_message(msg);
        }

        self.handle_core_events();
        let _ = self.core.poll();
        self.core.udp_batcher.flush(&self.udp_egress);
        self.core.tcp_batcher.flush(&self.tcp_egress);
        self.core.handle_tick();
    }

    fn apply_control_message(&mut self, msg: ParticipantControlMessage) {
        match msg {
            ParticipantControlMessage::TracksSnapshot(tracks) => {
                self.core.handle_available_tracks(&tracks);
            }
            ParticipantControlMessage::TracksPublished(tracks) => {
                self.core.handle_available_tracks(&tracks);
            }
            ParticipantControlMessage::TracksUnpublished(tracks) => {
                self.core.remove_available_tracks(&tracks);
            }
            ParticipantControlMessage::TrackPublishRejected(_) => {}
            ParticipantControlMessage::AudioSubscription(sub) => {
                self.core.downstream.set_audio_subscription(sub);
            }
        }
    }

    fn handle_core_events(&mut self) {
        let batch_size = self.core.events.len().min(16);

        for _ in 0..batch_size {
            if let Some(event) = self.core.events.drain(..1).next() {
                match event {
                    CoreEvent::SpawnTrack(rx) => {
                        let _ = self
                            .room_handle
                            .try_send(room::RoomMessage::PublishTrack(rx));
                    }
                }
            }
        }
    }

    pub fn disconnect(&mut self) {
        if self.disconnected {
            return;
        }

        self.disconnected = true;
        self.core.disconnect(crate::participant::core::DisconnectReason::SystemTerminated);
    }
}

impl std::fmt::Debug for Participant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Participant")
            .field("participant_id", &self.core.participant_id)
            .field("disconnected", &self.disconnected)
            .finish()
    }
}

