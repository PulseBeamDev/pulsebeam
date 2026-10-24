#pragma once

#include "api/scoped_refptr.h"
#include "api/video_codecs/video_encoder_factory.h"
#include "rust/cxx.h"
#include <cstdint>
#include <memory>

namespace pulsebeam::webrtc_sys {
struct RustVideoEncoderSelector;
struct RustVideoEncoderSelectorObserver;
class NativeOwnedVideoEncoderSelector;
rust::Box<RustVideoEncoderSelectorObserver> video_selector_owned_observer(const NativeOwnedVideoEncoderSelector&) noexcept;
class NativeSdpVideoFormat;
class NativeRenderResolution;

// Closed producer graph: caller Rust selectors only. The unique product starts
// with SDK refcount zero; its consuming conversion increments it exactly once.
class NativeOwnedVideoEncoderSelector final {
public:
  explicit NativeOwnedVideoEncoderSelector(
      std::unique_ptr<webrtc::VideoEncoderFactory::EncoderSelectorInterface>
          value)
      : value_(std::move(value)) {}
  webrtc::VideoEncoderFactory::EncoderSelectorInterface &value() const noexcept {
    return *value_;
  }
  std::unique_ptr<webrtc::VideoEncoderFactory::EncoderSelectorInterface>
  Take() noexcept {
    return std::move(value_);
  }

private:
  std::unique_ptr<webrtc::VideoEncoderFactory::EncoderSelectorInterface> value_;
};
class NativeVideoEncoderSelector final {
public:
  explicit NativeVideoEncoderSelector(
      webrtc::scoped_refptr<
          webrtc::VideoEncoderFactory::EncoderSelectorInterface>
          value)
      : value_(std::move(value)) {}
  const webrtc::scoped_refptr<
      webrtc::VideoEncoderFactory::EncoderSelectorInterface> &
  value() const noexcept {
    return value_;
  }

private:
  webrtc::scoped_refptr<webrtc::VideoEncoderFactory::EncoderSelectorInterface>
      value_;
};
std::unique_ptr<NativeOwnedVideoEncoderSelector>
    video_selector_owned_new(rust::Box<RustVideoEncoderSelector>) noexcept;
std::unique_ptr<NativeVideoEncoderSelector> video_selector_into_shared(
    std::unique_ptr<NativeOwnedVideoEncoderSelector>) noexcept;
void video_selector_owned_current(const NativeOwnedVideoEncoderSelector &,
                                  const NativeSdpVideoFormat &) noexcept;
std::unique_ptr<NativeSdpVideoFormat> video_selector_owned_bitrate(
    const NativeOwnedVideoEncoderSelector &, std::int64_t, std::int8_t,
    std::int32_t &) noexcept;
std::unique_ptr<NativeSdpVideoFormat> video_selector_owned_resolution(
    const NativeOwnedVideoEncoderSelector &, const NativeRenderResolution &) noexcept;
std::unique_ptr<NativeSdpVideoFormat>
video_selector_owned_broken(const NativeOwnedVideoEncoderSelector &) noexcept;
std::unique_ptr<NativeVideoEncoderSelector>
video_selector_clone(const NativeVideoEncoderSelector &) noexcept;
bool video_selector_same(const NativeVideoEncoderSelector &,
                         const NativeVideoEncoderSelector &) noexcept;
void video_selector_current(const NativeVideoEncoderSelector &,
                            const NativeSdpVideoFormat &) noexcept;
std::unique_ptr<NativeSdpVideoFormat>
video_selector_bitrate(const NativeVideoEncoderSelector &, std::int64_t,
                       std::int8_t, std::int32_t &) noexcept;
std::unique_ptr<NativeSdpVideoFormat>
video_selector_resolution(const NativeVideoEncoderSelector &,
                          const NativeRenderResolution &) noexcept;
std::unique_ptr<NativeSdpVideoFormat>
video_selector_broken(const NativeVideoEncoderSelector &) noexcept;
} // namespace pulsebeam::webrtc_sys
