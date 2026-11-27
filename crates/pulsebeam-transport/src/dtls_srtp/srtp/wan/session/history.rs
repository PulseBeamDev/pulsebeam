use std::{
    collections::HashMap,
    num::NonZeroU32,
    time::{Duration, Instant},
};

use pulsebeam_cc::{Controller, EcnMark, EcnValidation, FeedbackSample, Observation, Readiness};
use pulsebeam_feedback::{
    Ecn, Epoch, FeedbackMode, IndependentReport, PacketId, Receipt, Receiver, ReceiverConfig,
    ReceiverTime, ReportOrder, ReportWindow, SendCommit, SendFeedback, SendIdentity, SendReport,
    SendStatus, Sender, SenderConfig, SpaceId,
};

use super::{Binding, Error, Lease, Send, Session};
use crate::dtls_srtp::wan::{
    Direction,
    feedback::{MAX_METRICS, Metric, Report},
    handoff::Submission,
};

const CAPACITY: usize = 8192;
const RETENTION: Duration = Duration::from_secs(2);
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(2);
const BUDGET_FRESHNESS: Duration = Duration::from_millis(500);
const MAX_EXPIRATIONS: usize = 256;
const REPORT_INTERVAL: Duration = Duration::from_millis(10);
const TIMESTAMP_HZ: i64 = 65_536;
const ARRIVAL_HZ: u64 = 1024;
const ARRIVAL_OVER_RANGE: u16 = 0x1ffe;
const ARRIVAL_UNAVAILABLE: u16 = 0x1fff;

pub(super) struct History {
    origin: Instant,
    lease: Option<Lease>,
    epoch: Epoch,
    remote_clock: Option<(u32, i64)>,
    feedback_wait: HashMap<SpaceId, Instant>,
    last_feedback: Option<Instant>,
    controller: Controller,
    ecn: EcnValidation,
    samples: Vec<FeedbackSample>,
    pub(super) send: Sender,
    pub(super) receive: Receiver,
}

impl History {
    pub(super) fn new(binding: Binding, origin: Instant) -> Result<Self, Error> {
        let epoch = Epoch(binding.epoch.get());
        let send = Sender::new(
            SenderConfig {
                capacity: CAPACITY,
                retention: RETENTION,
                max_expirations_per_poll: MAX_EXPIRATIONS,
                max_ack_reordering: 1024,
                max_feedback_statuses: MAX_METRICS,
                initial_reordering_window: Duration::from_millis(30),
                max_reordering_window: Duration::from_millis(200),
            },
            FeedbackMode::Independent,
            8 * 1024 * 1024,
        )
        .map_err(|_| Error::Capacity)?;
        let mut receive = Receiver::new(
            ReceiverConfig {
                capacity: CAPACITY,
                max_report_statuses: MAX_METRICS,
                interval: REPORT_INTERVAL,
                order: ReportOrder::Independent,
            },
            epoch,
        )
        .map_err(|_| Error::Capacity)?;
        receive.reserve(1024 * 1024).map_err(|_| Error::Capacity)?;
        Ok(Self {
            origin,
            lease: None,
            epoch,
            remote_clock: None,
            feedback_wait: HashMap::new(),
            last_feedback: None,
            controller: Controller::new(Default::default()).map_err(|_| Error::Capacity)?,
            ecn: EcnValidation::default(),
            samples: Vec::new(),
            send,
            receive,
        })
    }

