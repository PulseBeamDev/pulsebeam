//! Materializes the SFU's current normalized packet values. This adapter is
//! private to the SFU; Link treats its bytes as opaque. The RTP-native #275
//! profile is a separate migration, not an optional-DD mode of this encoding.

use std::num::NonZeroU32;

use pulsebeam_core::dd::{
    self, DecodeTargetIndication, DependencyDescriptor, FrameDependencyTemplate, RenderResolution,
    TemplateDependencyStructure,
};
use pulsebeam_link::{CodecError, WireCodec};
use pulsebeam_runtime::time::Instant;

use crate::entity::{AudioTrackId, DataTrackId, TrackId, VideoTrackId};
use crate::participant::{RoutedTrackPacket, TrackPacket};
use crate::route::{MediaDelivery, NodeRouteAddress, RouteId};
use crate::rtp::{Frequency, MediaTime, RtpExtensions, RtpPacket};
use crate::shard::worker::ShardFrame;
use crate::track::DataLane;

pub(crate) struct ShardCodec {
    pub origin: Instant,
}

impl WireCodec<ShardFrame> for ShardCodec {
    fn encoded_len(&self, packet: &ShardFrame) -> Result<usize, CodecError> {
        let mut writer = Writer {
            dst: None,
            offset: 0,
        };
        encode(packet, &mut writer)?;
        Ok(writer.offset)
    }

    fn encode(&self, packet: &ShardFrame, dst: &mut [u8]) -> Result<usize, CodecError> {
        let mut writer = Writer {
            dst: Some(dst),
            offset: 0,
        };
        encode(packet, &mut writer)?;
        Ok(writer.offset)
    }

    fn decode(&self, wire: &[u8]) -> Result<ShardFrame, CodecError> {
        if wire.len() > 1200 {
            return Err(CodecError::TooLarge);
        }
        let mut reader = Reader(wire);
        if reader.u8()? != 1 {
            return Err(CodecError::Malformed);
        }
        // Destination addressing comes exclusively from the authenticated Link
        // binding's DeliveryTag, not sender-supplied application bytes.
        let address = NodeRouteAddress::new(RouteId::from_raw(0), 0);
        let frame = match reader.u8()? {
            0 => {
                let packet = crate::participant::reverse::ReversePacket::from_wire(reader.blob()?)
                    .ok_or(CodecError::Malformed)?;
                ShardFrame::Reverse { address, packet }
            }
            kind @ (1 | 2) => {
                let track_id = reader.track_id()?;
                let playout_ntp32 = reader.u32()?;
                let packet = if kind == 1 {
                    if !matches!(track_id, TrackId::Data(_)) {
                        return Err(CodecError::Malformed);
                    }
                    let lane = match reader.u8()? {
                        0 => DataLane::Latest,
                        1 => DataLane::Ordered,
                        2 => DataLane::Lossy,
                        _ => return Err(CodecError::Malformed),
                    };
                    TrackPacket::data(lane, reader.blob()?)
                } else {
                    if matches!(track_id, TrackId::Data(_)) {
                        return Err(CodecError::Malformed);
                    }
                    TrackPacket::Rtp(reader.rtp(self.origin)?)
                };
                ShardFrame::Media {
                    delivery: MediaDelivery {
                        address,
                        playout_ntp32,
                    },
                    payload: RoutedTrackPacket { track_id, packet },
                }
            }
            _ => return Err(CodecError::Malformed),
        };
        if !reader.0.is_empty() {
            return Err(CodecError::Malformed);
        }
        Ok(frame)
    }
}

