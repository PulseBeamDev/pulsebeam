use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use super::*;
use crate::control::native::Preparation;
use crate::control::native::RtpPublication as RtpPublicationPlan;
use crate::participant::policy::{ExecutionFailureKind, PublicationExecution};
#[cfg(test)]
use pulsebeam_proto::state as wire;
use pulsebeam_rtc::{CompiledRtpBinding, CompiledRtpConfigurationPatch};

#[cfg(test)]
mod tests;

struct RtpPublication {
    track: Track,
    slot: UpstreamSlotKey,
    binding: CompiledRtpBinding,
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
    active: BTreeSet<u64>,
}

impl NativeRtp {
    pub(super) fn owns_reference(&self, reference: u64) -> bool {
        self.publications.contains_key(&reference)
    }

    pub(super) fn owns_encoding(&self, ssrc: u32) -> bool {
        self.ssrcs.contains_key(&ssrc)
    }

    pub(super) fn ingress_route(
        &self,
        ssrc: u32,
        mid: Mid,
        rid: Option<crate::media::Rid>,
    ) -> Option<IncomingRtpRoute> {
        let reference = self.ssrcs.get(&ssrc)?;
        let source = self.publications.get(reference)?;
        if !source.enabled || !source.demanded || source.retiring || source.fanout.is_none() {
            return None;
        }
        Some(IncomingRtpRoute {
            ssrc: ssrc.into(),
            mid,
            rid,
            encoding: crate::track::SourceEncoding::Ssrc(ssrc.into()),
            upstream_slot: source.slot,
            track_id: source.track.id(),
            fanout: source.fanout,
        })
    }

    pub(super) fn bind_source(&mut self, track: TrackId, handle: TrackHandle) -> bool {
        let Some(reference) = self.tracks.get(track.as_bytes()).copied() else {
            return false;
        };
        let Some(source) = self.publications.get_mut(&reference) else {
            return false;
        };
        if source.track.id() != track {
            return false;
        }
        source.fanout = Some(handle);
        true
    }

    pub(super) fn keyframe_mid(
        &self,
        track: TrackId,
        encoding: crate::track::SourceEncoding,
    ) -> Option<Mid> {
        let source = self.publications.get(self.tracks.get(track.as_bytes())?)?;
        if source.track.id() != track
            || !source.enabled
            || !source.demanded
            || source.retiring
            || !source
                .binding
                .description()
                .encodings
                .iter()
                .any(|bound| matches!(encoding, crate::track::SourceEncoding::Ssrc(ssrc) if bound.primary == *ssrc))
        {
            return None;
        }
        Mid::new(&source.binding.description().mid).ok()
    }

    pub(super) fn slot_for_mid(&self, mid: Mid) -> Option<UpstreamSlotKey> {
        let reference = self.mids.get(&mid)?;
        Some(self.publications.get(reference)?.slot)
    }

    pub(super) fn poll_stats(&self, upstream: &mut UpstreamAllocator, now: Instant) {
        for reference in &self.active {
            if let Some(source) = self.publications.get(reference)
                && let Some(track) = upstream.track_mut(source.slot)
            {
                track.poll_stats(now);
            }
        }
    }

    fn detach(&mut self, reference: u64) -> Option<(TrackId, Option<TrackHandle>)> {
        let source = self.publications.get_mut(&reference)?;
        self.active.remove(&reference);
        let track_id = source.track.id();
        if self.tracks.get(track_id.as_bytes()) == Some(&reference) {
            self.tracks.remove(track_id.as_bytes());
        }
        if let Ok(mid) = Mid::new(&source.binding.description().mid)
            && self.mids.get(&mid) == Some(&reference)
        {
            self.mids.remove(&mid);
        }
        for encoding in &source.binding.description().encodings {
            if self.ssrcs.get(&encoding.primary) == Some(&reference) {
                self.ssrcs.remove(&encoding.primary);
            }
        }
        Some((track_id, source.fanout.take()))
    }
}

fn execution_bytes(spec_bytes: usize, encodings: usize) -> usize {
    // Prepay sparse B-tree/hash backing, monitor state, transport encoding caches
    // and cloned immutable binding/descriptor storage, including cutoff retention.
    std::mem::size_of::<RtpPublication>()
        .saturating_add(std::mem::size_of::<(
            usize,
            crate::participant::upstream::UpstreamSlot,
        )>())
        .saturating_add(std::mem::size_of::<((u8, [u8; 16]), UpstreamSlotKey)>())
        .saturating_add(std::mem::size_of::<((u8, [u8; 16]), TrackHandle)>())
        .saturating_add(std::mem::size_of::<(u64, TrackId)>())
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
        .saturating_add(spec_bytes.saturating_mul(16))
        .saturating_add(Connection::native_execution_metadata_bytes())
}

