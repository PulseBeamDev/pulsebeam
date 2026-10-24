#include "pulsebeam-webrtc-sys/native/desktop_frame.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <algorithm>
#include <cmath>
#include <cstring>
#include <limits>
#include <vector>
#include <cerrno>
#if defined(__linux__)
#include <fcntl.h>
#endif
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
#include "modules/desktop_capture/desktop_frame.h"
#include "modules/desktop_capture/shared_memory.h"
#define PULSEBEAM_DESKTOP_FRAME 1
#endif
namespace pulsebeam::webrtc_sys {
struct NativeDesktopFrame::State {
#if defined(PULSEBEAM_DESKTOP_FRAME)
  std::unique_ptr<webrtc::DesktopFrame> value;
  bool writable = false;
  bool shared_handle_is_fd = false;
#endif
};
NativeDesktopFrame::NativeDesktopFrame(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeDesktopFrame::~NativeDesktopFrame() = default;
const std::unique_ptr<NativeDesktopFrame::State>& NativeDesktopFrame::state() const noexcept { return state_; }
#if defined(PULSEBEAM_DESKTOP_FRAME)
std::unique_ptr<NativeDesktopFrame> wrap_desktop_frame(std::unique_ptr<webrtc::DesktopFrame> frame, bool writable, bool shared_handle_is_fd) {
  if (!frame) return nullptr;
  auto state = std::make_unique<NativeDesktopFrame::State>();
  state->value = std::move(frame);
  state->writable = writable;
  state->shared_handle_is_fd = shared_handle_is_fd;
  return std::make_unique<NativeDesktopFrame>(std::move(state));
}
namespace {
bool Dimensions(int w, int h) {
  return w >= 0 && h >= 0 && w <= INT32_MAX / 4 && (h == 0 || w <= INT32_MAX / h / 4);
}
bool Cpu(const webrtc::DesktopFrame& f) {
  const std::int64_t stride = f.stride();
  const auto absolute = stride < 0 ? -stride : stride;
  return f.data() && Dimensions(f.size().width(), f.size().height()) &&
      absolute >= std::int64_t(f.size().width()) * 4 &&
      (f.size().height() == 0 || absolute <= INT32_MAX / f.size().height());
}
bool Rect(const FfiDesktopRect& r) {
  return r.right >= r.left && r.bottom >= r.top &&
      std::int64_t(r.right) - r.left <= INT32_MAX && std::int64_t(r.bottom) - r.top <= INT32_MAX;
}
bool Within(const FfiDesktopRect& r, int w, int h) {
  return Rect(r) && r.left >= 0 && r.top >= 0 && r.right <= w && r.bottom <= h;
}
webrtc::DesktopRect Actual(const FfiDesktopRect& r) { return webrtc::DesktopRect::MakeLTRB(r.left, r.top, r.right, r.bottom); }
FfiDesktopRect Value(const webrtc::DesktopRect& r) {
  FfiDesktopRect result{}; result.left = r.left(); result.top = r.top(); result.right = r.right(); result.bottom = r.bottom(); return result;
}
bool Origin(int x, int y, int w, int h) {
  return std::int64_t(x) + w <= INT32_MAX && std::int64_t(y) + h <= INT32_MAX;
}
bool FrameRect(const webrtc::DesktopFrame& f) {
  const float scale = f.scale_factor();
  const float w = f.size().width() / scale, h = f.size().height() / scale;
  return std::isfinite(w) && std::isfinite(h) && w >= 0 && h >= 0 &&
      double(w) <= INT32_MAX && double(h) <= INT32_MAX &&
      Origin(f.top_left().x(), f.top_left().y(), static_cast<int>(w), static_cast<int>(h));
}
}
#endif
std::unique_ptr<NativeDesktopFrame> desktop_frame_new(std::int32_t width, std::int32_t height, std::uint32_t format) noexcept {
#if defined(PULSEBEAM_DESKTOP_FRAME)
  if (!Dimensions(width, height) || (format != webrtc::FOURCC_ARGB && format != webrtc::FOURCC_ABGR && format != webrtc::FOURCC_BGRA && format != webrtc::FOURCC_RGBA)) return nullptr;
  return wrap_desktop_frame(std::make_unique<webrtc::BasicDesktopFrame>(webrtc::DesktopSize(width, height), static_cast<webrtc::FourCC>(format)), true);
#else
  return nullptr;
#endif
}
std::unique_ptr<NativeDesktopFrame> desktop_frame_copy(const NativeDesktopFrame& frame) noexcept {
#if defined(PULSEBEAM_DESKTOP_FRAME)
  if (!Cpu(*frame.state()->value)) return nullptr;
  return wrap_desktop_frame(std::unique_ptr<webrtc::DesktopFrame>(webrtc::BasicDesktopFrame::CopyOf(*frame.state()->value)), true);
#else
  return nullptr;
#endif
}
std::unique_ptr<NativeDesktopFrame> desktop_frame_shared(std::int32_t width, std::int32_t height, std::int32_t stride, std::uint32_t format, std::size_t offset, std::unique_ptr<NativeDesktopMemory> memory, rust::Box<RustDesktopMemoryFactory> factory, bool create, std::uint8_t& error) noexcept {
  error = 3;
#if defined(PULSEBEAM_DESKTOP_FRAME) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
  if (!Dimensions(width, height) || (format != webrtc::FOURCC_ARGB && format != webrtc::FOURCC_ABGR && format != webrtc::FOURCC_BGRA && format != webrtc::FOURCC_RGBA)) { error = 2; return nullptr; }
  if (create) {
    auto provider = make_desktop_memory_factory(std::move(factory));
    if (!provider) { error = 2; return nullptr; }
    auto frame = webrtc::SharedMemoryDesktopFrame::Create(webrtc::DesktopSize(width, height), static_cast<webrtc::FourCC>(format), provider.get());
    if (!frame) { error = 1; return nullptr; }
    error = 0; return wrap_desktop_frame(std::move(frame), true, true);
  }
  // Validate both ends of signed-stride rows against the original allocation.
  // Rebase the owning SDK data view only after all bounds checks succeeded.
  const std::int64_t absolute = stride < 0 ? -std::int64_t(stride) : stride;
  if (!memory || absolute < std::int64_t(width) * 4 || (height && absolute > INT32_MAX / height)) { error = 2; return nullptr; }
  const auto capacity = desktop_memory_info(*memory).size;
  if (offset > capacity) { error = 2; return nullptr; }
  if (width && height) {
    const std::int64_t delta = std::int64_t(height - 1) * stride;
    const auto low = std::min<std::int64_t>(0, delta), high = std::max<std::int64_t>(0, delta);
    if (std::uint64_t(-low) > offset || std::uint64_t(high) > capacity - offset || std::uint64_t(width) * 4 > capacity - offset - high) { error = 2; return nullptr; }
  }
  auto buffer = take_desktop_memory_at(std::move(memory), offset);
  if (!buffer || (width && height && !buffer->data())) { error = 2; return nullptr; }
  auto frame = std::make_unique<webrtc::SharedMemoryDesktopFrame>(webrtc::DesktopSize(width, height), stride, static_cast<webrtc::FourCC>(format), std::move(buffer));
  error = 0; return wrap_desktop_frame(std::move(frame), true, true);
#else
  return nullptr;
#endif
}
FfiDesktopFd desktop_frame_fd(const NativeDesktopFrame& frame) noexcept {
  FfiDesktopFd out{}; out.fd = -1;
#if defined(PULSEBEAM_DESKTOP_FRAME) && defined(__linux__)
  auto* memory = frame.state()->value->shared_memory();
  if (!memory || memory->handle() == webrtc::SharedMemory::kInvalidHandle) return out;
  // SDK Handle is an open platform-specific integer, not necessarily a file
  // descriptor. Only our owning SharedMemory products establish FD semantics.
  if (!frame.state()->shared_handle_is_fd) { out.error = ENOTSUP; return out; }
  out.fd = fcntl(memory->handle(), F_DUPFD_CLOEXEC, 0);
  if (out.fd < 0) out.error = errno;
#else
  out.error = ENOTSUP;
#endif
  return out;
}
FfiDesktopFrameInfo desktop_frame_info(const NativeDesktopFrame& frame) noexcept {
  FfiDesktopFrameInfo out{};
#if defined(PULSEBEAM_DESKTOP_FRAME)
  const auto& f = *frame.state()->value;
  out.width = f.size().width(); out.height = f.size().height(); out.stride = f.stride(); out.pixel_format = f.pixel_format();
  out.left = f.top_left().x(); out.top = f.top_left().y(); out.scale_factor = f.scale_factor();
  out.rect_valid = FrameRect(f); if (out.rect_valid) out.rect = Value(f.rect());
  out.dpi_x = f.dpi().x(); out.dpi_y = f.dpi().y();
  out.has_device_scale = f.device_scale_factor().has_value(); out.device_scale = f.device_scale_factor().value_or(0);
  out.may_contain_cursor = f.may_contain_cursor(); out.capture_time_ms = f.capture_time_ms(); out.capturer_id = f.capturer_id();
  for (auto byte : f.icc_profile()) out.icc_profile.push_back(byte);
  for (webrtc::DesktopRegion::Iterator it(f.updated_region()); !it.IsAtEnd(); it.Advance()) out.updated_region.push_back(Value(it.rect()));
  out.has_cpu_data = f.data() != nullptr; out.has_texture = f.texture() != nullptr;
  if (auto* memory = f.shared_memory()) {
    out.has_shared_memory = true; out.shared_memory_size = memory->size(); out.shared_memory_id = memory->id();
    out.shared_memory_has_handle = memory->handle() != webrtc::SharedMemory::kInvalidHandle;
  }
#endif
  return out;
}
std::uint8_t desktop_frame_write_info(NativeDesktopFrame& frame, const FfiDesktopFrameInfo& info) noexcept {
#if defined(PULSEBEAM_DESKTOP_FRAME)
  auto& f = *frame.state()->value;
  if (!Origin(info.left, info.top, f.size().width(), f.size().height())) return 2;
  for (const auto& rect : info.updated_region) if (!Rect(rect)) return 2;
  f.set_top_left(webrtc::DesktopVector(info.left, info.top)); f.set_dpi(webrtc::DesktopVector(info.dpi_x, info.dpi_y));
  f.set_device_scale_factor(info.has_device_scale ? std::optional<float>(info.device_scale) : std::nullopt);
  f.set_may_contain_cursor(info.may_contain_cursor); f.set_capture_time_ms(info.capture_time_ms); f.set_capturer_id(info.capturer_id);
  f.set_icc_profile(std::vector<std::uint8_t>(info.icc_profile.begin(), info.icc_profile.end()));
  f.mutable_updated_region()->Clear();
  for (const auto& rect : info.updated_region) f.mutable_updated_region()->AddRect(Actual(rect));
  return 0;
#else
  return 3;
#endif
}
FfiDesktopBytes desktop_frame_pixels(const NativeDesktopFrame& frame, bool pixel, std::int32_t x, std::int32_t y) noexcept {
  FfiDesktopBytes out{};
#if defined(PULSEBEAM_DESKTOP_FRAME)
  const auto& f = *frame.state()->value;
  if (!Cpu(f)) { out.status = 1; return out; }
  if (pixel) {
    if (x < 0 || y < 0 || x >= f.size().width() || y >= f.size().height()) { out.status = 2; return out; }
    const auto* p = f.GetFrameDataAtPos(webrtc::DesktopVector(x, y));
    for (int i = 0; i < 4; ++i) out.bytes.push_back(p[i]);
  } else if (f.size().width() != 0 && f.size().height() != 0) {
    for (int row = 0; row < f.size().height(); ++row) {
      const auto* p = f.data() + std::int64_t(row) * f.stride();
      for (int i = 0; i < f.size().width() * 4; ++i) out.bytes.push_back(p[i]);
    }
  }
#else
  out.status = 3;
#endif
  return out;
}
std::uint8_t desktop_frame_black(NativeDesktopFrame& frame, bool write) noexcept {
#if defined(PULSEBEAM_DESKTOP_FRAME)
  auto& f = *frame.state()->value;
  // These SDK methods assume packed, positively strided, uint32-aligned data.
  // Never apply their memset/linear scan to a cropped/padded/negative buffer.
  if (!Cpu(f) || f.stride() != f.size().width() * 4 || reinterpret_cast<std::uintptr_t>(f.data()) % alignof(std::uint32_t)) return 2;
  if (write) { if (!frame.state()->writable) return 2; f.SetFrameDataToBlack(); return 0; }
  return f.FrameDataIsBlack() ? 1 : 0;
#else
  return 3;
#endif
}
std::uint8_t desktop_frame_is_black(const NativeDesktopFrame& frame) noexcept {
  return desktop_frame_black(const_cast<NativeDesktopFrame&>(frame), false);
}
std::uint8_t desktop_frame_copy_metadata(NativeDesktopFrame& frame, const NativeDesktopFrame& source) noexcept {
  return desktop_frame_copy_info(frame, const_cast<NativeDesktopFrame&>(source), false);
}
std::uint8_t desktop_frame_copy_buffer(NativeDesktopFrame& frame, rust::Slice<const std::uint8_t> bytes, std::size_t offset, std::int32_t stride, const FfiDesktopRect& dest) noexcept {
#if defined(PULSEBEAM_DESKTOP_FRAME)
  auto& f = *frame.state()->value;
  if (!frame.state()->writable || !Cpu(f)) return 1;
  if (!Within(dest, f.size().width(), f.size().height()) || offset > bytes.size()) return 2;
  const auto w = dest.right - dest.left, h = dest.bottom - dest.top;
  if (!w || !h) return 0;
  const std::int64_t last = std::int64_t(h - 1) * stride;
  const auto low = std::min<std::int64_t>(0, last), high = std::max<std::int64_t>(0, last);
  if (offset > INT64_MAX || std::uint64_t(-low) > offset || std::uint64_t(high) > bytes.size() - offset ||
      std::uint64_t(w) * 4 > bytes.size() - offset - high) return 2;
  f.CopyPixelsFrom(bytes.data() + offset, stride, Actual(dest)); return 0;
#else
  return 3;
#endif
}
std::uint8_t desktop_frame_copy_pixels(NativeDesktopFrame& frame, const NativeDesktopFrame& source, std::int32_t x, std::int32_t y, const FfiDesktopRect& dest) noexcept {
#if defined(PULSEBEAM_DESKTOP_FRAME)
  auto& f = *frame.state()->value; const auto& s = *source.state()->value;
  if (!frame.state()->writable || !Cpu(f) || !Cpu(s)) return 1;
  if (!Within(dest, f.size().width(), f.size().height()) || x < 0 || y < 0 ||
      std::int64_t(x) + std::int64_t(dest.right) - dest.left > s.size().width() ||
      std::int64_t(y) + std::int64_t(dest.bottom) - dest.top > s.size().height() || f.pixel_format() != s.pixel_format()) return 2;
  if (dest.left == dest.right || dest.top == dest.bottom) return 0;
  f.CopyPixelsFrom(s, webrtc::DesktopVector(x, y), Actual(dest)); return 0;
#else
  return 3;
#endif
}
std::uint8_t desktop_frame_copy_info(NativeDesktopFrame& frame, NativeDesktopFrame& source, bool move_info) noexcept {
#if defined(PULSEBEAM_DESKTOP_FRAME)
  auto& f = *frame.state()->value; auto& s = *source.state()->value;
  if (!Origin(s.top_left().x(), s.top_left().y(), f.size().width(), f.size().height())) return 2;
  if (move_info) f.MoveFrameInfoFrom(&s); else f.CopyFrameInfoFrom(s);
  return 0;
#else
  return 3;
#endif
}
std::uint8_t desktop_frame_copy_intersection(NativeDesktopFrame& frame, const NativeDesktopFrame& source, double horizontal_scale, double vertical_scale) noexcept {
#if defined(PULSEBEAM_DESKTOP_FRAME)
  auto& f = *frame.state()->value; const auto& s = *source.state()->value;
  if (!frame.state()->writable || !Cpu(f) || !Cpu(s)) return 3;
  if (f.pixel_format() != s.pixel_format() || !FrameRect(f) || !FrameRect(s) || !std::isfinite(horizontal_scale) || !std::isfinite(vertical_scale)) return 2;
  const auto origin = f.top_left(), from = s.top_left();
  std::int64_t dx = std::int64_t(from.x()) - origin.x(), dy = std::int64_t(from.y()) - origin.y();
  if (dx <= INT32_MIN || dx > INT32_MAX || dy <= INT32_MIN || dy > INT32_MAX || origin.x() == INT32_MIN || origin.y() == INT32_MIN) return 2;
  const auto ax = std::round((horizontal_scale - 1) * dx), ay = std::round((vertical_scale - 1) * dy);
  if (!std::isfinite(ax) || !std::isfinite(ay) || ax <= INT32_MIN || ax > INT32_MAX || ay <= INT32_MIN || ay > INT32_MAX) return 2;
  dx += static_cast<std::int64_t>(ax); dy += static_cast<std::int64_t>(ay);
  if (dx <= INT32_MIN || dx > INT32_MAX || dy <= INT32_MIN || dy > INT32_MAX) return 2;
  const auto a = f.rect(), b = s.rect();
  const std::int64_t left = std::int64_t(b.left()) + static_cast<std::int64_t>(ax), right = std::int64_t(b.right()) + static_cast<std::int64_t>(ax);
  const std::int64_t top = std::int64_t(b.top()) + static_cast<std::int64_t>(ay), bottom = std::int64_t(b.bottom()) + static_cast<std::int64_t>(ay);
  if (left < INT32_MIN || right > INT32_MAX || top < INT32_MIN || bottom > INT32_MAX) return 2;
  const auto l = std::max<std::int64_t>(a.left(), left), r = std::min<std::int64_t>(a.right(), right);
  const auto t = std::max<std::int64_t>(a.top(), top), d = std::min<std::int64_t>(a.bottom(), bottom);
  if (l < r && t < d) {
    const auto sx = std::max<std::int64_t>(0, -dx), sy = std::max<std::int64_t>(0, -dy);
    if (l - origin.x() < 0 || t - origin.y() < 0 || r - origin.x() > f.size().width() || d - origin.y() > f.size().height() || sx + r - l > s.size().width() || sy + d - t > s.size().height()) return 2;
  }
  return f.CopyIntersectingPixelsFrom(s, horizontal_scale, vertical_scale) ? 1 : 0;
#else
  return 3;
#endif
}
}
