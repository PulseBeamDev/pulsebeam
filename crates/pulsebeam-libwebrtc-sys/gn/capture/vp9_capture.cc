#include <array>
#include <cstdint>
#include <fstream>
#include <iostream>
#include <memory>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include "api/call/transport.h"
#include "api/environment/environment_factory.h"
#include "api/rtp_header_extension_id.h"
#include "api/rtp_parameters.h"
#include "api/units/time_delta.h"
#include "api/units/timestamp.h"
#include "api/video/i420_buffer.h"
#include "api/video/video_bitrate_allocation.h"
#include "api/video/video_frame.h"
#include "api/video_codecs/scalability_mode.h"
#include "api/video_codecs/video_codec.h"
#include "api/video_codecs/video_encoder.h"
#include "call/rtp_config.h"
#include "call/rtp_transport_controller_send.h"
#include "call/rtp_video_sender_interface.h"
#include "modules/rtp_rtcp/include/rtp_rtcp_defines.h"
#include "modules/video_coding/codecs/vp9/include/vp9.h"
#include "modules/video_coding/fec_controller_default.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "rtc_base/checks.h"
#include "rtc_base/thread.h"
#include "system_wrappers/include/clock.h"
#include "test/time_controller/simulated_time_controller.h"

namespace {
using namespace webrtc;

constexpr int kWidth = 640;
constexpr int kHeight = 360;
constexpr int kFps = 30;
constexpr int kPictures = 12;
constexpr int kRestartPicture = 8;
constexpr uint32_t kSsrc = 0x506c7362;
constexpr int kPayloadType = 98;
constexpr int kDdId = 9;
constexpr int kPacketBytes = 1147;
constexpr uint32_t kRtpOffset = 0xffffb000;
constexpr Timestamp kStart = Timestamp::Seconds(1000);

void WriteBytes(std::ofstream& out, std::span<const uint8_t> bytes) {
  out.write(reinterpret_cast<const char*>(bytes.data()), bytes.size());
  RTC_CHECK(out.good());
}

class WireCapture final : public Transport {
 public:
  WireCapture(Clock& clock, const std::string& dir)
      : clock_(clock), bytes_(dir + "/wire.bin", std::ios::binary),
        index_(dir + "/wire.tsv") {
    index_ << "relative_us\tkind\toffset\tsize\n";
  }

  bool SendRtp(std::span<const uint8_t> packet, const PacketOptions&) override {
    Record("rtp", packet);
    return true;
  }
  bool SendRtcp(std::span<const uint8_t> packet, const PacketOptions&) override {
    Record("rtcp", packet);
    return true;
  }

 private:
  void Record(std::string_view kind, std::span<const uint8_t> packet) {
    index_ << (clock_.CurrentTime() - kStart).us() << '\t' << kind << '\t'
           << bytes_.tellp() << '\t' << packet.size() << '\n';
    RTC_CHECK(index_.good());
    WriteBytes(bytes_, packet);
  }

  Clock& clock_;
  std::ofstream bytes_;
  std::ofstream index_;
};

class EncoderCapture final : public EncodedImageCallback {
 public:
  EncoderCapture(RtpVideoSenderInterface& sender, const std::string& dir)
      : sender_(sender), bytes_(dir + "/encoded.bin", std::ios::binary),
        index_(dir + "/encoded.tsv") {
    index_ << "unit\tinput_rtp\tspatial\ttemporal\twidth\theight\tkey\t"
              "inter_layer_predicted\tend_of_picture\tmode\toffset\tsize\t"
              "buffers_id_ref_update\n";
  }

  Result OnEncodedImage(const EncodedImage& image,
                        const CodecSpecificInfo* info) override {
    RTC_CHECK(info && info->generic_frame_info && info->scalability_mode);
    RTC_CHECK_EQ(info->codecType, kVideoCodecVP9);
    RTC_CHECK(!image.SimulcastIndex().value_or(0));
    const auto& generic = *info->generic_frame_info;
    const auto& vp9 = info->codecSpecific.VP9;
    RTC_CHECK_EQ(image.SpatialIndex().value_or(-1), generic.spatial_id);
    index_ << unit_++ << '\t' << image.RtpTimestamp() << '\t'
           << generic.spatial_id << '\t' << generic.temporal_id << '\t'
           << image._encodedWidth << '\t' << image._encodedHeight << '\t'
           << image.IsKey() << '\t' << vp9.inter_layer_predicted << '\t'
           << info->end_of_picture << '\t'
           << ScalabilityModeToString(*info->scalability_mode) << '\t'
           << bytes_.tellp() << '\t' << image.size() << '\t';
    const char* separator = "";
    for (const auto& buffer : generic.encoder_buffers) {
      index_ << separator << buffer.id << ':' << buffer.referenced << ':'
             << buffer.updated;
      separator = ",";
    }
    index_ << '\n';
    RTC_CHECK(index_.good());
    WriteBytes(bytes_, {image.data(), image.size()});
    auto result = sender_.OnEncodedImage(image, info);
    RTC_CHECK_EQ(result.error, Result::OK);
    return result;
  }

  void OnFrameDropped(uint32_t, int, bool) override {
    RTC_CHECK(false) << "VP9 pilot unexpectedly dropped a coded frame";
  }

  int units() const { return unit_; }

