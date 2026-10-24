#pragma once

#include "api/video/corruption_detection/frame_instrumentation_data.h"
#include "api/video/video_frame_metadata.h"
#include "modules/rtp_rtcp/source/rtp_video_header.h"
#include "rust/cxx.h"
#include <cstdint>
#include <memory>

namespace pulsebeam::webrtc_sys {

struct FfiRtpVideoHeaderValues;

class NativeRtpVideoHeader final {
public:
  NativeRtpVideoHeader() = default;
  explicit NativeRtpVideoHeader(const webrtc::RTPVideoHeader &source)
      : value_(source) {}
  const webrtc::RTPVideoHeader &value() const noexcept { return value_; }
  webrtc::RTPVideoHeader &value() noexcept { return value_; }

private:
  webrtc::RTPVideoHeader value_;
};

struct FfiVideoFrameMetadataValues;
struct FfiVideoHeaderCodecSpecifics;
struct FfiVideoHeaderVp8;
struct FfiVideoHeaderVp9;
struct FfiVideoHeaderH264;
struct FfiVp9GofInfo;
FfiVp9GofInfo CodecGofFields(const webrtc::GofInfoVP9&);
bool CodecGofFromFields(const FfiVp9GofInfo&, webrtc::GofInfoVP9&);

class NativeVideoFrameMetadata final {
public:
  NativeVideoFrameMetadata() = default;
  explicit NativeVideoFrameMetadata(const webrtc::VideoFrameMetadata &source)
      : value_(source) {}
  const webrtc::VideoFrameMetadata &value() const noexcept { return value_; }
  webrtc::VideoFrameMetadata &value() noexcept { return value_; }

private:
  webrtc::VideoFrameMetadata value_;
};

std::unique_ptr<NativeRtpVideoHeader> rtp_video_header_new() noexcept;
std::unique_ptr<NativeRtpVideoHeader>
rtp_video_header_clone(const NativeRtpVideoHeader &) noexcept;
void rtp_video_header_assign(NativeRtpVideoHeader &,
                             const NativeRtpVideoHeader &) noexcept;
FfiRtpVideoHeaderValues
rtp_video_header_values(const NativeRtpVideoHeader &) noexcept;
std::int32_t rtp_video_header_set_values(NativeRtpVideoHeader &,
                                         FfiRtpVideoHeaderValues) noexcept;
std::unique_ptr<NativeVideoFrameMetadata>
rtp_video_header_as_metadata(const NativeRtpVideoHeader &,
                             std::int32_t &) noexcept;
std::int32_t
rtp_video_header_set_metadata(NativeRtpVideoHeader &,
                              const NativeVideoFrameMetadata &) noexcept;
std::unique_ptr<NativeRtpVideoHeader>
rtp_video_header_from_metadata(const NativeVideoFrameMetadata &,
                               std::int32_t &) noexcept;

std::unique_ptr<NativeVideoFrameMetadata> video_frame_metadata_new() noexcept;
FfiVideoHeaderCodecSpecifics
video_frame_metadata_specifics(const NativeVideoFrameMetadata &) noexcept;
std::int32_t
video_frame_metadata_set_specifics(NativeVideoFrameMetadata &,
                                   FfiVideoHeaderCodecSpecifics) noexcept;
FfiVideoHeaderVp8 video_header_vp8_defaults() noexcept;
FfiVideoHeaderVp9 video_header_vp9_defaults() noexcept;
FfiVideoHeaderH264 video_header_h264_defaults() noexcept;

std::int32_t
video_frame_metadata_equals(const NativeVideoFrameMetadata &,
                            const NativeVideoFrameMetadata &) noexcept;
void video_header_vp8_init(FfiVideoHeaderVp8 &) noexcept;
std::int32_t video_header_vp9_init(FfiVideoHeaderVp9 &) noexcept;
std::int32_t video_header_vp8_equals(FfiVideoHeaderVp8,
                                     FfiVideoHeaderVp8) noexcept;
std::int32_t video_header_vp9_equals(FfiVideoHeaderVp9,
                                     FfiVideoHeaderVp9) noexcept;
std::int32_t video_header_h264_equals(FfiVideoHeaderH264,
                                      FfiVideoHeaderH264) noexcept;
std::int32_t vp9_gof_set_structure(FfiVp9GofInfo &, std::int32_t) noexcept;
std::int32_t vp9_gof_copy_prefix(FfiVp9GofInfo &, FfiVp9GofInfo) noexcept;
std::int32_t vp9_gof_equals(FfiVp9GofInfo, FfiVp9GofInfo) noexcept;

std::unique_ptr<NativeVideoFrameMetadata>
video_frame_metadata_clone(const NativeVideoFrameMetadata &) noexcept;
void video_frame_metadata_assign(NativeVideoFrameMetadata &,
                                 const NativeVideoFrameMetadata &) noexcept;
FfiVideoFrameMetadataValues
video_frame_metadata_values(const NativeVideoFrameMetadata &) noexcept;
// Reject only C++ non-fixed enum representations before any mutation.
std::int32_t
video_frame_metadata_set_values(NativeVideoFrameMetadata &,
                                FfiVideoFrameMetadataValues) noexcept;

// An actual owning SDK value. No host pointers, callbacks, or execution role.
class NativeFrameInstrumentationData final
    : public webrtc::FrameInstrumentationData {
public:
  NativeFrameInstrumentationData() = default;
  explicit NativeFrameInstrumentationData(
      const webrtc::FrameInstrumentationData &source)
      : webrtc::FrameInstrumentationData(source) {}
  const webrtc::FrameInstrumentationData &value() const noexcept {
    return *this;
  }
  webrtc::FrameInstrumentationData &value() noexcept { return *this; }
};

std::unique_ptr<NativeFrameInstrumentationData>
frame_instrumentation_new() noexcept;
std::unique_ptr<NativeFrameInstrumentationData>
frame_instrumentation_clone(const NativeFrameInstrumentationData &) noexcept;
void frame_instrumentation_assign(
    NativeFrameInstrumentationData &,
    const NativeFrameInstrumentationData &) noexcept;
rust::Vec<double>
frame_instrumentation_samples(const NativeFrameInstrumentationData &) noexcept;
bool frame_instrumentation_set_samples(NativeFrameInstrumentationData &,
                                       rust::Slice<const double>) noexcept;
bool frame_instrumentation_set_samples_owned(NativeFrameInstrumentationData &,
                                             rust::Vec<double>) noexcept;
} // namespace pulsebeam::webrtc_sys
