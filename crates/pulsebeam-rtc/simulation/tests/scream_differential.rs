use std::time::Duration;

use pulsebeam_cc::{Config, Controller, EcnMark, EcnValidation, FeedbackSample, Observation};
use pulsebeam_webrtc_sys as sys;
use sys::{DataRate, DataSize, Timestamp};

// The SDK logger ABI requires shared Send+Sync callbacks. Keep that test-only
// ownership and synchronization inside the native observation boundary.
#[allow(clippy::disallowed_types)]
mod native_log {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Events(Mutex<Vec<(sys::RtcEventKind, sys::RtcEventData)>>);
    impl sys::RtcEventLogger for Events {
        fn start(&self, _: sys::EventLogOutput, _: sys::EventLogOutputPeriod) -> bool {
            false
        }
        fn stop(&self) {}
        fn log(&self, event: sys::RtcEvent) {
            self.0.lock().unwrap().push((event.kind(), event.data()));
        }
    }

    #[derive(Default)]
    pub(super) struct Log(Arc<Events>);
    impl Log {
        pub(super) fn environment(&self, trials: &str) -> sys::Environment {
            let clock = sys::ManualClock::new(Duration::ZERO).unwrap();
            let queues = sys::TaskQueueFactory::cooperative(&clock).unwrap();
            sys::Environment::builder()
                .clock(&clock)
                .task_queue_factory(&queues)
                .field_trials(sys::FieldTrials::parse(trials).unwrap())
                .event_log(self.0.clone())
                .build()
                .unwrap()
        }

        pub(super) fn has_scream(&self) -> bool {
            self.0
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|(kind, _)| *kind == sys::RtcEventKind::BweUpdateScream)
        }

        pub(super) fn has_ce(&self) -> bool {
            self.0.0.lock().unwrap().iter().any(|(_, data)| {
                matches!(data,
                sys::RtcEventData::BweUpdateScream { l4s_marked_permille, .. }
                    if *l4s_marked_permille > 0)
            })
        }
    }
}

fn ts(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms).unwrap()
}
fn duration(ms: i64) -> Duration {
    Duration::from_millis(ms.try_into().unwrap())
}
fn observe(epoch: u64, feedback: &[FeedbackSample], fresh: bool, hold: i64) -> Observation<'_> {
    Observation {
        path_epoch: Some(epoch),
        path_available: true,
        feedback,
        feedback_hold: duration(hold),
        fresh_network_feedback: fresh,
        bytes_in_flight: 0,
        paced_queue_bytes: 0,
        desired_media_rate: 1_000_000,
        ecn: EcnValidation {
            l4s: true,
            ..Default::default()
        },
    }
}

