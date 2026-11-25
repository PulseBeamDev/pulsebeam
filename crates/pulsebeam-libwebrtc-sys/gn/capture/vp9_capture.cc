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
#include "modules/rtp_rtcp/source/rtp_rtcp_impl2.h"
#include "modules/rtp_rtcp/source/rtp_sender_audio.h"
#include "modules/video_coding/codecs/av1/libaom_av1_encoder.h"
#include "modules/video_coding/codecs/h264/include/h264.h"
#include "modules/video_coding/codecs/vp8/include/vp8.h"
#include "modules/video_coding/codecs/vp9/include/vp9.h"
#include "modules/video_coding/fec_controller_default.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "modules/video_coding/svc/create_scalability_structure.h"
#include "modules/video_coding/svc/scalability_mode_util.h"
#include "rtc_base/checks.h"
#include "rtc_base/logging.h"
#include "rtc_base/thread.h"
#include "system_wrappers/include/clock.h"
#include "test/time_controller/simulated_time_controller.h"

namespace {
using namespace webrtc;

constexpr int kFps = 30;
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
    RTC_CHECK(!image.SimulcastIndex().value_or(0));
    const auto& generic = *info->generic_frame_info;
    RTC_CHECK_EQ(image.SpatialIndex().value_or(0), generic.spatial_id);
    index_ << unit_++ << '\t' << image.RtpTimestamp() << '\t'
           << generic.spatial_id << '\t' << generic.temporal_id << '\t'
           << image._encodedWidth << '\t' << image._encodedHeight << '\t'
           << image.IsKey() << '\t';
    if (info->codecType == kVideoCodecVP9) {
      index_ << info->codecSpecific.VP9.inter_layer_predicted;
    }
    index_ << '\t' << info->end_of_picture << '\t'
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
    RTC_CHECK(false) << "Capture unexpectedly dropped a coded frame";
  }

  int units() const { return unit_; }

 private:
  RtpVideoSenderInterface& sender_;
  std::ofstream bytes_;
  std::ofstream index_;
  int unit_ = 0;
};

void CaptureOpus(std::istream& source, const std::string& dir) {
  constexpr int kOpusPt = 111;
  constexpr int kClockRate = 48000;
  constexpr int kSamplesPerPacket = 960;
  constexpr int kPackets = 300;
  GlobalSimulatedTimeController time(kStart);
  auto env = CreateEnvironment(time.GetClock(), time.GetTaskQueueFactory());
  WireCapture wire(env.clock(), dir);
  RtpRtcpInterface::Configuration config;
  config.audio = true;
  config.outgoing_transport = &wire;
  config.rtcp_report_interval_ms = 200;
  config.local_media_ssrc = kSsrc;
  config.rtp_state = RtpState{.sequence_number = 65510, .start_timestamp = kRtpOffset};
  config.rtcp_mode = RtcpMode::kCompound;
  auto rtp = ModuleRtpRtcpImpl2::CreateSendModule(env, config);
  rtp->RegisterSendPayloadFrequency(kOpusPt, kClockRate);
  RTPSenderAudio audio(&env.clock(), rtp->RtpSender());
  RTC_CHECK_EQ(audio.RegisterAudioPayload("opus", kOpusPt, kClockRate, 1, 48000), 0);
  RTC_CHECK_EQ(rtp->SetSendingStatus(true), 0);
  rtp->SetSendingMediaStatus(true);
  std::ofstream context(dir + "/context.tsv");
  context << "format\topus/48000/1\npt\t" << kOpusPt << "\nssrc\t" << kSsrc
          << "\nrtp_offset\t" << rtp->StartTimestamp() << '\n';
  RTC_CHECK(context.good());
  for (int i = 0; i < kPackets; ++i) {
    const int low = source.get();
    const int high = source.get();
    RTC_CHECK(low >= 0 && high >= 0);
    const size_t size = low + 256 * high;
    RTC_CHECK(size > 0 && size <= 1275);
    std::vector<uint8_t> payload(size);
    source.read(reinterpret_cast<char*>(payload.data()), size);
    RTC_CHECK(source.good());
    const uint32_t timestamp = i * kSamplesPerPacket;
    const Timestamp now = env.clock().CurrentTime();
    RTC_CHECK(rtp->OnSendingRtpFrame(timestamp, now.ms(), kOpusPt, false));
    RTC_CHECK(audio.SendAudio({.payload = payload, .payload_id = kOpusPt,
                              .rtp_timestamp = timestamp + rtp->StartTimestamp(),
                              .capture_time = now}));
    time.AdvanceTime(TimeDelta::Millis(20));
  }
  RTC_CHECK(source.peek() == std::istream::traits_type::eof());
}

