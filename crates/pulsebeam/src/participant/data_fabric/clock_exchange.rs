use pulsebeam_clock::{
    domain::{ActiveExchange, Config, LocalOrigin, ReferenceQuality, ReferenceSample},
    time::{Binding, DomainId, Generation},
};
use pulsebeam_proto::state::{
    NativeClockChallenge, NativeClockExchange, native_clock_exchange::Message,
};

use super::*;

const PERIOD: Duration = Duration::from_secs(1);
const MAX_RTT: Duration = Duration::from_millis(250);
const CONFIG: Config = Config {
    track_capacity: 1,
    nominal_delay_ns: 0,
    max_past_ns: 5_000_000_000,
    max_future_ns: 250_000_000,
    observation_max_age_ns: 3_000_000_000,
    max_drift_ppm: 2_000,
    max_exchange_rtt_ns: 250_000_000,
    min_fit_span_ns: 100_000_000,
    lock_uncertainty_ns: 10_000_000,
    reorder_ns: 2_000_000_000,
    history_capacity: 4,
    slew_ppm: 1_000,
    max_phase_error_ns: 1_000_000_000,
};

#[derive(Default)]
pub(super) struct Exchange {
    nonce: u64,
    last_started: Option<Instant>,
    pending: Option<(Instant, u64)>,
}

impl PublisherSource {
    pub(crate) fn clock_control(
        &mut self,
        control: NativeClockExchange,
        now: Instant,
        reference: crate::clock::reference::Estimate,
    ) -> Option<NativeClockExchange> {
        let (id, epoch) = self.ledger.clock.as_ref()?;
        if self.ledger.fenced || *epoch != control.clock_epoch
            || id.as_ref() != pulsebeam_proto::state::codec::NATIVE_CAPTURE_CLOCK_ID
        {
            return None;
        }
        let clock = Clock { id, epoch: *epoch };
        let uncertainty_ns = reference.uncertainty_ns?;
        match control.message? {
            Message::Enable(_) => {
                if self
                    .exchange
                    .last_started
                    .is_some_and(|last| now.saturating_duration_since(last) < PERIOD)
                {
                    return None;
                }
                let nonce = self.exchange.nonce.checked_add(1)?;
                if self
                    .ledger
                    .capture_reference
                    .as_ref()
                    .is_none_or(|probe| probe.host_generation != reference.evidence_generation)
                {
                    self.ledger.capture_reference = Some(Box::new(CaptureReference {
                        domain: ClockDomain::new(
                            CONFIG,
                            Binding {
                                domain: DomainId(self.incarnation.get()),
                                generation: Generation(clock.epoch),
                            },
                            LocalOrigin {
                                at: MonotonicTime::from_nanos(0),
                                global: reference.global,
                            },
                        )
                        .ok()?,
                        origin: now,
                        host_generation: reference.evidence_generation,
                        assigned_frontier: None,
                    }));
                }
                self.exchange.nonce = nonce;
                self.exchange.last_started = Some(now);
                self.exchange.pending = Some((now, nonce));
                Some(NativeClockExchange {
                    owned: control.owned,
                    clock_epoch: control.clock_epoch,
                    message: Some(Message::Challenge(NativeClockChallenge { nonce })),
                })
            }
            Message::Reply(reply) => {
                let (sent, _) = self
                    .exchange
                    .pending
                    .take_if(|(_, nonce)| *nonce == reply.nonce)?;
                let rtt = now.checked_duration_since(sent)?;
                if rtt.is_zero() || rtt > MAX_RTT {
                    return None;
                }
                let processing =
                    u64::try_from(reply.sent_nanos.checked_sub(reply.received_nanos)?).ok()?;
                if u128::from(processing) > rtt.as_nanos() {
                    return None;
                }
                let probe = self.ledger.capture_reference.as_mut()?;
                if probe.host_generation != reference.evidence_generation {
                    return None;
                }
                let source =
                    SourceTime::from_nanos(reply.received_nanos.midpoint(reply.sent_nanos));
                let sent_at = elapsed(probe.origin, sent)?;
                let at = elapsed(probe.origin, now)?;
                let _ = probe.domain.observe_exchange(
                    ActiveExchange {
                        binding: probe.domain.binding(),
                        source,
                        sent_at,
                        reference: ReferenceSample {
                            at,
                            global: reference.global,
                            quality: ReferenceQuality::Qualified { uncertainty_ns },
                        },
                        asymmetry_bound_ns: None,
                    },
                    probe
                        .assigned_frontier
                        .map_or(source, |previous| previous.max(source)),
                    at,
                );
                None
            }
            Message::Challenge(_) => None,
        }
    }
}

fn elapsed(origin: Instant, now: Instant) -> Option<MonotonicTime> {
    let elapsed = now.checked_duration_since(origin)?;
    Some(MonotonicTime::from_nanos(
        u64::try_from(elapsed.as_nanos()).ok()?,
    ))
}
