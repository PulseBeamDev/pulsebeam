#include "pulsebeam-webrtc-sys/native/video_encoder_selector.h"
#include "api/units/data_rate.h"
#include "pulsebeam-webrtc-sys/native/video_codec_values.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "rtc_base/checks.h"
#include <limits>
#include <optional>

namespace pulsebeam::webrtc_sys {
namespace {
std::optional<webrtc::SdpVideoFormat>
Selection(std::unique_ptr<NativeSdpVideoFormat> value) {
  return value
             ? std::optional<webrtc::SdpVideoFormat>(std::move(value->value()))
             : std::nullopt;
}
std::unique_ptr<NativeSdpVideoFormat>
Owning(std::optional<webrtc::SdpVideoFormat> value) {
  return value ? std::make_unique<NativeSdpVideoFormat>(std::move(*value))
               : nullptr;
}
class RustSelector final
    : public webrtc::VideoEncoderFactory::EncoderSelectorInterface {
public:
  explicit RustSelector(rust::Box<RustVideoEncoderSelector> provider)
      : provider_(std::move(provider)) {}
  rust::Box<RustVideoEncoderSelectorObserver> Observer() const { return video_selector_provider_observer(*provider_); }
  void OnCurrentEncoder(const webrtc::SdpVideoFormat &format) override {
    video_selector_provider_current(
        *provider_, std::make_unique<NativeSdpVideoFormat>(format));
  }
  std::optional<webrtc::SdpVideoFormat>
  OnAvailableBitrate(const webrtc::DataRate &rate) override {
    const auto kind = rate.IsPlusInfinity()    ? 1
                      : rate.IsMinusInfinity() ? -1
                                               : 0;
    return Selection(video_selector_provider_bitrate(
        *provider_, kind == 0 ? rate.bps() : 0, kind));
  }
  std::optional<webrtc::SdpVideoFormat>
  OnResolutionChange(const webrtc::RenderResolution &resolution) override {
    return Selection(video_selector_provider_resolution(
        *provider_, std::make_unique<NativeRenderResolution>(resolution)));
  }
  std::optional<webrtc::SdpVideoFormat> OnEncoderBroken() override {
    return Selection(video_selector_provider_broken(*provider_));
  }

private:
  rust::Box<RustVideoEncoderSelector> provider_;
};
} // namespace
rust::Box<RustVideoEncoderSelectorObserver> video_selector_owned_observer(const NativeOwnedVideoEncoderSelector& value) noexcept {
  // Closed producer graph: SDK builtin factories return no selector; every
  // non-null factory/owned product is our RustSelector with its own observer.
  return static_cast<const RustSelector&>(value.value()).Observer();
}
std::unique_ptr<NativeOwnedVideoEncoderSelector> video_selector_owned_new(
    rust::Box<RustVideoEncoderSelector> provider) noexcept {
  return std::make_unique<NativeOwnedVideoEncoderSelector>(
      std::make_unique<RustSelector>(std::move(provider)));
}
std::unique_ptr<NativeVideoEncoderSelector> video_selector_into_shared(
    std::unique_ptr<NativeOwnedVideoEncoderSelector> owner) noexcept {
  return std::make_unique<NativeVideoEncoderSelector>(
      webrtc::scoped_refptr<
          webrtc::VideoEncoderFactory::EncoderSelectorInterface>(
          owner->Take().release()));
}
std::unique_ptr<NativeVideoEncoderSelector>
video_selector_clone(const NativeVideoEncoderSelector &value) noexcept {
  return std::make_unique<NativeVideoEncoderSelector>(value.value());
}
bool video_selector_same(const NativeVideoEncoderSelector &a,
                         const NativeVideoEncoderSelector &b) noexcept {
  return a.value() == b.value();
}
void video_selector_current(const NativeVideoEncoderSelector &selector,
                            const NativeSdpVideoFormat &format) noexcept {
  selector.value()->OnCurrentEncoder(format.value());
}
namespace {
std::unique_ptr<NativeSdpVideoFormat>
Bitrate(webrtc::VideoEncoderFactory::EncoderSelectorInterface &selector,
        std::int64_t bits, std::int8_t kind, std::int32_t &status) noexcept {
  status = 0;
  std::optional<webrtc::DataRate> rate;
  if (kind == 1)
    rate = webrtc::DataRate::PlusInfinity();
  else if (kind == -1)
    rate = webrtc::DataRate::MinusInfinity();
  else {
    if (bits == INT64_MIN || bits == INT64_MAX) {
      status = 1;
      return nullptr;
    }
#if RTC_DCHECK_IS_ON
    if (bits < 0) {
      status = 2;
      return nullptr;
    }
#endif
    rate = webrtc::DataRate::BitsPerSec(bits);
  }
  return Owning(selector.OnAvailableBitrate(*rate));
}
} // namespace
std::unique_ptr<NativeSdpVideoFormat>
video_selector_bitrate(const NativeVideoEncoderSelector &selector,
                       std::int64_t bits, std::int8_t kind,
                       std::int32_t &status) noexcept {
  return Bitrate(*selector.value(), bits, kind, status);
}
void video_selector_owned_current(const NativeOwnedVideoEncoderSelector &selector,
                                  const NativeSdpVideoFormat &format) noexcept {
  selector.value().OnCurrentEncoder(format.value());
}
std::unique_ptr<NativeSdpVideoFormat> video_selector_owned_bitrate(
    const NativeOwnedVideoEncoderSelector &selector, std::int64_t bits,
    std::int8_t kind, std::int32_t &status) noexcept {
  return Bitrate(selector.value(), bits, kind, status);
}
std::unique_ptr<NativeSdpVideoFormat> video_selector_owned_resolution(
    const NativeOwnedVideoEncoderSelector &selector,
    const NativeRenderResolution &resolution) noexcept {
  return Owning(selector.value().OnResolutionChange(resolution.value()));
}
std::unique_ptr<NativeSdpVideoFormat>
video_selector_owned_broken(const NativeOwnedVideoEncoderSelector &selector) noexcept {
  return Owning(selector.value().OnEncoderBroken());
}
std::unique_ptr<NativeSdpVideoFormat>
video_selector_resolution(const NativeVideoEncoderSelector &selector,
                          const NativeRenderResolution &resolution) noexcept {
  return Owning(selector.value()->OnResolutionChange(resolution.value()));
}
std::unique_ptr<NativeSdpVideoFormat>
video_selector_broken(const NativeVideoEncoderSelector &selector) noexcept {
  return Owning(selector.value()->OnEncoderBroken());
}
} // namespace pulsebeam::webrtc_sys
