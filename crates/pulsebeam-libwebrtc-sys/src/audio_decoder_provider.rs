//! Owning, fallible modern caller decoders and frames. No SDK adapter or NetEq
//! installation is implied; generic capacity/compatibility admission is #39.
use crate::{AudioDecoderError, DecodedAudio, Environment};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Complete modern caller decoder contract. Legacy Decode/DecodeRedundant and
/// HasDecodePlc/DecodePlc are deliberately not public. A future SDK adapter must
/// still implement the SDK's internal legacy compatibility contract, not stub it.
/// Callbacks (including frame callbacks/destruction) share one serialized state.
/// Metadata is uncached; sample rate/channels must be lifetime-constant for SDK
/// use. Providers must not wait for or synchronously reenter their owning actor,
/// or panic in destruction. Application-owned strong cycles remain caller-owned.
pub trait AudioDecoderProvider: Send + 'static {
    fn sample_rate_hz(&mut self) -> i32;
    fn channels(&mut self) -> usize;
    fn reset(&mut self) -> Result<(), AudioDecoderError>;
    fn error_code(&mut self) -> i32 {
        0
    }
    fn packet_duration(&mut self, _encoded: &[u8]) -> i32 {
        -2
    }
    fn redundant_packet_duration(&mut self, _encoded: &[u8]) -> i32 {
        -2
    }
    fn packet_has_fec(&mut self, _encoded: &[u8]) -> bool {
        false
    }
    /// Own the payload and backend state required by each returned frame.
    /// Timestamps/priorities are exact; negative priorities violate the SDK
    /// contract. Null frames are observable standalone but not NetEq-admitted.
    fn parse_payload(
        &mut self,
        encoded: Vec<u8>,
        timestamp: u32,
    ) -> Result<Vec<AudioDecoderProviderParseResult>, AudioDecoderError>;
    /// Return only the newly appended, real interleaved concealment samples.
    /// Empty output is the native no-output disposition, not synthesized silence.
    /// Nonempty output must cover requested_samples_per_channel*channels.
    /// The existing prefix is available read-only; it cannot be replaced.
    fn generate_plc(
        &mut self,
        requested_samples_per_channel: usize,
        prefix: &[i16],
        max_appended_samples: usize,
    ) -> Result<Vec<i16>, AudioDecoderError>;
}
/// Owning modern frame contract. Duration is per-channel, zero if unknown;
/// SDK ParsePayload frames must be 10-120 ms. Metadata is not cached or coerced.
/// Decode returns owned actual PCM (count is samples.len()) or ordinary None,
/// distinct from callback Err. The supplied bound is total interleaved samples,
/// not per-channel; any future SDK span adapter requires separate proof.
pub trait AudioDecoderFrameProvider: Send + 'static {
    fn duration(&mut self) -> usize;
    fn is_dtx_packet(&mut self) -> bool {
        false
    }
    fn decode(
        &mut self,
        max_output_samples: usize,
    ) -> Result<Option<DecodedAudio>, AudioDecoderError>;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioDecoderProductOperation {
    SampleRate,
    Channels,
    Reset,
    ErrorCode,
    PacketDuration,
    RedundantPacketDuration,
    PacketHasFec,
    ParsePayload,
    GeneratePlc,
    FrameDuration,
    FrameDtx,
    FrameDecode,
    DestroyFrame,
    DestroyDecoder,
    /// Mandatory native compatibility dispatch, deliberately unsupported.
    NativeCompatibility,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioDecoderProductFailureKind {
    Provider(AudioDecoderError),
    InvalidParseResult,
    InvalidCapacity,
    InvalidOutput,
    FrameAlreadyDecoded,
    Panicked,
    Disabled,
    ReentrantCall,
    /// Approved no-write refusal, not genuine legacy/CNG DSP compatibility.
    UnsupportedCompatibility,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioDecoderProductError {
    pub operation: AudioDecoderProductOperation,
    pub kind: AudioDecoderProductFailureKind,
}
impl std::fmt::Display for AudioDecoderProductError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "caller audio decoder {:?}: {:?}",
            self.operation, self.kind
        )
    }
}
impl std::error::Error for AudioDecoderProductError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            AudioDecoderProductFailureKind::Provider(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Default)]
struct FailureState {
    first: Mutex<Option<AudioDecoderProductError>>,
    disabled: AtomicBool,
}
#[derive(Clone, Default)]
pub struct AudioDecoderProductFailureObserver(Arc<FailureState>);
impl AudioDecoderProductFailureObserver {
    pub fn failure(&self) -> Option<AudioDecoderProductError> {
        *self.0.first.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn is_disabled(&self) -> bool {
        self.0.disabled.load(Ordering::Acquire)
    }
    pub(crate) fn report(
        &self,
        operation: AudioDecoderProductOperation,
        kind: AudioDecoderProductFailureKind,
    ) -> AudioDecoderProductError {
        let e = AudioDecoderProductError { operation, kind };
        self.0
            .first
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(e);
        e
    }
    fn caught_drop(&self, operation: AudioDecoderProductOperation, call: impl FnOnce()) -> bool {
        let _callback = crate::provider_panic::ProviderCallbackScope::enter();
        if let Err(payload) = catch_unwind(AssertUnwindSafe(call)) {
            self.report(operation, AudioDecoderProductFailureKind::Panicked);
            self.0.disabled.store(true, Ordering::Release);
            crate::panic_payload::discard(payload);
            true
        } else {
            false
        }
    }
}
/// An owning transfer product returned by ParsePayload. It cannot be decoded
/// until bound to the owning decoder by CallerAudioDecoder::parse_payload.
/// Its transfer observer records destruction both before and after binding;
/// bound callback errors are observed through the decoder/frame shared observer.
pub struct AudioDecoderFrameProduct {
    provider: Option<Box<dyn AudioDecoderFrameProvider>>,
    attempted: bool,
    observer: AudioDecoderProductFailureObserver,
    bound_observer: Option<AudioDecoderProductFailureObserver>,
}
impl AudioDecoderFrameProduct {
    pub fn from_provider(provider: impl AudioDecoderFrameProvider) -> Self {
        Self {
            provider: Some(Box::new(provider)),
            attempted: false,
            observer: AudioDecoderProductFailureObserver::default(),
            bound_observer: None,
        }
    }
    pub fn failure_observer(&self) -> AudioDecoderProductFailureObserver {
        self.observer.clone()
    }
}
impl Drop for AudioDecoderFrameProduct {
    fn drop(&mut self) {
        let provider = self.provider.take();
        if self
            .observer
            .caught_drop(AudioDecoderProductOperation::DestroyFrame, || {
                drop(provider)
            })
            && let Some(bound) = &self.bound_observer
        {
            bound.report(
                AudioDecoderProductOperation::DestroyFrame,
                AudioDecoderProductFailureKind::Panicked,
            );
            bound.0.disabled.store(true, Ordering::Release);
        }
    }
}
/// Exact parse-result fields, with owning transfer product or native null.
pub struct AudioDecoderProviderParseResult {
    pub timestamp: u32,
    pub priority: i32,
    pub frame: Option<AudioDecoderFrameProduct>,
}
struct Products {
    frames: BTreeMap<u64, AudioDecoderFrameProduct>,
    provider: Option<Box<dyn AudioDecoderProvider>>,
    next_frame: u64,
}
thread_local! {
    static ACTIVE: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    // Outer SDK-owner scopes do not replace actual callback activation.
    static ACTIVE_SDK: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}
pub(crate) struct AudioDecoderSdkScope(usize);
impl Drop for AudioDecoderSdkScope {
    fn drop(&mut self) {
        ACTIVE_SDK.with(|active| {
            let mut active = active.borrow_mut();
            let i = active
                .iter()
                .rposition(|id| *id == self.0)
                .expect("active SDK decoder scope");
            active.remove(i);
        });
    }
}
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
    fn contains(id: usize) -> bool {
        ACTIVE.with(|active| active.borrow().contains(&id))
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            let i = active
                .iter()
                .rposition(|id| *id == self.0)
                .expect("active decoder callback");
            active.remove(i);
        });
    }
}
struct ProductState {
    products: Mutex<Products>,
    retired: Mutex<Vec<u64>>,
    observer: AudioDecoderProductFailureObserver,
    // Frames and decoder are destroyed before this actual retaining environment.
    _environment: Environment,
}
impl ProductState {
    fn id(&self) -> usize {
        self as *const Self as usize
    }
    fn drain_retired(&self, products: &mut Products) {
        loop {
            let ids = std::mem::take(&mut *self.retired.lock().unwrap_or_else(|e| e.into_inner()));
            if ids.is_empty() {
                break;
            }
            for id in ids {
                drop(products.frames.remove(&id));
            }
        }
    }
    fn invoke<T>(
        self: &Arc<Self>,
        operation: AudioDecoderProductOperation,
        call: impl FnOnce(&mut Products) -> Result<T, AudioDecoderProductFailureKind>,
    ) -> Result<T, AudioDecoderProductError> {
        let failure = |kind| self.observer.report(operation, kind);
        if self.observer.is_disabled() {
            return Err(failure(AudioDecoderProductFailureKind::Disabled));
        }
        let _active = Active::enter(self.id())
            .ok_or_else(|| failure(AudioDecoderProductFailureKind::ReentrantCall))?;
        let mut products = self.products.lock().unwrap_or_else(|e| e.into_inner());
        self.drain_retired(&mut products);
        if self.observer.is_disabled() {
            return Err(failure(AudioDecoderProductFailureKind::Disabled));
        }
        let result = match catch_unwind(AssertUnwindSafe(|| call(&mut products))) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(kind)) => Err(failure(kind)),
            Err(payload) => {
                let e = failure(AudioDecoderProductFailureKind::Panicked);
                self.observer.0.disabled.store(true, Ordering::Release);
                crate::panic_payload::discard(payload);
                Err(e)
            }
        };
        self.drain_retired(&mut products);
        result
    }
    fn retire_frame(&self, id: u64) {
        self.retired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id);
        // Drop during a same-product callback must defer destruction, not lock
        // recursively or run a frame destructor concurrently with decoder state.
        if Active::contains(self.id()) {
            return;
        }
        let _active = Active::enter(self.id()).expect("outside decoder callback");
        let mut products = self.products.lock().unwrap_or_else(|e| e.into_inner());
        self.drain_retired(&mut products); // Retirement remains valid when disabled.
    }
}
impl Drop for ProductState {
    fn drop(&mut self) {
        let products = self.products.get_mut().unwrap_or_else(|e| e.into_inner());
        // No frame leases or dispatch owners remain. Even disabled products must
        // retire every frame before destroying decoder state/environment.
        let frames = std::mem::take(&mut products.frames);
        drop(frames); // Each frame catches its own single destructor panic.
        let provider = products.provider.take();
        self.observer
            .caught_drop(AudioDecoderProductOperation::DestroyDecoder, || {
                drop(provider)
            });
    }
}
/// Owning standalone caller decoder; not an AudioDecoder SDK handle or an
/// installable factory/NetEq dependency. Clones share serialized callbacks.
#[derive(Clone)]
pub struct CallerAudioDecoder(Arc<ProductState>);
impl CallerAudioDecoder {
    pub(crate) fn sdk_entry(
        &self,
        operation: AudioDecoderProductOperation,
    ) -> Result<(), AudioDecoderProductError> {
        let kind = if self.0.observer.is_disabled() {
            Some(AudioDecoderProductFailureKind::Disabled)
        } else if Active::contains(self.0.id()) {
            Some(AudioDecoderProductFailureKind::ReentrantCall)
        } else {
            None
        };
        if let Some(kind) = kind {
            Err(self.0.observer.report(operation, kind))
        } else {
            Ok(())
        }
    }
    pub(crate) fn sdk_scope(
        &self,
        operation: AudioDecoderProductOperation,
    ) -> Result<AudioDecoderSdkScope, AudioDecoderProductError> {
        self.sdk_entry(operation)?;
        let scope = ACTIVE_SDK.with(|active| {
            let mut active = active.borrow_mut();
            let id = self.0.id();
            if active.contains(&id) {
                None
            } else {
                active.push(id);
                Some(AudioDecoderSdkScope(id))
            }
        });
        scope.ok_or_else(|| {
            self.0
                .observer
                .report(operation, AudioDecoderProductFailureKind::ReentrantCall)
        })
    }
    pub fn from_provider(environment: &Environment, provider: impl AudioDecoderProvider) -> Self {
        Self(Arc::new(ProductState {
            products: Mutex::new(Products {
                frames: BTreeMap::new(),
                provider: Some(Box::new(provider)),
                next_frame: 0,
            }),
            retired: Mutex::new(Vec::new()),
            observer: AudioDecoderProductFailureObserver::default(),
            _environment: environment.clone(),
        }))
    }
    pub fn failure_observer(&self) -> AudioDecoderProductFailureObserver {
        self.0.observer.clone()
    }
    pub fn sample_rate_hz(&self) -> Result<i32, AudioDecoderProductError> {
        self.0
            .invoke(AudioDecoderProductOperation::SampleRate, |p| {
                Ok(p.provider.as_deref_mut().unwrap().sample_rate_hz())
            })
    }
    pub fn channels(&self) -> Result<usize, AudioDecoderProductError> {
        self.0.invoke(AudioDecoderProductOperation::Channels, |p| {
            Ok(p.provider.as_deref_mut().unwrap().channels())
        })
    }
    pub fn reset(&self) -> Result<(), AudioDecoderProductError> {
        self.0.invoke(AudioDecoderProductOperation::Reset, |p| {
            p.provider
                .as_deref_mut()
                .unwrap()
                .reset()
                .map_err(AudioDecoderProductFailureKind::Provider)
        })
    }
    pub fn error_code(&self) -> Result<i32, AudioDecoderProductError> {
        self.0.invoke(AudioDecoderProductOperation::ErrorCode, |p| {
            Ok(p.provider.as_deref_mut().unwrap().error_code())
        })
    }
    pub fn packet_duration(&self, encoded: &[u8]) -> Result<i32, AudioDecoderProductError> {
        self.0
            .invoke(AudioDecoderProductOperation::PacketDuration, |p| {
                Ok(p.provider.as_deref_mut().unwrap().packet_duration(encoded))
            })
    }
    pub fn redundant_packet_duration(
        &self,
        encoded: &[u8],
    ) -> Result<i32, AudioDecoderProductError> {
        self.0
            .invoke(AudioDecoderProductOperation::RedundantPacketDuration, |p| {
                Ok(p.provider
                    .as_deref_mut()
                    .unwrap()
                    .redundant_packet_duration(encoded))
            })
    }
    pub fn packet_has_fec(&self, encoded: &[u8]) -> Result<bool, AudioDecoderProductError> {
        self.0
            .invoke(AudioDecoderProductOperation::PacketHasFec, |p| {
                Ok(p.provider.as_deref_mut().unwrap().packet_has_fec(encoded))
            })
    }
    pub fn parse_payload(
        &self,
        encoded: Vec<u8>,
        timestamp: u32,
    ) -> Result<Vec<CallerParsedAudioFrame>, AudioDecoderProductError> {
        self.0
            .invoke(AudioDecoderProductOperation::ParsePayload, |p| {
                let mut parsed = p
                    .provider
                    .as_deref_mut()
                    .unwrap()
                    .parse_payload(encoded, timestamp)
                    .map_err(AudioDecoderProductFailureKind::Provider)?;
                for item in &mut parsed {
                    if let Some(frame) = &mut item.frame {
                        frame.bound_observer = Some(self.0.observer.clone());
                    }
                }
                if parsed.iter().any(|item| item.priority < 0) {
                    return Err(AudioDecoderProductFailureKind::InvalidParseResult);
                }
                let mut output = Vec::with_capacity(parsed.len());
                for item in parsed {
                    let frame = if let Some(frame) = item.frame {
                        let id = p.next_frame;
                        p.next_frame = id
                            .checked_add(1)
                            .ok_or(AudioDecoderProductFailureKind::InvalidCapacity)?;
                        p.frames.insert(id, frame);
                        Some(CallerDecodableAudioFrame(Arc::new(FrameLease {
                            id,
                            decoder: self.clone(),
                        })))
                    } else {
                        None
                    };
                    output.push(CallerParsedAudioFrame {
                        timestamp: item.timestamp,
                        priority: item.priority,
                        frame,
                    });
                }
                Ok(output)
            })
    }
    /// Append owned actual PLC. Bounds are caller-supplied, not a stock 120-ms
    /// assumption. Failure leaves the caller's existing prefix unchanged.
    pub fn generate_plc(
        &self,
        requested_samples_per_channel: usize,
        concealment_audio: &mut Vec<i16>,
        max_appended_samples: usize,
    ) -> Result<(), AudioDecoderProductError> {
        self.0
            .invoke(AudioDecoderProductOperation::GeneratePlc, |p| {
                let limit = (isize::MAX as usize) / std::mem::size_of::<i16>();
                if concealment_audio
                    .len()
                    .checked_add(max_appended_samples)
                    .is_none_or(|total| total > limit)
                {
                    return Err(AudioDecoderProductFailureKind::InvalidCapacity);
                }
                let provider = p.provider.as_deref_mut().unwrap();
                let channels = provider.channels();
                let required = requested_samples_per_channel
                    .checked_mul(channels)
                    .ok_or(AudioDecoderProductFailureKind::InvalidCapacity)?;
                let appended = provider
                    .generate_plc(
                        requested_samples_per_channel,
                        concealment_audio,
                        max_appended_samples,
                    )
                    .map_err(AudioDecoderProductFailureKind::Provider)?;
                if appended.len() > max_appended_samples
                    || (!appended.is_empty()
                        && (channels == 0
                            || appended.len() < required
                            || appended.len() % channels != 0))
                {
                    return Err(AudioDecoderProductFailureKind::InvalidOutput);
                }
                concealment_audio
                    .try_reserve(appended.len())
                    .map_err(|_| AudioDecoderProductFailureKind::InvalidCapacity)?;
                concealment_audio.extend(appended);
                Ok(())
            })
    }
}
/// Exact parse fields, retaining nullable frames rather than fabricating one.
pub struct CallerParsedAudioFrame {
    pub timestamp: u32,
    pub priority: i32,
    pub frame: Option<CallerDecodableAudioFrame>,
}
struct FrameLease {
    id: u64,
    decoder: CallerAudioDecoder,
}
impl Drop for FrameLease {
    fn drop(&mut self) {
        self.decoder.0.retire_frame(self.id);
    }
}
/// Cloneable owning frame. Clones share one single-shot decode state and retain
/// the original decoder/environment. Frame callbacks/destruction take its mutex.
#[derive(Clone)]
pub struct CallerDecodableAudioFrame(Arc<FrameLease>);
impl CallerDecodableAudioFrame {
    pub fn failure_observer(&self) -> AudioDecoderProductFailureObserver {
        self.0.decoder.failure_observer()
    }
    pub fn sample_rate_hz(&self) -> Result<i32, AudioDecoderProductError> {
        self.0.decoder.sample_rate_hz()
    }
    pub fn channels(&self) -> Result<usize, AudioDecoderProductError> {
        self.0.decoder.channels()
    }
    pub fn duration(&self) -> Result<usize, AudioDecoderProductError> {
        self.0
            .decoder
            .0
            .invoke(AudioDecoderProductOperation::FrameDuration, |p| {
                Ok(p.frames
                    .get_mut(&self.0.id)
                    .expect("live frame lease")
                    .provider
                    .as_deref_mut()
                    .unwrap()
                    .duration())
            })
    }
    pub fn is_dtx_packet(&self) -> Result<bool, AudioDecoderProductError> {
        self.0
            .decoder
            .0
            .invoke(AudioDecoderProductOperation::FrameDtx, |p| {
                Ok(p.frames
                    .get_mut(&self.0.id)
                    .expect("live frame lease")
                    .provider
                    .as_deref_mut()
                    .unwrap()
                    .is_dtx_packet())
            })
    }
    /// A capacity refusal is pre-dispatch and does not consume the frame. Once
    /// the Decode callback starts, success, None, Err and panic are single-shot.
    /// No output span, fabricated PCM or implicit stock capacity is exposed.
    pub fn decode(
        &self,
        max_output_samples: usize,
    ) -> Result<Option<DecodedAudio>, AudioDecoderProductError> {
        self.0
            .decoder
            .0
            .invoke(AudioDecoderProductOperation::FrameDecode, |p| {
                if p.frames
                    .get(&self.0.id)
                    .expect("live frame lease")
                    .attempted
                {
                    return Err(AudioDecoderProductFailureKind::FrameAlreadyDecoded);
                }
                let channels = p.provider.as_deref_mut().unwrap().channels();
                let frame = p.frames.get_mut(&self.0.id).unwrap();
                let duration = frame.provider.as_deref_mut().unwrap().duration();
                let required = duration
                    .checked_mul(channels)
                    .ok_or(AudioDecoderProductFailureKind::InvalidCapacity)?;
                if max_output_samples < required
                    || max_output_samples > (isize::MAX as usize) / std::mem::size_of::<i16>()
                {
                    return Err(AudioDecoderProductFailureKind::InvalidCapacity);
                }
                frame.attempted = true;
                let output = frame
                    .provider
                    .as_deref_mut()
                    .unwrap()
                    .decode(max_output_samples)
                    .map_err(AudioDecoderProductFailureKind::Provider)?;
                if output
                    .as_ref()
                    .is_some_and(|output| output.samples.len() > max_output_samples)
                {
                    return Err(AudioDecoderProductFailureKind::InvalidOutput);
                }
                Ok(output)
            })
    }
}
