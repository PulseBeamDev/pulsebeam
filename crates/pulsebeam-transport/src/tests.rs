#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_methods,
    reason = "bounded deterministic transport fixtures with a single host clock anchor"
)]

use super::*;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime};

struct Pair {
    a: Transport,
    b: Transport,
    aa: SocketAddr,
    ba: SocketAddr,
    ab: VecDeque<Vec<u8>>,
    ba_out: VecDeque<Vec<u8>>,
    received: Vec<(Vec<u8>, [u8; 4], ReceiveTime)>,
    now: Time,
}

impl Pair {
    fn new(wrong_pin: bool) -> Self {
        Self::with_backend(wrong_pin, false)
    }

    fn with_backend(wrong_pin: bool, quic: bool) -> Self {
        if quic {
            return Self::quic(wrong_pin);
        }
        let ca = Credential::generate().unwrap();
        let cb = Credential::generate().unwrap();
        let aa = "127.0.0.1:5000".parse().unwrap();
        let ba = "127.0.0.1:5001".parse().unwrap();
        let expected_peer = if wrong_pin {
            ca.fingerprint()
        } else {
            cb.fingerprint()
        };
        let a = Transport::new(Peer {
            endpoint: ba,
            credential: ca.clone(),
            expected_peer,
            role: PeerRole::Initiator,
            class: NetworkClass::Lan,
        })
        .unwrap();
        let b = Transport::new(Peer {
            endpoint: aa,
            credential: cb,
            expected_peer: ca.fingerprint(),
            role: PeerRole::Responder,
            class: NetworkClass::Wan,
        })
        .unwrap();
        Self {
            a,
            b,
            aa,
            ba,
            ab: VecDeque::new(),
            ba_out: VecDeque::new(),
            received: Vec::new(),
            now: Time(0),
        }
    }

    fn quic(wrong_pin: bool) -> Self {
        Self::quic_profile(wrong_pin, false)
    }

    fn quic_profile(wrong_pin: bool, wrong_client: bool) -> Self {
        Self::quic_config(wrong_pin, wrong_client, QuicTuning::default())
    }

    fn quic_config(wrong_pin: bool, wrong_client: bool, tuning: QuicTuning) -> Self {
        let ca = QuicCredential::generate().unwrap();
        let cb = QuicCredential::generate().unwrap();
        let aa = "127.0.0.1:5000".parse().unwrap();
        let ba = "127.0.0.1:5001".parse().unwrap();
        let a = Transport::new(QuicPeer {
            endpoint: ba,
            credential: ca.clone(),
            expected_peer: if wrong_pin {
                ca.fingerprint()
            } else {
                cb.fingerprint()
            },
            role: PeerRole::Initiator,
            class: NetworkClass::Lan,
            tuning,
        })
        .unwrap();
        let expected_client = if wrong_client {
            cb.fingerprint()
        } else {
            ca.fingerprint()
        };
        let b = Transport::new(QuicPeer {
            endpoint: aa,
            credential: cb,
            expected_peer: expected_client,
            role: PeerRole::Responder,
            class: NetworkClass::Wan,
            tuning: QuicTuning::default(),
        })
        .unwrap();
        Self {
            a,
            b,
            aa,
            ba,
            ab: VecDeque::new(),
            ba_out: VecDeque::new(),
            received: Vec::new(),
            now: Time(0),
        }
    }

    fn start(&mut self) {
        let anchor = Instant::now();
        self.a.start(self.now, anchor).unwrap();
        self.b.start(self.now, anchor).unwrap();
    }

