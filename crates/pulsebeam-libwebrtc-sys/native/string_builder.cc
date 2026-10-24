#include "pulsebeam-webrtc-sys/native/string_builder.h"
#include "pulsebeam-webrtc-sys/native/rtc_error.h"
#include <bit>
#include <limits>
#include <string_view>
namespace pulsebeam::webrtc_sys {
namespace {
static_assert(std::numeric_limits<int>::digits == 31);
static_assert(std::numeric_limits<unsigned>::digits == 32);
absl::string_view View(rust::Slice<const std::uint8_t> text) {
  return {reinterpret_cast<const char*>(text.data()), text.size()};
}
rust::Vec<std::uint8_t> Bytes(std::string_view text) {
  rust::Vec<std::uint8_t> output;
  output.reserve(text.size());
  for (unsigned char byte : text) output.push_back(byte);
  return output;
}
}
std::unique_ptr<NativeStringBuilder> string_builder_default() noexcept { return std::make_unique<NativeStringBuilder>(); }
std::unique_ptr<NativeStringBuilder> string_builder_new(rust::Slice<const std::uint8_t> text) noexcept { return std::make_unique<NativeStringBuilder>(View(text)); }
std::unique_ptr<NativeStringBuilder> string_builder_clone(const NativeStringBuilder& value) noexcept { return std::make_unique<NativeStringBuilder>(value.value()); }
void string_builder_assign(NativeStringBuilder& target, const NativeStringBuilder& source) noexcept { target.value() = source.value(); }
rust::Slice<const std::uint8_t> string_builder_bytes(const NativeStringBuilder& value) noexcept {
  const auto& text = value.value().str();
  return {reinterpret_cast<const std::uint8_t*>(text.data()), text.size()};
}
std::size_t string_builder_size(const NativeStringBuilder& value) noexcept { return value.value().size(); }
void string_builder_clear(NativeStringBuilder& value) noexcept { value.value().Clear(); }
rust::Vec<std::uint8_t> string_builder_release(NativeStringBuilder& value) noexcept { return Bytes(value.value().Release()); }
void string_builder_append_bytes(NativeStringBuilder& value, rust::Slice<const std::uint8_t> text) noexcept { value.value() << View(text); }
void string_builder_append_byte(NativeStringBuilder& value, std::uint8_t input) noexcept { value.value() << std::bit_cast<char>(input); }
void string_builder_append_i32(NativeStringBuilder& value, std::int32_t input) noexcept { value.value() << static_cast<int>(input); }
void string_builder_append_u32(NativeStringBuilder& value, std::uint32_t input) noexcept { value.value() << static_cast<unsigned>(input); }
void string_builder_append_i64(NativeStringBuilder& value, std::int64_t input) noexcept { value.value() << static_cast<long long>(input); }
void string_builder_append_u64(NativeStringBuilder& value, std::uint64_t input) noexcept { value.value() << static_cast<unsigned long long>(input); }
bool string_builder_append_long(NativeStringBuilder& value, std::int64_t input) noexcept {
  if (input < std::numeric_limits<long>::min() || input > std::numeric_limits<long>::max()) return false;
  value.value() << static_cast<long>(input);
  return true;
}
bool string_builder_append_unsigned_long(NativeStringBuilder& value, std::uint64_t input) noexcept {
  if (input > std::numeric_limits<unsigned long>::max()) return false;
  value.value() << static_cast<unsigned long>(input);
  return true;
}
void string_builder_append_f32(NativeStringBuilder& value, float input) noexcept { value.value() << input; }
void string_builder_append_f64(NativeStringBuilder& value, double input) noexcept { value.value() << input; }
std::unique_ptr<NativeStringBuilder> rtc_error_copy_message_builder(const NativeRtcError& error) noexcept {
  // The pinned getter only returns its member; this cast obtains a const read
  // for the actual StringBuilder copy constructor, with no source mutation.
  return std::make_unique<NativeStringBuilder>(const_cast<webrtc::RTCError&>(error.value()).string_builder());
}
void rtc_error_assign_message_builder(NativeRtcError& error, const NativeStringBuilder& source) noexcept { error.value().string_builder() = source.value(); }
}
