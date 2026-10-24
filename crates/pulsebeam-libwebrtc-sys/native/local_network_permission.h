#pragma once
#include <memory>
#include <cstdint>
#include "rust/cxx.h"
#include "api/local_network_access_permission.h"
namespace pulsebeam::webrtc_sys {
struct RustLocalNetworkPermissionFactory;
class NativeLocalNetworkPermissionCompletion {
 public:
  struct State;
  explicit NativeLocalNetworkPermissionCompletion(std::shared_ptr<State> state);
  std::shared_ptr<State> state;
};
std::unique_ptr<NativeLocalNetworkPermissionCompletion> local_permission_completion_clone(const NativeLocalNetworkPermissionCompletion& completion);
bool local_permission_complete(const NativeLocalNetworkPermissionCompletion& completion, bool granted);
std::unique_ptr<webrtc::LocalNetworkAccessPermissionFactoryInterface> MakeLocalNetworkPermissionFactory(rust::Box<RustLocalNetworkPermissionFactory> factory);
}  // namespace pulsebeam::webrtc_sys
