#pragma once
#include <cstdint>
#include <memory>
#include "api/video_codecs/video_decoder.h"
#include "api/video_codecs/video_encoder.h"
#include "api/fec_controller_override.h"
#include "rust/cxx.h"
namespace pulsebeam::webrtc_sys {
struct RustDecodedImageSink;
struct RustEncodedImageSink;
struct RustFecOverride;
class NativeFecOverride final {
 public:
  struct State;
  explicit NativeFecOverride(std::shared_ptr<State> state) : state_(std::move(state)) {}
  const std::shared_ptr<State>& state() const { return state_; }
  webrtc::FecControllerOverride* callback() const noexcept;
 private:
  std::shared_ptr<State> state_;
};
std::shared_ptr<NativeFecOverride> BorrowFecOverride(webrtc::FecControllerOverride*);
void RetireFecOverride(const NativeFecOverride&);
std::shared_ptr<NativeFecOverride> fec_override_new(rust::Box<RustFecOverride>) noexcept;
bool fec_override_same(const NativeFecOverride&, const NativeFecOverride&) noexcept;
bool fec_override_allowed(const NativeFecOverride&, bool) noexcept;
bool fec_override_retired(const NativeFecOverride&) noexcept;
struct FfiEncodedCallbackResult;
class NativeEncodedImage;
class NativeCodecSpecificInfo;
class NativeEncodedImageSink final {
 public:
  struct State;
  explicit NativeEncodedImageSink(std::shared_ptr<State> state) : state_(std::move(state)) {}
  const std::shared_ptr<State>& state() const { return state_; }
  webrtc::EncodedImageCallback* callback() const noexcept;
 private:
  std::shared_ptr<State> state_;
};
std::unique_ptr<NativeEncodedImageSink> encoded_sink_new(rust::Box<RustEncodedImageSink>) noexcept;
std::unique_ptr<NativeEncodedImageSink> encoded_sink_clone(const NativeEncodedImageSink&) noexcept;
bool encoded_sink_same(const NativeEncodedImageSink&, const NativeEncodedImageSink&) noexcept;
FfiEncodedCallbackResult encoded_sink_frame(const NativeEncodedImageSink&, const NativeEncodedImage&, const NativeCodecSpecificInfo*) noexcept;
void encoded_sink_dropped(const NativeEncodedImageSink&, std::uint32_t, std::int32_t, bool) noexcept;
class NativeRawVideoFrame;
class NativeDecodedImageSink final {
 public:
  struct State;
  explicit NativeDecodedImageSink(std::shared_ptr<State> state) : state_(std::move(state)) {}
  const std::shared_ptr<State>& state() const { return state_; }
  webrtc::DecodedImageCallback* callback() const noexcept;
 private:
  std::shared_ptr<State> state_;
};
std::unique_ptr<NativeDecodedImageSink> decoded_sink_new(rust::Box<RustDecodedImageSink>) noexcept;
std::unique_ptr<NativeDecodedImageSink> decoded_sink_clone(const NativeDecodedImageSink&) noexcept;
bool decoded_sink_same(const NativeDecodedImageSink&, const NativeDecodedImageSink&) noexcept;
std::int32_t decoded_sink_frame(const NativeDecodedImageSink&, NativeRawVideoFrame&, std::uint8_t, std::int64_t, bool, std::int32_t, bool, std::uint8_t) noexcept;
}  // namespace pulsebeam::webrtc_sys