void Capture(std::istream& source, std::string_view requested,
             ScalabilityMode mode, VideoCodecType codec_type,
             const std::string& dir) {
  const bool vp9 = codec_type == kVideoCodecVP9;
  const bool pilot = vp9 && (mode == ScalabilityMode::kS2T1 ||
                            mode == ScalabilityMode::kL2T1);
  const int width = pilot ? 640 : 1152;
  const int height = pilot ? 360 : 720;
  const int pictures = pilot ? 12 : 36;
  const int restart_picture = pilot ? 8 : 24;
  const auto config = ScalabilityStructureConfig(mode);
  RTC_CHECK(config);
  GlobalSimulatedTimeController time(kStart);
  auto env = CreateEnvironment(time.GetClock(), time.GetTaskQueueFactory());
  WireCapture wire(env.clock(), dir);
  RtpTransportControllerSend transport({
      .env = env,
      // This lane records intact sender output, not pacer congestion drops.
      .bitrate_config = {.start_bitrate_bps = vp9 ? 3'000'000 : 30'000'000},
      .network_thread = time.GetMainThread(),
  });
  transport.EnsureStarted();

  RtpConfig rtp;
  rtp.ssrcs = {kSsrc};
  switch (codec_type) {
    case kVideoCodecVP8: rtp.payload_name = "VP8"; break;
    case kVideoCodecVP9: rtp.payload_name = "VP9"; break;
    case kVideoCodecH264: rtp.payload_name = "H264"; break;
    case kVideoCodecAV1: rtp.payload_name = "AV1"; break;
    default: RTC_CHECK(false);
  }
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
  context << "requested_mode\t" << requested << "\nformat\t"
          << rtp.payload_name << "/90000\n";
  if (codec_type == kVideoCodecH264) {
    context << "packetization_mode\t1\n";
  } else {
    context << "profile_id\t0\n";
  }
  context << "ssrc\t" << kSsrc
          << "\npt\t" << kPayloadType
          << "\ndd_id\t" << kDdId << "\ndd_uri\t"
          << RtpExtension::kDependencyDescriptorUri << "\nrtp_offset\t"
          << kRtpOffset << "\nmax_packet_bytes\t" << kPacketBytes << '\n';
  RTC_CHECK(context.good());

  EncoderCapture observe(*sender, dir);
  std::unique_ptr<VideoEncoder> encoder;
  switch (codec_type) {
    case kVideoCodecVP8: encoder = CreateVp8Encoder(env); break;
    case kVideoCodecVP9: encoder = CreateVp9Encoder(env); break;
    case kVideoCodecH264: encoder = CreateH264Encoder(env); break;
    case kVideoCodecAV1: encoder = CreateLibaomAv1Encoder(env); break;
    default: RTC_CHECK(false);
  }
  VideoCodec codec;
  codec.codecType = codec_type;
  if (vp9) {
    *codec.VP9() = VideoEncoder::GetDefaultVp9Settings();
    codec.VP9()->numberOfSpatialLayers = config->num_spatial_layers;
    codec.VP9()->numberOfTemporalLayers = config->num_temporal_layers;
  } else if (codec_type == kVideoCodecVP8) {
    *codec.VP8() = VideoEncoder::GetDefaultVp8Settings();
    codec.VP8()->numberOfTemporalLayers = config->num_temporal_layers;
  } else if (codec_type == kVideoCodecH264) {
    *codec.H264() = VideoEncoder::GetDefaultH264Settings();
    codec.H264()->numberOfTemporalLayers = config->num_temporal_layers;
  } else {
    codec.qpMax = 63;
  }
  codec.width = width;
  codec.height = height;
  codec.maxFramerate = kFps;
  codec.startBitrate = 1200;
  codec.maxBitrate = 2000;
  codec.active = true;
  codec.SetScalabilityMode(mode);
  VideoBitrateAllocation rates;
  auto spatial_layers = std::span(codec.spatialLayers);
  auto numerators = std::span(config->scaling_factor_num);
  auto denominators = std::span(config->scaling_factor_den);
  for (int sid = 0; sid < config->num_spatial_layers; ++sid) {
    auto& layer = spatial_layers[sid];
    layer.width = width * numerators[sid] / denominators[sid];
    layer.height = height * numerators[sid] / denominators[sid];
    RTC_CHECK_EQ(layer.width * denominators[sid], width * numerators[sid]);
    RTC_CHECK_EQ(layer.height * denominators[sid], height * numerators[sid]);
    layer.maxFramerate = kFps;
    layer.numberOfTemporalLayers = config->num_temporal_layers;
    layer.minBitrate = 100;
    layer.targetBitrate = sid == 0 ? 300 : 900;
    layer.maxBitrate = layer.targetBitrate;
    layer.active = true;
    for (int tid = 0; tid < config->num_temporal_layers; ++tid) {
      RTC_CHECK(rates.SetBitrate(
          sid, tid, layer.targetBitrate * 1000 / config->num_temporal_layers));
    }
  }
  RTC_CHECK_EQ(encoder->InitEncode(&codec, {1, kPacketBytes}), 0);
  RTC_CHECK_EQ(encoder->RegisterEncodeCompleteCallback(&observe), 0);
  encoder->SetRates({rates, kFps});

  const size_t luma = width * height;
  std::vector<uint8_t> raw(luma * 3 / 2);
  for (int i = 0; i < pictures; ++i) {
    const int units_before = observe.units();
    const int64_t elapsed_us = i * 1'000'000 / kFps;
    time.AdvanceTime(kStart + TimeDelta::Micros(elapsed_us) -
                     env.clock().CurrentTime());
    source.read(reinterpret_cast<char*>(raw.data()), raw.size());
    RTC_CHECK(source.good());
    const auto planes = std::span(raw);
    const auto* y = planes.data();
    const auto* u = planes.subspan(luma, luma / 4).data();
    const auto* v = planes.last(luma / 4).data();
    auto frame = VideoFrame::Builder()
                     .set_video_frame_buffer(I420Buffer::Copy(
                         width, height, y, width, u, width / 2, v, width / 2))
                     .set_timestamp_us(env.clock().CurrentTime().us())
                     .set_timestamp_rtp(i * (90000 / kFps))
                     .build();
    const std::vector<VideoFrameType> type = {
        i == 0 || i == restart_picture ? VideoFrameType::kVideoFrameKey
                                       : VideoFrameType::kVideoFrameDelta};
    RTC_CHECK_EQ(encoder->Encode(frame, &type), 0)
        << "mode=" << requested << " input_picture=" << i;
    if (pilot) {
      RTC_CHECK_EQ(observe.units() - units_before, config->num_spatial_layers);
    }
  }
  RTC_CHECK(source.peek() == std::istream::traits_type::eof());
  RTC_CHECK_EQ(encoder->Release(), 0);
  time.AdvanceTime(TimeDelta::Millis(400));
  transport.DestroyRtpVideoSender(sender);
}
}  // namespace

