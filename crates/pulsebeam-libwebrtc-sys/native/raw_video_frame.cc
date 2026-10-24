#include "pulsebeam-webrtc-sys/native/raw_video_frame.h"
#include "pulsebeam-webrtc-sys/native/codec.h"
#include "pulsebeam-webrtc-sys/native/media_values.h"
#include "pulsebeam-webrtc-sys/native/video.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

#include <algorithm>
#include <cstring>
#include <limits>
#include <mutex>
#include <optional>
#include <string>
#include <vector>
#include "api/make_ref_counted.h"
#include "api/units/timestamp.h"

namespace pulsebeam::webrtc_sys {
namespace {
webrtc::Timestamp TimestampFromBits(std::int64_t value) {
  if (value == INT64_MAX) return webrtc::Timestamp::PlusInfinity();
  if (value == INT64_MIN) return webrtc::Timestamp::MinusInfinity();
  return webrtc::Timestamp::Micros(value);
}
bool RotationValid(std::uint16_t value) { return value == 0 || value == 90 || value == 180 || value == 270; }
bool AllocationSafe(int width, int height, int y, int u, int v) {
  if (width <= 0 || height <= 0 || width == INT32_MAX || y < width ||
      u < (width + 1) / 2 || v < (width + 1) / 2) return false;
  const std::int64_t bytes = static_cast<std::int64_t>(y) * height +
      (static_cast<std::int64_t>(u) + v) * ((static_cast<std::int64_t>(height) + 1) / 2);
  return bytes > 0 && bytes <= INT32_MAX;
}
bool GeometrySafe(int width, int height) {
  return width > 0 && height > 0 && video_source_i420_allocation_size(width, height) != 0;
}
bool CropSafe(int width, int height, int x, int y, int cw, int ch) {
  return x >= 0 && y >= 0 && cw > 0 && ch > 0 &&
      static_cast<std::int64_t>(x) + cw <= width && static_cast<std::int64_t>(y) + ch <= height;
}
bool RectSafe(const FfiRawUpdateRect& r, int width, int height) {
  return r.x >= 0 && r.y >= 0 && r.width >= 0 && r.height >= 0 &&
      static_cast<std::int64_t>(r.x) + r.width <= width &&
      static_cast<std::int64_t>(r.y) + r.height <= height;
}
webrtc::VideoFrame::UpdateRect Rect(const FfiRawUpdateRect& r) { return {r.x, r.y, r.width, r.height}; }
FfiRawUpdateRect Rect(const webrtc::VideoFrame::UpdateRect& r) { return {r.offset_x, r.offset_y, r.width, r.height}; }
const webrtc::PlanarYuvBuffer* Planar(const webrtc::VideoFrameBuffer& buffer) {
  using T = webrtc::VideoFrameBuffer::Type;
  switch (buffer.type()) {
    case T::kI420: case T::kI420A: return buffer.GetI420();
    case T::kI422: return buffer.GetI422();
    case T::kI444: return buffer.GetI444();
    case T::kI010: return buffer.GetI010();
    case T::kI210: return buffer.GetI210();
    case T::kI410: return buffer.GetI410();
    default: return nullptr;
  }
}
FfiRawBufferInfo BufferInfo(const webrtc::VideoFrameBuffer& b) {
  FfiRawBufferInfo result{};
  result.buffer_type = static_cast<std::uint8_t>(b.type());
  result.width = b.width(); result.height = b.height();
  if (const auto* p = Planar(b)) {
    result.chroma_width = p->ChromaWidth(); result.chroma_height = p->ChromaHeight();
    result.stride_y = p->StrideY(); result.stride_u = p->StrideU(); result.stride_v = p->StrideV();
    if (b.type() == webrtc::VideoFrameBuffer::Type::kI420A) result.stride_a = b.GetI420A()->StrideA();
  } else if (b.type() == webrtc::VideoFrameBuffer::Type::kNV12) {
    const auto* p = b.GetNV12();
    result.chroma_width = p->ChromaWidth(); result.chroma_height = p->ChromaHeight();
    result.stride_y = p->StrideY(); result.stride_uv = p->StrideUV();
  }
  return result;
}
template <typename Sample>
FfiRawVideoPlane CopyPlane(const Sample* data, int stride, int width, int height) {
  FfiRawVideoPlane result{};
  if (!data || stride < width || width <= 0 || height <= 0) return result;
  const auto size = static_cast<std::uint64_t>(width) * height;
  if (size > static_cast<std::uint64_t>(PTRDIFF_MAX) / sizeof(Sample) ||
      static_cast<std::uint64_t>(stride) * height > static_cast<std::uint64_t>(PTRDIFF_MAX) / sizeof(Sample)) return result;
  result.valid = true;
  result.width = width; result.height = height; result.stride = stride;
  result.bits = sizeof(Sample) * 8;
  if constexpr (sizeof(Sample) == 1) result.data8.reserve(size);
  else result.data16.reserve(size);
  for (int row = 0; row < height; ++row) {
    const auto* source = data + static_cast<std::ptrdiff_t>(row) * stride;
    for (int column = 0; column < width; ++column) {
      if constexpr (sizeof(Sample) == 1) result.data8.push_back(source[column]);
      else result.data16.push_back(source[column]);
    }
  }
  return result;
}
FfiRawVideoPlane BufferPlane(const webrtc::VideoFrameBuffer& b, std::uint8_t plane) {
  using T = webrtc::VideoFrameBuffer::Type;
  if (b.type() == T::kNV12) {
    const auto* p = b.GetNV12();
    if (plane == 0) return CopyPlane(p->DataY(), p->StrideY(), b.width(), b.height());
    if (plane == 4 && p->ChromaWidth() <= INT32_MAX / 2)
      return CopyPlane(p->DataUV(), p->StrideUV(), p->ChromaWidth() * 2, p->ChromaHeight());
    return {};
  }
  if (plane == 3 && b.type() == T::kI420A) {
    const auto* p = b.GetI420A();
    return CopyPlane(p->DataA(), p->StrideA(), b.width(), b.height());
  }
  if (plane > 2) return {};
  const auto* p = Planar(b);
  if (!p) return {};
  const auto width = plane == 0 ? b.width() : p->ChromaWidth();
  const auto height = plane == 0 ? b.height() : p->ChromaHeight();
  const auto stride = plane == 0 ? p->StrideY() : plane == 1 ? p->StrideU() : p->StrideV();
  if (b.type() == T::kI010 || b.type() == T::kI210 || b.type() == T::kI410) {
    const auto* pixels = static_cast<const webrtc::PlanarYuv16BBuffer*>(p);
    return CopyPlane(plane == 0 ? pixels->DataY() : plane == 1 ? pixels->DataU() : pixels->DataV(), stride, width, height);
  }
  const auto* pixels = static_cast<const webrtc::PlanarYuv8Buffer*>(p);
  return CopyPlane(plane == 0 ? pixels->DataY() : plane == 1 ? pixels->DataU() : pixels->DataV(), stride, width, height);
}
}  // namespace

rust::String raw_buffer_type_name(std::uint8_t type) noexcept {
  return type <= 8 ? webrtc::VideoFrameBufferTypeToString(static_cast<webrtc::VideoFrameBuffer::Type>(type)) : "";
}
std::unique_ptr<NativeRawVideoFrame> raw_frame_from_render_time(const NativeRawVideoBuffer& buffer,
    std::uint32_t rtp_timestamp, std::int64_t milliseconds, std::uint16_t rotation) noexcept {
  if (!RotationValid(rotation) || milliseconds > INT64_MAX / 1000 || milliseconds < INT64_MIN / 1000) return nullptr;
  return std::make_unique<NativeRawVideoFrame>(webrtc::VideoFrame(buffer.value(), rtp_timestamp, milliseconds, static_cast<webrtc::VideoRotation>(rotation)));
}

std::size_t raw_buffer_queue_bytes(const webrtc::VideoFrameBuffer& b, std::size_t limit) noexcept {
  const auto info = BufferInfo(b);
  if (info.width <= 0 || info.height <= 0) return limit + 1;
  std::uint64_t total = 0;
  auto add = [&](std::int32_t stride, std::int32_t rows, std::uint32_t bytes = 1) {
    if (stride < 0 || rows < 0) return false;
    const auto size = static_cast<std::uint64_t>(stride) * rows * bytes;
    if (size > limit - total) return false;
    total += size; return true;
  };
  using T = webrtc::VideoFrameBuffer::Type;
  if (b.type() == T::kNative) {
    // The SDK exposes no allocation size for opaque storage. Bound retained
    // frame count and packed I420-equivalent geometry, not GPU allocation.
    total = static_cast<std::uint64_t>(info.width) * info.height +
        2 * ((static_cast<std::uint64_t>(info.width) + 1) / 2) * ((static_cast<std::uint64_t>(info.height) + 1) / 2);
    return total <= limit ? total : limit + 1;
  }
  const auto bytes = (b.type() == T::kI010 || b.type() == T::kI210 || b.type() == T::kI410) ? 2 : 1;
  if (!add(info.stride_y, info.height, bytes)) return limit + 1;
  if (b.type() == T::kNV12) {
    if (!add(info.stride_uv, info.chroma_height)) return limit + 1;
  } else {
    if (!add(info.stride_u, info.chroma_height, bytes) ||
        !add(info.stride_v, info.chroma_height, bytes)) return limit + 1;
    if (b.type() == T::kI420A && b.GetI420A()->DataA() && !add(info.stride_a, info.height)) return limit + 1;
  }
  return total;
}

std::unique_ptr<NativeRawVideoFrame> raw_frame_clone(const NativeRawVideoFrame& frame) noexcept {
  return std::make_unique<NativeRawVideoFrame>(frame.value());
}
std::unique_ptr<NativeRawVideoFrame> raw_frame_from_buffer(const NativeRawVideoBuffer& buffer) noexcept {
  return std::make_unique<NativeRawVideoFrame>(webrtc::VideoFrame::Builder().set_video_frame_buffer(buffer.value()).build());
}
FfiRawFrameMetadata raw_frame_metadata(const NativeRawVideoFrame& frame) noexcept {
  const auto& f = frame.value();
  FfiRawFrameMetadata r{};
  r.id = f.id(); r.timestamp_us = f.timestamp_us(); r.rtp_timestamp = f.rtp_timestamp(); r.ntp_time_ms = f.ntp_time_ms();
  r.has_presentation_timestamp = f.presentation_timestamp().has_value();
  if (f.presentation_timestamp()) r.presentation_timestamp_us = MediaTimestampBits(*f.presentation_timestamp());
  r.has_reference_time = f.reference_time().has_value();
  if (f.reference_time()) r.reference_time_us = MediaTimestampBits(*f.reference_time());
  r.rotation = static_cast<std::uint16_t>(f.rotation());
  r.has_color_space = f.color_space().has_value();
  if (f.color_space()) r.color_space = MediaColorToFfi(*f.color_space());
  const auto render = f.render_parameters();
  r.low_latency = render.use_low_latency_rendering;
  r.has_max_composition_delay = render.max_composition_delay_in_frames.has_value();
  r.max_composition_delay = render.max_composition_delay_in_frames.value_or(0);
  r.has_update_rect = f.has_update_rect(); r.update_rect = Rect(f.update_rect());
  r.packet_infos = MediaPacketInfosToFfi(f.packet_infos());
  const auto processing = f.processing_time();
  r.has_processing_time = processing.has_value();
  if (processing) { r.processing_start_us = MediaTimestampBits(processing->start); r.processing_finish_us = MediaTimestampBits(processing->finish); }
  r.repeat_frame = f.is_repeat_frame();
  r.has_content_type = f.content_type().has_value();
  r.content_type = static_cast<std::uint8_t>(f.content_type().value_or(webrtc::VideoContentType::UNSPECIFIED));
  return r;
}
bool raw_frame_set_metadata(NativeRawVideoFrame& frame, const FfiRawFrameMetadata& r) noexcept {
  if (!RotationValid(r.rotation) || (r.has_update_rect && !RectSafe(r.update_rect, frame.value().width(), frame.value().height()))) return false;
  webrtc::VideoFrame::Builder builder;
  builder.set_video_frame_buffer(frame.value().video_frame_buffer())
      .set_id(r.id).set_timestamp_us(r.timestamp_us).set_rtp_timestamp(r.rtp_timestamp).set_ntp_time_ms(r.ntp_time_ms)
      .set_rotation(static_cast<webrtc::VideoRotation>(r.rotation))
      .set_is_repeat_frame(r.repeat_frame)
      .set_packet_infos(MediaPacketInfosFromFfi({r.packet_infos.data(), r.packet_infos.size()}));
  if (r.has_presentation_timestamp) builder.set_presentation_timestamp(TimestampFromBits(r.presentation_timestamp_us));
  if (r.has_reference_time) builder.set_reference_time(TimestampFromBits(r.reference_time_us));
  if (r.has_color_space) builder.set_color_space(MediaColorFromFfi(r.color_space));
  if (r.has_update_rect) builder.set_update_rect(Rect(r.update_rect));
  if (r.has_content_type) builder.set_content_type(static_cast<webrtc::VideoContentType>(r.content_type));
  auto replacement = builder.build();
  webrtc::VideoFrame::RenderParameters render;
  render.use_low_latency_rendering = r.low_latency;
  if (r.has_max_composition_delay) render.max_composition_delay_in_frames = r.max_composition_delay;
  replacement.set_render_parameters(render);
  if (r.has_processing_time) replacement.set_processing_time({TimestampFromBits(r.processing_start_us), TimestampFromBits(r.processing_finish_us)});
  frame.value() = std::move(replacement);
  return true;
}
std::unique_ptr<NativeRawVideoBuffer> raw_frame_buffer(const NativeRawVideoFrame& frame) noexcept {
  return std::make_unique<NativeRawVideoBuffer>(frame.value().video_frame_buffer());
}
bool raw_frame_set_buffer(NativeRawVideoFrame& frame, const NativeRawVideoBuffer& buffer) noexcept {
  if (!GeometrySafe(buffer.value()->width(), buffer.value()->height())) return false;
  if (frame.value().has_update_rect() && !RectSafe(Rect(frame.value().update_rect()), buffer.value()->width(), buffer.value()->height())) return false;
  frame.value().set_video_frame_buffer(buffer.value());
  return true;
}
std::int64_t raw_frame_timestamp_us(const NativeRawVideoFrame& frame) noexcept { return frame.value().timestamp_us(); }
std::int32_t raw_frame_width(const NativeRawVideoFrame& frame) noexcept { return frame.value().width(); }
std::int32_t raw_frame_height(const NativeRawVideoFrame& frame) noexcept { return frame.value().height(); }
bool raw_frame_is_texture(const NativeRawVideoFrame& frame) noexcept { return frame.value().is_texture(); }
FfiRawUpdateRect raw_frame_update_rect(const NativeRawVideoFrame& frame) noexcept { return Rect(frame.value().update_rect()); }
std::uint32_t raw_frame_size(const NativeRawVideoFrame& frame) noexcept { return frame.value().size(); }
std::int64_t raw_frame_render_time_ms(const NativeRawVideoFrame& frame) noexcept { return frame.value().render_time_ms(); }
std::unique_ptr<NativeVideoFrame> raw_frame_legacy(const NativeRawVideoFrame& frame) noexcept {
  auto buffer = frame.value().video_frame_buffer()->ToI420();
  if (!buffer) return nullptr;
  auto snapshot = frame.value(); snapshot.set_video_frame_buffer(buffer);
  return wrap_video_frame(snapshot);
}
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_clone(const NativeRawVideoBuffer& buffer) noexcept {
  return std::make_unique<NativeRawVideoBuffer>(buffer.value());
}
FfiRawBufferInfo raw_buffer_info(const NativeRawVideoBuffer& buffer) noexcept { return BufferInfo(*buffer.value()); }
rust::Vec<std::uint8_t> raw_buffer_storage(const NativeRawVideoBuffer& buffer) noexcept {
  const auto text = buffer.value()->storage_representation();
  rust::Vec<std::uint8_t> result;
  result.reserve(text.size()); for (unsigned char c : text) result.push_back(c);
  return result;
}
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_to_i420(const NativeRawVideoBuffer& buffer) noexcept {
  auto value = buffer.value()->ToI420();
  return value ? std::make_unique<NativeRawVideoBuffer>(std::move(value)) : nullptr;
}
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_get_i420(const NativeRawVideoBuffer& buffer) noexcept {
  const auto* value = buffer.value()->GetI420();
  return value ? std::make_unique<NativeRawVideoBuffer>(webrtc::scoped_refptr<webrtc::VideoFrameBuffer>(const_cast<webrtc::I420BufferInterface*>(value))) : nullptr;
}
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_crop_scale(const NativeRawVideoBuffer& buffer,
    std::int32_t x, std::int32_t y, std::int32_t cw, std::int32_t ch, std::int32_t sw, std::int32_t sh) noexcept {
  if (!CropSafe(buffer.value()->width(), buffer.value()->height(), x, y, cw, ch) || cw > 32768 || ch > 32768 || !GeometrySafe(sw, sh)) return nullptr;
  const std::int64_t width = sw, height = sh;
  std::int64_t bytes = 0;
  using T = webrtc::VideoFrameBuffer::Type;
  switch (buffer.value()->type()) {
    case T::kI422: bytes = height * (width + 2 * ((width + 1) / 2)); break;
    case T::kI444: bytes = 3 * width * height; break;
    case T::kNV12: bytes = width * height + (width + width % 2) * ((height + 1) / 2); break;
    default: break;  // Other CPU types use the base I420 allocation above.
  }
  if (bytes > INT32_MAX) return nullptr;
  // The default SDK method dereferences ToI420 without a failure check. The
  // admitted CPU graph has a real fallback; preflight conversion before entry.
  if (!buffer.value()->ToI420()) return nullptr;
  auto value = buffer.value()->CropAndScale(x, y, cw, ch, sw, sh);
  return value ? std::make_unique<NativeRawVideoBuffer>(std::move(value)) : nullptr;
}
std::unique_ptr<NativeRawVideoBuffer> raw_buffer_mapped(const NativeRawVideoBuffer& buffer, rust::Slice<const std::uint8_t> types) noexcept {
  if (buffer.value()->type() != webrtc::VideoFrameBuffer::Type::kNative) return nullptr;
  std::vector<webrtc::VideoFrameBuffer::Type> native;
  for (auto type : types) { if (type > 8) return nullptr; native.push_back(static_cast<webrtc::VideoFrameBuffer::Type>(type)); }
  auto value = buffer.value()->GetMappedFrameBuffer(native);
  return value ? std::make_unique<NativeRawVideoBuffer>(std::move(value)) : nullptr;
}
FfiRawVideoPlane raw_buffer_plane(const NativeRawVideoBuffer& buffer, std::uint8_t plane) noexcept { return BufferPlane(*buffer.value(), plane); }
std::unique_ptr<NativeWritableI420> raw_i420_create(std::int32_t w, std::int32_t h, std::int32_t y, std::int32_t u, std::int32_t v) noexcept {
  if (!AllocationSafe(w, h, y, u, v)) return nullptr;
  return std::make_unique<NativeWritableI420>(webrtc::I420Buffer::Create(w, h, y, u, v));
}
std::unique_ptr<NativeWritableI420> raw_i420_copy(const NativeRawVideoBuffer& buffer) noexcept {
  const auto* source = buffer.value()->GetI420();
  if (!source || !GeometrySafe(source->width(), source->height())) return nullptr;
  return std::make_unique<NativeWritableI420>(webrtc::I420Buffer::Copy(*source), true);
}
std::unique_ptr<NativeWritableI420> raw_i420_rotate(const NativeRawVideoBuffer& buffer, std::uint16_t rotation) noexcept {
  const auto* source = buffer.value()->GetI420();
  if (!source || !RotationValid(rotation) || !GeometrySafe(source->width(), source->height())) return nullptr;
  return std::make_unique<NativeWritableI420>(webrtc::I420Buffer::Rotate(*source, static_cast<webrtc::VideoRotation>(rotation)), true);
}
std::unique_ptr<NativeRawVideoBuffer> raw_i420_freeze(std::unique_ptr<NativeWritableI420> buffer) noexcept {
  if (!buffer || !buffer->Initialized()) return nullptr;
  return std::make_unique<NativeRawVideoBuffer>(buffer->Freeze());
}
FfiRawBufferInfo raw_i420_info(const NativeWritableI420& buffer) noexcept { return BufferInfo(buffer.value()); }
bool raw_i420_initialized(const NativeWritableI420& buffer) noexcept { return buffer.Initialized(); }
FfiRawVideoPlane raw_i420_plane(const NativeWritableI420& buffer, std::uint8_t plane) noexcept {
  return buffer.PlaneInitialized(plane) ? BufferPlane(buffer.value(), plane) : FfiRawVideoPlane{};
}
bool raw_i420_write_plane(NativeWritableI420& buffer, std::uint8_t plane, rust::Slice<const std::uint8_t> source, std::int32_t stride) noexcept {
  if (plane > 2) return false;
  auto& b = buffer.value();
  const auto width = plane == 0 ? b.width() : b.ChromaWidth();
  const auto height = plane == 0 ? b.height() : b.ChromaHeight();
  if (stride < width || static_cast<std::uint64_t>(height - 1) * stride + width > source.size()) return false;
  auto* target = plane == 0 ? b.MutableDataY() : plane == 1 ? b.MutableDataU() : b.MutableDataV();
  const auto target_stride = plane == 0 ? b.StrideY() : plane == 1 ? b.StrideU() : b.StrideV();
  for (int row = 0; row < height; ++row) std::memcpy(target + static_cast<std::ptrdiff_t>(row) * target_stride,
      source.data() + static_cast<std::ptrdiff_t>(row) * stride, width);
  buffer.MarkPlane(plane);
  return true;
}
void raw_i420_initialize(NativeWritableI420& buffer, bool black) noexcept {
  if (black) webrtc::I420Buffer::SetBlack(&buffer.value()); else buffer.value().InitializeData();
  buffer.MarkAll();
}
bool raw_i420_crop_from(NativeWritableI420& target, const NativeRawVideoBuffer& source,
    std::int32_t x, std::int32_t y, std::int32_t w, std::int32_t h) noexcept {
  const auto* i420 = source.value()->GetI420();
  if (!i420 || !CropSafe(i420->width(), i420->height(), x, y, w, h) || w > 32768 || h > 32768) return false;
  target.value().CropAndScaleFrom(*i420, x, y, w, h); target.MarkAll(); return true;
}
bool raw_i420_scale_from(NativeWritableI420& target, const NativeRawVideoBuffer& source, bool center_crop) noexcept {
  const auto* i420 = source.value()->GetI420();
  if (!i420) return false;
  if (center_crop) {
    const auto width_product = static_cast<std::int64_t>(target.value().width()) * i420->height();
    const auto height_product = static_cast<std::int64_t>(target.value().height()) * i420->width();
    if (width_product > INT32_MAX || height_product > INT32_MAX ||
        width_product / target.value().height() == 0 || height_product / target.value().width() == 0 ||
        std::min<std::int64_t>(i420->width(), width_product / target.value().height()) > 32768 ||
        std::min<std::int64_t>(i420->height(), height_product / target.value().width()) > 32768) return false;
    target.value().CropAndScaleFrom(*i420);
  } else {
    if (i420->width() > 32768 || i420->height() > 32768) return false;
    target.value().ScaleFrom(*i420);
  }
  target.MarkAll(); return true;
}

struct NativeVideoBufferPreparation::State {
  std::mutex mutex;
  std::optional<std::size_t> prepared;
  bool closed = false;
};
namespace {
class PreparedHandler : public webrtc::VideoFrameBuffer::PreparedFrameHandler {
 public:
  PreparedHandler(std::shared_ptr<NativeVideoBufferPreparation::State> state,
      webrtc::scoped_refptr<webrtc::VideoFrameBuffer> buffer) : state_(std::move(state)), buffer_(std::move(buffer)) {}
  void OnFramePrepared(std::size_t identifier) override {
    std::lock_guard lock(state_->mutex);
    if (!state_->closed) state_->prepared = identifier;
  }
 private:
  std::shared_ptr<NativeVideoBufferPreparation::State> state_;
  webrtc::scoped_refptr<webrtc::VideoFrameBuffer> buffer_;
};
}  // namespace
std::unique_ptr<NativeVideoBufferPreparation> raw_buffer_prepare(const NativeRawVideoBuffer& buffer,
    std::size_t width, std::size_t height, std::size_t identifier) noexcept {
  auto state = std::make_shared<NativeVideoBufferPreparation::State>();
  buffer.value()->PrepareMappedBufferAsync(width, height, webrtc::make_ref_counted<PreparedHandler>(state, buffer.value()), identifier);
  return std::make_unique<NativeVideoBufferPreparation>(std::move(state));
}
bool raw_buffer_prepared(const NativeVideoBufferPreparation& request, std::size_t& identifier) noexcept {
  std::lock_guard lock(request.state()->mutex);
  if (!request.state()->prepared) return false;
  identifier = *request.state()->prepared; request.state()->prepared.reset(); return true;
}
void raw_buffer_preparation_close(const NativeVideoBufferPreparation& request) noexcept {
  std::lock_guard lock(request.state()->mutex); request.state()->closed = true;
}
FfiRawUpdateRect raw_update_rect_operation(FfiRawUpdateRect a, FfiRawUpdateRect b, std::uint8_t operation,
    rust::Slice<const std::int32_t> geometry, bool& valid) noexcept {
  valid = false;
  auto end_safe = [](const FfiRawUpdateRect& r) {
    const auto x = static_cast<std::int64_t>(r.x) + r.width, y = static_cast<std::int64_t>(r.y) + r.height;
    return x >= INT32_MIN && x <= INT32_MAX && y >= INT32_MIN && y <= INT32_MAX;
  };
  auto result = Rect(a);
  if (operation <= 1) {
    if (!result.IsEmpty() && !Rect(b).IsEmpty()) {
      if (!end_safe(a) || !end_safe(b)) return {};
      const std::int64_t ax = static_cast<std::int64_t>(a.x) + a.width, ay = static_cast<std::int64_t>(a.y) + a.height;
      const std::int64_t bx = static_cast<std::int64_t>(b.x) + b.width, by = static_cast<std::int64_t>(b.y) + b.height;
      const auto width = operation == 0 ? std::max(ax, bx) - std::min<std::int64_t>(a.x, b.x)
                                       : std::min(ax, bx) - std::max<std::int64_t>(a.x, b.x);
      const auto height = operation == 0 ? std::max(ay, by) - std::min<std::int64_t>(a.y, b.y)
                                        : std::min(ay, by) - std::max<std::int64_t>(a.y, b.y);
      if (width < INT32_MIN || width > INT32_MAX || height < INT32_MIN || height > INT32_MAX ||
          (operation == 0 && (width <= 0 || height <= 0))) return {};
    }
    if (operation == 0) result.Union(Rect(b)); else result.Intersect(Rect(b));
  } else if (operation == 2) {
    result.MakeEmptyUpdate();
  } else if (operation == 3) {
    if (geometry.size() != 8 || geometry[0] <= 0 || geometry[1] <= 0 ||
        !CropSafe(geometry[0], geometry[1], geometry[2], geometry[3], geometry[4], geometry[5]) ||
        geometry[6] <= 0 || geometry[7] <= 0 || !RectSafe(a, geometry[0], geometry[1])) return {};
    // This pin multiplies in int, then rounds/expands before clipping. Prove
    // every reached intermediate, not just the final rectangle dimensions.
    const std::int64_t cx = geometry[2], cy = geometry[3], cw = geometry[4], ch = geometry[5];
    const std::int64_t sw = geometry[6], sh = geometry[7];
    bool outside = static_cast<std::int64_t>(a.x) + a.width < cx || a.x > cx + cw ||
                   static_cast<std::int64_t>(a.y) + a.height < cy;
    if (!outside) {
      // Preserve this pin's crop-width expression in its vertical test.
      if (cy + cw > INT32_MAX) return {};
      outside = a.y > cy + cw;
    }
    if (!outside) {
      auto safe_axis = [](std::int64_t offset, std::int64_t extent, std::int64_t crop,
                          std::int64_t crop_size, std::int64_t scaled) {
        auto position = offset - crop;
        if (position < 0) { extent += position; position = 0; }
        const auto lower = position * scaled;
        const auto upper = extent * scaled + crop_size - 1;
        if (lower > INT32_MAX || extent * scaled + crop_size > INT32_MAX || upper < INT32_MIN) return false;
        position = lower / crop_size; extent = upper / crop_size;
        if (position % 2) { --position; ++extent; }
        if (extent % 2) ++extent;
        if (scaled != crop_size) {
          if (position > 0) { position -= 2; extent += 2; }
          extent += 2;
        }
        return extent <= INT32_MAX && position + extent <= INT32_MAX;
      };
      if (!safe_axis(a.x, a.width, cx, cw, sw) || !safe_axis(a.y, a.height, cy, ch, sh)) return {};
    }
    result = result.ScaleWithFrame(geometry[0], geometry[1], geometry[2], geometry[3], geometry[4], geometry[5], geometry[6], geometry[7]);
  } else return {};
  valid = true; return Rect(result);
}

}  // namespace pulsebeam::webrtc_sys
