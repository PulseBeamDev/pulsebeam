use std::{collections::VecDeque, mem::size_of, time::Duration};

use crate::{Ecn, Epoch, Error, PacketId, SpaceId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportOrder {
    Continuous(SpaceId),
    Independent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceiverConfig {
    pub capacity: usize,
    pub max_report_statuses: usize,
    pub interval: Duration,
    pub order: ReportOrder,
}

impl ReceiverConfig {
    pub fn minimum_storage_bytes(self) -> Option<usize> {
        self.capacity.checked_mul(size_of::<Entry>())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Receipt {
    pub space: SpaceId,
    pub packet: PacketId,
    pub arrived_at: Duration,
    pub ecn: Option<Ecn>,
    /// Opaque caller correlation, returned unchanged. It does not partition history.
    pub tag: u64,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    receipt: Receipt,
    pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Observation {
    Inserted { retired: usize },
    Duplicate,
    TooOld,
}

/// A borrow-free report opportunity. Only accept the prefix actually serialized
/// or delivered. Any intervening observation or lifecycle change invalidates it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReportWindow {
    pub epoch: Epoch,
    pub space: SpaceId,
    pub base: PacketId,
    pub count: usize,
    version: u64,
}

#[derive(Debug)]
pub struct Receiver {
    config: ReceiverConfig,
    epoch: Epoch,
    receipts: VecDeque<Entry>,
    highest: Option<u64>,
    next: Option<u64>,
    deadline: Option<Duration>,
    version: u64,
}

impl Receiver {
    pub fn new(config: ReceiverConfig, epoch: Epoch) -> Result<Self, Error> {
        if config.capacity == 0
            || config.max_report_statuses == 0
            || config.interval.is_zero()
            || config.minimum_storage_bytes().is_none()
            || u64::try_from(config.capacity).is_err()
            || u64::try_from(config.max_report_statuses).is_err()
        {
            return Err(Error::InvalidConfig);
        }
        Ok(Self {
            config,
            epoch,
            receipts: VecDeque::new(),
            highest: None,
            next: None,
            deadline: None,
            version: 0,
        })
    }

    /// Fund all receipt backing before observing packets. Allocation failure or
    /// insufficient credit leaves the previous backing and pending work intact.
    pub fn reserve(&mut self, limit: usize) -> Result<usize, Error> {
        if self.receipts.capacity() >= self.config.capacity {
            let bytes = self.storage_bytes();
            return (bytes <= limit).then_some(bytes).ok_or(Error::Exhausted);
        }
        self.config
            .minimum_storage_bytes()
            .filter(|bytes| *bytes <= limit)
            .ok_or(Error::Exhausted)?;
        let mut expanded = VecDeque::new();
        expanded
            .try_reserve_exact(self.config.capacity)
            .map_err(|_| Error::Exhausted)?;
        let bytes = expanded
            .capacity()
            .checked_mul(size_of::<Entry>())
            .filter(|bytes| *bytes <= limit)
            .ok_or(Error::Exhausted)?;
        expanded.append(&mut self.receipts);
        self.receipts = expanded;
        Ok(bytes)
    }

    pub fn storage_bytes(&self) -> usize {
        self.receipts.capacity().saturating_mul(size_of::<Entry>())
    }

    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }

    pub fn highest(&self, space: SpaceId) -> Option<PacketId> {
        match self.config.order {
            ReportOrder::Continuous(expected) => (space == expected)
                .then_some(self.highest)
                .flatten()
                .map(PacketId),
            ReportOrder::Independent => self
                .receipts
                .iter()
                .filter(|entry| entry.receipt.space == space)
                .map(|entry| entry.receipt.packet)
                .max(),
        }
    }

    pub fn observe(&mut self, epoch: Epoch, receipt: Receipt) -> Result<Observation, Error> {
        if epoch != self.epoch {
            return Err(Error::WrongEpoch);
        }
        if receipt.packet.0 == u64::MAX {
            return Err(Error::InvalidRange);
        }
        if self.receipts.capacity() < self.config.capacity {
            return Err(Error::Exhausted);
        }
        let version = self.version.checked_add(1).ok_or(Error::Exhausted)?;
        let mut retired = 0usize;
        let mut index = match self.config.order {
            ReportOrder::Continuous(space) => {
                if space != receipt.space {
                    return Err(Error::WrongSpace);
                }
                let sequence = receipt.packet.0;
                let highest = self
                    .highest
                    .map_or(sequence, |highest| highest.max(sequence));
                let capacity =
                    u64::try_from(self.config.capacity).map_err(|_| Error::InvalidConfig)?;
                let floor = highest.saturating_sub(capacity.saturating_sub(1));
                if sequence < floor {
                    return Ok(Observation::TooOld);
                }
                let Err(index) = self
                    .receipts
                    .binary_search_by_key(&receipt.packet, |entry| entry.receipt.packet)
                else {
                    return Ok(Observation::Duplicate);
                };
                while self
                    .receipts
                    .front()
                    .is_some_and(|entry| entry.receipt.packet.0 < floor)
                {
                    self.receipts.pop_front();
                    retired = retired.saturating_add(1);
                }
                self.highest = Some(highest);
                self.next = Some(
                    self.next
                        .map_or(sequence, |next| next.min(sequence))
                        .max(floor),
                );
                index.saturating_sub(retired)
            }
            ReportOrder::Independent => {
                if self.receipt(receipt.space, receipt.packet).is_some() {
                    return Ok(Observation::Duplicate);
                }
                self.receipts.len()
            }
        };
        if self.receipts.len() == self.config.capacity {
            self.receipts.pop_front();
            retired = retired.saturating_add(1);
            index = index.saturating_sub(1);
        }
        self.receipts.insert(
            index,
            Entry {
                receipt,
                pending: true,
            },
        );
        self.version = version;
        self.deadline
            .get_or_insert_with(|| receipt.arrived_at.saturating_add(self.config.interval));
        Ok(Observation::Inserted { retired })
    }

    pub fn receipt(&self, space: SpaceId, packet: PacketId) -> Option<&Receipt> {
        match self.config.order {
            ReportOrder::Continuous(expected) if expected == space => self
                .receipts
                .binary_search_by_key(&packet, |entry| entry.receipt.packet)
                .ok()
                .and_then(|index| self.receipts.get(index))
                .map(|entry| &entry.receipt),
            ReportOrder::Continuous(_) => None,
            ReportOrder::Independent => self
                .receipts
                .iter()
                .find(|entry| entry.receipt.space == space && entry.receipt.packet == packet)
                .map(|entry| &entry.receipt),
        }
    }

    pub fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    pub fn report(&self, now: Duration) -> Option<ReportWindow> {
        if now < self.deadline? {
            return None;
        }
        let max_statuses = u64::try_from(self.config.max_report_statuses).ok()?;
        let (space, base, end) = match self.config.order {
            ReportOrder::Continuous(space) => {
                let base = self.next?;
                let end = base
                    .saturating_add(max_statuses)
                    .min(self.highest?.checked_add(1)?);
                (space, base, end)
            }
            ReportOrder::Independent => {
                let space = self
                    .receipts
                    .iter()
                    .find(|entry| entry.pending)?
                    .receipt
                    .space;
                let first = self
                    .receipts
                    .iter()
                    .filter(|entry| entry.receipt.space == space && entry.pending)
                    .map(|entry| entry.receipt.packet.0)
                    .min()?;
                let preceding = self
                    .receipts
                    .iter()
                    .filter(|entry| {
                        entry.receipt.space == space
                            && !entry.pending
                            && entry.receipt.packet.0 < first
                    })
                    .map(|entry| entry.receipt.packet.0.saturating_add(1))
                    .max()
                    .unwrap_or(first);
                let base = preceding.max(first.saturating_sub(max_statuses.saturating_sub(1)));
                let end = self
                    .receipts
                    .iter()
                    .filter(|entry| entry.receipt.space == space && entry.pending)
                    .map(|entry| entry.receipt.packet.0.saturating_add(1))
                    .max()?
                    .min(base.saturating_add(max_statuses));
                (space, base, end)
            }
        };
        let count = usize::try_from(end.checked_sub(base)?)
            .ok()
            .filter(|count| *count != 0)?;
        Some(ReportWindow {
            epoch: self.epoch,
            space,
            base: PacketId(base),
            count,
            version: self.version,
        })
    }

    /// The first retained receipt at or beyond the window base, even if beyond
    /// its bounded end. Callers can anchor loss-only windows without inventing a source.
    pub fn first_receipt(&self, report: ReportWindow) -> Option<&Receipt> {
        if report.epoch != self.epoch || report.version != self.version {
            return None;
        }
        self.receipts
            .iter()
            .filter(|entry| {
                entry.receipt.space == report.space && entry.receipt.packet >= report.base
            })
            .min_by_key(|entry| entry.receipt.packet)
            .map(|entry| &entry.receipt)
    }

    pub fn accept(
        &mut self,
        report: ReportWindow,
        count: usize,
        now: Duration,
    ) -> Result<(), Error> {
        if self.report(now) != Some(report) {
            return Err(Error::StaleReport);
        }
        if count == 0 || count > report.count {
            return Err(Error::InvalidRange);
        }
        let version = self.version.checked_add(1).ok_or(Error::Exhausted)?;
        let end = report
            .base
            .0
            .checked_add(u64::try_from(count).map_err(|_| Error::InvalidRange)?)
            .ok_or(Error::InvalidRange)?;
        for entry in &mut self.receipts {
            if entry.receipt.space == report.space
                && (report.base.0..end).contains(&entry.receipt.packet.0)
            {
                entry.pending = false;
            }
        }
        let pending = match self.config.order {
            ReportOrder::Continuous(_) => {
                self.next = Some(end);
                self.highest.is_some_and(|highest| end <= highest)
            }
            ReportOrder::Independent => self.receipts.iter().any(|entry| entry.pending),
        };
        self.deadline = pending.then_some(now);
        self.version = version;
        Ok(())
    }

    /// Backpressure consumes no statuses. A future deadline avoids a due-now spin.
    pub fn defer(&mut self, now: Duration, until: Duration) -> Result<(), Error> {
        if self.deadline.is_none() || until <= now {
            return Err(Error::InvalidRange);
        }
        let version = self.version.checked_add(1).ok_or(Error::Exhausted)?;
        self.deadline = Some(until);
        self.version = version;
        Ok(())
    }

    pub fn clear(&mut self, epoch: Epoch) {
        self.receipts.clear();
        self.epoch = epoch;
        self.highest = None;
        self.next = None;
        self.deadline = None;
        self.version = self.version.saturating_add(1);
    }

    pub fn abort(&mut self) {
        self.clear(self.epoch);
        self.receipts = VecDeque::new();
    }
}

#[cfg(test)]
mod tests;