impl Connection {
    pub(crate) fn native_rtp_execution_bytes(spec_bytes: usize, encodings: usize) -> usize {
        execution_bytes(spec_bytes, encodings)
    }

    pub(super) fn prepare_native_rtp(
        &mut self,
        work: Preparation,
        plan: RtpPublicationPlan,
        events: &mut impl ParticipantSink,
    ) {
        let Some(work) = self.current_native_preparation(work) else {
            return;
        };
        let RtpPublicationPlan {
            label,
            kind,
            mut binding,
            format,
            enabled,
        } = plan;
        tracing::debug!(
            participant = ?self.participant_id,
            connection = ?self.connection_id,
            reference = work.reference,
            ?kind,
            %label,
            "native RTP publication preparation"
        );
        let reference = work.reference;
        if let Some(source) = self.native_rtp.publications.get(&reference) {
            let active = enabled && source.demanded;
            binding.set_active(active);
            let previous = source.binding.clone();
            let track = source.track.clone();
            if let Err(error) = self
                .transport
                .update_compiled_rtp(CompiledRtpConfigurationPatch {
                    inbound: vec![binding.clone()],
                    ..Default::default()
                })
            {
                self.complete_native_preparation(
                    work,
                    Err(ExecutionFailureKind::from(error).into()),
                );
                return;
            }
            let execution = PublicationExecution {
                track,
                enabled,
                active,
            };
            if let Some(now) = self.finish_native_preparation(work) {
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
                self.record_native_publication(work, execution, now);
            } else if self
                .transport
                .update_compiled_rtp(CompiledRtpConfigurationPatch {
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
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Invalid.into()));
            return;
        };
        let Ok(mid) = Mid::new(&binding.description().mid) else {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Unsupported.into()));
            return;
        };
        if self.native_rtp.mids.contains_key(&mid)
            || binding
                .description()
                .encodings
                .iter()
                .any(|encoding| self.native_rtp.ssrcs.contains_key(&encoding.primary))
        {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::Conflict.into()));
            return;
        }
        let id = self
            .connection_id
            .derive_publication_track_id(kind, &label, incarnation);
        let meta = track::TrackMeta {
            room_id: self.room_id,
            shard_id: self.shard_id,
            id,
            origin: self.participant_id,
            connection_id: self.connection_id,
            label: Some(label),
        };
        let (upstream, mut track) = if kind == TrackKind::Audio {
            track::new_audio(mid, meta)
        } else {
            let encodings = binding
                .description()
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
                self.complete_native_preparation(
                    work,
                    Err(ExecutionFailureKind::Unsupported.into()),
                );
                return;
            };
            track::new_video_ssrcs(mid, meta, encodings)
        };
        match &mut track {
            Track::Audio(audio) => audio.format = format,
            Track::Video(video) => video.format = format,
            Track::Data(_) => {
                self.complete_native_preparation(work, Err(ExecutionFailureKind::Internal.into()));
                return;
            }
        }
        if let Err(error) = self
            .transport
            .update_compiled_rtp(CompiledRtpConfigurationPatch {
                inbound: vec![binding.clone()],
                ..Default::default()
            })
        {
            self.complete_native_preparation(work, Err(ExecutionFailureKind::from(error).into()));
            return;
        }
        let execution = PublicationExecution {
            track: track.clone(),
            enabled,
            active: false,
        };
        let Some(now) = self.native_preparation_time(work) else {
            if self
                .transport
                .update_compiled_rtp(CompiledRtpConfigurationPatch {
                    remove_encodings: binding
                        .description()
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
                self.defer_native_preparation(work);
            }
            return;
        };
        let Some(slot) = self.upstream.insert_managed_track(mid, upstream) else {
            if self
                .transport
                .update_compiled_rtp(CompiledRtpConfigurationPatch {
                    remove_encodings: binding
                        .description()
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
                self.complete_native_preparation(work, Err(ExecutionFailureKind::Conflict.into()));
            }
            return;
        };
        self.native_execution.finish(work.scope());
        self.native_rtp.tracks.insert(*id.as_bytes(), reference);
        self.native_rtp.mids.insert(mid, reference);
        for encoding in &binding.description().encodings {
            self.native_rtp.ssrcs.insert(encoding.primary, reference);
        }
        self.native_rtp.publications.insert(
            reference,
            RtpPublication {
                track: track.clone(),
                slot,
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
        self.record_native_publication(work, execution, now);
    }

    pub(crate) fn native_rtp_demand(&mut self, track: TrackId, demanded: bool) {
        let Some(reference) = self.native_rtp.tracks.get(track.as_bytes()).copied() else {
            return;
        };
        let Some(source) = self.native_rtp.publications.get(&reference) else {
            return;
        };
        if source.track.id() != track {
            return;
        }
        let active = source.enabled && demanded;
        for encoding in &source.binding.description().encodings {
            if encoding.active != active
                && self
                    .transport
                    .set_rtp_encoding_active(encoding.primary, active)
                    .is_err()
            {
                self.disconnect(DisconnectReason::SignalingOutput);
                return;
            }
        }
        if let Some(source) = self.native_rtp.publications.get_mut(&reference) {
            source.demanded = demanded;
            source.binding.set_active(active);
        }
        if active {
            self.native_rtp.active.insert(reference);
        } else {
            self.native_rtp.active.remove(&reference);
            self.discard_native_keyframes(reference);
        }
        self.record_native_resource_observation(
            crate::participant::policy::ResourceObservation::Activity {
                binding: reference,
                active,
            },
        );
    }

    fn discard_native_keyframes(&mut self, reference: u64) {
        let Some(source) = self.native_rtp.publications.get(&reference) else {
            return;
        };
        let Ok(mid) = Mid::new(&source.binding.description().mid) else {
            return;
        };
        self.transport.discard_keyframes(mid);
        for encoding in &source.binding.description().encodings {
            let encoding = crate::track::SourceEncoding::Ssrc(encoding.primary.into());
            self.last_keyframe_request.remove(&(mid, encoding));
            self.pending_keyframe_requests.remove(&(mid, encoding));
        }
    }

    pub(super) fn apply_native_retirement(
        &mut self,
        scope: crate::participant::policy::ExecutionScope,
        events: &mut impl ParticipantSink,
    ) {
        if !self.native_execution.retire(scope) {
            return;
        }
        let reference = scope.binding;
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
        self.detach_native_rtp(reference);
        let Some(source) = self.native_rtp.publications.get_mut(&reference) else {
            return;
        };
        source.retiring = true;
        source.enabled = false;
        let remove_encodings = source
            .binding
            .description()
            .encodings
            .iter()
            .map(|encoding| encoding.primary)
            .collect();
        let meta = source.track.meta().clone();
        for encoding in &source.binding.description().encodings {
            self.transport.retire_encoding(encoding.primary);
        }
        if self
            .transport
            .update_compiled_rtp(CompiledRtpConfigurationPatch {
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

    fn detach_native_rtp(&mut self, reference: u64) {
        if let Some((track, Some(handle))) = self.native_rtp.detach(reference) {
            self.upstream.unbind_track_handle(track, handle);
        }
    }

    pub(super) fn fence_native_resource(&mut self, reference: u64) {
        self.native_execution.fence(reference);
        self.fence_native_data(reference);
        self.fence_native_receiver(reference);
        self.discard_native_keyframes(reference);
        self.detach_native_rtp(reference);
        if let Some(source) = self.native_rtp.publications.get_mut(&reference) {
            source.enabled = false;
            for encoding in &source.binding.description().encodings {
                self.transport.retire_encoding(encoding.primary);
            }
        }
    }

    pub(super) fn native_resource_ended(&mut self, reference: u64) {
        self.fence_native_resource(reference);
        if self.disconnect_reason.is_some() {
            return;
        }
        self.record_native_resource_observation(
            crate::participant::policy::ResourceObservation::Ended { binding: reference },
        );
    }

    pub(super) fn native_rtp_ended(&mut self, ssrc: u32) {
        let Some(reference) = self.native_rtp.ssrcs.get(&ssrc).copied() else {
            return;
        };
        self.native_resource_ended(reference);
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
        let track_id = source.track.id();
        let slot = source.slot;
        self.native_rtp.publications.remove(&reference);
        self.upstream.remove_managed_track(slot, track_id);
        self.record_native_resource_observation(
            crate::participant::policy::ResourceObservation::Released { binding: reference },
        );
    }
}
