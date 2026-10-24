//! Complete serialized caller NetEq product callbacks.
use crate::{
    AudioFrameMut, NetEqConfig, NetEqDecoderFormat, NetEqLifetimeStatistics,
    NetEqNetworkStatistics, NetEqOperation, NetEqOperationsAndState, NetEqReturnCode,
    RtpHeaderValues, RtpReceivedPacketInfo, SdpAudioFormat, Timestamp, ffi,
};
use std::{
    collections::BTreeMap,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
};

/// Scoped output-only native slot. Omitted pointers remain None. Reading an
/// arbitrary SDK output pointer before writing it would read uninitialized
/// memory, so this view exposes only writes made during this callback. Leaving
/// it unwritten preserves the caller's storage, including on native failure.
/// It cannot be retained past the callback or substituted with an owning slot.
/// ```compile_fail
/// use pulsebeam_webrtc_sys::NetEqOutput;
/// fn retain<'a>(output: NetEqOutput<'a, bool>) -> NetEqOutput<'static, bool> {
///     output
/// }
/// ```
pub struct NetEqOutput<'a, T> {
    pending: &'a mut Option<T>,
}
impl<T> NetEqOutput<'_, T> {
    pub fn write(&mut self, value: T) {
        *self.pending = Some(value);
    }
    pub fn written_value(&self) -> Option<&T> {
        self.pending.as_ref()
    }
}

/// Complete native product contract. The adapter serializes all calls,
/// including const getters, so a provider needs Send but not Sync. Native
/// callbacks may migrate between threads. Never reenter or wait for the caller
/// session, and perform only caller-driven work in controlled environments.
///
/// The three insertion methods are distinct. Their defaults follow the SDK's
/// header -> timestamp -> packet-info delegation without losing metadata.
/// Implement packet-info insertion, or override the other overloads as well.
/// GetAudio receives a scoped view of the original frame, never a staged copy.
/// A successful output must describe initialized, bounded 10-ms audio; failure
/// may leave partially mutated frame fields just as the native contract allows.
/// No stock codec whitelist, packet-count policy or operation-tag filtering is
/// applied to this product. A panic permanently latches observable failure.
pub trait NetEqProvider: Send + 'static {
    fn insert_packet(&mut self, header: &RtpHeaderValues, payload: &[u8]) -> NetEqReturnCode {
        self.insert_packet_at(header, payload, Timestamp::minus_infinity())
    }
    fn insert_packet_at(
        &mut self,
        header: &RtpHeaderValues,
        payload: &[u8],
        receive_time: Timestamp,
    ) -> NetEqReturnCode {
        match RtpReceivedPacketInfo::from_header(header, receive_time) {
            Ok(info) => self.insert_packet_with_info(header, payload, &info),
            Err(_) => NetEqReturnCode::FAIL,
        }
    }
    fn insert_packet_with_info(
        &mut self,
        header: &RtpHeaderValues,
        payload: &[u8],
        info: &RtpReceivedPacketInfo,
    ) -> NetEqReturnCode;
    fn insert_empty_packet(&mut self, _header: &RtpHeaderValues) {}
    fn get_audio(
        &mut self,
        frame: &mut AudioFrameMut<'_>,
        muted: Option<NetEqOutput<'_, bool>>,
        current_sample_rate_hz: Option<NetEqOutput<'_, i32>>,
        action_override: Option<NetEqOperation>,
    ) -> NetEqReturnCode;
    fn set_codecs(&mut self, codecs: &BTreeMap<i32, SdpAudioFormat>);
    fn register_payload_type(&mut self, payload_type: i32, format: &SdpAudioFormat) -> bool;
    fn create_decoder(&mut self, _payload_type: i32) -> bool {
        false
    }
    fn remove_payload_type(&mut self, payload_type: u8) -> NetEqReturnCode;
    fn remove_all_payload_types(&mut self);
    fn set_maximum_buffer_packets(&mut self, count: usize);
    fn set_fast_accelerate(&mut self, enable: bool);
    fn set_minimum_delay(&mut self, delay_ms: i32) -> bool;
    fn set_maximum_delay(&mut self, delay_ms: i32) -> bool;
    fn set_base_minimum_delay_ms(&mut self, delay_ms: i32) -> bool;
    fn base_minimum_delay_ms(&self) -> i32;
    fn target_delay_ms(&self) -> i32;
    fn filtered_current_delay_ms(&self) -> i32;
    fn network_statistics(
        &mut self,
        stats: Option<NetEqOutput<'_, NetEqNetworkStatistics>>,
    ) -> NetEqReturnCode;
    fn current_network_statistics(&self) -> NetEqNetworkStatistics;
    fn lifetime_statistics(&self) -> NetEqLifetimeStatistics;
    fn operations_and_state(&self) -> NetEqOperationsAndState;
    fn playout_timestamp(&self) -> Option<u32>;
    fn last_output_sample_rate_hz(&self) -> i32;
    fn decoder_format(&self, _payload_type: i32) -> Option<NetEqDecoderFormat> {
        None
    }
    fn current_decoder_format(&self) -> Option<NetEqDecoderFormat> {
        None
    }
    fn flush_buffers(&mut self);
    fn enable_nack(&mut self, _max_list_size: usize) {}
    fn disable_nack(&mut self) {}
    fn nack_list(&self, _round_trip_time_ms: i64) -> Vec<u16> {
        Vec::new()
    }
    fn sync_buffer_size_ms(&self) -> i32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetEqFailure {
    Panicked,
    InvalidAudioOutput,
    InvalidSdkOutput,
    ReentrantCall,
}
/// Durable first-failure observation, independent of product lifetime.
#[derive(Clone, Default)]
pub struct NetEqFailureObserver(Arc<AtomicU8>);
impl NetEqFailureObserver {
    pub fn failure(&self) -> Option<NetEqFailure> {
        match self.0.load(Ordering::Acquire) {
            1 => Some(NetEqFailure::Panicked),
            2 => Some(NetEqFailure::InvalidAudioOutput),
            3 => Some(NetEqFailure::InvalidSdkOutput),
            4 => Some(NetEqFailure::ReentrantCall),
            _ => None,
        }
    }
    pub fn has_failed(&self) -> bool {
        self.0.load(Ordering::Acquire) != 0
    }
    fn report(&self, value: u8) {
        let _ = self
            .0
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire);
    }
}

