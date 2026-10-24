#pragma once
#include <cstdint>
#include <memory>
#include "rust/cxx.h"
#include "api/async_dns_resolver.h"
namespace pulsebeam::webrtc_sys {
struct RustAsyncDnsFactory;
struct RustAsyncDnsResolver;
struct RustSocketRole;
struct RustDnsCallback;
struct FfiOwnedDnsStart;
struct FfiOwnedDnsResult;
struct FfiOwnedSocketResult;
class NativeSocketAddress;
class NativeOwnedDnsResolver {
 public:
  struct Core;
  explicit NativeOwnedDnsResolver(std::shared_ptr<Core> core) noexcept;
  ~NativeOwnedDnsResolver();
  std::shared_ptr<Core> core;
};
class NativeDnsResolverContext {
 public:
  explicit NativeDnsResolverContext(std::weak_ptr<NativeOwnedDnsResolver::Core> core) noexcept;
  std::weak_ptr<NativeOwnedDnsResolver::Core> core;
};
class NativeOwnedDnsResult {
 public:
  struct State;
  explicit NativeOwnedDnsResult(std::unique_ptr<State> state) noexcept;
  ~NativeOwnedDnsResult();
  std::unique_ptr<State> state;
};
class NativeDnsQuery {
 public:
  struct State;
  explicit NativeDnsQuery(std::unique_ptr<State> state) noexcept;
  ~NativeDnsQuery();
  bool Cancel() const noexcept;
  std::unique_ptr<State> state;
};
std::unique_ptr<NativeOwnedDnsResolver> owned_dns_new(rust::Box<RustAsyncDnsResolver> source, rust::Box<RustSocketRole> role) noexcept;
std::shared_ptr<NativeDnsResolverContext> owned_dns_context(const NativeOwnedDnsResolver& resolver) noexcept;
FfiOwnedDnsStart owned_dns_start(const NativeDnsResolverContext& context, const NativeSocketAddress& address, bool has_family, std::int32_t family, rust::Box<RustDnsCallback> callback) noexcept;
FfiOwnedDnsResult owned_dns_result(const NativeDnsResolverContext& context) noexcept;
FfiOwnedSocketResult owned_dns_result_call(const NativeOwnedDnsResult& result, bool address, std::int32_t family) noexcept;
bool owned_dns_query_cancel(const NativeDnsQuery& query) noexcept;
std::unique_ptr<webrtc::AsyncDnsResolverInterface> async_dns_resolver_adapter(rust::Box<RustAsyncDnsResolver> source);
class NativeDnsResolutionCompletion {
 public:
  struct Request;
  explicit NativeDnsResolutionCompletion(std::shared_ptr<Request> request);
  ~NativeDnsResolutionCompletion();
  std::shared_ptr<Request> request;
};
std::uint8_t dns_resolution_complete(const NativeDnsResolutionCompletion& completion) noexcept;
std::unique_ptr<webrtc::AsyncDnsResolverFactoryInterface> async_dns_factory_adapter(rust::Box<RustAsyncDnsFactory> source);
}  // namespace pulsebeam::webrtc_sys
