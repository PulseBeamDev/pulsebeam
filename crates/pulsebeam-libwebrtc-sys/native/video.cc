#include "pulsebeam-webrtc-sys/native/video.h"
#include "pulsebeam-webrtc-sys/native/media_stream.h"
#include "pulsebeam-webrtc-sys/native/live_frame_transformer.h"
#include "pulsebeam-webrtc-sys/native/video_sink_queue.h"
#include "pulsebeam-webrtc-sys/native/video_source_controls.h"
#include "pulsebeam-webrtc-sys/native/raw_video_source.h"
#include "pulsebeam-webrtc-sys/native/video_sink_wants.h"
#include "pulsebeam-webrtc-sys/native/source_encoded_video.h"
#include "pc/video_rtp_track_source.h"
#include "pc/video_track_source_proxy.h"
#include "pulsebeam-webrtc-sys/native/audio.h"
#include "pulsebeam-webrtc-sys/native/video_codec_values.h"
#include "pulsebeam-webrtc-sys/native/rtp_values.h"
#include "pulsebeam-webrtc-sys/native/rtc_error.h"

#include <algorithm>
#include <atomic>
#include <cmath>
#include <climits>
#include <cstring>
#include <deque>
#include <limits>
#include <map>
#include <mutex>
#include <optional>
#include <string>
#include <type_traits>
#include <utility>
#include <vector>

#include "api/dtmf_sender_interface.h"
#include "api/crypto/frame_encryptor_interface.h"
#include "pulsebeam-webrtc-sys/native/video_encoder_selector.h"
#include "api/crypto/frame_decryptor_interface.h"
#include "api/frame_transformer_interface.h"
#include "pulsebeam-webrtc-sys/native/frame_callback_slot.h"
#include "pulsebeam-webrtc-sys/native/media_values.h"
#include "api/make_ref_counted.h"
#include "api/media_stream_interface.h"
#include "api/peer_connection_interface.h"
#include "api/rtp_receiver_interface.h"
#include "api/sframe/sframe_decrypter_interface.h"
#include "api/rtp_parameters.h"
#include "api/rtp_sender_interface.h"
#include "api/rtp_transceiver_interface.h"
#include "api/scoped_refptr.h"
#include "api/video/i420_buffer.h"
#include "api/video_codecs/scalability_mode.h"
#include "api/video/video_frame.h"
#include "api/video/video_sink_interface.h"
#include "media/base/video_broadcaster.h"
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
#include "modules/video_capture/video_capture.h"
#include "modules/video_capture/video_capture_factory.h"
#include "modules/video_capture/video_capture_options.h"
#include "modules/desktop_capture/desktop_capturer.h"
#include "modules/desktop_capture/delegated_source_list_controller.h"
#include "modules/desktop_capture/desktop_frame.h"
#include "modules/desktop_capture/desktop_capture_options.h"
#if defined(WEBRTC_USE_PIPEWIRE)
#include "modules/desktop_capture/linux/wayland/base_capturer_pipewire.h"
#include <glib.h>
#endif
#include "libyuv/convert.h"
#include "rtc_base/time_utils.h"
#endif
#include "pc/video_track_source.h"
#include "pulsebeam-webrtc-sys/native/codec.h"
#include "pulsebeam-webrtc-sys/native/execution.h"
#include "pulsebeam-webrtc-sys/native/clock.h"
#include "pulsebeam-webrtc-sys/native/peer.h"
#include "pulsebeam-webrtc-sys/native/transport.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include "rtc_base/thread.h"

