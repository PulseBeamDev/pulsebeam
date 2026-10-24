#include "pulsebeam-webrtc-sys/native/stats.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "api/stats/rtcstats_objects.h"
#include <string_view>
#include <type_traits>
#include <limits>
namespace pulsebeam::webrtc_sys {
namespace {
rust::Vec<std::uint8_t> Bytes(std::string_view v) { rust::Vec<std::uint8_t> out; out.reserve(v.size()); for (unsigned char b : v) out.push_back(b); return out; }
template<class T> std::string String(const T& v) { return std::string(reinterpret_cast<const char*>(v.data()), v.size()); }
webrtc::Timestamp Time(std::int64_t v) { if (v == INT64_MAX) return webrtc::Timestamp::PlusInfinity(); if (v == INT64_MIN) return webrtc::Timestamp::MinusInfinity(); return webrtc::Timestamp::Micros(v); }
std::int64_t Time(webrtc::Timestamp v) { if (v.IsPlusInfinity()) return INT64_MAX; if (v.IsMinusInfinity()) return INT64_MIN; return v.us(); }
template<class T> constexpr std::uint8_t TypeCode() {
  if constexpr (std::is_same_v<T, bool>) return 1;
  if constexpr (std::is_same_v<T, int32_t>) return 2;
  if constexpr (std::is_same_v<T, uint32_t>) return 3;
  if constexpr (std::is_same_v<T, int64_t>) return 4;
  if constexpr (std::is_same_v<T, uint64_t>) return 5;
  if constexpr (std::is_same_v<T, double>) return 6;
  if constexpr (std::is_same_v<T, std::string>) return 7;
  if constexpr (std::is_same_v<T, std::vector<bool>>) return 8;
  if constexpr (std::is_same_v<T, std::vector<int32_t>>) return 9;
  if constexpr (std::is_same_v<T, std::vector<uint32_t>>) return 10;
  if constexpr (std::is_same_v<T, std::vector<int64_t>>) return 11;
  if constexpr (std::is_same_v<T, std::vector<uint64_t>>) return 12;
  if constexpr (std::is_same_v<T, std::vector<double>>) return 13;
  if constexpr (std::is_same_v<T, std::vector<std::string>>) return 14;
  if constexpr (std::is_same_v<T, std::map<std::string,uint64_t>>) return 15;
  if constexpr (std::is_same_v<T, std::map<std::string,double>>) return 16;
  return 0;
}
template<class T> FfiRtcStatsValue Pack(const std::optional<T>& value) {
  FfiRtcStatsValue out{}; out.kind = TypeCode<T>(); out.has_value = value.has_value(); if (!value) return out;
  if constexpr (std::is_same_v<T, bool>) { out.boolean = *value; }
  else if constexpr (std::is_same_v<T, int32_t>) { out.int32_value = *value; }
  else if constexpr (std::is_same_v<T, uint32_t>) { out.uint32_value = *value; }
  else if constexpr (std::is_same_v<T, int64_t>) { out.int64_value = *value; }
  else if constexpr (std::is_same_v<T, uint64_t>) { out.uint64_value = *value; }
  else if constexpr (std::is_same_v<T, double>) { out.decimal = *value; }
  else if constexpr (std::is_same_v<T, std::string>) { out.text = Bytes(*value); }
  else if constexpr (std::is_same_v<T, std::vector<bool>>) { for (auto v : *value) out.booleans.push_back(v); }
  else if constexpr (std::is_same_v<T, std::vector<int32_t>>) { for (auto v : *value) out.int32s.push_back(v); }
  else if constexpr (std::is_same_v<T, std::vector<uint32_t>>) { for (auto v : *value) out.uint32s.push_back(v); }
  else if constexpr (std::is_same_v<T, std::vector<int64_t>>) { for (auto v : *value) out.int64s.push_back(v); }
  else if constexpr (std::is_same_v<T, std::vector<uint64_t>>) { for (auto v : *value) out.uint64s.push_back(v); }
  else if constexpr (std::is_same_v<T, std::vector<double>>) { for (auto v : *value) out.decimals.push_back(v); }
  else if constexpr (std::is_same_v<T, std::vector<std::string>>) { for (const auto& v : *value) { FfiRtcStatsBytes b; b.value = Bytes(v); out.texts.push_back(std::move(b)); } }
  else if constexpr (std::is_same_v<T, std::map<std::string,uint64_t>>) { for (const auto& [key, v] : *value) { FfiRtcStatsUint64Entry item; item.key = Bytes(key); item.value = v; out.uint64_map.push_back(std::move(item)); } }
  else if constexpr (std::is_same_v<T, std::map<std::string,double>>) { for (const auto& [key, v] : *value) { FfiRtcStatsDoubleEntry item; item.key = Bytes(key); item.value = v; out.decimal_map.push_back(std::move(item)); } }
  return out;
}
template<class T> std::optional<T> Unpack(const FfiRtcStatsValue& value) {
  if (!value.has_value) return std::nullopt;
  if constexpr (std::is_same_v<T, bool>) { return value.boolean; }
  else if constexpr (std::is_same_v<T, int32_t>) { return value.int32_value; }
  else if constexpr (std::is_same_v<T, uint32_t>) { return value.uint32_value; }
  else if constexpr (std::is_same_v<T, int64_t>) { return value.int64_value; }
  else if constexpr (std::is_same_v<T, uint64_t>) { return value.uint64_value; }
  else if constexpr (std::is_same_v<T, double>) { return value.decimal; }
  else if constexpr (std::is_same_v<T, std::string>) { return String(value.text); }
  else if constexpr (std::is_same_v<T, std::vector<bool>>) { T out; for (auto v : value.booleans) out.push_back(v); return out; }
  else if constexpr (std::is_same_v<T, std::vector<int32_t>>) { T out; for (auto v : value.int32s) out.push_back(v); return out; }
  else if constexpr (std::is_same_v<T, std::vector<uint32_t>>) { T out; for (auto v : value.uint32s) out.push_back(v); return out; }
  else if constexpr (std::is_same_v<T, std::vector<int64_t>>) { T out; for (auto v : value.int64s) out.push_back(v); return out; }
  else if constexpr (std::is_same_v<T, std::vector<uint64_t>>) { T out; for (auto v : value.uint64s) out.push_back(v); return out; }
  else if constexpr (std::is_same_v<T, std::vector<double>>) { T out; for (auto v : value.decimals) out.push_back(v); return out; }
  else if constexpr (std::is_same_v<T, std::vector<std::string>>) { T out; for (const auto& v : value.texts) out.push_back(String(v.value)); return out; }
  else if constexpr (std::is_same_v<T, std::map<std::string,uint64_t>>) { T out; for (const auto& v : value.uint64_map) out.emplace(String(v.key), v.value); return out; }
  else if constexpr (std::is_same_v<T, std::map<std::string,double>>) { T out; for (const auto& v : value.decimal_map) out.emplace(String(v.key), v.value); return out; }
}
#include "pulsebeam-webrtc-sys/native/stats_objects.h"
FfiRtcStatsAttribute Attribute(const webrtc::Attribute& value) {
  FfiRtcStatsAttribute out; out.name = Bytes(value.name());
  out.value = std::visit([](const auto* value) { return Pack(*value); }, value.as_variant()); return out;
}
std::unique_ptr<NativeRtcStats> Wrap(std::unique_ptr<webrtc::RTCStats> value) { return value ? std::make_unique<NativeRtcStats>(std::move(value)) : nullptr; }
}
std::int32_t legacy_stats_id_type(const NativeLegacyStatsId& v) noexcept { return static_cast<std::int32_t>(v.value->type()); }
rust::Vec<std::uint8_t> legacy_stats_id_string(const NativeLegacyStatsId& v) noexcept { return Bytes(v.value->ToString()); }
std::unique_ptr<NativeLegacyStatsId> legacy_stats_id_copy(const NativeLegacyStatsId& v) noexcept { return std::make_unique<NativeLegacyStatsId>(v.value); }
bool legacy_stats_id_same(const NativeLegacyStatsId& a, const NativeLegacyStatsId& b) noexcept { return a.value.get() == b.value.get(); }
bool legacy_stats_id_equal(const NativeLegacyStatsId& a, const NativeLegacyStatsId& b) noexcept { return a.value->Equals(b.value); }
rust::Vec<FfiLegacyStatsReport> copy_legacy_stats(const webrtc::StatsReports& reports) noexcept {
  rust::Vec<FfiLegacyStatsReport> out;
  for (const auto* report : reports) {
    FfiLegacyStatsReport r;
    r.id = std::make_unique<NativeLegacyStatsId>(report->id());
    r.timestamp_ms = report->timestamp(); r.native_type = Bytes(report->TypeToString());
    for (const auto& [name, value] : report->values()) {
      FfiLegacyStatsValue v{}; v.name = static_cast<std::int32_t>(name);
      v.display_name = Bytes(value->display_name()); v.native_string = Bytes(value->ToString());
      v.kind = static_cast<std::uint8_t>(value->type());
      using V = webrtc::StatsReport::Value;
      switch (value->type()) {
        case V::kInt: v.integer = value->int_val(); break;
        case V::kInt64: v.integer = value->int64_val(); break;
        case V::kFloat: v.decimal = value->float_val(); break;
        case V::kString: v.text = Bytes(value->string_val()); break;
        case V::kStaticString: v.text = Bytes(value->static_string_val()); break;
        case V::kBool: v.flag = value->bool_val(); break;
        case V::kId: v.id = std::make_unique<NativeLegacyStatsId>(value->id_val()); break;
      }
      r.values.push_back(std::move(v));
    }
    out.push_back(std::move(r));
  }
  return out;
}
std::unique_ptr<NativeRtcStats> rtc_stats_new(std::uint8_t kind, rust::Slice<const std::uint8_t> id, std::int64_t timestamp) noexcept { return Wrap(NewStats(kind, String(id), Time(timestamp))); }
std::unique_ptr<NativeRtcStats> rtc_stats_copy(const NativeRtcStats& value) noexcept { return Wrap(value.value().copy()); }
std::unique_ptr<NativeRtcStats> rtc_stats_copy_as(const NativeRtcStats& value, std::uint8_t kind) noexcept { return Wrap(CopyAs(value.value(), kind)); }
std::uint8_t rtc_stats_class(const NativeRtcStats& value) noexcept { return StatsClass(value.value()); }
rust::Vec<std::uint8_t> rtc_stats_id(const NativeRtcStats& value) noexcept { return Bytes(value.value().id()); }
rust::Vec<std::uint8_t> rtc_stats_type(const NativeRtcStats& value) noexcept { return Bytes(value.value().type()); }
std::int64_t rtc_stats_timestamp(const NativeRtcStats& value) noexcept { return Time(value.value().timestamp()); }
void rtc_stats_set_timestamp(NativeRtcStats& value, std::int64_t timestamp) noexcept { value.mutate().set_timestamp(Time(timestamp)); }
FfiRtcStatsData rtc_stats_data(const NativeRtcStats& value) noexcept { return StatsData(value.value()); }
bool rtc_stats_set_data(NativeRtcStats& value, const FfiRtcStatsData& data) noexcept { return ApplyStatsData(value.mutate(), data); }
rust::Vec<FfiRtcStatsAttribute> rtc_stats_attributes(const NativeRtcStats& value) noexcept { rust::Vec<FfiRtcStatsAttribute> out; for (const auto& attr : value.value().Attributes()) out.push_back(Attribute(attr)); return out; }
bool rtc_stats_equal(const NativeRtcStats& a, const NativeRtcStats& b) noexcept { return a.value() == b.value(); }
rust::Vec<std::uint8_t> rtc_stats_json(const NativeRtcStats& value) noexcept { return Bytes(value.value().ToJson()); }
std::unique_ptr<NativeRtcStatsAttribute> rtc_stats_attribute_new(rust::Slice<const std::uint8_t> name, const FfiRtcStatsValue& value) noexcept {
  switch (value.kind) {
    case 1: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<bool>(value)));
    case 2: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<int32_t>(value)));
    case 3: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<uint32_t>(value)));
    case 4: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<int64_t>(value)));
    case 5: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<uint64_t>(value)));
    case 6: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<double>(value)));
    case 7: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::string>(value)));
    case 8: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::vector<bool>>(value)));
    case 9: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::vector<int32_t>>(value)));
    case 10: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::vector<uint32_t>>(value)));
    case 11: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::vector<int64_t>>(value)));
    case 12: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::vector<uint64_t>>(value)));
    case 13: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::vector<double>>(value)));
    case 14: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::vector<std::string>>(value)));
    case 15: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::map<std::string,uint64_t>>(value)));
    case 16: return std::make_unique<NativeRtcStatsAttribute>(String(name), StatsPayload(Unpack<std::map<std::string,double>>(value)));
    default: return nullptr;
  }
}
std::unique_ptr<NativeRtcStatsAttribute> rtc_stats_attribute_copy(const NativeRtcStatsAttribute& value) noexcept { return std::make_unique<NativeRtcStatsAttribute>(value); }
FfiRtcStatsAttribute rtc_stats_attribute_data(const NativeRtcStatsAttribute& value) noexcept { return Attribute(value.view()); }
bool rtc_stats_attribute_has_value(const NativeRtcStatsAttribute& value) noexcept { return value.view().has_value(); }
bool rtc_stats_attribute_is_sequence(const NativeRtcStatsAttribute& value) noexcept { return value.view().is_sequence(); }
bool rtc_stats_attribute_is_string(const NativeRtcStatsAttribute& value) noexcept { return value.view().is_string(); }
rust::Vec<std::uint8_t> rtc_stats_attribute_string(const NativeRtcStatsAttribute& value) noexcept { return Bytes(value.view().ToString()); }
bool rtc_stats_attribute_equal(const NativeRtcStatsAttribute& a, const NativeRtcStatsAttribute& b) noexcept { return a.view() == b.view(); }
std::unique_ptr<NativeRtcStatsReport> rtc_stats_report_clone(const NativeRtcStatsReport& value) noexcept { return std::make_unique<NativeRtcStatsReport>(value.value); }
bool rtc_stats_report_same(const NativeRtcStatsReport& a, const NativeRtcStatsReport& b) noexcept { return a.value.get() == b.value.get(); }
std::int64_t rtc_stats_report_timestamp(const NativeRtcStatsReport& value) noexcept { return Time(value.value->timestamp()); }
std::size_t rtc_stats_report_size(const NativeRtcStatsReport& value) noexcept { return value.value->size(); }
rust::Vec<FfiRtcStatsBytes> rtc_stats_report_keys(const NativeRtcStatsReport& value) noexcept { rust::Vec<FfiRtcStatsBytes> out; for (const auto& stat : *value.value) { FfiRtcStatsBytes id; id.value = Bytes(stat.id()); out.push_back(std::move(id)); } return out; }
std::unique_ptr<NativeRtcStats> rtc_stats_report_get(const NativeRtcStatsReport& value, rust::Slice<const std::uint8_t> id, std::uint8_t kind) noexcept {
  const webrtc::RTCStats* stat = nullptr; const auto key = String(id);
  switch (kind) {
    case 0: stat = value.value->Get(key); break;
    case 1: stat = value.value->GetAs<webrtc::RTCCertificateStats>(key); break;
    case 2: stat = value.value->GetAs<webrtc::RTCCodecStats>(key); break;
    case 3: stat = value.value->GetAs<webrtc::RTCDataChannelStats>(key); break;
    case 4: stat = value.value->GetAs<webrtc::RTCIceCandidatePairStats>(key); break;
    case 5: stat = value.value->GetAs<webrtc::RTCIceCandidateStats>(key); break;
    case 6: stat = value.value->GetAs<webrtc::RTCLocalIceCandidateStats>(key); break;
    case 7: stat = value.value->GetAs<webrtc::RTCRemoteIceCandidateStats>(key); break;
    case 8: stat = value.value->GetAs<webrtc::RTCPeerConnectionStats>(key); break;
    case 9: stat = value.value->GetAs<webrtc::RTCRtpStreamStats>(key); break;
    case 10: stat = value.value->GetAs<webrtc::RTCReceivedRtpStreamStats>(key); break;
    case 11: stat = value.value->GetAs<webrtc::RTCSentRtpStreamStats>(key); break;
    case 12: stat = value.value->GetAs<webrtc::RTCInboundRtpStreamStats>(key); break;
    case 13: stat = value.value->GetAs<webrtc::RTCOutboundRtpStreamStats>(key); break;
    case 14: stat = value.value->GetAs<webrtc::RTCRemoteInboundRtpStreamStats>(key); break;
    case 15: stat = value.value->GetAs<webrtc::RTCRemoteOutboundRtpStreamStats>(key); break;
    case 16: stat = value.value->GetAs<webrtc::RTCMediaSourceStats>(key); break;
    case 17: stat = value.value->GetAs<webrtc::RTCAudioSourceStats>(key); break;
    case 18: stat = value.value->GetAs<webrtc::RTCVideoSourceStats>(key); break;
    case 19: stat = value.value->GetAs<webrtc::RTCTransportStats>(key); break;
    case 20: stat = value.value->GetAs<webrtc::RTCAudioPlayoutStats>(key); break;
  }
  return stat ? Wrap(stat->copy()) : nullptr;
}
rust::Vec<FfiRtcStatsEntry> rtc_stats_report_of_class(const NativeRtcStatsReport& value, std::uint8_t kind) noexcept {
  rust::Vec<FfiRtcStatsEntry> out;
  switch (kind) {
    case 1: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCCertificateStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 2: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCCodecStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 3: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCDataChannelStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 4: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCIceCandidatePairStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 5: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCIceCandidateStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 6: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCLocalIceCandidateStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 7: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCRemoteIceCandidateStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 8: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCPeerConnectionStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 9: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCRtpStreamStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 10: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCReceivedRtpStreamStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 11: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCSentRtpStreamStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 12: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCInboundRtpStreamStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 13: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCOutboundRtpStreamStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 14: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCRemoteInboundRtpStreamStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 15: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCRemoteOutboundRtpStreamStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 16: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCMediaSourceStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 17: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCAudioSourceStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 18: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCVideoSourceStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 19: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCTransportStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
    case 20: for (const auto* stat : value.value->GetStatsOfType<webrtc::RTCAudioPlayoutStats>()) { FfiRtcStatsEntry e; e.value = Wrap(stat->copy()); out.push_back(std::move(e)); } break;
  }
  return out;
}
bool rtc_stats_report_serializable(const NativeRtcStatsReport& value) noexcept { for (const auto& stat : *value.value) if (!stat.timestamp().IsFinite()) return false; return true; }
rust::Vec<std::uint8_t> rtc_stats_report_json(const NativeRtcStatsReport& value) noexcept { return Bytes(value.value->ToJson()); }
std::unique_ptr<NativeRtcStatsReportBuilder> rtc_stats_report_copy(const NativeRtcStatsReport& value) noexcept { return std::make_unique<NativeRtcStatsReportBuilder>(value.value->Copy()); }
std::unique_ptr<NativeRtcStatsReportBuilder> rtc_stats_report_new(std::int64_t time) noexcept { return std::make_unique<NativeRtcStatsReportBuilder>(webrtc::RTCStatsReport::Create(Time(time))); }
std::size_t rtc_stats_builder_size(const NativeRtcStatsReportBuilder& value) noexcept { return value.value->size(); }
bool rtc_stats_builder_add(NativeRtcStatsReportBuilder& value, std::unique_ptr<NativeRtcStats> stat, bool try_add) noexcept {
  if (try_add) { auto immutable = stat->take(); return value.value->TryAddStats(std::move(immutable)) != nullptr; }
  if (value.value->Get(stat->value().id())) return false;
  value.value->AddStats(stat->take()); return true;
}
std::unique_ptr<NativeRtcStats> rtc_stats_builder_take(NativeRtcStatsReportBuilder& value, rust::Slice<const std::uint8_t> id) noexcept { auto stat = value.value->Take(String(id)); return stat ? std::make_unique<NativeRtcStats>(std::move(stat)) : nullptr; }
bool rtc_stats_builder_merge(NativeRtcStatsReportBuilder& value, NativeRtcStatsReportBuilder& other) noexcept {
  for (const auto& stat : *other.value) if (value.value->Get(stat.id())) return false;
  value.value->TakeMembersFrom(other.value); return true;
}
std::unique_ptr<NativeRtcStatsReport> rtc_stats_report_freeze(std::unique_ptr<NativeRtcStatsReportBuilder> value) noexcept { return std::make_unique<NativeRtcStatsReport>(std::move(value->value)); }
} // namespace pulsebeam::webrtc_sys
