//! Real standalone SDK EncodedAudioFrame virtuals, not a caller AudioDecoder
//! implementation or NetEq dependency. Every synchronous call owns a receipt.
use crate::{
    AudioDecoderProductError, AudioDecoderProductFailureKind, AudioDecoderProductOperation,
    AudioSpeechType, CallerDecodableAudioFrame, ffi,
};
use std::sync::{Arc, Mutex};

/// Exact SDK DecodeResult fields. The PCM lives in the caller's mutable span;
/// successful count is total interleaved samples, not samples per channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioDecoderFrameDecodeResult {
    pub num_decoded_samples: usize,
    pub speech_type: AudioSpeechType,
}
pub(crate) struct RustCallerAudioFrameCall {
    pub(crate) frame: CallerDecodableAudioFrame,
    pub(crate) outcome: Arc<Mutex<Option<AudioDecoderProductError>>>,
}
impl RustCallerAudioFrameCall {
    fn report(&self, error: AudioDecoderProductError) {
        self.frame
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
impl CallerDecodableAudioFrame {
    fn sdk_call(
        &self,
    ) -> (
        Box<RustCallerAudioFrameCall>,
        Arc<Mutex<Option<AudioDecoderProductError>>>,
    ) {
        let outcome = Arc::new(Mutex::new(None));
        (
            Box::new(RustCallerAudioFrameCall {
                frame: self.clone(),
                outcome: outcome.clone(),
            }),
            outcome,
        )
    }
    /// Real SDK Duration virtual; errors are not exposed as unknown duration.
    /// No duration cache or stock 120-ms limit is imposed on standalone reads.
    pub fn duration_via_sdk(&self) -> Result<usize, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call();
        let value = ffi::audio_caller_frame_sdk_duration(call);
        receipt(&outcome)?;
        Ok(value)
    }
    /// Real SDK IsDtxPacket virtual. Panic/reentry/disabled is not false success.
    pub fn is_dtx_packet_via_sdk(&self) -> Result<bool, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call();
        let value = ffi::audio_caller_frame_sdk_dtx(call);
        receipt(&outcome)?;
        Ok(value)
    }
    /// Real SDK Decode virtual into an explicit caller-owned interleaved span.
    /// The original decoder/frame mutex, lifetime and single-shot state are used.
    /// No implicit allocation/stock bound, resampling, silence or truncation.
    /// Pre-dispatch capacity refusal remains retryable; an actual Decode callback
    /// attempt consumes the frame on success, None, Err, panic or malformed data.
    /// Errors/None leave the entire span unchanged; success writes only the
    /// returned prefix count and preserves the remaining tail. Ordinary None is
    /// distinct from typed callback/native-boundary failure.
    pub fn decode_into_via_sdk(
        &self,
        output: &mut [i16],
    ) -> Result<Option<AudioDecoderFrameDecodeResult>, AudioDecoderProductError> {
        let (call, outcome) = self.sdk_call();
        let value = ffi::audio_caller_frame_sdk_decode(call, output);
        receipt(&outcome)?;
        Ok(value.present.then_some(AudioDecoderFrameDecodeResult {
            num_decoded_samples: value.num_decoded_samples,
            speech_type: AudioSpeechType(value.speech_type),
        }))
    }
}
pub(crate) fn audio_caller_frame_duration_callback(source: &RustCallerAudioFrameCall) -> usize {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source.frame.duration() {
        Ok(value) => value,
        Err(error) => {
            source.report(error);
            0
        }
    }
}
pub(crate) fn audio_caller_frame_dtx_callback(source: &RustCallerAudioFrameCall) -> bool {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source.frame.is_dtx_packet() {
        Ok(value) => value,
        Err(error) => {
            source.report(error);
            false
        }
    }
}
pub(crate) fn audio_caller_frame_decode_callback(
    source: &RustCallerAudioFrameCall,
    capacity: usize,
) -> ffi::FfiDecodedAudio {
    let _callback = crate::provider_panic::ProviderCallbackScope::enter();
    match source.frame.decode(capacity) {
        Ok(Some(value)) => ffi::FfiDecodedAudio {
            samples: value.samples,
            speech_type: value.speech_type.0,
            status: 1,
        },
        Ok(None) => ffi::FfiDecodedAudio {
            samples: Vec::new(),
            speech_type: 0,
            status: 0,
        },
        Err(error) => {
            source.report(error);
            ffi::FfiDecodedAudio {
                samples: Vec::new(),
                speech_type: 0,
                status: 0,
            }
        }
    }
}
pub(crate) fn audio_caller_frame_guard_error(source: &RustCallerAudioFrameCall, kind: u8) {
    let kind = match kind {
        0 => AudioDecoderProductFailureKind::InvalidCapacity,
        2 => AudioDecoderProductFailureKind::FrameAlreadyDecoded,
        _ => AudioDecoderProductFailureKind::InvalidOutput,
    };
    source.report(AudioDecoderProductError {
        operation: AudioDecoderProductOperation::FrameDecode,
        kind,
    });
}
