#include "pulsebeam-webrtc-sys/native/task_queue_provider.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

#include <algorithm>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <string>
#include <utility>
#include <vector>

namespace pulsebeam::webrtc_sys {
namespace {
using QueueState = NativeTaskQueueTicket::State;
using TaskSlot = NativeTaskQueueTicket::Slot;
struct ProviderBox {
  explicit ProviderBox(rust::Box<RustTaskQueueProvider> value) : value(std::move(value)) {}
  rust::Box<RustTaskQueueProvider> value;
};
thread_local std::vector<const QueueState*> posting;
// This marker is scoped with Current() for provider execution/destruction. It
// permits diagnostics without downcasting an arbitrary SDK current queue.
thread_local const webrtc::TaskQueueBase* current_provider = nullptr;
struct ProviderCurrentScope {
  explicit ProviderCurrentScope(const webrtc::TaskQueueBase* queue)
      : previous(current_provider) { current_provider = queue; }
  ~ProviderCurrentScope() { current_provider = previous; }
  const webrtc::TaskQueueBase* previous;
};
struct PostingScope {
  explicit PostingScope(const QueueState* state) { posting.push_back(state); }
  ~PostingScope() { posting.pop_back(); }
};
class ProviderQueue final : public webrtc::TaskQueueBase {
 public:
  explicit ProviderQueue(std::shared_ptr<QueueState> state) noexcept;
  absl::string_view queue_name() const override;
  void Delete() override;
  bool Failed() const noexcept;
  static void RunTask(webrtc::TaskQueueBase* queue, absl::AnyInvocable<void() &&> task) {
    ProviderCurrentScope provider_current(queue);
    CurrentTaskQueueSetter current(queue);
    std::move(task)();
    // Destroy captures before restoring Current().
    task = nullptr;
  }
  static void DestroyTask(webrtc::TaskQueueBase* queue, absl::AnyInvocable<void() &&> task) {
    ProviderCurrentScope provider_current(queue);
    CurrentTaskQueueSetter current(queue);
    task = nullptr;
  }
 protected:
  void PostTaskImpl(absl::AnyInvocable<void() &&> task, const PostTaskTraits&,
                    const webrtc::Location&) override;
  void PostDelayedTaskImpl(absl::AnyInvocable<void() &&> task, webrtc::TimeDelta delay,
                           const PostDelayedTaskTraits& traits,
                           const webrtc::Location&) override;
 private:
  std::shared_ptr<QueueState> state_;
};
}  // namespace

struct NativeTaskQueueTicket::Slot {
  Slot(absl::AnyInvocable<void() &&> value, bool delayed)
      : task(std::move(value)), delayed(delayed) {}
  absl::AnyInvocable<void() &&> task;
  const bool delayed;
  bool ready = false;
  bool canceled = false;
};

struct NativeTaskQueueTicket::State {
  explicit State(rust::Box<RustTaskQueueProvider> value)
      : provider(std::make_shared<ProviderBox>(std::move(value))) {
    failed = !task_queue_provider_present(*provider->value);
    names.emplace_back("unnamed");
  }
  bool Failed() const {
    std::lock_guard lock(mutex);
    return closed || failed || (provider && task_queue_provider_failed(*provider->value));
  }
  absl::string_view Name() {
    std::shared_ptr<ProviderBox> source;
    {
      std::lock_guard lock(mutex);
      if (closed || failed) return names.front();
      source = provider;
      ++active;
    }
    auto bytes = task_queue_provider_name(*source->value);
    std::string name;
    if (!bytes.empty()) name.assign(reinterpret_cast<const char*>(bytes.data()), bytes.size());
    std::lock_guard lock(mutex);
    --active;
    changed.notify_all();
    auto found = std::find(names.begin(), names.end(), name);
    if (found == names.end()) { names.push_back(std::move(name)); return names.back(); }
    return *found;
  }
  // The first unconsumed immediate task gates subsequent immediate tasks.
  // Delayed tasks become eligible only when their scheduler submits the ticket.
  auto Ready(bool execute) {
    bool earlier_immediate = false;
    for (auto it = slots.begin(); it != slots.end(); ++it) {
      if ((*it)->canceled) return it;
      if (execute && (*it)->ready && ((*it)->delayed || !earlier_immediate)) return it;
      if (!(*it)->delayed) earlier_immediate = true;
    }
    return slots.end();
  }
  void Drain() {
    const bool posting_here = std::find(posting.begin(), posting.end(), this) != posting.end();
    {
      std::lock_guard lock(mutex);
      if (closed || running) return;
      const bool execute = !posting_here && !failed && provider && !task_queue_provider_failed(*provider->value);
      if (Ready(execute) == slots.end()) return;
      running = true;
    }
    for (;;) {
      absl::AnyInvocable<void() &&> task;
      webrtc::TaskQueueBase* identity;
      bool canceled;
      {
        std::lock_guard lock(mutex);
        const bool execute = !posting_here && !failed && provider && !task_queue_provider_failed(*provider->value);
        auto it = Ready(execute);
        if (closed || it == slots.end()) {
          running = false;
          changed.notify_all();
          return;
        }
        task = std::move((*it)->task);
        canceled = (*it)->canceled;
        slots.erase(it);
        identity = queue;
      }
      if (canceled) ProviderQueue::DestroyTask(identity, std::move(task));
      else ProviderQueue::RunTask(identity, std::move(task));
    }
  }
  bool Submit(const std::shared_ptr<TaskSlot>& slot) {
    std::shared_ptr<ProviderBox> rejected;
    {
      std::lock_guard lock(mutex);
      if (closed || failed || !provider || task_queue_provider_failed(*provider->value)) return false;
      if (slot && (!slot->task || slot->ready || slot->canceled)) return false;
      if (std::find(posting.begin(), posting.end(), this) != posting.end()) {
        // A provider must schedule, not invoke native work synchronously inside
        // its posting callback. Reject before entering the SDK callable.
        failed = true;
        rejected = provider;
      } else if (slot) {
        slot->ready = true;
      } else {
        wake_posted = false;
      }
    }
    if (rejected) { task_queue_provider_mark_failed(*rejected->value); return false; }
    Drain();
    return true;
  }
  void Cancel(const std::shared_ptr<TaskSlot>& slot) {
    {
      std::lock_guard lock(mutex);
      if (slot) {
        auto it = std::find(slots.begin(), slots.end(), slot);
        if (it == slots.end()) return;
        slot->canceled = true;
      } else {
        wake_posted = false;
        // A canceled internal wake cancels the deliveries it represented.
        for (auto& entry : slots) if (entry->ready) entry->canceled = true;
      }
    }
    // Destruction is serialized with execution. A ticket dropped by another
    // scheduler thread must not introduce a concurrent native queue sequence.
    Drain();
  }
  void Post(absl::AnyInvocable<void() &&> task, bool delayed,
            webrtc::TimeDelta delay, bool high_precision) {
    std::shared_ptr<ProviderBox> source;
    std::shared_ptr<TaskSlot> slot;
    webrtc::TaskQueueBase* identity;
    {
      std::lock_guard lock(mutex);
      identity = queue;
      if (!closed && !failed && !task_queue_provider_failed(*provider->value)) {
        source = provider;
        slot = std::make_shared<TaskSlot>(std::move(task), delayed);
        slots.push_back(slot);
        ++active;
      }
    }
    if (!source) { ProviderQueue::DestroyTask(identity, std::move(task)); return; }
    {
      PostingScope scope(this);
      auto ticket = std::make_unique<NativeTaskQueueTicket>(shared_from_this(), slot);
      const auto bits = delay.IsPlusInfinity() ? INT64_MAX : delay.us_or(INT64_MIN);
      task_queue_provider_post(*source->value, std::move(ticket), delayed, bits, high_precision);
    }
    bool wake = false;
    {
      std::lock_guard lock(mutex);
      failed = failed || task_queue_provider_failed(*source->value);
      if (!closed && !failed && !running && !wake_posted && Ready(true) != slots.end()) {
        wake = true;
        wake_posted = true;
      }
    }
    if (wake) {
      // Cancellation inside the first posting callback may release a FIFO-
      // gated successor. Schedule a drain notification rather than entering
      // that successor on the native caller's posting stack.
      PostingScope scope(this);
      task_queue_provider_post(*source->value,
          std::make_unique<NativeTaskQueueTicket>(shared_from_this(), nullptr), false, 0, false);
    }
    {
      std::lock_guard lock(mutex);
      failed = failed || task_queue_provider_failed(*source->value);
      --active;
      changed.notify_all();
    }
  }
  void Close() {
    std::vector<std::shared_ptr<TaskSlot>> pending;
    std::shared_ptr<ProviderBox> source;
    webrtc::TaskQueueBase* identity;
    {
      std::unique_lock lock(mutex);
      closed = true;
      changed.wait(lock, [&] { return !running && active == 0; });
      pending = std::move(slots);
      source = std::move(provider);
      identity = queue;
    }
    for (auto& slot : pending) ProviderQueue::DestroyTask(identity, std::move(slot->task));
    task_queue_provider_delete(*source->value);
    source.reset();
    std::lock_guard lock(mutex);
    queue = nullptr;
  }

