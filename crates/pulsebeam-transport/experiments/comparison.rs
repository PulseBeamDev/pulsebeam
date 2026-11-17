//! The six adaptive corpus arms. Fixed/bare fixtures have no ordinary dispatch.
use super::*;
use serde_json::{Value, json};

pub(super) const HORIZON_MS: u64 = 6000;
pub(super) const CUTOFF_MS: u64 = 7500;
#[derive(Clone, Copy)]
enum Arm {
    Native(NativeController),
    Udp,
    Quic(QuicCongestion),
}
const ARMS: [Arm; 6] = [
    Arm::Native(NativeController::ScreamV2),
    Arm::Native(NativeController::Gcc),
    Arm::Udp,
    Arm::Quic(QuicCongestion::OuterControlled),
    Arm::Quic(QuicCongestion::Cubic),
    Arm::Quic(QuicCongestion::Bbr3),
];
impl Arm {
    fn label(self) -> &'static str {
        match self {
            Self::Native(controller) => controller.label(),
            Self::Udp => "udp_scream",
            Self::Quic(cc) => match cc {
                QuicCongestion::OuterControlled => "quic_outer",
                QuicCongestion::Cubic => "quic_cubic",
                QuicCongestion::Bbr3 => "quic_bbr3",
            },
        }
    }
}

pub(super) fn summary(
    corpus: &corpus::Corpus,
    receiver: &receiver::Receiver,
    cutoff_ns: u64,
    complete: bool,
) -> Value {
    let report = (cutoff_ns >= HORIZON_MS * 1_000_000)
        .then(|| receiver.observations.finish(corpus, cutoff_ns, complete));
    let decode = report.as_ref().map_or_else(
        || receiver.observations.coverage(cutoff_ns),
        |report| report.timeline.decode.clone(),
    );
    let timeline = report.as_ref().map(|report| &report.timeline);
    let count = |status| decode.iter().filter(|value| **value == status).count();
    json!({"qualified": timeline.is_some_and(|timeline| timeline.qualified),
        "unavailable_timeline_reason": if timeline.is_none() { Some("trace ended before offered horizon") } else { None },
        "observed_cutoff_ns": cutoff_ns, "offered_captures": corpus.len(), "timely": count(presentation::DecodeStatus::Timely),
        "late": count(presentation::DecodeStatus::Late), "missing": count(presentation::DecodeStatus::Missing),
        "unknown": count(presentation::DecodeStatus::Unknown),
        "startup_blank_ns": timeline.map(|t| t.startup_blank_ns), "blank_ns": timeline.map(|t| t.blank_ns),
        "presented_ns": timeline.map(|t| t.presented_ns), "held_ns": timeline.map(|t| t.held_ns),
        "freeze_ns": timeline.map(|t| t.freeze_ns), "longest_freeze_ns": timeline.map(|t| t.longest_freeze_ns),
        "freezes": timeline.map(|t| t.freezes), "displayed_age_ns_times_duration_ns": timeline.map(|t| t.age_ns_times_duration_ns.to_string()),
        "decoded_representation_squared_error": report.as_ref().map(|r| r.decoded_representation_squared_error),
        "decoded_representation_samples": report.as_ref().map(|r| r.decoded_representation_samples),
        "presented_squared_error_times_ns": report.as_ref().map(|r| r.presented_squared_error_times_ns.to_string()),
        "presented_samples_times_ns": report.as_ref().map(|r| r.presented_samples_times_ns.to_string()),
        "decoded_representation_psnr_i420_db": report.as_ref().map(|r| psnr(u128::from(r.decoded_representation_squared_error), u128::from(r.decoded_representation_samples))),
        "presented_psnr_i420_db": report.as_ref().map(|r| psnr(r.presented_squared_error_times_ns, r.presented_samples_times_ns)),
        "invalid_identity": receiver.observations.invalid_identity,
        "invalid_timestamp": receiver.observations.invalid_timestamp,
        "duplicates": receiver.observations.duplicates,
        "decoder_complete_units": receiver.complete, "dependency_discarded": receiver.dependency_discarded,
        "decoder_rejected": receiver.rejected, "trace": receiver.observations.trace(report.as_ref()),
    })
}
fn psnr(error: u128, samples: u128) -> Value {
    if samples == 0 {
        Value::Null
    } else if error == 0 {
        json!("infinite")
    } else {
        json!(10.0 * (255.0f64.powi(2) * samples as f64 / error as f64).log10())
    }
}
pub(super) fn wire<T>(link: &Link<T>) -> Value {
    json!({"attempted_bytes_by_direction": link.bytes, "scheduled_bytes_by_direction": link.accepted_bytes,
        "dropped_bytes_by_direction": link.dropped_bytes, "dropped_packets": link.dropped,
        "pending_packets": link.pending.len(), "peak_queue_ms": link.peak_queue_ms,
        "accounting": "UDP payload plus 28-byte IPv4/UDP overhead, forward/reverse; scheduled includes pending"})
}

