#pragma once
#include <cstddef>
#include <cstdint>
#include <memory>
#include "rust/cxx.h"
#include "api/scoped_refptr.h"
namespace webrtc { class AudioFrame; class AudioMixer; }
namespace pulsebeam::webrtc_sys {
struct FfiAudioFrameFields;
struct FfiMixerFrame;
struct FfiPcmViewInfo;
FfiPcmViewInfo pcm_i16_shape(rust::Slice<const std::int16_t>, std::size_t, std::size_t, std::uint8_t) noexcept;
FfiPcmViewInfo pcm_f32_shape(rust::Slice<const float>, std::size_t, std::size_t, std::uint8_t) noexcept;
FfiPcmViewInfo pcm_i16_planar_shape(rust::Slice<const std::size_t>, std::size_t) noexcept;
FfiPcmViewInfo pcm_f32_planar_shape(rust::Slice<const std::size_t>, std::size_t) noexcept;
void pcm_i16_clear(rust::Slice<const std::size_t>, std::size_t) noexcept;
void pcm_f32_clear(rust::Slice<const std::size_t>, std::size_t) noexcept;
std::uint8_t pcm_i16_copy(rust::Slice<std::int16_t>, rust::Slice<const std::int16_t>, std::size_t, std::size_t, bool, bool, bool) noexcept;
std::uint8_t pcm_f32_copy(rust::Slice<float>, rust::Slice<const float>, std::size_t, std::size_t, bool, bool, bool) noexcept;

// Native operations mutate one uniquely owned frame. The full backing buffer
// is initialized once before any Rust view or public-field assignment.
class NativeAudioFrame final {
 public:
  NativeAudioFrame();
  NativeAudioFrame(std::int32_t, std::size_t, std::int32_t);
  // Internal synchronous callback view. Never transfer this wrapper to an
  // owning Rust AudioFrame; the SDK frame and its backing storage stay borrowed.
  explicit NativeAudioFrame(webrtc::AudioFrame& frame) noexcept;
  // Native producer has already established its active prefix/mute state.
  // Initialize only remaining owned storage, never invent producer output.
  explicit NativeAudioFrame(std::unique_ptr<webrtc::AudioFrame> frame) noexcept;
  ~NativeAudioFrame();
  webrtc::AudioFrame& value() noexcept;
  const webrtc::AudioFrame& value() const noexcept;
 private:
  std::unique_ptr<webrtc::AudioFrame> owned_;
  webrtc::AudioFrame* frame_;
};
bool MediaAudioFrameViewValid(const webrtc::AudioFrame&) noexcept;
std::unique_ptr<NativeAudioFrame> media_audio_frame_new() noexcept;
std::unique_ptr<NativeAudioFrame> media_audio_frame_with_format(std::int32_t, std::size_t, std::int32_t) noexcept;
std::size_t media_audio_default_channel_size(std::size_t) noexcept;
std::int32_t media_channel_layout_count(std::int32_t) noexcept;
std::int32_t media_channel_order(std::int32_t, std::int32_t) noexcept;
std::int32_t media_channel_layout_guess(std::int32_t) noexcept;
rust::String media_channel_layout_string(std::int32_t) noexcept;
FfiAudioFrameFields media_audio_frame_fields(const NativeAudioFrame&) noexcept;
bool media_audio_frame_set_fields(NativeAudioFrame&, const FfiAudioFrameFields&) noexcept;
void media_audio_frame_reset(NativeAudioFrame&, bool) noexcept;
bool media_audio_frame_update(NativeAudioFrame&, std::uint32_t, rust::Slice<const std::int16_t>, bool,
    std::size_t, std::int32_t, std::int32_t, std::int32_t, std::size_t) noexcept;
bool media_audio_frame_copy(NativeAudioFrame&, const NativeAudioFrame&) noexcept;
rust::Slice<const std::int16_t> media_audio_frame_data(const NativeAudioFrame&) noexcept;
rust::Slice<const std::int16_t> media_audio_frame_data_view(const NativeAudioFrame&, bool&) noexcept;
rust::Slice<std::int16_t> media_audio_frame_mutable_data(NativeAudioFrame&) noexcept;
rust::Slice<std::int16_t> media_audio_frame_mutable_shape(NativeAudioFrame&, std::size_t, std::size_t, bool&) noexcept;
rust::Slice<std::int16_t> media_audio_frame_fill_shape(NativeAudioFrame&, std::size_t, std::size_t, std::int16_t, bool&) noexcept;
void media_audio_frame_mute(NativeAudioFrame&) noexcept;
bool media_audio_frame_muted(const NativeAudioFrame&) noexcept;
std::size_t media_audio_frame_max_samples(const NativeAudioFrame&) noexcept;
std::size_t media_audio_frame_samples_per_channel(const NativeAudioFrame&) noexcept;
std::size_t media_audio_frame_num_channels(const NativeAudioFrame&) noexcept;
std::int32_t media_audio_frame_layout(const NativeAudioFrame&) noexcept;
std::int32_t media_audio_frame_sample_rate(const NativeAudioFrame&) noexcept;
bool media_audio_frame_set_layout(NativeAudioFrame&, std::int32_t, std::size_t) noexcept;
std::int64_t media_audio_frame_capture_timestamp(const NativeAudioFrame&, bool&) noexcept;
void media_audio_frame_set_capture_timestamp(NativeAudioFrame&, std::int64_t) noexcept;
bool media_audio_frame_set_sample_rate(NativeAudioFrame&, std::int32_t) noexcept;
struct RustMixerSource;
class NativeMixerSource final {
 public:
  struct State;
  explicit NativeMixerSource(std::shared_ptr<State>) noexcept;
  ~NativeMixerSource();
  const std::shared_ptr<State>& state() const noexcept;
 private:
  std::shared_ptr<State> state_;
};
struct RustMixerProvider;
class NativeMixerSourceToken final {
 public:
  struct State;
  explicit NativeMixerSourceToken(std::shared_ptr<State>) noexcept;
  ~NativeMixerSourceToken();
  const std::shared_ptr<State>& state() const noexcept;
 private:
  std::shared_ptr<State> state_;
};
class NativeMixerInputs final {
 public:
  struct State;
  explicit NativeMixerInputs(const State&) noexcept;
  const State& state() const noexcept;
 private:
  const State& state_;
};
std::unique_ptr<NativeMixerSourceToken> mixer_token_clone(const NativeMixerSourceToken&) noexcept;
bool mixer_token_equal(const NativeMixerSourceToken&, const NativeMixerSourceToken&) noexcept;
std::int32_t mixer_input_ssrc(const NativeMixerInputs&, const NativeMixerSourceToken&, std::uint8_t&) noexcept;
std::int32_t mixer_input_rate(const NativeMixerInputs&, const NativeMixerSourceToken&, std::uint8_t&) noexcept;
FfiMixerFrame mixer_input_frame(const NativeMixerInputs&, const NativeMixerSourceToken&, std::int32_t, std::uint8_t&) noexcept;
struct RustMixerModule;
class NativeAudioMixer final {
 public:
  struct State;
  explicit NativeAudioMixer(rust::Box<RustMixerModule>);
  NativeAudioMixer(rust::Box<RustMixerModule>, rust::Box<RustMixerProvider>);
  ~NativeAudioMixer();
  State& state() noexcept;
  webrtc::scoped_refptr<webrtc::AudioMixer> mixer() const noexcept;
 private:
  std::shared_ptr<State> state_;
};
std::unique_ptr<NativeMixerSource> mixer_source_new(rust::Box<RustMixerSource>) noexcept;
FfiMixerFrame mixer_source_get_audio(const NativeMixerSource&, std::int32_t, std::uint8_t&) noexcept;
std::int32_t mixer_source_get_ssrc(const NativeMixerSource&, bool&) noexcept;
std::int32_t mixer_source_get_rate(const NativeMixerSource&, bool&) noexcept;
std::unique_ptr<NativeAudioMixer> audio_mixer_new(rust::Box<RustMixerModule>) noexcept;
std::unique_ptr<NativeAudioMixer> audio_mixer_from_provider(rust::Box<RustMixerModule>, rust::Box<RustMixerProvider>) noexcept;
bool audio_mixer_add(NativeAudioMixer&, const NativeMixerSource&, std::uint8_t&) noexcept;
bool audio_mixer_remove(NativeAudioMixer&, const NativeMixerSource&, std::uint8_t&) noexcept;
std::unique_ptr<NativeAudioFrame> audio_mixer_mix(NativeAudioMixer&, std::size_t, std::uint8_t&) noexcept;
}  // namespace pulsebeam::webrtc_sys
