#pragma once
#include <cstdint>
#include <memory>
#include "rtc_base/strings/string_builder.h"
#include "rust/cxx.h"
namespace pulsebeam::webrtc_sys {
class NativeRtcError;
class NativeStringBuilder final {
 public:
  NativeStringBuilder() = default;
  explicit NativeStringBuilder(absl::string_view text) : value_(text) {}
  explicit NativeStringBuilder(const webrtc::StringBuilder& value) : value_(value) {}
  webrtc::StringBuilder& value() noexcept { return value_; }
  const webrtc::StringBuilder& value() const noexcept { return value_; }
 private:
  webrtc::StringBuilder value_;
};
std::unique_ptr<NativeStringBuilder> string_builder_default() noexcept;
std::unique_ptr<NativeStringBuilder> string_builder_new(rust::Slice<const std::uint8_t> text) noexcept;
std::unique_ptr<NativeStringBuilder> string_builder_clone(const NativeStringBuilder& value) noexcept;
void string_builder_assign(NativeStringBuilder& target, const NativeStringBuilder& source) noexcept;
rust::Slice<const std::uint8_t> string_builder_bytes(const NativeStringBuilder& value) noexcept;
std::size_t string_builder_size(const NativeStringBuilder& value) noexcept;
void string_builder_clear(NativeStringBuilder& value) noexcept;
rust::Vec<std::uint8_t> string_builder_release(NativeStringBuilder& value) noexcept;
void string_builder_append_bytes(NativeStringBuilder& value, rust::Slice<const std::uint8_t> text) noexcept;
void string_builder_append_byte(NativeStringBuilder& value, std::uint8_t input) noexcept;
void string_builder_append_i32(NativeStringBuilder& value, std::int32_t input) noexcept;
void string_builder_append_u32(NativeStringBuilder& value, std::uint32_t input) noexcept;
void string_builder_append_i64(NativeStringBuilder& value, std::int64_t input) noexcept;
void string_builder_append_u64(NativeStringBuilder& value, std::uint64_t input) noexcept;
bool string_builder_append_long(NativeStringBuilder& value, std::int64_t input) noexcept;
bool string_builder_append_unsigned_long(NativeStringBuilder& value, std::uint64_t input) noexcept;
void string_builder_append_f32(NativeStringBuilder& value, float input) noexcept;
void string_builder_append_f64(NativeStringBuilder& value, double input) noexcept;
std::unique_ptr<NativeStringBuilder> rtc_error_copy_message_builder(const NativeRtcError& error) noexcept;
void rtc_error_assign_message_builder(NativeRtcError& error, const NativeStringBuilder& source) noexcept;
}