#[derive(Default)]
struct Assembly {
    parts: BTreeMap<u16, Vec<u8>>,
    count: u16,
    timestamp: u32,
    key: bool,
}
#[derive(Default)]
struct Rung {
    pending: BTreeMap<u32, Assembly>,
    last: Option<u32>,
    reordered: u64,
    incomplete_retired: u64,
}
impl Rung {
    fn receive(&mut self, bytes: &[u8]) -> Option<(Vec<u8>, u32, bool, bool)> {
        assert!(bytes.len() >= 20 && &bytes[..4] == b"PBC1");
        let part = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
        let count = u16::from_be_bytes(bytes[8..10].try_into().unwrap());
        let sequence = u32::from_be_bytes(bytes[12..16].try_into().unwrap());
        let timestamp = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let key = bytes[5] == 1;
        assert!(part < count && count <= 1024);
        if self.last.is_some_and(|last| sequence <= last) {
            self.reordered += 1;
            return None;
        }
        let entry = self.pending.entry(sequence).or_insert_with(|| Assembly {
            count,
            timestamp,
            key,
            ..Default::default()
        });
        assert_eq!(
            (entry.count, entry.timestamp, entry.key),
            (count, timestamp, key)
        );
        if let Some(previous) = entry.parts.insert(part, bytes[20..].to_vec()) {
            assert_eq!(previous, bytes[20..]);
        }
        assert!(
            self.pending.len() <= 64,
            "bounded incomplete picture window"
        );
        if self.pending[&sequence].parts.len() != usize::from(count) {
            return None;
        }
        let picture = self.pending.remove(&sequence).unwrap();
        let missing = self.last.map_or(sequence != 0, |last| sequence != last + 1);
        self.last = Some(sequence);
        let obsolete: Vec<_> = self
            .pending
            .keys()
            .copied()
            .filter(|key| *key < sequence)
            .collect();
        for key in obsolete {
            self.pending.remove(&key);
            self.incomplete_retired += 1;
        }
        Some((
            picture.parts.into_values().flatten().collect(),
            timestamp,
            key,
            missing,
        ))
    }
}
fn fragments(unit: &corpus::Encoded, sequence: u32) -> Vec<Packet> {
    let count = u16::try_from(unit.bytes.len().div_ceil(CHUNK)).unwrap();
    assert!(count > 0 && count <= 1024);
    unit.bytes
        .chunks(CHUNK)
        .enumerate()
        .map(|(part, payload)| {
            let mut bytes = Vec::with_capacity(20 + payload.len());
            bytes.extend_from_slice(b"PBC1");
            bytes.push(u8::try_from(unit.layer).unwrap());
            bytes.push(u8::from(unit.key));
            bytes.extend_from_slice(&u16::try_from(part).unwrap().to_be_bytes());
            bytes.extend_from_slice(&count.to_be_bytes());
            bytes.extend_from_slice(&[0, 0]);
            bytes.extend_from_slice(&sequence.to_be_bytes());
            bytes.extend_from_slice(&unit.timestamp.to_be_bytes());
            bytes.extend_from_slice(payload);
            Packet {
                bytes,
                expires: Time(u64::MAX),
            }
        })
        .collect()
}

