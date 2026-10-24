#pragma once
#include <cstddef>
#include <cstdint>
#include <memory>
#include "rust/cxx.h"
#include "rtc_base/ssl_certificate.h"

namespace pulsebeam::webrtc_sys {
struct RustTlsCertificateVerifier;
struct FfiTlsCertificateStats;
class NativeTlsCertificate final {
 public:
  explicit NativeTlsCertificate(std::unique_ptr<webrtc::SSLCertificate> certificate) noexcept;
  const webrtc::SSLCertificate& certificate() const noexcept;
  std::unique_ptr<webrtc::SSLCertificate> Take() noexcept;
 private:
  std::unique_ptr<webrtc::SSLCertificate> certificate_;
};
class NativeTlsCertificateChain final {
 public:
  explicit NativeTlsCertificateChain(std::unique_ptr<webrtc::SSLCertChain> chain) noexcept;
  const webrtc::SSLCertChain& chain() const noexcept;
  std::unique_ptr<webrtc::SSLCertChain> Take() noexcept;
  void Append(std::unique_ptr<NativeTlsCertificate> certificate) noexcept;
 private:
  std::unique_ptr<webrtc::SSLCertChain> chain_;
};
std::unique_ptr<NativeTlsCertificate> tls_certificate_from_pem(rust::Str pem) noexcept;
std::unique_ptr<NativeTlsCertificate> tls_certificate_clone(const NativeTlsCertificate& certificate) noexcept;
rust::String tls_certificate_pem(const NativeTlsCertificate& certificate) noexcept;
rust::Vec<std::uint8_t> tls_certificate_der(const NativeTlsCertificate& certificate) noexcept;
rust::String tls_certificate_signature_digest(const NativeTlsCertificate& certificate, bool& valid) noexcept;
rust::Vec<std::uint8_t> tls_certificate_digest(const NativeTlsCertificate& certificate, rust::Str algorithm, std::size_t capacity, bool& valid) noexcept;
std::int64_t tls_certificate_expiration(const NativeTlsCertificate& certificate) noexcept;
rust::Vec<FfiTlsCertificateStats> tls_certificate_stats(const NativeTlsCertificate& certificate) noexcept;
std::unique_ptr<NativeTlsCertificateChain> tls_chain_new() noexcept;
void tls_chain_append(NativeTlsCertificateChain& chain, std::unique_ptr<NativeTlsCertificate> certificate) noexcept;
std::unique_ptr<NativeTlsCertificateChain> tls_chain_clone(const NativeTlsCertificateChain& chain) noexcept;
std::size_t tls_chain_size(const NativeTlsCertificateChain& chain) noexcept;
std::unique_ptr<NativeTlsCertificate> tls_chain_get(const NativeTlsCertificateChain& chain, std::size_t index) noexcept;
rust::Vec<FfiTlsCertificateStats> tls_chain_stats(const NativeTlsCertificateChain& chain) noexcept;
// Shared backing for verifiers constructed by this binding. The SDK owns a
// small proxy; socket creation contexts retain the actual backing independently.
class NativeTlsVerifier final {
 public:
  struct State;
  explicit NativeTlsVerifier(rust::Box<RustTlsCertificateVerifier> source) noexcept;
  explicit NativeTlsVerifier(std::unique_ptr<webrtc::SSLCertificateVerifier> source) noexcept;
  ~NativeTlsVerifier();
  bool Verify(const webrtc::SSLCertificate& certificate) const;
  bool VerifyChain(const webrtc::SSLCertChain& chain) const;
  bool Failed() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
// Private closed-origin operation: non-null inputs are the peer dependency
// proxy made by the two adapters below, never foreign C++ verifiers.
std::shared_ptr<NativeTlsVerifier> retain_binding_peer_tls_verifier(webrtc::SSLCertificateVerifier* verifier) noexcept;
bool tls_verifier_verify(const NativeTlsVerifier& verifier, const NativeTlsCertificate& certificate) noexcept;
bool tls_verifier_verify_chain(const NativeTlsVerifier& verifier, const NativeTlsCertificateChain& chain) noexcept;
bool tls_verifier_failed(const NativeTlsVerifier& verifier) noexcept;
std::unique_ptr<webrtc::SSLCertificateVerifier> tls_certificate_verifier_adapter(rust::Box<RustTlsCertificateVerifier> verifier) noexcept;
// Internal TURN-CA component only. Calls are serialized and invoke no user
// callbacks. This does not admit arbitrary externally supplied C++ verifiers.
std::unique_ptr<webrtc::SSLCertificateVerifier> owned_native_tls_certificate_verifier_adapter(std::unique_ptr<webrtc::SSLCertificateVerifier> verifier) noexcept;
}
