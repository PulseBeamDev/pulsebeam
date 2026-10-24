//! Owning standalone stock APM operations, separate from peer capture.
use crate::{AudioProcessingConfig, Environment, ffi};
use std::{marker::PhantomData, rc::Rc};

/// Exact native status, including nonfatal warnings and unnamed future codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioProcessingStatus(pub i32);
impl AudioProcessingStatus {
    pub const OK: Self = Self(0);
    pub const BAD_PARAMETER: Self = Self(-6);
    pub const BAD_SAMPLE_RATE: Self = Self(-7);
    pub const BAD_CHANNEL_COUNT: Self = Self(-9);
    pub const STREAM_PARAMETER_NOT_SET: Self = Self(-11);
    pub const BAD_STREAM_PARAMETER_WARNING: Self = Self(-13);
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioProcessorError {
    CreationFailed,
    InvalidDimensions,
    BufferTooShort,
    InvalidAnalogLevel,
    InvalidFloatInput,
    UnsafeFieldTrial,
    InvalidConfiguration,
    /// Raw SDK value is preserved, but this consuming range is not yet qualified.
    UnqualifiedGainRange,
    UnqualifiedEchoInputRange,
    UnqualifiedEchoDelayRange,
    ReentrantCall,
    NoOwningProcessorSelected,
    LinearOutputUnavailable,
    LinearEchoNotConfigured,
    UnqualifiedLinearOutputQuery,
    UnsafeQueuedRuntimeSetting,
    UnqualifiedQueuedRuntimeSetting,
    UnsafeRuntimeSetting,
    UnqualifiedRuntimeReinitialization,
    UnqualifiedDumpWorker,
    InvalidDumpWorker,
    DumpFileConversionFailed,
    InvalidDumpPath,
    UnqualifiedDumpFileAccess,
    /// A void caller callback failed. Native processing may have continued,
    /// but no PCM result was published. Inspect the retained component observer.
    CallbackFailed,
}
impl std::fmt::Display for AudioProcessorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for AudioProcessorError {}

/// Native StreamConfig. Frame size is floor(rate / 100), not rounded up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioProcessingStream {
    pub sample_rate_hz: i32,
    pub channels: usize,
}
impl AudioProcessingStream {
    pub fn frame_size(self) -> Result<usize, AudioProcessorError> {
        if self.sample_rate_hz < 0 || self.channels > i32::MAX as usize {
            return Err(AudioProcessorError::InvalidDimensions);
        }
        Ok(self.sample_rate_hz as usize / 100)
    }
    pub fn sample_count(self) -> Result<usize, AudioProcessorError> {
        self.frame_size()?
            .checked_mul(self.channels)
            .ok_or(AudioProcessorError::InvalidDimensions)
    }
}

/// Exact four native stream descriptions. Default preserves the SDK's empty
/// ProcessingConfig; empty capture is not safe to initialize in this pin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioProcessingFormats {
    pub input: AudioProcessingStream,
    pub output: AudioProcessingStream,
    pub reverse_input: AudioProcessingStream,
    pub reverse_output: AudioProcessingStream,
}
impl Default for AudioProcessingFormats {
    fn default() -> Self {
        let f = ffi::audio_processor_formats_default();
        Self {
            input: AudioProcessingStream {
                sample_rate_hz: f.input_rate,
                channels: f.input_channels,
            },
            output: AudioProcessingStream {
                sample_rate_hz: f.output_rate,
                channels: f.output_channels,
            },
            reverse_input: AudioProcessingStream {
                sample_rate_hz: f.reverse_input_rate,
                channels: f.reverse_input_channels,
            },
            reverse_output: AudioProcessingStream {
                sample_rate_hz: f.reverse_output_rate,
                channels: f.reverse_output_channels,
            },
        }
    }
}
impl AudioProcessingFormats {
    fn ffi(self) -> ffi::FfiApmFormats {
        ffi::FfiApmFormats {
            input_rate: self.input.sample_rate_hz,
            input_channels: self.input.channels,
            output_rate: self.output.sample_rate_hz,
            output_channels: self.output.channels,
            reverse_input_rate: self.reverse_input.sample_rate_hz,
            reverse_input_channels: self.reverse_input.channels,
            reverse_output_rate: self.reverse_output.sample_rate_hz,
            reverse_output_channels: self.reverse_output.channels,
        }
    }
}

