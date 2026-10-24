#pragma once

#include <cstdint>
#include <memory>
#include <string>

#include "api/scoped_refptr.h"
#include "rust/cxx.h"
#include "pulsebeam-webrtc-sys/native/audio_processor.h"

namespace webrtc {
class AudioDeviceModule;
class AudioProcessing;
class FrameTransformerInterface;
class PeerConnectionFactoryInterface;
class PeerConnectionInterface;
class RTCCertificate;
class Thread;
} // namespace webrtc

namespace pulsebeam::webrtc_sys {

struct RustMetronome;
struct RustVideoFactoryEnvironment;
struct RustCertificateGenerator;
struct RustAsyncDnsFactory;
struct RustVideoBitrateAllocatorFactory;
struct RustNetworkMonitorFactory;
struct RustNetworkManagerFactory;
struct RustPacketSocketFactory;
struct RustNetworkStatePredictorFactory;
struct RustFecControllerFactory;
struct RustNetworkControllerFactory;
struct RustFieldTrials;
struct RustTlsCertificateVerifier;
struct RustLocalNetworkPermissionFactory;
struct FfiCertificateKeyParams;
struct FfiCertificatePem;
struct FfiCertificateData;
struct FfiCertificateFingerprint;
struct FfiIceServer;
struct FfiPeerConfiguration;
class NativePeerConnection;
class NativeRtpSender;
class NativeRtpReceiver;
struct FfiFactoryOptions;
struct FfiCryptoOptions;
struct FfiOfferAnswerOptions;
struct FfiAudioDevice;
struct FfiNativeAudioDeviceCreation;
struct FfiAudioProcessingConfig;
struct FfiAudioProcessingState;
class NativeAudioEncoderFailureObserver;
struct FfiDescriptionSnapshot;
struct FfiPeerIceSnapshot;
struct FfiPeerEvent;
struct FfiPeerEventObservation;
class NativeAudioMixer;
class NativeAudioDeviceObserver;
class NativeAudioDevice;
class NativeAudioDecoderFactory;
class NativeAudioEncoderFactory;
class NativeDataChannel;
class NativeEnvironment;
class NativeEventLogOutput;
class NativeRtcEventLogFactory;
struct RustEventLogRetention;
class NativeNetworkManagerProvider;
class NativePacketSocketFactoryProvider;
class NativeThread;
class NativeVideoDecoderFactory;
class NativeVideoEncoderFactory;

class NativeReadiness;
class ReadinessSignal;

// An immutable, independently retained certificate resource. No mutable
// identity or SSL backend pointers are exposed by the Rust bridge.
class NativeCertificate final {
public:
  explicit NativeCertificate(
      webrtc::scoped_refptr<webrtc::RTCCertificate> certificate) noexcept;
  ~NativeCertificate();
  const webrtc::scoped_refptr<webrtc::RTCCertificate> &
  certificate() const noexcept;

private:
  const webrtc::scoped_refptr<webrtc::RTCCertificate> certificate_;
};

class NativePeerConfiguration final {
public:
  struct State;
  explicit NativePeerConfiguration(std::unique_ptr<State> state) noexcept;
  ~NativePeerConfiguration();
  const std::unique_ptr<State> &state() const noexcept;

private:
  std::unique_ptr<State> state_;
};
FfiPeerConfiguration peer_configuration_default(bool aggressive) noexcept;
std::unique_ptr<NativePeerConfiguration>
peer_configuration_new(const FfiPeerConfiguration &values) noexcept;
void peer_configuration_add_certificate(
    NativePeerConfiguration &native,
    const NativeCertificate &certificate) noexcept;
FfiPeerConfiguration
peer_configuration_values(const NativePeerConfiguration &native) noexcept;
std::size_t peer_configuration_certificate_count(
    const NativePeerConfiguration &native) noexcept;
std::shared_ptr<NativeCertificate>
peer_configuration_certificate(const NativePeerConfiguration &native,
                               std::size_t index) noexcept;
struct FfiCallAdmissionStats;
FfiCallAdmissionStats peer_call_admission_stats(const NativePeerConnection& peer) noexcept;
std::unique_ptr<NativePeerConfiguration>
peer_get_configuration(const NativePeerConnection &peer) noexcept;
std::uint8_t peer_set_configuration(const NativePeerConnection &peer,
                                    const NativePeerConfiguration &native,
                                    rust::String &error) noexcept;

class NativePeerConnectionFactory final {
public:
  struct State;

  explicit NativePeerConnectionFactory(std::unique_ptr<State> state) noexcept;
  ~NativePeerConnectionFactory();
  NativePeerConnectionFactory(const NativePeerConnectionFactory &) = delete;
  NativePeerConnectionFactory &
  operator=(const NativePeerConnectionFactory &) = delete;

