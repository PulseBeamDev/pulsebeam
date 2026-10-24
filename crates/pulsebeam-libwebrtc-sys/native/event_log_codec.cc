#include "pulsebeam-webrtc-sys/native/event_log_codec.h"
#include "pulsebeam-webrtc-sys/native/event_log.h"  // IWYU pragma: keep
#include <limits>
#include <map>
#include <span>
#include <vector>
#include "logging/rtc_event_log/events/rtc_event_begin_log.h"
#include "logging/rtc_event_log/events/rtc_event_end_log.h"
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
#include "logging/rtc_event_log/events/rtc_event_field_encoding_parser.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"

namespace pulsebeam::webrtc_sys {
namespace {
#define EVENT_CODECS(V) \
  V(RtcEventAlrState) \
  V(RtcEventRouteChange) \
  V(RtcEventRemoteEstimate) \
  V(RtcEventAudioNetworkAdaptation) \
  V(RtcEventAudioPlayout) \
  V(RtcEventAudioReceiveStreamConfig) \
  V(RtcEventAudioSendStreamConfig) \
  V(RtcEventBweUpdateDelayBased) \
  V(RtcEventBweUpdateLossBased) \
  V(RtcEventBweUpdateScream) \
  V(RtcEventDtlsTransportState) \
  V(RtcEventDtlsWritableState) \
  V(RtcEventIceCandidatePairConfig) \
  V(RtcEventIceCandidatePair) \
  V(RtcEventProbeClusterCreated) \
  V(RtcEventProbeResultFailure) \
  V(RtcEventProbeResultSuccess) \
  V(RtcEventRtcpPacketIncoming) \
  V(RtcEventRtcpPacketOutgoing) \
  V(RtcEventRtpPacketIncoming) \
  V(RtcEventRtpPacketOutgoing) \
  V(RtcEventVideoReceiveStreamConfig) \
  V(RtcEventVideoSendStreamConfig) \
  V(RtcEventFrameDecoded) \
  V(RtcEventBeginLog) \
  V(RtcEventEndLog)

using Type = webrtc::RtcEvent::Type;
bool HasCodec(Type kind) {
  switch (kind) {
#define HAS_CODEC(E) case webrtc::E::kType: return true;
    EVENT_CODECS(HAS_CODEC)
#undef HAS_CODEC
    default: return false;
  }
}
bool Implemented(Type kind) {
  return kind == webrtc::RtcEventAlrState::kType || kind == webrtc::RtcEventAudioPlayout::kType ||
      kind == webrtc::RtcEventBweUpdateDelayBased::kType || kind == webrtc::RtcEventBeginLog::kType ||
      kind == webrtc::RtcEventEndLog::kType;
}
rust::Vec<std::uint8_t> Bytes(const std::string& bytes) {
  rust::Vec<std::uint8_t> result;
  result.reserve(bytes.size());
  for (unsigned char byte : bytes) result.push_back(byte);
  return result;
}
FfiEventParseStatus Status(::RtcEventLogParseStatus status) {
  return {status.ok(), rust::String(status.message())};
}
std::int64_t TimeBits(webrtc::Timestamp time) {
  if (time.IsPlusInfinity()) return std::numeric_limits<std::int64_t>::max();
  if (time.IsMinusInfinity()) return std::numeric_limits<std::int64_t>::min();
  return time.us();
}
// The SDK converts decoded milliseconds to microseconds with unchecked signed
// multiplication. Use its own field parser, preserving every safe native
// failure/partial append; reject only values that would overflow that conversion.
bool SafeTimes(Type kind, absl::string_view bytes, bool batched) {
  if (!Implemented(kind)) return true;
  webrtc::EventParser parser;
  if (!parser.Initialize(bytes, batched).ok()) return true;
  constexpr webrtc::FieldParameters timestamp{
      .name = "timestamp_ms", .field_id = webrtc::FieldParameters::kTimestampField,
      .field_type = webrtc::FieldType::kVarInt, .value_width = 64};
  auto safe = [](std::span<std::uint64_t> values) {
    for (auto value : values) {
      auto millis = webrtc::DecodeFromUnsignedToType<std::int64_t>(value);
      if (millis < std::numeric_limits<std::int64_t>::min() / 1000 ||
          millis > std::numeric_limits<std::int64_t>::max() / 1000) return false;
    }
    return true;
  };
  auto times = parser.ParseNumericField(timestamp);
  if (!times.ok()) return true;  // Preserve native failure and partial output.
  if (!safe(times.value())) return false;
  if (kind == webrtc::RtcEventBeginLog::kType) {
    constexpr webrtc::FieldParameters utc{.name = "utc_start_time_ms", .field_id = 1,
        .field_type = webrtc::FieldType::kVarInt, .value_width = 64};
    auto utc_times = parser.ParseNumericField(utc);
    return !utc_times.ok() || safe(utc_times.value());
  }
  return true;
}
template<class Output>
::RtcEventLogParseStatus Stub(
    ::RtcEventLogParseStatus (*parse)(absl::string_view, bool, Output&),
    absl::string_view bytes, bool batched) {
  Output output;
  return parse(bytes, batched, output);
}
}  // namespace
struct NativeEventEncoding::State {
  Type kind;
  std::vector<const webrtc::RtcEvent*> events;
};
NativeEventEncoding::NativeEventEncoding(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeEventEncoding::~NativeEventEncoding() = default;
const std::unique_ptr<NativeEventEncoding::State>& NativeEventEncoding::state() const noexcept { return state_; }
std::unique_ptr<NativeEventEncoding> event_codec_encoder(std::uint32_t kind) noexcept {
  auto type = static_cast<Type>(kind);
  return HasCodec(type) ? std::make_unique<NativeEventEncoding>(std::make_unique<NativeEventEncoding::State>(type)) : nullptr;
}
bool event_codec_add(NativeEventEncoding& batch, const NativeRtcEvent& event) noexcept {
  const auto kind = batch.state()->kind;
  // Only encoders extracting concrete fields impose this fatal SDK check.
  // EndLog reads common timestamps; current stub codecs inspect no events.
  if (Implemented(kind) && kind != webrtc::RtcEventEndLog::kType &&
      event.event().GetType() != kind) return false;
  batch.state()->events.push_back(&event.event());
  return true;
}
rust::Vec<std::uint8_t> event_codec_encode(const NativeEventEncoding& batch) noexcept {
  const std::span<const webrtc::RtcEvent*> events(batch.state()->events);
  switch (batch.state()->kind) {
#define ENCODE(E) case webrtc::E::kType: return Bytes(webrtc::E::Encode(events));
    EVENT_CODECS(ENCODE)
#undef ENCODE
    default: return {};
  }
}
FfiEventParseResult event_codec_parse(std::uint32_t raw_kind,
    rust::Slice<const std::uint8_t> input, bool batched) noexcept {
  FfiEventParseResult result{};
  const auto kind = static_cast<Type>(raw_kind);
  if (!HasCodec(kind)) { result.guard = 2; return result; }
  const absl::string_view bytes(reinterpret_cast<const char*>(input.data()), input.size());
  if (!SafeTimes(kind, bytes, batched)) { result.guard = 1; return result; }
  auto row = [](webrtc::Timestamp time) { FfiLoggedEvent value{}; value.timestamp = TimeBits(time); return value; };
  if (kind == webrtc::RtcEventAlrState::kType) {
    std::vector<webrtc::LoggedAlrStateEvent> output;
    result.status = Status(webrtc::RtcEventAlrState::Parse(bytes, batched, output));
    for (const auto& value : output) { auto r = row(value.timestamp); r.in_alr = value.in_alr; result.rows.push_back(r); }
  } else if (kind == webrtc::RtcEventAudioPlayout::kType) {
    std::map<std::uint32_t, std::vector<webrtc::LoggedAudioPlayoutEvent>> output;
    result.status = Status(webrtc::RtcEventAudioPlayout::Parse(bytes, batched, output));
    for (const auto& [ssrc, events] : output) for (const auto& value : events) {
      auto r = row(value.timestamp); r.ssrc = ssrc; result.rows.push_back(r);
    }
  } else if (kind == webrtc::RtcEventBweUpdateDelayBased::kType) {
    std::vector<webrtc::LoggedBweDelayBasedUpdate> output;
    result.status = Status(webrtc::RtcEventBweUpdateDelayBased::Parse(bytes, batched, output));
    for (const auto& value : output) {
      auto r = row(value.timestamp); r.bitrate_bps = value.bitrate_bps;
      r.detector_state = static_cast<std::int32_t>(value.detector_state); result.rows.push_back(r);
    }
  } else if (kind == webrtc::RtcEventBeginLog::kType) {
    std::vector<webrtc::LoggedStartEvent> output;
    result.status = Status(webrtc::RtcEventBeginLog::Parse(bytes, batched, output));
    for (const auto& value : output) { auto r = row(value.timestamp); r.utc_time = TimeBits(value.utc_start_time); result.rows.push_back(r); }
  } else if (kind == webrtc::RtcEventEndLog::kType) {
    std::vector<webrtc::LoggedStopEvent> output;
    result.status = Status(webrtc::RtcEventEndLog::Parse(bytes, batched, output));
    for (const auto& value : output) result.rows.push_back(row(value.timestamp));
  } else {
    // Invoke even stub methods: native empty encodings and Not Implemented
    // statuses are real SDK outcomes, not binding-level Unsupported results.
    switch (kind) {
#define PARSE(E) case webrtc::E::kType: result.status = Status(Stub(webrtc::E::Parse, bytes, batched)); break;
      EVENT_CODECS(PARSE)
#undef PARSE
      default: break;
    }
  }
  return result;
}
FfiEventParseStatus event_codec_status(bool success, rust::Str error, rust::Str file, std::int32_t line) noexcept {
  return Status(success ? ::RtcEventLogParseStatus::Success() :
      ::RtcEventLogParseStatus::Error(
          absl::string_view(error.data(), error.size()), absl::string_view(file.data(), file.size()), line));
}
#undef EVENT_CODECS
}  // namespace pulsebeam::webrtc_sys
