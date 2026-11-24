//! Caller-clocked RTP ordering for a single source lifetime.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    ops::Range,
    time::{Duration, Instant},
};

use pulsebeam_core::rtp_serial::{ExtensionError, extend_u32};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    delay: Duration,
    reorder: u32,
    capacity: Usage,
}

impl Limits {
    /// Zero delay or reorder distance settles gaps immediately. A reorder
    /// distance must be smaller than half the canonical sequence space.
    pub fn new(delay: Duration, reorder: u32, packets: usize, bytes: usize) -> Option<Self> {
        (reorder < (1 << 31) && packets != 0 && bytes != 0).then_some(Self {
            delay,
            reorder,
            capacity: Usage { packets, bytes },
        })
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            delay: Duration::from_millis(60),
            reorder: 64,
            capacity: Usage {
                packets: 128,
                bytes: 256 * 1024,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub packets: usize,
    pub bytes: usize,
}

impl Usage {
    pub const fn remaining(self, used: Self) -> Self {
        Self {
            packets: self.packets.saturating_sub(used.packets),
            bytes: self.bytes.saturating_sub(used.bytes),
        }
    }
}

#[derive(Debug)]
pub struct Input<S, P> {
    pub source: S,
    pub sequence: u32,
    pub timestamp: u32,
    pub arrived_at: Instant,
    /// Retained variable-size storage, including the payload and extensions.
    /// The packet count independently bounds fixed-size entries.
    pub retained_bytes: NonZeroUsize,
    pub packet: P,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    Retired,
    Clock,
    Sequence(ExtensionError),
    Late,
    Duplicate,
    Capacity,
}

#[derive(Debug)]
pub enum Push<S, P> {
    Queued,
    /// The new packet took the place of one farther from the drain frontier.
    Evicted(Input<S, P>),
    Rejected {
        input: Input<S, P>,
        reason: Rejected,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapReason {
    Deadline,
    ReorderLimit,
    /// A caller chose not to deliver already ordered work past its age limit.
    Expired,
}

#[derive(Debug)]
pub enum Output<S, P> {
    Packet {
        input: Input<S, P>,
        sequence: u64,
        /// Unknown if extension would be ambiguous or before the local origin.
        /// This is media time, never a capture or wall-clock estimate.
        timestamp: Option<u64>,
    },
    Gap {
        sequence: Range<u64>,
        reason: GapReason,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct ClockWentBackwards;

struct Entry<S, P> {
    expiry: Instant,
    input: Input<S, P>,
}

/// Bounded storage and work per insert or drain. The first packet establishes
/// the sequence origin. A gap needs evidence from a later packet; silence
/// without such evidence does not create an invented loss or deadline.
pub struct Buffer<S, P> {
    source: S,
    limits: Limits,
    queue: BTreeMap<u64, Entry<S, P>>,
    expiries: BTreeSet<(Instant, u64)>,
    bytes: usize,
    next: Option<u64>,
    exhausted: bool,
    clock: Option<Instant>,
    timestamp: Option<u64>,
}

impl<S: Eq, P> Buffer<S, P> {
    pub fn new(source: S, limits: Limits) -> Self {
        Self {
            source,
            limits,
            queue: BTreeMap::new(),
            expiries: BTreeSet::new(),
            bytes: 0,
            next: None,
            exhausted: false,
            clock: None,
            timestamp: None,
        }
    }

    pub fn usage(&self) -> Usage {
        Usage {
            packets: self.queue.len(),
            bytes: self.bytes,
        }
    }

    /// Replace this source with a new, never-reused lifetime identity and
    /// report the discarded storage. A reset to the same lifetime is a no-op.
    pub fn replace_source(&mut self, source: S) -> Usage {
        if source == self.source {
            return Usage::default();
        }
        let retired = self.usage();
        let clock = self.clock;
        *self = Self::new(source, self.limits);
        self.clock = clock;
        retired
    }

    pub fn push(&mut self, now: Instant, input: Input<S, P>) -> Push<S, P> {
        self.push_with_headroom(
            now,
            input,
            Usage {
                packets: usize::MAX,
                bytes: usize::MAX,
            },
        )
    }

    /// Apply both the per-source limits and the unused portion of a caller's
    /// outer track/session budget. Read `usage` before/after to account for it.
    /// If necessary, replace at most one farther packet; never scan the queue.
    pub fn push_with_headroom(
        &mut self,
        now: Instant,
        input: Input<S, P>,
        outer: Usage,
    ) -> Push<S, P> {
        let (sequence, expiry) = match self.classify(now, &input) {
            Ok(position) => position,
            Err(reason) => return Push::Rejected { input, reason },
        };
        let local = self.limits.capacity.remaining(self.usage());
        let headroom = Usage {
            packets: local.packets.min(outer.packets),
            bytes: local.bytes.min(outer.bytes),
        };
        let mut displaced = None;
        if headroom.packets == 0 || input.retained_bytes.get() > headroom.bytes {
            let Some((&last, entry)) = self.queue.last_key_value() else {
                return Push::Rejected {
                    input,
                    reason: Rejected::Capacity,
                };
            };
            if sequence >= last
                || input.retained_bytes.get()
                    > headroom
                        .bytes
                        .saturating_add(entry.input.retained_bytes.get())
            {
                return Push::Rejected {
                    input,
                    reason: Rejected::Capacity,
                };
            }
            displaced = self.remove(last).map(|entry| entry.input);
        }
        self.next.get_or_insert(sequence);
        self.bytes = self.bytes.saturating_add(input.retained_bytes.get());
        self.expiries.insert((expiry, sequence));
        self.queue.insert(sequence, Entry { expiry, input });
        displaced.map_or(Push::Queued, Push::Evicted)
    }

    fn classify(&mut self, now: Instant, input: &Input<S, P>) -> Result<(u64, Instant), Rejected> {
        if self.source != input.source {
            return Err(Rejected::Retired);
        }
        self.advance_clock(now).map_err(|_| Rejected::Clock)?;
        if input.arrived_at > now {
            return Err(Rejected::Clock);
        }
        if self.exhausted {
            return Err(Rejected::Sequence(ExtensionError::Overflow));
        }
        let sequence = self
            .next
            .map_or_else(
                || Ok(u64::from(input.sequence)),
                |next| extend_u32(input.sequence, next),
            )
            .map_err(Rejected::Sequence)?;
        if self.next.is_some_and(|next| sequence < next)
            || self
                .pending_gap(now)
                .is_some_and(|(gap, _)| sequence < gap.end)
        {
            return Err(Rejected::Late);
        }
        if self.queue.contains_key(&sequence) {
            return Err(Rejected::Duplicate);
        }
        let expiry = input
            .arrived_at
            .checked_add(self.limits.delay)
            .ok_or(Rejected::Clock)?;
        Ok((sequence, expiry))
    }

    /// Next time a drain can produce output, or none when blocked on input.
    pub fn deadline(&self) -> Option<Instant> {
        let clock = self.clock?;
        let next = self.next?;
        if self.queue.contains_key(&next) || self.distance_exceeded(next) {
            return Some(clock);
        }
        self.expiries.first().map(|(at, _)| *at)
    }

    /// Return at most one ordered packet or gap. Call repeatedly within the
    /// caller's work budget until no event is ready.
    pub fn poll(&mut self, now: Instant) -> Result<Option<Output<S, P>>, ClockWentBackwards> {
        self.advance_clock(now)?;
        let Some(next) = self.next else {
            return Ok(None);
        };
        if let Some(entry) = self.remove(next) {
            self.next = next.checked_add(1);
            self.exhausted = self.next.is_none();
            let timestamp = self.timestamp.map_or_else(
                || Some(u64::from(entry.input.timestamp)),
                |anchor| extend_u32(entry.input.timestamp, anchor).ok(),
            );
            if let Some(timestamp) = timestamp {
                self.timestamp = Some(
                    self.timestamp
                        .map_or(timestamp, |anchor| anchor.max(timestamp)),
                );
            }
            return Ok(Some(Output::Packet {
                input: entry.input,
                sequence: next,
                timestamp,
            }));
        }
        if let Some((sequence, reason)) = self.pending_gap(now) {
            self.next = Some(sequence.end);
            return Ok(Some(Output::Gap { sequence, reason }));
        }
        Ok(None)
    }

    fn advance_clock(&mut self, now: Instant) -> Result<(), ClockWentBackwards> {
        if self.clock.is_some_and(|last| now < last) {
            return Err(ClockWentBackwards);
        }
        self.clock = Some(now);
        Ok(())
    }

    fn pending_gap(&self, now: Instant) -> Option<(Range<u64>, GapReason)> {
        let next = self.next?;
        let (&first, _) = self.queue.first_key_value()?;
        if next >= first {
            return None;
        }
        let reason = if self.distance_exceeded(next) {
            GapReason::ReorderLimit
        } else if self.expiries.first().is_some_and(|(at, _)| *at <= now) {
            GapReason::Deadline
        } else {
            return None;
        };
        Some((next..first, reason))
    }

    fn distance_exceeded(&self, next: u64) -> bool {
        self.queue
            .last_key_value()
            .is_some_and(|(&last, _)| last.saturating_sub(next) > u64::from(self.limits.reorder))
    }

    fn remove(&mut self, sequence: u64) -> Option<Entry<S, P>> {
        let entry = self.queue.remove(&sequence)?;
        self.bytes = self.bytes.saturating_sub(entry.input.retained_bytes.get());
        self.expiries.remove(&(entry.expiry, sequence));
        Some(entry)
    }
}

#[cfg(test)]
mod tests;
