use crate::clock::WallAnchor;
use crate::keys::TrackKey;
use crate::rtp::{self, PacketDerivedFacts, PacketForwardingState};
use crate::track::DataLane;
use tokio::time::Instant;

#[allow(
    clippy::large_enum_variant,
    reason = "RTP packets stay inline to keep the forwarding hot path allocation-free"
)]
#[derive(Debug)]
pub enum TrackPacket {
    Data {
        lane: DataLane,
        bytes: Vec<u8>,
    },
    Rtp {
        packet: PacketForwardingState,
        encoding: Option<crate::rtp::EncodingId>,
        audio_level_extension: Option<u8>,
        media: ForwardPacket,
    },
}

pub enum TrackPacketRef<'a> {
    Data {
        lane: DataLane,
        bytes: &'a [u8],
    },
    Rtp {
        packet: &'a PacketForwardingState,
        encoding: Option<crate::rtp::EncodingId>,
        audio_level_extension: Option<u8>,
        media: &'a ForwardPacket,
    },
}

#[derive(Debug)]
pub enum ForwardPacket {
    Local(pulsebeam_rtc::MediaPacket),
    Transit(pulsebeam_rtc::TransitMediaPacket),
}

pub(crate) fn derive_packet(
    media: &pulsebeam_rtc::MediaPacket,
    descriptor: &pulsebeam_rtc::EncodedStreamDescriptor,
    wall: &WallAnchor,
) -> Result<PacketForwardingState, pulsebeam_rtc::RtcPeerError> {
    debug_assert_eq!(media.stream(), descriptor.stream());
    let codec = rtp::Codec::from_name(descriptor.codec().name()).unwrap_or(rtp::Codec::H264);
    let semantics = media.semantics(descriptor)?;
    let mut facts = PacketDerivedFacts::default();
    facts.raw_dependency_descriptor = media
        .dependency_descriptor(descriptor)?
        .filter(|value| value.len() <= pulsebeam_core::dd::model::MAX_DD_LEN)
        .map(|value| pulsebeam_core::dd::RawDependencyDescriptor(value.iter().copied().collect()));
    facts.video_layers_allocation =
        media
            .video_layers_allocation(descriptor)?
            .map(|vla| rtp::VideoLayersAllocation {
                current_simulcast_stream_index: vla.current_stream(),
                simulcast_streams: vla
                    .streams()
                    .iter()
                    .map(|stream| rtp::SimulcastStreamAllocation {
                        spatial_layers: stream
                            .spatial_layers()
                            .iter()
                            .map(|spatial| rtp::SpatialLayerAllocation {
                                temporal_layers: spatial
                                    .cumulative_temporal_kbps()
                                    .iter()
                                    .copied()
                                    .map(|cumulative_kbps| rtp::TemporalLayerAllocation {
                                        cumulative_kbps,
                                    })
                                    .collect(),
                                resolution_and_framerate: spatial.resolution().map(
                                    |(width, height, framerate)| rtp::ResolutionAndFramerate {
                                        width,
                                        height,
                                        framerate,
                                    },
                                ),
                            })
                            .collect(),
                    })
                    .collect(),
            });
    let nal = semantics
        .h264()
        .map_or_else(rtp::h264::NalFlags::empty, |metadata| {
            rtp::h264::NalFlags::from_parts(metadata.sps(), metadata.pps(), metadata.idr())
        });
    let received_at = Instant::from_std(media.received_at());
    let playout_time = wall
        .to_instant_system(media.playout_time())
        .map_err(|error| {
            pulsebeam_rtc::RtcPeerError::Transport(format!(
                "media playout projection failed: {error:?}"
            ))
        })?;
    Ok(PacketForwardingState {
        marker: media.marker(),
        derived: facts,
        size_bytes: media.bytes().len(),
        seq_no: media.sequence().get().into(),
        rtp_ts: rtp::MediaTime::new(
            media.timestamp().get(),
            if codec == rtp::Codec::Opus {
                rtp::AUDIO_FREQUENCY
            } else {
                rtp::VIDEO_FREQUENCY
            },
        ),
        arrival_ts: received_at,
        packet_id: media.packet_id(),
        playout_time,
        is_keyframe: semantics.keyframe(),
        is_frame_start: semantics.frame_start(),
        nal,
        #[cfg(test)]
        codec,
        #[cfg(test)]
        ssrc: 0.into(),
        #[cfg(test)]
        rid: descriptor.rid().map(rtp::EncodingId::from),
        #[cfg(test)]
        test_audio_level: None,
        #[cfg(test)]
        payload: media.payload().to_vec(),
    })
}

impl ForwardPacket {
    pub fn packet(&self) -> &pulsebeam_rtc::MediaPacket {
        match self {
            Self::Local(packet) => packet,
            Self::Transit(packet) => packet.packet(),
        }
    }

    pub fn materialize(&self) -> pulsebeam_rtc::TransitMediaPacket {
        pulsebeam_rtc::TransitMediaPacket::materialize(self.packet())
    }
}

impl TrackPacket {
    pub fn as_ref(&self) -> TrackPacketRef<'_> {
        match self {
            Self::Data { lane, bytes } => TrackPacketRef::Data { lane: *lane, bytes },
            Self::Rtp {
                packet,
                encoding,
                audio_level_extension,
                media,
            } => TrackPacketRef::Rtp {
                packet,
                encoding: *encoding,
                audio_level_extension: *audio_level_extension,
                media,
            },
        }
    }
}

#[derive(Debug)]
pub struct RoutedTrackPacket {
    pub key: TrackKey,
    pub packet: TrackPacket,
}

impl RoutedTrackPacket {
    pub fn set_remote_timing(
        &mut self,
        playout: tokio::time::Instant,
        arrival: tokio::time::Instant,
    ) {
        match &mut self.packet {
            TrackPacket::Rtp { packet, .. } => {
                packet.playout_time = playout;
                packet.arrival_ts = arrival;
                packet.validate_derived();
            }
            TrackPacket::Data { .. } => {}
        }
    }

    pub fn into_rtp(self) -> Option<PacketForwardingState> {
        match self.packet {
            TrackPacket::Rtp { packet, .. } => Some(packet),
            TrackPacket::Data { .. } => {
                debug_assert!(false, "a data packet cannot enter the RTP path");
                None
            }
        }
    }
}
