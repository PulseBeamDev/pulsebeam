#pragma once

#include <array>
#include <cstdint>
#include <cstring>
#include <limits>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
#include <unordered_map>
#include <utility>

namespace pulsebeam::webrtc_sys::opus_carrier {
// An adapter-private transport across fixed 48 kHz, two-channel input blocks.
// Both source and passthrough encoder use this layout, independently of Opus
// payload channels or SDP stereo preference. PCM remixing would corrupt bytes.
// It never appears on the wire. Each 10 ms callback carries its own envelope,
// so reset cannot release reservations for input still on a native queue.
constexpr std::size_t kChannels = 2;
constexpr std::size_t kBytesPerChannelBlock = 960;
constexpr std::size_t kHeaderBytes = 36;
constexpr std::size_t kMaxPayloadBytes = 1200;
constexpr std::size_t kMaxPendingSlots = 24;
constexpr std::array<std::uint8_t, 8> kMagic = {'P','B','O','P','U','S','0','2'};

inline void Store16(std::uint8_t* out, std::uint16_t n) {
  out[0] = n & 255;
  out[1] = n >> 8;
}
inline void Store32(std::uint8_t* out, std::uint32_t n) {
  for (int i = 0; i < 4; ++i) out[i] = (n >> (i * 8)) & 255;
}
inline std::uint16_t Load16(const std::uint8_t* in) {
  return std::uint16_t(in[0]) | (std::uint16_t(in[1]) << 8);
}
inline std::uint32_t Load32(const std::uint8_t* in) {
  return std::uint32_t(in[0]) | (std::uint32_t(in[1]) << 8) |
         (std::uint32_t(in[2]) << 16) | (std::uint32_t(in[3]) << 24);
}
inline std::uint32_t Checksum(std::span<const std::uint8_t> data) {
  std::uint32_t hash = 2166136261u;
  for (auto byte : data) hash = (hash ^ byte) * 16777619u;
  return hash;
}
// A packet records the exact binding-owned source-sink generations present at
// admission. Each recipient's mask covers its queued 10 ms input copies.
struct PendingPacket {
  std::unordered_map<std::uint32_t, std::uint8_t> recipients;
};
struct Budget {
  std::mutex mutex;
  std::uint32_t next_packet = 0;
  std::size_t slots = 0;
  std::unordered_map<std::uint32_t, PendingPacket> pending;
};
struct Registry {
  std::mutex mutex;
  std::uint32_t next_source = 0;
  std::unordered_map<std::uint32_t, std::weak_ptr<Budget>> sources;
};
inline Registry& Budgets() {
  static Registry registry;
  return registry;
}
inline std::uint32_t Register(const std::shared_ptr<Budget>& budget) {
  auto& registry = Budgets();
  std::lock_guard lock(registry.mutex);
  if (registry.next_source == std::numeric_limits<std::uint32_t>::max()) return 0;
  const auto id = ++registry.next_source;
  registry.sources[id] = budget;
  return id;
}
inline void Unregister(std::uint32_t source) {
  auto& registry = Budgets();
  std::lock_guard lock(registry.mutex);
  registry.sources.erase(source);
}
inline std::optional<std::uint32_t> Reserve(
    Budget& budget, std::size_t slots,
    std::span<const std::uint32_t> recipients) {
  std::lock_guard lock(budget.mutex);
  if (slots == 0 || slots > 6 || recipients.empty() ||
      recipients.size() > (kMaxPendingSlots - budget.slots) / slots ||
      budget.next_packet == std::numeric_limits<std::uint32_t>::max())
    return std::nullopt;
  PendingPacket pending;
  const auto mask = static_cast<std::uint8_t>((1u << slots) - 1);
  for (auto recipient : recipients) {
    if (!recipient || !pending.recipients.emplace(recipient, mask).second)
      return std::nullopt;
  }
  const auto id = ++budget.next_packet;
  budget.slots += slots * recipients.size();
  budget.pending.emplace(id, std::move(pending));
  return id;
}
inline void Acknowledge(std::uint32_t source, std::uint32_t packet,
                        std::uint32_t recipient, std::uint8_t slot) {
  if (slot >= 6) return;
  auto& registry = Budgets();
  std::shared_ptr<Budget> budget;
  {
    std::lock_guard lock(registry.mutex);
    if (auto it = registry.sources.find(source); it != registry.sources.end())
      budget = it->second.lock();
  }
  if (!budget) return;
  std::lock_guard lock(budget->mutex);
  const auto it = budget->pending.find(packet);
  if (it == budget->pending.end()) return;
  const auto copy = it->second.recipients.find(recipient);
  const auto bit = static_cast<std::uint8_t>(1u << slot);
  if (copy == it->second.recipients.end() || !(copy->second & bit)) return;
  copy->second &= static_cast<std::uint8_t>(~bit);
  --budget->slots;
  if (!copy->second) it->second.recipients.erase(copy);
  if (it->second.recipients.empty()) budget->pending.erase(it);
}
inline bool HasMagic(std::span<const std::uint8_t> bytes) {
  return bytes.size() >= kMagic.size() &&
         std::memcmp(bytes.data(), kMagic.data(), kMagic.size()) == 0;
}
} // namespace pulsebeam::webrtc_sys::opus_carrier
