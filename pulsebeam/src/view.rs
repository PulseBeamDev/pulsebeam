//! The per-shard read snapshot the data plane resolves packets against.
//!
//! One [`ShardView`] per shard, written only by the control plane and read
//! only by the shard that owns it. A shard never reads another shard's view,
//! the write handles are never shared, and a read guard never crosses an
//! await or a mailbox operation — see `docs/thread-per-core.md` for why that
//! list is what makes a read-mostly snapshot an exception to the no-shared-
//! state rule rather than a hole in it.
//!
//! The generation is local coordination metadata: it says which lifecycle
//! transaction a shard has observed, and the control plane waits for every
//! affected shard to acknowledge one before advertising anything built in it.
//! It is never a substitute for the epoch. Wire safety is still `(route,
//! epoch)`, because a generation says nothing to a peer that has been holding
//! a datagram for a second and a half.
#![deny(clippy::arithmetic_side_effects)]

use crate::id::ShardId;
use crate::route::{RouteAction, RouteId, TransportHandle, TransportRoute};
use crate::shard::participants::ParticipantKey;

/// One shard's complete routing image at one generation.
///
/// Immutable by contract: the data plane holds a read guard over it and
/// resolves, never writes. Nothing here may be an `Rtc`, a socket, a lock, a
/// reference-counted pointer, or a reference into another shard — a key in
/// this view names an arena entry on *this* shard and is meaningless anywhere
/// else.
#[derive(Debug, Clone, Default)]
pub(crate) struct ShardView {
    pub generation: u64,
    pub routes: RouteImage,
    pub transports: TransportImage,
}

