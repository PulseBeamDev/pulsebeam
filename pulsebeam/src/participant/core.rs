use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use pulsebeam_runtime::net;
use str0m::{
    Event, Input, Output, Rtc,
    media::{Direction, MediaAdded, MediaKind, Mid},
    rtp::{ExtensionValues, RtpPacket},
};

use crate::entity::TrackId;
use crate::participant::downstream::DownstreamManager;
use crate::{
    entity,
    participant::{
        audio::{AudioAllocator, AudioTrackData},
        batcher::Batcher,
        effect::{self, Effect},
        video::VideoAllocator,
    },
    track::{self, TrackMeta, TrackReceiver, TrackSender},
};

const RTC_POLL_BUDGET: usize = 64;

/// Core per-participant state and synchronous media management logic.
///
/// This struct is a self-contained, synchronous state machine. It knows nothing
/// about async runtimes or I/O. It accepts inputs via its `handle_*` methods
/// and produces a queue of `Effect`s that the async `ParticipantActor` must apply.
pub struct ParticipantCore {
    pub participant_id: Arc<entity::ParticipantId>,
    pub rtc: Rtc,
    pub batcher: Batcher,
    pub effects: effect::Queue,
    pub published_tracks: HashMap<Mid, TrackSender>,
    video_allocator: VideoAllocator,
    audio_allocator: AudioAllocator,
    pub downstream_manager: DownstreamManager,
}

impl ParticipantCore {
    /// Creates a new `ParticipantCore` with a configured `Rtc` instance and `Batcher`.
    pub fn new(participant_id: Arc<entity::ParticipantId>, rtc: Rtc, batcher: Batcher) -> Self {
        Self {
            participant_id,
            rtc,
            batcher,
            published_tracks: HashMap::new(),
            video_allocator: VideoAllocator::default(),
            audio_allocator: AudioAllocator::default(),
            downstream_manager: DownstreamManager::new(),
            effects: VecDeque::with_capacity(64),
        }
    }

    // --- INPUT HANDLERS (called by the async Actor) ---

    pub fn handle_udp_packet(&mut self, packet: net::RecvPacket) {
        let contents = match (&*packet.buf).try_into() {
            Ok(contents) => contents,
            Err(err) => {
                tracing::warn!("invalid packet: {err}");
                return;
            }
        };

        tracing::trace!(?contents, "handle udp packet");
        let recv = str0m::net::Receive {
            proto: str0m::net::Protocol::Udp,
            source: packet.src,
            destination: packet.dst,
            contents,
        };
        if let Err(e) = self
            .rtc
            .handle_input(Input::Receive(tokio::time::Instant::now().into_std(), recv))
        {
            tracing::error!(participant_id = %self.participant_id, "Rtc::handle_input error: {}", e);
        }

        // str0m only holds at most 1 rtp for receiving..
        self.poll_rtc();
    }

    pub fn handle_published_tracks(
        &mut self,
        tracks: &HashMap<Arc<entity::TrackId>, TrackReceiver>,
    ) {
        for track_handle in tracks.values() {
            if track_handle.meta.id.origin_participant == self.participant_id {
                continue; // Ignore loopback.
            }
            // Let the allocator know about the track. It will return it if we should subscribe.
            self.add_available_track(track_handle);
        }
    }

    pub fn subscribe_track(&mut self, track_id: Arc<TrackId>) {
        self.downstream_manager.resume_track(&track_id);
    }

    pub fn remove_available_tracks(
        &mut self,
        tracks: &HashMap<Arc<entity::TrackId>, TrackReceiver>,
    ) {
        for (track_id, track_handle) in tracks.iter() {
            match track_handle.meta.kind {
                MediaKind::Video => {
                    self.video_allocator
                        .remove_track(&mut self.effects, track_id);
                    self.downstream_manager.remove_track(track_id);
                }
                MediaKind::Audio => {
                    self.audio_allocator.remove_track(track_id);
                    self.downstream_manager.remove_track(track_id);
                }
            }
        }
    }

    // --- SYNCHRONOUS WORK CYCLE ---

    /// The main synchronous work function. Drains media, handles timeouts, and polls the engine.
    pub fn tick(&mut self) -> Option<Duration> {
        // self.rtc.handle_input(Input::Timeout(now)).ok()?;

        // for _ in 0..RTC_POLL_BUDGET {
        //     if !self.rtc.is_alive() {
        //         self.effects.push_back(Effect::Disconnect);
        //         return None;
        //     }
        //     match self.rtc.poll_output() {
        //         Ok(Output::Timeout(t)) => {
        //             deadline = Some(t.saturating_duration_since(now));
        //             break;
        //         }
        //         Ok(Output::Transmit(tx)) => {
        //             self.batcher.push_back(tx.destination, &tx.contents);
        //         }
        //         Ok(Output::Event(event)) => {
        //             self.handle_event(event);
        //         }
        //         Err(_) => {
        //             self.effects.push_back(Effect::Disconnect);
        //             return None;
        //         }
        //     }
        // }
        // deadline

        self.poll_rtc()
    }

