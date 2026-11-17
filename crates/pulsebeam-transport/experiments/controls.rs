use super::*;

fn scenario(name: &str) -> Scenario {
    *SCENARIOS
        .iter()
        .find(|scenario| scenario.name == name)
        .unwrap()
}

fn options() -> Audit {
    Audit::default()
}

#[test]
fn lossless_fifo_has_exact_serialization_delay_and_independent_directions() {
    let mut link = Link::new(scenario("clean"), 1, 6000);
    link.push(0, 0, 1000, 1).unwrap();
    link.push(1, 0, 1000, 2).unwrap();
    link.push(0, 1, 100, 3).unwrap();
    assert_eq!(link.pop(18), None);
    assert_eq!(link.pop(19), Some((1, 1)));
    assert_eq!(link.pop(19), Some((0, 3)));
    assert_eq!(link.pop(21), None);
    assert_eq!(link.pop(22), Some((1, 2)));
    assert!(link.pending.is_empty());
    assert_eq!(link.dropped, 0);
    assert_eq!(link.bytes, [2000, 100]);
}

#[test]
fn queue_overflow_is_explicit_and_does_not_consume_service() {
    let mut link = Link::new(scenario("clean"), 1, 6000);
    for packet in 0..63 {
        link.push(0, 0, 1000, packet).unwrap();
    }
    let finish = link.finish[0];
    assert_eq!(link.push(0, 0, 1000, 63), Err(63));
    assert_eq!(link.finish[0], finish);
    assert_eq!(link.pending.len(), 63);
    assert_eq!(link.dropped, 1);
}

#[test]
fn serialization_crosses_rate_changes_at_the_actual_boundary() {
    let link = Link::<()>::new(scenario("capacity_step"), 1, 6000);
    assert_eq!(
        link.serialization_finish(1_999_000_000, 1000, false),
        2_009_166_667
    );
    assert_eq!(
        link.serialization_finish(3_999_000_000, 1000, false),
        4_002_960_000
    );
    assert_eq!(
        link.serialization_finish(1_999_000_000, 1000, true),
        2_030_250_000
    );
}

#[test]
fn measurement_handoff_preserves_outstanding_packets_and_serialization_debt() {
    let mut link = Link::new(scenario("clean"), 1, 6000);
    link.push(100, 0, 1000, 7).unwrap();
    link.begin_measurement(101, 6000, 1);
    assert_eq!(link.bytes, [0; 2]);
    assert_eq!(link.finish[0], 2_200_000);
    assert_eq!(link.pop(17), None);
    assert_eq!(link.pop(18), Some((1, 7)));
}

#[test]
fn coarse_host_turns_and_one_unit_polls_preserve_feedback_progress() {
    for cc in [QuicCongestion::OuterControlled, QuicCongestion::Cubic] {
        for budget in [1, 256] {
            let metrics = quic_run(
                scenario("host_turn_4ms"),
                1,
                2000,
                cc,
                true,
                Audit {
                    poll_budget: budget,
                    ..options()
                },
            );
            assert!(
                !metrics.closed,
                "{cc:?}, budget={budget}: {:?}",
                metrics.failure
            );
            assert_eq!(metrics.network_drops, 0);
            let offered_after_startup = (0..metrics.frames)
                .filter(|id| *id as u64 * FRAME_MS >= 1000)
                .count();
            let timely_after_startup = metrics
                .complete
                .iter()
                .filter(|(id, at)| {
                    **id as u64 * FRAME_MS >= 1000 && **at <= **id as u64 * FRAME_MS + DEADLINE_MS
                })
                .count();
            assert_eq!(
                timely_after_startup, offered_after_startup,
                "{cc:?}, budget={budget}"
            );
        }
    }
}

