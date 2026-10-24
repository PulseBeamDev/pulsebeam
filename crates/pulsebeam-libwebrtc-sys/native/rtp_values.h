#pragma once
#include <cstdint>
#include <memory>
#include <optional>
#include <utility>
#include "api/priority.h"
#include "api/crypto/frame_decryptor_interface.h"
#include "api/sframe/sframe_encrypter_interface.h"
#include "api/rtp_parameters.h"
#include "api/transport/rtp/rtp_source.h"
#include <vector>
#include "api/rtp_transceiver_interface.h"
#include "api/video/resolution.h"
#include "rust/cxx.h"
namespace pulsebeam::webrtc_sys {
struct FfiSframeInit;
class NativeSframeEncrypterInit final {
 public:
  explicit NativeSframeEncrypterInit(webrtc::SframeEncrypterInit value) : value_(value) {}
  const webrtc::SframeEncrypterInit& value() const noexcept { return value_; }
  webrtc::SframeEncrypterInit& value() noexcept { return value_; }
 private:
  webrtc::SframeEncrypterInit value_;
};
std::unique_ptr<NativeSframeEncrypterInit> sframe_init_new(std::int32_t mode, std::int32_t suite) noexcept;
std::unique_ptr<NativeSframeEncrypterInit> sframe_init_clone(const NativeSframeEncrypterInit& source) noexcept;
void sframe_init_assign(NativeSframeEncrypterInit& target, const NativeSframeEncrypterInit& source) noexcept;
FfiSframeInit sframe_init_fields(const NativeSframeEncrypterInit& value) noexcept;
void sframe_init_set_mode(NativeSframeEncrypterInit& value, std::int32_t mode) noexcept;
void sframe_init_set_suite(NativeSframeEncrypterInit& value, std::int32_t suite) noexcept;
struct FfiFrameDecryptionResult;
class NativeFrameDecryptionResult final {
 public:
  explicit NativeFrameDecryptionResult(webrtc::FrameDecryptorInterface::Result value) : value_(value) {}
  const webrtc::FrameDecryptorInterface::Result& value() const noexcept { return *value_; }
  // SDK Result fields are const. Rebuild the complete native value rather than
  // attempting assignment or mutating those fields. Storage is always engaged.
  void replace(webrtc::FrameDecryptorInterface::Result value) noexcept { value_.emplace(value); }
 private:
  std::optional<webrtc::FrameDecryptorInterface::Result> value_;
};
std::unique_ptr<NativeFrameDecryptionResult> frame_decryption_result_new(std::int32_t status, std::size_t written) noexcept;
std::unique_ptr<NativeFrameDecryptionResult> frame_decryption_result_clone(const NativeFrameDecryptionResult& value) noexcept;
FfiFrameDecryptionResult frame_decryption_result_fields(const NativeFrameDecryptionResult& value) noexcept;
bool frame_decryption_result_is_ok(const NativeFrameDecryptionResult& value) noexcept;
void frame_decryption_result_assign(NativeFrameDecryptionResult& target, const NativeFrameDecryptionResult& source) noexcept;
void frame_decryption_result_set_status(NativeFrameDecryptionResult& value, std::int32_t status) noexcept;
void frame_decryption_result_set_written(NativeFrameDecryptionResult& value, std::size_t written) noexcept;
struct FfiRtpResolutionPair;
class NativeRtpResolution final {
 public:
  explicit NativeRtpResolution(webrtc::Resolution value) : value_(value) {}
  const webrtc::Resolution& value() const noexcept { return value_; }
  webrtc::Resolution& value() noexcept { return value_; }
 private:
  webrtc::Resolution value_;
};
std::unique_ptr<NativeRtpResolution> rtp_resolution_new(std::int32_t width, std::int32_t height) noexcept;
std::unique_ptr<NativeRtpResolution> rtp_resolution_clone(const NativeRtpResolution& source) noexcept;
void rtp_resolution_assign(NativeRtpResolution& target, const NativeRtpResolution& source) noexcept;
bool rtp_resolution_equal(const NativeRtpResolution& lhs, const NativeRtpResolution& rhs) noexcept;
std::int32_t rtp_resolution_width(const NativeRtpResolution& value) noexcept;
std::int32_t rtp_resolution_height(const NativeRtpResolution& value) noexcept;
void rtp_resolution_set_width(NativeRtpResolution& value, std::int32_t width) noexcept;
void rtp_resolution_set_height(NativeRtpResolution& value, std::int32_t height) noexcept;
std::int32_t rtp_resolution_pixel_count(const NativeRtpResolution& value, bool& valid) noexcept;
FfiRtpResolutionPair rtp_resolution_to_pair(const NativeRtpResolution& value) noexcept;

class NativeRtcError;
class NativeStringBuilder;
struct FfiRtcpFeedbackFields;
struct FfiRtpCodecFields;
struct FfiRtpCodecParameter;
class NativeRtcpFeedback final {
 public:
  explicit NativeRtcpFeedback(webrtc::RtcpFeedback value) : value_(std::move(value)) {}
  const webrtc::RtcpFeedback& value() const noexcept { return value_; }
  webrtc::RtcpFeedback& value() noexcept { return value_; }
 private:
  webrtc::RtcpFeedback value_;
};
class NativeRtpCodec final {
 public:
  explicit NativeRtpCodec(webrtc::RtpCodec value) : value_(std::move(value)) {}
  const webrtc::RtpCodec& value() const noexcept { return value_; }
  webrtc::RtpCodec& value() noexcept { return value_; }
 private:
  webrtc::RtpCodec value_;
};
std::unique_ptr<NativeRtcpFeedback> rtcp_feedback_default() noexcept;
std::unique_ptr<NativeRtcpFeedback> rtcp_feedback_for_type(std::int32_t type) noexcept;
std::unique_ptr<NativeRtcpFeedback> rtcp_feedback_new(std::int32_t type, std::int32_t message) noexcept;
std::unique_ptr<NativeRtcpFeedback> rtcp_feedback_clone(const NativeRtcpFeedback& source) noexcept;
void rtcp_feedback_assign(NativeRtcpFeedback& target, const NativeRtcpFeedback& source) noexcept;
bool rtcp_feedback_equal(const NativeRtcpFeedback& lhs, const NativeRtcpFeedback& rhs) noexcept;
FfiRtcpFeedbackFields rtcp_feedback_fields(const NativeRtcpFeedback& value) noexcept;
void rtcp_feedback_set_type(NativeRtcpFeedback& value, std::int32_t type) noexcept;
void rtcp_feedback_set_message(NativeRtcpFeedback& value, bool present, std::int32_t message) noexcept;
rust::String rtcp_feedback_type_string(std::int32_t type) noexcept;
rust::String rtcp_feedback_optional_type_string(bool present, std::int32_t type) noexcept;
rust::String rtp_media_type_string(std::int32_t type, bool& valid) noexcept;
std::unique_ptr<NativeRtpCodec> rtp_codec_default() noexcept;
std::unique_ptr<NativeRtpCodec> rtp_codec_clone(const NativeRtpCodec& source) noexcept;
void rtp_codec_assign(NativeRtpCodec& target, const NativeRtpCodec& source) noexcept;
bool rtp_codec_equal(const NativeRtpCodec& lhs, const NativeRtpCodec& rhs) noexcept;
FfiRtpCodecFields rtp_codec_fields(const NativeRtpCodec& value) noexcept;
void rtp_codec_set_name(NativeRtpCodec& value, rust::Slice<const std::uint8_t> name) noexcept;
void rtp_codec_set_kind(NativeRtpCodec& value, std::int32_t kind) noexcept;
void rtp_codec_set_clock_rate(NativeRtpCodec& value, bool present, std::int32_t rate) noexcept;
void rtp_codec_set_num_channels(NativeRtpCodec& value, bool present, std::int32_t count) noexcept;
std::size_t rtp_codec_feedback_len(const NativeRtpCodec& value) noexcept;
std::unique_ptr<NativeRtcpFeedback> rtp_codec_feedback_get(const NativeRtpCodec& value, std::size_t index) noexcept;
void rtp_codec_feedback_clear(NativeRtpCodec& value) noexcept;
void rtp_codec_feedback_add(NativeRtpCodec& value, const NativeRtcpFeedback& feedback) noexcept;
rust::Vec<FfiRtpCodecParameter> rtp_codec_parameters(const NativeRtpCodec& value) noexcept;
void rtp_codec_set_parameters(NativeRtpCodec& value, rust::Slice<const FfiRtpCodecParameter> parameters) noexcept;
rust::Vec<std::uint8_t> rtp_codec_mime_type(const NativeRtpCodec& value, bool& valid) noexcept;
rust::Vec<std::uint8_t> rtp_codec_string(const NativeRtpCodec& value, bool& valid) noexcept;
bool rtp_codec_is_resiliency(const NativeRtpCodec& value) noexcept;
bool rtp_codec_is_media(const NativeRtpCodec& value) noexcept;
bool rtp_codec_write_fmtp(const NativeRtpCodec& value, NativeStringBuilder& output) noexcept;
std::unique_ptr<NativeRtcError> rtp_codec_parse_fmtp(NativeRtpCodec& value, rust::Slice<const std::uint8_t> text) noexcept;

struct FfiRtpEncodingFields;
class NativeRtpSender;
class NativeRtpReceiver;

class NativeRtpSource final {
 public:
  explicit NativeRtpSource(webrtc::RtpSource value) : value_(std::move(value)) {}
  const webrtc::RtpSource& value() const noexcept { return value_; }
  webrtc::RtpSource& value() noexcept { return value_; }
 private:
  webrtc::RtpSource value_;
};
class NativeRtpSourceList final {
 public:
  explicit NativeRtpSourceList(std::vector<webrtc::RtpSource> value) : value_(std::move(value)) {}
  const std::vector<webrtc::RtpSource>& value() const noexcept { return value_; }
 private:
  std::vector<webrtc::RtpSource> value_;
};
struct FfiRtpSourceFields;
std::unique_ptr<NativeRtpSource> rtp_source_new(const FfiRtpSourceFields& fields) noexcept;
std::unique_ptr<NativeRtpSource> rtp_source_clone(const NativeRtpSource& value) noexcept;
void rtp_source_assign(NativeRtpSource& target, const NativeRtpSource& source) noexcept;
bool rtp_source_equal(const NativeRtpSource& left, const NativeRtpSource& right) noexcept;
FfiRtpSourceFields rtp_source_fields(const NativeRtpSource& value) noexcept;
void rtp_source_set_audio_level(NativeRtpSource& value, bool present, std::uint8_t level) noexcept;
rust::Vec<std::uint8_t> rtp_source_string(const NativeRtpSource& value) noexcept;
std::unique_ptr<NativeRtpSourceList> rtp_receiver_sources(const NativeRtpReceiver& receiver) noexcept;
std::size_t rtp_source_list_len(const NativeRtpSourceList& value) noexcept;
std::unique_ptr<NativeRtpSource> rtp_source_list_get(const NativeRtpSourceList& value, std::size_t index) noexcept;
void string_builder_append_rtp_source(NativeStringBuilder& output, const NativeRtpSource& value) noexcept;
void rtc_error_builder_append_rtp_source(NativeRtcError& output, const NativeRtpSource& value) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_source(std::unique_ptr<NativeRtcError> error, const NativeRtpSource& value) noexcept;
class NativeRtpEncoding final {
 public:
  explicit NativeRtpEncoding(webrtc::RtpEncodingParameters value) : value_(std::move(value)) {}
  const webrtc::RtpEncodingParameters& value() const noexcept { return value_; }
  webrtc::RtpEncodingParameters& value() noexcept { return value_; }
 private:
  webrtc::RtpEncodingParameters value_;
};
class NativeRtpEncodingList final {
 public:
  explicit NativeRtpEncodingList(std::vector<webrtc::RtpEncodingParameters> value) : value_(std::move(value)) {}
  const std::vector<webrtc::RtpEncodingParameters>& value() const noexcept { return value_; }
 private:
  std::vector<webrtc::RtpEncodingParameters> value_;
};
std::unique_ptr<NativeRtpEncoding> rtp_encoding_default() noexcept;
std::unique_ptr<NativeRtpEncoding> rtp_encoding_clone(const NativeRtpEncoding& source) noexcept;
void rtp_encoding_assign(NativeRtpEncoding& target, const NativeRtpEncoding& source) noexcept;
bool rtp_encoding_equal(const NativeRtpEncoding& lhs, const NativeRtpEncoding& rhs) noexcept;
FfiRtpEncodingFields rtp_encoding_fields(const NativeRtpEncoding& value) noexcept;
void rtp_encoding_set_ssrc(NativeRtpEncoding& value, bool present, std::uint32_t input) noexcept;
void rtp_encoding_set_bitrate_priority(NativeRtpEncoding& value, double input) noexcept;
void rtp_encoding_set_network_priority(NativeRtpEncoding& value, std::int32_t input) noexcept;
void rtp_encoding_set_max_bitrate_bps(NativeRtpEncoding& value, bool present, int input) noexcept;
void rtp_encoding_set_min_bitrate_bps(NativeRtpEncoding& value, bool present, int input) noexcept;
void rtp_encoding_set_max_framerate(NativeRtpEncoding& value, bool present, double input) noexcept;
void rtp_encoding_set_num_temporal_layers(NativeRtpEncoding& value, bool present, int input) noexcept;
void rtp_encoding_set_scale_resolution_down_by(NativeRtpEncoding& value, bool present, double input) noexcept;
void rtp_encoding_set_active(NativeRtpEncoding& value, bool input) noexcept;
void rtp_encoding_set_request_key_frame(NativeRtpEncoding& value, bool input) noexcept;
void rtp_encoding_set_adaptive_ptime(NativeRtpEncoding& value, bool input) noexcept;
void rtp_encoding_set_csrcs(NativeRtpEncoding& value, bool present, rust::Slice<const std::uint32_t> csrcs) noexcept;
void rtp_encoding_set_rid(NativeRtpEncoding& value, rust::Slice<const std::uint8_t> rid) noexcept;
void rtp_encoding_set_scalability_mode(NativeRtpEncoding& value, bool present, rust::Slice<const std::uint8_t> mode) noexcept;
std::unique_ptr<NativeRtpResolution> rtp_encoding_scale_to(const NativeRtpEncoding& value) noexcept;
void rtp_encoding_set_scale_to(NativeRtpEncoding& value, const NativeRtpResolution& resolution) noexcept;
void rtp_encoding_clear_scale_to(NativeRtpEncoding& value) noexcept;
std::unique_ptr<NativeRtpCodec> rtp_encoding_codec(const NativeRtpEncoding& value) noexcept;
void rtp_encoding_set_codec(NativeRtpEncoding& value, const NativeRtpCodec& codec) noexcept;
void rtp_encoding_clear_codec(NativeRtpEncoding& value) noexcept;
rust::Vec<std::uint8_t> rtp_encoding_string(const NativeRtpEncoding& value, bool& valid) noexcept;
std::size_t rtp_encoding_list_len(const NativeRtpEncodingList& values) noexcept;
std::unique_ptr<NativeRtpEncoding> rtp_encoding_list_get(const NativeRtpEncodingList& values, std::size_t index) noexcept;
std::unique_ptr<NativeRtpEncodingList> rtp_sender_initial_encodings(const NativeRtpSender& sender) noexcept;

class NativeRtpTransceiverInit final {
 public:
  explicit NativeRtpTransceiverInit(const webrtc::RtpTransceiverInit& value) : value_(value) {}
  const webrtc::RtpTransceiverInit& value() const noexcept { return value_; }
  webrtc::RtpTransceiverInit& value() noexcept { return value_; }
 private:
  webrtc::RtpTransceiverInit value_;
};
std::unique_ptr<NativeRtpTransceiverInit> rtp_transceiver_init_default() noexcept;
std::unique_ptr<NativeRtpTransceiverInit> rtp_transceiver_init_clone(const NativeRtpTransceiverInit& source) noexcept;
void rtp_transceiver_init_assign(NativeRtpTransceiverInit& target, const NativeRtpTransceiverInit& source) noexcept;
std::int32_t rtp_transceiver_init_direction(const NativeRtpTransceiverInit& value) noexcept;
void rtp_transceiver_init_set_direction(NativeRtpTransceiverInit& value, std::int32_t direction) noexcept;
std::size_t rtp_transceiver_init_stream_ids_len(const NativeRtpTransceiverInit& value) noexcept;
rust::Vec<std::uint8_t> rtp_transceiver_init_stream_id_get(const NativeRtpTransceiverInit& value, std::size_t index) noexcept;
void rtp_transceiver_init_stream_ids_clear(NativeRtpTransceiverInit& value) noexcept;
void rtp_transceiver_init_stream_id_add(NativeRtpTransceiverInit& value, rust::Slice<const std::uint8_t> id) noexcept;
std::size_t rtp_transceiver_init_encodings_len(const NativeRtpTransceiverInit& value) noexcept;
std::unique_ptr<NativeRtpEncoding> rtp_transceiver_init_encoding_get(const NativeRtpTransceiverInit& value, std::size_t index) noexcept;
void rtp_transceiver_init_encodings_clear(NativeRtpTransceiverInit& value) noexcept;
void rtp_transceiver_init_encoding_add(NativeRtpTransceiverInit& value, const NativeRtpEncoding& encoding) noexcept;

class NativePriorityValue final {
 public:
  explicit NativePriorityValue(webrtc::PriorityValue value) : value_(std::move(value)) {}
  const webrtc::PriorityValue& value() const noexcept { return value_; }
  webrtc::PriorityValue& value() noexcept { return value_; }
 private:
  webrtc::PriorityValue value_;
};
std::unique_ptr<NativePriorityValue> priority_value_new(std::uint16_t value) noexcept;
std::unique_ptr<NativePriorityValue> priority_value_from_priority(std::int32_t priority) noexcept;
std::unique_ptr<NativePriorityValue> priority_value_clone(const NativePriorityValue& source) noexcept;
void priority_value_assign(NativePriorityValue& target, const NativePriorityValue& source) noexcept;
const std::uint16_t& priority_value_ref(const NativePriorityValue& value) noexcept;
std::uint16_t& priority_value_mut(NativePriorityValue& value) noexcept;
const std::uint16_t& priority_value_deref(const NativePriorityValue& value) noexcept;
std::uint16_t& priority_value_deref_mut(NativePriorityValue& value) noexcept;
const std::uint16_t& priority_value_arrow(const NativePriorityValue& value) noexcept;
std::uint16_t& priority_value_arrow_mut(NativePriorityValue& value) noexcept;
const std::uint16_t& priority_value_conversion(const NativePriorityValue& value) noexcept;
std::uint16_t priority_value_into_value(std::unique_ptr<NativePriorityValue> value) noexcept;
std::uint16_t priority_value_into_const_value(std::unique_ptr<NativePriorityValue> value) noexcept;
std::uint16_t priority_value_into_deref(std::unique_ptr<NativePriorityValue> value) noexcept;
std::uint16_t priority_value_into_const_deref(std::unique_ptr<NativePriorityValue> value) noexcept;
bool priority_value_equal(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept;
bool priority_value_not_equal(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept;
bool priority_value_less(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept;
bool priority_value_less_equal(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept;
bool priority_value_greater(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept;
bool priority_value_greater_equal(const NativePriorityValue& lhs, const NativePriorityValue& rhs) noexcept;

class NativeRtpCodecParameters final {
 public:
  explicit NativeRtpCodecParameters(webrtc::RtpCodecParameters value) : value_(std::move(value)) {}
  const webrtc::RtpCodecParameters& value() const noexcept { return value_; }
  webrtc::RtpCodecParameters& value() noexcept { return value_; }
 private:
  webrtc::RtpCodecParameters value_;
};
std::unique_ptr<NativeRtpCodecParameters> rtp_codec_parameters_default() noexcept;
std::unique_ptr<NativeRtpCodecParameters> rtp_codec_parameters_clone(const NativeRtpCodecParameters& source) noexcept;
void rtp_codec_parameters_assign(NativeRtpCodecParameters& target, const NativeRtpCodecParameters& source) noexcept;
bool rtp_codec_parameters_equal(const NativeRtpCodecParameters& lhs, const NativeRtpCodecParameters& rhs) noexcept;
class NativeRtcpParameters final {
 public:
  explicit NativeRtcpParameters(webrtc::RtcpParameters value) : value_(std::move(value)) {}
  const webrtc::RtcpParameters& value() const noexcept { return value_; }
  webrtc::RtcpParameters& value() noexcept { return value_; }
 private:
  webrtc::RtcpParameters value_;
};
std::unique_ptr<NativeRtcpParameters> rtcp_parameters_default() noexcept;
std::unique_ptr<NativeRtcpParameters> rtcp_parameters_clone(const NativeRtcpParameters& source) noexcept;
void rtcp_parameters_assign(NativeRtcpParameters& target, const NativeRtcpParameters& source) noexcept;
bool rtcp_parameters_equal(const NativeRtcpParameters& lhs, const NativeRtcpParameters& rhs) noexcept;

std::int32_t rtp_codec_parameters_payload_type(const NativeRtpCodecParameters& value) noexcept;
void rtp_codec_parameters_set_payload_type(NativeRtpCodecParameters& value, std::int32_t payload) noexcept;
std::unique_ptr<NativeRtpCodec> rtp_codec_parameters_base(const NativeRtpCodecParameters& value) noexcept;
void rtp_codec_parameters_set_base(NativeRtpCodecParameters& value, const NativeRtpCodec& codec) noexcept;
rust::Vec<std::uint8_t> rtp_codec_parameters_mime_type(const NativeRtpCodecParameters& value, bool& valid) noexcept;
rust::Vec<std::uint8_t> rtp_codec_parameters_string(const NativeRtpCodecParameters& value, bool& valid) noexcept;
bool rtp_codec_parameters_is_resiliency(const NativeRtpCodecParameters& value) noexcept;
bool rtp_codec_parameters_is_media(const NativeRtpCodecParameters& value) noexcept;
struct FfiRtcpParametersFields;
FfiRtcpParametersFields rtcp_parameters_fields(const NativeRtcpParameters& value) noexcept;
void rtcp_parameters_set_ssrc(NativeRtcpParameters& value, bool present, std::uint32_t ssrc) noexcept;
void rtcp_parameters_set_cname(NativeRtcpParameters& value, rust::Slice<const std::uint8_t> cname) noexcept;
void rtcp_parameters_set_reduced_size(NativeRtcpParameters& value, bool reduced) noexcept;
void rtcp_parameters_set_mux(NativeRtcpParameters& value, bool mux) noexcept;
rust::Vec<std::uint8_t> rtcp_parameters_string(const NativeRtcpParameters& value) noexcept;


bool string_builder_append_codec_parameters(NativeStringBuilder& output, const NativeRtpCodecParameters& input) noexcept;
bool rtc_error_builder_append_codec_parameters(NativeRtcError& output, const NativeRtpCodecParameters& input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_codec_parameters(std::unique_ptr<NativeRtcError> error, const NativeRtpCodecParameters& input, bool& valid) noexcept;
bool string_builder_append_rtcp_parameters(NativeStringBuilder& output, const NativeRtcpParameters& input) noexcept;
bool rtc_error_builder_append_rtcp_parameters(NativeRtcError& output, const NativeRtcpParameters& input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_rtcp_parameters(std::unique_ptr<NativeRtcError> error, const NativeRtcpParameters& input, bool& valid) noexcept;


class NativeRtpHeaderExtensionId final {
 public:
  explicit NativeRtpHeaderExtensionId(webrtc::RtpHeaderExtensionId value) : value_(std::move(value)) {}
  const webrtc::RtpHeaderExtensionId& value() const noexcept { return value_; }
  webrtc::RtpHeaderExtensionId& value() noexcept { return value_; }
 private:
  webrtc::RtpHeaderExtensionId value_;
};
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_default() noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_not_set() noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_new(std::int32_t value) noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_constant(std::uint32_t index) noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_id_clone(const NativeRtpHeaderExtensionId& source) noexcept;
void rtp_header_id_assign(NativeRtpHeaderExtensionId& target, const NativeRtpHeaderExtensionId& source) noexcept;
std::int32_t rtp_header_id_to_int(const NativeRtpHeaderExtensionId& value) noexcept;
bool rtp_header_id_valid(const NativeRtpHeaderExtensionId& value) noexcept;
bool rtp_header_id_is_set(const NativeRtpHeaderExtensionId& value) noexcept;
rust::Vec<std::uint8_t> rtp_header_id_string(const NativeRtpHeaderExtensionId& value) noexcept;
std::size_t rtp_header_id_hash(const NativeRtpHeaderExtensionId& value) noexcept;
const std::int32_t& rtp_header_id_ref(const NativeRtpHeaderExtensionId& value) noexcept;
std::int32_t& rtp_header_id_ref_mut(NativeRtpHeaderExtensionId& value) noexcept;
const std::int32_t& rtp_header_id_deref(const NativeRtpHeaderExtensionId& value) noexcept;
std::int32_t& rtp_header_id_deref_mut(NativeRtpHeaderExtensionId& value) noexcept;
const std::int32_t& rtp_header_id_arrow(const NativeRtpHeaderExtensionId& value) noexcept;
std::int32_t& rtp_header_id_arrow_mut(NativeRtpHeaderExtensionId& value) noexcept;
const std::int32_t& rtp_header_id_conversion(const NativeRtpHeaderExtensionId& value) noexcept;
std::int32_t rtp_header_id_into_value(std::unique_ptr<NativeRtpHeaderExtensionId> value) noexcept;
std::int32_t rtp_header_id_into_const_value(std::unique_ptr<NativeRtpHeaderExtensionId> value) noexcept;
std::int32_t rtp_header_id_into_deref(std::unique_ptr<NativeRtpHeaderExtensionId> value) noexcept;
std::int32_t rtp_header_id_into_const_deref(std::unique_ptr<NativeRtpHeaderExtensionId> value) noexcept;
bool rtp_header_id_equal(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept;
bool rtp_header_id_not_equal(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept;
bool rtp_header_id_less(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept;
bool rtp_header_id_less_equal(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept;
bool rtp_header_id_greater(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept;
bool rtp_header_id_greater_equal(const NativeRtpHeaderExtensionId& lhs, const NativeRtpHeaderExtensionId& rhs) noexcept;


class NativeRtpExtension final {
 public:
  explicit NativeRtpExtension(webrtc::RtpExtension value) : value_(std::move(value)) {}
  const webrtc::RtpExtension& value() const noexcept { return value_; }
  webrtc::RtpExtension& value() noexcept { return value_; }
 private:
  webrtc::RtpExtension value_;
};
class NativeRtpExtensionList final {
 public:
  NativeRtpExtensionList() = default;
  explicit NativeRtpExtensionList(std::vector<webrtc::RtpExtension> values) : values_(std::move(values)) {}
  const std::vector<webrtc::RtpExtension>& value() const noexcept { return values_; }
  std::vector<webrtc::RtpExtension>& value() noexcept { return values_; }
 private:
  std::vector<webrtc::RtpExtension> values_;
};
// Pointer validity is enforced by the Rust list-borrow lifetime. No SDK ownership.
class NativeRtpExtensionBorrowed final {
 public:
  explicit NativeRtpExtensionBorrowed(const webrtc::RtpExtension* value) : value_(value) {}
  const webrtc::RtpExtension& value() const noexcept { return *value_; }
 private:
  const webrtc::RtpExtension* value_;
};
struct FfiRtpExtensionFields;
std::unique_ptr<NativeRtpExtension> rtp_extension_default() noexcept;
std::unique_ptr<NativeRtpExtension> rtp_extension_new(rust::Slice<const std::uint8_t> uri, const NativeRtpHeaderExtensionId& id, bool explicit_encrypt, bool encrypt) noexcept;
std::unique_ptr<NativeRtpExtension> rtp_extension_new_int(rust::Slice<const std::uint8_t> uri, std::int32_t id, bool explicit_encrypt, bool encrypt) noexcept;
std::unique_ptr<NativeRtpExtension> rtp_extension_clone(const NativeRtpExtension& value) noexcept;
void rtp_extension_assign(NativeRtpExtension& target, const NativeRtpExtension& source) noexcept;
bool rtp_extension_equal(const NativeRtpExtension& lhs, const NativeRtpExtension& rhs) noexcept;
FfiRtpExtensionFields rtp_extension_fields(const NativeRtpExtension& value) noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_id(const NativeRtpExtension& value) noexcept;
void rtp_extension_set_uri(NativeRtpExtension& value, rust::Slice<const std::uint8_t> uri) noexcept;
void rtp_extension_set_id(NativeRtpExtension& value, const NativeRtpHeaderExtensionId& id) noexcept;
void rtp_extension_set_encrypt(NativeRtpExtension& value, bool encrypt) noexcept;
rust::Vec<std::uint8_t> rtp_extension_to_string(const NativeRtpExtension& value) noexcept;
rust::Vec<std::uint8_t> rtp_extension_string(const NativeRtpExtension& value) noexcept;
bool rtp_extension_supported_audio(rust::Slice<const std::uint8_t> uri) noexcept;
bool rtp_extension_supported_video(rust::Slice<const std::uint8_t> uri) noexcept;
bool rtp_extension_encryption_supported(rust::Slice<const std::uint8_t> uri) noexcept;
std::unique_ptr<NativeRtpExtensionList> rtp_extension_list_new() noexcept;
std::unique_ptr<NativeRtpExtensionList> rtp_extension_list_clone(const NativeRtpExtensionList& values) noexcept;
void rtp_extension_list_assign(NativeRtpExtensionList& target, const NativeRtpExtensionList& source) noexcept;
std::size_t rtp_extension_list_len(const NativeRtpExtensionList& values) noexcept;
void rtp_extension_list_add(NativeRtpExtensionList& values, const NativeRtpExtension& value) noexcept;
void rtp_extension_list_clear(NativeRtpExtensionList& values) noexcept;
std::unique_ptr<NativeRtpExtension> rtp_extension_list_get(const NativeRtpExtensionList& values, std::size_t index) noexcept;
std::unique_ptr<NativeRtpExtensionBorrowed> rtp_extension_list_find(const NativeRtpExtensionList& values, rust::Slice<const std::uint8_t> uri, std::int32_t filter, bool& valid) noexcept;
std::unique_ptr<NativeRtpExtensionBorrowed> rtp_extension_list_find_encryption(const NativeRtpExtensionList& values, rust::Slice<const std::uint8_t> uri, bool encrypt) noexcept;
std::unique_ptr<NativeRtpExtensionList> rtp_extension_list_deduplicate(const NativeRtpExtensionList& values, std::int32_t filter) noexcept;
std::unique_ptr<NativeRtpExtension> rtp_extension_borrowed_clone(const NativeRtpExtensionBorrowed& value) noexcept;
FfiRtpExtensionFields rtp_extension_borrowed_fields(const NativeRtpExtensionBorrowed& value) noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_borrowed_id(const NativeRtpExtensionBorrowed& value) noexcept;
rust::Vec<std::uint8_t> rtp_extension_borrowed_to_string(const NativeRtpExtensionBorrowed& value) noexcept;
rust::Vec<std::uint8_t> rtp_extension_borrowed_string(const NativeRtpExtensionBorrowed& value) noexcept;

rust::Vec<std::uint8_t> rtp_extension_encrypt_header_extensions_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_audio_level_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_timestamp_offset_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_abs_send_time_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_absolute_capture_time_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_video_rotation_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_video_content_type_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_video_timing_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_generic_frame_descriptor_uri_00() noexcept;
rust::Vec<std::uint8_t> rtp_extension_dependency_descriptor_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_video_layers_allocation_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_transport_sequence_number_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_transport_sequence_number_v2_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_playout_delay_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_color_space_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_mid_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_rid_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_repaired_rid_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_video_frame_tracking_id_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_csrc_audio_levels_uri() noexcept;
rust::Vec<std::uint8_t> rtp_extension_corruption_detection_uri() noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_min_id() noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_max_id() noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_extension_one_byte_max_id() noexcept;
std::int32_t rtp_extension_max_value_size() noexcept;
std::int32_t rtp_extension_one_byte_max_value_size() noexcept;

void string_builder_append_header_extension_id(NativeStringBuilder& output, const NativeRtpHeaderExtensionId& input) noexcept;
void rtc_error_builder_append_header_extension_id(NativeRtcError& output, const NativeRtpHeaderExtensionId& input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_header_extension_id(std::unique_ptr<NativeRtcError> error, const NativeRtpHeaderExtensionId& input) noexcept;
void string_builder_append_rtp_extension(NativeStringBuilder& output, const NativeRtpExtension& input) noexcept;
void rtc_error_builder_append_rtp_extension(NativeRtcError& output, const NativeRtpExtension& input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_extension(std::unique_ptr<NativeRtcError> error, const NativeRtpExtension& input) noexcept;
void string_builder_append_rtp_extension_ref(NativeStringBuilder& output, const NativeRtpExtensionBorrowed& input) noexcept;
void rtc_error_builder_append_rtp_extension_ref(NativeRtcError& output, const NativeRtpExtensionBorrowed& input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_extension_ref(std::unique_ptr<NativeRtcError> error, const NativeRtpExtensionBorrowed& input) noexcept;



// Entire SDK capability values, without peer/factory origin affinity.
class NativePeerConnectionFactory;
class NativeRtpCodecCapability final {
 public:
  explicit NativeRtpCodecCapability(webrtc::RtpCodecCapability value) : value_(std::move(value)) {}
  const webrtc::RtpCodecCapability& value() const noexcept { return value_; }
  webrtc::RtpCodecCapability& value() noexcept { return value_; }
 private:
  webrtc::RtpCodecCapability value_;
};
std::unique_ptr<NativeRtpCodecCapability> rtp_codec_capability_default() noexcept;
std::unique_ptr<NativeRtpCodecCapability> rtp_codec_capability_clone(const NativeRtpCodecCapability& value) noexcept;
void rtp_codec_capability_assign(NativeRtpCodecCapability& target, const NativeRtpCodecCapability& source) noexcept;
bool rtp_codec_capability_equal(const NativeRtpCodecCapability& lhs, const NativeRtpCodecCapability& rhs) noexcept;
class NativeRtpHeaderExtensionCapability final {
 public:
  explicit NativeRtpHeaderExtensionCapability(webrtc::RtpHeaderExtensionCapability value) : value_(std::move(value)) {}
  const webrtc::RtpHeaderExtensionCapability& value() const noexcept { return value_; }
  webrtc::RtpHeaderExtensionCapability& value() noexcept { return value_; }
 private:
  webrtc::RtpHeaderExtensionCapability value_;
};
std::unique_ptr<NativeRtpHeaderExtensionCapability> rtp_header_capability_default() noexcept;
std::unique_ptr<NativeRtpHeaderExtensionCapability> rtp_header_capability_clone(const NativeRtpHeaderExtensionCapability& value) noexcept;
void rtp_header_capability_assign(NativeRtpHeaderExtensionCapability& target, const NativeRtpHeaderExtensionCapability& source) noexcept;
bool rtp_header_capability_equal(const NativeRtpHeaderExtensionCapability& lhs, const NativeRtpHeaderExtensionCapability& rhs) noexcept;
class NativeRtpCapabilities final {
 public:
  explicit NativeRtpCapabilities(webrtc::RtpCapabilities value) : value_(std::move(value)) {}
  const webrtc::RtpCapabilities& value() const noexcept { return value_; }
  webrtc::RtpCapabilities& value() noexcept { return value_; }
 private:
  webrtc::RtpCapabilities value_;
};
std::unique_ptr<NativeRtpCapabilities> rtp_capabilities_default() noexcept;
std::unique_ptr<NativeRtpCapabilities> rtp_capabilities_clone(const NativeRtpCapabilities& value) noexcept;
void rtp_capabilities_assign(NativeRtpCapabilities& target, const NativeRtpCapabilities& source) noexcept;
bool rtp_capabilities_equal(const NativeRtpCapabilities& lhs, const NativeRtpCapabilities& rhs) noexcept;
struct FfiRtpCodecCapabilityFields;
struct FfiRtpHeaderCapabilityFields;
FfiRtpCodecCapabilityFields rtp_codec_capability_fields(const NativeRtpCodecCapability& value) noexcept;
void rtp_codec_capability_set_payload_type(NativeRtpCodecCapability& value, bool present, std::int32_t input) noexcept;
std::unique_ptr<NativeRtpCodec> rtp_codec_capability_base(const NativeRtpCodecCapability& value) noexcept;
void rtp_codec_capability_set_base(NativeRtpCodecCapability& value, const NativeRtpCodec& codec) noexcept;
rust::Vec<std::uint8_t> rtp_codec_capability_modes(const NativeRtpCodecCapability& value) noexcept;
void rtp_codec_capability_set_modes(NativeRtpCodecCapability& value, rust::Slice<const std::uint8_t> modes) noexcept;
rust::Vec<std::uint8_t> rtp_codec_capability_string(const NativeRtpCodecCapability& value) noexcept;
rust::Vec<std::uint8_t> rtp_codec_capability_mime_type(const NativeRtpCodecCapability& value, bool& valid) noexcept;
bool rtp_codec_capability_is_resiliency(const NativeRtpCodecCapability& value) noexcept;
bool rtp_codec_capability_is_media(const NativeRtpCodecCapability& value) noexcept;
std::unique_ptr<NativeRtpHeaderExtensionCapability> rtp_header_capability_new(rust::Slice<const std::uint8_t> uri, const NativeRtpHeaderExtensionId& id, bool encrypt, std::int32_t direction, std::uint8_t constructor) noexcept;
FfiRtpHeaderCapabilityFields rtp_header_capability_fields(const NativeRtpHeaderExtensionCapability& value) noexcept;
std::unique_ptr<NativeRtpHeaderExtensionId> rtp_header_capability_id(const NativeRtpHeaderExtensionCapability& value) noexcept;
void rtp_header_capability_set_id(NativeRtpHeaderExtensionCapability& value, const NativeRtpHeaderExtensionId& id) noexcept;
void rtp_header_capability_clear_id(NativeRtpHeaderExtensionCapability& value) noexcept;
void rtp_header_capability_set_uri(NativeRtpHeaderExtensionCapability& value, rust::Slice<const std::uint8_t> uri) noexcept;
void rtp_header_capability_set_encrypt(NativeRtpHeaderExtensionCapability& value, bool encrypt) noexcept;
void rtp_header_capability_set_direction(NativeRtpHeaderExtensionCapability& value, std::int32_t direction) noexcept;
rust::Vec<std::int32_t> rtp_capabilities_fec(const NativeRtpCapabilities& value) noexcept;
void rtp_capabilities_set_fec(NativeRtpCapabilities& value, rust::Slice<const std::int32_t> fec) noexcept;
std::unique_ptr<NativeRtpCapabilities> factory_rtp_capabilities_value(const NativePeerConnectionFactory& factory, std::int32_t kind, bool sender) noexcept;
rust::Vec<std::uint8_t> rtp_direction_string(std::int32_t direction) noexcept;
void string_builder_append_rtp_direction(NativeStringBuilder& output, std::int32_t direction) noexcept;
void rtc_error_builder_append_rtp_direction(NativeRtcError& output, std::int32_t direction) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_direction(std::unique_ptr<NativeRtcError> error, std::int32_t direction) noexcept;
rust::Vec<std::uint8_t> rtp_scalability_modes_all() noexcept;
rust::Vec<std::uint8_t> rtp_scalability_mode_string(bool present, std::uint8_t mode, bool& valid) noexcept;
void string_builder_append_rtp_codec_capability(NativeStringBuilder& output, const NativeRtpCodecCapability& input) noexcept;
void rtc_error_builder_append_rtp_codec_capability(NativeRtcError& output, const NativeRtpCodecCapability& input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_codec_capability(std::unique_ptr<NativeRtcError> error, const NativeRtpCodecCapability& input) noexcept;
std::size_t rtp_capabilities_codecs_len(const NativeRtpCapabilities& value) noexcept;
std::unique_ptr<NativeRtpCodecCapability> rtp_capabilities_codecs_get(const NativeRtpCapabilities& value, std::size_t index) noexcept;
void rtp_capabilities_codecs_clear(NativeRtpCapabilities& value) noexcept;
void rtp_capabilities_codecs_add(NativeRtpCapabilities& value, const NativeRtpCodecCapability& element) noexcept;
std::size_t rtp_capabilities_header_extensions_len(const NativeRtpCapabilities& value) noexcept;
std::unique_ptr<NativeRtpHeaderExtensionCapability> rtp_capabilities_header_extensions_get(const NativeRtpCapabilities& value, std::size_t index) noexcept;
void rtp_capabilities_header_extensions_clear(NativeRtpCapabilities& value) noexcept;
void rtp_capabilities_header_extensions_add(NativeRtpCapabilities& value, const NativeRtpHeaderExtensionCapability& element) noexcept;

class NativeRtpParameters final {
 public:
  explicit NativeRtpParameters(webrtc::RtpParameters value) : value_(std::move(value)) {}
  const webrtc::RtpParameters& value() const noexcept { return value_; }
  webrtc::RtpParameters& value() noexcept { return value_; }
 private:
  webrtc::RtpParameters value_;
};
struct FfiRtpParametersFields;
std::unique_ptr<NativeRtpParameters> rtp_sender_parameters_value(const NativeRtpSender& sender) noexcept;
std::unique_ptr<NativeRtpParameters> rtp_receiver_parameters_value(const NativeRtpReceiver& receiver) noexcept;
bool rtp_receiver_set_parameters_value(const NativeRtpReceiver& receiver, const NativeRtpParameters& parameters) noexcept;
bool rtp_receiver_set_jitter_buffer_minimum_delay(const NativeRtpReceiver& receiver, bool present, double seconds) noexcept;
std::unique_ptr<NativeRtpParameters> rtp_parameters_default() noexcept;
std::unique_ptr<NativeRtpParameters> rtp_parameters_clone(const NativeRtpParameters& value) noexcept;
void rtp_parameters_assign(NativeRtpParameters& target, const NativeRtpParameters& source) noexcept;
bool rtp_parameters_equal(const NativeRtpParameters& lhs, const NativeRtpParameters& rhs) noexcept;
bool rtp_parameters_not_equal(const NativeRtpParameters& lhs, const NativeRtpParameters& rhs) noexcept;
FfiRtpParametersFields rtp_parameters_fields(const NativeRtpParameters& value) noexcept;
void rtp_parameters_set_transaction_id(NativeRtpParameters& value, rust::Slice<const std::uint8_t> bytes) noexcept;
void rtp_parameters_set_mid(NativeRtpParameters& value, rust::Slice<const std::uint8_t> bytes) noexcept;
void rtp_parameters_set_degradation_preference(NativeRtpParameters& value, bool present, std::int32_t raw) noexcept;
std::unique_ptr<NativeRtcpParameters> rtp_parameters_rtcp(const NativeRtpParameters& value) noexcept;
void rtp_parameters_set_rtcp(NativeRtpParameters& value, const NativeRtcpParameters& rtcp) noexcept;
std::unique_ptr<NativeRtpExtensionList> rtp_parameters_header_extensions(const NativeRtpParameters& value) noexcept;
void rtp_parameters_set_header_extensions(NativeRtpParameters& value, const NativeRtpExtensionList& extensions) noexcept;
bool rtp_parameters_is_mixed_codec(const NativeRtpParameters& value) noexcept;
rust::Vec<std::uint8_t> rtp_parameters_string(const NativeRtpParameters& value, bool& valid) noexcept;
rust::Vec<std::uint8_t> rtp_degradation_preference_string(std::int32_t raw, bool& valid) noexcept;
std::size_t rtp_parameters_codecs_len(const NativeRtpParameters& value) noexcept;
std::unique_ptr<NativeRtpCodecParameters> rtp_parameters_codecs_get(const NativeRtpParameters& value, std::size_t index) noexcept;
void rtp_parameters_codecs_clear(NativeRtpParameters& value) noexcept;
void rtp_parameters_codecs_add(NativeRtpParameters& value, const NativeRtpCodecParameters& element) noexcept;
std::size_t rtp_parameters_encodings_len(const NativeRtpParameters& value) noexcept;
std::unique_ptr<NativeRtpEncoding> rtp_parameters_encodings_get(const NativeRtpParameters& value, std::size_t index) noexcept;
void rtp_parameters_encodings_clear(NativeRtpParameters& value) noexcept;
void rtp_parameters_encodings_add(NativeRtpParameters& value, const NativeRtpEncoding& element) noexcept;
bool string_builder_append_rtp_parameters(NativeStringBuilder& output, const NativeRtpParameters& input) noexcept;
bool rtc_error_builder_append_rtp_parameters(NativeRtcError& output, const NativeRtpParameters& input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_rtp_parameters(std::unique_ptr<NativeRtcError> error, const NativeRtpParameters& input, bool& valid) noexcept;

std::size_t priority_value_hash(const NativePriorityValue& value) noexcept;

bool string_builder_append_codec(NativeStringBuilder& output, const NativeRtpCodec& input) noexcept;
bool rtc_error_builder_append_codec(NativeRtcError& output, const NativeRtpCodec& input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_codec(std::unique_ptr<NativeRtcError> error, const NativeRtpCodec& input, bool& valid) noexcept;
bool string_builder_append_encoding(NativeStringBuilder& output, const NativeRtpEncoding& input) noexcept;
bool rtc_error_builder_append_encoding(NativeRtcError& output, const NativeRtpEncoding& input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_encoding(std::unique_ptr<NativeRtcError> error, const NativeRtpEncoding& input, bool& valid) noexcept;
bool string_builder_append_media_type(NativeStringBuilder& output, std::int32_t kind) noexcept;
bool rtc_error_builder_append_media_type(NativeRtcError& output, std::int32_t kind) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_media_type(std::unique_ptr<NativeRtcError> error, std::int32_t kind, bool& valid) noexcept;
bool string_builder_append_feedback_type(NativeStringBuilder& output, std::int32_t kind) noexcept;
bool rtc_error_builder_append_feedback_type(NativeRtcError& output, std::int32_t kind) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_feedback_type(std::unique_ptr<NativeRtcError> error, std::int32_t kind, bool& valid) noexcept;
bool string_builder_append_optional_feedback_type(NativeStringBuilder& output, bool present, std::int32_t kind) noexcept;
bool rtc_error_builder_append_optional_feedback_type(NativeRtcError& output, bool present, std::int32_t kind) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_optional_feedback_type(std::unique_ptr<NativeRtcError> error, bool present, std::int32_t kind, bool& valid) noexcept;
}
