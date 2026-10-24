//! Standalone caller-product source coverage; execution is still deferred.
use pulsebeam_webrtc_sys::*;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Calls {
    packets: Vec<(
        u8,
        RtpHeaderValues,
        Vec<u8>,
        Option<Timestamp>,
        Option<RtpReceivedPacketInfo>,
    )>,
    codecs: BTreeMap<i32, SdpAudioFormat>,
    registration: Option<(i32, SdpAudioFormat)>,
    controls: Vec<(&'static str, i128)>,
    overrides: Vec<Option<NetEqOperation>>,
    outputs: Vec<(bool, bool)>,
    prior_sample: Option<i16>,
    drops: usize,
}
#[derive(Clone, Copy)]
enum AudioBehavior {
    Success,
    NativeFailure,
    Panic,
    InvalidShape,
}
struct Engine {
    calls: Arc<Mutex<Calls>>,
    audio: AudioBehavior,
    panic_drop: bool,
}
impl Engine {
    fn control(&self, name: &'static str, value: i128) {
        self.calls.lock().unwrap().controls.push((name, value));
    }
    fn packet(
        &self,
        overload: u8,
        header: &RtpHeaderValues,
        payload: &[u8],
        time: Option<Timestamp>,
        info: Option<&RtpReceivedPacketInfo>,
    ) {
        self.calls.lock().unwrap().packets.push((
            overload,
            header.clone(),
            payload.to_vec(),
            time,
            info.cloned(),
        ));
    }
}
fn format() -> SdpAudioFormat {
    let mut format = SdpAudioFormat::new(vec![b'x', 0, 255], i32::MIN, usize::MAX);
    format.parameters.insert(vec![0, 254], vec![253, 0]);
    format
}
fn decoder_format(payload_type: i32) -> NetEqDecoderFormat {
    NetEqDecoderFormat {
        payload_type,
        sample_rate_hz: i32::MAX,
        num_channels: i32::MIN,
        sdp_format: format(),
    }
}
fn network_stats() -> NetEqNetworkStatistics {
    NetEqNetworkStatistics {
        current_buffer_size_ms: u16::MAX,
        mean_waiting_time_ms: i32::MIN,
        max_waiting_time_ms: i32::MAX,
        ..Default::default()
    }
}
impl NetEqProvider for Engine {
    fn insert_packet(&mut self, h: &RtpHeaderValues, p: &[u8]) -> NetEqReturnCode {
        self.packet(0, h, p, None, None);
        NetEqReturnCode(41)
    }
    fn insert_packet_at(&mut self, h: &RtpHeaderValues, p: &[u8], t: Timestamp) -> NetEqReturnCode {
        self.packet(1, h, p, Some(t), None);
        NetEqReturnCode(-42)
    }
    fn insert_packet_with_info(
        &mut self,
        h: &RtpHeaderValues,
        p: &[u8],
        info: &RtpReceivedPacketInfo,
    ) -> NetEqReturnCode {
        self.packet(2, h, p, None, Some(info));
        NetEqReturnCode(43)
    }
    fn insert_empty_packet(&mut self, h: &RtpHeaderValues) {
        self.packet(3, h, &[], None, None);
    }
    fn get_audio(
        &mut self,
        frame: &mut AudioFrameMut<'_>,
        mut muted: Option<NetEqOutput<'_, bool>>,
        mut rate: Option<NetEqOutput<'_, i32>>,
        operation: Option<NetEqOperation>,
    ) -> NetEqReturnCode {
        {
            let mut calls = self.calls.lock().unwrap();
            calls.overrides.push(operation);
            calls.outputs.push((muted.is_some(), rate.is_some()));
        }
        // Native output parameters are output-only, not input/output slots.
        assert!(muted.as_ref().is_none_or(|v| v.written_value().is_none()));
        assert!(rate.as_ref().is_none_or(|v| v.written_value().is_none()));
        match self.audio {
            AudioBehavior::Panic => panic!("contained caller audio panic"),
            AudioBehavior::NativeFailure => {
                let mut fields = frame.fields();
                fields.timestamp = 37;
                frame.set_fields(&fields).unwrap();
                return NetEqReturnCode(-53); // Do not touch output slots.
            }
            AudioBehavior::InvalidShape => {
                let mut fields = frame.fields();
                fields.sample_rate_hz = -31;
                frame.set_fields(&fields).unwrap();
                return NetEqReturnCode::OK;
            }
            AudioBehavior::Success => {}
        }
        self.calls.lock().unwrap().prior_sample = frame.data().ok().map(|v| v[7500]);
        frame
            .update_frame(
                137,
                Some(&vec![101; 441 * 16]),
                441,
                44100,
                AudioFrameSpeechType::NORMAL_SPEECH,
                AudioFrameVadActivity::ACTIVE,
                16,
            )
            .unwrap();
        // Root-owned storage evidence survives entry, including inactive data.
        // An SDK callback without full evidence must refuse this access.
        if let Ok(data) = frame.mutable_data() {
            data[7500] = 71;
        }
        frame.set_absolute_capture_timestamp_ms(149);
        if let Some(v) = &mut muted {
            v.write(false);
            assert_eq!(v.written_value(), Some(&false));
        }
        if let Some(v) = &mut rate {
            v.write(44100);
        }
        NetEqReturnCode::OK
    }
    fn set_codecs(&mut self, codecs: &BTreeMap<i32, SdpAudioFormat>) {
        self.calls.lock().unwrap().codecs = codecs.clone();
    }
    fn register_payload_type(&mut self, pt: i32, format: &SdpAudioFormat) -> bool {
        self.calls.lock().unwrap().registration = Some((pt, format.clone()));
        true
    }
    fn create_decoder(&mut self, pt: i32) -> bool {
        self.control("create", pt as i128);
        true
    }
    fn remove_payload_type(&mut self, pt: u8) -> NetEqReturnCode {
        self.control("remove", pt as i128);
        NetEqReturnCode(-47)
    }
    fn remove_all_payload_types(&mut self) {
        self.control("remove_all", 0);
    }
    fn set_maximum_buffer_packets(&mut self, count: usize) {
        self.control("max_packets", count as i128);
    }
    fn set_fast_accelerate(&mut self, value: bool) {
        self.control("fast", value as i128);
    }
    fn set_minimum_delay(&mut self, value: i32) -> bool {
        self.control("min_delay", value as i128);
        true
    }
    fn set_maximum_delay(&mut self, value: i32) -> bool {
        self.control("max_delay", value as i128);
        true
    }
    fn set_base_minimum_delay_ms(&mut self, value: i32) -> bool {
        self.control("base_delay", value as i128);
        true
    }
    fn base_minimum_delay_ms(&self) -> i32 {
        7
    }
    fn target_delay_ms(&self) -> i32 {
        11
    }
    fn filtered_current_delay_ms(&self) -> i32 {
        13
    }
    fn network_statistics(
        &mut self,
        stats: Option<NetEqOutput<'_, NetEqNetworkStatistics>>,
    ) -> NetEqReturnCode {
        self.control("reset_stats", stats.is_some() as i128);
        if let Some(mut stats) = stats {
            stats.write(network_stats());
        }
        NetEqReturnCode(-59)
    }
    fn current_network_statistics(&self) -> NetEqNetworkStatistics {
        network_stats()
    }
    fn lifetime_statistics(&self) -> NetEqLifetimeStatistics {
        NetEqLifetimeStatistics {
            total_samples_received: u64::MAX,
            interruption_count: i32::MIN,
            total_processing_delay_us: u64::MAX,
            ..Default::default()
        }
    }
    fn operations_and_state(&self) -> NetEqOperationsAndState {
        NetEqOperationsAndState {
            current_buffer_size_ms: u64::MAX,
            next_packet_available: true,
            ..Default::default()
        }
    }
    fn playout_timestamp(&self) -> Option<u32> {
        Some(u32::MAX)
    }
    fn last_output_sample_rate_hz(&self) -> i32 {
        -73
    } // Pure root reads retain raw provider values.
    fn decoder_format(&self, pt: i32) -> Option<NetEqDecoderFormat> {
        Some(decoder_format(pt))
    }
    fn current_decoder_format(&self) -> Option<NetEqDecoderFormat> {
        Some(decoder_format(-83))
    }
    fn flush_buffers(&mut self) {
        self.control("flush", 0);
    }
    fn enable_nack(&mut self, count: usize) {
        self.control("nack", count as i128);
    }
    fn disable_nack(&mut self) {
        self.control("no_nack", 0);
    }
    fn nack_list(&self, rtt: i64) -> Vec<u16> {
        self.control("rtt", rtt as i128);
        vec![u16::MAX, 0, 51]
    }
    fn sync_buffer_size_ms(&self) -> i32 {
        17
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.calls.lock().unwrap().drops += 1;
        if self.panic_drop {
            panic!("contained caller destructor panic");
        }
    }
}
fn product(audio: AudioBehavior, panic_drop: bool) -> (NetEq, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let environment = Environment::builder().build().unwrap();
    let config = NetEqConfig {
        sample_rate_hz: -91,
        max_packets_in_buffer: usize::MAX,
        max_delay_ms: i32::MAX,
        min_delay_ms: i32::MIN,
        ..Default::default()
    };
    let engine = NetEq::from_provider(
        &environment,
        &config,
        Engine {
            calls: calls.clone(),
            audio,
            panic_drop,
        },
    );
    (engine, calls) // Retained native environment outlives this local owner.
}

#[test]
fn all_packet_overloads_preserve_distinct_inputs_without_stock_policy() {
    let (mut engine, calls) = product(AudioBehavior::NativeFailure, false);
    let header = RtpHeaderValues {
        payload_type: 255,
        sequence_number: 65,
        timestamp: 67,
        ssrc: 71,
        extension: RtpHeaderExtensionValues {
            has_video_timing: false,
            video_timing: VideoSendTiming {
                encode_start_delta_ms: 1,
                encode_finish_delta_ms: 2,
                packetization_finish_delta_ms: 3,
                pacer_exit_delta_ms: 4,
                network_timestamp_delta_ms: 5,
                network2_timestamp_delta_ms: 6,
                flags: 0x81,
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let info = RtpReceivedPacketInfo {
        ssrc: 79,
        csrcs: vec![83; 20],
        sequence_number: 89,
        rtp_timestamp: 97,
        receive_time: Timestamp::plus_infinity(),
        audio_level: Some(255),
        local_capture_clock_offset: Some(TimeDelta::minus_infinity()),
        absolute_capture_time: Some(AbsoluteCaptureTime {
            absolute_capture_timestamp: u64::MAX,
            estimated_capture_clock_offset: Some(i64::MIN),
        }),
    };
    assert_eq!(
        engine.insert_packet(&header, &[1, 0, 255]),
        NetEqReturnCode(41)
    );
    assert_eq!(
        engine.insert_packet_at(&header, &[], Timestamp::plus_infinity()),
        Ok(NetEqReturnCode(-42))
    );
    assert_eq!(
        engine.insert_packet_with_info(&header, &[2], &info),
        Ok(NetEqReturnCode(43))
    );
    engine.insert_empty_packet(&header);
    assert_eq!(
        calls.lock().unwrap().packets,
        vec![
            (0, header.clone(), vec![1, 0, 255], None, None),
            (
                1,
                header.clone(),
                vec![],
                Some(Timestamp::plus_infinity()),
                None
            ),
            (2, header.clone(), vec![2], None, Some(info)),
            (3, header, vec![], None, None),
        ]
    );
    assert!(!engine.is_failed());
}

#[test]
fn codec_control_statistics_and_compatibility_surface_is_complete() {
    let (mut engine, calls) = product(AudioBehavior::NativeFailure, false);
    let codecs = BTreeMap::from([(i32::MIN, format()), (i32::MAX, format())]);
    engine.set_codecs(&codecs).unwrap();
    assert!(engine.register_payload_type(-107, &format()));
    assert!(engine.create_decoder(i32::MAX));
    assert_eq!(engine.remove_payload_type(255), NetEqReturnCode(-47));
    // Caller operations/state reports queued packets, but stock's RemoveAll
    // hazard policy does not apply to a caller engine's own storage contract.
    engine.remove_all_payload_types().unwrap();
    engine.set_maximum_buffer_packets(usize::MAX).unwrap();
    engine.set_fast_accelerate(true);
    assert!(engine.set_minimum_delay(i32::MIN));
    assert!(engine.set_maximum_delay(i32::MAX));
    assert!(engine.set_base_minimum_delay_ms(-113));
    assert_eq!(
        (
            engine.base_minimum_delay_ms(),
            engine.target_delay_ms(),
            engine.filtered_current_delay_ms()
        ),
        (7, 11, 13)
    );
    assert_eq!(
        engine.network_statistics(),
        (NetEqReturnCode(-59), network_stats())
    );
    assert_eq!(engine.current_network_statistics(), network_stats());
    assert_eq!(
        engine.lifetime_statistics().total_samples_received,
        u64::MAX
    );
    assert_eq!(
        engine.lifetime_statistics().total_processing_delay_us,
        u64::MAX
    );
    assert_eq!(engine.lifetime_statistics().interruption_count, i32::MIN);
    assert_eq!(
        engine.operations_and_state().current_buffer_size_ms,
        u64::MAX
    );
    assert!(engine.operations_and_state().next_packet_available);
    assert_eq!(engine.playout_timestamp(), Some(u32::MAX));
    assert_eq!(engine.last_output_sample_rate_hz(), -73);
    assert_eq!(
        engine.decoder_format(i32::MIN),
        Some(decoder_format(i32::MIN))
    );
    assert_eq!(engine.current_decoder_format(), Some(decoder_format(-83)));
    engine.flush_buffers();
    engine.enable_nack(usize::MAX);
    engine.disable_nack();
    assert_eq!(engine.nack_list(i64::MIN), vec![u16::MAX, 0, 51]);
    assert_eq!(engine.sync_buffer_size_ms(), 17);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.codecs, codecs);
    assert_eq!(calls.registration, Some((-107, format())));
    assert_eq!(
        calls.controls,
        vec![
            ("create", i32::MAX as i128),
            ("remove", 255),
            ("remove_all", 0),
            ("max_packets", usize::MAX as i128),
            ("fast", 1),
            ("min_delay", i32::MIN as i128),
            ("max_delay", i32::MAX as i128),
            ("base_delay", -113),
            ("reset_stats", 1),
            ("flush", 0),
            ("nack", usize::MAX as i128),
            ("no_nack", 0),
            ("rtt", i64::MIN as i128),
        ]
    );
}

#[test]
fn direct_frame_view_preserves_owner_storage_and_nonstock_rate_channel_output() {
    let (mut engine, calls) = product(AudioBehavior::Success, false);
    let mut frame = AudioFrame::new();
    frame.mutable_data()[7500] = 119;
    let mut muted = true;
    let mut rate = -127;
    assert_eq!(
        engine.get_audio(
            &mut frame,
            Some(&mut muted),
            Some(&mut rate),
            Some(NetEqOperation(i32::MIN))
        ),
        Ok(NetEqReturnCode::OK)
    );
    assert_eq!((muted, rate), (false, 44100));
    assert_eq!(frame.fields().timestamp, 137);
    assert_eq!(frame.absolute_capture_timestamp_ms(), Some(149));
    assert_eq!(
        (frame.num_channels(), frame.samples_per_channel()),
        (16, 441)
    );
    assert_eq!(frame.data_view().unwrap(), &[101; 441 * 16]);
    assert_eq!(frame.data()[7500], 71);
    assert_eq!(calls.lock().unwrap().prior_sample, Some(119));
    assert_eq!(
        calls.lock().unwrap().overrides,
        vec![Some(NetEqOperation(i32::MIN))]
    );
    assert_eq!(calls.lock().unwrap().outputs, vec![(true, true)]);
    assert!(!engine.is_failed());
    assert_eq!(engine.failure_observer().unwrap().failure(), None);
}

#[test]
fn native_failure_keeps_partial_frame_mutations_and_unwritten_optional_outputs() {
    let (mut engine, calls) = product(AudioBehavior::NativeFailure, false);
    let mut frame = AudioFrame::new();
    let mut muted = true;
    let mut rate = -131;
    assert_eq!(
        engine.get_audio(&mut frame, Some(&mut muted), Some(&mut rate), None),
        Ok(NetEqReturnCode(-53))
    );
    assert_eq!((muted, rate), (true, -131));
    assert_eq!(frame.fields().timestamp, 37);
    assert_eq!(
        engine.get_audio(&mut frame, None, None, None),
        Ok(NetEqReturnCode(-53))
    );
    assert_eq!(
        calls.lock().unwrap().outputs,
        vec![(true, true), (false, false)]
    );
    assert!(!engine.is_failed());
}

#[test]
fn panic_and_invalid_success_latch_failure_without_replacing_the_engine() {
    for (behavior, failure) in [
        (AudioBehavior::Panic, NetEqFailure::Panicked),
        (
            AudioBehavior::InvalidShape,
            NetEqFailure::InvalidAudioOutput,
        ),
    ] {
        let (mut engine, calls) = product(behavior, false);
        let observer = engine.failure_observer().unwrap();
        let mut frame = AudioFrame::new();
        let mut muted = true;
        let mut rate = -139;
        assert_eq!(
            engine.get_audio(&mut frame, Some(&mut muted), Some(&mut rate), None),
            Ok(NetEqReturnCode::FAIL)
        );
        assert_eq!((muted, rate), (true, -139));
        assert_eq!(observer.failure(), Some(failure));
        assert!(engine.is_failed());
        assert_eq!(
            engine.insert_packet(&RtpHeaderValues::default(), &[1]),
            NetEqReturnCode::FAIL
        );
        assert!(!engine.register_payload_type(0, &format()));
        assert_eq!(engine.last_output_sample_rate_hz(), 8000);
        assert_eq!(engine.playout_timestamp(), None);
        assert_eq!(engine.current_decoder_format(), None);
        assert_eq!(
            engine.current_network_statistics(),
            NetEqNetworkStatistics::default()
        );
        assert_eq!(
            engine.lifetime_statistics(),
            NetEqLifetimeStatistics::default()
        );
        engine.flush_buffers();
        assert!(calls.lock().unwrap().controls.is_empty());
        assert!(calls.lock().unwrap().packets.is_empty());
        drop(engine);
        assert_eq!(calls.lock().unwrap().drops, 1);
        assert_eq!(observer.failure(), Some(failure));
    }
}

#[test]
fn destructor_panic_is_contained_and_durable_observer_survives_product() {
    let (engine, calls) = product(AudioBehavior::NativeFailure, true);
    let observer = engine.failure_observer().unwrap();
    drop(engine);
    assert_eq!(calls.lock().unwrap().drops, 1);
    assert_eq!(observer.failure(), Some(NetEqFailure::Panicked));
}
