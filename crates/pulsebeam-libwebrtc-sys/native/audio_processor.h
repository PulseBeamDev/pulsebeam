#pragma once
#include <cstddef>
#include <cstdint>
#include <memory>
#include <vector>
#include "rust/cxx.h"
#include "api/scoped_refptr.h"
namespace webrtc { class AudioProcessing; class AudioBuffer; class NeuralResidualEchoEstimator; class Environment; }
namespace pulsebeam::webrtc_sys {
class NativeEnvironment;
struct RustApmDumpWorker;
struct RustInstalledApmEnvironment;
struct RustCallerApmDump;
struct RustApmCustomModule;
struct RustEchoEnvironment;
class NativeEchoCreationContext;
class NativeEchoDependencies final {
 public:
  explicit NativeEchoDependencies(webrtc::NeuralResidualEchoEstimator* value) noexcept : neural(value) {}
  webrtc::NeuralResidualEchoEstimator* neural;
};
struct FfiApmEchoDetectorMetrics;
struct FfiAec3Selection;
struct FfiEchoCanceller3Config;
struct FfiEchoControlAudio;
struct FfiEchoAudioShape;
struct FfiEchoControlMetrics;
struct FfiNeuralEchoInput;
struct FfiNeuralEchoResult;
struct FfiApmDumpConfig;
struct FfiApmConfig;
struct FfiApmFormats;
struct FfiApmStatistics;
struct FfiApmRuntimeSetting;
struct FfiAudioProcessingConfig;
struct FfiAudioProcessingFormatState;
class NativeApmCustomModule final {
 public:
  struct State;
  explicit NativeApmCustomModule(std::unique_ptr<State>) noexcept;
  ~NativeApmCustomModule();
  State& state() noexcept;
  const State& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeEchoAudio final {
 public:
  struct State;
  NativeEchoAudio(const webrtc::AudioBuffer&, bool);
  ~NativeEchoAudio();
  State& state() noexcept;
  const State& state() const noexcept;
  bool finite() const noexcept;
  void Commit(webrtc::AudioBuffer&) const;
 private:
  std::unique_ptr<State> state_;
};
class NativeEchoLinearOutput final {
 public:
  explicit NativeEchoLinearOutput(webrtc::AudioBuffer*);
  std::size_t channels() const noexcept;
  std::size_t frames() const noexcept;
  bool Write(std::size_t, rust::Slice<const float>);
  bool complete() const noexcept;
  bool finite() const noexcept;
  void Commit() const;
 private:
  webrtc::AudioBuffer* output_;
  std::size_t frames_;
  std::vector<std::vector<float>> channels_;
};
FfiEchoAudioShape audio_echo_view_shape(const NativeEchoAudio&) noexcept;
rust::Slice<const float> audio_echo_view_channel(const NativeEchoAudio&, std::size_t, bool, std::size_t, bool&) noexcept;
rust::Slice<float> audio_echo_view_channel_mut(NativeEchoAudio&, std::size_t, bool, std::size_t, bool&) noexcept;
std::size_t audio_echo_linear_channels(const NativeEchoLinearOutput&) noexcept;
std::size_t audio_echo_linear_frames(const NativeEchoLinearOutput&) noexcept;
bool audio_echo_linear_write(NativeEchoLinearOutput&, std::size_t, rust::Slice<const float>) noexcept;
class NativeEchoControl final {
 public:
  struct State;
  explicit NativeEchoControl(std::unique_ptr<State>) noexcept;
  ~NativeEchoControl();
  State& state() noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeApmCustomModule> audio_processor_echo_factory_provider_new(rust::Box<RustApmCustomModule>) noexcept;
void audio_processor_echo_factory_environment(NativeApmCustomModule&, rust::Box<RustEchoEnvironment>) noexcept;
void audio_echo_control_retain_environment(NativeEchoControl&, rust::Box<RustEchoEnvironment>) noexcept;
std::int32_t audio_echo_context_rate(const NativeEchoCreationContext&) noexcept;
std::int32_t audio_echo_context_render_channels(const NativeEchoCreationContext&) noexcept;
std::int32_t audio_echo_context_capture_channels(const NativeEchoCreationContext&) noexcept;
std::unique_ptr<NativeEchoControl> audio_echo_context_create(NativeApmCustomModule&, const NativeEchoCreationContext&, bool, std::uint8_t&) noexcept;
std::unique_ptr<NativeEchoControl> audio_echo_context_controller(const NativeEchoCreationContext&, bool, rust::Box<RustApmCustomModule>, std::uint8_t&) noexcept;
bool audio_echo_dependency_present(const NativeEchoDependencies&) noexcept;
std::uint8_t audio_echo_dependency_reset(NativeEchoDependencies&) noexcept;
FfiEchoCanceller3Config audio_echo_dependency_adjust(NativeEchoDependencies&, const FfiEchoCanceller3Config&, std::uint8_t&) noexcept;
FfiNeuralEchoResult audio_echo_dependency_estimate(NativeEchoDependencies&, FfiNeuralEchoInput) noexcept;
std::unique_ptr<NativeEchoControl> audio_echo_control_from_provider(std::int32_t, std::int32_t, std::int32_t, rust::Box<RustApmCustomModule>, std::uint8_t&) noexcept;
std::unique_ptr<NativeApmCustomModule> audio_processor_echo_factory_new(std::uint8_t, const FfiAec3Selection&) noexcept;
std::unique_ptr<NativeEchoControl> audio_echo_control_create(NativeApmCustomModule&, const NativeEnvironment&, std::int32_t, std::int32_t, std::int32_t, bool, const NativeApmCustomModule*, std::uint8_t&) noexcept;
FfiEchoControlAudio audio_echo_control_audio(NativeEchoControl&, std::uint8_t, std::int32_t, std::size_t, rust::Slice<const float>, bool, bool) noexcept;
FfiEchoControlMetrics audio_echo_control_metrics(NativeEchoControl&) noexcept;
std::uint8_t audio_echo_control_delay(NativeEchoControl&, std::int32_t) noexcept;
std::uint8_t audio_echo_control_usage(NativeEchoControl&, bool) noexcept;
bool audio_echo_control_active(NativeEchoControl&, std::uint8_t&) noexcept;
std::unique_ptr<NativeApmCustomModule> audio_processor_neural_new(rust::Box<RustApmCustomModule>) noexcept;
std::uint8_t audio_processor_neural_reset(NativeApmCustomModule&) noexcept;
FfiEchoCanceller3Config audio_processor_neural_adjust(NativeApmCustomModule&, const FfiEchoCanceller3Config&, std::uint8_t&) noexcept;
FfiNeuralEchoResult audio_processor_neural_estimate(NativeApmCustomModule&, FfiNeuralEchoInput) noexcept;
std::unique_ptr<NativeApmCustomModule> audio_processor_echo_detector_new(rust::Box<RustApmCustomModule>) noexcept;
std::unique_ptr<NativeApmCustomModule> audio_processor_echo_detector_clone(const NativeApmCustomModule&) noexcept;
std::uint8_t audio_processor_echo_detector_initialize(NativeApmCustomModule&, std::int32_t, std::int32_t, std::int32_t, std::int32_t) noexcept;
std::uint8_t audio_processor_echo_detector_analyze(NativeApmCustomModule&, bool, rust::Slice<const float>) noexcept;
FfiApmEchoDetectorMetrics audio_processor_echo_detector_metrics(NativeApmCustomModule&) noexcept;
std::unique_ptr<NativeApmCustomModule> audio_processor_custom_new(rust::Box<RustApmCustomModule>, bool) noexcept;
rust::Vec<std::uint8_t> audio_processor_custom_description(NativeApmCustomModule&, std::uint8_t&) noexcept;
rust::Vec<float> audio_processor_custom_audio(NativeApmCustomModule&, std::int32_t, std::size_t, rust::Slice<const float>, bool, std::uint8_t&) noexcept;
std::uint8_t audio_processor_custom_runtime(NativeApmCustomModule&, const FfiApmRuntimeSetting&) noexcept;
std::uint8_t audio_processor_custom_initialize(NativeApmCustomModule&, std::int32_t, std::int32_t) noexcept;
class NativeAudioProcessingModule final {
 public:
  struct State;
  explicit NativeAudioProcessingModule(std::unique_ptr<State>) noexcept;
  ~NativeAudioProcessingModule();
  State& state() noexcept;
  const State& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
// Shared ownership attaches to the actual SDK reference-counted facade, not
// merely to a temporary factory wrapper or the outer Rust handle.
class NativeInstalledAudioProcessor final {
 public:
  struct State;
  explicit NativeInstalledAudioProcessor(std::shared_ptr<State>) noexcept;
  ~NativeInstalledAudioProcessor();
  const std::shared_ptr<State>& state() const noexcept;
  webrtc::scoped_refptr<webrtc::AudioProcessing> module() const noexcept;
 private:
  std::shared_ptr<State> state_;
  webrtc::scoped_refptr<webrtc::AudioProcessing> module_;
};
std::unique_ptr<NativeInstalledAudioProcessor> audio_processor_into_installed(std::unique_ptr<NativeAudioProcessingModule>, rust::Box<RustInstalledApmEnvironment>, std::uint8_t&) noexcept;
NativeAudioProcessingModule& installed_audio_processor_core(const std::shared_ptr<NativeInstalledAudioProcessor::State>&) noexcept;
FfiApmConfig installed_audio_processor_original_config(const std::shared_ptr<NativeInstalledAudioProcessor::State>&) noexcept;
FfiApmConfig installed_audio_processor_config(const std::shared_ptr<NativeInstalledAudioProcessor::State>&, std::uint8_t&) noexcept;
std::uint8_t installed_audio_processor_apply(const std::shared_ptr<NativeInstalledAudioProcessor::State>&, const FfiApmConfig&) noexcept;
std::int32_t installed_audio_processor_reverse(const std::shared_ptr<NativeInstalledAudioProcessor::State>&, std::int32_t, std::size_t, rust::Slice<std::int16_t>, std::uint8_t&) noexcept;
std::int32_t installed_audio_processor_delay(const std::shared_ptr<NativeInstalledAudioProcessor::State>&, std::int32_t, std::uint8_t&) noexcept;
bool installed_audio_processor_reentry(const std::shared_ptr<NativeInstalledAudioProcessor::State>&) noexcept;
bool installed_audio_processor_has_echo_factory(const std::shared_ptr<NativeInstalledAudioProcessor::State>&, std::uint8_t&) noexcept;
std::int32_t installed_audio_processor_capture(const std::shared_ptr<NativeInstalledAudioProcessor::State>&, std::int32_t, std::size_t, rust::Slice<std::int16_t>, std::uint8_t, std::uint8_t, std::uint8_t, bool, bool&, std::uint8_t&) noexcept;
std::uint8_t installed_audio_processor_restore(const std::shared_ptr<NativeInstalledAudioProcessor::State>&) noexcept;
std::unique_ptr<NativeAudioProcessingModule> audio_processor_new(const NativeEnvironment&, const FfiAudioProcessingConfig&) noexcept;
bool audio_processor_dump_path(NativeAudioProcessingModule&, rust::Box<RustApmDumpWorker>, rust::Slice<const std::uint8_t>, std::int64_t, std::uint8_t&) noexcept;
bool audio_processor_dump_file(NativeAudioProcessingModule&, rust::Box<RustApmDumpWorker>, std::int32_t, std::int64_t, std::uint8_t&) noexcept;
std::uint8_t audio_processor_dump_attach(NativeAudioProcessingModule&, rust::Box<RustCallerApmDump>) noexcept;
std::uint8_t audio_processor_dump_detach(NativeAudioProcessingModule&) noexcept;
FfiApmDumpConfig audio_processor_dump_config_default() noexcept;
bool audio_processor_dump_config_equal(const FfiApmDumpConfig&, const FfiApmDumpConfig&) noexcept;
bool audio_processor_runtime_setting(NativeAudioProcessingModule&, const FfiApmRuntimeSetting&, bool, std::uint8_t&) noexcept;
bool audio_processor_config_equal(const FfiApmConfig&, const FfiApmConfig&, std::uint8_t) noexcept;
std::unique_ptr<NativeAudioProcessingModule> audio_processor_new_custom(const NativeEnvironment&, const FfiApmConfig&, bool, const FfiAec3Selection&, std::unique_ptr<NativeApmCustomModule>, std::unique_ptr<NativeApmCustomModule>, std::unique_ptr<NativeApmCustomModule>, std::unique_ptr<NativeApmCustomModule>, std::unique_ptr<NativeApmCustomModule>, std::unique_ptr<NativeApmCustomModule>, std::uint8_t&) noexcept;
bool audio_processor_custom_build(NativeAudioProcessingModule&, const NativeEnvironment&) noexcept;
std::unique_ptr<NativeAudioProcessingModule> audio_processor_new_config(const NativeEnvironment&, const FfiApmConfig&, bool, std::uint8_t&) noexcept;
std::uint8_t audio_processor_config_apply(NativeAudioProcessingModule&, const FfiApmConfig&) noexcept;
bool audio_processor_linear_output(NativeAudioProcessingModule&, rust::Slice<float>, std::uint8_t&) noexcept;
FfiApmStatistics audio_processor_statistics_default() noexcept;
FfiApmStatistics audio_processor_statistics(NativeAudioProcessingModule&, bool, bool) noexcept;
FfiApmConfig audio_processor_config_default() noexcept;
rust::String audio_processor_config_string(const FfiApmConfig&) noexcept;
FfiApmConfig audio_processor_config_get(const NativeAudioProcessingModule&) noexcept;
bool audio_processor_effects_admitted(const NativeEnvironment&, const FfiAudioProcessingConfig&) noexcept;
std::int32_t audio_processor_initialize(NativeAudioProcessingModule&, std::uint8_t&) noexcept;
FfiApmFormats audio_processor_formats_default() noexcept;
std::int32_t audio_processor_initialize_formats(NativeAudioProcessingModule&, const FfiApmFormats&, bool&, std::uint8_t&) noexcept;
std::uint8_t audio_processor_configure(NativeAudioProcessingModule&, const FfiAudioProcessingConfig&) noexcept;
FfiAudioProcessingFormatState audio_processor_format(const NativeAudioProcessingModule&) noexcept;
void audio_processor_set_muted(NativeAudioProcessingModule&, bool) noexcept;
bool audio_processor_muted(NativeAudioProcessingModule&) noexcept;
std::int32_t audio_processor_set_delay(NativeAudioProcessingModule&, std::int32_t) noexcept;
std::int32_t audio_processor_delay(const NativeAudioProcessingModule&) noexcept;
void audio_processor_set_analog_level(NativeAudioProcessingModule&, std::int32_t) noexcept;
std::int32_t audio_processor_analog_level(const NativeAudioProcessingModule&) noexcept;
void audio_processor_set_key(NativeAudioProcessingModule&, bool) noexcept;
std::int32_t audio_processor_i16(NativeAudioProcessingModule&, std::int32_t, std::size_t, rust::Slice<std::int16_t>, bool, std::uint8_t&) noexcept;
std::int32_t audio_processor_float(NativeAudioProcessingModule&, std::int32_t, std::size_t, rust::Slice<const float>, std::int32_t, std::size_t, rust::Slice<float>, bool, std::uint8_t&) noexcept;
std::int32_t audio_processor_analyze(NativeAudioProcessingModule&, std::int32_t, std::size_t, rust::Slice<const float>, std::uint8_t&) noexcept;
}
