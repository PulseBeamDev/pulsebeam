// Copyright 2025 The WebRTC Project Authors. All rights reserved.
// Rust adaptation of modules/pacing/pacing_controller.cc at libwebrtc
// 326fd06bb4d93900ed6947205b94cefd2413afca. See congestion/screamv2/LICENSE and PATENTS.

use std::time::{Duration, Instant};

const MAX_DEBT: Duration = Duration::from_millis(500);
const MAX_ELAPSED: Duration = Duration::from_secs(2);
const MAX_QUEUE_TIME: Duration = Duration::from_secs(2);
const MAX_PADDING_REPLAY: Duration = Duration::from_millis(50);
const PADDING_QUANTUM: Duration = Duration::from_millis(5);
const MAX_BURST_BYTES: u128 = 63_000;
// Fractional byte debt is retained instead of rounding on each budget update.
const DEBT_PER_BYTE: u128 = 8_000_000_000;

#[derive(Debug, Default)]
pub(crate) struct Pacer {
    processed_at: Option<Instant>,
    media_debt: u128,
    padding_debt: u128,
    data_rate: u64,
    configured_data_rate: u64,
    padding_rate: u64,
    burst_window: Duration,
    padding_remaining: usize,
    charged_bytes: u64,
}

impl Pacer {
    pub(crate) fn reset(&mut self) {
        let charged_bytes = self.charged_bytes;
        *self = Self {
            charged_bytes,
            ..Self::default()
        };
    }

    pub(crate) fn configure(
        &mut self,
        now: Instant,
        data_rate: u64,
        padding_rate: u64,
        burst_window: Duration,
    ) {
        let padding_rate = padding_rate.min(data_rate);
        // SetPacerConfig changes rates, not the last processed time or byte debt.
        self.data_rate = data_rate;
        self.configured_data_rate = data_rate;
        self.padding_rate = padding_rate;
        self.processed_at.get_or_insert(now);
        self.burst_window = burst_window;
        if padding_rate == 0 {
            self.padding_remaining = 0;
        }
    }

    pub(crate) fn update_queue(&mut self, transport_bytes: u64, average_queue_time: Duration) {
        let time_left = MAX_QUEUE_TIME
            .saturating_sub(average_queue_time)
            .max(Duration::from_millis(1));
        let required = u128::from(transport_bytes)
            .saturating_mul(DEBT_PER_BYTE)
            .checked_div(time_left.as_nanos())
            .unwrap_or(u128::MAX);
        self.data_rate = self
            .configured_data_rate
            .max(u64::try_from(required).unwrap_or(u64::MAX));
    }

    pub(crate) const fn transport_rate(&self) -> u64 {
        self.data_rate
    }

    pub(crate) fn eligible(&self, now: Instant) -> bool {
        self.next_deadline().is_some_and(|deadline| deadline <= now)
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if self.data_rate == 0 {
            return None;
        }
        let at = self.processed_at?;
        let drain = drain_time(self.media_debt, self.data_rate);
        let burst = self.burst_window.min(drain_time(
            MAX_BURST_BYTES.saturating_mul(DEBT_PER_BYTE),
            self.data_rate,
        ));
        // Once a burst exceeds the window, native pacing drains all its debt,
        // rather than sending as soon as it falls just below that window.
        at.checked_add(if drain < burst { Duration::ZERO } else { drain })
    }

    pub(crate) fn padding_deadline(&self) -> Option<Instant> {
        if self.padding_quantum_bytes() == 0 {
            return None;
        }
        if self.padding_remaining > 0 {
            return self.next_deadline();
        }
        self.processed_at?.checked_add(
            drain_time(self.media_debt, self.data_rate)
                .max(drain_time(self.padding_debt, self.padding_rate)),
        )
    }

    /// One native GeneratePadding request, served incrementally through the
    /// caller's bounded prepare/commit loop, never through a second pacer.
    pub(crate) fn prepare_padding(&mut self, now: Instant) -> Option<usize> {
        let Some(due) = self.padding_deadline() else {
            self.process_idle(now);
            return None;
        };
        if due > now {
            self.process_idle(now);
            return None;
        }
        if self.padding_remaining == 0 {
            let earliest = now.checked_sub(MAX_PADDING_REPLAY).unwrap_or(now);
            self.advance(due.max(earliest));
            self.padding_remaining = self.padding_quantum_bytes();
        }
        Some(self.padding_remaining)
    }

    pub(crate) fn process_idle(&mut self, now: Instant) {
        self.advance(now);
    }

    pub(crate) fn commit(&mut self, now: Instant, wire_bytes: usize) {
        self.padding_remaining = 0;
        // Newly arriving media cannot bank unlimited idle-time credit.
        self.advance(now);
        self.charge_sent(wire_bytes);
    }

    pub(crate) fn commit_padding(&mut self, now: Instant, wire_bytes: usize) {
        let earliest = now.checked_sub(MAX_PADDING_REPLAY).unwrap_or(now);
        self.advance(earliest);
        self.padding_remaining = self.padding_remaining.saturating_sub(wire_bytes);
        self.charge_sent(wire_bytes);
    }

