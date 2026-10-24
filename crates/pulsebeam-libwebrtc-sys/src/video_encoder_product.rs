//! Caller-sequence products returned by the actual selected native factory.
use crate::{Environment, SdpVideoFormat, VideoEncoderFactoryHandle, ffi};
use std::{fmt, marker::PhantomData, rc::Rc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoEncoderCreationError {
    /// Actual native Create returned null, without fallback or failed sentinels.
    NativeNotCreated,
}
impl fmt::Display for VideoEncoderCreationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for VideoEncoderCreationError {}

/// Move-only native encoder on its construction sequence. Its actual creation
/// Environment, Rust leases and selected factory outlive native teardown.
/// This local product is not an actor-importable handle.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::OwnedVideoEncoder>();
/// ```
pub struct OwnedVideoEncoder {
    native: cxx::UniquePtr<ffi::NativeVideoEncoderProduct>,
    factory: VideoEncoderFactoryHandle,
    _execution: Option<Rc<crate::peer::FactoryInner>>,
    _caller: PhantomData<Rc<()>>,
}
impl fmt::Debug for OwnedVideoEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedVideoEncoder").finish_non_exhaustive()
    }
}
impl VideoEncoderFactoryHandle {
    /// Invoke this factory's native Create with the exact format/environment.
    /// A caller product, native null or creation failure is never replaced with
    /// a stock encoder. Native builtins retain their own adapter behavior.
    pub fn create_encoder(
        &self,
        environment: &Environment,
        format: &SdpVideoFormat,
    ) -> Result<OwnedVideoEncoder, VideoEncoderCreationError> {
        let native = ffi::video_encoder_create_product(
            self.native(),
            environment.native(),
            Box::new(crate::video_factory_values::RustVideoFactoryEnvironment(
                environment.clone(),
            )),
            format.native(),
        );
        if native.is_null() {
            return Err(VideoEncoderCreationError::NativeNotCreated);
        }
        Ok(OwnedVideoEncoder {
            native,
            factory: self.clone(),
            _execution: None,
            _caller: PhantomData,
        })
    }
}
impl OwnedVideoEncoder {
    pub(crate) fn on_actor(
        owner: &crate::PeerConnectionFactory,
        format: &SdpVideoFormat,
    ) -> Result<Self, VideoEncoderCreationError> {
        let factory = owner
            .codec_encoder_factory()
            .ok_or(VideoEncoderCreationError::NativeNotCreated)?;
        let environment = owner.codec_environment();
        let execution = owner.codec_owner();
        let native = ffi::video_encoder_create_actor_product(
            execution.native(),
            factory.native(),
            environment.native(),
            Box::new(crate::video_factory_values::RustVideoFactoryEnvironment(
                environment.clone(),
            )),
            format.native(),
        );
        if native.is_null() {
            return Err(VideoEncoderCreationError::NativeNotCreated);
        }
        Ok(Self {
            native,
            factory: factory.clone(),
            _execution: Some(execution),
            _caller: PhantomData,
        })
    }
    pub(crate) fn native(&self) -> &ffi::NativeVideoEncoderProduct {
        self.native.as_ref().expect("owning native encoder")
    }
    /// Invoke the actual selected SDK GetEncoderInfo, including its native
    /// simulcast aggregation and pre-init caching. This is not a direct leaf
    /// provider query. A caller callback failure reached by this invocation
    /// remains an error, with lasting evidence on failure_observer().
    pub fn info(&self) -> Result<crate::OwnedVideoEncoderInfo, crate::CodecError> {
        let value = ffi::video_encoder_product_info(self.native());
        if value.is_null() {
            Err(crate::CodecError::CallbackUnavailable)
        } else {
            Ok(crate::OwnedVideoEncoderInfo::from_native(value))
        }
    }
    /// Independent fresh native info invocation, byte-preserving name projection.
    pub fn implementation_name_bytes(&self) -> Result<Vec<u8>, crate::CodecError> {
        let mut valid = false;
        let value = ffi::video_encoder_product_name(self.native(), &mut valid);
        if valid {
            Ok(value)
        } else {
            Err(crate::CodecError::CallbackUnavailable)
        }
    }
    pub fn set_fec_controller_override(&mut self, value: Option<&crate::FecControllerOverride>) {
        ffi::video_encoder_product_fec(
            self.native.pin_mut(),
            value.map_or_else(cxx::SharedPtr::null, |v| v.native.clone()),
        );
    }
    /// Exact nullable registration. The actual product retains this callback
    /// independently of the caller handle through Release and native teardown.
    pub fn register_callback(&mut self, callback: Option<&crate::EncodedImageCallbackSink>) -> i32 {
        let pointer = callback.map_or(std::ptr::null(), |v| v.native() as *const _);
        // SAFETY: borrowed for this call, retained by native before returning.
        unsafe { ffi::video_encoder_product_register_callback(self.native.pin_mut(), pointer) }
    }
    /// Full native InitEncode and exact status. Deprecated three-argument
    /// dispatch is separate and intentionally has no capability/thread options.
    pub fn initialize(
        &mut self,
        codec: &crate::OwnedVideoCodec,
        settings: crate::VideoEncoderSettingsValue,
    ) -> i32 {
        ffi::video_encoder_product_initialize(
            self.native.pin_mut(),
            codec.native(),
            settings.native(),
            false,
        )
    }
    pub fn initialize_legacy(
        &mut self,
        codec: &crate::OwnedVideoCodec,
        number_of_cores: i32,
        max_payload_size: usize,
    ) -> i32 {
        let settings = crate::VideoEncoderSettingsValue {
            loss_notification: false,
            number_of_cores,
            max_payload_size,
            encoder_thread_limit: None,
        };
        ffi::video_encoder_product_initialize(
            self.native.pin_mut(),
            codec.native(),
            settings.native(),
            true,
        )
    }
    /// Actual native raw input. None and Some(&[]) are distinct native pointers.
    pub fn encode(
        &mut self,
        frame: &crate::RawVideoFrame,
        frame_types: Option<&[crate::EncodedImageFrameType]>,
    ) -> i32 {
        let types: Vec<_> = frame_types
            .unwrap_or_default()
            .iter()
            .map(|v| v.native())
            .collect();
        ffi::video_encoder_product_encode(
            self.native.pin_mut(),
            frame.native(),
            frame_types.is_some(),
            &types,
        )
    }
    pub fn set_rates(&mut self, rates: &crate::OwnedVideoRateControl) {
        ffi::video_encoder_product_rates(self.native.pin_mut(), rates.native());
    }
    /// Actual SDK void methods. Callback failures are observable separately on
    /// failure_observer(); no success/status or acknowledgement is invented.
    pub fn on_packet_loss_rate_update(&mut self, packet_loss_rate: f32) {
        ffi::video_encoder_product_packet_loss(self.native.pin_mut(), packet_loss_rate);
    }
    pub fn on_rtt_update(&mut self, rtt_ms: i64) {
        ffi::video_encoder_product_rtt(self.native.pin_mut(), rtt_ms);
    }
    pub fn on_loss_notification(&mut self, notification: crate::VideoEncoderLossNotification) {
        ffi::video_encoder_product_loss_notification(self.native.pin_mut(), notification.native());
    }
    /// Forward every call to the selected product's native Release.
    pub fn release(&mut self) -> i32 {
        ffi::video_encoder_product_release(self.native.pin_mut())
    }
    pub fn failure_observer(&self) -> Option<crate::VideoFactoryFailureObserver> {
        self.factory.failure_observer()
    }
}
