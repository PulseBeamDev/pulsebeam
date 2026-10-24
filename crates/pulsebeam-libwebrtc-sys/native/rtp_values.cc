#include "pulsebeam-webrtc-sys/native/rtp_values.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <limits>
#include "absl/hash/hash.h"
#include "absl/strings/str_format.h"
#include "pulsebeam-webrtc-sys/native/rtc_error.h"
#include "pulsebeam-webrtc-sys/native/string_builder.h"
namespace pulsebeam::webrtc_sys {
static_assert(std::numeric_limits<int>::digits == 31);
std::unique_ptr<NativeSframeEncrypterInit> sframe_init_new(std::int32_t mode, std::int32_t suite) noexcept {
  return std::make_unique<NativeSframeEncrypterInit>(webrtc::SframeEncrypterInit{
      static_cast<webrtc::SframeMode>(mode), static_cast<webrtc::SframeCipherSuite>(suite)});
}
std::unique_ptr<NativeSframeEncrypterInit> sframe_init_clone(const NativeSframeEncrypterInit& source) noexcept { return std::make_unique<NativeSframeEncrypterInit>(source.value()); }
void sframe_init_assign(NativeSframeEncrypterInit& target, const NativeSframeEncrypterInit& source) noexcept { target.value() = source.value(); }
FfiSframeInit sframe_init_fields(const NativeSframeEncrypterInit& value) noexcept {
  return {static_cast<std::int32_t>(value.value().mode), static_cast<std::int32_t>(value.value().cipher_suite)};
}
void sframe_init_set_mode(NativeSframeEncrypterInit& value, std::int32_t mode) noexcept { value.value().mode = static_cast<webrtc::SframeMode>(mode); }
void sframe_init_set_suite(NativeSframeEncrypterInit& value, std::int32_t suite) noexcept { value.value().cipher_suite = static_cast<webrtc::SframeCipherSuite>(suite); }
std::unique_ptr<NativeFrameDecryptionResult> frame_decryption_result_new(std::int32_t status, std::size_t written) noexcept {
  return std::make_unique<NativeFrameDecryptionResult>(webrtc::FrameDecryptorInterface::Result(
      static_cast<webrtc::FrameDecryptorInterface::Status>(status), written));
}
std::unique_ptr<NativeFrameDecryptionResult> frame_decryption_result_clone(const NativeFrameDecryptionResult& value) noexcept {
  return std::make_unique<NativeFrameDecryptionResult>(value.value());
}
FfiFrameDecryptionResult frame_decryption_result_fields(const NativeFrameDecryptionResult& value) noexcept {
  return {static_cast<std::int32_t>(value.value().status), value.value().bytes_written};
}
bool frame_decryption_result_is_ok(const NativeFrameDecryptionResult& value) noexcept { return value.value().IsOk(); }
void frame_decryption_result_assign(NativeFrameDecryptionResult& target, const NativeFrameDecryptionResult& source) noexcept { target.replace(source.value()); }
void frame_decryption_result_set_status(NativeFrameDecryptionResult& value, std::int32_t status) noexcept { value.replace({static_cast<webrtc::FrameDecryptorInterface::Status>(status), value.value().bytes_written}); }
void frame_decryption_result_set_written(NativeFrameDecryptionResult& value, std::size_t written) noexcept { value.replace({value.value().status, written}); }
std::unique_ptr<NativeRtpResolution> rtp_resolution_new(std::int32_t width, std::int32_t height) noexcept {
  return std::make_unique<NativeRtpResolution>(webrtc::Resolution{width, height});
}
std::unique_ptr<NativeRtpResolution> rtp_resolution_clone(const NativeRtpResolution& source) noexcept { return std::make_unique<NativeRtpResolution>(source.value()); }
void rtp_resolution_assign(NativeRtpResolution& target, const NativeRtpResolution& source) noexcept { target.value() = source.value(); }
bool rtp_resolution_equal(const NativeRtpResolution& lhs, const NativeRtpResolution& rhs) noexcept { return lhs.value() == rhs.value(); }
std::int32_t rtp_resolution_width(const NativeRtpResolution& value) noexcept { return value.value().width; }
std::int32_t rtp_resolution_height(const NativeRtpResolution& value) noexcept { return value.value().height; }
void rtp_resolution_set_width(NativeRtpResolution& value, std::int32_t width) noexcept { value.value().width = width; }
void rtp_resolution_set_height(NativeRtpResolution& value, std::int32_t height) noexcept { value.value().height = height; }
std::int32_t rtp_resolution_pixel_count(const NativeRtpResolution& value, bool& valid) noexcept {
  // Widen only for a consumption-point overflow check. Negative/zero metadata
  // remain constructible and all representable SDK products are accepted.
  const auto product = static_cast<std::int64_t>(value.value().width) * static_cast<std::int64_t>(value.value().height);
  valid = product >= std::numeric_limits<int>::min() && product <= std::numeric_limits<int>::max();
  if (!valid) return 0;
  return value.value().PixelCount();
}
FfiRtpResolutionPair rtp_resolution_to_pair(const NativeRtpResolution& value) noexcept {
  const auto pair = value.value().ToPair();
  FfiRtpResolutionPair output;
  output.first = pair.first;
  output.second = pair.second;
  return output;
}

namespace {
absl::string_view RtpView(rust::Slice<const std::uint8_t> bytes) noexcept { return {reinterpret_cast<const char*>(bytes.data()), bytes.size()}; }
absl::string_view RtpView(const rust::Vec<std::uint8_t>& bytes) noexcept { return {reinterpret_cast<const char*>(bytes.data()), bytes.size()}; }
std::string RtpString(rust::Slice<const std::uint8_t> bytes) { const auto view = RtpView(bytes); return std::string(view.data(), view.size()); }
rust::Vec<std::uint8_t> RtpBytes(absl::string_view bytes) noexcept {
  rust::Vec<std::uint8_t> output; output.reserve(bytes.size());
  for (unsigned char byte : bytes) output.push_back(byte);
  return output;
}
bool MimeKind(webrtc::MediaType kind) noexcept { return kind == webrtc::MediaType::AUDIO || kind == webrtc::MediaType::VIDEO || kind == webrtc::MediaType::DATA; }
}

namespace {
template <typename T> std::int64_t RtpSourceTimeBits(T value) noexcept {
  return value.IsMinusInfinity() ? std::numeric_limits<std::int64_t>::min() :
         value.IsPlusInfinity() ? std::numeric_limits<std::int64_t>::max() : value.us();
}
template <typename T> T RtpSourceTime(std::int64_t bits) noexcept {
  return bits == std::numeric_limits<std::int64_t>::min() ? T::MinusInfinity() :
         bits == std::numeric_limits<std::int64_t>::max() ? T::PlusInfinity() : T::Micros(bits);
}
}
std::unique_ptr<NativeRtpSource> rtp_source_new(const FfiRtpSourceFields& fields) noexcept {
  webrtc::RtpSource::Extensions extensions;
  if (fields.has_audio_level) extensions.audio_level = fields.audio_level;
  if (fields.has_absolute_capture_time) {
    webrtc::AbsoluteCaptureTime capture;
    capture.absolute_capture_timestamp = fields.absolute_capture_time.absolute_capture_timestamp;
    if (fields.absolute_capture_time.has_estimated_capture_clock_offset)
      capture.estimated_capture_clock_offset = fields.absolute_capture_time.estimated_capture_clock_offset;
    extensions.absolute_capture_time = capture;
  }
  if (fields.has_local_capture_clock_offset)
    extensions.local_capture_clock_offset = RtpSourceTime<webrtc::TimeDelta>(fields.local_capture_clock_offset_us);
  return std::make_unique<NativeRtpSource>(webrtc::RtpSource(
      RtpSourceTime<webrtc::Timestamp>(fields.timestamp_us), fields.source_id,
      static_cast<webrtc::RtpSourceType>(fields.source_type), fields.rtp_timestamp, extensions));
}
std::unique_ptr<NativeRtpSource> rtp_source_clone(const NativeRtpSource& value) noexcept {
  return std::make_unique<NativeRtpSource>(value.value());
}
void rtp_source_assign(NativeRtpSource& target, const NativeRtpSource& source) noexcept {
  target.value() = source.value();
}
bool rtp_source_equal(const NativeRtpSource& left, const NativeRtpSource& right) noexcept {
  return left.value() == right.value();
}
FfiRtpSourceFields rtp_source_fields(const NativeRtpSource& value) noexcept {
  const auto& source = value.value();
  FfiRtpSourceFields fields{};
  fields.timestamp_us = RtpSourceTimeBits(source.timestamp());
  fields.source_id = source.source_id();
  fields.source_type = static_cast<std::int32_t>(source.source_type());
  fields.rtp_timestamp = source.rtp_timestamp();
  fields.has_audio_level = source.audio_level().has_value();
  fields.audio_level = source.audio_level().value_or(0);
  const auto capture = source.absolute_capture_time();
  fields.has_absolute_capture_time = capture.has_value();
  if (capture) {
    fields.absolute_capture_time.absolute_capture_timestamp = capture->absolute_capture_timestamp;
    fields.absolute_capture_time.has_estimated_capture_clock_offset = capture->estimated_capture_clock_offset.has_value();
    fields.absolute_capture_time.estimated_capture_clock_offset = capture->estimated_capture_clock_offset.value_or(0);
  }
  const auto offset = source.local_capture_clock_offset();
  fields.has_local_capture_clock_offset = offset.has_value();
  if (offset) fields.local_capture_clock_offset_us = RtpSourceTimeBits(*offset);
  return fields;
}
void rtp_source_set_audio_level(NativeRtpSource& value, bool present, std::uint8_t level) noexcept {
  value.value().set_audio_level(present ? std::optional<std::uint8_t>(level) : std::nullopt);
}
rust::Vec<std::uint8_t> rtp_source_string(const NativeRtpSource& value) noexcept {
  return RtpBytes(absl::StrFormat("%v", value.value()));
}
std::size_t rtp_source_list_len(const NativeRtpSourceList& value) noexcept {
  return value.value().size();
}
std::unique_ptr<NativeRtpSource> rtp_source_list_get(const NativeRtpSourceList& value, std::size_t index) noexcept {
  return std::make_unique<NativeRtpSource>(value.value().at(index));
}
void string_builder_append_rtp_source(NativeStringBuilder& output, const NativeRtpSource& value) noexcept {
  output.value() << value.value();
}
void rtc_error_builder_append_rtp_source(NativeRtcError& output, const NativeRtpSource& value) noexcept {
  output.value().string_builder() << value.value();
}
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_source(std::unique_ptr<NativeRtcError> error, const NativeRtpSource& value) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << value.value());
}
std::unique_ptr<NativeRtcpFeedback> rtcp_feedback_default() noexcept { return std::make_unique<NativeRtcpFeedback>(webrtc::RtcpFeedback()); }
std::unique_ptr<NativeRtcpFeedback> rtcp_feedback_for_type(std::int32_t type) noexcept { return std::make_unique<NativeRtcpFeedback>(webrtc::RtcpFeedback(static_cast<webrtc::RtcpFeedbackType>(type))); }
std::unique_ptr<NativeRtcpFeedback> rtcp_feedback_new(std::int32_t type, std::int32_t message) noexcept { return std::make_unique<NativeRtcpFeedback>(webrtc::RtcpFeedback(static_cast<webrtc::RtcpFeedbackType>(type), static_cast<webrtc::RtcpFeedbackMessageType>(message))); }
std::unique_ptr<NativeRtcpFeedback> rtcp_feedback_clone(const NativeRtcpFeedback& source) noexcept { return std::make_unique<NativeRtcpFeedback>(source.value()); }
void rtcp_feedback_assign(NativeRtcpFeedback& target, const NativeRtcpFeedback& source) noexcept { target.value() = source.value(); }
bool rtcp_feedback_equal(const NativeRtcpFeedback& lhs, const NativeRtcpFeedback& rhs) noexcept { return lhs.value() == rhs.value(); }
FfiRtcpFeedbackFields rtcp_feedback_fields(const NativeRtcpFeedback& value) noexcept {
  FfiRtcpFeedbackFields out;
  out.type_code = static_cast<std::int32_t>(value.value().type);
  out.has_message = value.value().message_type.has_value();
  out.message_code = out.has_message ? static_cast<std::int32_t>(*value.value().message_type) : 0;
  return out;
}
void rtcp_feedback_set_type(NativeRtcpFeedback& value, std::int32_t type) noexcept { value.value().type = static_cast<webrtc::RtcpFeedbackType>(type); }
void rtcp_feedback_set_message(NativeRtcpFeedback& value, bool present, std::int32_t message) noexcept { value.value().message_type = present ? std::optional<webrtc::RtcpFeedbackMessageType>(static_cast<webrtc::RtcpFeedbackMessageType>(message)) : std::nullopt; }
rust::String rtcp_feedback_type_string(std::int32_t type) noexcept { return rust::String(absl::StrFormat("%v", static_cast<webrtc::RtcpFeedbackType>(type))); }
rust::String rtcp_feedback_optional_type_string(bool present, std::int32_t type) noexcept {
  const auto value = present ? std::optional<webrtc::RtcpFeedbackType>(static_cast<webrtc::RtcpFeedbackType>(type)) : std::nullopt;
  return rust::String(absl::StrFormat("%v", value));
}
rust::String rtp_media_type_string(std::int32_t type, bool& valid) noexcept {
  const auto value = static_cast<webrtc::MediaType>(type); valid = MimeKind(value);
  if (!valid) return rust::String();
  return rust::String(absl::StrFormat("%v", value));
}
std::unique_ptr<NativeRtpCodec> rtp_codec_default() noexcept { return std::make_unique<NativeRtpCodec>(webrtc::RtpCodec()); }
std::unique_ptr<NativeRtpCodec> rtp_codec_clone(const NativeRtpCodec& source) noexcept { return std::make_unique<NativeRtpCodec>(source.value()); }
void rtp_codec_assign(NativeRtpCodec& target, const NativeRtpCodec& source) noexcept { target.value() = source.value(); }
bool rtp_codec_equal(const NativeRtpCodec& lhs, const NativeRtpCodec& rhs) noexcept { return lhs.value() == rhs.value(); }
FfiRtpCodecFields rtp_codec_fields(const NativeRtpCodec& value) noexcept {
  FfiRtpCodecFields out; const auto& sdk = value.value();
  out.name_bytes = RtpBytes(sdk.name); out.kind = static_cast<std::int32_t>(sdk.kind);
  out.has_clock_rate = sdk.clock_rate.has_value(); out.clock_rate = sdk.clock_rate.value_or(0);
  out.has_num_channels = sdk.num_channels.has_value(); out.num_channels = sdk.num_channels.value_or(0);
  return out;
}
void rtp_codec_set_name(NativeRtpCodec& value, rust::Slice<const std::uint8_t> name) noexcept { const auto view = RtpView(name); value.value().name.assign(view.data(), view.size()); }
void rtp_codec_set_kind(NativeRtpCodec& value, std::int32_t kind) noexcept { value.value().kind = static_cast<webrtc::MediaType>(kind); }
void rtp_codec_set_clock_rate(NativeRtpCodec& value, bool present, std::int32_t rate) noexcept { value.value().clock_rate = present ? std::optional<int>(rate) : std::nullopt; }
void rtp_codec_set_num_channels(NativeRtpCodec& value, bool present, std::int32_t count) noexcept { value.value().num_channels = present ? std::optional<int>(count) : std::nullopt; }
std::size_t rtp_codec_feedback_len(const NativeRtpCodec& value) noexcept { return value.value().rtcp_feedback.size(); }
std::unique_ptr<NativeRtcpFeedback> rtp_codec_feedback_get(const NativeRtpCodec& value, std::size_t index) noexcept { if (index >= value.value().rtcp_feedback.size()) return nullptr; return std::make_unique<NativeRtcpFeedback>(value.value().rtcp_feedback[index]); }
void rtp_codec_feedback_clear(NativeRtpCodec& value) noexcept { value.value().rtcp_feedback.clear(); }
void rtp_codec_feedback_add(NativeRtpCodec& value, const NativeRtcpFeedback& feedback) noexcept { value.value().rtcp_feedback.push_back(feedback.value()); }
rust::Vec<FfiRtpCodecParameter> rtp_codec_parameters(const NativeRtpCodec& value) noexcept {
  rust::Vec<FfiRtpCodecParameter> out; out.reserve(value.value().parameters.size());
  for (const auto& pair : value.value().parameters) { FfiRtpCodecParameter entry; entry.key = RtpBytes(pair.first); entry.value = RtpBytes(pair.second); out.push_back(std::move(entry)); }
  return out;
}
void rtp_codec_set_parameters(NativeRtpCodec& value, rust::Slice<const FfiRtpCodecParameter> parameters) noexcept {
  webrtc::CodecParameterMap map;
  for (const auto& entry : parameters) {
    const auto key = RtpView(entry.key); const auto val = RtpView(entry.value);
    map[std::string(key.data(), key.size())] = std::string(val.data(), val.size());
  }
  value.value().parameters = std::move(map);
}
rust::Vec<std::uint8_t> rtp_codec_mime_type(const NativeRtpCodec& value, bool& valid) noexcept { valid = MimeKind(value.value().kind); if (!valid) return {}; return RtpBytes(value.value().mime_type()); }
rust::Vec<std::uint8_t> rtp_codec_string(const NativeRtpCodec& value, bool& valid) noexcept { valid = MimeKind(value.value().kind); if (!valid) return {}; return RtpBytes(absl::StrFormat("%v", value.value())); }
bool rtp_codec_is_resiliency(const NativeRtpCodec& value) noexcept { return value.value().IsResiliencyCodec(); }
bool rtp_codec_is_media(const NativeRtpCodec& value) noexcept { return value.value().IsMediaCodec(); }
bool rtp_codec_write_fmtp(const NativeRtpCodec& value, NativeStringBuilder& output) noexcept { return webrtc::WriteFmtpParameters(value.value().parameters, output.value()); }
std::unique_ptr<NativeRtcError> rtp_codec_parse_fmtp(NativeRtpCodec& value, rust::Slice<const std::uint8_t> text) noexcept { return std::make_unique<NativeRtcError>(webrtc::ParseFmtpParameterSet(RtpView(text), value.value().parameters)); }

