#include "pulsebeam-webrtc-sys/native/data_channel.h"

#include <algorithm>
#include <deque>
#include <limits>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>
#include <utility>

#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/peer.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "rtc_base/copy_on_write_buffer.h"
#include "rtc_base/thread.h"

namespace pulsebeam::webrtc_sys {
namespace {

enum EventKind : std::uint8_t {
  kNoEvent = 0,
  kStateChanged = 1,
  kMessage = 2,
  kBufferedAmountChanged = 3,
  kMaxMessageSizeChanged = 4,
  kSendComplete = 5,
  kMessagesDropped = 6,
};

enum SendResult : std::uint8_t {
  kSent = 0,
  kNotOpen = 1,
  kBackpressure = 2,
  kSendFailed = 3,
};

struct NotifyOnExit {
  std::shared_ptr<ReadinessSignal> readiness;
  ~NotifyOnExit() { SignalReadiness(readiness); }
};

rust::Vec<std::uint8_t> TextBytes(std::string_view value) {
  rust::Vec<std::uint8_t> out;
  out.reserve(value.size());
  for (unsigned char byte : value) out.push_back(byte);
  return out;
}

FfiDataChannelError CopyError(const webrtc::RTCError& error) {
  FfiDataChannelError result{};
  result.has_error = !error.ok();
  result.error_type = static_cast<std::uint8_t>(error.type());
  result.error_detail = static_cast<std::uint8_t>(error.error_detail());
  result.sctp_cause_code = error.sctp_cause_code().has_value()
      ? static_cast<std::int32_t>(*error.sctp_cause_code()) : -1;
  result.message = TextBytes(error.message());
  return result;
}

constexpr std::size_t kMaxRetainedMessages = 256;
constexpr std::size_t kMaxRetainedMessageBytes = 1024 * 1024;

std::uint64_t SaturatingAdd(std::uint64_t value, std::uint64_t increment) {
  return value + std::min(std::numeric_limits<std::uint64_t>::max() - value,
                          increment);
}

struct EventState {
  explicit EventState(std::shared_ptr<ReadinessSignal> signal)
      : readiness(std::move(signal)) {}
  void PushState(webrtc::DataChannelInterface::DataState state) {
    NotifyOnExit notify{readiness};
    std::lock_guard lock(mutex);
    if (closed) {
      return;
    }
    // Advisory state is a callback-time snapshot, not a historical transition
    // reconstructed from the state at polling time. Retain only the latest.
    pending_state = state;
  }

  void PushInitialState(webrtc::DataChannelInterface::DataState state) {
    NotifyOnExit notify{readiness};
    std::lock_guard lock(mutex);
    // Register first, then sample. A callback racing with this sample wins.
    if (!closed && !pending_state) {
      pending_state = state;
    }
  }

  void PushMessage(const webrtc::DataBuffer& buffer) {
    NotifyOnExit notify{readiness};
    std::lock_guard lock(mutex);
    if (closed) return;
    // Bound admission before copying, including empty messages. Drop the newest
    // delivery rather than evicting messages the application already retained.
    // This is an owned-inbox policy, not SCTP receive-window credit.
    if (events.size() == kMaxRetainedMessages ||
        buffer.data.size() > kMaxRetainedMessageBytes - retained_message_bytes) {
      dropped_messages = SaturatingAdd(dropped_messages, 1);
      dropped_bytes = SaturatingAdd(dropped_bytes, buffer.data.size());
      return;
    }
    FfiDataChannelEvent event{};
    event.kind = kMessage;
    event.binary = buffer.binary;
    event.data.reserve(buffer.data.size());
    for (std::uint8_t byte : buffer.data) event.data.push_back(byte);
    retained_message_bytes += buffer.data.size();
    events.push_back(std::move(event));
  }

  void PushBufferedAmount(std::uint64_t sent_data_size) {
    NotifyOnExit notify{readiness};
    std::lock_guard lock(mutex);
    if (!closed) {
      const auto previous = pending_sent_bytes.value_or(0);
      const auto remaining = std::numeric_limits<std::uint64_t>::max() - previous;
      pending_sent_bytes = previous + std::min(remaining, sent_data_size);
    }
  }

  void PushMaxMessageSize(int value) {
    NotifyOnExit notify{readiness};
    std::lock_guard lock(mutex);
    if (!closed) pending_max_message_size = value;
  }