    pub(super) fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.origin)
    }

    pub(super) fn sync(&mut self, lease: Option<Lease>, now: Instant) -> Result<(), Error> {
        if self.lease != lease {
            // The session already fences stale path tickets; reset all observations
            // on path change without borrowing a new SRTP key or reusing an SSRC.
            self.send.clear(self.epoch);
            self.send.set_availability(self.epoch, lease.is_some());
            self.receive.clear(self.epoch);
            self.remote_clock = None;
            self.feedback_wait.clear();
            self.last_feedback = None;
            self.ecn = EcnValidation::default();
            self.lease = lease;
            self.controller = Controller::new(Default::default()).map_err(|_| Error::Capacity)?;
            self.update_controller(now)?;
        }
        Ok(())
    }

    pub(super) fn instant(&self, elapsed: Duration) -> Option<Instant> {
        self.origin.checked_add(elapsed)
    }

    pub(super) fn send_budget(&self, now: Instant, lease: Lease) -> Option<super::SendBudget> {
        let feedback_at = self.last_feedback?;
        let age = now.checked_duration_since(feedback_at)?;
        let snapshot = self.controller.snapshot();
        (self.lease == Some(lease) && age < BUDGET_FRESHNESS && !snapshot.feedback_stale).then_some(
            super::SendBudget {
                feedback_at,
                valid_until: feedback_at.checked_add(BUDGET_FRESHNESS)?,
                payload_bps: snapshot.target_media_payload_rate,
                pacing_bps: snapshot.nominal_pacing_transport_rate,
            },
        )
    }

    pub(super) fn ready(&self, now: Instant, bytes: usize, reserve_bytes: usize) -> Readiness {
        let flight = self.send.observations().bytes_in_flight;
        let window = self.controller.snapshot().max_rtp_bytes_in_flight;
        let size = u64::try_from(bytes).unwrap_or(u64::MAX);
        let reserve = u64::try_from(reserve_bytes)
            .unwrap_or(u64::MAX)
            .min(window.saturating_sub(size));
        self.controller
            .ready(self.elapsed(now), flight.saturating_add(reserve), bytes)
    }

    pub(super) fn deadline(&self, now: Instant) -> Option<Instant> {
        let elapsed = self.elapsed(now);
        self.send
            .next_deadline(elapsed)
            .into_iter()
            .chain(self.receive.deadline())
            .filter_map(|next| self.origin.checked_add(next))
            .chain(self.feedback_deadline())
            .min()
    }

    pub(super) fn poll(&mut self, now: Instant) -> Result<(), Error> {
        self.send.clear_observations();
        self.send.expire(self.elapsed(now), MAX_EXPIRATIONS);
        self.update_controller(now)
    }

    fn update_controller(&mut self, now: Instant) -> Result<(), Error> {
        let elapsed = self.elapsed(now);
        let input = self.send.observations();
        if input.exhausted {
            return Err(Error::Capacity);
        }
        for receipt in input
            .feedback
            .iter()
            .filter(|sample| sample.received && sample.tag != 0)
        {
            match receipt.ecn {
                Some(Ecn::Ect0 | Ecn::Ce) => self.ecn.classic = true,
                _ => self.ecn.bleached = true,
            }
        }
        self.samples.clear();
        self.samples.extend(
            input
                .feedback
                .iter()
                .chain(&input.synthetic)
                .filter_map(|item| {
                    if !(item.received || item.newly_acked || item.reported_lost_for_the_first_time)
                    {
                        return None;
                    }
                    Some(FeedbackSample {
                        sent_at: item.committed_at,
                        received_at: item.observed_at.unwrap_or(elapsed),
                        transport_bytes: item.transport_bytes,
                        received: item.received,
                        newly_acked: item.newly_acked,
                        lost: item.reported_lost_for_the_first_time,
                        recovered: item.recovered,
                        receiver_arrival_micros: match item.receiver_arrival {
                            Some(ReceiverTime::Micros(value)) => Some(value),
                            _ => None,
                        },
                        ecn: match (item.tag != 0, item.ecn) {
                            (true, Some(Ecn::Ect0)) => Some(EcnMark::Ect0),
                            (true, Some(Ecn::Ce)) => Some(EcnMark::Ce),
                            _ => None,
                        },
                    })
                }),
        );
        self.controller
            .update(
                elapsed,
                Observation {
                    path_epoch: self.lease.map(|_| self.epoch.0),
                    path_available: self.lease.is_some(),
                    feedback: &self.samples,
                    feedback_hold: input
                        .timing
                        .and_then(|timing| timing.feedback_hold)
                        .unwrap_or_default(),
                    fresh_network_feedback: input.fresh_network_feedback,
                    bytes_in_flight: input.bytes_in_flight,
                    paced_queue_bytes: 0,
                    desired_media_rate: u64::MAX,
                    ecn: self.ecn,
                },
            )
            .map_err(|_| Error::Time)?;
        Ok(())
    }

    pub(super) fn prepare(
        &mut self,
        now: Instant,
        ssrc: NonZeroU32,
        position: u64,
        wire_len: usize,
    ) -> Result<SendCommit, Error> {
        let commit = SendCommit {
            at: self.elapsed(now),
            wire_len,
            path_epoch: self.epoch,
            identity: SendIdentity {
                space: space(ssrc),
                packet: PacketId(position),
                feedback_sequence: None,
                tag: 0,
            },
        };
        self.send
            .prepare_commit(commit)
            .map_err(|_| Error::Capacity)?;
        Ok(commit)
    }

    pub(super) fn accepted(
        &mut self,
        now: Instant,
        mut commit: SendCommit,
        ect0: bool,
    ) -> Result<(), Error> {
        commit.identity.tag = u64::from(ect0);
        self.send
            .commit_preflighted(commit)
            .map_err(|_| Error::Capacity)?;
        self.controller.commit_send(
            commit.at,
            commit.wire_len,
            self.send.observations().bytes_in_flight,
            false,
        );
        self.feedback_wait
            .entry(commit.identity.space)
            .or_insert(now);
        Ok(())
    }

    pub(super) fn retire_send(&mut self, ssrc: NonZeroU32) {
        let space = space(ssrc);
        self.feedback_wait.remove(&space);
        self.send.retire_source(space);
    }

    fn feedback_deadline(&self) -> Option<Instant> {
        self.feedback_wait
            .values()
            .min()?
            .checked_add(FEEDBACK_TIMEOUT)
    }

    pub(super) fn observe(
        &mut self,
        now: Instant,
        ssrc: NonZeroU32,
        position: u64,
        ecn: Option<Ecn>,
    ) {
        let receipt = Receipt {
            space: space(ssrc),
            packet: PacketId(position),
            arrived_at: self.elapsed(now),
            ecn,
            tag: position,
        };
        let _ = self.receive.observe(self.epoch, receipt);
    }

    fn encode(
        &self,
        window: ReportWindow,
        ssrc: NonZeroU32,
        now: Duration,
    ) -> Result<Vec<u8>, Error> {
        let ticks = now.as_micros().saturating_mul(TIMESTAMP_HZ as u128) / 1_000_000;
        let timestamp = u32::try_from(ticks & u128::from(u32::MAX)).map_err(|_| Error::Time)?;
        let mut metrics = vec![Metric::MISSING; window.count];
        for receipt in self.receive.report_receipts(window) {
            let ecn = match receipt.ecn {
                Some(Ecn::Ect1) => 1,
                Some(Ecn::Ect0) => 2,
                Some(Ecn::Ce) => 3,
                Some(Ecn::NotEct) | None => 0,
            };
            let arrival = now.saturating_sub(receipt.arrived_at).as_micros();
            let offset = (arrival.saturating_mul(u128::from(ARRIVAL_HZ)) / 1_000_000)
                .min(u128::from(ARRIVAL_OVER_RANGE));
            let offset = u16::try_from(offset).map_err(|_| Error::Time)?;
            let index = receipt
                .packet
                .0
                .checked_sub(window.base.0)
                .and_then(|index| usize::try_from(index).ok())
                .ok_or(Error::Window)?;
            *metrics.get_mut(index).ok_or(Error::Window)? =
                Metric::received(ecn, offset).ok_or(Error::Window)?;
        }
        let begin =
            u16::try_from(window.base.0 & u64::from(u16::MAX)).map_err(|_| Error::Window)?;
        Report::encode(ssrc, begin, timestamp, &metrics)
            .map_err(|error| Error::Protection(error.into()))
    }

    fn apply(
        &mut self,
        now: Instant,
        index: u32,
        base: u64,
        report: Report<'_>,
    ) -> Result<(), Error> {
        let elapsed = self.elapsed(now);
        let timestamp = if let Some((_, previous)) = self.remote_clock {
            let wire_previous =
                u32::try_from(previous & i64::from(u32::MAX)).map_err(|_| Error::Time)?;
            let delta =
                i32::from_ne_bytes(report.timestamp.wrapping_sub(wire_previous).to_ne_bytes());
            previous.checked_add(i64::from(delta)).ok_or(Error::Time)?
        } else {
            i64::from(report.timestamp)
        };
        if self
            .remote_clock
            .is_none_or(|(previous, _)| index > previous)
        {
            self.remote_clock = Some((index, timestamp));
        }
        let report_micros = timestamp.saturating_mul(1_000_000) / TIMESTAMP_HZ;
        let statuses: Vec<_> = report
            .metrics()
            .map(|metric| {
                let Some((ecn, offset)) = metric.observation() else {
                    return SendStatus {
                        received: false,
                        arrival: None,
                        ecn: None,
                        hold: None,
                    };
                };
                let (arrival, hold) = match offset {
                    ARRIVAL_OVER_RANGE => (ReceiverTime::OverRange, None),
                    ARRIVAL_UNAVAILABLE => (ReceiverTime::Unavailable, None),
                    _ => {
                        let micros = u64::from(offset).saturating_mul(1_000_000) / ARRIVAL_HZ;
                        let arrival =
                            report_micros.saturating_sub(i64::try_from(micros).unwrap_or(i64::MAX));
                        (
                            ReceiverTime::Micros(arrival),
                            Some(Duration::from_micros(micros)),
                        )
                    }
                };
                SendStatus {
                    received: true,
                    arrival: Some(arrival),
                    ecn: Some(match ecn {
                        1 => Ecn::Ect1,
                        2 => Ecn::Ect0,
                        3 => Ecn::Ce,
                        _ => Ecn::NotEct,
                    }),
                    hold,
                }
            })
            .collect();
        self.send.clear_observations();
        self.send.process_feedback(SendFeedback {
            received_at: elapsed,
            path_epoch: self.epoch,
            report: SendReport::Independent {
                reports: &[IndependentReport {
                    space: space(report.ssrc),
                    base: PacketId(base),
                    generation: i64::from(index),
                    statuses: &statuses,
                }],
            },
        });
        if self.send.observations().fresh_network_feedback {
            self.last_feedback = Some(now);
            self.feedback_wait.retain(|&space, since| {
                *since = now;
                self.send.source_has_in_flight(space)
            });
        }
        self.update_controller(now)
    }
}

