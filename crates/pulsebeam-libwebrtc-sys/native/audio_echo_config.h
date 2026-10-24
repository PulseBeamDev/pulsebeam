#pragma once
#include "api/audio/echo_canceller3_config.h"
namespace pulsebeam::webrtc_sys {
struct FfiEchoCanceller3Config;
webrtc::EchoCanceller3Config EchoConfigFromFfi(const FfiEchoCanceller3Config&);
FfiEchoCanceller3Config EchoConfigToFfi(const webrtc::EchoCanceller3Config&);
FfiEchoCanceller3Config audio_echo_config_default(bool) noexcept;
bool audio_echo_config_validate(FfiEchoCanceller3Config&) noexcept;
}
