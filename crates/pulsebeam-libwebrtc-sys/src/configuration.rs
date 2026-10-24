use std::ops::{BitOr, BitOrAssign};

use crate::ffi;

/// Open native adapter-type bitmask. Unknown bits are retained unchanged.
/// `NONE` ignores no adapter category, not every network interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkAdapterMask(u32);

impl NetworkAdapterMask {
    pub const NONE: Self = Self(0);
    pub const ETHERNET: Self = Self(1 << 0);
    pub const WIFI: Self = Self(1 << 1);
    pub const CELLULAR: Self = Self(1 << 2);
    pub const VPN: Self = Self(1 << 3);
    pub const LOOPBACK: Self = Self(1 << 4);
    pub const ANY_ADDRESS: Self = Self(1 << 5);
    pub const CELLULAR_2G: Self = Self(1 << 6);
    pub const CELLULAR_3G: Self = Self(1 << 7);
    pub const CELLULAR_4G: Self = Self(1 << 8);
    pub const CELLULAR_5G: Self = Self(1 << 9);

    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl Default for NetworkAdapterMask {
    fn default() -> Self {
        Self::LOOPBACK
    }
}

impl BitOr for NetworkAdapterMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for NetworkAdapterMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Native SSL maximum-version selector, including the native TLS/DTLS aliases.
/// Peer connections use this for DTLS, not for TURN TLS. In the pinned engine,
/// DTLS has a 1.2 minimum: legacy values cap at 1.2 rather than enabling an older
/// protocol. `Unspecified` and `DTLS_13` permit 1.3 when the build supports it,
/// otherwise 1.2. This selector alone does not prove the negotiated version.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(i32)]
pub enum SslProtocolVersion {
    Unspecified = -1,
    Tls10 = 0,
    Tls11 = 1,
    Tls12 = 2,
    #[default]
    Tls13 = 3,
}

impl SslProtocolVersion {
    pub const DTLS_10: Self = Self::Tls11;
    pub const DTLS_12: Self = Self::Tls12;
    pub const DTLS_13: Self = Self::Tls13;
}

/// Options copied by the native factory into subsequently created peers.
/// Updating these options does not alter peers that already exist. Native
/// `SetOptions` has no validation result or corresponding readback operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FactoryOptions {
    /// Disable native media encryption and DTLS for subsequently created peers.
    /// This is insecure and also prevents DTLS-dependent capabilities such as
    /// SCTP data channels. Ordinary applications should retain `false`.
    pub disable_encryption: bool,
    /// Forward the native platform-network-monitor option. The pinned Linux
    /// C++ factory stores but does not consult this field, so setting it does
    /// not disable an installed network monitor. Platform wrappers may differ.
    pub disable_network_monitor: bool,
    /// Adapter categories ignored by subsequent peers' port allocators.
    /// The default ignores loopback only.
    pub ignored_network_adapters: NetworkAdapterMask,
    /// Maximum native secure protocol version; defaults to the DTLS 1.3 alias.
    pub ssl_max_version: SslProtocolVersion,
}

impl FactoryOptions {
    pub(crate) fn ffi(self) -> ffi::FfiFactoryOptions {
        ffi::FfiFactoryOptions {
            disable_encryption: self.disable_encryption,
            disable_network_monitor: self.disable_network_monitor,
            // A native int holds a bit pattern, not a nonnegative count.
            network_ignore_mask: self.ignored_network_adapters.bits() as i32,
            ssl_max_version: self.ssl_max_version as i32,
        }
    }
}