impl Session {
    pub fn ecn_marking_allowed(&self) -> bool {
        !self.history.ecn.bleached
    }

    pub(super) fn enforce_feedback_deadline(&mut self, now: Instant) -> Result<(), Error> {
        let due = |history: &History| {
            history
                .feedback_deadline()
                .is_some_and(|deadline| now >= deadline)
        };
        if !due(&self.history) || self.lease(now).is_none() {
            return Ok(());
        }
        // A window expiring at the deadline no longer requires feedback.
        self.prune_windows()?;
        if due(&self.history) {
            self.close();
            return Err(Error::FeedbackTimeout);
        }
        Ok(())
    }

    pub fn receive_feedback(
        &mut self,
        now: Instant,
        binding: Binding,
        wire: &[u8],
    ) -> Result<(), Error> {
        if binding != self.binding || self.lease(now).is_none() {
            return Err(Error::Window);
        }
        self.advance(now)?;
        self.enforce_feedback_deadline(now)?;
        let (plaintext, index) = self
            .protector
            .as_mut()
            .ok_or(Error::Window)?
            .unseal_feedback(wire)
            .map_err(Error::Protection)?;
        let report = Report::parse(&plaintext).map_err(|error| Error::Protection(error.into()))?;
        let spec = self
            .feedback_window(Direction::Send, report.ssrc, now)
            .ok_or(Error::Window)?;
        let offset =
            u16::try_from(report.metrics().len().saturating_sub(1)).map_err(|_| Error::Window)?;
        let last = report.begin.checked_add(offset).ok_or(Error::Window)?;
        let base = spec.admits(report.begin).ok_or(Error::Window)?;
        spec.admits(last).ok_or(Error::Window)?;
        let result = self.history.apply(now, index, base, report);
        if result.is_err() {
            self.close();
        }
        result
    }

