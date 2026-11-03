use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use pulsebeam_proto::prelude::Message;
use slotmap::Key as _;

use super::*;
use crate::participant::native::{Preparation, track_descriptor};
use crate::participant::rtp::{failure_code, publication_binding};
use pulsebeam_proto::state::{self as wire, resource_spec};
use pulsebeam_rtc::{RtpBinding, RtpConfigurationPatch};

#[cfg(test)]
mod tests;

struct RtpPublication {
    track: Track,
    upstream: track::UpstreamTrack,
    binding: RtpBinding,
    enabled: bool,
    demanded: bool,
    retiring: bool,
    fanout: Option<TrackHandle>,
}

#[derive(Default)]
pub(super) struct NativeRtp {
    publications: BTreeMap<u64, RtpPublication>,
    tracks: BTreeMap<[u8; 16], u64>,
    mids: BTreeMap<Mid, u64>,
    ssrcs: BTreeMap<u32, u64>,
    fanouts: BTreeMap<u64, u64>,
    active: BTreeSet<u64>,
}

impl NativeRtp {
    pub(super) fn owns_encoding(&self, ssrc: u32) -> bool {
        self.ssrcs.contains_key(&ssrc)
    }

    pub(super) fn bind_source(&mut self, track: TrackId, handle: TrackHandle) {
        let Some(reference) = self.tracks.get(track.as_bytes()).copied() else {
            return;
        };
        let Some(source) = self.publications.get_mut(&reference) else {
            return;
        };
        if let Some(previous) = source.fanout {
            self.fanouts.remove(&previous.data().as_ffi());
        }
        source.fanout = Some(handle);
        self.fanouts.insert(handle.data().as_ffi(), reference);
    }

    pub(super) fn track_for_fanout(&self, handle: TrackHandle) -> Option<TrackId> {
        let reference = self.fanouts.get(&handle.data().as_ffi())?;
        Some(self.publications.get(reference)?.track.id())
    }

    pub(super) fn keyframe_mid(
        &self,
        track: TrackId,
        encoding: crate::track::SourceEncoding,
    ) -> Option<Mid> {
        let source = self.publications.get(self.tracks.get(track.as_bytes())?)?;
        if !source.enabled
            || !source.demanded
            || source.retiring
            || !source
                .binding
                .encodings
                .iter()
                .any(|bound| matches!(encoding, crate::track::SourceEncoding::Ssrc(ssrc) if bound.primary == *ssrc))
        {
            return None;
        }
        Mid::new(&source.binding.mid).ok()
    }

    pub(super) fn sender_report(
        &mut self,
        mid: Mid,
        report: crate::rtp::sync::SenderInfo,
        arrival: Instant,
    ) {
        if let Some(reference) = self.mids.get(&mid)
            && let Some(source) = self.publications.get_mut(reference)
        {
            source.upstream.sender_report(report, arrival);
        }
    }

    pub(super) fn poll_stats(&mut self, now: Instant) {
        for reference in &self.active {
            if let Some(source) = self.publications.get_mut(reference) {
                source.upstream.poll_stats(now);
            }
        }
    }

    fn detach(&mut self, reference: u64) {
        let Some(source) = self.publications.get_mut(&reference) else {
            return;
        };
        self.active.remove(&reference);
        self.tracks.remove(source.track.id().as_bytes());
        if let Ok(mid) = Mid::new(&source.binding.mid) {
            self.mids.remove(&mid);
        }
        for encoding in &source.binding.encodings {
            self.ssrcs.remove(&encoding.primary);
        }
        if let Some(handle) = source.fanout.take() {
            self.fanouts.remove(&handle.data().as_ffi());
        }
    }
}

fn declaration(spec: &wire::ResourceSpec) -> Option<(&str, wire::TrackKind, &wire::RtpParameters)> {
    let resource_spec::Declaration::Publication(publication) = spec.declaration.as_ref()? else {
        return None;
    };
    let (kind, binding) = match publication.kind.as_ref()? {
        wire::publication_spec::Kind::Audio(audio) => {
            (wire::TrackKind::Audio, audio.binding.as_ref()?)
        }
        wire::publication_spec::Kind::Video(video) => {
            (wire::TrackKind::Video, video.binding.as_ref()?)
        }
        _ => return None,
    };
    let wire::track_binding::Transport::Rtp(parameters) = binding.transport.as_ref()? else {
        return None;
    };
    Some((&publication.label, kind, parameters))
}

