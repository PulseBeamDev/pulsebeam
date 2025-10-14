use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use str0m::{
    Event, Input, Output, Rtc,
    media::{Direction, MediaAdded, MediaKind, Mid},
    rtp::{ExtensionValues, RtpPacket},
};

use crate::participant::batcher::Batcher;
use crate::{
    entity,
    participant::{
        audio::{AudioAllocator, AudioTrackData},
        effect::{self, Effect},
        video::VideoAllocator,
    },
    track,
    track::TrackMeta,
};

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("rtc error: {0}")]
    RtcError(str0m::RtcError),
}

/// Core per-participant state and synchronous media management logic.
///
/// This struct is a self-contained, synchronous state machine. It knows nothing
/// about async runtimes or I/O. It accepts inputs via its `handle_*` methods
/// and produces a queue of `Effect`s that the async actor counterpart must apply.
pub struct ParticipantCore {
    pub participant_id: Arc<entity::ParticipantId>,
    pub rtc: Rtc,
    pub batcher: Batcher,
    published_tracks: HashMap<Mid, track::TrackSender>,
    video_allocator: VideoAllocator,
    audio_allocator: AudioAllocator,
    pub effects: effect::Queue,
}

impl ParticipantCore {
    /// Creates a new `ParticipantCore` with a configured `Rtc` instance.
    pub fn new(participant_id: Arc<entity::ParticipantId>, rtc: Rtc, batcher: Batcher) -> Self {
        Self {
            participant_id,
            rtc,
            batcher,
            published_tracks: HashMap::new(),
            video_allocator: VideoAllocator::default(),
            audio_allocator: AudioAllocator::default(),
            effects: VecDeque::with_capacity(32),
        }
    }

    // --- INPUT HANDLERS (called by the async Actor) ---

    /// Handles an incoming UDP packet from the network.
    pub fn handle_udp_packet(&mut self, now: Instant, packet: str0m::net::Receive) {
        let input = Input::Receive(now.into(), packet);
        if let Err(e) = self.rtc.handle_input(input) {
            tracing::error!(participant_id = %self.participant_id, "Rtc::handle_input error: {}", e);
        }
    }

    /// Handles an RTP packet forwarded from another participant in the room.
    /// This method finds the correct outgoing media slot and writes the packet.
    pub fn handle_forward_rtp(&mut self, track_meta: Arc<track::TrackMeta>, rtp: Arc<RtpPacket>) {
        let Some(mid) = self.get_slot(&track_meta, &rtp.header.ext_vals) else {
            return;
        };

        let pt = {
            let Some(media) = self.rtc.media(mid) else {
                tracing::warn!(%mid, "no media found for mid; dropping forwarded RTP");
                return;
            };
            let Some(&pt) = media.remote_pts().first() else {
                tracing::warn!(%mid, "no negotiated payload type for mid; dropping forwarded RTP");
                return;
            };
            pt
        };

        let mut api = self.rtc.direct_api();
        let Some(writer) = api.stream_tx_by_mid(mid, rtp.header.ext_vals.rid) else {
            tracing::warn!(%mid, "no TxStream for mid; cannot forward RTP");
            return;
        };

        if let Err(err) = writer.write_rtp(
            pt,
            rtp.seq_no,
            rtp.header.timestamp,
            rtp.timestamp,
            rtp.header.marker,
            rtp.header.ext_vals.clone(),
            true,
            rtp.payload.clone(),
        ) {
            tracing::error!(%mid, %err, "failed to forward RTP packet");
        }
    }

    // --- SYNCHRONOUS ENGINE PROCESSING ---

    /// Polls the WebRTC engine, converting all outputs into `Effect`s.
    /// This should be called in a tight loop with a budget by the actor.
    pub fn poll_engine(&mut self) -> Result<Option<Duration>, ParticipantError> {
        let output = self
            .rtc
            .poll_output()
            .map_err(|err| ParticipantError::RtcError(err))?;

        match output {
            Output::Transmit(tx) => {
                // Convert Transmit output into an Effect for the actor to handle.
                self.batcher.push_back(tx.destination, &tx.contents);
            }
            Output::Event(event) => {
                // Handle internal events directly.
                self.handle_event(event);
            }
            Output::Timeout(deadline) => {
                // The actor will observe this and schedule the next wakeup.
                let now = tokio::time::Instant::now().into_std();
                let duration = deadline.saturating_duration_since(now);
                if !duration.is_zero() {
                    return Ok(Some(duration));
                }
                let _ = self.rtc.handle_input(Input::Timeout(now));
            }
        }

        Ok(None)
    }