std::unique_ptr<NativeRtpEncoding> rtp_encoding_default() noexcept { return std::make_unique<NativeRtpEncoding>(webrtc::RtpEncodingParameters()); }
std::unique_ptr<NativeRtpEncoding> rtp_encoding_clone(const NativeRtpEncoding& source) noexcept { return std::make_unique<NativeRtpEncoding>(source.value()); }
void rtp_encoding_assign(NativeRtpEncoding& target, const NativeRtpEncoding& source) noexcept { target.value() = source.value(); }
bool rtp_encoding_equal(const NativeRtpEncoding& lhs, const NativeRtpEncoding& rhs) noexcept { return lhs.value() == rhs.value(); }
FfiRtpEncodingFields rtp_encoding_fields(const NativeRtpEncoding& value) noexcept {
  FfiRtpEncodingFields out; const auto& sdk = value.value();
  out.has_ssrc = sdk.ssrc.has_value(); out.ssrc = sdk.ssrc.value_or(0);
  out.bitrate_priority = sdk.bitrate_priority;
  out.network_priority = static_cast<std::int32_t>(sdk.network_priority);
  out.has_max_bitrate_bps = sdk.max_bitrate_bps.has_value(); out.max_bitrate_bps = sdk.max_bitrate_bps.value_or(0);
  out.has_min_bitrate_bps = sdk.min_bitrate_bps.has_value(); out.min_bitrate_bps = sdk.min_bitrate_bps.value_or(0);
  out.has_max_framerate = sdk.max_framerate.has_value(); out.max_framerate = sdk.max_framerate.value_or(0);
  out.has_num_temporal_layers = sdk.num_temporal_layers.has_value(); out.num_temporal_layers = sdk.num_temporal_layers.value_or(0);
  out.has_scale_resolution_down_by = sdk.scale_resolution_down_by.has_value(); out.scale_resolution_down_by = sdk.scale_resolution_down_by.value_or(0);
  out.active = sdk.active;
  out.request_key_frame = sdk.request_key_frame;
  out.adaptive_ptime = sdk.adaptive_ptime;
  out.has_csrcs = sdk.csrcs.has_value();
  if (sdk.csrcs) { out.csrcs.reserve(sdk.csrcs->size()); for (auto csrc : *sdk.csrcs) out.csrcs.push_back(csrc); }
  out.has_scalability_mode = sdk.scalability_mode.has_value();
  if (sdk.scalability_mode) out.scalability_mode = RtpBytes(*sdk.scalability_mode);
  out.rid = RtpBytes(sdk.rid);
  return out;
}
void rtp_encoding_set_ssrc(NativeRtpEncoding& value, bool present, std::uint32_t input) noexcept { value.value().ssrc = present ? std::optional<std::uint32_t>(input) : std::nullopt; }
void rtp_encoding_set_bitrate_priority(NativeRtpEncoding& value, double input) noexcept { value.value().bitrate_priority = input; }
void rtp_encoding_set_network_priority(NativeRtpEncoding& value, std::int32_t input) noexcept { value.value().network_priority = static_cast<webrtc::Priority>(input); }
void rtp_encoding_set_max_bitrate_bps(NativeRtpEncoding& value, bool present, int input) noexcept { value.value().max_bitrate_bps = present ? std::optional<int>(input) : std::nullopt; }
void rtp_encoding_set_min_bitrate_bps(NativeRtpEncoding& value, bool present, int input) noexcept { value.value().min_bitrate_bps = present ? std::optional<int>(input) : std::nullopt; }
void rtp_encoding_set_max_framerate(NativeRtpEncoding& value, bool present, double input) noexcept { value.value().max_framerate = present ? std::optional<double>(input) : std::nullopt; }
void rtp_encoding_set_num_temporal_layers(NativeRtpEncoding& value, bool present, int input) noexcept { value.value().num_temporal_layers = present ? std::optional<int>(input) : std::nullopt; }
void rtp_encoding_set_scale_resolution_down_by(NativeRtpEncoding& value, bool present, double input) noexcept { value.value().scale_resolution_down_by = present ? std::optional<double>(input) : std::nullopt; }
void rtp_encoding_set_active(NativeRtpEncoding& value, bool input) noexcept { value.value().active = input; }
void rtp_encoding_set_request_key_frame(NativeRtpEncoding& value, bool input) noexcept { value.value().request_key_frame = input; }
void rtp_encoding_set_adaptive_ptime(NativeRtpEncoding& value, bool input) noexcept { value.value().adaptive_ptime = input; }