  std::uint64_t ReserveSend() {
    std::lock_guard lock(mutex);
    // Include completed-but-unconsumed outcomes in the bound. Reservation
    // precedes the native call, which may invoke or discard its callback inline.
    if (outstanding_sends == 64 || next_send == std::numeric_limits<std::uint64_t>::max()) {
      return 0;
    }
    ++outstanding_sends;
    return ++next_send;
  }

  void CompleteSend(std::uint64_t id, FfiDataChannelError error, bool canceled) {
    NotifyOnExit notify{readiness};
    FfiDataChannelEvent event{};
    event.kind = kSendComplete;
    event.send_id = id;
    event.error = std::move(error);
    event.canceled = canceled;
    std::lock_guard lock(mutex);
    completions.push_back(std::move(event));
  }

  FfiDataChannelEvent Take() {
    std::lock_guard lock(mutex);
    FfiDataChannelEvent event{};
    // Terminal send outcomes cannot be lost to coalescing or channel close.
    if (!completions.empty()) {
      event = std::move(completions.front());
      completions.pop_front();
      --outstanding_sends;
    } else if (dropped_messages != 0) {
      event.kind = kMessagesDropped;
      event.dropped_messages = std::exchange(dropped_messages, 0);
      event.dropped_bytes = std::exchange(dropped_bytes, 0);
    } else if (pending_state) {
      event.kind = kStateChanged;
      event.state = static_cast<std::uint8_t>(*pending_state);
      if (*pending_state == webrtc::DataChannelInterface::kClosed) {
        closed = true;
        events.clear();
        retained_message_bytes = 0;
        pending_sent_bytes.reset();
        pending_max_message_size.reset();
      }
      pending_state.reset();
    } else if (pending_sent_bytes) {
      event.kind = kBufferedAmountChanged;
      event.sent_data_size = *pending_sent_bytes;
      pending_sent_bytes.reset();
    } else if (pending_max_message_size) {
      event.kind = kMaxMessageSizeChanged;
      event.max_message_size = *pending_max_message_size;
      pending_max_message_size.reset();
    } else if (!events.empty()) {
      event = std::move(events.front());
      events.pop_front();
      retained_message_bytes -= event.data.size();
    }
    return event;
  }

  const std::shared_ptr<ReadinessSignal> readiness;
  std::mutex mutex;
  std::deque<FfiDataChannelEvent> events;
  std::deque<FfiDataChannelEvent> completions;
  std::size_t retained_message_bytes = 0;
  std::uint64_t dropped_messages = 0, dropped_bytes = 0;
  std::size_t outstanding_sends = 0;
  std::uint64_t next_send = 0;
  std::optional<int> pending_max_message_size;
  std::optional<webrtc::DataChannelInterface::DataState> pending_state;
  std::optional<std::uint64_t> pending_sent_bytes;
  bool closed = false;
};

// The pinned SCTP implementation discards the callback when disconnected from
// its transport. The callback's lifetime, not an observer or channel raw pointer,
// owns this fallback. Destruction settles cancellation even on queued-task drop.
struct SendCompletion {
  std::shared_ptr<EventState> events;
  std::uint64_t id;
  bool completed = false;

  ~SendCompletion() {
    if (!completed) events->CompleteSend(id, FfiDataChannelError{}, true);
  }
  void Finish(const webrtc::RTCError& error) {
    completed = true;
    events->CompleteSend(id, CopyError(error), false);
  }
};

class DataChannelObserver final : public webrtc::DataChannelObserver {
 public:
  DataChannelObserver(std::shared_ptr<EventState> events,
                      webrtc::DataChannelInterface* channel) noexcept
      : events_(std::move(events)), channel_(channel) {}

  // Capture on the native network sequence without scheduling application
  // work. The pinned public state() accessor deliberately bypasses its proxy
  // for observer callbacks and directly reads the triggering state here.
  // No other channel operations are invoked until callbacks unwind.
  bool IsOkToCallOnTheNetworkThread() override { return true; }
  void OnStateChange() override { events_->PushState(channel_->state()); }
  void OnMaxMessageSize(int value) override { events_->PushMaxMessageSize(value); }
  void OnMessage(const webrtc::DataBuffer& buffer) override {
    events_->PushMessage(buffer);
  }
  void OnBufferedAmountChange(std::uint64_t sent_data_size) override {
    events_->PushBufferedAmount(sent_data_size);
  }

