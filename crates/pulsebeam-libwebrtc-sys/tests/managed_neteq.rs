//! Actual native PCMU packets through stock NetEq using the selected runtime clock.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::{collections::BTreeMap, fmt::Debug, time::Duration};

fn serialize_debug<T: Debug, S: serde::Serializer>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("{value:?}"))
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct State {
    #[serde(serialize_with = "serialize_debug")]
    network: NetEqNetworkStatistics,
    #[serde(serialize_with = "serialize_debug")]
    lifetime: NetEqLifetimeStatistics,
    #[serde(serialize_with = "serialize_debug")]
    operations: NetEqOperationsAndState,
    #[serde(serialize_with = "serialize_debug")]
    decoder: Option<NetEqDecoderFormat>,
    playout_timestamp: Option<u32>,
    target_delay_ms: i32,
    filtered_current_delay_ms: i32,
    sync_buffer_size_ms: i32,
    last_output_sample_rate_hz: i32,
}
impl State {
    fn take(engine: &mut NetEq) -> Self {
        Self {
            network: engine.current_network_statistics(),
            lifetime: engine.lifetime_statistics(),
            operations: engine.operations_and_state(),
            decoder: engine.current_decoder_format(),
            playout_timestamp: engine.playout_timestamp(),
            target_delay_ms: engine.target_delay_ms(),
            filtered_current_delay_ms: engine.filtered_current_delay_ms(),
            sync_buffer_size_ms: engine.sync_buffer_size_ms(),
            last_output_sample_rate_hz: engine.last_output_sample_rate_hz(),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct CodecMapChange {
    after_insertion: bool,
    #[serde(serialize_with = "serialize_debug")]
    codecs: BTreeMap<i32, SdpAudioFormat>,
    #[serde(serialize_with = "serialize_debug")]
    result: Result<(), NetEqError>,
    before: State,
    after: State,
}
impl CodecMapChange {
    fn apply(
        engine: &mut NetEq,
        after_insertion: bool,
        codecs: BTreeMap<i32, SdpAudioFormat>,
    ) -> Self {
        let before = State::take(engine);
        let result = engine.set_codecs(&codecs);
        assert_eq!(result, Ok(()));
        let after = State::take(engine);
        Self {
            after_insertion,
            codecs,
            result,
            before,
            after,
        }
    }
}

#[derive(Debug, PartialEq, Serialize)]
struct Insertion {
    #[serde(serialize_with = "serialize_debug")]
    header: RtpHeaderValues,
    receive_time_us: i64,
    code: i32,
}

#[derive(Debug, PartialEq, Serialize)]
struct Step {
    input_timestamp: u32,
    input: Vec<i16>,
    #[serde(serialize_with = "serialize_debug")]
    encoded: AudioEncoderOutput,
    insertion: Option<Insertion>,
    #[serde(serialize_with = "serialize_debug")]
    omitted_header: Option<RtpHeaderValues>,
    codec_map_changes: Vec<CodecMapChange>,
    code: i32,
    muted: bool,
    output_rate_hz: i32,
    #[serde(serialize_with = "serialize_debug")]
    fields: AudioFrameFields,
    channel_layout: i32,
    absolute_capture_timestamp_ms: Option<i64>,
    samples: Vec<i16>,
    state: State,
}

#[derive(Debug, PartialEq, Serialize)]
struct Observation {
    omitted_packet_indices: Vec<usize>,
    reconfigure_to_pcma: bool,
    clock_bounds_us: (i64, i64),
    started_at_us: i64,
    finished_at_us: i64,
    before: State,
    steps: Vec<Step>,
    final_network_code: i32,
    #[serde(serialize_with = "serialize_debug")]
    final_network: NetEqNetworkStatistics,
    after_reset: State,
}

fn application<R: SessionRuntime>(
    runtime: R,
    omitted_packet_indices: &[usize],
    reconfigure_to_pcma: bool,
) -> Observation {
    let factory = runtime.environment_factory().unwrap();
    let environment = runtime.create_environment(&factory).unwrap();
    let format = SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1);
    let encoders = AudioEncoderFactory::builtin().unwrap();
    let decoders = AudioDecoderFactory::builtin().unwrap();
    let pcmu_encoder = encoders
        .create(
            &environment,
            &format,
            &AudioEncoderOptions {
                payload_type: 0,
                codec_pair_id: None,
            },
        )
        .unwrap()
        .expect("actual native PCMU encoder");
    let pcma_format = SdpAudioFormat::new(b"PCMA".to_vec(), 8000, 1);
    let pcma_encoder = reconfigure_to_pcma.then(|| {
        encoders
            .create(
                &environment,
                &pcma_format,
                &AudioEncoderOptions {
                    payload_type: 8,
                    codec_pair_id: None,
                },
            )
            .unwrap()
            .expect("actual native PCMA encoder")
    });
    let mut config = NetEqConfig::default();
    config.sample_rate_hz = 8000;
    let mut engine = NetEq::stock(&environment, &config, &decoders).unwrap();
    assert!(engine.register_payload_type(0, &format));
    let bounds = engine.clock_time_bounds();
    assert_eq!(bounds, environment.clock_time_bounds());
    drop(factory);
    drop(encoders);
    drop(decoders);
    // Keep this exact selected environment to sample arrival time, not another
    // clock or a normalized logical timestamp. Products retain their own copies.
    let started_at_us = environment.timestamp().microseconds().unwrap();
    let before = State::take(&mut engine);
    let mut steps = Vec::new();
    let mut sequence = u16::MAX - 4;
    let mut packet_index = 0;
    for block in 0..40 {
        let input_timestamp = 1000 + block * 80;
        let input = (0..80)
            .map(|i| {
                if (block * 80 + i) % 8 < 4 {
                    12000
                } else {
                    -12000
                }
            })
            .collect::<Vec<_>>();
        let (encoder, payload_type) = if reconfigure_to_pcma && block >= 20 {
            (pcma_encoder.as_ref().unwrap(), 8)
        } else {
            (&pcmu_encoder, 0)
        };
        let encoded = encoder.encode(input_timestamp, &input).unwrap();
        let mut omitted_header = None;
        let mut codec_map_changes = Vec::new();
        let insertion = if encoded.bytes.is_empty() {
            None
        } else {
            assert_eq!(encoded.info.primary.payload_type, i32::from(payload_type));
            let switching = reconfigure_to_pcma && packet_index == 10;
            if switching {
                // Admit the new format without replacing the old current PT.
                // No flush or normalized buffer state is manufactured here.
                codec_map_changes.push(CodecMapChange::apply(
                    &mut engine,
                    false,
                    BTreeMap::from([(0, format.clone()), (8, pcma_format.clone())]),
                ));
            }
            let header = RtpHeaderValues {
                payload_type,
                sequence_number: sequence,
                timestamp: encoded.info.primary.encoded_timestamp,
                ssrc: 73,
                ..Default::default()
            };
            // External packet loss does not rewind the sender. Keep the actual
            // encoded bytes/header even when no receiver insertion takes place.
            sequence = sequence.wrapping_add(1);
            let omit = omitted_packet_indices.contains(&packet_index);
            packet_index += 1;
            if omit {
                omitted_header = Some(header);
                None
            } else {
                let receive_time = environment.timestamp();
                assert!(
                    engine.receive_time_is_safe(receive_time),
                    "retained bounds must prove finite subtraction"
                );
                let code = engine
                    .insert_packet_at(&header, &encoded.bytes, receive_time)
                    .unwrap();
                assert_eq!(code, NetEqReturnCode::OK);
                if switching {
                    // Genuine PT8 input drives the native payload switch. Retire
                    // PT0 with PT8 still queued, before any GetAudio for this packet.
                    codec_map_changes.push(CodecMapChange::apply(
                        &mut engine,
                        true,
                        BTreeMap::from([(8, pcma_format.clone())]),
                    ));
                }
                Some(Insertion {
                    header,
                    receive_time_us: receive_time.microseconds().unwrap(),
                    code: code.0,
                })
            }
        };
        let mut frame = AudioFrame::new();
        let mut muted = true;
        let mut output_rate_hz = -1;
        let code = engine
            .get_audio(
                &mut frame,
                Some(&mut muted),
                Some(&mut output_rate_hz),
                None,
            )
            .unwrap();
        assert_eq!(code, NetEqReturnCode::OK);
        let fields = frame.fields();
        assert_eq!(
            (
                fields.sample_rate_hz,
                fields.num_channels,
                fields.samples_per_channel
            ),
            (8000, 1, 80)
        );
        let samples = frame.data_view().unwrap().to_vec();
        steps.push(Step {
            input_timestamp,
            input,
            encoded,
            insertion,
            omitted_header,
            codec_map_changes,
            code: code.0,
            muted,
            output_rate_hz,
            fields,
            channel_layout: frame.channel_layout().0,
            absolute_capture_timestamp_ms: frame.absolute_capture_timestamp_ms(),
            samples,
            state: State::take(&mut engine),
        });
    }
    assert_eq!(packet_index, 20);
    let headers = steps
        .iter()
        .filter_map(|step| {
            step.insertion
                .as_ref()
                .map(|i| &i.header)
                .or(step.omitted_header.as_ref())
        })
        .collect::<Vec<_>>();
    assert_eq!(headers.len(), 20);
    for (index, header) in headers.iter().enumerate() {
        assert_eq!(
            header.sequence_number,
            (u16::MAX - 4).wrapping_add(index as u16)
        );
        assert_eq!(header.timestamp, 1000 + index as u32 * 160);
        assert_eq!(
            header.payload_type,
            if reconfigure_to_pcma && index >= 10 {
                8
            } else {
                0
            }
        );
    }
    let changes = steps
        .iter()
        .flat_map(|s| &s.codec_map_changes)
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), if reconfigure_to_pcma { 2 } else { 0 });
    if reconfigure_to_pcma {
        let (admit, retire) = (changes[0], changes[1]);
        assert!(!admit.after_insertion);
        assert!(retire.after_insertion);
        assert_eq!(
            admit.before.decoder.as_ref().unwrap(),
            &NetEqDecoderFormat {
                payload_type: 0,
                sample_rate_hz: 8000,
                num_channels: 1,
                sdp_format: format.clone(),
            }
        );
        assert_eq!(admit.before, admit.after);
        assert_eq!(
            retire.before.decoder.as_ref().unwrap(),
            &NetEqDecoderFormat {
                payload_type: 8,
                sample_rate_hz: 8000,
                num_channels: 1,
                sdp_format: pcma_format.clone(),
            }
        );
        assert!(retire.before.operations.current_buffer_size_ms > 0);
        assert_eq!(
            retire.before, retire.after,
            "retiring PT0 must preserve queued PT8 and existing playout state"
        );
        let switched = steps
            .iter()
            .position(|s| !s.codec_map_changes.is_empty())
            .unwrap();
        assert_eq!(switched, 21);
        assert_ne!(steps[19].encoded.bytes, steps[switched].encoded.bytes);
        assert!(steps[switched..].iter().any(|s| {
            s.fields.speech_type == AudioFrameSpeechType::NORMAL_SPEECH
                && !s.fields.packet_infos.is_empty()
                && s.samples.iter().any(|&v| v != 0)
                && s.state
                    .decoder
                    .as_ref()
                    .is_some_and(|d| d.payload_type == 8)
        }));
        let last = &steps.last().unwrap().state;
        assert_eq!(last.decoder, retire.after.decoder);
        assert!(
            last.lifetime.total_samples_received > admit.before.lifetime.total_samples_received
        );
        assert!(
            last.lifetime.jitter_buffer_emitted_count
                > admit.before.lifetime.jitter_buffer_emitted_count
        );
    }
    let omissions = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.omitted_header.is_some())
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    assert_eq!(omissions.len(), omitted_packet_indices.len());
    if let (Some(&first), Some(&last)) = (omissions.first(), omissions.last()) {
        let pre_loss = &steps[first - 1].state.lifetime;
        let during_loss = &steps[last].state.lifetime;
        assert!(during_loss.concealed_samples > pre_loss.concealed_samples);
        assert!(during_loss.concealment_events > pre_loss.concealment_events);
        assert!(
            steps[last + 1..].iter().any(|s| {
                s.fields.speech_type == AudioFrameSpeechType::NORMAL_SPEECH
                    && !s.fields.packet_infos.is_empty()
                    && s.samples.iter().any(|&v| v != 0)
            }),
            "normal native speech with received-packet metadata must resume after the burst"
        );
        assert!(
            steps.last().unwrap().state.lifetime.total_samples_received
                > during_loss.total_samples_received
        );
    }
    let (final_network_code, final_network) = engine.network_statistics();
    assert_eq!(final_network_code, NetEqReturnCode::OK);
    let after_reset = State::take(&mut engine);
    assert!(after_reset.lifetime.total_samples_received >= 1600);
    assert!(after_reset.lifetime.jitter_buffer_emitted_count > 0);
    assert!(after_reset.playout_timestamp.is_some());
    assert!(
        steps
            .iter()
            .any(|s| s.state.operations.current_buffer_size_ms > 0)
    );
    assert!(
        steps
            .iter()
            .filter(|s| s.samples.iter().any(|&v| v != 0))
            .count()
            >= 20
    );
    assert!(
        steps.iter().any(|s| !s.fields.packet_infos.is_empty()),
        "native finite arrival metadata must reach output frames"
    );
    drop(pcmu_encoder);
    drop(pcma_encoder);
    drop(engine);
    let finished_at_us = environment.timestamp().microseconds().unwrap();
    drop(environment);
    Observation {
        omitted_packet_indices: omitted_packet_indices.to_vec(),
        reconfigure_to_pcma,
        clock_bounds_us: (
            bounds.minimum().microseconds(),
            bounds.maximum().microseconds(),
        ),
        started_at_us,
        finished_at_us,
        before,
        steps,
        final_network_code: final_network_code.0,
        final_network,
        after_reset,
    }
}

