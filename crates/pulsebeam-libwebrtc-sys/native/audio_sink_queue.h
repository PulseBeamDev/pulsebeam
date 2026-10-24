#pragma once

#include <cstdint>
#include <cstring>
#include <deque>
#include <limits>
#include <mutex>
#include <optional>
#include <utility>
#include <vector>

#include "api/audio/audio_frame.h"
#include "api/media_stream_interface.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {

// Internal owning-copy collector shared by local and received track sinks.
// No callback pointer or caller storage survives OnData. Attachment owners
// must keep this collector alive until native RemoveSink has returned.
class ReceivedAudioCollector final : public webrtc::AudioTrackSinkInterface {
 public:
  void OnData(const void* data, int bits, int rate, size_t channels,
              size_t frames) override {
    OnData(data, bits, rate, channels, frames, std::nullopt);
  }
  void OnData(const void* data, int bits, int rate, size_t channels,
              size_t frames, std::optional<int64_t> capture_ms) override {
    std::lock_guard lock(mutex_);
    if (!active_) return;
    if (!data || bits != 16 || rate <= 0 || channels < 1 || channels > 16 ||
        frames == 0 || frames > webrtc::AudioFrame::kMaxDataSizeSamples / channels) {
      if (dropped_ != std::numeric_limits<std::uint64_t>::max()) ++dropped_;
      return;
    }
    Frame frame;
    frame.rate = static_cast<std::uint32_t>(rate);
    frame.channels = static_cast<std::uint8_t>(channels);
    frame.frames = static_cast<std::uint32_t>(frames);
    frame.capture_ms = capture_ms;
    frame.samples.resize(channels * frames);
    std::memcpy(frame.samples.data(), data, frame.samples.size() * sizeof(int16_t));
    if (frames_.size() == 8) {
      frames_.pop_front();
      if (dropped_ != std::numeric_limits<std::uint64_t>::max()) ++dropped_;
    }
    frames_.push_back(std::move(frame));
  }
  FfiReceivedAudioFrame Take() {
    std::lock_guard lock(mutex_);
    FfiReceivedAudioFrame out;
    if (frames_.empty()) return out;
    auto frame = std::move(frames_.front());
    frames_.pop_front();
    out.valid = true;
    out.sample_rate_hz = frame.rate;
    out.channels = frame.channels;
    out.samples_per_channel = frame.frames;
    out.has_capture_time = frame.capture_ms.has_value();
    out.capture_time_ms = frame.capture_ms.value_or(0);
    for (auto sample : frame.samples) out.samples.push_back(sample);
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
  }

 private:
  struct Frame {
    std::vector<std::int16_t> samples;
    std::uint32_t rate = 0;
    std::uint8_t channels = 0;
    std::uint32_t frames = 0;
    std::optional<std::int64_t> capture_ms;
  };
  mutable std::mutex mutex_;
  std::deque<Frame> frames_;
  std::uint64_t dropped_ = 0;
  bool active_ = true;
};

}  // namespace pulsebeam::webrtc_sys
