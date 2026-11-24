//! Shard-owned Fabric fanout for endpoint, shard and authenticated relay input.
//! Local branch queues carry positions into one immutable packet store.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU64;

use pulsebeam_core::{
    dd::evidence::{
        Decision, DecodeTarget, Enqueue, FilteredBranch, FrameStatus, Frames, Qualified,
        SourceContext,
    },
    fabric::{
        ExtensionKind, FabricPacket, MAJOR, Profile, REGISTRY_REVISION, WireError,
        media_source::{IdentityAdmission, MediaIdentity, Rejected},
    },
};
use pulsebeam_runtime::time::{Duration, Instant};
use slotmap::SlotMap;

use super::{LiveAdmissions, LiveOffer, POSITIONS, ShardOffer};
use crate::keys::ParticipantHandle;

mod reliable;
mod repair;
pub(crate) use reliable::{
    DataExpectation, DeliveryOutcome, DeliveryReport, ExpectationAdmission, ExpectationError,
    ExpectationWrite, PropagationOutcome, PropagationStep, ReceiptAcceptance, ShardDataCohort,
};
pub(crate) use repair::{RepairAdmission, RepairFragment, RepairRequest};

slotmap::new_key_type! {
    pub struct FabricBranchHandle;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepairTarget {
    pub(crate) branch: FabricBranchHandle,
    pub(crate) destination: ParticipantHandle,
    pub(crate) incarnation: NonZeroU64,
}

#[derive(Clone, Copy)]
pub(crate) struct RepairWork {
    pub(crate) due: Instant,
    pub(crate) expires: Instant,
    pub(crate) target: RepairTarget,
    pub(crate) request: RepairRequest,
    pub(crate) queued: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepairQueue {
    Queued,
    Backpressured,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DataHandoff {
    pub(crate) written: bool,
    pub(crate) completed: Option<RepairRequest>,
}

pub type OwnedPacket = FabricPacket<Box<[u8]>>;
pub type PacketView<'a> = FabricPacket<&'a [u8], &'a [u8]>;

pub struct OutputEvidence {
    pub mode: pulsebeam_core::dd::output::Mode,
    pub entry: bool,
    pub reset_after: Option<u64>,
    pub dd: Option<(u64, Qualified)>,
    pub(crate) output_position: u64,
    pub(crate) ingress: Option<NonZeroU64>,
    pub(crate) capture: Option<pulsebeam_clock::deadline::Timing>,
}

#[derive(Clone, Copy)]
struct FrameReservation {
    number: u64,
    structure_sequence: u64,
}

pub(crate) const MAX_BRANCHES: usize = 64;
const QUEUE_PACKETS: usize = 32;
const MAX_AGE: Duration = Duration::from_millis(120);
const DD_REORDER_WAIT: Duration = Duration::from_millis(40);
const DD_REORDER_POSITIONS: u64 = 16;

/// Fixed at source installation. `extensions` describes the full possible
/// emission set even when a particular packet carries none of those hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Emission {
    profile: Profile,
    mask: u16,
    minimum_revision: u8,
}

impl Emission {
    pub(crate) const DATA: Self = Self {
        profile: Profile::Data,
        mask: 0,
        minimum_revision: 0,
    };

    pub fn new(profile: Profile, extensions: &[ExtensionKind]) -> Option<Self> {
        let mut mask = 0;
        let mut minimum_revision = 0;
        for &kind in extensions {
            if !kind.is_permitted_on(profile) {
                return None;
            }
            mask |= bit(kind);
            minimum_revision = minimum_revision.max(kind.minimum_revision());
        }
        Some(Self {
            profile,
            mask,
            minimum_revision,
        })
    }

    pub(crate) const fn profile(self) -> Profile {
        self.profile
    }

    pub(crate) fn includes(self, kind: ExtensionKind) -> bool {
        self.mask & bit(kind) != 0
    }

    pub(crate) fn selects_dd_encoding(self) -> bool {
        self.profile == Profile::H264 && self.includes(ExtensionKind::DependencyDescriptor)
    }

    pub(crate) const fn wire_profile(self) -> (u8, u16) {
        (self.profile as u8, self.mask)
    }

    pub(crate) fn from_wire_profile(pt: u8, mask: u16, major: u8, revision: u8) -> Option<Self> {
        let profile = Profile::from_pt(pt).ok()?;
        let mut minimum_revision = 0;
        for id in 0_u8..16 {
            if mask & (1 << id) == 0 {
                continue;
            }
            let kind = ExtensionKind::from_id(id).ok()?;
            if !kind.is_permitted_on(profile) {
                return None;
            }
            minimum_revision = minimum_revision.max(kind.minimum_revision());
        }
        let emission = Self {
            profile,
            mask,
            minimum_revision,
        };
        emission.fits_registry(major, revision).then_some(emission)
    }

    pub(crate) fn fits_registry(self, major: u8, revision: u8) -> bool {
        major == MAJOR && (self.minimum_revision..=REGISTRY_REVISION).contains(&revision)
    }

    pub(crate) fn accepts(self, packet: &OwnedPacket) -> bool {
        packet.header().profile == self.profile
            && packet
                .extensions()
                .all(|ext| self.mask & bit(ext.kind) != 0)
    }

    fn fits(self, receiver: Self) -> bool {
        self.profile == receiver.profile && self.mask & !receiver.mask == 0
    }
}

fn bit(kind: ExtensionKind) -> u16 {
    1 << kind as u8
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    Binding,
    InvalidDescriptor,
    Stale,
    Conflict,
    Fenced,
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct Offers {
    pub enqueued: usize,
    pub full: usize,
    pub duplicates: usize,
    pub omitted: usize,
    pub suspended: usize,
    pub awaiting_entry: usize,
    pub(crate) dd_structure: Option<crate::rtp::normalize::StreamFacts>,
    pub(crate) dd_sample: DecodeTargetSample,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DecodeTargetSample {
    #[default]
    None,
    Unknown,
    Bytes([u64; crate::rtp::monitor::MAX_LADDER_TARGETS]),
}

impl DecodeTargetSample {
    fn record(&mut self, qualified: Option<&Qualified>, payload_bytes: usize) {
        if *self == Self::Unknown {
            return;
        }
        let Some(qualified) = qualified else {
            *self = Self::Unknown;
            return;
        };
        let payload_bytes = u64::try_from(payload_bytes).unwrap_or(u64::MAX);
        let mut bytes = match *self {
            Self::Bytes(bytes) => bytes,
            _ => [0; crate::rtp::monitor::MAX_LADDER_TARGETS],
        };
        for (target, total) in bytes.iter_mut().enumerate() {
            if (qualified.active_targets & (1 << target)) != 0
                && qualified.descriptor.is_in_decode_target(target)
            {
                *total = total.saturating_add(payload_bytes);
            }
        }
        *self = Self::Bytes(bytes);
    }
}

impl Offers {
    pub(crate) fn record(&mut self, outcome: LiveOffer) {
        let count = match outcome {
            LiveOffer::Enqueued => &mut self.enqueued,
            LiveOffer::Refused => &mut self.full,
            LiveOffer::Duplicate => &mut self.duplicates,
            LiveOffer::Stale | LiveOffer::AheadOfSource => return,
        };
        *count = count.saturating_add(1);
    }
}

/// One installed source incarnation. The owning router must discard this
/// object on source retirement; destination retirement removes its branch.
pub struct SourceFanout {
    emission: Emission,
    high: Option<u64>,
    fates: BTreeMap<u64, Frozen>,
    retired_before: u64,
    branches: SlotMap<FabricBranchHandle, Branch>,
    fenced: bool,
    evidence: Option<Box<SourceEvidence>>,
    media_identity: Option<Box<MediaIdentity>>,
    expectations: Option<Box<reliable::Expectations>>,
}

#[derive(Default)]
struct SourceEvidence {
    context: SourceContext,
    frames: Frames,
    ordered_through: Option<u64>,
}

impl SourceEvidence {
    fn qualify_position(
        &mut self,
        sequence: u64,
        image: &OwnedPacket,
        identity: Option<&MediaIdentity>,
    ) -> Result<Option<(pulsebeam_core::dd::evidence::Qualified, FrameStatus)>, Rejection> {
        if let Some(identity) = identity {
            self.context
                .bridge_padding_before(sequence, |positions| identity.covers_padding(positions));
        }
        let raw = image
            .extensions()
            .find(|extension| extension.kind == ExtensionKind::DependencyDescriptor)
            .map(|extension| extension.value);
        let Ok(qualified) = self.context.observe(sequence, raw) else {
            // Invalid media cannot update the template reader or bridge the gap.
            let _ = self.context.observe(sequence, None);
            return Err(Rejection::InvalidDescriptor);
        };
        Ok(qualified.map(|qualified| {
            let frame = self.frames.observe(sequence, &qualified);
            (qualified, frame)
        }))
    }
}

fn dd_structure(
    (qualified, frame): &(Qualified, FrameStatus),
) -> Option<crate::rtp::normalize::StreamFacts> {
    let descriptor = &qualified.descriptor;
    let structure = descriptor.attached_structure.as_ref()?;
    (matches!(frame, FrameStatus::Resolved(_))
        && descriptor.start_of_frame
        && descriptor.frame_dependencies.frame_diffs.is_empty()
        && structure.spatial_layer_count() == 1)
        .then(|| crate::rtp::normalize::StreamFacts::from_structure(structure))
}

struct Frozen {
    packet: OwnedPacket,
    capture: Option<pulsebeam_clock::deadline::Timing>,
    live_arrival: Option<Instant>,
    ready_at: Option<std::time::Instant>,
    live_ingress: Option<NonZeroU64>,
    endpoint_original: bool,
}

impl Frozen {
    fn live(&self, now: Instant) -> bool {
        self.live_arrival.is_some_and(|arrival| {
            now >= arrival && now.saturating_duration_since(arrival) < MAX_AGE
        })
    }
}

/// A live position validated by the installed source owner. Every branch
/// admits the same full position and original lifetime, even on retry.
#[derive(Clone, Copy)]
pub(crate) struct LiveWork {
    high: u64,
    sequence: u64,
    arrival: Instant,
    expires_at: Instant,
    hops: u8,
}

impl LiveWork {
    fn new(high: u64, sequence: u64, arrival: Instant, hops: u8) -> Option<Self> {
        Some(Self {
            high,
            sequence,
            arrival,
            expires_at: arrival.checked_add(MAX_AGE)?,
            hops,
        })
    }

    pub(crate) fn sequence(self) -> u64 {
        self.sequence
    }

    pub(crate) fn arrival(self) -> Instant {
        self.arrival
    }

    pub(crate) fn expires_at(self) -> Instant {
        self.expires_at
    }

    pub(crate) fn enqueue(
        self,
        now: Instant,
        admissions: &mut LiveAdmissions,
        enqueue: impl FnOnce() -> bool,
    ) -> LiveOffer {
        if now < self.arrival || now >= self.expires_at {
            return LiveOffer::Stale;
        }
        admissions.offer(self.high, self.sequence, enqueue)
    }
}

pub(crate) struct Transit<'a> {
    pub work: LiveWork,
    pub packet: PacketView<'a>,
    pub ready_at: Option<std::time::Instant>,
}

struct Branch {
    destination: ParticipantHandle,
    admissions: LiveAdmissions,
    repairs: Option<Box<repair::Repairs>>,
    queue: VecDeque<Queued>,
    filter: Option<Box<FilteredBranch>>,
    dd_reset: Option<u64>,
    filtered_structure: Option<u64>,
    omitted: u64,
    last_omitted: Option<u64>,
}

struct Queued {
    sequence: u64,
    output_position: u64,
    queued_at: Instant,
    repair_only: Option<RepairRequest>,
    hops: u8,
    ingress: Option<NonZeroU64>,
    selection: pulsebeam_core::dd::output::Mode,
    entry: bool,
    frame: Option<FrameReservation>,
}

impl SourceFanout {
    pub(crate) fn emission(&self) -> Emission {
        self.emission
    }

    pub fn new(emission: Emission) -> Self {
        Self {
            emission,
            high: None,
            fates: BTreeMap::new(),
            retired_before: 0,
            media_identity: None,
            branches: SlotMap::with_key(),
            fenced: false,
            evidence: emission.selects_dd_encoding().then(Box::default),
            expectations: None,
        }
    }

    pub fn install_branch(
        &mut self,
        destination: ParticipantHandle,
        receiver: Emission,
    ) -> Option<FabricBranchHandle> {
        self.install(destination, receiver, None)
    }

    /// Only the receiving edge may install this candidate queue. Its binding
    /// must already qualify decoder initialization and codec unit boundaries.
    pub fn install_filtered_branch(
        &mut self,
        destination: ParticipantHandle,
        receiver: Emission,
        target: DecodeTarget,
    ) -> Option<FabricBranchHandle> {
        if !matches!(self.emission.profile, Profile::H264 | Profile::Av1)
            || self.emission.mask & bit(ExtensionKind::DependencyDescriptor) == 0
        {
            return None;
        }
        self.install(
            destination,
            receiver,
            Some(Box::new(FilteredBranch::new(target, true))),
        )
    }

    fn install(
        &mut self,
        destination: ParticipantHandle,
        receiver: Emission,
        filter: Option<Box<FilteredBranch>>,
    ) -> Option<FabricBranchHandle> {
        if self.fenced || !self.emission.fits(receiver) || self.branches.len() >= MAX_BRANCHES {
            return None;
        }
        let mut existing = self
            .branches
            .values()
            .filter(|branch| branch.destination == destination);
        if let Some(active) = existing.next()
            && (existing.next().is_some()
                || active
                    .filter
                    .as_ref()
                    .zip(filter.as_ref())
                    .is_none_or(|(active, staged)| {
                        active.selected_target() == staged.selected_target()
                    }))
        {
            return None;
        }
        if self.emission.mask & bit(ExtensionKind::DependencyDescriptor) != 0 {
            self.evidence.get_or_insert_with(Box::default);
        }
        Some(self.branches.insert(Branch {
            destination,
            admissions: LiveAdmissions::default(),
            repairs: None,
            queue: VecDeque::with_capacity(QUEUE_PACKETS),
            filter,
            dd_reset: None,
            filtered_structure: None,
            omitted: 0,
            last_omitted: None,
        }))
    }

    pub(crate) fn branch_target(&self, branch: FabricBranchHandle) -> Option<DecodeTarget> {
        Some(
            self.branches
                .get(branch)?
                .filter
                .as_ref()?
                .selected_target(),
        )
    }

    pub(crate) fn destinations(
        &self,
    ) -> impl Iterator<Item = (FabricBranchHandle, ParticipantHandle)> + '_ {
        self.branches
            .iter()
            .map(|(handle, branch)| (handle, branch.destination))
    }

    #[cfg(test)]
    fn accept_data_expectation(
        &mut self,
        now: Instant,
        expectation: DataExpectation,
        mut eligible: impl FnMut(FabricBranchHandle, ParticipantHandle) -> bool,
    ) -> Result<ExpectationAdmission, ExpectationError> {
        self.accept_data_expectation_until(now, None, expectation, |key, destination| {
            eligible(key, destination).then_some(NonZeroU64::MIN)
        })
    }

    pub(crate) fn accept_data_expectation_until(
        &mut self,
        now: Instant,
        source_deadline: Option<Instant>,
        expectation: DataExpectation,
        eligible: impl FnMut(FabricBranchHandle, ParticipantHandle) -> Option<NonZeroU64>,
    ) -> Result<ExpectationAdmission, ExpectationError> {
        self.accept_data_cohort(now, source_deadline, expectation, eligible, Vec::new)
    }

    pub(crate) fn accept_data_cohort(
        &mut self,
        now: Instant,
        source_deadline: Option<Instant>,
        expectation: DataExpectation,
        mut eligible: impl FnMut(FabricBranchHandle, ParticipantHandle) -> Option<NonZeroU64>,
        shards: impl FnOnce() -> Vec<ShardDataCohort>,
    ) -> Result<ExpectationAdmission, ExpectationError> {
        if self.fenced || self.emission.profile() != Profile::Data {
            return Err(ExpectationError::Binding);
        }
        let branches = &self.branches;
        self.expectations.get_or_insert_with(Box::default).accept(
            now,
            source_deadline,
            expectation,
            || {
                branches.iter().filter_map(move |(key, branch)| {
                    eligible(key, branch.destination)
                        .map(|incarnation| ((key, branch.destination), incarnation))
                })
            },
            shards,
        )
    }

    pub(crate) fn permits_shard_data(
        &self,
        now: Instant,
        unit: DataExpectation,
        binding: super::ShardBinding,
        route: &crate::shard_update::CompiledRoute,
    ) -> bool {
        self.expectations
            .as_ref()
            .is_some_and(|expectations| expectations.has_shard(now, unit, binding, route))
    }

    pub(crate) fn record_shard_receipt(
        &mut self,
        now: Instant,
        route: &crate::shard_update::CompiledRoute,
        receipt: super::ShardDataReceipt,
    ) -> Option<ReceiptAcceptance> {
        if self.fenced {
            return None;
        }
        self.expectations.as_mut()?.record_shard_receipt(
            now,
            receipt.binding,
            route,
            receipt.unit,
            receipt.branch,
        )
    }

    pub(crate) fn shard_repair_window(
        &self,
        now: Instant,
        route: &crate::shard_update::CompiledRoute,
        repair: super::ShardDataRepair,
    ) -> Option<u16> {
        if self.fenced {
            return None;
        }
        self.expectations
            .as_ref()?
            .shard_repair_window(now, route, repair)
    }

    pub(crate) fn send_initial_shard_notices(
        &mut self,
        now: Instant,
        unit: DataExpectation,
        send: impl FnMut(crate::shard_update::CompiledRoute, super::ShardDataExpectation),
    ) {
        if let Some(expectations) = &mut self.expectations {
            expectations.send_initial_notices(now, unit, send);
        }
    }

    pub(crate) fn shard_notice_deadline(&self) -> Option<Instant> {
        self.expectations.as_ref()?.notice_deadline()
    }

    pub(crate) fn tick_shard_notice(
        &mut self,
        now: Instant,
        send: impl FnOnce(crate::shard_update::CompiledRoute, super::ShardDataExpectation),
    ) {
        if let Some(expectations) = &mut self.expectations {
            expectations.tick_notice(now, send);
        }
    }

    /// The callback performs the current authorization check and write together.
    pub(crate) fn handoff_data_expectation<T>(
        &self,
        now: Instant,
        unit_seq: u64,
        branch: FabricBranchHandle,
        destination: ParticipantHandle,
        write_authorized: impl FnOnce(DataExpectation) -> Option<T>,
    ) -> Option<T> {
        if !self.authorizes(branch, destination) {
            return None;
        }
        let (expected, _) =
            self.expectations
                .as_ref()?
                .for_child(now, unit_seq, (branch, destination))?;
        write_authorized(expected)
    }

    pub(crate) fn repair_until(
        &self,
        now: Instant,
        target: RepairTarget,
        request: RepairRequest,
    ) -> Option<Instant> {
        if !self.authorizes(target.branch, target.destination) {
            return None;
        }
        self.expectations.as_ref()?.repair_until(
            now,
            request.fragment,
            (target.branch, target.destination),
            target.incarnation,
        )
    }

    pub(crate) fn has_media_position(&self, sequence: u64) -> bool {
        !self.fenced
            && self.emission.profile() != Profile::Data
            && self
                .fates
                .get(&sequence)
                .is_some_and(|frozen| frozen.live_arrival.is_some())
    }

    pub(crate) fn cached_peer_media(
        &self,
        sequence: u64,
        ingress: Option<NonZeroU64>,
        now: Instant,
    ) -> Option<(&OwnedPacket, Instant)> {
        if self.fenced || self.emission.profile() == Profile::Data {
            return None;
        }
        let frozen = self.fates.get(&sequence)?;
        let arrival = frozen.live_arrival?;
        let expires = arrival.checked_add(super::network_repair::MAX_REPAIR_LIFETIME)?;
        (now >= arrival
            && now < expires
            && frozen.live_ingress == ingress
            && frozen.packet.remaining_hops() != 0)
            .then_some((&frozen.packet, expires))
    }

    pub(crate) fn cached_data_position(
        &self,
        fragment: RepairFragment,
        fragment_count: u16,
        ingress: Option<NonZeroU64>,
        now: Instant,
    ) -> Option<u64> {
        use pulsebeam_proto::data_record::wire_identity;
        if self.fenced || self.emission.profile() != Profile::Data {
            return None;
        }
        let mut matches = self.fates.iter().filter_map(|(&sequence, frozen)| {
            let arrival = frozen.live_arrival?;
            if now < arrival || now.saturating_duration_since(arrival) >= reliable::LIFETIME {
                return None;
            }
            if frozen.packet.remaining_hops() == 0 || frozen.live_ingress != ingress {
                return None;
            }
            let identity = wire_identity(frozen.packet.payload()).ok()?;
            let header = frozen.packet.header();
            (identity.unit_seq == fragment.unit_seq
                && identity.index == fragment.index
                && identity.count == fragment_count
                && header.marker == identity.final_fragment())
            .then_some(sequence)
        });
        let sequence = matches.next()?;
        matches.next().is_none().then_some(sequence)
    }

    pub(crate) fn cached_peer_data(
        &self,
        sequence: u64,
        fragment: RepairFragment,
        count: u16,
        ingress: Option<NonZeroU64>,
        now: Instant,
    ) -> Option<(&OwnedPacket, Instant)> {
        if self.cached_data_position(fragment, count, ingress, now)? != sequence {
            return None;
        }
        let frozen = self.fates.get(&sequence)?;
        Some((
            &frozen.packet,
            frozen.live_arrival?.checked_add(reliable::LIFETIME)?,
        ))
    }

    pub(crate) fn pending_repair_until(
        &self,
        now: Instant,
        target: RepairTarget,
        request: RepairRequest,
    ) -> Option<Instant> {
        let expectation = self.repair_until(now, target, request)?;
        let repairs = self.branches.get(target.branch)?.repairs.as_ref()?;
        if repairs.incarnation != target.incarnation {
            return None;
        }
        let deadline = expectation.min(repairs.pending_until(request)?);
        (now < deadline).then_some(deadline)
    }

    pub(crate) fn arm_network_repair(
        &mut self,
        target: RepairTarget,
        request: RepairRequest,
    ) -> bool {
        self.data_repairs(target)
            .is_some_and(|repairs| repairs.arm_network_repair(request))
    }

    pub(crate) fn request_data_repair(
        &mut self,
        now: Instant,
        target: RepairTarget,
        request: RepairRequest,
    ) -> RepairAdmission {
        let Some(until) = self.repair_until(now, target, request) else {
            return RepairAdmission::Ineligible;
        };
        self.branches
            .get_mut(target.branch)
            .map_or(RepairAdmission::Ineligible, |branch| {
                let repairs = branch
                    .repairs
                    .get_or_insert_with(|| Box::new(repair::Repairs::new(target.incarnation)));
                if repairs.incarnation != target.incarnation {
                    return RepairAdmission::Ineligible;
                }
                repairs.request(now, request, until)
            })
    }

    fn data_repairs(&mut self, target: RepairTarget) -> Option<&mut repair::Repairs> {
        let branch = self.branches.get_mut(target.branch)?;
        if branch.destination != target.destination {
            return None;
        }
        branch
            .repairs
            .as_deref_mut()
            .filter(|repairs| repairs.incarnation == target.incarnation)
    }

    pub(crate) fn cancel_data_repair(
        &mut self,
        target: RepairTarget,
        request: RepairRequest,
    ) -> bool {
        self.data_repairs(target)
            .is_some_and(|repairs| repairs.cancel(request))
    }

    pub(crate) fn expire_data_repair(
        &mut self,
        now: Instant,
        target: RepairTarget,
    ) -> Option<RepairRequest> {
        self.data_repairs(target)?.expire(now)
    }

    pub(crate) fn claim_data_repair(
        &mut self,
        now: Instant,
        target: RepairTarget,
        request: RepairRequest,
        sequence: u64,
    ) -> bool {
        self.repair_until(now, target, request).is_some()
            && self
                .data_repairs(target)
                .is_some_and(|repairs| repairs.claim(now, request, sequence))
    }

    pub(crate) fn handoff_data_repair(
        &mut self,
        now: Instant,
        target: RepairTarget,
        request: RepairRequest,
        sequence: u64,
        write_authorized: impl FnOnce() -> bool,
    ) -> bool {
        self.repair_until(now, target, request).is_some()
            && self
                .data_repairs(target)
                .is_some_and(|repairs| repairs.handoff(now, request, sequence, write_authorized))
    }

    pub(crate) fn next_data_repair_expiry(&self) -> Option<(Instant, RepairTarget, RepairRequest)> {
        self.branches
            .iter()
            .filter_map(|(branch, bound)| {
                let repairs = bound.repairs.as_ref()?;
                let (expires, request) = repairs.next_expiry()?;
                let target = RepairTarget {
                    branch,
                    destination: bound.destination,
                    incarnation: repairs.incarnation,
                };
                Some((expires, target, request))
            })
            .min_by_key(|(expires, _, request)| (*expires, request.id))
    }

    pub(crate) fn next_data_repair(&self) -> Option<RepairWork> {
        self.branches
            .iter()
            .filter_map(|(branch, bound)| {
                let repairs = bound.repairs.as_ref()?;
                let (due, expires, request, queued) = repairs.next()?;
                Some(RepairWork {
                    due,
                    expires,
                    request,
                    queued,
                    target: RepairTarget {
                        branch,
                        destination: bound.destination,
                        incarnation: repairs.incarnation,
                    },
                })
            })
            .min_by_key(|work| (work.due, work.request.id))
    }

    pub(crate) fn defer_data_repair(&mut self, now: Instant, work: RepairWork) {
        if let Some(repairs) = self.data_repairs(work.target) {
            repairs.defer(now, work.request);
        }
    }

    pub(crate) fn claim_live_data(
        &mut self,
        now: Instant,
        fragment: RepairFragment,
        sequence: u64,
    ) {
        let Some(expectations) = &self.expectations else {
            return;
        };
        for (handle, branch) in &mut self.branches {
            if let Some(repairs) = &mut branch.repairs
                && let Some(request) = repairs.for_fragment(fragment)
                && branch
                    .queue
                    .iter()
                    .any(|queued| queued.sequence == sequence && queued.repair_only.is_none())
                && expectations
                    .repair_until(
                        now,
                        fragment,
                        (handle, branch.destination),
                        repairs.incarnation,
                    )
                    .is_some()
            {
                repairs.claim(now, request, sequence);
            }
        }
    }

    pub(crate) fn repair_is_queued(
        &mut self,
        target: RepairTarget,
        request: RepairRequest,
    ) -> bool {
        self.data_repairs(target)
            .is_some_and(|repairs| repairs.is_queued(request))
    }

    pub(crate) fn claim_live_repair(
        &mut self,
        now: Instant,
        target: RepairTarget,
        request: RepairRequest,
        sequence: u64,
    ) {
        if self.repair_until(now, target, request).is_none() {
            return;
        }
        if let Some(branch) = self.branches.get_mut(target.branch)
            && branch
                .queue
                .iter()
                .any(|queued| queued.sequence == sequence && queued.repair_only.is_none())
            && let Some(repairs) = &mut branch.repairs
        {
            repairs.claim(now, request, sequence);
        }
    }

    pub(crate) fn enqueue_data_repair(
        &mut self,
        now: Instant,
        target: RepairTarget,
        request: RepairRequest,
        sequence: u64,
    ) -> RepairQueue {
        let Some(frozen) = self
            .fates
            .get(&sequence)
            .filter(|frozen| frozen.live_arrival.is_some())
        else {
            return RepairQueue::Unavailable;
        };
        if !pulsebeam_proto::data_record::wire_identity(frozen.packet.payload()).is_ok_and(
            |identity| {
                identity.unit_seq == request.fragment.unit_seq
                    && identity.index == request.fragment.index
            },
        ) {
            return RepairQueue::Unavailable;
        }
        let hops = frozen.packet.remaining_hops();
        self.enqueue_known_repair(now, target, request, sequence, hops, None)
    }

    pub(crate) fn enqueue_network_repair(
        &mut self,
        now: Instant,
        target: RepairTarget,
        demand: super::network_repair::Demand,
        packet: OwnedPacket,
        parent: NonZeroU64,
    ) -> RepairQueue {
        self.enqueue_repair_image(
            now,
            target,
            demand.request,
            demand.sequence,
            packet,
            Some(parent),
        )
    }

    pub(crate) fn enqueue_shard_repair(
        &mut self,
        now: Instant,
        target: RepairTarget,
        image: super::ShardDataRepairImage,
    ) -> RepairQueue {
        if image.repair.receiver.branch != target.incarnation {
            return RepairQueue::Unavailable;
        }
        self.enqueue_repair_image(
            now,
            target,
            image.repair.request,
            image.sequence,
            image.packet,
            None,
        )
    }

    fn enqueue_repair_image(
        &mut self,
        now: Instant,
        target: RepairTarget,
        request: RepairRequest,
        sequence: u64,
        packet: OwnedPacket,
        ingress: Option<NonZeroU64>,
    ) -> RepairQueue {
        let [a, b, c, d, ..] = sequence.to_le_bytes();
        if self.pending_repair_until(now, target, request).is_none()
            || !self.emission.accepts(&packet)
            || packet.header().sequence != u32::from_le_bytes([a, b, c, d])
        {
            return RepairQueue::Unavailable;
        }
        let valid_fragment = pulsebeam_proto::data_record::wire_identity(packet.payload())
            .is_ok_and(|identity| {
                identity.unit_seq == request.fragment.unit_seq
                    && identity.index == request.fragment.index
                    && packet.header().marker == identity.final_fragment()
                    && self
                        .expectations
                        .as_ref()
                        .and_then(|expectations| {
                            expectations.for_child(
                                now,
                                identity.unit_seq,
                                (target.branch, target.destination),
                            )
                        })
                        .is_some_and(|(unit, _)| unit.fragment_count == identity.count)
            });
        if !valid_fragment
            || !self
                .branches
                .get(target.branch)
                .and_then(|branch| branch.repairs.as_ref())
                .is_some_and(|repairs| repairs.can_claim(now, request, sequence))
        {
            return RepairQueue::Unavailable;
        }
        let high = self.high.map_or(sequence, |old| old.max(sequence));
        let oldest = high.saturating_sub(POSITIONS - 1);
        if sequence < oldest {
            return RepairQueue::Unavailable;
        }
        let hops = packet.remaining_hops();
        if let Some(original) = self.fates.get(&sequence) {
            if !original.packet.same_content(&packet) {
                return RepairQueue::Unavailable;
            }
        } else {
            self.retire_before(oldest);
            self.fates.insert(
                sequence,
                Frozen {
                    packet,
                    capture: None,
                    live_arrival: None,
                    ready_at: None,
                    live_ingress: None,
                    endpoint_original: false,
                },
            );
        }
        self.high = Some(high);
        self.enqueue_known_repair(now, target, request, sequence, hops, ingress)
    }

    fn enqueue_known_repair(
        &mut self,
        now: Instant,
        target: RepairTarget,
        request: RepairRequest,
        sequence: u64,
        hops: u8,
        ingress: Option<NonZeroU64>,
    ) -> RepairQueue {
        if self.repair_until(now, target, request).is_none() {
            return RepairQueue::Unavailable;
        }
        let Some(high) = self.high else {
            return RepairQueue::Unavailable;
        };
        let Some(branch) = self.branches.get_mut(target.branch) else {
            return RepairQueue::Unavailable;
        };
        branch.discard_stale(now, &self.fates, |_| true);
        let Some(repairs) = &mut branch.repairs else {
            return RepairQueue::Unavailable;
        };
        if repairs.incarnation != target.incarnation || !repairs.can_claim(now, request, sequence) {
            return RepairQueue::Unavailable;
        }
        if let Some(queued) = branch
            .queue
            .iter_mut()
            .find(|queued| queued.sequence == sequence)
        {
            if queued.repair_only.is_some() {
                queued.repair_only = Some(request);
                queued.ingress = ingress;
            }
        } else {
            let mut enqueue = || {
                if branch.queue.len() >= QUEUE_PACKETS {
                    return false;
                }
                branch.queue.push_back(Queued {
                    sequence,
                    output_position: sequence,
                    queued_at: now,
                    repair_only: Some(request),
                    hops,
                    ingress,
                    selection: pulsebeam_core::dd::output::Mode::Whole,
                    entry: false,
                    frame: None,
                });
                true
            };
            match branch.admissions.offer(high, sequence, &mut enqueue) {
                LiveOffer::Enqueued => {}
                LiveOffer::Duplicate => {
                    if !enqueue() {
                        return RepairQueue::Backpressured;
                    }
                }
                LiveOffer::Refused => return RepairQueue::Backpressured,
                LiveOffer::Stale | LiveOffer::AheadOfSource => return RepairQueue::Unavailable,
            }
        }
        if repairs.claim(now, request, sequence) {
            RepairQueue::Queued
        } else {
            RepairQueue::Unavailable
        }
    }

    pub(crate) fn admit_data(
        &mut self,
        now: Instant,
        handle: FabricBranchHandle,
        destination: ParticipantHandle,
        current_incarnation: Option<NonZeroU64>,
        permits: impl Fn(Option<NonZeroU64>) -> bool,
        write: impl FnOnce(u64, PacketView<'_>, Option<RepairRequest>) -> bool,
    ) -> Option<DataHandoff> {
        if self.emission.profile() != Profile::Data || !self.authorizes(handle, destination) {
            return None;
        }
        loop {
            let branch = self.branches.get_mut(handle)?;
            branch.discard_stale(now, &self.fates, &permits);
            let front = branch.queue.front()?;
            let (sequence, repair_only, hops) = (front.sequence, front.repair_only, front.hops);
            let claim = branch.repairs.as_ref().and_then(|repairs| {
                let request = repairs.queued(now, sequence)?;
                Some((
                    RepairTarget {
                        branch: handle,
                        destination,
                        incarnation: repairs.incarnation,
                    },
                    request,
                ))
            });
            let request = claim.and_then(|(target, request)| {
                self.repair_until(now, target, request).map(|_| request)
            });
            let branch = self.branches.get_mut(handle)?;
            if repair_only.is_some() && repair_only != request {
                branch.discard_front();
                continue;
            }
            let packet = self.fates.get(&sequence)?.packet.arrival_view(hops)?;
            if let Some(current) = current_incarnation
                && !pulsebeam_proto::data_record::wire_identity(packet.payload()).is_ok_and(
                    |identity| {
                        self.expectations
                            .as_ref()
                            .and_then(|expected| {
                                expected.for_child(now, identity.unit_seq, (handle, destination))
                            })
                            .is_some_and(|(unit, incarnation)| {
                                current == incarnation && unit.fragment_count == identity.count
                            })
                    },
                )
            {
                branch.discard_front();
                continue;
            }
            let written = write(sequence, packet, request);
            if !written && request.is_some() {
                return Some(DataHandoff {
                    written,
                    completed: None,
                });
            }
            let completed = request.filter(|&request| {
                branch
                    .repairs
                    .as_mut()
                    .is_some_and(|repairs| repairs.handoff(now, request, sequence, || true))
            });
            branch.discard_front();
            return Some(DataHandoff { written, completed });
        }
    }

    pub(crate) fn data_propagation_deadline(&self) -> Option<Instant> {
        self.expectations.as_ref()?.deadline()
    }

    pub(crate) fn tick_data_propagation(
        &mut self,
        now: Instant,
        write: impl FnOnce(
            FabricBranchHandle,
            ParticipantHandle,
            DataExpectation,
            NonZeroU64,
        ) -> ExpectationWrite,
    ) -> Option<PropagationStep> {
        let branches = &self.branches;
        let fenced = self.fenced;
        self.expectations
            .as_mut()?
            .tick(now, |(branch, destination), unit, incarnation| {
                if fenced
                    || branches
                        .get(branch)
                        .is_none_or(|live| live.destination != destination)
                {
                    ExpectationWrite::Revoked
                } else {
                    write(branch, destination, unit, incarnation)
                }
            })
    }

    pub(crate) fn settle_data_propagation(
        &mut self,
        now: Instant,
        unit: DataExpectation,
        branch: FabricBranchHandle,
        destination: ParticipantHandle,
        outcome: PropagationOutcome,
    ) -> bool {
        self.authorizes(branch, destination)
            && self
                .expectations
                .as_mut()
                .is_some_and(|ledger| ledger.settle(now, unit, (branch, destination), outcome))
    }

    pub(crate) fn record_data_receipt(
        &mut self,
        now: Instant,
        unit: DataExpectation,
        branch: FabricBranchHandle,
        destination: ParticipantHandle,
        branch_incarnation: NonZeroU64,
    ) -> Option<ReceiptAcceptance> {
        if !self.authorizes(branch, destination) {
            return None;
        }
        self.expectations.as_mut()?.record_receipt(
            now,
            unit,
            (branch, destination),
            branch_incarnation,
        )
    }

    pub(crate) fn data_delivery_deadline(&self) -> Option<Instant> {
        self.expectations.as_ref()?.delivery_deadline()
    }

    pub(crate) fn tick_data_delivery(
        &mut self,
        now: Instant,
        write: impl FnOnce(DeliveryReport) -> ExpectationWrite,
    ) -> Option<()> {
        self.expectations.as_mut()?.tick_delivery(now, write)
    }

    pub(crate) fn branch_for(&self, destination: ParticipantHandle) -> Option<FabricBranchHandle> {
        self.branches
            .iter()
            .find_map(|(handle, branch)| (branch.destination == destination).then_some(handle))
    }

    pub fn retain_destinations(&mut self, mut authorized: impl FnMut(ParticipantHandle) -> bool) {
        self.branches
            .retain(|_, branch| authorized(branch.destination));
    }

    pub(crate) fn authorizes(
        &self,
        handle: FabricBranchHandle,
        destination: ParticipantHandle,
    ) -> bool {
        !self.fenced
            && self
                .branches
                .get(handle)
                .is_some_and(|branch| branch.destination == destination)
    }

    pub(crate) fn lose_output_context(
        &mut self,
        handle: FabricBranchHandle,
        destination: ParticipantHandle,
    ) -> bool {
        if !self.authorizes(handle, destination) || self.evidence.is_none() {
            return false;
        }
        let Some(branch) = self.branches.get_mut(handle) else {
            return false;
        };
        if branch.dd_reset == self.high {
            return false;
        }
        branch.dd_reset = self.high;
        if let Some(filter) = &mut branch.filter {
            filter.lose_context();
            branch.queue.clear();
            true
        } else {
            false
        }
    }

    pub(crate) fn awaiting_entry(
        &self,
        mut authorized: impl FnMut(ParticipantHandle) -> bool,
    ) -> Option<(FabricBranchHandle, ParticipantHandle)> {
        if self.fenced {
            return None;
        }
        self.branches.iter().find_map(|(handle, branch)| {
            (branch
                .filter
                .as_ref()
                .is_some_and(|filter| filter.awaiting_entry())
                && authorized(branch.destination))
            .then_some((handle, branch.destination))
        })
    }

    pub fn retire_branch(&mut self, handle: FabricBranchHandle) -> bool {
        self.branches.remove(handle).is_some()
    }

    /// Accept an owned canonical attempt from the installed source. The
    /// extended source sequence comes from the source owner, not the wire PT
    /// or the destination. A retry cannot refresh the first copy's lifetime.
    #[cfg(test)]
    pub fn offer(
        &mut self,
        now: Instant,
        arrival: Instant,
        sequence: u64,
        packet: OwnedPacket,
    ) -> Result<Offers, Rejection> {
        self.offer_from(now, arrival, sequence, Some(packet), None, None)
    }

    pub(crate) fn offer_from_shard(
        &mut self,
        now: Instant,
        offer: ShardOffer,
    ) -> Result<Offers, Rejection> {
        self.offer_from(
            now,
            offer.arrival,
            offer.sequence,
            Some(offer.packet),
            None,
            offer.ready_at,
        )
    }

    pub(crate) fn bind_media_identity(&mut self, identity: MediaIdentity) -> bool {
        if self.fenced
            || self.high.is_some()
            || self.media_identity.is_some()
            || self.emission.profile() != identity.profile()
        {
            return false;
        }
        self.media_identity = Some(Box::new(identity));
        true
    }

    pub(crate) fn offer_endpoint_media(
        &mut self,
        now: Instant,
        input: &crate::rtp::fabric_ingress::MediaIngress,
        peer_repair: bool,
        assign_timestamp: impl FnOnce(Instant, u32) -> u32,
    ) -> Result<Option<(u64, u8, Offers)>, Rejection> {
        if self.fenced {
            return Err(Rejection::Fenced);
        }
        let identity = self.media_identity.as_mut().ok_or(Rejection::Binding)?;
        let originals = &self.fates;
        let admitted = input.with_input(|media| {
            identity.admit(
                now.into(),
                input.arrival,
                input.position,
                media,
                |sequence| originals.get(&sequence).map(|frozen| &frozen.packet),
            )
        });
        let admitted = match admitted {
            Ok(admitted) => admitted,
            Err(Rejected::Conflict) => {
                self.fence();
                return Err(Rejection::Conflict);
            }
            Err(Rejected::Wire(WireError::Oversize)) => {
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("fabric_rtp_ingress_oversize");
                metrics::counter!("participant_fabric_ingress_reject", "reason" => "oversize")
                    .increment(1);
                return Err(Rejection::Binding);
            }
            Err(_) => return Err(Rejection::Stale),
        };
        // RTP copy recognition and peer repair have independent windows.
        // Advancing the former must not discard an entitled peer original.
        let identity_floor = identity.oldest();
        if !peer_repair {
            self.retire_before(identity_floor);
        }
        let IdentityAdmission::Media {
            sequence,
            arrival,
            original,
        } = admitted
        else {
            return Ok(None);
        };
        let original = original.map(|packet| {
            let timestamp = assign_timestamp(Instant::from_std(arrival), packet.header().timestamp);
            let packet = packet.with_admitted_media_timestamp(timestamp);
            match input.protected_bytes {
                Some(bytes) => packet.with_source_protected_bytes(bytes),
                None => packet,
            }
        });
        let report = self.offer_from(
            now,
            Instant::from_std(arrival),
            sequence,
            original,
            None,
            Some(input.ready_at),
        )?;
        let hops = self
            .fates
            .get(&sequence)
            .ok_or(Rejection::Stale)?
            .packet
            .remaining_hops();
        Ok(Some((sequence, hops, report)))
    }

    fn fence(&mut self) {
        self.fenced = true;
        self.fates.clear();
        self.branches.clear();
    }

    pub(crate) fn retain_endpoint_data(&mut self, sequence: u64, packet: OwnedPacket) {
        debug_assert!(self.emission.accepts(&packet));
        debug_assert!(!self.fates.contains_key(&sequence));
        self.fates.insert(
            sequence,
            Frozen {
                packet,
                capture: None,
                live_arrival: None,
                ready_at: None,
                live_ingress: None,
                endpoint_original: true,
            },
        );
    }

    pub(crate) fn endpoint_data(&self, sequence: u64) -> Option<&OwnedPacket> {
        self.fates
            .get(&sequence)
            .filter(|frozen| frozen.endpoint_original)
            .map(|frozen| &frozen.packet)
    }

    #[cfg(test)]
    pub(crate) fn endpoint_data_count(&self) -> usize {
        self.fates
            .values()
            .filter(|entry| entry.endpoint_original)
            .count()
    }

    pub(crate) fn release_endpoint_data(&mut self, sequence: u64) {
        if let Some(frozen) = self.fates.get_mut(&sequence) {
            // Endpoint unit closure revokes live and repair work for this original.
            if frozen.endpoint_original {
                self.fates.remove(&sequence);
            }
        }
    }

    fn retire_before(&mut self, oldest: u64) {
        while self.retired_before < oldest {
            let Some((&sequence, frozen)) = self.fates.range(self.retired_before..oldest).next()
            else {
                break;
            };
            self.retired_before = sequence.saturating_add(1);
            if !frozen.endpoint_original {
                self.fates.remove(&sequence);
            }
        }
        self.retired_before = self.retired_before.max(oldest);
    }

    pub(crate) fn offer_retained_data(
        &mut self,
        now: Instant,
        arrival: Instant,
        sequence: u64,
    ) -> Result<Offers, Rejection> {
        self.endpoint_data(sequence).ok_or(Rejection::Stale)?;
        self.offer_from(now, arrival, sequence, None, None, None)
    }

    fn offer_from(
        &mut self,
        now: Instant,
        arrival: Instant,
        sequence: u64,
        packet: Option<OwnedPacket>,
        ingress: Option<NonZeroU64>,
        ready_at: Option<std::time::Instant>,
    ) -> Result<Offers, Rejection> {
        if self.fenced {
            return Err(Rejection::Fenced);
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "Fabric wire carries only the low 32 bits"
        )]
        let wire_sequence = sequence as u32;
        if packet.as_ref().is_some_and(|packet| {
            packet.header().sequence != wire_sequence || !self.emission.accepts(packet)
        }) {
            return Err(Rejection::Binding);
        }
        if now < arrival || now.saturating_duration_since(arrival) >= MAX_AGE {
            return Err(Rejection::Stale);
        }
        let high = self.high.map_or(sequence, |high| high.max(sequence));
        let oldest = high.saturating_sub(POSITIONS - 1);
        if sequence < oldest {
            return Err(Rejection::Stale);
        }
        let original = self.fates.get(&sequence);
        if let Some(original) = original {
            if original.live_arrival.is_some() && !original.live(now) {
                return Err(Rejection::Stale);
            }
            if packet
                .as_ref()
                .is_some_and(|packet| !original.packet.same_content(packet))
            {
                self.fence();
                return Err(Rejection::Conflict);
            }
        }
        let image = packet
            .as_ref()
            .or_else(|| original.map(|frozen| &frozen.packet))
            .ok_or(Rejection::Stale)?;
        let hops = image.remaining_hops();
        let arrival = original
            .and_then(|frozen| frozen.live_arrival)
            .unwrap_or(arrival);
        let work = LiveWork::new(high, sequence, arrival, hops).ok_or(Rejection::Stale)?;
        let ordered = self.evidence.is_some()
            && (self.emission.selects_dd_encoding() || !self.branches.is_empty())
            && self.branches.values().all(|branch| branch.filter.is_some());
        let qualified = if ordered {
            None
        } else if let Some(evidence) = &mut self.evidence {
            evidence.qualify_position(sequence, image, self.media_identity.as_deref())?
        } else {
            None
        };
        let dd_structure = self
            .high
            .is_none_or(|old| old < sequence)
            .then(|| qualified.as_ref().and_then(dd_structure))
            .flatten();
        let new_live = original.is_none_or(|frozen| frozen.live_arrival.is_none());
        let mut dd_sample = DecodeTargetSample::None;
        if new_live && self.emission.selects_dd_encoding() && !ordered {
            dd_sample.record(qualified.as_ref().map(|(dd, _)| dd), image.payload().len());
        }
        if new_live {
            self.retire_before(oldest);
            if let Some(frozen) = self.fates.get_mut(&sequence) {
                frozen.live_arrival = Some(arrival);
                frozen.ready_at = ready_at;
                frozen.live_ingress = ingress;
                if !frozen.endpoint_original
                    && let Some(packet) = packet
                {
                    frozen.packet = packet;
                }
            } else {
                self.fates.insert(
                    sequence,
                    Frozen {
                        packet: packet.ok_or(Rejection::Stale)?,
                        capture: None,
                        live_arrival: Some(arrival),
                        ready_at,
                        live_ingress: ingress,
                        endpoint_original: false,
                    },
                );
            }
        }
        self.high = Some(high);
        if ordered {
            return Ok(self.offer_ordered_dd(now, high));
        }
        if let Some(evidence) = &mut self.evidence {
            evidence.ordered_through = Some(high);
        }
        let mut report = Offers {
            dd_structure,
            dd_sample,
            ..Offers::default()
        };
        for branch in self.branches.values_mut() {
            branch.discard_stale(now, &self.fates, |_| true);
            branch.offer(now, work, ingress, qualified.as_ref(), false, &mut report);
        }
        Ok(report)
    }

    fn offer_ordered_dd(&mut self, now: Instant, high: u64) -> Offers {
        let mut report = Offers::default();
        let Some(evidence) = &mut self.evidence else {
            return report;
        };
        for _ in 0..=DD_REORDER_POSITIONS {
            let next = match evidence.ordered_through {
                Some(previous) => match previous.checked_add(1) {
                    Some(next) => next,
                    None => break,
                },
                None => high,
            };
            let Some((&position, frozen)) = self
                .fates
                .range(next..)
                .find(|(_, frozen)| frozen.live_arrival.is_some())
            else {
                break;
            };
            let Some(arrival) = frozen.live_arrival else {
                break;
            };
            let padding_only = position > next
                && self
                    .media_identity
                    .as_ref()
                    .is_some_and(|identity| identity.covers_padding(next..position));
            if position != next
                && !padding_only
                && high.saturating_sub(next) < DD_REORDER_POSITIONS
                && now.saturating_duration_since(arrival) < DD_REORDER_WAIT
            {
                break;
            }

            evidence.ordered_through = Some(position);
            if !frozen.live(now) {
                let _ = evidence.context.observe(position, None);
                continue;
            }
            let qualified = evidence
                .qualify_position(position, &frozen.packet, self.media_identity.as_deref())
                .ok()
                .flatten();
            if self.emission.selects_dd_encoding() {
                report.dd_sample.record(
                    qualified.as_ref().map(|(dd, _)| dd),
                    frozen.packet.payload().len(),
                );
            }
            if let Some(facts) = qualified.as_ref().and_then(dd_structure) {
                report.dd_structure = Some(facts);
            }
            let Some(work) = LiveWork::new(high, position, arrival, frozen.packet.remaining_hops())
            else {
                continue;
            };
            for branch in self.branches.values_mut() {
                branch.discard_stale(now, &self.fates, |_| true);
                branch.offer(
                    now,
                    work,
                    frozen.live_ingress,
                    qualified.as_ref(),
                    true,
                    &mut report,
                );
            }
        }
        report
    }

    pub(crate) fn offer_bound_network(
        &mut self,
        now: Instant,
        arrival: Instant,
        sequence: u64,
        ingress: NonZeroU64,
        packet: OwnedPacket,
    ) -> Result<(u64, Offers), Rejection> {
        self.offer_from(now, arrival, sequence, Some(packet), Some(ingress), None)
            .map(|offers| (sequence, offers))
    }

    pub(crate) fn attach_network_capture(
        &mut self,
        sequence: u64,
        ingress: NonZeroU64,
        arrival: Instant,
        capture: pulsebeam_clock::deadline::Timing,
    ) {
        if let Some(frozen) = self.fates.get_mut(&sequence)
            && frozen.live_ingress == Some(ingress)
            && frozen.live_arrival == Some(arrival)
            && frozen.capture.is_none()
        {
            frozen.capture = Some(capture);
        }
    }

    #[cfg(test)]
    pub(crate) fn offer_network(
        &mut self,
        now: Instant,
        arrival: Instant,
        anchor: u64,
        ingress: NonZeroU64,
        packet: OwnedPacket,
    ) -> Result<(u64, Offers), Rejection> {
        const CYCLE: u64 = 1 << 32;
        const MASK: u64 = CYCLE - 1;
        let high = if self.emission.profile() == Profile::Data {
            self.high.map_or(anchor, |high| high.max(anchor))
        } else {
            self.high.unwrap_or(anchor)
        };
        let ahead = u64::from(packet.header().sequence).wrapping_sub(high & MASK) & MASK;
        let sequence = if ahead < POSITIONS {
            high.checked_add(ahead)
        } else {
            let behind = CYCLE.saturating_sub(ahead);
            if behind >= POSITIONS {
                return Err(Rejection::Stale);
            }
            high.checked_sub(behind)
        }
        .ok_or(Rejection::Stale)?;
        self.offer_bound_network(now, arrival, sequence, ingress, packet)
    }

    pub(crate) fn live_sequence_anchor(&self, now: Instant) -> Option<u64> {
        let high = self.high?;
        (!self.fenced && self.fates.get(&high).is_some_and(|frozen| frozen.live(now)))
            .then_some(high)
    }

    pub(crate) fn transit(&self, sequence: u64, hops: u8) -> Option<Transit<'_>> {
        let frozen = self.fates.get(&sequence)?;
        Some(Transit {
            work: LiveWork::new(self.high?, sequence, frozen.live_arrival?, hops)?,
            packet: frozen.packet.arrival_view(hops)?,
            ready_at: frozen.ready_at,
        })
    }

    /// Last cancellable local handoff: recheck exact branch incarnation, age,
    /// and the currently authorized destination before exposing packet bytes.
    #[cfg(test)]
    pub fn admit<'a, T, E>(
        &'a mut self,
        now: Instant,
        branch: FabricBranchHandle,
        destination: ParticipantHandle,
        write: impl FnOnce(u64, PacketView<'a>) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        self.admit_with_evidence(
            now,
            branch,
            destination,
            |_| true,
            |sequence, packet, _| write(sequence, packet),
        )
    }

    pub fn admit_with_evidence<'a, T, E>(
        &'a mut self,
        now: Instant,
        branch: FabricBranchHandle,
        destination: ParticipantHandle,
        permits: impl Fn(Option<NonZeroU64>) -> bool,
        write: impl FnOnce(u64, PacketView<'a>, OutputEvidence) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        let Some(branch) = self.branches.get_mut(branch) else {
            return Ok(None);
        };
        if branch.destination != destination {
            return Ok(None);
        }
        branch.discard_stale(now, &self.fates, permits);
        let Some(queued) = branch
            .queue
            .front()
            .filter(|queued| queued.repair_only.is_none())
        else {
            return Ok(None);
        };
        let Some(frozen) = self.fates.get(&queued.sequence) else {
            return Ok(None);
        };
        let Some(packet) = frozen.packet.arrival_view(queued.hops) else {
            return Ok(None);
        };
        let dd = queued.frame.and_then(|frame| {
            if branch
                .dd_reset
                .is_some_and(|fence| queued.sequence <= fence)
            {
                return None;
            }
            let evidence = self.evidence.as_ref()?;
            if !evidence.frames.is_consistent(frame.number) {
                return None;
            }
            let raw = packet
                .extensions()
                .find(|e| e.kind == ExtensionKind::DependencyDescriptor)?
                .value;
            let qualified = evidence
                .context
                .qualify(queued.sequence, Some(raw))
                .ok()??;
            (qualified.structure_sequence == frame.structure_sequence)
                .then_some((frame.number, qualified))
        });
        let capture = frozen
            .capture
            .filter(|_| frozen.live_ingress == queued.ingress);
        let projected = OutputEvidence {
            mode: queued.selection,
            entry: queued.entry,
            reset_after: branch.dd_reset,
            dd,
            output_position: queued.output_position,
            ingress: queued.ingress,
            capture,
        };
        let result = write(queued.sequence, packet, projected)?;
        if capture.is_some()
            && let Some(arrival) = frozen.live_arrival
        {
            let residence = now.saturating_duration_since(queued.queued_at);
            let remaining = MAX_AGE.saturating_sub(now.saturating_duration_since(arrival));
            metrics::histogram!("fabric_rtp_branch_queue_residence_seconds")
                .record(residence.as_secs_f64());
            metrics::histogram!("fabric_rtp_branch_handoff_remaining_seconds")
                .record(remaining.as_secs_f64());
        }
        if let Some(ready_at) = frozen.ready_at {
            let enqueued = Instant::now();
            metrics::histogram!("fabric_rtp_local_enqueue_delay_us").record(
                enqueued
                    .into_std()
                    .saturating_duration_since(ready_at)
                    .as_secs_f64()
                    * 1e6,
            );
            metrics::histogram!("fabric_rtp_branch_age_us").record(
                enqueued
                    .saturating_duration_since(queued.queued_at)
                    .as_secs_f64()
                    * 1e6,
            );
        }
        branch.queue.pop_front();
        Ok(Some(result))
    }

    #[cfg(test)]
    pub fn take(
        &mut self,
        now: Instant,
        branch: FabricBranchHandle,
        destination: ParticipantHandle,
    ) -> Option<PacketView<'_>> {
        self.admit(now, branch, destination, |_, packet| {
            Ok::<_, std::convert::Infallible>(packet)
        })
        .unwrap()
    }
}

