#pragma once
#include <cstdint>
#include <cstddef>
#include <memory>
#include "rust/cxx.h"
namespace webrtc { class Clock; }
namespace pulsebeam::webrtc_sys {
struct RustClockProvider;
enum class ClockKind { Provider, Simulated, RealTime, RetainedSimulated, RetainedRealTime };
class NativeEnvironment;
class NativeClock final {
 public:
  explicit NativeClock(std::shared_ptr<webrtc::Clock> clock, ClockKind kind = ClockKind::Provider,
      bool monotonic = false, const webrtc::Clock* sdk_identity = nullptr) noexcept;
  ~NativeClock();
  const std::shared_ptr<webrtc::Clock>& clock() const noexcept;
  const webrtc::Clock* sdk_identity() const noexcept;
  bool simulated() const noexcept;
  bool owned_simulated() const noexcept;
  bool real_time() const noexcept;
  bool owned_real_time() const noexcept;
  bool monotonic() const noexcept;
 private:
  std::shared_ptr<webrtc::Clock> clock_;
  ClockKind kind_;
  bool monotonic_;
  const webrtc::Clock* sdk_identity_;
};
std::unique_ptr<NativeClock> new_provider_clock(rust::Box<RustClockProvider> provider, bool monotonic) noexcept;
std::unique_ptr<NativeClock> new_simulated_clock(std::int64_t initial_time_us,
    std::int64_t minimum_time_us, std::int64_t maximum_time_us) noexcept;
std::unique_ptr<NativeClock> new_real_time_clock() noexcept;
std::unique_ptr<NativeClock> environment_clock(const NativeEnvironment& environment,
    const NativeClock* selected, bool manual) noexcept;
bool clock_is_monotonic(const NativeClock& clock) noexcept;
struct FfiClockNtpSample;
// A matched SDK sample, only available while a real-time handle retains its lease.
FfiClockNtpSample real_time_clock_native_sample(const NativeClock& clock) noexcept;
// Only called through the Rust SimulatedClock newtype.
bool advance_simulated_clock(const NativeClock& clock, std::int64_t delta_us) noexcept;
std::int64_t provider_clock_timestamp(const NativeClock& clock) noexcept;
struct FfiClockNtpValue;
FfiClockNtpValue provider_clock_current_ntp(const NativeClock& clock) noexcept;
FfiClockNtpValue provider_clock_convert_ntp(const NativeClock& clock, std::int64_t timestamp) noexcept;
std::unique_ptr<webrtc::Clock> clock_reference(const NativeClock& clock) noexcept;
std::int64_t clock_timestamp_operation(std::int64_t left, std::int64_t right, std::uint8_t operation) noexcept;
std::int64_t clock_delta_operation(std::int64_t left, std::int64_t right, std::uint8_t operation) noexcept;
std::int64_t clock_delta_size_scalar(std::int64_t value, std::size_t scalar, bool divide) noexcept;
std::int64_t clock_delta_float_scalar(std::int64_t value, double scalar, bool divide) noexcept;
std::int64_t clock_delta_div_float32(std::int64_t value, float scalar) noexcept;
std::int64_t clock_delta_negate(std::int64_t value) noexcept;
double clock_delta_ratio(std::int64_t left, std::int64_t right) noexcept;
rust::String clock_delta_string(std::int64_t value) noexcept;
struct FfiClockValue;
FfiClockValue clock_timestamp_from_int(std::int64_t value, std::uint8_t unit) noexcept;
FfiClockValue clock_timestamp_from_float(double value, std::uint8_t unit) noexcept;
FfiClockValue clock_timestamp_from_float32(float value, std::uint8_t unit) noexcept;
FfiClockValue clock_timestamp_convert(std::int64_t value, std::uint8_t unit) noexcept;
double clock_timestamp_float(std::int64_t value, std::uint8_t unit) noexcept;
float clock_timestamp_float32(std::int64_t value, std::uint8_t unit) noexcept;
rust::String clock_timestamp_string(std::int64_t value) noexcept;
std::int64_t ntp_time_to_ms(std::uint64_t value) noexcept;
std::int64_t ntp_time_to_utc_us(std::uint64_t value) noexcept;
std::int64_t clock_ms_to_q32x32(std::int64_t value) noexcept;
std::uint64_t clock_ms_to_uq32x32(std::int64_t value) noexcept;
std::int64_t clock_q32x32_to_ms(std::int64_t value) noexcept;
std::int64_t clock_uq32x32_to_ms(std::uint64_t value) noexcept;
std::int64_t clock_q32x32_to_us(std::int64_t value) noexcept;
std::int64_t clock_uq32x32_to_us(std::uint64_t value) noexcept;
}  // namespace pulsebeam::webrtc_sys
