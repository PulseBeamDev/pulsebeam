#include "pulsebeam-webrtc-sys/native/network_values.h"
#include <cstring>
#include <cstdio>
#include <limits>
#include <algorithm>
#include <mutex>
#include <optional>
#include "rtc_base/mdns_responder_interface.h"
#include "rtc_base/thread.h"
#include <utility>
#include <vector>
#include "pulsebeam-webrtc-sys/native/execution.h"  // IWYU pragma: keep
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "rtc_base/network.h"

namespace pulsebeam::webrtc_sys {
namespace {
webrtc::IPAddress Ip(rust::Slice<const std::uint8_t> bytes) {
  if (bytes.size() == 4) { in_addr value{}; std::memcpy(&value, bytes.data(), 4); return webrtc::IPAddress(value); }
  if (bytes.size() == 16) { in6_addr value{}; std::memcpy(&value, bytes.data(), 16); return webrtc::IPAddress(value); }
  return {};
}
rust::Vec<std::uint8_t> Bytes(const webrtc::IPAddress& ip) {
  rust::Vec<std::uint8_t> result;
  if (ip.family() == AF_INET) {
    auto value = ip.ipv4_address(); auto* data = reinterpret_cast<const std::uint8_t*>(&value);
    for (std::size_t i = 0; i < 4; ++i) result.push_back(data[i]);
  } else if (ip.family() == AF_INET6) {
    auto value = ip.ipv6_address(); auto* data = reinterpret_cast<const std::uint8_t*>(&value);
    for (std::size_t i = 0; i < 16; ++i) result.push_back(data[i]);
  }
  return result;
}
absl::string_view View(rust::Slice<const std::uint8_t> bytes) { return {reinterpret_cast<const char*>(bytes.data()), bytes.size()}; }
rust::Vec<std::uint8_t> Bytes(absl::string_view text) {
  rust::Vec<std::uint8_t> result;
  for (unsigned char byte : text) result.push_back(byte);
  return result;
}
}  // namespace
struct NativeDefaultLocalAddressProvider::State final : webrtc::DefaultLocalAddressProvider {
  explicit State(rust::Box<RustDefaultLocalAddressProvider> value) : source(std::move(value)) {}
  bool GetDefaultLocalAddress(int family, webrtc::IPAddress* address) const override {
    auto result = default_address_get(*source, family);
    if (!result.present) return false;
    *address = Ip({result.ip.data(), result.ip.size()});
    return true;
  }
  rust::Box<RustDefaultLocalAddressProvider> source;
};
NativeDefaultLocalAddressProvider::NativeDefaultLocalAddressProvider(rust::Box<RustDefaultLocalAddressProvider> provider) noexcept
    : state_(std::make_unique<State>(std::move(provider))) {}
NativeDefaultLocalAddressProvider::~NativeDefaultLocalAddressProvider() = default;
const webrtc::DefaultLocalAddressProvider& NativeDefaultLocalAddressProvider::provider() const noexcept { return *state_; }
const std::unique_ptr<NativeDefaultLocalAddressProvider::State>& NativeDefaultLocalAddressProvider::state() const noexcept { return state_; }
std::shared_ptr<NativeDefaultLocalAddressProvider> default_address_provider_new(rust::Box<RustDefaultLocalAddressProvider> source) noexcept {
  return std::make_shared<NativeDefaultLocalAddressProvider>(std::move(source));
}
FfiDefaultLocalAddress default_address_provider_get(const NativeDefaultLocalAddressProvider& value, std::int32_t family) noexcept {
  webrtc::IPAddress address;
  const bool present = value.provider().GetDefaultLocalAddress(family, &address);
  return {present, Bytes(address)};
}
bool default_address_provider_failed(const NativeDefaultLocalAddressProvider& value) noexcept { return default_address_failed(*value.state()->source); }

// Pending native callbacks are owned by the manager's network-role scope, not
// by completion aliases. All completion/cancellation submissions synchronize
// with retirement before touching the role's raw dispatch pointer.
struct NativeMdnsCompletion::State {
  using Created = webrtc::MdnsResponderInterface::NameCreatedCallback;
  using Removed = webrtc::MdnsResponderInterface::NameRemovedCallback;
  State(webrtc::IPAddress address, Created callback)
      : thread(webrtc::Thread::Current()), address(address), created(std::move(callback)) {}
  State(webrtc::IPAddress address, Removed callback)
      : thread(webrtc::Thread::Current()), address(address), removed(std::move(callback)), removal(true) {}
  void Cancel() {
    Created old_created;
    Removed old_removed;
    { std::lock_guard lock(mutex); thread = nullptr; old_created = std::move(created); old_removed = std::move(removed); }
    if (on_done) on_done();
    // Dropping a callback can reenter the provider. Never run its destructor
    // while holding the submission/retirement mutex.
  }
  bool Live() { std::lock_guard lock(mutex); return thread != nullptr; }
  void Run(std::string name, bool removed_value, bool cancel) {
    Created pending_created;
    Removed pending_removed;
    { std::lock_guard lock(mutex);
      if (!thread) return;
      thread = nullptr;
      pending_created = std::move(created); pending_removed = std::move(removed);
    }
    if (!cancel) {
      if (pending_created) pending_created(address, name);
      else if (pending_removed) pending_removed(removed_value);
    }
    if (on_done) on_done();
    // No facade or role pointer is used after entering the callback frame.
  }
  std::mutex mutex;
  webrtc::Thread* thread;
  const webrtc::IPAddress address;
  Created created;
  Removed removed;
  bool removal = false;
  bool submitted = false;
  std::function<void()> on_done;
};
NativeMdnsCompletion::NativeMdnsCompletion(std::shared_ptr<State> value) noexcept : state(std::move(value)) {}
std::uint8_t mdns_complete(const NativeMdnsCompletion& completion, rust::Slice<const std::uint8_t> ip, rust::Slice<const std::uint8_t> name, bool removed) noexcept {
  auto state = completion.state;
  std::lock_guard lock(state->mutex);
  if (!state->thread || state->submitted) return 1;
  // Port's callback DCHECK requires the requested address, not an arbitrary
  // replacement. Invalid completion input is rejected without consuming it.
  if (!state->removal && Ip(ip) != state->address) return 2;
  state->submitted = true;
  std::string owned_name(name.empty() ? "" : reinterpret_cast<const char*>(name.data()), name.size());
  state->thread->PostTask([state, owned_name = std::move(owned_name), removed]() mutable { state->Run(std::move(owned_name), removed, false); });
  return 0;
}
bool mdns_cancel(const NativeMdnsCompletion& completion) noexcept {
  auto state = completion.state;
  std::lock_guard lock(state->mutex);
  if (!state->thread || state->submitted) return false;
  state->submitted = true;
  state->thread->PostTask([state] { state->Run({}, false, true); });
  return true;
}

struct NativeMdnsResponder::State final : webrtc::MdnsResponderInterface, webrtc::MdnsResponderProvider,
                                         std::enable_shared_from_this<NativeMdnsResponder::State> {
  State(std::weak_ptr<NativeNetworkResources> scope, rust::Box<RustMdnsResponder> provider)
      : resources(std::move(scope)), source(std::make_shared<rust::Box<RustMdnsResponder>>(std::move(provider))) {}
  webrtc::MdnsResponderInterface* GetMdnsResponder() const override {
    // A selected provider remains selected even after failure/retirement:
    // null here would let a Network retained by another live consumer silently
    // publish the unobfuscated address. Retired operations admit no new work.
    return const_cast<State*>(this);
  }
  void CreateNameForAddress(const webrtc::IPAddress& address, NameCreatedCallback callback) override {
    auto lease = shared_from_this();
    auto provider = source;
    if (retired || !provider) return;
    Prune();
    auto request = std::make_shared<NativeMdnsCompletion::State>(address, std::move(callback));
    request->on_done = [weak = weak_from_this()] { if (auto owner = weak.lock()) owner->Prune(); };
    requests.push_back(request);
    if (!mdns_source_create(**provider, Bytes(address), std::make_shared<NativeMdnsCompletion>(request))) request->Cancel();
  }
  void RemoveNameForAddress(const webrtc::IPAddress& address, NameRemovedCallback callback) override {
    auto lease = shared_from_this();
    auto provider = source;
    if (retired || !provider) return;
    Prune();
    auto request = std::make_shared<NativeMdnsCompletion::State>(address, std::move(callback));
    request->on_done = [weak = weak_from_this()] { if (auto owner = weak.lock()) owner->Prune(); };
    requests.push_back(request);
    if (!mdns_source_remove(**provider, Bytes(address), std::make_shared<NativeMdnsCompletion>(request))) request->Cancel();
  }
  void Prune() { std::erase_if(requests, [](const auto& request) { return !request->Live(); }); }
  void Retire() {
    if (retired) return;
    retired = true;
    auto pending = std::move(requests);
    auto provider = source;
    for (const auto& request : pending) request->Cancel();
    if (provider) mdns_source_retire(**provider);
    // Provider and callback destructors run here on the network role. Active
    // provider frames retain their own immutable source lease across reentry.
  }
  std::weak_ptr<NativeNetworkResources> resources;
  const std::shared_ptr<rust::Box<RustMdnsResponder>> source;
  std::vector<std::shared_ptr<NativeMdnsCompletion::State>> requests;
  bool retired = false;
};
struct NativeManagerSubscription::State {
  State(std::uint8_t selected_kind, rust::Box<RustManagerCallback> source)
      : kind(selected_kind), callback(std::make_shared<rust::Box<RustManagerCallback>>(std::move(source))) {}
  const std::uint8_t kind;
  bool active = true;
  bool bound = false;
  std::shared_ptr<rust::Box<RustManagerCallback>> callback;
};
NativeManagerSubscription::NativeManagerSubscription(std::shared_ptr<NativeNetworkResources> resources, std::shared_ptr<State> state) noexcept
    : resources_(std::move(resources)), state_(std::move(state)) {}
NativeManagerSubscription::~NativeManagerSubscription() { Cancel(); }
bool NativeManagerSubscription::Cancel() const noexcept { return resources_->RemoveObserver(state_); }
bool NativeManagerSubscription::Active() const noexcept { return state_->active; }
NativeNetworkResources::NativeNetworkResources() noexcept = default;
NativeNetworkResources::~NativeNetworkResources() = default;
void NativeNetworkResources::AttachObserver(const std::shared_ptr<NativeManagerSubscription::State>& state) noexcept {
  if (!manager_ || !state->active || state->bound) return;
  state->bound = true;
  auto deliver = [state] {
    auto callback = state->callback;
    if (!state->active || !callback) return;
    manager_callback_fire(**callback);
  };
  if (state->kind == 0) manager_->SubscribeNetworksChanged(state.get(), std::move(deliver));
  else manager_->SubscribeError(state.get(), std::move(deliver));
}
void NativeNetworkResources::BindManager(webrtc::NetworkManager* manager) noexcept {
  if (retired_) return;
  manager_ = manager;
  for (const auto& weak : observers_) if (auto state = weak.lock()) AttachObserver(state);
}
bool NativeNetworkResources::ManagerReady() const noexcept { return !retired_ && manager_; }
bool NativeNetworkResources::DeliveringManager(std::uint8_t mask) const noexcept { return (delivering_manager_ & mask) != 0; }
void NativeNetworkResources::AddObserver(std::shared_ptr<NativeManagerSubscription::State> state) noexcept {
  Prune();
  observers_.push_back(state);
  AttachObserver(state);
}
bool NativeNetworkResources::RemoveObserver(const std::shared_ptr<NativeManagerSubscription::State>& state) noexcept {
  if (!state->active) return false;
  state->active = false;
  auto callback = std::move(state->callback);
  // The SDK removes tombstoned receivers at the end of Send. Delay captured
  // Rust destructors until that cleanup has finished, including sibling cancel.
  if (callback && delivering_manager_) cancelled_manager_callbacks_.push_back(callback);
  if (manager_ && state->bound) {
    state->bound = false;
    if (state->kind == 0) manager_->UnsubscribeNetworksChanged(state.get());
    else manager_->UnsubscribeError(state.get());
  }
  Prune();
  return true;
}
std::uint8_t NativeNetworkResources::NotifyManager(std::uint8_t kind) noexcept {
  if (retired_) return 1;
  if (!manager_) return 3;
  const auto mask = static_cast<std::uint8_t>(1 << kind);
  if (DeliveringManager(mask)) return 2;
  delivering_manager_ |= mask;
  if (kind == 0) manager_->NotifyNetworksChanged(); else manager_->NotifyError();
  delivering_manager_ &= static_cast<std::uint8_t>(~mask);
  if (!delivering_manager_) {
    auto retired = std::move(cancelled_manager_callbacks_);
    cancelled_manager_callbacks_.clear();
  }
  return 0;
}
std::unique_ptr<NativeManagerSubscription> manager_observe(std::shared_ptr<NativeNetworkResources> resources, std::uint8_t kind, rust::Box<RustManagerCallback> callback) noexcept {
  if (!resources || resources->retired() || kind > 1 || resources->DeliveringManager(static_cast<std::uint8_t>(1 << kind))) return nullptr;
  auto state = std::make_shared<NativeManagerSubscription::State>(kind, std::move(callback));
  resources->AddObserver(state);
  return std::make_unique<NativeManagerSubscription>(std::move(resources), std::move(state));
}
bool manager_subscription_cancel(const NativeManagerSubscription& subscription) noexcept { return subscription.Cancel(); }
bool manager_subscription_active(const NativeManagerSubscription& subscription) noexcept { return subscription.Active(); }
bool manager_observation_retired(const NativeNetworkResources& resources) noexcept { return resources.retired(); }
bool manager_observation_ready(const NativeNetworkResources& resources) noexcept { return resources.ManagerReady(); }
std::uint8_t manager_observation_notify(const NativeNetworkResources& resources, std::uint8_t kind) noexcept {
  return const_cast<NativeNetworkResources&>(resources).NotifyManager(kind);
}
void NativeNetworkResources::Retire() noexcept {
  if (retired_) return;
  retired_ = true;
  auto observers = std::move(observers_);
  for (const auto& weak : observers) if (auto state = weak.lock()) RemoveObserver(state);
  manager_ = nullptr;
  auto responders = std::move(responders_);
  for (const auto& weak : responders) if (auto responder = weak.lock()) responder->Retire();
}
bool NativeNetworkResources::retired() const noexcept { return retired_; }
void NativeNetworkResources::Retain(std::shared_ptr<NativeMdnsResponder> responder) noexcept { Prune(); responders_.push_back(responder); }
void NativeNetworkResources::Prune() noexcept {
  std::erase_if(responders_, [](const auto& weak) { return weak.expired(); });
  std::erase_if(observers_, [](const auto& weak) { auto state = weak.lock(); return !state || !state->active; });
}
NativeMdnsResponder::NativeMdnsResponder(std::weak_ptr<NativeNetworkResources> resources, rust::Box<RustMdnsResponder> source) noexcept
    : state_(std::make_shared<State>(std::move(resources), std::move(source))) {}
NativeMdnsResponder::~NativeMdnsResponder() {
  auto resources = state_->resources.lock();
  state_->Retire();
  if (resources) resources->Prune();
}
void NativeMdnsResponder::Retire() noexcept { state_->Retire(); }
const webrtc::MdnsResponderProvider& NativeMdnsResponder::provider() const noexcept { return *state_; }
webrtc::MdnsResponderInterface* NativeMdnsResponder::responder() const noexcept { return state_->GetMdnsResponder(); }
const std::shared_ptr<NativeMdnsResponder::State>& NativeMdnsResponder::state() const noexcept { return state_; }
std::shared_ptr<NativeMdnsResponder> mdns_responder_new(std::shared_ptr<NativeNetworkResources> resources, rust::Box<RustMdnsResponder> source) noexcept {
  if (!resources || resources->retired()) return {};
  auto responder = std::make_shared<NativeMdnsResponder>(resources, std::move(source));
  resources->Retain(responder);
  return responder;
}
bool mdns_responder_retired(const NativeMdnsResponder& responder) noexcept { return responder.state()->retired; }
bool mdns_responder_same(const NativeMdnsResponder& first, const NativeMdnsResponder& second) noexcept { return first.state().get() == second.state().get(); }
bool mdns_responder_failed(const NativeMdnsResponder& responder) noexcept {
  auto state = responder.state();
  auto provider = state->source;
  return mdns_source_any_failed(**provider);
}
std::uint8_t mdns_responder_request(const NativeMdnsResponder& responder, rust::Slice<const std::uint8_t> ip, bool remove, rust::Box<RustMdnsCallback> callback) noexcept {
  auto state = responder.state();
  if (state->source) mdns_callback_bind(*callback, **state->source);
  if (state->retired) return 1;
  auto provider = state->source;
  if (!provider || mdns_source_failed(**provider)) return 2;
  auto owned = std::make_shared<rust::Box<RustMdnsCallback>>(std::move(callback));
  if (remove) state->RemoveNameForAddress(Ip(ip), [owned](bool removed) { mdns_callback_fire(**owned, {}, {}, removed); });
  else state->CreateNameForAddress(Ip(ip), [owned](const webrtc::IPAddress& address, absl::string_view name) { mdns_callback_fire(**owned, Bytes(address), Bytes(name), false); });
  return mdns_source_failed(**provider) ? 2 : 0;
}

NativeNetwork::NativeNetwork(std::unique_ptr<webrtc::Network> network) noexcept : network_(std::move(network)) {}
NativeNetwork::~NativeNetwork() = default;
bool NativeNetwork::Delivering(std::uint8_t mask) const noexcept { return (delivering_ & mask) != 0; }
void NativeNetwork::BeginNotification(std::uint8_t mask) const noexcept { delivering_ |= mask; }
void NativeNetwork::EndNotification(std::uint8_t mask) const noexcept {
  delivering_ &= static_cast<std::uint8_t>(~mask);
  if (!delivering_) {
    // Receiver removal while Send is active is deferred by the SDK. Hold Rust
    // closures until its receiver-vector cleanup has completed, so closure
    // destructors can safely reenter registration and native setters.
    auto retired = std::move(cancelled_callbacks_);
    cancelled_callbacks_.clear();
  }
}
void NativeNetwork::RetainCancelledCallback(std::shared_ptr<void> callback) const noexcept { cancelled_callbacks_.push_back(std::move(callback)); }
webrtc::Network& NativeNetwork::network() const noexcept { return *network_; }
void NativeNetwork::SetDefaultProvider(std::shared_ptr<NativeDefaultLocalAddressProvider> provider) const noexcept {
  network_->set_default_local_address_provider(provider ? &provider->provider() : nullptr);
  default_provider_ = std::move(provider);
}
std::shared_ptr<NativeDefaultLocalAddressProvider> NativeNetwork::GetDefaultProvider() const noexcept {
  const auto* actual = network_->default_local_address_provider();
  // This owner is returned only for the real native link, not for a cached
  // configuration choice. Managed native links need their manager's owner.
  if (!actual || !default_provider_ || actual != &default_provider_->provider()) return {};
  return default_provider_;
}
void NativeNetwork::SetMdnsProvider(std::shared_ptr<NativeMdnsResponder> provider) const noexcept {
  network_->set_mdns_responder_provider(provider ? &provider->provider() : nullptr);
  mdns_provider_ = std::move(provider);
}
std::shared_ptr<NativeMdnsResponder> NativeNetwork::GetMdnsProvider() const noexcept {
  auto* actual = network_->GetMdnsResponder();
  if (!actual || !mdns_provider_ || actual != mdns_provider_->responder()) return {};
  return mdns_provider_;
}
void network_value_set_mdns(const NativeNetwork& network, std::shared_ptr<NativeMdnsResponder> provider) noexcept { network.SetMdnsProvider(std::move(provider)); }
std::shared_ptr<NativeMdnsResponder> network_value_mdns(const NativeNetwork& network) noexcept { return network.GetMdnsProvider(); }
void network_value_set_default_provider(const NativeNetwork& network, std::shared_ptr<NativeDefaultLocalAddressProvider> provider) noexcept { network.SetDefaultProvider(std::move(provider)); }
std::shared_ptr<NativeDefaultLocalAddressProvider> network_value_default_provider(const NativeNetwork& network) noexcept { return network.GetDefaultProvider(); }
std::shared_ptr<NativeNetwork> network_value_new(rust::Slice<const std::uint8_t> name,
    rust::Slice<const std::uint8_t> description, rust::Slice<const std::uint8_t> prefix,
    std::int32_t prefix_length, std::int32_t adapter) noexcept {
  return std::make_shared<NativeNetwork>(std::make_unique<webrtc::Network>(View(name), View(description), Ip(prefix), prefix_length, static_cast<webrtc::AdapterType>(adapter)));
}
std::shared_ptr<NativeNetwork> network_value_clone(const NativeNetwork& network) noexcept {
  return std::make_shared<NativeNetwork>(network.network().Clone());
}
bool network_value_same(const NativeNetwork& first, const NativeNetwork& second) noexcept { return &first.network() == &second.network(); }
rust::Vec<std::uint8_t> network_value_bytes(const NativeNetwork& value, std::uint8_t field) noexcept {
  const auto& network = value.network();
  switch (field) {
    case 0: return Bytes(network.name());
    case 1: return Bytes(network.description());
    case 2: return Bytes(network.prefix());
    case 3: return Bytes(network.key());
    case 4: return Bytes(network.GetBestIP());
    case 5: return Bytes(network.ToString());
    default: return {};
  }
}
std::int32_t network_value_info(const NativeNetwork& value, std::uint8_t field) noexcept {
  const auto& network = value.network();
  switch (field) {
    case 0: return network.scope_id();
    case 1: return network.ignored();
    case 2: return network.type();
    case 3: return network.underlying_type_for_vpn();
    case 4: return network.preference();
    case 5: return network.active();
    case 6: return network.id();
    case 7: return static_cast<std::int32_t>(network.network_preference());
    case 8: return static_cast<std::int32_t>(network.network_slice());
    case 9: return network.IsVpn();
    case 10: return network.IsCellular();
    case 11: return network.family();
    case 12: return network.prefix_length();
    default: return 0;
  }
}
bool network_value_set(const NativeNetwork& value, std::uint8_t field, std::int32_t input) noexcept {
  const std::uint8_t mask = (field == 2 || field == 3) ? 1 : field == 7 ? 2 : field == 8 ? 4 : 0;
  const bool nested = value.Delivering(mask);
  // SDK CallbackList forbids recursive Send. Equal values are still safe:
  // these SDK setters return before sending when their value is unchanged.
  if (nested && network_value_info(value, field) != input) return false;
  if (!nested) value.BeginNotification(mask);
  auto& network = value.network();
  switch (field) {
    case 0: network.set_scope_id(input); break;
    case 1: network.set_ignored(input); break;
    case 2: network.set_type(static_cast<webrtc::AdapterType>(input)); break;
    case 3: network.set_underlying_type_for_vpn(static_cast<webrtc::AdapterType>(input)); break;
    case 4: network.set_preference(input); break;
    case 5: network.set_active(input); break;
    case 6: network.set_id(static_cast<std::uint16_t>(input)); break;
    case 7: network.set_network_preference(static_cast<webrtc::NetworkPreference>(input)); break;
    case 8: network.set_network_slice(static_cast<webrtc::NetworkSlice>(input)); break;
    default: break;
  }
  if (!nested) value.EndNotification(mask);
  return true;
}

bool network_value_notify(const NativeNetwork& value, std::uint8_t kind) noexcept {
  if (kind > 2) return false;
  const auto mask = static_cast<std::uint8_t>(1 << kind);
  if (value.Delivering(mask)) return false;
  value.BeginNotification(mask);
  auto& network = value.network();
  // The publication sender is the actual owning Network, preserving Port's
  // sender-identity precondition rather than passing a synthetic/raw identity.
  switch (kind) {
    case 0: network.NotifyTypeChanged(&network); break;
    case 1: network.NotifyNetworkPreferenceChanged(&network); break;
    case 2: network.NotifyNetworkSliceChanged(&network); break;
  }
  value.EndNotification(mask);
  return true;
}

struct NativeNetworkSubscription::State {
  State(std::uint8_t selected_kind, rust::Box<RustNetworkCallback> selected_callback)
      : kind(selected_kind), callback(std::move(selected_callback)) {}
  std::uint8_t kind;
  bool active = true;
  rust::Box<RustNetworkCallback> callback;
};
NativeNetworkSubscription::NativeNetworkSubscription(std::shared_ptr<NativeNetwork> network, std::shared_ptr<State> state) noexcept
    : network_(std::move(network)), state_(std::move(state)) {}
NativeNetworkSubscription::~NativeNetworkSubscription() { Cancel(); }
bool NativeNetworkSubscription::Cancel() const noexcept {
  if (!state_->active) return false;
  state_->active = false;
  if (network_->Delivering(7)) network_->RetainCancelledCallback(state_);
  switch (state_->kind) {
    case 0: network_->network().UnsubscribeTypeChanged(state_.get()); break;
    case 1: network_->network().UnsubscribeNetworkPreferenceChanged(state_.get()); break;
    case 2: network_->network().UnsubscribeNetworkSliceChanged(state_.get()); break;
  }
  return true;
}
bool NativeNetworkSubscription::Failed() const noexcept { return network_callback_failed(*state_->callback); }
std::unique_ptr<NativeNetworkSubscription> network_value_subscribe(std::shared_ptr<NativeNetwork> network, std::uint8_t kind, rust::Box<RustNetworkCallback> callback) noexcept {
  if (kind > 2 || network->Delivering(static_cast<std::uint8_t>(1 << kind))) return nullptr;
  auto state = std::make_shared<NativeNetworkSubscription::State>(kind, std::move(callback));
  std::weak_ptr<NativeNetwork> weak = network;
  auto deliver = [state, weak](const webrtc::Network*) {
    // Keep the callback frame alive if it cancels/drops its own subscription.
    auto lease = state;
    auto owner = weak.lock();
    if (lease->active && owner) network_callback_fire(*lease->callback, std::move(owner));
  };
  switch (kind) {
    case 0: network->network().SubscribeTypeChanged(state.get(), deliver); break;
    case 1: network->network().SubscribeNetworkPreferenceChanged(state.get(), deliver); break;
    case 2: network->network().SubscribeNetworkSliceChanged(state.get(), deliver); break;
  }
  return std::make_unique<NativeNetworkSubscription>(std::move(network), std::move(state));
}
bool network_subscription_cancel(const NativeNetworkSubscription& subscription) noexcept { return subscription.Cancel(); }
bool network_subscription_failed(const NativeNetworkSubscription& subscription) noexcept { return subscription.Failed(); }

void network_value_add_ip(const NativeNetwork& value, rust::Slice<const std::uint8_t> ip, std::int32_t flags) noexcept {
  value.network().AddIP(webrtc::InterfaceAddress(Ip(ip), flags));
}
rust::Vec<FfiInterfaceAddress> network_value_ips(const NativeNetwork& value) noexcept {
  rust::Vec<FfiInterfaceAddress> result;
  for (const auto& ip : value.network().GetIPs()) result.push_back({Bytes(ip), ip.ipv6_flags()});
  return result;
}
bool network_value_set_ips(const NativeNetwork& value, rust::Slice<const FfiInterfaceAddress> ips, bool already_changed) noexcept {
  std::vector<webrtc::InterfaceAddress> input;
  input.reserve(ips.size());
  for (const auto& ip : ips) input.emplace_back(Ip({ip.ip.data(), ip.ip.size()}), ip.flags);
  return value.network().SetIPs(input, already_changed);
}
void network_value_clear_ips(const NativeNetwork& value) noexcept { value.network().ClearIPs(); }
std::int32_t network_value_cost(const NativeNetwork& value, const NativeFieldTrials& trials) noexcept {
  const auto& network = value.network();
  // This combination enters the SDK's VPN-only DCHECK path. Preserve storing
  // and reading either value, rejecting only the reached cost computation.
  if (network.IsVpn() && network.underlying_type_for_vpn() == webrtc::ADAPTER_TYPE_VPN) return -1;
  return network.GetCost(trials.view());
}
FfiNetworkCostGuess network_guess_cost(std::int32_t cost) noexcept {
  const auto [adapter, vpn, slice] = webrtc::Network::GuessAdapterFromNetworkCost(cost);
  return {static_cast<std::int32_t>(adapter), vpn, static_cast<std::int32_t>(slice)};
}
bool network_adapter_is_cellular(std::int32_t adapter) noexcept { return webrtc::Network::IsCellular(static_cast<webrtc::AdapterType>(adapter)); }
rust::Vec<std::uint8_t> network_make_key(rust::Slice<const std::uint8_t> name, rust::Slice<const std::uint8_t> prefix, std::int32_t length) noexcept {
  return Bytes(webrtc::MakeNetworkKey(View(name), Ip(prefix), length));
}
std::int32_t network_adapter_from_name(rust::Slice<const std::uint8_t> name) noexcept {
  if constexpr (std::numeric_limits<char>::is_signed) {
    for (std::size_t i = 0; i < name.size(); ++i) {
      const int character = static_cast<char>(name[i]);
      if (character < 0 && character != EOF) {
        // The SDK calls isdigit(char) only after matching a type prefix and
        // while all earlier suffix bytes are digits. Ask that same native
        // classifier about the safe prefix, rather than guessing its patterns.
        // An unrecognized prefix means this bad byte is never reached.
        if (webrtc::GetAdapterTypeFromName(View({name.data(), i})) != webrtc::ADAPTER_TYPE_UNKNOWN) return -1;
        break;
      }
    }
  }
  return webrtc::GetAdapterTypeFromName(View(name));
}
NativeNetworkMask::NativeNetworkMask(std::unique_ptr<webrtc::NetworkMask> mask) noexcept : mask_(std::move(mask)) {}
NativeNetworkMask::~NativeNetworkMask() = default;
const webrtc::NetworkMask& NativeNetworkMask::mask() const noexcept { return *mask_; }
std::unique_ptr<NativeNetworkMask> network_mask_new(rust::Slice<const std::uint8_t> prefix, std::int32_t length) noexcept {
  return std::make_unique<NativeNetworkMask>(std::make_unique<webrtc::NetworkMask>(Ip(prefix), length));
}
rust::Vec<std::uint8_t> network_mask_address(const NativeNetworkMask& mask) noexcept { return Bytes(mask.mask().address()); }
std::int32_t network_mask_prefix_length(const NativeNetworkMask& mask) noexcept { return mask.mask().prefix_length(); }
bool network_mask_equal(const NativeNetworkMask& first, const NativeNetworkMask& second) noexcept { return first.mask() == second.mask(); }
}  // namespace pulsebeam::webrtc_sys
