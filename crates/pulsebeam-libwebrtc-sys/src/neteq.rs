//! Uniquely owned, serialized NetEq product.
//!
//! Registration stays lazy. Stock-only guards reject unsafe invocations without
//! selecting replacement decoders or changing caller metadata. Native outcomes
//! and normalization remain native.
use crate::{
    AudioDecoderFactory, AudioFrame, ClockTimeBounds, Environment, NetEqConfig, NetEqDecoderFormat,
    NetEqLifetimeStatistics, NetEqNetworkStatistics, NetEqOperation, NetEqOperationsAndState,
    NetEqReturnCode, RtpHeaderValues, RtpReceivedPacketInfo, SdpAudioFormat, Timestamp, ffi,
};
use std::{collections::BTreeMap, marker::PhantomData, rc::Rc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetEqError {
    InvalidStockConfiguration,
    InvalidCodecMap,
    InvalidOperation,
    /// Stock RemoveAll leaves queued frames holding raw decoder pointers.
    /// Explicitly flush the buffers or consume their packets before removing all.
    BufferedPackets,
    /// The lifetime clock bounds do not prove finite native subtraction.
    ReceiveTimeOutOfRange,
}
impl std::fmt::Display for NetEqError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for NetEqError {}

/// Thread-compatible, not thread-safe. Mutable access serializes all calls.
/// The environment and decoder-factory dependencies outlive the product.
/// This resource deliberately cannot be sent or shared across Rust threads.
pub struct NetEq {
    native: cxx::UniquePtr<ffi::NativeNetEq>,
    _environment: Environment,
    clock_bounds: ClockTimeBounds,
    failure_observer: Option<crate::NetEqFailureObserver>,
    _not_send_sync: PhantomData<Rc<()>>,
}
impl NetEq {
    pub(crate) fn from_factory_native(
        native: cxx::UniquePtr<ffi::NativeNetEq>,
        environment: Environment,
        observer: crate::NetEqFailureObserver,
    ) -> Self {
        let clock_bounds = environment.clock_time_bounds();
        Self {
            native,
            _environment: environment,
            clock_bounds,
            failure_observer: Some(observer),
            _not_send_sync: PhantomData,
        }
    }
    /// Construct a complete caller engine, with no stock engine or decoder
    /// substitution. Only GetAudio's initialized 10-ms frame egress is guarded;
    /// stock invocation/codec/receive-time policies do not constrain this engine.
    /// The native adapter retains the environment even after a future factory
    /// transfers the product into an SDK session. Callback panics fail closed.
    /// This does not install a peer factory; factory wiring is a separate step.
    pub fn from_provider(
        environment: &Environment,
        config: &NetEqConfig,
        provider: impl crate::NetEqProvider,
    ) -> Self {
        let environment = environment.clone();
        let clock_bounds = environment.clock_time_bounds();
        let observer = crate::NetEqFailureObserver::default();
        let source = crate::neteq_provider::RustNetEqProvider::new(
            Box::new(provider),
            config,
            observer.clone(),
        );
        let native = ffi::neteq_provider_create(environment.native(), Box::new(source));
        Self {
            native,
            _environment: environment,
            clock_bounds,
            failure_observer: Some(observer),
            _not_send_sync: PhantomData,
        }
    }
    /// Clone the durable first-failure observer for a caller product. This
    /// remains usable after product destruction. Explicit stock/failed native
    /// products have no caller callback observer; use is_failed for those.
    pub fn failure_observer(&self) -> Option<crate::NetEqFailureObserver> {
        self.failure_observer.clone()
    }
    /// Explicit permanently failed product, never a stock replacement. Native
    /// packet/audio calls fail without touching outputs; optional getters are
    /// absent and statistics/delays are zero. A consumer-safe configured rate
    /// (1000..=192000 Hz) is retained, otherwise the getter returns 8000 Hz.
    /// No clock is sampled and no decoder is created. Factory error handling
    /// will use this product rather than handing the receive channel a null.
    pub fn failed(environment: &Environment, config: &NetEqConfig) -> Self {
        let environment = environment.clone();
        let clock_bounds = environment.clock_time_bounds();
        Self {
            native: ffi::neteq_failed_create(config.sample_rate_hz),
            _environment: environment,
            clock_bounds,
            failure_observer: None,
            _not_send_sync: PhantomData,
        }
    }
    /// Whether this product is explicitly failed or a caller failure has latched.
    pub fn is_failed(&self) -> bool {
        ffi::neteq_is_failed(self.inner())
    }
    /// Construct the stock implementation, subject only to its safety guards.
    /// This does not register codecs or instantiate a speech decoder.
    pub fn stock(
        environment: &Environment,
        config: &NetEqConfig,
        decoder_factory: &AudioDecoderFactory,
    ) -> Result<Self, NetEqError> {
        Self::stock_with_decoder_factory(environment, config, Some(decoder_factory))
    }
    /// Preserve an explicitly absent native decoder factory. Registration stays
    /// lazy, but ordinary decoder creation and packet consumption are refused
    /// before the SDK dereferences a missing factory. No builtin is substituted.
    pub fn stock_with_decoder_factory(
        environment: &Environment,
        config: &NetEqConfig,
        decoder_factory: Option<&AudioDecoderFactory>,
    ) -> Result<Self, NetEqError> {
        let environment = environment.clone();
        // A durable range, not a sampled clock value. Default SDK current time
        // is signed int64 nanoseconds / 1000 even with testing time installed.
        let clock_bounds = environment.clock_time_bounds();
        let factory =
            decoder_factory.map_or(std::ptr::null(), |factory| factory.native() as *const _);
        // SAFETY: any selected owner remains alive for this synchronous call;
        // native construction retains it. Null is a distinct supported input.
        let native = unsafe {
            ffi::neteq_stock_create(
                environment.native(),
                &config.to_native(),
                factory,
                clock_bounds.minimum().microseconds(),
                clock_bounds.maximum().microseconds(),
            )
        };
        if native.is_null() {
            return Err(NetEqError::InvalidStockConfiguration);
        }
        Ok(Self {
            native,
            _environment: environment,
            clock_bounds,
            failure_observer: None,
            _not_send_sync: PhantomData,
        })
    }
    fn inner(&self) -> &ffi::NativeNetEq {
        self.native.as_ref().expect("live NetEq")
    }
    pub fn insert_packet(&mut self, header: &RtpHeaderValues, payload: &[u8]) -> NetEqReturnCode {
        NetEqReturnCode(ffi::neteq_insert_packet(
            self.native.pin_mut(),
            &header.to_native(),
            payload,
        ))
    }
    /// Inclusive clock bounds retained for this engine's entire lifetime.
    pub fn clock_time_bounds(&self) -> ClockTimeBounds {
        self.clock_bounds
    }
    /// Whether this timestamp proves finite native subtraction throughout use.
    /// Missing time (minus infinity) always passes. Unrestricted caller clocks
    /// permit only zero or missing time; bounded clocks permit a broader range.
    pub fn receive_time_is_safe(&self, receive_time: Timestamp) -> bool {
        crate::neteq_safety::receive_time_is_safe(self.clock_bounds, receive_time)
    }
    /// Calls the distinct timestamp overload, not the packet-info overload.
    /// Rejects an unprovable receive time before native insertion or mutation.
    /// Valid timestamps pass through unchanged, with no extra clock read.
    pub fn insert_packet_at(
        &mut self,
        header: &RtpHeaderValues,
        payload: &[u8],
        receive_time: Timestamp,
    ) -> Result<NetEqReturnCode, NetEqError> {
        if ffi::neteq_is_stock(self.inner()) && !self.receive_time_is_safe(receive_time) {
            return Err(NetEqError::ReceiveTimeOutOfRange);
        }
        Ok(NetEqReturnCode(ffi::neteq_insert_packet_at(
            self.native.pin_mut(),
            &header.to_native(),
            payload,
            receive_time.native_microsecond_bits(),
        )))
    }
    /// Retains all packet-info fields independently of the supplied RTP header.
    /// The stock receive-time guard does not erase or replace any metadata.
    pub fn insert_packet_with_info(
        &mut self,
        header: &RtpHeaderValues,
        payload: &[u8],
        info: &RtpReceivedPacketInfo,
    ) -> Result<NetEqReturnCode, NetEqError> {
        if ffi::neteq_is_stock(self.inner()) && !self.receive_time_is_safe(info.receive_time) {
            return Err(NetEqError::ReceiveTimeOutOfRange);
        }
        Ok(NetEqReturnCode(ffi::neteq_insert_packet_info(
            self.native.pin_mut(),
            &header.to_native(),
            payload,
            &info.to_native(),
        )))
    }
    /// Native failure does not imply that the frame fields describe valid audio.
    /// Output pointer presence is retained, and their prior values survive when
    /// native code leaves them untouched. A guard failure returns FAIL.
    #[allow(clippy::too_many_arguments)]
    pub fn get_audio(
        &mut self,
        frame: &mut AudioFrame,
        muted: Option<&mut bool>,
        sample_rate_hz: Option<&mut i32>,
        action_override: Option<NetEqOperation>,
    ) -> Result<NetEqReturnCode, NetEqError> {
        if ffi::neteq_is_stock(self.inner())
            && action_override.is_some_and(|v| !(0..=10).contains(&v.0))
        {
            return Err(NetEqError::InvalidOperation);
        }
        let result = ffi::neteq_get_audio(
            self.native.pin_mut(),
            frame.native_mut(),
            muted.is_some(),
            muted.as_deref().copied().unwrap_or(false),
            sample_rate_hz.is_some(),
            sample_rate_hz.as_deref().copied().unwrap_or(0),
            action_override.is_some(),
            action_override.map_or(0, |v| v.0),
        );
        if let Some(value) = muted {
            *value = result.muted;
        }
        if let Some(value) = sample_rate_hz {
            *value = result.sample_rate_hz;
        }
        Ok(NetEqReturnCode(result.code))
    }
    pub fn set_codecs(&mut self, codecs: &BTreeMap<i32, SdpAudioFormat>) -> Result<(), NetEqError> {
        let codecs: Vec<_> = codecs
            .iter()
            .map(|(&payload_type, format)| ffi::FfiNetEqCodec {
                payload_type,
                format: format.to_native(),
            })
            .collect();
        if ffi::neteq_set_codecs(self.native.pin_mut(), &codecs) {
            Ok(())
        } else {
            Err(NetEqError::InvalidCodecMap)
        }
    }
    pub fn register_payload_type(&mut self, payload_type: i32, format: &SdpAudioFormat) -> bool {
        ffi::neteq_register_payload_type(self.native.pin_mut(), payload_type, &format.to_native())
    }
    pub fn create_decoder(&mut self, payload_type: i32) -> bool {
        ffi::neteq_create_decoder(self.native.pin_mut(), payload_type)
    }
    pub fn remove_payload_type(&mut self, payload_type: u8) -> NetEqReturnCode {
        NetEqReturnCode(ffi::neteq_remove_payload_type(
            self.native.pin_mut(),
            payload_type,
        ))
    }
    /// Preserves native removal when the packet buffer is empty. Refuses a
    /// queued-packet removal without mutating codecs, frames, or timestamps.
    /// `flush_buffers` is an explicit, separate choice, never an implicit reset.
    pub fn remove_all_payload_types(&mut self) -> Result<(), NetEqError> {
        if ffi::neteq_remove_all_payload_types(self.native.pin_mut()) {
            Ok(())
        } else {
            Err(NetEqError::BufferedPackets)
        }
    }
    /// Retains the constructor's packet-count arithmetic bound after resizing.
    /// Unsafe counts are refused before buffer eviction or any native mutation.
    pub fn set_maximum_buffer_packets(&mut self, count: usize) -> Result<(), NetEqError> {
        if ffi::neteq_set_maximum_buffer_packets(self.native.pin_mut(), count) {
            Ok(())
        } else {
            Err(NetEqError::InvalidStockConfiguration)
        }
    }
    pub fn set_fast_accelerate(&mut self, enable: bool) {
        ffi::neteq_set_fast_accelerate(self.native.pin_mut(), enable);
    }
    pub fn set_minimum_delay(&mut self, delay_ms: i32) -> bool {
        ffi::neteq_set_minimum_delay(self.native.pin_mut(), delay_ms)
    }
    pub fn set_maximum_delay(&mut self, delay_ms: i32) -> bool {
        ffi::neteq_set_maximum_delay(self.native.pin_mut(), delay_ms)
    }
    pub fn set_base_minimum_delay_ms(&mut self, delay_ms: i32) -> bool {
        ffi::neteq_set_base_minimum_delay(self.native.pin_mut(), delay_ms)
    }
    pub fn base_minimum_delay_ms(&self) -> i32 {
        ffi::neteq_base_minimum_delay(self.inner())
    }
    pub fn target_delay_ms(&self) -> i32 {
        ffi::neteq_target_delay(self.inner())
    }
    pub fn filtered_current_delay_ms(&self) -> i32 {
        ffi::neteq_filtered_current_delay(self.inner())
    }
    /// Resetting statistics query, preserving the native return code.
    pub fn network_statistics(&mut self) -> (NetEqReturnCode, NetEqNetworkStatistics) {
        let result = ffi::neteq_network_statistics(self.native.pin_mut());
        (
            NetEqReturnCode(result.code),
            NetEqNetworkStatistics::from_native(result.value),
        )
    }
    pub fn current_network_statistics(&self) -> NetEqNetworkStatistics {
        NetEqNetworkStatistics::from_native(ffi::neteq_current_network_statistics(self.inner()))
    }
    pub fn lifetime_statistics(&self) -> NetEqLifetimeStatistics {
        NetEqLifetimeStatistics::from_native(ffi::neteq_lifetime_statistics(self.inner()))
    }
    pub fn operations_and_state(&self) -> NetEqOperationsAndState {
        NetEqOperationsAndState::from_native(ffi::neteq_operations_and_state(self.inner()))
    }
    pub fn playout_timestamp(&self) -> Option<u32> {
        let mut present = false;
        let value = ffi::neteq_playout_timestamp(self.inner(), &mut present);
        present.then_some(value)
    }
    pub fn last_output_sample_rate_hz(&self) -> i32 {
        ffi::neteq_last_output_sample_rate(self.inner())
    }
    pub fn current_decoder_format(&mut self) -> Option<NetEqDecoderFormat> {
        let result = ffi::neteq_current_decoder_format(self.native.pin_mut());
        result
            .present
            .then(|| NetEqDecoderFormat::from_native(result.value))
    }
    /// Calls the SDK's deprecated compatibility virtual independently.
    pub fn decoder_format(&self, payload_type: i32) -> Option<NetEqDecoderFormat> {
        let result = ffi::neteq_decoder_format(self.inner(), payload_type);
        result
            .present
            .then(|| NetEqDecoderFormat::from_native(result.value))
    }
    pub fn flush_buffers(&mut self) {
        ffi::neteq_flush_buffers(self.native.pin_mut());
    }
    pub fn sync_buffer_size_ms(&self) -> i32 {
        ffi::neteq_sync_buffer_size(self.inner())
    }
    pub fn insert_empty_packet(&mut self, header: &RtpHeaderValues) {
        ffi::neteq_insert_empty_packet(self.native.pin_mut(), &header.to_native());
    }
    pub fn enable_nack(&mut self, max_list_size: usize) {
        ffi::neteq_enable_nack(self.native.pin_mut(), max_list_size);
    }
    pub fn disable_nack(&mut self) {
        ffi::neteq_disable_nack(self.native.pin_mut());
    }
    pub fn nack_list(&self, round_trip_time_ms: i64) -> Vec<u16> {
        ffi::neteq_nack_list(self.inner(), round_trip_time_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clock, ClockProvider, FiniteTimestamp, NtpTime};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct CountingClock(AtomicUsize);
    impl ClockProvider for CountingClock {
        fn current_time(&self) -> FiniteTimestamp {
            self.0.fetch_add(1, Ordering::SeqCst);
            FiniteTimestamp::from_microseconds(0).unwrap()
        }
        fn convert_timestamp_to_ntp_time(&self, timestamp: Timestamp) -> NtpTime {
            NtpTime::from_raw(timestamp.native_microsecond_bits() as u64)
        }
    }

    #[test]
    fn failed_product_remains_callable_without_stock_or_output_mutation() {
        let provider = Arc::new(CountingClock(AtomicUsize::new(0)));
        let clock = Clock::from_provider(provider.clone());
        let environment = Environment::builder().clock_handle(&clock).build().unwrap();
        let calls = provider.0.load(Ordering::SeqCst);
        let mut config = NetEqConfig::default();
        config.sample_rate_hz = 16000;
        let mut engine = NetEq::failed(&environment, &config);
        assert!(engine.is_failed());
        assert!(!ffi::neteq_is_stock(engine.inner()));
        assert_eq!(engine.last_output_sample_rate_hz(), 16000);
        let mut frame = AudioFrame::new();
        frame
            .update_frame(
                11,
                Some(&[7; 80]),
                80,
                8000,
                crate::AudioFrameSpeechType::NORMAL_SPEECH,
                crate::AudioFrameVadActivity::ACTIVE,
                1,
            )
            .unwrap();
        let fields = frame.fields();
        let samples = frame.data().to_vec();
        let mut muted = true;
        let mut rate = -97;
        // Raw caller tags must not acquire a stock-operation whitelist.
        assert_eq!(
            engine
                .get_audio(
                    &mut frame,
                    Some(&mut muted),
                    Some(&mut rate),
                    Some(NetEqOperation(i32::MIN))
                )
                .unwrap(),
            NetEqReturnCode::FAIL
        );
        assert!(muted);
        assert_eq!(rate, -97);
        assert_eq!(frame.fields(), fields);
        assert_eq!(frame.data(), samples);
        let header = RtpHeaderValues::default();
        assert_eq!(engine.insert_packet(&header, &[]), NetEqReturnCode::FAIL);
        assert_eq!(
            engine
                .insert_packet_at(&header, &[], Timestamp::plus_infinity())
                .unwrap(),
            NetEqReturnCode::FAIL
        );
        assert!(
            !engine.register_payload_type(-1, &SdpAudioFormat::new(b"foreign".to_vec(), -1, 0))
        );
        assert!(!engine.create_decoder(96));
        assert_eq!(engine.remove_payload_type(96), NetEqReturnCode::FAIL);
        assert!(!engine.set_minimum_delay(20));
        assert!(!engine.set_maximum_delay(20));
        assert!(!engine.set_base_minimum_delay_ms(20));
        assert_eq!(engine.base_minimum_delay_ms(), 0);
        assert_eq!(engine.target_delay_ms(), 0);
        assert_eq!(engine.filtered_current_delay_ms(), 0);
        assert_eq!(engine.sync_buffer_size_ms(), 0);
        assert_eq!(
            engine.network_statistics(),
            (NetEqReturnCode::FAIL, NetEqNetworkStatistics::default())
        );
        assert_eq!(
            engine.current_network_statistics(),
            NetEqNetworkStatistics::default()
        );
        assert_eq!(
            engine.lifetime_statistics(),
            NetEqLifetimeStatistics::default()
        );
        assert_eq!(
            engine.operations_and_state(),
            NetEqOperationsAndState::default()
        );
        assert!(engine.current_decoder_format().is_none());
        assert!(engine.decoder_format(96).is_none());
        assert!(engine.playout_timestamp().is_none());
        engine.enable_nack(usize::MAX);
        assert!(engine.nack_list(i64::MAX).is_empty());
        engine.disable_nack();
        engine.set_fast_accelerate(true);
        engine.set_maximum_buffer_packets(usize::MAX).unwrap();
        engine.insert_empty_packet(&header);
        engine.remove_all_payload_types().unwrap();
        engine.flush_buffers();
        assert!(engine.is_failed());
        for invalid_rate in [i32::MIN, 0, 999, 192001, i32::MAX] {
            config.sample_rate_hz = invalid_rate;
            assert_eq!(
                NetEq::failed(&environment, &config).last_output_sample_rate_hz(),
                8000
            );
        }
        assert_eq!(provider.0.load(Ordering::SeqCst), calls);
    }

    #[test]
    fn native_virtual_insertion_cannot_bypass_durable_receive_time_guard() {
        let provider = Arc::new(CountingClock(AtomicUsize::new(0)));
        let bounds = ClockTimeBounds::new(
            FiniteTimestamp::from_microseconds(-10).unwrap(),
            FiniteTimestamp::from_microseconds(10).unwrap(),
        )
        .unwrap();
        let clock = Clock::from_bounded_provider(provider.clone(), bounds);
        let environment = Environment::builder().clock_handle(&clock).build().unwrap();
        let factory = AudioDecoderFactory::builtin().unwrap();
        let mut engine = NetEq::stock(&environment, &NetEqConfig::default(), &factory).unwrap();
        assert!(engine.register_payload_type(96, &SdpAudioFormat::new(b"PCMU".to_vec(), 8000, 1)));
        let header = RtpHeaderValues {
            payload_type: 96,
            timestamp: 1000,
            sequence_number: 1,
            ..Default::default()
        };
        let before = engine.operations_and_state();
        let calls = provider.0.load(Ordering::SeqCst);
        for receive in [i64::MAX, i64::MAX - 1, i64::MIN + 1] {
            // Deliberately bypass both public Rust timestamp insertion methods.
            assert_eq!(
                ffi::neteq_insert_packet_at(
                    engine.native.pin_mut(),
                    &header.to_native(),
                    &[0xff; 160],
                    receive
                ),
                NetEqReturnCode::FAIL.0
            );
            assert_eq!(engine.operations_and_state(), before);
            assert_eq!(provider.0.load(Ordering::SeqCst), calls);
        }
        assert!(engine.receive_time_is_safe(Timestamp::minus_infinity()));
        assert!(engine.receive_time_is_safe(Timestamp::zero()));
        assert_eq!(provider.0.load(Ordering::SeqCst), calls);
    }
}