    fn poll_rtc(&mut self) -> Option<Duration> {
        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    let now = tokio::time::Instant::now().into_std();
                    let duration = deadline.saturating_duration_since(now);
                    if duration.is_zero() {
                        let _ = self.rtc.handle_input(Input::Timeout(now));
                        continue;
                    }
                    return Some(duration);
                }
                Ok(Output::Transmit(tx)) => {
                    self.batcher.push_back(tx.destination, &tx.contents);
                }
                Ok(Output::Event(event)) => {
                    self.handle_event(event);
                }
                Err(_) => {
                    self.rtc.disconnect();
                }
            }
        }
        None
    }

    // --- STATE MODIFICATION ---

    pub fn add_published_track(&mut self, track: TrackSender) {
        self.published_tracks
            .insert(track.meta.id.origin_mid, track);
    }

    // --- PRIVATE HELPERS ---

    pub fn handle_forward_rtp(&mut self, track_meta: Arc<TrackMeta>, rtp: Arc<RtpPacket>) {
        let Some(mid) = self.get_slot(&track_meta, &rtp.header.ext_vals) else {
            tracing::trace!(?track_meta, "no slot found");
            return;
        };
        let Some(pt) = self
            .rtc
            .media(mid)
            .and_then(|m| m.remote_pts().first().copied())
        else {
            tracing::warn!(?track_meta, ?mid, "no matched pt");
            return;
        };

        let mut api = self.rtc.direct_api();
        let Some(writer) = api.stream_tx_by_mid(mid, None) else {
            tracing::warn!(?track_meta, ?mid, "no stream_tx found");
            return;
        };

        let _ = writer.write_rtp(
            pt,
            rtp.seq_no,
            rtp.header.timestamp,
            rtp.timestamp,
            rtp.header.marker,
            rtp.header.ext_vals.clone(),
            true,
            rtp.payload.clone(),
        );
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::IceConnectionStateChange(state) if state.is_disconnected() => {
                self.effects.push_back(Effect::Disconnect);
            }
            Event::MediaAdded(media) => self.handle_media_added(media),
            Event::RtpPacket(rtp) => self.handle_incoming_rtp(rtp),
            Event::KeyframeRequest(req) => {
                let Some(track_id) = self.video_allocator.get_track(&req.mid) else {
                    tracing::warn!(?req, "no track found from slots");
                    return;
                };

                self.downstream_manager.request_keyframe(track_id);
            }
            _ => {}
        }
    }

    fn handle_incoming_rtp(&mut self, mut rtp: RtpPacket) {
        let mut api = self.rtc.direct_api();
        let Some(stream) = api.stream_rx(&rtp.header.ssrc) else {
            tracing::warn!(?rtp.header.ssrc, "no stream_rx found");
            return;
        };
        let mid = stream.mid();
        let rid = stream.rid();

        let Some(track) = self.published_tracks.get_mut(&mid) else {
            tracing::warn!(?rtp.header.ssrc, ?mid, "no published_tracks found");
            return;
        };

        rtp.header.ext_vals.rid = rid;
        track.send(rid.as_ref(), rtp);
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        match media.direction {
            Direction::RecvOnly => self.handle_incoming_media_stream(media),
            Direction::SendOnly => self.allocate_outgoing_media_slot(media),
            _ => self.effects.push_back(Effect::Disconnect),
        }
    }

    fn handle_incoming_media_stream(&mut self, media: MediaAdded) {
        let track_id = Arc::new(entity::TrackId::new(self.participant_id.clone(), media.mid));
        let track_meta = Arc::new(track::TrackMeta {
            id: track_id,
            kind: media.kind,
            simulcast_rids: media.simulcast.map(|s| s.recv),
        });
        self.effects.push_back(Effect::SpawnTrack(track_meta));
    }

    fn allocate_outgoing_media_slot(&mut self, media: MediaAdded) {
        match media.kind {
            MediaKind::Video => self.video_allocator.add_slot(&mut self.effects, media.mid),
            MediaKind::Audio => self.audio_allocator.add_slot(media.mid),
        }
    }

    /// Informs allocators about a new track. Returns `Some(TrackReceiver)` if a subscription is desired.
    fn add_available_track(&mut self, track: &TrackReceiver) {
        match track.meta.kind {
            MediaKind::Video => self
                .video_allocator
                .add_track(&mut self.effects, track.meta.id.clone()),
            MediaKind::Audio => self
                .audio_allocator
                .add_track(&mut self.effects, track.meta.id.clone()),
        }
        self.downstream_manager.add_track(track.clone());
    }

    fn get_slot(&mut self, track_meta: &Arc<TrackMeta>, ext_vals: &ExtensionValues) -> Option<Mid> {
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
}
