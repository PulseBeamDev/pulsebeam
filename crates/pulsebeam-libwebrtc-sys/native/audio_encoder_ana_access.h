#pragma once

#include <cstdint>
#include <optional>
#include <vector>
#include "modules/audio_coding/audio_network_adaptor/controller_manager.h"
#include "system_wrappers/include/clock.h"
#include "modules/audio_coding/audio_network_adaptor/frame_length_controller.h"
#include "modules/audio_coding/audio_network_adaptor/frame_length_controller_v2.h"

namespace pulsebeam::webrtc_sys::ana_detail {

// Read-only inspection of the pinned manager's real reorder history. Explicit
// template instantiation permits naming these members without modifying the SDK
// class definition (and without layout casts, alternate algorithms or an ODR
// difference). All uses require the exact stock manager, its encoder lock and
// retained clock evidence proving the native timestamp subtraction safe.
// This header is private bridge implementation, never a Rust/CXX escape hatch.
template <typename Tag, typename Tag::Type Member>
struct ReadMember {
  friend constexpr typename Tag::Type ana_member(Tag) { return Member; }
};
struct LastTime {
  using Type = std::optional<std::int64_t> webrtc::ControllerManagerImpl::*;
  friend constexpr Type ana_member(LastTime);
};
struct Config {
  using Type = const webrtc::ControllerManagerImpl::Config webrtc::ControllerManagerImpl::*;
  friend constexpr Type ana_member(Config);
};
struct Environment {
  using Type = const webrtc::Environment webrtc::ControllerManagerImpl::*;
  friend constexpr Type ana_member(Environment);
};
struct Sorted {
  using Type = std::vector<webrtc::Controller*> webrtc::ControllerManagerImpl::*;
  friend constexpr Type ana_member(Sorted);
};
template struct ReadMember<LastTime, &webrtc::ControllerManagerImpl::last_reordering_time_ms_>;
template struct ReadMember<Config, &webrtc::ControllerManagerImpl::config_>;
template struct ReadMember<Environment, &webrtc::ControllerManagerImpl::env_>;
template struct ReadMember<Sorted, &webrtc::ControllerManagerImpl::sorted_controllers_>;
// These fields are read only immediately before the real native decision.
#define PULSEBEAM_ANA_READ(Tag, Owner, Value, Field) \
  struct Tag { using Type = Value Owner::*; friend constexpr Type ana_member(Tag); }; \
  template struct ReadMember<Tag, &Owner::Field>
using Legacy = webrtc::FrameLengthController;
using V2 = webrtc::FrameLengthControllerV2;
PULSEBEAM_ANA_READ(LegacyConfig, Legacy, const Legacy::Config, config_);
PULSEBEAM_ANA_READ(LegacyFrame, Legacy, std::set<int>::const_iterator, frame_length_ms_);
PULSEBEAM_ANA_READ(LegacyBandwidth, Legacy, std::optional<int>, uplink_bandwidth_bps_);
PULSEBEAM_ANA_READ(LegacyLoss, Legacy, std::optional<float>, uplink_packet_loss_fraction_);
PULSEBEAM_ANA_READ(LegacyOverhead, Legacy, std::optional<std::size_t>, overhead_bytes_per_packet_);
PULSEBEAM_ANA_READ(V2Frames, V2, std::vector<int>, encoder_frame_lengths_ms_);
PULSEBEAM_ANA_READ(V2Minimum, V2, const int, min_payload_bitrate_bps_);
PULSEBEAM_ANA_READ(V2Slow, V2, const bool, use_slow_adaptation_);
PULSEBEAM_ANA_READ(V2Bandwidth, V2, std::optional<int>, uplink_bandwidth_bps_);
PULSEBEAM_ANA_READ(V2Target, V2, std::optional<int>, target_bitrate_bps_);
PULSEBEAM_ANA_READ(V2Overhead, V2, std::optional<int>, overhead_bytes_per_packet_);
#undef PULSEBEAM_ANA_READ

struct LastPoint {
  friend float ana_distance(LastPoint, const webrtc::ControllerManagerImpl&, int, float);
};
template <auto Member>
struct ReadPoint {
  friend float ana_distance(LastPoint, const webrtc::ControllerManagerImpl& manager,
      int bandwidth, float loss) {
    const auto& previous = manager.*Member;
    auto current = previous;
    current.uplink_bandwidth_bps = bandwidth;
    current.uplink_packet_loss_fraction = loss;
    return previous.SquaredDistanceTo(current);
  }
};
template struct ReadPoint<&webrtc::ControllerManagerImpl::last_scoring_point_>;

// Called only with a nonempty scoring map and both metrics present, when a
// comparator preflight refused sorting. Preserve a genuine native cached-order
// result if the stock gates skip that sort. Query the original clock once,
// exactly where the native gated path would query it, and use the actual native
// squared-distance implementation. Successful sorting still delegates to the
// original manager without any extra clock sampling or shadow history.
inline std::optional<std::vector<webrtc::Controller*>> SkippedSort(
    const webrtc::ControllerManagerImpl& manager,
    const webrtc::Controller::NetworkMetrics& metrics, bool distance_context_safe) {
  const auto& last = manager.*ana_member(LastTime{});
  if (!last) return std::nullopt;
  const auto& env = manager.*ana_member(Environment{});
  const auto& config = manager.*ana_member(Config{});
  const auto now = env.clock().TimeInMilliseconds();
  const auto cached = [&] { return manager.*ana_member(Sorted{}); };
  if (now - *last < config.min_reordering_time_ms) return cached();
  // A rejected FP context must not be used to evaluate the distance preflight.
  if (!distance_context_safe) return std::nullopt;
  if (ana_distance(LastPoint{}, manager, *metrics.uplink_bandwidth_bps,
          *metrics.uplink_packet_loss_fraction) < config.min_reordering_squared_distance)
    return cached();
  return std::nullopt;
}

}  // namespace pulsebeam::webrtc_sys::ana_detail