  const std::unique_ptr<State> &state() const noexcept;
  webrtc::scoped_refptr<webrtc::PeerConnectionFactoryInterface>
  factory() const noexcept;
  webrtc::Thread *signaling_thread() const noexcept;
  webrtc::Thread *worker_thread() const noexcept;
  webrtc::scoped_refptr<webrtc::AudioProcessing>
  audio_processing() const noexcept;
  FfiAudioProcessingConfig audio_processing_config() const noexcept;
  std::shared_ptr<NativeInstalledAudioProcessor::State> installed_audio_processing() const noexcept;
  webrtc::scoped_refptr<webrtc::AudioDeviceModule>
  audio_device() const noexcept;
  std::shared_ptr<ReadinessSignal> readiness() const noexcept;

private:
  std::unique_ptr<State> state_;
};

bool factory_requires_deferred_teardown(const NativePeerConnectionFactory&) noexcept;

class NativeFrameTransformerHosts;
class NativePeerConnection final {
public:
  struct State;

  explicit NativePeerConnection(std::unique_ptr<State> state) noexcept;
  ~NativePeerConnection();
  NativePeerConnection(const NativePeerConnection &) = delete;
  NativePeerConnection &operator=(const NativePeerConnection &) = delete;

  const std::unique_ptr<State> &state() const noexcept;
  webrtc::scoped_refptr<webrtc::PeerConnectionInterface> peer() const noexcept;
  webrtc::Thread *network_thread() const noexcept;
  webrtc::Thread *signaling_thread() const noexcept;
  webrtc::Thread *worker_thread() const noexcept;
  webrtc::scoped_refptr<webrtc::FrameTransformerInterface>
  opus_transformer() const noexcept;
  NativeFrameTransformerHosts& frame_transformer_hosts() const noexcept;
  std::shared_ptr<ReadinessSignal> readiness() const noexcept;
  bool reserve_encoded_receiver(const std::string &id) const noexcept;
  bool reserve_audio_receiver(const std::string &id) const noexcept;
  void release_audio_receiver(const std::string &id) const noexcept;

private:
  std::unique_ptr<State> state_;
};

std::unique_ptr<NativePeerConnectionFactory> new_peer_connection_factory(
    const NativeEnvironment &environment, const NativeThread &network_thread,
    const NativeThread &worker_thread, const NativeThread &signaling_thread,
    const NativeNetworkManagerProvider *network_manager,
    rust::Box<RustNetworkManagerFactory> network_manager_factory,
    rust::Box<RustNetworkMonitorFactory> network_monitor_factory,
    rust::Box<RustNetworkStatePredictorFactory> network_predictor_factory,
    rust::Box<RustFecControllerFactory> fec_factory,
    rust::Box<RustNetworkControllerFactory> network_controller_factory,
    const NativePacketSocketFactoryProvider *packet_socket_factory,
    rust::Box<RustPacketSocketFactory> socket_factory,
    const NativeAudioEncoderFactory *audio_encoder,
    const NativeAudioDecoderFactory *audio_decoder,
    const NativeVideoEncoderFactory *video_encoder,
    const NativeVideoDecoderFactory *video_decoder,
    rust::Box<RustVideoFactoryEnvironment> video_creation_environment,
    bool production_system_clock, bool native_audio, const FfiNativeAudioDeviceCreation& native_audio_creation, std::unique_ptr<NativeAudioDevice> audio_device_module, bool event_logging,
    rust::Box<RustEventLogRetention> event_log_retention,
    const NativeRtcEventLogFactory *event_log_factory,
    rust::Box<RustMetronome> encode_metronome,
    rust::Box<RustMetronome> decode_metronome, const NativeReadiness *readiness,
    std::int32_t codec_core_budget, const FfiAudioProcessingConfig &processing, std::unique_ptr<NativeAudioMixer> audio_mixer, std::unique_ptr<NativeInstalledAudioProcessor> audio_processor, rust::String &error) noexcept;
std::unique_ptr<NativeAudioEncoderFailureObserver> factory_audio_encoder_failure_observer(
    const NativePeerConnectionFactory&) noexcept;
FfiCertificateKeyParams certificate_default_key_params(bool rsa) noexcept;
bool certificate_key_params_valid(
    const FfiCertificateKeyParams &params) noexcept;
std::shared_ptr<NativeCertificate>
certificate_generate(const FfiCertificateKeyParams &params, bool has_lifetime,
                     std::uint64_t lifetime_ms) noexcept;
std::shared_ptr<NativeCertificate> certificate_from_pem(rust::Str private_key,
                                                        rust::Str certificate,
                                                        bool chain) noexcept;
FfiCertificatePem
certificate_to_pem(const NativeCertificate &certificate) noexcept;
std::uint64_t
certificate_expires(const NativeCertificate &certificate) noexcept;
bool certificate_has_expired(const NativeCertificate &certificate,
                             std::uint64_t now) noexcept;
rust::Vec<FfiCertificateData>
certificate_chain(const NativeCertificate &certificate) noexcept;
FfiCertificateFingerprint
certificate_fingerprint(const NativeCertificate &certificate,
                        rust::Str algorithm) noexcept;
bool certificate_equal(const NativeCertificate &a,
                       const NativeCertificate &b) noexcept;
bool certificate_same_resource(const NativeCertificate &a,
                               const NativeCertificate &b) noexcept;
FfiCryptoOptions crypto_default_options() noexcept;
bool crypto_supported_srtp_suites(const FfiCryptoOptions &options,
                                  rust::Vec<std::uint16_t> &suites) noexcept;
rust::Vec<std::uint16_t> crypto_supported_key_exchange_groups() noexcept;
bool crypto_key_exchange_group_name(std::uint16_t group,
                                    rust::String &name) noexcept;
FfiCryptoOptions crypto_update_key_exchange_groups(
    const NativeEnvironment &environment, const FfiCryptoOptions &options,
    rust::Slice<const std::uint16_t> disabled) noexcept;
void factory_set_options(const NativePeerConnectionFactory &factory,
                         const FfiFactoryOptions &options) noexcept;
FfiApmConfig factory_audio_processor_config(const NativePeerConnectionFactory&, std::uint8_t&) noexcept;
std::uint8_t factory_audio_processor_apply(const NativePeerConnectionFactory&, const FfiApmConfig&) noexcept;
std::int32_t factory_audio_processor_reverse(const NativePeerConnectionFactory&, std::int32_t, std::size_t, rust::Slice<std::int16_t>, std::uint8_t&) noexcept;
std::int32_t factory_audio_processor_delay(const NativePeerConnectionFactory&, std::int32_t, std::uint8_t&) noexcept;
FfiAudioProcessingState factory_audio_processing_state(
    const NativePeerConnectionFactory &factory) noexcept;
std::int32_t factory_register_audio_device_observer(const NativePeerConnectionFactory&, std::unique_ptr<NativeAudioDeviceObserver>) noexcept;
bool factory_audio_devices(const NativePeerConnectionFactory &factory,
                           bool recording, rust::Vec<FfiAudioDevice> &devices,
                           rust::String &error) noexcept;
bool factory_select_audio_device(const NativePeerConnectionFactory &factory,
                                 bool recording, std::uint16_t index,
                                 rust::String &error) noexcept;

std::unique_ptr<NativePeerConnection> create_peer_connection(
    const NativePeerConnectionFactory &factory,
    const NativePeerConfiguration &configuration,
    rust::Box<RustFieldTrials> field_trials,
    rust::Box<RustTlsCertificateVerifier> tls_verifier,
    rust::Box<RustLocalNetworkPermissionFactory> local_permission,
    rust::Box<RustCertificateGenerator> certificate_generator,
    rust::Box<RustAsyncDnsFactory> async_dns_factory,
    rust::Box<RustVideoBitrateAllocatorFactory> video_bitrate_factory,
    std::uint8_t &error_kind, rust::String &error) noexcept;

std::uint8_t peer_create_offer(const NativePeerConnection &peer,
                               std::uint64_t operation_id,
                               const FfiOfferAnswerOptions &options) noexcept;
std::uint8_t peer_create_answer(const NativePeerConnection &peer,
                                std::uint64_t operation_id,
                                const FfiOfferAnswerOptions &options) noexcept;
std::uint8_t peer_start_event_log(const NativePeerConnection &peer,
                                  std::unique_ptr<NativeEventLogOutput> output,
                                  bool has_period,
                                  std::int64_t period_ms) noexcept;
std::uint8_t peer_stop_event_log(const NativePeerConnection &peer) noexcept;
bool peer_restart_ice(const NativePeerConnection &peer) noexcept;
bool peer_should_fire_negotiation_needed(const NativePeerConnection &peer,
                                         std::uint32_t event_id) noexcept;
std::uint8_t peer_set_local_description_auto(const NativePeerConnection &peer,
                                             std::uint64_t operation_id,
                                             bool legacy) noexcept;
std::uint8_t peer_modify_ice_candidate(const NativePeerConnection &peer,
                                       bool remove, rust::Slice<const std::uint8_t> sdp_mid,
                                       std::int32_t sdp_mline_index,
                                       rust::Slice<const std::uint8_t> candidate,
                                       rust::Vec<std::uint8_t> &message) noexcept;
std::uint8_t peer_request_stats(const NativePeerConnection &peer,
                                std::uint64_t operation_id) noexcept;
bool peer_set_bitrate(const NativePeerConnection &peer, std::int32_t minimum,
                      std::int32_t start, std::int32_t maximum,
                      std::uint8_t &error_type, rust::String &message) noexcept;
std::uint8_t peer_native_state(const NativePeerConnection& peer,
                               std::uint8_t field) noexcept;
FfiPeerIceSnapshot peer_ice_snapshot(const NativePeerConnection &peer) noexcept;
std::uint8_t
peer_descriptions(const NativePeerConnection &peer,
                  rust::Vec<FfiDescriptionSnapshot> &descriptions) noexcept;
std::uint8_t peer_set_local_description(const NativePeerConnection &peer,
                                        std::uint64_t operation_id,
                                        std::uint8_t sdp_type,
                                        rust::Slice<const std::uint8_t> sdp, bool legacy) noexcept;
std::uint8_t peer_set_remote_description(const NativePeerConnection &peer,
                                         std::uint64_t operation_id,
                                         std::uint8_t sdp_type,
                                         rust::Slice<const std::uint8_t> sdp, bool legacy) noexcept;
std::uint8_t peer_add_ice_candidate(const NativePeerConnection &peer,
                                    std::uint64_t operation_id,
                                    rust::Slice<const std::uint8_t> sdp_mid,
                                    std::int32_t sdp_mline_index,
                                    rust::Slice<const std::uint8_t> candidate) noexcept;
std::uint8_t peer_request_sender_stats(const NativePeerConnection&, const NativeRtpSender&, std::uint64_t) noexcept;
std::uint8_t peer_request_receiver_stats(const NativePeerConnection&, const NativeRtpReceiver&, std::uint64_t) noexcept;
std::uint8_t peer_request_stats_with_empty_selector(const NativePeerConnection&, bool, std::uint64_t) noexcept;
bool peer_reconfigure_bandwidth_estimation(const NativePeerConnection&, bool allow_probe) noexcept;
std::uint8_t peer_observe_channel_messages(const NativePeerConnection&, bool enabled) noexcept;
class NativeMediaTrack;
class NativeMediaStream;
class NativeMediaStreams;
struct FfiTrackArrival;
std::unique_ptr<NativeMediaStream> peer_take_stream_event(const NativePeerConnection&, std::uint64_t) noexcept;
FfiTrackArrival peer_take_track_event(const NativePeerConnection&, std::uint64_t) noexcept;
std::unique_ptr<NativeMediaStream> peer_factory_create_stream(const NativePeerConnectionFactory&, rust::Slice<const std::uint8_t>) noexcept;
std::uint8_t peer_mutate_stream(const NativePeerConnection&, const NativeMediaStream&, bool remove) noexcept;
std::unique_ptr<NativeMediaStreams> peer_streams(const NativePeerConnection&, bool remote) noexcept;
std::uint8_t peer_request_legacy_stats(const NativePeerConnection&, std::uint8_t, std::uint64_t) noexcept;
std::uint8_t peer_request_legacy_stats_with_track(const NativePeerConnection&, const NativeMediaTrack&, std::uint8_t, std::uint64_t) noexcept;
void peer_clear_stats_cache(const NativePeerConnection&) noexcept;
FfiPeerEvent peer_take_event(const NativePeerConnection &peer) noexcept;
FfiPeerEventObservation
peer_event_observation(const NativePeerConnection &peer) noexcept;
// Binding-only callback-retention unit hook, not transport evidence.
bool test_peer_candidate_observation(const NativePeerConnection &peer, rust::Slice<const std::uint8_t> mid, std::int32_t index, rust::Slice<const std::uint8_t> sdp, bool removed) noexcept;
bool test_peer_ice_observations(const NativePeerConnection &peer, std::int32_t legacy, std::int32_t standardized, std::uint32_t repetitions) noexcept;
bool test_peer_control_observations(const NativePeerConnection &peer,
                                    std::uint32_t message_bytes,
                                    bool arrivals) noexcept;
std::unique_ptr<NativeDataChannel>
peer_take_data_channel(const NativePeerConnection &peer,
                       std::uint64_t arrival_id) noexcept;
bool factory_in_native_execution_context(const NativePeerConnectionFactory &factory) noexcept;
bool factory_control_call_allowed(const NativePeerConnectionFactory &factory) noexcept;
bool peer_control_call_allowed(const NativePeerConnection &peer) noexcept;
bool close_peer_connection(const NativePeerConnection &peer) noexcept;
// Route audio lifecycle through WebRTC AudioState, not directly through ADM.
bool peer_set_native_audio_enabled(const NativePeerConnection &peer,
                                   bool recording, bool enabled,
                                   rust::String &error) noexcept;
// Pull one 10 ms receive block from the headless audio transport. This does
// not access any operating-system audio device.
bool pump_headless_audio(const NativePeerConnection &peer) noexcept;

} // namespace pulsebeam::webrtc_sys