void rtp_encoding_set_csrcs(NativeRtpEncoding& value, bool present, rust::Slice<const std::uint32_t> csrcs) noexcept {
  if (!present) { value.value().csrcs.reset(); return; }
  value.value().csrcs = std::vector<std::uint32_t>(csrcs.begin(), csrcs.end());
}
void rtp_encoding_set_rid(NativeRtpEncoding& value, rust::Slice<const std::uint8_t> rid) noexcept { const auto view = RtpView(rid); value.value().rid.assign(view.data(), view.size()); }
void rtp_encoding_set_scalability_mode(NativeRtpEncoding& value, bool present, rust::Slice<const std::uint8_t> mode) noexcept {
  if (!present) { value.value().scalability_mode.reset(); return; }
  const auto view = RtpView(mode); value.value().scalability_mode = std::string(view.data(), view.size());
}
std::unique_ptr<NativeRtpResolution> rtp_encoding_scale_to(const NativeRtpEncoding& value) noexcept { if (!value.value().scale_resolution_down_to) return nullptr; return std::make_unique<NativeRtpResolution>(*value.value().scale_resolution_down_to); }
void rtp_encoding_set_scale_to(NativeRtpEncoding& value, const NativeRtpResolution& resolution) noexcept { value.value().scale_resolution_down_to = resolution.value(); }
void rtp_encoding_clear_scale_to(NativeRtpEncoding& value) noexcept { value.value().scale_resolution_down_to.reset(); }
std::unique_ptr<NativeRtpCodec> rtp_encoding_codec(const NativeRtpEncoding& value) noexcept { if (!value.value().codec) return nullptr; return std::make_unique<NativeRtpCodec>(*value.value().codec); }
void rtp_encoding_set_codec(NativeRtpEncoding& value, const NativeRtpCodec& codec) noexcept { value.value().codec = codec.value(); }
void rtp_encoding_clear_codec(NativeRtpEncoding& value) noexcept { value.value().codec.reset(); }
rust::Vec<std::uint8_t> rtp_encoding_string(const NativeRtpEncoding& value, bool& valid) noexcept {
  valid = !value.value().codec || MimeKind(value.value().codec->kind);
  if (!valid) return {};
  return RtpBytes(absl::StrFormat("%v", value.value()));
}
std::size_t rtp_encoding_list_len(const NativeRtpEncodingList& values) noexcept { return values.value().size(); }
std::unique_ptr<NativeRtpEncoding> rtp_encoding_list_get(const NativeRtpEncodingList& values, std::size_t index) noexcept { if (index >= values.value().size()) return nullptr; return std::make_unique<NativeRtpEncoding>(values.value()[index]); }

std::unique_ptr<NativeRtpTransceiverInit> rtp_transceiver_init_default() noexcept { return std::make_unique<NativeRtpTransceiverInit>(webrtc::RtpTransceiverInit()); }
std::unique_ptr<NativeRtpTransceiverInit> rtp_transceiver_init_clone(const NativeRtpTransceiverInit& source) noexcept { return std::make_unique<NativeRtpTransceiverInit>(source.value()); }
void rtp_transceiver_init_assign(NativeRtpTransceiverInit& target, const NativeRtpTransceiverInit& source) noexcept { target.value() = source.value(); }
std::int32_t rtp_transceiver_init_direction(const NativeRtpTransceiverInit& value) noexcept { return static_cast<std::int32_t>(value.value().direction); }
void rtp_transceiver_init_set_direction(NativeRtpTransceiverInit& value, std::int32_t direction) noexcept { value.value().direction = static_cast<webrtc::RtpTransceiverDirection>(direction); }
std::size_t rtp_transceiver_init_stream_ids_len(const NativeRtpTransceiverInit& value) noexcept { return value.value().stream_ids.size(); }
rust::Vec<std::uint8_t> rtp_transceiver_init_stream_id_get(const NativeRtpTransceiverInit& value, std::size_t index) noexcept { if (index >= value.value().stream_ids.size()) return {}; return RtpBytes(value.value().stream_ids[index]); }
void rtp_transceiver_init_stream_ids_clear(NativeRtpTransceiverInit& value) noexcept { value.value().stream_ids.clear(); }
void rtp_transceiver_init_stream_id_add(NativeRtpTransceiverInit& value, rust::Slice<const std::uint8_t> id) noexcept { const auto view = RtpView(id); value.value().stream_ids.emplace_back(view.data(), view.size()); }
std::size_t rtp_transceiver_init_encodings_len(const NativeRtpTransceiverInit& value) noexcept { return value.value().send_encodings.size(); }
std::unique_ptr<NativeRtpEncoding> rtp_transceiver_init_encoding_get(const NativeRtpTransceiverInit& value, std::size_t index) noexcept { if (index >= value.value().send_encodings.size()) return nullptr; return std::make_unique<NativeRtpEncoding>(value.value().send_encodings[index]); }
void rtp_transceiver_init_encodings_clear(NativeRtpTransceiverInit& value) noexcept { value.value().send_encodings.clear(); }
void rtp_transceiver_init_encoding_add(NativeRtpTransceiverInit& value, const NativeRtpEncoding& encoding) noexcept { value.value().send_encodings.push_back(encoding.value()); }

