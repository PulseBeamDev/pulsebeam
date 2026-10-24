#include "pulsebeam-webrtc-sys/native/network_predictor.h"
#include <utility>
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {
namespace {
class PredictorAdapter final : public webrtc::NetworkStatePredictor {
 public:
  explicit PredictorAdapter(rust::Box<RustNetworkStatePredictor> source)
      : source_(std::move(source)) {}
  webrtc::BandwidthUsage Update(std::int64_t send_time_ms,
                               std::int64_t arrival_time_ms,
                               webrtc::BandwidthUsage state) override {
    // BandwidthUsage is a scoped enum with fixed int underlying type. Preserve
    // the full signed state domain instead of normalizing unknown values.
    return static_cast<webrtc::BandwidthUsage>(network_predictor_update(
        *source_, send_time_ms, arrival_time_ms,
        static_cast<std::int32_t>(state)));
  }
 private:
  rust::Box<RustNetworkStatePredictor> source_;
};
class PredictorFactoryAdapter final
    : public webrtc::NetworkStatePredictorFactoryInterface {
 public:
  explicit PredictorFactoryAdapter(
      rust::Box<RustNetworkStatePredictorFactory> source)
      : source_(std::move(source)) {}
  std::unique_ptr<webrtc::NetworkStatePredictor>
  CreateNetworkStatePredictor() override {
    auto source = network_predictor_create(*source_);
    if (!network_predictor_present(*source)) return nullptr;
    return std::make_unique<PredictorAdapter>(std::move(source));
  }
 private:
  rust::Box<RustNetworkStatePredictorFactory> source_;
};
}  // namespace
std::unique_ptr<webrtc::NetworkStatePredictorFactoryInterface>
network_predictor_factory_adapter(
    rust::Box<RustNetworkStatePredictorFactory> source) noexcept {
  if (!network_predictor_factory_present(*source)) return nullptr;
  return std::make_unique<PredictorFactoryAdapter>(std::move(source));
}

rust::Vec<std::int32_t> network_predictor_native_probe(
    rust::Box<RustNetworkStatePredictorFactory> source,
    rust::Slice<const FfiNetworkPredictorInput> inputs) noexcept {
  rust::Vec<std::int32_t> results;
  auto factory = network_predictor_factory_adapter(std::move(source));
  if (!factory) return results;
  auto predictor = factory->CreateNetworkStatePredictor();
  // Products must remain independent of the factory owning the borrowed
  // configuration pointers. Exercise that native destruction order explicitly.
  factory.reset();
  if (!predictor) return results;
  for (const auto& input : inputs)
    results.push_back(static_cast<std::int32_t>(predictor->Update(
        input.send_time_ms, input.arrival_time_ms,
        static_cast<webrtc::BandwidthUsage>(input.state))));
  return results;
}
}  // namespace pulsebeam::webrtc_sys