#[test]
fn actual_sdk_and_rust_replay() {
    use sys::NetworkControllerReplayEvent as Event;

    let log = native_log::Log::default();
    let factory = sys::NetworkControllerFactory::goog_cc(sys::GoogCcFactoryConfig::new(true));
    let constraints = sys::TargetRateConstraints {
        at_time: ts(0),
        min_data_rate: Some(DataRate::BitsPerSecond(0)),
        max_data_rate: None,
        starting_rate: Some(DataRate::BitsPerSecond(300_000)),
    };
    let config = sys::NetworkControllerReplayConfig {
        constraints: constraints.clone(),
        stream_based_config: sys::StreamsConfig {
            at_time: ts(0),
            max_total_allocated_bitrate: Some(DataRate::BitsPerSecond(1_000_000)),
            ..Default::default()
        },
        default_pacing_time_window: sys::TimeDelta::from_millis(40).unwrap(),
    };
    let mut events = vec![
        Event::NetworkAvailability(sys::NetworkAvailability {
            at_time: ts(0),
            network_available: true,
        }),
        Event::NetworkRouteChange(sys::NetworkRouteChange {
            at_time: ts(1),
            constraints: sys::TargetRateConstraints {
                at_time: ts(1),
                ..constraints.clone()
            },
            restart_bwe: true,
        }),
    ];
    // One new receipt per feedback, plus at most one first-time loss. Never
    // replay recovered or duplicate records: host ACK-edge normalization differs.
    // (send, arrival, feedback, CE, optional earlier lost send)
    let trace = [
        (10, 30, 40, false, None),
        (60, 90, 100, false, None),
        (110, 150, 160, true, None),
        (170, 195, 205, false, None),
        (220, 260, 270, true, None),
        (500, 520, 530, false, None),
        (600, 620, 630, false, Some(570)),
        (700, 720, 730, false, Some(670)),
        (800, 820, 830, false, Some(770)),
        (900, 920, 930, false, Some(870)),
        (1000, 1020, 1030, false, None),
        (1100, 1120, 1130, false, None),
        (1200, 1220, 1230, false, None),
        (1300, 1320, 1330, false, None),
        (1410, 1430, 1440, false, None),
    ];
    let mut sequence = 0;
    for (send, arrival, feedback, ce, lost_send) in trace {
        if send == 500 {
            events.push(Event::ProcessInterval(sys::ProcessInterval {
                at_time: ts(400),
                ..Default::default()
            }));
        }
        if send == 1410 {
            events.push(Event::NetworkRouteChange(sys::NetworkRouteChange {
                at_time: ts(1400),
                constraints: sys::TargetRateConstraints {
                    at_time: ts(1400),
                    ..constraints.clone()
                },
                // A host epoch change retains the target, matching native's
                // no-restart route change, not its safe-restart policy.
                restart_bwe: false,
            }));
        }
        let mut packet = |at, flight| {
            sequence += 1;
            sys::SentPacket {
                send_time: ts(at),
                size: DataSize::Bytes(1200),
                sequence_number: sequence,
                data_in_flight: DataSize::Bytes(flight),
                ..Default::default()
            }
        };
        let lost = lost_send.map(|at| packet(at, 1200));
        if let Some(packet) = &lost {
            events.push(Event::SentPacket(packet.clone()));
        }
        let receipt = packet(send, if lost.is_some() { 2400 } else { 1200 });
        events.push(Event::SentPacket(receipt.clone()));
        let mut results = Vec::new();
        if let Some(packet) = lost {
            results.push(sys::PacketResult {
                sent_packet: packet,
                reported_lost_for_the_first_time: true,
                sent_with_ect1: true,
                ..Default::default()
            });
        }
        results.push(sys::PacketResult {
            sent_packet: receipt,
            receive_time: ts(arrival),
            arrival_time_offset: Some(sys::TimeDelta::from_millis(feedback - arrival).unwrap()),
            ecn: if ce {
                sys::EcnMarking::CongestionExperienced
            } else {
                sys::EcnMarking::Ect1
            },
            sent_with_ect1: true,
            ..Default::default()
        });
        events.push(Event::TransportPacketsFeedback(
            sys::TransportPacketsFeedback {
                feedback_time: ts(feedback),
                data_in_flight: DataSize::Bytes(0),
                transport_supports_ecn: true,
                packet_feedbacks: results,
                ..Default::default()
            },
        ));
    }
    let environment = log.environment("WebRTC-Bwe-ScreamV2/mode:always/");
    let native =
        sys::network_controller_replay_for_test(&environment, &factory, config.clone(), &events)
            .unwrap();
    assert!(log.has_scream(), "SCReAM was not selected");
    let gcc_log = native_log::Log::default();
    let gcc_env = gcc_log.environment("WebRTC-Bwe-ScreamV2/mode:disabled/");
    sys::network_controller_replay_for_test(&gcc_env, &factory, config, &events).unwrap();
    assert!(!gcc_log.has_scream(), "GCC control selected SCReAM");

    let mut rust = Controller::new(Config::default()).unwrap();
    let mut envelope = rust
        .update(Duration::ZERO, observe(1, &[], false, 10))
        .unwrap();
    let mut epoch = 1;
    let (mut target, mut window, mut pacing) = (None, None, None);
    let mut references = Vec::new();
    let mut rates = Vec::new();
    let mut reasons = Vec::new();
    let mut queues = Vec::new();
    let mut restarts = 0;
    assert_eq!(native.len(), events.len());
    for (i, (event, update)) in events.iter().zip(&native).enumerate() {
        if let Some(value) = &update.target_rate {
            target = Some(value.target_rate);
        }
        if let Some(value) = update.congestion_window {
            window = Some(value);
        }
        if let Some(value) = &update.pacer_config {
            pacing = Some(value.clone());
        }
        let compare = match event {
            Event::NetworkAvailability(_) => true,
            Event::NetworkRouteChange(route) => {
                if route.at_time == ts(1400) {
                    // Excite a real reset of non-initial rate/window evidence.
                    assert!(envelope.target_media_payload_rate > 300_000);
                    assert_ne!(envelope.reference_window, 1_000);
                    restarts += 1;
                }
                epoch += 1;
                envelope = rust
                    .update(
                        duration(route.at_time.millis().unwrap()),
                        observe(epoch, &[], false, 10),
                    )
                    .unwrap();
                true
            }
            Event::SentPacket(packet) => {
                let now = duration(packet.send_time.millis().unwrap());
                let DataSize::Bytes(flight) = packet.data_in_flight else {
                    panic!("finite flight")
                };
                assert_eq!(
                    rust.ready(now, (flight - 1200) as u64, 1200),
                    pulsebeam_cc::Readiness::Ready,
                    "send event {i}"
                );
                rust.commit_send(now, 1200, flight as u64, false);
                false
            }
            Event::ProcessInterval(process) => {
                envelope = rust
                    .update(
                        duration(process.at_time.millis().unwrap()),
                        observe(epoch, &[], false, 10),
                    )
                    .unwrap();
                true
            }
            Event::TransportPacketsFeedback(feedback) => {
                let now = feedback.feedback_time.millis().unwrap();
                let samples: Vec<_> = feedback
                    .packet_feedbacks
                    .iter()
                    .map(|result| {
                        let lost = result.reported_lost_for_the_first_time;
                        FeedbackSample {
                            sent_at: duration(result.sent_packet.send_time.millis().unwrap()),
                            received_at: duration(now),
                            transport_bytes: 1200,
                            received: !lost,
                            newly_acked: true,
                            lost,
                            recovered: false,
                            receiver_arrival_micros: if lost {
                                None
                            } else {
                                Some(result.receive_time.millis().unwrap() * 1000)
                            },
                            ecn: if lost {
                                None
                            } else {
                                Some(if result.ecn == sys::EcnMarking::CongestionExperienced {
                                    EcnMark::Ce
                                } else {
                                    EcnMark::Ect1
                                })
                            },
                        }
                    })
                    .collect();
                let hold = feedback
                    .packet_feedbacks
                    .iter()
                    .find_map(|r| r.arrival_time_offset.map(|v| v.millis().unwrap()))
                    .unwrap();
                envelope = rust
                    .update(duration(now), observe(epoch, &samples, true, hold))
                    .unwrap();
                references.push(envelope.reference_window);
                rates.push(envelope.target_media_payload_rate);
                reasons.push(envelope.reason);
                queues.push(envelope.queue_delay);
                true
            }
            _ => panic!("unexpected replay event"),
        };
        if compare {
            assert_eq!(
                target,
                Some(DataRate::BitsPerSecond(
                    envelope.target_media_payload_rate as i64
                )),
                "target at event {i}: {event:?}, Rust {envelope:?}"
            );
            assert_eq!(
                window,
                Some(DataSize::Bytes(envelope.max_rtp_bytes_in_flight as i64)),
                "window at event {i}: {event:?}"
            );
            let native_pacing = pacing.as_ref().expect("native pacing update");
            let expected = sys::PacerConfig::create(
                ts(0),
                DataRate::BitsPerSecond(envelope.nominal_pacing_transport_rate as i64),
                DataRate::BitsPerSecond(envelope.padding_transport_rate as i64),
                sys::TimeDelta::from_microseconds(
                    envelope.pacing_window.as_micros().try_into().unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                native_pacing.time_window, expected.time_window,
                "pacing window at event {i}"
            );
            assert_eq!(
                native_pacing.data_window, expected.data_window,
                "pacing data at event {i}: {event:?}"
            );
            assert_eq!(
                native_pacing.pad_window, expected.pad_window,
                "padding at event {i}"
            );
        }
    }
    assert_eq!(restarts, 1);
    assert_ne!(envelope.reference_window, 1_000);
    assert_eq!(envelope.smoothed_rtt, duration(20));
    assert_eq!(envelope.queue_delay, Duration::ZERO);
    assert!(
        references.windows(2).any(|w| w[0] != w[1]),
        "reference window did not change: {references:?}"
    );
    assert!(
        rates.windows(2).any(|w| w[0] != w[1]),
        "target rate did not change: {rates:?}"
    );
    assert!(
        queues.windows(2).any(|w| w[0] < w[1]) && queues.windows(2).any(|w| w[0] > w[1]),
        "queue trend not exercised: {queues:?}"
    );
    assert!(
        reasons
            .iter()
            .filter(|&&r| r == pulsebeam_cc::ControllerReason::L4s)
            .count()
            >= 2,
        "CE reactions: {reasons:?}"
    );
    assert!(
        reasons.contains(&pulsebeam_cc::ControllerReason::Loss),
        "loss reaction: {reasons:?}"
    );
    assert!(log.has_ce(), "native CE signal absent");
}
