#include "pulsebeam-webrtc-sys/native/clock.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include <limits>
#include <cmath>
#include <mutex>
#include <utility>
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "system_wrappers/include/clock.h"
#include "api/environment/environment.h"
#include "system_wrappers/include/ntp_time.h"
#include "rtc_base/time_utils.h"
namespace pulsebeam::webrtc_sys {
namespace {
constexpr auto kMin = std::numeric_limits<std::int64_t>::min();
constexpr auto kMax = std::numeric_limits<std::int64_t>::max();
webrtc::Timestamp Timestamp(std::int64_t value) {
  if (value == kMin) return webrtc::Timestamp::MinusInfinity();
  if (value == kMax) return webrtc::Timestamp::PlusInfinity();
  return webrtc::Timestamp::Micros(value);
}
FfiClockValue Value(webrtc::Timestamp value) {
  return {true, value.IsPlusInfinity() ? kMax : value.us_or(kMin)};
}
webrtc::TimeDelta Delta(std::int64_t value) {
  if (value == kMin) return webrtc::TimeDelta::MinusInfinity();
  if (value == kMax) return webrtc::TimeDelta::PlusInfinity();
  return webrtc::TimeDelta::Micros(value);
}
std::int64_t Raw(webrtc::TimeDelta value) { return value.IsPlusInfinity() ? kMax : value.us_or(kMin); }
template <class F> FfiClockValue FloatingTimestamp(F value, std::uint8_t unit) {
  const F scaled = value * (unit == 0 ? F(1) : unit == 1 ? F(1000) : F(1000000));
  // Preserve the native floating type's multiplication and truncation. Infinity
  // is supported, but checked casts cannot accept NaN or finite sentinel bounds.
  const F limit = std::ldexp(F(1), 63);
  if (std::isnan(scaled) || (std::isfinite(scaled) && (scaled <= -limit || scaled >= limit))) return {false, 0};
  return Value(unit == 0 ? webrtc::Timestamp::Micros(value) : unit == 1 ? webrtc::Timestamp::Millis(value) : webrtc::Timestamp::Seconds(value));
}
class RustClockAdapter final : public webrtc::Clock {
 public:
  explicit RustClockAdapter(rust::Box<RustClockProvider> provider) : provider_(std::move(provider)) {}
  webrtc::Timestamp CurrentTime() override { return Timestamp(clock_provider_current_time(*provider_)); }
  webrtc::NtpTime ConvertTimestampToNtpTime(webrtc::Timestamp timestamp) override {
    return webrtc::NtpTime(clock_provider_convert_ntp(*provider_, Value(timestamp).value));
  }
 private:
  rust::Box<RustClockProvider> provider_;
};
// Retain the process lease through every SDK clock consumer. Conversion and
// calibration remain native; no binding-owned arithmetic shadows the SDK.
class RealTimeClockAdapter final : public webrtc::Clock {
 public:
  explicit RealTimeClockAdapter(std::unique_ptr<NativeProductionLease> lease)
      : lease_(std::move(lease)), clock_(webrtc::Clock::GetRealTimeClock()) {}
  webrtc::Timestamp CurrentTime() override { return clock_->CurrentTime(); }
  webrtc::NtpTime ConvertTimestampToNtpTime(webrtc::Timestamp timestamp) override {
    if (!timestamp.IsFinite()) return webrtc::NtpTime();
    return clock_->ConvertTimestampToNtpTime(timestamp);
  }
  FfiClockNtpSample Sample() const {
    const auto timestamp = clock_->CurrentTime();
    return {timestamp.us(), static_cast<std::uint64_t>(
        clock_->ConvertTimestampToNtpTime(timestamp))};
  }
 private:
  // The shared adapter, not just its Rust handle, retains the lease. Installed
  // environments can outlive all Rust clock handles without admitting hooks.
  std::unique_ptr<NativeProductionLease> lease_;
  webrtc::Clock* clock_;
};
class SimulatedClockAdapter final : public webrtc::Clock {
 public:
  SimulatedClockAdapter(std::int64_t initial, std::int64_t minimum,
                        std::int64_t maximum)
      : clock_(initial), minimum_(minimum), maximum_(maximum) {}
  webrtc::Timestamp CurrentTime() override {
    std::lock_guard lock(mutex_);
    return clock_.CurrentTime();
  }
  webrtc::NtpTime ConvertTimestampToNtpTime(webrtc::Timestamp timestamp) override {
    if (!timestamp.IsFinite()) return webrtc::NtpTime();
    return clock_.ConvertTimestampToNtpTime(timestamp);
  }
  bool Advance(std::int64_t delta_us) {
    if (delta_us == kMin || delta_us == kMax) return false;
    std::lock_guard lock(mutex_);
    // Integral atomic fetch_add wraps by the SDK's contract. Unrestricted
    // clocks exclude only unreadable sentinel results; opt-in bounds also
    // constrain the wrapped result. Validate and mutate under the same lock.
    const auto next = static_cast<std::uint64_t>(clock_.TimeInMicroseconds()) +
                      static_cast<std::uint64_t>(delta_us);
    if (next == static_cast<std::uint64_t>(kMin) || next == static_cast<std::uint64_t>(kMax)) return false;
    const auto signed_next = next <= static_cast<std::uint64_t>(kMax)
        ? static_cast<std::int64_t>(next)
        : -1 - static_cast<std::int64_t>(~next);
    if (signed_next < minimum_ || signed_next > maximum_) return false;
    clock_.AdvanceTime(webrtc::TimeDelta::Micros(delta_us));
    return true;
  }
 private:
  std::mutex mutex_;
  webrtc::SimulatedClock clock_;
  const std::int64_t minimum_;
  const std::int64_t maximum_;
};
FfiClockNtpValue ConvertNtp(const NativeClock& clock, webrtc::Timestamp timestamp) {
  if ((clock.simulated() || clock.real_time()) && !timestamp.IsFinite()) return {1, 0};
  return {0, static_cast<std::uint64_t>(clock.clock()->ConvertTimestampToNtpTime(timestamp))};
}
// The delegate owns the exact SDK environment, not an echoed clock selection.
// Its storage owns the reached bridge clock adapters. A raw SDK real-time
// singleton needs the same process lease as real_time().
class EnvironmentClockReference final : public webrtc::Clock {
 public:
  EnvironmentClockReference(webrtc::Environment environment,
                            std::shared_ptr<webrtc::Clock> real_time = {})
      : environment_(std::move(environment)), real_time_(std::move(real_time)) {}
  webrtc::Timestamp CurrentTime() override { return environment_.clock().CurrentTime(); }
  webrtc::NtpTime ConvertTimestampToNtpTime(webrtc::Timestamp timestamp) override {
    return real_time_ ? real_time_->ConvertTimestampToNtpTime(timestamp)
                      : environment_.clock().ConvertTimestampToNtpTime(timestamp);
  }
 private:
  webrtc::Environment environment_;
  std::shared_ptr<webrtc::Clock> real_time_;
};
class ClockReference final : public webrtc::Clock {
 public:
  explicit ClockReference(std::shared_ptr<webrtc::Clock> clock) : clock_(std::move(clock)) {}
  webrtc::Timestamp CurrentTime() override { return clock_->CurrentTime(); }
  webrtc::NtpTime ConvertTimestampToNtpTime(webrtc::Timestamp timestamp) override { return clock_->ConvertTimestampToNtpTime(timestamp); }
 private:
  std::shared_ptr<webrtc::Clock> clock_;
};
}
NativeClock::NativeClock(std::shared_ptr<webrtc::Clock> clock, ClockKind kind, bool monotonic, const webrtc::Clock* sdk_identity) noexcept
    : clock_(std::move(clock)), kind_(kind), monotonic_(monotonic),
      sdk_identity_(sdk_identity ? sdk_identity : clock_.get()) {}
NativeClock::~NativeClock() = default;
const std::shared_ptr<webrtc::Clock>& NativeClock::clock() const noexcept { return clock_; }
const webrtc::Clock* NativeClock::sdk_identity() const noexcept { return sdk_identity_; }
bool NativeClock::simulated() const noexcept {
  return kind_ == ClockKind::Simulated || kind_ == ClockKind::RetainedSimulated;
}
bool NativeClock::owned_simulated() const noexcept { return kind_ == ClockKind::Simulated; }
bool NativeClock::real_time() const noexcept {
  return kind_ == ClockKind::RealTime || kind_ == ClockKind::RetainedRealTime;
}
bool NativeClock::owned_real_time() const noexcept { return kind_ == ClockKind::RealTime; }
bool NativeClock::monotonic() const noexcept { return monotonic_; }
std::unique_ptr<NativeClock> new_real_time_clock() noexcept {
  auto lease = new_production_lease();
  if (!lease || webrtc::GetClockForTesting()) return nullptr;
  return std::make_unique<NativeClock>(
      std::make_shared<RealTimeClockAdapter>(std::move(lease)), ClockKind::RealTime, true,
      webrtc::Clock::GetRealTimeClock());
}
std::unique_ptr<NativeClock> environment_clock(const NativeEnvironment& environment,
    const NativeClock* selected, bool manual) noexcept {
  auto sdk = environment.environment();
  const auto* identity = &sdk.clock();
  if (&sdk.clock() == webrtc::Clock::GetRealTimeClock()) {
    auto real_time = new_real_time_clock();
    if (!real_time) return nullptr;
    return std::make_unique<NativeClock>(
        std::make_shared<EnvironmentClockReference>(std::move(sdk), real_time->clock()),
        ClockKind::RetainedRealTime, true, identity);
  }
  const auto kind = manual || (selected && selected->simulated())
      ? ClockKind::RetainedSimulated
      : selected && selected->real_time() ? ClockKind::RetainedRealTime : ClockKind::Provider;
  const bool monotonic = manual || (selected && selected->monotonic()) ||
      environment_clock_time_bounds(environment).monotonic;
  return std::make_unique<NativeClock>(
      std::make_shared<EnvironmentClockReference>(std::move(sdk)), kind, monotonic, identity);
}
bool clock_is_monotonic(const NativeClock& clock) noexcept { return clock.monotonic(); }
FfiClockNtpSample real_time_clock_native_sample(const NativeClock& clock) noexcept {
  if (!clock.owned_real_time()) return {0, 0};
  return static_cast<RealTimeClockAdapter&>(*clock.clock()).Sample();
}
std::unique_ptr<NativeClock> new_simulated_clock(std::int64_t initial_time_us,
    std::int64_t minimum_time_us, std::int64_t maximum_time_us) noexcept {
  if (minimum_time_us == kMin || maximum_time_us == kMax ||
      minimum_time_us > maximum_time_us ||
      initial_time_us < minimum_time_us || initial_time_us > maximum_time_us) return nullptr;
  return std::make_unique<NativeClock>(
      std::make_shared<SimulatedClockAdapter>(initial_time_us, minimum_time_us, maximum_time_us),
      ClockKind::Simulated);
}
bool advance_simulated_clock(const NativeClock& clock, std::int64_t delta_us) noexcept {
  if (!clock.owned_simulated()) return false;
  return static_cast<SimulatedClockAdapter&>(*clock.clock()).Advance(delta_us);
}
std::unique_ptr<NativeClock> new_provider_clock(rust::Box<RustClockProvider> provider, bool monotonic) noexcept {
  return std::make_unique<NativeClock>(std::make_shared<RustClockAdapter>(std::move(provider)), ClockKind::Provider, monotonic);
}
std::int64_t provider_clock_timestamp(const NativeClock& clock) noexcept { return Value(clock.clock()->CurrentTime()).value; }
FfiClockNtpValue provider_clock_current_ntp(const NativeClock& clock) noexcept { return ConvertNtp(clock, clock.clock()->CurrentTime()); }
FfiClockNtpValue provider_clock_convert_ntp(const NativeClock& clock, std::int64_t timestamp) noexcept { return ConvertNtp(clock, Timestamp(timestamp)); }
std::unique_ptr<webrtc::Clock> clock_reference(const NativeClock& clock) noexcept { return std::make_unique<ClockReference>(clock.clock()); }
// All arithmetic entry points are private bridge operations. Rust validates
// each exact intermediate and native finite/infinity precondition before entry.
std::int64_t clock_timestamp_operation(std::int64_t left, std::int64_t right, std::uint8_t operation) noexcept {
  if (operation == 2) return Raw(Timestamp(left) - Timestamp(right));
  return Value(operation == 0 ? Timestamp(left) + Delta(right) : Timestamp(left) - Delta(right)).value;
}
std::int64_t clock_delta_operation(std::int64_t left, std::int64_t right, std::uint8_t operation) noexcept {
  const auto value = Delta(left);
  switch (operation) {
    case 0: return Raw(value + Delta(right));
    case 1: return Raw(value - Delta(right));
    case 2: return Raw(value * right);
    case 3: return Raw(value / right);
    case 4: return Raw(value.RoundTo(Delta(right)));
    case 5: return Raw(value.RoundUpTo(Delta(right)));
    default: return Raw(value.RoundDownTo(Delta(right)));
  }
}
std::int64_t clock_delta_size_scalar(std::int64_t value, std::size_t scalar, bool divide) noexcept { return Raw(divide ? Delta(value) / scalar : Delta(value) * scalar); }
std::int64_t clock_delta_float_scalar(std::int64_t value, double scalar, bool divide) noexcept { return Raw(divide ? Delta(value) / scalar : Delta(value) * scalar); }
std::int64_t clock_delta_div_float32(std::int64_t value, float scalar) noexcept { return Raw(Delta(value) / scalar); }
std::int64_t clock_delta_negate(std::int64_t value) noexcept { return Raw(-Delta(value)); }
double clock_delta_ratio(std::int64_t left, std::int64_t right) noexcept { return Delta(left) / Delta(right); }
rust::String clock_delta_string(std::int64_t value) noexcept { return rust::String(webrtc::ToString(Delta(value))); }
FfiClockValue clock_timestamp_from_int(std::int64_t value, std::uint8_t unit) noexcept {
  const std::int64_t scale = unit == 0 ? 1 : unit == 1 ? 1000 : 1000000;
  // Match the native integral constructor's strict pre-multiplication bounds.
  if (value <= kMin / scale || value >= kMax / scale) return {false, 0};
  return Value(unit == 0 ? webrtc::Timestamp::Micros(value) : unit == 1 ? webrtc::Timestamp::Millis(value) : webrtc::Timestamp::Seconds(value));
}
FfiClockValue clock_timestamp_from_float(double value, std::uint8_t unit) noexcept {
  return FloatingTimestamp(value, unit);
}
FfiClockValue clock_timestamp_from_float32(float value, std::uint8_t unit) noexcept { return FloatingTimestamp(value, unit); }
FfiClockValue clock_timestamp_convert(std::int64_t value, std::uint8_t unit) noexcept {
  if (value == kMin || value == kMax) return {false, 0};
  if (unit == 3 && (value < kMin / 1000 || value > kMax / 1000)) return {false, 0};
  const auto timestamp = Timestamp(value);
  return {true, unit == 0 ? timestamp.us() : unit == 1 ? timestamp.ms() : unit == 2 ? timestamp.seconds() : timestamp.ns()};
}
double clock_timestamp_float(std::int64_t value, std::uint8_t unit) noexcept {
  const auto timestamp = Timestamp(value);
  return unit == 0 ? timestamp.us<double>() : unit == 1 ? timestamp.ms<double>() : unit == 2 ? timestamp.seconds<double>() : timestamp.ns<double>();
}
float clock_timestamp_float32(std::int64_t value, std::uint8_t unit) noexcept {
  const auto timestamp = Timestamp(value);
  return unit == 0 ? timestamp.us<float>() : unit == 1 ? timestamp.ms<float>() : unit == 2 ? timestamp.seconds<float>() : timestamp.ns<float>();
}
rust::String clock_timestamp_string(std::int64_t value) noexcept { return rust::String(webrtc::ToString(Timestamp(value))); }
std::int64_t ntp_time_to_ms(std::uint64_t value) noexcept { return webrtc::NtpTime(value).ToMs(); }
std::int64_t ntp_time_to_utc_us(std::uint64_t value) noexcept { return webrtc::Clock::NtpToUtc(webrtc::NtpTime(value)).us_or(std::numeric_limits<std::int64_t>::min()); }
std::int64_t clock_ms_to_q32x32(std::int64_t value) noexcept { return webrtc::Int64MsToQ32x32(value); }
std::uint64_t clock_ms_to_uq32x32(std::int64_t value) noexcept { return webrtc::Int64MsToUQ32x32(value); }
std::int64_t clock_q32x32_to_ms(std::int64_t value) noexcept { return webrtc::Q32x32ToInt64Ms(value); }
std::int64_t clock_uq32x32_to_ms(std::uint64_t value) noexcept { return webrtc::UQ32x32ToInt64Ms(value); }
std::int64_t clock_q32x32_to_us(std::int64_t value) noexcept { return webrtc::Q32x32ToInt64Us(value); }
std::int64_t clock_uq32x32_to_us(std::uint64_t value) noexcept { return webrtc::UQ32x32ToInt64Us(value); }
}  // namespace pulsebeam::webrtc_sys
