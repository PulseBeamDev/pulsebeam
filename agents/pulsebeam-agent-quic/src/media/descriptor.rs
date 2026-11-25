//! Dependency descriptors on logical RTP packets, not carrier fragments.

pub use pulsebeam_core::dd::{
    DdFields, DdReadError, DdWriteError, DecodeTargetIndication, DependencyDescriptor,
    DependencyDescriptorReader as Reader, FrameDependencyTemplate, RenderResolution,
    TemplateDependencyStructure,
};

use pulsebeam_core::{
    dd::{DependencyDescriptorWriter, MAX_DD_LEN},
    fabric::{Extension, ExtensionKind},
};

use super::Packet;

#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The first packet needs frame facts; continuations need an open frame
    /// with the same timestamp and the next source sequence.
    FramePosition,
    MissingReference,
    Encode(DdWriteError),
}

pub struct Encoded {
    bytes: [u8; MAX_DD_LEN],
    len: usize,
}

impl Encoded {
    pub fn extension(&self) -> Extension<'_> {
        Extension {
            kind: ExtensionKind::DependencyDescriptor,
            value: self.bytes.get(..self.len).unwrap_or_default(),
        }
    }
}

/// One descriptor context for one source lifetime. Keep encoded output on
/// transport backpressure. Reset this stage when replacing or abandoning the
/// source; the next frame must then provide a template structure.
#[derive(Default)]
pub struct Writer {
    codec: DependencyDescriptorWriter,
    frame: Option<OpenFrame>,
}

struct OpenFrame {
    timestamp: u32,
    next_sequence: u32,
    descriptor: DependencyDescriptor,
}

impl Writer {
    pub fn reset(&mut self) {
        self.codec.reset();
        self.frame = None;
    }

    /// Supply actual encoder facts on the first packet, including any template
    /// update. Packet boundaries replace the facts' start/end flags. Every
    /// packet repeats the frame number and dependencies; state updates are
    /// carried only by the first packet. This stage does not infer references.
    pub fn write<P>(
        &mut self,
        packet: &Packet<P>,
        facts: Option<&DependencyDescriptor>,
    ) -> Result<Encoded, WriteError> {
        self.write_if(packet, facts, |_| true)?
            .ok_or(WriteError::FramePosition)
    }

    /// A declined offer leaves both the descriptor structure and packet cursor
    /// unchanged. The sink must not enqueue anything when declining.
    pub fn write_if<P>(
        &mut self,
        packet: &Packet<P>,
        facts: Option<&DependencyDescriptor>,
        accept: impl FnOnce(Extension<'_>) -> bool,
    ) -> Result<Option<Encoded>, WriteError> {
        let mut bytes = [0; MAX_DD_LEN];
        let accept_bytes = |value: &[u8]| {
            accept(Extension {
                kind: ExtensionKind::DependencyDescriptor,
                value,
            })
        };
        let len = match (packet.first, facts, &mut self.frame) {
            (true, Some(facts), None) => {
                let mut descriptor = facts.clone();
                descriptor.start_of_frame = true;
                descriptor.end_of_frame = packet.last;
                let Some(len) = self
                    .codec
                    .write_if(&descriptor, &mut bytes, accept_bytes)
                    .map_err(WriteError::Encode)?
                else {
                    return Ok(None);
                };
                if !packet.last {
                    descriptor.start_of_frame = false;
                    descriptor.attached_structure = None;
                    descriptor.active_decode_targets = None;
                    self.frame = Some(OpenFrame {
                        timestamp: packet.timestamp,
                        next_sequence: packet.sequence.wrapping_add(1),
                        descriptor,
                    });
                }
                len
            }
            (false, None, Some(frame))
                if frame.timestamp == packet.timestamp
                    && frame.next_sequence == packet.sequence =>
            {
                let mut descriptor = frame.descriptor.clone();
                descriptor.end_of_frame = packet.last;
                let Some(len) = self
                    .codec
                    .write_if(&descriptor, &mut bytes, accept_bytes)
                    .map_err(WriteError::Encode)?
                else {
                    return Ok(None);
                };
                frame.next_sequence = packet.sequence.wrapping_add(1);
                if packet.last {
                    self.frame = None;
                }
                len
            }
            _ => return Err(WriteError::FramePosition),
        };
        Ok(Some(Encoded { bytes, len }))
    }
}

/// One ordered frame-number space for the lifetime of an encoded publication.
/// Encoder facts describe the packetized spatial frame, not the whole picture.
pub struct Publisher {
    writer: Writer,
    next_frame: u16,
    next_packet: Option<u32>,
    completed_frames: u16,
}

impl Publisher {
    pub fn new(first_frame: u16) -> Self {
        Self {
            writer: Writer::default(),
            next_frame: first_frame,
            next_packet: None,
            completed_frames: 0,
        }
    }

    pub fn next_frame(&self) -> u16 {
        self.next_frame
    }

    /// Offer a packet and its descriptor as one operation. Facts are required
    /// on the first packet only. Frame differences refer to already completed
    /// frames in this publication; frames may share an RTP timestamp.
    pub fn write_if<P>(
        &mut self,
        packet: &Packet<P>,
        facts: Option<&DependencyDescriptor>,
        accept: impl FnOnce(Extension<'_>) -> bool,
    ) -> Result<bool, WriteError> {
        if self.next_packet.is_some_and(|next| next != packet.sequence)
            || (packet.marker && !packet.last)
            || facts.is_some_and(|facts| facts.frame_number != self.next_frame)
        {
            return Err(WriteError::FramePosition);
        }
        if facts.is_some_and(|facts| {
            facts
                .frame_dependencies
                .frame_diffs
                .iter()
                .any(|&diff| diff == 0 || diff > self.completed_frames)
        }) {
            return Err(WriteError::MissingReference);
        }
        if self.writer.write_if(packet, facts, accept)?.is_none() {
            return Ok(false);
        }
        self.next_packet = Some(packet.sequence.wrapping_add(1));
        if packet.last {
            self.next_frame = self.next_frame.wrapping_add(1);
            self.completed_frames = self.completed_frames.saturating_add(1);
        }
        Ok(true)
    }
}
