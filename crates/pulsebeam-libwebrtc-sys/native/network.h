#pragma once

#include <cstdint>
#include <memory>

#include "rust/cxx.h"

namespace webrtc {
class NetworkManager;
class NetworkMonitorFactory;
class PacketSocketFactory;
class FieldTrialsView;
class Thread;
}

namespace pulsebeam::webrtc_sys {

struct RustNetworkMonitorFactory;
struct RustNetworkManagerFactory;
class NativeNetworkChange final {
 public:
  struct State;
  explicit NativeNetworkChange(std::shared_ptr<State> state) noexcept;
  ~NativeNetworkChange();
  bool Notify() const noexcept;
 private:
  std::shared_ptr<State> state_;
};
bool network_change_notify(const NativeNetworkChange& changes) noexcept;
std::unique_ptr<webrtc::NetworkMonitorFactory> network_monitor_factory_adapter(rust::Box<RustNetworkMonitorFactory> factory) noexcept;

std::unique_ptr<webrtc::NetworkManager> network_manager_factory_adapter(rust::Box<RustNetworkManagerFactory> factory, const webrtc::FieldTrialsView& trials, webrtc::Thread* network_thread) noexcept;

class NativeManualClock;
class NativeDriverThread;

class NativeSimulatedNetwork final {
 public:
  struct State;

  explicit NativeSimulatedNetwork(std::shared_ptr<State> state) noexcept;
  ~NativeSimulatedNetwork();
  NativeSimulatedNetwork(const NativeSimulatedNetwork&) = delete;
  NativeSimulatedNetwork& operator=(const NativeSimulatedNetwork&) = delete;

  const std::shared_ptr<State>& state() const noexcept;

 private:
  std::shared_ptr<State> state_;
};

class NativeNetworkEndpoint final {
 public:
  struct State;

  explicit NativeNetworkEndpoint(std::shared_ptr<State> state) noexcept;
  ~NativeNetworkEndpoint();
  NativeNetworkEndpoint(const NativeNetworkEndpoint&) = delete;
  NativeNetworkEndpoint& operator=(const NativeNetworkEndpoint&) = delete;

  const std::shared_ptr<State>& state() const noexcept;

 private:
  std::shared_ptr<State> state_;
};

class NativeNetworkManagerProvider final {
 public:
  explicit NativeNetworkManagerProvider(
      std::shared_ptr<NativeNetworkEndpoint::State> endpoint) noexcept;
  ~NativeNetworkManagerProvider();

  std::unique_ptr<webrtc::NetworkManager> Create() const noexcept;
  const std::shared_ptr<NativeNetworkEndpoint::State>& endpoint()
      const noexcept;

 private:
  std::shared_ptr<NativeNetworkEndpoint::State> endpoint_;
};

class NativePacketSocketFactoryProvider final {
 public:
  explicit NativePacketSocketFactoryProvider(
      std::shared_ptr<NativeNetworkEndpoint::State> endpoint) noexcept;
  ~NativePacketSocketFactoryProvider();

  std::unique_ptr<webrtc::PacketSocketFactory> Create() const noexcept;
  const std::shared_ptr<NativeNetworkEndpoint::State>& endpoint()
      const noexcept;

 private:
  std::shared_ptr<NativeNetworkEndpoint::State> endpoint_;
};

class NativeSimulatedUdpSocket final {
 public:
  struct State;

