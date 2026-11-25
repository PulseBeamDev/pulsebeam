use std::{ops::Range, time::Instant};

use super::{Access, Cause, Dependency, Framing, OrderedPacket, Parts, checked_dependency, vp8};

struct Frame {
    first_sequence: u64,
    timestamp: u32,
    arrived_at: Instant,
    dependency: Dependency,
    dd_number: Option<u16>,
    descriptor: vp8::Descriptor,
    partition_id: u8,
    partitions: Vec<Range<usize>>,
    data: Vec<u8>,
}

pub(super) struct Assembly {
    frame: Option<Frame>,
    limit: usize,
}

impl Assembly {
    pub(super) fn new(limit: usize) -> Self {
        Self { frame: None, limit }
    }

    pub(super) fn lose(&mut self) {
        self.frame = None;
    }

    pub(super) fn push(
        &mut self,
        packet: &OrderedPacket<'_>,
        dependency: Dependency,
    ) -> Result<Option<Parts>, (Cause, u64)> {
        // Take ownership so a rejected fragment cannot leave a partial unit live.
        let previous = self.frame.take();
        let first = previous
            .as_ref()
            .map_or(packet.sequence, |f| f.first_sequence);
        let (descriptor, bytes) =
            vp8::Descriptor::parse(packet.payload).ok_or((Cause::InvalidFraming, first))?;
        let start = descriptor.starts_frame();
        let number = checked_dependency(
            &dependency,
            start,
            packet.marker,
            0,
            descriptor.temporal.map(|layer| layer.id),
        )
        .map_err(|cause| (cause, first))?;
        let mut frame = match previous {
            Some(frame) if !start => frame,
            None if start => Frame {
                first_sequence: packet.sequence,
                timestamp: packet.timestamp,
                arrived_at: packet.arrived_at,
                dependency,
                dd_number: number,
                descriptor,
                partition_id: 0,
                partitions: Vec::new(),
                data: Vec::new(),
            },
            _ => return Err((Cause::MissingBoundary, first)),
        };
        if frame.timestamp != packet.timestamp
            || frame.dd_number.zip(number).is_some_and(|(a, b)| a != b)
            || frame
                .descriptor
                .picture_id
                .zip(descriptor.picture_id)
                .is_some_and(|(a, b)| a != b)
            || (!start
                && descriptor.partition_start
                && descriptor.partition_id <= frame.partition_id)
            || (!descriptor.partition_start && descriptor.partition_id != frame.partition_id)
        {
            return Err((Cause::MissingBoundary, first));
        }
        frame.dd_number = frame.dd_number.or(number);
        let begin = frame.data.len();
        let end = begin
            .checked_add(bytes.len())
            .filter(|&len| len <= self.limit)
            .ok_or((Cause::UnitLimit, first))?;
        frame.data.extend_from_slice(bytes);
        frame.arrived_at = frame.arrived_at.min(packet.arrived_at);
        if descriptor.partition_start {
            frame.partition_id = descriptor.partition_id;
            frame.partitions.push(begin..end);
        } else {
            frame
                .partitions
                .last_mut()
                .ok_or((Cause::MissingBoundary, first))?
                .end = end;
        }
        if !packet.marker {
            self.frame = Some(frame);
            return Ok(None);
        }
        let access = match frame.data.as_slice() {
            [tag, _, _, ..] if tag & 1 != 0 => Access::DependentVideo,
            [_, _, _, 0x9d, 0x01, 0x2a, _, _, _, _, ..] => Access::IndependentVideo,
            _ => return Err((Cause::InvalidFraming, first)),
        };
        Ok(Some(Parts {
            first_sequence: first,
            arrived_at: frame.arrived_at,
            dependency: frame.dependency,
            framing: Framing::Vp8 {
                descriptor: frame.descriptor,
                partitions: frame.partitions,
            },
            access,
            data: frame.data,
        }))
    }
}