 private:
  RtpVideoSenderInterface& sender_;
  std::ofstream bytes_;
  std::ofstream index_;
  int unit_ = 0;
};

void Capture(std::istream& source, std::string_view requested,
             ScalabilityMode mode, const std::string& dir) {
  GlobalSimulatedTimeController time(kStart);
  auto env = CreateEnvironment(time.GetClock(), time.GetTaskQueueFactory());
  WireCapture wire(env.clock(), dir);
  RtpTransportControllerSend transport({
      .env = env,
      .bitrate_config = {.start_bitrate_bps = 3'000'000},
      .network_thread = time.GetMainThread(),
  });
  transport.EnsureStarted();

  RtpConfig rtp;
  rtp.ssrcs = {kSsrc};
  rtp.payload_name = "VP9";
  rtp.payload_type = kPayloadType;
  rtp.max_packet_size = kPacketBytes;
  rtp.extmap_allow_mixed = true;
  rtp.extensions.emplace_back(RtpExtension::kDependencyDescriptorUri,
                              RtpHeaderExtensionId(kDdId));
  auto* sender = transport.CreateRtpVideoSender(
      {{kSsrc, {.sequence_number = 65510, .start_timestamp = kRtpOffset}}},
      {{kSsrc, {.picture_id = 50, .shared_frame_id = 100, .frame_id = 100}}},
      rtp, /*rtcp_report_interval_ms=*/200, &wire, {},
      std::make_unique<FecControllerDefault>(env), {}, nullptr);
  sender->SetSending(true);
  transport.OnNetworkAvailability(true);

  std::ofstream context(dir + "/context.tsv");
  context << "requested_mode\t" << requested << "\nformat\tVP9/90000\n"
          << "profile_id\t0\nssrc\t" << kSsrc << "\npt\t" << kPayloadType
          << "\ndd_id\t" << kDdId << "\ndd_uri\t"
          << RtpExtension::kDependencyDescriptorUri << "\nrtp_offset\t"
          << kRtpOffset << "\nmax_packet_bytes\t" << kPacketBytes << '\n';
  RTC_CHECK(context.good());

  EncoderCapture observe(*sender, dir);
  auto encoder = CreateVp9Encoder(env);
  VideoCodec codec;
  codec.codecType = kVideoCodecVP9;
  *codec.VP9() = VideoEncoder::GetDefaultVp9Settings();
  codec.VP9()->numberOfSpatialLayers = 2;
  codec.width = kWidth;
  codec.height = kHeight;
  codec.maxFramerate = kFps;
  codec.startBitrate = 1200;
  codec.maxBitrate = 2000;
  codec.active = true;
  codec.SetScalabilityMode(mode);
  VideoBitrateAllocation rates;
  for (int sid = 0; sid < 2; ++sid) {
    auto& layer = codec.spatialLayers[sid];
    layer.width = kWidth >> (1 - sid);
    layer.height = kHeight >> (1 - sid);
    layer.maxFramerate = kFps;
    layer.numberOfTemporalLayers = 1;
    layer.minBitrate = 100;
    layer.targetBitrate = sid == 0 ? 300 : 900;
    layer.maxBitrate = layer.targetBitrate;
    layer.active = true;
    RTC_CHECK(rates.SetBitrate(sid, 0, layer.targetBitrate * 1000));
  }
  RTC_CHECK_EQ(encoder->InitEncode(&codec, {1, kPacketBytes}), 0);
  RTC_CHECK_EQ(encoder->RegisterEncodeCompleteCallback(&observe), 0);
  encoder->SetRates({rates, kFps});

  std::array<uint8_t, kWidth * kHeight * 3 / 2> raw;
  for (int i = 0; i < kPictures; ++i) {
    const int64_t elapsed_us = i * 1'000'000 / kFps;
    time.AdvanceTime(kStart + TimeDelta::Micros(elapsed_us) -
                     env.clock().CurrentTime());
    source.read(reinterpret_cast<char*>(raw.data()), raw.size());
    RTC_CHECK(source.good());
    const auto* y = raw.data();
    const auto* u = y + kWidth * kHeight;
    const auto* v = u + kWidth * kHeight / 4;
    auto frame = VideoFrame::Builder()
                     .set_video_frame_buffer(I420Buffer::Copy(
                         kWidth, kHeight, y, kWidth, u, kWidth / 2, v, kWidth / 2))
                     .set_timestamp_us(env.clock().CurrentTime().us())
                     .set_timestamp_rtp(i * (90000 / kFps))
                     .build();
    const std::vector<VideoFrameType> type = {
        i == 0 || i == kRestartPicture ? VideoFrameType::kVideoFrameKey
                                       : VideoFrameType::kVideoFrameDelta};
    RTC_CHECK_EQ(encoder->Encode(frame, &type), 0);
    RTC_CHECK_EQ(observe.units(), 2 * (i + 1));
  }
  RTC_CHECK(source.peek() == std::istream::traits_type::eof());
  RTC_CHECK_EQ(encoder->Release(), 0);
  time.AdvanceTime(TimeDelta::Millis(400));
  transport.DestroyRtpVideoSender(sender);
}
}  // namespace

int main(int argc, char** argv) {
  if (argc != 4 || (std::string_view(argv[1]) != "S2T1" &&
                    std::string_view(argv[1]) != "L2T1")) {
    std::cerr << "usage: vp9_capture S2T1|L2T1 INPUT_I420 OUTPUT_DIRECTORY\n";
    return 2;
  }
  std::ifstream source(argv[2], std::ios::binary);
  RTC_CHECK(source.good());
  const std::string_view requested(argv[1]);
  const auto mode = requested == "S2T1" ? webrtc::ScalabilityMode::kS2T1
                                       : webrtc::ScalabilityMode::kL2T1;
  Capture(source, requested, mode, argv[3]);
}
