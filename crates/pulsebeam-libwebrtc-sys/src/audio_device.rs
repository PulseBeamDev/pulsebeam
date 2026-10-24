//! Explicit Linux/native owning AudioDeviceModule. Source-only until final qualification.
use crate::{Environment, WorkerThread, ffi};
use std::{fmt, marker::PhantomData, rc::Rc};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AudioDeviceLayer {
    PlatformDefault = 0,
    LinuxAlsa = 3,
    LinuxPulse = 4,
    Dummy = 10,
}
impl AudioDeviceLayer {
    fn from_native(value: i32) -> Result<Self, AudioDeviceError> {
        match value {
            0 => Ok(Self::PlatformDefault),
            3 => Ok(Self::LinuxAlsa),
            4 => Ok(Self::LinuxPulse),
            10 => Ok(Self::Dummy),
            _ => Err(AudioDeviceError::InvalidNativeLayer(value)),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioDeviceCreationMethod {
    /// Declared but not implemented by this SDK pin. Refused without substituting
    /// the free factory or inventing a nullable native result.
    ModuleCreate {
        bypass_voice_processing: bool,
    },
    CreateAudioDeviceModule {
        bypass_voice_processing: bool,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioDeviceCreationConfig {
    pub layer: AudioDeviceLayer,
    pub method: AudioDeviceCreationMethod,
}
impl Default for AudioDeviceCreationConfig {
    fn default() -> Self {
        Self {
            layer: AudioDeviceLayer::PlatformDefault,
            method: AudioDeviceCreationMethod::CreateAudioDeviceModule {
                bypass_voice_processing: false,
            },
        }
    }
}
impl AudioDeviceCreationConfig {
    pub(crate) fn ffi(self) -> ffi::FfiNativeAudioDeviceCreation {
        let (helper, bypass) = match self.method {
            AudioDeviceCreationMethod::CreateAudioDeviceModule {
                bypass_voice_processing,
            } => (true, bypass_voice_processing),
            AudioDeviceCreationMethod::ModuleCreate {
                bypass_voice_processing,
            } => (false, bypass_voice_processing),
        };
        ffi::FfiNativeAudioDeviceCreation {
            layer: self.layer as u8,
            helper,
            bypass,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioDeviceError {
    NativeArtifactRequired,
    /// The SDK declares AudioDeviceModule::Create but contains no definition.
    UnlinkedStaticCreate,
    NativeModuleUnavailable,
    ControlledExecutionConflict,
    ActiveTransportRegistration,
    SessionClosed,
    Native(i32),
    InvalidNativeLayer(i32),
}
impl fmt::Display for AudioDeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "audio device: {self:?}")
    }
}
impl std::error::Error for AudioDeviceError {}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceName {
    pub name: Vec<u8>,
    pub guid: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AudioDeviceStatistics {
    pub synthesized_samples_duration_s: f64,
    pub synthesized_samples_events: u64,
    pub total_samples_duration_s: f64,
    pub total_playout_delay_s: f64,
    pub total_samples_count: u64,
    pub dropped_samples_duration_s: f64,
    pub dropped_samples_events: u64,
    pub total_capture_samples_duration_s: f64,
    pub total_capture_delay_s: f64,
    pub total_capture_samples_count: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformAudioProcessingTopology {
    Independent,
    EchoCancellationAndNoiseSuppressionCoupled,
    Unknown(i32),
}
#[derive(Clone, Debug, PartialEq)]
pub struct PlatformAudioProcessingState {
    pub topology: PlatformAudioProcessingTopology,
    pub echo_cancellation_available: bool,
    pub noise_suppression_available: bool,
    pub auto_gain_control_available: bool,
    pub echo_cancellation_requested: Option<bool>,
    pub noise_suppression_requested: Option<bool>,
    pub auto_gain_control_requested: Option<bool>,
    pub echo_cancellation_active: Option<bool>,
    pub noise_suppression_active: Option<bool>,
    pub auto_gain_control_active: Option<bool>,
    pub voice_processing_enabled_requested: Option<bool>,
    pub voice_processing_bypassed_requested: Option<bool>,
    pub voice_processing_agc_enabled_requested: Option<bool>,
    pub voice_processing_enabled_active: Option<bool>,
    pub voice_processing_bypassed_active: Option<bool>,
    pub voice_processing_agc_enabled_active: Option<bool>,
}
/// Owns an explicitly supplied live worker and retains its environment. All ADM
/// calls and final SDK release marshal to that worker. No thread is started by
/// this owner and no headless/platform fallback is selected on creation failure.
pub struct AudioDeviceModule {
    native: cxx::UniquePtr<ffi::NativeAudioDevice>,
    _worker: WorkerThread,
    _environment: Environment,
    // Release after module, callbacks, worker and environment. Factory handoff
    // acquires its own production lease before consuming this one.
    _production_lease: cxx::UniquePtr<ffi::NativeProductionLease>,
    _sequence: PhantomData<Rc<()>>,
}

/// Factory-bound device controls/diagnostics for the exact installed ADM.
/// Retains its entire local owner graph. Engine lifecycle and AudioTransport
/// registration remain engine-owned rather than exposing conflicting teardown.
pub struct InstalledAudioDevice {
    native: cxx::UniquePtr<ffi::NativeAudioDevice>,
    _factory: Rc<crate::peer::FactoryInner>,
}
impl InstalledAudioDevice {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeAudioDevice>,
        factory: Rc<crate::peer::FactoryInner>,
    ) -> Result<Self, AudioDeviceError> {
        if native.is_null() {
            return Err(AudioDeviceError::NativeModuleUnavailable);
        }
        Ok(Self {
            native,
            _factory: factory,
        })
    }
    fn status(value: i32) -> Result<(), AudioDeviceError> {
        AudioDeviceModule::status(value)
    }
    pub fn platform_processing_topology(&mut self) -> PlatformAudioProcessingTopology {
        let r = ffi::audio_device_call(self.native.pin_mut(), 59, 0);
        match r.int_value as i32 {
            0 => PlatformAudioProcessingTopology::Independent,
            1 => PlatformAudioProcessingTopology::EchoCancellationAndNoiseSuppressionCoupled,
            n => PlatformAudioProcessingTopology::Unknown(n),
        }
    }

    pub fn playout_initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 4, 0);
        r.bool_value
    }
    pub fn recording_initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 6, 0);
        r.bool_value
    }
    pub fn playing(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 9, 0);
        r.bool_value
    }
    pub fn recording(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 12, 0);
        r.bool_value
    }
    pub fn speaker_initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 14, 0);
        r.bool_value
    }
    pub fn microphone_initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 16, 0);
        r.bool_value
    }
    pub fn playout_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 17, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn recording_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 18, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn speaker_volume_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 19, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_speaker_volume(&mut self, value: u32) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 20, value);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn speaker_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 21, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn max_speaker_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 22, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn min_speaker_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 23, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn microphone_volume_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 24, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_microphone_volume(&mut self, value: u32) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 25, value);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn microphone_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 26, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn max_microphone_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 27, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn min_microphone_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 28, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn speaker_mute_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 29, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_speaker_mute(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 30, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn speaker_mute(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 31, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn microphone_mute_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 32, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_microphone_mute(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 33, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn microphone_mute(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 34, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn stereo_playout_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 35, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_stereo_playout(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 36, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn stereo_playout(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 37, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn stereo_recording_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 38, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_stereo_recording(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 39, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn stereo_recording(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 40, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn built_in_aec_available(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 41, 0);
        r.bool_value
    }
    pub fn built_in_agc_available(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 42, 0);
        r.bool_value
    }
    pub fn built_in_ns_available(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 43, 0);
        r.bool_value
    }
    pub fn enable_built_in_aec(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 44, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn enable_built_in_agc(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 45, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn enable_built_in_ns(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 46, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn platform_voice_processing_path_available(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 47, 0);
        r.bool_value
    }
    pub fn enable_platform_voice_processing_path(
        &mut self,
        value: bool,
    ) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 48, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn set_playout_device(&mut self, value: u16) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 50, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn set_recording_device(&mut self, value: u16) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 51, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn playout_device_count(&mut self) -> Result<u16, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 52, 0);
        Self::status(r.status)?;
        Ok(r.int_value as u16)
    }
    pub fn recording_device_count(&mut self) -> Result<u16, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 53, 0);
        Self::status(r.status)?;
        Ok(r.int_value as u16)
    }
    pub fn playout_underrun_count(&mut self) -> i32 {
        let r = ffi::audio_device_call(self.native.pin_mut(), 54, 0);
        r.int_value as i32
    }
    pub fn current_playout_device(&mut self) -> i32 {
        let r = ffi::audio_device_call(self.native.pin_mut(), 55, 0);
        r.int_value as i32
    }
    pub fn current_recording_device(&mut self) -> i32 {
        let r = ffi::audio_device_call(self.native.pin_mut(), 56, 0);
        r.int_value as i32
    }
    pub fn playout_delay_ms(&mut self) -> Result<u16, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 57, 0);
        Self::status(r.status)?;
        Ok(r.int_value as u16)
    }
    pub fn active_audio_layer(&mut self) -> Result<AudioDeviceLayer, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 58, 0);
        Self::status(r.status)?;
        AudioDeviceLayer::from_native(r.int_value as i32)
    }
    pub fn initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 2, 0);
        r.bool_value
    }
    pub fn stop_on_mute_mode_enabled(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 49, 0);
        r.bool_value
    }
    pub fn device_name(
        &mut self,
        recording: bool,
        index: u16,
    ) -> Result<AudioDeviceName, AudioDeviceError> {
        let f = ffi::audio_device_name(self.native.pin_mut(), recording, index);
        Self::status(f.status)?;
        Ok(AudioDeviceName {
            name: f.name,
            guid: f.guid,
        })
    }
    pub fn statistics(&mut self) -> Option<AudioDeviceStatistics> {
        let f = ffi::audio_device_statistics(self.native.pin_mut());
        if !f.present {
            return None;
        }
        Some(AudioDeviceStatistics {
            synthesized_samples_duration_s: f.synthesized_samples_duration_s,
            synthesized_samples_events: f.synthesized_samples_events,
            total_samples_duration_s: f.total_samples_duration_s,
            total_playout_delay_s: f.total_playout_delay_s,
            total_samples_count: f.total_samples_count,
            dropped_samples_duration_s: f.dropped_samples_duration_s,
            dropped_samples_events: f.dropped_samples_events,
            total_capture_samples_duration_s: f.total_capture_samples_duration_s,
            total_capture_delay_s: f.total_capture_delay_s,
            total_capture_samples_count: f.total_capture_samples_count,
        })
    }
    pub fn platform_processing_state(&mut self) -> PlatformAudioProcessingState {
        let f = ffi::audio_device_platform_state(self.native.pin_mut());
        let optional = |value| match value {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        };
        PlatformAudioProcessingState {
            topology: match f.topology {
                0 => PlatformAudioProcessingTopology::Independent,
                1 => PlatformAudioProcessingTopology::EchoCancellationAndNoiseSuppressionCoupled,
                n => PlatformAudioProcessingTopology::Unknown(n),
            },
            echo_cancellation_available: f.echo_cancellation_available,
            noise_suppression_available: f.noise_suppression_available,
            auto_gain_control_available: f.auto_gain_control_available,
            echo_cancellation_requested: optional(f.echo_cancellation_requested),
            noise_suppression_requested: optional(f.noise_suppression_requested),
            auto_gain_control_requested: optional(f.auto_gain_control_requested),
            echo_cancellation_active: optional(f.echo_cancellation_active),
            noise_suppression_active: optional(f.noise_suppression_active),
            auto_gain_control_active: optional(f.auto_gain_control_active),
            voice_processing_enabled_requested: optional(f.voice_processing_enabled_requested),
            voice_processing_bypassed_requested: optional(f.voice_processing_bypassed_requested),
            voice_processing_agc_enabled_requested: optional(
                f.voice_processing_agc_enabled_requested,
            ),
            voice_processing_enabled_active: optional(f.voice_processing_enabled_active),
            voice_processing_bypassed_active: optional(f.voice_processing_bypassed_active),
            voice_processing_agc_enabled_active: optional(f.voice_processing_agc_enabled_active),
        }
    }
    pub fn register_observer(
        &mut self,
        observer: crate::AudioDeviceObserver,
    ) -> Result<(), AudioDeviceError> {
        Self::status(ffi::factory_register_audio_device_observer(
            self._factory.native(),
            observer.native,
        ))
    }
    pub fn unregister_observer(&mut self) -> Result<(), AudioDeviceError> {
        Self::status(ffi::factory_register_audio_device_observer(
            self._factory.native(),
            cxx::UniquePtr::null(),
        ))
    }
}

