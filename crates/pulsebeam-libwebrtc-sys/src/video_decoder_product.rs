//! Caller-sequence owning products returned by the actual selected SDK factory.
use crate::{Environment, OwnedVideoDecoderInfo, SdpVideoFormat, VideoDecoderFactoryHandle, ffi};
use std::{fmt, marker::PhantomData, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoDecoderCreationError {
    /// The actual selected SDK Create returned null. No fallback was selected.
    NativeNotCreated,
    /// Unsupported-format logging would fatally name unknown scalability metadata.
    UnsafeNativeFormatFormatting,
}
impl fmt::Display for VideoDecoderCreationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for VideoDecoderCreationError {}

/// Move-only, caller-sequence SDK decoder. Creation, methods and final release
/// stay on the construction thread. This is not an actor-importable local handle.
/// The actual creation Environment and its Rust leases, private factory adapter
/// and selected registration outlive the decoder and its final native teardown.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::OwnedVideoDecoder>();
/// ```
pub struct OwnedVideoDecoder {
    // Drop native product before the selected factory registration.
    native: cxx::UniquePtr<ffi::NativeVideoDecoderProduct>,
    factory: VideoDecoderFactoryHandle,
    _execution: Option<Rc<crate::peer::FactoryInner>>,
    _caller: PhantomData<Rc<()>>,
}
impl fmt::Debug for OwnedVideoDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedVideoDecoder").finish_non_exhaustive()
    }
}
impl VideoDecoderFactoryHandle {
    /// Call the selected SDK Create with the exact owned format/environment.
    /// A null result is not converted to stock selection or a failed sentinel.
    pub fn create_decoder(
        &self,
        environment: &Environment,
        format: &SdpVideoFormat,
    ) -> Result<OwnedVideoDecoder, VideoDecoderCreationError> {
        let mut status = 0;
        let native = ffi::video_decoder_create_product(
            self.native(),
            environment.native(),
            Box::new(crate::video_factory_values::RustVideoFactoryEnvironment(
                environment.clone(),
            )),
            format.native(),
            &mut status,
        );
        match status {
            1 => return Err(VideoDecoderCreationError::UnsafeNativeFormatFormatting),
            0 => {}
            _ => unreachable!("closed decoder creation admission status"),
        }
        if native.is_null() {
            return Err(VideoDecoderCreationError::NativeNotCreated);
        }
        Ok(OwnedVideoDecoder {
            native,
            factory: self.clone(),
            _execution: None,
            _caller: PhantomData,
        })
    }
}
impl OwnedVideoDecoder {
    pub(crate) fn on_actor(
        owner: &crate::PeerConnectionFactory,
        format: &SdpVideoFormat,
    ) -> Result<Self, VideoDecoderCreationError> {
        let factory = owner
            .codec_decoder_factory()
            .ok_or(VideoDecoderCreationError::NativeNotCreated)?;
        let environment = owner.codec_environment();
        let execution = owner.codec_owner();
        let mut status = 0;
        let native = ffi::video_decoder_create_actor_product(
            execution.native(),
            factory.native(),
            environment.native(),
            Box::new(crate::video_factory_values::RustVideoFactoryEnvironment(
                environment.clone(),
            )),
            format.native(),
            &mut status,
        );
        if status == 1 {
            return Err(VideoDecoderCreationError::UnsafeNativeFormatFormatting);
        }
        if native.is_null() {
            return Err(VideoDecoderCreationError::NativeNotCreated);
        }
        Ok(Self {
            native,
            factory: factory.clone(),
            _execution: Some(execution),
            _caller: PhantomData,
        })
    }
    fn native(&self) -> &ffi::NativeVideoDecoderProduct {
        self.native.as_ref().unwrap()
    }
    /// Actual SDK GetDecoderInfo as an independent byte-preserving value copy.
    pub fn info(&self) -> Result<OwnedVideoDecoderInfo, crate::CodecError> {
        let native = ffi::video_decoder_product_info(self.native());
        if native.is_null() {
            return Err(crate::CodecError::CallbackUnavailable);
        }
        Ok(OwnedVideoDecoderInfo::from_native(native))
    }
    /// Complete exact SDK configuration and its actual boolean result.
    pub fn configure(&mut self, settings: &crate::OwnedVideoDecoderSettings) -> bool {
        ffi::video_decoder_product_configure(self.native.pin_mut(), settings.native())
    }
    /// The separate deprecated SDK ImplementationName method. Its result need
    /// not equal GetDecoderInfo's name, particularly on caller adapters.
    pub fn implementation_name_bytes(&self) -> Result<Vec<u8>, crate::CodecError> {
        let mut valid = false;
        let name = ffi::video_decoder_product_implementation_name(self.native(), &mut valid);
        if valid {
            Ok(name)
        } else {
            Err(crate::CodecError::CallbackUnavailable)
        }
    }
    /// Actual native registration result, including explicit null. The product
    /// retains the installed callback independently of this caller handle.
    /// A negative caller-adapter status does not imply it discarded the argument.
    pub fn register_callback(&mut self, callback: Option<&crate::DecodedImageCallbackSink>) -> i32 {
        let pointer = callback.map_or(std::ptr::null(), |value| value.native() as *const _);
        // SAFETY: the optional pointer is borrowed for this synchronous call;
        // native registration retains its own owning copy before returning.
        unsafe { ffi::video_decoder_product_register_callback(self.native.pin_mut(), pointer) }
    }
    /// Complete owning EncodedImage and exact native integer result. Native
    /// parse failures remain native statuses; unsafe storage is a bridge error.
    pub fn decode(
        &mut self,
        frame: &crate::EncodedImage,
        render_time_ms: i64,
    ) -> Result<i32, crate::CodecError> {
        self.decode_inner(frame, false, false, render_time_ms)
    }
    pub fn decode_with_missing_frames(
        &mut self,
        frame: &crate::EncodedImage,
        missing_frames: bool,
        render_time_ms: i64,
    ) -> Result<i32, crate::CodecError> {
        self.decode_inner(frame, true, missing_frames, render_time_ms)
    }
    fn decode_inner(
        &mut self,
        frame: &crate::EncodedImage,
        overload: bool,
        missing: bool,
        render_time_ms: i64,
    ) -> Result<i32, crate::CodecError> {
        let mut valid = false;
        let status = ffi::video_decoder_product_decode(
            self.native.pin_mut(),
            frame.native(),
            overload,
            missing,
            render_time_ms,
            &mut valid,
        );
        if valid {
            Ok(status)
        } else {
            Err(crate::CodecError::InvalidFrame)
        }
    }
    /// Actual SDK Release status. Calls are forwarded, not cached or normalized;
    /// final destruction retains normal selected SDK destructor behavior.
    pub fn release(&mut self) -> i32 {
        ffi::video_decoder_product_release(self.native.pin_mut())
    }
    pub fn failure_observer(&self) -> Option<crate::VideoFactoryFailureObserver> {
        self.factory.failure_observer()
    }
}
