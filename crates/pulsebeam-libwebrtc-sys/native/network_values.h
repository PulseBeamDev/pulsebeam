#pragma once
#include <cstdint>
#include <memory>
#include <vector>
#include "rust/cxx.h"

namespace webrtc { class Network; class NetworkMask; class DefaultLocalAddressProvider; class MdnsResponderProvider; class MdnsResponderInterface; class NetworkManager; }
namespace pulsebeam::webrtc_sys {
class NativeFieldTrials;
struct RustMdnsResponder;
struct RustMdnsCallback;
class NativeMdnsResponder;
class NativeMdnsCompletion final {
 public:
  struct State;
  explicit NativeMdnsCompletion(std::shared_ptr<State> state) noexcept;
  std::shared_ptr<State> state;
};
struct RustManagerCallback;
class NativeNetworkResources;
class NativeManagerSubscription final {
 public:
  struct State;
  NativeManagerSubscription(std::shared_ptr<NativeNetworkResources> resources, std::shared_ptr<State> state) noexcept;
  ~NativeManagerSubscription();
  bool Cancel() const noexcept;
  bool Active() const noexcept;
 private:
  std::shared_ptr<NativeNetworkResources> resources_;
  std::shared_ptr<State> state_;
};
class NativeNetworkResources final {
 public:
  NativeNetworkResources() noexcept;
  ~NativeNetworkResources();
  void Retire() noexcept;
  bool retired() const noexcept;
  void Retain(std::shared_ptr<NativeMdnsResponder> responder) noexcept;
  void Prune() noexcept;
  void BindManager(webrtc::NetworkManager* manager) noexcept;
  bool ManagerReady() const noexcept;
  bool DeliveringManager(std::uint8_t mask) const noexcept;
  std::uint8_t NotifyManager(std::uint8_t kind) noexcept;
  void AddObserver(std::shared_ptr<NativeManagerSubscription::State> state) noexcept;
  bool RemoveObserver(const std::shared_ptr<NativeManagerSubscription::State>& state) noexcept;
 private:
  void AttachObserver(const std::shared_ptr<NativeManagerSubscription::State>& state) noexcept;
  bool retired_ = false;
  webrtc::NetworkManager* manager_ = nullptr;
  std::uint8_t delivering_manager_ = 0;
  std::vector<std::weak_ptr<NativeManagerSubscription::State>> observers_;
  std::vector<std::shared_ptr<void>> cancelled_manager_callbacks_;
  std::vector<std::weak_ptr<NativeMdnsResponder>> responders_;
};
std::unique_ptr<NativeManagerSubscription> manager_observe(std::shared_ptr<NativeNetworkResources> resources, std::uint8_t kind, rust::Box<RustManagerCallback> callback) noexcept;
bool manager_subscription_cancel(const NativeManagerSubscription& subscription) noexcept;
bool manager_subscription_active(const NativeManagerSubscription& subscription) noexcept;
bool manager_observation_retired(const NativeNetworkResources& resources) noexcept;
bool manager_observation_ready(const NativeNetworkResources& resources) noexcept;
std::uint8_t manager_observation_notify(const NativeNetworkResources& resources, std::uint8_t kind) noexcept;

class NativeMdnsResponder final {
 public:
  struct State;
  NativeMdnsResponder(std::weak_ptr<NativeNetworkResources> resources, rust::Box<RustMdnsResponder> source) noexcept;
  ~NativeMdnsResponder();
  void Retire() noexcept;
  const webrtc::MdnsResponderProvider& provider() const noexcept;
  webrtc::MdnsResponderInterface* responder() const noexcept;
  const std::shared_ptr<State>& state() const noexcept;
 private:
  std::shared_ptr<State> state_;
};
std::shared_ptr<NativeMdnsResponder> mdns_responder_new(std::shared_ptr<NativeNetworkResources> resources, rust::Box<RustMdnsResponder> source) noexcept;
bool mdns_responder_retired(const NativeMdnsResponder& responder) noexcept;
bool mdns_responder_same(const NativeMdnsResponder& first, const NativeMdnsResponder& second) noexcept;
bool mdns_responder_failed(const NativeMdnsResponder& responder) noexcept;
std::uint8_t mdns_responder_request(const NativeMdnsResponder& responder, rust::Slice<const std::uint8_t> ip, bool remove, rust::Box<RustMdnsCallback> callback) noexcept;
std::uint8_t mdns_complete(const NativeMdnsCompletion& completion, rust::Slice<const std::uint8_t> ip, rust::Slice<const std::uint8_t> name, bool removed) noexcept;
bool mdns_cancel(const NativeMdnsCompletion& completion) noexcept;
struct FfiInterfaceAddress;
struct FfiNetworkCostGuess;
struct FfiDefaultLocalAddress;
struct RustDefaultLocalAddressProvider;
class NativeDefaultLocalAddressProvider final {
 public:
  struct State;
  explicit NativeDefaultLocalAddressProvider(rust::Box<RustDefaultLocalAddressProvider> provider) noexcept;
  ~NativeDefaultLocalAddressProvider();
  const webrtc::DefaultLocalAddressProvider& provider() const noexcept;
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::shared_ptr<NativeDefaultLocalAddressProvider> default_address_provider_new(rust::Box<RustDefaultLocalAddressProvider> provider) noexcept;
FfiDefaultLocalAddress default_address_provider_get(const NativeDefaultLocalAddressProvider& provider, std::int32_t family) noexcept;
bool default_address_provider_failed(const NativeDefaultLocalAddressProvider& provider) noexcept;

// Stable SDK allocation. Sharing a handle never copies a native Network;
// Rust handles remain sequence-bound.
class NativeNetwork final : public std::enable_shared_from_this<NativeNetwork> {
 public:
  explicit NativeNetwork(std::unique_ptr<webrtc::Network> network) noexcept;
  ~NativeNetwork();
  webrtc::Network& network() const noexcept;
  void SetDefaultProvider(std::shared_ptr<NativeDefaultLocalAddressProvider> provider) const noexcept;
  std::shared_ptr<NativeDefaultLocalAddressProvider> GetDefaultProvider() const noexcept;
  void SetMdnsProvider(std::shared_ptr<NativeMdnsResponder> provider) const noexcept;
  std::shared_ptr<NativeMdnsResponder> GetMdnsProvider() const noexcept;
  bool Delivering(std::uint8_t mask) const noexcept;
  void BeginNotification(std::uint8_t mask) const noexcept;
  void EndNotification(std::uint8_t mask) const noexcept;
  void RetainCancelledCallback(std::shared_ptr<void> callback) const noexcept;
 private:
  mutable std::uint8_t delivering_ = 0;
  mutable std::vector<std::shared_ptr<void>> cancelled_callbacks_;
  // The SDK network must be destroyed before the provider its raw link uses.
  mutable std::shared_ptr<NativeDefaultLocalAddressProvider> default_provider_;
  mutable std::shared_ptr<NativeMdnsResponder> mdns_provider_;
  std::unique_ptr<webrtc::Network> network_;
};
struct RustNetworkCallback;
class NativeNetworkSubscription final {
 public:
  struct State;
  NativeNetworkSubscription(std::shared_ptr<NativeNetwork> network, std::shared_ptr<State> state) noexcept;
  ~NativeNetworkSubscription();
  bool Cancel() const noexcept;
  bool Failed() const noexcept;
 private:
  std::shared_ptr<NativeNetwork> network_;
  std::shared_ptr<State> state_;
};
std::unique_ptr<NativeNetworkSubscription> network_value_subscribe(std::shared_ptr<NativeNetwork> network, std::uint8_t kind, rust::Box<RustNetworkCallback> callback) noexcept;
bool network_subscription_cancel(const NativeNetworkSubscription& subscription) noexcept;
bool network_subscription_failed(const NativeNetworkSubscription& subscription) noexcept;

class NativeNetworkMask final {
 public:
  explicit NativeNetworkMask(std::unique_ptr<webrtc::NetworkMask> mask) noexcept;
  ~NativeNetworkMask();
  const webrtc::NetworkMask& mask() const noexcept;
 private:
  std::unique_ptr<webrtc::NetworkMask> mask_;
};
std::shared_ptr<NativeNetwork> network_value_new(rust::Slice<const std::uint8_t> name, rust::Slice<const std::uint8_t> description, rust::Slice<const std::uint8_t> prefix, std::int32_t prefix_length, std::int32_t adapter) noexcept;
std::shared_ptr<NativeNetwork> network_value_clone(const NativeNetwork& network) noexcept;
void network_value_set_default_provider(const NativeNetwork& network, std::shared_ptr<NativeDefaultLocalAddressProvider> provider) noexcept;
std::shared_ptr<NativeDefaultLocalAddressProvider> network_value_default_provider(const NativeNetwork& network) noexcept;
void network_value_set_mdns(const NativeNetwork& network, std::shared_ptr<NativeMdnsResponder> provider) noexcept;
std::shared_ptr<NativeMdnsResponder> network_value_mdns(const NativeNetwork& network) noexcept;
bool network_value_same(const NativeNetwork& first, const NativeNetwork& second) noexcept;
rust::Vec<std::uint8_t> network_value_bytes(const NativeNetwork& network, std::uint8_t field) noexcept;
std::int32_t network_value_info(const NativeNetwork& network, std::uint8_t field) noexcept;
bool network_value_set(const NativeNetwork& network, std::uint8_t field, std::int32_t value) noexcept;
bool network_value_notify(const NativeNetwork& network, std::uint8_t kind) noexcept;
void network_value_add_ip(const NativeNetwork& network, rust::Slice<const std::uint8_t> ip, std::int32_t flags) noexcept;
rust::Vec<FfiInterfaceAddress> network_value_ips(const NativeNetwork& network) noexcept;
bool network_value_set_ips(const NativeNetwork& network, rust::Slice<const FfiInterfaceAddress> ips, bool already_changed) noexcept;
void network_value_clear_ips(const NativeNetwork& network) noexcept;
std::int32_t network_value_cost(const NativeNetwork& network, const NativeFieldTrials& trials) noexcept;
FfiNetworkCostGuess network_guess_cost(std::int32_t cost) noexcept;
bool network_adapter_is_cellular(std::int32_t adapter) noexcept;
rust::Vec<std::uint8_t> network_make_key(rust::Slice<const std::uint8_t> name, rust::Slice<const std::uint8_t> prefix, std::int32_t prefix_length) noexcept;
std::int32_t network_adapter_from_name(rust::Slice<const std::uint8_t> name) noexcept;
std::unique_ptr<NativeNetworkMask> network_mask_new(rust::Slice<const std::uint8_t> prefix, std::int32_t length) noexcept;
rust::Vec<std::uint8_t> network_mask_address(const NativeNetworkMask& mask) noexcept;
std::int32_t network_mask_prefix_length(const NativeNetworkMask& mask) noexcept;
bool network_mask_equal(const NativeNetworkMask& first, const NativeNetworkMask& second) noexcept;
}  // namespace pulsebeam::webrtc_sys
