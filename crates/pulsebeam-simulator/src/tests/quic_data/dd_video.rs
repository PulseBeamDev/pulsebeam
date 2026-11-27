use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use pulsebeam_agent_core::{AgentCommand, DesiredState, MediaKind, PublicationIntent};
use pulsebeam_agent_quic::{
    executor::{
        host::Agent,
        rtp::{Binding, Publication, Receive, SendPacket, SendPacketError},
        timed::{Policy, Receive as TimedReceive},
    },
    fabric::{Extension, ExtensionKind},
    media::{
        jitter::GapReason,
        units::{Access, Cause, Codec, Dependency, Event, Framing},
        vp9::Descriptor,
    },
};
use pulsebeam_core::dd::DependencyDescriptorReader;
use pulsebeam_runtime::time::{Instant, sleep_until, timeout};
use pulsebeam_testdata::wire_capture::{Cursor, PacketKind, descriptor_rtp};

use super::{
    DatagramAdmission, RootCertStore, pinned_video, sim_placement, sim_quic_agent,
    sim_quic_agent_with_placement, stage_rtp_source,
};

pub(in crate::tests) const TRACK: &str = "spatial-camera";
const CAPTURE_PICTURES: u32 = 36;
const WIRE: &[u8] = include_bytes!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L3T1/wire.bin");
const INDEX: &str = include_str!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L3T1/wire.tsv");
const INDEPENDENT_WIRE: &[u8] =
    include_bytes!("../../../../pulsebeam-testdata/fixtures/vp9-v1/S3T1/wire.bin");
const INDEPENDENT_INDEX: &str =
    include_str!("../../../../pulsebeam-testdata/fixtures/vp9-v1/S3T1/wire.tsv");

#[derive(Clone, Copy)]
pub(in crate::tests) enum Graph {
    InterLayer,
    Independent,
    Temporal,
    TemporalReplacement,
    H264Temporal,
    Vp8Temporal,
    Av1Temporal,
    Av1Spatial,
    Av1KeyPicture,
    Vp9KeyPicture,
}

impl Graph {
    pub(super) fn codec(self) -> Codec {
        match self {
            Self::H264Temporal => Codec::H264,
            Self::Vp8Temporal => Codec::Vp8,
            Self::Av1Temporal | Self::Av1Spatial | Self::Av1KeyPicture => Codec::Av1,
            _ => Codec::Vp9,
        }
    }

    fn capture(self, picture_offset: u32) -> (&'static [u8], &'static str, u16) {
        match self {
            Self::InterLayer => (WIRE, INDEX, 3),
            Self::H264Temporal => (
                include_bytes!("../../../../pulsebeam-testdata/fixtures/h264-v1/L1T3/wire.bin"),
                include_str!("../../../../pulsebeam-testdata/fixtures/h264-v1/L1T3/wire.tsv"),
                1,
            ),
            Self::Vp8Temporal => (
                include_bytes!("../../../../pulsebeam-testdata/fixtures/vp8-v1/L1T3/wire.bin"),
                include_str!("../../../../pulsebeam-testdata/fixtures/vp8-v1/L1T3/wire.tsv"),
                1,
            ),
            Self::Av1Temporal => (
                include_bytes!("../../../../pulsebeam-testdata/fixtures/av1-v1/L1T3/wire.bin"),
                include_str!("../../../../pulsebeam-testdata/fixtures/av1-v1/L1T3/wire.tsv"),
                1,
            ),
            Self::Av1Spatial => (
                include_bytes!("../../../../pulsebeam-testdata/fixtures/av1-v1/L3T1/wire.bin"),
                include_str!("../../../../pulsebeam-testdata/fixtures/av1-v1/L3T1/wire.tsv"),
                3,
            ),
            Self::Av1KeyPicture => (
                include_bytes!("../../../../pulsebeam-testdata/fixtures/av1-v1/L3T1_KEY/wire.bin"),
                include_str!("../../../../pulsebeam-testdata/fixtures/av1-v1/L3T1_KEY/wire.tsv"),
                3,
            ),
            Self::Vp9KeyPicture => (
                include_bytes!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L3T1_KEY/wire.bin"),
                include_str!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L3T1_KEY/wire.tsv"),
                3,
            ),
            Self::Independent => (INDEPENDENT_WIRE, INDEPENDENT_INDEX, 3),
            // Two opening epochs leave time for the cold subscription to attach.
            Self::TemporalReplacement if picture_offset < 2 * CAPTURE_PICTURES => (
                include_bytes!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L1T2/wire.bin"),
                include_str!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L1T2/wire.tsv"),
                1,
            ),
            Self::Temporal | Self::TemporalReplacement => (
                include_bytes!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L1T3/wire.bin"),
                include_str!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L1T3/wire.tsv"),
                1,
            ),
        }
    }
}

