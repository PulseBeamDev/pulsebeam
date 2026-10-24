#include "pulsebeam-webrtc-sys/native/rtc_error.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <string>
#include <string_view>
#include <limits>
#include <bit>
#include "absl/strings/str_format.h"

namespace pulsebeam::webrtc_sys {
namespace {
// All supported SDK artifacts use 32-bit int/unsigned. Fail compilation rather
// than silently narrowing or omitting part of an exotic native integer domain.
static_assert(std::numeric_limits<int>::digits == 31);
static_assert(std::numeric_limits<unsigned>::digits == 32);
std::string String(rust::Str value) { return std::string(value.data(), value.size()); }
absl::string_view View(rust::Slice<const std::uint8_t> value) {
  return {reinterpret_cast<const char*>(value.data()), value.size()};
}
rust::Vec<std::uint8_t> Bytes(std::string_view value) {
  rust::Vec<std::uint8_t> result;
  result.reserve(value.size());
  for (unsigned char byte : value) result.push_back(byte);
  return result;
}
bool HasTypeName(std::int32_t value) {
  return value >= 0 && value <= static_cast<std::int32_t>(webrtc::RTCErrorType::OPERATION_ERROR_WITH_DATA);
}
bool HasDetailName(std::int32_t value) {
  return value >= 0 && value <= static_cast<std::int32_t>(webrtc::RTCErrorDetailType::HARDWARE_ENCODER_ERROR);
}
}
std::unique_ptr<NativeRtcError> rtc_error_default() noexcept {
  return std::make_unique<NativeRtcError>(webrtc::RTCError());
}
std::unique_ptr<NativeRtcError> rtc_error_for_type(std::int32_t kind) noexcept {
  return std::make_unique<NativeRtcError>(webrtc::RTCError(static_cast<webrtc::RTCErrorType>(kind)));
}
std::unique_ptr<NativeRtcError> rtc_error_new(std::int32_t kind, rust::Slice<const std::uint8_t> message) noexcept {
  return std::make_unique<NativeRtcError>(webrtc::RTCError(static_cast<webrtc::RTCErrorType>(kind), View(message)));
}
std::unique_ptr<NativeRtcError> rtc_error_factory(std::int32_t kind, rust::Slice<const std::uint8_t> message) noexcept {
  const auto text = View(message);
  switch (static_cast<webrtc::RTCErrorType>(kind)) {
    case webrtc::RTCErrorType::NONE: return std::make_unique<NativeRtcError>(webrtc::RTCError::OK());
    case webrtc::RTCErrorType::UNSUPPORTED_OPERATION: return std::make_unique<NativeRtcError>(webrtc::RTCError::UnsupportedOperation(text));
    case webrtc::RTCErrorType::UNSUPPORTED_PARAMETER: return std::make_unique<NativeRtcError>(webrtc::RTCError::UnsupportedParameter(text));
    case webrtc::RTCErrorType::INVALID_PARAMETER: return std::make_unique<NativeRtcError>(webrtc::RTCError::InvalidParameter(text));
    case webrtc::RTCErrorType::INVALID_RANGE: return std::make_unique<NativeRtcError>(webrtc::RTCError::InvalidRange(text));
    case webrtc::RTCErrorType::SYNTAX_ERROR: return std::make_unique<NativeRtcError>(webrtc::RTCError::SyntaxError(text));
    case webrtc::RTCErrorType::INVALID_STATE: return std::make_unique<NativeRtcError>(webrtc::RTCError::InvalidState(text));
    case webrtc::RTCErrorType::INVALID_MODIFICATION: return std::make_unique<NativeRtcError>(webrtc::RTCError::InvalidModification(text));
    case webrtc::RTCErrorType::NETWORK_ERROR: return std::make_unique<NativeRtcError>(webrtc::RTCError::NetworkError(text));
    case webrtc::RTCErrorType::RESOURCE_EXHAUSTED: return std::make_unique<NativeRtcError>(webrtc::RTCError::ResourceExhausted(text));
    case webrtc::RTCErrorType::INTERNAL_ERROR: return std::make_unique<NativeRtcError>(webrtc::RTCError::InternalError(text));
    case webrtc::RTCErrorType::OPERATION_ERROR_WITH_DATA: return std::make_unique<NativeRtcError>(webrtc::RTCError::OperationErrorWithData(text));
  }
  return nullptr;  // Only known named factories reach here from safe Rust.
}
std::unique_ptr<NativeRtcError> rtc_error_clone(const NativeRtcError& error) noexcept {
  return std::make_unique<NativeRtcError>(error.value());
}
void rtc_error_assign(NativeRtcError& target, const NativeRtcError& source) noexcept { target.value() = source.value(); }
void rtc_error_move_assign(NativeRtcError& target, std::unique_ptr<NativeRtcError> source) noexcept { target.value() = std::move(source->value()); }
FfiRtcError rtc_error_fields(const NativeRtcError& error) noexcept {
  FfiRtcError out;
  out.kind = static_cast<std::int32_t>(error.value().type());
  out.detail = static_cast<std::int32_t>(error.value().error_detail());
  out.message_bytes = Bytes(error.value().message());
  out.has_sctp_cause_code = error.value().sctp_cause_code().has_value();
  out.sctp_cause_code = error.value().sctp_cause_code().value_or(0);
  return out;
}
bool rtc_error_is_ok(const NativeRtcError& error) noexcept { return error.value().ok(); }
void rtc_error_set_type(NativeRtcError& error, std::int32_t kind) noexcept { error.value().set_type(static_cast<webrtc::RTCErrorType>(kind)); }
void rtc_error_set_message(NativeRtcError& error, rust::Slice<const std::uint8_t> message) noexcept { error.value().set_message(View(message)); }
void rtc_error_set_detail(NativeRtcError& error, std::int32_t detail) noexcept { error.value().set_error_detail(static_cast<webrtc::RTCErrorDetailType>(detail)); }
void rtc_error_set_sctp_cause(NativeRtcError& error, std::uint16_t cause) noexcept { error.value().set_sctp_cause_code(cause); }
void rtc_error_append_text(NativeRtcError& error, rust::Str text) noexcept { error.value().string_builder() << String(text); }
std::unique_ptr<NativeRtcError> rtc_error_with_appended_bytes(std::unique_ptr<NativeRtcError> error, rust::Slice<const std::uint8_t> text) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << View(text));
}
std::unique_ptr<NativeRtcError> rtc_error_stream_byte(std::unique_ptr<NativeRtcError> error, std::uint8_t input) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << std::bit_cast<char>(input));
}
std::unique_ptr<NativeRtcError> rtc_error_stream_i32(std::unique_ptr<NativeRtcError> error, std::int32_t input) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << static_cast<int>(input));
}
std::unique_ptr<NativeRtcError> rtc_error_stream_u32(std::unique_ptr<NativeRtcError> error, std::uint32_t input) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << static_cast<unsigned>(input));
}
std::unique_ptr<NativeRtcError> rtc_error_stream_i64(std::unique_ptr<NativeRtcError> error, std::int64_t input) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << static_cast<long long>(input));
}
std::unique_ptr<NativeRtcError> rtc_error_stream_u64(std::unique_ptr<NativeRtcError> error, std::uint64_t input) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << static_cast<unsigned long long>(input));
}
std::unique_ptr<NativeRtcError> rtc_error_stream_f32(std::unique_ptr<NativeRtcError> error, float input) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << input);
}
std::unique_ptr<NativeRtcError> rtc_error_stream_f64(std::unique_ptr<NativeRtcError> error, double input) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << input);
}
std::unique_ptr<NativeRtcError> rtc_error_stream_bool(std::unique_ptr<NativeRtcError> error, bool input) noexcept {
  return std::make_unique<NativeRtcError>(std::move(error->value()) << input);
}
std::unique_ptr<NativeRtcError> rtc_error_stream_long(std::unique_ptr<NativeRtcError> error, std::int64_t input, bool& valid) noexcept {
  valid = input >= std::numeric_limits<long>::min() && input <= std::numeric_limits<long>::max();
  if (!valid) return error;
  return std::make_unique<NativeRtcError>(std::move(error->value()) << static_cast<long>(input));
}
std::unique_ptr<NativeRtcError> rtc_error_stream_unsigned_long(std::unique_ptr<NativeRtcError> error, std::uint64_t input, bool& valid) noexcept {
  valid = input <= std::numeric_limits<unsigned long>::max();
  if (!valid) return error;
  return std::make_unique<NativeRtcError>(std::move(error->value()) << static_cast<unsigned long>(input));
}
rust::String rtc_error_type_name(std::int32_t kind, bool& valid) noexcept {
  // api/rtc_error.cc ToString indexes an array without a bounds check. Unknown
  // values remain stored/observable; only this unsafe consumption is rejected.
  valid = HasTypeName(kind);
  if (!valid) return rust::String();
  const auto text = webrtc::ToString(static_cast<webrtc::RTCErrorType>(kind));
  return rust::String(text.data(), text.size());
}
rust::String rtc_error_detail_name(std::int32_t detail, bool& valid) noexcept {
  valid = HasDetailName(detail);
  if (!valid) return rust::String();
  const auto text = webrtc::ToString(static_cast<webrtc::RTCErrorDetailType>(detail));
  return rust::String(text.data(), text.size());
}
rust::Vec<std::uint8_t> rtc_error_string(const NativeRtcError& error, bool& valid) noexcept {
  valid = HasTypeName(static_cast<std::int32_t>(error.value().type()));
  if (!valid) return rust::Vec<std::uint8_t>();
  return Bytes(absl::StrFormat("%v", error.value()));
}
rust::Slice<const std::uint8_t> rtc_error_builder_bytes(NativeRtcError& error) noexcept {
  const auto& text = error.value().string_builder().str();
  return {reinterpret_cast<const std::uint8_t*>(text.data()), text.size()};
}
std::size_t rtc_error_builder_size(const NativeRtcError& error) noexcept {
  // string_builder() is non-const, so use the const message getter only for
  // message fields above. Full builder reads happen through the exclusive
  // Rust access facade. This helper's C++ const view does not mutate storage.
  return const_cast<webrtc::RTCError&>(error.value()).string_builder().size();
}
void rtc_error_builder_clear(NativeRtcError& error) noexcept { error.value().string_builder().Clear(); }
rust::Vec<std::uint8_t> rtc_error_builder_release(NativeRtcError& error) noexcept { return Bytes(error.value().string_builder().Release()); }
void rtc_error_append_byte(NativeRtcError& error, std::uint8_t value) noexcept { error.value().string_builder() << std::bit_cast<char>(value); }
void rtc_error_append_bytes(NativeRtcError& error, rust::Slice<const std::uint8_t> value) noexcept { error.value().string_builder() << View(value); }
void rtc_error_append_i32(NativeRtcError& error, std::int32_t value) noexcept { error.value().string_builder() << static_cast<int>(value); }
void rtc_error_append_u32(NativeRtcError& error, std::uint32_t value) noexcept { error.value().string_builder() << static_cast<unsigned>(value); }
void rtc_error_append_i64(NativeRtcError& error, std::int64_t value) noexcept { error.value().string_builder() << static_cast<long long>(value); }
void rtc_error_append_u64(NativeRtcError& error, std::uint64_t value) noexcept { error.value().string_builder() << static_cast<unsigned long long>(value); }
bool rtc_error_append_long(NativeRtcError& error, std::int64_t value) noexcept {
  if (value < std::numeric_limits<long>::min() || value > std::numeric_limits<long>::max()) return false;
  error.value().string_builder() << static_cast<long>(value);
  return true;
}
bool rtc_error_append_unsigned_long(NativeRtcError& error, std::uint64_t value) noexcept {
  if (value > std::numeric_limits<unsigned long>::max()) return false;
  error.value().string_builder() << static_cast<unsigned long>(value);
  return true;
}
void rtc_error_append_f32(NativeRtcError& error, float value) noexcept { error.value().string_builder() << value; }
void rtc_error_append_f64(NativeRtcError& error, double value) noexcept { error.value().string_builder() << value; }

}
