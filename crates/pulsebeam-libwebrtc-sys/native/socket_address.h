#pragma once
#include <cstdint>
#include <memory>
#include "rust/cxx.h"
#include "rtc_base/socket_address.h"
namespace pulsebeam::webrtc_sys {
class NativeSocketAddress {
 public:
  explicit NativeSocketAddress(webrtc::SocketAddress value) : value(std::move(value)) {}
  webrtc::SocketAddress value;
};
std::int32_t address_family(std::uint8_t kind);
rust::Vec<std::uint8_t> ip_address_parse(rust::Slice<const std::uint8_t> text);
rust::Vec<std::uint8_t> ip_address_special(std::int32_t family, bool loopback);
rust::Vec<std::uint8_t> ip_address_transform(rust::Slice<const std::uint8_t> ip, std::uint8_t operation, std::int32_t length);
rust::Vec<std::uint8_t> ip_address_text(rust::Slice<const std::uint8_t> ip, std::uint8_t kind, std::int32_t flags);
std::int64_t ip_address_info(rust::Slice<const std::uint8_t> ip, std::uint8_t kind);
std::uint64_t ip_address_hash(rust::Slice<const std::uint8_t> ip);
std::uint32_t ip_address_predicates(rust::Slice<const std::uint8_t> ip);
std::int8_t ip_address_compare(rust::Slice<const std::uint8_t> a, rust::Slice<const std::uint8_t> b);
std::unique_ptr<NativeSocketAddress> socket_address_new(rust::Slice<const std::uint8_t> ip, std::uint16_t port);
std::unique_ptr<NativeSocketAddress> socket_address_host(rust::Slice<const std::uint8_t> host, std::uint16_t port);
std::unique_ptr<NativeSocketAddress> socket_address_empty(std::int32_t family);
std::unique_ptr<NativeSocketAddress> socket_address_clone(const NativeSocketAddress& address);
void socket_address_clear(NativeSocketAddress& address);
void socket_address_set_ip(NativeSocketAddress& address, rust::Slice<const std::uint8_t> ip, bool resolved);
void socket_address_set_host(NativeSocketAddress& address, rust::Slice<const std::uint8_t> host);
void socket_address_set_port(NativeSocketAddress& address, std::uint16_t port);
void socket_address_set_scope(NativeSocketAddress& address, std::int32_t scope);
std::uint8_t socket_address_parse(NativeSocketAddress& address, rust::Slice<const std::uint8_t> text);
rust::Vec<std::uint8_t> socket_address_bytes(const NativeSocketAddress& address, std::uint8_t kind);
std::int64_t socket_address_info(const NativeSocketAddress& address, std::uint8_t kind);
std::uint32_t socket_address_predicates(const NativeSocketAddress& address);
std::uint64_t socket_address_hash(const NativeSocketAddress& address);
std::int8_t socket_address_compare(const NativeSocketAddress& a, const NativeSocketAddress& b);
bool socket_address_equal_ip(const NativeSocketAddress& a, const NativeSocketAddress& b);
bool socket_address_equal_port(const NativeSocketAddress& a, const NativeSocketAddress& b);
rust::Vec<std::uint8_t> socket_address_storage(const NativeSocketAddress& address, std::uint8_t kind);
std::size_t socket_address_native_size(std::uint8_t kind);
std::size_t socket_address_storage_written(const NativeSocketAddress& address, std::uint8_t kind);
std::unique_ptr<NativeSocketAddress> socket_address_from_storage(rust::Slice<const std::uint8_t> bytes, bool ipv4);
rust::Vec<std::uint8_t> ip_address_from_storage(rust::Slice<const std::uint8_t> bytes);
}  // namespace pulsebeam::webrtc_sys
