use crate::clock::WallAnchor;
use crate::id::ShardId;
use crate::keys::ParticipantKey;
use crate::participant::reverse::ReversePacket;
use crate::participant::{
    ForwardPacket, ParticipantInput, RoutedTrackPacket, TrackPacket, TrackPacketRef,
};
use crate::route::{Envelope, RouteAction, RouteRuntime};
use crate::rtp::{PacketForwardingState, cache::TrackStreamCache};
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
    pub now: tokio::time::Instant,
    pub registry: &'a mut super::participants::ParticipantRegistry,
    pub dirty: &'a mut super::dirty::DirtyTracker,
    pub wall: &'a WallAnchor,
    pub router: &'a R,
}

struct TrackRuntime {
    origin_key: ParticipantKey,
    cache: Option<TrackStreamCache>,
    encodings: Vec<Option<crate::rtp::EncodingId>>,
    publisher: Option<ParticipantKey>,
    link_seq: u32,
}

/// Hand a packet to every destination-local recipient in an owned track plan.
fn fanout_local(plan: &crate::shard_update::TrackPlan, mut deliver: impl FnMut(ParticipantKey)) {
    for &subscriber in &plan.local {
        deliver(subscriber);
    }
}

/// Forward a packet to the shards a plan routes to, numbering each hop so the
/// destination can tell loss from reordering.
fn fanout_remote(
    plan: &crate::shard_update::TrackPlan,
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
    subscriber: ParticipantKey,
    fanout: TrackKey,
    pkt: TrackPacketRef<'_>,
    cache: Option<&TrackStreamCache>,
) {
    let Some(participant) = ctx.registry.resolve_mut(subscriber) else {
        debug_assert!(false, "a track plan must name a live participant");
        return;
    };
    participant.input(ParticipantInput::Track {
        now: ctx.now,
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

    pub(crate) fn apply_update_op(&mut self, op: &crate::shard_update::ShardUpdateOp) {
        match op {
            crate::shard_update::ShardUpdateOp::RetireRoute { handle } => {
                let retired = self.routes.retire(*handle);
                debug_assert!(retired || self.routes.entry(*handle).is_none());
            }
            crate::shard_update::ShardUpdateOp::InsertTrackRuntime { key, runtime } => {
                let descriptor = runtime.descriptor.as_ref();
                let origin_key = descriptor
                    .map(|descriptor| descriptor.origin_key)
                    .or(runtime.publisher)
                    .unwrap_or_default();
                let cache = descriptor
                    .is_some_and(|descriptor| !descriptor.encodings.is_empty())
                    .then(TrackStreamCache::new);
                let encodings = descriptor
                    .map(|descriptor| descriptor.encodings.clone())
                    .unwrap_or_default();
                let publisher = runtime.publisher;
                let key = *key;
                if let Some(previous) = self.tracks.get(key) {
                    debug_assert_eq!(
                        previous.origin_key, origin_key,
                        "a runtime key cannot change its publisher binding"
                    );
                }
                let previous = self.tracks.insert(
                    key,
                    TrackRuntime {
                        origin_key,
                        cache,
                        encodings,
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
            crate::shard_update::ShardUpdateOp::RemoveTrackRuntime { key, .. } => {
                self.retire_track(*key);
            }
            crate::shard_update::ShardUpdateOp::InstallRoute { handle, action } => {
                self.routes.install_action(*handle, *action);
            }
            crate::shard_update::ShardUpdateOp::InstallTransport { .. }
            | crate::shard_update::ShardUpdateOp::RetireTransport { .. }
            | crate::shard_update::ShardUpdateOp::InsertParticipant
            | crate::shard_update::ShardUpdateOp::RemoveParticipant { .. }
            | crate::shard_update::ShardUpdateOp::Placeholder => {}
        }
    }

    pub(crate) fn replay_cached_track(
        &mut self,
        key: TrackKey,
        local: &mut Vec<ParticipantKey>,
        remote: &mut Vec<crate::route::RouteHandle>,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) -> bool {
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_work(
            "track_cached_replay_target",
            local.len().saturating_add(remote.len()),
        );
        let Some(runtime) = self.tracks.get_mut(key) else {
            debug_assert!(
                false,
                "a cached replay plan must resolve to a track runtime"
            );
            return false;
        };
        let Some(cache) = runtime.cache.as_ref() else {
            return false;
        };
        if !cache.has_replay(&runtime.encodings) {
            return false;
        }
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_work(
            "track_cached_replay_ready",
            local.len().saturating_add(remote.len()),
        );

        local.retain(|&subscriber| {
            let Some(participant) = ctx.registry.resolve_mut(subscriber) else {
                return false;
            };
            let rendered = participant.replay_cached_track(ctx.now, key, cache);
            if rendered {
                ctx.dirty.mark(subscriber, participant);
            }
            !rendered
        });

        for packet in cache.replay(&runtime.encodings) {
            let Some(media) = cache.forward_for(&packet) else {
                continue;
            };
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_work("track_cached_replay_packet", remote.len());
            let playout = ctx.wall.to_ntp(packet.playout_time);
            for destination in remote.iter().copied() {
                let env = Envelope::media(destination, runtime.link_seq, playout.middle32());
                runtime.link_seq = runtime.link_seq.wrapping_add(1);
                ctx.router.send_media(
                    destination.shard(),
                    env,
                    RoutedTrackPacket {
                        key,
                        packet: TrackPacket::Rtp {
                            packet: packet.clone(),
                            encoding: cache.encoding_of(&packet),
                            audio_level_extension: None,
                            media: ForwardPacket::Transit(media.materialize()),
                        },
                    },
                );
            }
        }
        remote.clear();
        true
    }

    #[inline]
    pub fn route_rtp_with_plan(
        &mut self,
        key: TrackKey,
        origin: Origin,
        pkt: PacketForwardingState,
        encoding: Option<crate::rtp::EncodingId>,
        audio_level_extension: Option<u8>,
        media: ForwardPacket,
        plan: &crate::shard_update::TrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) {
        let Some(runtime) = self.tracks.get_mut(key) else {
            debug_assert!(false, "an RTP packet must resolve to a live track");
            return;
        };
        let seq = pkt.seq_no;
        let too_old;
        let (packet, media, cache) = if let Some(track_cache) = runtime.cache.as_mut() {
            too_old = track_cache.push_forward(pkt, media, encoding);
            let Some(packet) = too_old.as_ref().map(|(packet, _)| packet).or_else(|| {
                track_cache
                    .encoding(encoding)
                    .and_then(|stream| stream.get(seq))
            }) else {
                debug_assert!(false, "a cached packet must be readable");
                return;
            };
            let media = too_old
                .as_ref()
                .map(|(_, media)| media)
                .or_else(|| track_cache.forward_for(packet));
            let Some(media) = media else {
                debug_assert!(false, "a cached RTP packet retains its facade media");
                return;
            };
            (packet, media, Some(&*track_cache))
        } else {
            (&pkt, &media, None)
        };
        fanout_local(plan, |subscriber| {
            forward_track(
                ctx,
                subscriber,
                key,
                TrackPacketRef::Rtp {
                    packet,
                    encoding,
                    audio_level_extension,
                    media,
                },
                cache,
            );
        });
        if origin.is_local() {
            let playout = ctx.wall.to_ntp(packet.playout_time);
            fanout_remote(
                plan,
                &mut runtime.link_seq,
                playout.middle32(),
                || RoutedTrackPacket {
                    key,
                    packet: TrackPacket::Rtp {
                        packet: packet.clone(),
                        encoding,
                        audio_level_extension,
                        media: ForwardPacket::Transit(media.materialize()),
                    },
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
        plan: &crate::shard_update::TrackPlan,
        ctx: &mut ForwardingContext<'_, impl ShardTransport>,
    ) {
        debug_assert_eq!(packet.key, key);
        match packet.packet {
            TrackPacket::Rtp {
                packet,
                encoding,
                audio_level_extension,
                media,
            } => {
                self.route_rtp_with_plan(
                    key,
                    origin,
                    packet,
                    encoding,
                    audio_level_extension,
                    media,
                    plan,
                    ctx,
                );
            }
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
        plan: &crate::shard_update::TrackPlan,
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
        plan: &crate::shard_update::TrackPlan,
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

    pub fn resolve_reverse(&self, action: RouteAction) -> Option<(ParticipantKey, TrackKey)> {
        let RouteAction::Reverse { target } = action else {
            debug_assert!(false, "reverse frame resolved a non-reverse route");
            return None;
        };
        let runtime = self.tracks.get(target)?;
        let origin = runtime.publisher.unwrap_or(runtime.origin_key);
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
        let mut participant_keys = SlotMap::<ParticipantKey, ()>::with_key();
        let origin_key = participant_keys.insert(());
        runtime.apply_update_op(&crate::shard_update::ShardUpdateOp::InsertTrackRuntime {
            key,
            runtime: crate::shard_update::TrackRuntime {
                descriptor: Some(crate::shard_update::TrackDescriptor {
                    origin_key,
                    encodings: vec![Some(crate::rtp::EncodingId::from("q"))],
                }),
                ..Default::default()
            },
        });
        runtime.tracks.get_mut(key).unwrap().link_seq = 7;
        runtime.apply_update_op(&crate::shard_update::ShardUpdateOp::InsertTrackRuntime {
            key,
            runtime: crate::shard_update::TrackRuntime {
                descriptor: Some(crate::shard_update::TrackDescriptor {
                    origin_key,
                    encodings: vec![Some(crate::rtp::EncodingId::from("f"))],
                }),
                ..Default::default()
            },
        });

        assert_eq!(runtime.tracks.get(key).unwrap().link_seq, 7);
    }

    #[test]
    fn reinserting_a_live_data_runtime_preserves_hop_state() {
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut keys = SlotMap::<TrackKey, ()>::with_key();
        let key = keys.insert(());
        let op = crate::shard_update::ShardUpdateOp::InsertTrackRuntime {
            key,
            runtime: crate::shard_update::TrackRuntime {
                publisher: None,
                publisher_effect: None,
                ..Default::default()
            },
        };

        runtime.apply_update_op(&op);
        runtime.tracks.get_mut(key).unwrap().link_seq = 17;
        runtime.tracks.get_mut(key).unwrap().cache = Some(TrackStreamCache::new());
        runtime.apply_update_op(&op);

        assert_eq!(runtime.tracks.get(key).unwrap().link_seq, 17);
        assert!(runtime.tracks.get(key).unwrap().cache.is_some());
    }

    #[test]
    fn reverse_packet_uses_the_generic_reverse_route() {
        let transport = CaptureTransport {
            frames: RefCell::new(Vec::new()),
        };
        let target =
            crate::route::RouteHandle::new(crate::route::RouteId::new(ShardId::new(2), 9), 1);
        let plan = crate::shard_update::TrackPlan {
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
