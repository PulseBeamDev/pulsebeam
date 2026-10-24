//! Owning current decoder and parsed-frame resources. Obsolete raw Decode,
//! DecodeRedundant and unsized DecodePlc entry points are intentionally omitted.
use std::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

use crate::{Environment, ffi};

struct AudioCodecPairIdInner {
    native: cxx::UniquePtr<ffi::NativeAudioCodecPairId>,
    numeric: u64,
}
// SAFETY: the SDK pair ID is an immutable copyable uint64 value. Only const
// reads and copies into native optionals are exposed.
unsafe impl Send for AudioCodecPairIdInner {}
// SAFETY: see Send; clones share immutable identity, not a mutable generator.
unsafe impl Sync for AudioCodecPairIdInner {}

/// A native-generated codec-pair identity. Numeric representation, equality,
/// ordering and hash are stable; no arbitrary numeric reconstruction exists.
/// The SDK's process-wide generator allows at most 2^63 creations per process.
#[derive(Clone)]
pub struct AudioCodecPairId(Arc<AudioCodecPairIdInner>);
impl AudioCodecPairId {
    pub fn create() -> Self {
        let native = ffi::audio_codec_pair_id_create();
        let numeric =
            ffi::audio_codec_pair_id_numeric(native.as_ref().expect("native codec pair ID"));
        Self(Arc::new(AudioCodecPairIdInner { native, numeric }))
    }
    /// Independent SDK value copy of this identity, without consuming another
    /// process-global generated ID. Useful for owning callback option copies.
    pub fn copy_value(&self) -> Self {
        Self::from_native(ffi::audio_codec_pair_id_copy(
            self.0.native.as_ref().unwrap(),
        ))
    }
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeAudioCodecPairId>) -> Self {
        let numeric =
            ffi::audio_codec_pair_id_numeric(native.as_ref().expect("owned codec pair ID"));
        Self(Arc::new(AudioCodecPairIdInner { native, numeric }))
    }
    pub fn numeric_representation(&self) -> u64 {
        self.0.numeric
    }
    pub(crate) fn native(&self) -> &cxx::UniquePtr<ffi::NativeAudioCodecPairId> {
        &self.0.native
    }
}
impl fmt::Debug for AudioCodecPairId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AudioCodecPairId")
            .field(&self.0.numeric)
            .finish()
    }
}
impl PartialEq for AudioCodecPairId {
    fn eq(&self, other: &Self) -> bool {
        self.0.numeric == other.0.numeric
    }
}
impl Eq for AudioCodecPairId {}
impl PartialOrd for AudioCodecPairId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for AudioCodecPairId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.numeric.cmp(&other.0.numeric)
    }
}
impl Hash for AudioCodecPairId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.numeric.hash(state);
    }
}

/// Exact native decoder speech tag, including future unknown values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioSpeechType(pub i32);
impl AudioSpeechType {
    pub const SPEECH: Self = Self(1);
    pub const COMFORT_NOISE: Self = Self(2);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioDecoderError {
    /// A native integer conversion cannot represent this encoded input.
    InvalidPayload,
    /// The native frame contract permits only one decode attempt.
    FrameAlreadyDecoded,
    /// The owning append buffer cannot accommodate a bounded native PLC append.
    InvalidCapacity,
    /// A native product violated the selected implementation's proven bounds.
    NativeInvariant,
}
impl fmt::Display for AudioDecoderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPayload => "encoded audio exceeds native scalar limits",
            Self::FrameAlreadyDecoded => "audio frame was already decoded",
            Self::InvalidCapacity => "audio output capacity is out of range",
            Self::NativeInvariant => "native audio decoder violated its resource contract",
        })
    }
}
impl std::error::Error for AudioDecoderError {}

/// Native packet metadata. Durations are samples per channel; -1 means an
/// error and -2 means no estimate is implemented. Neither sentinel is erased.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioPacketInfo {
    pub duration: i32,
    pub redundant_duration: i32,
    pub has_fec: bool,
}

/// A successful native decode, copied into owning interleaved PCM storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedAudio {
    pub samples: Vec<i16>,
    pub speech_type: AudioSpeechType,
}

struct AudioDecoderInner {
    native: cxx::UniquePtr<ffi::NativeAudioDecoder>,
    // Preserve Rust-side environment dependencies, including randomness leases.
    _environment: Environment,
}
// SAFETY: every stateful native decoder/frame call takes the same shared mutex. Frame
// products retain the native decoder and this Rust environment owner.
unsafe impl Send for AudioDecoderInner {}
// SAFETY: see Send; no mutable decoder or output-buffer view escapes.
unsafe impl Sync for AudioDecoderInner {}

