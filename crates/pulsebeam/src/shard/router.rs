use crate::clock::WallAnchor;
use crate::id::ShardId;
use crate::keys::ParticipantHandle;
use crate::participant::reverse::ReversePacket;
use crate::participant::{ParticipantInput, RoutedTrackPacket, TrackPacket, TrackPacketRef};
use crate::route::{MediaDelivery, RouteAction, RouteRuntime};
use crate::rtp::{RtpPacket, cache::TrackStreamCache};
use slotmap::SlotMap;
use std::collections::HashMap;

use super::worker::{MediaPayload, ShardFrame};

pub(crate) use crate::keys::TrackHandle;

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

pub(crate) trait IngressTransport {
    fn send_frame(&self, dst: ShardId, frame: ShardFrame);
}

pub(crate) struct ForwardingContext<'a> {
    pub registry: &'a mut super::participants::ParticipantRegistry,
    pub dirty: &'a mut super::dirty::DirtyTracker,
    pub wall: &'a WallAnchor,
}

struct TrackRuntime {
    id: crate::entity::TrackId,
    origin: Option<ParticipantHandle>,
    cache: Option<TrackStreamCache>,
    publisher: Option<ParticipantHandle>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct InstalledTrackPlan {
    pub local: Vec<ParticipantHandle>,
    pub remote: Vec<crate::shard_update::CompiledRoute>,
    pub reverse_route: Option<crate::shard_update::CompiledRoute>,
}

/// Hand a packet to every destination-local recipient in an owned track plan.
fn fanout_local(plan: &InstalledTrackPlan, mut deliver: impl FnMut(ParticipantHandle)) {
    for &subscriber in &plan.local {
        deliver(subscriber);
    }
}

/// Forward media through the destinations installed in the track plan.
fn fanout_remote(
    plan: &InstalledTrackPlan,
    playout: u32,
    mut payload: impl FnMut() -> MediaPayload,
) {
    for remote in &plan.remote {
        let delivery = MediaDelivery {
            address: remote.address,
            playout_ntp32: playout,
        };
        let _ = super::worker::send_link_frame(
            &remote.tx,
            remote.clock_origin,
            ShardFrame::Media {
                delivery,
                payload: payload(),
            },
        );
    }
}

fn forward_track(
    ctx: &mut ForwardingContext<'_>,
    subscriber: ParticipantHandle,
    fanout: TrackHandle,
    pkt: TrackPacketRef<'_>,
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

pub(crate) struct ShardRuntime {
    tracks: SlotMap<TrackHandle, TrackRuntime>,
    track_handles: HashMap<crate::entity::TrackId, TrackHandle>,
    pub(crate) routes: RouteRuntime,
}

impl ShardRuntime {
    pub fn new(shard_id: ShardId) -> Self {
        Self {
            tracks: SlotMap::with_key(),
            track_handles: HashMap::new(),
            routes: RouteRuntime::new(shard_id),
        }
    }

    pub(crate) fn track_handle(&self, track_id: crate::entity::TrackId) -> Option<TrackHandle> {
        self.track_handles.get(&track_id).copied()
    }

    pub(crate) fn track_id(&self, handle: TrackHandle) -> Option<crate::entity::TrackId> {
        self.tracks.get(handle).map(|runtime| runtime.id)
    }

    pub(crate) fn source_origin(&self, handle: TrackHandle) -> Option<ParticipantHandle> {
        self.tracks.get(handle).and_then(|runtime| runtime.origin)
    }

    pub(crate) fn retire_track(&mut self, track_id: crate::entity::TrackId) {
        let Some(handle) = self.track_handles.remove(&track_id) else {
            return;
        };
        let _ = self.tracks.remove(handle);
    }

    pub(crate) fn install_track(
        &mut self,
        track_id: crate::entity::TrackId,
        descriptor: Option<&crate::shard_update::TrackDescriptor>,
        origin: Option<ParticipantHandle>,
        publisher: Option<ParticipantHandle>,
    ) -> TrackHandle {
        let cache = descriptor
            .filter(|descriptor| descriptor.kind == crate::entity::TrackKind::Video)
            .map(|descriptor| {
                debug_assert!(descriptor.encodings.len() <= 3);
                TrackStreamCache::with_encodings(&descriptor.encodings)
            });
        if let Some(handle) = self.track_handles.get(&track_id).copied() {
            let Some(runtime) = self.tracks.get_mut(handle) else {
                debug_assert!(false, "a track identity must resolve to its shard handle");
                return handle;
            };
            debug_assert_eq!(
                runtime.origin, origin,
                "a runtime handle cannot change its publisher binding"
            );
            runtime.origin = origin;
            runtime.publisher = publisher;
            return handle;
        }
        let handle = self.tracks.insert(TrackRuntime {
            id: track_id,
            origin,
            cache,
            publisher,
        });
        self.track_handles.insert(track_id, handle);
        handle
    }

    #[inline]
    pub fn route_rtp_with_plan(
        &mut self,
        key: TrackHandle,
        origin: Origin,
        pkt: RtpPacket,
        plan: &InstalledTrackPlan,
        ctx: &mut ForwardingContext<'_>,
    ) {
        let Some(runtime) = self.tracks.get_mut(key) else {
            debug_assert!(false, "an RTP packet must resolve to a live track");
            return;
        };
        let seq = pkt.seq_no;
        let too_old;
        let (packet, cache) = if let Some(track_cache) = runtime.cache.as_mut() {
            let Some(encoding) = track_cache.source(&pkt) else {
                return;
            };
            too_old = track_cache.push_encoding(encoding, pkt);
            let Some(packet) = too_old.as_ref().or_else(|| {
                track_cache
                    .encoding(encoding)
                    .and_then(|stream| stream.get(seq))
            }) else {
                debug_assert!(false, "a cached packet must be readable");
                return;
            };
            (packet, Some(&*track_cache))
        } else {
            (&pkt, None)
        };
        fanout_local(plan, |subscriber| {
            forward_track(ctx, subscriber, key, TrackPacketRef::Rtp(packet), cache);
        });
        if origin.is_local() {
            let playout = ctx.wall.to_ntp(packet.playout_time);
            let track_id = runtime.id;
            fanout_remote(plan, playout.middle32(), || RoutedTrackPacket {
                track_id,
                packet: TrackPacket::Rtp(packet.to_transit()),
            });
        }
    }

    pub fn route_packet_with_plan(
        &mut self,
        key: TrackHandle,
        origin: Origin,
        packet: TrackPacket,
        plan: &InstalledTrackPlan,
        ctx: &mut ForwardingContext<'_>,
    ) {
        match packet {
            TrackPacket::Rtp(packet) => self.route_rtp_with_plan(key, origin, packet, plan, ctx),
            TrackPacket::Data { lane, bytes } => {
                self.route_data_with_plan(key, origin, lane, bytes, plan, ctx);
            }
        }
    }

    #[allow(
        clippy::disallowed_types,
        reason = "RFC254 clones immutable data storage for local Link fanout; cross-node peers serialize"
    )]
    pub fn route_data_with_plan(
        &mut self,
        stream: TrackHandle,
        origin: Origin,
        lane: crate::track::DataLane,
        packet: bytes::Bytes,
        plan: &InstalledTrackPlan,
        ctx: &mut ForwardingContext<'_>,
    ) {
        let Some(runtime) = self.tracks.get_mut(stream) else {
            debug_assert!(false, "data key must resolve to runtime state");
            return;
        };
        fanout_local(plan, |subscriber| {
            forward_track(
                ctx,
                subscriber,
                stream,
                TrackPacketRef::Data {
                    lane,
                    bytes: &packet,
                },
                None,
            );
        });
        if origin.is_local() {
            let playout = ctx.wall.ntp();
            let track_id = runtime.id;
            fanout_remote(plan, playout.middle32(), || RoutedTrackPacket {
                track_id,
                packet: TrackPacket::Data {
                    lane,
                    bytes: packet.clone(),
                },
            });
        }
    }