fn custom(corpus: &corpus::Corpus, scenario: Scenario, seed: u64, path: path::MediaPath) -> Value {
    let addresses = [
        "127.0.0.1:41001".parse().unwrap(),
        "127.0.0.1:41002".parse().unwrap(),
    ];
    let mut peers = path.sessions(Instant::now(), addresses);
    let mut inbound: [VecDeque<Vec<u8>>; 2] = std::array::from_fn(|_| VecDeque::new());
    let mut link = Link::new(scenario, seed, u64::MAX / 4);
    let mut epoch = 0;
    for at in 0..5000 {
        while let Some((to, bytes)) = link.pop(at) {
            inbound[to].push_back(bytes);
        }
        for who in 0..2 {
            let mut settled = false;
            for _ in 0..4096 {
                let mut output = Vec::new();
                peers[who].poll(at, &mut output, &mut Vec::new()).unwrap();
                for bytes in output {
                    let _ = link.push(at, who, bytes.len() + 28, bytes);
                }
                if !peers[who].input_ready() {
                    if peers[who].needs_progress() {
                        continue;
                    }
                    settled = true;
                    break;
                }
                let Some(bytes) = inbound[who].pop_front() else {
                    settled = true;
                    break;
                };
                peers[who].receive(addresses[1 - who], &bytes, at);
            }
            assert!(settled);
        }
        if at >= 200
            && peers.iter().all(Session::ready)
            && inbound.iter().all(VecDeque::is_empty)
            && link.pending.is_empty()
        {
            epoch = at + 1;
            break;
        }
    }
    assert!(epoch > 0);
    link.begin_measurement(epoch, HORIZON_MS, seed);
    let mut source = corpus::Source::new();
    let mut receiver = receiver::Receiver::new(corpus, measurement::CodecClock::Direct90k);
    let mut queue = media_queue::PictureQueue::default();
    let mut rungs: [Rung; 3] = std::array::from_fn(|_| Rung::default());
    let mut sequence = [0u32; 3];
    let mut backpressured = [0u64; 3];
    let mut request = [false; 3];
    let mut last_request: [Option<u64>; 3] = [None; 3];
    let mut produced = Vec::new();
    let mut next = 0;
    let mut failure = None;
    let mut observed_ms = 0;
    for elapsed in 0..CUTOFF_MS {
        observed_ms = elapsed + 1;
        let at = epoch + elapsed;
        while let Some((to, bytes)) = link.pop(elapsed) {
            inbound[to].push_back(bytes);
        }
        if next < corpus.len() && corpus.offered(next).at_ns.div_ceil(1_000_000) <= elapsed {
            if !queue.is_empty() {
                for value in &mut backpressured {
                    *value += 1;
                }
            } else {
                let budget = peers[0].encoder_target_bps().unwrap().min(2_200_000);
                for unit in source.encode(&corpus.frame(next), budget, request) {
                    let layer = unit.layer;
                    let packets = fragments(&unit, sequence[layer]);
                    produced.push(json!({"capture": unit.id.capture, "epoch": unit.id.epoch, "layer": layer,
                        "encoded_sequence": sequence[layer], "timestamp": unit.timestamp, "key": unit.key,
                        "encoded_bytes": unit.bytes.len(), "fragments": packets.len(), "provenance": "live_codec_callback"}));
                    sequence[layer] += 1;
                    request[layer] = false;
                    queue.push_packets(next * 3 + layer, unit.id.epoch, packets);
                }
            }
            next += 1;
        }
        if !scenario.turn(elapsed) {
            continue;
        }
        peers[0].demand(!queue.is_empty());
        for who in 0..2 {
            let mut settled = false;
            for _ in 0..4096 {
                let mut output = Vec::new();
                let mut received = Vec::new();
                if let Err(error) = peers[who].poll(at, &mut output, &mut received) {
                    failure = Some(format!("{error:?}"));
                    break;
                }
                for bytes in output {
                    let _ = link.push(elapsed, who, bytes.len() + 28, bytes);
                }
                for bytes in received {
                    if who == 0 {
                        assert!(bytes.len() == 5 && &bytes[..4] == b"PBK1");
                        request[usize::from(bytes[4])] = true;
                    } else {
                        let layer = usize::from(bytes[4]);
                        assert!(layer < 3);
                        if let Some((data, timestamp, key, missing)) = rungs[layer].receive(&bytes)
                        {
                            receiver.unit(
                                layer,
                                &data,
                                timestamp,
                                key,
                                missing,
                                (elapsed + 1) * 1_000_000,
                            );
                        }
                    }
                }
                if !peers[who].input_ready() {
                    if peers[who].needs_progress() {
                        continue;
                    }
                    settled = true;
                    break;
                }
                if let Some(bytes) = inbound[who].pop_front() {
                    peers[who].receive(addresses[1 - who], &bytes, at);
                    continue;
                }
                if who == 1 {
                    let need = (0..3).find(|layer| {
                        receiver.waiting[*layer]
                            && receiver.complete[*layer] > 0
                            && last_request[*layer]
                                .is_none_or(|last| elapsed.saturating_sub(last) >= 200)
                    });
                    if let Some(layer) = need {
                        let mut bytes = b"PBK1".to_vec();
                        bytes.push(u8::try_from(layer).unwrap());
                        match peers[who].submit(
                            &Packet {
                                bytes,
                                expires: Time(u64::MAX),
                            },
                            true,
                        ) {
                            Ok(()) => {
                                last_request[layer] = Some(elapsed);
                                continue;
                            }
                            Err(Error::Blocked | Error::NotReady) => {}
                            Err(error) => {
                                failure = Some(format!("{error:?}"));
                                break;
                            }
                        }
                    }
                } else if let Some(packet) = queue.front() {
                    match peers[who].submit(packet, true) {
                        Ok(()) => {
                            queue.accepted();
                            continue;
                        }
                        Err(Error::Blocked | Error::NotReady) => {}
                        Err(error) => {
                            failure = Some(format!("{error:?}"));
                            break;
                        }
                    }
                }
                settled = true;
                break;
            }
            if failure.is_some() {
                break;
            }
            assert!(settled, "corpus transport continuation exhausted");
        }
        receiver.collect((elapsed + 1) * 1_000_000);
        if failure.is_some() {
            break;
        }
    }
    let unobserved = u64::try_from(corpus.len() - next).unwrap();
    for layer in 0..3 {
        assert_eq!(
            u64::from(sequence[layer])
                + source.omissions[layer]
                + source.suspended[layer]
                + backpressured[layer]
                + unobserved,
            u64::try_from(corpus.len()).unwrap()
        );
    }
    let retired = queue.retire();
    let by_representation: BTreeMap<_, _> = retired
        .iter()
        .map(|picture| (picture.capture, picture))
        .collect();
    for unit in &mut produced {
        let key = usize::try_from(unit["capture"].as_u64().unwrap()).unwrap() * 3
            + usize::try_from(unit["layer"].as_u64().unwrap()).unwrap();
        let (accepted, queued) = by_representation.get(&key).map_or_else(
            || {
                (
                    usize::try_from(unit["fragments"].as_u64().unwrap()).unwrap(),
                    0,
                )
            },
            |picture| (picture.admitted_fragments, picture.queued_fragments),
        );
        unit["caller_accepted_fragments"] = json!(accepted);
        unit["caller_queued_fragments_at_retirement"] = json!(queued);
        unit["caller_disposition"] = json!(if queued == 0 {
            "fully_accepted_by_transport"
        } else {
            "retired_at_cutoff"
        });
    }
    let terminal: Vec<_> = retired.iter().map(|picture| json!({"capture": picture.capture / 3, "layer": picture.capture % 3,
        "epoch": picture.epoch, "accepted_prefix_fragments": picture.admitted_fragments, "queued_tail_fragments": picture.queued_fragments,
        "disposition": "retired_at_cutoff_not_expired"})).collect();
    json!({"summary": summary(corpus, &receiver, observed_ms*1_000_000, failure.is_none()), "wire": wire(&link),
        "source": {"produced_by_layer": sequence, "paused_by_layer": source.suspended, "encoder_omissions_by_layer": source.omissions,
            "backpressured_by_layer": backpressured, "unobserved_by_layer": ([unobserved; 3]), "produced": produced},
        "lifecycle": {"caller_admitted_pictures": queue.admitted_pictures, "caller_terminal": terminal,
            "incomplete_at_receiver_by_layer": rungs.each_ref().map(|rung| rung.pending.len()),
            "incomplete_retired_by_layer": rungs.each_ref().map(|rung| rung.incomplete_retired),
            "reordered_fragments_by_layer": rungs.each_ref().map(|rung| rung.reordered),
            "transport_disposition": null, "transport_disposition_reason": "accepted send is not per-representation terminal delivery evidence"},
        "failure": failure, "inner_cc": path.inner_cc(), "outer_cc": "rust_screamv2"})
}

