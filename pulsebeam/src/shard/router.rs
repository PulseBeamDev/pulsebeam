use slotmap::SecondaryMap;
use str0m::media::Rid;

use crate::clock::WallAnchor;
use crate::entity::TrackId;
use crate::id::ShardId;
use crate::keys::ParticipantKey;
use crate::participant::{ParticipantInput, RoutedTrackPacket, TrackPacket};
use crate::route::{Envelope, RouteAction, RouteRuntime};
use crate::rtp::{RtpPacket, cache::TrackStreamCache};

use super::worker::{MediaPayload, Reverse, ShardFrame};

pub(crate) use crate::keys::TrackKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Local,
    Remote,
}

impl Origin {
    fn is_local(self) -> bool {
        matches!(self, Self::Local)
    }
}

pub(crate) trait ShardTransport {
    fn send_media(&self, dst: ShardId, env: Envelope, payload: MediaPayload);
    fn send_frame(&self, dst: ShardId, frame: ShardFrame);
}

pub(crate) struct ForwardingContext<'a, R> {
    pub registry: &'a mut super::participants::ParticipantRegistry,
    pub dirty: &'a mut super::dirty::DirtyTracker,
    pub wall: &'a WallAnchor,
    pub router: &'a R,
}

struct TrackRuntime {
    id: TrackId,
    origin_key: ParticipantKey,
    encodings: Vec<Option<Rid>>,
    cache: Option<TrackStreamCache>,
    link_seq: u32,
}

struct UnreliableRuntime {
    publisher: Option<ParticipantKey>,
    link_seq: u32,
}

/// The same stream, plus what the feedback path needs to address its publisher.
pub(crate) struct ReliableRuntime {
    publisher: Option<ParticipantKey>,
    link_seq: u32,
}

/// Hand a packet to every destination-local recipient in a compiled plan.
fn fanout_local(plan: &crate::plan::FlatTrackPlan, mut deliver: impl FnMut(ParticipantKey)) {
    for &subscriber in plan.local.values() {
        deliver(subscriber);
    }
}

/// Forward a packet to the shards a plan routes to, numbering each hop so the
/// destination can tell loss from reordering.
fn fanout_remote(
    plan: &crate::plan::FlatTrackPlan,
    link_seq: &mut u32,
    playout: u32,
    mut payload: impl FnMut() -> MediaPayload,
    ctx: &impl ShardTransport,
) {
    for remote in plan.remote.values() {
        let env = Envelope::media(*remote, *link_seq, playout);
        *link_seq = link_seq.wrapping_add(1);
        ctx.send_media(remote.shard(), env, payload());
    }
}

fn forward_track(
    ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    subscriber: ParticipantKey,
    fanout: TrackKey,
    pkt: &RtpPacket,
    cache: Option<&TrackStreamCache>,
) {
    let Some(participant) = ctx.registry.resolve_mut(subscriber) else {
        debug_assert!(false, "a track plan must name a live participant");
        return;
    };
    participant.input(ParticipantInput::Track {
        key: fanout,
        packet: pkt,
        cache,
    });
    ctx.dirty.mark(subscriber, participant);
}

fn forward_unreliable(
    registry: &mut super::participants::ParticipantRegistry,
    dirty: &mut super::dirty::DirtyTracker,
    subscriber: ParticipantKey,
    stream: TrackKey,
    pkt: &[u8],
) {
    let Some(participant) = registry.resolve_mut(subscriber) else {
        debug_assert!(false, "a data plan must name a live participant");
        return;
    };
    participant.input(ParticipantInput::Data {
        stream,
        packet: pkt,
    });
    dirty.mark(subscriber, participant);
}

fn forward_reliable(
    registry: &mut super::participants::ParticipantRegistry,
    dirty: &mut super::dirty::DirtyTracker,
    subscriber: ParticipantKey,
    stream: TrackKey,
    frame: &[u8],
) {
    let Some(participant) = registry.resolve_mut(subscriber) else {
        debug_assert!(false, "a reliable data plan must name a live participant");
        return;
    };
    participant.input(ParticipantInput::ReliableData { stream, frame });
    dirty.mark(subscriber, participant);
}

pub(crate) struct ShardRuntime {
    tracks: SecondaryMap<TrackKey, TrackRuntime>,
    unreliable: SecondaryMap<TrackKey, UnreliableRuntime>,
    reliable: SecondaryMap<TrackKey, ReliableRuntime>,
    pub(crate) routes: RouteRuntime,
}