std::unique_ptr<NativePriorityValue> priority_value_new(std::uint16_t value) noexcept { return std::make_unique<NativePriorityValue>(webrtc::PriorityValue(value)); }
std::unique_ptr<NativePriorityValue> priority_value_from_priority(std::int32_t priority) noexcept { if (priority < 0 || priority > 3) return nullptr; return std::make_unique<NativePriorityValue>(webrtc::PriorityValue(static_cast<webrtc::Priority>(priority))); }
std::unique_ptr<NativePriorityValue> priority_value_clone(const NativePriorityValue& source) noexcept { return std::make_unique<NativePriorityValue>(source.value()); }
void priority_value_assign(NativePriorityValue& target, const NativePriorityValue& source) noexcept { target.value() = source.value(); }
const std::uint16_t& priority_value_ref(const NativePriorityValue& value) noexcept { return value.value().value(); }
std::uint16_t& priority_value_mut(NativePriorityValue& value) noexcept { return value.value().value(); }
const std::uint16_t& priority_value_deref(const NativePriorityValue& value) noexcept { return *value.value(); }
std::uint16_t& priority_value_deref_mut(NativePriorityValue& value) noexcept { return *value.value(); }
const std::uint16_t& priority_value_arrow(const NativePriorityValue& value) noexcept { return *value.value().operator->(); }
std::uint16_t& priority_value_arrow_mut(NativePriorityValue& value) noexcept { return *value.value().operator->(); }
const std::uint16_t& priority_value_conversion(const NativePriorityValue& value) noexcept { return static_cast<const std::uint16_t&>(value.value()); }
std::uint16_t priority_value_into_value(std::unique_ptr<NativePriorityValue> value) noexcept { return std::move(value->value()).value(); }
std::uint16_t priority_value_into_const_value(std::unique_ptr<NativePriorityValue> value) noexcept { return std::move(std::as_const(value->value())).value(); }
std::uint16_t priority_value_into_deref(std::unique_ptr<NativePriorityValue> value) noexcept { return *std::move(value->value()); }
std::uint16_t priority_value_into_const_deref(std::unique_ptr<NativePriorityValue> value) noexcept { return *std::move(std::as_const(value->value())); }
bool priority_value_equal(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept { return lhs.value() == rhs.value(); }
bool priority_value_not_equal(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept { return lhs.value() != rhs.value(); }
bool priority_value_less(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept { return lhs.value() < rhs.value(); }
bool priority_value_less_equal(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept { return lhs.value() <= rhs.value(); }
bool priority_value_greater(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept { return lhs.value() > rhs.value(); }
bool priority_value_greater_equal(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept { return lhs.value() >= rhs.value(); }

std::unique_ptr<NativeRtpCodecParameters> rtp_codec_parameters_default() noexcept { return std::make_unique<NativeRtpCodecParameters>(webrtc::RtpCodecParameters{}); }
std::unique_ptr<NativeRtpCodecParameters> rtp_codec_parameters_clone(const NativeRtpCodecParameters& source) noexcept { return std::make_unique<NativeRtpCodecParameters>(source.value()); }
void rtp_codec_parameters_assign(NativeRtpCodecParameters& target, const NativeRtpCodecParameters& source) noexcept { target.value() = source.value(); }
bool rtp_codec_parameters_equal(const NativeRtpCodecParameters& lhs, const NativeRtpCodecParameters& rhs) noexcept { return lhs.value() == rhs.value(); }
std::unique_ptr<NativeRtcpParameters> rtcp_parameters_default() noexcept { return std::make_unique<NativeRtcpParameters>(webrtc::RtcpParameters{}); }
std::unique_ptr<NativeRtcpParameters> rtcp_parameters_clone(const NativeRtcpParameters& source) noexcept { return std::make_unique<NativeRtcpParameters>(source.value()); }
void rtcp_parameters_assign(NativeRtcpParameters& target, const NativeRtcpParameters& source) noexcept { target.value() = source.value(); }
bool rtcp_parameters_equal(const NativeRtcpParameters& lhs, const NativeRtcpParameters& rhs) noexcept { return lhs.value() == rhs.value(); }

std::int32_t rtp_codec_parameters_payload_type(const NativeRtpCodecParameters& value) noexcept { return value.value().payload_type; }
void rtp_codec_parameters_set_payload_type(NativeRtpCodecParameters& value, std::int32_t payload) noexcept { value.value().payload_type = payload; }
std::unique_ptr<NativeRtpCodec> rtp_codec_parameters_base(const NativeRtpCodecParameters& value) noexcept { return std::make_unique<NativeRtpCodec>(static_cast<const webrtc::RtpCodec&>(value.value())); }
void rtp_codec_parameters_set_base(NativeRtpCodecParameters& value, const NativeRtpCodec& codec) noexcept { static_cast<webrtc::RtpCodec&>(value.value()) = codec.value(); }
rust::Vec<std::uint8_t> rtp_codec_parameters_mime_type(const NativeRtpCodecParameters& value, bool& valid) noexcept { valid = MimeKind(value.value().kind); if (!valid) return {}; return RtpBytes(value.value().mime_type()); }
rust::Vec<std::uint8_t> rtp_codec_parameters_string(const NativeRtpCodecParameters& value, bool& valid) noexcept { valid = MimeKind(value.value().kind); if (!valid) return {}; return RtpBytes(absl::StrFormat("%v", value.value())); }
bool rtp_codec_parameters_is_resiliency(const NativeRtpCodecParameters& value) noexcept { return value.value().IsResiliencyCodec(); }
bool rtp_codec_parameters_is_media(const NativeRtpCodecParameters& value) noexcept { return value.value().IsMediaCodec(); }
FfiRtcpParametersFields rtcp_parameters_fields(const NativeRtcpParameters& value) noexcept { const auto& sdk = value.value(); FfiRtcpParametersFields out; out.has_ssrc = sdk.ssrc.has_value(); out.ssrc = sdk.ssrc.value_or(0); out.cname_bytes = RtpBytes(sdk.cname); out.reduced_size = sdk.reduced_size; out.mux = sdk.mux; return out; }
void rtcp_parameters_set_ssrc(NativeRtcpParameters& value, bool present, std::uint32_t ssrc) noexcept { value.value().ssrc = present ? std::optional<std::uint32_t>(ssrc) : std::nullopt; }
void rtcp_parameters_set_cname(NativeRtcpParameters& value, rust::Slice<const std::uint8_t> cname) noexcept { const auto view = RtpView(cname); value.value().cname.assign(view.data(), view.size()); }
void rtcp_parameters_set_reduced_size(NativeRtcpParameters& value, bool reduced) noexcept { value.value().reduced_size = reduced; }
void rtcp_parameters_set_mux(NativeRtcpParameters& value, bool mux) noexcept { value.value().mux = mux; }
rust::Vec<std::uint8_t> rtcp_parameters_string(const NativeRtcpParameters& value) noexcept { return RtpBytes(absl::StrFormat("%v", value.value())); }


bool string_builder_append_codec_parameters(NativeStringBuilder& output, const NativeRtpCodecParameters& input) noexcept { if (!(MimeKind(input.value().kind))) return false; output.value() << input.value(); return true; }
bool rtc_error_builder_append_codec_parameters(NativeRtcError& output, const NativeRtpCodecParameters& input) noexcept { if (!(MimeKind(input.value().kind))) return false; output.value().string_builder() << input.value(); return true; }
std::unique_ptr<NativeRtcError> rtc_error_stream_codec_parameters(std::unique_ptr<NativeRtcError> error, const NativeRtpCodecParameters& input, bool& valid) noexcept { valid = MimeKind(input.value().kind); if (!valid) return error; return std::make_unique<NativeRtcError>(std::move(error->value()) << input.value()); }
bool string_builder_append_rtcp_parameters(NativeStringBuilder& output, const NativeRtcpParameters& input) noexcept { if (!(true)) return false; output.value() << input.value(); return true; }
bool rtc_error_builder_append_rtcp_parameters(NativeRtcError& output, const NativeRtcpParameters& input) noexcept { if (!(true)) return false; output.value().string_builder() << input.value(); return true; }
std::unique_ptr<NativeRtcError> rtc_error_stream_rtcp_parameters(std::unique_ptr<NativeRtcError> error, const NativeRtcpParameters& input, bool& valid) noexcept { valid = true; if (!valid) return error; return std::make_unique<NativeRtcError>(std::move(error->value()) << input.value()); }


std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_default() noexcept { return std::make_unique<NativeRtpHeaderExtensionId>(webrtc::RtpHeaderExtensionId()); }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_not_set() noexcept { return std::make_unique<NativeRtpHeaderExtensionId>(webrtc::RtpHeaderExtensionId::NotSet()); }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_new(std::int32_t value) noexcept { return std::make_unique<NativeRtpHeaderExtensionId>(webrtc::RtpHeaderExtensionId(value)); }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_constant(std::uint32_t index) noexcept { switch (index) { case 0: return std::make_unique<NativeRtpHeaderExtensionId>(webrtc::RtpHeaderExtensionId::kMinId); case 1: return std::make_unique<NativeRtpHeaderExtensionId>(webrtc::RtpHeaderExtensionId::kMaxId); case 2: return std::make_unique<NativeRtpHeaderExtensionId>(webrtc::RtpHeaderExtensionId::kOneByteHeaderExtensionMaxId); default: return nullptr; } }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_clone(const NativeRtpHeaderExtensionId& source) noexcept { return std::make_unique<NativeRtpHeaderExtensionId>(source.value()); }
void rtp_header_id_assign(NativeRtpHeaderExtensionId& target, const NativeRtpHeaderExtensionId& source) noexcept { target.value() = source.value(); }
std::int32_t rtp_header_id_to_int(const NativeRtpHeaderExtensionId& value) noexcept { return value.value().operator int(); }
bool rtp_header_id_valid(const NativeRtpHeaderExtensionId& value) noexcept { return value.value().Valid(); }
bool rtp_header_id_is_set(const NativeRtpHeaderExtensionId& value) noexcept { return value.value().IsSet(); }
rust::Vec<std::uint8_t> rtp_header_id_string(const NativeRtpHeaderExtensionId& value) noexcept { return RtpBytes(absl::StrFormat("%v", value.value())); }
std::size_t rtp_header_id_hash(const NativeRtpHeaderExtensionId& value) noexcept { return absl::Hash<webrtc::RtpHeaderExtensionId>{}(value.value()); }
const std::int32_t& rtp_header_id_ref(const NativeRtpHeaderExtensionId& value) noexcept { return value.value().value(); }
std::int32_t& rtp_header_id_ref_mut(NativeRtpHeaderExtensionId& value) noexcept { return value.value().value(); }
const std::int32_t& rtp_header_id_deref(const NativeRtpHeaderExtensionId& value) noexcept { return *value.value(); }
std::int32_t& rtp_header_id_deref_mut(NativeRtpHeaderExtensionId& value) noexcept { return *value.value(); }
const std::int32_t& rtp_header_id_arrow(const NativeRtpHeaderExtensionId& value) noexcept { return *value.value().operator->(); }
std::int32_t& rtp_header_id_arrow_mut(NativeRtpHeaderExtensionId& value) noexcept { return *value.value().operator->(); }
const std::int32_t& rtp_header_id_conversion(const NativeRtpHeaderExtensionId& value) noexcept { return value.value().operator const int&(); }
std::int32_t rtp_header_id_into_value(std::unique_ptr<NativeRtpHeaderExtensionId> value) noexcept { return std::move(value->value()).value(); }
std::int32_t rtp_header_id_into_const_value(std::unique_ptr<NativeRtpHeaderExtensionId> value) noexcept { return std::move(std::as_const(value->value())).value(); }
std::int32_t rtp_header_id_into_deref(std::unique_ptr<NativeRtpHeaderExtensionId> value) noexcept { return *std::move(value->value()); }
std::int32_t rtp_header_id_into_const_deref(std::unique_ptr<NativeRtpHeaderExtensionId> value) noexcept { return *std::move(std::as_const(value->value())); }
bool rtp_header_id_equal(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept { return lhs.value() == rhs.value(); }
bool rtp_header_id_not_equal(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept { return lhs.value() != rhs.value(); }
bool rtp_header_id_less(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept { return lhs.value() < rhs.value(); }
bool rtp_header_id_less_equal(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept { return lhs.value() <= rhs.value(); }
bool rtp_header_id_greater(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept { return lhs.value() > rhs.value(); }
bool rtp_header_id_greater_equal(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept { return lhs.value() >= rhs.value(); }

// Filter is an unscoped, non-fixed enum: its representable range is 0..3,
// not the full int domain. The extra value 3 is harmless in the SDK helpers.

std::unique_ptr<NativeRtpExtension> rtp_extension_default() noexcept { return std::make_unique<NativeRtpExtension>(webrtc::RtpExtension()); }
std::unique_ptr<NativeRtpExtension> rtp_extension_new(rust::Slice<const std::uint8_t> uri, const NativeRtpHeaderExtensionId& id, bool explicit_encrypt, bool encrypt) noexcept { if (!id.value().Valid()) return nullptr; if (explicit_encrypt) return std::make_unique<NativeRtpExtension>(webrtc::RtpExtension(RtpView(uri), id.value(), encrypt)); return std::make_unique<NativeRtpExtension>(webrtc::RtpExtension(RtpView(uri), id.value())); }
std::unique_ptr<NativeRtpExtension> rtp_extension_new_int(rust::Slice<const std::uint8_t> uri, std::int32_t id, bool explicit_encrypt, bool encrypt) noexcept { if (!webrtc::RtpHeaderExtensionId(id).Valid()) return nullptr; if (explicit_encrypt) return std::make_unique<NativeRtpExtension>(webrtc::RtpExtension(RtpView(uri), id, encrypt)); return std::make_unique<NativeRtpExtension>(webrtc::RtpExtension(RtpView(uri), id)); }
std::unique_ptr<NativeRtpExtension> rtp_extension_clone(const NativeRtpExtension& value) noexcept { return std::make_unique<NativeRtpExtension>(value.value()); }
void rtp_extension_assign(NativeRtpExtension& target, const NativeRtpExtension& source) noexcept { target.value() = source.value(); }
bool rtp_extension_equal(const NativeRtpExtension& lhs, const NativeRtpExtension& rhs) noexcept { return lhs.value() == rhs.value(); }
FfiRtpExtensionFields rtp_extension_fields(const NativeRtpExtension& value) noexcept { FfiRtpExtensionFields out; out.uri_bytes = RtpBytes(value.value().uri); out.encrypt = value.value().encrypt; return out; }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_id(const NativeRtpExtension& value) noexcept { return std::make_unique<NativeRtpHeaderExtensionId>(value.value().id); }
void rtp_extension_set_uri(NativeRtpExtension& value, rust::Slice<const std::uint8_t> uri) noexcept { const auto view = RtpView(uri); value.value().uri.assign(view.data(), view.size()); }
void rtp_extension_set_id(NativeRtpExtension& value, const NativeRtpHeaderExtensionId& id) noexcept { value.value().id = id.value(); }
void rtp_extension_set_encrypt(NativeRtpExtension& value, bool encrypt) noexcept { value.value().encrypt = encrypt; }
rust::Vec<std::uint8_t> rtp_extension_to_string(const NativeRtpExtension& value) noexcept { return RtpBytes(value.value().ToString()); }
rust::Vec<std::uint8_t> rtp_extension_string(const NativeRtpExtension& value) noexcept { return RtpBytes(absl::StrFormat("%v", value.value())); }
bool rtp_extension_supported_audio(rust::Slice<const std::uint8_t> uri) noexcept { return webrtc::RtpExtension::IsSupportedForAudio(RtpView(uri)); }
bool rtp_extension_supported_video(rust::Slice<const std::uint8_t> uri) noexcept { return webrtc::RtpExtension::IsSupportedForVideo(RtpView(uri)); }
bool rtp_extension_encryption_supported(rust::Slice<const std::uint8_t> uri) noexcept { return webrtc::RtpExtension::IsEncryptionSupported(RtpView(uri)); }
std::unique_ptr<NativeRtpExtensionList> rtp_extension_list_new() noexcept { return std::make_unique<NativeRtpExtensionList>(); }
std::unique_ptr<NativeRtpExtensionList> rtp_extension_list_clone(const NativeRtpExtensionList& values) noexcept { return std::make_unique<NativeRtpExtensionList>(values.value()); }
void rtp_extension_list_assign(NativeRtpExtensionList& target, const NativeRtpExtensionList& source) noexcept { target.value() = source.value(); }
std::size_t rtp_extension_list_len(const NativeRtpExtensionList& values) noexcept { return values.value().size(); }
void rtp_extension_list_add(NativeRtpExtensionList& values, const NativeRtpExtension& value) noexcept { values.value().push_back(value.value()); }
void rtp_extension_list_clear(NativeRtpExtensionList& values) noexcept { values.value().clear(); }
std::unique_ptr<NativeRtpExtension> rtp_extension_list_get(const NativeRtpExtensionList& values, std::size_t index) noexcept { if (index >= values.value().size()) return nullptr; return std::make_unique<NativeRtpExtension>(values.value()[index]); }
std::unique_ptr<NativeRtpExtensionBorrowed> rtp_extension_list_find(const NativeRtpExtensionList& values, rust::Slice<const std::uint8_t> uri, std::int32_t filter, bool& valid) noexcept { valid = filter >= 0 && filter <= 3; if (!valid) return nullptr; const auto* result = webrtc::RtpExtension::FindHeaderExtensionByUri(values.value(), RtpView(uri), static_cast<webrtc::RtpExtension::Filter>(filter)); return result ? std::make_unique<NativeRtpExtensionBorrowed>(result) : nullptr; }
std::unique_ptr<NativeRtpExtensionBorrowed> rtp_extension_list_find_encryption(const NativeRtpExtensionList& values, rust::Slice<const std::uint8_t> uri, bool encrypt) noexcept { const auto* result = webrtc::RtpExtension::FindHeaderExtensionByUriAndEncryption(values.value(), RtpView(uri), encrypt); return result ? std::make_unique<NativeRtpExtensionBorrowed>(result) : nullptr; }
std::unique_ptr<NativeRtpExtensionList> rtp_extension_list_deduplicate(const NativeRtpExtensionList& values, std::int32_t filter) noexcept { if (filter < 0 || filter > 3) return nullptr; return std::make_unique<NativeRtpExtensionList>(webrtc::RtpExtension::DeduplicateHeaderExtensions(values.value(), static_cast<webrtc::RtpExtension::Filter>(filter))); }
std::unique_ptr<NativeRtpExtension> rtp_extension_borrowed_clone(const NativeRtpExtensionBorrowed& value) noexcept { return std::make_unique<NativeRtpExtension>(value.value()); }
FfiRtpExtensionFields rtp_extension_borrowed_fields(const NativeRtpExtensionBorrowed& value) noexcept { FfiRtpExtensionFields out; out.uri_bytes = RtpBytes(value.value().uri); out.encrypt = value.value().encrypt; return out; }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_borrowed_id(const NativeRtpExtensionBorrowed& value) noexcept { return std::make_unique<NativeRtpHeaderExtensionId>(value.value().id); }
rust::Vec<std::uint8_t> rtp_extension_borrowed_to_string(const NativeRtpExtensionBorrowed& value) noexcept { return RtpBytes(value.value().ToString()); }
rust::Vec<std::uint8_t> rtp_extension_borrowed_string(const NativeRtpExtensionBorrowed& value) noexcept { return RtpBytes(absl::StrFormat("%v", value.value())); }

rust::Vec<std::uint8_t> rtp_extension_encrypt_header_extensions_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kEncryptHeaderExtensionsUri); }
rust::Vec<std::uint8_t> rtp_extension_audio_level_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kAudioLevelUri); }
rust::Vec<std::uint8_t> rtp_extension_timestamp_offset_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kTimestampOffsetUri); }
rust::Vec<std::uint8_t> rtp_extension_abs_send_time_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kAbsSendTimeUri); }
rust::Vec<std::uint8_t> rtp_extension_absolute_capture_time_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kAbsoluteCaptureTimeUri); }
rust::Vec<std::uint8_t> rtp_extension_video_rotation_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kVideoRotationUri); }
rust::Vec<std::uint8_t> rtp_extension_video_content_type_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kVideoContentTypeUri); }
rust::Vec<std::uint8_t> rtp_extension_video_timing_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kVideoTimingUri); }
rust::Vec<std::uint8_t> rtp_extension_generic_frame_descriptor_uri_00() noexcept { return RtpBytes(webrtc::RtpExtension::kGenericFrameDescriptorUri00); }
rust::Vec<std::uint8_t> rtp_extension_dependency_descriptor_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kDependencyDescriptorUri); }
rust::Vec<std::uint8_t> rtp_extension_video_layers_allocation_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kVideoLayersAllocationUri); }
rust::Vec<std::uint8_t> rtp_extension_transport_sequence_number_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kTransportSequenceNumberUri); }
rust::Vec<std::uint8_t> rtp_extension_transport_sequence_number_v2_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kTransportSequenceNumberV2Uri); }
rust::Vec<std::uint8_t> rtp_extension_playout_delay_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kPlayoutDelayUri); }
rust::Vec<std::uint8_t> rtp_extension_color_space_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kColorSpaceUri); }
rust::Vec<std::uint8_t> rtp_extension_mid_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kMidUri); }
rust::Vec<std::uint8_t> rtp_extension_rid_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kRidUri); }
rust::Vec<std::uint8_t> rtp_extension_repaired_rid_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kRepairedRidUri); }
rust::Vec<std::uint8_t> rtp_extension_video_frame_tracking_id_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kVideoFrameTrackingIdUri); }
rust::Vec<std::uint8_t> rtp_extension_csrc_audio_levels_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kCsrcAudioLevelsUri); }
rust::Vec<std::uint8_t> rtp_extension_corruption_detection_uri() noexcept { return RtpBytes(webrtc::RtpExtension::kCorruptionDetectionUri); }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_min_id() noexcept { return std::make_unique<NativeRtpHeaderExtensionId>(webrtc::RtpExtension::kMinId); }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_max_id() noexcept { return std::make_unique<NativeRtpHeaderExtensionId>(webrtc::RtpExtension::kMaxId); }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_one_byte_max_id() noexcept { return std::make_unique<NativeRtpHeaderExtensionId>(webrtc::RtpExtension::kOneByteHeaderExtensionMaxId); }
std::int32_t rtp_extension_max_value_size() noexcept { return webrtc::RtpExtension::kMaxValueSize; }
std::int32_t rtp_extension_one_byte_max_value_size() noexcept { return webrtc::RtpExtension::kOneByteHeaderExtensionMaxValueSize; }

