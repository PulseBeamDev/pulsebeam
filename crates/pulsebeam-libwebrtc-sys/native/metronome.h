#pragma once
#include <memory>
#include "rust/cxx.h"
namespace webrtc { class Metronome; }
namespace pulsebeam::webrtc_sys {
struct RustMetronome;
class NativeMetronomeTick final {
 public:
  struct State;
  explicit NativeMetronomeTick(std::shared_ptr<State> state) noexcept;
  ~NativeMetronomeTick();
  bool Signal() noexcept;
 private:
  std::shared_ptr<State> state_;
};
bool metronome_tick_signal(std::unique_ptr<NativeMetronomeTick> tick) noexcept;
std::unique_ptr<webrtc::Metronome> metronome_adapter(rust::Box<RustMetronome> source) noexcept;
}  // namespace pulsebeam::webrtc_sys
