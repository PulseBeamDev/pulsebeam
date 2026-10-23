use crate::media::Rid;
use pulsebeam_core::dd::{DependencyDescriptor, RawDependencyDescriptor};
use pulsebeam_core::vla::VideoLayersAllocation;
#[allow(
    clippy::disallowed_types,
    reason = "immutable extension snapshots stay shard-local; transit copies storage"
)]
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::disallowed_types,
    reason = "immutable extension snapshots stay shard-local; transit copies storage"
)]
pub struct RtpExtensions {
    pub rid: Option<Rid>,
    pub audio_level: Option<i8>,
    pub raw_dependency_descriptor: Option<Arc<RawDependencyDescriptor>>,
    pub dependency_descriptor: Option<Arc<DependencyDescriptor>>,
    pub video_layers_allocation: Option<Arc<VideoLayersAllocation>>,
    pub opaque: Vec<pulsebeam_rtc::RtpExtension>,
}

impl RtpExtensions {
    /// Serialize the receiver descriptor, preserving the projected counter when
    /// source template context requires falling back to the original bytes.
    pub fn write_dependency_descriptor(
        &self,
        writer: &mut pulsebeam_core::dd::DependencyDescriptorWriter,
        buf: &mut [u8],
    ) -> Option<usize> {
        if let Some(dd) = self.dependency_descriptor()
            && let Ok(len) = writer.write(dd, buf)
        {
            return Some(len);
        }
        let raw = self.raw_dependency_descriptor()?;
        let dst = buf.get_mut(..raw.0.len())?;
        dst.copy_from_slice(&raw.0);
        if let Some(dd) = self.dependency_descriptor()
            && let Some(counter) = dst.get_mut(1..pulsebeam_core::dd::MANDATORY_LEN)
        {
            counter.copy_from_slice(&dd.frame_number.to_be_bytes());
        }
        Some(dst.len())
    }

    pub fn raw_dependency_descriptor(&self) -> Option<&RawDependencyDescriptor> {
        self.raw_dependency_descriptor.as_deref()
    }
    pub fn dependency_descriptor(&self) -> Option<&DependencyDescriptor> {
        self.dependency_descriptor.as_deref()
    }
    pub fn video_layers_allocation(&self) -> Option<&VideoLayersAllocation> {
        self.video_layers_allocation.as_deref()
    }

    #[allow(
        clippy::disallowed_types,
        reason = "immutable shard-local descriptor snapshot"
    )]
    pub fn set_raw_descriptor(&mut self, value: RawDependencyDescriptor) {
        self.raw_dependency_descriptor = Some(Arc::new(value));
    }
    #[allow(
        clippy::disallowed_types,
        reason = "immutable shard-local descriptor snapshot"
    )]
    pub fn set_descriptor(&mut self, value: DependencyDescriptor) {
        self.dependency_descriptor = Some(Arc::new(value));
    }
    #[allow(
        clippy::disallowed_types,
        reason = "immutable shard-local descriptor snapshot"
    )]
    pub fn set_descriptor_arc(&mut self, value: Arc<DependencyDescriptor>) {
        self.dependency_descriptor = Some(value);
    }
    #[allow(
        clippy::disallowed_types,
        reason = "immutable shard-local allocation snapshot"
    )]
    pub fn set_video_layers_allocation(&mut self, value: VideoLayersAllocation) {
        self.video_layers_allocation = Some(Arc::new(value));
    }

    #[allow(
        clippy::disallowed_types,
        reason = "re-anchor immutable snapshots on the receiving shard"
    )]
    pub fn rehome(&mut self) {
        if let Some(dd) = &mut self.dependency_descriptor {
            *dd = Arc::new((**dd).clone());
        }
        if let Some(raw) = &mut self.raw_dependency_descriptor {
            *raw = Arc::new((**raw).clone());
        }
        for extension in &mut self.opaque {
            extension.uri = Arc::from(extension.uri.as_ref());
            extension.value = bytes::Bytes::copy_from_slice(&extension.value);
        }
    }

    pub fn to_transit(&self) -> Self {
        let mut owned = self.clone();
        owned.video_layers_allocation = None;
        owned.rehome();
        owned
    }
}
