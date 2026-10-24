#pragma once

#include <deque>
#include <limits>
#include <mutex>
#include <optional>
#include <utility>

#include "api/scoped_refptr.h"
#include "api/video/recordable_encoded_frame.h"
#include "api/video/video_sink_interface.h"
#include "pulsebeam-webrtc-sys/native/media_values.h"
#include "pulsebeam-webrtc-sys/native/video_sink_queue.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {

// Source encoded-output is the SDK recordable-frame interface, not the RTP
// receiver's transformable-frame interception slot. Retain the actual immutable
// encoded buffer in a bounded queue; copy bytes only when the caller polls.
class SourceEncodedFrameSink final
    : public webrtc::VideoSinkInterface<webrtc::RecordableEncodedFrame> {
 public:
  void OnFrame(const webrtc::RecordableEncodedFrame& frame) override {
    std::lock_guard lock(mutex_);
    if (!active_) return;
    auto buffer = frame.encoded_buffer();
    const auto bytes = buffer ? buffer->size() : 0;
    if (bytes > kMaxBytes || (bytes != 0 && !buffer->data())) {
      Lost();
      return;
    }
    while (frames_.size() >= kMaxFrames || queued_bytes_ + bytes > kMaxBytes) {
      queued_bytes_ -= frames_.front().buffer ? frames_.front().buffer->size() : 0;
      frames_.pop_front();
      Lost();
    }
    frames_.push_back({std::move(buffer), frame.color_space(), frame.video_rotation(),
                       frame.codec(), frame.is_key_frame(), frame.resolution(),
                       frame.render_time()});
    queued_bytes_ += bytes;
  }
  void OnDiscardedFrame() override { notifications_.OnDiscardedFrame(); }
  void OnConstraintsChanged(const webrtc::VideoTrackSourceConstraints& constraints) override {
    notifications_.OnConstraintsChanged(constraints);
  }
  FfiRecordableVideoFrame Take() {
    std::lock_guard lock(mutex_);
    if (frames_.empty()) return {};
    auto frame = std::move(frames_.front());
    frames_.pop_front();
    const auto bytes = frame.buffer ? frame.buffer->size() : 0;
    queued_bytes_ -= bytes;
    FfiRecordableVideoFrame result{};
    result.valid = true;
    result.has_buffer = static_cast<bool>(frame.buffer);
    result.data.reserve(bytes);
    for (std::size_t i = 0; i < bytes; ++i) result.data.push_back(frame.buffer->data()[i]);
    result.codec = static_cast<std::int32_t>(frame.codec);
    result.key_frame = frame.key_frame;
    result.width = frame.resolution.width;
    result.height = frame.resolution.height;
    result.has_rotation = frame.rotation.has_value();
    result.rotation = static_cast<std::uint16_t>(frame.rotation.value_or(webrtc::kVideoRotation_0));
    result.has_color_space = frame.color_space.has_value();
    if (frame.color_space) result.color_space = MediaColorToFfi(*frame.color_space);
    result.render_time_us = MediaTimestampBits(frame.render_time);
    return result;
  }
  FfiVideoSinkEvent TakeEvent() { return notifications_.TakeEvent(); }
  std::uint64_t DroppedEvents() { return notifications_.DroppedEvents(); }
  std::uint64_t DroppedFrames() {
    std::lock_guard lock(mutex_);
    return dropped_;
  }
  void Deactivate() {
    {
      std::lock_guard lock(mutex_);
      active_ = false;
      frames_.clear();
      queued_bytes_ = 0;
    }
    notifications_.Deactivate();
  }

 private:
  struct Frame {
    webrtc::scoped_refptr<const webrtc::EncodedImageBufferInterface> buffer;
    std::optional<webrtc::ColorSpace> color_space;
    std::optional<webrtc::VideoRotation> rotation;
    webrtc::VideoCodecType codec;
    bool key_frame;
    webrtc::RecordableEncodedFrame::EncodedResolution resolution;
    webrtc::Timestamp render_time;
  };
  void Lost() {
    if (dropped_ != std::numeric_limits<std::uint64_t>::max()) ++dropped_;
  }
  static constexpr std::size_t kMaxFrames = 8;
  static constexpr std::size_t kMaxBytes = 64 * 1024 * 1024;
  std::mutex mutex_;
  std::deque<Frame> frames_;
  std::size_t queued_bytes_ = 0;
  std::uint64_t dropped_ = 0;
  bool active_ = true;
  FrameSink notifications_;
};

}  // namespace pulsebeam::webrtc_sys