#[test]
fn dropping_charged_ciphertext_fabricates_loss_on_a_lossless_high_rtt_path() {
    let retained = quic_run(
        scenario("rtt_160ms"),
        1,
        6000,
        QuicCongestion::Cubic,
        false,
        options(),
    );
    let dropped = quic_run(
        scenario("rtt_160ms"),
        1,
        6000,
        QuicCongestion::Cubic,
        false,
        Audit {
            retain_protected: false,
            ..options()
        },
    );
    assert!(!retained.closed);
    assert_eq!(retained.network_drops, 0);
    assert_eq!(retained.staged_expired_packets, Some(0));
    assert_eq!(dropped.network_drops, 0);
    assert!(dropped.staged_expired_packets.unwrap() > 0);
    assert!(dropped.quic_losses > retained.quic_losses);
    assert!(retained.complete.len() > dropped.complete.len());
}

#[test]
fn fixture_padding_preserves_independently_decoded_pixels() {
    use pulsebeam_webrtc_sys::{
        DecodedImageCallbackProvider, DecodedImageCallbackSink, EncodedImage,
        EncodedImageFrameType, EncodedVideoBuffer, Environment, OwnedVideoDecoderSettings,
        RawVideoFrame, SdpVideoFormat, VideoDecoderFactoryHandle,
    };
    use std::sync::mpsc::{Sender, channel};

    struct Output(Sender<Vec<u8>>);
    impl Output {
        fn capture(&self, frame: &RawVideoFrame) {
            assert_eq!((frame.width(), frame.height()), (16, 16));
            self.0
                .send(frame.to_legacy().unwrap().buffer.as_bytes().to_vec())
                .unwrap();
        }
    }
    impl DecodedImageCallbackProvider for Output {
        fn decoded(&self, frame: &mut RawVideoFrame) -> i32 {
            self.capture(frame);
            0
        }
        fn decoded_with_time(&self, frame: &mut RawVideoFrame, _: i64) -> i32 {
            self.capture(frame);
            0
        }
        fn decoded_with_metadata(&self, frame: &mut RawVideoFrame, _: Option<i32>, _: Option<u8>) {
            self.capture(frame);
        }
    }
    let environment = Environment::builder().build().unwrap();
    let factory = VideoDecoderFactoryHandle::builtin().unwrap();
    let mut decoder = factory
        .create_decoder(&environment, &SdpVideoFormat::new("H264"))
        .unwrap();
    let mut settings = OwnedVideoDecoderSettings::default();
    let mut fields = settings.fields();
    fields.codec_type = 4; // SDK VideoCodecType::kVideoCodecH264, the exact owned-settings ordinal.
    fields.number_of_cores = 1;
    fields.max_render_width = 16;
    fields.max_render_height = 16;
    settings.set_fields(fields).unwrap();
    assert!(decoder.configure(&settings));
    let (decoded, received) = channel();
    let sink = DecodedImageCallbackSink::new(Output(decoded));
    let mut pixels = Vec::new();
    assert_eq!(decoder.register_callback(Some(&sink)), 0);
    for (index, bytes) in [KEY.to_vec(), frame(0), frame(1), frame(4)]
        .into_iter()
        .enumerate()
    {
        let mut image = EncodedImage::default();
        image.set_buffer(&EncodedVideoBuffer::from_owned_bytes(bytes));
        let mut metadata = image.metadata();
        metadata.width = 16;
        metadata.height = 16;
        metadata.frame_type = EncodedImageFrameType::Key;
        metadata.rtp_timestamp = u32::try_from(index).unwrap() * 3000;
        image.set_metadata(&metadata).unwrap();
        assert_eq!(decoder.decode(&image, 0).unwrap(), 0);
        pixels.push(
            received
                .try_recv()
                .expect("synchronous native decoder output"),
        );
        assert!(received.try_recv().is_err());
    }
    assert_eq!(pixels[0].len(), 16 * 16 * 3 / 2);
    assert!(pixels.iter().all(|frame| *frame == pixels[0]));
    assert_eq!(decoder.release(), 0);
}
