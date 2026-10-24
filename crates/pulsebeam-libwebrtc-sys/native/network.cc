#include "pulsebeam-webrtc-sys/native/network.h"

#include <algorithm>
#include <cerrno>
#include <cstring>
#include <deque>
#include <functional>
#include <map>
#include <mutex>
#include <optional>
#include <set>
#include <utility>
#include <vector>

#include "api/async_dns_resolver.h"
#include "api/environment/environment.h"
#include "api/environment/environment_factory.h"
#include "api/packet_socket_factory.h"
#include "p2p/base/async_stun_tcp_socket.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/network_values.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "rtc_base/network_monitor.h"
#include "rtc_base/network_monitor_factory.h"
#include "rtc_base/async_packet_socket.h"
#include "rtc_base/socket_adapters.h"
#include "rtc_base/async_tcp_socket.h"
#include "rtc_base/ip_address.h"
#include "rtc_base/network.h"
#include "rtc_base/network/received_packet.h"
#include "rtc_base/network/sent_packet.h"
#include "rtc_base/socket.h"
#include "rtc_base/socket_address.h"
#include "rtc_base/ssl_adapter.h"
#include "rtc_base/thread.h"

namespace pulsebeam::webrtc_sys {

struct NativeNetworkChange::State {
  std::mutex mutex;
  webrtc::Thread* thread = nullptr;
  bool running = false;
  std::uint64_t generation = 0;
  std::function<void()> callback;
};
NativeNetworkChange::NativeNetworkChange(std::shared_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeNetworkChange::~NativeNetworkChange() = default;
bool NativeNetworkChange::Notify() const noexcept {
  auto state = state_;
  // Posting under the retirement lock ensures the thread is still owned by
  // the factory. Neither a retained notifier nor a queued task owns that thread.
  std::lock_guard lock(state->mutex);
  if (!state->running || !state->thread) return false;
  auto generation = state->generation;
  state->thread->PostTask([state, generation] {
    std::function<void()> callback;
    {
      std::lock_guard lock(state->mutex);
      if (!state->running || state->generation != generation) return;
      callback = state->callback;
    }
    // Monitor retirement and this task run on the same network thread. The
    // callback may reenter teardown, so no monitor access follows invocation.
    if (callback) callback();
  });
  return true;
}
bool network_change_notify(const NativeNetworkChange& changes) noexcept {
  return changes.Notify();
}

namespace {

rust::Vec<std::uint8_t> FromIp(const webrtc::IPAddress& ip) noexcept;
rust::Slice<const std::uint8_t> InterfaceBytes(absl::string_view name) {
  return {reinterpret_cast<const std::uint8_t*>(name.data()), name.size()};
}
class RustNetworkMonitorAdapter final : public webrtc::NetworkMonitorInterface {
 public:
  RustNetworkMonitorAdapter(rust::Box<RustNetworkMonitor> monitor,
                            std::shared_ptr<NativeNetworkChange::State> state)
      : monitor_(std::move(monitor)), state_(std::move(state)) {
    state_->callback = [this] { InvokeNetworksChangedCallback(); };
  }
  ~RustNetworkMonitorAdapter() override {
    // ConnectionContext destroys BasicNetworkManager on signaling, even though
    // the monitor was created/used on network. Retire the Rust product on its
    // original sequence while the factory still owns both threads.
    auto retire = [this] {
      if (Suspend()) network_monitor_stop(**monitor_);
      {
        std::lock_guard lock(state_->mutex);
        state_->thread = nullptr;
        state_->callback = {};
      }
      monitor_.reset();
    };
    webrtc::Thread* thread;
    { std::lock_guard lock(state_->mutex); thread = state_->thread; }
    if (thread && !thread->IsCurrent()) thread->BlockingCall(retire);
    else retire();
  }
  void Start() override {
    {
      std::lock_guard lock(state_->mutex);
      ++state_->generation;
      state_->running = true;
    }
    if (!network_monitor_start(**monitor_)) {
      Suspend();
      network_monitor_stop(**monitor_);
    }
  }
  void Stop() override {
    Suspend();
    network_monitor_stop(**monitor_);
  }
  InterfaceInfo GetInterfaceInfo(absl::string_view name) override {
    auto info = network_monitor_info(**monitor_, InterfaceBytes(name));
    InterfaceInfo result;
    result.adapter_type = static_cast<webrtc::AdapterType>(info.adapter_type);
    result.underlying_type_for_vpn = static_cast<webrtc::AdapterType>(info.underlying_type_for_vpn);
    result.network_preference = static_cast<webrtc::NetworkPreference>(info.preference);
    result.available = info.available;
    result.slice = static_cast<webrtc::NetworkSlice>(info.slice);
    return result;
  }
  bool SupportsBindSocketToNetwork() const override {
    return network_monitor_supports_binding(**monitor_);
  }
  webrtc::NetworkBindingResult BindSocketToNetwork(
      int socket, const webrtc::IPAddress& address,
      absl::string_view name) override {
    auto bytes = FromIp(address);
    return static_cast<webrtc::NetworkBindingResult>(network_monitor_bind(
        **monitor_, socket, {bytes.data(), bytes.size()}, InterfaceBytes(name)));
  }
 private:
  bool Suspend() {
    std::lock_guard lock(state_->mutex);
    bool running = state_->running;
    state_->running = false;
    ++state_->generation;
    return running;
  }
  std::optional<rust::Box<RustNetworkMonitor>> monitor_;
  std::shared_ptr<NativeNetworkChange::State> state_;
};
class RustNetworkMonitorFactoryAdapter final : public webrtc::NetworkMonitorFactory {
 public:
  explicit RustNetworkMonitorFactoryAdapter(rust::Box<RustNetworkMonitorFactory> factory)
      : factory_(std::move(factory)) {}
  webrtc::NetworkMonitorInterface* CreateNetworkMonitor(
      const webrtc::FieldTrialsView& trials) override {
    auto state = std::make_shared<NativeNetworkChange::State>();
    state->thread = webrtc::Thread::Current();
    if (!state->thread) return nullptr;
    auto monitor = network_monitor_create(*factory_,
        std::make_unique<NativeFieldTrials>(trials.CreateCopy()),
        std::make_shared<NativeNetworkChange>(state));
    if (!network_monitor_present(*monitor)) {
      std::lock_guard lock(state->mutex);
      state->thread = nullptr;
      return nullptr;
    }
    return new RustNetworkMonitorAdapter(std::move(monitor), std::move(state));
  }
 private:
  rust::Box<RustNetworkMonitorFactory> factory_;
};


enum Error : std::uint8_t {
  kOk = 0,
  kInvalidAddress = 1,
  kInvalidPort = 2,
  kAddressInUse = 3,
  kEndpointClosed = 4,
  kSocketClosed = 5,
  kPacketNotFound = 6,
  kDestinationUnavailable = 7,
  kUnsupportedTransport = 8,
  kUnsupportedDns = 9,
  kNativeConstructionFailed = 10,
};

class SimulatedAsyncPacketSocket;
class SimulatedAsyncTcpSocket;

struct TcpRegistration {
  std::mutex mutex;
  webrtc::Thread* thread = nullptr;
  SimulatedAsyncTcpSocket* socket = nullptr;
};

struct SocketRegistration {
  std::mutex mutex;
  webrtc::Thread* thread = nullptr;
  SimulatedAsyncPacketSocket* socket = nullptr;
};

struct PacketData {
  enum class Kind { kUdp, kTcpConnect, kTcpData } kind = Kind::kUdp;
  std::weak_ptr<TcpRegistration> tcp_source;
  std::uint64_t id;
  webrtc::SocketAddress source;
  webrtc::SocketAddress destination;
  std::vector<std::uint8_t> payload;
  std::int64_t deadline_us;
};

struct ReceivedData {
  webrtc::SocketAddress source;
  std::vector<std::uint8_t> payload;
};

std::optional<webrtc::IPAddress> ToIp(
    rust::Slice<const std::uint8_t> bytes) noexcept {
  if (bytes.size() == 4) {
    const std::uint32_t value =
        (static_cast<std::uint32_t>(bytes[0]) << 24) |
        (static_cast<std::uint32_t>(bytes[1]) << 16) |
        (static_cast<std::uint32_t>(bytes[2]) << 8) |
        static_cast<std::uint32_t>(bytes[3]);
    return webrtc::IPAddress(value);
  }
  if (bytes.size() == 16) {
    in6_addr value;
    std::memcpy(&value, bytes.data(), bytes.size());
    return webrtc::IPAddress(value);
  }
  return std::nullopt;
}

rust::Vec<std::uint8_t> FromIp(const webrtc::IPAddress& ip) noexcept {
  rust::Vec<std::uint8_t> result;
  if (ip.family() == AF_INET) {
    const std::uint32_t value = ip.v4AddressAsHostOrderInteger();
    result.push_back(static_cast<std::uint8_t>(value >> 24));
    result.push_back(static_cast<std::uint8_t>(value >> 16));
    result.push_back(static_cast<std::uint8_t>(value >> 8));
    result.push_back(static_cast<std::uint8_t>(value));
  } else if (ip.family() == AF_INET6) {
    const in6_addr value = ip.ipv6_address();
    const auto* bytes = reinterpret_cast<const std::uint8_t*>(&value);
    for (std::size_t index = 0; index < 16; ++index) {
      result.push_back(bytes[index]);
    }
  }
  return result;
}

rust::Vec<std::uint8_t> FromBytes(
    const std::vector<std::uint8_t>& bytes) noexcept {
  rust::Vec<std::uint8_t> result;
  result.reserve(bytes.size());
  for (std::uint8_t byte : bytes) {
    result.push_back(byte);
  }
  return result;
}

void CloseRegistration(const std::shared_ptr<SocketRegistration>& registration);

}  // namespace

std::unique_ptr<webrtc::NetworkMonitorFactory> network_monitor_factory_adapter(
    rust::Box<RustNetworkMonitorFactory> factory) noexcept {
  if (!network_monitor_factory_present(*factory)) return nullptr;
  return std::make_unique<RustNetworkMonitorFactoryAdapter>(std::move(factory));
}

namespace {
void ManagerSignalActive(const std::shared_ptr<NativeNetworkChange::State>& signal, bool active, bool retire = false) {
  std::lock_guard lock(signal->mutex);
  if (signal->running != active || retire) ++signal->generation;
  signal->running = active;
  if (retire) { signal->thread = nullptr; signal->callback = {}; }
}

// The SDK destroys this facade on signaling. Its Rust product and every
// sequence-bound dependent Network are explicitly retired on the network role.
class RustNetworkManagerAdapter final : public webrtc::NetworkManager {
 public:
  struct State {
    State(rust::Box<RustNetworkManagerFactory> selected_factory, rust::Box<RustNetworkManager> selected_product,
          std::shared_ptr<NativeNetworkChange::State> changed, std::shared_ptr<NativeNetworkChange::State> error,
          std::shared_ptr<NativeNetworkResources> selected_resources)
        : factory(std::move(selected_factory)), changes(std::move(changed)), errors(std::move(error)), resources(std::move(selected_resources)), product(std::move(selected_product)) {}
    // Keep the creation environment through product and network destruction.
    rust::Box<RustNetworkManagerFactory> factory;
    std::shared_ptr<NativeNetworkChange::State> changes;
    std::shared_ptr<NativeNetworkChange::State> errors;
    std::shared_ptr<NativeNetworkResources> resources;
    std::map<const webrtc::Network*, std::shared_ptr<NativeNetwork>> retained_networks;
    std::map<webrtc::MdnsResponderInterface*, std::shared_ptr<NativeMdnsResponder>> retained_mdns;
    rust::Box<RustNetworkManager> product;
    std::size_t updating_clients = 0;
    bool failure_signalled = false;
  };
  RustNetworkManagerAdapter(webrtc::Thread* owner, std::unique_ptr<State> state)
      : owner_(owner), state_(std::move(state)) {
    state_->resources->BindManager(this);
    state_->changes->callback = [resources = state_->resources] { resources->NotifyManager(0); };
    state_->errors->callback = [resources = state_->resources] { resources->NotifyManager(1); };
  }
  ~RustNetworkManagerAdapter() override {
    auto retire = [&] {
      ManagerSignalActive(state_->changes, false, true);
      ManagerSignalActive(state_->errors, false, true);
      while (state_->updating_clients != 0) {
        --state_->updating_clients;
        network_manager_stop(*state_->product);
      }
      state_->resources->Retire();
      state_.reset();
    };
    if (owner_->IsCurrent()) retire(); else owner_->BlockingCall(retire);
  }
  bool InitializeProduct() { return network_manager_initialize(*state_->product); }
  void Initialize() override { Invoke([&] { network_manager_initialize(*state_->product); Failure(); }); }
  void StartUpdating() override {
    Invoke([&] {
      ++state_->updating_clients;
      ManagerSignalActive(state_->changes, true);
      ManagerSignalActive(state_->errors, true);
      // Every newly updating consumer needs its initial error even if an
      // earlier consumer already observed this product's terminal failure.
      state_->failure_signalled = false;
      network_manager_start(*state_->product);
      Failure();
    });
  }
  void StopUpdating() override {
    Invoke([&] {
      if (state_->updating_clients != 0) --state_->updating_clients;
      if (state_->updating_clients == 0) {
        ManagerSignalActive(state_->changes, false);
        ManagerSignalActive(state_->errors, false);
        state_->failure_signalled = false;
      }
      network_manager_stop(*state_->product);
    });
  }
  std::vector<const webrtc::Network*> GetNetworks() const override { return Networks(false); }
  std::vector<const webrtc::Network*> GetAnyAddressNetworks() override { return Networks(true); }
  EnumerationPermission enumeration_permission() const override {
    return Invoke([&] {
      const bool blocked = network_manager_permission(*state_->product);
      Failure();
      return blocked ? ENUMERATION_BLOCKED : ENUMERATION_ALLOWED;
    });
  }
  bool GetDefaultLocalAddress(int family, webrtc::IPAddress* address) const override {
    return Invoke([&] {
      auto result = network_manager_default_address(*state_->product, family);
      Failure();
      if (!result.present) return false;
      if (result.ip.empty()) { *address = webrtc::IPAddress(); return true; }
      auto ip = ToIp({result.ip.data(), result.ip.size()});
      if (!ip) return false;
      *address = *ip;
      return true;
    });
  }
  webrtc::MdnsResponderInterface* GetMdnsResponder() const override {
    return Invoke([&]() -> webrtc::MdnsResponderInterface* {
      auto value = network_manager_mdns(*state_->product);
      Failure();
      if (!value) return nullptr;
      auto* responder = value->responder();
      if (responder) state_->retained_mdns.try_emplace(responder, std::move(value));
      return responder;
    });
  }
  void DumpNetworks() override { Invoke([&] { network_manager_dump(*state_->product); Failure(); }); }
  void set_vpn_list(const std::vector<webrtc::NetworkMask>& masks) override {
    Invoke([&] {
      rust::Vec<FfiIpNetworkMask> values;
      for (const auto& mask : masks) values.push_back({FromIp(mask.address()), mask.prefix_length()});
      network_manager_set_vpn(*state_->product, std::move(values));
      Failure();
    });
  }
 private:
  template<class F> auto Invoke(F&& operation) const -> decltype(operation()) {
    if (owner_->IsCurrent()) return operation();
    return owner_->BlockingCall(std::forward<F>(operation));
  }
  void Failure() const {
    if (!state_->failure_signalled && network_manager_failed(*state_->product)) {
      if (NativeNetworkChange(state_->errors).Notify()) state_->failure_signalled = true;
    }
  }
  std::vector<const webrtc::Network*> Networks(bool any) const {
    return Invoke([&] {
      auto values = network_manager_networks(*state_->product, any);
      std::vector<const webrtc::Network*> result;
      result.reserve(values.size());
      for (const auto& value : values) {
        // Public Rust Network constructors never return null. Do not pass a
        // null entry into the SDK's dereferencing allocator path regardless.
        if (!value.network) continue;
        const auto* native = &value.network->network();
        state_->retained_networks.try_emplace(native, value.network);
        result.push_back(native);
      }
      Failure();
      return result;
    });
  }
  webrtc::Thread* const owner_;
  std::unique_ptr<State> state_;
};
}  // namespace

std::unique_ptr<webrtc::NetworkManager> network_manager_factory_adapter(
    rust::Box<RustNetworkManagerFactory> factory, const webrtc::FieldTrialsView& trials,
    webrtc::Thread* network_thread) noexcept {
  if (!network_manager_factory_present(*factory) || !network_thread) return nullptr;
  std::unique_ptr<webrtc::NetworkManager> result;
  auto create = [&] {
    auto changes = std::make_shared<NativeNetworkChange::State>();
    auto errors = std::make_shared<NativeNetworkChange::State>();
    changes->thread = errors->thread = network_thread;
    auto resources = std::make_shared<NativeNetworkResources>();
    auto product = network_manager_create(*factory, std::make_unique<NativeFieldTrials>(trials.CreateCopy()),
        std::make_shared<NativeNetworkChange>(changes), std::make_shared<NativeNetworkChange>(errors), resources);
    if (!network_manager_present(*product)) {
      ManagerSignalActive(changes, false, true);
      ManagerSignalActive(errors, false, true);
      resources->Retire();
      return;
    }
    auto state = std::make_unique<RustNetworkManagerAdapter::State>(std::move(factory), std::move(product), changes, errors, resources);
    auto adapter = std::make_unique<RustNetworkManagerAdapter>(network_thread, std::move(state));
    // ConnectionContext does not call Initialize on an injected manager. Finish
    // initialization before its first allocator can subscribe/start updating.
    if (!adapter->InitializeProduct()) return;
    result = std::move(adapter);
  };
  if (network_thread->IsCurrent()) create(); else network_thread->BlockingCall(create);
  return result;
}

struct NativeSimulatedNetwork::State {
  explicit State(const NativeManualClock* clock_value) noexcept
      : clock(clock_value), owned_thread(webrtc::Thread::Create()),
        thread(owned_thread.get()) {}
  State(const NativeManualClock* clock_value, webrtc::Thread* borrowed) noexcept
      : clock(clock_value), thread(borrowed) {}

  const NativeManualClock* clock;
  std::unique_ptr<webrtc::Thread> owned_thread;
  webrtc::Thread* thread;
  std::mutex mutex;
  std::map<webrtc::IPAddress, std::weak_ptr<NativeNetworkEndpoint::State>>
      endpoints;
  std::map<webrtc::SocketAddress, std::weak_ptr<SocketRegistration>> sockets;
  std::map<webrtc::SocketAddress, std::weak_ptr<TcpRegistration>> tcp_sockets;
  std::map<std::string, std::vector<webrtc::IPAddress>> dns_records;
  std::map<std::uint64_t, PacketData> pending;
  std::deque<std::uint64_t> outbound;
  std::uint64_t next_packet_id = 1;
};

struct NativeNetworkEndpoint::State {
  State(std::shared_ptr<NativeSimulatedNetwork::State> network_value,
        webrtc::IPAddress ip_value) noexcept
      : network(std::move(network_value)), ip(std::move(ip_value)) {}

  std::shared_ptr<NativeSimulatedNetwork::State> network;
  webrtc::IPAddress ip;
  std::mutex mutex;
  bool closed = false;
  std::vector<std::weak_ptr<SocketRegistration>> sockets;
  std::vector<std::weak_ptr<TcpRegistration>> tcp_sockets;
};

namespace {

bool EndpointClosed(
    const std::shared_ptr<NativeNetworkEndpoint::State>& endpoint) noexcept {
  std::lock_guard lock(endpoint->mutex);
  return endpoint->closed;
}

void RemoveSocket(const std::shared_ptr<NativeSimulatedNetwork::State>& network,
                  const webrtc::SocketAddress& address,
                  const std::shared_ptr<SocketRegistration>& registration) {
  std::lock_guard lock(network->mutex);
  const auto found = network->sockets.find(address);
  if (found != network->sockets.end() &&
      found->second.lock() == registration) {
    network->sockets.erase(found);
  }
}

class SimulatedAsyncPacketSocket final : public webrtc::AsyncPacketSocket {
 public:
  SimulatedAsyncPacketSocket(
      std::shared_ptr<NativeNetworkEndpoint::State> endpoint,
      webrtc::SocketAddress address,
      std::shared_ptr<SocketRegistration> registration) noexcept
      : endpoint_(std::move(endpoint)),
        address_(std::move(address)),
        registration_(std::move(registration)) {}

  ~SimulatedAsyncPacketSocket() override {
    Close();
    std::lock_guard lock(registration_->mutex);
    registration_->socket = nullptr;
    registration_->thread = nullptr;
  }

  webrtc::SocketAddress GetLocalAddress() const override { return address_; }
  webrtc::SocketAddress GetRemoteAddress() const override { return {}; }

  int Send(const void*,
           std::size_t,
           const webrtc::AsyncSocketPacketOptions&) override {
    error_ = EINVAL;
    return -1;
  }

  int SendTo(const void* data,
             std::size_t size,
             const webrtc::SocketAddress& destination,
             const webrtc::AsyncSocketPacketOptions& options) override {
    if (closed_) {
      error_ = EINVAL;
      return -1;
    }
    if (!destination.IsComplete()) {
      error_ = EINVAL;
      return -1;
    }
    auto network = endpoint_->network;
    PacketData packet;
    {
      std::lock_guard lock(network->mutex);
      packet.id = network->next_packet_id++;
      packet.source = address_;
      packet.destination = destination;
      const auto* bytes = static_cast<const std::uint8_t*>(data);
      packet.payload.assign(bytes, bytes + size);
      packet.deadline_us = manual_clock_time_us(*network->clock);
      network->pending.emplace(packet.id, packet);
      network->outbound.push_back(packet.id);
    }
    webrtc::SentPacketInfo sent(options.packet_id, packet.deadline_us / 1000);
    NotifySentPacket(this, sent);
    return static_cast<int>(size);
  }

  int Close() override {
    if (closed_) {
      return 0;
    }
    closed_ = true;
    RemoveSocket(endpoint_->network, address_, registration_);
    NotifyClosed(0);
    return 0;
  }

  State GetState() const override {
    return closed_ ? STATE_CLOSED : STATE_BOUND;
  }
  int GetOption(webrtc::Socket::Option option, int* value) override {
    const auto found = options_.find(option);
    if (found == options_.end()) {
      error_ = EINVAL;
      return -1;
    }
    *value = found->second;
    return 0;
  }
  int SetOption(webrtc::Socket::Option option, int value) override {
    options_[option] = value;
    return 0;
  }
  int GetError() const override { return error_; }
  void SetError(int error) override { error_ = error; }

  bool Receive(const PacketData& packet) {
    if (closed_ || EndpointClosed(endpoint_)) {
      return false;
    }
    webrtc::ReceivedIpPacket received(
        std::span<const std::uint8_t>(packet.payload), packet.source,
        webrtc::Timestamp::Micros(manual_clock_time_us(*endpoint_->network->clock)));
    NotifyPacketReceived(received);
    return true;
  }

 private:
  std::shared_ptr<NativeNetworkEndpoint::State> endpoint_;
  webrtc::SocketAddress address_;
  std::shared_ptr<SocketRegistration> registration_;
  std::map<webrtc::Socket::Option, int> options_;
  int error_ = 0;
  bool closed_ = false;
};

void CloseRegistration(
    const std::shared_ptr<SocketRegistration>& registration) {
  webrtc::Thread* thread;
  {
    std::lock_guard lock(registration->mutex);
    thread = registration->thread;
  }
  if (thread == nullptr) {
    return;
  }
  thread->BlockingCall([registration] {
    SimulatedAsyncPacketSocket* socket;
    {
      std::lock_guard lock(registration->mutex);
      socket = registration->socket;
    }
    if (socket != nullptr) {
      socket->Close();
    }
  });
}

std::unique_ptr<webrtc::AsyncPacketSocket> BindSocket(
    const std::shared_ptr<NativeNetworkEndpoint::State>& endpoint,
    const webrtc::SocketAddress& requested,
    std::uint16_t min_port,
    std::uint16_t max_port) {
  webrtc::Thread* current_thread = webrtc::Thread::Current();
  if (current_thread == nullptr) {
    return nullptr;
  }
  std::lock_guard endpoint_lock(endpoint->mutex);
  if (endpoint->closed) {
    return nullptr;
  }
  webrtc::IPAddress ip = requested.ipaddr();
  if (ip.IsNil()) {
    ip = endpoint->ip;
  }
  if (ip != endpoint->ip) {
    return nullptr;
  }
  std::uint16_t first = requested.port();
  std::uint16_t last = first;
  if (first == 0) {
    first = min_port == 0 ? 49152 : min_port;
    last = max_port < first ? 65535 : max_port;
  }

  auto network = endpoint->network;
  std::lock_guard lock(network->mutex);
  for (std::uint32_t candidate = first; candidate <= last; ++candidate) {
    const webrtc::SocketAddress address(ip, static_cast<int>(candidate));
    const auto found = network->sockets.find(address);
    if (found != network->sockets.end() && !found->second.expired()) {
      continue;
    }
    auto registration = std::make_shared<SocketRegistration>();
    auto socket = std::make_unique<SimulatedAsyncPacketSocket>(
        endpoint, address, registration);
    registration->thread = current_thread;
    registration->socket = socket.get();
    network->sockets[address] = registration;
    endpoint->sockets.push_back(registration);
    return socket;
  }
  return nullptr;
}

class SimulatedAsyncTcpSocket final : public webrtc::AsyncPacketSocket {
 public:
  SimulatedAsyncTcpSocket(
      std::shared_ptr<NativeNetworkEndpoint::State> endpoint,
      webrtc::SocketAddress local,
      webrtc::SocketAddress remote,
      std::shared_ptr<TcpRegistration> registration)
      : endpoint_(std::move(endpoint)), local_(std::move(local)),
        remote_(std::move(remote)), registration_(std::move(registration)) {}

  ~SimulatedAsyncTcpSocket() override {
    Close();
    std::lock_guard lock(registration_->mutex);
    registration_->socket = nullptr;
    registration_->thread = nullptr;
  }

  webrtc::SocketAddress GetLocalAddress() const override { return local_; }
  webrtc::SocketAddress GetRemoteAddress() const override { return remote_; }
  State GetState() const override { return state_; }
  int GetError() const override { return error_; }
  void SetError(int error) override { error_ = error; }
  int GetOption(webrtc::Socket::Option option, int* value) override {
    const auto found = options_.find(option);
    if (found == options_.end() || !value) { error_ = EINVAL; return -1; }
    *value = found->second;
    return 0;
  }
  int SetOption(webrtc::Socket::Option option, int value) override {
    options_[option] = value;
    return 0;
  }

  void QueueConnect() {
    PacketData request;
    request.kind = PacketData::Kind::kTcpConnect;
    Queue(std::move(request));
  }
  int Send(const void* data, std::size_t size,
           const webrtc::AsyncSocketPacketOptions& options) override {
    if (state_ != STATE_CONNECTED || EndpointClosed(endpoint_)) {
      error_ = ENOTCONN;
      return -1;
    }
    PacketData packet;
    packet.kind = PacketData::Kind::kTcpData;
    const auto* bytes = static_cast<const std::uint8_t*>(data);
    packet.payload.assign(bytes, bytes + size);
    const auto timestamp = Queue(std::move(packet));
    NotifySentPacket(this, webrtc::SentPacketInfo(options.packet_id,
                                                   timestamp / 1000));
    return static_cast<int>(size);
  }
  int SendTo(const void* data, std::size_t size,
             const webrtc::SocketAddress& remote,
             const webrtc::AsyncSocketPacketOptions& options) override {
    if (remote != remote_) { error_ = EINVAL; return -1; }
    return Send(data, size, options);
  }
  int Close() override {
    if (state_ == STATE_CLOSED) return 0;
    state_ = STATE_CLOSED;
    const auto network = endpoint_->network;
    {
      std::lock_guard lock(network->mutex);
      const auto found = network->tcp_sockets.find(local_);
      if (found != network->tcp_sockets.end() &&
          found->second.lock() == registration_) {
        network->tcp_sockets.erase(found);
      }
    }
    NotifyClosed(error_);
    return 0;
  }
  void CompleteConnect(bool accepted) {
    if (state_ != STATE_CONNECTING || EndpointClosed(endpoint_)) return;
    if (accepted) {
      state_ = STATE_CONNECTED;
      NotifyConnect(this);
      NotifyReadyToSend(this);
    } else {
      error_ = ECONNREFUSED;
      Close();
    }
  }
  bool Receive(const PacketData& packet) {
    if (state_ != STATE_CONNECTED || EndpointClosed(endpoint_) ||
        packet.source != remote_) return false;
    webrtc::ReceivedIpPacket received(
        std::span<const std::uint8_t>(packet.payload), remote_,
        webrtc::Timestamp::Micros(manual_clock_time_us(*endpoint_->network->clock)));
    NotifyPacketReceived(received);
    return true;
  }

 private:
  std::int64_t Queue(PacketData packet) {
    auto network = endpoint_->network;
    std::lock_guard lock(network->mutex);
    packet.id = network->next_packet_id++;
    packet.source = local_;
    packet.destination = remote_;
    packet.tcp_source = registration_;
    packet.deadline_us = manual_clock_time_us(*network->clock);
    const auto timestamp = packet.deadline_us;
    network->pending.emplace(packet.id, std::move(packet));
    network->outbound.push_back(network->next_packet_id - 1);
    return timestamp;
  }
  std::shared_ptr<NativeNetworkEndpoint::State> endpoint_;
  webrtc::SocketAddress local_;
  webrtc::SocketAddress remote_;
  std::shared_ptr<TcpRegistration> registration_;
  std::map<webrtc::Socket::Option, int> options_;
  State state_ = STATE_CONNECTING;
  int error_ = 0;
};

std::unique_ptr<SimulatedAsyncTcpSocket> ConnectTcpSocket(
    const std::shared_ptr<NativeNetworkEndpoint::State>& endpoint,
    const webrtc::SocketAddress& requested,
    const webrtc::SocketAddress& remote) {
  auto* current_thread = webrtc::Thread::Current();
  if (!current_thread || !remote.IsComplete()) return nullptr;
  std::lock_guard endpoint_lock(endpoint->mutex);
  if (endpoint->closed) return nullptr;
  webrtc::IPAddress ip = requested.ipaddr();
  if (ip.IsNil()) ip = endpoint->ip;
  if (ip != endpoint->ip) return nullptr;
  auto network = endpoint->network;
  std::lock_guard lock(network->mutex);
  const auto first = requested.port() == 0 ? 49152 : requested.port();
  const auto last = requested.port() == 0 ? 65535 : requested.port();
  for (std::uint32_t port = first; port <= last; ++port) {
    const webrtc::SocketAddress local(ip, static_cast<int>(port));
    const auto found = network->tcp_sockets.find(local);
    if (found != network->tcp_sockets.end() && !found->second.expired())
      continue;
    auto registration = std::make_shared<TcpRegistration>();
    auto socket = std::make_unique<SimulatedAsyncTcpSocket>(
        endpoint, local, remote, registration);
    registration->thread = current_thread;
    registration->socket = socket.get();
    network->tcp_sockets[local] = registration;
    endpoint->tcp_sockets.push_back(registration);
    // QueueConnect locks the network. Finish binding before queueing.
    network->thread->PostTask([weak = std::weak_ptr<TcpRegistration>(registration)] {
      if (auto live = weak.lock()) {
        if (live->socket) live->socket->QueueConnect();
      }
    });
    return socket;
  }
  return nullptr;
}

// Adapt the externally delivered TCP byte stream to WebRTC's Socket API so
// its existing SSLAdapter performs TURN/TLS, including certificate checks.
// The adapter never creates an OS socket or an autonomous worker thread.
class SimulatedTcpRawSocket final : public webrtc::Socket {
 public:
  explicit SimulatedTcpRawSocket(std::unique_ptr<SimulatedAsyncTcpSocket> socket)
      : socket_(std::move(socket)) {
    socket_->SubscribeConnect(this, [this](webrtc::AsyncPacketSocket*) {
      NotifyConnectEvent(this);
      NotifyWriteEvent(this);
    });
    socket_->SubscribeReadyToSend(this, [this](webrtc::AsyncPacketSocket*) {
      NotifyWriteEvent(this);
    });
    socket_->SubscribeCloseEvent(this, [this](webrtc::AsyncPacketSocket*, int error) {
      NotifyCloseEvent(this, error);
    });
    socket_->RegisterReceivedPacketCallback(
        [this](webrtc::AsyncPacketSocket*, const webrtc::ReceivedIpPacket& packet) {
          const auto bytes = packet.payload();
          received_.insert(received_.end(), bytes.begin(), bytes.end());
          NotifyReadEvent(this);
        });
  }

  ~SimulatedTcpRawSocket() override {
    socket_->DeregisterReceivedPacketCallback();
    socket_->UnsubscribeConnect(this);
    socket_->UnsubscribeReadyToSend(this);
    socket_->UnsubscribeCloseEvent(this);
    socket_->Close();
  }

  webrtc::SocketAddress GetLocalAddress() const override {
    return socket_->GetLocalAddress();
  }
  webrtc::SocketAddress GetRemoteAddress() const override {
    return socket_->GetRemoteAddress();
  }
  int Bind(const webrtc::SocketAddress&) override { return 0; }
  int Connect(const webrtc::SocketAddress& address) override {
    if (address != socket_->GetRemoteAddress()) {
      socket_->SetError(EINVAL);
      return -1;
    }
    socket_->SetError(EWOULDBLOCK);
    return -1;
  }
  int Send(const void* data, std::size_t size) override {
    return socket_->Send(data, size, webrtc::AsyncSocketPacketOptions());
  }
  int SendTo(const void* data, std::size_t size,
             const webrtc::SocketAddress& address) override {
    return socket_->SendTo(data, size, address,
                           webrtc::AsyncSocketPacketOptions());
  }
  int Recv(void* data, std::size_t size, std::int64_t* timestamp) override {
    if (received_.empty()) {
      if (socket_->GetState() == webrtc::AsyncPacketSocket::STATE_CLOSED)
        return 0;
      socket_->SetError(EWOULDBLOCK);
      return -1;
    }
    const auto count = std::min(size, received_.size());
    auto* bytes = static_cast<std::uint8_t*>(data);
    for (std::size_t index = 0; index < count; ++index) {
      bytes[index] = received_.front();
      received_.pop_front();
    }
    if (timestamp) *timestamp = -1;
    return static_cast<int>(count);
  }
  int Listen(int) override { socket_->SetError(EOPNOTSUPP); return -1; }
  webrtc::Socket* Accept(webrtc::SocketAddress*) override {
    socket_->SetError(EOPNOTSUPP);
    return nullptr;
  }
  int Close() override { return socket_->Close(); }
  int GetError() const override { return socket_->GetError(); }
  void SetError(int error) override { socket_->SetError(error); }
  ConnState GetState() const override {
    switch (socket_->GetState()) {
      case webrtc::AsyncPacketSocket::STATE_CONNECTED: return CS_CONNECTED;
      case webrtc::AsyncPacketSocket::STATE_CONNECTING: return CS_CONNECTING;
      default: return CS_CLOSED;
    }
  }
  int GetOption(Option option, int* value) override {
    return socket_->GetOption(option, value);
  }
  int SetOption(Option option, int value) override {
    return socket_->SetOption(option, value);
  }

 private:
  std::unique_ptr<SimulatedAsyncTcpSocket> socket_;
  std::deque<std::uint8_t> received_;
};

class SimulatedDnsResolver final : public webrtc::AsyncDnsResolverInterface {
 public:
  explicit SimulatedDnsResolver(
      std::shared_ptr<NativeSimulatedNetwork::State> network)
      : state_(std::make_shared<State>(std::move(network))) {}

  void Start(const webrtc::SocketAddress& address,
             absl::AnyInvocable<void()> callback) override {
    Start(address, AF_UNSPEC, std::move(callback));
  }

  void Start(const webrtc::SocketAddress& address,
             int family,
             absl::AnyInvocable<void()> callback) override {
    const auto state = state_;
    state->callback = std::move(callback);
    const auto generation = ++state->generation;
    auto* thread = webrtc::Thread::Current();
    if (!thread) return;
    thread->PostTask([weak = std::weak_ptr<State>(state), address, family,
                      generation] {
      const auto state = weak.lock();
      if (!state || state->generation != generation) return;
      state->result.address = address;
      state->result.resolved.clear();
      if (!address.ipaddr().IsNil()) {
        state->result.resolved.push_back(address.ipaddr());
      } else {
        std::lock_guard lock(state->network->mutex);
        const auto found = state->network->dns_records.find(address.hostname());
        if (found != state->network->dns_records.end()) {
          state->result.resolved = found->second;
        }
      }
      if (family != AF_UNSPEC) {
        std::erase_if(state->result.resolved,
                      [family](const webrtc::IPAddress& ip) {
                        return ip.family() != family;
                      });
      }
      state->result.error = state->result.resolved.empty() ? ENOENT : 0;
      auto completed = std::move(state->callback);
      if (completed) completed();
    });
  }

  const webrtc::AsyncDnsResolverResult& result() const override {
    return state_->result;
  }

 private:
  struct Result final : webrtc::AsyncDnsResolverResult {
    bool GetResolvedAddress(int family,
                            webrtc::SocketAddress* address_out) const override {
      if (error != 0 || !address_out) return false;
      for (const auto& ip : resolved) {
        if (family == AF_UNSPEC || ip.family() == family) {
          *address_out = address;
          address_out->SetResolvedIP(ip);
          return true;
        }
      }
      return false;
    }
    int GetError() const override { return error; }
    webrtc::SocketAddress address;
    std::vector<webrtc::IPAddress> resolved;
    int error = ENOENT;
  };
  struct State {
    explicit State(std::shared_ptr<NativeSimulatedNetwork::State> value)
        : network(std::move(value)) {}
    std::shared_ptr<NativeSimulatedNetwork::State> network;
    Result result;
    absl::AnyInvocable<void()> callback;
    std::uint64_t generation = 0;
  };
  std::shared_ptr<State> state_;
};

class SimulatedPacketSocketFactory final
    : public webrtc::PacketSocketFactory {
 public:
  explicit SimulatedPacketSocketFactory(
      std::shared_ptr<NativeNetworkEndpoint::State> endpoint) noexcept
      : endpoint_(std::move(endpoint)) {}

  std::unique_ptr<webrtc::AsyncPacketSocket> CreateUdpSocket(
      const webrtc::Environment&,
      const webrtc::SocketAddress& address,
      std::uint16_t min_port,
      std::uint16_t max_port) override {
    return BindSocket(endpoint_, address, min_port, max_port);
  }
  std::unique_ptr<webrtc::AsyncListenSocket> CreateServerTcpSocket(
      const webrtc::Environment&,
      const webrtc::SocketAddress&,
      std::uint16_t,
      std::uint16_t,
      int) override {
    return nullptr;
  }
  std::unique_ptr<webrtc::AsyncPacketSocket> CreateClientTcpSocket(
      const webrtc::Environment& env,
      const webrtc::SocketAddress& local,
      const webrtc::SocketAddress& remote,
      const webrtc::PacketSocketTcpOptions& options) override {
    if (endpoint_->network->owned_thread) return nullptr;
    auto socket = ConnectTcpSocket(endpoint_, local, remote);
    if (!socket) return nullptr;
    const int tls = options.opts & (OPT_TLS | OPT_TLS_FAKE | OPT_TLS_INSECURE);
    if ((tls & (tls - 1)) != 0) return nullptr;
    // Plain TCP needs the SDK packet framing too. The simulated socket below
    // is only a byte stream, not an ICE packet socket.
    std::unique_ptr<webrtc::Socket> raw =
        std::make_unique<SimulatedTcpRawSocket>(std::move(socket));
    if (tls == OPT_TLS_FAKE) {
      raw = std::make_unique<webrtc::AsyncSSLSocket>(raw.release());
    } else if (tls != 0) {
      std::unique_ptr<webrtc::SSLAdapter> ssl(webrtc::SSLAdapter::Create(raw.release()));
      if (!ssl) return nullptr;
      if (tls == OPT_TLS_INSECURE) ssl->SetIgnoreBadCert(true);
      ssl->SetAlpnProtocols(options.tls_alpn_protocols);
      ssl->SetEllipticCurves(options.tls_elliptic_curves);
      ssl->SetCertVerifier(options.tls_cert_verifier);
      if (ssl->StartSSL(remote.hostname()) != 0) return nullptr;
      raw = std::move(ssl);
    }
    if (raw->Connect(remote) < 0 && !raw->IsBlocking()) return nullptr;
    if (options.opts & OPT_STUN)
      return std::make_unique<webrtc::AsyncStunTCPSocket>(env, std::move(raw));
    return std::make_unique<webrtc::AsyncTCPSocket>(env, std::move(raw));
  }
  std::unique_ptr<webrtc::AsyncDnsResolverInterface> CreateAsyncDnsResolver()
      override {
    if (endpoint_->network->owned_thread) return nullptr;
    return std::make_unique<SimulatedDnsResolver>(endpoint_->network);
  }
  std::unique_ptr<webrtc::AsyncPacketSocket> CreateClientUdpSocket(
      const webrtc::Environment&,
      const webrtc::SocketAddress&,
      const webrtc::SocketAddress&,
      std::uint16_t,
      std::uint16_t,
      const webrtc::PacketSocketTcpOptions&) override {
    return nullptr;
  }

 private:
  std::shared_ptr<NativeNetworkEndpoint::State> endpoint_;
};

class SimulatedNetworkManager final : public webrtc::NetworkManagerBase {
 public:
  explicit SimulatedNetworkManager(
      std::shared_ptr<NativeNetworkEndpoint::State> endpoint) noexcept
      : endpoint_(std::move(endpoint)) {}

  void StartUpdating() override {
    ++started_;
    if (started_ != 1) {
      NotifyNetworksChanged();
      return;
    }
    const int prefix_length = endpoint_->ip.family() == AF_INET ? 24 : 64;
    auto network = std::make_unique<webrtc::Network>(
        "pulsebeam", "pulsebeam simulated network",
        webrtc::TruncateIP(endpoint_->ip, prefix_length), prefix_length,
        webrtc::ADAPTER_TYPE_ETHERNET);
    network->set_default_local_address_provider(this);
    network->AddIP(endpoint_->ip);
    if (endpoint_->ip.family() == AF_INET) {
      set_default_local_addresses(endpoint_->ip, webrtc::IPAddress());
    } else {
      set_default_local_addresses(webrtc::IPAddress(), endpoint_->ip);
    }
    bool changed = false;
    std::vector<std::unique_ptr<webrtc::Network>> networks;
    networks.push_back(std::move(network));
    MergeNetworkList(std::move(networks), &changed);
    NotifyNetworksChanged();
  }

  void StopUpdating() override {
    if (started_ != 0) {
      --started_;
    }
  }

 private:
  std::shared_ptr<NativeNetworkEndpoint::State> endpoint_;
  std::size_t started_ = 0;
};

void CloseEndpoint(
    const std::shared_ptr<NativeNetworkEndpoint::State>& endpoint) noexcept {
  std::vector<std::shared_ptr<SocketRegistration>> sockets;
  {
    std::lock_guard lock(endpoint->mutex);
    if (endpoint->closed) {
      return;
    }
    endpoint->closed = true;
    for (const auto& weak : endpoint->sockets) {
      if (auto socket = weak.lock()) {
        sockets.push_back(std::move(socket));
      }
    }
    endpoint->sockets.clear();
  }
  for (const auto& socket : sockets) {
    CloseRegistration(socket);
  }
  for (const auto& weak : endpoint->tcp_sockets) {
    if (auto registration = weak.lock()) {
      webrtc::Thread* thread;
      {
        std::lock_guard lock(registration->mutex);
        thread = registration->thread;
      }
      if (thread) thread->BlockingCall([registration] {
        if (registration->socket) registration->socket->Close();
      });
    }
  }
  endpoint->tcp_sockets.clear();
  auto network = endpoint->network;
  std::lock_guard lock(network->mutex);
  network->endpoints.erase(endpoint->ip);
  std::set<std::uint64_t> removed;
  for (auto iterator = network->pending.begin();
       iterator != network->pending.end();) {
    if (iterator->second.source.ipaddr() == endpoint->ip ||
        iterator->second.destination.ipaddr() == endpoint->ip) {
      removed.insert(iterator->first);
      iterator = network->pending.erase(iterator);
    } else {
      ++iterator;
    }
  }
  std::erase_if(network->outbound,
                [&](std::uint64_t id) { return removed.contains(id); });
}

}  // namespace

struct NativeSimulatedUdpSocket::State {
  webrtc::Thread* thread = nullptr;
  std::unique_ptr<webrtc::AsyncPacketSocket> socket;
  std::mutex mutex;
  std::deque<ReceivedData> received;
};

struct NativeOutboundPacket::State {
  explicit State(PacketData packet_value) noexcept
      : packet(std::move(packet_value)) {}
  PacketData packet;
};

struct NativeReceivedPacket::State {
  explicit State(ReceivedData packet_value) noexcept
      : packet(std::move(packet_value)) {}
  ReceivedData packet;
};

NativeSimulatedNetwork::NativeSimulatedNetwork(
    std::shared_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeSimulatedNetwork::~NativeSimulatedNetwork() = default;
const std::shared_ptr<NativeSimulatedNetwork::State>&
NativeSimulatedNetwork::state() const noexcept {
  return state_;
}

NativeNetworkEndpoint::NativeNetworkEndpoint(
    std::shared_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeNetworkEndpoint::~NativeNetworkEndpoint() {
  CloseEndpoint(state_);
}
const std::shared_ptr<NativeNetworkEndpoint::State>& NativeNetworkEndpoint::state()
    const noexcept {
  return state_;
}

NativeNetworkManagerProvider::NativeNetworkManagerProvider(
    std::shared_ptr<NativeNetworkEndpoint::State> endpoint) noexcept
    : endpoint_(std::move(endpoint)) {}
NativeNetworkManagerProvider::~NativeNetworkManagerProvider() = default;
std::unique_ptr<webrtc::NetworkManager> NativeNetworkManagerProvider::Create()
    const noexcept {
  return EndpointClosed(endpoint_)
             ? nullptr
             : std::make_unique<SimulatedNetworkManager>(endpoint_);
}
const std::shared_ptr<NativeNetworkEndpoint::State>&
NativeNetworkManagerProvider::endpoint() const noexcept {
  return endpoint_;
}

NativePacketSocketFactoryProvider::NativePacketSocketFactoryProvider(
    std::shared_ptr<NativeNetworkEndpoint::State> endpoint) noexcept
    : endpoint_(std::move(endpoint)) {}
NativePacketSocketFactoryProvider::~NativePacketSocketFactoryProvider() =
    default;
std::unique_ptr<webrtc::PacketSocketFactory>
NativePacketSocketFactoryProvider::Create() const noexcept {
  return EndpointClosed(endpoint_)
             ? nullptr
             : std::make_unique<SimulatedPacketSocketFactory>(endpoint_);
}
const std::shared_ptr<NativeNetworkEndpoint::State>&
NativePacketSocketFactoryProvider::endpoint() const noexcept {
  return endpoint_;
}

NativeSimulatedUdpSocket::NativeSimulatedUdpSocket(
    std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeSimulatedUdpSocket::~NativeSimulatedUdpSocket() {
  if (state_ && state_->thread) {
    state_->thread->BlockingCall([this] { state_->socket.reset(); });
  }
}
const std::unique_ptr<NativeSimulatedUdpSocket::State>&
NativeSimulatedUdpSocket::state() const noexcept {
  return state_;
}

NativeOutboundPacket::NativeOutboundPacket(
    std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeOutboundPacket::~NativeOutboundPacket() = default;
const std::unique_ptr<NativeOutboundPacket::State>& NativeOutboundPacket::state()
    const noexcept {
  return state_;
}

NativeReceivedPacket::NativeReceivedPacket(
    std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeReceivedPacket::~NativeReceivedPacket() = default;
const std::unique_ptr<NativeReceivedPacket::State>& NativeReceivedPacket::state()
    const noexcept {
  return state_;
}

std::unique_ptr<NativeSimulatedNetwork> new_simulated_network(
    const NativeManualClock& clock) noexcept {
  auto state = std::make_shared<NativeSimulatedNetwork::State>(&clock);
  if (!state->thread || !state->thread->Start()) {
    return nullptr;
  }
  return std::make_unique<NativeSimulatedNetwork>(std::move(state));
}

std::unique_ptr<NativeSimulatedNetwork> new_controlled_simulated_network(
    const NativeManualClock& clock,
    const NativeDriverThread& driver) noexcept {
  if (!driver.thread() || !driver.thread()->IsCurrent() ||
      !driver.uses_clock(clock)) return nullptr;
  return std::make_unique<NativeSimulatedNetwork>(
      std::make_shared<NativeSimulatedNetwork::State>(&clock, driver.thread()));
}

bool add_simulated_dns_record(const NativeSimulatedNetwork& network,
                              rust::Str hostname,
                              rust::Slice<const std::uint8_t> ip_bytes) noexcept {
  const auto ip = ToIp(ip_bytes);
  if (!ip || ip->IsNil() || hostname.empty()) return false;
  const auto& state = network.state();
  if (state->owned_thread || !state->thread->IsCurrent()) return false;
  std::lock_guard lock(state->mutex);
  auto& records = state->dns_records[std::string(hostname)];
  if (std::find(records.begin(), records.end(), *ip) == records.end()) {
    records.push_back(*ip);
  }
  return true;
}

std::unique_ptr<NativeNetworkEndpoint> register_network_endpoint(
    const NativeSimulatedNetwork& network,
    rust::Slice<const std::uint8_t> ip_bytes,
    std::uint8_t& error) noexcept {
  const auto ip = ToIp(ip_bytes);
  if (!ip || ip->IsNil()) {
    error = kInvalidAddress;
    return nullptr;
  }
  const auto& state = network.state();
  std::lock_guard lock(state->mutex);
  const auto found = state->endpoints.find(*ip);
  if (found != state->endpoints.end() && !found->second.expired()) {
    error = kAddressInUse;
    return nullptr;
  }
  auto endpoint =
      std::make_shared<NativeNetworkEndpoint::State>(state, *ip);
  state->endpoints[*ip] = endpoint;
  error = kOk;
  return std::make_unique<NativeNetworkEndpoint>(std::move(endpoint));
}

void close_network_endpoint(NativeNetworkEndpoint& endpoint) noexcept {
  CloseEndpoint(endpoint.state());
}

std::unique_ptr<NativeNetworkManagerProvider> new_network_manager_provider(
    const NativeNetworkEndpoint& endpoint) noexcept {
  if (EndpointClosed(endpoint.state())) {
    return nullptr;
  }
  return std::make_unique<NativeNetworkManagerProvider>(endpoint.state());
}

bool network_manager_provider_is_valid(
    const NativeNetworkManagerProvider& provider) noexcept {
  const auto& endpoint = provider.endpoint();
  if (EndpointClosed(endpoint)) {
    return false;
  }
  bool valid = false;
  endpoint->network->thread->BlockingCall([&] {
    auto manager = provider.Create();
    manager->StartUpdating();
    valid = manager->GetNetworks().size() == 1;
    manager->StopUpdating();
  });
  return valid;
}

std::unique_ptr<NativePacketSocketFactoryProvider>
new_packet_socket_factory_provider(
    const NativeNetworkEndpoint& endpoint) noexcept {
  if (EndpointClosed(endpoint.state())) {
    return nullptr;
  }
  return std::make_unique<NativePacketSocketFactoryProvider>(endpoint.state());
}

bool packet_socket_factory_supports_udp(
    const NativePacketSocketFactoryProvider& provider) noexcept {
  return !EndpointClosed(provider.endpoint());
}
bool packet_socket_factory_supports_tcp(
    const NativePacketSocketFactoryProvider& provider) noexcept {
  const auto& endpoint = provider.endpoint();
  return !EndpointClosed(endpoint) && !endpoint->network->owned_thread;
}
bool packet_socket_factory_supports_dns(
    const NativePacketSocketFactoryProvider& provider) noexcept {
  const auto& endpoint = provider.endpoint();
  return !EndpointClosed(endpoint) && !endpoint->network->owned_thread;
}

std::unique_ptr<NativeSimulatedUdpSocket> create_simulated_udp_socket(
    const NativePacketSocketFactoryProvider& provider,
    std::uint16_t port,
    std::uint8_t& error) noexcept {
  const auto& endpoint = provider.endpoint();
  if (EndpointClosed(endpoint)) {
    error = kEndpointClosed;
    return nullptr;
  }
  auto state = std::make_unique<NativeSimulatedUdpSocket::State>();
  state->thread = endpoint->network->thread;
  state->thread->BlockingCall([&] {
    auto factory = provider.Create();
    webrtc::Environment environment = webrtc::CreateEnvironment();
    state->socket = factory->CreateUdpSocket(
        environment, webrtc::SocketAddress(endpoint->ip, port), 0, 0);
    if (state->socket) {
      auto* received_state = state.get();
      state->socket->RegisterReceivedPacketCallback(
          [received_state](webrtc::AsyncPacketSocket*,
                           const webrtc::ReceivedIpPacket& packet) {
            ReceivedData received{packet.source_address(),
                                  std::vector<std::uint8_t>(
                                      packet.payload().begin(),
                                      packet.payload().end())};
            std::lock_guard lock(received_state->mutex);
            received_state->received.push_back(std::move(received));
          });
    }
  });
  if (!state->socket) {
    error = port == 0 ? kNativeConstructionFailed : kAddressInUse;
    return nullptr;
  }
  error = kOk;
  return std::make_unique<NativeSimulatedUdpSocket>(std::move(state));
}

rust::Vec<std::uint8_t> simulated_udp_local_ip(
    const NativeSimulatedUdpSocket& socket) noexcept {
  webrtc::IPAddress ip;
  socket.state()->thread->BlockingCall(
      [&] { ip = socket.state()->socket->GetLocalAddress().ipaddr(); });
  return FromIp(ip);
}

std::uint16_t simulated_udp_local_port(
    const NativeSimulatedUdpSocket& socket) noexcept {
  std::uint16_t port = 0;
  socket.state()->thread->BlockingCall(
      [&] { port = socket.state()->socket->GetLocalAddress().port(); });
  return port;
}

bool simulated_udp_send_to(const NativeSimulatedUdpSocket& socket,
                           rust::Slice<const std::uint8_t> destination_ip,
                           std::uint16_t destination_port,
                           rust::Vec<std::uint8_t> payload,
                           std::uint8_t& error) noexcept {
  const auto ip = ToIp(destination_ip);
  if (!ip || ip->IsNil()) {
    error = kInvalidAddress;
    return false;
  }
  if (destination_port == 0) {
    error = kInvalidPort;
    return false;
  }
  bool sent = false;
  socket.state()->thread->BlockingCall([&] {
    webrtc::AsyncSocketPacketOptions options;
    sent = socket.state()->socket->SendTo(
               payload.data(), payload.size(),
               webrtc::SocketAddress(*ip, destination_port), options) >= 0;
  });
  error = sent ? kOk : kSocketClosed;
  return sent;
}

std::unique_ptr<NativeReceivedPacket> simulated_udp_take_received(
    const NativeSimulatedUdpSocket& socket) noexcept {
  std::lock_guard lock(socket.state()->mutex);
  if (socket.state()->received.empty()) {
    return nullptr;
  }
  ReceivedData packet = std::move(socket.state()->received.front());
  socket.state()->received.pop_front();
  return std::make_unique<NativeReceivedPacket>(
      std::make_unique<NativeReceivedPacket::State>(std::move(packet)));
}

void close_simulated_udp_socket(NativeSimulatedUdpSocket& socket) noexcept {
  if (!socket.state() || !socket.state()->thread || !socket.state()->socket) {
    return;
  }
  socket.state()->thread->BlockingCall([&] { socket.state()->socket->Close(); });
  std::lock_guard lock(socket.state()->mutex);
  socket.state()->received.clear();
}

std::unique_ptr<NativeOutboundPacket> take_outbound_packet(
    const NativeSimulatedNetwork& network) noexcept {
  const auto& state = network.state();
  std::lock_guard lock(state->mutex);
  while (!state->outbound.empty()) {
    const std::uint64_t id = state->outbound.front();
    state->outbound.pop_front();
    const auto packet = state->pending.find(id);
    if (packet != state->pending.end()) {
      return std::make_unique<NativeOutboundPacket>(
          std::make_unique<NativeOutboundPacket::State>(packet->second));
    }
  }
  return nullptr;
}

std::uint64_t outbound_packet_id(const NativeOutboundPacket& packet) noexcept {
  return packet.state()->packet.id;
}
std::uint8_t outbound_packet_kind(const NativeOutboundPacket& packet) noexcept {
  return static_cast<std::uint8_t>(packet.state()->packet.kind);
}
rust::Vec<std::uint8_t> outbound_packet_source_ip(
    const NativeOutboundPacket& packet) noexcept {
  return FromIp(packet.state()->packet.source.ipaddr());
}
std::uint16_t outbound_packet_source_port(
    const NativeOutboundPacket& packet) noexcept {
  return packet.state()->packet.source.port();
}
rust::Vec<std::uint8_t> outbound_packet_destination_ip(
    const NativeOutboundPacket& packet) noexcept {
  return FromIp(packet.state()->packet.destination.ipaddr());
}
std::uint16_t outbound_packet_destination_port(
    const NativeOutboundPacket& packet) noexcept {
  return packet.state()->packet.destination.port();
}
rust::Vec<std::uint8_t> outbound_packet_payload(
    const NativeOutboundPacket& packet) noexcept {
  return FromBytes(packet.state()->packet.payload);
}
std::int64_t outbound_packet_deadline_us(
    const NativeOutboundPacket& packet) noexcept {
  return packet.state()->packet.deadline_us;
}

bool deliver_outbound_packet(const NativeSimulatedNetwork& network,
                             std::uint64_t packet_id,
                             bool keep_pending,
                             std::uint8_t& error) noexcept {
  PacketData packet;
  std::shared_ptr<SocketRegistration> registration;
  {
    const auto& state = network.state();
    std::lock_guard lock(state->mutex);
    const auto found = state->pending.find(packet_id);
    if (found == state->pending.end()) {
      error = kPacketNotFound;
      return false;
    }
    packet = found->second;
    if (!keep_pending) {
      state->pending.erase(found);
    }
    const auto destination = state->sockets.find(packet.destination);
    if (destination != state->sockets.end()) {
      registration = destination->second.lock();
    }
  }
  if (packet.kind != PacketData::Kind::kUdp) {
    if (keep_pending && packet.kind == PacketData::Kind::kTcpConnect) {
      error = kUnsupportedTransport;
      return false;
    }
    auto source = packet.tcp_source.lock();
    if (!source) { error = kSocketClosed; return false; }
    bool delivered = false;
    source->thread->BlockingCall([&] {
      if (source->socket && source->socket->GetState() !=
                                webrtc::AsyncPacketSocket::STATE_CLOSED) {
        if (packet.kind == PacketData::Kind::kTcpConnect)
          source->socket->CompleteConnect(true);
        delivered = true;
      }
    });
    error = delivered ? kOk : kSocketClosed;
    return delivered;
  }
  if (!registration) {
    error = kDestinationUnavailable;
    return false;
  }
  bool delivered = false;
  webrtc::Thread* thread;
  {
    std::lock_guard lock(registration->mutex);
    thread = registration->thread;
  }
  if (thread != nullptr) {
    thread->BlockingCall([&] {
      SimulatedAsyncPacketSocket* socket;
      {
        std::lock_guard lock(registration->mutex);
        socket = registration->socket;
      }
      delivered = socket != nullptr && socket->Receive(packet);
    });
  }
  error = delivered ? kOk : kDestinationUnavailable;
  return delivered;
}

bool drop_outbound_packet(const NativeSimulatedNetwork& network,
                          std::uint64_t packet_id,
                          std::uint8_t& error) noexcept {
  const auto& state = network.state();
  std::shared_ptr<TcpRegistration> tcp;
  {
    std::lock_guard lock(state->mutex);
    const auto found = state->pending.find(packet_id);
    if (found == state->pending.end()) {
      error = kPacketNotFound;
      return false;
    }
    if (found->second.kind == PacketData::Kind::kTcpConnect)
      tcp = found->second.tcp_source.lock();
    state->pending.erase(found);
  }
  if (tcp && tcp->thread) {
    tcp->thread->BlockingCall([&] {
      if (tcp->socket) tcp->socket->CompleteConnect(false);
    });
  }
  error = kOk;
  return true;
}

bool inject_simulated_tcp_data(const NativeSimulatedNetwork& network,
                               rust::Slice<const std::uint8_t> source_ip,
                               std::uint16_t source_port,
                               rust::Slice<const std::uint8_t> destination_ip,
                               std::uint16_t destination_port,
                               rust::Slice<const std::uint8_t> data,
                               std::uint8_t& error) noexcept {
  const auto source = ToIp(source_ip);
  const auto destination = ToIp(destination_ip);
  if (!source || !destination || source->IsNil() || destination->IsNil()) {
    error = kInvalidAddress;
    return false;
  }
  if (!source_port || !destination_port) {
    error = kInvalidPort;
    return false;
  }
  PacketData packet;
  packet.source = webrtc::SocketAddress(*source, source_port);
  packet.destination = webrtc::SocketAddress(*destination, destination_port);
  packet.payload.assign(data.begin(), data.end());
  std::shared_ptr<TcpRegistration> registration;
  {
    std::lock_guard lock(network.state()->mutex);
    const auto found = network.state()->tcp_sockets.find(packet.destination);
    if (found != network.state()->tcp_sockets.end())
      registration = found->second.lock();
  }
  if (!registration || !registration->thread) {
    error = kDestinationUnavailable;
    return false;
  }
  bool received = false;
  registration->thread->BlockingCall([&] {
    if (registration->socket) received = registration->socket->Receive(packet);
  });
  error = received ? kOk : kDestinationUnavailable;
  return received;
}

rust::Vec<std::uint8_t> received_packet_source_ip(
    const NativeReceivedPacket& packet) noexcept {
  return FromIp(packet.state()->packet.source.ipaddr());
}
std::uint16_t received_packet_source_port(
    const NativeReceivedPacket& packet) noexcept {
  return packet.state()->packet.source.port();
}
rust::Vec<std::uint8_t> received_packet_payload(
    const NativeReceivedPacket& packet) noexcept {
  return FromBytes(packet.state()->packet.payload);
}
}  // namespace pulsebeam::webrtc_sys