/// Endpoint routes — imported tracks, audio, data, reliable and reverse
/// paths — addressed by [`RouteId`].
#[derive(Debug, Clone, Default)]
pub(crate) struct RouteImage {
    /// Dense by slot. `None` is a free slot; the vector never shrinks, so a
    /// slot's index is stable for as long as the shard lives.
    slots: Vec<Option<RouteBinding>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteBinding {
    pub epoch: u16,
    pub action: RouteAction,
}

impl RouteImage {
    /// The compiled action behind a route, if this generation has that
    /// incarnation live. The epoch check is the whole point: a slot is dense
    /// and reused, so the slot number alone names a slot, not a tenant.
    pub fn resolve(&self, route: RouteId, epoch: u16) -> Option<&RouteAction> {
        match self.slots.get(route.index()) {
            Some(Some(binding)) if binding.epoch == epoch => Some(&binding.action),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    fn install(&mut self, route: RouteId, binding: RouteBinding) {
        let idx = route.index();
        if idx >= self.slots.len() {
            self.slots.resize(idx.saturating_add(1), None);
        }
        let Some(slot) = self.slots.get_mut(idx) else {
            debug_assert!(false, "the resize above guarantees this slot exists");
            return;
        };
        *slot = Some(binding);
    }

    fn retire(&mut self, route: RouteId, epoch: u16) {
        let Some(slot) = self.slots.get_mut(route.index()) else {
            return;
        };
        if slot.is_some_and(|binding| binding.epoch == epoch) {
            *slot = None;
        }
    }
}

/// Client ICE associations, addressed by [`TransportRoute`]. A separate
/// namespace from [`RouteImage`] for the same reason the two route families
/// are separate types.
#[derive(Debug, Clone, Default)]
pub(crate) struct TransportImage {
    slots: Vec<Option<TransportBinding>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TransportBinding {
    pub epoch: u16,
    pub participant: ParticipantKey,
}

impl TransportImage {
    pub fn resolve(&self, handle: TransportHandle) -> Option<ParticipantKey> {
        match self.slots.get(handle.route.index()) {
            Some(Some(binding)) if binding.epoch == handle.epoch => Some(binding.participant),
            _ => None,
        }
    }

    fn install(&mut self, route: TransportRoute, binding: TransportBinding) {
        let idx = route.index();
        if idx >= self.slots.len() {
            self.slots.resize(idx.saturating_add(1), None);
        }
        let Some(slot) = self.slots.get_mut(idx) else {
            debug_assert!(false, "the resize above guarantees this slot exists");
            return;
        };
        *slot = Some(binding);
    }

    fn retire(&mut self, handle: TransportHandle) {
        let Some(slot) = self.slots.get_mut(handle.route.index()) else {
            return;
        };
        if slot.is_some_and(|binding| binding.epoch == handle.epoch) {
            *slot = None;
        }
    }
}

/// One lifecycle transaction's effect on one shard.
///
/// A whole generation is one of these, applied and published exactly once.
/// It is a list rather than a whole-image replacement because a transaction
/// that changes one route on a shard holding ten thousand should not copy ten
/// thousand — but it is still a single deterministic batch, which is what
/// `left-right` needs to replay it identically onto both copies.
#[derive(Debug, Clone)]
pub(crate) struct ShardViewDelta {
    pub generation: u64,
    pub ops: Vec<ViewOp>,
}

#[derive(Debug, Clone)]
pub(crate) enum ViewOp {
    InstallRoute {
        route: RouteId,
        binding: RouteBinding,
    },
    RetireRoute {
        route: RouteId,
        epoch: u16,
    },
    InstallTransport {
        route: TransportRoute,
        binding: TransportBinding,
    },
    RetireTransport {
        handle: TransportHandle,
    },
    /// Replace the whole image. For a shard that restarted or missed a
    /// notification: the control plane sends the complete current generation
    /// rather than replaying a delta sequence against a history it cannot
    /// know the shard still has.
    Reconcile(Box<ShardView>),
}

impl ShardViewDelta {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            ops: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    fn apply_to(&self, view: &mut ShardView) {
        for op in &self.ops {
            match op {
                ViewOp::InstallRoute { route, binding } => view.routes.install(*route, *binding),
                ViewOp::RetireRoute { route, epoch } => view.routes.retire(*route, *epoch),
                ViewOp::InstallTransport { route, binding } => {
                    view.transports.install(*route, *binding);
                }
                ViewOp::RetireTransport { handle } => view.transports.retire(*handle),
                ViewOp::Reconcile(full) => {
                    view.routes = full.routes.clone();
                    view.transports = full.transports.clone();
                }
            }
        }
        debug_assert!(
            self.generation > view.generation || view.generation == 0,
            "a generation must not go backwards"
        );
        view.generation = self.generation;
    }
}

impl left_right::Absorb<ShardViewDelta> for ShardView {
    fn absorb_first(&mut self, op: &mut ShardViewDelta, _other: &Self) {
        op.apply_to(self);
    }

    fn absorb_second(&mut self, op: ShardViewDelta, _other: &Self) {
        op.apply_to(self);
    }

    fn drop_first(self: Box<Self>) {}

    fn sync_with(&mut self, first: &Self) {
        self.clone_from(first);
    }
}

/// The control plane's end of one shard's view.
///
/// Held only by the control-plane actor. It is never wrapped in a lock, never
/// shared with a shard, and no shard ever calls `publish` — the publication
/// budget is one call per affected shard per committed generation, and that
/// is only enforceable if there is exactly one caller.
pub(crate) struct ShardViewWriter {
    write: left_right::WriteHandle<ShardView, ShardViewDelta>,
    /// The generation staged but not yet published.
    pending: Option<ShardViewDelta>,
    /// The last generation handed to `publish`.
    published: u64,
    /// The last generation the shard reported having observed.
    acknowledged: u64,
    publications: u64,
}

impl ShardViewWriter {
    /// Stage an operation into the generation currently being built. Nothing
    /// is visible to the shard until [`Self::publish`].
    pub fn stage(&mut self, generation: u64, op: ViewOp) {
        let delta = self
            .pending
            .get_or_insert_with(|| ShardViewDelta::new(generation));
        debug_assert_eq!(
            delta.generation, generation,
            "a writer stages one generation at a time"
        );
        delta.ops.push(op);
    }


    /// Discard the staged generation. The previously committed view stays
    /// valid, which is what makes a failed prepare recoverable without the
    /// shard ever having seen a partially built image.
    pub fn abort(&mut self) {
        self.pending = None;
    }

    /// Apply the staged batch and publish it, exactly once. A generation that
    /// projects to no change publishes nothing at all.
    pub fn publish(&mut self) -> Option<u64> {
        let delta = self.pending.take()?;
        if delta.is_empty() {
            return None;
        }
        let generation = delta.generation;
        self.write.append(delta);
        self.write.publish();
        self.published = generation;
        self.publications = self.publications.saturating_add(1);
        Some(generation)
    }

    pub fn observe_ack(&mut self, generation: u64) {
        self.acknowledged = self.acknowledged.max(generation);
    }

    pub fn is_acknowledged(&self, generation: u64) -> bool {
        self.acknowledged >= generation
    }


    #[cfg(test)]
    pub fn publications(&self) -> u64 {
        self.publications
    }
}

/// Create one shard's view, returning the control-plane writer and the read
/// handle that shard will hold.
pub(crate) fn new_shard_view(shard_id: ShardId) -> (ShardViewWriter, ShardViewReader) {
    let (write, read) = left_right::new::<ShardView, ShardViewDelta>();
    (
        ShardViewWriter {
            write,
            pending: None,
            published: 0,
            acknowledged: 0,
            publications: 0,
        },
        ShardViewReader { shard_id, read },
    )
}

/// The shard's end of its own view. Read-only by construction — there is no
/// method here that mutates, and the writer is not reachable from it.
pub(crate) struct ShardViewReader {
    shard_id: ShardId,
    read: left_right::ReadHandle<ShardView>,
}

impl ShardViewReader {
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Enter the current generation. Hold this across every dependent route
    /// and lifecycle read for one receive batch or processing tick, and
    /// release it before anything that could await or block.
    pub fn enter(&self) -> Option<left_right::ReadGuard<'_, ShardView>> {
        self.read.enter()
    }

    /// The generation the shard would resolve against right now, for the
    /// acknowledgement it sends back to the control plane.
    pub fn generation(&self) -> u64 {
        self.read.enter().map_or(0, |guard| guard.generation)
    }
}

impl std::fmt::Debug for ShardViewReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardViewReader")
            .field("shard_id", &self.shard_id)
            .field("generation", &self.generation())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::shard::router::{LocalTrackKey, RoomKey};
    use slotmap::KeyData;

    fn shard() -> ShardId {
        ShardId::new(0)
    }

    fn action() -> RouteAction {
        RouteAction::Audio {
            room: RoomKey::default(),
            track: LocalTrackKey::default(),
        }
    }

    fn participant(n: u32) -> ParticipantKey {
        ParticipantKey::from(KeyData::from_ffi(u64::from(n) | (1 << 32)))
    }

    fn route(slot: u32) -> RouteId {
        RouteId::new(shard(), slot)
    }

    fn transport(slot: u32) -> TransportRoute {
        TransportRoute::new(shard(), slot)
    }

    #[test]
    fn a_reader_sees_nothing_until_the_writer_publishes() {
        let (mut writer, reader) = new_shard_view(shard());
        writer.stage(
            1,
            ViewOp::InstallRoute {
                route: route(0),
                binding: RouteBinding {
                    epoch: 0,
                    action: action(),
                },
            },
        );

        assert_eq!(reader.generation(), 0, "staging is not publishing");
        let guard = reader.enter().expect("a view exists from construction");
        assert!(guard.routes.resolve(route(0), 0).is_none());
        drop(guard);

        assert_eq!(writer.publish(), Some(1));
        assert_eq!(reader.generation(), 1);
        let guard = reader.enter().expect("published");
        assert!(guard.routes.resolve(route(0), 0).is_some());
    }

    /// The publication budget: ten routes and a hundred participants changing
    /// in one transaction is one publication, not a hundred and ten.
    #[test]
    fn one_generation_publishes_once_however_many_ops_it_carries() {
        let (mut writer, reader) = new_shard_view(shard());
        for slot in 0..110u32 {
            writer.stage(
                1,
                ViewOp::InstallRoute {
                    route: route(slot),
                    binding: RouteBinding {
                        epoch: 0,
                        action: action(),
                    },
                },
            );
        }
        assert_eq!(writer.publish(), Some(1));
        assert_eq!(writer.publications(), 1);

        let guard = reader.enter().expect("published");
        assert_eq!(guard.routes.len(), 110, "every op landed");
        assert_eq!(guard.generation, 1, "in one generation");
    }

    #[test]
    fn a_generation_that_changes_nothing_publishes_nothing() {
        let (mut writer, _reader) = new_shard_view(shard());
        assert_eq!(writer.publish(), None);
        assert_eq!(writer.publications(), 0);
    }

    /// A failed prepare must leave the previously committed view intact and
    /// the shard unaware the transaction was ever attempted.
    #[test]
    fn aborting_a_staged_generation_leaves_the_committed_view_valid() {
        let (mut writer, reader) = new_shard_view(shard());
        writer.stage(
            1,
            ViewOp::InstallTransport {
                route: transport(0),
                binding: TransportBinding {
                    epoch: 0,
                    participant: participant(1),
                },
            },
        );
        assert_eq!(writer.publish(), Some(1));

        writer.stage(
            2,
            ViewOp::RetireTransport {
                handle: TransportHandle::new(transport(0), 0),
            },
        );
        writer.abort();
        assert_eq!(writer.publish(), None, "an aborted generation publishes nothing");

        let guard = reader.enter().expect("published");
        assert_eq!(guard.generation, 1, "the committed generation stands");
        assert_eq!(
            guard
                .transports
                .resolve(TransportHandle::new(transport(0), 0)),
            Some(participant(1)),
        );
    }

    #[test]
    fn a_retired_route_is_absent_from_the_next_generation() {
        let (mut writer, reader) = new_shard_view(shard());
        writer.stage(
            1,
            ViewOp::InstallRoute {
                route: route(3),
                binding: RouteBinding {
                    epoch: 4,
                    action: action(),
                },
            },
        );
        writer.publish();

        writer.stage(
            2,
            ViewOp::RetireRoute {
                route: route(3),
                epoch: 4,
            },
        );
        writer.publish();

        let guard = reader.enter().expect("published");
        assert!(guard.routes.resolve(route(3), 4).is_none());
    }

    /// A teardown naming a superseded incarnation must not retire the one
    /// that replaced it.
    #[test]
    fn a_stale_retire_does_not_remove_the_incarnation_that_replaced_it() {
        let (mut writer, reader) = new_shard_view(shard());
        writer.stage(
            1,
            ViewOp::InstallRoute {
                route: route(0),
                binding: RouteBinding {
                    epoch: 9,
                    action: action(),
                },
            },
        );
        writer.publish();

        writer.stage(
            2,
            ViewOp::RetireRoute {
                route: route(0),
                epoch: 8,
            },
        );
        writer.publish();

        let guard = reader.enter().expect("published");
        assert!(
            guard.routes.resolve(route(0), 9).is_some(),
            "epoch 8's teardown must not touch epoch 9"
        );
    }

    /// A shard that missed a notification is repaired by being handed the
    /// whole current generation, not by replaying deltas against a history
    /// the control plane cannot know it still has.
    #[test]
    fn reconciliation_replaces_the_whole_image() {
        let (mut writer, reader) = new_shard_view(shard());
        writer.stage(
            1,
            ViewOp::InstallRoute {
                route: route(0),
                binding: RouteBinding {
                    epoch: 0,
                    action: action(),
                },
            },
        );
        writer.publish();

        let mut repaired = ShardView::default();
        repaired.routes.install(
            route(7),
            RouteBinding {
                epoch: 2,
                action: action(),
            },
        );
        writer.stage(5, ViewOp::Reconcile(Box::new(repaired)));
        writer.publish();

        let guard = reader.enter().expect("published");
        assert_eq!(guard.generation, 5);
        assert!(guard.routes.resolve(route(0), 0).is_none(), "the old image is gone");
        assert!(guard.routes.resolve(route(7), 2).is_some(), "the sent image is live");
    }

    #[test]
    fn the_barrier_only_clears_once_the_shard_acknowledges() {
        let (mut writer, _reader) = new_shard_view(shard());
        writer.stage(
            1,
            ViewOp::InstallRoute {
                route: route(0),
                binding: RouteBinding {
                    epoch: 0,
                    action: action(),
                },
            },
        );
        let generation = writer.publish().expect("published");

        assert!(!writer.is_acknowledged(generation), "nothing acked yet");
        writer.observe_ack(generation);
        assert!(writer.is_acknowledged(generation));
    }

    /// The two namespaces are indexed by the same slot numbers and must not
    /// reach into each other.
    #[test]
    fn the_two_images_do_not_alias() {
        let (mut writer, reader) = new_shard_view(shard());
        writer.stage(
            1,
            ViewOp::InstallRoute {
                route: route(0),
                binding: RouteBinding {
                    epoch: 0,
                    action: action(),
                },
            },
        );
        writer.publish();

        let guard = reader.enter().expect("published");
        assert!(guard.routes.resolve(route(0), 0).is_some());
        assert_eq!(
            guard
                .transports
                .resolve(TransportHandle::new(transport(0), 0)),
            None,
            "installing an endpoint route must not populate a transport slot"
        );
    }

    /// An abandoned generation must not leave its ops staged. They would
    /// otherwise be published by whatever generation came next, which is a
    /// transaction that never happened becoming externally visible.
    #[test]
    fn an_aborted_generation_does_not_ride_along_with_the_next_one() {
        let (mut writer, reader) = new_shard_view(shard());

        writer.stage(
            1,
            ViewOp::InstallRoute {
                route: route(1),
                binding: RouteBinding {
                    epoch: 0,
                    action: action(),
                },
            },
        );
        writer.abort();

        writer.stage(
            2,
            ViewOp::InstallRoute {
                route: route(2),
                binding: RouteBinding {
                    epoch: 0,
                    action: action(),
                },
            },
        );
        assert_eq!(writer.publish(), Some(2));

        let guard = reader.enter().expect("published");
        assert!(
            guard.routes.resolve(route(1), 0).is_none(),
            "the abandoned generation's route must not appear"
        );
        assert!(guard.routes.resolve(route(2), 0).is_some());
    }
}
