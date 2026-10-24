#![cfg(feature = "native")]
use crate::ffi;
/// Owned scalar copy of the actual GetMetadata result, not a configuration
/// echo. The pinned ScreenCastPortal currently returns empty session details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopCaptureMetadataSnapshot {
    pub session_handle: String,
    pub pipewire_stream_node_id: u32,
    pub has_proxy: bool,
    pub has_cancellable: bool,
}
/// Owns references to every non-null GObject in SDK SessionDetails. No raw
/// platform pointers escape, and these references do not revive a capturer.
pub struct DesktopCaptureMetadata {
    native: cxx::UniquePtr<ffi::NativeDesktopMetadata>,
}
impl DesktopCaptureMetadata {
    pub(crate) fn new(native: cxx::UniquePtr<ffi::NativeDesktopMetadata>) -> Self {
        Self { native }
    }
    fn value(&self) -> &ffi::NativeDesktopMetadata {
        self.native.as_ref().expect("owned SDK metadata")
    }
    pub fn snapshot(&self) -> DesktopCaptureMetadataSnapshot {
        let value = ffi::desktop_metadata_read(self.value());
        DesktopCaptureMetadataSnapshot {
            session_handle: value.session_handle,
            pipewire_stream_node_id: value.pipewire_stream_node_id,
            has_proxy: value.has_proxy,
            has_cancellable: value.has_cancellable,
        }
    }
    pub fn same_proxy(&self, other: &Self) -> bool {
        ffi::desktop_metadata_equal(self.value(), other.value(), false)
    }
    pub fn same_cancellable(&self, other: &Self) -> bool {
        ffi::desktop_metadata_equal(self.value(), other.value(), true)
    }
}
impl Clone for DesktopCaptureMetadata {
    fn clone(&self) -> Self {
        // Existing supported products retain actual SDK GObject refs; cloning
        // does not perform discovery or need their original capture/factory.
        Self::new(ffi::desktop_metadata_clone(self.value()))
    }
}
