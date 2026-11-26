use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, ensure};
use pulsebeam_agent_core::{AgentCommand, DesiredState, MediaKind, PublicationIntent};
use pulsebeam_agent_quic::{
    executor::{
        host::Agent,
        rtp::{Binding, Publication, Receive, SendPacket, SendPacketError},
    },
    fabric::{Extension, ExtensionKind},
    media::vp9::Descriptor,
};
use pulsebeam_core::dd::DependencyDescriptorReader;
use pulsebeam_runtime::time::{Instant, sleep_until, timeout};
use pulsebeam_testdata::wire_capture::{Cursor, PacketKind, descriptor_rtp};

use super::{
    DatagramAdmission, RootCertStore, pinned_video, sim_placement, sim_quic_agent,
    sim_quic_agent_with_placement, stage_rtp_source,
};

const TRACK: &str = "spatial-camera";
const WIRE: &[u8] = include_bytes!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L3T1/wire.bin");
const INDEX: &str = include_str!("../../../../pulsebeam-testdata/fixtures/vp9-v1/L3T1/wire.tsv");

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

pub(in crate::tests) async fn connect_publisher(
    address: SocketAddr,
    local: std::net::IpAddr,
    edge: SocketAddr,
    roots: RootCertStore,
    publisher_token: &str,
) -> Result<(Agent, Publication)> {
    let mut publisher =
        sim_quic_agent_with_placement(address, local, edge, roots, publisher_token, sim_placement)?;
    publisher.enable_vp9_dependency_descriptors(TRACK)?;
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
    let source = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(source) = publisher.rtp_publication(MediaKind::Video, TRACK) {
                return anyhow::Ok(source);
            }
            publisher
                .turn()
                .await
                .map_err(|e| anyhow::anyhow!("VP9 publisher: {e:?}"))?;
        }
    })
    .await
    .context("VP9 publication not installed")??;
    Ok((publisher, source))
}

pub(in crate::tests) async fn replay(
    publisher: &mut Agent,
    source: &Publication,
    start: Instant,
    repeat: bool,
) -> Result<()> {
    const PICTURES: u16 = 36;
    const LAYERS: u16 = 3;
    let mut next = None;
    let mut next_packet_at = start;
    for cycle in 0_u16.. {
        let mut captured_next = None;
        let mut capture =
            Cursor::new(WIRE, INDEX).map_err(|_| anyhow::anyhow!("VP9 capture index"))?;
        while let Some(record) = capture.next_due(Duration::MAX) {
            if record.kind != PacketKind::Rtp {
                continue;
            }
            // Capture timestamps group whole packet bursts. Pace packets within
            // each burst so the relay proof does not start with a QUIC queue test.
            let at = (start + Duration::from_millis(1200) * u32::from(cycle) + record.send_after)
                .max(next_packet_at);
            while Instant::now() < at {
                pulsebeam_runtime::select! {
                    () = sleep_until(at) => break,
                    result = publisher.turn() => result.map_err(|e| anyhow::anyhow!("VP9 pacing: {e:?}"))?,
                }
            }
            let rtp = descriptor_rtp(record.bytes).context("VP9 capture RTP/DD")?;
            let sequence = next.unwrap_or_else(|| u32::from(rtp.sequence));
            ensure!(
                captured_next.is_none_or(|expected| expected == rtp.sequence),
                "capture sequence discontinuity"
            );
            captured_next = Some(rtp.sequence.wrapping_add(1));
            next = Some(sequence.wrapping_add(1));
            let mut dd = rtp.descriptor.to_vec();
            let mut payload = rtp.payload.to_vec();
            if cycle != 0 {
                let picture_shift = cycle.wrapping_mul(PICTURES);
                let frame = u16::from_be_bytes([dd[1], dd[2]]);
                dd[1..3].copy_from_slice(
                    &frame
                        .wrapping_add(picture_shift.wrapping_mul(LAYERS))
                        .to_be_bytes(),
                );
                shift_picture_id(&mut payload, picture_shift)?;
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
                            .wrapping_add(u32::from(cycle) * u32::from(PICTURES) * 3000),
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
        *first = first.wrapping_add(shift.to_le_bytes()[0]) & 0x7f;
    }
    if *flags & 0x10 == 0 && *flags & 0x20 != 0 {
        let index = if *first & 0x80 != 0 { 2 } else { 1 };
        let tl0 = rest.get_mut(index).context("short VP9 TL0 index")?;
        *tl0 = tl0.wrapping_add(shift.to_le_bytes()[0]);
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
