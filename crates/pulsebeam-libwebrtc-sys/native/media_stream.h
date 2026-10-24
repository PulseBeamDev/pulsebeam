#pragma once
#include <cstdint>
#include <memory>
#include <vector>
#include "api/scoped_refptr.h"
#include "rust/cxx.h"

namespace webrtc { class MediaStreamInterface; class MediaStreamTrackInterface; }
namespace pulsebeam::webrtc_sys {
class NativePeerConnectionFactory;
class NativeMediaTrack;
class NativeRtpReceiver;
struct FfiAudioSourceOptions;
struct FfiAudioProcessorStats;
struct FfiTrackAudioOptionsResult;
struct FfiVideoSourceInfo;
struct FfiOptionalF64;
struct FfiOptionalI32;
struct FfiRtpByteString;

class NativeTrackSource final {
 public:
  struct State;
  explicit NativeTrackSource(std::unique_ptr<State>) noexcept;
  ~NativeTrackSource();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeAudioProcessor final {
 public:
  struct State;
  explicit NativeAudioProcessor(std::unique_ptr<State>) noexcept;
  ~NativeAudioProcessor();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeMediaObserver final {
 public:
  struct State;
  explicit NativeMediaObserver(std::unique_ptr<State>) noexcept;
  ~NativeMediaObserver();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeMediaStream final {
 public:
  explicit NativeMediaStream(webrtc::scoped_refptr<webrtc::MediaStreamInterface>) noexcept;
  ~NativeMediaStream();
  const webrtc::scoped_refptr<webrtc::MediaStreamInterface>& stream() const noexcept;
 private:
  webrtc::scoped_refptr<webrtc::MediaStreamInterface> stream_;
};
class NativeMediaStreams final {
 public:
  explicit NativeMediaStreams(std::vector<webrtc::scoped_refptr<webrtc::MediaStreamInterface>>) noexcept;
  ~NativeMediaStreams();
  const std::vector<webrtc::scoped_refptr<webrtc::MediaStreamInterface>>& streams() const noexcept;
 private:
  std::vector<webrtc::scoped_refptr<webrtc::MediaStreamInterface>> streams_;
};
class NativeMediaTracks final {
 public:
  explicit NativeMediaTracks(std::vector<webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface>>) noexcept;
  ~NativeMediaTracks();
  const std::vector<webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface>>& tracks() const noexcept;
 private:
  std::vector<webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface>> tracks_;
};
std::unique_ptr<NativeTrackSource> media_track_source(const NativeMediaTrack&, const NativePeerConnectionFactory&, bool video) noexcept;
std::int32_t track_source_state(const NativeTrackSource&) noexcept;
bool track_source_remote(const NativeTrackSource&) noexcept;
bool track_source_same_resource(const NativeTrackSource&, const NativeTrackSource&) noexcept;
FfiAudioSourceOptions track_source_audio_options(const NativeTrackSource&) noexcept;
FfiVideoSourceInfo track_source_video_info(const NativeTrackSource&) noexcept;
void media_track_set_volume(const NativeMediaTrack&, double) noexcept;
void media_track_set_should_receive(const NativeMediaTrack&, bool) noexcept;
void media_track_set_content_hint(const NativeMediaTrack&, std::int32_t) noexcept;
FfiTrackAudioOptionsResult media_track_set_audio_options(const NativeMediaTrack&, const FfiAudioSourceOptions&) noexcept;
FfiOptionalF64 media_track_audio_volume(const NativeMediaTrack&) noexcept;
FfiOptionalI32 media_track_signal_level(const NativeMediaTrack&) noexcept;
std::int32_t media_track_should_receive(const NativeMediaTrack&) noexcept;
std::int32_t media_track_content_hint(const NativeMediaTrack&) noexcept;
std::unique_ptr<NativeAudioProcessor> media_track_audio_processor(const NativeMediaTrack&, const NativePeerConnectionFactory&) noexcept;
FfiAudioProcessorStats audio_processor_stats(const NativeAudioProcessor&, bool has_remote_tracks) noexcept;
bool audio_processor_same_resource(const NativeAudioProcessor&, const NativeAudioProcessor&) noexcept;

std::unique_ptr<NativeMediaStreams> rtp_receiver_streams(const NativeRtpReceiver&) noexcept;
bool media_stream_mutate_track(const NativeMediaStream&, const NativeMediaTrack&, bool remove) noexcept;
std::size_t media_streams_len(const NativeMediaStreams&) noexcept;
std::unique_ptr<NativeMediaStream> media_streams_get(const NativeMediaStreams&, std::size_t) noexcept;
rust::Vec<std::uint8_t> media_stream_id(const NativeMediaStream&) noexcept;
bool media_stream_same_resource(const NativeMediaStream&, const NativeMediaStream&) noexcept;
std::unique_ptr<NativeMediaTracks> media_stream_tracks(const NativeMediaStream&, bool video) noexcept;
std::size_t media_tracks_len(const NativeMediaTracks&) noexcept;
std::unique_ptr<NativeMediaTrack> media_tracks_get(const NativeMediaTracks&, std::size_t) noexcept;
std::unique_ptr<NativeMediaTrack> media_stream_find(const NativeMediaStream&, bool video, rust::Slice<const std::uint8_t>) noexcept;
std::unique_ptr<NativeMediaObserver> media_track_observe(const NativeMediaTrack&, const NativePeerConnectionFactory&, std::size_t) noexcept;
std::unique_ptr<NativeMediaObserver> media_stream_observe(const NativeMediaStream&, const NativePeerConnectionFactory&, std::size_t) noexcept;
bool media_observer_take(const NativeMediaObserver&) noexcept;
std::uint64_t media_observer_take_dropped(const NativeMediaObserver&) noexcept;
}  // namespace pulsebeam::webrtc_sys
