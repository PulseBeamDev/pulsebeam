use std::{cmp::Ordering, num::NonZeroU64};

use pulsebeam_runtime::time::Instant;

use crate::{entity::PublicationKey, id::ShardId, route::NodeRouteAddress};

pub(crate) mod hop_expectations;
pub mod local;
pub(crate) mod network_repair;
pub(crate) mod relay;

/// The destination installs this exact source incarnation before accepting
/// in-process offers. No publishing or receiving shard handle crosses the lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShardBinding {
    pub destination: NodeRouteAddress,
    pub publication: PublicationKey,
    pub origin: ShardId,
    pub incarnation: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RtpSourceEncoding {
    pub primary: crate::media::Ssrc,
    pub rid: Option<crate::media::Rid>,
    pub payload_type: crate::media::Pt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShardDataReceiver {
    pub(crate) connection: crate::entity::ConnectionId,
    pub(crate) branch: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShardDataReady {
    pub(crate) binding: ShardBinding,
    pub(crate) receiver: ShardDataReceiver,
    pub(crate) reference: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ShardDataExpectation {
    pub(crate) binding: ShardBinding,
    pub(crate) unit: local::DataExpectation,
    pub(crate) expires: Instant,
    pub(crate) receivers: Box<[ShardDataReceiver]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShardDataReceipt {
    pub(crate) binding: ShardBinding,
    pub(crate) unit: local::DataExpectation,
    pub(crate) branch: NonZeroU64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ShardDataRepair {
    pub(crate) binding: ShardBinding,
    pub(crate) receiver: ShardDataReceiver,
    pub(crate) request: local::RepairRequest,
}

#[derive(Clone, Debug)]
pub(crate) struct ShardDataRepairImage {
    pub(crate) repair: ShardDataRepair,
    pub(crate) sequence: u64,
    pub(crate) packet: local::OwnedPacket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DataPublisher {
    pub publication: crate::entity::PublicationKey,
    pub connection: crate::entity::ConnectionId,
    pub incarnation: NonZeroU64,
}

/// One destination of a live receive route. Association and full key are
/// supplied by Link; the remaining identity comes from the installed plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveNetworkChild {
    pub ingress: pulsebeam_link::FabricIngress,
    pub destination: NodeRouteAddress,
    pub publication: PublicationKey,
    pub incarnation: NonZeroU64,
    pub major: u8,
    pub revision: u8,
    pub sequence_anchor: u64,
    pub emission: local::Emission,
    pub window: Option<pulsebeam_core::fabric::wan::Window>,
}

#[derive(Clone, Debug)]
pub(crate) struct NetworkPacket {
    pub(crate) window: pulsebeam_core::fabric::wan::Window,
    pub(crate) position: u64,
    pub(crate) packet: local::OwnedPacket,
}

impl From<pulsebeam_link::wan::Received<local::OwnedPacket, pulsebeam_core::fabric::wan::Window>>
    for NetworkPacket
{
    fn from(
        input: pulsebeam_link::wan::Received<
            local::OwnedPacket,
            pulsebeam_core::fabric::wan::Window,
        >,
    ) -> Self {
        Self {
            window: input.route.descriptor(),
            position: input.position,
            packet: input.packet,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NetworkDataExpectation {
    pub(crate) parent: LiveNetworkChild,
    pub(crate) unit: local::DataExpectation,
    pub(crate) expires: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct LiveNextHop {
    pub peer: pulsebeam_link::NetworkId,
    pub key: pulsebeam_link::NodeRouteKey,
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledPublisherSend {
    pub publisher: crate::shard_update::PublisherAnchor,
    pub shard: crate::id::ShardId,
    pub next: LiveNextHop,
    pub expires_at: Instant,
    pub tx: crate::link::wan_host::WanTx,
}

impl hop_expectations::Path for CompiledPublisherSend {
    fn matches(&self, other: &Self) -> bool {
        self.next == other.next
            && self.shard == other.shard
            && self.publisher == other.publisher
            && self.tx == other.tx
    }

    fn deadline(&self) -> Instant {
        self.expires_at
    }
}

pub(crate) type PublisherExpectation = hop_expectations::Notice<CompiledPublisherSend>;
pub(crate) type PublisherExpectationStep = hop_expectations::Step<CompiledPublisherSend>;

#[derive(Debug)]
pub(crate) struct PublisherObservation {
    pub route: CompiledPublisherSend,
    pub unit: local::DataExpectation,
    pub outcome: local::PropagationOutcome,
}

#[derive(Debug)]
pub(crate) enum PublisherSendChange {
    Install {
        route: CompiledPublisherSend,
        lifetime: pulsebeam_link::BindingCutoff,
    },
    Retire(CompiledPublisherSend),
}

impl PublisherSendChange {
    pub(crate) fn route(&self) -> &CompiledPublisherSend {
        match self {
            Self::Install { route, .. } | Self::Retire(route) => route,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledLiveRelay {
    pub input: LiveNetworkChild,
    pub next: LiveNextHop,
    pub expires_at: Instant,
    pub tx: crate::link::wan_host::WanTx,
}

#[derive(Debug)]
pub(crate) enum LiveRelayChange {
    Install(CompiledLiveRelay),
    Retire(CompiledLiveRelay),
}

impl LiveRelayChange {
    pub(crate) fn route(&self) -> &CompiledLiveRelay {
        match self {
            Self::Install(route) | Self::Retire(route) => route,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ShardOffer {
    pub binding: ShardBinding,
    pub arrival: Instant,
    pub ready_at: Option<std::time::Instant>,
    pub sequence: u64,
    pub packet: local::OwnedPacket,
}

const POSITIONS: u64 = 4096;
const WORD_BITS: u64 = 64;
const WORDS: usize = 64;
const _: () = assert!(WORDS as u64 * WORD_BITS == POSITIONS);

/// Live admission ledger for exactly one installed source and branch incarnation.
/// The shard owns it alongside the branch's output queue. A replacement branch
/// must start with a new ledger and fence the old queue before accepting work.
pub struct LiveAdmissions {
    high: Option<u64>,
    bits: [u64; WORDS],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveOffer {
    Enqueued,
    Refused,
    Duplicate,
    Stale,
    AheadOfSource,
}

impl Default for LiveAdmissions {
    fn default() -> Self {
        Self {
            high: None,
            bits: [0; WORDS],
        }
    }
}

impl LiveAdmissions {
    /// `source_high` is the canonical identity owner's validated high-water
    /// mark, not a wire sequence or a number supplied by the output. Advance
    /// retirement even when the queue is full, but consume this position only
    /// if `enqueue` returns true. The closure must not publish on refusal.
    pub fn offer(
        &mut self,
        source_high: u64,
        sequence: u64,
        enqueue: impl FnOnce() -> bool,
    ) -> LiveOffer {
        if sequence > source_high {
            return LiveOffer::AheadOfSource;
        }
        self.advance(source_high);
        if self
            .high
            .is_some_and(|high| high.saturating_sub(sequence) >= POSITIONS)
        {
            return LiveOffer::Stale;
        }
        let (word, bit) = slot(sequence);
        let Some(admitted) = self.bits.get_mut(word) else {
            return LiveOffer::Stale;
        };
        if *admitted & bit != 0 {
            return LiveOffer::Duplicate;
        }
        if !enqueue() {
            return LiveOffer::Refused;
        }
        *admitted |= bit;
        LiveOffer::Enqueued
    }

    fn advance(&mut self, source_high: u64) {
        if let Some(high) = self.high {
            match source_high.saturating_sub(high).cmp(&POSITIONS) {
                Ordering::Less if source_high <= high => return,
                Ordering::Less => self.clear_entering_slots(high, source_high),
                Ordering::Equal | Ordering::Greater => self.bits.fill(0),
            }
        }
        self.high = Some(source_high);
    }

    fn clear_entering_slots(&mut self, high: u64, source_high: u64) {
        let mut next = high.saturating_add(1);
        let mut remaining = source_high.saturating_sub(high);
        while remaining != 0 {
            let word = word_index(next);
            let offset = next & (WORD_BITS - 1);
            let count = remaining.min(WORD_BITS.saturating_sub(offset));
            let mask = (u64::MAX >> WORD_BITS.saturating_sub(count)) << offset;
            if let Some(bits) = self.bits.get_mut(word) {
                *bits &= !mask;
            }
            remaining = remaining.saturating_sub(count);
            next = next.saturating_add(count);
        }
    }
}

fn word_index(sequence: u64) -> usize {
    let position = sequence & (POSITIONS - 1);
    usize::try_from(position / WORD_BITS).unwrap_or_default()
}

fn slot(sequence: u64) -> (usize, u64) {
    (word_index(sequence), 1 << (sequence & (WORD_BITS - 1)))
}

#[cfg(test)]
mod tests;