    fn poll(&mut self, budget: usize) -> Result<(), Error> {
        let now = self.now;
        let output = &mut self.ab;
        assert!(
            self.a.poll(
                now,
                budget,
                |datagram| {
                    assert!(datagram.payload.len() <= UDP_BUDGET);
                    output.push_back(datagram.payload.to_vec());
                    SendOutcome::Accepted {
                        bytes: datagram.payload.len(),
                    }
                },
                |_| {}
            )? <= budget
        );
        let output = &mut self.ba_out;
        let received = &mut self.received;
        assert!(
            self.b.poll(
                now,
                budget,
                |datagram| {
                    output.push_back(datagram.payload.to_vec());
                    SendOutcome::Accepted {
                        bytes: datagram.payload.len(),
                    }
                },
                |application| received.push((
                    application.payload.to_vec(),
                    application.context,
                    application.observed
                ))
            )? <= budget
        );
        Ok(())
    }

    fn step(&mut self) -> Result<(), Error> {
        self.now.0 += 1_000_000;
        self.poll(16)?;
        let observed = ReceiveTime {
            monotonic: self.now,
            wall: Some(SystemTime::UNIX_EPOCH),
        };
        for _ in 0..64 {
            let mut progress = false;
            if !self.b.needs_progress()
                && let Some(bytes) = self.ab.pop_front()
            {
                self.b.receive(self.aa, &bytes, observed)?;
                progress = true;
            }
            if !self.a.needs_progress()
                && let Some(bytes) = self.ba_out.pop_front()
            {
                self.a.receive(self.ba, &bytes, observed)?;
                progress = true;
            }
            if !progress {
                break;
            }
            self.poll(16)?;
        }
        Ok(())
    }

    fn connect(&mut self) {
        self.start();
        for _ in 0..5000 {
            self.step().unwrap();
            if self.a.health().ready && self.b.health().ready {
                return;
            }
        }
        panic!("handshake did not finish");
    }
}

#[test]
fn standalone_datagrams_preserve_context_boundaries_and_original_observations() {
    for quic in [false, true] {
        standalone_datagrams_preserve_context_boundaries_and_original_observations_case(quic);
    }
}

fn standalone_datagrams_preserve_context_boundaries_and_original_observations_case(quic: bool) {
    let mut pair = Pair::with_backend(false, quic);
    pair.connect();
    let payload = vec![19; pair.a.payload_capacity()];
    let expiry = Time(pair.now.0 + 1_000_000_000);
    pair.a.try_submit([1, 2, 3, 4], &payload, expiry).unwrap();
    assert_eq!(pair.a.try_submit([0; 4], &[], expiry), Err(Error::Blocked));
    assert_eq!(pair.a.health().bytes_in_flight, 0);
    assert_eq!(pair.a.health().staging.datagrams, 1);
    assert_eq!(pair.a.health().staging.oldest_age, Some(Duration::ZERO));
    for _ in 0..100 {
        pair.step().unwrap();
    }
    assert_eq!(pair.a.health().staging.datagrams, 0);
    assert_eq!(pair.received.len(), 1);
    assert_eq!(pair.received[0].0, payload);
    assert_eq!(pair.received[0].1, [1, 2, 3, 4]);
    assert_eq!(pair.received[0].2.wall, Some(SystemTime::UNIX_EPOCH));
    assert_eq!(pair.a.health().bytes_in_flight, 0);
    assert_eq!(pair.a.health().sent, 1);
    assert_eq!(pair.b.health().received, 1);
}

#[test]
fn explicit_lifecycle_and_rejections_do_not_consume_caller_data() {
    for quic in [false, true] {
        explicit_lifecycle_and_rejections_do_not_consume_caller_data_case(quic);
    }
}

fn explicit_lifecycle_and_rejections_do_not_consume_caller_data_case(quic: bool) {
    let mut pair = Pair::with_backend(false, quic);
    let payload = vec![7; PAYLOAD_BUDGET + 1];
    assert_eq!(
        pair.a.try_submit([0; 4], &payload, Time(1)),
        Err(Error::NotStarted)
    );
    pair.start();
    assert_eq!(
        pair.a.try_submit([0; 4], &[], Time(1)),
        Err(Error::NotReady)
    );
    assert_eq!(pair.a.start(Time(0), Instant::now()), Err(Error::Busy));
    pair.a.close();
    assert_eq!(
        pair.a.try_submit([0; 4], &payload, Time(1)),
        Err(Error::Closed)
    );
    assert_eq!(pair.a.start(Time(0), Instant::now()), Err(Error::Closed));
    assert_eq!(payload, vec![7; PAYLOAD_BUDGET + 1]);
}

