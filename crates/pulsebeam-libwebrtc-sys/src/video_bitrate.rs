use crate::provider_panic::catch_provider as catch_unwind;
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{FieldTrialsSnapshot, VideoScalabilityMode, ffi};

/// Native codec identity, distinct from an SDP codec name or format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodecType {
    Generic,
    Vp8,
    Vp9,
    Av1,
    H264,
    H265,
    Native(i32),
}
impl VideoCodecType {
    pub(crate) fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Generic,
            1 => Self::Vp8,
            2 => Self::Vp9,
            3 => Self::Av1,
            4 => Self::H264,
            5 => Self::H265,
            other => Self::Native(other),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodecMode {
    Realtime,
    Screensharing,
    Native(i32),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoEncoderComplexity {
    Low,
    Normal,
    High,
    Higher,
    Max,
    Native(i32),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoInterLayerPrediction {
    Off,
    On,
    OnKeyPicture,
    Native(i32),
}

/// All native SimulcastStream/SpatialLayer fields. Rates remain in native kbps;
/// signed dimensions and fractional frame rates are not normalized.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoCodecLayer {
    pub width: i32,
    pub height: i32,
    pub max_framerate: f32,
    pub temporal_layers: u8,
    pub max_bitrate_kbps: u32,
    pub target_bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub qp_max: u32,
    pub active: bool,
    pub format: Option<crate::SdpVideoFormat>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VideoCodecSpecificConfiguration {
    Vp8 {
        temporal_layers: u8,
        denoising: bool,
        automatic_resize: bool,
        key_frame_interval: i32,
    },
    Vp9 {
        temporal_layers: u8,
        denoising: bool,
        key_frame_interval: i32,
        adaptive_qp: bool,
        automatic_resize: bool,
        spatial_layers: u8,
        flexible_mode: bool,
        inter_layer_prediction: VideoInterLayerPrediction,
    },
    H264 {
        key_frame_interval: i32,
        temporal_layers: u8,
    },
    Av1 {
        automatic_resize: bool,
    },
}

/// Complete owned snapshot of the pinned VideoCodec creation input. This is
/// not VideoEncoderSettings and does not convert kbps to lossy u32 bps.
/// Layer vectors include every native array slot, including inactive entries;
/// number_of_simulcast_streams separately preserves the native count.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoCodecConfiguration {
    pub codec_type: VideoCodecType,
    pub width: u16,
    pub height: u16,
    pub start_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub max_framerate: u32,
    pub active: bool,
    pub qp_max: u32,
    pub number_of_simulcast_streams: u8,
    pub simulcast_streams: Vec<VideoCodecLayer>,
    pub spatial_layers: Vec<VideoCodecLayer>,
    pub mode: VideoCodecMode,
    pub expect_encode_from_texture: bool,
    pub timing_delay_ms: i64,
    pub timing_outlier_ratio_percent: u16,
    pub legacy_conference_mode: bool,
    pub scalability_mode: Option<VideoScalabilityMode>,
    pub complexity: VideoEncoderComplexity,
    pub frame_drop_enabled: bool,
    pub codec_specific: Option<VideoCodecSpecificConfiguration>,
}

fn layer(value: ffi::FfiVideoBitrateCodecLayer) -> VideoCodecLayer {
    VideoCodecLayer {
        width: value.width,
        height: value.height,
        max_framerate: value.max_framerate,
        temporal_layers: value.temporal_layers,
        max_bitrate_kbps: value.max_bitrate_kbps,
        target_bitrate_kbps: value.target_bitrate_kbps,
        min_bitrate_kbps: value.min_bitrate_kbps,
        qp_max: value.qp_max,
        active: value.active,
        format: (!value.format.is_null()).then(|| crate::SdpVideoFormat::from_native(value.format)),
    }
}
impl VideoCodecConfiguration {
    pub(crate) fn native(&self) -> ffi::FfiVideoBitrateCodec {
        fn layer(v: &VideoCodecLayer) -> ffi::FfiVideoBitrateCodecLayer {
            ffi::FfiVideoBitrateCodecLayer {
                width: v.width,
                height: v.height,
                max_framerate: v.max_framerate,
                temporal_layers: v.temporal_layers,
                max_bitrate_kbps: v.max_bitrate_kbps,
                target_bitrate_kbps: v.target_bitrate_kbps,
                min_bitrate_kbps: v.min_bitrate_kbps,
                qp_max: v.qp_max,
                active: v.active,
                format: v.format.as_ref().map_or_else(cxx::UniquePtr::null, |f| {
                    ffi::sdp_video_format_clone(f.native())
                }),
            }
        }
        let mut v = ffi::FfiVideoBitrateCodec {
            codec_type: match self.codec_type {
                VideoCodecType::Generic => 0,
                VideoCodecType::Vp8 => 1,
                VideoCodecType::Vp9 => 2,
                VideoCodecType::Av1 => 3,
                VideoCodecType::H264 => 4,
                VideoCodecType::H265 => 5,
                VideoCodecType::Native(v) => v,
            },
            width: self.width,
            height: self.height,
            start_bitrate_kbps: self.start_bitrate_kbps,
            max_bitrate_kbps: self.max_bitrate_kbps,
            min_bitrate_kbps: self.min_bitrate_kbps,
            max_framerate: self.max_framerate,
            active: self.active,
            qp_max: self.qp_max,
            number_of_simulcast_streams: self.number_of_simulcast_streams,
            simulcast_streams: self.simulcast_streams.iter().map(layer).collect(),
            spatial_layers: self.spatial_layers.iter().map(layer).collect(),
            mode: match self.mode {
                VideoCodecMode::Realtime => 0,
                VideoCodecMode::Screensharing => 1,
                VideoCodecMode::Native(v) => v,
            },
            expect_encode_from_texture: self.expect_encode_from_texture,
            timing_delay_ms: self.timing_delay_ms,
            timing_outlier_ratio_percent: self.timing_outlier_ratio_percent,
            legacy_conference_mode: self.legacy_conference_mode,
            scalability_mode: self
                .scalability_mode
                .as_ref()
                .map_or_else(String::new, |v| v.as_str().to_owned()),
            complexity: match self.complexity {
                VideoEncoderComplexity::Low => -1,
                VideoEncoderComplexity::Normal => 0,
                VideoEncoderComplexity::High => 1,
                VideoEncoderComplexity::Higher => 2,
                VideoEncoderComplexity::Max => 3,
                VideoEncoderComplexity::Native(v) => v,
            },
            frame_drop_enabled: self.frame_drop_enabled,
            ..Default::default()
        };
        match self.codec_specific {
            Some(VideoCodecSpecificConfiguration::Vp8 {
                temporal_layers,
                denoising,
                automatic_resize,
                key_frame_interval,
            }) => {
                v.temporal_layers = temporal_layers;
                v.denoising = denoising;
                v.automatic_resize = automatic_resize;
                v.key_frame_interval = key_frame_interval;
            }
            Some(VideoCodecSpecificConfiguration::Vp9 {
                temporal_layers,
                denoising,
                key_frame_interval,
                adaptive_qp,
                automatic_resize,
                spatial_layers,
                flexible_mode,
                inter_layer_prediction,
            }) => {
                v.temporal_layers = temporal_layers;
                v.denoising = denoising;
                v.key_frame_interval = key_frame_interval;
                v.adaptive_qp = adaptive_qp;
                v.automatic_resize = automatic_resize;
                v.specific_spatial_layers = spatial_layers;
                v.flexible_mode = flexible_mode;
                v.inter_layer_prediction = match inter_layer_prediction {
                    VideoInterLayerPrediction::Off => 0,
                    VideoInterLayerPrediction::On => 1,
                    VideoInterLayerPrediction::OnKeyPicture => 2,
                    VideoInterLayerPrediction::Native(v) => v,
                };
            }
            Some(VideoCodecSpecificConfiguration::H264 {
                key_frame_interval,
                temporal_layers,
            }) => {
                v.key_frame_interval = key_frame_interval;
                v.temporal_layers = temporal_layers;
            }
            Some(VideoCodecSpecificConfiguration::Av1 { automatic_resize }) => {
                v.automatic_resize = automatic_resize
            }
            None => {}
        }
        v
    }
    pub(crate) fn from_native(value: ffi::FfiVideoBitrateCodec) -> Self {
        let codec_type = VideoCodecType::from_native(value.codec_type);
        let codec_specific = match codec_type {
            VideoCodecType::Vp8 => Some(VideoCodecSpecificConfiguration::Vp8 {
                temporal_layers: value.temporal_layers,
                denoising: value.denoising,
                automatic_resize: value.automatic_resize,
                key_frame_interval: value.key_frame_interval,
            }),
            VideoCodecType::Vp9 => Some(VideoCodecSpecificConfiguration::Vp9 {
                temporal_layers: value.temporal_layers,
                denoising: value.denoising,
                key_frame_interval: value.key_frame_interval,
                adaptive_qp: value.adaptive_qp,
                automatic_resize: value.automatic_resize,
                spatial_layers: value.specific_spatial_layers,
                flexible_mode: value.flexible_mode,
                inter_layer_prediction: match value.inter_layer_prediction {
                    0 => VideoInterLayerPrediction::Off,
                    1 => VideoInterLayerPrediction::On,
                    2 => VideoInterLayerPrediction::OnKeyPicture,
                    other => VideoInterLayerPrediction::Native(other),
                },
            }),
            VideoCodecType::H264 => Some(VideoCodecSpecificConfiguration::H264 {
                key_frame_interval: value.key_frame_interval,
                temporal_layers: value.temporal_layers,
            }),
            VideoCodecType::Av1 => Some(VideoCodecSpecificConfiguration::Av1 {
                automatic_resize: value.automatic_resize,
            }),
            _ => None,
        };
        Self {
            codec_type,
            width: value.width,
            height: value.height,
            start_bitrate_kbps: value.start_bitrate_kbps,
            max_bitrate_kbps: value.max_bitrate_kbps,
            min_bitrate_kbps: value.min_bitrate_kbps,
            max_framerate: value.max_framerate,
            active: value.active,
            qp_max: value.qp_max,
            number_of_simulcast_streams: value.number_of_simulcast_streams,
            simulcast_streams: value.simulcast_streams.into_iter().map(layer).collect(),
            spatial_layers: value.spatial_layers.into_iter().map(layer).collect(),
            mode: match value.mode {
                0 => VideoCodecMode::Realtime,
                1 => VideoCodecMode::Screensharing,
                other => VideoCodecMode::Native(other),
            },
            expect_encode_from_texture: value.expect_encode_from_texture,
            timing_delay_ms: value.timing_delay_ms,
            timing_outlier_ratio_percent: value.timing_outlier_ratio_percent,
            legacy_conference_mode: value.legacy_conference_mode,
            scalability_mode: (!value.scalability_mode.is_empty())
                .then(|| VideoScalabilityMode::from_native(value.scalability_mode)),
            complexity: match value.complexity {
                -1 => VideoEncoderComplexity::Low,
                0 => VideoEncoderComplexity::Normal,
                1 => VideoEncoderComplexity::High,
                2 => VideoEncoderComplexity::Higher,
                3 => VideoEncoderComplexity::Max,
                other => VideoEncoderComplexity::Native(other),
            },
            frame_drop_enabled: value.frame_drop_enabled,
            codec_specific,
        }
    }
}

/// An exact native DataRate state. Native allocator input is not narrowed to
/// u32, rounded to kbps, or converted from infinity into a finite sentinel.
/// Compatibility name for the shared native data-rate value.
pub type VideoAllocationDataRate = crate::DataRate;
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoBitrateAllocationParameters {
    pub total_bitrate: VideoAllocationDataRate,
    pub framerate: f64,
}

/// Per-layer, noncumulative native allocation. None is unset; Some(0) explicitly
/// disables a layer. The sum must fit u32, as required by native SetBitrate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoBitrateAllocation {
    pub layer_bitrates_bps: [[Option<u32>; 4]; 5],
    pub bandwidth_limited: bool,
}
impl VideoBitrateAllocation {
    pub(crate) fn from_native(value: ffi::FfiVideoBitrateAllocation) -> Self {
        Self {
            layer_bitrates_bps: std::array::from_fn(|s| {
                std::array::from_fn(|t| {
                    value.has_layer_bitrate[s][t].then_some(value.layer_bitrates_bps[s][t])
                })
            }),
            bandwidth_limited: value.bandwidth_limited,
        }
    }
    pub fn sum_bps(&self) -> Option<u32> {
        self.layer_bitrates_bps
            .iter()
            .flatten()
            .try_fold(0_u32, |sum, rate| sum.checked_add(rate.unwrap_or(0)))
    }
    pub(crate) fn native(self) -> Option<ffi::FfiVideoBitrateAllocation> {
        self.sum_bps()?;
        Some(ffi::FfiVideoBitrateAllocation {
            layer_bitrates_bps: self
                .layer_bitrates_bps
                .map(|row| row.map(|rate| rate.unwrap_or(0))),
            has_layer_bitrate: self
                .layer_bitrates_bps
                .map(|row| row.map(|rate| rate.is_some())),
            bandwidth_limited: self.bandwidth_limited,
        })
    }
}

/// Retained creation environment with no local peer/factory or actor owner.
/// Copies of its field-trial view can independently outlive peer retirement.
pub struct VideoBitrateAllocatorEnvironment(crate::EventLogEnvironment);
impl VideoBitrateAllocatorEnvironment {
    pub fn time_us(&self) -> i64 {
        self.0.time_us()
    }
    pub fn timestamp(&self) -> crate::Timestamp {
        self.0.timestamp()
    }
    pub fn field_trials(&self) -> FieldTrialsSnapshot {
        self.0.field_trials()
    }
    pub fn environment(&self) -> &crate::Environment {
        self.0.environment()
    }
}

/// A callable allocator product, not a factory-time policy callback. Native
/// calls and destruction use its worker sequence. Send permits native ownership
/// transfer; Sync is not required. Do not wait for or reenter the owning actor.
/// A panic or overflowing allocation disables future methods on this product
/// and supplies an empty allocation. Provider destructors must not panic.
pub trait VideoBitrateAllocator: Send + 'static {
    fn allocate(&mut self, parameters: VideoBitrateAllocationParameters) -> VideoBitrateAllocation;
    fn get_allocation(&mut self, total_bitrate_bps: u32, framerate: u32) -> VideoBitrateAllocation {
        self.allocate(VideoBitrateAllocationParameters {
            total_bitrate: VideoAllocationDataRate::BitsPerSecond(i64::from(total_bitrate_bps)),
            framerate: f64::from(framerate),
        })
    }
    fn set_legacy_conference_mode(&mut self, _enabled: bool) {}
}
/// Select a fresh owned Rust product or the SDK helper using the original native
/// Environment and complete VideoCodec. NativeBuiltin does not implement or
/// narrow the callable Rust product contract.
pub enum VideoBitrateAllocatorProduct {
    Provider(Box<dyn VideoBitrateAllocator>),
    NativeBuiltin,
}
/// Factory callbacks are serialized across all registration clones. Providers
/// need Send, not Sync, and may maintain ordinary mutable creation state.
pub trait VideoBitrateAllocatorFactoryProvider: Send + 'static {
    fn create(
        &mut self,
        environment: VideoBitrateAllocatorEnvironment,
        codec: VideoCodecConfiguration,
    ) -> VideoBitrateAllocatorProduct;
}

