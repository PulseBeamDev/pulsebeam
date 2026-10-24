#pragma once

#include "api/media_stream_interface.h"
#include "api/video_track_source_constraints.h"
#include "rtc_base/thread.h"

namespace pulsebeam::webrtc_sys {

// Use the source's existing owner worker. In controlled execution it is the
// caller-owned role, not a new thread or a hidden pump. These SDK methods are
// void requests: dispatch does not promise a generated frame or applied limits.
inline void RequestVideoSourceRefresh(webrtc::VideoTrackSourceInterface& source,
                                      webrtc::Thread& worker) {
  worker.BlockingCall([&] { source.RequestRefreshFrame(); });
}

inline void GenerateVideoSourceKeyFrame(webrtc::VideoTrackSourceInterface& source,
                                       webrtc::Thread& worker) {
  worker.BlockingCall([&] { source.GenerateKeyFrame(); });
}

inline void ProcessVideoSourceConstraints(
    webrtc::VideoTrackSourceInterface& source, webrtc::Thread& worker,
    const webrtc::VideoTrackSourceConstraints& constraints) {
  worker.BlockingCall([&] { source.ProcessConstraints(constraints); });
}

}  // namespace pulsebeam::webrtc_sys