impl Branch {
    fn offer(
        &mut self,
        now: Instant,
        work: LiveWork,
        ingress: Option<NonZeroU64>,
        evidence: Option<&(Qualified, FrameStatus)>,
        ordered: bool,
        report: &mut Offers,
    ) {
        let LiveWork { sequence, hops, .. } = work;
        if self.last_omitted.is_some_and(|fence| sequence <= fence)
            || self.filter.is_some() && self.dd_reset.is_some_and(|fence| sequence <= fence)
        {
            return;
        }
        let Some(output_position) = sequence.checked_sub(self.omitted) else {
            return;
        };
        let queue = &mut self.queue;
        let mut filtered = None;
        let frame = evidence.and_then(|(q, status)| match status {
            FrameStatus::Resolved(number) => Some(FrameReservation {
                number: *number,
                structure_sequence: q.structure_sequence,
            }),
            _ => None,
        });
        let outcome = work.enqueue(now, &mut self.admissions, || {
            let mut enqueue = |kind| {
                if queue.len() >= QUEUE_PACKETS {
                    return false;
                }
                if kind == Enqueue::Enter {
                    queue.clear();
                }
                queue.push_back(Queued {
                    sequence,
                    output_position,
                    queued_at: Instant::now(),
                    repair_only: None,
                    hops,
                    ingress,
                    selection: pulsebeam_core::dd::output::Mode::Whole,
                    entry: kind == Enqueue::Enter,
                    frame,
                });
                metrics::histogram!("fabric_branch_depth_packets")
                    .record(f64::from(u32::try_from(queue.len()).unwrap_or(u32::MAX)));
                true
            };
            if let Some(filter) = &mut self.filter {
                let (qualified, frame) =
                    evidence.map_or((None, FrameStatus::Unresolved), |(q, f)| (Some(q), *f));
                let qualified = qualified.filter(|q| {
                    self.filtered_structure
                        .is_none_or(|old| q.structure_sequence >= old)
                });
                if qualified.is_none() && evidence.is_some() {
                    filtered = Some(Decision::Fenced);
                    return false;
                }
                let changed = qualified
                    .is_some_and(|q| self.filtered_structure != Some(q.structure_sequence));
                let reset = changed && !filter.awaiting_entry();
                if reset {
                    filter.lose_context();
                }
                if let Some(q) = qualified {
                    self.filtered_structure = Some(q.structure_sequence);
                }
                let qualified = qualified.filter(|q| {
                    !filter.awaiting_entry() || q.descriptor.attached_structure.is_some()
                });
                let mut decision = filter.offer(sequence, frame, qualified, enqueue);
                if reset && decision != Decision::Entered {
                    decision = Decision::Suspended;
                }
                if decision == Decision::Suspended {
                    queue.clear();
                } else if matches!(decision, Decision::Admitted | Decision::Entered)
                    && let Some(packet) = queue.back_mut()
                {
                    packet.selection = pulsebeam_core::dd::output::Mode::Filtered(
                        filter.selected_target().index(),
                    );
                }
                filtered = Some(decision);
                matches!(decision, Decision::Admitted | Decision::Entered)
            } else {
                enqueue(Enqueue::Continue)
            }
        });
        let count = match (outcome, filtered) {
            (LiveOffer::Enqueued, _) => &mut report.enqueued,
            (LiveOffer::Duplicate, _) => &mut report.duplicates,
            (LiveOffer::Refused, Some(Decision::Omitted)) => {
                // Only ordered, qualified omissions are known not to be loss.
                // Freeze the projection when queuing; never renumber admitted work.
                if ordered {
                    self.omitted = self.omitted.saturating_add(1);
                    self.last_omitted = Some(sequence);
                }
                &mut report.omitted
            }
            (LiveOffer::Refused, Some(Decision::Suspended)) => &mut report.suspended,
            (LiveOffer::Refused, Some(Decision::AwaitingEntry)) => &mut report.awaiting_entry,
            (LiveOffer::Refused, Some(Decision::Fenced)) => return,
            (LiveOffer::Refused, _) => {
                metrics::counter!("fabric_branch_dropped_packets", "reason" => "full").increment(1);
                &mut report.full
            }
            _ => return,
        };
        *count = count.saturating_add(1);
    }

    fn discard_stale(
        &mut self,
        now: Instant,
        fates: &BTreeMap<u64, Frozen>,
        permits: impl Fn(Option<NonZeroU64>) -> bool,
    ) {
        while self.queue.front().is_some_and(|queued| {
            !permits(queued.ingress)
                || fates.get(&queued.sequence).is_none_or(|frozen| {
                    if let Some(request) = queued.repair_only {
                        self.repairs
                            .as_ref()
                            .and_then(|repairs| repairs.queued(now, queued.sequence))
                            != Some(request)
                    } else {
                        !frozen.live(now)
                    }
                })
        }) {
            metrics::counter!("fabric_branch_dropped_packets", "reason" => "stale_or_revoked")
                .increment(1);
            self.discard_front();
        }
    }

    fn discard_front(&mut self) {
        if let Some(queued) = self.queue.pop_front()
            && let Some(repairs) = &mut self.repairs
        {
            repairs.release(queued.sequence);
        }
    }
}

#[cfg(test)]
mod tests;