namespace pulsebeam::webrtc_sys {

std::size_t frame_crypto_max_output_capacity() noexcept { return std::vector<std::uint8_t>().max_size(); }

class RustFrameEncryptorAdapter : public webrtc::FrameEncryptorInterface {
 public:
  explicit RustFrameEncryptorAdapter(rust::Box<RustFrameEncryptorProvider> provider) : provider_(std::move(provider)) {}
  FfiFrameCryptoFailure failure() const noexcept { return frame_encryptor_provider_failure(*provider_); }
  std::size_t GetMaxCiphertextByteSize(webrtc::MediaType media, std::size_t size) override {
    return frame_encryptor_provider_max_size(*provider_, static_cast<std::int32_t>(media), size);
  }
  int Encrypt(webrtc::MediaType media, std::uint32_t ssrc, std::span<const std::uint8_t> additional,
      std::span<const std::uint8_t> input, std::span<std::uint8_t> output, std::size_t* written) override {
    *written = 0;
    auto result = frame_encryptor_provider_encrypt(*provider_, static_cast<std::int32_t>(media), ssrc,
        {additional.data(), additional.size()}, {input.data(), input.size()}, output.size());
    // Audio and video send consumers resize their SDK buffer without this check.
    if (result.written > output.size() || result.written > result.bytes.size()) return -1;
    if (result.written != 0) std::memcpy(output.data(), result.bytes.data(), result.written);
    *written = result.written;
    return result.status;
  }
 private:
  rust::Box<RustFrameEncryptorProvider> provider_;
};
class RustFrameDecryptorAdapter : public webrtc::FrameDecryptorInterface {
 public:
  explicit RustFrameDecryptorAdapter(rust::Box<RustFrameDecryptorProvider> provider) : provider_(std::move(provider)) {}
  FfiFrameCryptoFailure failure() const noexcept { return frame_decryptor_provider_failure(*provider_); }
  std::size_t GetMaxPlaintextByteSize(webrtc::MediaType media, std::size_t size) override {
    return frame_decryptor_provider_max_size(*provider_, static_cast<std::int32_t>(media), size);
  }
  Result Decrypt(webrtc::MediaType media, const std::vector<std::uint32_t>& csrcs,
      std::span<const std::uint8_t> additional, std::span<const std::uint8_t> input,
      std::span<std::uint8_t> output) override {
    auto result = frame_decryptor_provider_decrypt(*provider_, static_cast<std::int32_t>(media),
        {csrcs.data(), csrcs.size()}, {additional.data(), additional.size()}, {input.data(), input.size()}, output.size());
    // Audio trusts bytes_written. Video may decrypt in-place, so input and SDK
    // output can overlap. Rust sees separate zero-filled scratch, never output.
    if (result.written > output.size() || result.written > result.bytes.size()) return {Status::kFailedToDecrypt, 0};
    if (result.written != 0) std::memcpy(output.data(), result.bytes.data(), result.written);
    return {static_cast<Status>(result.status), result.written};
  }
 private:
  rust::Box<RustFrameDecryptorProvider> provider_;
};
struct NativeFrameEncryptor::State { webrtc::scoped_refptr<webrtc::FrameEncryptorInterface> value; };
struct NativeFrameDecryptor::State {
  webrtc::scoped_refptr<webrtc::FrameDecryptorInterface> value;
  // A decryptor must only ever attach to one receiver, including after detach.
  webrtc::scoped_refptr<webrtc::RtpReceiverInterface> receiver;
};
NativeFrameEncryptor::NativeFrameEncryptor(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeFrameEncryptor::~NativeFrameEncryptor() = default;
const std::unique_ptr<NativeFrameEncryptor::State>& NativeFrameEncryptor::state() const noexcept { return state_; }
NativeFrameDecryptor::NativeFrameDecryptor(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeFrameDecryptor::~NativeFrameDecryptor() = default;
const std::unique_ptr<NativeFrameDecryptor::State>& NativeFrameDecryptor::state() const noexcept { return state_; }
std::unique_ptr<NativeFrameEncryptor> frame_encryptor_new(rust::Box<RustFrameEncryptorProvider> provider) noexcept {
  auto state = std::make_unique<NativeFrameEncryptor::State>();
  state->value = webrtc::make_ref_counted<RustFrameEncryptorAdapter>(std::move(provider));
  return std::make_unique<NativeFrameEncryptor>(std::move(state));
}
std::unique_ptr<NativeFrameEncryptor> frame_encryptor_clone(const NativeFrameEncryptor& value) noexcept {
  return std::make_unique<NativeFrameEncryptor>(std::make_unique<NativeFrameEncryptor::State>(*value.state()));
}
bool frame_encryptor_same(const NativeFrameEncryptor& lhs, const NativeFrameEncryptor& rhs) noexcept { return lhs.state()->value.get() == rhs.state()->value.get(); }
FfiFrameCryptoFailure frame_encryptor_failure(const NativeFrameEncryptor& value) noexcept {
  // All admitted producers are our adapters; no SDK automatic producer exists.
  return static_cast<RustFrameEncryptorAdapter*>(value.state()->value.get())->failure();
}
std::size_t frame_encryptor_max_size(const NativeFrameEncryptor& value, std::int32_t media, std::size_t size) noexcept {
  return value.state()->value->GetMaxCiphertextByteSize(static_cast<webrtc::MediaType>(media), size);
}
FfiFrameCryptoOutput frame_encryptor_encrypt(const NativeFrameEncryptor& value, std::int32_t media, std::uint32_t ssrc,
    rust::Slice<const std::uint8_t> additional, rust::Slice<const std::uint8_t> input, std::size_t capacity) noexcept {
  std::vector<std::uint8_t> output(capacity, 0);
  std::size_t written = 0;
  const auto status = value.state()->value->Encrypt(static_cast<webrtc::MediaType>(media), ssrc,
      {additional.data(), additional.size()}, {input.data(), input.size()}, output, &written);
  if (written > output.size()) return {-1, 0, {}};
  FfiFrameCryptoOutput result{status, written, {}};
  for (std::size_t i = 0; i < written; ++i) result.bytes.push_back(output[i]);
  return result;
}
std::unique_ptr<NativeFrameDecryptor> frame_decryptor_new(const NativeRtpReceiver& receiver, rust::Box<RustFrameDecryptorProvider> provider) noexcept {
  auto state = std::make_unique<NativeFrameDecryptor::State>();
  state->value = webrtc::make_ref_counted<RustFrameDecryptorAdapter>(std::move(provider));
  state->receiver = receiver.receiver();
  return std::make_unique<NativeFrameDecryptor>(std::move(state));
}
std::unique_ptr<NativeFrameDecryptor> frame_decryptor_clone(const NativeFrameDecryptor& value) noexcept {
  return std::make_unique<NativeFrameDecryptor>(std::make_unique<NativeFrameDecryptor::State>(*value.state()));
}
bool frame_decryptor_same(const NativeFrameDecryptor& lhs, const NativeFrameDecryptor& rhs) noexcept { return lhs.state()->value.get() == rhs.state()->value.get(); }
FfiFrameCryptoFailure frame_decryptor_failure(const NativeFrameDecryptor& value) noexcept {
  return static_cast<RustFrameDecryptorAdapter*>(value.state()->value.get())->failure();
}
std::size_t frame_decryptor_max_size(const NativeFrameDecryptor& value, std::int32_t media, std::size_t size) noexcept {
  return value.state()->value->GetMaxPlaintextByteSize(static_cast<webrtc::MediaType>(media), size);
}
FfiFrameCryptoOutput frame_decryptor_decrypt(const NativeFrameDecryptor& value, std::int32_t media, rust::Slice<const std::uint32_t> csrcs,
    rust::Slice<const std::uint8_t> additional, rust::Slice<const std::uint8_t> input, std::size_t capacity) noexcept {
  std::vector<std::uint8_t> output(capacity, 0);
  const auto result = value.state()->value->Decrypt(static_cast<webrtc::MediaType>(media),
      std::vector<std::uint32_t>(csrcs.begin(), csrcs.end()), {additional.data(), additional.size()}, {input.data(), input.size()}, output);
  if (result.bytes_written > output.size()) return {static_cast<std::int32_t>(webrtc::FrameDecryptorInterface::Status::kFailedToDecrypt), 0, {}};
  FfiFrameCryptoOutput converted{static_cast<std::int32_t>(result.status), result.bytes_written, {}};
  for (std::size_t i = 0; i < result.bytes_written; ++i) converted.bytes.push_back(output[i]);
  return converted;
}
void rtp_sender_set_encoder_selector(const NativeRtpSender& sender, const NativeVideoEncoderSelector* selector) noexcept {
  sender.sender()->SetEncoderSelector(selector ? selector->value() : nullptr);
}
void rtp_sender_set_owned_encoder_selector(const NativeRtpSender& sender, std::unique_ptr<NativeOwnedVideoEncoderSelector> selector) noexcept {
  sender.sender()->SetEncoderSelector(selector ? selector->Take() : nullptr);
}
void rtp_sender_set_frame_encryptor(const NativeRtpSender& sender, const NativeFrameEncryptor* value) noexcept {
  sender.sender()->SetFrameEncryptor(value ? value->state()->value : nullptr);
}
std::unique_ptr<NativeFrameEncryptor> rtp_sender_frame_encryptor(const NativeRtpSender& sender) noexcept {
  auto value = sender.sender()->GetFrameEncryptor();
  if (!value) return nullptr;
  auto state = std::make_unique<NativeFrameEncryptor::State>();
  state->value = std::move(value);
  return std::make_unique<NativeFrameEncryptor>(std::move(state));
}
bool rtp_receiver_set_frame_decryptor(const NativeRtpReceiver& receiver, const NativeFrameDecryptor* value) noexcept {
  if (value && value->state()->receiver.get() != receiver.receiver().get()) return false;
  receiver.receiver()->SetFrameDecryptor(value ? value->state()->value : nullptr);
  return true;
}
std::unique_ptr<NativeFrameDecryptor> rtp_receiver_frame_decryptor(const NativeRtpReceiver& receiver) noexcept {
  auto value = receiver.receiver()->GetFrameDecryptor();
  if (!value) return nullptr;
  auto state = std::make_unique<NativeFrameDecryptor::State>();
  state->value = std::move(value);
  state->receiver = receiver.receiver();
  return std::make_unique<NativeFrameDecryptor>(std::move(state));
}
// This SDK pin returns native SFrame creation errors, after possible owner
// mutation. Keep actual returned-interface dispatch rather than fake handles.
struct NativeSframeEncrypter::State {
  webrtc::scoped_refptr<webrtc::SframeEncrypterInterface> value;
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> owner;
  webrtc::Thread* signaling;
};
struct NativeSframeDecrypter::State {
  webrtc::scoped_refptr<webrtc::SframeDecrypterInterface> value;
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> owner;
  webrtc::Thread* signaling;
};
NativeSframeEncrypter::NativeSframeEncrypter(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeSframeDecrypter::NativeSframeDecrypter(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeSframeEncrypter::~NativeSframeEncrypter() {
  if (state_ && state_->value) state_->signaling->BlockingCall([value = std::move(state_->value)]() mutable { value = nullptr; });
}
NativeSframeDecrypter::~NativeSframeDecrypter() {
  if (state_ && state_->value) state_->signaling->BlockingCall([value = std::move(state_->value)]() mutable { value = nullptr; });
}
const std::unique_ptr<NativeSframeEncrypter::State>& NativeSframeEncrypter::state() const noexcept { return state_; }
const std::unique_ptr<NativeSframeDecrypter::State>& NativeSframeDecrypter::state() const noexcept { return state_; }
FfiSframeEncrypterCreation rtp_sender_create_sframe(const NativeRtpSender& sender, const NativePeerConnection& peer, const NativeSframeEncrypterInit& init) noexcept {
  // The SDK binds this endpoint's creation callback to raw transceiver this.
  // Peer retention alone is insufficient after negotiation/rollback removal.
  auto owner = sender.sframe_owner();
  if (!owner) return {nullptr, nullptr, true};
  auto result = sender.sender()->CreateSframeEncrypterOrError(init.value());
  if (!result.ok()) return {nullptr, std::make_unique<NativeRtcError>(result.MoveError()), false};
  auto state = std::make_unique<NativeSframeEncrypter::State>();
  state->value = result.MoveValue();
  state->owner = std::move(owner);
  state->signaling = peer.signaling_thread();
  if (!state->value) return {nullptr, nullptr, false};
  return {std::make_unique<NativeSframeEncrypter>(std::move(state)), nullptr, false};
}
FfiSframeDecrypterCreation rtp_receiver_create_sframe(const NativeRtpReceiver& receiver, const NativePeerConnection& peer, std::int32_t suite) noexcept {
  auto owner = receiver.sframe_owner();
  const bool unified = peer.peer()->GetConfiguration().sdp_semantics == webrtc::SdpSemantics::kUnifiedPlan;
  if (!owner && unified) {
    // A receiver-only TrackRemoved record may predate transceiver destruction.
    // Use only exact SDK graph ownership, never IDs/SSRCs or guessed joins.
    for (const auto& candidate : peer.peer()->GetTransceivers()) {
      if (candidate->receiver() == receiver.receiver()) { owner = candidate; break; }
    }
    if (!owner) return {nullptr, nullptr, true};
  }
  // Plan B endpoints have no owner callback and safely return INTERNAL_ERROR.
  auto result = receiver.receiver()->CreateSframeDecrypterOrError(static_cast<webrtc::SframeCipherSuite>(suite));
  if (!result.ok()) return {nullptr, std::make_unique<NativeRtcError>(result.MoveError()), false};
  auto state = std::make_unique<NativeSframeDecrypter::State>();
  state->value = result.MoveValue();
  state->owner = std::move(owner);
  state->signaling = peer.signaling_thread();
  if (!state->value) return {nullptr, nullptr, false};
  return {std::make_unique<NativeSframeDecrypter>(std::move(state)), nullptr, false};
}
std::unique_ptr<NativeSframeEncrypter> sframe_encrypter_clone(const NativeSframeEncrypter& value) noexcept {
  return std::make_unique<NativeSframeEncrypter>(std::make_unique<NativeSframeEncrypter::State>(*value.state()));
}
std::unique_ptr<NativeSframeDecrypter> sframe_decrypter_clone(const NativeSframeDecrypter& value) noexcept {
  return std::make_unique<NativeSframeDecrypter>(std::make_unique<NativeSframeDecrypter::State>(*value.state()));
}
std::unique_ptr<NativeRtcError> sframe_set_encryption_key(const NativeSframeEncrypter& value, std::uint64_t id, rust::Slice<const std::uint8_t> key) noexcept {
  return value.state()->signaling->BlockingCall([&] {
    return std::make_unique<NativeRtcError>(value.state()->value->SetEncryptionKey(id, {key.data(), key.size()}));
  });
}
std::unique_ptr<NativeRtcError> sframe_add_decryption_key(const NativeSframeDecrypter& value, std::uint64_t id, rust::Slice<const std::uint8_t> key) noexcept {
  return value.state()->signaling->BlockingCall([&] {
    return std::make_unique<NativeRtcError>(value.state()->value->AddDecryptionKey(id, {key.data(), key.size()}));
  });
}
std::unique_ptr<NativeRtcError> sframe_remove_decryption_key(const NativeSframeDecrypter& value, std::uint64_t id) noexcept {
  return value.state()->signaling->BlockingCall([&] {
    return std::make_unique<NativeRtcError>(value.state()->value->RemoveDecryptionKey(id));
  });
}
namespace {

#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
class ScreenFrameSink final : public webrtc::DesktopCapturer::Callback {
 public:
  ScreenFrameSink(webrtc::scoped_refptr<PushVideoSource> source,
                  const std::atomic<bool>& source_closed,
                  std::atomic<std::uint8_t>& status,
                  webrtc::Clock* clock)
      : source_(std::move(source)), source_closed_(source_closed), status_(status), clock_(clock) {}
  void SetReceiver(rust::Box<RustDesktopCaptureReceiver> source) {
    std::shared_ptr<rust::Box<RustDesktopCaptureReceiver>> next;
    if (desktop_receiver_present(*source)) next = std::make_shared<rust::Box<RustDesktopCaptureReceiver>>(std::move(source));
    { std::lock_guard lock(receiver_mutex_); receiver_.swap(next); }
  }
  void OnFrameCaptureStart() override {
    auto receiver = Receiver();
    if (receiver) desktop_receiver_start(**receiver);
  }
  void OnCaptureResult(webrtc::DesktopCapturer::Result result,
                       std::unique_ptr<webrtc::DesktopFrame> frame) override {
    if (auto receiver = Receiver()) {
      if (result != webrtc::DesktopCapturer::Result::SUCCESS || !frame) {
        if (result == webrtc::DesktopCapturer::Result::ERROR_PERMANENT) {
          auto current = status_.load();
          while (current != 6 && !status_.compare_exchange_weak(current, 4)) {}
        }
        Failed();
      } else {
        auto current = status_.load();
        while (current != 2 && current != 3 && current != 4 && current != 6 &&
               !status_.compare_exchange_weak(current, 2)) {}
      }
      desktop_receiver_result(**receiver, static_cast<std::int32_t>(result), wrap_desktop_frame(std::move(frame)));
      return;
    }
    if (source_closed_.load()) return;
    if (result != webrtc::DesktopCapturer::Result::SUCCESS || !frame) {
      if (result == webrtc::DesktopCapturer::Result::ERROR_PERMANENT) {
        auto current = status_.load();
        while (current != 6 && !status_.compare_exchange_weak(current, 4)) {}
      }
      Failed();
      return;
    }
    if (status_.load() == 3 || status_.load() == 4 || status_.load() == 6) return;
    std::uint8_t current = status_.load();
    while (current != 2 && current != 3 && current != 4 && current != 6 &&
           !status_.compare_exchange_weak(current, 2)) {}
    if (current == 3 || current == 4 || current == 6) return;
    const int width = frame->size().width();
    const int height = frame->size().height();
    // Portal/accelerated capturers can return texture-backed frames with no
    // CPU buffer. Do not dereference them or interpret non-ARGB pixels as ARGB.
    const std::int64_t stride = frame->stride();
    const std::int64_t absolute_stride = stride < 0 ? -stride : stride;
    if (width <= 0 || height <= 0 || width > INT32_MAX / 4 ||
        width > INT32_MAX / height / 4 ||
        !frame->data() || frame->pixel_format() != webrtc::FOURCC_ARGB ||
        absolute_stride < std::int64_t(width) * webrtc::DesktopFrame::kBytesPerPixel ||
        absolute_stride > INT32_MAX / height) {
      Failed();
      return;
    }
    auto buffer = webrtc::I420Buffer::Create(width, height);
    if (libyuv::ARGBToI420(frame->data(), frame->stride(),
                          buffer->MutableDataY(), buffer->StrideY(),
                          buffer->MutableDataU(), buffer->StrideU(),
                          buffer->MutableDataV(), buffer->StrideV(),
                          width, height) != 0) {
      Failed();
      return;
    }
    if (status_.load() == 6) return;
    source_->Push(webrtc::VideoFrame::Builder()
                      .set_video_frame_buffer(std::move(buffer))
                      .set_timestamp_us(clock_->CurrentTime().us()).build());
  }
  std::uint64_t failed_frames() const { return failed_frames_.load(); }
 private:
  void Failed() {
    auto old = failed_frames_.load();
    while (old != std::numeric_limits<std::uint64_t>::max() &&
           !failed_frames_.compare_exchange_weak(old, old + 1)) {}
  }
  webrtc::scoped_refptr<PushVideoSource> source_;
  const std::atomic<bool>& source_closed_;
  std::atomic<std::uint8_t>& status_;
  webrtc::Clock* clock_;  // Borrowed from the owning capture's retained options.
  std::atomic<std::uint64_t> failed_frames_{0};
  std::shared_ptr<rust::Box<RustDesktopCaptureReceiver>> Receiver() {
    std::lock_guard lock(receiver_mutex_); return receiver_;
  }
  std::mutex receiver_mutex_;
  std::shared_ptr<rust::Box<RustDesktopCaptureReceiver>> receiver_;
};

class ScreenSelectionObserver final
    : public webrtc::DelegatedSourceListController::Observer {
 public:
  explicit ScreenSelectionObserver(std::atomic<std::uint8_t>& status)
      : status_(status) {}
  void SetObserver(rust::Box<RustDesktopSelectionObserver> source) {
    std::shared_ptr<rust::Box<RustDesktopSelectionObserver>> next;
    if (desktop_selection_present(*source)) next = std::make_shared<rust::Box<RustDesktopSelectionObserver>>(std::move(source));
    { std::lock_guard lock(observer_mutex_); observer_.swap(next); }
  }
  void OnSelection() override {
    std::uint8_t pending = 0;
    status_.compare_exchange_strong(pending, 1);
    Notify(0);
  }
  void OnCancelled() override { SetUnlessSessionClosed(3); Notify(1); }
  void OnError() override { SetUnlessSessionClosed(4); Notify(2); }
 private:
  void Notify(std::uint8_t event) {
    std::shared_ptr<rust::Box<RustDesktopSelectionObserver>> observer;
    { std::lock_guard lock(observer_mutex_); observer = observer_; }
    if (observer) desktop_selection_notify(**observer, event);
  }
  void SetUnlessSessionClosed(std::uint8_t value) {
    auto current = status_.load();
    while (current != 6 && !status_.compare_exchange_weak(current, value)) {}
  }
  std::atomic<std::uint8_t>& status_;
  std::mutex observer_mutex_;
  std::shared_ptr<rust::Box<RustDesktopSelectionObserver>> observer_;
};

#if defined(WEBRTC_USE_PIPEWIRE)
// Use the same upstream PipeWire capturer selected by the Linux factory, but
// retain the portal notifier event which DesktopCapturer does not expose.
class ObservablePipeWireCapturer final : public webrtc::BaseCapturerPipeWire {
 public:
  ObservablePipeWireCapturer(const webrtc::DesktopCaptureOptions& options,
                            webrtc::CaptureType type,
                            std::atomic<std::uint8_t>& status,
                            const std::atomic<bool>& closed)
      : BaseCapturerPipeWire(options, type), status_(status), closed_(closed) {}
  void OnScreenCastSessionClosed() override {
    auto current = status_.load();
    while (!closed_.load() && current != 3 && current != 4 && current != 6 &&
           !status_.compare_exchange_weak(current, 6)) {}
    BaseCapturerPipeWire::OnScreenCastSessionClosed();
  }
 private:
  std::atomic<std::uint8_t>& status_;
  const std::atomic<bool>& closed_;
};
#endif

class RawCameraFrameSink final : public webrtc::RawVideoSinkInterface {
 public:
  RawCameraFrameSink(rust::Box<RustRawCameraSink> source,
      std::atomic<std::int64_t>& last_frame, std::atomic<bool>& seen)
      : source_(std::move(source)), last_frame_(last_frame), seen_(seen) {}
  std::int32_t OnRawFrame(std::uint8_t* bytes, std::size_t size,
      const webrtc::VideoCaptureCapability& settings, webrtc::VideoRotation rotation,
      std::int64_t capture_time) override {
    last_frame_.store(webrtc::TimeMicros()); seen_.store(true);
    FfiCameraSettings value{};
    value.width = settings.width; value.height = settings.height;
    value.max_fps = settings.maxFPS; value.pixel_format = static_cast<std::int32_t>(settings.videoType);
    value.interlaced = settings.interlaced;
    std::uint8_t empty = 0;
    if (!bytes && size) return -1;
    return camera_raw_frame(*source_, {bytes ? bytes : &empty, size}, value,
        static_cast<std::uint16_t>(rotation), capture_time);
  }
 private:
  rust::Box<RustRawCameraSink> source_;
  std::atomic<std::int64_t>& last_frame_;
  std::atomic<bool>& seen_;
};

class CameraFrameSink final : public webrtc::VideoSinkInterface<webrtc::VideoFrame> {
 public:
  CameraFrameSink(webrtc::scoped_refptr<PushVideoSource> source,
                  const std::atomic<bool>& source_closed,
                  std::atomic<std::int64_t>& last_frame_us,
                  std::atomic<bool>& frame_seen)
      : source_(std::move(source)), source_closed_(source_closed),
        last_frame_us_(last_frame_us), frame_seen_(frame_seen) {}
  void OnFrame(const webrtc::VideoFrame& frame) override {
    if (source_closed_.load()) return;
    last_frame_us_.store(webrtc::TimeMicros());
    frame_seen_.store(true);
    source_->Push(frame);
  }
 private:
  webrtc::scoped_refptr<PushVideoSource> source_;
  const std::atomic<bool>& source_closed_;
  std::atomic<std::int64_t>& last_frame_us_;
  std::atomic<bool>& frame_seen_;
};
#endif



// Raw interception consumes receiver frames before native reference finding.
// Ordered observation snapshots metadata and forwards the original unchanged;
// the recordable-frame callback then releases its exact native RTP identity.
// Keep one transformer per receiver lifetime: replacing upstream's delegate
// does not reset the old callback registration in this pinned revision.
// Dispatch owned observations only after releasing collector locks. Shared
// ownership fences concurrent callback retirement and native sink teardown.
struct EncodedObserverNotification {
  std::shared_ptr<rust::Box<RustEncodedVideoObserver>> observer;
  FfiEncodedVideoFrame frame{};
  std::uint64_t dropped = 0;
  ~EncodedObserverNotification() {
    if (observer && (frame.available || dropped))
      encoded_video_frame_observed(**observer, std::move(frame), dropped);
  }
};

class EncodedFrameCollector
    : public webrtc::FrameTransformerInterface,
      public webrtc::VideoSinkInterface<webrtc::RecordableEncodedFrame> {
 public:
  EncodedFrameCollector(std::shared_ptr<ReadinessSignal> readiness, bool ordered,
      std::optional<rust::Box<RustEncodedVideoObserver>> observer = std::nullopt)
      : readiness_(std::move(readiness)), ordered_(ordered) {
    if (observer) observer_ =
        std::make_shared<rust::Box<RustEncodedVideoObserver>>(std::move(*observer));
  }
  void Transform(std::unique_ptr<webrtc::TransformableFrameInterface> frame) override {
    ReadinessNotification notify{};
    EncodedObserverNotification observation;
    std::shared_ptr<StockFrameCallbackSlot> callback;
    {
      std::lock_guard lock(mutex_);
      if (active_ && frame->GetDirection() ==
                         webrtc::TransformableFrameInterface::Direction::kReceiver) {
        observation.observer = observer_;
        auto* video = static_cast<webrtc::TransformableVideoFrameInterface*>(frame.get());
        const auto data = frame->GetData();
        if (data.size() <= kMaxBytes) {
          auto& queue = ordered_ ? pending_frames_ : frames_;
          auto& queued_bytes = ordered_ ? pending_bytes_ : queued_bytes_;
          const auto max_frames = ordered_ ? kMaxPendingFrames : kMaxFrames;
          while (!queue.empty() &&
                 (queue.size() >= max_frames ||
                  queued_bytes > kMaxBytes - data.size())) {
            queued_bytes -= queue.front().data.size();
            queue.pop_front();
            Lost();
            ++observation.dropped;
          }
          StoredFrame snapshot;
          snapshot.data.assign(data.begin(), data.end());
          snapshot.mime_type = frame->GetMimeType();
          snapshot.rtp_timestamp = frame->GetTimestamp();
          snapshot.ssrc = frame->GetSsrc();
          snapshot.payload_type = frame->GetPayloadType();
          snapshot.key_frame = video->IsKeyFrame();
          snapshot.rid = video->Rid();
          if (auto time = video->CaptureTime()) snapshot.capture_time_us = MediaTimestampBits(*time);
          if (auto time = video->ReceiveTime()) snapshot.receive_time_us = MediaTimestampBits(*time);
          const auto metadata = video->Metadata();
          snapshot.frame_id = metadata.GetFrameId();
          if (snapshot.frame_id) {
            snapshot.spatial_index = metadata.GetSpatialIndex();
            snapshot.temporal_index = metadata.GetTemporalIndex();
            if (auto dependencies = metadata.GetDependencies()) {
              snapshot.dependencies.assign(dependencies->begin(), dependencies->end());
            }
            for (auto indication : metadata.GetDecodeTargetIndications())
              snapshot.decode_target_indications.push_back(static_cast<std::uint8_t>(indication));
          }
          queued_bytes += snapshot.data.size();
          queue.push_back(std::move(snapshot));
        } else {
          Lost();
          ++observation.dropped;
        }
        if (!ordered_) {
          notify.readiness = readiness_;
          return;
        }
      }
      const auto it = callbacks_.find(frame->GetSsrc());
      if (it != callbacks_.end()) callback = it->second;
    }
    // Native reference finding, frame buffering, and decode scheduling remain
    // authoritative. The recordable-frame sink below observes only after that
    // path has selected the next encoded frame.
    if (callback) callback->OnTransformedFrame(std::move(frame));
  }

  void OnFrame(const webrtc::RecordableEncodedFrame& frame) override {
    ReadinessNotification notify{};
    EncodedObserverNotification observation;
    const auto buffer = frame.encoded_buffer();
    {
      std::lock_guard lock(mutex_);
      if (!active_) return;
      observation.observer = observer_;
      if (!buffer) {
        observation.dropped = 1;
        return;
      }
      const auto rtp_timestamp = frame.rtp_timestamp();
      const auto ssrc = frame.ssrc();
      if (!rtp_timestamp || !ssrc) {
        Lost();
        observation.dropped = 1;
        return;
      }
      const auto match = std::find_if(
          pending_frames_.begin(), pending_frames_.end(), [&](const StoredFrame& candidate) {
            return candidate.rtp_timestamp == *rtp_timestamp &&
                   candidate.ssrc == *ssrc;
          });
      if (match == pending_frames_.end() ||
          match->key_frame != frame.is_key_frame() ||
          match->data.size() != buffer->size() ||
          !std::equal(match->data.begin(), match->data.end(), buffer->data())) {
        Lost();
        observation.dropped = 1;
        return;
      }
      StoredFrame snapshot = std::move(*match);
      pending_bytes_ -= snapshot.data.size();
      pending_frames_.erase(match);
      if (observation.observer) observation.frame = Snapshot(snapshot);
      while (!frames_.empty() &&
             (frames_.size() >= kMaxFrames ||
              queued_bytes_ > kMaxBytes - snapshot.data.size())) {
        queued_bytes_ -= frames_.front().data.size();
        frames_.pop_front();
        Lost();
      }
      queued_bytes_ += snapshot.data.size();
      frames_.push_back(std::move(snapshot));
      notify.readiness = readiness_;
    }
  }

  void RegisterTransformedFrameSinkCallback(
      webrtc::scoped_refptr<webrtc::TransformedFrameCallback> callback,
      std::uint32_t ssrc) override {
    auto replacement = std::make_shared<StockFrameCallbackSlot>(std::move(callback));
    std::shared_ptr<StockFrameCallbackSlot> retired;
    {
      std::lock_guard lock(mutex_);
      retired = std::exchange(callbacks_[ssrc], std::move(replacement));
    }
    if (retired) retired->Retire();
  }
  void UnregisterTransformedFrameSinkCallback(std::uint32_t ssrc) override {
    std::shared_ptr<StockFrameCallbackSlot> retired;
    {
      std::lock_guard lock(mutex_);
      const auto it = callbacks_.find(ssrc);
      if (it != callbacks_.end()) {
        retired = std::move(it->second);
        callbacks_.erase(it);
      }
    }
    if (retired) retired->Retire();
  }

  FfiEncodedVideoFrame Take() {
    std::lock_guard lock(mutex_);
    FfiEncodedVideoFrame result{};
    if (frames_.empty()) return result;
    auto frame = std::move(frames_.front());
    frames_.pop_front();
    queued_bytes_ -= frame.data.size();
    return Snapshot(frame);
  }
  std::uint64_t DroppedFrames() {
    std::lock_guard lock(mutex_);
    return dropped_frames_;
  }
  void Deactivate() {
    std::shared_ptr<rust::Box<RustEncodedVideoObserver>> retired;
    {
      std::lock_guard lock(mutex_);
      active_ = false;
      pending_frames_.clear();
      pending_bytes_ = 0;
      frames_.clear();
      queued_bytes_ = 0;
      retired = std::move(observer_);
    }
    if (retired) encoded_video_observer_closed(**retired);
  }

 private:
  void Lost() {
    if (dropped_frames_ != std::numeric_limits<std::uint64_t>::max()) ++dropped_frames_;
  }
  struct StoredFrame {
    std::vector<std::uint8_t> data;
    std::string mime_type;
    std::uint32_t rtp_timestamp = 0;
    std::uint32_t ssrc = 0;
    std::uint8_t payload_type = 0;
    bool key_frame = false;
    std::optional<std::string> rid;
    std::optional<std::int64_t> capture_time_us;
    std::optional<std::int64_t> receive_time_us;
    std::optional<std::int64_t> frame_id;
    std::int32_t spatial_index = 0;
    std::int32_t temporal_index = 0;
    std::vector<std::int64_t> dependencies;
    std::vector<std::uint8_t> decode_target_indications;
  };
  static FfiEncodedVideoFrame Snapshot(const StoredFrame& frame) {
    FfiEncodedVideoFrame result{};
    result.available = true;
    for (auto byte : frame.data) result.data.push_back(byte);
    result.mime_type = frame.mime_type;
    result.rtp_timestamp = frame.rtp_timestamp;
    result.ssrc = frame.ssrc;
    result.payload_type = frame.payload_type;
    result.key_frame = frame.key_frame;
    result.has_rid = frame.rid.has_value();
    result.rid = frame.rid.value_or("");
    result.has_capture_time = frame.capture_time_us.has_value();
    result.capture_time_us = frame.capture_time_us.value_or(0);
    result.has_receive_time = frame.receive_time_us.has_value();
    result.receive_time_us = frame.receive_time_us.value_or(0);
    result.has_frame_id = frame.frame_id.has_value();
    result.frame_id = frame.frame_id.value_or(0);
    result.spatial_index = frame.spatial_index;
    result.temporal_index = frame.temporal_index;
    for (auto dependency : frame.dependencies) result.dependencies.push_back(dependency);
    for (auto indication : frame.decode_target_indications)
      result.decode_target_indications.push_back(indication);
    return result;
  }
  const std::shared_ptr<ReadinessSignal> readiness_;
  std::shared_ptr<rust::Box<RustEncodedVideoObserver>> observer_;
  const bool ordered_;
  static constexpr std::size_t kMaxFrames = 4;
  static constexpr std::size_t kMaxPendingFrames = 512;
  static constexpr std::size_t kMaxBytes = 4 * 1024 * 1024;
  std::mutex mutex_;
  std::map<std::uint32_t, std::shared_ptr<StockFrameCallbackSlot>> callbacks_;
  std::deque<StoredFrame> pending_frames_;
  std::size_t pending_bytes_ = 0;
  std::deque<StoredFrame> frames_;
  std::size_t queued_bytes_ = 0;
  std::uint64_t dropped_frames_ = 0;
  bool active_ = true;
};

std::optional<webrtc::RtpTransceiverDirection> Direction(std::uint8_t value) {
  if (value > static_cast<std::uint8_t>(
                  webrtc::RtpTransceiverDirection::kInactive)) {
    return std::nullopt;
  }
  return static_cast<webrtc::RtpTransceiverDirection>(value);
}

void SetError(const webrtc::RTCError& rtc_error, std::uint8_t& error_type,
              rust::String& error) {
  error_type = static_cast<std::uint8_t>(rtc_error.type());
  error = rtc_error.message();
}

rust::String FeedbackName(const webrtc::RtcpFeedback& feedback) {
  std::string name;
  switch (feedback.type) {
    case webrtc::RtcpFeedbackType::NONE: name = "none"; break;
    case webrtc::RtcpFeedbackType::CCM: name = "ccm"; break;
    case webrtc::RtcpFeedbackType::LNTF: name = "goog-lntf"; break;
    case webrtc::RtcpFeedbackType::NACK: name = "nack"; break;
    case webrtc::RtcpFeedbackType::REMB: name = "goog-remb"; break;
    case webrtc::RtcpFeedbackType::TRANSPORT_CC: name = "transport-cc"; break;
    case webrtc::RtcpFeedbackType::CCFB: name = "ccfb"; break;
    default: name = "unknown:" + std::to_string(static_cast<int>(feedback.type)); break;
  }
  if (feedback.message_type) {
    switch (*feedback.message_type) {
      case webrtc::RtcpFeedbackMessageType::GENERIC_NACK: name += "/generic"; break;
      case webrtc::RtcpFeedbackMessageType::PLI: name += "/pli"; break;
      case webrtc::RtcpFeedbackMessageType::FIR: name += "/fir"; break;
      default: name += "/unknown:" + std::to_string(static_cast<int>(*feedback.message_type)); break;
    }
  }
  return name;
}

webrtc::scoped_refptr<webrtc::VideoTrackInterface> VideoTrack(
    const webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface>& track) {
  if (!track || track->kind() != webrtc::MediaStreamTrackInterface::kVideoKind) {
    return nullptr;
  }
  return webrtc::scoped_refptr<webrtc::VideoTrackInterface>(
      static_cast<webrtc::VideoTrackInterface*>(track.get()));
}

}  // namespace

struct NativeDesktopOptions::State {
  webrtc::Thread* role = nullptr;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  std::optional<webrtc::DesktopCaptureOptions> value;
#endif
};
NativeDesktopOptions::NativeDesktopOptions(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeDesktopOptions::~NativeDesktopOptions() {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (state_->role && !state_->role->IsCurrent())
    state_->role->BlockingCall([&] { state_->value.reset(); });
  else state_->value.reset();
#endif
}
const std::unique_ptr<NativeDesktopOptions::State>& NativeDesktopOptions::state() const noexcept { return state_; }

std::unique_ptr<NativeDesktopOptions> desktop_options_new(
    const NativePeerConnectionFactory& factory, const NativeEnvironment& environment,
    bool defaults, bool selected_environment) noexcept {
  auto* role = factory.signaling_thread();
  if (role && !role->IsCurrent()) return role->BlockingCall([&] {
    return desktop_options_new(factory, environment, defaults, selected_environment);
  });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto state = std::make_unique<NativeDesktopOptions::State>();
  state->role = role;
  if (selected_environment) state->value = defaults
      ? webrtc::DesktopCaptureOptions::CreateDefault(environment.environment())
      : webrtc::DesktopCaptureOptions(environment.environment());
  else state->value = defaults ? webrtc::DesktopCaptureOptions::CreateDefault()
                              : webrtc::DesktopCaptureOptions();
  return std::make_unique<NativeDesktopOptions>(std::move(state));
#else
  return nullptr;
#endif
}
std::unique_ptr<NativeDesktopOptions> desktop_options_clone(const NativeDesktopOptions& options) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_options_clone(options); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto state = std::make_unique<NativeDesktopOptions::State>();
  state->role = role; state->value = options.state()->value;
  return std::make_unique<NativeDesktopOptions>(std::move(state));
#else
  return nullptr;
#endif
}
std::uint8_t desktop_is_wayland(const NativeDesktopOptions& options) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_is_wayland(options); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS) && (defined(WEBRTC_USE_PIPEWIRE) || defined(WEBRTC_USE_X11))
  return webrtc::DesktopCapturer::IsRunningUnderWayland() ? 1 : 0;
#else
  return 2;
#endif
}
FfiDesktopOptions desktop_options_read(const NativeDesktopOptions& options) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_options_read(options); });
  FfiDesktopOptions result{};
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  const auto& value = *options.state()->value;
  result.use_update_notifications = value.use_update_notifications();
  result.disable_effects = value.disable_effects();
  result.detect_updated_region = value.detect_updated_region();
  result.prefer_cursor_embedded = value.prefer_cursor_embedded();
  result.has_full_screen_window_detector = bool(value.full_screen_window_detector());
#if defined(WEBRTC_USE_X11)
  result.x11_supported = true; result.has_x_display = bool(value.x_display());
#endif
#if defined(WEBRTC_USE_PIPEWIRE)
  result.pipewire_supported = true;
  result.allow_pipewire = value.allow_pipewire();
  result.width = value.get_width(); result.height = value.get_height();
  result.pipewire_use_damage_region = value.pipewire_use_damage_region();
  result.has_screencast_stream = bool(value.screencast_stream());
#endif
#endif
  return result;
}
std::uint8_t desktop_options_write(const NativeDesktopOptions& options, const FfiDesktopOptions& fields) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_options_write(options, fields); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
#if !defined(WEBRTC_USE_PIPEWIRE)
  if (fields.pipewire_supported) return 2;
#endif
  auto& value = *options.state()->value;
  value.set_use_update_notifications(fields.use_update_notifications);
  value.set_disable_effects(fields.disable_effects);
  value.set_detect_updated_region(fields.detect_updated_region);
  value.set_prefer_cursor_embedded(fields.prefer_cursor_embedded);
#if defined(WEBRTC_USE_PIPEWIRE)
  if (fields.pipewire_supported) {
    value.set_allow_pipewire(fields.allow_pipewire);
    value.set_width(fields.width); value.set_height(fields.height);
    value.set_pipewire_use_damage_region(fields.pipewire_use_damage_region);
  }
#endif
  return 0;
#else
  return 2;
#endif
}
std::int64_t desktop_options_time_us(const NativeDesktopOptions& options) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_options_time_us(options); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  return options.state()->value->clock().CurrentTime().us();
#else
  return 0;
#endif
}
bool desktop_options_clock_matches(const NativeDesktopOptions& options, const NativeClock& clock) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_options_clock_matches(options, clock); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  return &options.state()->value->clock() == clock.sdk_identity();
#else
  return false;
#endif
}
std::unique_ptr<NativeDesktopOptions> desktop_options_resource(const NativeDesktopOptions& options, std::uint8_t kind) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_options_resource(options, kind); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto state = std::make_unique<NativeDesktopOptions::State>();
  state->role = role; state->value.emplace();
  const auto& value = *options.state()->value;
