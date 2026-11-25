#include <array>
#include <cstdint>
#include <fstream>
#include <iostream>
#include <map>
#include <optional>
#include <span>
#include <string_view>
#include <vector>

#include "api/rtp_header_extension_id.h"
#include "api/video/rtp_video_frame_assembler.h"
#include "modules/rtp_rtcp/include/rtp_header_extension_map.h"
#include "modules/rtp_rtcp/source/rtp_dependency_descriptor_extension.h"
#include "modules/rtp_rtcp/source/rtp_packet_received.h"
#include "modules/rtp_rtcp/source/video_rtp_depacketizer_h264.h"
#include "modules/video_coding/h264_sps_pps_tracker.h"
#include "rtc_base/checks.h"

namespace {
using namespace webrtc;

RtpVideoFrameAssembler::PayloadFormat Format(std::string_view codec) {
  if (codec == "AV1") return RtpVideoFrameAssembler::kAv1;
  if (codec == "VP9") return RtpVideoFrameAssembler::kVp9;
  if (codec == "VP8") return RtpVideoFrameAssembler::kVp8;
  RTC_CHECK(codec == "H264");
  return RtpVideoFrameAssembler::kH264;
}
}  // namespace

int main(int argc, char** argv) {
  RTC_CHECK_EQ(argc, 4)
      << "usage: codec_receive CODEC PACKET_STREAM FRAME_BYTES";
  std::ifstream input(argv[2], std::ios::binary);
  std::ofstream output(argv[3], std::ios::binary);
  RTC_CHECK(input.is_open());
  RTC_CHECK(output.is_open());
  RtpHeaderExtensionMap extensions;
  RTC_CHECK(extensions.Register<RtpDependencyDescriptorExtension>(
      RtpHeaderExtensionId(9)));
  const auto format = Format(argv[1]);
  RtpVideoFrameAssembler assembler(format);
  VideoRtpDepacketizerH264 h264;
  video_coding::H264SpsPpsTracker tracker;
  std::map<uint16_t, std::vector<uint8_t>> annex_b;
  std::array<uint8_t, 1200> bytes;
  std::optional<uint32_t> ssrc;
  std::optional<int> pt;
  std::cout
      << "first_seq\tlast_seq\trtp_timestamp\tid\toffset\tsize\treferences\n";
  for (;;) {
    // Each RTP datagram is prefixed by its two-byte network-order length.
    const int high = input.get();
    if (high == std::ifstream::traits_type::eof()) break;
    const int low = input.get();
    RTC_CHECK_GE(low, 0);
    const size_t length = (static_cast<size_t>(high) << 8) | low;
    RTC_CHECK_GT(length, 0);
    RTC_CHECK_LE(length, bytes.size());
    input.read(reinterpret_cast<char*>(bytes.data()), length);
    RTC_CHECK(input.good());
    RtpPacketReceived packet(&extensions);
    RTC_CHECK(packet.Parse(bytes.data(), length));
    RTC_CHECK(packet.HasExtension<RtpDependencyDescriptorExtension>());
    RTC_CHECK_EQ(packet.Ssrc(), ssrc.value_or(packet.Ssrc()));
    RTC_CHECK_EQ(packet.PayloadType(), pt.value_or(packet.PayloadType()));
    ssrc = packet.Ssrc();
    pt = packet.PayloadType();
    if (format == RtpVideoFrameAssembler::kH264) {
      // The frame assembler resolves DD and packet membership. The production
      // H.264 receive path also normalizes individual payloads to Annex B.
      auto fragment = h264.Parse(packet.PayloadBuffer());
      RTC_CHECK(fragment);
      auto fixed = tracker.CopyAndFixBitstream(fragment->video_payload,
                                               &fragment->video_header);
      RTC_CHECK_EQ(fixed.action, video_coding::H264SpsPpsTracker::kInsert);
      annex_b.emplace(
          packet.SequenceNumber(),
          std::vector<uint8_t>(fixed.bitstream.begin(), fixed.bitstream.end()));
    }
    for (auto& assembled : assembler.InsertPacket(packet)) {
      auto frame = assembled.ExtractFrame();
      std::vector<uint8_t> h264_frame;
      std::span<const uint8_t> bitstream(frame->data(), frame->size());
      if (format == RtpVideoFrameAssembler::kH264) {
        uint16_t seq = assembled.RtpSeqNumStart();
        do {
          auto fragment = annex_b.find(seq);
          RTC_CHECK(fragment != annex_b.end());
          h264_frame.insert(h264_frame.end(), fragment->second.begin(),
                            fragment->second.end());
          annex_b.erase(fragment);
        } while (seq++ != assembled.RtpSeqNumEnd());
        bitstream = h264_frame;
      }
      std::cout << assembled.RtpSeqNumStart() << '\t'
                << assembled.RtpSeqNumEnd() << '\t' << frame->RtpTimestamp()
                << '\t' << frame->Id() << '\t' << output.tellp() << '\t'
                << bitstream.size() << '\t';
      for (size_t i = 0; i < frame->num_references; ++i) {
        if (i) std::cout << ',';
        std::cout << frame->references[i];
      }
      std::cout << '\n';
      output.write(reinterpret_cast<const char*>(bitstream.data()),
                   bitstream.size());
      RTC_CHECK(output.good());
    }
  }
  RTC_CHECK(input.eof() && ssrc.has_value());
  RTC_CHECK(std::cout.good());
  RTC_CHECK_GT(output.tellp(), 0);
}
