#pragma once
#include <memory>
#include "api/fec_controller.h"
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
class NativeEnvironment;
struct RustFecControllerFactory;
struct RustFecCompletion;
struct FfiFecProtectionParams;
struct FfiFecProtectionResult;
// Shared state, not shared ownership of the native sender. Synchronous calls
// require an invocation loan; worker tasks check retirement at execution.
class NativeFecProtectionCallback final {
 public:
  struct State;
  explicit NativeFecProtectionCallback(webrtc::VCMProtectionCallback* callback);
  ~NativeFecProtectionCallback();
  void Retire() noexcept;
  State& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<webrtc::FecControllerFactoryInterface> fec_factory_adapter(
    rust::Box<RustFecControllerFactory> source) noexcept;
FfiFecProtectionResult fec_protection_request(
    const NativeFecProtectionCallback& source, FfiFecProtectionParams delta,
    FfiFecProtectionParams key) noexcept;
std::uint8_t fec_set_retransmission_mode(
    const NativeFecProtectionCallback& source, std::int32_t mode) noexcept;
void fec_protection_request_async(
    std::shared_ptr<NativeFecProtectionCallback> source,
    FfiFecProtectionParams delta, FfiFecProtectionParams key,
    rust::Box<RustFecCompletion> completion) noexcept;
void fec_set_retransmission_mode_async(
    std::shared_ptr<NativeFecProtectionCallback> source, std::int32_t mode,
    rust::Box<RustFecCompletion> completion) noexcept;
std::uint32_t fec_native_probe(const NativeEnvironment& environment,
    rust::Box<RustFecControllerFactory> source) noexcept;
}  // namespace pulsebeam::webrtc_sys
