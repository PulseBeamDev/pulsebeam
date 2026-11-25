use std::time::Instant;

use super::{Access, Cause, Dependency, Framing, OrderedPacket, Parts, checked_dependency, vp9};

struct Frame {
    first_sequence: u64,
    timestamp: u32,
    arrived_at: Instant,
    dependency: Dependency,
    dd_number: Option<u16>,
    descriptor: vp9::Descriptor,
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
        let previous = self.frame.take();
        let first = previous
            .as_ref()
            .map_or(packet.sequence, |f| f.first_sequence);
        let (descriptor, bytes) =
            vp9::Descriptor::parse(packet.payload).ok_or((Cause::InvalidFraming, first))?;
        if packet.marker && !descriptor.frame_end {
            return Err((Cause::MissingBoundary, first));
        }
        let number = checked_dependency(
            &dependency,
            descriptor.frame_start,
            descriptor.frame_end,
            descriptor.layer.map_or(0, |layer| layer.spatial_id),
            descriptor.layer.map(|layer| layer.temporal_id),
        )
        .map_err(|cause| (cause, first))?;
        let frame_end = descriptor.frame_end;
        let mut frame = match previous {
            None if descriptor.frame_start => Frame {
                first_sequence: packet.sequence,
                timestamp: packet.timestamp,
                arrived_at: packet.arrived_at,
                dependency,
                dd_number: number,
                descriptor,
                data: Vec::new(),
            },
            Some(frame) if !descriptor.frame_start => {
                let initial = &frame.descriptor;
                if frame.timestamp != packet.timestamp
                    || frame.dd_number.zip(number).is_some_and(|(a, b)| a != b)
                    || initial.picture_id != descriptor.picture_id
                    || initial.predicted != descriptor.predicted
                    || initial.layer != descriptor.layer
                    || initial.references != descriptor.references
                {
                    return Err((Cause::MissingBoundary, first));
                }
                frame
            }
            _ => return Err((Cause::MissingBoundary, first)),
        };
        if bytes.len() > self.limit.saturating_sub(frame.data.len()) {
            return Err((Cause::UnitLimit, first));
        }
        frame.data.extend_from_slice(bytes);
        frame.dd_number = frame.dd_number.or(number);
        frame.arrived_at = frame.arrived_at.min(packet.arrived_at);
        if !frame_end {
            self.frame = Some(frame);
            return Ok(None);
        }
        let access = if frame.descriptor.predicted
            || frame
                .descriptor
                .layer
                .is_some_and(|layer| layer.depends_on_lower_spatial)
        {
            Access::DependentVideo
        } else {
            Access::IndependentVideo
        };
        frame.descriptor.frame_end = true;
        Ok(Some(Parts {
            first_sequence: first,
            arrived_at: frame.arrived_at,
            dependency: frame.dependency,
            framing: Framing::Vp9 {
                descriptor: frame.descriptor,
                end_of_picture: packet.marker,
            },
            access,
            data: frame.data,
        }))
    }
}