#if defined(WEBRTC_USE_X11)
  if (kind == 0 && value.x_display()) { state->value->set_x_display(value.x_display()); return std::make_unique<NativeDesktopOptions>(std::move(state)); }
#endif
#if defined(WEBRTC_USE_PIPEWIRE)
  if (kind == 1 && value.screencast_stream()) { state->value->set_screencast_stream(value.screencast_stream()); return std::make_unique<NativeDesktopOptions>(std::move(state)); }
#endif
#endif
  return nullptr;
}
std::unique_ptr<NativeDesktopOptions> desktop_resource_create(const NativeDesktopOptions& options, std::uint8_t kind, rust::Str name) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_resource_create(options, kind, name); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto state = std::make_unique<NativeDesktopOptions::State>();
  state->role = role; state->value.emplace();
#if defined(WEBRTC_USE_X11)
  if (kind == 0) {
    const std::string display_name(name.data(), name.size());
    if (display_name.find('\0') != std::string::npos) return nullptr;
    auto display = webrtc::SharedXDisplay::Create(display_name);
    if (!display) return nullptr;
    state->value->set_x_display(std::move(display));
    return std::make_unique<NativeDesktopOptions>(std::move(state));
  }
#endif
#if defined(WEBRTC_USE_PIPEWIRE)
  if (kind == 1) {
    auto stream = webrtc::SharedScreenCastStream::CreateDefault();
    if (!stream) return nullptr;
    state->value->set_screencast_stream(std::move(stream));
    return std::make_unique<NativeDesktopOptions>(std::move(state));
  }
#endif
#endif
  return nullptr;
}
std::uint8_t desktop_options_set_resource(const NativeDesktopOptions& options, std::uint8_t kind, bool present, const NativeDesktopOptions& resource) noexcept {
  if (options.state()->role != resource.state()->role) return 3;
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_options_set_resource(options, kind, present, resource); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto& value = *options.state()->value;
#if defined(WEBRTC_USE_X11)
  if (kind == 0) { value.set_x_display(present ? resource.state()->value->x_display() : nullptr); return 0; }
#endif
#if defined(WEBRTC_USE_PIPEWIRE)
  if (kind == 1) { value.set_screencast_stream(present ? resource.state()->value->screencast_stream() : nullptr); return 0; }
#endif
#endif
  return 2;
}
bool desktop_resource_equal(const NativeDesktopOptions& a, const NativeDesktopOptions& b, std::uint8_t kind) noexcept {
  if (a.state()->role != b.state()->role) return false;
  auto* role = a.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return desktop_resource_equal(a, b, kind); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
#if defined(WEBRTC_USE_X11)
  if (kind == 0) return a.state()->value->x_display().get() == b.state()->value->x_display().get();
#endif
#if defined(WEBRTC_USE_PIPEWIRE)
  if (kind == 1) return a.state()->value->screencast_stream().get() == b.state()->value->screencast_stream().get();
#endif
#endif
  return false;
}

// The SDK portal uses GIO's thread-default context but supplies no pump.
// Use an isolated context on the existing SDK role: pumping the global GTK
// context here could dispatch unrelated UI callbacks on the wrong OS thread.
// No new executor or process-global binding registry is introduced. The SDK's
// PortalGuard gates late callbacks after retirement. The SDK does not expose
// cancellation-completion accounting; Stop drains ready work, not future I/O.
// See the documented native limitation for SDK-only pending GTask resources.
class CaptureGlibContext final : public std::enable_shared_from_this<CaptureGlibContext> {
 public:
  explicit CaptureGlibContext(webrtc::Thread* role) : role_(role) {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    context_ = g_main_context_new();
#endif
  }
  ~CaptureGlibContext() {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    g_main_context_unref(context_);
#endif
  }
  static std::shared_ptr<CaptureGlibContext> Create(webrtc::Thread* role) {
    auto result = std::make_shared<CaptureGlibContext>(role);
    result->Schedule();
    return result;
  }
  bool IsCurrent() const {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    return role_->IsCurrent() && g_main_context_is_owner(context_);
#else
    return false;
#endif
  }
  class Scope {
   public:
    explicit Scope(const CaptureGlibContext* owner) : owner_(owner) {
      (void)owner_;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
      if (owner_) g_main_context_push_thread_default(owner_->context_);
#endif
    }
    ~Scope() {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
      if (owner_) g_main_context_pop_thread_default(owner_->context_);
#endif
    }
   private:
    const CaptureGlibContext* owner_;
  };
  template <typename F> auto Call(F&& call) const -> std::invoke_result_t<F> {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    auto invoke = [&]() -> std::invoke_result_t<F> {
      Scope scope(this);
      return std::forward<F>(call)();
    };
    return role_->IsCurrent() ? invoke() : role_->BlockingCall(invoke);
#else
    return std::forward<F>(call)();
#endif
  }
  void Stop() {
    active_.store(false);
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    // Called on the role after SDK destruction invalidates PortalGuard and
    // joins stream callbacks. Ready cancellation/unsubscribe work is safe to
    // dispatch. An empty queue is NOT proof that all asynchronous I/O finished.
    while (g_main_context_iteration(context_, false)) {}
#endif
  }
 private:
  void Schedule() {
    (void)role_;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    std::weak_ptr<CaptureGlibContext> weak = shared_from_this();
    role_->PostDelayedTask([weak] {
      auto owner = weak.lock();
      if (!owner || !owner->active_.load()) return;
      // A bounded batch lets other SDK queue work run even when GIO stays ready.
      // This context contains SDK portal work only, never application GTK work.
      if (g_main_context_acquire(owner->context_)) {
        {
          Scope scope(owner.get());
          for (unsigned i = 0; i < 32 && owner->active_.load(); ++i)
            if (!g_main_context_iteration(owner->context_, false)) break;
        }
        g_main_context_release(owner->context_);
      }
      if (owner->active_.load()) owner->Schedule();
    }, webrtc::TimeDelta::Millis(2));
#endif
  }
  webrtc::Thread* role_;
  std::atomic<bool> active_{true};
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  GMainContext* context_;
#endif
};

struct NativeScreenCapture::State {
  webrtc::Thread* role = nullptr;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  std::optional<webrtc::DesktopCaptureOptions> options;
  std::shared_ptr<CaptureGlibContext> context;
  std::unique_ptr<webrtc::DesktopCapturer> capturer;
  std::unique_ptr<ScreenFrameSink> sink;
  std::unique_ptr<ScreenSelectionObserver> selection;
  bool observing_selection = false;
#endif
  std::atomic<std::uint8_t> status{0};
  std::uint64_t final_failed_frames = 0;
  std::atomic<bool> closed{false};
  ~State() {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    // Also covers failed construction after observer installation. The SDK
    // must retire borrowed sink/observer pointers before those members die.
    capturer.reset();
    if (context) context->Stop();
#endif
  }
};

struct NativeCameraOptions::State {
  webrtc::Thread* role = nullptr;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
  std::unique_ptr<webrtc::VideoCaptureOptions> value;
#endif
};
NativeCameraOptions::NativeCameraOptions(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeCameraOptions::~NativeCameraOptions() {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
  auto release = [&] { state_->value.reset(); };
  if (state_->role && !state_->role->IsCurrent()) state_->role->BlockingCall(release);
  else release();
#endif
}
const std::unique_ptr<NativeCameraOptions::State>& NativeCameraOptions::state() const noexcept { return state_; }
std::unique_ptr<NativeCameraOptions> camera_options_new(const NativePeerConnectionFactory& factory) noexcept {
  auto* role = factory.signaling_thread();
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_options_new(factory); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
  auto state = std::make_unique<NativeCameraOptions::State>();
  state->role = role;
  state->value = std::make_unique<webrtc::VideoCaptureOptions>();
  return std::make_unique<NativeCameraOptions>(std::move(state));
#else
  return nullptr;
#endif
}
std::unique_ptr<NativeCameraOptions> camera_options_clone(const NativeCameraOptions& options) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_options_clone(options); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
  auto state = std::make_unique<NativeCameraOptions::State>();
  state->role = role;
  state->value = std::make_unique<webrtc::VideoCaptureOptions>(*options.state()->value);
  return std::make_unique<NativeCameraOptions>(std::move(state));
#else
  return nullptr;
#endif
}
std::uint8_t camera_options_flags(const NativeCameraOptions& options) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_options_flags(options); });
  std::uint8_t flags = 0;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
  if (options.state()->value->allow_v4l2()) flags |= 1;
#if defined(WEBRTC_USE_PIPEWIRE)
  flags |= 2;
  if (options.state()->value->allow_pipewire()) flags |= 4;
#endif
#endif
  return flags;
}
bool camera_options_write(const NativeCameraOptions& options, bool v4l2, bool pipewire) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_options_write(options, v4l2, pipewire); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
#if !defined(WEBRTC_USE_PIPEWIRE)
  if (pipewire) return false;
#else
  options.state()->value->set_allow_pipewire(pipewire);
#endif
  options.state()->value->set_allow_v4l2(v4l2);
  return true;
#else
  return false;
#endif
}
bool camera_options_set_fd(const NativeCameraOptions& options, std::int32_t fd) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_options_set_fd(options, fd); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID) && defined(WEBRTC_USE_PIPEWIRE)
  options.state()->value->set_pipewire_fd(fd);
  return true;
#else
  return false;
#endif
}
std::int32_t camera_options_initialize(const NativeCameraOptions& options, const RustCameraInitializationObserver& observer) noexcept {
  auto* role = options.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_options_initialize(options, observer); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
#if defined(WEBRTC_USE_PIPEWIRE)
  // SDK Cleanup holds callback_lock_ while joining the loop. Finish on that
  // loop can wait on the same lock; no public quiescence fence repairs this.
  // Reject before Init creates any session, never substitute a V4L2 product.
  if (options.state()->value->allow_pipewire()) return -2;
#endif
  struct Callback final : webrtc::VideoCaptureOptions::Callback {
    explicit Callback(const RustCameraInitializationObserver& source) : source(source) {}
    void OnInitialized(webrtc::VideoCaptureOptions::Status status) override {
      result = static_cast<std::int32_t>(status);
      camera_options_initialized(source, result);
    }
    const RustCameraInitializationObserver& source;
    std::int32_t result = -1;
  } callback(observer);
  options.state()->value->Init(&callback);
  return callback.result;
#else
  return -1;
#endif
}

struct NativeCameraDeviceInfo::State {
  webrtc::Thread* role = nullptr;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  std::unique_ptr<webrtc::VideoCaptureModule::DeviceInfo> info;
#endif
};
NativeCameraDeviceInfo::NativeCameraDeviceInfo(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeCameraDeviceInfo::~NativeCameraDeviceInfo() { camera_info_close(*this); }
const std::unique_ptr<NativeCameraDeviceInfo::State>& NativeCameraDeviceInfo::state() const noexcept { return state_; }
void camera_info_close(const NativeCameraDeviceInfo& info) noexcept {
  auto* role = info.state()->role;
  if (role && !role->IsCurrent()) { role->BlockingCall([&] { camera_info_close(info); }); return; }
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  info.state()->info.reset();
#endif
}
std::unique_ptr<NativeCameraDeviceInfo> camera_info_new(const NativePeerConnectionFactory& factory) noexcept {
  auto* role = factory.signaling_thread();
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_info_new(factory); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto state = std::make_unique<NativeCameraDeviceInfo::State>();
  state->role = role;
  state->info.reset(webrtc::VideoCaptureFactory::CreateDeviceInfo());
  if (!state->info) return nullptr;
  return std::make_unique<NativeCameraDeviceInfo>(std::move(state));
#else
  return nullptr;
#endif
}
std::unique_ptr<NativeCameraDeviceInfo> camera_info_new_with_options(const NativePeerConnectionFactory& factory, const NativeCameraOptions& options) noexcept {
  auto* role = factory.signaling_thread();
  if (role != options.state()->role) return nullptr;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_info_new_with_options(factory, options); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
#if defined(WEBRTC_USE_PIPEWIRE)
  if (options.state()->value->allow_pipewire()) return nullptr;
#endif
  auto state = std::make_unique<NativeCameraDeviceInfo::State>();
  state->role = role;
  state->info.reset(webrtc::VideoCaptureFactory::CreateDeviceInfo(options.state()->value.get()));
  if (!state->info) return nullptr;
  return std::make_unique<NativeCameraDeviceInfo>(std::move(state));
#else
  return nullptr;
#endif
}
FfiCameraInfoReply camera_info_query(const NativeCameraDeviceInfo& info, const FfiCameraInfoRequest& request) noexcept {
  auto* role = info.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_info_query(info, request); });
  FfiCameraInfoReply reply{};
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto* actual = info.state()->info.get();
  if (!actual) { reply.status = 1; return reply; }
  const std::string id(request.id.data(), request.id.size()), title(request.title.data(), request.title.size());
  if (id.find('\0') != std::string::npos || title.find('\0') != std::string::npos) { reply.status = 3; return reply; }
  webrtc::VideoCaptureCapability cap{};
  switch (request.kind) {
    case 0: reply.count = actual->NumberOfDevices(); break;
    case 1: {
      std::vector<char> name(request.name_capacity), unique(request.unique_capacity), product(request.product_present ? request.product_capacity : 0);
      char empty = 0;
      reply.native_result = actual->GetDeviceName(request.index,
          name.empty() ? &empty : name.data(), request.name_capacity,
          unique.empty() ? &empty : unique.data(), request.unique_capacity,
          request.product_present ? (product.empty() ? &empty : product.data()) : nullptr,
          request.product_present ? request.product_capacity : 0);
      auto copy = [](const std::vector<char>& input, rust::Vec<std::uint8_t>& output) {
        for (auto byte : input) { if (byte == 0) break; output.push_back(static_cast<std::uint8_t>(byte)); }
      };
      copy(name, reply.name); copy(unique, reply.unique_id); copy(product, reply.product_id);
      break;
    }
    case 2: {
      const auto count = actual->NumberOfCapabilities(id.c_str());
      if (count < 0) reply.native_result = count; else reply.count = static_cast<std::uint32_t>(count);
      break;
    }
    case 3: reply.native_result = actual->GetCapability(id.c_str(), request.index, cap); break;
    case 4: {
      auto rotation = webrtc::kVideoRotation_0;
      reply.native_result = actual->GetOrientation(id.c_str(), rotation);
      reply.rotation = static_cast<std::uint16_t>(rotation);
      break;
    }
    case 5: {
      const auto& s = request.settings;
      if (s.width < 0 || s.height < 0 || s.max_fps < 0 ||
          s.width > INT32_MAX / 4 || (s.height != 0 && s.width > INT32_MAX / s.height / 4)) {
        reply.status = 3; return reply;
      }
      webrtc::VideoCaptureCapability wanted{};
      wanted.width = s.width; wanted.height = s.height; wanted.maxFPS = s.max_fps;
      wanted.videoType = static_cast<webrtc::VideoType>(s.pixel_format); wanted.interlaced = s.interlaced;
      const auto index = actual->GetBestMatchedCapability(id.c_str(), wanted, cap);
      if (index < 0) reply.native_result = index; else reply.count = static_cast<std::uint32_t>(index);
      break;
    }
    case 6: reply.native_result = actual->DisplayCaptureSettingsDialogBox(id.c_str(), title.c_str(), nullptr, request.x, request.y); break;
    default: reply.status = 3; break;
  }
  reply.settings.width = cap.width; reply.settings.height = cap.height;
  reply.settings.max_fps = cap.maxFPS; reply.settings.pixel_format = static_cast<std::int32_t>(cap.videoType);
  reply.settings.interlaced = cap.interlaced;
#else
  reply.status = 2;
#endif
  return reply;
}

struct NativeCamera::State {
  webrtc::Thread* role = nullptr;
  bool device_stopped = false;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  webrtc::scoped_refptr<webrtc::VideoCaptureModule> module;
  std::unique_ptr<CameraFrameSink> sink;
  std::unique_ptr<RawCameraFrameSink> raw_sink;
  std::atomic<std::int64_t> last_frame_us{0};
  std::atomic<bool> frame_seen{false};
#endif
  std::atomic<bool> closed{false};
};

struct NativeVideoSource::State {
  webrtc::scoped_refptr<PushVideoSource> source;
  webrtc::Thread* signaling_thread = nullptr;
  webrtc::Thread* worker_thread = nullptr;
  std::atomic<bool> closed{false};
};

struct NativeVideoTrack::State {
  webrtc::scoped_refptr<webrtc::VideoTrackInterface> track;
};