void string_builder_append_header_extension_id(NativeStringBuilder& output, const NativeRtpHeaderExtensionId& input) noexcept { output.value() << input.value(); }
void rtc_error_builder_append_header_extension_id(NativeRtcError& output, const NativeRtpHeaderExtensionId& input) noexcept { output.value().string_builder() << input.value(); }
std::unique_ptr<NativeRtcError> rtc_error_stream_header_extension_id(std::unique_ptr<NativeRtcError> error, const NativeRtpHeaderExtensionId& input) noexcept { return std::make_unique<NativeRtcError>(std::move(error->value()) << input.value()); }
void string_builder_append_rtp_extension(NativeStringBuilder& output, const NativeRtpExtension& input) noexcept { output.value() << input.value(); }
void rtc_error_builder_append_rtp_extension(NativeRtcError& output, const NativeRtpExtension& input) noexcept { output.value().string_builder() << input.value(); }
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_extension(std::unique_ptr<NativeRtcError> error, const NativeRtpExtension& input) noexcept { return std::make_unique<NativeRtcError>(std::move(error->value()) << input.value()); }
void string_builder_append_rtp_extension_ref(NativeStringBuilder& output, const NativeRtpExtensionBorrowed& input) noexcept { output.value() << input.value(); }
void rtc_error_builder_append_rtp_extension_ref(NativeRtcError& output, const NativeRtpExtensionBorrowed& input) noexcept { output.value().string_builder() << input.value(); }
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_extension_ref(std::unique_ptr<NativeRtcError> error, const NativeRtpExtensionBorrowed& input) noexcept { return std::make_unique<NativeRtcError>(std::move(error->value()) << input.value()); }



