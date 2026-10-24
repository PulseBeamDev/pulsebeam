#include "pulsebeam-webrtc-sys/native/live_frame_transformer.h"

#include <atomic>
#include <algorithm>
#include "api/peer_connection_interface.h"
#include "api/rtp_sender_interface.h"
#include "api/rtp_receiver_interface.h"
#include "pulsebeam-webrtc-sys/native/peer.h"
#include "pulsebeam-webrtc-sys/native/video.h"
#include <map>
#include <mutex>
#include <optional>
#include <utility>
#include <vector>
#include "api/make_ref_counted.h"
#include "pulsebeam-webrtc-sys/native/frame_callback_slot.h"
#include "pulsebeam-webrtc-sys/native/frame_transformer.h"
#include "pulsebeam-webrtc-sys/native/video_metadata.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {
namespace {
void Count(std::atomic<std::uint64_t>& counter) {
  auto value = counter.load(std::memory_order_relaxed);
  while (value != UINT64_MAX && !counter.compare_exchange_weak(value, value + 1, std::memory_order_relaxed)) {}
}
struct Counts {
  std::atomic<std::uint64_t> delivered{0}, completed{0}, abandoned{0}, late{0}, capacity{0}, wrong_frame{0}, callback_forwards{0}, late_callback_forwards{0};
};
struct ProviderLifecycle {
  explicit ProviderLifecycle(std::size_t limit) : limit(limit) {}
  const std::size_t limit;
  std::atomic<std::size_t> in_flight{0};
  std::atomic<bool> closed{false};
  Counts counts;
};
}  // namespace
struct NativeFrameTransformer::State {
  State(rust::Box<RustFrameTransformerProvider> provider, std::size_t limit)
      : provider(std::move(provider)), lifecycle(std::make_shared<ProviderLifecycle>(limit)) {}
  rust::Box<RustFrameTransformerProvider> provider;
  std::shared_ptr<ProviderLifecycle> lifecycle;
};
namespace {
struct Generation {
  explicit Generation(std::shared_ptr<ProviderLifecycle> provider) : provider(std::move(provider)) {}
  // Completion/queued frame provenance must not retain callback userdata:
  // userdata may itself own its asynchronous frame/completion queue.
  std::shared_ptr<ProviderLifecycle> provider;
  std::atomic<bool> live{true};
};
struct Selection {
  explicit Selection(std::shared_ptr<NativeFrameTransformer::State> provider)
      : provider(std::move(provider)), generation(std::make_shared<Generation>(this->provider->lifecycle)) {}
  std::shared_ptr<NativeFrameTransformer::State> provider;
  std::shared_ptr<Generation> generation;
};
struct Route {
  std::shared_ptr<StockFrameCallbackSlot> sdk;
  std::shared_ptr<StockFrameCallbackSlot> output;
  std::optional<std::uint32_t> key;
};
}  // namespace
struct FrameTransformCallbackState {
  std::shared_ptr<Generation> generation;
  std::shared_ptr<StockFrameCallbackSlot> output;
  FfiFrameTransformBinding binding;
  bool Live() const {
    return generation->live.load(std::memory_order_acquire) &&
           !generation->provider->closed.load(std::memory_order_acquire) && output->IsLive();
  }
};
// Exact native producer/request lineage, not an SSRC/source-ID join. SDK clones
// preserve this marker and their actual native media/direction semantics.
struct FrameTransformProvenance : FrameTransformCallbackState {
  std::atomic<bool> consumed{false};
  bool Finish() {
    if (consumed.exchange(true, std::memory_order_acq_rel)) return false;
    generation->provider->in_flight.fetch_sub(1, std::memory_order_acq_rel);
    return true;
  }
};
struct NativeFrameTransformerHost::State {
  State(bool audio, bool sender, webrtc::scoped_refptr<webrtc::FrameTransformerInterface> metadata)
      : audio(audio), sender(sender), metadata(std::move(metadata)) {}
  const bool audio;
  const bool sender;
  const webrtc::scoped_refptr<webrtc::FrameTransformerInterface> metadata;
  std::mutex mutex;
  std::map<std::optional<std::uint32_t>, std::shared_ptr<Route>> routes;
  std::shared_ptr<Selection> selected;
  webrtc::scoped_refptr<webrtc::FrameTransformerInterface> collector;
};
namespace {
std::unique_ptr<NativeFrameTransformCallback> Callback(
    const std::shared_ptr<NativeFrameTransformerHost::State>& state,
    const std::shared_ptr<Selection>& selected, const std::shared_ptr<Route>& route) {
  auto callback = std::make_shared<FrameTransformCallbackState>();
  callback->generation = selected->generation;
  callback->output = route->output;
  callback->binding = {state->audio, state->sender, route->key.has_value(), route->key.value_or(0)};
  return std::make_unique<NativeFrameTransformCallback>(std::move(callback));
}
void Notify(const std::shared_ptr<NativeFrameTransformerHost::State>& state,
    const std::shared_ptr<Selection>& selected, const std::shared_ptr<Route>& route, bool registered) {
  if (selected) frame_transformer_provider_binding(*selected->provider->provider, Callback(state, selected, route), registered);
}
std::uint8_t CheckFrame(const std::unique_ptr<NativeTransformableFrame>& frame, const FfiFrameTransformBinding& binding) {
  using Direction = webrtc::TransformableFrameInterface::Direction;
  // Native consumers branch on the preserved stock role before concrete casts.
  if (!frame || !frame->provenance() || frame->audio() != binding.audio ||
      (frame->frame().GetDirection() != Direction::kSender && frame->frame().GetDirection() != Direction::kReceiver)) return 2;
  if (frame->audio() ? !transformable_audio_completion_safe(*frame, binding.sender)
                     : !transformable_video_completion_safe(*frame, binding.sender)) return 4;
  return 0;
}
class SlotForwarder : public webrtc::TransformedFrameCallback {
 public:
  explicit SlotForwarder(std::shared_ptr<StockFrameCallbackSlot> slot) : slot_(std::move(slot)) {}
  void OnTransformedFrame(std::unique_ptr<webrtc::TransformableFrameInterface> frame) override { slot_->OnTransformedFrame(std::move(frame)); }
  void StartShortCircuiting() override { slot_->StartShortCircuiting(); }
 private:
  std::shared_ptr<StockFrameCallbackSlot> slot_;
};
// This is the stage after caller completion. It consults the actual current
// collector, whose SDK callback registrations are made on its owner role.
class OutputStage : public webrtc::TransformedFrameCallback {
 public:
  OutputStage(std::weak_ptr<NativeFrameTransformerHost::State> state, std::shared_ptr<StockFrameCallbackSlot> sdk)
      : state_(std::move(state)), sdk_(std::move(sdk)) {}
  void OnTransformedFrame(std::unique_ptr<webrtc::TransformableFrameInterface> frame) override {
    auto state = state_.lock();
    if (!state) return;
    webrtc::scoped_refptr<webrtc::FrameTransformerInterface> collector;
    { std::lock_guard lock(state->mutex); collector = state->collector; }
    if (collector) collector->Transform(std::move(frame)); else sdk_->OnTransformedFrame(std::move(frame));
  }
  void StartShortCircuiting() override { sdk_->StartShortCircuiting(); }
 private:
  std::weak_ptr<NativeFrameTransformerHost::State> state_;
  std::shared_ptr<StockFrameCallbackSlot> sdk_;
};
void Dispatch(const std::shared_ptr<NativeFrameTransformerHost::State>& state,
    const std::shared_ptr<Route>& route, std::unique_ptr<webrtc::TransformableFrameInterface> frame) {
  std::shared_ptr<Selection> selected;
  { std::lock_guard lock(state->mutex); selected = state->selected; }
  if (!selected || !selected->generation->live.load(std::memory_order_acquire) || selected->provider->lifecycle->closed.load(std::memory_order_acquire)) {
    route->output->OnTransformedFrame(std::move(frame));
    return;
  }
  auto provider = selected->provider->lifecycle;
  auto active = provider->in_flight.load(std::memory_order_acquire);
  do {
    if (active >= provider->limit) { Count(provider->counts.capacity); return; }
  } while (!provider->in_flight.compare_exchange_weak(active, active + 1, std::memory_order_acq_rel));
  auto request = std::make_shared<FrameTransformProvenance>();
  request->generation = selected->generation;
  request->output = route->output;
  request->binding = {state->audio, state->sender, route->key.has_value(), route->key.value_or(0)};
  const auto expected = state->sender ? webrtc::TransformableFrameInterface::Direction::kSender : webrtc::TransformableFrameInterface::Direction::kReceiver;
  if (!frame || frame->GetDirection() != expected) {
    request->Finish(); Count(provider->counts.wrong_frame); return;
  }
  webrtc::VideoCodecType codec = webrtc::kVideoCodecGeneric;
  if (!state->audio) codec = static_cast<const webrtc::TransformableVideoFrameInterface&>(*frame).header().codec;
  auto owned = std::make_unique<NativeTransformableFrame>(std::move(frame), state->audio, codec);
  owned->set_provenance(request);
  Count(provider->counts.delivered);
  frame_transformer_provider_transform(*selected->provider->provider, std::move(owned), std::make_unique<NativeFrameTransformCompletion>(std::move(request)));
}
// Metadata is applied on the actual encoder queue before calling Rust. A
// retained completion never reenters the metadata transformer's queue lookup.
class MetadataStage : public webrtc::TransformedFrameCallback {
 public:
  MetadataStage(std::weak_ptr<NativeFrameTransformerHost::State> state, std::shared_ptr<Route> route)
      : state_(std::move(state)), route_(std::move(route)) {}
  void OnTransformedFrame(std::unique_ptr<webrtc::TransformableFrameInterface> frame) override {
    if (auto state = state_.lock()) Dispatch(state, route_, std::move(frame));
  }
  void StartShortCircuiting() override { route_->sdk->StartShortCircuiting(); }
 private:
  std::weak_ptr<NativeFrameTransformerHost::State> state_;
  std::shared_ptr<Route> route_;
};
void Register(webrtc::FrameTransformerInterface& transformer, const std::shared_ptr<Route>& route,
    webrtc::scoped_refptr<webrtc::TransformedFrameCallback> callback) {
  if (route->key) transformer.RegisterTransformedFrameSinkCallback(std::move(callback), *route->key);
  else transformer.RegisterTransformedFrameCallback(std::move(callback));
}
void Unregister(webrtc::FrameTransformerInterface& transformer, const std::shared_ptr<Route>& route) {
  if (route->key) transformer.UnregisterTransformedFrameSinkCallback(*route->key);
  else transformer.UnregisterTransformedFrameCallback();
}
void RegisterRoute(const std::shared_ptr<NativeFrameTransformerHost::State>& state,
    webrtc::scoped_refptr<webrtc::TransformedFrameCallback> callback, std::optional<std::uint32_t> key) {
  auto route = std::make_shared<Route>();
  route->key = key;
  route->sdk = std::make_shared<StockFrameCallbackSlot>(std::move(callback));
  route->output = std::make_shared<StockFrameCallbackSlot>(webrtc::make_ref_counted<OutputStage>(state, route->sdk));
  std::shared_ptr<Route> previous;
  std::shared_ptr<Selection> selected;
  webrtc::scoped_refptr<webrtc::FrameTransformerInterface> collector;
  {
    std::lock_guard lock(state->mutex);
    previous = std::exchange(state->routes[key], route);
    collector = state->collector;
    selected = state->selected;
  }
  if (previous) { previous->output->Retire(); previous->sdk->Retire(); Notify(state, selected, previous, false); }
  if (collector) Register(*collector, route, webrtc::make_ref_counted<SlotForwarder>(route->sdk));
  if (state->metadata) Register(*state->metadata, route, webrtc::make_ref_counted<MetadataStage>(state, route));
  Notify(state, selected, route, true);
}
void UnregisterRoute(const std::shared_ptr<NativeFrameTransformerHost::State>& state, std::optional<std::uint32_t> key) {
  std::shared_ptr<Route> route;
  std::shared_ptr<Selection> selected;
  webrtc::scoped_refptr<webrtc::FrameTransformerInterface> collector;
  {
    std::lock_guard lock(state->mutex);
    auto it = state->routes.find(key);
    if (it == state->routes.end()) return;
    route = std::move(it->second); state->routes.erase(it);
    collector = state->collector;
    selected = state->selected;
  }
  if (state->metadata) Unregister(*state->metadata, route);
  if (collector) Unregister(*collector, route);
  route->output->Retire();
  route->sdk->Retire();
  Notify(state, selected, route, false);
}
}  // namespace
NativeFrameTransformer::~NativeFrameTransformer() = default;
NativeFrameTransformCompletion::~NativeFrameTransformCompletion() {
  if (request_ && request_->Finish()) Count(request_->generation->provider->counts.abandoned);
}
NativeFrameTransformerHost::NativeFrameTransformerHost(bool audio, bool sender,
    webrtc::scoped_refptr<webrtc::FrameTransformerInterface> metadata)
    : state_(std::make_shared<State>(audio, sender, std::move(metadata))) {}
