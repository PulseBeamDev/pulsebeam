//! PRODUCTION WIRE CONTRACT — EDIT WITH EXTREME CARE. Changes can break deployed clients, mixed-version nodes, and production traffic. Require compatibility review and wire-vector tests.
//!
//! [FabricPacket #275 §5.4][rfc]: authenticated binary SCTP messages map 1:1
//! to logical DATA packets. The WAN projection carries optional DATA v1 DD
//! as a packet extension rather than in the payload. The Fabric sequence is
//! assigned on first fragment admission. Every fragment of a unit keeps the
//! first admitted 90 kHz time; M marks only the final fragment. Unit identity
//! and capture metadata remain in the record and do not qualify ACT.
//!
//! [rfc]: https://github.com/PulseBeamDev/pulsebeam/issues/275

use core::mem::size_of;
use std::{collections::BTreeMap, time::Duration};

use pulsebeam_clock::{
    domain::{ClockDomain, DataCapture, DataReceipt, Provenance, Quality},
    time::{ClockRate, GlobalDuration, MonotonicTime, SourceTime},
};
use pulsebeam_core::fabric::{Extension, ExtensionKind, FabricPacket, Header, Profile, WireError};
use pulsebeam_proto::{
    data_control::{OriginalDemand, PublisherBinding, SourceUnit},
    data_endpoint::{Binding, Delivery, Endpoint},
    data_record::{self, Clock, Record},
};
use pulsebeam_runtime::time::Instant;

use crate::route::fabric::local::{OwnedPacket, SourceFanout};

const WINDOW: usize = 512;
const WINDOW_MASK: u64 = 511;
const UNIT_LIMIT: usize = 64;
const BYTE_LIMIT: usize = 4 * 1024 * 1024;
const LIFETIME: Duration = Duration::from_secs(2);
const ORIGINAL_RETRY: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Record(data_record::Error),
    Fabric(WireError),
    Capacity,
    Expired,
    UnitConflict,
    SourceConflict,
    Fenced,
    Sequence,
    Projection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteError {
    Projection,
    Binding,
    Sctp(pulsebeam_rtc::WriteError),
}

pub(crate) type InstalledBinding = Binding<pulsebeam_rtc::DataChannelInstallation>;

pub(crate) struct ReceiverStage {
    pub(crate) source: crate::entity::PublicationKey,
    pub(crate) endpoint: pulsebeam_proto::data_control::SubscriberBinding,
    pub(crate) prepared: PreparedSctp,
    pub(crate) capture_clock: Option<pulsebeam_proto::state::DataV1CaptureClock>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubscriberLease {
    pub(crate) owned: u64,
    pub(crate) branch: crate::shard::router::FabricLocalBranch,
    pub(crate) endpoint: pulsebeam_proto::data_control::SubscriberBinding,
}

pub(crate) struct PreparedSctp {
    channel: pulsebeam_proto::data_endpoint::Channel<pulsebeam_rtc::DataChannelInstallation>,
    format: pulsebeam_proto::data_endpoint::Format,
}

impl PreparedSctp {
    pub(crate) fn from_negotiation(
        parameters: pulsebeam_proto::state::SctpDataV1Parameters,
        installation: pulsebeam_rtc::DataChannelInstallation,
    ) -> Option<Self> {
        use core::num::{NonZeroU16, NonZeroU64};
        let stream = NonZeroU16::new(installation.channel().value())?;
        if parameters.stream_id != Some(u32::from(stream.get())) {
            return None;
        }
        Some(Self {
            channel: pulsebeam_proto::data_endpoint::Channel {
                association: installation,
                stream,
                incarnation: NonZeroU64::new(parameters.channel_incarnation)?,
            },
            format: parameters.format()?,
        })
    }

