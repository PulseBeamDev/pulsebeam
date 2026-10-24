#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/clock.h"
#include "pulsebeam-webrtc-sys/native/event_log.h"
#include "pulsebeam-webrtc-sys/native/task_queue_provider.h"
#include "pulsebeam-webrtc-sys/native/runtime_retirement.h"

#include <algorithm>
#include <atomic>
#include <limits>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "api/environment/environment.h"
#include "api/environment/environment_factory.h"
#include "api/field_trials.h"
#include "api/rtc_event_log_output_file.h"
#include "api/task_queue/default_task_queue_factory.h"
#include "api/task_queue/task_queue_base.h"
#include "api/task_queue/task_queue_factory.h"
#include "api/units/time_delta.h"
#include "api/units/timestamp.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "rtc_base/crypto_random.h"
#include "rtc_base/event.h"
#include "rtc_base/null_socket_server.h"
#include "rtc_base/synchronization/yield_policy.h"
#include "rtc_base/thread.h"
#include "rtc_base/time_utils.h"
#include "system_wrappers/include/clock.h"

#if defined(__linux__) && defined(__x86_64__) && !defined(__ANDROID__)
#include <cerrno>
#include <climits>
#include <new>
#include <pthread.h>
#include <semaphore.h>
#include <sys/mman.h>
#include <ucontext.h>
#include <unistd.h>
#include "third_party/libvpx/source/libvpx/vpx_util/vpx_thread.h"
#if __has_feature(address_sanitizer)
#define PULSEBEAM_ASAN_FIBERS 1
#include <sanitizer/common_interface_defs.h>
#endif
#endif

namespace pulsebeam::webrtc_sys {
namespace {

constexpr std::int64_t kNoDeadline = std::numeric_limits<std::int64_t>::min();
std::atomic_bool driver_active{false};
std::mutex hooks_mutex;
bool randomness_active = false;
std::size_t production_leases = 0;
thread_local std::vector<webrtc::TaskQueueBase*> suspended_queues;
// Stay within NTP era zero as well as the signed nanosecond clock. Native
// NTP seconds otherwise wrap in 2036 even though the microsecond clock fits.
constexpr std::int64_t kMaxTimeUs =
    (static_cast<std::int64_t>(std::numeric_limits<std::uint32_t>::max()) -
     2208988800LL) * 1000000 + 999999;

class SharedClock final : public webrtc::Clock {
 public:
  explicit SharedClock(std::shared_ptr<NativeManualClock::State> state) noexcept
      : state_(std::move(state)) {}

  webrtc::Timestamp CurrentTime() override;
  webrtc::NtpTime ConvertTimestampToNtpTime(
      webrtc::Timestamp timestamp) override;

 private:
  std::shared_ptr<NativeManualClock::State> state_;
};

class CooperativeTaskQueue;
thread_local CooperativeTaskQueue* current_cooperative_queue = nullptr;
thread_local webrtc::TaskQueueBase* controlled_driver_identity = nullptr;
thread_local CapturedDriverTasks::State* captured_driver_posts = nullptr;
class CooperativeScope final {
 public:
  explicit CooperativeScope(CooperativeTaskQueue* queue) : previous_(current_cooperative_queue) { current_cooperative_queue = queue; }
  ~CooperativeScope() { current_cooperative_queue = previous_; }
 private:
  CooperativeTaskQueue* previous_;
};

webrtc::TimeDelta NativeDelay(std::int64_t bits) {
  return bits == INT64_MAX ? webrtc::TimeDelta::PlusInfinity()
      : bits == INT64_MIN ? webrtc::TimeDelta::MinusInfinity() : webrtc::TimeDelta::Micros(bits);
}
void PostNativeDelay(webrtc::TaskQueueBase& queue, std::int64_t bits, bool high_precision, rust::Box<RustTask> task) {
  auto callback = [task = std::move(task)]() mutable { run_task(std::move(task)); };
  if (high_precision) queue.PostDelayedHighPrecisionTask(std::move(callback), NativeDelay(bits));
  else queue.PostDelayedTask(std::move(callback), NativeDelay(bits));
}

struct ScheduledTask {
  CooperativeTaskQueue* queue;
  bool delayed;
  absl::AnyInvocable<void() &&> task;
};

using TaskKey = std::pair<std::int64_t, std::uint64_t>;

class CooperativeTaskQueue final : public webrtc::TaskQueueBase {
 public:
  using WorkerScope = CurrentTaskQueueSetter;
  CooperativeTaskQueue(std::shared_ptr<NativeTaskQueueFactory::State> state,
                       std::string name,
                       webrtc::TaskQueueBase* identity = nullptr,
                       bool lifecycle = true) noexcept
      : state_(std::move(state)), name_(std::move(name)),
        identity_(identity ? identity : this), lifecycle_(lifecycle) {}

  webrtc::TaskQueueBase* identity() const { return identity_; }
  bool lifecycle() const { return lifecycle_; }
  // Called only on a newly created, unpublished public queue.
  void MarkCallerOwned() { lifecycle_ = false; }

  absl::string_view queue_name() const override { return name_; }
  void Delete() override;
  void Run(absl::AnyInvocable<void() &&> task) noexcept;
  void PostCaptured(absl::AnyInvocable<void() &&> task, CapturedDriverTasks::State& capture);

 protected:
  void PostTaskImpl(absl::AnyInvocable<void() &&> task,
                    const PostTaskTraits&,
                    const webrtc::Location&) override;
  void PostDelayedTaskImpl(absl::AnyInvocable<void() &&> task,
                           webrtc::TimeDelta delay,
                           const PostDelayedTaskTraits&,
                           const webrtc::Location&) override;

 private:
  std::shared_ptr<NativeTaskQueueFactory::State> state_;
  std::string name_;
  webrtc::TaskQueueBase* identity_;
  bool lifecycle_;
};

class CooperativeTaskQueueFactory final : public webrtc::TaskQueueFactory {
 public:
  explicit CooperativeTaskQueueFactory(
      std::shared_ptr<NativeTaskQueueFactory::State> state) noexcept
      : state_(std::move(state)) {}

  std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter>
  CreateTaskQueue(absl::string_view name, Priority) const override {
    return std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter>(
        new CooperativeTaskQueue(state_, std::string(name)));
  }

 private:
  std::shared_ptr<NativeTaskQueueFactory::State> state_;
};

class SharedTaskQueueFactory final : public webrtc::TaskQueueFactory {
 public:
  explicit SharedTaskQueueFactory(std::shared_ptr<NativeTaskQueueFactory::State> state) : state_(std::move(state)) {}
  std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter>
  CreateTaskQueue(absl::string_view name, Priority priority) const override;
 private:
  std::shared_ptr<NativeTaskQueueFactory::State> state_;
};

class EnvironmentTaskQueueFactory final : public webrtc::TaskQueueFactory {
 public:
  EnvironmentTaskQueueFactory(webrtc::Environment environment,
      std::shared_ptr<NativeProductionLease> production)
      : production_(std::move(production)), environment_(std::move(environment)) {}
  std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter>
  CreateTaskQueue(absl::string_view name, Priority priority) const override {
    return environment_.task_queue_factory().CreateTaskQueue(name, priority);
  }
 private:
  std::shared_ptr<NativeProductionLease> production_;
  webrtc::Environment environment_;
};

class SeededRandom final : public webrtc::RandomGenerator {
 public:
  explicit SeededRandom(std::uint64_t seed) noexcept : state_(seed) {}

  bool Generate(void* output, std::size_t length) override {
    std::lock_guard lock(mutex_);
    auto* bytes = static_cast<std::uint8_t*>(output);
    while (length != 0) {
      std::uint64_t value = Next();
      const std::size_t count = std::min(length, sizeof(value));
      for (std::size_t index = 0; index < count; ++index) {
        bytes[index] = static_cast<std::uint8_t>(value >> (index * 8));
      }
      bytes += count;
      length -= count;
    }
    return true;
  }

 private:
  std::uint64_t Next() noexcept {
    std::uint64_t value = (state_ += 0x9e3779b97f4a7c15ULL);
    value = (value ^ (value >> 30)) * 0xbf58476d1ce4e5b9ULL;
    value = (value ^ (value >> 27)) * 0x94d049bb133111ebULL;
    return value ^ (value >> 31);
  }

  std::mutex mutex_;
  std::uint64_t state_;
};

webrtc::TaskQueueFactory::Priority ToPriority(std::uint8_t priority) noexcept {
  switch (priority) {
    case 1:
      return webrtc::TaskQueueFactory::Priority::kHigh;
    case 2:
      return webrtc::TaskQueueFactory::Priority::kVideo;
    case 3:
      return webrtc::TaskQueueFactory::Priority::kAudio;
    case 4:
      return webrtc::TaskQueueFactory::Priority::kLow;
    default:
      return webrtc::TaskQueueFactory::Priority::kNormal;
  }
}

}  // namespace

struct NativeManualClock::State {
  explicit State(std::int64_t initial_time_us) noexcept
      : clock(initial_time_us) {}