fn replay_case(omitted_packet_indices: &[usize], reconfigure_to_pcma: bool, trace_suffix: &str) {
    let threaded = application(ThreadedRuntime, omitted_packet_indices, reconfigure_to_pcma);
    let managed = std::array::from_fn::<_, 2, _>(|_| {
        eprintln!("CONTROLLED_MEDIA_BEGIN");
        let world = ControlledWorld::acquire(1005, Duration::from_secs(7)).unwrap();
        let observation = application(
            world.runtime().unwrap(),
            omitted_packet_indices,
            reconfigure_to_pcma,
        );
        assert_eq!(world.now(), Duration::from_secs(7));
        drop(world);
        eprintln!("CONTROLLED_MEDIA_END");
        observation
    });
    if let Some(mut path) = std::env::var_os("PULSEBEAM_NETEQ_TRACE_PATH") {
        path.push(trace_suffix);
        std::fs::write(path, serde_json::to_vec(&(&threaded, &managed)).unwrap()).unwrap();
    }
    // Same complete clock/arrival inputs prove full managed replay, including
    // every native statistic and received-packet field, not just decoded PCM.
    assert_eq!(managed[0], managed[1]);
    for observation in &managed {
        assert_eq!(observation.started_at_us, 7_000_000);
        assert_eq!(observation.finished_at_us, 7_000_000);
        assert_eq!(threaded.steps.len(), observation.steps.len());
        for (real, controlled) in threaded.steps.iter().zip(&observation.steps) {
            assert_eq!(real.encoded, controlled.encoded);
            assert_eq!(real.samples, controlled.samples);
        }
    }
    // Default threaded arrivals use real environment time, not the managed
    // 7s clock. Retain all of those timestamps and processing-delay statistics;
    // claiming full cross-runtime timing equality would violate preconditions.
}

#[test]
fn native_pcmu_packets_play_through_stock_neteq_and_replay() {
    replay_case(&[], false, "");
}

#[test]
fn native_pcmu_packet_loss_conceals_and_recovers_with_full_replay() {
    // Three consecutive 20ms encoder packets after six packets of warm-up.
    // Do not prescribe a literal PLC waveform or the first concealment pull.
    // Every actual native output sample still participates in full replay.
    replay_case(&[6, 7, 8], false, ".loss");
}

#[test]
fn native_codec_map_replacement_uses_pcma_packets_and_replays() {
    replay_case(&[], true, ".reconfigure");
}