fn execution_bytes(work: &Preparation, encodings: usize) -> usize {
    // Prepay sparse B-tree/hash backing, monitor state, transport encoding caches
    // and cloned immutable binding/descriptor storage, including cutoff retention.
    std::mem::size_of::<RtpPublication>()
        .saturating_add(256)
        .saturating_mul(12)
        .saturating_add(
            encodings.saturating_mul(
                std::mem::size_of::<track::UpstreamTrackLayer>()
                    .saturating_add(std::mem::size_of::<track::TrackLayer>())
                    .saturating_add(1024)
                    .saturating_mul(12),
            ),
        )
        .saturating_add(work.spec.encoded_len().saturating_mul(16))
}

impl Participant {
    pub(super) fn prepare_native_rtp(
        &mut self,
        work: Preparation,
        events: &mut impl ParticipantSink,
    ) {
        let result = self.native_rtp_binding(&work);
        let (label, kind, mut binding) = match result {
            Ok(value) => value,
            Err(code) => {
                self.complete_native_preparation(work, Err(failure(code)));
                return;
            }
        };
        tracing::debug!(
            participant = ?self.participant_id,
            connection = ?self.connection_id,
            reference = work.reference,
            ?kind,
            %label,
            "native RTP publication preparation"
        );
        let enabled = work
            .spec
            .settings
            .as_ref()
            .is_some_and(|settings| settings.enabled == Some(true));
        let reference = work.reference;
        if let Some(source) = self.native_rtp.publications.get(&reference) {
            let active = enabled && source.demanded;
            for encoding in &mut binding.encodings {
                encoding.active = active;
            }
            let previous = source.binding.clone();
            let descriptor = track_descriptor(&source.track);
            if let Err(error) = self.transport.update_rtp(RtpConfigurationPatch {
                inbound: vec![binding.clone()],
                ..Default::default()
            }) {
                self.complete_native_preparation(work, Err(failure(failure_code(error))));
                return;
            }
            let live = self.native_rtp_live(&work, descriptor, enabled, active);
            if self.complete_native_preparation(work, Ok(live)) {
                if let Some(source) = self.native_rtp.publications.get_mut(&reference) {
                    source.enabled = enabled;
                    source.binding = binding;
                }
                if active {
                    self.native_rtp.active.insert(reference);
                } else {
                    self.native_rtp.active.remove(&reference);
                }
                if !active {
                    self.discard_native_keyframes(reference);
                }
            } else if self
                .transport
                .update_rtp(RtpConfigurationPatch {
                    inbound: vec![previous],
                    ..Default::default()
                })
                .is_err()
            {
                self.disconnect(DisconnectReason::SignalingOutput);
            }
            return;
        }
        let Some(incarnation) = NonZeroU64::new(reference) else {
            self.complete_native_preparation(
                work,
                Err(failure(wire::FailureCode::InvalidConfiguration)),
            );
            return;
        };
        let Ok(mid) = Mid::new(&binding.mid) else {
            self.complete_native_preparation(work, Err(failure(wire::FailureCode::Unsupported)));
            return;
        };
        if self.native_rtp.mids.contains_key(&mid)
            || binding
                .encodings
                .iter()
                .any(|encoding| self.native_rtp.ssrcs.contains_key(&encoding.primary))
        {
            self.complete_native_preparation(work, Err(failure(wire::FailureCode::Incompatible)));
            return;
        }
        if self
            .signaling
            .reserve_execution(reference, execution_bytes(&work, binding.encodings.len()))
            .is_err()
        {
            self.complete_native_preparation(
                work,
                Err(failure(wire::FailureCode::CapacityExceeded)),
            );
            return;
        }
        let track_kind = if kind == wire::TrackKind::Audio {
            TrackKind::Audio
        } else {
            TrackKind::Video
        };
        let id = self
            .connection_id
            .derive_publication_track_id(track_kind, &label, incarnation);
        let meta = track::TrackMeta {
            room_id: self.room_id,
            shard_id: self.shard_id,
            id,
            origin: self.participant_id,
            connection_id: self.connection_id,
            label: Some(label),
        };
        let (upstream, mut track) = if kind == wire::TrackKind::Audio {
            track::new_audio(mid, meta)
        } else {
            let encodings = binding
                .encodings
                .iter()
                .map(|encoding| {
                    encoding
                        .rid
                        .as_deref()
                        .map(crate::media::Rid::new)
                        .transpose()
                        .map(|rid| (encoding.primary.into(), rid))
                })
                .collect::<Result<Vec<_>, _>>();
            let Ok(encodings) = encodings else {
                let _ = self.signaling.reserve_execution(reference, 0);
                self.complete_native_preparation(
                    work,
                    Err(failure(wire::FailureCode::Unsupported)),
                );
                return;
            };
            track::new_video_ssrcs(mid, meta, encodings)
        };
        let format = declaration(&work.spec)
            .and_then(|(_, _, parameters)| parameters.codec.as_ref()?.format);
        match &mut track {
            Track::Audio(audio) => audio.format = format,
            Track::Video(video) => video.format = format,
            Track::Data(_) => {
                let _ = self.signaling.reserve_execution(reference, 0);
                self.complete_native_preparation(
                    work,
                    Err(failure(wire::FailureCode::InternalFailure)),
                );
                return;
            }
        }
        if let Err(error) = self.transport.update_rtp(RtpConfigurationPatch {
            inbound: vec![binding.clone()],
            ..Default::default()
        }) {
            let _ = self.signaling.reserve_execution(reference, 0);
            self.complete_native_preparation(work, Err(failure(failure_code(error))));
            return;
        }
        let live = self.native_rtp_live(&work, track_descriptor(&track), enabled, false);
        if !self.complete_native_preparation(work, Ok(live)) {
            if self
                .transport
                .update_rtp(RtpConfigurationPatch {
                    remove_encodings: binding
                        .encodings
                        .iter()
                        .map(|encoding| encoding.primary)
                        .collect(),
                    ..Default::default()
                })
                .is_err()
            {
                self.disconnect(DisconnectReason::SignalingOutput);
            } else {
                let _ = self.signaling.reserve_execution(reference, 0);
            }
            return;
        }
        self.native_rtp.tracks.insert(*id.as_bytes(), reference);
        self.native_rtp.mids.insert(mid, reference);
        for encoding in &binding.encodings {
            self.native_rtp.ssrcs.insert(encoding.primary, reference);
        }
        self.native_rtp.publications.insert(
            reference,
            RtpPublication {
                track: track.clone(),
                upstream,
                binding,
                enabled,
                demanded: false,
                retiring: false,
                fanout: None,
            },
        );
        tracing::debug!(
            participant = ?self.participant_id,
            connection = ?self.connection_id,
            reference,
            track = ?id,
            "native RTP publication advertised"
        );
        events.publish_track(track);
    }

