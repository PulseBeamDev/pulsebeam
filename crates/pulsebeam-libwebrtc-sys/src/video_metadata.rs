use std::fmt;

use crate::ffi;

/// An independent owning WebRTC corruption-detection metadata value.
///
/// Setters return the SDK's boolean result and leave its validation semantics
/// unchanged, including accepted NaN values. This value does not install a
/// corruption-detection generator or transmit an RTP header extension.
pub struct FrameInstrumentationData {
    pub(crate) native: cxx::UniquePtr<ffi::NativeFrameInstrumentationData>,
}

// SAFETY: the SDK value contains only scalars and an owned vector. Shared
// methods are read-only; all mutation requires exclusive Rust access. Copy and
// final destruction have no engine, callback, or thread-affinity dependency.
unsafe impl Send for FrameInstrumentationData {}
// SAFETY: see Send; no mutable native reference escapes this wrapper.
unsafe impl Sync for FrameInstrumentationData {}

impl FrameInstrumentationData {
    pub fn new() -> Self {
        Self {
            native: ffi::frame_instrumentation_new(),
        }
    }

    pub(crate) fn native(&self) -> &ffi::NativeFrameInstrumentationData {
        self.native.as_ref().expect("allocated SDK metadata")
    }

    /// Native copy assignment, including an independent copy of every sample.
    pub fn assign_from(&mut self, source: &Self) {
        ffi::frame_instrumentation_assign(self.native.pin_mut(), source.native());
    }

    pub fn sequence_index(&self) -> i32 {
        self.native().sequence_index()
    }

    pub fn set_sequence_index(&mut self, index: i32) -> bool {
        self.native.pin_mut().set_sequence_index(index)
    }

    pub fn is_droppable(&self) -> bool {
        self.native().is_droppable()
    }

    pub fn set_droppable(&mut self, droppable: bool) {
        self.native.pin_mut().set_droppable(droppable);
    }

    pub fn std_dev(&self) -> f64 {
        self.native().std_dev()
    }

    pub fn set_std_dev(&mut self, std_dev: f64) -> bool {
        self.native.pin_mut().set_std_dev(std_dev)
    }

    pub fn luma_error_threshold(&self) -> i32 {
        self.native().luma_error_threshold()
    }

    pub fn set_luma_error_threshold(&mut self, threshold: i32) -> bool {
        self.native.pin_mut().set_luma_error_threshold(threshold)
    }

    pub fn chroma_error_threshold(&self) -> i32 {
        self.native().chroma_error_threshold()
    }

    pub fn set_chroma_error_threshold(&mut self, threshold: i32) -> bool {
        self.native.pin_mut().set_chroma_error_threshold(threshold)
    }

    /// Independent ordered copy of the SDK's sample span.
    pub fn sample_values(&self) -> Vec<f64> {
        ffi::frame_instrumentation_samples(self.native())
    }

    /// Calls the SDK's span overload, which copies accepted samples.
    pub fn set_sample_values(&mut self, samples: &[f64]) -> bool {
        ffi::frame_instrumentation_set_samples(self.native.pin_mut(), samples)
    }

    /// Calls the SDK's owning vector-rvalue overload.
    ///
    /// The input is consumed even if the SDK rejects it. Rejection leaves the
    /// value's previous samples unchanged, just as the native overload does.
    pub fn set_sample_values_owned(&mut self, samples: Vec<f64>) -> bool {
        ffi::frame_instrumentation_set_samples_owned(self.native.pin_mut(), samples)
    }

    pub fn holds_upper_bits(&self) -> bool {
        self.native().holds_upper_bits()
    }

    pub fn is_sync_only(&self) -> bool {
        self.native().is_sync_only()
    }
}

impl Default for FrameInstrumentationData {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for FrameInstrumentationData {
    fn clone(&self) -> Self {
        Self {
            native: ffi::frame_instrumentation_clone(self.native()),
        }
    }
}

impl fmt::Debug for FrameInstrumentationData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameInstrumentationData")
            .field("sequence_index", &self.sequence_index())
            .field("droppable", &self.is_droppable())
            .field("std_dev", &self.std_dev())
            .field("luma_error_threshold", &self.luma_error_threshold())
            .field("chroma_error_threshold", &self.chroma_error_threshold())
            .field("sample_values", &self.sample_values())
            .finish()
    }
}