std::unique_ptr<NativeRtpCodecCapability> rtp_codec_capability_default() noexcept { return std::make_unique<NativeRtpCodecCapability>(webrtc::RtpCodecCapability()); }
std::unique_ptr<NativeRtpCodecCapability> rtp_codec_capability_clone(const NativeRtpCodecCapability& value) noexcept { return std::make_unique<NativeRtpCodecCapability>(value.value()); }
void rtp_codec_capability_assign(NativeRtpCodecCapability& target, const NativeRtpCodecCapability& source) noexcept { target.value() = source.value(); }
bool rtp_codec_capability_equal(const NativeRtpCodecCapability& lhs, const NativeRtpCodecCapability& rhs) noexcept { return lhs.value() == rhs.value(); }
std::unique_ptr<NativeRtpHeaderExtensionCapability> rtp_header_capability_default() noexcept { return std::make_unique<NativeRtpHeaderExtensionCapability>(webrtc::RtpHeaderExtensionCapability()); }
std::unique_ptr<NativeRtpHeaderExtensionCapability> rtp_header_capability_clone(const NativeRtpHeaderExtensionCapability& value) noexcept { return std::make_unique<NativeRtpHeaderExtensionCapability>(value.value()); }
void rtp_header_capability_assign(NativeRtpHeaderExtensionCapability& target, const NativeRtpHeaderExtensionCapability& source) noexcept { target.value() = source.value(); }
bool rtp_header_capability_equal(const NativeRtpHeaderExtensionCapability& lhs, const NativeRtpHeaderExtensionCapability& rhs) noexcept { return lhs.value() == rhs.value(); }
std::unique_ptr<NativeRtpCapabilities> rtp_capabilities_default() noexcept { return std::make_unique<NativeRtpCapabilities>(webrtc::RtpCapabilities()); }
std::unique_ptr<NativeRtpCapabilities> rtp_capabilities_clone(const NativeRtpCapabilities& value) noexcept { return std::make_unique<NativeRtpCapabilities>(value.value()); }
void rtp_capabilities_assign(NativeRtpCapabilities& target, const NativeRtpCapabilities& source) noexcept { target.value() = source.value(); }
bool rtp_capabilities_equal(const NativeRtpCapabilities& lhs, const NativeRtpCapabilities& rhs) noexcept { return lhs.value() == rhs.value(); }
FfiRtpCodecCapabilityFields rtp_codec_capability_fields(const NativeRtpCodecCapability& value) noexcept { FfiRtpCodecCapabilityFields out; out.has_payload_type = value.value().preferred_payload_type.has_value(); out.payload_type = value.value().preferred_payload_type.value_or(0); return out; }
void rtp_codec_capability_set_payload_type(NativeRtpCodecCapability& value, bool present, std::int32_t input) noexcept { value.value().preferred_payload_type = present ? std::optional<int>(input) : std::nullopt; }
std::unique_ptr<NativeRtpCodec> rtp_codec_capability_base(const NativeRtpCodecCapability& value) noexcept { return std::make_unique<NativeRtpCodec>(static_cast<const webrtc::RtpCodec&>(value.value())); }
void rtp_codec_capability_set_base(NativeRtpCodecCapability& value, const NativeRtpCodec& codec) noexcept { static_cast<webrtc::RtpCodec&>(value.value()) = codec.value(); }
rust::Vec<std::uint8_t> rtp_codec_capability_modes(const NativeRtpCodecCapability& value) noexcept { rust::Vec<std::uint8_t> out; for (auto mode : value.value().scalability_modes) out.push_back(static_cast<std::uint8_t>(mode)); return out; }
void rtp_codec_capability_set_modes(NativeRtpCodecCapability& value, rust::Slice<const std::uint8_t> modes) noexcept { value.value().scalability_modes.clear(); for (auto mode : modes) value.value().scalability_modes.push_back(static_cast<webrtc::ScalabilityMode>(mode)); }
// Unlike the base formatter, the capability formatter uses an audio equality
// check and otherwise prints video. Unknown kinds and modes are safe here.
rust::Vec<std::uint8_t> rtp_codec_capability_string(const NativeRtpCodecCapability& value) noexcept { return RtpBytes(absl::StrFormat("%v", value.value())); }
rust::Vec<std::uint8_t> rtp_codec_capability_mime_type(const NativeRtpCodecCapability& value, bool& valid) noexcept { valid = MimeKind(value.value().kind); if (!valid) return {}; return RtpBytes(value.value().mime_type()); }
bool rtp_codec_capability_is_resiliency(const NativeRtpCodecCapability& value) noexcept { return value.value().IsResiliencyCodec(); }
bool rtp_codec_capability_is_media(const NativeRtpCodecCapability& value) noexcept { return value.value().IsMediaCodec(); }
std::unique_ptr<NativeRtpHeaderExtensionCapability> rtp_header_capability_new(rust::Slice<const std::uint8_t> uri, const NativeRtpHeaderExtensionId& id, bool encrypt, std::int32_t direction, std::uint8_t constructor) noexcept {
  const auto bytes = RtpString(uri);
  const auto d = static_cast<webrtc::RtpTransceiverDirection>(direction);
  switch (constructor) {
    case 0: return std::make_unique<NativeRtpHeaderExtensionCapability>(webrtc::RtpHeaderExtensionCapability(bytes));
    case 1: return std::make_unique<NativeRtpHeaderExtensionCapability>(webrtc::RtpHeaderExtensionCapability(bytes, id.value()));
    case 2: return std::make_unique<NativeRtpHeaderExtensionCapability>(webrtc::RtpHeaderExtensionCapability(bytes, id.value(), d));
    default: return std::make_unique<NativeRtpHeaderExtensionCapability>(webrtc::RtpHeaderExtensionCapability(bytes, id.value(), encrypt, d));
  }
}
FfiRtpHeaderCapabilityFields rtp_header_capability_fields(const NativeRtpHeaderExtensionCapability& value) noexcept { FfiRtpHeaderCapabilityFields out; out.uri_bytes = RtpBytes(value.value().uri); out.encrypt = value.value().preferred_encrypt; out.direction = static_cast<std::int32_t>(value.value().direction); return out; }
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_capability_id(const NativeRtpHeaderExtensionCapability& value) noexcept { if (!value.value().preferred_id) return nullptr; return std::make_unique<NativeRtpHeaderExtensionId>(*value.value().preferred_id); }
void rtp_header_capability_set_id(NativeRtpHeaderExtensionCapability& value, const NativeRtpHeaderExtensionId& id) noexcept { value.value().preferred_id = id.value(); }
void rtp_header_capability_clear_id(NativeRtpHeaderExtensionCapability& value) noexcept { value.value().preferred_id.reset(); }
void rtp_header_capability_set_uri(NativeRtpHeaderExtensionCapability& value, rust::Slice<const std::uint8_t> uri) noexcept { value.value().uri = RtpString(uri); }
void rtp_header_capability_set_encrypt(NativeRtpHeaderExtensionCapability& value, bool encrypt) noexcept { value.value().preferred_encrypt = encrypt; }
void rtp_header_capability_set_direction(NativeRtpHeaderExtensionCapability& value, std::int32_t direction) noexcept { value.value().direction = static_cast<webrtc::RtpTransceiverDirection>(direction); }
rust::Vec<std::int32_t> rtp_capabilities_fec(const NativeRtpCapabilities& value) noexcept { rust::Vec<std::int32_t> out; for (auto fec : value.value().fec) out.push_back(static_cast<std::int32_t>(fec)); return out; }
void rtp_capabilities_set_fec(NativeRtpCapabilities& value, rust::Slice<const std::int32_t> fec) noexcept { value.value().fec.clear(); for (auto mechanism : fec) value.value().fec.push_back(static_cast<webrtc::FecMechanism>(mechanism)); }
rust::Vec<std::uint8_t> rtp_direction_string(std::int32_t direction) noexcept { return RtpBytes(absl::StrFormat("%v", static_cast<webrtc::RtpTransceiverDirection>(direction))); }
void string_builder_append_rtp_direction(NativeStringBuilder& output, std::int32_t direction) noexcept { output.value() << static_cast<webrtc::RtpTransceiverDirection>(direction); }
void rtc_error_builder_append_rtp_direction(NativeRtcError& output, std::int32_t direction) noexcept { output.value().string_builder() << static_cast<webrtc::RtpTransceiverDirection>(direction); }
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_direction(std::unique_ptr<NativeRtcError> error, std::int32_t direction) noexcept { return std::make_unique<NativeRtcError>(std::move(error->value()) << static_cast<webrtc::RtpTransceiverDirection>(direction)); }
rust::Vec<std::uint8_t> rtp_scalability_modes_all() noexcept { rust::Vec<std::uint8_t> out; for (auto mode : webrtc::kAllScalabilityModes) out.push_back(static_cast<std::uint8_t>(mode)); return out; }
rust::Vec<std::uint8_t> rtp_scalability_mode_string(bool present, std::uint8_t mode, bool& valid) noexcept {
  valid = !present || mode < webrtc::kScalabilityModeCount;
  if (!valid) return {};
  return RtpBytes(webrtc::ScalabilityModeToString(present ? std::optional<webrtc::ScalabilityMode>(static_cast<webrtc::ScalabilityMode>(mode)) : std::nullopt));
}
void string_builder_append_rtp_codec_capability(NativeStringBuilder& output, const NativeRtpCodecCapability& input) noexcept { output.value() << input.value(); }
void rtc_error_builder_append_rtp_codec_capability(NativeRtcError& output, const NativeRtpCodecCapability& input) noexcept { output.value().string_builder() << input.value(); }
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_codec_capability(std::unique_ptr<NativeRtcError> error, const NativeRtpCodecCapability& input) noexcept { return std::make_unique<NativeRtcError>(std::move(error->value()) << input.value()); }
std::size_t rtp_capabilities_codecs_len(const NativeRtpCapabilities& value) noexcept { return value.value().codecs.size(); }
std::unique_ptr<NativeRtpCodecCapability> rtp_capabilities_codecs_get(const NativeRtpCapabilities& value, std::size_t index) noexcept { if (index >= value.value().codecs.size()) return nullptr; return std::make_unique<NativeRtpCodecCapability>(value.value().codecs[index]); }
void rtp_capabilities_codecs_clear(NativeRtpCapabilities& value) noexcept { value.value().codecs.clear(); }
void rtp_capabilities_codecs_add(NativeRtpCapabilities& value, const NativeRtpCodecCapability& element) noexcept { value.value().codecs.push_back(element.value()); }
std::size_t rtp_capabilities_header_extensions_len(const NativeRtpCapabilities& value) noexcept { return value.value().header_extensions.size(); }
std::unique_ptr<NativeRtpHeaderExtensionCapability> rtp_capabilities_header_extensions_get(const NativeRtpCapabilities& value, std::size_t index) noexcept { if (index >= value.value().header_extensions.size()) return nullptr; return std::make_unique<NativeRtpHeaderExtensionCapability>(value.value().header_extensions[index]); }
void rtp_capabilities_header_extensions_clear(NativeRtpCapabilities& value) noexcept { value.value().header_extensions.clear(); }
void rtp_capabilities_header_extensions_add(NativeRtpCapabilities& value, const NativeRtpHeaderExtensionCapability& element) noexcept { value.value().header_extensions.push_back(element.value()); }