#[test]
fn staging_rechecks_expiry_and_never_accounts_failed_or_short_socket_submissions() {
    let mut pair = Pair::new(false);
    pair.connect();
    let now = pair.now;
    assert_eq!(
        pair.a
            .try_submit([0; 4], &[0; PAYLOAD_BUDGET + 1], Time(now.0 + 1)),
        Err(Error::Oversize)
    );
    assert_eq!(pair.a.try_submit([0; 4], &[], now), Err(Error::Expired));
    pair.a.try_submit([0; 4], &[1], Time(now.0 + 1)).unwrap();
    let mut submitted = 0;
    pair.a
        .poll(
            Time(now.0 + 1),
            64,
            |_| {
                submitted += 1;
                SendOutcome::Rejected
            },
            |_| {},
        )
        .unwrap();
    assert_eq!(submitted, 0);
    assert_eq!(pair.a.health().expired, 1);
    assert_eq!(pair.a.health().bytes_in_flight, 0);
    assert!(pair.a.needs_progress());
    pair.a
        .poll(Time(now.0 + 1), 64, |_| SendOutcome::Rejected, |_| {})
        .unwrap();
    assert!(pair.a.input_ready());
    for short in [false, true] {
        pair.a
            .try_submit([0; 4], &[1], Time(now.0 + 1_000_000_000))
            .unwrap();
        pair.a
            .poll(
                Time(now.0 + 1),
                64,
                |datagram| {
                    if short {
                        SendOutcome::Accepted {
                            bytes: datagram.payload.len() - 1,
                        }
                    } else {
                        SendOutcome::Rejected
                    }
                },
                |_| {},
            )
            .unwrap();
        assert_eq!(pair.a.health().bytes_in_flight, 0);
        assert_eq!(pair.a.health().sent, 0);
    }
    assert_eq!(pair.a.health().send_failed, 2);
}

#[test]
fn bounded_progress_busy_input_wrong_address_and_time_regression_are_explicit() {
    for quic in [false, true] {
        bounded_progress_busy_input_wrong_address_and_time_regression_are_explicit_case(quic);
    }
}

fn bounded_progress_busy_input_wrong_address_and_time_regression_are_explicit_case(quic: bool) {
    let mut pair = Pair::with_backend(false, quic);
    pair.start();
    let observed = ReceiveTime {
        monotonic: Time(0),
        wall: None,
    };
    assert_eq!(pair.a.receive(pair.ba, &[0], observed), Err(Error::Busy));
    pair.poll(0).unwrap();
    assert!(pair.a.needs_progress());
    pair.connect_after_start();
    let invalid = pair.a.health().invalid;
    pair.a
        .receive(
            pair.aa,
            &[0],
            ReceiveTime {
                monotonic: pair.now,
                wall: None,
            },
        )
        .unwrap();
    pair.a
        .receive(
            pair.ba,
            &[0; UDP_BUDGET + 1],
            ReceiveTime {
                monotonic: pair.now,
                wall: None,
            },
        )
        .unwrap();
    assert_eq!(pair.a.health().invalid, invalid + 2);
    assert_eq!(
        pair.a
            .poll(Time(pair.now.0 - 1), 1, |_| SendOutcome::Rejected, |_| {}),
        Err(Error::Time)
    );
    assert!(!pair.a.health().ready);
    assert_eq!(pair.a.health().closed, Some(Error::Time));
}

impl Pair {
    fn connect_after_start(&mut self) {
        for _ in 0..5000 {
            self.step().unwrap();
            if self.a.health().ready && self.b.health().ready {
                return;
            }
        }
        panic!("handshake did not finish");
    }
}

