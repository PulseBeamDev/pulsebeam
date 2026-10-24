#pragma once

#include <cstdint>
#include <memory>
#include <utility>

#include "api/frame_transformer_interface.h"
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
struct FrameTransformProvenance;
class NativeVideoFrameMetadata;
class NativeRtpVideoHeader;
struct FfiTransformableFrameTime;
struct FfiTransformableAudioFields;

// Closed producer graph: only actual stock SDK frames and SDK clones enter this
// owner. No external implementation can satisfy the SDK's private Passkey.
// Live delivery provenance retains the unregister-barrier slot. SDK unregister
// releases its callback on the SDK role; late frame destruction cannot move
// final callback/borrowed-queue release onto the frame owner's thread.
class NativeTransformableFrame final {
public:
  NativeTransformableFrame(
      std::unique_ptr<webrtc::TransformableFrameInterface> frame, bool audio,
      webrtc::VideoCodecType mime_codec = webrtc::kVideoCodecGeneric)
      : frame_(std::move(frame)), audio_(audio), mime_codec_(mime_codec) {}
  webrtc::TransformableFrameInterface &frame() noexcept { return *frame_; }
  const webrtc::TransformableFrameInterface &frame() const noexcept {
    return *frame_;
  }
  bool audio() const noexcept { return audio_; }
  const std::shared_ptr<FrameTransformProvenance>& provenance() const noexcept { return provenance_; }
  void set_provenance(std::shared_ptr<FrameTransformProvenance> provenance) noexcept { provenance_ = std::move(provenance); }
  webrtc::VideoCodecType mime_codec() const noexcept { return mime_codec_; }
  std::unique_ptr<webrtc::TransformableFrameInterface> Take() noexcept {
    return std::move(frame_);
  }

private:
  std::unique_ptr<webrtc::TransformableFrameInterface> frame_;
  bool audio_;
  std::shared_ptr<FrameTransformProvenance> provenance_;
  // The video MIME codec is immutable in the stock frame, unlike its header.
  // Capture it from the construction path, not from mutable metadata later.
  webrtc::VideoCodecType mime_codec_;
};

bool transformable_audio_completion_safe(const NativeTransformableFrame& value, bool sender_target) noexcept;
bool transformable_video_completion_safe(const NativeTransformableFrame& value, bool sender_target) noexcept;
bool transformable_frame_is_audio(const NativeTransformableFrame &) noexcept;
rust::Slice<const std::uint8_t>
transformable_frame_data(const NativeTransformableFrame &) noexcept;
void transformable_frame_set_data(NativeTransformableFrame &,
                                  rust::Slice<const std::uint8_t>) noexcept;
std::uint8_t
transformable_frame_payload_type(const NativeTransformableFrame &) noexcept;
bool transformable_frame_can_set_payload_type(
    const NativeTransformableFrame &) noexcept;
bool transformable_frame_set_payload_type(NativeTransformableFrame &,
                                          std::uint8_t) noexcept;
std::uint32_t
transformable_frame_ssrc(const NativeTransformableFrame &) noexcept;
std::uint32_t
transformable_frame_timestamp(const NativeTransformableFrame &) noexcept;
void transformable_frame_set_timestamp(NativeTransformableFrame &,
                                       std::uint32_t) noexcept;
std::int32_t
transformable_frame_direction(const NativeTransformableFrame &) noexcept;
rust::Vec<std::uint8_t>
transformable_frame_mime(const NativeTransformableFrame &,
                         std::int32_t &) noexcept;
FfiTransformableFrameTime
transformable_frame_time(const NativeTransformableFrame &,
                         std::uint8_t) noexcept;
bool transformable_frame_can_set_capture_time(
    const NativeTransformableFrame &) noexcept;
std::int32_t transformable_frame_set_capture_time(NativeTransformableFrame &,
                                                  bool, std::int64_t) noexcept;
std::unique_ptr<NativeTransformableFrame>
transformable_frame_clone(NativeTransformableFrame &, std::int32_t &) noexcept;
FfiTransformableAudioFields
transformable_audio_fields(const NativeTransformableFrame &) noexcept;
rust::Vec<std::uint32_t>
transformable_audio_csrcs(const NativeTransformableFrame &) noexcept;
bool transformable_audio_can_set_level(
    const NativeTransformableFrame &) noexcept;
bool transformable_audio_set_level(NativeTransformableFrame &, bool,
                                   std::uint8_t) noexcept;
bool transformable_video_is_key(const NativeTransformableFrame &) noexcept;
rust::Vec<std::uint8_t>
transformable_video_rid(const NativeTransformableFrame &, bool &) noexcept;
std::unique_ptr<NativeVideoFrameMetadata>
transformable_video_metadata(const NativeTransformableFrame &,
                             std::int32_t &) noexcept;
std::unique_ptr<NativeRtpVideoHeader>
transformable_video_header(const NativeTransformableFrame &) noexcept;
std::int32_t
transformable_video_set_metadata(NativeTransformableFrame &,
                                 const NativeVideoFrameMetadata &) noexcept;
} // namespace pulsebeam::webrtc_sys