pub(super) async fn exchange(
    address: SocketAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    publisher_token: &str,
    receiver_token: &str,
    target: u32,
) -> Result<()> {
    let (mut publisher, source) =
        connect_publisher(address, edge.ip(), edge, roots.clone(), publisher_token).await?;
    let id = source.publication_id();
    let mut receiver = sim_quic_agent(address, edge, roots, receiver_token)?.with_raw_rtp();
    let mut video = pinned_video(id.to_owned());
    video.decode_target = Some(target);
    receiver.command(AgentCommand::ReplaceDesired(DesiredState {
        revision: 1,
        video: vec![video],
        rtp_sources: vec![stage_rtp_source(id, "video/vp9")],
        ..Default::default()
    }))?;
    receiver.command(AgentCommand::Join("stage".into()))?;
    let binding = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(binding) = receiver.rtp_subscription(MediaKind::Video, id) {
                return anyhow::Ok(binding);
            }
            receiver
                .turn()
                .await
                .map_err(|e| anyhow::anyhow!("VP9 receiver: {e:?}"))?;
        }
    })
    .await
    .context("VP9 receiver not installed")??;
    exchange_packets(&mut publisher, &source, &mut receiver, binding, target).await
}

pub(in crate::tests) async fn prove_projected_video(
    graph: Graph,
    local: std::net::IpAddr,
    [source_edge, viewer_edge]: [super::whole_video::Endpoint; 2],
    roots: RootCertStore,
    publisher_token: &str,
    viewer_token: &str,
) -> Result<()> {
    let (mut publisher, source) = connect_dd_publisher(
        source_edge.placement,
        local,
        source_edge.edge,
        roots.clone(),
        publisher_token,
        graph.codec(),
    )
    .await?;
    ensure!(
        publisher.quic_peer() == Some(source_edge.edge),
        "DD publisher used an unexpected edge"
    );
    let mut receiver = sim_quic_agent_with_placement(
        viewer_edge.placement,
        local,
        viewer_edge.edge,
        roots,
        viewer_token,
        sim_placement,
    )?;
    let send = replay_graph(&mut publisher, &source, graph);
    let receive = async {
        prove_dd_cadence(
            &mut receiver,
            source.publication_id(),
            Cadence::Capture(graph),
            (source_edge.edge != viewer_edge.edge).then_some(Duration::from_secs(8)),
        )
        .await?;
        ensure!(
            receiver.quic_peer() == Some(viewer_edge.edge),
            "DD receiver used an unexpected edge"
        );
        Ok(())
    };
    pulsebeam_runtime::select! { result = receive => result, result = send => result }
}

pub(in crate::tests) enum Cadence {
    Capture(Graph),
    Av1StockWhole,
}

struct SpatialPicture {
    timestamp: u32,
    access: Access,
    next_layer: u8,
    data: Vec<u8>,
    vp9_sizes: Vec<u32>,
}

