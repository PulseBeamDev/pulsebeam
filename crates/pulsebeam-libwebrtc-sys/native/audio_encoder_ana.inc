// Private implementation included after NativeAudioEncoder::State is complete.
// No SDK definitions or numerical algorithms are replaced here.
namespace {
bool AudioEncoderTargetBitrateSafe(const NativeAudioEncoder::State&, int);

struct AnaControllerRuntime {
  std::vector<std::shared_ptr<AnaBitrateRuntime>> bitrates;
  void Begin() { for (auto& value : bitrates) value->failure = AnaBitrateFailure::None; }
};
struct OwnedAnaRuntime {
  explicit OwnedAnaRuntime(NativeAudioEncoder::State& value) : owner(value) {}
  NativeAudioEncoder::State& owner;
  std::shared_ptr<AnaSmoothingRuntime> smoothing;
  std::weak_ptr<AnaControllerRuntime> controllers;
  std::shared_ptr<const AudioNetworkAdaptorAdmission> pending;
  bool capable = false, active = false;
  std::uint8_t operation = 0, error = 0;
  void Begin(std::uint8_t op) {
    operation = op; error = 0;
    if (smoothing) smoothing->failure = AnaSmoothingFailure::None;
    if (auto group = controllers.lock()) group->Begin();
  }
  void Reject(std::uint8_t reason = 4) {
    if (!error) error = reason;
    std::uint32_t empty = 0;
    owner.sdk_failure->first.compare_exchange_strong(empty,
        (std::uint32_t{operation} << 8) | reason, std::memory_order_acq_rel);
  }
  void CheckSmoothing() {
    if (smoothing && !smoothing->healthy())
      Reject(smoothing->failure == AnaSmoothingFailure::Clock ? 2 : 4);
  }
};
class AnaOperation {
 public:
  AnaOperation(NativeAudioEncoder::State& owner, std::uint8_t operation) : runtime_(owner.ana) {
    if (runtime_) runtime_->Begin(operation);
  }
  std::uint8_t error() const {
    if (!runtime_) return 0;
    runtime_->CheckSmoothing(); return runtime_->error;
  }
 private:
  const std::shared_ptr<OwnedAnaRuntime> runtime_;
};

#if WEBRTC_ENABLE_PROTOBUF
bool AnaLegacyDecisionSafe(const webrtc::FrameLengthController& native) {
  using namespace ana_detail;
  const auto& config = native.*ana_member(LegacyConfig{});
  const auto frame = native.*ana_member(LegacyFrame{});
  const auto& bandwidth = native.*ana_member(LegacyBandwidth{});
  const auto& loss = native.*ana_member(LegacyLoss{});
  const auto& overhead = native.*ana_member(LegacyOverhead{});
  const auto rate = [&](int offset, int duration) {
    return AnaUnsignedOverheadRate(*overhead, offset, duration);
  };
  const auto sum_safe = [&](int offset, int duration) {
    if (duration <= 0) return false;
    const auto sum = std::int64_t{config.min_encoder_bitrate_bps} + 5000 + rate(offset, duration);
    return sum >= INT32_MIN && sum <= INT32_MAX;
  };
  auto longer = std::next(frame);
  auto increasing = config.fl_changing_bandwidths_bps.end();
  for (; longer != config.encoder_frame_lengths_ms.end(); ++longer) {
    increasing = config.fl_changing_bandwidths_bps.find({*frame, *longer});
    if (increasing != config.fl_changing_bandwidths_bps.end()) break;
  }
  if (increasing != config.fl_changing_bandwidths_bps.end()) {
#if RTC_DCHECK_IS_ON
    if (overhead && !AnaOffsetAssertSafe(*overhead, config.fl_increase_overhead_offset)) return false;
#endif
    if (bandwidth && overhead) {
      if (!sum_safe(config.fl_increase_overhead_offset, *frame)) return false;
      const auto threshold = std::int64_t{config.min_encoder_bitrate_bps} + 5000 + rate(config.fl_increase_overhead_offset, *frame);
      if (*bandwidth <= threshold) return true;  // Native never reaches decreasing.
    }
    if (bandwidth && *bandwidth <= increasing->second && loss &&
        *loss <= config.fl_increasing_packet_loss_fraction) return true;
  }
  auto shorter = frame;
  while (shorter != config.encoder_frame_lengths_ms.begin()) {
    --shorter;
    if (config.fl_changing_bandwidths_bps.find({*frame, *shorter}) != config.fl_changing_bandwidths_bps.end())
      return !bandwidth || !overhead || sum_safe(config.fl_decrease_overhead_offset, *shorter);
  }
  return true;
}
bool AnaV2DecisionSafe(const webrtc::FrameLengthControllerV2& native) {
  using namespace ana_detail;
  const auto& bandwidth = native.*ana_member(V2Bandwidth{});
  const auto& target = native.*ana_member(V2Target{});
  const auto& overhead = native.*ana_member(V2Overhead{});
  if (!bandwidth || !target || !overhead) return true;
  // V2 has no occupied-slot assertion: its native overwrite is intentional
  // observable behavior, unlike the asserting writers below.
  const auto product = std::int64_t{*overhead} * 8 * 1000;
  if (product < INT32_MIN || product > INT32_MAX) return false;
  const auto consumed = native.*ana_member(V2Slow{}) ? *bandwidth : *target;
  for (int frame : native.*ana_member(V2Frames{})) {
    if (frame <= 0) return false;
    const auto rate = static_cast<int>(product) / frame;
    if (!AnaIntDifferenceSafe(consumed, rate)) return false;
    if (consumed - rate > native.*ana_member(V2Minimum{})) break;
  }
  return true;
}

class OwnedAnaController final : public webrtc::Controller {
 public:
  OwnedAnaController(webrtc::Controller& native, int kind, OwnedAnaRuntime& runtime,
      const AudioNetworkAdaptorAdmission& admission, std::size_t index, const std::shared_ptr<AnaControllerRuntime>& group)
      : native_(native), kind_(kind), runtime_(runtime) {
    if (kind == AnaController::kBitrateController) {
      bitrate_status_ = std::make_shared<AnaBitrateRuntime>(); group->bitrates.push_back(bitrate_status_);
      const auto offsets = *admission.controller_bitrate_offsets[index];
      webrtc::audio_network_adaptor::BitrateController::Config config(
          runtime.owner.encoder->GetTargetBitrate(), admission.initial_frame_length_ms, offsets.increase, offsets.decrease);
      bitrate_ = std::make_unique<GuardedAnaBitrateController>(
          static_cast<webrtc::audio_network_adaptor::BitrateController&>(native), config, bitrate_status_);
    }
  }
  void UpdateNetworkMetrics(const NetworkMetrics& metrics) override {
    runtime_.CheckSmoothing(); if (runtime_.error) return;
    if (kind_ == AnaController::kFecController && !AnaSmoothingContextSafe()) { runtime_.Reject(); return; }
    if (bitrate_) { bitrate_->UpdateNetworkMetrics(metrics); CheckBitrate(); }
    else native_.UpdateNetworkMetrics(metrics);
  }
  void MakeDecision(webrtc::AudioEncoderRuntimeConfig* config) override {
    runtime_.CheckSmoothing(); if (runtime_.error) return;
    bool safe = config != nullptr;
    if (kind_ == AnaController::kFecController && !AnaSmoothingContextSafe()) safe = false;
    if (safe) switch (kind_) {
#if RTC_DCHECK_IS_ON
      case AnaController::kFecController: safe = !config->enable_fec && !config->uplink_packet_loss_fraction; break;
      case AnaController::kChannelController: safe = !config->num_channels; break;
      case AnaController::kDtxController: safe = !config->enable_dtx; break;
#endif
      case AnaController::kFrameLengthController:
        safe = AnaLegacyDecisionSafe(static_cast<const webrtc::FrameLengthController&>(native_));
#if RTC_DCHECK_IS_ON
        safe = safe && !config->frame_length_ms;
#endif
        break;
      case AnaController::kFrameLengthControllerV2:
        safe = AnaV2DecisionSafe(static_cast<const webrtc::FrameLengthControllerV2&>(native_)); break;
      default: break;
    }
    if (!safe) { runtime_.Reject(); return; }
    if (bitrate_) { bitrate_->MakeDecision(config); CheckBitrate(); }
    else native_.MakeDecision(config);
  }
 private:
  void CheckBitrate() { if (!bitrate_status_->healthy()) runtime_.Reject(); }
  webrtc::Controller& native_;
  const int kind_;
  OwnedAnaRuntime& runtime_;
  std::shared_ptr<AnaBitrateRuntime> bitrate_status_;
  std::unique_ptr<GuardedAnaBitrateController> bitrate_;
};
class OwnedAnaManager final : public webrtc::ControllerManager {
 public:
  OwnedAnaManager(std::unique_ptr<webrtc::ControllerManager> native, OwnedAnaRuntime& runtime,
      std::shared_ptr<const AudioNetworkAdaptorAdmission> admission,
      std::shared_ptr<AnaControllerRuntime> group)
      : native_(std::move(native)), runtime_(runtime), admission_(std::move(admission)), group_(std::move(group)), handoff_(runtime) {
    const auto controllers = native_->GetControllers();
    if (controllers.size() != admission_->controller_kinds.size()) { runtime_.Reject(); return; }
    for (std::size_t i = 0; i < controllers.size(); ++i) {
      auto proxy = std::make_unique<OwnedAnaController>(*controllers[i], admission_->controller_kinds[i], runtime_, *admission_, i, group_);
      mapped_.emplace(controllers[i], proxy.get()); proxies_.push_back(std::move(proxy));
    }
  }
  std::vector<webrtc::Controller*> GetControllers() const override {
    std::vector<webrtc::Controller*> result;
    for (const auto& proxy : proxies_) result.push_back(proxy.get());
    return result;
  }
  // Execute the real native decision pipeline once, before entering the
  // adaptor's statistics/event-log writer. Rejected partial decisions must not
  // be recorded as successful ANA actions. Only a complete genuine native
  // result is handed to the real adaptor's ordinary writer below.
  bool Prepare(const webrtc::Controller::NetworkMetrics& metrics) {
    handoff_.value.reset();
    auto selected = Select(metrics);
    webrtc::AudioEncoderRuntimeConfig config;
    for (auto* controller : selected) {
      if (runtime_.error) return false;
      controller->MakeDecision(&config);
    }
    if (runtime_.error) return false;
    handoff_.value = std::move(config); return true;
  }
  const webrtc::AudioEncoderRuntimeConfig& prepared() const { return *handoff_.value; }
  void Discard() { handoff_.value.reset(); }
  std::vector<webrtc::Controller*> GetSortedControllers(const webrtc::Controller::NetworkMetrics&) override {
    if (!handoff_.value || runtime_.error) { runtime_.Reject(); return {}; }
    return {&handoff_};
  }
 private:
  class DecisionHandoff final : public webrtc::Controller {
   public:
    explicit DecisionHandoff(OwnedAnaRuntime& runtime) : runtime_(runtime) {}
    std::optional<webrtc::AudioEncoderRuntimeConfig> value;
    void UpdateNetworkMetrics(const NetworkMetrics&) override { runtime_.Reject(); }
    void MakeDecision(webrtc::AudioEncoderRuntimeConfig* output) override {
      if (!output || !value) { runtime_.Reject(); return; }
      *output = std::move(*value); value.reset();
    }
   private:
    OwnedAnaRuntime& runtime_;
  };
  std::vector<webrtc::Controller*> Select(const webrtc::Controller::NetworkMetrics& metrics) {
    runtime_.CheckSmoothing(); if (runtime_.error) return {};
    if (!AnaScoringOrderSafe(*admission_, metrics.uplink_bandwidth_bps, metrics.uplink_packet_loss_fraction)) {
      // Create returns this concrete final stock type. Inspect only its actual
      // skip gates; do not sample time on a successful ordinary sorting path.
      auto skipped = ana_detail::SkippedSort(static_cast<const webrtc::ControllerManagerImpl&>(*native_), metrics, AnaSmoothingContextSafe());
      if (skipped) return Map(*skipped);
      runtime_.Reject(); return {};
    }
    return Map(native_->GetSortedControllers(metrics));
  }
 private:
  std::vector<webrtc::Controller*> Map(const std::vector<webrtc::Controller*>& selected) {
    std::vector<webrtc::Controller*> result;
    for (auto* controller : selected) {
      const auto found = mapped_.find(controller);
      if (found == mapped_.end()) { runtime_.Reject(); return {}; }
      result.push_back(found->second);
    }
    return result;
  }
  // Destroy proxies before the exact manager which owns their native targets.
  const std::unique_ptr<webrtc::ControllerManager> native_;
  OwnedAnaRuntime& runtime_;
  const std::shared_ptr<const AudioNetworkAdaptorAdmission> admission_;
  const std::shared_ptr<AnaControllerRuntime> group_;
  DecisionHandoff handoff_;
  std::vector<std::unique_ptr<OwnedAnaController>> proxies_;
  std::unordered_map<webrtc::Controller*, webrtc::Controller*> mapped_;
};
class OwnedAnaAdaptor final : public webrtc::AudioNetworkAdaptor {
 public:
  OwnedAnaAdaptor(OwnedAnaRuntime& runtime, OwnedAnaManager& manager, std::unique_ptr<webrtc::AudioNetworkAdaptor> native)
      : runtime_(runtime), manager_(manager), native_(std::move(native)) {}
  // Exact sparse updates forwarded to the native last_metrics_, not sampled or
  // hypothetical values. The pinned implementation has no other metric writer.
  void SetUplinkBandwidth(int value) override {
    if (Healthy()) { native_->SetUplinkBandwidth(value); metrics_.uplink_bandwidth_bps = value; }
  }
  void SetUplinkPacketLossFraction(float value) override {
    if (Healthy()) { native_->SetUplinkPacketLossFraction(value); metrics_.uplink_packet_loss_fraction = value; }
  }
  void SetRtt(int value) override {
    if (Healthy()) { native_->SetRtt(value); metrics_.rtt_ms = value; }
  }
  void SetTargetAudioBitrate(int value) override {
    if (Healthy()) { native_->SetTargetAudioBitrate(value); metrics_.target_audio_bitrate_bps = value; }
  }
  void SetOverhead(std::size_t value) override {
    if (Healthy()) { native_->SetOverhead(value); metrics_.overhead_bytes_per_packet = value; }
  }
  webrtc::AudioEncoderRuntimeConfig GetEncoderRuntimeConfig() override {
    if (!Healthy() || !manager_.Prepare(metrics_)) return {};
    const auto& decision = manager_.prepared();
    if (decision.bitrate_bps && !AudioEncoderTargetBitrateSafe(runtime_.owner, *decision.bitrate_bps)) runtime_.Reject(1);
    if (decision.uplink_packet_loss_fraction) {
      const float loss = *decision.uplink_packet_loss_fraction;
#if RTC_DCHECK_IS_ON
      if (!(loss >= 0.0f && loss <= 1.0f)) runtime_.Reject();
#endif
      // The new-format native event logger eventually narrows loss*0x3fff to
      // uint32_t. Check the actual native estimate, not a predicted recurrence
      // envelope; legacy logging and stats retain its exact admitted value.
      const float encoded = loss * 0x3fff;
      if (!std::isfinite(encoded) || encoded <= -1.0f || encoded >= 0x1p32f) runtime_.Reject();
    }
    if (!Healthy()) { manager_.Discard(); return {}; }
    // Native controller algorithms and clock reads have already run once in
    // native order. Hand their exact complete result to the native adaptor for
    // its unchanged statistics/event logging, without re-running decisions.
    return native_->GetEncoderRuntimeConfig();
  }
  void StartDebugDump(FILE* file) override { native_->StartDebugDump(file); }
  void StopDebugDump() override { native_->StopDebugDump(); }
  webrtc::ANAStats GetStats() const override { return native_->GetStats(); }
 private:
  bool Healthy() { runtime_.CheckSmoothing(); return runtime_.error == 0; }
  OwnedAnaRuntime& runtime_;
  OwnedAnaManager& manager_;  // Owned transitively by native_.
  webrtc::Controller::NetworkMetrics metrics_;
  const std::unique_ptr<webrtc::AudioNetworkAdaptor> native_;
};
#endif

std::unique_ptr<webrtc::AudioNetworkAdaptor> CreateOwnedAna(const std::shared_ptr<OwnedAnaRuntime>& runtime,
    absl::string_view config) {
#if WEBRTC_ENABLE_PROTOBUF
  if (!runtime->capable || !runtime->pending || !runtime->owner.encoder) { runtime->Reject(); return nullptr; }
  auto& opus = static_cast<webrtc::AudioEncoderOpusImpl&>(*runtime->owner.encoder);
  auto native = webrtc::ControllerManagerImpl::Create(runtime->owner.environment, config,
      opus.NumChannels(), opus.supported_frame_lengths_ms(), webrtc::AudioEncoderOpusConfig::kMinBitrateBps,
      opus.num_channels_to_encode(), opus.next_frame_length_ms(), opus.GetTargetBitrate(), opus.fec_enabled(), opus.GetDtx());
  if (!native) { runtime->Reject(); return nullptr; }
  auto group = std::make_shared<AnaControllerRuntime>();
  auto manager = std::make_unique<OwnedAnaManager>(std::move(native), *runtime, runtime->pending, group);
  if (runtime->error) return nullptr;
  runtime->controllers = group;
  auto* selected = manager.get();
  auto adaptor = std::make_unique<webrtc::AudioNetworkAdaptorImpl>(runtime->owner.environment, std::move(manager));
  return std::make_unique<OwnedAnaAdaptor>(*runtime, *selected, std::move(adaptor));
#else
  runtime->Reject(3); return nullptr;
#endif
}
std::unique_ptr<webrtc::AudioEncoder> CreateOwnedOpus(NativeAudioEncoder::State& owner,
    webrtc::AudioEncoderOpusConfig config, int payload_type) {
  auto runtime = std::make_shared<OwnedAnaRuntime>(owner);
  std::unique_ptr<webrtc::SmoothingFilter> smoother;
  if (owner.ana_clock_evidence) {
    runtime->smoothing = std::make_shared<AnaSmoothingRuntime>(*owner.ana_clock_evidence);
    smoother = BoundedAnaBitrateSmoother::Create(runtime->smoothing);
    runtime->capable = bool(smoother);
  }
  // Without a usable ANA proof ordinary stock encoding remains available. Its
  // untouched default smoother is never consumed: enable is refused beforehand.
  if (!smoother) smoother = std::make_unique<webrtc::SmoothingFilterImpl>(5000);
  owner.ana = runtime;
  return webrtc::AudioEncoderOpusImpl::CreateForTesting(owner.environment, std::move(config), payload_type,
      [runtime](absl::string_view bytes) { return CreateOwnedAna(runtime, bytes); }, std::move(smoother));
}
}  // namespace
