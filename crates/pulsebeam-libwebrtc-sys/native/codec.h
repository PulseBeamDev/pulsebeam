#pragma once

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <optional>
#include <vector>
#include <utility>

#include "api/audio_codecs/audio_codec_pair_id.h"
#include "api/audio_codecs/audio_decoder_factory.h"
#include "api/audio_codecs/audio_encoder_factory.h"

#include "api/neteq/neteq.h"
#include "api/scoped_refptr.h"
#include "rust/cxx.h"

namespace webrtc {
class AudioDecoder;
class AudioDecoderFactory;
class AudioEncoderFactory;
class FrameTransformerInterface;
class VideoFrame;
class Environment;
struct SdpVideoFormat;
class VideoEncoder;
class VideoDecoderFactory;
class VideoEncoderFactory;
class NetEqFactory;
} // namespace webrtc

namespace pulsebeam::webrtc_sys {

// Lossless value conversions shared by owning products and provider adapters.
// Conversion alone does not validate an engine's invocation preconditions.
struct FfiNetEqConfig;
webrtc::NetEq::Config NetEqConfigFromFfi(const FfiNetEqConfig &value) noexcept;
FfiNetEqConfig NetEqConfigToFfi(const webrtc::NetEq::Config &value) noexcept;
FfiNetEqConfig neteq_config_round_trip(const FfiNetEqConfig &value) noexcept;
struct FfiNetEqNetworkStatistics;
webrtc::NetEqNetworkStatistics
NetEqNetworkStatisticsFromFfi(const FfiNetEqNetworkStatistics &value) noexcept;
FfiNetEqNetworkStatistics NetEqNetworkStatisticsToFfi(
    const webrtc::NetEqNetworkStatistics &value) noexcept;
FfiNetEqNetworkStatistics neteq_network_statistics_round_trip(
    const FfiNetEqNetworkStatistics &value) noexcept;
struct FfiNetEqLifetimeStatistics;
webrtc::NetEqLifetimeStatistics NetEqLifetimeStatisticsFromFfi(
    const FfiNetEqLifetimeStatistics &value) noexcept;
FfiNetEqLifetimeStatistics NetEqLifetimeStatisticsToFfi(
    const webrtc::NetEqLifetimeStatistics &value) noexcept;
FfiNetEqLifetimeStatistics neteq_lifetime_statistics_round_trip(
    const FfiNetEqLifetimeStatistics &value) noexcept;
struct FfiNetEqOperationsAndState;
webrtc::NetEqOperationsAndState NetEqOperationsAndStateFromFfi(
    const FfiNetEqOperationsAndState &value) noexcept;
FfiNetEqOperationsAndState NetEqOperationsAndStateToFfi(
    const webrtc::NetEqOperationsAndState &value) noexcept;
FfiNetEqOperationsAndState neteq_operations_and_state_round_trip(
    const FfiNetEqOperationsAndState &value) noexcept;
struct FfiNetEqDecoderFormat;
webrtc::NetEq::DecoderFormat
NetEqDecoderFormatFromFfi(const FfiNetEqDecoderFormat &value) noexcept;
FfiNetEqDecoderFormat
NetEqDecoderFormatToFfi(const webrtc::NetEq::DecoderFormat &value) noexcept;
FfiNetEqDecoderFormat
neteq_decoder_format_round_trip(const FfiNetEqDecoderFormat &value) noexcept;
FfiNetEqConfig neteq_config_default() noexcept;
rust::String neteq_config_description(const FfiNetEqConfig &value) noexcept;
// Stock-only preconditions, not restrictions on representable config values or
// caller NetEq products. Packet-count arithmetic assumes guarded stock
// payloads.
bool neteq_stock_config_safe(const FfiNetEqConfig &value) noexcept;
bool neteq_stock_payload_size_safe(std::size_t size) noexcept;
bool neteq_stock_buffer_packets_safe(std::size_t count) noexcept;
// Extract RED's nonempty inner payload types without unsigned underflow or
// out-of-buffer pointer arithmetic. Does not instantiate any decoder.
bool neteq_stock_red_payload_types(rust::Slice<const std::uint8_t> payload,
                                   rust::Vec<std::uint8_t> &types) noexcept;

struct FfiCodecFormat;
struct FfiSdpAudioFormat;
webrtc::SdpAudioFormat
AudioFormatFromFfi(const FfiSdpAudioFormat &value) noexcept;
FfiSdpAudioFormat
AudioFormatToFfi(const webrtc::SdpAudioFormat &value) noexcept;
struct FfiAudioCodecInfo;
struct FfiAudioCodecSpec;
struct FfiAudioDecoderCreation;
struct FfiAudioEncoderBitrateStep;
struct FfiAudioEncoderBitrateProbe;
struct FfiAudioEncoderSmoothingProbe;
struct FfiAudioEncoderQuery;
struct FfiAudioEncoderCreation;
struct FfiAudioEncoderInfo;
struct FfiAudioEncoderRange;
struct FfiAudioEncoderStatistics;
struct FfiAudioEncoderOutput;
struct FfiBitrateAllocationUpdate;
struct FfiParsedAudioFrames;
struct FfiDecodedAudio;
struct FfiAudioFrameDecodeInto;
struct FfiCallerParsedAudioFrame;
struct FfiAudioPacketInfo;
struct FfiAudioPlc;
class NativeEnvironment;
struct FfiEncodedVideoMetadata;
struct FfiCodecSupport;
struct FfiVideoFactorySupport;
class NativeSdpVideoFormat;
class NativeRtpResolution;
struct FfiCodecTestResult;
struct FfiDecoderStatistics;
struct CodecDecoderObservations;
struct RustVideoDecoderFactory;
struct RustVideoEncoderFactory;
struct RustVideoFactoryEnvironment;

class NativeVideoFrame final {
public:
  struct State;
  explicit NativeVideoFrame(std::unique_ptr<State> state) noexcept;
  ~NativeVideoFrame();
  const State &state() const noexcept;

private:
  std::unique_ptr<State> state_;
};

class NativeEncodedVideoFrame final {
public:
  struct State;
  explicit NativeEncodedVideoFrame(std::unique_ptr<State> state) noexcept;
  ~NativeEncodedVideoFrame();
  const State &state() const noexcept;

private:
  std::unique_ptr<State> state_;
};

class NativeEncodedImageCallback final {
public:
  struct State;
  explicit NativeEncodedImageCallback(std::shared_ptr<State> state) noexcept;
  ~NativeEncodedImageCallback();
  const std::shared_ptr<State> &state() const noexcept;

private:
  std::shared_ptr<State> state_;
};

class NativeDecodedImageCallback final {
public:
  struct State;
  explicit NativeDecodedImageCallback(std::shared_ptr<State> state) noexcept;
  ~NativeDecodedImageCallback();
  const std::shared_ptr<State> &state() const noexcept;

private:
  std::shared_ptr<State> state_;
};

class NativeRawVideoFrame;
struct FfiDecodedCallbackResult;
FfiDecodedCallbackResult decoded_callback_frame(const NativeDecodedImageCallback&, NativeRawVideoFrame&, std::uint8_t overload, std::int64_t time_ms, bool has_time, std::int32_t optional_time_ms, bool has_qp, std::uint8_t qp) noexcept;
bool encoded_callback_retired(const NativeEncodedImageCallback&) noexcept;
bool encoded_callback_same(const NativeEncodedImageCallback&, const NativeEncodedImageCallback&) noexcept;
bool decoded_callback_retired(const NativeDecodedImageCallback&) noexcept;
bool decoded_callback_same(const NativeDecodedImageCallback&, const NativeDecodedImageCallback&) noexcept;

class NativeVideoDecoderSettings;
class NativeVideoDecoderInfo;
class NativeVideoEncoderInfo;
class NativeSdpVideoFormat;
struct RustVideoFactoryEnvironment;
class NativeVideoDecoderProduct final {
public:
  struct State;
  explicit NativeVideoDecoderProduct(std::unique_ptr<State> state) noexcept;
  ~NativeVideoDecoderProduct();
  const State &state() const noexcept;
  State &state() noexcept;
private:
  std::unique_ptr<State> state_;
};
class NativeVideoEncoderProduct final {
public:
  struct State;
  explicit NativeVideoEncoderProduct(std::unique_ptr<State> state) noexcept;
  ~NativeVideoEncoderProduct();
  const State &state() const noexcept;
  State &state() noexcept;
private:
  std::unique_ptr<State> state_;
};
class NativeVideoEncoderFactory;
class NativeOwnedVideoEncoderSelector;
std::unique_ptr<NativeOwnedVideoEncoderSelector> video_encoder_factory_selector(const NativeVideoEncoderFactory&, bool&) noexcept;
class NativeVideoDecoderFactory;
class NativePeerConnectionFactory;
std::unique_ptr<NativeVideoEncoderProduct> video_encoder_create_actor_product(const NativePeerConnectionFactory&, const NativeVideoEncoderFactory&, const NativeEnvironment&, rust::Box<RustVideoFactoryEnvironment>, const NativeSdpVideoFormat&) noexcept;
std::unique_ptr<NativeVideoDecoderProduct> video_decoder_create_actor_product(const NativePeerConnectionFactory&, const NativeVideoDecoderFactory&, const NativeEnvironment&, rust::Box<RustVideoFactoryEnvironment>, const NativeSdpVideoFormat&, std::int32_t&) noexcept;
std::unique_ptr<NativeVideoEncoderProduct> video_encoder_create_product(
    const NativeVideoEncoderFactory &, const NativeEnvironment &,
    rust::Box<RustVideoFactoryEnvironment>, const NativeSdpVideoFormat &) noexcept;
class NativeVideoCodec;
class NativeEncodedImageSink;
class NativeEncodedImage;
class NativeFecOverride;
void video_encoder_product_fec(NativeVideoEncoderProduct&, std::shared_ptr<NativeFecOverride>) noexcept;
class NativeCodecSpecificInfo;
struct FfiEncodedCallbackResult;
std::int32_t video_encoder_product_register_callback(NativeVideoEncoderProduct&, const NativeEncodedImageSink*) noexcept;
FfiEncodedCallbackResult encoded_callback_frame(const NativeEncodedImageCallback&, const NativeEncodedImage&, const NativeCodecSpecificInfo*) noexcept;
bool encoded_callback_dropped(const NativeEncodedImageCallback&, std::uint32_t, std::int32_t, bool) noexcept;
class NativeVideoRateControl;
class NativeRawVideoFrame;
struct FfiOwnedEncoderSettings;
std::int32_t video_encoder_product_initialize(NativeVideoEncoderProduct&, const NativeVideoCodec&, FfiOwnedEncoderSettings, bool) noexcept;
std::int32_t video_encoder_product_encode(NativeVideoEncoderProduct&, const NativeRawVideoFrame&, bool, rust::Slice<const std::int32_t>) noexcept;
void video_encoder_product_rates(NativeVideoEncoderProduct&, const NativeVideoRateControl&) noexcept;
std::unique_ptr<NativeVideoEncoderInfo> video_encoder_product_info(const NativeVideoEncoderProduct &) noexcept;
rust::Vec<std::uint8_t> video_encoder_product_name(const NativeVideoEncoderProduct &, bool& valid) noexcept;
struct FfiVideoEncoderLossNotification;
void video_encoder_product_packet_loss(NativeVideoEncoderProduct&, float) noexcept;
void video_encoder_product_rtt(NativeVideoEncoderProduct&, std::int64_t) noexcept;
void video_encoder_product_loss_notification(NativeVideoEncoderProduct&, FfiVideoEncoderLossNotification) noexcept;
std::int32_t video_encoder_product_release(NativeVideoEncoderProduct &) noexcept;
class NativeVideoDecoderFactory;
class NativeDecodedImageSink;
class NativeEncodedImage;
std::int32_t video_decoder_product_register_callback(NativeVideoDecoderProduct&, const NativeDecodedImageSink*) noexcept;
std::int32_t video_decoder_product_decode(NativeVideoDecoderProduct&, const NativeEncodedImage&, bool missing_overload, bool missing, std::int64_t render_time_ms, bool& valid) noexcept;
std::unique_ptr<NativeVideoDecoderProduct> video_decoder_create_product(
    const NativeVideoDecoderFactory &, const NativeEnvironment &,
    rust::Box<RustVideoFactoryEnvironment>, const NativeSdpVideoFormat &,
    std::int32_t &status) noexcept;
std::unique_ptr<NativeVideoDecoderInfo> video_decoder_product_info(
    const NativeVideoDecoderProduct &) noexcept;
rust::Vec<std::uint8_t> video_decoder_product_implementation_name(
    const NativeVideoDecoderProduct &, bool &valid) noexcept;
bool video_decoder_product_configure(NativeVideoDecoderProduct &, const NativeVideoDecoderSettings &) noexcept;
std::int32_t video_decoder_product_release(NativeVideoDecoderProduct &) noexcept;

class NativeVideoEncoderFactory final {
public:
  explicit NativeVideoEncoderFactory(
      std::unique_ptr<webrtc::VideoEncoderFactory> factory,
      bool rust_factory = false, bool builtin_factory = false) noexcept;
  ~NativeVideoEncoderFactory();
  webrtc::VideoEncoderFactory &factory() const noexcept;
  bool rust_factory() const noexcept { return rust_factory_; }
  bool builtin_factory() const noexcept { return builtin_factory_; }
  std::unique_ptr<webrtc::VideoEncoderFactory> CreateAdapter(
      std::shared_ptr<rust::Box<RustVideoFactoryEnvironment>>, bool installed = true) const;

private:
  std::unique_ptr<webrtc::VideoEncoderFactory> factory_;
  bool rust_factory_;
  bool builtin_factory_;
};
// Preserve the actual SDK encoder and all virtual behavior. Only the closed
// builtin producer tag plus exact bound Environment can acquire runtime scope.
std::unique_ptr<webrtc::VideoEncoder> retain_builtin_encoder_execution(
    const NativeVideoEncoderFactory&, const NativeEnvironment&,
    const webrtc::Environment&, const webrtc::SdpVideoFormat&,
    std::unique_ptr<webrtc::VideoEncoder>) noexcept;

class NativeVideoDecoderFactory final {
public:
  explicit NativeVideoDecoderFactory(
      std::unique_ptr<webrtc::VideoDecoderFactory> factory,
      std::shared_ptr<CodecDecoderObservations> observations = nullptr,
      bool rust_factory = false) noexcept;
  ~NativeVideoDecoderFactory();
  webrtc::VideoDecoderFactory &factory() const noexcept;
  bool rust_factory() const noexcept { return rust_factory_; }
  std::unique_ptr<webrtc::VideoDecoderFactory> CreateAdapter(
      std::shared_ptr<rust::Box<RustVideoFactoryEnvironment>>, bool installed = true) const;
  const std::shared_ptr<CodecDecoderObservations> &
  observations() const noexcept;

private:
  std::unique_ptr<webrtc::VideoDecoderFactory> factory_;
  std::shared_ptr<CodecDecoderObservations> observations_;
  bool rust_factory_;
};

class NativeAudioEncoderFactory final {
public:
  struct State;
  explicit NativeAudioEncoderFactory(std::unique_ptr<State> state) noexcept;
  ~NativeAudioEncoderFactory();
  const State &state() const noexcept;
  webrtc::scoped_refptr<webrtc::AudioEncoderFactory> factory() const noexcept;
  webrtc::scoped_refptr<webrtc::FrameTransformerInterface>
  opus_transformer() const noexcept;

private:
  std::unique_ptr<State> state_;
};

class NativeAudioEncoder final {
public:
  struct State;
  explicit NativeAudioEncoder(std::unique_ptr<State> state) noexcept;
  ~NativeAudioEncoder();
  State &state() const noexcept;
  // Stored SDK envelope; state() resolves its exact original guarded backend.
  webrtc::AudioEncoder &sdk_encoder() const noexcept;
  bool forwarding() const noexcept;
  // Consumes only a privately constructed forwarding envelope, not its origin.
  std::unique_ptr<webrtc::AudioEncoder> take_sdk_encoder() noexcept;

private:
  std::unique_ptr<State> state_;
};

class NativeAudioEncoderFailureObserver final {
public:
  struct State;
  explicit NativeAudioEncoderFailureObserver(std::shared_ptr<State> state) : state_(std::move(state)) {}
  const std::shared_ptr<State> &state() const noexcept { return state_; }
private:
  std::shared_ptr<State> state_;
};
std::unique_ptr<NativeAudioEncoderFailureObserver> audio_encoder_sdk_failure_observer(const NativeAudioEncoder &) noexcept;
std::uint32_t audio_encoder_sdk_failure(const NativeAudioEncoderFailureObserver &) noexcept;
std::unique_ptr<NativeAudioEncoderFailureObserver> audio_encoder_sdk_failure_clone(const NativeAudioEncoderFailureObserver &) noexcept;
class NativeInstalledAudioEncoderFactory final {
public:
  struct State;
  explicit NativeInstalledAudioEncoderFactory(std::unique_ptr<State> state) noexcept;
  ~NativeInstalledAudioEncoderFactory();
  webrtc::scoped_refptr<webrtc::AudioEncoderFactory> factory() const noexcept;
  const std::unique_ptr<State>& state() const noexcept { return state_; }
private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeInstalledAudioEncoderFactory> audio_installed_encoder_factory(
    const NativeAudioEncoderFactory*, const NativeEnvironment&,
    std::shared_ptr<rust::Box<RustVideoFactoryEnvironment>>, bool production_system_clock) noexcept;
std::unique_ptr<NativeAudioEncoderFailureObserver> audio_installed_encoder_failure_observer(
    const NativeInstalledAudioEncoderFactory&) noexcept;

struct AudioEncoderTransfer;
std::unique_ptr<webrtc::AudioEncoder> audio_encoder_forward_to_sdk(rust::Box<AudioEncoderTransfer>) noexcept;
std::unique_ptr<NativeAudioEncoder> audio_encoder_forward_product(rust::Box<AudioEncoderTransfer>) noexcept;
struct RustAudioEncoderFactoryCall;
class NativeAudioEncoderOptions;
rust::Vec<FfiAudioCodecSpec> audio_caller_encoder_formats(rust::Box<RustAudioEncoderFactoryCall>) noexcept;
FfiAudioEncoderQuery audio_caller_encoder_query(rust::Box<RustAudioEncoderFactoryCall>, const FfiSdpAudioFormat &) noexcept;
std::unique_ptr<NativeAudioEncoder> audio_caller_encoder_create(rust::Box<RustAudioEncoderFactoryCall>,
    const NativeEnvironment &, const FfiSdpAudioFormat &, const NativeAudioEncoderOptions &) noexcept;
// Persistent standalone caller factories. State owns one refcounted SDK
// adapter and its Rust registration. No SDK factory accessor/peer selection is
// exposed: each synchronous operation uses a scoped, independently owned call.
// Rust owners enforce serialization and same-registration requests. Raw C++
// callers must likewise serialize and keep matching call borrows alive.
class NativeCallerAudioEncoderFactory final {
public:
  struct State;
  explicit NativeCallerAudioEncoderFactory(std::unique_ptr<State> state) noexcept;
  ~NativeCallerAudioEncoderFactory();
  const std::unique_ptr<State> &state() const noexcept { return state_; }
private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeCallerAudioEncoderFactory> audio_caller_encoder_factory_new(rust::Box<RustAudioEncoderFactoryCall>) noexcept;
rust::Vec<FfiAudioCodecSpec> audio_caller_encoder_factory_formats(const NativeCallerAudioEncoderFactory &, const RustAudioEncoderFactoryCall &) noexcept;
FfiAudioEncoderQuery audio_caller_encoder_factory_query(const NativeCallerAudioEncoderFactory &, const RustAudioEncoderFactoryCall &, const FfiSdpAudioFormat &) noexcept;
std::unique_ptr<NativeAudioEncoder> audio_caller_encoder_factory_create(const NativeCallerAudioEncoderFactory &, const RustAudioEncoderFactoryCall &,
    const NativeEnvironment &, const FfiSdpAudioFormat &, const NativeAudioEncoderOptions &) noexcept;
// Private regression witnesses that enter the actual SDK forwarding virtuals.
FfiAudioEncoderOutput audio_encoder_forward_encode_sdk(const NativeAudioEncoder &,
    std::uint32_t timestamp, rust::Slice<const std::int16_t> audio, rust::Slice<const std::uint8_t> prefix) noexcept;
void audio_encoder_forward_target_sdk(const NativeAudioEncoder &, std::int32_t bitrate) noexcept;
void audio_encoder_forward_loss_sdk(const NativeAudioEncoder &, float fraction) noexcept;
bool audio_encoder_forward_adaptor_sdk(const NativeAudioEncoder &, rust::Slice<const std::uint8_t> config) noexcept;

class NativeAudioDecoderFactory final {
public:
  struct State;
  explicit NativeAudioDecoderFactory(std::unique_ptr<State> state) noexcept;
  // Internal factory-Create context: retain the exact SDK-supplied factory.
  // No replacement factory, decoder construction, or added telemetry wrapper.
  explicit NativeAudioDecoderFactory(
      webrtc::scoped_refptr<webrtc::AudioDecoderFactory> factory) noexcept;
  ~NativeAudioDecoderFactory();
  const State &state() const noexcept;
  webrtc::scoped_refptr<webrtc::AudioDecoderFactory> factory() const noexcept;

private:
  std::unique_ptr<State> state_;
};

// Preserve null and the exact supplied SDK identity, never stock substitution.
// Optional selected provenance transfers only on exact pointer equality.
std::unique_ptr<NativeAudioDecoderFactory> wrap_audio_decoder_factory(
    webrtc::scoped_refptr<webrtc::AudioDecoderFactory> factory,
    const NativeAudioDecoderFactory *selected = nullptr) noexcept;
std::uint8_t audio_decoder_factory_builtin_kind(
    const NativeAudioDecoderFactory &factory) noexcept;
bool audio_decoder_factory_context_contract(
    const NativeAudioDecoderFactory &factory,
    const NativeAudioDecoderFactory &other,
    const NativeEnvironment &environment) noexcept;

class NativeAudioCodecPairId final {
public:
  NativeAudioCodecPairId() noexcept;
  explicit NativeAudioCodecPairId(webrtc::AudioCodecPairId value) noexcept;
  const webrtc::AudioCodecPairId &value() const noexcept;

private:
  webrtc::AudioCodecPairId value_;
};
std::unique_ptr<NativeAudioCodecPairId> audio_codec_pair_id_create() noexcept;
std::unique_ptr<NativeAudioCodecPairId> audio_codec_pair_id_copy(const NativeAudioCodecPairId &) noexcept;
std::uint64_t
audio_codec_pair_id_numeric(const NativeAudioCodecPairId &id) noexcept;

class NativeAudioEncoderOptions final {
public:
  NativeAudioEncoderOptions() = default;
  explicit NativeAudioEncoderOptions(webrtc::AudioEncoderFactory::Options value) : value_(std::move(value)) {}
  const webrtc::AudioEncoderFactory::Options &value() const noexcept { return value_; }
  webrtc::AudioEncoderFactory::Options &value() noexcept { return value_; }
private:
  webrtc::AudioEncoderFactory::Options value_;
};
std::unique_ptr<NativeAudioEncoderOptions> audio_encoder_options_default() noexcept;
std::unique_ptr<NativeAudioEncoderOptions> audio_encoder_options_copy(const NativeAudioEncoderOptions &) noexcept;
void audio_encoder_options_assign(NativeAudioEncoderOptions &, const NativeAudioEncoderOptions &) noexcept;
std::int32_t audio_encoder_options_payload_type(const NativeAudioEncoderOptions &) noexcept;
void audio_encoder_options_set_payload_type(NativeAudioEncoderOptions &, std::int32_t) noexcept;
std::unique_ptr<NativeAudioCodecPairId> audio_encoder_options_pair_id(const NativeAudioEncoderOptions &) noexcept;
void audio_encoder_options_set_pair_id(NativeAudioEncoderOptions &, const std::unique_ptr<NativeAudioCodecPairId> &) noexcept;

// Current owning decoder/frame resources, not the obsolete raw Decode/Plc API.
class NativeAudioDecoder final {
public:
  struct State;
  explicit NativeAudioDecoder(std::shared_ptr<State> state) noexcept;
  ~NativeAudioDecoder();
  const std::shared_ptr<State> &state() const noexcept;

private:
  std::shared_ptr<State> state_;
};
// Internal ownership handoff for a just-created decoder. NetEq's lazy factory
// must consume this exact instance, not make a throwaway construction probe.
// Refuses to detach a decoder shared with any outstanding parsed frame.
std::unique_ptr<webrtc::AudioDecoder>
audio_decoder_take_fresh(std::unique_ptr<NativeAudioDecoder> decoder) noexcept;
// Non-consuming handoff preflight, not permission to detach the backend or
// discard its environment/provenance. Native parsed-frame owners must be gone.
bool audio_decoder_factory_transfer_ready(const NativeAudioDecoder &decoder) noexcept;
std::size_t audio_decoder_forwarded_frame_count(const NativeAudioDecoder &decoder) noexcept;
struct AudioDecoderTransfer;
// Actual SDK-derived product, retaining the complete native wrapper and Rust
// environment lease. Not permission to install an arbitrary factory/consumer.
std::unique_ptr<webrtc::AudioDecoder> audio_decoder_forward_to_sdk(rust::Box<AudioDecoderTransfer> owner) noexcept;
std::unique_ptr<NativeAudioDecoder> audio_decoder_forward_product(rust::Box<AudioDecoderTransfer> owner) noexcept;
struct RustAudioDecoderFactoryCall;
rust::Vec<FfiAudioCodecSpec> audio_caller_decoder_formats(rust::Box<RustAudioDecoderFactoryCall> source) noexcept;
bool audio_caller_decoder_supported(rust::Box<RustAudioDecoderFactoryCall> source, const FfiSdpAudioFormat &format) noexcept;
std::unique_ptr<NativeAudioDecoder> audio_caller_decoder_create(rust::Box<RustAudioDecoderFactoryCall> source,
    const NativeEnvironment &environment, const FfiSdpAudioFormat &format,
    bool compatibility, const std::unique_ptr<NativeAudioCodecPairId> &pair) noexcept;

class NativeCallerAudioDecoderFactory final {
public:
  struct State;
  explicit NativeCallerAudioDecoderFactory(std::unique_ptr<State> state) noexcept;
  ~NativeCallerAudioDecoderFactory();
  const std::unique_ptr<State> &state() const noexcept { return state_; }
private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeCallerAudioDecoderFactory> audio_caller_decoder_factory_new(rust::Box<RustAudioDecoderFactoryCall>) noexcept;
rust::Vec<FfiAudioCodecSpec> audio_caller_decoder_factory_formats(const NativeCallerAudioDecoderFactory &, const RustAudioDecoderFactoryCall &) noexcept;
bool audio_caller_decoder_factory_supported(const NativeCallerAudioDecoderFactory &, const RustAudioDecoderFactoryCall &, const FfiSdpAudioFormat &) noexcept;
std::unique_ptr<NativeAudioDecoder> audio_caller_decoder_factory_create(const NativeCallerAudioDecoderFactory &, const RustAudioDecoderFactoryCall &,
    const NativeEnvironment &, const FfiSdpAudioFormat &, bool compatibility, const std::unique_ptr<NativeAudioCodecPairId> &) noexcept;

// Internal single-use handoff, not a public decoder provider. Request identity
// includes the exact format and Create overload/pair identity, never an RTP PT.
// The engine guard must prepare each lazy request before entering stock NetEq.
// A missing/mismatched request returns null and never probes the source
// factory.
enum class NetEqDecoderLifetimeState { Prepared, Consumed, Destroyed };
struct NetEqDecoderLifetime final {
  std::atomic<NetEqDecoderLifetimeState> state{
      NetEqDecoderLifetimeState::Prepared};
};
class PreparedNetEqDecoderFactory : public webrtc::AudioDecoderFactory {
public:
  struct State;
  explicit PreparedNetEqDecoderFactory(
      const NativeAudioDecoderFactory &factory);
  ~PreparedNetEqDecoderFactory() override;
  std::vector<webrtc::AudioCodecSpec> GetSupportedDecoders() override;
  bool IsSupportedDecoder(const webrtc::SdpAudioFormat &format) override;
  std::unique_ptr<webrtc::AudioDecoder>
  Create(const webrtc::Environment &env,
         const webrtc::SdpAudioFormat &format) override;
  std::unique_ptr<webrtc::AudioDecoder>
  Create(const webrtc::Environment &env, const webrtc::SdpAudioFormat &format,
         std::optional<webrtc::AudioCodecPairId> pair_id) override;
  // Takes the exact fresh owning decoder. Rejects shared/non-NetEq-compatible
  // decoders and duplicate pending identities, without replacement
  // construction.
  std::shared_ptr<NetEqDecoderLifetime>
  Prepare(const webrtc::SdpAudioFormat &format, bool compatibility,
          std::optional<webrtc::AudioCodecPairId> pair_id,
          std::unique_ptr<NativeAudioDecoder> decoder);
  void CancelPrepared();
  std::shared_ptr<NetEqDecoderLifetime>
  PrepareDecoder(const NativeEnvironment &environment,
                 const webrtc::SdpAudioFormat &format);

private:
  std::unique_ptr<webrtc::AudioDecoder>
  Consume(const webrtc::Environment &env, const webrtc::SdpAudioFormat &format,
          bool compatibility, std::optional<webrtc::AudioCodecPairId> pair_id);
  std::unique_ptr<State> state_;
};
bool neteq_decoder_handoff_contract(
    const NativeAudioDecoderFactory &factory,
    const NativeEnvironment &environment) noexcept;

// Uniquely owned, thread-compatible product. All calls must be serialized.
class NativeAudioFrame;
struct FfiMediaRtpHeader;
struct FfiRtpReceivedPacketInfo;
struct FfiNetEqCodec;
struct FfiNetEqAudioResult;
struct FfiNetEqNetworkStatisticsResult;
struct FfiNetEqDecoderFormatResult;
struct RustNetEqProvider;
class NativeNetEq final {
public:
  explicit NativeNetEq(std::unique_ptr<webrtc::NetEq> value, bool stock = false,
                       bool failed = false,
                       std::function<bool()> failure_query = {}) noexcept;
  ~NativeNetEq();
  webrtc::NetEq &value() noexcept;
  const webrtc::NetEq &value() const noexcept;
  bool is_stock() const noexcept;
  bool is_failed() const noexcept;
  std::unique_ptr<webrtc::NetEq> Release() noexcept;

private:
  std::unique_ptr<webrtc::NetEq> value_;
  const bool stock_;
  const bool failed_;
  // Destroyed before value_; its non-owning query never outlives that product.
  const std::function<bool()> failure_query_;
};
struct RustNetEqFactory;
struct FfiNetEqFactoryCreation;
// Standalone invocation uses the native Create inputs and ownership rules but
// does not apply SDK receive-channel output restrictions.
FfiNetEqFactoryCreation
neteq_factory_create(rust::Box<RustNetEqFactory> source,
                     const NativeEnvironment &environment,
                     const FfiNetEqConfig &config,
                     const NativeAudioDecoderFactory *decoder_factory) noexcept;
bool neteq_factory_failed_product_contract(
    rust::Box<RustNetEqFactory> source, const NativeEnvironment &environment,
    const NativeAudioDecoderFactory &decoder_factory) noexcept;
std::unique_ptr<NativeNetEq>
neteq_provider_create(const NativeEnvironment &environment,
                      rust::Box<RustNetEqProvider> provider) noexcept;
std::unique_ptr<NativeNetEq>
neteq_failed_create(std::int32_t configured_rate) noexcept;
std::unique_ptr<NativeNetEq>
neteq_failed_create_for_sdk(std::int32_t configured_rate) noexcept;
bool neteq_is_failed(const NativeNetEq &) noexcept;
bool neteq_is_stock(const NativeNetEq &) noexcept;
std::unique_ptr<NativeNetEq> neteq_stock_create(
    const NativeEnvironment &environment, const FfiNetEqConfig &config,
    const NativeAudioDecoderFactory *factory, std::int64_t minimum_time_us,
    std::int64_t maximum_time_us) noexcept;
std::int32_t
neteq_insert_packet(NativeNetEq &value, const FfiMediaRtpHeader &header,
                    rust::Slice<const std::uint8_t> payload) noexcept;
std::int32_t neteq_insert_packet_at(NativeNetEq &value,
                                    const FfiMediaRtpHeader &header,
                                    rust::Slice<const std::uint8_t> payload,
                                    std::int64_t receive_time_us) noexcept;
std::int32_t
neteq_insert_packet_info(NativeNetEq &value, const FfiMediaRtpHeader &header,
                         rust::Slice<const std::uint8_t> payload,
                         const FfiRtpReceivedPacketInfo &info) noexcept;
FfiNetEqAudioResult neteq_get_audio(NativeNetEq &value, NativeAudioFrame &frame,
                                    bool want_muted, bool muted, bool want_rate,
                                    std::int32_t rate, bool has_override,
                                    std::int32_t operation) noexcept;
bool neteq_set_codecs(NativeNetEq &value,
                      rust::Slice<const FfiNetEqCodec> codecs) noexcept;
bool neteq_register_payload_type(NativeNetEq &value, std::int32_t payload_type,
                                 const FfiSdpAudioFormat &format) noexcept;
bool neteq_create_decoder(NativeNetEq &value,
                          std::int32_t payload_type) noexcept;
std::int32_t neteq_remove_payload_type(NativeNetEq &value,
                                       std::uint8_t payload_type) noexcept;
bool neteq_remove_all_payload_types(NativeNetEq &value) noexcept;
bool neteq_set_maximum_buffer_packets(NativeNetEq &value,
                                      std::size_t count) noexcept;
void neteq_set_fast_accelerate(NativeNetEq &value, bool enable) noexcept;
bool neteq_set_minimum_delay(NativeNetEq &value,
                             std::int32_t delay_ms) noexcept;
bool neteq_set_maximum_delay(NativeNetEq &value,
                             std::int32_t delay_ms) noexcept;
bool neteq_set_base_minimum_delay(NativeNetEq &value,
                                  std::int32_t delay_ms) noexcept;
std::int32_t neteq_base_minimum_delay(const NativeNetEq &value) noexcept;
std::int32_t neteq_target_delay(const NativeNetEq &value) noexcept;
std::int32_t neteq_filtered_current_delay(const NativeNetEq &value) noexcept;
FfiNetEqNetworkStatisticsResult
neteq_network_statistics(NativeNetEq &value) noexcept;
FfiNetEqNetworkStatistics
neteq_current_network_statistics(const NativeNetEq &value) noexcept;
FfiNetEqLifetimeStatistics
neteq_lifetime_statistics(const NativeNetEq &value) noexcept;
FfiNetEqOperationsAndState
neteq_operations_and_state(const NativeNetEq &value) noexcept;
std::uint32_t neteq_playout_timestamp(const NativeNetEq &value,
                                      bool &present) noexcept;
std::int32_t neteq_last_output_sample_rate(const NativeNetEq &value) noexcept;
FfiNetEqDecoderFormatResult
neteq_current_decoder_format(NativeNetEq &value) noexcept;
FfiNetEqDecoderFormatResult
neteq_decoder_format(const NativeNetEq &value,
                     std::int32_t payload_type) noexcept;
void neteq_flush_buffers(NativeNetEq &value) noexcept;
std::int32_t neteq_sync_buffer_size(const NativeNetEq &value) noexcept;
void neteq_insert_empty_packet(NativeNetEq &value,
                               const FfiMediaRtpHeader &header) noexcept;
void neteq_enable_nack(NativeNetEq &value, std::size_t max_list_size) noexcept;
void neteq_disable_nack(NativeNetEq &value) noexcept;
rust::Vec<std::uint16_t>
neteq_nack_list(const NativeNetEq &value,
                std::int64_t round_trip_time_ms) noexcept;

class NativeDecodableAudioFrame final {
public:
  struct State;
  explicit NativeDecodableAudioFrame(std::unique_ptr<State> state) noexcept;
  ~NativeDecodableAudioFrame();
  const State &state() const noexcept;

private:
  std::unique_ptr<State> state_;
};
FfiAudioDecoderCreation
audio_decoder_create(const NativeAudioDecoderFactory &factory,
                     const NativeEnvironment &environment,
                     const FfiSdpAudioFormat &format) noexcept;
FfiAudioDecoderCreation audio_decoder_create_with_pair_id(
    const NativeAudioDecoderFactory &factory,
    const NativeEnvironment &environment, const FfiSdpAudioFormat &format,
    const std::unique_ptr<NativeAudioCodecPairId> &pair_id) noexcept;
std::int32_t
audio_decoder_sample_rate(const NativeAudioDecoder &decoder) noexcept;
std::size_t audio_decoder_channels(const NativeAudioDecoder &decoder) noexcept;
void audio_decoder_reset(const NativeAudioDecoder &decoder) noexcept;
std::int32_t
audio_decoder_error_code(const NativeAudioDecoder &decoder) noexcept;
FfiAudioPacketInfo
audio_decoder_packet_info(const NativeAudioDecoder &decoder,
                          rust::Slice<const std::uint8_t> encoded) noexcept;
FfiParsedAudioFrames
audio_decoder_parse(const NativeAudioDecoder &decoder,
                    rust::Slice<const std::uint8_t> encoded,
                    std::uint32_t timestamp) noexcept;
FfiAudioPlc
audio_decoder_generate_plc(const NativeAudioDecoder &decoder,
                           std::size_t requested_samples,
                           rust::Slice<const std::int16_t> prefix) noexcept;
std::size_t
audio_frame_duration(const NativeDecodableAudioFrame &frame) noexcept;
bool audio_frame_is_dtx(const NativeDecodableAudioFrame &frame) noexcept;
FfiDecodedAudio
audio_frame_decode(const NativeDecodableAudioFrame &frame) noexcept;
// Standalone actual SDK EncodedAudioFrame virtuals. The owned Rust call retains
// the caller frame/decoder lease and a fresh receipt; no native decoder or NetEq
// installation is implied. Decode uses the supplied span, never a stock bound.
struct RustCallerAudioFrameCall;
std::size_t audio_caller_frame_sdk_duration(rust::Box<RustCallerAudioFrameCall>) noexcept;
bool audio_caller_frame_sdk_dtx(rust::Box<RustCallerAudioFrameCall>) noexcept;
FfiAudioFrameDecodeInto audio_caller_frame_sdk_decode(rust::Box<RustCallerAudioFrameCall>, rust::Slice<std::int16_t> output) noexcept;
struct RustCallerAudioDecoderCall;
std::int32_t audio_caller_decoder_sdk_scalar(rust::Box<RustCallerAudioDecoderCall>, std::uint8_t kind) noexcept;
std::size_t audio_caller_decoder_sdk_channels(rust::Box<RustCallerAudioDecoderCall>) noexcept;
void audio_caller_decoder_sdk_reset(rust::Box<RustCallerAudioDecoderCall>) noexcept;
std::int32_t audio_caller_decoder_sdk_packet_duration(rust::Box<RustCallerAudioDecoderCall>, rust::Slice<const std::uint8_t>, bool redundant) noexcept;
bool audio_caller_decoder_sdk_packet_fec(rust::Box<RustCallerAudioDecoderCall>, rust::Slice<const std::uint8_t>) noexcept;
rust::Vec<FfiCallerParsedAudioFrame> audio_caller_decoder_sdk_parse(rust::Box<RustCallerAudioDecoderCall>, rust::Slice<const std::uint8_t>, std::uint32_t timestamp) noexcept;
FfiAudioPlc audio_caller_decoder_sdk_plc(rust::Box<RustCallerAudioDecoderCall>, std::size_t requested, rust::Slice<const std::int16_t> prefix) noexcept;
// Persistent standalone SDK decoder. Rust owner enforces same-root requests,
// serialized dispatch and synchronous borrows; raw C++ callers must do likewise.
// No native decoder accessor or factory/NetEq selection is exposed.
class NativeCallerAudioDecoderProduct final {
public:
  struct State;
  explicit NativeCallerAudioDecoderProduct(std::unique_ptr<State> state) noexcept;
  ~NativeCallerAudioDecoderProduct();
  const std::unique_ptr<State> &state() const noexcept { return state_; }
private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeCallerAudioDecoderProduct> audio_caller_decoder_product_new(rust::Box<RustCallerAudioDecoderCall>) noexcept;
std::int32_t audio_caller_decoder_product_scalar(const NativeCallerAudioDecoderProduct &, const RustCallerAudioDecoderCall &, std::uint8_t kind) noexcept;
std::size_t audio_caller_decoder_product_channels(const NativeCallerAudioDecoderProduct &, const RustCallerAudioDecoderCall &) noexcept;
void audio_caller_decoder_product_reset(const NativeCallerAudioDecoderProduct &, const RustCallerAudioDecoderCall &) noexcept;
std::int32_t audio_caller_decoder_product_duration(const NativeCallerAudioDecoderProduct &, const RustCallerAudioDecoderCall &, rust::Slice<const std::uint8_t>, bool redundant) noexcept;
bool audio_caller_decoder_product_fec(const NativeCallerAudioDecoderProduct &, const RustCallerAudioDecoderCall &, rust::Slice<const std::uint8_t>) noexcept;
rust::Vec<FfiCallerParsedAudioFrame> audio_caller_decoder_product_parse(const NativeCallerAudioDecoderProduct &, const RustCallerAudioDecoderCall &, rust::Slice<const std::uint8_t>, std::uint32_t timestamp) noexcept;
FfiAudioPlc audio_caller_decoder_product_plc(const NativeCallerAudioDecoderProduct &, const RustCallerAudioDecoderCall &, std::size_t requested, rust::Slice<const std::int16_t> prefix) noexcept;
// Private regression witness only; not a public Rust legacy Decode surface.
std::int32_t audio_caller_decoder_sdk_legacy_refusal(rust::Box<RustCallerAudioDecoderCall>, bool redundant, rust::Slice<std::int16_t>, std::int32_t &speech) noexcept;

std::unique_ptr<NativeVideoEncoderFactory>
new_video_encoder_factory(rust::Box<RustVideoEncoderFactory> factory) noexcept;
std::unique_ptr<NativeVideoDecoderFactory>
new_video_decoder_factory(rust::Box<RustVideoDecoderFactory> factory) noexcept;
std::unique_ptr<NativeAudioEncoderFactory>
new_builtin_audio_encoder_factory() noexcept;
std::unique_ptr<NativeAudioEncoderFactory>
new_opus_carrier_audio_encoder_factory() noexcept;
std::uint64_t audio_encoder_opus_handoff_failures(
    const NativeAudioEncoderFactory &factory) noexcept;
std::unique_ptr<NativeAudioDecoderFactory>
new_builtin_audio_decoder_factory() noexcept;
std::unique_ptr<NativeVideoEncoderFactory>
new_builtin_video_encoder_factory() noexcept;
std::unique_ptr<NativeVideoDecoderFactory>
new_builtin_video_decoder_factory() noexcept;
std::unique_ptr<NativeVideoDecoderFactory>
new_builtin_vp8_decoder_factory() noexcept;
std::unique_ptr<NativeAudioDecoderFactory>
new_builtin_opus_decoder_factory() noexcept;
FfiDecoderStatistics
video_decoder_statistics(const NativeVideoDecoderFactory &factory) noexcept;
FfiDecoderStatistics
audio_decoder_statistics(const NativeAudioDecoderFactory &factory) noexcept;
rust::Vec<FfiAudioCodecSpec>
audio_encoder_formats(const NativeAudioEncoderFactory &factory) noexcept;
FfiAudioEncoderQuery
audio_encoder_query(const NativeAudioEncoderFactory &factory,
                    const FfiSdpAudioFormat &format) noexcept;
FfiAudioEncoderCreation audio_encoder_create(
    const NativeAudioEncoderFactory &factory,
    const NativeEnvironment &environment, const FfiSdpAudioFormat &format,
    std::int32_t payload_type,
    const std::unique_ptr<NativeAudioCodecPairId> &pair_id) noexcept;
FfiAudioEncoderCreation audio_encoder_create_with_options(
    const NativeAudioEncoderFactory &factory, const NativeEnvironment &environment,
    const FfiSdpAudioFormat &format, const NativeAudioEncoderOptions &options) noexcept;
// Exact default allocation->bandwidth arguments; guard only compiled SDK
// constructor/getter assertions, retaining NDEBUG infinity/narrowing behavior.
void audio_encoder_default_allocation_arguments(std::int64_t target, std::int8_t kind,
    std::int64_t period, bool &valid, std::int32_t &bitrate, std::int64_t &milliseconds) noexcept;
// Internal smoothing-input predicate only; does not authorize ANA enablement.
bool audio_encoder_adaptor_smoothing_inputs_safe(std::int32_t bitrate,
                                                 bool has_period,
                                                 std::int64_t period) noexcept;
// Internal initialization witness under a caller-supplied lifetime millisecond
// span, not proof of native clock identity, bounds, or signed time arithmetic.
bool audio_encoder_adaptor_initialization_safe(
    std::int32_t duration, std::uint64_t clock_span_ms) noexcept;
// Internal isolated native-controller exercise, not adaptor enablement. Status:
// 0 success, 1 metrics, 2 occupied slot, 3 arithmetic, 4 invalid construction.
FfiAudioEncoderBitrateProbe audio_encoder_adaptor_bitrate_probe(
    std::int32_t initial_frame, std::int32_t increase, std::int32_t decrease,
    rust::Slice<const FfiAudioEncoderBitrateStep> steps) noexcept;
// Internal isolated native-filter exercise using retained clock evidence. No
// encoder/adaptor mutation and no clock reads. Status: 0 success, 1 clock,
// 2 floating context, 3 initialization, 4 sample, 5 time constant, 6 average.
FfiAudioEncoderSmoothingProbe audio_encoder_adaptor_smoothing_probe(
    const NativeAudioEncoder &encoder, std::int32_t first_sample,
    std::int32_t sample, std::int64_t first_time_us,
    std::int64_t sample_time_us, std::int64_t average_time_us,
    bool has_duration, std::int32_t duration) noexcept;
// Internal comparator-domain preflight for supplied hypothetical metrics.
// Does not track native reorder history or authorize ANA enablement.
std::uint8_t audio_encoder_check_network_adaptor_scoring(
    const NativeAudioEncoder &encoder, rust::Slice<const std::uint8_t> bytes,
    bool has_bandwidth, std::int32_t bandwidth, bool has_loss,
    float loss) noexcept;
// Internal smoother preflight using retained environment clock evidence;
// does not authorize ANA enablement or cover controller/scoring consumption.
std::uint8_t audio_encoder_check_network_adaptor_smoothing(
    const NativeAudioEncoder &encoder,
    rust::Slice<const std::uint8_t> bytes) noexcept;
// Internal universal-state controller arithmetic preflight only.
// Caller-supplied metrics/bounds are not native state evidence; clock, scoring
// and smoothing obligations remain separate. Status: 0 arithmetic safe, 1
// invalid construction, 2 unsafe arithmetic, 3 unavailable capability.
std::uint8_t audio_encoder_check_network_adaptor_arithmetic(
    const NativeAudioEncoder &encoder, rust::Slice<const std::uint8_t> bytes,
    bool has_overhead, std::size_t overhead, bool has_target,
    std::int32_t target, bool has_uplink, std::int32_t uplink_min,
    std::int32_t uplink_max) noexcept;
// Internal non-mutating construction preflight, not an ANA enable operation.
// 0: constructible, 1: invalid construction, 3: unavailable artifact/product.
std::uint8_t audio_encoder_check_network_adaptor_config(
    const NativeAudioEncoder &encoder,
    rust::Slice<const std::uint8_t> bytes) noexcept;
FfiAudioEncoderInfo
audio_encoder_info(const NativeAudioEncoder &encoder) noexcept;
FfiAudioEncoderRange
audio_encoder_frame_length_range(const NativeAudioEncoder &encoder) noexcept;
FfiAudioEncoderRange
audio_encoder_bitrate_range(const NativeAudioEncoder &encoder) noexcept;
FfiAudioEncoderStatistics
audio_encoder_statistics(const NativeAudioEncoder &encoder) noexcept;
FfiAudioEncoderOutput
audio_encoder_encode(const NativeAudioEncoder &encoder, std::uint32_t timestamp,
                     rust::Slice<const std::int16_t> audio) noexcept;
void audio_encoder_reset(const NativeAudioEncoder &encoder) noexcept;
bool audio_encoder_set_fec(const NativeAudioEncoder &encoder,
                           bool enable) noexcept;
bool audio_encoder_set_dtx(const NativeAudioEncoder &encoder,
                           bool enable) noexcept;
bool audio_encoder_set_application(const NativeAudioEncoder &encoder,
                                   bool speech) noexcept;
bool audio_encoder_enable_network_adaptor(const NativeAudioEncoder &encoder,
    rust::Slice<const std::uint8_t> config, std::uint8_t &error) noexcept;
void audio_encoder_disable_network_adaptor(
    const NativeAudioEncoder &encoder) noexcept;
void audio_encoder_set_max_playback_rate(const NativeAudioEncoder &encoder,
                                         std::int32_t frequency) noexcept;
bool audio_encoder_set_target_bitrate(const NativeAudioEncoder &encoder,
                                      std::int32_t bitrate) noexcept;
bool audio_encoder_on_target_bitrate(const NativeAudioEncoder &encoder,
                                     std::int32_t bitrate) noexcept;
bool audio_encoder_on_bandwidth(const NativeAudioEncoder &encoder,
                                std::int32_t bitrate, bool has_period,
                                std::int64_t period) noexcept;
bool audio_encoder_on_allocation(
    const NativeAudioEncoder &encoder,
    const FfiBitrateAllocationUpdate &update) noexcept;
bool audio_encoder_on_overhead(const NativeAudioEncoder &encoder,
                               std::size_t overhead) noexcept;
std::uint8_t audio_encoder_on_packet_loss(const NativeAudioEncoder &encoder,
                                          float fraction) noexcept;
bool audio_encoder_on_rtt(const NativeAudioEncoder &encoder,
                          std::int32_t rtt) noexcept;
bool audio_encoder_set_receiver_frame_length_range(
    const NativeAudioEncoder &encoder, std::int32_t minimum,
    std::int32_t maximum) noexcept;
rust::Vec<FfiAudioCodecSpec>
audio_decoder_formats(const NativeAudioDecoderFactory &factory) noexcept;
std::int8_t audio_decoder_supported(const NativeAudioDecoderFactory &factory,
                                    const FfiSdpAudioFormat &format) noexcept;
rust::Vec<std::uint8_t>
audio_format_description(const FfiSdpAudioFormat &format) noexcept;
rust::Vec<std::uint8_t>
audio_codec_info_description(const FfiAudioCodecInfo &info) noexcept;
bool audio_codec_info_has_fixed_bitrate(const FfiAudioCodecInfo &info, bool &valid) noexcept;
FfiAudioCodecInfo audio_codec_info_create(std::int32_t sample_rate_hz, std::size_t num_channels,
    std::int32_t default_bitrate_bps, std::int32_t min_bitrate_bps,
    std::int32_t max_bitrate_bps, bool use_range, bool &valid) noexcept;
rust::Vec<std::uint8_t>
audio_codec_spec_description(const FfiAudioCodecSpec &spec) noexcept;
std::uint64_t native_codec_thread_token() noexcept;

FfiVideoFactorySupport video_encoder_query_value(
    const NativeVideoEncoderFactory &, const NativeSdpVideoFormat &, bool,
    rust::Slice<const std::uint8_t>, const NativeRtpResolution *, bool) noexcept;
FfiVideoFactorySupport
video_decoder_query_value(const NativeVideoDecoderFactory &,
                          const NativeSdpVideoFormat &, bool,
                          const NativeRtpResolution *, bool) noexcept;
class NativeSdpVideoFormatList;
std::unique_ptr<NativeSdpVideoFormatList>
video_encoder_formats_value(const NativeVideoEncoderFactory &) noexcept;
std::unique_ptr<NativeSdpVideoFormatList>
video_encoder_implementations_value(const NativeVideoEncoderFactory &) noexcept;
std::unique_ptr<NativeSdpVideoFormatList>
video_decoder_formats_value(const NativeVideoDecoderFactory &) noexcept;
bool video_scalability_mode_valid(rust::Str name) noexcept;
rust::Vec<FfiCodecFormat>
video_encoder_formats(const NativeVideoEncoderFactory &factory) noexcept;
FfiCodecSupport video_encoder_query(const NativeVideoEncoderFactory &factory,
                                    const FfiCodecFormat &format,
                                    bool has_scalability_mode,
                                    rust::Str scalability_mode,
                                    bool has_resolution, std::uint32_t width,
                                    std::uint32_t height) noexcept;
rust::Vec<FfiCodecFormat>
video_decoder_formats(const NativeVideoDecoderFactory &factory) noexcept;
FfiCodecSupport video_decoder_query(const NativeVideoDecoderFactory &factory,
                                    const FfiCodecFormat &format,
                                    bool reference_scaling, bool has_resolution,
                                    std::uint32_t width,
                                    std::uint32_t height) noexcept;

std::uint32_t native_video_frame_width(const NativeVideoFrame &frame) noexcept;
std::uint32_t native_video_frame_height(const NativeVideoFrame &frame) noexcept;
std::int64_t
native_video_frame_timestamp_us(const NativeVideoFrame &frame) noexcept;
std::uint32_t
native_video_frame_rtp_timestamp(const NativeVideoFrame &frame) noexcept;
std::uint16_t
native_video_frame_rotation(const NativeVideoFrame &frame) noexcept;
rust::Vec<std::uint8_t>
native_video_frame_i420(const NativeVideoFrame &frame) noexcept;
std::uint32_t
native_encoded_frame_width(const NativeEncodedVideoFrame &frame) noexcept;
std::uint32_t
native_encoded_frame_height(const NativeEncodedVideoFrame &frame) noexcept;
std::uint32_t native_encoded_frame_rtp_timestamp(
    const NativeEncodedVideoFrame &frame) noexcept;
bool native_encoded_frame_key(const NativeEncodedVideoFrame &frame) noexcept;
std::int32_t
native_encoded_frame_qp(const NativeEncodedVideoFrame &frame) noexcept;
rust::Vec<std::uint8_t>
native_encoded_frame_data(const NativeEncodedVideoFrame &frame) noexcept;

bool encoded_callback_emit(const NativeEncodedImageCallback &callback,
                           rust::Slice<const std::uint8_t> data,
                           std::uint32_t width, std::uint32_t height,
                           std::uint32_t rtp_timestamp, bool key_frame,
                           std::int32_t qp,
                           const FfiEncodedVideoMetadata &metadata) noexcept;
bool decoded_callback_emit(const NativeDecodedImageCallback &callback,
                           rust::Slice<const std::uint8_t> data,
                           std::uint32_t width, std::uint32_t height,
                           std::int64_t timestamp_us,
                           std::uint32_t rtp_timestamp,
                           std::uint16_t rotation) noexcept;

FfiCodecTestResult
test_codec_roundtrip(const NativeVideoEncoderFactory &encoder,
                     const NativeVideoDecoderFactory &decoder,
                     std::uint32_t frames) noexcept;
bool test_encoder_factory_cross_thread(
    const NativeVideoEncoderFactory &factory) noexcept;
std::unique_ptr<NativeVideoFrame>
wrap_video_frame(const webrtc::VideoFrame &frame) noexcept;

} // namespace pulsebeam::webrtc_sys
