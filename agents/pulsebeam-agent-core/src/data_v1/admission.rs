use alloc::{collections::BTreeMap, vec, vec::Vec};
use core::{num::NonZeroU64, time::Duration};

use pulsebeam_proto::{
    data_control::{self, PublisherObservation, Record as _, publisher_observation},
    data_endpoint::{Binding, Delivery, Endpoint},
    data_record,
};

use crate::ffi::{CaptureClock, DataSendOptions};

use super::{BYTE_LIMIT, CONTROL_RETRY, Error, LIFETIME, OutgoingUnit, Publisher, UNIT_LIMIT};

const DELIVERY_HISTORY: usize = 128;
const DELIVERY_BRANCH_LIMIT: usize = 64;
const DELIVERY_LIFETIME: Duration = LIFETIME.saturating_mul(2).saturating_add(CONTROL_RETRY);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalDelivery {
    Confirmed,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublisherDelivery {
    pub unit_seq: u64,
    pub branch: NonZeroU64,
    pub outcome: TerminalDelivery,
}

struct DeliveryHistory {
    expires: Duration,
    fragment_count: u16,
    reported: BTreeMap<NonZeroU64, TerminalDelivery>,
}

struct RetainedUnit {
    unit: OutgoingUnit,
    charge: usize,
    expires: Duration,
    retry_at: Option<Duration>,
}

impl RetainedUnit {
    fn deadline(&self) -> Duration {
        self.retry_at
            .map_or(self.expires, |retry| retry.min(self.expires))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmittedUnit {
    pub unit_seq: u64,
    pub fragment_count: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdmissionAction {
    RetryFirst { unit_seq: u64, record: Vec<u8> },
    Unconfirmed { unit_seq: u64 },
}

/// Retains originals under the exact Reliable publisher binding. Local
/// admission and first-fragment source acknowledgment are separate from delivery.
pub struct ReliablePublisher<A> {
    publisher: Publisher<A>,
    now: Duration,
    bytes: usize,
    retained: BTreeMap<u64, RetainedUnit>,
    delivery: BTreeMap<u64, DeliveryHistory>,
}

impl<A: Copy + Eq> ReliablePublisher<A> {
    pub fn new(
        binding: Binding<A>,
        clock: Option<CaptureClock>,
        first: u64,
    ) -> Result<Self, Error> {
        if binding.format.delivery() != Delivery::Reliable {
            return Err(Error::Binding);
        }
        Ok(Self {
            publisher: Publisher::for_binding(binding, clock, first)?,
            now: Duration::ZERO,
            bytes: 0,
            retained: BTreeMap::new(),
            delivery: BTreeMap::new(),
        })
    }

    pub(super) fn with_record_dd(mut self, record_dd: bool) -> Self {
        self.publisher = self.publisher.with_record_dd(record_dd);
        self
    }

    /// Retain the original before granting a sequence for initial transmission.
    /// The caller services due timer actions before admitting new work.
    pub fn admit(
        &mut self,
        binding: Binding<A>,
        bytes: Vec<u8>,
        options: DataSendOptions,
        now: Duration,
    ) -> Result<AdmittedUnit, Error> {
        if binding != self.publisher.binding {
            return Err(Error::Binding);
        }
        self.now = self.now.max(now);
        let count = data_record::fragment_count(bytes.len()).map_err(Error::Record)?;
        let charge = bytes
            .len()
            .checked_add(super::descriptor_budget(self.publisher.record_dd, count))
            .ok_or(Error::Capacity)?;
        let total = self.bytes.saturating_add(charge);
        if self.retained.len() >= UNIT_LIMIT || total > BYTE_LIMIT {
            return Err(Error::Capacity);
        }
        while self
            .delivery
            .first_key_value()
            .is_some_and(|(_, history)| self.now >= history.expires)
        {
            self.delivery.pop_first();
        }
        if self.delivery.len() >= DELIVERY_HISTORY {
            return Err(Error::Capacity);
        }
        let expires = self.now.checked_add(LIFETIME).ok_or(Error::Capacity)?;
        let report_expires = self
            .now
            .checked_add(DELIVERY_LIFETIME)
            .ok_or(Error::Capacity)?;
        let unit = self.publisher.admit(binding, bytes, options)?;
        let admitted = AdmittedUnit {
            unit_seq: unit.unit_seq(),
            fragment_count: unit.count(),
        };
        self.retained.insert(
            admitted.unit_seq,
            RetainedUnit {
                unit,
                charge,
                expires,
                retry_at: Some(self.now.saturating_add(CONTROL_RETRY)),
            },
        );
        self.delivery.insert(
            admitted.unit_seq,
            DeliveryHistory {
                expires: report_expires,
                fragment_count: admitted.fragment_count,
                reported: BTreeMap::new(),
            },
        );
        self.bytes = total;
        Ok(admitted)
    }

    pub fn write_initial(
        &mut self,
        admitted: AdmittedUnit,
        index: u16,
        out: &mut [u8],
        now: Duration,
    ) -> Result<usize, Error> {
        self.now = self.now.max(now);
        let entry = self
            .retained
            .get(&admitted.unit_seq)
            .filter(|entry| self.now < entry.expires)
            .ok_or(Error::Retired)?;
        if admitted.fragment_count != entry.unit.count() {
            return Err(Error::Conflict);
        }
        entry.unit.write(index, out)
    }

    pub fn write_original(
        &mut self,
        binding: Binding<A>,
        demand: data_control::OriginalDemand,
        out: &mut [u8],
        now: Duration,
    ) -> Result<Option<usize>, Error> {
        Ok(self
            .write_original_until(binding, demand, out, now)?
            .map(|(size, _)| size))
    }

    pub(super) fn write_original_until(
        &mut self,
        binding: Binding<A>,
        demand: data_control::OriginalDemand,
        out: &mut [u8],
        now: Duration,
    ) -> Result<Option<(usize, Duration)>, Error> {
        let endpoint = self.publisher_endpoint(binding)?;
        let invalid = Error::Control(data_control::Error::Structure);
        let seq = demand
            .unit
            .as_ref()
            .and_then(|unit| unit.unit_seq)
            .ok_or(invalid)?;
        let index = demand
            .fragment_index
            .and_then(|index| u16::try_from(index).ok())
            .ok_or(invalid)?;
        PublisherObservation {
            record: Some(publisher_observation::Record::RequestOriginal(demand)),
        }
        .check(endpoint)
        .map_err(Error::Control)?;
        self.now = self.now.max(now);
        let Some(entry) = self
            .retained
            .get(&seq)
            .filter(|entry| self.now < entry.expires && index < entry.unit.count())
        else {
            return Ok(None);
        };
        Ok(Some((entry.unit.write(index, out)?, entry.expires)))
    }

    pub fn deadline(&self) -> Option<Duration> {
        self.retained.values().map(RetainedUnit::deadline).min()
    }

    pub(super) fn expiry(&self) -> Option<Duration> {
        self.retained
            .first_key_value()
            .map(|(_, unit)| unit.expires)
    }

    pub(super) fn expire_admission(&mut self, now: Duration, retiring: bool) -> Option<u64> {
        self.now = self.now.max(now);
        while retiring || self.expiry().is_some_and(|expires| expires <= self.now) {
            let (seq, entry) = self.retained.pop_first()?;
            self.bytes = self.bytes.saturating_sub(entry.charge);
            if entry.retry_at.is_some() {
                return Some(seq);
            }
        }
        None
    }

    pub(super) fn first_fragment_retry(
        &mut self,
        now: Duration,
    ) -> Result<Option<(u64, Vec<u8>)>, Error> {
        self.now = self.now.max(now);
        let Some((&seq, entry)) = self
            .retained
            .iter_mut()
            .filter(|(_, entry)| {
                self.now < entry.expires && entry.retry_at.is_some_and(|retry| retry <= self.now)
            })
            .min_by_key(|(_, entry)| entry.retry_at)
        else {
            return Ok(None);
        };
        let mut record = vec![0; super::record_capacity(self.publisher.record_dd)];
        let len = entry.unit.write(0, &mut record)?;
        record.truncate(len);
        entry.retry_at = Some(self.now.saturating_add(CONTROL_RETRY));
        Ok(Some((seq, record)))
    }

    pub fn next_action(&mut self, now: Duration) -> Result<Option<AdmissionAction>, Error> {
        if let Some(unit_seq) = self.expire_admission(now, false) {
            return Ok(Some(AdmissionAction::Unconfirmed { unit_seq }));
        }
        Ok(self
            .first_fragment_retry(now)?
            .map(|(unit_seq, record)| AdmissionAction::RetryFirst { unit_seq, record }))
    }

    pub(super) fn discard(&mut self) {
        self.retained.clear();
        self.delivery.clear();
        self.bytes = 0;
    }

    /// Delivery history keeps only metadata after the original bytes expire.
    /// A report may race a timed-out admission; neither implies the other.
    pub fn observe_delivery(
        &mut self,
        binding: Binding<A>,
        report: data_control::DeliveryOutcome,
        now: Duration,
    ) -> Result<Option<PublisherDelivery>, Error> {
        let endpoint = self.publisher_endpoint(binding)?;
        let invalid = Error::Control(data_control::Error::Structure);
        let expectation = report.expectation.as_ref().ok_or(invalid)?;
        let unit = expectation.unit.as_ref().ok_or(invalid)?;
        let seq = unit
            .source
            .as_ref()
            .and_then(|source| source.unit_seq)
            .ok_or(invalid)?;
        let branch = NonZeroU64::new(unit.branch_incarnation).ok_or(invalid)?;
        let count = expectation.fragment_count;
        let outcome = match data_control::ControlOutcome::try_from(report.outcome) {
            Ok(data_control::ControlOutcome::Confirmed) => TerminalDelivery::Confirmed,
            Ok(data_control::ControlOutcome::Failed) => TerminalDelivery::Failed,
            Ok(data_control::ControlOutcome::Unknown) => TerminalDelivery::Unknown,
            _ => return Err(invalid),
        };
        PublisherObservation {
            record: Some(publisher_observation::Record::Delivery(report)),
        }
        .check(endpoint)
        .map_err(Error::Control)?;
        self.now = self.now.max(now);
        let Some(history) = self
            .delivery
            .get_mut(&seq)
            .filter(|history| self.now < history.expires)
        else {
            return Ok(None);
        };
        if count != u32::from(history.fragment_count) {
            return Err(Error::Conflict);
        }
        if let Some(previous) = history.reported.get(&branch) {
            return if *previous == outcome {
                Ok(None)
            } else {
                Err(Error::Conflict)
            };
        }
        if history.reported.len() >= DELIVERY_BRANCH_LIMIT {
            return Err(Error::Capacity);
        }
        history.reported.insert(branch, outcome);
        Ok(Some(PublisherDelivery {
            unit_seq: seq,
            branch,
            outcome,
        }))
    }

    fn publisher_endpoint(
        &self,
        binding: Binding<A>,
    ) -> Result<data_control::PublisherBinding, Error> {
        if binding == self.publisher.binding
            && let Endpoint::Publisher(endpoint) = binding.endpoint
        {
            Ok(endpoint)
        } else {
            Err(Error::Binding)
        }
    }

    /// Consume a checked admission on the current authenticated publisher lease.
    /// An acknowledgment does not renew the original deadline or release bytes
    /// that may still be needed by the source.
    pub fn acknowledge(
        &mut self,
        binding: Binding<A>,
        admission: data_control::SourceUnit,
        now: Duration,
    ) -> Result<bool, Error> {
        let endpoint = self.publisher_endpoint(binding)?;
        let seq = admission
            .unit_seq
            .ok_or(Error::Control(data_control::Error::Structure))?;
        PublisherObservation {
            record: Some(publisher_observation::Record::Admission(admission)),
        }
        .check(endpoint)
        .map_err(Error::Control)?;
        self.now = self.now.max(now);
        let Some(entry) = self.retained.get_mut(&seq) else {
            return Ok(false);
        };
        Ok(self.now < entry.expires && entry.retry_at.take().is_some())
    }
}
