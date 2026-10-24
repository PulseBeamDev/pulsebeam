#include "pulsebeam-webrtc-sys/native/audio_frame_values.h"
#include <algorithm>
#include <map>
#include <mutex>
#include "api/audio/audio_mixer.h"
#include "api/make_ref_counted.h"
#include "modules/audio_mixer/audio_mixer_impl.h"
#include "modules/audio_mixer/default_output_rate_calculator.h"
#include <cstring>
#include <limits>
#include <vector>
#include "api/audio/audio_frame.h"
#include "api/audio/audio_view.h"
#include "api/audio/channel_layout.h"
#include "api/rtp_packet_infos.h"
#include "pulsebeam-webrtc-sys/native/media_values.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {
namespace {
template <typename View>
FfiPcmViewInfo PcmInfo(const View& view) {
  FfiPcmViewInfo out;
  out.valid = true;
  out.channels = webrtc::NumChannels(view);
  out.samples = webrtc::SamplesPerChannel(view);
  out.size = view.size();
  out.empty = view.empty();
  out.mono = webrtc::IsMono(view);
  out.interleaved = webrtc::IsInterleavedView(view);
  return out;
}
template <typename T>
FfiPcmViewInfo PcmShape(rust::Slice<const T> data, std::size_t samples,
                      std::size_t channels, std::uint8_t layout) {
  if (layout == 0) return PcmInfo(webrtc::MonoView<const T>(data.data(), data.size()));
  if (channels && samples > data.size() / channels) return {};
  if (layout == 1) {
    if (channels > webrtc::kMaxNumberOfAudioChannels || (channels && !samples)) return {};
    return PcmInfo(webrtc::InterleavedView<const T>(data.data(), samples, channels));
  }
  if (layout == 2) return PcmInfo(webrtc::DeinterleavedView<const T>(data.data(), samples, channels));
  return {};
}
template <typename T>
FfiPcmViewInfo PlanarShape(rust::Slice<const std::size_t> pointers, std::size_t samples) {
  if (!pointers.empty() && samples > std::numeric_limits<std::size_t>::max() / pointers.size()) return {};
  std::vector<const T*> channels;
  for (auto pointer : pointers) channels.push_back(reinterpret_cast<const T*>(pointer));
  return PcmInfo(webrtc::DeinterleavedView<const T>(channels, samples));
}
template <typename T>
void PlanarClear(rust::Slice<const std::size_t> pointers, std::size_t samples) {
  std::vector<T*> channels;
  for (auto pointer : pointers) channels.push_back(reinterpret_cast<T*>(pointer));
  webrtc::DeinterleavedView<T>(channels, samples).Clear();
}
template <typename T>
std::uint8_t PcmCopy(rust::Slice<T> destination, rust::Slice<const T> source,
                     std::size_t samples, std::size_t channels,
                     bool destination_mono, bool source_mono, bool method) {
  if (channels > webrtc::kMaxNumberOfAudioChannels ||
      (channels && samples > source.size() / channels) ||
      (channels && samples > destination.size() / channels) ||
      ((destination_mono || source_mono) && channels != 1)) return 1;
  const auto size = samples * channels;  // Division checks proved no overflow.
  if (!size) return 2;  // Pinned memcpy helpers form element-zero addresses.
  webrtc::InterleavedView<T> out(destination.data(), samples, channels);
  webrtc::InterleavedView<const T> in(source.data(), samples, channels);
  if (method) { out.CopyFrom(in); return 0; }
  if (destination_mono && source_mono) {
    auto mono_out = out.AsMono();
    webrtc::CopySamples(mono_out, in.AsMono());
  } else if (destination_mono) {
    auto mono_out = out.AsMono();
    webrtc::CopySamples(mono_out, in);
  } else if (source_mono) {
    webrtc::CopySamples(out, in.AsMono());
  } else {
    webrtc::CopySamples(out, in);
  }
  return 0;
}
static_assert(webrtc::AudioFrame::kMaxDataSizeSamples == 7680);
static_assert(webrtc::kMaxNumberOfAudioChannels == 16);
bool CapacityShape(std::size_t samples, std::size_t channels) {
  // Division checks both multiplication overflow and the fixed buffer bound.
  return channels == 0 || samples <= webrtc::AudioFrame::kMaxDataSizeSamples / channels;
}
bool Enums(std::int32_t speech, std::int32_t vad) {
  return speech >= 0 && speech <= 7 && vad >= 0 && vad <= 3;
}
bool LayoutIndex(std::int32_t layout) { return layout >= 0 && layout <= 32; }
void Initialize(webrtc::AudioFrame& frame) {
  // A fresh frame is muted, so this zeros the entire backing array. Preserve
  // the constructor's mute state. Later shape changes may expose any prefix.
  frame.mutable_data();
  frame.Mute();
}
}  // namespace
FfiPcmViewInfo pcm_i16_shape(rust::Slice<const std::int16_t> data, std::size_t samples, std::size_t channels, std::uint8_t layout) noexcept { return PcmShape(data, samples, channels, layout); }
FfiPcmViewInfo pcm_f32_shape(rust::Slice<const float> data, std::size_t samples, std::size_t channels, std::uint8_t layout) noexcept { return PcmShape(data, samples, channels, layout); }
FfiPcmViewInfo pcm_i16_planar_shape(rust::Slice<const std::size_t> pointers, std::size_t samples) noexcept { return PlanarShape<std::int16_t>(pointers, samples); }
FfiPcmViewInfo pcm_f32_planar_shape(rust::Slice<const std::size_t> pointers, std::size_t samples) noexcept { return PlanarShape<float>(pointers, samples); }
void pcm_i16_clear(rust::Slice<const std::size_t> pointers, std::size_t samples) noexcept { PlanarClear<std::int16_t>(pointers, samples); }
void pcm_f32_clear(rust::Slice<const std::size_t> pointers, std::size_t samples) noexcept { PlanarClear<float>(pointers, samples); }
std::uint8_t pcm_i16_copy(rust::Slice<std::int16_t> out, rust::Slice<const std::int16_t> in, std::size_t samples, std::size_t channels, bool dm, bool sm, bool method) noexcept { return PcmCopy(out, in, samples, channels, dm, sm, method); }
std::uint8_t pcm_f32_copy(rust::Slice<float> out, rust::Slice<const float> in, std::size_t samples, std::size_t channels, bool dm, bool sm, bool method) noexcept { return PcmCopy(out, in, samples, channels, dm, sm, method); }