impl SpatialPicture {
    fn finish(mut self) -> (Access, Vec<u8>) {
        if self.vp9_sizes.len() > 1 {
            // The libvpx input is one VP9 superframe, with little-endian layer sizes.
            const SIZE_BYTES: u8 = 4;
            let marker = 0xc0 | ((SIZE_BYTES - 1) << 3) | (self.vp9_sizes.len() as u8 - 1);
            self.data.push(marker);
            for size in self.vp9_sizes {
                self.data.extend_from_slice(&size.to_le_bytes());
            }
            self.data.push(marker);
        }
        (self.access, self.data)
    }
}

pub(in crate::tests) async fn prove_dd_cadence(
    receiver: &mut Agent,
    id: &str,
    cadence: Cadence,
    initial_binding_budget: Option<Duration>,
) -> Result<()> {
    let (codec, dimensions, frame_ticks) = match cadence {
        Cadence::Capture(graph) => (graph.codec(), (1152, 720), 3_000),
        Cadence::Av1StockWhole => (Codec::Av1, (1280, 720), 3_060),
    };
    let replace_structure = matches!(cadence, Cadence::Capture(Graph::TemporalReplacement));
    let key_picture = matches!(
        cadence,
        Cadence::Capture(Graph::Av1KeyPicture | Graph::Vp9KeyPicture)
    );
    let spatial = key_picture || matches!(cadence, Cadence::Capture(Graph::Av1Spatial));
    let (mime, name) = match codec {
        Codec::H264 => ("video/h264", "H264"),
        Codec::Vp8 => ("video/vp8", "VP8"),
        Codec::Av1 => ("video/av1", "AV1"),
        _ => ("video/vp9", "VP9"),
    };
    let mut video = pinned_video(id.to_owned());
    let mut desired = DesiredState {
        rtp_sources: vec![stage_rtp_source(id, mime)],
        ..Default::default()
    };
    let mut decoder = super::whole_video::NativeDecoder::new(name)?;
    let mut binding = None;
    // The three targets keep every fourth, every second, or every picture.
    let whole = matches!(cadence, Cadence::Av1StockWhole);
    let phases: &[_] = if whole {
        &[(0, frame_ticks, Some(0))]
    } else if replace_structure {
        &[(1, 3_000, Some(1)), (1, 6_000, Some(2))]
    } else if spatial {
        &[
            (0, frame_ticks, None),
            (1, frame_ticks, None),
            (2, frame_ticks, None),
            (0, frame_ticks, None),
        ]
    } else {
        &[
            (0, frame_ticks * 4, None),
            (1, frame_ticks * 2, None),
            (2, frame_ticks, None),
            (0, frame_ticks * 4, None),
        ]
    };
    let mut last_decoded = None::<u32>;
    for &(target, ticks, expected_structure) in phases {
        let initial_phase = binding.is_none();
        if initial_phase || !replace_structure {
            video.decode_target = (!whole).then_some(target);
            desired.revision = receiver.snapshot().desired_revision + 1;
            desired.video = vec![video.clone()];
            receiver.command(AgentCommand::ReplaceDesired(desired.clone()))?;
        }
        if initial_phase {
            receiver.command(AgentCommand::Join("stage".into()))?;
            desired.connected = true;
            if let Some(budget) = initial_binding_budget {
                timeout(budget, async {
                    while receiver.rtp_subscription(MediaKind::Video, id).is_none() {
                        receiver
                            .turn()
                            .await
                            .map_err(|e| anyhow::anyhow!("{name} cadence receiver setup: {e:?}"))?;
                    }
                    anyhow::Ok(())
                })
                .await
                .with_context(|| {
                    format!(
                        "{name} initial subscription not installed; state: {:?}",
                        receiver.snapshot()
                    )
                })??;
            }
        }
        let dimensions = if spatial {
            [(288, 180), (576, 360), (1152, 720)][target as usize]
        } else {
            dimensions
        };
        // The AV1 spatial capture has one opening key picture per three-second loop.
        let budget = Duration::from_secs(if spatial && codec == Codec::Av1 { 4 } else { 3 });
        timeout(budget, async {
            let mut previous: Option<u32> = None;
            let mut consecutive = 0;
            let mut awaiting_entry = initial_phase || replace_structure || spatial;
            let mut may_reset = !initial_phase;
            let mut picture: Option<SpatialPicture> = None;
            loop {
                if let Some(current) = receiver.rtp_subscription(MediaKind::Video, id) {
                    if let Some(initial) = binding {
                        ensure!(initial == current, "{name} target change replaced the subscription binding");
                    } else {
                        receiver.attach_encoded(current, Policy::Freshness {
                            reorder: Duration::from_millis(40),
                            max_age: Duration::from_millis(250),
                        }).map_err(|e| anyhow::anyhow!("{name} cadence receive setup: {e:?}"))?;
                        binding = Some(current);
                    }
                    while let Some(event) = receiver.next_encoded(current) {
                        let unit = match event {
                            TimedReceive::Media(Event::Unit(unit)) => unit,
                            // A policy change retires the forwarding branch and may
                            // discard its queued RTP. Resume at a fresh keyframe.
                            TimedReceive::Media(Event::Discontinuity(gap))
                                if may_reset && gap.source == current
                                    && matches!(gap.cause, Cause::Gap(GapReason::Deadline)) =>
                            {
                                may_reset = false;
                                awaiting_entry = true;
                                picture = None;
                                previous = None;
                                consecutive = 0;
                                continue;
                            }
                            event => anyhow::bail!("{name} cadence receive at target {target}: {event:?}"),
                        };
                        ensure!(unit.codec == codec && unit.source == current);
                        let Dependency::Parsed(dd) = &unit.dependency else {
                            anyhow::bail!("{name} cadence unit lacks parsed DD: {:?}", unit.dependency);
                        };
                        let temporal_id = dd.temporal_id();
                        if let Framing::Vp9 { descriptor, .. } = &unit.framing {
                            ensure!(descriptor.layer.context("missing VP9 layer")?.temporal_id == temporal_id);
                        }
                        let timestamp = unit.rtp_time.context("missing media RTP time")? as u32;
                        let (access, data) = if spatial {
                            let (spatial_id, end_of_picture) = match &unit.framing {
                                Framing::Av1 { layer, end_of_picture, .. } => (layer.spatial_id, *end_of_picture),
                                Framing::Vp9 { descriptor, end_of_picture } => (
                                    descriptor.layer.context("missing VP9 spatial framing")?.spatial_id,
                                    *end_of_picture,
                                ),
                                _ => anyhow::bail!("missing {name} spatial framing"),
                            };
                            ensure!(temporal_id == 0 && spatial_id == dd.spatial_id());
                            if picture.is_none() {
                                if awaiting_entry && unit.access != Access::IndependentVideo {
                                    continue;
                                }
                                // L3T1_KEY only depends on lower spatial layers in key pictures.
                                let starts_at = if key_picture && unit.access == Access::DependentVideo {
                                    target as u8
                                } else {
                                    0
                                };
                                ensure!(spatial_id == starts_at, "wrong first layer for {name} target {target}");
                                picture = Some(SpatialPicture {
                                    timestamp,
                                    access: unit.access,
                                    next_layer: starts_at,
                                    data: Vec::new(),
                                    vp9_sizes: Vec::new(),
                                });
                            }
                            let current = picture.as_mut().context("spatial picture assembly")?;
                            ensure!(current.timestamp == timestamp && current.next_layer == spatial_id,
                                "{name} spatial units are not contiguous");
                            if codec == Codec::Vp9 {
                                current.vp9_sizes.push(u32::try_from(unit.data.len())?);
                            }
                            current.data.extend_from_slice(&unit.data);
                            current.next_layer += 1;
                            if !end_of_picture {
                                continue;
                            }
                            let complete = picture.take().context("spatial picture completion")?;
                            if awaiting_entry && u32::from(complete.next_layer) != target + 1 {
                                continue;
                            }
                            ensure!(u32::from(complete.next_layer) == target + 1, "wrong top layer for {name} target {target}");
                            complete.finish()
                        } else {
                            (unit.access, unit.data)
                        };
                        if awaiting_entry {
                            let attached_temporal_id = dd.attached_structure.as_ref().and_then(|structure| {
                                structure.templates.iter().map(|template| template.temporal_id).max()
                            });
                            if access != Access::IndependentVideo
                                || expected_structure.is_some_and(|expected| attached_temporal_id != Some(expected))
                            {
                                continue;
                            }
                            awaiting_entry = false;
                        }
                        ensure!(last_decoded.is_none_or(|last| {
                            let delta = timestamp.wrapping_sub(last);
                            delta != 0 && delta < (1 << 31)
                        }), "{name} decoder clock regressed");
                        ensure!(decoder.decode(timestamp, access, &data)? == dimensions);
                        last_decoded = Some(timestamp);
                        consecutive = if u32::from(temporal_id) <= target
                            && previous.is_some_and(|last| timestamp.wrapping_sub(last) == ticks)
                        {
                            consecutive + 1
                        } else {
                            0
                        };
                        previous = Some(timestamp);
                        if consecutive >= 5 {
                            return Ok(());
                        }
                    }
                }
                receiver.turn().await.map_err(|e| anyhow::anyhow!("{name} cadence receiver: {e:?}"))?;
            }
        }).await.with_context(|| format!("no six consecutive decoded pictures at {ticks} ticks for {name} (target {:?}); state: {:?}", video.decode_target, receiver.snapshot()))??;
    }
    Ok(())
}

