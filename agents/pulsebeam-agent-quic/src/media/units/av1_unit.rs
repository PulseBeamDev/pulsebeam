use std::time::Instant;

use super::{Access, Cause, Dependency, Framing, OrderedPacket, Parts, av1};

struct Frame {
    first_sequence: u64,
    timestamp: u32,
    arrived_at: Instant,
    dependency: Dependency,
    dd_number: Option<u16>,
    dd_layer: Option<av1::Layer>,
    new_sequence: bool,
    obus: av1::Reconstruction,
}

pub(super) struct Assembly {
    frame: Option<Frame>,
    limit: usize,
    whole_source: bool,
    after_marker: bool,
}

impl Assembly {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            frame: None,
            limit,
            whole_source: false,
            after_marker: false,
        }
    }

    pub(super) fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }

    pub(super) fn set_whole_source(&mut self) {
        self.whole_source = true;
    }

    pub(super) fn lose(&mut self) {
        self.frame = None;
        self.after_marker = false;
    }

    pub(super) fn push(
        &mut self,
        packet: &OrderedPacket<'_>,
        dependency: Dependency,
    ) -> Result<Option<Parts>, (Cause, u64)> {
        let previous = self.frame.take();
        let after_marker = std::mem::take(&mut self.after_marker);
        let first = previous
            .as_ref()
            .map_or(packet.sequence, |f| f.first_sequence);
        let (aggregation, _) =
            av1::Aggregation::parse(packet.payload).map_err(|e| (cause(e), first))?;
        let (start, end, number, parsed_layer) = match &dependency {
            Dependency::Parsed(dd) if !self.whole_source => (
                dd.start_of_frame,
                dd.end_of_frame,
                Some(dd.frame_number),
                Some(av1::Layer {
                    temporal_id: dd.temporal_id(),
                    spatial_id: dd.spatial_id(),
                }),
            ),
            Dependency::MissingContext(dd) if !self.whole_source => (
                dd.start_of_frame,
                dd.end_of_frame,
                Some(dd.frame_number),
                None,
            ),
            Dependency::Absent if self.whole_source => {
                if previous.is_none() && !after_marker && !aggregation.new_sequence {
                    self.after_marker = packet.marker && !aggregation.continues;
                    return Err((Cause::MissingBoundary, first));
                }
                (previous.is_none(), packet.marker, None, None)
            }
            Dependency::Absent => return Err((Cause::MissingBoundary, first)),
            _ => return Err((Cause::InvalidFraming, first)),
        };
        if (start && aggregation.continuation)
            || (end && aggregation.continues)
            || (aggregation.new_sequence && !start)
            || (packet.marker && !end)
        {
            return Err((Cause::MissingBoundary, first));
        }
        let mut frame = match previous {
            None if start => Frame {
                first_sequence: packet.sequence,
                timestamp: packet.timestamp,
                arrived_at: packet.arrived_at,
                dependency,
                dd_number: number,
                dd_layer: parsed_layer,
                new_sequence: aggregation.new_sequence,
                obus: av1::Reconstruction::new(self.limit),
            },
            Some(frame)
                if !start
                    && frame.timestamp == packet.timestamp
                    && frame.dd_number.zip(number).is_none_or(|(a, b)| a == b)
                    && frame.dd_layer.zip(parsed_layer).is_none_or(|(a, b)| a == b) =>
            {
                frame
            }
            _ => return Err((Cause::MissingBoundary, first)),
        };
        frame
            .obus
            .push(packet.payload)
            .map_err(|e| (cause(e), first))?;
        frame.arrived_at = frame.arrived_at.min(packet.arrived_at);
        frame.dd_layer = frame.dd_layer.or(parsed_layer);
        if !end {
            self.frame = Some(frame);
            return Ok(None);
        }
        let unit = frame.obus.finish().map_err(|e| (cause(e), first))?;
        let layer = unit.layer.or(frame.dd_layer).unwrap_or(av1::Layer {
            temporal_id: 0,
            spatial_id: 0,
        });
        if frame.dd_layer.is_some_and(|dd| dd != layer)
            || (self.whole_source && (layer.spatial_id != 0 || layer.temporal_id != 0))
        {
            return Err((Cause::InvalidFraming, first));
        }
        self.after_marker = packet.marker;
        let access = if frame.new_sequence && unit.sequence_header.is_some() {
            Access::IndependentVideo
        } else {
            Access::DependentVideo
        };
        Ok(Some(Parts {
            first_sequence: first,
            arrived_at: frame.arrived_at,
            access,
            dependency: frame.dependency,
            framing: Framing::Av1 {
                layer,
                sequence_header: unit.sequence_header,
                new_sequence: frame.new_sequence,
                end_of_picture: packet.marker,
            },
            data: unit.data,
        }))
    }
}

fn cause(error: av1::Error) -> Cause {
    match error {
        av1::Error::Framing => Cause::InvalidFraming,
        av1::Error::Limit => Cause::UnitLimit,
    }
}
