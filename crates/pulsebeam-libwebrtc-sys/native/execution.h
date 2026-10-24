#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>

#include "absl/functional/any_invocable.h"
#include "api/environment/environment.h"
#include "api/field_trials_view.h"
#include "api/rtc_event_log_output.h"
#include "rust/cxx.h"

namespace webrtc {
class Thread;
class TaskQueueBase;
}

namespace pulsebeam::webrtc_sys {

// Binding-owned cancellation for an admitted SDK request known to post its
// observer continuation last. Other posts made during the call remain intact.
// Only the current controlled role is intercepted. Cancel destroys captures
// without dispatching task bodies, pumping queues, or advancing time.
class CapturedDriverTasks final {
 public:
  struct State;
  CapturedDriverTasks() noexcept;
  ~CapturedDriverTasks();
  bool Capture(webrtc::Thread* thread, absl::AnyInvocable<bool()> request) noexcept;
  void Cancel() noexcept;
  bool retired() const noexcept;
 private:
  std::unique_ptr<State> state_;
};

struct RustTask;
struct RustRuntimeEnvironmentRetention;
struct RustTaskQueueFactory;
class NativeClock;
struct RustReadiness;
struct RustFieldTrials;
std::unique_ptr<webrtc::FieldTrialsView> field_trials_adapter(rust::Box<RustFieldTrials> trials) noexcept;
struct RustEventLogOutput;
struct RustRtcEventLog;

class NativeEventLogOutput final {
 public:
  explicit NativeEventLogOutput(std::unique_ptr<webrtc::RtcEventLogOutput> output) noexcept;
  ~NativeEventLogOutput();
  std::unique_ptr<webrtc::RtcEventLogOutput> Take() noexcept;
  webrtc::RtcEventLogOutput* output() const noexcept;
 private:
  std::unique_ptr<webrtc::RtcEventLogOutput> output_;
};
std::unique_ptr<NativeEventLogOutput> event_log_output_from_sink(rust::Box<RustEventLogOutput> output) noexcept;
std::unique_ptr<NativeEventLogOutput> event_log_output_file(rust::Str filename, std::size_t maximum_bytes) noexcept;
bool event_log_output_native_active(const NativeEventLogOutput& output) noexcept;
bool event_log_output_native_write(NativeEventLogOutput& output, rust::Slice<const std::uint8_t> bytes) noexcept;
void event_log_output_native_flush(NativeEventLogOutput& output) noexcept;

// Contains only a thread-safe Rust notification state, never engine handles.
class ReadinessSignal final {
 public:
  explicit ReadinessSignal(rust::Box<RustReadiness> readiness) noexcept;
  ~ReadinessSignal();
  void Signal() const noexcept;

 private:
  rust::Box<RustReadiness> readiness_;
};

class NativeReadiness final {
 public:
  explicit NativeReadiness(rust::Box<RustReadiness> readiness) noexcept;
  ~NativeReadiness();
  std::shared_ptr<ReadinessSignal> signal() const noexcept;

 private:
  std::shared_ptr<ReadinessSignal> signal_;
};

std::unique_ptr<NativeReadiness> new_readiness(
    rust::Box<RustReadiness> readiness) noexcept;

inline void SignalReadiness(const std::shared_ptr<ReadinessSignal>& signal) {
  if (signal) signal->Signal();
}

struct ReadinessNotification {
  std::shared_ptr<ReadinessSignal> readiness;
  ~ReadinessNotification() { SignalReadiness(readiness); }
};

// Reserve production before starting any engine queues/threads. Controlled
// acquisition checks the same reservation under the process-hook lock.
class NativeProductionLease final {
 public:
  NativeProductionLease() noexcept = default;
  ~NativeProductionLease();
};
std::unique_ptr<NativeProductionLease> new_production_lease() noexcept;

class NativeManualClock final {
 public:
  struct State;

  explicit NativeManualClock(std::shared_ptr<State> state) noexcept;
  ~NativeManualClock();

  NativeManualClock(const NativeManualClock&) = delete;
  NativeManualClock& operator=(const NativeManualClock&) = delete;

  const std::shared_ptr<State>& state() const noexcept;

 private:
  std::shared_ptr<State> state_;
};

class NativeTaskQueueFactory final {
 public:
  struct State;

  explicit NativeTaskQueueFactory(std::shared_ptr<State> state,
      std::optional<webrtc::Environment> environment = std::nullopt,
      std::shared_ptr<NativeProductionLease> production = nullptr) noexcept;
  ~NativeTaskQueueFactory();

