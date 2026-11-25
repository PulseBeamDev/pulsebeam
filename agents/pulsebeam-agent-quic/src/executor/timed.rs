//! Optional, independently drained encoded media on live RTP bindings.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use pulsebeam_agent_core::MediaKind;

use super::{
    host::Agent,
    rtp::{Binding, ReceivedPacket},
};
use crate::{
    media::{
        jitter::{self, GapReason},
        units::{
            Access, Cause, Codec, Depacketizer, Discontinuity, Event, Recovery, SenderReport,
            VideoRecovery,
        },
    },
    transport::DatagramAdmission,
};

pub(super) const TRACKS: usize = 16;
const PACKETS: usize = 128;
const PACKET_BYTES: usize = 256 * 1024;
const EVENTS: usize = 32;
const UNIT_BYTES: NonZeroUsize = NonZeroUsize::new(512 * 1024).unwrap();
const QUEUED_BYTES: usize = 1024 * 1024;
const REORDER_LIMIT: Duration = Duration::from_secs(2);
const AGE_LIMIT: Duration = Duration::from_secs(10);
const REFRESH_RETRY: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Wait for reordering, then emit gaps and complete units, including
    /// dependent video. An application decides how to represent holes.
    Recording { reorder: Duration },
    /// Expire work by local residence, and resume video only at an
    /// independently decodable unit. RTCP refresh is best effort.
    Freshness {
        reorder: Duration,
        max_age: Duration,
    },
}

impl Policy {
    fn reorder(self) -> Duration {
        match self {
            Self::Recording { reorder } | Self::Freshness { reorder, .. } => reorder,
        }
    }