fn valid_structure(structure: &TemplateDependencyStructure) -> bool {
    let targets = usize::from(structure.decode_target_count);
    let chains = usize::from(structure.chain_count);
    structure.template_id_offset < 64
        && (1..=dd::MAX_DECODE_TARGETS).contains(&targets)
        && chains <= targets
        && !structure.templates.is_empty()
        && structure.templates.iter().all(|template| {
            usize::from(template.spatial_id) < dd::MAX_SPATIAL_IDS
                && usize::from(template.temporal_id) < dd::MAX_TEMPORAL_IDS
                && template.dtis.len() == targets
                && template.chain_diffs.len() == chains
        })
        && (if chains == 0 {
            structure.decode_target_protected_by.is_empty()
        } else {
            structure.decode_target_protected_by.len() == targets
                && structure
                    .decode_target_protected_by
                    .iter()
                    .all(|chain| usize::from(*chain) < chains)
        })
        && (structure.resolutions.is_empty()
            || (structure.resolutions.len() == structure.spatial_layer_count()
                && structure
                    .resolutions
                    .iter()
                    .all(|resolution| resolution.width != 0 && resolution.height != 0)))
}

fn encode(frame: &ShardFrame, writer: &mut Writer<'_>) -> Result<(), CodecError> {
    writer.u8(1)?;
    match frame {
        ShardFrame::Ingress { .. } => Err(CodecError::Unsupported),
        ShardFrame::Reverse { packet, .. } => {
            writer.u8(0)?;
            writer.blob(packet.wire())
        }
        ShardFrame::Media { delivery, payload } => {
            writer.u8(match &payload.packet {
                TrackPacket::Data { .. } => 1,
                TrackPacket::Rtp(_) => 2,
            })?;
            writer.u8(match payload.track_id {
                TrackId::Audio(_) => 0,
                TrackId::Video(_) => 1,
                TrackId::Data(_) => 2,
            })?;
            writer.bytes(payload.track_id.as_bytes())?;
            writer.u32(delivery.playout_ntp32)?;
            match &payload.packet {
                TrackPacket::Data { lane, bytes } => {
                    if !matches!(payload.track_id, TrackId::Data(_)) {
                        return Err(CodecError::Unsupported);
                    }
                    writer.u8(match lane {
                        DataLane::Latest => 0,
                        DataLane::Ordered => 1,
                        DataLane::Lossy => 2,
                    })?;
                    writer.blob(bytes)
                }
                TrackPacket::Rtp(packet) => {
                    if matches!(payload.track_id, TrackId::Data(_)) {
                        return Err(CodecError::Unsupported);
                    }
                    writer.rtp(packet)
                }
            }
        }
    }
}