    fn native_rtp_binding(
        &self,
        work: &Preparation,
    ) -> Result<(String, wire::TrackKind, RtpBinding), wire::FailureCode> {
        let (label, kind, parameters) =
            declaration(&work.spec).ok_or(wire::FailureCode::Unsupported)?;
        let capabilities = self
            .negotiated
            .native_rtp_capabilities
            .as_ref()
            .ok_or(wire::FailureCode::Unsupported)?;
        let binding = publication_binding(parameters, kind, capabilities)?;
        // These are the existing production packet processors. Do not classify
        // opaque VP8/VP9 as H.264 or invent RID lanes. Temporal H.264 requires
        // its declared dependency descriptor, not guessed temporal-layer facts.
        let dependency_descriptor = parameters.header_extensions.iter().any(|extension| {
            extension.r#type
                == wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor as i32
        });
        if !matches!(binding.codec.parameters.name.as_str(), "opus" | "h264")
            || kind == wire::TrackKind::Audio
                && (binding.encodings.len() != 1
                    || binding
                        .encodings
                        .iter()
                        .any(|encoding| encoding.rid.is_some()))
            || parameters.encodings.iter().any(|encoding| {
                encoding.scalability_mode.is_some_and(|mode| {
                    mode != wire::RtpScalabilityMode::L1t1 as i32
                        && (mode != wire::RtpScalabilityMode::L1t3 as i32
                            || binding.codec.parameters.name != "h264"
                            || !dependency_descriptor)
                })
            })
        {
            return Err(wire::FailureCode::Unsupported);
        }
        Ok((label.to_owned(), kind, binding))
    }

    fn native_rtp_live(
        &self,
        work: &Preparation,
        track: Option<wire::Track>,
        enabled: bool,
        active: bool,
    ) -> wire::LiveResource {
        wire::LiveResource {
            spec: Some(work.spec.clone()),
            participant: Some(wire::Participant {
                participant_id: self.participant_id.as_str(),
                participant_external_id: self.participant_external_id.as_str().to_owned(),
            }),
            track,
            execution: if enabled {
                wire::ExecutionStatus::Installed
            } else {
                wire::ExecutionStatus::Disabled
            } as i32,
            execution_failure: None,
            activity: Some(if active {
                wire::MediaActivity::Active
            } else {
                wire::MediaActivity::Idle
            } as i32),
        }
    }

    pub(crate) fn native_rtp_demand(&mut self, track: TrackId, demanded: bool) {
        let Some(reference) = self.native_rtp.tracks.get(track.as_bytes()).copied() else {
            return;
        };
        let Some(source) = self.native_rtp.publications.get(&reference) else {
            return;
        };
        let active = source.enabled && demanded;
        let mut binding = source.binding.clone();
        for encoding in &mut binding.encodings {
            encoding.active = active;
        }
        if binding != source.binding
            && self
                .transport
                .update_rtp(RtpConfigurationPatch {
                    inbound: vec![binding.clone()],
                    ..Default::default()
                })
                .is_err()
        {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        }
        if let Some(source) = self.native_rtp.publications.get_mut(&reference) {
            source.demanded = demanded;
            source.binding = binding;
        }
        if active {
            self.native_rtp.active.insert(reference);
        } else {
            self.native_rtp.active.remove(&reference);
            self.discard_native_keyframes(reference);
        }
        if self.signaling.observe_activity(reference, active).is_err() {
            self.disconnect(DisconnectReason::SignalingOutput);
        }
    }

