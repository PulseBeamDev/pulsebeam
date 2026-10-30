#include "pulsebeam-webrtc-sys/native/codec.h"
#include "pulsebeam-webrtc-sys/native/video_codec_values.h"
#include "pulsebeam-webrtc-sys/native/video_encoder_selector.h"
#include "pulsebeam-webrtc-sys/native/rtp_values.h"
#include "pulsebeam-webrtc-sys/native/encoded_image.h"
#include "pulsebeam-webrtc-sys/native/video_codec_callbacks.h"
#include "pulsebeam-webrtc-sys/native/raw_video_frame.h"
#include "pulsebeam-webrtc-sys/native/video.h"
#include "pulsebeam-webrtc-sys/native/peer.h"
#include "rtc_base/thread.h"
#include <condition_variable>

#include <algorithm>
#include <atomic>
#include <cfenv>
#include <cfloat>
#include <cmath>
#if defined(__linux__) && defined(__x86_64__)
#include <xmmintrin.h>
#endif
#include <cstring>
#include <limits>
#include <numeric>
#include <map>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <thread>
#include <utility>
#include <unordered_map>
#include <vector>

#include "absl/strings/match.h"
#include "absl/strings/str_cat.h"
#include "api/audio/audio_frame.h"
#include "api/audio_codecs/audio_decoder.h"
#include "api/audio_codecs/audio_decoder_factory.h"
#include "system_wrappers/include/clock.h"
#include "api/audio_codecs/audio_encoder_factory.h"
#include "api/audio_codecs/builtin_audio_decoder_factory.h"
#include "api/audio_codecs/builtin_audio_encoder_factory.h"
#include "api/audio_codecs/opus/audio_decoder_opus.h"
#include "api/audio_codecs/opus/audio_decoder_multi_channel_opus.h"
#include "api/audio_codecs/opus/audio_encoder_multi_channel_opus_config.h"
#include "common_audio/smoothing_filter.h"
#include "rtc_base/string_encode.h"
#include "api/environment/environment_factory.h"
#include "api/neteq/neteq.h"
#include "api/neteq/neteq_factory.h"
#include "api/neteq/default_neteq_controller_factory.h"
#include "modules/audio_coding/neteq/decoder_database.h"
#include "modules/audio_coding/neteq/expand.h"
#include "modules/audio_coding/neteq/neteq_impl.h"
#include "modules/audio_coding/neteq/packet_buffer.h"
#include "modules/audio_coding/neteq/sync_buffer.h"
#include "api/rtp_headers.h"
#include "pulsebeam-webrtc-sys/native/audio_frame_values.h"
#include "pulsebeam-webrtc-sys/native/media_values.h"
#include "api/make_ref_counted.h"
#include "api/frame_transformer_interface.h"
#include "pulsebeam-webrtc-sys/native/frame_callback_slot.h"
#include "api/task_queue/task_queue_base.h"
#include "api/scoped_refptr.h"
#include "api/units/data_rate.h"
#include "api/video/encoded_image.h"
#include "api/video/i420_buffer.h"
#include "api/video/video_frame.h"
#include "api/video/video_frame_type.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/scalability_mode.h"
#include "api/video_codecs/scalability_mode_helper.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_codec.h"
#include "api/video_codecs/video_decoder.h"
#include "api/video_codecs/video_decoder_factory.h"
#include "api/video_codecs/video_encoder.h"
#include "api/video_codecs/video_encoder_factory.h"
#include "modules/audio_coding/codecs/opus/audio_coder_opus_common.h"
#include "modules/audio_coding/codecs/opus/audio_encoder_opus.h"
#include "modules/audio_coding/audio_network_adaptor/bitrate_controller.h"
#include "modules/audio_coding/audio_network_adaptor/audio_network_adaptor_impl.h"
#include "api/audio_codecs/opus/audio_encoder_opus.h"
#if WEBRTC_ENABLE_PROTOBUF
#include "modules/audio_coding/audio_network_adaptor/util/threshold_curve.h"
#ifdef WEBRTC_ANDROID_PLATFORM_BUILD
#include "external/webrtc/webrtc/modules/audio_coding/audio_network_adaptor/config.pb.h"
#else
#include "modules/audio_coding/audio_network_adaptor/config.pb.h"
#endif
#endif
#include "modules/video_coding/codecs/vp8/include/vp8.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "media/engine/simulcast_encoder_adapter.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "pulsebeam-webrtc-sys/native/opus_carrier.h"
#include "pulsebeam-webrtc-sys/native/audio_encoder_ana_access.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/event_log.h"

namespace pulsebeam::webrtc_sys {


namespace {
bool StockNetEqHasPackets(const webrtc::NetEq& value);
bool StockNetEqCodecSafe(int payload_type, const webrtc::SdpAudioFormat& format) {
  if (payload_type < 0 || payload_type > 127) return false;
  // DecoderInfo::CngDecoder::Create asserts this even at lazy registration.
  return !absl::EqualsIgnoreCase(format.name, "CN") ||
      format.clockrate_hz == 8000 || format.clockrate_hz == 16000 ||
      format.clockrate_hz == 32000 || format.clockrate_hz == 48000;
}
}  // namespace

namespace {
// Installed factory errors must not return null to ChannelReceive or secretly
// replace the requested implementation with a stock decoder. This product is
// permanently failed, has no decoder/clock side effects, and remains callable.
class FailedNetEq final : public webrtc::NetEq {
 public:
  explicit FailedNetEq(int configured_rate)
      : rate_(configured_rate >= 1000 && configured_rate <= 192000
          ? configured_rate : 8000) {}
  int InsertPacket(const webrtc::RTPHeader&, std::span<const uint8_t>) override { return kFail; }
  int InsertPacket(const webrtc::RTPHeader&, std::span<const uint8_t>, webrtc::Timestamp) override { return kFail; }
  int InsertPacket(const webrtc::RTPHeader&, std::span<const uint8_t>, const webrtc::RtpPacketInfo&) override { return kFail; }
  void InsertEmptyPacket(const webrtc::RTPHeader&) override {}
  int GetAudio(webrtc::AudioFrame*, bool*, int*, std::optional<Operation>) override { return kFail; }
  void SetCodecs(const std::map<int, webrtc::SdpAudioFormat>&) override {}
  bool RegisterPayloadType(int, const webrtc::SdpAudioFormat&) override { return false; }
  bool CreateDecoder(int) override { return false; }
  int RemovePayloadType(uint8_t) override { return kFail; }
  void SetMaximumBufferPackets(size_t) override {}
  void SetFastAccelerate(bool) override {}
  void RemoveAllPayloadTypes() override {}
  bool SetMinimumDelay(int) override { return false; }
  bool SetMaximumDelay(int) override { return false; }
  bool SetBaseMinimumDelayMs(int) override { return false; }
  int GetBaseMinimumDelayMs() const override { return 0; }
  int TargetDelayMs() const override { return 0; }
  int FilteredCurrentDelayMs() const override { return 0; }
  int NetworkStatistics(webrtc::NetEqNetworkStatistics* stats) override {
    if (stats) *stats = {};
    return kFail;
  }
  webrtc::NetEqNetworkStatistics CurrentNetworkStatistics() const override { return {}; }
  webrtc::NetEqLifetimeStatistics GetLifetimeStatistics() const override { return {}; }
  webrtc::NetEqOperationsAndState GetOperationsAndState() const override { return {}; }
  std::optional<uint32_t> GetPlayoutTimestamp() const override { return std::nullopt; }
  int last_output_sample_rate_hz() const override { return rate_; }
  std::optional<DecoderFormat> GetDecoderFormat(int) const override { return std::nullopt; }
  std::optional<DecoderFormat> GetCurrentDecoderFormat() const override { return std::nullopt; }
  void FlushBuffers() override {}
  void EnableNack(size_t) override {}
  void DisableNack() override {}
  std::vector<uint16_t> GetNackList(int64_t) const override { return {}; }
  int SyncBufferSizeMs() const override { return 0; }
 private:
  const int rate_;
};
}  // namespace

namespace {
// This synchronous marker proves only a crate-owned frame's initialization.
// A native SDK callback gets no such proof. Nested calls restore prior context;
// no addresses are retained between calls and pointer reuse conveys no proof.
thread_local const webrtc::AudioFrame* initialized_neteq_frame = nullptr;
class OwnedNetEqFrameScope final {
 public:
  explicit OwnedNetEqFrameScope(const webrtc::AudioFrame& frame)
      : previous_(initialized_neteq_frame) { initialized_neteq_frame = &frame; }
  ~OwnedNetEqFrameScope() { initialized_neteq_frame = previous_; }
 private:
  const webrtc::AudioFrame* previous_;
};
class CallerNetEq final : public webrtc::NetEq {
 public:
  CallerNetEq(const webrtc::Environment& environment, rust::Box<RustNetEqProvider> provider)
      : environment_(environment), provider_(std::move(provider)) {}
  bool failed() const { return neteq_provider_failed(*provider_); }
  int InsertPacket(const webrtc::RTPHeader& header, std::span<const uint8_t> payload) override {
    return neteq_provider_insert_packet(*provider_, MediaRtpHeaderToFfi(header), {payload.data(), payload.size()});
  }
  int InsertPacket(const webrtc::RTPHeader& header, std::span<const uint8_t> payload, webrtc::Timestamp time) override {
    return neteq_provider_insert_packet_at(*provider_, MediaRtpHeaderToFfi(header), {payload.data(), payload.size()},
        time.IsMinusInfinity() ? INT64_MIN : time.IsPlusInfinity() ? INT64_MAX : time.us());
  }
  int InsertPacket(const webrtc::RTPHeader& header, std::span<const uint8_t> payload, const webrtc::RtpPacketInfo& info) override {
    return neteq_provider_insert_packet_info(*provider_, MediaRtpHeaderToFfi(header), {payload.data(), payload.size()}, MediaPacketInfoToFfi(info));
  }
  void InsertEmptyPacket(const webrtc::RTPHeader& header) override {
    neteq_provider_insert_empty_packet(*provider_, MediaRtpHeaderToFfi(header));
  }
  int GetAudio(webrtc::AudioFrame* frame, bool* muted, int* rate, std::optional<Operation> operation) override {
    if (!frame) return kFail;
    NativeAudioFrame borrowed(*frame);
    auto output = neteq_provider_get_audio(*provider_, borrowed,
        initialized_neteq_frame == frame, muted != nullptr, rate != nullptr,
        operation.has_value(), operation ? static_cast<int>(*operation) : 0);
    // These are output-only pointers and may be uninitialized on SDK entry.
    // Never snapshot their old contents. Unwritten slots stay untouched.
    if (muted && output.muted_written) *muted = output.muted;
    if (rate && output.rate_written) *rate = output.sample_rate_hz;
    return output.code;
  }
  void SetCodecs(const std::map<int, webrtc::SdpAudioFormat>& codecs) override {
    rust::Vec<FfiNetEqCodec> values;
    for (const auto& [type, format] : codecs) values.push_back({type, AudioFormatToFfi(format)});
    neteq_provider_set_codecs(*provider_, std::move(values));
  }
  bool RegisterPayloadType(int type, const webrtc::SdpAudioFormat& format) override {
    return neteq_provider_register_payload_type(*provider_, type, AudioFormatToFfi(format));
  }
  bool CreateDecoder(int type) override { return neteq_provider_create_decoder(*provider_, type); }
  int RemovePayloadType(uint8_t type) override { return neteq_provider_remove_payload_type(*provider_, type); }
  void RemoveAllPayloadTypes() override { neteq_provider_remove_all_payload_types(*provider_); }
  void SetMaximumBufferPackets(size_t count) override { neteq_provider_set_maximum_buffer_packets(*provider_, count); }
  void SetFastAccelerate(bool enable) override { neteq_provider_set_fast_accelerate(*provider_, enable); }
  bool SetMinimumDelay(int delay) override { return neteq_provider_set_minimum_delay(*provider_, delay); }
  bool SetMaximumDelay(int delay) override { return neteq_provider_set_maximum_delay(*provider_, delay); }
  bool SetBaseMinimumDelayMs(int delay) override { return neteq_provider_set_base_minimum_delay(*provider_, delay); }
  int GetBaseMinimumDelayMs() const override { return neteq_provider_base_minimum_delay(*provider_); }
  int TargetDelayMs() const override { return neteq_provider_target_delay(*provider_); }
  int FilteredCurrentDelayMs() const override { return neteq_provider_filtered_current_delay(*provider_); }
  int NetworkStatistics(webrtc::NetEqNetworkStatistics* stats) override {
    auto output = neteq_provider_network_statistics(*provider_, stats != nullptr);
    if (stats && output.written) *stats = NetEqNetworkStatisticsFromFfi(output.value);
    return output.code;
  }
  webrtc::NetEqNetworkStatistics CurrentNetworkStatistics() const override {
    return NetEqNetworkStatisticsFromFfi(neteq_provider_current_network_statistics(*provider_));
  }
  webrtc::NetEqLifetimeStatistics GetLifetimeStatistics() const override {
    return NetEqLifetimeStatisticsFromFfi(neteq_provider_lifetime_statistics(*provider_));
  }
  webrtc::NetEqOperationsAndState GetOperationsAndState() const override {
    return NetEqOperationsAndStateFromFfi(neteq_provider_operations_and_state(*provider_));
  }
  std::optional<uint32_t> GetPlayoutTimestamp() const override {
    bool present = false;
    auto timestamp = neteq_provider_playout_timestamp(*provider_, present);
    return present ? std::optional(timestamp) : std::nullopt;
  }
  int last_output_sample_rate_hz() const override { return neteq_provider_last_output_sample_rate(*provider_); }
  std::optional<DecoderFormat> GetDecoderFormat(int type) const override {
    auto result = neteq_provider_decoder_format(*provider_, type);
    return result.present ? std::optional(NetEqDecoderFormatFromFfi(result.value)) : std::nullopt;
  }
  std::optional<DecoderFormat> GetCurrentDecoderFormat() const override {
    auto result = neteq_provider_current_decoder_format(*provider_);
    return result.present ? std::optional(NetEqDecoderFormatFromFfi(result.value)) : std::nullopt;
  }
  void FlushBuffers() override { neteq_provider_flush_buffers(*provider_); }
  void EnableNack(size_t count) override { neteq_provider_enable_nack(*provider_, count); }
  void DisableNack() override { neteq_provider_disable_nack(*provider_); }
  std::vector<uint16_t> GetNackList(int64_t rtt) const override {
    auto values = neteq_provider_nack_list(*provider_, rtt);
    return {values.begin(), values.end()};
  }
  int SyncBufferSizeMs() const override { return neteq_provider_sync_buffer_size(*provider_); }
 private:
  // Destruction drops the caller before releasing the environment it can use.
  const webrtc::Environment environment_;
  rust::Box<RustNetEqProvider> provider_;
};
}  // namespace

NativeNetEq::NativeNetEq(std::unique_ptr<webrtc::NetEq> value, bool stock, bool failed,
    std::function<bool()> failure_query) noexcept
    : value_(std::move(value)), stock_(stock), failed_(failed), failure_query_(std::move(failure_query)) {}
std::unique_ptr<NativeNetEq> neteq_provider_create(const NativeEnvironment& environment,
    rust::Box<RustNetEqProvider> provider) noexcept {
  auto product = std::make_unique<CallerNetEq>(environment.environment(), std::move(provider));
  auto* source = product.get();
  return std::make_unique<NativeNetEq>(std::move(product), false, false, [source] { return source->failed(); });
}
std::unique_ptr<NativeNetEq> neteq_failed_create(int32_t configured_rate) noexcept {
  return std::make_unique<NativeNetEq>(std::make_unique<FailedNetEq>(configured_rate), false, true);
}
// SDK mixer rate selection happens before GetAudio's failure is observed.
// Keep this domain separate from the standalone failed product. No engine,
// decoder or clock is substituted or sampled by failure construction.
std::unique_ptr<NativeNetEq> neteq_failed_create_for_sdk(int32_t configured_rate) noexcept {
  const auto rate = configured_rate >= 8000 && configured_rate <= 48000 ? configured_rate : 8000;
  return neteq_failed_create(rate);
}
bool neteq_is_failed(const NativeNetEq& value) noexcept { return value.is_failed(); }
bool neteq_is_stock(const NativeNetEq& value) noexcept { return value.is_stock(); }
NativeNetEq::~NativeNetEq() = default;
webrtc::NetEq& NativeNetEq::value() noexcept { return *value_; }
const webrtc::NetEq& NativeNetEq::value() const noexcept { return *value_; }
bool NativeNetEq::is_stock() const noexcept { return stock_; }
bool NativeNetEq::is_failed() const noexcept { return failed_ || (failure_query_ && failure_query_()); }
std::unique_ptr<webrtc::NetEq> NativeNetEq::Release() noexcept { return std::move(value_); }

namespace {
// SDK ownership is represented here, but peer installation is deliberately not
// exported until the complete consumer admission contract has been proved.
class CallerNetEqFactory final : public webrtc::NetEqFactory {
 public:
  CallerNetEqFactory(rust::Box<RustNetEqFactory> source,
      const NativeEnvironment& selected_environment,
      const NativeAudioDecoderFactory* selected_decoder_factory,
      bool sdk_consumer)
      : source_(std::move(source)),
        environment_(clone_environment(selected_environment)),
        decoder_factory_(selected_decoder_factory
            ? wrap_audio_decoder_factory(selected_decoder_factory->factory(), selected_decoder_factory)
            : nullptr),
        sdk_consumer_(sdk_consumer) {}
  FfiNetEqFactoryCreation Dispatch(const webrtc::Environment& environment,
      const webrtc::NetEq::Config& config,
      webrtc::scoped_refptr<webrtc::AudioDecoderFactory> decoder_factory) const {
    return neteq_factory_create_product(*source_,
        retain_environment_from_sdk(environment, environment_.get()),
        NetEqConfigToFfi(config),
        wrap_audio_decoder_factory(std::move(decoder_factory), decoder_factory_.get()),
        sdk_consumer_);
  }
  std::unique_ptr<webrtc::NetEq> Create(const webrtc::Environment& environment,
      const webrtc::NetEq::Config& config,
      webrtc::scoped_refptr<webrtc::AudioDecoderFactory> decoder_factory) const override {
    auto result = Dispatch(environment, config, std::move(decoder_factory));
    // Error and panic already record a durable failure and construct a failed
    // non-null, non-stock product. Never replace a supplied factory with stock.
    return result.product->Release();
  }
 private:
  rust::Box<RustNetEqFactory> source_;
  std::unique_ptr<NativeEnvironment> environment_;
  std::unique_ptr<NativeAudioDecoderFactory> decoder_factory_;
  const bool sdk_consumer_;
};
}  // namespace

FfiNetEqFactoryCreation neteq_factory_create(rust::Box<RustNetEqFactory> source,
    const NativeEnvironment& environment, const FfiNetEqConfig& config,
    const NativeAudioDecoderFactory* decoder_factory) noexcept {
  CallerNetEqFactory factory(std::move(source), environment, decoder_factory, false);
  return factory.Dispatch(environment.environment(), NetEqConfigFromFfi(config),
      decoder_factory ? decoder_factory->factory() : nullptr);
}
bool neteq_factory_failed_product_contract(rust::Box<RustNetEqFactory> source,
    const NativeEnvironment& environment, const NativeAudioDecoderFactory& decoder_factory) noexcept {
  CallerNetEqFactory factory(std::move(source), environment, &decoder_factory, true);
  webrtc::NetEq::Config config;
  config.sample_rate_hz = 384000;  // Deliberately not a mixer-safe failure rate.
  auto product = factory.Create(environment.environment(), config, decoder_factory.factory());
  if (!product || product->last_output_sample_rate_hz() != 8000 ||
      product->GetCurrentDecoderFormat() || product->GetPlayoutTimestamp()) return false;
  webrtc::AudioFrame frame;
  bool muted = true;
  int rate = -71;
  if (product->GetAudio(&frame, &muted, &rate, std::nullopt) != webrtc::NetEq::kFail ||
      !muted || rate != -71) return false;
  webrtc::RTPHeader header;
  const std::uint8_t byte = 0;
  return product->InsertPacket(header, {&byte, 1}) == webrtc::NetEq::kFail &&
      product->FilteredCurrentDelayMs() == 0 && product->TargetDelayMs() == 0;
}
std::int32_t neteq_insert_packet(NativeNetEq& value, const FfiMediaRtpHeader& header,
    rust::Slice<const std::uint8_t> payload) noexcept {
  webrtc::RTPHeader native;
  if (!MediaRtpHeaderFromFfi(header, native)) return webrtc::NetEq::kFail;
  return value.value().InsertPacket(native, std::span<const std::uint8_t>(payload.data(), payload.size()));
}
std::int32_t neteq_insert_packet_at(NativeNetEq& value, const FfiMediaRtpHeader& header,
    rust::Slice<const std::uint8_t> payload, std::int64_t receive_time_us) noexcept {
  webrtc::RTPHeader native;
  if (!MediaRtpHeaderFromFfi(header, native)) return webrtc::NetEq::kFail;
  const auto receive_time = receive_time_us == std::numeric_limits<std::int64_t>::min()
      ? webrtc::Timestamp::MinusInfinity() : receive_time_us == std::numeric_limits<std::int64_t>::max()
      ? webrtc::Timestamp::PlusInfinity() : webrtc::Timestamp::Micros(receive_time_us);
  return value.value().InsertPacket(native, std::span<const std::uint8_t>(payload.data(), payload.size()), receive_time);
}
std::int32_t neteq_insert_packet_info(NativeNetEq& value, const FfiMediaRtpHeader& header,
    rust::Slice<const std::uint8_t> payload, const FfiRtpReceivedPacketInfo& info) noexcept {
  webrtc::RTPHeader native;
  if (!MediaRtpHeaderFromFfi(header, native)) return webrtc::NetEq::kFail;
  return value.value().InsertPacket(native, std::span<const std::uint8_t>(payload.data(), payload.size()), MediaPacketInfoFromFfi(info));
}
FfiNetEqAudioResult neteq_get_audio(NativeNetEq& value, NativeAudioFrame& frame,
    bool want_muted, bool muted, bool want_rate, std::int32_t rate,
    bool has_override, std::int32_t operation) noexcept {
  const auto override = has_override
      ? std::optional(static_cast<webrtc::NetEq::Operation>(operation)) : std::nullopt;
  const OwnedNetEqFrameScope initialization(frame.value());
  const auto code = value.value().GetAudio(&frame.value(), want_muted ? &muted : nullptr,
      want_rate ? &rate : nullptr, override);
  return {code, muted, rate};
}
bool neteq_set_codecs(NativeNetEq& value, rust::Slice<const FfiNetEqCodec> codecs) noexcept {
  std::map<int, webrtc::SdpAudioFormat> native;
  for (const auto& codec : codecs) {
    auto format = AudioFormatFromFfi(codec.format);
    if (value.is_stock() && !StockNetEqCodecSafe(codec.payload_type, format)) return false;
    if (!native.emplace(codec.payload_type, std::move(format)).second) return false;
  }
  value.value().SetCodecs(native);
  return true;
}
bool neteq_register_payload_type(NativeNetEq& value, std::int32_t payload_type, const FfiSdpAudioFormat& format) noexcept {
  return value.value().RegisterPayloadType(payload_type, AudioFormatFromFfi(format));
}
bool neteq_create_decoder(NativeNetEq& value, std::int32_t payload_type) noexcept { return value.value().CreateDecoder(payload_type); }
std::int32_t neteq_remove_payload_type(NativeNetEq& value, std::uint8_t payload_type) noexcept { return value.value().RemovePayloadType(payload_type); }
bool neteq_remove_all_payload_types(NativeNetEq& value) noexcept {
  // Stock RemoveAll does not discard queued frames with raw decoder pointers.
  // Refuse, rather than silently flushing audio or changing native timestamps.
  if (value.is_stock() && StockNetEqHasPackets(value.value())) return false;
  value.value().RemoveAllPayloadTypes();
  return true;
}
bool neteq_set_maximum_buffer_packets(NativeNetEq& value, std::size_t count) noexcept {
  if (value.is_stock() && !neteq_stock_buffer_packets_safe(count)) return false;
  value.value().SetMaximumBufferPackets(count);
  return true;
}
void neteq_set_fast_accelerate(NativeNetEq& value, bool enable) noexcept { value.value().SetFastAccelerate(enable); }
bool neteq_set_minimum_delay(NativeNetEq& value, std::int32_t delay_ms) noexcept { return value.value().SetMinimumDelay(delay_ms); }
bool neteq_set_maximum_delay(NativeNetEq& value, std::int32_t delay_ms) noexcept { return value.value().SetMaximumDelay(delay_ms); }
bool neteq_set_base_minimum_delay(NativeNetEq& value, std::int32_t delay_ms) noexcept { return value.value().SetBaseMinimumDelayMs(delay_ms); }
std::int32_t neteq_base_minimum_delay(const NativeNetEq& value) noexcept { return value.value().GetBaseMinimumDelayMs(); }
std::int32_t neteq_target_delay(const NativeNetEq& value) noexcept { return value.value().TargetDelayMs(); }
std::int32_t neteq_filtered_current_delay(const NativeNetEq& value) noexcept { return value.value().FilteredCurrentDelayMs(); }
std::int32_t neteq_last_output_sample_rate(const NativeNetEq& value) noexcept { return value.value().last_output_sample_rate_hz(); }
void neteq_flush_buffers(NativeNetEq& value) noexcept { value.value().FlushBuffers(); }
std::int32_t neteq_sync_buffer_size(const NativeNetEq& value) noexcept { return value.value().SyncBufferSizeMs(); }
void neteq_enable_nack(NativeNetEq& value, std::size_t max_list_size) noexcept { value.value().EnableNack(max_list_size); }
void neteq_disable_nack(NativeNetEq& value) noexcept { value.value().DisableNack(); }

FfiNetEqNetworkStatisticsResult neteq_network_statistics(NativeNetEq& value) noexcept {
  webrtc::NetEqNetworkStatistics native{};
  const auto code = value.value().NetworkStatistics(&native);
  return {code, NetEqNetworkStatisticsToFfi(native)};
}
FfiNetEqNetworkStatistics neteq_current_network_statistics(const NativeNetEq& value) noexcept {
  return NetEqNetworkStatisticsToFfi(value.value().CurrentNetworkStatistics());
}
FfiNetEqLifetimeStatistics neteq_lifetime_statistics(const NativeNetEq& value) noexcept {
  return NetEqLifetimeStatisticsToFfi(value.value().GetLifetimeStatistics());
}
FfiNetEqOperationsAndState neteq_operations_and_state(const NativeNetEq& value) noexcept {
  return NetEqOperationsAndStateToFfi(value.value().GetOperationsAndState());
}
std::uint32_t neteq_playout_timestamp(const NativeNetEq& value, bool& present) noexcept {
  const auto result = value.value().GetPlayoutTimestamp();
  present = result.has_value();
  return result.value_or(0);
}
FfiNetEqDecoderFormatResult neteq_current_decoder_format(NativeNetEq& value) noexcept {
  const auto result = value.value().GetCurrentDecoderFormat();
  return result ? FfiNetEqDecoderFormatResult{true, NetEqDecoderFormatToFfi(*result)} : FfiNetEqDecoderFormatResult{false, {}};
}
FfiNetEqDecoderFormatResult neteq_decoder_format(const NativeNetEq& value, std::int32_t payload_type) noexcept {
  const auto result = value.value().GetDecoderFormat(payload_type);
  return result ? FfiNetEqDecoderFormatResult{true, NetEqDecoderFormatToFfi(*result)} : FfiNetEqDecoderFormatResult{false, {}};
}
void neteq_insert_empty_packet(NativeNetEq& value, const FfiMediaRtpHeader& header) noexcept {
  webrtc::RTPHeader native;
  if (MediaRtpHeaderFromFfi(header, native)) value.value().InsertEmptyPacket(native);
}
rust::Vec<std::uint16_t> neteq_nack_list(const NativeNetEq& value, std::int64_t round_trip_time_ms) noexcept {
  const auto native = value.value().GetNackList(round_trip_time_ms);
  rust::Vec<std::uint16_t> result;
  result.reserve(native.size());
  for (auto sequence : native) result.push_back(sequence);
  return result;
}

struct CodecDecoderObservations {
  void Hit(std::uint64_t FfiDecoderStatistics::*field) {
    std::lock_guard lock(mutex);
    ObserveLocked();
    ++(value.*field);
  }
  void Observe() {
    std::lock_guard lock(mutex);
    ObserveLocked();
  }
  void Input(const std::uint8_t* data, std::size_t size) {
    std::lock_guard lock(mutex);
    ObserveLocked();
    ++value.input_packets;
    std::uint64_t hash = 14695981039346656037ULL;
    for (std::size_t i = 0; i < size; ++i) hash = (hash ^ data[i]) * 1099511628211ULL;
    value.last_input_hash = hash;
  }
  FfiDecoderStatistics Snapshot() const {
    std::lock_guard lock(mutex);
    return value;
  }
 private:
  void ObserveLocked() {
    const auto current = std::this_thread::get_id();
    if (!thread) {
      thread = current;
      value.thread_token = native_codec_thread_token();
    } else if (*thread != current) {
      ++value.thread_mismatches;
    }
  }
  mutable std::mutex mutex;
  std::optional<std::thread::id> thread;
  FfiDecoderStatistics value{};
};

namespace {
class ObservedVp8Decoder final : public webrtc::VideoDecoder {
 public:
  ObservedVp8Decoder(std::unique_ptr<webrtc::VideoDecoder> decoder,
                    std::shared_ptr<CodecDecoderObservations> observations)
      : callback_(observations), decoder_(std::move(decoder)),
        observations_(std::move(observations)) {}
  bool Configure(const Settings& settings) override {
    observations_->Hit(&FfiDecoderStatistics::configure_calls);
    Settings serial = settings;
    serial.set_number_of_cores(1);
    const bool result = decoder_->Configure(serial);
    if (!result) observations_->Hit(&FfiDecoderStatistics::decode_errors);
    return result;
  }
  std::int32_t Decode(const webrtc::EncodedImage& image,
                      std::int64_t render_time_ms) override {
    observations_->Hit(&FfiDecoderStatistics::decode_calls);
    observations_->Input(image.data(), image.size());
    const auto result = decoder_->Decode(image, render_time_ms);
    if (result < 0) observations_->Hit(&FfiDecoderStatistics::decode_errors);
    return result;
  }
  std::int32_t Decode(const webrtc::EncodedImage& image, bool missing,
                      std::int64_t render_time_ms) override {
    observations_->Hit(&FfiDecoderStatistics::decode_calls);
    observations_->Input(image.data(), image.size());
    const auto result = decoder_->Decode(image, missing, render_time_ms);
    if (result < 0) observations_->Hit(&FfiDecoderStatistics::decode_errors);
    return result;
  }
  std::int32_t RegisterDecodeCompleteCallback(
      webrtc::DecodedImageCallback* callback) override {
    observations_->Observe();
    callback_.target = callback;
    return decoder_->RegisterDecodeCompleteCallback(callback ? &callback_ : nullptr);
  }
  std::int32_t Release() override {
    observations_->Observe();
    decoder_->RegisterDecodeCompleteCallback(nullptr);
    callback_.target = nullptr;
    return decoder_->Release();
  }
  DecoderInfo GetDecoderInfo() const override { return decoder_->GetDecoderInfo(); }
  const char* ImplementationName() const override { return decoder_->ImplementationName(); }
 private:
  class Callback final : public webrtc::DecodedImageCallback {
   public:
    explicit Callback(std::shared_ptr<CodecDecoderObservations> observations)
        : observations_(std::move(observations)) {}
    std::int32_t Decoded(webrtc::VideoFrame& frame) override {
      observations_->Hit(&FfiDecoderStatistics::decoded_outputs);
      if (!target) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
      const auto result = target->Decoded(frame);
      if (result < 0) observations_->Hit(&FfiDecoderStatistics::decode_errors);
      return result;
    }
    std::int32_t Decoded(webrtc::VideoFrame& frame, std::int64_t time_ms) override {
      observations_->Hit(&FfiDecoderStatistics::decoded_outputs);
      if (!target) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
      const auto result = target->Decoded(frame, time_ms);
      if (result < 0) observations_->Hit(&FfiDecoderStatistics::decode_errors);
      return result;
    }
    void Decoded(webrtc::VideoFrame& frame, std::optional<std::int32_t> time_ms,
                 std::optional<std::uint8_t> qp) override {
      observations_->Hit(&FfiDecoderStatistics::decoded_outputs);
      if (target) target->Decoded(frame, time_ms, qp);
    }
    webrtc::DecodedImageCallback* target = nullptr;
   private:
    std::shared_ptr<CodecDecoderObservations> observations_;
  };
  // Keep the callback proxy alive while the underlying decoder is destroyed.
  Callback callback_;
  std::unique_ptr<webrtc::VideoDecoder> decoder_;
  std::shared_ptr<CodecDecoderObservations> observations_;
};

class ObservedVp8Factory final : public webrtc::VideoDecoderFactory {
 public:
  explicit ObservedVp8Factory(std::shared_ptr<CodecDecoderObservations> observations)
      : observations_(std::move(observations)) {}
  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return {webrtc::SdpVideoFormat("VP8")};
  }
  CodecSupport QueryCodecSupport(const webrtc::SdpVideoFormat& format,
      bool reference_scaling, std::optional<webrtc::Resolution>) const override {
    return {absl::EqualsIgnoreCase(format.name, "VP8") && !reference_scaling, false};
  }
  std::unique_ptr<webrtc::VideoDecoder> Create(const webrtc::Environment& env,
      const webrtc::SdpVideoFormat& format) override {
    if (!absl::EqualsIgnoreCase(format.name, "VP8")) return nullptr;
    observations_->Hit(&FfiDecoderStatistics::decoder_creations);
    return std::make_unique<ObservedVp8Decoder>(
        webrtc::CreateVp8Decoder(env), observations_);
  }
 private:
  std::shared_ptr<CodecDecoderObservations> observations_;
};

class ObservedOpusDecoder final : public webrtc::AudioDecoder {
 public:
  ObservedOpusDecoder(std::unique_ptr<webrtc::AudioDecoder> decoder,
                     std::shared_ptr<CodecDecoderObservations> observations)
      : decoder_(std::move(decoder)), observations_(std::move(observations)) {}
  std::vector<ParseResult> ParsePayload(webrtc::Buffer&& payload,
                                        std::uint32_t timestamp) override {
    observations_->Observe();
    observations_->Input(payload.data(), payload.size());
    auto frames = decoder_->ParsePayload(std::move(payload), timestamp);
    for (auto& result : frames) {
      if (result.frame) result.frame = std::make_unique<Frame>(
          decoder_, std::move(result.frame), observations_);
    }
    return frames;
  }
  void Reset() override { observations_->Observe(); decoder_->Reset(); }
  int SampleRateHz() const override { return decoder_->SampleRateHz(); }
  std::size_t Channels() const override { return decoder_->Channels(); }
  int ErrorCode() override { return decoder_->ErrorCode(); }
  int PacketDuration(const std::uint8_t* data, std::size_t size) const override {
    return decoder_->PacketDuration(data, size);
  }
  int PacketDurationRedundant(const std::uint8_t* data, std::size_t size) const override {
    return decoder_->PacketDurationRedundant(data, size);
  }
  bool PacketHasFec(const std::uint8_t* data, std::size_t size) const override {
    return decoder_->PacketHasFec(data, size);
  }
  bool HasDecodePlc() const override { return decoder_->HasDecodePlc(); }
  std::size_t DecodePlc(std::size_t frames, std::int16_t* decoded) override {
    observations_->Observe();
    return decoder_->DecodePlc(frames, decoded);
  }
  void GeneratePlc(std::size_t samples, webrtc::BufferT<std::int16_t>* output) override {
    observations_->Observe(); decoder_->GeneratePlc(samples, output);
  }
 protected:
  int DecodeInternal(const std::uint8_t* data, std::size_t size, int rate,
                     std::int16_t* decoded, SpeechType* speech) override {
    observations_->Hit(&FfiDecoderStatistics::decode_calls);
    // The wrapper's nonvirtual public Decode has already checked capacity
    // against the same delegated PacketDuration/Channels before reaching here.
    observations_->Input(data, size);
    const int result = decoder_->Decode(data, size, rate,
        std::numeric_limits<std::size_t>::max(), decoded, speech);
    CountResult(result);
    return result;
  }
  int DecodeRedundantInternal(const std::uint8_t* data, std::size_t size, int rate,
                     std::int16_t* decoded, SpeechType* speech) override {
    observations_->Hit(&FfiDecoderStatistics::decode_calls);
    observations_->Input(data, size);
    const int result = decoder_->DecodeRedundant(data, size, rate,
        std::numeric_limits<std::size_t>::max(), decoded, speech);
    CountResult(result);
    return result;
  }
 private:
  class Frame final : public EncodedAudioFrame {
   public:
    Frame(std::shared_ptr<webrtc::AudioDecoder> decoder,
          std::unique_ptr<EncodedAudioFrame> frame,
          std::shared_ptr<CodecDecoderObservations> observations)
        : decoder_(std::move(decoder)), frame_(std::move(frame)),
          observations_(std::move(observations)) {}
    std::size_t Duration() const override { return frame_->Duration(); }
    bool IsDtxPacket() const override { return frame_->IsDtxPacket(); }
    std::optional<DecodeResult> Decode(std::span<std::int16_t> output) const override {
      observations_->Hit(&FfiDecoderStatistics::decode_calls);
      auto result = frame_->Decode(output);
      if (!result) observations_->Hit(&FfiDecoderStatistics::decode_errors);
      else if (result->num_decoded_samples != 0)
        observations_->Hit(&FfiDecoderStatistics::decoded_outputs);
      return result;
    }
   private:
    // Frames hold raw decoder pointers upstream. Destroy each frame before
    // releasing its decoder even if it outlives our wrapping decoder.
    std::shared_ptr<webrtc::AudioDecoder> decoder_;
    std::unique_ptr<EncodedAudioFrame> frame_;
    std::shared_ptr<CodecDecoderObservations> observations_;
  };
  void CountResult(int result) {
    if (result < 0) observations_->Hit(&FfiDecoderStatistics::decode_errors);
    else if (result > 0) observations_->Hit(&FfiDecoderStatistics::decoded_outputs);
  }
  std::shared_ptr<webrtc::AudioDecoder> decoder_;
  std::shared_ptr<CodecDecoderObservations> observations_;
};

class ObservedOpusFactory : public webrtc::AudioDecoderFactory {
 public:
  explicit ObservedOpusFactory(std::shared_ptr<CodecDecoderObservations> observations)
      : builtin_(webrtc::CreateBuiltinAudioDecoderFactory()),
        observations_(std::move(observations)) {}
  std::vector<webrtc::AudioCodecSpec> GetSupportedDecoders() override {
    auto formats = builtin_->GetSupportedDecoders();
    std::erase_if(formats, [](const auto& spec) {
      return !absl::EqualsIgnoreCase(spec.format.name, "opus");
    });
    return formats;
  }
  bool IsSupportedDecoder(const webrtc::SdpAudioFormat& format) override {
    return absl::EqualsIgnoreCase(format.name, "opus") && builtin_->IsSupportedDecoder(format);
  }
  std::unique_ptr<webrtc::AudioDecoder> Create(const webrtc::Environment& env,
      const webrtc::SdpAudioFormat& format) override {
    return Create(env, format, std::nullopt);
  }
  std::unique_ptr<webrtc::AudioDecoder> Create(const webrtc::Environment& env,
      const webrtc::SdpAudioFormat& format,
      std::optional<webrtc::AudioCodecPairId> pair_id) override {
    if (!IsSupportedDecoder(format)) return nullptr;
    auto decoder = builtin_->Create(env, format, pair_id);
    if (!decoder) return nullptr;
    observations_->Hit(&FfiDecoderStatistics::decoder_creations);
    return std::make_unique<ObservedOpusDecoder>(std::move(decoder), observations_);
  }
 private:
  webrtc::scoped_refptr<webrtc::AudioDecoderFactory> builtin_;
  std::shared_ptr<CodecDecoderObservations> observations_;
};
} // namespace

struct NativeVideoFrame::State {
  std::uint32_t width = 0, height = 0, rtp_timestamp = 0;
  std::int64_t timestamp_us = 0;
  std::uint16_t rotation = 0;
  std::vector<std::uint8_t> i420;
};
struct NativeEncodedVideoFrame::State {
  std::uint32_t width = 0, height = 0, rtp_timestamp = 0;
  bool key_frame = false;
  std::int32_t qp = -1;
  std::vector<std::uint8_t> data;
};
struct NativeEncodedImageCallback::State {
  std::mutex mutex;
  webrtc::EncodedImageCallback *callback = nullptr;
  bool active = false;
  bool engine = false;
  std::size_t configured_streams = 0;
  bool AdmitEncoded(const webrtc::EncodedImage& image, const webrtc::CodecSpecificInfo* info) const {
    if (!engine) return true;  // Caller-owned local callback has no packetizer.
    const auto stream = image.SimulcastIndex().value_or(0);
    if (!configured_streams || stream < 0 || static_cast<std::size_t>(stream) >= configured_streams) return false;
    if (image.SpatialIndex() && (*image.SpatialIndex() < 0 || *image.SpatialIndex() >= static_cast<int>(webrtc::kMaxSpatialLayers))) return false;
    if (!info) return true;
    if (info->codecType != codec_type) return false;
    if (info->codecType == webrtc::kVideoCodecVP9) {
      const auto& v = info->codecSpecific.VP9;
      if (v.num_ref_pics > std::size(v.p_diff)) return false;
      if (v.inter_pic_predicted && v.flexible_mode && v.num_ref_pics == 0) return false;
      if (v.ss_data_available) {
        if (v.num_spatial_layers == 0 || v.num_spatial_layers > std::size(v.width) ||
            v.gof.num_frames_in_gof > std::size(v.gof.temporal_idx)) return false;
        for (std::size_t i = 0; i < v.gof.num_frames_in_gof; ++i)
          if (v.gof.num_ref_pics[i] > std::size(v.gof.pid_diff[i])) return false;
      }
      const auto spatial = v.num_spatial_layers > 1 ? image.SpatialIndex().value_or(255) : 255;
      if (spatial >= static_cast<int>(v.num_spatial_layers) - 1 && !info->end_of_picture) return false;
    }
    return true;
  }
  webrtc::VideoCodecType codec_type = webrtc::kVideoCodecGeneric;
  webrtc::H264PacketizationMode h264_packetization_mode =
      webrtc::H264PacketizationMode::NonInterleaved;
  std::condition_variable idle;
  std::size_t in_flight = 0;
  void Retire() {
    std::unique_lock lock(mutex); active = false;
    idle.wait(lock, [&] { return in_flight == 0; });
    callback = nullptr;
  }
  template <typename Operation>
  std::optional<webrtc::EncodedImageCallback::Result> Invoke(Operation operation,
      const webrtc::EncodedImage* image = nullptr, const webrtc::CodecSpecificInfo* info = nullptr) {
    webrtc::EncodedImageCallback* target;
    {
      std::lock_guard lock(mutex);
      if (!active || !callback || (image && !AdmitEncoded(*image, info))) return std::nullopt;
      ++in_flight; target = callback;
    }
    struct Exit {
      State& state;
      ~Exit() { std::lock_guard lock(state.mutex); if (--state.in_flight == 0) state.idle.notify_all(); }
    } exit{*this};
    return operation(*target);
  }
};
struct NativeDecodedImageCallback::State {
  std::shared_ptr<NativeEnvironment> engine_environment;
  bool AdmitTime(std::uint8_t overload, std::int64_t milliseconds, bool present, std::int32_t optional_ms) const {
    if (!engine_environment) return true;  // Local caller callbacks have no VCM arithmetic.
    const auto bounds = environment_clock_time_bounds(*engine_environment);
    if (!bounds.known || !bounds.monotonic || bounds.minimum_us == INT64_MIN || bounds.maximum_us == INT64_MAX) return false;
    const auto now = engine_environment->environment().clock().CurrentTime();
    if (!now.IsFinite() || now.us() < bounds.minimum_us || now.us() > bounds.maximum_us) return false;
    if (overload == 1) {
      present = milliseconds >= 0;
      optional_ms = static_cast<std::int32_t>(milliseconds); // Preserve native narrowing.
    }
    if (!present || overload == 0)
      return static_cast<__int128>(bounds.maximum_us) - bounds.minimum_us < INT64_MAX;
    const auto delta = static_cast<std::int64_t>(optional_ms) * 1000;
    // The exact retained monotonic clock proves every native decode_start is
    // between its promised minimum and this sampled time, even for late output.
    return static_cast<__int128>(bounds.minimum_us) + delta > INT64_MIN &&
        static_cast<__int128>(now.us()) + delta < INT64_MAX;
  }
  std::mutex mutex;
  std::condition_variable idle;
  webrtc::DecodedImageCallback *callback = nullptr;
  bool active = false;
  std::size_t in_flight = 0;
  void Retire() {
    std::unique_lock lock(mutex);
    active = false;
    idle.wait(lock, [&] { return in_flight == 0; });
    callback = nullptr;
  }
  template <typename Operation>
  FfiDecodedCallbackResult Invoke(Operation operation) {
    webrtc::DecodedImageCallback* target;
    {
      std::lock_guard lock(mutex);
      if (!active || !callback) return {false, WEBRTC_VIDEO_CODEC_UNINITIALIZED};
      ++in_flight; target = callback;
    }
    struct Exit {
      State& state;
      ~Exit() { std::lock_guard lock(state.mutex); if (--state.in_flight == 0) state.idle.notify_all(); }
    } exit{*this};
    return {true, operation(*target)};
  }
};
struct NativeAudioEncoderFactory::State {
  bool selected_builtin = false;  // Proven at creation, never inferred from SDP.
  webrtc::scoped_refptr<webrtc::AudioEncoderFactory> factory;
  webrtc::scoped_refptr<webrtc::FrameTransformerInterface> opus_transformer;
  std::shared_ptr<std::atomic<std::uint64_t>> handoff_failures;
};
struct NativeAudioDecoderFactory::State {
  webrtc::scoped_refptr<webrtc::AudioDecoderFactory> factory;
  std::shared_ptr<CodecDecoderObservations> observations;
  // Local selection evidence, never inferred from advertised SDP or RTTI.
  // 0: unproven SDK reference, 1: selected stock, 2: selected observed Opus.
  std::uint8_t builtin_kind = 0;
};

namespace {

std::string AudioString(const rust::Vec<std::uint8_t>& bytes) {
  if (bytes.empty()) return {};
  return std::string(reinterpret_cast<const char*>(bytes.data()), bytes.size());
}
rust::Vec<std::uint8_t> AudioBytes(const std::string& value) {
  rust::Vec<std::uint8_t> result;
  result.reserve(value.size());
  for (const unsigned char byte : value) result.push_back(byte);
  return result;
}
webrtc::SdpAudioFormat ToNative(const FfiSdpAudioFormat& format) {
  webrtc::CodecParameterMap parameters;
  for (const auto& entry : format.parameters) {
    parameters.emplace(AudioString(entry.key), AudioString(entry.value));
  }
  return webrtc::SdpAudioFormat(AudioString(format.name), format.clockrate_hz,
                              format.num_channels, std::move(parameters));
}
FfiSdpAudioFormat FromNative(const webrtc::SdpAudioFormat& format) {
  FfiSdpAudioFormat result;
  result.name = AudioBytes(format.name);
  result.clockrate_hz = format.clockrate_hz;
  result.num_channels = format.num_channels;
  for (const auto& [key, value] : format.parameters) {
    result.parameters.push_back(FfiAudioCodecParameter{AudioBytes(key), AudioBytes(value)});
  }
  return result;
}
FfiAudioCodecInfo FromNative(const webrtc::AudioCodecInfo& info) {
  return FfiAudioCodecInfo{info.sample_rate_hz, info.num_channels,
      info.default_bitrate_bps, info.min_bitrate_bps, info.max_bitrate_bps,
      info.allow_comfort_noise, info.supports_network_adaption};
}
webrtc::AudioCodecInfo AudioInfo(const FfiAudioCodecInfo& info) {
  // Serialization does not consume codec capabilities. Start with a valid
  // constructor, then preserve even externally observed invalid metadata.
  webrtc::AudioCodecInfo result(8000, 1, 0);
  result.sample_rate_hz = info.sample_rate_hz;
  result.num_channels = info.num_channels;
  result.default_bitrate_bps = info.default_bitrate_bps;
  result.min_bitrate_bps = info.min_bitrate_bps;
  result.max_bitrate_bps = info.max_bitrate_bps;
  result.allow_comfort_noise = info.allow_comfort_noise;
  result.supports_network_adaption = info.supports_network_adaption;
  return result;
}

webrtc::SdpVideoFormat ToNative(const FfiCodecFormat &format) {
  std::map<std::string, std::string> parameters;
  for (const auto &parameter : format.parameters) {
    parameters.emplace(std::string(parameter.key),
                       std::string(parameter.value));
  }
  absl::InlinedVector<webrtc::ScalabilityMode, webrtc::kScalabilityModeCount> modes;
  for (const auto& name : format.scalability_modes) {
    const auto mode = webrtc::ScalabilityModeStringToEnum(std::string(name));
    // Rust's public mode wrapper is constructed only through the native parser
    // or from native enum serialization; invalid names cannot enter this path.
    RTC_CHECK(mode.has_value());
    modes.push_back(*mode);
  }
  return webrtc::SdpVideoFormat(std::string(format.name), parameters, modes);
}

FfiCodecFormat FromNative(const webrtc::SdpVideoFormat &format) {
  FfiCodecFormat result;
  result.name = format.name;
  for (const auto mode : format.scalability_modes) {
    result.scalability_modes.push_back(
        std::string(webrtc::ScalabilityModeToString(mode)));
  }
  result.parameters.reserve(format.parameters.size());
  for (const auto &[key, value] : format.parameters) {
    result.parameters.push_back(FfiCodecParameter{key, value});
  }
  return result;
}

std::optional<webrtc::Resolution> Resolution(bool present, std::uint32_t width,
                                             std::uint32_t height) {
  if (!present || width == 0 || height == 0) {
    return std::nullopt;
  }
  return webrtc::Resolution{static_cast<int>(width), static_cast<int>(height)};
}

rust::Vec<std::uint8_t> ToRust(const std::vector<std::uint8_t> &data) {
  // Copy in Rust so a frame does not make per-byte vector ABI calls.
  return copy_native_bytes(
      rust::Slice<const std::uint8_t>(data.data(), data.size()));
}

std::vector<std::uint8_t> CopyI420(const webrtc::VideoFrame &frame) {
  auto buffer = frame.video_frame_buffer()->ToI420();
  const int width = buffer->width();
  const int height = buffer->height();
  const int chroma_width = (width + 1) / 2;
  const int chroma_height = (height + 1) / 2;
  std::vector<std::uint8_t> result;
  result.reserve(static_cast<std::size_t>(width) * height +
                 2 * static_cast<std::size_t>(chroma_width) * chroma_height);
  auto append_plane = [&](const std::uint8_t *source, int stride, int row_size,
                          int rows) {
    for (int row = 0; row < rows; ++row) {
      result.insert(result.end(), source + row * stride,
                    source + row * stride + row_size);
    }
  };
  append_plane(buffer->DataY(), buffer->StrideY(), width, height);
  append_plane(buffer->DataU(), buffer->StrideU(), chroma_width, chroma_height);
  append_plane(buffer->DataV(), buffer->StrideV(), chroma_width, chroma_height);
  return result;
}

using VideoCreationRetention = std::shared_ptr<rust::Box<RustVideoFactoryEnvironment>>;
std::unique_ptr<rust::Box<RustVideoFactoryEnvironment>> CreationRetention(
    const VideoCreationRetention &source, const webrtc::Environment &environment) {
  return std::make_unique<rust::Box<RustVideoFactoryEnvironment>>(
      source ? video_factory_environment_clone(**source)
             : video_factory_environment_from_sdk(retain_environment_from_sdk(environment)));
}
// One actual SDK simulcast product's info operation evidence. It retains no
// codec or Rust userdata, and is not a cache of caller capability values.
struct EncoderInfoInvocation {
  std::atomic<bool> failed{false};
  bool installed = false;
  bool leaf_multistream_initialized = false;
  bool adapter_initialized = false;
  void Fail() { failed.store(true, std::memory_order_release); }
  // Accessed only by the admitted synchronous SDK call and its leaf queries.
  bool aggregating = false;
  std::uint32_t alignment = 1;
  std::uint32_t trial_alignment = 1;
  void Begin() { failed.store(false, std::memory_order_release); aggregating = true; alignment = trial_alignment; }
  void End() { aggregating = false; }
  bool AdmitAlignment(std::uint32_t next, bool captured_for_context) {
    if (!aggregating && !captured_for_context) return true;
    const auto prior = aggregating ? alignment : trial_alignment;
    const std::uint64_t product = prior == 0 || next == 0 ? 0 : static_cast<std::uint64_t>(prior / std::gcd(prior, next)) * next;
    if (product > std::numeric_limits<std::uint32_t>::max()) return false;
    if (aggregating) alignment = static_cast<std::uint32_t>(product);
    return true;
  }
  bool Succeeded() const { return !failed.load(std::memory_order_acquire); }
};
class RustEncoder final : public webrtc::VideoEncoder {
  class Call final {
   public:
    explicit Call(const RustEncoder& owner, bool allow_released = false) : owner_(owner) {
      std::lock_guard lock(owner_.mutex_);
      if (owner_.call_active_ || (!allow_released && owner_.released_)) return;
      owner_.call_active_ = true; entered_ = true;
    }
    ~Call() { if (entered_) { std::lock_guard lock(owner_.mutex_); owner_.call_active_ = false; } }
    explicit operator bool() const { return entered_; }
    Call(const Call&) = delete; Call& operator=(const Call&) = delete;
   private:
    const RustEncoder& owner_;
    bool entered_ = false;
  };
public:
  RustEncoder(rust::Box<RustVideoEncoder> encoder,
              const webrtc::SdpVideoFormat &format,
              std::unique_ptr<rust::Box<RustVideoFactoryEnvironment>> retention,
              rust::Box<RustVideoEncoderFactory> factory,
              std::shared_ptr<EncoderInfoInvocation> info_invocation) noexcept
      : retention_(std::move(retention)), factory_(std::move(factory)),
        info_invocation_(std::move(info_invocation)), encoder_(std::move(encoder)),
        callback_(std::make_shared<NativeEncodedImageCallback>(
            std::make_shared<NativeEncodedImageCallback::State>())),
        failed_(!encoder_is_valid(*encoder_)) {
    callback_->state()->engine = info_invocation_->installed;
    if (format.name == "H264") callback_->state()->codec_type = webrtc::kVideoCodecH264;
    else if (format.name == "VP8") callback_->state()->codec_type = webrtc::kVideoCodecVP8;
    else if (format.name == "VP9") callback_->state()->codec_type = webrtc::kVideoCodecVP9;
    else if (format.name == "AV1") callback_->state()->codec_type = webrtc::kVideoCodecAV1;
    else if (format.name == "H265") callback_->state()->codec_type = webrtc::kVideoCodecH265;
    const auto packetization_mode =
        format.parameters.find("packetization-mode");
    if (packetization_mode != format.parameters.end() &&
        packetization_mode->second == "0") {
      callback_->state()->h264_packetization_mode =
          webrtc::H264PacketizationMode::SingleNalUnit;
    }
  }
  ~RustEncoder() override { if (!released_) Release(); }
  void SetFecControllerOverride(webrtc::FecControllerOverride* target) override {
    Call call(*this);
    if (!call) { encoder_factory_report_feedback_failure(*factory_); return; }
    if (fec_override_) RetireFecOverride(*fec_override_);
    fec_override_ = BorrowFecOverride(target);
    if (encoder_set_fec_override(*encoder_, fec_override_) != WEBRTC_VIDEO_CODEC_OK) encoder_factory_report_feedback_failure(*factory_);
  }

  int32_t InitEncode(const webrtc::VideoCodec* codec, std::int32_t cores, std::size_t payload) override {
    return InitEncode(codec, Settings(Capabilities(false), cores, payload));
  }
  int InitEncode(const webrtc::VideoCodec *codec, const Settings &settings) override {
    Call call(*this, true); if (!call || failed_) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    if (!codec) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    released_ = false;
    {
      auto& state = *callback_->state();
      std::lock_guard lock(state.mutex);
      if (state.in_flight) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
      state.configured_streams = std::max<unsigned>(codec->numberOfSimulcastStreams, 1);
    }
    FfiOwnedEncoderSettings value{settings.capabilities.loss_notification, settings.number_of_cores,
        settings.max_payload_size, settings.encoder_thread_limit.has_value(), settings.encoder_thread_limit.value_or(0)};
    const auto result = encoder_init_value(*encoder_, std::make_unique<NativeVideoCodec>(*codec), value);
    if (result >= 0 && codec->numberOfSimulcastStreams > 1) info_invocation_->leaf_multistream_initialized = true;
    return result;
  }

  int32_t RegisterEncodeCompleteCallback(
      webrtc::EncodedImageCallback *callback) override;

  int32_t Release() override;

  int32_t Encode(const webrtc::VideoFrame &frame,
      const std::vector<webrtc::VideoFrameType> *frame_types) override {
    Call call(*this); if (!call || failed_) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    rust::Vec<std::int32_t> types;
    if (frame_types) for (auto type : *frame_types) types.push_back(static_cast<std::int32_t>(type));
    return encoder_encode_value(*encoder_, std::make_unique<NativeRawVideoFrame>(frame),
        frame_types != nullptr, std::move(types), callback_);
  }
  void SetRates(const RateControlParameters &parameters) override {
    Call call(*this);
    if (!call || failed_ || encoder_set_rates_value(*encoder_, std::make_unique<NativeVideoRateControl>(parameters)) != WEBRTC_VIDEO_CODEC_OK)
      encoder_factory_report_feedback_failure(*factory_);
  }

  void OnPacketLossRateUpdate(float rate) override {
    Call call(*this);
    if (!call || encoder_packet_loss(*encoder_, rate) != WEBRTC_VIDEO_CODEC_OK) encoder_factory_report_feedback_failure(*factory_);
  }
  void OnRttUpdate(std::int64_t rtt) override {
    Call call(*this);
    if (!call || encoder_rtt(*encoder_, rtt) != WEBRTC_VIDEO_CODEC_OK) encoder_factory_report_feedback_failure(*factory_);
  }
  void OnLossNotification(const LossNotification& loss) override {
    Call call(*this);
    FfiVideoEncoderLossNotification value{loss.timestamp_of_last_decodable, loss.timestamp_of_last_received,
        loss.dependencies_of_last_received_decodable.has_value(), loss.dependencies_of_last_received_decodable.value_or(false),
        loss.last_received_decodable.has_value(), loss.last_received_decodable.value_or(false)};
    if (!call || encoder_loss_notification(*encoder_, value) != WEBRTC_VIDEO_CODEC_OK) encoder_factory_report_feedback_failure(*factory_);
  }
  EncoderInfo GetEncoderInfo() const override {
    Call call(*this, true);
    const auto info = call ? encoder_get_info_value(*encoder_) : nullptr;
    // In an active multi-stream query the SDK folds these exact results with
    // uint32 lcm. Reject only a reached nonrepresentable fold; the plain value
    // and independent getters continue to admit every uint32 alignment.
    // FetchOrCreateEncoderContext snapshots the first info result as both
    // PrimaryInfo and FallbackInfo (this factory has no fallback). Check its
    // later override fold before it enters that private SDK cache as well.
    const bool captured_for_context = call && first_info_;
    if (call) first_info_ = false;
    if (info && info_invocation_->AdmitAlignment(info->value().requested_resolution_alignment, captured_for_context)) return info->value();
    info_invocation_->Fail();
    encoder_factory_report_codec_info_failure(*factory_);
    // The native value-only ABI cannot return an error. Keep the failure
    // separate from its mandatory default value, never invent a caller name.
    return EncoderInfo{};
  }

private:
  mutable std::mutex mutex_;
  mutable bool call_active_ = false;
  mutable bool first_info_ = true;
  std::unique_ptr<rust::Box<RustVideoFactoryEnvironment>> retention_;
  rust::Box<RustVideoEncoderFactory> factory_;
  std::shared_ptr<EncoderInfoInvocation> info_invocation_;
  rust::Box<RustVideoEncoder> encoder_;
  std::shared_ptr<NativeEncodedImageCallback> callback_;
  std::shared_ptr<NativeFecOverride> fec_override_;
  bool released_ = false;
  const bool failed_;
};

class RustDecoder final : public webrtc::VideoDecoder {
  // Exclusive Rust codec borrows cannot overlap. Admission is synchronized,
  // but no bridge mutex spans caller code, downstream SDK or scheduling hooks.
  // Unsupported concurrent/reentrant operations fail rather than block/reenter.
  class Call final {
   public:
    explicit Call(const RustDecoder& owner, bool allow_released = false) : owner_(owner) {
      std::lock_guard lock(owner_.mutex_);
      if (owner_.call_active_ || (!allow_released && owner_.released_)) return;
      owner_.call_active_ = true; entered_ = true;
    }
    ~Call() { if (entered_) { std::lock_guard lock(owner_.mutex_); owner_.call_active_ = false; } }
    explicit operator bool() const { return entered_; }
    Call(const Call&) = delete;
    Call& operator=(const Call&) = delete;
   private:
    const RustDecoder& owner_;
    bool entered_ = false;
  };
public:
  RustDecoder(rust::Box<RustVideoDecoder> decoder,
              std::unique_ptr<rust::Box<RustVideoFactoryEnvironment>> retention,
              std::unique_ptr<NativeEnvironment> engine_environment) noexcept
      : retention_(std::move(retention)), decoder_(std::move(decoder)),
        callback_(std::make_shared<NativeDecodedImageCallback>(
            std::make_shared<NativeDecodedImageCallback::State>())) {
    callback_->state()->engine_environment = std::move(engine_environment);
  }
  ~RustDecoder() override { if (!released_) Release(); }

  bool Configure(const Settings &settings) override {
    Call call(*this, true); if (!call) return false;
    released_ = false;
    return decoder_configure_value(*decoder_, std::make_unique<NativeVideoDecoderSettings>(settings));
  }

  std::unique_ptr<NativeVideoDecoderInfo> InfoValue() const {
    Call call(*this, true); if (!call) return nullptr;
    auto info = decoder_get_info_value(*decoder_);
    info_valid_ = info != nullptr;
    return info;
  }
  bool InfoSucceeded() const { Call call(*this, true); return call && info_valid_; }
  int32_t RegisterDecodeCompleteCallback(
      webrtc::DecodedImageCallback *callback) override;
  int32_t Release() override;

  int32_t Decode(const webrtc::EncodedImage &image, int64_t render_time_ms) override {
    return DecodeValue(image, false, false, render_time_ms);
  }
  int32_t Decode(const webrtc::EncodedImage &image, bool missing, int64_t render_time_ms) override {
    return DecodeValue(image, true, missing, render_time_ms);
  }
  int32_t DecodeValue(const webrtc::EncodedImage& image, bool overload, bool missing, int64_t render_time_ms) {
    Call call(*this); if (!call) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    const auto buffer = image.GetEncodedData();
    if (image.size() && (!buffer || !image.data() || image.size() > buffer->size())) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    return decoder_decode_value(*decoder_, std::make_unique<NativeEncodedImage>(image), overload, missing, render_time_ms, callback_);
  }
  bool CallbackMatches(webrtc::DecodedImageCallback* callback) const {
    Call call(*this, true); if (!call) return false;
    const auto state = callback_->state();
    std::lock_guard slot_lock(state->mutex);
    return state->callback == callback && (callback == nullptr || state->active);
  }

  const char* ImplementationName() const override {
    Call call(*this, true); if (!call) return "";
    const auto bytes = decoder_implementation_name(*decoder_, name_valid_);
    name_.assign(bytes.begin(), bytes.end());
    return name_.c_str();
  }
  bool NameSucceeded() const {
    Call call(*this, true); return call && name_valid_;
  }
  DecoderInfo GetDecoderInfo() const override {
    const auto info = InfoValue();
    // The SDK value-returning ABI has no error arm. Public owning info drivers
    // preserve this failure as null; the engine receives no cached caller info.
    return info ? info->value() : DecoderInfo{};
  }

private:
  mutable std::mutex mutex_;
  std::unique_ptr<rust::Box<RustVideoFactoryEnvironment>> retention_;
  rust::Box<RustVideoDecoder> decoder_;
  mutable bool call_active_ = false;
  mutable std::string name_;
  mutable bool name_valid_ = false;
  mutable bool info_valid_ = false;
  std::shared_ptr<NativeDecodedImageCallback> callback_;
  bool released_ = false;
};

class RustPlainEncoderFactory final : public webrtc::VideoEncoderFactory {
public:
  explicit RustPlainEncoderFactory(rust::Box<RustVideoEncoderFactory> factory,
      VideoCreationRetention retention = nullptr,
      std::shared_ptr<EncoderInfoInvocation> info_invocation = std::make_shared<EncoderInfoInvocation>())
      : retention_(std::move(retention)), info_invocation_(std::move(info_invocation)), factory_(std::move(factory)) {}
  std::shared_ptr<RustPlainEncoderFactory> WithInfoInvocation(std::shared_ptr<EncoderInfoInvocation> invocation) const {
    return std::make_shared<RustPlainEncoderFactory>(encoder_factory_clone(*factory_), retention_, std::move(invocation));
  }
  std::unique_ptr<EncoderSelectorInterface> SelectorValue(bool& valid) const {
    auto selected = encoder_factory_selector(*factory_, valid);
    return selected ? selected->Take() : nullptr;
  }
  std::unique_ptr<EncoderSelectorInterface> GetEncoderSelector() const override { bool valid; return SelectorValue(valid); }
  std::unique_ptr<NativeSdpVideoFormatList> FormatsValue() const { return encoder_factory_formats(*factory_); }
  std::unique_ptr<NativeSdpVideoFormatList> ImplementationsValue() const { return encoder_factory_implementations(*factory_); }
  void ReportInfoFailure() const { encoder_factory_report_codec_info_failure(*factory_); }
  void ReportFeedbackFailure() const { encoder_factory_report_feedback_failure(*factory_); }
  std::shared_ptr<RustPlainEncoderFactory> WithRetention(VideoCreationRetention retention) const {
    return std::make_shared<RustPlainEncoderFactory>(
        encoder_factory_clone(*factory_), std::move(retention));
  }
  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    auto result = encoder_factory_formats(*factory_);
    return std::move(result->values);
  }
  std::vector<webrtc::SdpVideoFormat> GetImplementations() const override {
    auto result = encoder_factory_implementations(*factory_);
    return std::move(result->values);
  }
  FfiVideoFactorySupport QueryValue(
      const webrtc::SdpVideoFormat &format,
      std::optional<std::string> mode,
      std::optional<webrtc::Resolution> resolution, bool legacy = false) const {
    const std::string bytes = mode.value_or("");
    return encoder_factory_query(*factory_,
        std::make_unique<NativeSdpVideoFormat>(format), mode.has_value(),
        rust::Slice<const std::uint8_t>(reinterpret_cast<const std::uint8_t *>(bytes.data()), bytes.size()),
        resolution ? std::make_unique<NativeRtpResolution>(*resolution) : nullptr, legacy);
  }
  CodecSupport QueryCodecSupport(const webrtc::SdpVideoFormat& format, std::optional<std::string> mode) const override {
    const auto result = QueryValue(format, std::move(mode), std::nullopt, true);
    return {result.support.supported, result.support.power_efficient};
  }
  CodecSupport QueryCodecSupport(
      const webrtc::SdpVideoFormat &format,
      std::optional<std::string> mode,
      std::optional<webrtc::Resolution> resolution) const override {
    auto result = QueryValue(format, std::move(mode), resolution);
    return {result.support.supported, result.support.power_efficient};
  }
  std::unique_ptr<webrtc::VideoEncoder>
  Create(const webrtc::Environment &environment,
         const webrtc::SdpVideoFormat &format) override {
    auto retention = CreationRetention(retention_, environment);
    auto selected = video_factory_environment_native(**retention);
    auto encoder = encoder_factory_create(*factory_,
        retain_environment_from_sdk(environment, selected.get()), **retention,
        std::make_unique<NativeSdpVideoFormat>(format));
    if (!encoder_is_valid(*encoder)) return nullptr;
    return std::make_unique<RustEncoder>(std::move(encoder), format, std::move(retention), encoder_factory_clone(*factory_), info_invocation_);
  }

private:
  VideoCreationRetention retention_;
  std::shared_ptr<EncoderInfoInvocation> info_invocation_;
  rust::Box<RustVideoEncoderFactory> factory_;
};

// Preserve the SDK adapter and all inherited virtual behavior. Environment
// storage and the exact private factory outlive the adapter's deferred Create,
// Release, and destructor. No bridge lock is held across delegated SDK calls.
class RetainedSimulcastEncoder final : public webrtc::VideoEncoder {
  class Call final {
   public:
    explicit Call(const RetainedSimulcastEncoder& owner) : owner_(owner) {
      std::lock_guard lock(owner_.mutex_);
      if (!owner_.call_active_) { owner_.call_active_ = true; entered_ = true; }
    }
    ~Call() { if (entered_) { std::lock_guard lock(owner_.mutex_); owner_.call_active_ = false; } }
    explicit operator bool() const { return entered_; }
    Call(const Call&) = delete; Call& operator=(const Call&) = delete;
   private:
    const RetainedSimulcastEncoder& owner_;
    bool entered_ = false;
  };
 public:
  RetainedSimulcastEncoder(const webrtc::Environment &environment,
      std::shared_ptr<RustPlainEncoderFactory> factory,
      const webrtc::SdpVideoFormat &format, bool installed)
      : environment_(environment), info_invocation_(std::make_shared<EncoderInfoInvocation>()),
        factory_(factory->WithInfoInvocation(info_invocation_)),
        encoder_(std::make_unique<webrtc::SimulcastEncoderAdapter>(
            environment_, factory_.get(), nullptr, format)),
        info_override_(environment_.field_trials()) {
    // FieldTrialsView promises immutable values. Parse with the actual public
    // SDK settings type, after the adapter has read these same keys. The Rust
    // view shares its winning first-value cache, so these immutable SDK reads
    // invoke no additional Rust provider callbacks or substitutions. Default
    // native trials obey the same SDK immutability contract.
    info_invocation_->trial_alignment = info_override_.requested_resolution_alignment().value_or(1);
    info_invocation_->installed = installed;
  }
  void SetFecControllerOverride(webrtc::FecControllerOverride *value) override {
    Call call(*this); if (call) encoder_->SetFecControllerOverride(value);
  }
  template<class Operation> int Initialize(const webrtc::VideoCodec* codec, Operation operation) {
    info_invocation_->leaf_multistream_initialized = false;
    const int result = operation();
    if (result >= 0 && codec) {
      info_invocation_->adapter_initialized = true;
      std::uint32_t sum = codec->numberOfSimulcastStreams == 0 ? codec->maxBitrate : 0;
      for (std::size_t i = 0; i < codec->numberOfSimulcastStreams; ++i) sum += codec->simulcastStream[i].maxBitrate;
      const auto total = sum == 0 ? 1u : std::max<unsigned>(codec->numberOfSimulcastStreams, 1);
      bypass_ = total == 1 || info_invocation_->leaf_multistream_initialized;
      frame_type_count_ = std::max<unsigned>(codec->numberOfSimulcastStreams, 1);
      active_indices_.clear();
      for (std::size_t i = 0; i < total; ++i)
        if (codec->numberOfSimulcastStreams == 0 || codec->simulcastStream[i].active) active_indices_.push_back(i);
    }
    return result;
  }
  int InitEncode(const webrtc::VideoCodec *codec, int32_t cores, size_t size) override {
    Call call(*this); return call ? Initialize(codec, [&] { return encoder_->InitEncode(codec, cores, size); }) : WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  }
  int InitEncode(const webrtc::VideoCodec *codec, const Settings &settings) override {
    Call call(*this); return call ? Initialize(codec, [&] { return encoder_->InitEncode(codec, settings); }) : WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  }
  int32_t RegisterEncodeCompleteCallback(webrtc::EncodedImageCallback *callback) override {
    Call call(*this); return call ? encoder_->RegisterEncodeCompleteCallback(callback) : WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  }
  int32_t Release() override {
    Call call(*this); if (!call) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    const auto result = encoder_->Release(); info_invocation_->adapter_initialized = false; return result;
  }
  int32_t Encode(const webrtc::VideoFrame &frame,
      const std::vector<webrtc::VideoFrameType> *types) override {
    Call call(*this); if (!call) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
    if (info_invocation_->adapter_initialized && types) {
#if RTC_DCHECK_IS_ON
      if (bypass_ && types->size() != frame_type_count_) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
#endif
      if (!bypass_) for (auto index : active_indices_)
        if (types->size() <= index) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    }
    return encoder_->Encode(frame, types);
  }
  void SetRates(const RateControlParameters &rates) override {
    Call call(*this); if (!call) { factory_->ReportFeedbackFailure(); return; }
    if (info_invocation_->adapter_initialized && !(rates.framerate_fps < 1.0)) {
      const auto rounded = rates.framerate_fps + 0.5;
      bool safe = std::isfinite(rounded) && rounded >= 0 && rounded < 4294967296.0;
      if (!bypass_ && !active_indices_.empty() && rates.bitrate.get_sum_bps() && !rates.bandwidth_allocation.IsZero()) {
        safe = safe && rates.bandwidth_allocation.IsFinite();
        if (safe) for (auto index : active_indices_) {
          const __int128 numerator = static_cast<__int128>(rates.bandwidth_allocation.bps()) * rates.bitrate.GetSpatialLayerSum(index);
          safe = safe && numerator >= INT64_MIN && numerator <= INT64_MAX;
#if RTC_DCHECK_IS_ON
          safe = safe && numerator / rates.bitrate.get_sum_bps() >= 0;
#endif
        }
      }
      if (!safe) { factory_->ReportFeedbackFailure(); return; }
    }
    encoder_->SetRates(rates);
  }
  void OnPacketLossRateUpdate(float rate) override { Call call(*this); if (call) encoder_->OnPacketLossRateUpdate(rate); else factory_->ReportFeedbackFailure(); }
  void OnRttUpdate(int64_t rtt) override { Call call(*this); if (call) encoder_->OnRttUpdate(rtt); else factory_->ReportFeedbackFailure(); }
  void OnLossNotification(const LossNotification &loss) override { Call call(*this); if (call) encoder_->OnLossNotification(loss); else factory_->ReportFeedbackFailure(); }
  EncoderInfo GetEncoderInfo() const override {
    Call call(*this);
    if (!call) { info_valid_.store(false, std::memory_order_release); factory_->ReportInfoFailure(); return EncoderInfo{}; }
    info_invocation_->Begin();
    auto info = encoder_->GetEncoderInfo();
    if (info_invocation_->installed) {
      // Current scaling configuration is private in this SDK. Until a native
      // consumer hook exists, installed alignment must be representable for
      // every supported rung. Plain owning metadata remains unrestricted.
      const auto multiplier = info.apply_alignment_to_all_simulcast_layers
          ? (1u << (webrtc::kMaxSimulcastStreams - 1)) : 1u;
      bool safe = info.requested_resolution_alignment != 0 &&
          info.requested_resolution_alignment <= static_cast<std::uint32_t>(INT_MAX) / multiplier;
      for (auto pixel : info.preferred_pixel_formats)
        safe = safe && static_cast<int>(pixel) >= 0 && static_cast<int>(pixel) <= 8;
      bool valid, found;
      video_encoder_info_limits(NativeVideoEncoderInfo(info), INT_MAX, valid, found);
      safe = safe && valid;
      if (!safe) {
        info_invocation_->Fail(); factory_->ReportInfoFailure();
        info = EncoderInfo{};
      }
    }
    info_valid_.store(info_invocation_->Succeeded(), std::memory_order_release);
    info_invocation_->End();
    return info;
  }
  bool InfoSucceeded() const { return info_valid_.load(std::memory_order_acquire); }
 private:
  mutable std::mutex mutex_;
  mutable bool call_active_ = false;
  mutable std::atomic<bool> info_valid_{false};
  webrtc::Environment environment_;
  std::shared_ptr<EncoderInfoInvocation> info_invocation_;
  std::shared_ptr<RustPlainEncoderFactory> factory_;
  std::unique_ptr<webrtc::VideoEncoder> encoder_;
  const webrtc::SimulcastEncoderAdapterEncoderInfoSettings info_override_;
  bool bypass_ = false;
  std::size_t frame_type_count_ = 0;
  std::vector<std::size_t> active_indices_;
};

class RustEncoderFactory final : public webrtc::VideoEncoderFactory {
public:
  explicit RustEncoderFactory(rust::Box<RustVideoEncoderFactory> factory)
      : plain_(std::make_shared<RustPlainEncoderFactory>(std::move(factory))) {}
  std::unique_ptr<NativeSdpVideoFormatList> FormatsValue() const { return plain_->FormatsValue(); }
  std::unique_ptr<NativeSdpVideoFormatList> ImplementationsValue() const { return plain_->ImplementationsValue(); }
  explicit RustEncoderFactory(std::shared_ptr<RustPlainEncoderFactory> plain, bool installed)
      : plain_(std::move(plain)), installed_(installed) {}
  std::unique_ptr<webrtc::VideoEncoderFactory> WithRetention(VideoCreationRetention retention, bool installed) const {
    return std::make_unique<RustEncoderFactory>(plain_->WithRetention(std::move(retention)), installed);
  }
  std::unique_ptr<EncoderSelectorInterface> SelectorValue(bool& valid) const { return plain_->SelectorValue(valid); }
  std::unique_ptr<EncoderSelectorInterface> GetEncoderSelector() const override { return plain_->GetEncoderSelector(); }
  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return plain_->GetSupportedFormats();
  }
  std::vector<webrtc::SdpVideoFormat> GetImplementations() const override {
    return plain_->GetImplementations();
  }
  FfiVideoFactorySupport QueryValue(
      const webrtc::SdpVideoFormat &format, std::optional<std::string> mode,
      std::optional<webrtc::Resolution> resolution, bool legacy = false) const {
    return plain_->QueryValue(format, std::move(mode), resolution, legacy);
  }
  CodecSupport QueryCodecSupport(const webrtc::SdpVideoFormat& format, std::optional<std::string> mode) const override {
    return plain_->QueryCodecSupport(format, std::move(mode));
  }
  CodecSupport QueryCodecSupport(
      const webrtc::SdpVideoFormat &format,
      std::optional<std::string> scalability_mode,
      std::optional<webrtc::Resolution> resolution) const override {
    return plain_->QueryCodecSupport(format, scalability_mode, resolution);
  }
  std::unique_ptr<webrtc::VideoEncoder> Create(
      const webrtc::Environment &env,
      const webrtc::SdpVideoFormat &format) override {
    return std::make_unique<RetainedSimulcastEncoder>(env, plain_, format, installed_);
  }

private:
  std::shared_ptr<RustPlainEncoderFactory> plain_;
  const bool installed_ = false;
};

class RustDecoderFactory final : public webrtc::VideoDecoderFactory {
public:
  std::unique_ptr<NativeSdpVideoFormatList> FormatsValue() const { return decoder_factory_formats(*factory_); }
  explicit RustDecoderFactory(rust::Box<RustVideoDecoderFactory> factory,
      VideoCreationRetention retention = nullptr, bool installed = false)
      : retention_(std::move(retention)), factory_(std::move(factory)), installed_(installed) {}
  std::unique_ptr<webrtc::VideoDecoderFactory> WithRetention(VideoCreationRetention retention, bool installed) const {
    return std::make_unique<RustDecoderFactory>(
        decoder_factory_clone(*factory_), std::move(retention), installed);
  }
  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    auto result = decoder_factory_formats(*factory_);
    return std::move(result->values);
  }
  FfiVideoFactorySupport QueryValue(
      const webrtc::SdpVideoFormat &format, bool reference_scaling,
      std::optional<webrtc::Resolution> resolution, bool legacy = false) const {
    return decoder_factory_query(*factory_,
        std::make_unique<NativeSdpVideoFormat>(format), reference_scaling,
        resolution ? std::make_unique<NativeRtpResolution>(*resolution) : nullptr, legacy);
  }
  CodecSupport QueryCodecSupport(const webrtc::SdpVideoFormat& format, bool reference_scaling) const override {
    const auto result = QueryValue(format, reference_scaling, std::nullopt, true);
    return {result.support.supported, result.support.power_efficient};
  }
  CodecSupport QueryCodecSupport(
      const webrtc::SdpVideoFormat &format, bool reference_scaling,
      std::optional<webrtc::Resolution> resolution) const override {
    auto result = QueryValue(format, reference_scaling, resolution);
    return {result.support.supported, result.support.power_efficient};
  }
  std::unique_ptr<webrtc::VideoDecoder>
  Create(const webrtc::Environment &environment,
         const webrtc::SdpVideoFormat &format) override {
    auto retention = CreationRetention(retention_, environment);
    auto selected = video_factory_environment_native(**retention);
    auto decoder = decoder_factory_create(*factory_,
        retain_environment_from_sdk(environment, selected.get()), **retention,
        std::make_unique<NativeSdpVideoFormat>(format));
    if (!decoder_is_valid(*decoder))
      return nullptr;
    auto callback_environment = installed_ ? retain_environment_from_sdk(environment, selected.get()) : nullptr;
    return std::make_unique<RustDecoder>(std::move(decoder), std::move(retention), std::move(callback_environment));
  }

private:
  VideoCreationRetention retention_;
  rust::Box<RustVideoDecoderFactory> factory_;
  const bool installed_;
};

class EncodedCollector final : public webrtc::EncodedImageCallback {
public:
  Result OnEncodedImage(const webrtc::EncodedImage &image,
                        const webrtc::CodecSpecificInfo *) override {
    images.push_back(image);
    return Result(Result::OK, image.RtpTimestamp());
  }
  void OnFrameDropped(uint32_t, int, bool) override {}
  std::vector<webrtc::EncodedImage> images;
};

class DecodedCollector final : public webrtc::DecodedImageCallback {
public:
  int32_t Decoded(webrtc::VideoFrame &frame) override {
    ++count;
    for (std::uint8_t value : CopyI420(frame))
      checksum = checksum * 131 + value;
    return 0;
  }
  std::uint32_t count = 0;
  std::uint64_t checksum = 0;
};

} // namespace

NativeVideoFrame::NativeVideoFrame(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeVideoFrame::~NativeVideoFrame() = default;
const NativeVideoFrame::State &NativeVideoFrame::state() const noexcept {
  return *state_;
}
std::unique_ptr<NativeVideoFrame>
wrap_video_frame(const webrtc::VideoFrame &frame) noexcept {
  auto state = std::make_unique<NativeVideoFrame::State>();
  state->width = static_cast<std::uint32_t>(frame.width());
  state->height = static_cast<std::uint32_t>(frame.height());
  state->timestamp_us = frame.timestamp_us();
  state->rtp_timestamp = frame.rtp_timestamp();
  state->rotation = static_cast<std::uint16_t>(frame.rotation());
  state->i420 = CopyI420(frame);
  return std::make_unique<NativeVideoFrame>(std::move(state));
}
NativeEncodedVideoFrame::NativeEncodedVideoFrame(
    std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeEncodedVideoFrame::~NativeEncodedVideoFrame() = default;
const NativeEncodedVideoFrame::State &
NativeEncodedVideoFrame::state() const noexcept {
  return *state_;
}
NativeEncodedImageCallback::NativeEncodedImageCallback(
    std::shared_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeEncodedImageCallback::~NativeEncodedImageCallback() = default;
const std::shared_ptr<NativeEncodedImageCallback::State> &
NativeEncodedImageCallback::state() const noexcept {
  return state_;
}
NativeDecodedImageCallback::NativeDecodedImageCallback(
    std::shared_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeDecodedImageCallback::~NativeDecodedImageCallback() = default;
const std::shared_ptr<NativeDecodedImageCallback::State> &
NativeDecodedImageCallback::state() const noexcept {
  return state_;
}

int32_t RustEncoder::RegisterEncodeCompleteCallback(
    webrtc::EncodedImageCallback *callback) {
  Call call(*this, true); if (!call) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  auto old = callback_->state(); old->Retire();
  auto state = std::make_shared<NativeEncodedImageCallback::State>();
  state->codec_type = old->codec_type; state->h264_packetization_mode = old->h264_packetization_mode;
  state->engine = old->engine; state->configured_streams = old->configured_streams;
  state->callback = callback; state->active = callback != nullptr;
  callback_ = std::make_shared<NativeEncodedImageCallback>(std::move(state));
  return failed_ ? WEBRTC_VIDEO_CODEC_UNINITIALIZED : encoder_register_callback_value(*encoder_, callback ? callback_ : nullptr);
}
int32_t RustEncoder::Release() {
  Call call(*this, true); if (!call) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  callback_->state()->Retire();
  info_invocation_->adapter_initialized = false;
  if (fec_override_) RetireFecOverride(*fec_override_);
  released_ = true;
  return failed_ ? WEBRTC_VIDEO_CODEC_OK : encoder_release(*encoder_);
}
int32_t RustDecoder::RegisterDecodeCompleteCallback(
    webrtc::DecodedImageCallback *callback) {
  Call call(*this, true); if (!call) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  callback_->state()->Retire();
  auto state = std::make_shared<NativeDecodedImageCallback::State>();
  state->engine_environment = callback_->state()->engine_environment;
  state->callback = callback; state->active = callback != nullptr;
  callback_ = std::make_shared<NativeDecodedImageCallback>(std::move(state));
  return decoder_register_callback_value(*decoder_, callback ? callback_ : nullptr);
}
int32_t RustDecoder::Release() {
  Call call(*this, true); if (!call) return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
  callback_->state()->Retire();
  released_ = true;
  return decoder_release(*decoder_);
}

namespace {
// A lifecycle/context delegate, not an encoder implementation. Native codec
// decisions, error values, metadata and output remain the SDK encoder's own.
class ScopedBuiltinVp8Encoder final : public webrtc::VideoEncoder {
 public:
  ScopedBuiltinVp8Encoder(std::unique_ptr<webrtc::VideoEncoder> encoder,
      std::shared_ptr<NativeBuiltinVp8Execution> execution)
      : execution_(std::move(execution)), encoder_(std::move(encoder)) {}
  ~ScopedBuiltinVp8Encoder() override {
    NativeBuiltinVp8Scope scope(execution_);
    encoder_.reset();
  }
  void SetFecControllerOverride(webrtc::FecControllerOverride* value) override {
    NativeBuiltinVp8Scope scope(execution_);
    encoder_->SetFecControllerOverride(value);
  }
  int32_t InitEncode(const webrtc::VideoCodec* codec, int32_t cores, size_t payload) override {
    execution_->SetCores(cores);
    NativeBuiltinVp8Scope scope(execution_);
    return encoder_->InitEncode(codec, cores, payload);
  }
  int InitEncode(const webrtc::VideoCodec* codec, const Settings& settings) override {
    execution_->SetCores(settings.number_of_cores);
    NativeBuiltinVp8Scope scope(execution_);
    return encoder_->InitEncode(codec, settings);
  }
  int32_t RegisterEncodeCompleteCallback(webrtc::EncodedImageCallback* callback) override {
    NativeBuiltinVp8Scope scope(execution_);
    callback_.target = callback;
    return encoder_->RegisterEncodeCompleteCallback(callback ? &callback_ : nullptr);
  }
  int32_t Release() override {
    NativeBuiltinVp8Scope scope(execution_);
    return encoder_->Release();
  }
  int32_t Encode(const webrtc::VideoFrame& frame,
      const std::vector<webrtc::VideoFrameType>* types) override {
    NativeBuiltinVp8Scope scope(execution_);
    return encoder_->Encode(frame, types);
  }
  void SetRates(const RateControlParameters& parameters) override {
    NativeBuiltinVp8Scope scope(execution_);
    encoder_->SetRates(parameters);
  }
  void OnPacketLossRateUpdate(float value) override {
    NativeBuiltinVp8Scope scope(execution_);
    encoder_->OnPacketLossRateUpdate(value);
  }
  void OnRttUpdate(int64_t value) override {
    NativeBuiltinVp8Scope scope(execution_);
    encoder_->OnRttUpdate(value);
  }
  void OnLossNotification(const LossNotification& value) override {
    NativeBuiltinVp8Scope scope(execution_);
    encoder_->OnLossNotification(value);
  }
  EncoderInfo GetEncoderInfo() const override {
    NativeBuiltinVp8Scope scope(execution_);
    return encoder_->GetEncoderInfo();
  }
 private:
  class Callback final : public webrtc::EncodedImageCallback {
   public:
    Result OnEncodedImage(const webrtc::EncodedImage& image,
        const webrtc::CodecSpecificInfo* info) override {
      // Do not lend encoder provenance to application/SDK observer code or
      // unrelated nested products, including native decoders.
      NativeBuiltinVp8Scope scope(nullptr);
      return target->OnEncodedImage(image, info);
    }
    void OnFrameDropped(uint32_t timestamp, int spatial, bool end) override {
      NativeBuiltinVp8Scope scope(nullptr);
      target->OnFrameDropped(timestamp, spatial, end);
    }
    webrtc::EncodedImageCallback* target = nullptr;
  };
  std::shared_ptr<NativeBuiltinVp8Execution> execution_;
  Callback callback_;
  std::unique_ptr<webrtc::VideoEncoder> encoder_;
};
}  // namespace
std::unique_ptr<webrtc::VideoEncoder> retain_builtin_encoder_execution(
    const NativeVideoEncoderFactory& factory, const NativeEnvironment& selected,
    const webrtc::Environment& actual, const webrtc::SdpVideoFormat& format,
    std::unique_ptr<webrtc::VideoEncoder> encoder) noexcept {
  if (!encoder || !factory.builtin_factory() || !absl::EqualsIgnoreCase(format.name, "VP8")) return encoder;
  bool valid;
  auto execution = builtin_vp8_execution(selected, actual, valid);
  if (!valid) return nullptr;  // Never substitute native OS services on mismatch.
  if (!execution) return encoder;  // An unbound/native-only environment.
  return std::make_unique<ScopedBuiltinVp8Encoder>(std::move(encoder), std::move(execution));
}

struct NativeVideoEncoderProduct::State {
  webrtc::Thread* sequence = nullptr;
  // Native encoder and private adapter must die before either environment lease.
  VideoCreationRetention retention;
  std::unique_ptr<NativeEnvironment> environment_owner;
  webrtc::Environment environment;
  std::unique_ptr<webrtc::VideoEncoderFactory> adapter;
  std::unique_ptr<NativeEncodedImageSink> output_callback;
  std::shared_ptr<NativeFecOverride> fec_override;
  std::unique_ptr<webrtc::VideoEncoder> encoder;
  bool rust_product = false;
  State(VideoCreationRetention retained, std::unique_ptr<NativeEnvironment> owner)
      : retention(std::move(retained)), environment_owner(std::move(owner)),
        environment(environment_owner->environment()) {}
};
NativeVideoEncoderProduct::NativeVideoEncoderProduct(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeVideoEncoderProduct::~NativeVideoEncoderProduct() {
  if (auto* sequence = state_->sequence; sequence && !sequence->IsCurrent()) sequence->BlockingCall([&] { state_.reset(); });
}
const NativeVideoEncoderProduct::State& NativeVideoEncoderProduct::state() const noexcept { return *state_; }
NativeVideoEncoderProduct::State& NativeVideoEncoderProduct::state() noexcept { return *state_; }
std::unique_ptr<NativeVideoEncoderProduct> video_encoder_create_product(
    const NativeVideoEncoderFactory& factory, const NativeEnvironment& environment,
    rust::Box<RustVideoFactoryEnvironment> retained, const NativeSdpVideoFormat& format) noexcept {
  auto retention = std::make_shared<rust::Box<RustVideoFactoryEnvironment>>(std::move(retained));
  auto state = std::make_unique<NativeVideoEncoderProduct::State>(retention, clone_environment(environment));
  auto* selected = &factory.factory();
  if (factory.rust_factory()) {
    state->adapter = factory.CreateAdapter(retention, false);
    selected = state->adapter.get();
  }
  // The builtin Create checks codec membership and constructs its adapter.
  // Unlike Query/Init, it does not invoke FuzzyMatch's format diagnostics.
  state->encoder = retain_builtin_encoder_execution(factory, *state->environment_owner,
      state->environment, format.value(), selected->Create(state->environment, format.value()));
  state->rust_product = factory.rust_factory();
  if (!state->encoder) return nullptr;
  return std::make_unique<NativeVideoEncoderProduct>(std::move(state));
}
std::unique_ptr<NativeVideoEncoderProduct> video_encoder_create_actor_product(const NativePeerConnectionFactory& owner,
    const NativeVideoEncoderFactory& factory, const NativeEnvironment& environment,
    rust::Box<RustVideoFactoryEnvironment> retained, const NativeSdpVideoFormat& format) noexcept {
  return owner.worker_thread()->BlockingCall([&] {
    auto product = video_encoder_create_product(factory, environment, std::move(retained), format);
    if (product) product->state().sequence = owner.worker_thread();
    return product;
  });
}
void video_encoder_product_fec(NativeVideoEncoderProduct& product, std::shared_ptr<NativeFecOverride> value) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_fec(product, std::move(value)); });

  auto& state = product.state();
  state.encoder->SetFecControllerOverride(value ? value->callback() : nullptr);
  state.fec_override = std::move(value);
}
std::int32_t video_encoder_product_register_callback(NativeVideoEncoderProduct& product, const NativeEncodedImageSink* sink) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_register_callback(product, sink); });

  auto candidate = sink ? encoded_sink_clone(*sink) : nullptr;
  auto& state = product.state();
  const auto status = state.encoder->RegisterEncodeCompleteCallback(candidate ? candidate->callback() : nullptr);
  // Keep the caller's pointer alive independently of leaf callback statuses.
  // Native adapters and the builtin context delegate retain this callback.
  state.output_callback = std::move(candidate);
  return status;
}
std::int32_t video_encoder_product_initialize(NativeVideoEncoderProduct& product, const NativeVideoCodec& codec, FfiOwnedEncoderSettings fields, bool legacy) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_initialize(product, codec, fields, legacy); });

  const auto& value = codec.value();
  // The selected SDK simulcast adapter traverses this prefix during InitEncode.
  if (value.numberOfSimulcastStreams > webrtc::kMaxSimulcastStreams) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
  webrtc::VideoEncoder::Settings settings(webrtc::VideoEncoder::Capabilities(fields.loss_notification), fields.number_of_cores, fields.max_payload_size);
  if (fields.has_thread_limit) settings.encoder_thread_limit = fields.thread_limit;
  return legacy ? product.state().encoder->InitEncode(&value, fields.number_of_cores, fields.max_payload_size)
                : product.state().encoder->InitEncode(&value, settings);
}
std::int32_t video_encoder_product_encode(NativeVideoEncoderProduct& product, const NativeRawVideoFrame& frame, bool has_types, rust::Slice<const std::int32_t> input) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_encode(product, frame, has_types, input); });

  std::vector<webrtc::VideoFrameType> types;
  for (auto type : input) {
    // VideoFrameType is a non-fixed enum. Preserve all representable values.
    if (type < 0 || type > 7) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    types.push_back(static_cast<webrtc::VideoFrameType>(type));
  }
  return product.state().encoder->Encode(frame.value(), has_types ? &types : nullptr);
}
void video_encoder_product_rates(NativeVideoEncoderProduct& product, const NativeVideoRateControl& rates) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_rates(product, rates); });
 product.state().encoder->SetRates(rates.value()); }
std::unique_ptr<NativeVideoEncoderInfo> video_encoder_product_info(const NativeVideoEncoderProduct& product) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_info(product); });

  auto info = product.state().encoder->GetEncoderInfo();
  if (product.state().rust_product && !static_cast<const RetainedSimulcastEncoder&>(*product.state().encoder).InfoSucceeded()) return nullptr;
  return std::make_unique<NativeVideoEncoderInfo>(std::move(info));
}
rust::Vec<std::uint8_t> video_encoder_product_name(const NativeVideoEncoderProduct& product, bool& valid) noexcept {
  const auto info = video_encoder_product_info(product); valid = info != nullptr;
  rust::Vec<std::uint8_t> result;
  if (info) for (const unsigned char value : info->value().implementation_name) result.push_back(value);
  return result;
}
void video_encoder_product_packet_loss(NativeVideoEncoderProduct& product, float rate) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_packet_loss(product, rate); });
 product.state().encoder->OnPacketLossRateUpdate(rate); }
void video_encoder_product_rtt(NativeVideoEncoderProduct& product, std::int64_t rtt) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_rtt(product, rtt); });
 product.state().encoder->OnRttUpdate(rtt); }
void video_encoder_product_loss_notification(NativeVideoEncoderProduct& product, FfiVideoEncoderLossNotification value) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_loss_notification(product, value); });

  webrtc::VideoEncoder::LossNotification loss{value.last_decodable, value.last_received,
      value.has_dependencies_decodable ? std::optional<bool>(value.dependencies_decodable) : std::nullopt,
      value.has_received_decodable ? std::optional<bool>(value.received_decodable) : std::nullopt};
  product.state().encoder->OnLossNotification(loss);
}
std::int32_t video_encoder_product_release(NativeVideoEncoderProduct& product) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_encoder_product_release(product); });
 return product.state().encoder->Release(); }

struct NativeVideoDecoderProduct::State {
  webrtc::Thread* sequence = nullptr;
  // Reverse destruction order: decoder, private factory adapter, actual copied
  // SDK Environment and its independent Rust-only dependency retention.
  VideoCreationRetention retention;
  std::unique_ptr<NativeEnvironment> environment_owner;
  webrtc::Environment environment;
  std::unique_ptr<webrtc::VideoDecoderFactory> adapter;
  // The borrowed callback must outlive Release and actual decoder destruction.
  std::unique_ptr<NativeDecodedImageSink> output_callback;
  std::unique_ptr<webrtc::VideoDecoder> decoder;
  bool rust_product = false;
  bool builtin_av1 = false;
  State(VideoCreationRetention retained, std::unique_ptr<NativeEnvironment> owner)
      : retention(std::move(retained)), environment_owner(std::move(owner)),
        environment(environment_owner->environment()) {}
};
NativeVideoDecoderProduct::NativeVideoDecoderProduct(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeVideoDecoderProduct::~NativeVideoDecoderProduct() {
  if (auto* sequence = state_->sequence; sequence && !sequence->IsCurrent()) sequence->BlockingCall([&] { state_.reset(); });
}
const NativeVideoDecoderProduct::State &NativeVideoDecoderProduct::state() const noexcept { return *state_; }
NativeVideoDecoderProduct::State &NativeVideoDecoderProduct::state() noexcept { return *state_; }
std::unique_ptr<NativeVideoDecoderProduct> video_decoder_create_product(
    const NativeVideoDecoderFactory &factory, const NativeEnvironment &environment,
    rust::Box<RustVideoFactoryEnvironment> retained, const NativeSdpVideoFormat &format,
    std::int32_t &status) noexcept {
  status = 0;
  // Only the selected general builtin has unsupported-format logging. Its
  // stateless query is a precise witness for that reached diagnostic path;
  // supported formats never have unrelated scalability metadata rejected.
  if (!factory.rust_factory() && !factory.observations() &&
      !SdpVideoFormatStringSafe(format.value()) &&
      !factory.factory().QueryCodecSupport(format.value(), false, std::nullopt).is_supported) {
    status = 1;
    return nullptr;
  }
  auto retention = std::make_shared<rust::Box<RustVideoFactoryEnvironment>>(std::move(retained));
  auto state = std::make_unique<NativeVideoDecoderProduct::State>(retention, clone_environment(environment));
  auto *selected = &factory.factory();
  if (factory.rust_factory()) {
    state->adapter = factory.CreateAdapter(retention, false);
    selected = state->adapter.get();
  }
  state->decoder = selected->Create(state->environment, format.value());
  state->rust_product = factory.rust_factory();
  state->builtin_av1 = !state->rust_product && absl::EqualsIgnoreCase(format.value().name, "AV1");
  if (!state->decoder) return nullptr;
  return std::make_unique<NativeVideoDecoderProduct>(std::move(state));
}
std::unique_ptr<NativeVideoDecoderProduct> video_decoder_create_actor_product(const NativePeerConnectionFactory& owner,
    const NativeVideoDecoderFactory& factory, const NativeEnvironment& environment,
    rust::Box<RustVideoFactoryEnvironment> retained, const NativeSdpVideoFormat& format, std::int32_t& status) noexcept {
  return owner.worker_thread()->BlockingCall([&] {
    auto product = video_decoder_create_product(factory, environment, std::move(retained), format, status);
    if (product) product->state().sequence = owner.worker_thread();
    return product;
  });
}
std::int32_t video_decoder_product_register_callback(NativeVideoDecoderProduct& product, const NativeDecodedImageSink* sink) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_decoder_product_register_callback(product, sink); });

  auto candidate = sink ? decoded_sink_clone(*sink) : nullptr;
  auto* target = candidate ? candidate->callback() : nullptr;
  auto& state = product.state();
  const auto status = state.decoder->RegisterDecodeCompleteCallback(target);
  // Every admitted stock CPU decoder installs the argument. Rust's adapter can
  // return a caller registration error after storing it, or reject after Release.
  // Use its actual slot witness, not the sign of the native status.
  const bool installed = !state.rust_product || static_cast<const RustDecoder&>(*state.decoder).CallbackMatches(target);
  if (installed) state.output_callback = std::move(candidate);
  return status;
}
std::int32_t video_decoder_product_decode(NativeVideoDecoderProduct& product, const NativeEncodedImage& image,
    bool missing_overload, bool missing, std::int64_t render_time_ms, bool& valid) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_decoder_product_decode(product, image, missing_overload, missing, render_time_ms, valid); });

  const auto& value = image.value(); const auto buffer = value.GetEncodedData();
  valid = value.size() == 0 || (buffer && value.data() && value.size() <= buffer->size());
  // dav1d_data_wrap validates a non-null pointer even for an empty image; the
  // pinned wrapper ignores its error before SendData. VP8/VP9 concealment must
  // retain their distinct zero-size/null behavior. No integer size clamp is
  // applied to the native libvpx unsigned narrowing or H264's own error path.
  if (product.state().builtin_av1 && !value.data()) valid = false;
  if (!valid) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
  return missing_overload ? product.state().decoder->Decode(value, missing, render_time_ms)
                          : product.state().decoder->Decode(value, render_time_ms);
}
std::unique_ptr<NativeVideoDecoderInfo> video_decoder_product_info(const NativeVideoDecoderProduct &product) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_decoder_product_info(product); });

  auto info = product.state().decoder->GetDecoderInfo();
  if (product.state().rust_product && !static_cast<const RustDecoder&>(*product.state().decoder).InfoSucceeded()) return nullptr;
  return std::make_unique<NativeVideoDecoderInfo>(std::move(info));
}
rust::Vec<std::uint8_t> video_decoder_product_implementation_name(const NativeVideoDecoderProduct &product, bool& valid) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_decoder_product_implementation_name(product, valid); });

  const auto *name = product.state().decoder->ImplementationName();
  valid = name != nullptr;
  if (product.state().rust_product) valid = static_cast<const RustDecoder&>(*product.state().decoder).NameSucceeded();
  rust::Vec<std::uint8_t> bytes;
  if (valid) for (const auto *value = name; *value; ++value) bytes.push_back(static_cast<std::uint8_t>(*value));
  return bytes;
}
bool video_decoder_product_configure(NativeVideoDecoderProduct& product, const NativeVideoDecoderSettings& settings) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_decoder_product_configure(product, settings); });

  return product.state().decoder->Configure(settings.value());
}
std::int32_t video_decoder_product_release(NativeVideoDecoderProduct &product) noexcept {
  if (auto* sequence = product.state().sequence; sequence && !sequence->IsCurrent()) return sequence->BlockingCall([&] { return video_decoder_product_release(product); });

  return product.state().decoder->Release();
}

std::unique_ptr<NativeOwnedVideoEncoderSelector> video_encoder_factory_selector(const NativeVideoEncoderFactory& factory, bool& valid) noexcept {
  valid = true;
  auto value = factory.rust_factory() ? static_cast<RustEncoderFactory&>(factory.factory()).SelectorValue(valid) : factory.factory().GetEncoderSelector();
  return value ? std::make_unique<NativeOwnedVideoEncoderSelector>(std::move(value)) : nullptr;
}
NativeVideoEncoderFactory::NativeVideoEncoderFactory(
    std::unique_ptr<webrtc::VideoEncoderFactory> factory, bool rust_factory, bool builtin_factory) noexcept
    : factory_(std::move(factory)), rust_factory_(rust_factory), builtin_factory_(builtin_factory) {}
NativeVideoEncoderFactory::~NativeVideoEncoderFactory() = default;
std::unique_ptr<webrtc::VideoEncoderFactory> NativeVideoEncoderFactory::CreateAdapter(
    VideoCreationRetention retention, bool installed) const {
  // Closed producer tag, set only by the RustEncoderFactory constructor.
  return static_cast<RustEncoderFactory &>(*factory_).WithRetention(std::move(retention), installed);
}
webrtc::VideoEncoderFactory &
NativeVideoEncoderFactory::factory() const noexcept {
  return *factory_;
}
NativeVideoDecoderFactory::NativeVideoDecoderFactory(
    std::unique_ptr<webrtc::VideoDecoderFactory> factory,
    std::shared_ptr<CodecDecoderObservations> observations, bool rust_factory) noexcept
    : factory_(std::move(factory)), observations_(std::move(observations)), rust_factory_(rust_factory) {}
NativeVideoDecoderFactory::~NativeVideoDecoderFactory() = default;
std::unique_ptr<webrtc::VideoDecoderFactory> NativeVideoDecoderFactory::CreateAdapter(
    VideoCreationRetention retention, bool installed) const {
  return static_cast<RustDecoderFactory &>(*factory_).WithRetention(std::move(retention), installed);
}
webrtc::VideoDecoderFactory &
NativeVideoDecoderFactory::factory() const noexcept {
  return *factory_;
}
const std::shared_ptr<CodecDecoderObservations>&
NativeVideoDecoderFactory::observations() const noexcept { return observations_; }

NativeAudioEncoderFactory::NativeAudioEncoderFactory(
    std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeAudioEncoderFactory::~NativeAudioEncoderFactory() = default;
const NativeAudioEncoderFactory::State &
NativeAudioEncoderFactory::state() const noexcept {
  return *state_;
}
webrtc::scoped_refptr<webrtc::AudioEncoderFactory>
NativeAudioEncoderFactory::factory() const noexcept {
  return state_->factory;
}
webrtc::scoped_refptr<webrtc::FrameTransformerInterface>
NativeAudioEncoderFactory::opus_transformer() const noexcept {
  return state_->opus_transformer;
}
std::uint64_t audio_encoder_opus_handoff_failures(
    const NativeAudioEncoderFactory& factory) noexcept {
  const auto& counter = factory.state().handoff_failures;
  return counter ? counter->load() : 0;
}
NativeAudioDecoderFactory::NativeAudioDecoderFactory(
    std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeAudioDecoderFactory::NativeAudioDecoderFactory(
    webrtc::scoped_refptr<webrtc::AudioDecoderFactory> factory) noexcept
    : state_(std::make_unique<State>()) {
  state_->factory = std::move(factory);
}
std::unique_ptr<NativeAudioDecoderFactory> wrap_audio_decoder_factory(
    webrtc::scoped_refptr<webrtc::AudioDecoderFactory> factory,
    const NativeAudioDecoderFactory* selected) noexcept {
  if (!factory) return nullptr;
  if (selected && selected->factory().get() == factory.get()) {
    // Copy provenance/telemetry only after exact identity comparison. The
    // supplied scoped_refptr is the very same object, not a factory replacement.
    auto state = std::make_unique<NativeAudioDecoderFactory::State>(selected->state());
    state->factory = std::move(factory);
    return std::make_unique<NativeAudioDecoderFactory>(std::move(state));
  }
  return std::make_unique<NativeAudioDecoderFactory>(std::move(factory));
}
std::uint8_t audio_decoder_factory_builtin_kind(
    const NativeAudioDecoderFactory& factory) noexcept {
  return factory.state().builtin_kind;
}
NativeAudioDecoderFactory::~NativeAudioDecoderFactory() = default;
const NativeAudioDecoderFactory::State &
NativeAudioDecoderFactory::state() const noexcept {
  return *state_;
}
webrtc::scoped_refptr<webrtc::AudioDecoderFactory>
NativeAudioDecoderFactory::factory() const noexcept {
  return state_->factory;
}

std::unique_ptr<NativeVideoEncoderFactory>
new_video_encoder_factory(rust::Box<RustVideoEncoderFactory> factory) noexcept {
  return std::make_unique<NativeVideoEncoderFactory>(
      std::make_unique<RustEncoderFactory>(std::move(factory)), true);
}
std::unique_ptr<NativeVideoDecoderFactory>
new_video_decoder_factory(rust::Box<RustVideoDecoderFactory> factory) noexcept {
  return std::make_unique<NativeVideoDecoderFactory>(
      std::make_unique<RustDecoderFactory>(std::move(factory)), nullptr, true);
}
namespace {
// One completed emission, including legacy emissions without a declared level.
// No payload/timestamp/SSRC join: Encode -> initial Transform is synchronous on
// ChannelSend's dedicated queue in the pinned PeerConnection audio pipeline.
struct OpusEmission {
  bool has_level = false;
  std::uint8_t level = 0;
  bool voice = true;
};
struct OpusEmissionSlot {
  std::mutex mutex;
  std::optional<OpusEmission> pending;
};
// Only installed through the provided PC sender setter, after AddTransceiver.
// PC voice streams are constructed with null transformers; registration and
// reset then run on the encoder queue. Arbitrary direct Call/AudioSendStream
// construction with a non-null transformer does NOT satisfy this invariant.
class OpusMetadataTransformer : public webrtc::FrameTransformerInterface {
 public:
  OpusMetadataTransformer()
      : failures_(std::make_shared<std::atomic<std::uint64_t>>(0)) {}
  std::shared_ptr<std::atomic<std::uint64_t>> failures() const {
    return failures_;
  }
  void Begin(webrtc::TaskQueueBase* queue,
             const std::shared_ptr<OpusEmissionSlot>& slot) {
    { std::lock_guard lock(slot->mutex); slot->pending.reset(); }
    if (!queue) return;
    std::lock_guard lock(mutex_);
    emissions_[queue] = slot;
  }
  void Clear(webrtc::TaskQueueBase* queue,
             const std::shared_ptr<OpusEmissionSlot>& slot) {
    { std::lock_guard lock(slot->mutex); slot->pending.reset(); }
    std::lock_guard lock(mutex_);
    const auto it = emissions_.find(queue);
    // An old encoder can be destroyed after its queue address is reused.
    // Never erase a replacement generation's registration.
    if (it != emissions_.end() && it->second.lock() == slot)
      emissions_.erase(it);
  }
  void Publish(const std::shared_ptr<OpusEmissionSlot>& slot,
               OpusEmission emission) {
    std::lock_guard lock(slot->mutex);
    slot->pending = emission;
  }
  void RegisterTransformedFrameCallback(
      webrtc::scoped_refptr<webrtc::TransformedFrameCallback> callback) override {
    auto* queue = webrtc::TaskQueueBase::Current();
    if (!queue || !callback) { ++*failures_; return; }
    auto replacement = std::make_shared<StockFrameCallbackSlot>(std::move(callback));
    std::shared_ptr<StockFrameCallbackSlot> retired;
    {
      std::lock_guard lock(mutex_);
      retired = std::exchange(callbacks_[queue], std::move(replacement));
    }
    if (retired) retired->Retire();
  }
  void UnregisterTransformedFrameCallback() override {
    std::shared_ptr<StockFrameCallbackSlot> retired;
    {
      std::lock_guard lock(mutex_);
      const auto it = callbacks_.find(webrtc::TaskQueueBase::Current());
      if (it != callbacks_.end()) {
        retired = std::move(it->second);
        callbacks_.erase(it);
      }
    }
    if (retired) retired->Retire();
  }
  void Transform(
      std::unique_ptr<webrtc::TransformableFrameInterface> frame) override {
    const auto* queue = webrtc::TaskQueueBase::Current();
    std::shared_ptr<OpusEmissionSlot> slot;
    std::shared_ptr<StockFrameCallbackSlot> callback;
    {
      std::lock_guard lock(mutex_);
      if (auto it = emissions_.find(queue); it != emissions_.end())
        slot = it->second.lock();
      if (auto it = callbacks_.find(queue); it != callbacks_.end())
        callback = it->second;
    }
    std::optional<OpusEmission> emission;
    if (slot) {
      std::lock_guard lock(slot->mutex);
      emission = std::exchange(slot->pending, std::nullopt);
    }
    if (!emission || !callback || !frame ||
        frame->GetDirection() != webrtc::TransformableFrameInterface::Direction::kSender) {
      ++*failures_;
      return;
    }
    if (emission->has_level) {
      auto* audio = static_cast<webrtc::TransformableAudioFrameInterface*>(frame.get());
      const bool voice = audio->Type() ==
          webrtc::TransformableAudioFrameInterface::FrameType::kAudioFrameSpeech;
      if (!audio->CanSetAudioLevel() || voice != emission->voice) {
        ++*failures_;
        return;
      }
      audio->SetAudioLevel(emission->level);
    }
    // The slot admits only a live generation and pins unregister until the
    // native call returns, without a lock across an injected scheduler post.
    callback->OnTransformedFrame(std::move(frame));
  }
 private:
  std::shared_ptr<std::atomic<std::uint64_t>> failures_;
  std::mutex mutex_;
  std::unordered_map<const webrtc::TaskQueueBase*, std::weak_ptr<OpusEmissionSlot>>
      emissions_;
  std::unordered_map<const webrtc::TaskQueueBase*,
                     std::shared_ptr<StockFrameCallbackSlot>>
      callbacks_;
};
// This factory is exclusively for Opus-frame tracks. Ordinary PCM must use
// a separate peer factory: a PCM sentinel cannot be made collision-free.
class OpusCarrierEncoder final : public webrtc::AudioEncoder {
 public:
  OpusCarrierEncoder(webrtc::AudioEncoderFactory::Options options,
                     webrtc::scoped_refptr<OpusMetadataTransformer> transformer)
      : options_(std::move(options)), transformer_(std::move(transformer)) {}
  ~OpusCarrierEncoder() override { transformer_->Clear(queue_, emission_slot_); }

  int SampleRateHz() const override { return 48000; }
  // AudioEncoder::NumChannels describes INPUT, not the encoded Opus payload.
  // Match the private source layout so ACM never remixes the opaque bytes.
  size_t NumChannels() const override { return opus_carrier::kChannels; }
  size_t Num10MsFramesInNextPacket() const override {
    return next_slots_;
  }
  size_t Max10MsFramesInAPacket() const override { return 6; }
  int GetTargetBitrate() const override {
    return 32000;
  }
  void Reset() override {
    transformer_->Clear(queue_, emission_slot_);
    ReleasePacket();
    remaining_ = 0;
    bytes_.clear();
    first_timestamp_.reset();
  }
  std::optional<std::pair<webrtc::TimeDelta, webrtc::TimeDelta>>
  GetFrameLengthRange() const override {
    return std::pair(webrtc::TimeDelta::Millis(10),
                     webrtc::TimeDelta::Millis(60));
  }

 private:
  EncodedInfo EncodeImpl(std::uint32_t timestamp,
                         std::span<const std::int16_t> audio,
                         webrtc::Buffer* encoded) override {
    using namespace opus_carrier;
    auto* queue = webrtc::TaskQueueBase::Current();
    if (queue_ != queue) transformer_->Clear(queue_, emission_slot_);
    queue_ = queue;
    transformer_->Begin(queue_, emission_slot_);
    EncodedInfo info;
    auto block = std::span(reinterpret_cast<const std::uint8_t*>(audio.data()),
                           audio.size_bytes());
    const auto block_bytes = kBytesPerChannelBlock * kChannels;
    if (block.size() != block_bytes || !HasMagic(block)) {
      Reset();
      return info;
    }
    const auto length = Load16(block.data() + 8);
    const auto duration = Load16(block.data() + 10);
    const auto packet_timestamp = Load32(block.data() + 12);
    const auto checksum = Load32(block.data() + 16);
    const auto source = Load32(block.data() + 20);
    const auto packet = Load32(block.data() + 24);
    const auto recipient = Load32(block.data() + 28);
    const auto slot = block[32];
    const OpusEmission emission{block[35] != 0, block[33], block[34] != 0};
    if (block[35] > 1 || (emission.has_level && (block[33] > 127 || block[34] > 1))) {
      Reset();
      return info;
    }
    if (duration < 480 || duration > 2880 || duration % 480 != 0 ||
        !length || length > kMaxPayloadBytes ||
        length > (duration / 480) * (block_bytes - kHeaderBytes) ||
        slot >= duration / 480 || !source || !packet || !recipient) {
      Reset();
      return info;
    }
    // This native input copy has been consumed, even if reset or replacement
    // means it no longer belongs to a complete packet assembly. Never release
    // still-queued copies from another recipient or later slots on Reset.
    Acknowledge(source, packet, recipient, slot);
    if (slot == 0) {
      expected_bytes_ = length;
      next_slots_ = duration / 480;
      remaining_ = next_slots_;
      packet_timestamp_ = packet_timestamp;
      checksum_ = checksum;
      source_id_ = source;
      packet_id_ = packet;
      recipient_id_ = recipient;
      emission_ = emission;
      if (!first_timestamp_) {
        first_timestamp_ = packet_timestamp_;
        first_encoder_timestamp_ = timestamp;
      }
      bytes_.clear();
      bytes_.reserve(expected_bytes_);
    } else if (!remaining_ || source != source_id_ || packet != packet_id_ ||
               recipient != recipient_id_ || length != expected_bytes_ ||
               duration / 480 != next_slots_ ||
               slot != next_slots_ - remaining_ ||
               packet_timestamp != packet_timestamp_ || checksum != checksum_ ||
               emission.has_level != emission_.has_level ||
               (emission.has_level && (emission.level != emission_.level ||
                                       emission.voice != emission_.voice))) {
      Reset();
      return info;
    }
    block = block.subspan(kHeaderBytes);
    const auto needed = expected_bytes_ - bytes_.size();
    bytes_.insert(bytes_.end(), block.begin(),
                  block.begin() + std::min(needed, block.size()));
    if (--remaining_ != 0) return info;
    if (bytes_.size() != expected_bytes_ || Checksum(bytes_) != checksum_) {
      bytes_.clear();
      ReleasePacket();
      return info;
    }
    encoded->AppendData(bytes_.data(), bytes_.size());
    info.encoded_bytes = bytes_.size();
    info.encoded_timestamp = first_encoder_timestamp_ +
                             (packet_timestamp_ - *first_timestamp_);
    info.payload_type = options_.payload_type;
    info.encoder_type = CodecType::kOpus;
    if (emission_.has_level) info.speech = emission_.voice;
    transformer_->Publish(emission_slot_, emission_);
    bytes_.clear();
    ReleasePacket();
    return info;
  }

  void ReleasePacket() {
    source_id_ = 0;
    packet_id_ = 0;
    recipient_id_ = 0;
  }

  const webrtc::AudioEncoderFactory::Options options_;
  webrtc::scoped_refptr<OpusMetadataTransformer> transformer_;
  std::shared_ptr<OpusEmissionSlot> emission_slot_ = std::make_shared<OpusEmissionSlot>();
  webrtc::TaskQueueBase* queue_ = nullptr;
  OpusEmission emission_;
  std::vector<std::uint8_t> bytes_;
  std::optional<std::uint32_t> first_timestamp_;
  std::uint32_t first_encoder_timestamp_ = 0;
  std::uint32_t packet_timestamp_ = 0;
  std::uint32_t checksum_ = 0;
  std::uint32_t source_id_ = 0;
  std::uint32_t packet_id_ = 0;
  std::uint32_t recipient_id_ = 0;
  size_t expected_bytes_ = 0;
  size_t next_slots_ = 2;
  size_t remaining_ = 0;
};

class OpusCarrierFactory : public webrtc::AudioEncoderFactory {
 public:
  explicit OpusCarrierFactory(webrtc::scoped_refptr<OpusMetadataTransformer> transformer)
      : builtin_(webrtc::CreateBuiltinAudioEncoderFactory()),
        transformer_(std::move(transformer)) {}
  std::vector<webrtc::AudioCodecSpec> GetSupportedEncoders() override {
    std::vector<webrtc::AudioCodecSpec> opus;
    for (auto spec : builtin_->GetSupportedEncoders()) {
      if (IsOpus(spec.format)) {
        spec.format.parameters["stereo"] = "0";
        spec.info.num_channels = opus_carrier::kChannels;
        // Opus has one RFC 7587 RTP identity (opus/48000/2). The pinned
        // payload mapper ignores stereo fmtp when matching audio codecs, so
        // advertising stereo=0 and stereo=1 separately creates duplicate PTs.
        // Advertise the canonical format once. Private input dimensions do
        // not select or rewrite the genuine Opus payload's channel count.
        opus.push_back(std::move(spec));
      }
    }
    return opus;
  }
  std::optional<webrtc::AudioCodecInfo> QueryAudioEncoder(
      const webrtc::SdpAudioFormat& format) override {
    if (!IsOpus(format)) return std::nullopt;
    auto info = builtin_->QueryAudioEncoder(format);
    if (info) info->num_channels = opus_carrier::kChannels;
    return info;
  }
  std::unique_ptr<webrtc::AudioEncoder> Create(
      const webrtc::Environment&, const webrtc::SdpAudioFormat& format,
      Options options) override {
    if (!QueryAudioEncoder(format)) return nullptr;
    return std::make_unique<OpusCarrierEncoder>(
        std::move(options), transformer_);
  }
 private:
  static bool IsOpus(const webrtc::SdpAudioFormat& format) {
    return webrtc::SdpAudioFormat("opus", 48000, 2).Matches(format);
  }
 private:
  webrtc::scoped_refptr<webrtc::AudioEncoderFactory> builtin_;
  webrtc::scoped_refptr<OpusMetadataTransformer> transformer_;
};
} // namespace

std::unique_ptr<NativeAudioEncoderFactory>
new_opus_carrier_audio_encoder_factory() noexcept {
  auto state = std::make_unique<NativeAudioEncoderFactory::State>();
  auto transformer = webrtc::make_ref_counted<OpusMetadataTransformer>();
  state->handoff_failures = transformer->failures();
  state->opus_transformer = transformer;
  state->factory = webrtc::make_ref_counted<OpusCarrierFactory>(std::move(transformer));
  return std::make_unique<NativeAudioEncoderFactory>(std::move(state));
}

std::unique_ptr<NativeAudioEncoderFactory>
new_builtin_audio_encoder_factory() noexcept {
  auto state = std::make_unique<NativeAudioEncoderFactory::State>();
  state->factory = webrtc::CreateBuiltinAudioEncoderFactory();
  state->selected_builtin = true;
  return state->factory
             ? std::make_unique<NativeAudioEncoderFactory>(std::move(state))
             : nullptr;
}
std::unique_ptr<NativeAudioDecoderFactory>
new_builtin_audio_decoder_factory() noexcept {
  auto state = std::make_unique<NativeAudioDecoderFactory::State>();
  state->factory = webrtc::CreateBuiltinAudioDecoderFactory();
  state->builtin_kind = 1;
  return state->factory
             ? std::make_unique<NativeAudioDecoderFactory>(std::move(state))
             : nullptr;
}

std::unique_ptr<NativeVideoEncoderFactory>
new_builtin_video_encoder_factory() noexcept {
  auto factory = webrtc::CreateBuiltinVideoEncoderFactory();
  return factory ? std::make_unique<NativeVideoEncoderFactory>(std::move(factory), false, true) : nullptr;
}

std::unique_ptr<NativeVideoDecoderFactory>
new_builtin_video_decoder_factory() noexcept {
  auto factory = webrtc::CreateBuiltinVideoDecoderFactory();
  return factory ? std::make_unique<NativeVideoDecoderFactory>(std::move(factory)) : nullptr;
}

std::unique_ptr<NativeVideoDecoderFactory>
new_builtin_vp8_decoder_factory() noexcept {
  auto observations = std::make_shared<CodecDecoderObservations>();
  return std::make_unique<NativeVideoDecoderFactory>(
      std::make_unique<ObservedVp8Factory>(observations), observations);
}

std::unique_ptr<NativeAudioDecoderFactory>
new_builtin_opus_decoder_factory() noexcept {
  auto state = std::make_unique<NativeAudioDecoderFactory::State>();
  state->observations = std::make_shared<CodecDecoderObservations>();
  state->factory = webrtc::make_ref_counted<ObservedOpusFactory>(state->observations);
  state->builtin_kind = 2;
  return std::make_unique<NativeAudioDecoderFactory>(std::move(state));
}

FfiDecoderStatistics video_decoder_statistics(
    const NativeVideoDecoderFactory& factory) noexcept {
  return factory.observations() ? factory.observations()->Snapshot() : FfiDecoderStatistics{};
}
FfiDecoderStatistics audio_decoder_statistics(
    const NativeAudioDecoderFactory& factory) noexcept {
  return factory.state().observations ? factory.state().observations->Snapshot() : FfiDecoderStatistics{};
}
rust::Vec<FfiAudioCodecSpec> audio_encoder_formats(
    const NativeAudioEncoderFactory& factory) noexcept {
  rust::Vec<FfiAudioCodecSpec> result;
  for (const auto& spec : factory.factory()->GetSupportedEncoders()) {
    result.push_back(FfiAudioCodecSpec{FromNative(spec.format), FromNative(spec.info)});
  }
  return result;
}
rust::Vec<FfiAudioCodecSpec> audio_decoder_formats(
    const NativeAudioDecoderFactory& factory) noexcept {
  rust::Vec<FfiAudioCodecSpec> result;
  for (const auto& spec : factory.factory()->GetSupportedDecoders()) {
    result.push_back(FfiAudioCodecSpec{FromNative(spec.format), FromNative(spec.info)});
  }
  return result;
}
std::int8_t audio_decoder_supported(const NativeAudioDecoderFactory& factory,
    const FfiSdpAudioFormat& format) noexcept {
  // A refcounted SDK view alone is not evidence for stock parser/decode bounds.
  if (factory.state().builtin_kind == 0) return -1;
  const auto native = ToNative(format);
  // The observed factory short-circuits every non-Opus name before delegation.
  if (factory.state().observations && !absl::EqualsIgnoreCase(native.name, "opus")) return 0;
  // Both currently constructible factory resources use the stock template.
  // Keep these backend-specific guards off future caller-defined factories.
  // Stock L16 narrows channels before checking the codec name; G711 DCHECKs
  // invalid matching configurations rather than simply returning unsupported.
  if (native.num_channels > static_cast<std::size_t>(std::numeric_limits<int>::max())) return -1;
  if (native.clockrate_hz == 8000 &&
      (absl::EqualsIgnoreCase(native.name, "PCMU") || absl::EqualsIgnoreCase(native.name, "PCMA")) &&
      native.num_channels > webrtc::AudioDecoder::kMaxNumberOfChannels) return -1;
  // Opus precedes multiopus in the stock template. Do not reject irrelevant
  // malformed multiopus parameters when ordinary Opus already accepts a format.
  const auto stereo = native.parameters.find("stereo");
  const bool ordinary_opus = absl::EqualsIgnoreCase(native.name, "opus") &&
      native.clockrate_hz == 48000 && native.num_channels == 2 &&
      (stereo == native.parameters.end() || stereo->second == "0" || stereo->second == "1");
  if (!ordinary_opus && native.num_channels >= 1 &&
      native.num_channels <= webrtc::AudioDecoder::kMaxNumberOfChannels) {
    // Multiopus IsOk adds these signed integers after validating the mapping.
    // Its parser intentionally does not first filter the format's name/rate.
    const auto streams = webrtc::GetFormatParameter<int>(native, "num_streams");
    const auto coupled = webrtc::GetFormatParameter<int>(native, "coupled_streams");
    const auto mapping = webrtc::GetFormatParameter<std::vector<unsigned char>>(native, "channel_mapping");
    if (streams && coupled && mapping && *coupled >= 0 && *streams >= *coupled &&
        mapping->size() == native.num_channels &&
        *streams > std::numeric_limits<int>::max() - *coupled) return -1;
  }
  return factory.factory()->IsSupportedDecoder(native) ? 1 : 0;
}
bool audio_decoder_factory_context_contract(const NativeAudioDecoderFactory& factory,
    const NativeAudioDecoderFactory& other, const NativeEnvironment& environment) noexcept {
  const auto kind = audio_decoder_factory_builtin_kind(factory);
  if (kind == 0 || factory.factory().get() == other.factory().get()) return false;
  auto proven = wrap_audio_decoder_factory(factory.factory(), &factory);
  auto unproven = wrap_audio_decoder_factory(factory.factory());
  auto mismatch = wrap_audio_decoder_factory(factory.factory(), &other);
  if (!proven || !unproven || !mismatch || wrap_audio_decoder_factory(nullptr, &factory) ||
      proven->factory().get() != factory.factory().get() ||
      unproven->factory().get() != factory.factory().get() ||
      mismatch->factory().get() != factory.factory().get() ||
      proven->state().observations != factory.state().observations ||
      audio_decoder_factory_builtin_kind(*proven) != kind ||
      audio_decoder_factory_builtin_kind(*unproven) != 0 ||
      audio_decoder_factory_builtin_kind(*mismatch) != 0) return false;
  const auto format = FromNative(webrtc::SdpAudioFormat("opus", 48000, 2));
  if (audio_decoder_supported(*proven, format) != 1 ||
      audio_decoder_supported(*unproven, format) != -1 ||
      audio_decoder_supported(*mismatch, format) != -1) return false;
  // Neither refusal reaches a decoder constructor or NetEq dependency setup.
  const auto refused = audio_decoder_create(*unproven, environment, format);
  FfiNetEqConfig config{};
  config.sample_rate_hz = 8000;
  return !refused.valid && !refused.decoder &&
      !neteq_stock_create(environment, config, mismatch.get(), 0, 1);
}
namespace {
enum class OwnedAudioDecoderKind { Opus, G722, G711, L16 };
}

namespace {
std::vector<float> SelectedOpusBitrateMultipliers(const webrtc::FieldTrialsView& trials) {
  constexpr char key[] = "WebRTC-Audio-OpusBitrateMultipliers";
  if (!trials.IsEnabled(key)) return {};
  std::vector<std::string> pieces;
  webrtc::tokenize(trials.Lookup(key), '-', &pieces);
  if (pieces.size() < 2 || pieces[0] != "Enabled") return {};
  std::vector<float> result(pieces.size() - 1);
  for (std::size_t i = 1; i < pieces.size(); ++i) {
    if (!webrtc::FromString(pieces[i], &result[i - 1])) return {};
  }
  return result;
}

// This preflight belongs only to the two selected encoder factories. Unknown
// names and native parser rejection retain their ordinary unsupported result.
int SelectedAudioEncoderFormatAdmission(const NativeAudioEncoderFactory& factory,
    const webrtc::SdpAudioFormat& format) {
  if (factory.opus_transformer()) return 1;  // Carrier has an exact format gate.
  const bool multiopus = absl::EqualsIgnoreCase(format.name, "multiopus") && format.clockrate_hz == 48000;
  if (multiopus) {
    // Default bitrate is calculated even when maxaveragebitrate is supplied,
    // and before IsOk's much smaller supported-channel bound is checked.
    const auto playback = webrtc::GetFormatParameter<int>(format, "maxplaybackrate");
    const int rate = playback && *playback >= 8000 ? std::min(*playback, 48000) : 48000;
    const int per_channel = rate <= 8000 ? 12000 : rate <= 16000 ? 20000 : 32000;
    if (format.num_channels == 0 || format.num_channels >
        static_cast<std::size_t>(std::numeric_limits<int>::max() / per_channel)) return -1;
    // Preserve earlier IsOk rejection before investigating its stream addition.
    if (format.num_channels <= webrtc::AudioEncoder::kMaxNumberOfChannels) {
      const auto average = webrtc::GetFormatParameter<int>(format, "maxaveragebitrate");
      const int bitrate = average ? std::clamp(*average, 6000, 510000)
          : static_cast<int>(format.num_channels) * per_channel;
      if (bitrate <= webrtc::AudioEncoderMultiChannelOpusConfig::kMaxBitrateBps) {
        const auto streams = webrtc::GetFormatParameter<int>(format, "num_streams");
        const auto coupled = webrtc::GetFormatParameter<int>(format, "coupled_streams");
        const auto mapping = webrtc::GetFormatParameter<std::vector<unsigned char>>(format, "channel_mapping");
        if (streams && coupled && mapping && *coupled >= 0 && *streams >= *coupled &&
            mapping->size() == format.num_channels) {
          if (*streams > std::numeric_limits<int>::max() - *coupled) return -1;
          // Native IsOk eventually rejects this sum, but allocates an inverse
          // map first. Return that same unsupported result without allocation.
          if (*streams + *coupled >= 255) return 0;
        }
      }
    }
  }
  if (format.clockrate_hz == 8000) {
    const bool g722 = absl::EqualsIgnoreCase(format.name, "G722");
    const bool g711 = absl::EqualsIgnoreCase(format.name, "PCMU") || absl::EqualsIgnoreCase(format.name, "PCMA");
    if ((g722 || (g711 && format.num_channels >= 1)) &&
        (format.num_channels == 0 || format.num_channels > webrtc::AudioEncoder::kMaxNumberOfChannels)) return -1;
  }
  // L16 is the final stock fallback. Its parser checks the channel narrowing
  // before the codec name, and DCHECKs even for an otherwise unknown format.
  // Earlier stock parsers cannot accept this width; preserve the unsupported
  // result that L16 returns after its range check in non-DCHECK builds.
  if (format.num_channels > static_cast<std::size_t>(std::numeric_limits<int>::max())) return 0;
  return 1;
}
}  // namespace

FfiAudioEncoderQuery audio_encoder_query(const NativeAudioEncoderFactory& factory,
    const FfiSdpAudioFormat& format) noexcept {
  FfiAudioEncoderQuery result{};
  const auto native = ToNative(format);
  const int admission = SelectedAudioEncoderFormatAdmission(factory, native);
  result.valid = admission >= 0;
  if (admission != 1) return result;
  const auto info = factory.factory()->QueryAudioEncoder(native);
  result.present = info.has_value();
  if (info) result.info = FromNative(*info);
  return result;
}

namespace {
// Derived from the actual environment's immutable retaining clock contract.
// Millisecond smoothers and the packet-loss Timestamp subtraction have distinct
// domains: a wide finite microsecond range can still have safe millisecond math.
struct AnaClockEvidence {
  std::int64_t minimum_ms;
  std::int64_t maximum_ms;
  std::uint64_t span_ms;
  bool packet_loss_deltas_safe;

  bool admits_duration(int duration) const {
    const auto wide = static_cast<std::int64_t>(duration);
    if (wide > 0 && maximum_ms > INT64_MAX - wide) return false;
    if (wide < 0 && minimum_ms < INT64_MIN - wide) return false;
    const auto magnitude = static_cast<std::uint64_t>(wide < 0 ? -wide : wide);
    return span_ms <= static_cast<std::uint64_t>(INT64_MAX) - magnitude;
  }
};

std::optional<AnaClockEvidence> RetainedAnaClockEvidence(const FfiClockTimeBounds& bounds) {
  if (!bounds.known || (!bounds.monotonic && bounds.minimum_us != bounds.maximum_us) ||
      bounds.minimum_us > bounds.maximum_us ||
      bounds.minimum_us == INT64_MIN || bounds.maximum_us == INT64_MAX) return std::nullopt;
  const auto minimum = webrtc::Timestamp::Micros(bounds.minimum_us).ms();
  const auto maximum = webrtc::Timestamp::Micros(bounds.maximum_us).ms();
  const auto span_ms = static_cast<std::uint64_t>(maximum) - static_cast<std::uint64_t>(minimum);
  const auto span_us = static_cast<std::uint64_t>(bounds.maximum_us) - static_cast<std::uint64_t>(bounds.minimum_us);
  // Timestamp subtraction constructs a finite TimeDelta::Micros. Its native
  // DCHECK excludes INT64_MAX even though the signed subtraction itself fits.
  return AnaClockEvidence{minimum, maximum, span_ms, span_us < static_cast<std::uint64_t>(INT64_MAX)};
}

static_assert(std::numeric_limits<float>::is_iec559 && std::numeric_limits<float>::digits == 24 &&
    std::numeric_limits<float>::radix == 2 && sizeof(int) == sizeof(std::int32_t));

bool AnaSmoothingContextSafe() {
#if defined(__FAST_MATH__) || !defined(__linux__) || !defined(__x86_64__)
  // Other targets require their own floating-environment admission contract.
  return false;
#else
  static_assert(FLT_EVAL_METHOD == 0);
  unsigned short x87_control;
  asm volatile("fnstcw %0" : "=m"(x87_control));
  const unsigned int sse_control = _mm_getcsr();
  // Every exception, including inexact, must be masked. Native powf and the
  // recurrence may produce infinities/NaNs before the consuming average check.
  // Preserve binary32 subnormals and nearest-even rounding in both FP units.
  return std::fegetround() == FE_TONEAREST &&
      (x87_control & 0x003fu) == 0x003fu &&
      (sse_control & 0x1f80u) == 0x1f80u &&
      (sse_control & 0xe040u) == 0;
#endif
}

bool AnaBitrateSampleSafe(std::int32_t bitrate) {
  // Exact adjacent binary32 boundary, not float(INT32_MAX), which is 2^31.
  // The native recurrence preserves [-2^31, 2^31-128] under RN-even arithmetic;
  // see docs/audio-encoder-contracts.md for the endpoint argument and assumptions.
  constexpr float lower = -0x1p31f;
  constexpr float upper = 0x1.fffffep30f;
  const float sample = static_cast<float>(bitrate);
  return sample >= lower && sample <= upper;
}

bool AnaBandwidthPeriodSafe(bool has_period, std::int64_t period) {
  if (!has_period) return true;
  // Reproduce the SDK's two distinct operations. Large and even negative i64
  // periods may produce a nonamplifying rounded alpha; do not substitute an
  // arbitrary i32 period limit or change its pinned ABI conversion semantics.
  if (period < std::numeric_limits<std::int64_t>::min() / 4 ||
      period > std::numeric_limits<std::int64_t>::max() / 4) return false;
  const int native_duration = static_cast<int>(period * 4);
  const float alpha = native_duration == 0 ? 0.0f : std::exp(-1.0f / native_duration);
  // Some large negative durations round alpha to exactly one. Preserve that
  // native frozen-filter behavior instead of rejecting by duration sign alone.
  return std::isfinite(alpha) && alpha >= 0.0f && alpha <= 1.0f;
}

bool AnaSmoothingInitializationSafe(int duration, std::uint64_t clock_span_ms) {
  if (!AnaSmoothingContextSafe()) return false;
  const float alpha = duration == 0 ? 0.0f : std::exp(-1.0f / duration);
  if (!std::isfinite(alpha) || alpha < 0.0f) return false;
  if (duration < 0) {
    // The initial phase ends before the first sample. A rounded unit alpha or
    // a durable zero millisecond span makes every post-init multiplier one.
    return alpha <= 1.0f || clock_span_ms == 0;
  }
  if (alpha > 1.0f) return false;
  if (duration <= 1) return true; // Native has distinct zero/one-duration branches.
  const float factor = ::powf(duration, -1.0f / duration);
  const float constant = duration - ::powf(duration, 1.0f - 1.0f / duration);
  if (!std::isfinite(factor) || factor <= 0.0f || factor > 1.0f ||
      !std::isfinite(constant) || constant <= 0.0f) return false;
  // Do not assume powf is monotone just because the exact real power is. Witness
  // every possible initial-phase exponent under the supplied lifetime span.
  // This is evaluated once when retaining a proof, not on each native update.
  const auto window = std::min(clock_span_ms, static_cast<std::uint64_t>(duration));
  float previous = ::powf(factor, -static_cast<std::int64_t>(duration));
  if (!std::isfinite(previous) || previous <= 0.0f) return false;
  for (std::uint64_t elapsed = 1; elapsed <= window; ++elapsed) {
    const float power = ::powf(factor, static_cast<std::int64_t>(elapsed) - duration);
    if (!std::isfinite(power) || power <= 0.0f || power > previous) return false;
    previous = power;
  }
  // Both powf values in the native numerator are finite and nonincreasing;
  // their difference is nonnegative. With a positive constant, exp sees a
  // nonpositive argument (possibly -infinity), giving a multiplier in [0,1].
  return true;
}

enum class AnaSmoothingFailure : std::uint8_t {
  None = 0, Clock = 1, Context = 2, Initialization = 3,
  Sample = 4, TimeConstant = 5, Average = 6,
};

// Shared by delegating runtime guards under the owning encoder's mutex. Failure
// is sticky within an operation; only its owner may begin a fresh operation.
struct AnaSmoothingRuntime {
  explicit AnaSmoothingRuntime(AnaClockEvidence evidence) : clock(evidence) {}
  const AnaClockEvidence clock;
  AnaSmoothingFailure failure = AnaSmoothingFailure::None;

  void Reject(AnaSmoothingFailure reason) {
    if (failure == AnaSmoothingFailure::None) failure = reason;
  }
  bool healthy() const { return failure == AnaSmoothingFailure::None; }
};

// A delegating consuming-operation guard, not a replacement average. Owning
// Opus creation installs it; native numerical state stays inside SmoothingFilterImpl;
// only actual returned averages are admitted to downstream integer conversion.
class BoundedAnaBitrateSmoother final : public webrtc::SmoothingFilter {
 public:
  static std::unique_ptr<BoundedAnaBitrateSmoother> Create(
      std::shared_ptr<AnaSmoothingRuntime> runtime) {
    if (!runtime || !runtime->healthy()) return nullptr;
    if (!AnaSmoothingContextSafe()) {
      runtime->Reject(AnaSmoothingFailure::Context);
      return nullptr;
    }
    if (!runtime->clock.admits_duration(kInitializationMs)) {
      runtime->Reject(AnaSmoothingFailure::Clock);
      return nullptr;
    }
    if (!AnaSmoothingInitializationSafe(kInitializationMs, runtime->clock.span_ms)) {
      runtime->Reject(AnaSmoothingFailure::Initialization);
      return nullptr;
    }
    return std::unique_ptr<BoundedAnaBitrateSmoother>(
        new BoundedAnaBitrateSmoother(std::move(runtime)));
  }

  void AddSample(float sample, webrtc::Timestamp now) override {
    if (!runtime_->healthy()) return;
    // Include rounded float(INT32_MAX). It is legal filter state, even though
    // returning that particular average to the SDK's int cast is not legal.
    if (!std::isfinite(sample) || sample < -0x1p31f || sample > 0x1p31f) {
      runtime_->Reject(AnaSmoothingFailure::Sample);
      return;
    }
    const auto time = CheckTime(now);
    if (!time) return;
    if (!init_end_ms_) init_end_ms_ = *time + kInitializationMs;
    filter_.AddSample(sample, now);
    last_state_ms_ = *time;
  }

  std::optional<float> GetAverage(webrtc::Timestamp now) override {
    if (!runtime_->healthy()) return std::nullopt;
    // Native ignores the timestamp entirely before the first sample.
    if (!init_end_ms_) return filter_.GetAverage(now);
    const auto time = CheckTime(now);
    if (!time) return std::nullopt;
    const auto average = filter_.GetAverage(now);
    last_state_ms_ = *time;
    // Check the actual native result at the narrowing consumption seam. Do not
    // predict the average or reject an ignored rounded-high sample beforehand.
    if (average && (!std::isfinite(*average) || *average < -0x1p31f || *average >= 0x1p31f)) {
      runtime_->Reject(AnaSmoothingFailure::Average);
      return std::nullopt;
    }
    return average;
  }

  bool SetTimeConstantMs(int duration) override {
    if (!runtime_->healthy()) return false;
    // The SDK returns false without evaluating alpha until initialization ends.
    // Preserve ignored negative/large durations and the native return value.
    if (!init_end_ms_ || *last_state_ms_ < *init_end_ms_)
      return filter_.SetTimeConstantMs(duration);
    if (!AnaSmoothingContextSafe()) {
      runtime_->Reject(AnaSmoothingFailure::Context);
      return false;
    }
    const float alpha = duration == 0 ? 0.0f : std::exp(-1.0f / duration);
    if (!std::isfinite(alpha) || alpha < 0.0f) {
      runtime_->Reject(AnaSmoothingFailure::TimeConstant);
      return false;
    }
    // Amplification is not itself unsafe. The native extrapolation has only FP
    // operations, no numerical assertions or integer casts. In the admitted
    // nontrapping context even overflowing powf/recurrence remains defined;
    // GetAverage checks the actual result before any downstream integer cast.
    // Do not predict state or restrict a native-valid period by its sign.
    return filter_.SetTimeConstantMs(duration);
  }

 private:
  static constexpr int kInitializationMs = 5000;
  explicit BoundedAnaBitrateSmoother(std::shared_ptr<AnaSmoothingRuntime> runtime)
      : runtime_(std::move(runtime)), filter_(kInitializationMs) {}

  std::optional<std::int64_t> CheckTime(webrtc::Timestamp now) {
    if (!now.IsFinite()) {
      runtime_->Reject(AnaSmoothingFailure::Clock);
      return std::nullopt;
    }
    const auto time = now.ms();
    if (time < runtime_->clock.minimum_ms || time > runtime_->clock.maximum_ms ||
        (last_state_ms_ && time < *last_state_ms_)) {
      runtime_->Reject(AnaSmoothingFailure::Clock);
      return std::nullopt;
    }
    if (!AnaSmoothingContextSafe()) {
      runtime_->Reject(AnaSmoothingFailure::Context);
      return std::nullopt;
    }
    return time;
  }

  const std::shared_ptr<AnaSmoothingRuntime> runtime_;
  webrtc::SmoothingFilterImpl filter_;
  // Phase/time evidence only. The native filter owns all numerical state.
  std::optional<std::int64_t> init_end_ms_;
  std::optional<std::int64_t> last_state_ms_;
};

// These are retaining obligations, not permission to invoke native ANA yet.
// Admission reads the actual product and never constructs or replaces controllers.
struct AudioNetworkAdaptorAdmission {
  struct Offsets { int increase; int decrease; };
  std::uint8_t controller_mask = 0;
  std::vector<int> controller_kinds;
  std::vector<std::optional<Offsets>> controller_bitrate_offsets;
  std::vector<int> fec_time_constants_ms;
  struct ScoringPoint { int bandwidth_bps; float loss; };
  std::vector<ScoringPoint> scoring_points;
  std::vector<int> frame_lengths_ms;
  int initial_frame_length_ms = 0;
  std::optional<int> fec_time_constant_ms;
  std::optional<Offsets> legacy_offsets;
  std::vector<std::pair<int, int>> legacy_transitions;
  std::optional<Offsets> bitrate_offsets;
  std::optional<int> minimum_payload_bitrate_bps;
  bool slow_adaptation = false;
};
bool AnaIntDifferenceSafe(int value, int rate) {
  const auto result = static_cast<std::int64_t>(value) - rate;
  return result >= INT32_MIN && result <= INT32_MAX;
}

bool AnaOffsetAssertSafe(std::size_t overhead, int offset) {
  // RTC_DCHECK_GE/LE use SafeGe/SafeLe, so signed/unsigned comparisons are
  // mathematical, not the usual C++ unsigned promotion of a negative operand.
  if (offset == INT32_MIN) return false; // Native negates it as signed int.
  const auto magnitude = static_cast<std::size_t>(offset < 0 ? -offset : offset);
  return magnitude <= overhead;
}

int AnaUnsignedOverheadRate(std::size_t overhead, int offset, int frame) {
  // Preserve the SDK's defined size_t addition/multiplication wrap and pinned
  // narrowing. Replacing this with signed or saturating arithmetic changes it.
  return static_cast<int>((overhead + offset) * 8 * 1000 / frame);
}

enum class AnaBitrateFailure : std::uint8_t {
  None = 0, Metrics = 1, Slot = 2, Arithmetic = 3, Construction = 4,
};
struct AnaBitrateRuntime {
  AnaBitrateFailure failure = AnaBitrateFailure::None;
  bool healthy() const { return failure == AnaBitrateFailure::None; }
  void Reject(AnaBitrateFailure reason) {
    if (healthy()) failure = reason;
  }
};

// The owning manager must retain the original native controller and supply its
// exact factory construction arguments. This proxy never predicts bitrate or
// replaces native decisions. Only sparse metrics and consumed frame history are
// retained, including the SDK's conditional frame update.
class GuardedAnaBitrateController final : public webrtc::Controller {
 public:
  GuardedAnaBitrateController(webrtc::audio_network_adaptor::BitrateController& native,
      const webrtc::audio_network_adaptor::BitrateController::Config& config,
      std::shared_ptr<AnaBitrateRuntime> runtime)
      : native_(native), increase_(config.fl_increase_overhead_offset),
        decrease_(config.fl_decrease_overhead_offset),
        frame_ms_(config.initial_frame_length_ms), runtime_(std::move(runtime)) {}

  void UpdateNetworkMetrics(const NetworkMetrics& metrics) override {
    if (!runtime_->healthy()) return;
#if RTC_DCHECK_IS_ON
    if (metrics.overhead_bytes_per_packet && *metrics.overhead_bytes_per_packet == 0) {
      runtime_->Reject(AnaBitrateFailure::Metrics);
      return;
    }
#endif
    native_.UpdateNetworkMetrics(metrics);
    if (metrics.target_audio_bitrate_bps) target_ = metrics.target_audio_bitrate_bps;
    if (metrics.overhead_bytes_per_packet) overhead_ = metrics.overhead_bytes_per_packet;
  }

  void MakeDecision(webrtc::AudioEncoderRuntimeConfig* config) override {
    if (!runtime_->healthy()) return;
    if (!config || (RTC_DCHECK_IS_ON && config->bitrate_bps)) {
      runtime_->Reject(AnaBitrateFailure::Slot);
      return;
    }
    const bool consumes = target_.has_value() && overhead_.has_value();
    const int frame = consumes ? config->frame_length_ms.value_or(frame_ms_) : frame_ms_;
    if (consumes) {
      const int offset = config->last_fl_change_increase ? increase_ : decrease_;
      // Check only the offset and frame the SDK will actually consume. An
      // inactive INT_MIN offset or an ignored frame slot must not reject it.
      if (frame <= 0 ||
#if RTC_DCHECK_IS_ON
          !AnaOffsetAssertSafe(*overhead_, offset) ||
#endif
          !AnaIntDifferenceSafe(*target_, AnaUnsignedOverheadRate(*overhead_, offset, frame))) {
        runtime_->Reject(AnaBitrateFailure::Arithmetic);
        return;
      }
    }
    native_.MakeDecision(config);
    if (consumes) frame_ms_ = frame;
  }

 private:
  webrtc::audio_network_adaptor::BitrateController& native_;
  const int increase_;
  const int decrease_;
  int frame_ms_;
  const std::shared_ptr<AnaBitrateRuntime> runtime_;
  std::optional<int> target_;
  std::optional<std::size_t> overhead_;
};

#if WEBRTC_ENABLE_PROTOBUF
float AnaScoringDistance(const AudioNetworkAdaptorAdmission::ScoringPoint& point,
    int bandwidth, float loss) {
  // Match the pinned manager's normalization, subtraction and pow overloads.
  const auto normalize_bandwidth = [](int value) {
    return static_cast<float>(std::clamp(value, 0, 120000)) / 120000;
  };
  const auto normalize_loss = [](float value) { return std::min(value * 3.3333f, 1.0f); };
  const float delta_bandwidth = normalize_bandwidth(bandwidth) - normalize_bandwidth(point.bandwidth_bps);
  const float delta_loss = normalize_loss(loss) - normalize_loss(point.loss);
  const double distance = std::pow(delta_bandwidth, 2) + std::pow(delta_loss, 2);
  // Model the pinned RN-even IEEE narrowing without an out-of-range finite
  // double-to-float cast in the preflight. Native distance overflow yields +inf.
  const double maximum = std::numeric_limits<float>::max();
  if (distance > maximum) {
    const double midpoint = std::ldexp(1.0, 128) - std::ldexp(1.0, 103);
    return distance >= midpoint ? std::numeric_limits<float>::infinity() : std::numeric_limits<float>::max();
  }
  return static_cast<float>(distance);
}

bool AnaScoringOrderSafe(const AudioNetworkAdaptorAdmission& admission,
    std::optional<int> bandwidth, std::optional<float> loss) {
  if (admission.scoring_points.empty() || !bandwidth || !loss) return true;
  if (!AnaSmoothingContextSafe()) return false;
  bool nan = false;
  bool distinct = false;
  std::optional<float> first_distance;
  for (const auto& point : admission.scoring_points) {
    const float distance = AnaScoringDistance(point, *bandwidth, *loss);
    if (std::isnan(distance)) {
      nan = true;
    } else if (!first_distance) {
      first_distance = distance;
    } else if (*first_distance != distance) {
      distinct = true;
    }
  }
  // A NaN compares equivalent to every scored controller. That violates
  // equivalence transitivity exactly when two non-NaN distance classes exist.
  // Unscored controllers form a separate, strictly later equivalence class.
  return !(nan && distinct);
}

bool AnaControllerArithmeticSafe(const AudioNetworkAdaptorAdmission& admission,
    std::optional<std::size_t> overhead, std::optional<int> target,
    std::optional<std::pair<int, int>> uplink_bounds) {
  if (uplink_bounds && uplink_bounds->first > uplink_bounds->second) return false;
  if (!overhead) return true;
  if (admission.bitrate_offsets) {
    // UpdateNetworkMetrics asserts this before checking whether target is known.
    if (*overhead == 0) return false;
    if (target) {
      auto frames = admission.frame_lengths_ms;
      frames.push_back(admission.initial_frame_length_ms);
      const auto offsets = *admission.bitrate_offsets;
      for (int frame : frames) {
        if (frame <= 0) return false;
        for (int offset : {offsets.decrease, offsets.increase}) {
          // Only legacy frames write last_fl_change_increase. Without them the
          // native runtime config always selects the decrease offset.
          if (offset == offsets.increase && offset != offsets.decrease && !admission.legacy_offsets) continue;
          if (!AnaOffsetAssertSafe(*overhead, offset) ||
              !AnaIntDifferenceSafe(*target, AnaUnsignedOverheadRate(*overhead, offset, frame))) return false;
        }
      }
    }
  }
  if (admission.legacy_offsets) {
    const auto offsets = *admission.legacy_offsets;
    const auto& frames = admission.frame_lengths_ms;
    const auto has_transition = [&](int from, int to) {
      return std::find(admission.legacy_transitions.begin(), admission.legacy_transitions.end(),
          std::pair<int, int>{from, to}) != admission.legacy_transitions.end();
    };
    for (int from : frames) {
      // Match the native first configured transition in each search direction.
      const auto longer = std::find_if(frames.begin(), frames.end(), [&](int to) {
        return to > from && has_transition(from, to);
      });
      if (longer != frames.end()) {
        if (!AnaOffsetAssertSafe(*overhead, offsets.increase)) return false;
        if (uplink_bounds && static_cast<std::int64_t>(webrtc::AudioEncoderOpusConfig::kMinBitrateBps) +
            5000 + AnaUnsignedOverheadRate(*overhead, offsets.increase, from) > INT32_MAX) return false;
      }
      const auto shorter = std::find_if(frames.rbegin(), frames.rend(), [&](int to) {
        return to < from && has_transition(from, to);
      });
      // The native decrease branch has no offset bound assertion or negation.
      if (shorter != frames.rend() && uplink_bounds &&
          static_cast<std::int64_t>(webrtc::AudioEncoderOpusConfig::kMinBitrateBps) +
              5000 + AnaUnsignedOverheadRate(*overhead, offsets.decrease, *shorter) > INT32_MAX) return false;
    }
  }
  if (admission.minimum_payload_bitrate_bps && target && uplink_bounds) {
    // V2 first narrows size_t to int, then performs two signed products. Check
    // that exact consumed int, not an arbitrary range on the original size_t.
    const auto product = static_cast<std::int64_t>(static_cast<int>(*overhead)) * 8000;
    if (product < INT32_MIN || product > INT32_MAX) return false;
    for (int frame : admission.frame_lengths_ms) {
      if (frame <= 0) return false;
      const int rate = static_cast<int>(product) / frame;
      if (admission.slow_adaptation) {
        if (!AnaIntDifferenceSafe(uplink_bounds->first, rate) ||
            !AnaIntDifferenceSafe(uplink_bounds->second, rate)) return false;
      } else if (!AnaIntDifferenceSafe(*target, rate)) return false;
    }
  }
  return true;
}

using AnaConfig = webrtc::audio_network_adaptor::config::ControllerManager;
using AnaController = webrtc::audio_network_adaptor::config::Controller;
using AnaFecThreshold = webrtc::audio_network_adaptor::config::FecController::Threshold;

bool AnaThresholdConstructible(const AnaFecThreshold& threshold) {
  if (!threshold.has_low_bandwidth_bps() || !threshold.has_high_bandwidth_bps() ||
      !threshold.has_low_bandwidth_packet_loss() || !threshold.has_high_bandwidth_packet_loss()) return false;
#if RTC_DCHECK_IS_ON
  // Match the native float domain, including its integer-to-float rounding and
  // merged endpoints. Comparisons also reject the NaNs that violate its DCHECKs.
  return static_cast<float>(threshold.low_bandwidth_bps()) <= static_cast<float>(threshold.high_bandwidth_bps()) &&
      threshold.low_bandwidth_packet_loss() >= threshold.high_bandwidth_packet_loss();
#else
  return true;  // Native nonasserting curves retain their supplied FP values.
#endif
}
webrtc::ThresholdCurve AnaThreshold(const AnaFecThreshold& threshold) {
  return {static_cast<float>(threshold.low_bandwidth_bps()), threshold.low_bandwidth_packet_loss(),
      static_cast<float>(threshold.high_bandwidth_bps()), threshold.high_bandwidth_packet_loss()};
}
#endif

[[maybe_unused]] std::optional<AudioNetworkAdaptorAdmission> AdmitAudioNetworkAdaptorConstruction(
    const webrtc::AudioEncoderOpusImpl& encoder, rust::Slice<const std::uint8_t> bytes) {
#if WEBRTC_ENABLE_PROTOBUF
  if (bytes.size() > INT32_MAX) return std::nullopt;
  AnaConfig config;
  if (!config.ParseFromArray(bytes.data(), static_cast<int>(bytes.size()))) return std::nullopt;
  AudioNetworkAdaptorAdmission result;
  const auto frames = encoder.supported_frame_lengths_ms();
  result.frame_lengths_ms.assign(frames.begin(), frames.end());
  std::sort(result.frame_lengths_ms.begin(), result.frame_lengths_ms.end());
  result.initial_frame_length_ms = encoder.next_frame_length_ms();
  for (const auto& controller : config.controllers()) {
    std::uint8_t writes = 0;
    std::optional<AudioNetworkAdaptorAdmission::Offsets> bitrate_offsets;
    switch (controller.controller_case()) {
      case AnaController::kFecController: {
        writes = 1;
        const auto& fec = controller.fec_controller();
        if (!fec.has_fec_enabling_threshold() || !fec.has_fec_disabling_threshold() ||
            !fec.has_time_constant_ms() || !AnaThresholdConstructible(fec.fec_enabling_threshold()) ||
            !AnaThresholdConstructible(fec.fec_disabling_threshold())) return std::nullopt;
#if RTC_DCHECK_IS_ON
        if (!(AnaThreshold(fec.fec_disabling_threshold()) <= AnaThreshold(fec.fec_enabling_threshold()))) return std::nullopt;
#endif
        result.fec_time_constant_ms = fec.time_constant_ms();
        result.fec_time_constants_ms.push_back(fec.time_constant_ms());
        break;
      }
      case AnaController::kFrameLengthController: {
        writes = 2;
        const auto& fl = controller.frame_length_controller();
        if (!fl.has_fl_increasing_packet_loss_fraction() || !fl.has_fl_decreasing_packet_loss_fraction() ||
            std::find(frames.begin(), frames.end(), encoder.next_frame_length_ms()) == frames.end()) return std::nullopt;
        result.legacy_offsets = AudioNetworkAdaptorAdmission::Offsets{
            fl.fl_increase_overhead_offset(), fl.fl_decrease_overhead_offset()};
        // Keep every native map entry. Replacement-pair comments in the proto
        // do not erase the older pairs from the native implementation.
        if (fl.has_fl_20ms_to_40ms_bandwidth_bps()) result.legacy_transitions.emplace_back(20, 40);
        if (fl.has_fl_20ms_to_60ms_bandwidth_bps()) result.legacy_transitions.emplace_back(20, 60);
        if (fl.has_fl_40ms_to_20ms_bandwidth_bps()) result.legacy_transitions.emplace_back(40, 20);
        if (fl.has_fl_40ms_to_60ms_bandwidth_bps()) result.legacy_transitions.emplace_back(40, 60);
        if (fl.has_fl_60ms_to_40ms_bandwidth_bps()) result.legacy_transitions.emplace_back(60, 40);
        if (fl.has_fl_60ms_to_20ms_bandwidth_bps()) result.legacy_transitions.emplace_back(60, 20);
        if (fl.has_fl_60ms_to_120ms_bandwidth_bps()) result.legacy_transitions.emplace_back(60, 120);
        if (fl.has_fl_120ms_to_60ms_bandwidth_bps()) result.legacy_transitions.emplace_back(120, 60);
        break;
      }
      case AnaController::kChannelController: {
        writes = 4;
        const auto& channels = controller.channel_controller();
        if (!channels.has_channel_1_to_2_bandwidth_bps() || !channels.has_channel_2_to_1_bandwidth_bps() ||
            encoder.num_channels_to_encode() == 0 || encoder.num_channels_to_encode() > 2 ||
            encoder.num_channels_to_encode() > encoder.NumChannels()) return std::nullopt;
        break;
      }
      case AnaController::kDtxController: {
        writes = 8;
        const auto& dtx = controller.dtx_controller();
        if (!dtx.has_dtx_enabling_bandwidth_bps() || !dtx.has_dtx_disabling_bandwidth_bps()) return std::nullopt;
        break;
      }
      case AnaController::kBitrateController: {
        writes = 16;
        if (encoder.GetTargetBitrate() <= 0 || encoder.next_frame_length_ms() <= 0) return std::nullopt;
        const auto& bitrate = controller.bitrate_controller();
        result.bitrate_offsets = AudioNetworkAdaptorAdmission::Offsets{
            bitrate.fl_increase_overhead_offset(), bitrate.fl_decrease_overhead_offset()};
        bitrate_offsets = result.bitrate_offsets;
        break;
      }
      case AnaController::kFrameLengthControllerV2: {
        writes = 2;
        if (frames.empty()) return std::nullopt;
        const auto& fl = controller.frame_length_controller_v2();
        // Proto2 absence is natively zero/false, not a missing required field.
        result.minimum_payload_bitrate_bps = fl.min_payload_bitrate_bps();
        result.slow_adaptation = fl.use_slow_adaptation();
        break;
      }
      default:
        // Includes unset/unknown oneofs and the removed RPLR controller.
        return std::nullopt;
    }
    // Construction does not consume writer slots. In particular V2 can be
    // inactive beside another frame writer. Check occupancy only at each
    // reached native decision, in the actual selected order.
    result.controller_mask |= writes;
    result.controller_kinds.push_back(static_cast<int>(controller.controller_case()));
    result.controller_bitrate_offsets.push_back(bitrate_offsets);
    if (controller.has_scoring_point()) {
      const auto& score = controller.scoring_point();
      if (!score.has_uplink_bandwidth_bps() || !score.has_uplink_packet_loss_fraction()) return std::nullopt;
      // Construction accepts NaNs. Retain the actual points for a separate
      // strict-weak-ordering check when native scoring consumes both metrics.
      result.scoring_points.push_back({score.uplink_bandwidth_bps(), score.uplink_packet_loss_fraction()});
    }
  }
  if (!result.scoring_points.empty() && (!config.has_min_reordering_time_ms() ||
      !config.has_min_reordering_squared_distance())) return std::nullopt;
  // An empty controller list is accepted by native code. Unknown fields and
  // ignored configuration values are neither rewritten nor range-restricted.
  return result;
#else
  return std::nullopt;
#endif
}
}  // namespace

struct NativeAudioEncoderFailureObserver::State {
  std::atomic<std::uint32_t> first{0};
};
namespace { struct OwnedAnaRuntime; }
struct NativeAudioEncoder::State {
  State(webrtc::Environment env, std::unique_ptr<webrtc::AudioEncoder> value,
      bool is_opus, std::vector<float> multipliers, FfiClockTimeBounds bounds)
      : environment(std::move(env)), encoder(std::move(value)), opus(is_opus),
        bitrate_multipliers(std::move(multipliers)),
        ana_clock_evidence(RetainedAnaClockEvidence(bounds)) {}
  ~State() { encoder.reset(); }  // Runtime/dependencies remain alive through SDK destruction.
  mutable std::mutex mutex;
  webrtc::Environment environment;
  std::unique_ptr<webrtc::AudioEncoder> encoder;
  std::shared_ptr<OwnedAnaRuntime> ana;
  const bool opus;
  // The SDK caches the same immutable trial at creation, not at each update.
  const std::vector<float> bitrate_multipliers;
  const std::optional<AnaClockEvidence> ana_clock_evidence;
  std::optional<std::size_t> overhead_bytes;
  // A forwarding envelope owns this exact origin through its Rust transfer.
  // Never infer a backend type from metadata or cast the envelope as OpusImpl.
  const NativeAudioEncoder* forwarded_origin = nullptr;
  std::shared_ptr<NativeAudioEncoderFailureObserver::State> sdk_failure =
      std::make_shared<NativeAudioEncoderFailureObserver::State>();
};
NativeAudioEncoder::NativeAudioEncoder(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeAudioEncoder::~NativeAudioEncoder() = default;
NativeAudioEncoder::State& NativeAudioEncoder::state() const noexcept {
  return state_->forwarded_origin ? state_->forwarded_origin->state() : *state_;
}
webrtc::AudioEncoder& NativeAudioEncoder::sdk_encoder() const noexcept { return *state_->encoder; }
bool NativeAudioEncoder::forwarding() const noexcept { return state_->forwarded_origin != nullptr; }
std::unique_ptr<webrtc::AudioEncoder> NativeAudioEncoder::take_sdk_encoder() noexcept {
  if (!forwarding()) return nullptr;
  return std::move(state_->encoder);
}
std::unique_ptr<NativeAudioEncoderFailureObserver> audio_encoder_sdk_failure_observer(
    const NativeAudioEncoder& encoder) noexcept {
  return std::make_unique<NativeAudioEncoderFailureObserver>(encoder.state().sdk_failure);
}
std::uint32_t audio_encoder_sdk_failure(const NativeAudioEncoderFailureObserver& source) noexcept {
  return source.state()->first.load(std::memory_order_acquire);
}
std::unique_ptr<NativeAudioEncoderFailureObserver> audio_encoder_sdk_failure_clone(
    const NativeAudioEncoderFailureObserver& source) noexcept {
  return std::make_unique<NativeAudioEncoderFailureObserver>(source.state());
}

#include "pulsebeam-webrtc-sys/native/audio_encoder_ana.inc"

static FfiAudioEncoderCreation CreateAudioEncoderWithOptions(const NativeAudioEncoderFactory& factory,
    const NativeEnvironment& environment, const FfiSdpAudioFormat& format,
    const webrtc::AudioEncoderFactory::Options& options,
    std::optional<FfiClockTimeBounds> clock_bounds = std::nullopt) noexcept {
  FfiAudioEncoderCreation result{};
  const auto query = audio_encoder_query(factory, format);
  result.valid = query.valid;
  if (!query.valid || !query.present) return result;
  const auto native = ToNative(format);
  if (!factory.opus_transformer()) {
    const bool opus = absl::EqualsIgnoreCase(native.name, "opus");
    const bool multiopus = absl::EqualsIgnoreCase(native.name, "multiopus");
    if ((opus || multiopus) && (options.payload_type < 0 || options.payload_type > 127)) {
      result.valid = false;
      return result;
    }
    // Query permits ignored input channels with zero coded streams. Libopus
    // rejects that layout and this SDK constructor would abort, not return null.
    if (multiopus && webrtc::GetFormatParameter<int>(native, "num_streams") == 0) {
      result.valid = false;
      return result;
    }
  }
  auto env = environment.environment();
  const bool opus = factory.state().selected_builtin && !factory.opus_transformer() && absl::EqualsIgnoreCase(native.name, "opus");
  auto multipliers = opus ? SelectedOpusBitrateMultipliers(env.field_trials()) : std::vector<float>();
  auto state = std::make_unique<NativeAudioEncoder::State>(std::move(env), nullptr, opus,
      std::move(multipliers), clock_bounds.value_or(environment_clock_time_bounds(environment)));
  if (opus) {
    // Same SDK leaf/config/options as the known builtin factory. Construct it
    // once with delegates; never replace an already selected mutable product.
    auto config = webrtc::AudioEncoderOpus::SdpToConfig(native);
    if (config && config->IsOk()) state->encoder = CreateOwnedOpus(*state, std::move(*config), options.payload_type);
  } else state->encoder = factory.factory()->Create(state->environment, native, options);
  if (state->encoder) result.encoder = std::make_unique<NativeAudioEncoder>(std::move(state));
  return result;
}
FfiAudioEncoderCreation audio_encoder_create(const NativeAudioEncoderFactory& factory,
    const NativeEnvironment& environment, const FfiSdpAudioFormat& format, std::int32_t payload_type,
    const std::unique_ptr<NativeAudioCodecPairId>& pair_id) noexcept {
  webrtc::AudioEncoderFactory::Options options;
  options.payload_type = payload_type;
  if (pair_id) options.codec_pair_id = pair_id->value();
  return CreateAudioEncoderWithOptions(factory, environment, format, options);
}
FfiAudioEncoderCreation audio_encoder_create_with_options(const NativeAudioEncoderFactory& factory,
    const NativeEnvironment& environment, const FfiSdpAudioFormat& format,
    const NativeAudioEncoderOptions& options) noexcept {
  // Pass an actual SDK value copy into Create, including exact optional ID.
  // Reuse only admission/product ownership, not a Rust field projection.
  return CreateAudioEncoderWithOptions(factory, environment, format, options.value());
}
void audio_encoder_default_allocation_arguments(std::int64_t target, std::int8_t kind,
    std::int64_t period, bool& valid, std::int32_t& bitrate, std::int64_t& milliseconds) noexcept {
  valid = false;
  if (kind < -1 || kind > 1) return;
#if RTC_DCHECK_IS_ON
  // DataRate is one-sided and integral getters DCHECK finite values. Do not
  // convert those assertions into an unconditional release-artifact policy.
  if (kind != 0 || target < 0 || target == INT64_MAX ||
      period == INT64_MIN || period == INT64_MAX) return;
#endif
  const auto rate = kind == 1 ? webrtc::DataRate::PlusInfinity() :
      kind == -1 ? webrtc::DataRate::MinusInfinity() : webrtc::DataRate::BitsPerSec(target);
  const auto delta = period == INT64_MIN ? webrtc::TimeDelta::MinusInfinity() :
      period == INT64_MAX ? webrtc::TimeDelta::PlusInfinity() : webrtc::TimeDelta::Micros(period);
  // Same getters and implicit int64->int conversion as the pinned base method.
  bitrate = rate.bps();
  milliseconds = delta.ms();
  valid = true;
}
bool audio_encoder_adaptor_smoothing_inputs_safe(std::int32_t bitrate,
    bool has_period, std::int64_t period) noexcept {
  // Input preflight only. Native clock, initialization/libm, controller arithmetic
  // and every consuming operation must be admitted separately before enablement.
  return AnaSmoothingContextSafe() && AnaBitrateSampleSafe(bitrate) && AnaBandwidthPeriodSafe(has_period, period);
}
bool audio_encoder_adaptor_initialization_safe(std::int32_t duration,
    std::uint64_t clock_span_ms) noexcept {
  // Caller-supplied span is not evidence of a native clock's lifetime behavior.
  return AnaSmoothingInitializationSafe(duration, clock_span_ms);
}
FfiAudioEncoderBitrateProbe audio_encoder_adaptor_bitrate_probe(std::int32_t initial_frame,
    std::int32_t increase, std::int32_t decrease,
    rust::Slice<const FfiAudioEncoderBitrateStep> steps) noexcept {
  FfiAudioEncoderBitrateProbe result{};
  if (initial_frame <= 0) {
    result.status = static_cast<std::uint8_t>(AnaBitrateFailure::Construction);
    return result;
  }
  const webrtc::audio_network_adaptor::BitrateController::Config construction(
      6000, initial_frame, increase, decrease);
  webrtc::audio_network_adaptor::BitrateController native(construction);
  auto runtime = std::make_shared<AnaBitrateRuntime>();
  GuardedAnaBitrateController controller(native, construction, runtime);
  for (const auto& step : steps) {
    webrtc::Controller::NetworkMetrics metrics;
    if (step.has_target) metrics.target_audio_bitrate_bps = step.target;
    if (step.has_overhead) metrics.overhead_bytes_per_packet = step.overhead;
    controller.UpdateNetworkMetrics(metrics);
    webrtc::AudioEncoderRuntimeConfig config;
    if (step.has_frame) config.frame_length_ms = step.frame;
    config.last_fl_change_increase = step.increasing;
    if (step.occupied) config.bitrate_bps = 0;
    controller.MakeDecision(&config);
    if (!runtime->healthy()) break;
    if (config.bitrate_bps) result.bitrates.push_back(*config.bitrate_bps);
  }
  result.status = static_cast<std::uint8_t>(runtime->failure);
  return result;
}
FfiAudioEncoderSmoothingProbe audio_encoder_adaptor_smoothing_probe(
    const NativeAudioEncoder& encoder, std::int32_t first_sample, std::int32_t sample,
    std::int64_t first_time_us, std::int64_t sample_time_us, std::int64_t average_time_us,
    bool has_duration, std::int32_t duration) noexcept {
  FfiAudioEncoderSmoothingProbe result{};
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  if (!state.ana_clock_evidence) {
    result.status = static_cast<std::uint8_t>(AnaSmoothingFailure::Clock);
    return result;
  }
  auto runtime = std::make_shared<AnaSmoothingRuntime>(*state.ana_clock_evidence);
  auto filter = BoundedAnaBitrateSmoother::Create(runtime);
  if (filter) {
    filter->AddSample(static_cast<float>(first_sample), webrtc::Timestamp::Micros(first_time_us));
    filter->GetAverage(webrtc::Timestamp::Micros(sample_time_us));
    if (has_duration) result.duration_applied = filter->SetTimeConstantMs(duration);
    filter->AddSample(static_cast<float>(sample), webrtc::Timestamp::Micros(sample_time_us));
    const auto average = filter->GetAverage(webrtc::Timestamp::Micros(average_time_us));
    result.has_average = average.has_value();
    if (average) result.average = *average;
  }
  result.status = static_cast<std::uint8_t>(runtime->failure);
  return result;
}
std::uint8_t audio_encoder_check_network_adaptor_scoring(const NativeAudioEncoder& encoder,
    rust::Slice<const std::uint8_t> bytes, bool has_bandwidth, std::int32_t bandwidth,
    bool has_loss, float loss) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
#if WEBRTC_ENABLE_PROTOBUF
  if (state.opus) {
    const auto& opus = static_cast<const webrtc::AudioEncoderOpusImpl&>(*state.encoder);
    const auto admission = AdmitAudioNetworkAdaptorConstruction(opus, bytes);
    if (!admission) return 1;
    return AnaScoringOrderSafe(*admission,
        has_bandwidth ? std::optional<int>(bandwidth) : std::nullopt,
        has_loss ? std::optional<float>(loss) : std::nullopt) ? 0 : 2;
  }
#endif
  return 3;
}
std::uint8_t audio_encoder_check_network_adaptor_smoothing(const NativeAudioEncoder& encoder,
    rust::Slice<const std::uint8_t> bytes) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
#if WEBRTC_ENABLE_PROTOBUF
  if (state.opus) {
    const auto& opus = static_cast<const webrtc::AudioEncoderOpusImpl&>(*state.encoder);
    const auto admission = AdmitAudioNetworkAdaptorConstruction(opus, bytes);
    if (!admission) return 1;
    const auto& clock = state.ana_clock_evidence;
    if (!clock) return 2;
    const auto check = [&](int duration) {
      return clock->admits_duration(duration) && AnaSmoothingInitializationSafe(duration, clock->span_ms);
    };
    if (!check(5000) || (admission->fec_time_constant_ms && !check(*admission->fec_time_constant_ms))) return 2;
    return 0;
  }
#endif
  return 3;
}
std::uint8_t audio_encoder_check_network_adaptor_arithmetic(const NativeAudioEncoder& encoder,
    rust::Slice<const std::uint8_t> bytes, bool has_overhead, std::size_t overhead,
    bool has_target, std::int32_t target, bool has_uplink, std::int32_t uplink_min,
    std::int32_t uplink_max) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
#if WEBRTC_ENABLE_PROTOBUF
  if (state.opus) {
    const auto& opus = static_cast<const webrtc::AudioEncoderOpusImpl&>(*state.encoder);
    const auto admission = AdmitAudioNetworkAdaptorConstruction(opus, bytes);
    if (!admission) return 1;
    return AnaControllerArithmeticSafe(*admission,
        has_overhead ? std::optional<std::size_t>(overhead) : std::nullopt,
        has_target ? std::optional<int>(target) : std::nullopt,
        has_uplink ? std::optional<std::pair<int, int>>(std::pair<int, int>{uplink_min, uplink_max}) : std::nullopt) ? 0 : 2;
  }
#endif
  return 3;
}
std::uint8_t audio_encoder_check_network_adaptor_config(const NativeAudioEncoder& encoder,
    rust::Slice<const std::uint8_t> bytes) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
#if WEBRTC_ENABLE_PROTOBUF
  if (state.opus) {
    const auto& opus = static_cast<const webrtc::AudioEncoderOpusImpl&>(*state.encoder);
    return AdmitAudioNetworkAdaptorConstruction(opus, bytes) ? 0 : 1;
  }
#endif
  // Do not probe ControllerManager::Create in a protobuf-disabled artifact:
  // that path asserts rather than safely reporting its unavailable capability.
  return 3;
}
FfiAudioEncoderInfo audio_encoder_info(const NativeAudioEncoder& encoder) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  const auto& value = *state.encoder;
  return {value.SampleRateHz(), value.NumChannels(), value.RtpTimestampRateHz(),
      value.Num10MsFramesInNextPacket(), value.Max10MsFramesInAPacket(),
      value.GetTargetBitrate(), value.GetDtx()};
}

FfiAudioEncoderRange audio_encoder_frame_length_range(const NativeAudioEncoder& encoder) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  FfiAudioEncoderRange result{};
  const auto range = state.encoder->GetFrameLengthRange();
  if (!range) return result;
  result.present = true;
  result.minimum = range->first.IsMinusInfinity() ? INT64_MIN : range->first.us_or(INT64_MAX);
  result.maximum = range->second.IsMinusInfinity() ? INT64_MIN : range->second.us_or(INT64_MAX);
  return result;
}
FfiAudioEncoderRange audio_encoder_bitrate_range(const NativeAudioEncoder& encoder) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  FfiAudioEncoderRange result{};
  const auto range = state.encoder->GetBitrateRange();
  if (!range) return result;
  result.present = true;
  result.minimum = range->first.bps_or(0);
  result.maximum = range->second.bps_or(0);
  result.minimum_kind = range->first.IsMinusInfinity() ? -1 : range->first.IsPlusInfinity() ? 1 : 0;
  result.maximum_kind = range->second.IsMinusInfinity() ? -1 : range->second.IsPlusInfinity() ? 1 : 0;
  return result;
}
FfiAudioEncoderStatistics audio_encoder_statistics(const NativeAudioEncoder& encoder) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  const auto stats = state.encoder->GetANAStats();
  FfiAudioEncoderStatistics result{};
  const std::array<std::optional<std::uint32_t>, 6> counters = {
      stats.bitrate_action_counter, stats.channel_action_counter, stats.dtx_action_counter,
      stats.fec_action_counter, stats.frame_length_increase_counter, stats.frame_length_decrease_counter};
  for (std::size_t i = 0; i < counters.size(); ++i) {
    if (counters[i]) {
      result.present_mask |= 1u << i;
      result.counters[i] = *counters[i];
    }
  }
  result.loss_present = stats.uplink_packet_loss_fraction.has_value();
  result.loss = stats.uplink_packet_loss_fraction.value_or(0.0f);
  return result;
}

namespace {
bool AudioEncoderTargetBitrateSafe(const NativeAudioEncoder::State& state, int target) {
  if (!state.opus) return true;
  const int bitrate = std::clamp(target, 6000, 510000);
  if (bitrate == state.encoder->GetTargetBitrate()) return true;
  const std::size_t index = bitrate / 1000 - 5;
  if (index >= state.bitrate_multipliers.size()) return true;
  const float multiplied = state.bitrate_multipliers[index] * bitrate;
  // float(INT_MAX) rounds up; use the exclusive upper endpoint before casting.
  if (!std::isfinite(multiplied) || multiplied < -2147483648.0f || multiplied >= 2147483648.0f) return false;
  const int native = static_cast<int>(multiplied);
  return native > 0 || native == -1 || native == -1000;  // Libopus MAX/AUTO.
}
bool AudioEncoderBandwidthSafe(const NativeAudioEncoder::State& state, int bitrate) {
  if (!state.opus || !state.overhead_bytes) return true;
  // Match native size_t promotion/wrapping and the selected compiler's cast.
  // Only the following signed subtraction needs a representability guard.
  const auto frames = state.encoder->Num10MsFramesInNextPacket();
  if (frames == 0) return false;
  const int overhead = static_cast<int>(*state.overhead_bytes * 8 * 100 / frames);
  const auto target = static_cast<std::int64_t>(bitrate) - overhead;
  return target >= INT32_MIN && target <= INT32_MAX &&
      AudioEncoderTargetBitrateSafe(state, static_cast<int>(target));
}
FfiAudioEncodedInfoLeaf FromNative(const webrtc::AudioEncoder::EncodedInfoLeaf& value) {
  return {value.encoded_bytes, value.encoded_timestamp, value.payload_type,
      value.send_even_if_empty, value.speech, static_cast<std::int32_t>(value.encoder_type)};
}
}  // namespace
FfiAudioEncoderOutput audio_encoder_encode(const NativeAudioEncoder& encoder, std::uint32_t timestamp,
    rust::Slice<const std::int16_t> audio) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  FfiAudioEncoderOutput result{};
  result.status = 1;
  const int sample_rate = state.encoder->SampleRateHz();
  const auto channels = state.encoder->NumChannels();
  if (sample_rate <= 0 || channels == 0 || channels > std::numeric_limits<std::size_t>::max() /
      static_cast<std::size_t>(sample_rate) || audio.size() != channels * sample_rate / 100) return result;
  AnaOperation operation(state, 10);
  webrtc::Buffer encoded;
  const auto info = state.encoder->Encode(timestamp, std::span<const std::int16_t>(audio.data(), audio.size()), &encoded);
  if (const auto error = operation.error()) { result.status = error == 2 ? 4 : 3; return result; }
  result.status = 2;
  if (info.encoded_bytes != encoded.size()) return result;
  if (!info.redundant.empty()) {
    std::size_t total = 0;
    for (const auto& leaf : info.redundant) {
      if (leaf.encoded_bytes > encoded.size() - total) return result;
      total += leaf.encoded_bytes;
    }
    if (total != info.encoded_bytes) return result;
  }
  result.bytes.reserve(encoded.size());
  for (std::size_t i = 0; i < encoded.size(); ++i) result.bytes.push_back(encoded.data()[i]);
  result.primary = FromNative(static_cast<const webrtc::AudioEncoder::EncodedInfoLeaf&>(info));
  result.redundant.reserve(info.redundant.size());
  for (const auto& leaf : info.redundant) result.redundant.push_back(FromNative(leaf));
  result.status = 0;
  return result;
}
void audio_encoder_reset(const NativeAudioEncoder& encoder) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  state.encoder->Reset();
}
bool audio_encoder_set_fec(const NativeAudioEncoder& encoder, bool enable) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  return state.encoder->SetFec(enable);
}
bool audio_encoder_set_dtx(const NativeAudioEncoder& encoder, bool enable) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  return state.encoder->SetDtx(enable);
}
bool audio_encoder_set_application(const NativeAudioEncoder& encoder, bool speech) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  return state.encoder->SetApplication(speech ? webrtc::AudioEncoder::Application::kSpeech : webrtc::AudioEncoder::Application::kAudio);
}
bool audio_encoder_enable_network_adaptor(const NativeAudioEncoder& encoder,
    rust::Slice<const std::uint8_t> bytes, std::uint8_t& error) noexcept {
  auto& state = encoder.state(); std::lock_guard lock(state.mutex); error = 0;
  const absl::string_view config(reinterpret_cast<const char*>(bytes.data()), bytes.size());
  if (!state.opus) return state.encoder->EnableAudioNetworkAdaptor(config);
#if WEBRTC_ENABLE_PROTOBUF
  if (!state.ana_clock_evidence) { error = 2; return false; }
  if (!state.ana || !state.ana->capable || !AnaSmoothingContextSafe()) { error = 4; return false; }
  auto admission = AdmitAudioNetworkAdaptorConstruction(
      static_cast<const webrtc::AudioEncoderOpusImpl&>(*state.encoder), bytes);
  if (!admission) { error = 1; return false; }
  for (int duration : admission->fec_time_constants_ms) {
    // FEC's native smoother performs only nontrapping FP before returning its
    // actual loss estimate. Protect its integral clock arithmetic here, then
    // admit the actual estimate before native stats/event-log consumers. Do not
    // reject an otherwise usable duration by predicting its future average.
    if (!state.ana_clock_evidence->admits_duration(duration)) { error = 4; return false; }
  }
  AnaOperation operation(state, 7);
  state.ana->pending = std::make_shared<AudioNetworkAdaptorAdmission>(std::move(*admission));
  const bool enabled = state.encoder->EnableAudioNetworkAdaptor(config);
  state.ana->pending.reset(); state.ana->active = enabled;
  error = operation.error(); return enabled;
#else
  error = 3; return false;  // The disabled native factory asserts; do not enter it.
#endif
}
void audio_encoder_disable_network_adaptor(const NativeAudioEncoder& encoder) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  state.encoder->DisableAudioNetworkAdaptor();
  if (state.ana) { state.ana->active = false; state.ana->controllers.reset(); }
}
void audio_encoder_set_max_playback_rate(const NativeAudioEncoder& encoder, std::int32_t frequency) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  state.encoder->SetMaxPlaybackRate(frequency);
}

bool audio_encoder_set_target_bitrate(const NativeAudioEncoder& encoder, std::int32_t bitrate) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  if (!AudioEncoderTargetBitrateSafe(state, bitrate)) return false;
  state.encoder->SetTargetBitrate(bitrate);
  return true;
}
bool audio_encoder_on_target_bitrate(const NativeAudioEncoder& encoder, std::int32_t bitrate) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  if (!AudioEncoderTargetBitrateSafe(state, bitrate)) return false;
  state.encoder->OnReceivedTargetAudioBitrate(bitrate);
  return true;
}
bool audio_encoder_on_bandwidth(const NativeAudioEncoder& encoder, std::int32_t bitrate,
    bool has_period, std::int64_t period) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  const bool active = state.ana && state.ana->active;
  if (active ? (has_period && (period < INT64_MIN / 4 || period > INT64_MAX / 4)) : !AudioEncoderBandwidthSafe(state, bitrate)) return false;
  AnaOperation operation(state, 3);
  state.encoder->OnReceivedUplinkBandwidth(bitrate, has_period ? std::optional<std::int64_t>(period) : std::nullopt);
  return operation.error() == 0;
}
bool audio_encoder_on_allocation(const NativeAudioEncoder& encoder, const FfiBitrateAllocationUpdate& update) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  // Both pinned native allocation implementations narrow target to int and
  // query bwe_period as integral milliseconds, even for otherwise no-op codecs.
  if (update.target_kind != 0 || update.target_bitrate < 0 || update.target_bitrate > INT32_MAX ||
      update.bwe_period == INT64_MIN || update.bwe_period == INT64_MAX ||
      (!(state.ana && state.ana->active) && !AudioEncoderBandwidthSafe(state, static_cast<int>(update.target_bitrate)))) return false;
  const auto delta = [](std::int64_t value) {
    if (value == INT64_MIN) return webrtc::TimeDelta::MinusInfinity();
    if (value == INT64_MAX) return webrtc::TimeDelta::PlusInfinity();
    return webrtc::TimeDelta::Micros(value);
  };
  webrtc::BitrateAllocationUpdate native;
  native.target_bitrate = webrtc::DataRate::BitsPerSec(update.target_bitrate);
  native.packet_loss_ratio = update.packet_loss_ratio;
  native.round_trip_time = delta(update.round_trip_time);
  native.bwe_period = delta(update.bwe_period);
  native.cwnd_reduce_ratio = update.cwnd_reduce_ratio;
  AnaOperation operation(state, 4);
  state.encoder->OnReceivedUplinkAllocation(native);
  return operation.error() == 0;
}
bool audio_encoder_on_overhead(const NativeAudioEncoder& encoder, std::size_t overhead) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  AnaOperation operation(state, 11);
  state.encoder->OnReceivedOverhead(overhead);
  if (state.opus && !(state.ana && state.ana->active)) state.overhead_bytes = overhead;
  return operation.error() == 0;
}
std::uint8_t audio_encoder_on_packet_loss(const NativeAudioEncoder& encoder, float fraction) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  if (state.opus) {
    if (!std::isfinite(fraction) || fraction < 0 || fraction > 1) return 1;
    // This smoother subtracts native microsecond timestamps before conversion,
    // unlike ANA's millisecond smoothers. Require the stronger retained domain.
    const auto& clock = state.ana_clock_evidence;
    if (!clock || !clock->packet_loss_deltas_safe) return 2;
  }
  AnaOperation operation(state, 5);
  state.encoder->OnReceivedUplinkPacketLossFraction(fraction);
  return operation.error() ? (operation.error() == 2 ? 2 : 3) : 0;
}
bool audio_encoder_on_rtt(const NativeAudioEncoder& encoder, std::int32_t rtt) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
  AnaOperation operation(state, 12);
  state.encoder->OnReceivedRtt(rtt);
  return operation.error() == 0;
}
bool audio_encoder_set_receiver_frame_length_range(const NativeAudioEncoder& encoder,
    std::int32_t minimum, std::int32_t maximum) noexcept {
  auto& state = encoder.state();
  std::lock_guard lock(state.mutex);
#if RTC_DCHECK_IS_ON
  if (state.ana && state.ana->active) return false;
#endif
  state.encoder->SetReceiverFrameLengthRange(minimum, maximum);
  return true;
}

namespace {
using AudioEncoderRetention = std::shared_ptr<rust::Box<AudioEncoderTransfer>>;
struct InstalledAudioEncoderContext {
  std::shared_ptr<NativeProductionLease> production_clock_lease;
  std::shared_ptr<rust::Box<RustVideoFactoryEnvironment>> rust_dependencies;
  std::unique_ptr<NativeEnvironment> environment;
  std::unique_ptr<NativeAudioEncoderFactory> selected;
  std::shared_ptr<NativeAudioEncoderFailureObserver::State> failure =
      std::make_shared<NativeAudioEncoderFailureObserver::State>();
};
class RetainedAudioEncoder final : public webrtc::AudioEncoder {
 public:
  explicit RetainedAudioEncoder(AudioEncoderRetention owner) : owner_(std::move(owner)) {}
  RetainedAudioEncoder(std::unique_ptr<NativeAudioEncoder> owner,
      std::shared_ptr<InstalledAudioEncoderContext> context)
      : installed_context_(std::move(context)), installed_owner_(std::move(owner)) {}
  const NativeAudioEncoder& original() const {
    return installed_owner_ ? *installed_owner_ : audio_encoder_transfer_native(**owner_);
  }
  int SampleRateHz() const override { auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->SampleRateHz(); }
  std::size_t NumChannels() const override { auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->NumChannels(); }
  int RtpTimestampRateHz() const override { auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->RtpTimestampRateHz(); }
  std::size_t Num10MsFramesInNextPacket() const override { auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->Num10MsFramesInNextPacket(); }
  std::size_t Max10MsFramesInAPacket() const override { auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->Max10MsFramesInAPacket(); }
  int GetTargetBitrate() const override { auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->GetTargetBitrate(); }
  bool GetDtx() const override { auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->GetDtx(); }
  void Reset() override { audio_encoder_reset(original()); }
  bool SetFec(bool enabled) override { return audio_encoder_set_fec(original(), enabled); }
  bool SetDtx(bool enabled) override { return audio_encoder_set_dtx(original(), enabled); }
  bool SetApplication(Application application) override {
    auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->SetApplication(application);
  }
  void SetMaxPlaybackRate(int frequency) override { audio_encoder_set_max_playback_rate(original(), frequency); }
  void SetTargetBitrate(int bitrate) override {
    if (!audio_encoder_set_target_bitrate(original(), bitrate)) Reject(1, 1);
  }
  void OnReceivedTargetAudioBitrate(int bitrate) override {
    if (!audio_encoder_on_target_bitrate(original(), bitrate)) Reject(2, 1);
  }
  void OnReceivedUplinkBandwidth(int bitrate, std::optional<std::int64_t> period) override {
    if (!audio_encoder_on_bandwidth(original(), bitrate, period.has_value(), period.value_or(0))) Reject(3, 1);
  }
  void OnReceivedUplinkAllocation(webrtc::BitrateAllocationUpdate update) override {
    const auto delta = [](webrtc::TimeDelta value) {
      return value.IsMinusInfinity() ? INT64_MIN : value.us_or(INT64_MAX);
    };
    FfiBitrateAllocationUpdate value{};
    value.target_kind = update.target_bitrate.IsMinusInfinity() ? -1 : update.target_bitrate.IsPlusInfinity() ? 1 : 0;
    value.target_bitrate = update.target_bitrate.bps_or(0);
    value.packet_loss_ratio = update.packet_loss_ratio;
    value.round_trip_time = delta(update.round_trip_time);
    value.bwe_period = delta(update.bwe_period);
    value.cwnd_reduce_ratio = update.cwnd_reduce_ratio;
    if (!audio_encoder_on_allocation(original(), value)) Reject(4, 1);
  }
  void OnReceivedUplinkPacketLossFraction(float fraction) override {
    const auto status = audio_encoder_on_packet_loss(original(), fraction);
    if (status != 0) Reject(5, status == 2 ? 2 : status == 3 ? 4 : 1);
  }
  void OnReceivedUplinkRecoverablePacketLossFraction(float) override {
    // No currently constructible selected backend overrides the aborting SDK
    // default. Do not invoke it or pretend an unimplemented control succeeded.
    Reject(6, 3);
  }
  bool EnableAudioNetworkAdaptor(absl::string_view config) override {
    std::uint8_t error = 0;
    const auto* bytes = reinterpret_cast<const std::uint8_t*>(config.data());
    const std::uint8_t empty = 0;
    const bool enabled = audio_encoder_enable_network_adaptor(original(),
        rust::Slice<const std::uint8_t>(config.empty() ? &empty : bytes, config.size()), error);
    if (error) Reject(7, error);
    return enabled && !error;
  }
  void DisableAudioNetworkAdaptor() override { audio_encoder_disable_network_adaptor(original()); }
  void OnReceivedRtt(int rtt) override { if (!audio_encoder_on_rtt(original(), rtt)) Reject(12, 4); }
  void OnReceivedOverhead(std::size_t bytes) override { if (!audio_encoder_on_overhead(original(), bytes)) Reject(11, 4); }
  void SetReceiverFrameLengthRange(int minimum, int maximum) override { if (!audio_encoder_set_receiver_frame_length_range(original(), minimum, maximum)) Reject(13, 1); }
  webrtc::ANAStats GetANAStats() const override { auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->GetANAStats(); }
  std::optional<std::pair<webrtc::TimeDelta, webrtc::TimeDelta>> GetFrameLengthRange() const override {
    auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->GetFrameLengthRange();
  }
  std::optional<std::pair<webrtc::DataRate, webrtc::DataRate>> GetBitrateRange() const override {
    auto& s = original().state(); std::lock_guard lock(s.mutex); return s.encoder->GetBitrateRange();
  }
  // ReclaimContainedEncoders is explicitly forbidden by the pinned header.
  // Neither call nor override it in this adapter.
 protected:
  EncodedInfo EncodeImpl(std::uint32_t timestamp, std::span<const std::int16_t> audio,
      webrtc::Buffer* encoded) override {
    auto& state = original().state();
    std::lock_guard lock(state.mutex);
    // Preserve the actual caller Buffer append and original EncodedInfo,
    // including redundant leaves. Do not reconstruct it through FFI values.
    AnaOperation operation(state, 10);
    auto result = state.encoder->Encode(timestamp, audio, encoded);
    operation.error();  // Retain ANA-side refusal; actual stock payload/metadata stay genuine.
    return result;
  }
 private:
  void Reject(std::uint8_t operation, std::uint8_t error) const {
    std::uint32_t empty = 0;
    original().state().sdk_failure->first.compare_exchange_strong(empty,
        (std::uint32_t{operation} << 8) | error, std::memory_order_acq_rel);
  }
  // Destroy the exact mutable product before its native/Rust dependency leases.
  const std::shared_ptr<InstalledAudioEncoderContext> installed_context_;
  const std::unique_ptr<NativeAudioEncoder> installed_owner_;
  AudioEncoderRetention owner_;
};
} // namespace
std::unique_ptr<webrtc::AudioEncoder> audio_encoder_forward_to_sdk(
    rust::Box<AudioEncoderTransfer> owner) noexcept {
  return std::make_unique<RetainedAudioEncoder>(
      std::make_shared<rust::Box<AudioEncoderTransfer>>(std::move(owner)));
}
std::unique_ptr<NativeAudioEncoder> audio_encoder_forward_product(
    rust::Box<AudioEncoderTransfer> owner) noexcept {
  const auto& original = audio_encoder_transfer_native(*owner);
  auto env = original.state().environment;
  auto forwarded = audio_encoder_forward_to_sdk(std::move(owner));
  auto state = std::make_unique<NativeAudioEncoder::State>(std::move(env), std::move(forwarded),
      false, std::vector<float>{}, FfiClockTimeBounds{});
  state->forwarded_origin = &original; // Retained by the SDK envelope, not a registry.
  return std::make_unique<NativeAudioEncoder>(std::move(state));
}

namespace {
class InstalledAudioEncoderFactory : public webrtc::AudioEncoderFactory {
 public:
  explicit InstalledAudioEncoderFactory(std::shared_ptr<InstalledAudioEncoderContext> context)
      : context_(std::move(context)) {}
  std::vector<webrtc::AudioCodecSpec> GetSupportedEncoders() override {
    return context_->selected->factory()->GetSupportedEncoders();
  }
  std::optional<webrtc::AudioCodecInfo> QueryAudioEncoder(const webrtc::SdpAudioFormat& format) override {
    const auto query = audio_encoder_query(*context_->selected, FromNative(format));
    if (!query.valid) Observe(8, 1);
    return query.present ? std::optional(AudioInfo(query.info)) : std::nullopt;
  }
  std::unique_ptr<webrtc::AudioEncoder> Create(const webrtc::Environment& env,
      const webrtc::SdpAudioFormat& format, Options options) override {
    auto environment = retain_environment_from_sdk(env, context_->environment.get());
    auto bounds = environment_clock_time_bounds(*environment);
    // A separate retaining production lease proves the actual SDK singleton's
    // monotonic domain through final product/dependency destruction. Caller
    // clocks retain their own identity-checked evidence without another read.
    if (context_->production_clock_lease && bounds.known &&
        &env.clock() == &context_->environment->environment().clock() &&
        &env.clock() == webrtc::Clock::GetRealTimeClock())
      bounds.monotonic = true;
    auto created = CreateAudioEncoderWithOptions(*context_->selected, *environment,
        FromNative(format), options, bounds);
    if (!created.valid) { Observe(9, 1); return nullptr; }
    if (!created.encoder) return nullptr;
    created.encoder->state().sdk_failure = context_->failure;
    return std::make_unique<RetainedAudioEncoder>(std::move(created.encoder), context_);
  }
 private:
  void Observe(std::uint32_t operation, std::uint32_t error) {
    std::uint32_t empty = 0;
    context_->failure->first.compare_exchange_strong(empty, operation << 8 | error);
  }
  const std::shared_ptr<InstalledAudioEncoderContext> context_;
};
}  // namespace

struct NativeInstalledAudioEncoderFactory::State {
  std::shared_ptr<NativeAudioEncoderFailureObserver::State> failure;
  webrtc::scoped_refptr<webrtc::AudioEncoderFactory> factory;
};
NativeInstalledAudioEncoderFactory::NativeInstalledAudioEncoderFactory(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeInstalledAudioEncoderFactory::~NativeInstalledAudioEncoderFactory() = default;
webrtc::scoped_refptr<webrtc::AudioEncoderFactory>
NativeInstalledAudioEncoderFactory::factory() const noexcept { return state_->factory; }

std::unique_ptr<NativeInstalledAudioEncoderFactory> audio_installed_encoder_factory(
    const NativeAudioEncoderFactory* selected, const NativeEnvironment& environment,
    std::shared_ptr<rust::Box<RustVideoFactoryEnvironment>> dependencies,
    bool production_system_clock) noexcept {
  auto context = std::make_shared<InstalledAudioEncoderContext>();
  context->rust_dependencies = std::move(dependencies);
  context->environment = clone_environment(environment);
  context->selected = selected
      ? std::make_unique<NativeAudioEncoderFactory>(
            std::make_unique<NativeAudioEncoderFactory::State>(selected->state()))
      : new_builtin_audio_encoder_factory();
  if (production_system_clock) {
    context->production_clock_lease = new_production_lease();
    if (!context->production_clock_lease) return nullptr;
  }
  auto state = std::make_unique<NativeInstalledAudioEncoderFactory::State>();
  state->failure = context->failure;
  state->factory = webrtc::make_ref_counted<InstalledAudioEncoderFactory>(std::move(context));
  return std::make_unique<NativeInstalledAudioEncoderFactory>(std::move(state));
}
std::unique_ptr<NativeAudioEncoderFailureObserver> audio_installed_encoder_failure_observer(
    const NativeInstalledAudioEncoderFactory& factory) noexcept {
  return std::make_unique<NativeAudioEncoderFailureObserver>(factory.state()->failure);
}

namespace {
class CallerAudioEncoderFactory : public webrtc::AudioEncoderFactory {
 public:
  explicit CallerAudioEncoderFactory(rust::Box<RustAudioEncoderFactoryCall> source) : source_(std::move(source)) {}
  // Owner serialization and Rust SDK-scope reentry admission precede this
  // stack borrow. No scoped request/environment/receipt survives the operation.
  class CallScope {
   public:
    CallScope(CallerAudioEncoderFactory& factory, const RustAudioEncoderFactoryCall& source)
        : factory_(factory), previous_(factory.active_) { factory_.active_ = &source; }
    ~CallScope() { factory_.active_ = previous_; }
    CallScope(const CallScope&) = delete;
    CallScope& operator=(const CallScope&) = delete;
   private:
    CallerAudioEncoderFactory& factory_;
    const RustAudioEncoderFactoryCall* previous_;
  };
  std::vector<webrtc::AudioCodecSpec> GetSupportedEncoders() override {
    std::vector<webrtc::AudioCodecSpec> result;
    for (const auto& value : audio_caller_encoder_formats_callback(source())) {
      result.push_back({ToNative(value.format), AudioInfo(value.info)});
    }
    return result;
  }
  std::optional<webrtc::AudioCodecInfo> QueryAudioEncoder(const webrtc::SdpAudioFormat& format) override {
    const auto value = audio_caller_encoder_query_callback(source(), FromNative(format));
    if (!value.valid || !value.present) return std::nullopt;
    return AudioInfo(value.info);
  }
  std::unique_ptr<webrtc::AudioEncoder> Create(const webrtc::Environment& environment,
      const webrtc::SdpAudioFormat& format, Options options) override {
    // Do not Query or normalize the caller's independent Create disposition.
    auto selected = audio_caller_encoder_environment(source());
    auto product = audio_caller_encoder_create_callback(source(),
        retain_environment_from_sdk(environment, selected.get()), FromNative(format),
        std::make_unique<NativeAudioEncoderOptions>(std::move(options)));
    if (!product) return nullptr; // Explicit Rust receipt distinguishes failures.
    if (!product->forwarding()) {
      audio_caller_encoder_transfer_failed(source());
      return nullptr;
    }
    return product->take_sdk_encoder(); // Move envelope, retain original inside it.
  }
 private:
  const RustAudioEncoderFactoryCall& source() const { return active_ ? *active_ : *source_; }
  rust::Box<RustAudioEncoderFactoryCall> source_;
  const RustAudioEncoderFactoryCall* active_ = nullptr;
};
std::unique_ptr<NativeAudioEncoder> WrapCallerEncoder(std::unique_ptr<webrtc::AudioEncoder> encoder) {
  if (!encoder) return nullptr;
  // This private factory only returns RetainedAudioEncoder, not a guessed
  // concrete codec. Keep its directly owned original state/provenance intact.
  const auto& original = static_cast<const RetainedAudioEncoder&>(*encoder).original();
  auto env = original.state().environment;
  auto state = std::make_unique<NativeAudioEncoder::State>(std::move(env), std::move(encoder),
      false, std::vector<float>{}, FfiClockTimeBounds{});
  state->forwarded_origin = &original;
  return std::make_unique<NativeAudioEncoder>(std::move(state));
}
} // namespace
rust::Vec<FfiAudioCodecSpec> audio_caller_encoder_formats(rust::Box<RustAudioEncoderFactoryCall> source) noexcept {
  auto factory = webrtc::make_ref_counted<CallerAudioEncoderFactory>(std::move(source));
  rust::Vec<FfiAudioCodecSpec> result;
  for (const auto& value : factory->GetSupportedEncoders()) result.push_back({FromNative(value.format), FromNative(value.info)});
  return result;
}
FfiAudioEncoderQuery audio_caller_encoder_query(rust::Box<RustAudioEncoderFactoryCall> source,
    const FfiSdpAudioFormat& format) noexcept {
  auto factory = webrtc::make_ref_counted<CallerAudioEncoderFactory>(std::move(source));
  FfiAudioEncoderQuery result{};
  const auto info = factory->QueryAudioEncoder(ToNative(format));
  result.valid = true; // Callback error is held in the explicit per-call receipt.
  if (info) { result.present = true; result.info = FromNative(*info); }
  return result;
}
std::unique_ptr<NativeAudioEncoder> audio_caller_encoder_create(rust::Box<RustAudioEncoderFactoryCall> source,
    const NativeEnvironment& environment, const FfiSdpAudioFormat& format,
    const NativeAudioEncoderOptions& options) noexcept {
  auto factory = webrtc::make_ref_counted<CallerAudioEncoderFactory>(std::move(source));
  return WrapCallerEncoder(factory->Create(environment.environment(), ToNative(format), options.value()));
}
struct NativeCallerAudioEncoderFactory::State {
  explicit State(rust::Box<RustAudioEncoderFactoryCall> source)
      : factory(webrtc::make_ref_counted<CallerAudioEncoderFactory>(std::move(source))) {}
  webrtc::scoped_refptr<CallerAudioEncoderFactory> factory;
};
NativeCallerAudioEncoderFactory::NativeCallerAudioEncoderFactory(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeCallerAudioEncoderFactory::~NativeCallerAudioEncoderFactory() = default;
std::unique_ptr<NativeCallerAudioEncoderFactory> audio_caller_encoder_factory_new(rust::Box<RustAudioEncoderFactoryCall> source) noexcept {
  return std::make_unique<NativeCallerAudioEncoderFactory>(std::make_unique<NativeCallerAudioEncoderFactory::State>(std::move(source)));
}
rust::Vec<FfiAudioCodecSpec> audio_caller_encoder_factory_formats(const NativeCallerAudioEncoderFactory& handle,
    const RustAudioEncoderFactoryCall& source) noexcept {
  auto& factory = *handle.state()->factory;
  CallerAudioEncoderFactory::CallScope call(factory, source);
  rust::Vec<FfiAudioCodecSpec> result;
  for (const auto& value : factory.GetSupportedEncoders()) result.push_back({FromNative(value.format), FromNative(value.info)});
  return result;
}
FfiAudioEncoderQuery audio_caller_encoder_factory_query(const NativeCallerAudioEncoderFactory& handle,
    const RustAudioEncoderFactoryCall& source, const FfiSdpAudioFormat& format) noexcept {
  auto& factory = *handle.state()->factory;
  CallerAudioEncoderFactory::CallScope call(factory, source);
  FfiAudioEncoderQuery result{};
  const auto info = factory.QueryAudioEncoder(ToNative(format));
  result.valid = true; // Binding errors are in this operation's Rust receipt.
  if (info) { result.present = true; result.info = FromNative(*info); }
  return result;
}
std::unique_ptr<NativeAudioEncoder> audio_caller_encoder_factory_create(const NativeCallerAudioEncoderFactory& handle,
    const RustAudioEncoderFactoryCall& source, const NativeEnvironment& environment,
    const FfiSdpAudioFormat& format, const NativeAudioEncoderOptions& options) noexcept {
  auto& factory = *handle.state()->factory;
  CallerAudioEncoderFactory::CallScope call(factory, source);
  return WrapCallerEncoder(factory.Create(environment.environment(), ToNative(format), options.value()));
}

FfiAudioEncoderOutput audio_encoder_forward_encode_sdk(const NativeAudioEncoder& source,
    std::uint32_t timestamp, rust::Slice<const std::int16_t> audio,
    rust::Slice<const std::uint8_t> prefix) noexcept {
  FfiAudioEncoderOutput result{};
  result.status = 2;
  if (!source.forwarding()) return result;
  const auto& sdk = source.sdk_encoder();
  const auto rate = sdk.SampleRateHz();
  const auto channels = sdk.NumChannels();
  if (rate <= 0 || channels == 0 || channels > std::numeric_limits<std::size_t>::max() /
      static_cast<std::size_t>(rate) || audio.size() != channels * rate / 100) {
    result.status = 1;
    return result;
  }
  webrtc::Buffer encoded(prefix.data(), prefix.size());
  const auto info = source.sdk_encoder().Encode(timestamp,
      std::span<const std::int16_t>(audio.data(), audio.size()), &encoded);
  if (encoded.size() < prefix.size() || encoded.size() - prefix.size() != info.encoded_bytes) return result;
  result.primary = FromNative(static_cast<const webrtc::AudioEncoder::EncodedInfoLeaf&>(info));
  for (const auto& leaf : info.redundant) result.redundant.push_back(FromNative(leaf));
  for (const auto byte : encoded) result.bytes.push_back(byte);
  result.status = 0;
  return result;
}
void audio_encoder_forward_target_sdk(const NativeAudioEncoder& source, std::int32_t bitrate) noexcept {
  if (source.forwarding()) source.sdk_encoder().OnReceivedTargetAudioBitrate(bitrate);
}
void audio_encoder_forward_loss_sdk(const NativeAudioEncoder& source, float fraction) noexcept {
  if (source.forwarding()) source.sdk_encoder().OnReceivedUplinkPacketLossFraction(fraction);
}
bool audio_encoder_forward_adaptor_sdk(const NativeAudioEncoder& source,
    rust::Slice<const std::uint8_t> config) noexcept {
  if (!source.forwarding()) return false;
  return source.sdk_encoder().EnableAudioNetworkAdaptor(
      absl::string_view(reinterpret_cast<const char*>(config.data()), config.size()));
}

struct NativeAudioDecoder::State {
  State(webrtc::Environment env, std::unique_ptr<webrtc::AudioDecoder> value,
        OwnedAudioDecoderKind selected)
      : environment(std::move(env)), decoder(std::move(value)), kind(selected),
        channels(decoder->Channels()), sample_rate(decoder->SampleRateHz()) {}
  // Mutex survives destruction of both the product and its environment.
  mutable std::mutex mutex;
  webrtc::Environment environment;
  std::unique_ptr<webrtc::AudioDecoder> decoder;
  OwnedAudioDecoderKind kind;
  const std::size_t channels;
  const int sample_rate;
  // Actual SDK forwarded-frame owners, shared by nested forwarding layers.
  // These frames may retain the Rust lease without creating a State reference.
  std::shared_ptr<std::atomic<std::size_t>> forwarded_frames =
      std::make_shared<std::atomic<std::size_t>>(0);
};
struct NativeDecodableAudioFrame::State {
  std::shared_ptr<NativeAudioDecoder::State> decoder;
  std::unique_ptr<webrtc::AudioDecoder::EncodedAudioFrame> frame;
  mutable bool decoded = false;
};
NativeAudioDecoder::NativeAudioDecoder(std::shared_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeAudioDecoder::~NativeAudioDecoder() = default;
const std::shared_ptr<NativeAudioDecoder::State>& NativeAudioDecoder::state() const noexcept {
  return state_;
}
std::size_t audio_decoder_forwarded_frame_count(const NativeAudioDecoder& decoder) noexcept {
  return decoder.state()->forwarded_frames->load(std::memory_order_acquire);
}
bool audio_decoder_factory_transfer_ready(const NativeAudioDecoder& decoder) noexcept {
  const auto& state = decoder.state();
  std::lock_guard lock(state->mutex);
  return state.use_count() == 1 && state->decoder != nullptr &&
      state->forwarded_frames->load(std::memory_order_acquire) == 0;
}
namespace {
using AudioDecoderRetention = std::shared_ptr<rust::Box<AudioDecoderTransfer>>;
NativeAudioDecoder::State& RetainedDecoderState(const AudioDecoderRetention& owner) {
  return *audio_decoder_transfer_native(**owner).state();
}

// These are real SDK frames from the original decoder, not reconstructed
// payloads or inferred metadata. Keep their original backend and Rust lease
// alive even after the SDK forwarding decoder itself has been destroyed.
class RetainedAudioDecoderFrame final : public webrtc::AudioDecoder::EncodedAudioFrame {
 public:
  RetainedAudioDecoderFrame(AudioDecoderRetention owner,
      std::unique_ptr<webrtc::AudioDecoder::EncodedAudioFrame> frame)
      : owner_(std::move(owner)), frame_(std::move(frame)) {
    RetainedDecoderState(owner_).forwarded_frames->fetch_add(1, std::memory_order_release);
  }
  ~RetainedAudioDecoderFrame() override {
    auto& state = RetainedDecoderState(owner_);
    std::lock_guard lock(state.mutex);
    // Frame destruction precedes owner/backend/lease destruction and shares
    // the same backend mutex as decode, reset and other forwarded operations.
    frame_.reset();
    state.forwarded_frames->fetch_sub(1, std::memory_order_release);
  }
  std::size_t Duration() const override {
    auto& state = RetainedDecoderState(owner_);
    std::lock_guard lock(state.mutex);
    return frame_->Duration();
  }
  bool IsDtxPacket() const override {
    auto& state = RetainedDecoderState(owner_);
    std::lock_guard lock(state.mutex);
    return frame_->IsDtxPacket();
  }
  std::optional<DecodeResult> Decode(std::span<std::int16_t> decoded) const override {
    auto& state = RetainedDecoderState(owner_);
    std::lock_guard lock(state.mutex);
    if (decoded_) return std::nullopt;
    decoded_ = true;
    // The SDK caller supplies the span under EncodedAudioFrame's capacity
    // contract. Safe Rust uses the existing stock-origin bound in frame_decode.
    // Do not replace it with a guessed universal decoder capacity.
    return frame_->Decode(decoded);
  }
 private:
  AudioDecoderRetention owner_;
  std::unique_ptr<webrtc::AudioDecoder::EncodedAudioFrame> frame_;
  mutable bool decoded_ = false;
};

class RetainedAudioDecoder final : public webrtc::AudioDecoder {
 public:
  explicit RetainedAudioDecoder(AudioDecoderRetention owner) : owner_(std::move(owner)) {}
  const NativeAudioDecoder::State& original_state() const { return source(); }
  std::vector<ParseResult> ParsePayload(webrtc::Buffer&& payload, std::uint32_t timestamp) override {
    auto& state = source();
    std::lock_guard lock(state.mutex);
    auto parsed = state.decoder->ParsePayload(std::move(payload), timestamp);
    for (auto& value : parsed) {
      if (value.frame) value.frame = std::make_unique<RetainedAudioDecoderFrame>(owner_, std::move(value.frame));
    }
    return parsed;
  }
  void GeneratePlc(std::size_t samples, webrtc::BufferT<std::int16_t>* audio) override {
    auto& state = source(); std::lock_guard lock(state.mutex);
    state.decoder->GeneratePlc(samples, audio);
  }
  void Reset() override {
    auto& state = source(); std::lock_guard lock(state.mutex); state.decoder->Reset();
  }
  int ErrorCode() override {
    auto& state = source(); std::lock_guard lock(state.mutex); return state.decoder->ErrorCode();
  }
  int PacketDuration(const std::uint8_t* data, std::size_t size) const override {
    auto& state = source(); std::lock_guard lock(state.mutex); return state.decoder->PacketDuration(data, size);
  }
  int PacketDurationRedundant(const std::uint8_t* data, std::size_t size) const override {
    auto& state = source(); std::lock_guard lock(state.mutex); return state.decoder->PacketDurationRedundant(data, size);
  }
  bool PacketHasFec(const std::uint8_t* data, std::size_t size) const override {
    auto& state = source(); std::lock_guard lock(state.mutex); return state.decoder->PacketHasFec(data, size);
  }
  int SampleRateHz() const override {
    auto& state = source(); std::lock_guard lock(state.mutex); return state.decoder->SampleRateHz();
  }
  std::size_t Channels() const override {
    auto& state = source(); std::lock_guard lock(state.mutex); return state.decoder->Channels();
  }
  // Preserve SDK-internal compatibility virtuals. These are NOT public Rust
  // raw-output APIs and do not relax their original caller-buffer contracts.
  bool HasDecodePlc() const override {
    auto& state = source(); std::lock_guard lock(state.mutex); return state.decoder->HasDecodePlc();
  }
  std::size_t DecodePlc(std::size_t frames, std::int16_t* decoded) override {
    auto& state = source(); std::lock_guard lock(state.mutex); return state.decoder->DecodePlc(frames, decoded);
  }
 protected:
  int DecodeInternal(const std::uint8_t* data, std::size_t size, int rate,
      std::int16_t* decoded, SpeechType* speech) override {
    auto& state = source(); std::lock_guard lock(state.mutex);
    // The forwarding instance's nonvirtual Decode already performs the SDK
    // capacity check. Repeat native delegation without a second invented cap.
    return state.decoder->Decode(data, size, rate, std::numeric_limits<std::size_t>::max(), decoded, speech);
  }
  int DecodeRedundantInternal(const std::uint8_t* data, std::size_t size, int rate,
      std::int16_t* decoded, SpeechType* speech) override {
    auto& state = source(); std::lock_guard lock(state.mutex);
    return state.decoder->DecodeRedundant(data, size, rate, std::numeric_limits<std::size_t>::max(), decoded, speech);
  }
 private:
  NativeAudioDecoder::State& source() const { return RetainedDecoderState(owner_); }
  AudioDecoderRetention owner_;
};
} // namespace
std::unique_ptr<webrtc::AudioDecoder> audio_decoder_forward_to_sdk(
    rust::Box<AudioDecoderTransfer> transfer) noexcept {
  if (!audio_decoder_factory_transfer_ready(audio_decoder_transfer_native(*transfer))) return nullptr;
  return std::make_unique<RetainedAudioDecoder>(
      std::make_shared<rust::Box<AudioDecoderTransfer>>(std::move(transfer)));
}
std::unique_ptr<NativeAudioDecoder> audio_decoder_forward_product(
    rust::Box<AudioDecoderTransfer> transfer) noexcept {
  const auto& source = audio_decoder_transfer_native(*transfer).state();
  auto env = source->environment;
  const auto kind = source->kind;
  auto frames = source->forwarded_frames;
  auto decoder = audio_decoder_forward_to_sdk(std::move(transfer));
  if (!decoder) return nullptr;
  auto state = std::make_shared<NativeAudioDecoder::State>(std::move(env), std::move(decoder), kind);
  state->forwarded_frames = std::move(frames);
  return std::make_unique<NativeAudioDecoder>(std::move(state));
}
namespace {
// A real refcounted SDK factory used by one-shot drivers or a persistent owner.
// Scoped requests retain inputs and independent receipts only for synchronous
// operations. The SDK owner retains its Rust registration, not last-call state;
// it is not a global registry or an installable unproved dependency.
class CallerAudioDecoderFactory : public webrtc::AudioDecoderFactory {
 public:
  explicit CallerAudioDecoderFactory(rust::Box<RustAudioDecoderFactoryCall> source)
      : source_(std::move(source)) {}
  class CallScope {
   public:
    CallScope(CallerAudioDecoderFactory& factory, const RustAudioDecoderFactoryCall& source)
        : factory_(factory), previous_(factory.active_) { factory_.active_ = &source; }
    ~CallScope() { factory_.active_ = previous_; }
    CallScope(const CallScope&) = delete;
    CallScope& operator=(const CallScope&) = delete;
   private:
    CallerAudioDecoderFactory& factory_;
    const RustAudioDecoderFactoryCall* previous_;
  };
  std::vector<webrtc::AudioCodecSpec> GetSupportedDecoders() override {
    std::vector<webrtc::AudioCodecSpec> result;
    for (const auto& value : audio_caller_decoder_formats_callback(source())) {
      // Discovery is observation, not an admission to the voice engine.
      result.push_back({ToNative(value.format), AudioInfo(value.info)});
    }
    return result;
  }
  bool IsSupportedDecoder(const webrtc::SdpAudioFormat& format) override {
    return audio_caller_decoder_supported_callback(source(), FromNative(format));
  }
  std::unique_ptr<webrtc::AudioDecoder> Create(const webrtc::Environment& env,
      const webrtc::SdpAudioFormat& format) override {
    return CreateOwned(env, format, false, std::nullopt);
  }
  std::unique_ptr<webrtc::AudioDecoder> Create(const webrtc::Environment& env,
      const webrtc::SdpAudioFormat& format,
      std::optional<webrtc::AudioCodecPairId> pair) override {
    return CreateOwned(env, format, true, pair);
  }
 private:
  const RustAudioDecoderFactoryCall& source() const { return active_ ? *active_ : *source_; }
  std::unique_ptr<webrtc::AudioDecoder> CreateOwned(const webrtc::Environment& env,
      const webrtc::SdpAudioFormat& format, bool compatibility,
      std::optional<webrtc::AudioCodecPairId> pair) {
    auto selected = audio_caller_decoder_environment(source());
    auto product = audio_caller_decoder_create_callback(source(),
        retain_environment_from_sdk(env, selected.get()), FromNative(format), compatibility,
        pair ? std::make_unique<NativeAudioCodecPairId>(*pair) : nullptr);
    if (!product) return nullptr; // Rust receipt distinguishes failure from None.
    if (!audio_decoder_factory_transfer_ready(*product)) {
      audio_caller_decoder_transfer_failed(source(), compatibility);
      return nullptr;
    }
    // The private Rust callback constructs a RetainedAudioDecoder envelope.
    // Move that SDK envelope, NOT its original backend: the complete original
    // product, provenance, mutex and Rust environment lease remain inside it.
    return std::move(product->state()->decoder);
  }
  rust::Box<RustAudioDecoderFactoryCall> source_;
  const RustAudioDecoderFactoryCall* active_ = nullptr;
};
std::unique_ptr<NativeAudioDecoder> WrapCallerDecoder(std::unique_ptr<webrtc::AudioDecoder> decoder) {
  if (!decoder) return nullptr;
  // Exact private RetainedAudioDecoder constructor provenance, never a codec
  // name or discovery capability. Keep original state and frame-counter lease.
  const auto& origin = static_cast<const RetainedAudioDecoder&>(*decoder).original_state();
  auto env = origin.environment;
  const auto kind = origin.kind;
  auto frames = origin.forwarded_frames;
  auto state = std::make_shared<NativeAudioDecoder::State>(std::move(env), std::move(decoder), kind);
  state->forwarded_frames = std::move(frames);
  return std::make_unique<NativeAudioDecoder>(std::move(state));
}
} // namespace
rust::Vec<FfiAudioCodecSpec> audio_caller_decoder_formats(
    rust::Box<RustAudioDecoderFactoryCall> source) noexcept {
  auto factory = webrtc::make_ref_counted<CallerAudioDecoderFactory>(std::move(source));
  rust::Vec<FfiAudioCodecSpec> result;
  for (const auto& value : factory->GetSupportedDecoders()) {
    result.push_back({FromNative(value.format), FromNative(value.info)});
  }
  return result;
}
bool audio_caller_decoder_supported(rust::Box<RustAudioDecoderFactoryCall> source,
    const FfiSdpAudioFormat& format) noexcept {
  auto factory = webrtc::make_ref_counted<CallerAudioDecoderFactory>(std::move(source));
  return factory->IsSupportedDecoder(ToNative(format));
}
std::unique_ptr<NativeAudioDecoder> audio_caller_decoder_create(
    rust::Box<RustAudioDecoderFactoryCall> source, const NativeEnvironment& environment,
    const FfiSdpAudioFormat& format, bool compatibility,
    const std::unique_ptr<NativeAudioCodecPairId>& pair) noexcept {
  auto factory = webrtc::make_ref_counted<CallerAudioDecoderFactory>(std::move(source));
  const auto native = ToNative(format);
  auto decoder = compatibility
      ? factory->Create(environment.environment(), native,
          pair ? std::optional<webrtc::AudioCodecPairId>(pair->value()) : std::nullopt)
      : factory->Create(environment.environment(), native);
  return WrapCallerDecoder(std::move(decoder));
}
struct NativeCallerAudioDecoderFactory::State {
  explicit State(rust::Box<RustAudioDecoderFactoryCall> source)
      : factory(webrtc::make_ref_counted<CallerAudioDecoderFactory>(std::move(source))) {}
  webrtc::scoped_refptr<CallerAudioDecoderFactory> factory;
};
NativeCallerAudioDecoderFactory::NativeCallerAudioDecoderFactory(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeCallerAudioDecoderFactory::~NativeCallerAudioDecoderFactory() = default;
std::unique_ptr<NativeCallerAudioDecoderFactory> audio_caller_decoder_factory_new(rust::Box<RustAudioDecoderFactoryCall> source) noexcept {
  return std::make_unique<NativeCallerAudioDecoderFactory>(std::make_unique<NativeCallerAudioDecoderFactory::State>(std::move(source)));
}
rust::Vec<FfiAudioCodecSpec> audio_caller_decoder_factory_formats(const NativeCallerAudioDecoderFactory& handle,
    const RustAudioDecoderFactoryCall& source) noexcept {
  auto& factory = *handle.state()->factory;
  CallerAudioDecoderFactory::CallScope call(factory, source);
  rust::Vec<FfiAudioCodecSpec> result;
  for (const auto& value : factory.GetSupportedDecoders()) result.push_back({FromNative(value.format), FromNative(value.info)});
  return result;
}
bool audio_caller_decoder_factory_supported(const NativeCallerAudioDecoderFactory& handle,
    const RustAudioDecoderFactoryCall& source, const FfiSdpAudioFormat& format) noexcept {
  auto& factory = *handle.state()->factory;
  CallerAudioDecoderFactory::CallScope call(factory, source);
  return factory.IsSupportedDecoder(ToNative(format));
}
std::unique_ptr<NativeAudioDecoder> audio_caller_decoder_factory_create(const NativeCallerAudioDecoderFactory& handle,
    const RustAudioDecoderFactoryCall& source, const NativeEnvironment& environment,
    const FfiSdpAudioFormat& format, bool compatibility,
    const std::unique_ptr<NativeAudioCodecPairId>& pair) noexcept {
  auto& factory = *handle.state()->factory;
  CallerAudioDecoderFactory::CallScope call(factory, source);
  const auto native = ToNative(format);
  return WrapCallerDecoder(compatibility
      ? factory.Create(environment.environment(), native, pair ? std::optional<webrtc::AudioCodecPairId>(pair->value()) : std::nullopt)
      : factory.Create(environment.environment(), native));
}
std::unique_ptr<webrtc::AudioDecoder> audio_decoder_take_fresh(
    std::unique_ptr<NativeAudioDecoder> decoder) noexcept {
  if (!decoder) return nullptr;
  const auto& state = decoder->state();
  // This consumes the sole owning handle. Parsed frames retain the same State;
  // detaching while any exist would leave their decoder pointer dangling.
  if (!audio_decoder_factory_transfer_ready(*decoder)) return nullptr;
  return std::move(state->decoder);
}
namespace {
// Keeps the original decoder and every native compatibility virtual intact.
// Parsed frames continue to refer to the original instance, not this notifier.
class NetEqLifetimeDecoder final : public webrtc::AudioDecoder {
 public:
  NetEqLifetimeDecoder(webrtc::Environment env,
      std::unique_ptr<webrtc::AudioDecoder> decoder,
      std::shared_ptr<NetEqDecoderLifetime> lifetime)
      : environment_(std::move(env)), decoder_(std::move(decoder)),
        lifetime_(std::move(lifetime)) {}
  ~NetEqLifetimeDecoder() override {
    decoder_.reset();
    lifetime_->state.store(NetEqDecoderLifetimeState::Destroyed);
  }
  const webrtc::AudioDecoder* original() const { return decoder_.get(); }
  std::vector<ParseResult> ParsePayload(webrtc::Buffer&& payload, uint32_t timestamp) override {
    return decoder_->ParsePayload(std::move(payload), timestamp);
  }
  bool HasDecodePlc() const override { return decoder_->HasDecodePlc(); }
  size_t DecodePlc(size_t frames, int16_t* decoded) override {
    return decoder_->DecodePlc(frames, decoded);
  }
  void GeneratePlc(size_t samples, webrtc::BufferT<int16_t>* audio) override {
    decoder_->GeneratePlc(samples, audio);
  }
  void Reset() override { decoder_->Reset(); }
  int ErrorCode() override { return decoder_->ErrorCode(); }
  int PacketDuration(const uint8_t* data, size_t size) const override {
    return decoder_->PacketDuration(data, size);
  }
  int PacketDurationRedundant(const uint8_t* data, size_t size) const override {
    return decoder_->PacketDurationRedundant(data, size);
  }
  bool PacketHasFec(const uint8_t* data, size_t size) const override {
    return decoder_->PacketHasFec(data, size);
  }
  int SampleRateHz() const override { return decoder_->SampleRateHz(); }
  size_t Channels() const override { return decoder_->Channels(); }
 protected:
  int DecodeInternal(const uint8_t* data, size_t size, int rate,
      int16_t* decoded, SpeechType* speech) override {
    // The outer nonvirtual wrapper already performed the SDK capacity check.
    // This is SDK-internal delegation, not a safe Rust raw-output API.
    return decoder_->Decode(data, size, rate, std::numeric_limits<size_t>::max(), decoded, speech);
  }
  int DecodeRedundantInternal(const uint8_t* data, size_t size, int rate,
      int16_t* decoded, SpeechType* speech) override {
    return decoder_->DecodeRedundant(data, size, rate,
        std::numeric_limits<size_t>::max(), decoded, speech);
  }
 private:
  webrtc::Environment environment_;
  std::unique_ptr<webrtc::AudioDecoder> decoder_;
  std::shared_ptr<NetEqDecoderLifetime> lifetime_;
};
bool SameNetEqEnvironment(const webrtc::Environment& a, const webrtc::Environment& b) {
  return &a.clock() == &b.clock() && &a.field_trials() == &b.field_trials() &&
      &a.task_queue_factory() == &b.task_queue_factory() && &a.event_log() == &b.event_log();
}
} // namespace
struct PreparedNetEqDecoderFactory::State {
  struct Pending {
    webrtc::SdpAudioFormat format;
    bool compatibility;
    std::optional<webrtc::AudioCodecPairId> pair_id;
    webrtc::Environment environment;
    std::unique_ptr<webrtc::AudioDecoder> decoder;
    std::shared_ptr<NetEqDecoderLifetime> lifetime;
  };
  std::unique_ptr<NativeAudioDecoderFactory> source;
  std::mutex mutex;
  std::vector<Pending> pending;
};
PreparedNetEqDecoderFactory::PreparedNetEqDecoderFactory(const NativeAudioDecoderFactory& factory)
    : state_(std::make_unique<State>()) {
  state_->source = std::make_unique<NativeAudioDecoderFactory>(
      std::make_unique<NativeAudioDecoderFactory::State>(factory.state()));
}
PreparedNetEqDecoderFactory::~PreparedNetEqDecoderFactory() = default;
std::vector<webrtc::AudioCodecSpec> PreparedNetEqDecoderFactory::GetSupportedDecoders() {
  return state_->source->factory()->GetSupportedDecoders();
}
bool PreparedNetEqDecoderFactory::IsSupportedDecoder(const webrtc::SdpAudioFormat& format) {
  return audio_decoder_supported(*state_->source, AudioFormatToFfi(format)) == 1;
}
std::shared_ptr<NetEqDecoderLifetime> PreparedNetEqDecoderFactory::Prepare(
    const webrtc::SdpAudioFormat& format, bool compatibility,
    std::optional<webrtc::AudioCodecPairId> pair_id,
    std::unique_ptr<NativeAudioDecoder> decoder) {
  if (!decoder || (!compatibility && pair_id)) return nullptr;
  const auto rate = audio_decoder_sample_rate(*decoder);
  if (rate != 8000 && rate != 16000 && rate != 32000 && rate != 48000) return nullptr;
  // Validate the actual instance, not SDP-advertised channels. This is precisely
  // NetEqImpl::SetSampleRateAndChannels' 10ms AudioFrame shape requirement.
  // No extra sync-buffer restriction or arbitrary stereo-only policy is added.
  const auto channels = audio_decoder_channels(*decoder);
  const auto output_samples = webrtc::SampleRateToDefaultChannelSize(rate);
  if (channels == 0 || channels > webrtc::kMaxNumberOfAudioChannels ||
      channels > webrtc::AudioFrame::kMaxDataSizeSamples / output_samples)
    return nullptr;
  auto env = decoder->state()->environment;
  auto owned = audio_decoder_take_fresh(std::move(decoder));
  if (!owned) return nullptr;
  std::lock_guard lock(state_->mutex);
  for (const auto& entry : state_->pending) {
    if (entry.format == format && entry.compatibility == compatibility && entry.pair_id == pair_id)
      return nullptr;
  }
  auto lifetime = std::make_shared<NetEqDecoderLifetime>();
  auto notified = std::make_unique<NetEqLifetimeDecoder>(env, std::move(owned), lifetime);
  state_->pending.push_back(State::Pending{format, compatibility, pair_id,
      std::move(env), std::move(notified), lifetime});
  return lifetime;
}
std::unique_ptr<webrtc::AudioDecoder> PreparedNetEqDecoderFactory::Consume(
    const webrtc::Environment& env, const webrtc::SdpAudioFormat& format,
    bool compatibility, std::optional<webrtc::AudioCodecPairId> pair_id) {
  std::lock_guard lock(state_->mutex);
  for (auto it = state_->pending.begin(); it != state_->pending.end(); ++it) {
    if (it->format == format && it->compatibility == compatibility && it->pair_id == pair_id &&
        SameNetEqEnvironment(env, it->environment)) {
      auto result = std::move(it->decoder);
      it->lifetime->state.store(NetEqDecoderLifetimeState::Consumed);
      state_->pending.erase(it);
      return result;
    }
  }
  return nullptr;
}
std::unique_ptr<webrtc::AudioDecoder> PreparedNetEqDecoderFactory::Create(
    const webrtc::Environment& env, const webrtc::SdpAudioFormat& format) {
  return Consume(env, format, false, std::nullopt);
}
std::unique_ptr<webrtc::AudioDecoder> PreparedNetEqDecoderFactory::Create(
    const webrtc::Environment& env, const webrtc::SdpAudioFormat& format,
    std::optional<webrtc::AudioCodecPairId> pair_id) {
  return Consume(env, format, true, pair_id);
}
std::shared_ptr<NetEqDecoderLifetime> PreparedNetEqDecoderFactory::PrepareDecoder(
    const NativeEnvironment& environment, const webrtc::SdpAudioFormat& format) {
  auto created = audio_decoder_create(*state_->source, environment, AudioFormatToFfi(format));
  if (!created.valid || !created.decoder) return nullptr;
  return Prepare(format, false, std::nullopt, std::move(created.decoder));
}
void PreparedNetEqDecoderFactory::CancelPrepared() {
  std::lock_guard lock(state_->mutex);
  state_->pending.clear();
}
bool neteq_decoder_handoff_contract(const NativeAudioDecoderFactory& factory,
    const NativeEnvironment& environment) noexcept {
  auto handoff = webrtc::make_ref_counted<PreparedNetEqDecoderFactory>(factory);
  const webrtc::SdpAudioFormat format("opus", 48000, 2, {{"stereo", "0"}});
  const auto ffi_format = AudioFormatToFfi(format);
  const auto env = environment.environment();
  if (handoff->Prepare(format, false, std::nullopt, nullptr) || handoff->Create(env, format))
    return false;
  const auto destroyed = [](const auto& token) {
    return token && token->state.load() == NetEqDecoderLifetimeState::Destroyed;
  };
  auto fresh = audio_decoder_create(factory, environment, ffi_format);
  if (!fresh.valid || !fresh.decoder) return false;
  const auto* identity = fresh.decoder->state()->decoder.get();
  auto token = handoff->Prepare(format, false, std::nullopt, std::move(fresh.decoder));
  if (!token || token->state.load() != NetEqDecoderLifetimeState::Prepared) return false;
  // Neither format mismatch, environment mismatch nor overload mismatch may
  // consume or construct another decoder.
  if (handoff->Create(env, webrtc::SdpAudioFormat("opus", 48000, 2, {{"stereo", "1"}})) ||
      handoff->Create(env, format, std::nullopt) ||
      handoff->Create(webrtc::EnvironmentFactory().Create(), format)) return false;
  auto decoded = handoff->Create(env, format);
  // The factory returns a notifier holding the same original decoder object.
  if (!decoded || static_cast<NetEqLifetimeDecoder*>(decoded.get())->original() != identity ||
      token->state.load() != NetEqDecoderLifetimeState::Consumed ||
      decoded->SampleRateHz() != 48000 || decoded->Channels() != 1 ||
      handoff->Create(env, format)) return false;
  decoded.reset();
  if (!destroyed(token)) return false;

  std::unique_ptr<NativeAudioCodecPairId> absent;
  auto pair = audio_codec_pair_id_create();
  auto other = audio_codec_pair_id_create();
  fresh = audio_decoder_create_with_pair_id(factory, environment, ffi_format, absent);
  token = handoff->Prepare(format, true, std::nullopt, std::move(fresh.decoder));
  if (!token || handoff->Create(env, format) || handoff->Create(env, format, pair->value()))
    return false;
  decoded = handoff->Create(env, format, std::nullopt);
  if (!decoded) return false;
  decoded.reset();
  if (!destroyed(token)) return false;
  fresh = audio_decoder_create_with_pair_id(factory, environment, ffi_format, pair);
  token = handoff->Prepare(format, true, pair->value(), std::move(fresh.decoder));
  if (!token || handoff->Create(env, format, std::nullopt) ||
      handoff->Create(env, format, other->value())) return false;
  decoded = handoff->Create(env, format, pair->value());
  if (!decoded) return false;
  // Consumed products outlive the handoff factory itself.
  handoff = nullptr;
  if (token->state.load() != NetEqDecoderLifetimeState::Consumed || decoded->Channels() != 1)
    return false;
  decoded.reset();
  if (!destroyed(token)) return false;

  handoff = webrtc::make_ref_counted<PreparedNetEqDecoderFactory>(factory);
  fresh = audio_decoder_create(factory, environment, ffi_format);
  token = handoff->Prepare(format, false, std::nullopt, std::move(fresh.decoder));
  if (!token) return false;
  handoff->CancelPrepared();
  if (!destroyed(token) || handoff->Create(env, format)) return false;
  fresh = audio_decoder_create(factory, environment, ffi_format);
  identity = fresh.decoder->state()->decoder.get();
  token = handoff->Prepare(format, false, std::nullopt, std::move(fresh.decoder));
  fresh = audio_decoder_create(factory, environment, ffi_format);
  if (!token || handoff->Prepare(format, false, std::nullopt, std::move(fresh.decoder)))
    return false;
  decoded = handoff->Create(env, format);
  if (!decoded || static_cast<NetEqLifetimeDecoder*>(decoded.get())->original() != identity)
    return false;
  decoded.reset();
  if (!destroyed(token)) return false;
  fresh = audio_decoder_create(factory, environment, ffi_format);
  auto shared_state = fresh.decoder->state();
  if (handoff->Prepare(format, false, std::nullopt, std::move(fresh.decoder)) ||
      handoff->Create(env, format)) return false;
  // The shared decoder was not detached or destroyed by the refused handoff.
  if (!shared_state->decoder || shared_state->decoder->Channels() != 1) return false;
  return true;
}
NativeDecodableAudioFrame::NativeDecodableAudioFrame(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeDecodableAudioFrame::~NativeDecodableAudioFrame() {
  // Frame destruction is serialized too; destroy it before its decoder owner.
  std::lock_guard lock(state_->decoder->mutex);
  state_->frame.reset();
}
const NativeDecodableAudioFrame::State& NativeDecodableAudioFrame::state() const noexcept {
  return *state_;
}

webrtc::SdpAudioFormat AudioFormatFromFfi(const FfiSdpAudioFormat& value) noexcept {
  return ToNative(value);
}
FfiSdpAudioFormat AudioFormatToFfi(const webrtc::SdpAudioFormat& value) noexcept {
  return FromNative(value);
}

bool neteq_stock_config_safe(const FfiNetEqConfig& value) noexcept {
  const auto rate = value.sample_rate_hz;
  return (rate == 8000 || rate == 16000 || rate == 32000 || rate == 48000) &&
      neteq_stock_buffer_packets_safe(value.max_packets_in_buffer);
}
bool neteq_stock_buffer_packets_safe(std::size_t count) noexcept {
  return count <= static_cast<size_t>(std::numeric_limits<int>::max() / (3 * 120));
}
bool neteq_stock_payload_size_safe(std::size_t size) noexcept {
  // In particular, prevents Opus's size_t -> opus_int32 narrowing. Applying
  // the bound to an outer RED packet also bounds each validated inner block.
  return size <= static_cast<size_t>(std::numeric_limits<int32_t>::max());
}
bool neteq_stock_red_payload_types(rust::Slice<const std::uint8_t> payload,
    rust::Vec<std::uint8_t>& types) noexcept {
  types.clear();
  if (!neteq_stock_payload_size_safe(payload.size())) return false;
  rust::Vec<std::uint8_t> found;
  size_t offset = 0;
  size_t claimed = 0;
  for (;;) {
    if (offset == payload.size()) return false;
    const auto tag = payload[offset];
    if ((tag & 0x80) != 0) {
      if (payload.size() - offset < 4) return false;
      const size_t length = ((payload[offset + 2] & 3) << 8) | payload[offset + 3];
      offset += 4;
      if (length > payload.size() - claimed) return false;
      claimed += length;
      if (length != 0) {
        // More nonempty blocks can never become a valid stock RED packet.
        // Reject immediately rather than growing a vector through all headers.
        if (found.size() == 32) return false;
        found.push_back(tag & 0x7f);
      }
    } else {
      ++offset;
      if (claimed > payload.size() - offset) return false;
      if (payload.size() - offset - claimed != 0) found.push_back(tag & 0x7f);
      if (found.size() > 32) return false;
      types = std::move(found);
      return true;
    }
  }
}
namespace {
// Observe packet metadata without changing stock packet selection, decoding,
// statistics, or clocks. All actual buffer operations still run in PacketBuffer.
// The snapshot lets guards examine the prospective post-discard front without
// moving discards ahead of GetAudio's muted-state branch.
class ObservedNetEqPacketBuffer final : public webrtc::PacketBuffer {
 public:
  ObservedNetEqPacketBuffer(size_t capacity, const webrtc::TickTimer* timer,
      webrtc::StatisticsCalculator* stats)
      : PacketBuffer(capacity, timer, stats) {}

  int InsertPacket(webrtc::Packet&& packet) override {
    if (packet.empty()) return PacketBuffer::InsertPacket(std::move(packet));
    webrtc::Packet metadata;
    metadata.timestamp = packet.timestamp;
    metadata.sequence_number = packet.sequence_number;
    metadata.payload_type = packet.payload_type;
    metadata.priority = packet.priority;
    const auto result = PacketBuffer::InsertPacket(std::move(packet));
    // Native InsertPacket may have called our virtual Flush. Reproduce only
    // metadata membership, using the SDK's own wrap-aware packet comparison.
    auto right = std::find_if(snapshot_.rbegin(), snapshot_.rend(),
        [&](const auto& old) { return metadata >= old; });
    if (right != snapshot_.rend() && metadata.timestamp == right->timestamp)
      return result;
    auto left = right.base();
    if (left != snapshot_.end() && metadata.timestamp == left->timestamp)
      left = snapshot_.erase(left);
    snapshot_.insert(left, std::move(metadata));
    return result;
  }
  void Flush() override {
    PacketBuffer::Flush();
    snapshot_.clear();
  }
  std::optional<webrtc::Packet> GetNextPacket() override {
    auto packet = PacketBuffer::GetNextPacket();
    if (packet) snapshot_.pop_front();
    return packet;
  }
  int DiscardNextPacket() override {
    const auto result = PacketBuffer::DiscardNextPacket();
    if (result == kOK) snapshot_.pop_front();
    return result;
  }
  void DiscardOldPackets(uint32_t timestamp, uint32_t horizon) override {
    PacketBuffer::DiscardOldPackets(timestamp, horizon);
    snapshot_.remove_if([&](const auto& packet) {
      return IsObsoleteTimestamp(packet.timestamp, timestamp, horizon);
    });
  }
  void DiscardPacketsWithPayloadType(uint8_t payload_type) override {
    PacketBuffer::DiscardPacketsWithPayloadType(payload_type);
    snapshot_.remove_if([&](const auto& packet) {
      return packet.payload_type == payload_type;
    });
  }
  // SetMaxNumberOfPackets calls virtual DiscardNextPacket, and
  // DiscardAllOldPackets calls virtual DiscardOldPackets, so both stay observed.
  bool ContainsPayloadType(uint8_t payload_type) const {
    return std::any_of(snapshot_.begin(), snapshot_.end(), [&](const auto& packet) {
      return packet.payload_type == payload_type;
    });
  }
  const webrtc::Packet* DecisionFront(const webrtc::DecoderDatabase& database,
      bool new_codec, webrtc::NetEq::Mode last_mode, uint32_t end_timestamp,
      uint32_t horizon, uint64_t generated_noise_samples) const {
    for (const auto& packet : snapshot_) {
      if (!new_codec && IsObsoleteTimestamp(packet.timestamp, end_timestamp, horizon))
        continue;
      if (last_mode == webrtc::NetEq::Mode::kRfc3389Cng &&
          database.IsComfortNoise(packet.payload_type) &&
          (end_timestamp >= packet.timestamp ||
           end_timestamp + generated_noise_samples > packet.timestamp))
        continue;
      return &packet;
    }
    return nullptr;
  }
 private:
  webrtc::PacketList snapshot_;
};

struct StockNetEqDependencies final : webrtc::NetEqImpl::Dependencies {
  StockNetEqDependencies(const webrtc::Environment& env,
      const webrtc::NetEq::Config& config,
      webrtc::scoped_refptr<PreparedNetEqDecoderFactory> factory)
      : Dependencies(env, config, std::move(factory),
            webrtc::DefaultNetEqControllerFactory()) {
    packet_buffer = std::make_unique<ObservedNetEqPacketBuffer>(
        config.max_packets_in_buffer, tick_timer.get(), stats.get());
  }
};

// Base destruction runs in reverse order: keep the native dependency leases
// alive until NetEqImpl and all of its component destructors have finished.
class StockNetEqEnvironmentOwner {
 protected:
  explicit StockNetEqEnvironmentOwner(std::unique_ptr<NativeEnvironment> environment)
      : environment_(std::move(environment)) {}
  std::unique_ptr<NativeEnvironment> environment_;
};

class GuardedStockNetEq final : private StockNetEqEnvironmentOwner,
                              public webrtc::NetEqImpl {
 public:
  GuardedStockNetEq(std::unique_ptr<NativeEnvironment> environment,
      const webrtc::NetEq::Config& config,
      webrtc::scoped_refptr<PreparedNetEqDecoderFactory> factory,
      Dependencies&& deps, int64_t minimum_time_us, int64_t maximum_time_us)
      : StockNetEqEnvironmentOwner(std::move(environment)),
        NetEqImpl(config, std::move(deps)), factory_(std::move(factory)),
        minimum_time_us_(minimum_time_us),
        maximum_time_us_(maximum_time_us) {}

  // Preserve overload forwarding while guarding every path to the stock
  // packet-info implementation. The other two overloads do not erase metadata.
  using NetEqImpl::InsertPacket;
  int InsertPacket(const webrtc::RTPHeader& header,
      std::span<const uint8_t> payload, const webrtc::RtpPacketInfo& info) override {
    const auto receive = info.receive_time();
    if (receive.IsPlusInfinity() ||
        (!receive.IsMinusInfinity() && !neteq_receive_time_safe(
            minimum_time_us_, maximum_time_us_, receive.us()))) return kFail;
    if (!neteq_stock_payload_size_safe(payload.size())) return kFail;
    std::vector<uint8_t> types;
    if (decoder_database_->IsRed(header.payloadType)) {
      rust::Vec<uint8_t> inner;
      if (!neteq_stock_red_payload_types({payload.data(), payload.size()}, inner))
        return kFail;
      // SplitRed pushes blocks to the front, reversing their header order.
      // CheckRedPayloads then keeps the first ordinary PT, not first format.
      std::optional<uint8_t> ordinary;
      for (size_t i = inner.size(); i != 0; --i) {
        const uint8_t pt = inner[i - 1];
        const auto* decoder = decoder_database_->GetDecoderInfo(pt);
        if (decoder && decoder->IsRed()) continue;
        if (!decoder || (!decoder->IsComfortNoise() && !decoder->IsDtmf())) {
          if (ordinary && *ordinary != pt) continue;
          ordinary = pt;
        }
        types.push_back(pt);
      }
    } else {
      types.push_back(header.payloadType);
    }
    // Unknown retained PTs fail stock's CheckPayloadTypes before any creation.
    // Preserve that native failure without constructing unrelated decoders.
    for (const auto pt : types) {
      if (!decoder_database_->GetDecoderInfo(pt))
        return NetEqImpl::InsertPacket(header, payload, info);
    }
    for (const auto pt : types) {
      if (!EnsureDecoder(pt)) return kFail;
    }
    return NetEqImpl::InsertPacket(header, payload, info);
  }

  bool CreateDecoder(int payload_type) override {
    // Native GetDecoder takes uint8_t. Preserve its compatibility narrowing.
    if (!EnsureDecoder(static_cast<uint8_t>(payload_type))) return false;
    return NetEqImpl::CreateDecoder(payload_type);
  }
  std::optional<DecoderFormat> GetCurrentDecoderFormat() const override {
    if (current_rtp_payload_type_) {
      const auto* info = decoder_database_->GetDecoderInfo(*current_rtp_payload_type_);
      // The pinned getter dereferences GetDecoder even for internally handled
      // formats that have no AudioDecoder. Absence is not invented DSP metadata.
      if (info && (info->IsComfortNoise() || info->IsDtmf() || info->IsRed()))
        return std::nullopt;
      if (!EnsureDecoder(*current_rtp_payload_type_)) return std::nullopt;
    }
    return NetEqImpl::GetCurrentDecoderFormat();
  }
  int GetAudio(webrtc::AudioFrame* frame, bool* muted, int* rate,
      std::optional<Operation> action) override {
    if (action && (*action < Operation::kNormal || *action > Operation::kUndefined ||
        (*action == Operation::kUndefined && !new_codec_)))
      return kFail;
    const auto& packets = observed_packets();
    // GetAudio increments the timer before GetDecision subtracts one tick.
    // Use unsigned arithmetic exactly as stock does, without another clock read.
    const uint64_t noise = generated_noise_stopwatch_
        ? generated_noise_stopwatch_->ElapsedTicks() * output_size_samples_ +
              controller_->noise_fast_forward()
        : 0;
    const auto* next = packets.DecisionFront(*decoder_database_, new_codec_, last_mode_,
        sync_buffer_->end_timestamp(), 5 * fs_hz_, noise);
    const auto future = sync_buffer_->FutureLength();
    const auto overlap = expand_->overlap_length();
    const bool normalizes_to_normal = future >= overlap &&
        future - overlap >= output_size_samples_;
    if (action == Operation::kRfc3389Cng && !normalizes_to_normal && next &&
        !decoder_database_->IsComfortNoise(next->payload_type))
      return kFail;
    // A current PT may be stale after SetCodecs. Do not construct an unused
    // replacement merely because only CN packets remain in the buffer.
    if (current_rtp_payload_type_ && packets.ContainsPayloadType(*current_rtp_payload_type_) &&
        !EnsureDecoder(*current_rtp_payload_type_))
      return kFail;
    if (next && !EnsureDecoder(next->payload_type)) return kFail;
    // Do not constrain other named overrides or require the requested action
    // to survive the SDK's normalizations. Native failure outputs remain native.
    return NetEqImpl::GetAudio(frame, muted, rate, action);
  }

  void SetCodecs(const std::map<int, webrtc::SdpAudioFormat>& codecs) override {
    for (const auto& [pt, format] : codecs) {
      if (!StockNetEqCodecSafe(pt, format)) return;
    }
    NetEqImpl::SetCodecs(codecs);
  }
  bool RegisterPayloadType(int pt, const webrtc::SdpAudioFormat& format) override {
    return StockNetEqCodecSafe(pt, format) && NetEqImpl::RegisterPayloadType(pt, format);
  }
  void SetMaximumBufferPackets(size_t count) override {
    if (neteq_stock_buffer_packets_safe(count)) NetEqImpl::SetMaximumBufferPackets(count);
  }
  void RemoveAllPayloadTypes() override {
    // The Rust dispatch reports BufferedPackets before calling this method.
    // Also protect callers reaching this product through the SDK virtual seam.
    if (!HasBufferedPackets()) NetEqImpl::RemoveAllPayloadTypes();
  }
  bool HasBufferedPackets() const { return !packet_buffer_->Empty(); }

 private:
  const ObservedNetEqPacketBuffer& observed_packets() const {
    return static_cast<const ObservedNetEqPacketBuffer&>(*packet_buffer_);
  }
  bool EnsureDecoder(uint8_t pt) const {
    const auto* info = decoder_database_->GetDecoderInfo(pt);
    if (!info || info->IsComfortNoise() || info->IsDtmf() || info->IsRed()) return true;
    // A negative RTP clock rate violates TimestampScaler's denominator assertion.
    // Registration itself remains lazy and does not query capabilities/Create.
    if (info->GetFormat().clockrate_hz < 0 || !factory_) return false;
    auto& token = lifetimes_[pt];
    if (token && token->state.load() == NetEqDecoderLifetimeState::Consumed) return true;
    if (token && token->state.load() == NetEqDecoderLifetimeState::Prepared) return false;
    token = factory_->PrepareDecoder(*environment_, info->GetFormat());
    if (!token) return false;
    // Consume immediately for this PT. Same-format registrations are thus
    // associated by explicit sequence, never inferred from factory arguments.
    const auto* decoder = decoder_database_->GetDecoder(pt);
    if (!decoder || token->state.load() != NetEqDecoderLifetimeState::Consumed) {
      factory_->CancelPrepared();
      return false;
    }
    return true;
  }
  webrtc::scoped_refptr<PreparedNetEqDecoderFactory> factory_;
  const int64_t minimum_time_us_;
  const int64_t maximum_time_us_;
  mutable std::unordered_map<uint8_t, std::shared_ptr<NetEqDecoderLifetime>> lifetimes_;
};

bool StockNetEqHasPackets(const webrtc::NetEq& value) {
  return static_cast<const GuardedStockNetEq&>(value).HasBufferedPackets();
}
}  // namespace

std::unique_ptr<NativeNetEq> neteq_stock_create(const NativeEnvironment& environment,
    const FfiNetEqConfig& config, const NativeAudioDecoderFactory* factory,
    int64_t minimum_time_us, int64_t maximum_time_us) noexcept {
  if (!neteq_stock_config_safe(config) ||
      (factory && (!factory->factory() || factory->state().builtin_kind == 0)) ||
      minimum_time_us == std::numeric_limits<int64_t>::min() ||
      maximum_time_us == std::numeric_limits<int64_t>::max() ||
      minimum_time_us > maximum_time_us) return nullptr;
  auto retained = clone_environment(environment);
  webrtc::scoped_refptr<PreparedNetEqDecoderFactory> handoff;
  if (factory) handoff = webrtc::make_ref_counted<PreparedNetEqDecoderFactory>(*factory);
  const auto native_config = NetEqConfigFromFfi(config);
  auto deps = StockNetEqDependencies(retained->environment(), native_config, handoff);
  auto engine = std::make_unique<GuardedStockNetEq>(std::move(retained), native_config,
      std::move(handoff), std::move(deps), minimum_time_us, maximum_time_us);
  return std::make_unique<NativeNetEq>(std::move(engine), true);
}

webrtc::NetEq::Config NetEqConfigFromFfi(const FfiNetEqConfig& value) noexcept {
  webrtc::NetEq::Config result;
  result.sample_rate_hz = value.sample_rate_hz;
  result.max_packets_in_buffer = value.max_packets_in_buffer;
  result.max_delay_ms = value.max_delay_ms;
  result.min_delay_ms = value.min_delay_ms;
  result.enable_fast_accelerate = value.enable_fast_accelerate;
  result.enable_muted_state = value.enable_muted_state;
  result.enable_rtx_handling = value.enable_rtx_handling;
  result.for_test_no_time_stretching = value.for_test_no_time_stretching;
  return result;
}
FfiNetEqConfig NetEqConfigToFfi(const webrtc::NetEq::Config& value) noexcept {
  FfiNetEqConfig result{};
  result.sample_rate_hz = value.sample_rate_hz;
  result.max_packets_in_buffer = value.max_packets_in_buffer;
  result.max_delay_ms = value.max_delay_ms;
  result.min_delay_ms = value.min_delay_ms;
  result.enable_fast_accelerate = value.enable_fast_accelerate;
  result.enable_muted_state = value.enable_muted_state;
  result.enable_rtx_handling = value.enable_rtx_handling;
  result.for_test_no_time_stretching = value.for_test_no_time_stretching;
  return result;
}
webrtc::NetEqNetworkStatistics NetEqNetworkStatisticsFromFfi(const FfiNetEqNetworkStatistics& value) noexcept {
  webrtc::NetEqNetworkStatistics result;
  result.current_buffer_size_ms = value.current_buffer_size_ms;
  result.preferred_buffer_size_ms = value.preferred_buffer_size_ms;
  result.jitter_peaks_found = value.jitter_peaks_found;
  result.expand_rate = value.expand_rate;
  result.speech_expand_rate = value.speech_expand_rate;
  result.preemptive_rate = value.preemptive_rate;
  result.accelerate_rate = value.accelerate_rate;
  result.secondary_decoded_rate = value.secondary_decoded_rate;
  result.secondary_discarded_rate = value.secondary_discarded_rate;
  result.mean_waiting_time_ms = value.mean_waiting_time_ms;
  result.median_waiting_time_ms = value.median_waiting_time_ms;
  result.min_waiting_time_ms = value.min_waiting_time_ms;
  result.max_waiting_time_ms = value.max_waiting_time_ms;
  return result;
}
FfiNetEqNetworkStatistics NetEqNetworkStatisticsToFfi(const webrtc::NetEqNetworkStatistics& value) noexcept {
  FfiNetEqNetworkStatistics result{};
  result.current_buffer_size_ms = value.current_buffer_size_ms;
  result.preferred_buffer_size_ms = value.preferred_buffer_size_ms;
  result.jitter_peaks_found = value.jitter_peaks_found;
  result.expand_rate = value.expand_rate;
  result.speech_expand_rate = value.speech_expand_rate;
  result.preemptive_rate = value.preemptive_rate;
  result.accelerate_rate = value.accelerate_rate;
  result.secondary_decoded_rate = value.secondary_decoded_rate;
  result.secondary_discarded_rate = value.secondary_discarded_rate;
  result.mean_waiting_time_ms = value.mean_waiting_time_ms;
  result.median_waiting_time_ms = value.median_waiting_time_ms;
  result.min_waiting_time_ms = value.min_waiting_time_ms;
  result.max_waiting_time_ms = value.max_waiting_time_ms;
  return result;
}
webrtc::NetEqLifetimeStatistics NetEqLifetimeStatisticsFromFfi(const FfiNetEqLifetimeStatistics& value) noexcept {
  webrtc::NetEqLifetimeStatistics result;
  result.total_samples_received = value.total_samples_received;
  result.concealed_samples = value.concealed_samples;
  result.concealment_events = value.concealment_events;
  result.jitter_buffer_delay_ms = value.jitter_buffer_delay_ms;
  result.jitter_buffer_emitted_count = value.jitter_buffer_emitted_count;
  result.jitter_buffer_target_delay_ms = value.jitter_buffer_target_delay_ms;
  result.jitter_buffer_minimum_delay_ms = value.jitter_buffer_minimum_delay_ms;
  result.inserted_samples_for_deceleration = value.inserted_samples_for_deceleration;
  result.removed_samples_for_acceleration = value.removed_samples_for_acceleration;
  result.silent_concealed_samples = value.silent_concealed_samples;
  result.fec_packets_received = value.fec_packets_received;
  result.fec_packets_discarded = value.fec_packets_discarded;
  result.packets_discarded = value.packets_discarded;
  result.delayed_packet_outage_samples = value.delayed_packet_outage_samples;
  result.delayed_packet_outage_events = value.delayed_packet_outage_events;
  result.relative_packet_arrival_delay_ms = value.relative_packet_arrival_delay_ms;
  result.jitter_buffer_packets_received = value.jitter_buffer_packets_received;
  result.generated_noise_samples = value.generated_noise_samples;
  result.total_processing_delay_us = value.total_processing_delay_us;
  result.interruption_count = value.interruption_count;
  result.total_interruption_duration_ms = value.total_interruption_duration_ms;
  return result;
}
FfiNetEqLifetimeStatistics NetEqLifetimeStatisticsToFfi(const webrtc::NetEqLifetimeStatistics& value) noexcept {
  FfiNetEqLifetimeStatistics result{};
  result.total_samples_received = value.total_samples_received;
  result.concealed_samples = value.concealed_samples;
  result.concealment_events = value.concealment_events;
  result.jitter_buffer_delay_ms = value.jitter_buffer_delay_ms;
  result.jitter_buffer_emitted_count = value.jitter_buffer_emitted_count;
  result.jitter_buffer_target_delay_ms = value.jitter_buffer_target_delay_ms;
  result.jitter_buffer_minimum_delay_ms = value.jitter_buffer_minimum_delay_ms;
  result.inserted_samples_for_deceleration = value.inserted_samples_for_deceleration;
  result.removed_samples_for_acceleration = value.removed_samples_for_acceleration;
  result.silent_concealed_samples = value.silent_concealed_samples;
  result.fec_packets_received = value.fec_packets_received;
  result.fec_packets_discarded = value.fec_packets_discarded;
  result.packets_discarded = value.packets_discarded;
  result.delayed_packet_outage_samples = value.delayed_packet_outage_samples;
  result.delayed_packet_outage_events = value.delayed_packet_outage_events;
  result.relative_packet_arrival_delay_ms = value.relative_packet_arrival_delay_ms;
  result.jitter_buffer_packets_received = value.jitter_buffer_packets_received;
  result.generated_noise_samples = value.generated_noise_samples;
  result.total_processing_delay_us = value.total_processing_delay_us;
  result.interruption_count = value.interruption_count;
  result.total_interruption_duration_ms = value.total_interruption_duration_ms;
  return result;
}
webrtc::NetEqOperationsAndState NetEqOperationsAndStateFromFfi(const FfiNetEqOperationsAndState& value) noexcept {
  webrtc::NetEqOperationsAndState result;
  result.preemptive_samples = value.preemptive_samples;
  result.accelerate_samples = value.accelerate_samples;
  result.packet_buffer_flushes = value.packet_buffer_flushes;
  result.last_waiting_time_ms = value.last_waiting_time_ms;
  result.current_buffer_size_ms = value.current_buffer_size_ms;
  result.current_frame_size_ms = value.current_frame_size_ms;
  result.next_packet_available = value.next_packet_available;
  return result;
}
FfiNetEqOperationsAndState NetEqOperationsAndStateToFfi(const webrtc::NetEqOperationsAndState& value) noexcept {
  FfiNetEqOperationsAndState result{};
  result.preemptive_samples = value.preemptive_samples;
  result.accelerate_samples = value.accelerate_samples;
  result.packet_buffer_flushes = value.packet_buffer_flushes;
  result.last_waiting_time_ms = value.last_waiting_time_ms;
  result.current_buffer_size_ms = value.current_buffer_size_ms;
  result.current_frame_size_ms = value.current_frame_size_ms;
  result.next_packet_available = value.next_packet_available;
  return result;
}
webrtc::NetEq::DecoderFormat NetEqDecoderFormatFromFfi(const FfiNetEqDecoderFormat& value) noexcept {
  return {value.payload_type, value.sample_rate_hz, value.num_channels,
          AudioFormatFromFfi(value.sdp_format)};
}
FfiNetEqDecoderFormat NetEqDecoderFormatToFfi(const webrtc::NetEq::DecoderFormat& value) noexcept {
  return {value.payload_type, value.sample_rate_hz, value.num_channels,
          AudioFormatToFfi(value.sdp_format)};
}
FfiNetEqConfig neteq_config_round_trip(const FfiNetEqConfig& value) noexcept {
  return NetEqConfigToFfi(NetEqConfigFromFfi(value));
}
FfiNetEqNetworkStatistics neteq_network_statistics_round_trip(const FfiNetEqNetworkStatistics& value) noexcept {
  return NetEqNetworkStatisticsToFfi(NetEqNetworkStatisticsFromFfi(value));
}
FfiNetEqLifetimeStatistics neteq_lifetime_statistics_round_trip(const FfiNetEqLifetimeStatistics& value) noexcept {
  return NetEqLifetimeStatisticsToFfi(NetEqLifetimeStatisticsFromFfi(value));
}
FfiNetEqOperationsAndState neteq_operations_and_state_round_trip(const FfiNetEqOperationsAndState& value) noexcept {
  return NetEqOperationsAndStateToFfi(NetEqOperationsAndStateFromFfi(value));
}
FfiNetEqDecoderFormat neteq_decoder_format_round_trip(const FfiNetEqDecoderFormat& value) noexcept {
  return NetEqDecoderFormatToFfi(NetEqDecoderFormatFromFfi(value));
}

FfiNetEqConfig neteq_config_default() noexcept {
  return NetEqConfigToFfi(webrtc::NetEq::Config{});
}
rust::String neteq_config_description(const FfiNetEqConfig& value) noexcept {
  return rust::String(NetEqConfigFromFfi(value).ToString());
}

NativeAudioCodecPairId::NativeAudioCodecPairId() noexcept
    : value_(webrtc::AudioCodecPairId::Create()) {}
NativeAudioCodecPairId::NativeAudioCodecPairId(webrtc::AudioCodecPairId value) noexcept
    : value_(value) {}
std::unique_ptr<NativeAudioCodecPairId> audio_codec_pair_id_copy(const NativeAudioCodecPairId &source) noexcept {
  return std::make_unique<NativeAudioCodecPairId>(source.value());
}
const webrtc::AudioCodecPairId& NativeAudioCodecPairId::value() const noexcept { return value_; }
std::unique_ptr<NativeAudioCodecPairId> audio_codec_pair_id_create() noexcept {
  return std::make_unique<NativeAudioCodecPairId>();
}
std::uint64_t audio_codec_pair_id_numeric(const NativeAudioCodecPairId& id) noexcept {
  return id.value().NumericRepresentation();
}

std::unique_ptr<NativeAudioEncoderOptions> audio_encoder_options_default() noexcept { return std::make_unique<NativeAudioEncoderOptions>(); }
std::unique_ptr<NativeAudioEncoderOptions> audio_encoder_options_copy(const NativeAudioEncoderOptions &source) noexcept { return std::make_unique<NativeAudioEncoderOptions>(source.value()); }
void audio_encoder_options_assign(NativeAudioEncoderOptions &target, const NativeAudioEncoderOptions &source) noexcept { target.value() = source.value(); }
std::int32_t audio_encoder_options_payload_type(const NativeAudioEncoderOptions &source) noexcept { return source.value().payload_type; }
void audio_encoder_options_set_payload_type(NativeAudioEncoderOptions &target, std::int32_t value) noexcept { target.value().payload_type = value; }
std::unique_ptr<NativeAudioCodecPairId> audio_encoder_options_pair_id(const NativeAudioEncoderOptions &source) noexcept {
  return source.value().codec_pair_id ? std::make_unique<NativeAudioCodecPairId>(*source.value().codec_pair_id) : nullptr;
}
void audio_encoder_options_set_pair_id(NativeAudioEncoderOptions &target, const std::unique_ptr<NativeAudioCodecPairId> &id) noexcept {
  target.value().codec_pair_id = id ? std::optional<webrtc::AudioCodecPairId>(id->value()) : std::nullopt;
}

static FfiAudioDecoderCreation CreateAudioDecoder(const NativeAudioDecoderFactory& factory,
    const NativeEnvironment& environment, const FfiSdpAudioFormat& format,
    bool compatibility, std::optional<webrtc::AudioCodecPairId> pair_id) noexcept {
  FfiAudioDecoderCreation result{};
  const auto supported = audio_decoder_supported(factory, format);
  if (supported < 0) return result;
  result.valid = true;
  if (supported == 0) return result;
  const auto native = ToNative(format);
  // These are the stock template's actual first two selectors. Multiopus can
  // accept an unrelated SDP name/rate; never infer its backend from that name.
  auto kind = OwnedAudioDecoderKind::L16;
  if (webrtc::AudioDecoderOpus::SdpToConfig(native) ||
      webrtc::AudioDecoderMultiChannelOpus::SdpToConfig(native)) {
    kind = OwnedAudioDecoderKind::Opus;
  } else if (absl::EqualsIgnoreCase(native.name, "G722")) {
    kind = OwnedAudioDecoderKind::G722;
  } else if (absl::EqualsIgnoreCase(native.name, "PCMU") ||
             absl::EqualsIgnoreCase(native.name, "PCMA")) {
    kind = OwnedAudioDecoderKind::G711;
  }
  auto env = environment.environment();
  auto decoder = compatibility ? factory.factory()->Create(env, native, pair_id)
                               : factory.factory()->Create(env, native);
  // Preserve every native null result, including zero-stream multiopus and
  // allocation failure. A supported description need not construct a decoder.
  if (!decoder) return result;
  const auto channels = decoder->Channels();
  if (channels == 0 || channels > webrtc::AudioDecoder::kMaxNumberOfChannels ||
      decoder->SampleRateHz() <= 0 || decoder->SampleRateHz() > 48000) {
    result.valid = false;
    return result;
  }
  result.decoder = std::make_unique<NativeAudioDecoder>(
      std::make_shared<NativeAudioDecoder::State>(std::move(env), std::move(decoder), kind));
  return result;
}
FfiAudioDecoderCreation audio_decoder_create(const NativeAudioDecoderFactory& factory,
    const NativeEnvironment& environment, const FfiSdpAudioFormat& format) noexcept {
  return CreateAudioDecoder(factory, environment, format, false, std::nullopt);
}
FfiAudioDecoderCreation audio_decoder_create_with_pair_id(const NativeAudioDecoderFactory& factory,
    const NativeEnvironment& environment, const FfiSdpAudioFormat& format,
    const std::unique_ptr<NativeAudioCodecPairId>& pair_id) noexcept {
  return CreateAudioDecoder(factory, environment, format, true,
      pair_id ? std::optional<webrtc::AudioCodecPairId>(pair_id->value()) : std::nullopt);
}
std::int32_t audio_decoder_sample_rate(const NativeAudioDecoder& decoder) noexcept {
  return decoder.state()->sample_rate;
}
std::size_t audio_decoder_channels(const NativeAudioDecoder& decoder) noexcept {
  return decoder.state()->channels;
}
void audio_decoder_reset(const NativeAudioDecoder& decoder) noexcept {
  const auto& state = *decoder.state();
  std::lock_guard lock(state.mutex);
  state.decoder->Reset();
}
std::int32_t audio_decoder_error_code(const NativeAudioDecoder& decoder) noexcept {
  const auto& state = *decoder.state();
  std::lock_guard lock(state.mutex);
  return state.decoder->ErrorCode();
}
FfiAudioPacketInfo audio_decoder_packet_info(const NativeAudioDecoder& decoder,
    rust::Slice<const std::uint8_t> encoded) noexcept {
  const auto& state = *decoder.state();
  FfiAudioPacketInfo result{};
  const auto maximum = static_cast<std::size_t>(std::numeric_limits<int>::max());
  // Whole-payload duration queries narrow signed counts. Use the selected
  // implementation's exact scale instead of a curated common packet-size cap.
  switch (state.kind) {
    case OwnedAudioDecoderKind::Opus:
      if (encoded.size() > maximum) return result;
      break;
    case OwnedAudioDecoderKind::G722:
      if (encoded.size() / state.channels > maximum / 2) return result;
      break;
    case OwnedAudioDecoderKind::G711:
      if (encoded.size() / state.channels > maximum) return result;
      break;
    case OwnedAudioDecoderKind::L16:
      if (encoded.size() / (2 * state.channels) > maximum) return result;
      break;
  }
  std::lock_guard lock(state.mutex);
  result.duration = state.decoder->PacketDuration(encoded.data(), encoded.size());
  result.redundant_duration = state.decoder->PacketDurationRedundant(encoded.data(), encoded.size());
  result.has_fec = state.decoder->PacketHasFec(encoded.data(), encoded.size());
  result.valid = true;
  return result;
}
FfiParsedAudioFrames audio_decoder_parse(const NativeAudioDecoder& decoder,
    rust::Slice<const std::uint8_t> encoded, std::uint32_t timestamp) noexcept {
  const auto& owner = decoder.state();
  FfiParsedAudioFrames result{};
  if (owner->kind == OwnedAudioDecoderKind::Opus &&
      encoded.size() > static_cast<std::size_t>(std::numeric_limits<int>::max())) return result;
  std::lock_guard lock(owner->mutex);
  // SplitBySamples DCHECKs data() even for empty payloads. Allocate one byte
  // of capacity without changing the size/content of the input buffer.
  webrtc::Buffer payload(encoded.data(), encoded.size(), std::max<std::size_t>(1, encoded.size()));
  auto parsed = owner->decoder->ParsePayload(std::move(payload), timestamp);
  result.valid = true;
  result.frames.reserve(parsed.size());
  for (auto& value : parsed) {
    FfiParsedAudioFrame frame{};
    frame.timestamp = value.timestamp;
    frame.priority = value.priority;
    if (value.frame) {
      auto state = std::make_unique<NativeDecodableAudioFrame::State>();
      state->decoder = owner;
      state->frame = std::move(value.frame);
      frame.frame = std::make_unique<NativeDecodableAudioFrame>(std::move(state));
    }
    result.frames.push_back(std::move(frame));
  }
  return result;
}
FfiAudioPlc audio_decoder_generate_plc(const NativeAudioDecoder& decoder,
    std::size_t requested_samples, rust::Slice<const std::int16_t> prefix) noexcept {
  const auto& state = *decoder.state();
  FfiAudioPlc result{};
  const auto bound = std::size_t{5760} * state.channels;
  const auto maximum = static_cast<std::size_t>(std::numeric_limits<std::ptrdiff_t>::max()) / sizeof(std::int16_t);
  if (prefix.size() > maximum - bound) return result;
  std::lock_guard lock(state.mutex);
  webrtc::BufferT<std::int16_t> buffer(prefix.data(), prefix.size());
  state.decoder->GeneratePlc(requested_samples, &buffer);
  if (buffer.size() < prefix.size() || buffer.size() - prefix.size() > bound) return result;
  result.samples.reserve(buffer.size());
  for (const auto sample : buffer) result.samples.push_back(sample);
  result.valid = true;
  return result;
}
std::size_t audio_frame_duration(const NativeDecodableAudioFrame& frame) noexcept {
  const auto& state = frame.state();
  std::lock_guard lock(state.decoder->mutex);
  return state.frame->Duration();
}
bool audio_frame_is_dtx(const NativeDecodableAudioFrame& frame) noexcept {
  const auto& state = frame.state();
  std::lock_guard lock(state.decoder->mutex);
  return state.frame->IsDtxPacket();
}
FfiDecodedAudio audio_frame_decode(const NativeDecodableAudioFrame& frame) noexcept {
  const auto& state = frame.state();
  std::lock_guard lock(state.decoder->mutex);
  FfiDecodedAudio result{};
  if (state.decoded) { result.status = 2; return result; }
  state.decoded = true;
  // Stock PCM/G722 SplitBySamples emits chunks shorter than 40 ms. Stock
  // Opus/multiopus primary/FEC/PLC output is at most 120 ms at <=48 kHz.
  // This proof is backend-specific, not a promise for future custom decoders.
  const auto bound = std::size_t{5760} * state.decoder->channels;
  if (state.frame->Duration() > bound / state.decoder->channels) {
    result.status = 3;
    return result;
  }
  std::vector<std::int16_t> output(bound);
  const auto decoded = state.frame->Decode(std::span<std::int16_t>(output));
  if (!decoded) return result;
  if (decoded->num_decoded_samples > output.size()) { result.status = 3; return result; }
  result.samples.reserve(decoded->num_decoded_samples);
  for (std::size_t i = 0; i < decoded->num_decoded_samples; ++i) result.samples.push_back(output[i]);
  result.speech_type = static_cast<std::int32_t>(decoded->speech_type);
  result.status = 1;
  return result;
}

namespace {
// A standalone leaf, not an AudioDecoder with stubbed legacy virtuals. The Rust
// lease retains the real caller decoder/frame state, including its environment.
class CallerSdkEncodedAudioFrame final : public webrtc::AudioDecoder::EncodedAudioFrame {
 public:
  explicit CallerSdkEncodedAudioFrame(rust::Box<RustCallerAudioFrameCall> source) : source_(std::move(source)) {}
  // Only the private SDK Parse driver retires this exact leaf after extracting
  // the original Rust lease; no pointer or envelope escapes its native decoder.
  rust::Box<RustCallerAudioFrameCall> TakeSource() { return std::move(source_); }
  std::size_t Duration() const override { return audio_caller_frame_duration_callback(*source_); }
  bool IsDtxPacket() const override { return audio_caller_frame_dtx_callback(*source_); }
  std::optional<DecodeResult> Decode(std::span<std::int16_t> decoded) const override {
    if (attempted_) {
      audio_caller_frame_guard_error(*source_, 2);
      return std::nullopt;
    }
    attempted_ = true;
    // Also defend raw native spans before Rust/DSP dispatch. Safe Rust callers
    // already supply representable exclusive slices. A capacity refusal does
    // not consume the shared Rust attempt; this one-shot SDK object is retired.
    if (decoded.size() > static_cast<std::size_t>(PTRDIFF_MAX) / sizeof(std::int16_t)) {
      audio_caller_frame_guard_error(*source_, 0);
      return std::nullopt;
    }
    const auto output = audio_caller_frame_decode_callback(*source_, decoded.size());
    if (output.status == 0) return std::nullopt; // Receipt distinguishes Err/None.
    // Pinned non-fixed enum {1,2} has a defined cast range [0,3], including
    // unnamed native values. Preserve those, not an arbitrary named-tag-only
    // whitelist; an unbounded Rust i32 cannot be cast before this guard.
    static_assert(webrtc::AudioDecoder::kSpeech == 1 && webrtc::AudioDecoder::kComfortNoise == 2);
    if (output.status != 1 || output.samples.size() > decoded.size() ||
        output.speech_type < 0 || output.speech_type > 3) {
      audio_caller_frame_guard_error(*source_, 1);
      return std::nullopt;
    }
    const auto speech = static_cast<webrtc::AudioDecoder::SpeechType>(output.speech_type);
    // Validate everything before the first write. Failure/None cannot corrupt
    // the caller prefix; successful short/empty output need not fill the span.
    if (!output.samples.empty()) std::copy(output.samples.begin(), output.samples.end(), decoded.begin());
    return DecodeResult{output.samples.size(), speech};
  }
 private:
  rust::Box<RustCallerAudioFrameCall> source_;
  mutable bool attempted_ = false;
};
class CallerSdkAudioDecoder final : public webrtc::AudioDecoder {
 public:
  explicit CallerSdkAudioDecoder(rust::Box<RustCallerAudioDecoderCall> source) : source_(std::move(source)) {}
  class CallScope {
   public:
    CallScope(CallerSdkAudioDecoder& decoder, const RustCallerAudioDecoderCall& source)
        : decoder_(decoder), previous_(decoder.active_) { decoder_.active_ = &source; }
    ~CallScope() { decoder_.active_ = previous_; }
    CallScope(const CallScope&) = delete;
    CallScope& operator=(const CallScope&) = delete;
   private:
    CallerSdkAudioDecoder& decoder_;
    const RustCallerAudioDecoderCall* previous_;
  };
  int SampleRateHz() const override { return audio_caller_decoder_scalar_callback(source(), 0); }
  std::size_t Channels() const override { return audio_caller_decoder_channels_callback(source()); }
  void Reset() override { audio_caller_decoder_reset_callback(source()); }
  int ErrorCode() override { return audio_caller_decoder_scalar_callback(source(), 1); }
  int PacketDuration(const std::uint8_t* encoded, std::size_t size) const override {
    return audio_caller_decoder_packet_duration_callback(source(), Payload(encoded, size), false);
  }
  int PacketDurationRedundant(const std::uint8_t* encoded, std::size_t size) const override {
    return audio_caller_decoder_packet_duration_callback(source(), Payload(encoded, size), true);
  }
  bool PacketHasFec(const std::uint8_t* encoded, std::size_t size) const override {
    return audio_caller_decoder_packet_fec_callback(source(), Payload(encoded, size));
  }
  std::vector<ParseResult> ParsePayload(webrtc::Buffer&& payload, std::uint32_t timestamp) override {
    rust::Vec<std::uint8_t> bytes;
    bytes.reserve(payload.size());
    for (const auto byte : payload) bytes.push_back(byte);
    auto parsed = audio_caller_decoder_parse_callback(source(), std::move(bytes), timestamp);
    std::vector<ParseResult> result;
    result.reserve(parsed.size());
    for (auto& item : parsed) {
      std::unique_ptr<EncodedAudioFrame> frame;
      if (audio_caller_parsed_frame_present(*item.transfer)) {
        // rust::Vec drops its shared elements as Rust values: never leave a
        // moved-from/null rust::Box in their Box<T> fields. Swap with a valid
        // absent transfer before moving the now C++-local owning box.
        auto owner = audio_caller_parsed_frame_null(source());
        std::swap(owner, item.transfer);
        frame = std::make_unique<CallerSdkEncodedAudioFrame>(audio_caller_parsed_frame_bind(std::move(owner)));
      }
      result.emplace_back(item.timestamp, item.priority, std::move(frame));
    }
    return result;
  }
  void GeneratePlc(std::size_t requested, webrtc::BufferT<std::int16_t>* buffer) override {
    const auto prefix_size = buffer->size();
    static const std::int16_t empty = 0; // Non-null storage for an empty slice, never PCM.
    const auto prefix = rust::Slice<const std::int16_t>(prefix_size ? buffer->data() : &empty, prefix_size);
    const auto output = audio_caller_decoder_plc_callback(source(), requested, prefix);
    if (!output.valid) return; // This operation's receipt refuses apparent no-output success.
    if (output.samples.size() < prefix_size ||
        output.samples.size() - prefix_size > audio_caller_decoder_plc_capacity(source()) ||
        (prefix_size && !std::equal(buffer->data(), buffer->data() + prefix_size, output.samples.begin()))) {
      audio_caller_decoder_plc_invalid(source());
      return;
    }
    const auto appended = output.samples.size() - prefix_size;
    if (appended) buffer->AppendData(output.samples.data() + prefix_size, appended);
  }
  const RustCallerAudioDecoderCall& source() const { return active_ ? *active_ : *source_; }
 protected:
  int DecodeInternal(const std::uint8_t*, std::size_t, int, std::int16_t*, SpeechType*) override {
    // Human-approved explicit incompatibility: do not read payload or touch
    // unsized PCM/speech pointers, do not invent CNG/PLC or choose another DSP.
    // Base redundant routing reaches this same refusal. Legacy PLC defaults
    // remain native false/zero. This is not installed internal-CNG support.
    audio_caller_decoder_compatibility_refused(source());
    return -1;
  }
 private:
  static rust::Slice<const std::uint8_t> Payload(const std::uint8_t* data, std::size_t size) {
    static const std::uint8_t empty = 0; // Empty span needs valid Rust slice storage.
    return {size ? data : &empty, size};
  }
  rust::Box<RustCallerAudioDecoderCall> source_;
  const RustCallerAudioDecoderCall* active_ = nullptr;
};
} // namespace
std::size_t audio_caller_frame_sdk_duration(rust::Box<RustCallerAudioFrameCall> source) noexcept {
  CallerSdkEncodedAudioFrame frame(std::move(source));
  const webrtc::AudioDecoder::EncodedAudioFrame& sdk = frame;
  return sdk.Duration();
}
bool audio_caller_frame_sdk_dtx(rust::Box<RustCallerAudioFrameCall> source) noexcept {
  CallerSdkEncodedAudioFrame frame(std::move(source));
  const webrtc::AudioDecoder::EncodedAudioFrame& sdk = frame;
  return sdk.IsDtxPacket();
}
FfiAudioFrameDecodeInto audio_caller_frame_sdk_decode(rust::Box<RustCallerAudioFrameCall> source,
    rust::Slice<std::int16_t> output) noexcept {
  CallerSdkEncodedAudioFrame frame(std::move(source));
  const webrtc::AudioDecoder::EncodedAudioFrame& sdk = frame;
  const auto decoded = sdk.Decode(std::span<std::int16_t>(output.data(), output.size()));
  FfiAudioFrameDecodeInto result{};
  if (decoded) {
    result.present = true;
    result.num_decoded_samples = decoded->num_decoded_samples;
    result.speech_type = static_cast<std::int32_t>(decoded->speech_type);
  }
  return result;
}

std::int32_t audio_caller_decoder_sdk_scalar(rust::Box<RustCallerAudioDecoderCall> source, std::uint8_t kind) noexcept {
  CallerSdkAudioDecoder decoder(std::move(source));
  webrtc::AudioDecoder& sdk = decoder;
  return kind == 0 ? sdk.SampleRateHz() : sdk.ErrorCode();
}
std::size_t audio_caller_decoder_sdk_channels(rust::Box<RustCallerAudioDecoderCall> source) noexcept {
  CallerSdkAudioDecoder decoder(std::move(source));
  const webrtc::AudioDecoder& sdk = decoder;
  return sdk.Channels();
}
void audio_caller_decoder_sdk_reset(rust::Box<RustCallerAudioDecoderCall> source) noexcept {
  CallerSdkAudioDecoder decoder(std::move(source));
  webrtc::AudioDecoder& sdk = decoder;
  sdk.Reset();
}
std::int32_t audio_caller_decoder_sdk_packet_duration(rust::Box<RustCallerAudioDecoderCall> source,
    rust::Slice<const std::uint8_t> encoded, bool redundant) noexcept {
  CallerSdkAudioDecoder decoder(std::move(source));
  const webrtc::AudioDecoder& sdk = decoder;
  return redundant ? sdk.PacketDurationRedundant(encoded.data(), encoded.size()) : sdk.PacketDuration(encoded.data(), encoded.size());
}
bool audio_caller_decoder_sdk_packet_fec(rust::Box<RustCallerAudioDecoderCall> source,
    rust::Slice<const std::uint8_t> encoded) noexcept {
  CallerSdkAudioDecoder decoder(std::move(source));
  const webrtc::AudioDecoder& sdk = decoder;
  return sdk.PacketHasFec(encoded.data(), encoded.size());
}
namespace {
rust::Vec<FfiCallerParsedAudioFrame> ParseCallerDecoderSdk(CallerSdkAudioDecoder& decoder,
    rust::Slice<const std::uint8_t> encoded, std::uint32_t timestamp) {
  webrtc::AudioDecoder& sdk = decoder;
  webrtc::Buffer payload(encoded.data(), encoded.size());
  auto parsed = sdk.ParsePayload(std::move(payload), timestamp);
  rust::Vec<FfiCallerParsedAudioFrame> result;
  result.reserve(parsed.size());
  for (auto& item : parsed) {
    // Exact private constructor provenance: this decoder only creates the
    // caller leaf above; no guessed decoder kind/format or stock frame bound.
    auto owner = item.frame ? audio_caller_parsed_frame_from_sdk(
        static_cast<CallerSdkEncodedAudioFrame&>(*item.frame).TakeSource())
        : audio_caller_parsed_frame_null(decoder.source());
    result.push_back({item.timestamp, item.priority, std::move(owner)});
  }
  // Every native frame retires here before its SDK decoder can retire; returned
  // Rust leases independently retain actual caller decoder/environment state.
  return result;
}
FfiAudioPlc GenerateCallerDecoderPlcSdk(CallerSdkAudioDecoder& decoder,
    std::size_t requested, rust::Slice<const std::int16_t> prefix) {
  webrtc::AudioDecoder& sdk = decoder;
  webrtc::BufferT<std::int16_t> buffer(prefix.data(), prefix.size());
  sdk.GeneratePlc(requested, &buffer);
  FfiAudioPlc result{};
  result.valid = true; // Callback/guard error remains in the independent receipt.
  result.samples.reserve(buffer.size());
  for (const auto sample : buffer) result.samples.push_back(sample);
  return result;
}
} // namespace
rust::Vec<FfiCallerParsedAudioFrame> audio_caller_decoder_sdk_parse(rust::Box<RustCallerAudioDecoderCall> source,
    rust::Slice<const std::uint8_t> encoded, std::uint32_t timestamp) noexcept {
  CallerSdkAudioDecoder decoder(std::move(source));
  return ParseCallerDecoderSdk(decoder, encoded, timestamp);
}
FfiAudioPlc audio_caller_decoder_sdk_plc(rust::Box<RustCallerAudioDecoderCall> source,
    std::size_t requested, rust::Slice<const std::int16_t> prefix) noexcept {
  CallerSdkAudioDecoder decoder(std::move(source));
  return GenerateCallerDecoderPlcSdk(decoder, requested, prefix);
}
std::int32_t audio_caller_decoder_sdk_legacy_refusal(rust::Box<RustCallerAudioDecoderCall> source,
    bool redundant, rust::Slice<std::int16_t> output, std::int32_t& speech) noexcept {
  CallerSdkAudioDecoder decoder(std::move(source));
  webrtc::AudioDecoder& sdk = decoder;
  auto native_speech = webrtc::AudioDecoder::kComfortNoise;
  const auto result = redundant ? sdk.DecodeRedundant(nullptr, 0, 8000, output.size() * sizeof(std::int16_t), output.data(), &native_speech)
      : sdk.Decode(nullptr, 0, 8000, output.size() * sizeof(std::int16_t), output.data(), &native_speech);
  speech = static_cast<std::int32_t>(native_speech);
  return result;
}

struct NativeCallerAudioDecoderProduct::State {
  explicit State(rust::Box<RustCallerAudioDecoderCall> source)
      : decoder(std::make_unique<CallerSdkAudioDecoder>(std::move(source))) {}
  std::unique_ptr<CallerSdkAudioDecoder> decoder;
};
NativeCallerAudioDecoderProduct::NativeCallerAudioDecoderProduct(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeCallerAudioDecoderProduct::~NativeCallerAudioDecoderProduct() = default;
std::unique_ptr<NativeCallerAudioDecoderProduct> audio_caller_decoder_product_new(rust::Box<RustCallerAudioDecoderCall> source) noexcept {
  return std::make_unique<NativeCallerAudioDecoderProduct>(std::make_unique<NativeCallerAudioDecoderProduct::State>(std::move(source)));
}
std::int32_t audio_caller_decoder_product_scalar(const NativeCallerAudioDecoderProduct& owner,
    const RustCallerAudioDecoderCall& source, std::uint8_t kind) noexcept {
  auto& decoder = *owner.state()->decoder;
  CallerSdkAudioDecoder::CallScope call(decoder, source);
  webrtc::AudioDecoder& sdk = decoder;
  return kind == 0 ? sdk.SampleRateHz() : sdk.ErrorCode();
}
std::size_t audio_caller_decoder_product_channels(const NativeCallerAudioDecoderProduct& owner,
    const RustCallerAudioDecoderCall& source) noexcept {
  auto& decoder = *owner.state()->decoder;
  CallerSdkAudioDecoder::CallScope call(decoder, source);
  const webrtc::AudioDecoder& sdk = decoder;
  return sdk.Channels();
}
void audio_caller_decoder_product_reset(const NativeCallerAudioDecoderProduct& owner,
    const RustCallerAudioDecoderCall& source) noexcept {
  auto& decoder = *owner.state()->decoder;
  CallerSdkAudioDecoder::CallScope call(decoder, source);
  webrtc::AudioDecoder& sdk = decoder;
  sdk.Reset();
}
std::int32_t audio_caller_decoder_product_duration(const NativeCallerAudioDecoderProduct& owner,
    const RustCallerAudioDecoderCall& source, rust::Slice<const std::uint8_t> encoded, bool redundant) noexcept {
  auto& decoder = *owner.state()->decoder;
  CallerSdkAudioDecoder::CallScope call(decoder, source);
  const webrtc::AudioDecoder& sdk = decoder;
  return redundant ? sdk.PacketDurationRedundant(encoded.data(), encoded.size()) : sdk.PacketDuration(encoded.data(), encoded.size());
}
bool audio_caller_decoder_product_fec(const NativeCallerAudioDecoderProduct& owner,
    const RustCallerAudioDecoderCall& source, rust::Slice<const std::uint8_t> encoded) noexcept {
  auto& decoder = *owner.state()->decoder;
  CallerSdkAudioDecoder::CallScope call(decoder, source);
  const webrtc::AudioDecoder& sdk = decoder;
  return sdk.PacketHasFec(encoded.data(), encoded.size());
}
rust::Vec<FfiCallerParsedAudioFrame> audio_caller_decoder_product_parse(const NativeCallerAudioDecoderProduct& owner,
    const RustCallerAudioDecoderCall& source, rust::Slice<const std::uint8_t> encoded, std::uint32_t timestamp) noexcept {
  auto& decoder = *owner.state()->decoder;
  CallerSdkAudioDecoder::CallScope call(decoder, source);
  return ParseCallerDecoderSdk(decoder, encoded, timestamp);
}
FfiAudioPlc audio_caller_decoder_product_plc(const NativeCallerAudioDecoderProduct& owner,
    const RustCallerAudioDecoderCall& source, std::size_t requested, rust::Slice<const std::int16_t> prefix) noexcept {
  auto& decoder = *owner.state()->decoder;
  CallerSdkAudioDecoder::CallScope call(decoder, source);
  return GenerateCallerDecoderPlcSdk(decoder, requested, prefix);
}

rust::Vec<std::uint8_t> audio_format_description(const FfiSdpAudioFormat& format) noexcept {
  return AudioBytes(absl::StrCat(ToNative(format)));
}
FfiAudioCodecInfo audio_codec_info_create(std::int32_t sample_rate_hz, std::size_t num_channels,
    std::int32_t default_bitrate_bps, std::int32_t min_bitrate_bps,
    std::int32_t max_bitrate_bps, bool use_range, bool &valid) noexcept {
  valid = true;
#if RTC_DCHECK_IS_ON
  // Guard only constructor assertions actually compiled into this artifact.
  const auto minimum = use_range ? min_bitrate_bps : default_bitrate_bps;
  const auto maximum = use_range ? max_bitrate_bps : default_bitrate_bps;
  if (sample_rate_hz <= 0 || num_channels == 0 || minimum < 0 ||
      minimum > default_bitrate_bps || maximum < default_bitrate_bps) {
    valid = false;
    return {};
  }
#endif
  if (use_range) return FromNative(webrtc::AudioCodecInfo(sample_rate_hz, num_channels,
      default_bitrate_bps, min_bitrate_bps, max_bitrate_bps));
  return FromNative(webrtc::AudioCodecInfo(sample_rate_hz, num_channels, default_bitrate_bps));
}
bool audio_codec_info_has_fixed_bitrate(const FfiAudioCodecInfo &info, bool &valid) noexcept {
  valid = true;
#if RTC_DCHECK_IS_ON
  if (info.min_bitrate_bps < 0 || info.min_bitrate_bps > info.default_bitrate_bps ||
      info.max_bitrate_bps < info.default_bitrate_bps) {
    valid = false;
    return false;
  }
#endif
  return AudioInfo(info).HasFixedBitrate();
}
rust::Vec<std::uint8_t> audio_codec_info_description(const FfiAudioCodecInfo& info) noexcept {
  return AudioBytes(absl::StrCat(AudioInfo(info)));
}
rust::Vec<std::uint8_t> audio_codec_spec_description(const FfiAudioCodecSpec& spec) noexcept {
  return AudioBytes(absl::StrCat(webrtc::AudioCodecSpec{ToNative(spec.format), AudioInfo(spec.info)}));
}
std::uint64_t native_codec_thread_token() noexcept {
  return static_cast<std::uint64_t>(std::hash<std::thread::id>{}(std::this_thread::get_id()));
}

bool video_scalability_mode_valid(rust::Str name) noexcept {
  return webrtc::ScalabilityModeStringToEnum(std::string(name)).has_value();
}

std::unique_ptr<NativeSdpVideoFormatList> video_encoder_formats_value(const NativeVideoEncoderFactory& factory) noexcept {
  if (factory.rust_factory()) return static_cast<RustEncoderFactory&>(factory.factory()).FormatsValue();
  auto result = std::make_unique<NativeSdpVideoFormatList>(); result->values = factory.factory().GetSupportedFormats(); return result;
}
std::unique_ptr<NativeSdpVideoFormatList> video_encoder_implementations_value(const NativeVideoEncoderFactory& factory) noexcept {
  if (factory.rust_factory()) return static_cast<RustEncoderFactory&>(factory.factory()).ImplementationsValue();
  auto result = std::make_unique<NativeSdpVideoFormatList>(); result->values = factory.factory().GetImplementations(); return result;
}
std::unique_ptr<NativeSdpVideoFormatList> video_decoder_formats_value(const NativeVideoDecoderFactory& factory) noexcept {
  if (factory.rust_factory()) return static_cast<RustDecoderFactory&>(factory.factory()).FormatsValue();
  auto result = std::make_unique<NativeSdpVideoFormatList>(); result->values = factory.factory().GetSupportedFormats(); return result;
}
FfiVideoFactorySupport video_encoder_query_value(
    const NativeVideoEncoderFactory &factory, const NativeSdpVideoFormat &format,
    bool has_mode, rust::Slice<const std::uint8_t> mode,
    const NativeRtpResolution *resolution, bool legacy) noexcept {
  auto optional_resolution = resolution ? std::optional<webrtc::Resolution>(resolution->value()) : std::nullopt;
  auto optional_mode = has_mode ? std::optional<std::string>(
      std::string(reinterpret_cast<const char *>(mode.data()), mode.size())) : std::nullopt;
  if (factory.rust_factory()) {
    // Closed producer graph: only new_video_encoder_factory sets this tag,
    // and it creates exactly RustEncoderFactory (no RTTI/foreign cast).
    return static_cast<RustEncoderFactory &>(factory.factory()).QueryValue(
        format.value(), std::move(optional_mode), optional_resolution, legacy);
  }
  // Actual selected builtin's query uses this same pure supported-format list.
  // Guard its reached fuzzy-match arithmetic/diagnostic sinks without invoking
  // the helper twice or replacing its result. Signed resolution is unused.
  const auto status = SdpVideoFormatFuzzyMatchStatus(
      factory.factory().GetSupportedFormats(), format.value());
  if (status != 0) return {{false, false}, static_cast<std::uint8_t>(status == 1 ? 7 : 8)};
  auto support = legacy ? factory.factory().QueryCodecSupport(format.value(), std::move(optional_mode))
                        : factory.factory().QueryCodecSupport(format.value(), std::move(optional_mode), optional_resolution);
  return {{support.is_supported, support.is_power_efficient}, 0};
}
FfiVideoFactorySupport video_decoder_query_value(
    const NativeVideoDecoderFactory &factory, const NativeSdpVideoFormat &format,
    bool reference_scaling, const NativeRtpResolution *resolution, bool legacy) noexcept {
  auto optional_resolution = resolution ? std::optional<webrtc::Resolution>(resolution->value()) : std::nullopt;
  if (factory.rust_factory()) {
    return static_cast<RustDecoderFactory &>(factory.factory()).QueryValue(
        format.value(), reference_scaling, optional_resolution, legacy);
  }
  auto support = legacy ? factory.factory().QueryCodecSupport(format.value(), reference_scaling)
                        : factory.factory().QueryCodecSupport(format.value(), reference_scaling, optional_resolution);
  return {{support.is_supported, support.is_power_efficient}, 0};
}
rust::Vec<FfiCodecFormat>
video_encoder_formats(const NativeVideoEncoderFactory &factory) noexcept {
  rust::Vec<FfiCodecFormat> result;
  for (const auto &f : factory.factory().GetSupportedFormats())
    result.push_back(FromNative(f));
  return result;
}
FfiCodecSupport video_encoder_query(const NativeVideoEncoderFactory &factory,
                                    const FfiCodecFormat &format,
                                    bool has_mode, rust::Str mode, bool has,
                                    std::uint32_t width,
                                    std::uint32_t height) noexcept {
  auto support = factory.factory().QueryCodecSupport(
      ToNative(format),
      has_mode ? std::optional<std::string>(std::string(mode)) : std::nullopt,
      Resolution(has, width, height));
  return {support.is_supported, support.is_power_efficient};
}
rust::Vec<FfiCodecFormat>
video_decoder_formats(const NativeVideoDecoderFactory &factory) noexcept {
  rust::Vec<FfiCodecFormat> result;
  for (const auto &f : factory.factory().GetSupportedFormats())
    result.push_back(FromNative(f));
  return result;
}
FfiCodecSupport video_decoder_query(const NativeVideoDecoderFactory &factory,
                                    const FfiCodecFormat &format,
                                    bool reference_scaling, bool has,
                                    std::uint32_t width,
                                    std::uint32_t height) noexcept {
  auto support = factory.factory().QueryCodecSupport(
      ToNative(format), reference_scaling, Resolution(has, width, height));
  return {support.is_supported, support.is_power_efficient};
}

std::uint32_t native_video_frame_width(const NativeVideoFrame &frame) noexcept {
  return frame.state().width;
}
std::uint32_t
native_video_frame_height(const NativeVideoFrame &frame) noexcept {
  return frame.state().height;
}
std::int64_t
native_video_frame_timestamp_us(const NativeVideoFrame &frame) noexcept {
  return frame.state().timestamp_us;
}
std::uint32_t
native_video_frame_rtp_timestamp(const NativeVideoFrame &frame) noexcept {
  return frame.state().rtp_timestamp;
}
std::uint16_t native_video_frame_rotation(const NativeVideoFrame &frame) noexcept {
  return frame.state().rotation;
}
rust::Vec<std::uint8_t>
native_video_frame_i420(const NativeVideoFrame &frame) noexcept {
  return ToRust(frame.state().i420);
}
std::uint32_t
native_encoded_frame_width(const NativeEncodedVideoFrame &frame) noexcept {
  return frame.state().width;
}
std::uint32_t
native_encoded_frame_height(const NativeEncodedVideoFrame &frame) noexcept {
  return frame.state().height;
}
std::uint32_t native_encoded_frame_rtp_timestamp(
    const NativeEncodedVideoFrame &frame) noexcept {
  return frame.state().rtp_timestamp;
}
bool native_encoded_frame_key(const NativeEncodedVideoFrame &frame) noexcept {
  return frame.state().key_frame;
}
std::int32_t
native_encoded_frame_qp(const NativeEncodedVideoFrame &frame) noexcept {
  return frame.state().qp;
}
rust::Vec<std::uint8_t>
native_encoded_frame_data(const NativeEncodedVideoFrame &frame) noexcept {
  return ToRust(frame.state().data);
}

bool encoded_callback_emit(const NativeEncodedImageCallback &callback,
                           rust::Slice<const std::uint8_t> data,
                           std::uint32_t width, std::uint32_t height,
                           std::uint32_t rtp_timestamp, bool key_frame,
                           std::int32_t qp,
                           const FfiEncodedVideoMetadata &metadata) noexcept {
  if (encoded_callback_retired(callback)) return false;
  auto state = callback.state();
  webrtc::EncodedImage image;
  image.SetEncodedData(
      webrtc::EncodedImageBuffer::Create(data.data(), data.size()));
  image._encodedWidth = width;
  image._encodedHeight = height;
  image.SetRtpTimestamp(rtp_timestamp);
  image.SetFrameType(key_frame ? webrtc::VideoFrameType::kVideoFrameKey
                               : webrtc::VideoFrameType::kVideoFrameDelta);
  image.qp_ = qp;
  if (metadata.simulcast_index < -1 ||
      metadata.simulcast_index >= static_cast<int>(webrtc::kMaxSimulcastStreams) ||
      metadata.spatial_index < -1 ||
      metadata.spatial_index >= static_cast<int>(webrtc::kMaxSpatialLayers) ||
      metadata.temporal_index < -1 ||
      metadata.temporal_index >= static_cast<int>(webrtc::kMaxTemporalStreams) ||
      metadata.num_spatial_layers == 0 ||
      metadata.num_spatial_layers > webrtc::kMaxVp9NumberOfSpatialLayers)
    return false;
  if (metadata.simulcast_index >= 0)
    image.SetSimulcastIndex(metadata.simulcast_index);
  if (metadata.spatial_index >= 0)
    image.SetSpatialIndex(metadata.spatial_index);
  if (metadata.temporal_index >= 0)
    image.SetTemporalIndex(metadata.temporal_index);
  const auto codec = state->codec_type;
  if (metadata.codec != 0 &&
      !((metadata.codec == 1 && codec == webrtc::kVideoCodecVP8) ||
        (metadata.codec == 2 && codec == webrtc::kVideoCodecVP9) ||
        (metadata.codec == 3 && codec == webrtc::kVideoCodecH264) ||
        (metadata.codec == 4 && codec == webrtc::kVideoCodecAV1) ||
        (metadata.codec == 5 && codec == webrtc::kVideoCodecH265)))
    return false;
  std::optional<webrtc::CodecSpecificInfo> codec_specific;
  if (codec != webrtc::kVideoCodecGeneric) {
    codec_specific.emplace();
    codec_specific->codecType = codec;
    codec_specific->end_of_picture = metadata.end_of_picture;
    if (codec == webrtc::kVideoCodecH264) {
      codec_specific->codecSpecific.H264.packetization_mode =
          state->h264_packetization_mode;
      codec_specific->codecSpecific.H264.temporal_idx =
          metadata.temporal_index < 0 ? webrtc::kNoTemporalIdx : metadata.temporal_index;
      codec_specific->codecSpecific.H264.base_layer_sync = metadata.layer_sync;
      codec_specific->codecSpecific.H264.idr_frame = key_frame;
    } else if (codec == webrtc::kVideoCodecVP8) {
      auto &vp8 = codec_specific->codecSpecific.VP8;
      vp8.nonReference = metadata.non_reference;
      vp8.temporalIdx = metadata.temporal_index < 0
          ? webrtc::kNoTemporalIdx : metadata.temporal_index;
      vp8.layerSync = metadata.layer_sync;
      vp8.keyIdx = metadata.key_index;
    } else if (codec == webrtc::kVideoCodecVP9) {
      auto &vp9 = codec_specific->codecSpecific.VP9;
      vp9.first_frame_in_picture = metadata.first_frame_in_picture;
      vp9.inter_pic_predicted = metadata.inter_picture_predicted;
      vp9.flexible_mode = metadata.flexible_mode;
      vp9.temporal_idx = metadata.temporal_index < 0
          ? webrtc::kNoTemporalIdx : metadata.temporal_index;
      vp9.temporal_up_switch = metadata.temporal_up_switch;
      vp9.inter_layer_predicted = metadata.inter_layer_predicted;
      vp9.num_spatial_layers = metadata.num_spatial_layers;
      vp9.spatial_layer_resolution_present = key_frame && metadata.num_spatial_layers == 1;
      vp9.width[0] = width;
      vp9.height[0] = height;
    }
  }
  const auto result = state->Invoke([&](webrtc::EncodedImageCallback& target) {
    return target.OnEncodedImage(image, codec_specific ? &*codec_specific : nullptr);
  });
  return result && result->error == webrtc::EncodedImageCallback::Result::OK;
}
FfiEncodedCallbackResult encoded_callback_frame(const NativeEncodedImageCallback& callback, const NativeEncodedImage& image, const NativeCodecSpecificInfo* info) noexcept {
  const auto data = image.value().GetEncodedData();
  if (image.value().size() && (!data || !image.value().data() || image.value().size() > data->size())) return {false, 1, 0, false};
  const auto value = callback.state()->Invoke([&](webrtc::EncodedImageCallback& target) { return target.OnEncodedImage(image.value(), info ? &info->value() : nullptr); }, &image.value(), info ? &info->value() : nullptr);
  if (!value) return {false, 1, 0, false};
  return {true, static_cast<std::int32_t>(value->error), value->frame_id, value->drop_next_frame};
}
bool encoded_callback_dropped(const NativeEncodedImageCallback& callback, std::uint32_t timestamp, std::int32_t spatial_id, bool end) noexcept {
  return callback.state()->Invoke([&](webrtc::EncodedImageCallback& target) {
    target.OnFrameDropped(timestamp, spatial_id, end);
    return webrtc::EncodedImageCallback::Result(webrtc::EncodedImageCallback::Result::OK);
  }).has_value();
}
bool encoded_callback_retired(const NativeEncodedImageCallback& callback) noexcept {
  std::lock_guard lock(callback.state()->mutex); return !callback.state()->active;
}
bool encoded_callback_same(const NativeEncodedImageCallback& a, const NativeEncodedImageCallback& b) noexcept { return a.state() == b.state(); }
FfiDecodedCallbackResult decoded_callback_frame(const NativeDecodedImageCallback& callback, NativeRawVideoFrame& frame,
    std::uint8_t overload, std::int64_t time_ms, bool has_time, std::int32_t optional_time_ms, bool has_qp, std::uint8_t qp) noexcept {
  if (!callback.state()->AdmitTime(overload, time_ms, has_time, optional_time_ms)) return {false, WEBRTC_VIDEO_CODEC_ERR_PARAMETER};
  return callback.state()->Invoke([&](webrtc::DecodedImageCallback& target) -> std::int32_t {
    if (overload == 0) return target.Decoded(frame.value());
    if (overload == 1) return target.Decoded(frame.value(), time_ms);
    target.Decoded(frame.value(), has_time ? std::optional<std::int32_t>(optional_time_ms) : std::nullopt,
                   has_qp ? std::optional<std::uint8_t>(qp) : std::nullopt);
    return 0;  // The optional overload has no native return value.
  });
}
bool decoded_callback_retired(const NativeDecodedImageCallback& callback) noexcept {
  std::lock_guard lock(callback.state()->mutex); return !callback.state()->active;
}
bool decoded_callback_same(const NativeDecodedImageCallback& a, const NativeDecodedImageCallback& b) noexcept { return a.state() == b.state(); }
bool decoded_callback_emit(const NativeDecodedImageCallback &callback,
                           rust::Slice<const std::uint8_t> data,
                           std::uint32_t width, std::uint32_t height,
                           std::int64_t timestamp_us,
                           std::uint32_t rtp_timestamp,
                           std::uint16_t rotation) noexcept {
  if (decoded_callback_retired(callback) || !callback.state()->AdmitTime(0, 0, false, 0)) return false;
  const std::size_t y = static_cast<std::size_t>(width) * height;
  const std::uint32_t cw = (width + 1) / 2, ch = (height + 1) / 2;
  const std::size_t c = static_cast<std::size_t>(cw) * ch;
  if (width == 0 || height == 0 || data.size() != y + 2 * c ||
      (rotation != 0 && rotation != 90 && rotation != 180 && rotation != 270))
    return false;
  if (video_source_i420_allocation_size(width, height) == 0) return false;
  auto buffer =
      webrtc::I420Buffer::Copy(width, height, data.data(), width,
                               data.data() + y, cw, data.data() + y + c, cw);
  auto frame = webrtc::VideoFrame::Builder()
                   .set_video_frame_buffer(buffer)
                   .set_timestamp_us(timestamp_us)
                   .set_rtp_timestamp(rtp_timestamp)
                   .set_rotation(static_cast<webrtc::VideoRotation>(rotation))
                   .build();
  const auto result = callback.state()->Invoke([&](webrtc::DecodedImageCallback& target) { return target.Decoded(frame); });
  return result.invoked && result.status == 0;
}

FfiCodecTestResult
test_codec_roundtrip(const NativeVideoEncoderFactory &encoder_factory,
                     const NativeVideoDecoderFactory &decoder_factory,
                     std::uint32_t frames) noexcept {
  FfiCodecTestResult result{WEBRTC_VIDEO_CODEC_ERROR, 0, 0, 0};
  webrtc::Environment env = shared_default_event_log(webrtc::EnvironmentFactory().Create());
  webrtc::SdpVideoFormat format("H264", {{"profile-level-id", "42e01f"}});
  auto encoder = encoder_factory.factory().Create(env, format);
  auto decoder = decoder_factory.factory().Create(env, format);
  if (!encoder || !decoder)
    return result;
  webrtc::VideoCodec codec{};
  codec.codecType = webrtc::kVideoCodecH264;
  codec.width = 16;
  codec.height = 16;
  codec.startBitrate = 64;
  codec.minBitrate = 16;
  codec.maxBitrate = 256;
  codec.maxFramerate = 30;
  webrtc::VideoEncoder::Settings encoder_settings(
      webrtc::VideoEncoder::Capabilities(false), 1, 1200);
  webrtc::VideoDecoder::Settings decoder_settings;
  decoder_settings.set_codec_type(webrtc::kVideoCodecH264);
  decoder_settings.set_number_of_cores(1);
  decoder_settings.set_max_render_resolution({16, 16});
  EncodedCollector encoded;
  DecodedCollector decoded;
  if (encoder->InitEncode(&codec, encoder_settings) != 0 ||
      encoder->RegisterEncodeCompleteCallback(&encoded) != 0 ||
      !decoder->Configure(decoder_settings) ||
      decoder->RegisterDecodeCompleteCallback(&decoded) != 0)
    return result;
  webrtc::VideoBitrateAllocation allocation;
  allocation.SetBitrate(0, 0, 64000);
  encoder->SetRates({allocation, 30.0, webrtc::DataRate::BitsPerSec(64000)});
  for (std::uint32_t index = 0; index < frames; ++index) {
    auto buffer = webrtc::I420Buffer::Create(16, 16);
    std::memset(buffer->MutableDataY(), static_cast<int>(index + 1), 16 * 16);
    std::memset(buffer->MutableDataU(), 64, 8 * 8);
    std::memset(buffer->MutableDataV(), 192, 8 * 8);
    auto frame = webrtc::VideoFrame::Builder()
                     .set_video_frame_buffer(buffer)
                     .set_timestamp_us(index * 1000)
                     .set_rtp_timestamp(9000 + index)
                     .build();
    std::vector<webrtc::VideoFrameType> types{
        index == 0 ? webrtc::VideoFrameType::kVideoFrameKey
                   : webrtc::VideoFrameType::kVideoFrameDelta};
    if (encoder->Encode(frame, &types) != 0)
      break;
  }
  result.encoded_frames = encoded.images.size();
  for (const auto &image : encoded.images) {
    if (decoder->Decode(image, 0) != 0)
      break;
  }
  result.decoded_frames = decoded.count;
  result.checksum = decoded.checksum;
  const int encoder_release = encoder->Release();
  const int decoder_release = decoder->Release();
  result.status =
      (encoder_release == 0 && decoder_release == 0 &&
       result.encoded_frames == frames && result.decoded_frames == frames)
          ? 0
          : WEBRTC_VIDEO_CODEC_ERROR;
  return result;
}
bool test_encoder_factory_cross_thread(
    const NativeVideoEncoderFactory &factory) noexcept {
  std::unique_ptr<webrtc::VideoEncoder> encoder;
  std::thread worker([&] {
    auto formats = factory.factory().GetSupportedFormats();
    if (formats.empty())
      return;
    webrtc::Environment env = shared_default_event_log(webrtc::EnvironmentFactory().Create());
    encoder = factory.factory().Create(env, formats.front());
  });
  worker.join();
  const bool created = encoder != nullptr;
  encoder.reset();
  return created;
}

} // namespace pulsebeam::webrtc_sys