struct NativeVideoSourceView::State {
  webrtc::scoped_refptr<webrtc::VideoTrackSourceInterface> source;
  webrtc::scoped_refptr<PushVideoSource> producer;
  webrtc::VideoBroadcaster* remote_broadcaster = nullptr;
  webrtc::Thread* signaling = nullptr;
  webrtc::Thread* worker = nullptr;
};
struct NativeVideoObserver::State final : webrtc::ObserverInterface {
  webrtc::scoped_refptr<webrtc::VideoTrackSourceInterface> source;
  webrtc::scoped_refptr<webrtc::VideoTrackInterface> track;
  webrtc::Thread* signaling = nullptr;
  bool registered = false;  // signaling only
  std::mutex mutex;
  std::size_t capacity = 0;
  std::size_t pending = 0;
  std::uint64_t dropped = 0;
  webrtc::NotifierInterface* notifier() const {
    return source ? static_cast<webrtc::NotifierInterface*>(source.get())
                  : static_cast<webrtc::NotifierInterface*>(track.get());
  }
  void OnChanged() override {
    std::lock_guard lock(mutex);
    if (pending < capacity) ++pending;
    else if (dropped != std::numeric_limits<std::uint64_t>::max()) ++dropped;
  }
};
NativeVideoSourceView::NativeVideoSourceView(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeVideoSourceView::~NativeVideoSourceView() {
  state_->signaling->BlockingCall([&] {
    state_->producer = nullptr;
    state_->source = nullptr;
  });
}
const std::unique_ptr<NativeVideoSourceView::State>& NativeVideoSourceView::state() const noexcept { return state_; }
std::uint64_t video_source_rejected_sink_requests(const NativeVideoSource& source) noexcept {
  return source.state()->source->broadcaster().RejectedRequests();
}
std::unique_ptr<NativeVideoSourceView> video_source_as_view(const NativeVideoSource& source) noexcept {
  auto state = std::make_unique<NativeVideoSourceView::State>();
  state->signaling = source.state()->signaling_thread;
  state->worker = source.state()->worker_thread;
  state->signaling->BlockingCall([&] {
    state->source = source.state()->source;
    state->producer = source.state()->source;
  });
  return std::make_unique<NativeVideoSourceView>(std::move(state));
}
std::unique_ptr<NativeVideoSourceView> video_track_source(const NativeVideoTrack& track, const NativePeerConnectionFactory& factory) noexcept {
  auto state = std::make_unique<NativeVideoSourceView::State>();
  state->signaling = factory.signaling_thread();
  state->worker = factory.worker_thread();
  state->signaling->BlockingCall([&] {
    state->source = track.state()->track->GetSource();
    if (!state->source) return;
    // Closed safe-Rust construction: all tracks are SDK VideoTrack objects,
    // whose GetSource returns this exact proxy specialization. Its immutable
    // internal source is either our Push source or SDK VideoRtpTrackSource.
    // No user SourceInterface/TrackInterface injection exists in this graph.
    auto* internal = static_cast<webrtc::VideoTrackSourceProxyWithInternal<
        webrtc::VideoTrackSourceInterface>*>(state->source.get())->internal();
    if (internal->remote()) {
      state->remote_broadcaster = static_cast<webrtc::VideoBroadcaster*>(
          static_cast<webrtc::VideoRtpTrackSource*>(internal)->source());
    } else {
      state->producer = static_cast<PushVideoSource*>(internal);
    }
  });
  if (!state->source) return nullptr;
  return std::make_unique<NativeVideoSourceView>(std::move(state));
}
std::unique_ptr<NativeVideoSourceView> video_track_source_from_producer(
    const NativeVideoTrack& track, const NativePeerConnectionFactory& factory,
    const NativeVideoSource& source) noexcept {
  auto view = video_track_source(track, factory);
  if (view && view->state()->producer.get() != source.state()->source.get()) return nullptr;
  return view;
}
bool video_source_view_rejected_sink_requests(
    const NativeVideoSourceView& source, std::uint64_t& count) noexcept {
  if (!source.state()->producer) return false;
  count = source.state()->producer->broadcaster().RejectedRequests();
  return true;
}
struct NativeVideoSourceEncodedSink::State {
  webrtc::scoped_refptr<webrtc::VideoTrackSourceInterface> source;
  webrtc::Thread* worker = nullptr;
  SourceEncodedFrameSink sink;
  bool closed = false;
};
NativeVideoSourceEncodedSink::NativeVideoSourceEncodedSink(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeVideoSourceEncodedSink::~NativeVideoSourceEncodedSink() { video_source_encoded_close(*this); }
const std::unique_ptr<NativeVideoSourceEncodedSink::State>&
NativeVideoSourceEncodedSink::state() const noexcept { return state_; }
std::unique_ptr<NativeVideoSourceEncodedSink> video_source_attach_encoded_output(
    const NativeVideoSourceView& source) noexcept {
  if (!source.state()->signaling->BlockingCall([&] {
        return source.state()->source->SupportsEncodedOutput();
      })) return nullptr;
  auto state = std::make_unique<NativeVideoSourceEncodedSink::State>();
  state->source = source.state()->source;
  state->worker = source.state()->worker;
  state->worker->BlockingCall([&] { state->source->AddEncodedSink(&state->sink); });
  return std::make_unique<NativeVideoSourceEncodedSink>(std::move(state));
}
FfiRecordableVideoFrame video_source_encoded_take_frame(const NativeVideoSourceEncodedSink& sink) noexcept {
  return sink.state()->sink.Take();
}
FfiVideoSinkEvent video_source_encoded_take_event(const NativeVideoSourceEncodedSink& sink) noexcept {
  return sink.state()->sink.TakeEvent();
}
std::uint64_t video_source_encoded_dropped_frames(const NativeVideoSourceEncodedSink& sink) noexcept {
  return sink.state()->sink.DroppedFrames();
}
std::uint64_t video_source_encoded_dropped_events(const NativeVideoSourceEncodedSink& sink) noexcept {
  return sink.state()->sink.DroppedEvents();
}
void video_source_encoded_close(const NativeVideoSourceEncodedSink& sink) noexcept {
  auto& state = *sink.state();
  if (state.closed) return;
  state.closed = true;
  state.sink.Deactivate();
  state.worker->BlockingCall([&] { state.source->RemoveEncodedSink(&state.sink); });
}

void video_source_view_request_refresh(const NativeVideoSourceView& view) noexcept {
  RequestVideoSourceRefresh(*view.state()->source, *view.state()->worker);
}
void video_source_view_generate_key_frame(const NativeVideoSourceView& view) noexcept {
  GenerateVideoSourceKeyFrame(*view.state()->source, *view.state()->worker);
}
void video_source_view_process_constraints(const NativeVideoSourceView& view,
    bool has_min, double min, bool has_max, double max) noexcept {
  webrtc::VideoTrackSourceConstraints constraints;
  if (has_min) constraints.min_fps = min;
  if (has_max) constraints.max_fps = max;
  ProcessVideoSourceConstraints(*view.state()->source, *view.state()->worker, constraints);
}
void video_track_request_refresh(const NativeVideoTrack& track) noexcept {
  track.state()->track->RequestRefreshFrame();
}
bool video_source_views_same_interface(const NativeVideoSourceView& a, const NativeVideoSourceView& b) noexcept {
  return a.state()->source.get() == b.state()->source.get();
}
std::uint8_t video_source_view_state(const NativeVideoSourceView& source) noexcept {
  return source.state()->signaling->BlockingCall([&] { return static_cast<std::uint8_t>(source.state()->source->state()); });
}
bool video_source_view_remote(const NativeVideoSourceView& source) noexcept {
  return source.state()->signaling->BlockingCall([&] { return source.state()->source->remote(); });
}
bool video_source_view_screencast(const NativeVideoSourceView& source) noexcept {
  return source.state()->signaling->BlockingCall([&] { return source.state()->source->is_screencast(); });
}
std::int8_t video_source_view_denoising(const NativeVideoSourceView& source) noexcept {
  return source.state()->signaling->BlockingCall([&]() -> std::int8_t {
    const auto value = source.state()->source->needs_denoising();
    return value ? static_cast<std::int8_t>(*value) : -1;
  });
}
bool video_source_view_encoded_output(const NativeVideoSourceView& source) noexcept {
  return source.state()->signaling->BlockingCall([&] { return source.state()->source->SupportsEncodedOutput(); });
}
FfiVideoSourceStats video_source_view_stats(const NativeVideoSourceView& source) noexcept {
  return source.state()->signaling->BlockingCall([&] {
    FfiVideoSourceStats out;
    webrtc::VideoTrackSourceInterface::Stats stats{};
    if (source.state()->source->GetStats(&stats)) {
      out.available = true;
      out.input_width = stats.input_width;
      out.input_height = stats.input_height;
    }
    return out;
  });
}
NativeVideoObserver::NativeVideoObserver(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeVideoObserver::~NativeVideoObserver() {
  video_observer_close(*this);
  state_->signaling->BlockingCall([&] { state_->source = nullptr; state_->track = nullptr; });
}
const std::unique_ptr<NativeVideoObserver::State>& NativeVideoObserver::state() const noexcept { return state_; }
std::unique_ptr<NativeVideoObserver> video_source_view_observe(const NativeVideoSourceView& source, std::size_t capacity) noexcept {
  if (!capacity) return nullptr;
  auto state = std::make_unique<NativeVideoObserver::State>();
  state->signaling = source.state()->signaling;
  state->capacity = capacity;
  state->signaling->BlockingCall([&] {
    state->source = source.state()->source;
    state->notifier()->RegisterObserver(state.get());
    state->registered = true;
  });
  return std::make_unique<NativeVideoObserver>(std::move(state));
}
std::unique_ptr<NativeVideoObserver> video_track_observe(const NativeVideoTrack& track, const NativePeerConnectionFactory& factory, std::size_t capacity) noexcept {
  if (!capacity) return nullptr;
  auto state = std::make_unique<NativeVideoObserver::State>();
  state->signaling = factory.signaling_thread();
  state->capacity = capacity;
  state->signaling->BlockingCall([&] {
    state->track = track.state()->track;
    state->notifier()->RegisterObserver(state.get());
    state->registered = true;
  });
  return std::make_unique<NativeVideoObserver>(std::move(state));
}
bool video_observer_take(const NativeVideoObserver& observer) noexcept {
  auto& state = *observer.state();
  std::lock_guard lock(state.mutex);
  if (!state.pending) return false;
  --state.pending;
  return true;
}
std::uint64_t video_observer_take_dropped(const NativeVideoObserver& observer) noexcept {
  auto& state = *observer.state();
  std::lock_guard lock(state.mutex);
  return std::exchange(state.dropped, 0);
}
void video_observer_close(const NativeVideoObserver& observer) noexcept {
  auto& state = *observer.state();
  state.signaling->BlockingCall([&] {
    if (!state.registered) return;
    state.notifier()->UnregisterObserver(&state);
    state.registered = false;
  });
}

struct NativeLocalMediaStream::State {
  webrtc::scoped_refptr<webrtc::MediaStreamInterface> stream;
  webrtc::Thread* signaling = nullptr;
};
struct NativeLocalMediaStreamObserver::State final : webrtc::ObserverInterface {
  webrtc::scoped_refptr<webrtc::MediaStreamInterface> stream;
  webrtc::Thread* signaling = nullptr;
  bool registered = false;  // signaling only
  std::mutex mutex;
  std::size_t capacity = 0;
  std::size_t pending = 0;
  std::uint64_t dropped = 0;
  void OnChanged() override {
    std::lock_guard lock(mutex);
    if (pending < capacity) ++pending;
    else if (dropped != UINT64_MAX) ++dropped;
  }
};
NativeLocalMediaStream::NativeLocalMediaStream(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeLocalMediaStream::~NativeLocalMediaStream() {
  state_->signaling->BlockingCall([&] { state_->stream = nullptr; });
}
const std::unique_ptr<NativeLocalMediaStream::State>& NativeLocalMediaStream::state() const noexcept { return state_; }
std::unique_ptr<NativeLocalMediaStream> create_media_stream(const NativePeerConnectionFactory& factory, rust::Str id) noexcept {
  auto state = std::make_unique<NativeLocalMediaStream::State>();
  state->signaling = factory.signaling_thread();
  if (!state->signaling) return nullptr;
  state->signaling->BlockingCall([&] { state->stream = factory.factory()->CreateLocalMediaStream(std::string(id)); });
  if (!state->stream) return nullptr;
  return std::make_unique<NativeLocalMediaStream>(std::move(state));
}
rust::String local_media_stream_id(const NativeLocalMediaStream& stream) noexcept {
  return stream.state()->signaling->BlockingCall([&] { return rust::String(stream.state()->stream->id()); });
}
rust::Vec<std::size_t> local_media_stream_track_keys(const NativeLocalMediaStream& stream, bool audio) noexcept {
  return stream.state()->signaling->BlockingCall([&] {
    rust::Vec<std::size_t> keys;
    if (audio) {
      for (const auto& track : stream.state()->stream->GetAudioTracks()) keys.push_back(reinterpret_cast<std::size_t>(track.get()));
    } else {
      for (const auto& track : stream.state()->stream->GetVideoTracks()) keys.push_back(reinterpret_cast<std::size_t>(track.get()));
    }
    return keys;
  });
}
std::size_t local_media_stream_find_track(const NativeLocalMediaStream& stream, bool audio, rust::Str id) noexcept {
  return stream.state()->signaling->BlockingCall([&] {
    const auto name = std::string(id);
    return audio ? reinterpret_cast<std::size_t>(stream.state()->stream->FindAudioTrack(name).get())
                 : reinterpret_cast<std::size_t>(stream.state()->stream->FindVideoTrack(name).get());
  });
}
std::size_t video_track_identity(const NativeVideoTrack& track) noexcept {
  return reinterpret_cast<std::size_t>(track.state()->track.get());
}
bool local_media_stream_audio_track(const NativeLocalMediaStream& stream, const NativeAudioTrack& track, bool add) noexcept {
  return stream.state()->signaling->BlockingCall([&] {
    webrtc::scoped_refptr<webrtc::AudioTrackInterface> value(audio_track_sdk_handle(track));
    return add ? stream.state()->stream->AddTrack(value) : stream.state()->stream->RemoveTrack(value);
  });
}
bool local_media_stream_video_track(const NativeLocalMediaStream& stream, const NativeVideoTrack& track, bool add) noexcept {
  return stream.state()->signaling->BlockingCall([&] {
    return add ? stream.state()->stream->AddTrack(track.state()->track) : stream.state()->stream->RemoveTrack(track.state()->track);
  });
}
NativeLocalMediaStreamObserver::NativeLocalMediaStreamObserver(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeLocalMediaStreamObserver::~NativeLocalMediaStreamObserver() {
  local_media_stream_observer_close(*this);
  state_->signaling->BlockingCall([&] { state_->stream = nullptr; });
}
const std::unique_ptr<NativeLocalMediaStreamObserver::State>& NativeLocalMediaStreamObserver::state() const noexcept { return state_; }
std::unique_ptr<NativeLocalMediaStreamObserver> local_media_stream_observe(const NativeLocalMediaStream& stream, std::size_t capacity) noexcept {
  if (!capacity) return nullptr;
  auto state = std::make_unique<NativeLocalMediaStreamObserver::State>();
  state->signaling = stream.state()->signaling;
  state->capacity = capacity;
  state->signaling->BlockingCall([&] {
    state->stream = stream.state()->stream;
    state->stream->RegisterObserver(state.get());
    state->registered = true;
  });
  return std::make_unique<NativeLocalMediaStreamObserver>(std::move(state));
}
bool local_media_stream_observer_take(const NativeLocalMediaStreamObserver& observer) noexcept {
  auto& state = *observer.state();
  std::lock_guard lock(state.mutex);
  if (!state.pending) return false;
  --state.pending;
  return true;
}
std::uint64_t local_media_stream_observer_take_dropped(const NativeLocalMediaStreamObserver& observer) noexcept {
  auto& state = *observer.state();
  std::lock_guard lock(state.mutex);
  return std::exchange(state.dropped, 0);
}
void local_media_stream_observer_close(const NativeLocalMediaStreamObserver& observer) noexcept {
  auto& state = *observer.state();
  state.signaling->BlockingCall([&] {
    if (!state.registered) return;
    state.stream->UnregisterObserver(&state);
    state.registered = false;
  });
}

struct NativeEncodedVideoSink::State {
  webrtc::scoped_refptr<EncodedFrameCollector> collector;
  webrtc::scoped_refptr<webrtc::VideoTrackSourceInterface> source;
  webrtc::Thread* worker = nullptr;
  std::atomic<bool> closed{false};
};

struct NativeVideoSink::State {
  webrtc::scoped_refptr<webrtc::VideoTrackSourceInterface> source;
  webrtc::scoped_refptr<PushVideoSource> producer;
  webrtc::VideoBroadcaster* remote_broadcaster = nullptr;
  webrtc::Thread* worker = nullptr;
  webrtc::scoped_refptr<webrtc::VideoTrackInterface> track;
  std::unique_ptr<FrameSink> sink;
  std::atomic<bool> closed{false};
};

struct NativeRtpSender::State {
  webrtc::scoped_refptr<webrtc::RtpSenderInterface> sender;
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> owner;
  std::optional<webrtc::RtpParameters> parameters;
};


struct RtpPacketQueue {
  std::mutex mutex;
  std::size_t capacity = 0;
  std::deque<FfiRtpPacketEvent> events;
  std::uint64_t dropped = 0;
  void Reset(std::size_t size) { std::lock_guard lock(mutex); capacity = size; events.clear(); dropped = 0; }
  void Push(std::uint8_t event_type, webrtc::MediaType kind) {
    std::lock_guard lock(mutex);
    if (!capacity) return;
    if (events.size() == capacity) { events.pop_front(); if (dropped != std::numeric_limits<std::uint64_t>::max()) ++dropped; }
    FfiRtpPacketEvent event{}; event.present = true; event.event_type = event_type; event.media_type = static_cast<std::int32_t>(kind);
    events.push_back(event);
  }
  FfiRtpPacketEvent Take() { std::lock_guard lock(mutex); if (events.empty()) return {}; auto event = events.front(); events.pop_front(); return event; }
  std::uint64_t TakeDropped() { std::lock_guard lock(mutex); return std::exchange(dropped, 0); }
};
struct NativeRtpSenderObserver::State final : webrtc::RtpSenderObserverInterface {
  webrtc::scoped_refptr<webrtc::RtpSenderInterface> resource;
  bool registered = false;
  RtpPacketQueue queue;
  ~State() override = default;
  void OnFirstPacketSent(webrtc::MediaType kind) override { queue.Push(0, kind); }
};
NativeRtpSenderObserver::NativeRtpSenderObserver(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeRtpSenderObserver::~NativeRtpSenderObserver() {
  // Serialize removal via the stock signaling proxy before observer/vtable
  // destruction begins. The Rust owner still retains its peer/factory lease.
  if (state_->registered) state_->resource->SetObserver(nullptr);
}
const std::unique_ptr<NativeRtpSenderObserver::State>& NativeRtpSenderObserver::state() const noexcept { return state_; }
std::unique_ptr<NativeRtpSenderObserver> rtp_sender_observer(const NativeRtpSender& value) noexcept {
  auto state = std::make_unique<NativeRtpSenderObserver::State>(); state->resource = value.sender();
  return std::make_unique<NativeRtpSenderObserver>(std::move(state));
}
std::size_t rtp_sender_observer_identity(const NativeRtpSenderObserver& value) noexcept { return reinterpret_cast<std::uintptr_t>(value.state()->resource.get()); }
bool rtp_sender_observer_register(const NativeRtpSenderObserver& value, std::size_t capacity) noexcept {
  auto& state = *value.state(); if (!capacity || state.registered) return false;
  state.queue.Reset(capacity); state.registered = true;
  // Late registration can synchronously replay; never hold the queue mutex.
  state.resource->SetObserver(&state); return true;
}
void rtp_sender_observer_unregister(const NativeRtpSenderObserver& value) noexcept {
  auto& state = *value.state(); if (state.registered) { state.resource->SetObserver(nullptr); state.registered = false; }
}
FfiRtpPacketEvent rtp_sender_observer_take(const NativeRtpSenderObserver& value) noexcept { return value.state()->queue.Take(); }
std::uint64_t rtp_sender_observer_take_dropped(const NativeRtpSenderObserver& value) noexcept { return value.state()->queue.TakeDropped(); }
struct NativeRtpReceiverObserver::State final : webrtc::RtpReceiverObserverInterface {
  webrtc::scoped_refptr<webrtc::RtpReceiverInterface> resource;
  bool registered = false;
  RtpPacketQueue queue;
  ~State() override = default;
  void OnFirstPacketReceived(webrtc::MediaType kind) override { queue.Push(1, kind); }
  void OnFirstPacketReceivedAfterReceptiveChange(webrtc::MediaType kind) override { queue.Push(2, kind); }
};
NativeRtpReceiverObserver::NativeRtpReceiverObserver(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeRtpReceiverObserver::~NativeRtpReceiverObserver() {
  // Serialize removal via the stock signaling proxy before observer/vtable
  // destruction begins. The Rust owner still retains its peer/factory lease.
  if (state_->registered) state_->resource->SetObserver(nullptr);
}
const std::unique_ptr<NativeRtpReceiverObserver::State>& NativeRtpReceiverObserver::state() const noexcept { return state_; }
std::unique_ptr<NativeRtpReceiverObserver> rtp_receiver_observer(const NativeRtpReceiver& value) noexcept {
  auto state = std::make_unique<NativeRtpReceiverObserver::State>(); state->resource = value.receiver();
  return std::make_unique<NativeRtpReceiverObserver>(std::move(state));
}
std::size_t rtp_receiver_observer_identity(const NativeRtpReceiverObserver& value) noexcept { return reinterpret_cast<std::uintptr_t>(value.state()->resource.get()); }
bool rtp_receiver_observer_register(const NativeRtpReceiverObserver& value, std::size_t capacity) noexcept {
  auto& state = *value.state(); if (!capacity || state.registered) return false;
  state.queue.Reset(capacity); state.registered = true;
  // Late registration can synchronously replay; never hold the queue mutex.
  state.resource->SetObserver(&state); return true;
}
void rtp_receiver_observer_unregister(const NativeRtpReceiverObserver& value) noexcept {
  auto& state = *value.state(); if (state.registered) { state.resource->SetObserver(nullptr); state.registered = false; }
}
FfiRtpPacketEvent rtp_receiver_observer_take(const NativeRtpReceiverObserver& value) noexcept { return value.state()->queue.Take(); }
std::uint64_t rtp_receiver_observer_take_dropped(const NativeRtpReceiverObserver& value) noexcept { return value.state()->queue.TakeDropped(); }

struct NativeDtmfSender::State final : webrtc::DtmfSenderObserverInterface {
  // GetDtmfSender returns the signaling-thread proxy. Every method below,
  // including unregister during destruction, is marshaled by that proxy.
  webrtc::scoped_refptr<webrtc::DtmfSenderInterface> sender;
  std::shared_ptr<ReadinessSignal> readiness;
  bool registered = false;
  std::mutex mutex;
  std::size_t capacity = 0;
  std::deque<FfiDtmfToneChange> events;
  std::uint64_t dropped = 0;

  ~State() override = default;

  void OnToneChange(const std::string& tone, const std::string& buffer) override {
    Push(tone, true, buffer);
  }
  void OnToneChange(const std::string& tone) override {
    Push(tone, false, {});
  }
  void Push(const std::string& tone, bool has_buffer, const std::string& buffer) {
    // Declare before the lock so the native callback wakes only after unlocking.
    struct Notify { std::shared_ptr<ReadinessSignal> signal;
      ~Notify() { SignalReadiness(signal); } } notify{readiness};
    std::lock_guard<std::mutex> lock(mutex);
    if (!capacity) return;
    if (events.size() == capacity) {
      events.pop_front();
      if (dropped != std::numeric_limits<std::uint64_t>::max()) ++dropped;
    }
    FfiDtmfToneChange event;
    event.present = true;
    event.tone = rust::String(tone);
    event.has_remaining_tones = has_buffer;
    event.remaining_tones = rust::String(buffer);
    events.push_back(std::move(event));
  }
};

struct NativeRtpReceiver::State {
  webrtc::scoped_refptr<webrtc::RtpReceiverInterface> receiver;
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> owner;
};

struct NativeRtpTransceiver::State {
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> transceiver;
};

struct NativeTransceiverList::State {
  std::vector<webrtc::scoped_refptr<webrtc::RtpTransceiverInterface>> transceivers;
};

NativeScreenCapture::NativeScreenCapture(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeScreenCapture::~NativeScreenCapture() { close_screen(*this); }
const std::unique_ptr<NativeScreenCapture::State>&
NativeScreenCapture::state() const noexcept { return state_; }

NativeCamera::NativeCamera(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeCamera::~NativeCamera() { close_camera(*this); }
const std::unique_ptr<NativeCamera::State>& NativeCamera::state() const noexcept {
  return state_;
}

NativeVideoSource::NativeVideoSource(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeVideoSource::~NativeVideoSource() {
  close_video_source(*this);
  if (state_ && state_->source && state_->signaling_thread) {
    state_->signaling_thread->BlockingCall([this] { state_->source = nullptr; });
  }
}
const std::unique_ptr<NativeVideoSource::State>&
NativeVideoSource::state() const noexcept {
  return state_;
}

NativeVideoTrack::NativeVideoTrack(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeVideoTrack::~NativeVideoTrack() = default;
const std::unique_ptr<NativeVideoTrack::State>&
NativeVideoTrack::state() const noexcept {
  return state_;
}

NativeEncodedVideoSink::NativeEncodedVideoSink(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeEncodedVideoSink::~NativeEncodedVideoSink() {
  close_encoded_video_sink(*this);
}
const std::unique_ptr<NativeEncodedVideoSink::State>&
NativeEncodedVideoSink::state() const noexcept {
  return state_;
}

NativeVideoSink::NativeVideoSink(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeVideoSink::~NativeVideoSink() { close_video_sink(*this); }
const std::unique_ptr<NativeVideoSink::State>&
NativeVideoSink::state() const noexcept {
  return state_;
}

NativeRtpSender::NativeRtpSender(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeRtpSender::~NativeRtpSender() = default;
webrtc::scoped_refptr<webrtc::RtpSenderInterface> NativeRtpSender::sender() const noexcept { return state_->sender; }
webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> NativeRtpSender::sframe_owner() const noexcept { return state_->owner; }
const std::unique_ptr<NativeRtpSender::State>&
NativeRtpSender::state() const noexcept {
  return state_;
}

NativeDtmfSender::NativeDtmfSender(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeDtmfSender::~NativeDtmfSender() {
  // Retire registration via the signaling proxy before the observer object's
  // destructor begins, including any vtable teardown concurrent with callbacks.
  if (state_->registered) {
    state_->sender->UnregisterObserver();
    state_->registered = false;
  }
}
const std::unique_ptr<NativeDtmfSender::State>&
NativeDtmfSender::state() const noexcept { return state_; }

NativeRtpReceiver::NativeRtpReceiver(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeRtpReceiver::~NativeRtpReceiver() = default;
const std::unique_ptr<NativeRtpReceiver::State>&
NativeRtpReceiver::state() const noexcept {
  return state_;
}
webrtc::scoped_refptr<webrtc::RtpReceiverInterface>
NativeRtpReceiver::receiver() const noexcept { return state_->receiver; }
webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> NativeRtpReceiver::sframe_owner() const noexcept { return state_->owner; }

NativeRtpTransceiver::NativeRtpTransceiver(
    std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeRtpTransceiver::~NativeRtpTransceiver() = default;
const std::unique_ptr<NativeRtpTransceiver::State>&
NativeRtpTransceiver::state() const noexcept {
  return state_;
}

bool screen_sources(bool windows, rust::Vec<FfiScreenSource>& screens,
                    rust::String& error) noexcept {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto options = webrtc::DesktopCaptureOptions::CreateDefault();
  auto capturer = windows
      ? webrtc::DesktopCapturer::CreateWindowCapturer(options)
      : webrtc::DesktopCapturer::CreateScreenCapturer(options);
  if (!capturer) { error = "desktop capture unavailable on this platform"; return false; }
  webrtc::DesktopCapturer::SourceList sources;
  if (!capturer->GetSourceList(&sources)) {
    error = "screen source enumeration failed";
    return false;
  }
  for (const auto& source : sources) {
    FfiScreenSource screen{};
    screen.id = static_cast<std::int64_t>(source.id);
    screen.name = source.title;
    screen.display_id = source.display_id;
    screens.push_back(std::move(screen));
  }
  return true;
#else
  error = "screen capture requires a native artifact";
  return false;
#endif
}
static std::unique_ptr<NativeScreenCapture> create_desktop_capture_impl(
    const NativeVideoSource& source, const NativeDesktopOptions& options,
    std::uint8_t kind, bool select, std::int64_t id,
    std::shared_ptr<CaptureGlibContext> context,
    rust::Box<RustDesktopCaptureReceiver>& receiver,
    rust::Box<RustDesktopSelectionObserver>& observer, rust::String& error) noexcept {
  auto* role = source.state()->signaling_thread;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (source.state()->closed.load() || !source.state()->source) { error = "desktop source is closed"; return nullptr; }
  const auto& value = *options.state()->value;
#if defined(WEBRTC_USE_PIPEWIRE)
  const auto width = value.get_width(), height = value.get_height();
  if (width > INT32_MAX / 4 || height > INT32_MAX ||
      (height != 0 && width > INT32_MAX / height / 4)) {
    error = "desktop dimensions exceed native signed image arithmetic"; return nullptr;
  }
  if (value.allow_pipewire() && webrtc::BaseCapturerPipeWire::IsSupported() && !value.screencast_stream()) {
    error = "PipeWire requires a retained screencast stream"; return nullptr;
  }
#endif
  std::unique_ptr<webrtc::DesktopCapturer> capturer;
  if (kind == 0) capturer = webrtc::DesktopCapturer::CreateScreenCapturer(value);
  else if (kind == 1) capturer = webrtc::DesktopCapturer::CreateWindowCapturer(value);
  else if (kind == 2) capturer = webrtc::DesktopCapturer::CreateGenericCapturer(value);
  if (!capturer) { error = "selected SDK desktop factory is unavailable"; return nullptr; }
  auto state = std::make_unique<NativeScreenCapture::State>();
  state->role = role; state->options = value; state->context = std::move(context);
  state->capturer = std::move(capturer);
  state->sink = std::make_unique<ScreenFrameSink>(source.state()->source, source.state()->closed, state->status, &state->options->clock());
  state->sink->SetReceiver(std::move(receiver));
  if (auto* delegated = state->capturer->GetDelegatedSourceListController()) {
    state->selection = std::make_unique<ScreenSelectionObserver>(state->status);
    state->selection->SetObserver(std::move(observer));
    delegated->Observe(state->selection.get());
    state->observing_selection = true;
  } else {
    if (desktop_selection_present(*observer)) { error = "selected SDK capturer has no delegated selection controller"; return nullptr; }
    state->status.store(1);
  }
  if (select && !state->capturer->SelectSource(static_cast<webrtc::DesktopCapturer::SourceId>(id))) {
    error = "SDK rejected the selected desktop source"; return nullptr;
  }
  state->capturer->Start(state->sink.get());
  return std::make_unique<NativeScreenCapture>(std::move(state));
#else
  error = "desktop capture requires a native artifact"; return nullptr;
#endif
}

std::unique_ptr<NativeScreenCapture> create_desktop_capture(
    const NativeVideoSource& source, const NativeDesktopOptions& options,
    std::uint8_t kind, bool select, std::int64_t id,
    rust::Box<RustDesktopCaptureReceiver> receiver,
    rust::Box<RustDesktopSelectionObserver> observer, rust::String& error) noexcept {
  auto* role = source.state()->signaling_thread;
  if (role != options.state()->role) { error = "desktop options belong to another SDK role"; return nullptr; }
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (options.state()->value->allow_pipewire() && webrtc::BaseCapturerPipeWire::IsSupported()) {
    if (!role) { error = "Wayland progress requires a retained SDK role"; return nullptr; }
    auto context = CaptureGlibContext::Create(role);
    return context->Call([&] { return create_desktop_capture_impl(source, options, kind, select, id, context, receiver, observer, error); });
  }
#endif
  auto create = [&] { return create_desktop_capture_impl(source, options, kind, select, id, nullptr, receiver, observer, error); };
  return role && !role->IsCurrent() ? role->BlockingCall(create) : create();
}

static std::unique_ptr<NativeScreenCapture> open_screen_impl(
    const NativeVideoSource& source, std::int64_t screen_id, bool window,
    std::shared_ptr<CaptureGlibContext> context, rust::String& error) noexcept {
  auto* role = source.state()->signaling_thread;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (source.state()->closed.load() || !source.state()->source) {
    error = "desktop source is closed";
    return nullptr;
  }
  auto options = webrtc::DesktopCaptureOptions::CreateDefault();
  auto state = std::make_unique<NativeScreenCapture::State>();
  state->role = role;
  state->options = options;
  state->context = std::move(context);
  std::unique_ptr<webrtc::DesktopCapturer> capturer;
#if defined(WEBRTC_USE_PIPEWIRE)
  if (webrtc::BaseCapturerPipeWire::IsSupported()) {
    capturer = std::make_unique<ObservablePipeWireCapturer>(
        options, window ? webrtc::CaptureType::kWindow
                        : webrtc::CaptureType::kScreen,
        state->status, state->closed);
  }
#endif
  if (!capturer) {
    capturer = window
        ? webrtc::DesktopCapturer::CreateWindowCapturer(options)
        : webrtc::DesktopCapturer::CreateScreenCapturer(options);
  }
  if (!capturer) { error = "desktop capture unavailable on this platform"; return nullptr; }
  webrtc::DesktopCapturer::SourceList available;
  if (!capturer->GetSourceList(&available) ||
      std::none_of(available.begin(), available.end(), [screen_id](const auto& item) {
        return static_cast<std::int64_t>(item.id) == screen_id;
      }) ||
      !capturer->SelectSource(static_cast<webrtc::DesktopCapturer::SourceId>(screen_id))) {
    error = "screen source is not available";
    return nullptr;
  }
  state->sink = std::make_unique<ScreenFrameSink>(
      source.state()->source, source.state()->closed, state->status, &state->options->clock());
  if (auto* delegated = capturer->GetDelegatedSourceListController()) {
    state->selection = std::make_unique<ScreenSelectionObserver>(state->status);
    delegated->Observe(state->selection.get());
    state->observing_selection = true;
  } else {
    state->status.store(1);
  }
  capturer->Start(state->sink.get());
  state->capturer = std::move(capturer);
  return std::make_unique<NativeScreenCapture>(std::move(state));
#else
  error = "screen capture requires a native artifact";
  return nullptr;
#endif
}
std::unique_ptr<NativeScreenCapture> open_screen(
    const NativeVideoSource& source, std::int64_t screen_id, bool window,
    rust::String& error) noexcept {
  auto* role = source.state()->signaling_thread;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_PIPEWIRE) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (webrtc::BaseCapturerPipeWire::IsSupported()) {
    if (!role) { error = "Wayland progress requires a retained SDK role"; return nullptr; }
    auto context = CaptureGlibContext::Create(role);
    return context->Call([&] { return open_screen_impl(source, screen_id, window, context, error); });
  }
#endif
  auto create = [&] { return open_screen_impl(source, screen_id, window, nullptr, error); };
  return role && !role->IsCurrent() ? role->BlockingCall(create) : create();
}
bool screen_capture_next_frame(const NativeScreenCapture& capture) noexcept {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto context = capture.state()->context;
  if (context && !context->IsCurrent()) return context->Call([&] { return screen_capture_next_frame(capture); });
  CaptureGlibContext::Scope scope(context.get());
#endif
  auto* role = capture.state()->role;
  if (
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
      !context &&
#endif
      role && !role->IsCurrent())
    return role->BlockingCall([&] { return screen_capture_next_frame(capture); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (capture.state()->closed.load() || !capture.state()->capturer) return false;
  capture.state()->capturer->CaptureFrame();
  return true;
#else
  return false;
#endif
}
std::uint8_t screen_capture_status(const NativeScreenCapture& capture) noexcept {
  if (capture.state()->closed.load()) return 5;
  return capture.state()->status.load();
}
std::uint64_t screen_capture_failed_frames(const NativeScreenCapture& capture) noexcept {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto context = capture.state()->context;
  if (context && !context->IsCurrent()) return context->Call([&] { return screen_capture_failed_frames(capture); });
  CaptureGlibContext::Scope scope(context.get());
#endif
  auto* role = capture.state()->role;
  if (
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
      !context &&
#endif
      role && !role->IsCurrent())
    return role->BlockingCall([&] { return screen_capture_failed_frames(capture); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  return capture.state()->sink ? capture.state()->sink->failed_frames()
                               : capture.state()->final_failed_frames;
#else
  return 0;
#endif
}
bool close_screen(const NativeScreenCapture& capture) noexcept {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto context = capture.state()->context;
  if (context && !context->IsCurrent()) return context->Call([&] { return close_screen(capture); });
  CaptureGlibContext::Scope scope(context.get());
#endif
  auto* role = capture.state()->role;
  if (
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
      !context &&
#endif
      role && !role->IsCurrent())
    return role->BlockingCall([&] { return close_screen(capture); });
  if (capture.state()->closed.exchange(true)) return true;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  capture.state()->capturer.reset();
  capture.state()->final_failed_frames = capture.state()->sink->failed_frames();
  capture.state()->sink.reset();
  capture.state()->selection.reset();
  capture.state()->options.reset();
  if (context) context->Stop();
  capture.state()->context.reset();
#endif
  return true;
}

template <typename F> auto OnDesktopCaptureRole(const NativeScreenCapture& capture, F&& call) -> std::invoke_result_t<F> {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto context = capture.state()->context;
  if (context) return context->Call(std::forward<F>(call));
#endif
  auto* role = capture.state()->role;
  return role && !role->IsCurrent() ? role->BlockingCall(std::forward<F>(call)) : std::forward<F>(call)();
}
struct NativeDesktopMetadata::State {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_GIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  webrtc::DesktopCaptureMetadata value;
  ~State() {
    if (value.session_details.proxy) g_object_unref(value.session_details.proxy);
    if (value.session_details.cancellable) g_object_unref(value.session_details.cancellable);
  }
#endif
};
NativeDesktopMetadata::NativeDesktopMetadata(std::unique_ptr<State> state) noexcept : state_(std::move(state)) {}
NativeDesktopMetadata::~NativeDesktopMetadata() = default;
const std::unique_ptr<NativeDesktopMetadata::State>& NativeDesktopMetadata::state() const noexcept { return state_; }
std::unique_ptr<NativeDesktopMetadata> desktop_capture_metadata(const NativeScreenCapture& capture) noexcept {
  return OnDesktopCaptureRole(capture, [&]() -> std::unique_ptr<NativeDesktopMetadata> {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_GIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    if (capture.state()->closed.load() || !capture.state()->capturer) return nullptr;
    auto state = std::make_unique<NativeDesktopMetadata::State>();
    state->value = capture.state()->capturer->GetMetadata();
    // SessionDetails contains borrowed GObjects, not owning smart pointers.
    if (state->value.session_details.proxy) g_object_ref(state->value.session_details.proxy);
    if (state->value.session_details.cancellable) g_object_ref(state->value.session_details.cancellable);
    return std::make_unique<NativeDesktopMetadata>(std::move(state));
#else
    return nullptr;
#endif
  });
}
std::unique_ptr<NativeDesktopMetadata> desktop_metadata_clone(const NativeDesktopMetadata& metadata) noexcept {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_GIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto state = std::make_unique<NativeDesktopMetadata::State>();
  state->value = metadata.state()->value;
  if (state->value.session_details.proxy) g_object_ref(state->value.session_details.proxy);
  if (state->value.session_details.cancellable) g_object_ref(state->value.session_details.cancellable);
  return std::make_unique<NativeDesktopMetadata>(std::move(state));
#else
  return nullptr;
#endif
}
FfiDesktopMetadata desktop_metadata_read(const NativeDesktopMetadata& metadata) noexcept {
  FfiDesktopMetadata out{};
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_GIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  const auto& value = metadata.state()->value.session_details;
  out.session_handle = value.session_handle; out.pipewire_stream_node_id = value.pipewire_stream_node_id;
  out.has_proxy = value.proxy != nullptr; out.has_cancellable = value.cancellable != nullptr;
#endif
  return out;
}
bool desktop_metadata_equal(const NativeDesktopMetadata& a, const NativeDesktopMetadata& b, bool cancellable) noexcept {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_USE_GIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  const auto& x = a.state()->value.session_details; const auto& y = b.state()->value.session_details;
  return cancellable ? x.cancellable && x.cancellable == y.cancellable : x.proxy && x.proxy == y.proxy;
#else
  return false;
#endif
}
std::uint8_t desktop_set_memory_factory(const NativeScreenCapture& capture, rust::Box<RustDesktopMemoryFactory> source) noexcept {
  return OnDesktopCaptureRole(capture, [&]() -> std::uint8_t {
    if (capture.state()->closed.load()) return 1;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && defined(WEBRTC_LINUX) && !defined(WEBRTC_ANDROID)
    // Pinned Linux screen/window/PipeWire capturers inherit the base no-op.
    // A selected provider cannot be represented as installed. None may still
    // call the SDK's actual optional setter; it makes no allocation promise.
    if (desktop_memory_factory_present(*source)) return 2;
    capture.state()->capturer->SetSharedMemoryFactory(nullptr);
    return 0;
#else
    return 2;
#endif
  });
}
std::uint8_t desktop_set_receiver(const NativeScreenCapture& capture, rust::Box<RustDesktopCaptureReceiver> source) noexcept {
  return OnDesktopCaptureRole(capture, [&]() -> std::uint8_t {
    if (capture.state()->closed.load()) return 1;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    capture.state()->sink->SetReceiver(std::move(source)); return 0;
#else
    return 2;
#endif
  });
}
std::uint8_t desktop_set_selection_observer(const NativeScreenCapture& capture, rust::Box<RustDesktopSelectionObserver> source) noexcept {
  return OnDesktopCaptureRole(capture, [&]() -> std::uint8_t {
    const auto& state = capture.state();
    if (state->closed.load()) return 1;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
    auto* delegated = state->capturer->GetDelegatedSourceListController();
    if (!delegated) return 2;
    if (state->observing_selection) { delegated->Observe(nullptr); state->observing_selection = false; }
    const bool present = desktop_selection_present(*source);
    state->selection->SetObserver(std::move(source));
    if (present) { delegated->Observe(state->selection.get()); state->observing_selection = true; }
    return 0;
#else
    return 2;
#endif
  });
}

FfiDesktopControl desktop_control(const NativeScreenCapture& capture, std::uint8_t kind,
    std::int64_t id, std::int32_t x, std::int32_t y,
    std::uint32_t width, std::uint32_t height) noexcept {
  const auto& state = capture.state();
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto context = state->context;
  if (context && !context->IsCurrent()) return context->Call([&] { return desktop_control(capture, kind, id, x, y, width, height); });
  CaptureGlibContext::Scope scope(context.get());
#endif
  if (
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
      !context &&
#endif
      state->role && !state->role->IsCurrent())
    return state->role->BlockingCall([&] { return desktop_control(capture, kind, id, x, y, width, height); });
  FfiDesktopControl reply{};
  if (state->closed.load()) { reply.status = 1; return reply; }
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto* capturer = state->capturer.get();
  if (!capturer) { reply.status = 1; return reply; }
  switch (kind) {
    case 0: {
      webrtc::DesktopCapturer::SourceList sources;
      reply.flag = capturer->GetSourceList(&sources);
      if (reply.flag) for (const auto& source : sources) {
        FfiScreenSource item{};
        item.id = static_cast<std::int64_t>(source.id);
        item.name = source.title;
        item.display_id = source.display_id;
        reply.sources.push_back(std::move(item));
      }
      break;
    }
    case 1: reply.flag = capturer->SelectSource(static_cast<webrtc::DesktopCapturer::SourceId>(id)); break;
    case 2: reply.flag = capturer->FocusOnSelectedSource(); break;
    case 3: reply.flag = capturer->IsOccluded(webrtc::DesktopVector(x, y)); break;
    case 4: capturer->SetExcludedWindow(static_cast<webrtc::WindowId>(id)); break;
    case 5: capturer->SetMaxFrameRate(width); break;
    case 6:
      if (width > INT32_MAX / 4 || height > INT32_MAX ||
          (height != 0 && width > INT32_MAX / height / 4)) reply.status = 3;
      else capturer->UpdateResolution(width, height);
      break;
    case 7: reply.flag = capturer->GetDelegatedSourceListController() != nullptr; break;
    case 8: case 9: case 10: case 11: {
      auto* delegated = capturer->GetDelegatedSourceListController();
      if (!delegated) { reply.status = 2; break; }
      if (kind == 8) delegated->EnsureVisible();
      else if (kind == 9) delegated->EnsureHidden();
      else {
        const bool enable = kind == 10;
        // SDK Observe requires clearing the prior observer before registering.
        // A repeated setter must not reach its non-null/non-null DCHECK.
        if (enable != state->observing_selection) {
          delegated->Observe(enable ? state->selection.get() : nullptr);
          state->observing_selection = enable;
        }
      }
      break;
    }
    default: reply.status = 3; break;
  }
#else
  reply.status = 2;
#endif
  return reply;
}

bool camera_devices(rust::Vec<FfiCameraDevice>& devices,
                    rust::String& error) noexcept {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  std::unique_ptr<webrtc::VideoCaptureModule::DeviceInfo> info(
      webrtc::VideoCaptureFactory::CreateDeviceInfo());
  if (!info) {
    error = "camera enumeration unavailable on this platform";
    return false;
  }
  for (std::uint32_t i = 0; i < info->NumberOfDevices(); ++i) {
    char name[webrtc::kVideoCaptureDeviceNameLength] = {};
    char id[webrtc::kVideoCaptureUniqueNameLength] = {};
    char product[webrtc::kVideoCaptureProductIdLength] = {};
    if (info->GetDeviceName(i, name, sizeof(name), id, sizeof(id), product, sizeof(product)) != 0) {
      devices.clear();
      error = "camera device lookup failed";
      return false;
    }
    FfiCameraDevice device{};
    device.name = name;
    device.id = id;
    device.product_id = product;
    devices.push_back(std::move(device));
  }
  return true;
#else
  error = "camera enumeration requires a supported native artifact";
  return false;
#endif
}

FfiCameraDeviceControl camera_device_control(rust::Str device_id, std::uint8_t kind,
    const FfiCameraSettings& requested) noexcept {
  FfiCameraDeviceControl reply{};
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  const std::string id(device_id.data(), device_id.size());
  if (id.empty() || id.find('\0') != std::string::npos) { reply.status = 3; return reply; }
  std::unique_ptr<webrtc::VideoCaptureModule::DeviceInfo> info(webrtc::VideoCaptureFactory::CreateDeviceInfo());
  if (!info) { reply.status = 2; return reply; }
  auto append = [&](const webrtc::VideoCaptureCapability& value) {
    FfiCameraSettings cap{};
    cap.width = value.width; cap.height = value.height; cap.max_fps = value.maxFPS;
    cap.pixel_format = static_cast<std::int32_t>(value.videoType);
    cap.interlaced = value.interlaced;
    reply.capabilities.push_back(std::move(cap));
  };
  if (kind == 0) {
    const auto count = info->NumberOfCapabilities(id.c_str());
    if (count < 0) { reply.native_result = count; return reply; }
    for (int index = 0; index < count; ++index) {
      webrtc::VideoCaptureCapability value;
      reply.native_result = info->GetCapability(id.c_str(), index, value);
      if (reply.native_result != 0) { reply.capabilities.clear(); return reply; }
      append(value);
    }
  } else if (kind == 1) {
    webrtc::VideoRotation value = webrtc::kVideoRotation_0;
    reply.native_result = info->GetOrientation(id.c_str(), value);
    if (reply.native_result == 0) reply.orientation = static_cast<std::uint16_t>(value);
  } else if (kind == 2) {
    if (requested.width < 0 || requested.height < 0 || requested.max_fps < 0 ||
        std::int64_t{requested.width} * requested.height > INT32_MAX / 4) {
      reply.status = 3; return reply;
    }
    webrtc::VideoCaptureCapability input;
    input.width = requested.width; input.height = requested.height; input.maxFPS = requested.max_fps;
    input.videoType = static_cast<webrtc::VideoType>(requested.pixel_format);
    input.interlaced = requested.interlaced;
    webrtc::VideoCaptureCapability output;
    const auto result = info->GetBestMatchedCapability(id.c_str(), input, output);
    if (result < 0) reply.native_result = result;
    else { reply.matched_index = result; append(output); }
  } else reply.status = 3;
#else
  reply.status = 2;
#endif
  return reply;
}

bool camera_formats(rust::Str device_id,
                    rust::Vec<FfiCameraFormat>& formats,
                    rust::String& error) noexcept {
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  const std::string id(device_id.data(), device_id.size());
  if (id.empty() || id.find('\0') != std::string::npos) {
    error = "invalid camera device id";
    return false;
  }
  std::unique_ptr<webrtc::VideoCaptureModule::DeviceInfo> info(
      webrtc::VideoCaptureFactory::CreateDeviceInfo());
  if (!info) { error = "camera formats unavailable on this platform"; return false; }
  const int count = info->NumberOfCapabilities(id.c_str());
  if (count < 0) { error = "camera device is unavailable"; return false; }
  for (int i = 0; i < count; ++i) {
    webrtc::VideoCaptureCapability cap{};
    if (info->GetCapability(id.c_str(), i, cap) != 0) {
      formats.clear();
      error = "camera capability lookup failed";
      return false;
    }
    if (cap.width <= 0 || cap.height <= 0 || cap.maxFPS <= 0) continue;
    FfiCameraFormat format{};
    format.width = static_cast<std::uint32_t>(cap.width);
    format.height = static_cast<std::uint32_t>(cap.height);
    format.max_fps = static_cast<std::uint32_t>(cap.maxFPS);
    format.pixel_format = static_cast<std::int32_t>(cap.videoType);
    formats.push_back(format);
  }
  return true;
#else
  error = "camera formats require a supported native artifact";
  return false;
#endif
}

static std::unique_ptr<NativeCamera> create_camera_impl(const NativeVideoSource& source,
    const NativeCameraOptions* options, rust::Str device_id, rust::String& error) noexcept {
  auto* role = source.state()->signaling_thread;
  if (options && role != options->state()->role) { error = "camera options belong to another SDK role"; return nullptr; }
  if (role && !role->IsCurrent())
    return role->BlockingCall([&] { return create_camera_impl(source, options, device_id, error); });
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  const std::string id(device_id.data(), device_id.size());
  if (source.state()->closed.load() || !source.state()->source ||
      id.empty() || id.find('\0') != std::string::npos) {
    error = "invalid camera source or device id"; return nullptr;
  }
#if defined(WEBRTC_LINUX)
#if defined(WEBRTC_USE_PIPEWIRE)
  if (options && options->state()->value->allow_pipewire()) {
    error = "pinned PipeWire camera session lacks safe shutdown ordering"; return nullptr;
  }
#endif
  auto module = options
      ? webrtc::VideoCaptureFactory::Create(options->state()->value.get(), id.c_str())
      : webrtc::VideoCaptureFactory::Create(id.c_str());
#else
  auto module = webrtc::VideoCaptureFactory::Create(id.c_str());
#endif
  if (!module) { error = "camera capture module unavailable"; return nullptr; }
  auto state = std::make_unique<NativeCamera::State>();
  state->role = role;
  state->device_stopped = true;
  state->last_frame_us.store(webrtc::TimeMicros());
  state->sink = std::make_unique<CameraFrameSink>(source.state()->source,
      source.state()->closed, state->last_frame_us, state->frame_seen);
  module->RegisterCaptureDataCallback(state->sink.get());
  state->module = std::move(module);
  return std::make_unique<NativeCamera>(std::move(state));
#else
  error = "camera capture requires a supported native artifact";
  return nullptr;
#endif
}

std::unique_ptr<NativeCamera> create_camera(const NativeVideoSource& source,
    rust::Str device_id, rust::String& error) noexcept {
  return create_camera_impl(source, nullptr, device_id, error);
}
std::unique_ptr<NativeCamera> create_camera_with_options(const NativeVideoSource& source,
    const NativeCameraOptions& options, rust::Str device_id, rust::String& error) noexcept {
  return create_camera_impl(source, &options, device_id, error);
}
std::unique_ptr<NativeCamera> open_camera(const NativeVideoSource& source,
                                           rust::Str device_id,
                                           std::uint32_t width,
                                           std::uint32_t height,
                                           std::uint32_t fps,
                                           rust::String& error) noexcept {
  auto* role = source.state()->signaling_thread;
  if (role && !role->IsCurrent()) {
    return role->BlockingCall([&] { return open_camera(source, device_id, width, height, fps, error); });
  }
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (source.state()->closed.load() || !source.state()->source ||
      device_id.empty() || width == 0 || height == 0 || fps == 0 ||
      width > 4096 || height > 4096 || fps > 120) {
    error = "invalid camera source or capture settings";
    return nullptr;
  }
  const std::string id(device_id.data(), device_id.size());
  if (id.find('\0') != std::string::npos) {
    error = "camera device id contains a NUL byte";
    return nullptr;
  }
  std::unique_ptr<webrtc::VideoCaptureModule::DeviceInfo> info(
      webrtc::VideoCaptureFactory::CreateDeviceInfo());
  if (!info) { error = "camera capture unavailable on this platform"; return nullptr; }
  webrtc::VideoCaptureCapability requested{};
  requested.width = static_cast<int>(width);
  requested.height = static_cast<int>(height);
  requested.maxFPS = static_cast<int>(fps);
  webrtc::VideoCaptureCapability chosen{};
  if (info->GetBestMatchedCapability(id.c_str(), requested, chosen) < 0) {
    error = "camera has no matching capture capability";
    return nullptr;
  }
  auto camera = create_camera(source, device_id, error);
  if (!camera) return nullptr;
  if (camera->state()->module->StartCapture(chosen) != 0) {
    error = "camera start failed";
    return nullptr;
  }
  camera->state()->device_stopped = false;
  return camera;
#else
  error = "camera capture requires a supported native artifact";
  return nullptr;
#endif
}
std::uint8_t camera_capture_status(const NativeCamera& camera,
                                   std::uint64_t stale_after_ms) noexcept {
  const auto& state = camera.state();
  if (state->role && !state->role->IsCurrent())
    return state->role->BlockingCall([&] { return camera_capture_status(camera, stale_after_ms); });
  if (state->closed.load()) return 3;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (!state->module) return 4;
  if (!state->module->CaptureStarted()) return state->device_stopped ? 5 : 4;
  const auto last = state->last_frame_us.load();
  const auto elapsed = webrtc::TimeMicros() - last;
  // Saturate instead of overflowing when an application supplies a very
  // large timeout. A stale frame means delivery stopped, not a diagnosed
  // hardware or permission cause.
  const auto limit = stale_after_ms > static_cast<std::uint64_t>(INT64_MAX / 1000)
                         ? INT64_MAX
                         : static_cast<std::int64_t>(stale_after_ms * 1000);
  if (elapsed >= limit) return 2;
  return state->frame_seen.load() ? 1 : 0;
#else
  (void)stale_after_ms;
  return 4;
#endif
}

bool close_camera(const NativeCamera& camera) noexcept {
  auto* role = camera.state()->role;
  if (role && !role->IsCurrent())
    return role->BlockingCall([&] { return close_camera(camera); });
  if (camera.state()->closed.exchange(true)) return true;
  bool stopped = true;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (camera.state()->module) {
    stopped = camera.state()->module->StopCapture() == 0;
    camera.state()->module->DeRegisterCaptureDataCallback();
    camera.state()->sink.reset();
    camera.state()->raw_sink.reset();
    camera.state()->module = nullptr;
  }
#endif
  return stopped;
}

std::uint8_t camera_set_raw_sink(const NativeCamera& camera, rust::Box<RustRawCameraSink> source) noexcept {
  auto* role = camera.state()->role;
  if (role && !role->IsCurrent()) return role->BlockingCall([&] { return camera_set_raw_sink(camera, std::move(source)); });
  if (camera.state()->closed.load()) return 1;
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  auto& state = *camera.state();
  if (!state.module) return 1;
  // This real SDK call clears both slots and waits for api_lock_ held by an
  // in-flight callback. This is deliberately NOT an atomic swap or an SDK patch:
  // frames in the unregister/register gap may be discarded by the SDK.
  state.module->DeRegisterCaptureDataCallback();
  auto previous = std::move(state.raw_sink);
  if (camera_raw_sink_present(*source)) {
    state.raw_sink = std::make_unique<RawCameraFrameSink>(std::move(source), state.last_frame_us, state.frame_seen);
    state.module->RegisterCaptureDataCallback(state.raw_sink.get());
  } else state.module->RegisterCaptureDataCallback(state.sink.get());
  // No native mutex is held during arbitrary user Drop. Reentry is caught by
  // the Rust callback scope before any SDK blocking call.
  previous.reset();
  return 0;
#else
  return 2;
#endif
}

FfiCameraControl camera_control(const NativeCamera& camera, std::uint8_t kind,
    std::uint16_t rotation, bool flag, const FfiCameraSettings& settings) noexcept {
  const auto& state = camera.state();
  if (state->role && !state->role->IsCurrent())
    return state->role->BlockingCall([&] { return camera_control(camera, kind, rotation, flag, settings); });
  FfiCameraControl reply{};
  if (state->closed.load()) { reply.status = 1; return reply; }
#if defined(PULSEBEAM_WEBRTC_NATIVE_AUDIO) && !defined(WEBRTC_ANDROID) && !defined(WEBRTC_IOS)
  if (!state->module) { reply.status = 1; return reply; }
  switch (kind) {
    case 0: {
      webrtc::VideoCaptureCapability value;
      reply.native_result = state->module->CaptureSettings(value);
      if (reply.native_result == 0) {
        reply.settings.width = value.width;
        reply.settings.height = value.height;
        reply.settings.max_fps = value.maxFPS;
        reply.settings.pixel_format = static_cast<std::int32_t>(value.videoType);
        reply.settings.interlaced = value.interlaced;
      }
      break;
    }
    case 1: {
      const auto* name = state->module->CurrentDeviceName();
      if (name) reply.device_name = name;
      break;
    }
    case 2: reply.flag = state->module->CaptureStarted(); break;
    case 3:
      if (rotation != 0 && rotation != 90 && rotation != 180 && rotation != 270) { reply.status = 3; break; }
      reply.native_result = state->module->SetCaptureRotation(static_cast<webrtc::VideoRotation>(rotation));
      break;
    case 4: reply.flag = state->module->SetApplyRotation(flag); break;
    case 5: reply.flag = state->module->GetApplyRotation(); break;
    case 6:
      reply.native_result = state->module->StopCapture();
      if (reply.native_result == 0) state->device_stopped = true;
      break;
    case 7: {
      if (settings.width < 0 || settings.height < 0 || settings.max_fps < 0 ||
          std::int64_t{settings.width} * settings.height > INT32_MAX / 4) {
        reply.status = 3; break;
      }
      webrtc::VideoCaptureCapability value;
      value.width = settings.width;
      value.height = settings.height;
      value.maxFPS = settings.max_fps;
      value.videoType = static_cast<webrtc::VideoType>(settings.pixel_format);
      value.interlaced = settings.interlaced;
      reply.native_result = state->module->StartCapture(value);
      if (reply.native_result == 0) {
        state->device_stopped = false;
        state->frame_seen.store(false);
        state->last_frame_us.store(webrtc::TimeMicros());
      }
      break;
    }
    default: reply.status = 3; break;
  }
#else
  reply.status = 2;
#endif
  return reply;
}

std::unique_ptr<NativeVideoSource> create_video_source(
    const NativePeerConnectionFactory& factory) noexcept {
  auto state = std::make_unique<NativeVideoSource::State>();
  state->signaling_thread = factory.signaling_thread();
  state->worker_thread = factory.worker_thread();
  if (!state->signaling_thread) {
    return nullptr;
  }
  state->signaling_thread->BlockingCall(
      [&state] {
        state->source = webrtc::make_ref_counted<PushVideoSource>();
        state->source->SetState(webrtc::MediaSourceInterface::kLive);
      });
  return std::make_unique<NativeVideoSource>(std::move(state));
}

bool close_video_source(const NativeVideoSource& source) noexcept {
  auto& state = *source.state();
  if (state.closed.exchange(true)) {
    return true;
  }
  if (!state.source || !state.signaling_thread) {
    return false;
  }
  state.signaling_thread->BlockingCall(
      [&state] { state.source->SetState(webrtc::MediaSourceInterface::kEnded); });
  return true;
}

std::uint8_t video_source_state(const NativeVideoSource& source) noexcept {
  return source.state()->signaling_thread->BlockingCall([&] {
    return static_cast<std::uint8_t>(source.state()->source->state());
  });
}

bool video_source_push_encoded_trigger(
    const NativeVideoSource& source, std::uint32_t width,
    std::uint32_t height, std::int64_t timestamp_us,
    std::int64_t token) noexcept {
  if (source.state()->closed.load() || width == 0 || height == 0 ||
      width > 4096 || height > 4096 || token <= 0 || timestamp_us < 0) return false;
  auto buffer = webrtc::I420Buffer::Create(static_cast<int>(width),
                                           static_cast<int>(height));
  buffer->InitializeData();
  auto frame = webrtc::VideoFrame::Builder()
                   .set_video_frame_buffer(std::move(buffer))
                   .set_timestamp_us(timestamp_us)
                   .set_presentation_timestamp(webrtc::Timestamp::Micros(token))
                   .build();
  source.state()->signaling_thread->BlockingCall(
      [&] { source.state()->source->Push(frame); });
  return true;
}

std::size_t video_source_i420_allocation_size(std::uint32_t width,
                                             std::uint32_t height) noexcept {
  static_assert(std::numeric_limits<int>::max() == INT32_MAX);
  if (width == 0 || height == 0 || width > INT32_MAX || height > INT32_MAX) return 0;
  // I420Buffer::I420DataSize narrows this exact packed allocation to int.
  // Widen first. For int32 dimensions the products and sum fit uint64_t.
  const std::uint64_t w = width, h = height;
  const auto size = w * h + 2 * ((w + 1) / 2) * ((h + 1) / 2);
  if (size > INT32_MAX) return 0;
  // This bound also proves width+1 and height+1 in the SDK constructors
  // cannot overflow int, including their odd-dimension chroma rounding.
  return static_cast<std::size_t>(size);
}

bool video_source_push_frame(const NativeVideoSource& source,
                             rust::Slice<const std::uint8_t> data,
                             std::uint32_t width, std::uint32_t height,
                             std::int64_t timestamp_us,
                             std::uint32_t rtp_timestamp,
                             std::uint16_t rotation) noexcept {
  const auto allocation_size = video_source_i420_allocation_size(width, height);
  if (source.state()->closed.load() || allocation_size == 0 ||
      timestamp_us == INT64_MIN || timestamp_us == INT64_MAX ||
      data.size() != allocation_size ||
      (rotation != 0 && rotation != 90 && rotation != 180 && rotation != 270)) {
    return false;
  }
  const std::size_t chroma_width = (static_cast<std::size_t>(width) + 1) / 2;
  const std::size_t chroma_height = (static_cast<std::size_t>(height) + 1) / 2;
  const std::size_t y_size = static_cast<std::size_t>(width) * height;
  const std::size_t chroma_size = chroma_width * chroma_height;
  auto buffer = webrtc::I420Buffer::Create(static_cast<int>(width),
                                           static_cast<int>(height));
  std::memcpy(buffer->MutableDataY(), data.data(), y_size);
  std::memcpy(buffer->MutableDataU(), data.data() + y_size, chroma_size);
  std::memcpy(buffer->MutableDataV(), data.data() + y_size + chroma_size,
              chroma_size);
  source.state()->source->Push(
      webrtc::VideoFrame::Builder()
          .set_video_frame_buffer(std::move(buffer))
          .set_timestamp_us(timestamp_us)
          .set_rtp_timestamp(rtp_timestamp)
          .set_rotation(static_cast<webrtc::VideoRotation>(rotation))
          .build());
  return true;
}

bool video_source_push_native_frame(const NativeVideoSource& source, const NativeRawVideoFrame& frame) noexcept {
  if (source.state()->closed.load() || frame.value().width() <= 0 || frame.value().height() <= 0 ||
      frame.value().timestamp_us() == INT64_MIN || frame.value().timestamp_us() == INT64_MAX ||
      video_source_i420_allocation_size(frame.value().width(), frame.value().height()) == 0) return false;
  source.state()->source->Push(frame.value());
  return true;
}

std::unique_ptr<NativeVideoTrack> create_video_track(
    const NativePeerConnectionFactory& factory,
    const NativeVideoSource& source, rust::Str id) noexcept {
  if (source.state()->closed.load()) {
    return nullptr;
  }
  auto track = factory.factory()->CreateVideoTrack(
      source.state()->source, std::string_view(id.data(), id.size()));
  if (!track) {
    return nullptr;
  }
  auto state = std::make_unique<NativeVideoTrack::State>();
  state->track = std::move(track);
  return std::make_unique<NativeVideoTrack>(std::move(state));
}

NativeMediaTrack::NativeMediaTrack(webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface> track) noexcept : track_(std::move(track)) {}
NativeMediaTrack::~NativeMediaTrack() = default;
const webrtc::scoped_refptr<webrtc::MediaStreamTrackInterface>& NativeMediaTrack::track() const noexcept { return track_; }
rust::Vec<std::uint8_t> media_track_id(const NativeMediaTrack& track) noexcept {
  rust::Vec<std::uint8_t> out; for (unsigned char c : track.track()->id()) out.push_back(c); return out;
}
rust::Vec<std::uint8_t> media_track_kind(const NativeMediaTrack& track) noexcept {
  rust::Vec<std::uint8_t> out; for (unsigned char c : track.track()->kind()) out.push_back(c); return out;
}
std::int32_t media_track_state(const NativeMediaTrack& track) noexcept { return static_cast<std::int32_t>(track.track()->state()); }
bool media_track_enabled(const NativeMediaTrack& track) noexcept { return track.track()->enabled(); }
bool media_track_set_enabled(const NativeMediaTrack& track, bool enabled) noexcept { return track.track()->set_enabled(enabled); }
bool media_track_same_resource(const NativeMediaTrack& a, const NativeMediaTrack& b) noexcept { return a.track().get() == b.track().get(); }
std::unique_ptr<NativeMediaTrack> video_track_reference(const NativeVideoTrack& track) noexcept { return std::make_unique<NativeMediaTrack>(track.state()->track); }
std::unique_ptr<NativeMediaTrack> rtp_sender_track_reference(const NativeRtpSender& sender) noexcept {
  auto track = sender.state()->sender->track(); return track ? std::make_unique<NativeMediaTrack>(std::move(track)) : nullptr;
}
std::unique_ptr<NativeMediaTrack> rtp_receiver_track_reference(const NativeRtpReceiver& receiver) noexcept {
  auto track = receiver.state()->receiver->track(); return track ? std::make_unique<NativeMediaTrack>(std::move(track)) : nullptr;
}
rust::String video_track_id(const NativeVideoTrack& track) noexcept {
  return track.state()->track->id();
}
bool video_track_enabled(const NativeVideoTrack& track) noexcept {
  return track.state()->track->enabled();
}
bool video_track_set_enabled(const NativeVideoTrack& track,
                             bool enabled) noexcept {
  return track.state()->track->set_enabled(enabled);
}
std::uint8_t video_track_state(const NativeVideoTrack& track) noexcept {
  return static_cast<std::uint8_t>(track.state()->track->state());
}

rust::String video_track_kind(const NativeVideoTrack& track) noexcept {
  return track.state()->track->kind();
}
bool video_track_should_receive(const NativeVideoTrack& track) noexcept {
  return track.state()->track->should_receive();
}
void video_track_set_should_receive(const NativeVideoTrack& track, bool receive) noexcept {
  track.state()->track->set_should_receive(receive);
}
std::uint8_t video_track_content_hint(const NativeVideoTrack& track) noexcept {
  return static_cast<std::uint8_t>(track.state()->track->content_hint());
}
void video_track_set_content_hint(const NativeVideoTrack& track, std::uint8_t hint) noexcept {
  // Closed SDK enum; only the four typed Rust variants cross this bridge.
  track.state()->track->set_content_hint(static_cast<webrtc::VideoTrackInterface::ContentHint>(hint));
}

namespace {
// Binding outcomes, not invented SDK errors: 0 invoked, 1 closed, 2 unsafe
// alignment, 3 invalid target. The complete scope runs on the existing worker.
std::uint8_t ApplySinkWants(NativeVideoSink::State& state,
                            const webrtc::VideoSinkWants& wants) {
  if (state.closed.load()) return 1;
  auto apply = [&] {
    if (state.track) state.track->AddOrUpdateSink(state.sink.get(), wants);
    else state.source->AddOrUpdateSink(state.sink.get(), wants);
  };
  if (state.producer) {
    return state.producer->broadcaster().AdmitAndApply(state.sink.get(), wants, apply) ? 0 : 2;
  }
  // Closed remote construction has only app raw sinks; both this preflight and
  // application run on the same worker as every other raw mutation/replay.
  if (!state.remote_broadcaster) return 3;
  if (!RemoteBroadcasterObservation::Admits(*state.remote_broadcaster, state.sink.get(), wants)) return 2;
  apply();
  return 0;
}
std::unique_ptr<NativeVideoSink> AttachVideoSink(
    const NativeVideoSourceView& view,
    webrtc::scoped_refptr<webrtc::VideoTrackInterface> track,
    const FfiVideoSinkWants& input, std::uint8_t& status,
    std::optional<rust::Box<RustDecodedVideoObserver>> observer = std::nullopt) {
  auto state = std::make_unique<NativeVideoSink::State>();
  state->source = view.state()->source;
  state->producer = view.state()->producer;
  state->remote_broadcaster = view.state()->remote_broadcaster;
  state->worker = view.state()->worker;
  state->track = std::move(track);
  if (state->track && state->track->GetSource() != state->source.get()) {
    status = 3;
    return nullptr;
  }
  state->sink = std::make_unique<FrameSink>(std::move(observer));
  const auto wants = ToVideoSinkWants(input);
  status = state->worker->BlockingCall([&] { return ApplySinkWants(*state, wants); });
  if (status != 0) return nullptr;
  return std::make_unique<NativeVideoSink>(std::move(state));
}
}  // namespace
FfiVideoSinkWants video_sink_default_wants() noexcept {
  return FromVideoSinkWants(webrtc::VideoSinkWants{});
}
std::unique_ptr<NativeVideoSink> video_track_attach_sink(
    const NativeVideoTrack& track, const NativeVideoSourceView& source,
    const FfiVideoSinkWants& wants, std::uint8_t& status) noexcept {
  return AttachVideoSink(source, track.state()->track, wants, status);
}
std::unique_ptr<NativeVideoSink> video_track_attach_observed_sink(
    const NativeVideoTrack& track, const NativeVideoSourceView& source,
    const FfiVideoSinkWants& wants, rust::Box<RustDecodedVideoObserver> observer,
    std::uint8_t& status) noexcept {
  return AttachVideoSink(source, track.state()->track, wants, status,
                         std::make_optional(std::move(observer)));
}
std::unique_ptr<NativeVideoSink> video_source_attach_sink(
    const NativeVideoSourceView& source, const FfiVideoSinkWants& wants,
    std::uint8_t& status) noexcept {
  return AttachVideoSink(source, nullptr, wants, status);
}
std::uint8_t video_sink_update_wants(const NativeVideoSink& sink,
                                     const FfiVideoSinkWants& input) noexcept {
  const auto wants = ToVideoSinkWants(input);
  return sink.state()->worker->BlockingCall([&] { return ApplySinkWants(*sink.state(), wants); });
}
FfiVideoSinkWants video_sink_wants(const NativeVideoSink& sink) noexcept {
  auto& state = *sink.state();
  return state.worker->BlockingCall([&]() -> FfiVideoSinkWants {
    if (state.closed.load()) return {};
    auto wants = state.producer
        ? state.producer->broadcaster().WantsForSink(state.sink.get())
        : RemoteBroadcasterObservation::WantsForSink(*state.remote_broadcaster, state.sink.get());
    return wants ? FromVideoSinkWants(*wants) : FfiVideoSinkWants{};
  });
}

std::unique_ptr<NativeRawVideoFrame> video_sink_take_native_frame(const NativeVideoSink& sink) noexcept {
  return sink.state()->sink ? sink.state()->sink->TakeNative() : nullptr;
}

std::unique_ptr<NativeVideoFrame> video_sink_take_frame(
    const NativeVideoSink& sink) noexcept {
  return sink.state()->sink ? sink.state()->sink->Take() : nullptr;
}

std::uint64_t video_sink_dropped_frames(const NativeVideoSink& sink) noexcept {
  return sink.state()->sink ? sink.state()->sink->DroppedFrames() : 0;
}

FfiVideoSinkEvent video_sink_take_event(const NativeVideoSink& sink) noexcept {
  return sink.state()->sink ? sink.state()->sink->TakeEvent() : FfiVideoSinkEvent{};
}

std::uint64_t video_sink_dropped_events(const NativeVideoSink& sink) noexcept {
  return sink.state()->sink ? sink.state()->sink->DroppedEvents() : 0;
}

bool close_video_sink(const NativeVideoSink& sink) noexcept {
  auto& state = *sink.state();
  if (state.closed.exchange(true)) {
    return true;
  }
  if (!state.source || !state.sink) return false;
  state.sink->Deactivate();
  state.worker->BlockingCall([&] {
    if (state.track) state.track->RemoveSink(state.sink.get());
    else state.source->RemoveSink(state.sink.get());
  });
  state.sink->RetireObserver();
  return true;
}

NativeTransceiverList::NativeTransceiverList(std::unique_ptr<State> state) noexcept
    : state_(std::move(state)) {}
NativeTransceiverList::~NativeTransceiverList() = default;
const std::unique_ptr<NativeTransceiverList::State>&
NativeTransceiverList::state() const noexcept { return state_; }

std::unique_ptr<NativeTransceiverList> peer_audio_transceivers(
    const NativePeerConnection& peer) noexcept {
  if (!peer.peer()) return nullptr;
  auto state = std::make_unique<NativeTransceiverList::State>();
  for (auto& transceiver : peer.peer()->GetTransceivers()) {
    if (transceiver && transceiver->media_type() == webrtc::MediaType::AUDIO) {
      state->transceivers.push_back(std::move(transceiver));
    }
  }
  return std::make_unique<NativeTransceiverList>(std::move(state));
}

std::unique_ptr<NativeTransceiverList> peer_video_transceivers(
    const NativePeerConnection& peer) noexcept {
  if (!peer.peer()) {
    return nullptr;
  }
  auto state = std::make_unique<NativeTransceiverList::State>();
  for (auto& transceiver : peer.peer()->GetTransceivers()) {
    if (transceiver && transceiver->media_type() == webrtc::MediaType::VIDEO) {
      state->transceivers.push_back(std::move(transceiver));
    }
  }
  return std::make_unique<NativeTransceiverList>(std::move(state));
}

rust::Vec<FfiVideoCodecCapability> peer_video_codec_capabilities(
    const NativePeerConnectionFactory& factory, bool sender) noexcept {
  rust::Vec<FfiVideoCodecCapability> result;
  if (!factory.factory() || !factory.signaling_thread()) {
    return result;
  }
  const auto capabilities = factory.signaling_thread()->BlockingCall([&] {
    return sender
        ? factory.factory()->GetRtpSenderCapabilities(webrtc::MediaType::VIDEO)
        : factory.factory()->GetRtpReceiverCapabilities(webrtc::MediaType::VIDEO);
  });
  for (const auto& codec : capabilities.codecs) {
    FfiVideoCodecCapability item;
    webrtc::SdpVideoFormat format(codec.name, codec.parameters);
    format.scalability_modes.assign(codec.scalability_modes.begin(), codec.scalability_modes.end());
    item.format = std::make_unique<NativeSdpVideoFormat>(std::move(format));
    item.clock_rate = codec.clock_rate.value_or(-1);
    item.preferred_payload_type = codec.preferred_payload_type.value_or(-1);
    for (const auto& feedback : codec.rtcp_feedback) {
      item.rtcp_feedback.push_back(FeedbackName(feedback));
    }
    result.push_back(std::move(item));
  }
  return result;
}

std::size_t transceiver_list_len(const NativeTransceiverList& list) noexcept {
  return list.state()->transceivers.size();
}

std::unique_ptr<NativeRtpTransceiver> transceiver_list_at(
    const NativeTransceiverList& list, std::size_t index) noexcept {
  if (index >= list.state()->transceivers.size()) {
    return nullptr;
  }
  return wrap_rtp_transceiver(list.state()->transceivers[index]);
}

std::unique_ptr<NativeRtpTransceiver> wrap_rtp_transceiver(
    webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> transceiver) noexcept {
  if (!transceiver) {
    return nullptr;
  }
  auto state = std::make_unique<NativeRtpTransceiver::State>();
  state->transceiver = std::move(transceiver);
  return std::make_unique<NativeRtpTransceiver>(std::move(state));
}

bool rtp_receiver_request_keyframe(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver) noexcept {
  if (!peer.worker_thread()) return false;
  const auto receivers = peer.peer()->GetReceivers();
  if (std::find(receivers.begin(), receivers.end(), receiver.state()->receiver) ==
      receivers.end()) return false;
  auto track = VideoTrack(receiver.state()->receiver->track());
  if (!track || track->state() != webrtc::MediaStreamTrackInterface::kLive ||
      !track->GetSource() || !track->GetSource()->remote()) return false;
  peer.worker_thread()->BlockingCall([&] { track->GetSource()->GenerateKeyFrame(); });
  return true;
}

static std::unique_ptr<NativeEncodedVideoSink> AttachEncodedVideoSink(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver,
    bool ordered, std::optional<rust::Box<RustEncodedVideoObserver>> observer) noexcept {
  if (!peer.worker_thread()) return nullptr;
  // Receiver handles may outlive their original peer or come from another peer.
  const auto receivers = peer.peer()->GetReceivers();
  if (std::find(receivers.begin(), receivers.end(), receiver.state()->receiver) ==
      receivers.end()) return nullptr;
  if (!peer.reserve_encoded_receiver(receiver.state()->receiver->id())) return nullptr;
  auto track = VideoTrack(receiver.state()->receiver->track());
  if (!track || !track->GetSource() || !track->GetSource()->remote() ||
      (ordered && !track->GetSource()->SupportsEncodedOutput())) return nullptr;
  auto state = std::make_unique<NativeEncodedVideoSink::State>();
  state->collector = webrtc::make_ref_counted<EncodedFrameCollector>(
      peer.readiness(), ordered, std::move(observer));
  if (ordered) state->source = track->GetSource();
  state->worker = peer.worker_thread();
  peer.worker_thread()->BlockingCall([&] {
    peer.frame_transformer_hosts().Receiver(peer, receiver.state()->receiver)->SetCollector(state->collector);
    if (state->source) state->source->AddEncodedSink(state->collector.get());
  });
  return std::make_unique<NativeEncodedVideoSink>(std::move(state));
}

std::unique_ptr<NativeEncodedVideoSink> rtp_receiver_attach_encoded_video_sink(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver,
    bool ordered) noexcept {
  return AttachEncodedVideoSink(peer, receiver, ordered, std::nullopt);
}
std::unique_ptr<NativeEncodedVideoSink> rtp_receiver_attach_observed_encoded_video_sink(
    const NativePeerConnection& peer, const NativeRtpReceiver& receiver,
    rust::Box<RustEncodedVideoObserver> observer) noexcept {
  return AttachEncodedVideoSink(peer, receiver, true, std::move(observer));
}

FfiEncodedVideoFrame encoded_video_sink_take_frame(
    const NativeEncodedVideoSink& sink) noexcept {
  return sink.state()->collector->Take();
}
std::uint64_t encoded_video_sink_dropped_frames(
    const NativeEncodedVideoSink& sink) noexcept {
  return sink.state()->collector->DroppedFrames();
}
bool close_encoded_video_sink(const NativeEncodedVideoSink& sink) noexcept {
  auto& state = *sink.state();
  if (state.closed.exchange(true)) return true;
  if (state.worker && state.source) {
    state.worker->BlockingCall([&] {
      state.source->RemoveEncodedSink(state.collector.get());
    });
  }
  state.collector->Deactivate();
  return true;
}

std::unique_ptr<NativeRtpReceiver> wrap_rtp_receiver(
    webrtc::scoped_refptr<webrtc::RtpReceiverInterface> receiver,
    webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> owner) noexcept {
  if (!receiver) {
    return nullptr;
  }
  auto state = std::make_unique<NativeRtpReceiver::State>();
  state->receiver = std::move(receiver);
  state->owner = std::move(owner);
  return std::make_unique<NativeRtpReceiver>(std::move(state));
}

namespace { bool VideoParameterConsumptionSafe(const std::vector<webrtc::RtpEncodingParameters>&); }
bool transceiver_init_consumption_safe(const NativePeerConnection& peer,
    const NativeRtpTransceiverInit& init, bool video) noexcept {
  // The pinned AddTransceiver overloads RTC_CHECK Unified Plan, and later
  // consumers assume a real direction and safely representable video math.
  const auto direction = static_cast<int>(init.value().direction);
  return peer.peer()->GetConfiguration().sdp_semantics == webrtc::SdpSemantics::kUnifiedPlan &&
      direction >= 0 && direction <= 4 &&
      (!video || VideoParameterConsumptionSafe(init.value().send_encodings));
}
std::unique_ptr<NativeRtpTransceiver> peer_add_transceiver_value(
    const NativePeerConnection& peer, std::int32_t kind, const NativeRtpTransceiverInit& init,
    bool& valid, std::unique_ptr<NativeRtcError>& error) noexcept {
  valid = transceiver_init_consumption_safe(peer, init, kind == static_cast<int>(webrtc::MediaType::VIDEO));
  if (!valid) return nullptr;
  auto result = peer.peer()->AddTransceiver(static_cast<webrtc::MediaType>(kind), init.value());
  if (!result.ok()) { error = std::make_unique<NativeRtcError>(result.MoveError()); return nullptr; }
  return wrap_rtp_transceiver(result.MoveValue());
}
std::unique_ptr<NativeRtpTransceiver> peer_add_video_transceiver_value(
    const NativePeerConnection& peer, const NativeVideoTrack& track, const NativeRtpTransceiverInit& init,
    bool& valid, std::unique_ptr<NativeRtcError>& error) noexcept {
  valid = transceiver_init_consumption_safe(peer, init, true);
  if (!valid) return nullptr;
  auto result = peer.peer()->AddTransceiver(track.state()->track, init.value());
  if (!result.ok()) { error = std::make_unique<NativeRtcError>(result.MoveError()); return nullptr; }
  return wrap_rtp_transceiver(result.MoveValue());
}
std::unique_ptr<NativeRtpTransceiver> peer_add_media_transceiver_value(
    const NativePeerConnection& peer, const NativeMediaTrack& track, const NativeRtpTransceiverInit& init,
    bool& valid, std::unique_ptr<NativeRtcError>& error) noexcept {
  valid = transceiver_init_consumption_safe(peer, init,
      track.track()->kind() == webrtc::MediaStreamTrackInterface::kVideoKind);
  if (!valid) return nullptr;
  auto result = peer.peer()->AddTransceiver(track.track(), init.value());
  if (!result.ok()) { error = std::make_unique<NativeRtcError>(result.MoveError()); return nullptr; }
  return wrap_rtp_transceiver(result.MoveValue());
}
std::unique_ptr<NativeRtpTransceiver> peer_add_video_transceiver(
    const NativePeerConnection& peer, const NativeVideoTrack& track,
    std::uint8_t direction, rust::Slice<const rust::String> rids,
    std::uint8_t& error_type, rust::String& error) noexcept {
  const auto parsed = Direction(direction);
  if (!parsed) {
    error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_PARAMETER);
    error = "invalid RTP transceiver direction";
    return nullptr;
  }
  webrtc::RtpTransceiverInit init;
  init.direction = *parsed;
  for (const auto& rid : rids) {
    webrtc::RtpEncodingParameters encoding;
    encoding.rid = std::string(rid);
    init.send_encodings.push_back(std::move(encoding));
  }
  auto result = peer.peer()->AddTransceiver(track.state()->track, init);
  if (!result.ok()) {
    SetError(result.error(), error_type, error);
    return nullptr;
  }
  return wrap_rtp_transceiver(result.MoveValue());
}

bool peer_remove_track(const NativePeerConnection& peer,
                       const NativeRtpSender& sender,
                       std::uint8_t& error_type, rust::String& error) noexcept {
  auto result = peer.peer()->RemoveTrackOrError(sender.state()->sender);
  if (!result.ok()) {
    SetError(result, error_type, error);
    return false;
  }
  return true;
}

bool rtp_sender_set_media_track(const NativeRtpSender& sender, const NativeMediaTrack& track) noexcept {
  return sender.sender()->SetTrack(track.track().get());
}
NativeRtpEndpoints::NativeRtpEndpoints() = default;
NativeRtpEndpoints::~NativeRtpEndpoints() = default;
std::unique_ptr<NativeRtpEndpoints> peer_endpoints(const NativePeerConnection& peer) noexcept {
  auto value = std::make_unique<NativeRtpEndpoints>();
  peer.signaling_thread()->BlockingCall([&] {
    value->senders = peer.peer()->GetSenders();
    value->receivers = peer.peer()->GetReceivers();
    value->unified = peer.peer()->GetConfiguration().sdp_semantics == webrtc::SdpSemantics::kUnifiedPlan;
    if (value->unified) value->transceivers = peer.peer()->GetTransceivers();
  });
  return value;
}
std::size_t endpoint_list_len(const NativeRtpEndpoints& v, std::uint8_t kind) noexcept {
  return kind == 0 ? v.senders.size() : kind == 1 ? v.receivers.size() : v.transceivers.size();
}
bool endpoint_list_unified(const NativeRtpEndpoints& v) noexcept { return v.unified; }
std::unique_ptr<NativeRtpSender> endpoint_list_sender(const NativeRtpEndpoints& v, std::size_t i) noexcept {
  if (i >= v.senders.size()) return nullptr;
  auto state = std::make_unique<NativeRtpSender::State>();
  state->sender = v.senders[i];
  for (const auto& owner : v.transceivers)
    if (owner->sender() == state->sender) { state->owner = owner; break; }
  return std::make_unique<NativeRtpSender>(std::move(state));
}
std::unique_ptr<NativeRtpReceiver> endpoint_list_receiver(const NativeRtpEndpoints& v, std::size_t i) noexcept {
  if (i >= v.receivers.size()) return nullptr;
  webrtc::scoped_refptr<webrtc::RtpTransceiverInterface> parent;
  for (const auto& owner : v.transceivers)
    if (owner->receiver() == v.receivers[i]) { parent = owner; break; }
  return wrap_rtp_receiver(v.receivers[i], std::move(parent));
}
std::unique_ptr<NativeRtpTransceiver> endpoint_list_transceiver(const NativeRtpEndpoints& v, std::size_t i) noexcept {
  if (i >= v.transceivers.size()) return nullptr;
  auto state = std::make_unique<NativeRtpTransceiver::State>();
  state->transceiver = v.transceivers[i];
  return std::make_unique<NativeRtpTransceiver>(std::move(state));
}
std::unique_ptr<NativeRtpSender> rtp_transceiver_sender(
    const NativeRtpTransceiver& transceiver) noexcept {
  auto state = std::make_unique<NativeRtpSender::State>();
  state->sender = transceiver.state()->transceiver->sender();
  state->owner = transceiver.state()->transceiver;
  return std::make_unique<NativeRtpSender>(std::move(state));
}

std::unique_ptr<NativeRtpReceiver> rtp_transceiver_receiver(
    const NativeRtpTransceiver& transceiver) noexcept {
  auto state = std::make_unique<NativeRtpReceiver::State>();
  state->receiver = transceiver.state()->transceiver->receiver();
  state->owner = transceiver.state()->transceiver;
  return std::make_unique<NativeRtpReceiver>(std::move(state));
}

std::uint8_t rtp_transceiver_direction(
    const NativeRtpTransceiver& transceiver) noexcept {
  return static_cast<std::uint8_t>(
      transceiver.state()->transceiver->direction());
}

std::int8_t rtp_transceiver_current_direction(
    const NativeRtpTransceiver& transceiver) noexcept {
  const auto direction = transceiver.state()->transceiver->current_direction();
  return direction ? static_cast<std::int8_t>(*direction) : -1;
}

std::int32_t rtp_transceiver_direction_value(
    const NativeRtpTransceiver& transceiver) noexcept {
  return static_cast<std::int32_t>(transceiver.state()->transceiver->direction());
}

bool rtp_transceiver_current_direction_value(
    const NativeRtpTransceiver& transceiver, std::int32_t& direction) noexcept {
  const auto value = transceiver.state()->transceiver->current_direction();
  if (!value) return false;
  direction = static_cast<std::int32_t>(*value);
  return true;
}

std::unique_ptr<NativeRtcError> rtp_transceiver_set_direction_value(
    const NativeRtpTransceiver& transceiver, std::int32_t direction,
    bool& safe) noexcept {
  // SetDirectionWithError accepts unknowns on an active transceiver, but
  // deferred CreateOffer direction-based codec selection CHECKs. Guard only
  // that consumer; do not narrow standalone or header capability metadata.
  safe = direction >= static_cast<std::int32_t>(webrtc::RtpTransceiverDirection::kSendRecv) &&
         direction <= static_cast<std::int32_t>(webrtc::RtpTransceiverDirection::kStopped);
  if (!safe) return nullptr;
  return std::make_unique<NativeRtcError>(
      transceiver.state()->transceiver->SetDirectionWithError(
          static_cast<webrtc::RtpTransceiverDirection>(direction)));
}
std::unique_ptr<NativeRtcError> rtp_transceiver_stop_standard(
    const NativeRtpTransceiver& transceiver) noexcept {
  return std::make_unique<NativeRtcError>(
      transceiver.state()->transceiver->StopStandard());
}

std::int32_t rtp_transceiver_media_type(const NativeRtpTransceiver& transceiver) noexcept {
  return static_cast<std::int32_t>(transceiver.state()->transceiver->media_type());
}
bool rtp_transceiver_stopping(const NativeRtpTransceiver& transceiver) noexcept {
  return transceiver.state()->transceiver->stopping();
}
bool rtp_transceiver_receptive(const NativeRtpTransceiver& transceiver) noexcept {
  return transceiver.state()->transceiver->receptive();
}
bool rtp_transceiver_fired_direction(
    const NativeRtpTransceiver& transceiver, std::int32_t& direction) noexcept {
  const auto value = transceiver.state()->transceiver->fired_direction();
  if (!value) return false;
  direction = static_cast<std::int32_t>(*value);
  return true;
}
bool rtp_transceiver_sframe_enabled(
    const NativeRtpTransceiver& transceiver, bool& enabled) noexcept {
  const auto value = transceiver.state()->transceiver->SframeEnabled();
  if (!value) return false;
  enabled = *value;
  return true;
}

std::unique_ptr<NativeRtpCapabilities> rtp_transceiver_codec_preferences_value(
    const NativeRtpTransceiver& transceiver) noexcept {
  // The container is private FFI transport for the complete copied SDK vector.
  // Neither header extensions nor FEC are fabricated as transceiver capabilities.
  webrtc::RtpCapabilities result;
  result.codecs = transceiver.state()->transceiver->codec_preferences();
  return std::make_unique<NativeRtpCapabilities>(std::move(result));
}

std::unique_ptr<NativeRtpCapabilities> rtp_transceiver_header_capabilities_value(
    const NativeRtpTransceiver& transceiver, bool negotiated) noexcept {
  webrtc::RtpCapabilities result;
  result.header_extensions = negotiated
      ? transceiver.state()->transceiver->GetNegotiatedHeaderExtensions()
      : transceiver.state()->transceiver->GetHeaderExtensionsToNegotiate();
  return std::make_unique<NativeRtpCapabilities>(std::move(result));
}

std::unique_ptr<NativeRtcError> rtp_transceiver_set_codec_preferences_value(
    const NativeRtpTransceiver& transceiver,
    const NativeRtpCapabilities& values) noexcept {
  // The SDK consumes a mutable span, and retains its own preference copies.
  // Preserve all caller metadata, leaving semantic validation/deduplication to
  // the SDK. No factory lookup, format reconstruction or enum stringification.
  auto requested = values.value().codecs;
  auto result = transceiver.state()->transceiver->SetCodecPreferences(requested);
  return std::make_unique<NativeRtcError>(std::move(result));
}
std::unique_ptr<NativeRtcError> rtp_transceiver_set_header_capabilities_value(
    const NativeRtpTransceiver& transceiver,
    const NativeRtpCapabilities& values) noexcept {
  auto requested = values.value().header_extensions;
  auto result = transceiver.state()->transceiver->SetHeaderExtensionsToNegotiate(requested);
  return std::make_unique<NativeRtcError>(std::move(result));
}

rust::Vec<FfiRtpHeaderExtension> rtp_transceiver_header_extensions(
    const NativeRtpTransceiver& transceiver, bool negotiated) noexcept {
  const auto extensions = negotiated
      ? transceiver.state()->transceiver->GetNegotiatedHeaderExtensions()
      : transceiver.state()->transceiver->GetHeaderExtensionsToNegotiate();
  rust::Vec<FfiRtpHeaderExtension> result;
  for (const auto& extension : extensions) {
    result.push_back(FfiRtpHeaderExtension{
        extension.uri, extension.preferred_id.value_or(-1),
        extension.preferred_encrypt, static_cast<std::uint8_t>(extension.direction)});
  }
  return result;
}

bool rtp_transceiver_set_header_extensions(
    const NativeRtpTransceiver& transceiver,
    rust::Slice<const FfiRtpHeaderExtension> extensions,
    std::uint8_t& error_type, rust::String& error) noexcept {
  std::vector<webrtc::RtpHeaderExtensionCapability> requested;
  for (const auto& extension : extensions) {
    if (extension.direction > static_cast<std::uint8_t>(webrtc::RtpTransceiverDirection::kStopped)) {
      error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_PARAMETER);
      error = "invalid RTP header extension direction";
      return false;
    }
    webrtc::RtpHeaderExtensionCapability value;
    value.uri = std::string(extension.uri);
    value.direction = static_cast<webrtc::RtpTransceiverDirection>(extension.direction);
    requested.push_back(std::move(value));
  }
  const auto result = transceiver.state()->transceiver->SetHeaderExtensionsToNegotiate(requested);
  if (!result.ok()) {
    SetError(result, error_type, error);
    return false;
  }
  return true;
}

bool rtp_transceiver_stopped(
    const NativeRtpTransceiver& transceiver) noexcept {
  return transceiver.state()->transceiver->stopped();
}

bool rtp_transceiver_mid(const NativeRtpTransceiver& transceiver,
                         rust::String& mid) noexcept {
  const auto value = transceiver.state()->transceiver->mid();
  if (!value) {
    return false;
  }
  mid = *value;
  return true;
}

bool rtp_transceiver_set_video_codec_preferences(
    const NativeRtpTransceiver& transceiver, const NativePeerConnectionFactory& factory,
    rust::Slice<const FfiCodecFormat> formats, std::uint8_t& error_type,
    rust::String& error) noexcept {
  if (!factory.factory() || !factory.signaling_thread()) {
    error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_STATE);
    error = "peer is unavailable";
    return false;
  }
  const auto capabilities = factory.signaling_thread()->BlockingCall([&] {
    return factory.factory()->GetRtpSenderCapabilities(webrtc::MediaType::VIDEO);
  });
  std::vector<webrtc::RtpCodecCapability> selected;
  for (const auto& format : formats) {
    std::map<std::string, std::string> parameters;
    for (const auto& item : format.parameters) {
      if (!parameters.emplace(std::string(item.key), std::string(item.value)).second) {
        error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_PARAMETER);
        error = "duplicate codec parameter";
        return false;
      }
    }
    const auto it = std::find_if(capabilities.codecs.begin(), capabilities.codecs.end(),
        [&](const auto& codec) {
          return codec.name == std::string(format.name) && codec.parameters == parameters;
        });
    if (it == capabilities.codecs.end() ||
        std::find(selected.begin(), selected.end(), *it) != selected.end()) {
      error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_PARAMETER);
      error = "unknown or repeated video codec capability";
      return false;
    }
    selected.push_back(*it);
  }
  auto result = factory.signaling_thread()->BlockingCall([&] {
    return transceiver.state()->transceiver->SetCodecPreferences(selected);
  });
  if (!result.ok()) {
    SetError(result, error_type, error);
    return false;
  }
  return true;
}

bool rtp_transceiver_set_audio_codec_preferences(
    const NativeRtpTransceiver& transceiver,
    const NativePeerConnectionFactory& factory,
    rust::Slice<const FfiCodecFormat> formats, std::uint8_t& error_type,
    rust::String& error) noexcept {
  if (!factory.factory() || !factory.signaling_thread()) {
    error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_STATE);
    error = "peer is unavailable";
    return false;
  }
  const auto capabilities = factory.signaling_thread()->BlockingCall([&] {
    return factory.factory()->GetRtpSenderCapabilities(webrtc::MediaType::AUDIO);
  });
  std::vector<webrtc::RtpCodecCapability> selected;
  for (const auto& format : formats) {
    std::map<std::string, std::string> parameters;
    for (const auto& item : format.parameters) {
      if (!parameters.emplace(std::string(item.key), std::string(item.value)).second) {
        error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_PARAMETER);
        error = "duplicate codec parameter";
        return false;
      }
    }
    const auto it = std::find_if(capabilities.codecs.begin(), capabilities.codecs.end(),
        [&](const auto& codec) {
          return codec.name == std::string(format.name) && codec.parameters == parameters;
        });
    if (it == capabilities.codecs.end() ||
        std::find(selected.begin(), selected.end(), *it) != selected.end()) {
      error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_PARAMETER);
      error = "unknown or repeated audio codec capability";
      return false;
    }
    selected.push_back(*it);
  }
  auto outcome = factory.signaling_thread()->BlockingCall([&] {
    return transceiver.state()->transceiver->SetCodecPreferences(selected);
  });
  if (!outcome.ok()) {
    SetError(outcome, error_type, error);
    return false;
  }
  return true;
}

bool rtp_transceiver_stop(const NativeRtpTransceiver& transceiver,
                          std::uint8_t& error_type,
                          rust::String& error) noexcept {
  auto result = transceiver.state()->transceiver->StopStandard();
  if (!result.ok()) {
    SetError(result, error_type, error);
    return false;
  }
  return true;
}

bool rtp_transceiver_set_direction(const NativeRtpTransceiver& transceiver,
                                   std::uint8_t direction,
                                   std::uint8_t& error_type,
                                   rust::String& error) noexcept {
  const auto parsed = Direction(direction);
  if (!parsed) {
    error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_PARAMETER);
    error = "invalid RTP transceiver direction";
    return false;
  }
  auto result = transceiver.state()->transceiver->SetDirectionWithError(*parsed);
  if (!result.ok()) {
    SetError(result, error_type, error);
    return false;
  }
  return true;
}

std::size_t rtp_sender_identity(const NativeRtpSender& sender) noexcept {
  return reinterpret_cast<std::uintptr_t>(sender.state()->sender.get());
}
std::unique_ptr<NativeRtpSender> rtp_sender_copy(const NativeRtpSender& sender) noexcept {
  auto state = std::make_unique<NativeRtpSender::State>();
  state->sender = sender.state()->sender;
  state->owner = sender.state()->owner;
  return std::make_unique<NativeRtpSender>(std::move(state));
}
bool rtp_sender_same_resource(const NativeRtpSender& first, const NativeRtpSender& second) noexcept {
  return first.sender().get() == second.sender().get();
}
bool rtp_receiver_same_resource(const NativeRtpReceiver& first, const NativeRtpReceiver& second) noexcept {
  return first.receiver().get() == second.receiver().get();
}
bool rtp_transceiver_same_resource(const NativeRtpTransceiver& first, const NativeRtpTransceiver& second) noexcept {
  return first.state()->transceiver.get() == second.state()->transceiver.get();
}
std::size_t dtmf_sender_identity(const NativeDtmfSender& sender) noexcept {
  return reinterpret_cast<std::uintptr_t>(sender.state()->sender.get());
}
std::unique_ptr<NativeDtmfSender> rtp_sender_dtmf(const NativeRtpSender& sender,
                                                const NativePeerConnection& peer) noexcept {
  auto native = sender.state()->sender->GetDtmfSender();
  if (!native) return nullptr;
  auto state = std::make_unique<NativeDtmfSender::State>();
  state->readiness = peer.readiness();
  state->sender = std::move(native);
  return std::make_unique<NativeDtmfSender>(std::move(state));
}
bool dtmf_sender_can_insert(const NativeDtmfSender& sender) noexcept {
  return sender.state()->sender->CanInsertDtmf();
}
std::uint8_t dtmf_sender_insert(const NativeDtmfSender& sender, rust::Str tones,
    std::int32_t duration, std::int32_t gap, bool has_comma_delay,
    std::int32_t comma_delay) noexcept {
  const std::string input(tones);
  const int comma = has_comma_delay ? comma_delay :
      webrtc::DtmfSenderInterface::kDtmfDefaultCommaDelayMs;
  // Pinned native checks accept duration 40..6000 and gaps >=30, despite the
  // interface comments' stricter values. Invalid native inputs are delegated
  // unchanged and return false. Only a normal tone consumes duration + gap;
  // empty/unrecognized/comma-only input must not receive an unused upper cap.
  // Accepted parameters and tones are immutable between guarded InsertDtmf
  // calls, retaining this proof for the asynchronously consumed signed sum.
  if (duration >= 40 && duration <= 6000 && gap >= 30 && comma >= 30 &&
      input.find_first_of("0123456789ABCDabcd#*") != std::string::npos &&
      gap > std::numeric_limits<int>::max() - duration) return 2;
  const bool accepted = has_comma_delay
      ? sender.state()->sender->InsertDtmf(input, duration, gap, comma_delay)
      : sender.state()->sender->InsertDtmf(input, duration, gap);
  return accepted ? 1 : 0;
}
rust::String dtmf_sender_tones(const NativeDtmfSender& sender) noexcept {
  return rust::String(sender.state()->sender->tones());
}
std::int32_t dtmf_sender_duration(const NativeDtmfSender& sender) noexcept {
  return sender.state()->sender->duration();
}
std::int32_t dtmf_sender_inter_tone_gap(const NativeDtmfSender& sender) noexcept {
  return sender.state()->sender->inter_tone_gap();
}
std::int32_t dtmf_sender_comma_delay(const NativeDtmfSender& sender) noexcept {
  return sender.state()->sender->comma_delay();
}
bool dtmf_sender_register_observer(const NativeDtmfSender& sender, std::size_t capacity) noexcept {
  auto& state = *sender.state();
  if (!capacity || state.registered) return false;
  {
    std::lock_guard<std::mutex> lock(state.mutex);
    state.capacity = capacity;
    state.events.clear();
    state.dropped = 0;
  }
  state.sender->RegisterObserver(&state);
  state.registered = true;
  return true;
}
void dtmf_sender_unregister_observer(const NativeDtmfSender& sender) noexcept {
  auto& state = *sender.state();
  if (!state.registered) return;
  state.sender->UnregisterObserver();
  state.registered = false;
}
FfiDtmfToneChange dtmf_sender_take_tone_change(const NativeDtmfSender& sender) noexcept {
  auto& state = *sender.state();
  std::lock_guard<std::mutex> lock(state.mutex);
  if (state.events.empty()) return {};
  auto event = std::move(state.events.front());
  state.events.pop_front();
  return event;
}
std::uint64_t dtmf_sender_take_dropped(const NativeDtmfSender& sender) noexcept {
  auto& state = *sender.state();
  std::lock_guard<std::mutex> lock(state.mutex);
  return std::exchange(state.dropped, 0);
}

std::unique_ptr<NativeDtlsTransport> rtp_sender_dtls(
    const NativeRtpSender& sender, const NativePeerConnection& peer) noexcept {
  return wrap_dtls_transport(sender.state()->sender->dtls_transport(), peer.network_thread(), peer.readiness());
}
std::unique_ptr<NativeDtlsTransport> rtp_receiver_dtls(
    const NativeRtpReceiver& receiver, const NativePeerConnection& peer) noexcept {
  return wrap_dtls_transport(receiver.state()->receiver->dtls_transport(), peer.network_thread(), peer.readiness());
}
bool media_type_to_string(std::int32_t kind, rust::String& name) noexcept {
  // UNSUPPORTED/ANY are real enum alternatives but the pinned converter's
  // default branch asserts. Guard at the actual converter consumption.
  if (kind < 0 || kind > 2) return false;
  name = webrtc::MediaTypeToString(static_cast<webrtc::MediaType>(kind));
  return true;
}
std::unique_ptr<NativeRtpEncodingList> rtp_sender_initial_encodings(const NativeRtpSender& sender) noexcept {
  // The sender proxy executes this getter on signaling and returns SDK value
  // copies. Subsequent list/value ownership has no engine origin affinity.
  return std::make_unique<NativeRtpEncodingList>(sender.state()->sender->init_send_encodings());
}
namespace {
bool VideoParameterConsumptionSafe(const std::vector<webrtc::RtpEncodingParameters>& encodings) {
  for (const auto& encoding : encodings) {
    if (encoding.max_framerate && (!std::isfinite(*encoding.max_framerate) || *encoding.max_framerate >= static_cast<double>(INT_MAX) + 1.0)) return false;
    if (encoding.scale_resolution_down_by && !std::isfinite(*encoding.scale_resolution_down_by)) return false;
    if (encoding.scale_resolution_down_to) {
      const auto resolution = *encoding.scale_resolution_down_to;
      const auto pixels = static_cast<std::int64_t>(resolution.width) * resolution.height;
      if (pixels > INT_MAX || pixels < INT_MIN) return false;
    }
  }
  return true;
}
bool SenderParameterConsumptionSafe(const NativeRtpSender& sender, const webrtc::RtpParameters& value) {
  return sender.sender()->media_type() != webrtc::MediaType::VIDEO || VideoParameterConsumptionSafe(value.encodings);
}
}  // namespace
struct NativeSetParametersOperation::State {
  std::mutex mutex;
  bool completed = false;
  std::unique_ptr<NativeRtcError> result;
};
bool set_parameters_operation_ready(const NativeSetParametersOperation& operation) noexcept {
  std::lock_guard lock(operation.state()->mutex); return operation.state()->completed;
}
std::unique_ptr<NativeRtcError> set_parameters_operation_take(const NativeSetParametersOperation& operation) noexcept {
  std::lock_guard lock(operation.state()->mutex); return std::move(operation.state()->result);
}
std::unique_ptr<NativeRtcError> rtp_sender_set_parameters_value(const NativeRtpSender& sender, const NativeRtpParameters& parameters, bool& valid) noexcept {
  valid = SenderParameterConsumptionSafe(sender, parameters.value());
  if (!valid) return nullptr;
  return std::make_unique<NativeRtcError>(sender.sender()->SetParameters(parameters.value()));
}
std::shared_ptr<NativeSetParametersOperation> rtp_sender_set_parameters_async(const NativeRtpSender& sender, const NativeRtpParameters& parameters, bool& valid) noexcept {
  valid = SenderParameterConsumptionSafe(sender, parameters.value());
  if (!valid) return nullptr;
  auto state = std::make_shared<NativeSetParametersOperation::State>();
  sender.sender()->SetParametersAsync(parameters.value(), [state](webrtc::RTCError error) {
    std::lock_guard lock(state->mutex);
    state->result = std::make_unique<NativeRtcError>(std::move(error)); state->completed = true;
  });
  return std::make_shared<NativeSetParametersOperation>(std::move(state));
}
std::unique_ptr<NativeRtpParameters> rtp_sender_parameters_value(const NativeRtpSender& sender) noexcept {
  // NativeRtpSender holds the stock signaling-thread proxy; the blocking
  // getter returns the entire SDK value and starts its actual transaction.
  // Do not consult/update the legacy overlay cache or call GetParameters twice.
  return std::make_unique<NativeRtpParameters>(sender.state()->sender->GetParameters());
}
std::uint32_t rtp_sender_ssrc(const NativeRtpSender& sender) noexcept {
  return sender.state()->sender->ssrc();
}
std::int32_t rtp_sender_media_type(const NativeRtpSender& sender) noexcept {
  return static_cast<std::int32_t>(sender.state()->sender->media_type());
}
rust::Vec<rust::String> rtp_sender_stream_ids(const NativeRtpSender& sender) noexcept {
  rust::Vec<rust::String> result;
  for (const auto& id : sender.state()->sender->stream_ids()) result.push_back(rust::String(id));
  return result;
}
rust::Vec<FfiRtpByteString> rtp_sender_stream_id_bytes(const NativeRtpSender& sender) noexcept {
  rust::Vec<FfiRtpByteString> result;
  for (const auto& id : sender.state()->sender->stream_ids()) {
    FfiRtpByteString value;
    value.bytes.reserve(id.size());
    for (unsigned char byte : id) value.bytes.push_back(byte);
    result.push_back(std::move(value));
  }
  return result;
}
void rtp_sender_set_stream_id_bytes(const NativeRtpSender& sender, rust::Slice<const FfiRtpByteString> ids) noexcept {
  std::vector<std::string> native;
  for (const auto& id : ids) native.emplace_back(reinterpret_cast<const char*>(id.bytes.data()), id.bytes.size());
  sender.state()->sender->SetStreams(native);
}
void rtp_sender_set_streams(const NativeRtpSender& sender,
    rust::Slice<const rust::String> ids) noexcept {
  std::vector<std::string> native;
  for (const auto& id : ids) native.emplace_back(std::string(id));
  sender.state()->sender->SetStreams(native);
}
rust::String rtp_sender_id(const NativeRtpSender& sender) noexcept {
  return sender.state()->sender->id();
}

std::unique_ptr<NativeRtcError> rtp_sender_generate_keyframe(
    const NativeRtpSender& sender, const NativePeerConnection& peer,
    rust::Slice<const FfiRtpByteString> rids) noexcept {
  if (!peer.peer() || !peer.signaling_thread()) return nullptr;
  std::vector<std::string> names;
  names.reserve(rids.size());
  for (const auto& rid : rids)
    names.emplace_back(reinterpret_cast<const char*>(rid.bytes.data()), rid.bytes.size());
  // The retained native proxy marshals to signaling; explicit ownership of the
  // peer/factory graph keeps that role alive for the complete native call.
  return std::make_unique<NativeRtcError>(sender.sender()->GenerateKeyFrame(names));
}
bool rtp_sender_request_keyframe(const NativeRtpSender& sender,
                                 const NativePeerConnection& peer,
                                 rust::Slice<const rust::String> rids,
                                 std::uint8_t& error_type,
                                 rust::String& error) noexcept {
  if (!peer.peer() || !peer.signaling_thread()) {
    error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_STATE);
    error = "peer is unavailable";
    return false;
  }
  std::vector<std::string> names;
  names.reserve(rids.size());
  for (const auto& rid : rids) {
    names.emplace_back(std::string(rid));
  }
  const auto result = peer.signaling_thread()->BlockingCall([&] {
    return sender.state()->sender->GenerateKeyFrame(names);
  });
  if (!result.ok()) {
    SetError(result, error_type, error);
    return false;
  }
  return true;
}

bool rtp_sender_get_parameters(const NativeRtpSender& sender,
                               FfiSenderParameters& output) noexcept {
  auto& state = *sender.state();
  if (!state.sender) {
    return false;
  }
  state.parameters = state.sender->GetParameters();
  output.transaction_id = state.parameters->transaction_id;
  output.encodings.clear();
  for (const auto& encoding : state.parameters->encodings) {
    FfiSenderEncoding copied;
    copied.rid = encoding.rid;
    copied.active = encoding.active;
    copied.has_max_bitrate = encoding.max_bitrate_bps.has_value();
    copied.max_bitrate_bps = encoding.max_bitrate_bps.value_or(0);
    copied.has_max_framerate = encoding.max_framerate.has_value();
    copied.max_framerate = encoding.max_framerate.value_or(0);
    copied.has_scale_by = encoding.scale_resolution_down_by.has_value();
    copied.scale_by = encoding.scale_resolution_down_by.value_or(0);
    copied.has_scale_to = encoding.scale_resolution_down_to.has_value();
    copied.scale_to_width = encoding.scale_resolution_down_to
                                ? encoding.scale_resolution_down_to->width : 0;
    copied.scale_to_height = encoding.scale_resolution_down_to
                                 ? encoding.scale_resolution_down_to->height : 0;
    copied.has_scalability_mode = encoding.scalability_mode.has_value();
    copied.scalability_mode = encoding.scalability_mode.value_or("");
    output.encodings.push_back(std::move(copied));
  }
  return true;
}

bool rtp_sender_set_parameters(const NativeRtpSender& sender,
                               const FfiSenderParameters& input,
                               std::uint8_t& error_type,
                               rust::String& error) noexcept {
  auto& state = *sender.state();
  if (!state.sender || !state.parameters ||
      input.transaction_id != state.parameters->transaction_id) {
    error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_STATE);
    error = "get current sender parameters before updating";
    return false;
  }
  if (input.encodings.size() != state.parameters->encodings.size()) {
    error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_MODIFICATION);
    error = "encoding count cannot change without renegotiation";
    return false;
  }
  auto updated = *state.parameters;
  for (std::size_t i = 0; i < input.encodings.size(); ++i) {
    const auto& source = input.encodings[i];
    auto& target = updated.encodings[i];
    if (std::string(source.rid) != target.rid) {
      error_type = static_cast<std::uint8_t>(webrtc::RTCErrorType::INVALID_MODIFICATION);
      error = "encoding RID cannot change without renegotiation";
      return false;
    }
    target.active = source.active;
    target.max_bitrate_bps = source.has_max_bitrate
                                 ? std::optional<int>(source.max_bitrate_bps) : std::nullopt;
    target.max_framerate = source.has_max_framerate
                               ? std::optional<double>(source.max_framerate) : std::nullopt;
    target.scale_resolution_down_by = source.has_scale_by
                                           ? std::optional<double>(source.scale_by) : std::nullopt;
    target.scale_resolution_down_to = source.has_scale_to
        ? std::optional<webrtc::Resolution>(webrtc::Resolution{
              source.scale_to_width, source.scale_to_height}) : std::nullopt;
    target.scalability_mode = source.has_scalability_mode
                                  ? std::optional<std::string>(std::string(source.scalability_mode))
                                  : std::nullopt;
  }
  auto result = state.sender->SetParameters(updated);
  if (!result.ok()) {
    SetError(result, error_type, error);
    return false;
  }
  state.parameters.reset();  // A successful update consumes the transaction.
  return true;
}
std::unique_ptr<NativeVideoTrack> rtp_sender_track(
    const NativeRtpSender& sender) noexcept {
  auto track = VideoTrack(sender.state()->sender->track());
  if (!track) {
    return nullptr;
  }
  auto state = std::make_unique<NativeVideoTrack::State>();
  state->track = std::move(track);
  return std::make_unique<NativeVideoTrack>(std::move(state));
}

bool rtp_sender_set_video_track(const NativeRtpSender& sender,
                                const NativeVideoTrack& track) noexcept {
  return sender.state()->sender->SetTrack(track.state()->track.get());
}

bool rtp_sender_clear_track(const NativeRtpSender& sender) noexcept {
  return sender.state()->sender->SetTrack(nullptr);
}
std::unique_ptr<NativeRtpSourceList> rtp_receiver_sources(
    const NativeRtpReceiver& receiver) noexcept {
  // The SDK receiver proxy supplies its actual SSRC/CSRC observations, without
  // inferred track associations or guessed joins to stats/packet events.
  return std::make_unique<NativeRtpSourceList>(
      receiver.state()->receiver->GetSources());
}
std::unique_ptr<NativeRtpParameters> rtp_receiver_parameters_value(
    const NativeRtpReceiver& receiver) noexcept {
  // The stock receiver proxy marshals GetParameters to the worker sequence.
  // Copy the whole SDK value, without a sender transaction or overlay cache.
  return std::make_unique<NativeRtpParameters>(
      receiver.state()->receiver->GetParameters());
}
bool rtp_receiver_set_parameters_value(
    const NativeRtpReceiver& receiver,
    const NativeRtpParameters& parameters) noexcept {
  // At this pin the stock proxy inherits the SDK's unsupported false result.
  // Do not invent success or replace this call with a sender operation.
  return receiver.state()->receiver->SetParameters(parameters.value());
}
bool rtp_receiver_set_jitter_buffer_minimum_delay(
    const NativeRtpReceiver& receiver, bool present, double seconds) noexcept {
  // JitterBufferDelay::GetMs uses saturated_cast<int>, which CHECKs on NaN.
  // Guard before even caching the value: consumption may happen after start.
  // All other values retain native saturation/clamping and optionality.
  if (present && std::isnan(seconds)) return false;
  receiver.state()->receiver->SetJitterBufferMinimumDelay(
      present ? std::optional<double>(seconds) : std::nullopt);
  return true;
}
std::int32_t rtp_receiver_media_type(const NativeRtpReceiver& receiver) noexcept {
  return static_cast<std::int32_t>(receiver.state()->receiver->media_type());
}
std::unique_ptr<NativeMediaStreams> rtp_receiver_streams(const NativeRtpReceiver& receiver) noexcept {
  return std::make_unique<NativeMediaStreams>(receiver.state()->receiver->streams());
}
rust::Vec<FfiRtpByteString> rtp_receiver_stream_id_bytes(const NativeRtpReceiver& receiver) noexcept {
  rust::Vec<FfiRtpByteString> out;
  for (const auto& id : receiver.state()->receiver->stream_ids()) {
    FfiRtpByteString item; for (unsigned char c : id) item.bytes.push_back(c); out.push_back(std::move(item));
  }
  return out;
}
rust::Vec<std::uint8_t> rtp_receiver_id_bytes(const NativeRtpReceiver& receiver) noexcept {
  rust::Vec<std::uint8_t> out; for (unsigned char c : receiver.state()->receiver->id()) out.push_back(c); return out;
}
rust::Vec<std::uint8_t> rtp_sender_id_bytes(const NativeRtpSender& sender) noexcept {
  rust::Vec<std::uint8_t> out; for (unsigned char c : sender.state()->sender->id()) out.push_back(c); return out;
}
rust::Vec<rust::String> rtp_receiver_stream_ids(const NativeRtpReceiver& receiver) noexcept {
  rust::Vec<rust::String> result;
  for (const auto& id : receiver.state()->receiver->stream_ids()) result.push_back(rust::String(id));
  return result;
}
rust::String rtp_receiver_id(const NativeRtpReceiver& receiver) noexcept {
  return receiver.state()->receiver->id();
}
std::unique_ptr<NativeVideoTrack> rtp_receiver_track(
    const NativeRtpReceiver& receiver) noexcept {
  auto track = VideoTrack(receiver.state()->receiver->track());
  if (!track) {
    return nullptr;
  }
  auto state = std::make_unique<NativeVideoTrack::State>();
  state->track = std::move(track);
  return std::make_unique<NativeVideoTrack>(std::move(state));
}

}  // namespace pulsebeam::webrtc_sys
