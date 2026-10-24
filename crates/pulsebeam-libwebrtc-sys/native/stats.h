#pragma once
#include <memory>
#include <optional>
#include <variant>
#include <vector>
#include <map>
#include <string>
#include "rust/cxx.h"
#include "api/stats/rtc_stats_report.h"
#include "api/legacy_stats_types.h"
#include "api/stats/attribute.h"
namespace pulsebeam::webrtc_sys {
struct FfiLegacyStatsReport;
class NativeLegacyStatsId final {
 public:
  explicit NativeLegacyStatsId(webrtc::StatsReport::Id id) : value(std::move(id)) {}
  webrtc::StatsReport::Id value;
};
std::int32_t legacy_stats_id_type(const NativeLegacyStatsId&) noexcept;
rust::Vec<std::uint8_t> legacy_stats_id_string(const NativeLegacyStatsId&) noexcept;
std::unique_ptr<NativeLegacyStatsId> legacy_stats_id_copy(const NativeLegacyStatsId&) noexcept;
bool legacy_stats_id_same(const NativeLegacyStatsId&, const NativeLegacyStatsId&) noexcept;
bool legacy_stats_id_equal(const NativeLegacyStatsId&, const NativeLegacyStatsId&) noexcept;
rust::Vec<FfiLegacyStatsReport> copy_legacy_stats(const webrtc::StatsReports&) noexcept;
struct FfiRtcStatsValue;
struct FfiRtcStatsAttribute;
struct FfiRtcStatsData;
struct FfiRtcStatsEntry;
struct FfiRtcStatsBytes;
using StatsPayload = std::variant<std::optional<bool>, std::optional<int32_t>, std::optional<uint32_t>, std::optional<int64_t>, std::optional<uint64_t>, std::optional<double>, std::optional<std::string>, std::optional<std::vector<bool>>, std::optional<std::vector<int32_t>>, std::optional<std::vector<uint32_t>>, std::optional<std::vector<int64_t>>, std::optional<std::vector<uint64_t>>, std::optional<std::vector<double>>, std::optional<std::vector<std::string>>, std::optional<std::map<std::string,uint64_t>>, std::optional<std::map<std::string,double>>>;
class NativeRtcStats final {
 public:
  explicit NativeRtcStats(std::unique_ptr<webrtc::RTCStats> v) : mutable_(std::move(v)) {}
  explicit NativeRtcStats(std::unique_ptr<const webrtc::RTCStats> v) : immutable_(std::move(v)) {}
  const webrtc::RTCStats& value() const { return mutable_ ? *mutable_ : *immutable_; }
  webrtc::RTCStats& mutate() { if (!mutable_) { mutable_ = immutable_->copy(); immutable_.reset(); } return *mutable_; }
  std::unique_ptr<const webrtc::RTCStats> take() { if (mutable_) return std::move(mutable_); return std::move(immutable_); }
 private:
  std::unique_ptr<webrtc::RTCStats> mutable_;
  std::unique_ptr<const webrtc::RTCStats> immutable_;
};
class NativeRtcStatsAttribute final {
 public:
  NativeRtcStatsAttribute(std::string name, StatsPayload data) : name_(std::move(name)), data_(std::move(data)) {}
  webrtc::Attribute view() const { return std::visit([&](const auto& v) { return webrtc::Attribute(name_.c_str(), &v); }, data_); }
 private:
  std::string name_;
  StatsPayload data_;
};
class NativeRtcStatsReport final {
 public:
  explicit NativeRtcStatsReport(webrtc::scoped_refptr<const webrtc::RTCStatsReport> v) : value(std::move(v)) {}
  webrtc::scoped_refptr<const webrtc::RTCStatsReport> value;
};
class NativeRtcStatsReportBuilder final {
 public:
  explicit NativeRtcStatsReportBuilder(webrtc::scoped_refptr<webrtc::RTCStatsReport> v) : value(std::move(v)) {}
  webrtc::scoped_refptr<webrtc::RTCStatsReport> value;
};
std::unique_ptr<NativeRtcStats> rtc_stats_new(std::uint8_t class_kind, rust::Slice<const std::uint8_t> id, std::int64_t timestamp) noexcept;
std::unique_ptr<NativeRtcStats> rtc_stats_copy(const NativeRtcStats& value) noexcept;
std::unique_ptr<NativeRtcStats> rtc_stats_copy_as(const NativeRtcStats& value, std::uint8_t class_kind) noexcept;
std::uint8_t rtc_stats_class(const NativeRtcStats& value) noexcept;
rust::Vec<std::uint8_t> rtc_stats_id(const NativeRtcStats& value) noexcept;
rust::Vec<std::uint8_t> rtc_stats_type(const NativeRtcStats& value) noexcept;
std::int64_t rtc_stats_timestamp(const NativeRtcStats& value) noexcept;
void rtc_stats_set_timestamp(NativeRtcStats& value, std::int64_t timestamp) noexcept;
FfiRtcStatsData rtc_stats_data(const NativeRtcStats& value) noexcept;
bool rtc_stats_set_data(NativeRtcStats& value, const FfiRtcStatsData& data) noexcept;
rust::Vec<FfiRtcStatsAttribute> rtc_stats_attributes(const NativeRtcStats& value) noexcept;
bool rtc_stats_equal(const NativeRtcStats& a, const NativeRtcStats& b) noexcept;
rust::Vec<std::uint8_t> rtc_stats_json(const NativeRtcStats& value) noexcept;
std::unique_ptr<NativeRtcStatsAttribute> rtc_stats_attribute_new(rust::Slice<const std::uint8_t> name, const FfiRtcStatsValue& value) noexcept;
std::unique_ptr<NativeRtcStatsAttribute> rtc_stats_attribute_copy(const NativeRtcStatsAttribute& value) noexcept;
FfiRtcStatsAttribute rtc_stats_attribute_data(const NativeRtcStatsAttribute& value) noexcept;
bool rtc_stats_attribute_has_value(const NativeRtcStatsAttribute& value) noexcept;
bool rtc_stats_attribute_is_sequence(const NativeRtcStatsAttribute& value) noexcept;
bool rtc_stats_attribute_is_string(const NativeRtcStatsAttribute& value) noexcept;
rust::Vec<std::uint8_t> rtc_stats_attribute_string(const NativeRtcStatsAttribute& value) noexcept;
bool rtc_stats_attribute_equal(const NativeRtcStatsAttribute& a, const NativeRtcStatsAttribute& b) noexcept;
std::unique_ptr<NativeRtcStatsReport> rtc_stats_report_clone(const NativeRtcStatsReport& value) noexcept;
bool rtc_stats_report_same(const NativeRtcStatsReport& a, const NativeRtcStatsReport& b) noexcept;
std::int64_t rtc_stats_report_timestamp(const NativeRtcStatsReport& value) noexcept;
std::size_t rtc_stats_report_size(const NativeRtcStatsReport& value) noexcept;
rust::Vec<FfiRtcStatsBytes> rtc_stats_report_keys(const NativeRtcStatsReport& value) noexcept;
std::unique_ptr<NativeRtcStats> rtc_stats_report_get(const NativeRtcStatsReport& value, rust::Slice<const std::uint8_t> id, std::uint8_t class_kind) noexcept;
rust::Vec<FfiRtcStatsEntry> rtc_stats_report_of_class(const NativeRtcStatsReport& value, std::uint8_t class_kind) noexcept;
bool rtc_stats_report_serializable(const NativeRtcStatsReport& value) noexcept;
rust::Vec<std::uint8_t> rtc_stats_report_json(const NativeRtcStatsReport& value) noexcept;
std::unique_ptr<NativeRtcStatsReportBuilder> rtc_stats_report_copy(const NativeRtcStatsReport& value) noexcept;
std::unique_ptr<NativeRtcStatsReportBuilder> rtc_stats_report_new(std::int64_t timestamp) noexcept;
std::size_t rtc_stats_builder_size(const NativeRtcStatsReportBuilder& value) noexcept;
bool rtc_stats_builder_add(NativeRtcStatsReportBuilder& value, std::unique_ptr<NativeRtcStats> stat, bool try_add) noexcept;
std::unique_ptr<NativeRtcStats> rtc_stats_builder_take(NativeRtcStatsReportBuilder& value, rust::Slice<const std::uint8_t> id) noexcept;
bool rtc_stats_builder_merge(NativeRtcStatsReportBuilder& value, NativeRtcStatsReportBuilder& other) noexcept;
std::unique_ptr<NativeRtcStatsReport> rtc_stats_report_freeze(std::unique_ptr<NativeRtcStatsReportBuilder> value) noexcept;
} // namespace pulsebeam::webrtc_sys
