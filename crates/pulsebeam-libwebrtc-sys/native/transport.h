#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <utility>

#include "api/candidate.h"
#include "api/dtls_transport_interface.h"
#include "api/sctp_transport_interface.h"
#include "rust/cxx.h"

namespace webrtc { class Thread; }
namespace pulsebeam::webrtc_sys {
class NativePeerConnection;
class ReadinessSignal;
struct FfiDtlsInformation;
struct FfiSctpInformation;
class NativeRtcError;
struct FfiIceInternalValue;
struct FfiIceConfig;
FfiIceConfig ice_config_default() noexcept;
FfiIceConfig ice_config_resolve(const FfiIceConfig&) noexcept;
std::unique_ptr<NativeRtcError> ice_config_validate(const FfiIceConfig&) noexcept;
class NativeTlsCertificateChain;
struct FfiCertificateData;

struct FfiIcePacketEvent;
struct FfiIcePacketOptions;
struct FfiIcePacketState;
struct FfiIceSocketResult;
struct FfiIceOption;
struct FfiIceRouteResult;
FfiIcePacketOptions ice_packet_options_default() noexcept;
struct FfiTransportCandidate;
struct FfiTransportCandidateParse;
struct FfiIceStats;
struct FfiIcePair;
class NativeSocketAddress;
class NativeTransportCandidate final {
 public:
  explicit NativeTransportCandidate(webrtc::Candidate v) : value(std::move(v)) {}
  webrtc::Candidate value;
};
std::unique_ptr<NativeTransportCandidate> transport_candidate_default() noexcept;
std::unique_ptr<NativeTransportCandidate> transport_candidate_clone(const NativeTransportCandidate&) noexcept;
FfiTransportCandidate transport_candidate_fields(const NativeTransportCandidate&) noexcept;
FfiTransportCandidateParse transport_candidate_parse(rust::Slice<const std::uint8_t>) noexcept;
std::unique_ptr<NativeRtcError> transport_candidate_verify(const NativeTransportCandidate&) noexcept;
void transport_candidate_set_bytes(NativeTransportCandidate&,std::uint8_t,rust::Slice<const std::uint8_t>) noexcept;
void transport_candidate_set_integer(NativeTransportCandidate&,std::uint8_t,std::int64_t) noexcept;
void transport_candidate_set_address(NativeTransportCandidate&,bool,const NativeSocketAddress&) noexcept;
rust::Vec<std::uint8_t> transport_candidate_serialize(const NativeTransportCandidate&,std::uint8_t,bool) noexcept;
bool transport_candidate_equivalent(const NativeTransportCandidate&,const NativeTransportCandidate&,bool) noexcept;

class NativeDtlsInformation final {
 public:
  explicit NativeDtlsInformation(webrtc::DtlsTransportInformation value)
      : value_(std::move(value)) {}
  const webrtc::DtlsTransportInformation& value() const noexcept { return value_; }
  void Assign(const NativeDtlsInformation& other) noexcept { value_ = other.value_; }
  void MoveAssign(NativeDtlsInformation& other) noexcept { value_ = std::move(other.value_); }
 private:
  webrtc::DtlsTransportInformation value_;
};
class NativeSctpInformation final {
 public:
  explicit NativeSctpInformation(webrtc::SctpTransportInformation value)
      : value_(std::move(value)) {}
  const webrtc::SctpTransportInformation& value() const noexcept { return value_; }
  void Assign(const NativeSctpInformation& other) noexcept { value_ = other.value_; }
 private:
  webrtc::SctpTransportInformation value_;
};
class NativeDtlsEvent final {
 public:
  std::unique_ptr<NativeDtlsInformation> information;
  webrtc::RTCError error;
};

class NativeDtlsTransport final {
 public:
  struct State;
  explicit NativeDtlsTransport(std::unique_ptr<State> state) noexcept;
  ~NativeDtlsTransport();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeSctpTransport final {
 public:
  struct State;
  explicit NativeSctpTransport(std::unique_ptr<State> state) noexcept;
  ~NativeSctpTransport();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
// Retains only the public facade. Its internal pointer is fetched afresh within
// each serialized operation and is never stored or returned across tasks.
class NativeIceTransport final {
 public:
  struct State;
  explicit NativeIceTransport(std::unique_ptr<State> state) noexcept;
  ~NativeIceTransport();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::uint8_t ice_transport_register_packet_observer(const NativeIceTransport&,std::size_t) noexcept;
bool ice_transport_unregister_packet_observer(const NativeIceTransport&) noexcept;
FfiIcePacketEvent ice_transport_packet_event(const NativeIceTransport&) noexcept;
std::uint64_t ice_transport_packet_dropped(const NativeIceTransport&) noexcept;
FfiIcePacketState ice_transport_packet_state(const NativeIceTransport&) noexcept;
FfiIceSocketResult ice_transport_send_packet(const NativeIceTransport&,rust::Slice<const std::uint8_t>,const FfiIcePacketOptions&,std::int32_t) noexcept;
FfiIceSocketResult ice_transport_set_option(const NativeIceTransport&,std::uint8_t,std::int32_t) noexcept;
FfiIceOption ice_transport_get_option(const NativeIceTransport&,std::uint8_t) noexcept;
FfiIceRouteResult ice_transport_route(const NativeIceTransport&) noexcept;
bool ice_transport_candidate(const NativeIceTransport&,const NativeTransportCandidate&,bool) noexcept;
bool ice_transport_remove_all_candidates(const NativeIceTransport&) noexcept;
FfiIceStats ice_transport_stats(const NativeIceTransport&) noexcept;
FfiIcePair ice_transport_selected_pair(const NativeIceTransport&) noexcept;
bool ice_transport_config(const NativeIceTransport&, FfiIceConfig&) noexcept;
std::unique_ptr<NativeRtcError> ice_transport_set_config(const NativeIceTransport&, const FfiIceConfig&) noexcept;
bool ice_transport_set_role(const NativeIceTransport&, std::int32_t) noexcept;
bool ice_transport_set_parameters(const NativeIceTransport&, bool, rust::Slice<const std::uint8_t>, rust::Slice<const std::uint8_t>, bool) noexcept;
bool ice_transport_set_remote_mode(const NativeIceTransport&, std::uint8_t) noexcept;
bool ice_transport_maybe_start_gathering(const NativeIceTransport&) noexcept;

std::unique_ptr<NativeIceTransport> dtls_transport_ice(
    const NativeDtlsTransport& transport) noexcept;
bool ice_transport_has_internal(const NativeIceTransport& transport) noexcept;
FfiIceInternalValue ice_transport_internal_value(const NativeIceTransport& transport,
                                                std::uint8_t field) noexcept;
std::unique_ptr<NativeDtlsTransport> wrap_dtls_transport(
    webrtc::scoped_refptr<webrtc::DtlsTransportInterface> transport,
    webrtc::Thread* network,
    std::shared_ptr<ReadinessSignal> readiness = nullptr) noexcept;
std::unique_ptr<NativeDtlsTransport> peer_dtls_transport_by_mid(
    const NativePeerConnection& peer, rust::Slice<const std::uint8_t> mid) noexcept;
bool ice_transport_same_resource(const NativeIceTransport& first, const NativeIceTransport& second) noexcept;
std::unique_ptr<NativeSctpTransport> peer_sctp_transport(
    const NativePeerConnection& peer) noexcept;
std::uintptr_t dtls_transport_identity(const NativeDtlsTransport& transport) noexcept;
std::uintptr_t sctp_transport_identity(const NativeSctpTransport& transport) noexcept;
std::unique_ptr<NativeDtlsInformation> dtls_transport_information(
    const NativeDtlsTransport& transport) noexcept;
std::unique_ptr<NativeSctpInformation> sctp_transport_information(
    const NativeSctpTransport& transport) noexcept;
std::unique_ptr<NativeDtlsTransport> sctp_transport_dtls(
    const NativeSctpTransport& transport) noexcept;
std::unique_ptr<NativeDtlsTransport> sctp_information_dtls(
    const NativeSctpInformation& information, const NativePeerConnection& peer) noexcept;
FfiDtlsInformation dtls_information_fields(const NativeDtlsInformation& information) noexcept;
FfiSctpInformation sctp_information_fields(const NativeSctpInformation& information) noexcept;
rust::Vec<FfiCertificateData> dtls_information_remote_certificates(
    const NativeDtlsInformation& information) noexcept;
std::unique_ptr<NativeDtlsInformation> dtls_information_default() noexcept;
std::unique_ptr<NativeDtlsInformation> dtls_information_new(std::int32_t state) noexcept;
std::unique_ptr<NativeSctpInformation> sctp_information_default() noexcept;
std::unique_ptr<NativeSctpInformation> sctp_information_new(std::int32_t state) noexcept;
std::unique_ptr<NativeDtlsInformation> dtls_information_from_parts(const FfiDtlsInformation& fields, std::unique_ptr<NativeTlsCertificateChain> chain) noexcept;
std::unique_ptr<NativeDtlsInformation> dtls_information_from_legacy_parts(const FfiDtlsInformation& fields, std::unique_ptr<NativeTlsCertificateChain> chain) noexcept;
void dtls_information_assign(NativeDtlsInformation& target, const NativeDtlsInformation& source) noexcept;
void dtls_information_move_assign(NativeDtlsInformation& target, std::unique_ptr<NativeDtlsInformation> source) noexcept;
std::unique_ptr<NativeTlsCertificateChain> dtls_information_certificate_chain(const NativeDtlsInformation& information) noexcept;
std::unique_ptr<NativeSctpInformation> sctp_information_without_dtls(const FfiSctpInformation& fields) noexcept;
std::unique_ptr<NativeSctpInformation> sctp_information_with_dtls(const FfiSctpInformation& fields, const NativeDtlsTransport& transport) noexcept;
void sctp_information_assign(NativeSctpInformation& target, const NativeSctpInformation& source) noexcept;
rust::String sctp_information_string(const NativeSctpInformation& information) noexcept;
rust::String sctp_state_string(std::int32_t state) noexcept;
std::unique_ptr<NativeDtlsInformation> dtls_information_clone(
    const NativeDtlsInformation& information) noexcept;
std::unique_ptr<NativeSctpInformation> sctp_information_clone(
    const NativeSctpInformation& information) noexcept;
bool dtls_transport_register_observer(const NativeDtlsTransport& transport,
                                      std::size_t capacity) noexcept;
bool sctp_transport_register_observer(const NativeSctpTransport& transport,
                                      std::size_t capacity) noexcept;
void dtls_transport_unregister_observer(const NativeDtlsTransport& transport) noexcept;
void sctp_transport_unregister_observer(const NativeSctpTransport& transport) noexcept;
std::unique_ptr<NativeDtlsEvent> dtls_transport_take_event(
    const NativeDtlsTransport& transport) noexcept;
std::unique_ptr<NativeSctpInformation> sctp_transport_take_event(
    const NativeSctpTransport& transport) noexcept;
std::uint64_t dtls_transport_take_dropped(const NativeDtlsTransport& transport) noexcept;
std::uint64_t sctp_transport_take_dropped(const NativeSctpTransport& transport) noexcept;
std::unique_ptr<NativeDtlsInformation> dtls_event_information(NativeDtlsEvent& event) noexcept;
std::unique_ptr<NativeRtcError> dtls_event_error(const NativeDtlsEvent& event) noexcept;
}  // namespace pulsebeam::webrtc_sys
