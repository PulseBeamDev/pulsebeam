use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use pulsebeam_agent_core::{AgentCommand, DesiredState, MediaKind, PublicationIntent};
use pulsebeam_agent_quic::{
    executor::{
        rtp::{SendPacket, SendPacketError},
        timed::{Policy, Receive},
    },
    fabric::Profile,
    media::units::{Access, Cause, Codec, Depacketizer, Dependency, Event, OrderedPacket},
};
use pulsebeam_rtc_simulation::{
    DecodedImageCallbackProvider, DecodedImageCallbackSink, EncodedImage, EncodedImageFrameType,
    EncodedVideoBuffer, Environment, OwnedVideoDecoder, OwnedVideoDecoderSettings, RawVideoFrame,
    SdpVideoFormat, VideoDecoderFactoryHandle,
};
use pulsebeam_runtime::time::{Instant, sleep_until, timeout};
use pulsebeam_testdata::wire_capture::{Cursor, PacketKind, descriptor_rtp};

use super::{
    DatagramAdmission, RootCertStore, pinned_video, sim_placement, sim_quic_agent_with_placement,
    stage_rtp_source,
};

const TRACK: &str = "whole-camera";
const PICTURES: usize = 20;

#[derive(Clone, Copy)]
pub(in crate::tests) enum Capture {
    H264,
    Vp8,
    Vp9,
    Av1,
}

impl Capture {
    fn format(self) -> (Profile, Codec, &'static str, &'static str) {
        match self {
            Self::H264 => (Profile::H264, Codec::H264, "video/h264", "H264"),
            Self::Vp8 => (Profile::Vp8, Codec::Vp8, "video/vp8", "VP8"),
            Self::Vp9 => (Profile::Vp9, Codec::Vp9, "video/vp9", "VP9"),
            Self::Av1 => (Profile::Av1, Codec::Av1, "video/av1", "AV1"),
        }
    }

