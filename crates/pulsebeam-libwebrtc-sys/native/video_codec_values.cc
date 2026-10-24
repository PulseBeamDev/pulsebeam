#include "pulsebeam-webrtc-sys/native/video_codec_values.h"
#include "absl/strings/match.h"
#include "pulsebeam-webrtc-sys/native/rtc_error.h"
#include "pulsebeam-webrtc-sys/native/video_metadata.h"
#include "pulsebeam-webrtc-sys/native/string_builder.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "modules/video_coding/svc/scalability_mode_util.h"
#include "rtc_base/logging.h"
#include "rtc_base/checks.h"
#include <limits>
#include <algorithm>

namespace pulsebeam::webrtc_sys {
namespace {
template <typename Bytes> std::string Text(const Bytes &bytes) {
  return std::string(reinterpret_cast<const char *>(bytes.data()),
                     bytes.size());
}
rust::Vec<std::uint8_t> Bytes(const std::string &text) {
  rust::Vec<std::uint8_t> bytes;
  bytes.reserve(text.size());
  for (unsigned char b : text)
    bytes.push_back(b);
  return bytes;
}
template <typename Pairs>
webrtc::CodecParameterMap Parameters(const Pairs &pairs) {
  webrtc::CodecParameterMap result;
  for (const auto &pair : pairs)
    result.emplace(Text(pair.key), Text(pair.value));
  return result;
}
template <typename Input>
absl::InlinedVector<webrtc::ScalabilityMode, webrtc::kScalabilityModeCount>
Modes(const Input &input) {
  absl::InlinedVector<webrtc::ScalabilityMode, webrtc::kScalabilityModeCount>
      result;
  for (auto mode : input)
    result.push_back(static_cast<webrtc::ScalabilityMode>(mode));
  return result;
}
} // namespace
namespace {
webrtc::VideoEncoder::ResolutionBitrateLimits NativeLimits(const FfiVideoResolutionBitrateLimits& value) {
  return {value.frame_size_pixels, value.min_start_bitrate_bps, value.min_bitrate_bps, value.max_bitrate_bps};
}
FfiVideoResolutionBitrateLimits LimitsFields(const webrtc::VideoEncoder::ResolutionBitrateLimits& value) {
  return {value.frame_size_pixels, value.min_start_bitrate_bps, value.min_bitrate_bps, value.max_bitrate_bps};
}
}
namespace {
bool ReadAllocation(const FfiVideoBitrateAllocation& source, webrtc::VideoBitrateAllocation& target) {
  webrtc::VideoBitrateAllocation candidate;
  for (std::size_t s = 0; s < webrtc::kMaxSpatialLayers; ++s)
    for (std::size_t t = 0; t < webrtc::kMaxTemporalStreams; ++t)
      if (source.has_layer_bitrate[s][t] && !candidate.SetBitrate(s, t, source.layer_bitrates_bps[s][t])) return false;
  candidate.set_bw_limited(source.bandwidth_limited);
  target = candidate;
  return true;
}
FfiVideoBitrateAllocation AllocationFields(const webrtc::VideoBitrateAllocation& source) {
  FfiVideoBitrateAllocation result{};
  for (std::size_t s = 0; s < webrtc::kMaxSpatialLayers; ++s)
    for (std::size_t t = 0; t < webrtc::kMaxTemporalStreams; ++t) {
      result.has_layer_bitrate[s][t] = source.HasBitrate(s, t);
      result.layer_bitrates_bps[s][t] = source.GetBitrate(s, t);
    }
  result.bandwidth_limited = source.is_bw_limited();
  return result;
}
bool ReadBandwidth(std::int64_t bits, std::int8_t kind, webrtc::DataRate& target) {
  if (kind == 1) target = webrtc::DataRate::PlusInfinity();
  else if (kind == -1) target = webrtc::DataRate::MinusInfinity();
  else if (kind == 0 && bits != std::numeric_limits<std::int64_t>::min() && bits != std::numeric_limits<std::int64_t>::max()) {
#if RTC_DCHECK_IS_ON
    if (bits < 0) return false;
#endif
    target = webrtc::DataRate::BitsPerSec(bits);
  }
  else return false;
  return true;
}
}
namespace {
FfiCodecDependencyTemplate CodecTemplateFields(const webrtc::FrameDependencyTemplate& source) {
  FfiCodecDependencyTemplate result;
  result.spatial_id = source.spatial_id; result.temporal_id = source.temporal_id;
  for (auto v : source.decode_target_indications) result.indications.push_back(static_cast<std::int32_t>(v));
  for (auto v : source.frame_diffs) result.frame_diffs.push_back(v);
  for (auto v : source.chain_diffs) result.chain_diffs.push_back(v);
  return result;
}
webrtc::FrameDependencyTemplate ReadCodecTemplate(const FfiCodecDependencyTemplate& fields) {
  webrtc::FrameDependencyTemplate result;
  result.spatial_id = fields.spatial_id; result.temporal_id = fields.temporal_id;
  for (auto v : fields.indications) result.decode_target_indications.push_back(static_cast<webrtc::DecodeTargetIndication>(v));
  for (auto v : fields.frame_diffs) result.frame_diffs.push_back(v);
  for (auto v : fields.chain_diffs) result.chain_diffs.push_back(v);
  return result;
}
}
std::unique_ptr<NativeCodecSpecificInfo> codec_specific_new() noexcept { return std::make_unique<NativeCodecSpecificInfo>(); }
std::unique_ptr<NativeCodecSpecificInfo> codec_specific_clone(const NativeCodecSpecificInfo& source) noexcept { return std::make_unique<NativeCodecSpecificInfo>(source.value()); }
FfiCodecSpecificFields codec_specific_fields(const NativeCodecSpecificInfo& source) noexcept {
  const auto& v = source.value();
  FfiCodecSpecificFields f{};
  f.codec_type = static_cast<std::int32_t>(v.codecType); f.end_of_picture = v.end_of_picture;
  // Supply initialized physical GOF storage even for non-VP9 values.
  f.vp9_gof = CodecGofFields(webrtc::GofInfoVP9{});
  if (v.codecType == webrtc::kVideoCodecVP8) {
    const auto& a = v.codecSpecific.VP8;
    f.vp8_non_reference = a.nonReference; f.vp8_temporal_idx = a.temporalIdx; f.vp8_layer_sync = a.layerSync; f.vp8_key_idx = a.keyIdx;
    f.vp8_explicit_dependencies = a.useExplicitDependencies; f.vp8_referenced_count = a.referencedBuffersCount; f.vp8_updated_count = a.updatedBuffersCount;
    for (std::size_t i = 0; i < 3; ++i) { f.vp8_referenced[i] = a.referencedBuffers[i]; f.vp8_updated[i] = a.updatedBuffers[i]; }
  } else if (v.codecType == webrtc::kVideoCodecVP9) {
    const auto& a = v.codecSpecific.VP9;
    f.vp9_first_frame = a.first_frame_in_picture; f.vp9_inter_pic = a.inter_pic_predicted; f.vp9_flexible = a.flexible_mode;
    f.vp9_ss_available = a.ss_data_available; f.vp9_non_ref_inter_layer = a.non_ref_for_inter_layer_pred; f.vp9_temporal_idx = a.temporal_idx;
    f.vp9_temporal_up_switch = a.temporal_up_switch; f.vp9_inter_layer = a.inter_layer_predicted; f.vp9_gof_idx = a.gof_idx;
    f.vp9_spatial_layers = a.num_spatial_layers; f.vp9_first_active = a.first_active_layer; f.vp9_has_resolution = a.spatial_layer_resolution_present;
    for (std::size_t i = 0; i < 8; ++i) { f.vp9_width[i] = a.width[i]; f.vp9_height[i] = a.height[i]; }
    f.vp9_gof = CodecGofFields(a.gof); f.vp9_ref_count = a.num_ref_pics;
    for (std::size_t i = 0; i < 3; ++i) f.vp9_p_diff[i] = a.p_diff[i];
  } else if (v.codecType == webrtc::kVideoCodecH264) {
    const auto& a = v.codecSpecific.H264;
    f.h264_packetization = static_cast<std::int32_t>(a.packetization_mode); f.h264_temporal_idx = a.temporal_idx; f.h264_base_sync = a.base_layer_sync; f.h264_idr = a.idr_frame;
  }
  if (v.generic_frame_info) {
    const auto& a = *v.generic_frame_info; f.has_generic = true; f.generic = CodecTemplateFields(a);
    for (const auto& b : a.encoder_buffers) f.buffers.push_back({b.id, b.referenced, b.updated});
    for (bool b : a.part_of_chain) f.part_of_chain.push_back(b);
    f.active_targets = static_cast<std::uint32_t>(a.active_decode_targets.to_ulong());
  }
  if (v.template_structure) {
    const auto& a = *v.template_structure; f.has_structure = true;
    f.structure.structure_id = a.structure_id; f.structure.decode_targets = a.num_decode_targets; f.structure.chains = a.num_chains;
    for (auto b : a.decode_target_protected_by_chain) f.structure.protected_by_chain.push_back(b);
    for (auto b : a.resolutions) f.structure.resolutions.push_back({b.Width(), b.Height()});
    for (const auto& b : a.templates) f.structure.templates.push_back(CodecTemplateFields(b));
  }
  f.has_scalability = v.scalability_mode.has_value(); f.scalability = static_cast<std::uint8_t>(v.scalability_mode.value_or(webrtc::ScalabilityMode::kL1T1));
  if (v.frame_instrumentation_data) f.instrumentation = std::make_unique<NativeFrameInstrumentationData>(*v.frame_instrumentation_data);
  return f;
}
bool codec_specific_set_fields(NativeCodecSpecificInfo& target, FfiCodecSpecificFields f) noexcept {
  // VideoCodecType is a non-fixed enum; its storage range is 0..7.
  if (f.codec_type < 0 || f.codec_type > 7) return false;
  webrtc::CodecSpecificInfo v;
  v.codecType = static_cast<webrtc::VideoCodecType>(f.codec_type); v.end_of_picture = f.end_of_picture;
  if (v.codecType == webrtc::kVideoCodecVP8) {
    auto& a = v.codecSpecific.VP8;
    a.nonReference = f.vp8_non_reference; a.temporalIdx = f.vp8_temporal_idx; a.layerSync = f.vp8_layer_sync; a.keyIdx = f.vp8_key_idx;
    a.useExplicitDependencies = f.vp8_explicit_dependencies; a.referencedBuffersCount = f.vp8_referenced_count; a.updatedBuffersCount = f.vp8_updated_count;
    for (std::size_t i = 0; i < 3; ++i) { a.referencedBuffers[i] = f.vp8_referenced[i]; a.updatedBuffers[i] = f.vp8_updated[i]; }
  } else if (v.codecType == webrtc::kVideoCodecVP9) {
    auto& a = v.codecSpecific.VP9;
    a.first_frame_in_picture = f.vp9_first_frame; a.inter_pic_predicted = f.vp9_inter_pic; a.flexible_mode = f.vp9_flexible;
    a.ss_data_available = f.vp9_ss_available; a.non_ref_for_inter_layer_pred = f.vp9_non_ref_inter_layer; a.temporal_idx = f.vp9_temporal_idx;
    a.temporal_up_switch = f.vp9_temporal_up_switch; a.inter_layer_predicted = f.vp9_inter_layer; a.gof_idx = f.vp9_gof_idx;
    a.num_spatial_layers = f.vp9_spatial_layers; a.first_active_layer = f.vp9_first_active; a.spatial_layer_resolution_present = f.vp9_has_resolution;
    for (std::size_t i = 0; i < 8; ++i) { a.width[i] = f.vp9_width[i]; a.height[i] = f.vp9_height[i]; }
    if (!CodecGofFromFields(f.vp9_gof, a.gof)) return false;
    a.num_ref_pics = f.vp9_ref_count;
    for (std::size_t i = 0; i < 3; ++i) a.p_diff[i] = f.vp9_p_diff[i];
  } else if (v.codecType == webrtc::kVideoCodecH264) {
    auto& a = v.codecSpecific.H264;
    a.packetization_mode = static_cast<webrtc::H264PacketizationMode>(f.h264_packetization); a.temporal_idx = f.h264_temporal_idx; a.base_layer_sync = f.h264_base_sync; a.idr_frame = f.h264_idr;
  }
  if (f.has_generic) {
    auto& a = v.generic_frame_info.emplace(); static_cast<webrtc::FrameDependencyTemplate&>(a) = ReadCodecTemplate(f.generic);
    for (const auto& b : f.buffers) a.encoder_buffers.emplace_back(b.id, b.referenced, b.updated);
    for (bool b : f.part_of_chain) a.part_of_chain.push_back(b);
    a.active_decode_targets = f.active_targets;
  }
  if (f.has_structure) {
    auto& a = v.template_structure.emplace(); const auto& b = f.structure;
    a.structure_id = b.structure_id; a.num_decode_targets = b.decode_targets; a.num_chains = b.chains;
    for (auto c : b.protected_by_chain) a.decode_target_protected_by_chain.push_back(c);
    for (auto c : b.resolutions) a.resolutions.emplace_back(c.width, c.height);
    for (const auto& c : b.templates) a.templates.push_back(ReadCodecTemplate(c));
  }
  if (f.has_scalability) v.scalability_mode = static_cast<webrtc::ScalabilityMode>(f.scalability);
  if (f.instrumentation) v.frame_instrumentation_data = f.instrumentation->value();
  target.value() = v;
  return true;
}
std::unique_ptr<NativeVideoCodec> video_codec_new() noexcept { return std::make_unique<NativeVideoCodec>(); }
std::unique_ptr<NativeVideoCodec> video_codec_clone(const NativeVideoCodec& source) noexcept { return std::make_unique<NativeVideoCodec>(source.value()); }
FfiVideoBitrateCodec video_codec_fields(const NativeVideoCodec& source) noexcept { return VideoCodecConfigurationFields(source.value()); }
bool video_codec_set_fields(NativeVideoCodec& target, FfiVideoBitrateCodec fields) noexcept {
  // Validate storage discriminants before invoking union accessors. Inert array
  // entries and signed layer dimensions retain their complete native domain.
  if (fields.codec_type < 0 || fields.codec_type > 7 || fields.simulcast_streams.size() != webrtc::kMaxSimulcastStreams || fields.spatial_layers.size() != webrtc::kMaxSpatialLayers) return false;
  webrtc::VideoCodec candidate;
  candidate.codecType = static_cast<webrtc::VideoCodecType>(fields.codec_type);
  candidate.width = fields.width; candidate.height = fields.height;
  candidate.startBitrate = fields.start_bitrate_kbps; candidate.minBitrate = fields.min_bitrate_kbps; candidate.maxBitrate = fields.max_bitrate_kbps;
  candidate.maxFramerate = fields.max_framerate; candidate.active = fields.active; candidate.qpMax = fields.qp_max;
  candidate.numberOfSimulcastStreams = fields.number_of_simulcast_streams;
  auto layer = [](const FfiVideoBitrateCodecLayer& input, webrtc::SimulcastStream& output) {
    output.width = input.width; output.height = input.height; output.maxFramerate = input.max_framerate;
    output.numberOfTemporalLayers = input.temporal_layers; output.maxBitrate = input.max_bitrate_kbps;
    output.targetBitrate = input.target_bitrate_kbps; output.minBitrate = input.min_bitrate_kbps; output.qpMax = input.qp_max; output.active = input.active;
    output.format = input.format ? std::optional<webrtc::SdpVideoFormat>(input.format->value()) : std::nullopt;
  };
  for (std::size_t i = 0; i < webrtc::kMaxSimulcastStreams; ++i) layer(fields.simulcast_streams[i], candidate.simulcastStream[i]);
  for (std::size_t i = 0; i < webrtc::kMaxSpatialLayers; ++i) layer(fields.spatial_layers[i], candidate.spatialLayers[i]);
  candidate.mode = static_cast<webrtc::VideoCodecMode>(fields.mode);
  candidate.expect_encode_from_texture = fields.expect_encode_from_texture;
  candidate.timing_frame_thresholds = {fields.timing_delay_ms, fields.timing_outlier_ratio_percent};
  candidate.legacy_conference_mode = fields.legacy_conference_mode;
  if (!fields.scalability_mode.empty()) {
    const auto mode = webrtc::ScalabilityModeFromString(std::string(fields.scalability_mode));
    if (!mode) return false;
    candidate.SetScalabilityMode(*mode);
  }
  candidate.SetVideoEncoderComplexity(static_cast<webrtc::VideoCodecComplexity>(fields.complexity));
  candidate.SetFrameDropEnabled(fields.frame_drop_enabled);
  switch (candidate.codecType) {
    case webrtc::kVideoCodecVP8: *candidate.VP8() = {fields.temporal_layers, fields.denoising, fields.automatic_resize, fields.key_frame_interval}; break;
    case webrtc::kVideoCodecVP9: *candidate.VP9() = {fields.temporal_layers, fields.denoising, fields.key_frame_interval, fields.adaptive_qp, fields.automatic_resize, fields.specific_spatial_layers, fields.flexible_mode, static_cast<webrtc::InterLayerPredMode>(fields.inter_layer_prediction)}; break;
    case webrtc::kVideoCodecH264: *candidate.H264() = {fields.key_frame_interval, fields.temporal_layers}; break;
    case webrtc::kVideoCodecAV1: *candidate.AV1() = {fields.automatic_resize}; break;
    default: break;
  }
  target.value() = candidate;
  return true;
}
std::unique_ptr<NativeVideoBitrateAllocation> video_allocation_new() noexcept { return std::make_unique<NativeVideoBitrateAllocation>(); }
std::unique_ptr<NativeVideoBitrateAllocation> video_allocation_clone(const NativeVideoBitrateAllocation& source) noexcept { return std::make_unique<NativeVideoBitrateAllocation>(source.value()); }
FfiVideoBitrateAllocation video_allocation_fields(const NativeVideoBitrateAllocation& source) noexcept { return AllocationFields(source.value()); }
bool video_allocation_set_fields(NativeVideoBitrateAllocation& target, FfiVideoBitrateAllocation fields) noexcept { return ReadAllocation(fields, target.value()); }
bool video_allocation_set(NativeVideoBitrateAllocation& target, std::size_t spatial, std::size_t temporal, std::uint32_t bits) noexcept {
  return spatial < webrtc::kMaxSpatialLayers && temporal < webrtc::kMaxTemporalStreams && target.value().SetBitrate(spatial, temporal, bits);
}
std::uint32_t video_allocation_sum(const NativeVideoBitrateAllocation& source, std::size_t spatial, std::size_t temporal, std::uint8_t operation) noexcept {
  if (operation == 0) return source.value().get_sum_bps();
  if (operation == 1) return source.value().get_sum_kbps();
  if (spatial >= webrtc::kMaxSpatialLayers) return 0;
  if (operation == 2) return source.value().GetSpatialLayerSum(spatial);
  if (operation == 3 && temporal < webrtc::kMaxTemporalStreams) return source.value().GetTemporalLayerSum(spatial, temporal);
  return 0;
}
rust::Vec<std::uint32_t> video_allocation_temporal(const NativeVideoBitrateAllocation& source, std::size_t spatial) noexcept {
  rust::Vec<std::uint32_t> result;
  if (spatial < webrtc::kMaxSpatialLayers) for (auto bits : source.value().GetTemporalLayerAllocation(spatial)) result.push_back(bits);
  return result;
}
std::unique_ptr<NativeVideoBitrateAllocation> video_allocation_simulcast(const NativeVideoBitrateAllocation& source, std::size_t spatial) noexcept {
  if (spatial >= webrtc::kMaxSpatialLayers) return nullptr;
  const auto layers = source.value().GetSimulcastAllocations();
  return layers[spatial] ? std::make_unique<NativeVideoBitrateAllocation>(*layers[spatial]) : nullptr;
}
bool video_allocation_equal(const NativeVideoBitrateAllocation& a, const NativeVideoBitrateAllocation& b) noexcept { return a.value() == b.value(); }
rust::String video_allocation_string(const NativeVideoBitrateAllocation& source) noexcept { return source.value().ToString(); }
std::unique_ptr<NativeVideoRateControl> video_rate_control_new() noexcept { return std::make_unique<NativeVideoRateControl>(); }
std::unique_ptr<NativeVideoRateControl> video_rate_control_clone(const NativeVideoRateControl& source) noexcept { return std::make_unique<NativeVideoRateControl>(source.value()); }
FfiOwnedVideoRateControl video_rate_control_fields(const NativeVideoRateControl& source) noexcept {
  const auto& v = source.value();
  return {AllocationFields(v.target_bitrate), AllocationFields(v.bitrate), v.framerate_fps,
      v.bandwidth_allocation.IsFinite() ? v.bandwidth_allocation.bps() : 0,
      static_cast<std::int8_t>(v.bandwidth_allocation.IsFinite() ? 0 : v.bandwidth_allocation.IsPlusInfinity() ? 1 : -1)};
}
bool video_rate_control_set_fields(NativeVideoRateControl& target, FfiOwnedVideoRateControl fields) noexcept {
  webrtc::VideoEncoder::RateControlParameters candidate;
  if (!ReadAllocation(fields.target, candidate.target_bitrate) || !ReadAllocation(fields.effective, candidate.bitrate) ||
      !ReadBandwidth(fields.bandwidth_bps, fields.bandwidth_kind, candidate.bandwidth_allocation)) return false;
  candidate.framerate_fps = fields.framerate_fps;
  target.value() = candidate;
  return true;
}
bool video_rate_control_equal(const NativeVideoRateControl& a, const NativeVideoRateControl& b) noexcept { return a.value() == b.value(); }
std::unique_ptr<NativeVideoRateControl> video_rate_control_construct(FfiVideoBitrateAllocation fields, double fps, bool has_bandwidth, std::int64_t bits, std::int8_t kind) noexcept {
  webrtc::VideoBitrateAllocation allocation;
  webrtc::DataRate bandwidth = webrtc::DataRate::Zero();
  if (!ReadAllocation(fields, allocation) || (has_bandwidth && !ReadBandwidth(bits, kind, bandwidth))) return nullptr;
  return has_bandwidth ? std::make_unique<NativeVideoRateControl>(webrtc::VideoEncoder::RateControlParameters(allocation, fps, bandwidth))
                       : std::make_unique<NativeVideoRateControl>(webrtc::VideoEncoder::RateControlParameters(allocation, fps));
}
static_assert(webrtc::kMaxSpatialLayers == 5);
static_assert(static_cast<int>(webrtc::VideoFrameBuffer::Type::kNative) == 0 && static_cast<int>(webrtc::VideoFrameBuffer::Type::kNV12) == 8);
std::unique_ptr<NativeVideoEncoderInfo> video_encoder_info_new() noexcept { return std::make_unique<NativeVideoEncoderInfo>(); }
std::unique_ptr<NativeVideoEncoderInfo> video_encoder_info_clone(const NativeVideoEncoderInfo& source) noexcept { return std::make_unique<NativeVideoEncoderInfo>(source.value()); }
void video_encoder_info_assign(NativeVideoEncoderInfo& target, const NativeVideoEncoderInfo& source) noexcept { target.value() = source.value(); }
FfiOwnedVideoEncoderInfo video_encoder_info_fields(const NativeVideoEncoderInfo& source) noexcept {
  const auto& value = source.value();
  FfiOwnedVideoEncoderInfo result;
  result.has_thresholds = value.scaling_settings.thresholds.has_value();
  if (value.scaling_settings.thresholds) {
    result.qp_low = value.scaling_settings.thresholds->low; result.qp_high = value.scaling_settings.thresholds->high;
  }
  result.min_pixels_per_frame = value.scaling_settings.min_pixels_per_frame;
  result.requested_resolution_alignment = value.requested_resolution_alignment;
  result.apply_alignment_to_all_simulcast_layers = value.apply_alignment_to_all_simulcast_layers;
  result.supports_native_handle = value.supports_native_handle;
  result.implementation_name = Bytes(value.implementation_name);
  result.has_trusted_rate_controller = value.has_trusted_rate_controller;
  result.is_hardware_accelerated = value.is_hardware_accelerated;
  result.enable_cpu_overuse_detection = value.enable_cpu_overuse_detection;
  for (const auto& allocation : value.fps_allocation) {
    FfiVideoEncoderFpsAllocation layer;
    for (auto fraction : allocation) layer.fractions.push_back(fraction);
    result.fps_allocation.push_back(std::move(layer));
  }
  for (const auto& limit : value.resolution_bitrate_limits) result.resolution_bitrate_limits.push_back(LimitsFields(limit));
  result.supports_simulcast = value.supports_simulcast;
  for (auto format : value.preferred_pixel_formats) result.preferred_pixel_formats.push_back(static_cast<std::int32_t>(format));
  result.has_is_qp_trusted = value.is_qp_trusted.has_value(); result.is_qp_trusted = value.is_qp_trusted.value_or(false);
  result.has_min_qp = value.min_qp.has_value(); result.min_qp = value.min_qp.value_or(0);
  result.has_mapped_resolution = value.mapped_resolution.has_value();
  if (value.mapped_resolution) { result.mapped_width = value.mapped_resolution->width; result.mapped_height = value.mapped_resolution->height; }
  return result;
}
bool video_encoder_info_set_fields(NativeVideoEncoderInfo& target, FfiOwnedVideoEncoderInfo fields) noexcept {
  if (fields.fps_allocation.size() != webrtc::kMaxSpatialLayers) return false;
  auto value = target.value();
  value.scaling_settings = webrtc::VideoEncoder::ScalingSettings::kOff;
  if (fields.has_thresholds) value.scaling_settings = webrtc::VideoEncoder::ScalingSettings(fields.qp_low, fields.qp_high);
  value.scaling_settings.min_pixels_per_frame = fields.min_pixels_per_frame;
  value.requested_resolution_alignment = fields.requested_resolution_alignment;
  value.apply_alignment_to_all_simulcast_layers = fields.apply_alignment_to_all_simulcast_layers;
  value.supports_native_handle = fields.supports_native_handle;
  value.implementation_name = Text(fields.implementation_name);
  value.has_trusted_rate_controller = fields.has_trusted_rate_controller;
  value.is_hardware_accelerated = fields.is_hardware_accelerated;
  value.enable_cpu_overuse_detection = fields.enable_cpu_overuse_detection;
  for (std::size_t i = 0; i < webrtc::kMaxSpatialLayers; ++i) {
    value.fps_allocation[i].clear();
    for (auto fraction : fields.fps_allocation[i].fractions) value.fps_allocation[i].push_back(fraction);
  }
  value.resolution_bitrate_limits.clear();
  for (const auto& limit : fields.resolution_bitrate_limits) value.resolution_bitrate_limits.push_back(NativeLimits(limit));
  value.supports_simulcast = fields.supports_simulcast;
  value.preferred_pixel_formats.clear();
  // Fixed-underlying scoped enum: preserve every int32 storage value here.
  // Its fatal naming operation is checked only when ToString is reached.
  for (auto format : fields.preferred_pixel_formats) value.preferred_pixel_formats.push_back(static_cast<webrtc::VideoFrameBuffer::Type>(format));
  value.is_qp_trusted = fields.has_is_qp_trusted ? std::optional<bool>(fields.is_qp_trusted) : std::nullopt;
  value.min_qp = fields.has_min_qp ? std::optional<int>(fields.min_qp) : std::nullopt;
  value.mapped_resolution = fields.has_mapped_resolution ? std::optional<webrtc::VideoEncoder::Resolution>(webrtc::VideoEncoder::Resolution(fields.mapped_width, fields.mapped_height)) : std::nullopt;
  target.value() = value;
  return true;
}
bool video_encoder_info_equal(const NativeVideoEncoderInfo& a, const NativeVideoEncoderInfo& b) noexcept { return a.value() == b.value(); }
rust::Vec<std::uint8_t> video_encoder_info_string(const NativeVideoEncoderInfo& source, bool& valid) noexcept {
  valid = true;
  for (auto format : source.value().preferred_pixel_formats) {
    const auto raw = static_cast<std::int32_t>(format);
    if (raw < 0 || raw > 8) { valid = false; return {}; }
  }
  return Bytes(source.value().ToString());
}
FfiVideoResolutionBitrateLimits video_encoder_info_limits(const NativeVideoEncoderInfo& source, std::int32_t pixels, bool& valid, bool& found) noexcept {
  valid = true; found = false;
#if RTC_DCHECK_IS_ON
  auto limits = source.value().resolution_bitrate_limits;
  std::sort(limits.begin(), limits.end(), [](const auto& a, const auto& b) { return a.frame_size_pixels < b.frame_size_pixels; });
  for (std::size_t i = 0; i < limits.size(); ++i) {
    const auto& value = limits[i];
    if (value.min_bitrate_bps < 0 || value.min_start_bitrate_bps < 0 || value.max_bitrate_bps < value.min_bitrate_bps ||
        (i && (value.min_bitrate_bps < limits[i-1].min_bitrate_bps || value.min_start_bitrate_bps < limits[i-1].min_start_bitrate_bps || value.max_bitrate_bps < limits[i-1].max_bitrate_bps))) {
      valid = false; return {};
    }
    // Match the SDK's early return. Unconsumed suffix metadata is unrestricted.
    if (value.frame_size_pixels >= pixels) break;
  }
#endif
  const auto limit = source.value().GetEncoderBitrateLimitsForResolution(pixels);
  found = limit.has_value();
  return limit ? LimitsFields(*limit) : FfiVideoResolutionBitrateLimits{};
}
bool video_resolution_bitrate_limits_equal(FfiVideoResolutionBitrateLimits a, FfiVideoResolutionBitrateLimits b) noexcept { return NativeLimits(a) == NativeLimits(b); }
std::unique_ptr<NativeVideoDecoderSettings> video_decoder_settings_new() noexcept {
  return std::make_unique<NativeVideoDecoderSettings>();
}
std::unique_ptr<NativeVideoDecoderSettings> video_decoder_settings_clone(const NativeVideoDecoderSettings &source) noexcept {
  return std::make_unique<NativeVideoDecoderSettings>(source.value());
}
void video_decoder_settings_assign(NativeVideoDecoderSettings &target, const NativeVideoDecoderSettings &source) noexcept { target.value() = source.value(); }
FfiOwnedVideoDecoderSettings video_decoder_settings_fields(const NativeVideoDecoderSettings &source) noexcept {
  const auto &value = source.value();
  const auto resolution = value.max_render_resolution();
  return {value.buffer_pool_size().has_value(), value.buffer_pool_size().value_or(0),
      resolution.Width(), resolution.Height(), value.number_of_cores(), static_cast<std::int32_t>(value.codec_type())};
}
std::int32_t video_decoder_settings_set_fields(NativeVideoDecoderSettings &target, FfiOwnedVideoDecoderSettings fields) noexcept {
  // This pin's non-fixed enum has enumerators 0..5, hence its C++ value range
  // is 0..7. Keep unnamed 6/7 metadata; never cast outside that range.
  static_assert(webrtc::kVideoCodecGeneric == 0 && webrtc::kVideoCodecH265 == 5);
  if (fields.codec_type < 0 || fields.codec_type > 7) return 2;
#if RTC_DCHECK_IS_ON
  if (fields.cores <= 0) return 1;
#endif
  auto value = target.value();
  value.set_buffer_pool_size(fields.has_buffer_pool_size ? std::optional<int>(fields.buffer_pool_size) : std::nullopt);
  value.set_max_render_resolution(webrtc::RenderResolution(fields.width, fields.height));
  value.set_number_of_cores(fields.cores);
  value.set_codec_type(static_cast<webrtc::VideoCodecType>(fields.codec_type));
  target.value() = value;
  return 0;
}
std::unique_ptr<NativeVideoDecoderInfo> video_decoder_info_new() noexcept { return std::make_unique<NativeVideoDecoderInfo>(); }
std::unique_ptr<NativeVideoDecoderInfo> video_decoder_info_clone(const NativeVideoDecoderInfo &source) noexcept {
  return std::make_unique<NativeVideoDecoderInfo>(source.value());
}
void video_decoder_info_assign(NativeVideoDecoderInfo &target, const NativeVideoDecoderInfo &source) noexcept { target.value() = source.value(); }
FfiOwnedVideoDecoderInfo video_decoder_info_fields(const NativeVideoDecoderInfo &source) noexcept {
  return {Bytes(source.value().implementation_name), source.value().is_hardware_accelerated};
}
void video_decoder_info_set_fields(NativeVideoDecoderInfo &target, FfiOwnedVideoDecoderInfo fields) noexcept {
  target.value().implementation_name = Text(fields.implementation_name);
  target.value().is_hardware_accelerated = fields.hardware_accelerated;
}
bool video_decoder_info_equal(const NativeVideoDecoderInfo &left, const NativeVideoDecoderInfo &right) noexcept { return left.value() == right.value(); }
rust::Vec<std::uint8_t> video_decoder_info_string(const NativeVideoDecoderInfo &value) noexcept { return Bytes(value.value().ToString()); }
bool SdpVideoFormatStringSafe(const webrtc::SdpVideoFormat &value) noexcept {
  for (auto mode : value.scalability_modes)
    if (static_cast<std::uint8_t>(mode) >= webrtc::kScalabilityModeCount)
      return false;
  return true;
}
bool string_builder_append_sdp_video_format(
    NativeStringBuilder &target, const NativeSdpVideoFormat &value) noexcept {
  if (!SdpVideoFormatStringSafe(value.value()))
    return false;
  target.value() << value.value();
  return true;
}
bool rtc_error_builder_append_sdp_video_format(
    NativeRtcError &target, const NativeSdpVideoFormat &value) noexcept {
  if (!SdpVideoFormatStringSafe(value.value()))
    return false;
  target.value().string_builder() << value.value();
  return true;
}
std::unique_ptr<NativeRtcError>
rtc_error_stream_sdp_video_format(std::unique_ptr<NativeRtcError> target,
                                  const NativeSdpVideoFormat &value,
                                  bool &valid) noexcept {
  valid = SdpVideoFormatStringSafe(value.value());
  if (!valid)
    return target;
  return std::make_unique<NativeRtcError>(std::move(target->value())
                                          << value.value());
}
std::unique_ptr<NativeSdpVideoFormat>
sdp_video_format_new(rust::Slice<const std::uint8_t> name) noexcept {
  return std::make_unique<NativeSdpVideoFormat>(
      webrtc::SdpVideoFormat(Text(name)));
}
std::unique_ptr<NativeSdpVideoFormat> sdp_video_format_with_parameters(
    rust::Slice<const std::uint8_t> name,
    rust::Slice<const FfiRtpCodecParameter> parameters,
    rust::Slice<const std::uint8_t> modes, bool with_modes) noexcept {
  const auto map = Parameters(parameters);
  return std::make_unique<NativeSdpVideoFormat>(
      with_modes ? webrtc::SdpVideoFormat(Text(name), map, Modes(modes))
                 : webrtc::SdpVideoFormat(Text(name), map));
}
std::unique_ptr<NativeSdpVideoFormat>
sdp_video_format_with_modes(const NativeSdpVideoFormat &source,
                            rust::Slice<const std::uint8_t> modes) noexcept {
  return std::make_unique<NativeSdpVideoFormat>(
      webrtc::SdpVideoFormat(source.value(), Modes(modes)));
}
std::unique_ptr<NativeSdpVideoFormat>
sdp_video_format_clone(const NativeSdpVideoFormat &value) noexcept {
  return std::make_unique<NativeSdpVideoFormat>(value.value());
}
std::unique_ptr<NativeSdpVideoFormat>
sdp_video_format_move(std::unique_ptr<NativeSdpVideoFormat> source) noexcept {
  return std::make_unique<NativeSdpVideoFormat>(std::move(source->value()));
}
void sdp_video_format_assign(NativeSdpVideoFormat &target,
                             const NativeSdpVideoFormat &source) noexcept {
  target.value() = source.value();
}
void sdp_video_format_move_assign(
    NativeSdpVideoFormat &target,
    std::unique_ptr<NativeSdpVideoFormat> source) noexcept {
  target.value() = std::move(source->value());
}
FfiSdpVideoFormatFields
sdp_video_format_fields(const NativeSdpVideoFormat &input) noexcept {
  const auto &value = input.value();
  FfiSdpVideoFormatFields result;
  result.name = Bytes(value.name);
  for (const auto &pair : value.parameters)
    result.parameters.push_back(
        FfiRtpCodecParameter{Bytes(pair.first), Bytes(pair.second)});
  for (auto mode : value.scalability_modes)
    result.scalability_modes.push_back(static_cast<std::uint8_t>(mode));
  result.has_packetization = value.packetization.has_value();
  if (value.packetization)
    result.packetization = Bytes(*value.packetization);
  result.has_tx_mode = value.tx_mode.has_value();
  if (value.tx_mode)
    result.tx_mode = Bytes(*value.tx_mode);
  return result;
}
void sdp_video_format_set_fields(NativeSdpVideoFormat &target,
                                 FfiSdpVideoFormatFields fields) noexcept {
  webrtc::SdpVideoFormat value(Text(fields.name), Parameters(fields.parameters),
                               Modes(fields.scalability_modes));
  if (fields.has_packetization)
    value.packetization = Text(fields.packetization);
  if (fields.has_tx_mode)
    value.tx_mode = Text(fields.tx_mode);
  target.value() = std::move(value);
}
bool sdp_video_format_equal(const NativeSdpVideoFormat &l,
                            const NativeSdpVideoFormat &r) noexcept {
  return l.value() == r.value();
}
bool sdp_video_format_same_codec(const NativeSdpVideoFormat &l,
                                 const NativeSdpVideoFormat &r) noexcept {
  return l.value().IsSameCodec(r.value());
}
rust::Vec<std::uint8_t>
sdp_video_format_string(const NativeSdpVideoFormat &value,
                        bool &valid) noexcept {
  valid = SdpVideoFormatStringSafe(value.value());
  return valid ? Bytes(value.value().ToString()) : rust::Vec<std::uint8_t>();
}
std::unique_ptr<NativeSdpVideoFormat>
sdp_video_format_named(std::uint8_t kind) noexcept {
  switch (kind) {
  case 0:
    return std::make_unique<NativeSdpVideoFormat>(
        webrtc::SdpVideoFormat::VP8());
  case 1:
    return std::make_unique<NativeSdpVideoFormat>(
        webrtc::SdpVideoFormat::H264());
  case 2:
    return std::make_unique<NativeSdpVideoFormat>(
        webrtc::SdpVideoFormat::H265());
  case 3:
    return std::make_unique<NativeSdpVideoFormat>(
        webrtc::SdpVideoFormat::VP9Profile0());
  case 4:
    return std::make_unique<NativeSdpVideoFormat>(
        webrtc::SdpVideoFormat::VP9Profile1());
  case 5:
    return std::make_unique<NativeSdpVideoFormat>(
        webrtc::SdpVideoFormat::VP9Profile2());
  case 6:
    return std::make_unique<NativeSdpVideoFormat>(
        webrtc::SdpVideoFormat::VP9Profile3());
  case 7:
    return std::make_unique<NativeSdpVideoFormat>(
        webrtc::SdpVideoFormat::AV1Profile0());
  case 8:
    return std::make_unique<NativeSdpVideoFormat>(
        webrtc::SdpVideoFormat::AV1Profile1());
  default:
    return nullptr;
  }
}
bool sdp_video_format_list_valid(const NativeSdpVideoFormatList& list) noexcept { return list.valid; }
void sdp_video_format_list_set_valid(NativeSdpVideoFormatList& list, bool valid) noexcept { list.valid = valid; }
std::unique_ptr<NativeSdpVideoFormatList> sdp_video_format_list_new() noexcept {
  return std::make_unique<NativeSdpVideoFormatList>();
}
std::size_t
sdp_video_format_list_len(const NativeSdpVideoFormatList &list) noexcept {
  return list.values.size();
}
std::unique_ptr<NativeSdpVideoFormat>
sdp_video_format_list_get(const NativeSdpVideoFormatList &list,
                          std::size_t index) noexcept {
  return index < list.values.size()
             ? std::make_unique<NativeSdpVideoFormat>(list.values[index])
             : nullptr;
}
void sdp_video_format_list_add(NativeSdpVideoFormatList &list,
                               const NativeSdpVideoFormat &value) noexcept {
  list.values.push_back(value.value());
}
bool sdp_video_format_in_list(const NativeSdpVideoFormat &value,
                              const NativeSdpVideoFormatList &list) noexcept {
  return value.value().IsCodecInList(list.values);
}
std::int32_t
SdpVideoFormatFuzzyMatchStatus(const std::vector<webrtc::SdpVideoFormat> &list,
                               const webrtc::SdpVideoFormat &request) noexcept {
  // Preflight only the reached arithmetic and logging sinks. Do not substitute
  // the candidate/result: the actual SDK helper below performs the match.
  const webrtc::SdpVideoFormat *selected = nullptr;
  std::size_t best = 0;
  for (const auto &format : list) {
    if (!absl::EqualsIgnoreCase(format.name, request.name))
      continue;
    std::size_t matches = 0;
    for (const auto &pair : format.parameters) {
      const auto found = request.parameters.find(pair.first);
      if (found != request.parameters.end() && found->second == pair.second) {
        if (matches ==
            static_cast<std::size_t>(std::numeric_limits<int>::max())) {
          return 2;
        }
        ++matches;
      }
    }
    if (!selected || matches > best) {
      selected = &format;
      best = matches;
    }
  }
#if RTC_LOG_ENABLED()
  if (!selected || *selected != request) {
    // Logging severity is process-global and can change concurrently. When
    // logging is compiled in, these native branches can consume ToString;
    // sampling IsNoop before dispatch would not retain that safety evidence.
    if (!SdpVideoFormatStringSafe(request) ||
        (selected && !SdpVideoFormatStringSafe(*selected))) {
      return 1;
    }
  }
#endif
  return 0;
}
std::unique_ptr<NativeSdpVideoFormat>
sdp_video_format_fuzzy_match(const NativeSdpVideoFormatList &list,
                             const NativeSdpVideoFormat &request,
                             std::int32_t &status) noexcept {
  status = SdpVideoFormatFuzzyMatchStatus(list.values, request.value());
  if (status != 0)
    return nullptr;
  auto result = webrtc::FuzzyMatchSdpVideoFormat(list.values, request.value());
  return result ? std::make_unique<NativeSdpVideoFormat>(std::move(*result))
                : nullptr;
}
std::unique_ptr<NativeRenderResolution> render_resolution_default() noexcept {
  return std::make_unique<NativeRenderResolution>();
}
std::unique_ptr<NativeRenderResolution>
render_resolution_new(std::int32_t width, std::int32_t height) noexcept {
  return std::make_unique<NativeRenderResolution>(
      webrtc::RenderResolution(width, height));
}
std::unique_ptr<NativeRenderResolution>
render_resolution_clone(const NativeRenderResolution &value) noexcept {
  return std::make_unique<NativeRenderResolution>(value.value());
}
void render_resolution_assign(NativeRenderResolution &target,
                              const NativeRenderResolution &source) noexcept {
  target.value() = source.value();
}
std::int32_t
render_resolution_width(const NativeRenderResolution &value) noexcept {
  return value.value().Width();
}
std::int32_t
render_resolution_height(const NativeRenderResolution &value) noexcept {
  return value.value().Height();
}
bool render_resolution_valid(const NativeRenderResolution &value) noexcept {
  return value.value().Valid();
}
bool render_resolution_equal(const NativeRenderResolution &l,
                             const NativeRenderResolution &r) noexcept {
  return l.value() == r.value();
}
} // namespace pulsebeam::webrtc_sys
