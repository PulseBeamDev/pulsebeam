#include "pulsebeam-webrtc-sys/native/video_bitrate.h"

#include <string>
#include <utility>

#include "api/video/builtin_video_bitrate_allocator_factory.h"
#include "api/video/video_codec_constants.h"
#include "api/video_codecs/scalability_mode.h"
#include "pulsebeam-webrtc-sys/native/event_log.h"
#include "pulsebeam-webrtc-sys/native/video_codec_values.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {
namespace {
static_assert(webrtc::kMaxSpatialLayers == 5);
static_assert(webrtc::kMaxTemporalStreams == 4);

FfiVideoBitrateCodecLayer CodecLayer(const webrtc::SimulcastStream& source) {
  FfiVideoBitrateCodecLayer value{};
  value.width = source.width;
  value.height = source.height;
  value.max_framerate = source.maxFramerate;
  value.temporal_layers = source.numberOfTemporalLayers;
  value.max_bitrate_kbps = source.maxBitrate;
  value.target_bitrate_kbps = source.targetBitrate;
  value.min_bitrate_kbps = source.minBitrate;
  value.qp_max = source.qpMax;
  value.active = source.active;
  if (source.format) value.format = std::make_unique<NativeSdpVideoFormat>(*source.format);
  return value;
}

}  // namespace
FfiVideoBitrateCodec VideoCodecConfigurationFields(const webrtc::VideoCodec& source) {
  FfiVideoBitrateCodec value{};
  value.codec_type = static_cast<std::int32_t>(source.codecType);
  value.width = source.width;
  value.height = source.height;
  value.start_bitrate_kbps = source.startBitrate;
  value.max_bitrate_kbps = source.maxBitrate;
  value.min_bitrate_kbps = source.minBitrate;
  value.max_framerate = source.maxFramerate;
  value.active = source.active;
  value.qp_max = source.qpMax;
  value.number_of_simulcast_streams = source.numberOfSimulcastStreams;
  for (const auto& stream : source.simulcastStream)
    value.simulcast_streams.push_back(CodecLayer(stream));
  for (const auto& stream : source.spatialLayers)
    value.spatial_layers.push_back(CodecLayer(stream));
  value.mode = static_cast<std::int32_t>(source.mode);
  value.expect_encode_from_texture = source.expect_encode_from_texture;
  value.timing_delay_ms = source.timing_frame_thresholds.delay_ms;
  value.timing_outlier_ratio_percent =
      source.timing_frame_thresholds.outlier_ratio_percent;
  value.legacy_conference_mode = source.legacy_conference_mode;
  if (const auto mode = source.GetScalabilityMode())
    value.scalability_mode =
        std::string(webrtc::ScalabilityModeToString(*mode));
  value.complexity =
      static_cast<std::int32_t>(source.GetVideoEncoderComplexity());
  value.frame_drop_enabled = source.GetFrameDropEnabled();
  switch (source.codecType) {
    case webrtc::kVideoCodecVP8: {
      const auto& codec = source.VP8();
      value.temporal_layers = codec.numberOfTemporalLayers;
      value.denoising = codec.denoisingOn;
      value.automatic_resize = codec.automaticResizeOn;
      value.key_frame_interval = codec.keyFrameInterval;
      break;
    }
    case webrtc::kVideoCodecVP9: {
      const auto& codec = source.VP9();
      value.temporal_layers = codec.numberOfTemporalLayers;
      value.denoising = codec.denoisingOn;
      value.key_frame_interval = codec.keyFrameInterval;
      value.adaptive_qp = codec.adaptiveQpMode;
      value.automatic_resize = codec.automaticResizeOn;
      value.specific_spatial_layers = codec.numberOfSpatialLayers;
      value.flexible_mode = codec.flexibleMode;
      value.inter_layer_prediction =
          static_cast<std::int32_t>(codec.interLayerPred);
      break;
    }
    case webrtc::kVideoCodecH264: {
      const auto& codec = source.H264();
      value.key_frame_interval = codec.keyFrameInterval;
      value.temporal_layers = codec.numberOfTemporalLayers;
      break;
    }
    case webrtc::kVideoCodecAV1:
      value.automatic_resize = source.AV1().automatic_resize_on;
      break;
    default:
      break;
  }
  return value;
}

