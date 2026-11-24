use crate::id::ShardId;
use crate::keys::ParticipantHandle;
use crate::participant::reverse::ReversePacket;
use crate::route::fabric::local::{
    Emission, FabricBranchHandle, MAX_BRANCHES, Offers, OwnedPacket, PacketView, Rejection,
    RepairAdmission, RepairRequest, RepairTarget, SourceFanout,
};
use crate::route::fabric::{
    CompiledLiveRelay, CompiledPublisherSend, LiveAdmissions, LiveNetworkChild, NetworkPacket,
    ShardBinding, ShardOffer, network_repair::Interest, relay::LiveRelays,
};
use crate::route::{NodeRouteAddress, RouteAction, RouteRuntime};
use slotmap::SlotMap;
use std::{collections::HashMap, num::NonZeroU64};

use super::worker::ShardFrame;

pub(crate) use crate::keys::TrackHandle;

pub(crate) trait IngressTransport {
    fn send_frame(&self, dst: ShardId, frame: ShardFrame);
}

slotmap::new_key_type! {
    pub(crate) struct FabricSourceHandle;
}

// Preserve the provisioned ingress and DATA reservation independently of
// local/shard-only RTP, which is limited by the installed publication set.
const MAX_RESERVED_FABRIC_SOURCES: usize = 4;

