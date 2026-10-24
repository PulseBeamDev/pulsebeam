#include "pulsebeam-webrtc-sys/native/async_dns.h"
#include "pulsebeam-webrtc-sys/native/socket_address.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "api/task_queue/task_queue_base.h"
#include "rtc_base/thread.h"
#include <functional>
#include <mutex>
#include <utility>
#include <vector>

namespace pulsebeam::webrtc_sys {
namespace {
using Callback = absl::AnyInvocable<void()>;
struct ResolverScope : std::enable_shared_from_this<ResolverScope> {
  std::shared_ptr<NativeDnsResolutionCompletion::Request> Begin(Callback callback);
  void Retire();
  std::mutex mutex;
  // Borrowed only until product destruction on this exact sequence. Native
  // owners must destroy sequence-affine products before deleting their queue;
  // peer ownership retains factory role queues through native peer teardown.
  webrtc::TaskQueueBase* queue = nullptr;
  bool retired = false;
  std::shared_ptr<NativeDnsResolutionCompletion::Request> pending;
};
}
struct NativeDnsResolutionCompletion::Request : std::enable_shared_from_this<Request> {
  Request(std::shared_ptr<ResolverScope> scope, Callback callback)
      : scope(std::move(scope)), callback(std::move(callback)) {}
  std::uint8_t Complete() {
    std::lock_guard lock(scope->mutex);
    if (scope->retired) return 1;
    if (scope->pending.get() != this) return 2;
    if (!callback || submitted) return 3;
    if (!scope->queue) return 4;
    submitted = true;
    scope->queue->PostTask([request = shared_from_this()] { request->Run(); });
    return 0;
  }
  void Discard() {
    std::lock_guard lock(scope->mutex);
    if (scope->retired || !callback || submitted) return;
    // In the supported native path a current queue exists. Without one, no
    // arbitrary Rust thread may release sequence-affine captures. Retain the
    // request on its product until same-sequence retirement instead.
    if (!scope->queue) return;
    submitted = true;
    scope->queue->PostTask([request = shared_from_this()] { request->Cancel(); });
  }
  bool Cancel() {
    Callback discarded;
    {
      std::lock_guard lock(scope->mutex);
      discarded = std::move(callback);
    }
    const bool cancelled = static_cast<bool>(discarded);
    discarded = nullptr;
    return cancelled;
  }
  void Run() {
    Callback pending;
    {
      std::lock_guard lock(scope->mutex);
      if (scope->retired || scope->pending.get() != this || !callback) return;
      pending = std::move(callback);
    }
    pending();
    // Invocation may delete the product. Access only this request and locals.
    pending = nullptr;
  }
  std::shared_ptr<ResolverScope> scope;
  Callback callback;
  bool submitted = false;
};
namespace {
std::shared_ptr<NativeDnsResolutionCompletion::Request> ResolverScope::Begin(Callback callback) {
  auto request = std::make_shared<NativeDnsResolutionCompletion::Request>(shared_from_this(), std::move(callback));
  Callback discarded;
  {
    std::lock_guard lock(mutex);
    if (retired) {
      discarded = std::move(request->callback);
    } else {
      if (pending) discarded = std::move(pending->callback);
      queue = webrtc::TaskQueueBase::Current();
      pending = request;
    }
  }
  discarded = nullptr;
  return request;
}
void ResolverScope::Retire() {
  Callback discarded;
  {
    std::lock_guard lock(mutex);
    retired = true;
    queue = nullptr;
    if (pending) discarded = std::move(pending->callback);
    pending.reset();
  }
  discarded = nullptr;
}
struct ResolverSource {
  explicit ResolverSource(rust::Box<RustAsyncDnsResolver> source) : source(std::move(source)) {}
  rust::Box<RustAsyncDnsResolver> source;
};
struct ResultSource {
  explicit ResultSource(rust::Box<RustAsyncDnsResult> source) : source(std::move(source)) {}
  rust::Box<RustAsyncDnsResult> source;
};
class Result final : public webrtc::AsyncDnsResolverResult, public std::enable_shared_from_this<Result> {
 public:
  explicit Result(rust::Box<RustAsyncDnsResult> source) : source_(std::make_shared<ResultSource>(std::move(source))) {}
  std::size_t identity() const { return async_dns_result_identity(*source_->source); }
  bool Failed() const { return async_dns_result_failed(*source_->source); }
  mutable bool calling = false;
  bool GetResolvedAddress(int family, webrtc::SocketAddress* address) const override {
    auto source = source_;
    auto resolved = async_dns_result_address(*source->source, family);
    if (!resolved) return false;
    *address = resolved->value;
    return true;
  }
  int GetError() const override {
    auto source = source_;
    return async_dns_result_error(*source->source);
  }
 private:
  std::shared_ptr<ResultSource> source_;
};
struct ResultViews { std::vector<std::shared_ptr<Result>> values; };
class Resolver final : public webrtc::AsyncDnsResolverInterface {
 public:
  Resolver(rust::Box<RustAsyncDnsResolver> source, std::shared_ptr<ResolverScope> scope)
      : source_(std::make_shared<ResolverSource>(std::move(source))), scope_(std::move(scope)), results_(std::make_shared<ResultViews>()) {}
  ~Resolver() override { scope_->Retire(); }
  bool Failed() const { return async_dns_source_failed(*source_->source); }
  void Start(const webrtc::SocketAddress& address, Callback callback) override {
    StartImpl(address, false, 0, std::move(callback));
  }
  void Start(const webrtc::SocketAddress& address, int family, Callback callback) override {
    StartImpl(address, true, family, std::move(callback));
  }
  const webrtc::AsyncDnsResolverResult& result() const override {
    auto source = source_;
    auto results = results_;
    auto view = async_dns_result_view(*source->source);
    const auto identity = async_dns_result_identity(*view);
    for (const auto& existing : results->values) {
      if (existing->identity() == identity) return *existing;
    }
    results->values.push_back(std::make_shared<Result>(std::move(view)));
    return *results->values.back();
  }
 private:
  void StartImpl(const webrtc::SocketAddress& address, bool has_family, int family, Callback callback) {
    // Old callback disposal can reenter product ownership. Retain both sources
    // before Begin, and never read product members after calling into Rust.
    auto source = source_;
    auto scope = scope_;
    auto input = std::make_unique<NativeSocketAddress>(address);
    auto request = scope->Begin(std::move(callback));
    async_dns_start(*source->source, std::move(input), has_family, family,
        std::make_unique<NativeDnsResolutionCompletion>(std::move(request)));
  }
  std::shared_ptr<ResolverSource> source_;
  std::shared_ptr<ResolverScope> scope_;
  std::shared_ptr<ResultViews> results_;
};
struct FactorySource {
  explicit FactorySource(rust::Box<RustAsyncDnsFactory> source) : source(std::move(source)) {}
  rust::Box<RustAsyncDnsFactory> source;
};
class Factory final : public webrtc::AsyncDnsResolverFactoryInterface {
 public:
  explicit Factory(rust::Box<RustAsyncDnsFactory> source) : source_(std::make_shared<FactorySource>(std::move(source))) {}
  std::unique_ptr<webrtc::AsyncDnsResolverInterface> Create() override {
    auto source = source_;
    return std::make_unique<Resolver>(async_dns_factory_create(*source->source), std::make_shared<ResolverScope>());
  }
  std::unique_ptr<webrtc::AsyncDnsResolverInterface> CreateAndResolve(const webrtc::SocketAddress& address, Callback callback) override {
    return CreateStarted(address, false, 0, std::move(callback));
  }
  std::unique_ptr<webrtc::AsyncDnsResolverInterface> CreateAndResolve(const webrtc::SocketAddress& address, int family, Callback callback) override {
    return CreateStarted(address, true, family, std::move(callback));
  }
 private:
  std::unique_ptr<webrtc::AsyncDnsResolverInterface> CreateStarted(const webrtc::SocketAddress& address, bool has_family, int family, Callback callback) {
    auto scope = std::make_shared<ResolverScope>();
    auto request = scope->Begin(std::move(callback));
    auto factory = source_;
    auto source = async_dns_factory_create_and_resolve(*factory->source, std::make_unique<NativeSocketAddress>(address), has_family, family,
        std::make_unique<NativeDnsResolutionCompletion>(std::move(request)));
    return std::make_unique<Resolver>(std::move(source), std::move(scope));
  }
  std::shared_ptr<FactorySource> source_;
};
}
// Establish the actual owning Thread queue on its existing OS thread. This
// supports caller-owned controlled entries without dispatch, wait or time change.
class DnsRoleScope : public webrtc::TaskQueueBase {
 public:
  static void Run(webrtc::Thread* role, const std::function<void()>& body) {
    CurrentTaskQueueSetter current(role);
    body();
  }
};
struct NativeOwnedDnsResolver::Core {
  explicit Core(rust::Box<RustSocketRole> role) : role(std::make_shared<rust::Box<RustSocketRole>>(std::move(role))) {}
  ~Core() {
    DnsRoleScope::Run(thread, [&] { Close(); resolver.reset(); scope.reset(); role.reset(); });
  }
  std::uint8_t Status() const {
    if (closed) return 1;
    if (!thread->IsCurrent() || (webrtc::TaskQueueBase::Current() && webrtc::TaskQueueBase::Current() != thread)) return 3;
    if (calling) return 4;
    return resolver->Failed() ? 5 : 0;
  }
  void Close() { if (!closed) { closed = true; scope->Retire(); } }
  std::shared_ptr<rust::Box<RustSocketRole>> role;  // Released last.
  webrtc::Thread* thread = webrtc::Thread::Current();
  bool closed = false;
  bool calling = false;
  bool started = false;
  std::shared_ptr<ResolverScope> scope = std::make_shared<ResolverScope>();
  std::unique_ptr<Resolver> resolver;
};
NativeOwnedDnsResolver::NativeOwnedDnsResolver(std::shared_ptr<Core> value) noexcept : core(std::move(value)) {}
NativeOwnedDnsResolver::~NativeOwnedDnsResolver() {
  auto owner = std::move(core);
  DnsRoleScope::Run(owner->thread, [&] { owner->Close(); owner.reset(); });
}
NativeDnsResolverContext::NativeDnsResolverContext(std::weak_ptr<NativeOwnedDnsResolver::Core> value) noexcept : core(std::move(value)) {}
std::unique_ptr<NativeOwnedDnsResolver> owned_dns_new(rust::Box<RustAsyncDnsResolver> source, rust::Box<RustSocketRole> role) noexcept {
  auto* thread = webrtc::Thread::Current();
  if (!thread || (webrtc::TaskQueueBase::Current() && webrtc::TaskQueueBase::Current() != thread)) return nullptr;
  auto core = std::make_shared<NativeOwnedDnsResolver::Core>(std::move(role));
  core->resolver = std::make_unique<Resolver>(std::move(source), core->scope);
  return std::make_unique<NativeOwnedDnsResolver>(std::move(core));
}
std::shared_ptr<NativeDnsResolverContext> owned_dns_context(const NativeOwnedDnsResolver& resolver) noexcept {
  return std::make_shared<NativeDnsResolverContext>(resolver.core);
}
struct NativeDnsQuery::State {
  std::weak_ptr<NativeOwnedDnsResolver::Core> core;
  std::shared_ptr<NativeDnsResolutionCompletion::Request> request;
};
NativeDnsQuery::NativeDnsQuery(std::unique_ptr<State> value) noexcept : state(std::move(value)) {}
NativeDnsQuery::~NativeDnsQuery() { Cancel(); }
bool NativeDnsQuery::Cancel() const noexcept {
  auto owner = state->core.lock();
  if (!owner || owner->closed || !owner->thread->IsCurrent()) return false;
  bool cancelled = false;
  DnsRoleScope::Run(owner->thread, [&] { cancelled = state->request->Cancel(); });
  return cancelled;
}
bool owned_dns_query_cancel(const NativeDnsQuery& query) noexcept { return query.Cancel(); }
FfiOwnedDnsStart owned_dns_start(const NativeDnsResolverContext& context, const NativeSocketAddress& address,
    bool has_family, std::int32_t family, rust::Box<RustDnsCallback> callback) noexcept {
  auto owner = context.core.lock();
  if (!owner) return {1, {}};
  auto status = owner->Status();
  if (status) return {status, {}};
  if (owner->started) return {13, {}};
  owner->started = true;
  owner->calling = true;
  struct Calling { bool& flag; ~Calling() { flag = false; } } calling{owner->calling};
  // A completed notification moves its Rust Box out before calling it. The
  // remaining queued shell owns weak metadata only, never role-local captures.
  auto capture = std::make_shared<std::unique_ptr<rust::Box<RustDnsCallback>>>(std::make_unique<rust::Box<RustDnsCallback>>(std::move(callback)));
  struct CaptureCleanup {
    webrtc::Thread* thread;
    decltype(capture)& value;
    ~CaptureCleanup() { DnsRoleScope::Run(thread, [&] { value.reset(); }); }
  } capture_cleanup{owner->thread, capture};
  Callback notify = [weak = std::weak_ptr<NativeOwnedDnsResolver::Core>(owner), capture] {
    auto frame = weak.lock();
    if (!frame || frame->closed || !*capture) return;
    auto body = std::move(*capture);
    owned_dns_callback_fire(std::move(*body));
  };
  DnsRoleScope::Run(owner->thread, [&] {
    if (has_family) owner->resolver->Start(address.value, family, std::move(notify));
    else owner->resolver->Start(address.value, std::move(notify));
  });
  if (owner->closed) return {1, {}};
  if (owner->resolver->Failed()) {
    auto pending = owner->scope->pending;
    if (pending) DnsRoleScope::Run(owner->thread, [&] { pending->Cancel(); });
    return {5, {}};
  }
  auto state = std::make_unique<NativeDnsQuery::State>();
  state->core = owner;
  state->request = owner->scope->pending;
  return {0, std::make_unique<NativeDnsQuery>(std::move(state))};
}
struct NativeOwnedDnsResult::State {
  std::shared_ptr<rust::Box<RustSocketRole>> role;
  webrtc::Thread* thread;
  std::shared_ptr<const Result> result;
};
NativeOwnedDnsResult::NativeOwnedDnsResult(std::unique_ptr<State> value) noexcept : state(std::move(value)) {}
NativeOwnedDnsResult::~NativeOwnedDnsResult() {
  auto owner = std::move(state);
  DnsRoleScope::Run(owner->thread, [&] { owner.reset(); });
}
FfiOwnedDnsResult owned_dns_result(const NativeDnsResolverContext& context) noexcept {
  auto owner = context.core.lock();
  if (!owner) return {1, {}};
  auto status = owner->Status();
  if (status) return {status, {}};
  owner->calling = true;
  struct Calling { bool& flag; ~Calling() { flag = false; } } calling{owner->calling};
  std::shared_ptr<const Result> view;
  DnsRoleScope::Run(owner->thread, [&] { view = static_cast<const Result&>(owner->resolver->result()).shared_from_this(); });
  if (owner->closed) return {1, {}};
  if (owner->resolver->Failed()) return {5, {}};
  auto state = std::make_unique<NativeOwnedDnsResult::State>();
  state->role = owner->role; state->thread = owner->thread; state->result = std::move(view);
  return {0, std::make_unique<NativeOwnedDnsResult>(std::move(state))};
}
FfiOwnedSocketResult owned_dns_result_call(const NativeOwnedDnsResult& result, bool address, std::int32_t family) noexcept {
  const auto& state = *result.state;
  FfiOwnedSocketResult output{};
  if (!state.thread->IsCurrent() || (webrtc::TaskQueueBase::Current() && webrtc::TaskQueueBase::Current() != state.thread)) { output.status = 3; return output; }
  auto role = state.role;  // Keeps actual resources through query/capture Drop.
  auto view = state.result;
  if (view->calling) { output.status = 4; return output; }
  if (view->Failed()) { output.status = 5; return output; }
  view->calling = true;
  struct Calling { bool& flag; ~Calling() { flag = false; } } calling{view->calling};
  DnsRoleScope::Run(state.thread, [&] {
    if (address) { webrtc::SocketAddress resolved; if (view->GetResolvedAddress(family, &resolved)) output.address = std::make_unique<NativeSocketAddress>(resolved); }
    else output.code = view->GetError();
  });
  if (view->Failed()) output.status = 5;
  return output;
}
NativeDnsResolutionCompletion::NativeDnsResolutionCompletion(std::shared_ptr<Request> request) : request(std::move(request)) {}
NativeDnsResolutionCompletion::~NativeDnsResolutionCompletion() { request->Discard(); }
std::uint8_t dns_resolution_complete(const NativeDnsResolutionCompletion& completion) noexcept { return completion.request->Complete(); }
std::unique_ptr<webrtc::AsyncDnsResolverInterface> async_dns_resolver_adapter(rust::Box<RustAsyncDnsResolver> source) {
  return std::make_unique<Resolver>(std::move(source), std::make_shared<ResolverScope>());
}
std::unique_ptr<webrtc::AsyncDnsResolverFactoryInterface> async_dns_factory_adapter(rust::Box<RustAsyncDnsFactory> source) {
  if (!async_dns_factory_present(*source)) return nullptr;
  return std::make_unique<Factory>(std::move(source));
}
}  // namespace pulsebeam::webrtc_sys
