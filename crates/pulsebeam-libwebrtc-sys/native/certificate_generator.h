#pragma once
#include <cstdint>
#include <memory>
#include "rust/cxx.h"
#include "rtc_base/rtc_certificate_generator.h"
#include "rtc_base/thread.h"

namespace pulsebeam::webrtc_sys {
class NativeCertificate;
struct RustCertificateGenerator;
// Owned by the peer factory, not an individual generator. SDK generation work
// may outlive its generator while the signaling queue is still available.
class CertificateGeneratorScope {
 public:
  struct State;
  explicit CertificateGeneratorScope(webrtc::Thread* signaling);
  void Retire();
  std::shared_ptr<State> state;
};
class NativeCertificateCompletion {
 public:
  struct Request;
  explicit NativeCertificateCompletion(std::shared_ptr<Request> request);
  ~NativeCertificateCompletion();
  std::shared_ptr<Request> request;
};
std::uint8_t certificate_complete_success(const NativeCertificateCompletion& completion, const NativeCertificate& certificate) noexcept;
std::uint8_t certificate_complete_failure(const NativeCertificateCompletion& completion) noexcept;
std::unique_ptr<webrtc::RTCCertificateGeneratorInterface> certificate_generator_adapter(
    rust::Box<RustCertificateGenerator> source, const CertificateGeneratorScope& scope);
}  // namespace pulsebeam::webrtc_sys