/// Native rates/channels reported by the currently initialized processing graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioProcessingFormatState {
    pub processing_rate_hz: i32,
    pub split_rate_hz: i32,
    pub input_channels: usize,
    pub processing_channels: usize,
    pub output_channels: usize,
    pub reverse_channels: usize,
}

/// Exact optional SDK statistics. Pulling a snapshot preserves the native
/// reporting queue and aggregation-window side effects.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioProcessingStatistics {
    /// On failure every value is unavailable, not a successful native absence.
    pub failure: Option<AudioProcessorError>,
    pub voice_detected: Option<bool>,
    pub echo_return_loss: Option<f64>,
    pub echo_return_loss_enhancement: Option<f64>,
    pub divergent_filter_fraction: Option<f64>,
    pub delay_median_ms: Option<i32>,
    pub delay_standard_deviation_ms: Option<i32>,
    pub residual_echo_likelihood: Option<f64>,
    pub residual_echo_likelihood_recent_max: Option<f64>,
    pub delay_ms: Option<i32>,
}
impl AudioProcessingStatistics {
    fn from_ffi(s: ffi::FfiApmStatistics) -> Self {
        Self {
            failure: AudioProcessingModule::admission(s.error).err(),
            voice_detected: (s.present & (1 << 0) != 0).then_some(s.voice_detected),
            echo_return_loss: (s.present & (1 << 1) != 0).then_some(s.echo_return_loss),
            echo_return_loss_enhancement: (s.present & (1 << 2) != 0)
                .then_some(s.echo_return_loss_enhancement),
            divergent_filter_fraction: (s.present & (1 << 3) != 0)
                .then_some(s.divergent_filter_fraction),
            delay_median_ms: (s.present & (1 << 4) != 0).then_some(s.delay_median_ms),
            delay_standard_deviation_ms: (s.present & (1 << 5) != 0)
                .then_some(s.delay_standard_deviation_ms),
            residual_echo_likelihood: (s.present & (1 << 6) != 0)
                .then_some(s.residual_echo_likelihood),
            residual_echo_likelihood_recent_max: (s.present & (1 << 7) != 0)
                .then_some(s.residual_echo_likelihood_recent_max),
            delay_ms: (s.present & (1 << 8) != 0).then_some(s.delay_ms),
        }
    }
}
impl Default for AudioProcessingStatistics {
    fn default() -> Self {
        Self::from_ffi(ffi::audio_processor_statistics_default())
    }
}