int main(int argc, char** argv) {
  webrtc::LogMessage::LogToDebug(webrtc::LS_WARNING);
  using namespace webrtc;
  if (argc != 4 && argc != 5) {
    std::cerr << "usage: vp9_capture [--av1|--vp8|--h264] MODE INPUT_I420 OUTPUT_DIRECTORY\n";
    return 2;
  }
  VideoCodecType codec = kVideoCodecVP9;
  if (argc == 5) {
    const std::string_view flag(argv[1]);
    if (flag == "--av1") codec = kVideoCodecAV1;
    else if (flag == "--vp8") codec = kVideoCodecVP8;
    else if (flag == "--h264") codec = kVideoCodecH264;
    else return 2;
  }
  if (argc == 4 && std::string_view(argv[1]) == "--opus") {
    std::ifstream source(argv[2], std::ios::binary);
    RTC_CHECK(source.good());
    CaptureOpus(source, argv[3]);
    return 0;
  }
  const int arg = argc - 3;
  const std::string_view requested(argv[arg]);
  const auto mode = ScalabilityModeFromString(requested);
  if (!mode || !ScalabilityStructureConfig(*mode) ||
      (codec == kVideoCodecVP9 && !VP9Encoder::SupportsScalabilityMode(*mode)) ||
      (codec == kVideoCodecH264 && !H264Encoder::SupportsScalabilityMode(*mode)) ||
      (codec == kVideoCodecVP8 && !requested.starts_with("L1T"))) {
    std::cerr << "unsupported upstream scalability mode: " << requested << '\n';
    return 2;
  }
  std::ifstream source(argv[arg + 1], std::ios::binary);
  RTC_CHECK(source.good());
  Capture(source, requested, *mode, codec, argv[arg + 2]);
}
