#include "pulsebeam-webrtc-sys/native/transport.h"

#include <deque>
#include <limits>
#include <mutex>
#include <string>
#include <utility>

#include "pulsebeam-webrtc-sys/native/peer.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/tls.h"
#include "pulsebeam-webrtc-sys/native/socket_address.h"
#include "pulsebeam-webrtc-sys/native/rtc_error.h"
#include "absl/strings/str_format.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "api/peer_connection_interface.h"
#include "rtc_base/thread.h"
#include "rtc_base/buffer.h"
#include "p2p/base/ice_transport_internal.h"

namespace pulsebeam::webrtc_sys {
namespace {
template <class F> decltype(auto) OnNetwork(webrtc::Thread* thread, F&& call) {
  // SDK network roles can forbid even self-BlockingCall after setup.
  if (thread->IsCurrent()) return std::forward<F>(call)();
  return thread->BlockingCall(std::forward<F>(call));
}
template <class Event> struct EventQueue {
  std::shared_ptr<ReadinessSignal> readiness;
  std::mutex mutex;
  std::size_t capacity = 0;
  std::deque<Event> events;
  std::uint64_t dropped = 0;
  void Reset(std::size_t size) {
    std::lock_guard lock(mutex);
    capacity = size;
    events.clear();
    dropped = 0;
  }
  void Push(Event event) {
    struct Notify { std::shared_ptr<ReadinessSignal> signal;
      ~Notify() { SignalReadiness(signal); } } notify{readiness};
    std::lock_guard lock(mutex);
    if (!capacity) return;
    if (events.size() == capacity) {
      events.pop_front();
      if (dropped != std::numeric_limits<std::uint64_t>::max()) ++dropped;
    }
    events.push_back(std::move(event));
  }
  std::uint64_t TakeDropped() {
    std::lock_guard lock(mutex);
    return std::exchange(dropped, 0);
  }
};
}
struct NativeDtlsTransport::State final : webrtc::DtlsTransportObserverInterface {
  webrtc::scoped_refptr<webrtc::DtlsTransportInterface> transport;
  webrtc::Thread* network = nullptr;
  bool registered = false;
  EventQueue<std::unique_ptr<NativeDtlsEvent>> queue;
  ~State() override = default;
  void OnStateChange(webrtc::DtlsTransportInformation info) override {
    auto event = std::make_unique<NativeDtlsEvent>();
    event->information = std::make_unique<NativeDtlsInformation>(std::move(info));
    queue.Push(std::move(event));
  }
  void OnError(webrtc::RTCError error) override {
    auto event = std::make_unique<NativeDtlsEvent>();
    event->error = std::move(error);
    queue.Push(std::move(event));
  }
};
struct NativeSctpTransport::State final : webrtc::SctpTransportObserverInterface {
  webrtc::scoped_refptr<webrtc::SctpTransportInterface> transport;
  webrtc::Thread* network = nullptr;
  bool registered = false;
  EventQueue<std::unique_ptr<NativeSctpInformation>> queue;
  ~State() override = default;
  void OnStateChange(webrtc::SctpTransportInformation info) override {
    // Preserve the callback's actual DTLS ref, not a later transport getter.
    queue.Push(std::make_unique<NativeSctpInformation>(std::move(info)));
  }
};
struct IcePacketObserver;
struct NativeIceTransport::State {
  webrtc::scoped_refptr<webrtc::IceTransportInterface> transport;
  webrtc::Thread* network = nullptr;
  std::shared_ptr<ReadinessSignal> readiness;
  std::shared_ptr<IcePacketObserver> observer;
};
namespace {
webrtc::TimeDelta IceDelta(std::int64_t value) {
  if (value == std::numeric_limits<std::int64_t>::min()) return webrtc::TimeDelta::MinusInfinity();
  if (value == std::numeric_limits<std::int64_t>::max()) return webrtc::TimeDelta::PlusInfinity();
  return webrtc::TimeDelta::Micros(value);
}
std::int64_t IceDeltaRaw(webrtc::TimeDelta value) {
  return value.IsPlusInfinity() ? std::numeric_limits<std::int64_t>::max() :
      value.us_or(std::numeric_limits<std::int64_t>::min());
}
webrtc::IceConfig IceConfigFrom(const FfiIceConfig& value) {
  webrtc::IceConfig out;
  if (value.has_receiving_timeout) out.receiving_timeout = IceDelta(value.receiving_timeout);
  if (value.has_backup_connection_ping_interval) out.backup_connection_ping_interval = IceDelta(value.backup_connection_ping_interval);
  if (value.has_stable_writable_connection_ping_interval) out.stable_writable_connection_ping_interval = IceDelta(value.stable_writable_connection_ping_interval);
  if (value.has_regather_on_failed_networks_interval) out.regather_on_failed_networks_interval = IceDelta(value.regather_on_failed_networks_interval);
  if (value.has_receiving_switching_delay) out.receiving_switching_delay = IceDelta(value.receiving_switching_delay);
  if (value.has_ice_check_interval_strong_connectivity) out.ice_check_interval_strong_connectivity = IceDelta(value.ice_check_interval_strong_connectivity);
  if (value.has_ice_check_interval_weak_connectivity) out.ice_check_interval_weak_connectivity = IceDelta(value.ice_check_interval_weak_connectivity);
  if (value.has_ice_check_min_interval) out.ice_check_min_interval = IceDelta(value.ice_check_min_interval);
  if (value.has_ice_unwritable_timeout) out.ice_unwritable_timeout = IceDelta(value.ice_unwritable_timeout);
  if (value.has_ice_inactive_timeout) out.ice_inactive_timeout = IceDelta(value.ice_inactive_timeout);
  if (value.has_stun_keepalive_interval) out.stun_keepalive_interval = IceDelta(value.stun_keepalive_interval);
  out.prioritize_most_likely_candidate_pairs = value.prioritize_most_likely_candidate_pairs;
  out.presume_writable_when_fully_relayed = value.presume_writable_when_fully_relayed;
  out.surface_ice_candidates_on_ice_transport_type_changed = value.surface_ice_candidates_on_ice_transport_type_changed;
  out.dtls_handshake_in_stun = value.dtls_handshake_in_stun;
  out.continual_gathering_policy = static_cast<webrtc::ContinualGatheringPolicy>(value.continual_gathering_policy);
  out.default_nomination_mode = static_cast<webrtc::NominationMode>(value.default_nomination_mode);
  out.vpn_preference = static_cast<webrtc::VpnPreference>(value.vpn_preference);
  if (value.has_network_preference) out.network_preference = static_cast<webrtc::AdapterType>(value.network_preference);
  if (value.has_ice_unwritable_min_checks) out.ice_unwritable_min_checks = value.ice_unwritable_min_checks;
  return out;
}
FfiIceConfig IceConfigTo(const webrtc::IceConfig& value) {
  FfiIceConfig out{};
  out.has_receiving_timeout = value.receiving_timeout.has_value();
  if (value.receiving_timeout) out.receiving_timeout = IceDeltaRaw(*value.receiving_timeout);
  out.has_backup_connection_ping_interval = value.backup_connection_ping_interval.has_value();
  if (value.backup_connection_ping_interval) out.backup_connection_ping_interval = IceDeltaRaw(*value.backup_connection_ping_interval);
  out.has_stable_writable_connection_ping_interval = value.stable_writable_connection_ping_interval.has_value();
  if (value.stable_writable_connection_ping_interval) out.stable_writable_connection_ping_interval = IceDeltaRaw(*value.stable_writable_connection_ping_interval);
  out.has_regather_on_failed_networks_interval = value.regather_on_failed_networks_interval.has_value();
  if (value.regather_on_failed_networks_interval) out.regather_on_failed_networks_interval = IceDeltaRaw(*value.regather_on_failed_networks_interval);
  out.has_receiving_switching_delay = value.receiving_switching_delay.has_value();
  if (value.receiving_switching_delay) out.receiving_switching_delay = IceDeltaRaw(*value.receiving_switching_delay);
  out.has_ice_check_interval_strong_connectivity = value.ice_check_interval_strong_connectivity.has_value();
  if (value.ice_check_interval_strong_connectivity) out.ice_check_interval_strong_connectivity = IceDeltaRaw(*value.ice_check_interval_strong_connectivity);
  out.has_ice_check_interval_weak_connectivity = value.ice_check_interval_weak_connectivity.has_value();
  if (value.ice_check_interval_weak_connectivity) out.ice_check_interval_weak_connectivity = IceDeltaRaw(*value.ice_check_interval_weak_connectivity);
  out.has_ice_check_min_interval = value.ice_check_min_interval.has_value();
  if (value.ice_check_min_interval) out.ice_check_min_interval = IceDeltaRaw(*value.ice_check_min_interval);
  out.has_ice_unwritable_timeout = value.ice_unwritable_timeout.has_value();
  if (value.ice_unwritable_timeout) out.ice_unwritable_timeout = IceDeltaRaw(*value.ice_unwritable_timeout);
  out.has_ice_inactive_timeout = value.ice_inactive_timeout.has_value();
  if (value.ice_inactive_timeout) out.ice_inactive_timeout = IceDeltaRaw(*value.ice_inactive_timeout);
  out.has_stun_keepalive_interval = value.stun_keepalive_interval.has_value();
  if (value.stun_keepalive_interval) out.stun_keepalive_interval = IceDeltaRaw(*value.stun_keepalive_interval);
  out.prioritize_most_likely_candidate_pairs = value.prioritize_most_likely_candidate_pairs;
  out.presume_writable_when_fully_relayed = value.presume_writable_when_fully_relayed;
  out.surface_ice_candidates_on_ice_transport_type_changed = value.surface_ice_candidates_on_ice_transport_type_changed;
  out.dtls_handshake_in_stun = value.dtls_handshake_in_stun;
  out.continual_gathering_policy = static_cast<std::uint8_t>(value.continual_gathering_policy);
  out.default_nomination_mode = static_cast<std::uint8_t>(value.default_nomination_mode);
  out.vpn_preference = static_cast<std::uint8_t>(value.vpn_preference);
  out.has_network_preference = value.network_preference.has_value();
  out.network_preference = static_cast<std::int32_t>(value.network_preference.value_or(webrtc::ADAPTER_TYPE_UNKNOWN));
  out.has_ice_unwritable_min_checks = value.ice_unwritable_min_checks.has_value();
  out.ice_unwritable_min_checks = value.ice_unwritable_min_checks.value_or(0);
  return out;
}
template<class F> bool WithIce(const NativeIceTransport& transport, F&& operation) {
  auto& state = *transport.state();
  return state.network->BlockingCall([&] {
    auto* internal = state.transport->internal();
    if (!internal) return false;
    operation(*internal);
    return true;
  });
}
}
FfiIceConfig ice_config_default() noexcept { return IceConfigTo(webrtc::IceConfig()); }
FfiIceConfig ice_config_resolve(const FfiIceConfig& value) noexcept {
  auto config = IceConfigFrom(value);
  config.receiving_timeout = config.receiving_timeout_or_default();
  config.backup_connection_ping_interval = config.backup_connection_ping_interval_or_default();
  config.stable_writable_connection_ping_interval = config.stable_writable_connection_ping_interval_or_default();
  config.regather_on_failed_networks_interval = config.regather_on_failed_networks_interval_or_default();
  config.receiving_switching_delay = config.receiving_switching_delay_or_default();
  config.ice_check_interval_strong_connectivity = config.ice_check_interval_strong_connectivity_or_default();
  config.ice_check_interval_weak_connectivity = config.ice_check_interval_weak_connectivity_or_default();
  config.ice_check_min_interval = config.ice_check_min_interval_or_default();
  config.ice_unwritable_timeout = config.ice_unwritable_timeout_or_default();
  config.ice_inactive_timeout = config.ice_inactive_timeout_or_default();
  config.stun_keepalive_interval = config.stun_keepalive_interval_or_default();
  config.ice_unwritable_min_checks = config.ice_unwritable_min_checks_or_default();
  return IceConfigTo(config);
}
std::unique_ptr<NativeRtcError> ice_config_validate(const FfiIceConfig& value) noexcept {
  return std::make_unique<NativeRtcError>(IceConfigFrom(value).IsValid());
}
bool ice_transport_config(const NativeIceTransport& transport, FfiIceConfig& out) noexcept {
  return WithIce(transport,[&](auto& ice) { out = IceConfigTo(ice.config()); });
}
std::unique_ptr<NativeRtcError> ice_transport_set_config(const NativeIceTransport& transport,
                                                       const FfiIceConfig& value) noexcept {
  std::unique_ptr<NativeRtcError> result;
  WithIce(transport,[&](auto& ice) {
    auto config = IceConfigFrom(value);
    auto error = config.IsValid();
    if (error.ok()) ice.SetIceConfig(config);
    result = std::make_unique<NativeRtcError>(std::move(error));
  });
  return result;
}
bool ice_transport_set_role(const NativeIceTransport& transport, std::int32_t role) noexcept {
  return WithIce(transport,[&](auto& ice) { ice.SetIceRole(static_cast<webrtc::IceRole>(role)); });
}
bool ice_transport_set_parameters(const NativeIceTransport& transport, bool remote,
    rust::Slice<const std::uint8_t> ufrag, rust::Slice<const std::uint8_t> password,
    bool renomination) noexcept {
  return WithIce(transport,[&](auto& ice) {
    webrtc::IceParameters parameters(
      absl::string_view(reinterpret_cast<const char*>(ufrag.data()),ufrag.size()),
      absl::string_view(reinterpret_cast<const char*>(password.data()),password.size()),renomination);
    if (remote) ice.SetRemoteIceParameters(parameters); else ice.SetIceParameters(parameters);
  });
}
bool ice_transport_set_remote_mode(const NativeIceTransport& transport, std::uint8_t mode) noexcept {
  return WithIce(transport,[&](auto& ice) { ice.SetRemoteIceMode(static_cast<webrtc::IceMode>(mode)); });
}
bool ice_transport_maybe_start_gathering(const NativeIceTransport& transport) noexcept {
  return WithIce(transport,[](auto& ice) { ice.MaybeStartGathering(); });
}

NativeIceTransport::NativeIceTransport(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeIceTransport::~NativeIceTransport() {
  ice_transport_unregister_packet_observer(*this);
  auto* network = state_->network;
  network->BlockingCall([this] { state_.reset(); });
}
const std::unique_ptr<NativeIceTransport::State>& NativeIceTransport::state() const noexcept {
  return state_;
}
std::unique_ptr<NativeIceTransport> dtls_transport_ice(const NativeDtlsTransport& transport) noexcept {
  auto& owner = *transport.state();
  auto ice = OnNetwork(owner.network, [&] { return owner.transport->ice_transport(); });
  if (!ice) return nullptr;
  auto state = std::make_unique<NativeIceTransport::State>();
  state->transport = std::move(ice);
  state->network = owner.network;
  state->readiness = owner.queue.readiness;
  return std::make_unique<NativeIceTransport>(std::move(state));
}
bool ice_transport_has_internal(const NativeIceTransport& transport) noexcept {
  auto& state = *transport.state();
  return OnNetwork(state.network, [&] { return state.transport->internal() != nullptr; });
}
FfiIceInternalValue ice_transport_internal_value(const NativeIceTransport& transport,
                                                std::uint8_t field) noexcept {
  auto& state = *transport.state();
  return OnNetwork(state.network, [&] {
    FfiIceInternalValue out{};
    auto* internal = state.transport->internal();
    if (!internal) return out;
    out.available = true;
    switch (field) {
      case 0: out.value = static_cast<std::int32_t>(internal->GetState()); break;
      case 1: out.value = static_cast<std::int32_t>(internal->GetIceTransportState()); break;
      case 2: out.value = internal->component(); break;
      case 3: out.value = static_cast<std::int32_t>(internal->GetIceRole()); break;
      case 4: out.value = static_cast<std::int32_t>(internal->gathering_state()); break;
      case 5: {
        auto rtt = internal->GetRttEstimate();
        out.present = rtt.has_value();
        out.value = rtt.value_or(0);
        break;
      }
      default: out.available = false; break;
    }
    return out;
  });
}
NativeDtlsTransport::NativeDtlsTransport(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeDtlsTransport::~NativeDtlsTransport() {
  // Unregister and serialize against callbacks before beginning destruction of
  // the observer object (including its vtable), not inside State::~State.
  auto* network = state_->network;
  network->BlockingCall([this] {
    if (state_->registered) state_->transport->UnregisterObserver();
    // Retire queued snapshots and the final native reference on their role.
    // The Rust owner keeps the peer and its role threads alive through here.
    state_.reset();
  });
}
const std::unique_ptr<NativeDtlsTransport::State>& NativeDtlsTransport::state() const noexcept {
  return state_;
}
NativeSctpTransport::NativeSctpTransport(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeSctpTransport::~NativeSctpTransport() {
  auto* network = state_->network;
  network->BlockingCall([this] {
    if (state_->registered) state_->transport->UnregisterObserver();
    state_.reset();
  });
}
const std::unique_ptr<NativeSctpTransport::State>& NativeSctpTransport::state() const noexcept {
  return state_;
}
std::unique_ptr<NativeDtlsTransport> wrap_dtls_transport(
    webrtc::scoped_refptr<webrtc::DtlsTransportInterface> transport,
    webrtc::Thread* network,
    std::shared_ptr<ReadinessSignal> readiness) noexcept {
  if (!transport) return nullptr;
  auto state = std::make_unique<NativeDtlsTransport::State>();
  state->queue.readiness = std::move(readiness);
  state->transport = std::move(transport);
  state->network = network;
  return std::make_unique<NativeDtlsTransport>(std::move(state));
}
std::unique_ptr<NativeDtlsTransport> peer_dtls_transport_by_mid(
    const NativePeerConnection& peer, rust::Slice<const std::uint8_t> mid) noexcept {
  auto native = peer.network_thread()->BlockingCall([&] {
    return peer.peer()->LookupDtlsTransportByMid(
        std::string(reinterpret_cast<const char*>(mid.data()), mid.size()));
  });
  return wrap_dtls_transport(std::move(native), peer.network_thread(), peer.readiness());
}
bool ice_transport_same_resource(const NativeIceTransport& first, const NativeIceTransport& second) noexcept {
  return first.state()->transport.get() == second.state()->transport.get();
}
std::unique_ptr<NativeSctpTransport> peer_sctp_transport(const NativePeerConnection& peer) noexcept {
  auto transport = peer.peer()->GetSctpTransport();
  if (!transport) return nullptr;
  auto state = std::make_unique<NativeSctpTransport::State>();
  state->queue.readiness = peer.readiness();
  state->transport = std::move(transport);
  state->network = peer.network_thread();
  return std::make_unique<NativeSctpTransport>(std::move(state));
}
std::uintptr_t dtls_transport_identity(const NativeDtlsTransport& transport) noexcept {
  return reinterpret_cast<std::uintptr_t>(transport.state()->transport.get());
}
std::uintptr_t sctp_transport_identity(const NativeSctpTransport& transport) noexcept {
  return reinterpret_cast<std::uintptr_t>(transport.state()->transport.get());
}
std::unique_ptr<NativeDtlsInformation> dtls_transport_information(
    const NativeDtlsTransport& transport) noexcept {
  return std::make_unique<NativeDtlsInformation>(transport.state()->transport->Information());
}
std::unique_ptr<NativeSctpInformation> sctp_transport_information(
    const NativeSctpTransport& transport) noexcept {
  return std::make_unique<NativeSctpInformation>(transport.state()->transport->Information());
}
std::unique_ptr<NativeDtlsTransport> sctp_transport_dtls(
    const NativeSctpTransport& transport) noexcept {
  auto& state = *transport.state();
  // Unlike Information(), this pinned SCTP accessor does not proxy itself.
  auto dtls = state.network->BlockingCall([&] {
    return state.transport->dtls_transport();
  });
  return wrap_dtls_transport(std::move(dtls), state.network, state.queue.readiness);
}
std::unique_ptr<NativeDtlsTransport> sctp_information_dtls(
    const NativeSctpInformation& information, const NativePeerConnection& peer) noexcept {
  return wrap_dtls_transport(information.value().dtls_transport(), peer.network_thread(), peer.readiness());
}
FfiDtlsInformation dtls_information_fields(const NativeDtlsInformation& information) noexcept {
  const auto& value = information.value();
  FfiDtlsInformation out{};
  out.state = static_cast<std::int32_t>(value.state());
  out.has_role = value.role().has_value();
  if (value.role()) out.role = static_cast<std::int32_t>(*value.role());
  out.has_tls_version = value.tls_version().has_value();
  out.tls_version = value.tls_version().value_or(0);
  out.has_ssl_cipher_suite = value.ssl_cipher_suite().has_value();
  out.ssl_cipher_suite = value.ssl_cipher_suite().value_or(0);
  out.has_srtp_cipher_suite = value.srtp_cipher_suite().has_value();
  out.srtp_cipher_suite = value.srtp_cipher_suite().value_or(0);
  out.has_ssl_group_id = value.ssl_group_id().has_value();
  out.ssl_group_id = value.ssl_group_id().value_or(0);
  out.has_remote_certificates = value.remote_ssl_certificates() != nullptr;
  return out;
}
FfiSctpInformation sctp_information_fields(const NativeSctpInformation& information) noexcept {
  const auto& value = information.value();
  FfiSctpInformation out{};
  out.state = static_cast<std::int32_t>(value.state());
  out.has_dtls_transport = value.dtls_transport() != nullptr;
  out.has_max_message_size = value.MaxMessageSize().has_value();
  out.max_message_size = value.MaxMessageSize().value_or(0);
  out.has_max_channels = value.MaxChannels().has_value();
  out.max_channels = value.MaxChannels().value_or(0);
  return out;
}
rust::Vec<FfiCertificateData> dtls_information_remote_certificates(
    const NativeDtlsInformation& information) noexcept {
  rust::Vec<FfiCertificateData> out;
  const auto* chain = information.value().remote_ssl_certificates();
  if (!chain) return out;
  for (std::size_t index = 0; index < chain->GetSize(); ++index) {
    const auto& certificate = chain->Get(index);
    FfiCertificateData data{};
    data.pem = certificate.ToPEMString();
    webrtc::Buffer der;
    certificate.ToDER(&der);
    for (auto byte : der) data.der.push_back(byte);
    data.expiration_unix_seconds = certificate.CertificateExpirationTime();
    std::string algorithm;
    data.has_signature_digest_algorithm = certificate.GetSignatureDigestAlgorithm(&algorithm);
    data.signature_digest_algorithm = algorithm;
    out.push_back(std::move(data));
  }
  return out;
}
std::unique_ptr<NativeDtlsInformation> dtls_information_default() noexcept {
  return std::make_unique<NativeDtlsInformation>(webrtc::DtlsTransportInformation());
}
std::unique_ptr<NativeDtlsInformation> dtls_information_new(std::int32_t state) noexcept {
  return std::make_unique<NativeDtlsInformation>(
      webrtc::DtlsTransportInformation(static_cast<webrtc::DtlsTransportState>(state)));
}
std::unique_ptr<NativeSctpInformation> sctp_information_default() noexcept {
  return std::make_unique<NativeSctpInformation>(webrtc::SctpTransportInformation());
}
std::unique_ptr<NativeSctpInformation> sctp_information_new(std::int32_t state) noexcept {
  return std::make_unique<NativeSctpInformation>(
      webrtc::SctpTransportInformation(static_cast<webrtc::SctpTransportState>(state)));
}
std::unique_ptr<NativeDtlsInformation> dtls_information_from_parts(
    const FfiDtlsInformation& fields, std::unique_ptr<NativeTlsCertificateChain> chain) noexcept {
  return std::make_unique<NativeDtlsInformation>(webrtc::DtlsTransportInformation(
      static_cast<webrtc::DtlsTransportState>(fields.state),
      fields.has_role ? std::optional<webrtc::DtlsTransportTlsRole>(static_cast<webrtc::DtlsTransportTlsRole>(fields.role)) : std::nullopt,
      fields.has_tls_version ? std::optional<int>(fields.tls_version) : std::nullopt,
      fields.has_ssl_cipher_suite ? std::optional<int>(fields.ssl_cipher_suite) : std::nullopt,
      fields.has_srtp_cipher_suite ? std::optional<int>(fields.srtp_cipher_suite) : std::nullopt,
      chain ? chain->Take() : nullptr,
      fields.has_ssl_group_id ? std::optional<int>(fields.ssl_group_id) : std::nullopt));
}
std::unique_ptr<NativeDtlsInformation> dtls_information_from_legacy_parts(
    const FfiDtlsInformation& fields, std::unique_ptr<NativeTlsCertificateChain> chain) noexcept {
  return std::make_unique<NativeDtlsInformation>(webrtc::DtlsTransportInformation(
      static_cast<webrtc::DtlsTransportState>(fields.state),
      fields.has_tls_version ? std::optional<int>(fields.tls_version) : std::nullopt,
      fields.has_ssl_cipher_suite ? std::optional<int>(fields.ssl_cipher_suite) : std::nullopt,
      fields.has_srtp_cipher_suite ? std::optional<int>(fields.srtp_cipher_suite) : std::nullopt,
      chain ? chain->Take() : nullptr));
}
void dtls_information_assign(NativeDtlsInformation& target, const NativeDtlsInformation& source) noexcept { target.Assign(source); }
void dtls_information_move_assign(NativeDtlsInformation& target, std::unique_ptr<NativeDtlsInformation> source) noexcept { target.MoveAssign(*source); }
std::unique_ptr<NativeTlsCertificateChain> dtls_information_certificate_chain(const NativeDtlsInformation& information) noexcept {
  const auto* chain = information.value().remote_ssl_certificates();
  return chain ? std::make_unique<NativeTlsCertificateChain>(chain->Clone()) : nullptr;
}
std::unique_ptr<NativeSctpInformation> sctp_information_without_dtls(const FfiSctpInformation& fields) noexcept {
  return std::make_unique<NativeSctpInformation>(webrtc::SctpTransportInformation(
      static_cast<webrtc::SctpTransportState>(fields.state), nullptr,
      fields.has_max_message_size ? std::optional<double>(fields.max_message_size) : std::nullopt,
      fields.has_max_channels ? std::optional<int>(fields.max_channels) : std::nullopt));
}
std::unique_ptr<NativeSctpInformation> sctp_information_with_dtls(const FfiSctpInformation& fields, const NativeDtlsTransport& transport) noexcept {
  return std::make_unique<NativeSctpInformation>(webrtc::SctpTransportInformation(
      static_cast<webrtc::SctpTransportState>(fields.state), transport.state()->transport,
      fields.has_max_message_size ? std::optional<double>(fields.max_message_size) : std::nullopt,
      fields.has_max_channels ? std::optional<int>(fields.max_channels) : std::nullopt));
}
void sctp_information_assign(NativeSctpInformation& target, const NativeSctpInformation& source) noexcept { target.Assign(source); }
rust::String sctp_information_string(const NativeSctpInformation& information) noexcept { return absl::StrFormat("%v", information.value()); }
rust::String sctp_state_string(std::int32_t state) noexcept { return absl::StrFormat("%v", static_cast<webrtc::SctpTransportState>(state)); }
std::unique_ptr<NativeDtlsInformation> dtls_information_clone(
    const NativeDtlsInformation& information) noexcept {
  return std::make_unique<NativeDtlsInformation>(information.value());
}
std::unique_ptr<NativeSctpInformation> sctp_information_clone(
    const NativeSctpInformation& information) noexcept {
  return std::make_unique<NativeSctpInformation>(information.value());
}
bool dtls_transport_register_observer(const NativeDtlsTransport& transport,
                                      std::size_t capacity) noexcept {
  auto& state = *transport.state();
  return OnNetwork(state.network, [&] {
    if (!capacity || state.registered) return false;
    state.queue.Reset(capacity);
    state.transport->RegisterObserver(&state);
    state.registered = true;
    return true;
  });
}
bool sctp_transport_register_observer(const NativeSctpTransport& transport,
                                      std::size_t capacity) noexcept {
  auto& state = *transport.state();
  return OnNetwork(state.network, [&] {
    if (!capacity || state.registered) return false;
    state.queue.Reset(capacity);
    state.transport->RegisterObserver(&state);
    state.registered = true;
    return true;
  });
}
void dtls_transport_unregister_observer(const NativeDtlsTransport& transport) noexcept {
  auto& state = *transport.state();
  OnNetwork(state.network, [&] {
    if (!state.registered) return;
    state.transport->UnregisterObserver();
    state.registered = false;
  });
}
void sctp_transport_unregister_observer(const NativeSctpTransport& transport) noexcept {
  auto& state = *transport.state();
  OnNetwork(state.network, [&] {
    if (!state.registered) return;
    state.transport->UnregisterObserver();
    state.registered = false;
  });
}
std::unique_ptr<NativeDtlsEvent> dtls_transport_take_event(const NativeDtlsTransport& transport) noexcept {
  auto& queue = transport.state()->queue;
  std::lock_guard lock(queue.mutex);
  if (queue.events.empty()) return nullptr;
  auto event = std::move(queue.events.front());
  queue.events.pop_front();
  return event;
}
std::unique_ptr<NativeSctpInformation> sctp_transport_take_event(const NativeSctpTransport& transport) noexcept {
  auto& queue = transport.state()->queue;
  std::lock_guard lock(queue.mutex);
  if (queue.events.empty()) return nullptr;
  auto event = std::move(queue.events.front());
  queue.events.pop_front();
  return event;
}
std::uint64_t dtls_transport_take_dropped(const NativeDtlsTransport& transport) noexcept {
  return transport.state()->queue.TakeDropped();
}
std::uint64_t sctp_transport_take_dropped(const NativeSctpTransport& transport) noexcept {
  return transport.state()->queue.TakeDropped();
}
std::unique_ptr<NativeDtlsInformation> dtls_event_information(NativeDtlsEvent& event) noexcept {
  return std::move(event.information);
}
std::unique_ptr<NativeRtcError> dtls_event_error(const NativeDtlsEvent& event) noexcept {
  return std::make_unique<NativeRtcError>(event.error);
}
namespace {
rust::Vec<std::uint8_t> IceBytes(absl::string_view value) {
  rust::Vec<std::uint8_t> out; out.reserve(value.size());
  for (unsigned char byte:value) out.push_back(byte);
  return out;
}
std::unique_ptr<NativeTransportCandidate> IceCandidateCopy(const webrtc::Candidate& value) {
  return std::make_unique<NativeTransportCandidate>(value);
}
}
std::unique_ptr<NativeTransportCandidate> transport_candidate_default() noexcept { return IceCandidateCopy(webrtc::Candidate()); }
std::unique_ptr<NativeTransportCandidate> transport_candidate_clone(const NativeTransportCandidate& candidate) noexcept { return IceCandidateCopy(candidate.value); }
FfiTransportCandidate transport_candidate_fields(const NativeTransportCandidate& candidate) noexcept {
  const auto& value=candidate.value;
  FfiTransportCandidate out{};
  out.id=IceBytes(value.id());
  out.protocol=IceBytes(value.protocol());
  out.relay_protocol=IceBytes(value.relay_protocol());
  out.username=IceBytes(value.username());
  out.password=IceBytes(value.password());
  out.network_name=IceBytes(value.network_name());
  out.foundation=IceBytes(value.foundation());
  out.tcptype=IceBytes(value.tcptype());
  out.url=IceBytes(value.url());
  out.component=static_cast<std::int32_t>(value.component());
  out.priority=static_cast<std::uint32_t>(value.priority());
  out.candidate_type=static_cast<std::int32_t>(value.type());
  out.network_type=static_cast<std::int32_t>(value.network_type());
  out.underlying_type_for_vpn=static_cast<std::int32_t>(value.underlying_type_for_vpn());
  out.generation=static_cast<std::uint32_t>(value.generation());
  out.network_id=static_cast<std::uint16_t>(value.network_id());
  out.network_cost=static_cast<std::uint16_t>(value.network_cost());
  out.network_slice=static_cast<std::int32_t>(value.network_slice());
  out.address=std::make_unique<NativeSocketAddress>(value.address());
  out.related_address=std::make_unique<NativeSocketAddress>(value.related_address());
  return out;
}
FfiTransportCandidateParse transport_candidate_parse(rust::Slice<const std::uint8_t> bytes) noexcept {
  FfiTransportCandidateParse out{};
  auto result=webrtc::Candidate::ParseCandidateString(absl::string_view(reinterpret_cast<const char*>(bytes.data()),bytes.size()));
  if (result.ok()) out.candidate=IceCandidateCopy(result.value());
  else out.error=std::make_unique<NativeRtcError>(result.MoveError());
  return out;
}
std::unique_ptr<NativeRtcError> transport_candidate_verify(const NativeTransportCandidate& candidate) noexcept { return std::make_unique<NativeRtcError>(webrtc::VerifyCandidate(candidate.value)); }
void transport_candidate_set_bytes(NativeTransportCandidate& candidate,std::uint8_t field,rust::Slice<const std::uint8_t> bytes) noexcept {
  absl::string_view value(reinterpret_cast<const char*>(bytes.data()),bytes.size());
  switch(field) {
    case 0: candidate.value.set_protocol(value); break;
    case 1: candidate.value.set_relay_protocol(value); break;
    case 2: candidate.value.set_username(value); break;
    case 3: candidate.value.set_password(value); break;
    case 4: candidate.value.set_network_name(value); break;
    case 5: candidate.value.set_foundation(value); break;
    case 6: candidate.value.set_tcptype(value); break;
    case 7: candidate.value.set_url(value); break;
  }
}
void transport_candidate_set_integer(NativeTransportCandidate& candidate,std::uint8_t field,std::int64_t value) noexcept {
  switch(field) {
    case 0: candidate.value.set_component(static_cast<std::int32_t>(value)); break;
    case 1: candidate.value.set_priority(static_cast<std::uint32_t>(value)); break;
    case 2: candidate.value.set_type(static_cast<webrtc::IceCandidateType>(value)); break;
    case 3: candidate.value.set_network_type(static_cast<webrtc::AdapterType>(value)); break;
    case 4: candidate.value.set_underlying_type_for_vpn(static_cast<webrtc::AdapterType>(value)); break;
    case 5: candidate.value.set_generation(static_cast<std::uint32_t>(value)); break;
    case 6: candidate.value.set_network_id(static_cast<std::uint16_t>(value)); break;
    case 7: candidate.value.set_network_cost(static_cast<std::uint16_t>(value)); break;
    case 8: candidate.value.set_network_slice(static_cast<webrtc::NetworkSlice>(value)); break;
  }
}
void transport_candidate_set_address(NativeTransportCandidate& candidate,bool related,const NativeSocketAddress& value) noexcept {
  if (related) candidate.value.set_related_address(value.value); else candidate.value.set_address(value.value);
}
rust::Vec<std::uint8_t> transport_candidate_serialize(const NativeTransportCandidate& candidate,std::uint8_t kind,bool include_ufrag) noexcept {
  if (kind==0) return IceBytes(candidate.value.ToString());
  if (kind==1) return IceBytes(candidate.value.ToSensitiveString());
  return IceBytes(candidate.value.ToCandidateAttribute(include_ufrag));
}
bool transport_candidate_equivalent(const NativeTransportCandidate& first,const NativeTransportCandidate& second,bool removal) noexcept {
  return removal ? first.value.MatchesForRemoval(second.value) : first.value.IsEquivalent(second.value);
}
bool ice_transport_candidate(const NativeIceTransport& transport,const NativeTransportCandidate& candidate,bool remove) noexcept {
  return WithIce(transport,[&](auto& ice) { if (remove) ice.RemoveRemoteCandidate(candidate.value); else ice.AddRemoteCandidate(candidate.value); });
}
bool ice_transport_remove_all_candidates(const NativeIceTransport& transport) noexcept { return WithIce(transport,[](auto& ice) { ice.RemoveAllRemoteCandidates(); }); }
FfiIcePair ice_transport_selected_pair(const NativeIceTransport& transport) noexcept {
  FfiIcePair out{};
  out.available=WithIce(transport,[&](auto& ice) {
    auto pair=ice.GetSelectedCandidatePair();
    if (pair) { out.local=IceCandidateCopy(pair->local_candidate()); out.remote=IceCandidateCopy(pair->remote_candidate()); }
  });
  return out;
}
FfiIceStats ice_transport_stats(const NativeIceTransport& transport) noexcept {
  FfiIceStats out{};
  out.available=WithIce(transport,[&](auto& ice) {
    webrtc::IceTransportStats stats;
    out.success=ice.GetStats(&stats);
    if (!out.success) return;
    out.selected_candidate_pair_changes=stats.selected_candidate_pair_changes;
    out.bytes_sent=stats.bytes_sent; out.bytes_received=stats.bytes_received;
    out.packets_sent=stats.packets_sent; out.packets_received=stats.packets_received;
    out.ice_role=static_cast<std::int32_t>(stats.ice_role);
    out.ice_local_username_fragment=IceBytes(stats.ice_local_username_fragment);
    out.ice_state=static_cast<std::int32_t>(stats.ice_state);
    for (const auto& value:stats.candidate_stats_list) {
      FfiIceCandidateStats candidate{};
      candidate.candidate=IceCandidateCopy(value.candidate());
      candidate.has_stun_stats=value.stun_stats().has_value();
      if (value.stun_stats()) {
        candidate.stun_binding_requests_sent=value.stun_stats()->stun_binding_requests_sent;
        candidate.stun_binding_responses_received=value.stun_stats()->stun_binding_responses_received;
        candidate.stun_binding_rtt_ms_total=value.stun_stats()->stun_binding_rtt_ms_total;
        candidate.stun_binding_rtt_ms_squared_total=value.stun_stats()->stun_binding_rtt_ms_squared_total;
      }
      out.candidate_stats.push_back(std::move(candidate));
    }
    for (const auto& value:stats.connection_infos) {
      FfiIceConnectionInfo connection{};
      connection.best_connection=value.best_connection;
      connection.writable=value.writable;
      connection.receiving=value.receiving;
      connection.timeout=value.timeout;
      connection.rtt=value.rtt;
      connection.sent_discarded_bytes=value.sent_discarded_bytes;
      connection.sent_total_bytes=value.sent_total_bytes;
      connection.sent_bytes_second=value.sent_bytes_second;
      connection.sent_discarded_packets=value.sent_discarded_packets;
      connection.sent_total_packets=value.sent_total_packets;
      connection.sent_ping_requests_total=value.sent_ping_requests_total;
      connection.sent_ping_requests_before_first_response=value.sent_ping_requests_before_first_response;
      connection.sent_ping_responses=value.sent_ping_responses;
      connection.recv_total_bytes=value.recv_total_bytes;
      connection.recv_bytes_second=value.recv_bytes_second;
      connection.packets_received=value.packets_received;
      connection.recv_ping_requests=value.recv_ping_requests;
      connection.recv_ping_responses=value.recv_ping_responses;
      connection.priority=value.priority;
      connection.nominated=value.nominated;
      connection.total_round_trip_time_ms=value.total_round_trip_time_ms;
      connection.local_candidate=IceCandidateCopy(value.local_candidate);
      connection.remote_candidate=IceCandidateCopy(value.remote_candidate);
      connection.state=static_cast<std::int32_t>(value.state);
      connection.has_current_round_trip_time_ms=value.current_round_trip_time_ms.has_value();
      connection.current_round_trip_time_ms=value.current_round_trip_time_ms.value_or(0);
      connection.has_last_data_received=value.last_data_received.has_value();
      if (value.last_data_received) connection.last_data_received=value.last_data_received->IsPlusInfinity() ? std::numeric_limits<std::int64_t>::max():value.last_data_received->us_or(std::numeric_limits<std::int64_t>::min());
      connection.has_last_data_sent=value.last_data_sent.has_value();
      if (value.last_data_sent) connection.last_data_sent=value.last_data_sent->IsPlusInfinity() ? std::numeric_limits<std::int64_t>::max():value.last_data_sent->us_or(std::numeric_limits<std::int64_t>::min());
      out.connection_infos.push_back(std::move(connection));
    }
  });
  return out;
}

namespace {
FfiIcePacketInfo IcePacketInfo(const webrtc::PacketInfo& value) {
  FfiIcePacketInfo out{};
  out.included_in_feedback=value.included_in_feedback; out.included_in_allocation=value.included_in_allocation;
  out.is_media=value.is_media; out.packet_type=static_cast<std::int32_t>(value.packet_type);
  out.protocol=static_cast<std::int32_t>(value.protocol);
  out.has_network_id=value.network_id.has_value(); out.network_id=value.network_id.value_or(0);
  out.packet_size_bytes=value.packet_size_bytes; out.turn_overhead_bytes=value.turn_overhead_bytes; out.ip_overhead_bytes=value.ip_overhead_bytes;
  return out;
}
webrtc::AsyncSocketPacketOptions IcePacketOptions(const FfiIcePacketOptions& value) {
  webrtc::AsyncSocketPacketOptions out;
  out.dscp=static_cast<webrtc::DiffServCodePoint>(value.dscp);
  out.ect_1=value.ect_1; out.packet_id=value.packet_id;
  out.packet_time_params.rtp_sendtime_extension_id=value.rtp_sendtime_extension_id;
  out.packet_time_params.srtp_auth_key.assign(value.srtp_auth_key.begin(),value.srtp_auth_key.end());
  out.packet_time_params.srtp_auth_tag_len=value.srtp_auth_tag_len;
  out.packet_time_params.srtp_packet_index=value.srtp_packet_index;
  out.info_signaled_after_sent.included_in_feedback=value.info.included_in_feedback;
  out.info_signaled_after_sent.included_in_allocation=value.info.included_in_allocation;
  out.info_signaled_after_sent.is_media=value.info.is_media;
  out.info_signaled_after_sent.packet_type=static_cast<webrtc::PacketType>(value.info.packet_type);
  out.info_signaled_after_sent.protocol=static_cast<webrtc::PacketInfoProtocolType>(value.info.protocol);
  if (value.info.has_network_id) out.info_signaled_after_sent.network_id=value.info.network_id;
  out.info_signaled_after_sent.packet_size_bytes=value.info.packet_size_bytes;
  out.info_signaled_after_sent.turn_overhead_bytes=value.info.turn_overhead_bytes;
  out.info_signaled_after_sent.ip_overhead_bytes=value.info.ip_overhead_bytes;
  out.batchable=value.batchable; out.last_packet_in_batch=value.last_packet_in_batch;
  return out;
}
FfiIceRouteEndpoint IceRouteEndpoint(const webrtc::RouteEndpoint& value) {
  return {static_cast<std::int32_t>(value.adapter_type()),value.adapter_id(),value.network_id(),value.uses_turn()};
}
FfiIceRoute IceRoute(std::optional<webrtc::NetworkRoute> value) {
  FfiIceRoute out{};
  if (value) {
    out.present=true; out.connected=value->connected;
    out.local=IceRouteEndpoint(value->local); out.remote=IceRouteEndpoint(value->remote);
    out.last_sent_packet_id=value->last_sent_packet_id; out.packet_overhead=value->packet_overhead;
  }
  return out;
}
}
FfiIcePacketOptions ice_packet_options_default() noexcept {
  webrtc::AsyncSocketPacketOptions value;
  FfiIcePacketOptions out{};
  out.dscp=static_cast<std::int32_t>(value.dscp);
  out.ect_1=value.ect_1; out.packet_id=value.packet_id;
  out.rtp_sendtime_extension_id=value.packet_time_params.rtp_sendtime_extension_id;
  for (auto byte:value.packet_time_params.srtp_auth_key) out.srtp_auth_key.push_back(static_cast<std::uint8_t>(byte));
  out.srtp_auth_tag_len=value.packet_time_params.srtp_auth_tag_len; out.srtp_packet_index=value.packet_time_params.srtp_packet_index;
  out.info=IcePacketInfo(value.info_signaled_after_sent); out.batchable=value.batchable; out.last_packet_in_batch=value.last_packet_in_batch;
  return out;
}
FfiIcePacketState ice_transport_packet_state(const NativeIceTransport& transport) noexcept {
  FfiIcePacketState out{};
  out.available=WithIce(transport,[&](auto& ice) {
    out.name=IceBytes(ice.transport_name()); out.writable=ice.writable(); out.receiving=ice.receiving(); out.error=ice.GetError();
  });
  return out;
}
FfiIceSocketResult ice_transport_send_packet(const NativeIceTransport& transport,rust::Slice<const std::uint8_t> data,const FfiIcePacketOptions& options,std::int32_t flags) noexcept {
  FfiIceSocketResult out{};
  out.available=WithIce(transport,[&](auto& ice) {
    out.result=ice.SendPacket(reinterpret_cast<const char*>(data.data()),data.size(),IcePacketOptions(options),flags);
    out.error=ice.GetError();
  });
  return out;
}
FfiIceSocketResult ice_transport_set_option(const NativeIceTransport& transport,std::uint8_t option,std::int32_t value) noexcept {
  FfiIceSocketResult out{};
  out.available=WithIce(transport,[&](auto& ice) {out.result=ice.SetOption(static_cast<webrtc::Socket::Option>(option),value);out.error=ice.GetError();});
  return out;
}
FfiIceOption ice_transport_get_option(const NativeIceTransport& transport,std::uint8_t option) noexcept {
  FfiIceOption out{};
  out.available=WithIce(transport,[&](auto& ice) {out.success=ice.GetOption(static_cast<webrtc::Socket::Option>(option),&out.value);});
  return out;
}
FfiIceRouteResult ice_transport_route(const NativeIceTransport& transport) noexcept {
  FfiIceRouteResult out{};
  out.available=WithIce(transport,[&](auto& ice) {out.route=IceRoute(ice.network_route());});
  return out;
}

struct IcePacketObserver {
  EventQueue<FfiIcePacketEvent> queue;
  bool registered = false; // only accessed on the network role
};
std::uint8_t ice_transport_register_packet_observer(const NativeIceTransport& transport,std::size_t capacity) noexcept {
  if (!capacity) return 1;
  auto& state=*transport.state();
  return state.network->BlockingCall([&]() -> std::uint8_t {
    auto* ice=state.transport->internal();
    if (!ice) return 2;
    if (state.observer && state.observer->registered) return 3;
    auto observer=std::make_shared<IcePacketObserver>();
    observer->queue.readiness=state.readiness; observer->queue.Reset(capacity);
    std::weak_ptr<IcePacketObserver> weak=observer;
    void* tag=observer.get();
    ice->SubscribeWritableState(tag,[weak](auto* transport) {
      if (auto observer=weak.lock()) {FfiIcePacketEvent e{};e.kind=1;e.flag=transport->writable();observer->queue.Push(std::move(e));}
    });
    ice->SubscribeReadyToSend(tag,[weak](auto*) {
      if (auto observer=weak.lock()) {FfiIcePacketEvent e{};e.kind=2;observer->queue.Push(std::move(e));}
    });
    ice->AddGatheringStateCallback(tag,[weak](auto* transport) {
      if (auto observer=weak.lock()) {FfiIcePacketEvent e{};e.kind=3;e.state=static_cast<std::int32_t>(transport->gathering_state());observer->queue.Push(std::move(e));}
    });
    ice->RegisterReceivedPacketCallback(tag,[weak](auto*,const webrtc::ReceivedIpPacket& packet) {
      if (auto observer=weak.lock()) {
        FfiIcePacketEvent e{};e.kind=4;
        e.payload.reserve(packet.payload().size());for(auto byte:packet.payload())e.payload.push_back(byte);
        e.source=std::make_unique<NativeSocketAddress>(packet.source_address());
        e.has_arrival_time=packet.arrival_time().has_value();
        if(packet.arrival_time()) e.arrival_time=packet.arrival_time()->IsPlusInfinity()?std::numeric_limits<std::int64_t>::max():packet.arrival_time()->us_or(std::numeric_limits<std::int64_t>::min());
        e.ecn=static_cast<std::int32_t>(packet.ecn());e.decryption=static_cast<std::int32_t>(packet.decryption_info());
        observer->queue.Push(std::move(e));
      }
    });
    ice->SubscribeSentPacket(tag,[weak](auto*,const webrtc::SentPacketInfo& packet) {
      if (auto observer=weak.lock()) {
        FfiIcePacketEvent e{};e.kind=5;e.packet_id=packet.packet_id;e.send_time_ms=packet.send_time_ms;e.info=IcePacketInfo(packet.info);
        observer->queue.Push(std::move(e));
      }
    });
    ice->SubscribeNetworkRouteChanged(tag,[weak](auto route) {
      if(auto observer=weak.lock()) {FfiIcePacketEvent e{};e.kind=6;e.route=IceRoute(std::move(route));observer->queue.Push(std::move(e));}
    });
    observer->registered=true;state.observer=std::move(observer);
    return 0;
  });
}
bool ice_transport_unregister_packet_observer(const NativeIceTransport& transport) noexcept {
  auto& state=*transport.state();
  return state.network->BlockingCall([&] {
    auto* ice=state.transport->internal();
    if (state.observer) {
      if (ice && state.observer->registered) {
        void* tag=state.observer.get();
        ice->UnsubscribeWritableState(tag);ice->UnsubscribeReadyToSend(tag);
        ice->RemoveGatheringStateCallback(tag);ice->DeregisterReceivedPacketCallback(tag);
        ice->UnsubscribeSentPacket(tag);ice->UnsubscribeNetworkRouteChanged(tag);
      }
      state.observer->registered=false;state.observer->queue.Reset(0);
    }
    return ice!=nullptr;
  });
}
FfiIcePacketEvent ice_transport_packet_event(const NativeIceTransport& transport) noexcept {
  auto observer=transport.state()->observer;
  if(!observer) return FfiIcePacketEvent{};
  std::lock_guard lock(observer->queue.mutex);
  if(observer->queue.events.empty()) return FfiIcePacketEvent{};
  auto event=std::move(observer->queue.events.front());observer->queue.events.pop_front();return event;
}
std::uint64_t ice_transport_packet_dropped(const NativeIceTransport& transport) noexcept {
  auto observer=transport.state()->observer;
  return observer ? observer->queue.TakeDropped():0;
}

}  // namespace pulsebeam::webrtc_sys
