use crate::clock::WallAnchor;
use crate::id::ShardId;
use crate::keys::ParticipantHandle;
use crate::participant::reverse::ReversePacket;
use crate::participant::{ParticipantInput, RoutedTrackPacket, TrackPacket, TrackPacketRef};
use crate::route::{Envelope, RouteAction, RouteRuntime};
use crate::rtp::{RtpPacket, cache::TrackStreamCache};
use slotmap::SecondaryMap;

use super::worker::{MediaPayload, ShardFrame};

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
    origin: Option<ParticipantHandle>,
    cache: Option<TrackStreamCache>,
    publisher: Option<ParticipantHandle>,
    link_seq: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct InstalledTrackPlan {
    pub local: Vec<ParticipantHandle>,
    pub remote: Vec<crate::route::NodeRouteAddress>,
    pub reverse_route: Option<crate::route::NodeRouteAddress>,
}

/// Hand a packet to every destination-local recipient in an owned track plan.
fn fanout_local(plan: &InstalledTrackPlan, mut deliver: impl FnMut(ParticipantHandle)) {
    for &subscriber in &plan.local {
        deliver(subscriber);
    }
}

/// Forward a packet to the shards a plan routes to, numbering each hop so the
/// destination can tell loss from reordering.
fn fanout_remote(
    plan: &InstalledTrackPlan,
    link_seq: &mut u32,
    playout: u32,
    mut payload: impl FnMut() -> MediaPayload,
    ctx: &impl ShardTransport,
) {
    for remote in &plan.remote {
        let env = Envelope::media(*remote, *link_seq, playout);
        *link_seq = link_seq.wrapping_add(1);
        ctx.send_media(remote.shard(), env, payload());
    }
}

fn forward_track(
    ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    subscriber: ParticipantHandle,
    fanout: TrackKey,
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
    tracks: SecondaryMap<TrackKey, TrackRuntime>,
    pub(crate) routes: RouteRuntime,
}

impl ShardRuntime {
    pub fn new(shard_id: ShardId) -> Self {
        Self {
            tracks: SecondaryMap::new(),
            routes: RouteRuntime::new(shard_id),
        }
    }

    pub(crate) fn retire_track(&mut self, key: TrackKey) {
        let _ = self.tracks.remove(key);
    }