/// A live owning decoder. Clones name the same serialized native product.
#[derive(Clone)]
pub struct AudioDecoder(Arc<AudioDecoderInner>);
// Keep the complete native decoder wrapper/state and the Rust environment
// lease together. Parsed Rust frames retain the same Arc and prevent handoff;
// native shared-State owners are also gated before preparing the handoff.
pub(crate) struct AudioDecoderTransfer {
    pub(crate) native: cxx::UniquePtr<ffi::NativeAudioDecoder>,
    pub(crate) _environment: Environment,
}
// SAFETY: same complete guarded native product and retaining environment as
// AudioDecoderInner. SDK owners only expose const Rust borrows; native methods
// and frame lifetime operations share the original decoder-state mutex.
unsafe impl Send for AudioDecoderTransfer {}
unsafe impl Sync for AudioDecoderTransfer {}
pub(crate) fn audio_decoder_transfer_native(
    source: &AudioDecoderTransfer,
) -> &ffi::NativeAudioDecoder {
    source.native.as_ref().expect("owned decoder handoff")
}
impl AudioDecoder {
    pub(crate) fn try_into_factory_transfer(self) -> Result<AudioDecoderTransfer, Self> {
        match Arc::try_unwrap(self.0) {
            Ok(inner) => {
                if !ffi::audio_decoder_factory_transfer_ready(
                    inner.native.as_ref().expect("owning decoder"),
                ) {
                    return Err(Self(Arc::new(inner)));
                }
                Ok(AudioDecoderTransfer {
                    native: inner.native,
                    _environment: inner._environment,
                })
            }
            Err(inner) => Err(Self(inner)),
        }
    }
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeAudioDecoder>,
        environment: Environment,
    ) -> Self {
        debug_assert!(!native.is_null());
        Self(Arc::new(AudioDecoderInner {
            native,
            _environment: environment,
        }))
    }
    fn native(&self) -> &ffi::NativeAudioDecoder {
        self.0.native.as_ref().expect("validated audio decoder")
    }
    pub fn sample_rate_hz(&self) -> i32 {
        ffi::audio_decoder_sample_rate(self.native())
    }
    pub fn channels(&self) -> usize {
        ffi::audio_decoder_channels(self.native())
    }
    pub fn reset(&self) {
        ffi::audio_decoder_reset(self.native());
    }
    pub fn error_code(&self) -> i32 {
        ffi::audio_decoder_error_code(self.native())
    }
    /// Native duration/FEC observations with implementation-specific integer
    /// conversion guards. No codec metadata is inferred from the SDP name.
    pub fn packet_info(&self, encoded: &[u8]) -> Result<AudioPacketInfo, AudioDecoderError> {
        let value = ffi::audio_decoder_packet_info(self.native(), encoded);
        if !value.valid {
            return Err(AudioDecoderError::InvalidPayload);
        }
        Ok(AudioPacketInfo {
            duration: value.duration,
            redundant_duration: value.redundant_duration,
            has_fec: value.has_fec,
        })
    }
    /// Parse zero or more owning frames, retaining exact priorities and RTP
    /// timestamps, including native modulo-2^32 timestamp wrap.
    pub fn parse_payload(
        &self,
        encoded: Vec<u8>,
        timestamp: u32,
    ) -> Result<Vec<ParsedAudioFrame>, AudioDecoderError> {
        let value = ffi::audio_decoder_parse(self.native(), &encoded, timestamp);
        if !value.valid {
            return Err(AudioDecoderError::InvalidPayload);
        }
        Ok(value
            .frames
            .into_iter()
            .map(|parsed| ParsedAudioFrame {
                timestamp: parsed.timestamp,
                priority: parsed.priority,
                frame: if parsed.frame.is_null() {
                    None
                } else {
                    Some(DecodableAudioFrame {
                        native: parsed.frame,
                        decoder: self.clone(),
                    })
                },
            })
            .collect())
    }
    /// Append the actual native PLC output to an owning buffer. The selected
    /// implementation can produce nothing or ignore the requested duration;
    /// no silence is synthesized and the existing prefix is preserved.
    pub fn generate_plc(
        &self,
        requested_samples_per_channel: usize,
        concealment_audio: &mut Vec<i16>,
    ) -> Result<(), AudioDecoderError> {
        let value = ffi::audio_decoder_generate_plc(
            self.native(),
            requested_samples_per_channel,
            concealment_audio,
        );
        if !value.valid {
            return Err(AudioDecoderError::InvalidCapacity);
        }
        *concealment_audio = value.samples;
        Ok(())
    }
}

/// Exact native parse-result metadata and nullable frame ownership.
pub struct ParsedAudioFrame {
    pub timestamp: u32,
    pub priority: i32,
    pub frame: Option<DecodableAudioFrame>,
}

/// An owning parsed frame. Its decoder/environment outlive the frame, even
/// after the original decoder and factory handles are dropped.
pub struct DecodableAudioFrame {
    // Field order releases the frame before the retained Rust decoder owner.
    native: cxx::UniquePtr<ffi::NativeDecodableAudioFrame>,
    decoder: AudioDecoder,
}
// SAFETY: native frame reads, decode attempts and destruction share the
// retained decoder's mutex. A decode attempt is single-shot, including failure.
unsafe impl Send for DecodableAudioFrame {}
// SAFETY: see Send; outputs are owning copies and no native frame view escapes.
unsafe impl Sync for DecodableAudioFrame {}
impl DecodableAudioFrame {
    fn native(&self) -> &ffi::NativeDecodableAudioFrame {
        self.native
            .as_ref()
            .expect("validated decodable audio frame")
    }
    pub fn duration(&self) -> usize {
        ffi::audio_frame_duration(self.native())
    }
    pub fn is_dtx_packet(&self) -> bool {
        ffi::audio_frame_is_dtx(self.native())
    }
    pub fn sample_rate_hz(&self) -> i32 {
        self.decoder.sample_rate_hz()
    }
    pub fn channels(&self) -> usize {
        self.decoder.channels()
    }
    /// Decode once into an implementation-bounded owning buffer. Native
    /// optional failure remains Ok(None), distinct from adapter safety errors.
    pub fn decode(&self) -> Result<Option<DecodedAudio>, AudioDecoderError> {
        let value = ffi::audio_frame_decode(self.native());
        match value.status {
            0 => Ok(None),
            1 => Ok(Some(DecodedAudio {
                samples: value.samples,
                speech_type: AudioSpeechType(value.speech_type),
            })),
            2 => Err(AudioDecoderError::FrameAlreadyDecoded),
            _ => Err(AudioDecoderError::NativeInvariant),
        }
    }
}