pub(crate) struct RustNetEqProvider {
    provider: Mutex<Option<Box<dyn NetEqProvider>>>,
    observer: NetEqFailureObserver,
    fallback_rate: i32,
    sdk_consumer: bool,
    // Drop the product before releasing Rust-only creation dependency leases.
    creation_environment: Option<crate::Environment>,
}
use crate::provider_panic::catch_provider as catch_unwind;

impl RustNetEqProvider {
    pub(crate) fn new(
        provider: Box<dyn NetEqProvider>,
        config: &NetEqConfig,
        observer: NetEqFailureObserver,
    ) -> Self {
        Self::with_profile(provider, config, observer, false)
    }
    // Internal only until factory context and all SDK egress guards are wired.
    pub(crate) fn new_for_sdk(
        provider: Box<dyn NetEqProvider>,
        config: &NetEqConfig,
        observer: NetEqFailureObserver,
    ) -> Self {
        Self::with_profile(provider, config, observer, true)
    }
    fn with_profile(
        provider: Box<dyn NetEqProvider>,
        config: &NetEqConfig,
        observer: NetEqFailureObserver,
        sdk_consumer: bool,
    ) -> Self {
        let rate_valid = if sdk_consumer {
            crate::neteq_sdk_safety::last_output_rate_is_safe(config.sample_rate_hz)
        } else {
            (1000..=192000).contains(&config.sample_rate_hz)
        };
        Self {
            provider: Mutex::new(Some(provider)),
            observer,
            sdk_consumer,
            fallback_rate: if rate_valid {
                config.sample_rate_hz
            } else {
                8000
            },
            creation_environment: None,
        }
    }
    pub(crate) fn retain_creation_environment(&mut self, environment: crate::Environment) {
        self.creation_environment = Some(environment);
    }
    fn call<T>(&self, fallback: T, action: impl FnOnce(&mut dyn NetEqProvider) -> T) -> T {
        if self.observer.has_failed() {
            return fallback;
        }
        let Some(_scope) =
            crate::neteq_callback_scope::NetEqCallbackScope::enter(self as *const Self as usize)
        else {
            self.observer.report(4);
            return fallback;
        };
        let mut provider = self
            .provider
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.observer.has_failed() {
            return fallback;
        }
        match catch_unwind(AssertUnwindSafe(|| provider.as_deref_mut().map(action))) {
            Ok(Some(value)) if !self.observer.has_failed() => value,
            Ok(_) => fallback,
            Err(_) => {
                self.observer.report(1);
                fallback
            }
        }
    }
}
impl Drop for RustNetEqProvider {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if catch_unwind(AssertUnwindSafe(|| drop(provider))).is_err() {
            self.observer.report(1);
        }
    }
}