    pub fn route_reverse(&self, packet: ReversePacket, plan: &InstalledTrackPlan) {
        let Some(target) = &plan.reverse_route else {
            debug_assert!(false, "reverse packet has no compiled reverse route");
            return;
        };
        let _ = super::worker::send_link_frame(
            &target.tx,
            target.clock_origin,
            ShardFrame::Reverse {
                address: target.address,
                packet,
            },
        );
    }

    pub fn resolve_reverse(&self, action: RouteAction) -> Option<(ParticipantHandle, TrackHandle)> {
        let RouteAction::Reverse { target } = action else {
            debug_assert!(false, "reverse frame resolved a non-reverse route");
            return None;
        };
        let runtime = self.tracks.get(target)?;
        let origin = runtime.publisher.or(runtime.origin)?;
        Some((origin, target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ShardId;
    use slotmap::SlotMap;
    fn track_id(kind: crate::entity::TrackKind, label: &str) -> crate::entity::TrackId {
        crate::entity::ConnectionId::new().derive_track_id(kind, label)
    }

    #[test]
    fn reinserting_a_live_track_runtime_preserves_its_forwarding_state() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut participant_keys = SlotMap::<ParticipantHandle, ()>::with_key();
        let origin_key = participant_keys.insert(());
        let descriptor = crate::shard_update::TrackDescriptor {
            origin: crate::entity::ParticipantId::new(),
            kind: crate::entity::TrackKind::Video,
            encodings: vec![Some(crate::media::Rid::from("q")).into()],
        };
        let track_id = track_id(crate::entity::TrackKind::Video, "video");
        let key = runtime.install_track(track_id, Some(&descriptor), Some(origin_key), None);
        let origin_before = runtime.tracks.get(key).unwrap().origin;
        let descriptor = crate::shard_update::TrackDescriptor {
            origin: descriptor.origin,
            kind: crate::entity::TrackKind::Video,
            encodings: vec![Some(crate::media::Rid::from("f")).into()],
        };
        assert_eq!(
            runtime.install_track(track_id, Some(&descriptor), Some(origin_key), None),
            key
        );

        assert_eq!(runtime.tracks.get(key).unwrap().origin, origin_before);
        assert!(runtime.tracks.get(key).unwrap().cache.is_some());
    }

    #[test]
    fn an_empty_encoding_descriptor_still_allocates_stream_cache() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut participant_keys = SlotMap::<ParticipantHandle, ()>::with_key();
        let origin_key = participant_keys.insert(());

        let descriptor = crate::shard_update::TrackDescriptor {
            origin: crate::entity::ParticipantId::new(),
            kind: crate::entity::TrackKind::Video,
            encodings: Vec::new(),
        };
        let key = runtime.install_track(
            track_id(crate::entity::TrackKind::Video, "empty"),
            Some(&descriptor),
            Some(origin_key),
            None,
        );

        assert!(
            runtime
                .tracks
                .get(key)
                .is_some_and(|track| track.cache.is_some())
        );
    }

    #[test]
    fn an_audio_track_descriptor_does_not_allocate_a_stream_cache() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut participant_keys = SlotMap::<ParticipantHandle, ()>::with_key();
        let origin_key = participant_keys.insert(());

        let descriptor = crate::shard_update::TrackDescriptor {
            origin: crate::entity::ParticipantId::new(),
            kind: crate::entity::TrackKind::Audio,
            encodings: Vec::new(),
        };
        let key = runtime.install_track(
            track_id(crate::entity::TrackKind::Audio, "audio"),
            Some(&descriptor),
            Some(origin_key),
            None,
        );

        assert!(
            runtime
                .tracks
                .get(key)
                .is_some_and(|track| track.cache.is_none())
        );
    }

