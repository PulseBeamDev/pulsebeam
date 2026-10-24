#include "pulsebeam-webrtc-sys/native/audio.h"
#include "pulsebeam-webrtc-sys/native/live_frame_transformer.h"
#include "pulsebeam-webrtc-sys/native/audio_sink_queue.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <cstring>
#include <cmath>
#include <deque>
#include <limits>
#include <list>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <utility>
#include <vector>

#include "api/frame_transformer_interface.h"
#include "api/audio/audio_processing.h"
#include "pulsebeam-webrtc-sys/native/frame_callback_slot.h"
#include "pulsebeam-webrtc-sys/native/media_values.h"
#include "api/make_ref_counted.h"
#include "api/media_stream_interface.h"
#include "api/notifier.h"
#include "api/peer_connection_interface.h"
#include "api/scoped_refptr.h"
#include "api/rtp_transceiver_interface.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/peer.h"
#include "pulsebeam-webrtc-sys/native/rtp_values.h"
#include "pulsebeam-webrtc-sys/native/rtc_error.h"
#include "pulsebeam-webrtc-sys/native/opus_carrier.h"
#include "pulsebeam-webrtc-sys/native/video.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "rtc_base/thread.h"

namespace pulsebeam::webrtc_sys {
namespace {

// Both the caller and worker processing scope refuse callback recursion before
// source locks. Scope identity is the stable owned State, never codec metadata.
class PcmCaptureScope {
 public:
  explicit PcmCaptureScope(const void* source) {
    admitted_ = std::find(active_.begin(), active_.end(), source) == active_.end();
    if (admitted_) active_.push_back(source);
  }
  ~PcmCaptureScope() { if (admitted_) active_.pop_back(); }
  bool admitted() const { return admitted_; }
  static bool Active(const void* source) { return std::find(active_.begin(), active_.end(), source) != active_.end(); }
 private:
  static thread_local std::vector<const void*> active_;
  bool admitted_ = false;
};
thread_local std::vector<const void*> PcmCaptureScope::active_;
std::optional<bool> ReadOptionBool(std::int8_t value) {
  return value < 0 ? std::nullopt : std::optional<bool>(value != 0);
}
std::optional<int> ReadOptionInt(const FfiOptionalI32& value) {
  return value.present ? std::optional<int>(value.value) : std::nullopt;
}
std::optional<webrtc::AudioProcessingMode> ReadOptionMode(const FfiOptionalI32& value) {
  return value.present ? std::optional(static_cast<webrtc::AudioProcessingMode>(value.value))
                       : std::nullopt;
}
template <typename T>
FfiOptionalI32 WriteOptionInt(const std::optional<T>& value) {
  FfiOptionalI32 out;
  out.present = value.has_value();
  out.value = value ? static_cast<std::int32_t>(*value) : 0;
  return out;
}
std::uint8_t CheckAudioOptions(const FfiAudioSourceOptions& options) {
  for (const auto& mode : {options.echo_cancellation_mode, options.auto_gain_control_mode,
                          options.noise_suppression_mode, options.highpass_filter_mode}) {
    if (mode.present && (mode.value < 0 || mode.value > 2)) return 1;
  }
  return 0;
}
webrtc::AudioOptions ReadAudioOptions(const FfiAudioSourceOptions& value) {
  webrtc::AudioOptions out;
  out.echo_cancellation = ReadOptionBool(value.echo_cancellation);
  out.echo_cancellation_mode = ReadOptionMode(value.echo_cancellation_mode);
  out.auto_gain_control = ReadOptionBool(value.auto_gain_control);
  out.auto_gain_control_mode = ReadOptionMode(value.auto_gain_control_mode);
  out.noise_suppression = ReadOptionBool(value.noise_suppression);
  out.noise_suppression_mode = ReadOptionMode(value.noise_suppression_mode);
  out.highpass_filter = ReadOptionBool(value.highpass_filter);
  out.highpass_filter_mode = ReadOptionMode(value.highpass_filter_mode);
  out.stereo_swapping = ReadOptionBool(value.stereo_swapping);
  out.audio_jitter_buffer_max_packets = ReadOptionInt(value.audio_jitter_buffer_max_packets);
  out.audio_jitter_buffer_fast_accelerate = ReadOptionBool(value.audio_jitter_buffer_fast_accelerate);
  out.audio_jitter_buffer_min_delay_ms = ReadOptionInt(value.audio_jitter_buffer_min_delay_ms);
  out.audio_network_adaptor = ReadOptionBool(value.audio_network_adaptor);
  if (value.has_adaptor_config) {
    out.audio_network_adaptor_config = std::string{};
    if (!value.adaptor_config.empty()) {
      out.audio_network_adaptor_config->assign(
          reinterpret_cast<const char*>(value.adaptor_config.data()), value.adaptor_config.size());
    }
  }
  out.init_recording_on_send = ReadOptionBool(value.init_recording_on_send);
  return out;
}
FfiAudioSourceOptions WriteAudioOptions(const webrtc::AudioOptions& value) {
  const auto boolean = [](std::optional<bool> v) -> std::int8_t {
    return v ? static_cast<std::int8_t>(*v) : -1;
  };
  FfiAudioSourceOptions out;
  out.echo_cancellation = boolean(value.echo_cancellation);
  out.echo_cancellation_mode = WriteOptionInt(value.echo_cancellation_mode);
  out.auto_gain_control = boolean(value.auto_gain_control);
  out.auto_gain_control_mode = WriteOptionInt(value.auto_gain_control_mode);
  out.noise_suppression = boolean(value.noise_suppression);
  out.noise_suppression_mode = WriteOptionInt(value.noise_suppression_mode);
  out.highpass_filter = boolean(value.highpass_filter);
  out.highpass_filter_mode = WriteOptionInt(value.highpass_filter_mode);
  out.stereo_swapping = boolean(value.stereo_swapping);
  out.audio_jitter_buffer_max_packets = WriteOptionInt(value.audio_jitter_buffer_max_packets);
  out.audio_jitter_buffer_fast_accelerate = boolean(value.audio_jitter_buffer_fast_accelerate);
  out.audio_jitter_buffer_min_delay_ms = WriteOptionInt(value.audio_jitter_buffer_min_delay_ms);
  out.audio_network_adaptor = boolean(value.audio_network_adaptor);
  out.has_adaptor_config = value.audio_network_adaptor_config.has_value();
  if (value.audio_network_adaptor_config) {
    for (unsigned char byte : *value.audio_network_adaptor_config) out.adaptor_config.push_back(byte);
  }
  out.init_recording_on_send = boolean(value.init_recording_on_send);
  return out;
}

FfiAudioTrackStatus ReadAudioTrackStatus(webrtc::AudioTrackInterface& track) {
  FfiAudioTrackStatus out;
  out.present = true;
  out.id = rust::String(track.id());
  out.kind = rust::String(track.kind());
  out.enabled = track.enabled();
  out.state = static_cast<std::uint8_t>(track.state());
  out.volume = track.GetVolume();
  int level = 0;
  out.signal_level.present = track.GetSignalLevel(&level);
  out.signal_level.value = level;
  return out;
}

// Keep the sink list synchronized with synchronous source delivery. RemoveSink
// cannot return until any in-flight OnData has finished using that sink.
class PushAudioSource : public webrtc::Notifier<webrtc::AudioSourceInterface> {
 public:
  SourceState state() const override { return closed_.load() ? kEnded : kLive; }
  bool remote() const override { return false; }
  const webrtc::AudioOptions options() const override {
    std::lock_guard lock(mutex_);
    return options_;
  }
  void SetOptions(const webrtc::AudioOptions& options) override {
    {
      std::lock_guard lock(mutex_);
      options_ = options;
    }
    FireOnChanged();
  }

