#pragma once
#include <cstdint>
#include <memory>
#include <utility>
#include "api/rtc_error.h"
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
struct FfiRtcError;
class NativeRtcError final {
 public:
  explicit NativeRtcError(webrtc::RTCError value) : value_(std::move(value)) {}
  const webrtc::RTCError& value() const noexcept { return value_; }
  webrtc::RTCError& value() noexcept { return value_; }
 private:
  webrtc::RTCError value_;
};
std::unique_ptr<NativeRtcError> rtc_error_default() noexcept;
std::unique_ptr<NativeRtcError> rtc_error_for_type(std::int32_t kind) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_new(std::int32_t kind, rust::Slice<const std::uint8_t> message) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_factory(std::int32_t kind, rust::Slice<const std::uint8_t> message) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_clone(const NativeRtcError& error) noexcept;
void rtc_error_assign(NativeRtcError& target, const NativeRtcError& source) noexcept;
void rtc_error_move_assign(NativeRtcError& target, std::unique_ptr<NativeRtcError> source) noexcept;
FfiRtcError rtc_error_fields(const NativeRtcError& error) noexcept;
bool rtc_error_is_ok(const NativeRtcError& error) noexcept;
void rtc_error_set_type(NativeRtcError& error, std::int32_t kind) noexcept;
void rtc_error_set_message(NativeRtcError& error, rust::Slice<const std::uint8_t> message) noexcept;
void rtc_error_set_detail(NativeRtcError& error, std::int32_t detail) noexcept;
void rtc_error_set_sctp_cause(NativeRtcError& error, std::uint16_t cause) noexcept;
void rtc_error_append_text(NativeRtcError& error, rust::Str text) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_with_appended_bytes(std::unique_ptr<NativeRtcError> error, rust::Slice<const std::uint8_t> text) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_byte(std::unique_ptr<NativeRtcError> error, std::uint8_t input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_i32(std::unique_ptr<NativeRtcError> error, std::int32_t input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_u32(std::unique_ptr<NativeRtcError> error, std::uint32_t input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_i64(std::unique_ptr<NativeRtcError> error, std::int64_t input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_u64(std::unique_ptr<NativeRtcError> error, std::uint64_t input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_f32(std::unique_ptr<NativeRtcError> error, float input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_f64(std::unique_ptr<NativeRtcError> error, double input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_bool(std::unique_ptr<NativeRtcError> error, bool input) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_long(std::unique_ptr<NativeRtcError> error, std::int64_t input, bool& valid) noexcept;
std::unique_ptr<NativeRtcError> rtc_error_stream_unsigned_long(std::unique_ptr<NativeRtcError> error, std::uint64_t input, bool& valid) noexcept;
rust::String rtc_error_type_name(std::int32_t kind, bool& valid) noexcept;
rust::String rtc_error_detail_name(std::int32_t detail, bool& valid) noexcept;
rust::Vec<std::uint8_t> rtc_error_string(const NativeRtcError& error, bool& valid) noexcept;
rust::Slice<const std::uint8_t> rtc_error_builder_bytes(NativeRtcError& error) noexcept;
std::size_t rtc_error_builder_size(const NativeRtcError& error) noexcept;
void rtc_error_builder_clear(NativeRtcError& error) noexcept;
rust::Vec<std::uint8_t> rtc_error_builder_release(NativeRtcError& error) noexcept;
void rtc_error_append_byte(NativeRtcError& error, std::uint8_t value) noexcept;
void rtc_error_append_bytes(NativeRtcError& error, rust::Slice<const std::uint8_t> value) noexcept;
void rtc_error_append_i32(NativeRtcError& error, std::int32_t value) noexcept;
void rtc_error_append_u32(NativeRtcError& error, std::uint32_t value) noexcept;
void rtc_error_append_i64(NativeRtcError& error, std::int64_t value) noexcept;
void rtc_error_append_u64(NativeRtcError& error, std::uint64_t value) noexcept;
bool rtc_error_append_long(NativeRtcError& error, std::int64_t value) noexcept;
bool rtc_error_append_unsigned_long(NativeRtcError& error, std::uint64_t value) noexcept;
void rtc_error_append_f32(NativeRtcError& error, float value) noexcept;
void rtc_error_append_f64(NativeRtcError& error, double value) noexcept;
}
