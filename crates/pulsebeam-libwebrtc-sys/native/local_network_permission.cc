#include "pulsebeam-webrtc-sys/native/local_network_permission.h"
#include "pulsebeam-webrtc-sys/native/socket_address.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <mutex>
#include <utility>
#include <vector>
#include "rtc_base/thread.h"
namespace pulsebeam::webrtc_sys {
struct NativeLocalNetworkPermissionCompletion::State {
  using Callback = absl::AnyInvocable<void(webrtc::LocalNetworkAccessPermissionStatus)>;
  explicit State(Callback callback) : thread(webrtc::Thread::Current()), callback(std::move(callback)) {}
  void Cancel() {
    std::lock_guard<std::mutex> lock(mutex);
    thread = nullptr;
    callback = nullptr;
  }
  void Run(bool granted) {
    Callback pending;
    {
      std::lock_guard<std::mutex> lock(mutex);
      if (!thread || !callback) return;
      pending = std::move(callback);
      thread = nullptr;
    }
    // Completion may destroy the permission and its owning port. No adapter
    // member or raw thread is accessed after entering the callback frame.
    pending(granted ? webrtc::LocalNetworkAccessPermissionStatus::kGranted : webrtc::LocalNetworkAccessPermissionStatus::kDenied);
  }
  std::mutex mutex;
  webrtc::Thread* thread;
  Callback callback;
  bool submitted = false;
};
NativeLocalNetworkPermissionCompletion::NativeLocalNetworkPermissionCompletion(std::shared_ptr<State> state) : state(std::move(state)) {}
std::unique_ptr<NativeLocalNetworkPermissionCompletion> local_permission_completion_clone(const NativeLocalNetworkPermissionCompletion& completion) { return std::make_unique<NativeLocalNetworkPermissionCompletion>(completion.state); }
bool local_permission_complete(const NativeLocalNetworkPermissionCompletion& completion, bool granted) {
  auto state = completion.state;
  std::lock_guard<std::mutex> lock(state->mutex);
  if (!state->thread || !state->callback || state->submitted) return false;
  state->submitted = true;
  // Posting while holding the cancellation mutex prevents a retained raw
  // thread from racing product retirement. Delivery always checks live state.
  state->thread->PostTask([state, granted] { state->Run(granted); });
  return true;
}
namespace {
class Permission final : public webrtc::LocalNetworkAccessPermissionInterface {
 public:
  explicit Permission(rust::Box<RustLocalNetworkPermission> provider) : provider_(std::move(provider)) {}
  ~Permission() override { for (auto& state : requests_) state->Cancel(); }
  bool ShouldRequestPermission(const webrtc::SocketAddress& address) override {
    return local_permission_should_request(*provider_, std::make_unique<NativeSocketAddress>(address));
  }
  void RequestPermission(const webrtc::SocketAddress& address, absl::AnyInvocable<void(webrtc::LocalNetworkAccessPermissionStatus)> callback) override {
    if (!webrtc::Thread::Current()) {
      // No dispatch sequence exists. Do not call Rust or retain a raw pointer;
      // rejection on the current sequence is safe for the native consumer.
      std::move(callback)(webrtc::LocalNetworkAccessPermissionStatus::kDenied);
      return;
    }
    auto state = std::make_shared<NativeLocalNetworkPermissionCompletion::State>(std::move(callback));
    requests_.push_back(state);
    local_permission_request(*provider_, std::make_unique<NativeSocketAddress>(address), std::make_unique<NativeLocalNetworkPermissionCompletion>(state));
  }
 private:
  rust::Box<RustLocalNetworkPermission> provider_;
  std::vector<std::shared_ptr<NativeLocalNetworkPermissionCompletion::State>> requests_;
};
class Factory final : public webrtc::LocalNetworkAccessPermissionFactoryInterface {
 public:
  explicit Factory(rust::Box<RustLocalNetworkPermissionFactory> provider) : provider_(std::move(provider)) {}
  std::unique_ptr<webrtc::LocalNetworkAccessPermissionInterface> Create() override {
    // The SDK immediately dereferences Create's product; failed Rust creation
    // must produce a rejecting object, never an invalid native null product.
    return std::make_unique<Permission>(local_permission_create(*provider_));
  }
 private:
  rust::Box<RustLocalNetworkPermissionFactory> provider_;
};
}
std::unique_ptr<webrtc::LocalNetworkAccessPermissionFactoryInterface> MakeLocalNetworkPermissionFactory(rust::Box<RustLocalNetworkPermissionFactory> factory) {
  if (!local_permission_factory_present(*factory)) return nullptr;
  return std::make_unique<Factory>(std::move(factory));
}
}  // namespace pulsebeam::webrtc_sys
