#pragma once

#include <cstddef>
#include <cstdint>
#include <deque>
#include <limits>
#include <memory>
#include <mutex>
#include <optional>
#include <utility>

#include "api/video/video_frame.h"
#include "api/video/video_sink_interface.h"
#include "api/video_track_source_constraints.h"
#include "rtc_base/time_utils.h"
#include "pulsebeam-webrtc-sys/native/codec.h"
#include "pulsebeam-webrtc-sys/native/raw_video_frame.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {

// Owning callback collector. Attachment owners retain this object until native
// RemoveSink returns. No callback pointer or borrowed constraints escape.
class FrameSink final : public webrtc::VideoSinkInterface<webrtc::VideoFrame> {
 public:
  explicit FrameSink(std::optional<rust::Box<RustDecodedVideoObserver>> observer = std::nullopt)
      : observer_(std::move(observer)) {}

  void OnFrame(const webrtc::VideoFrame& frame) override {
    std::lock_guard lock(mutex_);
    if (!active_) return;
    // Optional synchronous facts precede retention loss. This is the SAME sink:
    // no extra VideoSinkWants, decoder, timing, or delivery policy is installed.
    if (observer_) {
      decoded_video_frame_observed(**observer_, std::make_unique<NativeRawVideoFrame>(frame),
                                   webrtc::TimeMicros());
    }
    // Keep the existing four-frame/16 MiB decoded-frame retention policy.
    if (frame.width() <= 0 || frame.height() <= 0) {
      Lost();
      return;
    }
    const auto bytes = raw_buffer_queue_bytes(*frame.video_frame_buffer(), kMaxBytes);
    if (bytes > kMaxBytes) {
      Lost();
      return;
    }
    while (frames_.size() >= kMaxFrames || queued_bytes_ + bytes > kMaxBytes) {
      queued_bytes_ -= frames_.front().bytes;
      frames_.pop_front();
      Lost();
    }
    frames_.push_back({std::make_unique<NativeRawVideoFrame>(frame), static_cast<std::size_t>(bytes)});
    queued_bytes_ += static_cast<std::size_t>(bytes);
  }

  void OnDiscardedFrame() override {
    std::lock_guard lock(mutex_);
    if (!active_) return;
    FfiVideoSinkEvent event{};
    event.valid = true;
    event.kind = 0;
    PushEvent(std::move(event));
  }

  void OnConstraintsChanged(
      const webrtc::VideoTrackSourceConstraints& constraints) override {
    std::lock_guard lock(mutex_);
    if (!active_) return;
    FfiVideoSinkEvent event{};
    event.valid = true;
    event.kind = 1;
    event.has_min_fps = constraints.min_fps.has_value();
    event.min_fps = constraints.min_fps.value_or(0);
    event.has_max_fps = constraints.max_fps.has_value();
    event.max_fps = constraints.max_fps.value_or(0);
    PushEvent(std::move(event));
  }

  std::unique_ptr<NativeVideoFrame> Take() {
    auto frame = TakeNative();
    return frame ? raw_frame_legacy(*frame) : nullptr;
  }

  std::unique_ptr<NativeRawVideoFrame> TakeNative() {
    std::lock_guard lock(mutex_);
    if (frames_.empty()) return nullptr;
    auto frame = std::move(frames_.front());
    frames_.pop_front();
    queued_bytes_ -= frame.bytes;
    return std::move(frame.frame);
  }

  FfiVideoSinkEvent TakeEvent() {
    std::lock_guard lock(mutex_);
    if (events_.empty()) return {};
    auto event = std::move(events_.front());
    events_.pop_front();
    return event;
  }

  std::uint64_t DroppedFrames() {
    std::lock_guard lock(mutex_);
    return dropped_frames_;
  }

  std::uint64_t DroppedEvents() {
    std::lock_guard lock(mutex_);
    return dropped_events_;
  }

  void Deactivate() {
    std::lock_guard lock(mutex_);
    active_ = false;
    frames_.clear();
    queued_bytes_ = 0;
    // Control notifications already accepted remain consumable after close.
  }

  // Release user state only after the attachment owner's RemoveSink barrier.
  // Closed sink handles can remain alive to drain accepted control records.
  void RetireObserver() {
    std::optional<rust::Box<RustDecodedVideoObserver>> observer;
    {
      std::lock_guard lock(mutex_);
      observer.swap(observer_);
    }
  }

 private:
  void Lost() {
    if (dropped_frames_ != std::numeric_limits<std::uint64_t>::max()) {
      ++dropped_frames_;
    }
  }

  void PushEvent(FfiVideoSinkEvent event) {
    if (events_.size() == 8) {
      events_.pop_front();
      if (dropped_events_ != std::numeric_limits<std::uint64_t>::max()) {
        ++dropped_events_;
      }
    }
    events_.push_back(std::move(event));
  }

  struct QueuedFrame {
    std::unique_ptr<NativeRawVideoFrame> frame;
    std::size_t bytes;
  };
  static constexpr std::size_t kMaxFrames = 4;
  static constexpr std::size_t kMaxBytes = 16 * 1024 * 1024;
  std::mutex mutex_;
  std::optional<rust::Box<RustDecodedVideoObserver>> observer_;
  std::deque<QueuedFrame> frames_;
  std::deque<FfiVideoSinkEvent> events_;
  std::size_t queued_bytes_ = 0;
  std::uint64_t dropped_frames_ = 0;
  std::uint64_t dropped_events_ = 0;
  bool active_ = true;
};

}  // namespace pulsebeam::webrtc_sys
