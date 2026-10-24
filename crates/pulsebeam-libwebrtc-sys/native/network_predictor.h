#pragma once
#include <memory>
#include "api/network_state_predictor.h"
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
struct RustNetworkStatePredictorFactory;
struct FfiNetworkPredictorInput;
std::unique_ptr<webrtc::NetworkStatePredictorFactoryInterface>
network_predictor_factory_adapter(
    rust::Box<RustNetworkStatePredictorFactory> source) noexcept;
rust::Vec<std::int32_t> network_predictor_native_probe(
    rust::Box<RustNetworkStatePredictorFactory> source,
    rust::Slice<const FfiNetworkPredictorInput> inputs) noexcept;
}  // namespace pulsebeam::webrtc_sys
