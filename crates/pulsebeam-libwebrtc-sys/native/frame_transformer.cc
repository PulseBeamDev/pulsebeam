#include "pulsebeam-webrtc-sys/native/frame_transformer.h"

#include <limits>
#include <optional>
#include <span>

#include "api/frame_transformer_factory.h"
#include "api/video_codecs/video_codec.h"
#include "api/units/timestamp.h"
#include "pulsebeam-webrtc-sys/native/media_values.h"
#include "pulsebeam-webrtc-sys/native/video_metadata.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "rtc_base/checks.h"

namespace pulsebeam::webrtc_sys {
namespace {
const webrtc::TransformableAudioFrameInterface &
Audio(const NativeTransformableFrame &frame) {
  return static_cast<const webrtc::TransformableAudioFrameInterface &>(
      frame.frame());
}
webrtc::TransformableAudioFrameInterface &
Audio(NativeTransformableFrame &frame) {
  return static_cast<webrtc::TransformableAudioFrameInterface &>(frame.frame());
}
const webrtc::TransformableVideoFrameInterface &
Video(const NativeTransformableFrame &frame) {
  return static_cast<const webrtc::TransformableVideoFrameInterface &>(
      frame.frame());
}
webrtc::TransformableVideoFrameInterface &
Video(NativeTransformableFrame &frame) {
  return static_cast<webrtc::TransformableVideoFrameInterface &>(frame.frame());
}
rust::Vec<std::uint8_t> Bytes(const std::string &source) {
  rust::Vec<std::uint8_t> result;
  result.reserve(source.size());
  for (unsigned char byte : source)
    result.push_back(byte);
  return result;
}
bool NamedCodec(webrtc::VideoCodecType codec) {
  switch (codec) {
  case webrtc::kVideoCodecGeneric:
  case webrtc::kVideoCodecVP8:
  case webrtc::kVideoCodecVP9:
  case webrtc::kVideoCodecAV1:
  case webrtc::kVideoCodecH264:
  case webrtc::kVideoCodecH265:
    return true;
  default:
    return false;
  }
}
// Outgoing audio stores rounded milliseconds in uint64_t. Its CaptureTime
// getter constructs Timestamp::Millis from that unsigned value. Guard that
// actual multiplication (and the compiled FromFraction DCHECK), not an
// arbitrary Timestamp input domain. Small negative microseconds round to zero.
bool AudioCaptureMillisSafe(std::uint64_t millis) {
  constexpr auto limit = std::numeric_limits<std::int64_t>::max() / 1000;
#if RTC_DCHECK_IS_ON
  return millis < static_cast<std::uint64_t>(limit);
#else
  return millis <= static_cast<std::uint64_t>(limit);
#endif
}
} // namespace

bool transformable_audio_completion_safe(const NativeTransformableFrame& value, bool sender_target) noexcept {
  const auto& audio = Audio(value);
  if (const auto level = audio.AudioLevel(); level && *level > 127) return false;
  // SendFrame narrows the optional unsigned absolute timestamp to int64_t,
  // then OnSendingRtpFrame/SendRtpAudio construct Millis only if it is positive.
  // Receiver NTP timestamps can also be explicitly forwarded to that sender.
  if (sender_target) {
    const auto capture = audio.AbsoluteCaptureTimestamp();
    if (capture && *capture <= static_cast<std::uint64_t>(std::numeric_limits<std::int64_t>::max()) &&
        !AudioCaptureMillisSafe(*capture)) return false;
  }
  return true;
}
bool transformable_video_completion_safe(const NativeTransformableFrame& value, bool sender_target) noexcept {
  std::int32_t status = 0;
  auto metadata = transformable_video_metadata(value, status);
  if (!metadata) return false;
  const bool sender_frame = value.frame().GetDirection() == webrtc::TransformableFrameInterface::Direction::kSender;
  // Sender consumers use a stock sender's private header directly, but convert
  // receiver Metadata. Receiver consumers convert stock sender Metadata and
  // otherwise extract the receiver's actual (not cached Metadata) header.
  auto header = sender_target == sender_frame
      ? transformable_video_header(value)
      : rtp_video_header_from_metadata(*metadata, status);
  if (!header) return false;
  const auto& actual = header->value();
  // The generic receiver finder checked_casts spatial_index before its
  // graceful upper-range/dependency-count drops. Preserve those native drops.
  if (!sender_target) return !actual.generic || actual.generic->spatial_index >= 0;
  if (value.frame().GetData().size() > static_cast<std::size_t>(std::numeric_limits<int>::max())) return false;
  auto codec = actual.codec;
  if (sender_frame) {
    // GetCodecType is private to the stock sender. Its public immutable MIME
    // identifies the actual packetizer, unlike the mutable metadata codec.
    if (!NamedCodec(value.mime_codec())) return false;
    const auto mime = Video(value).GetMimeType();
    bool found = false;
    for (const auto candidate : {webrtc::kVideoCodecGeneric, webrtc::kVideoCodecVP8, webrtc::kVideoCodecVP9,
                                 webrtc::kVideoCodecAV1, webrtc::kVideoCodecH264, webrtc::kVideoCodecH265}) {
      if (mime == std::string("video/") + webrtc::CodecTypeToPayloadString(candidate)) { codec = candidate; found = true; break; }
    }
    if (!found) return false;
  }
  // The packetizer's codec can differ from the mutable header codec. Validate
  // each actual std::get selection, rather than pretending a codec setter also
  // changed the stock sender's immutable selection.
  if ((codec == webrtc::kVideoCodecVP8 && !std::holds_alternative<webrtc::RTPVideoHeaderVP8>(actual.video_type_header)) ||
      (codec == webrtc::kVideoCodecVP9 && !std::holds_alternative<webrtc::RTPVideoHeaderVP9>(actual.video_type_header)) ||
      (codec == webrtc::kVideoCodecH264 && !std::holds_alternative<webrtc::RTPVideoHeaderH264>(actual.video_type_header))) return false;
#if RTC_DCHECK_IS_ON
  if (codec == webrtc::kVideoCodecVP8 && !value.frame().GetData().empty()) {
    const auto& vp8 = std::get<webrtc::RTPVideoHeaderVP8>(actual.video_type_header);
    if ((vp8.pictureId != webrtc::kNoPictureId && vp8.pictureId < 0) ||
        (vp8.tl0PicIdx != webrtc::kNoTl0PicIdx && (vp8.tl0PicIdx < 0 || vp8.tl0PicIdx > 255)) ||
        (vp8.temporalIdx != webrtc::kNoTemporalIdx ? vp8.temporalIdx > 3 : vp8.layerSync) ||
        (vp8.keyIdx != webrtc::kNoKeyIdx && (vp8.keyIdx < 0 || vp8.keyIdx > 31))) return false;
  }
#endif
  if (codec == webrtc::kVideoCodecVP9) {
    auto vp9 = std::get<webrtc::RTPVideoHeaderVP9>(actual.video_type_header);
    if (vp9.num_spatial_layers > webrtc::kMaxVp9NumberOfSpatialLayers) return false;
    // Match RemoveInactiveSpatialLayers' narrow integer assignments, without
    // copying/reading inactive array prefixes. Its loops are bounded above.
    if (vp9.first_active_layer) {
      vp9.num_spatial_layers -= vp9.first_active_layer;
      vp9.spatial_idx -= vp9.first_active_layer;
    }
    if (vp9.inter_pic_predicted && vp9.flexible_mode &&
        (vp9.num_ref_pics == 0 || vp9.num_ref_pics > webrtc::kMaxVp9RefPics)) return false;
    if (vp9.ss_data_available) {
      if (vp9.num_spatial_layers == 0 || vp9.num_spatial_layers > webrtc::kMaxVp9NumberOfSpatialLayers ||
          vp9.gof.num_frames_in_gof > webrtc::kMaxVp9FramesInGof) return false;
      for (std::size_t i = 0; i < vp9.gof.num_frames_in_gof; ++i)
        if (vp9.gof.num_ref_pics[i] > webrtc::kMaxVp9RefPics) return false;
    }
    if (!value.frame().GetData().empty() && !(vp9.spatial_idx < vp9.num_spatial_layers - 1 || vp9.end_of_picture)) return false;
  }
  if (actual.generic) {
    const auto& generic = *actual.generic;
    if (generic.spatial_index < 0 || generic.spatial_index >= 8 || generic.temporal_index < 0 || generic.temporal_index >= 8) return false;
    for (const auto dependency : generic.dependencies) {
      if (dependency >= generic.frame_id) return false;
      const auto diff = static_cast<std::uint64_t>(generic.frame_id) - static_cast<std::uint64_t>(dependency);
      if (diff > static_cast<std::uint64_t>(std::numeric_limits<int>::max())) return false;
    }
  }
  return true;
}
bool transformable_frame_is_audio(
    const NativeTransformableFrame &value) noexcept {
  return value.audio();
}
rust::Slice<const std::uint8_t>
transformable_frame_data(const NativeTransformableFrame &value) noexcept {
  auto data = value.frame().GetData();
  return {data.data(), data.size()};
}
void transformable_frame_set_data(
    NativeTransformableFrame &value,
    rust::Slice<const std::uint8_t> data) noexcept {
  value.frame().SetData(
      std::span<const std::uint8_t>(data.data(), data.size()));
}
std::uint8_t transformable_frame_payload_type(
    const NativeTransformableFrame &value) noexcept {
  return value.frame().GetPayloadType();
}
bool transformable_frame_can_set_payload_type(
    const NativeTransformableFrame &value) noexcept {
  return value.frame().CanSetPayloadType();
}
bool transformable_frame_set_payload_type(NativeTransformableFrame &value,
                                          std::uint8_t payload) noexcept {
  if (!value.frame().CanSetPayloadType())
    return false;
  value.frame().SetPayloadType(payload);
  return true;
}
std::uint32_t
transformable_frame_ssrc(const NativeTransformableFrame &value) noexcept {
  return value.frame().GetSsrc();
}
std::uint32_t
transformable_frame_timestamp(const NativeTransformableFrame &value) noexcept {
  return value.frame().GetTimestamp();
}
void transformable_frame_set_timestamp(NativeTransformableFrame &value,
                                       std::uint32_t timestamp) noexcept {
  value.frame().SetRTPTimestamp(timestamp);
}
std::int32_t
transformable_frame_direction(const NativeTransformableFrame &value) noexcept {
  return static_cast<std::int32_t>(value.frame().GetDirection());
}
rust::Vec<std::uint8_t>
transformable_frame_mime(const NativeTransformableFrame &value,
                         std::int32_t &status) noexcept {
  status = 0;
  if (!value.audio() && !NamedCodec(value.mime_codec())) {
    status = 2;
    return {};
  }
  return Bytes(value.frame().GetMimeType());
}
FfiTransformableFrameTime
transformable_frame_time(const NativeTransformableFrame &value,
                         std::uint8_t query) noexcept {
  FfiTransformableFrameTime result{};
  std::optional<webrtc::Timestamp> time;
  switch (query) {
  case 0:
    time = value.frame().GetCaptureTimeIdentifier();
    break;
  case 1:
    time = value.frame().GetPresentationTimestamp();
    break;
  case 2:
    time = value.frame().ReceiveTime();
    break;
  case 3:
    if (value.audio() &&
        value.frame().GetDirection() ==
            webrtc::TransformableFrameInterface::Direction::kSender) {
      const auto millis = Audio(value).AbsoluteCaptureTimestamp();
      if (millis && !AudioCaptureMillisSafe(*millis)) {
        result.status = 3;
        return result;
      }
    }
    time = value.frame().CaptureTime();
    break;
  case 4: {
    const auto delta = value.frame().SenderCaptureTimeOffset();
    result.present = delta.has_value();
    if (delta)
      result.bits = delta->IsPlusInfinity()    ? INT64_MAX
                    : delta->IsMinusInfinity() ? INT64_MIN
                                               : delta->us();
    return result;
  }
  default:
    return result;
  }
  result.present = time.has_value();
  if (time)
    result.bits = MediaTimestampBits(*time);
  return result;
}
bool transformable_frame_can_set_capture_time(
    const NativeTransformableFrame &value) noexcept {
  return value.frame().CanSetCaptureTime();
}
std::int32_t
transformable_frame_set_capture_time(NativeTransformableFrame &value,
                                     bool present, std::int64_t bits) noexcept {
  if (!value.frame().CanSetCaptureTime())
    return 1;
  std::optional<webrtc::Timestamp> time;
  if (present) {
    if (bits == INT64_MIN || bits == INT64_MAX)
      return 3;
    time = webrtc::Timestamp::Micros(bits);
    const auto millis = time->ms();
    if (millis < 0 ||
        !AudioCaptureMillisSafe(static_cast<std::uint64_t>(millis)))
      return 3;
  }
  value.frame().SetCaptureTime(time);
  return 0;
}
std::unique_ptr<NativeTransformableFrame>
transformable_frame_clone(NativeTransformableFrame &value,
                          std::int32_t &status) noexcept {
  status = 0;
  if (value.audio()) {
    auto result = std::make_unique<NativeTransformableFrame>(webrtc::CloneAudioFrame(&Audio(value)), true);
    result->set_provenance(value.provenance());
    return result;
  }
  auto metadata = transformable_video_metadata(value, status);
  if (!metadata)
    return nullptr;
  // CloneVideoFrame converts metadata through RTPVideoHeader::FromMetadata.
  auto header = rtp_video_header_from_metadata(*metadata, status);
  if (!header) {
    status = 4;
    return nullptr;
  }
  auto frame = webrtc::CloneVideoFrame(&Video(value));
  auto result = std::make_unique<NativeTransformableFrame>(std::move(frame), false, metadata->value().GetCodec());
  result->set_provenance(value.provenance());
  return result;
}
FfiTransformableAudioFields
transformable_audio_fields(const NativeTransformableFrame &value) noexcept {
  FfiTransformableAudioFields result{};
  const auto &audio = Audio(value);
  result.frame_type = static_cast<std::int32_t>(audio.Type());
  const auto sequence = audio.SequenceNumber();
  result.has_sequence = sequence.has_value();
  result.sequence = sequence.value_or(0);
  const auto capture = audio.AbsoluteCaptureTimestamp();
  result.has_absolute_capture = capture.has_value();
  result.absolute_capture = capture.value_or(0);
  const auto level = audio.AudioLevel();
  result.has_level = level.has_value();
  result.level = level.value_or(0);
  return result;
}
rust::Vec<std::uint32_t>
transformable_audio_csrcs(const NativeTransformableFrame &value) noexcept {
  rust::Vec<std::uint32_t> result;
  for (auto source : Audio(value).GetContributingSources())
    result.push_back(source);
  return result;
}
bool transformable_audio_can_set_level(
    const NativeTransformableFrame &value) noexcept {
  return Audio(value).CanSetAudioLevel();
}
bool transformable_audio_set_level(NativeTransformableFrame &value,
                                   bool present, std::uint8_t level) noexcept {
  if (!Audio(value).CanSetAudioLevel())
    return false;
  Audio(value).SetAudioLevel(present ? std::optional<std::uint8_t>(level)
                                     : std::nullopt);
  return true;
}
bool transformable_video_is_key(
    const NativeTransformableFrame &value) noexcept {
  return Video(value).IsKeyFrame();
}
rust::Vec<std::uint8_t>
transformable_video_rid(const NativeTransformableFrame &value,
                        bool &present) noexcept {
  const auto rid = Video(value).Rid();
  present = rid.has_value();
  return rid ? Bytes(*rid) : rust::Vec<std::uint8_t>();
}
std::unique_ptr<NativeVideoFrameMetadata>
transformable_video_metadata(const NativeTransformableFrame &value,
                             std::int32_t &status) noexcept {
  // Stock sender Metadata consumes GetAsMetadata's codec-selected std::get;
  // receiver Metadata reads its cached value without re-reading the header.
  if (value.frame().GetDirection() ==
      webrtc::TransformableFrameInterface::Direction::kSender) {
    NativeRtpVideoHeader header(Video(value).header());
    auto converted = rtp_video_header_as_metadata(header, status);
    if (!converted) {
      status = 4;
      return nullptr;
    }
  }
  status = 0;
  return std::make_unique<NativeVideoFrameMetadata>(Video(value).Metadata());
}
std::unique_ptr<NativeRtpVideoHeader>
transformable_video_header(const NativeTransformableFrame &value) noexcept {
  return std::make_unique<NativeRtpVideoHeader>(Video(value).header());
}
std::int32_t transformable_video_set_metadata(
    NativeTransformableFrame &value,
    const NativeVideoFrameMetadata &metadata) noexcept {
  if (value.provenance()) {
    const auto current = Video(value).Metadata();
    const auto& next = metadata.value();
    const auto a = current.GetDependencies(), b = next.GetDependencies();
    const bool equal_dependencies = a.has_value() == b.has_value() &&
        (!a || std::equal(a->begin(), a->end(), b->begin(), b->end()));
    const auto ad = current.GetDecodeTargetIndications(), bd = next.GetDecodeTargetIndications();
    // This public SDK has no current sender structure/cardinality hook.
    // Preserve native-generated dependency metadata rather than pretending a
    // frame-local bound or an old request snapshot certifies its consumption.
    if (current.GetFrameId() != next.GetFrameId() || !equal_dependencies ||
        current.GetSpatialIndex() != next.GetSpatialIndex() || current.GetTemporalIndex() != next.GetTemporalIndex() ||
        !std::equal(ad.begin(), ad.end(), bd.begin(), bd.end())) return 7;
  }
  if (value.frame().GetDirection() ==
      webrtc::TransformableFrameInterface::Direction::kReceiver) {
    NativeVideoFrameMetadata selected(Video(value).Metadata());
    selected.value().SetFrameId(metadata.value().GetFrameId());
    selected.value().SetDependencies(metadata.value().GetDependencies());
#if RTC_DCHECK_IS_ON
    const auto equal = video_frame_metadata_equals(selected, metadata);
    if (equal == 0)
      return 5;
    if (equal != 1)
      return 6;
#endif
    // The native setter applies only the two fields and deliberately does not
    // update cached Metadata. Its converted header still needs its std::get.
    std::int32_t status = 0;
    if (!rtp_video_header_from_metadata(selected, status))
      return 4;
  } else {
    std::int32_t status = 0;
    if (!rtp_video_header_from_metadata(metadata, status))
      return 4;
  }
  Video(value).SetMetadata(metadata.value());
  return 0;
}
} // namespace pulsebeam::webrtc_sys
