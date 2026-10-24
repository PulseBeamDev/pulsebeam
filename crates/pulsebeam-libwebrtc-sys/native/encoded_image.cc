#include "pulsebeam-webrtc-sys/native/encoded_image.h"
#include "pulsebeam-webrtc-sys/native/media_values.h"
#include "pulsebeam-webrtc-sys/src/lib.rs.h"
#include <algorithm>
#include <cstring>
#include <limits>
#include <iterator>
#include <optional>

namespace pulsebeam::webrtc_sys {
namespace {
webrtc::Timestamp Timestamp(std::int64_t bits) {
  if (bits == INT64_MAX) return webrtc::Timestamp::PlusInfinity();
  if (bits == INT64_MIN) return webrtc::Timestamp::MinusInfinity();
  return webrtc::Timestamp::Micros(bits);
}
rust::Vec<std::uint8_t> Bytes(const std::uint8_t* data, std::size_t size) {
  rust::Vec<std::uint8_t> result;
  result.reserve(size);
  for (std::size_t i = 0; i < size; ++i) result.push_back(data[i]);
  return result;
}
bool Index(const FfiOptionalI32& value, int limit) { return !value.present || (value.value >= 0 && value.value < limit); }
FfiOptionalI32 Index(std::optional<int> value) { return {value.has_value(), value.value_or(0)}; }
std::optional<int> Index(const FfiOptionalI32& value) { return value.present ? std::optional<int>(value.value) : std::nullopt; }
bool LayerIndex(const webrtc::EncodedImage& value, int index) { return index >= 0 && index <= value.SpatialIndex().value_or(0); }
}
NativeWritableEncodedBuffer::NativeWritableEncodedBuffer(std::size_t size) : value_(webrtc::EncodedImageBuffer::Create(size)) {}
bool NativeWritableEncodedBuffer::Initialized() const {
  return value_->size() == 0 || (!initialized_.empty() && initialized_.begin()->first == 0 && initialized_.begin()->second >= value_->size());
}
void NativeWritableEncodedBuffer::Resize(std::size_t size) {
  const auto old_size = value_->size();
  const auto old_address = reinterpret_cast<std::uintptr_t>(value_->data());
  value_->Realloc(size);
  // Buffer::SetSize keeps truncated storage when capacity suffices, but a new
  // allocation copies only the old logical prefix. Its new allocation is made
  // before freeing the old one, so actual allocation identity is observable.
  if (reinterpret_cast<std::uintptr_t>(value_->data()) != old_address) {
    const auto kept = std::min(size, old_size);
    initialized_.erase(initialized_.lower_bound(kept), initialized_.end());
    if (!initialized_.empty()) initialized_.rbegin()->second = std::min(initialized_.rbegin()->second, kept);
  }
}
bool NativeWritableEncodedBuffer::Write(std::size_t offset, rust::Slice<const std::uint8_t> data) {
  if (offset > value_->size() || data.size() > value_->size() - offset) return false;
  if (data.empty()) return true;
  std::memcpy(value_->data() + offset, data.data(), data.size());
  auto end = offset + data.size();
  auto it = initialized_.lower_bound(offset);
  if (it != initialized_.begin()) {
    auto previous = std::prev(it);
    if (previous->second >= offset) { offset = previous->first; end = std::max(end, previous->second); it = initialized_.erase(previous); }
  }
  while (it != initialized_.end() && it->first <= end) { end = std::max(end, it->second); it = initialized_.erase(it); }
  initialized_.emplace(offset, end);
  return true;
}
std::unique_ptr<NativeEncodedImage> encoded_image_new() noexcept { return std::make_unique<NativeEncodedImage>(); }
std::unique_ptr<NativeEncodedImage> encoded_image_clone(const NativeEncodedImage& value) noexcept { return std::make_unique<NativeEncodedImage>(value.value()); }
void encoded_image_assign(NativeEncodedImage& value, const NativeEncodedImage& other) noexcept { value.value() = other.value(); }
FfiEncodedImageMetadata encoded_image_metadata(const NativeEncodedImage& image) noexcept {
  const auto& value = image.value();
  FfiEncodedImageMetadata result{};
  result.width = value._encodedWidth; result.height = value._encodedHeight;
  result.rtp_timestamp = value.RtpTimestamp(); result.ntp_time_ms = value.NtpTimeMs(); result.capture_time_ms = value.capture_time_ms_;
  result.frame_type = static_cast<std::int32_t>(value.frame_type()); result.rotation = static_cast<std::int32_t>(value.rotation()); result.content_type = static_cast<std::uint8_t>(value.contentType()); result.qp = value.qp_;
  result.simulcast_index = Index(value.SimulcastIndex()); result.spatial_index = Index(value.SpatialIndex()); result.temporal_index = Index(value.TemporalIndex());
  // Native indices may be lowered without erasing earlier map entries. Query a
  // value-copy at its permitted upper index so assignment preserves those too.
  auto sizes = value;
  sizes.SetSpatialIndex(webrtc::kMaxSpatialLayers - 1);
  for (int i = 0; i < webrtc::kMaxSpatialLayers; ++i) if (const auto size = sizes.SpatialLayerFrameSize(i)) result.spatial_layer_sizes.push_back({i, *size});
  result.has_presentation_time = value.PresentationTimestamp().has_value();
  if (result.has_presentation_time) result.presentation_time_us = MediaTimestampBits(*value.PresentationTimestamp());
  result.has_tracking_id = value.VideoFrameTrackingId().has_value(); result.tracking_id = value.VideoFrameTrackingId().value_or(0);
  result.has_color_space = value.ColorSpace() != nullptr; if (result.has_color_space) result.color_space = MediaColorToFfi(*value.ColorSpace());
  result.has_playout_delay = value.PlayoutDelay().has_value(); if (result.has_playout_delay) result.playout_delay = MediaPlayoutToFfi(*value.PlayoutDelay());
  result.packet_infos = MediaPacketInfosToFfi(value.PacketInfos());
  result.retransmission_allowed = value.RetransmissionAllowed(); result.steady_state_refresh = value.IsSteadyStateRefreshFrame();
  const auto timing = value.video_timing();
  result.timing_flags = timing.flags;
  result.timing_ms = {timing.encode_start_ms, timing.encode_finish_ms, timing.packetization_finish_ms, timing.pacer_exit_ms, timing.network_timestamp_ms, timing.network2_timestamp_ms, timing.receive_start_ms, timing.receive_finish_ms};
  const auto filter = value.corruption_detection_filter_settings(); result.has_corruption_filter = filter.has_value();
  if (filter) { result.filter_std_dev = filter->std_dev; result.filter_luma_threshold = filter->luma_error_threshold; result.filter_chroma_threshold = filter->chroma_error_threshold; }
  const auto psnr = value.psnr(); result.has_psnr = psnr.has_value(); if (psnr) result.psnr = {psnr->y, psnr->u, psnr->v};
  result.has_end_of_temporal_unit = value.is_end_of_temporal_unit().has_value(); result.end_of_temporal_unit = value.is_end_of_temporal_unit().value_or(false);
  return result;
}
bool encoded_image_set_metadata(NativeEncodedImage& image, const FfiEncodedImageMetadata& r) noexcept {
  // Only native setter/enum-representation preconditions apply to inert values.
  if (r.rotation < 0 || r.rotation > 511 ||
      !Index(r.simulcast_index, webrtc::kMaxSimulcastStreams) || !Index(r.spatial_index, webrtc::kMaxSpatialLayers) || !Index(r.temporal_index, webrtc::kMaxTemporalStreams)) return false;
  for (const auto& size : r.spatial_layer_sizes) if (size.index < 0 || size.index >= webrtc::kMaxSpatialLayers) return false;
  webrtc::EncodedImage value;
  const auto buffer = image.value().GetEncodedData();
  if (buffer) { value.SetEncodedData(buffer); value.set_size(image.value().size()); }
  value._encodedWidth = r.width; value._encodedHeight = r.height; value.SetRtpTimestamp(r.rtp_timestamp);
  value.ntp_time_ms_ = r.ntp_time_ms; value.capture_time_ms_ = r.capture_time_ms;
  value.set_frame_type(static_cast<webrtc::VideoFrameType>(r.frame_type)); value.rotation_ = static_cast<webrtc::VideoRotation>(r.rotation); value.content_type_ = static_cast<webrtc::VideoContentType>(r.content_type); value.qp_ = r.qp;
  value.SetSimulcastIndex(Index(r.simulcast_index)); value.SetTemporalIndex(Index(r.temporal_index));
  value.SetSpatialIndex(webrtc::kMaxSpatialLayers - 1);
  for (const auto& size : r.spatial_layer_sizes) value.SetSpatialLayerFrameSize(size.index, size.bytes);
  value.SetSpatialIndex(Index(r.spatial_index));
  if (r.has_presentation_time) value.SetPresentationTimestamp(Timestamp(r.presentation_time_us));
  if (r.has_tracking_id) value.SetVideoFrameTrackingId(r.tracking_id);
  if (r.has_color_space) value.SetColorSpace(MediaColorFromFfi(r.color_space));
  if (r.has_playout_delay) value.SetPlayoutDelay(MediaPlayoutFromFfi(r.playout_delay));
  value.SetPacketInfos(MediaPacketInfosFromFfi({r.packet_infos.data(), r.packet_infos.size()}));
  value.SetRetransmissionAllowed(r.retransmission_allowed); value.SetIsSteadyStateRefreshFrame(r.steady_state_refresh);
  value.timing_ = {r.timing_flags, r.timing_ms[0], r.timing_ms[1], r.timing_ms[2], r.timing_ms[3], r.timing_ms[4], r.timing_ms[5], r.timing_ms[6], r.timing_ms[7]};
  if (r.has_corruption_filter) value.set_corruption_detection_filter_settings({r.filter_std_dev, r.filter_luma_threshold, r.filter_chroma_threshold});
  if (r.has_psnr) value.set_psnr(webrtc::EncodedImage::Psnr{r.psnr[0], r.psnr[1], r.psnr[2]});
  if (r.has_end_of_temporal_unit) value.set_end_of_temporal_unit(r.end_of_temporal_unit);
  image.value() = std::move(value);
  return true;
}
bool encoded_image_capture_time(const NativeEncodedImage& value, std::int64_t& bits) noexcept {
  constexpr auto limit = std::numeric_limits<std::int64_t>::max() / 1000;
#if RTC_DCHECK_IS_ON
  if (value.value().capture_time_ms_ >= limit) return false;
#else
  if (value.value().capture_time_ms_ > limit) return false;
#endif
  bits = MediaTimestampBits(value.value().CaptureTime()); return true;
}
void encoded_image_set_encode_time(NativeEncodedImage& value, std::int64_t start, std::int64_t finish) noexcept { value.value().SetEncodeTime(start, finish); }
bool encoded_image_is_key(const NativeEncodedImage& value) noexcept { return value.value().IsKey(); }
bool encoded_image_is_delta(const NativeEncodedImage& value) noexcept { return value.value().IsDelta(); }
std::size_t encoded_image_size(const NativeEncodedImage& value) noexcept { return value.value().size(); }
bool encoded_image_set_size(NativeEncodedImage& value, std::size_t size) noexcept {
  const auto buffer = value.value().GetEncodedData(); if (size != 0 && (!buffer || size > buffer->size())) return false;
  value.value().set_size(size); return true;
}
rust::Vec<std::uint8_t> encoded_image_data(const NativeEncodedImage& value) noexcept { return Bytes(value.value().data(), value.value().size()); }
std::unique_ptr<NativeEncodedVideoBuffer> encoded_image_buffer(const NativeEncodedImage& value) noexcept { const auto buffer = value.value().GetEncodedData(); return buffer ? std::make_unique<NativeEncodedVideoBuffer>(buffer) : nullptr; }
void encoded_image_set_buffer(NativeEncodedImage& value, const NativeEncodedVideoBuffer& buffer) noexcept { value.value().SetEncodedData(buffer.value()); }
void encoded_image_clear_buffer(NativeEncodedImage& value) noexcept { value.value().ClearEncodedData(); }
bool encoded_image_layer_size(const NativeEncodedImage& value, std::int32_t index, std::size_t& size, bool& present) noexcept {
  if (!LayerIndex(value.value(), index)) return false;
  const auto result = value.value().SpatialLayerFrameSize(index); present = result.has_value(); size = result.value_or(0); return true;
}
bool encoded_image_set_layer_size(NativeEncodedImage& value, std::int32_t index, std::size_t size) noexcept {
  if (!LayerIndex(value.value(), index)) return false;
  value.value().SetSpatialLayerFrameSize(index, size); return true;
}
bool encoded_image_frame_type_name(std::int32_t type, rust::String& name) noexcept {
  if (type != 0 && type != 3 && type != 4) return false;
  const auto text = webrtc::VideoFrameTypeToString(static_cast<webrtc::VideoFrameType>(type));
  name = rust::String(text.data(), text.size()); return true;
}
std::unique_ptr<NativeEncodedVideoBuffer> encoded_buffer_empty() noexcept { return std::make_unique<NativeEncodedVideoBuffer>(webrtc::EncodedImageBuffer::Create()); }
std::unique_ptr<NativeEncodedVideoBuffer> encoded_buffer_new_owned(rust::Vec<std::uint8_t> data) noexcept {
  webrtc::Buffer buffer(data.data(), data.size());
  return std::make_unique<NativeEncodedVideoBuffer>(webrtc::EncodedImageBuffer::Create(std::move(buffer)));
}
std::unique_ptr<NativeEncodedVideoBuffer> encoded_buffer_new(rust::Slice<const std::uint8_t> data) noexcept { return std::make_unique<NativeEncodedVideoBuffer>(webrtc::EncodedImageBuffer::Create(data.data(), data.size())); }
std::unique_ptr<NativeEncodedVideoBuffer> encoded_buffer_clone(const NativeEncodedVideoBuffer& value) noexcept { return std::make_unique<NativeEncodedVideoBuffer>(value.value()); }
std::size_t encoded_buffer_size(const NativeEncodedVideoBuffer& value) noexcept { return value.value()->size(); }
rust::Vec<std::uint8_t> encoded_buffer_data(const NativeEncodedVideoBuffer& value) noexcept { return Bytes(value.value()->data(), value.value()->size()); }
std::unique_ptr<NativeWritableEncodedBuffer> encoded_writer_new(std::size_t size) noexcept { return std::make_unique<NativeWritableEncodedBuffer>(size); }
std::size_t encoded_writer_size(const NativeWritableEncodedBuffer& value) noexcept { return value.value().size(); }
void encoded_writer_resize(NativeWritableEncodedBuffer& value, std::size_t size) noexcept { value.Resize(size); }
bool encoded_writer_write(NativeWritableEncodedBuffer& value, std::size_t offset, rust::Slice<const std::uint8_t> data) noexcept { return value.Write(offset, data); }
FfiEncodedBufferRead encoded_writer_read(const NativeWritableEncodedBuffer& value) noexcept { return {value.Initialized(), value.Initialized() ? Bytes(value.value().data(), value.value().size()) : rust::Vec<std::uint8_t>()}; }
std::unique_ptr<NativeEncodedVideoBuffer> encoded_writer_freeze(std::unique_ptr<NativeWritableEncodedBuffer> value) noexcept { return value->Initialized() ? std::make_unique<NativeEncodedVideoBuffer>(value->Freeze()) : nullptr; }
}  // namespace pulsebeam::webrtc_sys