    fn wire(self) -> (&'static [u8], &'static str) {
        macro_rules! fixture {
            ($path:literal) => {
                (
                    include_bytes!(concat!(
                        "../../../../pulsebeam-testdata/fixtures/",
                        $path,
                        "/wire.bin"
                    )),
                    include_str!(concat!(
                        "../../../../pulsebeam-testdata/fixtures/",
                        $path,
                        "/wire.tsv"
                    )),
                )
            };
        }
        match self {
            Self::H264 => fixture!("h264-v1/L1T1"),
            Self::Vp8 => fixture!("vp8-v1/L1T1"),
            Self::Vp9 => fixture!("vp9-v1/L1T1"),
            Self::Av1 => fixture!("av1-v1/L1T1"),
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::tests) struct Endpoint {
    pub placement: SocketAddr,
    pub edge: SocketAddr,
}

pub(in crate::tests) async fn prove_whole_video(
    case: Capture,
    local: IpAddr,
    [source_edge, viewer_edge]: [Endpoint; 2],
    roots: RootCertStore,
    publisher_token: &str,
    viewer_token: &str,
) -> Result<()> {
    let (profile, codec, mime, name) = case.format();
    let connect = |endpoint: Endpoint, token, roots| {
        sim_quic_agent_with_placement(
            endpoint.placement,
            local,
            endpoint.edge,
            roots,
            token,
            sim_placement,
        )
    };
    let mut publisher = connect(source_edge, publisher_token, roots.clone())?;
    publisher.configure_video_codec(TRACK, profile)?;
    publisher.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        publications: vec![PublicationIntent {
            slot: TRACK.into(),
            track_id: TRACK.into(),
            kind: MediaKind::Video,
            active: true,
        }],
        ..Default::default()
    }))?;
    publisher.command(AgentCommand::Join("stage".into()))?;
    let source = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(source) = publisher.rtp_publication(MediaKind::Video, TRACK) {
                return anyhow::Ok(source);
            }
            publisher
                .turn()
                .await
                .map_err(|e| anyhow::anyhow!("{name} publisher setup: {e:?}"))?;
        }
    })
    .await
    .with_context(|| format!("{name} publication not installed"))??;
    ensure!(publisher.quic_peer() == Some(source_edge.edge));

    let (wire, index) = case.wire();
    let mut capture =
        Cursor::new(wire, index).map_err(|_| anyhow::anyhow!("{name} capture index"))?;
    let mut assembler = Depacketizer::new((), codec).with_av1_whole_source();
    let mut packets = Vec::new();
    let mut expected = Vec::new();
    let captured_at = Instant::now().into_std();
    while let Some(record) = capture.next_due(Duration::MAX) {
        if record.kind != PacketKind::Rtp {
            continue;
        }
        let rtp = descriptor_rtp(record.bytes).with_context(|| format!("{name} captured RTP"))?;
        match assembler
            .push(
                (),
                OrderedPacket {
                    sequence: packets.len() as u64,
                    timestamp: rtp.timestamp,
                    rtp_time: None,
                    arrived_at: captured_at,
                    marker: rtp.marker,
                    payload: rtp.payload,
                    descriptor: None,
                },
            )
            .map_err(|e| anyhow::anyhow!("{name} capture assembly: {e:?}"))?
        {
            Some(Event::Unit(unit)) => expected.push((unit.access, unit.data)),
            None => {}
            event => anyhow::bail!("{name} capture discontinuity: {event:?}"),
        }
        packets.push((record.send_after, rtp));
    }
    ensure!(
        expected.len() >= PICTURES && expected[0].0 == Access::IndependentVideo,
        "incomplete {name} capture"
    );
    let send = async {
        let mut start = Instant::now();
        let mut next = start;
        let mut sequence = 0_u32;
        let mut rtp_offset = 0_u32;
        let cycle_ticks = u32::try_from(expected.len())? * 3000;
        let cycle = Duration::from_micros(u64::from(cycle_ticks) * 1000 / 90);
        loop {
            for (elapsed, packet) in &packets {
                let send_at = (start + *elapsed).max(next);
                while Instant::now() < send_at {
                    pulsebeam_runtime::select! {
                        () = sleep_until(send_at) => break,
                        result = publisher.turn() => result.map_err(|e| anyhow::anyhow!("{name} pacing: {e:?}"))?,
                    }
                }
                loop {
                    match publisher.send_rtp(
                        &source,
                        SendPacket {
                            sequence,
                            timestamp: packet.timestamp.wrapping_add(rtp_offset),
                            marker: packet.marker,
                            extensions: &[],
                            payload: packet.payload,
                        },
                    ) {
                        Ok(()) => break,
                        Err(SendPacketError::Datagram(
                            DatagramAdmission::ControlTurn | DatagramAdmission::Full,
                        )) => {}
                        Err(e) => anyhow::bail!("{name} replay: {e:?}"),
                    }
                    publisher
                        .turn()
                        .await
                        .map_err(|e| anyhow::anyhow!("{name} send progress: {e:?}"))?;
                }
                sequence = sequence.wrapping_add(1);
                next = Instant::now() + Duration::from_millis(1);
            }
            start += cycle;
            rtp_offset = rtp_offset.wrapping_add(cycle_ticks);
        }
    };
    let receive = async {
        let id = source.publication_id();
        let mut receiver = connect(viewer_edge, viewer_token, roots)?;
        receiver.command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 1,
            video: vec![pinned_video(id.to_owned())],
            rtp_sources: vec![stage_rtp_source(id, mime)],
            ..Default::default()
        }))?;
        receiver.command(AgentCommand::Join("stage".into()))?;
        let binding = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(binding) = receiver.rtp_subscription(MediaKind::Video, id) {
                    return anyhow::Ok(binding);
                }
                receiver
                    .turn()
                    .await
                    .map_err(|e| anyhow::anyhow!("{name} receiver setup: {e:?}"))?;
            }
        })
        .await
        .with_context(|| {
            format!(
                "{name} subscription not installed on {}: {:?}",
                viewer_edge.edge,
                receiver.snapshot()
            )
        })??;
        ensure!(receiver.quic_peer() == Some(viewer_edge.edge));
        receiver
            .attach_encoded(
                binding,
                Policy::Freshness {
                    reorder: Duration::from_millis(40),
                    max_age: Duration::from_millis(250),
                },
            )
            .map_err(|e| anyhow::anyhow!("{name} encoded receive setup: {e:?}"))?;

        let mut decoder = NativeDecoder::new(name)?;
        let mut index = 0;
        let mut previous = None;
        while index < PICTURES {
            let event = loop {
                if let Some(event) = receiver.next_encoded(binding) {
                    break event;
                }
                receiver
                    .turn()
                    .await
                    .map_err(|e| anyhow::anyhow!("{name} receive progress: {e:?}"))?;
            };
            let unit = match event {
                Receive::Media(Event::Unit(unit)) => unit,
                // A cold WAN branch can start in the middle of a picture.
                Receive::Media(Event::Discontinuity(gap))
                    if index == 0 && matches!(gap.cause, Cause::MissingBoundary) =>
                {
                    continue;
                }
                event => anyhow::bail!("{name} unit {index} missing: {event:?}"),
            };
            ensure!(
                unit.source == binding && unit.codec == codec,
                "incorrect {name} binding or codec"
            );
            ensure!(
                matches!(unit.dependency, Dependency::Absent),
                "unexpected DD on {name} whole stream"
            );
            // The subscription can attach between capture epochs. Begin with
            // the next complete first picture and require consecutive units.
            if index == 0 && unit.data != expected[0].1 {
                continue;
            }
            ensure!(
                (unit.access, &unit.data) == (expected[index].0, &expected[index].1),
                "{name} unit {index} changed in transit"
            );
            let [a, b, c, d, ..] = unit
                .rtp_time
                .with_context(|| format!("{name} unit missing RTP time"))?
                .to_le_bytes();
            let timestamp = u32::from_le_bytes([a, b, c, d]);
            ensure!(
                previous.is_none_or(|last| timestamp.wrapping_sub(last) == 3000),
                "{name} unit {index} has a nonconsecutive RTP timestamp"
            );
            previous = Some(timestamp);
            ensure!(
                decoder
                    .decode(timestamp, unit.access, &unit.data)
                    .with_context(|| format!("{name} decode failed at unit {index}"))?
                    == (1152, 720)
            );
            index += 1;
        }
        Ok(())
    };
    timeout(Duration::from_secs(18), async {
        pulsebeam_runtime::select! { result = send => result, result = receive => result }
    })
    .await
    .with_context(|| format!("remote {name} encoded receive timed out"))?
}

