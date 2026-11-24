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
        let mut bytes = [0; MAX_DD_LEN];
        let len = match (packet.first, facts, &mut self.frame) {
            (true, Some(facts), None) => {
                self.validate_dependencies(facts)
                    .map_err(WriteError::Encode)?;
                let mut descriptor = facts.clone();
                descriptor.start_of_frame = true;
                descriptor.end_of_frame = packet.last;
                let len = self
                    .codec
                    .write(&descriptor, &mut bytes)
                    .map_err(WriteError::Encode)?;
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
                frame.descriptor.end_of_frame = packet.last;
                let len = self
                    .codec
                    .write(&frame.descriptor, &mut bytes)
                    .map_err(WriteError::Encode)?;
                frame.next_sequence = packet.sequence.wrapping_add(1);
                if packet.last {
                    self.frame = None;
                }
                len
            }
            _ => return Err(WriteError::FramePosition),
        };
        Ok(Encoded { bytes, len })
    }

    fn validate_dependencies(&self, facts: &DependencyDescriptor) -> Result<(), DdWriteError> {
        let structure = facts
            .attached_structure
            .as_deref()
            .or_else(|| self.codec.structure())
            .ok_or(DdWriteError::NoStructure)?;
        let template = structure
            .template(facts.template_id)
            .ok_or(DdWriteError::UnknownTemplateId(facts.template_id))?;
        let dependencies = &facts.frame_dependencies;
        if dependencies.spatial_id != template.spatial_id
            || dependencies.temporal_id != template.temporal_id
            || (!facts.fields.custom_dtis() && dependencies.dtis != template.dtis)
            || (!facts.fields.custom_fdiffs() && dependencies.frame_diffs != template.frame_diffs)
            || (!facts.fields.custom_chains() && dependencies.chain_diffs != template.chain_diffs)
        {
            return Err(DdWriteError::Inconsistent);
        }
        Ok(())
    }
}
