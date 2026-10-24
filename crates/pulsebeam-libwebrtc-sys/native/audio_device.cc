#include "pulsebeam-webrtc-sys/native/audio_device.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/peer.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <algorithm>
#include <iterator>
#include <cstring>
#include <limits>
#include <vector>
#include "api/audio/audio_device.h"
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO)
#include "api/audio/create_audio_device_module.h"
#endif
#include "rtc_base/thread.h"
namespace pulsebeam::webrtc_sys {

namespace {
bool Extent(std::size_t frames, std::size_t bytes_per_frame, std::size_t& size) {
  if (bytes_per_frame && frames > std::numeric_limits<std::size_t>::max() / bytes_per_frame) return false;
  size = frames * bytes_per_frame;
  return size <= static_cast<std::size_t>(std::numeric_limits<std::ptrdiff_t>::max());
}
FfiAudioDevicePcm Pcm(std::size_t frames, std::size_t bytes_per_frame, std::size_t channels, std::int64_t rate) {
  FfiAudioDevicePcm f{}; f.frames = frames; f.bytes_per_frame = bytes_per_frame; f.channels = channels; f.rate = rate;
  if (channels && bytes_per_frame % channels == 0 && bytes_per_frame / channels <= static_cast<std::size_t>(INT32_MAX / 8))
    f.bits = static_cast<std::int32_t>(8 * (bytes_per_frame / channels));
  return f;
}
}
NativeAudioTransport::NativeAudioTransport(rust::Box<RustAudioDeviceCallbacks> p) noexcept : callbacks_(std::move(p)) {}
NativeAudioTransport::~NativeAudioTransport() { audio_device_callbacks_dispose(*callbacks_); }
std::int32_t NativeAudioTransport::RecordedDataIsAvailable(const void* data, std::size_t frames, std::size_t bpf, std::size_t channels, std::uint32_t rate, std::uint32_t delay, std::int32_t drift, std::uint32_t mic, bool key, std::uint32_t& new_mic) {
  return RecordedDataIsAvailable(data, frames, bpf, channels, rate, delay, drift, mic, key, new_mic, std::nullopt);
}
std::int32_t NativeAudioTransport::RecordedDataIsAvailable(const void* data, std::size_t frames, std::size_t bpf, std::size_t channels, std::uint32_t rate, std::uint32_t delay, std::int32_t drift, std::uint32_t mic, bool key, std::uint32_t& new_mic, std::optional<std::int64_t> timestamp) {
  new_mic = 0;
  std::size_t size = 0;
  if (!Extent(frames, bpf, size) || (size && !data)) { audio_device_callbacks_invalid(*callbacks_, 0); return -1; }
  auto f = Pcm(frames, bpf, channels, rate); f.delay_ms = delay; f.clock_drift = drift; f.mic_level = mic; f.key_pressed = key;
  f.capture_time_present = timestamp.has_value(); f.capture_time_ns = timestamp.value_or(0);
  const std::uint8_t empty = 0;
  const auto r = audio_device_recorded(*callbacks_, std::move(f), rust::Slice<const std::uint8_t>(size ? static_cast<const std::uint8_t*>(data) : &empty, size));
  if (r.failed) return -1; new_mic = r.mic_level; return r.status;
}
std::int32_t NativeAudioTransport::Render(FfiAudioDevicePcm f, bool pull, void* output, std::size_t* frames_out, std::int64_t* elapsed, std::int64_t* ntp) {
  if (frames_out) *frames_out = 0; if (elapsed) *elapsed = -1; if (ntp) *ntp = -1;
  std::size_t size = 0;
  if (!Extent(f.frames, f.bytes_per_frame, size) || (size && !output)) { audio_device_callbacks_invalid(*callbacks_, pull ? 2 : 1); return -1; }
  // The native transport contract requires initialized output even on failure.
  // Never expose native output as a mutable Rust borrow or publish partial
  // custom mutations on wrapper failure. A void pull failure is observer-only.
  if (size) std::memset(output, 0, size);
  std::vector<std::uint8_t> work(size, 0); std::uint8_t empty = 0;
  const auto r = audio_device_render(*callbacks_, std::move(f), pull, rust::Slice<std::uint8_t>(size ? work.data() : &empty, size));
  if (r.failed) return -1;
  if (size) std::memcpy(output, work.data(), size);
  if (frames_out) *frames_out = r.frames; if (elapsed) *elapsed = r.elapsed_ms; if (ntp) *ntp = r.ntp_ms;
  return r.status;
}
std::int32_t NativeAudioTransport::NeedMorePlayData(std::size_t frames, std::size_t bpf, std::size_t channels, std::uint32_t rate, void* output, std::size_t& frames_out, std::int64_t* elapsed, std::int64_t* ntp) {
  return Render(Pcm(frames, bpf, channels, rate), false, output, &frames_out, elapsed, ntp);
}
void NativeAudioTransport::PullRenderData(int bits, int rate, std::size_t channels, std::size_t frames, void* output, std::int64_t* elapsed, std::int64_t* ntp) {
  if (elapsed) *elapsed = -1; if (ntp) *ntp = -1;
  if (bits <= 0 || bits % 8 || channels > std::numeric_limits<std::size_t>::max() / static_cast<std::size_t>(bits / 8)) { audio_device_callbacks_invalid(*callbacks_, 2); return; }
  auto f = Pcm(frames, channels * static_cast<std::size_t>(bits / 8), channels, rate); f.bits = bits;
  Render(std::move(f), true, output, nullptr, elapsed, ntp);
}
NativeAudioDeviceObserver::NativeAudioDeviceObserver(rust::Box<RustAudioDeviceCallbacks> p) noexcept : callbacks_(std::move(p)) {}
NativeAudioDeviceObserver::~NativeAudioDeviceObserver() { audio_device_callbacks_dispose(*callbacks_); }
void NativeAudioDeviceObserver::OnDevicesUpdated() { audio_device_observed(*callbacks_, false, 0); }
void NativeAudioDeviceObserver::OnSpeechActivityEvent(webrtc::AudioDeviceModule::SpeechActivityEvent event) { audio_device_observed(*callbacks_, true, static_cast<std::int32_t>(event)); }
std::unique_ptr<NativeAudioTransport> audio_transport_new(rust::Box<RustAudioDeviceCallbacks> p) noexcept { return std::make_unique<NativeAudioTransport>(std::move(p)); }
std::unique_ptr<NativeAudioDeviceObserver> audio_device_observer_new(rust::Box<RustAudioDeviceCallbacks> p) noexcept { return std::make_unique<NativeAudioDeviceObserver>(std::move(p)); }

struct NativeAudioDevice::State {
  webrtc::Thread* worker = nullptr;
  bool owns_module = true;
  webrtc::scoped_refptr<webrtc::AudioDeviceModule> module;
  // Retire accepted raw-pointer registrations only after final ADM release,
  // including when a backend cannot synchronously unregister an observer.
  std::vector<std::unique_ptr<NativeAudioTransport>> transports;
  std::vector<std::unique_ptr<NativeAudioDeviceObserver>> observers;
};
NativeAudioDevice::NativeAudioDevice(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeAudioDevice::~NativeAudioDevice() {
  // Rust drops this owner before the explicitly consumed worker/environment.
  state_->worker->BlockingCall([&] { if (state_->owns_module) { state_->module->StopRecording(); state_->module->StopPlayout(); state_->module->RegisterAudioCallback(nullptr); state_->module->SetObserver(nullptr); state_->module->Terminate(); } state_->module = nullptr; state_->transports.clear(); state_->observers.clear(); });
}
NativeAudioDevice::State& NativeAudioDevice::state() noexcept { return *state_; }
webrtc::Thread* NativeAudioDevice::worker() const noexcept { return state_->worker; }
webrtc::scoped_refptr<webrtc::AudioDeviceModule> NativeAudioDevice::module() const noexcept { return state_->module; }
void NativeAudioDevice::installed() noexcept { state_->owns_module = false; }
std::unique_ptr<NativeAudioDevice> audio_device_from_factory(const NativePeerConnectionFactory& factory) noexcept {
  auto module = factory.audio_device(); if (!module) return nullptr;
  auto state = std::make_unique<NativeAudioDevice::State>(); state->module = std::move(module); state->worker = factory.worker_thread(); state->owns_module = false;
  return std::make_unique<NativeAudioDevice>(std::move(state));
}
std::unique_ptr<NativeAudioDevice> audio_device_new(const NativeEnvironment& environment, const NativeThread& worker, std::uint8_t layer, bool bypass, bool helper, std::uint8_t& error) noexcept {
  error = 0;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO)
  // This pin declares Module::Create but supplies no definition. Do not turn
  // that missing entry point into a null native result or a helper substitute.
  if (!helper) { error = 2; return nullptr; }
  auto state = std::make_unique<NativeAudioDevice::State>(); state->worker = worker.thread();
  state->worker->BlockingCall([&] {
    const auto selected = static_cast<webrtc::AudioDeviceModule::AudioLayer>(layer);
    state->module = webrtc::CreateAudioDeviceModule(environment.environment(), selected, bypass);
  });
  if (!state->module) return nullptr;
  return std::make_unique<NativeAudioDevice>(std::move(state));
#else
  error = 1; return nullptr;
#endif
}
std::int32_t audio_device_register_transport(NativeAudioDevice& device, std::unique_ptr<NativeAudioTransport> transport, std::uint8_t& error) noexcept {
  error = 0; std::int32_t result = -1;
  device.state().worker->BlockingCall([&] {
    auto& module = *device.state().module;
    // AudioDeviceBuffer assigns a non-atomic raw transport pointer. Its warning
    // about active media is not synchronization against the backend callback.
    if (module.Initialized() && (module.Playing() || module.Recording())) { error = 1; return; }
    result = module.RegisterAudioCallback(transport.get());
    if (!result && transport) device.state().transports.push_back(std::move(transport));
  });
  return result;
}
std::int32_t audio_device_register_observer(NativeAudioDevice& device, std::unique_ptr<NativeAudioDeviceObserver> observer) noexcept {
  std::int32_t result = -1;
  device.state().worker->BlockingCall([&] { result = device.state().module->SetObserver(observer.get()); if (!result && observer) device.state().observers.push_back(std::move(observer)); });
  return result;
}
FfiAudioDeviceResult audio_device_call(NativeAudioDevice& device, std::uint8_t operation, std::uint32_t argument) noexcept {
  FfiAudioDeviceResult r{};
  device.state().worker->BlockingCall([&] {
    auto& adm = *device.state().module;
    switch (operation) {
      case 0: { r.status = adm.Init(); break; }
      case 1: { r.status = adm.Terminate(); break; }
      case 2: { r.bool_value = adm.Initialized(); break; }
      case 3: { r.status = adm.InitPlayout(); break; }
      case 4: { r.bool_value = adm.PlayoutIsInitialized(); break; }
      case 5: { r.status = adm.InitRecording(); break; }
      case 6: { r.bool_value = adm.RecordingIsInitialized(); break; }
      case 7: { r.status = adm.StartPlayout(); break; }
      case 8: { r.status = adm.StopPlayout(); break; }
      case 9: { r.bool_value = adm.Playing(); break; }
      case 10: { r.status = adm.StartRecording(); break; }
      case 11: { r.status = adm.StopRecording(); break; }
      case 12: { r.bool_value = adm.Recording(); break; }
      case 13: { r.status = adm.InitSpeaker(); break; }
      case 14: { r.bool_value = adm.SpeakerIsInitialized(); break; }
      case 15: { r.status = adm.InitMicrophone(); break; }
      case 16: { r.bool_value = adm.MicrophoneIsInitialized(); break; }
      case 17: { r.status = adm.PlayoutIsAvailable(&r.bool_value); break; }
      case 18: { r.status = adm.RecordingIsAvailable(&r.bool_value); break; }
      case 19: { r.status = adm.SpeakerVolumeIsAvailable(&r.bool_value); break; }
      case 20: { r.status = adm.SetSpeakerVolume(argument); break; }
      case 21: { r.status = adm.SpeakerVolume(&r.int_value); break; }
      case 22: { r.status = adm.MaxSpeakerVolume(&r.int_value); break; }
      case 23: { r.status = adm.MinSpeakerVolume(&r.int_value); break; }
      case 24: { r.status = adm.MicrophoneVolumeIsAvailable(&r.bool_value); break; }
      case 25: { r.status = adm.SetMicrophoneVolume(argument); break; }
      case 26: { r.status = adm.MicrophoneVolume(&r.int_value); break; }
      case 27: { r.status = adm.MaxMicrophoneVolume(&r.int_value); break; }
      case 28: { r.status = adm.MinMicrophoneVolume(&r.int_value); break; }
      case 29: { r.status = adm.SpeakerMuteIsAvailable(&r.bool_value); break; }
      case 30: { r.status = adm.SetSpeakerMute(argument != 0); break; }
      case 31: { r.status = adm.SpeakerMute(&r.bool_value); break; }
      case 32: { r.status = adm.MicrophoneMuteIsAvailable(&r.bool_value); break; }
      case 33: { r.status = adm.SetMicrophoneMute(argument != 0); break; }
      case 34: { r.status = adm.MicrophoneMute(&r.bool_value); break; }
      case 35: { r.status = adm.StereoPlayoutIsAvailable(&r.bool_value); break; }
      case 36: { r.status = adm.SetStereoPlayout(argument != 0); break; }
      case 37: { r.status = adm.StereoPlayout(&r.bool_value); break; }
      case 38: { r.status = adm.StereoRecordingIsAvailable(&r.bool_value); break; }
      case 39: { r.status = adm.SetStereoRecording(argument != 0); break; }
      case 40: { r.status = adm.StereoRecording(&r.bool_value); break; }
      case 41: { r.bool_value = adm.BuiltInAECIsAvailable(); break; }
      case 42: { r.bool_value = adm.BuiltInAGCIsAvailable(); break; }
      case 43: { r.bool_value = adm.BuiltInNSIsAvailable(); break; }
      case 44: { r.status = adm.EnableBuiltInAEC(argument != 0); break; }
      case 45: { r.status = adm.EnableBuiltInAGC(argument != 0); break; }
      case 46: { r.status = adm.EnableBuiltInNS(argument != 0); break; }
      case 47: { r.bool_value = adm.PlatformVoiceProcessingPathIsAvailable(); break; }
      case 48: { r.status = adm.EnablePlatformVoiceProcessingPath(argument != 0); break; }
      case 49: { r.bool_value = adm.IsStopOnMuteModeEnabled(); break; }
      case 50: { r.status = adm.SetPlayoutDevice(static_cast<std::uint16_t>(argument)); break; }
      case 51: { r.status = adm.SetRecordingDevice(static_cast<std::uint16_t>(argument)); break; }
      case 52: { const auto count = adm.PlayoutDevices(); r.status = count < 0 ? count : 0; if (count >= 0) r.int_value = count; break; }
      case 53: { const auto count = adm.RecordingDevices(); r.status = count < 0 ? count : 0; if (count >= 0) r.int_value = count; break; }
      case 54: { r.int_value = static_cast<std::uint32_t>(adm.GetPlayoutUnderrunCount()); break; }
      case 55: { r.int_value = static_cast<std::uint32_t>(adm.GetPlayoutDevice()); break; }
      case 56: { r.int_value = static_cast<std::uint32_t>(adm.GetRecordingDevice()); break; }
      case 57: { std::uint16_t value = 0; r.status = adm.PlayoutDelay(&value); r.int_value = value; break; }
      case 58: { auto value = webrtc::AudioDeviceModule::kPlatformDefaultAudio; r.status = adm.ActiveAudioLayer(&value); r.int_value = static_cast<std::uint32_t>(value); break; }
      case 59: { r.int_value = static_cast<std::uint32_t>(adm.GetPlatformAudioProcessingTopology()); break; }
      default: r.status = -1; break;
    }
  }); return r;
}
FfiAudioDeviceName audio_device_name(NativeAudioDevice& device, bool recording, std::uint16_t index) noexcept {
  FfiAudioDeviceName r{};
  device.state().worker->BlockingCall([&] {
    char name[webrtc::kAdmMaxDeviceNameSize] = {}, guid[webrtc::kAdmMaxGuidSize] = {};
    r.status = recording ? device.state().module->RecordingDeviceName(index, name, guid) : device.state().module->PlayoutDeviceName(index, name, guid);
    if (r.status) return;
    for (const char* p = name; p != std::end(name) && *p; ++p) r.name.push_back(static_cast<std::uint8_t>(*p));
    for (const char* p = guid; p != std::end(guid) && *p; ++p) r.guid.push_back(static_cast<std::uint8_t>(*p));
  }); return r;
}
FfiAudioDeviceStatistics audio_device_statistics(NativeAudioDevice& device) noexcept {
  FfiAudioDeviceStatistics f{};
  device.state().worker->BlockingCall([&] {
    const auto value = device.state().module->GetStats(); f.present = value.has_value();
    if (!value) return;
    f.synthesized_samples_duration_s = value->synthesized_samples_duration_s;
    f.synthesized_samples_events = value->synthesized_samples_events;
    f.total_samples_duration_s = value->total_samples_duration_s;
    f.total_playout_delay_s = value->total_playout_delay_s;
    f.total_samples_count = value->total_samples_count;
    f.dropped_samples_duration_s = value->dropped_samples_duration_s;
    f.dropped_samples_events = value->dropped_samples_events;
    f.total_capture_samples_duration_s = value->total_capture_samples_duration_s;
    f.total_capture_delay_s = value->total_capture_delay_s;
    f.total_capture_samples_count = value->total_capture_samples_count;
  }); return f;
}
FfiAudioDevicePlatformState audio_device_platform_state(NativeAudioDevice& device) noexcept {
  FfiAudioDevicePlatformState f{};
  device.state().worker->BlockingCall([&] {
    const auto value = device.state().module->GetPlatformAudioProcessingState();
    const auto optional = [](std::optional<bool> v) -> std::uint8_t { return v ? (*v ? 2 : 1) : 0; };
    f.topology = static_cast<std::int32_t>(value.topology);
    f.echo_cancellation_available = value.is_echo_cancellation_available;
    f.noise_suppression_available = value.is_noise_suppression_available;
    f.auto_gain_control_available = value.is_auto_gain_control_available;
    f.echo_cancellation_requested = optional(value.is_echo_cancellation_requested);
    f.noise_suppression_requested = optional(value.is_noise_suppression_requested);
    f.auto_gain_control_requested = optional(value.is_auto_gain_control_requested);
    f.echo_cancellation_active = optional(value.is_echo_cancellation_active);
    f.noise_suppression_active = optional(value.is_noise_suppression_active);
    f.auto_gain_control_active = optional(value.is_auto_gain_control_active);
    f.voice_processing_enabled_requested = optional(value.is_voice_processing_enabled_requested);
    f.voice_processing_bypassed_requested = optional(value.is_voice_processing_bypassed_requested);
    f.voice_processing_agc_enabled_requested = optional(value.is_voice_processing_agc_enabled_requested);
    f.voice_processing_enabled_active = optional(value.is_voice_processing_enabled_active);
    f.voice_processing_bypassed_active = optional(value.is_voice_processing_bypassed_active);
    f.voice_processing_agc_enabled_active = optional(value.is_voice_processing_agc_enabled_active);
  }); return f;
}
}