pub(in crate::tests) async fn connect_publisher(
    address: SocketAddr,
    local: std::net::IpAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    publisher_token: &str,
) -> Result<(Agent, Publication)> {
    connect_dd_publisher(address, local, edge, roots, publisher_token, Codec::Vp9).await
}

pub(in crate::tests) async fn connect_dd_publisher(
    address: SocketAddr,
    local: std::net::IpAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    publisher_token: &str,
    codec: Codec,
) -> Result<(Agent, Publication)> {
    let mut publisher =
        sim_quic_agent_with_placement(address, local, edge, roots, publisher_token, sim_placement)?;
    match codec {
        Codec::H264 => publisher.enable_h264_dependency_descriptors(TRACK)?,
        Codec::Vp8 => publisher.enable_vp8_dependency_descriptors(TRACK)?,
        Codec::Av1 => publisher.enable_av1_dependency_descriptors(TRACK)?,
        Codec::Vp9 => publisher.enable_vp9_dependency_descriptors(TRACK)?,
        _ => anyhow::bail!("no descriptor replay publisher for {codec:?}"),
    }
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
                .map_err(|e| anyhow::anyhow!("{codec:?} DD publisher: {e:?}"))?;
        }
    })
    .await
    .with_context(|| format!("{codec:?} DD publication not installed"))??;
    Ok((publisher, source))
}