fn fabric_reservations_full(sources: &SlotMap<FabricSourceHandle, FabricSource>) -> bool {
    sources
        .values()
        .filter(|source| {
            matches!(source.ingress, FabricIngress::Network { .. })
                || source.fanout.emission().profile() == pulsebeam_core::fabric::Profile::Data
        })
        .take(MAX_RESERVED_FABRIC_SOURCES)
        .count()
        == MAX_RESERVED_FABRIC_SOURCES
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FabricLocalBranch {
    source: FabricSourceHandle,
    branch: FabricBranchHandle,
}

impl FabricLocalBranch {
    pub(super) fn source(self) -> FabricSourceHandle {
        self.source
    }

    pub(super) fn repair_target(
        self,
        destination: ParticipantHandle,
        incarnation: NonZeroU64,
    ) -> crate::route::fabric::local::RepairTarget {
        crate::route::fabric::local::RepairTarget {
            branch: self.branch,
            destination,
            incarnation,
        }
    }

    pub(super) fn for_repair(
        source: FabricSourceHandle,
        target: crate::route::fabric::local::RepairTarget,
    ) -> Self {
        Self {
            source,
            branch: target.branch,
        }
    }

    pub(crate) fn scope(self) -> Option<pulsebeam_rtc::MediaScope> {
        use slotmap::Key;
        let source = u128::from(self.source.data().as_ffi());
        let branch = u128::from(self.branch.data().as_ffi());
        std::num::NonZeroU128::new((source << 64) | branch).map(pulsebeam_rtc::MediaScope::new)
    }

    pub(crate) fn from_scope(scope: pulsebeam_rtc::MediaScope) -> Option<Self> {
        let bits = scope.owner().get();
        let source = u64::try_from(bits >> 64).ok()?;
        let branch = u64::try_from(bits & u128::from(u64::MAX)).ok()?;
        Some(Self {
            source: slotmap::KeyData::from_ffi(source).into(),
            branch: slotmap::KeyData::from_ffi(branch).into(),
        })
    }
}

#[cfg(test)]
impl FabricLocalBranch {
    pub(crate) fn for_test() -> Self {
        Self {
            source: slotmap::SlotMap::with_key().insert(()),
            branch: slotmap::SlotMap::with_key().insert(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FabricOwner {
    Local(ParticipantHandle),
    Shard(ShardBinding),
    Network,
}

pub(super) enum FabricRepairSource {
    Publisher(ParticipantHandle, pulsebeam_rtc::DataChannelInstallation),
    Shard(ShardBinding),
}

pub(super) enum FabricDeliveryTarget {
    Publisher(ParticipantHandle, pulsebeam_rtc::DataChannelInstallation),
    ShardShadow(ShardBinding),
}

struct FabricSource {
    track: TrackHandle,
    encoding: Option<crate::route::fabric::RtpSourceEncoding>,
    ingress: FabricIngress,
    fanout: SourceFanout,
    reliable_installation: Option<pulsebeam_rtc::DataChannelInstallation>,
    publisher_clock: Option<Box<crate::rtp::publisher_clock::PublisherClock>>,
    publisher_timing: Option<(
        u64,
        pulsebeam_runtime::time::Instant,
        pulsebeam_clock::domain::Projection,
    )>,
}

impl FabricSource {
    fn freeze_publisher_expectation(
        &mut self,
        unit: crate::route::fabric::local::DataExpectation,
        expires: pulsebeam_runtime::time::Instant,
        now: pulsebeam_runtime::time::Instant,
    ) {
        let FabricIngress::Publisher {
            publisher_sends,
            data_expectations,
            ..
        } = &mut self.ingress
        else {
            return;
        };
        if publisher_sends.is_empty() {
            return;
        }
        let mut wan_peers = std::collections::HashSet::new();
        let routes = publisher_sends
            .iter()
            .map(|hop| &hop.route)
            .filter(|route| route.expires_at > now && route.tx.is_live())
            .filter(|route| wan_peers.insert(route.next.peer));
        if !data_expectations
            .get_or_insert_with(Box::default)
            .seed(now, (), unit, expires, routes)
        {
            metrics::counter!("fabric_relay_data_expectation_outcome", "outcome" => "unknown")
                .increment(1);
        }
    }

    fn repair_parent(
        &self,
        request: RepairRequest,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<LiveNetworkChild> {
        let FabricIngress::Network {
            children,
            expected_parents,
            ..
        } = &self.ingress
        else {
            return None;
        };
        let parent = expected_parents.as_deref()?.parent(request.fragment, now)?;
        children
            .iter()
            .any(|child| child.spec == parent && child.lifetime.is_live())
            .then_some(parent)
    }
}

struct NetworkRepair {
    child: crate::route::fabric::network_repair::RepairChild,
    parent: LiveNetworkChild,
    target: crate::route::fabric::local::RepairTarget,
    lifetime: pulsebeam_link::BindingCutoff,
    ingress: crate::route::fabric::network_repair::RepairIngress,
}

impl NetworkRepair {
    fn parent_at(
        &self,
        children: &[NetworkChild],
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<NonZeroU64> {
        if !self.lifetime.is_live() || now >= self.child.demand.expires {
            return None;
        }
        children
            .iter()
            .find(|parent| parent.spec == self.parent && parent.lifetime.is_live())
            .map(|parent| parent.ingress)
    }
}

impl Drop for NetworkRepair {
    fn drop(&mut self) {
        self.lifetime.fence();
    }
}

struct NetworkMediaRepair {
    child: crate::route::fabric::network_repair::MediaRepairChild,
    lifetime: pulsebeam_link::BindingCutoff,
}

impl Drop for NetworkMediaRepair {
    fn drop(&mut self) {
        self.lifetime.fence();
    }
}

struct NetworkChild {
    ingress: NonZeroU64,
    spec: LiveNetworkChild,
    lifetime: pulsebeam_link::BindingCutoff,
    media_gaps: crate::route::fabric::network_repair::MediaGaps,
}

impl Drop for NetworkChild {
    fn drop(&mut self) {
        self.lifetime.fence();
    }
}

enum FabricIngress {
    Publisher {
        participant: ParticipantHandle,
        incarnation: NonZeroU64,
        data: Option<Box<crate::participant::data_fabric::PublisherSource>>,
        refresh: Option<FabricRefresh>,
        destinations: Vec<ShardFabricDestination>,
        publisher_sends: Vec<PublisherHop>,
        data_expectations: Option<
            Box<crate::route::fabric::hop_expectations::HopExpectations<(), CompiledPublisherSend>>,
        >,
    },
    Shard {
        binding: ShardBinding,
        /// Only an exact binding installation can widen this entitlement.
        members: Vec<ParticipantHandle>,
        last_refresh: Option<pulsebeam_runtime::time::Instant>,
    },
    Network {
        incarnation: NonZeroU64,
        emission: Emission,
        sequence_anchor: u64,
        children: Vec<NetworkChild>,
        repairs: Vec<NetworkRepair>,
        media_repairs: Vec<NetworkMediaRepair>,
        last_ingress: NonZeroU64,
        members: Vec<ParticipantHandle>,
        wan_transit: bool,
        relays: LiveRelays,
        expected_parents: Option<
            Box<crate::route::fabric::hop_expectations::ExpectedUnits<LiveNetworkChild, ()>>,
        >,
    },
}

impl FabricIngress {
    fn authorizes(&self, ingress: Option<NonZeroU64>) -> bool {
        match self {
            Self::Network { children, .. } => ingress.is_some_and(|ingress| {
                children
                    .iter()
                    .any(|child| child.ingress == ingress && child.lifetime.is_live())
            }),
            _ => ingress.is_none(),
        }
    }

    fn retain_members(&mut self, authorized: impl Fn(ParticipantHandle) -> bool) {
        match self {
            Self::Shard { members, .. } => members.retain(|member| authorized(*member)),
            Self::Network {
                members,
                children,
                repairs,
                ..
            } => {
                let before = members.len();
                members.retain(|member| authorized(*member));
                if before != members.len() {
                    for child in children {
                        child.lifetime.fence();
                    }
                    repairs.clear();
                }
            }
            Self::Publisher { .. } => {}
        }
    }

    fn owner(&self) -> FabricOwner {
        match self {
            Self::Publisher { participant, .. } => FabricOwner::Local(*participant),
            Self::Shard { binding, .. } => FabricOwner::Shard(*binding),
            Self::Network { .. } => FabricOwner::Network,
        }
    }
}

struct PublisherHop {
    route: CompiledPublisherSend,
    lifetime: pulsebeam_link::BindingCutoff,
    admissions: LiveAdmissions,
}

impl Drop for PublisherHop {
    fn drop(&mut self) {
        self.lifetime.fence();
    }
}

struct ShardFabricDestination {
    binding: ShardBinding,
    route: crate::shard_update::CompiledRoute,
    admissions: LiveAdmissions,
    ordered_receivers: Vec<crate::route::fabric::ShardDataReceiver>,
}

struct FabricRefresh {
    primary_ssrc: std::num::NonZeroU32,
    last_requested: Option<pulsebeam_runtime::time::Instant>,
}

impl FabricRefresh {
    fn request(
        &mut self,
        now: pulsebeam_runtime::time::Instant,
        send: impl FnOnce(u32) -> bool,
    ) -> bool {
        if self.last_requested.is_some_and(|last| {
            now.saturating_duration_since(last) < crate::track::KEYFRAME_DEBOUNCE
        }) || !send(self.primary_ssrc.get())
        {
            return false;
        }
        self.last_requested = Some(now);
        true
    }
}

const MAX_TRACK_FABRIC_SOURCES: usize = crate::track::MAX_SIMULCAST_LAYERS;
type TrackFabricSources = arrayvec::ArrayVec<FabricSourceHandle, MAX_TRACK_FABRIC_SOURCES>;

#[derive(Default)]
struct TrackSources(TrackFabricSources);

impl TrackSources {
    fn sources(&self) -> &[FabricSourceHandle] {
        &self.0
    }

    fn fabric(&self) -> Option<FabricSourceHandle> {
        let [source] = self.sources() else {
            return None;
        };
        Some(*source)
    }

    fn insert_fabric(&mut self, source: FabricSourceHandle) {
        self.0.push(source);
    }

    fn remove_fabric(&mut self, source: FabricSourceHandle) {
        self.0.retain(|candidate| *candidate != source);
    }
}

struct TrackRuntime {
    id: crate::entity::PublicationKey,
    media: TrackSources,
    origin: Option<ParticipantHandle>,
    publisher: Option<ParticipantHandle>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct InstalledTrackPlan {
    pub local: Vec<ParticipantHandle>,
    pub remote: Vec<crate::shard_update::CompiledRoute>,
    pub wan_transit: bool,
    pub reverse_route: Option<crate::shard_update::CompiledRoute>,
}

fn admits_fabric_encoding<'a>(
    sources: impl ExactSizeIterator<Item = Option<&'a FabricSource>>,
    kind: crate::entity::TrackKind,
    encoding: Option<crate::route::fabric::RtpSourceEncoding>,
    emission: Emission,
    same_owner: impl Fn(&FabricIngress) -> bool,
) -> bool {
    use crate::entity::TrackKind;
    use pulsebeam_core::fabric::{ExtensionKind, Profile};
    if sources.len() == 0 {
        return true;
    }
    let Some(encoding) = encoding else {
        return false;
    };
    let dd = ExtensionKind::DependencyDescriptor;
    kind == TrackKind::Video
        && emission.profile() == Profile::H264
        && emission.includes(dd)
        && sources.len() < MAX_TRACK_FABRIC_SOURCES
        && sources.into_iter().all(|source| {
            source.is_some_and(|source| {
                source.fanout.emission().profile() == Profile::H264
                    && source.fanout.emission().includes(dd)
                    && same_owner(&source.ingress)
                    && source.encoding.is_some_and(|existing| {
                        existing.primary != encoding.primary
                            && existing.rid.zip(encoding.rid).is_none_or(|(a, b)| a != b)
                    })
            })
        })
}

fn fabric_profile_fits(kind: crate::entity::TrackKind, emission: Emission) -> bool {
    use crate::entity::TrackKind;
    use pulsebeam_core::fabric::Profile;
    matches!(
        (kind, emission.profile()),
        (TrackKind::Audio, Profile::Opus)
            | (TrackKind::Video, Profile::H264 | Profile::Av1)
            | (TrackKind::Data, Profile::Data)
    )
}

pub(crate) struct ShardRuntime {
    tracks: SlotMap<TrackHandle, TrackRuntime>,
    track_handles: HashMap<crate::entity::PublicationKey, TrackHandle>,
    fabric_sources: SlotMap<FabricSourceHandle, FabricSource>,
    last_fabric_incarnation: u64,
    shard_id: ShardId,
    pub(crate) routes: RouteRuntime,
}

impl ShardRuntime {
    pub fn new(shard_id: ShardId) -> Self {
        Self {
            tracks: SlotMap::with_key(),
            track_handles: HashMap::new(),
            fabric_sources: SlotMap::with_key(),
            last_fabric_incarnation: 0,
            shard_id,
            routes: RouteRuntime::new(shard_id),
        }
    }

    pub(crate) fn track_handle(
        &self,
        publication_id: crate::entity::PublicationKey,
    ) -> Option<TrackHandle> {
        self.track_handles.get(&publication_id).copied()
    }

    pub(crate) fn publication_id(
        &self,
        handle: TrackHandle,
    ) -> Option<crate::entity::PublicationKey> {
        self.tracks.get(handle).map(|runtime| runtime.id)
    }

    pub(crate) fn source_origin(&self, handle: TrackHandle) -> Option<ParticipantHandle> {
        self.tracks.get(handle).and_then(|runtime| runtime.origin)
    }

    pub(crate) fn retire_track(&mut self, publication_id: crate::entity::PublicationKey) {
        let Some(handle) = self.track_handles.remove(&publication_id) else {
            return;
        };
        if let Some(track) = self.tracks.remove(handle) {
            for &source in track.media.sources() {
                self.retire_fabric_source(source);
            }
        }
    }

    /// A Fabric source is opted in explicitly, never by an ordinary track plan.
    pub(crate) fn install_fabric_source(
        &mut self,
        track: TrackHandle,
        publisher: ParticipantHandle,
        emission: Emission,
        refresh_ssrc: Option<std::num::NonZeroU32>,
        encoding: Option<crate::route::fabric::RtpSourceEncoding>,
    ) -> Option<FabricSourceHandle> {
        use crate::entity::TrackKind;

        if emission.profile() == pulsebeam_core::fabric::Profile::Data
            && fabric_reservations_full(&self.fabric_sources)
        {
            return None;
        }
        let bound = self.tracks.get_mut(track)?;
        if bound.origin.or(bound.publisher) != Some(publisher)
            || !admits_fabric_encoding(
                bound
                    .media
                    .sources()
                    .iter()
                    .map(|source| self.fabric_sources.get(*source)),
                bound.id.kind(),
                encoding,
                emission,
                |ingress| matches!(ingress, FabricIngress::Publisher { participant, .. } if *participant == publisher),
            )
            || (refresh_ssrc.is_some() && bound.id.kind() != TrackKind::Video)
            || !fabric_profile_fits(bound.id.kind(), emission)
        {
            return None;
        }
        let incarnation = NonZeroU64::new(self.last_fabric_incarnation.checked_add(1)?)?;
        self.last_fabric_incarnation = incarnation.get();
        let source = self.fabric_sources.insert(FabricSource {
            track,
            encoding,
            ingress: FabricIngress::Publisher {
                participant: publisher,
                incarnation,
                data: None,
                destinations: Vec::new(),
                publisher_sends: Vec::new(),
                data_expectations: None,
                refresh: refresh_ssrc.map(|primary_ssrc| FabricRefresh {
                    primary_ssrc,
                    last_requested: None,
                }),
            },
            fanout: SourceFanout::new(emission),
            reliable_installation: None,
            publisher_clock: None,
            publisher_timing: None,
        });
        bound.media.insert_fabric(source);
        Some(source)
    }

    pub(super) fn fabric_incarnation(&self, source: FabricSourceHandle) -> Option<NonZeroU64> {
        Some(match &self.fabric_sources.get(source)?.ingress {
            FabricIngress::Publisher { incarnation, .. }
            | FabricIngress::Network { incarnation, .. } => *incarnation,
            FabricIngress::Shard { binding, .. } => binding.incarnation,
        })
    }

    pub(super) fn bind_shard_fabric_destination(
        &mut self,
        binding: ShardBinding,
        plan: &InstalledTrackPlan,
    ) -> Option<()> {
        if binding.origin != self.shard_id || binding.destination.shard() == self.shard_id {
            return None;
        }
        let route = plan
            .remote
            .iter()
            .find(|route| route.address == binding.destination && route.targets_local_route())?;
        let source =
            self.fabric_source_for_incarnation(binding.publication, binding.incarnation)?;
        let FabricIngress::Publisher { destinations, .. } =
            &mut self.fabric_sources.get_mut(source)?.ingress
        else {
            return None;
        };
        if let Some(existing) = destinations
            .iter()
            .find(|branch| branch.binding.destination.shard() == route.address.shard())
        {
            return (existing.binding == binding && existing.route == *route).then_some(());
        }
        if destinations.len() >= MAX_BRANCHES {
            return None;
        }
        destinations.push(ShardFabricDestination {
            binding,
            route: route.clone(),
            admissions: LiveAdmissions::default(),
            ordered_receivers: Vec::new(),
        });
        Some(())
    }

    pub(super) fn set_shard_data_ready(
        &mut self,
        ready: crate::route::fabric::ShardDataReady,
        enabled: bool,
    ) -> Option<()> {
        let track = self.track_handle(ready.binding.publication)?;
        let source = self.tracks.get(track)?.media.fabric()?;
        let bound = self.fabric_sources.get_mut(source)?;
        if bound.fanout.emission().profile() != pulsebeam_core::fabric::Profile::Data {
            return None;
        }
        let FabricIngress::Publisher { destinations, .. } = &mut bound.ingress else {
            return None;
        };
        let destination = destinations
            .iter_mut()
            .find(|entry| entry.binding == ready.binding && entry.route.tx.is_live())?;
        let receivers = &mut destination.ordered_receivers;
        if !enabled {
            let index = receivers
                .iter()
                .position(|receiver| *receiver == ready.receiver)?;
            receivers.swap_remove(index);
            return Some(());
        }
        if let Some(current) = receivers
            .iter_mut()
            .find(|current| current.connection == ready.receiver.connection)
        {
            if current.branch > ready.receiver.branch {
                return None;
            }
            *current = ready.receiver;
        } else if receivers.len() < MAX_BRANCHES {
            receivers.push(ready.receiver);
        } else {
            return None;
        }
        #[cfg(feature = "sim")]
        crate::sim_metrics::record_routing_counter("fabric_shard_data_ready");
        Some(())
    }

    pub(super) fn install_shard_fabric_source(
        &mut self,
        binding: ShardBinding,
        encoding: Option<crate::route::fabric::RtpSourceEncoding>,
        emission: Emission,
        plan: &InstalledTrackPlan,
    ) -> Option<FabricSourceHandle> {
        if plan.local.is_empty() || plan.local.len() > MAX_BRANCHES {
            return None;
        }
        let track = self.shard_fabric_track(binding)?;
        let existing_source = self.shard_fabric_source(binding);
        let bound = self.tracks.get_mut(track)?;
        if bound.origin.is_some()
            || bound.publisher.is_some()
            || !fabric_profile_fits(bound.id.kind(), emission)
        {
            return None;
        }
        if let Some(source) = existing_source {
            let existing = self.fabric_sources.get_mut(source)?;
            let FabricIngress::Shard {
                binding: installed,
                members,
                ..
            } = &mut existing.ingress
            else {
                return None;
            };
            if *installed != binding
                || existing.encoding != encoding
                || existing.fanout.emission() != emission
            {
                return None;
            }
            members.clone_from(&plan.local);
            return Some(source);
        }
        if !admits_fabric_encoding(
            bound
                .media
                .sources()
                .iter()
                .map(|source| self.fabric_sources.get(*source)),
            bound.id.kind(),
            encoding,
            emission,
            |ingress| matches!(ingress, FabricIngress::Shard { binding: existing, .. } if existing.origin == binding.origin && existing.destination == binding.destination && existing.incarnation != binding.incarnation),
        ) || (emission.profile() == pulsebeam_core::fabric::Profile::Data
            && fabric_reservations_full(&self.fabric_sources))
        {
            return None;
        }
        let source = self.fabric_sources.insert(FabricSource {
            track,
            encoding,
            ingress: FabricIngress::Shard {
                binding,
                members: plan.local.clone(),
                last_refresh: None,
            },
            fanout: SourceFanout::new(emission),
            reliable_installation: None,
            publisher_clock: None,
            publisher_timing: None,
        });
        bound.media.insert_fabric(source);
        Some(source)
    }

    fn shard_fabric_track(&self, binding: ShardBinding) -> Option<TrackHandle> {
        if binding.origin == binding.destination.shard() {
            return None;
        }
        let RouteAction::Forward { target } = self.routes.resolve(binding.destination)? else {
            return None;
        };
        (self.publication_id(target) == Some(binding.publication)).then_some(target)
    }

    pub(super) fn shard_fabric_source(&self, binding: ShardBinding) -> Option<FabricSourceHandle> {
        let track = self.shard_fabric_track(binding)?;
        self.tracks
            .get(track)?
            .media
            .sources()
            .iter()
            .copied()
            .find(|&source| {
                self.fabric_sources
                    .get(source)
                    .is_some_and(|source| source.ingress.owner() == FabricOwner::Shard(binding))
            })
    }

    pub(super) fn offer_shard_fabric(
        &mut self,
        now: pulsebeam_runtime::time::Instant,
        offer: ShardOffer,
    ) -> Result<Offers, Rejection> {
        let source = self
            .shard_fabric_source(offer.binding)
            .ok_or(Rejection::Binding)?;
        let bound = self
            .fabric_sources
            .get_mut(source)
            .ok_or(Rejection::Binding)?;
        if !matches!(&bound.ingress, FabricIngress::Shard { members, .. } if !members.is_empty()) {
            return Err(Rejection::Binding);
        }
        bound.fanout.offer_from_shard(now, offer)
    }

    pub(super) fn install_live_network_child(
        &mut self,
        child: LiveNetworkChild,
        lifetime: pulsebeam_link::BindingCutoff,
        plan: &InstalledTrackPlan,
    ) -> Option<LiveNetworkChild> {
        let RouteAction::Forward { target } = self.routes.resolve(child.destination)? else {
            return None;
        };
        if !lifetime.is_live()
            || child.ingress.purpose != pulsebeam_link::FabricPurpose::Live
            || self.publication_id(target) != Some(child.publication)
            || !child.window.map_or_else(
                || child.emission.fits_registry(child.major, child.revision),
                |window| {
                    window.profile() == child.emission.profile()
                        && window.emission() == child.emission.wire_profile().1
                        && window.ssrc().get().to_be_bytes() == child.ingress.key.to_bytes()
                },
            )
            || (plan.local.is_empty() && !(plan.wan_transit && child.window.is_some()))
            || plan.local.len() > MAX_BRANCHES
        {
            return None;
        }
        if self.fabric_sources.values().any(|source| {
            matches!(&source.ingress, FabricIngress::Network { children, .. }
                if children.iter().any(|bound| bound.spec.ingress == child.ingress && bound.spec != child))
        }) {
            return None;
        }
        let track = self.tracks.get_mut(target)?;
        if track.origin.is_some()
            || track.publisher.is_some()
            || !fabric_profile_fits(track.id.kind(), child.emission)
        {
            return None;
        }
        if let Some(source) = track.media.fabric() {
            let source = self.fabric_sources.get_mut(source)?;
            let FabricIngress::Network {
                incarnation,
                emission,
                sequence_anchor,
                children,
                last_ingress,
                members,
                ..
            } = &mut source.ingress
            else {
                return None;
            };
            if *incarnation != child.incarnation
                || *emission != child.emission
                || (child.window.is_none()
                    && child.emission.profile() != pulsebeam_core::fabric::Profile::Data
                    && *sequence_anchor != child.sequence_anchor)
                || children
                    .iter()
                    .any(|member| member.spec.window.is_some() != child.window.is_some())
                || members.len() != plan.local.len()
                || !plan.local.iter().all(|member| {
                    members.contains(member) && source.fanout.branch_for(*member).is_some()
                })
            {
                return None;
            }
            if let Some(existing) = children.iter().find(|entry| entry.spec == child) {
                return existing.lifetime.is_live().then_some(child);
            }
            children.retain(|entry| entry.lifetime.is_live());
            if children.len() >= MAX_BRANCHES {
                return None;
            }
            let ingress = last_ingress.checked_add(1)?;
            children.push(NetworkChild {
                ingress,
                spec: child,
                lifetime,
                media_gaps: Default::default(),
            });
            *last_ingress = ingress;
        } else {
            if !track.media.sources().is_empty() || fabric_reservations_full(&self.fabric_sources) {
                return None;
            }
            let mut fanout = SourceFanout::new(child.emission);
            for &member in &plan.local {
                fanout.install_branch(member, child.emission)?;
            }
            let source = self.fabric_sources.insert(FabricSource {
                track: target,
                encoding: None,
                ingress: FabricIngress::Network {
                    incarnation: child.incarnation,
                    emission: child.emission,
                    sequence_anchor: child.sequence_anchor,
                    children: vec![NetworkChild {
                        ingress: NonZeroU64::MIN,
                        spec: child,
                        lifetime,
                        media_gaps: Default::default(),
                    }],
                    repairs: Vec::new(),
                    media_repairs: Vec::new(),
                    last_ingress: NonZeroU64::MIN,
                    members: plan.local.clone(),
                    wan_transit: plan.wan_transit,
                    relays: LiveRelays::default(),
                    expected_parents: None,
                },
                fanout,
                reliable_installation: None,
                publisher_clock: None,
                publisher_timing: None,
            });
            track.media.insert_fabric(source);
        }
        Some(child)
    }

    pub(super) fn network_fabric_source(
        &self,
        address: NodeRouteAddress,
    ) -> Option<FabricSourceHandle> {
        let RouteAction::Forward { target } = self.routes.resolve(address)? else {
            return None;
        };
        let source = self.tracks.get(target)?.media.fabric()?;
        let bound = self.fabric_sources.get(source)?;
        matches!(&bound.ingress, FabricIngress::Network { .. }).then_some(source)
    }

    pub(super) fn is_shard_fabric_source(&self, source: FabricSourceHandle) -> bool {
        self.fabric_sources
            .get(source)
            .is_some_and(|bound| matches!(bound.ingress, FabricIngress::Shard { .. }))
    }

    pub(super) fn remote_fabric_branch(
        &self,
        source: FabricSourceHandle,
        member: ParticipantHandle,
    ) -> Option<FabricLocalBranch> {
        let bound = self.fabric_sources.get(source)?;
        if matches!(bound.ingress.owner(), FabricOwner::Local(_)) {
            return None;
        }
        let branch = bound.fanout.branch_for(member)?;
        let handle = FabricLocalBranch { source, branch };
        self.fabric_handoff_owner(handle, member)?;
        Some(handle)
    }

    pub(super) fn network_fabric_branch(
        &self,
        address: NodeRouteAddress,
        member: ParticipantHandle,
    ) -> Option<FabricLocalBranch> {
        let source = self.network_fabric_source(address)?;
        self.remote_fabric_branch(source, member)
    }

    pub(super) fn request_network_repair(
        &mut self,
        source: FabricSourceHandle,
        target: RepairTarget,
        receiver: crate::entity::ConnectionId,
        request: RepairRequest,
        now: pulsebeam_runtime::time::Instant,
    ) -> Result<Interest, RepairAdmission> {
        let bound = self
            .fabric_sources
            .get_mut(source)
            .ok_or(RepairAdmission::Ineligible)?;
        let parent = bound
            .repair_parent(request, now)
            .ok_or(RepairAdmission::Ineligible)?;
        match bound.fanout.request_data_repair(now, target, request) {
            RepairAdmission::First | RepairAdmission::Replay => {}
            other => return Err(other),
        }
        let expires = bound
            .fanout
            .pending_repair_until(now, target, request)
            .ok_or(RepairAdmission::Replay)?;
        Ok(Interest {
            parent,
            receiver,
            branch: target.incarnation,
            request,
            expires,
        })
    }

    pub(super) fn settle_unavailable_network_repair(
        &mut self,
        source: FabricSourceHandle,
        target: RepairTarget,
        interest: Interest,
        now: pulsebeam_runtime::time::Instant,
    ) -> bool {
        let Some(bound) = self.fabric_sources.get(source) else {
            return false;
        };
        if interest.branch != target.incarnation
            || bound.repair_parent(interest.request, now) != Some(interest.parent)
            || bound
                .fanout
                .pending_repair_until(now, target, interest.request)
                != Some(interest.expires)
        {
            return false;
        }
        self.cancel_network_repair(
            source,
            target,
            Some(interest.receiver),
            interest.request,
            now,
        )
        .is_some()
    }

    pub(super) fn cancel_network_repair(
        &mut self,
        source: FabricSourceHandle,
        target: RepairTarget,
        receiver: Option<crate::entity::ConnectionId>,
        request: RepairRequest,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<Interest> {
        let bound = self.fabric_sources.get_mut(source)?;
        let interest = (|| {
            Some(Interest {
                parent: bound.repair_parent(request, now)?,
                receiver: receiver?,
                branch: target.incarnation,
                request,
                expires: bound.fanout.pending_repair_until(now, target, request)?,
            })
        })();
        let FabricIngress::Network { repairs, .. } = &mut bound.ingress else {
            return None;
        };
        if !bound.fanout.cancel_data_repair(target, request) {
            return None;
        }
        repairs.retain(|repair| repair.target != target || repair.child.demand.request != request);
        interest
    }

    pub(super) fn install_network_repair(
        &mut self,
        child: crate::route::fabric::network_repair::RepairChild,
        target: crate::route::fabric::local::RepairTarget,
        lifetime: pulsebeam_link::BindingCutoff,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<crate::route::fabric::network_repair::RepairChild> {
        use crate::route::fabric::network_repair::{Admission, RepairIngress};
        let route = child.route;
        if !lifetime.is_live() || target.incarnation != route.branch {
            return None;
        }
        let source = self.network_fabric_source(child.destination)?;
        let bound = self.fabric_sources.get_mut(source)?;
        let expected_parent = bound.repair_parent(child.demand.request, now)?;
        if !child.matches_parent(expected_parent) {
            return None;
        }
        let FabricIngress::Network {
            children, repairs, ..
        } = &mut bound.ingress
        else {
            return None;
        };
        if !children
            .iter()
            .any(|parent| parent.lifetime.is_live() && parent.spec == expected_parent)
        {
            return None;
        }
        let deadline = bound
            .fanout
            .pending_repair_until(now, target, child.demand.request)?;
        if child.demand.expires > deadline {
            return None;
        }
        repairs.retain(|repair| repair.lifetime.is_live() && now < repair.child.demand.expires);
        if repairs.len() >= MAX_BRANCHES
            || repairs
                .iter()
                .any(|repair| repair.child.route.ingress == route.ingress)
        {
            return None;
        }
        let mut ingress = RepairIngress::new(route, child.demand.expires)?;
        if ingress.install(route, child.demand, now) != Admission::Installed
            || !ingress.acknowledge(route, child.demand, now)
            || !bound
                .fanout
                .arm_network_repair(target, child.demand.request)
        {
            return None;
        }
        repairs.push(NetworkRepair {
            child,
            parent: expected_parent,
            target,
            lifetime,
            ingress,
        });
        Some(child)
    }

    pub(super) fn install_live_relay(
        &mut self,
        now: pulsebeam_runtime::time::Instant,
        compiled: CompiledLiveRelay,
    ) -> bool {
        let Some(source) = self
            .network_fabric_source(compiled.input.destination)
            .and_then(|source| self.fabric_sources.get_mut(source))
        else {
            return false;
        };
        let FabricIngress::Network {
            children, relays, ..
        } = &mut source.ingress
        else {
            return false;
        };
        if !children
            .iter()
            .any(|child| child.spec == compiled.input && child.lifetime.is_live())
            || compiled.input.ingress.association == compiled.next.peer
        {
            return false;
        }
        relays.install(now, compiled)
    }

    pub(super) fn retire_live_relay(&mut self, relay: &CompiledLiveRelay) -> bool {
        if relay.tx.is_live() {
            return false;
        }
        if let Some(source) = self.network_fabric_source(relay.input.destination)
            && let Some(bound) = self.fabric_sources.get_mut(source)
            && let FabricIngress::Network { relays, .. } = &mut bound.ingress
        {
            relays.retire(relay);
        }
        true
    }

    pub(super) fn network_repair_target(
        &self,
        address: NodeRouteAddress,
        ingress: pulsebeam_link::FabricIngress,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<(
        crate::route::fabric::network_repair::RepairChild,
        FabricLocalBranch,
        ParticipantHandle,
    )> {
        let source = self.network_fabric_source(address)?;
        let bound = self.fabric_sources.get(source)?;
        let FabricIngress::Network {
            children, repairs, ..
        } = &bound.ingress
        else {
            return None;
        };
        let repair = repairs.iter().find(|repair| {
            repair.child.destination == address && repair.child.route.ingress == ingress
        })?;
        repair.parent_at(children, now)?;
        bound
            .fanout
            .pending_repair_until(now, repair.target, repair.child.demand.request)?;
        Some((
            repair.child,
            FabricLocalBranch::for_repair(source, repair.target),
            repair.target.destination,
        ))
    }

    pub(super) fn offer_network_repair(
        &mut self,
        child: crate::route::fabric::network_repair::RepairChild,
        now: pulsebeam_runtime::time::Instant,
        packet: OwnedPacket,
    ) -> bool {
        use crate::route::fabric::local::RepairQueue;
        if packet.header().sequence != child.demand.wire_sequence() {
            return false;
        }
        let Some(source) = self.network_fabric_source(child.destination) else {
            return false;
        };
        let Some(bound) = self.fabric_sources.get_mut(source) else {
            return false;
        };
        let FabricIngress::Network {
            children, repairs, ..
        } = &mut bound.ingress
        else {
            return false;
        };
        let Some(repair) = repairs.iter_mut().find(|repair| repair.child == child) else {
            return false;
        };
        let Some(parent) = repair.parent_at(children, now) else {
            return false;
        };
        let demand = child.demand;
        let Some(current) = repair.ingress.claim(
            child.route.ingress,
            demand.request.fragment,
            demand.sequence,
            now,
        ) else {
            return bound.fanout.repair_is_queued(repair.target, demand.request);
        };
        if current != demand {
            repair.ingress.release(child.route, current, now);
            return false;
        }
        let queued =
            bound
                .fanout
                .enqueue_network_repair(now, repair.target, demand, packet, parent);
        if queued != RepairQueue::Queued {
            repair.ingress.release(child.route, demand, now);
        }
        queued != RepairQueue::Unavailable
    }

    pub(super) fn install_media_repair_child(
        &mut self,
        child: crate::route::fabric::network_repair::MediaRepairChild,
        lifetime: pulsebeam_link::BindingCutoff,
        now: pulsebeam_runtime::time::Instant,
    ) -> bool {
        let Some(source) = self
            .network_fabric_source(child.parent.destination)
            .and_then(|source| self.fabric_sources.get_mut(source))
        else {
            return false;
        };
        let FabricIngress::Network {
            children,
            media_repairs,
            members,
            wan_transit,
            ..
        } = &mut source.ingress
        else {
            return false;
        };
        if !child.valid()
            || source.fanout.has_media_position(child.position)
            || !lifetime.is_live()
            || now >= child.expires
            || (members.is_empty() && !*wan_transit)
            || !children
                .iter()
                .any(|parent| parent.spec == child.parent && parent.lifetime.is_live())
        {
            return false;
        }
        media_repairs.retain(|repair| {
            now < repair.child.expires
                && repair.lifetime.is_live()
                && children
                    .iter()
                    .any(|parent| parent.spec == repair.child.parent && parent.lifetime.is_live())
        });
        if media_repairs.len() >= 32
            || media_repairs
                .iter()
                .any(|repair| repair.child.ingress() == child.ingress())
        {
            return false;
        }
        media_repairs.push(NetworkMediaRepair { child, lifetime });
        true
    }

    pub(super) fn take_media_repair(
        &mut self,
        address: NodeRouteAddress,
        ingress: pulsebeam_link::FabricIngress,
        image: NetworkPacket,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<(pulsebeam_link::FabricIngress, NetworkPacket)> {
        let NetworkPacket {
            window,
            position,
            packet,
            ..
        } = image;
        let source = self.network_fabric_source(address)?;
        let FabricIngress::Network {
            children,
            media_repairs,
            ..
        } = &mut self.fabric_sources.get_mut(source)?.ingress
        else {
            return None;
        };
        let index = media_repairs.iter().position(|repair| {
            repair.child.ingress() == ingress
                && repair.child.parent.destination == address
                && repair.child.window == window
                && repair.child.position == position
                && now < repair.child.expires
                && repair.lifetime.is_live()
                && children
                    .iter()
                    .any(|parent| parent.spec == repair.child.parent && parent.lifetime.is_live())
        })?;
        let repair = media_repairs.swap_remove(index);
        let parent = repair.child.parent;
        Some((
            parent.ingress,
            NetworkPacket {
                window: parent.window?,
                position,
                packet,
                capture: None,
            },
        ))
    }

    pub(super) fn observe_network_media_position(
        &mut self,
        address: NodeRouteAddress,
        ingress: pulsebeam_link::FabricIngress,
        position: u64,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<crate::route::fabric::network_repair::MediaInterest> {
        let source = self.network_fabric_source(address)?;
        let bound = self.fabric_sources.get_mut(source)?;
        let FabricIngress::Network { children, .. } = &mut bound.ingress else {
            return None;
        };
        let path = children.iter_mut().find(|child| {
            child.spec.destination == address
                && child.spec.ingress == ingress
                && child.lifetime.is_live()
        })?;
        let (position, expires) = path.media_gaps.observe(position, now, |candidate| {
            bound.fanout.has_media_position(candidate)
        })?;
        Some(crate::route::fabric::network_repair::MediaInterest {
            parent: path.spec,
            branch: path.ingress,
            position,
            expires,
        })
    }

    pub(super) fn offer_live_network(
        &mut self,
        address: NodeRouteAddress,
        ingress: pulsebeam_link::FabricIngress,
        now: pulsebeam_runtime::time::Instant,
        arrival: pulsebeam_runtime::time::Instant,
        image: NetworkPacket,
    ) -> Result<Offers, Rejection> {
        let source = self
            .network_fabric_source(address)
            .ok_or(Rejection::Binding)?;
        let bound = self
            .fabric_sources
            .get_mut(source)
            .ok_or(Rejection::Binding)?;
        let FabricIngress::Network {
            children,
            members,
            wan_transit,
            repairs,
            media_repairs,
            relays,
            ..
        } = &mut bound.ingress
        else {
            return Err(Rejection::Binding);
        };
        let child = children
            .iter()
            .find(|child| {
                child.spec.destination == address
                    && child.spec.ingress == ingress
                    && child.lifetime.is_live()
            })
            .ok_or(Rejection::Binding)?;
        let input = child.spec;
        if members.is_empty() && !(*wan_transit && input.window.is_some()) {
            return Err(Rejection::Binding);
        }
        let NetworkPacket {
            window,
            position,
            packet,
            capture,
        } = image;
        if input.window != Some(window) || !window.contains(position) {
            return Err(Rejection::Binding);
        }
        let hops = packet.remaining_hops();
        let admission =
            bound
                .fanout
                .offer_bound_network(now, arrival, position, child.ingress, packet);
        let (sequence, mut report) = match admission {
            Ok(admitted) => admitted,
            Err(error) => {
                if error == Rejection::Conflict {
                    for child in children {
                        child.lifetime.fence();
                    }
                    repairs.clear();
                    media_repairs.clear();
                }
                return Err(error);
            }
        };
        if let Some(capture) = capture {
            bound
                .fanout
                .attach_network_capture(sequence, child.ingress, arrival, capture);
        }
        media_repairs.retain(|repair| {
            repair.child.position != sequence
                && now < repair.child.expires
                && repair.lifetime.is_live()
        });
        for repair in repairs.iter() {
            if repair.child.demand.sequence == sequence && repair.parent_at(children, now).is_some()
            {
                bound.fanout.claim_live_repair(
                    now,
                    repair.target,
                    repair.child.demand.request,
                    sequence,
                );
            }
        }
        if hops != 0
            && let Some(frozen) = bound.fanout.transit(sequence, hops)
        {
            relays.offer(now, input, frozen, &mut report);
        }
        Ok(report)
    }

    pub(super) fn retire_fabric_route(&mut self, address: NodeRouteAddress) {
        self.fabric_sources.retain(|source, bound| {
            match &mut bound.ingress {
                FabricIngress::Shard { binding, .. } if binding.destination == address => {
                    if let Some(track) = self.tracks.get_mut(bound.track) {
                        track.media.remove_fabric(source);
                    }
                    return false;
                }
                FabricIngress::Network {
                    children,
                    repairs,
                    media_repairs,
                    relays,
                    ..
                } => {
                    children.retain(|child| child.spec.destination != address);
                    repairs.retain(|repair| repair.child.destination != address);
                    media_repairs.retain(|repair| repair.child.parent.destination != address);
                    relays.retire_input(address);
                }
                _ => {}
            }
            true
        });
    }

    pub(super) fn local_fabric_publication(
        &self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
    ) -> Option<crate::entity::PublicationKey> {
        let bound = self.fabric_sources.get(source)?;
        if bound.ingress.owner() != FabricOwner::Local(publisher) {
            return None;
        }
        self.publication_id(bound.track)
    }

    pub(super) fn fence_local_fabric_source(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
    ) {
        if self.local_fabric_publication(source, publisher).is_some() {
            self.retire_fabric_source(source);
        }
    }

    pub(crate) fn retire_fabric_source(&mut self, source: FabricSourceHandle) -> bool {
        let Some(retired) = self.fabric_sources.remove(source) else {
            return false;
        };
        if let Some(track) = self.tracks.get_mut(retired.track) {
            track.media.remove_fabric(source);
        }
        true
    }

    pub(crate) fn retire_fabric_participant(&mut self, participant: ParticipantHandle) {
        self.fabric_sources.retain(|key, source| {
            if source.ingress.owner() == FabricOwner::Local(participant) {
                if let Some(track) = self.tracks.get_mut(source.track) {
                    track.media.remove_fabric(key);
                }
                return false;
            }
            source
                .ingress
                .retain_members(|destination| destination != participant);
            source
                .fanout
                .retain_destinations(|destination| destination != participant);
            true
        });
    }

    pub(crate) fn reconcile_fabric_plan(
        &mut self,
        track: TrackHandle,
        plan: Option<&InstalledTrackPlan>,
    ) {
        let Some(track) = self.tracks.get(track) else {
            return;
        };
        for &key in track.media.sources() {
            let Some(source) = self.fabric_sources.get_mut(key) else {
                continue;
            };
            let still_authorized =
                |destination| plan.is_some_and(|plan| plan.local.contains(&destination));
            source.ingress.retain_members(still_authorized);
            source.fanout.retain_destinations(still_authorized);
            if let FabricIngress::Network {
                wan_transit,
                members,
                children,
                emission,
                repairs,
                ..
            } = &mut source.ingress
            {
                if let Some(plan) = plan {
                    for &member in &plan.local {
                        if !members.contains(&member)
                            && source.fanout.install_branch(member, *emission).is_some()
                        {
                            members.push(member);
                        }
                    }
                }
                let permitted = plan.is_some_and(|plan| plan.wan_transit);
                if *wan_transit && !permitted && members.is_empty() {
                    for child in children {
                        child.lifetime.fence();
                    }
                    repairs.clear();
                }
                *wan_transit = permitted;
            }
            if let FabricIngress::Publisher { destinations, .. } = &mut source.ingress {
                destinations.retain(|destination| {
                    plan.is_some_and(|plan| plan.remote.contains(&destination.route))
                });
            }
        }
    }

    pub(super) fn fabric_sequence_anchor(
        &self,
        source: FabricSourceHandle,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<crate::shard_update::PublisherPosition> {
        let source = self.fabric_sources.get(source)?;
        let sequence = source.fanout.live_sequence_anchor(now)?;
        let timing = source.publisher_timing.and_then(|(position, at, timing)| {
            (position == sequence
                && now.checked_duration_since(at)?
                    < crate::route::fabric::network_repair::MAX_REPAIR_LIFETIME)
                .then_some(timing)
        });
        Some(crate::shard_update::PublisherPosition { sequence, timing })
    }

    pub(super) fn fabric_emission(&self, source: FabricSourceHandle) -> Option<Emission> {
        Some(self.fabric_sources.get(source)?.fanout.emission())
    }

    pub(super) fn fabric_source_for_track(&self, track: TrackHandle) -> Option<FabricSourceHandle> {
        self.tracks.get(track)?.media.fabric()
    }

    fn fabric_source_for_incarnation(
        &self,
        publication: crate::entity::PublicationKey,
        incarnation: NonZeroU64,
    ) -> Option<FabricSourceHandle> {
        self.tracks
            .get(self.track_handle(publication)?)?
            .media
            .sources()
            .iter()
            .copied()
            .find(|&source| self.fabric_incarnation(source) == Some(incarnation))
    }

    pub(super) fn fabric_dd_source(
        &self,
        source: FabricSourceHandle,
    ) -> Option<(TrackHandle, crate::route::fabric::RtpSourceEncoding)> {
        let source = self.fabric_sources.get(source)?;
        source
            .fanout
            .emission()
            .selects_dd_encoding()
            .then(|| Some((source.track, source.encoding?)))?
    }

    pub(super) fn fabric_source_for_encoding(
        &self,
        track: TrackHandle,
        selector: crate::track::SourceEncoding,
    ) -> Option<FabricSourceHandle> {
        let mut sources = self
            .tracks
            .get(track)?
            .media
            .sources()
            .iter()
            .copied()
            .filter(|handle| {
                self.fabric_sources
                    .get(*handle)
                    .and_then(|source| source.encoding)
                    .is_some_and(|encoding| selector.matches(encoding.primary, encoding.rid))
            });
        let source = sources.next()?;
        sources.next().is_none().then_some(source)
    }

    pub(super) fn fabric_branch_target(&self, branch: FabricLocalBranch) -> Option<u8> {
        Some(
            self.fabric_sources
                .get(branch.source)?
                .fanout
                .branch_target(branch.branch)?
                .index(),
        )
    }

    pub(super) fn fabric_destinations(
        &self,
        source: FabricSourceHandle,
    ) -> impl Iterator<Item = (FabricLocalBranch, ParticipantHandle)> + '_ {
        self.fabric_sources
            .get(source)
            .into_iter()
            .flat_map(move |bound| {
                bound
                    .fanout
                    .destinations()
                    .map(move |(branch, destination)| {
                        (FabricLocalBranch { source, branch }, destination)
                    })
            })
    }

    pub(super) fn retire_fabric_branch(
        &mut self,
        branch: FabricLocalBranch,
        destination: ParticipantHandle,
    ) {
        if let Some(source) = self.fabric_sources.get_mut(branch.source)
            && source.fanout.authorizes(branch.branch, destination)
        {
            source.fanout.retire_branch(branch.branch);
        }
    }

    pub(crate) fn install_fabric_branch(
        &mut self,
        track: TrackHandle,
        source: FabricSourceHandle,
        destination: ParticipantHandle,
        receiver: Emission,
        plan: &InstalledTrackPlan,
        target: Option<pulsebeam_core::dd::evidence::DecodeTarget>,
    ) -> Option<FabricLocalBranch> {
        let bound = self.fabric_sources.get_mut(source)?;
        if bound.track != track
            || !plan.local.contains(&destination)
            || matches!(&bound.ingress, FabricIngress::Shard { members, .. } | FabricIngress::Network { members, .. } if !members.contains(&destination))
        {
            return None;
        }
        let branch = match target {
            Some(target) => bound
                .fanout
                .install_filtered_branch(destination, receiver, target),
            None => bound.fanout.install_branch(destination, receiver),
        }?;
        Some(FabricLocalBranch { source, branch })
    }

    pub(super) fn install_publisher_send(
        &mut self,
        source: FabricSourceHandle,
        route: CompiledPublisherSend,
        lifetime: pulsebeam_link::BindingCutoff,
        now: pulsebeam_runtime::time::Instant,
    ) -> bool {
        if route.shard != self.shard_id
            || now >= route.expires_at
            || !route.tx.is_live()
            || !lifetime.is_live()
        {
            return false;
        }
        let Some(bound) = self.fabric_sources.get_mut(source) else {
            return false;
        };
        let FabricIngress::Publisher {
            incarnation,
            publisher_sends,
            ..
        } = &mut bound.ingress
        else {
            return false;
        };
        if *incarnation != route.publisher.incarnation
            || self.tracks.get(bound.track).map(|track| track.id)
                != Some(route.publisher.publication)
            || bound.fanout.emission() != route.publisher.emission
        {
            return false;
        }
        publisher_sends.retain(|hop| hop.route.tx.is_live() && now < hop.route.expires_at);
        if !lifetime.is_live() {
            return false;
        }
        if let Some(hop) = publisher_sends
            .iter_mut()
            .find(|hop| hop.route.next == route.next)
        {
            if hop.route.tx != route.tx {
                return false;
            }
            hop.route = route;
        } else if publisher_sends.len() < MAX_BRANCHES {
            publisher_sends.push(PublisherHop {
                route,
                lifetime,
                admissions: LiveAdmissions::default(),
            });
        } else {
            return false;
        }
        true
    }

    pub(super) fn retire_publisher_send(&mut self, route: &CompiledPublisherSend) -> bool {
        if route.shard != self.shard_id || route.tx.is_live() {
            return false;
        }
        if let Some(source) = self
            .track_handle(route.publisher.publication)
            .and_then(|track| self.fabric_source_for_track(track))
            && let Some(bound) = self.fabric_sources.get_mut(source)
            && let FabricIngress::Publisher {
                publisher_sends, ..
            } = &mut bound.ingress
        {
            publisher_sends.retain(|hop| hop.route.tx != route.tx);
        }
        true
    }

    pub(super) fn next_endpoint_data_expiry(&self) -> Option<pulsebeam_runtime::time::Instant> {
        self.fabric_sources
            .values()
            .filter_map(|bound| match &bound.ingress {
                FabricIngress::Publisher {
                    data: Some(data), ..
                } => data.expiry(),
                _ => None,
            })
            .min()
    }

    pub(super) fn expire_endpoint_data(&mut self, now: pulsebeam_runtime::time::Instant) {
        for bound in self.fabric_sources.values_mut() {
            if let FabricIngress::Publisher {
                data: Some(data), ..
            } = &mut bound.ingress
            {
                data.expire(&mut bound.fanout, now);
            }
        }
    }

    pub(super) fn bind_data_publisher(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        binding: crate::participant::data_fabric::InstalledBinding,
        clock: Option<pulsebeam_proto::data_record::Clock<'_>>,
    ) -> bool {
        let Some(bound) = self.fabric_sources.get_mut(source) else {
            return false;
        };
        let FabricIngress::Publisher {
            participant, data, ..
        } = &mut bound.ingress
        else {
            return false;
        };
        if *participant != publisher
            || data.is_some()
            || bound.fanout.emission().profile() != pulsebeam_core::fabric::Profile::Data
        {
            return false;
        }
        *data = crate::participant::data_fabric::PublisherSource::new(source, binding, clock)
            .map(Box::new);
        data.is_some()
    }

    pub(super) fn data_publisher(
        &self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
    ) -> Option<(
        &crate::participant::data_fabric::PublisherSource,
        &SourceFanout,
    )> {
        let bound = self.fabric_sources.get(source)?;
        let FabricIngress::Publisher {
            participant, data, ..
        } = &bound.ingress
        else {
            return None;
        };
        data.as_deref()
            .filter(|data| *participant == publisher && data.matches(installation))
            .map(|data| (data, &bound.fanout))
    }

    pub(super) fn data_publisher_mut(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
    ) -> Option<(
        &mut crate::participant::data_fabric::PublisherSource,
        &mut SourceFanout,
    )> {
        let bound = self.fabric_sources.get_mut(source)?;
        let FabricIngress::Publisher {
            participant, data, ..
        } = &mut bound.ingress
        else {
            return None;
        };
        data.as_deref_mut()
            .filter(|data| *participant == publisher && data.matches(installation))
            .map(|data| (data, &mut bound.fanout))
    }

    pub(super) fn offer_fabric_data(
        &mut self,
        publisher: ParticipantHandle,
        now: pulsebeam_runtime::time::Instant,
        publication: &crate::participant::event::FabricDataPosition,
    ) -> Result<Offers, Rejection> {
        let bound = self
            .fabric_sources
            .get_mut(publication.source)
            .ok_or(Rejection::Binding)?;
        let FabricIngress::Publisher {
            participant,
            data: Some(data),
            ..
        } = &bound.ingress
        else {
            return Err(Rejection::Binding);
        };
        if *participant != publisher
            || !data.matches(publication.installation)
            || data.sequence_anchor().is_none()
        {
            return Err(Rejection::Binding);
        }
        let sequence = publication.sequence;
        let original = bound
            .fanout
            .endpoint_data(sequence)
            .ok_or(Rejection::Stale)?;
        let hops = original.remaining_hops();
        let ordered = publication.reliable.map(|admission| admission.unit);
        let mut report = bound
            .fanout
            .offer_retained_data(now, publication.arrival, sequence)?;
        Self::forward_publisher_branches(bound, now, sequence, hops, ordered, &mut report);
        Ok(report)
    }

    pub(super) fn bind_fabric_media_identity(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        identity: pulsebeam_core::fabric::media_source::MediaIdentity,
        primary_ssrc: u32,
        reference: Option<(
            pulsebeam_runtime::time::Instant,
            crate::clock::reference::Estimate,
        )>,
        deferred: Option<pulsebeam_rtc::SenderReport>,
    ) -> bool {
        use pulsebeam_clock::time::{Binding, ClockRate, DomainId, Generation};
        let Some(bound) = self.fabric_sources.get_mut(source) else {
            return false;
        };
        let FabricIngress::Publisher {
            participant,
            incarnation,
            ..
        } = bound.ingress
        else {
            return false;
        };
        if participant != publisher || !bound.fanout.bind_media_identity(identity) {
            return false;
        }
        use pulsebeam_core::fabric::Profile;
        let rate = match bound.fanout.emission().profile() {
            Profile::Opus => ClockRate::OPUS,
            Profile::H264 | Profile::Av1 => ClockRate::VIDEO,
            Profile::Data => return false,
        };
        if let Some((now, reference)) = reference
            && let Ok(clock) = crate::rtp::publisher_clock::PublisherClock::admit(
                Binding {
                    domain: DomainId(incarnation.get()),
                    generation: Generation(0),
                },
                primary_ssrc,
                rate,
                now,
                &reference,
                deferred,
            )
        {
            bound.publisher_clock = Some(Box::new(clock));
        }
        true
    }

    pub(super) fn observe_fabric_sender_report(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        now: pulsebeam_runtime::time::Instant,
        report: pulsebeam_rtc::SenderReport,
    ) {
        if let Some(bound) = self.fabric_sources.get_mut(source)
            && bound.ingress.owner() == FabricOwner::Local(publisher)
            && let Some(clock) = bound.publisher_clock.as_mut()
        {
            let _ = clock.report(report, now);
        }
    }

    pub(super) fn offer_fabric_media(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        now: pulsebeam_runtime::time::Instant,
        input: &crate::rtp::fabric_ingress::MediaIngress,
        reference: Option<&crate::clock::reference::Estimate>,
    ) -> Result<Offers, Rejection> {
        let bound = self
            .fabric_sources
            .get_mut(source)
            .ok_or(Rejection::Binding)?;
        let FabricIngress::Publisher {
            participant,
            publisher_sends,
            ..
        } = &bound.ingress
        else {
            return Err(Rejection::Binding);
        };
        if *participant != publisher {
            return Err(Rejection::Binding);
        }
        let peer_repair = publisher_sends
            .iter()
            .any(|hop| now < hop.route.expires_at && hop.route.tx.is_live());
        let clock = &mut bound.publisher_clock;
        let mut assigned = None;
        let Some((sequence, hops, mut report)) =
            bound
                .fanout
                .offer_endpoint_media(now, input, peer_repair, |arrival, timestamp| {
                    if let Some(clock) = clock {
                        clock.sync_host_reference(now, reference);
                        let sample = clock.assign_sample(0, timestamp, arrival, now);
                        assigned = Some(sample.projection);
                        sample.timestamp
                    } else {
                        // Standalone shard fixtures may omit the node clock.
                        // Never export an endpoint RTP coordinate as canonical.
                        0
                    }
                })?
        else {
            return Ok(Offers::default());
        };
        if let Some(projection) = assigned
            && bound.fanout.live_sequence_anchor(now) == Some(sequence)
        {
            bound.publisher_timing = projection.map(|(at, projection)| (sequence, at, projection));
        }
        Self::forward_publisher_branches(bound, now, sequence, hops, None, &mut report);
        Ok(report)
    }

    #[cfg(test)]
    pub(crate) fn offer_fabric(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        now: pulsebeam_runtime::time::Instant,
        arrival: pulsebeam_runtime::time::Instant,
        sequence: u64,
        packet: OwnedPacket,
    ) -> Result<Offers, Rejection> {
        let bound = self
            .fabric_sources
            .get_mut(source)
            .ok_or(Rejection::Binding)?;
        let FabricIngress::Publisher {
            participant,
            destinations,
            ..
        } = &bound.ingress
        else {
            return Err(Rejection::Binding);
        };
        if *participant != publisher {
            return Err(Rejection::Binding);
        }
        let ordered = if bound.reliable_installation.is_some() && !destinations.is_empty() {
            let identity = pulsebeam_proto::data_record::wire_identity(packet.payload())
                .map_err(|_| Rejection::Binding)?;
            Some(crate::route::fabric::local::DataExpectation {
                unit_seq: identity.unit_seq,
                fragment_count: identity.count,
            })
        } else {
            None
        };
        let hops = packet.remaining_hops();
        let mut report = bound.fanout.offer(now, arrival, sequence, packet)?;
        Self::forward_publisher_branches(bound, now, sequence, hops, ordered, &mut report);
        Ok(report)
    }

    fn forward_publisher_branches(
        bound: &mut FabricSource,
        now: pulsebeam_runtime::time::Instant,
        sequence: u64,
        hops: u8,
        ordered: Option<crate::route::fabric::local::DataExpectation>,
        report: &mut Offers,
    ) {
        let FabricIngress::Publisher {
            destinations,
            publisher_sends,
            ..
        } = &mut bound.ingress
        else {
            return;
        };
        if (!destinations.is_empty() || !publisher_sends.is_empty())
            && let Some(frozen) = bound.fanout.transit(sequence, hops)
        {
            for destination in destinations {
                if ordered.is_some_and(|unit| {
                    !bound.fanout.permits_shard_data(
                        now,
                        unit,
                        destination.binding,
                        &destination.route,
                    )
                }) {
                    continue;
                }
                let outcome = frozen.work.enqueue(now, &mut destination.admissions, || {
                    #[cfg(feature = "sim")]
                    if ordered.is_some()
                        && destination.binding.origin != destination.binding.destination.shard()
                        && crate::sim_metrics::drop_shard_data_offer(frozen.packet.payload())
                    {
                        // Simulate loss after admission, not mailbox backpressure.
                        return true;
                    }
                    destination.route.offer_fabric(ShardOffer {
                        binding: destination.binding,
                        arrival: frozen.work.arrival(),
                        ready_at: frozen.ready_at,
                        sequence: frozen.work.sequence(),
                        packet: frozen.packet.clone().into_owned(),
                    })
                });
                report.record(outcome);
            }
            for hop in publisher_sends {
                if let Some(outcome) = crate::route::fabric::relay::offer_live_network(
                    &mut hop.admissions,
                    &hop.route.tx,
                    hop.route.expires_at,
                    &frozen,
                    now,
                ) {
                    report.record(outcome);
                }
            }
        }
    }

    pub(super) fn fabric_handoff_owner(
        &self,
        handle: FabricLocalBranch,
        destination: ParticipantHandle,
    ) -> Option<FabricOwner> {
        let source = self.fabric_sources.get(handle.source)?;
        let owner = source.ingress.owner();
        let bound = match owner {
            FabricOwner::Local(_) => true,
            FabricOwner::Shard(binding) => self.shard_fabric_track(binding) == Some(source.track),
            FabricOwner::Network => {
                matches!(&source.ingress, FabricIngress::Network { children, .. }
                if children.iter().any(|child| child.lifetime.is_live()))
            }
        };
        (bound && source.fanout.authorizes(handle.branch, destination)).then_some(owner)
    }

    pub(super) fn awaiting_fabric_refresh(
        &self,
        source: FabricSourceHandle,
        authorized: impl FnMut(ParticipantHandle) -> bool,
    ) -> Option<(FabricLocalBranch, ParticipantHandle)> {
        let bound = self.fabric_sources.get(source)?;
        if !matches!(
            &bound.ingress,
            FabricIngress::Publisher {
                refresh: Some(_),
                ..
            } | FabricIngress::Shard { .. }
        ) {
            return None;
        }
        let (branch, destination) = bound.fanout.awaiting_entry(authorized)?;
        Some((FabricLocalBranch { source, branch }, destination))
    }

    pub(super) fn lose_fabric_output_context(
        &mut self,
        handle: FabricLocalBranch,
        destination: ParticipantHandle,
    ) -> bool {
        self.fabric_sources
            .get_mut(handle.source)
            .is_some_and(|source| {
                source
                    .fanout
                    .lose_output_context(handle.branch, destination)
            })
    }

    pub(super) fn request_fabric_refresh(
        &mut self,
        handle: FabricLocalBranch,
        destination: ParticipantHandle,
        now: pulsebeam_runtime::time::Instant,
        request: impl FnOnce(ParticipantHandle, u32) -> bool,
    ) -> bool {
        let Some(source) = self.fabric_sources.get_mut(handle.source) else {
            return false;
        };
        let FabricIngress::Publisher {
            participant: publisher,
            refresh: Some(refresh),
            ..
        } = &mut source.ingress
        else {
            return false;
        };
        if !source.fanout.authorizes(handle.branch, destination) {
            return false;
        }
        refresh.request(now, |ssrc| request(*publisher, ssrc))
    }

    pub(super) fn request_peer_media_refresh(
        &mut self,
        source: FabricSourceHandle,
        route: &CompiledPublisherSend,
        position: u64,
        now: pulsebeam_runtime::time::Instant,
        request: impl FnOnce(u32) -> bool,
    ) -> bool {
        let Some(FabricIngress::Publisher {
            publisher_sends,
            refresh: Some(refresh),
            ..
        }) = self
            .fabric_sources
            .get_mut(source)
            .map(|bound| &mut bound.ingress)
        else {
            return false;
        };
        route.tx.covers(position)
            && Self::publisher_path_live(publisher_sends, route, now)
            && refresh.request(now, request)
    }

    pub(super) fn prepare_shard_fabric_refresh(
        &mut self,
        handle: FabricLocalBranch,
        destination: ParticipantHandle,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<ShardBinding> {
        let FabricOwner::Shard(binding) = self.fabric_handoff_owner(handle, destination)? else {
            return None;
        };
        let FabricIngress::Shard { last_refresh, .. } =
            &mut self.fabric_sources.get_mut(handle.source)?.ingress
        else {
            return None;
        };
        if last_refresh.is_some_and(|last| {
            now.saturating_duration_since(last) < crate::track::KEYFRAME_DEBOUNCE
        }) {
            return None;
        }
        *last_refresh = Some(now);
        Some(binding)
    }

    pub(super) fn request_shard_fabric_refresh(
        &mut self,
        binding: ShardBinding,
        now: pulsebeam_runtime::time::Instant,
        request: impl FnOnce(FabricSourceHandle, ParticipantHandle, u32) -> bool,
    ) -> bool {
        if binding.origin != self.shard_id || binding.destination.shard() == self.shard_id {
            return false;
        }
        let Some(source) =
            self.fabric_source_for_incarnation(binding.publication, binding.incarnation)
        else {
            return false;
        };
        let Some(FabricIngress::Publisher {
            participant,
            destinations,
            refresh: Some(refresh),
            ..
        }) = self
            .fabric_sources
            .get_mut(source)
            .map(|source| &mut source.ingress)
        else {
            return false;
        };
        if !destinations
            .iter()
            .any(|destination| destination.binding == binding)
        {
            return false;
        }
        refresh.request(now, |ssrc| request(source, *participant, ssrc))
    }

    pub(super) fn fabric_ingress_live(
        &self,
        handle: FabricLocalBranch,
        ingress: Option<NonZeroU64>,
    ) -> bool {
        self.fabric_sources
            .get(handle.source)
            .is_some_and(|source| source.ingress.authorizes(ingress))
    }

    pub(super) fn accept_local_data_expectation(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        admission: crate::participant::data_fabric::ReliableAdmission,
        now: pulsebeam_runtime::time::Instant,
        mut eligible: impl FnMut(FabricLocalBranch, ParticipantHandle) -> Option<NonZeroU64>,
    ) -> bool {
        if self.local_fabric_publication(source, publisher).is_none() {
            return false;
        }
        self.fabric_sources.get_mut(source).is_some_and(|bound| {
            if bound
                .reliable_installation
                .is_some_and(|installed| installed != admission.installation)
            {
                return false;
            }
            let FabricIngress::Publisher { destinations, .. } = &bound.ingress else {
                return false;
            };
            let Ok(accepted) = bound.fanout.accept_data_cohort(
                now,
                Some(admission.expires),
                admission.unit,
                |branch, dest| eligible(FabricLocalBranch { source, branch }, dest),
                || {
                    let mut count = 0usize;
                    let mut cohort = Vec::new();
                    for destination in destinations.iter().filter(|dest| {
                        dest.route.tx.is_live() && !dest.ordered_receivers.is_empty()
                    }) {
                        let remaining = MAX_BRANCHES.saturating_add(1).saturating_sub(count);
                        let receivers: Box<[_]> = destination
                            .ordered_receivers
                            .iter()
                            .take(remaining)
                            .copied()
                            .collect();
                        count = count.saturating_add(receivers.len());
                        cohort.push(crate::route::fabric::local::ShardDataCohort {
                            binding: destination.binding,
                            route: destination.route.clone(),
                            receivers,
                        });
                        if count > MAX_BRANCHES {
                            break;
                        }
                    }
                    cohort
                },
            ) else {
                return false;
            };
            if accepted == crate::route::fabric::local::ExpectationAdmission::First {
                bound.freeze_publisher_expectation(admission.unit, admission.expires, now);
                // The first expectation must enter the same mailbox before live fragments.
                bound
                    .fanout
                    .send_initial_shard_notices(now, admission.unit, |route, notice| {
                        #[cfg(feature = "sim")]
                        crate::sim_metrics::record_shard_data_notice(&notice);
                        Self::send_shard_data_notice(route, notice);
                    });
            }
            bound.reliable_installation = Some(admission.installation);
            true
        })
    }

    fn send_shard_data_notice(
        route: crate::shard_update::CompiledRoute,
        notice: crate::route::fabric::ShardDataExpectation,
    ) {
        crate::shard::worker::send_link_frame(
            &route.tx,
            route.clock_origin,
            ShardFrame::FabricDataExpectation(notice),
        );
    }

    pub(super) fn next_shard_data_notice(
        &self,
    ) -> Option<(
        pulsebeam_runtime::time::Instant,
        FabricSourceHandle,
        ParticipantHandle,
    )> {
        self.fabric_sources
            .iter()
            .filter_map(|(source, bound)| {
                let FabricOwner::Local(publisher) = bound.ingress.owner() else {
                    return None;
                };
                Some((bound.fanout.shard_notice_deadline()?, source, publisher))
            })
            .min()
    }

    pub(super) fn tick_shard_data_notice(
        &mut self,
        source: FabricSourceHandle,
        now: pulsebeam_runtime::time::Instant,
    ) {
        if let Some(bound) = self.fabric_sources.get_mut(source) {
            let FabricIngress::Publisher { destinations, .. } = &bound.ingress else {
                return;
            };
            bound.fanout.tick_shard_notice(now, |route, notice| {
                if destinations
                    .iter()
                    .any(|dest| dest.binding == notice.binding && dest.route == route)
                {
                    Self::send_shard_data_notice(route, notice);
                }
            });
        }
    }

    pub(super) fn shard_receipt_publisher(
        &self,
        receipt: crate::route::fabric::ShardDataReceipt,
    ) -> Option<(FabricSourceHandle, ParticipantHandle)> {
        let track = self.track_handle(receipt.binding.publication)?;
        let source = self.tracks.get(track)?.media.fabric()?;
        let FabricOwner::Local(publisher) = self.fabric_sources.get(source)?.ingress.owner() else {
            return None;
        };
        Some((source, publisher))
    }

    pub(super) fn resolve_shard_data_repair(
        &self,
        now: pulsebeam_runtime::time::Instant,
        repair: crate::route::fabric::ShardDataRepair,
    ) -> Option<(
        FabricSourceHandle,
        ParticipantHandle,
        pulsebeam_rtc::DataChannelInstallation,
        &crate::shard_update::CompiledRoute,
        u16,
    )> {
        let track = self.track_handle(repair.binding.publication)?;
        let source = self.tracks.get(track)?.media.fabric()?;
        let bound = self.fabric_sources.get(source)?;
        let FabricIngress::Publisher {
            participant,
            destinations,
            ..
        } = &bound.ingress
        else {
            return None;
        };
        let destination = destinations.iter().find(|dest| {
            dest.binding == repair.binding
                && dest.route.tx.is_live()
                && dest.ordered_receivers.contains(&repair.receiver)
        })?;
        let count = bound
            .fanout
            .shard_repair_window(now, &destination.route, repair)?;
        Some((
            source,
            *participant,
            bound.reliable_installation?,
            &destination.route,
            count,
        ))
    }

    pub(super) fn record_shard_data_receipt(
        &mut self,
        source: FabricSourceHandle,
        now: pulsebeam_runtime::time::Instant,
        receipt: crate::route::fabric::ShardDataReceipt,
    ) -> Option<crate::route::fabric::local::ReceiptAcceptance> {
        let bound = self.fabric_sources.get_mut(source)?;
        let FabricIngress::Publisher { destinations, .. } = &bound.ingress else {
            return None;
        };
        let destination = destinations.iter().find(|dest| {
            dest.binding == receipt.binding
                && dest.route.tx.is_live()
                && dest
                    .ordered_receivers
                    .iter()
                    .any(|receiver| receiver.branch == receipt.branch)
        })?;
        bound
            .fanout
            .record_shard_receipt(now, &destination.route, receipt)
    }

    pub(super) fn accept_shard_data_expectation(
        &mut self,
        notice: &crate::route::fabric::ShardDataExpectation,
        now: pulsebeam_runtime::time::Instant,
        mut receiver: impl FnMut(
            FabricLocalBranch,
            ParticipantHandle,
        ) -> Option<crate::route::fabric::ShardDataReceiver>,
    ) -> Option<crate::route::fabric::local::ExpectationAdmission> {
        if notice.receivers.is_empty() || notice.receivers.len() > MAX_BRANCHES {
            return None;
        }
        let source = self.shard_fabric_source(notice.binding)?;
        let bound = self.fabric_sources.get_mut(source)?;
        let FabricIngress::Shard { members, .. } = &bound.ingress else {
            return None;
        };
        bound
            .fanout
            .accept_data_expectation_until(
                now,
                Some(notice.expires),
                notice.unit,
                |branch, dest| {
                    if !members.contains(&dest) {
                        return None;
                    }
                    let current = receiver(FabricLocalBranch { source, branch }, dest)?;
                    notice
                        .receivers
                        .contains(&current)
                        .then_some(current.branch)
                },
            )
            .ok()
    }

    pub(super) fn accept_network_data_expectation(
        &mut self,
        notice: crate::route::fabric::NetworkDataExpectation,
        now: pulsebeam_runtime::time::Instant,
        mut eligible: impl FnMut(FabricLocalBranch, ParticipantHandle) -> Option<NonZeroU64>,
    ) -> bool {
        let parent = notice.parent;
        let Some(source) = self.network_fabric_source(parent.destination) else {
            return false;
        };
        let Some(bound) = self.fabric_sources.get_mut(source) else {
            return false;
        };
        let FabricIngress::Network {
            children,
            expected_parents,
            ..
        } = &mut bound.ingress
        else {
            return false;
        };
        if !children
            .iter()
            .any(|child| child.spec == parent && child.lifetime.is_live())
        {
            return false;
        }
        use crate::route::fabric::local::ExpectationAdmission;
        match bound.fanout.accept_data_expectation_until(
            now,
            Some(notice.expires),
            notice.unit,
            |branch, destination| eligible(FabricLocalBranch { source, branch }, destination),
        ) {
            Ok(ExpectationAdmission::First) => expected_parents
                .get_or_insert_with(Box::default)
                .admit(now, parent, notice.unit, notice.expires, || ()),
            Ok(ExpectationAdmission::Replay) => expected_parents
                .as_deref()
                .is_some_and(|units| units.contains(parent, notice.unit, now)),
            Err(_) => false,
        }
    }

    pub(super) fn next_publisher_data_expectation(
        &self,
    ) -> Option<(pulsebeam_runtime::time::Instant, FabricSourceHandle)> {
        self.fabric_sources
            .iter()
            .filter_map(|(source, bound)| {
                let FabricIngress::Publisher {
                    data_expectations, ..
                } = &bound.ingress
                else {
                    return None;
                };
                Some((data_expectations.as_deref()?.deadline()?, source))
            })
            .min()
    }

    pub(super) fn tick_publisher_data_expectation(
        &mut self,
        source: FabricSourceHandle,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<crate::route::fabric::PublisherExpectationStep> {
        let FabricIngress::Publisher {
            data_expectations,
            publisher_sends,
            ..
        } = &mut self.fabric_sources.get_mut(source)?.ingress
        else {
            return None;
        };
        data_expectations.as_deref_mut()?.tick(now, |route| {
            Self::publisher_path_live(publisher_sends, route, now)
        })
    }

    fn publisher_path_live(
        publisher_sends: &[PublisherHop],
        route: &CompiledPublisherSend,
        now: pulsebeam_runtime::time::Instant,
    ) -> bool {
        use crate::route::fabric::hop_expectations::Path;
        route.tx.is_live()
            && publisher_sends
                .iter()
                .any(|hop| hop.route.matches(route) && now < hop.route.expires_at)
    }

    pub(super) fn observe_publisher_data_expectation(
        &mut self,
        source: FabricSourceHandle,
        observation: crate::route::fabric::PublisherObservation,
        now: pulsebeam_runtime::time::Instant,
    ) -> bool {
        let Some(bound) = self.fabric_sources.get_mut(source) else {
            return false;
        };
        let FabricIngress::Publisher {
            data_expectations,
            publisher_sends,
            ..
        } = &mut bound.ingress
        else {
            return false;
        };
        Self::publisher_path_live(publisher_sends, &observation.route, now)
            && data_expectations.as_deref_mut().is_some_and(|tracker| {
                tracker.observe(
                    now,
                    &observation.route,
                    observation.unit,
                    observation.outcome,
                )
            })
    }

    pub(super) fn publisher_repair_window(
        &self,
        source: FabricSourceHandle,
        lookup: &crate::route::fabric::relay::PublisherRepairLookup,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<(pulsebeam_runtime::time::Instant, u16)> {
        let FabricIngress::Publisher {
            data_expectations,
            publisher_sends,
            ..
        } = &self.fabric_sources.get(source)?.ingress
        else {
            return None;
        };
        if !Self::publisher_path_live(publisher_sends, &lookup.route, now) {
            return None;
        }
        data_expectations
            .as_deref()?
            .confirmed_window(&lookup.route, lookup.request.fragment, now)
    }

    #[cfg(all(test, feature = "sim"))]
    pub(super) fn freeze_publisher_expectation_fixture(
        &mut self,
        source: FabricSourceHandle,
        unit: crate::route::fabric::local::DataExpectation,
        expires: pulsebeam_runtime::time::Instant,
        now: pulsebeam_runtime::time::Instant,
    ) {
        if let Some(bound) = self.fabric_sources.get_mut(source) {
            bound.freeze_publisher_expectation(unit, expires, now);
        }
    }

    pub(super) fn locate_publisher_repair(
        &self,
        source: FabricSourceHandle,
        lookup: &crate::route::fabric::relay::PublisherRepairLookup,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<crate::route::fabric::relay::LocatedRepair> {
        use crate::route::fabric::relay::{LocatedRepair, PeerRepairLookup};
        let (expires, count) = self.publisher_repair_window(source, lookup, now)?;
        let bound = self.fabric_sources.get(source)?;
        let (sequence, expires) = match &bound.ingress {
            FabricIngress::Publisher {
                data: Some(data), ..
            } => {
                let fragment = lookup.request.fragment;
                let (sequence, packet, expiry) =
                    data.cached_original(&bound.fanout, fragment.unit_seq, fragment.index, now)?;
                if !pulsebeam_proto::data_record::wire_identity(packet.payload())
                    .is_ok_and(|identity| identity.count == count)
                {
                    return None;
                }
                (sequence, expires.min(expiry))
            }
            _ => (
                bound
                    .fanout
                    .cached_data_position(lookup.request.fragment, count, None, now)?,
                expires,
            ),
        };
        Some(LocatedRepair {
            lookup: Box::new(PeerRepairLookup::Publisher(lookup.clone())),
            sequence,
            expires,
        })
    }

    fn media_repair_cache(
        &self,
        lookup: &crate::route::fabric::relay::MediaRepairLookup,
        publisher_source: Option<FabricSourceHandle>,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<(&OwnedPacket, pulsebeam_runtime::time::Instant)> {
        use crate::route::fabric::relay::MediaRepairParent;
        let (bound, ingress, expires) = match &lookup.parent {
            MediaRepairParent::Relay(route) => {
                let source = self.network_fabric_source(route.input.destination)?;
                let bound = self.fabric_sources.get(source)?;
                let FabricIngress::Network {
                    children, relays, ..
                } = &bound.ingress
                else {
                    return None;
                };
                if !route
                    .input
                    .window
                    .is_some_and(|window| window.contains(lookup.position))
                    || !route.tx.covers(lookup.position)
                    || !relays.has_live_path(route, now)
                {
                    return None;
                }
                let child = children
                    .iter()
                    .find(|child| child.spec == route.input && child.lifetime.is_live())?;
                (bound, Some(child.ingress), route.expires_at)
            }
            MediaRepairParent::Publisher(route) => {
                let bound = self.fabric_sources.get(publisher_source?)?;
                let FabricIngress::Publisher {
                    publisher_sends, ..
                } = &bound.ingress
                else {
                    return None;
                };
                if !route.tx.covers(lookup.position)
                    || !Self::publisher_path_live(publisher_sends, route, now)
                {
                    return None;
                }
                (bound, None, route.expires_at)
            }
        };
        let (packet, cached_until) =
            bound
                .fanout
                .cached_peer_media(lookup.position, ingress, now)?;
        Some((packet, expires.min(cached_until)))
    }

    pub(super) fn locate_media_repair(
        &self,
        lookup: crate::route::fabric::relay::MediaRepairLookup,
        publisher_source: Option<FabricSourceHandle>,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<crate::route::fabric::relay::LocatedRepair> {
        use crate::route::fabric::relay::{LocatedRepair, PeerRepairLookup};
        let (_, expires) = self.media_repair_cache(&lookup, publisher_source, now)?;
        Some(LocatedRepair {
            sequence: lookup.position,
            lookup: Box::new(PeerRepairLookup::Media(lookup)),
            expires,
        })
    }

    pub(super) fn send_peer_repair(
        &self,
        now: pulsebeam_runtime::time::Instant,
        handoff: crate::route::fabric::relay::PeerRepairHandoff,
        publisher_source: Option<FabricSourceHandle>,
        publisher_original: Option<(&OwnedPacket, pulsebeam_runtime::time::Instant)>,
    ) {
        use crate::route::fabric::relay::PeerRepairLookup;
        let located = &handoff.located;
        let (source, window, count, fragment) = match located.lookup.as_ref() {
            PeerRepairLookup::Media(lookup) => {
                if lookup.position == located.sequence
                    && let Some((cached, expires)) =
                        self.media_repair_cache(lookup, publisher_source, now)
                {
                    let expires = expires.min(located.expires);
                    if now < expires {
                        let _ = handoff
                            .tx
                            .try_send(now, expires, lookup.position, cached.clone());
                    }
                }
                return;
            }
            PeerRepairLookup::Publisher(lookup) => {
                let Some(source) = publisher_source else {
                    return;
                };
                let Some((window, count)) = self.publisher_repair_window(source, lookup, now)
                else {
                    return;
                };
                (source, window, count, lookup.request.fragment)
            }
        };
        let Some(bound) = self.fabric_sources.get(source) else {
            return;
        };
        let cached = match &bound.ingress {
            FabricIngress::Publisher { data: Some(_), .. } => {
                publisher_original.filter(|(packet, _)| {
                    pulsebeam_proto::data_record::wire_identity(packet.payload())
                        .is_ok_and(|identity| identity.count == count)
                })
            }
            _ => bound
                .fanout
                .cached_peer_data(located.sequence, fragment, count, None, now),
        };
        let Some((packet, cached_until)) = cached else {
            return;
        };
        let expires = located.expires.min(window).min(cached_until);
        if now >= expires {
            return;
        }
        let _ = handoff
            .tx
            .try_send(now, expires, located.sequence, packet.clone());
    }

    pub(super) fn has_live_network_ingress(&self, source: FabricSourceHandle) -> bool {
        self.fabric_sources.get(source).is_some_and(|bound| matches!(&bound.ingress, FabricIngress::Network { children, .. } if children.iter().any(|child| child.lifetime.is_live())))
    }

    pub(super) fn next_fabric_data_propagation(
        &self,
    ) -> Option<(
        pulsebeam_runtime::time::Instant,
        FabricSourceHandle,
        FabricOwner,
    )> {
        self.fabric_sources
            .iter()
            .filter_map(|(source, bound)| {
                Some((
                    bound.fanout.data_propagation_deadline()?,
                    source,
                    bound.ingress.owner(),
                ))
            })
            .min_by_key(|(deadline, source, _)| (*deadline, *source))
    }

    pub(super) fn next_fabric_delivery_report(
        &self,
    ) -> Option<(
        pulsebeam_runtime::time::Instant,
        FabricSourceHandle,
        FabricDeliveryTarget,
    )> {
        self.fabric_sources
            .iter()
            .filter_map(|(source, bound)| {
                let deadline = bound.fanout.data_delivery_deadline()?;
                let target = match bound.ingress.owner() {
                    FabricOwner::Local(publisher) => {
                        FabricDeliveryTarget::Publisher(publisher, bound.reliable_installation?)
                    }
                    FabricOwner::Shard(binding) => FabricDeliveryTarget::ShardShadow(binding),
                    FabricOwner::Network => return None,
                };
                Some((deadline, source, target))
            })
            .min_by_key(|(deadline, source, _)| (*deadline, *source))
    }

    pub(super) fn tick_fabric_delivery_report(
        &mut self,
        source: FabricSourceHandle,
        now: pulsebeam_runtime::time::Instant,
        write: impl FnOnce(
            crate::route::fabric::local::DeliveryReport,
        ) -> crate::route::fabric::local::ExpectationWrite,
    ) {
        if let Some(bound) = self.fabric_sources.get_mut(source) {
            bound.fanout.tick_data_delivery(now, write);
        }
    }

    pub(super) fn tick_fabric_data_propagation(
        &mut self,
        source: FabricSourceHandle,
        now: pulsebeam_runtime::time::Instant,
        write: impl FnOnce(
            FabricLocalBranch,
            ParticipantHandle,
            crate::route::fabric::local::DataExpectation,
            NonZeroU64,
        ) -> crate::route::fabric::local::ExpectationWrite,
    ) -> Option<crate::route::fabric::local::PropagationStep> {
        self.fabric_sources
            .get_mut(source)?
            .fanout
            .tick_data_propagation(now, |branch, destination, unit, incarnation| {
                write(
                    FabricLocalBranch { source, branch },
                    destination,
                    unit,
                    incarnation,
                )
            })
    }

    pub(super) fn settle_fabric_data_propagation(
        &mut self,
        destination: ParticipantHandle,
        branch: FabricLocalBranch,
        unit: crate::route::fabric::local::DataExpectation,
        outcome: crate::route::fabric::local::PropagationOutcome,
        now: pulsebeam_runtime::time::Instant,
    ) -> bool {
        self.fabric_sources
            .get_mut(branch.source)
            .is_some_and(|source| {
                source.fanout.settle_data_propagation(
                    now,
                    unit,
                    branch.branch,
                    destination,
                    outcome,
                )
            })
    }

    pub(super) fn record_fabric_assembly_receipt(
        &mut self,
        destination: ParticipantHandle,
        lease: crate::participant::data_fabric::SubscriberLease,
        unit: crate::route::fabric::local::DataExpectation,
        now: pulsebeam_runtime::time::Instant,
    ) -> Option<crate::route::fabric::local::ReceiptAcceptance> {
        let branch = lease.branch;
        self.fabric_sources
            .get_mut(branch.source)?
            .fanout
            .record_data_receipt(now, unit, branch.branch, destination, lease.endpoint.branch)
    }

    pub(super) fn reliable_data_installation(
        &self,
        source: FabricSourceHandle,
    ) -> Option<pulsebeam_rtc::DataChannelInstallation> {
        self.fabric_sources.get(source)?.reliable_installation
    }

    pub(super) fn repair_fanout(
        &mut self,
        source: FabricSourceHandle,
    ) -> Option<&mut SourceFanout> {
        let bound = self.fabric_sources.get_mut(source)?;
        match bound.ingress.owner() {
            FabricOwner::Local(_) if bound.reliable_installation.is_some() => {
                Some(&mut bound.fanout)
            }
            FabricOwner::Shard(_) => Some(&mut bound.fanout),
            _ => None,
        }
    }

    pub(super) fn next_network_repair_expiry(
        &self,
    ) -> Option<(
        pulsebeam_runtime::time::Instant,
        FabricSourceHandle,
        RepairTarget,
        RepairRequest,
    )> {
        self.fabric_sources
            .iter()
            .filter_map(|(source, bound)| {
                if !matches!(bound.ingress, FabricIngress::Network { .. }) {
                    return None;
                }
                let (expires, target, request) = bound.fanout.next_data_repair_expiry()?;
                Some((expires, source, target, request))
            })
            .min_by_key(|(expires, _, _, request)| (*expires, request.id))
    }

    pub(super) fn next_fabric_data_repair(
        &self,
    ) -> Option<(
        FabricSourceHandle,
        FabricRepairSource,
        crate::route::fabric::local::RepairWork,
    )> {
        self.fabric_sources
            .iter()
            .filter_map(|(source, bound)| {
                let owner = match bound.ingress.owner() {
                    FabricOwner::Local(publisher) => {
                        FabricRepairSource::Publisher(publisher, bound.reliable_installation?)
                    }
                    FabricOwner::Shard(binding) => FabricRepairSource::Shard(binding),
                    FabricOwner::Network => return None,
                };
                Some((source, owner, bound.fanout.next_data_repair()?))
            })
            .min_by_key(|(source, _, work)| (work.due, *source))
    }

    pub(super) fn admit_fabric_data(
        &mut self,
        handle: FabricLocalBranch,
        destination: ParticipantHandle,
        now: pulsebeam_runtime::time::Instant,
        current_branch: Option<NonZeroU64>,
        write: impl FnOnce(
            u64,
            PacketView<'_>,
            Option<crate::route::fabric::local::RepairRequest>,
        ) -> bool,
    ) -> Option<crate::route::fabric::local::DataHandoff> {
        let source = self.fabric_sources.get_mut(handle.source)?;
        if let FabricIngress::Network {
            children, repairs, ..
        } = &mut source.ingress
        {
            repairs.retain(|repair| {
                let current = repair.target.branch != handle.branch
                    || repair.target.destination != destination
                    || current_branch == Some(repair.target.incarnation);
                if current && repair.parent_at(children, now).is_some() {
                    true
                } else {
                    source
                        .fanout
                        .cancel_data_repair(repair.target, repair.child.demand.request);
                    false
                }
            });
        }
        // The WAN ingress has its own route-scoped expectation and repair binding.
        let cohort_incarnation = match source.ingress {
            FabricIngress::Network { .. } => None,
            FabricIngress::Publisher { .. } | FabricIngress::Shard { .. } => current_branch,
        };
        let handoff = source.fanout.admit_data(
            now,
            handle.branch,
            destination,
            cohort_incarnation,
            |ingress| source.ingress.authorizes(ingress),
            write,
        )?;
        if let Some(completed) = handoff.completed
            && let FabricIngress::Network { repairs, .. } = &mut source.ingress
            && let Some(index) = repairs.iter().position(|repair| {
                repair.target.branch == handle.branch
                    && repair.target.destination == destination
                    && repair.child.demand.request == completed
            })
        {
            let mut repair = repairs.swap_remove(index);
            repair
                .ingress
                .handoff(repair.child.route, repair.child.demand, now, || true);
        }
        Some(handoff)
    }

    pub(super) fn admit_fabric<T, E>(
        &mut self,
        handle: FabricLocalBranch,
        destination: ParticipantHandle,
        now: pulsebeam_runtime::time::Instant,
        write: impl FnOnce(
            u64,
            PacketView<'_>,
            crate::route::fabric::local::OutputEvidence,
        ) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        let Some(source) = self.fabric_sources.get_mut(handle.source) else {
            return Ok(None);
        };
        source.fanout.admit_with_evidence(
            now,
            handle.branch,
            destination,
            |ingress| source.ingress.authorizes(ingress),
            write,
        )
    }

    pub(crate) fn install_track(
        &mut self,
        publication_id: crate::entity::PublicationKey,
        _descriptor: Option<&crate::shard_update::TrackDescriptor>,
        origin: Option<ParticipantHandle>,
        publisher: Option<ParticipantHandle>,
    ) -> TrackHandle {
        if let Some(handle) = self.track_handles.get(&publication_id).copied() {
            let Some(runtime) = self.tracks.get_mut(handle) else {
                debug_assert!(false, "a track identity must resolve to its shard handle");
                return handle;
            };
            debug_assert_eq!(
                runtime.origin, origin,
                "a runtime handle cannot change its publisher binding"
            );
            if runtime.origin.or(runtime.publisher) != origin.or(publisher) {
                for source in std::mem::take(&mut runtime.media.0) {
                    self.fabric_sources.remove(source);
                }
            }
            runtime.origin = origin;
            runtime.publisher = publisher;
            return handle;
        }
        let handle = self.tracks.insert(TrackRuntime {
            id: publication_id,
            media: TrackSources::default(),
            origin,
            publisher,
        });
        self.track_handles.insert(publication_id, handle);
        handle
    }

    pub fn route_reverse(&self, packet: ReversePacket, plan: &InstalledTrackPlan) {
        let Some(target) = &plan.reverse_route else {
            // Feedback can arrive before the peer provisions its reverse leg.
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
    fn publication_id(
        kind: crate::entity::TrackKind,
        label: &str,
    ) -> crate::entity::PublicationKey {
        crate::entity::ConnectionId::new().fixture_publication(kind, label)
    }

    #[test]
    fn ordered_readiness_requires_the_current_compiled_destination_and_incarnation() {
        use crate::route::{
            RouteId,
            fabric::{ShardDataReady, ShardDataReceiver},
        };
        use crate::shard_update::CompiledRoute;
        use pulsebeam_link::{Destination, LinkControl, LinkTime};

        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let origin = ShardId::new(0);
        let address = NodeRouteAddress::new(RouteId::new(ShardId::new(1), 1), 1);
        let mut runtime = ShardRuntime::new(origin);
        let mut participants = SlotMap::<ParticipantHandle, ()>::with_key();
        let publisher = participants.insert(());
        let publication = publication_id(crate::entity::TrackKind::Data, "ordered");
        let track = runtime.install_track(publication, None, Some(publisher), None);
        let source = runtime
            .install_fabric_source(track, publisher, Emission::DATA, None, None)
            .unwrap();
        let binding = ShardBinding {
            destination: address,
            publication,
            origin,
            incarnation: runtime.fabric_incarnation(source).unwrap(),
        };
        let current = ShardDataReady {
            binding,
            reference: 1,
            receiver: ShardDataReceiver {
                connection: crate::entity::ConnectionId::new(),
                branch: NonZeroU64::MIN,
            },
        };
        assert_eq!(runtime.set_shard_data_ready(current, true), None);
        let mut links = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
        let (port, mut rx) = links.create_port().unwrap();
        let (_, tx) = links
            .bind_send(Destination::Local {
                port,
                tag: crate::link::delivery_tag(address),
            })
            .unwrap();
        let plan = InstalledTrackPlan {
            remote: vec![CompiledRoute {
                address,
                tx,
                clock_origin: pulsebeam_runtime::time::Instant::now(),
            }],
            ..Default::default()
        };
        assert_eq!(
            runtime.bind_shard_fabric_destination(binding, &plan),
            Some(())
        );
        let wrong_source = ShardDataReady {
            binding: ShardBinding {
                incarnation: NonZeroU64::MAX,
                ..binding
            },
            ..current
        };
        assert_eq!(runtime.set_shard_data_ready(wrong_source, true), None);
        assert_eq!(runtime.set_shard_data_ready(current, true), Some(()));
        assert_eq!(runtime.set_shard_data_ready(current, true), Some(()));
        use crate::route::fabric::{
            ShardDataReceipt,
            local::{
                DataExpectation, DeliveryOutcome, ExpectationAdmission, ExpectationWrite,
                ReceiptAcceptance, ShardDataCohort,
            },
        };
        use pulsebeam_runtime::time::Duration;
        let route = &plan.remote[0];
        let now = route.clock_origin;
        let expires = now + Duration::from_millis(250);
        let unit = DataExpectation {
            unit_seq: 1,
            fragment_count: 1,
        };
        let silent = ShardDataReceiver {
            connection: crate::entity::ConnectionId::new(),
            branch: NonZeroU64::new(2).unwrap(),
        };
        let fanout = &mut runtime.fabric_sources.get_mut(source).unwrap().fanout;
        assert_eq!(
            fanout.accept_data_cohort(
                now,
                Some(expires),
                unit,
                |_, _| None,
                || vec![ShardDataCohort {
                    binding,
                    route: route.clone(),
                    receivers: Box::new([current.receiver, silent]),
                }]
            ),
            Ok(ExpectationAdmission::First)
        );
        assert!(fanout.permits_shard_data(now, unit, binding, route));
        runtime.tick_shard_data_notice(source, now);
        let receipt = ShardDataReceipt {
            binding,
            unit,
            branch: current.receiver.branch,
        };
        let wrong = ShardDataReceipt {
            unit: DataExpectation {
                fragment_count: 2,
                ..unit
            },
            ..receipt
        };
        assert_eq!(runtime.record_shard_data_receipt(source, now, wrong), None);
        assert_eq!(
            runtime.record_shard_data_receipt(source, now, receipt),
            Some(ReceiptAcceptance::First)
        );
        runtime.tick_fabric_delivery_report(source, now, |report| {
            assert_eq!(
                (report.unit, report.branch_incarnation, report.outcome),
                (unit, receipt.branch, DeliveryOutcome::Confirmed)
            );
            ExpectationWrite::Queued
        });
        assert_eq!(
            runtime.record_shard_data_receipt(source, now, receipt),
            Some(ReceiptAcceptance::Replay)
        );
        let successor = ShardDataReady {
            receiver: ShardDataReceiver {
                branch: NonZeroU64::MAX,
                ..current.receiver
            },
            ..current
        };
        assert_eq!(runtime.set_shard_data_ready(successor, true), Some(()));
        assert_eq!(runtime.set_shard_data_ready(current, false), None);
        assert_eq!(runtime.set_shard_data_ready(current, true), None);
        runtime.tick_shard_data_notice(source, now + Duration::from_millis(100));
        let mut sent = Vec::new();
        assert_eq!(rx.try_recv_batch(&mut sent, 8), 2);
        for received in sent {
            let ShardFrame::FabricDataExpectation(notice) = received.packet else {
                panic!("expected notice")
            };
            assert_eq!(
                (notice.binding, notice.unit, notice.expires),
                (binding, unit, expires)
            );
            assert_eq!(notice.receivers.as_ref(), &[current.receiver, silent]);
        }
        assert_eq!(
            runtime.record_shard_data_receipt(
                source,
                now,
                ShardDataReceipt {
                    branch: successor.receiver.branch,
                    ..receipt
                }
            ),
            None
        );
        runtime.tick_shard_data_notice(source, expires);
        assert!(runtime.next_shard_data_notice().is_none());
        runtime.tick_fabric_delivery_report(source, expires, |report| {
            assert_eq!(
                (report.unit, report.branch_incarnation, report.outcome),
                (unit, silent.branch, DeliveryOutcome::Unknown)
            );
            ExpectationWrite::Queued
        });
        assert!(
            !runtime.fabric_sources[source]
                .fanout
                .permits_shard_data(expires, unit, binding, route)
        );
        assert_eq!(runtime.set_shard_data_ready(successor, false), Some(()));
        assert_eq!(runtime.set_shard_data_ready(successor, false), None);
        runtime.reconcile_fabric_plan(track, None);
        assert_eq!(runtime.set_shard_data_ready(successor, true), None);
    }

    #[test]
    fn reinserting_a_live_track_runtime_preserves_its_forwarding_state() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let mut participant_keys = SlotMap::<ParticipantHandle, ()>::with_key();
        let origin_key = participant_keys.insert(());
        let descriptor = crate::shard_update::TrackDescriptor {
            origin: crate::entity::ConnectionId::new(),
            kind: crate::entity::TrackKind::Video,
            encodings: vec![Some(crate::media::Rid::from("q")).into()],
        };
        let publication_id = publication_id(crate::entity::TrackKind::Video, "video");
        let key = runtime.install_track(publication_id, Some(&descriptor), Some(origin_key), None);
        let origin_before = runtime.tracks.get(key).unwrap().origin;
        let descriptor = crate::shard_update::TrackDescriptor {
            origin: descriptor.origin,
            kind: crate::entity::TrackKind::Video,
            encodings: vec![Some(crate::media::Rid::from("f")).into()],
        };
        assert_eq!(
            runtime.install_track(publication_id, Some(&descriptor), Some(origin_key), None),
            key
        );

        assert_eq!(runtime.tracks.get(key).unwrap().origin, origin_before);
        assert!(runtime.tracks[key].media.sources().is_empty());
    }

    #[test]
    fn publisher_fabric_sources_require_distinct_dd_encodings_and_retire_individually() {
        use crate::route::fabric::RtpSourceEncoding;
        use pulsebeam_core::fabric::{ExtensionKind, Profile};
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let publisher = SlotMap::<ParticipantHandle, ()>::with_key().insert(());
        let publication = publication_id(crate::entity::TrackKind::Video, "video");
        let track = runtime.install_track(publication, None, Some(publisher), None);
        let dd = Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
        let bare = Emission::new(Profile::H264, &[]).unwrap();
        let encoding = |ssrc: u32, rid| RtpSourceEncoding {
            primary: ssrc.into(),
            rid: Some(crate::media::Rid::new(rid).unwrap()),
            payload_type: 96_u8.into(),
        };
        let install = |runtime: &mut ShardRuntime, emission, encoding: RtpSourceEncoding| {
            runtime.install_fabric_source(
                track,
                publisher,
                emission,
                std::num::NonZeroU32::new(*encoding.primary),
                Some(encoding),
            )
        };
        let first = install(&mut runtime, dd, encoding(1, "q")).unwrap();
        assert!(install(&mut runtime, dd, encoding(1, "h")).is_none());
        assert!(install(&mut runtime, dd, encoding(2, "q")).is_none());
        assert!(install(&mut runtime, bare, encoding(2, "h")).is_none());
        let second = install(&mut runtime, dd, encoding(2, "h")).unwrap();
        let third = install(&mut runtime, dd, encoding(3, "f")).unwrap();
        assert!(install(&mut runtime, dd, encoding(4, "x")).is_none());
        assert!(runtime.fabric_source_for_track(track).is_none());
        let select = |ssrc: u32| crate::track::SourceEncoding::Ssrc(ssrc.into());
        assert_eq!(
            runtime.fabric_source_for_encoding(track, select(2)),
            Some(second)
        );
        assert!(
            runtime
                .fabric_source_for_encoding(track, select(4))
                .is_none()
        );
        let old = runtime.fabric_incarnation(second).unwrap();
        let address = NodeRouteAddress::new(crate::route::RouteId::new(ShardId::new(1), 1), 1);
        let binding = ShardBinding {
            destination: address,
            publication,
            origin: ShardId::new(0),
            incarnation: old,
        };
        let mut links =
            pulsebeam_link::LinkControl::new(|| pulsebeam_link::LinkTime::from_nanos(0)).unwrap();
        let (port, _rx) = links.create_port().unwrap();
        let (_, tx) = links
            .bind_send(pulsebeam_link::Destination::Local {
                port,
                tag: crate::link::delivery_tag(address),
            })
            .unwrap();
        let now = pulsebeam_runtime::time::Instant::now();
        let plan = InstalledTrackPlan {
            remote: vec![crate::shard_update::CompiledRoute {
                address,
                tx,
                clock_origin: now,
            }],
            ..Default::default()
        };
        assert_eq!(
            runtime.bind_shard_fabric_destination(binding, &plan),
            Some(())
        );
        assert!(
            runtime.request_shard_fabric_refresh(binding, now, |source, owner, ssrc| {
                assert_eq!((source, owner, ssrc), (second, publisher, 2));
                true
            })
        );
        let unbound = ShardBinding {
            incarnation: runtime.fabric_incarnation(first).unwrap(),
            ..binding
        };
        assert!(
            !runtime
                .request_shard_fabric_refresh(unbound, now, |_, _, _| panic!("unbound encoding"))
        );
        assert!(runtime.retire_fabric_source(second));
        let replacement = install(&mut runtime, dd, encoding(2, "h")).unwrap();
        assert!(
            !runtime
                .request_shard_fabric_refresh(binding, now, |_, _, _| panic!("retired encoding"))
        );
        assert!(runtime.fabric_incarnation(second).is_none());
        assert!(runtime.fabric_incarnation(replacement).unwrap() > old);
        assert_eq!(
            runtime.fabric_source_for_encoding(track, select(2)),
            Some(replacement)
        );
        runtime.retire_track(publication);
        for source in [first, third, replacement] {
            assert!(runtime.fabric_incarnation(source).is_none());
        }
    }

    #[test]
    fn retiring_shard_route_removes_every_imported_encoding() {
        use crate::route::{RouteId, fabric::RtpSourceEncoding};
        use pulsebeam_core::fabric::{ExtensionKind, Profile};
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let shard = ShardId::new(0);
        let mut runtime = ShardRuntime::new(shard);
        let receiver = SlotMap::<ParticipantHandle, ()>::with_key().insert(());
        let publication = publication_id(crate::entity::TrackKind::Video, "imported");
        let track = runtime.install_track(publication, None, None, None);
        let route = NodeRouteAddress::new(RouteId::new(shard, 2), 1);
        runtime
            .routes
            .install_action(route, RouteAction::Forward { target: track });
        let plan = InstalledTrackPlan {
            local: vec![receiver],
            ..Default::default()
        };
        let emission =
            Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
        let install = |runtime: &mut ShardRuntime, number: u32| {
            let binding = ShardBinding {
                destination: route,
                publication,
                origin: ShardId::new(1),
                incarnation: NonZeroU64::new(u64::from(number)).unwrap(),
            };
            let encoding = RtpSourceEncoding {
                primary: number.into(),
                rid: None,
                payload_type: 96_u8.into(),
            };
            let source = runtime
                .install_shard_fabric_source(binding, Some(encoding), emission, &plan)
                .unwrap();
            assert_eq!(runtime.shard_fabric_source(binding), Some(source));
            (binding, source)
        };
        let sources = [install(&mut runtime, 1), install(&mut runtime, 2)];
        assert!(runtime.fabric_source_for_track(track).is_none());
        assert!(runtime.routes.retire(route));
        runtime.retire_fabric_route(route);
        for (binding, source) in sources {
            assert!(runtime.shard_fabric_source(binding).is_none());
            assert!(runtime.fabric_incarnation(source).is_none());
        }
        assert!(runtime.tracks[track].media.sources().is_empty());
    }

    #[test]
    fn reinserting_a_fenced_track_cannot_revive_its_retired_source() {
        let _rng = pulsebeam_runtime::rand::seeded_rng(1);
        let mut runtime = ShardRuntime::new(ShardId::new(0));
        let publisher = SlotMap::<ParticipantHandle, ()>::with_key().insert(());
        let descriptor = crate::shard_update::TrackDescriptor {
            origin: crate::entity::ConnectionId::new(),
            kind: crate::entity::TrackKind::Video,
            encodings: Vec::new(),
        };
        let publication_id = publication_id(crate::entity::TrackKind::Video, "video");
        let key = runtime.install_track(publication_id, Some(&descriptor), Some(publisher), None);
        assert!(runtime.tracks[key].media.sources().is_empty());
        let emission = Emission::new(pulsebeam_core::fabric::Profile::H264, &[]).unwrap();
        let source = runtime
            .install_fabric_source(key, publisher, emission, None, None)
            .unwrap();
        assert!(runtime.retire_fabric_source(source));
        assert_eq!(
            runtime.install_track(publication_id, Some(&descriptor), Some(publisher), None),
            key
        );

        assert!(runtime.tracks[key].media.sources().is_empty());
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
        ShardRuntime::new(ShardId::new(0)).route_reverse(
            ReversePacket::keyframe(None, crate::media::KeyframeRequestKind::Pli),
            &plan,
        );

        let mut frames = Vec::new();
        assert_eq!(rx.try_recv_batch(&mut frames, 1), 1);
        assert!(
            matches!(&frames[0].packet, ShardFrame::Reverse { address, .. } if *address == target)
        );
    }
}