  webrtc::SimulatedClock clock;
  std::mutex advance_mutex;
  std::mutex factories_mutex;
  std::vector<std::weak_ptr<NativeTaskQueueFactory::State>> factories;
  std::atomic<std::uint64_t> next_sequence{0};
  std::mutex retirement_mutex;
  std::weak_ptr<NativeRuntimeRetirement> retirement;
  void WakeRetirement() {
    std::shared_ptr<NativeRuntimeRetirement> owner;
    {
      std::lock_guard lock(retirement_mutex);
      owner = retirement.lock();
    }
    if (owner) owner->Wake();
  }
};

struct NativeTaskQueueFactory::State {
  State() noexcept : default_factory(webrtc::CreateDefaultTaskQueueFactory()) {}
  explicit State(rust::Box<RustTaskQueueFactory> provider) noexcept
      : default_factory(task_queue_provider_adapter(std::move(provider))), provider_backed(true) {}
  explicit State(std::shared_ptr<NativeManualClock::State> clock_state) noexcept
      : clock(std::move(clock_state)) {}
  // Metadata for an exact SDK-owned default delegate, never a bare factory.
  explicit State(std::nullptr_t) noexcept {}

  bool cooperative() const noexcept { return clock != nullptr; }

  std::shared_ptr<NativeManualClock::State> clock;
  std::unique_ptr<webrtc::TaskQueueFactory> default_factory;
  bool provider_backed = false;
  mutable std::mutex mutex;
  const std::thread::id creator = std::this_thread::get_id();
  std::map<TaskKey, ScheduledTask> tasks;
  std::map<CooperativeTaskQueue*, std::size_t> running;
  std::map<CooperativeTaskQueue*, bool> deleted;
};

struct NativeTaskQueue::State {
  explicit State(std::unique_ptr<webrtc::TaskQueueBase,
                                 webrtc::TaskQueueDeleter> queue, bool provider_backed, bool cooperative,
                 std::optional<webrtc::Environment> environment = std::nullopt,
                 std::shared_ptr<NativeProductionLease> production = nullptr) noexcept
      : production(std::move(production)), environment(std::move(environment)), queue(std::move(queue)),
        provider_backed(provider_backed), cooperative(cooperative) {}

  // Outlive queue deletion, including provider callbacks and task retirement.
  std::shared_ptr<NativeProductionLease> production;
  std::optional<webrtc::Environment> environment;
  std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter> queue;
  const bool provider_backed;
  const bool cooperative;
};

webrtc::TaskQueueBase* task_queue_for_blocking_consumer(const NativeTaskQueue& queue) noexcept {
  const auto& s = *queue.state();
  if (s.provider_backed || s.cooperative || s.queue->IsCurrent()) return nullptr;
  return s.queue.get();
}

struct NativeEnvironment::State {
  explicit State(webrtc::Environment value, FfiClockTimeBounds bounds = {},
      std::shared_ptr<NativeTaskQueueFactory::State> queues = nullptr) noexcept
      : queues(std::move(queues)), environment(std::move(value)), clock_bounds(bounds) {}