struct FactoryState {
    provider: Mutex<Option<Box<dyn VideoBitrateAllocatorFactoryProvider>>>,
    builtin: bool,
    failed: AtomicBool,
}
impl Drop for FactoryState {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(provider)));
    }
}
/// Shareable creation-time registration. Omission leaves the native per-peer
/// default untouched; builtin explicitly selects the independent SDK helper.
#[derive(Clone)]
pub struct VideoBitrateAllocatorFactory(Arc<FactoryState>);
impl VideoBitrateAllocatorFactory {
    pub fn from_provider(provider: impl VideoBitrateAllocatorFactoryProvider) -> Self {
        Self(Arc::new(FactoryState {
            provider: Mutex::new(Some(Box::new(provider))),
            builtin: false,
            failed: AtomicBool::new(false),
        }))
    }
    pub fn builtin() -> Self {
        Self(Arc::new(FactoryState {
            provider: Mutex::new(None),
            builtin: true,
            failed: AtomicBool::new(false),
        }))
    }
    pub fn failed(&self) -> bool {
        self.0.failed.load(Ordering::Acquire)
    }
}
pub(crate) struct RustVideoBitrateAllocatorFactory(
    pub Option<VideoBitrateAllocatorFactory>,
    pub Option<crate::Environment>,
);
pub(crate) struct RustVideoBitrateAllocator {
    provider: Mutex<Option<Box<dyn VideoBitrateAllocator>>>,
    builtin: bool,
    failed: AtomicBool,
    _environment: Option<crate::EventLogEnvironment>,
}
impl RustVideoBitrateAllocator {
    fn with<T>(&self, fallback: T, call: impl FnOnce(&mut dyn VideoBitrateAllocator) -> T) -> T {
        if self.failed.load(Ordering::Acquire) {
            return fallback;
        }
        let mut provider = self
            .provider
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.failed.load(Ordering::Acquire) {
            return fallback;
        }
        let Some(provider) = provider.as_deref_mut() else {
            return fallback;
        };
        match catch_unwind(AssertUnwindSafe(|| call(provider))) {
            Ok(value) => value,
            Err(_) => {
                self.failed.store(true, Ordering::Release);
                fallback
            }
        }
    }
    fn allocation(
        &self,
        call: impl FnOnce(&mut dyn VideoBitrateAllocator) -> VideoBitrateAllocation,
    ) -> ffi::FfiVideoBitrateAllocation {
        self.with(ffi::FfiVideoBitrateAllocation::default(), |provider| {
            call(provider).native().unwrap_or_else(|| {
                self.failed.store(true, Ordering::Release);
                ffi::FfiVideoBitrateAllocation::default()
            })
        })
    }
}
impl Drop for RustVideoBitrateAllocator {
    fn drop(&mut self) {
        let provider = self
            .provider
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ = catch_unwind(AssertUnwindSafe(|| drop(provider)));
    }
}
pub(crate) fn video_bitrate_factory_present(source: &RustVideoBitrateAllocatorFactory) -> bool {
    source.0.is_some()
}
pub(crate) fn video_bitrate_factory_builtin(source: &RustVideoBitrateAllocatorFactory) -> bool {
    source.0.as_ref().is_some_and(|factory| factory.0.builtin)
}
pub(crate) fn video_bitrate_factory_create(
    source: &RustVideoBitrateAllocatorFactory,
    environment: cxx::UniquePtr<ffi::NativeEnvironment>,
    codec: ffi::FfiVideoBitrateCodec,
) -> Box<RustVideoBitrateAllocator> {
    let environment = crate::EventLogEnvironment::from_native(environment, source.1.as_ref());
    let mut provider = None;
    let mut builtin = false;
    if let Some(factory) = &source.0
        && !factory.failed()
    {
        let mut factory_provider = factory
            .0
            .provider
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // A previous invocation may have failed while this one waited.
        if !factory.failed() {
            let result = catch_unwind(AssertUnwindSafe(|| {
                factory_provider.as_deref_mut().map(|provider| {
                    provider.create(
                        VideoBitrateAllocatorEnvironment(environment.clone()),
                        VideoCodecConfiguration::from_native(codec),
                    )
                })
            }));
            match result {
                Ok(Some(VideoBitrateAllocatorProduct::Provider(value))) => provider = Some(value),
                Ok(Some(VideoBitrateAllocatorProduct::NativeBuiltin)) | Ok(None) => builtin = true,
                Err(_) => factory.0.failed.store(true, Ordering::Release),
            }
        }
    }
    Box::new(RustVideoBitrateAllocator {
        provider: Mutex::new(provider),
        builtin,
        failed: AtomicBool::new(false),
        _environment: Some(environment),
    })
}
pub(crate) fn video_bitrate_allocator_builtin(source: &RustVideoBitrateAllocator) -> bool {
    source.builtin
}
pub(crate) fn video_bitrate_allocator_allocate(
    source: &RustVideoBitrateAllocator,
    bps: i64,
    rate_kind: i8,
    framerate: f64,
) -> ffi::FfiVideoBitrateAllocation {
    let total_bitrate = match rate_kind {
        -1 => VideoAllocationDataRate::NegativeInfinity,
        1 => VideoAllocationDataRate::PositiveInfinity,
        _ => VideoAllocationDataRate::BitsPerSecond(bps),
    };
    source.allocation(|provider| {
        provider.allocate(VideoBitrateAllocationParameters {
            total_bitrate,
            framerate,
        })
    })
}
pub(crate) fn video_bitrate_allocator_get_allocation(
    source: &RustVideoBitrateAllocator,
    bps: u32,
    framerate: u32,
) -> ffi::FfiVideoBitrateAllocation {
    source.allocation(|provider| provider.get_allocation(bps, framerate))
}
pub(crate) fn video_bitrate_allocator_set_legacy(
    source: &RustVideoBitrateAllocator,
    enabled: bool,
) {
    source.with((), |provider| provider.set_legacy_conference_mode(enabled));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, sync::atomic::AtomicUsize};

    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Allocate(VideoBitrateAllocationParameters),
        Get(u32, u32),
        Legacy(bool),
    }
    struct Observe {
        calls: Arc<Mutex<Vec<Call>>>,
        count: Cell<usize>,
    }
    impl VideoBitrateAllocator for Observe {
        fn allocate(
            &mut self,
            parameters: VideoBitrateAllocationParameters,
        ) -> VideoBitrateAllocation {
            self.count.set(self.count.get() + 1);
            self.calls.lock().unwrap().push(Call::Allocate(parameters));
            let mut result = VideoBitrateAllocation {
                bandwidth_limited: true,
                ..Default::default()
            };
            result.layer_bitrates_bps[0][0] = Some(0);
            result.layer_bitrates_bps[4][3] = Some(u32::MAX);
            result
        }
        fn get_allocation(&mut self, bps: u32, framerate: u32) -> VideoBitrateAllocation {
            self.calls.lock().unwrap().push(Call::Get(bps, framerate));
            VideoBitrateAllocation::default()
        }
        fn set_legacy_conference_mode(&mut self, enabled: bool) {
            self.calls.lock().unwrap().push(Call::Legacy(enabled));
        }
    }
    fn product(provider: impl VideoBitrateAllocator) -> RustVideoBitrateAllocator {
        RustVideoBitrateAllocator {
            provider: Mutex::new(Some(Box::new(provider))),
            builtin: false,
            failed: AtomicBool::new(false),
            _environment: None,
        }
    }
    #[test]
    fn registration_and_values_are_actor_safe_without_sync_products() {
        fn send<T: Send>() {}
        fn shared<T: Send + Sync>() {}
        send::<Observe>();
        shared::<VideoBitrateAllocatorFactory>();
        shared::<VideoBitrateAllocatorEnvironment>();
        shared::<VideoCodecConfiguration>();
        shared::<crate::PeerConnectionDependencies>();
        assert!(!video_bitrate_factory_present(
            &RustVideoBitrateAllocatorFactory(None, None)
        ));
        let builtin = VideoBitrateAllocatorFactory::builtin();
        let native = RustVideoBitrateAllocatorFactory(Some(builtin.clone()), None);
        assert!(video_bitrate_factory_present(&native));
        assert!(video_bitrate_factory_builtin(&native));
        assert!(!builtin.failed());
    }
    #[test]
    fn concurrent_factory_admission_is_serialized_and_failure_is_latched() {
        struct Factory {
            calls: Arc<AtomicUsize>,
            mutable: Cell<usize>,
        }
        impl VideoBitrateAllocatorFactoryProvider for Factory {
            fn create(
                &mut self,
                _: VideoBitrateAllocatorEnvironment,
                _: VideoCodecConfiguration,
            ) -> VideoBitrateAllocatorProduct {
                self.mutable.set(self.mutable.get() + 1);
                self.calls.fetch_add(1, Ordering::SeqCst);
                panic!("factory failure");
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let registration = VideoBitrateAllocatorFactory::from_provider(Factory {
            calls: calls.clone(),
            mutable: Cell::new(0),
        });
        let source = Arc::new(RustVideoBitrateAllocatorFactory(
            Some(registration.clone()),
            None,
        ));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let source = source.clone();
                std::thread::spawn(move || {
                    let environment = crate::Environment::builder().build().unwrap();
                    let product = video_bitrate_factory_create(
                        &source,
                        ffi::clone_environment(environment.native()),
                        ffi::FfiVideoBitrateCodec::default(),
                    );
                    assert!(!video_bitrate_allocator_builtin(&product));
                    let result = video_bitrate_allocator_get_allocation(&product, 100, 30);
                    assert!(
                        !result
                            .has_layer_bitrate
                            .iter()
                            .flatten()
                            .any(|&present| present)
                    );
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(registration.failed());
    }
    #[test]
    fn independent_product_methods_and_exact_rate_states_are_preserved() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let source = product(Observe {
            calls: calls.clone(),
            count: Cell::new(0),
        });
        for (bps, kind, rate) in [
            (
                i64::MAX - 1,
                0,
                VideoAllocationDataRate::BitsPerSecond(i64::MAX - 1),
            ),
            (0, 1, VideoAllocationDataRate::PositiveInfinity),
            (0, -1, VideoAllocationDataRate::NegativeInfinity),
        ] {
            let output = video_bitrate_allocator_allocate(&source, bps, kind, 29.97);
            assert!(output.bandwidth_limited);
            assert!(output.has_layer_bitrate[0][0]);
            assert_eq!(output.layer_bitrates_bps[0][0], 0);
            assert!(!output.has_layer_bitrate[0][1]);
            assert!(output.has_layer_bitrate[4][3]);
            assert_eq!(output.layer_bitrates_bps[4][3], u32::MAX);
            assert_eq!(
                calls.lock().unwrap().last(),
                Some(&Call::Allocate(VideoBitrateAllocationParameters {
                    total_bitrate: rate,
                    framerate: 29.97
                }))
            );
        }
        video_bitrate_allocator_get_allocation(&source, u32::MAX, u32::MAX);
        video_bitrate_allocator_set_legacy(&source, true);
        let calls = calls.lock().unwrap();
        assert_eq!(
            &calls[3..],
            &[Call::Get(u32::MAX, u32::MAX), Call::Legacy(true)]
        );
        assert!(!source.failed.load(Ordering::Acquire));
    }
    #[test]
    fn overflow_rejects_whole_allocation_and_deactivates_every_method() {
        struct Overflow(Arc<Mutex<usize>>);
        impl VideoBitrateAllocator for Overflow {
            fn allocate(&mut self, _: VideoBitrateAllocationParameters) -> VideoBitrateAllocation {
                *self.0.lock().unwrap() += 1;
                let mut output = VideoBitrateAllocation {
                    bandwidth_limited: true,
                    ..Default::default()
                };
                output.layer_bitrates_bps[0][0] = Some(u32::MAX);
                output.layer_bitrates_bps[0][1] = Some(1);
                output
            }
            fn set_legacy_conference_mode(&mut self, _: bool) {
                panic!("failed product must not enter setter");
            }
        }
        let calls = Arc::new(Mutex::new(0));
        let source = product(Overflow(calls.clone()));
        for _ in 0..2 {
            let output = video_bitrate_allocator_get_allocation(&source, 1, 30);
            assert!(
                !output
                    .has_layer_bitrate
                    .iter()
                    .flatten()
                    .any(|&present| present)
            );
            assert!(!output.bandwidth_limited);
        }
        video_bitrate_allocator_set_legacy(&source, true);
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(source.failed.load(Ordering::Acquire));
    }
    #[test]
    fn callback_and_destructor_panics_do_not_cross_the_abi() {
        struct Panic;
        impl VideoBitrateAllocator for Panic {
            fn allocate(&mut self, _: VideoBitrateAllocationParameters) -> VideoBitrateAllocation {
                panic!("allocate panic");
            }
            fn set_legacy_conference_mode(&mut self, _: bool) {
                panic!("setter must not be entered after failure");
            }
        }
        impl Drop for Panic {
            fn drop(&mut self) {
                panic!("drop panic");
            }
        }
        let source = product(Panic);
        let output = video_bitrate_allocator_allocate(&source, 1, 0, f64::NAN);
        assert!(
            !output
                .has_layer_bitrate
                .iter()
                .flatten()
                .any(|&present| present)
        );
        assert!(source.failed.load(Ordering::Acquire));
        video_bitrate_allocator_set_legacy(&source, true);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(source))).is_ok());
    }
    #[test]
    fn codec_snapshot_does_not_normalize_inactive_slots_signed_fields_or_kbps() {
        let mut input = ffi::FfiVideoBitrateCodec {
            codec_type: 2,
            width: u16::MAX,
            height: 7,
            start_bitrate_kbps: u32::MAX,
            mode: 1,
            complexity: -1,
            timing_delay_ms: -17,
            timing_outlier_ratio_percent: u16::MAX,
            number_of_simulcast_streams: 0,
            temporal_layers: 4,
            denoising: true,
            automatic_resize: true,
            key_frame_interval: -101,
            adaptive_qp: true,
            specific_spatial_layers: 5,
            flexible_mode: true,
            inter_layer_prediction: 2,
            frame_drop_enabled: true,
            legacy_conference_mode: true,
            ..Default::default()
        };
        input
            .simulcast_streams
            .push(ffi::FfiVideoBitrateCodecLayer {
                width: -9,
                height: -8,
                max_framerate: 29.5,
                temporal_layers: 3,
                max_bitrate_kbps: u32::MAX,
                target_bitrate_kbps: 123,
                min_bitrate_kbps: 17,
                qp_max: 63,
                active: false,
                format: crate::SdpVideoFormat::with_parameters(
                    "custom-mixed-codec",
                    &std::collections::BTreeMap::from([(b"arbitrary".to_vec(), b"value".to_vec())]),
                )
                .native,
            });
        let output = VideoCodecConfiguration::from_native(input);
        assert_eq!(output.start_bitrate_kbps, u32::MAX);
        assert_eq!(output.width, u16::MAX);
        assert_eq!(output.timing_delay_ms, -17);
        assert_eq!(output.timing_outlier_ratio_percent, u16::MAX);
        assert_eq!(output.number_of_simulcast_streams, 0);
        assert_eq!(output.simulcast_streams.len(), 1);
        let layer = &output.simulcast_streams[0];
        assert_eq!(
            (layer.width, layer.height, layer.max_framerate),
            (-9, -8, 29.5)
        );
        assert_eq!(layer.max_bitrate_kbps, u32::MAX);
        assert!(!layer.active);
        let format = layer.format.as_ref().unwrap().fields();
        assert_eq!(format.name, b"custom-mixed-codec");
        assert_eq!(format.parameters[b"arbitrary".as_slice()], b"value");
        assert_eq!(
            output.codec_specific,
            Some(VideoCodecSpecificConfiguration::Vp9 {
                temporal_layers: 4,
                denoising: true,
                key_frame_interval: -101,
                adaptive_qp: true,
                automatic_resize: true,
                spatial_layers: 5,
                flexible_mode: true,
                inter_layer_prediction: VideoInterLayerPrediction::OnKeyPicture,
            })
        );
    }
}