/// Single-sequence owning stock module. The exact environment is retained until
/// after native module destruction. No device, worker or hidden pump is created.
pub(crate) struct RustInstalledApmEnvironment {
    pub(crate) _environment: Environment,
}
pub struct AudioProcessingModule {
    native: cxx::UniquePtr<ffi::NativeAudioProcessingModule>,
    _environment: Environment,
    _sequence: PhantomData<Rc<()>>,
}
impl AudioProcessingModule {
    pub(crate) fn native_pin(&mut self) -> std::pin::Pin<&mut ffi::NativeAudioProcessingModule> {
        self.native.pin_mut()
    }
    pub(crate) fn into_installed(
        self,
    ) -> Result<cxx::UniquePtr<ffi::NativeInstalledAudioProcessor>, AudioProcessorError> {
        let mut error = 0;
        let native = ffi::audio_processor_into_installed(
            self.native,
            Box::new(RustInstalledApmEnvironment {
                _environment: self._environment,
            }),
            &mut error,
        );
        Self::admission(error)?;
        Ok(native)
    }
    pub(crate) fn admission(error: u8) -> Result<(), AudioProcessorError> {
        match error {
            0 => Ok(()),
            1 => Err(AudioProcessorError::UnsafeFieldTrial),
            2 => Err(AudioProcessorError::InvalidDimensions),
            4 => Err(AudioProcessorError::UnqualifiedGainRange),
            5 => Err(AudioProcessorError::LinearOutputUnavailable),
            6 => Err(AudioProcessorError::BufferTooShort),
            7 => Err(AudioProcessorError::LinearEchoNotConfigured),
            8 => Err(AudioProcessorError::UnqualifiedLinearOutputQuery),
            9 => Err(AudioProcessorError::UnsafeQueuedRuntimeSetting),
            10 => Err(AudioProcessorError::UnqualifiedQueuedRuntimeSetting),
            11 => Err(AudioProcessorError::UnsafeRuntimeSetting),
            12 => Err(AudioProcessorError::UnqualifiedRuntimeReinitialization),
            13 => Err(AudioProcessorError::UnqualifiedDumpWorker),
            14 => Err(AudioProcessorError::InvalidDumpWorker),
            15 => Err(AudioProcessorError::DumpFileConversionFailed),
            16 => Err(AudioProcessorError::InvalidDumpPath),
            17 => Err(AudioProcessorError::UnqualifiedDumpFileAccess),
            18 => Err(AudioProcessorError::CallbackFailed),
            19 => Err(AudioProcessorError::UnqualifiedEchoInputRange),
            20 => Err(AudioProcessorError::UnqualifiedEchoDelayRange),
            21 => Err(AudioProcessorError::ReentrantCall),
            22 => Err(AudioProcessorError::NoOwningProcessorSelected),
            _ => Err(AudioProcessorError::InvalidConfiguration),
        }
    }
    /// Create and attach the actual stock AEC dump. Filename bytes are passed
    /// unchanged (non-UTF8 paths are supported on Linux); NUL is not a path.
    /// The worker is consumed on every outcome and retained through native
    /// dump destruction on success. Only proven threaded workers are admitted.
    /// Negative maximum sizes retain native unlimited-size behavior.
    pub fn create_and_attach_aec_dump(
        &mut self,
        filename: &[u8],
        maximum_bytes: i64,
        worker: crate::TaskQueue,
    ) -> Result<bool, AudioProcessorError> {
        if filename.contains(&0) {
            return Err(AudioProcessorError::InvalidDumpPath);
        }
        let worker = crate::audio_processor_dump::RustApmDumpWorker::new(worker)?;
        let mut error = 0;
        let attached = ffi::audio_processor_dump_path(
            self.native.pin_mut(),
            worker,
            filename,
            maximum_bytes,
            &mut error,
        );
        Self::admission(error)?;
        Ok(attached)
    }
    /// Owning FILE overload. The file and worker are consumed even on failure.
    /// Conversion preserves the descriptor's access mode and current position.
    /// In protobuf-disabled builds the SDK leaves FILE open; this owning adapter
    /// closes it instead of leaking it, while retaining the native false result.
    #[cfg(unix)]
    pub fn create_and_attach_aec_dump_file(
        &mut self,
        file: std::fs::File,
        maximum_bytes: i64,
        worker: crate::TaskQueue,
    ) -> Result<bool, AudioProcessorError> {
        use std::os::fd::IntoRawFd;
        let worker = crate::audio_processor_dump::RustApmDumpWorker::new(worker)?;
        let mut error = 0;
        let attached = ffi::audio_processor_dump_file(
            self.native.pin_mut(),
            worker,
            file.into_raw_fd(),
            maximum_bytes,
            &mut error,
        );
        Self::admission(error)?;
        Ok(attached)
    }
    /// Native synchronous detach and flush. Release the retained queue only
    /// after the native dumper has finished its completion barrier.
    pub fn detach_aec_dump(&mut self) -> Result<(), AudioProcessorError> {
        Self::admission(ffi::audio_processor_dump_detach(self.native.pin_mut()))
    }
    /// Transfer unique ownership to the actual native APM. A callback error
    /// does not roll back attachment. Clone the dump observer before handoff.
    pub fn attach_aec_dump(
        &mut self,
        dump: crate::AudioProcessingDump,
    ) -> Result<(), AudioProcessorError> {
        Self::admission(ffi::audio_processor_dump_attach(
            self.native.pin_mut(),
            dump.into_inner(),
        ))
    }
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeAudioProcessingModule>,
        environment: Environment,
    ) -> Result<Self, AudioProcessorError> {
        if native.is_null() {
            return Err(AudioProcessorError::CreationFailed);
        }
        Ok(Self {
            native,
            _environment: environment,
            _sequence: PhantomData,
        })
    }
    /// Call the actual void native setter. Ok means the call was admitted,
    /// not that enqueueing succeeded. Its native acknowledgment is unavailable.
    pub fn set_runtime_setting(
        &mut self,
        setting: crate::AudioProcessingRuntimeSetting,
    ) -> Result<(), AudioProcessorError> {
        let mut error = 0;
        ffi::audio_processor_runtime_setting(
            self.native.pin_mut(),
            &setting.ffi(),
            false,
            &mut error,
        );
        Self::admission(error)
    }
    /// Return the actual native queue acknowledgment, not a cached disposition.
    pub fn post_runtime_setting(
        &mut self,
        setting: crate::AudioProcessingRuntimeSetting,
    ) -> Result<bool, AudioProcessorError> {
        let mut error = 0;
        let accepted = ffi::audio_processor_runtime_setting(
            self.native.pin_mut(),
            &setting.ffi(),
            true,
            &mut error,
        );
        Self::admission(error)?;
        Ok(accepted)
    }
    /// Copy the native 16-kHz linear echo output into the required channel
    /// prefix. Extra caller rows stay untouched. Native absence is false unless
    /// this SDK build asserts on that query, in which case it is a wrapper error.
    pub fn linear_aec_output(
        &mut self,
        destination: &mut [[f32; 160]],
    ) -> Result<bool, AudioProcessorError> {
        let mut error = 0;
        let available = ffi::audio_processor_linear_output(
            self.native.pin_mut(),
            destination.as_flattened_mut(),
            &mut error,
        );
        Self::admission(error)?;
        Ok(available)
    }
    /// Fallible projection of the same native statistics operation. No snapshot
    /// is returned after a callback failure, even if native getters supplied
    /// fallback metric values internally.
    pub fn try_statistics(&mut self) -> Result<AudioProcessingStatistics, AudioProcessorError> {
        let statistics = self.statistics();
        match statistics.failure {
            Some(error) => Err(error),
            None => Ok(statistics),
        }
    }
    pub fn try_statistics_for_remote_tracks(
        &mut self,
        has_remote_tracks: bool,
    ) -> Result<AudioProcessingStatistics, AudioProcessorError> {
        let statistics = self.statistics_for_remote_tracks(has_remote_tracks);
        match statistics.failure {
            Some(error) => Err(error),
            None => Ok(statistics),
        }
    }
    /// Pull the actual native statistics snapshot. Inspect failure before using
    /// any values, or use try_statistics to enforce the fallible projection.
    pub fn statistics(&mut self) -> AudioProcessingStatistics {
        AudioProcessingStatistics::from_ffi(ffi::audio_processor_statistics(
            self.native.pin_mut(),
            false,
            false,
        ))
    }
    /// Deprecated native overload, passing the remote-track argument unchanged.
    pub fn statistics_for_remote_tracks(
        &mut self,
        has_remote_tracks: bool,
    ) -> AudioProcessingStatistics {
        AudioProcessingStatistics::from_ffi(ffi::audio_processor_statistics(
            self.native.pin_mut(),
            true,
            has_remote_tracks,
        ))
    }
    /// Drive the builtin builder with every native config member unchanged.
    pub fn builtin_with_config(
        environment: &Environment,
        config: &crate::AudioProcessorConfig,
    ) -> Result<Self, AudioProcessorError> {
        Self::builtin_configured(environment, config, false)
    }
    /// Explicitly enable the AEC3 linear-output filter in both native default
    /// mono/multichannel configs via the public builder. The separate top-level
    /// config is passed unchanged, including its storage-export flag.
    pub fn builtin_with_linear_echo(
        environment: &Environment,
        config: &crate::AudioProcessorConfig,
    ) -> Result<Self, AudioProcessorError> {
        Self::builtin_configured(environment, config, true)
    }
    fn builtin_configured(
        environment: &Environment,
        config: &crate::AudioProcessorConfig,
        linear_echo: bool,
    ) -> Result<Self, AudioProcessorError> {
        let environment = environment.clone();
        let mut error = 0;
        let native = ffi::audio_processor_new_config(
            environment.native(),
            &config.ffi(),
            linear_echo,
            &mut error,
        );
        Self::admission(error)?;
        if native.is_null() {
            return Err(AudioProcessorError::CreationFailed);
        }
        Ok(Self {
            native,
            _environment: environment,
            _sequence: PhantomData,
        })
    }
    /// Native ApplyConfig, including its actual GC2 validation/reset behavior.
    pub fn apply_config(
        &mut self,
        config: &crate::AudioProcessorConfig,
    ) -> Result<(), AudioProcessorError> {
        Self::admission(ffi::audio_processor_config_apply(
            self.native.pin_mut(),
            &config.ffi(),
        ))
    }
    /// Select builtin APM with the same initial effect choices as peer capture.
    /// This creates a standalone module, not an installed peer component.
    pub fn builtin(
        environment: &Environment,
        config: AudioProcessingConfig,
    ) -> Result<Self, AudioProcessorError> {
        let environment = environment.clone();
        if !ffi::audio_processor_effects_admitted(environment.native(), &config.ffi()) {
            return Err(AudioProcessorError::UnsafeFieldTrial);
        }
        let native = ffi::audio_processor_new(environment.native(), &config.ffi());
        if native.is_null() {
            return Err(AudioProcessorError::CreationFailed);
        }
        Ok(Self {
            native,
            _environment: environment,
            _sequence: PhantomData,
        })
    }
    pub fn initialize(&mut self) -> Result<AudioProcessingStatus, AudioProcessorError> {
        let mut error = 0;
        let status = ffi::audio_processor_initialize(self.native.pin_mut(), &mut error);
        Self::admission(error)?;
        Ok(AudioProcessingStatus(status))
    }
    /// Initialize the actual four-stream graph. This native overload always
    /// returns OK, even for unsafe dimensions, so inadmissible layouts are
    /// wrapper errors before entry, never invented native status codes.
    pub fn initialize_with_formats(
        &mut self,
        formats: AudioProcessingFormats,
    ) -> Result<AudioProcessingStatus, AudioProcessorError> {
        for s in [
            formats.input,
            formats.output,
            formats.reverse_input,
            formats.reverse_output,
        ] {
            s.sample_count()?;
        }
        let mut admitted = false;
        let mut error = 0;
        let status = ffi::audio_processor_initialize_formats(
            self.native.pin_mut(),
            &formats.ffi(),
            &mut admitted,
            &mut error,
        );
        Self::admission(error)?;
        if !admitted {
            return Err(AudioProcessorError::InvalidDimensions);
        }
        Ok(AudioProcessingStatus(status))
    }
    /// Apply effect selection to this actual module without substituting a
    /// product. Settings not represented by these choices remain unchanged.
    pub fn configure_effects(
        &mut self,
        config: AudioProcessingConfig,
    ) -> Result<(), AudioProcessorError> {
        Self::admission(ffi::audio_processor_configure(
            self.native.pin_mut(),
            &config.ffi(),
        ))
    }
    /// Current actual SDK configuration, including disabled nested fields.
    pub fn configuration(&self) -> crate::AudioProcessorConfig {
        crate::AudioProcessorConfig::from_ffi(ffi::audio_processor_config_get(
            self.native.as_ref().unwrap(),
        ))
    }
    pub fn format_state(&self) -> AudioProcessingFormatState {
        let s = ffi::audio_processor_format(self.native.as_ref().unwrap());
        AudioProcessingFormatState {
            processing_rate_hz: s.processing_rate_hz,
            split_rate_hz: s.split_rate_hz,
            input_channels: s.input_channels,
            processing_channels: s.processing_channels,
            output_channels: s.output_channels,
            reverse_channels: s.reverse_channels,
        }
    }
    pub fn set_output_will_be_muted(&mut self, muted: bool) {
        ffi::audio_processor_set_muted(self.native.pin_mut(), muted);
    }
    pub fn output_will_be_muted(&mut self) -> bool {
        ffi::audio_processor_muted(self.native.pin_mut())
    }
    /// The SDK preserves its actual clamp and warning for delays outside 0..500.
    pub fn set_stream_delay_ms(&mut self, delay: i32) -> AudioProcessingStatus {
        AudioProcessingStatus(ffi::audio_processor_set_delay(self.native.pin_mut(), delay))
    }
    pub fn stream_delay_ms(&self) -> i32 {
        ffi::audio_processor_delay(self.native.as_ref().unwrap())
    }
    pub fn set_stream_analog_level(&mut self, level: i32) -> Result<(), AudioProcessorError> {
        if !(0..=255).contains(&level) {
            return Err(AudioProcessorError::InvalidAnalogLevel);
        }
        ffi::audio_processor_set_analog_level(self.native.pin_mut(), level);
        Ok(())
    }
    pub fn recommended_stream_analog_level(&self) -> i32 {
        ffi::audio_processor_analog_level(self.native.as_ref().unwrap())
    }
    pub fn set_stream_key_pressed(&mut self, pressed: bool) {
        ffi::audio_processor_set_key(self.native.pin_mut(), pressed);
    }

    /// Process only the native 10-ms prefix in place. Tail samples are untouched.
    /// The returned native error/warning does not imply that the prefix was
    /// untouched: unsupported formats may produce native best-effort output.
    pub fn process_interleaved(
        &mut self,
        stream: AudioProcessingStream,
        samples: &mut [i16],
    ) -> Result<AudioProcessingStatus, AudioProcessorError> {
        self.process_i16(stream, samples, false)
    }
    pub fn process_reverse_interleaved(
        &mut self,
        stream: AudioProcessingStream,
        samples: &mut [i16],
    ) -> Result<AudioProcessingStatus, AudioProcessorError> {
        self.process_i16(stream, samples, true)
    }
    fn process_i16(
        &mut self,
        stream: AudioProcessingStream,
        samples: &mut [i16],
        reverse: bool,
    ) -> Result<AudioProcessingStatus, AudioProcessorError> {
        // Stock mono downmix sums into int32 and divides by a narrowed int16
        // channel count. 65536 would yield zero; larger counts can overflow.
        if stream.channels > u16::MAX as usize {
            return Err(AudioProcessorError::InvalidDimensions);
        }
        if samples.len() < stream.sample_count()? {
            return Err(AudioProcessorError::BufferTooShort);
        }
        let mut error = 0;
        let status = ffi::audio_processor_i16(
            self.native.pin_mut(),
            stream.sample_rate_hz,
            stream.channels,
            samples,
            reverse,
            &mut error,
        );
        Self::admission(error)?;
        Ok(AudioProcessingStatus(status))
    }
    /// Deinterleaved normalized float channels in contiguous channel-major
    /// storage. Input/output layouts may differ exactly as allowed by the SDK.
    /// Only native-sized prefixes of caller-initialized output are available.
    pub fn process_planar(
        &mut self,
        input: AudioProcessingStream,
        samples: &[f32],
        output: AudioProcessingStream,
        destination: &mut [f32],
    ) -> Result<AudioProcessingStatus, AudioProcessorError> {
        self.process_float(input, samples, output, destination, false)
    }
    pub fn process_reverse_planar(
        &mut self,
        input: AudioProcessingStream,
        samples: &[f32],
        output: AudioProcessingStream,
        destination: &mut [f32],
    ) -> Result<AudioProcessingStatus, AudioProcessorError> {
        self.process_float(input, samples, output, destination, true)
    }
    fn float_input(
        stream: AudioProcessingStream,
        samples: &[f32],
    ) -> Result<(), AudioProcessorError> {
        let count = stream.sample_count()?;
        if samples.len() < count {
            return Err(AudioProcessorError::BufferTooShort);
        }
        if samples[..count]
            .iter()
            .any(|v| !v.is_finite() || !(-1.0..=1.0).contains(v))
        {
            return Err(AudioProcessorError::InvalidFloatInput);
        }
        Ok(())
    }
    fn process_float(
        &mut self,
        input: AudioProcessingStream,
        samples: &[f32],
        output: AudioProcessingStream,
        destination: &mut [f32],
        reverse: bool,
    ) -> Result<AudioProcessingStatus, AudioProcessorError> {
        Self::float_input(input, samples)?;
        if destination.len() < output.sample_count()? {
            return Err(AudioProcessorError::BufferTooShort);
        }
        let mut error = 0;
        let status = ffi::audio_processor_float(
            self.native.pin_mut(),
            input.sample_rate_hz,
            input.channels,
            samples,
            output.sample_rate_hz,
            output.channels,
            destination,
            reverse,
            &mut error,
        );
        Self::admission(error)?;
        Ok(AudioProcessingStatus(status))
    }
    pub fn analyze_reverse_planar(
        &mut self,
        stream: AudioProcessingStream,
        samples: &[f32],
    ) -> Result<AudioProcessingStatus, AudioProcessorError> {
        Self::float_input(stream, samples)?;
        let mut error = 0;
        let status = ffi::audio_processor_analyze(
            self.native.pin_mut(),
            stream.sample_rate_hz,
            stream.channels,
            samples,
            &mut error,
        );
        Self::admission(error)?;
        Ok(AudioProcessingStatus(status))
    }
}