#[test]
fn wrong_identity_cannot_become_ready_even_with_bounded_handshake_work() {
    for quic in [false, true] {
        wrong_identity_cannot_become_ready_even_with_bounded_handshake_work_case(quic);
    }
}

fn wrong_identity_cannot_become_ready_even_with_bounded_handshake_work_case(quic: bool) {
    let mut pair = Pair::with_backend(true, quic);
    pair.start();
    for _ in 0..5000 {
        match pair.step() {
            Err(Error::Security | Error::Closed) => {
                assert!(!pair.a.health().ready);
                return;
            }
            Ok(()) => {}
            Err(error) => panic!("unexpected error: {error:?}"),
        }
    }
    panic!("wrong certificate was not rejected");
}

#[test]
fn one_blocked_association_does_not_starve_an_independent_peer() {
    let mut blocked = Pair::new(false);
    blocked.start();
    let mut live = Pair::new(false);
    live.start();
    for _ in 0..5000 {
        blocked.now.0 += 1_000_000;
        blocked.poll(1).unwrap();
        blocked.ab.clear();
        blocked.ba_out.clear();
        live.step().unwrap();
        if live.a.health().ready && live.b.health().ready {
            break;
        }
    }
    assert!(!blocked.a.health().ready);
    assert!(live.a.health().ready && live.b.health().ready);
    live.a
        .try_submit([0; 4], &[42], Time(live.now.0 + 1_000_000_000))
        .unwrap();
    for _ in 0..100 {
        live.step().unwrap();
    }
    assert_eq!(live.received.len(), 1);
    assert_eq!(live.received[0].0, [42]);
}

#[test]
fn stalled_host_does_not_create_an_unbounded_catchup_burst() {
    let mut pair = Pair::new(false);
    pair.connect();
    pair.now.0 += 30_000_000_000;
    pair.poll(1).unwrap();
    assert!(pair.ab.len() <= 1);
    assert!(pair.ba_out.len() <= 1);
    assert!(pair.received.is_empty());
}

#[test]
fn quic_retains_unsent_ciphertext_and_expires_it_without_outer_commit() {
    for expires in [false, true] {
        let mut pair = Pair::quic(false);
        pair.connect();
        for _ in 0..20 {
            pair.step().unwrap();
        }
        let accepted_at = pair.now;
        let expiry = if expires {
            Time(pair.now.0 + 2_000_000)
        } else {
            Time(u64::MAX)
        };
        pair.a.try_submit([7; 4], b"retained", expiry).unwrap();
        let mut rejected = Vec::new();
        pair.a
            .poll(
                pair.now,
                256,
                |datagram| {
                    rejected.push(datagram.payload.to_vec());
                    SendOutcome::Rejected
                },
                |_| panic!("unexpected application input"),
            )
            .unwrap();
        assert_eq!(
            rejected.len(),
            1,
            "at most one unsent UDP output is retained"
        );
        assert_eq!(pair.a.health().sent, 0);
        assert_eq!(pair.a.health().bytes_in_flight, 0);
        assert_eq!(pair.a.health().pending_udp_bytes, rejected[0].len());
        assert!(
            !pair.a.needs_progress(),
            "socket backpressure must not busy-spin"
        );
        assert!(!pair.a.input_ready());
        assert!(pair.a.next_deadline().unwrap() > accepted_at);
        if expires {
            pair.now.0 += 3_000_000;
            pair.a
                .poll(
                    pair.now,
                    256,
                    |datagram| {
                        assert_ne!(
                            datagram.payload, rejected[0],
                            "expired application ciphertext reached the socket"
                        );
                        SendOutcome::Accepted {
                            bytes: datagram.payload.len(),
                        }
                    },
                    |_| {},
                )
                .unwrap();
            assert_eq!(pair.a.health().expired, 1);
            assert_eq!(pair.a.health().sent, 0);
            assert_eq!(pair.a.health().bytes_in_flight, 0);
            assert_eq!(pair.a.health().pending_udp_bytes, 0);
            assert_eq!(pair.a.health().staging.datagrams, 0);
        } else {
            pair.now.0 += 1_000_000;
            let mut retried = Vec::new();
            pair.a
                .poll(
                    pair.now,
                    256,
                    |datagram| {
                        retried.push(datagram.payload.to_vec());
                        pair.ab.push_back(datagram.payload.to_vec());
                        SendOutcome::Accepted {
                            bytes: datagram.payload.len(),
                        }
                    },
                    |_| {},
                )
                .unwrap();
            assert_eq!(
                retried[0], rejected[0],
                "retry must preserve the serialized QUIC packet"
            );
            assert_eq!(pair.a.health().sent, 1);
            assert_eq!(pair.a.health().pending_udp_bytes, 0);
            assert_eq!(pair.a.health().bytes_in_flight, rejected[0].len() as u64);
            for _ in 0..100 {
                pair.step().unwrap();
            }
            assert_eq!(pair.received.len(), 1);
            assert_eq!(pair.received[0].0, b"retained");
            assert_eq!(pair.a.health().bytes_in_flight, 0);
        }
    }
}