 private:
  std::shared_ptr<EventState> events_;
  // The State owns the channel until observer unregistration has quiesced.
  webrtc::DataChannelInterface* const channel_;
};

std::optional<webrtc::PriorityValue> Priority(std::int32_t value) {
  return value < 0 ? std::nullopt
      : std::make_optional(webrtc::PriorityValue(static_cast<std::uint16_t>(value)));
}

}  // namespace

struct NativeDataChannel::State {
  webrtc::scoped_refptr<webrtc::DataChannelInterface> channel;
  webrtc::scoped_refptr<webrtc::PeerConnectionInterface> peer;
  std::shared_ptr<EventState> events;
  std::unique_ptr<DataChannelObserver> observer;
  webrtc::Thread* signaling_thread = nullptr;
  bool observing = true;
};

NativeDataChannel::NativeDataChannel(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeDataChannel::~NativeDataChannel() {
  if (!state_ || !state_->signaling_thread) {
    return;
  }
  state_->signaling_thread->BlockingCall([this] {
    if (state_->channel) {
      state_->channel->UnregisterObserver();
      state_->channel->Close();
      state_->channel = nullptr;
    }
    state_->peer = nullptr;
  });
  state_->observer.reset();
}
const std::unique_ptr<NativeDataChannel::State>& NativeDataChannel::state()
    const noexcept {
  return state_;
}

std::unique_ptr<NativeDataChannel> wrap_data_channel(
    webrtc::scoped_refptr<webrtc::DataChannelInterface> channel,
    webrtc::scoped_refptr<webrtc::PeerConnectionInterface> peer,
    webrtc::Thread* signaling_thread,
    std::shared_ptr<ReadinessSignal> readiness) noexcept {
  if (!channel || !peer || !signaling_thread) {
    return nullptr;
  }
  auto state = std::make_unique<NativeDataChannel::State>();
  state->channel = std::move(channel);
  state->peer = std::move(peer);
  state->events = std::make_shared<EventState>(std::move(readiness));
  state->observer = std::make_unique<DataChannelObserver>(state->events,
                                                         state->channel.get());
  state->signaling_thread = signaling_thread;
  state->signaling_thread->BlockingCall([&] {
    state->channel->RegisterObserver(state->observer.get());
    state->events->PushInitialState(state->channel->state());
  });
  return std::make_unique<NativeDataChannel>(std::move(state));
}

std::unique_ptr<NativeDataChannel> create_data_channel(
    const NativePeerConnection& peer,
    rust::Slice<const std::uint8_t> label,
    const FfiDataChannelInit& fields,
    std::uint8_t& error_type,
    rust::Vec<std::uint8_t>& error) noexcept {
  webrtc::DataChannelInit configuration;
  configuration.reliable = fields.reliable;
  configuration.ordered = fields.ordered;
  // The pinned SDK normalizes signed reliability inputs before its mutual-
  // exclusion validation. Do not pre-filter or clamp them in the adapter.
  if (fields.has_max_retransmit_time_ms) {
    configuration.maxRetransmitTime = fields.max_retransmit_time_ms;
  }
  if (fields.has_max_retransmits) {
    configuration.maxRetransmits = fields.max_retransmits;
  }
  configuration.protocol.assign(
      reinterpret_cast<const char*>(fields.protocol.data()), fields.protocol.size());
  configuration.negotiated = fields.negotiated;
  configuration.id = fields.id;
  if (auto parsed_priority = Priority(fields.priority)) {
    configuration.priority = webrtc::PriorityValue(*parsed_priority);
  }
  auto result = peer.peer()->CreateDataChannelOrError(
      std::string(reinterpret_cast<const char*>(label.data()), label.size()),
      fields.has_configuration ? &configuration : nullptr);
  if (!result.ok()) {
    error_type = static_cast<std::uint8_t>(result.error().type());
    error = TextBytes(result.error().message());
    return nullptr;
  }
  return wrap_data_channel(result.MoveValue(), peer.peer(),
                           peer.signaling_thread(), peer.readiness());
}

rust::Vec<std::uint8_t> data_channel_label(const NativeDataChannel& channel) noexcept {
  return TextBytes(channel.state()->channel->label());
}
bool data_channel_reliable(const NativeDataChannel& channel) noexcept {
  return channel.state()->channel->reliable();
}
FfiDataChannelCounters data_channel_counters(const NativeDataChannel& channel) noexcept {
  const auto& native = channel.state()->channel;
  FfiDataChannelCounters result{};
  result.messages_sent = native->messages_sent();
  result.bytes_sent = native->bytes_sent();
  result.messages_received = native->messages_received();
  result.bytes_received = native->bytes_received();
  return result;
}
bool data_channel_ordered(const NativeDataChannel& channel) noexcept {
  return channel.state()->channel->ordered();
}
std::int32_t data_channel_max_retransmit_time_ms(
    const NativeDataChannel& channel) noexcept {
  return channel.state()->channel->maxPacketLifeTime().value_or(-1);
}
std::int32_t data_channel_max_retransmits(
    const NativeDataChannel& channel) noexcept {
  return channel.state()->channel->maxRetransmitsOpt().value_or(-1);
}
rust::Vec<std::uint8_t> data_channel_protocol(const NativeDataChannel& channel) noexcept {
  return TextBytes(channel.state()->channel->protocol());
}
bool data_channel_negotiated(const NativeDataChannel& channel) noexcept {
  return channel.state()->channel->negotiated();
}
std::int32_t data_channel_id(const NativeDataChannel& channel) noexcept {
  return channel.state()->channel->id();
}
std::uint16_t data_channel_priority(const NativeDataChannel& channel) noexcept {
  return channel.state()->channel->priority().value();
}
std::uint8_t data_channel_state(const NativeDataChannel& channel) noexcept {
  return static_cast<std::uint8_t>(channel.state()->channel->state());
}
std::uint64_t data_channel_buffered_amount(
    const NativeDataChannel& channel) noexcept {
  return channel.state()->channel->buffered_amount();
}
std::uint8_t data_channel_send(const NativeDataChannel& channel,
                               rust::Slice<const std::uint8_t> data,
                               bool binary) noexcept {
  const auto& native = channel.state()->channel;
  if (native->state() != webrtc::DataChannelInterface::kOpen) {
    return kNotOpen;
  }
  const std::uint64_t queued = native->buffered_amount();
  const std::uint64_t capacity =
      webrtc::DataChannelInterface::MaxSendQueueSize();
  if (queued >= capacity || data.size() > capacity - queued) {
    return kBackpressure;
  }
  const webrtc::CopyOnWriteBuffer bytes(data.data(), data.size());
  return native->Send(webrtc::DataBuffer(bytes, binary)) ? kSent : kSendFailed;
}
std::uint64_t data_channel_send_async(const NativeDataChannel& channel,
                                      rust::Slice<const std::uint8_t> data,
                                      bool binary) noexcept {
  const auto& state = *channel.state();
  const auto id = state.events->ReserveSend();
  if (id == 0) return 0;
  auto completion = std::make_unique<SendCompletion>();
  completion->events = state.events;
  completion->id = id;
  state.channel->SendAsync(
      webrtc::DataBuffer(webrtc::CopyOnWriteBuffer(data.data(), data.size()), binary),
      [completion = std::move(completion)](webrtc::RTCError error) mutable {
        completion->Finish(error);
      });
  return id;
}
FfiDataChannelEvent data_channel_take_event(
    const NativeDataChannel& channel) noexcept {
  return channel.state()->events->Take();
}
bool close_data_channel(const NativeDataChannel& channel) noexcept {
  channel.state()->channel->Close();
  return true;
}
FfiDataChannelError data_channel_error(const NativeDataChannel& channel) noexcept {
  return CopyError(channel.state()->channel->error());
}
FfiDataChannelReceiveQueueLimits data_channel_receive_queue_limits() noexcept {
  FfiDataChannelReceiveQueueLimits limits{};
  limits.max_messages = kMaxRetainedMessages;
  limits.max_bytes = kMaxRetainedMessageBytes;
  return limits;
}
std::uint64_t data_channel_send_queue_capacity() noexcept {
  return webrtc::DataChannelInterface::MaxSendQueueSize();
}
void data_channel_set_event_observation(const NativeDataChannel& channel,
                                       bool enabled) noexcept {
  auto& state = *channel.state();
  if (enabled == state.observing) return;
  state.signaling_thread->BlockingCall([&] {
    if (enabled) {
      state.channel->RegisterObserver(state.observer.get());
      // Registration may deliver native queued messages. This is an explicit
      // current-state sample at re-registration, not a historical transition.
      state.events->PushState(state.channel->state());
    } else {
      state.channel->UnregisterObserver();
    }
  });
  state.observing = enabled;
}

}  // namespace pulsebeam::webrtc_sys
