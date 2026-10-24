#pragma once

#include <cstdint>
#include <cstddef>
#include <memory>

#include "rust/cxx.h"
#include "api/audio_options.h"

namespace webrtc { class AudioTrackInterface; }

namespace pulsebeam::webrtc_sys {
struct FfiAudioSourceOptions;
webrtc::AudioOptions media_audio_options_read(const FfiAudioSourceOptions&) noexcept;
FfiAudioSourceOptions media_audio_options_write(const webrtc::AudioOptions&) noexcept;
std::uint8_t media_audio_options_check(const FfiAudioSourceOptions&) noexcept;

class NativePeerConnectionFactory;
class NativePeerConnection;
class NativeRtpTransceiver;
class NativeRtpReceiver;
class NativeRtpSender;
struct FfiReceivedAudioFrame;
struct FfiEncodedAudioFrame;
struct FfiAudioCodecCapability;
struct FfiCodecFormat;
struct FfiAudioSourceOptions;
struct FfiAudioTrackStatus;
struct FfiOptionalI32;
struct FfiAudioObserverEvent;
class NativeAudioSource;
class NativeAudioTrack;

struct FfiAudioProcessorStatistics;
class NativeAudioTrackProcessor final {
 public:
  struct State;
  explicit NativeAudioTrackProcessor(std::unique_ptr<State> state) noexcept;
  ~NativeAudioTrackProcessor();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeAudioTrackProcessor> audio_track_processor(const NativeAudioTrack& track) noexcept;
bool audio_track_processors_same(const NativeAudioTrackProcessor& a, const NativeAudioTrackProcessor& b) noexcept;
FfiAudioProcessorStatistics audio_track_processor_stats(const NativeAudioTrackProcessor& processor, bool has_remote_tracks) noexcept;
FfiAudioProcessorStatistics audio_track_processor_statistics_default() noexcept;

// Internal synchronous SDK-handle access; never exported through CXX.
webrtc::AudioTrackInterface* audio_track_sdk_handle(const NativeAudioTrack& track) noexcept;
std::size_t audio_track_identity(const NativeAudioTrack& track) noexcept;

class NativeAudioSourceView final {
 public:
  struct State;
  explicit NativeAudioSourceView(std::unique_ptr<State> state) noexcept;
  ~NativeAudioSourceView();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeAudioObserver final {
 public:
  struct State;
  explicit NativeAudioObserver(std::unique_ptr<State> state) noexcept;
  ~NativeAudioObserver();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeAudioSourceView> audio_source_view(const NativeAudioSource& source) noexcept;
std::unique_ptr<NativeAudioSourceView> audio_track_source_view(const NativeAudioTrack& track) noexcept;
std::unique_ptr<NativeAudioTrack> rtp_receiver_audio_track(const NativePeerConnection& peer, const NativeRtpReceiver& receiver) noexcept;
std::unique_ptr<NativeAudioTrack> rtp_sender_audio_track(const NativePeerConnection& peer, const NativeRtpSender& sender) noexcept;
bool audio_tracks_same(const NativeAudioTrack& a, const NativeAudioTrack& b) noexcept;
bool audio_source_view_same(const NativeAudioSourceView& a, const NativeAudioSourceView& b) noexcept;
std::uint8_t audio_source_view_state(const NativeAudioSourceView& view) noexcept;
bool audio_source_view_remote(const NativeAudioSourceView& view) noexcept;
FfiAudioSourceOptions audio_source_view_options(const NativeAudioSourceView& view) noexcept;
std::uint8_t audio_source_view_set_options(const NativeAudioSourceView& view, const FfiAudioSourceOptions& options) noexcept;
double audio_source_view_volume(const NativeAudioSourceView& view) noexcept;
std::uint8_t audio_source_view_set_volume(const NativeAudioSourceView& view, double volume) noexcept;
std::unique_ptr<NativeAudioObserver> audio_source_view_observe(const NativeAudioSourceView& view, bool volume, std::size_t capacity) noexcept;
std::unique_ptr<NativeAudioObserver> audio_track_observe(const NativeAudioTrack& track, std::size_t capacity) noexcept;
FfiAudioObserverEvent audio_observer_take(const NativeAudioObserver& observer) noexcept;
std::uint64_t audio_observer_take_dropped(const NativeAudioObserver& observer) noexcept;
void audio_observer_close(const NativeAudioObserver& observer) noexcept;

class NativeAudioTrack;
class NativeMediaTrack;
std::unique_ptr<NativeMediaTrack> audio_track_reference(const NativeAudioTrack&) noexcept;
class NativeAudioSource final {
 public:
  struct State;
  explicit NativeAudioSource(std::unique_ptr<State> state) noexcept;
  ~NativeAudioSource();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeAudioSink final {
 public:
  struct State;
  explicit NativeAudioSink(std::unique_ptr<State> state) noexcept;
  ~NativeAudioSink();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeEncodedAudioSink final {
 public:
  struct State;
  explicit NativeEncodedAudioSink(std::unique_ptr<State> state) noexcept;
  ~NativeEncodedAudioSink();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeAudioTrack final {
 public:
  struct State;
  explicit NativeAudioTrack(std::unique_ptr<State> state) noexcept;
  ~NativeAudioTrack();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

std::unique_ptr<NativeAudioSource> create_audio_source(
    const NativePeerConnectionFactory& factory, std::uint8_t& failure) noexcept;
std::unique_ptr<NativeAudioSource> create_encoded_audio_source(
    const NativePeerConnectionFactory& factory, std::uint8_t channels) noexcept;
rust::Vec<FfiAudioCodecCapability> peer_audio_codec_capabilities(
    const NativePeerConnectionFactory& factory, bool sender) noexcept;
bool rtp_transceiver_set_audio_codec_preferences(
    const NativeRtpTransceiver& transceiver,
    const NativePeerConnectionFactory& factory,
    rust::Slice<const FfiCodecFormat> formats, std::uint8_t& error_type,
    rust::String& error) noexcept;
bool audio_source_capture_active(const NativeAudioSource& source) noexcept;
bool close_audio_source(const NativeAudioSource& source) noexcept;
bool audio_source_push_pcm(const NativeAudioSource& source,
                           rust::Slice<const std::int16_t> samples,
                           std::uint32_t sample_rate_hz,
                           std::uint8_t channels,
                           std::int64_t timestamp_us,
                           std::int64_t capture_time_us,
                           std::uint8_t& failure,
                           std::int32_t& processing_error, std::uint8_t& wrapper_error) noexcept;
bool audio_source_push_opus(const NativeAudioSource& source,
                            rust::Slice<const std::uint8_t> payload,
                            std::uint32_t rtp_timestamp,
                            std::uint32_t samples_per_channel) noexcept;
bool audio_source_push_opus_at(const NativeAudioSource& source,
                              rust::Slice<const std::uint8_t> payload,
                              std::uint32_t rtp_timestamp,
                              std::uint32_t samples_per_channel,
                              std::int64_t capture_time_us) noexcept;
bool audio_source_push_opus_with_level_at(const NativeAudioSource& source,
                              rust::Slice<const std::uint8_t> payload,
                              std::uint32_t rtp_timestamp,
                              std::uint32_t samples_per_channel,
                              std::int64_t capture_time_us, bool has_level,
                              std::uint8_t level_dbov, bool voice_activity) noexcept;
std::unique_ptr<NativeAudioTrack> create_microphone_track(
    const NativePeerConnectionFactory& factory, rust::Str id) noexcept;
std::unique_ptr<NativeAudioTrack> create_audio_track(
    const NativePeerConnectionFactory& factory, const NativeAudioSource& source,
    rust::Str id) noexcept;
std::unique_ptr<NativeAudioSink> audio_track_attach_sink(const NativeAudioTrack& track) noexcept;
bool rtp_sender_set_audio_track(const NativeRtpSender& sender, const NativeAudioTrack& track) noexcept;
FfiAudioSourceOptions audio_options_default() noexcept;
bool audio_options_equal(const FfiAudioSourceOptions& a, const FfiAudioSourceOptions& b) noexcept;
FfiAudioSourceOptions audio_options_set_all(const FfiAudioSourceOptions& value, const FfiAudioSourceOptions& changes) noexcept;
rust::String audio_options_description(const FfiAudioSourceOptions& value) noexcept;
std::uint8_t audio_source_set_options(const NativeAudioSource& source, const FfiAudioSourceOptions& options) noexcept;
std::uint8_t audio_source_set_volume(const NativeAudioSource& source, double volume) noexcept;
std::uint8_t audio_track_set_volume(const NativeAudioTrack& track, double volume) noexcept;
std::uint8_t rtp_sender_set_audio_track_volume(const NativePeerConnection& peer, const NativeRtpSender& sender, double volume) noexcept;
FfiAudioSourceOptions audio_source_options(const NativeAudioSource& source) noexcept;
std::uint8_t audio_source_state(const NativeAudioSource& source) noexcept;
bool audio_source_remote(const NativeAudioSource& source) noexcept;
double audio_source_volume(const NativeAudioSource& source) noexcept;
FfiAudioTrackStatus audio_track_status(const NativeAudioTrack& track) noexcept;
FfiAudioTrackStatus rtp_sender_audio_track_status(const NativePeerConnection& peer, const NativeRtpSender& sender) noexcept;
std::uint8_t audio_track_state(const NativeAudioTrack& track) noexcept;
FfiOptionalI32 audio_track_signal_level(const NativeAudioTrack& track) noexcept;
double audio_track_volume(const NativeAudioTrack& track) noexcept;
rust::String audio_track_kind(const NativeAudioTrack& track) noexcept;
rust::String audio_track_id(const NativeAudioTrack& track) noexcept;
bool audio_track_enabled(const NativeAudioTrack& track) noexcept;
bool audio_track_set_enabled(const NativeAudioTrack& track, bool enabled) noexcept;
bool audio_track_set_processing_options(const NativeAudioTrack& track,
                                        std::uint8_t echo, std::uint8_t noise,
                                        std::uint8_t gain,
                                        rust::String& error) noexcept;
std::unique_ptr<NativeAudioSink> rtp_receiver_attach_audio_sink(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver) noexcept;
FfiReceivedAudioFrame audio_sink_take_frame(const NativeAudioSink& sink) noexcept;
std::uint64_t audio_sink_dropped_frames(const NativeAudioSink& sink) noexcept;
bool close_audio_sink(const NativeAudioSink& sink) noexcept;
std::unique_ptr<NativeEncodedAudioSink> rtp_receiver_attach_encoded_audio_sink(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver) noexcept;
FfiEncodedAudioFrame encoded_audio_sink_take_frame(
    const NativeEncodedAudioSink& sink) noexcept;
std::uint64_t encoded_audio_sink_dropped_frames(
    const NativeEncodedAudioSink& sink) noexcept;
bool close_encoded_audio_sink(const NativeEncodedAudioSink& sink) noexcept;
class NativeRtpTransceiverInit;
class NativeRtcError;
std::unique_ptr<NativeRtpTransceiver> peer_add_audio_transceiver_value(
    const NativePeerConnection&, const NativeAudioTrack&, const NativeRtpTransceiverInit&,
    bool&, std::unique_ptr<NativeRtcError>&) noexcept;
std::unique_ptr<NativeRtpTransceiver> peer_add_audio_transceiver(
    const NativePeerConnection& peer, const NativeAudioTrack& track,
    std::uint8_t direction, std::uint8_t& error_type,
    rust::String& error) noexcept;

}  // namespace pulsebeam::webrtc_sys