  void AddSink(webrtc::AudioTrackSinkInterface* sink) override {
    std::lock_guard lock(mutex_);
    if (closed_ || !sink ||
        next_recipient_ == std::numeric_limits<std::uint32_t>::max() ||
        std::any_of(sinks_.begin(), sinks_.end(),
                    [sink](const auto& entry) { return entry.sink == sink; }))
      return;
    sinks_.push_back({sink, ++next_recipient_});
  }
  void RemoveSink(webrtc::AudioTrackSinkInterface* sink) override {
    std::lock_guard lock(mutex_);
    // Queued native input can outlive detachment. Keep its reservation until
    // consumption, or source close; removal alone is not a consumption receipt.
    sinks_.remove_if([sink](const auto& entry) { return entry.sink == sink; });
  }
  bool HasSinks() {
    std::lock_guard lock(mutex_);
    return !closed_ && !sinks_.empty();
  }
  bool Push(const std::int16_t* samples, int sample_rate, size_t channels,
            size_t frames,
            std::optional<std::int64_t> capture_time_ms = std::nullopt) {
    std::lock_guard lock(mutex_);
    if (closed_) return false;
    for (const auto& entry : sinks_) {
      // Only explicitly mapped controlled timing is forwarded. Ordinary raw
      // caller timestamps are not automatically a TimeMillis capture clock.
      entry.sink->OnData(samples, 16, sample_rate, channels, frames, capture_time_ms);
    }
    return true;
  }
  bool PushOpus(std::span<const std::uint8_t> payload,
                std::uint32_t rtp_timestamp, std::uint32_t samples_per_channel,
                std::int64_t capture_time_us,
                const std::shared_ptr<opus_carrier::Budget>& budget,
                std::uint32_t source_id, bool has_level,
                std::uint8_t level_dbov, bool voice_activity) {
    using namespace opus_carrier;
    // Snapshot and deliver the complete packet under the same sink lock.
    // No recipient can join/leave between its individual carrier blocks.
    std::lock_guard lock(mutex_);
    if (closed_ || sinks_.empty() || !source_id) return false;
    const auto slots = samples_per_channel / 480;
    const auto capacity = kBytesPerChannelBlock * kChannels - kHeaderBytes;
    if (payload.size() > slots * capacity) return false;
    std::vector<std::uint32_t> recipients;
    recipients.reserve(sinks_.size());
    for (const auto& entry : sinks_) recipients.push_back(entry.recipient);
    const auto packet = Reserve(*budget, slots, recipients);
    if (!packet) return false;
    const auto checksum = Checksum(payload);
    for (const auto& entry : sinks_) {
      for (std::uint32_t slot = 0; slot < slots; ++slot) {
        std::array<std::int16_t, 480 * kChannels> samples{};
        auto* bytes = reinterpret_cast<std::uint8_t*>(samples.data());
        std::memcpy(bytes, kMagic.data(), kMagic.size());
        Store16(bytes + 8, static_cast<std::uint16_t>(payload.size()));
        Store16(bytes + 10, static_cast<std::uint16_t>(samples_per_channel));
        Store32(bytes + 12, rtp_timestamp);
        Store32(bytes + 16, checksum);
        Store32(bytes + 20, source_id);
        Store32(bytes + 24, *packet);
        Store32(bytes + 28, entry.recipient);
        bytes[32] = static_cast<std::uint8_t>(slot);
        bytes[33] = has_level ? level_dbov : 0;
        bytes[34] = has_level && voice_activity;
        bytes[35] = has_level;
        const auto offset = slot * capacity;
        if (offset < payload.size())
          std::memcpy(bytes + kHeaderBytes, payload.data() + offset,
                      std::min(capacity, payload.size() - offset));
        const auto capture_ms = capture_time_us < 0 ? std::nullopt :
            std::optional<std::int64_t>(capture_time_us / 1000 + slot * 10);
        entry.sink->OnData(samples.data(), 16, 48000, kChannels, 480, capture_ms);
      }
    }
    return true;
  }
  void Close() {
    {
      std::lock_guard lock(mutex_);
      if (closed_.exchange(true)) return;
    }
    FireOnChanged();
  }

 private:
  mutable std::mutex mutex_;
  struct Sink {
    webrtc::AudioTrackSinkInterface* sink;
    std::uint32_t recipient;
  };
  std::list<Sink> sinks_;
  std::uint32_t next_recipient_ = 0;
  webrtc::AudioOptions options_;
  std::atomic<bool> closed_{false};
};

// Receive callbacks arrive on a WebRTC audio thread. The queue retains at
// most eight 10 ms blocks; consumers observe loss through DroppedFrames().
// Opus TOC (RFC 6716 section 3) determines packet duration at 48 kHz.
std::uint32_t OpusSamples(std::span<const std::uint8_t> data) {
  if (data.empty()) return 0;
  const unsigned config = data[0] >> 3;
  const unsigned code = data[0] & 3;
  const unsigned count = code == 0 ? 1 : code == 3
      ? (data.size() > 1 ? data[1] & 63 : 0) : 2;
  if (count == 0 || count > 48) return 0;
  const unsigned per_frame = config < 12
      ? ((config & 3) == 3 ? 2880u : (480u << (config & 3)))
      : config < 16 ? (480u << (config & 1))
      : (120u << (config & 3));
  const unsigned samples = count * per_frame;
  return samples <= 5760 ? samples : 0;
}

// Swallow frames before NetEq so observing Opus never invokes its decoder.
// After close, forward through the native delegate to resume decoding. Audio
// delegate replacement unregisters the old callback in this pinned revision.
class EncodedAudioCollector : public webrtc::FrameTransformerInterface {
 public:
  explicit EncodedAudioCollector(std::shared_ptr<ReadinessSignal> readiness)
      : readiness_(std::move(readiness)) {}
  void Transform(std::unique_ptr<webrtc::TransformableFrameInterface> frame) override {
    ReadinessNotification notify{};
    std::unique_lock lock(mutex_);
    if (!active_) {
      auto callback = callback_;
      lock.unlock();
      if (callback) callback->OnTransformedFrame(std::move(frame));
      return;
    }
    notify.readiness = readiness_;
    const auto data = frame->GetData();
    if (frame->GetDirection() !=
            webrtc::TransformableFrameInterface::Direction::kReceiver ||
        frame->GetMimeType() != "audio/opus" || data.empty() ||
        data.size() > 65536) {
      Lost();
      return;
    }
    const auto samples = OpusSamples(data);
    if (!samples) {
      Lost();
      return;
    }
    while (!frames_.empty() &&
           (frames_.size() >= 64 || bytes_ > 262144 - data.size())) {
      bytes_ -= frames_.front().data.size();
      frames_.pop_front();
      Lost();
    }
    Frame saved;
    saved.data.assign(data.begin(), data.end());
    saved.rtp_timestamp = frame->GetTimestamp();
    saved.ssrc = frame->GetSsrc();
    saved.payload_type = frame->GetPayloadType();
    saved.samples = static_cast<std::uint32_t>(samples);
    // This transformer is installed on an audio receiver's declared native
    // callback, whose frames implement the public audio-specific interface.
    const auto* audio_frame =
        static_cast<const webrtc::TransformableAudioFrameInterface*>(frame.get());
    saved.sequence_number = audio_frame->SequenceNumber();
    saved.audio_level_dbov = audio_frame->AudioLevel();
    // Pinned TransformableIncomingAudioFrame::Type projects the received
    // RFC6464 V bit, not decoder/PCM speech classification. It returns CN
    // also when the extension is absent, so presence is required here.
    // This collector only accepts the native receiver's unmodified frames.
    if (saved.audio_level_dbov) {
      using Type = webrtc::TransformableAudioFrameInterface::FrameType;
      if (audio_frame->Type() == Type::kAudioFrameSpeech)
        saved.voice_activity = true;
      else if (audio_frame->Type() == Type::kAudioFrameCN)
        saved.voice_activity = false;
    }
    if (auto time = frame->CaptureTime()) saved.capture_us = MediaTimestampBits(*time);
    if (auto time = frame->ReceiveTime()) saved.receive_us = MediaTimestampBits(*time);
    bytes_ += saved.data.size();
    frames_.push_back(std::move(saved));
  }
  void RegisterTransformedFrameCallback(
      webrtc::scoped_refptr<webrtc::TransformedFrameCallback> callback) override {
    auto replacement = std::make_shared<StockFrameCallbackSlot>(std::move(callback));
    std::shared_ptr<StockFrameCallbackSlot> retired;
    {
      std::lock_guard lock(mutex_);
      retired = std::exchange(callback_, std::move(replacement));
    }
    if (retired) retired->Retire();
  }
  void UnregisterTransformedFrameCallback() override {
    std::shared_ptr<StockFrameCallbackSlot> retired;
    {
      std::lock_guard lock(mutex_);
      retired = std::move(callback_);
    }
    if (retired) retired->Retire();
  }
  FfiEncodedAudioFrame Take() {
    std::lock_guard lock(mutex_);
    FfiEncodedAudioFrame out{};
    if (frames_.empty()) return out;
    auto frame = std::move(frames_.front());
    frames_.pop_front();
    bytes_ -= frame.data.size();
    out.available = true;
    for (auto byte : frame.data) out.data.push_back(byte);
    out.rtp_timestamp = frame.rtp_timestamp;
    out.ssrc = frame.ssrc;
    out.payload_type = frame.payload_type;
    out.samples_per_channel = frame.samples;
    out.has_sequence_number = frame.sequence_number.has_value();
    out.sequence_number = frame.sequence_number.value_or(0);
    out.has_audio_level = frame.audio_level_dbov.has_value();
    out.audio_level_dbov = frame.audio_level_dbov.value_or(0);
    out.has_voice_activity = frame.voice_activity.has_value();
    out.voice_activity = frame.voice_activity.value_or(false);
    out.has_capture_time = frame.capture_us.has_value();
    out.capture_time_us = frame.capture_us.value_or(0);
    out.has_receive_time = frame.receive_us.has_value();
    out.receive_time_us = frame.receive_us.value_or(0);
    return out;
  }
  std::uint64_t DroppedFrames() const {
    std::lock_guard lock(mutex_);
    return dropped_;
  }
  void Deactivate() {
    std::lock_guard lock(mutex_);
    active_ = false;
    frames_.clear();
    bytes_ = 0;
  }

