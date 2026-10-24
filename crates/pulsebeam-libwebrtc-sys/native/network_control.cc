#include "pulsebeam-webrtc-sys/native/network_control.h"
#include <limits>
#include <utility>
#include "api/units/data_size.h"
#include "api/transport/goog_cc_factory.h"
#include "pulsebeam-webrtc-sys/native/network_predictor.h"
#include "system_wrappers/include/clock.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {
namespace {
constexpr auto kPlus = std::numeric_limits<std::int64_t>::max();
constexpr auto kMinus = std::numeric_limits<std::int64_t>::min();
std::int64_t Pack(webrtc::Timestamp value) { return value.IsPlusInfinity() ? kPlus : value.IsMinusInfinity() ? kMinus : value.us(); }
std::int64_t Pack(webrtc::TimeDelta value) { return value.IsPlusInfinity() ? kPlus : value.IsMinusInfinity() ? kMinus : value.us(); }
FfiNcMagnitude Pack(webrtc::DataRate value) { return {value.IsFinite() ? value.bps() : 0, static_cast<std::int8_t>(value.IsPlusInfinity() ? 1 : value.IsMinusInfinity() ? -1 : 0)}; }
FfiNcMagnitude Pack(webrtc::DataSize value) { return {value.IsFinite() ? value.bytes() : 0, static_cast<std::int8_t>(value.IsPlusInfinity() ? 1 : value.IsMinusInfinity() ? -1 : 0)}; }
webrtc::Timestamp NativeTimestamp(std::int64_t value) { return value == kPlus ? webrtc::Timestamp::PlusInfinity() : value == kMinus ? webrtc::Timestamp::MinusInfinity() : webrtc::Timestamp::Micros(value); }
webrtc::TimeDelta NativeDelta(std::int64_t value) { return value == kPlus ? webrtc::TimeDelta::PlusInfinity() : value == kMinus ? webrtc::TimeDelta::MinusInfinity() : webrtc::TimeDelta::Micros(value); }
webrtc::DataRate NativeRate(const FfiNcMagnitude& value, bool& valid) {
  if (value.kind == 1) return webrtc::DataRate::PlusInfinity();
  if (value.kind == -1) return webrtc::DataRate::MinusInfinity();
  if (value.kind != 0 || value.value < 0 || value.value == kPlus) { valid = false; return webrtc::DataRate::Zero(); }
  return webrtc::DataRate::BitsPerSec(value.value);
}
webrtc::DataSize NativeSize(const FfiNcMagnitude& value, bool& valid) {
  if (value.kind == 1) return webrtc::DataSize::PlusInfinity();
  if (value.kind == -1) return webrtc::DataSize::MinusInfinity();
  if (value.kind != 0 || value.value < 0 || value.value == kPlus) { valid = false; return webrtc::DataSize::Zero(); }
  return webrtc::DataSize::Bytes(value.value);
}
FfiNcBitrateAllocationLimits Pack(const webrtc::BitrateAllocationLimits& value) {
  FfiNcBitrateAllocationLimits result{};
  result.min_allocatable_rate = Pack(value.min_allocatable_rate);
  result.max_allocatable_rate = Pack(value.max_allocatable_rate);
  result.max_padding_rate = Pack(value.max_padding_rate);
  return result;
}
webrtc::BitrateAllocationLimits Unpack(const FfiNcBitrateAllocationLimits& value, bool& valid) {
  webrtc::BitrateAllocationLimits result{};
  result.min_allocatable_rate = NativeRate(value.min_allocatable_rate, valid);
  result.max_allocatable_rate = NativeRate(value.max_allocatable_rate, valid);
  result.max_padding_rate = NativeRate(value.max_padding_rate, valid);
  return result;
}
FfiNcStreamsConfig Pack(const webrtc::StreamsConfig& value) {
  FfiNcStreamsConfig result{};
  result.at_time = Pack(value.at_time);
  result.has_requests_alr_probing = value.requests_alr_probing.has_value();
  if (value.requests_alr_probing) result.requests_alr_probing = *value.requests_alr_probing;
  result.enable_repeated_initial_probing = value.enable_repeated_initial_probing;
  result.has_pacing_factor = value.pacing_factor.has_value();
  if (value.pacing_factor) result.pacing_factor = *value.pacing_factor;
  result.has_min_total_allocated_bitrate = value.min_total_allocated_bitrate.has_value();
  if (value.min_total_allocated_bitrate) result.min_total_allocated_bitrate = Pack(*value.min_total_allocated_bitrate);
  result.has_max_padding_rate = value.max_padding_rate.has_value();
  if (value.max_padding_rate) result.max_padding_rate = Pack(*value.max_padding_rate);
  result.has_max_total_allocated_bitrate = value.max_total_allocated_bitrate.has_value();
  if (value.max_total_allocated_bitrate) result.max_total_allocated_bitrate = Pack(*value.max_total_allocated_bitrate);
  return result;
}
webrtc::StreamsConfig Unpack(const FfiNcStreamsConfig& value, bool& valid) {
  webrtc::StreamsConfig result{};
  result.at_time = NativeTimestamp(value.at_time);
  if (value.has_requests_alr_probing) result.requests_alr_probing = value.requests_alr_probing;
  result.enable_repeated_initial_probing = value.enable_repeated_initial_probing;
  if (value.has_pacing_factor) result.pacing_factor = value.pacing_factor;
  if (value.has_min_total_allocated_bitrate) result.min_total_allocated_bitrate = NativeRate(value.min_total_allocated_bitrate, valid);
  if (value.has_max_padding_rate) result.max_padding_rate = NativeRate(value.max_padding_rate, valid);
  if (value.has_max_total_allocated_bitrate) result.max_total_allocated_bitrate = NativeRate(value.max_total_allocated_bitrate, valid);
  return result;
}
FfiNcTargetRateConstraints Pack(const webrtc::TargetRateConstraints& value) {
  FfiNcTargetRateConstraints result{};
  result.at_time = Pack(value.at_time);
  result.has_min_data_rate = value.min_data_rate.has_value();
  if (value.min_data_rate) result.min_data_rate = Pack(*value.min_data_rate);
  result.has_max_data_rate = value.max_data_rate.has_value();
  if (value.max_data_rate) result.max_data_rate = Pack(*value.max_data_rate);
  result.has_starting_rate = value.starting_rate.has_value();
  if (value.starting_rate) result.starting_rate = Pack(*value.starting_rate);
  return result;
}
webrtc::TargetRateConstraints Unpack(const FfiNcTargetRateConstraints& value, bool& valid) {
  webrtc::TargetRateConstraints result{};
  result.at_time = NativeTimestamp(value.at_time);
  if (value.has_min_data_rate) result.min_data_rate = NativeRate(value.min_data_rate, valid);
  if (value.has_max_data_rate) result.max_data_rate = NativeRate(value.max_data_rate, valid);
  if (value.has_starting_rate) result.starting_rate = NativeRate(value.starting_rate, valid);
  return result;
}
FfiNcNetworkAvailability Pack(const webrtc::NetworkAvailability& value) {
  FfiNcNetworkAvailability result{};
  result.at_time = Pack(value.at_time);
  result.network_available = value.network_available;
  return result;
}
webrtc::NetworkAvailability Unpack(const FfiNcNetworkAvailability& value, bool& valid) {
  webrtc::NetworkAvailability result{};
  result.at_time = NativeTimestamp(value.at_time);
  result.network_available = value.network_available;
  return result;
}
FfiNcNetworkRouteChange Pack(const webrtc::NetworkRouteChange& value) {
  FfiNcNetworkRouteChange result{};
  result.at_time = Pack(value.at_time);
  result.constraints = Pack(value.constraints);
  result.restart_bwe = value.restart_bwe;
  return result;
}
webrtc::NetworkRouteChange Unpack(const FfiNcNetworkRouteChange& value, bool& valid) {
  webrtc::NetworkRouteChange result{};
  result.at_time = NativeTimestamp(value.at_time);
  result.constraints = Unpack(value.constraints, valid);
  result.restart_bwe = value.restart_bwe;
  return result;
}
FfiNcPacedPacketInfo Pack(const webrtc::PacedPacketInfo& value) {
  FfiNcPacedPacketInfo result{};
  result.send_bitrate = Pack(value.send_bitrate);
  result.probe_cluster_id = value.probe_cluster_id;
  result.probe_cluster_min_probes = value.probe_cluster_min_probes;
  result.probe_cluster_min_bytes = value.probe_cluster_min_bytes;
  result.probe_cluster_bytes_sent = value.probe_cluster_bytes_sent;
  return result;
}
webrtc::PacedPacketInfo Unpack(const FfiNcPacedPacketInfo& value, bool& valid) {
  webrtc::PacedPacketInfo result{};
  result.send_bitrate = NativeRate(value.send_bitrate, valid);
  result.probe_cluster_id = value.probe_cluster_id;
  result.probe_cluster_min_probes = value.probe_cluster_min_probes;
  result.probe_cluster_min_bytes = value.probe_cluster_min_bytes;
  result.probe_cluster_bytes_sent = value.probe_cluster_bytes_sent;
  return result;
}
FfiNcSentPacket Pack(const webrtc::SentPacket& value) {
  FfiNcSentPacket result{};
  result.send_time = Pack(value.send_time);
  result.size = Pack(value.size);
  result.prior_unacked_data = Pack(value.prior_unacked_data);
  result.pacing_info = Pack(value.pacing_info);
  result.audio = value.audio;
  result.sequence_number = value.sequence_number;
  result.data_in_flight = Pack(value.data_in_flight);
  return result;
}
webrtc::SentPacket Unpack(const FfiNcSentPacket& value, bool& valid) {
  webrtc::SentPacket result{};
  result.send_time = NativeTimestamp(value.send_time);
  result.size = NativeSize(value.size, valid);
  result.prior_unacked_data = NativeSize(value.prior_unacked_data, valid);
  result.pacing_info = Unpack(value.pacing_info, valid);
  result.audio = value.audio;
  result.sequence_number = value.sequence_number;
  result.data_in_flight = NativeSize(value.data_in_flight, valid);
  return result;
}
FfiNcReceivedPacket Pack(const webrtc::ReceivedPacket& value) {
  FfiNcReceivedPacket result{};
  result.send_time = Pack(value.send_time);
  result.receive_time = Pack(value.receive_time);
  result.size = Pack(value.size);
  return result;
}
webrtc::ReceivedPacket Unpack(const FfiNcReceivedPacket& value, bool& valid) {
  webrtc::ReceivedPacket result{};
  result.send_time = NativeTimestamp(value.send_time);
  result.receive_time = NativeTimestamp(value.receive_time);
  result.size = NativeSize(value.size, valid);
  return result;
}
FfiNcRemoteBitrateReport Pack(const webrtc::RemoteBitrateReport& value) {
  FfiNcRemoteBitrateReport result{};
  result.receive_time = Pack(value.receive_time);
  result.bandwidth = Pack(value.bandwidth);
  return result;
}
webrtc::RemoteBitrateReport Unpack(const FfiNcRemoteBitrateReport& value, bool& valid) {
  webrtc::RemoteBitrateReport result{};
  result.receive_time = NativeTimestamp(value.receive_time);
  result.bandwidth = NativeRate(value.bandwidth, valid);
  return result;
}
FfiNcRoundTripTimeUpdate Pack(const webrtc::RoundTripTimeUpdate& value) {
  FfiNcRoundTripTimeUpdate result{};
  result.receive_time = Pack(value.receive_time);
  result.round_trip_time = Pack(value.round_trip_time);
  result.smoothed = value.smoothed;
  return result;
}
webrtc::RoundTripTimeUpdate Unpack(const FfiNcRoundTripTimeUpdate& value, bool& valid) {
  webrtc::RoundTripTimeUpdate result{};
  result.receive_time = NativeTimestamp(value.receive_time);
  result.round_trip_time = NativeDelta(value.round_trip_time);
  result.smoothed = value.smoothed;
  return result;
}
FfiNcTransportLossReport Pack(const webrtc::TransportLossReport& value) {
  FfiNcTransportLossReport result{};
  result.receive_time = Pack(value.receive_time);
  result.start_time = Pack(value.start_time);
  result.end_time = Pack(value.end_time);
  result.packets_lost_delta = value.packets_lost_delta;
  result.packets_received_delta = value.packets_received_delta;
  return result;
}
webrtc::TransportLossReport Unpack(const FfiNcTransportLossReport& value, bool& valid) {
  webrtc::TransportLossReport result{};
  result.receive_time = NativeTimestamp(value.receive_time);
  result.start_time = NativeTimestamp(value.start_time);
  result.end_time = NativeTimestamp(value.end_time);
  result.packets_lost_delta = value.packets_lost_delta;
  result.packets_received_delta = value.packets_received_delta;
  return result;
}
FfiNcRtpPacketInfo Pack(const webrtc::PacketResult::RtpPacketInfo& value) {
  FfiNcRtpPacketInfo result{};
  result.ssrc = value.ssrc;
  result.rtp_sequence_number = value.rtp_sequence_number;
  result.is_retransmission = value.is_retransmission;
  return result;
}
webrtc::PacketResult::RtpPacketInfo Unpack(const FfiNcRtpPacketInfo& value, bool& valid) {
  webrtc::PacketResult::RtpPacketInfo result{};
  result.ssrc = value.ssrc;
  result.rtp_sequence_number = value.rtp_sequence_number;
  result.is_retransmission = value.is_retransmission;
  return result;
}
FfiNcPacketResult Pack(const webrtc::PacketResult& value) {
  FfiNcPacketResult result{};
  result.sent_packet = Pack(value.sent_packet);
  result.receive_time = Pack(value.receive_time);
  result.has_arrival_time_offset = value.arrival_time_offset.has_value();
  if (value.arrival_time_offset) result.arrival_time_offset = Pack(*value.arrival_time_offset);
  result.ecn = static_cast<std::uint8_t>(value.ecn);
  result.sent_with_ect1 = value.sent_with_ect1;
  result.reported_lost_for_the_first_time = value.reported_lost_for_the_first_time;
  result.reported_recovered_for_the_first_time = value.reported_recovered_for_the_first_time;
  result.has_rtp_packet_info = value.rtp_packet_info.has_value();
  if (value.rtp_packet_info) result.rtp_packet_info = Pack(*value.rtp_packet_info);
  return result;
}
webrtc::PacketResult Unpack(const FfiNcPacketResult& value, bool& valid) {
  webrtc::PacketResult result{};
  result.sent_packet = Unpack(value.sent_packet, valid);
  result.receive_time = NativeTimestamp(value.receive_time);
  if (value.has_arrival_time_offset) result.arrival_time_offset = NativeDelta(value.arrival_time_offset);
  result.ecn = static_cast<webrtc::EcnMarking>(value.ecn);
  result.sent_with_ect1 = value.sent_with_ect1;
  result.reported_lost_for_the_first_time = value.reported_lost_for_the_first_time;
  result.reported_recovered_for_the_first_time = value.reported_recovered_for_the_first_time;
  if (value.has_rtp_packet_info) result.rtp_packet_info = Unpack(value.rtp_packet_info, valid);
  return result;
}
FfiNcTransportPacketsFeedback Pack(const webrtc::TransportPacketsFeedback& value) {
  FfiNcTransportPacketsFeedback result{};
  result.feedback_time = Pack(value.feedback_time);
  result.data_in_flight = Pack(value.data_in_flight);
  result.transport_supports_ecn = value.transport_supports_ecn;
  result.packet_feedbacks.reserve(value.packet_feedbacks.size());
  for (const auto& item : value.packet_feedbacks) result.packet_feedbacks.push_back(Pack(item));
  result.sendless_arrival_times.reserve(value.sendless_arrival_times.size());
  for (const auto& item : value.sendless_arrival_times) result.sendless_arrival_times.push_back(Pack(item));
  return result;
}
webrtc::TransportPacketsFeedback Unpack(const FfiNcTransportPacketsFeedback& value, bool& valid) {
  webrtc::TransportPacketsFeedback result{};
  result.feedback_time = NativeTimestamp(value.feedback_time);
  result.data_in_flight = NativeSize(value.data_in_flight, valid);
  result.transport_supports_ecn = value.transport_supports_ecn;
  result.packet_feedbacks.reserve(value.packet_feedbacks.size());
  for (const auto& item : value.packet_feedbacks) result.packet_feedbacks.push_back(Unpack(item, valid));
  result.sendless_arrival_times.reserve(value.sendless_arrival_times.size());
  for (const auto& item : value.sendless_arrival_times) result.sendless_arrival_times.push_back(NativeTimestamp(item));
  return result;
}
FfiNcNetworkEstimate Pack(const webrtc::NetworkEstimate& value) {
  FfiNcNetworkEstimate result{};
  result.at_time = Pack(value.at_time);
  result.bandwidth = Pack(value.bandwidth);
  result.round_trip_time = Pack(value.round_trip_time);
  result.bwe_period = Pack(value.bwe_period);
  result.loss_rate_ratio = value.loss_rate_ratio;
  return result;
}
webrtc::NetworkEstimate Unpack(const FfiNcNetworkEstimate& value, bool& valid) {
  webrtc::NetworkEstimate result{};
  result.at_time = NativeTimestamp(value.at_time);
  result.bandwidth = NativeRate(value.bandwidth, valid);
  result.round_trip_time = NativeDelta(value.round_trip_time);
  result.bwe_period = NativeDelta(value.bwe_period);
  result.loss_rate_ratio = value.loss_rate_ratio;
  return result;
}
FfiNcPacerConfig Pack(const webrtc::PacerConfig& value) {
  FfiNcPacerConfig result{};
  result.at_time = Pack(value.at_time);
  result.data_window = Pack(value.data_window);
  result.time_window = Pack(value.time_window);
  result.pad_window = Pack(value.pad_window);
  return result;
}
webrtc::PacerConfig Unpack(const FfiNcPacerConfig& value, bool& valid) {
  webrtc::PacerConfig result{};
  result.at_time = NativeTimestamp(value.at_time);
  result.data_window = NativeSize(value.data_window, valid);
  result.time_window = NativeDelta(value.time_window);
  result.pad_window = NativeSize(value.pad_window, valid);
  return result;
}
FfiNcProbeClusterConfig Pack(const webrtc::ProbeClusterConfig& value) {
  FfiNcProbeClusterConfig result{};
  result.at_time = Pack(value.at_time);
  result.target_data_rate = Pack(value.target_data_rate);
  result.target_duration = Pack(value.target_duration);
  result.min_probe_delta = Pack(value.min_probe_delta);
  result.target_probe_count = value.target_probe_count;
  result.id = value.id;
  return result;
}
webrtc::ProbeClusterConfig Unpack(const FfiNcProbeClusterConfig& value, bool& valid) {
  webrtc::ProbeClusterConfig result{};
  result.at_time = NativeTimestamp(value.at_time);
  result.target_data_rate = NativeRate(value.target_data_rate, valid);
  result.target_duration = NativeDelta(value.target_duration);
  result.min_probe_delta = NativeDelta(value.min_probe_delta);
  result.target_probe_count = value.target_probe_count;
  result.id = value.id;
  return result;
}
FfiNcTargetTransferRate Pack(const webrtc::TargetTransferRate& value) {
  FfiNcTargetTransferRate result{};
  result.at_time = Pack(value.at_time);
  result.network_estimate = Pack(value.network_estimate);
  result.target_rate = Pack(value.target_rate);
  result.cwnd_reduce_ratio = value.cwnd_reduce_ratio;
  return result;
}
webrtc::TargetTransferRate Unpack(const FfiNcTargetTransferRate& value, bool& valid) {
  webrtc::TargetTransferRate result{};
  result.at_time = NativeTimestamp(value.at_time);
  result.network_estimate = Unpack(value.network_estimate, valid);
  result.target_rate = NativeRate(value.target_rate, valid);
  result.cwnd_reduce_ratio = value.cwnd_reduce_ratio;
  return result;
}
FfiNcNetworkControlUpdate Pack(const webrtc::NetworkControlUpdate& value) {
  FfiNcNetworkControlUpdate result{};
  result.has_congestion_window = value.congestion_window.has_value();
  if (value.congestion_window) result.congestion_window = Pack(*value.congestion_window);
  result.has_pacer_config = value.pacer_config.has_value();
  if (value.pacer_config) result.pacer_config = Pack(*value.pacer_config);
  result.probe_cluster_configs.reserve(value.probe_cluster_configs.size());
  for (const auto& item : value.probe_cluster_configs) result.probe_cluster_configs.push_back(Pack(item));
  result.has_target_rate = value.target_rate.has_value();
  if (value.target_rate) result.target_rate = Pack(*value.target_rate);
  return result;
}
webrtc::NetworkControlUpdate Unpack(const FfiNcNetworkControlUpdate& value, bool& valid) {
  webrtc::NetworkControlUpdate result{};
  if (value.has_congestion_window) result.congestion_window = NativeSize(value.congestion_window, valid);
  if (value.has_pacer_config) result.pacer_config = Unpack(value.pacer_config, valid);
  result.probe_cluster_configs.reserve(value.probe_cluster_configs.size());
  for (const auto& item : value.probe_cluster_configs) result.probe_cluster_configs.push_back(Unpack(item, valid));
  if (value.has_target_rate) result.target_rate = Unpack(value.target_rate, valid);
  return result;
}
FfiNcProcessInterval Pack(const webrtc::ProcessInterval& value) {
  FfiNcProcessInterval result{};
  result.at_time = Pack(value.at_time);
  result.has_pacer_queue = value.pacer_queue.has_value();
  if (value.pacer_queue) result.pacer_queue = Pack(*value.pacer_queue);
  return result;
}
webrtc::ProcessInterval Unpack(const FfiNcProcessInterval& value, bool& valid) {
  webrtc::ProcessInterval result{};
  result.at_time = NativeTimestamp(value.at_time);
  if (value.has_pacer_queue) result.pacer_queue = NativeSize(value.pacer_queue, valid);
  return result;
}
FfiNcNetworkStateEstimate Pack(const webrtc::NetworkStateEstimate& value) {
  FfiNcNetworkStateEstimate result{};
  result.confidence = value.confidence;
  result.update_time = Pack(value.update_time);
  result.last_receive_time = Pack(value.last_receive_time);
  result.last_send_time = Pack(value.last_send_time);
  result.link_capacity = Pack(value.link_capacity);
  result.link_capacity_lower = Pack(value.link_capacity_lower);
  result.link_capacity_upper = Pack(value.link_capacity_upper);
  result.pre_link_buffer_delay = Pack(value.pre_link_buffer_delay);
  result.post_link_buffer_delay = Pack(value.post_link_buffer_delay);
  result.propagation_delay = Pack(value.propagation_delay);
  result.time_delta = Pack(value.time_delta);
  result.last_feed_time = Pack(value.last_feed_time);
  result.cross_delay_rate = value.cross_delay_rate;
  result.spike_delay_rate = value.spike_delay_rate;
  result.link_capacity_std_dev = Pack(value.link_capacity_std_dev);
  result.link_capacity_min = Pack(value.link_capacity_min);
  result.cross_traffic_ratio = value.cross_traffic_ratio;
  return result;
}
webrtc::NetworkStateEstimate Unpack(const FfiNcNetworkStateEstimate& value, bool& valid) {
  webrtc::NetworkStateEstimate result{};
  result.confidence = value.confidence;
  result.update_time = NativeTimestamp(value.update_time);
  result.last_receive_time = NativeTimestamp(value.last_receive_time);
  result.last_send_time = NativeTimestamp(value.last_send_time);
  result.link_capacity = NativeRate(value.link_capacity, valid);
  result.link_capacity_lower = NativeRate(value.link_capacity_lower, valid);
  result.link_capacity_upper = NativeRate(value.link_capacity_upper, valid);
  result.pre_link_buffer_delay = NativeDelta(value.pre_link_buffer_delay);
  result.post_link_buffer_delay = NativeDelta(value.post_link_buffer_delay);
  result.propagation_delay = NativeDelta(value.propagation_delay);
  result.time_delta = NativeDelta(value.time_delta);
  result.last_feed_time = NativeTimestamp(value.last_feed_time);
  result.cross_delay_rate = value.cross_delay_rate;
  result.spike_delay_rate = value.spike_delay_rate;
  result.link_capacity_std_dev = NativeRate(value.link_capacity_std_dev, valid);
  result.link_capacity_min = NativeRate(value.link_capacity_min, valid);
  result.cross_traffic_ratio = value.cross_traffic_ratio;
  return result;
}

webrtc::NetworkControlUpdate Reject(const webrtc::Environment& env) {
  webrtc::NetworkControlUpdate result;
  webrtc::TargetTransferRate target;
  target.at_time = env.clock().CurrentTime();
  target.network_estimate.at_time = target.at_time;
  target.network_estimate.bandwidth = webrtc::DataRate::Zero();
  target.network_estimate.round_trip_time = webrtc::TimeDelta::Zero();
  target.network_estimate.bwe_period = webrtc::TimeDelta::Zero();
  target.target_rate = webrtc::DataRate::Zero();
  result.target_rate = target;
  // Keep the engine's last valid pacing configuration for packets it already
  // owns. Setting zero pacing while old packets are enqueued violates the
  // native EnqueuePacket precondition; zero target requests no new allocation.
  return result;
}
class EstimatorAdapter final : public webrtc::NetworkStateEstimator {
 public:
  explicit EstimatorAdapter(rust::Box<RustNetworkStateEstimator> source)
      : source_(std::move(source)) {}
  std::optional<webrtc::NetworkStateEstimate> GetCurrentEstimate() override {
    auto reply = network_estimator_estimate(*source_);
    if (!reply.present) return std::nullopt;
    bool valid = true;
    auto estimate = Unpack(reply.value, valid);
    if (!valid) { network_estimator_invalid(*source_, 2); return std::nullopt; }
    return estimate;
  }
  void OnTransportPacketsFeedback(const webrtc::TransportPacketsFeedback& value) override {
    network_estimator_feedback(*source_, Pack(value));
  }
  void OnReceivedPacket(const webrtc::PacketResult& value) override {
    network_estimator_received(*source_, Pack(value));
  }
  void OnRouteChange(const webrtc::NetworkRouteChange& value) override {
    network_estimator_route(*source_, Pack(value));
  }
 private:
  rust::Box<RustNetworkStateEstimator> source_;
};
class EstimatorFactoryAdapter final : public webrtc::NetworkStateEstimatorFactory {
 public:
  explicit EstimatorFactoryAdapter(rust::Box<RustNetworkStateEstimatorFactory> source)
      : source_(std::move(source)) {}
  std::unique_ptr<webrtc::NetworkStateEstimator> Create(const webrtc::FieldTrialsView* trials) override {
    // A null native input is distinct from an owning copy of an empty view.
    auto copy = trials ? std::make_unique<NativeFieldTrials>(trials->CreateCopy()) : nullptr;
    auto product = network_estimator_create(*source_, std::move(copy));
    if (!network_estimator_present(*product)) return nullptr;
    return std::make_unique<EstimatorAdapter>(std::move(product));
  }
 private:
  rust::Box<RustNetworkStateEstimatorFactory> source_;
};
std::unique_ptr<webrtc::NetworkStateEstimatorFactory> EstimatorFactory(rust::Box<RustNetworkStateEstimatorFactory> source) {
  if (!network_estimator_factory_present(*source)) return nullptr;
  return std::make_unique<EstimatorFactoryAdapter>(std::move(source));
}

// Native SDK controllers can outlive the factory that created them (the stock
// SDK path itself creates a temporary factory). Retain the Rust execution-domain
// and child registration owners through the actual controller's destruction.
class RetainedSdkController final : public webrtc::NetworkControllerInterface {
 public:
  RetainedSdkController(rust::Box<RustNetworkControllerFactory> source,
                        std::unique_ptr<webrtc::NetworkControllerInterface> actual)
      : source_(std::move(source)), actual_(std::move(actual)) {}
  webrtc::NetworkControlUpdate OnNetworkAvailability(webrtc::NetworkAvailability value) override { return actual_->OnNetworkAvailability(std::move(value)); }
  webrtc::NetworkControlUpdate OnNetworkRouteChange(webrtc::NetworkRouteChange value) override { return actual_->OnNetworkRouteChange(std::move(value)); }
  webrtc::NetworkControlUpdate OnProcessInterval(webrtc::ProcessInterval value) override { return actual_->OnProcessInterval(std::move(value)); }
  webrtc::NetworkControlUpdate OnRemoteBitrateReport(webrtc::RemoteBitrateReport value) override { return actual_->OnRemoteBitrateReport(std::move(value)); }
  webrtc::NetworkControlUpdate OnRoundTripTimeUpdate(webrtc::RoundTripTimeUpdate value) override { return actual_->OnRoundTripTimeUpdate(std::move(value)); }
  webrtc::NetworkControlUpdate OnSentPacket(webrtc::SentPacket value) override { return actual_->OnSentPacket(std::move(value)); }
  webrtc::NetworkControlUpdate OnReceivedPacket(webrtc::ReceivedPacket value) override { return actual_->OnReceivedPacket(std::move(value)); }
  webrtc::NetworkControlUpdate OnStreamsConfig(webrtc::StreamsConfig value) override { return actual_->OnStreamsConfig(std::move(value)); }
  webrtc::NetworkControlUpdate OnTargetRateConstraints(webrtc::TargetRateConstraints value) override { return actual_->OnTargetRateConstraints(std::move(value)); }
  webrtc::NetworkControlUpdate OnTransportLossReport(webrtc::TransportLossReport value) override { return actual_->OnTransportLossReport(std::move(value)); }
  webrtc::NetworkControlUpdate OnTransportPacketsFeedback(webrtc::TransportPacketsFeedback value) override { return actual_->OnTransportPacketsFeedback(std::move(value)); }
  webrtc::NetworkControlUpdate OnNetworkStateEstimate(webrtc::NetworkStateEstimate value) override { return actual_->OnNetworkStateEstimate(std::move(value)); }
  bool SupportsEcnAdaptation() const override { return actual_->SupportsEcnAdaptation(); }
 private:
  rust::Box<RustNetworkControllerFactory> source_;
  std::unique_ptr<webrtc::NetworkControllerInterface> actual_;
};
class GoogCcFactoryAdapter final : public webrtc::NetworkControllerFactoryInterface {
 public:
  explicit GoogCcFactoryAdapter(rust::Box<RustNetworkControllerFactory> source)
      : source_(std::move(source)),
        predictor_(network_predictor_factory_adapter(network_controller_goog_cc_predictor(*source_))) {
    webrtc::GoogCcFactoryConfig config;
    config.rfc_8888_feedback_negotiated = network_controller_goog_cc_rfc8888(*source_);
    config.network_state_estimator_factory = EstimatorFactory(network_controller_goog_cc_estimator(*source_));
    config.network_state_predictor_factory = predictor_.get();
    actual_ = std::make_unique<webrtc::GoogCcNetworkControllerFactory>(std::move(config));
  }
  std::unique_ptr<webrtc::NetworkControllerInterface> Create(webrtc::NetworkControllerConfig config) override {
    auto retained = network_controller_factory_retain(*source_);
    auto controller = actual_->Create(std::move(config));
    return std::make_unique<RetainedSdkController>(std::move(retained), std::move(controller));
  }
  webrtc::TimeDelta GetProcessInterval() const override { return actual_->GetProcessInterval(); }
 private:
  rust::Box<RustNetworkControllerFactory> source_;
  // The SDK config borrows this factory, so destroy the actual factory first.
  std::unique_ptr<webrtc::NetworkStatePredictorFactoryInterface> predictor_;
  std::unique_ptr<webrtc::GoogCcNetworkControllerFactory> actual_;
};

class ControllerAdapter final : public webrtc::NetworkControllerInterface {
 public:
  ControllerAdapter(rust::Box<RustNetworkController> source, webrtc::Environment env)
      : source_(std::move(source)), env_(std::move(env)) {}
  webrtc::NetworkControlUpdate OnNetworkAvailability(webrtc::NetworkAvailability value) override {
    FfiNcMessage message{};
    message.network_availability = Pack(value);
    return Update(0, std::move(message));
  }
  webrtc::NetworkControlUpdate OnNetworkRouteChange(webrtc::NetworkRouteChange value) override {
    FfiNcMessage message{};
    message.network_route_change = Pack(value);
    return Update(1, std::move(message));
  }
  webrtc::NetworkControlUpdate OnProcessInterval(webrtc::ProcessInterval value) override {
    FfiNcMessage message{};
    message.process_interval = Pack(value);
    return Update(2, std::move(message));
  }
  webrtc::NetworkControlUpdate OnRemoteBitrateReport(webrtc::RemoteBitrateReport value) override {
    FfiNcMessage message{};
    message.remote_bitrate_report = Pack(value);
    return Update(3, std::move(message));
  }
  webrtc::NetworkControlUpdate OnRoundTripTimeUpdate(webrtc::RoundTripTimeUpdate value) override {
    FfiNcMessage message{};
    message.round_trip_time_update = Pack(value);
    return Update(4, std::move(message));
  }
  webrtc::NetworkControlUpdate OnSentPacket(webrtc::SentPacket value) override {
    FfiNcMessage message{};
    message.sent_packet = Pack(value);
    return Update(5, std::move(message));
  }
  webrtc::NetworkControlUpdate OnReceivedPacket(webrtc::ReceivedPacket value) override {
    FfiNcMessage message{};
    message.received_packet = Pack(value);
    return Update(6, std::move(message));
  }
  webrtc::NetworkControlUpdate OnStreamsConfig(webrtc::StreamsConfig value) override {
    FfiNcMessage message{};
    message.streams_config = Pack(value);
    return Update(7, std::move(message));
  }
  webrtc::NetworkControlUpdate OnTargetRateConstraints(webrtc::TargetRateConstraints value) override {
    FfiNcMessage message{};
    message.target_rate_constraints = Pack(value);
    return Update(8, std::move(message));
  }
  webrtc::NetworkControlUpdate OnTransportLossReport(webrtc::TransportLossReport value) override {
    FfiNcMessage message{};
    message.transport_loss_report = Pack(value);
    return Update(9, std::move(message));
  }
  webrtc::NetworkControlUpdate OnTransportPacketsFeedback(webrtc::TransportPacketsFeedback value) override {
    FfiNcMessage message{};
    message.transport_packets_feedback = Pack(value);
    return Update(10, std::move(message));
  }
  webrtc::NetworkControlUpdate OnNetworkStateEstimate(webrtc::NetworkStateEstimate value) override {
    FfiNcMessage message{};
    message.network_state_estimate = Pack(value);
    return Update(11, std::move(message));
  }
  bool SupportsEcnAdaptation() const override { return network_controller_ecn(*source_); }
 private:
  webrtc::NetworkControlUpdate Update(std::uint8_t kind, FfiNcMessage message) {
    auto reply = network_controller_update(*source_, kind, std::move(message));
    if (reply.failed) return Reject(env_);
    bool valid = true;
    auto update = Unpack(reply, valid);
    if (!valid) { network_controller_invalid(*source_, 2); return Reject(env_); }
    // Native pacing, probing and rate arithmetic is libwebrtc-owned.
    return update;
  }
  rust::Box<RustNetworkController> source_;
  webrtc::Environment env_;
};
class FactoryAdapter final : public webrtc::NetworkControllerFactoryInterface {
 public:
  explicit FactoryAdapter(rust::Box<RustNetworkControllerFactory> source) : source_(std::move(source)) {}
  std::unique_ptr<webrtc::NetworkControllerInterface> Create(webrtc::NetworkControllerConfig config) override {
    FfiNcConfig value{};
    value.constraints = Pack(config.constraints);
    value.stream_based_config = Pack(config.stream_based_config);
    value.default_pacing_time_window = Pack(config.default_pacing_time_window);
    auto product = network_controller_create(*source_, retain_environment_from_sdk(config.env), std::move(value));
    // RtpTransportControllerSend requires a non-null controller. A failed
    // factory gets an explicitly rejecting adapter, never a stock substitute.
    return std::make_unique<ControllerAdapter>(std::move(product), std::move(config.env));
  }
  webrtc::TimeDelta GetProcessInterval() const override { return NativeDelta(network_controller_interval(*source_)); }
 private:
  rust::Box<RustNetworkControllerFactory> source_;
};
}  // namespace
std::unique_ptr<webrtc::NetworkControllerFactoryInterface> network_controller_factory_adapter(rust::Box<RustNetworkControllerFactory> source) noexcept {
  if (!network_controller_factory_present(*source)) return nullptr;
  if (network_controller_is_goog_cc(*source))
    return std::make_unique<GoogCcFactoryAdapter>(std::move(source));
  return std::make_unique<FactoryAdapter>(std::move(source));
}
bool network_controller_injection_enabled(const NativeEnvironment& environment) noexcept {
  return environment.environment().field_trials().IsEnabled("WebRTC-Bwe-InjectedCongestionController");
}

