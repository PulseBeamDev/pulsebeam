#include "pulsebeam-webrtc-sys/native/socket_address.h"
#include <cstdlib>
#include <cstring>
#include <string>
#include "absl/strings/string_view.h"
namespace pulsebeam::webrtc_sys {
namespace {
webrtc::IPAddress Ip(rust::Slice<const std::uint8_t> bytes) {
  if (bytes.size() == 4) { in_addr value{}; std::memcpy(&value, bytes.data(), 4); return webrtc::IPAddress(value); }
  if (bytes.size() == 16) { in6_addr value{}; std::memcpy(&value, bytes.data(), 16); return webrtc::IPAddress(value); }
  return webrtc::IPAddress();
}
rust::Vec<std::uint8_t> Bytes(const void* data, std::size_t size) {
  rust::Vec<std::uint8_t> result;
  const auto* bytes = static_cast<const std::uint8_t*>(data);
  for (std::size_t i = 0; i < size; ++i) result.push_back(bytes[i]);
  return result;
}
rust::Vec<std::uint8_t> Bytes(const std::string& text) { return Bytes(text.data(), text.size()); }
rust::Vec<std::uint8_t> IpBytes(const webrtc::IPAddress& ip) {
  if (ip.family() == AF_INET) { auto value = ip.ipv4_address(); return Bytes(&value, sizeof(value)); }
  if (ip.family() == AF_INET6) { auto value = ip.ipv6_address(); return Bytes(&value, sizeof(value)); }
  return {};
}
absl::string_view View(rust::Slice<const std::uint8_t> bytes) { return {reinterpret_cast<const char*>(bytes.data()), bytes.size()}; }
bool Storage(rust::Slice<const std::uint8_t> bytes, sockaddr_storage& storage) {
  if (bytes.size() != sizeof(sockaddr_storage) && bytes.size() != sizeof(sockaddr_in)) return false;
  std::memset(&storage, 0, sizeof(storage));
  std::memcpy(&storage, bytes.data(), bytes.size());
  return true;
}
}
std::int32_t address_family(std::uint8_t kind) { return kind == 1 ? AF_INET : kind == 2 ? AF_INET6 : AF_UNSPEC; }
rust::Vec<std::uint8_t> ip_address_parse(rust::Slice<const std::uint8_t> text) {
  webrtc::IPAddress ip;
  return webrtc::IPFromString(View(text), &ip) ? IpBytes(ip) : rust::Vec<std::uint8_t>{};
}
rust::Vec<std::uint8_t> ip_address_special(std::int32_t family, bool loopback) { return IpBytes(loopback ? webrtc::GetLoopbackIP(family) : webrtc::GetAnyIP(family)); }
rust::Vec<std::uint8_t> ip_address_transform(rust::Slice<const std::uint8_t> bytes, std::uint8_t operation, std::int32_t length) {
  auto ip = Ip(bytes);
  switch (operation) {
    case 0: return IpBytes(ip.Normalized());
    case 1: return IpBytes(ip.NormalizeWithCheckForEmbeddedIPv4Address());
    case 2: return IpBytes(ip.AsIPv6Address());
    default: return IpBytes(webrtc::TruncateIP(ip, length));
  }
}
rust::Vec<std::uint8_t> ip_address_text(rust::Slice<const std::uint8_t> bytes, std::uint8_t kind, std::int32_t flags) {
  auto ip = Ip(bytes);
  return Bytes(kind == 1 ? ip.ToSensitiveString() : kind == 2 ? webrtc::InterfaceAddress(ip, flags).ToString() : kind == 3 ? absl::StrCat(ip) : ip.ToString());
}
std::int64_t ip_address_info(rust::Slice<const std::uint8_t> bytes, std::uint8_t kind) {
  auto ip = Ip(bytes);
  switch (kind) {
    case 0: return ip.family(); case 1: return ip.Size(); case 2: return ip.overhead();
    case 3: return ip.v4AddressAsHostOrderInteger(); case 4: return webrtc::IPAddressPrecedence(ip);
    default: return webrtc::CountIPMaskBits(ip);
  }
}
std::uint64_t ip_address_hash(rust::Slice<const std::uint8_t> bytes) { return webrtc::HashIP(Ip(bytes)); }
std::uint32_t ip_address_predicates(rust::Slice<const std::uint8_t> bytes) {
  auto ip = Ip(bytes); std::uint32_t result = 0;
  const bool values[] = {ip.IsNil(), webrtc::IPIsAny(ip), webrtc::IPIsLoopback(ip), webrtc::IPIsLinkLocal(ip), webrtc::IPIsPrivateNetwork(ip), webrtc::IPIsSharedNetwork(ip), webrtc::IPIsPrivate(ip), webrtc::IPIsUnspec(ip), webrtc::IPIs6Bone(ip), webrtc::IPIs6To4(ip), webrtc::IPIsIsatap(ip), webrtc::IPIsMacBased(ip), webrtc::IPIsNat64(ip), webrtc::IPIsSiteLocal(ip), webrtc::IPIsTeredo(ip), webrtc::IPIsULA(ip), webrtc::IPIsV4Compatibility(ip), webrtc::IPIsV4Mapped(ip)};
  for (std::size_t i = 0; i < sizeof(values) / sizeof(values[0]); ++i) if (values[i]) result |= 1u << i;
  return result;
}
std::int8_t ip_address_compare(rust::Slice<const std::uint8_t> a, rust::Slice<const std::uint8_t> b) { auto x = Ip(a), y = Ip(b); return x == y ? 0 : x < y ? -1 : 1; }
std::unique_ptr<NativeSocketAddress> socket_address_new(rust::Slice<const std::uint8_t> ip, std::uint16_t port) { return std::make_unique<NativeSocketAddress>(webrtc::SocketAddress(Ip(ip), port)); }
std::unique_ptr<NativeSocketAddress> socket_address_host(rust::Slice<const std::uint8_t> host, std::uint16_t port) { return std::make_unique<NativeSocketAddress>(webrtc::SocketAddress(View(host), port)); }
std::unique_ptr<NativeSocketAddress> socket_address_empty(std::int32_t family) { return std::make_unique<NativeSocketAddress>(webrtc::EmptySocketAddressWithFamily(family)); }
std::unique_ptr<NativeSocketAddress> socket_address_clone(const NativeSocketAddress& address) { return std::make_unique<NativeSocketAddress>(address.value); }
void socket_address_clear(NativeSocketAddress& address) { address.value.Clear(); }
void socket_address_set_ip(NativeSocketAddress& address, rust::Slice<const std::uint8_t> bytes, bool resolved) { auto ip = Ip(bytes); if (resolved) address.value.SetResolvedIP(ip); else address.value.SetIP(ip); }
void socket_address_set_host(NativeSocketAddress& address, rust::Slice<const std::uint8_t> host) { address.value.SetIP(View(host)); }
void socket_address_set_port(NativeSocketAddress& address, std::uint16_t port) { address.value.SetPort(port); }
void socket_address_set_scope(NativeSocketAddress& address, std::int32_t scope) { address.value.SetScopeID(scope); }
std::uint8_t socket_address_parse(NativeSocketAddress& address, rust::Slice<const std::uint8_t> bytes) {
  auto text = View(bytes);
  if (text.empty()) return 2;
  auto colon = text.find(':');
  if (text[0] == '[') {
    auto close = text.rfind(']');
    colon = close == absl::string_view::npos ? absl::string_view::npos : text.find(':', close);
  }
  if (colon != absl::string_view::npos) {
    // Match the SDK's strtoul -> int conversion, including partial parses.
    int port = static_cast<int>(std::strtoul(std::string(text.substr(colon + 1)).c_str(), nullptr, 10));
    if (port < 0 || port > 65535) return 3;
  }
  return address.value.FromString(text) ? 1 : 0;
}
rust::Vec<std::uint8_t> socket_address_bytes(const NativeSocketAddress& address, std::uint8_t kind) {
  const auto& a = address.value;
  switch (kind) {
    case 0: return Bytes(a.hostname()); case 1: return IpBytes(a.ipaddr()); case 2: return Bytes(a.HostAsURIString());
    case 3: return Bytes(a.HostAsSensitiveURIString()); case 4: return Bytes(a.PortAsString());
    case 5: return Bytes(a.ToString()); case 6: return Bytes(a.ToSensitiveString());
    default: return Bytes(a.ToSensitiveNameAndAddressString());
  }
}
std::int64_t socket_address_info(const NativeSocketAddress& address, std::uint8_t kind) {
  const auto& a = address.value;
  switch (kind) { case 0: return a.family(); case 1: return a.ip(); case 2: return a.port(); case 3: return a.scope_id(); default: return static_cast<int>(a.GetIPAddressType()); }
}
std::uint32_t socket_address_predicates(const NativeSocketAddress& address) {
  const auto& a = address.value; std::uint32_t result = 0;
  const bool values[] = {a.IsNil(), a.IsComplete(), a.IsAnyIP(), a.IsLoopbackIP(), a.IsPrivateIP(), a.IsUnresolvedIP()};
  for (std::size_t i = 0; i < 6; ++i) if (values[i]) result |= 1u << i;
  return result;
}
std::uint64_t socket_address_hash(const NativeSocketAddress& address) { return address.value.Hash(); }
std::int8_t socket_address_compare(const NativeSocketAddress& a, const NativeSocketAddress& b) { return a.value == b.value ? 0 : a.value < b.value ? -1 : 1; }
bool socket_address_equal_ip(const NativeSocketAddress& a, const NativeSocketAddress& b) { return a.value.EqualIPs(b.value); }
bool socket_address_equal_port(const NativeSocketAddress& a, const NativeSocketAddress& b) { return a.value.EqualPorts(b.value); }
std::size_t socket_address_native_size(std::uint8_t kind) { return kind == 2 ? sizeof(sockaddr_in) : sizeof(sockaddr_storage); }
rust::Vec<std::uint8_t> socket_address_storage(const NativeSocketAddress& address, std::uint8_t kind) {
  if (kind == 2) { sockaddr_in value{}; address.value.ToSockAddr(&value); return Bytes(&value, sizeof(value)); }
  sockaddr_storage value{};
  if (kind == 1) address.value.ToDualStackSockAddrStorage(&value); else address.value.ToSockAddrStorage(&value);
  return Bytes(&value, sizeof(value));
}
std::size_t socket_address_storage_written(const NativeSocketAddress& address, std::uint8_t kind) {
  if (kind == 2) return sizeof(sockaddr_in);
  sockaddr_storage value{};
  return kind == 1 ? address.value.ToDualStackSockAddrStorage(&value) : address.value.ToSockAddrStorage(&value);
}
std::unique_ptr<NativeSocketAddress> socket_address_from_storage(rust::Slice<const std::uint8_t> bytes, bool ipv4) {
  webrtc::SocketAddress address;
  if (ipv4) {
    if (bytes.size() != sizeof(sockaddr_in)) return nullptr;
    sockaddr_in value{}; std::memcpy(&value, bytes.data(), sizeof(value));
    if (!address.FromSockAddr(value)) return nullptr;
  } else {
    if (bytes.size() != sizeof(sockaddr_storage)) return nullptr;
    sockaddr_storage value{}; std::memcpy(&value, bytes.data(), sizeof(value));
    if (!webrtc::SocketAddressFromSockAddrStorage(value, &address)) return nullptr;
  }
  return std::make_unique<NativeSocketAddress>(address);
}
rust::Vec<std::uint8_t> ip_address_from_storage(rust::Slice<const std::uint8_t> bytes) {
  sockaddr_storage value{};
  if (!Storage(bytes, value)) return {};
  addrinfo info{}; info.ai_addr = reinterpret_cast<sockaddr*>(&value);
  webrtc::IPAddress ip;
  return webrtc::IPFromAddrInfo(&info, &ip) ? IpBytes(ip) : rust::Vec<std::uint8_t>{};
}
}  // namespace pulsebeam::webrtc_sys