 private:
  void Lost() {
    if (dropped_ != std::numeric_limits<std::uint64_t>::max()) ++dropped_;
  }
  struct Frame {
    std::vector<std::uint8_t> data;
    std::uint32_t rtp_timestamp = 0;
    std::uint32_t ssrc = 0;
    std::uint8_t payload_type = 0;
    std::uint32_t samples = 0;
    std::optional<std::uint16_t> sequence_number;
    std::optional<std::uint8_t> audio_level_dbov;
    std::optional<bool> voice_activity;
    std::optional<std::int64_t> capture_us;
    std::optional<std::int64_t> receive_us;
  };
  const std::shared_ptr<ReadinessSignal> readiness_;
  mutable std::mutex mutex_;
  std::shared_ptr<StockFrameCallbackSlot> callback_;
  std::deque<Frame> frames_;
  std::size_t bytes_ = 0;
  std::uint64_t dropped_ = 0;
  bool active_ = true;
};

}  // namespace
webrtc::AudioOptions media_audio_options_read(const FfiAudioSourceOptions& v) noexcept { return ReadAudioOptions(v); }
FfiAudioSourceOptions media_audio_options_write(const webrtc::AudioOptions& v) noexcept { return WriteAudioOptions(v); }
std::uint8_t media_audio_options_check(const FfiAudioSourceOptions& v) noexcept { return CheckAudioOptions(v); }

struct NativeEncodedAudioSink::State {
  const NativePeerConnection* peer = nullptr;
  std::string receiver_id;
  webrtc::scoped_refptr<EncodedAudioCollector> collector;
  std::atomic<bool> closed{false};
};

struct NativeAudioSink::State {
  const NativePeerConnection* peer = nullptr;
  std::string receiver_id;
  webrtc::scoped_refptr<webrtc::AudioTrackInterface> track;
  std::unique_ptr<ReceivedAudioCollector> collector;
  webrtc::Thread* signaling_thread = nullptr;
  std::atomic<bool> closed{false};
};

struct NativeAudioSource::State {
  webrtc::scoped_refptr<PushAudioSource> source;
  webrtc::scoped_refptr<webrtc::AudioProcessing> processing;
  FfiAudioProcessingConfig processing_config{};
  std::shared_ptr<NativeInstalledAudioProcessor::State> installed_processing;
  webrtc::Thread* signaling_thread = nullptr;
  webrtc::Thread* worker_thread = nullptr;
  std::mutex frame_mutex;
  std::shared_ptr<opus_carrier::Budget> budget;
  std::uint32_t budget_id = 0;
  std::uint8_t encoded_channels = 0;
};

struct NativeAudioTrack::State {
  bool opus_frames = false;
  webrtc::scoped_refptr<webrtc::AudioTrackInterface> track;
  webrtc::scoped_refptr<webrtc::AudioSourceInterface> microphone_source;
  webrtc::Thread* signaling_thread = nullptr;
};

webrtc::AudioTrackInterface* audio_track_sdk_handle(const NativeAudioTrack& track) noexcept {
  return track.state()->track.get();
}
std::size_t audio_track_identity(const NativeAudioTrack& track) noexcept {
  return reinterpret_cast<std::size_t>(audio_track_sdk_handle(track));
}

struct NativeAudioTrackProcessor::State {
  webrtc::scoped_refptr<webrtc::AudioProcessorInterface> processor;
  webrtc::Thread* signaling_thread = nullptr;
};

struct NativeAudioSourceView::State {
  webrtc::scoped_refptr<webrtc::AudioSourceInterface> source;
  webrtc::Thread* signaling_thread = nullptr;
};
struct NativeAudioObserver::State final : webrtc::ObserverInterface,
                                        webrtc::AudioSourceInterface::AudioObserver {
  webrtc::scoped_refptr<webrtc::AudioSourceInterface> source;
  webrtc::scoped_refptr<webrtc::AudioTrackInterface> track;
  webrtc::Thread* signaling_thread = nullptr;
  bool volume_only = false;
  bool registered = false;  // signaling only
  std::size_t capacity = 0;
  std::mutex mutex;
  std::deque<FfiAudioObserverEvent> events;
  std::uint64_t dropped = 0;
  void Push(bool volume, double value) {
    std::lock_guard lock(mutex);
    if (events.size() == capacity) {
      events.pop_front();
      if (dropped != std::numeric_limits<std::uint64_t>::max()) ++dropped;
    }
    FfiAudioObserverEvent event;
    event.present = true;
    event.volume = volume;
    event.value = value;
    events.push_back(std::move(event));
  }
  void OnChanged() override { Push(false, 0); }
  void OnSetVolume(double volume) override { Push(true, volume); }
};
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
static FfiAudioProcessorStatistics WriteProcessorStatistics(
    const webrtc::AudioProcessorInterface::AudioProcessorStatistics& value) {
  FfiAudioProcessorStatistics out;
  out.typing_noise_detected = value.typing_noise_detected;
  const auto& apm = value.apm_statistics;
  out.has_voice_detected = apm.voice_detected.has_value();
  out.voice_detected = apm.voice_detected.value_or(false);
  out.has_echo_return_loss = apm.echo_return_loss.has_value();
  out.echo_return_loss = apm.echo_return_loss.value_or(0);
  out.has_echo_return_loss_enhancement = apm.echo_return_loss_enhancement.has_value();
  out.echo_return_loss_enhancement = apm.echo_return_loss_enhancement.value_or(0);
  out.has_divergent_filter_fraction = apm.divergent_filter_fraction.has_value();
  out.divergent_filter_fraction = apm.divergent_filter_fraction.value_or(0);
  out.has_delay_median_ms = apm.delay_median_ms.has_value();
  out.delay_median_ms = apm.delay_median_ms.value_or(0);
  out.has_delay_standard_deviation_ms = apm.delay_standard_deviation_ms.has_value();
  out.delay_standard_deviation_ms = apm.delay_standard_deviation_ms.value_or(0);
  out.has_residual_echo_likelihood = apm.residual_echo_likelihood.has_value();
  out.residual_echo_likelihood = apm.residual_echo_likelihood.value_or(0);
  out.has_residual_echo_likelihood_recent_max = apm.residual_echo_likelihood_recent_max.has_value();
  out.residual_echo_likelihood_recent_max = apm.residual_echo_likelihood_recent_max.value_or(0);
  out.has_delay_ms = apm.delay_ms.has_value();
  out.delay_ms = apm.delay_ms.value_or(0);
  return out;
}
#pragma clang diagnostic pop
FfiAudioProcessorStatistics audio_track_processor_statistics_default() noexcept {
  return WriteProcessorStatistics({});
}
NativeAudioTrackProcessor::NativeAudioTrackProcessor(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeAudioTrackProcessor::~NativeAudioTrackProcessor() {
  state_->signaling_thread->BlockingCall([&] { state_->processor = nullptr; });
}
const std::unique_ptr<NativeAudioTrackProcessor::State>& NativeAudioTrackProcessor::state() const noexcept { return state_; }
std::unique_ptr<NativeAudioTrackProcessor> audio_track_processor(const NativeAudioTrack& track) noexcept {
  auto state = std::make_unique<NativeAudioTrackProcessor::State>();
  state->signaling_thread = track.state()->signaling_thread;
  state->signaling_thread->BlockingCall([&] { state->processor = track.state()->track->GetAudioProcessor(); });
  if (!state->processor) return nullptr;
  return std::make_unique<NativeAudioTrackProcessor>(std::move(state));
}
bool audio_track_processors_same(const NativeAudioTrackProcessor& a, const NativeAudioTrackProcessor& b) noexcept {
  return a.state()->processor.get() == b.state()->processor.get();
}
FfiAudioProcessorStatistics audio_track_processor_stats(const NativeAudioTrackProcessor& processor, bool has_remote_tracks) noexcept {
  return processor.state()->signaling_thread->BlockingCall([&] {
    return WriteProcessorStatistics(processor.state()->processor->GetStats(has_remote_tracks));
  });
}

NativeAudioSourceView::NativeAudioSourceView(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeAudioSourceView::~NativeAudioSourceView() {
  state_->signaling_thread->BlockingCall([&] { state_->source = nullptr; });
}
const std::unique_ptr<NativeAudioSourceView::State>& NativeAudioSourceView::state() const noexcept { return state_; }
NativeAudioObserver::NativeAudioObserver(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeAudioObserver::~NativeAudioObserver() {
  audio_observer_close(*this);
  state_->signaling_thread->BlockingCall([&] { state_->source = nullptr; state_->track = nullptr; });
}
const std::unique_ptr<NativeAudioObserver::State>& NativeAudioObserver::state() const noexcept { return state_; }
std::unique_ptr<NativeAudioSourceView> audio_source_view(const NativeAudioSource& source) noexcept {
  auto state = std::make_unique<NativeAudioSourceView::State>();
  state->signaling_thread = source.state()->signaling_thread;
  state->signaling_thread->BlockingCall([&] { state->source = source.state()->source; });
  return std::make_unique<NativeAudioSourceView>(std::move(state));
}
std::unique_ptr<NativeAudioSourceView> audio_track_source_view(const NativeAudioTrack& track) noexcept {
  auto state = std::make_unique<NativeAudioSourceView::State>();
  state->signaling_thread = track.state()->signaling_thread;
  state->signaling_thread->BlockingCall([&] { state->source = track.state()->track->GetSource(); });
  if (!state->source) return nullptr;
  return std::make_unique<NativeAudioSourceView>(std::move(state));
}
std::unique_ptr<NativeAudioTrack> rtp_receiver_audio_track(const NativePeerConnection& peer,
                                                       const NativeRtpReceiver& receiver) noexcept {
  if (!peer.peer() || !peer.signaling_thread()) return nullptr;
  return peer.signaling_thread()->BlockingCall([&]() -> std::unique_ptr<NativeAudioTrack> {
    auto remote = receiver.receiver();
    bool belongs = false;
    for (const auto& candidate : peer.peer()->GetReceivers()) if (candidate == remote) belongs = true;
    if (!belongs || !remote || !remote->track() || remote->track()->kind() != webrtc::MediaStreamTrackInterface::kAudioKind) return nullptr;
    auto state = std::make_unique<NativeAudioTrack::State>();
    state->signaling_thread = peer.signaling_thread();
    state->track = static_cast<webrtc::AudioTrackInterface*>(remote->track().get());
    return std::make_unique<NativeAudioTrack>(std::move(state));
  });
}
std::unique_ptr<NativeAudioTrack> rtp_sender_audio_track(const NativePeerConnection& peer,
                                                     const NativeRtpSender& sender) noexcept {
  if (!peer.peer() || !peer.signaling_thread()) return nullptr;
  return peer.signaling_thread()->BlockingCall([&]() -> std::unique_ptr<NativeAudioTrack> {
    auto track = sender.sender()->track();
    if (!track || track->kind() != webrtc::MediaStreamTrackInterface::kAudioKind) return nullptr;
    auto state = std::make_unique<NativeAudioTrack::State>();
    state->signaling_thread = peer.signaling_thread();
    state->track = static_cast<webrtc::AudioTrackInterface*>(track.get());
    return std::make_unique<NativeAudioTrack>(std::move(state));
  });
}
bool audio_tracks_same(const NativeAudioTrack& a, const NativeAudioTrack& b) noexcept { return a.state()->track == b.state()->track; }
bool audio_source_view_same(const NativeAudioSourceView& a, const NativeAudioSourceView& b) noexcept {
  return a.state()->source == b.state()->source;
}
std::uint8_t audio_source_view_state(const NativeAudioSourceView& view) noexcept {
  return view.state()->signaling_thread->BlockingCall([&] { return static_cast<std::uint8_t>(view.state()->source->state()); });
}
bool audio_source_view_remote(const NativeAudioSourceView& view) noexcept {
  return view.state()->signaling_thread->BlockingCall([&] { return view.state()->source->remote(); });
}
FfiAudioSourceOptions audio_source_view_options(const NativeAudioSourceView& view) noexcept {
  return view.state()->signaling_thread->BlockingCall([&] { return WriteAudioOptions(view.state()->source->options()); });
}
std::uint8_t audio_source_view_set_options(const NativeAudioSourceView& view, const FfiAudioSourceOptions& options) noexcept {
  if (auto error = CheckAudioOptions(options)) return error;
  const auto native = ReadAudioOptions(options);
  view.state()->signaling_thread->BlockingCall([&] { view.state()->source->SetOptions(native); });
  return 0;
}
double audio_source_view_volume(const NativeAudioSourceView& view) noexcept {
  return view.state()->signaling_thread->BlockingCall([&] { return view.state()->source->GetVolume(); });
}
std::uint8_t audio_source_view_set_volume(const NativeAudioSourceView& view, double volume) noexcept {
  if (!std::isfinite(volume) || volume < 0 || volume > 10) return 2;
  view.state()->signaling_thread->BlockingCall([&] { view.state()->source->SetVolume(volume); });
  return 0;
}
std::unique_ptr<NativeAudioObserver> audio_source_view_observe(const NativeAudioSourceView& view,
                                                           bool volume, std::size_t capacity) noexcept {
  if (!capacity) return nullptr;
  auto state = std::make_unique<NativeAudioObserver::State>();
  state->source = view.state()->source;
  state->signaling_thread = view.state()->signaling_thread;
  state->volume_only = volume;
  state->capacity = capacity;
  state->signaling_thread->BlockingCall([&] {
    if (volume) state->source->RegisterAudioObserver(state.get());
    else state->source->RegisterObserver(state.get());
    state->registered = true;
  });
  return std::make_unique<NativeAudioObserver>(std::move(state));
}
std::unique_ptr<NativeAudioObserver> audio_track_observe(const NativeAudioTrack& track, std::size_t capacity) noexcept {
  if (!capacity) return nullptr;
  auto state = std::make_unique<NativeAudioObserver::State>();
  state->track = track.state()->track;
  state->signaling_thread = track.state()->signaling_thread;
  state->capacity = capacity;
  state->signaling_thread->BlockingCall([&] { state->track->RegisterObserver(state.get()); state->registered = true; });
  return std::make_unique<NativeAudioObserver>(std::move(state));
}
void audio_observer_close(const NativeAudioObserver& observer) noexcept {
  auto& state = *observer.state();
  state.signaling_thread->BlockingCall([&] {
    if (!state.registered) return;
    if (state.volume_only) state.source->UnregisterAudioObserver(&state);
    else if (state.track) state.track->UnregisterObserver(&state);
    else state.source->UnregisterObserver(&state);
    state.registered = false;
  });
}
FfiAudioObserverEvent audio_observer_take(const NativeAudioObserver& observer) noexcept {
  auto& state = *observer.state();
  std::lock_guard lock(state.mutex);
  if (state.events.empty()) return {};
  auto event = std::move(state.events.front());
  state.events.pop_front();
  return event;
}
std::uint64_t audio_observer_take_dropped(const NativeAudioObserver& observer) noexcept {
  auto& state = *observer.state();
  std::lock_guard lock(state.mutex);
  return std::exchange(state.dropped, 0);
}

NativeEncodedAudioSink::NativeEncodedAudioSink(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeEncodedAudioSink::~NativeEncodedAudioSink() { close_encoded_audio_sink(*this); }
const std::unique_ptr<NativeEncodedAudioSink::State>&
NativeEncodedAudioSink::state() const noexcept { return state_; }

NativeAudioSink::NativeAudioSink(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeAudioSink::~NativeAudioSink() { close_audio_sink(*this); }
const std::unique_ptr<NativeAudioSink::State>& NativeAudioSink::state() const noexcept {
  return state_;
}

NativeAudioSource::NativeAudioSource(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeAudioSource::~NativeAudioSource() {
  close_audio_source(*this);
  if (state_ && state_->signaling_thread) {
    state_->signaling_thread->BlockingCall([this] { state_->source = nullptr; });
  }
}
const std::unique_ptr<NativeAudioSource::State>& NativeAudioSource::state() const noexcept {
  return state_;
}
NativeAudioTrack::NativeAudioTrack(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeAudioTrack::~NativeAudioTrack() = default;
const std::unique_ptr<NativeAudioTrack::State>& NativeAudioTrack::state() const noexcept {
  return state_;
}

std::unique_ptr<NativeAudioSource> create_audio_source(
    const NativePeerConnectionFactory& factory, std::uint8_t& failure) noexcept {
  failure = 0;
  auto state = std::make_unique<NativeAudioSource::State>();
  state->signaling_thread = factory.signaling_thread();
  state->worker_thread = factory.worker_thread();
  state->processing = factory.audio_processing();
  state->processing_config = factory.audio_processing_config();
  state->installed_processing = factory.installed_audio_processing();
  // Native capture owns the shared APM's capture thread. Do not call that
  // instance from a second custom capture path or manufacture another module.
  if (state->processing && factory.audio_device()) { failure = 1; return nullptr; }
  if (!state->signaling_thread || !state->worker_thread) return nullptr;
  state->signaling_thread->BlockingCall(
      [&] { state->source = webrtc::make_ref_counted<PushAudioSource>(); });
  return std::make_unique<NativeAudioSource>(std::move(state));
}
std::unique_ptr<NativeAudioSource> create_encoded_audio_source(
    const NativePeerConnectionFactory& factory, std::uint8_t channels) noexcept {
  if (channels != 1 && channels != 2) return nullptr;
  std::uint8_t failure = 0;
  auto source = create_audio_source(factory, failure);
  if (source) source->state()->encoded_channels = channels;
  return source;
}
rust::Vec<FfiAudioCodecCapability> peer_audio_codec_capabilities(
    const NativePeerConnectionFactory& factory, bool sender) noexcept {
  rust::Vec<FfiAudioCodecCapability> result;
  if (!factory.factory() || !factory.signaling_thread()) return result;
  const auto capabilities = factory.signaling_thread()->BlockingCall([&] {
    return sender
        ? factory.factory()->GetRtpSenderCapabilities(webrtc::MediaType::AUDIO)
        : factory.factory()->GetRtpReceiverCapabilities(webrtc::MediaType::AUDIO);
  });
  for (const auto& codec : capabilities.codecs) {
    FfiAudioCodecCapability item;
    item.format.name = codec.name;
    for (const auto& [key, value] : codec.parameters) {
      item.format.parameters.push_back({key, value});
    }
    item.clock_rate = codec.clock_rate.value_or(-1);
    item.channels = codec.num_channels.value_or(-1);
    result.push_back(std::move(item));
  }
  return result;
}
bool audio_source_capture_active(const NativeAudioSource& source) noexcept {
  return PcmCaptureScope::Active(source.state().get()) || installed_audio_processor_reentry(source.state()->installed_processing);
}
bool close_audio_source(const NativeAudioSource& source) noexcept {
  if (audio_source_capture_active(source)) return false;
  auto& state = *source.state();
  if (!state.source || !state.signaling_thread) return false;
  {
    std::lock_guard lock(state.frame_mutex);
    if (state.budget_id) {
      opus_carrier::Unregister(state.budget_id);
      state.budget_id = 0;
      state.budget.reset();
    }
    state.signaling_thread->BlockingCall([&] { state.source->Close(); });
  }
  return true;
}
bool audio_source_push_pcm(const NativeAudioSource& source,
                           rust::Slice<const std::int16_t> samples,
                           std::uint32_t sample_rate_hz,
                           std::uint8_t channels,
                           std::int64_t timestamp_us,
                           std::int64_t capture_time_us,
                           std::uint8_t& failure,
                           std::int32_t& processing_error,
                           std::uint8_t& wrapper_error) noexcept {
  failure = 0;
  processing_error = 0; wrapper_error = 0;
  auto& state = *source.state();
  PcmCaptureScope caller_scope(&state);
  if (!caller_scope.admitted()) { failure = 3; return false; }
  const auto capture_ms = capture_time_us < 0 ? std::nullopt
      : std::optional<std::int64_t>(capture_time_us / 1000);
  if (capture_time_us < -1 || capture_time_us / 1000 > std::numeric_limits<std::int64_t>::max() / 48000) return false;
  if (timestamp_us < 0 ||
      sample_rate_hz < 100 || sample_rate_hz > 48000 ||
      (channels != 1 && channels != 2 && channels != 4 && channels != 6 && channels != 8) ||
      samples.size() != (sample_rate_hz / 100) * channels) return false;
  if (state.processing && ((sample_rate_hz != 8000 && sample_rate_hz != 16000 && sample_rate_hz != 32000 && sample_rate_hz != 48000) || (channels != 1 && channels != 2))) return false;
  if (!state.processing)
    return state.source->Push(samples.data(), static_cast<int>(sample_rate_hz),
                              channels, sample_rate_hz / 100, capture_ms);
  std::lock_guard lock(state.frame_mutex);
  // This is an owned input copy for in-place native processing, not initialized
  // output used to disguise a failed or partial processing result.
  std::vector<std::int16_t> processed(samples.begin(), samples.end());
  state.worker_thread->BlockingCall([&] {
    PcmCaptureScope worker_scope(&state);
    // BlockingCall may run directly when the caller already owns this worker.
    // Its caller scope already protects that case; a different worker needs its
    // own scope before APM can call environment dependencies.
    if (state.installed_processing) {
      const auto options = state.source->options();
      const auto option = [](std::optional<bool> v) -> std::uint8_t { return !v ? 0 : (*v ? 2 : 1); };
      bool needs_feedback = false;
      processing_error = installed_audio_processor_capture(state.installed_processing,
          static_cast<std::int32_t>(sample_rate_hz), channels,
          rust::Slice<std::int16_t>(processed.data(), processed.size()),
          option(options.echo_cancellation), option(options.noise_suppression), option(options.auto_gain_control),
          !state.source->HasSinks(), needs_feedback, wrapper_error);
      if (needs_feedback) failure = 2;
      else if (wrapper_error) failure = 4;
      else if (processing_error != webrtc::AudioProcessing::kNoError) failure = 1;
      return;
    }
    const auto current = state.processing->GetConfig();
    auto config = current;
    const auto options = state.source->options();
    config.echo_canceller.enabled = options.echo_cancellation.value_or(state.processing_config.echo_cancellation);
    config.noise_suppression.enabled = options.noise_suppression.value_or(state.processing_config.noise_suppression != 0);
    if (state.processing_config.noise_suppression != 0)
      config.noise_suppression.level = static_cast<webrtc::AudioProcessing::Config::NoiseSuppression::Level>(
          state.processing_config.noise_suppression - 1);
    const bool gain = options.auto_gain_control.value_or(state.processing_config.gain_control != 0);
    config.gain_controller1.enabled = gain && state.processing_config.gain_control != 4;
    config.gain_controller2.enabled = gain && state.processing_config.gain_control == 4;
    if (state.processing_config.gain_control >= 1 && state.processing_config.gain_control <= 3) {
      config.gain_controller1.mode = static_cast<webrtc::AudioProcessing::Config::GainController1::Mode>(
          state.processing_config.gain_control - 1);
      config.gain_controller1.analog_gain_controller.enabled = state.processing_config.gain_control == 1;
    }
    if (state.processing_config.gain_control == 4)
      config.gain_controller2.adaptive_digital.enabled = true;
    if (config.echo_canceller.enabled ||
        (config.gain_controller1.enabled && config.gain_controller1.mode ==
             webrtc::AudioProcessing::Config::GainController1::kAdaptiveAnalog)) {
      // Custom PCM has no render/delay or microphone gain feedback contract.
      // These features remain usable through the real native capture transport.
      failure = 2;
      return;
    }
    // The voice engine's generic Linux options force analog AGC and High NS.
    // At this custom capture boundary preserve the actual selected algorithms,
    // while applying the source's native enable/disable choices. Reconfigure
    // only when those settings differ, retaining ordinary DSP history.
    if (config.echo_canceller.enabled != current.echo_canceller.enabled ||
        config.noise_suppression.enabled != current.noise_suppression.enabled ||
        config.noise_suppression.level != current.noise_suppression.level ||
        config.gain_controller1 != current.gain_controller1 ||
        config.gain_controller2 != current.gain_controller2)
      state.processing->ApplyConfig(config);
    state.processing->set_output_will_be_muted(!state.source->HasSinks());
    const webrtc::StreamConfig stream(static_cast<int>(sample_rate_hz), channels);
    processing_error = state.processing->ProcessStream(processed.data(), stream, stream, processed.data());
    if (processing_error != webrtc::AudioProcessing::kNoError) failure = 1;
  });
  if (failure) return false;
  return state.source->Push(processed.data(), static_cast<int>(sample_rate_hz),
                            channels, sample_rate_hz / 100, capture_ms);
}
bool audio_source_push_opus(const NativeAudioSource& source,
                            rust::Slice<const std::uint8_t> payload,
                            std::uint32_t rtp_timestamp,
                            std::uint32_t samples_per_channel) noexcept {
  return audio_source_push_opus_at(source, payload, rtp_timestamp,
                                   samples_per_channel, -1);
}
bool audio_source_push_opus_at(const NativeAudioSource& source,
                            rust::Slice<const std::uint8_t> payload,
                            std::uint32_t rtp_timestamp,
                            std::uint32_t samples_per_channel,
                            std::int64_t capture_time_us) noexcept {
  return audio_source_push_opus_with_level_at(source, payload, rtp_timestamp,
      samples_per_channel, capture_time_us, false, 0, false);
}
bool audio_source_push_opus_with_level_at(const NativeAudioSource& source,
                            rust::Slice<const std::uint8_t> payload,
                            std::uint32_t rtp_timestamp,
                            std::uint32_t samples_per_channel,
                            std::int64_t capture_time_us, bool has_level,
                            std::uint8_t level_dbov, bool voice_activity) noexcept {
  if (capture_time_us < -1 || (has_level && level_dbov > 127)) return false;
  using namespace opus_carrier;
  // Validate the genuine encoded payload independently of the fixed private
  // input layout shared by PushOpus and OpusCarrierEncoder.
  const auto channels = source.state()->encoded_channels;
  const auto block_bytes = kBytesPerChannelBlock * kChannels;
  if ((channels != 1 && channels != 2) ||
      samples_per_channel == 0 || samples_per_channel > 2880 ||
      samples_per_channel % 480 != 0 || payload.empty() ||
      payload.size() > kMaxPayloadBytes ||
      ((payload[0] & 4) != 0) != (channels == 2) ||
      OpusSamples({payload.data(), payload.size()}) != samples_per_channel ||
      payload.size() >
          (samples_per_channel / 480) * (block_bytes - kHeaderBytes)) return false;
  auto& state = *source.state();
  std::lock_guard lock(state.frame_mutex);
  if (!state.budget) {
    state.budget = std::make_shared<Budget>();
    state.budget_id = Register(state.budget);
  }
  return state.source->PushOpus({payload.data(), payload.size()}, rtp_timestamp,
                                samples_per_channel, capture_time_us,
                                state.budget, state.budget_id, has_level,
                                level_dbov, voice_activity);
}
std::unique_ptr<NativeAudioTrack> create_microphone_track(
    const NativePeerConnectionFactory& factory, rust::Str id) noexcept {
  if (!factory.audio_device() || !factory.factory() || id.empty()) return nullptr;
  auto source = factory.factory()->CreateAudioSource(webrtc::AudioOptions{});
  if (!source) return nullptr;
  auto track = factory.factory()->CreateAudioTrack(
      std::string(id.data(), id.size()), source.get());
  if (!track) return nullptr;
  auto state = std::make_unique<NativeAudioTrack::State>();
  state->track = std::move(track);
  state->microphone_source = std::move(source);
  state->signaling_thread = factory.signaling_thread();
  return std::make_unique<NativeAudioTrack>(std::move(state));
}
std::unique_ptr<NativeAudioTrack> create_audio_track(
    const NativePeerConnectionFactory& factory, const NativeAudioSource& source,
    rust::Str id) noexcept {
  if (!source.state()->source || source.state()->source->state() !=
      webrtc::MediaSourceInterface::kLive || id.empty()) return nullptr;
  auto track = factory.factory()->CreateAudioTrack(
      std::string(id.data(), id.size()), source.state()->source.get());
  if (!track) return nullptr;
  auto state = std::make_unique<NativeAudioTrack::State>();
  state->track = std::move(track);
  state->signaling_thread = factory.signaling_thread();
  state->opus_frames = source.state()->encoded_channels != 0;
  return std::make_unique<NativeAudioTrack>(std::move(state));
}
std::unique_ptr<NativeMediaTrack> audio_track_reference(const NativeAudioTrack& track) noexcept {
  return std::make_unique<NativeMediaTrack>(track.state()->track);
}
std::unique_ptr<NativeAudioSink> audio_track_attach_sink(const NativeAudioTrack& track) noexcept {
  if (track.state()->opus_frames || !track.state()->signaling_thread || !track.state()->track ||
      track.state()->track->state() != webrtc::MediaStreamTrackInterface::kLive)
    return nullptr;
  auto state = std::make_unique<NativeAudioSink::State>();
  state->track = track.state()->track;
  state->signaling_thread = track.state()->signaling_thread;
  state->collector = std::make_unique<ReceivedAudioCollector>();
  state->signaling_thread->BlockingCall([&] { state->track->AddSink(state->collector.get()); });
  return std::make_unique<NativeAudioSink>(std::move(state));
}
bool rtp_sender_set_audio_track(const NativeRtpSender& sender, const NativeAudioTrack& track) noexcept {
  return sender.sender()->SetTrack(track.state()->track.get());
}
FfiAudioSourceOptions audio_options_default() noexcept {
  return WriteAudioOptions(webrtc::AudioOptions{});
}
bool audio_options_equal(const FfiAudioSourceOptions& a, const FfiAudioSourceOptions& b) noexcept {
  return ReadAudioOptions(a) == ReadAudioOptions(b);
}
FfiAudioSourceOptions audio_options_set_all(const FfiAudioSourceOptions& value,
                                          const FfiAudioSourceOptions& changes) noexcept {
  auto options = ReadAudioOptions(value);
  options.SetAll(ReadAudioOptions(changes));
  return WriteAudioOptions(options);
}
rust::String audio_options_description(const FfiAudioSourceOptions& value) noexcept {
  return rust::String(ReadAudioOptions(value).ToString());
}
std::uint8_t audio_source_set_options(const NativeAudioSource& source,
                                      const FfiAudioSourceOptions& options) noexcept {
  // Source options reach the send channel. Jitter options do not become
  // receive-channel/NetEq settings through this path; preserve their values.
  if (auto error = CheckAudioOptions(options)) return error;
  const auto native = ReadAudioOptions(options);
  source.state()->signaling_thread->BlockingCall([&] { source.state()->source->SetOptions(native); });
  return 0;  // Native SetOptions is void; this is invocation, not effective state.
}
std::uint8_t audio_source_set_volume(const NativeAudioSource& source, double volume) noexcept {
  if (!std::isfinite(volume) || volume < 0 || volume > 10) return 2;
  source.state()->signaling_thread->BlockingCall([&] { source.state()->source->SetVolume(volume); });
  return 0;
}
std::uint8_t audio_track_set_volume(const NativeAudioTrack& track, double volume) noexcept {
  if (!std::isfinite(volume) || volume < 0 || volume > 10) return 2;
  track.state()->signaling_thread->BlockingCall([&] { track.state()->track->SetVolume(volume); });
  return 0;
}
std::uint8_t rtp_sender_set_audio_track_volume(const NativePeerConnection& peer,
                                             const NativeRtpSender& sender, double volume) noexcept {
  if (!std::isfinite(volume) || volume < 0 || volume > 10) return 2;
  return peer.signaling_thread()->BlockingCall([&]() -> std::uint8_t {
    auto track = sender.sender()->track();
    if (!track || track->kind() != webrtc::MediaStreamTrackInterface::kAudioKind) return 3;
    static_cast<webrtc::AudioTrackInterface*>(track.get())->SetVolume(volume);
    return 0;
  });
}
FfiAudioSourceOptions audio_source_options(const NativeAudioSource& source) noexcept {
  return source.state()->signaling_thread->BlockingCall([&] {
    return WriteAudioOptions(source.state()->source->options());
  });
}
std::uint8_t audio_source_state(const NativeAudioSource& source) noexcept {
  return source.state()->signaling_thread->BlockingCall([&] {
    return static_cast<std::uint8_t>(source.state()->source->state());
  });
}
bool audio_source_remote(const NativeAudioSource& source) noexcept {
  return source.state()->signaling_thread->BlockingCall([&] { return source.state()->source->remote(); });
}
double audio_source_volume(const NativeAudioSource& source) noexcept {
  return source.state()->signaling_thread->BlockingCall([&] { return source.state()->source->GetVolume(); });
}
FfiAudioTrackStatus audio_track_status(const NativeAudioTrack& track) noexcept {
  return track.state()->signaling_thread->BlockingCall([&] {
    return ReadAudioTrackStatus(*track.state()->track);
  });
}
FfiAudioTrackStatus rtp_sender_audio_track_status(const NativePeerConnection& peer,
                                                const NativeRtpSender& sender) noexcept {
  return peer.signaling_thread()->BlockingCall([&] {
    auto track = sender.sender()->track();
    if (!track || track->kind() != webrtc::MediaStreamTrackInterface::kAudioKind)
      return FfiAudioTrackStatus{};
    return ReadAudioTrackStatus(*static_cast<webrtc::AudioTrackInterface*>(track.get()));
  });
}
std::uint8_t audio_track_state(const NativeAudioTrack& track) noexcept {
  return static_cast<std::uint8_t>(track.state()->track->state());
}
FfiOptionalI32 audio_track_signal_level(const NativeAudioTrack& track) noexcept {
  return track.state()->signaling_thread->BlockingCall([&] {
    FfiOptionalI32 out;
    int level = 0;
    out.present = track.state()->track->GetSignalLevel(&level);
    out.value = level;
    return out;
  });
}
double audio_track_volume(const NativeAudioTrack& track) noexcept {
  return track.state()->signaling_thread->BlockingCall([&] { return track.state()->track->GetVolume(); });
}
rust::String audio_track_kind(const NativeAudioTrack& track) noexcept {
  return track.state()->track->kind();
}
rust::String audio_track_id(const NativeAudioTrack& track) noexcept {
  return track.state()->track->id();
}
bool audio_track_enabled(const NativeAudioTrack& track) noexcept {
  return track.state()->track->enabled();
}
bool audio_track_set_enabled(const NativeAudioTrack& track, bool enabled) noexcept {
  return track.state()->track->set_enabled(enabled);
}
bool audio_track_set_processing_options(const NativeAudioTrack& track,
                                        std::uint8_t echo, std::uint8_t noise,
                                        std::uint8_t gain,
                                        rust::String& error) noexcept {
  if (!track.state()->signaling_thread || echo > 3 || noise > 3 || gain > 3) {
    error = "invalid audio processing options or closed signaling thread";
    return false;
  }
  const auto option = [](std::uint8_t choice, std::optional<bool>& enabled,
                         std::optional<webrtc::AudioProcessingMode>& mode) {
    enabled = choice != 0;
    if (choice != 0) {
      mode = static_cast<webrtc::AudioProcessingMode>(choice - 1);
    }
  };
  webrtc::AudioOptions options;
  option(echo, options.echo_cancellation, options.echo_cancellation_mode);
  option(noise, options.noise_suppression, options.noise_suppression_mode);
  option(gain, options.auto_gain_control, options.auto_gain_control_mode);
  webrtc::AudioProcessingOptionsResult result;
  track.state()->signaling_thread->BlockingCall([&] {
    result = track.state()->track->SetAudioProcessingOptions(options);
  });
  if (!result.ok()) {
    error = result.message;
    return false;
  }
  return true;
}
std::unique_ptr<NativeAudioSink> rtp_receiver_attach_audio_sink(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver) noexcept {
  if (!peer.peer() || !peer.signaling_thread()) return nullptr;
  const auto remote = receiver.receiver();
  bool belongs = false;
  for (const auto& candidate : peer.peer()->GetReceivers()) {
    if (candidate == remote) belongs = true;
  }
  if (!belongs || !remote || !remote->track() ||
      remote->track()->kind() != webrtc::MediaStreamTrackInterface::kAudioKind ||
      !peer.reserve_audio_receiver(remote->id())) {
    return nullptr;
  }
  auto state = std::make_unique<NativeAudioSink::State>();
  state->peer = &peer;
  state->receiver_id = remote->id();
  state->track = static_cast<webrtc::AudioTrackInterface*>(remote->track().get());
  state->collector = std::make_unique<ReceivedAudioCollector>();
  state->signaling_thread = peer.signaling_thread();
  auto* track = state->track.get();
  auto* collector = state->collector.get();
  state->signaling_thread->BlockingCall([&] { track->AddSink(collector); });
  return std::make_unique<NativeAudioSink>(std::move(state));
}
FfiReceivedAudioFrame audio_sink_take_frame(const NativeAudioSink& sink) noexcept {
  // A retired receiver must not drain stale PCM or trigger headless playout
  // for unrelated live receivers when its empty sink is polled.
  if (sink.state()->closed.load() ||
      sink.state()->track->state() == webrtc::MediaStreamTrackInterface::kEnded)
    return {};
  auto frame = sink.state()->collector->Take();
  if (!frame.valid && !sink.state()->closed.load() && sink.state()->peer &&
      pump_headless_audio(*sink.state()->peer)) {
    frame = sink.state()->collector->Take();
  }
  return frame;
}
std::uint64_t audio_sink_dropped_frames(const NativeAudioSink& sink) noexcept {
  return sink.state()->collector->DroppedFrames();
}
bool close_audio_sink(const NativeAudioSink& sink) noexcept {
  auto& state = *sink.state();
  if (state.closed.exchange(true)) return true;
  state.collector->Deactivate();
  state.signaling_thread->BlockingCall([&] {
    state.track->RemoveSink(state.collector.get());
  });
  if (state.peer) state.peer->release_audio_receiver(state.receiver_id);
  return true;
}
std::unique_ptr<NativeEncodedAudioSink> rtp_receiver_attach_encoded_audio_sink(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver) noexcept {
  if (!peer.peer() || !peer.worker_thread() || !receiver.receiver()) return nullptr;
  const auto remote = receiver.receiver();
  bool belongs = false;
  for (const auto& candidate : peer.peer()->GetReceivers()) {
    if (candidate == remote) belongs = true;
  }
  if (!belongs || remote->media_type() != webrtc::MediaType::AUDIO) return nullptr;
  bool opus = false;
  for (const auto& codec : remote->GetParameters().codecs) {
    if (codec.name == "opus" || codec.name == "OPUS") opus = true;
  }
  if (!opus || !peer.reserve_audio_receiver(remote->id())) return nullptr;
  auto state = std::make_unique<NativeEncodedAudioSink::State>();
  state->peer = &peer;
  state->receiver_id = remote->id();
  state->collector = webrtc::make_ref_counted<EncodedAudioCollector>(peer.readiness());
  peer.worker_thread()->BlockingCall([&] {
    peer.frame_transformer_hosts().Receiver(peer, remote)->SetCollector(state->collector);
  });
  return std::make_unique<NativeEncodedAudioSink>(std::move(state));
}
FfiEncodedAudioFrame encoded_audio_sink_take_frame(
    const NativeEncodedAudioSink& sink) noexcept {
  return sink.state()->collector->Take();
}
std::uint64_t encoded_audio_sink_dropped_frames(
    const NativeEncodedAudioSink& sink) noexcept {
  return sink.state()->collector->DroppedFrames();
}
bool close_encoded_audio_sink(const NativeEncodedAudioSink& sink) noexcept {
  auto& state = *sink.state();
  if (state.closed.exchange(true)) return true;
  state.collector->Deactivate();
  state.peer->release_audio_receiver(state.receiver_id);
  return true;
}
std::unique_ptr<NativeRtpTransceiver> peer_add_audio_transceiver_value(
    const NativePeerConnection& peer, const NativeAudioTrack& track,
    const NativeRtpTransceiverInit& init, bool& valid,
    std::unique_ptr<NativeRtcError>& error) noexcept {
  valid = transceiver_init_consumption_safe(peer, init, false);
  if (!valid) return nullptr;
  auto result = peer.peer()->AddTransceiver(track.state()->track, init.value());
  if (!result.ok()) { error = std::make_unique<NativeRtcError>(result.MoveError()); return nullptr; }
  return wrap_rtp_transceiver(result.MoveValue());
}
std::unique_ptr<NativeRtpTransceiver> peer_add_audio_transceiver(
    const NativePeerConnection& peer, const NativeAudioTrack& track,
    std::uint8_t direction, std::uint8_t& error_type,
    rust::String& error) noexcept {
  webrtc::RtpTransceiverInit init;
  switch (direction) {
    case 0: init.direction = webrtc::RtpTransceiverDirection::kSendRecv; break;
    case 1: init.direction = webrtc::RtpTransceiverDirection::kSendOnly; break;
    case 2: init.direction = webrtc::RtpTransceiverDirection::kRecvOnly; break;
    case 3: init.direction = webrtc::RtpTransceiverDirection::kInactive; break;
    default:
      error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_PARAMETER);
      error = "invalid RTP transceiver direction";
      return nullptr;
  }
  auto result = peer.peer()->AddTransceiver(track.state()->track, init);
  if (!result.ok()) {
    error_type = static_cast<std::uint8_t>(result.error().type());
    error = result.error().message();
    return nullptr;
  }
  auto transceiver = result.MoveValue();
  if (track.state()->opus_frames) {
    if (auto transformer = peer.opus_transformer()) {
      // A stable host composes factory metadata with caller transformation.
      // ChannelSend must never initialize a second native delegate.
      peer.frame_transformer_hosts().Sender(peer, transceiver->sender());
    }
  }
  return wrap_rtp_transceiver(std::move(transceiver));
}

}  // namespace pulsebeam::webrtc_sys