impl AudioDeviceModule {
    pub(crate) fn into_factory(
        self,
    ) -> (
        cxx::UniquePtr<ffi::NativeAudioDevice>,
        WorkerThread,
        Environment,
    ) {
        (self.native, self._worker, self._environment)
    }
    /// The SDK's static Create is declaration-only at this pin. Returns
    /// UnlinkedStaticCreate, not a fabricated null or a substituted helper.
    pub fn create(
        environment: &Environment,
        worker: WorkerThread,
        layer: AudioDeviceLayer,
        bypass_voice_processing: bool,
    ) -> Result<Option<Self>, AudioDeviceError> {
        Self::create_impl(environment, worker, layer, bypass_voice_processing, false)
    }
    /// Actual CreateAudioDeviceModule helper, not substituted static creation.
    pub fn create_with_helper(
        environment: &Environment,
        worker: WorkerThread,
        layer: AudioDeviceLayer,
    ) -> Result<Option<Self>, AudioDeviceError> {
        Self::create_with_helper_options(environment, worker, layer, false)
    }
    /// Actual free factory including its bypass_voice_processing argument.
    pub fn create_with_helper_options(
        environment: &Environment,
        worker: WorkerThread,
        layer: AudioDeviceLayer,
        bypass_voice_processing: bool,
    ) -> Result<Option<Self>, AudioDeviceError> {
        Self::create_impl(environment, worker, layer, bypass_voice_processing, true)
    }
    fn create_impl(
        environment: &Environment,
        worker: WorkerThread,
        layer: AudioDeviceLayer,
        bypass: bool,
        helper: bool,
    ) -> Result<Option<Self>, AudioDeviceError> {
        if !cfg!(feature = "native") {
            return Err(AudioDeviceError::NativeArtifactRequired);
        }
        if !helper {
            return Err(AudioDeviceError::UnlinkedStaticCreate);
        }
        let lease = ffi::new_production_lease();
        if lease.is_null() {
            return Err(AudioDeviceError::ControlledExecutionConflict);
        }
        let environment = environment.clone();
        let mut error = 0;
        let native = ffi::audio_device_new(
            environment.native(),
            worker.native(),
            layer as u8,
            bypass,
            helper,
            &mut error,
        );
        if error != 0 {
            return Err(if error == 2 {
                AudioDeviceError::UnlinkedStaticCreate
            } else {
                AudioDeviceError::NativeArtifactRequired
            });
        }
        if native.is_null() {
            return Ok(None);
        }
        Ok(Some(Self {
            native,
            _worker: worker,
            _environment: environment,
            _production_lease: lease,
            _sequence: PhantomData,
        }))
    }
    pub fn register_transport(
        &mut self,
        transport: crate::AudioTransport,
    ) -> Result<(), AudioDeviceError> {
        let mut error = 0;
        let status = ffi::audio_device_register_transport(
            self.native.pin_mut(),
            transport.native,
            &mut error,
        );
        if error != 0 {
            return Err(AudioDeviceError::ActiveTransportRegistration);
        }
        Self::status(status)
    }
    pub fn unregister_transport(&mut self) -> Result<(), AudioDeviceError> {
        let mut error = 0;
        let status = ffi::audio_device_register_transport(
            self.native.pin_mut(),
            cxx::UniquePtr::null(),
            &mut error,
        );
        if error != 0 {
            return Err(AudioDeviceError::ActiveTransportRegistration);
        }
        Self::status(status)
    }
    pub fn register_observer(
        &mut self,
        observer: crate::AudioDeviceObserver,
    ) -> Result<(), AudioDeviceError> {
        Self::status(ffi::audio_device_register_observer(
            self.native.pin_mut(),
            observer.native,
        ))
    }
    pub fn unregister_observer(&mut self) -> Result<(), AudioDeviceError> {
        Self::status(ffi::audio_device_register_observer(
            self.native.pin_mut(),
            cxx::UniquePtr::null(),
        ))
    }
    pub fn platform_processing_topology(&mut self) -> PlatformAudioProcessingTopology {
        let r = ffi::audio_device_call(self.native.pin_mut(), 59, 0);
        match r.int_value as i32 {
            0 => PlatformAudioProcessingTopology::Independent,
            1 => PlatformAudioProcessingTopology::EchoCancellationAndNoiseSuppressionCoupled,
            n => PlatformAudioProcessingTopology::Unknown(n),
        }
    }
    fn status(value: i32) -> Result<(), AudioDeviceError> {
        if value == 0 {
            Ok(())
        } else {
            Err(AudioDeviceError::Native(value))
        }
    }
    pub fn init(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 0, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn terminate(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 1, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 2, 0);
        r.bool_value
    }
    pub fn init_playout(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 3, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn playout_initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 4, 0);
        r.bool_value
    }
    pub fn init_recording(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 5, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn recording_initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 6, 0);
        r.bool_value
    }
    pub fn start_playout(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 7, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn stop_playout(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 8, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn playing(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 9, 0);
        r.bool_value
    }
    pub fn start_recording(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 10, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn stop_recording(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 11, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn recording(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 12, 0);
        r.bool_value
    }
    pub fn init_speaker(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 13, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn speaker_initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 14, 0);
        r.bool_value
    }
    pub fn init_microphone(&mut self) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 15, 0);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn microphone_initialized(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 16, 0);
        r.bool_value
    }
    pub fn playout_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 17, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn recording_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 18, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn speaker_volume_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 19, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_speaker_volume(&mut self, value: u32) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 20, value);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn speaker_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 21, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn max_speaker_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 22, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn min_speaker_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 23, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn microphone_volume_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 24, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_microphone_volume(&mut self, value: u32) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 25, value);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn microphone_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 26, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn max_microphone_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 27, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn min_microphone_volume(&mut self) -> Result<u32, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 28, 0);
        Self::status(r.status)?;
        Ok(r.int_value)
    }
    pub fn speaker_mute_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 29, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_speaker_mute(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 30, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn speaker_mute(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 31, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn microphone_mute_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 32, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_microphone_mute(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 33, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn microphone_mute(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 34, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn stereo_playout_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 35, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_stereo_playout(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 36, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn stereo_playout(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 37, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn stereo_recording_available(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 38, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn set_stereo_recording(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 39, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn stereo_recording(&mut self) -> Result<bool, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 40, 0);
        Self::status(r.status)?;
        Ok(r.bool_value)
    }
    pub fn built_in_aec_available(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 41, 0);
        r.bool_value
    }
    pub fn built_in_agc_available(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 42, 0);
        r.bool_value
    }
    pub fn built_in_ns_available(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 43, 0);
        r.bool_value
    }
    pub fn enable_built_in_aec(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 44, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn enable_built_in_agc(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 45, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn enable_built_in_ns(&mut self, value: bool) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 46, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn platform_voice_processing_path_available(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 47, 0);
        r.bool_value
    }
    pub fn enable_platform_voice_processing_path(
        &mut self,
        value: bool,
    ) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 48, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn stop_on_mute_mode_enabled(&mut self) -> bool {
        let r = ffi::audio_device_call(self.native.pin_mut(), 49, 0);
        r.bool_value
    }
    pub fn set_playout_device(&mut self, value: u16) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 50, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn set_recording_device(&mut self, value: u16) -> Result<(), AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 51, value as u32);
        Self::status(r.status)?;
        Ok(())
    }
    pub fn playout_device_count(&mut self) -> Result<u16, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 52, 0);
        Self::status(r.status)?;
        Ok(r.int_value as u16)
    }
    pub fn recording_device_count(&mut self) -> Result<u16, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 53, 0);
        Self::status(r.status)?;
        Ok(r.int_value as u16)
    }
    pub fn playout_underrun_count(&mut self) -> i32 {
        let r = ffi::audio_device_call(self.native.pin_mut(), 54, 0);
        r.int_value as i32
    }
    pub fn current_playout_device(&mut self) -> i32 {
        let r = ffi::audio_device_call(self.native.pin_mut(), 55, 0);
        r.int_value as i32
    }
    pub fn current_recording_device(&mut self) -> i32 {
        let r = ffi::audio_device_call(self.native.pin_mut(), 56, 0);
        r.int_value as i32
    }
    pub fn playout_delay_ms(&mut self) -> Result<u16, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 57, 0);
        Self::status(r.status)?;
        Ok(r.int_value as u16)
    }
    pub fn active_audio_layer(&mut self) -> Result<AudioDeviceLayer, AudioDeviceError> {
        let r = ffi::audio_device_call(self.native.pin_mut(), 58, 0);
        Self::status(r.status)?;
        AudioDeviceLayer::from_native(r.int_value as i32)
    }
    pub fn device_name(
        &mut self,
        recording: bool,
        index: u16,
    ) -> Result<AudioDeviceName, AudioDeviceError> {
        let f = ffi::audio_device_name(self.native.pin_mut(), recording, index);
        Self::status(f.status)?;
        Ok(AudioDeviceName {
            name: f.name,
            guid: f.guid,
        })
    }
    pub fn statistics(&mut self) -> Option<AudioDeviceStatistics> {
        let f = ffi::audio_device_statistics(self.native.pin_mut());
        if !f.present {
            return None;
        }
        Some(AudioDeviceStatistics {
            synthesized_samples_duration_s: f.synthesized_samples_duration_s,
            synthesized_samples_events: f.synthesized_samples_events,
            total_samples_duration_s: f.total_samples_duration_s,
            total_playout_delay_s: f.total_playout_delay_s,
            total_samples_count: f.total_samples_count,
            dropped_samples_duration_s: f.dropped_samples_duration_s,
            dropped_samples_events: f.dropped_samples_events,
            total_capture_samples_duration_s: f.total_capture_samples_duration_s,
            total_capture_delay_s: f.total_capture_delay_s,
            total_capture_samples_count: f.total_capture_samples_count,
        })
    }
    pub fn platform_processing_state(&mut self) -> PlatformAudioProcessingState {
        let f = ffi::audio_device_platform_state(self.native.pin_mut());
        let optional = |value| match value {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        };
        PlatformAudioProcessingState {
            topology: match f.topology {
                0 => PlatformAudioProcessingTopology::Independent,
                1 => PlatformAudioProcessingTopology::EchoCancellationAndNoiseSuppressionCoupled,
                n => PlatformAudioProcessingTopology::Unknown(n),
            },
            echo_cancellation_available: f.echo_cancellation_available,
            noise_suppression_available: f.noise_suppression_available,
            auto_gain_control_available: f.auto_gain_control_available,
            echo_cancellation_requested: optional(f.echo_cancellation_requested),
            noise_suppression_requested: optional(f.noise_suppression_requested),
            auto_gain_control_requested: optional(f.auto_gain_control_requested),
            echo_cancellation_active: optional(f.echo_cancellation_active),
            noise_suppression_active: optional(f.noise_suppression_active),
            auto_gain_control_active: optional(f.auto_gain_control_active),
            voice_processing_enabled_requested: optional(f.voice_processing_enabled_requested),
            voice_processing_bypassed_requested: optional(f.voice_processing_bypassed_requested),
            voice_processing_agc_enabled_requested: optional(
                f.voice_processing_agc_enabled_requested,
            ),
            voice_processing_enabled_active: optional(f.voice_processing_enabled_active),
            voice_processing_bypassed_active: optional(f.voice_processing_bypassed_active),
            voice_processing_agc_enabled_active: optional(f.voice_processing_agc_enabled_active),
        }
    }
}