  NativeTaskQueueFactory(const NativeTaskQueueFactory&) = delete;
  NativeTaskQueueFactory& operator=(const NativeTaskQueueFactory&) = delete;

  const std::shared_ptr<State>& state() const noexcept;
  const std::optional<webrtc::Environment>& environment() const noexcept;
  const std::shared_ptr<NativeProductionLease>& production_lease() const noexcept;

 private:
  std::shared_ptr<NativeProductionLease> production_lease_;
  std::optional<webrtc::Environment> environment_;
  std::shared_ptr<State> state_;
};

class NativeTaskQueue final {
 public:
  struct State;

  explicit NativeTaskQueue(std::unique_ptr<State> state) noexcept;
  ~NativeTaskQueue();

  NativeTaskQueue(const NativeTaskQueue&) = delete;
  NativeTaskQueue& operator=(const NativeTaskQueue&) = delete;

  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

// Only known threaded queues can guarantee progress while a synchronous SDK
// consumer blocks waiting for a posted completion task. The owner must survive.
webrtc::TaskQueueBase* task_queue_for_blocking_consumer(const NativeTaskQueue&) noexcept;

class NativeEnvironment final {
 public:
  struct State;

  explicit NativeEnvironment(std::unique_ptr<State> state) noexcept;
  ~NativeEnvironment();

  NativeEnvironment(const NativeEnvironment&) = delete;
  NativeEnvironment& operator=(const NativeEnvironment&) = delete;

  const std::unique_ptr<State>& state() const noexcept;
  webrtc::Environment environment() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

// Binding-internal provenance for the actual builtin VP8 encoder. This owns
// native Environment aliases only, never a Rust driver/Rc executor. Resolution
// requires an exact selected service identity and a bound cooperative factory.
class NativeBuiltinVp8Execution final {
 public:
  struct State;
  explicit NativeBuiltinVp8Execution(std::unique_ptr<State> state) noexcept;
  ~NativeBuiltinVp8Execution();
  void SetCores(int cores) noexcept;
 private:
  friend class NativeBuiltinVp8Scope;
  std::unique_ptr<State> state_;
};
std::shared_ptr<NativeBuiltinVp8Execution> builtin_vp8_execution(
    const NativeEnvironment& selected, const webrtc::Environment& actual,
    bool& valid) noexcept;
class NativeBuiltinVp8Scope final {
 public:
  explicit NativeBuiltinVp8Scope(
      const std::shared_ptr<NativeBuiltinVp8Execution>& execution) noexcept;
  ~NativeBuiltinVp8Scope();
  NativeBuiltinVp8Scope(const NativeBuiltinVp8Scope&) = delete;
  NativeBuiltinVp8Scope& operator=(const NativeBuiltinVp8Scope&) = delete;
 private:
  NativeBuiltinVp8Execution::State* previous_;
};

struct RustEnvironmentFactoryRetention;
struct FfiClockTimeBounds;
class NativeEnvironmentFactory final {
 public:
  struct State;
  explicit NativeEnvironmentFactory(std::unique_ptr<State> state) noexcept;
  ~NativeEnvironmentFactory();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeEnvironmentFactory> new_environment_factory(FfiClockTimeBounds bounds) noexcept;
std::unique_ptr<NativeEnvironmentFactory> environment_factory_from_environment(const NativeEnvironment& environment) noexcept;
std::unique_ptr<NativeEnvironmentFactory> clone_environment_factory(const NativeEnvironmentFactory& factory) noexcept;
void environment_factory_set_trials(NativeEnvironmentFactory& factory, rust::Box<RustFieldTrials> trials) noexcept;
void environment_factory_set_manual_clock(NativeEnvironmentFactory& factory, const NativeManualClock& clock, FfiClockTimeBounds bounds) noexcept;
void environment_factory_set_clock(NativeEnvironmentFactory& factory, const NativeClock& clock, FfiClockTimeBounds bounds) noexcept;
void environment_factory_set_queues(NativeEnvironmentFactory& factory, const NativeTaskQueueFactory& queues) noexcept;
void environment_factory_set_log(NativeEnvironmentFactory& factory, rust::Box<RustRtcEventLog> log) noexcept;
class NativeRtcEventLogProduct;
std::uint8_t environment_factory_set_log_product(NativeEnvironmentFactory& factory, std::unique_ptr<NativeRtcEventLogProduct> log) noexcept;
std::unique_ptr<NativeEnvironment> environment_factory_create(const NativeEnvironmentFactory& factory, rust::Box<RustEnvironmentFactoryRetention> retention) noexcept;
std::unique_ptr<NativeTaskQueueFactory> environment_default_queue_metadata(const NativeEnvironment& environment) noexcept;

class NativeRandomnessLease final {
 public:
  NativeRandomnessLease() noexcept = default;
  ~NativeRandomnessLease();

