#pragma once

#include <algorithm>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <utility>
#include <vector>

#include "api/video/encoded_image.h"
#include "api/video_codecs/video_decoder.h"
#include "api/video_codecs/video_decoder_factory.h"
#include "video/null_video_decoder.h"

namespace pulsebeam::webrtc_sys {

struct EncodedFrameSelection {
  using Callback = std::function<void(const webrtc::EncodedImage&, uint32_t)>;
  explicit EncodedFrameSelection(Callback callback) : callback(std::move(callback)) {}
  Callback callback;
};

class EncodedFrameSelections final {
 public:
  std::shared_ptr<EncodedFrameSelection> Register(EncodedFrameSelection::Callback callback) {
    auto selection = std::make_shared<EncodedFrameSelection>(std::move(callback));
    std::lock_guard lock(mutex_);
    std::erase_if(selections_, [](const auto& entry) { return entry.expired(); });
    selections_.push_back(selection);
    return selection;
  }
  void OnSelected(const webrtc::EncodedImage& image, uint32_t ssrc) {
    std::vector<std::shared_ptr<EncodedFrameSelection>> live;
    {
      std::lock_guard lock(mutex_);
      for (const auto& entry : selections_) {
        if (auto selection = entry.lock()) live.push_back(std::move(selection));
      }
    }
    for (const auto& selection : live) selection->callback(image, ssrc);
  }

 private:
  std::mutex mutex_;
  std::vector<std::weak_ptr<EncodedFrameSelection>> selections_;
};

class SelectedFrameDecoder final : public webrtc::VideoDecoder {
 public:
  SelectedFrameDecoder(std::unique_ptr<webrtc::VideoDecoder> decoder,
                       std::shared_ptr<EncodedFrameSelections> selections,
                       uint32_t ssrc)
      : decoder_(std::move(decoder)), selections_(std::move(selections)), ssrc_(ssrc) {}

  bool Configure(const Settings& settings) override { return decoder_->Configure(settings); }
  int32_t Decode(const webrtc::EncodedImage& image, int64_t render_ms) override {
    const auto result = decoder_->Decode(image, render_ms);
    selections_->OnSelected(image, ssrc_);
    return result;
  }
  int32_t Decode(const webrtc::EncodedImage& image, bool missing,
                 int64_t render_ms) override {
    const auto result = decoder_->Decode(image, missing, render_ms);
    selections_->OnSelected(image, ssrc_);
    return result;
  }
  int32_t RegisterDecodeCompleteCallback(webrtc::DecodedImageCallback* callback) override {
    return decoder_->RegisterDecodeCompleteCallback(callback);
  }
  int32_t Release() override { return decoder_->Release(); }
  DecoderInfo GetDecoderInfo() const override { return decoder_->GetDecoderInfo(); }
  const char* ImplementationName() const override { return decoder_->ImplementationName(); }

 private:
  std::unique_ptr<webrtc::VideoDecoder> decoder_;
  std::shared_ptr<EncodedFrameSelections> selections_;
  const uint32_t ssrc_;
};

class SelectedFrameDecoderFactory final : public webrtc::VideoDecoderFactory {
 public:
  SelectedFrameDecoderFactory(webrtc::VideoDecoderFactory& factory,
                              std::shared_ptr<EncodedFrameSelections> selections,
                              uint32_t ssrc)
      : factory_(factory), selections_(std::move(selections)), ssrc_(ssrc) {}

  std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override {
    return factory_.GetSupportedFormats();
  }
  CodecSupport QueryCodecSupport(const webrtc::SdpVideoFormat& format,
                                  bool reference_scaling) const override {
    return factory_.QueryCodecSupport(format, reference_scaling);
  }
  CodecSupport QueryCodecSupport(
      const webrtc::SdpVideoFormat& format, bool reference_scaling,
      std::optional<webrtc::Resolution> resolution) const override {
    return factory_.QueryCodecSupport(format, reference_scaling, resolution);
  }
  std::unique_ptr<webrtc::VideoDecoder> Create(
      const webrtc::Environment& environment,
      const webrtc::SdpVideoFormat& format) override {
    auto decoder = factory_.Create(environment, format);
    // Match VideoReceiveStream2's own fallback, retaining observation when the
    // selected factory cannot create an implementation for a configured codec.
    if (!decoder) decoder = std::make_unique<webrtc::NullVideoDecoder>();
    return std::make_unique<SelectedFrameDecoder>(
        std::move(decoder), selections_, ssrc_);
  }

 private:
  webrtc::VideoDecoderFactory& factory_;
  std::shared_ptr<EncodedFrameSelections> selections_;
  const uint32_t ssrc_;
};

}  // namespace pulsebeam::webrtc_sys