    pub(super) fn flush_feedback(&mut self, send: &mut Send<'_>) -> Result<(), Error> {
        let now = self.now;
        let elapsed = self.history.elapsed(now);
        let Some(window) = self.history.receive.report(elapsed) else {
            return Ok(());
        };
        let ssrc = u32::try_from(window.space.0).ok().and_then(NonZeroU32::new);
        let route = ssrc.and_then(|ssrc| self.feedback_window(Direction::Receive, ssrc, now));
        let Some(spec) = route else {
            let _ = self.history.receive.accept(window, window.count, elapsed);
            return Ok(());
        };
        let bytes = self.history.encode(window, spec.ssrc, elapsed)?;
        let (Some(protector), Some(path)) = (&mut self.protector, &mut self.path) else {
            return Ok(());
        };
        let accepted = protector
            .prepare_feedback(path, now, spec.expires, &bytes)
            .is_ok_and(|prepared| {
                matches!(
                    protector.submit(path, now, prepared, send),
                    Submission::Accepted { .. }
                )
            });
        if accepted {
            let _ = self.history.receive.accept(window, window.count, elapsed);
        } else {
            let _ = self
                .history
                .receive
                .defer(elapsed, elapsed.saturating_add(REPORT_INTERVAL));
        }
        Ok(())
    }
}

pub(super) fn space(ssrc: NonZeroU32) -> SpaceId {
    SpaceId(u64::from(ssrc.get()))
}