namespace {
bool RtpParametersFormattingSafe(const webrtc::RtpParameters& p) noexcept {
  if (p.degradation_preference) {
    const auto raw = static_cast<std::int32_t>(*p.degradation_preference);
    if (raw < 0 || raw > 3) return false;
  }
  for (const auto& codec : p.codecs) if (!MimeKind(codec.kind)) return false;
  for (const auto& encoding : p.encodings) if (encoding.codec && !MimeKind(encoding.codec->kind)) return false;
  return true;
}
}  // namespace
std::unique_ptr<NativeRtpParameters> rtp_parameters_default() noexcept { return std::make_unique<NativeRtpParameters>(webrtc::RtpParameters()); }
std::unique_ptr<NativeRtpParameters> rtp_parameters_clone(const NativeRtpParameters& value) noexcept { return std::make_unique<NativeRtpParameters>(value.value()); }
void rtp_parameters_assign(NativeRtpParameters& target, const NativeRtpParameters& source) noexcept { target.value() = source.value(); }
bool rtp_parameters_equal(const NativeRtpParameters& lhs, const NativeRtpParameters& rhs) noexcept { return lhs.value() == rhs.value(); }
bool rtp_parameters_not_equal(const NativeRtpParameters& lhs, const NativeRtpParameters& rhs) noexcept { return lhs.value() != rhs.value(); }
FfiRtpParametersFields rtp_parameters_fields(const NativeRtpParameters& value) noexcept { FfiRtpParametersFields out; const auto& p = value.value(); out.transaction_id_bytes = RtpBytes(p.transaction_id); out.mid_bytes = RtpBytes(p.mid); out.has_degradation_preference = p.degradation_preference.has_value(); out.degradation_preference = p.degradation_preference ? static_cast<std::int32_t>(*p.degradation_preference) : 0; return out; }
void rtp_parameters_set_transaction_id(NativeRtpParameters& value, rust::Slice<const std::uint8_t> bytes) noexcept { const auto view = RtpView(bytes); value.value().transaction_id.assign(view.data(), view.size()); }
void rtp_parameters_set_mid(NativeRtpParameters& value, rust::Slice<const std::uint8_t> bytes) noexcept { const auto view = RtpView(bytes); value.value().mid.assign(view.data(), view.size()); }
void rtp_parameters_set_degradation_preference(NativeRtpParameters& value, bool present, std::int32_t raw) noexcept { value.value().degradation_preference = present ? std::optional<webrtc::DegradationPreference>(static_cast<webrtc::DegradationPreference>(raw)) : std::nullopt; }
std::unique_ptr<NativeRtcpParameters> rtp_parameters_rtcp(const NativeRtpParameters& value) noexcept { return std::make_unique<NativeRtcpParameters>(value.value().rtcp); }
void rtp_parameters_set_rtcp(NativeRtpParameters& value, const NativeRtcpParameters& rtcp) noexcept { value.value().rtcp = rtcp.value(); }
std::unique_ptr<NativeRtpExtensionList> rtp_parameters_header_extensions(const NativeRtpParameters& value) noexcept { return std::make_unique<NativeRtpExtensionList>(value.value().header_extensions); }
void rtp_parameters_set_header_extensions(NativeRtpParameters& value, const NativeRtpExtensionList& extensions) noexcept { value.value().header_extensions = extensions.value(); }
bool rtp_parameters_is_mixed_codec(const NativeRtpParameters& value) noexcept { return value.value().IsMixedCodec(); }
rust::Vec<std::uint8_t> rtp_parameters_string(const NativeRtpParameters& value, bool& valid) noexcept { valid = RtpParametersFormattingSafe(value.value()); if (!valid) return {}; return RtpBytes(absl::StrFormat("%v", value.value())); }
rust::Vec<std::uint8_t> rtp_degradation_preference_string(std::int32_t raw, bool& valid) noexcept { valid = raw >= 0 && raw <= 3; if (!valid) return {}; return RtpBytes(webrtc::DegradationPreferenceToString(static_cast<webrtc::DegradationPreference>(raw))); }
std::size_t rtp_parameters_codecs_len(const NativeRtpParameters& value) noexcept { return value.value().codecs.size(); }
std::unique_ptr<NativeRtpCodecParameters> rtp_parameters_codecs_get(const NativeRtpParameters& value, std::size_t index) noexcept { if (index >= value.value().codecs.size()) return nullptr; return std::make_unique<NativeRtpCodecParameters>(value.value().codecs[index]); }
void rtp_parameters_codecs_clear(NativeRtpParameters& value) noexcept { value.value().codecs.clear(); }
void rtp_parameters_codecs_add(NativeRtpParameters& value, const NativeRtpCodecParameters& element) noexcept { value.value().codecs.push_back(element.value()); }
std::size_t rtp_parameters_encodings_len(const NativeRtpParameters& value) noexcept { return value.value().encodings.size(); }
std::unique_ptr<NativeRtpEncoding> rtp_parameters_encodings_get(const NativeRtpParameters& value, std::size_t index) noexcept { if (index >= value.value().encodings.size()) return nullptr; return std::make_unique<NativeRtpEncoding>(value.value().encodings[index]); }
void rtp_parameters_encodings_clear(NativeRtpParameters& value) noexcept { value.value().encodings.clear(); }
void rtp_parameters_encodings_add(NativeRtpParameters& value, const NativeRtpEncoding& element) noexcept { value.value().encodings.push_back(element.value()); }
bool string_builder_append_rtp_parameters(NativeStringBuilder& output, const NativeRtpParameters& input) noexcept { if (!RtpParametersFormattingSafe(input.value())) return false; output.value() << input.value(); return true; }
bool rtc_error_builder_append_rtp_parameters(NativeRtcError& output, const NativeRtpParameters& input) noexcept { if (!RtpParametersFormattingSafe(input.value())) return false; output.value().string_builder() << input.value(); return true; }
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_parameters(std::unique_ptr<NativeRtcError> error, const NativeRtpParameters& input, bool& valid) noexcept { valid = RtpParametersFormattingSafe(input.value()); if (!valid) return error; return std::make_unique<NativeRtcError>(std::move(error->value()) << input.value()); }

