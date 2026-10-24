#include "pulsebeam-webrtc-sys/native/tls.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <algorithm>
#include <climits>
#include <string>
#include <optional>
#include <mutex>
#include <utility>
#include <vector>
#include "openssl/digest.h"
#include "rtc_base/buffer.h"
#include "rtc_base/openssl.h"
#ifndef OPENSSL_IS_BORINGSSL
#error "Owned immutable TLS certificates require the qualified BoringSSL backend"
#endif

namespace pulsebeam::webrtc_sys {
namespace {
rust::Vec<std::uint8_t> Bytes(const webrtc::Buffer& buffer) {
  rust::Vec<std::uint8_t> result;
  for (std::uint8_t byte : buffer) result.push_back(byte);
  return result;
}
rust::Vec<FfiTlsCertificateStats> Stats(std::unique_ptr<webrtc::SSLCertificateStats> stats) {
  rust::Vec<FfiTlsCertificateStats> result;
  for (auto* member = stats.get(); member; member = member->issuer.get()) {
    result.push_back(FfiTlsCertificateStats{rust::String(member->fingerprint),
        rust::String(member->fingerprint_algorithm), rust::String(member->base64_certificate)});
  }
  return result;
}
class RustTlsVerifier final : public webrtc::SSLCertificateVerifier {
 public:
  explicit RustTlsVerifier(std::shared_ptr<NativeTlsVerifier> verifier) : verifier_(std::move(verifier)) {}
  bool Verify(const webrtc::SSLCertificate& certificate) override {
    auto lease = verifier_;
    return lease->Verify(certificate);
  }
  bool VerifyChain(const webrtc::SSLCertChain& chain) override {
    auto lease = verifier_;
    return lease->VerifyChain(chain);
  }
  const std::shared_ptr<NativeTlsVerifier>& backing() const { return verifier_; }
 private:
  std::shared_ptr<NativeTlsVerifier> verifier_;
};
}
struct NativeTlsVerifier::State {
  explicit State(rust::Box<RustTlsCertificateVerifier> value) : source(std::move(value)) {}
  explicit State(std::unique_ptr<webrtc::SSLCertificateVerifier> value) : native(std::move(value)) {}
  std::optional<rust::Box<RustTlsCertificateVerifier>> source;
  std::unique_ptr<webrtc::SSLCertificateVerifier> native;
  std::mutex native_mutex;
};
NativeTlsVerifier::NativeTlsVerifier(rust::Box<RustTlsCertificateVerifier> source) noexcept
    : state_(std::make_unique<State>(std::move(source))) {}
NativeTlsVerifier::NativeTlsVerifier(std::unique_ptr<webrtc::SSLCertificateVerifier> source) noexcept
    : state_(std::make_unique<State>(std::move(source))) {}
NativeTlsVerifier::~NativeTlsVerifier() = default;
bool NativeTlsVerifier::Verify(const webrtc::SSLCertificate& certificate) const {
  if (state_->source) return rust_tls_certificate_verify(**state_->source, std::make_unique<NativeTlsCertificate>(certificate.Clone()));
  std::lock_guard lock(state_->native_mutex);
  return state_->native->Verify(certificate);
}
bool NativeTlsVerifier::VerifyChain(const webrtc::SSLCertChain& chain) const {
  if (state_->source) return rust_tls_certificate_verify_chain(**state_->source, std::make_unique<NativeTlsCertificateChain>(chain.Clone()));
  std::lock_guard lock(state_->native_mutex);
  return state_->native->VerifyChain(chain);
}
bool NativeTlsVerifier::Failed() const noexcept { return state_->source && rust_tls_certificate_verifier_failed(**state_->source); }
std::shared_ptr<NativeTlsVerifier> retain_binding_peer_tls_verifier(webrtc::SSLCertificateVerifier* verifier) noexcept {
  // This helper is called only by the private factory installed in peer.cc.
  // SDK PeerConnection fills TURN options from its dependency verifier, which
  // this binding creates exclusively through the proxy below. No pointer/ID
  // registry or reinterpretation of arbitrary external verifiers is used.
  return verifier ? static_cast<RustTlsVerifier*>(verifier)->backing() : nullptr;
}
bool tls_verifier_verify(const NativeTlsVerifier& verifier, const NativeTlsCertificate& certificate) noexcept { return verifier.Verify(certificate.certificate()); }
bool tls_verifier_verify_chain(const NativeTlsVerifier& verifier, const NativeTlsCertificateChain& chain) noexcept { return verifier.VerifyChain(chain.chain()); }
bool tls_verifier_failed(const NativeTlsVerifier& verifier) noexcept { return verifier.Failed(); }
NativeTlsCertificate::NativeTlsCertificate(std::unique_ptr<webrtc::SSLCertificate> certificate) noexcept
    : certificate_(std::move(certificate)) {}
const webrtc::SSLCertificate& NativeTlsCertificate::certificate() const noexcept { return *certificate_; }
std::unique_ptr<webrtc::SSLCertificate> NativeTlsCertificate::Take() noexcept { return std::move(certificate_); }
NativeTlsCertificateChain::NativeTlsCertificateChain(std::unique_ptr<webrtc::SSLCertChain> chain) noexcept
    : chain_(std::move(chain)) {}
const webrtc::SSLCertChain& NativeTlsCertificateChain::chain() const noexcept { return *chain_; }
std::unique_ptr<webrtc::SSLCertChain> NativeTlsCertificateChain::Take() noexcept { return std::move(chain_); }
void NativeTlsCertificateChain::Append(std::unique_ptr<NativeTlsCertificate> certificate) noexcept {
  std::vector<std::unique_ptr<webrtc::SSLCertificate>> members;
  members.reserve(chain_->GetSize() + 1);
  for (std::size_t i = 0; i < chain_->GetSize(); ++i) members.push_back(chain_->Get(i).Clone());
  members.push_back(certificate->Take());
  chain_ = std::make_unique<webrtc::SSLCertChain>(std::move(members));
}
std::unique_ptr<NativeTlsCertificate> tls_certificate_from_pem(rust::Str pem) noexcept {
  if (pem.size() > INT_MAX) return nullptr;
  // The fallback OpenSSL parser uses a NUL-terminated BIO. Keep storage owned
  // and terminated even though the qualified BoringSSL parser is length-aware.
  const std::string owned(pem);
  auto certificate = webrtc::SSLCertificate::FromPEMString(owned);
  if (!certificate) return nullptr;
  return std::make_unique<NativeTlsCertificate>(std::move(certificate));
}
std::unique_ptr<NativeTlsCertificate> tls_certificate_clone(const NativeTlsCertificate& certificate) noexcept {
  return std::make_unique<NativeTlsCertificate>(certificate.certificate().Clone());
}
rust::String tls_certificate_pem(const NativeTlsCertificate& certificate) noexcept {
  return rust::String(certificate.certificate().ToPEMString());
}
rust::Vec<std::uint8_t> tls_certificate_der(const NativeTlsCertificate& certificate) noexcept {
  webrtc::Buffer result;
  certificate.certificate().ToDER(&result);
  return Bytes(result);
}
rust::String tls_certificate_signature_digest(const NativeTlsCertificate& certificate, bool& valid) noexcept {
  std::string result;
  valid = certificate.certificate().GetSignatureDigestAlgorithm(&result);
  return rust::String(result);
}
rust::Vec<std::uint8_t> tls_certificate_digest(const NativeTlsCertificate& certificate, rust::Str algorithm,
    std::size_t capacity, bool& valid) noexcept {
  valid = false;
  if (!capacity) return {};
  // Every recognized digest fits EVP_MAX_MD_SIZE. Capacity above that does
  // not change the result; avoid allocating an unbounded caller-sized buffer.
  webrtc::Buffer result(0, std::min(capacity, static_cast<std::size_t>(EVP_MAX_MD_SIZE)));
  valid = certificate.certificate().ComputeDigest(absl::string_view(algorithm.data(), algorithm.size()), result);
  return valid ? Bytes(result) : rust::Vec<std::uint8_t>();
}
std::int64_t tls_certificate_expiration(const NativeTlsCertificate& certificate) noexcept {
  return certificate.certificate().CertificateExpirationTime();
}
rust::Vec<FfiTlsCertificateStats> tls_certificate_stats(const NativeTlsCertificate& certificate) noexcept {
  return Stats(certificate.certificate().GetStats());
}
std::unique_ptr<NativeTlsCertificateChain> tls_chain_new() noexcept {
  return std::make_unique<NativeTlsCertificateChain>(
      std::make_unique<webrtc::SSLCertChain>(std::vector<std::unique_ptr<webrtc::SSLCertificate>>{}));
}
void tls_chain_append(NativeTlsCertificateChain& chain, std::unique_ptr<NativeTlsCertificate> certificate) noexcept {
  chain.Append(std::move(certificate));
}
std::unique_ptr<NativeTlsCertificateChain> tls_chain_clone(const NativeTlsCertificateChain& chain) noexcept {
  return std::make_unique<NativeTlsCertificateChain>(chain.chain().Clone());
}
std::size_t tls_chain_size(const NativeTlsCertificateChain& chain) noexcept { return chain.chain().GetSize(); }
std::unique_ptr<NativeTlsCertificate> tls_chain_get(const NativeTlsCertificateChain& chain, std::size_t index) noexcept {
  if (index >= chain.chain().GetSize()) return nullptr;
  return std::make_unique<NativeTlsCertificate>(chain.chain().Get(index).Clone());
}
rust::Vec<FfiTlsCertificateStats> tls_chain_stats(const NativeTlsCertificateChain& chain) noexcept {
  return Stats(chain.chain().GetStats());
}
std::unique_ptr<webrtc::SSLCertificateVerifier> tls_certificate_verifier_adapter(rust::Box<RustTlsCertificateVerifier> verifier) noexcept {
  if (!tls_certificate_verifier_present(*verifier)) return nullptr;
  return std::make_unique<RustTlsVerifier>(std::make_shared<NativeTlsVerifier>(std::move(verifier)));
}
std::unique_ptr<webrtc::SSLCertificateVerifier> owned_native_tls_certificate_verifier_adapter(std::unique_ptr<webrtc::SSLCertificateVerifier> verifier) noexcept {
  if (!verifier) return nullptr;
  return std::make_unique<RustTlsVerifier>(std::make_shared<NativeTlsVerifier>(std::move(verifier)));
}
}