    fn max_age(self) -> Option<Duration> {
        match self {
            Self::Recording { .. } => None,
            Self::Freshness { max_age, .. } => Some(max_age),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachError {
    NotLive,
    AlreadyAttached,
    Capacity,
    /// Reorder wait is at most two seconds. Freshness age must be positive,
    /// at least the reorder wait, and at most ten seconds.
    InvalidPolicy,
}

/// A bounded summary of events lost at the final handoff. Sequence bounds
/// are inclusive and can enclose more than one loss interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loss {
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub overflow: u64,
    pub expired: u64,
    /// Events discarded to preserve ordering or fence dependent video after expiry.
    pub invalidated: u64,
}

#[derive(Debug)]
pub enum Receive {
    Media(Event<Binding>),
    Loss(Loss),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub received: u64,
    pub late: u64,
    pub duplicate: u64,
    pub reorder_overflow: u64,
    pub gaps: u64,
    pub expired_packets: u64,
    pub expired_events: u64,
    pub handoff_overflow: u64,
    pub invalidated_events: u64,
    pub withheld_video: u64,
    pub refresh_sent: u64,
    pub refresh_backpressure: u64,
    pub residence_max: Duration,
}

/// A pinned reader that reattaches to the current binding after reconnect.
/// The application owns subscription intent and drives Agent turns.
pub struct Reader {
    kind: MediaKind,
    publication: String,
    policy: Policy,
    binding: Option<Binding>,
}

impl Reader {
    pub fn new(kind: MediaKind, publication: String, policy: Policy) -> Self {
        Self {
            kind,
            publication,
            policy,
            binding: None,
        }
    }

    /// Attach the current lifetime even if a downstream sink cannot read yet.
    pub fn sync(&mut self, agent: &mut Agent) -> Result<Option<Binding>, AttachError> {
        let current = agent.rtp_subscription(self.kind, &self.publication);
        if current != self.binding {
            if let Some(previous) = self.binding.take() {
                agent.detach_encoded(previous);
            }
            if let Some(binding) = current {
                agent.attach_encoded(binding, self.policy)?;
                self.binding = Some(binding);
            }
        }
        Ok(self.binding)
    }

    pub fn next(&mut self, agent: &mut Agent) -> Result<Option<(Binding, Receive)>, AttachError> {
        Ok(self
            .sync(agent)?
            .and_then(|binding| agent.next_encoded(binding).map(|event| (binding, event))))
    }
}

pub(super) struct Track {
    binding: Binding,
    policy: Policy,
    jitter: jitter::Buffer<Binding, ReceivedPacket>,
    units: Depacketizer<Binding>,
    recovery: Option<VideoRecovery<Binding>>,
    refresh_at: Option<Instant>,
    queue: VecDeque<Event<Binding>>,
    queued_bytes: usize,
    before: Option<Loss>,
    after: Option<Loss>,
    stats: Stats,
}

impl Track {
    pub(super) fn with_av1_whole_source(mut self) -> Self {
        self.units = self.units.with_av1_whole_source();
        self
    }

    pub(super) fn new(binding: Binding, codec: Codec, policy: Policy) -> Result<Self, AttachError> {
        let reorder = policy.reorder();
        if reorder > REORDER_LIMIT
            || policy
                .max_age()
                .is_some_and(|age| age.is_zero() || age < reorder || age > AGE_LIMIT)
        {
            return Err(AttachError::InvalidPolicy);
        }
        let limits = jitter::Limits::new(reorder, 64, PACKETS, PACKET_BYTES)
            .ok_or(AttachError::InvalidPolicy)?;
        let units = Depacketizer::new(binding, codec).with_video_limit(UNIT_BYTES);
        Ok(Self {
            binding,
            policy,
            jitter: jitter::Buffer::new(binding, limits),
            units,
            recovery: (codec != Codec::Opus && policy.max_age().is_some())
                .then(|| VideoRecovery::new(binding)),
            refresh_at: None,
            queue: VecDeque::new(),
            queued_bytes: 0,
            before: None,
            after: None,
            stats: Stats::default(),
        })
    }

    pub(super) fn stats(&self) -> Stats {
        self.stats
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        let expire = self.policy.max_age().and_then(|age| {
            self.queue
                .iter()
                .filter_map(arrival)
                .min()?
                .checked_add(age)
        });
        [self.jitter.deadline(), self.refresh_at, expire]
            .into_iter()
            .flatten()
            .min()
    }

    pub(super) fn push(
        &mut self,
        now: Instant,
        packet: ReceivedPacket,
        report: Option<SenderReport>,
    ) {
        self.stats.received = self.stats.received.saturating_add(1);
        match self.jitter.push(now, packet.into()) {
            jitter::Push::Rejected {
                reason: jitter::Rejected::Late,
                ..
            } => self.stats.late = self.stats.late.saturating_add(1),
            jitter::Push::Rejected {
                reason: jitter::Rejected::Duplicate,
                ..
            } => self.stats.duplicate = self.stats.duplicate.saturating_add(1),
            jitter::Push::Evicted(_)
            | jitter::Push::Rejected {
                reason: jitter::Rejected::Capacity,
                ..
            } => {
                self.stats.reorder_overflow = self.stats.reorder_overflow.saturating_add(1);
            }
            jitter::Push::Queued | jitter::Push::Rejected { .. } => {}
        }
        self.advance(now, report);
    }

    pub(super) fn advance(&mut self, now: Instant, report: Option<SenderReport>) {
        self.expire(now);
        if let Some(report) = report {
            self.units.observe_report(self.binding, report);
        } else {
            self.units.clear_report();
        }
        while let Ok(Some(output)) = self.jitter.poll(now) {
            let event = match output {
                jitter::Output::Gap { sequence, reason } => {
                    self.stats.gaps = self.stats.gaps.saturating_add(1);
                    self.units
                        .gap(self.binding, sequence, reason)
                        .ok()
                        .map(Event::Discontinuity)
                }
                jitter::Output::Packet {
                    input,
                    sequence,
                    timestamp,
                } => {
                    if self.expired(now, input.arrived_at) {
                        self.stats.expired_packets = self.stats.expired_packets.saturating_add(1);
                        self.units
                            .gap(
                                self.binding,
                                sequence..sequence.saturating_add(1),
                                GapReason::Expired,
                            )
                            .ok()
                            .map(Event::Discontinuity)
                    } else {
                        self.units
                            .push(self.binding, input.packet.ordered(sequence, timestamp))
                            .ok()
                            .flatten()
                    }
                }
            };
            if let Some(event) = event {
                self.emit(now, event);
            }
        }
        self.schedule_refresh(now);
    }

    fn emit(&mut self, now: Instant, event: Event<Binding>) {
        let event = match event {
            Event::Unit(unit) if self.expired(now, unit.arrived_at) => {
                self.stats.expired_events = self.stats.expired_events.saturating_add(1);
                Event::Discontinuity(Discontinuity {
                    source: self.binding,
                    sequence: unit.first_sequence..unit.last_sequence.saturating_add(1),
                    cause: Cause::Gap(GapReason::Expired),
                })
            }
            other => other,
        };
        if self.after.is_some() {
            self.record_overflow(&event);
            return;
        }
        let event = if let Some(recovery) = &mut self.recovery {
            match recovery.apply(event) {
                Recovery::Deliver(event) => event,
                Recovery::Withheld(_) => {
                    self.stats.withheld_video = self.stats.withheld_video.saturating_add(1);
                    return;
                }
                Recovery::Retired => return,
            }
        } else {
            event
        };
        let bytes = retained_bytes(&event);
        if self.queue.len() >= EVENTS || self.queued_bytes.saturating_add(bytes) > QUEUED_BYTES {
            self.record_overflow(&event);
            return;
        }
        if matches!(&event, Event::Unit(unit) if unit.access == Access::IndependentVideo) {
            self.refresh_at = None;
        }
        self.queue.push_back(event);
        self.queued_bytes = self.queued_bytes.saturating_add(bytes);
    }

    fn record_overflow(&mut self, event: &Event<Binding>) {
        self.stats.handoff_overflow = self.stats.handoff_overflow.saturating_add(1);
        let tally = loss(&mut self.after, event);
        tally.overflow = tally.overflow.saturating_add(1);
        if let Some(recovery) = &mut self.recovery {
            recovery.lose_context();
        }
    }

    pub(super) fn next(&mut self, now: Instant) -> Option<Receive> {
        self.expire(now);
        if let Some(loss) = self.before.take() {
            return Some(Receive::Loss(loss));
        }
        if let Some(event) = self.pop() {
            if let Some(arrived) = arrival(&event) {
                self.stats.residence_max = self
                    .stats
                    .residence_max
                    .max(now.saturating_duration_since(arrived));
            }
            return Some(Receive::Media(event));
        }
        self.after.take().map(Receive::Loss)
    }

    fn pop(&mut self) -> Option<Event<Binding>> {
        let event = self.queue.pop_front()?;
        self.queued_bytes = self.queued_bytes.saturating_sub(retained_bytes(&event));
        Some(event)
    }

    fn expire(&mut self, now: Instant) {
        let Some(last_expired) = self
            .queue
            .iter()
            .rposition(|event| arrival(event).is_some_and(|at| self.expired(now, at)))
        else {
            return;
        };
        let count = if let Some(recovery) = &mut self.recovery {
            recovery.mark_loss();
            self.queue.len()
        } else {
            last_expired.saturating_add(1)
        };
        for _ in 0..count {
            let Some(event) = self.pop() else {
                break;
            };
            let expired = arrival(&event).is_some_and(|at| self.expired(now, at));
            let tally = loss(&mut self.before, &event);
            if expired {
                tally.expired = tally.expired.saturating_add(1);
                self.stats.expired_events = self.stats.expired_events.saturating_add(1);
            } else {
                tally.invalidated = tally.invalidated.saturating_add(1);
                self.stats.invalidated_events = self.stats.invalidated_events.saturating_add(1);
            }
        }
        self.schedule_refresh(now);
    }

    fn expired(&self, now: Instant, at: Instant) -> bool {
        self.policy
            .max_age()
            .is_some_and(|age| now.saturating_duration_since(at) >= age)
    }

    fn schedule_refresh(&mut self, now: Instant) {
        if self
            .recovery
            .as_mut()
            .and_then(VideoRecovery::take_refresh)
            .is_some()
        {
            self.refresh_at = Some(now);
        }
    }

    pub(super) fn refresh_due(&self, now: Instant) -> bool {
        self.refresh_at.is_some_and(|at| now >= at)
    }

    pub(super) fn refresh_result(&mut self, now: Instant, result: Result<(), DatagramAdmission>) {
        self.refresh_at = match result {
            Ok(()) => {
                self.stats.refresh_sent = self.stats.refresh_sent.saturating_add(1);
                None
            }
            Err(DatagramAdmission::Full | DatagramAdmission::ControlTurn) => {
                self.stats.refresh_backpressure = self.stats.refresh_backpressure.saturating_add(1);
                now.checked_add(REFRESH_RETRY)
            }
            Err(DatagramAdmission::Unsupported | DatagramAdmission::Size) => None,
        };
    }
}

#[cfg(test)]
mod tests;

fn arrival(event: &Event<Binding>) -> Option<Instant> {
    match event {
        Event::Unit(unit) => Some(unit.arrived_at),
        Event::Discontinuity(_) => None,
    }
}

fn retained_bytes(event: &Event<Binding>) -> usize {
    match event {
        Event::Unit(unit) => unit.data.capacity(),
        Event::Discontinuity(_) => 0,
    }
}

fn loss<'a>(pending: &'a mut Option<Loss>, event: &Event<Binding>) -> &'a mut Loss {
    let (first, last) = match event {
        Event::Unit(unit) => (unit.first_sequence, unit.last_sequence),
        Event::Discontinuity(gap) => (
            gap.sequence.start,
            gap.sequence.end.saturating_sub(1).max(gap.sequence.start),
        ),
    };
    let tally = pending.get_or_insert(Loss {
        first_sequence: first,
        last_sequence: last,
        overflow: 0,
        expired: 0,
        invalidated: 0,
    });
    tally.first_sequence = tally.first_sequence.min(first);
    tally.last_sequence = tally.last_sequence.max(last);
    tally
}
