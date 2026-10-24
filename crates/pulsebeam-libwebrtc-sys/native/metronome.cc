#include "pulsebeam-webrtc-sys/native/metronome.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <algorithm>
#include <cstdint>
#include <mutex>
#include <utility>
#include <vector>
#include "api/metronome/metronome.h"
#include "rtc_base/thread.h"

namespace pulsebeam::webrtc_sys {
struct NativeMetronomeTick::State : std::enable_shared_from_this<State> {
  using Callback = absl::AnyInvocable<void() &&>;
  explicit State(Callback callback) : thread(webrtc::Thread::Current()), callback(std::move(callback)) {}
  bool Signal() {
    std::lock_guard lock(mutex);
    if (!thread || !callback || submitted) return false;
    submitted = true;
    // Admission and retirement use the same mutex. No posted closure borrows
    // the adapter, provider or worker, and invocation never occurs inline.
    thread->PostTask([state = shared_from_this()] { state->Run(); });
    return true;
  }
  void Discard() {
    std::lock_guard lock(mutex);
    if (!thread || !callback || submitted) return;
    submitted = true;
    thread->PostTask([state = shared_from_this()] { state->Retire(); });
  }
  void Retire() {
    Callback discarded;
    {
      std::lock_guard lock(mutex);
      thread = nullptr;
      discarded = std::move(callback);
    }
    // Called on the requesting worker, outside the cancellation mutex.
    // Capture disposal may release SDK owners or reenter their destruction.
    discarded = nullptr;
  }
  void Run() {
    Callback pending;
    {
      std::lock_guard lock(mutex);
      if (!thread || !callback) return;
      thread = nullptr;
      pending = std::move(callback);
    }
    std::move(pending)();
    pending = nullptr;
  }
  std::mutex mutex;
  webrtc::Thread* thread;
  Callback callback;
  bool submitted = false;
};
NativeMetronomeTick::NativeMetronomeTick(std::shared_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeMetronomeTick::~NativeMetronomeTick() { state_->Discard(); }
bool NativeMetronomeTick::Signal() noexcept { return state_->Signal(); }
bool metronome_tick_signal(std::unique_ptr<NativeMetronomeTick> tick) noexcept { return tick && tick->Signal(); }
namespace {
class Adapter final : public webrtc::Metronome {
 public:
  explicit Adapter(rust::Box<RustMetronome> source) : source_(std::move(source)) {}
  ~Adapter() override {
    // The SDK retires its factory metronomes on its still-live worker thread.
    for (auto& request : requests_) if (auto state = request.lock()) state->Retire();
  }
  webrtc::TimeDelta TickPeriod() const override {
    const auto bits = metronome_tick_period(*source_);
    if (bits == INT64_MAX) return webrtc::TimeDelta::PlusInfinity();
    if (bits == INT64_MIN) return webrtc::TimeDelta::MinusInfinity();
    return webrtc::TimeDelta::Micros(bits);
  }
  void RequestCallOnNextTick(absl::AnyInvocable<void() &&> callback) override {
    // Factory metronomes are worker-thread dependencies, not arbitrary queues.
    if (!webrtc::Thread::Current()) return;
    requests_.erase(std::remove_if(requests_.begin(), requests_.end(),
        [](const auto& request) { return request.expired(); }), requests_.end());
    auto state = std::make_shared<NativeMetronomeTick::State>(std::move(callback));
    requests_.push_back(state);
    metronome_request_tick(*source_, std::make_unique<NativeMetronomeTick>(std::move(state)));
  }
 private:
  rust::Box<RustMetronome> source_;
  std::vector<std::weak_ptr<NativeMetronomeTick::State>> requests_;
};
}  // namespace
std::unique_ptr<webrtc::Metronome> metronome_adapter(rust::Box<RustMetronome> source) noexcept {
  if (!metronome_present(*source)) return nullptr;
  return std::make_unique<Adapter>(std::move(source));
}
}  // namespace pulsebeam::webrtc_sys