struct Writer<'a> {
    dst: Option<&'a mut [u8]>,
    offset: usize,
}
impl Writer<'_> {
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        let end = self
            .offset
            .checked_add(bytes.len())
            .filter(|end| *end <= 1200)
            .ok_or(CodecError::TooLarge)?;
        if let Some(dst) = &mut self.dst {
            dst.get_mut(self.offset..end)
                .ok_or(CodecError::TooLarge)?
                .copy_from_slice(bytes);
        }
        self.offset = end;
        Ok(())
    }
    fn u8(&mut self, value: u8) -> Result<(), CodecError> {
        self.bytes(&[value])
    }
    fn u16(&mut self, value: u16) -> Result<(), CodecError> {
        self.bytes(&value.to_be_bytes())
    }
    fn u32(&mut self, value: u32) -> Result<(), CodecError> {
        self.bytes(&value.to_be_bytes())
    }
    fn u64(&mut self, value: u64) -> Result<(), CodecError> {
        self.bytes(&value.to_be_bytes())
    }
    fn count(&mut self, count: usize) -> Result<(), CodecError> {
        self.u8(u8::try_from(count).map_err(|_| CodecError::TooLarge)?)
    }
    fn blob(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        self.u16(u16::try_from(bytes.len()).map_err(|_| CodecError::TooLarge)?)?;
        self.bytes(bytes)
    }
    fn option<T>(
        &mut self,
        value: Option<&T>,
        write: impl FnOnce(&mut Self, &T) -> Result<(), CodecError>,
    ) -> Result<(), CodecError> {
        self.u8(u8::from(value.is_some()))?;
        if let Some(value) = value {
            write(self, value)?;
        }
        Ok(())
    }
    fn resolution(&mut self, resolution: &RenderResolution) -> Result<(), CodecError> {
        self.u16(resolution.width)?;
        self.u16(resolution.height)
    }
    fn template(&mut self, template: &FrameDependencyTemplate) -> Result<(), CodecError> {
        self.u8(template.spatial_id)?;
        self.u8(template.temporal_id)?;
        self.count(template.dtis.len())?;
        for dti in &template.dtis {
            self.u8(u8::try_from(dti.bits()).map_err(|_| CodecError::Malformed)?)?;
        }
        self.count(template.frame_diffs.len())?;
        for diff in &template.frame_diffs {
            self.u16(*diff)?;
        }
        self.count(template.chain_diffs.len())?;
        self.bytes(&template.chain_diffs)
    }
    fn structure(&mut self, structure: &TemplateDependencyStructure) -> Result<(), CodecError> {
        if !valid_structure(structure) {
            return Err(CodecError::Unsupported);
        }
        self.u8(structure.template_id_offset)?;
        self.u8(structure.decode_target_count)?;
        self.u8(structure.chain_count)?;
        self.count(structure.decode_target_protected_by.len())?;
        self.bytes(&structure.decode_target_protected_by)?;
        self.count(structure.templates.len())?;
        for template in &structure.templates {
            self.template(template)?;
        }
        self.count(structure.resolutions.len())?;
        for resolution in &structure.resolutions {
            self.resolution(resolution)?;
        }
        Ok(())
    }
    fn descriptor(&mut self, descriptor: &DependencyDescriptor) -> Result<(), CodecError> {
        self.u8(u8::from(descriptor.start_of_frame)
            | (u8::from(descriptor.end_of_frame) << 1)
            | (u8::from(descriptor.fields.custom_dtis()) << 2)
            | (u8::from(descriptor.fields.custom_fdiffs()) << 3)
            | (u8::from(descriptor.fields.custom_chains()) << 4))?;
        self.u8(descriptor.template_id)?;
        self.u16(descriptor.frame_number)?;
        self.option(descriptor.active_decode_targets.as_ref(), |writer, bits| {
            writer.u32(*bits)
        })?;
        self.template(&descriptor.frame_dependencies)?;
        self.option(descriptor.resolution.as_ref(), Self::resolution)?;
        self.option(descriptor.attached_structure.as_deref(), Self::structure)
    }
    fn rtp(&mut self, packet: &RtpPacket) -> Result<(), CodecError> {
        self.u32(packet.ssrc.into())?;
        self.u64(packet.seq_no.into())?;
        self.u64(packet.rtp_ts.numer())?;
        self.u32(packet.rtp_ts.frequency().get())?;
        self.u8(u8::from(packet.marker)
            | (u8::from(packet.is_keyframe) << 1)
            | (u8::from(packet.is_frame_start) << 2)
            | (u8::from(packet.nal.sps()) << 3)
            | (u8::from(packet.nal.pps()) << 4)
            | (u8::from(packet.nal.idr()) << 5))?;
        self.u32(packet.reported_height)?;
        self.option(packet.ext_vals.rid.as_ref(), |writer, rid| {
            writer.blob(rid.as_bytes())
        })?;
        self.option(packet.ext_vals.audio_level.as_ref(), |writer, level| {
            writer.bytes(&level.to_be_bytes())
        })?;
        self.option(
            packet.ext_vals.raw_dependency_descriptor(),
            |writer, raw| writer.blob(&raw.0),
        )?;
        self.option(packet.ext_vals.dependency_descriptor(), Self::descriptor)?;
        if packet.ext_vals.opaque.len() > 32 {
            return Err(CodecError::TooLarge);
        }
        self.count(packet.ext_vals.opaque.len())?;
        for extension in packet.ext_vals.opaque.iter() {
            if extension.uri.len() > 255 || extension.value.len() > 255 {
                return Err(CodecError::TooLarge);
            }
            self.blob(extension.uri.as_bytes())?;
            self.blob(&extension.value)?;
        }
        self.blob(&packet.payload)
    }
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], CodecError> {
        let (value, rest) = self
            .0
            .split_at_checked(length)
            .ok_or(CodecError::Malformed)?;
        self.0 = rest;
        Ok(value)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        self.take(N)?.try_into().map_err(|_| CodecError::Malformed)
    }
    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(u8::from_be_bytes(self.array()?))
    }
    fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn count(&mut self, max: usize) -> Result<usize, CodecError> {
        let value = usize::from(self.u8()?);
        if value > max {
            Err(CodecError::Malformed)
        } else {
            Ok(value)
        }
    }
    fn blob(&mut self) -> Result<&'a [u8], CodecError> {
        let length = usize::from(self.u16()?);
        self.take(length)
    }
    fn option<T>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Result<T, CodecError>,
    ) -> Result<Option<T>, CodecError> {
        match self.u8()? {
            0 => Ok(None),
            1 => read(self).map(Some),
            _ => Err(CodecError::Malformed),
        }
    }
    fn resolution(&mut self) -> Result<RenderResolution, CodecError> {
        Ok(RenderResolution {
            width: self.u16()?,
            height: self.u16()?,
        })
    }
    fn track_id(&mut self) -> Result<TrackId, CodecError> {
        let kind = self.u8()?;
        let bytes = self.array()?;
        match kind {
            0 => Ok(TrackId::Audio(AudioTrackId::from_bytes(bytes))),
            1 => Ok(TrackId::Video(VideoTrackId::from_bytes(bytes))),
            2 => Ok(TrackId::Data(DataTrackId::from_bytes(bytes))),
            _ => Err(CodecError::Malformed),
        }
    }
    fn template(&mut self) -> Result<FrameDependencyTemplate, CodecError> {
        let mut template = FrameDependencyTemplate {
            spatial_id: self.u8()?,
            temporal_id: self.u8()?,
            ..Default::default()
        };
        if usize::from(template.spatial_id) >= dd::MAX_SPATIAL_IDS
            || usize::from(template.temporal_id) >= dd::MAX_TEMPORAL_IDS
        {
            return Err(CodecError::Malformed);
        }
        for _ in 0..self.count(dd::MAX_DECODE_TARGETS)? {
            let value = self.u8()?;
            if value > 3 {
                return Err(CodecError::Malformed);
            }
            template
                .dtis
                .try_push(DecodeTargetIndication::from_bits(u32::from(value)))
                .map_err(|_| CodecError::Malformed)?;
        }
        for _ in 0..self.count(dd::MAX_FRAME_DIFFS)? {
            template
                .frame_diffs
                .try_push(self.u16()?)
                .map_err(|_| CodecError::Malformed)?;
        }
        for _ in 0..self.count(dd::MAX_CHAINS)? {
            template
                .chain_diffs
                .try_push(self.u8()?)
                .map_err(|_| CodecError::Malformed)?;
        }
        Ok(template)
    }
    fn structure(&mut self) -> Result<TemplateDependencyStructure, CodecError> {
        let mut structure = TemplateDependencyStructure {
            template_id_offset: self.u8()?,
            decode_target_count: self.u8()?,
            chain_count: self.u8()?,
            ..Default::default()
        };
        if structure.template_id_offset >= 64
            || usize::from(structure.decode_target_count) > dd::MAX_DECODE_TARGETS
            || usize::from(structure.chain_count) > dd::MAX_CHAINS
        {
            return Err(CodecError::Malformed);
        }
        for _ in 0..self.count(dd::MAX_DECODE_TARGETS)? {
            structure
                .decode_target_protected_by
                .try_push(self.u8()?)
                .map_err(|_| CodecError::Malformed)?;
        }
        for _ in 0..self.count(dd::MAX_TEMPLATES)? {
            structure
                .templates
                .try_push(self.template()?)
                .map_err(|_| CodecError::Malformed)?;
        }
        for _ in 0..self.count(dd::MAX_SPATIAL_IDS)? {
            structure
                .resolutions
                .try_push(self.resolution()?)
                .map_err(|_| CodecError::Malformed)?;
        }
        if !valid_structure(&structure) {
            return Err(CodecError::Malformed);
        }
        Ok(structure)
    }
    fn descriptor(&mut self) -> Result<DependencyDescriptor, CodecError> {
        let flags = self.u8()?;
        if flags & !31 != 0 {
            return Err(CodecError::Malformed);
        }
        let template_id = self.u8()?;
        if template_id >= 64 {
            return Err(CodecError::Malformed);
        }
        let mut fields = dd::DdFields::empty();
        fields.set_custom_dtis(flags & 4 != 0);
        fields.set_custom_fdiffs(flags & 8 != 0);
        fields.set_custom_chains(flags & 16 != 0);
        Ok(DependencyDescriptor {
            start_of_frame: flags & 1 != 0,
            end_of_frame: flags & 2 != 0,
            template_id,
            frame_number: self.u16()?,
            fields,
            active_decode_targets: self.option(Self::u32)?,
            frame_dependencies: self.template()?,
            resolution: self.option(Self::resolution)?,
            attached_structure: self.option(Self::structure)?.map(Box::new),
        })
    }
    fn rtp(&mut self, origin: Instant) -> Result<RtpPacket, CodecError> {
        let ssrc = self.u32()?.into();
        let seq_no = self.u64()?.into();
        let ticks = self.u64()?;
        let frequency =
            Frequency::from_nonzero(NonZeroU32::new(self.u32()?).ok_or(CodecError::Malformed)?);
        let flags = self.u8()?;
        if flags & !63 != 0 {
            return Err(CodecError::Malformed);
        }
        let reported_height = self.u32()?;
        let rid = self.option(|reader| {
            crate::media::Rid::new(
                std::str::from_utf8(reader.blob()?).map_err(|_| CodecError::Malformed)?,
            )
            .map_err(|_| CodecError::Malformed)
        })?;
        let audio_level = self.option(|reader| Ok(i8::from_be_bytes(reader.array()?)))?;
        if audio_level.is_some_and(|level| level > 0 || level == i8::MIN) {
            return Err(CodecError::Malformed);
        }
        let mut ext_vals = RtpExtensions {
            rid,
            audio_level,
            ..Default::default()
        };
        if let Some(raw) = self.option(|reader| {
            let raw = reader.blob()?;
            if !(dd::MANDATORY_LEN..=dd::MAX_DD_LEN).contains(&raw.len()) {
                return Err(CodecError::Malformed);
            }
            Ok(dd::RawDependencyDescriptor(raw.iter().copied().collect()))
        })? {
            ext_vals.set_raw_descriptor(raw);
        }
        if let Some(descriptor) = self.option(Self::descriptor)? {
            ext_vals.set_descriptor(descriptor);
        }
        let count = self.count(32)?;
        let mut opaque = Vec::with_capacity(count);
        for _ in 0..count {
            let uri = std::str::from_utf8(self.blob()?).map_err(|_| CodecError::Malformed)?;
            let value = self.blob()?;
            if uri.len() > 255 || value.len() > 255 {
                return Err(CodecError::Malformed);
            }
            opaque.push(pulsebeam_rtc::RtpExtension {
                uri: uri.into(),
                value: value.to_vec().into(),
            });
        }
        ext_vals.opaque = opaque.into();
        Ok(RtpPacket {
            ssrc,
            seq_no,
            rtp_ts: MediaTime::new(ticks, frequency),
            marker: flags & 1 != 0,
            is_keyframe: flags & 2 != 0,
            is_frame_start: flags & 4 != 0,
            nal: crate::rtp::h264::NalFlags::from_components(
                flags & 8 != 0,
                flags & 16 != 0,
                flags & 32 != 0,
            ),
            reported_height,
            ext_vals,
            arrival_ts: origin,
            playout_time: origin,
            payload: self.blob()?.into(),
        })
    }
}