    #[test]
    fn reinserting_a_live_data_runtime_preserves_cache_state() {
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let track_id = track_id(crate::entity::TrackKind::Data, "data");
        let key = runtime.install_track(track_id, None, None, None);
        runtime.tracks.get_mut(key).unwrap().cache = Some(TrackStreamCache::new());
        assert_eq!(runtime.install_track(track_id, None, None, None), key);

        assert!(runtime.tracks.get(key).unwrap().cache.is_some());
    }

    #[test]
    fn reverse_packet_uses_the_generic_reverse_route() {
        let mut links =
            pulsebeam_link::LinkControl::new(|| pulsebeam_link::LinkTime::from_nanos(0)).unwrap();
        let (port, mut rx) = links.create_port().unwrap();
        let (_, tx) = links
            .bind_send(pulsebeam_link::Destination::Local {
                port,
                tag: pulsebeam_link::DeliveryTag::from_raw(0),
            })
            .unwrap();
        let target =
            crate::route::NodeRouteAddress::new(crate::route::RouteId::new(ShardId::new(2), 9), 1);
        let plan = InstalledTrackPlan {
            reverse_route: Some(crate::shard_update::CompiledRoute {
                address: target,
                tx,
                clock_origin: pulsebeam_runtime::time::Instant::now(),
            }),
            ..Default::default()
        };
        ShardRuntime::new(ShardId::new(0))
            .route_reverse(ReversePacket::reliable_control(vec![4, 5]), &plan);

        let mut frames = Vec::new();
        assert_eq!(rx.try_recv_batch(&mut frames, 1), 1);
        assert!(
            matches!(&frames[0].packet, ShardFrame::Reverse { address, .. } if *address == target)
        );
    }
}
