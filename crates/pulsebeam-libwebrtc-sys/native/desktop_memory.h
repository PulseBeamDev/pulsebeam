#pragma once
#include <cstdint>
#include <memory>
#include "rust/cxx.h"
namespace webrtc { class SharedMemory; class SharedMemoryFactory; }
namespace pulsebeam::webrtc_sys {
struct FfiDesktopMemoryInfo;
struct FfiDesktopFd;
struct FfiDesktopBytes;
struct RustDesktopMemoryFactory;
struct RustDesktopMemoryLease;
class NativeDesktopMemory final {
 public:
  struct State;
  explicit NativeDesktopMemory(std::unique_ptr<State> state) noexcept;
  ~NativeDesktopMemory();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeDesktopMemory> desktop_memory_new(std::size_t size, std::int32_t id, bool heap, std::int32_t& error) noexcept;
// Takes fd ownership on every path, including validation failure.
std::unique_ptr<NativeDesktopMemory> desktop_memory_map(std::int32_t fd, std::size_t size, std::int32_t id, std::int32_t& error) noexcept;
FfiDesktopMemoryInfo desktop_memory_info(const NativeDesktopMemory& memory) noexcept;
FfiDesktopBytes desktop_memory_read(const NativeDesktopMemory& memory, std::size_t offset, std::size_t size) noexcept;
bool desktop_memory_write(NativeDesktopMemory& memory, std::size_t offset, rust::Slice<const std::uint8_t> bytes) noexcept;
FfiDesktopFd desktop_memory_fd(const NativeDesktopMemory& memory) noexcept;
void desktop_memory_retain(NativeDesktopMemory& memory, rust::Box<RustDesktopMemoryLease> lease) noexcept;
std::unique_ptr<webrtc::SharedMemory> take_desktop_memory(std::unique_ptr<NativeDesktopMemory> memory) noexcept;
std::unique_ptr<webrtc::SharedMemory> take_desktop_memory_at(std::unique_ptr<NativeDesktopMemory> memory, std::size_t offset) noexcept;
std::unique_ptr<webrtc::SharedMemoryFactory> make_desktop_memory_factory(rust::Box<RustDesktopMemoryFactory> source) noexcept;
}
