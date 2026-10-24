#pragma once

#include <cstdint>
#include <memory>
#include <utility>
#include "api/scoped_refptr.h"
#include "api/video/i420_buffer.h"
#include "api/video/video_frame.h"
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
struct FfiRawFrameMetadata;
struct FfiRawBufferInfo;
struct FfiRawVideoPlane;
struct FfiRawUpdateRect;
class NativeVideoFrame;

class NativeRawVideoFrame final {
 public:
  explicit NativeRawVideoFrame(webrtc::VideoFrame value) : value_(std::move(value)) {}
  const webrtc::VideoFrame& value() const { return value_; }
  webrtc::VideoFrame& value() { return value_; }
 private:
  webrtc::VideoFrame value_;
};
class NativeRawVideoBuffer final {
 public:
  explicit NativeRawVideoBuffer(webrtc::scoped_refptr<webrtc::VideoFrameBuffer> value)
      : value_(std::move(value)) {}
  const webrtc::scoped_refptr<webrtc::VideoFrameBuffer>& value() const { return value_; }
 private:
  webrtc::scoped_refptr<webrtc::VideoFrameBuffer> value_;
};
// Not cloneable or publishable while mutable. The only transition to shared
// SDK frame/buffer ownership consumes this writer and removes its native ref.
class NativeWritableI420 final {
 public:
  explicit NativeWritableI420(webrtc::scoped_refptr<webrtc::I420Buffer> value, bool initialized = false)
      : value_(std::move(value)), initialized_{initialized, initialized, initialized} {}
  webrtc::I420Buffer& value() const { return *value_; }
  bool Initialized() const { return initialized_[0] && initialized_[1] && initialized_[2]; }
  bool PlaneInitialized(std::uint8_t plane) const { return plane < 3 && initialized_[plane]; }
  void MarkPlane(std::uint8_t plane) { initialized_[plane] = true; }
  void MarkAll() { for (auto& plane : initialized_) plane = true; }
  webrtc::scoped_refptr<webrtc::I420Buffer> Freeze() { return std::move(value_); }
 private:
  webrtc::scoped_refptr<webrtc::I420Buffer> value_;
  bool initialized_[3];
};
class NativeVideoBufferPreparation final {
 public:
  struct State;
  explicit NativeVideoBufferPreparation(std::shared_ptr<State> state) : state_(std::move(state)) {}
  const std::shared_ptr<State>& state() const { return state_; }
 private:
  std::shared_ptr<State> state_;
};

rust::String raw_buffer_type_name(std::uint8_t) noexcept;
std::unique_ptr<NativeRawVideoFrame> raw_frame_from_render_time(const NativeRawVideoBuffer&, std::uint32_t, std::int64_t, std::uint16_t) noexcept;
std::size_t raw_buffer_queue_bytes(const webrtc::VideoFrameBuffer&, std::size_t limit) noexcept;
std::unique_ptr<NativeRawVideoFrame> raw_frame_clone(const NativeRawVideoFrame&) noexcept;
std::unique_ptr<NativeRawVideoFrame> raw_frame_from_buffer(const NativeRawVideoBuffer&) noexcept;
FfiRawFrameMetadata raw_frame_metadata(const NativeRawVideoFrame&) noexcept;
bool raw_frame_set_metadata(NativeRawVideoFrame&, const FfiRawFrameMetadata&) noexcept;
std::unique_ptr<NativeRawVideoBuffer> raw_frame_buffer(const NativeRawVideoFrame&) noexcept;
bool raw_frame_set_buffer(NativeRawVideoFrame&, const NativeRawVideoBuffer&) noexcept;
std::int64_t raw_frame_timestamp_us(const NativeRawVideoFrame&) noexcept;
std::int32_t raw_frame_width(const NativeRawVideoFrame&) noexcept;
std::int32_t raw_frame_height(const NativeRawVideoFrame&) noexcept;
bool raw_frame_is_texture(const NativeRawVideoFrame&) noexcept;
FfiRawUpdateRect raw_frame_update_rect(const NativeRawVideoFrame&) noexcept;
std::uint32_t raw_frame_size(const NativeRawVideoFrame&) noexcept;
std::int64_t raw_frame_render_time_ms(const NativeRawVideoFrame&) noexcept;
std::unique_ptr<NativeVideoFrame> raw_frame_legacy(const NativeRawVideoFrame&) noexcept;
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_clone(const NativeRawVideoBuffer&) noexcept;
FfiRawBufferInfo raw_buffer_info(const NativeRawVideoBuffer&) noexcept;
rust::Vec<std::uint8_t> raw_buffer_storage(const NativeRawVideoBuffer&) noexcept;
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_to_i420(const NativeRawVideoBuffer&) noexcept;
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_get_i420(const NativeRawVideoBuffer&) noexcept;
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_crop_scale(const NativeRawVideoBuffer&, std::int32_t, std::int32_t, std::int32_t, std::int32_t, std::int32_t, std::int32_t) noexcept;
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_mapped(const NativeRawVideoBuffer&, rust::Slice<const std::uint8_t>) noexcept;
std::unique_ptr<NativeVideoBufferPreparation> raw_buffer_prepare(const NativeRawVideoBuffer&, std::size_t, std::size_t, std::size_t) noexcept;
bool raw_buffer_prepared(const NativeVideoBufferPreparation&, std::size_t&) noexcept;
void raw_buffer_preparation_close(const NativeVideoBufferPreparation&) noexcept;
FfiRawVideoPlane raw_buffer_plane(const NativeRawVideoBuffer&, std::uint8_t) noexcept;
std::unique_ptr<NativeWritableI420> raw_i420_create(std::int32_t, std::int32_t, std::int32_t, std::int32_t, std::int32_t) noexcept;
std::unique_ptr<NativeWritableI420> raw_i420_copy(const NativeRawVideoBuffer&) noexcept;
std::unique_ptr<NativeWritableI420> raw_i420_rotate(const NativeRawVideoBuffer&, std::uint16_t) noexcept;
std::unique_ptr<NativeRawVideoBuffer> raw_i420_freeze(std::unique_ptr<NativeWritableI420>) noexcept;
FfiRawBufferInfo raw_i420_info(const NativeWritableI420&) noexcept;
bool raw_i420_initialized(const NativeWritableI420&) noexcept;
FfiRawVideoPlane raw_i420_plane(const NativeWritableI420&, std::uint8_t) noexcept;
bool raw_i420_write_plane(NativeWritableI420&, std::uint8_t, rust::Slice<const std::uint8_t>, std::int32_t) noexcept;
void raw_i420_initialize(NativeWritableI420&, bool) noexcept;
bool raw_i420_crop_from(NativeWritableI420&, const NativeRawVideoBuffer&, std::int32_t, std::int32_t, std::int32_t, std::int32_t) noexcept;
bool raw_i420_scale_from(NativeWritableI420&, const NativeRawVideoBuffer&, bool) noexcept;
FfiRawUpdateRect raw_update_rect_operation(FfiRawUpdateRect, FfiRawUpdateRect, std::uint8_t, rust::Slice<const std::int32_t>, bool&) noexcept;

}  // namespace pulsebeam::webrtc_sys