namespace {
webrtc::VideoBitrateAllocation Allocation(FfiVideoBitrateAllocation source) {
  webrtc::VideoBitrateAllocation value;
  for (std::size_t spatial = 0; spatial < webrtc::kMaxSpatialLayers; ++spatial) {
    for (std::size_t temporal = 0;
         temporal < webrtc::kMaxTemporalStreams; ++temporal) {
      if (source.has_layer_bitrate[spatial][temporal] &&
          !value.SetBitrate(spatial, temporal,
                            source.layer_bitrates_bps[spatial][temporal])) {
        // Rust validates the complete sum before crossing the ABI. Defend the
        // conversion too, never returning an accidentally partial allocation.
        return webrtc::VideoBitrateAllocation();
      }
    }
  }
  value.set_bw_limited(source.bandwidth_limited);
  return value;
}

class RustAllocatorAdapter final : public webrtc::VideoBitrateAllocator {
 public:
  explicit RustAllocatorAdapter(rust::Box<RustVideoBitrateAllocator> source)
      : source_(std::move(source)) {}
  webrtc::VideoBitrateAllocation GetAllocation(
      std::uint32_t total_bitrate_bps, std::uint32_t framerate) override {
    return Allocation(video_bitrate_allocator_get_allocation(
        *source_, total_bitrate_bps, framerate));
  }
  webrtc::VideoBitrateAllocation Allocate(
      webrtc::VideoBitrateAllocationParameters parameters) override {
    const auto rate = parameters.total_bitrate;
    return Allocation(video_bitrate_allocator_allocate(
        *source_, rate.IsFinite() ? rate.bps() : 0,
        static_cast<std::int8_t>(
            rate.IsFinite() ? 0 : rate.IsPlusInfinity() ? 1 : -1),
        parameters.framerate));
  }
  void SetLegacyConferenceMode(bool enabled) override {
    video_bitrate_allocator_set_legacy(*source_, enabled);
  }
 private:
  rust::Box<RustVideoBitrateAllocator> source_;
};

class RustAllocatorFactoryAdapter final
    : public webrtc::VideoBitrateAllocatorFactory {
 public:
  explicit RustAllocatorFactoryAdapter(
      rust::Box<RustVideoBitrateAllocatorFactory> source)
      : source_(std::move(source)) {}
  std::unique_ptr<webrtc::VideoBitrateAllocator> Create(
      const webrtc::Environment& environment,
      const webrtc::VideoCodec& codec) override {
    auto source = video_bitrate_factory_create(
        *source_, rtc_event_log_factory_environment(environment),
        VideoCodecConfigurationFields(codec));
    if (video_bitrate_allocator_builtin(*source)) {
      return webrtc::CreateBuiltinVideoBitrateAllocatorFactory()->Create(
          environment, codec);
    }
    // Failed Rust factories receive a non-null rejecting product, not a hidden
    // native-default replacement or a null pointer consumed by encoder setup.
    return std::make_unique<RustAllocatorAdapter>(std::move(source));
  }
 private:
  rust::Box<RustVideoBitrateAllocatorFactory> source_;
};
}  // namespace

std::unique_ptr<webrtc::VideoBitrateAllocatorFactory>
video_bitrate_factory_adapter(
    rust::Box<RustVideoBitrateAllocatorFactory> source) noexcept {
  if (!video_bitrate_factory_present(*source)) return nullptr;
  if (video_bitrate_factory_builtin(*source))
    return webrtc::CreateBuiltinVideoBitrateAllocatorFactory();
  return std::make_unique<RustAllocatorFactoryAdapter>(std::move(source));
}
}  // namespace pulsebeam::webrtc_sys