impl ShardRuntime {
    pub fn new(shard_id: ShardId) -> Self {
        Self {
            tracks: SecondaryMap::new(),
            unreliable: SecondaryMap::new(),
            reliable: SecondaryMap::new(),
            routes: RouteRuntime::new(shard_id),
        }
    }

    pub(crate) fn retire_track(&mut self, key: TrackKey) {
        let media = self.tracks.remove(key).is_some();
        let realtime = self.unreliable.remove(key).is_some();
        let reliable = self.reliable.remove(key).is_some();
        debug_assert_eq!(media as u8 + realtime as u8 + reliable as u8, 1);
    }

    pub(crate) fn apply_view_op(&mut self, op: &crate::view::ViewOp) {
        match op {
            crate::view::ViewOp::RetireRoute { handle } => {
                let retired = self.routes.retire(*handle);
                debug_assert!(retired || self.routes.entry(*handle).is_none());
            }
            crate::view::ViewOp::InsertTrackRuntime { key, runtime } => {
                let crate::view::TrackRuntime::Media(descriptor) = runtime else {
                    let crate::view::TrackRuntime::Data {
                        lane, publisher, ..
                    } = runtime
                    else {
                        unreachable!()
                    };
                    match lane {
                        crate::track::DataLane::Realtime => {
                            let previous = self.unreliable.insert(
                                *key,
                                UnreliableRuntime {
                                    publisher: *publisher,
                                    link_seq: 0,
                                },
                            );
                            debug_assert!(
                                previous.is_none()
                                    || previous
                                        .as_ref()
                                        .is_some_and(|old| old.publisher == *publisher)
                            );
                            if let Some(previous) = previous
                                && let Some(current) = self.unreliable.get_mut(*key)
                            {
                                current.link_seq = previous.link_seq;
                            }
                        }
                        crate::track::DataLane::Reliable => {
                            let previous = self.reliable.insert(
                                *key,
                                ReliableRuntime {
                                    publisher: *publisher,
                                    link_seq: 0,
                                },
                            );
                            debug_assert!(
                                previous.is_none()
                                    || previous
                                        .as_ref()
                                        .is_some_and(|old| old.publisher == *publisher)
                            );
                            if let Some(previous) = previous
                                && let Some(current) = self.reliable.get_mut(*key)
                            {
                                current.link_seq = previous.link_seq;
                            }
                        }
                    }
                    return;
                };
                if matches!(descriptor.id.kind(), crate::entity::TrackKind::Data) {
                    debug_assert!(false, "a track runtime key must match its publication kind");
                    return;
                }
                let key = *key;
                if let Some(previous) = self.tracks.get(key) {
                    debug_assert_eq!(
                        previous.id, descriptor.id,
                        "a runtime key cannot change its logical track"
                    );
                    debug_assert_eq!(
                        previous.origin_key, descriptor.origin_key,
                        "a runtime key cannot change its publisher binding"
                    );
                    if previous.id != descriptor.id {
                        return;
                    }
                }
                let previous = self.tracks.insert(
                    key,
                    TrackRuntime {
                        id: descriptor.id,
                        origin_key: descriptor.origin_key,
                        encodings: descriptor.encodings.clone(),
                        cache: None,
                        link_seq: 0,
                    },
                );
                if let Some(previous) = previous {
                    let Some(current) = self.tracks.get_mut(key) else {
                        debug_assert!(false, "inserted track runtime must remain addressable");
                        return;
                    };
                    current.cache = previous.cache;
                    current.link_seq = previous.link_seq;
                }
            }
            crate::view::ViewOp::RemoveTrackRuntime { key, .. } => self.retire_track(*key),
            crate::view::ViewOp::InstallRoute { .. }
            | crate::view::ViewOp::InstallTransport { .. }
            | crate::view::ViewOp::RetireTransport { .. }
            | crate::view::ViewOp::InsertParticipant
            | crate::view::ViewOp::RemoveParticipant { .. }
            | crate::view::ViewOp::BindTrack { .. } => {}
        }
    }

    pub(crate) fn track_descriptor(&self, key: TrackKey) -> Option<(TrackId, &[Option<Rid>])> {
        self.tracks
            .get(key)
            .map(|track| (track.id, track.encodings.as_slice()))
    }