rust::Vec<FfiNcNetworkControlUpdate> network_controller_native_probe(
    const NativeEnvironment& environment, rust::Box<RustNetworkControllerFactory> source,
    rust::Slice<const FfiNcMessage> messages, rust::Slice<const std::uint8_t> kinds) noexcept {
  rust::Vec<FfiNcNetworkControlUpdate> result;
  if (messages.size() != kinds.size()) return result;
  auto factory = network_controller_factory_adapter(std::move(source));
  if (!factory) return result;
  webrtc::NetworkControllerConfig config{environment.environment()};
  auto controller = factory->Create(std::move(config));
  (void)factory->GetProcessInterval();
  factory.reset();
  for (std::size_t index = 0; index < messages.size(); ++index) {
    bool valid = true;
    webrtc::NetworkControlUpdate update;
    const auto& value = messages[index];
    switch (kinds[index]) {
      case 0: { auto input = Unpack(value.network_availability, valid); if (valid) update = controller->OnNetworkAvailability(std::move(input)); break; }
      case 1: { auto input = Unpack(value.network_route_change, valid); if (valid) update = controller->OnNetworkRouteChange(std::move(input)); break; }
      case 2: { auto input = Unpack(value.process_interval, valid); if (valid) update = controller->OnProcessInterval(std::move(input)); break; }
      case 3: { auto input = Unpack(value.remote_bitrate_report, valid); if (valid) update = controller->OnRemoteBitrateReport(std::move(input)); break; }
      case 4: { auto input = Unpack(value.round_trip_time_update, valid); if (valid) update = controller->OnRoundTripTimeUpdate(std::move(input)); break; }
      case 5: { auto input = Unpack(value.sent_packet, valid); if (valid) update = controller->OnSentPacket(std::move(input)); break; }
      case 6: { auto input = Unpack(value.received_packet, valid); if (valid) update = controller->OnReceivedPacket(std::move(input)); break; }
      case 7: { auto input = Unpack(value.streams_config, valid); if (valid) update = controller->OnStreamsConfig(std::move(input)); break; }
      case 8: { auto input = Unpack(value.target_rate_constraints, valid); if (valid) update = controller->OnTargetRateConstraints(std::move(input)); break; }
      case 9: { auto input = Unpack(value.transport_loss_report, valid); if (valid) update = controller->OnTransportLossReport(std::move(input)); break; }
      case 10: { auto input = Unpack(value.transport_packets_feedback, valid); if (valid) update = controller->OnTransportPacketsFeedback(std::move(input)); break; }
      case 11: { auto input = Unpack(value.network_state_estimate, valid); if (valid) update = controller->OnNetworkStateEstimate(std::move(input)); break; }
      case 12: {
        update = Reject(environment.environment());
        update.target_rate->target_rate = webrtc::DataRate::BitsPerSec(controller->SupportsEcnAdaptation() ? 1 : 0);
        break;
      }
      default: valid = false; break;
    }
    auto packed = Pack(update);
    packed.failed = !valid;
    result.push_back(std::move(packed));
  }
  return result;
}
}  // namespace pulsebeam::webrtc_sys