std::size_t priority_value_hash(const NativePriorityValue& value) noexcept { return absl::Hash<webrtc::PriorityValue>{}(value.value()); }

bool string_builder_append_codec(NativeStringBuilder& output, const NativeRtpCodec& input) noexcept { if (!(MimeKind(input.value().kind))) return false; output.value() << (input.value()); return true; }
bool rtc_error_builder_append_codec(NativeRtcError& output, const NativeRtpCodec& input) noexcept { if (!(MimeKind(input.value().kind))) return false; output.value().string_builder() << (input.value()); return true; }
std::unique_ptr<NativeRtcError> rtc_error_stream_codec(std::unique_ptr<NativeRtcError> error, const NativeRtpCodec& input, bool& valid) noexcept { valid = MimeKind(input.value().kind); if (!valid) return error; return std::make_unique<NativeRtcError>(std::move(error->value()) << (input.value())); }
bool string_builder_append_encoding(NativeStringBuilder& output, const NativeRtpEncoding& input) noexcept { if (!(!input.value().codec || MimeKind(input.value().codec->kind))) return false; output.value() << (input.value()); return true; }
bool rtc_error_builder_append_encoding(NativeRtcError& output, const NativeRtpEncoding& input) noexcept { if (!(!input.value().codec || MimeKind(input.value().codec->kind))) return false; output.value().string_builder() << (input.value()); return true; }
std::unique_ptr<NativeRtcError> rtc_error_stream_encoding(std::unique_ptr<NativeRtcError> error, const NativeRtpEncoding& input, bool& valid) noexcept { valid = !input.value().codec || MimeKind(input.value().codec->kind); if (!valid) return error; return std::make_unique<NativeRtcError>(std::move(error->value()) << (input.value())); }
bool string_builder_append_media_type(NativeStringBuilder& output, std::int32_t kind) noexcept { if (!(kind >= 0 && kind <= 2)) return false; output.value() << (static_cast<webrtc::MediaType>(kind)); return true; }
bool rtc_error_builder_append_media_type(NativeRtcError& output, std::int32_t kind) noexcept { if (!(kind >= 0 && kind <= 2)) return false; output.value().string_builder() << (static_cast<webrtc::MediaType>(kind)); return true; }
std::unique_ptr<NativeRtcError> rtc_error_stream_media_type(std::unique_ptr<NativeRtcError> error, std::int32_t kind, bool& valid) noexcept { valid = kind >= 0 && kind <= 2; if (!valid) return error; return std::make_unique<NativeRtcError>(std::move(error->value()) << (static_cast<webrtc::MediaType>(kind))); }
bool string_builder_append_feedback_type(NativeStringBuilder& output, std::int32_t kind) noexcept { if (!(true)) return false; output.value() << (static_cast<webrtc::RtcpFeedbackType>(kind)); return true; }
bool rtc_error_builder_append_feedback_type(NativeRtcError& output, std::int32_t kind) noexcept { if (!(true)) return false; output.value().string_builder() << (static_cast<webrtc::RtcpFeedbackType>(kind)); return true; }
std::unique_ptr<NativeRtcError> rtc_error_stream_feedback_type(std::unique_ptr<NativeRtcError> error, std::int32_t kind, bool& valid) noexcept { valid = true; if (!valid) return error; return std::make_unique<NativeRtcError>(std::move(error->value()) << (static_cast<webrtc::RtcpFeedbackType>(kind))); }
bool string_builder_append_optional_feedback_type(NativeStringBuilder& output, bool present, std::int32_t kind) noexcept { if (!(true)) return false; output.value() << (present ? std::optional<webrtc::RtcpFeedbackType>(static_cast<webrtc::RtcpFeedbackType>(kind)) : std::nullopt); return true; }
bool rtc_error_builder_append_optional_feedback_type(NativeRtcError& output, bool present, std::int32_t kind) noexcept { if (!(true)) return false; output.value().string_builder() << (present ? std::optional<webrtc::RtcpFeedbackType>(static_cast<webrtc::RtcpFeedbackType>(kind)) : std::nullopt); return true; }
std::unique_ptr<NativeRtcError> rtc_error_stream_optional_feedback_type(std::unique_ptr<NativeRtcError> error, bool present, std::int32_t kind, bool& valid) noexcept { valid = true; if (!valid) return error; return std::make_unique<NativeRtcError>(std::move(error->value()) << (present ? std::optional<webrtc::RtcpFeedbackType>(static_cast<webrtc::RtcpFeedbackType>(kind)) : std::nullopt)); }
}