NativeFrameTransformerHost::~NativeFrameTransformerHost() = default;
void NativeFrameTransformerHost::SetCaller(const NativeFrameTransformer* provider) {
  std::shared_ptr<Selection> replacement = provider ? std::make_shared<Selection>(provider->state()) : nullptr;
  std::shared_ptr<Selection> previous;
  std::vector<std::shared_ptr<Route>> routes;
  {
    std::lock_guard lock(state_->mutex);
    if ((!state_->selected && !replacement) || (state_->selected && replacement && state_->selected->provider == replacement->provider)) return;
    previous = std::exchange(state_->selected, replacement);
    if (previous) previous->generation->live.store(false, std::memory_order_release);
    for (const auto& [key, route] : state_->routes) routes.push_back(route);
  }
  for (const auto& route : routes) Notify(state_, previous, route, false);
  for (const auto& route : routes) Notify(state_, replacement, route, true);
}
void NativeFrameTransformerHost::SetCollector(webrtc::scoped_refptr<webrtc::FrameTransformerInterface> collector) {
  std::vector<std::shared_ptr<Route>> routes;
  {
    std::lock_guard lock(state_->mutex);
    if (state_->collector == collector) return;
    for (const auto& [key, route] : state_->routes) routes.push_back(route);
  }
  if (collector) for (const auto& route : routes) Register(*collector, route, webrtc::make_ref_counted<SlotForwarder>(route->sdk));
  webrtc::scoped_refptr<webrtc::FrameTransformerInterface> previous;
  { std::lock_guard lock(state_->mutex); previous = std::exchange(state_->collector, std::move(collector)); }
  if (previous) for (const auto& route : routes) Unregister(*previous, route);
}
void NativeFrameTransformerHost::Transform(std::unique_ptr<webrtc::TransformableFrameInterface> frame) {
  std::shared_ptr<Route> route;
  {
    std::lock_guard lock(state_->mutex);
    auto it = state_->routes.find(frame->GetSsrc());
    if (it == state_->routes.end()) it = state_->routes.find(std::nullopt);
    if (it != state_->routes.end()) route = it->second;
  }
  if (!route) return;
  if (state_->metadata) state_->metadata->Transform(std::move(frame));
  else Dispatch(state_, route, std::move(frame));
}
void NativeFrameTransformerHost::RegisterTransformedFrameCallback(webrtc::scoped_refptr<webrtc::TransformedFrameCallback> callback) { RegisterRoute(state_, std::move(callback), std::nullopt); }
void NativeFrameTransformerHost::UnregisterTransformedFrameCallback() { UnregisterRoute(state_, std::nullopt); }
void NativeFrameTransformerHost::RegisterTransformedFrameSinkCallback(webrtc::scoped_refptr<webrtc::TransformedFrameCallback> callback, std::uint32_t ssrc) { RegisterRoute(state_, std::move(callback), ssrc); }
void NativeFrameTransformerHost::UnregisterTransformedFrameSinkCallback(std::uint32_t ssrc) { UnregisterRoute(state_, ssrc); }
std::unique_ptr<NativeFrameTransformer> frame_transformer_new(rust::Box<RustFrameTransformerProvider> provider, std::size_t limit) noexcept {
  return std::make_unique<NativeFrameTransformer>(std::make_shared<NativeFrameTransformer::State>(std::move(provider), limit));
}
std::unique_ptr<NativeFrameTransformer> frame_transformer_copy(const NativeFrameTransformer& value) noexcept { return std::make_unique<NativeFrameTransformer>(value.state()); }
bool frame_transformer_same(const NativeFrameTransformer& a, const NativeFrameTransformer& b) noexcept { return a.state() == b.state(); }
void frame_transformer_close(const NativeFrameTransformer& value) noexcept { value.state()->lifecycle->closed.store(true, std::memory_order_release); }
bool frame_transformer_closed(const NativeFrameTransformer& value) noexcept { return value.state()->lifecycle->closed.load(std::memory_order_acquire); }
FfiFrameTransformerStatistics frame_transformer_statistics(const NativeFrameTransformer& value) noexcept {
  const auto& state = *value.state()->lifecycle;
  return {state.in_flight.load(), state.counts.delivered.load(), state.counts.completed.load(), state.counts.abandoned.load(),
      state.counts.late.load(), state.counts.capacity.load(), state.counts.wrong_frame.load(), frame_transformer_provider_panics(*value.state()->provider), state.counts.callback_forwards.load(), state.counts.late_callback_forwards.load()};
}
FfiFrameTransformBinding frame_transform_completion_binding(const NativeFrameTransformCompletion& value) noexcept { return value.request()->binding; }
std::uint8_t frame_transform_complete(std::unique_ptr<NativeFrameTransformCompletion> completion, std::unique_ptr<NativeTransformableFrame> frame) noexcept {
  const auto& request = completion->request();
  if (const auto error = CheckFrame(frame, request->binding)) {
    Count(request->generation->provider->counts.wrong_frame); return error;
  }
  if (!frame->audio() && request->binding.sender && frame->provenance()->output != request->output) {
    Count(request->generation->provider->counts.wrong_frame); return 4;
  }
  if (!request->Finish()) return 3;
  if (!request->Live() || !request->output->OnTransformedFrame(frame->Take())) {
    Count(request->generation->provider->counts.late); return 1;
  }
  Count(request->generation->provider->counts.completed); return 0;
}
bool frame_transform_short_circuit(const NativeFrameTransformCompletion& completion) noexcept {
  const auto& request = completion.request();
  return !request->consumed.load(std::memory_order_acquire) && request->Live() && request->output->StartShortCircuiting();
}
std::unique_ptr<NativeFrameTransformCallback> frame_transform_completion_callback(const NativeFrameTransformCompletion& completion) noexcept {
  return std::make_unique<NativeFrameTransformCallback>(completion.request());
}
std::unique_ptr<NativeFrameTransformCallback> frame_transform_callback_copy(const NativeFrameTransformCallback& callback) noexcept {
  return std::make_unique<NativeFrameTransformCallback>(callback.state());
}
bool frame_transform_callback_same(const NativeFrameTransformCallback& a, const NativeFrameTransformCallback& b) noexcept {
  return a.state()->generation == b.state()->generation && a.state()->output == b.state()->output;
}
FfiFrameTransformBinding frame_transform_callback_binding(const NativeFrameTransformCallback& callback) noexcept { return callback.state()->binding; }
bool frame_transform_callback_retired(const NativeFrameTransformCallback& callback) noexcept { return !callback.state()->Live(); }
bool frame_transform_callback_short_circuit(const NativeFrameTransformCallback& callback) noexcept {
  return callback.state()->Live() && callback.state()->output->StartShortCircuiting();
}
std::uint8_t frame_transform_callback_forward(const NativeFrameTransformCallback& callback, std::unique_ptr<NativeTransformableFrame> frame) noexcept {
  const auto& state = callback.state();
  if (const auto error = CheckFrame(frame, state->binding)) { Count(state->generation->provider->counts.wrong_frame); return error; }
  auto source = frame->provenance();
  // Cross-route video forwarding cannot establish the target's current private
  // dependency structure without an SDK hook. Exact callback ownership is
  // causal identity, never an SSRC or queue-name approximation.
  if (!frame->audio() && state->binding.sender && source->output != state->output) {
    Count(state->generation->provider->counts.wrong_frame); return 4;
  }
  if (!state->Live() || !state->output->OnTransformedFrame(frame->Take())) {
    Count(state->generation->provider->counts.late_callback_forwards); return 1;
  }
  Count(state->generation->provider->counts.callback_forwards);
  if (source->Finish()) Count(source->generation->provider->counts.completed);
  return 0;
}
struct NativeFrameTransformerHosts::State {
  struct SenderEntry {
    webrtc::scoped_refptr<webrtc::RtpSenderInterface> endpoint;
    webrtc::scoped_refptr<NativeFrameTransformerHost> host;
  };
  struct ReceiverEntry {
    webrtc::scoped_refptr<webrtc::RtpReceiverInterface> endpoint;
    webrtc::scoped_refptr<NativeFrameTransformerHost> host;
  };
  std::mutex mutex;
  std::map<const webrtc::RtpSenderInterface*, SenderEntry> senders;
  std::map<const webrtc::RtpReceiverInterface*, ReceiverEntry> receivers;
};
NativeFrameTransformerHosts::NativeFrameTransformerHosts() : state_(std::make_unique<State>()) {}
NativeFrameTransformerHosts::~NativeFrameTransformerHosts() = default;
webrtc::scoped_refptr<NativeFrameTransformerHost> NativeFrameTransformerHosts::Sender(
    const NativePeerConnection& peer, webrtc::scoped_refptr<webrtc::RtpSenderInterface> endpoint) {
  const bool audio = endpoint->media_type() == webrtc::MediaType::AUDIO;
  auto candidate = webrtc::make_ref_counted<NativeFrameTransformerHost>(audio, true, audio ? peer.opus_transformer() : nullptr);
  webrtc::scoped_refptr<NativeFrameTransformerHost> result;
  bool created;
  {
    std::lock_guard lock(state_->mutex);
    auto entry = state_->senders.try_emplace(endpoint.get(), State::SenderEntry{endpoint, candidate});
    result = entry.first->second.host; created = entry.second;
  }
  // No bridge mutex spans the SDK setter, including registration callbacks.
  if (created) {
    if (peer.signaling_thread()->IsCurrent()) endpoint->SetFrameTransformer(result);
    else peer.signaling_thread()->BlockingCall([&] { endpoint->SetFrameTransformer(result); });
  }
  return result;
}
webrtc::scoped_refptr<NativeFrameTransformerHost> NativeFrameTransformerHosts::Receiver(
    const NativePeerConnection& peer, webrtc::scoped_refptr<webrtc::RtpReceiverInterface> endpoint) {
  const bool audio = endpoint->media_type() == webrtc::MediaType::AUDIO;
  auto candidate = webrtc::make_ref_counted<NativeFrameTransformerHost>(audio, false);
  webrtc::scoped_refptr<NativeFrameTransformerHost> result;
  bool created;
  {
    std::lock_guard lock(state_->mutex);
    auto entry = state_->receivers.try_emplace(endpoint.get(), State::ReceiverEntry{endpoint, candidate});
    result = entry.first->second.host; created = entry.second;
  }
  if (created) {
    if (peer.worker_thread()->IsCurrent()) endpoint->SetFrameTransformer(result);
    else peer.worker_thread()->BlockingCall([&] { endpoint->SetFrameTransformer(result); });
  }
  return result;
}
void NativeFrameTransformerHosts::ClearSender(const webrtc::RtpSenderInterface* endpoint) {
  webrtc::scoped_refptr<NativeFrameTransformerHost> host;
  { std::lock_guard lock(state_->mutex); if (auto it = state_->senders.find(endpoint); it != state_->senders.end()) host = it->second.host; }
  if (host) host->SetCaller(nullptr);
}
void NativeFrameTransformerHosts::ClearReceiver(const webrtc::RtpReceiverInterface* endpoint) {
  webrtc::scoped_refptr<NativeFrameTransformerHost> host;
  { std::lock_guard lock(state_->mutex); if (auto it = state_->receivers.find(endpoint); it != state_->receivers.end()) host = it->second.host; }
  if (host) host->SetCaller(nullptr);
}
void NativeFrameTransformerHosts::Retire() {
  std::vector<webrtc::scoped_refptr<NativeFrameTransformerHost>> hosts;
  {
    std::lock_guard lock(state_->mutex);
    for (const auto& [key, entry] : state_->senders) hosts.push_back(entry.host);
    for (const auto& [key, entry] : state_->receivers) hosts.push_back(entry.host);
  }
  for (const auto& host : hosts) host->SetCaller(nullptr);
}
bool rtp_sender_select_frame_transformer(const NativePeerConnection& peer, const NativeRtpSender& sender, const NativeFrameTransformer* provider) noexcept {
  if (!peer.peer() || (provider && frame_transformer_closed(*provider))) return false;
  const auto endpoints = peer.peer()->GetSenders();
  if (std::find(endpoints.begin(), endpoints.end(), sender.sender()) == endpoints.end()) return false;
  if (provider) peer.frame_transformer_hosts().Sender(peer, sender.sender())->SetCaller(provider);
  else peer.frame_transformer_hosts().ClearSender(sender.sender().get());
  return true;
}
bool rtp_receiver_select_frame_transformer(const NativePeerConnection& peer, const NativeRtpReceiver& receiver, const NativeFrameTransformer* provider) noexcept {
  if (!peer.peer() || (provider && frame_transformer_closed(*provider))) return false;
  const auto endpoints = peer.peer()->GetReceivers();
  if (std::find(endpoints.begin(), endpoints.end(), receiver.receiver()) == endpoints.end()) return false;
  if (provider) peer.frame_transformer_hosts().Receiver(peer, receiver.receiver())->SetCaller(provider);
  else peer.frame_transformer_hosts().ClearReceiver(receiver.receiver().get());
  return true;
}
}  // namespace pulsebeam::webrtc_sys
