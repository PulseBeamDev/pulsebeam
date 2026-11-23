//! Whole-source RTP sequence and sender-clock mapping per output SSRC.
//! Preparation is side-effect free; only a successful queue transaction may
//! install the returned state. Irrevocable SRTP indexes are RTC-owned.

use std::time::Instant;

const HALF_SEQUENCE_RANGE: u64 = 1 << 15;
const TIMESTAMP_HALF: u32 = 1 << 31;
const MAX_SRTP_INDEX: u64 = (1 << 48) - 1;

/// The last accepted handoff, not merely a queued RTP write. The caller must
/// also supply the highest transmitted SR RTP time in this SSRC's extended
/// output clock, if a report has crossed that boundary.
#[derive(Clone, Copy, Default)]
pub struct Transmitted {
    pub primary: Option<u64>,
    pub sender_report: Option<u64>,
}

#[derive(Clone, Copy)]
pub struct Input {
    pub sequence: u64,
    pub timestamp: u32,
}

/// Output clock correlation. `wallclock` is bound to `anchor_timestamp`, not
/// to the arrival of every subsequent packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Projected {
    pub sequence: u64,
    pub timestamp: u32,
    pub anchor_timestamp: u64,
    pub wallclock: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    Stale,
    AmbiguousEpoch,
    IndexExhausted,
    Clock,
}

#[derive(Clone, Copy)]
struct Epoch<B> {
    source: B,
    input_sequence: u64,
    output_sequence: u64,
    timestamp_offset: u32,
    anchor_timestamp: u64,
    wallclock: Instant,
}

/// Bind `B` to an exact authorized source incarnation, never to an RTP SSRC.
/// `replay_positions` bounds late first transmissions in the qualified receiver.
#[derive(Clone, Copy)]
pub struct Timeline<B> {
    epoch: Option<Epoch<B>>,
    initial_sequence: u64,
    sequence_high: u64,
    timestamp_high: u64,
    sample_interval: u64,
    clock_rate: u32,
    replay_positions: u16,
}

impl<B: Copy + Eq> Timeline<B> {
    /// Choose the seed sequence and RTP timestamp randomly for the first epoch.
    pub fn new(
        sequence: u16,
        timestamp: u32,
        clock_rate: u32,
        replay_positions: u16,
    ) -> Option<Self> {
        if clock_rate == 0 || !(1..=32768).contains(&replay_positions) {
            return None;
        }
        Some(Self {
            epoch: None,
            initial_sequence: u64::from(sequence),
            sequence_high: u64::from(sequence),
            timestamp_high: u64::from(timestamp),
            sample_interval: 1,
            clock_rate,
            replay_positions,
        })
    }

    pub(super) fn committed_source(&self) -> Option<B> {
        self.epoch.map(|epoch| epoch.source)
    }

    /// `cut` is an explicit authorized branch/source change. The caller fences
    /// previous queued work in the same transaction as accepting the new write.
    pub fn prepare(
        &self,
        source: B,
        input: Input,
        now: Instant,
        transmitted: Transmitted,
        cut: bool,
    ) -> Result<(Projected, Self), Rejected> {
        let mut next = *self;
        let epoch = match self.epoch {
            Some(epoch) if epoch.source == source && !cut => epoch,
            Some(previous) if previous.source != source && cut => {
                let reserved_gap = self
                    .sequence_high
                    .checked_add(2)
                    .ok_or(Rejected::IndexExhausted)?;
                let duration = now
                    .checked_duration_since(previous.wallclock)
                    .ok_or(Rejected::Clock)?;
                let ticks = duration
                    .as_nanos()
                    .saturating_mul(u128::from(self.clock_rate))
                    / 1_000_000_000;
                let extrapolated = previous
                    .anchor_timestamp
                    .checked_add(u64::try_from(ticks).map_err(|_| Rejected::Clock)?)
                    .ok_or(Rejected::Clock)?;
                let sr_floor = transmitted
                    .sender_report
                    .map_or(extrapolated, |sr| sr.max(extrapolated));
                let media_floor = self
                    .timestamp_high
                    .checked_add(self.sample_interval)
                    .ok_or(Rejected::Clock)?;
                start_epoch(source, input, reserved_gap, sr_floor.max(media_floor), now)
            }
            None if !cut => {
                start_epoch(source, input, self.sequence_high, self.timestamp_high, now)
            }
            _ => return Err(Rejected::Stale),
        };
        let position = input
            .sequence
            .checked_sub(epoch.input_sequence)
            .ok_or(Rejected::Stale)?;
        let sequence = epoch
            .output_sequence
            .checked_add(position)
            .filter(|index| *index <= MAX_SRTP_INDEX)
            .ok_or(Rejected::IndexExhausted)?;
        let reference = transmitted.primary.unwrap_or(self.initial_sequence);
        if sequence.saturating_sub(reference) >= HALF_SEQUENCE_RANGE {
            return Err(Rejected::AmbiguousEpoch);
        }
        if reference.saturating_sub(sequence) >= u64::from(self.replay_positions) {
            return Err(Rejected::Stale);
        }
        let timestamp = input.timestamp.wrapping_add(epoch.timestamp_offset);
        if let Some(extended) =
            unwrap_timestamp(timestamp, self.timestamp_high.max(epoch.anchor_timestamp))
            && extended > self.timestamp_high
        {
            if next.epoch.is_some_and(|old| old.source == epoch.source) {
                next.sample_interval = extended
                    .saturating_sub(self.timestamp_high)
                    .clamp(1, u64::from(self.clock_rate));
            }
            next.timestamp_high = extended;
        }
        next.sequence_high = self.sequence_high.max(sequence);
        next.epoch = Some(epoch);
        Ok((
            Projected {
                sequence,
                timestamp,
                anchor_timestamp: epoch.anchor_timestamp,
                wallclock: epoch.wallclock,
            },
            next,
        ))
    }
}

fn start_epoch<B>(
    source: B,
    input: Input,
    sequence: u64,
    timestamp: u64,
    wallclock: Instant,
) -> Epoch<B> {
    Epoch {
        source,
        input_sequence: input.sequence,
        output_sequence: sequence,
        timestamp_offset: low32(timestamp).wrapping_sub(input.timestamp),
        anchor_timestamp: timestamp,
        wallclock,
    }
}

fn unwrap_timestamp(raw: u32, anchor: u64) -> Option<u64> {
    let ahead = raw.wrapping_sub(low32(anchor));
    match ahead.cmp(&TIMESTAMP_HALF) {
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Less => anchor.checked_add(u64::from(ahead)),
        std::cmp::Ordering::Greater => anchor.checked_sub(u64::from(ahead.wrapping_neg())),
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "RTP carries only the low 32 clock bits"
)]
fn low32(timestamp: u64) -> u32 {
    timestamp as u32
}

#[cfg(test)]
mod tests;
