#include "pulsebeam-webrtc-sys/native/event_log.h"
#include "pulsebeam-webrtc-sys/native/runtime_retirement.h"

#include <condition_variable>
#include <deque>
#include <mutex>
#include <span>
#include <thread>
#include <utility>

#include "api/rtc_event_log/rtc_event_log_factory.h"
#include "api/environment/environment_factory.h"

#include "api/units/timestamp.h"
#include "logging/rtc_event_log/events/rtc_event_begin_log.h"
#include "logging/rtc_event_log/events/rtc_event_end_log.h"
#include "modules/rtp_rtcp/include/rtp_header_extension_map.h"
#include "modules/rtp_rtcp/source/rtp_packet_received.h"
#include "modules/rtp_rtcp/source/rtp_header_extensions.h"
#include "modules/rtp_rtcp/source/rtp_generic_frame_descriptor_extension.h"
#include "modules/rtp_rtcp/source/rtp_dependency_descriptor_extension.h"
#include "modules/rtp_rtcp/source/rtp_video_layers_allocation_extension.h"
#include "modules/rtp_rtcp/source/corruption_detection_extension.h"
#include "modules/rtp_rtcp/source/rtp_packet_to_send.h"
#include "logging/rtc_event_log/events/rtc_event_alr_state.h"
#include "logging/rtc_event_log/events/rtc_event_route_change.h"
#include "logging/rtc_event_log/events/rtc_event_remote_estimate.h"
#include "logging/rtc_event_log/events/rtc_event_audio_network_adaptation.h"
#include "logging/rtc_event_log/events/rtc_event_audio_playout.h"
#include "logging/rtc_event_log/events/rtc_event_audio_receive_stream_config.h"
#include "logging/rtc_event_log/events/rtc_event_audio_send_stream_config.h"
#include "logging/rtc_event_log/events/rtc_event_bwe_update_delay_based.h"
#include "logging/rtc_event_log/events/rtc_event_bwe_update_loss_based.h"
#include "logging/rtc_event_log/events/rtc_event_bwe_update_scream.h"
#include "logging/rtc_event_log/events/rtc_event_dtls_transport_state.h"
#include "logging/rtc_event_log/events/rtc_event_dtls_writable_state.h"
#include "logging/rtc_event_log/events/rtc_event_ice_candidate_pair_config.h"
#include "logging/rtc_event_log/events/rtc_event_ice_candidate_pair.h"
#include "logging/rtc_event_log/events/rtc_event_probe_cluster_created.h"
#include "logging/rtc_event_log/events/rtc_event_probe_result_failure.h"
#include "logging/rtc_event_log/events/rtc_event_probe_result_success.h"
#include "logging/rtc_event_log/events/rtc_event_rtcp_packet_incoming.h"
#include "logging/rtc_event_log/events/rtc_event_rtcp_packet_outgoing.h"
#include "logging/rtc_event_log/events/rtc_event_rtp_packet_incoming.h"
#include "logging/rtc_event_log/events/rtc_event_rtp_packet_outgoing.h"
#include "logging/rtc_event_log/events/rtc_event_video_receive_stream_config.h"
#include "logging/rtc_event_log/events/rtc_event_video_send_stream_config.h"
#include "logging/rtc_event_log/events/rtc_event_frame_decoded.h"
#include "logging/rtc_event_log/events/rtc_event_neteq_set_minimum_delay.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {
namespace { thread_local EventLogCallScope* current_log_call = nullptr; }
EventLogCallScope::EventLogCallScope() noexcept : previous_(current_log_call) { current_log_call = this; }
EventLogCallScope::~EventLogCallScope() { current_log_call = previous_; }
void record_event_log_status(std::uint8_t status) noexcept {
  if (current_log_call && status) current_log_call->status = status;
}
namespace {
class RustRtcEventLogAdapter final : public webrtc::RtcEventLog {
 public:
  explicit RustRtcEventLogAdapter(rust::Box<RustRtcEventLog> logger)
      : logger_(std::make_shared<rust::Box<RustRtcEventLog>>(std::move(logger))) {}
  bool Failed() const { return rust_event_log_failed(**logger_); }
  bool StartLogging(std::unique_ptr<webrtc::RtcEventLogOutput> output,
                    std::int64_t period_ms) override {
    auto logger = logger_;
    return rust_event_log_start(**logger,
        std::make_unique<NativeEventLogOutput>(std::move(output)), period_ms);
  }
  void StopLogging() override {
    auto logger = logger_;
    rust_event_log_stop(**logger);
  }
  void StopLogging(std::function<void()> callback) override {
    // Native async completion permits destroying the adapter from the callback.
    // Retain the Rust registration through the entire borrowed callback frame.
    auto logger = logger_;
    rust_event_log_stop_async(**logger,
        std::make_unique<NativeEventLogStop>(std::move(callback)));
  }
  void Log(std::unique_ptr<webrtc::RtcEvent> event) override {
    auto logger = logger_;
    if (event) rust_event_log_log(**logger,
        std::make_unique<NativeRtcEvent>(std::move(event)));
  }
 private:
  std::shared_ptr<rust::Box<RustRtcEventLog>> logger_;
};
enum class SharedLogKind { Custom, Null, Stock };
struct SharedLogStorage {
  // Released after the logger and every creation dependency, including on the
  // production-only retirement thread used for own-output-queue destruction.
  std::shared_ptr<NativeProductionLease> production;
  std::shared_ptr<rust::Box<RustEnvironmentFactoryRandomness>> randomness;
  std::shared_ptr<rust::Box<RustEventLogRetention>> retained;
  std::optional<webrtc::Environment> context;
  std::unique_ptr<webrtc::RtcEventLog> owned;
  webrtc::RtcEventLog* logger;
};
class SharedLogState : public std::enable_shared_from_this<SharedLogState> {
 public:
  SharedLogState(std::unique_ptr<SharedLogStorage> storage, SharedLogKind kind)
      : owned_storage_(std::move(storage)), storage_(owned_storage_.get()), kind_(kind) {
    if (storage_->context) retirement_ = runtime_retirement_for(*storage_->context);
    if (retirement_) {
      // Native aliases keep this state alive, but only the creator table owns
      // the actual affine logger/queue and its creation dependencies.
      retirement_id_ = retirement_->Retain(
          [storage = std::move(owned_storage_)]() mutable { storage.reset(); });
    }
  }
  ~SharedLogState() {
    if (retirement_) {
      retirement_->Release(retirement_id_);
      return;
    }
    if (OnOutputQueue()) {
      // Threaded execution still uses its off-output-queue teardown. Managed
      // storage always takes the creator ticket path above, with no worker.
      std::thread([storage = std::move(owned_storage_)]() mutable { storage.reset(); }).detach();
    }
  }
  void RetainRandomness(rust::Box<RustEnvironmentFactoryRandomness> retention) {
    auto incoming = std::make_shared<rust::Box<RustEnvironmentFactoryRandomness>>(std::move(retention));
    {
      std::lock_guard lock(mutex_);
      // A live installation is process-exclusive, so an existing guard
      // necessarily retains the same installation. Never capture an SDK
      // Environment here: it could own this very logger and form a cycle.
      if (!storage_->randomness) storage_->randomness.swap(incoming);
    }
  }
  bool Stock() const { return kind_ == SharedLogKind::Stock; }
  bool Failed() const {
    return kind_ == SharedLogKind::Custom &&
        static_cast<RustRtcEventLogAdapter&>(*storage_->logger).Failed();
  }
  void MarkOutputQueue() {
    if (!Stock()) return;
    std::lock_guard lock(mutex_);
    output_queue_ = webrtc::TaskQueueBase::Current();
  }
  bool OnOutputQueue() const {
    std::lock_guard lock(mutex_);
    return Stock() && output_queue_ && output_queue_ == webrtc::TaskQueueBase::Current();
  }
  std::uint8_t Enter(bool control) {
    std::lock_guard lock(mutex_);
    if (pending_) return 2;
    if (draining_) return 8;
    if (Stock() && control) {
      if ((output_queue_ && output_queue_ == webrtc::TaskQueueBase::Current()) || !ControlSequenceCurrentLocked()) return 3;
    }
    ++active_;
    return 0;
  }
  void Leave() { std::lock_guard lock(mutex_); --active_; }
  std::uint8_t Start(std::unique_ptr<webrtc::RtcEventLogOutput> output, std::int64_t period);
  std::uint8_t Stop() {
    if (auto error = Enter(true)) return error;
    storage_->logger->StopLogging();
    const bool failed = Failed();
    Leave();
    return failed ? 7 : 0;
  }
  std::uint8_t Log(std::unique_ptr<webrtc::RtcEvent> event, bool engine) {
    {
      std::lock_guard lock(mutex_);
      if (pending_) {
        if (!engine) return 2;
        deferred_.push_back(std::move(event));
        return 0;
      }
      if (draining_ && !engine) return 8;
      ++active_;
    }
    storage_->logger->Log(std::move(event));
    const bool failed = Failed();
    Leave();
    return failed ? 7 : 0;
  }
  std::uint8_t StopAsync(std::function<void(bool)> callback,
      std::shared_ptr<rust::Box<RustEventLogRetention>> retention = nullptr) {
    {
      std::lock_guard lock(mutex_);
      if (pending_) return 2;
      if (active_ || draining_) return 8;
      if (Stock() && ((output_queue_ && output_queue_ == webrtc::TaskQueueBase::Current()) || !ControlSequenceCurrentLocked())) return 3;
      pending_ = true;
      std::lock_guard callbacks_lock(callbacks_->mutex);
      ++callbacks_->count;
    }
    struct Completion {
      // If this is the last owner, release the SDK logger before Rust leases.
      std::shared_ptr<rust::Box<RustEventLogRetention>> retention;
      std::shared_ptr<SharedLogState> state;
      std::function<void(bool)> callback;
      std::mutex mutex;
      bool returned = false;
      bool completed = false;
    };
    auto completion = std::make_shared<Completion>();
    completion->retention = std::move(retention);
    completion->state = shared_from_this();
    completion->callback = std::move(callback);
    auto finish = [completion] {
      auto retention = std::move(completion->retention);
      auto state = std::move(completion->state);
      auto& self = *state;
      {
        std::lock_guard lock(self.mutex_);
        self.pending_ = false;
        self.draining_ = true;
      }
      // SDK engine events arriving during async stop remain owned and are
      // forwarded after real native output release, never under a bridge lock.
      for (;;) {
        std::unique_ptr<webrtc::RtcEvent> event;
        {
          std::lock_guard lock(self.mutex_);
          if (self.deferred_.empty()) { self.draining_ = false; break; }
          event = std::move(self.deferred_.front());
          self.deferred_.pop_front();
        }
        self.storage_->logger->Log(std::move(event));
      }
      completion->callback(self.Failed());
      auto callbacks = self.callbacks_;
      // Release callback-owned SDK aliases before waking a stock owner that
      // must synchronously join on close. The tracker owns no logger/resources.
      state.reset();
      retention.reset();
      { std::lock_guard lock(callbacks->mutex); --callbacks->count; }
      callbacks->changed.notify_all();
    };
    storage_->logger->StopLogging([completion, finish] {
      completion->state->MarkOutputQueue();
      bool run;
      { std::lock_guard lock(completion->mutex); completion->completed = true; run = completion->returned; }
      if (run) finish();
    });
    // A Rust completion dropped during unwinding must not publish success
    // before the adapter has latched the callback panic.
    bool run;
    { std::lock_guard lock(completion->mutex); completion->returned = true; run = completion->completed; }
    if (run) finish();
    return 0;
  }
  void WaitForCallbacks() {
    // A scoped callback owns both its logger state and creation environment.
    // Releasing a view/product must not wait on its caller-pumped output queue.
    // The creator retirement scope keeps the actual storage/driver alive and
    // drains the admitted native callback before final dependent teardown.
    if (!Stock() || OnOutputQueue() || retirement_) return;
    std::unique_lock lock(callbacks_->mutex);
    callbacks_->changed.wait(lock, [&] { return callbacks_->count == 0; });
  }
 private:
  bool ControlSequenceCurrentLocked() {
    auto* queue = webrtc::TaskQueueBase::Current();
    if (!control_attached_) {
      control_attached_ = true;
      control_thread_ = std::this_thread::get_id();
      control_queue_ = queue;
      return true;
    }
    return control_queue_ ? control_queue_ == queue : control_thread_ == std::this_thread::get_id();
  }
  std::unique_ptr<SharedLogStorage> owned_storage_;
  SharedLogStorage* storage_;
  const SharedLogKind kind_;
  std::shared_ptr<NativeRuntimeRetirement> retirement_;
  NativeRuntimeRetirement::Id retirement_id_ = 0;
  mutable std::mutex mutex_;
  struct CallbackTracker {
    std::mutex mutex;
    std::condition_variable changed;
    std::size_t count = 0;
  };
  std::shared_ptr<CallbackTracker> callbacks_ = std::make_shared<CallbackTracker>();
  // Match the SDK's detached, first-control binding in release builds too.
  bool control_attached_ = false;
  std::thread::id control_thread_;
  webrtc::TaskQueueBase* control_queue_ = nullptr;
  webrtc::TaskQueueBase* output_queue_ = nullptr;  // comparison only; logger owns it
  std::size_t active_ = 0;
  bool pending_ = false;
  bool draining_ = false;
  std::deque<std::unique_ptr<webrtc::RtcEvent>> deferred_;
};
class SharedLogOutput final : public webrtc::RtcEventLogOutput {
 public:
  SharedLogOutput(std::unique_ptr<webrtc::RtcEventLogOutput> output, std::weak_ptr<SharedLogState> state)
      : output_(std::move(output)), state_(std::move(state)),
        caller_(std::this_thread::get_id()), caller_queue_(webrtc::TaskQueueBase::Current()) {}
  ~SharedLogOutput() override { Mark(); output_.reset(); }
  bool IsActive() const override { Mark(); return output_->IsActive(); }
  bool Write(absl::string_view bytes) override { Mark(); return output_->Write(bytes); }
  void Flush() override { Mark(); output_->Flush(); }
 private:
  void Mark() const {
    if (caller_ != std::this_thread::get_id() || caller_queue_ != webrtc::TaskQueueBase::Current()) {
      if (auto state = state_.lock()) state->MarkOutputQueue();
    }
  }
  std::unique_ptr<webrtc::RtcEventLogOutput> output_;
  std::weak_ptr<SharedLogState> state_;
  std::thread::id caller_;
  webrtc::TaskQueueBase* caller_queue_;
};
std::uint8_t SharedLogState::Start(std::unique_ptr<webrtc::RtcEventLogOutput> output, std::int64_t period) {
  bool was_attached;
  { std::lock_guard lock(mutex_); was_attached = control_attached_; }
  if (auto error = Enter(true)) return error;
  if (Stock()) output = std::make_unique<SharedLogOutput>(std::move(output), weak_from_this());
  const bool started = storage_->logger->StartLogging(std::move(output), period);
  const bool failed = Failed();
  Leave();
  if (Stock() && !started && !was_attached) { std::lock_guard lock(mutex_); control_attached_ = false; }
  return failed ? 7 : started ? 0 : 4;
}
class SharedEventLog final : public webrtc::RtcEventLog {
 public:
  explicit SharedEventLog(std::shared_ptr<SharedLogState> state) : state_(std::move(state)) {}
  const std::shared_ptr<SharedLogState>& state() const { return state_; }
  bool StartLogging(std::unique_ptr<webrtc::RtcEventLogOutput> output, std::int64_t period) override {
    auto state = state_;
    const auto error = state->Start(std::move(output), period);
    record_event_log_status(error);
    return error == 0;
  }
  void StopLogging() override { auto state = state_; record_event_log_status(state->Stop()); }
  void StopLogging(std::function<void()> callback) override {
    auto state = state_;
    record_event_log_status(state->StopAsync([callback = std::move(callback)](bool) { callback(); }));
  }
  void Log(std::unique_ptr<webrtc::RtcEvent> event) override {
    auto state = state_;
    state->Log(std::move(event), true);
  }
 private:
  std::shared_ptr<SharedLogState> state_;
};
std::unique_ptr<webrtc::RtcEventLog> MakeSharedLog(std::unique_ptr<webrtc::RtcEventLog> logger,
    SharedLogKind kind, std::optional<webrtc::Environment> context = std::nullopt,
    std::shared_ptr<rust::Box<RustEventLogRetention>> retained = nullptr,
    std::shared_ptr<NativeProductionLease> production = nullptr) {
  auto* pointer = logger ? logger.get() : &context->event_log();
  auto storage = std::make_unique<SharedLogStorage>(SharedLogStorage{
      std::move(production), nullptr, std::move(retained), std::move(context), std::move(logger), pointer});
  return std::make_unique<SharedEventLog>(std::make_shared<SharedLogState>(std::move(storage), kind));
}
class SharedBuiltinLogFactory final : public webrtc::RtcEventLogFactoryInterface {
 public:
  explicit SharedBuiltinLogFactory(rust::Box<RustEventLogRetention> retention)
      : retained_(std::make_shared<rust::Box<RustEventLogRetention>>(std::move(retention))) {}
  std::unique_ptr<webrtc::RtcEventLog> Create(const webrtc::Environment& environment) const override {
    const bool stock = !environment.field_trials().IsEnabled("WebRTC-RtcEventLogKillSwitch");
    std::shared_ptr<NativeProductionLease> production;
    if (stock && !runtime_retirement_for(environment)) {
      production = new_production_lease();
      if (!production) return nullptr;
    }
    webrtc::RtcEventLogFactory factory;
    return MakeSharedLog(factory.Create(environment), stock ? SharedLogKind::Stock : SharedLogKind::Null, environment, retained_, std::move(production));
  }
 private:
  std::shared_ptr<rust::Box<RustEventLogRetention>> retained_;
};
class RustRtcEventLogFactoryAdapter final : public webrtc::RtcEventLogFactoryInterface {
 public:
  RustRtcEventLogFactoryAdapter(std::shared_ptr<rust::Box<RustRtcEventLogFactory>> factory, bool installed)
      : factory_(std::move(factory)), installed_(installed) {}
  std::unique_ptr<webrtc::RtcEventLog> Create(const webrtc::Environment& environment) const override {
    auto logger = rust_event_log_create(**factory_, rtc_event_log_factory_environment(environment));
    // Keep a rejecting adapter after factory failure, never a stock product.
    // This also preserves the custom product's failure diagnostic.
    return installed_ ? shared_custom_event_log(std::move(logger)) : rtc_event_log_adapter(std::move(logger));
  }
 private:
  std::shared_ptr<rust::Box<RustRtcEventLogFactory>> factory_;
  const bool installed_;
};
}  // namespace

