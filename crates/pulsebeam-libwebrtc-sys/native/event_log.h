#pragma once

#include <cstdint>
#include <functional>
#include <memory>

#include "api/environment/environment.h"
#include "api/rtc_event_log/rtc_event.h"
#include "api/rtc_event_log/rtc_event_log.h"
#include "api/rtc_event_log/rtc_event_log_factory_interface.h"
#include "rust/cxx.h"

namespace pulsebeam::webrtc_sys {
struct FfiRtcEventPayload;
struct FfiRtcEventRtpExtension;
struct FfiRtcEventDependencyDescriptor;
struct FfiRtcEventDependencyStructure;
struct FfiRtpHeaderExtension;
struct RustRtcEventLog;
struct RustRtcEventLogFactory;
class NativeEnvironment;
class NativeEventLogOutput;

// Exclusive ownership of an event, independent of a peer or factory graph.
class NativeRtcEvent final {
 public:
  explicit NativeRtcEvent(std::unique_ptr<webrtc::RtcEvent> event) noexcept;
  ~NativeRtcEvent();
  const webrtc::RtcEvent& event() const noexcept;
  std::unique_ptr<webrtc::RtcEvent> Take() noexcept;
 private:
  std::unique_ptr<webrtc::RtcEvent> event_;
};
std::unique_ptr<NativeRtcEvent> new_rtc_event(std::uint32_t kind, const FfiRtcEventPayload& payload) noexcept;
std::unique_ptr<NativeRtcEvent> new_rtc_event_boundary(bool begin, std::int64_t timestamp_us, std::int64_t utc_start_us) noexcept;
std::unique_ptr<NativeRtcEvent> new_rtc_event_rtp(bool incoming, rust::Slice<const std::uint8_t> packet, rust::Slice<const FfiRtpHeaderExtension> extensions, std::int32_t probe_cluster_id, bool has_rtx_sequence, std::uint16_t rtx_sequence) noexcept;
std::unique_ptr<NativeRtcEvent> rtc_event_copy(const NativeRtcEvent& event) noexcept;
FfiRtcEventDependencyDescriptor rtc_event_rtp_dependency_descriptor(const NativeRtcEvent& event, bool has_structure, FfiRtcEventDependencyStructure structure) noexcept;
FfiRtcEventRtpExtension rtc_event_rtp_extension(const NativeRtcEvent& event, std::uint8_t kind) noexcept;
FfiRtcEventPayload rtc_event_payload(const NativeRtcEvent& event) noexcept;
std::uint32_t rtc_event_kind(const NativeRtcEvent& event) noexcept;
bool rtc_event_is_config(const NativeRtcEvent& event) noexcept;
std::uint32_t rtc_event_group_key(const NativeRtcEvent& event) noexcept;
std::int64_t rtc_event_timestamp_us(const NativeRtcEvent& event) noexcept;
void rtc_event_set_timestamp(NativeRtcEvent& event, std::int64_t timestamp_us) noexcept;

class NativeEventLogStop final {
 public:
  explicit NativeEventLogStop(std::function<void()> callback) noexcept;
  ~NativeEventLogStop();
  void Complete() noexcept;
 private:
  std::function<void()> callback_;
};
void event_log_stop_complete(std::unique_ptr<NativeEventLogStop> completion) noexcept;

class NativeRtcEventLogFactory final {
 public:
  explicit NativeRtcEventLogFactory(rust::Box<RustRtcEventLogFactory> factory) noexcept;
  ~NativeRtcEventLogFactory();
  std::unique_ptr<webrtc::RtcEventLogFactoryInterface> CreateAdapter(bool installed = true) const noexcept;
 private:
  std::shared_ptr<rust::Box<RustRtcEventLogFactory>> factory_;
};
std::unique_ptr<NativeRtcEventLogFactory> new_rtc_event_log_factory(rust::Box<RustRtcEventLogFactory> factory) noexcept;

struct RustEventLogRetention;
struct RustEnvironmentFactoryRandomness;
void retain_event_log_randomness(webrtc::RtcEventLog& logger, rust::Box<RustEnvironmentFactoryRandomness> retention) noexcept;
struct RustEventLogProductStop;

// Scoped result propagation through SDK void/bool peer logger entry points.
class EventLogCallScope final {
 public:
  EventLogCallScope() noexcept;
  ~EventLogCallScope();
  std::uint8_t status = 0;
 private:
  friend void record_event_log_status(std::uint8_t) noexcept;
  EventLogCallScope* previous_;
};
std::unique_ptr<webrtc::RtcEventLog> shared_custom_event_log(rust::Box<RustRtcEventLog> logger) noexcept;
webrtc::Environment shared_default_event_log(webrtc::Environment environment) noexcept;
std::unique_ptr<webrtc::RtcEventLogFactoryInterface> shared_builtin_event_log_factory(
    rust::Box<RustEventLogRetention> retention) noexcept;

class NativeEventLogUtility final {
 public:
  struct State;
  explicit NativeEventLogUtility(std::unique_ptr<State> state) noexcept;
  ~NativeEventLogUtility();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeEventLogUtility> environment_event_log(const NativeEnvironment& environment) noexcept;
std::uint8_t event_log_utility_start(NativeEventLogUtility& log,
    std::unique_ptr<NativeEventLogOutput> output, std::int64_t period_ms) noexcept;
std::uint8_t event_log_utility_log(NativeEventLogUtility& log, std::unique_ptr<NativeRtcEvent> event) noexcept;
std::uint8_t event_log_utility_stop(NativeEventLogUtility& log) noexcept;
std::uint8_t event_log_utility_stop_async(NativeEventLogUtility& log,
    rust::Box<RustEventLogProductStop> completion, rust::Box<RustEventLogRetention> retention) noexcept;
void event_log_utility_close(NativeEventLogUtility& log) noexcept;
// Private fixture: exercise the SDK producer alias, not the public busy check.
void test_event_log_sdk_dispatch(const NativeEnvironment& environment, std::unique_ptr<NativeRtcEvent> event) noexcept;
std::unique_ptr<NativeEnvironment> test_stock_event_log_environment(const NativeEnvironment& environment,
    rust::Box<RustEventLogRetention> retention) noexcept;
class NativeRtcEventLogProduct final {
 public:
  struct State;
  explicit NativeRtcEventLogProduct(std::unique_ptr<State> state) noexcept;
  ~NativeRtcEventLogProduct();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeRtcEventLogProduct> event_log_product_create(
    std::unique_ptr<NativeRtcEventLogFactory> factory, const NativeEnvironment& environment,
    rust::Box<RustEventLogRetention> retention, bool controlled, std::uint8_t& error) noexcept;
std::uint8_t event_log_product_start(NativeRtcEventLogProduct& log,
    std::unique_ptr<NativeEventLogOutput> output, std::int64_t period_ms) noexcept;
std::uint8_t event_log_product_log(NativeRtcEventLogProduct& log,
    std::unique_ptr<NativeRtcEvent> event) noexcept;
std::uint8_t event_log_product_stop(NativeRtcEventLogProduct& log) noexcept;
std::uint8_t event_log_product_stop_async(NativeRtcEventLogProduct& log,
    rust::Box<RustEventLogProductStop> completion) noexcept;
void event_log_product_close(NativeRtcEventLogProduct& log) noexcept;
std::unique_ptr<webrtc::RtcEventLog> take_event_log_product(NativeRtcEventLogProduct& log, std::uint8_t& error) noexcept;

// Environment's leaf takes ownership of the custom logger; it does not borrow
// Rust registration storage or require access to the application actor.
std::unique_ptr<webrtc::RtcEventLog> rtc_event_log_adapter(rust::Box<RustRtcEventLog> logger) noexcept;
// Only products of this bridge's custom factory/adapter may be queried here.
bool rtc_event_log_adapter_failed(const webrtc::RtcEventLog& logger) noexcept;
std::unique_ptr<NativeEnvironment> rtc_event_log_factory_environment(const webrtc::Environment& environment) noexcept;
}  // namespace pulsebeam::webrtc_sys