#[test]
fn quic_outer_feedback_silence_closes_even_while_normal_quic_acks_arrive() {
    let mut pair = Pair::quic(false);
    pair.connect();
    for _ in 0..20 {
        pair.step().unwrap();
    }
    let acks = pair.a.health().quic.unwrap().acks_received;
    for id in [1, 2] {
        pair.a
            .try_submit([id; 4], b"no outer feedback", Time(u64::MAX))
            .unwrap();
        pair.step().unwrap();
    }
    assert_eq!(pair.received.len(), 2);
    assert!(pair.a.health().bytes_in_flight > 0);
    assert!(
        pair.a.health().quic.unwrap().acks_received > acks,
        "ordinary QUIC ACKs must not substitute for outer feedback"
    );
    pair.now.0 += 2_000_000_000;
    assert_eq!(
        pair.a.poll(
            pair.now,
            256,
            |_| panic!("send after feedback silence"),
            |_| {}
        ),
        Err(Error::FeedbackUnavailable)
    );
    assert_eq!(pair.a.health().closed, Some(Error::FeedbackUnavailable));
    assert_eq!(pair.a.health().bytes_in_flight, 0);
    assert_eq!(pair.a.admission(), Err(Error::Closed));
    assert_eq!(pair.a.next_deadline(), None);
}

#[test]
fn quic_requires_the_intended_client_and_rejects_strict_queue_cancellation() {
    let mut pair = Pair::quic_profile(false, true);
    pair.start();
    let mut rejected = false;
    for _ in 0..5000 {
        if pair.step() == Err(Error::Security) {
            rejected = true;
            break;
        }
    }
    assert!(rejected);
    assert!(!pair.b.health().ready);
    assert!(pair.received.is_empty());
    let credential = QuicCredential::generate().unwrap();
    let mut tuning = QuicTuning::default();
    assert!(!tuning.queue_cancellation_supported());
    tuning.require_queue_cancellation = true;
    assert!(matches!(
        Transport::new(QuicPeer {
            endpoint: "127.0.0.1:6000".parse().unwrap(),
            expected_peer: credential.fingerprint(),
            credential,
            role: PeerRole::Initiator,
            class: NetworkClass::Lan,
            tuning,
        }),
        Err(Error::UnsupportedProfile)
    ));
}

