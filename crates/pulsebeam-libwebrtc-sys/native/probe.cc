#include "pulsebeam-webrtc-sys/native/probe.h"

namespace pulsebeam::webrtc_sys {

rust::Str bridge_identity() noexcept {
  return "pulsebeam-webrtc-sys-bridge-v2";
}

}  // namespace pulsebeam::webrtc_sys
