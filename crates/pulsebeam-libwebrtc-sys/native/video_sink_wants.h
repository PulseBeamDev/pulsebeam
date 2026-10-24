#pragma once

#include <optional>
#include <utility>
#include <vector>

#include "api/video/video_source_interface.h"
#include "media/base/video_broadcaster.h"
#include "pulsebeam-webrtc-sys/native/raw_video_source.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "rtc_base/synchronization/mutex.h"

namespace pulsebeam::webrtc_sys {

inline webrtc::VideoSinkWants ToVideoSinkWants(const FfiVideoSinkWants& v) {
  webrtc::VideoSinkWants result;
  result.rotation_applied = v.rotation_applied;
  result.black_frames = v.black_frames;
  result.max_pixel_count = v.max_pixel_count;
  if (v.has_target_pixel_count) result.target_pixel_count = v.target_pixel_count;
  result.max_framerate_fps = v.max_framerate_fps;
  result.resolution_alignment = v.resolution_alignment;
  result.resolutions.reserve(v.resolutions.size());
  for (const auto& size : v.resolutions) result.resolutions.emplace_back(size.width, size.height);
  if (v.has_requested_resolution) result.requested_resolution.emplace(v.requested_width, v.requested_height);
  result.is_active = v.is_active;
  if (v.has_aggregates) {
    result.aggregates.emplace();
    result.aggregates->any_active_without_requested_resolution = v.any_active_without_requested_resolution;
  }
  return result;
}

inline FfiVideoSinkWants FromVideoSinkWants(const webrtc::VideoSinkWants& v) {
  FfiVideoSinkWants result{};
  result.valid = true;
  result.rotation_applied = v.rotation_applied;
  result.black_frames = v.black_frames;
  result.max_pixel_count = v.max_pixel_count;
  result.has_target_pixel_count = v.target_pixel_count.has_value();
  result.target_pixel_count = v.target_pixel_count.value_or(0);
  result.max_framerate_fps = v.max_framerate_fps;
  result.resolution_alignment = v.resolution_alignment;
  for (const auto& size : v.resolutions) result.resolutions.push_back({size.width, size.height});
  result.has_requested_resolution = v.requested_resolution.has_value();
  if (v.requested_resolution) {
    result.requested_width = v.requested_resolution->width;
    result.requested_height = v.requested_resolution->height;
  }
  result.is_active = v.is_active;
  result.has_aggregates = v.aggregates.has_value();
  result.any_active_without_requested_resolution = v.aggregates && v.aggregates->any_active_without_requested_resolution;
  return result;
}

// Legal protected-member observation of the ACTUAL SDK base object. The member
// pointers name base-class members, not a downcast to an imaginary subclass.
// Remote RTP sources have no internal encoder registrations in the closed safe
// Rust graph. Their raw mutations (including enabled-state replay) execute on
// the same worker; caller preflight and application must share that worker scope.
class RemoteBroadcasterObservation : private webrtc::VideoBroadcaster {
  template <typename Reader>
  static auto Read(webrtc::VideoBroadcaster& actual, Reader&& reader) {
    using Pairs = const std::vector<SinkPair>& (webrtc::VideoSourceBase::*)() const;
    Pairs pairs = &RemoteBroadcasterObservation::sink_pairs;
    webrtc::Mutex webrtc::VideoBroadcaster::* mutex = &RemoteBroadcasterObservation::sinks_and_wants_lock_;
    webrtc::MutexLock lock(&(actual.*mutex));
    return std::forward<Reader>(reader)((actual.*pairs)());
  }

 public:
  using Sink = webrtc::VideoSinkInterface<webrtc::VideoFrame>;
  static bool Admits(webrtc::VideoBroadcaster& actual, Sink* sink,
                     const webrtc::VideoSinkWants& wants) {
    return Read(actual, [&](const auto& pairs) {
      SinkAlignmentBound bound;
      for (const auto& pair : pairs) {
        if (pair.sink != sink && !bound.Include(pair.wants.resolution_alignment)) return false;
      }
      return bound.Include(wants.resolution_alignment);
    });
  }
  static std::optional<webrtc::VideoSinkWants> WantsForSink(
      webrtc::VideoBroadcaster& actual, Sink* sink) {
    return Read(actual, [&](const auto& pairs) -> std::optional<webrtc::VideoSinkWants> {
      for (const auto& pair : pairs) if (pair.sink == sink) return pair.wants;
      return std::nullopt;
    });
  }

};

}  // namespace pulsebeam::webrtc_sys
