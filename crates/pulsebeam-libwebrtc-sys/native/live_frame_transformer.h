#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <utility>
#include "api/frame_transformer_interface.h"
#include "rust/cxx.h"

namespace webrtc { class RtpSenderInterface; class RtpReceiverInterface; }
namespace pulsebeam::webrtc_sys {
class NativePeerConnection;
class NativeRtpSender;
class NativeRtpReceiver;
struct RustFrameTransformerProvider;
struct FfiFrameTransformBinding;
struct FfiFrameTransformerStatistics;
struct FrameTransformProvenance;
struct FrameTransformCallbackState;
class NativeTransformableFrame;

// Caller provider ownership is independent of a host and of SDK callback queues.
class NativeFrameTransformer final {
 public:
  struct State;
  explicit NativeFrameTransformer(std::shared_ptr<State> state) : state_(std::move(state)) {}
  ~NativeFrameTransformer();
  const std::shared_ptr<State>& state() const { return state_; }
 private:
  std::shared_ptr<State> state_;
};
class NativeFrameTransformCompletion final {
 public:
  explicit NativeFrameTransformCompletion(std::shared_ptr<FrameTransformProvenance> request)
      : request_(std::move(request)) {}
  ~NativeFrameTransformCompletion();
  const std::shared_ptr<FrameTransformProvenance>& request() const { return request_; }
 private:
  std::shared_ptr<FrameTransformProvenance> request_;
};

// A retained capability for one actual SDK callback slot and caller epoch.
// It owns no provider userdata; unregister releases SDK references on its role.
class NativeFrameTransformCallback final {
 public:
  explicit NativeFrameTransformCallback(std::shared_ptr<FrameTransformCallbackState> state) : state_(std::move(state)) {}
  const std::shared_ptr<FrameTransformCallbackState>& state() const { return state_; }
 private:
  std::shared_ptr<FrameTransformCallbackState> state_;
};

// Install this stable SDK identity once per host. Sender metadata precedes
// caller transformation; receiver observation follows caller completion.
// Collector installation and SDK registration use the existing SDK owner role.
class NativeFrameTransformerHost : public webrtc::FrameTransformerInterface {
 public:
  struct State;
  NativeFrameTransformerHost(bool audio, bool sender,
      webrtc::scoped_refptr<webrtc::FrameTransformerInterface> sender_metadata = nullptr);
  ~NativeFrameTransformerHost() override;
  void SetCaller(const NativeFrameTransformer*);
  void SetCollector(webrtc::scoped_refptr<webrtc::FrameTransformerInterface>);
  void Transform(std::unique_ptr<webrtc::TransformableFrameInterface>) override;
  void RegisterTransformedFrameCallback(webrtc::scoped_refptr<webrtc::TransformedFrameCallback>) override;
  void UnregisterTransformedFrameCallback() override;
  void RegisterTransformedFrameSinkCallback(webrtc::scoped_refptr<webrtc::TransformedFrameCallback>, std::uint32_t) override;
  void UnregisterTransformedFrameSinkCallback(std::uint32_t) override;
 private:
  std::shared_ptr<State> state_;
};

// Explicit peer-owned installation resources, keyed by retained exact SDK
// endpoint identity. No global registry, reconstructed source key or getter echo.
class NativeFrameTransformerHosts final {
 public:
  struct State;
  NativeFrameTransformerHosts();
  ~NativeFrameTransformerHosts();
  webrtc::scoped_refptr<NativeFrameTransformerHost> Sender(const NativePeerConnection&, webrtc::scoped_refptr<webrtc::RtpSenderInterface>);
  webrtc::scoped_refptr<NativeFrameTransformerHost> Receiver(const NativePeerConnection&, webrtc::scoped_refptr<webrtc::RtpReceiverInterface>);
  void ClearSender(const webrtc::RtpSenderInterface*);
  void ClearReceiver(const webrtc::RtpReceiverInterface*);
  void Retire();
 private:
  std::unique_ptr<State> state_;
};
bool rtp_sender_select_frame_transformer(const NativePeerConnection&, const NativeRtpSender&, const NativeFrameTransformer*) noexcept;
bool rtp_receiver_select_frame_transformer(const NativePeerConnection&, const NativeRtpReceiver&, const NativeFrameTransformer*) noexcept;

std::unique_ptr<NativeFrameTransformer> frame_transformer_new(rust::Box<RustFrameTransformerProvider>, std::size_t) noexcept;
std::unique_ptr<NativeFrameTransformer> frame_transformer_copy(const NativeFrameTransformer&) noexcept;
bool frame_transformer_same(const NativeFrameTransformer&, const NativeFrameTransformer&) noexcept;
void frame_transformer_close(const NativeFrameTransformer&) noexcept;
bool frame_transformer_closed(const NativeFrameTransformer&) noexcept;
FfiFrameTransformerStatistics frame_transformer_statistics(const NativeFrameTransformer&) noexcept;
FfiFrameTransformBinding frame_transform_completion_binding(const NativeFrameTransformCompletion&) noexcept;
std::uint8_t frame_transform_complete(std::unique_ptr<NativeFrameTransformCompletion>, std::unique_ptr<NativeTransformableFrame>) noexcept;
bool frame_transform_short_circuit(const NativeFrameTransformCompletion&) noexcept;
std::unique_ptr<NativeFrameTransformCallback> frame_transform_completion_callback(const NativeFrameTransformCompletion&) noexcept;
std::unique_ptr<NativeFrameTransformCallback> frame_transform_callback_copy(const NativeFrameTransformCallback&) noexcept;
bool frame_transform_callback_same(const NativeFrameTransformCallback&, const NativeFrameTransformCallback&) noexcept;
FfiFrameTransformBinding frame_transform_callback_binding(const NativeFrameTransformCallback&) noexcept;
bool frame_transform_callback_retired(const NativeFrameTransformCallback&) noexcept;
bool frame_transform_callback_short_circuit(const NativeFrameTransformCallback&) noexcept;
std::uint8_t frame_transform_callback_forward(const NativeFrameTransformCallback&, std::unique_ptr<NativeTransformableFrame>) noexcept;
}  // namespace pulsebeam::webrtc_sys
