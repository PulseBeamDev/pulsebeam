#include "pulsebeam-webrtc-sys/native/fec.h"
#include <mutex>
#include <thread>
#include <utility>
#include "api/task_queue/task_queue_base.h"
#include "modules/video_coding/fec_controller_default.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {
struct NativeFecProtectionCallback::State {
  mutable std::mutex mutex;
  webrtc::VCMProtectionCallback* callback;
  webrtc::TaskQueueBase* queue = webrtc::TaskQueueBase::Current();
  std::thread::id thread = std::this_thread::get_id();
};
NativeFecProtectionCallback::NativeFecProtectionCallback(
    webrtc::VCMProtectionCallback* callback) : state_(std::make_unique<State>()) {
  state_->callback = callback;
}
NativeFecProtectionCallback::~NativeFecProtectionCallback() = default;
void NativeFecProtectionCallback::Retire() noexcept {
  std::lock_guard<std::mutex> lock(state_->mutex);
  state_->callback = nullptr;
}
NativeFecProtectionCallback::State& NativeFecProtectionCallback::state() const noexcept {
  return *state_;
}
namespace {
enum class LoanKind { None, Controller, Rates, Scheduled };
thread_local const NativeFecProtectionCallback* current_loan = nullptr;
thread_local LoanKind current_kind = LoanKind::None;
class Loan final {
 public:
  Loan(const NativeFecProtectionCallback* source, LoanKind kind)
      : previous_(current_loan), kind_(current_kind) {
    current_loan = source;
    current_kind = kind;
  }
  ~Loan() { current_loan = previous_; current_kind = kind_; }
 private:
  const NativeFecProtectionCallback* previous_;
  LoanKind kind_;
};
FfiFecProtectionResult Failure(std::uint8_t status) {
  FfiFecProtectionResult value{};
  value.status = status;
  return value;
}
class QueuedCompletion final {
 public:
  explicit QueuedCompletion(rust::Box<RustFecCompletion> source)
      : source_(std::move(source)) {}
  void Complete(FfiFecProtectionResult result) { fec_async_complete(*source_, result); }
 private:
  rust::Box<RustFecCompletion> source_;
};
std::uint8_t LoanStatus(const NativeFecProtectionCallback& source) {
  if (!source.state().callback) return 1;
  // RtpVideoSender destroys its RTP modules before its FEC controller. Queue
  // affinity alone is insufficient: unrelated Rust teardown callbacks could
  // run during that interval. A dynamic borrow is established only by an
  // actual native FEC call or an independently dispatched worker task.
  if (current_loan != &source) return 3;
  return 0;
}
bool OnWorker(const NativeFecProtectionCallback::State& state) {
  return webrtc::TaskQueueBase::Current() == state.queue &&
      (state.queue || std::this_thread::get_id() == state.thread);
}
bool Valid(const FfiFecProtectionParams& value) {
  // The native ULPFEC path DCHECKs this rate range. Preserve signed fields in
  // owned values, but reject invalid requests before native table indexing.
  return value.fec_rate >= 0 && value.fec_rate <= 255 &&
      (value.mask_type == 0 || value.mask_type == 1);
}
webrtc::FecProtectionParams Params(const FfiFecProtectionParams& value) {
  webrtc::FecProtectionParams result;
  result.fec_rate = value.fec_rate;
  result.max_fec_frames = value.max_fec_frames;
  result.fec_mask_type = static_cast<webrtc::FecMaskType>(value.mask_type);
  return result;
}
class ControllerAdapter final : public webrtc::FecController {
 public:
  explicit ControllerAdapter(rust::Box<RustFecController> source)
      : source_(std::move(source)) {}
  ~ControllerAdapter() override {
    auto callback = Callback();
    if (callback) callback->Retire();
    // source_ subsequently drops after the borrowed pointer is invalidated.
  }
  void SetProtectionCallback(webrtc::VCMProtectionCallback* callback) override {
    auto next = callback ? std::make_shared<NativeFecProtectionCallback>(callback)
                         : nullptr;
    std::shared_ptr<NativeFecProtectionCallback> previous;
    {
      std::lock_guard<std::mutex> lock(callback_mutex_);
      previous = std::exchange(callback_, next);
    }
    if (previous) previous->Retire();
    Loan loan(next.get(), LoanKind::Controller);
    fec_controller_set_callback(*source_, std::move(next));
  }
  void SetProtectionMethod(bool fec, bool nack) override {
    auto callback = Callback(); Loan loan(callback.get(), LoanKind::Controller);
    fec_controller_set_method(*source_, fec, nack);
  }
  void SetEncodingData(std::size_t width, std::size_t height,
                       std::size_t temporal_layers, std::size_t payload) override {
    auto callback = Callback(); Loan loan(callback.get(), LoanKind::Controller);
    fec_controller_set_encoding(*source_, width, height, temporal_layers, payload);
  }
  std::uint32_t UpdateFecRates(std::uint32_t bitrate, int framerate,
      std::uint8_t lost, std::vector<bool> mask, std::int64_t rtt) override {
    auto callback = Callback(); Loan loan(callback.get(), LoanKind::Rates);
    rust::Vec<std::uint8_t> values;
    values.reserve(mask.size());
    for (bool value : mask) values.push_back(value ? 1 : 0);
    return fec_controller_update_rates(*source_, bitrate, framerate, lost,
        rust::Slice<const std::uint8_t>(values.data(), values.size()), rtt);
  }
  void UpdateWithEncodedData(std::size_t length,
                            webrtc::VideoFrameType type) override {
    auto callback = Callback(); Loan loan(callback.get(), LoanKind::Controller);
    fec_controller_update_encoded(*source_, length, static_cast<std::int32_t>(type));
  }
  bool UseLossVectorMask() override {
    auto callback = Callback(); Loan loan(callback.get(), LoanKind::Controller);
    return fec_controller_use_mask(*source_);
  }
 private:
  std::shared_ptr<NativeFecProtectionCallback> Callback() {
    std::lock_guard<std::mutex> lock(callback_mutex_);
    return callback_;
  }
  rust::Box<RustFecController> source_;
  std::mutex callback_mutex_;
  std::shared_ptr<NativeFecProtectionCallback> callback_;
};
class FactoryAdapter final : public webrtc::FecControllerFactoryInterface {
 public:
  explicit FactoryAdapter(rust::Box<RustFecControllerFactory> source)
      : source_(std::move(source)) {}
  std::unique_ptr<webrtc::FecController> CreateFecController(
      const webrtc::Environment& env) override {
    auto source = fec_controller_create(*source_, retain_environment_from_sdk(env));
    if (fec_controller_builtin(*source))
      return std::make_unique<webrtc::FecControllerDefault>(env);
    // The native video-send caller unconditionally dereferences this product.
    // Factory failure must leave a non-null rejecting custom adapter.
    return std::make_unique<ControllerAdapter>(std::move(source));
  }
 private:
  rust::Box<RustFecControllerFactory> source_;
};
}  // namespace
std::unique_ptr<webrtc::FecControllerFactoryInterface> fec_factory_adapter(
    rust::Box<RustFecControllerFactory> source) noexcept {
  if (!fec_factory_present(*source)) return nullptr;
  return std::make_unique<FactoryAdapter>(std::move(source));
}
FfiFecProtectionResult fec_protection_request(
    const NativeFecProtectionCallback& source, FfiFecProtectionParams delta,
    FfiFecProtectionParams key) noexcept {
  auto& state = source.state();
  std::lock_guard<std::mutex> lock(state.mutex);
  if (auto status = LoanStatus(source)) return Failure(status);
  if (!OnWorker(state)) return Failure(2);
  if (!Valid(delta) || !Valid(key)) return Failure(4);
  const auto native_delta = Params(delta);
  const auto native_key = Params(key);
  FfiFecProtectionResult result{};
  result.native_result = state.callback->ProtectionRequest(&native_delta,
      &native_key, &result.video_bps, &result.nack_bps, &result.fec_bps);
  return result;
}
std::uint8_t fec_set_retransmission_mode(
    const NativeFecProtectionCallback& source, std::int32_t mode) noexcept {
  auto& state = source.state();
  std::lock_guard<std::mutex> lock(state.mutex);
  if (auto status = LoanStatus(source)) return status;
  // RtpVideoSender::OnBitrateUpdated holds its own nonrecursive mutex across
  // UpdateFecRates; SetRetransmissionMode would try to take that same mutex.
  if (current_kind == LoanKind::Rates) return 5;
  state.callback->SetRetransmissionMode(mode);
  return 0;
}
void fec_protection_request_async(
    std::shared_ptr<NativeFecProtectionCallback> source,
    FfiFecProtectionParams delta, FfiFecProtectionParams key,
    rust::Box<RustFecCompletion> completion) noexcept {
  auto& state = source->state();
  // Keep an extra completion owner until after the posting lock is released.
  // A rejecting/shutting-down queue can destroy the submitted task inline;
  // waking Rust under the state lock would permit a reentrant deadlock.
  auto finish = std::make_shared<QueuedCompletion>(std::move(completion));
  std::unique_lock<std::mutex> lock(state.mutex);
  if (!state.callback || !state.queue) {
    auto result = Failure(state.callback ? 2 : 1);
    lock.unlock();
    finish->Complete(result);
    return;
  }
  state.queue->PostTask([source = std::move(source), delta, key, finish]() mutable {
    FfiFecProtectionResult result;
    { Loan loan(source.get(), LoanKind::Scheduled);
      result = fec_protection_request(*source, delta, key); }
    finish->Complete(result);
  });
}
void fec_set_retransmission_mode_async(
    std::shared_ptr<NativeFecProtectionCallback> source, std::int32_t mode,
    rust::Box<RustFecCompletion> completion) noexcept {
  auto& state = source->state();
  auto finish = std::make_shared<QueuedCompletion>(std::move(completion));
  std::unique_lock<std::mutex> lock(state.mutex);
  if (!state.callback || !state.queue) {
    auto result = Failure(state.callback ? 2 : 1);
    lock.unlock();
    finish->Complete(result);
    return;
  }
  state.queue->PostTask([source = std::move(source), mode, finish]() mutable {
    FfiFecProtectionResult result;
    { Loan loan(source.get(), LoanKind::Scheduled);
      result = Failure(fec_set_retransmission_mode(*source, mode)); }
    finish->Complete(result);
  });
}
std::uint32_t fec_native_probe(const NativeEnvironment& environment,
    rust::Box<RustFecControllerFactory> source) noexcept {
  class Protection final : public webrtc::VCMProtectionCallback {
   public:
    int ProtectionRequest(const webrtc::FecProtectionParams* delta,
        const webrtc::FecProtectionParams* key, std::uint32_t* video,
        std::uint32_t* nack, std::uint32_t* fec) override {
      *video = delta->fec_rate;
      *nack = key->max_fec_frames;
      *fec = static_cast<std::uint32_t>(key->fec_mask_type);
      return last_mode;
    }
    void SetRetransmissionMode(int mode) override { last_mode = mode; }
    int last_mode = 0;
  } protection;
  auto factory = fec_factory_adapter(std::move(source));
  if (!factory) return 0;
  auto product = factory->CreateFecController(environment.environment());
  factory.reset();
  product->SetProtectionCallback(&protection);
  product->SetProtectionMethod(true, false);
  product->SetEncodingData(640, 360, 3, 1234);
  product->UpdateWithEncodedData(71, webrtc::VideoFrameType::kVideoFrameKey);
  product->UpdateWithEncodedData(0, webrtc::VideoFrameType::kEmptyFrame);
  product->UpdateWithEncodedData(89, static_cast<webrtc::VideoFrameType>(-19));
  product->UseLossVectorMask();
  return product->UpdateFecRates(123456, -7, 255, {true, false, true}, INT64_MIN);
}
}  // namespace pulsebeam::webrtc_sys
