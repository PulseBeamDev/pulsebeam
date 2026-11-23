//! Shared SANS-I/O DATA_V1 fragment and complete-unit owners.
//!
//! Hosts select this path from the installed binding, never from payload bytes.
//! `pulsebeam_proto::data_record` owns the binary format and capture validation.

use alloc::{collections::BTreeMap, vec, vec::Vec};
use core::{num::NonZeroU64, time::Duration};

use pulsebeam_proto::{
    data_control::{
        self, Record as _, SubscriberBinding, SubscriberControl, SubscriberObservation,
        subscriber_control, subscriber_observation,
    },
    data_endpoint::{Binding, Delivery, Endpoint},
    data_record::{self, Capture, Clock, FRAGMENT_BYTES, Header, Record},
};

use crate::ffi::{CaptureClock, CaptureTimestamp, DataSendOptions};

mod admission;
mod channel;
pub use admission::{
    AdmissionAction, AdmittedUnit, PublisherDelivery, ReliablePublisher, TerminalDelivery,
};
pub use channel::{
    Channel, Progress, PublisherControl, ReceiveChannel, ReceiverControl, ReceiverReceipt,
    SendChannel,
};

const WINDOW: u64 = 512;
const UNIT_LIMIT: usize = 64;
const BYTE_LIMIT: usize = 4 * 1024 * 1024;
const LIFETIME: Duration = Duration::from_secs(2);
const CONTROL_RETRY: Duration = Duration::from_millis(100);
const REPAIR_SPACING: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Binding,
    Record(data_record::Error),
    Control(data_control::Error),
    Exhausted,
    Retired,
    Capacity,
    Conflict,
}

fn bound_clock(clock: Option<&CaptureClock>) -> Option<Clock<'_>> {
    clock.map(|clock| Clock {
        id: &clock.id,
        epoch: clock.epoch,
    })
}

pub struct Publisher<A> {
    binding: Binding<A>,
    next: Option<u64>,
    clock: Option<CaptureClock>,
}

impl<A: Copy + Eq> Publisher<A> {
    pub fn new(
        binding: Binding<A>,
        clock: Option<CaptureClock>,
        first: u64,
    ) -> Result<Self, Error> {
        if binding.format.delivery() == Delivery::Reliable {
            return Err(Error::Binding);
        }
        Self::for_binding(binding, clock, first)
    }

    fn for_binding(
        binding: Binding<A>,
        clock: Option<CaptureClock>,
        first: u64,
    ) -> Result<Self, Error> {
        if !matches!(binding.endpoint, Endpoint::Publisher(_)) {
            return Err(Error::Binding);
        }
        Ok(Self {
            binding,
            next: Some(first),
            clock,
        })
    }

    /// Transfer ownership of one unit after the caller reserves its queue credit.
    /// Rejections do not consume a sequence. This is local admission, not a receipt.
    pub fn admit(
        &mut self,
        binding: Binding<A>,
        bytes: Vec<u8>,
        options: DataSendOptions,
    ) -> Result<OutgoingUnit, Error> {
        if binding != self.binding {
            return Err(Error::Binding);
        }
        let seq = self.next.ok_or(Error::Exhausted)?;
        let count = data_record::fragment_count(bytes.len()).map_err(Error::Record)?;
        if options
            .captured_at
            .as_ref()
            .is_some_and(|capture| Some(&capture.clock) != self.clock.as_ref())
        {
            return Err(Error::Record(data_record::Error::Clock));
        }
        self.next = seq.checked_add(1);
        Ok(OutgoingUnit {
            seq,
            count,
            bytes,
            captured_at: options.captured_at,
        })
    }
}

pub struct OutgoingUnit {
    seq: u64,
    count: u16,
    bytes: Vec<u8>,
    captured_at: Option<CaptureTimestamp>,
}

impl OutgoingUnit {
    pub const fn unit_seq(&self) -> u64 {
        self.seq
    }
    pub const fn count(&self) -> u16 {
        self.count
    }

