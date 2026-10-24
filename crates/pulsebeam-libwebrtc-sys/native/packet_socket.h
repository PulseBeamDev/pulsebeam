#pragma once
#include <cstddef>
#include <atomic>
#include <cstdint>
#include <memory>
#include "rust/cxx.h"
#include "api/packet_socket_factory.h"
#include "rtc_base/async_packet_socket.h"
#include "rtc_base/network/received_packet.h"
#include "rtc_base/network/sent_packet.h"
namespace webrtc { class Thread; }
namespace pulsebeam::webrtc_sys {
class NativeSocketAddress;
struct RustPacketSocketFactory;
// Private installation in the binding's PeerConnectionFactory only. Any TLS
// option verifier originates from our peer dependency adapter (tls.h).
std::unique_ptr<webrtc::PacketSocketFactory> socket_factory_adapter(rust::Box<RustPacketSocketFactory> source, const webrtc::Environment& environment, webrtc::Thread* network_role) noexcept;
struct RustPacketSocket;
struct RustSocketCallback;
struct RustSocketRole;
struct FfiOwnedSocketResult;
struct FfiSocketPacketOptions;
struct FfiSocketObservation;
struct FfiSocketSubscriptionResult;
struct RustListenSocket;
struct RustListenCallback;
struct RustAcceptCreator;
struct FfiListenSubscriptionResult;
struct FfiSocketAcceptResult;
class NativePacketSocketEvents;
class NativePacketSocketResources {
 public:
  struct State;
  NativePacketSocketResources() noexcept;
  ~NativePacketSocketResources();
  void Retire() noexcept;
  std::shared_ptr<State> state;
};
// The SDK factory exclusively owns this guard, not its metadata aliases. Destroy
// it before releasing the role, including on rejected/partial construction.
// It breaks product/context/resource cycles even when aliases remain alive.
class NativePacketSocketScope {
 public:
  NativePacketSocketScope() noexcept;
  ~NativePacketSocketScope();
  NativePacketSocketScope(const NativePacketSocketScope&) = delete;
  NativePacketSocketScope& operator=(const NativePacketSocketScope&) = delete;
  const std::shared_ptr<NativePacketSocketResources>& resources() const noexcept;
 private:
  std::shared_ptr<NativePacketSocketResources> resources_;
};
std::unique_ptr<NativePacketSocketScope> packet_socket_scope_new() noexcept;
std::shared_ptr<NativePacketSocketResources> packet_socket_scope_resources(const NativePacketSocketScope& scope) noexcept;
std::shared_ptr<NativePacketSocketEvents> packet_socket_events_in_scope(std::shared_ptr<NativePacketSocketResources> resources) noexcept;
class NativePacketSocketEvents {
 public:
  struct State;
  explicit NativePacketSocketEvents(std::shared_ptr<State> state) noexcept;
  std::shared_ptr<State> state;
};
class NativeOwnedPacketSocket {
 public:
  struct Core;
  explicit NativeOwnedPacketSocket(std::shared_ptr<Core> core) noexcept;
  ~NativeOwnedPacketSocket();
  std::shared_ptr<Core> core;
};
std::unique_ptr<NativeOwnedPacketSocket> owned_packet_socket_new(rust::Box<RustPacketSocket> source, std::shared_ptr<NativePacketSocketEvents> events, std::unique_ptr<NativePacketSocketScope> scope, rust::Box<RustSocketRole> role) noexcept;
FfiOwnedSocketResult socket_member_call(const NativePacketSocketEvents& events, std::uint8_t operation, rust::Slice<const std::uint8_t> payload, std::unique_ptr<NativeSocketAddress> address, FfiSocketPacketOptions options, std::int32_t option, std::int32_t value) noexcept;
class NativePacketSocketSubscription {
 public:
  struct State;
  NativePacketSocketSubscription(std::shared_ptr<NativePacketSocketEvents::State> owner, std::shared_ptr<State> state) noexcept;
  ~NativePacketSocketSubscription();
  bool Cancel() const noexcept;
  bool Active() const noexcept;
  bool Bound() const noexcept;
 private:
  std::shared_ptr<NativePacketSocketEvents::State> owner_;
  std::shared_ptr<State> state_;
};
FfiSocketSubscriptionResult socket_observe(const NativePacketSocketEvents& events, std::uint8_t kind, bool legacy, rust::Box<RustSocketCallback> callback) noexcept;
bool socket_subscription_cancel(const NativePacketSocketSubscription& subscription) noexcept;
bool socket_subscription_active(const NativePacketSocketSubscription& subscription) noexcept;
bool socket_subscription_bound(const NativePacketSocketSubscription& subscription) noexcept;
std::uint8_t packet_socket_resources_status(const NativePacketSocketResources& resources) noexcept;
// Creation and scope retirement run on the native network role. Retire even
// when construction rejects; posted values must never retain a live role loan.
void packet_socket_events_retire(const NativePacketSocketEvents& events) noexcept;
std::unique_ptr<webrtc::AsyncPacketSocket> packet_socket_adapter(rust::Box<RustPacketSocket> source, std::shared_ptr<NativePacketSocketEvents> events) noexcept;
class NativeListenSocketEvents {
 public:
  struct State;
  explicit NativeListenSocketEvents(std::shared_ptr<State> value) noexcept;
  std::shared_ptr<State> state;
};
class NativeOwnedListenSocket {
 public:
  struct Core;
  explicit NativeOwnedListenSocket(std::shared_ptr<Core> core) noexcept;
  ~NativeOwnedListenSocket();
  std::shared_ptr<Core> core;
};
class NativeListenSocketSubscription {
 public:
  struct State;
  NativeListenSocketSubscription(std::shared_ptr<NativeListenSocketEvents::State> owner, std::shared_ptr<State> state) noexcept;
  ~NativeListenSocketSubscription();
  bool Cancel() const noexcept;
  bool Active() const noexcept;
 private:
  std::shared_ptr<NativeListenSocketEvents::State> owner_;
  std::shared_ptr<State> state_;
};
std::unique_ptr<NativeOwnedListenSocket> owned_listen_socket_new(rust::Box<RustListenSocket> source, std::shared_ptr<NativeListenSocketEvents> events, std::unique_ptr<NativePacketSocketScope> scope, rust::Box<RustSocketRole> role) noexcept;
FfiOwnedSocketResult listen_member_call(const NativeListenSocketEvents& events, bool address) noexcept;
FfiListenSubscriptionResult listen_observe(const NativeListenSocketEvents& events, bool ownership, rust::Box<RustListenCallback> callback) noexcept;
bool listen_subscription_cancel(const NativeListenSocketSubscription& subscription) noexcept;
bool listen_subscription_active(const NativeListenSocketSubscription& subscription) noexcept;
class NativeAcceptedPacketSocket {
 public:
  NativeAcceptedPacketSocket(std::unique_ptr<webrtc::AsyncPacketSocket> socket, std::shared_ptr<NativePacketSocketResources::State> scope) noexcept;
  NativeAcceptedPacketSocket(std::shared_ptr<NativeOwnedPacketSocket::Core> product, std::shared_ptr<NativePacketSocketResources::State> scope) noexcept;
  ~NativeAcceptedPacketSocket();
  std::shared_ptr<NativeOwnedPacketSocket::Core> standalone;
  std::unique_ptr<webrtc::AsyncPacketSocket> socket;
  std::shared_ptr<NativePacketSocketResources::State> scope;
};
std::unique_ptr<NativeAcceptedPacketSocket> owned_socket_into_accepted(std::unique_ptr<NativeOwnedPacketSocket> socket, std::shared_ptr<NativePacketSocketResources> listener) noexcept;
class NativePacketSocketAccept {
 public:
  mutable std::atomic<std::uint8_t> status{0};
};
std::shared_ptr<NativeListenSocketEvents> listen_socket_events_in_scope(std::shared_ptr<NativePacketSocketResources> resources, bool standalone = false) noexcept;
void listen_socket_events_retire(const NativeListenSocketEvents& events) noexcept;
// Private SDK-managed domain: TCPPort installs its ownership-taking receiver
// before queued dispatch. Standalone owned creation instead requires explicit
// register_accept before publication; it does not use this readiness proof.
std::unique_ptr<webrtc::AsyncListenSocket> listen_socket_adapter(rust::Box<RustListenSocket> source, std::shared_ptr<NativeListenSocketEvents> events) noexcept;
std::unique_ptr<NativeAcceptedPacketSocket> accepted_socket_new(rust::Box<RustPacketSocket> source, std::shared_ptr<NativePacketSocketEvents> events, std::shared_ptr<NativePacketSocketResources> resources) noexcept;
FfiSocketAcceptResult listen_accept(const NativeListenSocketEvents& events, std::unique_ptr<NativeAcceptedPacketSocket> socket, bool post) noexcept;
std::shared_ptr<NativePacketSocketAccept> listen_post_creator(const NativeListenSocketEvents& events, rust::Box<RustAcceptCreator> creator) noexcept;
FfiSocketAcceptResult listen_accept_prepared(const NativeListenSocketEvents& events, std::unique_ptr<NativeAcceptedPacketSocket> socket, std::shared_ptr<NativePacketSocketAccept> ticket) noexcept;
void listen_accept_creation_failed(const NativePacketSocketAccept& ticket, bool retired) noexcept;
std::uint8_t listen_accept_status(const NativePacketSocketAccept& ticket) noexcept;
bool listen_accept_cancel(const NativePacketSocketAccept& ticket) noexcept;
struct FfiSocketPacketInfo;
struct FfiSocketPacketOptions;
struct FfiSocketSentPacket;
struct FfiSocketReceivedPacket;
std::uint8_t socket_event_simple(const NativePacketSocketEvents& events, std::uint8_t kind, std::int32_t error, bool post) noexcept;
std::uint8_t socket_event_received(const NativePacketSocketEvents& events, FfiSocketReceivedPacket packet, bool post) noexcept;
std::uint8_t socket_event_sent(const NativePacketSocketEvents& events, FfiSocketSentPacket packet, bool post) noexcept;
std::uint8_t socket_event_address(const NativePacketSocketEvents& events, std::unique_ptr<NativeSocketAddress> address, bool post) noexcept;
std::uint8_t socket_copy_information(const NativePacketSocketEvents& events, std::size_t size, FfiSocketPacketInfo& info) noexcept;
std::int32_t socket_provider_io_error() noexcept;
FfiSocketPacketInfo packet_info_to_ffi(const webrtc::PacketInfo& value) noexcept;
webrtc::PacketInfo packet_info_from_ffi(const FfiSocketPacketInfo& value) noexcept;
FfiSocketPacketOptions packet_options_to_ffi(const webrtc::AsyncSocketPacketOptions& value) noexcept;
webrtc::AsyncSocketPacketOptions packet_options_from_ffi(const FfiSocketPacketOptions& value) noexcept;
FfiSocketSentPacket sent_packet_to_ffi(const webrtc::SentPacketInfo& value) noexcept;
webrtc::SentPacketInfo sent_packet_from_ffi(const FfiSocketSentPacket& value) noexcept;
FfiSocketReceivedPacket received_ip_packet_to_ffi(const webrtc::ReceivedIpPacket& value) noexcept;
FfiSocketReceivedPacket socket_received_packet_copy(const FfiSocketReceivedPacket& packet, std::uint8_t decryption) noexcept;
FfiSocketReceivedPacket socket_received_packet_legacy(rust::Slice<const std::uint8_t> payload, const NativeSocketAddress& address, std::int64_t time_us) noexcept;
}  // namespace pulsebeam::webrtc_sys