#[test]
fn matched_low_delay_workload_keeps_backend_staging_shallow() {
    for quic in [false, true] {
        let mut pair = Pair::with_backend(false, quic);
        pair.connect();
        for _ in 0..20 {
            pair.step().unwrap();
        }
        let payload = [42; 1000];
        let mut admitted = 0;
        // Identical four-Mbit/s offered payload trace, one-millisecond host
        // turns, no loss or bottleneck. This qualifies this envelope only.
        for tick in 0..1000 {
            if tick % 2 == 0 {
                let result = pair
                    .a
                    .try_submit([1; 4], &payload, Time(pair.now.0 + 10_000_000));
                match result {
                    Ok(()) => admitted += 1,
                    Err(Error::Blocked) => {}
                    other => panic!("admission: {other:?}"),
                }
            }
            pair.step().unwrap();
            assert!(pair.a.health().staging.datagrams <= 1);
            assert!(
                pair.a
                    .health()
                    .staging
                    .oldest_age
                    .is_none_or(|age| age <= Duration::from_millis(1))
            );
        }
        for _ in 0..100 {
            pair.step().unwrap();
        }
        assert!(admitted >= 100);
        assert_eq!(pair.received.len(), admitted);
        assert_eq!(pair.a.health().expired, 0);
        assert_eq!(pair.a.health().bytes_in_flight, 0);
        assert_eq!(pair.a.health().staging.datagrams, 0);
        if let Some(stats) = pair.a.health().quic {
            assert_eq!(stats.inner_stall_polls, 0);
            assert_eq!(stats.queued_payload_bytes, 0);
            assert_eq!(stats.window_bytes, QuicTuning::default().inner_window_bytes);
        }
    }
}

#[test]
fn binding_inner_pacing_suppresses_non_actionable_outer_wakeups() {
    let tuning = QuicTuning {
        inner_pacing_bytes_per_second: 1200,
        ..QuicTuning::default()
    };
    let mut pair = Pair::quic_config(false, false, tuning);
    pair.connect();
    pair.a.set_demand(true);
    for _ in 0..100 {
        match pair.a.try_submit([1; 4], &[7; 1000], Time(u64::MAX)) {
            Ok(()) | Err(Error::Blocked) => {}
            other => panic!("admission: {other:?}"),
        }
        pair.poll(64).unwrap();
        if pair.a.health().quic.unwrap().inner_stall_polls > 0 {
            assert_eq!(pair.a.health().staging.datagrams, 1);
            assert!(!pair.a.needs_progress());
            assert!(
                pair.a.next_deadline().is_some_and(|at| at > pair.now),
                "outer readiness must not busy-spin while Noq cannot release the staged frame"
            );
            return;
        }
        pair.step().unwrap();
    }
    panic!("deliberately low inner pacing did not bind");
}

#[test]
fn quic_idle_timeout_is_not_reported_as_authentication_failure() {
    let mut pair = Pair::quic(false);
    pair.connect();
    for _ in 0..20 {
        pair.step().unwrap();
    }
    pair.now.0 += 61_000_000_000;
    assert_eq!(
        pair.a.poll(pair.now, 64, |_| SendOutcome::Rejected, |_| {}),
        Err(Error::Timeout)
    );
    assert_eq!(pair.a.health().closed, Some(Error::Timeout));
    assert_eq!(pair.a.health().bytes_in_flight, 0);
}

#[test]
fn zero_budget_clock_advance_withholds_input_until_the_protocol_tick_is_drained() {
    for quic in [false, true] {
        let mut pair = Pair::with_backend(false, quic);
        pair.connect();
        for _ in 0..20 {
            pair.step().unwrap();
        }
        assert!(pair.a.input_ready());
        pair.now.0 += 1_000_000;
        assert_eq!(
            pair.a
                .poll(pair.now, 0, |_| SendOutcome::Rejected, |_| {})
                .unwrap(),
            0
        );
        assert!(!pair.a.input_ready());
        assert!(pair.a.needs_progress());
        assert_eq!(pair.a.next_deadline(), Some(pair.now));
        assert_eq!(pair.a.admission(), Err(Error::Blocked));
        assert_eq!(
            pair.a.receive(
                pair.ba,
                &[],
                ReceiveTime {
                    monotonic: pair.now,
                    wall: None
                }
            ),
            Err(Error::Busy)
        );
        pair.a
            .poll(
                pair.now,
                64,
                |datagram| SendOutcome::Accepted {
                    bytes: datagram.payload.len(),
                },
                |_| {},
            )
            .unwrap();
        assert!(pair.a.input_ready());
        assert!(!pair.a.needs_progress());
    }
}
