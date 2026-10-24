#include "pulsebeam-webrtc-sys/native/video_codec_callbacks.h"
#include "pulsebeam-webrtc-sys/native/raw_video_frame.h"
#include "pulsebeam-webrtc-sys/native/encoded_image.h"
#include "pulsebeam-webrtc-sys/native/video_codec_values.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <mutex>
#include <condition_variable>
namespace pulsebeam::webrtc_sys {
struct NativeFecOverride::State final : webrtc::FecControllerOverride {
  explicit State(webrtc::FecControllerOverride* target) : target(target) {}
  explicit State(rust::Box<RustFecOverride> value) : provider(std::make_unique<rust::Box<RustFecOverride>>(std::move(value))) {}
  bool Invoke(bool allowed) {
    { std::lock_guard lock(mutex); if (!active) return false; ++in_flight; }
    struct Exit { State& owner; ~Exit() { std::lock_guard lock(owner.mutex); if (--owner.in_flight == 0) owner.idle.notify_all(); } } exit{*this};
    if (provider) fec_override_provider_allowed(**provider, allowed);
    else target->SetFecAllowed(allowed);
    return true;
  }
  void SetFecAllowed(bool allowed) override { Invoke(allowed); }
  void Retire() { std::unique_lock lock(mutex); active = false; idle.wait(lock, [&] { return in_flight == 0; }); target = nullptr; }
  std::mutex mutex;
  std::condition_variable idle;
  bool active = true;
  std::size_t in_flight = 0;
  webrtc::FecControllerOverride* target = nullptr;
  std::unique_ptr<rust::Box<RustFecOverride>> provider;
};
webrtc::FecControllerOverride* NativeFecOverride::callback() const noexcept { return state_.get(); }
std::shared_ptr<NativeFecOverride> BorrowFecOverride(webrtc::FecControllerOverride* target) { return target ? std::make_shared<NativeFecOverride>(std::make_shared<NativeFecOverride::State>(target)) : nullptr; }
void RetireFecOverride(const NativeFecOverride& value) { value.state()->Retire(); }
std::shared_ptr<NativeFecOverride> fec_override_new(rust::Box<RustFecOverride> provider) noexcept { return std::make_shared<NativeFecOverride>(std::make_shared<NativeFecOverride::State>(std::move(provider))); }
bool fec_override_same(const NativeFecOverride& a, const NativeFecOverride& b) noexcept { return a.state() == b.state(); }
bool fec_override_allowed(const NativeFecOverride& value, bool allowed) noexcept { return value.state()->Invoke(allowed); }
bool fec_override_retired(const NativeFecOverride& value) noexcept { std::lock_guard lock(value.state()->mutex); return !value.state()->active; }
struct NativeEncodedImageSink::State final : webrtc::EncodedImageCallback {
  explicit State(rust::Box<RustEncodedImageSink> value) : provider(std::move(value)) {}
  Result OnEncodedImage(const webrtc::EncodedImage& image, const webrtc::CodecSpecificInfo* info) override {
    // Own immutable bytes even if a caller's native producer uses custom release
    // storage. Only the actual logical prefix is published, never spare bytes.
    const auto data = image.GetEncodedData();
    if (image.size() && (!data || !image.data() || image.size() > data->size())) return Result(Result::ERROR_SEND_FAILED);
    webrtc::EncodedImage copy(image);
    if (data) copy.SetEncodedData(webrtc::EncodedImageBuffer::Create(image.data(), image.size()));
    const auto value = encoded_sink_provider_frame(*provider, std::make_unique<NativeEncodedImage>(copy),
        info ? std::make_unique<NativeCodecSpecificInfo>(*info) : nullptr);
    if (value.error < 0 || value.error > 1) return Result(Result::ERROR_SEND_FAILED);
    Result result(static_cast<Result::Error>(value.error), value.frame_id);
    result.drop_next_frame = value.drop_next_frame;
    return result;
  }
  void OnFrameDropped(std::uint32_t timestamp, int spatial_id, bool end) override { encoded_sink_provider_dropped(*provider, timestamp, spatial_id, end); }
  rust::Box<RustEncodedImageSink> provider;
};
webrtc::EncodedImageCallback* NativeEncodedImageSink::callback() const noexcept { return state_.get(); }
std::unique_ptr<NativeEncodedImageSink> encoded_sink_new(rust::Box<RustEncodedImageSink> provider) noexcept { return std::make_unique<NativeEncodedImageSink>(std::make_shared<NativeEncodedImageSink::State>(std::move(provider))); }
std::unique_ptr<NativeEncodedImageSink> encoded_sink_clone(const NativeEncodedImageSink& source) noexcept { return std::make_unique<NativeEncodedImageSink>(source.state()); }
bool encoded_sink_same(const NativeEncodedImageSink& a, const NativeEncodedImageSink& b) noexcept { return a.state() == b.state(); }
FfiEncodedCallbackResult encoded_sink_frame(const NativeEncodedImageSink& sink, const NativeEncodedImage& image, const NativeCodecSpecificInfo* info) noexcept {
  const auto value = sink.callback()->OnEncodedImage(image.value(), info ? &info->value() : nullptr);
  return {true, static_cast<std::int32_t>(value.error), value.frame_id, value.drop_next_frame};
}
void encoded_sink_dropped(const NativeEncodedImageSink& sink, std::uint32_t timestamp, std::int32_t spatial_id, bool end) noexcept { sink.callback()->OnFrameDropped(timestamp, spatial_id, end); }
struct NativeDecodedImageSink::State final : webrtc::DecodedImageCallback {
  explicit State(rust::Box<RustDecodedImageSink> provider) : provider(std::move(provider)) {}
  std::int32_t Invoke(webrtc::VideoFrame& frame, std::uint8_t overload, std::int64_t time,
      std::optional<std::int32_t> optional_time, std::optional<std::uint8_t> qp) {
    std::int32_t status = 0;
    auto returned = decoded_sink_provider_invoke(*provider, std::make_unique<NativeRawVideoFrame>(frame), overload,
        time, optional_time.has_value(), optional_time.value_or(0), qp.has_value(), qp.value_or(0), status);
    if (returned) frame = returned->value();
    return status;
  }
  std::int32_t Decoded(webrtc::VideoFrame& frame) override { return Invoke(frame, 0, 0, std::nullopt, std::nullopt); }
  std::int32_t Decoded(webrtc::VideoFrame& frame, std::int64_t time) override { return Invoke(frame, 1, time, std::nullopt, std::nullopt); }
  void Decoded(webrtc::VideoFrame& frame, std::optional<std::int32_t> time, std::optional<std::uint8_t> qp) override { Invoke(frame, 2, 0, time, qp); }
  rust::Box<RustDecodedImageSink> provider;
};
webrtc::DecodedImageCallback* NativeDecodedImageSink::callback() const noexcept { return state_.get(); }
std::unique_ptr<NativeDecodedImageSink> decoded_sink_new(rust::Box<RustDecodedImageSink> provider) noexcept { return std::make_unique<NativeDecodedImageSink>(std::make_shared<NativeDecodedImageSink::State>(std::move(provider))); }
std::unique_ptr<NativeDecodedImageSink> decoded_sink_clone(const NativeDecodedImageSink& source) noexcept { return std::make_unique<NativeDecodedImageSink>(source.state()); }
bool decoded_sink_same(const NativeDecodedImageSink& a, const NativeDecodedImageSink& b) noexcept { return a.state() == b.state(); }
std::int32_t decoded_sink_frame(const NativeDecodedImageSink& sink, NativeRawVideoFrame& frame, std::uint8_t overload,
    std::int64_t time, bool has_time, std::int32_t optional_time, bool has_qp, std::uint8_t qp) noexcept {
  auto* target = sink.callback();
  if (overload == 0) return target->Decoded(frame.value());
  if (overload == 1) return target->Decoded(frame.value(), time);
  target->Decoded(frame.value(), has_time ? std::optional<std::int32_t>(optional_time) : std::nullopt, has_qp ? std::optional<std::uint8_t>(qp) : std::nullopt);
  return 0;  // Void overload, not an SDK status.
}
}  // namespace pulsebeam::webrtc_sys