    pub(crate) fn install_track(
        &mut self,
        key: TrackKey,
        descriptor: Option<&crate::shard_update::TrackDescriptor>,
        origin: Option<ParticipantHandle>,
        publisher: Option<ParticipantHandle>,
    ) {
        let cache = descriptor
            .filter(|descriptor| descriptor.kind == crate::entity::TrackKind::Video)
            .map(|descriptor| {
                debug_assert!(descriptor.encodings.len() <= 3);
                TrackStreamCache::new()
            });
        if let Some(previous) = self.tracks.get(key) {
            debug_assert_eq!(
                previous.origin, origin,
                "a runtime key cannot change its publisher binding"
            );
        }
        let previous = self.tracks.insert(
            key,
            TrackRuntime {
                origin,
                cache,
                publisher,
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

    #[inline]
    pub fn route_rtp_with_plan(
        &mut self,
        key: TrackKey,
        origin: Origin,
        pkt: RtpPacket,
        plan: &InstalledTrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) {
        let Some(runtime) = self.tracks.get_mut(key) else {
            debug_assert!(false, "an RTP packet must resolve to a live track");
            return;
        };
        let rid = pkt.ext_vals.rid;
        let seq = pkt.seq_no;
        let too_old;
        let (packet, cache) = if let Some(track_cache) = runtime.cache.as_mut() {
            too_old = track_cache.push(pkt);
            let Some(packet) = too_old
                .as_ref()
                .or_else(|| track_cache.encoding(rid).and_then(|stream| stream.get(seq)))
            else {
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
            fanout_remote(
                plan,
                &mut runtime.link_seq,
                playout.middle32(),
                || RoutedTrackPacket {
                    key,
                    packet: TrackPacket::Rtp(packet.to_transit()),
                },
                ctx.router,
            );
        }
    }

    pub fn route_packet_with_plan(
        &mut self,
        key: TrackKey,
        origin: Origin,
        packet: RoutedTrackPacket,
        plan: &InstalledTrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) {
        debug_assert_eq!(packet.key, key);
        match packet.packet {
            TrackPacket::Rtp(packet) => self.route_rtp_with_plan(key, origin, packet, plan, ctx),
            TrackPacket::Data { lane, bytes } => {
                self.route_data_with_plan(key, origin, lane, bytes, plan, ctx);
            }
        }
    }

    pub fn route_data_with_plan(
        &mut self,
        stream: TrackKey,
        origin: Origin,
        lane: crate::track::DataLane,
        packet: Vec<u8>,
        plan: &InstalledTrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
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
            fanout_remote(
                plan,
                &mut runtime.link_seq,
                playout.middle32(),
                || RoutedTrackPacket {
                    key: stream,
                    packet: TrackPacket::Data {
                        lane,
                        bytes: packet.clone(),
                    },
                },
                ctx.router,
            );
        }
    }

    pub fn route_reverse(
        &self,
        packet: ReversePacket,
        plan: &InstalledTrackPlan,
        router: &impl ShardTransport,
    ) {
        let Some(target) = plan.reverse_route else {
            debug_assert!(false, "reverse packet has no compiled reverse route");
            return;
        };
        router.send_frame(
            target.shard(),
            ShardFrame::Reverse {
                env: Envelope::feedback(target),
                packet,
            },
        );
    }

    pub fn resolve_reverse(&self, action: RouteAction) -> Option<(ParticipantHandle, TrackKey)> {
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
    use std::cell::RefCell;

    struct CaptureTransport {
        frames: RefCell<Vec<ShardFrame>>,
    }

    impl ShardTransport for CaptureTransport {
        fn send_media(&self, _: ShardId, _: Envelope, _: MediaPayload) {}

        fn send_frame(&self, _: ShardId, frame: ShardFrame) {
            self.frames.borrow_mut().push(frame);
        }
    }

    #[test]
    fn reinserting_a_live_track_runtime_preserves_its_forwarding_state() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut track_keys = SlotMap::<TrackKey, ()>::with_key();
        let key = track_keys.insert(());
        let mut participant_keys = SlotMap::<ParticipantHandle, ()>::with_key();
        let origin_key = participant_keys.insert(());
        let descriptor = crate::shard_update::TrackDescriptor {
            origin: crate::entity::ParticipantId::new(),
            kind: crate::entity::TrackKind::Video,
            encodings: vec![Some(str0m::media::Rid::from("q"))],
        };
        runtime.install_track(key, Some(&descriptor), Some(origin_key), None);
        runtime.tracks.get_mut(key).unwrap().link_seq = 7;
        let descriptor = crate::shard_update::TrackDescriptor {
            origin: descriptor.origin,
            kind: crate::entity::TrackKind::Video,
            encodings: vec![Some(str0m::media::Rid::from("f"))],
        };
        runtime.install_track(key, Some(&descriptor), Some(origin_key), None);

        assert_eq!(runtime.tracks.get(key).unwrap().link_seq, 7);
    }

    #[test]
    fn an_empty_encoding_descriptor_still_allocates_stream_cache() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut track_keys = SlotMap::<TrackKey, ()>::with_key();
        let key = track_keys.insert(());
        let mut participant_keys = SlotMap::<ParticipantHandle, ()>::with_key();
        let origin_key = participant_keys.insert(());

        let descriptor = crate::shard_update::TrackDescriptor {
            origin: crate::entity::ParticipantId::new(),
            kind: crate::entity::TrackKind::Video,
            encodings: Vec::new(),
        };
        runtime.install_track(key, Some(&descriptor), Some(origin_key), None);

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
        let mut track_keys = SlotMap::<TrackKey, ()>::with_key();
        let key = track_keys.insert(());
        let mut participant_keys = SlotMap::<ParticipantHandle, ()>::with_key();
        let origin_key = participant_keys.insert(());

        let descriptor = crate::shard_update::TrackDescriptor {
            origin: crate::entity::ParticipantId::new(),
            kind: crate::entity::TrackKind::Audio,
            encodings: Vec::new(),
        };
        runtime.install_track(key, Some(&descriptor), Some(origin_key), None);

        assert!(
            runtime
                .tracks
                .get(key)
                .is_some_and(|track| track.cache.is_none())
        );
    }

    #[test]
    fn reinserting_a_live_data_runtime_preserves_hop_state() {
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut keys = SlotMap::<TrackKey, ()>::with_key();
        let key = keys.insert(());
        runtime.install_track(key, None, None, None);
        runtime.tracks.get_mut(key).unwrap().link_seq = 17;
        runtime.tracks.get_mut(key).unwrap().cache = Some(TrackStreamCache::new());
        runtime.install_track(key, None, None, None);

        assert_eq!(runtime.tracks.get(key).unwrap().link_seq, 17);
        assert!(runtime.tracks.get(key).unwrap().cache.is_some());
    }

    #[test]
    fn reverse_packet_uses_the_generic_reverse_route() {
        let transport = CaptureTransport {
            frames: RefCell::new(Vec::new()),
        };
        let target =
            crate::route::NodeRouteAddress::new(crate::route::RouteId::new(ShardId::new(2), 9), 1);
        let plan = InstalledTrackPlan {
            reverse_route: Some(target),
            ..Default::default()
        };
        ShardRuntime::new(ShardId::new(0)).route_reverse(
            ReversePacket::reliable_control(vec![4, 5]),
            &plan,
            &transport,
        );

        assert!(matches!(
            transport.frames.borrow().first(),
            Some(ShardFrame::Reverse { env, .. })
                if *env == crate::route::Envelope::feedback(target)
        ));
    }
}
