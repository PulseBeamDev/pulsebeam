#include "pulsebeam-webrtc-sys/native/media_values.h"
#include <algorithm>
#include <limits>
#include <iterator>
#include <optional>
#include <string>
#include <utility>
#include <vector>
#include "api/rtp_headers.h"
#include "api/rtp_packet_info.h"
#include "api/rtp_packet_infos.h"
#include "api/units/time_delta.h"
#include "api/units/timestamp.h"
#include "api/video/color_space.h"
#include "api/video/hdr_metadata.h"
#include "api/video/video_timing.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {
namespace {
constexpr auto kMinus = std::numeric_limits<std::int64_t>::min();
constexpr auto kPlus = std::numeric_limits<std::int64_t>::max();
webrtc::Timestamp Timestamp(std::int64_t us) {
  return us == kMinus ? webrtc::Timestamp::MinusInfinity() :
         us == kPlus ? webrtc::Timestamp::PlusInfinity() : webrtc::Timestamp::Micros(us);
}
webrtc::TimeDelta Delta(std::int64_t us) {
  return us == kMinus ? webrtc::TimeDelta::MinusInfinity() :
         us == kPlus ? webrtc::TimeDelta::PlusInfinity() : webrtc::TimeDelta::Micros(us);
}
template <typename T> std::int64_t Bits(T value) {
  return value.IsMinusInfinity() ? kMinus : value.IsPlusInfinity() ? kPlus : value.us();
}
std::string String(const rust::Vec<std::uint8_t>& bytes) {
  if (bytes.empty()) return {};
  return {reinterpret_cast<const char*>(bytes.data()), bytes.size()};
}
rust::Vec<std::uint8_t> Bytes(const std::string& value) {
  rust::Vec<std::uint8_t> result;
  result.reserve(value.size());
  for (unsigned char byte : value) result.push_back(byte);
  return result;
}
webrtc::AbsoluteCaptureTime Capture(const FfiAbsoluteCaptureTime& value) {
  webrtc::AbsoluteCaptureTime result;
  result.absolute_capture_timestamp = value.absolute_capture_timestamp;
  if (value.has_estimated_capture_clock_offset)
    result.estimated_capture_clock_offset = value.estimated_capture_clock_offset;
  return result;
}
FfiAbsoluteCaptureTime Capture(const webrtc::AbsoluteCaptureTime& value) {
  FfiAbsoluteCaptureTime result{};
  result.absolute_capture_timestamp = value.absolute_capture_timestamp;
  result.has_estimated_capture_clock_offset = value.estimated_capture_clock_offset.has_value();
  result.estimated_capture_clock_offset = value.estimated_capture_clock_offset.value_or(0);
  return result;
}
webrtc::HdrMetadata Hdr(const FfiMediaHdrMetadata& value) {
  webrtc::HdrMetadata result;
  auto& m = result.mastering_metadata;
  m.primary_r.x = value.primary_r_x; m.primary_r.y = value.primary_r_y;
  m.primary_g.x = value.primary_g_x; m.primary_g.y = value.primary_g_y;
  m.primary_b.x = value.primary_b_x; m.primary_b.y = value.primary_b_y;
  m.white_point.x = value.white_point_x; m.white_point.y = value.white_point_y;
  m.luminance_max = value.luminance_max; m.luminance_min = value.luminance_min;
  result.max_content_light_level = value.max_content_light_level;
  result.max_frame_average_light_level = value.max_frame_average_light_level;
  return result;
}
FfiMediaHdrMetadata Hdr(const webrtc::HdrMetadata& value) {
  FfiMediaHdrMetadata result{};
  const auto& m = value.mastering_metadata;
  result.primary_r_x = m.primary_r.x; result.primary_r_y = m.primary_r.y;
  result.primary_g_x = m.primary_g.x; result.primary_g_y = m.primary_g.y;
  result.primary_b_x = m.primary_b.x; result.primary_b_y = m.primary_b.y;
  result.white_point_x = m.white_point.x; result.white_point_y = m.white_point.y;
  result.luminance_max = m.luminance_max; result.luminance_min = m.luminance_min;
  result.max_content_light_level = value.max_content_light_level;
  result.max_frame_average_light_level = value.max_frame_average_light_level;
  return result;
}
webrtc::ColorSpace Color(const FfiMediaColorSpace& value) {
  const auto hdr = Hdr(value.hdr_metadata);
  // These scoped enums have fixed uint8/int underlying types. Unlike the
  // unscoped VideoRotation enum, unnamed raw values are safely representable.
  return webrtc::ColorSpace(static_cast<webrtc::ColorSpace::PrimaryID>(value.primaries),
      static_cast<webrtc::ColorSpace::TransferID>(value.transfer),
      static_cast<webrtc::ColorSpace::MatrixID>(value.matrix),
      static_cast<webrtc::ColorSpace::RangeID>(value.range),
      static_cast<webrtc::ColorSpace::ChromaSiting>(value.chroma_siting_horizontal),
      static_cast<webrtc::ColorSpace::ChromaSiting>(value.chroma_siting_vertical),
      value.has_hdr_metadata ? &hdr : nullptr);
}
FfiMediaColorSpace Color(const webrtc::ColorSpace& value) {
  FfiMediaColorSpace result{};
  result.primaries = static_cast<std::uint8_t>(value.primaries());
  result.transfer = static_cast<std::uint8_t>(value.transfer());
  result.matrix = static_cast<std::uint8_t>(value.matrix());
  result.range = static_cast<std::int32_t>(value.range());
  result.chroma_siting_horizontal = static_cast<std::int32_t>(value.chroma_siting_horizontal());
  result.chroma_siting_vertical = static_cast<std::int32_t>(value.chroma_siting_vertical());
  result.has_hdr_metadata = value.hdr_metadata() != nullptr;
  if (result.has_hdr_metadata) result.hdr_metadata = Hdr(*value.hdr_metadata());
  return result;
}
FfiMediaPlayoutDelay Playout(const webrtc::VideoPlayoutDelay& value) {
  FfiMediaPlayoutDelay result{};
  result.minimum_us = Bits(value.min()); result.maximum_us = Bits(value.max());
  return result;
}
webrtc::VideoPlayoutDelay Playout(const FfiMediaPlayoutDelay& value) {
  return webrtc::VideoPlayoutDelay(Delta(value.minimum_us), Delta(value.maximum_us));
}
}  // namespace