NativeRtcEvent::NativeRtcEvent(std::unique_ptr<webrtc::RtcEvent> event) noexcept
    : event_(std::move(event)) {}
NativeRtcEvent::~NativeRtcEvent() = default;
const webrtc::RtcEvent& NativeRtcEvent::event() const noexcept { return *event_; }
std::unique_ptr<webrtc::RtcEvent> NativeRtcEvent::Take() noexcept { return std::move(event_); }
namespace {
rust::Vec<std::uint8_t> EventBytes(std::span<const std::uint8_t> bytes) {
  rust::Vec<std::uint8_t> result;
  result.reserve(bytes.size());
  for (const auto byte : bytes) result.push_back(byte);
  return result;
}
FfiRtcEventStreamConfig EventStream(const webrtc::rtclog::StreamConfig& config) {
  FfiRtcEventStreamConfig result{};
  result.local_ssrc = config.local_ssrc;
  result.remote_ssrc = config.remote_ssrc;
  result.rtx_ssrc = config.rtx_ssrc;
  result.rsid = config.rsid;
  result.remb = config.remb;
  result.rtcp_mode = static_cast<std::int32_t>(config.rtcp_mode);
  for (const auto& extension : config.rtp_extensions) {
    result.extensions.push_back(FfiRtpHeaderExtension{
        extension.uri, extension.id.value(), extension.encrypt, 0});
  }
  for (const auto& codec : config.codecs) {
    result.codecs.push_back(FfiRtcEventStreamCodec{
        codec.payload_name, codec.payload_type, codec.rtx_payload_type});
  }
  return result;
}
template<typename Packet>
void EventRtp(const Packet& packet, FfiRtcEventPayload& result) {
  result.bytes = EventBytes(packet.RawHeader());
  result.packet_length = packet.packet_length();
  result.header_length = packet.header_length();
  result.payload_length = packet.payload_length();
  result.padding_length = packet.padding_length();
  result.ssrc = packet.Ssrc();
  result.rtp_timestamp = packet.Timestamp();
  result.sequence_number = packet.SequenceNumber();
  result.payload_type = packet.PayloadType();
  result.marker = packet.Marker();
  result.has_rtx_sequence_number = packet.rtx_original_sequence_number().has_value();
  result.rtx_sequence_number = packet.rtx_original_sequence_number().value_or(0);
}
template<typename Trait, typename Packet, typename... Args>
bool ReadEventExtension(const Packet& packet, FfiRtcEventRtpExtension& result, Args&&... args) {
  result.present = packet.template HasExtension<Trait>();
  if (!result.present) return false;
  result.raw = EventBytes(packet.template GetRawExtension<Trait>());
  result.parsed = packet.template GetExtension<Trait>(std::forward<Args>(args)...);
  return result.parsed;
}
template<typename Packet>
FfiRtcEventRtpExtension EventRtpExtension(const Packet& packet, std::uint8_t kind) {
  FfiRtcEventRtpExtension v{};
  switch (kind) {
    case 0: ReadEventExtension<webrtc::AbsoluteSendTime>(packet, v, &v.unsigned_value); break;
    case 1: {
      webrtc::AbsoluteCaptureTime capture{};
      if (ReadEventExtension<webrtc::AbsoluteCaptureTimeExtension>(packet, v, &capture)) {
        v.capture_timestamp = capture.absolute_capture_timestamp;
        v.has_optional = capture.estimated_capture_clock_offset.has_value();
        v.capture_offset = capture.estimated_capture_clock_offset.value_or(0);
      }
      break;
    }
    case 2: {
      webrtc::AudioLevel level{};
      if (ReadEventExtension<webrtc::AudioLevelExtension>(packet, v, &level)) {
        v.flag = level.voice_activity(); v.signed_value = level.level();
      }
      break;
    }
    case 3: {
      std::vector<std::uint8_t> levels;
      if (ReadEventExtension<webrtc::CsrcAudioLevel>(packet, v, &levels)) v.bytes = EventBytes(levels);
      break;
    }
    case 4: ReadEventExtension<webrtc::TransmissionOffset>(packet, v, &v.signed_value); break;
    case 5: ReadEventExtension<webrtc::TransportSequenceNumber>(packet, v, &v.sequence); break;
    case 6: {
      std::optional<webrtc::FeedbackRequest> feedback;
      if (ReadEventExtension<webrtc::TransportSequenceNumberV2>(packet, v, &v.sequence, &feedback)) {
        v.has_optional = feedback.has_value();
        if (feedback) {
          v.feedback_include_timestamps = feedback->include_timestamps;
          v.feedback_sequence_count = feedback->sequence_count;
        }
      }
      break;
    }
    case 7: {
      webrtc::VideoRotation rotation = webrtc::kVideoRotation_0;
      if (ReadEventExtension<webrtc::VideoOrientation>(packet, v, &rotation)) v.sequence = static_cast<std::uint16_t>(rotation);
      break;
    }
    case 8: {
      webrtc::VideoPlayoutDelay delay;
      if (ReadEventExtension<webrtc::PlayoutDelayLimits>(packet, v, &delay)) {
        v.min_delay_ms = delay.min().ms(); v.max_delay_ms = delay.max().ms();
      }
      break;
    }
    case 9: {
      webrtc::VideoContentType content{};
      if (ReadEventExtension<webrtc::VideoContentTypeExtension>(packet, v, &content)) v.unsigned_value = static_cast<std::uint8_t>(content);
      break;
    }
    case 10: {
      webrtc::VideoSendTiming timing{};
      if (ReadEventExtension<webrtc::VideoTimingExtension>(packet, v, &timing)) {
        v.timing_flags = timing.flags;
        v.encode_start = timing.encode_start_delta_ms; v.encode_finish = timing.encode_finish_delta_ms;
        v.packetization_finish = timing.packetization_finish_delta_ms; v.pacer_exit = timing.pacer_exit_delta_ms;
        v.network_timestamp = timing.network_timestamp_delta_ms; v.network2_timestamp = timing.network2_timestamp_delta_ms;
      }
      break;
    }
    case 11: {
      webrtc::ColorSpace color;
      if (ReadEventExtension<webrtc::ColorSpaceExtension>(packet, v, &color)) {
        v.primary = static_cast<std::uint8_t>(color.primaries());
        v.transfer = static_cast<std::uint8_t>(color.transfer());
        v.matrix = static_cast<std::uint8_t>(color.matrix());
        v.range = static_cast<std::uint8_t>(color.range());
        v.chroma_horizontal = static_cast<std::uint8_t>(color.chroma_siting_horizontal());
        v.chroma_vertical = static_cast<std::uint8_t>(color.chroma_siting_vertical());
        if (const auto* hdr = color.hdr_metadata()) {
          v.has_hdr = true;
          const auto& mastering = hdr->mastering_metadata;
          v.primary_r_x = mastering.primary_r.x; v.primary_r_y = mastering.primary_r.y;
          v.primary_g_x = mastering.primary_g.x; v.primary_g_y = mastering.primary_g.y;
          v.primary_b_x = mastering.primary_b.x; v.primary_b_y = mastering.primary_b.y;
          v.white_x = mastering.white_point.x; v.white_y = mastering.white_point.y;
          v.luminance_max = mastering.luminance_max; v.luminance_min = mastering.luminance_min;
          v.max_content_light_level = hdr->max_content_light_level;
          v.max_frame_average_light_level = hdr->max_frame_average_light_level;
        }
      }
      break;
    }
    case 12: case 13: case 14: {
      std::string text;
      if (kind == 12) ReadEventExtension<webrtc::RtpStreamId>(packet, v, &text);
      else if (kind == 13) ReadEventExtension<webrtc::RepairedRtpStreamId>(packet, v, &text);
      else ReadEventExtension<webrtc::RtpMid>(packet, v, &text);
      if (v.parsed) v.text = EventBytes(std::span<const std::uint8_t>(
          reinterpret_cast<const std::uint8_t*>(text.data()), text.size()));
      break;
    }
    case 15: {
      std::optional<std::uint8_t> level;
      if (ReadEventExtension<webrtc::InbandComfortNoiseExtension>(packet, v, &level)) {
        v.has_optional = level.has_value(); v.unsigned_value = level.value_or(0);
      }
      break;
    }
    case 16: ReadEventExtension<webrtc::VideoFrameTrackingIdExtension>(packet, v, &v.sequence); break;
    case 17: {
      webrtc::RtpGenericFrameDescriptor descriptor;
      if (ReadEventExtension<webrtc::RtpGenericFrameDescriptorExtension00>(packet, v, &descriptor)) {
        v.first_packet = descriptor.FirstPacketInSubFrame(); v.last_packet = descriptor.LastPacketInSubFrame();
        if (v.first_packet) {
          v.temporal_id = descriptor.TemporalLayer(); v.spatial_id = descriptor.SpatialLayer();
          v.spatial_mask = descriptor.SpatialLayersBitmask(); v.width = descriptor.Width(); v.height = descriptor.Height();
          v.frame_id = descriptor.FrameId();
          for (auto diff : descriptor.FrameDependenciesDiffs()) v.dependency_diffs.push_back(diff);
        }
      }
      break;
    }
    case 18: {
      webrtc::VideoLayersAllocation allocation;
      if (ReadEventExtension<webrtc::RtpVideoLayersAllocationExtension>(packet, v, &allocation)) {
        v.rtp_stream_index = allocation.rtp_stream_index;
        v.has_resolution_and_frame_rate = allocation.resolution_and_frame_rate_is_valid;
        for (const auto& layer : allocation.active_spatial_layers) {
          FfiRtcEventSpatialLayer result{};
          result.rtp_stream_index = layer.rtp_stream_index; result.spatial_id = layer.spatial_id;
          for (auto rate : layer.target_bitrate_per_temporal_layer) {
            result.rates.push_back(FfiRtcEventRate{
                rate.IsFinite() ? rate.bps() : 0,
                static_cast<std::int8_t>(rate.IsFinite() ? 0 : rate.IsPlusInfinity() ? 1 : -1)});
          }
          if (v.has_resolution_and_frame_rate) {
            result.width = layer.width; result.height = layer.height; result.frame_rate_fps = layer.frame_rate_fps;
          }
          v.allocation_layers.push_back(std::move(result));
        }
      }
      break;
    }
    case 19: {
      webrtc::CorruptionDetectionMessage message;
      if (ReadEventExtension<webrtc::CorruptionDetectionExtension>(packet, v, &message)) {
        v.sequence_index = message.sequence_index();
        v.interpret_sequence_index_as_msb = message.interpret_sequence_index_as_most_significant_bits();
        v.std_dev = message.std_dev(); v.luma_error_threshold = message.luma_error_threshold();
        v.chroma_error_threshold = message.chroma_error_threshold();
        for (auto sample : message.sample_values()) v.sample_values.push_back(sample);
      }
      break;
    }
    case 20: {
      webrtc::DependencyDescriptorMandatory descriptor{};
      if (ReadEventExtension<webrtc::RtpDependencyDescriptorExtension>(packet, v, &descriptor)) {
        v.first_packet = descriptor.first_packet_in_frame(); v.last_packet = descriptor.last_packet_in_frame();
        v.frame_number = descriptor.frame_number(); v.template_id = descriptor.template_id();
      }
      break;
    }
  }
  return v;
}
FfiRtcEventDependencyTemplate EventDependencyTemplate(const webrtc::FrameDependencyTemplate& native) {
  FfiRtcEventDependencyTemplate result{};
  result.spatial_id = native.spatial_id; result.temporal_id = native.temporal_id;
  for (auto dti : native.decode_target_indications) result.decode_target_indications.push_back(static_cast<std::uint8_t>(dti));
  for (auto diff : native.frame_diffs) result.frame_diffs.push_back(diff);
  for (auto diff : native.chain_diffs) result.chain_diffs.push_back(diff);
  return result;
}
FfiRtcEventDependencyStructure EventDependencyStructure(const webrtc::FrameDependencyStructure& native) {
  FfiRtcEventDependencyStructure result{};
  result.structure_id = native.structure_id; result.num_decode_targets = native.num_decode_targets; result.num_chains = native.num_chains;
  for (auto chain : native.decode_target_protected_by_chain) result.decode_target_protected_by_chain.push_back(chain);
  for (auto resolution : native.resolutions) result.resolutions.push_back(FfiRtcEventResolution{resolution.Width(), resolution.Height()});
  for (const auto& value : native.templates) result.templates.push_back(EventDependencyTemplate(value));
  return result;
}
webrtc::FrameDependencyStructure NativeDependencyStructure(const FfiRtcEventDependencyStructure& value) {
  webrtc::FrameDependencyStructure native;
  native.structure_id = value.structure_id; native.num_decode_targets = value.num_decode_targets; native.num_chains = value.num_chains;
  for (auto chain : value.decode_target_protected_by_chain) native.decode_target_protected_by_chain.push_back(chain);
  for (auto resolution : value.resolutions) native.resolutions.emplace_back(resolution.width, resolution.height);
  for (const auto& item : value.templates) {
    webrtc::FrameDependencyTemplate result;
    result.spatial_id = item.spatial_id; result.temporal_id = item.temporal_id;
    for (auto dti : item.decode_target_indications) result.decode_target_indications.push_back(static_cast<webrtc::DecodeTargetIndication>(dti));
    for (auto diff : item.frame_diffs) result.frame_diffs.push_back(diff);
    for (auto diff : item.chain_diffs) result.chain_diffs.push_back(diff);
    native.templates.push_back(std::move(result));
  }
  return native;
}
template<typename Packet>
FfiRtcEventDependencyDescriptor EventRtpDependencyDescriptor(const Packet& packet, const webrtc::FrameDependencyStructure* structure) {
  FfiRtcEventDependencyDescriptor result{};
  result.present = packet.template HasExtension<webrtc::RtpDependencyDescriptorExtension>();
  if (!result.present) return result;
  result.raw = EventBytes(packet.template GetRawExtension<webrtc::RtpDependencyDescriptorExtension>());
  webrtc::DependencyDescriptor descriptor;
  result.parsed = packet.template GetExtension<webrtc::RtpDependencyDescriptorExtension>(structure, &descriptor);
  if (!result.parsed) return result;
  result.first_packet_in_frame = descriptor.first_packet_in_frame; result.last_packet_in_frame = descriptor.last_packet_in_frame;
  result.frame_number = descriptor.frame_number;
  result.frame_dependencies = EventDependencyTemplate(descriptor.frame_dependencies);
  if (descriptor.resolution) {
    result.has_resolution = true;
    result.resolution = FfiRtcEventResolution{descriptor.resolution->Width(), descriptor.resolution->Height()};
  }
  result.has_active_decode_targets = descriptor.active_decode_targets_bitmask.has_value();
  result.active_decode_targets = descriptor.active_decode_targets_bitmask.value_or(0);
  if (descriptor.attached_structure) {
    result.has_attached_structure = true;
    result.attached_structure = EventDependencyStructure(*descriptor.attached_structure);
  }
  return result;
}
void EventRate(webrtc::DataRate rate, std::int64_t& value, std::int8_t& kind) {
  if (rate.IsFinite()) { value = rate.bps(); kind = 0; }
  else { value = 0; kind = rate.IsPlusInfinity() ? 1 : -1; }
}
}  // namespace
FfiRtcEventDependencyDescriptor rtc_event_rtp_dependency_descriptor(const NativeRtcEvent& event, bool has_structure, FfiRtcEventDependencyStructure structure) noexcept {
  auto native = NativeDependencyStructure(structure);
  const auto* context = has_structure ? &native : nullptr;
  const auto& base = event.event();
  if (base.GetType() == webrtc::RtcEvent::Type::RtpPacketIncoming) {
    return EventRtpDependencyDescriptor(static_cast<const webrtc::RtcEventRtpPacketIncoming&>(base), context);
  }
  if (base.GetType() == webrtc::RtcEvent::Type::RtpPacketOutgoing) {
    return EventRtpDependencyDescriptor(static_cast<const webrtc::RtcEventRtpPacketOutgoing&>(base), context);
  }
  return {};
}
FfiRtcEventRtpExtension rtc_event_rtp_extension(const NativeRtcEvent& event, std::uint8_t kind) noexcept {
  const auto& base = event.event();
  if (base.GetType() == webrtc::RtcEvent::Type::RtpPacketIncoming) {
    return EventRtpExtension(static_cast<const webrtc::RtcEventRtpPacketIncoming&>(base), kind);
  }
  if (base.GetType() == webrtc::RtcEvent::Type::RtpPacketOutgoing) {
    return EventRtpExtension(static_cast<const webrtc::RtcEventRtpPacketOutgoing&>(base), kind);
  }
  return {};
}
FfiRtcEventPayload rtc_event_payload(const NativeRtcEvent& event) noexcept {
  FfiRtcEventPayload result{};
  const auto& base = event.event();
  using Type = webrtc::RtcEvent::Type;
  switch (base.GetType()) {
    case Type::AlrStateEvent:
      result.flag = static_cast<const webrtc::RtcEventAlrState&>(base).in_alr(); break;
    case Type::RouteChangeEvent: {
      const auto& value = static_cast<const webrtc::RtcEventRouteChange&>(base);
      result.flag = value.connected(); result.overhead = value.overhead(); break;
    }
    case Type::RemoteEstimateEvent: {
      const auto& value = static_cast<const webrtc::RtcEventRemoteEstimate&>(base);
      EventRate(value.link_capacity_lower_, result.lower_rate_bps, result.lower_rate_kind);
      EventRate(value.link_capacity_upper_, result.upper_rate_bps, result.upper_rate_kind);
      break;
    }
    case Type::AudioNetworkAdaptation: {
      const auto& value = static_cast<const webrtc::RtcEventAudioNetworkAdaptation&>(base);
      if (value.bitrate_bps()) { result.adaptation_present |= 1; result.bitrate_bps = *value.bitrate_bps(); }
      if (value.frame_length_ms()) { result.adaptation_present |= 2; result.frame_length_ms = *value.frame_length_ms(); }
      if (value.uplink_packet_loss_fraction()) { result.adaptation_present |= 4; result.uplink_packet_loss = *value.uplink_packet_loss_fraction(); }
      if (value.enable_fec()) { result.adaptation_present |= 8; result.enable_fec = *value.enable_fec(); }
      if (value.enable_dtx()) { result.adaptation_present |= 16; result.enable_dtx = *value.enable_dtx(); }
      if (value.num_channels()) { result.adaptation_present |= 32; result.channels = *value.num_channels(); }
      break;
    }
    case Type::AudioPlayout:
      result.ssrc = static_cast<const webrtc::RtcEventAudioPlayout&>(base).ssrc(); break;
    case Type::AudioReceiveStreamConfig:
      result.stream = EventStream(static_cast<const webrtc::RtcEventAudioReceiveStreamConfig&>(base).config()); break;
    case Type::AudioSendStreamConfig:
      result.stream = EventStream(static_cast<const webrtc::RtcEventAudioSendStreamConfig&>(base).config()); break;
    case Type::VideoReceiveStreamConfig:
      result.stream = EventStream(static_cast<const webrtc::RtcEventVideoReceiveStreamConfig&>(base).config()); break;
    case Type::VideoSendStreamConfig:
      result.stream = EventStream(static_cast<const webrtc::RtcEventVideoSendStreamConfig&>(base).config()); break;
    case Type::BweUpdateDelayBased: {
      const auto& value = static_cast<const webrtc::RtcEventBweUpdateDelayBased&>(base);
      result.bitrate_bps = value.bitrate_bps();
      result.detector_state = static_cast<std::int32_t>(value.detector_state()); break;
    }
    case Type::BweUpdateLossBased: {
      const auto& value = static_cast<const webrtc::RtcEventBweUpdateLossBased&>(base);
      result.bitrate_bps = value.bitrate_bps();
      result.fraction_loss = value.fraction_loss(); result.total_packets = value.total_packets(); break;
    }
    case Type::BweUpdateScream: {
      const auto& value = static_cast<const webrtc::RtcEventBweUpdateScream&>(base);
      result.ref_window_bytes = value.ref_window_bytes();
      result.data_in_flight_bytes = value.data_in_flight_bytes();
      result.target_rate_kbps = value.target_rate_kbps();
      result.smoothed_rtt_ms = value.smoothed_rtt_ms();
      result.avg_queue_delay_ms = value.avg_queue_delay_ms();
      result.l4s_marked_permille = value.l4s_marked_permille(); break;
    }
    case Type::DtlsTransportState:
      result.dtls_state = static_cast<std::int32_t>(static_cast<const webrtc::RtcEventDtlsTransportState&>(base).dtls_transport_state()); break;
    case Type::DtlsWritableState:
      result.flag = static_cast<const webrtc::RtcEventDtlsWritableState&>(base).writable(); break;
    case Type::IceCandidatePairConfig: {
      const auto& value = static_cast<const webrtc::RtcEventIceCandidatePairConfig&>(base);
      const auto& description = value.candidate_pair_desc();
      result.ice_type = static_cast<std::int32_t>(value.type());
      result.candidate_pair_id = value.candidate_pair_id();
      result.local_candidate_type = static_cast<std::int32_t>(description.local_candidate_type);
      result.local_relay_protocol = static_cast<std::int32_t>(description.local_relay_protocol);
      result.local_network_type = static_cast<std::int32_t>(description.local_network_type);
      result.local_address_family = static_cast<std::int32_t>(description.local_address_family);
      result.remote_candidate_type = static_cast<std::int32_t>(description.remote_candidate_type);
      result.remote_address_family = static_cast<std::int32_t>(description.remote_address_family);
      result.candidate_pair_protocol = static_cast<std::int32_t>(description.candidate_pair_protocol); break;
    }
    case Type::IceCandidatePairEvent: {
      const auto& value = static_cast<const webrtc::RtcEventIceCandidatePair&>(base);
      result.ice_type = static_cast<std::int32_t>(value.type());
      result.candidate_pair_id = value.candidate_pair_id();
      result.transaction_id = value.transaction_id(); break;
    }
    case Type::ProbeClusterCreated: {
      const auto& value = static_cast<const webrtc::RtcEventProbeClusterCreated&>(base);
      result.probe_id = value.id(); result.bitrate_bps = value.bitrate_bps();
      result.min_probes = value.min_probes(); result.min_bytes = value.min_bytes(); break;
    }
    case Type::ProbeResultFailure: {
      const auto& value = static_cast<const webrtc::RtcEventProbeResultFailure&>(base);
      result.probe_id = value.id(); result.failure_reason = static_cast<std::int32_t>(value.failure_reason()); break;
    }
    case Type::ProbeResultSuccess: {
      const auto& value = static_cast<const webrtc::RtcEventProbeResultSuccess&>(base);
      result.probe_id = value.id(); result.bitrate_bps = value.bitrate_bps(); break;
    }
    case Type::RtcpPacketIncoming: {
      const auto& packet = static_cast<const webrtc::RtcEventRtcpPacketIncoming&>(base).packet();
      result.bytes = EventBytes(std::span(packet.data(), packet.size())); break;
    }
    case Type::RtcpPacketOutgoing: {
      const auto& packet = static_cast<const webrtc::RtcEventRtcpPacketOutgoing&>(base).packet();
      result.bytes = EventBytes(std::span(packet.data(), packet.size())); break;
    }
    case Type::RtpPacketIncoming:
      EventRtp(static_cast<const webrtc::RtcEventRtpPacketIncoming&>(base), result); break;
    case Type::RtpPacketOutgoing: {
      const auto& value = static_cast<const webrtc::RtcEventRtpPacketOutgoing&>(base);
      EventRtp(value, result); result.probe_cluster_id = value.probe_cluster_id(); break;
    }
    case Type::FrameDecoded: {
      const auto& value = static_cast<const webrtc::RtcEventFrameDecoded&>(base);
      result.render_time_ms = value.render_time_ms(); result.ssrc = value.ssrc();
      result.width = value.width(); result.height = value.height();
      result.video_codec = static_cast<std::int32_t>(value.codec()); result.qp = value.qp(); break;
    }
    case Type::NetEqSetMinimumDelay: {
      const auto& value = static_cast<const webrtc::RtcEventNetEqSetMinimumDelay&>(base);
      result.ssrc = value.remote_ssrc(); result.minimum_delay_ms = value.minimum_delay_ms(); break;
    }
    default: break;
  }
  return result;
}
namespace {
std::unique_ptr<webrtc::rtclog::StreamConfig> MakeEventStream(const FfiRtcEventStreamConfig& value) {
  auto result = std::make_unique<webrtc::rtclog::StreamConfig>();
  result->local_ssrc = value.local_ssrc;
  result->remote_ssrc = value.remote_ssrc;
  result->rtx_ssrc = value.rtx_ssrc;
  result->rsid = std::string(value.rsid);
  result->remb = value.remb;
  result->rtcp_mode = static_cast<webrtc::RtcpMode>(value.rtcp_mode);
  for (const auto& extension : value.extensions) {
    result->rtp_extensions.emplace_back(std::string(extension.uri),
        webrtc::RtpHeaderExtensionId(extension.preferred_id), extension.preferred_encrypt);
  }
  for (const auto& codec : value.codecs) {
    result->codecs.emplace_back(std::string(codec.payload_name), codec.payload_type, codec.rtx_payload_type);
  }
  return result;
}
webrtc::DataRate MakeEventRate(std::int64_t value, std::int8_t kind) {
  if (kind == 1) return webrtc::DataRate::PlusInfinity();
  if (kind == -1) return webrtc::DataRate::MinusInfinity();
  return webrtc::DataRate::BitsPerSec(value);
}
}  // namespace
std::unique_ptr<NativeRtcEvent> new_rtc_event(std::uint32_t kind, const FfiRtcEventPayload& v) noexcept {
  std::unique_ptr<webrtc::RtcEvent> event;
  switch (static_cast<webrtc::RtcEvent::Type>(kind)) {
    case webrtc::RtcEvent::Type::AlrStateEvent:
      event = std::make_unique<webrtc::RtcEventAlrState>(v.flag); break;
    case webrtc::RtcEvent::Type::RouteChangeEvent:
      event = std::make_unique<webrtc::RtcEventRouteChange>(v.flag, v.overhead); break;
    case webrtc::RtcEvent::Type::RemoteEstimateEvent:
      event = std::make_unique<webrtc::RtcEventRemoteEstimate>(MakeEventRate(v.lower_rate_bps, v.lower_rate_kind), MakeEventRate(v.upper_rate_bps, v.upper_rate_kind)); break;
    case webrtc::RtcEvent::Type::AudioNetworkAdaptation: {
      webrtc::AudioEncoderRuntimeConfig config;
      if (v.adaptation_present & 1) config.bitrate_bps = v.bitrate_bps;
      if (v.adaptation_present & 2) config.frame_length_ms = v.frame_length_ms;
      if (v.adaptation_present & 4) config.uplink_packet_loss_fraction = v.uplink_packet_loss;
      if (v.adaptation_present & 8) config.enable_fec = v.enable_fec;
      if (v.adaptation_present & 16) config.enable_dtx = v.enable_dtx;
      if (v.adaptation_present & 32) config.num_channels = v.channels;
      event = std::make_unique<webrtc::RtcEventAudioNetworkAdaptation>(config); break;
    }
    case webrtc::RtcEvent::Type::AudioPlayout:
      event = std::make_unique<webrtc::RtcEventAudioPlayout>(v.ssrc); break;
    case webrtc::RtcEvent::Type::AudioReceiveStreamConfig:
      event = std::make_unique<webrtc::RtcEventAudioReceiveStreamConfig>(MakeEventStream(v.stream)); break;
    case webrtc::RtcEvent::Type::AudioSendStreamConfig:
      event = std::make_unique<webrtc::RtcEventAudioSendStreamConfig>(MakeEventStream(v.stream)); break;
    case webrtc::RtcEvent::Type::VideoReceiveStreamConfig:
      event = std::make_unique<webrtc::RtcEventVideoReceiveStreamConfig>(MakeEventStream(v.stream)); break;
    case webrtc::RtcEvent::Type::VideoSendStreamConfig:
      event = std::make_unique<webrtc::RtcEventVideoSendStreamConfig>(MakeEventStream(v.stream)); break;
    case webrtc::RtcEvent::Type::BweUpdateDelayBased:
      event = std::make_unique<webrtc::RtcEventBweUpdateDelayBased>(v.bitrate_bps, static_cast<webrtc::BandwidthUsage>(v.detector_state)); break;
    case webrtc::RtcEvent::Type::BweUpdateLossBased:
      event = std::make_unique<webrtc::RtcEventBweUpdateLossBased>(v.bitrate_bps, v.fraction_loss, v.total_packets); break;
    case webrtc::RtcEvent::Type::BweUpdateScream:
      event = std::make_unique<webrtc::RtcEventBweUpdateScream>(webrtc::DataSize::Bytes(v.ref_window_bytes), webrtc::DataSize::Bytes(v.data_in_flight_bytes), webrtc::DataRate::KilobitsPerSec(v.target_rate_kbps), webrtc::TimeDelta::Millis(v.smoothed_rtt_ms), webrtc::TimeDelta::Millis(v.avg_queue_delay_ms), v.l4s_marked_permille); break;
    case webrtc::RtcEvent::Type::DtlsTransportState:
      event = std::make_unique<webrtc::RtcEventDtlsTransportState>(static_cast<webrtc::DtlsTransportState>(v.dtls_state)); break;
    case webrtc::RtcEvent::Type::DtlsWritableState:
      event = std::make_unique<webrtc::RtcEventDtlsWritableState>(v.flag); break;
    case webrtc::RtcEvent::Type::IceCandidatePairConfig: {
      webrtc::IceCandidatePairDescription description(static_cast<webrtc::IceCandidateType>(v.local_candidate_type), static_cast<webrtc::IceCandidateType>(v.remote_candidate_type));
      description.local_relay_protocol = static_cast<webrtc::IceCandidatePairProtocol>(v.local_relay_protocol);
      description.local_network_type = static_cast<webrtc::IceCandidateNetworkType>(v.local_network_type);
      description.local_address_family = static_cast<webrtc::IceCandidatePairAddressFamily>(v.local_address_family);
      description.remote_address_family = static_cast<webrtc::IceCandidatePairAddressFamily>(v.remote_address_family);
      description.candidate_pair_protocol = static_cast<webrtc::IceCandidatePairProtocol>(v.candidate_pair_protocol);
      event = std::make_unique<webrtc::RtcEventIceCandidatePairConfig>(static_cast<webrtc::IceCandidatePairConfigType>(v.ice_type), v.candidate_pair_id, description); break;
    }
    case webrtc::RtcEvent::Type::IceCandidatePairEvent:
      event = std::make_unique<webrtc::RtcEventIceCandidatePair>(static_cast<webrtc::IceCandidatePairEventType>(v.ice_type), v.candidate_pair_id, v.transaction_id); break;
    case webrtc::RtcEvent::Type::ProbeClusterCreated:
      event = std::make_unique<webrtc::RtcEventProbeClusterCreated>(v.probe_id, v.bitrate_bps, v.min_probes, v.min_bytes); break;
    case webrtc::RtcEvent::Type::ProbeResultFailure:
      event = std::make_unique<webrtc::RtcEventProbeResultFailure>(v.probe_id, static_cast<webrtc::ProbeFailureReason>(v.failure_reason)); break;
    case webrtc::RtcEvent::Type::ProbeResultSuccess:
      event = std::make_unique<webrtc::RtcEventProbeResultSuccess>(v.probe_id, v.bitrate_bps); break;
    case webrtc::RtcEvent::Type::RtcpPacketIncoming:
      event = std::make_unique<webrtc::RtcEventRtcpPacketIncoming>(std::span(v.bytes.data(), v.bytes.size())); break;
    case webrtc::RtcEvent::Type::RtcpPacketOutgoing:
      event = std::make_unique<webrtc::RtcEventRtcpPacketOutgoing>(std::span(v.bytes.data(), v.bytes.size())); break;
    case webrtc::RtcEvent::Type::FrameDecoded:
      event = std::make_unique<webrtc::RtcEventFrameDecoded>(v.render_time_ms, v.ssrc, v.width, v.height, static_cast<webrtc::VideoCodecType>(v.video_codec), v.qp); break;
    case webrtc::RtcEvent::Type::NetEqSetMinimumDelay:
      event = std::make_unique<webrtc::RtcEventNetEqSetMinimumDelay>(v.ssrc, v.minimum_delay_ms); break;
    default: return nullptr;
  }
  return std::make_unique<NativeRtcEvent>(std::move(event));
}
std::unique_ptr<NativeRtcEvent> new_rtc_event_boundary(bool begin, std::int64_t timestamp_us, std::int64_t utc_start_us) noexcept {
  const auto timestamp = webrtc::Timestamp::Micros(timestamp_us);
  if (begin) return std::make_unique<NativeRtcEvent>(std::make_unique<webrtc::RtcEventBeginLog>(timestamp, webrtc::Timestamp::Micros(utc_start_us)));
  return std::make_unique<NativeRtcEvent>(std::make_unique<webrtc::RtcEventEndLog>(timestamp));
}
std::unique_ptr<NativeRtcEvent> new_rtc_event_rtp(bool incoming, rust::Slice<const std::uint8_t> bytes, rust::Slice<const FfiRtpHeaderExtension> extensions, std::int32_t probe_cluster_id, bool has_rtx_sequence, std::uint16_t rtx_sequence) noexcept {
  webrtc::RtpHeaderExtensionMap map;
  for (const auto& extension : extensions) {
    map.RegisterByUri(webrtc::RtpHeaderExtensionId(extension.preferred_id), std::string(extension.uri));
  }
  const auto original_sequence = has_rtx_sequence ? std::optional<std::uint16_t>(rtx_sequence) : std::nullopt;
  if (incoming) {
    webrtc::RtpPacketReceived packet(&map);
    if (!packet.Parse(bytes.data(), bytes.size())) return nullptr;
    return std::make_unique<NativeRtcEvent>(std::make_unique<webrtc::RtcEventRtpPacketIncoming>(packet, original_sequence));
  }
  webrtc::RtpPacketToSend packet(&map);
  if (!packet.Parse(bytes.data(), bytes.size())) return nullptr;
  return std::make_unique<NativeRtcEvent>(std::make_unique<webrtc::RtcEventRtpPacketOutgoing>(packet, probe_cluster_id, original_sequence));
}
std::unique_ptr<NativeRtcEvent> rtc_event_copy(const NativeRtcEvent& event) noexcept {
  const auto& base = event.event();
  switch (base.GetType()) {
    case webrtc::RtcEvent::Type::AlrStateEvent:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventAlrState&>(base).Copy());
    case webrtc::RtcEvent::Type::RouteChangeEvent:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventRouteChange&>(base).Copy());
    case webrtc::RtcEvent::Type::RemoteEstimateEvent:
      return std::make_unique<NativeRtcEvent>(std::make_unique<webrtc::RtcEventRemoteEstimate>(static_cast<const webrtc::RtcEventRemoteEstimate&>(base)));
    case webrtc::RtcEvent::Type::AudioNetworkAdaptation:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventAudioNetworkAdaptation&>(base).Copy());
    case webrtc::RtcEvent::Type::AudioPlayout:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventAudioPlayout&>(base).Copy());
    case webrtc::RtcEvent::Type::AudioReceiveStreamConfig:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventAudioReceiveStreamConfig&>(base).Copy());
    case webrtc::RtcEvent::Type::AudioSendStreamConfig:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventAudioSendStreamConfig&>(base).Copy());
    case webrtc::RtcEvent::Type::BweUpdateDelayBased:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventBweUpdateDelayBased&>(base).Copy());
    case webrtc::RtcEvent::Type::BweUpdateLossBased:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventBweUpdateLossBased&>(base).Copy());
    case webrtc::RtcEvent::Type::BweUpdateScream:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventBweUpdateScream&>(base).Copy());
    case webrtc::RtcEvent::Type::DtlsTransportState:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventDtlsTransportState&>(base).Copy());
    case webrtc::RtcEvent::Type::DtlsWritableState:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventDtlsWritableState&>(base).Copy());
    case webrtc::RtcEvent::Type::IceCandidatePairConfig:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventIceCandidatePairConfig&>(base).Copy());
    case webrtc::RtcEvent::Type::IceCandidatePairEvent:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventIceCandidatePair&>(base).Copy());
    case webrtc::RtcEvent::Type::ProbeClusterCreated:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventProbeClusterCreated&>(base).Copy());
    case webrtc::RtcEvent::Type::ProbeResultFailure:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventProbeResultFailure&>(base).Copy());
    case webrtc::RtcEvent::Type::ProbeResultSuccess:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventProbeResultSuccess&>(base).Copy());
    case webrtc::RtcEvent::Type::RtcpPacketIncoming:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventRtcpPacketIncoming&>(base).Copy());
    case webrtc::RtcEvent::Type::RtcpPacketOutgoing:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventRtcpPacketOutgoing&>(base).Copy());
    case webrtc::RtcEvent::Type::RtpPacketIncoming:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventRtpPacketIncoming&>(base).Copy());
    case webrtc::RtcEvent::Type::RtpPacketOutgoing:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventRtpPacketOutgoing&>(base).Copy());
    case webrtc::RtcEvent::Type::VideoReceiveStreamConfig:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventVideoReceiveStreamConfig&>(base).Copy());
    case webrtc::RtcEvent::Type::VideoSendStreamConfig:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventVideoSendStreamConfig&>(base).Copy());
    case webrtc::RtcEvent::Type::FrameDecoded:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventFrameDecoded&>(base).Copy());
    case webrtc::RtcEvent::Type::NetEqSetMinimumDelay:
      return std::make_unique<NativeRtcEvent>(static_cast<const webrtc::RtcEventNetEqSetMinimumDelay&>(base).Copy());
    default: return nullptr;
  }
}
std::uint32_t rtc_event_kind(const NativeRtcEvent& event) noexcept {
  return static_cast<std::uint32_t>(event.event().GetType());
}
bool rtc_event_is_config(const NativeRtcEvent& event) noexcept { return event.event().IsConfigEvent(); }
std::uint32_t rtc_event_group_key(const NativeRtcEvent& event) noexcept { return event.event().GetGroupKey(); }
std::int64_t rtc_event_timestamp_us(const NativeRtcEvent& event) noexcept { return event.event().timestamp_us(); }
void rtc_event_set_timestamp(NativeRtcEvent& event, std::int64_t timestamp_us) noexcept {
  const_cast<webrtc::RtcEvent&>(event.event()).SetTimestamp(webrtc::Timestamp::Micros(timestamp_us));
}
NativeEventLogStop::NativeEventLogStop(std::function<void()> callback) noexcept : callback_(std::move(callback)) {}
NativeEventLogStop::~NativeEventLogStop() { Complete(); }
void NativeEventLogStop::Complete() noexcept {
  if (callback_) { auto callback = std::move(callback_); callback(); }
}
void event_log_stop_complete(std::unique_ptr<NativeEventLogStop> completion) noexcept {
  if (completion) completion->Complete();
}
NativeRtcEventLogFactory::NativeRtcEventLogFactory(rust::Box<RustRtcEventLogFactory> factory) noexcept
    : factory_(std::make_shared<rust::Box<RustRtcEventLogFactory>>(std::move(factory))) {}
