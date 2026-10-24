#![cfg(feature = "native")]
//! Owning SDK frame data. Captured buffers are read-only because an SDK shared
//! frame may alias its capturer's cache. SDK BasicDesktopFrame copies are mutable.
use crate::{PeerError, PeerErrorKind, ffi};
use std::{marker::PhantomData, rc::Rc};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DesktopRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}
impl DesktopRect {
    fn native(self) -> ffi::FfiDesktopRect {
        ffi::FfiDesktopRect {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }
    fn from_native(r: ffi::FfiDesktopRect) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DesktopFrameMetadata {
    pub top_left: (i32, i32),
    pub dpi: (i32, i32),
    pub device_scale_factor: Option<f32>,
    pub may_contain_cursor: bool,
    pub capture_time_ms: i64,
    pub capturer_id: u32,
    pub icc_profile: Vec<u8>,
    pub updated_region: Vec<DesktopRect>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopSharedMemoryInfo {
    pub size: usize,
    pub id: i32,
    pub has_handle: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub struct DesktopFrameInfo {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub pixel_format: u32,
    pub scale_factor: f32,
    /// None if the SDK's signed rectangle arithmetic would overflow.
    pub rect: Option<DesktopRect>,
    pub metadata: DesktopFrameMetadata,
    pub has_cpu_data: bool,
    pub has_texture: bool,
    pub shared_memory: Option<DesktopSharedMemoryInfo>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct DesktopFrameSnapshot {
    pub info: DesktopFrameInfo,
    /// Owned rows packed at width * 4 bytes, not the original stride. None for
    /// texture-only frames. Pixel format and original stride remain in info.
    pub packed_pixels: Option<Vec<u8>>,
}
/// A local, independently owned SDK BasicDesktopFrame. No factory/actor graph
/// is borrowed. Pixel and metadata mutation require exclusive access.
pub struct DesktopFrame {
    native: cxx::UniquePtr<ffi::NativeDesktopFrame>,
    _local: PhantomData<Rc<()>>,
}
/// Actual SDK capture output, including its original stride and native buffer
/// lifetime. No mutable dereference is provided. Use try_clone for an SDK CPU
/// copy; snapshots can cross threads while local SDK handles cannot.
pub struct CapturedDesktopFrame(DesktopFrame);
impl std::ops::Deref for CapturedDesktopFrame {
    type Target = DesktopFrame;
    fn deref(&self) -> &DesktopFrame {
        &self.0
    }
}
impl CapturedDesktopFrame {
    /// Metadata belongs to this owning frame wrapper, not its shared pixels.
    pub fn set_metadata(&mut self, metadata: &DesktopFrameMetadata) -> Result<(), PeerError> {
        self.0.set_metadata(metadata)
    }
    pub fn copy_metadata_from(&mut self, source: &DesktopFrame) -> Result<(), PeerError> {
        self.0.copy_metadata_from(source)
    }
    pub fn move_metadata_to(&mut self, destination: &mut DesktopFrame) -> Result<(), PeerError> {
        destination.move_metadata_from(&mut self.0)
    }
    pub(crate) fn new(native: cxx::UniquePtr<ffi::NativeDesktopFrame>) -> Self {
        Self(DesktopFrame::wrap(native))
    }
}
impl DesktopFrame {
    pub const ARGB: u32 = u32::from_le_bytes(*b"ARGB");
    pub const ABGR: u32 = u32::from_le_bytes(*b"ABGR");
    pub const BGRA: u32 = u32::from_le_bytes(*b"BGRA");
    pub const RGBA: u32 = u32::from_le_bytes(*b"RGBA");
    fn wrap(native: cxx::UniquePtr<ffi::NativeDesktopFrame>) -> Self {
        Self {
            native,
            _local: PhantomData,
        }
    }
    fn value(&self) -> &ffi::NativeDesktopFrame {
        self.native.as_ref().expect("owned SDK desktop frame")
    }
    pub fn new(width: i32, height: i32, pixel_format: u32) -> Result<Self, PeerError> {
        if width < 0
            || height < 0
            || width > i32::MAX / 4
            || (height != 0 && width > i32::MAX / height / 4)
            || ![Self::ARGB, Self::ABGR, Self::BGRA, Self::RGBA].contains(&pixel_format)
        {
            return Err(error(
                PeerErrorKind::InvalidParameter,
                "invalid SDK desktop dimensions or four-byte format",
            ));
        }
        let native = ffi::desktop_frame_new(width, height, pixel_format);
        if native.is_null() {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "native SDK desktop frames unavailable",
            ));
        }
        Ok(Self::wrap(native))
    }
    /// Preferred SDK SharedMemoryDesktopFrame constructor. Consumes the buffer
    /// on success or error; arbitrary pointers and undersized layouts are refused.
    /// Covers the deprecated ARGB/raw-pointer constructors with owning inputs.
    pub fn from_shared_memory(
        width: i32,
        height: i32,
        stride: i32,
        pixel_format: u32,
        memory: crate::DesktopMemory,
    ) -> Result<Self, PeerError> {
        Self::from_shared_memory_at(width, height, stride, pixel_format, 0, memory)
    }
    /// Owning row-zero view of the allocation, including bottom-up negative
    /// strides. SDK SharedMemory data/size describe the rebased forward view;
    /// the underlying allocation and dependency leases retain the entire buffer.
    pub fn from_shared_memory_at(
        width: i32,
        height: i32,
        stride: i32,
        pixel_format: u32,
        data_offset: usize,
        memory: crate::DesktopMemory,
    ) -> Result<Self, PeerError> {
        let mut status = 0;
        let native = ffi::desktop_frame_shared(
            width,
            height,
            stride,
            pixel_format,
            data_offset,
            memory.native,
            crate::desktop_memory::RustDesktopMemoryFactory::new(None),
            false,
            &mut status,
        );
        check(status)?;
        if native.is_null() {
            return Err(error(
                PeerErrorKind::InvalidState,
                "SDK shared frame creation failed",
            ));
        }
        Ok(Self::wrap(native))
    }
    /// Actual SDK SharedMemoryDesktopFrame::Create with its tightly packed
    /// stride. None is native allocation rejection, including provider panic or
    /// an undersized result. No BasicDesktopFrame fallback is selected.
    pub fn from_shared_memory_factory(
        width: i32,
        height: i32,
        pixel_format: u32,
        factory: &crate::DesktopMemoryFactoryHandle,
    ) -> Result<Option<Self>, PeerError> {
        let mut status = 0;
        let native = ffi::desktop_frame_shared(
            width,
            height,
            0,
            pixel_format,
            0,
            cxx::UniquePtr::null(),
            crate::desktop_memory::RustDesktopMemoryFactory::new(Some(factory)),
            true,
            &mut status,
        );
        if status == 1 {
            return Ok(None);
        }
        check(status)?;
        Ok((!native.is_null()).then(|| Self::wrap(native)))
    }
    /// Duplicate the actual SDK buffer handle while this frame owns it. None
    /// means no shared memory or SDK kInvalidHandle, not a fabricated descriptor.
    /// Unknown backend-specific handle semantics return Unsupported rather than
    /// guessing that every SDK integer handle is a file descriptor. This clones
    /// the file handle, not a borrowed pixel pointer or a rebased mapping.
    #[cfg(unix)]
    pub fn try_clone_shared_memory_fd(&self) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
        crate::desktop_memory::owned_fd(ffi::desktop_frame_fd(self.value()))
    }
    /// SDK BasicDesktopFrame::CopyOf, including pixel format and frame info.
    /// Texture-only or unrepresentable CPU layouts are not guessed or converted.
    pub fn try_clone(&self) -> Result<Self, PeerError> {
        let native = ffi::desktop_frame_copy(self.value());
        if native.is_null() {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "SDK frame has no safely copyable CPU buffer",
            ));
        }
        Ok(Self::wrap(native))
    }
    pub fn info(&self) -> DesktopFrameInfo {
        let f = ffi::desktop_frame_info(self.value());
        DesktopFrameInfo {
            width: f.width,
            height: f.height,
            stride: f.stride,
            pixel_format: f.pixel_format,
            scale_factor: f.scale_factor,
            rect: f.rect_valid.then(|| DesktopRect::from_native(f.rect)),
            metadata: DesktopFrameMetadata {
                top_left: (f.left, f.top),
                dpi: (f.dpi_x, f.dpi_y),
                device_scale_factor: f.has_device_scale.then_some(f.device_scale),
                may_contain_cursor: f.may_contain_cursor,
                capture_time_ms: f.capture_time_ms,
                capturer_id: f.capturer_id,
                icc_profile: f.icc_profile,
                updated_region: f
                    .updated_region
                    .into_iter()
                    .map(DesktopRect::from_native)
                    .collect(),
            },
            has_cpu_data: f.has_cpu_data,
            has_texture: f.has_texture,
            shared_memory: f.has_shared_memory.then_some(DesktopSharedMemoryInfo {
                size: f.shared_memory_size,
                id: f.shared_memory_id,
                has_handle: f.shared_memory_has_handle,
            }),
        }
    }
    pub fn set_metadata(&mut self, metadata: &DesktopFrameMetadata) -> Result<(), PeerError> {
        let f = ffi::FfiDesktopFrameInfo {
            left: metadata.top_left.0,
            top: metadata.top_left.1,
            dpi_x: metadata.dpi.0,
            dpi_y: metadata.dpi.1,
            has_device_scale: metadata.device_scale_factor.is_some(),
            device_scale: metadata.device_scale_factor.unwrap_or_default(),
            may_contain_cursor: metadata.may_contain_cursor,
            capture_time_ms: metadata.capture_time_ms,
            capturer_id: metadata.capturer_id,
            icc_profile: metadata.icc_profile.clone(),
            updated_region: metadata.updated_region.iter().map(|r| r.native()).collect(),
            ..Default::default()
        };
        check(ffi::desktop_frame_write_info(self.native.pin_mut(), &f))
    }
    pub fn packed_pixels(&self) -> Result<Vec<u8>, PeerError> {
        let result = ffi::desktop_frame_pixels(self.value(), false, 0, 0);
        check(result.status)?;
        Ok(result.bytes)
    }
    /// Calls SDK GetFrameDataAtPos, copying only the four owned pixel bytes.
    pub fn pixel_at(&self, x: i32, y: i32) -> Result<[u8; 4], PeerError> {
        let result = ffi::desktop_frame_pixels(self.value(), true, x, y);
        check(result.status)?;
        result
            .bytes
            .try_into()
            .map_err(|_| error(PeerErrorKind::Operation, "invalid SDK pixel reply"))
    }
    pub fn snapshot(&self) -> Result<DesktopFrameSnapshot, PeerError> {
        let info = self.info();
        let packed_pixels = if info.has_cpu_data {
            Some(self.packed_pixels()?)
        } else {
            None
        };
        Ok(DesktopFrameSnapshot {
            info,
            packed_pixels,
        })
    }
    pub fn set_data_to_black(&mut self) -> Result<(), PeerError> {
        match ffi::desktop_frame_black(self.native.pin_mut(), true) {
            0 => Ok(()),
            _ => Err(error(
                PeerErrorKind::UnsupportedOperation,
                "SDK black-fill requires packed positive CPU rows",
            )),
        }
    }
    /// The SDK's linear black scan is refused for padded/negative/unaligned data.
    pub fn data_is_black(&self) -> Result<bool, PeerError> {
        match ffi::desktop_frame_is_black(self.value()) {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(error(
                PeerErrorKind::UnsupportedOperation,
                "SDK black scan requires packed aligned CPU rows",
            )),
        }
    }
    pub fn copy_pixels_from_buffer(
        &mut self,
        bytes: &[u8],
        offset: usize,
        stride: i32,
        destination: DesktopRect,
    ) -> Result<(), PeerError> {
        check(ffi::desktop_frame_copy_buffer(
            self.native.pin_mut(),
            bytes,
            offset,
            stride,
            &destination.native(),
        ))
    }
    pub fn copy_pixels_from(
        &mut self,
        source: &DesktopFrame,
        source_position: (i32, i32),
        destination: DesktopRect,
    ) -> Result<(), PeerError> {
        check(ffi::desktop_frame_copy_pixels(
            self.native.pin_mut(),
            source.value(),
            source_position.0,
            source_position.1,
            &destination.native(),
        ))
    }
    pub fn copy_metadata_from(&mut self, source: &DesktopFrame) -> Result<(), PeerError> {
        check(ffi::desktop_frame_copy_metadata(
            self.native.pin_mut(),
            source.value(),
        ))
    }
    /// SDK move semantics swap the updated regions; they do not empty all source metadata.
    pub fn move_metadata_from(&mut self, source: &mut DesktopFrame) -> Result<(), PeerError> {
        check(ffi::desktop_frame_copy_info(
            self.native.pin_mut(),
            source.native.pin_mut(),
            true,
        ))
    }
    pub fn copy_intersecting_pixels_from(
        &mut self,
        source: &DesktopFrame,
        horizontal_scale: f64,
        vertical_scale: f64,
    ) -> Result<bool, PeerError> {
        match ffi::desktop_frame_copy_intersection(
            self.native.pin_mut(),
            source.value(),
            horizontal_scale,
            vertical_scale,
        ) {
            0 => Ok(false),
            1 => Ok(true),
            2 => Err(error(
                PeerErrorKind::InvalidParameter,
                "unsafe SDK intersection geometry or CPU layout",
            )),
            _ => Err(error(
                PeerErrorKind::UnsupportedOperation,
                "native desktop frame operation unavailable",
            )),
        }
    }
}
fn check(status: u8) -> Result<(), PeerError> {
    match status {
        0 => Ok(()),
        2 => Err(error(
            PeerErrorKind::InvalidParameter,
            "SDK desktop frame bounds or format mismatch",
        )),
        _ => Err(error(
            PeerErrorKind::UnsupportedOperation,
            "SDK desktop frame operation or CPU layout unavailable",
        )),
    }
}
fn error(kind: PeerErrorKind, message: &str) -> PeerError {
    PeerError {
        kind,
        message: message.into(),
    }
}
