#pragma once

#include <memory>
#include "api/video/video_bitrate_allocator_factory.h"
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
struct RustVideoBitrateAllocatorFactory;
std::unique_ptr<webrtc::VideoBitrateAllocatorFactory>
video_bitrate_factory_adapter(
    rust::Box<RustVideoBitrateAllocatorFactory> source) noexcept;
}  // namespace pulsebeam::webrtc_sys
