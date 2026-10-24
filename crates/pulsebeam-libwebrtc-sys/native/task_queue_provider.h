#pragma once

#include <cstdint>
#include <memory>

#include "api/task_queue/task_queue_base.h"
#include "api/task_queue/task_queue_factory.h"
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
struct RustTask;
struct RustTaskQueueFactory;
struct RustTaskQueueProvider;

// A move-only delivery ticket, not a borrowed queue or callable. Deletion
// revokes its shared slot before releasing the native queue identity.
class NativeTaskQueueTicket final {
 public:
  struct State;
  struct Slot;
  NativeTaskQueueTicket(std::shared_ptr<State> state,
                        std::shared_ptr<Slot> slot) noexcept;
  ~NativeTaskQueueTicket();
  bool Submit() noexcept;
 private:
  std::shared_ptr<State> state_;
  std::shared_ptr<Slot> slot_;
  bool submitted_ = false;
};
bool task_queue_ticket_submit(std::unique_ptr<NativeTaskQueueTicket> ticket) noexcept;

std::unique_ptr<webrtc::TaskQueueFactory> task_queue_provider_adapter(
    rust::Box<RustTaskQueueFactory> provider) noexcept;
// Only use with a product of task_queue_provider_adapter.
bool task_queue_provider_product_failed(const webrtc::TaskQueueBase& queue) noexcept;

// Owned snapshots only. The current SDK pointer never escapes to Rust.
rust::Vec<std::uint8_t> task_queue_current_name() noexcept;
std::size_t task_queue_current_identity() noexcept;
// Resolve only the live thread-local Current(), never the retained token.
std::uint8_t task_queue_current_post(std::size_t identity, rust::Box<RustTask> task) noexcept;
std::uint8_t task_queue_current_post_delayed(std::size_t identity, std::int64_t bits, bool high_precision, rust::Box<RustTask> task) noexcept;
}  // namespace pulsebeam::webrtc_sys