  NativeRandomnessLease(const NativeRandomnessLease&) = delete;
  NativeRandomnessLease& operator=(const NativeRandomnessLease&) = delete;
};

class NativeDriverThread final {
 public:
  struct State;

  explicit NativeDriverThread(std::unique_ptr<State> state) noexcept;
  ~NativeDriverThread();

  NativeDriverThread(const NativeDriverThread&) = delete;
  NativeDriverThread& operator=(const NativeDriverThread&) = delete;

  const std::unique_ptr<State>& state() const noexcept;
  webrtc::Thread* thread() const noexcept;
  bool uses_clock(const NativeManualClock& clock) const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeThread final {
 public:
  struct State;

  explicit NativeThread(std::unique_ptr<State> state) noexcept;
  ~NativeThread();

  NativeThread(const NativeThread&) = delete;
  NativeThread& operator=(const NativeThread&) = delete;

  const std::unique_ptr<State>& state() const noexcept;
  webrtc::Thread* thread() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

std::unique_ptr<NativeManualClock> new_manual_clock(
    std::int64_t initial_time_us) noexcept;
std::int64_t manual_clock_time_us(const NativeManualClock& clock) noexcept;
bool advance_manual_clock(const NativeManualClock& clock,
                          std::int64_t delta_us) noexcept;
std::int64_t system_clock_time_us() noexcept;

std::unique_ptr<NativeTaskQueueFactory>
new_default_task_queue_factory() noexcept;
std::unique_ptr<NativeTaskQueueFactory> new_provider_task_queue_factory(rust::Box<RustTaskQueueFactory> provider) noexcept;
std::unique_ptr<NativeTaskQueueFactory> new_cooperative_task_queue_factory(
    const NativeManualClock& clock) noexcept;
bool task_queue_factory_is_cooperative(
    const NativeTaskQueueFactory& factory) noexcept;
bool task_queue_factory_is_current(
    const NativeTaskQueueFactory& factory) noexcept;
bool task_queue_factory_uses_clock(const NativeTaskQueueFactory& factory,
                                   const NativeManualClock& clock) noexcept;
std::unique_ptr<NativeTaskQueue> create_task_queue(
    const NativeTaskQueueFactory& factory,
    rust::Slice<const std::uint8_t> name,
    std::uint8_t priority) noexcept;
bool post_task(const NativeTaskQueue& queue, rust::Box<RustTask> task) noexcept;
bool post_delayed_task(const NativeTaskQueue& queue,
                       std::int64_t delay_us,
                       rust::Box<RustTask> task) noexcept;
std::size_t task_queue_identity(const NativeTaskQueue& queue) noexcept;
bool task_queue_is_current(const NativeTaskQueue& queue) noexcept;
rust::Vec<std::uint8_t> task_queue_name(const NativeTaskQueue& queue) noexcept;
bool task_queue_has_failed(const NativeTaskQueue& queue) noexcept;
std::uint8_t post_native_delayed_task(const NativeTaskQueue& queue, std::int64_t bits, bool high_precision,
                              rust::Box<RustTask> task) noexcept;
std::size_t run_ready_tasks(const NativeTaskQueueFactory& factory) noexcept;
std::size_t pump_ready_tasks(const NativeTaskQueueFactory& factory,
                            std::size_t budget) noexcept;
std::int64_t next_task_deadline_us(
    const NativeTaskQueueFactory& factory) noexcept;

class NativeFieldTrials final {
 public:
  explicit NativeFieldTrials(std::unique_ptr<webrtc::FieldTrialsView> view) noexcept;
  ~NativeFieldTrials();
  const webrtc::FieldTrialsView& view() const noexcept;
 private:
  std::unique_ptr<webrtc::FieldTrialsView> view_;
};
bool field_trials_validate(rust::Str configuration) noexcept;
std::unique_ptr<NativeFieldTrials> environment_field_trials(const NativeEnvironment& environment) noexcept;
std::unique_ptr<NativeFieldTrials> field_trials_copy(const NativeFieldTrials& view) noexcept;
rust::String field_trials_lookup(const NativeFieldTrials& view, rust::Str key) noexcept;
bool field_trials_is_enabled(const NativeFieldTrials& view, rust::Str key) noexcept;
bool field_trials_is_disabled(const NativeFieldTrials& view, rust::Str key) noexcept;

std::unique_ptr<NativeEnvironment> create_environment(
    const NativeManualClock* clock,
    const NativeClock* custom_clock,
    const NativeTaskQueueFactory& factory,
    rust::Box<RustFieldTrials> field_trials,
    rust::Box<RustRtcEventLog> event_log,
    std::int64_t clock_minimum_us, std::int64_t clock_maximum_us) noexcept;
// Retain the actual SDK value. Transfer clock bounds only after exact clock
// identity comparison; absent/unmatched selection evidence stays unknown.
std::unique_ptr<NativeEnvironment> retain_environment_from_sdk(
    const webrtc::Environment& environment, const NativeEnvironment* selected = nullptr) noexcept;
FfiClockTimeBounds environment_clock_time_bounds(const NativeEnvironment& environment) noexcept;
bool environment_clock_identity_matches(const NativeEnvironment& environment,
    const NativeEnvironment& other) noexcept;
bool environment_queue_factory_identity_matches(const NativeEnvironment& environment,
    const NativeEnvironment& other) noexcept;
bool environment_context_retention_contract(const NativeEnvironment& environment,
    const NativeEnvironment& other) noexcept;
std::unique_ptr<NativeTaskQueueFactory> environment_task_queue_factory(
    const NativeEnvironment& environment, const NativeTaskQueueFactory& selected) noexcept;
std::unique_ptr<NativeEnvironment> bind_environment_selection(
    std::unique_ptr<NativeEnvironment> environment, const NativeEnvironment& selected) noexcept;
std::unique_ptr<NativeEnvironment> bind_driver_environment(
    const NativeDriverThread& driver, const NativeEnvironment& environment,
    rust::Box<RustRuntimeEnvironmentRetention> retained) noexcept;
std::unique_ptr<NativeEnvironment> clone_environment(
    const NativeEnvironment& environment) noexcept;
std::int64_t environment_time_us(const NativeEnvironment& environment) noexcept;
std::int64_t environment_timestamp(const NativeEnvironment& environment) noexcept;

std::unique_ptr<NativeRandomnessLease> new_seeded_randomness(
    std::uint64_t seed) noexcept;
std::uint64_t next_seeded_random_u64(
    const NativeRandomnessLease& lease) noexcept;

std::unique_ptr<NativeThread> new_thread(bool network) noexcept;
std::unique_ptr<NativeDriverThread> new_driver_thread(
    const NativeManualClock& clock) noexcept;
std::unique_ptr<NativeDriverThread> new_seeded_driver_thread(
    const NativeManualClock& clock, std::uint64_t seed) noexcept;
std::size_t driver_pump(const NativeDriverThread& driver,
                        std::size_t budget) noexcept;
std::unique_ptr<NativeThread> borrow_driver_thread(
    const NativeDriverThread& driver) noexcept;
bool driver_run_ready(const NativeDriverThread& driver) noexcept;
bool test_driver_lifecycle_yield(
    const NativeDriverThread& driver,
    const NativeTaskQueueFactory& factory) noexcept;
bool test_driver_capture_cancellation(const NativeDriverThread& driver) noexcept;
std::int64_t driver_next_deadline_us(const NativeDriverThread& driver) noexcept;
bool driver_is_current(const NativeDriverThread& driver) noexcept;
bool thread_is_owned(const NativeThread& thread) noexcept;
bool thread_is_current(const NativeThread& thread) noexcept;
bool thread_requires_retirement(const NativeThread& thread) noexcept;
bool thread_post_task(const NativeThread& thread,
                      rust::Box<RustTask> task) noexcept;
bool task_queue_current_delay_supported(std::int64_t bits) noexcept;
std::uint8_t thread_post_native_delayed_task(const NativeThread& thread, std::int64_t bits, bool high_precision, rust::Box<RustTask> task) noexcept;
bool thread_post_delayed_task(const NativeThread& thread,
                              std::int64_t delay_us,
                              rust::Box<RustTask> task) noexcept;

}  // namespace pulsebeam::webrtc_sys