  // Shared ownership is internal to tickets and posted-call frames. It never
  // extends the lifetime of the engine queue identity past Delete().
  std::shared_ptr<State> shared_from_this() { return owner.lock(); }
  std::weak_ptr<State> owner;
  mutable std::mutex mutex;
  std::condition_variable changed;
  webrtc::TaskQueueBase* queue = nullptr;
  std::shared_ptr<ProviderBox> provider;
  std::vector<std::shared_ptr<TaskSlot>> slots;
  std::deque<std::string> names;
  std::size_t active = 0;
  bool running = false;
  bool closed = false;
  bool failed = false;
  bool wake_posted = false;
};

namespace {
ProviderQueue::ProviderQueue(std::shared_ptr<QueueState> state) noexcept : state_(std::move(state)) { state_->queue = this; }
absl::string_view ProviderQueue::queue_name() const { return state_->Name(); }
bool ProviderQueue::Failed() const noexcept { return state_->Failed(); }
void ProviderQueue::Delete() { state_->Close(); delete this; }
void ProviderQueue::PostTaskImpl(absl::AnyInvocable<void() &&> task, const PostTaskTraits&, const webrtc::Location&) {
  auto state = state_;
  state->Post(std::move(task), false, webrtc::TimeDelta::Zero(), false);
}
void ProviderQueue::PostDelayedTaskImpl(absl::AnyInvocable<void() &&> task, webrtc::TimeDelta delay,
                                      const PostDelayedTaskTraits& traits, const webrtc::Location&) {
  auto state = state_;
  state->Post(std::move(task), true, delay, traits.high_precision);
}
class ProviderFactory final : public webrtc::TaskQueueFactory {
 public:
  explicit ProviderFactory(rust::Box<RustTaskQueueFactory> source) : source_(std::move(source)) {}
  std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter>
  CreateTaskQueue(absl::string_view name, Priority priority) const override {
    static const std::uint8_t empty = 0;
    const auto* data = name.empty() ? &empty : reinterpret_cast<const std::uint8_t*>(name.data());
    auto provider = task_queue_provider_create(*source_,
        rust::Slice<const std::uint8_t>(data, name.size()),
        static_cast<std::uint8_t>(priority));
    auto state = std::make_shared<QueueState>(std::move(provider));
    state->owner = state;
    return std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter>(new ProviderQueue(std::move(state)));
  }
 private:
  rust::Box<RustTaskQueueFactory> source_;
};
}  // namespace

NativeTaskQueueTicket::NativeTaskQueueTicket(std::shared_ptr<State> state, std::shared_ptr<Slot> slot) noexcept
    : state_(std::move(state)), slot_(std::move(slot)) {}
NativeTaskQueueTicket::~NativeTaskQueueTicket() { if (!submitted_) state_->Cancel(slot_); }
bool NativeTaskQueueTicket::Submit() noexcept { submitted_ = state_->Submit(slot_); return submitted_; }
bool task_queue_ticket_submit(std::unique_ptr<NativeTaskQueueTicket> ticket) noexcept { return ticket && ticket->Submit(); }
std::unique_ptr<webrtc::TaskQueueFactory> task_queue_provider_adapter(rust::Box<RustTaskQueueFactory> source) noexcept {
  return std::make_unique<ProviderFactory>(std::move(source));
}
bool task_queue_provider_product_failed(const webrtc::TaskQueueBase& queue) noexcept {
  return static_cast<const ProviderQueue&>(queue).Failed();
}
rust::Vec<std::uint8_t> task_queue_current_name() noexcept {
  rust::Vec<std::uint8_t> result;
  if (auto* queue = webrtc::TaskQueueBase::Current()) {
    for (auto value : queue->queue_name()) result.push_back(static_cast<std::uint8_t>(value));
  }
  return result;
}
std::size_t task_queue_current_identity() noexcept {
  return reinterpret_cast<std::uintptr_t>(webrtc::TaskQueueBase::Current());
}
std::uint8_t task_queue_current_post(std::size_t identity, rust::Box<RustTask> task) noexcept {
  auto* queue = webrtc::TaskQueueBase::Current();
  if (!queue || identity != reinterpret_cast<std::uintptr_t>(queue)) return 1;
  const bool provider = queue == current_provider;
  if (provider && task_queue_provider_product_failed(*queue)) return 2;
  queue->PostTask([task = std::move(task)]() mutable { run_task(std::move(task)); });
  return provider && task_queue_provider_product_failed(*queue) ? 2 : 0;
}
std::uint8_t task_queue_current_post_delayed(std::size_t identity, std::int64_t bits, bool high_precision, rust::Box<RustTask> task) noexcept {
  auto* queue = webrtc::TaskQueueBase::Current();
  if (!queue || identity != reinterpret_cast<std::uintptr_t>(queue)) return 1;
  const bool provider = queue == current_provider;
  if (provider && task_queue_provider_product_failed(*queue)) return 2;
  if (!provider && !task_queue_current_delay_supported(bits)) return 3;
  const auto delay = bits == INT64_MAX ? webrtc::TimeDelta::PlusInfinity()
      : bits == INT64_MIN ? webrtc::TimeDelta::MinusInfinity() : webrtc::TimeDelta::Micros(bits);
  auto callback = [task = std::move(task)]() mutable { run_task(std::move(task)); };
  if (high_precision) queue->PostDelayedHighPrecisionTask(std::move(callback), delay);
  else queue->PostDelayedTask(std::move(callback), delay);
  return provider && task_queue_provider_product_failed(*queue) ? 2 : 0;
}
}  // namespace pulsebeam::webrtc_sys
