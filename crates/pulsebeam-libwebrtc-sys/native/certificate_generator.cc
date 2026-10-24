#include "pulsebeam-webrtc-sys/native/certificate_generator.h"
#include "pulsebeam-webrtc-sys/native/peer.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <algorithm>
#include <mutex>
#include <utility>
#include <vector>

namespace pulsebeam::webrtc_sys {
using CertificateCallback = webrtc::RTCCertificateGeneratorInterface::Callback;
struct CertificateGeneratorScope::State {
  explicit State(webrtc::Thread* thread) : thread(thread) {}
  std::mutex mutex;
  webrtc::Thread* thread;
  std::vector<std::weak_ptr<NativeCertificateCompletion::Request>> requests;
};
struct NativeCertificateCompletion::Request : std::enable_shared_from_this<Request> {
  Request(std::shared_ptr<CertificateGeneratorScope::State> scope, CertificateCallback callback)
      : scope(std::move(scope)), callback(std::move(callback)) {}
  std::uint8_t Complete(webrtc::scoped_refptr<webrtc::RTCCertificate> certificate) {
    std::lock_guard lock(scope->mutex);
    if (!scope->thread) return 1;
    if (!callback || submitted) return 2;
    submitted = true;
    scope->thread->PostTask([request = shared_from_this(), certificate = std::move(certificate)]() mutable {
      request->Run(std::move(certificate));
    });
    return 0;
  }
  void Discard() {
    std::lock_guard lock(scope->mutex);
    if (!scope->thread || !callback || submitted) return;
    submitted = true;
    scope->thread->PostTask([request = shared_from_this()] { request->Cancel(); });
  }
  void Cancel() {
    CertificateCallback discarded;
    {
      std::lock_guard lock(scope->mutex);
      discarded = std::move(callback);
    }
    // The requesting signaling sequence disposes native captures, not Rust's
    // ticket-dropping thread. Capture destructors may reenter SDK ownership.
    discarded = nullptr;
  }
  void Run(webrtc::scoped_refptr<webrtc::RTCCertificate> certificate) {
    CertificateCallback pending;
    {
      std::lock_guard lock(scope->mutex);
      if (!scope->thread || !callback) return;
      pending = std::move(callback);
    }
    std::move(pending)(std::move(certificate));
    pending = nullptr;
  }
  std::shared_ptr<CertificateGeneratorScope::State> scope;
  CertificateCallback callback;
  bool submitted = false;
};
CertificateGeneratorScope::CertificateGeneratorScope(webrtc::Thread* signaling)
    : state(std::make_shared<State>(signaling)) {}
void CertificateGeneratorScope::Retire() {
  std::vector<CertificateCallback> discarded;
  {
    std::lock_guard lock(state->mutex);
    state->thread = nullptr;
    for (auto& weak : state->requests) {
      if (auto request = weak.lock()) discarded.push_back(std::move(request->callback));
    }
    state->requests.clear();
  }
  discarded.clear();
}
NativeCertificateCompletion::NativeCertificateCompletion(std::shared_ptr<Request> request)
    : request(std::move(request)) {}
NativeCertificateCompletion::~NativeCertificateCompletion() { request->Discard(); }
std::uint8_t certificate_complete_success(const NativeCertificateCompletion& completion, const NativeCertificate& certificate) noexcept {
  return completion.request->Complete(certificate.certificate());
}
std::uint8_t certificate_complete_failure(const NativeCertificateCompletion& completion) noexcept {
  return completion.request->Complete(nullptr);
}
namespace {
class Generator final : public webrtc::RTCCertificateGeneratorInterface {
 public:
  Generator(rust::Box<RustCertificateGenerator> source, std::shared_ptr<CertificateGeneratorScope::State> scope)
      : source_(std::move(source)), scope_(std::move(scope)) {}
  void GenerateCertificateAsync(const webrtc::KeyParams& params,
      const std::optional<uint64_t>& expires_ms, Callback callback) override {
    auto request = std::make_shared<NativeCertificateCompletion::Request>(scope_, std::move(callback));
    {
      std::lock_guard lock(scope_->mutex);
      // Interface entry is signaling-affine. Factory retirement cannot overlap
      // a valid call, but retained completion tickets can overlap retirement.
      if (!scope_->thread || scope_->thread != webrtc::Thread::Current()) return;
      scope_->requests.erase(std::remove_if(scope_->requests.begin(), scope_->requests.end(),
          [](const auto& weak) { return weak.expired(); }), scope_->requests.end());
      scope_->requests.push_back(request);
    }
    FfiCertificateKeyParams values{};
    if (params.type() == webrtc::KT_RSA) {
      values.kind = 0;
      values.modulus_bits = params.rsa_params().mod_size;
      values.public_exponent = params.rsa_params().pub_exp;
    } else {
      values.kind = 1;
      values.curve = static_cast<std::int32_t>(params.ec_curve());
    }
    certificate_generator_generate(*source_, values, expires_ms.has_value(), expires_ms.value_or(0),
        std::make_unique<NativeCertificateCompletion>(std::move(request)));
  }
 private:
  rust::Box<RustCertificateGenerator> source_;
  std::shared_ptr<CertificateGeneratorScope::State> scope_;
};
}  // namespace
std::unique_ptr<webrtc::RTCCertificateGeneratorInterface> certificate_generator_adapter(
    rust::Box<RustCertificateGenerator> source, const CertificateGeneratorScope& scope) {
  if (!certificate_generator_present(*source)) return nullptr;
  return std::make_unique<Generator>(std::move(source), scope.state);
}
}  // namespace pulsebeam::webrtc_sys
