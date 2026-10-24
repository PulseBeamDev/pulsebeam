//! Standalone actual SDK AudioDecoder virtual drivers. Public legacy APIs stay
//! excluded; the mandatory internal path deliberately refuses without writes.
//! Persistent standalone SDK owners are also available, not selected factory
//! products or NetEq installation.
use crate::audio_decoder_frame_sdk::RustCallerAudioFrameCall;
use crate::{
    AudioDecoderProductError, AudioDecoderProductFailureKind, AudioDecoderProductFailureObserver,
    AudioDecoderProductOperation, CallerAudioDecoder, CallerDecodableAudioFrame,
    CallerParsedAudioFrame, ffi,
};
use std::sync::{Arc, Mutex};
type Outcome = Arc<Mutex<Option<AudioDecoderProductError>>>;
pub(crate) struct RustCallerAudioDecoderCall {
    decoder: CallerAudioDecoder,
    outcome: Outcome,
    max_appended: usize,
}
/// Nullable parse transfer storage, not a fabricated frame for native null.
pub(crate) struct RustCallerParsedFrame {
    frame: Option<CallerDecodableAudioFrame>,
    outcome: Outcome,
}
impl RustCallerAudioDecoderCall {
    fn report(&self, error: AudioDecoderProductError) {
        self.decoder
            .failure_observer()
            .report(error.operation, error.kind);
        self.outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(error);
    }
}
fn receipt(
    outcome: &Mutex<Option<AudioDecoderProductError>>,
) -> Result<(), AudioDecoderProductError> {
    match *outcome.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
impl CallerAudioDecoder {
    /// Retain one actual SDK decoder across standalone calls. Construction does
    /// not query provider metadata or admit this decoder into a factory/NetEq.
    pub fn sdk_decoder(&self) -> CallerAudioDecoderSdk {
        let (owner, _) = self.sdk_call(0);
        CallerAudioDecoderSdk(Arc::new(DecoderSdkState {
            native: Mutex::new(ffi::audio_caller_decoder_product_new(owner)),
            decoder: self.clone(),
        }))
    }
    fn sdk_call(&self, max_appended: usize) -> (Box<RustCallerAudioDecoderCall>, Outcome) {
        let outcome = Arc::new(Mutex::new(None));
        (
            Box::new(RustCallerAudioDecoderCall {
                decoder: self.clone(),
                outcome: outcome.clone(),
                max_appended,
            }),
            outcome,
        )
    }
    pub fn sample_rate_hz_via_sdk(&self) -> Result<i32, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call(0);
        let value = ffi::audio_caller_decoder_sdk_scalar(call, 0);
        receipt(&outcome)?;
        Ok(value)
    }
    pub fn channels_via_sdk(&self) -> Result<usize, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call(0);
        let value = ffi::audio_caller_decoder_sdk_channels(call);
        receipt(&outcome)?;
        Ok(value)
    }
    pub fn error_code_via_sdk(&self) -> Result<i32, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call(0);
        let value = ffi::audio_caller_decoder_sdk_scalar(call, 1);
        receipt(&outcome)?;
        Ok(value)
    }
    pub fn reset_via_sdk(&self) -> Result<(), AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call(0);
        ffi::audio_caller_decoder_sdk_reset(call);
        receipt(&outcome)
    }
    pub fn packet_duration_via_sdk(&self, encoded: &[u8]) -> Result<i32, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call(0);
        let value = ffi::audio_caller_decoder_sdk_packet_duration(call, encoded, false);
        receipt(&outcome)?;
        Ok(value)
    }
    pub fn redundant_packet_duration_via_sdk(
        &self,
        encoded: &[u8],
    ) -> Result<i32, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call(0);
        let value = ffi::audio_caller_decoder_sdk_packet_duration(call, encoded, true);
        receipt(&outcome)?;
        Ok(value)
    }
    pub fn packet_has_fec_via_sdk(&self, encoded: &[u8]) -> Result<bool, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call(0);
        let value = ffi::audio_caller_decoder_sdk_packet_fec(call, encoded);
        receipt(&outcome)?;
        Ok(value)
    }
    /// Real SDK ParsePayload. The SDK decoder and its native frame envelopes
    /// coexist through this synchronous call; returned Rust leases retain actual
    /// caller decoder/environment state after both transient SDK owners retire.
    /// SDK timestamps/priorities/null frames round trip without prefiltering or
    /// stock parse/capacity assumptions. Callback failure is not empty success.
    pub fn parse_payload_via_sdk(
        &self,
        encoded: Vec<u8>,
        timestamp: u32,
    ) -> Result<Vec<CallerParsedAudioFrame>, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call(0);
        let parsed = ffi::audio_caller_decoder_sdk_parse(call, &encoded, timestamp);
        receipt(&outcome)?;
        Ok(parsed
            .into_iter()
            .map(|item| CallerParsedAudioFrame {
                timestamp: item.timestamp,
                priority: item.priority,
                frame: item.transfer.frame,
            })
            .collect())
    }
    /// Actual SDK GeneratePlc into an owning native append buffer. The current
    /// operation carries the explicit bound. Error never replaces the caller's
    /// prefix with an SDK no-output disposition or manufactured concealment.
    pub fn generate_plc_via_sdk(
        &self,
        requested: usize,
        prefix: &mut Vec<i16>,
        max_appended: usize,
    ) -> Result<(), AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call(max_appended);
        let value = ffi::audio_caller_decoder_sdk_plc(call, requested, prefix);
        receipt(&outcome)?;
        *prefix = value.samples;
        Ok(())
    }
}
struct DecoderSdkState {
    // Native SDK owner/its Rust box retire before the retaining caller decoder.
    native: Mutex<cxx::UniquePtr<ffi::NativeCallerAudioDecoderProduct>>,
    decoder: CallerAudioDecoder,
}
// SAFETY: the SDK object and its scoped request-pointer mutation are private and
// serialized by native's mutex; mutable Rust callbacks/frame lifetime operations
// also take the original decoder mutex. Requests are synchronous and outputs own
// data/leases. No native decoder pointer or unchecked generic output bound escapes.
unsafe impl Send for DecoderSdkState {}
unsafe impl Sync for DecoderSdkState {}
/// Persistent owning standalone caller SDK AudioDecoder. Clones share one actual
/// SDK instance and original caller DSP state. This is not AudioDecoder, a factory
/// return type or an installable peer/NetEq dependency. Internal legacy refuses.
#[derive(Clone)]
pub struct CallerAudioDecoderSdk(Arc<DecoderSdkState>);
impl CallerAudioDecoderSdk {
    pub fn failure_observer(&self) -> AudioDecoderProductFailureObserver {
        self.0.decoder.failure_observer()
    }
    fn invoke<T>(
        &self,
        operation: AudioDecoderProductOperation,
        max_appended: usize,
        call: impl FnOnce(&ffi::NativeCallerAudioDecoderProduct, &RustCallerAudioDecoderCall) -> T,
    ) -> Result<T, AudioDecoderProductError> {
        // Keyed by caller decoder, so distinct SDK handles cannot recursively
        // acquire each other's native mutex while sharing the same DSP mutex.
        let _scope = self.0.decoder.sdk_scope(operation)?;
        let native = self.0.native.lock().unwrap_or_else(|e| e.into_inner());
        self.0.decoder.sdk_entry(operation)?; // Recheck panic after waiting.
        let (source, outcome) = self.0.decoder.sdk_call(max_appended);
        let value = call(native.as_ref().expect("owned caller SDK decoder"), &source);
        receipt(&outcome)?;
        Ok(value)
    }
    pub fn sample_rate_hz(&self) -> Result<i32, AudioDecoderProductError> {
        self.invoke(AudioDecoderProductOperation::SampleRate, 0, |n, c| {
            ffi::audio_caller_decoder_product_scalar(n, c, 0)
        })
    }
    pub fn channels(&self) -> Result<usize, AudioDecoderProductError> {
        self.invoke(
            AudioDecoderProductOperation::Channels,
            0,
            ffi::audio_caller_decoder_product_channels,
        )
    }
    pub fn error_code(&self) -> Result<i32, AudioDecoderProductError> {
        self.invoke(AudioDecoderProductOperation::ErrorCode, 0, |n, c| {
            ffi::audio_caller_decoder_product_scalar(n, c, 1)
        })
    }
    pub fn reset(&self) -> Result<(), AudioDecoderProductError> {
        self.invoke(
            AudioDecoderProductOperation::Reset,
            0,
            ffi::audio_caller_decoder_product_reset,
        )
    }
    pub fn packet_duration(&self, encoded: &[u8]) -> Result<i32, AudioDecoderProductError> {
        self.invoke(AudioDecoderProductOperation::PacketDuration, 0, |n, c| {
            ffi::audio_caller_decoder_product_duration(n, c, encoded, false)
        })
    }
    pub fn redundant_packet_duration(
        &self,
        encoded: &[u8],
    ) -> Result<i32, AudioDecoderProductError> {
        self.invoke(
            AudioDecoderProductOperation::RedundantPacketDuration,
            0,
            |n, c| ffi::audio_caller_decoder_product_duration(n, c, encoded, true),
        )
    }
    pub fn packet_has_fec(&self, encoded: &[u8]) -> Result<bool, AudioDecoderProductError> {
        self.invoke(AudioDecoderProductOperation::PacketHasFec, 0, |n, c| {
            ffi::audio_caller_decoder_product_fec(n, c, encoded)
        })
    }
    /// Native leaves retire before this operation returns; extracted original
    /// Rust frame leases independently retain caller decoder/environment state.
    pub fn parse_payload(
        &self,
        encoded: Vec<u8>,
        timestamp: u32,
    ) -> Result<Vec<CallerParsedAudioFrame>, AudioDecoderProductError> {
        let parsed = self.invoke(AudioDecoderProductOperation::ParsePayload, 0, |n, c| {
            ffi::audio_caller_decoder_product_parse(n, c, &encoded, timestamp)
        })?;
        Ok(parsed
            .into_iter()
            .map(|item| CallerParsedAudioFrame {
                timestamp: item.timestamp,
                priority: item.priority,
                frame: item.transfer.frame,
            })
            .collect())
    }
    pub fn generate_plc(
        &self,
        requested: usize,
        prefix: &mut Vec<i16>,
        max_appended: usize,
    ) -> Result<(), AudioDecoderProductError> {
        let value = self.invoke(
            AudioDecoderProductOperation::GeneratePlc,
            max_appended,
            |n, c| ffi::audio_caller_decoder_product_plc(n, c, requested, prefix),
        )?;
        *prefix = value.samples;
        Ok(())
    }
}
pub(crate) fn audio_caller_decoder_scalar_callback(
    source: &RustCallerAudioDecoderCall,
    kind: u8,
) -> i32 {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    let result = match kind {
        0 => source.decoder.sample_rate_hz(),
        _ => source.decoder.error_code(),
    };
    match result {
        Ok(value) => value,
        Err(e) => {
            source.report(e);
            0
        }
    }
}
pub(crate) fn audio_caller_decoder_channels_callback(source: &RustCallerAudioDecoderCall) -> usize {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source.decoder.channels() {
        Ok(value) => value,
        Err(e) => {
            source.report(e);
            0
        }
    }
}
pub(crate) fn audio_caller_decoder_reset_callback(source: &RustCallerAudioDecoderCall) {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    if let Err(e) = source.decoder.reset() {
        source.report(e);
    }
}
pub(crate) fn audio_caller_decoder_packet_duration_callback(
    source: &RustCallerAudioDecoderCall,
    encoded: &[u8],
    redundant: bool,
) -> i32 {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    let result = if redundant {
        source.decoder.redundant_packet_duration(encoded)
    } else {
        source.decoder.packet_duration(encoded)
    };
    match result {
        Ok(value) => value,
        Err(e) => {
            source.report(e);
            0
        }
    }
}
pub(crate) fn audio_caller_decoder_packet_fec_callback(
    source: &RustCallerAudioDecoderCall,
    encoded: &[u8],
) -> bool {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source.decoder.packet_has_fec(encoded) {
        Ok(value) => value,
        Err(e) => {
            source.report(e);
            false
        }
    }
}
pub(crate) fn audio_caller_decoder_parse_callback(
    source: &RustCallerAudioDecoderCall,
    encoded: Vec<u8>,
    timestamp: u32,
) -> Vec<ffi::FfiCallerParsedAudioFrame> {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source.decoder.parse_payload(encoded, timestamp) {
        Ok(parsed) => parsed
            .into_iter()
            .map(|item| ffi::FfiCallerParsedAudioFrame {
                timestamp: item.timestamp,
                priority: item.priority,
                transfer: Box::new(RustCallerParsedFrame {
                    frame: item.frame,
                    outcome: source.outcome.clone(),
                }),
            })
            .collect(),
        Err(e) => {
            source.report(e);
            Vec::new()
        }
    }
}
pub(crate) fn audio_caller_parsed_frame_present(source: &RustCallerParsedFrame) -> bool {
    source.frame.is_some()
}
#[allow(
    clippy::boxed_local,
    reason = "CXX transfers ownership through this fixed Box ABI."
)]
pub(crate) fn audio_caller_parsed_frame_bind(
    source: Box<RustCallerParsedFrame>,
) -> Box<RustCallerAudioFrameCall> {
    Box::new(RustCallerAudioFrameCall {
        frame: source.frame.expect("present SDK parse-frame transfer"),
        outcome: source.outcome,
    })
}
#[allow(
    clippy::boxed_local,
    reason = "CXX transfers ownership through this fixed Box ABI."
)]
pub(crate) fn audio_caller_parsed_frame_from_sdk(
    source: Box<RustCallerAudioFrameCall>,
) -> Box<RustCallerParsedFrame> {
    Box::new(RustCallerParsedFrame {
        frame: Some(source.frame),
        outcome: source.outcome,
    })
}
pub(crate) fn audio_caller_parsed_frame_null(
    source: &RustCallerAudioDecoderCall,
) -> Box<RustCallerParsedFrame> {
    Box::new(RustCallerParsedFrame {
        frame: None,
        outcome: source.outcome.clone(),
    })
}
pub(crate) fn audio_caller_decoder_plc_callback(
    source: &RustCallerAudioDecoderCall,
    requested: usize,
    prefix: &[i16],
) -> ffi::FfiAudioPlc {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    let mut samples = prefix.to_vec();
    match source
        .decoder
        .generate_plc(requested, &mut samples, source.max_appended)
    {
        Ok(()) => ffi::FfiAudioPlc {
            valid: true,
            samples,
        },
        Err(e) => {
            source.report(e);
            ffi::FfiAudioPlc {
                valid: false,
                samples: Vec::new(),
            }
        }
    }
}
pub(crate) fn audio_caller_decoder_plc_capacity(source: &RustCallerAudioDecoderCall) -> usize {
    source.max_appended
}
pub(crate) fn audio_caller_decoder_compatibility_refused(source: &RustCallerAudioDecoderCall) {
    source.report(AudioDecoderProductError {
        operation: AudioDecoderProductOperation::NativeCompatibility,
        kind: AudioDecoderProductFailureKind::UnsupportedCompatibility,
    });
}
pub(crate) fn audio_caller_decoder_plc_invalid(source: &RustCallerAudioDecoderCall) {
    source.report(AudioDecoderProductError {
        operation: AudioDecoderProductOperation::GeneratePlc,
        kind: AudioDecoderProductFailureKind::InvalidOutput,
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AudioDecoderError, AudioDecoderProvider, AudioDecoderProviderParseResult, Environment,
    };
    struct CompatibilityProbe;
    impl AudioDecoderProvider for CompatibilityProbe {
        fn sample_rate_hz(&mut self) -> i32 {
            8000
        }
        fn channels(&mut self) -> usize {
            1
        }
        fn reset(&mut self) -> Result<(), AudioDecoderError> {
            Ok(())
        }
        fn parse_payload(
            &mut self,
            _: Vec<u8>,
            _: u32,
        ) -> Result<Vec<AudioDecoderProviderParseResult>, AudioDecoderError> {
            panic!("legacy refused before modern parser")
        }
        fn generate_plc(
            &mut self,
            _: usize,
            _: &[i16],
            _: usize,
        ) -> Result<Vec<i16>, AudioDecoderError> {
            panic!("legacy refused before PLC")
        }
    }
    #[test]
    fn actual_sdk_internal_legacy_refusal_returns_negative_without_pcm_or_speech_writes() {
        let environment = Environment::builder().build().unwrap();
        let decoder = CallerAudioDecoder::from_provider(&environment, CompatibilityProbe);
        for redundant in [false, true] {
            let (call, outcome) = decoder.sdk_call(0);
            let mut output = [123; 160];
            let mut speech = 2;
            assert_eq!(
                ffi::audio_caller_decoder_sdk_legacy_refusal(
                    call,
                    redundant,
                    &mut output,
                    &mut speech
                ),
                -1
            );
            assert_eq!(output, [123; 160]);
            assert_eq!(speech, 2);
            assert_eq!(
                receipt(&outcome),
                Err(AudioDecoderProductError {
                    operation: AudioDecoderProductOperation::NativeCompatibility,
                    kind: AudioDecoderProductFailureKind::UnsupportedCompatibility,
                })
            );
        }
        assert!(!decoder.failure_observer().is_disabled());
        assert_eq!(decoder.sample_rate_hz(), Ok(8000));
    }
}
