#pragma once
#include <cstdint>
#include <memory>
#include "rust/cxx.h"
#include "pulsebeam-webrtc-sys/native/desktop_memory.h"
namespace webrtc { class DesktopFrame; }
namespace pulsebeam::webrtc_sys {
struct FfiDesktopFrameInfo;
struct FfiDesktopRect;
struct FfiDesktopBytes;
class NativeDesktopFrame final {
 public:
  struct State;
  explicit NativeDesktopFrame(std::unique_ptr<State> state) noexcept;
  ~NativeDesktopFrame();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeDesktopFrame> wrap_desktop_frame(std::unique_ptr<webrtc::DesktopFrame> frame, bool writable = false, bool shared_handle_is_fd = false);
std::unique_ptr<NativeDesktopFrame> desktop_frame_new(std::int32_t width, std::int32_t height, std::uint32_t format) noexcept;
std::unique_ptr<NativeDesktopFrame> desktop_frame_copy(const NativeDesktopFrame& frame) noexcept;
std::unique_ptr<NativeDesktopFrame> desktop_frame_shared(std::int32_t width, std::int32_t height, std::int32_t stride, std::uint32_t format, std::size_t offset, std::unique_ptr<NativeDesktopMemory> memory, rust::Box<RustDesktopMemoryFactory> factory, bool create, std::uint8_t& error) noexcept;
FfiDesktopFd desktop_frame_fd(const NativeDesktopFrame& frame) noexcept;
FfiDesktopFrameInfo desktop_frame_info(const NativeDesktopFrame& frame) noexcept;
std::uint8_t desktop_frame_write_info(NativeDesktopFrame& frame, const FfiDesktopFrameInfo& info) noexcept;
FfiDesktopBytes desktop_frame_pixels(const NativeDesktopFrame& frame, bool pixel, std::int32_t x, std::int32_t y) noexcept;
std::uint8_t desktop_frame_black(NativeDesktopFrame& frame, bool write) noexcept;
std::uint8_t desktop_frame_is_black(const NativeDesktopFrame& frame) noexcept;
std::uint8_t desktop_frame_copy_metadata(NativeDesktopFrame& frame, const NativeDesktopFrame& source) noexcept;
std::uint8_t desktop_frame_copy_buffer(NativeDesktopFrame& frame, rust::Slice<const std::uint8_t> bytes, std::size_t offset, std::int32_t stride, const FfiDesktopRect& dest) noexcept;
std::uint8_t desktop_frame_copy_pixels(NativeDesktopFrame& frame, const NativeDesktopFrame& source, std::int32_t x, std::int32_t y, const FfiDesktopRect& dest) noexcept;
std::uint8_t desktop_frame_copy_info(NativeDesktopFrame& frame, NativeDesktopFrame& source, bool move_info) noexcept;
std::uint8_t desktop_frame_copy_intersection(NativeDesktopFrame& frame, const NativeDesktopFrame& source, double horizontal_scale, double vertical_scale) noexcept;
}