std::int64_t MediaTimestampBits(webrtc::Timestamp value) noexcept { return Bits(value); }
webrtc::ColorSpace MediaColorFromFfi(const FfiMediaColorSpace& value) noexcept { return Color(value); }
FfiMediaColorSpace MediaColorToFfi(const webrtc::ColorSpace& value) noexcept { return Color(value); }
webrtc::VideoPlayoutDelay MediaPlayoutFromFfi(const FfiMediaPlayoutDelay& value) noexcept { return Playout(value); }
FfiMediaPlayoutDelay MediaPlayoutToFfi(const webrtc::VideoPlayoutDelay& value) noexcept { return Playout(value); }

FfiMediaPlayoutDelay media_playout_delay_new(std::int64_t min, std::int64_t max) noexcept {
  return Playout(webrtc::VideoPlayoutDelay(Delta(min), Delta(max)));
}
bool media_playout_delay_set(FfiMediaPlayoutDelay& value, std::int64_t min, std::int64_t max) noexcept {
  auto native = Playout(value);
  if (!native.Set(Delta(min), Delta(max))) return false;
  value = Playout(native);
  return true;
}
bool media_hdr_validate(const FfiMediaHdrMetadata& value) noexcept { return Hdr(value).Validate(); }
rust::String media_color_space_string(const FfiMediaColorSpace& value) noexcept {
  return rust::String(Color(value).AsString());
}
bool media_color_space_set(FfiMediaColorSpace& value, std::uint8_t field, std::uint8_t candidate) noexcept {
  auto native = Color(value);
  bool accepted = false;
  switch (field) {
    case 0: accepted = native.set_primaries_from_uint8(candidate); break;
    case 1: accepted = native.set_transfer_from_uint8(candidate); break;
    case 2: accepted = native.set_matrix_from_uint8(candidate); break;
    case 3: accepted = native.set_range_from_uint8(candidate); break;
    case 4: accepted = native.set_chroma_siting_horizontal_from_uint8(candidate); break;
    case 5: accepted = native.set_chroma_siting_vertical_from_uint8(candidate); break;
  }
  if (accepted) value = Color(native);
  return accepted;
}
std::int64_t media_absolute_send_timestamp(std::uint32_t value) noexcept {
  if (value >= (1u << 24)) return kMinus;
  webrtc::RTPHeaderExtension native;
  native.hasAbsoluteSendTime = true;
  native.absoluteSendTime = value;
  return Bits(native.GetAbsoluteSendTimestamp());
}
std::uint16_t media_video_timing_difference(std::int64_t base, std::int64_t time) noexcept {
  if ((base > 0 && time < kMinus + base) || (base < 0 && time > kPlus + base)) return 0;
  return webrtc::VideoSendTiming::GetDeltaCappedMs(base, time);
}
std::uint16_t media_video_timing_duration(std::int64_t us) noexcept {
  if (us == kMinus || us == kPlus) return 0;
  return webrtc::VideoSendTiming::GetDeltaCappedMs(Delta(us));
}
bool MediaRtpHeaderFromFfi(const FfiMediaRtpHeader& value, webrtc::RTPHeader& result) noexcept {
  const auto& v = value.extension;
  // Largest rotation enumerator is 270: the unscoped enum range is 0..511.
  // Preserve unnamed but representable values, even with a false presence flag.
  if (v.video_rotation < 0 || v.video_rotation > 511 ||
      (v.has_audio_level && (v.audio_level < 0 || v.audio_level > 127))) return false;
  result.markerBit = value.marker; result.payloadType = value.payload_type;
  result.sequenceNumber = value.sequence_number; result.timestamp = value.timestamp;
  result.ssrc = value.ssrc; result.numCSRCs = value.csrc_count;
  std::copy(value.csrcs.begin(), value.csrcs.end(), std::begin(result.arrOfCSRCs));
  result.paddingLength = value.padding_length; result.headerLength = value.header_length;
  auto& n = result.extension;
  n.hasTransmissionTimeOffset = v.has_transmission_time_offset; n.transmissionTimeOffset = v.transmission_time_offset;
  n.hasAbsoluteSendTime = v.has_absolute_send_time; n.absoluteSendTime = v.absolute_send_time;
  n.absolute_capture_time = v.has_absolute_capture_time ? std::optional(Capture(v.absolute_capture_time)) : std::nullopt;
  n.hasTransportSequenceNumber = v.has_transport_sequence_number; n.transportSequenceNumber = v.transport_sequence_number;
  n.feedback_request = v.has_feedback_request ? std::optional(webrtc::FeedbackRequest{
      v.feedback_include_timestamps, v.feedback_sequence_count}) : std::nullopt;
  n.set_audio_level(v.has_audio_level ? std::optional(webrtc::AudioLevel(v.audio_voice_activity, v.audio_level)) : std::nullopt);
  n.hasVideoRotation = v.has_video_rotation; n.videoRotation = static_cast<webrtc::VideoRotation>(v.video_rotation);
  n.hasVideoContentType = v.has_video_content_type; n.videoContentType = static_cast<webrtc::VideoContentType>(v.video_content_type);
  n.has_video_timing = v.has_video_timing;
  n.video_timing.flags = v.video_timing.flags;
  n.video_timing.encode_start_delta_ms = v.video_timing.encode_start_delta_ms;
  n.video_timing.encode_finish_delta_ms = v.video_timing.encode_finish_delta_ms;
  n.video_timing.packetization_finish_delta_ms = v.video_timing.packetization_finish_delta_ms;
  n.video_timing.pacer_exit_delta_ms = v.video_timing.pacer_exit_delta_ms;
  n.video_timing.network_timestamp_delta_ms = v.video_timing.network_timestamp_delta_ms;
  n.video_timing.network2_timestamp_delta_ms = v.video_timing.network2_timestamp_delta_ms;
  n.playout_delay = Playout(v.playout_delay);
  n.stream_id = String(v.stream_id); n.repaired_stream_id = String(v.repaired_stream_id); n.mid = String(v.mid);
  n.color_space = v.has_color_space ? std::optional(Color(v.color_space)) : std::nullopt;
  return true;
}
FfiMediaRtpHeader MediaRtpHeaderToFfi(const webrtc::RTPHeader& value) noexcept {
  FfiMediaRtpHeader result{};
  result.marker = value.markerBit; result.payload_type = value.payloadType;
  result.sequence_number = value.sequenceNumber; result.timestamp = value.timestamp;
  result.ssrc = value.ssrc; result.csrc_count = value.numCSRCs;
  std::copy(std::begin(value.arrOfCSRCs), std::end(value.arrOfCSRCs), result.csrcs.begin());
  result.padding_length = value.paddingLength; result.header_length = value.headerLength;
  const auto& n = value.extension;
  auto& v = result.extension;
  v.has_transmission_time_offset = n.hasTransmissionTimeOffset; v.transmission_time_offset = n.transmissionTimeOffset;
  v.has_absolute_send_time = n.hasAbsoluteSendTime; v.absolute_send_time = n.absoluteSendTime;
  v.has_absolute_capture_time = n.absolute_capture_time.has_value();
  if (n.absolute_capture_time) v.absolute_capture_time = Capture(*n.absolute_capture_time);
  v.has_transport_sequence_number = n.hasTransportSequenceNumber; v.transport_sequence_number = n.transportSequenceNumber;
  v.has_feedback_request = n.feedback_request.has_value();
  if (n.feedback_request) {
    v.feedback_include_timestamps = n.feedback_request->include_timestamps;
    v.feedback_sequence_count = n.feedback_request->sequence_count;
  }
  v.has_audio_level = n.audio_level().has_value();
  if (n.audio_level()) { v.audio_voice_activity = n.audio_level()->voice_activity(); v.audio_level = n.audio_level()->level(); }
  v.has_video_rotation = n.hasVideoRotation; v.video_rotation = static_cast<std::int32_t>(n.videoRotation);
  v.has_video_content_type = n.hasVideoContentType; v.video_content_type = static_cast<std::uint8_t>(n.videoContentType);
  v.has_video_timing = n.has_video_timing;
  v.video_timing.flags = n.video_timing.flags;
  v.video_timing.encode_start_delta_ms = n.video_timing.encode_start_delta_ms;
  v.video_timing.encode_finish_delta_ms = n.video_timing.encode_finish_delta_ms;
  v.video_timing.packetization_finish_delta_ms = n.video_timing.packetization_finish_delta_ms;
  v.video_timing.pacer_exit_delta_ms = n.video_timing.pacer_exit_delta_ms;
  v.video_timing.network_timestamp_delta_ms = n.video_timing.network_timestamp_delta_ms;
  v.video_timing.network2_timestamp_delta_ms = n.video_timing.network2_timestamp_delta_ms;
  v.playout_delay = Playout(n.playout_delay);
  v.stream_id = Bytes(n.stream_id); v.repaired_stream_id = Bytes(n.repaired_stream_id); v.mid = Bytes(n.mid);
  v.has_color_space = n.color_space.has_value();
  if (n.color_space) v.color_space = Color(*n.color_space);
  return result;
}
FfiMediaRtpHeader media_rtp_header_round_trip(const FfiMediaRtpHeader& value, bool& valid) noexcept {
  webrtc::RTPHeader native;
  valid = MediaRtpHeaderFromFfi(value, native);
  // Failed conversion has not initialized the native header's timing scalars.
  // The carrier is not an observation: callers must check valid before use.
  if (!valid) return FfiMediaRtpHeader{};
  return MediaRtpHeaderToFfi(native);
}
webrtc::RtpPacketInfo MediaPacketInfoFromFfi(const FfiRtpReceivedPacketInfo& value) noexcept {
  webrtc::RtpPacketInfo result(value.ssrc, std::vector<std::uint32_t>(value.csrcs.begin(), value.csrcs.end()),
      value.rtp_timestamp, Timestamp(value.receive_time_us));
  result.set_sequence_number(value.sequence_number);
  if (value.has_audio_level) result.set_audio_level(value.audio_level);
  if (value.has_absolute_capture_time) result.set_absolute_capture_time(Capture(value.absolute_capture_time));
  if (value.has_local_capture_clock_offset) result.set_local_capture_clock_offset(Delta(value.local_capture_clock_offset_us));
  return result;
}
FfiRtpReceivedPacketInfo MediaPacketInfoToFfi(const webrtc::RtpPacketInfo& value) noexcept {
  FfiRtpReceivedPacketInfo result{};
  result.sequence_number = value.sequence_number(); result.ssrc = value.ssrc();
  for (auto csrc : value.csrcs()) result.csrcs.push_back(csrc);
  result.rtp_timestamp = value.rtp_timestamp(); result.receive_time_us = Bits(value.receive_time());
  result.has_audio_level = value.audio_level().has_value(); result.audio_level = value.audio_level().value_or(0);
  result.has_absolute_capture_time = value.absolute_capture_time().has_value();
  if (value.absolute_capture_time()) result.absolute_capture_time = Capture(*value.absolute_capture_time());
  result.has_local_capture_clock_offset = value.local_capture_clock_offset().has_value();
  if (value.local_capture_clock_offset()) result.local_capture_clock_offset_us = Bits(*value.local_capture_clock_offset());
  return result;
}
webrtc::RtpPacketInfos MediaPacketInfosFromFfi(rust::Slice<const FfiRtpReceivedPacketInfo> value) noexcept {
  std::vector<webrtc::RtpPacketInfo> entries;
  entries.reserve(value.size());
  for (const auto& entry : value) entries.push_back(MediaPacketInfoFromFfi(entry));
  return webrtc::RtpPacketInfos(std::move(entries));
}
rust::Vec<FfiRtpReceivedPacketInfo> MediaPacketInfosToFfi(const webrtc::RtpPacketInfos& value) noexcept {
  rust::Vec<FfiRtpReceivedPacketInfo> entries;
  entries.reserve(value.size());
  for (const auto& entry : value) entries.push_back(MediaPacketInfoToFfi(entry));
  return entries;
}
FfiRtpReceivedPacketInfo media_packet_info_from_header(const FfiMediaRtpHeader& value, std::int64_t time, bool& valid) noexcept {
  webrtc::RTPHeader native;
  valid = MediaRtpHeaderFromFfi(value, native);
  if (!valid) return MediaPacketInfoToFfi(webrtc::RtpPacketInfo());
  return MediaPacketInfoToFfi(webrtc::RtpPacketInfo(native, Timestamp(time)));
}
FfiRtpReceivedPacketInfo media_packet_info_round_trip(const FfiRtpReceivedPacketInfo& value) noexcept {
  return MediaPacketInfoToFfi(MediaPacketInfoFromFfi(value));
}
rust::Vec<FfiRtpReceivedPacketInfo> media_packet_infos_round_trip(rust::Slice<const FfiRtpReceivedPacketInfo> value) noexcept {
  return MediaPacketInfosToFfi(MediaPacketInfosFromFfi(value));
}
}  // namespace pulsebeam::webrtc_sys
