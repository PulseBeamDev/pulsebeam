#pragma once
#include "rust/cxx.h"
#include <cstdint>
namespace webrtc {
struct RTPHeader;
class RtpPacketInfo;
class RtpPacketInfos;
class ColorSpace;
class VideoPlayoutDelay;
class Timestamp;
} // namespace webrtc
namespace pulsebeam::webrtc_sys {
struct FfiMediaPlayoutDelay;
struct FfiMediaHdrMetadata;
struct FfiMediaColorSpace;
struct FfiMediaRtpHeader;
struct FfiRtpReceivedPacketInfo;

std::int64_t MediaTimestampBits(webrtc::Timestamp) noexcept;
webrtc::ColorSpace MediaColorFromFfi(const FfiMediaColorSpace &) noexcept;
FfiMediaColorSpace MediaColorToFfi(const webrtc::ColorSpace &) noexcept;
webrtc::VideoPlayoutDelay
MediaPlayoutFromFfi(const FfiMediaPlayoutDelay &) noexcept;
FfiMediaPlayoutDelay
MediaPlayoutToFfi(const webrtc::VideoPlayoutDelay &) noexcept;

// Reusable lossless native value conversion for owning media products.
// False means an input would trigger a native CHECK or an unsafe enum cast.
bool MediaRtpHeaderFromFfi(const FfiMediaRtpHeader &,
                           webrtc::RTPHeader &) noexcept;
// Requires every raw field, including inactive video timing, to be initialized.
// Standalone NetEq insertion obtains that proof through MediaRtpHeaderFromFfi.
// A bare SDK RTPHeader constructor does not initialize its timing deltas.
FfiMediaRtpHeader MediaRtpHeaderToFfi(const webrtc::RTPHeader &) noexcept;
webrtc::RtpPacketInfo
MediaPacketInfoFromFfi(const FfiRtpReceivedPacketInfo &) noexcept;
FfiRtpReceivedPacketInfo
MediaPacketInfoToFfi(const webrtc::RtpPacketInfo &) noexcept;
webrtc::RtpPacketInfos MediaPacketInfosFromFfi(
    rust::Slice<const FfiRtpReceivedPacketInfo>) noexcept;
rust::Vec<FfiRtpReceivedPacketInfo>
MediaPacketInfosToFfi(const webrtc::RtpPacketInfos &) noexcept;

FfiMediaPlayoutDelay media_playout_delay_new(std::int64_t,
                                             std::int64_t) noexcept;
bool media_playout_delay_set(FfiMediaPlayoutDelay &, std::int64_t,
                             std::int64_t) noexcept;
bool media_hdr_validate(const FfiMediaHdrMetadata &) noexcept;
rust::String media_color_space_string(const FfiMediaColorSpace &) noexcept;
bool media_color_space_set(FfiMediaColorSpace &, std::uint8_t,
                           std::uint8_t) noexcept;
std::int64_t media_absolute_send_timestamp(std::uint32_t) noexcept;
std::uint16_t media_video_timing_difference(std::int64_t,
                                            std::int64_t) noexcept;
std::uint16_t media_video_timing_duration(std::int64_t) noexcept;
FfiMediaRtpHeader media_rtp_header_round_trip(const FfiMediaRtpHeader &,
                                              bool &) noexcept;
FfiRtpReceivedPacketInfo
media_packet_info_from_header(const FfiMediaRtpHeader &, std::int64_t,
                              bool &) noexcept;
FfiRtpReceivedPacketInfo
media_packet_info_round_trip(const FfiRtpReceivedPacketInfo &) noexcept;
rust::Vec<FfiRtpReceivedPacketInfo> media_packet_infos_round_trip(
    rust::Slice<const FfiRtpReceivedPacketInfo>) noexcept;
} // namespace pulsebeam::webrtc_sys
