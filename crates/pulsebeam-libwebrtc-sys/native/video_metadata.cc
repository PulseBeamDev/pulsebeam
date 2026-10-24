#include "pulsebeam-webrtc-sys/native/video_metadata.h"
#include "pulsebeam-webrtc-sys/native/media_values.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

#include "rtc_base/checks.h"
#include <algorithm>
#include <span>
#include <utility>
#include <vector>

namespace pulsebeam::webrtc_sys {
namespace {
FfiVideoHeaderVp8 CopyVp8(const webrtc::RTPVideoHeaderVP8 &value) {
  FfiVideoHeaderVp8 result{};
  result.non_reference = value.nonReference;
  result.picture_id = value.pictureId;
  result.tl0_pic_idx = value.tl0PicIdx;
  result.temporal_idx = value.temporalIdx;
  result.layer_sync = value.layerSync;
  result.key_idx = value.keyIdx;
  result.partition_id = value.partitionId;
  result.beginning_of_partition = value.beginningOfPartition;
  return result;
}
FfiVideoHeaderH264 CopyH264(const webrtc::RTPVideoHeaderH264 &value) {
  FfiVideoHeaderH264 result{};
  result.nalu_type = value.nalu_type;
  result.packetization_type =
      static_cast<std::int32_t>(value.packetization_type);
  result.packetization_mode =
      static_cast<std::int32_t>(value.packetization_mode);
  for (const auto &nalu : value.nalus)
    result.nalus.push_back(
        FfiH264NaluInfo{nalu.type, nalu.sps_id, nalu.pps_id});
  return result;
}
FfiVp9GofInfo CopyGof(const webrtc::GofInfoVP9 &value) {
  FfiVp9GofInfo result{};
  result.num_frames_in_gof = value.num_frames_in_gof;
  result.pid_start = value.pid_start;
  // Copy physical storage, not only SDK-consumed prefixes. Binding values and
  // supported stock frame variants are value-initialized before population.
  for (std::size_t i = 0; i < webrtc::kMaxVp9FramesInGof; ++i) {
    result.temporal_idx.push_back(value.temporal_idx[i]);
    result.temporal_up_switch.push_back(value.temporal_up_switch[i]);
    result.num_ref_pics.push_back(value.num_ref_pics[i]);
    for (std::size_t r = 0; r < webrtc::kMaxVp9RefPics; ++r)
      result.pid_diff.push_back(value.pid_diff[i][r]);
  }
  return result;
}
FfiVideoHeaderVp9 CopyVp9(const webrtc::RTPVideoHeaderVP9 &value) {
  FfiVideoHeaderVp9 result{};
  result.inter_pic_predicted = value.inter_pic_predicted;
  result.flexible_mode = value.flexible_mode;
  result.beginning_of_frame = value.beginning_of_frame;
  result.end_of_frame = value.end_of_frame;
  result.ss_data_available = value.ss_data_available;
  result.non_ref_for_inter_layer_pred = value.non_ref_for_inter_layer_pred;
  result.picture_id = value.picture_id;
  result.max_picture_id = value.max_picture_id;
  result.tl0_pic_idx = value.tl0_pic_idx;
  result.temporal_idx = value.temporal_idx;
  result.spatial_idx = value.spatial_idx;
  result.temporal_up_switch = value.temporal_up_switch;
  result.inter_layer_predicted = value.inter_layer_predicted;
  result.gof_idx = value.gof_idx;
  result.num_ref_pics = value.num_ref_pics;
  result.num_spatial_layers = value.num_spatial_layers;
  result.first_active_layer = value.first_active_layer;
  result.spatial_layer_resolution_present =
      value.spatial_layer_resolution_present;
  result.end_of_picture = value.end_of_picture;
  result.gof = CopyGof(value.gof);
  for (std::size_t i = 0; i < webrtc::kMaxVp9RefPics; ++i) {
    result.pid_diff.push_back(value.pid_diff[i]);
    result.ref_picture_id.push_back(value.ref_picture_id[i]);
  }
  for (std::size_t i = 0; i < webrtc::kMaxVp9NumberOfSpatialLayers; ++i) {
    result.width.push_back(value.width[i]);
    result.height.push_back(value.height[i]);
  }
  return result;
}
} // namespace

FfiVideoHeaderVp8 video_header_vp8_defaults() noexcept {
  webrtc::RTPVideoHeaderVP8 value{};
  value.InitRTPVideoHeaderVP8();
  return CopyVp8(value);
}
FfiVideoHeaderVp9 video_header_vp9_defaults() noexcept {
  webrtc::RTPVideoHeaderVP9 value{};
  value.InitRTPVideoHeaderVP9();
  return CopyVp9(value);
}
FfiVideoHeaderH264 video_header_h264_defaults() noexcept {
  return CopyH264(webrtc::RTPVideoHeaderH264{});
}
FfiVideoHeaderCodecSpecifics video_frame_metadata_specifics(
    const NativeVideoFrameMetadata &source) noexcept {
  FfiVideoHeaderCodecSpecifics result{};
  const auto &value = source.value().GetRTPVideoHeaderCodecSpecifics();
  result.tag = static_cast<std::int32_t>(value.index());
  if (const auto *vp8 = std::get_if<webrtc::RTPVideoHeaderVP8>(&value))
    result.vp8 = CopyVp8(*vp8);
  if (const auto *vp9 = std::get_if<webrtc::RTPVideoHeaderVP9>(&value))
    result.vp9 = CopyVp9(*vp9);
  if (const auto *h264 = std::get_if<webrtc::RTPVideoHeaderH264>(&value))
    result.h264 = CopyH264(*h264);
  return result;
}
std::int32_t video_frame_metadata_set_specifics(
    NativeVideoFrameMetadata &target,
    FfiVideoHeaderCodecSpecifics input) noexcept {
  webrtc::RTPVideoHeaderCodecSpecifics value;
  switch (input.tag) {
  case 0:
    break;
  case 1: {
    auto &vp8 = value.emplace<webrtc::RTPVideoHeaderVP8>();
    vp8.nonReference = input.vp8.non_reference;
    vp8.pictureId = input.vp8.picture_id;
    vp8.tl0PicIdx = input.vp8.tl0_pic_idx;
    vp8.temporalIdx = input.vp8.temporal_idx;
    vp8.layerSync = input.vp8.layer_sync;
    vp8.keyIdx = input.vp8.key_idx;
    vp8.partitionId = input.vp8.partition_id;
    vp8.beginningOfPartition = input.vp8.beginning_of_partition;
    break;
  }
  case 2: {
    const auto &v = input.vp9;
    if (v.pid_diff.size() != webrtc::kMaxVp9RefPics ||
        v.ref_picture_id.size() != webrtc::kMaxVp9RefPics ||
        v.width.size() != webrtc::kMaxVp9NumberOfSpatialLayers ||
        v.height.size() != webrtc::kMaxVp9NumberOfSpatialLayers ||
        v.gof.temporal_idx.size() != webrtc::kMaxVp9FramesInGof ||
        v.gof.temporal_up_switch.size() != webrtc::kMaxVp9FramesInGof ||
        v.gof.num_ref_pics.size() != webrtc::kMaxVp9FramesInGof ||
        v.gof.pid_diff.size() !=
            webrtc::kMaxVp9FramesInGof * webrtc::kMaxVp9RefPics)
      return 3;
    auto &vp9 = value.emplace<webrtc::RTPVideoHeaderVP9>();
    vp9.inter_pic_predicted = v.inter_pic_predicted;
    vp9.flexible_mode = v.flexible_mode;
    vp9.beginning_of_frame = v.beginning_of_frame;
    vp9.end_of_frame = v.end_of_frame;
    vp9.ss_data_available = v.ss_data_available;
    vp9.non_ref_for_inter_layer_pred = v.non_ref_for_inter_layer_pred;
    vp9.picture_id = v.picture_id;
    vp9.max_picture_id = v.max_picture_id;
    vp9.tl0_pic_idx = v.tl0_pic_idx;
    vp9.temporal_idx = v.temporal_idx;
    vp9.spatial_idx = v.spatial_idx;
    vp9.temporal_up_switch = v.temporal_up_switch;
    vp9.inter_layer_predicted = v.inter_layer_predicted;
    vp9.gof_idx = v.gof_idx;
    vp9.num_ref_pics = v.num_ref_pics;
    vp9.num_spatial_layers = v.num_spatial_layers;
    vp9.first_active_layer = v.first_active_layer;
    vp9.spatial_layer_resolution_present = v.spatial_layer_resolution_present;
    vp9.end_of_picture = v.end_of_picture;
    vp9.gof.num_frames_in_gof = v.gof.num_frames_in_gof;
    vp9.gof.pid_start = v.gof.pid_start;
    for (std::size_t i = 0; i < webrtc::kMaxVp9RefPics; ++i) {
      vp9.pid_diff[i] = v.pid_diff[i];
      vp9.ref_picture_id[i] = v.ref_picture_id[i];
    }
    for (std::size_t i = 0; i < webrtc::kMaxVp9NumberOfSpatialLayers; ++i) {
      vp9.width[i] = v.width[i];
      vp9.height[i] = v.height[i];
    }
    for (std::size_t i = 0; i < webrtc::kMaxVp9FramesInGof; ++i) {
      vp9.gof.temporal_idx[i] = v.gof.temporal_idx[i];
      vp9.gof.temporal_up_switch[i] = v.gof.temporal_up_switch[i];
      vp9.gof.num_ref_pics[i] = v.gof.num_ref_pics[i];
      for (std::size_t r = 0; r < webrtc::kMaxVp9RefPics; ++r)
        vp9.gof.pid_diff[i][r] = v.gof.pid_diff[i * webrtc::kMaxVp9RefPics + r];
    }
    break;
  }
  case 3: {
    if (input.h264.packetization_type < 0 || input.h264.packetization_type > 3)
      return 4;
    auto &h264 = value.emplace<webrtc::RTPVideoHeaderH264>();
    h264.nalu_type = input.h264.nalu_type;
    h264.packetization_type = static_cast<webrtc::H264PacketizationTypes>(
        input.h264.packetization_type);
    h264.packetization_mode = static_cast<webrtc::H264PacketizationMode>(
        input.h264.packetization_mode);
    h264.nalus.reserve(input.h264.nalus.size());
    for (const auto &nalu : input.h264.nalus)
      h264.nalus.push_back(
          webrtc::NaluInfo{nalu.type_code, nalu.sps_id, nalu.pps_id});
    break;
  }
  default:
    return 5;
  }
  target.value().SetRTPVideoHeaderCodecSpecifics(std::move(value));
  return 0;
}

namespace {
bool LoadGof(const FfiVp9GofInfo &input, webrtc::GofInfoVP9 &value) {
  if (input.temporal_idx.size() != webrtc::kMaxVp9FramesInGof ||
      input.temporal_up_switch.size() != webrtc::kMaxVp9FramesInGof ||
      input.num_ref_pics.size() != webrtc::kMaxVp9FramesInGof ||
      input.pid_diff.size() !=
          webrtc::kMaxVp9FramesInGof * webrtc::kMaxVp9RefPics)
    return false;
  value.num_frames_in_gof = input.num_frames_in_gof;
  value.pid_start = input.pid_start;
  for (std::size_t i = 0; i < webrtc::kMaxVp9FramesInGof; ++i) {
    value.temporal_idx[i] = input.temporal_idx[i];
    value.temporal_up_switch[i] = input.temporal_up_switch[i];
    value.num_ref_pics[i] = input.num_ref_pics[i];
    for (std::size_t r = 0; r < webrtc::kMaxVp9RefPics; ++r)
      value.pid_diff[i][r] = input.pid_diff[i * webrtc::kMaxVp9RefPics + r];
  }
  return true;
}

// These guards mirror only the SDK equality read order, not its result. A
// mismatch before a potentially unsafe loop permits the actual SDK operator
// to short circuit. Unused or unvisited invalid counts remain admissible.
std::int32_t GofEqualityReadSafety(const webrtc::GofInfoVP9 &l,
                                   const webrtc::GofInfoVP9 &r) {
  if (l.num_frames_in_gof != r.num_frames_in_gof || l.pid_start != r.pid_start)
    return 0;
  for (std::size_t i = 0; i < l.num_frames_in_gof; ++i) {
    if (i >= webrtc::kMaxVp9FramesInGof)
      return 6;
    if (l.temporal_idx[i] != r.temporal_idx[i] ||
        l.temporal_up_switch[i] != r.temporal_up_switch[i] ||
        l.num_ref_pics[i] != r.num_ref_pics[i])
      return 0;
    for (std::size_t p = 0; p < l.num_ref_pics[i]; ++p) {
      if (p >= webrtc::kMaxVp9RefPics)
        return 7;
      if (l.pid_diff[i][p] != r.pid_diff[i][p])
        return 0;
    }
  }
  return 0;
}
std::int32_t Vp9EqualityReadSafety(const webrtc::RTPVideoHeaderVP9 &l,
                                   const webrtc::RTPVideoHeaderVP9 &r) {
  if (l.inter_pic_predicted != r.inter_pic_predicted ||
      l.flexible_mode != r.flexible_mode ||
      l.beginning_of_frame != r.beginning_of_frame ||
      l.end_of_frame != r.end_of_frame ||
      l.ss_data_available != r.ss_data_available ||
      l.non_ref_for_inter_layer_pred != r.non_ref_for_inter_layer_pred ||
      l.picture_id != r.picture_id || l.max_picture_id != r.max_picture_id ||
      l.tl0_pic_idx != r.tl0_pic_idx || l.temporal_idx != r.temporal_idx ||
      l.spatial_idx != r.spatial_idx || l.gof_idx != r.gof_idx ||
      l.temporal_up_switch != r.temporal_up_switch ||
      l.inter_layer_predicted != r.inter_layer_predicted ||
      l.num_ref_pics != r.num_ref_pics || l.end_of_picture != r.end_of_picture)
    return 0;
  for (std::size_t i = 0; i < l.num_ref_pics; ++i) {
    if (i >= webrtc::kMaxVp9RefPics)
      return 9;
    if (l.pid_diff[i] != r.pid_diff[i] ||
        l.ref_picture_id[i] != r.ref_picture_id[i])
      return 0;
  }
  if (l.ss_data_available) {
    if (l.spatial_layer_resolution_present !=
            r.spatial_layer_resolution_present ||
        l.num_spatial_layers != r.num_spatial_layers ||
        l.first_active_layer != r.first_active_layer)
      return 0;
    const auto status = GofEqualityReadSafety(l.gof, r.gof);
    if (status != 0)
      return status;
    if (l.gof != r.gof)
      return 0;
    if (l.spatial_layer_resolution_present) {
      for (std::size_t i = 0; i < l.num_spatial_layers; ++i) {
        if (i >= webrtc::kMaxVp9NumberOfSpatialLayers)
          return 10;
        if (l.width[i] != r.width[i] || l.height[i] != r.height[i])
          return 0;
      }
    }
  }
  return 0;
}
template <typename T>
bool EqualSpans(std::span<const T> l, std::span<const T> r) {
  return l.size() == r.size() && std::equal(l.begin(), l.end(), r.begin());
}
bool EqualMetadataPrefix(const webrtc::VideoFrameMetadata &l,
                         const webrtc::VideoFrameMetadata &r) {
  const auto ld = l.GetDependencies();
  const auto rd = r.GetDependencies();
  const bool dependencies_equal =
      ld.has_value() == rd.has_value() && (!ld || EqualSpans(*ld, *rd));
  return l.GetFrameType() == r.GetFrameType() && l.GetWidth() == r.GetWidth() &&
         l.GetHeight() == r.GetHeight() && l.GetRotation() == r.GetRotation() &&
         l.GetContentType() == r.GetContentType() &&
         l.GetFrameId() == r.GetFrameId() &&
         l.GetSpatialIndex() == r.GetSpatialIndex() &&
         l.GetTemporalIndex() == r.GetTemporalIndex() && dependencies_equal &&
         EqualSpans(l.GetDecodeTargetIndications(),
                    r.GetDecodeTargetIndications()) &&
         l.GetIsLastFrameInPicture() == r.GetIsLastFrameInPicture() &&
         l.GetSimulcastIdx() == r.GetSimulcastIdx() &&
         l.GetCodec() == r.GetCodec();
}
} // namespace

std::int32_t
video_frame_metadata_equals(const NativeVideoFrameMetadata &left,
                            const NativeVideoFrameMetadata &right) noexcept {
  const auto &l = left.value();
  const auto &r = right.value();
  if (EqualMetadataPrefix(l, r)) {
    const auto &lv = l.GetRTPVideoHeaderCodecSpecifics();
    const auto &rv = r.GetRTPVideoHeaderCodecSpecifics();
    const auto *lp = std::get_if<webrtc::RTPVideoHeaderVP9>(&lv);
    const auto *rp = std::get_if<webrtc::RTPVideoHeaderVP9>(&rv);
    if (lp && rp) {
      const auto status = Vp9EqualityReadSafety(*lp, *rp);
      if (status != 0)
        return status;
    }
  }
  return l == r ? 1 : 0;
}
void video_header_vp8_init(FfiVideoHeaderVp8 &input) noexcept {
  NativeVideoFrameMetadata metadata;
  FfiVideoHeaderCodecSpecifics variant{};
  variant.tag = 1;
  variant.vp8 = std::move(input);
  video_frame_metadata_set_specifics(metadata, std::move(variant));
  auto value = std::get<webrtc::RTPVideoHeaderVP8>(
      metadata.value().GetRTPVideoHeaderCodecSpecifics());
  value.InitRTPVideoHeaderVP8();
  input = CopyVp8(value);
}
std::int32_t video_header_vp9_init(FfiVideoHeaderVp9 &input) noexcept {
  NativeVideoFrameMetadata metadata;
  FfiVideoHeaderCodecSpecifics variant{};
  variant.tag = 2;
  variant.vp9 = input;
  const auto status =
      video_frame_metadata_set_specifics(metadata, std::move(variant));
  if (status != 0)
    return status;
  auto value = std::get<webrtc::RTPVideoHeaderVP9>(
      metadata.value().GetRTPVideoHeaderCodecSpecifics());
  value.InitRTPVideoHeaderVP9();
  input = CopyVp9(value);
  return 0;
}
std::int32_t video_header_vp8_equals(FfiVideoHeaderVp8 l,
                                     FfiVideoHeaderVp8 r) noexcept {
  NativeVideoFrameMetadata left, right;
  FfiVideoHeaderCodecSpecifics lv{}, rv{};
  lv.tag = rv.tag = 1;
  lv.vp8 = std::move(l);
  rv.vp8 = std::move(r);
  video_frame_metadata_set_specifics(left, std::move(lv));
  video_frame_metadata_set_specifics(right, std::move(rv));
  return video_frame_metadata_equals(left, right);
}
std::int32_t video_header_vp9_equals(FfiVideoHeaderVp9 l,
                                     FfiVideoHeaderVp9 r) noexcept {
  NativeVideoFrameMetadata left, right;
  FfiVideoHeaderCodecSpecifics lv{}, rv{};
  lv.tag = rv.tag = 2;
  lv.vp9 = std::move(l);
  rv.vp9 = std::move(r);
  auto status = video_frame_metadata_set_specifics(left, std::move(lv));
  if (status != 0)
    return status;
  status = video_frame_metadata_set_specifics(right, std::move(rv));
  if (status != 0)
    return status;
  return video_frame_metadata_equals(left, right);
}
std::int32_t video_header_h264_equals(FfiVideoHeaderH264 l,
                                      FfiVideoHeaderH264 r) noexcept {
  NativeVideoFrameMetadata left, right;
  FfiVideoHeaderCodecSpecifics lv{}, rv{};
  lv.tag = rv.tag = 3;
  lv.h264 = std::move(l);
  rv.h264 = std::move(r);
  auto status = video_frame_metadata_set_specifics(left, std::move(lv));
  if (status != 0)
    return status;
  status = video_frame_metadata_set_specifics(right, std::move(rv));
  if (status != 0)
    return status;
  return video_frame_metadata_equals(left, right);
}
FfiVp9GofInfo CodecGofFields(const webrtc::GofInfoVP9& value) { return CopyGof(value); }
bool CodecGofFromFields(const FfiVp9GofInfo& input, webrtc::GofInfoVP9& value) { return LoadGof(input, value); }
std::int32_t vp9_gof_set_structure(FfiVp9GofInfo &input,
                                   std::int32_t mode) noexcept {
  if (mode < 0 || mode > 3)
    return 11;
#if RTC_DCHECK_IS_ON
  if (mode == 3)
    return 8;
#endif
  webrtc::GofInfoVP9 value{};
  if (!LoadGof(input, value))
    return 3;
  value.SetGofInfoVP9(static_cast<webrtc::TemporalStructureMode>(mode));
  input = CopyGof(value);
  return 0;
}
std::int32_t vp9_gof_copy_prefix(FfiVp9GofInfo &input,
                                 FfiVp9GofInfo source) noexcept {
  webrtc::GofInfoVP9 value{}, src{};
  if (!LoadGof(input, value) || !LoadGof(source, src))
    return 3;
  if (src.num_frames_in_gof > webrtc::kMaxVp9FramesInGof)
    return 6;
  for (std::size_t i = 0; i < src.num_frames_in_gof; ++i)
    if (src.num_ref_pics[i] > webrtc::kMaxVp9RefPics)
      return 7;
  value.CopyGofInfoVP9(src);
  input = CopyGof(value);
  return 0;
}
std::int32_t vp9_gof_equals(FfiVp9GofInfo l, FfiVp9GofInfo r) noexcept {
  webrtc::GofInfoVP9 left{}, right{};
  if (!LoadGof(l, left) || !LoadGof(r, right))
    return 3;
  const auto status = GofEqualityReadSafety(left, right);
  if (status != 0)
    return status;
  return left == right ? 1 : 0;
}

std::unique_ptr<NativeVideoFrameMetadata> video_frame_metadata_new() noexcept {
  return std::make_unique<NativeVideoFrameMetadata>();
}
std::unique_ptr<NativeVideoFrameMetadata>
video_frame_metadata_clone(const NativeVideoFrameMetadata &source) noexcept {
  return std::make_unique<NativeVideoFrameMetadata>(source.value());
}
void video_frame_metadata_assign(
    NativeVideoFrameMetadata &target,
    const NativeVideoFrameMetadata &source) noexcept {
  target.value() = source.value();
}
FfiVideoFrameMetadataValues
video_frame_metadata_values(const NativeVideoFrameMetadata &source) noexcept {
  const auto &value = source.value();
  FfiVideoFrameMetadataValues result{};
  result.frame_type = static_cast<std::int32_t>(value.GetFrameType());
  result.width = value.GetWidth();
  result.height = value.GetHeight();
  result.rotation = static_cast<std::int32_t>(value.GetRotation());
  result.content_type = static_cast<std::uint8_t>(value.GetContentType());
  result.has_frame_id = value.GetFrameId().has_value();
  result.frame_id = value.GetFrameId().value_or(0);
  result.spatial_index = value.GetSpatialIndex();
  result.temporal_index = value.GetTemporalIndex();
  const auto dependencies = value.GetDependencies();
  result.has_dependencies = dependencies.has_value();
  if (dependencies) {
    for (auto dependency : *dependencies)
      result.dependencies.push_back(dependency);
  }
  for (auto indication : value.GetDecodeTargetIndications())
    result.decode_target_indications.push_back(
        static_cast<std::int32_t>(indication));
  result.is_last_frame_in_picture = value.GetIsLastFrameInPicture();
  result.simulcast_idx = value.GetSimulcastIdx();
  result.codec = static_cast<std::int32_t>(value.GetCodec());
  result.ssrc = value.GetSsrc();
  for (auto csrc : value.GetCsrcs())
    result.csrcs.push_back(csrc);
  return result;
}
std::int32_t
video_frame_metadata_set_values(NativeVideoFrameMetadata &target,
                                FfiVideoFrameMetadataValues input) noexcept {
  // These SDK enums have no fixed underlying type. Their C++ representable
  // ranges include unnamed values, but casts outside those ranges are UB.
  // Scoped/fixed-underlying enums below admit their full underlying domain.
  if (input.rotation < 0 || input.rotation > 511)
    return 1;
  if (input.codec < 0 || input.codec > 7)
    return 2;
  auto &value = target.value();
  value.SetFrameType(static_cast<webrtc::VideoFrameType>(input.frame_type));
  value.SetWidth(input.width);
  value.SetHeight(input.height);
  value.SetRotation(static_cast<webrtc::VideoRotation>(input.rotation));
  value.SetContentType(
      static_cast<webrtc::VideoContentType>(input.content_type));
  value.SetFrameId(input.has_frame_id
                       ? std::optional<std::int64_t>(input.frame_id)
                       : std::nullopt);
  value.SetSpatialIndex(input.spatial_index);
  value.SetTemporalIndex(input.temporal_index);
  if (input.has_dependencies) {
    value.SetDependencies(std::span<const std::int64_t>(
        input.dependencies.data(), input.dependencies.size()));
  } else {
    value.SetDependencies(std::nullopt);
  }
  std::vector<webrtc::DecodeTargetIndication> indications;
  indications.reserve(input.decode_target_indications.size());
  for (auto indication : input.decode_target_indications)
    indications.push_back(
        static_cast<webrtc::DecodeTargetIndication>(indication));
  value.SetDecodeTargetIndications(indications);
  value.SetIsLastFrameInPicture(input.is_last_frame_in_picture);
  value.SetSimulcastIdx(input.simulcast_idx);
  value.SetCodec(static_cast<webrtc::VideoCodecType>(input.codec));
  value.SetSsrc(input.ssrc);
  value.SetCsrcs(
      std::vector<std::uint32_t>(input.csrcs.begin(), input.csrcs.end()));
  return 0;
}
namespace {
bool VideoHeaderAssociationSafe(webrtc::VideoCodecType codec, std::size_t tag) {
  switch (codec) {
  case webrtc::kVideoCodecVP8:
    return tag == 1;
  case webrtc::kVideoCodecVP9:
    return tag == 2;
  case webrtc::kVideoCodecH264:
    return tag == 3;
  default:
    return true;
  }
}
} // namespace

std::unique_ptr<NativeRtpVideoHeader> rtp_video_header_new() noexcept {
  return std::make_unique<NativeRtpVideoHeader>();
}
std::unique_ptr<NativeRtpVideoHeader>
rtp_video_header_clone(const NativeRtpVideoHeader &source) noexcept {
  return std::make_unique<NativeRtpVideoHeader>(source.value());
}
void rtp_video_header_assign(NativeRtpVideoHeader &target,
                             const NativeRtpVideoHeader &source) noexcept {
  target.value() = source.value();
}
FfiRtpVideoHeaderValues
rtp_video_header_values(const NativeRtpVideoHeader &source) noexcept {
  const auto &value = source.value();
  FfiRtpVideoHeaderValues out{};
  out.has_generic = value.generic.has_value();
  if (value.generic) {
    const auto &generic = *value.generic;
    out.generic.frame_id = generic.frame_id;
    out.generic.spatial_index = generic.spatial_index;
    out.generic.temporal_index = generic.temporal_index;
    for (auto indication : generic.decode_target_indications)
      out.generic.decode_target_indications.push_back(
          static_cast<std::int32_t>(indication));
    for (auto dependency : generic.dependencies)
      out.generic.dependencies.push_back(dependency);
    for (auto diff : generic.chain_diffs)
      out.generic.chain_diffs.push_back(diff);
    out.generic.active_decode_targets =
        static_cast<std::uint32_t>(generic.active_decode_targets.to_ulong());
  }
  out.frame_type = static_cast<std::int32_t>(value.frame_type);
  out.width = value.width;
  out.height = value.height;
  out.rotation = static_cast<std::int32_t>(value.rotation);
  out.content_type = static_cast<std::uint8_t>(value.content_type);
  out.is_first_packet_in_frame = value.is_first_packet_in_frame;
  out.is_last_packet_in_frame = value.is_last_packet_in_frame;
  out.is_last_frame_in_picture = value.is_last_frame_in_picture;
  out.simulcast_idx = value.simulcastIdx;
  out.codec = static_cast<std::int32_t>(value.codec);
  out.has_playout_delay = value.playout_delay.has_value();
  if (value.playout_delay)
    out.playout_delay = MediaPlayoutToFfi(*value.playout_delay);
  const auto &timing = value.video_timing;
  out.video_timing.encode_start_delta_ms = timing.encode_start_delta_ms;
  out.video_timing.encode_finish_delta_ms = timing.encode_finish_delta_ms;
  out.video_timing.packetization_finish_delta_ms =
      timing.packetization_finish_delta_ms;
  out.video_timing.pacer_exit_delta_ms = timing.pacer_exit_delta_ms;
  out.video_timing.network_timestamp_delta_ms =
      timing.network_timestamp_delta_ms;
  out.video_timing.network2_timestamp_delta_ms =
      timing.network2_timestamp_delta_ms;
  out.video_timing.flags = timing.flags;
  out.has_color_space = value.color_space.has_value();
  if (value.color_space)
    out.color_space = MediaColorToFfi(*value.color_space);
  out.has_video_frame_tracking_id = value.video_frame_tracking_id.has_value();
  out.video_frame_tracking_id = value.video_frame_tracking_id.value_or(0);
  out.video_type_header.tag =
      static_cast<std::int32_t>(value.video_type_header.index());
  if (auto *vp8 =
          std::get_if<webrtc::RTPVideoHeaderVP8>(&value.video_type_header))
    out.video_type_header.vp8 = CopyVp8(*vp8);
  if (auto *vp9 =
          std::get_if<webrtc::RTPVideoHeaderVP9>(&value.video_type_header))
    out.video_type_header.vp9 = CopyVp9(*vp9);
  if (auto *h264 =
          std::get_if<webrtc::RTPVideoHeaderH264>(&value.video_type_header))
    out.video_type_header.h264 = CopyH264(*h264);
  if (auto *legacy = std::get_if<webrtc::RTPVideoHeaderLegacyGeneric>(
          &value.video_type_header))
    out.legacy_generic_picture_id = legacy->picture_id;
  out.has_absolute_capture_time = value.absolute_capture_time.has_value();
  if (value.absolute_capture_time) {
    out.absolute_capture_time.absolute_capture_timestamp =
        value.absolute_capture_time->absolute_capture_timestamp;
    out.absolute_capture_time.has_estimated_capture_clock_offset =
        value.absolute_capture_time->estimated_capture_clock_offset.has_value();
    out.absolute_capture_time.estimated_capture_clock_offset =
        value.absolute_capture_time->estimated_capture_clock_offset.value_or(0);
  }
  if (value.frame_instrumentation_data)
    out.frame_instrumentation_data =
        std::make_unique<NativeFrameInstrumentationData>(
            *value.frame_instrumentation_data);
  return out;
}
std::int32_t
rtp_video_header_set_values(NativeRtpVideoHeader &target,
                            FfiRtpVideoHeaderValues input) noexcept {
  if (input.rotation < 0 || input.rotation > 511)
    return 1;
  if (input.codec < 0 || input.codec > 7)
    return 2;
  webrtc::RTPVideoHeader value;
  if (input.has_generic) {
    auto &generic = value.generic.emplace();
    generic.frame_id = input.generic.frame_id;
    generic.spatial_index = input.generic.spatial_index;
    generic.temporal_index = input.generic.temporal_index;
    for (auto indication : input.generic.decode_target_indications)
      generic.decode_target_indications.push_back(
          static_cast<webrtc::DecodeTargetIndication>(indication));
    generic.dependencies.assign(input.generic.dependencies.begin(),
                                input.generic.dependencies.end());
    generic.chain_diffs.assign(input.generic.chain_diffs.begin(),
                               input.generic.chain_diffs.end());
    generic.active_decode_targets = input.generic.active_decode_targets;
  }
  value.frame_type = static_cast<webrtc::VideoFrameType>(input.frame_type);
  value.width = input.width;
  value.height = input.height;
  value.rotation = static_cast<webrtc::VideoRotation>(input.rotation);
  value.content_type =
      static_cast<webrtc::VideoContentType>(input.content_type);
  value.is_first_packet_in_frame = input.is_first_packet_in_frame;
  value.is_last_packet_in_frame = input.is_last_packet_in_frame;
  value.is_last_frame_in_picture = input.is_last_frame_in_picture;
  value.simulcastIdx = input.simulcast_idx;
  value.codec = static_cast<webrtc::VideoCodecType>(input.codec);
  if (input.has_playout_delay)
    value.playout_delay = MediaPlayoutFromFfi(input.playout_delay);
  auto &timing = value.video_timing;
  timing.encode_start_delta_ms = input.video_timing.encode_start_delta_ms;
  timing.encode_finish_delta_ms = input.video_timing.encode_finish_delta_ms;
  timing.packetization_finish_delta_ms =
      input.video_timing.packetization_finish_delta_ms;
  timing.pacer_exit_delta_ms = input.video_timing.pacer_exit_delta_ms;
  timing.network_timestamp_delta_ms =
      input.video_timing.network_timestamp_delta_ms;
  timing.network2_timestamp_delta_ms =
      input.video_timing.network2_timestamp_delta_ms;
  timing.flags = input.video_timing.flags;
  if (input.has_color_space)
    value.color_space = MediaColorFromFfi(input.color_space);
  if (input.has_video_frame_tracking_id)
    value.video_frame_tracking_id = input.video_frame_tracking_id;
  if (input.video_type_header.tag == 4) {
    value.video_type_header.emplace<webrtc::RTPVideoHeaderLegacyGeneric>()
        .picture_id = input.legacy_generic_picture_id;
  } else {
    NativeVideoFrameMetadata metadata;
    const auto status = video_frame_metadata_set_specifics(
        metadata, std::move(input.video_type_header));
    if (status != 0)
      return status;
    std::visit(
        [&](const auto &specific) { value.video_type_header = specific; },
        metadata.value().GetRTPVideoHeaderCodecSpecifics());
  }
  if (input.has_absolute_capture_time) {
    auto &capture = value.absolute_capture_time.emplace();
    capture.absolute_capture_timestamp =
        input.absolute_capture_time.absolute_capture_timestamp;
    if (input.absolute_capture_time.has_estimated_capture_clock_offset)
      capture.estimated_capture_clock_offset =
          input.absolute_capture_time.estimated_capture_clock_offset;
  }
  if (input.frame_instrumentation_data)
    value.frame_instrumentation_data =
        input.frame_instrumentation_data->value();
  target.value() = value;
  return 0;
}
std::unique_ptr<NativeVideoFrameMetadata>
rtp_video_header_as_metadata(const NativeRtpVideoHeader &source,
                             std::int32_t &status) noexcept {
  if (!VideoHeaderAssociationSafe(source.value().codec,
                                  source.value().video_type_header.index())) {
    status = 9;
    return nullptr;
  }
  status = 0;
  return std::make_unique<NativeVideoFrameMetadata>(
      source.value().GetAsMetadata());
}
std::int32_t rtp_video_header_set_metadata(
    NativeRtpVideoHeader &target,
    const NativeVideoFrameMetadata &metadata) noexcept {
  if (!VideoHeaderAssociationSafe(
          metadata.value().GetCodec(),
          metadata.value().GetRTPVideoHeaderCodecSpecifics().index()))
    return 9;
  target.value().SetFromMetadata(metadata.value());
  return 0;
}
std::unique_ptr<NativeRtpVideoHeader>
rtp_video_header_from_metadata(const NativeVideoFrameMetadata &metadata,
                               std::int32_t &status) noexcept {
  if (!VideoHeaderAssociationSafe(
          metadata.value().GetCodec(),
          metadata.value().GetRTPVideoHeaderCodecSpecifics().index())) {
    status = 9;
    return nullptr;
  }
  status = 0;
  return std::make_unique<NativeRtpVideoHeader>(
      webrtc::RTPVideoHeader::FromMetadata(metadata.value()));
}

std::unique_ptr<NativeFrameInstrumentationData>
frame_instrumentation_new() noexcept {
  return std::make_unique<NativeFrameInstrumentationData>();
}
std::unique_ptr<NativeFrameInstrumentationData> frame_instrumentation_clone(
    const NativeFrameInstrumentationData &source) noexcept {
  return std::make_unique<NativeFrameInstrumentationData>(source.value());
}
void frame_instrumentation_assign(
    NativeFrameInstrumentationData &target,
    const NativeFrameInstrumentationData &source) noexcept {
  target.value() = source.value();
}
rust::Vec<double> frame_instrumentation_samples(
    const NativeFrameInstrumentationData &source) noexcept {
  rust::Vec<double> result;
  for (double sample : source.sample_values())
    result.push_back(sample);
  return result;
}
bool frame_instrumentation_set_samples(
    NativeFrameInstrumentationData &target,
    rust::Slice<const double> samples) noexcept {
  return target.SetSampleValues(
      std::span<const double>(samples.data(), samples.size()));
}
bool frame_instrumentation_set_samples_owned(
    NativeFrameInstrumentationData &target,
    rust::Vec<double> samples) noexcept {
  std::vector<double> owned(samples.begin(), samples.end());
  return target.SetSampleValues(std::move(owned));
}
} // namespace pulsebeam::webrtc_sys
