#pragma once
#include <cstddef>
#include <cstdint>
#include <map>
#include <memory>
#include <utility>
#include "api/video/encoded_image.h"
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
struct FfiEncodedImageMetadata;
struct FfiEncodedBufferRead;
class NativeEncodedImage final {
 public:
  NativeEncodedImage() = default;
  explicit NativeEncodedImage(webrtc::EncodedImage value) : value_(std::move(value)) {}
  const webrtc::EncodedImage& value() const { return value_; }
  webrtc::EncodedImage& value() { return value_; }
 private:
  webrtc::EncodedImage value_;
};
class NativeEncodedVideoBuffer final {
 public:
  explicit NativeEncodedVideoBuffer(webrtc::scoped_refptr<webrtc::EncodedImageBufferInterface> value) : value_(std::move(value)) {}
  const webrtc::scoped_refptr<webrtc::EncodedImageBufferInterface>& value() const { return value_; }
 private:
  webrtc::scoped_refptr<webrtc::EncodedImageBufferInterface> value_;
};
// Exclusive until Freeze; SDK Realloc can expose uninitialized bytes. Ranges
// track actual writes, never inventing bytes or permitting uninitialized reads.
class NativeWritableEncodedBuffer final {
 public:
  explicit NativeWritableEncodedBuffer(std::size_t size);
  bool Initialized() const;
  void Resize(std::size_t size);
  bool Write(std::size_t offset, rust::Slice<const std::uint8_t> data);
  const webrtc::EncodedImageBuffer& value() const { return *value_; }
  webrtc::scoped_refptr<webrtc::EncodedImageBufferInterface> Freeze() { return std::move(value_); }
 private:
  webrtc::scoped_refptr<webrtc::EncodedImageBuffer> value_;
  std::map<std::size_t, std::size_t> initialized_;
};
std::unique_ptr<NativeEncodedImage> encoded_image_new() noexcept;
std::unique_ptr<NativeEncodedImage> encoded_image_clone(const NativeEncodedImage&) noexcept;
void encoded_image_assign(NativeEncodedImage&, const NativeEncodedImage&) noexcept;
FfiEncodedImageMetadata encoded_image_metadata(const NativeEncodedImage&) noexcept;
bool encoded_image_set_metadata(NativeEncodedImage&, const FfiEncodedImageMetadata&) noexcept;
bool encoded_image_capture_time(const NativeEncodedImage&, std::int64_t&) noexcept;
void encoded_image_set_encode_time(NativeEncodedImage&, std::int64_t, std::int64_t) noexcept;
bool encoded_image_is_key(const NativeEncodedImage&) noexcept;
bool encoded_image_is_delta(const NativeEncodedImage&) noexcept;
std::size_t encoded_image_size(const NativeEncodedImage&) noexcept;
bool encoded_image_set_size(NativeEncodedImage&, std::size_t) noexcept;
rust::Vec<std::uint8_t> encoded_image_data(const NativeEncodedImage&) noexcept;
std::unique_ptr<NativeEncodedVideoBuffer> encoded_image_buffer(const NativeEncodedImage&) noexcept;
void encoded_image_set_buffer(NativeEncodedImage&, const NativeEncodedVideoBuffer&) noexcept;
void encoded_image_clear_buffer(NativeEncodedImage&) noexcept;
bool encoded_image_layer_size(const NativeEncodedImage&, std::int32_t, std::size_t&, bool&) noexcept;
bool encoded_image_set_layer_size(NativeEncodedImage&, std::int32_t, std::size_t) noexcept;
bool encoded_image_frame_type_name(std::int32_t, rust::String&) noexcept;
std::unique_ptr<NativeEncodedVideoBuffer> encoded_buffer_empty() noexcept;
std::unique_ptr<NativeEncodedVideoBuffer> encoded_buffer_new(rust::Slice<const std::uint8_t>) noexcept;
std::unique_ptr<NativeEncodedVideoBuffer> encoded_buffer_new_owned(rust::Vec<std::uint8_t>) noexcept;
std::unique_ptr<NativeEncodedVideoBuffer> encoded_buffer_clone(const NativeEncodedVideoBuffer&) noexcept;
std::size_t encoded_buffer_size(const NativeEncodedVideoBuffer&) noexcept;
rust::Vec<std::uint8_t> encoded_buffer_data(const NativeEncodedVideoBuffer&) noexcept;
std::unique_ptr<NativeWritableEncodedBuffer> encoded_writer_new(std::size_t) noexcept;
std::size_t encoded_writer_size(const NativeWritableEncodedBuffer&) noexcept;
void encoded_writer_resize(NativeWritableEncodedBuffer&, std::size_t) noexcept;
bool encoded_writer_write(NativeWritableEncodedBuffer&, std::size_t, rust::Slice<const std::uint8_t>) noexcept;
FfiEncodedBufferRead encoded_writer_read(const NativeWritableEncodedBuffer&) noexcept;
std::unique_ptr<NativeEncodedVideoBuffer> encoded_writer_freeze(std::unique_ptr<NativeWritableEncodedBuffer>) noexcept;
}  // namespace pulsebeam::webrtc_sys
