//! Direct stock L16/multiopus consumers, without changing peer advertisement.
use pulsebeam_webrtc_sys::*;
use serde::Serialize;
use std::time::Duration;

const RATE: i32 = 48_000;
const BLOCKS: usize = 12;
const PERIODS: [usize; 6] = [48, 40, 32, 24, 20, 16];

#[derive(Debug, Eq, PartialEq, Serialize)]
struct Leaf {
    encoded_bytes: usize,
    encoded_timestamp: u32,
    payload_type: i32,
    send_even_if_empty: bool,
    speech: bool,
    encoder_type: i32,
}
impl From<&AudioEncodedInfoLeaf> for Leaf {
    fn from(value: &AudioEncodedInfoLeaf) -> Self {
        Self {
            encoded_bytes: value.encoded_bytes,
            encoded_timestamp: value.encoded_timestamp,
            payload_type: value.payload_type,
            send_even_if_empty: value.send_even_if_empty,
            speech: value.speech,
            encoder_type: value.encoder_type.0,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct PacketInfo {
    duration: i32,
    redundant_duration: i32,
    has_fec: bool,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct Step {
    input_timestamp: u32,
    input: Vec<i16>,
    frames_in_next_packet: usize,
    bytes: Vec<u8>,
    primary: Leaf,
    redundant: Vec<Leaf>,
    packet_info: Option<PacketInfo>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct Frame {
    step: usize,
    timestamp: u32,
    priority: i32,
    duration: usize,
    is_dtx: bool,
    sample_rate_hz: i32,
    channels: usize,
    speech_type: i32,
    samples: Vec<i16>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct Observation {
    name: Vec<u8>,
    parameters: Vec<(Vec<u8>, Vec<u8>)>,
    sample_rate_hz: i32,
    channels: usize,
    rtp_timestamp_rate_hz: i32,
    maximum_frames_in_packet: usize,
    target_bitrate_bps: i32,
    dtx: bool,
    steps: Vec<Step>,
    frames: Vec<Frame>,
}

fn format(multichannel: bool) -> SdpAudioFormat {
    if !multichannel {
        return SdpAudioFormat::new(b"L16".to_vec(), RATE, 2);
    }
    let mut format = SdpAudioFormat::new(b"multiopus".to_vec(), RATE, 6);
    for (key, value) in [
        ("num_streams", "4"),
        ("coupled_streams", "2"),
        ("channel_mapping", "0,4,1,2,3,5"),
    ] {
        format
            .parameters
            .insert(key.as_bytes().to_vec(), value.as_bytes().to_vec());
    }
    format
}

fn application<R: SessionRuntime>(runtime: R, format: &SdpAudioFormat) -> Observation {
    let factory = runtime.environment_factory().unwrap();
    let environment = runtime.create_environment(&factory).unwrap();
    let encoders = AudioEncoderFactory::builtin().unwrap();
    let decoders = AudioDecoderFactory::builtin().unwrap();
    let encoder = encoders
        .create(
            &environment,
            format,
            &AudioEncoderOptions {
                payload_type: 111,
                codec_pair_id: None,
            },
        )
        .unwrap()
        .expect("directly constructible native encoder, despite no advertisement");
    let decoder = decoders
        .create(&environment, format)
        .unwrap()
        .expect("matching native decoder");
    assert_eq!(encoder.sample_rate_hz(), RATE);
    assert_eq!(decoder.sample_rate_hz(), RATE);
    assert_eq!(encoder.channels(), format.num_channels);
    assert_eq!(decoder.channels(), format.num_channels);
    // Products retain their actual creation context after configuring owners go.
    drop(encoders);
    drop(decoders);
    drop(environment);
    drop(factory);

    let mut observation = Observation {
        name: format.name.clone(),
        parameters: format
            .parameters
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        sample_rate_hz: encoder.sample_rate_hz(),
        channels: encoder.channels(),
        rtp_timestamp_rate_hz: encoder.rtp_timestamp_rate_hz(),
        maximum_frames_in_packet: encoder.maximum_frames_in_packet(),
        target_bitrate_bps: encoder.target_bitrate_bps(),
        dtx: encoder.dtx(),
        steps: Vec::new(),
        frames: Vec::new(),
    };
    let mut parsed = Vec::new();
    for block in 0..BLOCKS {
        let input = (0..480)
            .flat_map(|sample| {
                (0..format.num_channels).map(move |channel| {
                    let phase = (block * 480 + sample) as f64 * std::f64::consts::TAU
                        / PERIODS[channel] as f64;
                    (12_000.0 * phase.sin()).round() as i16
                })
            })
            .collect::<Vec<_>>();
        let input_timestamp = (u32::MAX - 239).wrapping_add((block * 480) as u32);
        let frames_in_next_packet = encoder.frames_in_next_packet();
        let packet = encoder.encode(input_timestamp, &input).unwrap();
        assert_eq!(packet.info.primary.encoded_bytes, packet.bytes.len());
        let packet_info = if packet.bytes.is_empty() {
            None
        } else {
            assert_eq!(packet.info.primary.payload_type, 111);
            let info = decoder.packet_info(&packet.bytes).unwrap();
            for frame in decoder
                .parse_payload(packet.bytes.clone(), packet.info.primary.encoded_timestamp)
                .unwrap()
            {
                parsed.push((block, frame));
            }
            Some(PacketInfo {
                duration: info.duration,
                redundant_duration: info.redundant_duration,
                has_fec: info.has_fec,
            })
        };
        // Retain empty buffering results and all native metadata, not just packets.
        observation.steps.push(Step {
            input_timestamp,
            input,
            frames_in_next_packet,
            bytes: packet.bytes,
            primary: Leaf::from(&packet.info.primary),
            redundant: packet.info.redundant.iter().map(Leaf::from).collect(),
            packet_info,
        });
    }
    drop(encoder);
    // Parsed owning frames must keep their real decoder/environment alive.
    drop(decoder);
    for (step, parsed) in parsed {
        let frame = parsed.frame.expect("native parsed frame");
        let mut record = Frame {
            step,
            timestamp: parsed.timestamp,
            priority: parsed.priority,
            duration: frame.duration(),
            is_dtx: frame.is_dtx_packet(),
            sample_rate_hz: frame.sample_rate_hz(),
            channels: frame.channels(),
            speech_type: 0,
            samples: Vec::new(),
        };
        let decoded = frame.decode().unwrap().expect("actual native decode");
        record.speech_type = decoded.speech_type.0;
        record.samples = decoded.samples;
        assert_eq!(record.samples.len(), record.duration * record.channels);
        observation.frames.push(record);
    }
    observation
}

fn check(multichannel: bool, trace_variable: &str) {
    let format = format(multichannel);
    let threaded = application(ThreadedRuntime, &format);
    let managed = std::array::from_fn::<_, 2, _>(|_| {
        eprintln!("CONTROLLED_MEDIA_BEGIN");
        let world = ControlledWorld::acquire(1004, Duration::from_secs(7)).unwrap();
        let observation = application(world.runtime().unwrap(), &format);
        assert_eq!(world.now(), Duration::from_secs(7));
        drop(world);
        eprintln!("CONTROLLED_MEDIA_END");
        observation
    });
    // Only passive owned observations survive each complete native lifecycle.
    if let Some(path) = std::env::var_os(trace_variable) {
        std::fs::write(path, serde_json::to_vec(&(&threaded, &managed)).unwrap()).unwrap();
    }
    for observation in &managed {
        assert_eq!(observation, &threaded, "same application across runtimes");
    }
    assert_eq!(threaded.steps.len(), BLOCKS);
    assert!(
        threaded
            .steps
            .iter()
            .filter(|s| !s.bytes.is_empty())
            .count()
            >= 3
    );
    for timestamps in [
        threaded
            .steps
            .iter()
            .filter(|step| !step.bytes.is_empty())
            .map(|step| step.primary.encoded_timestamp)
            .collect::<Vec<_>>(),
        threaded
            .frames
            .iter()
            .map(|frame| frame.timestamp)
            .collect(),
    ] {
        assert!(timestamps.iter().any(|&t| t > u32::MAX - 480));
        assert!(timestamps.iter().any(|&t| t < 10_000));
    }
    let input: Vec<_> = threaded
        .steps
        .iter()
        .flat_map(|s| &s.input)
        .copied()
        .collect();
    let decoded: Vec<_> = threaded
        .frames
        .iter()
        .flat_map(|f| &f.samples)
        .copied()
        .collect();
    assert_eq!(decoded.len(), input.len());
    assert!(
        threaded
            .frames
            .iter()
            .all(|f| f.sample_rate_hz == RATE && f.channels == format.num_channels)
    );
    if !multichannel {
        assert_eq!(decoded, input, "native L16 lossless stereo roundtrip");
    } else {
        // Phase-independent tone energy proves every distinct channel survived
        // the real multi-stream mapping. No startup samples are removed.
        for channel in 0..format.num_channels {
            let samples: Vec<_> = decoded
                .iter()
                .skip(channel)
                .step_by(format.num_channels)
                .collect();
            let mut real = 0.0;
            let mut imaginary = 0.0;
            let mut energy = 0.0;
            for (index, &&sample) in samples.iter().enumerate() {
                let phase = index as f64 * std::f64::consts::TAU / PERIODS[channel] as f64;
                let sample = f64::from(sample);
                real += sample * phase.cos();
                imaginary += sample * phase.sin();
                energy += sample * sample;
            }
            assert!(energy > 1_000_000.0);
            let ratio =
                2.0 * (real * real + imaginary * imaginary) / (samples.len() as f64 * energy);
            assert!(
                ratio > 0.5,
                "channel {channel} lost its source tone: {ratio}"
            );
        }
    }
}

#[test]
fn native_l16_stereo_roundtrip_replays_across_runtimes() {
    check(false, "PULSEBEAM_L16_TRACE_PATH");
}

#[test]
fn native_multiopus_six_channels_roundtrip_replays_across_runtimes() {
    check(true, "PULSEBEAM_MULTIOPUS_TRACE_PATH");
}