    #[inline]
    pub fn route_video_with_plan(
        &mut self,
        fanout: TrackKey,
        pkt: RtpPacket,
        plan: &crate::plan::FlatTrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) {
        let Some(track) = self.tracks.get_mut(fanout) else {
            debug_assert!(false, "compiled video key must resolve to runtime state");
            return;
        };
        if track.id.kind() != crate::entity::TrackKind::Video {
            debug_assert!(false, "a video key must resolve to a video publication");
            return;
        }
        let rid = pkt.ext_vals.rid;
        let seq = pkt.seq_no;
        let cache = track.cache.get_or_insert_with(TrackStreamCache::new);
        let too_old = cache.push(pkt);
        let Some(packet) = too_old
            .as_ref()
            .or_else(|| cache.encoding(rid).and_then(|stream| stream.get(seq)))
        else {
            debug_assert!(false, "a cached packet must be readable");
            return;
        };
        fanout_local(plan, |subscriber| {
            forward_track(ctx, subscriber, fanout, packet, Some(cache));
        });
        let playout = ctx.wall.to_ntp(packet.playout_time);
        fanout_remote(
            plan,
            &mut track.link_seq,
            playout.middle32(),
            || RoutedTrackPacket {
                key: fanout,
                packet: TrackPacket::Rtp(packet.to_transit()),
            },
            ctx.router,
        );
    }

    fn route_audio_with_plan(
        &mut self,
        track: TrackKey,
        origin: Origin,
        pkt: RtpPacket,
        plan: &crate::plan::FlatTrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) {
        let Some(runtime) = self.tracks.get(track) else {
            debug_assert!(false, "compiled audio key must resolve to runtime state");
            return;
        };
        if runtime.id.kind() != crate::entity::TrackKind::Audio {
            debug_assert!(false, "an audio key must resolve to an audio publication");
            return;
        }
        fanout_local(plan, |subscriber| {
            forward_track(ctx, subscriber, track, &pkt, None);
        });
        if origin.is_local() {
            let playout = ctx.wall.to_ntp(pkt.playout_time);
            let Some(runtime) = self.tracks.get_mut(track) else {
                debug_assert!(false, "audio key must resolve to runtime state");
                return;
            };
            fanout_remote(
                plan,
                &mut runtime.link_seq,
                playout.middle32(),
                || RoutedTrackPacket {
                    key: track,
                    packet: TrackPacket::Rtp(pkt.to_transit()),
                },
                ctx.router,
            );
        }
    }

    pub fn route_rtp_with_plan(
        &mut self,
        key: TrackKey,
        origin: Origin,
        pkt: RtpPacket,
        plan: &crate::plan::FlatTrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) {
        let Some(kind) = self.tracks.get(key).map(|runtime| runtime.id.kind()) else {
            debug_assert!(false, "an RTP packet must resolve to a live track");
            return;
        };
        match kind {
            crate::entity::TrackKind::Video => {
                self.route_video_with_plan(key, pkt, plan, ctx);
            }
            crate::entity::TrackKind::Audio => {
                self.route_audio_with_plan(key, origin, pkt, plan, ctx);
            }
            crate::entity::TrackKind::Data => {
                debug_assert!(false, "a data track cannot carry RTP");
            }
        }
    }

    pub fn route_unreliable_with_plan(
        &mut self,
        stream: TrackKey,
        origin: Origin,
        packet: Vec<u8>,
        plan: &crate::plan::FlatTrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) {
        let Some(runtime) = self.unreliable.get_mut(stream) else {
            debug_assert!(false, "data key must resolve to runtime state");
            return;
        };
        fanout_local(plan, |subscriber| {
            forward_unreliable(ctx.registry, ctx.dirty, subscriber, stream, &packet);
        });
        if origin.is_local() {
            let playout = ctx.wall.ntp();
            fanout_remote(
                plan,
                &mut runtime.link_seq,
                playout.middle32(),
                || RoutedTrackPacket {
                    key: stream,
                    packet: TrackPacket::Data(packet.clone()),
                },
                ctx.router,
            );
        }
    }