    /// Serialize exactly one fragment into caller-owned transport storage.
    pub fn write(&self, index: u16, out: &mut [u8]) -> Result<usize, Error> {
        if index >= self.count {
            return Err(Error::Record(data_record::Error::FragmentIndex));
        }
        let offset = usize::from(index).saturating_mul(FRAGMENT_BYTES);
        let end = offset.saturating_add(FRAGMENT_BYTES).min(self.bytes.len());
        let bytes = self
            .bytes
            .get(offset..end)
            .ok_or(Error::Record(data_record::Error::FragmentIndex))?;
        let unit_len = u32::try_from(self.bytes.len())
            .map_err(|_| Error::Record(data_record::Error::UnitLength))?;
        let capture = self.captured_at.as_ref().map(|value| Capture {
            clock: Clock {
                id: &value.clock.id,
                epoch: value.clock.epoch,
            },
            nanos: value.nanos,
            uncertainty_ns: value.uncertainty_ns,
        });
        Record::new(
            Header {
                unit_seq: self.seq,
                index,
                count: self.count,
                unit_len,
                capture,
            },
            bytes,
            capture.map(|value| value.clock),
        )
        .and_then(|record| record.write(out))
        .map_err(Error::Record)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssembledUnit {
    pub unit_seq: u64,
    pub captured_at: Option<CaptureTimestamp>,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Shape {
    len: usize,
    count: u16,
    capture: Option<(i64, Option<u32>)>,
}

struct Partial {
    shape: Shape,
    bytes: Vec<u8>,
    received: u64,
}

struct OpenUnit {
    expires: Duration,
    expected: Option<u16>,
    partial: Option<Partial>,
    repair_after: Option<Duration>,
    requested: u64,
}

impl OpenUnit {
    fn bytes(&self) -> usize {
        self.partial.as_ref().map_or(0, |partial| partial.shape.len)
    }

    fn missing_fragments(&self) -> Option<u64> {
        let expected = u64::MAX.checked_shr(u64::BITS.checked_sub(u32::from(self.expected?))?)?;
        Some(expected & !self.partial.as_ref().map_or(0, |partial| partial.received))
    }

    fn repair_deadline(&self, paced_at: Duration) -> Option<Duration> {
        let at = self.repair_after?.max(paced_at);
        (at < self.expires && self.missing_fragments()? & !self.requested != 0).then_some(at)
    }
}

enum Slot {
    Open(OpenUnit),
    Closed,
    Delivered(u16),
}

struct PendingReceipt {
    count: u16,
    expires: Duration,
    retry_at: Option<Duration>,
}

impl PendingReceipt {
    fn deadline(&self) -> Duration {
        self.retry_at
            .map_or(Duration::ZERO, |retry| retry.min(self.expires))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ReceiptAction {
    Send(SubscriberControl),
    Unconfirmed { unit_seq: u64 },
}

pub struct Subscriber<A> {
    binding: Binding<A>,
    clock: Option<CaptureClock>,
    now: Duration,
    max_seen: Option<u64>,
    latest: Option<u64>,
    next_expiry: Option<Duration>,
    slots: BTreeMap<u64, Slot>,
    receipts: BTreeMap<u64, PendingReceipt>,
    next_request: Option<NonZeroU64>,
    repair_paced_at: Duration,
    active: usize,
    bytes: usize,
}

impl<A: Copy + Eq> Subscriber<A> {
    pub fn new(binding: Binding<A>, clock: Option<CaptureClock>) -> Result<Self, Error> {
        if !matches!(binding.endpoint, Endpoint::Subscriber(_)) {
            return Err(Error::Binding);
        }
        Ok(Self {
            binding,
            clock,
            now: Duration::ZERO,
            max_seen: None,
            latest: None,
            next_expiry: None,
            slots: BTreeMap::new(),
            receipts: BTreeMap::new(),
            next_request: Some(NonZeroU64::MIN),
            repair_paced_at: Duration::ZERO,
            active: 0,
            bytes: 0,
        })
    }

    pub fn next_expiry(&self) -> Option<Duration> {
        self.next_expiry
    }

    pub fn expire(&mut self, now: Duration) {
        self.now = self.now.max(now);
        if self.next_expiry.is_none_or(|expiry| self.now < expiry) {
            return;
        }
        self.next_expiry = None;
        for slot in self.slots.values_mut() {
            if let Slot::Open(unit) = slot {
                if self.now >= unit.expires {
                    self.active = self.active.saturating_sub(1);
                    self.bytes = self.bytes.saturating_sub(unit.bytes());
                    *slot = Slot::Closed;
                } else {
                    self.next_expiry = Some(
                        self.next_expiry
                            .map_or(unit.expires, |next| next.min(unit.expires)),
                    );
                }
            }
        }
    }

    /// Accept an expectation from the authenticated control owner for this lease.
    /// Replays do not renew deadlines or emit another application unit.
    pub fn expect(
        &mut self,
        binding: Binding<A>,
        expectation: data_control::Expectation,
        now: Duration,
    ) -> Result<Option<AssembledUnit>, Error> {
        let endpoint = self.reliable_endpoint(binding)?;
        let (seq, count) = expectation_fields(&expectation)?;
        SubscriberObservation {
            record: Some(subscriber_observation::Record::Expectation(expectation)),
        }
        .check(endpoint)
        .map_err(Error::Control)?;
        self.expire(now);
        if let Some(receipt) = self.receipts.get(&seq) {
            return if receipt.count == count {
                Ok(None)
            } else {
                Err(Error::Conflict)
            };
        }
        match self.slots.get(&seq) {
            Some(Slot::Delivered(delivered_count)) => {
                return if *delivered_count == count {
                    Ok(None)
                } else {
                    Err(Error::Conflict)
                };
            }
            Some(Slot::Closed) => return Err(Error::Retired),
            Some(Slot::Open(_)) => {}
            None if self.outside_window(seq) => return Err(Error::Retired),
            None => self.begin(seq, 0)?,
        }
        let Some(Slot::Open(unit)) = self.slots.get_mut(&seq) else {
            return Err(Error::Retired);
        };
        if unit.expected.is_some_and(|expected| expected != count)
            || unit
                .partial
                .as_ref()
                .is_some_and(|partial| partial.shape.count != count)
        {
            self.finish(seq, Slot::Closed);
            return Err(Error::Conflict);
        }
        if unit.expected.is_none() {
            unit.expected = Some(count);
            unit.repair_after = self.now.checked_add(CONTROL_RETRY);
        }
        Ok(self.assemble(seq))
    }

    pub fn receipt_deadline(&self) -> Option<Duration> {
        self.receipts.values().map(PendingReceipt::deadline).min()
    }

    /// Drive this on the control owner's existing timer and before processing
    /// more ingress at that deadline. The caller may drop a failed send attempt.
    pub fn next_receipt(&mut self, now: Duration) -> Option<ReceiptAction> {
        let endpoint = self.reliable_endpoint(self.binding).ok()?;
        self.expire(now);
        let (&seq, receipt) = self
            .receipts
            .iter_mut()
            .min_by_key(|(_, receipt)| receipt.deadline())?;
        if self.now < receipt.deadline() {
            return None;
        }
        if self.now >= receipt.expires {
            self.receipts.remove(&seq);
            return Some(ReceiptAction::Unconfirmed { unit_seq: seq });
        }
        receipt.retry_at = Some(self.now.saturating_add(CONTROL_RETRY));
        Some(ReceiptAction::Send(SubscriberControl {
            record: Some(subscriber_control::Record::AssemblyReceipt(
                data_control::Expectation {
                    unit: Some(data_control::BranchUnit {
                        source: Some(data_control::SourceUnit {
                            source_incarnation: endpoint.source.get(),
                            unit_seq: Some(seq),
                        }),
                        branch_incarnation: endpoint.branch.get(),
                    }),
                    fragment_count: u32::from(receipt.count),
                },
            )),
        }))
    }

    pub fn receipt_ack(
        &mut self,
        binding: Binding<A>,
        expectation: data_control::Expectation,
        now: Duration,
    ) -> Result<bool, Error> {
        let endpoint = self.reliable_endpoint(binding)?;
        let (seq, count) = expectation_fields(&expectation)?;
        SubscriberObservation {
            record: Some(subscriber_observation::Record::ReceiptAck(expectation)),
        }
        .check(endpoint)
        .map_err(Error::Control)?;
        self.expire(now);
        let Some(receipt) = self.receipts.get(&seq) else {
            return Ok(false);
        };
        if receipt.count != count {
            return Err(Error::Conflict);
        }
        if self.now >= receipt.expires || receipt.retry_at.is_none() {
            return Ok(false);
        }
        self.receipts.remove(&seq);
        Ok(true)
    }

    fn reliable_endpoint(&self, binding: Binding<A>) -> Result<SubscriberBinding, Error> {
        if binding == self.binding
            && binding.format.delivery() == Delivery::Reliable
            && let Endpoint::Subscriber(endpoint) = binding.endpoint
        {
            return Ok(endpoint);
        }
        Err(Error::Binding)
    }

    /// Bit i indicates that fragment i is missing from an accepted expectation.
    pub fn missing_fragments(&mut self, seq: u64, now: Duration) -> Option<u64> {
        self.expire(now);
        let Slot::Open(unit) = self.slots.get(&seq)? else {
            return None;
        };
        unit.missing_fragments()
    }

    fn repair_deadline(&self) -> Option<Duration> {
        self.next_request?;
        self.slots
            .values()
            .filter_map(|slot| match slot {
                Slot::Open(unit) => unit.repair_deadline(self.repair_paced_at),
                _ => None,
            })
            .min()
    }

    fn next_repair(&mut self, now: Duration) -> Option<SubscriberControl> {
        let endpoint = self.reliable_endpoint(self.binding).ok()?;
        self.expire(now);
        let request = self.next_request?;
        let (&seq, unit) = self.slots.iter_mut().find_map(|(seq, slot)| match slot {
            Slot::Open(unit)
                if unit
                    .repair_deadline(self.repair_paced_at)
                    .is_some_and(|at| at <= self.now) =>
            {
                Some((seq, unit))
            }
            _ => None,
        })?;
        let pending = unit.missing_fragments()? & !unit.requested;
        let index = pending.trailing_zeros();
        unit.requested |= 1_u64.checked_shl(index)?;
        self.next_request = request.checked_add(1);
        self.repair_paced_at = self.now.saturating_add(REPAIR_SPACING);
        Some(SubscriberControl {
            record: Some(subscriber_control::Record::RequestRepair(
                data_control::RepairDemand {
                    unit: Some(data_control::BranchUnit {
                        source: Some(data_control::SourceUnit {
                            source_incarnation: endpoint.source.get(),
                            unit_seq: Some(seq),
                        }),
                        branch_incarnation: endpoint.branch.get(),
                    }),
                    fragment_index: Some(index),
                    request_id: request.get(),
                },
            )),
        })
    }

    fn outside_window(&self, seq: u64) -> bool {
        self.max_seen
            .is_some_and(|max| max.saturating_sub(seq) >= WINDOW)
    }

    pub fn offer(
        &mut self,
        binding: Binding<A>,
        raw: &[u8],
        now: Duration,
    ) -> Result<Option<AssembledUnit>, Error> {
        if binding != self.binding {
            return Err(Error::Binding);
        }
        let record = Record::decode_without_descriptor(raw, bound_clock(self.clock.as_ref()))
            .map_err(Error::Record)?;
        let header = record.header();
        let seq = header.unit_seq;
        let index = header.index;
        let len = usize::try_from(header.unit_len)
            .map_err(|_| Error::Record(data_record::Error::UnitLength))?;
        let shape = Shape {
            len,
            count: header.count,
            capture: header
                .capture
                .map(|capture| (capture.nanos, capture.uncertainty_ns)),
        };
        let fragment_len = record.bytes().len();
        let fragment = raw
            .get(raw.len().saturating_sub(fragment_len)..)
            .ok_or(Error::Record(data_record::Error::Length))?;
        self.expire(now);
        if self.latest.is_some_and(|delivered| seq <= delivered) || self.outside_window(seq) {
            return Ok(None);
        }
        match self.slots.get(&seq) {
            Some(Slot::Closed | Slot::Delivered(_)) => return Ok(None),
            Some(Slot::Open(unit)) => {
                if unit.expected.is_some_and(|count| count != shape.count)
                    || unit
                        .partial
                        .as_ref()
                        .is_some_and(|partial| partial.shape != shape)
                {
                    self.finish(seq, Slot::Closed);
                    return Err(Error::Conflict);
                }
                if unit.partial.is_none() && self.bytes.saturating_add(shape.len) > BYTE_LIMIT {
                    return Err(Error::Capacity);
                }
            }
            None => self.begin(seq, shape.len)?,
        }
        let Some(Slot::Open(unit)) = self.slots.get_mut(&seq) else {
            return Ok(None);
        };
        let partial = unit.partial.get_or_insert_with(|| {
            self.bytes = self.bytes.saturating_add(shape.len);
            Partial {
                shape,
                bytes: vec![0; shape.len],
                received: 0,
            }
        });
        let offset = usize::from(index).saturating_mul(FRAGMENT_BYTES);
        let target = partial
            .bytes
            .get_mut(offset..offset.saturating_add(fragment.len()))
            .ok_or(Error::Record(data_record::Error::FragmentIndex))?;
        let mask = 1_u64
            .checked_shl(u32::from(index))
            .ok_or(Error::Record(data_record::Error::FragmentIndex))?;
        if partial.received & mask != 0 {
            if target != fragment {
                self.finish(seq, Slot::Closed);
                return Err(Error::Conflict);
            }
            return Ok(None);
        }
        target.copy_from_slice(fragment);
        partial.received |= mask;
        Ok(self.assemble(seq))
    }

    fn assemble(&mut self, seq: u64) -> Option<AssembledUnit> {
        let Slot::Open(unit) = self.slots.get(&seq)? else {
            return None;
        };
        let partial = unit.partial.as_ref()?;
        let shape = partial.shape;
        let delivery = self.binding.format.delivery();
        let terminal = if delivery == Delivery::Reliable {
            Slot::Delivered(unit.expected?)
        } else {
            Slot::Closed
        };
        if partial.received.count_ones() != u32::from(shape.count) {
            return None;
        }
        let unit = self.finish(seq, terminal)?;
        if let Some(count) = unit.expected {
            self.receipts.insert(
                seq,
                PendingReceipt {
                    count,
                    expires: unit.expires,
                    retry_at: None,
                },
            );
        }
        let partial = unit.partial?;
        if delivery == Delivery::Latest {
            self.latest = Some(seq);
            for (_, slot) in self.slots.range_mut(..seq) {
                if let Slot::Open(unit) = slot {
                    self.active = self.active.saturating_sub(1);
                    self.bytes = self.bytes.saturating_sub(unit.bytes());
                    *slot = Slot::Closed;
                }
            }
        }
        let captured_at = shape.capture.and_then(|(nanos, uncertainty_ns)| {
            self.clock.clone().map(|clock| CaptureTimestamp {
                clock,
                nanos,
                uncertainty_ns,
            })
        });
        Some(AssembledUnit {
            unit_seq: seq,
            captured_at,
            bytes: partial.bytes,
        })
    }

    fn begin(&mut self, seq: u64, reserve_bytes: usize) -> Result<(), Error> {
        let max = self.max_seen.map_or(seq, |previous| previous.max(seq));
        let floor = max.saturating_sub(WINDOW.saturating_sub(1));
        let (mut active, mut bytes) = (self.active, self.bytes);
        for (_, slot) in self.slots.range(..floor) {
            if let Slot::Open(unit) = slot {
                active = active.saturating_sub(1);
                bytes = bytes.saturating_sub(unit.bytes());
            }
        }
        if active.saturating_add(self.receipts.len()) >= UNIT_LIMIT
            || bytes.saturating_add(reserve_bytes) > BYTE_LIMIT
        {
            return Err(Error::Capacity);
        }
        let expires = self.now.checked_add(LIFETIME).ok_or(Error::Capacity)?;
        while self
            .slots
            .first_key_value()
            .is_some_and(|(&id, _)| id < floor)
        {
            let _ = self.slots.pop_first();
        }
        self.max_seen = Some(max);
        self.active = active.saturating_add(1);
        self.bytes = bytes;
        self.slots.insert(
            seq,
            Slot::Open(OpenUnit {
                expires,
                expected: None,
                partial: None,
                repair_after: None,
                requested: 0,
            }),
        );
        self.next_expiry = Some(self.next_expiry.map_or(expires, |next| next.min(expires)));
        Ok(())
    }

    fn finish(&mut self, seq: u64, terminal: Slot) -> Option<OpenUnit> {
        let slot = self.slots.get_mut(&seq)?;
        let Slot::Open(unit) = slot else {
            return None;
        };
        self.active = self.active.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(unit.bytes());
        let Slot::Open(unit) = core::mem::replace(slot, terminal) else {
            return None;
        };
        Some(unit)
    }
}

fn expectation_fields(expectation: &data_control::Expectation) -> Result<(u64, u16), Error> {
    let seq = expectation
        .unit
        .as_ref()
        .and_then(|unit| unit.source.as_ref())
        .and_then(|unit| unit.unit_seq)
        .ok_or(Error::Control(data_control::Error::Structure))?;
    let count = u16::try_from(expectation.fragment_count)
        .map_err(|_| Error::Control(data_control::Error::Structure))?;
    Ok((seq, count))
}

#[cfg(test)]
mod tests;
