#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <vector>

#include "api/scoped_refptr.h"
#include "rust/cxx.h"
#include "pulsebeam-webrtc-sys/native/desktop_frame.h"

namespace webrtc {
class MediaStreamTrackInterface;
class RtpSenderInterface;
class RtpReceiverInterface;
class RtpTransceiverInterface;
}  // namespace webrtc

namespace pulsebeam::webrtc_sys {

struct RustFrameEncryptorProvider;
struct RustFrameDecryptorProvider;
struct FfiFrameCryptoOutput;
struct FfiFrameCryptoFailure;
std::size_t frame_crypto_max_output_capacity() noexcept;
class NativeFrameEncryptor final {
 public:
  struct State;
  explicit NativeFrameEncryptor(std::unique_ptr<State> state) noexcept;
  ~NativeFrameEncryptor();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeFrameDecryptor final {
 public:
  struct State;
  explicit NativeFrameDecryptor(std::unique_ptr<State> state) noexcept;
  ~NativeFrameDecryptor();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativePeerConnectionFactory;
class NativePeerConnection;
class NativeRtpCapabilities;
class NativeRtcError;
class NativeVideoFrame;
class NativeRawVideoFrame;
struct FfiSenderParameters;
struct FfiCodecFormat;
struct FfiVideoSourceStats;
struct FfiVideoSinkEvent;
struct FfiVideoSinkWants;
struct FfiRecordableVideoFrame;
struct FfiVideoCodecCapability;
struct FfiEncodedVideoFrame;
struct FfiRtpHeaderExtension;

class NativeAudioTrack;
class NativeVideoTrack;
class NativeVideoSourceView final {
 public:
  struct State;
  explicit NativeVideoSourceView(std::unique_ptr<State> state) noexcept;
  ~NativeVideoSourceView();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeEnvironment;
class NativeVideoSource;
class NativeScreenCapture;
class NativeDesktopOptions final {
 public:
  struct State;
  explicit NativeDesktopOptions(std::unique_ptr<State> state) noexcept;
  ~NativeDesktopOptions();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeVideoSourceEncodedSink final {
 public:
  struct State;
  explicit NativeVideoSourceEncodedSink(std::unique_ptr<State> state) noexcept;
  ~NativeVideoSourceEncodedSink();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeVideoSourceEncodedSink> video_source_attach_encoded_output(const NativeVideoSourceView&) noexcept;
FfiRecordableVideoFrame video_source_encoded_take_frame(const NativeVideoSourceEncodedSink&) noexcept;
FfiVideoSinkEvent video_source_encoded_take_event(const NativeVideoSourceEncodedSink&) noexcept;
std::uint64_t video_source_encoded_dropped_frames(const NativeVideoSourceEncodedSink&) noexcept;
std::uint64_t video_source_encoded_dropped_events(const NativeVideoSourceEncodedSink&) noexcept;
void video_source_encoded_close(const NativeVideoSourceEncodedSink&) noexcept;

class NativeVideoObserver final {
 public:
  struct State;
  explicit NativeVideoObserver(std::unique_ptr<State> state) noexcept;
  ~NativeVideoObserver();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeLocalMediaStream final {
 public:
  struct State;
  explicit NativeLocalMediaStream(std::unique_ptr<State> state) noexcept;
  ~NativeLocalMediaStream();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeLocalMediaStreamObserver final {
 public:
  struct State;
  explicit NativeLocalMediaStreamObserver(std::unique_ptr<State> state) noexcept;
  ~NativeLocalMediaStreamObserver();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeLocalMediaStream> create_media_stream(const NativePeerConnectionFactory& factory, rust::Str id) noexcept;
rust::String local_media_stream_id(const NativeLocalMediaStream& stream) noexcept;
rust::Vec<std::size_t> local_media_stream_track_keys(const NativeLocalMediaStream& stream, bool audio) noexcept;
std::size_t local_media_stream_find_track(const NativeLocalMediaStream& stream, bool audio, rust::Str id) noexcept;
std::size_t video_track_identity(const NativeVideoTrack& track) noexcept;
bool local_media_stream_audio_track(const NativeLocalMediaStream& stream, const NativeAudioTrack& track, bool add) noexcept;
bool local_media_stream_video_track(const NativeLocalMediaStream& stream, const NativeVideoTrack& track, bool add) noexcept;
std::unique_ptr<NativeLocalMediaStreamObserver> local_media_stream_observe(const NativeLocalMediaStream& stream, std::size_t capacity) noexcept;
bool local_media_stream_observer_take(const NativeLocalMediaStreamObserver& observer) noexcept;
std::uint64_t local_media_stream_observer_take_dropped(const NativeLocalMediaStreamObserver& observer) noexcept;
void local_media_stream_observer_close(const NativeLocalMediaStreamObserver& observer) noexcept;
struct FfiDesktopOptions;
std::unique_ptr<NativeDesktopOptions> desktop_options_new(
    const NativePeerConnectionFactory& factory, const NativeEnvironment& environment,
    bool defaults, bool selected_environment) noexcept;
std::unique_ptr<NativeDesktopOptions> desktop_options_clone(const NativeDesktopOptions& options) noexcept;
FfiDesktopOptions desktop_options_read(const NativeDesktopOptions& options) noexcept;
std::uint8_t desktop_is_wayland(const NativeDesktopOptions& options) noexcept;
std::uint8_t desktop_options_write(const NativeDesktopOptions& options, const FfiDesktopOptions& fields) noexcept;
std::int64_t desktop_options_time_us(const NativeDesktopOptions& options) noexcept;
class NativeClock;
bool desktop_options_clock_matches(const NativeDesktopOptions& options, const NativeClock& clock) noexcept;
std::unique_ptr<NativeDesktopOptions> desktop_options_resource(const NativeDesktopOptions& options, std::uint8_t kind) noexcept;
std::unique_ptr<NativeDesktopOptions> desktop_resource_create(const NativeDesktopOptions& options, std::uint8_t kind, rust::Str name) noexcept;
std::uint8_t desktop_options_set_resource(const NativeDesktopOptions& options, std::uint8_t kind, bool present, const NativeDesktopOptions& resource) noexcept;
bool desktop_resource_equal(const NativeDesktopOptions& a, const NativeDesktopOptions& b, std::uint8_t kind) noexcept;
struct RustDesktopCaptureReceiver;
struct RustDesktopSelectionObserver;
std::unique_ptr<NativeScreenCapture> create_desktop_capture(
    const NativeVideoSource& source, const NativeDesktopOptions& options,
    std::uint8_t kind, bool select, std::int64_t id,
    rust::Box<RustDesktopCaptureReceiver> receiver,
    rust::Box<RustDesktopSelectionObserver> observer, rust::String& error) noexcept;

class NativeScreenCapture final {
 public:
  struct State;
  explicit NativeScreenCapture(std::unique_ptr<State> state) noexcept;
  ~NativeScreenCapture();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};

class NativeCamera final {
 public:
  struct State;
  explicit NativeCamera(std::unique_ptr<State> state) noexcept;
  ~NativeCamera();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};

class NativeVideoSource final {
 public:
  struct State;
  explicit NativeVideoSource(std::unique_ptr<State> state) noexcept;
  ~NativeVideoSource();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeMediaTrack final {
 public:
  explicit NativeMediaTrack(webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> track) noexcept;
  ~NativeMediaTrack();
  const webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface>& track() const noexcept;
 private:
  webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> track_;
};
rust::Vec<std::uint8_t> media_track_id(const NativeMediaTrack&) noexcept;
rust::Vec<std::uint8_t> media_track_kind(const NativeMediaTrack&) noexcept;
std::int32_t media_track_state(const NativeMediaTrack&) noexcept;
bool media_track_enabled(const NativeMediaTrack&) noexcept;
bool media_track_set_enabled(const NativeMediaTrack&, bool) noexcept;
bool media_track_same_resource(const NativeMediaTrack&, const NativeMediaTrack&) noexcept;
class NativeVideoTrack;
class NativeRtpSender;
class NativeRtpReceiver;
std::unique_ptr<NativeMediaTrack> video_track_reference(const NativeVideoTrack&) noexcept;
std::unique_ptr<NativeMediaTrack> rtp_sender_track_reference(const NativeRtpSender&) noexcept;
std::unique_ptr<NativeMediaTrack> rtp_receiver_track_reference(const NativeRtpReceiver&) noexcept;

class NativeVideoTrack final {
 public:
  struct State;
  explicit NativeVideoTrack(std::unique_ptr<State> state) noexcept;
  ~NativeVideoTrack();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeEncodedVideoSink final {
 public:
  struct State;
  explicit NativeEncodedVideoSink(std::unique_ptr<State> state) noexcept;
  ~NativeEncodedVideoSink();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeVideoSink final {
 public:
  struct State;
  explicit NativeVideoSink(std::unique_ptr<State> state) noexcept;
  ~NativeVideoSink();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeSframeEncrypterInit;
class NativeRtcError;
class NativePeerConnection;
struct FfiSframeEncrypterCreation;
struct FfiSframeDecrypterCreation;
class NativeSframeEncrypter final {
 public:
  struct State;
  explicit NativeSframeEncrypter(std::unique_ptr<State> state) noexcept;
  ~NativeSframeEncrypter();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
class NativeSframeDecrypter final {
 public:
  struct State;
  explicit NativeSframeDecrypter(std::unique_ptr<State> state) noexcept;
  ~NativeSframeDecrypter();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
struct FfiRtpByteString;
class NativeRtpSender;
class NativeRtpReceiver;
FfiSframeEncrypterCreation rtp_sender_create_sframe(const NativeRtpSender& sender, const NativePeerConnection& peer, const NativeSframeEncrypterInit& init) noexcept;
FfiSframeDecrypterCreation rtp_receiver_create_sframe(const NativeRtpReceiver& receiver, const NativePeerConnection& peer, std::int32_t suite) noexcept;
std::unique_ptr<NativeSframeEncrypter> sframe_encrypter_clone(const NativeSframeEncrypter& value) noexcept;
std::unique_ptr<NativeSframeDecrypter> sframe_decrypter_clone(const NativeSframeDecrypter& value) noexcept;
std::unique_ptr<NativeRtcError> sframe_set_encryption_key(const NativeSframeEncrypter& value, std::uint64_t id, rust::Slice<const std::uint8_t> key) noexcept;
std::unique_ptr<NativeRtcError> sframe_add_decryption_key(const NativeSframeDecrypter& value, std::uint64_t id, rust::Slice<const std::uint8_t> key) noexcept;
std::unique_ptr<NativeRtcError> sframe_remove_decryption_key(const NativeSframeDecrypter& value, std::uint64_t id) noexcept;

class NativeRtpSender final {
 public:
  struct State;
  explicit NativeRtpSender(std::unique_ptr<State> state) noexcept;
  ~NativeRtpSender();
  const std::unique_ptr<State>& state() const noexcept;
  webrtc::scoped_refptr<webrtc::RtpSenderInterface> sender() const noexcept;
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> sframe_owner() const noexcept;

 private:
  std::unique_ptr<State> state_;
};


class NativeRtcError;
class NativeRtpParameters;
class NativeRtpTransceiverInit;
class NativeRtpTransceiver;
bool transceiver_init_consumption_safe(const NativePeerConnection&, const NativeRtpTransceiverInit&, bool video) noexcept;
std::unique_ptr<NativeRtpTransceiver> peer_add_transceiver_value(const NativePeerConnection&, std::int32_t, const NativeRtpTransceiverInit&, bool&, std::unique_ptr<NativeRtcError>&) noexcept;
std::unique_ptr<NativeRtpTransceiver> peer_add_video_transceiver_value(const NativePeerConnection&, const NativeVideoTrack&, const NativeRtpTransceiverInit&, bool&, std::unique_ptr<NativeRtcError>&) noexcept;
std::unique_ptr<NativeRtpTransceiver> peer_add_media_transceiver_value(const NativePeerConnection&, const NativeMediaTrack&, const NativeRtpTransceiverInit&, bool&, std::unique_ptr<NativeRtcError>&) noexcept;
class NativeSetParametersOperation final {
 public:
  struct State;
  explicit NativeSetParametersOperation(std::shared_ptr<State> state) : state_(std::move(state)) {}
  const std::shared_ptr<State>& state() const noexcept { return state_; }
 private:
  std::shared_ptr<State> state_;
};
std::unique_ptr<NativeRtcError> rtp_sender_set_parameters_value(const NativeRtpSender&, const NativeRtpParameters&, bool&) noexcept;
std::shared_ptr<NativeSetParametersOperation> rtp_sender_set_parameters_async(const NativeRtpSender&, const NativeRtpParameters&, bool&) noexcept;
bool set_parameters_operation_ready(const NativeSetParametersOperation&) noexcept;
std::unique_ptr<NativeRtcError> set_parameters_operation_take(const NativeSetParametersOperation&) noexcept;
class NativeRtpReceiver;
struct FfiRtpPacketEvent;
class NativeRtpSenderObserver final {
 public:
  struct State;
  explicit NativeRtpSenderObserver(std::unique_ptr<State> state) noexcept;
  ~NativeRtpSenderObserver();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeRtpSenderObserver> rtp_sender_observer(const NativeRtpSender& value) noexcept;
std::size_t rtp_sender_observer_identity(const NativeRtpSenderObserver& value) noexcept;
bool rtp_sender_observer_register(const NativeRtpSenderObserver& value, std::size_t capacity) noexcept;
void rtp_sender_observer_unregister(const NativeRtpSenderObserver& value) noexcept;
FfiRtpPacketEvent rtp_sender_observer_take(const NativeRtpSenderObserver& value) noexcept;
std::uint64_t rtp_sender_observer_take_dropped(const NativeRtpSenderObserver& value) noexcept;
class NativeRtpReceiverObserver final {
 public:
  struct State;
  explicit NativeRtpReceiverObserver(std::unique_ptr<State> state) noexcept;
  ~NativeRtpReceiverObserver();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeRtpReceiverObserver> rtp_receiver_observer(const NativeRtpReceiver& value) noexcept;
std::size_t rtp_receiver_observer_identity(const NativeRtpReceiverObserver& value) noexcept;
bool rtp_receiver_observer_register(const NativeRtpReceiverObserver& value, std::size_t capacity) noexcept;
void rtp_receiver_observer_unregister(const NativeRtpReceiverObserver& value) noexcept;
FfiRtpPacketEvent rtp_receiver_observer_take(const NativeRtpReceiverObserver& value) noexcept;
std::uint64_t rtp_receiver_observer_take_dropped(const NativeRtpReceiverObserver& value) noexcept;

class NativeDtmfSender final {
 public:
  struct State;
  explicit NativeDtmfSender(std::unique_ptr<State> state) noexcept;
  ~NativeDtmfSender();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeRtpReceiver final {
 public:
  struct State;
  explicit NativeRtpReceiver(std::unique_ptr<State> state) noexcept;
  ~NativeRtpReceiver();
  const std::unique_ptr<State>& state() const noexcept;
  webrtc::scoped_refptr<webrtc::RtpReceiverInterface> receiver() const noexcept;
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> sframe_owner() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeTransceiverList final {
 public:
  struct State;
  explicit NativeTransceiverList(std::unique_ptr<State> state) noexcept;
  ~NativeTransceiverList();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

class NativeRtpTransceiver final {
 public:
  struct State;
  explicit NativeRtpTransceiver(std::unique_ptr<State> state) noexcept;
  ~NativeRtpTransceiver();
  const std::unique_ptr<State>& state() const noexcept;

 private:
  std::unique_ptr<State> state_;
};

struct RustCameraInitializationObserver;
class NativeCameraOptions final {
 public:
  struct State;
  explicit NativeCameraOptions(std::unique_ptr<State> state) noexcept;
  ~NativeCameraOptions();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeCameraOptions> camera_options_new(const NativePeerConnectionFactory& factory) noexcept;
std::unique_ptr<NativeCameraOptions> camera_options_clone(const NativeCameraOptions& options) noexcept;
std::uint8_t camera_options_flags(const NativeCameraOptions& options) noexcept;
bool camera_options_write(const NativeCameraOptions& options, bool v4l2, bool pipewire) noexcept;
bool camera_options_set_fd(const NativeCameraOptions& options, std::int32_t fd) noexcept;
std::int32_t camera_options_initialize(const NativeCameraOptions& options, const RustCameraInitializationObserver& observer) noexcept;

class NativeCameraDeviceInfo final {
 public:
  struct State;
  explicit NativeCameraDeviceInfo(std::unique_ptr<State> state) noexcept;
  ~NativeCameraDeviceInfo();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
struct FfiCameraInfoRequest;
struct FfiCameraInfoReply;
std::unique_ptr<NativeCameraDeviceInfo> camera_info_new(const NativePeerConnectionFactory& factory) noexcept;
std::unique_ptr<NativeCameraDeviceInfo> camera_info_new_with_options(const NativePeerConnectionFactory& factory, const NativeCameraOptions& options) noexcept;
FfiCameraInfoReply camera_info_query(const NativeCameraDeviceInfo& info, const FfiCameraInfoRequest& request) noexcept;
void camera_info_close(const NativeCameraDeviceInfo& info) noexcept;

struct FfiCameraDevice;
struct FfiCameraFormat;
struct FfiCameraDeviceControl;
struct FfiCameraSettings;
FfiCameraDeviceControl camera_device_control(rust::Str device_id, std::uint8_t kind,
    const FfiCameraSettings& settings) noexcept;
bool camera_formats(rust::Str device_id,
                    rust::Vec<FfiCameraFormat>& formats,
                    rust::String& error) noexcept;
struct FfiScreenSource;
bool screen_sources(bool windows, rust::Vec<FfiScreenSource>& screens,
                    rust::String& error) noexcept;
std::unique_ptr<NativeScreenCapture> open_screen(
    const NativeVideoSource& source, std::int64_t screen_id, bool window,
    rust::String& error) noexcept;
bool screen_capture_next_frame(const NativeScreenCapture& capture) noexcept;
std::uint8_t screen_capture_status(const NativeScreenCapture& capture) noexcept;
std::uint64_t screen_capture_failed_frames(const NativeScreenCapture& capture) noexcept;
bool close_screen(const NativeScreenCapture& capture) noexcept;
std::uint8_t desktop_set_memory_factory(const NativeScreenCapture& capture, rust::Box<RustDesktopMemoryFactory> source) noexcept;
std::uint8_t desktop_set_receiver(const NativeScreenCapture& capture, rust::Box<RustDesktopCaptureReceiver> source) noexcept;
std::uint8_t desktop_set_selection_observer(const NativeScreenCapture& capture, rust::Box<RustDesktopSelectionObserver> source) noexcept;
struct FfiDesktopMetadata;
class NativeDesktopMetadata final {
 public:
  struct State;
  explicit NativeDesktopMetadata(std::unique_ptr<State> state) noexcept;
  ~NativeDesktopMetadata();
  const std::unique_ptr<State>& state() const noexcept;
 private:
  std::unique_ptr<State> state_;
};
std::unique_ptr<NativeDesktopMetadata> desktop_capture_metadata(const NativeScreenCapture& capture) noexcept;
std::unique_ptr<NativeDesktopMetadata> desktop_metadata_clone(const NativeDesktopMetadata& metadata) noexcept;
FfiDesktopMetadata desktop_metadata_read(const NativeDesktopMetadata& metadata) noexcept;
bool desktop_metadata_equal(const NativeDesktopMetadata& a, const NativeDesktopMetadata& b, bool cancellable) noexcept;
struct FfiDesktopControl;
FfiDesktopControl desktop_control(const NativeScreenCapture& capture, std::uint8_t kind,
    std::int64_t id, std::int32_t x, std::int32_t y,
    std::uint32_t width, std::uint32_t height) noexcept;
bool camera_devices(rust::Vec<FfiCameraDevice>& devices,
                    rust::String& error) noexcept;
std::unique_ptr<NativeCamera> create_camera(const NativeVideoSource& source,
    rust::Str device_id, rust::String& error) noexcept;
std::unique_ptr<NativeCamera> create_camera_with_options(const NativeVideoSource& source,
    const NativeCameraOptions& options, rust::Str device_id, rust::String& error) noexcept;
std::unique_ptr<NativeCamera> open_camera(const NativeVideoSource& source,
                                           rust::Str device_id,
                                           std::uint32_t width,
                                           std::uint32_t height,
                                           std::uint32_t fps,
                                           rust::String& error) noexcept;
// Starting, streaming, stalled, closed, or capture stopped unexpectedly.
std::uint8_t camera_capture_status(const NativeCamera& camera,
                                   std::uint64_t stale_after_ms) noexcept;
bool close_camera(const NativeCamera& camera) noexcept;
struct RustRawCameraSink;
std::uint8_t camera_set_raw_sink(const NativeCamera& camera, rust::Box<RustRawCameraSink> source) noexcept;
struct FfiCameraSettings;
struct FfiCameraControl;
FfiCameraControl camera_control(const NativeCamera& camera, std::uint8_t kind,
    std::uint16_t rotation, bool flag, const FfiCameraSettings& settings) noexcept;

std::unique_ptr<NativeVideoSource> create_video_source(
    const NativePeerConnectionFactory& factory) noexcept;
bool close_video_source(const NativeVideoSource& source) noexcept;
std::uint8_t video_source_state(const NativeVideoSource& source) noexcept;
std::uint64_t video_source_rejected_sink_requests(const NativeVideoSource& source) noexcept;
bool video_source_view_rejected_sink_requests(const NativeVideoSourceView&, std::uint64_t&) noexcept;
std::unique_ptr<NativeVideoSourceView> video_source_as_view(const NativeVideoSource& source) noexcept;
std::unique_ptr<NativeVideoSourceView> video_track_source(const NativeVideoTrack& track, const NativePeerConnectionFactory& factory) noexcept;
std::unique_ptr<NativeVideoSourceView> video_track_source_from_producer(const NativeVideoTrack&, const NativePeerConnectionFactory&, const NativeVideoSource&) noexcept;
void video_source_view_request_refresh(const NativeVideoSourceView&) noexcept;
void video_source_view_generate_key_frame(const NativeVideoSourceView&) noexcept;
void video_source_view_process_constraints(const NativeVideoSourceView&,
    bool, double, bool, double) noexcept;
void video_track_request_refresh(const NativeVideoTrack&) noexcept;
bool video_source_views_same_interface(const NativeVideoSourceView& a, const NativeVideoSourceView& b) noexcept;
std::uint8_t video_source_view_state(const NativeVideoSourceView& source) noexcept;
bool video_source_view_remote(const NativeVideoSourceView& source) noexcept;
bool video_source_view_screencast(const NativeVideoSourceView& source) noexcept;
std::int8_t video_source_view_denoising(const NativeVideoSourceView& source) noexcept;
bool video_source_view_encoded_output(const NativeVideoSourceView& source) noexcept;
FfiVideoSourceStats video_source_view_stats(const NativeVideoSourceView& source) noexcept;
std::unique_ptr<NativeVideoObserver> video_source_view_observe(const NativeVideoSourceView& source, std::size_t capacity) noexcept;
std::unique_ptr<NativeVideoObserver> video_track_observe(const NativeVideoTrack& track, const NativePeerConnectionFactory& factory, std::size_t capacity) noexcept;
bool video_observer_take(const NativeVideoObserver& observer) noexcept;
std::uint64_t video_observer_take_dropped(const NativeVideoObserver& observer) noexcept;
void video_observer_close(const NativeVideoObserver& observer) noexcept;
bool video_source_push_encoded_trigger(
    const NativeVideoSource& source, std::uint32_t width,
    std::uint32_t height, std::int64_t timestamp_us,
    std::int64_t token) noexcept;
std::size_t video_source_i420_allocation_size(std::uint32_t width,
                                             std::uint32_t height) noexcept;
bool video_source_push_frame(const NativeVideoSource& source,
                             rust::Slice<const std::uint8_t> data,
                             std::uint32_t width, std::uint32_t height,
                             std::int64_t timestamp_us,
                             std::uint32_t rtp_timestamp,
                             std::uint16_t rotation) noexcept;
std::unique_ptr<NativeVideoTrack> create_video_track(
    const NativePeerConnectionFactory& factory,
    const NativeVideoSource& source, rust::Str id) noexcept;
rust::String video_track_id(const NativeVideoTrack& track) noexcept;
bool video_track_enabled(const NativeVideoTrack& track) noexcept;
bool video_track_set_enabled(const NativeVideoTrack& track,
                             bool enabled) noexcept;
std::uint8_t video_track_state(const NativeVideoTrack& track) noexcept;
rust::String video_track_kind(const NativeVideoTrack& track) noexcept;
bool video_track_should_receive(const NativeVideoTrack& track) noexcept;
void video_track_set_should_receive(const NativeVideoTrack& track, bool receive) noexcept;
std::uint8_t video_track_content_hint(const NativeVideoTrack& track) noexcept;
void video_track_set_content_hint(const NativeVideoTrack& track, std::uint8_t hint) noexcept;
FfiVideoSinkWants video_sink_default_wants() noexcept;
std::unique_ptr<NativeVideoSink> video_track_attach_sink(
    const NativeVideoTrack&, const NativeVideoSourceView&,
    const FfiVideoSinkWants&, std::uint8_t&) noexcept;
struct RustDecodedVideoObserver;
std::unique_ptr<NativeVideoSink> video_track_attach_observed_sink(
    const NativeVideoTrack&, const NativeVideoSourceView&,
    const FfiVideoSinkWants&, rust::Box<RustDecodedVideoObserver>, std::uint8_t&) noexcept;
std::unique_ptr<NativeVideoSink> video_source_attach_sink(
    const NativeVideoSourceView&, const FfiVideoSinkWants&, std::uint8_t&) noexcept;
std::uint8_t video_sink_update_wants(const NativeVideoSink&, const FfiVideoSinkWants&) noexcept;
FfiVideoSinkWants video_sink_wants(const NativeVideoSink&) noexcept;
bool video_source_push_native_frame(const NativeVideoSource&, const NativeRawVideoFrame&) noexcept;
std::unique_ptr<NativeRawVideoFrame> video_sink_take_native_frame(const NativeVideoSink&) noexcept;
std::unique_ptr<NativeVideoFrame> video_sink_take_frame(
    const NativeVideoSink& sink) noexcept;
std::uint64_t video_sink_dropped_frames(const NativeVideoSink& sink) noexcept;
FfiVideoSinkEvent video_sink_take_event(const NativeVideoSink& sink) noexcept;
std::uint64_t video_sink_dropped_events(const NativeVideoSink& sink) noexcept;
bool close_video_sink(const NativeVideoSink& sink) noexcept;

std::unique_ptr<NativeTransceiverList> peer_audio_transceivers(
    const NativePeerConnection& peer) noexcept;
std::unique_ptr<NativeTransceiverList> peer_video_transceivers(
    const NativePeerConnection& peer) noexcept;
rust::Vec<FfiVideoCodecCapability> peer_video_codec_capabilities(
    const NativePeerConnectionFactory& factory, bool sender) noexcept;
std::size_t transceiver_list_len(const NativeTransceiverList& list) noexcept;
std::unique_ptr<NativeRtpTransceiver> transceiver_list_at(
    const NativeTransceiverList& list, std::size_t index) noexcept;
std::unique_ptr<NativeRtpTransceiver> peer_add_video_transceiver(
    const NativePeerConnection& peer, const NativeVideoTrack& track,
    std::uint8_t direction, rust::Slice<const rust::String> rids,
    std::uint8_t& error_type, rust::String& error) noexcept;
bool peer_remove_track(const NativePeerConnection& peer,
                       const NativeRtpSender& sender,
                       std::uint8_t& error_type, rust::String& error) noexcept;
std::unique_ptr<NativeRtpTransceiver> peer_take_transceiver(
    const NativePeerConnection& peer, std::uint64_t arrival_id) noexcept;
std::unique_ptr<NativeRtpReceiver> peer_take_receiver(
    const NativePeerConnection& peer, std::uint64_t arrival_id) noexcept;
std::unique_ptr<NativeRtpTransceiver> wrap_rtp_transceiver(
    webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> transceiver) noexcept;
bool rtp_receiver_request_keyframe(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver) noexcept;
std::unique_ptr<NativeEncodedVideoSink> rtp_receiver_attach_encoded_video_sink(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver,
    bool ordered) noexcept;
struct RustEncodedVideoObserver;
std::unique_ptr<NativeEncodedVideoSink> rtp_receiver_attach_observed_encoded_video_sink(
    const NativePeerConnection&, const NativeRtpReceiver&,
    rust::Box<RustEncodedVideoObserver>) noexcept;
FfiEncodedVideoFrame encoded_video_sink_take_frame(
    const NativeEncodedVideoSink& sink) noexcept;
std::uint64_t encoded_video_sink_dropped_frames(
    const NativeEncodedVideoSink& sink) noexcept;
bool close_encoded_video_sink(const NativeEncodedVideoSink& sink) noexcept;

std::unique_ptr<NativeRtpReceiver> wrap_rtp_receiver(
    webrtc::scoped_refptr<webrtc::RtpReceiverInterface> receiver,
    webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> owner = nullptr) noexcept;
bool rtp_sender_set_media_track(const NativeRtpSender&, const NativeMediaTrack&) noexcept;
class NativeRtpEndpoints final {
 public:
  NativeRtpEndpoints();
  ~NativeRtpEndpoints();
  bool unified = false;
  std::vector<webrtc::scoped_refptr<webrtc::RtpSenderInterface>> senders;
  std::vector<webrtc::scoped_refptr<webrtc::RtpReceiverInterface>> receivers;
  std::vector<webrtc::scoped_refptr<webrtc::RtpTransceiverInterface>> transceivers;
};
std::unique_ptr<NativeRtpEndpoints> peer_endpoints(const NativePeerConnection&) noexcept;
std::size_t endpoint_list_len(const NativeRtpEndpoints&, std::uint8_t kind) noexcept;
bool endpoint_list_unified(const NativeRtpEndpoints&) noexcept;
std::unique_ptr<NativeRtpSender> endpoint_list_sender(const NativeRtpEndpoints&, std::size_t) noexcept;
std::unique_ptr<NativeRtpReceiver> endpoint_list_receiver(const NativeRtpEndpoints&, std::size_t) noexcept;
std::unique_ptr<NativeRtpTransceiver> endpoint_list_transceiver(const NativeRtpEndpoints&, std::size_t) noexcept;
std::unique_ptr<NativeRtpSender> rtp_transceiver_sender(
    const NativeRtpTransceiver& transceiver) noexcept;
std::unique_ptr<NativeRtpReceiver> rtp_transceiver_receiver(
    const NativeRtpTransceiver& transceiver) noexcept;
std::uint8_t rtp_transceiver_direction(
    const NativeRtpTransceiver& transceiver) noexcept;
std::int8_t rtp_transceiver_current_direction(
    const NativeRtpTransceiver& transceiver) noexcept;
std::int32_t rtp_transceiver_direction_value(
    const NativeRtpTransceiver& transceiver) noexcept;
bool rtp_transceiver_current_direction_value(
    const NativeRtpTransceiver& transceiver, std::int32_t& direction) noexcept;
std::unique_ptr<NativeRtcError> rtp_transceiver_set_direction_value(
    const NativeRtpTransceiver& transceiver, std::int32_t direction,
    bool& safe) noexcept;
std::unique_ptr<NativeRtcError> rtp_transceiver_stop_standard(
    const NativeRtpTransceiver& transceiver) noexcept;
bool rtp_transceiver_stopped(
    const NativeRtpTransceiver& transceiver) noexcept;
std::int32_t rtp_transceiver_media_type(
    const NativeRtpTransceiver& transceiver) noexcept;
bool rtp_transceiver_stopping(const NativeRtpTransceiver& transceiver) noexcept;
bool rtp_transceiver_receptive(const NativeRtpTransceiver& transceiver) noexcept;
bool rtp_transceiver_fired_direction(
    const NativeRtpTransceiver& transceiver, std::int32_t& direction) noexcept;
bool rtp_transceiver_sframe_enabled(
    const NativeRtpTransceiver& transceiver, bool& enabled) noexcept;
std::unique_ptr<NativeRtpCapabilities> rtp_transceiver_codec_preferences_value(
    const NativeRtpTransceiver& transceiver) noexcept;
std::unique_ptr<NativeRtpCapabilities> rtp_transceiver_header_capabilities_value(
    const NativeRtpTransceiver& transceiver, bool negotiated) noexcept;
std::unique_ptr<NativeRtcError> rtp_transceiver_set_codec_preferences_value(
    const NativeRtpTransceiver& transceiver,
    const NativeRtpCapabilities& values) noexcept;
std::unique_ptr<NativeRtcError> rtp_transceiver_set_header_capabilities_value(
    const NativeRtpTransceiver& transceiver,
    const NativeRtpCapabilities& values) noexcept;
rust::Vec<FfiRtpHeaderExtension> rtp_transceiver_header_extensions(
    const NativeRtpTransceiver& transceiver, bool negotiated) noexcept;
bool rtp_transceiver_set_header_extensions(
    const NativeRtpTransceiver& transceiver,
    rust::Slice<const FfiRtpHeaderExtension> extensions,
    std::uint8_t& error_type, rust::String& error) noexcept;
bool rtp_transceiver_mid(const NativeRtpTransceiver& transceiver,
                         rust::String& mid) noexcept;
bool rtp_transceiver_set_video_codec_preferences(
    const NativeRtpTransceiver& transceiver, const NativePeerConnectionFactory& factory,
    rust::Slice<const FfiCodecFormat> formats, std::uint8_t& error_type,
    rust::String& error) noexcept;
bool rtp_transceiver_stop(const NativeRtpTransceiver& transceiver,
                          std::uint8_t& error_type,
                          rust::String& error) noexcept;
bool rtp_transceiver_set_direction(const NativeRtpTransceiver& transceiver,
                                   std::uint8_t direction,
                                   std::uint8_t& error_type,
                                   rust::String& error) noexcept;
struct FfiDtmfToneChange;
std::size_t rtp_sender_identity(const NativeRtpSender& sender) noexcept;
std::unique_ptr<NativeRtpSender> rtp_sender_copy(const NativeRtpSender& sender) noexcept;
bool rtp_sender_same_resource(const NativeRtpSender& first, const NativeRtpSender& second) noexcept;
bool rtp_receiver_same_resource(const NativeRtpReceiver& first, const NativeRtpReceiver& second) noexcept;
bool rtp_transceiver_same_resource(const NativeRtpTransceiver& first, const NativeRtpTransceiver& second) noexcept;
std::size_t dtmf_sender_identity(const NativeDtmfSender& sender) noexcept;
std::unique_ptr<NativeDtmfSender> rtp_sender_dtmf(const NativeRtpSender& sender, const NativePeerConnection& peer) noexcept;
bool dtmf_sender_can_insert(const NativeDtmfSender& sender) noexcept;
std::uint8_t dtmf_sender_insert(const NativeDtmfSender& sender, rust::Str tones,
    std::int32_t duration, std::int32_t gap, bool has_comma_delay,
    std::int32_t comma_delay) noexcept;
rust::String dtmf_sender_tones(const NativeDtmfSender& sender) noexcept;
std::int32_t dtmf_sender_duration(const NativeDtmfSender& sender) noexcept;
std::int32_t dtmf_sender_inter_tone_gap(const NativeDtmfSender& sender) noexcept;
std::int32_t dtmf_sender_comma_delay(const NativeDtmfSender& sender) noexcept;
bool dtmf_sender_register_observer(const NativeDtmfSender& sender, std::size_t capacity) noexcept;
void dtmf_sender_unregister_observer(const NativeDtmfSender& sender) noexcept;
FfiDtmfToneChange dtmf_sender_take_tone_change(const NativeDtmfSender& sender) noexcept;
std::uint64_t dtmf_sender_take_dropped(const NativeDtmfSender& sender) noexcept;
class NativeDtlsTransport;
std::unique_ptr<NativeDtlsTransport> rtp_sender_dtls(
    const NativeRtpSender& sender, const NativePeerConnection& peer) noexcept;
std::unique_ptr<NativeDtlsTransport> rtp_receiver_dtls(
    const NativeRtpReceiver& receiver, const NativePeerConnection& peer) noexcept;
std::unique_ptr<NativeFrameEncryptor> frame_encryptor_new(rust::Box<RustFrameEncryptorProvider> provider) noexcept;
std::unique_ptr<NativeFrameEncryptor> frame_encryptor_clone(const NativeFrameEncryptor& value) noexcept;
bool frame_encryptor_same(const NativeFrameEncryptor& lhs, const NativeFrameEncryptor& rhs) noexcept;
FfiFrameCryptoFailure frame_encryptor_failure(const NativeFrameEncryptor& value) noexcept;
std::size_t frame_encryptor_max_size(const NativeFrameEncryptor& value, std::int32_t media, std::size_t size) noexcept;
FfiFrameCryptoOutput frame_encryptor_encrypt(const NativeFrameEncryptor& value, std::int32_t media, std::uint32_t ssrc,
    rust::Slice<const std::uint8_t> additional, rust::Slice<const std::uint8_t> input, std::size_t capacity) noexcept;
std::unique_ptr<NativeFrameDecryptor> frame_decryptor_new(const NativeRtpReceiver& receiver, rust::Box<RustFrameDecryptorProvider> provider) noexcept;
std::unique_ptr<NativeFrameDecryptor> frame_decryptor_clone(const NativeFrameDecryptor& value) noexcept;
bool frame_decryptor_same(const NativeFrameDecryptor& lhs, const NativeFrameDecryptor& rhs) noexcept;
FfiFrameCryptoFailure frame_decryptor_failure(const NativeFrameDecryptor& value) noexcept;
std::size_t frame_decryptor_max_size(const NativeFrameDecryptor& value, std::int32_t media, std::size_t size) noexcept;
FfiFrameCryptoOutput frame_decryptor_decrypt(const NativeFrameDecryptor& value, std::int32_t media, rust::Slice<const std::uint32_t> csrcs,
    rust::Slice<const std::uint8_t> additional, rust::Slice<const std::uint8_t> input, std::size_t capacity) noexcept;
class NativeVideoEncoderSelector;
class NativeOwnedVideoEncoderSelector;
void rtp_sender_set_encoder_selector(const NativeRtpSender&, const NativeVideoEncoderSelector*) noexcept;
void rtp_sender_set_owned_encoder_selector(const NativeRtpSender&, std::unique_ptr<NativeOwnedVideoEncoderSelector>) noexcept;
void rtp_sender_set_frame_encryptor(const NativeRtpSender& sender, const NativeFrameEncryptor* value) noexcept;
std::unique_ptr<NativeFrameEncryptor> rtp_sender_frame_encryptor(const NativeRtpSender& sender) noexcept;
bool rtp_receiver_set_frame_decryptor(const NativeRtpReceiver& receiver, const NativeFrameDecryptor* value) noexcept;
std::unique_ptr<NativeFrameDecryptor> rtp_receiver_frame_decryptor(const NativeRtpReceiver& receiver) noexcept;
bool media_type_to_string(std::int32_t kind, rust::String& name) noexcept;
std::uint32_t rtp_sender_ssrc(const NativeRtpSender& sender) noexcept;
std::int32_t rtp_sender_media_type(const NativeRtpSender& sender) noexcept;
rust::Vec<rust::String> rtp_sender_stream_ids(const NativeRtpSender& sender) noexcept;
rust::Vec<std::uint8_t> rtp_receiver_id_bytes(const NativeRtpReceiver&) noexcept;
rust::Vec<std::uint8_t> rtp_sender_id_bytes(const NativeRtpSender&) noexcept;
rust::Vec<FfiRtpByteString> rtp_receiver_stream_id_bytes(const NativeRtpReceiver&) noexcept;
rust::Vec<FfiRtpByteString> rtp_sender_stream_id_bytes(const NativeRtpSender& sender) noexcept;
void rtp_sender_set_stream_id_bytes(const NativeRtpSender& sender, rust::Slice<const FfiRtpByteString> ids) noexcept;
void rtp_sender_set_streams(const NativeRtpSender& sender,
    rust::Slice<const rust::String> ids) noexcept;
rust::String rtp_sender_id(const NativeRtpSender& sender) noexcept;
std::unique_ptr<NativeRtcError> rtp_sender_generate_keyframe(
    const NativeRtpSender& sender, const NativePeerConnection& peer,
    rust::Slice<const FfiRtpByteString> rids) noexcept;
bool rtp_sender_request_keyframe(const NativeRtpSender& sender,
                                 const NativePeerConnection& peer,
                                 rust::Slice<const rust::String> rids,
                                 std::uint8_t& error_type,
                                 rust::String& error) noexcept;
bool rtp_sender_get_parameters(const NativeRtpSender& sender,
                               FfiSenderParameters& parameters) noexcept;
bool rtp_sender_set_parameters(const NativeRtpSender& sender,
                               const FfiSenderParameters& parameters,
                               std::uint8_t& error_type,
                               rust::String& error) noexcept;
std::unique_ptr<NativeVideoTrack> rtp_sender_track(
    const NativeRtpSender& sender) noexcept;
bool rtp_sender_set_video_track(const NativeRtpSender& sender,
                                const NativeVideoTrack& track) noexcept;
bool rtp_sender_clear_track(const NativeRtpSender& sender) noexcept;
std::int32_t rtp_receiver_media_type(const NativeRtpReceiver& receiver) noexcept;
rust::Vec<rust::String> rtp_receiver_stream_ids(const NativeRtpReceiver& receiver) noexcept;
rust::String rtp_receiver_id(const NativeRtpReceiver& receiver) noexcept;
std::unique_ptr<NativeVideoTrack> rtp_receiver_track(
    const NativeRtpReceiver& receiver) noexcept;

}  // namespace pulsebeam::webrtc_sys
