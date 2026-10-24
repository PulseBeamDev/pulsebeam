#pragma once
#include <memory>
#include "api/transport/network_control.h"
#include "rust/cxx.h"
namespace pulsebeam::webrtc_sys {
class NativeEnvironment;
struct RustNetworkControllerFactory;
struct FfiNcMessage;
struct FfiNcNetworkControlUpdate;
std::unique_ptr<webrtc::NetworkControllerFactoryInterface> network_controller_factory_adapter(rust::Box<RustNetworkControllerFactory> source) noexcept;
bool network_controller_injection_enabled(const NativeEnvironment& environment) noexcept;
rust::Vec<FfiNcNetworkControlUpdate> network_controller_native_probe(const NativeEnvironment& environment, rust::Box<RustNetworkControllerFactory> source, rust::Slice<const FfiNcMessage> messages, rust::Slice<const std::uint8_t> kinds) noexcept;
}