  // Exact selected scheduler metadata, not inferred from a clock or queue name.
  // The SDK Environment (including a bound scope ticket) dies before this alias.
  std::shared_ptr<NativeTaskQueueFactory::State> queues;
  webrtc::Environment environment;
  // Immutable adapter-enforced evidence, not a sampled current timestamp.
  FfiClockTimeBounds clock_bounds;
};

struct NativeBuiltinVp8Execution::State {
  std::shared_ptr<NativeTaskQueueFactory::State> queues;
  std::shared_ptr<NativeRuntimeRetirement> owner;
  webrtc::Environment environment;
  int cores = 0;
  bool construction_failed = false;
};
namespace {
thread_local NativeBuiltinVp8Execution::State* current_builtin_vp8 = nullptr;
}
NativeBuiltinVp8Execution::NativeBuiltinVp8Execution(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeBuiltinVp8Execution::~NativeBuiltinVp8Execution() = default;
void NativeBuiltinVp8Execution::SetCores(int cores) noexcept {
  RTC_CHECK(state_->queues->creator == std::this_thread::get_id());
  // Retain the actual SDK Settings input, including invalid values. Validation
  // and native thread-count/complexity policy remain the SDK's responsibility.
  state_->cores = cores;
}
std::shared_ptr<NativeBuiltinVp8Execution> builtin_vp8_execution(
    const NativeEnvironment& selected, const webrtc::Environment& actual,
    bool& valid) noexcept {
  valid = true;
  const auto& queues = selected.state()->queues;
  if (!queues || !queues->cooperative()) return nullptr;
  auto selected_sdk = selected.environment();
  std::shared_ptr<NativeRuntimeRetirement> owner;
  {
    auto& registry = runtime_retirement_internal::registry();
    std::lock_guard lock(registry.mutex);
    // Unlike general retirement lookup, clock association is not sufficient
    // evidence that this exact Environment owns a runtime scope ticket.
    auto found = registry.factories.find(&selected_sdk.task_queue_factory());
    if (found != registry.factories.end()) owner = found->second.lock();
  }
  if (!owner) return nullptr;
  valid = queues->creator == std::this_thread::get_id() &&
      &actual.task_queue_factory() == &selected_sdk.task_queue_factory() &&
      &actual.clock() == &selected_sdk.clock();
  {
    std::lock_guard lock(queues->clock->retirement_mutex);
    valid = valid && queues->clock->retirement.lock() == owner;
  }
  if (!valid) return nullptr;
  return std::make_shared<NativeBuiltinVp8Execution>(
      std::make_unique<NativeBuiltinVp8Execution::State>(queues, std::move(owner), actual, 0));
}
NativeBuiltinVp8Scope::NativeBuiltinVp8Scope(
    const std::shared_ptr<NativeBuiltinVp8Execution>& execution) noexcept
    : previous_(current_builtin_vp8) {
  if (execution) RTC_CHECK(execution->state_->queues->creator == std::this_thread::get_id());
  current_builtin_vp8 = execution ? execution->state_.get() : nullptr;
}
NativeBuiltinVp8Scope::~NativeBuiltinVp8Scope() { current_builtin_vp8 = previous_; }

struct CapturedDriverTasks::State {
  struct Task {
    explicit Task(absl::AnyInvocable<void() &&> body) : body(std::move(body)) {}
    void Run() {
      auto active = std::move(body);
      if (!active) return;
      running = true;
      std::move(active)();
      active = nullptr;  // Retire SDK captures before declaring completion.
      running = false;
    }
    absl::AnyInvocable<void() &&> body;
    std::weak_ptr<NativeTaskQueueFactory::State> queue;
    TaskKey key;
    bool running = false;
  };
  webrtc::Thread* thread = nullptr;
  std::vector<std::shared_ptr<Task>> tasks;
};
CapturedDriverTasks::CapturedDriverTasks() noexcept : state_(std::make_unique<State>()) {}
CapturedDriverTasks::~CapturedDriverTasks() { Cancel(); }
bool CapturedDriverTasks::Capture(webrtc::Thread* thread, absl::AnyInvocable<bool()> request) noexcept {
  Cancel();
  if (thread != controlled_driver_identity) return request();
  state_->thread = thread;
  struct Scope {
    State* previous;
    ~Scope() { captured_driver_posts = previous; }
  } scope{captured_driver_posts};
  captured_driver_posts = state_.get();
  const bool accepted = request();
  if (accepted && !state_->tasks.empty()) {
    auto continuation = state_->tasks.back();
    state_->tasks.clear();  // Earlier posts retain their queued task bodies.
    state_->tasks.push_back(std::move(continuation));
  } else {
    state_->tasks.clear();  // Rejection must not cancel unrelated SDK work.
  }
  return accepted;
}
void CapturedDriverTasks::Cancel() noexcept {
  for (const auto& task : state_->tasks) {
    // This primitive is caller-sequence-only. Running work cannot be retired
    // by clearing its already-moved function; never assert otherwise.
    RTC_CHECK(!task->running);
    task->body = nullptr;  // Destroy SDK captures outside the queue lock.
    if (auto queue = task->queue.lock()) {
      std::lock_guard lock(queue->mutex);
      queue->tasks.erase(task->key);  // No abandoned no-op queue entries.
    }
  }
  state_->tasks.clear();
}
bool CapturedDriverTasks::retired() const noexcept {
  return std::all_of(state_->tasks.begin(), state_->tasks.end(), [](const auto& task) {
    return !task->body && !task->running;
  });
}

namespace {
std::shared_ptr<NativeTaskQueueFactory::State> MakeCooperativeState(
    const std::shared_ptr<NativeManualClock::State>& clock) {
  auto state = std::make_shared<NativeTaskQueueFactory::State>(clock);
  std::lock_guard lock(clock->factories_mutex);
  // A clock is one cooperative dispatch domain. Reject foreign creators
  // before registration, otherwise one inaccessible factory stalls PumpClock
  // and leaves its ready deadline permanently visible to the caller.
  for (const auto& weak : clock->factories) {
    if (auto existing = weak.lock(); existing && existing->creator != state->creator)
      return nullptr;
  }
  clock->factories.push_back(state);
  return state;
}

std::size_t PumpClock(const std::shared_ptr<NativeManualClock::State>& clock,
                      std::size_t budget,
                      webrtc::TaskQueueBase* excluded = nullptr,
                      bool lifecycle_only = false);
std::int64_t ClockDeadline(
    const std::shared_ptr<NativeManualClock::State>& clock);

#include "pulsebeam-webrtc-sys/native/vpx_execution.inc"

// Intercept thread posts rather than using upstream's elapsed-time pump,
// which has no finite dispatch bound at frozen virtual time.
class ControlledThread final : public webrtc::Thread {
 public:
  explicit ControlledThread(
      std::shared_ptr<NativeTaskQueueFactory::State> state)
      : webrtc::Thread(std::make_unique<webrtc::NullSocketServer>()),
        queue_(new CooperativeTaskQueue(std::move(state), "peer", this)) {}
  ~ControlledThread() override {
    Stop();
    ClearPending();
  }
  void ClearPending() {
    if (auto* queue = std::exchange(queue_, nullptr)) queue->Delete();
  }
 protected:
  void BlockingCallImpl(webrtc::FunctionView<void()> functor,
                        const webrtc::Location& location) override {
    // IsCurrent is a ThreadManager identity check, not a task-queue check.
    // A distinct cooperative queue can run on this same caller thread. Enter
    // the requested SDK thread sequence for its inline invocation, then restore
    // that caller queue. Keep native invoke policy and off-thread posting intact.
    if (IsCurrent() && webrtc::TaskQueueBase::Current() != this) {
      CurrentTaskQueueSetter current(this);
      webrtc::Thread::BlockingCallImpl(functor, location);
      return;
    }
    webrtc::Thread::BlockingCallImpl(functor, location);
  }
  void PostTaskImpl(absl::AnyInvocable<void() &&> task,
                    const PostTaskTraits& traits,
                    const webrtc::Location& location) override {
    if (!queue_ || IsQuitting()) return;
    if (captured_driver_posts && captured_driver_posts->thread == this) {
      queue_->PostCaptured(std::move(task), *captured_driver_posts);
    } else {
      queue_->PostTask(std::move(task), location);
    }
  }
  void PostDelayedTaskImpl(absl::AnyInvocable<void() &&> task,
                           webrtc::TimeDelta delay,
                           const PostDelayedTaskTraits& traits,
                           const webrtc::Location& location) override {
    if (queue_ && !IsQuitting()) queue_->PostDelayedTask(std::move(task), delay, location);
  }
 private:
  CooperativeTaskQueue* queue_;
};
}  // namespace

struct NativeDriverThread::State : webrtc::YieldInterface {
  struct Clock final : webrtc::ClockInterface {
    explicit Clock(std::shared_ptr<NativeManualClock::State> value)
        : state(std::move(value)) {}
    std::int64_t TimeNanos() const override {
      const auto micros = state->clock.TimeInMicroseconds();
      RTC_CHECK_LE(micros, kMaxTimeUs);
      return micros * 1000;
    }
    std::shared_ptr<NativeManualClock::State> state;
  };

  State(std::unique_ptr<webrtc::Thread> value,
        std::shared_ptr<NativeManualClock::State> clock_state,
        bool seeded = false)
      : thread(std::move(value)), clock(std::move(clock_state)), seeded(seeded) {
    webrtc::SetClockForTesting(&clock);
    yield_policy = std::make_unique<webrtc::ScopedYieldPolicy>(this);
    {
      std::lock_guard lock(clock.state->retirement_mutex);
      clock.state->retirement = retirement;
    }
#if defined(__linux__) && defined(__x86_64__) && !defined(__ANDROID__)
    vpx_runtime = std::make_unique<NativeVpxRuntime>(retirement);
#endif
  }
  void YieldExecution() override {
    // Supported Event waits have finite engine-owned ready dependencies.
    // Caller queues are never native lifecycle dependencies: pumping them
    // here could hang even after the event was signaled if they self-repost.
    // Never move time or reenter a suspended queue.
    suspended_queues.push_back(webrtc::TaskQueueBase::Current());
    PumpClock(clock.state, std::numeric_limits<std::size_t>::max(),
              webrtc::TaskQueueBase::Current(), true);
    suspended_queues.pop_back();
  }
  ~State() {
    RTC_CHECK(thread->IsCurrent());
    thread->Quit();
    // Pending captures can release native peers/codec queues. Destroy them
    // while the driver is still current and cooperative lifecycle yields work.
    static_cast<ControlledThread*>(thread.get())->ClearPending();
    // Keep the actual native driver, yield policy and global hooks alive until
    // every SDK Environment alias and installed affine resource has retired.
    // Only native-ready lifecycle work is dispatched; time never advances.
    for (;;) {
      const auto generation = retirement->generation();
      retirement->Drain();
      if (retirement->empty()) break;
      const auto progress = PumpClock(clock.state,
          std::numeric_limits<std::size_t>::max(), nullptr, true);
      retirement->Drain();
      if (retirement->empty()) break;
      if (progress == 0) retirement->Wait(generation);
    }
#if defined(__linux__) && defined(__x86_64__) && !defined(__ANDROID__)
    vpx_runtime.reset();
#endif
    controlled_driver_identity = nullptr;
    thread->UnwrapCurrent();
    thread.reset();
    yield_policy.reset();
    std::lock_guard lock(hooks_mutex);
    webrtc::SetClockForTesting(nullptr);
    if (seeded) {
      webrtc::SetDefaultRandomGenerator();
      randomness_active = false;
    }
    driver_active.store(false);
  }

  std::unique_ptr<webrtc::Thread> thread;
  Clock clock;
  bool seeded;
  std::shared_ptr<NativeRuntimeRetirement> retirement =
      std::make_shared<NativeRuntimeRetirement>();
  std::unique_ptr<webrtc::ScopedYieldPolicy> yield_policy;
#if defined(__linux__) && defined(__x86_64__) && !defined(__ANDROID__)
  std::unique_ptr<NativeVpxRuntime> vpx_runtime;
#endif
};

struct NativeThread::State {
  explicit State(std::unique_ptr<webrtc::Thread> value) noexcept
      : owned(std::move(value)), thread(owned.get()) {}
  explicit State(webrtc::Thread* value) noexcept : thread(value) {}

  std::unique_ptr<webrtc::Thread> owned;
  webrtc::Thread* thread;
};

webrtc::Timestamp SharedClock::CurrentTime() {
  return state_->clock.CurrentTime();
}

webrtc::NtpTime SharedClock::ConvertTimestampToNtpTime(
    webrtc::Timestamp timestamp) {
  // Keep finite-unit representation at the boundary; conversion is SDK-owned.
  if (!timestamp.IsFinite()) return webrtc::NtpTime();
  return state_->clock.ConvertTimestampToNtpTime(timestamp);
}

void CooperativeTaskQueue::PostCaptured(absl::AnyInvocable<void() &&> task, CapturedDriverTasks::State& group) {
  std::lock_guard lock(state_->mutex);
  if (state_->deleted[this]) return;
  auto capture = std::make_shared<CapturedDriverTasks::State::Task>(std::move(task));
  capture->queue = state_;
  capture->key = TaskKey{state_->clock->clock.TimeInMicroseconds(), state_->clock->next_sequence++};
  group.tasks.push_back(capture);
  state_->tasks.emplace(capture->key, ScheduledTask{this, false, [capture] { capture->Run(); }});
  state_->clock->WakeRetirement();
}

void CooperativeTaskQueue::PostTaskImpl(absl::AnyInvocable<void() &&> task,
                                        const PostTaskTraits&,
                                        const webrtc::Location&) {
  std::lock_guard lock(state_->mutex);
  if (state_->deleted[this]) {
    return;
  }
  const std::int64_t deadline = state_->clock->clock.TimeInMicroseconds();
  state_->tasks.emplace(TaskKey{deadline, state_->clock->next_sequence++},
                        ScheduledTask{this, false, std::move(task)});
  state_->clock->WakeRetirement();
}

void CooperativeTaskQueue::PostDelayedTaskImpl(
    absl::AnyInvocable<void() &&> task,
    webrtc::TimeDelta delay,
    const PostDelayedTaskTraits&,
    const webrtc::Location&) {
  std::lock_guard lock(state_->mutex);
  if (state_->deleted[this]) {
    return;
  }
  const std::int64_t now = state_->clock->clock.TimeInMicroseconds();
  const std::int64_t delta = std::max<std::int64_t>(0, delay.us());
  if (delta > kMaxTimeUs - now) return;
  const std::int64_t deadline = now + delta;
  state_->tasks.emplace(TaskKey{deadline, state_->clock->next_sequence++},
                        ScheduledTask{this, true, std::move(task)});
  state_->clock->WakeRetirement();
}

void CooperativeTaskQueue::Run(absl::AnyInvocable<void() &&> task) noexcept {
  // Nested pumping enters another task, not another frame of the suspended
  // encoder. Only that task's own builtin lifecycle guard may lend provenance.
  NativeBuiltinVp8Scope encoder_scope(nullptr);
  CooperativeScope scope(this);
  CurrentTaskQueueSetter current(identity_);
  std::move(task)();
  task = nullptr;
}

void CooperativeTaskQueue::Delete() {
  RTC_CHECK(state_->creator == std::this_thread::get_id());
  bool destroy = false;
  std::vector<absl::AnyInvocable<void() &&>> immediate;
  std::vector<absl::AnyInvocable<void() &&>> delayed;
  {
    std::unique_lock lock(state_->mutex);
    state_->deleted[this] = true;
    for (auto iterator = state_->tasks.begin();
         iterator != state_->tasks.end();) {
      if (iterator->second.queue != this) {
        ++iterator;
        continue;
      }
      auto& destination = iterator->second.delayed ? delayed : immediate;
      destination.push_back(std::move(iterator->second.task));
      iterator = state_->tasks.erase(iterator);
    }
    destroy = state_->running[this] == 0;
    if (destroy) {
      state_->running.erase(this);
      state_->deleted.erase(this);
    }
  }
  {
    CooperativeScope scope(this);
    CurrentTaskQueueSetter current(identity_);
    immediate.clear();
  }
  delayed.clear();
  // Retiring a queue from inside its dispatch never waits for that dispatch.
  if (destroy) delete this;
}

NativeManualClock::NativeManualClock(std::shared_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeManualClock::~NativeManualClock() = default;
const std::shared_ptr<NativeManualClock::State>& NativeManualClock::state()
    const noexcept {
  return state_;
}

NativeTaskQueueFactory::NativeTaskQueueFactory(
    std::shared_ptr<State> state, std::optional<webrtc::Environment> environment,
    std::shared_ptr<NativeProductionLease> production) noexcept
    : production_lease_(std::move(production)), environment_(std::move(environment)), state_(std::move(state)) {}
NativeTaskQueueFactory::~NativeTaskQueueFactory() = default;
const std::shared_ptr<NativeTaskQueueFactory::State>&
NativeTaskQueueFactory::state() const noexcept {
  return state_;
}

const std::optional<webrtc::Environment>& NativeTaskQueueFactory::environment() const noexcept {
  return environment_;
}

const std::shared_ptr<NativeProductionLease>& NativeTaskQueueFactory::production_lease() const noexcept {
  return production_lease_;
}

NativeTaskQueue::NativeTaskQueue(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeTaskQueue::~NativeTaskQueue() = default;
const std::unique_ptr<NativeTaskQueue::State>& NativeTaskQueue::state()
    const noexcept {
  return state_;
}

NativeEnvironment::NativeEnvironment(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeEnvironment::~NativeEnvironment() = default;
const std::unique_ptr<NativeEnvironment::State>& NativeEnvironment::state()
    const noexcept {
  return state_;
}
webrtc::Environment NativeEnvironment::environment() const noexcept {
  return state_->environment;
}

NativeRandomnessLease::~NativeRandomnessLease() {
  std::lock_guard lock(hooks_mutex);
  webrtc::SetDefaultRandomGenerator();
  randomness_active = false;
}

NativeProductionLease::~NativeProductionLease() {
  std::lock_guard lock(hooks_mutex);
  --production_leases;
}
std::unique_ptr<NativeProductionLease> new_production_lease() noexcept {
  std::lock_guard lock(hooks_mutex);
  if (driver_active.load()) return nullptr;
  ++production_leases;
  return std::make_unique<NativeProductionLease>();
}

ReadinessSignal::ReadinessSignal(rust::Box<RustReadiness> readiness) noexcept
    : readiness_(std::move(readiness)) {}
ReadinessSignal::~ReadinessSignal() = default;
void ReadinessSignal::Signal() const noexcept { notify_readiness(*readiness_); }
NativeReadiness::NativeReadiness(rust::Box<RustReadiness> readiness) noexcept
    : signal_(std::make_shared<ReadinessSignal>(std::move(readiness))) {}
NativeReadiness::~NativeReadiness() = default;
std::shared_ptr<ReadinessSignal> NativeReadiness::signal() const noexcept {
  return signal_;
}
std::unique_ptr<NativeReadiness> new_readiness(
    rust::Box<RustReadiness> readiness) noexcept {
  return std::make_unique<NativeReadiness>(std::move(readiness));
}

NativeThread::NativeThread(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeDriverThread::NativeDriverThread(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeDriverThread::~NativeDriverThread() = default;
const std::unique_ptr<NativeDriverThread::State>& NativeDriverThread::state()
    const noexcept {
  return state_;
}
webrtc::Thread* NativeDriverThread::thread() const noexcept {
  return state_ ? state_->thread.get() : nullptr;
}
bool NativeDriverThread::uses_clock(const NativeManualClock& clock) const noexcept {
  return state_ && state_->clock.state == clock.state();
}
NativeThread::~NativeThread() = default;
const std::unique_ptr<NativeThread::State>& NativeThread::state()
    const noexcept {
  return state_;
}
webrtc::Thread* NativeThread::thread() const noexcept {
  return state_ ? state_->thread : nullptr;
}

std::unique_ptr<NativeManualClock> new_manual_clock(
    std::int64_t initial_time_us) noexcept {
  if (initial_time_us < 0 || initial_time_us > kMaxTimeUs) return nullptr;
  return std::make_unique<NativeManualClock>(
      std::make_shared<NativeManualClock::State>(initial_time_us));
}

std::int64_t manual_clock_time_us(const NativeManualClock& clock) noexcept {
  return clock.state()->clock.TimeInMicroseconds();
}

bool advance_manual_clock(const NativeManualClock& clock,
                          std::int64_t delta_us) noexcept {
  std::lock_guard lock(clock.state()->advance_mutex);
  const std::int64_t now = clock.state()->clock.TimeInMicroseconds();
  if (delta_us < 0 ||
      delta_us > kMaxTimeUs - now) {
    return false;
  }
  clock.state()->clock.AdvanceTimeMicroseconds(delta_us);
  return true;
}

std::int64_t system_clock_time_us() noexcept {
  return webrtc::Clock::GetRealTimeClock()->TimeInMicroseconds();
}

std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter>
SharedTaskQueueFactory::CreateTaskQueue(absl::string_view name, Priority priority) const {
  return state_->default_factory->CreateTaskQueue(name, priority);
}

std::unique_ptr<NativeTaskQueueFactory> new_provider_task_queue_factory(rust::Box<RustTaskQueueFactory> provider) noexcept {
  return std::make_unique<NativeTaskQueueFactory>(
      std::make_shared<NativeTaskQueueFactory::State>(std::move(provider)));
}

std::unique_ptr<NativeTaskQueueFactory>
new_default_task_queue_factory() noexcept {
  return std::make_unique<NativeTaskQueueFactory>(
      std::make_shared<NativeTaskQueueFactory::State>());
}

std::unique_ptr<NativeTaskQueueFactory> new_cooperative_task_queue_factory(
    const NativeManualClock& clock) noexcept {
  auto state = MakeCooperativeState(clock.state());
  if (!state) return nullptr;
  return std::make_unique<NativeTaskQueueFactory>(std::move(state));
}

bool task_queue_factory_is_cooperative(
    const NativeTaskQueueFactory& factory) noexcept {
  return factory.state()->cooperative();
}

bool task_queue_factory_uses_clock(const NativeTaskQueueFactory& factory,
                                   const NativeManualClock& clock) noexcept {
  return factory.state()->clock == clock.state();
}

std::unique_ptr<NativeTaskQueue> create_task_queue(
    const NativeTaskQueueFactory& factory,
    rust::Slice<const std::uint8_t> name,
    std::uint8_t priority) noexcept {
  if (!task_queue_factory_is_current(factory)) return nullptr;
  auto production = factory.production_lease();
  if (!factory.state()->cooperative() && !production) {
    production = new_production_lease();
    if (!production) return nullptr;
  }
  const absl::string_view queue_name(reinterpret_cast<const char*>(name.data()), name.size());
  std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter> queue;
  if (factory.environment()) {
    queue = factory.environment()->task_queue_factory().CreateTaskQueue(queue_name, ToPriority(priority));
    if (queue && factory.state()->cooperative()) {
      // Identity-bound cooperative metadata proves the returned concrete
      // adapter. Keep its scheduler state, but public work is not lifecycle work.
      static_cast<CooperativeTaskQueue&>(*queue).MarkCallerOwned();
    }
  } else if (factory.state()->cooperative()) {
    // This entry point creates public caller queues, unlike the factory
    // installed into a native Environment for engine-owned media work.
    queue.reset(new CooperativeTaskQueue(
        factory.state(), std::string(queue_name), nullptr, false));
  } else {
    queue = factory.state()->default_factory->CreateTaskQueue(
        queue_name, ToPriority(priority));
  }
  if (!queue || (factory.state()->provider_backed && task_queue_provider_product_failed(*queue))) {
    return nullptr;
  }
  return std::make_unique<NativeTaskQueue>(
      std::make_unique<NativeTaskQueue::State>(std::move(queue), factory.state()->provider_backed,
          factory.state()->cooperative(), factory.environment(), std::move(production)));
}

bool post_task(const NativeTaskQueue& queue,
               rust::Box<RustTask> task) noexcept {
  if (!queue.state() || !queue.state()->queue) {
    return false;
  }
  if (task_queue_has_failed(queue)) return false;
  queue.state()->queue->PostTask(
      [task = std::move(task)]() mutable { run_task(std::move(task)); });
  return !task_queue_has_failed(queue);
}

bool post_delayed_task(const NativeTaskQueue& queue,
                       std::int64_t delay_us,
                       rust::Box<RustTask> task) noexcept {
  if (!queue.state() || !queue.state()->queue || delay_us < 0) {
    return false;
  }
  if (delay_us == INT64_MAX) return false; // Duration is finite, not infinity.
  return post_native_delayed_task(queue, delay_us, false, std::move(task)) == 0;
}

std::size_t task_queue_identity(const NativeTaskQueue& queue) noexcept {
  return reinterpret_cast<std::uintptr_t>(queue.state()->queue.get());
}
bool task_queue_is_current(const NativeTaskQueue& queue) noexcept { return queue.state()->queue->IsCurrent(); }
rust::Vec<std::uint8_t> task_queue_name(const NativeTaskQueue& queue) noexcept {
  rust::Vec<std::uint8_t> result;
  for (auto byte : queue.state()->queue->queue_name()) result.push_back(static_cast<std::uint8_t>(byte));
  return result;
}
bool task_queue_has_failed(const NativeTaskQueue& queue) noexcept {
  return queue.state()->provider_backed && task_queue_provider_product_failed(*queue.state()->queue);
}
std::uint8_t post_native_delayed_task(const NativeTaskQueue& queue, std::int64_t bits, bool high_precision,
                              rust::Box<RustTask> task) noexcept {
  if (task_queue_has_failed(queue)) return 1;
  if (!queue.state()->provider_backed && queue.state()->cooperative &&
      (bits == INT64_MIN || bits == INT64_MAX)) return 2;
  PostNativeDelay(*queue.state()->queue, bits, high_precision, std::move(task));
  return task_queue_has_failed(queue) ? 1 : 0;
}

bool task_queue_current_delay_supported(std::int64_t bits) noexcept {
  auto* queue = webrtc::TaskQueueBase::Current();
  if (!queue) return false;
  if (queue == controlled_driver_identity ||
      (current_cooperative_queue && current_cooperative_queue->identity() == queue))
    return bits != INT64_MIN && bits != INT64_MAX;
  return true;  // SDK backends own their delay arithmetic and rounding.
}

namespace {
using QueueState = NativeTaskQueueFactory::State;

std::vector<std::shared_ptr<QueueState>> ClockFactories(
    const std::shared_ptr<NativeManualClock::State>& clock) {
  std::vector<std::shared_ptr<QueueState>> result;
  std::lock_guard lock(clock->factories_mutex);
  for (auto it = clock->factories.begin(); it != clock->factories.end();) {
    if (auto state = it->lock()) {
      result.push_back(std::move(state));
      ++it;
    } else {
      it = clock->factories.erase(it);
    }
  }
  return result;
}

std::size_t PumpStates(const std::vector<std::shared_ptr<QueueState>>& states,
                       std::size_t budget,
                       webrtc::TaskQueueBase* excluded = nullptr,
                       bool lifecycle_only = false) {
  for (const auto& state : states) {
    if (state->creator != std::this_thread::get_id()) return 0;
  }
  std::size_t count = 0;
  while (count < budget) {
    std::shared_ptr<QueueState> selected;
    TaskKey key{std::numeric_limits<std::int64_t>::max(),
                std::numeric_limits<std::uint64_t>::max()};
    for (const auto& state : states) {
      std::lock_guard lock(state->mutex);
      const auto now = state->clock->clock.TimeInMicroseconds();
      for (auto it = state->tasks.begin();
           it != state->tasks.end() && it->first.first <= now; ++it) {
        auto* queue = it->second.queue;
        if ((lifecycle_only && !queue->lifecycle()) ||
            state->running[queue] != 0 || queue->identity() == excluded ||
            std::find(suspended_queues.begin(), suspended_queues.end(),
                      queue->identity()) != suspended_queues.end()) continue;
        if (it->first < key) {
          selected = state;
          key = it->first;
        }
        break;
      }
    }
    if (!selected) break;
    CooperativeTaskQueue* queue;
    absl::AnyInvocable<void() &&> task;
    {
      std::lock_guard lock(selected->mutex);
      auto it = selected->tasks.find(key);
      queue = it->second.queue;
      ++selected->running[queue];
      task = std::move(it->second.task);
      selected->tasks.erase(it);
    }
    queue->Run(std::move(task));
    bool destroy = false;
    {
      std::lock_guard lock(selected->mutex);
      if (--selected->running[queue] == 0 && selected->deleted[queue]) {
        selected->running.erase(queue);
        selected->deleted.erase(queue);
        destroy = true;
      }
    }
    if (destroy) delete queue;
    ++count;
  }
  return count;
}

std::size_t PumpClock(const std::shared_ptr<NativeManualClock::State>& clock,
                      std::size_t budget, webrtc::TaskQueueBase* excluded,
                      bool lifecycle_only) {
  return PumpStates(ClockFactories(clock), budget, excluded, lifecycle_only);
}

std::int64_t ClockDeadline(
    const std::shared_ptr<NativeManualClock::State>& clock) {
  std::int64_t deadline = kNoDeadline;
  for (const auto& state : ClockFactories(clock)) {
    std::lock_guard lock(state->mutex);
    if (!state->tasks.empty()) {
      const auto value = state->tasks.begin()->first.first;
      if (deadline == kNoDeadline || value < deadline) deadline = value;
    }
  }
  return deadline;
}
}  // namespace

bool task_queue_factory_is_current(
    const NativeTaskQueueFactory& factory) noexcept {
  return !factory.state()->cooperative() ||
         factory.state()->creator == std::this_thread::get_id();
}

std::size_t pump_ready_tasks(const NativeTaskQueueFactory& factory,
                            std::size_t budget) noexcept {
  if (!factory.state()->cooperative()) return 0;
  return PumpStates({factory.state()}, budget);
}

std::size_t run_ready_tasks(const NativeTaskQueueFactory& factory) noexcept {
  return pump_ready_tasks(factory, 1024);
}

std::int64_t next_task_deadline_us(
    const NativeTaskQueueFactory& factory) noexcept {
  const auto& state = factory.state();
  if (!state->cooperative()) {
    return kNoDeadline;
  }
  std::lock_guard lock(state->mutex);
  return state->tasks.empty() ? kNoDeadline : state->tasks.begin()->first.first;
}

namespace {
class RustEventLogOutputAdapter final : public webrtc::RtcEventLogOutput {
 public:
  explicit RustEventLogOutputAdapter(rust::Box<RustEventLogOutput> output)
      : output_(std::move(output)) {}
  bool IsActive() const override { return event_log_output_active(*output_); }
  bool Write(absl::string_view bytes) override {
    return event_log_output_write(*output_, rust::Slice<const std::uint8_t>(
        reinterpret_cast<const std::uint8_t*>(bytes.data()), bytes.size()));
  }
  void Flush() override { event_log_output_flush(*output_); }
 private:
  rust::Box<RustEventLogOutput> output_;
};
class RustFieldTrialsView final : public webrtc::FieldTrialsView {
 public:
  explicit RustFieldTrialsView(rust::Box<RustFieldTrials> view)
      : view_(std::move(view)), cache_(std::make_shared<Cache>()) {}
  std::string Lookup(absl::string_view key) const override {
    // Native consumers may validate a group and read it again. Enforce the
    // immutable-view contract even if a safe Rust provider changes its answer.
    const std::string name(key.data(), key.size());
    {
      std::lock_guard lock(cache_->mutex);
      const auto found = cache_->values.find(name);
      if (found != cache_->values.end()) return found->second;
    }
    // Never hold the cache lock across application code. Immutable nested
    // lookup of another key is not reentry into a mutable application owner.
    auto value = std::string(rust_field_trials_lookup(*view_, rust::Str(key.data(), key.size())));
    std::lock_guard lock(cache_->mutex);
    // Concurrent misses may evaluate the provider more than once, but every
    // reader receives the single winning value, including environment copies.
    return cache_->values.emplace(name, std::move(value)).first->second;
  }
  std::unique_ptr<webrtc::FieldTrialsView> CreateCopy() const override {
    auto copy = std::make_unique<RustFieldTrialsView>(rust_field_trials_copy(*view_));
    copy->cache_ = cache_;
    return copy;
  }
 private:
  struct Cache {
    std::mutex mutex;
    std::map<std::string, std::string> values;
  };
  rust::Box<RustFieldTrials> view_;
  std::shared_ptr<Cache> cache_;
};
}
NativeEventLogOutput::NativeEventLogOutput(std::unique_ptr<webrtc::RtcEventLogOutput> output) noexcept
    : output_(std::move(output)) {}
NativeEventLogOutput::~NativeEventLogOutput() = default;
webrtc::RtcEventLogOutput* NativeEventLogOutput::output() const noexcept { return output_.get(); }
bool event_log_output_native_active(const NativeEventLogOutput& output) noexcept {
  return output.output() && output.output()->IsActive();
}
bool event_log_output_native_write(NativeEventLogOutput& output, rust::Slice<const std::uint8_t> bytes) noexcept {
  return output.output() && output.output()->Write(absl::string_view(reinterpret_cast<const char*>(bytes.data()), bytes.size()));
}
void event_log_output_native_flush(NativeEventLogOutput& output) noexcept {
  if (output.output()) output.output()->Flush();
}
std::unique_ptr<webrtc::RtcEventLogOutput> NativeEventLogOutput::Take() noexcept { return std::move(output_); }
std::unique_ptr<NativeEventLogOutput> event_log_output_from_sink(rust::Box<RustEventLogOutput> output) noexcept {
  return std::make_unique<NativeEventLogOutput>(std::make_unique<RustEventLogOutputAdapter>(std::move(output)));
}
std::unique_ptr<NativeEventLogOutput> event_log_output_file(rust::Str filename, std::size_t maximum_bytes) noexcept {
  auto output = std::make_unique<webrtc::RtcEventLogOutputFile>(std::string(filename), maximum_bytes);
  if (!output->IsActive()) return nullptr;
  return std::make_unique<NativeEventLogOutput>(std::move(output));
}
std::unique_ptr<webrtc::FieldTrialsView> field_trials_adapter(rust::Box<RustFieldTrials> trials) noexcept {
  if (!field_trials_present(*trials)) return nullptr;
  return std::make_unique<RustFieldTrialsView>(std::move(trials));
}

NativeFieldTrials::NativeFieldTrials(std::unique_ptr<webrtc::FieldTrialsView> view) noexcept : view_(std::move(view)) {}
NativeFieldTrials::~NativeFieldTrials() = default;
const webrtc::FieldTrialsView& NativeFieldTrials::view() const noexcept { return *view_; }
bool field_trials_validate(rust::Str configuration) noexcept {
  return webrtc::FieldTrials::Create(absl::string_view(configuration.data(), configuration.size())) != nullptr;
}
std::unique_ptr<NativeFieldTrials> environment_field_trials(const NativeEnvironment& environment) noexcept {
  return std::make_unique<NativeFieldTrials>(environment.environment().field_trials().CreateCopy());
}
std::unique_ptr<NativeFieldTrials> field_trials_copy(const NativeFieldTrials& view) noexcept {
  return std::make_unique<NativeFieldTrials>(view.view().CreateCopy());
}
rust::String field_trials_lookup(const NativeFieldTrials& view, rust::Str key) noexcept {
  return rust::String(view.view().Lookup(absl::string_view(key.data(), key.size())));
}
bool field_trials_is_enabled(const NativeFieldTrials& view, rust::Str key) noexcept {
  return view.view().IsEnabled(absl::string_view(key.data(), key.size()));
}
bool field_trials_is_disabled(const NativeFieldTrials& view, rust::Str key) noexcept {
  return view.view().IsDisabled(absl::string_view(key.data(), key.size()));
}

std::unique_ptr<NativeEnvironment> create_environment(
    const NativeManualClock* clock,
    const NativeClock* custom_clock,
    const NativeTaskQueueFactory& factory,
    rust::Box<RustFieldTrials> field_trials,
    rust::Box<RustRtcEventLog> event_log,
    std::int64_t clock_minimum_us, std::int64_t clock_maximum_us) noexcept {
  // Private Rust registration supplies the selected adapter's enforced range.
  if (clock_minimum_us == INT64_MIN || clock_maximum_us == INT64_MAX ||
      clock_minimum_us > clock_maximum_us) return nullptr;
  if (!task_queue_factory_is_current(factory)) return nullptr;
  // A clock utility may retain the exact registered manual scheduling source.
  // Rust only supplies both pointers for that identity-bound delegate.
  if (custom_clock && factory.state()->cooperative() &&
      (!clock || !task_queue_factory_uses_clock(factory, *clock))) return nullptr;
  webrtc::EnvironmentFactory environment_factory;
  if (field_trials_present(*field_trials)) {
    environment_factory.Set(field_trials_adapter(std::move(field_trials)));
  }
  const bool custom_log = rust_event_log_present(*event_log);
  if (custom_log) {
    environment_factory.Set(shared_custom_event_log(std::move(event_log)));
  }
  if (custom_clock != nullptr) {
    environment_factory.Set(clock_reference(*custom_clock));
  } else if (clock != nullptr) {
    environment_factory.Set(std::make_unique<SharedClock>(clock->state()));
  }
  if (factory.environment()) {
    environment_factory.Set(std::make_unique<EnvironmentTaskQueueFactory>(*factory.environment(), factory.production_lease()));
  } else if (factory.state()->cooperative()) {
    environment_factory.Set(
        std::make_unique<CooperativeTaskQueueFactory>(factory.state()));
  } else {
    environment_factory.Set(std::make_unique<SharedTaskQueueFactory>(factory.state()));
  }
  return std::make_unique<NativeEnvironment>(
      std::make_unique<NativeEnvironment::State>(custom_log ? environment_factory.Create() : shared_default_event_log(environment_factory.Create()),
          FfiClockTimeBounds{true, clock_minimum_us, clock_maximum_us,
              custom_clock ? custom_clock->monotonic() : clock != nullptr}, factory.state()));
}

struct NativeEnvironmentFactory::State {
  webrtc::EnvironmentFactory factory;
  FfiClockTimeBounds bounds;
  bool log_selected = false;
  std::shared_ptr<NativeTaskQueueFactory::State> queues;
};
NativeEnvironmentFactory::NativeEnvironmentFactory(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeEnvironmentFactory::~NativeEnvironmentFactory() = default;
const std::unique_ptr<NativeEnvironmentFactory::State>& NativeEnvironmentFactory::state() const noexcept { return state_; }
std::unique_ptr<NativeEnvironmentFactory> new_environment_factory(FfiClockTimeBounds bounds) noexcept {
  return std::make_unique<NativeEnvironmentFactory>(std::make_unique<NativeEnvironmentFactory::State>(webrtc::EnvironmentFactory{}, bounds, false, nullptr));
}
std::unique_ptr<NativeEnvironmentFactory> environment_factory_from_environment(const NativeEnvironment& environment) noexcept {
  return std::make_unique<NativeEnvironmentFactory>(std::make_unique<NativeEnvironmentFactory::State>(webrtc::EnvironmentFactory(environment.environment()), environment.state()->clock_bounds, true, environment.state()->queues));
}
std::unique_ptr<NativeEnvironmentFactory> clone_environment_factory(const NativeEnvironmentFactory& factory) noexcept {
  return std::make_unique<NativeEnvironmentFactory>(std::make_unique<NativeEnvironmentFactory::State>(*factory.state()));
}
void environment_factory_set_trials(NativeEnvironmentFactory& factory, rust::Box<RustFieldTrials> trials) noexcept {
  factory.state()->factory.Set(field_trials_adapter(std::move(trials)));
}
void environment_factory_set_manual_clock(NativeEnvironmentFactory& factory, const NativeManualClock& clock, FfiClockTimeBounds bounds) noexcept {
  factory.state()->factory.Set(std::make_unique<SharedClock>(clock.state()));
  factory.state()->bounds = bounds;
}
void environment_factory_set_clock(NativeEnvironmentFactory& factory, const NativeClock& clock, FfiClockTimeBounds bounds) noexcept {
  factory.state()->factory.Set(clock_reference(clock));
  factory.state()->bounds = bounds;
}
void environment_factory_set_queues(NativeEnvironmentFactory& factory, const NativeTaskQueueFactory& queues) noexcept {
  factory.state()->queues = queues.state();
  if (queues.environment()) {
    factory.state()->factory.Set(std::make_unique<EnvironmentTaskQueueFactory>(*queues.environment(), queues.production_lease()));
  } else if (queues.state()->cooperative()) {
    factory.state()->factory.Set(std::make_unique<CooperativeTaskQueueFactory>(queues.state()));
  } else {
    factory.state()->factory.Set(std::make_unique<SharedTaskQueueFactory>(queues.state()));
  }
}
void environment_factory_set_log(NativeEnvironmentFactory& factory, rust::Box<RustRtcEventLog> log) noexcept {
  factory.state()->factory.Set(shared_custom_event_log(std::move(log)));
  factory.state()->log_selected = true;
}
std::uint8_t environment_factory_set_log_product(NativeEnvironmentFactory& factory, std::unique_ptr<NativeRtcEventLogProduct> log) noexcept {
  if (!log) return 1;
  std::uint8_t error;
  auto logger = take_event_log_product(*log, error);
  if (error) return error;
  factory.state()->factory.Set(std::move(logger));
  factory.state()->log_selected = true;
  return 0;
}
namespace {
// An owning field-trial delegate is the storage root for the complete SDK
// environment and its Rust input branch. Clock, queue and logger pointers stay
// exact. The SDK is destroyed before Rust leases, including on final SDK aliases.
struct FactoryDependencies {
  rust::Box<RustEnvironmentFactoryRetention> rust;
  webrtc::Environment environment;
};
class RetainedFactoryTrials final : public webrtc::FieldTrialsView {
 public:
  explicit RetainedFactoryTrials(std::shared_ptr<FactoryDependencies> dependencies,
      std::unique_ptr<webrtc::FieldTrialsView> copy = nullptr)
      : dependencies_(std::move(dependencies)), copy_(std::move(copy)) {}
  std::string Lookup(absl::string_view key) const override { return view().Lookup(key); }
  std::unique_ptr<webrtc::FieldTrialsView> CreateCopy() const override {
    return std::make_unique<RetainedFactoryTrials>(dependencies_, view().CreateCopy());
  }
 private:
  const webrtc::FieldTrialsView& view() const { return copy_ ? *copy_ : dependencies_->environment.field_trials(); }
  std::shared_ptr<FactoryDependencies> dependencies_;
  std::unique_ptr<webrtc::FieldTrialsView> copy_;
};
}
std::unique_ptr<NativeEnvironment> environment_factory_create(const NativeEnvironmentFactory& factory,
    rust::Box<RustEnvironmentFactoryRetention> retention) noexcept {
  auto environment = factory.state()->factory.Create();
  if (!factory.state()->log_selected) environment = shared_default_event_log(std::move(environment));
  if (environment_factory_has_randomness(*retention)) {
    retain_event_log_randomness(environment.event_log(), environment_factory_randomness(*retention));
  }
  auto dependencies = std::make_shared<FactoryDependencies>(std::move(retention), std::move(environment));
  webrtc::EnvironmentFactory retained;
  retained.Set(std::make_unique<RetainedFactoryTrials>(dependencies));
  retained.Set(&dependencies->environment.clock());
  retained.Set(&dependencies->environment.task_queue_factory());
  retained.Set(&dependencies->environment.event_log());
  return std::make_unique<NativeEnvironment>(std::make_unique<NativeEnvironment::State>(retained.Create(), factory.state()->bounds, factory.state()->queues));
}
std::unique_ptr<NativeTaskQueueFactory> environment_default_queue_metadata(const NativeEnvironment& environment) noexcept {
  return std::make_unique<NativeTaskQueueFactory>(std::make_shared<NativeTaskQueueFactory::State>(nullptr), environment.environment());
}

std::unique_ptr<NativeEnvironment> retain_environment_from_sdk(
    const webrtc::Environment& environment, const NativeEnvironment* selected) noexcept {
  FfiClockTimeBounds bounds{};
  if (selected && &environment.clock() == &selected->environment().clock()) {
    bounds = selected->state()->clock_bounds;
  }
  auto queues = selected && &environment.task_queue_factory() ==
          &selected->environment().task_queue_factory()
      ? selected->state()->queues : nullptr;
  return std::make_unique<NativeEnvironment>(std::make_unique<NativeEnvironment::State>(environment, bounds, std::move(queues)));
}
std::unique_ptr<NativeEnvironment> rtc_event_log_factory_environment(const webrtc::Environment& environment) noexcept {
  return retain_environment_from_sdk(environment);
}

std::unique_ptr<NativeTaskQueueFactory> environment_task_queue_factory(
    const NativeEnvironment& environment, const NativeTaskQueueFactory& selected) noexcept {
  // Rust supplies metadata only from the environment's selected registration or
  // an exact SDK utility identity match. Reuse that scheduler, never fork it.
  std::shared_ptr<NativeProductionLease> production;
  if (!selected.state()->cooperative()) {
    production = new_production_lease();
    if (!production) return nullptr;
  }
  return std::make_unique<NativeTaskQueueFactory>(selected.state(), environment.environment(), std::move(production));
}

std::unique_ptr<NativeEnvironment> bind_environment_selection(
    std::unique_ptr<NativeEnvironment> environment, const NativeEnvironment& selected) noexcept {
  if (environment && !environment->state()->clock_bounds.known &&
      environment_clock_identity_matches(*environment, selected)) {
    environment->state()->clock_bounds = selected.state()->clock_bounds;
  }
  if (environment && !environment->state()->queues &&
      environment_queue_factory_identity_matches(*environment, selected)) {
    environment->state()->queues = selected.state()->queues;
  }
  return environment;
}

std::unique_ptr<NativeEnvironment> bind_driver_environment(
    const NativeDriverThread& driver, const NativeEnvironment& environment,
    rust::Box<RustRuntimeEnvironmentRetention> retained) noexcept {
  if (!driver.thread() || !driver.thread()->IsCurrent()) return nullptr;
  auto original = environment.environment();
  auto owner = driver.state()->retirement;
  {
    auto& registry = runtime_retirement_internal::registry();
    std::lock_guard lock(registry.mutex);
    auto found = registry.factories.find(&original.task_queue_factory());
    // Clock association alone does not prove that this particular Environment
    // owns a scope ticket. Bind unowned siblings even when clocks match.
    if (found != registry.factories.end() && found->second.lock() == owner)
      return clone_environment(environment);
  }
  const auto id = owner->Retain([retained = std::move(retained)]() mutable {
    // Destroy the entire original Rust/native dependency set on the creator.
    // It contains no Rc driver; the closing driver owns the retirement table.
    (void)retained;
  });
  webrtc::EnvironmentFactory scoped(original);
  scoped.Set(std::make_unique<RuntimeBoundTaskQueueFactory>(
      original.task_queue_factory(), original.clock(), owner, id));
  return std::make_unique<NativeEnvironment>(
      std::make_unique<NativeEnvironment::State>(scoped.Create(), environment.state()->clock_bounds, environment.state()->queues));
}

std::unique_ptr<NativeEnvironment> clone_environment(
    const NativeEnvironment& environment) noexcept {
  return std::make_unique<NativeEnvironment>(
      std::make_unique<NativeEnvironment::State>(*environment.state()));
}
FfiClockTimeBounds environment_clock_time_bounds(const NativeEnvironment& environment) noexcept {
  return environment.state()->clock_bounds;
}
bool environment_clock_identity_matches(const NativeEnvironment& environment,
    const NativeEnvironment& other) noexcept {
  return &environment.environment().clock() == &other.environment().clock();
}
bool environment_queue_factory_identity_matches(const NativeEnvironment& environment,
    const NativeEnvironment& other) noexcept {
  return &environment.environment().task_queue_factory() == &other.environment().task_queue_factory();
}
bool environment_context_retention_contract(const NativeEnvironment& environment,
    const NativeEnvironment& other) noexcept {
  const auto original = environment.environment();
  if (!environment.state()->clock_bounds.known || &original.clock() == &other.environment().clock()) return false;
  const auto retained = retain_environment_from_sdk(original, &environment);
  const auto unknown = retain_environment_from_sdk(original);
  const auto mismatch = retain_environment_from_sdk(original, &other);
  const auto copy = clone_environment(*retained);
  const auto actual = copy->environment();
  const auto bounds = environment_clock_time_bounds(*copy);
  return &actual.clock() == &original.clock() &&
      &actual.task_queue_factory() == &original.task_queue_factory() &&
      &actual.field_trials() == &original.field_trials() &&
      &actual.event_log() == &original.event_log() && bounds.known &&
      bounds.minimum_us == environment.state()->clock_bounds.minimum_us &&
      bounds.maximum_us == environment.state()->clock_bounds.maximum_us &&
      bounds.monotonic == environment.state()->clock_bounds.monotonic &&
      !environment_clock_time_bounds(*unknown).known &&
      !environment_clock_time_bounds(*mismatch).known;
}

std::int64_t environment_time_us(
    const NativeEnvironment& environment) noexcept {
  return environment.state()->environment.clock().TimeInMicroseconds();
}

std::int64_t environment_timestamp(const NativeEnvironment& environment) noexcept {
  const auto value = environment.environment().clock().CurrentTime();
  return value.IsPlusInfinity() ? std::numeric_limits<std::int64_t>::max() : value.us_or(std::numeric_limits<std::int64_t>::min());
}

std::unique_ptr<NativeRandomnessLease> new_seeded_randomness(
    std::uint64_t seed) noexcept {
  std::lock_guard lock(hooks_mutex);
  if (randomness_active) return nullptr;
  randomness_active = true;
  webrtc::SetRandomGenerator(std::make_unique<SeededRandom>(seed));
  return std::make_unique<NativeRandomnessLease>();
}

std::uint64_t next_seeded_random_u64(const NativeRandomnessLease&) noexcept {
  return webrtc::CreateRandomId64();
}

std::unique_ptr<NativeThread> new_thread(bool network) noexcept {
  std::unique_ptr<webrtc::Thread> thread =
      network ? webrtc::Thread::CreateWithSocketServer()
              : webrtc::Thread::Create();
  if (!thread || !thread->Start()) {
    return nullptr;
  }
  return std::make_unique<NativeThread>(
      std::make_unique<NativeThread::State>(std::move(thread)));
}

std::unique_ptr<NativeDriverThread> new_driver_thread(
    const NativeManualClock& clock) noexcept {
  std::lock_guard lock(hooks_mutex);
  if (driver_active.load() || production_leases != 0 ||
      webrtc::Thread::Current() || webrtc::GetClockForTesting()) return nullptr;
  auto queues = MakeCooperativeState(clock.state());
  if (!queues) return nullptr;
  auto thread = std::make_unique<ControlledThread>(std::move(queues));
  if (!thread->WrapCurrent()) return nullptr;
  driver_active.store(true);
  controlled_driver_identity = thread.get();
  return std::make_unique<NativeDriverThread>(
      std::make_unique<NativeDriverThread::State>(std::move(thread), clock.state()));
}

std::unique_ptr<NativeThread> borrow_driver_thread(
    const NativeDriverThread& driver) noexcept {
  if (!driver.thread() || !driver.thread()->IsCurrent()) return nullptr;
  return std::make_unique<NativeThread>(
      std::make_unique<NativeThread::State>(driver.thread()));
}

std::unique_ptr<NativeDriverThread> new_seeded_driver_thread(
    const NativeManualClock& clock, std::uint64_t seed) noexcept {
  std::lock_guard lock(hooks_mutex);
  if (driver_active.load() || production_leases != 0 || randomness_active || webrtc::Thread::Current() ||
      webrtc::GetClockForTesting()) return nullptr;
  auto queues = MakeCooperativeState(clock.state());
  if (!queues) return nullptr;
  auto thread = std::make_unique<ControlledThread>(std::move(queues));
  if (!thread->WrapCurrent()) return nullptr;
  driver_active.store(true);
  controlled_driver_identity = thread.get();
  randomness_active = true;
  webrtc::SetRandomGenerator(std::make_unique<SeededRandom>(seed));
  return std::make_unique<NativeDriverThread>(
      std::make_unique<NativeDriverThread::State>(std::move(thread), clock.state(), true));
}

std::size_t driver_pump(const NativeDriverThread& driver,
                        std::size_t budget) noexcept {
  if (!driver.thread() || !driver.thread()->IsCurrent()) return 0;
  if (budget == 0) return 0;
  driver.state()->retirement->Drain();
  const auto dispatched = PumpClock(driver.state()->clock.state, budget);
  driver.state()->retirement->Drain();
  return dispatched;
}

bool driver_run_ready(const NativeDriverThread& driver) noexcept {
  if (!driver.thread() || !driver.thread()->IsCurrent()) return false;
  driver_pump(driver, 1024);
  return true;
}

bool test_driver_lifecycle_yield(
    const NativeDriverThread& driver,
    const NativeTaskQueueFactory& factory) noexcept {
  if (!driver_is_current(driver) ||
      factory.state()->clock != driver.state()->clock.state ||
      !task_queue_factory_is_current(factory)) return false;
  const auto before = driver.state()->clock.state->clock.TimeInMicroseconds();
  webrtc::Event event;
  CooperativeTaskQueueFactory implementation(factory.state());
  auto queue = implementation.CreateTaskQueue(
      "lifecycle-probe", webrtc::TaskQueueFactory::Priority::kNormal);
  queue->PostTask([&event] { event.Set(); });
  return event.Wait(webrtc::TimeDelta::PlusInfinity()) &&
         driver.state()->clock.state->clock.TimeInMicroseconds() == before;
}

// Binding primitive qualification: unlike a peer's SDK Close, a fresh driver
// has no unrelated teardown tasks. Count dispatches as well as body execution
// so leaving an empty continuation envelope queued is also a failure.
bool test_driver_capture_cancellation(const NativeDriverThread& driver) noexcept {
  if (!driver_is_current(driver)) return false;
  auto ran = std::make_shared<std::size_t>(0);
  const auto before = driver.state()->clock.state->clock.TimeInMicroseconds();
  CapturedDriverTasks capture;
  capture.Capture(driver.thread(), [&] {
    driver.thread()->PostTask([ran] { ++*ran; });
    return true;
  });
  capture.Cancel();
  return PumpClock(driver.state()->clock.state, 1) == 0 && *ran == 0 &&
      driver.state()->clock.state->clock.TimeInMicroseconds() == before;
}

std::int64_t driver_next_deadline_us(const NativeDriverThread& driver) noexcept {
  if (!driver.thread() || !driver.thread()->IsCurrent()) return kNoDeadline;
  return ClockDeadline(driver.state()->clock.state);
}

bool driver_is_current(const NativeDriverThread& driver) noexcept {
  return driver.thread() && driver.thread()->IsCurrent();
}

bool thread_is_owned(const NativeThread& thread) noexcept {
  return thread.state() && thread.state()->owned && !thread.state()->thread->IsQuitting();
}
bool thread_is_current(const NativeThread& thread) noexcept {
  return thread.thread() && thread.thread()->IsCurrent() && webrtc::TaskQueueBase::Current() == thread.thread();
}
bool thread_requires_retirement(const NativeThread& thread) noexcept {
  return webrtc::TaskQueueBase::Current() || (thread.thread() && thread.thread()->IsCurrent());
}
bool thread_post_task(const NativeThread& thread,
                      rust::Box<RustTask> task) noexcept {
  if (!thread.state() || !thread.state()->thread ||
      thread.state()->thread->IsQuitting()) {
    return false;
  }
  thread.state()->thread->PostTask(
      [task = std::move(task)]() mutable { run_task(std::move(task)); });
  return true;
}

bool thread_post_delayed_task(const NativeThread& thread,
                              std::int64_t delay_us,
                              rust::Box<RustTask> task) noexcept {
  if (!thread.state() || !thread.state()->thread || delay_us < 0 ||
      thread.state()->thread->IsQuitting()) {
    return false;
  }
  return thread_post_native_delayed_task(thread, delay_us, false, std::move(task)) == 0;
}

std::uint8_t thread_post_native_delayed_task(const NativeThread& thread, std::int64_t bits, bool high_precision, rust::Box<RustTask> task) noexcept {
  if (!thread.thread() || thread.thread()->IsQuitting()) return 1;
  const bool controlled = !thread.state()->owned;
  if (controlled && (bits == INT64_MIN || bits == INT64_MAX)) return 2;
  PostNativeDelay(*thread.thread(), bits, high_precision, std::move(task));
  return 0;
}

}  // namespace pulsebeam::webrtc_sys