pub(in crate::tests) async fn replay(
    publisher: &mut Agent,
    source: &Publication,
    start: Instant,
    repeat: bool,
) -> Result<()> {
    replay_capture(
        publisher,
        source,
        start,
        repeat,
        Refresh::Periodic,
        Graph::InterLayer,
    )
    .await
}

pub(in crate::tests) async fn replay_graph(
    publisher: &mut Agent,
    source: &Publication,
    graph: Graph,
) -> Result<()> {
    replay_capture(
        publisher,
        source,
        Instant::now(),
        true,
        Refresh::Periodic,
        graph,
    )
    .await
}

pub(in crate::tests) async fn replay_on_feedback(
    publisher: &mut Agent,
    source: &Publication,
) -> Result<()> {
    replay_capture(
        publisher,
        source,
        Instant::now(),
        true,
        Refresh::Wait,
        Graph::InterLayer,
    )
    .await
}

pub(in crate::tests) async fn replay_with_refresh(
    publisher: &mut Agent,
    source: &Publication,
) -> Result<()> {
    replay_capture(
        publisher,
        source,
        Instant::now(),
        true,
        Refresh::Interrupt,
        Graph::InterLayer,
    )
    .await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Refresh {
    Periodic,
    Wait,
    Interrupt,
}

async fn replay_capture(
    publisher: &mut Agent,
    source: &Publication,
    start: Instant,
    repeat: bool,
    refresh: Refresh,
    graph: Graph,
) -> Result<()> {
    let mut next = None;
    let mut next_packet_at = start;
    let mut next_cycle = start;
    let mut picture_offset = 0_u32;
    'stream: loop {
        let cycle_start = if refresh == Refresh::Wait {
            while !publisher.take_h264_keyframe_request(TRACK) {
                publisher
                    .turn()
                    .await
                    .map_err(|e| anyhow::anyhow!("VP9 publisher feedback: {e:?}"))?;
            }
            Instant::now()
        } else {
            next_cycle
        };
        let mut captured_next = None;
        let mut last_picture = None;
        let mut pictures_sent = 0;
        let (wire, index, layers) = graph.capture(picture_offset);
        let mut capture =
            Cursor::new(wire, index).map_err(|_| anyhow::anyhow!("VP9 capture index"))?;
        while let Some(record) = capture.next_due(Duration::MAX) {
            if record.kind != PacketKind::Rtp {
                continue;
            }
            // Capture timestamps group whole packet bursts. Pace packets within
            // each burst so the relay proof does not start with a QUIC queue test.
            let at = (cycle_start + record.send_after).max(next_packet_at);
            while Instant::now() < at {
                pulsebeam_runtime::select! {
                    () = sleep_until(at) => break,
                    result = publisher.turn() => result.map_err(|e| anyhow::anyhow!("VP9 pacing: {e:?}"))?,
                }
            }
            let rtp = descriptor_rtp(record.bytes).context("VP9 capture RTP/DD")?;
            if last_picture != Some(rtp.timestamp) {
                // Finish the current picture before answering a real refresh.
                // Start the next capture at its keyframe without resetting RTP.
                if refresh == Refresh::Interrupt
                    && pictures_sent != 0
                    && publisher.take_h264_keyframe_request(TRACK)
                {
                    picture_offset += pictures_sent;
                    next_cycle = at;
                    continue 'stream;
                }
                last_picture = Some(rtp.timestamp);
                pictures_sent += 1;
            }
            let sequence = next.unwrap_or(u32::from(rtp.sequence));
            ensure!(
                captured_next.is_none_or(|expected| expected == rtp.sequence),
                "capture sequence discontinuity"
            );
            captured_next = Some(rtp.sequence.wrapping_add(1));
            next = Some(sequence.wrapping_add(1));
            let mut dd = rtp.descriptor.to_vec();
            let mut payload = rtp.payload.to_vec();
            if picture_offset != 0 {
                let picture_shift = picture_offset as u16;
                let frame = u16::from_be_bytes([dd[1], dd[2]]);
                dd[1..3].copy_from_slice(
                    &frame
                        .wrapping_add(picture_shift.wrapping_mul(layers))
                        .to_be_bytes(),
                );
                if graph.codec() == Codec::Vp9 {
                    shift_picture_id(&mut payload, picture_shift)?;
                }
            }
            let extensions = [Extension {
                kind: ExtensionKind::DependencyDescriptor,
                value: &dd,
            }];
            loop {
                match publisher.send_rtp(
                    source,
                    SendPacket {
                        sequence,
                        timestamp: rtp
                            .timestamp
                            .wrapping_add(picture_offset.wrapping_mul(3000)),
                        marker: rtp.marker,
                        extensions: &extensions,
                        payload: &payload,
                    },
                ) {
                    Ok(()) => break,
                    Err(SendPacketError::Datagram(
                        DatagramAdmission::ControlTurn | DatagramAdmission::Full,
                    )) => {}
                    Err(e) => anyhow::bail!("VP9 captured packet send: {e:?}"),
                }
                publisher
                    .turn()
                    .await
                    .map_err(|e| anyhow::anyhow!("VP9 send progress: {e:?}"))?;
            }
            next_packet_at = Instant::now() + Duration::from_millis(1);
        }
        if !repeat {
            break;
        }
        ensure!(
            pictures_sent == CAPTURE_PICTURES,
            "unexpected VP9 capture length"
        );
        picture_offset += CAPTURE_PICTURES;
        next_cycle = cycle_start + Duration::from_millis(1200);
    }
    loop {
        publisher
            .turn()
            .await
            .map_err(|e| anyhow::anyhow!("VP9 publisher flush: {e:?}"))?;
    }
}