pub(crate) fn neteq_provider_failed(source: &RustNetEqProvider) -> bool {
    source.observer.has_failed()
}
pub(crate) fn neteq_provider_insert_packet(
    source: &RustNetEqProvider,
    header: ffi::FfiMediaRtpHeader,
    payload: &[u8],
) -> i32 {
    source.call(-1, |p| {
        p.insert_packet(&RtpHeaderValues::from_native(header), payload)
            .0
    })
}
pub(crate) fn neteq_provider_insert_packet_at(
    source: &RustNetEqProvider,
    header: ffi::FfiMediaRtpHeader,
    payload: &[u8],
    receive_time: i64,
) -> i32 {
    source.call(-1, |p| {
        p.insert_packet_at(
            &RtpHeaderValues::from_native(header),
            payload,
            Timestamp::from_native_microsecond_bits(receive_time),
        )
        .0
    })
}
pub(crate) fn neteq_provider_insert_packet_info(
    source: &RustNetEqProvider,
    header: ffi::FfiMediaRtpHeader,
    payload: &[u8],
    info: ffi::FfiRtpReceivedPacketInfo,
) -> i32 {
    source.call(-1, |p| {
        p.insert_packet_with_info(
            &RtpHeaderValues::from_native(header),
            payload,
            &RtpReceivedPacketInfo::from_native(info),
        )
        .0
    })
}
pub(crate) fn neteq_provider_insert_empty_packet(
    source: &RustNetEqProvider,
    header: ffi::FfiMediaRtpHeader,
) {
    source.call((), |p| {
        p.insert_empty_packet(&RtpHeaderValues::from_native(header))
    })
}
fn no_audio_output() -> ffi::FfiNetEqProviderAudioResult {
    ffi::FfiNetEqProviderAudioResult {
        code: -1,
        muted_written: false,
        muted: false,
        rate_written: false,
        sample_rate_hz: 0,
    }
}
pub(crate) fn neteq_provider_get_audio(
    source: &RustNetEqProvider,
    frame: Pin<&mut ffi::NativeAudioFrame>,
    initialized: bool,
    has_muted: bool,
    has_rate: bool,
    has_override: bool,
    operation: i32,
) -> ffi::FfiNetEqProviderAudioResult {
    source.call(no_audio_output(), |p| {
        let mut frame = if initialized {
            AudioFrameMut::from_initialized(frame)
        } else {
            AudioFrameMut::from_native(frame)
        };
        let mut muted = None;
        let mut rate = None;
        let code = p.get_audio(
            &mut frame,
            has_muted.then_some(NetEqOutput {
                pending: &mut muted,
            }),
            has_rate.then_some(NetEqOutput { pending: &mut rate }),
            has_override.then_some(NetEqOperation(operation)),
        );
        if code == NetEqReturnCode::OK {
            let fields = frame.fields();
            // NetEq's 10-ms frame contract, not the stock decoder rate policy.
            // SDK-consumer-specific resampler/getter guards are a separate gate.
            if fields.sample_rate_hz < 100
                || fields.samples_per_channel != fields.sample_rate_hz as usize / 100
                || fields.num_channels == 0
                || fields.num_channels > AudioFrameMut::MAX_CHANNELS
                || frame.data_view().is_err()
            {
                source.observer.report(2);
                return no_audio_output();
            }
            if source.sdk_consumer && !crate::neteq_sdk_safety::audio_fields_are_safe(&fields) {
                source.observer.report(3);
                return no_audio_output();
            }
        }
        ffi::FfiNetEqProviderAudioResult {
            code: code.0,
            muted_written: muted.is_some(),
            muted: muted.unwrap_or(false),
            rate_written: rate.is_some(),
            sample_rate_hz: rate.unwrap_or(0),
        }
    })
}
pub(crate) fn neteq_provider_set_codecs(
    source: &RustNetEqProvider,
    codecs: Vec<ffi::FfiNetEqCodec>,
) {
    source.call((), |p| {
        let codecs = codecs
            .into_iter()
            .map(|v| (v.payload_type, SdpAudioFormat::from_native(v.format)))
            .collect();
        p.set_codecs(&codecs)
    })
}
pub(crate) fn neteq_provider_register_payload_type(
    source: &RustNetEqProvider,
    payload_type: i32,
    format: ffi::FfiSdpAudioFormat,
) -> bool {
    source.call(false, |p| {
        p.register_payload_type(payload_type, &SdpAudioFormat::from_native(format))
    })
}
macro_rules! callback {
    ($name:ident, $method:ident, $fallback:expr $(, $arg:ident: $ty:ty)* => $result:ty) => {
        pub(crate) fn $name(source: &RustNetEqProvider $(, $arg: $ty)*) -> $result {
            source.call($fallback, |p| p.$method($($arg),*))
        }
    };
}
callback!(neteq_provider_create_decoder, create_decoder, false, payload_type: i32 => bool);
pub(crate) fn neteq_provider_remove_payload_type(
    source: &RustNetEqProvider,
    payload_type: u8,
) -> i32 {
    source.call(-1, |p| p.remove_payload_type(payload_type).0)
}
callback!(neteq_provider_remove_all_payload_types, remove_all_payload_types, () => ());
callback!(neteq_provider_set_maximum_buffer_packets, set_maximum_buffer_packets, (), count: usize => ());
callback!(neteq_provider_set_fast_accelerate, set_fast_accelerate, (), enable: bool => ());
callback!(neteq_provider_set_minimum_delay, set_minimum_delay, false, delay_ms: i32 => bool);
callback!(neteq_provider_set_maximum_delay, set_maximum_delay, false, delay_ms: i32 => bool);
callback!(neteq_provider_set_base_minimum_delay, set_base_minimum_delay_ms, false, delay_ms: i32 => bool);
callback!(neteq_provider_base_minimum_delay, base_minimum_delay_ms, 0 => i32);
callback!(neteq_provider_target_delay, target_delay_ms, 0 => i32);
callback!(neteq_provider_filtered_current_delay, filtered_current_delay_ms, 0 => i32);
pub(crate) fn neteq_provider_network_statistics(
    source: &RustNetEqProvider,
    has_stats: bool,
) -> ffi::FfiNetEqProviderStatisticsResult {
    source.call(
        ffi::FfiNetEqProviderStatisticsResult {
            code: -1,
            written: false,
            value: NetEqNetworkStatistics::default().to_native(),
        },
        |p| {
            let mut stats = None;
            let code = p.network_statistics(has_stats.then_some(NetEqOutput {
                pending: &mut stats,
            }));
            ffi::FfiNetEqProviderStatisticsResult {
                code: code.0,
                written: stats.is_some(),
                value: stats.unwrap_or_default().to_native(),
            }
        },
    )
}
macro_rules! statistics_callback {
    ($name:ident, $method:ident, $value:ident, $native:ident) => {
        pub(crate) fn $name(source: &RustNetEqProvider) -> ffi::$native {
            source.call($value::default().to_native(), |p| p.$method().to_native())
        }
    };
}
statistics_callback!(
    neteq_provider_current_network_statistics,
    current_network_statistics,
    NetEqNetworkStatistics,
    FfiNetEqNetworkStatistics
);
statistics_callback!(
    neteq_provider_lifetime_statistics,
    lifetime_statistics,
    NetEqLifetimeStatistics,
    FfiNetEqLifetimeStatistics
);
statistics_callback!(
    neteq_provider_operations_and_state,
    operations_and_state,
    NetEqOperationsAndState,
    FfiNetEqOperationsAndState
);
pub(crate) fn neteq_provider_playout_timestamp(
    source: &RustNetEqProvider,
    present: &mut bool,
) -> u32 {
    let result = source.call(None, |p| p.playout_timestamp());
    *present = result.is_some();
    result.unwrap_or(0)
}
pub(crate) fn neteq_provider_last_output_sample_rate(source: &RustNetEqProvider) -> i32 {
    source.call(source.fallback_rate, |p| {
        let rate = p.last_output_sample_rate_hz();
        if source.sdk_consumer && !crate::neteq_sdk_safety::last_output_rate_is_safe(rate) {
            source.observer.report(3);
        }
        rate
    })
}
fn decoder_format_result(value: Option<NetEqDecoderFormat>) -> ffi::FfiNetEqDecoderFormatResult {
    ffi::FfiNetEqDecoderFormatResult {
        present: value.is_some(),
        value: value
            .unwrap_or(NetEqDecoderFormat {
                payload_type: 0,
                sample_rate_hz: 0,
                num_channels: 0,
                sdp_format: SdpAudioFormat::new(Vec::new(), 0, 0),
            })
            .to_native(),
    }
}
fn checked_decoder_format_result(
    source: &RustNetEqProvider,
    value: Option<NetEqDecoderFormat>,
) -> ffi::FfiNetEqDecoderFormatResult {
    if source.sdk_consumer
        && value
            .as_ref()
            .is_some_and(|v| !crate::neteq_sdk_safety::decoder_format_is_safe(v))
    {
        source.observer.report(3);
        return decoder_format_result(None);
    }
    decoder_format_result(value)
}
pub(crate) fn neteq_provider_current_decoder_format(
    source: &RustNetEqProvider,
) -> ffi::FfiNetEqDecoderFormatResult {
    source.call(decoder_format_result(None), |p| {
        checked_decoder_format_result(source, p.current_decoder_format())
    })
}
pub(crate) fn neteq_provider_decoder_format(
    source: &RustNetEqProvider,
    payload_type: i32,
) -> ffi::FfiNetEqDecoderFormatResult {
    source.call(decoder_format_result(None), |p| {
        checked_decoder_format_result(source, p.decoder_format(payload_type))
    })
}
callback!(neteq_provider_flush_buffers, flush_buffers, () => ());
callback!(neteq_provider_sync_buffer_size, sync_buffer_size_ms, 0 => i32);
callback!(neteq_provider_enable_nack, enable_nack, (), max_list_size: usize => ());
callback!(neteq_provider_disable_nack, disable_nack, () => ());
callback!(neteq_provider_nack_list, nack_list, Vec::new(), round_trip_time_ms: i64 => Vec<u16>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioFrame, AudioFrameSpeechType, AudioFrameVadActivity};
    struct LeaveFrame;
    impl NetEqProvider for LeaveFrame {
        fn insert_packet_with_info(
            &mut self,
            _: &RtpHeaderValues,
            _: &[u8],
            _: &RtpReceivedPacketInfo,
        ) -> NetEqReturnCode {
            NetEqReturnCode::OK
        }
        fn get_audio(
            &mut self,
            _: &mut AudioFrameMut<'_>,
            muted: Option<NetEqOutput<'_, bool>>,
            rate: Option<NetEqOutput<'_, i32>>,
            _: Option<NetEqOperation>,
        ) -> NetEqReturnCode {
            if let Some(mut slot) = muted {
                slot.write(false);
            }
            if let Some(mut slot) = rate {
                slot.write(8000);
            }
            NetEqReturnCode::OK
        }
        fn set_codecs(&mut self, _: &BTreeMap<i32, SdpAudioFormat>) {}
        fn register_payload_type(&mut self, _: i32, _: &SdpAudioFormat) -> bool {
            true
        }
        fn remove_payload_type(&mut self, _: u8) -> NetEqReturnCode {
            NetEqReturnCode::OK
        }
        fn remove_all_payload_types(&mut self) {}
        fn set_maximum_buffer_packets(&mut self, _: usize) {}
        fn set_fast_accelerate(&mut self, _: bool) {}
        fn set_minimum_delay(&mut self, _: i32) -> bool {
            true
        }
        fn set_maximum_delay(&mut self, _: i32) -> bool {
            true
        }
        fn set_base_minimum_delay_ms(&mut self, _: i32) -> bool {
            true
        }
        fn base_minimum_delay_ms(&self) -> i32 {
            0
        }
        fn target_delay_ms(&self) -> i32 {
            0
        }
        fn filtered_current_delay_ms(&self) -> i32 {
            0
        }
        fn network_statistics(
            &mut self,
            _: Option<NetEqOutput<'_, NetEqNetworkStatistics>>,
        ) -> NetEqReturnCode {
            NetEqReturnCode::FAIL
        }
        fn current_network_statistics(&self) -> NetEqNetworkStatistics {
            NetEqNetworkStatistics {
                current_buffer_size_ms: u16::MAX,
                expand_rate: u16::MAX,
                mean_waiting_time_ms: i32::MIN,
                max_waiting_time_ms: i32::MAX,
                ..Default::default()
            }
        }
        fn lifetime_statistics(&self) -> NetEqLifetimeStatistics {
            NetEqLifetimeStatistics {
                total_samples_received: u64::MAX,
                jitter_buffer_delay_ms: u64::MAX,
                total_processing_delay_us: u64::MAX,
                interruption_count: i32::MIN,
                total_interruption_duration_ms: i32::MAX,
                ..Default::default()
            }
        }
        fn operations_and_state(&self) -> NetEqOperationsAndState {
            NetEqOperationsAndState {
                packet_buffer_flushes: u64::MAX,
                current_buffer_size_ms: u64::MAX,
                current_frame_size_ms: u64::MAX,
                ..Default::default()
            }
        }
        fn playout_timestamp(&self) -> Option<u32> {
            None
        }
        fn last_output_sample_rate_hz(&self) -> i32 {
            8000
        }
        fn flush_buffers(&mut self) {}
        fn sync_buffer_size_ms(&self) -> i32 {
            0
        }
    }
    fn frame(muted: bool) -> AudioFrame {
        let mut frame = AudioFrame::new();
        frame
            .update_frame(
                13,
                (!muted).then_some(&[7; 80][..]),
                80,
                8000,
                AudioFrameSpeechType::NORMAL_SPEECH,
                AudioFrameVadActivity::ACTIVE,
                1,
            )
            .unwrap();
        frame
    }
    #[test]
    fn callback_reentry_fails_closed_without_locking_or_overwriting_outputs() {
        let observer = NetEqFailureObserver::default();
        let source = RustNetEqProvider::new(
            Box::new(LeaveFrame),
            &NetEqConfig::default(),
            observer.clone(),
        );
        let mut frame = frame(false);
        let before = frame.fields();
        let result = source.call(17, |_| {
            let output =
                neteq_provider_get_audio(&source, frame.native_mut(), true, true, true, false, 0);
            assert_eq!(output.code, -1);
            assert!(!output.muted_written && !output.rate_written);
            99
        });
        assert_eq!(result, 17);
        assert_eq!(frame.fields(), before);
        assert_eq!(frame.data_view().unwrap(), &[7; 80]);
        assert_eq!(observer.failure(), Some(NetEqFailure::ReentrantCall));
        assert_eq!(
            source.call(23, |_| panic!("failed product must not dispatch")),
            23
        );
    }

    #[test]
    fn sdk_unknown_storage_cannot_gain_proof_from_shape_or_success_status() {
        let observer = NetEqFailureObserver::default();
        let source = RustNetEqProvider::new(
            Box::new(LeaveFrame),
            &NetEqConfig::default(),
            observer.clone(),
        );
        let mut frame = frame(false);
        let before = frame.fields();
        // Deliberately discard owner evidence to model SDK callback entry.
        let result =
            neteq_provider_get_audio(&source, frame.native_mut(), false, true, true, false, 0);
        assert_eq!(result.code, -1);
        assert!(!result.muted_written && !result.rate_written);
        assert_eq!(frame.fields(), before); // Constructing a view did not clear samples.
        assert_eq!(frame.data_view().unwrap(), &[7; 80]);
        assert_eq!(observer.failure(), Some(NetEqFailure::InvalidAudioOutput));
    }
    #[test]
    fn owner_initialization_and_sdk_muted_zero_storage_are_distinct_valid_proofs() {
        for (initialized, muted) in [(true, false), (false, true)] {
            let observer = NetEqFailureObserver::default();
            let source = RustNetEqProvider::new(
                Box::new(LeaveFrame),
                &NetEqConfig::default(),
                observer.clone(),
            );
            let mut frame = frame(muted);
            let result = neteq_provider_get_audio(
                &source,
                frame.native_mut(),
                initialized,
                true,
                true,
                false,
                0,
            );
            assert_eq!(result.code, 0);
            assert!(result.muted_written && result.rate_written);
            assert_eq!(result.sample_rate_hz, 8000);
            assert_eq!(observer.failure(), None);
        }
    }
    #[test]
    fn standalone_success_uses_frame_capacity_not_a_codec_or_resampler_rate_ceiling() {
        for (rate, channels) in [(384000, 2), (768099, 1)] {
            let observer = NetEqFailureObserver::default();
            let source = RustNetEqProvider::new(
                Box::new(LeaveFrame),
                &NetEqConfig::default(),
                observer.clone(),
            );
            let mut frame = AudioFrame::new();
            frame
                .update_frame(
                    13,
                    None,
                    rate as usize / 100,
                    rate,
                    AudioFrameSpeechType::NORMAL_SPEECH,
                    AudioFrameVadActivity::ACTIVE,
                    channels,
                )
                .unwrap();
            let result = neteq_provider_get_audio(
                &source,
                frame.native_mut(),
                false,
                false,
                false,
                false,
                0,
            );
            assert_eq!(result.code, 0);
            assert_eq!(frame.fields().sample_rate_hz, rate);
            assert!(!observer.has_failed());
        }
    }
    #[test]
    fn sdk_admission_is_separate_from_standalone_frame_validity() {
        for sdk in [false, true] {
            for (rate, channels, speech) in [
                (44100, 16, AudioFrameSpeechType::NORMAL_SPEECH),
                (4800, 1, AudioFrameSpeechType::NORMAL_SPEECH),
                (8000, 1, AudioFrameSpeechType::UNDEFINED),
            ] {
                let observer = NetEqFailureObserver::default();
                let source = RustNetEqProvider::with_profile(
                    Box::new(LeaveFrame),
                    &NetEqConfig::default(),
                    observer.clone(),
                    sdk,
                );
                let mut frame = AudioFrame::new();
                frame
                    .update_frame(
                        13,
                        None,
                        rate as usize / 100,
                        rate,
                        speech,
                        AudioFrameVadActivity::ACTIVE,
                        channels,
                    )
                    .unwrap();
                let before = frame.fields();
                let result = neteq_provider_get_audio(
                    &source,
                    frame.native_mut(),
                    false,
                    true,
                    true,
                    false,
                    0,
                );
                assert_eq!(result.code, if sdk { -1 } else { 0 });
                assert_eq!(result.muted_written, !sdk);
                assert_eq!(result.rate_written, !sdk);
                assert_eq!(frame.fields(), before);
                assert_eq!(
                    observer.failure(),
                    sdk.then_some(NetEqFailure::InvalidSdkOutput)
                );
            }
        }
    }
    #[test]
    fn sdk_decoder_metadata_failure_is_durable_and_raw_metadata_is_preserved() {
        for sdk in [false, true] {
            let observer = NetEqFailureObserver::default();
            let source = RustNetEqProvider::with_profile(
                Box::new(LeaveFrame),
                &NetEqConfig::default(),
                observer.clone(),
                sdk,
            );
            let value = NetEqDecoderFormat {
                payload_type: i32::MAX,
                sample_rate_hz: 48001,
                num_channels: i32::MIN,
                sdp_format: SdpAudioFormat::new(vec![0, 255], 48000, usize::MAX),
            };
            let result = source.call(decoder_format_result(None), |_| {
                checked_decoder_format_result(&source, Some(value.clone()))
            });
            assert_eq!(result.present, !sdk);
            if sdk {
                assert_eq!(observer.failure(), Some(NetEqFailure::InvalidSdkOutput));
                assert_eq!(
                    source.call(23, |_| panic!("failed source must not reenter provider")),
                    23
                );
                observer.report(1);
                assert_eq!(observer.failure(), Some(NetEqFailure::InvalidSdkOutput));
            } else {
                assert_eq!(NetEqDecoderFormat::from_native(result.value), value);
                assert!(!observer.has_failed());
            }
        }
    }
    #[test]
    fn sdk_statistics_preserve_raw_ranges_and_unwritten_reset_slots() {
        let observer = NetEqFailureObserver::default();
        let source = RustNetEqProvider::new_for_sdk(
            Box::new(LeaveFrame),
            &NetEqConfig::default(),
            observer.clone(),
        );
        assert_eq!(
            NetEqNetworkStatistics::from_native(neteq_provider_current_network_statistics(&source)),
            LeaveFrame.current_network_statistics()
        );
        assert_eq!(
            NetEqLifetimeStatistics::from_native(neteq_provider_lifetime_statistics(&source)),
            LeaveFrame.lifetime_statistics()
        );
        assert_eq!(
            NetEqOperationsAndState::from_native(neteq_provider_operations_and_state(&source)),
            LeaveFrame.operations_and_state()
        );
        for present in [false, true] {
            let stats = neteq_provider_network_statistics(&source, present);
            assert_eq!(stats.code, -1);
            assert!(!stats.written);
        }
        assert!(!observer.has_failed());
    }
    #[test]
    fn native_sdk_failed_product_is_non_stock_mixer_safe_and_preserves_outputs() {
        for rate in [i32::MIN, 1000, 8000, 44100, 48000, 48001, 192000, i32::MAX] {
            let mut product = ffi::neteq_failed_create_for_sdk(rate);
            let native = product.as_ref().unwrap();
            assert!(ffi::neteq_is_failed(native));
            assert!(!ffi::neteq_is_stock(native));
            assert_eq!(
                ffi::neteq_last_output_sample_rate(native),
                if (8000..=48000).contains(&rate) {
                    rate
                } else {
                    8000
                }
            );
            assert_eq!(ffi::neteq_filtered_current_delay(native), 0);
            assert!(!ffi::neteq_decoder_format(native, i32::MAX).present);
            let mut frame = frame(false);
            let before = frame.fields();
            let result = ffi::neteq_get_audio(
                product.pin_mut(),
                frame.native_mut(),
                true,
                true,
                true,
                -137,
                false,
                0,
            );
            assert_eq!(result.code, -1);
            assert!(result.muted);
            assert_eq!(result.sample_rate_hz, -137);
            assert_eq!(frame.fields(), before);
            assert_eq!(frame.data_view().unwrap(), &[7; 80]);
        }
    }
    #[test]
    fn sdk_failure_fallback_is_mixer_safe_without_replacing_the_provider() {
        for rate in [1000, 8000, 44100, 48000, 48001, 192000] {
            let config = NetEqConfig {
                sample_rate_hz: rate,
                ..Default::default()
            };
            let observer = NetEqFailureObserver::default();
            let source =
                RustNetEqProvider::new_for_sdk(Box::new(LeaveFrame), &config, observer.clone());
            observer.report(1);
            assert_eq!(
                neteq_provider_last_output_sample_rate(&source),
                if (8000..=48000).contains(&rate) {
                    rate
                } else {
                    8000
                }
            );
            assert_eq!(observer.failure(), Some(NetEqFailure::Panicked));
        }
    }
    #[test]
    fn omitted_slots_and_reset_statistics_failure_do_not_manufacture_writes() {
        let observer = NetEqFailureObserver::default();
        let source = RustNetEqProvider::new(
            Box::new(LeaveFrame),
            &NetEqConfig::default(),
            observer.clone(),
        );
        let mut frame = frame(true);
        let result = neteq_provider_get_audio(
            &source,
            frame.native_mut(),
            false,
            false,
            false,
            true,
            i32::MAX,
        );
        assert_eq!(result.code, 0);
        assert!(!result.muted_written && !result.rate_written);
        for present in [true, false] {
            let stats = neteq_provider_network_statistics(&source, present);
            assert_eq!(stats.code, -1);
            assert!(!stats.written);
        }
        assert!(!observer.has_failed());
    }
}