    pub(crate) fn bind(self, endpoint: Endpoint) -> InstalledBinding {
        Binding {
            channel: self.channel,
            format: self.format,
            endpoint,
        }
    }
}

/// Compiled by the destination owner from an acknowledged subscriber binding.
/// The branch and installation are compared again at handoff, not recovered
/// from the packet, DATA channel label or stream number.
pub(crate) struct Output {
    pub(super) branch: crate::shard::router::FabricLocalBranch,
    pub(super) installation: pulsebeam_rtc::DataChannelInstallation,
    clock: Option<(Box<str>, u64)>,
    #[cfg(feature = "sim")]
    source: core::num::NonZeroU64,
}

impl Output {
    pub(crate) fn from_subscriber(
        branch: crate::shard::router::FabricLocalBranch,
        binding: InstalledBinding,
        clock: Option<Clock<'_>>,
    ) -> Option<Self> {
        let Endpoint::Subscriber(_endpoint) = binding.endpoint else {
            return None;
        };
        if binding.channel.stream.get() != binding.channel.association.channel().value() {
            return None;
        }
        Some(Self {
            branch,
            installation: binding.channel.association,
            clock: clock.map(|clock| (Box::from(clock.id), clock.epoch)),
            #[cfg(feature = "sim")]
            source: _endpoint.source,
        })
    }

    pub(super) fn matches_repair(
        &self,
        packet: &OwnedPacket,
        repair: crate::route::fabric::local::RepairFragment,
    ) -> bool {
        let clock = self
            .clock
            .as_ref()
            .map(|(id, epoch)| Clock { id, epoch: *epoch });
        project_sctp_record(packet, clock, Some(repair)).is_ok()
    }