    pub(crate) fn padding_unavailable(&mut self) {
        // Upstream charges only padding debt when generation returns no packet.
        // This is not a wire departure and does not increment charged_bytes.
        self.charge_padding(self.padding_remaining);
        self.padding_remaining = 0;
    }

    fn advance(&mut self, at: Instant) {
        let previous = *self.processed_at.get_or_insert(at);
        if at <= previous {
            return;
        }
        let elapsed = at
            .saturating_duration_since(previous)
            .min(MAX_ELAPSED)
            .as_nanos();
        self.media_debt = self
            .media_debt
            .saturating_sub(u128::from(self.data_rate).saturating_mul(elapsed));
        self.padding_debt = self
            .padding_debt
            .saturating_sub(u128::from(self.padding_rate).saturating_mul(elapsed));
        self.processed_at = Some(at);
    }

    fn padding_quantum_bytes(&self) -> usize {
        usize::try_from(
            u128::from(self.padding_rate).saturating_mul(PADDING_QUANTUM.as_nanos())
                / DEBT_PER_BYTE,
        )
        .unwrap_or(usize::MAX)
    }

    fn charge_sent(&mut self, bytes: usize) {
        self.media_debt = self
            .media_debt
            .saturating_add((bytes as u128).saturating_mul(DEBT_PER_BYTE))
            .min(u128::from(self.data_rate).saturating_mul(MAX_DEBT.as_nanos()));
        self.charge_padding(bytes);
        self.charged_bytes = self
            .charged_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    fn charge_padding(&mut self, bytes: usize) {
        self.padding_debt = self
            .padding_debt
            .saturating_add((bytes as u128).saturating_mul(DEBT_PER_BYTE))
            .min(u128::from(self.padding_rate).saturating_mul(MAX_DEBT.as_nanos()));
    }

    #[cfg(test)]
    pub(crate) const fn charged_bytes(&self) -> u64 {
        self.charged_bytes
    }
}

fn drain_time(debt: u128, rate: u64) -> Duration {
    if rate == 0 {
        return if debt == 0 {
            Duration::ZERO
        } else {
            Duration::MAX
        };
    }
    Duration::from_nanos(u64::try_from(debt.div_ceil(u128::from(rate))).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pacer(at: Instant, data: u64, padding: u64, window_ms: u64) -> Pacer {
        let mut pacer = Pacer::default();
        pacer.configure(at, data, padding, Duration::from_millis(window_ms));
        pacer
    }

    #[test]
    fn long_queue_drains_at_native_rate_without_padding_or_debt_refund() {
        let now = Instant::now();
        let mut pacer = pacer(now, 80_000, 40_000, 40);
        pacer.commit(now, 100);
        let media_debt = pacer.media_debt;
        let padding_debt = pacer.padding_debt;
        pacer.update_queue(32_000, Duration::from_millis(1_900));
        assert_eq!(pacer.transport_rate(), 2_560_000);
        assert_eq!(pacer.padding_rate, 40_000);
        assert_eq!(pacer.media_debt, media_debt);
        assert_eq!(pacer.padding_debt, padding_debt);
        pacer.update_queue(32_000, Duration::from_secs(3));
        assert_eq!(pacer.transport_rate(), 256_000_000);
        pacer.update_queue(0, Duration::ZERO);
        assert_eq!(pacer.transport_rate(), 80_000);
        assert_eq!(pacer.media_debt, media_debt);
        assert_eq!(pacer.charged_bytes(), 100);
    }

    #[test]
    fn every_committed_rtp_byte_charges_both_native_debts_once() {
        let now = Instant::now();
        let mut pacer = pacer(now, 80_000, 40_000, 40);
        pacer.commit(now, 100);
        assert_eq!(pacer.media_debt, 100 * DEBT_PER_BYTE);
        assert_eq!(pacer.padding_debt, 100 * DEBT_PER_BYTE);
        assert_eq!(
            pacer.prepare_padding(now + Duration::from_millis(20)),
            Some(25)
        );
        pacer.commit_padding(now + Duration::from_millis(20), 30);
        assert_eq!(pacer.media_debt, 30 * DEBT_PER_BYTE);
        assert_eq!(pacer.padding_debt, 30 * DEBT_PER_BYTE);
        assert_eq!(pacer.charged_bytes(), 130);
    }

    #[test]
    fn burst_can_overshoot_by_one_packet_then_drains_fully() {
        let now = Instant::now();
        let mut pacer = pacer(now, 80_000, 0, 40);
        pacer.commit(now, 300);
        assert!(pacer.eligible(now));
        pacer.commit(now, 300);
        assert!(!pacer.eligible(now + Duration::from_millis(59)));
        assert_eq!(pacer.next_deadline(), Some(now + Duration::from_millis(60)));
        assert!(pacer.eligible(now + Duration::from_millis(60)));
        let mut fast = super::tests::pacer(now, 800_000_000, 0, 40);
        fast.commit(now, 63_000);
        assert!(!fast.eligible(now));
        assert_eq!(fast.next_deadline(), Some(now + Duration::from_micros(630)));
    }

    #[test]
    fn rate_changes_reprice_byte_debt_without_clearing_it() {
        let now = Instant::now();
        for (data, padding, media_ms, padding_ms) in
            [(160_000, 80_000, 50, 100), (40_000, 20_000, 200, 400)]
        {
            let mut pacer = pacer(now, 80_000, 40_000, 0);
            pacer.commit(now, 1_000);
            pacer.configure(
                now + Duration::from_millis(20),
                data,
                padding,
                Duration::ZERO,
            );
            assert_eq!(pacer.media_debt, 1_000 * DEBT_PER_BYTE);
            assert_eq!(pacer.padding_debt, 1_000 * DEBT_PER_BYTE);
            assert_eq!(
                pacer.next_deadline(),
                Some(now + Duration::from_millis(media_ms))
            );
            assert_eq!(
                pacer.padding_deadline(),
                Some(now + Duration::from_millis(padding_ms))
            );
        }
    }

    #[test]
    fn disabled_padding_freezes_debt_until_a_send_applies_the_zero_rate_cap() {
        let now = Instant::now();
        for send in [false, true] {
            let mut pacer = pacer(now, 80_000, 40_000, 0);
            pacer.commit(now, 500);
            pacer.process_idle(now + Duration::from_millis(20));
            pacer.configure(now + Duration::from_millis(20), 80_000, 0, Duration::ZERO);
            assert_eq!(pacer.padding_debt, 400 * DEBT_PER_BYTE);
            if send {
                pacer.commit(now + Duration::from_millis(50), 100);
            }
            pacer.process_idle(now + Duration::from_millis(100));
            pacer.configure(
                now + Duration::from_millis(100),
                80_000,
                40_000,
                Duration::ZERO,
            );
            assert_eq!(
                pacer.padding_deadline(),
                Some(now + Duration::from_millis(if send { 100 } else { 180 }))
            );
        }
    }

    #[test]
    fn padding_generation_is_a_five_ms_quantum_not_a_second_pacer() {
        let now = Instant::now();
        let mut pacer = pacer(now, 8_800_000, 8_000_000, 40);
        let mut packets = 0;
        while pacer.prepare_padding(now).is_some() {
            pacer.commit_padding(now, 1_200);
            packets += 1;
            assert!(packets <= 5);
        }
        assert_eq!(packets, 5);
        assert_eq!(pacer.charged_bytes(), 6_000);
        assert_eq!(pacer.media_debt, 6_000 * DEBT_PER_BYTE);
        assert_eq!(pacer.padding_debt, 6_000 * DEBT_PER_BYTE);
        assert_eq!(
            pacer.padding_deadline(),
            Some(now + Duration::from_millis(6))
        );
    }

    #[test]
    fn failed_generation_only_charges_padding_budget_not_wire_bytes() {
        let now = Instant::now();
        let mut pacer = pacer(now, 8_800_000, 8_000_000, 40);
        assert_eq!(pacer.prepare_padding(now), Some(5_000));
        pacer.padding_unavailable();
        assert_eq!(pacer.media_debt, 0);
        assert_eq!(pacer.padding_debt, 5_000 * DEBT_PER_BYTE);
        assert_eq!(pacer.charged_bytes(), 0);
        assert_eq!(
            pacer.padding_deadline(),
            Some(now + Duration::from_millis(5))
        );
    }

    #[test]
    fn stale_padding_processing_replays_at_most_fifty_ms_and_one_quantum() {
        let now = Instant::now();
        let mut pacer = pacer(now, 880_000, 800_000, 40);
        assert_eq!(pacer.prepare_padding(now), Some(500));
        pacer.commit_padding(now, 500);
        let later = now + Duration::from_secs(1);
        let mut packets = 0;
        while pacer.prepare_padding(later).is_some() {
            pacer.commit_padding(later, 500);
            packets += 1;
            assert!(packets <= 11);
        }
        assert_eq!(packets, 11);
        assert_eq!(
            pacer.padding_deadline(),
            Some(later + Duration::from_millis(5))
        );
    }

    #[test]
    fn idle_media_does_not_bank_credit_and_debts_are_capped_on_send() {
        let now = Instant::now();
        let mut pacer = pacer(now, 80_000, 40_000, 0);
        pacer.commit(now, 500);
        pacer.commit(now + Duration::from_secs(1), 500);
        assert_eq!(
            pacer.next_deadline(),
            Some(now + Duration::from_millis(1_050))
        );
        pacer.commit(now + Duration::from_secs(2), 100_000);
        assert_eq!(pacer.media_debt, 5_000 * DEBT_PER_BYTE);
        assert_eq!(pacer.padding_debt, 2_500 * DEBT_PER_BYTE);
        assert_eq!(pacer.charged_bytes(), 101_000);
        pacer.reset();
        assert_eq!(pacer.charged_bytes(), 101_000);
        assert_eq!(pacer.next_deadline(), None);
    }
}
