#include "pulsebeam-webrtc-sys/native/audio_processor.h"
#include "pulsebeam-webrtc-sys/native/audio_echo_config.h"
#include <algorithm>
#include <array>
#include <atomic>
#include <mutex>
#include <span>
#include <cerrno>
#include <cctype>
#include <cmath>
#include <cstdlib>
#include <cstdio>
#include <optional>
#if defined(WEBRTC_WIN)
#include <io.h>
#else
#include <fcntl.h>
#include <unistd.h>
#endif
#include <limits>
#include <string>
#include "api/audio/echo_canceller3_config.h"
#include "api/audio/echo_canceller3_factory.h"
#include "rtc_base/experiments/field_trial_parser.h"
#include <utility>
#include <vector>
#include "api/audio/builtin_audio_processing_builder.h"
#include "modules/audio_processing/include/aec_dump.h"
#include "modules/audio_processing/audio_buffer.h"
#include "modules/audio_processing/aec3/block.h"
#include "api/environment/environment.h"
#include "api/make_ref_counted.h"
#include "common_audio/include/audio_util.h"
#include "rtc_base/checks.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
// Pinned internal preflight declaration from aec3/echo_canceller3.h. Importing
// the full internal class header would require its non-exported debug-dump
// layout definitions; only this free function's public value types cross here.
namespace webrtc {
EchoCanceller3Config AdjustConfig(const EchoCanceller3Config&, const FieldTrialsView&);
}
namespace pulsebeam::webrtc_sys {
namespace {

// Caller callbacks are void in the SDK. Retain a separate operation-local
// failure scope without inventing native APM status codes.
webrtc::AudioProcessing::RuntimeSetting RuntimeFromFfi(const FfiApmRuntimeSetting&);
FfiApmConfig ConfigToFfi(const webrtc::AudioProcessing::Config&);
webrtc::AudioProcessing::Config ConfigFromFfi(const FfiApmConfig&);
class CallbackOperation;
CallbackOperation DumpGeneration(const NativeAudioProcessingModule&);
std::uint8_t DumpOutcome(const NativeAudioProcessingModule&, const CallbackOperation&);
bool RuntimeReinitializationUnsafe(const NativeAudioProcessingModule&);
void NoteCaptureResult(NativeAudioProcessingModule&, int, std::uint8_t, const CallbackOperation&);
bool SafeAecTrials(const webrtc::FieldTrialsView&);
bool SafeEchoConfig(const webrtc::EchoCanceller3Config&, const webrtc::FieldTrialsView&);
struct DumpCallbackStatus {
  std::atomic<std::uint64_t> failures{0};
  void Reject() noexcept;
};
// Synchronous native callbacks report to the nearest matching owning operation.
// Global observer totals remain diagnostics, never operation identity. Nested
// operations can handle their own failures without poisoning their caller, and
// another thread cannot change this operation's result.
class CallbackOperation {
 public:
  explicit CallbackOperation(std::vector<std::shared_ptr<DumpCallbackStatus>> statuses, bool failed = false)
      : statuses_(std::move(statuses)), failed_(failed) { active_.push_back(this); }
  ~CallbackOperation() { active_.pop_back(); }
  CallbackOperation(const CallbackOperation&) = delete;
  CallbackOperation& operator=(const CallbackOperation&) = delete;
  bool failed() const noexcept { return failed_; }
  const auto& metric_results() const noexcept { return metric_results_; }
  const auto& construction_results() const noexcept { return construction_results_; }
  static void Construction(const std::shared_ptr<DumpCallbackStatus>& status, bool success) {
    for (auto scope = active_.rbegin(); scope != active_.rend(); ++scope)
      for (const auto& selected : (*scope)->statuses_)
        if (selected == status) {
          auto& results = (*scope)->construction_results_;
          for (auto& result : results) if (result.first == status) { result.second = success; return; }
          results.emplace_back(status, success); return;
        }
  }
  static void Metric(const std::shared_ptr<DumpCallbackStatus>& status, bool success) {
    for (auto scope = active_.rbegin(); scope != active_.rend(); ++scope)
      for (const auto& selected : (*scope)->statuses_)
        if (selected == status) {
          auto& results = (*scope)->metric_results_;
          for (auto& result : results) if (result.first == status) { result.second = success; return; }
          results.emplace_back(status, success); return;
        }
  }
  static void Reject(const DumpCallbackStatus* status) noexcept {
    for (auto scope = active_.rbegin(); scope != active_.rend(); ++scope)
      for (const auto& selected : (*scope)->statuses_)
        if (selected.get() == status) { (*scope)->failed_ = true; return; }
  }
 private:
  std::vector<std::shared_ptr<DumpCallbackStatus>> statuses_;
  std::vector<std::pair<std::shared_ptr<DumpCallbackStatus>, bool>> metric_results_;
  std::vector<std::pair<std::shared_ptr<DumpCallbackStatus>, bool>> construction_results_;
  bool failed_ = false;
  inline static thread_local std::vector<CallbackOperation*> active_;
};
void DumpCallbackStatus::Reject() noexcept {
  failures.fetch_add(1, std::memory_order_release);
  CallbackOperation::Reject(this);
}
rust::Vec<std::uint8_t> DumpBytes(const std::string& value) {
  rust::Vec<std::uint8_t> result;
  result.reserve(value.size());
  for (unsigned char byte : value) result.push_back(byte);
  return result;
}
FfiApmDumpConfig DumpConfigToFfi(const webrtc::InternalAPMConfig& c) {
  FfiApmDumpConfig f{};
  f.aec_enabled = c.aec_enabled;
  f.aec_delay_agnostic_enabled = c.aec_delay_agnostic_enabled;
  f.aec_drift_compensation_enabled = c.aec_drift_compensation_enabled;
  f.aec_extended_filter_enabled = c.aec_extended_filter_enabled;
  f.aec_suppression_level = c.aec_suppression_level;
  f.agc_enabled = c.agc_enabled;
  f.agc_mode = c.agc_mode;
  f.agc_limiter_enabled = c.agc_limiter_enabled;
  f.hpf_enabled = c.hpf_enabled;
  f.ns_enabled = c.ns_enabled;
  f.ns_level = c.ns_level;
  f.transient_suppression_enabled = c.transient_suppression_enabled;
  f.noise_robust_agc_enabled = c.noise_robust_agc_enabled;
  f.pre_amplifier_enabled = c.pre_amplifier_enabled;
  f.pre_amplifier_fixed_gain_factor = c.pre_amplifier_fixed_gain_factor;
  f.experiments_description = DumpBytes(c.experiments_description);
  f.api_config_string = DumpBytes(c.api_config_string);
  return f;
}
webrtc::InternalAPMConfig DumpConfigFromFfi(const FfiApmDumpConfig& f) {
  webrtc::InternalAPMConfig c;
  c.aec_enabled = f.aec_enabled;
  c.aec_delay_agnostic_enabled = f.aec_delay_agnostic_enabled;
  c.aec_drift_compensation_enabled = f.aec_drift_compensation_enabled;
  c.aec_extended_filter_enabled = f.aec_extended_filter_enabled;
  c.aec_suppression_level = f.aec_suppression_level;
  c.agc_enabled = f.agc_enabled;
  c.agc_mode = f.agc_mode;
  c.agc_limiter_enabled = f.agc_limiter_enabled;
  c.hpf_enabled = f.hpf_enabled;
  c.ns_enabled = f.ns_enabled;
  c.ns_level = f.ns_level;
  c.transient_suppression_enabled = f.transient_suppression_enabled;
  c.noise_robust_agc_enabled = f.noise_robust_agc_enabled;
  c.pre_amplifier_enabled = f.pre_amplifier_enabled;
  c.pre_amplifier_fixed_gain_factor = f.pre_amplifier_fixed_gain_factor;
  c.experiments_description.assign(reinterpret_cast<const char*>(f.experiments_description.data()), f.experiments_description.size());
  c.api_config_string.assign(reinterpret_cast<const char*>(f.api_config_string.data()), f.api_config_string.size());
  return c;
}
FfiApmRuntimeSetting RuntimeSnapshot(const webrtc::AudioProcessing::RuntimeSetting& s) {
    FfiApmRuntimeSetting f{};
    using T = webrtc::AudioProcessing::RuntimeSetting::Type;
    switch (s.type()) {
      case T::kNotSpecified: break;
      case T::kCapturePreGain: f.kind = 1; s.GetFloat(&f.float_value); break;
      case T::kCapturePostGain: f.kind = 2; s.GetFloat(&f.float_value); break;
      case T::kCaptureCompressionGain: {
        // The SDK integer factory stores its admitted 0..90 payload as float.
        float value; s.GetFloat(&value); f.kind = 3; f.int_value = static_cast<std::int32_t>(value); break;
      }
      case T::kCaptureFixedPostGain: f.kind = 4; s.GetFloat(&f.float_value); break;
      case T::kPlayoutVolumeChange: f.kind = 5; s.GetInt(&f.int_value); break;
      case T::kCustomRenderProcessingRuntimeSetting: f.kind = 6; s.GetFloat(&f.float_value); break;
      case T::kPlayoutAudioDeviceChange: {
        webrtc::AudioProcessing::RuntimeSetting::PlayoutAudioDeviceInfo device;
        s.GetPlayoutAudioDeviceInfo(&device); f.kind = 7;
        f.device_id = device.id; f.device_max_volume = device.max_volume; break;
      }
      case T::kCaptureOutputUsed: f.kind = 8; s.GetBool(&f.bool_value); break;
    }
    return f;
}
class CallerAecDump final : public webrtc::AecDump {
 public:
  CallerAecDump(rust::Box<RustCallerApmDump> provider, std::shared_ptr<DumpCallbackStatus> status)
      : provider_(std::move(provider)), status_(std::move(status)) {}
  ~CallerAecDump() override { Observe(apm_dump_dispose(*provider_)); }
  void WriteInitMessage(const webrtc::ProcessingConfig& c, std::int64_t time) override {
    Observe(apm_dump_initialize(*provider_, {
      c.input_stream().sample_rate_hz(), c.input_stream().num_channels(),
      c.output_stream().sample_rate_hz(), c.output_stream().num_channels(),
      c.reverse_input_stream().sample_rate_hz(), c.reverse_input_stream().num_channels(),
      c.reverse_output_stream().sample_rate_hz(), c.reverse_output_stream().num_channels()}, time));
  }
  void AddCaptureStreamInput(const webrtc::AudioFrameView<const float>& v) override { Frame(0, v); }
  void AddCaptureStreamInput(webrtc::MonoView<const float> v) override { Float(0, 2, 1, v.size(), v); }
  void AddCaptureStreamOutput(const webrtc::AudioFrameView<const float>& v) override { Frame(1, v); }
  void AddCaptureStreamOutput(webrtc::MonoView<const float> v) override { Float(1, 2, 1, v.size(), v); }
  void AddCaptureStreamInput(const std::int16_t* data, int channels, int frames) override { Integer(0, data, channels, frames); }
  void AddCaptureStreamOutput(const std::int16_t* data, int channels, int frames) override { Integer(1, data, channels, frames); }
  void AddAudioProcessingState(const AudioProcessingState& s) override {
    Observe(apm_dump_state(*provider_, s.delay, s.drift, s.applied_input_volume.has_value(), s.applied_input_volume.value_or(0), s.keypress));
  }
  void WriteCaptureStreamMessage() override { Observe(apm_dump_capture(*provider_)); }
  void WriteRenderStreamMessage(const std::int16_t* data, int channels, int frames) override { Integer(2, data, channels, frames); }
  void WriteRenderStreamMessage(const webrtc::AudioFrameView<const float>& v) override { Frame(2, v); }
  void WriteRenderStreamMessage(const float* const* data, int channels, int frames) override {
    std::vector<float> flat;
    flat.reserve(std::size_t(channels) * std::size_t(frames));
    for (int ch = 0; ch < channels; ++ch) flat.insert(flat.end(), data[ch], data[ch] + frames);
    Float(2, 3, channels, frames, flat);
  }
  void WriteRuntimeSetting(const webrtc::AudioProcessing::RuntimeSetting& s) override { Observe(apm_dump_runtime(*provider_, RuntimeSnapshot(s))); }
  void WriteConfig(const webrtc::InternalAPMConfig& c) override { Observe(apm_dump_config(*provider_, DumpConfigToFfi(c))); }
 private:
  void Observe(bool success) { if (!success) status_->Reject(); }
  void Integer(std::uint8_t operation, const std::int16_t* data, int channels, int frames) {
    const auto count = std::size_t(channels) * std::size_t(frames);
    const std::int16_t empty_i = 0; const float empty_f = 0;
    Observe(apm_dump_audio(*provider_, operation, 0, channels, frames,
        rust::Slice<const std::int16_t>(count ? data : &empty_i, count), rust::Slice<const float>(&empty_f, 0)));
  }
  void Float(std::uint8_t operation, std::uint8_t layout, std::size_t channels, std::size_t frames, std::span<const float> data) {
    const std::int16_t empty_i = 0; const float empty_f = 0;
    Observe(apm_dump_audio(*provider_, operation, layout, channels, frames,
        rust::Slice<const std::int16_t>(&empty_i, 0), rust::Slice<const float>(data.empty() ? &empty_f : data.data(), data.size())));
  }
  void Frame(std::uint8_t operation, const webrtc::AudioFrameView<const float>& view) {
    std::vector<float> flat;
    flat.reserve(std::size_t(view.num_channels()) * std::size_t(view.samples_per_channel()));
    for (int ch = 0; ch < view.num_channels(); ++ch) {
      const auto channel = view.channel(ch);
      flat.insert(flat.end(), channel.begin(), channel.end());
    }
    Float(operation, 1, view.num_channels(), view.samples_per_channel(), flat);
  }
  rust::Box<RustCallerApmDump> provider_;
  std::shared_ptr<DumpCallbackStatus> status_;
};

}  // namespace

struct NativeEchoAudio::State {
  std::size_t channels, frames, bands, band_frames;
  bool split_ready, full_dirty = false, split_dirty = false;
  std::vector<float> full, split;
};
NativeEchoAudio::NativeEchoAudio(const webrtc::AudioBuffer& audio, bool ready) : state_(std::make_unique<State>()) {
  auto& s = *state_; s.channels = audio.num_channels(); s.frames = audio.num_frames();
  s.bands = audio.num_bands(); s.band_frames = audio.num_frames_per_band();
  s.split_ready = ready || s.bands == 1;
  s.full.reserve(s.channels * s.frames);
  for (std::size_t ch = 0; ch < s.channels; ++ch)
    s.full.insert(s.full.end(), audio.channels_const()[ch], audio.channels_const()[ch] + s.frames);
  // Capture analysis must not read allocated-but-not-yet-split storage.
  if (s.bands > 1 && s.split_ready) {
    s.split.reserve(s.channels * s.bands * s.band_frames);
    for (std::size_t ch = 0; ch < s.channels; ++ch)
      for (std::size_t band = 0; band < s.bands; ++band)
        s.split.insert(s.split.end(), audio.split_bands_const(ch)[band], audio.split_bands_const(ch)[band] + s.band_frames);
  }
}
NativeEchoAudio::~NativeEchoAudio() = default;
NativeEchoAudio::State& NativeEchoAudio::state() noexcept { return *state_; }
const NativeEchoAudio::State& NativeEchoAudio::state() const noexcept { return *state_; }
bool NativeEchoAudio::finite() const noexcept {
  const auto finite = [](float v) { return std::isfinite(v); };
  return std::all_of(state_->full.begin(), state_->full.end(), finite) &&
      std::all_of(state_->split.begin(), state_->split.end(), finite);
}
void NativeEchoAudio::Commit(webrtc::AudioBuffer& audio) const {
  const auto& s = *state_;
  if (s.full_dirty)
    for (std::size_t ch = 0; ch < s.channels; ++ch)
      std::copy_n(s.full.data() + ch * s.frames, s.frames, audio.channels()[ch]);
  if (s.split_dirty)
    for (std::size_t ch = 0; ch < s.channels; ++ch)
      for (std::size_t band = 0; band < s.bands; ++band)
        std::copy_n(s.split.data() + (ch * s.bands + band) * s.band_frames, s.band_frames, audio.split_bands(ch)[band]);
}
FfiEchoAudioShape audio_echo_view_shape(const NativeEchoAudio& audio) noexcept {
  const auto& s = audio.state();
  return {static_cast<std::int32_t>(s.frames * 100), s.channels, s.frames, s.bands, s.band_frames, s.split_ready};
}
rust::Slice<const float> audio_echo_view_channel(const NativeEchoAudio& audio, std::size_t channel, bool split, std::size_t band, bool& valid) noexcept {
  const auto& s = audio.state();
  valid = channel < s.channels && (!split || (s.split_ready && band < s.bands));
  if (!valid) return {};
  if (!split || s.bands == 1) return {s.full.data() + channel * s.frames, s.frames};
  return {s.split.data() + (channel * s.bands + band) * s.band_frames, s.band_frames};
}
rust::Slice<float> audio_echo_view_channel_mut(NativeEchoAudio& audio, std::size_t channel, bool split, std::size_t band, bool& valid) noexcept {
  auto& s = audio.state();
  valid = channel < s.channels && (!split || (s.split_ready && band < s.bands));
  if (!valid) return {};
  if (!split || s.bands == 1) { s.full_dirty = true; return {s.full.data() + channel * s.frames, s.frames}; }
  s.split_dirty = true; return {s.split.data() + (channel * s.bands + band) * s.band_frames, s.band_frames};
}
NativeEchoLinearOutput::NativeEchoLinearOutput(webrtc::AudioBuffer* output)
    : output_(output), frames_(output ? output->num_frames() : 0), channels_(output ? output->num_channels() : 0) {}
std::size_t NativeEchoLinearOutput::channels() const noexcept { return channels_.size(); }
std::size_t NativeEchoLinearOutput::frames() const noexcept { return frames_; }
bool NativeEchoLinearOutput::Write(std::size_t channel, rust::Slice<const float> samples) {
  if (channel >= channels_.size() || samples.size() != frames_) return false;
  channels_[channel].assign(samples.begin(), samples.end()); return true;
}
bool NativeEchoLinearOutput::complete() const noexcept {
  return std::all_of(channels_.begin(), channels_.end(), [&](const auto& ch) { return ch.size() == frames_; });
}
bool NativeEchoLinearOutput::finite() const noexcept {
  return std::all_of(channels_.begin(), channels_.end(), [](const auto& ch) {
    return std::all_of(ch.begin(), ch.end(), [](float v) { return std::isfinite(v); });
  });
}
void NativeEchoLinearOutput::Commit() const {
  if (!output_) return;
  for (std::size_t ch = 0; ch < channels_.size(); ++ch)
    std::copy(channels_[ch].begin(), channels_[ch].end(), output_->channels()[ch]);
}
std::size_t audio_echo_linear_channels(const NativeEchoLinearOutput& output) noexcept { return output.channels(); }
std::size_t audio_echo_linear_frames(const NativeEchoLinearOutput& output) noexcept { return output.frames(); }
bool audio_echo_linear_write(NativeEchoLinearOutput& output, std::size_t channel, rust::Slice<const float> samples) noexcept { return output.Write(channel, samples); }

namespace {
struct CustomCallbacks {
  rust::Box<RustApmCustomModule> provider;
  std::shared_ptr<DumpCallbackStatus> status;
  std::shared_ptr<std::atomic_bool> bounded_render = std::make_shared<std::atomic_bool>(false);
  std::int32_t rate = 0;
  bool render = false;
  CustomCallbacks(rust::Box<RustApmCustomModule> p, std::shared_ptr<DumpCallbackStatus> s)
      : provider(std::move(p)), status(std::move(s)) {}
  ~CustomCallbacks() { Observe(apm_custom_dispose(*provider)); }
  void Observe(bool success) const { if (!success) status->Reject(); }
  void ObserveMetrics(bool success) const { Observe(success); CallbackOperation::Metric(status, success); }
  void Initialize(int sample_rate, int channels) {
    rate = sample_rate;
    Observe(apm_custom_initialize(*provider, sample_rate, channels));
  }
  std::string Description() const {
    bool valid = false;
    const auto bytes = apm_custom_description(*provider, valid);
    Observe(valid);
    return bytes.empty() ? std::string() : std::string(reinterpret_cast<const char*>(bytes.data()), bytes.size());
  }
  std::vector<float> Snapshot(const webrtc::AudioBuffer& audio) const {
    std::vector<float> data;
    data.reserve(audio.num_channels() * audio.num_frames());
    for (std::size_t ch = 0; ch < audio.num_channels(); ++ch)
      data.insert(data.end(), audio.channels_const()[ch], audio.channels_const()[ch] + audio.num_frames());
    return data;
  }
  void Process(webrtc::AudioBuffer& audio) {
    // Never hand mutable native storage to Rust. Commit only a validated
    // complete callback frame, leaving the original finite working frame on
    // rejection/panic. The outer operation suppresses publication on failure.
    auto data = Snapshot(audio);
    float empty = 0;
    const bool valid = apm_custom_process(*provider, rate, audio.num_channels(), audio.num_frames(),
        rust::Slice<float>(data.empty() ? &empty : data.data(), data.size()));
    if (!valid) { Observe(false); return; }
    if (render && bounded_render->load(std::memory_order_relaxed) &&
        std::any_of(data.begin(), data.end(), [](float v) { return std::abs(v) > 32768.f; })) {
      // Provisional graph-dependent qualification boundary, not a claim of
      // native unsupported. Resolve the wider stateful filter envelope in the
      // consolidated qualification pass. Capture post accepts finite extremes.
      apm_custom_unqualified_render(*provider);
      Observe(false);
      return;
    }
    for (std::size_t ch = 0; ch < audio.num_channels(); ++ch)
      std::copy_n(data.data() + ch * audio.num_frames(), audio.num_frames(), audio.channels()[ch]);
  }
  void Analyze(const webrtc::AudioBuffer& audio) const {
    const auto data = Snapshot(audio);
    const float empty = 0;
    Observe(apm_custom_analyze(*provider, rate, audio.num_channels(), audio.num_frames(),
        rust::Slice<const float>(data.empty() ? &empty : data.data(), data.size())));
  }
};
void ClearEchoLinear(webrtc::AudioBuffer* linear) {
  // APM can consume this output before the owning operation reports failure.
  // This is private drain storage, never a successful Rust output.
  if (linear) for (std::size_t ch = 0; ch < linear->num_channels(); ++ch)
    std::fill_n(linear->channels()[ch], linear->num_frames(), 0.f);
}
class CallerEchoControl final : public webrtc::EchoControl {
 public:
  CallerEchoControl(rust::Box<RustApmCustomModule> provider, std::shared_ptr<DumpCallbackStatus> status,
      webrtc::NeuralResidualEchoEstimator* neural, std::shared_ptr<std::atomic_bool> invalid)
      : callbacks_(std::move(provider), std::move(status)), neural_(neural), invalid_(std::move(invalid)) {}
  void AnalyzeRender(webrtc::AudioBuffer* render) override {
    NativeEchoAudio audio(*render, true);
    NativeEchoDependencies dependencies(neural_);
    callbacks_.Observe(apm_echo_control_analyze(*callbacks_.provider, dependencies, audio, true));
  }
  void AnalyzeCapture(webrtc::AudioBuffer* capture) override {
    NativeEchoAudio audio(*capture, false);
    NativeEchoDependencies dependencies(neural_);
    callbacks_.Observe(apm_echo_control_analyze(*callbacks_.provider, dependencies, audio, false));
  }
  void ProcessCapture(webrtc::AudioBuffer* capture, bool level_change) override { Process(capture, nullptr, false, level_change); }
  void ProcessCapture(webrtc::AudioBuffer* capture, webrtc::AudioBuffer* linear, bool level_change) override { Process(capture, linear, true, level_change); }
  Metrics GetMetrics() const override {
    NativeEchoDependencies dependencies(neural_);
    const auto m = apm_echo_control_metrics(*callbacks_.provider, dependencies);
    callbacks_.ObserveMetrics(m.error == 0);
    return {m.echo_return_loss, m.echo_return_loss_enhancement, m.delay_ms};
  }
  void SetAudioBufferDelay(int delay) override { NativeEchoDependencies dependencies(neural_); callbacks_.Observe(apm_echo_control_delay(*callbacks_.provider, dependencies, delay)); }
  void SetCaptureOutputUsage(bool used) override { NativeEchoDependencies dependencies(neural_); callbacks_.Observe(apm_echo_control_usage(*callbacks_.provider, dependencies, used)); }
  bool ActiveProcessing() const override {
    NativeEchoDependencies dependencies(neural_);
    bool valid = false; const bool active = apm_echo_control_active(*callbacks_.provider, dependencies, valid);
    callbacks_.Observe(valid); return active;
  }
 private:
  void Process(webrtc::AudioBuffer* capture, webrtc::AudioBuffer* linear, bool overload, bool level_change) {
    if (invalid_->load(std::memory_order_acquire)) { ClearEchoLinear(linear); callbacks_.Observe(false); return; }
    NativeEchoAudio audio(*capture, true);
    NativeEchoLinearOutput output(linear);
    NativeEchoDependencies dependencies(neural_);
    if (!apm_echo_control_process(*callbacks_.provider, dependencies, audio, output, overload, level_change)) {
      if (linear) invalid_->store(true, std::memory_order_release);
      ClearEchoLinear(linear); callbacks_.Observe(false); return;
    }
    if (!output.complete() || !audio.finite() || !output.finite()) {
      apm_echo_control_invalid(*callbacks_.provider, !audio.finite() || !output.finite());
      if (linear) invalid_->store(true, std::memory_order_release);
      ClearEchoLinear(linear); callbacks_.Observe(false); return;
    }
    // Do not publish partial capture or pretend linear output. This product's
    // owning boundary checks the callback generation before reading either.
    audio.Commit(*capture); output.Commit();
  }
  CustomCallbacks callbacks_;
  webrtc::NeuralResidualEchoEstimator* neural_;
  std::shared_ptr<std::atomic_bool> invalid_;
};
class CallerCustomProcessing final : public webrtc::CustomProcessing {
 public:
  CustomCallbacks callbacks;
  CallerCustomProcessing(rust::Box<RustApmCustomModule> p, std::shared_ptr<DumpCallbackStatus> s)
      : callbacks(std::move(p), std::move(s)) {}
  void Initialize(int rate, int channels) override { callbacks.Initialize(rate, channels); }
  void Process(webrtc::AudioBuffer* audio) override { callbacks.Process(*audio); }
  std::string ToString() const override { return callbacks.Description(); }
  void SetRuntimeSetting(webrtc::AudioProcessing::RuntimeSetting s) override { callbacks.Observe(apm_custom_runtime(*callbacks.provider, RuntimeSnapshot(s))); }
};
class CallerCustomAnalyzer final : public webrtc::CustomAudioAnalyzer {
 public:
  CustomCallbacks callbacks;
  CallerCustomAnalyzer(rust::Box<RustApmCustomModule> p, std::shared_ptr<DumpCallbackStatus> s)
      : callbacks(std::move(p), std::move(s)) {}
  void Initialize(int rate, int channels) override { callbacks.Initialize(rate, channels); }
  void Analyze(const webrtc::AudioBuffer* audio) override { callbacks.Analyze(*audio); }
  std::string ToString() const override { return callbacks.Description(); }
};

class CallerEchoDetector : public webrtc::EchoDetector {
 public:
  CustomCallbacks callbacks;
  CallerEchoDetector(rust::Box<RustApmCustomModule> p, std::shared_ptr<DumpCallbackStatus> s) : callbacks(std::move(p), std::move(s)) {}
  void Initialize(int capture_rate, int capture_channels, int render_rate, int render_channels) override {
    callbacks.Observe(apm_echo_initialize(*callbacks.provider, capture_rate, capture_channels, render_rate, render_channels));
  }
  void AnalyzeRenderAudio(std::span<const float> samples) override { Analyze(true, samples); }
  void AnalyzeCaptureAudio(std::span<const float> samples) override { Analyze(false, samples); }
  Metrics GetMetrics() const override {
    const auto f = apm_echo_metrics(*callbacks.provider);
    callbacks.ObserveMetrics(!f.failed);
    Metrics m;
    if (f.likelihood_present) m.echo_likelihood = f.likelihood;
    if (f.recent_max_present) m.echo_likelihood_recent_max = f.recent_max;
    return m;
  }
 private:
  void Analyze(bool render, std::span<const float> samples) {
    const float empty = 0;
    callbacks.Observe(apm_echo_analyze(*callbacks.provider, render, rust::Slice<const float>(samples.empty() ? &empty : samples.data(), samples.size())));
  }
};


template <std::size_t N>
rust::Vec<float> NeuralSnapshot(std::span<const std::array<float, N>> rows) {
  rust::Vec<float> result;
  for (const auto& row : rows) for (float value : row) result.push_back(value);
  return result;
}
bool SafeNeuralSuppressor(const webrtc::EchoCanceller3Config::Suppressor& candidate,
    const webrtc::EchoCanceller3Config::Suppressor& original) {
  // EchoRemover constructs its detector from the original configuration. The
  // later neural override cannot change that detector's concrete mode.
  if (candidate.use_subband_nearend_detection != original.use_subband_nearend_detection) return false;
  webrtc::EchoCanceller3Config copy;
  copy.suppressor = candidate;
  // Validate a copy only, without field-trial adjustment or normalization of the
  // provider's result. Other fields are native defaults, not unrelated inputs.
  if (!webrtc::EchoCanceller3Config::Validate(&copy)) return false;
  for (const auto* tuning : {&candidate.nearend_tuning, &candidate.normal_tuning}) {
    if (!(tuning->mask_lf.enr_transparent < tuning->mask_lf.enr_suppress) ||
        !(tuning->mask_hf.enr_transparent < tuning->mask_hf.enr_suppress)) return false;
  }
  return true;
}
class CallerNeuralEstimator final : public webrtc::NeuralResidualEchoEstimator {
 public:
  CustomCallbacks callbacks;
  CallerNeuralEstimator(rust::Box<RustApmCustomModule> p, std::shared_ptr<DumpCallbackStatus> s) : callbacks(std::move(p), std::move(s)) {}
  void Estimate(const webrtc::Block& render, std::span<const std::array<float, 64>> y,
      std::span<const std::array<float, 64>> e, std::span<const std::array<float, 65>> S2,
      std::span<const std::array<float, 65>> Y2, std::span<const std::array<float, 65>> E2,
      bool dominant, std::span<std::array<float, 65>> R2, std::span<std::array<float, 65>> unbounded) override {
    // Native outputs need not be initialized on entry. Never snapshot them.
    // On void callback failure use finite scratch fallback solely to let native
    // unwind its DSP operation; the enclosing owning operation publishes no PCM.
    for (auto& row : R2) row.fill(0);
    for (auto& row : unbounded) row.fill(0);
    FfiNeuralEchoInput f{};
    f.render_bands = render.NumBands(); f.render_channels = render.NumChannels();
    for (int band = 0; band < render.NumBands(); ++band)
      for (int channel = 0; channel < render.NumChannels(); ++channel)
        for (float v : render.View(band, channel)) f.render.push_back(v);
    f.microphone = NeuralSnapshot<64>(y); f.linear_error = NeuralSnapshot<64>(e);
    f.linear_echo_spectrum = NeuralSnapshot<65>(S2); f.microphone_spectrum = NeuralSnapshot<65>(Y2); f.linear_error_spectrum = NeuralSnapshot<65>(E2);
    f.dominant_nearend = dominant; f.conservative_channels = R2.size(); f.unbounded_channels = unbounded.size();
    const auto value = apm_neural_estimate(*callbacks.provider, std::move(f));
    if (value.failed) { callbacks.Observe(false); return; }
    if (value.conservative.size() / 65 != R2.size() || value.conservative.size() % 65 ||
        value.unbounded.size() / 65 != unbounded.size() || value.unbounded.size() % 65 ||
        std::any_of(value.conservative.begin(), value.conservative.end(), [](float v) { return !std::isfinite(v); }) ||
        std::any_of(value.unbounded.begin(), value.unbounded.end(), [](float v) { return !std::isfinite(v); })) {
      apm_neural_invalid(*callbacks.provider); callbacks.Observe(false); return;
    }
    for (std::size_t ch = 0; ch < R2.size(); ++ch) std::copy_n(value.conservative.data() + 65 * ch, 65, R2[ch].begin());
    for (std::size_t ch = 0; ch < unbounded.size(); ++ch) std::copy_n(value.unbounded.data() + 65 * ch, 65, unbounded[ch].begin());
  }
  webrtc::EchoCanceller3Config::Suppressor AdjustConfig(const webrtc::EchoCanceller3Config::Suppressor& config) const override {
    bool accepted = false;
    const auto result = AdjustConfigRaw(config, &accepted);
    const bool safe = accepted && SafeNeuralSuppressor(result, config);
    CallbackOperation::Construction(callbacks.status, safe);
    if (safe) return result;
    if (accepted) { apm_neural_adjust_invalid(*callbacks.provider); callbacks.Observe(false); }
    // The void construction path must finish safely. The enclosing operation
    // observes failure and never publishes this private fallback as success.
    return config;
  }
  webrtc::EchoCanceller3Config::Suppressor AdjustConfigRaw(const webrtc::EchoCanceller3Config::Suppressor& config, bool* accepted = nullptr) const {
    webrtc::EchoCanceller3Config c; c.suppressor = config;
    bool valid = false;
    const auto f = apm_neural_adjust(*callbacks.provider, EchoConfigToFfi(c), valid);
    if (accepted) *accepted = valid;
    callbacks.Observe(valid);
    return valid ? EchoConfigFromFfi(f).suppressor : config;
  }
  void Reset() override { callbacks.Observe(apm_neural_reset(*callbacks.provider)); }
};

// A bounded safety envelope, not an acknowledgment cache or replacement queue.
// The SDK retains the original payloads. Max magnitude suffices for finite
// linear gain/product admission; NaN/Inf must be remembered independently.
struct PendingGainRange {
  bool any = false;
  bool nonfinite = false;
  float max_absolute = 0;
  void Include(float value) {
    any = true;
    if (!std::isfinite(value)) nonfinite = true;
    else max_absolute = std::max(max_absolute, std::abs(value));
  }
  void Merge(const PendingGainRange& other) {
    any |= other.any;
    nonfinite |= other.nonfinite;
    max_absolute = std::max(max_absolute, other.max_absolute);
  }
};
struct PendingGains {
  PendingGainRange pre, post;
  void Include(const FfiApmRuntimeSetting& f) {
    if (f.kind == 1) pre.Include(f.float_value);
    if (f.kind == 2) post.Include(f.float_value);
  }
  void Merge(const PendingGains& other) { pre.Merge(other.pre); post.Merge(other.post); }
  bool Safe(const webrtc::AudioProcessing::Config& c) const {
    if (pre.any && (c.pre_amplifier.enabled || c.capture_level_adjustment.enabled)) {
      if (pre.nonfinite) return false;
      if (c.pre_amplifier.enabled && c.capture_level_adjustment.enabled &&
          !std::isfinite(pre.max_absolute * std::abs(c.capture_level_adjustment.pre_gain_factor))) return false;
    }
    return !(post.any && c.capture_level_adjustment.enabled && post.nonfinite);
  }
};
}
namespace {
struct EchoProductHealth {
  std::atomic_bool invalid{false};
};
struct EchoFactoryObservation {
  std::atomic<std::uint64_t> creations{0};
  // Per-product state prevents retired standalone products from changing the
  // health of the controller selected later by the owning APM.
  // The SDK's pinned libc++ lacks atomic<shared_ptr<T>>. Its C++11 atomic
  // shared_ptr operations provide the same publication and ownership semantics.
  std::shared_ptr<EchoProductHealth> current;
};
bool EchoFactoryInvalid(const std::shared_ptr<EchoFactoryObservation>& observation) {
  if (!observation) return false;
  const auto health = std::atomic_load_explicit(&observation->current, std::memory_order_acquire);
  return health && health->invalid.load(std::memory_order_acquire);
}
class CallerEchoFactory;
// Observe each nonnull native creation without changing its inputs or product.
class ObservedEchoFactory final : public webrtc::EchoControlFactory {
 public:
  ObservedEchoFactory(std::unique_ptr<webrtc::EchoControlFactory> factory, std::shared_ptr<EchoFactoryObservation> observation)
      : factory_(std::move(factory)), observation_(std::move(observation)) {}
  void Rebind(std::shared_ptr<EchoFactoryObservation> observation) { observation_ = std::move(observation); }
  std::unique_ptr<webrtc::EchoControl> Create(const webrtc::Environment& env, int rate, int render, int capture) override {
    auto product = factory_->Create(env, rate, render, capture);
    if (product) observation_->creations.fetch_add(1, std::memory_order_release);
    return product;
  }
  std::unique_ptr<webrtc::EchoControl> Create(const webrtc::Environment& env, int rate, int render, int capture, webrtc::NeuralResidualEchoEstimator* neural) override {
    auto product = factory_->Create(env, rate, render, capture, neural);
    if (product) observation_->creations.fetch_add(1, std::memory_order_release);
    return product;
  }
 private:
  std::unique_ptr<webrtc::EchoControlFactory> factory_;
  std::shared_ptr<EchoFactoryObservation> observation_;
};
}
struct NativeApmCustomModule::State {
  std::int32_t custom_rate = 0;
  std::int32_t custom_channels = 0;
  std::shared_ptr<DumpCallbackStatus> status = std::make_shared<DumpCallbackStatus>();
  std::unique_ptr<webrtc::CustomProcessing> processor;
  std::unique_ptr<webrtc::CustomAudioAnalyzer> analyzer;
  webrtc::scoped_refptr<webrtc::EchoDetector> detector;
  std::unique_ptr<webrtc::NeuralResidualEchoEstimator> neural;
  std::unique_ptr<webrtc::EchoControlFactory> echo_factory;
  CallerEchoFactory* caller_echo_factory = nullptr;  // owned by echo_factory
  ObservedEchoFactory* echo_factory_forwarder = nullptr;  // owned by echo_factory
  std::shared_ptr<EchoFactoryObservation> echo_factory_observation;
  bool echo_factory_linear = false;
  std::int64_t echo_min_fixed_delay = 0, echo_max_fixed_delay = 0;
  std::vector<webrtc::EchoCanceller3Config> echo_configs;
};
NativeApmCustomModule::NativeApmCustomModule(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeApmCustomModule::~NativeApmCustomModule() = default;
NativeApmCustomModule::State& NativeApmCustomModule::state() noexcept { return *state_; }
const NativeApmCustomModule::State& NativeApmCustomModule::state() const noexcept { return *state_; }
std::unique_ptr<NativeApmCustomModule> audio_processor_echo_factory_new(std::uint8_t constructor, const FfiAec3Selection& f) noexcept {
  auto state = std::make_unique<NativeApmCustomModule::State>();
  const auto mono = constructor ? EchoConfigFromFfi(f.mono) : webrtc::EchoCanceller3Config();
  std::optional<webrtc::EchoCanceller3Config> multi;
  if (constructor == 2 && f.multi_present) multi = EchoConfigFromFfi(f.multi);
  state->echo_configs.push_back(mono); if (multi) state->echo_configs.push_back(*multi);
  state->echo_factory_linear = mono.filter.export_linear_aec_output && (!multi || multi->filter.export_linear_aec_output);
  state->echo_min_fixed_delay = mono.delay.fixed_capture_delay_samples;
  state->echo_max_fixed_delay = mono.delay.fixed_capture_delay_samples;
  if (multi) {
    state->echo_min_fixed_delay = std::min<std::int64_t>(state->echo_min_fixed_delay, multi->delay.fixed_capture_delay_samples);
    state->echo_max_fixed_delay = std::max<std::int64_t>(state->echo_max_fixed_delay, multi->delay.fixed_capture_delay_samples);
  }
  if (constructor == 0) state->echo_factory = std::make_unique<webrtc::EchoCanceller3Factory>();
  else if (constructor == 1) state->echo_factory = std::make_unique<webrtc::EchoCanceller3Factory>(mono);
  else state->echo_factory = std::make_unique<webrtc::EchoCanceller3Factory>(mono, multi);
  state->echo_factory_observation = std::make_shared<EchoFactoryObservation>();
  auto observed = std::make_unique<ObservedEchoFactory>(std::move(state->echo_factory), state->echo_factory_observation);
  state->echo_factory_forwarder = observed.get(); state->echo_factory = std::move(observed);
  return std::make_unique<NativeApmCustomModule>(std::move(state));
}
struct NativeEchoControl::State {
  // The transferred controller dies before every native/Rust dependency.
  std::optional<webrtc::Environment> creation_environment;
  std::optional<rust::Box<RustEchoEnvironment>> retained_environment;
  webrtc::NeuralResidualEchoEstimator* neural = nullptr;
  std::shared_ptr<std::atomic_bool> invalid = std::make_shared<std::atomic_bool>(false);
  std::shared_ptr<EchoProductHealth> factory_health;
  std::int32_t rate = 0;
  std::size_t render_channels = 0, capture_channels = 0;
  bool linear = false;
  bool caller = false;
  std::int64_t min_fixed_delay = 0, max_fixed_delay = 0;
  std::vector<std::shared_ptr<DumpCallbackStatus>> callbacks;
  std::unique_ptr<webrtc::AudioBuffer> render, capture, linear_output;
  std::unique_ptr<webrtc::EchoControl> control;
};
NativeEchoControl::NativeEchoControl(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeEchoControl::~NativeEchoControl() = default;
NativeEchoControl::State& NativeEchoControl::state() noexcept { return *state_; }
namespace {
bool EchoProductInvalid(const NativeEchoControl::State& state) {
  return state.invalid->load(std::memory_order_acquire) ||
      (state.factory_health && state.factory_health->invalid.load(std::memory_order_acquire));
}
CallbackOperation EchoGeneration(const NativeEchoControl::State& state) {
  return CallbackOperation(state.callbacks, EchoProductInvalid(state));
}
void EchoSnapshot(const webrtc::AudioBuffer& buffer, rust::Vec<float>& output) {
  output.reserve(buffer.num_channels() * buffer.num_frames());
  for (std::size_t ch = 0; ch < buffer.num_channels(); ++ch)
    for (std::size_t frame = 0; frame < buffer.num_frames(); ++frame) output.push_back(buffer.channels_const()[ch][frame]);
}
}
namespace {
std::unique_ptr<NativeEchoControl> EchoControlFromProvider(std::int32_t rate, std::int32_t render_channels,
    std::int32_t capture_channels, rust::Box<RustApmCustomModule> provider, webrtc::NeuralResidualEchoEstimator* neural, std::uint8_t& error) {
  error = 2;
  if ((rate != 8000 && rate != 16000 && rate != 32000 && rate != 48000) || render_channels <= 0 || capture_channels <= 0 ||
      capture_channels > std::numeric_limits<int>::max() / 160 ||
      render_channels > std::numeric_limits<int>::max() / (rate / 100) ||
      capture_channels > std::numeric_limits<int>::max() / (rate / 100)) return nullptr;
  auto state = std::make_unique<NativeEchoControl::State>();
  state->rate = rate; state->render_channels = render_channels; state->capture_channels = capture_channels;
  state->caller = true; state->linear = true;
  auto status = std::make_shared<DumpCallbackStatus>(); state->callbacks.push_back(status);
  state->neural = neural;
  state->control = std::make_unique<CallerEchoControl>(std::move(provider), std::move(status), neural, state->invalid);
  state->render = std::make_unique<webrtc::AudioBuffer>(rate, render_channels, rate, render_channels, rate);
  state->capture = std::make_unique<webrtc::AudioBuffer>(rate, capture_channels, rate, capture_channels, rate);
  state->linear_output = std::make_unique<webrtc::AudioBuffer>(16000, capture_channels, 16000, capture_channels, 16000);
  error = 0; return std::make_unique<NativeEchoControl>(std::move(state));
}
std::unique_ptr<NativeEchoControl> CreateEchoControl(NativeApmCustomModule& factory, const webrtc::Environment& env,
    std::int32_t rate, std::int32_t render_channels, std::int32_t capture_channels, bool neural_overload,
    webrtc::NeuralResidualEchoEstimator* neural, std::uint8_t& error) {
  error = 2;
  if (!factory.state().echo_factory || (rate != 16000 && rate != 32000 && rate != 48000 &&
      !(rate == 8000 && factory.state().caller_echo_factory)) || render_channels <= 0 || capture_channels <= 0 ||
      capture_channels > std::numeric_limits<int>::max() / 160 ||
      render_channels > std::numeric_limits<int>::max() / (rate / 100) ||
      capture_channels > std::numeric_limits<int>::max() / (rate / 100)) return nullptr;
  if (!factory.state().caller_echo_factory && !SafeAecTrials(env.field_trials())) { error = 1; return nullptr; }
  for (const auto& config : factory.state().echo_configs)
    if (!SafeEchoConfig(config, env.field_trials())) { error = 3; return nullptr; }
  auto state = std::make_unique<NativeEchoControl::State>();
  state->rate = rate; state->render_channels = render_channels; state->capture_channels = capture_channels;
  state->creation_environment = env; state->neural = neural;
  state->caller = bool(factory.state().caller_echo_factory);
  state->linear = factory.state().echo_factory_linear;
  state->min_fixed_delay = factory.state().echo_min_fixed_delay; state->max_fixed_delay = factory.state().echo_max_fixed_delay;
  state->callbacks.push_back(factory.state().status);
  if (neural) state->callbacks.push_back(static_cast<CallerNeuralEstimator&>(*neural).callbacks.status);
  for (const auto& config : factory.state().echo_configs) {
    const auto adjusted = webrtc::AdjustConfig(config, env.field_trials());
    state->min_fixed_delay = std::min<std::int64_t>(state->min_fixed_delay, adjusted.delay.fixed_capture_delay_samples);
    state->max_fixed_delay = std::max<std::int64_t>(state->max_fixed_delay, adjusted.delay.fixed_capture_delay_samples);
  }
  const auto before = EchoGeneration(*state);
  state->control = neural_overload
      ? factory.state().echo_factory->Create(env, rate, render_channels, capture_channels, neural)
      : factory.state().echo_factory->Create(env, rate, render_channels, capture_channels);
  state->factory_health = std::atomic_load_explicit(&factory.state().echo_factory_observation->current, std::memory_order_acquire);
  error = before.failed() || (state->factory_health && state->factory_health->invalid.load(std::memory_order_acquire)) ? 18 : 0;
  if (error || !state->control) return nullptr;
  state->render = std::make_unique<webrtc::AudioBuffer>(rate, render_channels, rate, render_channels, rate);
  state->capture = std::make_unique<webrtc::AudioBuffer>(rate, capture_channels, rate, capture_channels, rate);
  if (state->linear) state->linear_output = std::make_unique<webrtc::AudioBuffer>(16000, capture_channels, 16000, capture_channels, 16000);
  return std::make_unique<NativeEchoControl>(std::move(state));
}
}
std::unique_ptr<NativeEchoControl> audio_echo_control_from_provider(std::int32_t rate, std::int32_t render_channels,
    std::int32_t capture_channels, rust::Box<RustApmCustomModule> provider, std::uint8_t& error) noexcept {
  return EchoControlFromProvider(rate, render_channels, capture_channels, std::move(provider), nullptr, error);
}
std::unique_ptr<NativeEchoControl> audio_echo_control_create(NativeApmCustomModule& factory, const NativeEnvironment& environment,
    std::int32_t rate, std::int32_t render_channels, std::int32_t capture_channels, bool neural_overload,
    const NativeApmCustomModule* neural, std::uint8_t& error) noexcept {
  if (neural && !neural->state().neural) { error = 2; return nullptr; }
  return CreateEchoControl(factory, environment.environment(), rate, render_channels, capture_channels,
      neural_overload, neural ? neural->state().neural.get() : nullptr, error);
}
FfiEchoControlAudio audio_echo_control_audio(NativeEchoControl& owner, std::uint8_t operation,
    std::int32_t rate, std::size_t channels, rust::Slice<const float> samples, bool level_change, bool linear) noexcept {
  auto& state = owner.state(); FfiEchoControlAudio result{}; result.error = 2;
  const auto expected_channels = operation == 0 ? state.render_channels : state.capture_channels;
  if (operation > 3 || rate != state.rate || channels != expected_channels ||
      samples.size() != channels * static_cast<std::size_t>(state.rate / 100)) return result;
  // Provisional conventional FloatS16 DSP envelope, tracked for final numeric
  // qualification rather than mislabeled as native unsupported.
  if (std::any_of(samples.begin(), samples.end(), [](float v) { return !std::isfinite(v) || std::abs(v) > 32768.f; })) { result.error = 19; return result; }
  if (linear && (operation != 3 || !state.linear)) { result.error = 7; return result; }
  auto& buffer = operation == 0 ? *state.render : *state.capture;
  for (std::size_t ch = 0; ch < channels; ++ch)
    std::copy_n(samples.data() + ch * buffer.num_frames(), buffer.num_frames(), buffer.channels()[ch]);
  const auto before = EchoGeneration(state);
  if (operation != 1 && buffer.num_bands() > 1) buffer.SplitIntoFrequencyBands();
  if (operation == 0) state.control->AnalyzeRender(&buffer);
  else if (operation == 1) state.control->AnalyzeCapture(&buffer);
  else if (operation == 2) state.control->ProcessCapture(&buffer, level_change);
  else state.control->ProcessCapture(&buffer, linear ? state.linear_output.get() : nullptr, level_change);
  result.error = !before.failed() ? 0 : 18;
  if (result.error || operation < 2) return result;
  if (buffer.num_bands() > 1) buffer.MergeFrequencyBands();
  EchoSnapshot(buffer, result.samples);
  if (linear) EchoSnapshot(*state.linear_output, result.linear);
  return result;
}
FfiEchoControlMetrics audio_echo_control_metrics(NativeEchoControl& owner) noexcept {
  auto& state = owner.state(); const auto before = EchoGeneration(state);
  const auto m = state.control->GetMetrics(); FfiEchoControlMetrics result{};
  result.echo_return_loss = m.echo_return_loss; result.echo_return_loss_enhancement = m.echo_return_loss_enhancement; result.delay_ms = m.delay_ms;
  result.error = !before.failed() ? 0 : 18; return result;
}
std::uint8_t audio_echo_control_delay(NativeEchoControl& owner, std::int32_t delay) noexcept {
  auto& state = owner.state();
  // render_delay_buffer.cc multiplies milliseconds by 16 then adds the
  // selected fixed capture delay using signed int arithmetic.
  const auto low = std::int64_t(delay) * 16 + state.min_fixed_delay;
  const auto high = std::int64_t(delay) * 16 + state.max_fixed_delay;
  if (!state.caller && (std::int64_t(delay) * 16 < std::numeric_limits<int>::min() ||
      std::int64_t(delay) * 16 > std::numeric_limits<int>::max() ||
      low < 0 || high > std::numeric_limits<int>::max())) return 20;
  const auto before = EchoGeneration(state); state.control->SetAudioBufferDelay(delay);
  return !before.failed() ? 0 : 18;
}
std::uint8_t audio_echo_control_usage(NativeEchoControl& owner, bool used) noexcept {
  auto& state = owner.state(); const auto before = EchoGeneration(state); state.control->SetCaptureOutputUsage(used);
  return !before.failed() ? 0 : 18;
}
bool audio_echo_control_active(NativeEchoControl& owner, std::uint8_t& error) noexcept {
  auto& state = owner.state(); const auto before = EchoGeneration(state); const auto value = state.control->ActiveProcessing();
  error = !before.failed() ? 0 : 18; return value;
}
std::unique_ptr<NativeApmCustomModule> audio_processor_neural_new(rust::Box<RustApmCustomModule> module) noexcept {
  auto state = std::make_unique<NativeApmCustomModule::State>();
  state->neural = std::make_unique<CallerNeuralEstimator>(std::move(module), state->status);
  return std::make_unique<NativeApmCustomModule>(std::move(state));
}
std::uint8_t audio_processor_neural_reset(NativeApmCustomModule& module) noexcept {
  const CallbackOperation before({module.state().status});
  module.state().neural->Reset();
  return !before.failed() ? 0 : 18;
}
FfiEchoCanceller3Config audio_processor_neural_adjust(NativeApmCustomModule& module, const FfiEchoCanceller3Config& input, std::uint8_t& error) noexcept {
  auto c = EchoConfigFromFfi(input);
  const CallbackOperation before({module.state().status});
  c.suppressor = static_cast<CallerNeuralEstimator&>(*module.state().neural).AdjustConfigRaw(c.suppressor);
  error = !before.failed() ? 0 : 18;
  return EchoConfigToFfi(c);
}

namespace {
template <std::size_t N> std::vector<std::array<float, N>> NeuralRows(const rust::Vec<float>& values) {
  std::vector<std::array<float, N>> rows(values.size() / N);
  for (std::size_t row = 0; row < rows.size(); ++row) std::copy_n(values.data() + row * N, N, rows[row].begin());
  return rows;
}
}
namespace {
FfiNeuralEchoResult EstimateNeural(CallerNeuralEstimator& neural, FfiNeuralEchoInput f) {
  FfiNeuralEchoResult result{}; result.failed = true;
  constexpr auto max_index = static_cast<std::size_t>(std::numeric_limits<int>::max());
  constexpr auto max_rows = static_cast<std::size_t>(std::numeric_limits<std::ptrdiff_t>::max()) / sizeof(std::array<float, 65>);
  if (f.render_bands > max_index || f.render_channels > max_index ||
      (f.render_bands && f.render_channels > max_index / 64 / f.render_bands) ||
      f.render.size() != f.render_bands * f.render_channels * 64 ||
      f.microphone.size() % 64 || f.linear_error.size() % 64 ||
      f.linear_echo_spectrum.size() % 65 || f.microphone_spectrum.size() % 65 || f.linear_error_spectrum.size() % 65 ||
      f.conservative_channels > max_rows || f.unbounded_channels > max_rows) {
    auto& callback = neural.callbacks;
    apm_neural_invalid(*callback.provider); callback.Observe(false); return result;
  }
  webrtc::Block render(static_cast<int>(f.render_bands), static_cast<int>(f.render_channels));
  if (f.render_channels) for (int band = 0; band < render.NumBands(); ++band)
    for (int channel = 0; channel < render.NumChannels(); ++channel)
      std::copy_n(f.render.data() + (static_cast<std::size_t>(band) * f.render_channels + channel) * 64, 64, render.View(band, channel).begin());
  const auto y = NeuralRows<64>(f.microphone), e = NeuralRows<64>(f.linear_error);
  const auto S2 = NeuralRows<65>(f.linear_echo_spectrum), Y2 = NeuralRows<65>(f.microphone_spectrum), E2 = NeuralRows<65>(f.linear_error_spectrum);
  std::vector<std::array<float, 65>> conservative(f.conservative_channels), unbounded(f.unbounded_channels);
  const CallbackOperation before({neural.callbacks.status});
  neural.Estimate(render, y, e, S2, Y2, E2, f.dominant_nearend, conservative, unbounded);
  result.failed = before.failed();
  if (!result.failed) {
    result.conservative = NeuralSnapshot<65>(conservative);
    result.unbounded = NeuralSnapshot<65>(unbounded);
  }
  return result;
}

}
FfiNeuralEchoResult audio_processor_neural_estimate(NativeApmCustomModule& module, FfiNeuralEchoInput f) noexcept {
  return EstimateNeural(static_cast<CallerNeuralEstimator&>(*module.state().neural), std::move(f));
}

// All nonnull estimator inputs originate in the owning Rust neural adapter.
// Loans exist only while creation or a serialized controller method is active.
bool audio_echo_dependency_present(const NativeEchoDependencies& dependencies) noexcept { return dependencies.neural != nullptr; }
std::uint8_t audio_echo_dependency_reset(NativeEchoDependencies& dependencies) noexcept {
  if (!dependencies.neural) return 2;
  const CallbackOperation operation({static_cast<CallerNeuralEstimator&>(*dependencies.neural).callbacks.status});
  dependencies.neural->Reset(); return operation.failed() ? 18 : 0;
}
FfiEchoCanceller3Config audio_echo_dependency_adjust(NativeEchoDependencies& dependencies, const FfiEchoCanceller3Config& f, std::uint8_t& error) noexcept {
  error = 2; if (!dependencies.neural) return f;
  auto config = EchoConfigFromFfi(f);
  const CallbackOperation operation({static_cast<CallerNeuralEstimator&>(*dependencies.neural).callbacks.status});
  config.suppressor = static_cast<CallerNeuralEstimator&>(*dependencies.neural).AdjustConfigRaw(config.suppressor);
  error = operation.failed() ? 18 : 0; return EchoConfigToFfi(config);
}
FfiNeuralEchoResult audio_echo_dependency_estimate(NativeEchoDependencies& dependencies, FfiNeuralEchoInput f) noexcept {
  if (!dependencies.neural) { FfiNeuralEchoResult result{}; result.failed = true; return result; }
  return EstimateNeural(static_cast<CallerNeuralEstimator&>(*dependencies.neural), std::move(f));
}
class NativeEchoCreationContext final {
 public:
  const webrtc::Environment& environment;
  std::int32_t rate, render_channels, capture_channels;
  webrtc::NeuralResidualEchoEstimator* neural;
};
std::int32_t audio_echo_context_rate(const NativeEchoCreationContext& context) noexcept { return context.rate; }
std::int32_t audio_echo_context_render_channels(const NativeEchoCreationContext& context) noexcept { return context.render_channels; }
std::int32_t audio_echo_context_capture_channels(const NativeEchoCreationContext& context) noexcept { return context.capture_channels; }
std::unique_ptr<NativeEchoControl> audio_echo_context_create(NativeApmCustomModule& factory, const NativeEchoCreationContext& context, bool include_neural, std::uint8_t& error) noexcept {
  return CreateEchoControl(factory, context.environment, context.rate, context.render_channels, context.capture_channels,
      include_neural, include_neural ? context.neural : nullptr, error);
}
std::unique_ptr<NativeEchoControl> audio_echo_context_controller(const NativeEchoCreationContext& context, bool include_neural,
    rust::Box<RustApmCustomModule> provider, std::uint8_t& error) noexcept {
  auto result = EchoControlFromProvider(context.rate, context.render_channels, context.capture_channels, std::move(provider),
      include_neural ? context.neural : nullptr, error);
  if (result) result->state().creation_environment = context.environment;
  return result;
}
void audio_echo_control_retain_environment(NativeEchoControl& control, rust::Box<RustEchoEnvironment> environment) noexcept {
  if (!control.state().creation_environment)
    control.state().creation_environment = apm_echo_environment_native(*environment)->environment();
  control.state().retained_environment.emplace(std::move(environment));
}
namespace {
bool EchoEnvironmentMatches(const webrtc::Environment& a, const webrtc::Environment& b) {
  return &a.clock() == &b.clock() && &a.task_queue_factory() == &b.task_queue_factory() &&
      &a.field_trials() == &b.field_trials() && &a.event_log() == &b.event_log();
}
// A native Create consumer cannot accept null. This is a private failed product,
// not a substitute DSP or successful buffering result. It rejects every use and
// initializes only the SDK's drain-only linear storage before downstream reads.
class FailedEchoControl final : public webrtc::EchoControl {
 public:
  explicit FailedEchoControl(std::shared_ptr<CustomCallbacks> callbacks) : callbacks_(std::move(callbacks)) {}
  void AnalyzeRender(webrtc::AudioBuffer*) override { Reject(); }
  void AnalyzeCapture(webrtc::AudioBuffer*) override { Reject(); }
  void ProcessCapture(webrtc::AudioBuffer*, bool) override { Reject(); }
  void ProcessCapture(webrtc::AudioBuffer*, webrtc::AudioBuffer* linear, bool) override { ClearEchoLinear(linear); Reject(); }
  Metrics GetMetrics() const override { Reject(); CallbackOperation::Metric(callbacks_->status, false); return {}; }
  void SetAudioBufferDelay(int) override { Reject(); }
  void SetCaptureOutputUsage(bool) override { Reject(); }
  bool ActiveProcessing() const override { Reject(); return false; }
 private:
  void Reject() const { callbacks_->Observe(false); }
  std::shared_ptr<CustomCallbacks> callbacks_;
};
class TransferredEchoControl final : public webrtc::EchoControl {
 public:
  TransferredEchoControl(std::unique_ptr<NativeEchoControl> product, std::shared_ptr<CustomCallbacks> callbacks,
      std::shared_ptr<EchoProductHealth> health)
      : callbacks_(std::move(callbacks)), health_(std::move(health)), product_(std::move(product)) {}
  ~TransferredEchoControl() override {
    bool failed = false;
    { const CallbackOperation operation(product_->state().callbacks); product_.reset(); failed = operation.failed(); }
    if (failed) callbacks_->Observe(false);
  }
  void AnalyzeRender(webrtc::AudioBuffer* audio) override {
    if (Shape(*audio, true)) Run([&](auto& control) { control.AnalyzeRender(audio); });
  }
  void AnalyzeCapture(webrtc::AudioBuffer* audio) override {
    if (Shape(*audio, false)) Run([&](auto& control) { control.AnalyzeCapture(audio); });
  }
  void ProcessCapture(webrtc::AudioBuffer* audio, bool level_change) override { Process(audio, nullptr, false, level_change); }
  void ProcessCapture(webrtc::AudioBuffer* audio, webrtc::AudioBuffer* linear, bool level_change) override { Process(audio, linear, true, level_change); }
  Metrics GetMetrics() const override {
    Metrics result{}; const bool valid = Run([&](auto& control) { result = control.GetMetrics(); });
    CallbackOperation::Metric(callbacks_->status, valid); return result;
  }
  void SetAudioBufferDelay(int delay) override {
    Run([&](auto&) { if (audio_echo_control_delay(*product_, delay)) callbacks_->Observe(false); });
  }
  void SetCaptureOutputUsage(bool used) override { Run([&](auto& control) { control.SetCaptureOutputUsage(used); }); }
  bool ActiveProcessing() const override {
    bool result = false; return Run([&](auto& control) { result = control.ActiveProcessing(); }) && result;
  }
 private:
  bool Shape(const webrtc::AudioBuffer& audio, bool render) {
    auto& state = product_->state();
    if (audio.num_frames() == static_cast<std::size_t>(state.rate / 100) &&
        audio.num_channels() == (render ? state.render_channels : state.capture_channels)) return true;
    Invalidate(); return false;
  }
  void Invalidate() const {
    product_->state().invalid->store(true, std::memory_order_release);
    health_->invalid.store(true, std::memory_order_release);
    apm_echo_factory_invalid(*callbacks_->provider); callbacks_->Observe(false);
  }
  template <typename F> bool Run(F&& invoke) const {
    auto& state = product_->state();
    if (EchoProductInvalid(state)) {
      health_->invalid.store(true, std::memory_order_release); callbacks_->Observe(false); return false;
    }
    bool failed = false;
    { const CallbackOperation operation(state.callbacks); invoke(*state.control); failed = operation.failed(); }
    if (EchoProductInvalid(state)) {
      health_->invalid.store(true, std::memory_order_release); failed = true;
    }
    // Propagate only after the nested product scope has ended, so an operation
    // can observe the same factory status without stealing its own failure.
    if (failed) callbacks_->Observe(false);
    return !failed;
  }
  void Process(webrtc::AudioBuffer* audio, webrtc::AudioBuffer* linear, bool overload, bool level_change) {
    auto& state = product_->state();
    if (!Shape(*audio, false)) { ClearEchoLinear(linear); return; }
    if (linear && (!state.linear || linear->num_frames() != 160 || linear->num_channels() != state.capture_channels)) {
      Invalidate(); ClearEchoLinear(linear); return;
    }
    const bool valid = Run([&](auto& control) {
      if (overload) control.ProcessCapture(audio, linear, level_change);
      else control.ProcessCapture(audio, level_change);
    });
    if (!valid) ClearEchoLinear(linear);
  }
  std::shared_ptr<CustomCallbacks> callbacks_;
  std::shared_ptr<EchoProductHealth> health_;
  std::unique_ptr<NativeEchoControl> product_;
};
class CallerEchoFactory final : public webrtc::EchoControlFactory {
 public:
  CallerEchoFactory(rust::Box<RustApmCustomModule> provider, std::shared_ptr<DumpCallbackStatus> status,
      std::shared_ptr<EchoFactoryObservation> observation)
      : callbacks_(std::make_shared<CustomCallbacks>(std::move(provider), std::move(status))), observation_(std::move(observation)) {}
  void Bind(rust::Box<RustEchoEnvironment> environment) { environment_.emplace(std::move(environment)); }
  void Rebind(std::shared_ptr<EchoFactoryObservation> observation) { observation_ = std::move(observation); }
  std::unique_ptr<webrtc::EchoControl> Create(const webrtc::Environment& environment, int rate, int render, int capture) override {
    return CreateImpl(environment, rate, render, capture, false, nullptr);
  }
  std::unique_ptr<webrtc::EchoControl> Create(const webrtc::Environment& environment, int rate, int render, int capture, webrtc::NeuralResidualEchoEstimator* neural) override {
    return CreateImpl(environment, rate, render, capture, true, neural);
  }
 private:
  std::unique_ptr<webrtc::EchoControl> CreateImpl(const webrtc::Environment& environment, int rate, int render, int capture,
      bool neural_overload, webrtc::NeuralResidualEchoEstimator* neural) {
    auto health = std::make_shared<EchoProductHealth>();
    std::atomic_store_explicit(&observation_->current, health, std::memory_order_release);
    auto fail = [&]() -> std::unique_ptr<webrtc::EchoControl> {
      health->invalid.store(true, std::memory_order_release); callbacks_->Observe(false);
      return std::make_unique<FailedEchoControl>(callbacks_);
    };
    if (!environment_) { apm_echo_factory_invalid(*callbacks_->provider); return fail(); }
    NativeEchoCreationContext context{environment, rate, render, capture, neural};
    NativeEchoDependencies dependencies(neural);
    // The SDK environment value is copied, retaining its exact component owners.
    // Rust additionally retains the original non-native randomness/queue leases.
    auto retention = apm_echo_environment_native(**environment_);
    auto product = apm_echo_factory_create(*callbacks_->provider, retain_environment_from_sdk(environment, retention.get()), **environment_, context, dependencies, neural_overload);
    if (!product) return fail();
    auto& state = product->state();
    if (!state.control || state.rate != rate || state.render_channels != static_cast<std::size_t>(render) ||
        state.capture_channels != static_cast<std::size_t>(capture) || !state.retained_environment || !state.creation_environment ||
        !EchoEnvironmentMatches(*state.creation_environment, environment) || (state.neural && state.neural != neural) ||
        EchoProductInvalid(state)) {
      apm_echo_factory_invalid(*callbacks_->provider); return fail();
    }
    return std::make_unique<TransferredEchoControl>(std::move(product), callbacks_, std::move(health));
  }
  std::optional<rust::Box<RustEchoEnvironment>> environment_;
  std::shared_ptr<CustomCallbacks> callbacks_;
  std::shared_ptr<EchoFactoryObservation> observation_;
};
}
std::unique_ptr<NativeApmCustomModule> audio_processor_echo_factory_provider_new(rust::Box<RustApmCustomModule> provider) noexcept {
  auto state = std::make_unique<NativeApmCustomModule::State>();
  state->echo_factory_linear = true;  // per-product/output guarantee enforced before any native linear read
  state->echo_factory_observation = std::make_shared<EchoFactoryObservation>();
  auto factory = std::make_unique<CallerEchoFactory>(std::move(provider), state->status, state->echo_factory_observation);
  state->caller_echo_factory = factory.get();
  auto observed = std::make_unique<ObservedEchoFactory>(std::move(factory), state->echo_factory_observation);
  state->echo_factory_forwarder = observed.get(); state->echo_factory = std::move(observed);
  return std::make_unique<NativeApmCustomModule>(std::move(state));
}
void audio_processor_echo_factory_environment(NativeApmCustomModule& factory, rust::Box<RustEchoEnvironment> environment) noexcept {
  if (factory.state().caller_echo_factory) factory.state().caller_echo_factory->Bind(std::move(environment));
}

std::unique_ptr<NativeApmCustomModule> audio_processor_echo_detector_new(rust::Box<RustApmCustomModule> module) noexcept {
  auto state = std::make_unique<NativeApmCustomModule::State>();
  state->detector = webrtc::make_ref_counted<CallerEchoDetector>(std::move(module), state->status);
  return std::make_unique<NativeApmCustomModule>(std::move(state));
}
std::unique_ptr<NativeApmCustomModule> audio_processor_echo_detector_clone(const NativeApmCustomModule& module) noexcept {
  auto state = std::make_unique<NativeApmCustomModule::State>();
  state->status = module.state().status; state->detector = module.state().detector;
  return std::make_unique<NativeApmCustomModule>(std::move(state));
}
std::uint8_t audio_processor_echo_detector_initialize(NativeApmCustomModule& module, std::int32_t cr, std::int32_t cc, std::int32_t rr, std::int32_t rc) noexcept {
  const CallbackOperation before({module.state().status});
  module.state().detector->Initialize(cr, cc, rr, rc);
  return !before.failed() ? 0 : 18;
}
std::uint8_t audio_processor_echo_detector_analyze(NativeApmCustomModule& module, bool render, rust::Slice<const float> samples) noexcept {
  const CallbackOperation before({module.state().status});
  const auto view = std::span<const float>(samples.data(), samples.size());
  if (render) module.state().detector->AnalyzeRenderAudio(view); else module.state().detector->AnalyzeCaptureAudio(view);
  return !before.failed() ? 0 : 18;
}
FfiApmEchoDetectorMetrics audio_processor_echo_detector_metrics(NativeApmCustomModule& module) noexcept {
  const CallbackOperation before({module.state().status});
  const auto value = module.state().detector->GetMetrics();
  FfiApmEchoDetectorMetrics f{};
  f.failed = before.failed();
  f.likelihood_present = value.echo_likelihood.has_value(); f.likelihood = value.echo_likelihood.value_or(0);
  f.recent_max_present = value.echo_likelihood_recent_max.has_value(); f.recent_max = value.echo_likelihood_recent_max.value_or(0);
  return f;
}
std::unique_ptr<NativeApmCustomModule> audio_processor_custom_new(rust::Box<RustApmCustomModule> module, bool analyzer) noexcept {
  auto state = std::make_unique<NativeApmCustomModule::State>();
  if (analyzer) state->analyzer = std::make_unique<CallerCustomAnalyzer>(std::move(module), state->status);
  else state->processor = std::make_unique<CallerCustomProcessing>(std::move(module), state->status);
  return std::make_unique<NativeApmCustomModule>(std::move(state));
}
rust::Vec<std::uint8_t> audio_processor_custom_description(NativeApmCustomModule& module, std::uint8_t& error) noexcept {
  auto& state = module.state();
  const CallbackOperation before({state.status});
  const auto value = state.processor ? state.processor->ToString() : state.analyzer->ToString();
  error = !before.failed() ? 0 : 18;
  return DumpBytes(value);
}
std::uint8_t audio_processor_custom_initialize(NativeApmCustomModule& module, std::int32_t rate, std::int32_t channels) noexcept {
  auto& state = module.state();
  const CallbackOperation before({state.status});
  if (state.processor) state.processor->Initialize(rate, channels); else state.analyzer->Initialize(rate, channels);
  const bool success = !before.failed();
  state.custom_rate = success ? rate : 0; state.custom_channels = success ? channels : 0;
  return success ? 0 : 18;
}
rust::Vec<float> audio_processor_custom_audio(NativeApmCustomModule& module, std::int32_t rate,
    std::size_t channels, rust::Slice<const float> samples, bool analyze, std::uint8_t& error) noexcept {
  error = 2; rust::Vec<float> output;
  auto& state = module.state();
  const auto max = static_cast<std::size_t>(std::numeric_limits<int>::max());
  if (rate < 100 || !channels || channels > max ||
      static_cast<std::size_t>(rate / 100) > max / channels ||
      samples.size() != static_cast<std::size_t>(rate / 100) * channels ||
      state.custom_rate != rate || state.custom_channels != static_cast<std::int32_t>(channels) ||
      (analyze ? !state.analyzer : !state.processor)) return output;
  webrtc::AudioBuffer buffer(rate, channels, rate, channels, rate);
  const auto frames = buffer.num_frames();
  for (std::size_t ch = 0; ch < channels; ++ch)
    std::copy_n(samples.data() + ch * frames, frames, buffer.channels()[ch]);
  const CallbackOperation before({state.status});
  if (analyze) state.analyzer->Analyze(&buffer); else state.processor->Process(&buffer);
  error = !before.failed() ? 0 : 18;
  if (!error && !analyze) {
    output.reserve(samples.size());
    for (std::size_t ch = 0; ch < channels; ++ch)
      for (std::size_t frame = 0; frame < frames; ++frame) output.push_back(buffer.channels_const()[ch][frame]);
  }
  return output;
}
std::uint8_t audio_processor_custom_runtime(NativeApmCustomModule& module, const FfiApmRuntimeSetting& f) noexcept {
  // Only native RuntimeSetting factory preconditions apply here. Queue and
  // downstream gain-controller admission belongs to the enclosing APM, not a
  // direct caller processor receiving an otherwise opaque setting.
  if (f.kind > 8 || (f.kind == 3 && (f.int_value < 0 || f.int_value > 90)) ||
      (f.kind == 4 && !(f.float_value >= 0 && f.float_value <= 90))) return 11;
  const CallbackOperation before({module.state().status});
  module.state().processor->SetRuntimeSetting(RuntimeFromFfi(f));
  return !before.failed() ? 0 : 18;
}
struct NativeAudioProcessingModule::State {
  // Destroy the module/dumper before its worker and original environment.
  // The opaque Rust owner retains the exact queue, its factory and dependencies.
  webrtc::Environment environment;
  std::optional<rust::Box<RustApmDumpWorker>> dump_worker;
  webrtc::scoped_refptr<webrtc::AudioProcessing> module;
  bool linear_echo_configured = false;
  bool linear_output_guaranteed = false;
  PendingGains queued_gains;
  PendingGains possibly_queued_gains;
  webrtc::ProcessingConfig format_hint = webrtc::ProcessingConfig({{{16000, 1}, {16000, 1}, {16000, 1}, {16000, 1}}});
  bool capture_format_known = true;
  bool render_format_known = true;
  bool capture_graph_dirty = false;
  // Each native dumper holds its own shared copy through module destruction.
  std::shared_ptr<DumpCallbackStatus> dump_callbacks = std::make_shared<DumpCallbackStatus>();
  std::vector<std::shared_ptr<DumpCallbackStatus>> custom_callbacks;
  std::shared_ptr<std::atomic_bool> render_callback_policy;
  bool caller_echo_detector = false;
  std::vector<webrtc::EchoCanceller3Config> echo_configs;
  // The pin has a capacity-one statistics queue and a persistent read cache.
  // Track validity only, not metric values. All admitted owning capture and
  // installed snapshot operations are serialized; no extra native pull/drain.
  // Skipped metric callbacks (for example muted detector capture) leave native
  // metric fields unchanged. Their validity must persist too, independently of
  // other providers returning good metrics on a later frame.
  std::vector<std::shared_ptr<DumpCallbackStatus>> invalid_metric_sources;
  // A rejected neural constructor cannot become successful DSP on the next
  // frame merely because its private fallback configuration was safe. Track
  // only this owner's unhandled construction results until actual recreation.
  mutable std::vector<std::shared_ptr<DumpCallbackStatus>> invalid_construction_sources;
  std::optional<std::uint8_t> queued_statistics_error;
  std::uint8_t cached_statistics_error = 0;
  std::uint8_t linear_callback_error = 0;
  // Unique factory selection. Prior standalone products do not establish this
  // module's allocation state; baseline the creation count before Build.
  std::shared_ptr<EchoFactoryObservation> echo_factory_observation;
  std::uint64_t observed_echo_creations = 0;
};
namespace {
bool EchoControllerSelected(const NativeAudioProcessingModule& p, const webrtc::AudioProcessing::Config& config) {
  return bool(p.state().echo_factory_observation) || config.echo_canceller.enabled;
}
bool ApmEchoInvalid(const NativeAudioProcessingModule& p) {
  return EchoFactoryInvalid(p.state().echo_factory_observation) ||
      (EchoControllerSelected(p, p.state().module->GetConfig()) && !p.state().invalid_construction_sources.empty());
}
void RefreshSelectedEchoAllocation(NativeAudioProcessingModule& p) {
  auto& state = p.state();
  if (!state.echo_factory_observation) return;
  const auto creations = state.echo_factory_observation->creations.load(std::memory_order_acquire);
  if (creations == state.observed_echo_creations) return;
  // Poll immediately after every native entry that can reconstruct a controller.
  // The pin allocates linear storage after Create iff export is enabled then.
  // Export-only config changes without reconstruction do not alter that buffer.
  state.observed_echo_creations = creations;
  state.linear_output_guaranteed = state.module->GetConfig().echo_canceller.export_linear_aec_output;
}
webrtc::AudioProcessingStats TakeApmStatistics(NativeAudioProcessingModule& p, bool remote_overload, bool remote, std::uint8_t& error) {
  auto result = remote_overload ? p.state().module->GetStatistics(remote) : p.state().module->GetStatistics();
  if (p.state().queued_statistics_error) {
    p.state().cached_statistics_error = *p.state().queued_statistics_error;
    p.state().queued_statistics_error.reset();
  }
  error = ApmEchoInvalid(p) ? 18 : p.state().cached_statistics_error;
  return result;
}
}

struct NativeInstalledAudioProcessor::State {
  // Rust queue/clock/randomness retention must outlive final module/dump release.
  rust::Box<RustInstalledApmEnvironment> environment;
  std::unique_ptr<NativeAudioProcessingModule> processor;
  FfiApmConfig initial_config;
  std::recursive_mutex operations;
  std::atomic<std::uint8_t> pending_error{0};
  webrtc::AudioProcessing::Config last_config;
  bool capture_config_ready = false;
  bool explicit_render_ready = false, explicit_delay_ready = false;
  std::uint8_t echo_option = 0, noise_option = 0, gain_option = 0;
  void Record(std::uint8_t error) {
    if (error) { std::uint8_t empty = 0; pending_error.compare_exchange_strong(empty, error); }
  }
  State(std::unique_ptr<NativeAudioProcessingModule> p, rust::Box<RustInstalledApmEnvironment> e)
      : environment(std::move(e)), processor(std::move(p)), initial_config(audio_processor_config_get(*processor)), last_config(processor->state().module->GetConfig()) {}
  ~State() { processor.reset(); }
};
namespace {
thread_local std::vector<const NativeInstalledAudioProcessor::State*> active_installed_apm;
class InstalledApmScope {
 public:
  explicit InstalledApmScope(const NativeInstalledAudioProcessor::State* state) { active_installed_apm.push_back(state); }
  ~InstalledApmScope() { active_installed_apm.pop_back(); }
};
// Every SDK ref retains the whole owning graph. The underlying selected module
// is never recreated or substituted. Caller-driven capture uses admitted core
// operations and a publication gate. Raw DSP forwarding is not a physical-device
// failure channel; that combination is explicitly unqualified and not enabled.
class RetainedAudioProcessor : public webrtc::AudioProcessing {
 public:
  explicit RetainedAudioProcessor(std::shared_ptr<NativeInstalledAudioProcessor::State> owner) : owner_(std::move(owner)) {}
  int Initialize() override {
    std::lock_guard lock(owner_->operations);
    InstalledApmScope scope(owner_.get());
    const auto before = DumpGeneration(*owner_->processor);
    const int status = Core().Initialize();
    NoteInitialize(); owner_->Record(DumpOutcome(*owner_->processor, before)); return status;
  }
  int Initialize(const webrtc::ProcessingConfig& config) override {
    std::lock_guard lock(owner_->operations);
    InstalledApmScope scope(owner_.get());
    const auto before = DumpGeneration(*owner_->processor);
    const int status = Core().Initialize(config);
    NoteInitialize(); owner_->Record(DumpOutcome(*owner_->processor, before)); return status;
  }
  void ApplyConfig(const Config& config) override {
    if (installed_audio_processor_reentry(owner_)) return;
    std::lock_guard lock(owner_->operations); InstalledApmScope scope(owner_.get());
    owner_->capture_config_ready = false;
    const auto error = audio_processor_config_apply(*owner_->processor, ConfigToFfi(config));
    owner_->Record(error); owner_->last_config = Core().GetConfig();
  }
  int proc_sample_rate_hz() const override { return Core().proc_sample_rate_hz(); }
  int proc_split_sample_rate_hz() const override { return Core().proc_split_sample_rate_hz(); }
  std::size_t num_input_channels() const override { return Core().num_input_channels(); }
  std::size_t num_proc_channels() const override { return Core().num_proc_channels(); }
  std::size_t num_output_channels() const override { return Core().num_output_channels(); }
  std::size_t num_reverse_channels() const override { return Core().num_reverse_channels(); }
  void set_output_will_be_muted(bool muted) override { Core().set_output_will_be_muted(muted); }
  bool get_output_will_be_muted() override { return Core().get_output_will_be_muted(); }
  void SetRuntimeSetting(RuntimeSetting setting) override {
    if (installed_audio_processor_reentry(owner_)) return;
    std::lock_guard lock(owner_->operations); InstalledApmScope scope(owner_.get());
    std::uint8_t error = 0;
    audio_processor_runtime_setting(*owner_->processor, RuntimeSnapshot(setting), false, error); owner_->Record(error);
  }
  bool PostRuntimeSetting(RuntimeSetting setting) override {
    if (installed_audio_processor_reentry(owner_)) return false;
    std::lock_guard lock(owner_->operations); InstalledApmScope scope(owner_.get());
    std::uint8_t error = 0;
    const bool accepted = audio_processor_runtime_setting(*owner_->processor, RuntimeSnapshot(setting), true, error); owner_->Record(error); return accepted;
  }
  int ProcessStream(const std::int16_t* src, const webrtc::StreamConfig& input, const webrtc::StreamConfig& output, std::int16_t* dest) override {
    std::lock_guard lock(owner_->operations); InstalledApmScope scope(owner_.get());
    const auto operation = DumpGeneration(*owner_->processor);
    const int status = Core().ProcessStream(src, input, output, dest);
    const auto error = DumpOutcome(*owner_->processor, operation);
    NoteCaptureResult(*owner_->processor, status, error, operation); owner_->Record(error);
    return status;
  }
  int ProcessStream(const float* const* src, const webrtc::StreamConfig& input, const webrtc::StreamConfig& output, float* const* dest) override {
    std::lock_guard lock(owner_->operations); InstalledApmScope scope(owner_.get());
    const auto operation = DumpGeneration(*owner_->processor);
    const int status = Core().ProcessStream(src, input, output, dest);
    const auto error = DumpOutcome(*owner_->processor, operation);
    NoteCaptureResult(*owner_->processor, status, error, operation); owner_->Record(error);
    return status;
  }
  int ProcessReverseStream(const std::int16_t* src, const webrtc::StreamConfig& input, const webrtc::StreamConfig& output, std::int16_t* dest) override {
    return Reverse([&] { return Core().ProcessReverseStream(src, input, output, dest); });
  }
  int ProcessReverseStream(const float* const* src, const webrtc::StreamConfig& input, const webrtc::StreamConfig& output, float* const* dest) override {
    return Reverse([&] { return Core().ProcessReverseStream(src, input, output, dest); });
  }
  int AnalyzeReverseStream(const float* const* data, const webrtc::StreamConfig& format) override {
    return Reverse([&] { return Core().AnalyzeReverseStream(data, format); });
  }
  bool GetLinearAecOutput(std::span<std::array<float, 160>> output) const override {
    if (installed_audio_processor_reentry(owner_)) return false;
    std::lock_guard lock(owner_->operations); InstalledApmScope scope(owner_.get());
    RefreshSelectedEchoAllocation(*owner_->processor);
    const auto& state = owner_->processor->state();
    const auto error = ApmEchoInvalid(*owner_->processor) ? 18 : state.linear_callback_error;
    if (error) { owner_->Record(error); return false; }
    return Core().GetLinearAecOutput(output);
  }
  void set_stream_analog_level(int value) override { Core().set_stream_analog_level(value); }
  int recommended_stream_analog_level() const override { return Core().recommended_stream_analog_level(); }
  int set_stream_delay_ms(int value) override { return Core().set_stream_delay_ms(value); }
  int stream_delay_ms() const override { return Core().stream_delay_ms(); }
  void set_stream_key_pressed(bool value) override { Core().set_stream_key_pressed(value); }
  bool CreateAndAttachAecDump(absl::string_view path, std::int64_t maximum, webrtc::TaskQueueBase* worker) override { return Core().CreateAndAttachAecDump(path, maximum, worker); }
  bool CreateAndAttachAecDump(FILE* file, std::int64_t maximum, webrtc::TaskQueueBase* worker) override { return Core().CreateAndAttachAecDump(file, maximum, worker); }
  void AttachAecDump(std::unique_ptr<webrtc::AecDump> dump) override { Core().AttachAecDump(std::move(dump)); }
  void DetachAecDump() override { Core().DetachAecDump(); }
  webrtc::AudioProcessingStats GetStatistics() override { return Statistics(false, false); }
  webrtc::AudioProcessingStats GetStatistics(bool remote) override { return Statistics(true, remote); }
  Config GetConfig() const override {
    if (installed_audio_processor_reentry(owner_)) {
      // Drain-only SDK reentry snapshot; the outer operation is marked failed.
      // Public factory snapshots refuse reentry before reaching this fallback.
      return owner_->last_config;
    }
    std::lock_guard lock(owner_->operations); return Core().GetConfig();
  }
 private:
  template <typename F> int Reverse(F&& invoke) {
    std::lock_guard lock(owner_->operations); InstalledApmScope scope(owner_.get());
    const auto operation = DumpGeneration(*owner_->processor);
    const int status = invoke();
    RefreshSelectedEchoAllocation(*owner_->processor);
    owner_->Record(DumpOutcome(*owner_->processor, operation));
    return status;
  }
  webrtc::AudioProcessingStats Statistics(bool overload, bool remote) {
    if (installed_audio_processor_reentry(owner_)) return {};
    std::lock_guard lock(owner_->operations); InstalledApmScope scope(owner_.get());
    std::uint8_t error = 0;
    auto result = TakeApmStatistics(*owner_->processor, overload, remote, error);
    owner_->Record(error);
    // SDK-only drain. The retained owner's failure channel, not native optional
    // absence, identifies a failed snapshot. Public peer stats still need an
    // operation-paired projection before this facade is fully qualified.
    return error ? webrtc::AudioProcessingStats{} : result;
  }
  void NoteInitialize() {
    auto& state = owner_->processor->state();
    state.capture_format_known = false; state.render_format_known = false; state.capture_graph_dirty = true;
    RefreshSelectedEchoAllocation(*owner_->processor);
  }
  webrtc::AudioProcessing& Core() const { return *owner_->processor->state().module; }
  std::shared_ptr<NativeInstalledAudioProcessor::State> owner_;
};
}
NativeInstalledAudioProcessor::NativeInstalledAudioProcessor(std::shared_ptr<State> state) noexcept
    : state_(std::move(state)), module_(webrtc::make_ref_counted<RetainedAudioProcessor>(state_)) {}
NativeInstalledAudioProcessor::~NativeInstalledAudioProcessor() = default;
const std::shared_ptr<NativeInstalledAudioProcessor::State>& NativeInstalledAudioProcessor::state() const noexcept { return state_; }
webrtc::scoped_refptr<webrtc::AudioProcessing> NativeInstalledAudioProcessor::module() const noexcept { return module_; }
std::unique_ptr<NativeInstalledAudioProcessor> audio_processor_into_installed(std::unique_ptr<NativeAudioProcessingModule> processor, rust::Box<RustInstalledApmEnvironment> environment, std::uint8_t& error) noexcept {
  error = !processor ? 2 : (RuntimeReinitializationUnsafe(*processor) ? 12 : 0);
  if (error) return nullptr;
  return std::make_unique<NativeInstalledAudioProcessor>(std::make_shared<NativeInstalledAudioProcessor::State>(std::move(processor), std::move(environment)));
}
NativeAudioProcessingModule& installed_audio_processor_core(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner) noexcept { return *owner->processor; }
FfiApmConfig installed_audio_processor_original_config(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner) noexcept { return owner->initial_config; }
bool installed_audio_processor_has_echo_factory(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner, std::uint8_t& error) noexcept {
  error = !owner ? 22 : (installed_audio_processor_reentry(owner) ? 21 : 0);
  if (error) return false;
  std::lock_guard lock(owner->operations);
  error = owner->pending_error.exchange(0);
  if (!error && ApmEchoInvalid(*owner->processor)) error = 18;
  return !error && bool(owner->processor->state().echo_factory_observation);
}
bool installed_audio_processor_reentry(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner) noexcept {
  if (!owner || std::find(active_installed_apm.begin(), active_installed_apm.end(), owner.get()) == active_installed_apm.end()) return false;
  owner->Record(21); return true;
}
std::uint8_t installed_audio_processor_restore(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner) noexcept {
  if (installed_audio_processor_reentry(owner)) return 21;
  std::lock_guard lock(owner->operations); InstalledApmScope scope(owner.get());
  const auto previous_error = owner->pending_error.exchange(0);
  const auto error = audio_processor_config_apply(*owner->processor, owner->initial_config);
  owner->last_config = owner->processor->state().module->GetConfig();
  owner->capture_config_ready = !error;
  const auto reentry = owner->pending_error.exchange(0);
  return previous_error ? previous_error : (error ? error : reentry);
}
std::int32_t installed_audio_processor_capture(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner,
    std::int32_t rate, std::size_t channels, rust::Slice<std::int16_t> samples,
    std::uint8_t echo, std::uint8_t noise, std::uint8_t gain, bool muted,
    bool& needs_feedback, std::uint8_t& error) noexcept {
  needs_feedback = false; error = 0;
  if (installed_audio_processor_reentry(owner)) { error = 21; return 0; }
  std::lock_guard lock(owner->operations); InstalledApmScope scope(owner.get());
  error = owner->pending_error.exchange(0); if (error) return 0;
  auto config = ConfigFromFfi(owner->initial_config);
  if (echo) config.echo_canceller.enabled = echo == 2;
  if (noise) config.noise_suppression.enabled = noise == 2;
  if (gain) {
    const bool enabled = gain == 2;
    if (!enabled) { config.gain_controller1.enabled = false; config.gain_controller2.enabled = false; }
    else if (!config.gain_controller1.enabled && !config.gain_controller2.enabled) config.gain_controller1.enabled = true;
  }
  if ((EchoControllerSelected(*owner->processor, config) && (!owner->explicit_render_ready || !owner->explicit_delay_ready)) || (config.gain_controller1.enabled &&
      (config.gain_controller1.analog_gain_controller.enabled ||
       config.gain_controller1.mode == webrtc::AudioProcessing::Config::GainController1::kAdaptiveAnalog))) {
    needs_feedback = true; return 0;
  }
  if (!owner->capture_config_ready || echo != owner->echo_option || noise != owner->noise_option || gain != owner->gain_option) {
    error = audio_processor_config_apply(*owner->processor, ConfigToFfi(config));
    owner->last_config = owner->processor->state().module->GetConfig();
    if (error) return 0;
    owner->capture_config_ready = true; owner->echo_option = echo; owner->noise_option = noise; owner->gain_option = gain;
  }
  audio_processor_set_muted(*owner->processor, muted);
  const auto status = audio_processor_i16(*owner->processor, rate, channels, samples, false, error);
  const auto nested_error = owner->pending_error.exchange(0);
  if (!error) error = nested_error;
  owner->last_config = owner->processor->state().module->GetConfig();
  if (!error && status == webrtc::AudioProcessing::kNoError) {
    // Preserve native runtime-setting consumption without promoting one
    // source's temporary enable flags into the factory's requested defaults.
    auto desired = owner->last_config;
    const auto base = ConfigFromFfi(owner->initial_config);
    desired.echo_canceller.enabled = base.echo_canceller.enabled;
    desired.noise_suppression.enabled = base.noise_suppression.enabled;
    desired.gain_controller1.enabled = base.gain_controller1.enabled;
    desired.gain_controller2.enabled = base.gain_controller2.enabled;
    owner->initial_config = ConfigToFfi(desired);
  }
  return status;
}

FfiApmConfig installed_audio_processor_config(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner, std::uint8_t& error) noexcept {
  error = !owner ? 22 : (installed_audio_processor_reentry(owner) ? 21 : 0);
  if (error) return {};
  std::lock_guard lock(owner->operations);
  error = owner->pending_error.exchange(0);
  if (!error && ApmEchoInvalid(*owner->processor)) error = 18;
  if (error) return {};
  return audio_processor_config_get(*owner->processor);
}
std::uint8_t installed_audio_processor_apply(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner, const FfiApmConfig& config) noexcept {
  if (!owner) return 22;
  if (installed_audio_processor_reentry(owner)) return 21;
  std::lock_guard lock(owner->operations); InstalledApmScope scope(owner.get());
  if (const auto pending = owner->pending_error.exchange(0)) return pending;
  owner->capture_config_ready = false;
  const auto error = audio_processor_config_apply(*owner->processor, config);
  owner->last_config = owner->processor->state().module->GetConfig();
  const auto nested = owner->pending_error.exchange(0);
  if (error || nested) return error ? error : nested;
  // This explicit application replaces the desired config restored at subsequent
  // caller capture, not merely an SDK option value overwritten on the next frame.
  owner->initial_config = ConfigToFfi(owner->last_config); owner->capture_config_ready = true;
  owner->echo_option = owner->noise_option = owner->gain_option = 0;
  return 0;
}
std::int32_t installed_audio_processor_reverse(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner,
    std::int32_t rate, std::size_t channels, rust::Slice<std::int16_t> samples, std::uint8_t& error) noexcept {
  error = !owner ? 22 : (installed_audio_processor_reentry(owner) ? 21 : 0);
  if (error) return 0;
  std::lock_guard lock(owner->operations); InstalledApmScope scope(owner.get());
  error = owner->pending_error.exchange(0); if (error) return 0;
  owner->explicit_render_ready = false;
  // Validate before native prefix iteration/allocation. The ordinary i16 API
  // accepts nonnative formats as native errors; its prefix arithmetic still
  // requires nonnegative representable rates/channels and initialized storage.
  if (rate < 0 || channels > static_cast<std::size_t>(std::numeric_limits<std::uint16_t>::max()) ||
      (channels && static_cast<std::size_t>(rate / 100) > std::numeric_limits<int>::max() / channels)) { error = 2; return 0; }
  const auto count = static_cast<std::size_t>(rate / 100) * channels;
  if (samples.size() < count) { error = 6; return 0; }
  std::vector<std::int16_t> working(samples.begin(), samples.begin() + count);
  std::int16_t empty = 0;
  const auto status = audio_processor_i16(*owner->processor, rate, channels,
      rust::Slice<std::int16_t>(working.empty() ? &empty : working.data(), working.size()), true, error);
  const auto nested = owner->pending_error.exchange(0); if (!error) error = nested;
  if (!error) std::copy(working.begin(), working.end(), samples.begin());
  owner->explicit_render_ready = !error && status == webrtc::AudioProcessing::kNoError;
  return status;
}
std::int32_t installed_audio_processor_delay(const std::shared_ptr<NativeInstalledAudioProcessor::State>& owner,
    std::int32_t delay, std::uint8_t& error) noexcept {
  error = !owner ? 22 : (installed_audio_processor_reentry(owner) ? 21 : 0);
  if (error) return 0;
  std::lock_guard lock(owner->operations);
  error = owner->pending_error.exchange(0); if (error) return 0;
  const auto status = audio_processor_set_delay(*owner->processor, delay);
  // This native setter clamps to its valid domain and preserves its warning.
  owner->explicit_delay_ready = status == webrtc::AudioProcessing::kNoError ||
      status == webrtc::AudioProcessing::kBadStreamParameterWarning;
  return status;
}

NativeAudioProcessingModule::NativeAudioProcessingModule(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeAudioProcessingModule::~NativeAudioProcessingModule() = default;
NativeAudioProcessingModule::State& NativeAudioProcessingModule::state() noexcept { return *state_; }
const NativeAudioProcessingModule::State& NativeAudioProcessingModule::state() const noexcept { return *state_; }
namespace {
CallbackOperation DumpGeneration(const NativeAudioProcessingModule& p) {
  auto statuses = p.state().custom_callbacks;
  statuses.push_back(p.state().dump_callbacks);
  return CallbackOperation(std::move(statuses));
}
void UpdateRenderCallbackPolicy(NativeAudioProcessingModule& p, const webrtc::AudioProcessing::Config& c) {
  if (p.state().render_callback_policy) p.state().render_callback_policy->store(p.state().caller_echo_detector || EchoControllerSelected(p, c) || c.gain_controller1.enabled, std::memory_order_relaxed);
}
std::uint8_t DumpOutcome(const NativeAudioProcessingModule& p, const CallbackOperation& operation) {
  auto& invalid = p.state().invalid_construction_sources;
  for (const auto& [status, success] : operation.construction_results()) {
    const auto found = std::find(invalid.begin(), invalid.end(), status);
    if (success) { if (found != invalid.end()) invalid.erase(found); }
    else if (found == invalid.end()) invalid.push_back(status);
  }
  return operation.failed() || ApmEchoInvalid(p) ? 18 : 0;
}
void SelectEffects(webrtc::AudioProcessing::Config& config, const FfiAudioProcessingConfig& selected) {
  config.echo_canceller.enabled = selected.echo_cancellation;
  config.noise_suppression.enabled = selected.noise_suppression != 0;
  if (selected.noise_suppression != 0) config.noise_suppression.level = static_cast<webrtc::AudioProcessing::Config::NoiseSuppression::Level>(selected.noise_suppression - 1);
  config.gain_controller1.enabled = selected.gain_control >= 1 && selected.gain_control <= 3;
  if (config.gain_controller1.enabled) {
    config.gain_controller1.mode = static_cast<webrtc::AudioProcessing::Config::GainController1::Mode>(selected.gain_control - 1);
    // The pin selects the analog manager by this flag, not by Mode.
    config.gain_controller1.analog_gain_controller.enabled = selected.gain_control == 1;
  }
  config.gain_controller2.enabled = selected.gain_control == 4;
  if (config.gain_controller2.enabled) config.gain_controller2.adaptive_digital.enabled = true;
}
// Even zero-length configurations get real, nonnull pointers for native
// DCHECKs. Rust supplies checked channel-major spans; no output is invented.
std::vector<const float*> InputPointers(rust::Slice<const float> samples, const webrtc::StreamConfig& config, const float* empty) {
  std::vector<const float*> result(std::max(std::size_t{1}, config.num_channels()), empty);
  if (config.num_frames() != 0) for (std::size_t ch = 0; ch < config.num_channels(); ++ch) result[ch] = samples.data() + ch * config.num_frames();
  return result;
}
std::vector<float*> OutputPointers(rust::Slice<float> samples, const webrtc::StreamConfig& config, float* empty) {
  std::vector<float*> result(std::max(std::size_t{1}, config.num_channels()), empty);
  if (config.num_frames() != 0) for (std::size_t ch = 0; ch < config.num_channels(); ++ch) result[ch] = samples.data() + ch * config.num_frames();
  return result;
}
}
std::unique_ptr<NativeAudioProcessingModule> audio_processor_new(const NativeEnvironment& environment, const FfiAudioProcessingConfig& selected) noexcept {
  if (selected.noise_suppression > 4 || selected.gain_control > 4) return nullptr;
  webrtc::AudioProcessing::Config config;
  SelectEffects(config, selected);
  auto state = std::make_unique<NativeAudioProcessingModule::State>(NativeAudioProcessingModule::State{environment.environment(), std::nullopt, nullptr});
  state->module = webrtc::BuiltinAudioProcessingBuilder(config).Build(state->environment);
  if (!state->module) return nullptr;
  return std::make_unique<NativeAudioProcessingModule>(std::move(state));
}
namespace {
// Use the SDK's actual token dispatch, including keyless values, duplicate
// parameters, percent units and ignored tokens. Check scanf representation
// before the SDK parser; do not edit, mask or replace the environment trials.
enum class NumericKind { Double, Int64, Int32 };
bool NumericRepresentable(const std::string& text, NumericKind kind) {
  char* end = nullptr;
  errno = 0;
  if (kind != NumericKind::Double) {
    const auto value = std::strtoll(text.c_str(), &end, 10);
    if (end == text.c_str()) return true;  // Native conversion also fails.
    if (errno == ERANGE) return false;
    return kind != NumericKind::Int32 ||
        (value >= std::numeric_limits<std::int32_t>::min() &&
         value <= std::numeric_limits<std::int32_t>::max());
  }
  double value = std::strtod(text.c_str(), &end);
  if (end == text.c_str()) return true;
  if (!std::isfinite(value)) return false;
  while (*end && std::isspace(static_cast<unsigned char>(*end))) ++end;
  if (*end == '%') value /= 100;
  return std::abs(value) <= std::numeric_limits<float>::max();
}
class NumericTrialCheck final : public webrtc::FieldTrialParameterInterface {
 public:
  NumericTrialCheck(const char* key, NumericKind kind)
      : FieldTrialParameterInterface(key), kind_(kind) {}
  bool safe = true;
 private:
  bool Parse(std::optional<std::string> value) override {
    if (value) safe = NumericRepresentable(*value, kind_) && safe;
    return true;
  }
  NumericKind kind_;
};
bool SafeEchoConfig(const webrtc::EchoCanceller3Config& config, const webrtc::FieldTrialsView& trials) {
  // Validate copies only. Never normalize the values consumed by the selected
  // native factory. Numeric trial parsing is admitted separately before this.
  auto copy = config;
  if (!webrtc::EchoCanceller3Config::Validate(&copy)) return false;
  const auto adjusted = webrtc::AdjustConfig(config, trials);
  copy = adjusted;
  if (!webrtc::EchoCanceller3Config::Validate(&copy)) return false;
  for (const auto* tuning : {&adjusted.suppressor.nearend_tuning, &adjusted.suppressor.normal_tuning}) {
    if (!(tuning->mask_lf.enr_transparent < tuning->mask_lf.enr_suppress) ||
        !(tuning->mask_hf.enr_transparent < tuning->mask_hf.enr_suppress)) return false;
  }
  return true;
}
bool SafeAecTrials(const webrtc::FieldTrialsView& trials) {
  NumericTrialCheck default_len("default_len", NumericKind::Double);
  NumericTrialCheck nearend_len("nearend_len", NumericKind::Double);
  webrtc::ParseFieldTrial({&default_len, &nearend_len},
      trials.Lookup("WebRTC-Aec3UseNearendReverbLen"));
  if (!default_len.safe || !nearend_len.safe) return false;
  NumericTrialCheck p0("nearend_tuning_mask_lf_enr_transparent", NumericKind::Double);
  NumericTrialCheck p1("nearend_tuning_mask_lf_enr_suppress", NumericKind::Double);
  NumericTrialCheck p2("nearend_tuning_mask_hf_enr_transparent", NumericKind::Double);
  NumericTrialCheck p3("nearend_tuning_mask_hf_enr_suppress", NumericKind::Double);
  NumericTrialCheck p4("nearend_tuning_max_inc_factor", NumericKind::Double);
  NumericTrialCheck p5("nearend_tuning_max_dec_factor_lf", NumericKind::Double);
  NumericTrialCheck p6("normal_tuning_mask_lf_enr_transparent", NumericKind::Double);
  NumericTrialCheck p7("normal_tuning_mask_lf_enr_suppress", NumericKind::Double);
  NumericTrialCheck p8("normal_tuning_mask_hf_enr_transparent", NumericKind::Double);
  NumericTrialCheck p9("normal_tuning_mask_hf_enr_suppress", NumericKind::Double);
  NumericTrialCheck p10("normal_tuning_max_inc_factor", NumericKind::Double);
  NumericTrialCheck p11("normal_tuning_max_dec_factor_lf", NumericKind::Double);
  NumericTrialCheck p12("dominant_nearend_detection_enr_threshold", NumericKind::Double);
  NumericTrialCheck p13("dominant_nearend_detection_enr_exit_threshold", NumericKind::Double);
  NumericTrialCheck p14("dominant_nearend_detection_snr_threshold", NumericKind::Double);
  NumericTrialCheck p15("dominant_nearend_detection_hold_duration", NumericKind::Int64);
  NumericTrialCheck p16("dominant_nearend_detection_trigger_threshold", NumericKind::Int64);
  webrtc::ParseFieldTrial({&p0, &p1, &p2, &p3, &p4, &p5, &p6, &p7, &p8, &p9, &p10, &p11, &p12, &p13, &p14, &p15, &p16},
      trials.Lookup("WebRTC-Aec3SuppressorTuningOverride"));
  if (!(p0.safe && p1.safe && p2.safe && p3.safe && p4.safe && p5.safe && p6.safe && p7.safe && p8.safe && p9.safe && p10.safe && p11.safe && p12.safe && p13.safe && p14.safe && p15.safe && p16.safe)) return false;
  const char* floating_trials[] = {
    "WebRTC-Aec3SuppressorNearendLfMaskTransparentOverride",
    "WebRTC-Aec3SuppressorNearendLfMaskSuppressOverride",
    "WebRTC-Aec3SuppressorNearendHfMaskTransparentOverride",
    "WebRTC-Aec3SuppressorNearendHfMaskSuppressOverride",
    "WebRTC-Aec3SuppressorNearendMaxIncFactorOverride",
    "WebRTC-Aec3SuppressorNearendMaxDecFactorLfOverride",
    "WebRTC-Aec3SuppressorNormalLfMaskTransparentOverride",
    "WebRTC-Aec3SuppressorNormalLfMaskSuppressOverride",
    "WebRTC-Aec3SuppressorNormalHfMaskTransparentOverride",
    "WebRTC-Aec3SuppressorNormalHfMaskSuppressOverride",
    "WebRTC-Aec3SuppressorNormalMaxIncFactorOverride",
    "WebRTC-Aec3SuppressorNormalMaxDecFactorLfOverride",
    "WebRTC-Aec3SuppressorDominantNearendEnrThresholdOverride",
    "WebRTC-Aec3SuppressorDominantNearendEnrExitThresholdOverride",
    "WebRTC-Aec3SuppressorDominantNearendSnrThresholdOverride",
    "WebRTC-Aec3SuppressorAntiHowlingGainOverride",
    "WebRTC-Aec3DelayEstimateSmoothingOverride",
    "WebRTC-Aec3DelayEstimateSmoothingDelayFoundOverride",
  };
  for (const auto* key : floating_trials) {
    NumericTrialCheck p("", NumericKind::Double);
    webrtc::ParseFieldTrial({&p}, trials.Lookup(key));
    if (!p.safe) return false;
  }
  const char* integer_trials[] = {
    "WebRTC-Aec3SuppressorDominantNearendHoldDurationOverride",
    "WebRTC-Aec3SuppressorDominantNearendTriggerThresholdOverride",
    "WebRTC-Aec3BufferingMaxAllowedExcessRenderBlocksOverride",
  };
  for (const auto* key : integer_trials) {
    NumericTrialCheck p("", NumericKind::Int64);
    webrtc::ParseFieldTrial({&p}, trials.Lookup(key));
    if (!p.safe) return false;
  }
  // This exact SDK adjustment is a private preflight, not a public DSP API or
  // an alternate configuration. The real builtin still consumes original trials.
  auto adjusted = webrtc::AdjustConfig(webrtc::EchoCanceller3Config{}, trials);
  auto copy = adjusted;
  if (!webrtc::EchoCanceller3Config::Validate(&copy)) return false;
  for (const auto* tuning : {&adjusted.suppressor.nearend_tuning,
                            &adjusted.suppressor.normal_tuning}) {
    if (!(tuning->mask_lf.enr_transparent < tuning->mask_lf.enr_suppress) ||
        !(tuning->mask_hf.enr_transparent < tuning->mask_hf.enr_suppress))
      return false;
  }
  return true;
}
}  // namespace
bool audio_processor_effects_admitted(const NativeEnvironment& environment, const FfiAudioProcessingConfig& selected) noexcept {
  const auto env = environment.environment();
  const auto& trials = env.field_trials();
  if (selected.echo_cancellation && !SafeAecTrials(trials)) return false;
  if (selected.gain_control == 1 &&
      trials.IsEnabled("WebRTC-Audio-2ndAgcMinMicLevelExperiment")) {
    const auto group = trials.Lookup("WebRTC-Audio-2ndAgcMinMicLevelExperiment");
    if (group.starts_with("Enabled-") &&
        !NumericRepresentable(group.substr(8), NumericKind::Int32))
      return false;
  }
  return true;
}
namespace {
webrtc::AudioProcessing::Config ConfigFromFfi(const FfiApmConfig& v) {
  webrtc::AudioProcessing::Config config;
  config.pipeline.maximum_internal_processing_rate = v.pipeline_maximum_internal_processing_rate;
  config.pipeline.multi_channel_render = v.pipeline_multi_channel_render;
  config.pipeline.multi_channel_capture = v.pipeline_multi_channel_capture;
  config.pipeline.capture_downmix_method = static_cast<webrtc::AudioProcessing::Config::Pipeline::DownmixMethod>(v.pipeline_capture_downmix_method);
  config.pipeline.capture_downmix_method_stereo_aec = static_cast<webrtc::AudioProcessing::Config::Pipeline::DownmixMethod>(v.pipeline_capture_downmix_method_stereo_aec);
  config.pre_amplifier.enabled = v.pre_amplifier_enabled;
  config.pre_amplifier.fixed_gain_factor = v.pre_amplifier_fixed_gain_factor;
  config.capture_level_adjustment.enabled = v.capture_level_adjustment_enabled;
  config.capture_level_adjustment.pre_gain_factor = v.capture_level_adjustment_pre_gain_factor;
  config.capture_level_adjustment.post_gain_factor = v.capture_level_adjustment_post_gain_factor;
  config.capture_level_adjustment.analog_mic_gain_emulation.enabled = v.capture_level_adjustment_analog_mic_gain_emulation_enabled;
  config.capture_level_adjustment.analog_mic_gain_emulation.initial_level = v.capture_level_adjustment_analog_mic_gain_emulation_initial_level;
  config.high_pass_filter.enabled = v.high_pass_filter_enabled;
  config.high_pass_filter.apply_in_full_band = v.high_pass_filter_apply_in_full_band;
  config.echo_canceller.enabled = v.echo_canceller_enabled;
  config.echo_canceller.export_linear_aec_output = v.echo_canceller_export_linear_aec_output;
  config.echo_canceller.enforce_high_pass_filtering = v.echo_canceller_enforce_high_pass_filtering;
  config.noise_suppression.enabled = v.noise_suppression_enabled;
  config.noise_suppression.level = static_cast<webrtc::AudioProcessing::Config::NoiseSuppression::Level>(v.noise_suppression_level);
  config.noise_suppression.analyze_linear_aec_output_when_available = v.noise_suppression_analyze_linear_aec_output_when_available;
  config.transient_suppression.enabled = v.transient_suppression_enabled;
  config.gain_controller1.enabled = v.gain_controller1_enabled;
  config.gain_controller1.mode = static_cast<webrtc::AudioProcessing::Config::GainController1::Mode>(v.gain_controller1_mode);
  config.gain_controller1.target_level_dbfs = v.gain_controller1_target_level_dbfs;
  config.gain_controller1.compression_gain_db = v.gain_controller1_compression_gain_db;
  config.gain_controller1.enable_limiter = v.gain_controller1_enable_limiter;
  config.gain_controller1.analog_gain_controller.enabled = v.gain_controller1_analog_gain_controller_enabled;
  config.gain_controller1.analog_gain_controller.startup_min_volume = v.gain_controller1_analog_gain_controller_startup_min_volume;
  config.gain_controller1.analog_gain_controller.clipped_level_min = v.gain_controller1_analog_gain_controller_clipped_level_min;
  config.gain_controller1.analog_gain_controller.enable_digital_adaptive = v.gain_controller1_analog_gain_controller_enable_digital_adaptive;
  config.gain_controller1.analog_gain_controller.clipped_level_step = v.gain_controller1_analog_gain_controller_clipped_level_step;
  config.gain_controller1.analog_gain_controller.clipped_ratio_threshold = v.gain_controller1_analog_gain_controller_clipped_ratio_threshold;
  config.gain_controller1.analog_gain_controller.clipped_wait_frames = v.gain_controller1_analog_gain_controller_clipped_wait_frames;
  config.gain_controller1.analog_gain_controller.clipping_predictor.enabled = v.gain_controller1_analog_gain_controller_clipping_predictor_enabled;
  config.gain_controller1.analog_gain_controller.clipping_predictor.mode = static_cast<webrtc::AudioProcessing::Config::GainController1::AnalogGainController::ClippingPredictor::Mode>(v.gain_controller1_analog_gain_controller_clipping_predictor_mode);
  config.gain_controller1.analog_gain_controller.clipping_predictor.window_length = v.gain_controller1_analog_gain_controller_clipping_predictor_window_length;
  config.gain_controller1.analog_gain_controller.clipping_predictor.reference_window_length = v.gain_controller1_analog_gain_controller_clipping_predictor_reference_window_length;
  config.gain_controller1.analog_gain_controller.clipping_predictor.reference_window_delay = v.gain_controller1_analog_gain_controller_clipping_predictor_reference_window_delay;
  config.gain_controller1.analog_gain_controller.clipping_predictor.clipping_threshold = v.gain_controller1_analog_gain_controller_clipping_predictor_clipping_threshold;
  config.gain_controller1.analog_gain_controller.clipping_predictor.crest_factor_margin = v.gain_controller1_analog_gain_controller_clipping_predictor_crest_factor_margin;
  config.gain_controller1.analog_gain_controller.clipping_predictor.use_predicted_step = v.gain_controller1_analog_gain_controller_clipping_predictor_use_predicted_step;
  config.gain_controller2.enabled = v.gain_controller2_enabled;
  config.gain_controller2.input_volume_controller.enabled = v.gain_controller2_input_volume_controller_enabled;
  config.gain_controller2.adaptive_digital.enabled = v.gain_controller2_adaptive_digital_enabled;
  config.gain_controller2.adaptive_digital.headroom_db = v.gain_controller2_adaptive_digital_headroom_db;
  config.gain_controller2.adaptive_digital.max_gain_db = v.gain_controller2_adaptive_digital_max_gain_db;
  config.gain_controller2.adaptive_digital.initial_gain_db = v.gain_controller2_adaptive_digital_initial_gain_db;
  config.gain_controller2.adaptive_digital.max_gain_change_db_per_second = v.gain_controller2_adaptive_digital_max_gain_change_db_per_second;
  config.gain_controller2.adaptive_digital.max_output_noise_level_dbfs = v.gain_controller2_adaptive_digital_max_output_noise_level_dbfs;
  config.gain_controller2.fixed_digital.gain_db = v.gain_controller2_fixed_digital_gain_db;
  return config;
}
FfiApmConfig ConfigToFfi(const webrtc::AudioProcessing::Config& config) {
  FfiApmConfig v;
  v.pipeline_maximum_internal_processing_rate = config.pipeline.maximum_internal_processing_rate;
  v.pipeline_multi_channel_render = config.pipeline.multi_channel_render;
  v.pipeline_multi_channel_capture = config.pipeline.multi_channel_capture;
  v.pipeline_capture_downmix_method = static_cast<std::int32_t>(config.pipeline.capture_downmix_method);
  v.pipeline_capture_downmix_method_stereo_aec = static_cast<std::int32_t>(config.pipeline.capture_downmix_method_stereo_aec);
  v.pre_amplifier_enabled = config.pre_amplifier.enabled;
  v.pre_amplifier_fixed_gain_factor = config.pre_amplifier.fixed_gain_factor;
  v.capture_level_adjustment_enabled = config.capture_level_adjustment.enabled;
  v.capture_level_adjustment_pre_gain_factor = config.capture_level_adjustment.pre_gain_factor;
  v.capture_level_adjustment_post_gain_factor = config.capture_level_adjustment.post_gain_factor;
  v.capture_level_adjustment_analog_mic_gain_emulation_enabled = config.capture_level_adjustment.analog_mic_gain_emulation.enabled;
  v.capture_level_adjustment_analog_mic_gain_emulation_initial_level = config.capture_level_adjustment.analog_mic_gain_emulation.initial_level;
  v.high_pass_filter_enabled = config.high_pass_filter.enabled;
  v.high_pass_filter_apply_in_full_band = config.high_pass_filter.apply_in_full_band;
  v.echo_canceller_enabled = config.echo_canceller.enabled;
  v.echo_canceller_export_linear_aec_output = config.echo_canceller.export_linear_aec_output;
  v.echo_canceller_enforce_high_pass_filtering = config.echo_canceller.enforce_high_pass_filtering;
  v.noise_suppression_enabled = config.noise_suppression.enabled;
  v.noise_suppression_level = static_cast<std::int32_t>(config.noise_suppression.level);
  v.noise_suppression_analyze_linear_aec_output_when_available = config.noise_suppression.analyze_linear_aec_output_when_available;
  v.transient_suppression_enabled = config.transient_suppression.enabled;
  v.gain_controller1_enabled = config.gain_controller1.enabled;
  v.gain_controller1_mode = static_cast<std::int32_t>(config.gain_controller1.mode);
  v.gain_controller1_target_level_dbfs = config.gain_controller1.target_level_dbfs;
  v.gain_controller1_compression_gain_db = config.gain_controller1.compression_gain_db;
  v.gain_controller1_enable_limiter = config.gain_controller1.enable_limiter;
  v.gain_controller1_analog_gain_controller_enabled = config.gain_controller1.analog_gain_controller.enabled;
  v.gain_controller1_analog_gain_controller_startup_min_volume = config.gain_controller1.analog_gain_controller.startup_min_volume;
  v.gain_controller1_analog_gain_controller_clipped_level_min = config.gain_controller1.analog_gain_controller.clipped_level_min;
  v.gain_controller1_analog_gain_controller_enable_digital_adaptive = config.gain_controller1.analog_gain_controller.enable_digital_adaptive;
  v.gain_controller1_analog_gain_controller_clipped_level_step = config.gain_controller1.analog_gain_controller.clipped_level_step;
  v.gain_controller1_analog_gain_controller_clipped_ratio_threshold = config.gain_controller1.analog_gain_controller.clipped_ratio_threshold;
  v.gain_controller1_analog_gain_controller_clipped_wait_frames = config.gain_controller1.analog_gain_controller.clipped_wait_frames;
  v.gain_controller1_analog_gain_controller_clipping_predictor_enabled = config.gain_controller1.analog_gain_controller.clipping_predictor.enabled;
  v.gain_controller1_analog_gain_controller_clipping_predictor_mode = static_cast<std::int32_t>(config.gain_controller1.analog_gain_controller.clipping_predictor.mode);
  v.gain_controller1_analog_gain_controller_clipping_predictor_window_length = config.gain_controller1.analog_gain_controller.clipping_predictor.window_length;
  v.gain_controller1_analog_gain_controller_clipping_predictor_reference_window_length = config.gain_controller1.analog_gain_controller.clipping_predictor.reference_window_length;
  v.gain_controller1_analog_gain_controller_clipping_predictor_reference_window_delay = config.gain_controller1.analog_gain_controller.clipping_predictor.reference_window_delay;
  v.gain_controller1_analog_gain_controller_clipping_predictor_clipping_threshold = config.gain_controller1.analog_gain_controller.clipping_predictor.clipping_threshold;
  v.gain_controller1_analog_gain_controller_clipping_predictor_crest_factor_margin = config.gain_controller1.analog_gain_controller.clipping_predictor.crest_factor_margin;
  v.gain_controller1_analog_gain_controller_clipping_predictor_use_predicted_step = config.gain_controller1.analog_gain_controller.clipping_predictor.use_predicted_step;
  v.gain_controller2_enabled = config.gain_controller2.enabled;
  v.gain_controller2_input_volume_controller_enabled = config.gain_controller2.input_volume_controller.enabled;
  v.gain_controller2_adaptive_digital_enabled = config.gain_controller2.adaptive_digital.enabled;
  v.gain_controller2_adaptive_digital_headroom_db = config.gain_controller2.adaptive_digital.headroom_db;
  v.gain_controller2_adaptive_digital_max_gain_db = config.gain_controller2.adaptive_digital.max_gain_db;
  v.gain_controller2_adaptive_digital_initial_gain_db = config.gain_controller2.adaptive_digital.initial_gain_db;
  v.gain_controller2_adaptive_digital_max_gain_change_db_per_second = config.gain_controller2.adaptive_digital.max_gain_change_db_per_second;
  v.gain_controller2_adaptive_digital_max_output_noise_level_dbfs = config.gain_controller2.adaptive_digital.max_output_noise_level_dbfs;
  v.gain_controller2_fixed_digital_gain_db = config.gain_controller2.fixed_digital.gain_db;
  return v;
}
// This is the pin's Validate predicate. Build does not run ApplyConfig's
// fallback; ApplyConfig does, even for a disabled GC2. Keep that distinction.
bool ValidGc2(const webrtc::AudioProcessing::Config::GainController2& g) {
  const auto& a = g.adaptive_digital;
  return g.fixed_digital.gain_db >= 0 && g.fixed_digital.gain_db < 50 &&
      a.headroom_db >= 0 && a.max_gain_db > 0 && a.initial_gain_db >= 0 &&
      a.max_gain_change_db_per_second > 0 && a.max_output_noise_level_dbfs <= 0;
}
bool RuntimeReinitializationUnsafe(const NativeAudioProcessingModule& p) {
#if RTC_DCHECK_IS_ON
  const auto c = p.state().module->GetConfig();
  return c.gain_controller2.enabled && !ValidGc2(c.gain_controller2);
#else
  return false;
#endif
}
bool FormatsReachInitialization(const webrtc::StreamConfig& input, const webrtc::StreamConfig& output) {
  const auto valid = [](const webrtc::StreamConfig& s) {
    return s.sample_rate_hz() >= 8000 && s.sample_rate_hz() <= 384000 && s.num_channels() > 0;
  };
  return valid(input) && valid(output) && (output.num_channels() == 1 || output.num_channels() == input.num_channels());
}
bool ImplicitInitializationUnsafe(const NativeAudioProcessingModule& p, const webrtc::StreamConfig& input, const webrtc::StreamConfig& output, bool reverse) {
  if (!RuntimeReinitializationUnsafe(p) || !FormatsReachInitialization(input, output)) return false;
  const auto& s = p.state();
  if (reverse) return !s.render_format_known || input != s.format_hint.reverse_input_stream() || output != s.format_hint.reverse_output_stream();
  return !s.capture_format_known || s.capture_graph_dirty || input != s.format_hint.input_stream() || output != s.format_hint.output_stream();
}
void NoteProcessingFormats(NativeAudioProcessingModule& p, const webrtc::StreamConfig& input, const webrtc::StreamConfig& output, bool reverse, int status) {
  RefreshSelectedEchoAllocation(p);
  if (reverse) {
    p.state().render_format_known = status == 0;
    if (status == 0) { p.state().format_hint.reverse_input_stream() = input; p.state().format_hint.reverse_output_stream() = output; }
  } else {
    p.state().capture_format_known = status == 0;
    if (status == 0) { p.state().format_hint.input_stream() = input; p.state().format_hint.output_stream() = output; }
  }
}
std::uint8_t AdmitPending(const NativeAudioProcessingModule& p, const webrtc::AudioProcessing::Config& c) {
  if (!p.state().queued_gains.Safe(c)) return 9;
  if (!p.state().possibly_queued_gains.Safe(c)) return 10;
  return 0;
}
void NoteCaptureResult(NativeAudioProcessingModule& p, int status, std::uint8_t callback_error, const CallbackOperation& operation) {
  RefreshSelectedEchoAllocation(p);
  if (callback_error) p.state().linear_callback_error = callback_error;
  auto& invalid = p.state().invalid_metric_sources;
  for (const auto& metric : operation.metric_results()) {
    const auto found = std::find(invalid.begin(), invalid.end(), metric.first);
    if (metric.second) { if (found != invalid.end()) invalid.erase(found); }
    else if (found == invalid.end()) invalid.push_back(metric.first);
  }
  // Both admitted successful capture overloads reach UpdateStatistics exactly
  // once. A full native queue discards newer snapshots, so do not replace its
  // validity tag with the latest frame's status or clear a failed cached tag
  // just because a subsequent frame succeeded.
  if (status == webrtc::AudioProcessing::kNoError) {
    if (!p.state().queued_statistics_error)
      p.state().queued_statistics_error = callback_error ? callback_error : invalid.empty() ? 0 : 18;
    p.state().linear_callback_error = callback_error;
    // Both pinned capture overloads finish the entire capture queue drain.
    p.state().queued_gains = {};
    p.state().possibly_queued_gains = {};
    p.state().capture_graph_dirty = false;
  } else {
    // A native error may precede or follow the drain. Do not invent an ack.
    p.state().possibly_queued_gains.Merge(p.state().queued_gains);
    p.state().queued_gains = {};
  }
}
std::uint8_t AdmitRuntime(const webrtc::AudioProcessing::Config& c, const FfiApmRuntimeSetting& f) {
  if (f.kind > 8) return 11;
#if RTC_DCHECK_IS_ON
  if (f.kind == 0) return 11;
#endif
  // These factories declare their domain before any deferred consumption.
  if (f.kind == 3 && (f.int_value < 0 || f.int_value > 90)) return 11;
  if (f.kind == 4 && !(f.float_value >= 0 && f.float_value <= 90)) return 11;
  PendingGains proposed;
  proposed.Include(f);
  return proposed.Safe(c) ? 0 : 11;
}
webrtc::AudioProcessing::RuntimeSetting RuntimeFromFfi(const FfiApmRuntimeSetting& f) {
  using Setting = webrtc::AudioProcessing::RuntimeSetting;
  switch (f.kind) {
    case 1: return Setting::CreateCapturePreGain(f.float_value);
    case 2: return Setting::CreateCapturePostGain(f.float_value);
    case 3: return Setting::CreateCompressionGainDb(f.int_value);
    case 4: return Setting::CreateCaptureFixedPostGain(f.float_value);
    case 5: return Setting::CreatePlayoutVolumeChange(f.int_value);
    case 6: return Setting::CreateCustomRenderSetting(f.float_value);
    case 7: return Setting::CreatePlayoutAudioDeviceChange({f.device_id, f.device_max_volume});
    case 8: return Setting::CreateCaptureOutputUsedSetting(f.bool_value);
    default: return Setting();
  }
}
std::uint8_t AdmitConfig(const webrtc::Environment& env, const webrtc::AudioProcessing::Config& c, bool build, bool linear_echo_configured = false, bool echo_factory_selected = false) {
  const bool echo_selected = c.echo_canceller.enabled || echo_factory_selected;
  if (echo_selected && c.echo_canceller.export_linear_aec_output && !linear_echo_configured) return 7;
#if RTC_DCHECK_IS_ON
  if (c.pipeline.maximum_internal_processing_rate != 32000 &&
      c.pipeline.maximum_internal_processing_rate != 48000) return 3;
#endif
  const auto& g = c.gain_controller1;
  if (g.enabled) {
    if (!g.analog_gain_controller.enabled) {
      if (g.target_level_dbfs < 0 || g.target_level_dbfs > 31 ||
          g.compression_gain_db < 0 || g.compression_gain_db > 90) return 3;
    } else {
      const auto& a = g.analog_gain_controller;
      if (a.clipped_level_min < 0 || a.clipped_level_min > 255 ||
          a.clipped_level_step < 1 || a.clipped_level_step > 255 ||
          !(a.clipped_ratio_threshold > 0 && a.clipped_ratio_threshold < 1) ||
          a.clipped_wait_frames < 1) return 3;
      const auto& p = a.clipping_predictor;
      if (p.enabled) {
        const std::int64_t capacity = std::int64_t(p.reference_window_length) + p.reference_window_delay;
        if (p.window_length < 1 || p.reference_window_length < 1 || p.reference_window_delay < 0 ||
            capacity > std::numeric_limits<int>::max() || p.window_length >= capacity) return 3;
        // Event mode compares floats. Peak modes narrow the same field to int.
        if (p.mode != webrtc::AudioProcessing::Config::GainController1::AnalogGainController::ClippingPredictor::kClippingEventPrediction &&
            !(std::isfinite(p.clipping_threshold) && p.clipping_threshold >= -2147483648.0f && p.clipping_threshold < 2147483648.0f)) return 3;
      }
    }
  }
  if (c.pre_amplifier.enabled || c.capture_level_adjustment.enabled) {
    float pre = c.pre_amplifier.enabled ? c.pre_amplifier.fixed_gain_factor : 1.f;
    const float adjustment = c.capture_level_adjustment.enabled ? c.capture_level_adjustment.pre_gain_factor : 1.f;
    const double product = double(pre) * adjustment;
    if (!std::isfinite(pre) || !std::isfinite(adjustment) || !std::isfinite(product) ||
        std::abs(product) > std::numeric_limits<float>::max() ||
        !std::isfinite(c.capture_level_adjustment.post_gain_factor)) return 3;
    const auto& mic = c.capture_level_adjustment.analog_mic_gain_emulation;
    if (mic.enabled && (mic.initial_level < 0 || mic.initial_level > 255)) return 3;
  }
  const auto& g2 = c.gain_controller2;
  if (g2.enabled && (build || ValidGc2(g2))) {
    if (!ValidGc2(g2)) return 3;
    if (g2.adaptive_digital.enabled) {
      const auto& a = g2.adaptive_digital;
#if RTC_DCHECK_IS_ON
      if (a.max_output_noise_level_dbfs < -90 ||
          !(a.max_gain_change_db_per_second * 10.f / 1000.f > 0)) return 3;
#endif
      // Actual steady target is noise-limited, independently of max_gain_db.
      // A finite powf alone cannot certify an extreme initial gain: preceding
      // DSP is not hard-clipped to the input PCM range. Wider initial settings
      // remain an explicit admission gap, not native capability absence.
      const float max_target_db = -webrtc::FloatS16ToDbfs(0.f);
      if (!std::isfinite(webrtc::DbToRatio(a.initial_gain_db))) return 3;
      if (a.initial_gain_db > max_target_db) return 4;
    }
  }
  const auto& trials = env.field_trials();
  if (echo_selected && !SafeAecTrials(trials)) return 1;
  if (g.enabled && g.analog_gain_controller.enabled &&
      trials.IsEnabled("WebRTC-Audio-2ndAgcMinMicLevelExperiment")) {
    const auto group = trials.Lookup("WebRTC-Audio-2ndAgcMinMicLevelExperiment");
    if (group.starts_with("Enabled-") && !NumericRepresentable(group.substr(8), NumericKind::Int32)) return 1;
  }
  return 0;
}
}
bool audio_processor_custom_build(NativeAudioProcessingModule& owner, const NativeEnvironment& environment) noexcept {
  auto product = webrtc::CustomAudioProcessing(owner.state().module)->Build(environment.environment());
  if (!product) return false;
  owner.state().module = std::move(product);  // exact module; native ignores the new environment
  return true;
}
std::unique_ptr<NativeAudioProcessingModule> audio_processor_new_config(const NativeEnvironment& environment, const FfiApmConfig& f, bool linear_echo, std::uint8_t& error) noexcept {
  return audio_processor_new_custom(environment, f, linear_echo, FfiAec3Selection{}, nullptr, nullptr, nullptr, nullptr, nullptr, nullptr, error);
}
std::unique_ptr<NativeAudioProcessingModule> audio_processor_new_custom(const NativeEnvironment& environment, const FfiApmConfig& f, bool linear_echo, const FfiAec3Selection& echo_config, std::unique_ptr<NativeApmCustomModule> capture_post, std::unique_ptr<NativeApmCustomModule> render_pre, std::unique_ptr<NativeApmCustomModule> analyzer, std::unique_ptr<NativeApmCustomModule> echo_detector, std::unique_ptr<NativeApmCustomModule> neural, std::unique_ptr<NativeApmCustomModule> echo_factory, std::uint8_t& error) noexcept {
  const auto env = environment.environment();
  const auto config = ConfigFromFfi(f);
  std::optional<webrtc::EchoCanceller3Config> mono, multi;
  if (echo_config.mono_present) mono = EchoConfigFromFfi(echo_config.mono);
  if (echo_config.multi_present) multi = EchoConfigFromFfi(echo_config.multi);
  if (linear_echo) {
    if (!mono) {
      mono = webrtc::EchoCanceller3Config();
      multi = webrtc::EchoCanceller3Config::CreateDefaultMultichannelConfig();
    }
    mono->filter.export_linear_aec_output = true;
    if (multi) multi->filter.export_linear_aec_output = true;
  }
  if (echo_factory && (!echo_factory->state().echo_factory || !echo_factory->state().echo_factory_forwarder)) { error = 2; return nullptr; }
  const bool linear_configured = echo_factory ? echo_factory->state().echo_factory_linear : (mono && mono->filter.export_linear_aec_output && (!multi || multi->filter.export_linear_aec_output));
  error = AdmitConfig(env, config, true, linear_configured, bool(echo_factory));
  if (error) return nullptr;
  auto state = std::make_unique<NativeAudioProcessingModule::State>(NativeAudioProcessingModule::State{env, std::nullopt, nullptr, linear_configured, config.echo_canceller.enabled && config.echo_canceller.export_linear_aec_output});
  if (echo_factory) {
    auto& selected = echo_factory->state();
    // A unique handoff starts a module-local observation domain. Products from
    // earlier standalone Create calls keep their own health, never this slot.
    selected.echo_factory_observation = std::make_shared<EchoFactoryObservation>();
    selected.echo_factory_forwarder->Rebind(selected.echo_factory_observation);
    if (selected.caller_echo_factory) selected.caller_echo_factory->Rebind(selected.echo_factory_observation);
    state->echo_configs = selected.echo_configs;
    state->echo_factory_observation = selected.echo_factory_observation;
  }
  else { if (mono) state->echo_configs.push_back(*mono); if (multi) state->echo_configs.push_back(*multi); }
  if (config.echo_canceller.enabled || state->echo_factory_observation) for (const auto& selected : state->echo_configs)
    if (!SafeEchoConfig(selected, env.field_trials())) { error = 3; return nullptr; }
  webrtc::BuiltinAudioProcessingBuilder builder(config);
  if (mono) builder.SetEchoCancellerConfig(*mono, multi);
  if ((capture_post && !capture_post->state().processor) || (render_pre && !render_pre->state().processor) || (analyzer && !analyzer->state().analyzer) || (echo_detector && !echo_detector->state().detector) || (neural && !neural->state().neural)) { error = 2; return nullptr; }
  if (echo_factory) {
    state->custom_callbacks.push_back(echo_factory->state().status);
    builder.SetEchoControlFactory(std::move(echo_factory->state().echo_factory));
  }
  if (neural) {
    state->custom_callbacks.push_back(neural->state().status);
    builder.SetNeuralResidualEchoEstimator(std::move(neural->state().neural));
  }
  state->caller_echo_detector = bool(echo_detector);
  if (echo_detector) {
    state->custom_callbacks.push_back(echo_detector->state().status);
    builder.SetEchoDetector(std::move(echo_detector->state().detector));
  }
  if (capture_post) {
    state->custom_callbacks.push_back(capture_post->state().status);
    builder.SetCapturePostProcessing(std::move(capture_post->state().processor));
  }
  if (render_pre) {
    auto& callback = static_cast<CallerCustomProcessing&>(*render_pre->state().processor).callbacks;
    callback.render = true;
    state->render_callback_policy = callback.bounded_render;
    state->render_callback_policy->store(state->caller_echo_detector || bool(state->echo_factory_observation) || config.echo_canceller.enabled || config.gain_controller1.enabled, std::memory_order_relaxed);
    state->custom_callbacks.push_back(render_pre->state().status);
    builder.SetRenderPreProcessing(std::move(render_pre->state().processor));
  }
  if (analyzer) {
    state->custom_callbacks.push_back(analyzer->state().status);
    builder.SetCaptureAnalyzer(std::move(analyzer->state().analyzer));
  }
  const CallbackOperation generation(state->custom_callbacks);
  state->module = builder.Build(state->environment);
  if (generation.failed() || EchoFactoryInvalid(state->echo_factory_observation)) { error = 18; return nullptr; }
  if (!state->module) return nullptr;
  auto result = std::make_unique<NativeAudioProcessingModule>(std::move(state));
  RefreshSelectedEchoAllocation(*result);
  return result;
}
std::uint8_t audio_processor_config_apply(NativeAudioProcessingModule& p, const FfiApmConfig& f) noexcept {
  const auto config = ConfigFromFfi(f);
  if (EchoControllerSelected(p, config) && p.state().module->num_reverse_channels() == 0) return 2;
  if (const auto error = AdmitPending(p, config)) return error;
  if (const auto error = AdmitConfig(p.state().environment, config, false, p.state().linear_echo_configured, bool(p.state().echo_factory_observation))) return error;
  if (EchoControllerSelected(p, config)) for (const auto& selected : p.state().echo_configs)
    if (!SafeEchoConfig(selected, p.state().environment.field_trials())) return 3;
  const auto previous = p.state().module->GetConfig();
  const auto generation = DumpGeneration(p);
  UpdateRenderCallbackPolicy(p, config);
  p.state().module->ApplyConfig(config);
  p.state().capture_graph_dirty = true;
  if (p.state().echo_factory_observation) RefreshSelectedEchoAllocation(p);
  else if (!config.echo_canceller.enabled || !config.echo_canceller.export_linear_aec_output)
    p.state().linear_output_guaranteed = false;
  else if (!previous.echo_canceller.enabled)
    p.state().linear_output_guaranteed = true;
  return DumpOutcome(p, generation);
}
bool audio_processor_dump_path(NativeAudioProcessingModule& p, rust::Box<RustApmDumpWorker> worker, rust::Slice<const std::uint8_t> path, std::int64_t maximum, std::uint8_t& error) noexcept {
  auto* queue = task_queue_for_blocking_consumer(apm_dump_worker_queue(*worker));
  error = queue ? 0 : 13;
  if (error) return false;
  const std::string filename(reinterpret_cast<const char*>(path.data()), path.size());
  if (filename.find('\0') != std::string::npos) { error = 16; return false; }
  const auto generation = DumpGeneration(p);
  const bool attached = p.state().module->CreateAndAttachAecDump(filename, maximum, queue);
  // Native replacement has finished destroying/flushing the old dump while
  // its old queue owner is still retained. Only now retire that worker.
  if (attached) p.state().dump_worker = std::move(worker);
  error = DumpOutcome(p, generation);
  return attached;
}
namespace {
void CloseDumpDescriptor(int fd) {
#if defined(WEBRTC_WIN)
  ::_close(fd);
#else
  ::close(fd);
#endif
}
std::FILE* DumpStreamFromDescriptor(int fd, std::int64_t maximum, std::uint8_t& error) {
#if defined(WEBRTC_WIN)
  return ::_fdopen(fd, "wb");  // The safe owning Rust FILE API is Unix-only.
#else
  const int flags = ::fcntl(fd, F_GETFL);
  if (flags < 0) return nullptr;
  const int access = flags & O_ACCMODE;
#if RTC_DCHECK_IS_ON && defined(WEBRTC_ENABLE_PROTOBUF)
  // Native writer asserts on failed fwrite. A zero budget provably never
  // writes; for other budgets readonly input is not a qualified write path.
  if (access == O_RDONLY && maximum != 0) { error = 17; return nullptr; }
#endif
  const char* mode = access == O_RDONLY ? "rb" : access == O_WRONLY ? "wb" : "r+b";
  // fdopen does not truncate or reposition an already-open descriptor.
  return ::fdopen(fd, mode);
#endif
}
}
bool audio_processor_dump_file(NativeAudioProcessingModule& p, rust::Box<RustApmDumpWorker> worker, std::int32_t fd, std::int64_t maximum, std::uint8_t& error) noexcept {
  auto* queue = task_queue_for_blocking_consumer(apm_dump_worker_queue(*worker));
  error = queue ? 0 : 13;
  if (error) { CloseDumpDescriptor(fd); return false; }
  auto* stream = DumpStreamFromDescriptor(fd, maximum, error);
  if (!stream) { CloseDumpDescriptor(fd); if (!error) error = 15; return false; }
  const auto generation = DumpGeneration(p);
  const bool attached = p.state().module->CreateAndAttachAecDump(stream, maximum, queue);
#ifndef WEBRTC_ENABLE_PROTOBUF
  // The actual null factory does NOT consume FILE*, unlike the enabled one.
  // This owning adapter must reclaim it after the real native false result.
  std::fclose(stream);
#endif
  if (attached) p.state().dump_worker = std::move(worker);
  error = DumpOutcome(p, generation);
  return attached;
}
std::uint8_t audio_processor_dump_attach(NativeAudioProcessingModule& p, rust::Box<RustCallerApmDump> dump) noexcept {
  const auto generation = DumpGeneration(p);
  p.state().module->AttachAecDump(std::make_unique<CallerAecDump>(std::move(dump), p.state().dump_callbacks));
  p.state().dump_worker.reset();
  return DumpOutcome(p, generation);
}
std::uint8_t audio_processor_dump_detach(NativeAudioProcessingModule& p) noexcept {
  const auto generation = DumpGeneration(p);
  p.state().module->DetachAecDump();
  p.state().dump_worker.reset();
  return DumpOutcome(p, generation);
}
FfiApmDumpConfig audio_processor_dump_config_default() noexcept { return DumpConfigToFfi(webrtc::InternalAPMConfig{}); }
bool audio_processor_dump_config_equal(const FfiApmDumpConfig& a, const FfiApmDumpConfig& b) noexcept { return DumpConfigFromFfi(a) == DumpConfigFromFfi(b); }
bool audio_processor_runtime_setting(NativeAudioProcessingModule& p, const FfiApmRuntimeSetting& f, bool post, std::uint8_t& error) noexcept {
  error = AdmitRuntime(p.state().module->GetConfig(), f);
  if (error) return false;
  const auto setting = RuntimeFromFfi(f);
  if (post) {
    const bool accepted = p.state().module->PostRuntimeSetting(setting);
    if (accepted && f.kind == 1) {
      p.state().queued_gains.pre = {};
      p.state().queued_gains.pre.Include(f.float_value);
      p.state().possibly_queued_gains.pre = {};
    }
    if (accepted && f.kind == 2) {
      p.state().queued_gains.post = {};
      p.state().queued_gains.post.Include(f.float_value);
      p.state().possibly_queued_gains.post = {};
    }
    return accepted;
  }
  p.state().module->SetRuntimeSetting(setting);
  // Void Set discards the native queue acknowledgment, including full-queue
  // failure. Preserve possible payloads without claiming enqueue success.
  p.state().possibly_queued_gains.Include(f);
  return false;  // No acknowledgment; the Rust void-set path ignores this.
}
bool audio_processor_config_equal(const FfiApmConfig& left, const FfiApmConfig& right, std::uint8_t group) noexcept {
  const auto a = ConfigFromFfi(left), b = ConfigFromFfi(right);
  switch (group) {
    case 0: return a.capture_level_adjustment == b.capture_level_adjustment;
    case 1: return a.capture_level_adjustment.analog_mic_gain_emulation == b.capture_level_adjustment.analog_mic_gain_emulation;
    case 2: return a.gain_controller1 == b.gain_controller1;
    case 3: return a.gain_controller2 == b.gain_controller2;
    case 4: return a.gain_controller2.input_volume_controller == b.gain_controller2.input_volume_controller;
    case 5: return a.gain_controller2.adaptive_digital == b.gain_controller2.adaptive_digital;
    default: return false;
  }
}
namespace {
FfiApmStatistics StatisticsToFfi(const webrtc::AudioProcessingStats& s) {
  FfiApmStatistics result{};
  if (s.voice_detected) { result.present |= std::uint16_t(1u << 0); result.voice_detected = *s.voice_detected; }
  if (s.echo_return_loss) { result.present |= std::uint16_t(1u << 1); result.echo_return_loss = *s.echo_return_loss; }
  if (s.echo_return_loss_enhancement) { result.present |= std::uint16_t(1u << 2); result.echo_return_loss_enhancement = *s.echo_return_loss_enhancement; }
  if (s.divergent_filter_fraction) { result.present |= std::uint16_t(1u << 3); result.divergent_filter_fraction = *s.divergent_filter_fraction; }
  if (s.delay_median_ms) { result.present |= std::uint16_t(1u << 4); result.delay_median_ms = *s.delay_median_ms; }
  if (s.delay_standard_deviation_ms) { result.present |= std::uint16_t(1u << 5); result.delay_standard_deviation_ms = *s.delay_standard_deviation_ms; }
  if (s.residual_echo_likelihood) { result.present |= std::uint16_t(1u << 6); result.residual_echo_likelihood = *s.residual_echo_likelihood; }
  if (s.residual_echo_likelihood_recent_max) { result.present |= std::uint16_t(1u << 7); result.residual_echo_likelihood_recent_max = *s.residual_echo_likelihood_recent_max; }
  if (s.delay_ms) { result.present |= std::uint16_t(1u << 8); result.delay_ms = *s.delay_ms; }
  return result;
}
}
bool audio_processor_linear_output(NativeAudioProcessingModule& p, rust::Slice<float> output, std::uint8_t& error) noexcept {
  error = ApmEchoInvalid(p) ? 18 : p.state().linear_callback_error;
  if (error) return false;
  RefreshSelectedEchoAllocation(p);
#if RTC_DCHECK_IS_ON
  if (!p.state().linear_output_guaranteed) { error = 8; return false; }
#endif
  const auto channels = p.state().module->num_proc_channels();
  // Structural conversion only: uninitialized native array rows avoid aliasing
  // Rust arrays as C++ std::array subobjects. Copy solely after the SDK confirms
  // availability; its successful path writes every sample of every row.
  auto rows = std::unique_ptr<std::array<float, 160>[]>(new std::array<float, 160>[channels]);
  if (!p.state().module->GetLinearAecOutput(std::span<std::array<float, 160>>(rows.get(), channels))) return false;
  if (output.size() / 160 < channels) { error = 6; return false; }
  for (std::size_t ch = 0; ch < channels; ++ch)
    std::copy(rows[ch].begin(), rows[ch].end(), output.data() + ch * 160);
  return true;
}
FfiApmStatistics audio_processor_statistics_default() noexcept { return StatisticsToFfi(webrtc::AudioProcessingStats{}); }
FfiApmStatistics audio_processor_statistics(NativeAudioProcessingModule& p, bool has_argument, bool remote) noexcept {
  const auto generation = DumpGeneration(p);
  std::uint8_t error = 0;
  auto result = StatisticsToFfi(TakeApmStatistics(p, has_argument, remote, error));
  if (const auto callback_error = DumpOutcome(p, generation)) error = callback_error;
  if (error) { result = StatisticsToFfi(webrtc::AudioProcessingStats{}); result.error = error; }
  return result;
}
FfiApmConfig audio_processor_config_default() noexcept { return ConfigToFfi(webrtc::AudioProcessing::Config{}); }
rust::String audio_processor_config_string(const FfiApmConfig& v) noexcept { return ConfigFromFfi(v).ToString(); }
FfiApmConfig audio_processor_config_get(const NativeAudioProcessingModule& p) noexcept { return ConfigToFfi(p.state().module->GetConfig()); }
std::int32_t audio_processor_initialize(NativeAudioProcessingModule& p, std::uint8_t& error) noexcept {
  error = RuntimeReinitializationUnsafe(p) ? 12 : 0;
  if (error) return 0;
  const auto generation = DumpGeneration(p);
  const auto status = p.state().module->Initialize();
  error = DumpOutcome(p, generation);
  if (p.state().echo_factory_observation) RefreshSelectedEchoAllocation(p);
  else if (status == 0) { const auto c = p.state().module->GetConfig(); p.state().linear_output_guaranteed = c.echo_canceller.enabled && c.echo_canceller.export_linear_aec_output; }
  return status;
}
FfiApmFormats audio_processor_formats_default() noexcept {
  const webrtc::ProcessingConfig config;
  return {config.input_stream().sample_rate_hz(), config.input_stream().num_channels(), config.output_stream().sample_rate_hz(), config.output_stream().num_channels(), config.reverse_input_stream().sample_rate_hz(), config.reverse_input_stream().num_channels(), config.reverse_output_stream().sample_rate_hz(), config.reverse_output_stream().num_channels()};
}
std::int32_t audio_processor_initialize_formats(NativeAudioProcessingModule& p, const FfiApmFormats& f, bool& admitted, std::uint8_t& error) noexcept {
  admitted = false;
  error = 0;
  const auto valid = [](std::int32_t rate, std::size_t channels) {
    return rate >= 8000 && rate <= 384000 && channels > 0 && channels <= std::numeric_limits<std::int32_t>::max();
  };
  if (!valid(f.input_rate, f.input_channels) || !valid(f.output_rate, f.output_channels) ||
      (f.output_channels != 1 && f.output_channels != f.input_channels)) return 0;
  if (f.reverse_input_channels == 0 && f.reverse_output_channels == 0) {
    // Absent render is allowed only when no selected echo controller needs it.
    if (EchoControllerSelected(p, p.state().module->GetConfig())) return 0;
  } else if (!valid(f.reverse_input_rate, f.reverse_input_channels) ||
             !valid(f.reverse_output_rate, f.reverse_output_channels) ||
             (f.reverse_input_channels != f.reverse_output_channels &&
              f.reverse_input_channels != 1 && f.reverse_output_channels != 1)) {
    return 0;
  }
  webrtc::ProcessingConfig config;
  config.input_stream() = webrtc::StreamConfig(f.input_rate, f.input_channels);
  config.output_stream() = webrtc::StreamConfig(f.output_rate, f.output_channels);
  config.reverse_input_stream() = webrtc::StreamConfig(f.reverse_input_rate, f.reverse_input_channels);
  config.reverse_output_stream() = webrtc::StreamConfig(f.reverse_output_rate, f.reverse_output_channels);
  error = RuntimeReinitializationUnsafe(p) ? 12 : 0;
  if (error) return 0;
  admitted = true;
  const auto generation = DumpGeneration(p);
  const auto status = p.state().module->Initialize(config);
  error = DumpOutcome(p, generation);
  if (status == 0) {
    p.state().format_hint = config;
    p.state().capture_format_known = p.state().render_format_known = true;
    p.state().capture_graph_dirty = false;
  }
  if (p.state().echo_factory_observation) RefreshSelectedEchoAllocation(p);
  else if (status == 0) { const auto c = p.state().module->GetConfig(); p.state().linear_output_guaranteed = c.echo_canceller.enabled && c.echo_canceller.export_linear_aec_output; }
  return status;
}
std::uint8_t audio_processor_configure(NativeAudioProcessingModule& p, const FfiAudioProcessingConfig& selected) noexcept {
  auto config = p.state().module->GetConfig();
  SelectEffects(config, selected);
  if (EchoControllerSelected(p, config) && p.state().module->num_reverse_channels() == 0) return 2;
  if (const auto error = AdmitPending(p, config)) return error;
  if (const auto error = AdmitConfig(p.state().environment, config, false, p.state().linear_echo_configured, bool(p.state().echo_factory_observation))) return error;
  if (EchoControllerSelected(p, config)) for (const auto& selected_config : p.state().echo_configs)
    if (!SafeEchoConfig(selected_config, p.state().environment.field_trials())) return 3;
  const auto previous = p.state().module->GetConfig();
  const auto generation = DumpGeneration(p);
  UpdateRenderCallbackPolicy(p, config);
  p.state().module->ApplyConfig(config);
  p.state().capture_graph_dirty = true;
  if (p.state().echo_factory_observation) RefreshSelectedEchoAllocation(p);
  else if (!config.echo_canceller.enabled || !config.echo_canceller.export_linear_aec_output)
    p.state().linear_output_guaranteed = false;
  else if (!previous.echo_canceller.enabled)
    p.state().linear_output_guaranteed = true;
  return DumpOutcome(p, generation);
}
FfiAudioProcessingFormatState audio_processor_format(const NativeAudioProcessingModule& p) noexcept {
  const auto& m = p.state().module;
  return {m->proc_sample_rate_hz(), m->proc_split_sample_rate_hz(), m->num_input_channels(), m->num_proc_channels(), m->num_output_channels(), m->num_reverse_channels()};
}
void audio_processor_set_muted(NativeAudioProcessingModule& p, bool v) noexcept { p.state().module->set_output_will_be_muted(v); }
bool audio_processor_muted(NativeAudioProcessingModule& p) noexcept { return p.state().module->get_output_will_be_muted(); }
std::int32_t audio_processor_set_delay(NativeAudioProcessingModule& p, std::int32_t v) noexcept { return p.state().module->set_stream_delay_ms(v); }
std::int32_t audio_processor_delay(const NativeAudioProcessingModule& p) noexcept { return p.state().module->stream_delay_ms(); }
void audio_processor_set_analog_level(NativeAudioProcessingModule& p, std::int32_t v) noexcept { p.state().module->set_stream_analog_level(v); }
std::int32_t audio_processor_analog_level(const NativeAudioProcessingModule& p) noexcept { return p.state().module->recommended_stream_analog_level(); }
void audio_processor_set_key(NativeAudioProcessingModule& p, bool v) noexcept { p.state().module->set_stream_key_pressed(v); }
std::int32_t audio_processor_i16(NativeAudioProcessingModule& p, std::int32_t rate, std::size_t channels, rust::Slice<std::int16_t> samples, bool reverse, std::uint8_t& error) noexcept {
  const webrtc::StreamConfig config(rate, channels);
  error = ImplicitInitializationUnsafe(p, config, config, reverse) ? 12 : 0;
  if (error) return 0;
  // Separate input also avoids memcpy overlap in native error-copy branches.
  std::vector<std::int16_t> input(samples.begin(), samples.begin() + config.num_samples());
  std::int16_t empty_input = 0, empty_output = 0;
  const auto* src = input.empty() ? &empty_input : input.data();
  std::vector<std::int16_t> result(samples.begin(), samples.begin() + config.num_samples());
  auto* dest = result.empty() ? &empty_output : result.data();
  const auto generation = DumpGeneration(p);
  const auto status = reverse ? p.state().module->ProcessReverseStream(src, config, config, dest) : p.state().module->ProcessStream(src, config, config, dest);
  NoteProcessingFormats(p, config, config, reverse, status);
  error = DumpOutcome(p, generation);
  if (!reverse) NoteCaptureResult(p, status, error, generation);
  if (!error) std::copy(result.begin(), result.end(), samples.begin());
  return status;
}
std::int32_t audio_processor_float(NativeAudioProcessingModule& p, std::int32_t input_rate, std::size_t input_channels, rust::Slice<const float> samples, std::int32_t output_rate, std::size_t output_channels, rust::Slice<float> destination, bool reverse, std::uint8_t& error) noexcept {
  const webrtc::StreamConfig input(input_rate, input_channels), output(output_rate, output_channels);
  error = ImplicitInitializationUnsafe(p, input, output, reverse) ? 12 : 0;
  if (error) return 0;
  float empty_input = 0, empty_output = 0;
  auto src = InputPointers(samples, input, &empty_input);
  std::vector<float> result(destination.begin(), destination.begin() + output.num_samples());
  auto dest = OutputPointers(rust::Slice<float>(result.empty() ? &empty_output : result.data(), result.size()), output, &empty_output);
  const auto generation = DumpGeneration(p);
  const auto status = reverse ? p.state().module->ProcessReverseStream(src.data(), input, output, dest.data()) : p.state().module->ProcessStream(src.data(), input, output, dest.data());
  NoteProcessingFormats(p, input, output, reverse, status);
  error = DumpOutcome(p, generation);
  if (!reverse) NoteCaptureResult(p, status, error, generation);
  if (!error) std::copy(result.begin(), result.end(), destination.begin());
  return status;
}
std::int32_t audio_processor_analyze(NativeAudioProcessingModule& p, std::int32_t rate, std::size_t channels, rust::Slice<const float> samples, std::uint8_t& error) noexcept {
  const webrtc::StreamConfig input(rate, channels);
  error = ImplicitInitializationUnsafe(p, input, input, true) ? 12 : 0;
  if (error) return 0;
  float empty = 0;
  auto src = InputPointers(samples, input, &empty);
  const auto generation = DumpGeneration(p);
  const auto status = p.state().module->AnalyzeReverseStream(src.data(), input);
  error = DumpOutcome(p, generation);
  NoteProcessingFormats(p, input, input, true, status);
  return status;
}
}