    // --- INTERNAL LOGIC ---

    pub fn get_slot(
        &mut self,
        track_meta: &Arc<TrackMeta>,
        ext_vals: &ExtensionValues,
    ) -> Option<Mid> {
        match track_meta.kind {
            MediaKind::Video => self.video_allocator.get_slot(&track_meta.id),
            MediaKind::Audio => self.audio_allocator.get_slot(
                &track_meta.id,
                &AudioTrackData {
                    audio_level: ext_vals.audio_level,
                    voice_activity: ext_vals.voice_activity,
                },
            ),
        }
    }

    /// Processes a snapshot of all tracks currently available in the room.
    pub fn handle_published_tracks(
        &mut self,
        tracks: &HashMap<Arc<entity::TrackId>, track::TrackReceiver>,
    ) {
        for track_handle in tracks.values() {
            if track_handle.meta.id.origin_participant == self.participant_id {
                continue; // Ignore loopback of our own tracks.
            }
            self.add_available_track(track_handle);
        }
    }

    /// Adds a locally published track, making it available to send RTP packets.
    pub fn add_published_track(&mut self, track: track::TrackSender) {
        self.published_tracks
            .insert(track.meta.id.origin_mid, track);
    }

    /// Removes tracks that are no longer available in the room.
    pub fn remove_available_tracks(
        &mut self,
        tracks: &HashMap<Arc<entity::TrackId>, track::TrackReceiver>,
    ) {
        for (track_id, track_handle) in tracks.iter() {
            match track_handle.meta.kind {
                MediaKind::Video => {
                    self.video_allocator
                        .remove_track(&mut self.effects, track_id);
                }
                MediaKind::Audio => {
                    self.audio_allocator.remove_track(track_id);
                }
            }
        }
    }

    /// Handles internal events from the `Rtc` engine.
    fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) => {
                if state.is_disconnected() {
                    self.effects.push_back(Effect::Disconnect);
                }
            }
            Event::MediaAdded(media) => self.handle_media_added(media),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp),
            _ => {}
        }
    }

    /// Handles an RTP packet received from the remote peer.
    fn handle_incoming_rtp(&mut self, mut rtp: RtpPacket) {
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx(&rtp.header.ssrc) else {
            tracing::warn!("no stream_rx matched rtp ssrc, dropping.");
            return;
        };

        let mid = stream.mid();
        let Some(track) = self.published_tracks.get_mut(&mid) else {
            tracing::warn!("no published track matched mid, dropping.");
            return;
        };

        rtp.header.ext_vals.rid = stream.rid();
        track.send(stream.rid().as_ref(), rtp);
    }

    /// Handles a new media section being added to the session.
    fn handle_media_added(&mut self, media: MediaAdded) {
        match media.direction {
            Direction::RecvOnly => self.handle_incoming_media_stream(media),
            Direction::SendOnly => self.allocate_outgoing_media_slot(media),
            dir => {
                tracing::warn!("Unsupported media direction {:?}, disconnecting", dir);
                self.effects.push_back(Effect::Disconnect);
            }
        }
    }

    /// A remote peer is publishing a new track to us. We need to create a
    /// local `Track` and publish it to the room.
    fn handle_incoming_media_stream(&mut self, media: MediaAdded) {
        let track_id = Arc::new(entity::TrackId::new(self.participant_id.clone(), media.mid));
        let track_meta = Arc::new(track::TrackMeta {
            id: track_id,
            kind: media.kind,
            simulcast_rids: media.simulcast.map(|s| s.recv),
        });

        tracing::info!("Published new track from remote peer: {:?}", track_meta);
        self.effects.push_back(Effect::SpawnTrack(track_meta));
    }

    /// We have negotiated a new outgoing media stream. We need to allocate a
    /// slot for it in the corresponding allocator.
    fn allocate_outgoing_media_slot(&mut self, media: MediaAdded) {
        match media.kind {
            MediaKind::Video => self.video_allocator.add_slot(&mut self.effects, media.mid),
            MediaKind::Audio => self.audio_allocator.add_slot(media.mid),
        }
    }

    /// Adds a track from the room that this participant can subscribe to.
    fn add_available_track(&mut self, track_handle: &track::TrackReceiver) {
        match track_handle.meta.kind {
            MediaKind::Video => {
                self.video_allocator
                    .add_track(&mut self.effects, track_handle.clone());
            }
            MediaKind::Audio => {
                self.audio_allocator
                    .add_track(&mut self.effects, track_handle.clone());
            }
        }
    }
}
