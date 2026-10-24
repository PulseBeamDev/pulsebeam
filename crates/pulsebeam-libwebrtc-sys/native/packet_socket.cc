#include "pulsebeam-webrtc-sys/native/packet_socket.h"
#include <limits>
#include <cerrno>
#include <functional>
#include <algorithm>
#include <vector>
#include <mutex>
#include "rtc_base/thread.h"
#include <optional>
#include <span>
#include "pulsebeam-webrtc-sys/native/socket_address.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/async_dns.h"
#include "pulsebeam-webrtc-sys/native/tls.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
namespace pulsebeam::webrtc_sys {
namespace {
webrtc::Timestamp Timestamp(std::int64_t value) {
  if (value == std::numeric_limits<std::int64_t>::max()) return webrtc::Timestamp::PlusInfinity();
  if (value == std::numeric_limits<std::int64_t>::min()) return webrtc::Timestamp::MinusInfinity();
  return webrtc::Timestamp::Micros(value);
}
std::int64_t TimestampBits(webrtc::Timestamp value) {
  if (value.IsPlusInfinity()) return std::numeric_limits<std::int64_t>::max();
  if (value.IsMinusInfinity()) return std::numeric_limits<std::int64_t>::min();
  return value.us();
}
}  // namespace
FfiSocketPacketInfo packet_info_to_ffi(const webrtc::PacketInfo& value) noexcept {
  return {value.included_in_feedback, value.included_in_allocation, value.is_media,
      static_cast<std::int32_t>(value.packet_type), static_cast<std::int32_t>(value.protocol),
      value.network_id.has_value(), value.network_id.value_or(0), value.packet_size_bytes,
      value.turn_overhead_bytes, value.ip_overhead_bytes};
}
webrtc::PacketInfo packet_info_from_ffi(const FfiSocketPacketInfo& value) noexcept {
  webrtc::PacketInfo result;
  result.included_in_feedback = value.included_in_feedback;
  result.included_in_allocation = value.included_in_allocation;
  result.is_media = value.is_media;
  result.packet_type = static_cast<webrtc::PacketType>(value.packet_type);
  result.protocol = static_cast<webrtc::PacketInfoProtocolType>(value.protocol);
  if (value.has_network_id) result.network_id = value.network_id;
  result.packet_size_bytes = value.packet_size_bytes;
  result.turn_overhead_bytes = value.turn_overhead_bytes;
  result.ip_overhead_bytes = value.ip_overhead_bytes;
  return result;
}
FfiSocketPacketOptions packet_options_to_ffi(const webrtc::AsyncSocketPacketOptions& value) noexcept {
  FfiSocketPacketOptions result;
  result.dscp = static_cast<std::int32_t>(value.dscp);
  result.ect_1 = value.ect_1;
  result.packet_id = value.packet_id;
  result.extension_id = value.packet_time_params.rtp_sendtime_extension_id;
  for (auto byte : value.packet_time_params.srtp_auth_key) result.auth_key.push_back(static_cast<std::uint8_t>(byte));
  result.auth_tag_len = value.packet_time_params.srtp_auth_tag_len;
  result.packet_index = value.packet_time_params.srtp_packet_index;
  result.info = packet_info_to_ffi(value.info_signaled_after_sent);
  result.batchable = value.batchable;
  result.last_packet_in_batch = value.last_packet_in_batch;
  return result;
}
webrtc::AsyncSocketPacketOptions packet_options_from_ffi(const FfiSocketPacketOptions& value) noexcept {
  webrtc::AsyncSocketPacketOptions result;
  // The public owning token checks the pinned unscoped enum representation.
  result.dscp = static_cast<webrtc::DiffServCodePoint>(value.dscp);
  result.ect_1 = value.ect_1;
  result.packet_id = value.packet_id;
  result.packet_time_params.rtp_sendtime_extension_id = value.extension_id;
  for (auto byte : value.auth_key) result.packet_time_params.srtp_auth_key.push_back(static_cast<char>(byte));
  result.packet_time_params.srtp_auth_tag_len = value.auth_tag_len;
  result.packet_time_params.srtp_packet_index = value.packet_index;
  result.info_signaled_after_sent = packet_info_from_ffi(value.info);
  result.batchable = value.batchable;
  result.last_packet_in_batch = value.last_packet_in_batch;
  return result;
}
FfiSocketSentPacket sent_packet_to_ffi(const webrtc::SentPacketInfo& value) noexcept {
  return {value.packet_id, value.send_time_ms, packet_info_to_ffi(value.info)};
}
webrtc::SentPacketInfo sent_packet_from_ffi(const FfiSocketSentPacket& value) noexcept {
  return webrtc::SentPacketInfo(value.packet_id, value.send_time_ms, packet_info_from_ffi(value.info));
}
FfiSocketReceivedPacket received_ip_packet_to_ffi(const webrtc::ReceivedIpPacket& value) noexcept {
  FfiSocketReceivedPacket result;
  for (auto byte : value.payload()) result.payload.push_back(byte);
  result.address = std::make_unique<NativeSocketAddress>(value.source_address());
  result.has_arrival = value.arrival_time().has_value();
  result.arrival_us = result.has_arrival ? TimestampBits(*value.arrival_time()) : 0;
  result.ecn = static_cast<std::uint8_t>(value.ecn());
  result.decryption = static_cast<std::uint8_t>(value.decryption_info());
  return result;
}
FfiSocketReceivedPacket socket_received_packet_copy(const FfiSocketReceivedPacket& value, std::uint8_t decryption) noexcept {
  std::optional<webrtc::Timestamp> arrival;
  if (value.has_arrival) arrival = Timestamp(value.arrival_us);
  const webrtc::ReceivedIpPacket packet(std::span<const std::uint8_t>(value.payload.data(), value.payload.size()),
      value.address->value, arrival, static_cast<webrtc::EcnMarking>(value.ecn),
      static_cast<webrtc::ReceivedIpPacket::DecryptionInfo>(value.decryption));
  return received_ip_packet_to_ffi(packet.CopyAndSet(static_cast<webrtc::ReceivedIpPacket::DecryptionInfo>(decryption)));
}
FfiSocketReceivedPacket socket_received_packet_legacy(rust::Slice<const std::uint8_t> payload,
    const NativeSocketAddress& address, std::int64_t time_us) noexcept {
  return received_ip_packet_to_ffi(webrtc::ReceivedIpPacket::CreateFromLegacy(payload.data(), payload.size(), time_us, address.value));
}

namespace {
class RustPacketSocketAdapter;
struct SocketResource {
  virtual ~SocketResource() = default;
  virtual void Disable() = 0;
  virtual void Release() = 0;
};
}
struct NativePacketSocketResources::State {
  explicit State(webrtc::Thread* owner) : role(owner) {}
  webrtc::Thread* CurrentRole() {
    std::lock_guard lock(mutex);
    auto* queue = webrtc::TaskQueueBase::Current();
    return !closing && role && role->IsCurrent() && (!queue || queue == role) ? role : nullptr;
  }
  bool Enroll(const std::shared_ptr<SocketResource>& resource) {
    std::lock_guard lock(mutex);
    auto* queue = webrtc::TaskQueueBase::Current();
    if (closing || !role || !role->IsCurrent() || (queue && queue != role)) return false;
    std::erase_if(resources, [](const auto& item) { return item.expired(); });
    resources.push_back(resource);
    return true;
  }
  void RetireOnRole() {
    std::vector<std::shared_ptr<SocketResource>> leases;
    {
      std::lock_guard lock(mutex);
      closing = true;
      role = nullptr;
      for (const auto& item : resources) if (auto live = item.lock()) leases.push_back(std::move(live));
      resources.clear();
    }
    // Disable the whole scope before any provider destructor can reenter a
    // sibling context. No resource/submission lock spans user destruction.
    for (const auto& item : leases) item->Disable();
    for (const auto& item : leases) item->Release();
  }
  std::mutex mutex;
  webrtc::Thread* role;
  bool closing = false;
  std::vector<std::weak_ptr<SocketResource>> resources;
};
NativePacketSocketResources::NativePacketSocketResources() noexcept
    : state(std::make_shared<State>(webrtc::Thread::Current())) {}
NativePacketSocketResources::~NativePacketSocketResources() { Retire(); }
void NativePacketSocketResources::Retire() noexcept {
  webrtc::Thread* role;
  {
    std::lock_guard lock(state->mutex);
    if (state->closing || !state->role) return;
    state->closing = true;
    role = state->role;
  }
  auto retire = [lease = state] { lease->RetireOnRole(); };
  if (role->IsCurrent()) retire(); else role->BlockingCall(std::move(retire));
}
std::uint8_t packet_socket_resources_status(const NativePacketSocketResources& resources) noexcept {
  std::lock_guard lock(resources.state->mutex);
  if (resources.state->closing || !resources.state->role) return 1;
  auto* queue = webrtc::TaskQueueBase::Current();
  return resources.state->role->IsCurrent() && (!queue || queue == resources.state->role) ? 0 : 3;
}
NativePacketSocketScope::NativePacketSocketScope() noexcept : resources_(std::make_shared<NativePacketSocketResources>()) {}
NativePacketSocketScope::~NativePacketSocketScope() { resources_->Retire(); }
const std::shared_ptr<NativePacketSocketResources>& NativePacketSocketScope::resources() const noexcept { return resources_; }
std::unique_ptr<NativePacketSocketScope> packet_socket_scope_new() noexcept {
  auto* role = webrtc::Thread::Current();
  auto* queue = webrtc::TaskQueueBase::Current();
  if (!role || (queue && queue != role)) return nullptr;
  return std::make_unique<NativePacketSocketScope>();
}
std::shared_ptr<NativePacketSocketResources> packet_socket_scope_resources(const NativePacketSocketScope& scope) noexcept { return scope.resources(); }
struct NativePacketSocketSubscription::State {
  State(std::uint8_t kind, bool legacy, rust::Box<RustSocketCallback> callback)
      : kind(kind), legacy(legacy), callback(std::make_shared<rust::Box<RustSocketCallback>>(std::move(callback))) {}
  const std::uint8_t kind;
  const bool legacy;
  bool active = true;
  bool bound = false;
  std::shared_ptr<rust::Box<RustSocketCallback>> callback;  // Role-local only.
};
struct NativePacketSocketEvents::State : SocketResource, std::enable_shared_from_this<NativePacketSocketEvents::State> {
  State(webrtc::Thread* owner, std::shared_ptr<NativePacketSocketResources::State> resources)
      : role(owner), queue_bound(webrtc::TaskQueueBase::Current() != nullptr), scope(std::move(resources)) {}
  bool OnRole() const { return role && role->IsCurrent() && (!queue_bound || webrtc::TaskQueueBase::Current() == role); }
  void Disable() override {
    std::lock_guard lock(mutex);
    role = nullptr;
    socket = nullptr;
    for (const auto& observer : observers) { observer->active = false; observer->bound = false; }
  }
  void Release() override {
    // Detach owning collections before any user destructor can reenter. SDK
    // trampolines retain tag metadata, with captures cleared here. Active frames delay callback
    // capture destruction until SDK tombstone cleanup has finished.
    auto old_source = std::move(source);
    auto old_observers = std::move(observers);
    for (const auto& observer : old_observers) {
      auto callback = std::move(observer->callback);
      if (callback && active) cancelled_callbacks.push_back(std::move(callback));
    }
    if (!active) { auto old = std::move(cancelled_callbacks); }
  }
  void Retire() { Disable(); Release(); }
  void Bind(RustPacketSocketAdapter* target);
  void Attach(const std::shared_ptr<NativePacketSocketSubscription::State>& observer);
  void Prune();
  void Detach(const std::shared_ptr<NativePacketSocketSubscription::State>& observer, bool receive_finished = false);
  bool Remove(const std::shared_ptr<NativePacketSocketSubscription::State>& observer);
  void Finish(std::uint16_t bit);
  std::uint8_t ObserveStatus(std::uint8_t kind);
  std::uint8_t Run(std::uint8_t channel, const std::function<void(RustPacketSocketAdapter&)>& callback);
  std::uint8_t Submit(std::uint8_t channel, std::function<void(RustPacketSocketAdapter&)> callback, bool post,
                      const std::shared_ptr<State>& lease) {
    {
      std::lock_guard lock(mutex);
      if (!role) return 1;
      if (post) {
        // The role loan remains protected until submission completes. The
        // message owns data only; cancellation never drops a Rust product here.
        role->PostTask([lease, channel, callback = std::move(callback)] { lease->Run(channel, callback); });
        return 0;
      }
      if (!OnRole()) return 3;
    }
    return Run(channel, callback);
  }
  std::mutex mutex;
  webrtc::Thread* role;
  RustPacketSocketAdapter* socket = nullptr;
  const bool queue_bound;  // The SDK checker binds to this actual Thread queue, or just its OS thread.
  std::uint16_t active = 0;  // Role-local channel admission, including cleanup.
  bool owns_receiver = false;
  std::weak_ptr<void> standalone_owner;  // Exact native core, never a role-name join.
  std::vector<std::shared_ptr<NativePacketSocketSubscription::State>> observers;
  std::vector<std::shared_ptr<rust::Box<RustSocketCallback>>> cancelled_callbacks;
  std::shared_ptr<NativePacketSocketResources::State> scope;
  std::shared_ptr<rust::Box<RustPacketSocket>> source;  // Role-local; cleared before role release.
};
namespace {
rust::Vec<std::uint8_t> SocketPayload(const void* data, std::size_t size) {
  rust::Vec<std::uint8_t> result;
  result.reserve(size);
  const auto* bytes = static_cast<const std::uint8_t*>(data);
  for (std::size_t i = 0; i < size; ++i) result.push_back(bytes[i]);
  return result;
}
class RustPacketSocketAdapter final : public webrtc::AsyncPacketSocket {
 public:
  RustPacketSocketAdapter(rust::Box<RustPacketSocket> source, std::shared_ptr<NativePacketSocketEvents> events)
      : events_(std::move(events)) {
    // Bind the SDK checker now, not to an arbitrary later subscriber's queue.
    RTC_DCHECK_RUN_ON(&network_checker_);
    events_->state->source = std::make_shared<rust::Box<RustPacketSocket>>(std::move(source));
    events_->state->Bind(this);
  }
  ~RustPacketSocketAdapter() override {
    auto state = events_->state;
    webrtc::Thread* role;
    { std::lock_guard lock(state->mutex); role = state->role; }
    // Factory-scope retirement may already have cleared both the product and
    // the role loan. Such a late facade drop must not dereference the old role.
    if (!role) return;
    auto retire = [state] { state->Retire(); };
    if (role->IsCurrent()) retire(); else role->BlockingCall(std::move(retire));
  }
  webrtc::SocketAddress GetLocalAddress() const override {
    auto source = events_->state->source;
    if (!source) return {};
    return packet_socket_local(**source, false)->value;
  }
  webrtc::SocketAddress GetRemoteAddress() const override {
    auto source = events_->state->source;
    if (!source) return {};
    return packet_socket_local(**source, true)->value;
  }
  int Send(const void* data, std::size_t size, const webrtc::AsyncSocketPacketOptions& options) override {
    auto source = events_->state->source;
    if (!source) return -1;
    return packet_socket_send(**source, SocketPayload(data, size), packet_options_to_ffi(options));
  }
  int SendTo(const void* data, std::size_t size, const webrtc::SocketAddress& address,
             const webrtc::AsyncSocketPacketOptions& options) override {
    auto source = events_->state->source;
    if (!source) return -1;
    return packet_socket_send_to(**source, SocketPayload(data, size), std::make_unique<NativeSocketAddress>(address), packet_options_to_ffi(options));
  }
  int Close() override { auto source = events_->state->source; return source ? packet_socket_close(**source) : -1; }
  State GetState() const override {
    auto source = events_->state->source;
    if (!source) return STATE_CLOSED;
    return static_cast<State>(packet_socket_state(**source));
  }
  int GetOption(webrtc::Socket::Option option, int* value) override {
    auto source = events_->state->source;
    if (!source) return -1;
    const auto result = packet_socket_get_option(**source, static_cast<std::int32_t>(option));
    if (result.code == 0) *value = result.value;
    return result.code;
  }
  int SetOption(webrtc::Socket::Option option, int value) override {
    auto source = events_->state->source;
    if (!source) return -1;
    return packet_socket_set_option(**source, static_cast<std::int32_t>(option), value);
  }
  int GetError() const override { auto source = events_->state->source; return source ? packet_socket_error(**source) : EIO; }
  void SetError(int error) override { auto source = events_->state->source; if (source) packet_socket_set_error(**source, error); }
  bool Failed() const { auto source = events_->state->source; return !source || packet_socket_source_failed(**source); }
  bool CopyInformation(std::size_t size, FfiSocketPacketInfo& info) const {
    auto source = events_->state->source;
    if (!source) return false;
    auto native = packet_info_from_ffi(info);
    webrtc::CopySocketInformationToPacketInfo(size, *this, &native);
    // The virtual address query can fail/reenter. Inspect the leased provider,
    // not the facade, before exposing helper output as a successful query.
    if (packet_socket_source_failed(**source)) return false;
    info = packet_info_to_ffi(native);
    return true;
  }
  void Simple(std::uint8_t kind, std::int32_t error) {
    if (kind == 0) NotifyClosed(error);
    else if (kind == 1) NotifyReadyToSend(this);
    else NotifyConnect(this);
    // No facade access after native callbacks, including receiver cleanup.
  }
  void Received(const FfiSocketReceivedPacket& value) {
    std::optional<webrtc::Timestamp> arrival;
    if (value.has_arrival) arrival = Timestamp(value.arrival_us);
    const webrtc::ReceivedIpPacket packet(std::span<const std::uint8_t>(value.payload.data(), value.payload.size()),
        value.address->value, arrival, static_cast<webrtc::EcnMarking>(value.ecn),
        static_cast<webrtc::ReceivedIpPacket::DecryptionInfo>(value.decryption));
    NotifyPacketReceived(packet);
  }
  void Sent(const FfiSocketSentPacket& value) {
    const auto packet = sent_packet_from_ffi(value);
    NotifySentPacket(this, packet);
  }
  void Address(const NativeSocketAddress& address) { NotifyAddressReady(this, address.value); }
 private:
  std::shared_ptr<NativePacketSocketEvents> events_;
};
}  // namespace
// Lifecycle cleanup may be requested from another cooperative queue on the
// same actual SDK Thread. Establish that Thread's real queue identity without
// dispatching tasks, waiting or advancing controlled time. Never crosses OS
// threads and never invents a queue token.
class SocketRoleRetirement : public webrtc::TaskQueueBase {
 public:
  static void Run(webrtc::Thread* role, const std::function<void()>& cleanup) {
    CurrentTaskQueueSetter current(role);
    cleanup();
  }
};
struct NativeOwnedPacketSocket::Core {
  explicit Core(rust::Box<RustSocketRole> role) : role(std::move(role)) {}
  rust::Box<RustSocketRole> role;  // Last: actual driver and environment owners.
  std::unique_ptr<NativePacketSocketScope> scope;
  std::shared_ptr<NativePacketSocketEvents> events;
  std::unique_ptr<webrtc::AsyncPacketSocket> socket;  // First to release.
};
NativeOwnedPacketSocket::NativeOwnedPacketSocket(std::shared_ptr<Core> value) noexcept : core(std::move(value)) {}
void RetireOwnedPacket(std::shared_ptr<NativeOwnedPacketSocket::Core> owner) {
  if (!owner) return;
  webrtc::Thread* role;
  { std::lock_guard lock(owner->events->state->mutex); role = owner->events->state->role; }
  if (role && role->IsCurrent()) {
    SocketRoleRetirement::Run(role, [&] { owner->scope.reset(); owner.reset(); });
  } else {
    // This branch is only inert metadata after an earlier retirement. The Rust
    // owner is !Send and the retained controlled driver fixes the OS thread.
    owner->scope.reset();
  }
}
NativeOwnedPacketSocket::~NativeOwnedPacketSocket() { RetireOwnedPacket(std::move(core)); }
std::unique_ptr<NativeOwnedPacketSocket> owned_packet_socket_new(rust::Box<RustPacketSocket> source,
    std::shared_ptr<NativePacketSocketEvents> events, std::unique_ptr<NativePacketSocketScope> scope,
    rust::Box<RustSocketRole> role) noexcept {
  if (!scope || !events || !scope->resources()->state->CurrentRole()) return nullptr;
  if (events->state->scope != scope->resources()->state) return nullptr;
  auto core = std::make_shared<NativeOwnedPacketSocket::Core>(std::move(role));
  core->scope = std::move(scope);
  core->events = std::move(events);
  core->socket = packet_socket_adapter(std::move(source), core->events);
  if (!core->socket) return nullptr;
  core->events->state->owns_receiver = true;
  core->events->state->standalone_owner = core;
  return std::make_unique<NativeOwnedPacketSocket>(std::move(core));
}
FfiOwnedSocketResult socket_member_call(const NativePacketSocketEvents& events, std::uint8_t operation,
    rust::Slice<const std::uint8_t> payload, std::unique_ptr<NativeSocketAddress> address,
    FfiSocketPacketOptions options, std::int32_t option, std::int32_t value) noexcept {
  auto state = events.state;
  auto owner_frame = state->standalone_owner.lock();
  RustPacketSocketAdapter* target;
  FfiOwnedSocketResult result{};
  {
    std::lock_guard lock(state->mutex);
    if (!state->role) { result.status = 1; return result; }
    if (!state->OnRole()) { result.status = 3; return result; }
    if (!state->socket) { result.status = 2; return result; }
    target = state->socket;
  }
  auto provider = state->source;
  const bool cleanup = operation == 5 || operation == 8 || operation == 9;
  if (!provider || (!cleanup && packet_socket_source_failed(**provider))) { result.status = 5; return result; }
  switch (operation) {
    case 0: result.address = std::make_unique<NativeSocketAddress>(target->GetLocalAddress()); break;
    case 1: result.address = std::make_unique<NativeSocketAddress>(target->GetRemoteAddress()); break;
    case 2: result.code = static_cast<std::int32_t>(target->GetState()); break;
    case 3: result.code = target->Send(payload.data(), payload.size(), packet_options_from_ffi(options)); break;
    case 4:
      if (!address) { result.status = 5; return result; }
      result.code = target->SendTo(payload.data(), payload.size(), address->value, packet_options_from_ffi(options)); break;
    case 5: result.code = target->Close(); break;
    case 6:
      if (option < 0 || option > 15) { result.status = 5; return result; }
      result.code = target->GetOption(static_cast<webrtc::Socket::Option>(option), &result.value); break;
    case 7:
      if (option < 0 || option > 15) { result.status = 5; return result; }
      result.code = target->SetOption(static_cast<webrtc::Socket::Option>(option), value); break;
    case 8: result.code = target->GetError(); break;
    case 9: target->SetError(value); break;
    default: result.status = 5; return result;
  }
  // The leased provider, not a reentrant facade/source slot, diagnoses failure.
  // Native signed operation results and nonzero GetOption results stay exact.
  if (!cleanup && packet_socket_source_failed(**provider)) result.status = 5;
  return result;
}
void NativePacketSocketEvents::State::Prune() {
  std::erase_if(observers, [](const auto& observer) { return !observer->active && !observer->bound; });
}
void NativePacketSocketEvents::State::Bind(RustPacketSocketAdapter* target) {
  { std::lock_guard lock(mutex); if (!role) return; socket = target; }
  for (const auto& observer : observers) Attach(observer);
}
void NativePacketSocketEvents::State::Attach(const std::shared_ptr<NativePacketSocketSubscription::State>& observer) {
  if (!socket || !observer->active || observer->bound) return;
  observer->bound = true;
  auto deliver = [weak = weak_from_this(), tag = observer](FfiSocketObservation value) {
    auto owner = weak.lock(); auto receiver = tag;
    if (!owner || !receiver || !receiver->active) return;
    auto callback = receiver->callback;
    if (!callback) return;
    if (!socket_callback_fire(**callback, std::move(value))) owner->Remove(receiver);
  };
  switch (observer->kind) {
    case 0:
      socket->SubscribeCloseEvent(observer.get(), [deliver](webrtc::AsyncPacketSocket*, int error) {
        FfiSocketObservation value{}; value.kind = 0; value.error = error; deliver(std::move(value));
      });
      break;
    case 1:
      socket->SubscribeReadyToSend(observer.get(), [deliver](webrtc::AsyncPacketSocket*) {
        FfiSocketObservation value{}; value.kind = 1; deliver(std::move(value));
      });
      break;
    case 2: {
      auto callback = [deliver](webrtc::AsyncPacketSocket*) {
        FfiSocketObservation value{}; value.kind = 2; deliver(std::move(value));
      };
      if (observer->legacy) socket->SubscribeConnect(std::move(callback));
      else socket->SubscribeConnect(observer.get(), std::move(callback));
      break;
    }
    case 3:
      socket->RegisterReceivedPacketCallback([deliver](webrtc::AsyncPacketSocket*, const webrtc::ReceivedIpPacket& packet) {
        FfiSocketObservation value{}; value.kind = 3; value.packet = received_ip_packet_to_ffi(packet); deliver(std::move(value));
      });
      break;
    case 4:
      socket->SubscribeSentPacket(observer.get(), [deliver](webrtc::AsyncPacketSocket*, const webrtc::SentPacketInfo& packet) {
        FfiSocketObservation value{}; value.kind = 4; value.sent = sent_packet_to_ffi(packet); deliver(std::move(value));
      });
      break;
    case 5:
      socket->SubscribeAddressReady(observer.get(), [deliver](webrtc::AsyncPacketSocket*, const webrtc::SocketAddress& address) {
        FfiSocketObservation value{}; value.kind = 5; value.address = std::make_unique<NativeSocketAddress>(address); deliver(std::move(value));
      });
      break;
  }
}
std::uint8_t NativePacketSocketEvents::State::ObserveStatus(std::uint8_t kind) {
  if (kind > 5) return 5;
  { std::lock_guard lock(scope->mutex);
    if (scope->closing || !scope->role) return 1;
    if (!scope->role->IsCurrent()) return 3;
  }
  std::lock_guard lock(mutex);
  if (!role) return 1;
  if (!OnRole()) return 3;
  if (active & (1u << kind)) return 4;
  if (kind == 3) {
    if (!owns_receiver) return 9;
    // Logical cancellation from another cooperative queue may still have its
    // native deregistration queued. Until bound clears, SDK storage is occupied.
    if (std::any_of(observers.begin(), observers.end(), [](const auto& observer) {
          return observer->kind == 3 && (observer->active || observer->bound);
        })) return 11;
  }
  return 0;
}
void NativePacketSocketEvents::State::Detach(const std::shared_ptr<NativePacketSocketSubscription::State>& observer, bool receive_finished) {
  // AnyInvocable is not a CallbackList tombstone. Never destroy its callable
  // storage while its own invocation is still on the stack.
  // Tagged-list sibling removal also violates this SDK's Send preconditions.
  if ((active & (1u << observer->kind)) && !receive_finished) return;
  if (observer->bound && socket) {
    observer->bound = false;
    switch (observer->kind) {
      case 0: socket->UnsubscribeCloseEvent(observer.get()); break;
      case 1: socket->UnsubscribeReadyToSend(observer.get()); break;
      case 2: if (!observer->legacy) socket->UnsubscribeConnect(observer.get()); break;
      case 3: socket->DeregisterReceivedPacketCallback(); break;
      case 4: socket->UnsubscribeSentPacket(observer.get()); break;
      case 5: socket->UnsubscribeAddressReady(observer.get()); break;
    }
  }
}
bool NativePacketSocketEvents::State::Remove(const std::shared_ptr<NativePacketSocketSubscription::State>& observer) {
  bool on_role;
  { std::lock_guard lock(mutex);
    if (!role || !role->IsCurrent()) return false;
    on_role = OnRole();
  }
  if (!observer->active) return false;
  observer->active = false;
  auto callback = std::move(observer->callback);
  // A trampoline holds a tag with a weak source. Pin Rust captures separately until
  // every enclosing native Send and its receiver-vector cleanup has returned.
  if (callback && (active || !on_role)) cancelled_callbacks.push_back(std::move(callback));
  if (!on_role) {
    // Same OS thread, different cooperative queue: logical cancellation is
    // immediate, but close-list unsubscription must not enter its queue checker.
    // Metadata-only work is safe if scope retirement cancels the queued task.
    std::lock_guard lock(mutex);
    role->PostTask([owner = shared_from_this(), observer] {
      if (!owner->OnRole()) return;
      owner->Detach(observer);
      owner->Prune();
      owner->Finish(0);
    });
    return true;
  }
  Detach(observer);
  Prune();
  // User capture destruction happens here, after all native/container mutation.
  return true;
}
void NativePacketSocketEvents::State::Finish(std::uint16_t bit) {
  auto detach_finished = [&] {
    for (const auto& observer : observers)
      if ((bit & (1u << observer->kind)) && !observer->active) Detach(observer, true);
    Prune();
  };
  detach_finished();
  // Nested different-channel Sends share one deferred capture queue. Keep the
  // last active channel admitted through cleanup; capture Drop cannot recursively
  // Add/Send it, even after the SDK's own Send flag has already been cleared.
  if (active == bit) {
    Prune();
    while (!cancelled_callbacks.empty()) {
      auto retired = std::move(cancelled_callbacks);
      cancelled_callbacks.clear();
      retired.clear();
      detach_finished();
    }
  }
  std::lock_guard lock(mutex);
  active &= static_cast<std::uint16_t>(~bit);
}
NativePacketSocketSubscription::NativePacketSocketSubscription(std::shared_ptr<NativePacketSocketEvents::State> owner, std::shared_ptr<State> state) noexcept
    : owner_(std::move(owner)), state_(std::move(state)) {}
NativePacketSocketSubscription::~NativePacketSocketSubscription() { Cancel(); }
bool NativePacketSocketSubscription::Cancel() const noexcept { return owner_->Remove(state_); }
bool NativePacketSocketSubscription::Active() const noexcept { return state_->active; }
bool NativePacketSocketSubscription::Bound() const noexcept { return state_->bound; }
FfiSocketSubscriptionResult socket_observe(const NativePacketSocketEvents& events, std::uint8_t kind, bool legacy, rust::Box<RustSocketCallback> callback) noexcept {
  auto owner = events.state;
  auto status = owner->ObserveStatus(kind);
  if (status != 0) return {status, {}};
  if (legacy && kind != 2) return {5, {}};
  auto observer = std::make_shared<NativePacketSocketSubscription::State>(kind, legacy, std::move(callback));
  owner->Prune();
  owner->observers.push_back(observer);
  owner->Attach(observer);
  return {0, std::make_unique<NativePacketSocketSubscription>(std::move(owner), std::move(observer))};
}
bool socket_subscription_cancel(const NativePacketSocketSubscription& subscription) noexcept { return subscription.Cancel(); }
bool socket_subscription_active(const NativePacketSocketSubscription& subscription) noexcept { return subscription.Active(); }
bool socket_subscription_bound(const NativePacketSocketSubscription& subscription) noexcept { return subscription.Bound(); }
std::uint8_t NativePacketSocketEvents::State::Run(std::uint8_t channel,
    const std::function<void(RustPacketSocketAdapter&)>& callback) {
  // A standalone shell can be dropped from a source/receiver callback. Its
  // actual SDK object must outlive Send/receive invocation and capture cleanup.
  auto owner_frame = standalone_owner.lock();
  RustPacketSocketAdapter* target;
  const auto bit = static_cast<std::uint16_t>(1u << channel);
  {
    std::lock_guard lock(mutex);
    if (!role) return 1;
    if (!OnRole()) return 3;
    if (!socket) return 2;
    if (active & bit) return 4;
    active |= bit;
    target = socket;
  }
  struct Frame {
    State& state;
    std::uint16_t bit;
    ~Frame() { state.Finish(bit); }
  } frame{*this, bit};
  // Never hold the submission lock across provider or native callback entry.
  if (target->Failed()) return 5;
  callback(*target);
  // Only the separately leased metadata is touched after callbacks return.
  return 0;
}
NativePacketSocketEvents::NativePacketSocketEvents(std::shared_ptr<State> value) noexcept : state(std::move(value)) {}
std::shared_ptr<NativePacketSocketEvents> packet_socket_events_in_scope(std::shared_ptr<NativePacketSocketResources> resources) noexcept {
  auto* role = resources ? resources->state->CurrentRole() : nullptr;
  if (!role) return nullptr;
  auto state = std::make_shared<NativePacketSocketEvents::State>(role, resources->state);
  if (!resources->state->Enroll(state)) { state->Retire(); return nullptr; }
  return std::make_shared<NativePacketSocketEvents>(std::move(state));
}
void packet_socket_events_retire(const NativePacketSocketEvents& events) noexcept { events.state->Retire(); }
std::unique_ptr<webrtc::AsyncPacketSocket> packet_socket_adapter(rust::Box<RustPacketSocket> source,
    std::shared_ptr<NativePacketSocketEvents> events) noexcept {
  if (!events) return nullptr;
  webrtc::Thread* role;
  { std::lock_guard lock(events->state->mutex); role = events->state->role; }
  if (!role || !role->IsCurrent() || events->state->scope->CurrentRole() != role) return nullptr;
  return std::make_unique<RustPacketSocketAdapter>(std::move(source), std::move(events));
}
std::uint8_t socket_event_simple(const NativePacketSocketEvents& events, std::uint8_t kind, std::int32_t error, bool post) noexcept {
  if (kind > 2) return 5;
  return events.state->Submit(kind, [kind, error](auto& socket) { socket.Simple(kind, error); }, post, events.state);
}
std::uint8_t socket_event_received(const NativePacketSocketEvents& events, FfiSocketReceivedPacket packet, bool post) noexcept {
  // Value-copy operations preserve representation 3, but the native DTLS/STUN
  // piggyback consumer cannot accept it as a delivered decryption status.
  if (packet.decryption > 2) return 6;
  auto owned = std::make_shared<FfiSocketReceivedPacket>(std::move(packet));
  return events.state->Submit(3, [owned](auto& socket) { socket.Received(*owned); }, post, events.state);
}
std::uint8_t socket_event_sent(const NativePacketSocketEvents& events, FfiSocketSentPacket packet, bool post) noexcept {
  // Preserve native metadata. Transport-feedback arithmetic is SDK-owned.
  auto owned = std::make_shared<FfiSocketSentPacket>(std::move(packet));
  return events.state->Submit(4, [owned](auto& socket) { socket.Sent(*owned); }, post, events.state);
}
std::uint8_t socket_event_address(const NativePacketSocketEvents& events, std::unique_ptr<NativeSocketAddress> address, bool post) noexcept {
  auto owned = std::shared_ptr<NativeSocketAddress>(std::move(address));
  return events.state->Submit(5, [owned](auto& socket) { socket.Address(*owned); }, post, events.state);
}
std::uint8_t socket_copy_information(const NativePacketSocketEvents& events, std::size_t size, FfiSocketPacketInfo& info) noexcept {
  bool copied = false;
  const auto status = events.state->Submit(6, [&](auto& socket) { copied = socket.CopyInformation(size, info); }, false, events.state);
  if (status != 0) return status;
  return copied ? 0 : 5;
}
std::int32_t socket_provider_io_error() noexcept { return EIO; }

namespace {
class RustListenSocketAdapter;
struct PendingAccept {
  std::shared_ptr<NativePacketSocketAccept> ticket = std::make_shared<NativePacketSocketAccept>();
  std::unique_ptr<NativeAcceptedPacketSocket> socket;
};
}
struct ListenDelivery {
  std::shared_ptr<NativeOwnedPacketSocket::Core> core;
  std::shared_ptr<rust::Box<RustPacketSocket>> provider;
  std::unique_ptr<webrtc::AsyncPacketSocket> escrow;
  bool claimed = false;
};
struct NativeListenSocketSubscription::State {
  State(bool ownership, rust::Box<RustListenCallback> callback)
      : ownership(ownership), callback(std::make_shared<rust::Box<RustListenCallback>>(std::move(callback))) {}
  bool ownership;
  bool active = true;
  bool bound = false;
  std::shared_ptr<rust::Box<RustListenCallback>> callback;
};
struct NativeListenSocketEvents::State : SocketResource, std::enable_shared_from_this<NativeListenSocketEvents::State> {
  State(webrtc::Thread* owner, std::shared_ptr<NativePacketSocketResources::State> scope)
      : role(owner), queue_bound(webrtc::TaskQueueBase::Current() != nullptr), scope(std::move(scope)) {}
  bool OnRole() const { return role && role->IsCurrent() && (!queue_bound || webrtc::TaskQueueBase::Current() == role); }
  void Disable() override {
    std::lock_guard lock(mutex);
    role = nullptr;
    socket = nullptr;
    inline_ready = false;
    for (const auto& observer : observers) { observer->active = false; if (!notifying) observer->bound = false; }
  }
  void Release() override {
    // Detach all owning collections before any Rust provider destructor runs.
    auto old_source = std::move(source);
    auto old_pending = std::move(pending);
    auto old_observers = std::move(observers);
    auto old_creators = std::move(creators);
    for (const auto& weak : old_creators) if (auto ticket = weak.lock()) {
      std::uint8_t expected = 0; ticket->status.compare_exchange_strong(expected, 4);
    }
    for (const auto& item : old_pending) {
      std::uint8_t expected = 0;
      item->ticket->status.compare_exchange_strong(expected, 4);
    }
    for (const auto& observer : old_observers) {
      auto callback = std::move(observer->callback);
      if (callback && notifying) cancelled_callbacks.push_back(std::move(callback));
    }
    if (notifying) observers = std::move(old_observers);
    else { auto old = std::move(cancelled_callbacks); }
  }
  void Retire() { Disable(); Release(); }
  void Prune() {
    std::vector<std::shared_ptr<PendingAccept>> old;
    std::erase_if(pending, [&](const auto& item) {
      if (item->ticket->status.load() == 0) return false;
      old.push_back(item);
      return true;
    });
    // Native products in removed records drop only after vector mutation ends.
  }
  void Run(const std::shared_ptr<PendingAccept>& item);
  bool HasOwner() const;
  void PruneObservers();
  void Attach(const std::shared_ptr<NativeListenSocketSubscription::State>& observer);
  bool Remove(const std::shared_ptr<NativeListenSocketSubscription::State>& observer);
  void Finish(RustListenSocketAdapter* target);
  std::mutex mutex;
  webrtc::Thread* role;
  RustListenSocketAdapter* socket = nullptr;
  const bool queue_bound;
  bool standalone = false;
  std::weak_ptr<void> standalone_owner;
  ListenDelivery* delivery = nullptr;  // Borrow of this actual synchronous Send frame.
  std::vector<std::shared_ptr<NativeListenSocketSubscription::State>> observers;
  std::vector<std::shared_ptr<rust::Box<RustListenCallback>>> cancelled_callbacks;
  bool inline_ready = false;
  bool notifying = false;
  std::shared_ptr<NativePacketSocketResources::State> scope;
  std::shared_ptr<rust::Box<RustListenSocket>> source;  // Role-local.
  std::vector<std::shared_ptr<PendingAccept>> pending;  // Owns every unclaimed product on the role.
  std::vector<std::weak_ptr<NativePacketSocketAccept>> creators;  // Actual pending creator tasks, metadata only.
};
namespace {
class RustListenSocketAdapter final : public webrtc::AsyncListenSocket {
 public:
  RustListenSocketAdapter(rust::Box<RustListenSocket> source, std::shared_ptr<NativeListenSocketEvents> events)
      : events_(std::move(events)) {
    events_->state->source = std::make_shared<rust::Box<RustListenSocket>>(std::move(source));
    std::lock_guard lock(events_->state->mutex);
    events_->state->socket = this;
    // This private facade is returned only to the pinned TCPPort creator, which
    // installs its ownership-taking receiver immediately after factory return.
    // The first actual role turn establishes that boundary, without a guessed
    // receiver count or an implicit pump. No standalone listener uses this gate.
    if (!events_->state->standalone) events_->state->role->PostTask([weak = events_->state->weak_from_this()] {
      if (auto state = weak.lock()) {
        std::lock_guard lock(state->mutex);
        if (state->role && state->socket) state->inline_ready = true;
      }
    });
  }
  ~RustListenSocketAdapter() override {
    auto state = events_->state;
    webrtc::Thread* role;
    { std::lock_guard lock(state->mutex); role = state->role; }
    if (!role) return;
    auto retire = [state] { state->Retire(); };
    if (role->IsCurrent()) retire(); else role->BlockingCall(std::move(retire));
  }
  State GetState() const override {
    auto source = events_->state->source;
    return source ? static_cast<State>(listen_socket_state(**source)) : State::kClosed;
  }
  webrtc::SocketAddress GetLocalAddress() const override {
    auto source = events_->state->source;
    return source ? listen_socket_local(**source)->value : webrtc::SocketAddress();
  }
 private:
  std::shared_ptr<NativeListenSocketEvents> events_;
};
}
bool NativeListenSocketEvents::State::HasOwner() const {
  return std::any_of(observers.begin(), observers.end(), [](const auto& observer) {
    return observer->ownership && observer->active && observer->bound;
  });
}
void NativeListenSocketEvents::State::PruneObservers() {
  if (notifying) return;
  std::erase_if(observers, [](const auto& observer) { return !observer->active && !observer->bound; });
}
void NativeListenSocketEvents::State::Attach(const std::shared_ptr<NativeListenSocketSubscription::State>& observer) {
  observer->bound = true;
  socket->SubscribeNewConnection(observer.get(), [weak = weak_from_this(), tag = observer](webrtc::AsyncListenSocket*, webrtc::AsyncPacketSocket* packet) {
    auto owner = weak.lock(); auto receiver = tag;
    if (!owner || !receiver || !receiver->active || !owner->delivery) return;
    auto callback = receiver->callback;
    auto& frame = *owner->delivery;
    if (!callback || !frame.provider) return;
    auto* actual = frame.claimed ? frame.core->socket.get() : frame.escrow.get();
    if (packet != actual) return;  // Exact product of this Send, no registry join.
    std::unique_ptr<NativeOwnedPacketSocket> owned;
    if (receiver->ownership) {
      if (frame.claimed || !frame.escrow) return;
      owned = std::make_unique<NativeOwnedPacketSocket>(frame.core);
      frame.core->socket = std::move(frame.escrow);
      frame.claimed = true;
    }
    if (!listen_callback_fire(**callback, **frame.provider, std::move(owned))) owner->Remove(receiver);
  });
}
bool NativeListenSocketEvents::State::Remove(const std::shared_ptr<NativeListenSocketSubscription::State>& observer) {
  webrtc::Thread* current_role;
  { std::lock_guard lock(mutex); if (!role || !role->IsCurrent()) return false; current_role = role; }
  if (!observer->active) return false;
  observer->active = false;
  auto callback = std::move(observer->callback);
  if (notifying) {
    if (callback) cancelled_callbacks.push_back(std::move(callback));
    return true;  // SDK Send cannot be mutated, including sibling removal.
  }
  SocketRoleRetirement::Run(current_role, [&] {
    if (observer->bound && socket) {
      observer->bound = false;
      socket->UnsubscribeNewConnection(observer.get());
    }
    PruneObservers();
    callback.reset();  // No container/native mutation spans user capture Drop.
  });
  return true;
}
void NativeListenSocketEvents::State::Finish(RustListenSocketAdapter* target) {
  delivery = nullptr;
  // The exact standalone frame pins target through SDK cleanup even if scope
  // retirement cleared the dispatch pointer. No user captures live in the SDK
  // tag after cancellation, and no native-list mutation runs during Send.
  do {
    for (const auto& observer : observers) if (!observer->active && observer->bound) {
      observer->bound = false;
      target->UnsubscribeNewConnection(observer.get());
    }
    std::erase_if(observers, [](const auto& observer) { return !observer->active && !observer->bound; });
    auto retired = std::move(cancelled_callbacks);
    cancelled_callbacks.clear();
    retired.clear();
  } while (!cancelled_callbacks.empty() || std::any_of(observers.begin(), observers.end(), [](const auto& observer) { return !observer->active && observer->bound; }));
  notifying = false;
}
void NativeListenSocketEvents::State::Run(const std::shared_ptr<PendingAccept>& item) {
  auto owner_frame = standalone_owner.lock();
  {
    std::lock_guard lock(mutex);
    if (!role || !socket || !OnRole()) return;
  }
  auto provider = source;
  if (!provider || listen_socket_failed(**provider)) {
    std::uint8_t expected = 0;
    item->ticket->status.compare_exchange_strong(expected, 5);
    Prune();
    return;
  }
  std::uint8_t expected = 0;
  if (!item->ticket->status.compare_exchange_strong(expected, 1)) { Prune(); return; }
  auto accepted = std::move(item->socket);
  Prune();
  // Pruning can destroy providers and the listener. Recheck all loans after it.
  if (!scope->CurrentRole()) { item->ticket->status.store(4); return; }
  if (listen_socket_failed(**provider)) { item->ticket->status.store(5); return; }
  RustListenSocketAdapter* target;
  {
    std::lock_guard lock(mutex);
    if (!role || !socket) { item->ticket->status.store(4); return; }
    target = socket;
  }
  if (!standalone) {
    // No public SDK seam supplies TCPPort's actual Network IP membership here.
    // Its later TCPConnection ctor repeatedly calls a fallible live getter and
    // DCHECKs membership. Never guess a bind-IP join, freeze getter metadata or
    // transfer an unguardable custom socket into that consumer.
    item->ticket->status.store(6);
    return;
  }
  if (!HasOwner()) { item->ticket->status.store(3); return; }
  notifying = true;
  struct Notification {
    State& state;
    RustListenSocketAdapter* target;
    ~Notification() { state.Finish(target); }
  } notification{*this, target};
  auto core = std::move(accepted->standalone);
  ListenDelivery frame{core, core->events->state->source, std::move(core->socket)};
  if (!frame.provider || packet_socket_source_failed(**frame.provider)) {
    core->socket = std::move(frame.escrow);
    RetireOwnedPacket(core);
    item->ticket->status.store(5);
    return;
  }
  // Keep linear ownership in escrow through passive native receivers. Only the
  // ownership-taking receiver may move it to the delivered owning shell.
  delivery = &frame;
  target->NotifyNewConnection(target, frame.escrow.get());
  delivery = nullptr;
  if (frame.claimed) item->ticket->status.store(2);
  else {
    core->socket = std::move(frame.escrow);
    RetireOwnedPacket(core);  // Active child frames still pin the actual facade.
    item->ticket->status.store(role ? 3 : 4);
  }
}
NativeListenSocketEvents::NativeListenSocketEvents(std::shared_ptr<State> value) noexcept : state(std::move(value)) {}
std::shared_ptr<NativeListenSocketEvents> listen_socket_events_in_scope(std::shared_ptr<NativePacketSocketResources> resources, bool standalone) noexcept {
  auto* role = resources ? resources->state->CurrentRole() : nullptr;
  if (!role) return nullptr;
  auto state = std::make_shared<NativeListenSocketEvents::State>(role, resources->state);
  state->standalone = standalone;  // Fixed before any creator can publish aliases.
  if (!resources->state->Enroll(state)) { state->Retire(); return nullptr; }
  return std::make_shared<NativeListenSocketEvents>(std::move(state));
}
void listen_socket_events_retire(const NativeListenSocketEvents& events) noexcept { events.state->Retire(); }
std::unique_ptr<webrtc::AsyncListenSocket> listen_socket_adapter(rust::Box<RustListenSocket> source,
    std::shared_ptr<NativeListenSocketEvents> events) noexcept {
  auto* role = events ? events->state->scope->CurrentRole() : nullptr;
  if (!role) return nullptr;
  return std::make_unique<RustListenSocketAdapter>(std::move(source), std::move(events));
}
NativeAcceptedPacketSocket::NativeAcceptedPacketSocket(std::unique_ptr<webrtc::AsyncPacketSocket> product,
    std::shared_ptr<NativePacketSocketResources::State> resources) noexcept : socket(std::move(product)), scope(std::move(resources)) {}
NativeAcceptedPacketSocket::NativeAcceptedPacketSocket(std::shared_ptr<NativeOwnedPacketSocket::Core> product,
    std::shared_ptr<NativePacketSocketResources::State> resources) noexcept : standalone(std::move(product)), scope(std::move(resources)) {}
NativeAcceptedPacketSocket::~NativeAcceptedPacketSocket() { RetireOwnedPacket(std::move(standalone)); }
std::unique_ptr<NativeAcceptedPacketSocket> owned_socket_into_accepted(std::unique_ptr<NativeOwnedPacketSocket> socket,
    std::shared_ptr<NativePacketSocketResources> listener) noexcept {
  if (!socket || !socket->core || !listener) return nullptr;
  auto* role = listener->state->CurrentRole();
  if (!role || socket->core->events->state->role != role) return nullptr;
  return std::make_unique<NativeAcceptedPacketSocket>(std::move(socket->core), listener->state);
}
std::unique_ptr<NativeAcceptedPacketSocket> accepted_socket_new(rust::Box<RustPacketSocket> source,
    std::shared_ptr<NativePacketSocketEvents> events, std::shared_ptr<NativePacketSocketResources> resources) noexcept {
  if (!resources || !resources->state->CurrentRole()) return nullptr;
  auto product = packet_socket_adapter(std::move(source), std::move(events));
  if (!product) return nullptr;
  return std::make_unique<NativeAcceptedPacketSocket>(std::move(product), resources->state);
}
FfiSocketAcceptResult ListenAccept(const NativeListenSocketEvents& events,
    std::unique_ptr<NativeAcceptedPacketSocket> socket, bool post, std::shared_ptr<NativePacketSocketAccept> ticket) noexcept {
  auto state = events.state;
  auto reject = [&](std::uint8_t code) { return FfiSocketAcceptResult{code, {}, std::move(socket)}; };
  // Pruning drops cancelled products and can reenter/retire the listener.
  // Recheck every admission edge afterwards, before storing/posting new work.
  {
    std::lock_guard lock(state->scope->mutex);
    if (state->scope->closing || !state->scope->role) return reject(1);
    auto* queue = webrtc::TaskQueueBase::Current();
    if (!state->scope->role->IsCurrent() || (queue && queue != state->scope->role)) return reject(3);
  }
  state->Prune();
  const auto scope_status = [&] {
    std::lock_guard lock(state->scope->mutex);
    if (state->scope->closing || !state->scope->role) return std::uint8_t{1};
    auto* queue = webrtc::TaskQueueBase::Current();
    return state->scope->role->IsCurrent() && (!queue || queue == state->scope->role) ? std::uint8_t{0} : std::uint8_t{3};
  }();
  if (scope_status != 0) return reject(scope_status);
  {
    std::lock_guard lock(state->mutex);
    if (!state->role) return reject(1);
    if (!state->socket || (!state->standalone && !post && !state->inline_ready)) return reject(2);
    if (!post && state->notifying) return reject(4);
  }
  if (socket->scope != state->scope || state->standalone != static_cast<bool>(socket->standalone)) return reject(9);
  if (!state->standalone) return reject(13);
  if (state->standalone && !state->HasOwner()) return reject(2);
  auto provider = state->source;
  if (!provider || listen_socket_failed(**provider)) return reject(5);
  if (ticket && ticket->status.load() != 0) return reject(4);
  auto pending = std::make_shared<PendingAccept>();
  if (ticket) pending->ticket = std::move(ticket);
  pending->socket = std::move(socket);
  state->pending.push_back(pending);
  if (!post) {
    state->Run(pending);
    return {0, pending->ticket, {}};
  }
  {
    std::lock_guard lock(state->mutex);
    state->role->PostTask([weak = state->weak_from_this(), request = std::weak_ptr<PendingAccept>(pending)] {
      auto owner = weak.lock(); auto item = request.lock();
      if (owner && item) owner->Run(item);
    });
  }
  return {0, pending->ticket, {}};
}
FfiSocketAcceptResult listen_accept(const NativeListenSocketEvents& events,
    std::unique_ptr<NativeAcceptedPacketSocket> socket, bool post) noexcept {
  return ListenAccept(events, std::move(socket), post, nullptr);
}
FfiSocketAcceptResult listen_accept_prepared(const NativeListenSocketEvents& events,
    std::unique_ptr<NativeAcceptedPacketSocket> socket, std::shared_ptr<NativePacketSocketAccept> ticket) noexcept {
  return ListenAccept(events, std::move(socket), true, std::move(ticket));
}
struct NativeOwnedListenSocket::Core {
  explicit Core(rust::Box<RustSocketRole> role) : role(std::move(role)) {}
  rust::Box<RustSocketRole> role;
  std::unique_ptr<NativePacketSocketScope> scope;
  std::shared_ptr<NativeListenSocketEvents> events;
  std::unique_ptr<webrtc::AsyncListenSocket> socket;
};
NativeOwnedListenSocket::NativeOwnedListenSocket(std::shared_ptr<Core> value) noexcept : core(std::move(value)) {}
NativeOwnedListenSocket::~NativeOwnedListenSocket() {
  auto owner = std::move(core);
  if (!owner) return;
  webrtc::Thread* role;
  { std::lock_guard lock(owner->events->state->mutex); role = owner->events->state->role; }
  if (role && role->IsCurrent()) SocketRoleRetirement::Run(role, [&] { owner->scope.reset(); owner.reset(); });
  else owner->scope.reset();
}
std::unique_ptr<NativeOwnedListenSocket> owned_listen_socket_new(rust::Box<RustListenSocket> source,
    std::shared_ptr<NativeListenSocketEvents> events, std::unique_ptr<NativePacketSocketScope> scope,
    rust::Box<RustSocketRole> role) noexcept {
  if (!events || !scope || !scope->resources()->state->CurrentRole()) return nullptr;
  if (!events->state->standalone || events->state->scope != scope->resources()->state) return nullptr;
  auto core = std::make_shared<NativeOwnedListenSocket::Core>(std::move(role));
  core->scope = std::move(scope); core->events = std::move(events);
  core->socket = listen_socket_adapter(std::move(source), core->events);
  if (!core->socket) return nullptr;
  core->events->state->standalone_owner = core;
  return std::make_unique<NativeOwnedListenSocket>(std::move(core));
}
FfiOwnedSocketResult listen_member_call(const NativeListenSocketEvents& events, bool address) noexcept {
  auto state = events.state;
  auto owner_frame = state->standalone_owner.lock();
  FfiOwnedSocketResult result{};
  RustListenSocketAdapter* target;
  { std::lock_guard lock(state->mutex);
    if (!state->role) { result.status = 1; return result; }
    if (!state->OnRole()) { result.status = 3; return result; }
    if (!state->socket) { result.status = 2; return result; }
    target = state->socket;
  }
  auto provider = state->source;
  if (!provider || listen_socket_failed(**provider)) { result.status = 5; return result; }
  if (address) result.address = std::make_unique<NativeSocketAddress>(target->GetLocalAddress());
  else result.code = static_cast<std::int32_t>(target->GetState());
  if (listen_socket_failed(**provider)) result.status = 5;
  return result;
}
NativeListenSocketSubscription::NativeListenSocketSubscription(std::shared_ptr<NativeListenSocketEvents::State> owner,
    std::shared_ptr<State> state) noexcept : owner_(std::move(owner)), state_(std::move(state)) {}
NativeListenSocketSubscription::~NativeListenSocketSubscription() { Cancel(); }
bool NativeListenSocketSubscription::Cancel() const noexcept { return owner_->Remove(state_); }
bool NativeListenSocketSubscription::Active() const noexcept { return state_->active; }
FfiListenSubscriptionResult listen_observe(const NativeListenSocketEvents& events, bool ownership,
    rust::Box<RustListenCallback> callback) noexcept {
  auto state = events.state;
  { std::lock_guard lock(state->mutex);
    if (!state->role) return {1, {}};
    if (!state->OnRole()) return {3, {}};
    if (!state->socket) return {2, {}};
    if (!state->standalone) return {9, {}};
    if (state->notifying) return {4, {}};
  }
  if (ownership && state->HasOwner()) return {12, {}};
  auto observer = std::make_shared<NativeListenSocketSubscription::State>(ownership, std::move(callback));
  state->PruneObservers();
  state->observers.push_back(observer);
  state->Attach(observer);
  return {0, std::make_unique<NativeListenSocketSubscription>(std::move(state), std::move(observer))};
}
bool listen_subscription_cancel(const NativeListenSocketSubscription& subscription) noexcept { return subscription.Cancel(); }
bool listen_subscription_active(const NativeListenSocketSubscription& subscription) noexcept { return subscription.Active(); }
void listen_accept_creation_failed(const NativePacketSocketAccept& ticket, bool retired) noexcept {
  std::uint8_t expected = 0;
  ticket.status.compare_exchange_strong(expected, retired ? 4 : 5);
}
struct PendingAcceptCreator {
  explicit PendingAcceptCreator(rust::Box<RustAcceptCreator> creator) : creator(std::move(creator)) {}
  ~PendingAcceptCreator() {
    if (!handed_off) { std::uint8_t expected = 0; ticket->status.compare_exchange_strong(expected, 4); }
  }
  std::shared_ptr<NativePacketSocketAccept> ticket = std::make_shared<NativePacketSocketAccept>();
  rust::Box<RustAcceptCreator> creator;  // Send capture; no role-local product.
  bool handed_off = false;
};
std::shared_ptr<NativePacketSocketAccept> listen_post_creator(const NativeListenSocketEvents& events,
    rust::Box<RustAcceptCreator> creator) noexcept {
  auto request = std::make_shared<PendingAcceptCreator>(std::move(creator));
  auto state = events.state;
  std::lock_guard lock(state->mutex);
  if (!state->role) { request->ticket->status.store(4); return request->ticket; }
  if (!state->standalone) { request->ticket->status.store(6); return request->ticket; }
  std::erase_if(state->creators, [](const auto& weak) { auto ticket = weak.lock(); return !ticket || ticket->status.load() != 0; });
  state->creators.push_back(request->ticket);
  state->role->PostTask([weak = state->weak_from_this(), request] {
    auto owner = weak.lock();
    if (!owner) return;
    auto owner_frame = owner->standalone_owner.lock();
    auto erase_request = [&] {
      std::lock_guard lock(owner->mutex);
      std::erase_if(owner->creators, [&](const auto& weak) { auto ticket = weak.lock(); return !ticket || ticket == request->ticket; });
    };
    if (request->ticket->status.load() != 0) { erase_request(); return; }
    if (!owner->OnRole() || !owner->socket) { erase_request(); return; }
    auto source = owner->source;
    if (!source || listen_socket_failed(**source)) {
      listen_accept_creation_failed(*request->ticket, false);
      erase_request(); return;
    }
    request->handed_off = listen_create_accepted(**source, std::move(request->creator), request->ticket);
    erase_request();
  });
  return request->ticket;
}
std::uint8_t listen_accept_status(const NativePacketSocketAccept& ticket) noexcept { return ticket.status.load(); }
bool listen_accept_cancel(const NativePacketSocketAccept& ticket) noexcept {
  std::uint8_t expected = 0;
  return ticket.status.compare_exchange_strong(expected, 3);
}

namespace {
FfiSocketTcpOptions SocketTcpOptions(const webrtc::PacketSocketTcpOptions& value) {
  FfiSocketTcpOptions result{};
  result.options = value.opts;
  const auto copy = [](const std::vector<std::string>& input, rust::Vec<FfiRtpByteString>& output) {
    for (const auto& item : input) {
      FfiRtpByteString bytes;
      for (unsigned char byte : item) bytes.bytes.push_back(byte);
      output.push_back(std::move(bytes));
    }
  };
  copy(value.tls_alpn_protocols, result.alpn);
  copy(value.tls_elliptic_curves, result.curves);
  // Only binding-created PeerConnection dependencies can reach this private
  // installation. Retain the actual backing while the SDK proxy is live,
  // before entering Rust or running any synchronous creation-time verification.
  result.verifier = retain_binding_peer_tls_verifier(value.tls_cert_verifier);
  return result;
}
struct PacketConstruction {
  std::shared_ptr<NativePacketSocketEvents> events;
  bool bound = false;
  ~PacketConstruction() { if (!bound) packet_socket_events_retire(*events); }
};
struct ListenConstruction {
  std::shared_ptr<NativeListenSocketEvents> events;
  bool bound = false;
  ~ListenConstruction() { if (!bound) listen_socket_events_retire(*events); }
};
struct FactorySource {
  explicit FactorySource(rust::Box<RustPacketSocketFactory> value) : source(std::move(value)) {}
  rust::Box<RustPacketSocketFactory> source;
};
class GeneralSocketFactory final : public webrtc::PacketSocketFactory {
 public:
  GeneralSocketFactory(rust::Box<RustPacketSocketFactory> source, const webrtc::Environment& environment,
                       std::unique_ptr<NativePacketSocketScope> scope)
      : source_(std::make_shared<FactorySource>(std::move(source))), environment_(environment), scope_(std::move(scope)) {}
  std::unique_ptr<webrtc::AsyncPacketSocket> CreateUdpSocket(const webrtc::Environment& env,
      const webrtc::SocketAddress& address, std::uint16_t min, std::uint16_t max) override {
    return CreatePacket(env, 0, address, nullptr, min, max, {});
  }
  std::unique_ptr<webrtc::AsyncPacketSocket> CreateClientTcpSocket(const webrtc::Environment& env,
      const webrtc::SocketAddress& local, const webrtc::SocketAddress& remote,
      const webrtc::PacketSocketTcpOptions& options) override {
    return CreatePacket(env, 1, local, &remote, 0, 0, SocketTcpOptions(options));
  }
  std::unique_ptr<webrtc::AsyncPacketSocket> CreateClientUdpSocket(const webrtc::Environment& env,
      const webrtc::SocketAddress& local, const webrtc::SocketAddress& remote,
      std::uint16_t min, std::uint16_t max, const webrtc::PacketSocketTcpOptions& options) override {
    return CreatePacket(env, 2, local, &remote, min, max, SocketTcpOptions(options));
  }
  std::unique_ptr<webrtc::AsyncListenSocket> CreateServerTcpSocket(const webrtc::Environment& env,
      const webrtc::SocketAddress& address, std::uint16_t min, std::uint16_t max, int options) override {
    auto source = source_;
    auto resources = scope_->resources();
    auto events = listen_socket_events_in_scope(resources);
    if (!events) return nullptr;
    ListenConstruction construction{events};
    FfiSocketCreateRequest request{};
    request.local = std::make_unique<NativeSocketAddress>(address);
    request.min_port = min; request.max_port = max; request.tcp.options = options;
    auto product = socket_factory_create_listener(*source->source, retain_environment_from_sdk(env),
        resources, events, std::move(request));
    if (!listen_socket_present(*product)) return nullptr;
    auto result = listen_socket_adapter(std::move(product), events);
    construction.bound = result != nullptr;
    return result;
  }
  std::unique_ptr<webrtc::AsyncDnsResolverInterface> CreateAsyncDnsResolver() override {
    auto source = source_;
    auto environment = retain_environment_from_sdk(environment_);
    const bool admitted = scope_->resources()->state->CurrentRole() != nullptr;
    return async_dns_resolver_adapter(socket_factory_create_dns(*source->source, std::move(environment), admitted));
  }
 private:
  std::unique_ptr<webrtc::AsyncPacketSocket> CreatePacket(const webrtc::Environment& env, std::uint8_t kind,
      const webrtc::SocketAddress& local, const webrtc::SocketAddress* remote,
      std::uint16_t min, std::uint16_t max, FfiSocketTcpOptions options) {
    auto source = source_;
    auto resources = scope_->resources();
    auto events = packet_socket_events_in_scope(resources);
    if (!events) return nullptr;
    PacketConstruction construction{events};
    FfiSocketCreateRequest request{};
    request.kind = kind; request.local = std::make_unique<NativeSocketAddress>(local);
    if (remote) request.remote = std::make_unique<NativeSocketAddress>(*remote);
    request.min_port = min; request.max_port = max; request.tcp = std::move(options);
    auto product = socket_factory_create_packet(*source->source, retain_environment_from_sdk(env),
        resources, events, std::move(request));
    if (!packet_socket_present(*product)) return nullptr;
    auto result = packet_socket_adapter(std::move(product), events);
    construction.bound = result != nullptr;
    return result;
  }
  // Destruction order is intentional: scope retirement (including rejected
  // construction) precedes environment and factory recipe release. Products
  // and active Rust entry frames retain their own recipe/environment owners.
  std::shared_ptr<FactorySource> source_;
  webrtc::Environment environment_;
  std::unique_ptr<NativePacketSocketScope> scope_;
};
}  // namespace
std::unique_ptr<webrtc::PacketSocketFactory> socket_factory_adapter(rust::Box<RustPacketSocketFactory> source,
    const webrtc::Environment& environment, webrtc::Thread* network_role) noexcept {
  if (!socket_factory_present(*source) || !network_role) return nullptr;
  std::unique_ptr<webrtc::PacketSocketFactory> result;
  auto create = [&] {
    auto scope = packet_socket_scope_new();
    if (scope) result = std::make_unique<GeneralSocketFactory>(std::move(source), environment, std::move(scope));
  };
  if (network_role->IsCurrent()) create(); else network_role->BlockingCall(create);
  return result;
}
}  // namespace pulsebeam::webrtc_sys