    pub(super) fn handle_native_rtp(
        &mut self,
        packet: pulsebeam_rtc::RtpPacket,
        mid: Mid,
        rid: Option<crate::media::Rid>,
        events: &mut impl ParticipantSink,
    ) {
        if self.disconnect_reason.is_some()
            || self
                .authorization_expiry
                .is_none_or(|expiry| expiry.is_expired_at(native_data::unix_now()))
        {
            return;
        }
        let Some(reference) = self.native_rtp.ssrcs.get(&packet.header.ssrc).copied() else {
            return;
        };
        let Some(source) = self.native_rtp.publications.get(&reference) else {
            return;
        };
        if !source.enabled || !source.demanded || source.retiring || source.fanout.is_none() {
            return;
        }
        let Some(incoming) = self.transport.convert_rtp(packet, mid, rid) else {
            return;
        };
        let Some(source) = self.native_rtp.publications.get_mut(&reference) else {
            return;
        };
        let encoding = crate::track::SourceEncoding::Ssrc(incoming.packet.ssrc);
        let processed = source.upstream.process(rid.as_ref(), incoming.packet, None);
        let fanout = source.fanout;
        let track = source.track.id();
        if processed.request_keyframe {
            self.enqueue_remote_keyframe((track, encoding), KeyframeRequestKind::Pli);
        }
        if let Some(packet) = processed.first {
            events.publish_track_packet(fanout, TrackPacket::Rtp(packet));
        }
        for packet in processed.remaining {
            events.publish_track_packet(fanout, TrackPacket::Rtp(packet));
        }
    }

    fn discard_native_keyframes(&mut self, reference: u64) {
        let Some(source) = self.native_rtp.publications.get(&reference) else {
            return;
        };
        let Ok(mid) = Mid::new(&source.binding.mid) else {
            return;
        };
        self.transport.discard_keyframes(mid);
        for encoding in &source.binding.encodings {
            let encoding = crate::track::SourceEncoding::Ssrc(encoding.primary.into());
            self.last_keyframe_request.remove(&(mid, encoding));
            self.pending_keyframe_requests.remove(&(mid, encoding));
        }
    }

    pub(super) fn retire_native_resource(
        &mut self,
        reference: u64,
        events: &mut impl ParticipantSink,
    ) {
        if !self.native_rtp.publications.contains_key(&reference) {
            self.retire_native_data(reference, events);
            return;
        }
        if self
            .native_rtp
            .publications
            .get(&reference)
            .is_some_and(|source| source.retiring)
        {
            return;
        }
        self.discard_native_keyframes(reference);
        self.native_rtp.detach(reference);
        let Some(source) = self.native_rtp.publications.get_mut(&reference) else {
            return;
        };
        source.retiring = true;
        source.enabled = false;
        let remove_encodings = source
            .binding
            .encodings
            .iter()
            .map(|encoding| encoding.primary)
            .collect();
        let meta = source.track.meta().clone();
        for encoding in &source.binding.encodings {
            self.transport.retire_encoding(encoding.primary);
        }
        if self
            .transport
            .update_rtp(RtpConfigurationPatch {
                remove_encodings,
                ..Default::default()
            })
            .is_err()
        {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        }
        events.unpublish_owned_track(meta, reference);
    }

    pub(super) fn native_rtp_ended(&mut self, ssrc: u32, events: &mut impl ParticipantSink) {
        let Some(reference) = self.native_rtp.ssrcs.get(&ssrc).copied() else {
            return;
        };
        if self.signaling.source_ended(reference).is_err() {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        }
        self.retire_native_resource(reference, events);
    }

    pub(super) fn complete_native_resource_cutoff(&mut self, reference: u64, success: bool) {
        let Some(source) = self.native_rtp.publications.get(&reference) else {
            self.complete_native_data_cutoff(reference, success);
            return;
        };
        if !source.retiring {
            return;
        }
        if !success {
            self.disconnect(DisconnectReason::SignalingOutput);
            return;
        }
        self.native_rtp.publications.remove(&reference);
        if self.signaling.reserve_execution(reference, 0).is_err()
            || self.signaling.cutoff_complete(reference).is_err()
        {
            self.disconnect(DisconnectReason::SignalingOutput);
        }
    }
}