    pub fn route_reliable_with_plan(
        &mut self,
        stream: TrackKey,
        origin: Origin,
        frame: Vec<u8>,
        plan: &crate::plan::FlatTrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) {
        debug_assert!(!frame.is_empty());
        let Some(_runtime) = self.reliable.get(stream) else {
            debug_assert!(false, "reliable key must resolve to runtime state");
            return;
        };
        fanout_local(plan, |subscriber| {
            forward_reliable(ctx.registry, ctx.dirty, subscriber, stream, &frame);
        });
        if origin.is_local() {
            let playout = ctx.wall.ntp();
            let Some(runtime) = self.reliable.get_mut(stream) else {
                debug_assert!(false, "reliable key must remain live during dispatch");
                return;
            };
            fanout_remote(
                plan,
                &mut runtime.link_seq,
                playout.middle32(),
                || RoutedTrackPacket {
                    key: stream,
                    packet: TrackPacket::Data(frame.clone()),
                },
                ctx.router,
            );
        }
    }

    pub fn route_reliable_control(
        &self,
        bytes: Vec<u8>,
        plan: &crate::plan::FlatTrackPlan,
        router: &impl ShardTransport,
    ) {
        let Some(target) = plan.reverse_route else {
            debug_assert!(false, "reliable control has no compiled reverse route");
            return;
        };
        router.send_frame(
            target.shard(),
            ShardFrame::Reverse {
                env: Envelope::feedback(target),
                body: Reverse::DataAck(bytes),
            },
        );
    }

    pub fn resolve_reverse(
        &self,
        action: RouteAction,
    ) -> Option<(ParticipantKey, crate::route::ReverseTarget)> {
        let RouteAction::Reverse { target } = action else {
            debug_assert!(false, "reverse frame resolved a non-reverse route");
            return None;
        };
        let origin = match target {
            crate::route::ReverseTarget::Track(key) => self
                .tracks
                .get(key)
                .filter(|runtime| runtime.id.kind() == crate::entity::TrackKind::Video)
                .map(|runtime| runtime.origin_key),
            crate::route::ReverseTarget::Reliable(stream) => self
                .reliable
                .get(stream)
                .and_then(|runtime| runtime.publisher),
        }?;
        Some((origin, target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{ParticipantId, TrackKind};
    use crate::id::ShardId;
    use crate::track::{Track, TrackMeta};
    use slotmap::SlotMap;

    fn descriptor(
        id: TrackId,
        origin_key: ParticipantKey,
        rid: &'static str,
    ) -> crate::view::TrackDescriptor {
        crate::view::TrackDescriptor {
            id,
            origin_key,
            participant: None,
            encodings: vec![Some(Rid::from(rid))],
            publication: Track::audio(
                TrackMeta {
                    room_id: crate::entity::RoomId::from_external(
                        &crate::entity::ExternalRoomId::new("test-room").unwrap(),
                    ),
                    shard_id: ShardId::new(0),
                    id,
                    origin: ParticipantId::from_bytes([7; 16]),
                },
                None,
            ),
        }
    }

    #[test]
    fn reinserting_a_live_track_runtime_replaces_its_encodings() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut track_keys = SlotMap::<TrackKey, ()>::with_key();
        let key = track_keys.insert(());
        let mut participant_keys = SlotMap::<ParticipantKey, ()>::with_key();
        let origin_key = participant_keys.insert(());
        let track_id =
            ParticipantId::from_bytes([7; 16]).derive_track_id(TrackKind::Video, "track");

        runtime.apply_view_op(&crate::view::ViewOp::InsertTrackRuntime {
            key,
            runtime: crate::view::TrackRuntime::Media(descriptor(track_id, origin_key, "q")),
        });
        runtime.apply_view_op(&crate::view::ViewOp::InsertTrackRuntime {
            key,
            runtime: crate::view::TrackRuntime::Media(descriptor(track_id, origin_key, "f")),
        });

        assert_eq!(
            runtime.track_descriptor(key).unwrap().1,
            &[Some(Rid::from("f"))]
        );
    }

    #[test]
    fn reinserting_a_live_data_runtime_preserves_hop_state() {
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut keys = SlotMap::<TrackKey, ()>::with_key();
        let key = keys.insert(());
        let op = crate::view::ViewOp::InsertTrackRuntime {
            key,
            runtime: crate::view::TrackRuntime::Data {
                lane: crate::track::DataLane::Reliable,
                publisher: None,
                publisher_effect: None,
            },
        };

        runtime.apply_view_op(&op);
        runtime.reliable.get_mut(key).unwrap().link_seq = 17;
        runtime.apply_view_op(&op);

        assert_eq!(runtime.reliable.get(key).unwrap().link_seq, 17);
    }
}