pub(super) struct NativeDecoder {
    decoder: OwnedVideoDecoder,
    decoded: std::sync::mpsc::Receiver<(u32, i32, i32)>,
}

impl NativeDecoder {
    pub(super) fn new(name: &str) -> Result<Self> {
        let factory = VideoDecoderFactoryHandle::builtin()?;
        let environment = Environment::builder().build()?;
        let (tx, decoded) = std::sync::mpsc::channel();
        let callback = DecodedImageCallbackSink::new(DecodedFrame(tx));
        let mut decoder = factory.create_decoder(&environment, &SdpVideoFormat::new(name))?;
        let mut settings = OwnedVideoDecoderSettings::default();
        if name == "H264" {
            let mut fields = settings.fields();
            // The SDK's H264 decoder rejects the default generic codec type.
            fields.codec_type = 4; // webrtc::kVideoCodecH264
            fields.number_of_cores = 1;
            fields.max_render_width = 1152;
            fields.max_render_height = 720;
            settings.set_fields(fields)?;
        }
        ensure!(
            decoder.configure(&settings),
            "native {name} decoder rejected {settings:?}"
        );
        ensure!(decoder.register_callback(Some(&callback)) == 0);
        Ok(Self { decoder, decoded })
    }

    pub(super) fn decode(
        &mut self,
        timestamp: u32,
        access: Access,
        data: &[u8],
    ) -> Result<(i32, i32)> {
        let mut image = EncodedImage::default();
        image.set_buffer(&EncodedVideoBuffer::from_bytes(data));
        let mut metadata = image.metadata();
        metadata.rtp_timestamp = timestamp;
        metadata.frame_type = if access == Access::IndependentVideo {
            EncodedImageFrameType::Key
        } else {
            EncodedImageFrameType::Delta
        };
        image.set_metadata(&metadata)?;
        ensure!(self.decoder.decode(&image, 0)? == 0, "native decode failed");
        let (decoded_at, width, height) =
            self.decoded.try_recv().context("missing decoded frame")?;
        ensure!(
            decoded_at == timestamp,
            "decoder did not return the supplied picture"
        );
        Ok((width, height))
    }
}

struct DecodedFrame(std::sync::mpsc::Sender<(u32, i32, i32)>);

impl DecodedImageCallbackProvider for DecodedFrame {
    fn decoded(&self, frame: &mut RawVideoFrame) -> i32 {
        let _ = self.0.send((
            frame.metadata().rtp_timestamp,
            frame.width(),
            frame.height(),
        ));
        0
    }
}
