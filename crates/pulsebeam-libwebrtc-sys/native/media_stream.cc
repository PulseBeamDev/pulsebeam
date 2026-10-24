#include "pulsebeam-webrtc-sys/native/audio.h"
#include "pulsebeam-webrtc-sys/native/media_stream.h"
#include "api/media_stream_interface.h"
#include "api/audio_options.h"
#include "api/audio/audio_processing_statistics.h"
#include "rtc_base/thread.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/peer.h"
#include "pulsebeam-webrtc-sys/native/video.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <limits>
#include <mutex>
#include <optional>
#include <string>
#include <utility>
namespace pulsebeam::webrtc_sys {
namespace {
rust::Vec<std::uint8_t> Bytes(const std::string& s) { rust::Vec<std::uint8_t> out; out.reserve(s.size()); for (unsigned char c : s) out.push_back(c); return out; }
template<class T> std::int8_t Choice(const std::optional<T>& x) { return x ? static_cast<std::int8_t>(*x) : -1; }
FfiOptionalI32 Optional(std::optional<std::int32_t> x) { return {x.has_value(), x.value_or(0)}; }
FfiOptionalF64 Optional(std::optional<double> x) { return {x.has_value(), x.value_or(0.0)}; }
webrtc::AudioTrackInterface* Audio(const NativeMediaTrack& t) { return t.track()->kind() == webrtc::MediaStreamTrackInterface::kAudioKind ? static_cast<webrtc::AudioTrackInterface*>(t.track().get()) : nullptr; }
webrtc::VideoTrackInterface* Video(const NativeMediaTrack& t) { return t.track()->kind() == webrtc::MediaStreamTrackInterface::kVideoKind ? static_cast<webrtc::VideoTrackInterface*>(t.track().get()) : nullptr; }
}
bool media_stream_mutate_track(const NativeMediaStream& stream, const NativeMediaTrack& track, bool remove) noexcept {
  if (auto* audio = Audio(track)) {
    webrtc::scoped_refptr<webrtc::AudioTrackInterface> value(audio);
    return remove ? stream.stream()->RemoveTrack(value) : stream.stream()->AddTrack(value);
  }
  if (auto* video = Video(track)) {
    webrtc::scoped_refptr<webrtc::VideoTrackInterface> value(video);
    return remove ? stream.stream()->RemoveTrack(value) : stream.stream()->AddTrack(value);
  }
  return false;
}
struct NativeTrackSource::State {
  webrtc::scoped_refptr<webrtc::AudioSourceInterface> audio;
  webrtc::scoped_refptr<webrtc::VideoTrackSourceInterface> video;
  webrtc::Thread* signaling;
  webrtc::MediaSourceInterface* source() const { return audio ? static_cast<webrtc::MediaSourceInterface*>(audio.get()) : static_cast<webrtc::MediaSourceInterface*>(video.get()); }
};
NativeTrackSource::NativeTrackSource(std::unique_ptr<State> s) noexcept : state_(std::move(s)) {}
NativeTrackSource::~NativeTrackSource() {
  // Remote sources need not be proxies. Release on their actual main role,
  // which the Rust track/factory lease retains until this destructor returns.
  state_->signaling->BlockingCall([this] { state_->audio = nullptr; state_->video = nullptr; });
}
const std::unique_ptr<NativeTrackSource::State>& NativeTrackSource::state() const noexcept { return state_; }
std::unique_ptr<NativeTrackSource> media_track_source(const NativeMediaTrack& t, const NativePeerConnectionFactory& f, bool video) noexcept {
  auto s = std::make_unique<NativeTrackSource::State>(); s->signaling = f.signaling_thread();
  s->signaling->BlockingCall([&] { if (video) { if (auto* v = Video(t)) s->video = v->GetSource(); } else { if (auto* a = Audio(t)) s->audio = a->GetSource(); } });
  if (!s->source()) return nullptr;
  return std::make_unique<NativeTrackSource>(std::move(s));
}
std::int32_t track_source_state(const NativeTrackSource& s) noexcept { std::int32_t out = 0; s.state()->signaling->BlockingCall([&]{ out = static_cast<std::int32_t>(s.state()->source()->state()); }); return out; }
bool track_source_remote(const NativeTrackSource& s) noexcept { bool out = false; s.state()->signaling->BlockingCall([&]{ out = s.state()->source()->remote(); }); return out; }
bool track_source_same_resource(const NativeTrackSource& a, const NativeTrackSource& b) noexcept { return a.state()->source() == b.state()->source(); }
FfiAudioSourceOptions track_source_audio_options(const NativeTrackSource& s) noexcept {
 return s.state()->signaling->BlockingCall([&] { return media_audio_options_write(s.state()->audio->options()); });
}
FfiVideoSourceInfo track_source_video_info(const NativeTrackSource& s) noexcept {
  FfiVideoSourceInfo out;
  s.state()->signaling->BlockingCall([&] {
    auto& v = *s.state()->video;
    out.is_screencast = v.is_screencast(); out.needs_denoising = Choice(v.needs_denoising()); out.supports_encoded_output = v.SupportsEncodedOutput();
    webrtc::VideoTrackSourceInterface::Stats stats{};
    out.has_stats = v.GetStats(&stats); if (out.has_stats) { out.input_width = stats.input_width; out.input_height = stats.input_height; }
  }); return out;
}
void media_track_set_volume(const NativeMediaTrack& t, double volume) noexcept { Audio(t)->SetVolume(volume); }
void media_track_set_should_receive(const NativeMediaTrack& t, bool receive) noexcept { Video(t)->set_should_receive(receive); }
void media_track_set_content_hint(const NativeMediaTrack& t, std::int32_t hint) noexcept { Video(t)->set_content_hint(static_cast<webrtc::VideoTrackInterface::ContentHint>(hint)); }
FfiTrackAudioOptionsResult media_track_set_audio_options(const NativeMediaTrack& t, const FfiAudioSourceOptions& x) noexcept {
  if (media_audio_options_check(x)) { FfiTrackAudioOptionsResult out; out.code = -1; out.message = Bytes("binding admission: invalid audio processing mode"); return out; }
  const auto o = media_audio_options_read(x);
  const auto result = Audio(t)->SetAudioProcessingOptions(o);
  FfiTrackAudioOptionsResult out; out.code = static_cast<std::int32_t>(result.code); out.message = Bytes(result.message); return out;
}
FfiOptionalF64 media_track_audio_volume(const NativeMediaTrack& t) noexcept { if (auto* a = Audio(t)) return {true, a->GetVolume()}; return {false, 0}; }
FfiOptionalI32 media_track_signal_level(const NativeMediaTrack& t) noexcept { int level = 0; auto* a = Audio(t); bool ok = a && a->GetSignalLevel(&level); return {ok, level}; }
std::int32_t media_track_should_receive(const NativeMediaTrack& t) noexcept { auto* v = Video(t); return v ? static_cast<std::int32_t>(v->should_receive()) : -1; }
std::int32_t media_track_content_hint(const NativeMediaTrack& t) noexcept { auto* v = Video(t); return v ? static_cast<std::int32_t>(v->content_hint()) : -1; }
struct NativeAudioProcessor::State {
  webrtc::scoped_refptr<webrtc::AudioProcessorInterface> processor;
  webrtc::Thread* signaling;
};
NativeAudioProcessor::NativeAudioProcessor(std::unique_ptr<State> s) noexcept : state_(std::move(s)) {}
NativeAudioProcessor::~NativeAudioProcessor() { state_->signaling->BlockingCall([this]{ state_->processor = nullptr; }); }
const std::unique_ptr<NativeAudioProcessor::State>& NativeAudioProcessor::state() const noexcept { return state_; }
std::unique_ptr<NativeAudioProcessor> media_track_audio_processor(const NativeMediaTrack& t, const NativePeerConnectionFactory& f) noexcept {
  auto s = std::make_unique<NativeAudioProcessor::State>(); s->signaling = f.signaling_thread();
  s->signaling->BlockingCall([&]{ if (auto* a = Audio(t)) s->processor = a->GetAudioProcessor(); });
  if (!s->processor) return nullptr; return std::make_unique<NativeAudioProcessor>(std::move(s));
}
bool audio_processor_same_resource(const NativeAudioProcessor& a, const NativeAudioProcessor& b) noexcept { return a.state()->processor == b.state()->processor; }
FfiAudioProcessorStats audio_processor_stats(const NativeAudioProcessor& p, bool has_remote_tracks) noexcept {
  FfiAudioProcessorStats out;
  p.state()->signaling->BlockingCall([&] {
    const auto s = p.state()->processor->GetStats(has_remote_tracks);
    out.typing_noise_detected = s.typing_noise_detected;
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    out.voice_detected = Choice(s.apm_statistics.voice_detected);
#pragma clang diagnostic pop
    out.echo_return_loss = Optional(s.apm_statistics.echo_return_loss);
    out.echo_return_loss_enhancement = Optional(s.apm_statistics.echo_return_loss_enhancement);
    out.divergent_filter_fraction = Optional(s.apm_statistics.divergent_filter_fraction);
    out.residual_echo_likelihood = Optional(s.apm_statistics.residual_echo_likelihood);
    out.residual_echo_likelihood_recent_max = Optional(s.apm_statistics.residual_echo_likelihood_recent_max);
    out.delay_median_ms = Optional(s.apm_statistics.delay_median_ms);
    out.delay_standard_deviation_ms = Optional(s.apm_statistics.delay_standard_deviation_ms);
    out.delay_ms = Optional(s.apm_statistics.delay_ms);
  }); return out;
}

NativeMediaStream::NativeMediaStream(webrtc::scoped_refptr<webrtc::MediaStreamInterface> s) noexcept : stream_(std::move(s)) {}
NativeMediaStream::~NativeMediaStream() = default;
const webrtc::scoped_refptr<webrtc::MediaStreamInterface>& NativeMediaStream::stream() const noexcept { return stream_; }
NativeMediaStreams::NativeMediaStreams(std::vector<webrtc::scoped_refptr<webrtc::MediaStreamInterface>> s) noexcept : streams_(std::move(s)) {}
NativeMediaStreams::~NativeMediaStreams() = default;
const std::vector<webrtc::scoped_refptr<webrtc::MediaStreamInterface>>& NativeMediaStreams::streams() const noexcept { return streams_; }
std::size_t media_streams_len(const NativeMediaStreams& s) noexcept { return s.streams().size(); }
std::unique_ptr<NativeMediaStream> media_streams_get(const NativeMediaStreams& s, std::size_t i) noexcept { if (i >= s.streams().size()) return nullptr; return std::make_unique<NativeMediaStream>(s.streams()[i]); }
rust::Vec<std::uint8_t> media_stream_id(const NativeMediaStream& s) noexcept { return Bytes(s.stream()->id()); }
bool media_stream_same_resource(const NativeMediaStream& a, const NativeMediaStream& b) noexcept { return a.stream() == b.stream(); }
NativeMediaTracks::NativeMediaTracks(std::vector<webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface>> t) noexcept : tracks_(std::move(t)) {}
NativeMediaTracks::~NativeMediaTracks() = default;
const std::vector<webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface>>& NativeMediaTracks::tracks() const noexcept { return tracks_; }
std::unique_ptr<NativeMediaTracks> media_stream_tracks(const NativeMediaStream& s, bool video) noexcept {
  std::vector<webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface>> tracks;
  if (video) { for (auto& t : s.stream()->GetVideoTracks()) tracks.push_back(t); }
  else { for (auto& t : s.stream()->GetAudioTracks()) tracks.push_back(t); }
  return std::make_unique<NativeMediaTracks>(std::move(tracks));
}
std::size_t media_tracks_len(const NativeMediaTracks& t) noexcept { return t.tracks().size(); }
std::unique_ptr<NativeMediaTrack> media_tracks_get(const NativeMediaTracks& t, std::size_t i) noexcept { if (i >= t.tracks().size()) return nullptr; return std::make_unique<NativeMediaTrack>(t.tracks()[i]); }
std::unique_ptr<NativeMediaTrack> media_stream_find(const NativeMediaStream& s, bool video, rust::Slice<const std::uint8_t> id) noexcept {
  const std::string bytes(reinterpret_cast<const char*>(id.data()), id.size());
  webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> track;
  if (video) track = s.stream()->FindVideoTrack(bytes); else track = s.stream()->FindAudioTrack(bytes);
  if (!track) return nullptr; return std::make_unique<NativeMediaTrack>(std::move(track));
}

struct NativeMediaObserver::State final : webrtc::ObserverInterface {
  webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> track;
  webrtc::scoped_refptr<webrtc::MediaStreamInterface> stream;
  webrtc::Thread* signaling;
  std::shared_ptr<ReadinessSignal> readiness;
  std::mutex mutex;
  std::size_t capacity = 0, queued = 0;
  std::uint64_t dropped = 0;
  webrtc::NotifierInterface* notifier() const { return track ? static_cast<webrtc::NotifierInterface*>(track.get()) : static_cast<webrtc::NotifierInterface*>(stream.get()); }
  void OnChanged() override {
    ReadinessNotification notify{readiness};
    std::lock_guard lock(mutex);
    if (queued < capacity) ++queued;
    else if (dropped != std::numeric_limits<std::uint64_t>::max()) ++dropped;
  }
};
NativeMediaObserver::NativeMediaObserver(std::unique_ptr<State> s) noexcept : state_(std::move(s)) {}
NativeMediaObserver::~NativeMediaObserver() { state_->signaling->BlockingCall([this] { state_->notifier()->UnregisterObserver(state_.get()); }); }
const std::unique_ptr<NativeMediaObserver::State>& NativeMediaObserver::state() const noexcept { return state_; }
std::unique_ptr<NativeMediaObserver> media_track_observe(const NativeMediaTrack& t, const NativePeerConnectionFactory& f, std::size_t capacity) noexcept {
  if (!capacity) return nullptr;
  auto s = std::make_unique<NativeMediaObserver::State>(); s->track = t.track(); s->signaling = f.signaling_thread(); s->readiness = f.readiness(); s->capacity = capacity;
  s->signaling->BlockingCall([&]{ s->notifier()->RegisterObserver(s.get()); });
  return std::make_unique<NativeMediaObserver>(std::move(s));
}
std::unique_ptr<NativeMediaObserver> media_stream_observe(const NativeMediaStream& stream, const NativePeerConnectionFactory& f, std::size_t capacity) noexcept {
  if (!capacity) return nullptr;
  auto s = std::make_unique<NativeMediaObserver::State>(); s->stream = stream.stream(); s->signaling = f.signaling_thread(); s->readiness = f.readiness(); s->capacity = capacity;
  s->signaling->BlockingCall([&]{ s->notifier()->RegisterObserver(s.get()); });
  return std::make_unique<NativeMediaObserver>(std::move(s));
}
bool media_observer_take(const NativeMediaObserver& o) noexcept { std::lock_guard lock(o.state()->mutex); if (!o.state()->queued) return false; --o.state()->queued; return true; }
std::uint64_t media_observer_take_dropped(const NativeMediaObserver& o) noexcept { std::lock_guard lock(o.state()->mutex); return std::exchange(o.state()->dropped, 0); }
}  // namespace pulsebeam::webrtc_sys