NativeAudioFrame::NativeAudioFrame()
    : owned_(std::make_unique<webrtc::AudioFrame>()), frame_(owned_.get()) { Initialize(*frame_); }
NativeAudioFrame::NativeAudioFrame(std::int32_t rate, std::size_t channels, std::int32_t layout)
    : owned_(std::make_unique<webrtc::AudioFrame>(rate, channels, static_cast<webrtc::ChannelLayout>(layout))),
      frame_(owned_.get()) { Initialize(*frame_); }
NativeAudioFrame::NativeAudioFrame(webrtc::AudioFrame& frame) noexcept : frame_(&frame) {}
NativeAudioFrame::NativeAudioFrame(std::unique_ptr<webrtc::AudioFrame> frame) noexcept
    : owned_(std::move(frame)), frame_(owned_.get()) {
  const bool muted = frame_->muted();
  auto* data = frame_->mutable_data();
  if (muted) frame_->Mute();
  else std::fill(data + frame_->samples_per_channel_ * frame_->num_channels_,
      data + webrtc::AudioFrame::kMaxDataSizeSamples, 0);
}
NativeAudioFrame::~NativeAudioFrame() = default;
webrtc::AudioFrame& NativeAudioFrame::value() noexcept { return *frame_; }
const webrtc::AudioFrame& NativeAudioFrame::value() const noexcept { return *frame_; }
bool MediaAudioFrameViewValid(const webrtc::AudioFrame& frame) noexcept {
  return CapacityShape(frame.samples_per_channel_, frame.num_channels_) &&
      frame.num_channels_ <= webrtc::kMaxNumberOfAudioChannels &&
      (frame.num_channels_ == 0 || frame.samples_per_channel_ != 0);
}
std::unique_ptr<NativeAudioFrame> media_audio_frame_new() noexcept { return std::make_unique<NativeAudioFrame>(); }
std::unique_ptr<NativeAudioFrame> media_audio_frame_with_format(std::int32_t rate, std::size_t channels, std::int32_t layout) noexcept {
  if (rate < 100 || rate > 192000 || channels > webrtc::kMaxNumberOfAudioChannels ||
      layout < 0 || layout > 63 || !CapacityShape(static_cast<std::size_t>(rate) / 100, channels)) return nullptr;
  return std::make_unique<NativeAudioFrame>(rate, channels, layout);
}
std::size_t media_audio_default_channel_size(std::size_t rate) noexcept {
  return rate > 192000 ? 0 : webrtc::SampleRateToDefaultChannelSize(rate);
}
std::int32_t media_channel_layout_count(std::int32_t layout) noexcept {
  if (!LayoutIndex(layout)) return 0;
  return webrtc::ChannelLayoutToChannelCount(static_cast<webrtc::ChannelLayout>(layout));
}
std::int32_t media_channel_order(std::int32_t layout, std::int32_t channel) noexcept {
  if (!LayoutIndex(layout) || channel < 0 || channel > 10) return -1;
  return webrtc::ChannelOrder(static_cast<webrtc::ChannelLayout>(layout), static_cast<webrtc::Channels>(channel));
}
std::int32_t media_channel_layout_guess(std::int32_t channels) noexcept {
  return static_cast<std::int32_t>(webrtc::GuessChannelLayout(channels));
}
rust::String media_channel_layout_string(std::int32_t layout) noexcept {
  if (!LayoutIndex(layout)) return rust::String();
  return rust::String(webrtc::ChannelLayoutToString(static_cast<webrtc::ChannelLayout>(layout)));
}
FfiAudioFrameFields media_audio_frame_fields(const NativeAudioFrame& owner) noexcept {
  const auto& frame = owner.value();
  FfiAudioFrameFields result{};
  result.timestamp = frame.timestamp_; result.elapsed_time_ms = frame.elapsed_time_ms_;
  result.ntp_time_ms = frame.ntp_time_ms_; result.samples_per_channel = frame.samples_per_channel_;
  result.sample_rate_hz = frame.sample_rate_hz_; result.num_channels = frame.num_channels_;
  result.speech_type = static_cast<std::int32_t>(frame.speech_type_);
  result.vad_activity = static_cast<std::int32_t>(frame.vad_activity_);
  result.packet_infos = MediaPacketInfosToFfi(frame.packet_infos_);
  return result;
}
bool media_audio_frame_set_fields(NativeAudioFrame& owner, const FfiAudioFrameFields& value) noexcept {
  if (!CapacityShape(value.samples_per_channel, value.num_channels) || !Enums(value.speech_type, value.vad_activity)) return false;
  auto& frame = owner.value();
  frame.timestamp_ = value.timestamp; frame.elapsed_time_ms_ = value.elapsed_time_ms;
  frame.ntp_time_ms_ = value.ntp_time_ms; frame.samples_per_channel_ = value.samples_per_channel;
  frame.sample_rate_hz_ = value.sample_rate_hz; frame.num_channels_ = value.num_channels;
  frame.speech_type_ = static_cast<webrtc::AudioFrame::SpeechType>(value.speech_type);
  frame.vad_activity_ = static_cast<webrtc::AudioFrame::VADActivity>(value.vad_activity);
  frame.packet_infos_ = MediaPacketInfosFromFfi(rust::Slice<const FfiRtpReceivedPacketInfo>(value.packet_infos.data(), value.packet_infos.size()));
  return true;
}
void media_audio_frame_reset(NativeAudioFrame& owner, bool mute) noexcept {
  if (mute) owner.value().Reset(); else owner.value().ResetWithoutMuting();
}
bool media_audio_frame_update(NativeAudioFrame& owner, std::uint32_t timestamp, rust::Slice<const std::int16_t> data,
    bool present, std::size_t samples, std::int32_t rate, std::int32_t speech, std::int32_t vad, std::size_t channels) noexcept {
  if (!CapacityShape(samples, channels) || channels > webrtc::kMaxNumberOfAudioChannels || !Enums(speech, vad)) return false;
  const auto length = samples * channels;
  if (present && data.size() < length) return false;
  const std::int16_t empty = 0;
  const auto* input = !present ? nullptr : length == 0 ? &empty : data.data();
  owner.value().UpdateFrame(timestamp, input, samples, rate, static_cast<webrtc::AudioFrame::SpeechType>(speech),
      static_cast<webrtc::AudioFrame::VADActivity>(vad), channels);
  return true;
}
bool media_audio_frame_copy(NativeAudioFrame& target, const NativeAudioFrame& source) noexcept {
  if (&target == &source) return true;
  if (!MediaAudioFrameViewValid(source.value())) return false;
  target.value().CopyFrom(source.value());
  return true;
}
rust::Slice<const std::int16_t> media_audio_frame_data(const NativeAudioFrame& owner) noexcept {
  return {owner.value().data(), owner.value().max_16bit_samples()};
}
rust::Slice<const std::int16_t> media_audio_frame_data_view(const NativeAudioFrame& owner, bool& valid) noexcept {
  valid = MediaAudioFrameViewValid(owner.value());
  if (!valid) return {};
  const auto view = owner.value().data_view().data();
  return {view.empty() ? owner.value().data() : view.data(), view.size()};
}
rust::Slice<std::int16_t> media_audio_frame_mutable_data(NativeAudioFrame& owner) noexcept {
  return {owner.value().mutable_data(), owner.value().max_16bit_samples()};
}
rust::Slice<std::int16_t> media_audio_frame_mutable_shape(NativeAudioFrame& owner, std::size_t samples, std::size_t channels, bool& valid) noexcept {
  valid = CapacityShape(samples, channels) && channels <= webrtc::kMaxNumberOfAudioChannels && channels <= samples;
  if (!valid) return {};
  auto view = owner.value().mutable_data(samples, channels).data();
  // This native view always starts at the real backing buffer, including empty
  // views. No second mutable_data() call: that would change prefix-only zeroing.
  return {view.data(), view.size()};
}
rust::Slice<std::int16_t> media_audio_frame_fill_shape(NativeAudioFrame& owner, std::size_t samples, std::size_t channels, std::int16_t value, bool& valid) noexcept {
  valid = CapacityShape(samples, channels) && channels <= webrtc::kMaxNumberOfAudioChannels && channels <= samples;
  if (!valid) return {};
  auto view = owner.value().mutable_data(samples, channels).data();
  // Initialize before constructing any Rust slice of a borrowed SDK buffer.
  std::fill(view.begin(), view.end(), value);
  return {view.data(), view.size()};
}
void media_audio_frame_mute(NativeAudioFrame& owner) noexcept { owner.value().Mute(); }
bool media_audio_frame_muted(const NativeAudioFrame& owner) noexcept { return owner.value().muted(); }
std::size_t media_audio_frame_max_samples(const NativeAudioFrame& owner) noexcept { return owner.value().max_16bit_samples(); }
std::size_t media_audio_frame_samples_per_channel(const NativeAudioFrame& owner) noexcept { return owner.value().samples_per_channel(); }
std::size_t media_audio_frame_num_channels(const NativeAudioFrame& owner) noexcept { return owner.value().num_channels(); }
std::int32_t media_audio_frame_layout(const NativeAudioFrame& owner) noexcept { return static_cast<std::int32_t>(owner.value().channel_layout()); }
std::int32_t media_audio_frame_sample_rate(const NativeAudioFrame& owner) noexcept { return owner.value().sample_rate_hz(); }
bool media_audio_frame_set_layout(NativeAudioFrame& owner, std::int32_t layout, std::size_t channels) noexcept {
  if (!LayoutIndex(layout) || !CapacityShape(owner.value().samples_per_channel_, channels)) return false;
  const auto native = static_cast<webrtc::ChannelLayout>(layout);
  const auto expected = webrtc::ChannelLayoutToChannelCount(native);
  if (expected != 0 && static_cast<std::size_t>(expected) != channels) return false;
  owner.value().SetLayoutAndNumChannels(native, channels);
  return true;
}
std::int64_t media_audio_frame_capture_timestamp(const NativeAudioFrame& owner, bool& present) noexcept {
  const auto value = owner.value().absolute_capture_timestamp_ms();
  present = value.has_value();
  return value.value_or(0);
}
void media_audio_frame_set_capture_timestamp(NativeAudioFrame& owner, std::int64_t timestamp) noexcept {
  owner.value().set_absolute_capture_timestamp_ms(timestamp);
}
bool media_audio_frame_set_sample_rate(NativeAudioFrame& owner, std::int32_t rate) noexcept {
  if (rate < 0 || rate > 192000 || !CapacityShape(static_cast<std::size_t>(rate) / 100, owner.value().num_channels_)) return false;
  owner.value().SetSampleRateAndChannelSize(rate);
  return true;
}
namespace {
thread_local std::uint8_t* active_mix_error = nullptr;
class MixScope {
 public:
  explicit MixScope(std::uint8_t& error) { active_mix_error = &error; }
  ~MixScope() { active_mix_error = nullptr; }
};
void MixFailure(std::uint8_t error) { if (active_mix_error && !*active_mix_error) *active_mix_error = error; }
void DrainMixerFrame(webrtc::AudioFrame* output, std::size_t channels) {
  // Void SDK failure drain only, never a successful owning mix result. Native
  // audio transport still requires a valid shape before its resampler access.
  output->UpdateFrame(0, nullptr, 80, 8000, webrtc::AudioFrame::kNormalSpeech,
      webrtc::AudioFrame::kVadUnknown, std::clamp<std::size_t>(channels, 1, webrtc::kMaxNumberOfAudioChannels));
}
class GuardedRateCalculator final : public webrtc::OutputRateCalculator {
 public:
  int CalculateOutputRateFromRange(std::span<const int> rates) override {
    if (!rates.empty()) {
      const auto maximum = *std::max_element(rates.begin(), rates.end());
      if (maximum < 8000 || maximum > 48000) {
        MixFailure(2);
        // The SDK has no rate error path. Finish its call without consuming
        // source frames; the owning operation refuses this output.
        return 8000;
      }
    }
    return original_.CalculateOutputRateFromRange(rates);
  }
 private:
  webrtc::DefaultOutputRateCalculator original_;
};
}
struct NativeMixerSource::State final : webrtc::AudioMixer::Source {
  explicit State(rust::Box<RustMixerSource> owner) : owner(std::move(owner)) {}
  AudioFrameInfo GetAudioFrameWithInfo(int rate, webrtc::AudioFrame* output) override {
    if (active_mix_error && *active_mix_error) return AudioFrameInfo::kError;
    auto frame = mixer_source_frame(*owner, rate);
    if (frame.failed) { MixFailure(4); return AudioFrameInfo::kError; }
    if (frame.disposition == 2) return AudioFrameInfo::kError;
    if (!frame.frame) { MixFailure(3); mixer_source_invalid(*owner); return AudioFrameInfo::kError; }
    const auto& value = frame.frame->value();
    if (!MediaAudioFrameViewValid(value) || value.sample_rate_hz_ != rate ||
        value.samples_per_channel_ != static_cast<std::size_t>(rate / 100) ||
        (value.num_channels_ != 1 && value.num_channels_ != 2) ||
        (frame.disposition == 0 && value.muted())) {
      MixFailure(3); mixer_source_invalid(*owner); return AudioFrameInfo::kError;
    }
    output->CopyFrom(value);
    return frame.disposition == 1 ? AudioFrameInfo::kMuted : AudioFrameInfo::kNormal;
  }
  int Ssrc() const override { bool valid = false; const auto v = mixer_source_ssrc(*owner, valid); if (!valid) MixFailure(4); return v; }
  int PreferredSampleRate() const override { bool valid = false; const auto v = mixer_source_rate(*owner, valid); if (!valid) MixFailure(4); return v; }
  const rust::Box<RustMixerSource> owner;
};
// Tokens retain registration identity only, never an unowned callable source.
// All source dereferences occur under CallerMixer's full callback lock.
struct NativeMixerSourceToken::State {
  explicit State(webrtc::AudioMixer::Source* source) : source(source) {}
  webrtc::AudioMixer::Source* const source;
};
struct NativeMixerInputs::State {
  std::map<webrtc::AudioMixer::Source*, std::shared_ptr<NativeMixerSourceToken::State>> sources;
};
NativeMixerSourceToken::NativeMixerSourceToken(std::shared_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeMixerSourceToken::~NativeMixerSourceToken() = default;
const std::shared_ptr<NativeMixerSourceToken::State>& NativeMixerSourceToken::state() const noexcept { return state_; }
NativeMixerInputs::NativeMixerInputs(const State& state) noexcept : state_(state) {}
const NativeMixerInputs::State& NativeMixerInputs::state() const noexcept { return state_; }
std::unique_ptr<NativeMixerSourceToken> mixer_token_clone(const NativeMixerSourceToken& token) noexcept {
  return std::make_unique<NativeMixerSourceToken>(token.state());
}
bool mixer_token_equal(const NativeMixerSourceToken& a, const NativeMixerSourceToken& b) noexcept { return a.state() == b.state(); }
namespace {
webrtc::AudioMixer::Source* RegisteredSource(const NativeMixerInputs& inputs, const NativeMixerSourceToken& token, std::uint8_t& error) {
  error = 0;
  const auto found = inputs.state().sources.find(token.state()->source);
  if (found == inputs.state().sources.end() || found->second != token.state()) { error = 5; return nullptr; }
  return found->first;
}
}
std::int32_t mixer_input_ssrc(const NativeMixerInputs& inputs, const NativeMixerSourceToken& token, std::uint8_t& error) noexcept {
  auto* source = RegisteredSource(inputs, token, error); if (!source) return 0;
  const auto value = source->Ssrc(); if (active_mix_error && *active_mix_error) error = *active_mix_error; return value;
}
std::int32_t mixer_input_rate(const NativeMixerInputs& inputs, const NativeMixerSourceToken& token, std::uint8_t& error) noexcept {
  auto* source = RegisteredSource(inputs, token, error); if (!source) return 0;
  const auto value = source->PreferredSampleRate(); if (active_mix_error && *active_mix_error) error = *active_mix_error; return value;
}
FfiMixerFrame mixer_input_frame(const NativeMixerInputs& inputs, const NativeMixerSourceToken& token, std::int32_t rate, std::uint8_t& error) noexcept {
  FfiMixerFrame result{}; result.disposition = 2;
  auto* source = RegisteredSource(inputs, token, error); if (!source) return result;
  if (rate != 8000 && rate != 16000 && rate != 32000 && rate != 48000) { error = 6; return result; }
  auto frame = std::make_unique<webrtc::AudioFrame>();
  const auto disposition = source->GetAudioFrameWithInfo(rate, frame.get());
  if (active_mix_error && *active_mix_error) { error = *active_mix_error; return result; }
  if (disposition == webrtc::AudioMixer::Source::AudioFrameInfo::kError) return result;
  if (!MediaAudioFrameViewValid(*frame) || frame->sample_rate_hz_ != rate ||
      frame->samples_per_channel_ != static_cast<std::size_t>(rate / 100) ||
      (frame->num_channels_ != 1 && frame->num_channels_ != 2) ||
      (disposition == webrtc::AudioMixer::Source::AudioFrameInfo::kNormal && frame->muted())) {
    error = 3; MixFailure(3); return result;
  }
  result.disposition = disposition == webrtc::AudioMixer::Source::AudioFrameInfo::kMuted ? 1 : 0;
  if (result.disposition == 1) frame->Mute();
  result.frame = std::make_unique<NativeAudioFrame>(std::move(frame)); return result;
}
namespace {
class CallerMixer : public webrtc::AudioMixer {
 public:
  explicit CallerMixer(rust::Box<RustMixerProvider> provider) : provider_(std::move(provider)) {}
  bool AddSource(Source* source) override {
    std::lock_guard lock(operations_);
    if (inputs_.sources.contains(source)) return false;
    auto registration = std::make_shared<NativeMixerSourceToken::State>(source);
    inputs_.sources.emplace(source, registration);
    NativeMixerInputs inputs(inputs_);
    bool failed = false;
    const auto added = mixer_provider_add(*provider_, std::make_unique<NativeMixerSourceToken>(registration), inputs, failed);
    if (failed || (active_mix_error && *active_mix_error)) { inputs_.sources.erase(source); MixFailure(4); return false; }
    if (!added) inputs_.sources.erase(source);
    return added;
  }
  void RemoveSource(Source* source) override {
    std::lock_guard lock(operations_);
    const auto found = inputs_.sources.find(source); if (found == inputs_.sources.end()) return;
    auto token = std::make_unique<NativeMixerSourceToken>(found->second);
    NativeMixerInputs inputs(inputs_);
    const bool removed = mixer_provider_remove(*provider_, std::move(token), inputs);
    // Info loans end at callback return. Always retire before returning to the
    // SDK, even on failure, because it may now destroy the raw source.
    inputs_.sources.erase(source);
    if (!removed) MixFailure(4);
  }
  void Mix(std::size_t channels, webrtc::AudioFrame* output) override {
    std::lock_guard lock(operations_);
    NativeMixerInputs inputs(inputs_);
    auto frame = mixer_provider_mix(*provider_, channels, inputs);
    if (frame.failed) { MixFailure(4); output->Mute(); return; }
    if (!frame.frame) { Invalid(output); return; }
    const auto& value = frame.frame->value();
    if (!MediaAudioFrameViewValid(value) || value.num_channels_ == 0 || value.num_channels_ != channels ||
        (value.sample_rate_hz_ != 8000 && value.sample_rate_hz_ != 16000 &&
         value.sample_rate_hz_ != 32000 && value.sample_rate_hz_ != 48000) ||
        value.samples_per_channel_ != static_cast<std::size_t>(value.sample_rate_hz_ / 100)) { Invalid(output); return; }
    if (active_mix_error && *active_mix_error) { output->Mute(); return; }
    output->CopyFrom(value);
  }
 private:
  void Invalid(webrtc::AudioFrame* output) { MixFailure(3); mixer_provider_invalid(*provider_); output->Mute(); }
  std::recursive_mutex operations_;
  rust::Box<RustMixerProvider> provider_;
  NativeMixerInputs::State inputs_;
};
}
struct NativeAudioMixer::State {
  explicit State(rust::Box<RustMixerModule> module) : owner(std::move(module)) {}
  rust::Box<RustMixerModule> owner;
  std::map<NativeMixerSource::State*, std::shared_ptr<NativeMixerSource::State>> sources;
  webrtc::scoped_refptr<webrtc::AudioMixer> mixer;
};
NativeMixerSource::NativeMixerSource(std::shared_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeMixerSource::~NativeMixerSource() = default;
const std::shared_ptr<NativeMixerSource::State>& NativeMixerSource::state() const noexcept { return state_; }
std::unique_ptr<NativeMixerSource> mixer_source_new(rust::Box<RustMixerSource> source) noexcept {
  return std::make_unique<NativeMixerSource>(std::make_shared<NativeMixerSource::State>(std::move(source)));
}
FfiMixerFrame mixer_source_get_audio(const NativeMixerSource& source, std::int32_t rate, std::uint8_t& error) noexcept {
  error = 0; FfiMixerFrame result{}; result.disposition = 2;
  if (active_mix_error) { error = 1; return result; }
  MixScope scope(error);
  auto frame = std::make_unique<webrtc::AudioFrame>();
  const auto disposition = source.state()->GetAudioFrameWithInfo(rate, frame.get());
  if (error) { result.failed = true; return result; }
  if (disposition == webrtc::AudioMixer::Source::AudioFrameInfo::kError) return result;
  result.disposition = disposition == webrtc::AudioMixer::Source::AudioFrameInfo::kMuted ? 1 : 0;
  if (result.disposition == 1) frame->Mute();
  result.frame = std::make_unique<NativeAudioFrame>(std::move(frame));
  return result;
}
std::int32_t mixer_source_get_ssrc(const NativeMixerSource& source, bool& valid) noexcept { return mixer_source_ssrc(*source.state()->owner, valid); }
std::int32_t mixer_source_get_rate(const NativeMixerSource& source, bool& valid) noexcept { return mixer_source_rate(*source.state()->owner, valid); }
NativeAudioMixer::NativeAudioMixer(rust::Box<RustMixerModule> module) : state_(std::make_shared<State>(std::move(module))) {
  state_->mixer = webrtc::AudioMixerImpl::Create(std::make_unique<GuardedRateCalculator>(), true);
}
NativeAudioMixer::NativeAudioMixer(rust::Box<RustMixerModule> module, rust::Box<RustMixerProvider> provider) : state_(std::make_shared<State>(std::move(module))) {
  state_->mixer = webrtc::make_ref_counted<CallerMixer>(std::move(provider));
}
NativeAudioMixer::~NativeAudioMixer() = default;
NativeAudioMixer::State& NativeAudioMixer::state() noexcept { return *state_; }
namespace {
// Installation retains the exact selected SDK mixer plus every registered Rust
// participant. A scope is required even when the SDK invokes Mix directly.
class InstalledMixer : public webrtc::AudioMixer {
 public:
  explicit InstalledMixer(std::shared_ptr<NativeAudioMixer::State> state) : state_(std::move(state)) {}
  bool AddSource(Source* source) override {
    if (active_mix_error) { MixFailure(1); mixer_module_failed(*state_->owner, 1); return false; }
    std::uint8_t error = 0; MixScope scope(error);
    const bool added = state_->mixer->AddSource(source);
    if (error) mixer_module_failed(*state_->owner, error);
    return added && !error;
  }
  void RemoveSource(Source* source) override {
    if (active_mix_error) {
      // Caller registries must retire the raw pointer even if user removal
      // reenters; native stock lock reentry remains a qualification boundary.
      MixFailure(1); mixer_module_failed(*state_->owner, 1);
      state_->mixer->RemoveSource(source); return;
    }
    std::uint8_t error = 0; MixScope scope(error);
    state_->mixer->RemoveSource(source);
    if (error) mixer_module_failed(*state_->owner, error);
  }
  void Mix(std::size_t channels, webrtc::AudioFrame* output) override {
    if (active_mix_error) {
      MixFailure(1); mixer_module_failed(*state_->owner, 1);
      DrainMixerFrame(output, channels);
      return;
    }
    std::uint8_t error = 0; MixScope scope(error);
    state_->mixer->Mix(channels, output);
    if (!MediaAudioFrameViewValid(*output)) error = 3;
    if (error) { mixer_module_failed(*state_->owner, error); DrainMixerFrame(output, channels); }
    // Native Mix is void. A failed installed request is observable through the
    // retained mixer observer, but no native success/status is invented here.
  }
 private:
  std::shared_ptr<NativeAudioMixer::State> state_;
};
}
webrtc::scoped_refptr<webrtc::AudioMixer> NativeAudioMixer::mixer() const noexcept { return webrtc::make_ref_counted<InstalledMixer>(state_); }
std::unique_ptr<NativeAudioMixer> audio_mixer_new(rust::Box<RustMixerModule> module) noexcept { return std::make_unique<NativeAudioMixer>(std::move(module)); }
std::unique_ptr<NativeAudioMixer> audio_mixer_from_provider(rust::Box<RustMixerModule> module, rust::Box<RustMixerProvider> provider) noexcept { return std::make_unique<NativeAudioMixer>(std::move(module), std::move(provider)); }
bool audio_mixer_add(NativeAudioMixer& mixer, const NativeMixerSource& source, std::uint8_t& error) noexcept {
  error = 0;
  if (active_mix_error) { error = 1; return false; }
  auto& state = mixer.state();
  if (state.sources.contains(source.state().get())) return false;
  MixScope scope(error);
  if (!state.mixer->AddSource(source.state().get()) || error) return false;
  state.sources.emplace(source.state().get(), source.state());
  return true;
}
bool audio_mixer_remove(NativeAudioMixer& mixer, const NativeMixerSource& source, std::uint8_t& error) noexcept {
  error = 0;
  if (active_mix_error) { error = 1; return false; }
  auto& state = mixer.state();
  const auto found = state.sources.find(source.state().get());
  if (found == state.sources.end()) return false;
  MixScope scope(error);
  state.mixer->RemoveSource(found->first);
  state.sources.erase(found);
  return error == 0;
}
std::unique_ptr<NativeAudioFrame> audio_mixer_mix(NativeAudioMixer& mixer, std::size_t channels, std::uint8_t& error) noexcept {
  error = 0;
  if (active_mix_error) { error = 1; return nullptr; }
  if (channels == 0) { error = 3; return nullptr; }
  MixScope scope(error);
  auto output = std::make_unique<webrtc::AudioFrame>();
  mixer.state().mixer->Mix(channels, output.get());
  if (error) { mixer_module_failed(*mixer.state().owner, error); return nullptr; }
  if (!MediaAudioFrameViewValid(*output)) { error = 3; mixer_module_failed(*mixer.state().owner, error); return nullptr; }
  return std::make_unique<NativeAudioFrame>(std::move(output));
}
}  // namespace pulsebeam::webrtc_sys
