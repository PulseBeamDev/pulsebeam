#if defined(__linux__) && !defined(_GNU_SOURCE)
#define _GNU_SOURCE
#endif
#include "pulsebeam-webrtc-sys/native/desktop_memory.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <cerrno>
#include <cstring>
#include <limits>
#include <vector>
#if defined(__linux__)
#include <fcntl.h>
#include <linux/memfd.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>
#endif
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
#include "modules/desktop_capture/shared_memory.h"
#define PULSEBEAM_DESKTOP_MEMORY 1
#endif
namespace pulsebeam::webrtc_sys {
#if defined(PULSEBEAM_DESKTOP_MEMORY)
namespace {
struct Storage {
  int fd = -1;
  std::size_t size = 0;
  void* data = nullptr;
  bool mapped = false;
  std::unique_ptr<std::uint8_t[]> heap;
  std::vector<rust::Box<RustDesktopMemoryLease>> leases;
  ~Storage() {
    if (mapped) munmap(data, size);
    if (fd >= 0) close(fd);
    heap.reset();
    // Rust dependency destructors run after the native buffer's final use and
    // are panic-contained by RustDesktopMemoryLease::drop.
  }
};
class Memory final : public webrtc::SharedMemory {
 public:
  Memory(std::unique_ptr<Storage> storage, int id, std::size_t offset = 0)
      : SharedMemory(offset ? static_cast<std::uint8_t*>(storage->data) + offset : storage->data,
                     storage->size - offset, storage->fd, id), storage_(std::move(storage)) {}
  Storage& storage() { return *storage_; }
  std::unique_ptr<Storage> TakeStorage() { return std::move(storage_); }
 private:
  std::unique_ptr<Storage> storage_;
};
class Factory final : public webrtc::SharedMemoryFactory {
 public:
  explicit Factory(rust::Box<RustDesktopMemoryFactory> source) : source_(std::move(source)) {}
  std::unique_ptr<webrtc::SharedMemory> CreateSharedMemory(std::size_t size) override {
    auto memory = desktop_memory_factory_create(*source_, size);
    if (!memory || desktop_memory_info(*memory).size < size) return nullptr;
    return take_desktop_memory(std::move(memory));
  }
 private:
  rust::Box<RustDesktopMemoryFactory> source_;
};
}
#endif
struct NativeDesktopMemory::State {
#if defined(PULSEBEAM_DESKTOP_MEMORY)
  std::unique_ptr<Memory> value;
#endif
};
NativeDesktopMemory::NativeDesktopMemory(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeDesktopMemory::~NativeDesktopMemory() = default;
const std::unique_ptr<NativeDesktopMemory::State>& NativeDesktopMemory::state() const noexcept { return state_; }
std::unique_ptr<NativeDesktopMemory> desktop_memory_map(std::int32_t fd, std::size_t size, std::int32_t id, std::int32_t& error) noexcept {
  error = 0;
#if defined(PULSEBEAM_DESKTOP_MEMORY)
  auto storage = std::make_unique<Storage>();
  storage->fd = fd; storage->size = size;
  // Linux seals are monotonic. Require shrink protection before fstat/mmap,
  // avoiding a truncation TOCTOU/SIGBUS through other descriptor aliases.
  const int seals = fcntl(fd, F_GET_SEALS);
  if (seals < 0) { error = errno; return nullptr; }
  if (!(seals & F_SEAL_SHRINK) || size > std::size_t(std::numeric_limits<std::ptrdiff_t>::max())) { error = EINVAL; return nullptr; }
  struct stat st{};
  if (fstat(fd, &st) != 0) { error = errno; return nullptr; }
  if (st.st_size < 0 || std::uint64_t(st.st_size) < size) { error = EINVAL; return nullptr; }
  if (size) {
    storage->data = mmap(nullptr, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (storage->data == MAP_FAILED) { storage->data = nullptr; error = errno; return nullptr; }
    storage->mapped = true;
  }
  auto state = std::make_unique<NativeDesktopMemory::State>();
  state->value = std::make_unique<Memory>(std::move(storage), id);
  return std::make_unique<NativeDesktopMemory>(std::move(state));
#else
#if defined(__linux__)
  if (fd >= 0) close(fd);
#endif
  error = ENOTSUP; return nullptr;
#endif
}
std::unique_ptr<NativeDesktopMemory> desktop_memory_new(std::size_t size, std::int32_t id, bool heap, std::int32_t& error) noexcept {
  error = 0;
#if defined(PULSEBEAM_DESKTOP_MEMORY)
  if (size > std::size_t(std::numeric_limits<std::ptrdiff_t>::max()) || size > std::size_t(std::numeric_limits<off_t>::max())) { error = EINVAL; return nullptr; }
  if (heap) {
    auto storage = std::make_unique<Storage>(); storage->size = size;
    storage->heap = std::make_unique<std::uint8_t[]>(size); storage->data = storage->heap.get();
    auto state = std::make_unique<NativeDesktopMemory::State>();
    state->value = std::make_unique<Memory>(std::move(storage), id);
    return std::make_unique<NativeDesktopMemory>(std::move(state));
  }
  const int fd = static_cast<int>(syscall(SYS_memfd_create, "pulsebeam-desktop", MFD_CLOEXEC | MFD_ALLOW_SEALING));
  if (fd < 0) { error = errno; return nullptr; }
  if (ftruncate(fd, static_cast<off_t>(size)) != 0 || fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK) != 0) {
    error = errno; close(fd); return nullptr;
  }
  return desktop_memory_map(fd, size, id, error);
#else
  error = ENOTSUP; return nullptr;
#endif
}
FfiDesktopMemoryInfo desktop_memory_info(const NativeDesktopMemory& memory) noexcept {
  FfiDesktopMemoryInfo out{};
#if defined(PULSEBEAM_DESKTOP_MEMORY)
  const auto& value = *memory.state()->value;
  out.size = value.size(); out.id = value.id(); out.has_handle = value.handle() != webrtc::SharedMemory::kInvalidHandle;
#endif
  return out;
}
FfiDesktopBytes desktop_memory_read(const NativeDesktopMemory& memory, std::size_t offset, std::size_t size) noexcept {
  FfiDesktopBytes out{};
#if defined(PULSEBEAM_DESKTOP_MEMORY)
  const auto& value = *memory.state()->value;
  if (offset > value.size() || size > value.size() - offset) { out.status = 2; return out; }
  const auto* data = static_cast<const std::uint8_t*>(value.data());
  for (std::size_t i = 0; i < size; ++i) out.bytes.push_back(data[offset + i]);
#else
  out.status = 3;
#endif
  return out;
}
bool desktop_memory_write(NativeDesktopMemory& memory, std::size_t offset, rust::Slice<const std::uint8_t> bytes) noexcept {
#if defined(PULSEBEAM_DESKTOP_MEMORY)
  auto& value = *memory.state()->value;
  if (offset > value.size() || bytes.size() > value.size() - offset) return false;
  if (bytes.size()) std::memcpy(static_cast<std::uint8_t*>(value.data()) + offset, bytes.data(), bytes.size());
  return true;
#else
  return false;
#endif
}
FfiDesktopFd desktop_memory_fd(const NativeDesktopMemory& memory) noexcept {
  FfiDesktopFd out{}; out.fd = -1;
#if defined(PULSEBEAM_DESKTOP_MEMORY)
  const int fd = memory.state()->value->handle();
  if (fd == webrtc::SharedMemory::kInvalidHandle) return out;
  out.fd = fcntl(fd, F_DUPFD_CLOEXEC, 0);
  if (out.fd < 0) out.error = errno;
#else
  out.error = ENOTSUP;
#endif
  return out;
}
void desktop_memory_retain(NativeDesktopMemory& memory, rust::Box<RustDesktopMemoryLease> lease) noexcept {
#if defined(PULSEBEAM_DESKTOP_MEMORY)
  memory.state()->value->storage().leases.push_back(std::move(lease));
#endif
}
#if defined(PULSEBEAM_DESKTOP_MEMORY)
std::unique_ptr<webrtc::SharedMemory> take_desktop_memory(std::unique_ptr<NativeDesktopMemory> memory) noexcept {
  if (!memory) return nullptr;
  return std::move(memory->state()->value);
}
std::unique_ptr<webrtc::SharedMemory> take_desktop_memory_at(std::unique_ptr<NativeDesktopMemory> memory, std::size_t offset) noexcept {
  if (!memory || offset > memory->state()->value->size()) return nullptr;
  if (!offset) return take_desktop_memory(std::move(memory));
  auto& value = *memory->state()->value;
  const int id = value.id();
  auto storage = value.TakeStorage();
  return std::make_unique<Memory>(std::move(storage), id, offset);
}
std::unique_ptr<webrtc::SharedMemoryFactory> make_desktop_memory_factory(rust::Box<RustDesktopMemoryFactory> source) noexcept {
  if (!desktop_memory_factory_present(*source)) return nullptr;
  return std::make_unique<Factory>(std::move(source));
}
#endif
}
