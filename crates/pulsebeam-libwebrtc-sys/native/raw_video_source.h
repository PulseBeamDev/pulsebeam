#pragma once

#include <atomic>
#include <cstdint>
#include <limits>
#include <mutex>
#include <numeric>
#include <optional>
#include <utility>

#include "api/video/video_frame.h"
#include "api/video/video_sink_interface.h"
#include "api/video/video_source_interface.h"
#include "media/base/video_broadcaster.h"
#include "pc/video_track_source.h"
#include "rtc_base/synchronization/mutex.h"

namespace pulsebeam::webrtc_sys {

// The SDK aggregates signed int alignments with std::lcm. Every nonzero
// alignment's absolute value and their LCM must fit int, including pairs that
// its current active/requested-resolution filter ignores. This also protects
// every later subset left by independent sink removal. Zero does not contribute
// to that nonzero LCM: it cannot be relied on to mask another pair's overflow.
class SinkAlignmentBound {
 public:
  bool Include(int alignment) {
    const auto signed_value = static_cast<std::int64_t>(alignment);
    const auto value = static_cast<std::uint64_t>(
        signed_value < 0 ? -signed_value : signed_value);
    constexpr auto max = static_cast<std::uint64_t>(std::numeric_limits<int>::max());
    if (value > max) return false;
    if (value == 0) return true;
    const auto factor = value / std::gcd(bound_, value);
    if (bound_ > max / factor) return false;
    bound_ *= factor;
    return true;
  }

 private:
  std::uint64_t bound_ = 1;
};

// Owns the real SDK broadcaster and its real pair list, not a last-request
// registry. All registration mutations enter this override, including later
// SDK encoder updates. OnFrame retains the SDK's own callback/removal barrier.
class AdmissionVideoBroadcaster final : public webrtc::VideoBroadcaster {
 public:
  using Sink = webrtc::VideoSinkInterface<webrtc::VideoFrame>;

  void AddOrUpdateSink(Sink* sink, const webrtc::VideoSinkWants& wants) override {
    std::lock_guard lock(mutations_);
    if (!AdmitAndApply(sink, wants, [&] {
          webrtc::VideoBroadcaster::AddOrUpdateSink(sink, wants);
        })) {
      // An opaque void SDK update may already have changed its track-local
      // state. Keeping the old pair could retain stale black_frames/geometry
      // expectations. Fail closed via real SDK retirement, never normalized
      // wants or a synthetic discarded-frame callback. App preflight below
      // rejects before entering the SDK and does not retire a prior pair.
      RemoveSink(sink);
    }
  }

  void RemoveSink(Sink* sink) override {
    std::lock_guard lock(mutations_);
    // A refused new registration, or a fail-closed SDK update, has no pair.
    // Native VideoSourceBase::RemoveSink DCHECKs on missing pairs. The real
    // SDK mutex acquired by HasSink still provides the callback barrier for
    // this absent case; present pairs use the native removal implementation.
    if (HasSink(sink)) webrtc::VideoBroadcaster::RemoveSink(sink);
  }

  // App track attachment/update must execute this entire scope on its existing
  // worker, where the SDK track proxy calls inline. Never hold this scope on
  // signaling across set_enabled's synchronous signaling->worker call.
  // Recursive acquisition is only for that same-worker track->source delegate.
  template <typename Apply>
  bool AdmitAndApply(Sink* sink, const webrtc::VideoSinkWants& wants,
                     Apply&& apply) {
    std::lock_guard lock(mutations_);
    if (!Admits(sink, wants)) {
      Rejected();
      return false;
    }
    std::forward<Apply>(apply)();
    return true;
  }

  std::optional<webrtc::VideoSinkWants> WantsForSink(Sink* sink) const {
    webrtc::MutexLock lock(&sinks_and_wants_lock_);
    for (const auto& pair : sink_pairs()) {
      if (pair.sink == sink) return pair.wants;
    }
    return std::nullopt;
  }

  // Explicit adapter diagnostic for refused requests, including preflight
  // refusals when that helper is used. Not an SDK error code, source statistic,
  // discarded-frame callback or applied flag.
  std::uint64_t RejectedRequests() const {
    return rejected_.load(std::memory_order_relaxed);
  }

 private:
  bool HasSink(Sink* sink) const {
    webrtc::MutexLock lock(&sinks_and_wants_lock_);
    for (const auto& pair : sink_pairs()) {
      if (pair.sink == sink) return true;
    }
    return false;
  }

  bool Admits(Sink* sink, const webrtc::VideoSinkWants& wants) const {
    // Release the SDK mutex before forwarding: its Add/Remove acquire it too.
    // mutations_ keeps this snapshot and the following native mutation atomic
    // against other mutations, without changing SDK callback dispatch.
    webrtc::MutexLock lock(&sinks_and_wants_lock_);
    SinkAlignmentBound bound;
    for (const auto& pair : sink_pairs()) {
      if (pair.sink != sink && !bound.Include(pair.wants.resolution_alignment)) {
        return false;
      }
    }
    return bound.Include(wants.resolution_alignment);
  }

  void Rejected() {
    auto value = rejected_.load(std::memory_order_relaxed);
    while (value != std::numeric_limits<std::uint64_t>::max() &&
           !rejected_.compare_exchange_weak(value, value + 1,
                                            std::memory_order_relaxed)) {}
  }

  std::recursive_mutex mutations_;
  std::atomic<std::uint64_t> rejected_{0};
};

class PushVideoSource : public webrtc::VideoTrackSource {
 public:
  PushVideoSource() : VideoTrackSource(false) {}
  void Push(const webrtc::VideoFrame& frame) { broadcaster_.OnFrame(frame); }
  AdmissionVideoBroadcaster& broadcaster() { return broadcaster_; }
  const AdmissionVideoBroadcaster& broadcaster() const { return broadcaster_; }

 protected:
  webrtc::VideoSourceInterface<webrtc::VideoFrame>* source() override {
    return &broadcaster_;
  }

 private:
  AdmissionVideoBroadcaster broadcaster_;
};

}  // namespace pulsebeam::webrtc_sys
