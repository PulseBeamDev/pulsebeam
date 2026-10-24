#pragma once

#include <condition_variable>
#include <cstdint>
#include <deque>
#include <map>
#include <memory>
#include <mutex>
#include <thread>
#include <utility>

#include "absl/functional/any_invocable.h"
#include "api/environment/environment.h"
#include "api/task_queue/task_queue_factory.h"
#include "api/task_queue/task_queue_base.h"
#include "rtc_base/checks.h"

namespace pulsebeam::webrtc_sys {

// A creator-local resource table, not an executor. Native aliases carry only
// IDs and synchronized notifications. No finalizer executes on a releasing
// thread, and no driver/Rc owner is transported to that thread.
class NativeRuntimeRetirement final {
 public:
  using Id = std::uint64_t;
  using Finalizer = absl::AnyInvocable<void() &&>;

  Id Retain(Finalizer finalizer) {
    std::lock_guard lock(mutex_);
    RTC_CHECK_NE(next_, 0u);
    const auto id = next_++;
    jobs_.emplace(id, std::move(finalizer));
    ++generation_;
    changed_.notify_all();
    return id;
  }
  void Release(Id id) {
    std::lock_guard lock(mutex_);
    ready_.push_back(id);
    ++generation_;
    changed_.notify_all();
  }
  std::size_t Drain() {
    RTC_CHECK(creator_ == std::this_thread::get_id());
    std::size_t retired = 0;
    for (;;) {
      Finalizer finalizer;
      {
        std::lock_guard lock(mutex_);
        if (ready_.empty()) return retired;
        const auto id = ready_.front();
        ready_.pop_front();
        auto found = jobs_.find(id);
        RTC_CHECK(found != jobs_.end());
        finalizer = std::move(found->second);
        jobs_.erase(found);
      }
      // In particular, SDK queue deletion and Rust provider destruction run
      // with neither the table nor a notification mutex borrowed.
      std::move(finalizer)();
      ++retired;
    }
  }
  bool empty() const {
    std::lock_guard lock(mutex_);
    return jobs_.empty();
  }
  std::uint64_t generation() const {
    std::lock_guard lock(mutex_);
    return generation_;
  }
  void Wake() {
    std::lock_guard lock(mutex_);
    ++generation_;
    changed_.notify_all();
  }
  void Wait(std::uint64_t generation) {
    std::unique_lock lock(mutex_);
    changed_.wait(lock, [&] { return generation_ != generation; });
  }
  ~NativeRuntimeRetirement() {
    // A late temporary notification owner may die on another thread, but the
    // creator must already have retired every affine finalizer before that.
    RTC_CHECK(jobs_.empty());
  }
 private:
  const std::thread::id creator_ = std::this_thread::get_id();
  mutable std::mutex mutex_;
  std::condition_variable changed_;
  std::map<Id, Finalizer> jobs_;
  std::deque<Id> ready_;
  Id next_ = 1;
  std::uint64_t generation_ = 0;
};

namespace runtime_retirement_internal {
struct Registry {
  std::mutex mutex;
  std::map<const webrtc::TaskQueueFactory*, std::weak_ptr<NativeRuntimeRetirement>> factories;
  struct ClockEntry {
    std::weak_ptr<NativeRuntimeRetirement> owner;
    std::size_t references = 0;
  };
  std::map<const webrtc::Clock*, ClockEntry> clocks;
};
inline Registry& registry() {
  // Only weak associations, never a driver, environment or affine resource.
  // Process lifetime avoids static-destruction ordering with SDK aliases.
  static auto* value = new Registry;
  return *value;
}
}  // namespace runtime_retirement_internal

inline std::shared_ptr<NativeRuntimeRetirement> runtime_retirement_for(
    const webrtc::Environment& environment) {
  auto& registry = runtime_retirement_internal::registry();
  std::lock_guard lock(registry.mutex);
  if (auto found = registry.factories.find(&environment.task_queue_factory());
      found != registry.factories.end()) {
    if (auto owner = found->second.lock()) return owner;
  }
  if (auto found = registry.clocks.find(&environment.clock());
      found != registry.clocks.end()) return found->second.owner.lock();
  return nullptr;
}

// SDK Environment copies retain this factory through their ordinary native
// storage leaf. The original factory and all Rust-only dependencies stay in a
// creator-owned table entry until the last such copy releases its ticket.
// Queue names/priorities, returned queue objects and execution are unchanged.
class RuntimeBoundTaskQueueFactory final : public webrtc::TaskQueueFactory {
 public:
  RuntimeBoundTaskQueueFactory(webrtc::TaskQueueFactory& delegate,
      webrtc::Clock& clock, std::shared_ptr<NativeRuntimeRetirement> owner,
      NativeRuntimeRetirement::Id id)
      : delegate_(delegate), clock_(clock), owner_(std::move(owner)), id_(id) {
    auto& registry = runtime_retirement_internal::registry();
    std::lock_guard lock(registry.mutex);
    registry.factories.emplace(this, owner_);
    auto& entry = registry.clocks[&clock_];
    if (auto previous = entry.owner.lock()) RTC_CHECK(previous == owner_);
    entry.owner = owner_;
    ++entry.references;
  }
  RuntimeBoundTaskQueueFactory(const RuntimeBoundTaskQueueFactory&) = delete;
  RuntimeBoundTaskQueueFactory& operator=(const RuntimeBoundTaskQueueFactory&) = delete;
  ~RuntimeBoundTaskQueueFactory() override {
    {
      auto& registry = runtime_retirement_internal::registry();
      std::lock_guard lock(registry.mutex);
      registry.factories.erase(this);
      auto clock = registry.clocks.find(&clock_);
      RTC_CHECK(clock != registry.clocks.end());
      if (--clock->second.references == 0) registry.clocks.erase(clock);
    }
    owner_->Release(id_);
  }
  std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter> CreateTaskQueue(
      absl::string_view name, Priority priority) const override {
    return delegate_.CreateTaskQueue(name, priority);
  }
 private:
  webrtc::TaskQueueFactory& delegate_;
  webrtc::Clock& clock_;
  std::shared_ptr<NativeRuntimeRetirement> owner_;
  NativeRuntimeRetirement::Id id_;
};

}  // namespace pulsebeam::webrtc_sys