pub(super) fn run() {
    let mut seeds = 1u64;
    let mut scenario_filter = String::new();
    let mut arm_filter = String::new();
    let mut source = 0u64;
    for argument in std::env::args().skip(1) {
        if let Some(value) = argument.strip_prefix("--seeds=") {
            seeds = value.parse().unwrap();
        } else if let Some(value) = argument.strip_prefix("--scenario=") {
            scenario_filter = value.into();
        } else if let Some(value) = argument.strip_prefix("--arm=") {
            arm_filter = value.into();
        } else if let Some(value) = argument.strip_prefix("--source=") {
            source = value.parse().unwrap();
        } else {
            panic!("unknown corpus comparison option {argument}");
        }
    }
    assert!((1..=3).contains(&seeds) && source <= 1);
    assert!(arm_filter.is_empty() || ARMS.iter().any(|arm| arm.label() == arm_filter));
    let scenarios: Vec<_> = SCENARIOS
        .iter()
        .copied()
        .filter(|scenario| {
            ["clean", "capacity_step", "loss_1pct", "rtt_160ms"].contains(&scenario.name)
        })
        .collect();
    assert!(
        scenario_filter.is_empty()
            || scenarios
                .iter()
                .any(|scenario| scenario.name == scenario_filter)
    );
    let corpus = corpus::Corpus::new(
        if source == 0 {
            pulsebeam_testdata::QualityVideoSource::Zero
        } else {
            pulsebeam_testdata::QualityVideoSource::One
        },
        0,
    );
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{}", json!({"kind":"metadata", "schema":"rtc-uplink-v1", "measurement":"offered-timeline-v1",
        "source_policy": corpus::SOURCE_POLICY, "allocation_policy": corpus::ALLOCATION_POLICY,
        "reference_policy": pulsebeam_testdata::QUALITY_COMMON_VIEW_POLICY, "corpus_manifest": pulsebeam_testdata::QUALITY_CORPUS_MANIFEST,
        "source": source, "lifetime": 0, "fps": corpus::FPS, "rung_caps_bps": corpus::CAPS,
        "initial_aggregate_bps": 300_000, "horizon_ms": HORIZON_MS, "cutoff_ms": CUTOFF_MS,
        "sample_weighting": "equal I420 samples (Y:U:V=4:1:1)", "decoder_observation": "end of 1ms host turn, no modeled codec CPU latency",
        "repair_interval_ms": 200, "sampling_ns": 10_000_000, "deadline_ns": 180_000_000, "freeze_threshold_ns": 200_000_000,
        "bigint_encoding": "exact decimal strings for duration-weighted sums", "browser_ecn_l4s_qualified": false})).unwrap();
    for scenario in scenarios
        .into_iter()
        .filter(|scenario| scenario_filter.is_empty() || scenario.name == scenario_filter)
    {
        for seed in 1..=seeds {
            for arm in ARMS
                .into_iter()
                .filter(|arm| arm_filter.is_empty() || arm.label() == arm_filter)
            {
                let mut result = match arm {
                    Arm::Native(controller) => uplink::run(&corpus, scenario, seed, controller),
                    Arm::Udp => custom(&corpus, scenario, seed, path::MediaPath::Udp),
                    Arm::Quic(cc) => custom(
                        &corpus,
                        scenario,
                        seed,
                        path::MediaPath::Quic { cc, overlay: true },
                    ),
                };
                result["kind"] = json!("run");
                result["schema"] = json!("rtc-uplink-v1");
                result["arm"] = json!(arm.label());
                result["scenario"] = json!(scenario.name);
                result["seed"] = json!(seed);
                result["source_id"] = json!(source);
                writeln!(out, "{result}").unwrap();
                out.flush().unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corpus_fragments_preserve_encoded_dependencies_not_capture_adjacency() {
        let mut unit = corpus::Encoded {
            id: presentation::CaptureId {
                source: 0,
                lifetime: 0,
                epoch: 0,
                capture: 0,
            },
            layer: 0,
            timestamp: 0,
            bytes: vec![73; 2501],
            key: true,
        };
        let packets = fragments(&unit, 0);
        let mut rung = Rung::default();
        assert!(rung.receive(&packets[2].bytes).is_none());
        assert!(rung.receive(&packets[2].bytes).is_none());
        assert!(rung.receive(&packets[0].bytes).is_none());
        let (actual, timestamp, key, missing) = rung.receive(&packets[1].bytes).unwrap();
        assert_eq!(actual, unit.bytes);
        assert_eq!(timestamp, 0);
        assert!(key && !missing);
        assert!(rung.receive(&packets[0].bytes).is_none());
        unit.id.capture = 9;
        unit.timestamp = 27_000;
        unit.key = false;
        let next = fragments(&unit, 1);
        let mut completed = None;
        for packet in &next {
            completed = rung.receive(&packet.bytes);
        }
        assert!(
            !completed.as_ref().unwrap().3,
            "unencoded capture gaps are not missing encoded references"
        );
        let lost = fragments(&unit, 2);
        rung.receive(&lost[0].bytes);
        let later = fragments(&unit, 3);
        for packet in &later {
            completed = rung.receive(&packet.bytes);
        }
        assert!(completed.as_ref().unwrap().3);
        assert_eq!(rung.incomplete_retired, 1);
        assert!(rung.pending.is_empty());
    }
    #[test]
    fn aggregate_quality_preserves_empty_and_exact_samples() {
        assert!(psnr(0, 0).is_null());
        assert_eq!(psnr(0, 1), json!("infinite"));
        assert_eq!(psnr(65_025, 1), json!(0.0));
    }
}