NativeRtcEventLogFactory::~NativeRtcEventLogFactory() = default;
std::unique_ptr<webrtc::RtcEventLogFactoryInterface> NativeRtcEventLogFactory::CreateAdapter(bool installed) const noexcept {
  return std::make_unique<RustRtcEventLogFactoryAdapter>(factory_, installed);
}
std::unique_ptr<NativeRtcEventLogFactory> new_rtc_event_log_factory(rust::Box<RustRtcEventLogFactory> factory) noexcept {
  return std::make_unique<NativeRtcEventLogFactory>(std::move(factory));
}
std::unique_ptr<webrtc::RtcEventLog> rtc_event_log_adapter(rust::Box<RustRtcEventLog> logger) noexcept {
  return std::make_unique<RustRtcEventLogAdapter>(std::move(logger));
}
void retain_event_log_randomness(webrtc::RtcEventLog& logger, rust::Box<RustEnvironmentFactoryRandomness> retention) noexcept {
  static_cast<SharedEventLog&>(logger).state()->RetainRandomness(std::move(retention));
}
std::unique_ptr<webrtc::RtcEventLog> shared_custom_event_log(rust::Box<RustRtcEventLog> logger) noexcept {
  return MakeSharedLog(rtc_event_log_adapter(std::move(logger)), SharedLogKind::Custom);
}
webrtc::Environment shared_default_event_log(webrtc::Environment environment) noexcept {
  webrtc::EnvironmentFactory factory(environment);
  factory.Set(MakeSharedLog(nullptr, SharedLogKind::Null, environment));
  return factory.Create();
}
std::unique_ptr<webrtc::RtcEventLogFactoryInterface> shared_builtin_event_log_factory(
    rust::Box<RustEventLogRetention> retention) noexcept {
  return std::make_unique<SharedBuiltinLogFactory>(std::move(retention));
}
struct NativeEventLogUtility::State {
  explicit State(webrtc::Environment environment)
      : environment(std::move(environment)),
        logger(static_cast<SharedEventLog&>(this->environment->event_log()).state()) {}
  std::optional<webrtc::Environment> environment;
  std::shared_ptr<SharedLogState> logger;
  bool initiated = false;
  bool closed = false;
};
NativeEventLogUtility::NativeEventLogUtility(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeEventLogUtility::~NativeEventLogUtility() { event_log_utility_close(*this); }
const std::unique_ptr<NativeEventLogUtility::State>& NativeEventLogUtility::state() const noexcept { return state_; }
std::unique_ptr<NativeEventLogUtility> environment_event_log(const NativeEnvironment& environment) noexcept {
  // Every exposed environment starts with our owning SDK logger adapter; peer
  // factories install the same adapter before the SDK composes its log leaf.
  return std::make_unique<NativeEventLogUtility>(std::make_unique<NativeEventLogUtility::State>(environment.environment()));
}
std::uint8_t event_log_utility_start(NativeEventLogUtility& log,
    std::unique_ptr<NativeEventLogOutput> output, std::int64_t period) noexcept {
  if (log.state()->closed) return 1;
  if (!output || period < 0) return 4;
  return log.state()->logger->Start(output->Take(), period);
}
std::uint8_t event_log_utility_log(NativeEventLogUtility& log, std::unique_ptr<NativeRtcEvent> event) noexcept {
  if (log.state()->closed) return 1;
  if (!event) return 4;
  return log.state()->logger->Log(event->Take(), false);
}
std::uint8_t event_log_utility_stop(NativeEventLogUtility& log) noexcept {
  return log.state()->closed ? 1 : log.state()->logger->Stop();
}
std::uint8_t event_log_utility_stop_async(NativeEventLogUtility& log,
    rust::Box<RustEventLogProductStop> completion, rust::Box<RustEventLogRetention> retention) noexcept {
  if (log.state()->closed) return 1;
  auto notify = std::make_shared<rust::Box<RustEventLogProductStop>>(std::move(completion));
  const auto error = log.state()->logger->StopAsync(
      [notify](bool failed) { event_log_product_stopped(**notify, failed); },
      std::make_shared<rust::Box<RustEventLogRetention>>(std::move(retention)));
  if (!error) log.state()->initiated = true;
  return error;
}
void event_log_utility_close(NativeEventLogUtility& log) noexcept {
  if (log.state()->closed) return;
  log.state()->closed = true;
  if (log.state()->initiated) log.state()->logger->WaitForCallbacks();
  log.state()->environment.reset();
  log.state()->logger.reset();
}

void test_event_log_sdk_dispatch(const NativeEnvironment& environment, std::unique_ptr<NativeRtcEvent> event) noexcept {
  environment.environment().event_log().Log(event->Take());
}

std::unique_ptr<NativeEnvironment> test_stock_event_log_environment(const NativeEnvironment& environment,
    rust::Box<RustEventLogRetention> retention) noexcept {
  auto factory = shared_builtin_event_log_factory(std::move(retention));
  webrtc::EnvironmentFactory composed(environment.environment());
  composed.Set(factory->Create(environment.environment()));
  return retain_environment_from_sdk(composed.Create(), &environment);
}

bool rtc_event_log_adapter_failed(const webrtc::RtcEventLog& logger) noexcept {
  return static_cast<const RustRtcEventLogAdapter&>(logger).Failed();
}
// Only the exclusive Rust product owner touches this wrapper. Async callbacks
// retain SharedLogState directly, the same authority used by SDK/utility aliases.
struct NativeRtcEventLogProduct::State {
  explicit State(std::shared_ptr<SharedLogState> logger) : logger(std::move(logger)) {}
  std::shared_ptr<SharedLogState> logger;
  const std::thread::id creator = std::this_thread::get_id();
  bool closed = false;
  bool started = false;
  bool initiated = false;
  std::uint8_t Check() const {
    if (closed) return 1;
    return creator == std::this_thread::get_id() ? 0 : 3;
  }
};
NativeRtcEventLogProduct::NativeRtcEventLogProduct(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeRtcEventLogProduct::~NativeRtcEventLogProduct() { event_log_product_close(*this); }
const std::unique_ptr<NativeRtcEventLogProduct::State>& NativeRtcEventLogProduct::state() const noexcept {
  return state_;
}
std::unique_ptr<NativeRtcEventLogProduct> event_log_product_create(
    std::unique_ptr<NativeRtcEventLogFactory> factory, const NativeEnvironment& environment,
    rust::Box<RustEventLogRetention> retention, bool controlled, std::uint8_t& error) noexcept {
  error = 4;
  const bool builtin = !factory;
  // The pinned SDK kill switch returns a null logger without an output queue.
  const bool stock = builtin && !environment.environment().field_trials().IsEnabled("WebRTC-RtcEventLogKillSwitch");
  std::shared_ptr<NativeProductionLease> production;
  if (stock && !runtime_retirement_for(environment.environment())) {
    // Bare cooperative environments have no creator-side drain authority.
    // Bound runtime environments use their native alias retirement scope.
    if (controlled) { error = 6; return nullptr; }
    production = new_production_lease();
    if (!production) { error = 6; return nullptr; }
  }
  auto adapter = factory ? factory->CreateAdapter(false) : std::make_unique<webrtc::RtcEventLogFactory>();
  auto logger = adapter->Create(environment.environment());
  if (!logger) return nullptr;
  if (!builtin && rtc_event_log_adapter_failed(*logger)) { error = 7; return nullptr; }
  auto shared = MakeSharedLog(std::move(logger),
      builtin ? (stock ? SharedLogKind::Stock : SharedLogKind::Null) : SharedLogKind::Custom,
      environment.environment(),
      std::make_shared<rust::Box<RustEventLogRetention>>(std::move(retention)),
      std::move(production));
  auto state = static_cast<SharedEventLog&>(*shared).state();
  error = 0;
  return std::make_unique<NativeRtcEventLogProduct>(std::make_unique<NativeRtcEventLogProduct::State>(std::move(state)));
}
std::uint8_t event_log_product_start(NativeRtcEventLogProduct& log,
    std::unique_ptr<NativeEventLogOutput> output, std::int64_t period_ms) noexcept {
  auto& state = *log.state();
  if (const auto error = state.Check()) return error;
  if (!output || period_ms < 0) return 4;
  const auto error = state.logger->Start(output->Take(), period_ms);
  if (!error) state.started = true;
  return error;
}
std::uint8_t event_log_product_log(NativeRtcEventLogProduct& log,
    std::unique_ptr<NativeRtcEvent> event) noexcept {
  auto& state = *log.state();
  if (const auto error = state.Check()) return error;
  return event ? state.logger->Log(event->Take(), false) : 4;
}
std::uint8_t event_log_product_stop(NativeRtcEventLogProduct& log) noexcept {
  auto& state = *log.state();
  if (const auto error = state.Check()) return error;
  const auto error = state.logger->Stop();
  if (!error || error == 7) state.started = false;
  return error;
}
std::uint8_t event_log_product_stop_async(NativeRtcEventLogProduct& log,
    rust::Box<RustEventLogProductStop> completion) noexcept {
  auto& state = *log.state();
  if (const auto error = state.Check()) return error;
  auto notify = std::make_shared<rust::Box<RustEventLogProductStop>>(std::move(completion));
  const auto error = state.logger->StopAsync(
      [notify](bool failed) { event_log_product_stopped(**notify, failed); });
  if (!error) { state.initiated = true; state.started = false; }
  return error;
}
void event_log_product_close(NativeRtcEventLogProduct& log) noexcept {
  auto& state = *log.state();
  if (state.closed) return;
  state.closed = true;
  if (state.initiated) state.logger->WaitForCallbacks();
  if (state.started) state.logger->Stop();
  state.logger.reset();
}
std::unique_ptr<webrtc::RtcEventLog> take_event_log_product(NativeRtcEventLogProduct& log,
    std::uint8_t& error) noexcept {
  auto& state = *log.state();
  error = state.Check();
  if (error) return nullptr;
  // Moving an owner is not an SDK logger member call. Pending completion stays
  // admitted in the shared state; wrapper destruction must not stop the logger.
  auto result = std::make_unique<SharedEventLog>(std::move(state.logger));
  state.closed = true;
  return result;
}
}  // namespace pulsebeam::webrtc_sys