  explicit NativeSimulatedUdpSocket(std::unique_ptr<State> state) noexcept;
  ~NativeSimulatedUdpSocket();
  NativeSimulatedUdpSocket(const NativeSimulatedUdpSocket&) = delete;
  NativeSimulatedUdpSocket& operator=(const NativeSimulatedUdpSocket&) = delete;

  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeOutboundPacket final {
 public:
  struct State;
  explicit NativeOutboundPacket(std::unique_ptr<State> state) noexcept;
  ~NativeOutboundPacket();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeReceivedPacket final {
 public:
  struct State;
  explicit NativeReceivedPacket(std::unique_ptr<State> state) noexcept;
  ~NativeReceivedPacket();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

std::unique_ptr<NativeSimulatedNetwork> new_simulated_network(
    const NativeManualClock& clock) noexcept;
std::unique_ptr<NativeSimulatedNetwork> new_controlled_simulated_network(
    const NativeManualClock& clock,
    const NativeDriverThread& driver) noexcept;
bool add_simulated_dns_record(const NativeSimulatedNetwork& network,
                              rust::Str hostname,
                              rust::Slice<const std::uint8_t> ip) noexcept;
std::unique_ptr<NativeNetworkEndpoint> register_network_endpoint(
    const NativeSimulatedNetwork& network,
    rust::Slice<const std::uint8_t> ip,
    std::uint8_t& error) noexcept;
void close_network_endpoint(NativeNetworkEndpoint& endpoint) noexcept;

std::unique_ptr<NativeNetworkManagerProvider> new_network_manager_provider(
    const NativeNetworkEndpoint& endpoint) noexcept;
bool network_manager_provider_is_valid(
    const NativeNetworkManagerProvider& provider) noexcept;
std::unique_ptr<NativePacketSocketFactoryProvider>
new_packet_socket_factory_provider(
    const NativeNetworkEndpoint& endpoint) noexcept;
bool packet_socket_factory_supports_udp(
    const NativePacketSocketFactoryProvider& provider) noexcept;
bool packet_socket_factory_supports_tcp(
    const NativePacketSocketFactoryProvider& provider) noexcept;
bool packet_socket_factory_supports_dns(
    const NativePacketSocketFactoryProvider& provider) noexcept;

std::unique_ptr<NativeSimulatedUdpSocket> create_simulated_udp_socket(
    const NativePacketSocketFactoryProvider& provider,
    std::uint16_t port,
    std::uint8_t& error) noexcept;
rust::Vec<std::uint8_t> simulated_udp_local_ip(
    const NativeSimulatedUdpSocket& socket) noexcept;
std::uint16_t simulated_udp_local_port(
    const NativeSimulatedUdpSocket& socket) noexcept;
bool simulated_udp_send_to(const NativeSimulatedUdpSocket& socket,
                           rust::Slice<const std::uint8_t> destination_ip,
                           std::uint16_t destination_port,
                           rust::Vec<std::uint8_t> payload,
                           std::uint8_t& error) noexcept;
std::unique_ptr<NativeReceivedPacket> simulated_udp_take_received(
    const NativeSimulatedUdpSocket& socket) noexcept;
void close_simulated_udp_socket(NativeSimulatedUdpSocket& socket) noexcept;

std::unique_ptr<NativeOutboundPacket> take_outbound_packet(
    const NativeSimulatedNetwork& network) noexcept;
std::uint64_t outbound_packet_id(const NativeOutboundPacket& packet) noexcept;
std::uint8_t outbound_packet_kind(const NativeOutboundPacket& packet) noexcept;
rust::Vec<std::uint8_t> outbound_packet_source_ip(
    const NativeOutboundPacket& packet) noexcept;
std::uint16_t outbound_packet_source_port(
    const NativeOutboundPacket& packet) noexcept;
rust::Vec<std::uint8_t> outbound_packet_destination_ip(
    const NativeOutboundPacket& packet) noexcept;
std::uint16_t outbound_packet_destination_port(
    const NativeOutboundPacket& packet) noexcept;
rust::Vec<std::uint8_t> outbound_packet_payload(
    const NativeOutboundPacket& packet) noexcept;
std::int64_t outbound_packet_deadline_us(
    const NativeOutboundPacket& packet) noexcept;
bool deliver_outbound_packet(const NativeSimulatedNetwork& network,
                             std::uint64_t packet_id,
                             bool keep_pending,
                             std::uint8_t& error) noexcept;
bool drop_outbound_packet(const NativeSimulatedNetwork& network,
                          std::uint64_t packet_id,
                          std::uint8_t& error) noexcept;
bool inject_simulated_tcp_data(const NativeSimulatedNetwork& network,
                               rust::Slice<const std::uint8_t> source_ip,
                               std::uint16_t source_port,
                               rust::Slice<const std::uint8_t> destination_ip,
                               std::uint16_t destination_port,
                               rust::Slice<const std::uint8_t> data,
                               std::uint8_t& error) noexcept;

rust::Vec<std::uint8_t> received_packet_source_ip(
    const NativeReceivedPacket& packet) noexcept;
std::uint16_t received_packet_source_port(
    const NativeReceivedPacket& packet) noexcept;
rust::Vec<std::uint8_t> received_packet_payload(
    const NativeReceivedPacket& packet) noexcept;

}  // namespace pulsebeam::webrtc_sys
