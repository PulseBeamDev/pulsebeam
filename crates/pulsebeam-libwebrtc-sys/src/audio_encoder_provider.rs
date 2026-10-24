//! Owning, fallible caller encoder products. SDK Encode failure admission is
//! separate unfinished work; this type is not a native/peer codec adapter.
use crate::{
    AudioEncoderApplication, AudioEncoderBitrateRange, AudioEncoderError,
    AudioEncoderFrameLengthRange, AudioEncoderOutput, AudioNetworkAdaptationStatistics,
    BitrateAllocationUpdate, DataRate, Environment,
};
use std::{
    cell::RefCell,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Complete named caller product contract, omitting only the SDK's explicitly
/// forbidden ReclaimContainedEncoders. Methods return owned data, never mutable
/// native buffers. Callbacks are serialized; Send suffices for mutable providers.
/// Do not wait for or synchronously reenter the owning actor/product, including
/// destruction. Inputs/metadata are not codec/rate/channel convenience-whitelisted.
/// SDK adapter/consumer admission is not implied by implementing this trait.
pub trait AudioEncoderProvider: Send + 'static {
    fn sample_rate_hz(&mut self) -> i32;
    fn channels(&mut self) -> usize;
    fn rtp_timestamp_rate_hz(&mut self) -> i32 {
        self.sample_rate_hz()
    }
    fn frames_in_next_packet(&mut self) -> usize;
    fn maximum_frames_in_packet(&mut self) -> usize;
    fn target_bitrate_bps(&mut self) -> i32;
    fn encode(
        &mut self,
        timestamp: u32,
        audio: &[i16],
    ) -> Result<AudioEncoderOutput, AudioEncoderError>;
    fn reset(&mut self) -> Result<(), AudioEncoderError>;
    fn set_fec(&mut self, enable: bool) -> Result<bool, AudioEncoderError> {
        Ok(!enable)
    }
    fn set_dtx(&mut self, enable: bool) -> Result<bool, AudioEncoderError> {
        Ok(!enable)
    }
    fn dtx(&mut self) -> bool {
        false
    }
    fn set_application(
        &mut self,
        _application: AudioEncoderApplication,
    ) -> Result<bool, AudioEncoderError> {
        Ok(false)
    }
    fn set_max_playback_rate(&mut self, _frequency: i32) -> Result<(), AudioEncoderError> {
        Ok(())
    }
    fn set_target_bitrate(&mut self, _bitrate: i32) -> Result<(), AudioEncoderError> {
        Ok(())
    }
    fn enable_audio_network_adaptor(&mut self, _config: &[u8]) -> Result<bool, AudioEncoderError> {
        Ok(false)
    }
    fn disable_audio_network_adaptor(&mut self) -> Result<(), AudioEncoderError> {
        Ok(())
    }
    fn on_received_uplink_packet_loss_fraction(
        &mut self,
        _fraction: f32,
    ) -> Result<(), AudioEncoderError> {
        Ok(())
    }
    fn on_received_uplink_recoverable_packet_loss_fraction(
        &mut self,
        _fraction: f32,
    ) -> Result<(), AudioEncoderError> {
        Err(AudioEncoderError::UnsupportedControl)
    }
    fn on_received_target_audio_bitrate(&mut self, bitrate: i32) -> Result<(), AudioEncoderError> {
        self.on_received_uplink_bandwidth(bitrate, None)
    }
    fn on_received_uplink_bandwidth(
        &mut self,
        _bitrate: i32,
        _period: Option<i64>,
    ) -> Result<(), AudioEncoderError> {
        Ok(())
    }
    fn on_received_uplink_allocation(
        &mut self,
        update: BitrateAllocationUpdate,
    ) -> Result<(), AudioEncoderError> {
        // Default delegation only: an override receives the complete untouched
        // update. Use actual SDK getters and artifact-local assertion guards,
        // including native release infinity and narrowing behavior.
        let (target, kind) = match update.target_bitrate {
            DataRate::BitsPerSecond(value) => (value, 0),
            DataRate::PositiveInfinity => (0, 1),
            DataRate::NegativeInfinity => (0, -1),
        };
        let (mut valid, mut bitrate, mut period) = (false, 0, 0);
        crate::ffi::audio_encoder_default_allocation_arguments(
            target,
            kind,
            update.bwe_period.native_microsecond_bits(),
            &mut valid,
            &mut bitrate,
            &mut period,
        );
        if !valid {
            return Err(AudioEncoderError::InvalidControl);
        }
        self.on_received_uplink_bandwidth(bitrate, Some(period))
    }
    fn on_received_rtt(&mut self, _rtt: i32) -> Result<(), AudioEncoderError> {
        Ok(())
    }
    fn on_received_overhead(&mut self, _bytes: usize) -> Result<(), AudioEncoderError> {
        Ok(())
    }
    fn set_receiver_frame_length_range(
        &mut self,
        _minimum: i32,
        _maximum: i32,
    ) -> Result<(), AudioEncoderError> {
        Ok(())
    }
    fn network_adaptation_statistics(&mut self) -> AudioNetworkAdaptationStatistics {
        AudioNetworkAdaptationStatistics::default()
    }
    fn frame_length_range(&mut self) -> Option<AudioEncoderFrameLengthRange>;
    fn bitrate_range(&mut self) -> Option<AudioEncoderBitrateRange> {
        None
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioEncoderProductOperation {
    SampleRate,
    Channels,
    TimestampRate,
    NextPacketFrames,
    MaximumPacketFrames,
    TargetBitrate,
    Encode,
    Reset,
    SetFec,
    SetDtx,
    Dtx,
    SetApplication,
    SetPlaybackRate,
    SetTargetBitrate,
    EnableAdaptor,
    DisableAdaptor,
    PacketLoss,
    RecoverablePacketLoss,
    ReceivedTargetBitrate,
    UplinkBandwidth,
    UplinkAllocation,
    Rtt,
    Overhead,
    ReceiverFrameLengthRange,
    Statistics,
    FrameLengthRange,
    BitrateRange,
    Destroy,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioEncoderProductFailureKind {
    Provider(AudioEncoderError),
    InvalidSamples,
    InvalidOutput,
    Panicked,
    Disabled,
    ReentrantCall,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioEncoderProductError {
    pub operation: AudioEncoderProductOperation,
    pub kind: AudioEncoderProductFailureKind,
}
impl std::fmt::Display for AudioEncoderProductError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "caller audio encoder {:?}: {:?}",
            self.operation, self.kind
        )
    }
}
impl std::error::Error for AudioEncoderProductError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            AudioEncoderProductFailureKind::Provider(error) => Some(error),
            _ => None,
        }
    }
}
#[derive(Default)]
struct FailureState {
    first: Mutex<Option<AudioEncoderProductError>>,
    disabled: AtomicBool,
}
#[derive(Clone, Default)]
pub struct AudioEncoderProductFailureObserver(Arc<FailureState>);
impl AudioEncoderProductFailureObserver {
    pub fn failure(&self) -> Option<AudioEncoderProductError> {
        *self.0.first.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn is_disabled(&self) -> bool {
        self.0.disabled.load(Ordering::Acquire)
    }
    fn report(&self, error: AudioEncoderProductError) {
        self.0
            .first
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(error);
    }
}
thread_local! { static ACTIVE: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) }; }
struct Active(usize);
impl Active {
    fn enter(id: usize) -> Option<Self> {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&id) {
                None
            } else {
                active.push(id);
                Some(Self(id))
            }
        })
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            let i = active
                .iter()
                .rposition(|id| *id == self.0)
                .expect("active encoder callback");
            active.remove(i);
        });
    }
}
struct ProductState {
    provider: Mutex<Option<Box<dyn AudioEncoderProvider>>>,
    observer: AudioEncoderProductFailureObserver,
    // Declaration order destroys the provider before its retaining environment.
    _environment: Environment,
}
impl ProductState {
    fn invoke<T>(
        &self,
        operation: AudioEncoderProductOperation,
        call: impl FnOnce(&mut dyn AudioEncoderProvider) -> Result<T, AudioEncoderProductFailureKind>,
    ) -> Result<T, AudioEncoderProductError> {
        let failure = |kind| {
            let e = AudioEncoderProductError { operation, kind };
            self.observer.report(e);
            e
        };
        if self.observer.is_disabled() {
            return Err(failure(AudioEncoderProductFailureKind::Disabled));
        }
        let _active = Active::enter(self as *const Self as usize)
            .ok_or_else(|| failure(AudioEncoderProductFailureKind::ReentrantCall))?;
        let mut provider = self.provider.lock().unwrap_or_else(|e| e.into_inner());
        if self.observer.is_disabled() {
            return Err(failure(AudioEncoderProductFailureKind::Disabled));
        }
        match catch_unwind(AssertUnwindSafe(|| {
            call(provider.as_deref_mut().expect("owned encoder provider"))
        })) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(kind)) => Err(failure(kind)),
            Err(payload) => {
                let e = failure(AudioEncoderProductFailureKind::Panicked);
                self.observer.0.disabled.store(true, Ordering::Release);
                crate::panic_payload::discard(payload);
                Err(e)
            }
        }
    }
}
impl Drop for ProductState {
    fn drop(&mut self) {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(provider))) {
            self.observer.report(AudioEncoderProductError {
                operation: AudioEncoderProductOperation::Destroy,
                kind: AudioEncoderProductFailureKind::Panicked,
            });
            self.observer.0.disabled.store(true, Ordering::Release);
            crate::panic_payload::discard(payload);
        }
    }
}
/// Owning fallible caller encoder product. Clones name one serialized callback
/// state. This is not an AudioEncoder SDK handle and is not accepted by native
/// factory/peer creation paths until #38 failure admission is implemented.
#[derive(Clone)]
pub struct CallerAudioEncoder(Arc<ProductState>);
impl CallerAudioEncoder {
    pub fn from_provider(environment: &Environment, provider: impl AudioEncoderProvider) -> Self {
        Self(Arc::new(ProductState {
            provider: Mutex::new(Some(Box::new(provider))),
            observer: AudioEncoderProductFailureObserver::default(),
            _environment: environment.clone(),
        }))
    }
    pub fn failure_observer(&self) -> AudioEncoderProductFailureObserver {
        self.0.observer.clone()
    }
    pub fn encode(
        &self,
        timestamp: u32,
        audio: &[i16],
    ) -> Result<AudioEncoderOutput, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::Encode, |provider| {
                // Exact SDK size_t expression, including defined unsigned wrapping.
                // Metadata itself is not assigned an arbitrary channel/rate policy.
                let channels = provider.channels();
                let rate = provider.sample_rate_hz();
                if audio.len() != channels.wrapping_mul(rate as usize) / 100 {
                    return Err(AudioEncoderProductFailureKind::InvalidSamples);
                }
                let value = provider
                    .encode(timestamp, audio)
                    .map_err(AudioEncoderProductFailureKind::Provider)?;
                if value.info.primary.encoded_bytes != value.bytes.len() {
                    return Err(AudioEncoderProductFailureKind::InvalidOutput);
                }
                if !value.info.redundant.is_empty() {
                    let mut total = 0usize;
                    for leaf in &value.info.redundant {
                        total = total
                            .checked_add(leaf.encoded_bytes)
                            .ok_or(AudioEncoderProductFailureKind::InvalidOutput)?;
                        if total > value.bytes.len() {
                            return Err(AudioEncoderProductFailureKind::InvalidOutput);
                        }
                    }
                    if total != value.bytes.len() {
                        return Err(AudioEncoderProductFailureKind::InvalidOutput);
                    }
                }
                Ok(value)
            })
    }
    pub fn sample_rate_hz(&self) -> Result<i32, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::SampleRate, |p| {
                Ok(p.sample_rate_hz())
            })
    }
    pub fn channels(&self) -> Result<usize, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::Channels, |p| Ok(p.channels()))
    }
    pub fn rtp_timestamp_rate_hz(&self) -> Result<i32, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::TimestampRate, |p| {
                Ok(p.rtp_timestamp_rate_hz())
            })
    }
    pub fn frames_in_next_packet(&self) -> Result<usize, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::NextPacketFrames, |p| {
                Ok(p.frames_in_next_packet())
            })
    }
    pub fn maximum_frames_in_packet(&self) -> Result<usize, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::MaximumPacketFrames, |p| {
                Ok(p.maximum_frames_in_packet())
            })
    }
    pub fn target_bitrate_bps(&self) -> Result<i32, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::TargetBitrate, |p| {
                Ok(p.target_bitrate_bps())
            })
    }
    pub fn dtx(&self) -> Result<bool, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::Dtx, |p| Ok(p.dtx()))
    }
    pub fn reset(&self) -> Result<(), AudioEncoderProductError> {
        self.0.invoke(AudioEncoderProductOperation::Reset, |p| {
            p.reset().map_err(AudioEncoderProductFailureKind::Provider)
        })
    }
    pub fn set_fec(&self, enable: bool) -> Result<bool, AudioEncoderProductError> {
        self.0.invoke(AudioEncoderProductOperation::SetFec, |p| {
            p.set_fec(enable)
                .map_err(AudioEncoderProductFailureKind::Provider)
        })
    }
    pub fn set_dtx(&self, enable: bool) -> Result<bool, AudioEncoderProductError> {
        self.0.invoke(AudioEncoderProductOperation::SetDtx, |p| {
            p.set_dtx(enable)
                .map_err(AudioEncoderProductFailureKind::Provider)
        })
    }
    pub fn set_application(
        &self,
        application: AudioEncoderApplication,
    ) -> Result<bool, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::SetApplication, |p| {
                p.set_application(application)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn set_max_playback_rate(&self, frequency: i32) -> Result<(), AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::SetPlaybackRate, |p| {
                p.set_max_playback_rate(frequency)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn set_target_bitrate(&self, bitrate: i32) -> Result<(), AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::SetTargetBitrate, |p| {
                p.set_target_bitrate(bitrate)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn enable_audio_network_adaptor(
        &self,
        config: &[u8],
    ) -> Result<bool, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::EnableAdaptor, |p| {
                p.enable_audio_network_adaptor(config)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn disable_audio_network_adaptor(&self) -> Result<(), AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::DisableAdaptor, |p| {
                p.disable_audio_network_adaptor()
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn on_received_uplink_packet_loss_fraction(
        &self,
        value: f32,
    ) -> Result<(), AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::PacketLoss, |p| {
                p.on_received_uplink_packet_loss_fraction(value)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn on_received_uplink_recoverable_packet_loss_fraction(
        &self,
        value: f32,
    ) -> Result<(), AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::RecoverablePacketLoss, |p| {
                p.on_received_uplink_recoverable_packet_loss_fraction(value)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn on_received_target_audio_bitrate(
        &self,
        value: i32,
    ) -> Result<(), AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::ReceivedTargetBitrate, |p| {
                p.on_received_target_audio_bitrate(value)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn on_received_uplink_bandwidth(
        &self,
        value: i32,
        period: Option<i64>,
    ) -> Result<(), AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::UplinkBandwidth, |p| {
                p.on_received_uplink_bandwidth(value, period)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn on_received_uplink_allocation(
        &self,
        value: BitrateAllocationUpdate,
    ) -> Result<(), AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::UplinkAllocation, |p| {
                p.on_received_uplink_allocation(value)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            })
    }
    pub fn on_received_rtt(&self, value: i32) -> Result<(), AudioEncoderProductError> {
        self.0.invoke(AudioEncoderProductOperation::Rtt, |p| {
            p.on_received_rtt(value)
                .map_err(AudioEncoderProductFailureKind::Provider)
        })
    }
    pub fn on_received_overhead(&self, value: usize) -> Result<(), AudioEncoderProductError> {
        self.0.invoke(AudioEncoderProductOperation::Overhead, |p| {
            p.on_received_overhead(value)
                .map_err(AudioEncoderProductFailureKind::Provider)
        })
    }
    pub fn set_receiver_frame_length_range(
        &self,
        minimum: i32,
        maximum: i32,
    ) -> Result<(), AudioEncoderProductError> {
        self.0.invoke(
            AudioEncoderProductOperation::ReceiverFrameLengthRange,
            |p| {
                p.set_receiver_frame_length_range(minimum, maximum)
                    .map_err(AudioEncoderProductFailureKind::Provider)
            },
        )
    }
    pub fn network_adaptation_statistics(
        &self,
    ) -> Result<AudioNetworkAdaptationStatistics, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::Statistics, |p| {
                Ok(p.network_adaptation_statistics())
            })
    }
    pub fn frame_length_range(
        &self,
    ) -> Result<Option<AudioEncoderFrameLengthRange>, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::FrameLengthRange, |p| {
                Ok(p.frame_length_range())
            })
    }
    pub fn bitrate_range(
        &self,
    ) -> Result<Option<AudioEncoderBitrateRange>, AudioEncoderProductError> {
        self.0
            .invoke(AudioEncoderProductOperation::BitrateRange, |p| {
                Ok(p.bitrate_range())
            })
    }
}
