#pragma once
#include <cstdint>
#include <memory>
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
class NativeRtcEvent;
struct FfiEventParseStatus;
struct FfiEventParseResult;

// Private synchronous loan container. Rust keeps every input event borrowed
// until Encode returns; neither pointers nor this container escape that call.
class NativeEventEncoding final {
 public:
  struct State;
  explicit NativeEventEncoding(std::unique_ptr<State> state) noexcept;
  ~NativeEventEncoding();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeEventEncoding> event_codec_encoder(std::uint32_t kind) noexcept;
bool event_codec_add(NativeEventEncoding& batch, const NativeRtcEvent& event) noexcept;
rust::Vec<std::uint8_t> event_codec_encode(const NativeEventEncoding& batch) noexcept;
FfiEventParseResult event_codec_parse(std::uint32_t kind, rust::Slice<const std::uint8_t> bytes, bool batched) noexcept;
FfiEventParseStatus event_codec_status(bool success, rust::Str error, rust::Str file, std::int32_t line) noexcept;
}  // namespace pulsebeam::webrtc_sys
