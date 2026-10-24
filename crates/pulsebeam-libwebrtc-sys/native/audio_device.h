#pragma once
#include <cstdint>
#include <memory>
#include "rust/cxx.h"
#include "api/audio/audio_device.h"
#include "api/audio/audio_device_defines.h"
namespace webrtc { class Thread; }
namespace pulsebeam::webrtc_sys {
class NativeEnvironment;
class NativePeerConnectionFactory;
class NativeThread;
struct FfiAudioDeviceResult;
struct FfiAudioDeviceName;
struct FfiAudioDeviceStatistics;
struct FfiAudioDevicePlatformState;
struct RustAudioDeviceCallbacks;
struct FfiAudioDevicePcm;
class NativeAudioTransport final : public webrtc::AudioTransport {
 public:
  explicit NativeAudioTransport(rust::Box<RustAudioDeviceCallbacks>) noexcept;
  ~NativeAudioTransport() override;
  std::int32_t RecordedDataIsAvailable(const void*, std::size_t, std::size_t, std::size_t, std::uint32_t, std::uint32_t, std::int32_t, std::uint32_t, bool, std::uint32_t&) override;
  std::int32_t RecordedDataIsAvailable(const void*, std::size_t, std::size_t, std::size_t, std::uint32_t, std::uint32_t, std::int32_t, std::uint32_t, bool, std::uint32_t&, std::optional<std::int64_t>) override;
  std::int32_t NeedMorePlayData(std::size_t, std::size_t, std::size_t, std::uint32_t, void*, std::size_t&, std::int64_t*, std::int64_t*) override;
  void PullRenderData(int, int, std::size_t, std::size_t, void*, std::int64_t*, std::int64_t*) override;
 private:
  std::int32_t Render(FfiAudioDevicePcm, bool, void*, std::size_t*, std::int64_t*, std::int64_t*);
  rust::Box<RustAudioDeviceCallbacks> callbacks_;
};
class NativeAudioDeviceObserver final : public webrtc::AudioDeviceObserver {
 public:
  explicit NativeAudioDeviceObserver(rust::Box<RustAudioDeviceCallbacks>) noexcept;
  ~NativeAudioDeviceObserver() override;
  void OnDevicesUpdated() override;
  void OnSpeechActivityEvent(webrtc::AudioDeviceModule::SpeechActivityEvent) override;
 private:
  rust::Box<RustAudioDeviceCallbacks> callbacks_;
};
std::unique_ptr<NativeAudioTransport> audio_transport_new(rust::Box<RustAudioDeviceCallbacks>) noexcept;
std::unique_ptr<NativeAudioDeviceObserver> audio_device_observer_new(rust::Box<RustAudioDeviceCallbacks>) noexcept;
class NativeAudioDevice final {
 public:
  struct State;
  explicit NativeAudioDevice(std::unique_ptr<State>) noexcept;
  ~NativeAudioDevice();
  State& state() noexcept;
  webrtc::Thread* worker() const noexcept;
  webrtc::scoped_refptr<webrtc::AudioDeviceModule> module() const noexcept;
  void installed() noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeAudioDevice> audio_device_from_factory(const NativePeerConnectionFactory&) noexcept;
std::unique_ptr<NativeAudioDevice> audio_device_new(const NativeEnvironment&, const NativeThread&, std::uint8_t, bool, bool, std::uint8_t&) noexcept;
std::int32_t audio_device_register_transport(NativeAudioDevice&, std::unique_ptr<NativeAudioTransport>, std::uint8_t&) noexcept;
std::int32_t audio_device_register_observer(NativeAudioDevice&, std::unique_ptr<NativeAudioDeviceObserver>) noexcept;
FfiAudioDeviceResult audio_device_call(NativeAudioDevice&, std::uint8_t, std::uint32_t) noexcept;
FfiAudioDeviceName audio_device_name(NativeAudioDevice&, bool, std::uint16_t) noexcept;
FfiAudioDeviceStatistics audio_device_statistics(NativeAudioDevice&) noexcept;
FfiAudioDevicePlatformState audio_device_platform_state(NativeAudioDevice&) noexcept;
}
