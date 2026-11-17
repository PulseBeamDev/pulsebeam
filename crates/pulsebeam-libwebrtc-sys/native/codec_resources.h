#pragma once

#include <algorithm>
#include <memory>
#include <optional>
#include <span>
#include <utility>

#include "api/video_codecs/video_decoder.h"
#include "api/video_codecs/video_decoder_factory.h"
#include "api/video_codecs/video_encoder.h"
#include "api/video_codecs/video_encoder_factory.h"

namespace pulsebeam::webrtc_sys {

class CoreBudgetEncoder final : public webrtc::VideoEncoder {
 public:
  CoreBudgetEncoder(std::unique_ptr<webrtc::VideoEncoder> encoder, int cores)
      : encoder_(std::move(encoder)), cores_(cores) {}

  void SetFecControllerOverride(webrtc::FecControllerOverride* controller) override {
    encoder_->SetFecControllerOverride(controller);
  }
  int32_t InitEncode(const webrtc::VideoCodec* codec, int32_t, size_t payload) override {
    return encoder_->InitEncode(codec, cores_, payload);
  }
  int InitEncode(const webrtc::VideoCodec* codec, const Settings& settings) override {
    auto bounded = settings;
    bounded.number_of_cores = cores_;
    bounded.encoder_thread_limit = std::min(
        bounded.encoder_thread_limit.value_or(cores_), cores_);
    return encoder_->InitEncode(codec, bounded);
  }
  int32_t RegisterEncodeCompleteCallback(webrtc::EncodedImageCallback* callback) override {
    return encoder_->RegisterEncodeCompleteCallback(callback);
  }
  int32_t Release() override { return encoder_->Release(); }
  int32_t Encode(const webrtc::VideoFrame& frame,
                 const std::vector<webrtc::VideoFrameType>* types) override {
    return encoder_->Encode(frame, types);
  }
  void SetRates(const RateControlParameters& parameters) override {
    encoder_->SetRates(parameters);
  }
  void SetComplexity(std::span<const webrtc::VideoCodecComplexity> complexity,
                     ComplexityControlMode mode) override {
    encoder_->SetComplexity(complexity, mode);
  }
  void OnPacketLossRateUpdate(float loss) override {
    encoder_->OnPacketLossRateUpdate(loss);
  }
  void OnRttUpdate(int64_t rtt_ms) override { encoder_->OnRttUpdate(rtt_ms); }
  void OnLossNotification(const LossNotification& notification) override {
    encoder_->OnLossNotification(notification);
  }
  EncoderInfo GetEncoderInfo() const override { return encoder_->GetEncoderInfo(); }

 private:
  std::unique_ptr<webrtc::VideoEncoder> encoder_;
  const int cores_;
};

class CoreBudgetDecoder final : public webrtc::VideoDecoder {
 public:
  CoreBudgetDecoder(std::unique_ptr<webrtc::VideoDecoder> decoder, int cores)
      : decoder_(std::move(decoder)), cores_(cores) {}

  bool Configure(const Settings& settings) override {
    auto bounded = settings;
    bounded.set_number_of_cores(cores_);
    return decoder_->Configure(bounded);
  }
  int32_t Decode(const webrtc::EncodedImage& image, int64_t render_ms) override {
    return decoder_->Decode(image, render_ms);
  }
  int32_t Decode(const webrtc::EncodedImage& image, bool missing,
                 int64_t render_ms) override {
    return decoder_->Decode(image, missing, render_ms);
  }
  int32_t RegisterDecodeCompleteCallback(webrtc::DecodedImageCallback* callback) override {
    return decoder_->RegisterDecodeCompleteCallback(callback);
  }
  int32_t Release() override { return decoder_->Release(); }
  DecoderInfo GetDecoderInfo() const override { return decoder_->GetDecoderInfo(); }
  const char* ImplementationName() const override {
    return decoder_->ImplementationName();
  }

 private:
  std::unique_ptr<webrtc::VideoDecoder> decoder_;
  const int cores_;
};

class CoreBudgetEncoderFactory final : public webrtc::VideoEncoderFactory {
 public:
  CoreBudgetEncoderFactory(std::unique_ptr<webrtc::VideoEncoderFactory> factory,
                           int cores)
      : factory_(std::move(factory)), cores_(cores) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return factory_->GetSupportedFormats();
  }
  std::vector<webrtc::SdpVideoFormat> GetImplementations() const override {
    return factory_->GetImplementations();
  }
  CodecSupport QueryCodecSupport(
      const webrtc::SdpVideoFormat& format,
      std::optional<std::string> scalability) const override {
    return factory_->QueryCodecSupport(format, std::move(scalability));
  }
  CodecSupport QueryCodecSupport(
      const webrtc::SdpVideoFormat& format,
      std::optional<std::string> scalability,
      std::optional<webrtc::Resolution> resolution) const override {
    return factory_->QueryCodecSupport(format, std::move(scalability), resolution);
  }
  std::unique_ptr<webrtc::VideoEncoder> Create(
      const webrtc::Environment& environment,
      const webrtc::SdpVideoFormat& format) override {
    auto encoder = factory_->Create(environment, format);
    if (!encoder) return nullptr;
    return std::make_unique<CoreBudgetEncoder>(std::move(encoder), cores_);
  }
  std::unique_ptr<EncoderSelectorInterface> GetEncoderSelector() const override {
    return factory_->GetEncoderSelector();
  }

 private:
  std::unique_ptr<webrtc::VideoEncoderFactory> factory_;
  const int cores_;
};

class CoreBudgetDecoderFactory final : public webrtc::VideoDecoderFactory {
 public:
  CoreBudgetDecoderFactory(std::unique_ptr<webrtc::VideoDecoderFactory> factory,
                           int cores)
      : factory_(std::move(factory)), cores_(cores) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return factory_->GetSupportedFormats();
  }
  CodecSupport QueryCodecSupport(const webrtc::SdpVideoFormat& format,
                                  bool reference_scaling) const override {
    return factory_->QueryCodecSupport(format, reference_scaling);
  }
  CodecSupport QueryCodecSupport(
      const webrtc::SdpVideoFormat& format, bool reference_scaling,
      std::optional<webrtc::Resolution> resolution) const override {
    return factory_->QueryCodecSupport(format, reference_scaling, resolution);
  }
  std::unique_ptr<webrtc::VideoDecoder> Create(
      const webrtc::Environment& environment,
      const webrtc::SdpVideoFormat& format) override {
    auto decoder = factory_->Create(environment, format);
    if (!decoder) return nullptr;
    return std::make_unique<CoreBudgetDecoder>(std::move(decoder), cores_);
  }

 private:
  std::unique_ptr<webrtc::VideoDecoderFactory> factory_;
  const int cores_;
};

}  // namespace pulsebeam::webrtc_sys