fn shift_picture_id(payload: &mut [u8], shift: u16) -> Result<()> {
    let [flags, first, rest @ ..] = payload else {
        anyhow::bail!("short VP9 replay payload")
    };
    ensure!(*flags & 0x80 != 0, "VP9 replay requires picture IDs");
    if *first & 0x80 != 0 {
        let second = rest.first_mut().context("short VP9 picture ID")?;
        let [high, low] = (u16::from_be_bytes([*first & 0x7f, *second]).wrapping_add(shift)
            & 0x7fff)
            .to_be_bytes();
        *first = high | 0x80;
        *second = low;
    } else {
        *first = first.wrapping_add(shift as u8) & 0x7f;
    }
    if *flags & 0x10 == 0 && *flags & 0x20 != 0 {
        let index = if *first & 0x80 != 0 { 2 } else { 1 };
        let tl0 = rest.get_mut(index).context("short VP9 TL0 index")?;
        *tl0 = tl0.wrapping_add(shift as u8);
    }
    Ok(())
}

async fn exchange_packets(
    publisher: &mut Agent,
    source: &Publication,
    receiver: &mut Agent,
    binding: Binding,
    target: u32,
) -> Result<()> {
    let start = Instant::now();
    let id = source.publication_id();
    let send = replay(publisher, source, start, false);
    let receive = async {
        let mut dd = DependencyDescriptorReader::new();
        let mut frames = [0_u32; 3];
        let deadline = start + Duration::from_secs(2);
        loop {
            while let Some(event) = receiver.next_rtp() {
                let Receive::Packet(packet) = event else {
                    anyhow::bail!("VP9 raw receive: {event:?}");
                };
                ensure!(packet.binding == binding && packet.publication_id == id);
                let extension = packet
                    .extensions()
                    .find(|ext| ext.kind == ExtensionKind::DependencyDescriptor)
                    .context("missing forwarded DD")?;
                let facts = dd
                    .read(extension.value)
                    .map_err(|e| anyhow::anyhow!("VP9 forwarded DD: {e:?}"))?;
                let (vp9, _) =
                    Descriptor::parse(packet.payload()).context("forwarded VP9 payload")?;
                let spatial = vp9.layer.context("VP9 spatial layer")?.spatial_id;
                ensure!(
                    u32::from(spatial) <= target,
                    "VP9 layer {spatial} leaked above target {target}"
                );
                if facts.end_of_frame && vp9.frame_end {
                    *frames
                        .get_mut(usize::from(spatial))
                        .context("unexpected VP9 layer")? += 1;
                }
            }
            pulsebeam_runtime::select! {
                () = sleep_until(deadline) => break,
                result = receiver.turn() => result.map_err(|e| anyhow::anyhow!("VP9 receiver progress: {e:?}"))?,
            }
        }
        ensure!(
            frames
                .iter()
                .take(target as usize + 1)
                .all(|&count| count >= 20),
            "target {target} did not carry the captured spatial chain: {frames:?}"
        );
        anyhow::Ok(())
    };
    pulsebeam_runtime::select! { result = receive => result, result = send => result }
}