    pub(super) fn write(
        &self,
        transport: &mut super::transport::RtcDriver,
        packet: crate::route::fabric::local::PacketView<'_>,
        repair: Option<crate::route::fabric::local::RepairFragment>,
    ) -> Result<(), WriteError> {
        if !transport.supports_fabric_data(self.installation) {
            return Err(WriteError::Binding);
        }
        let clock = self
            .clock
            .as_ref()
            .map(|(id, epoch)| Clock { id, epoch: *epoch });
        let _record =
            project_sctp_record(&packet, clock, repair).map_err(|_| WriteError::Projection)?;
        let bytes = packet.payload();
        transport
            .write_installed_binary(self.installation, bytes)
            .map_err(WriteError::Sctp)?;
        #[cfg(feature = "sim")]
        {
            let header = _record.header();
            crate::sim_metrics::record_wan_data_sctp(
                self.source.get(),
                header.unit_seq,
                header.index,
                bytes,
                false,
            );
        }
        Ok(())
    }
}

pub(super) enum Lease {
    Input(Input),
    Output(Output),
}

impl Lease {
    pub(super) const fn installation(&self) -> pulsebeam_rtc::DataChannelInstallation {
        match self {
            Self::Input(input) => input.installation,
            Self::Output(output) => output.installation,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ReliableAdmission {
    pub(crate) unit: crate::route::fabric::local::DataExpectation,
    pub(crate) expires: Instant,
    pub(crate) acknowledge: bool,
    pub(crate) fragment_index: u16,
    pub(crate) installation: pulsebeam_rtc::DataChannelInstallation,
}

pub(super) struct Input {
    pub source: crate::shard::router::FabricSourceHandle,
    pub installation: pulsebeam_rtc::DataChannelInstallation,
    pub(super) reliable_endpoint: Option<pulsebeam_proto::data_control::PublisherBinding>,
}

pub(crate) struct PublisherSource {
    source: crate::shard::router::FabricSourceHandle,
    installation: pulsebeam_rtc::DataChannelInstallation,
    reliable_endpoint: Option<PublisherBinding>,
    ledger: Source,
}

impl Input {
    pub(super) fn from_publisher(
        source: crate::shard::router::FabricSourceHandle,
        binding: InstalledBinding,
    ) -> Option<Self> {
        let Endpoint::Publisher(endpoint) = binding.endpoint else {
            return None;
        };
        if binding.channel.stream.get() != binding.channel.association.channel().value() {
            return None;
        }
        Some(Self {
            source,
            installation: binding.channel.association,
            reliable_endpoint: (binding.format.delivery() == Delivery::Reliable)
                .then_some(endpoint),
        })
    }
}

impl PublisherSource {
    pub(crate) fn new(
        source: crate::shard::router::FabricSourceHandle,
        binding: InstalledBinding,
        clock: Option<Clock<'_>>,
    ) -> Option<Self> {
        let input = Input::from_publisher(source, binding)?;
        Some(Self {
            source,
            installation: input.installation,
            reliable_endpoint: input.reliable_endpoint,
            ledger: Source::new(clock),
        })
    }

    pub(crate) fn expiry(&self) -> Option<Instant> {
        self.ledger.next_expiry
    }

    pub(crate) fn expire(&mut self, fanout: &mut SourceFanout, now: Instant) {
        self.ledger.expire(fanout, now);
    }

    pub(crate) fn matches(&self, installation: pulsebeam_rtc::DataChannelInstallation) -> bool {
        self.installation == installation
    }

    pub(crate) fn sequence_anchor(&self) -> Option<u64> {
        (!self.ledger.fenced).then_some(self.ledger.next_sequence)
    }

    pub(crate) fn cached_original<'a>(
        &self,
        fanout: &'a SourceFanout,
        unit_seq: u64,
        index: u16,
        now: Instant,
    ) -> Option<(u64, &'a OwnedPacket, Instant)> {
        let Original::Cached {
            sequence,
            packet,
            expires,
        } = self.ledger.original(fanout, unit_seq, index, now)
        else {
            return None;
        };
        Some((sequence, packet, expires))
    }

    pub(crate) fn request_original(
        &mut self,
        fanout: &SourceFanout,
        unit_seq: u64,
        index: u16,
        now: Instant,
    ) -> OriginalResolution {
        let Some(binding) = self.reliable_endpoint else {
            return OriginalResolution::Revoked;
        };
        self.ledger
            .request_original(fanout, binding, unit_seq, index, now)
    }

    pub(crate) fn original_unavailable(
        &mut self,
        fanout: &SourceFanout,
        demand: OriginalDemand,
        now: Instant,
    ) -> bool {
        self.ledger.original_unavailable(fanout, demand, now)
    }

    pub(crate) fn install_capture_reference(
        &mut self,
        clock: Clock<'_>,
        domain: ClockDomain,
        origin: Instant,
        host_generation: u64,
    ) -> bool {
        self.ledger
            .install_capture_reference(clock, domain, origin, host_generation)
    }

    pub(crate) fn publish(
        &mut self,
        fanout: &mut SourceFanout,
        bytes: &[u8],
        now: Instant,
        stamp: u32,
        time: DataTime,
    ) -> Result<super::event::FabricDataPosition, Error> {
        // Protected SCTP DD is not qualified for this binding.
        let admission = self
            .ledger
            .admit_record(fanout, bytes, now, stamp, time, false)?;
        #[cfg(feature = "sim")]
        if let Some(endpoint) = self.reliable_endpoint {
            crate::sim_metrics::record_wan_data_sctp(
                endpoint.source.get(),
                admission.unit.unit_seq,
                admission.index,
                bytes,
                true,
            );
        }
        Ok(super::event::FabricDataPosition {
            source: self.source,
            installation: self.installation,
            arrival: now,
            sequence: admission.sequence,
            reliable: self.reliable_endpoint.map(|_| ReliableAdmission {
                unit: crate::route::fabric::local::DataExpectation {
                    unit_seq: admission.unit.unit_seq,
                    fragment_count: admission.unit.fragment_count,
                },
                expires: admission.unit.expires,
                acknowledge: admission.needs_source_ack(),
                fragment_index: admission.index,
                installation: self.installation,
            }),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UnitShape {
    len: u32,
    fragments: u16,
    capture: Option<(i64, Option<u32>)>,
}

impl From<data_record::Header<'_>> for UnitShape {
    fn from(header: data_record::Header<'_>) -> Self {
        Self {
            len: header.unit_len,
            fragments: header.count,
            capture: header
                .capture
                .map(|value| (value.nanos, value.uncertainty_ns)),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Original<'a> {
    Cached {
        sequence: u64,
        packet: &'a OwnedPacket,
        expires: Instant,
    },
    Unseen {
        expires: Instant,
    },
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OriginalResolution {
    Cached { sequence: u64, expires: Instant },
    Request(OriginalDemand),
    Pending,
    Unavailable,
    Exhausted,
    Revoked,
}

#[derive(Default)]
struct OriginalRequests {
    last: u64,
    entries: BTreeMap<(u64, u16), PendingOriginal>,
}

struct PendingOriginal {
    request_id: u64,
    expires: Instant,
    retry_at: Instant,
    unavailable: bool,
}

struct OpenUnit {
    id: u64,
    shape: UnitShape,
    timestamp: u32,
    expires: Instant,
    charge: usize,
    fragments: Box<[Option<u64>]>,
}

#[derive(Default)]
enum Slot {
    #[default]
    Empty,
    Closed(u64),
    Open(Box<OpenUnit>),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Admission<'a> {
    pub sequence: u64,
    pub packet: &'a OwnedPacket,
    pub new_fragment: bool,
    pub new_unit: bool,
    unit: AdmittedUnit,
    index: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AdmittedUnit {
    pub unit_seq: u64,
    pub fragment_count: u16,
    pub expires: Instant,
}

impl Admission<'_> {
    pub(super) fn first_unit(&self) -> Option<AdmittedUnit> {
        self.new_unit.then_some(self.unit)
    }

    fn needs_source_ack(&self) -> bool {
        self.new_unit || !self.new_fragment || self.index == 0
    }

    #[cfg(test)]
    pub(super) fn source_ack(
        &self,
        binding: pulsebeam_proto::data_control::PublisherBinding,
    ) -> Option<pulsebeam_proto::data_control::PublisherObservation> {
        use pulsebeam_proto::data_control::{
            PublisherObservation, SourceUnit, publisher_observation,
        };
        self.needs_source_ack().then_some(PublisherObservation {
            record: Some(publisher_observation::Record::Admission(SourceUnit {
                source_incarnation: binding.source.get(),
                unit_seq: Some(self.unit.unit_seq),
            })),
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct DataTime {
    fallback: u32,
    reference: Option<crate::clock::reference::Estimate>,
}

impl DataTime {
    pub(crate) fn from_reference(reference: Option<crate::clock::reference::Estimate>) -> Self {
        let fallback = reference
            .and_then(|value| ClockRate::DATA.timestamp(value.global).ok())
            .map_or(0, |tick| tick.0);
        Self {
            fallback,
            reference,
        }
    }
}

/// Calibration belongs to this source incarnation and the record clock bound
/// at publication. It is installed only from verified source evidence, never
/// from capture stamps or from sharing an SCTP association.
struct CaptureReference {
    domain: ClockDomain,
    origin: Instant,
    host_generation: u64,
}

impl CaptureReference {
    fn timestamp(&self, capture: (i64, u32), now: Instant, time: DataTime) -> Option<u32> {
        let reference = time.reference?;
        if reference.evidence_generation != self.host_generation {
            return None;
        }
        let uncertainty = i64::try_from(reference.uncertainty_ns?).ok()?;
        let latest = reference
            .global
            .checked_add(GlobalDuration::from_nanos(uncertainty))
            .ok()?;
        let at = now.checked_duration_since(self.origin)?;
        let at = MonotonicTime::from_nanos(u64::try_from(at.as_nanos()).ok()?);
        let projected = self
            .domain
            .project_data(
                Some(DataCapture {
                    binding: self.domain.binding(),
                    source: SourceTime::from_nanos(capture.0),
                    uncertainty_ns: u64::from(capture.1),
                }),
                DataReceipt {
                    at,
                    latest_global: Some(latest),
                },
                at,
            )
            .ok()?;
        (projected.provenance == Provenance::DataCapture
            && projected.quality == Quality::Synchronized
            && projected.absolute_uncertainty_ns.is_some())
        .then_some(projected.rtp.0)
    }
}

/// One admitted source incarnation. The binding owner must match the exact
/// authenticated publish channel before admitting a fragment.
pub(super) struct Source {
    clock: Option<(Box<str>, u64)>,
    capture_reference: Option<Box<CaptureReference>>,
    slots: Box<[Slot; WINDOW]>,
    max_seen: Option<u64>,
    next_sequence: u64,
    next_expiry: Option<Instant>,
    live: usize,
    charged: usize,
    fenced: bool,
    originals: Option<Box<OriginalRequests>>,
}

impl Source {
    fn install_capture_reference(
        &mut self,
        clock: Clock<'_>,
        domain: ClockDomain,
        origin: Instant,
        host_generation: u64,
    ) -> bool {
        if self.fenced
            || !self
                .clock
                .as_ref()
                .is_some_and(|(id, epoch)| id.as_ref() == clock.id && *epoch == clock.epoch)
        {
            return false;
        }
        self.capture_reference = Some(Box::new(CaptureReference {
            domain,
            origin,
            host_generation,
        }));
        true
    }

    fn new(clock: Option<Clock<'_>>) -> Self {
        let clock: Option<(Box<str>, u64)> = clock.map(|value| (Box::from(value.id), value.epoch));
        let charged = size_of::<Self>()
            .saturating_add(size_of::<[Slot; WINDOW]>())
            .saturating_add(clock.as_ref().map_or(0, |(id, _)| id.len()));
        Self {
            clock,
            capture_reference: None,
            slots: Box::new(core::array::from_fn(|_| Slot::Empty)),
            max_seen: None,
            next_sequence: 0,
            next_expiry: None,
            live: 0,
            charged,
            fenced: false,
            originals: None,
        }
    }

    fn original<'a>(
        &self,
        fanout: &'a SourceFanout,
        unit_seq: u64,
        index: u16,
        now: Instant,
    ) -> Original<'a> {
        let Some(Slot::Open(unit)) = slot_index(unit_seq)
            .ok()
            .and_then(|slot| self.slots.get(slot))
        else {
            return Original::Unavailable;
        };
        if self.fenced || unit.id != unit_seq || now >= unit.expires {
            return Original::Unavailable;
        }
        match unit.fragments.get(usize::from(index)) {
            Some(Some(sequence)) => {
                fanout
                    .endpoint_data(*sequence)
                    .map_or(Original::Unavailable, |packet| Original::Cached {
                        sequence: *sequence,
                        packet,
                        expires: unit.expires,
                    })
            }
            Some(None) => Original::Unseen {
                expires: unit.expires,
            },
            None => Original::Unavailable,
        }
    }

    fn request_original(
        &mut self,
        fanout: &SourceFanout,
        binding: PublisherBinding,
        unit_seq: u64,
        index: u16,
        now: Instant,
    ) -> OriginalResolution {
        let expires = match self.original(fanout, unit_seq, index, now) {
            Original::Cached {
                sequence, expires, ..
            } => {
                return OriginalResolution::Cached { sequence, expires };
            }
            Original::Unseen { expires } => expires,
            Original::Unavailable => return OriginalResolution::Unavailable,
        };
        let requests = self.originals.get_or_insert_with(Default::default);
        requests.entries.retain(|_, entry| now < entry.expires);
        let key = (unit_seq, index);
        let retry_at = now
            .checked_add(ORIGINAL_RETRY)
            .unwrap_or(expires)
            .min(expires);
        let request_id = if let Some(entry) = requests.entries.get_mut(&key) {
            if entry.unavailable {
                return OriginalResolution::Unavailable;
            }
            if now < entry.retry_at {
                return OriginalResolution::Pending;
            }
            entry.retry_at = retry_at;
            entry.request_id
        } else {
            if requests.entries.len() >= UNIT_LIMIT {
                return OriginalResolution::Exhausted;
            }
            let Some(request_id) = requests.last.checked_add(1) else {
                return OriginalResolution::Exhausted;
            };
            requests.last = request_id;
            requests.entries.insert(
                key,
                PendingOriginal {
                    request_id,
                    expires,
                    retry_at,
                    unavailable: false,
                },
            );
            request_id
        };
        OriginalResolution::Request(OriginalDemand {
            unit: Some(SourceUnit {
                source_incarnation: binding.source.get(),
                unit_seq: Some(unit_seq),
            }),
            fragment_index: Some(u32::from(index)),
            request_id,
        })
    }

    fn original_unavailable(
        &mut self,
        fanout: &SourceFanout,
        demand: OriginalDemand,
        now: Instant,
    ) -> bool {
        let (Some(unit_seq), Some(index)) = (
            demand.unit.and_then(|unit| unit.unit_seq),
            demand
                .fragment_index
                .and_then(|index| u16::try_from(index).ok()),
        ) else {
            return false;
        };
        if !matches!(
            self.original(fanout, unit_seq, index, now),
            Original::Unseen { .. }
        ) {
            return false;
        }
        let Some(entry) = self
            .originals
            .as_mut()
            .and_then(|requests| requests.entries.get_mut(&(unit_seq, index)))
        else {
            return false;
        };
        if demand.request_id != entry.request_id || entry.unavailable || now >= entry.expires {
            return false;
        }
        entry.unavailable = true;
        true
    }

    fn admit_record<'a>(
        &mut self,
        fanout: &'a mut SourceFanout,
        bytes: &[u8],
        now: Instant,
        ingress_ntp32: u32,
        time: DataTime,
        allow_descriptor: bool,
    ) -> Result<Admission<'a>, Error> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        let clock = self
            .clock
            .as_ref()
            .map(|(id, epoch)| Clock { id, epoch: *epoch });
        let record = if allow_descriptor {
            Record::decode(bytes, clock)
        } else {
            Record::decode_without_descriptor(bytes, clock)
        }
        .map_err(Error::Record)?;
        let descriptor = record.dependency_descriptor();
        let mut projected;
        let payload = if descriptor.is_some() {
            projected = [0; data_record::MAX_RECORD_BYTES];
            let len = record
                .without_dependency_descriptor()
                .write(&mut projected)
                .map_err(Error::Record)?;
            projected.get(..len).ok_or(Error::Projection)?
        } else {
            bytes
        };
        let header = record.header();
        let shape = UnitShape::from(header);
        let capture = header
            .capture
            .and_then(|capture| Some((capture.nanos, capture.uncertainty_ns?)));
        let marker = record.final_fragment();
        let unit_id = header.unit_seq;
        let fragment_index = header.index;
        let index = usize::from(fragment_index);
        self.expire(fanout, now);
        if self
            .max_seen
            .is_some_and(|max| unit_id < max.saturating_sub(WINDOW_MASK))
        {
            return Err(Error::Expired);
        }
        let slot = slot_index(unit_id)?;
        let current = self.slots.get(slot).ok_or(Error::Sequence)?;
        let (is_new, timestamp) = match current {
            Slot::Closed(id) if *id == unit_id => return Err(Error::Expired),
            Slot::Open(unit) if unit.id == unit_id => {
                if let Some(sequence) = unit.fragments.get(index).and_then(Option::as_ref) {
                    let packet = fanout.endpoint_data(*sequence).ok_or(Error::Sequence)?;
                    if packet.payload() != payload || packet_descriptor(packet) != descriptor {
                        self.fenced = true;
                        return Err(Error::SourceConflict);
                    }
                    return self.admission(fanout, slot, index, false, false);
                }
                if unit.shape != shape {
                    self.close(fanout, slot);
                    return Err(Error::UnitConflict);
                }
                (false, unit.timestamp)
            }
            _ => (
                true,
                capture
                    .and_then(|capture| {
                        self.capture_reference
                            .as_ref()?
                            .timestamp(capture, now, time)
                    })
                    .unwrap_or(time.fallback),
            ),
        };

        let (old_floor, new_floor) = self.floors(unit_id);
        let unit_charge = if is_new {
            size_of::<OpenUnit>().saturating_add(
                usize::from(shape.fragments).saturating_mul(size_of::<Option<u64>>()),
            )
        } else {
            0
        };
        // The endpoint length covers the retained payload and DD value. A
        // two-byte element header can cost one more byte than the record field.
        let fragment_charge = bytes
            .len()
            .saturating_add(usize::from(descriptor.is_some_and(|dd| dd.len() > 16)))
            .saturating_add(size_of::<(u64, OwnedPacket)>());
        let charge = unit_charge.saturating_add(fragment_charge);
        let (mut remaining_live, mut remaining_bytes) = (self.live, self.charged);
        for old in old_floor..new_floor {
            if let Some(Slot::Open(unit)) = self.slots.get(slot_index(old)?)
                && unit.id == old
            {
                remaining_live = remaining_live.saturating_sub(1);
                remaining_bytes = remaining_bytes.saturating_sub(unit.charge);
            }
        }
        if (is_new && remaining_live >= UNIT_LIMIT)
            || remaining_bytes.saturating_add(charge) > BYTE_LIMIT
        {
            return Err(Error::Capacity);
        }
        let sequence = self.next_sequence;
        let next_sequence = sequence.checked_add(1).ok_or(Error::Sequence)?;
        let expires = now.checked_add(LIFETIME).ok_or(Error::Sequence)?;
        let extension = descriptor.map(|value| Extension {
            kind: ExtensionKind::DependencyDescriptor,
            value,
        });
        let packet = FabricPacket::new(
            Header {
                profile: Profile::Data,
                sequence: low32(sequence),
                timestamp,
                marker,
                ingress_ntp32,
            },
            extension.as_slice(),
            Box::from(payload),
        )
        .map_err(Error::Fabric)?;

        for old in old_floor..new_floor {
            let slot = slot_index(old)?;
            self.close(fanout, slot);
            if let Some(entry) = self.slots.get_mut(slot) {
                *entry = Slot::Empty;
            }
        }
        let entry = self.slots.get_mut(slot).ok_or(Error::Sequence)?;
        if is_new {
            *entry = Slot::Open(Box::new(OpenUnit {
                id: unit_id,
                shape,
                timestamp,
                expires,
                charge: unit_charge,
                fragments: (0..shape.fragments).map(|_| None).collect(),
            }));
            self.next_expiry = Some(
                self.next_expiry
                    .map_or(expires, |deadline| deadline.min(expires)),
            );
            self.live = self.live.saturating_add(1);
        }
        let Slot::Open(unit) = entry else {
            return Err(Error::Sequence);
        };
        let target = unit.fragments.get_mut(index).ok_or(Error::Sequence)?;
        *target = Some(sequence);
        fanout.retain_endpoint_data(sequence, packet);
        unit.charge = unit.charge.saturating_add(fragment_charge);
        self.charged = self.charged.saturating_add(charge);
        self.max_seen = Some(self.max_seen.map_or(unit_id, |max| max.max(unit_id)));
        self.next_sequence = next_sequence;
        if let Some(requests) = &mut self.originals {
            requests.entries.remove(&(unit_id, fragment_index));
        }
        self.admission(fanout, slot, index, true, is_new)
    }

    fn admission<'a>(
        &self,
        fanout: &'a SourceFanout,
        slot: usize,
        index: usize,
        new_fragment: bool,
        new_unit: bool,
    ) -> Result<Admission<'a>, Error> {
        let Some(Slot::Open(unit)) = self.slots.get(slot) else {
            return Err(Error::Sequence);
        };
        let &sequence = unit
            .fragments
            .get(index)
            .and_then(Option::as_ref)
            .ok_or(Error::Sequence)?;
        Ok(Admission {
            sequence,
            packet: fanout.endpoint_data(sequence).ok_or(Error::Sequence)?,
            new_fragment,
            new_unit,
            unit: AdmittedUnit {
                unit_seq: unit.id,
                fragment_count: unit.shape.fragments,
                expires: unit.expires,
            },
            index: u16::try_from(index).map_err(|_| Error::Sequence)?,
        })
    }

    // At most one window of slots becomes newly ineligible at a time. Probe
    // them before committing so a failed reservation never advances max_seen.
    fn floors(&self, candidate: u64) -> (u64, u64) {
        self.max_seen.map_or((0, 0), |old| {
            let low = old.saturating_sub(WINDOW_MASK);
            let high = old
                .max(candidate)
                .saturating_sub(WINDOW_MASK)
                .min(old.saturating_add(1));
            (low, high)
        })
    }

    fn close(&mut self, fanout: &mut SourceFanout, index: usize) {
        let Some(entry) = self.slots.get_mut(index) else {
            return;
        };
        let Slot::Open(unit) = entry else { return };
        self.live = self.live.saturating_sub(1);
        self.charged = self.charged.saturating_sub(unit.charge);
        for sequence in unit.fragments.iter().flatten() {
            fanout.release_endpoint_data(*sequence);
        }
        *entry = Slot::Closed(unit.id);
    }

    fn expire(&mut self, fanout: &mut SourceFanout, now: Instant) {
        if self.next_expiry.is_none_or(|deadline| now < deadline) {
            return;
        }
        let mut earliest = None::<Instant>;
        for index in 0..WINDOW {
            if let Some(Slot::Open(unit)) = self.slots.get(index) {
                if now >= unit.expires {
                    self.close(fanout, index);
                } else {
                    earliest =
                        Some(earliest.map_or(unit.expires, |previous| previous.min(unit.expires)));
                }
            }
        }
        self.next_expiry = earliest;
    }
}

fn slot_index(unit_id: u64) -> Result<usize, Error> {
    usize::try_from(unit_id & WINDOW_MASK).map_err(|_| Error::Sequence)
}

fn low32(value: u64) -> u32 {
    let [a, b, c, d, ..] = value.to_le_bytes();
    u32::from_le_bytes([a, b, c, d])
}

/// Strip precisely the canonical envelope. Authorization and last-handoff
/// binding checks belong to the destination owner before SCTP enqueue.
#[cfg(test)]
pub(super) fn project<'a>(
    packet: &'a FabricPacket<impl AsRef<[u8]>, impl AsRef<[u8]>>,
    clock: Option<Clock<'_>>,
) -> Result<&'a [u8], Error> {
    project_sctp_record(packet, clock, None)?;
    Ok(packet.payload())
}

fn packet_descriptor<P: AsRef<[u8]>, E: AsRef<[u8]>>(packet: &FabricPacket<P, E>) -> Option<&[u8]> {
    packet
        .extensions()
        .find(|ext| ext.kind == ExtensionKind::DependencyDescriptor)
        .map(|ext| ext.value)
}

fn project_sctp_record<'data, 'clock>(
    packet: &'data FabricPacket<impl AsRef<[u8]>, impl AsRef<[u8]>>,
    clock: Option<Clock<'clock>>,
    repair: Option<crate::route::fabric::local::RepairFragment>,
) -> Result<Record<'data, 'clock>, Error> {
    let record = project_record(packet, clock, repair)?;
    if record.dependency_descriptor().is_some() {
        return Err(Error::Projection);
    }
    Ok(record)
}

fn project_record<'data, 'clock>(
    packet: &'data FabricPacket<impl AsRef<[u8]>, impl AsRef<[u8]>>,
    clock: Option<Clock<'clock>>,
    repair: Option<crate::route::fabric::local::RepairFragment>,
) -> Result<Record<'data, 'clock>, Error> {
    let header = packet.header();
    if header.profile != Profile::Data {
        return Err(Error::Projection);
    }
    let record =
        Record::decode_without_descriptor(packet.payload(), clock).map_err(Error::Record)?;
    if header.marker != record.final_fragment()
        || repair.is_some_and(|repair| {
            repair.unit_seq != record.header().unit_seq || repair.index != record.header().index
        })
    {
        return Err(Error::Projection);
    }
    match packet_descriptor(packet) {
        None => Ok(record),
        Some(value) => record
            .with_dependency_descriptor(value)
            .map_err(Error::Record),
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::{R4, admit_dd_fixture, maximum_dd_bytes, r3_record, reconstruct_dd_fixture};
