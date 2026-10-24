use crate::{Environment, PeerError, PeerErrorKind, ffi};

/// Open IANA DTLS-SRTP protection-profile identifier.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DtlsSrtpCipherSuite(u16);

impl DtlsSrtpCipherSuite {
    pub const AES128_CM_SHA1_80: Self = Self(0x0001);
    pub const AES128_CM_SHA1_32: Self = Self(0x0002);
    pub const AEAD_AES128_GCM: Self = Self(0x0007);
    pub const AEAD_AES256_GCM: Self = Self(0x0008);

    pub const fn from_id(id: u16) -> Self {
        Self(id)
    }

    pub const fn id(self) -> u16 {
        self.0
    }
}

/// Open TLS/DTLS named-group identifier. Assigning a value does not imply
/// support by the selected native SSL implementation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct KeyExchangeGroup(u16);

impl KeyExchangeGroup {
    pub const SECP224R1: Self = Self(21);
    pub const SECP256R1: Self = Self(23);
    pub const SECP384R1: Self = Self(24);
    pub const SECP521R1: Self = Self(25);
    pub const X25519: Self = Self(29);
    pub const X25519_MLKEM768: Self = Self(0x11ec);

    pub const fn from_id(id: u16) -> Self {
        Self(id)
    }

    pub const fn id(self) -> u16 {
        self.0
    }

    /// Query groups supported by this native binary, not a binding allowlist.
    pub fn supported() -> Vec<Self> {
        ffi::crypto_supported_key_exchange_groups()
            .into_iter()
            .map(Self)
            .collect()
    }

    /// Native name, or `None` when this binary does not recognize the group.
    pub fn name(self) -> Option<String> {
        let mut name = String::new();
        ffi::crypto_key_exchange_group_name(self.0, &mut name).then_some(name)
    }
}

/// Ordered native ephemeral key-exchange selection. Empty and unknown values
/// are preserved; native support queries and actual handshake outcomes remain
/// authoritative. Defaults come from the native SSL implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyExchangeGroups(Vec<KeyExchangeGroup>);

impl Default for KeyExchangeGroups {
    fn default() -> Self {
        Self(
            ffi::crypto_default_options()
                .key_exchange_groups
                .into_iter()
                .map(KeyExchangeGroup)
                .collect(),
        )
    }
}

impl KeyExchangeGroups {
    pub fn new(groups: Vec<KeyExchangeGroup>) -> Self {
        Self(groups)
    }

    pub fn enabled(&self) -> &[KeyExchangeGroup] {
        &self.0
    }

    pub fn set_enabled(&mut self, groups: Vec<KeyExchangeGroup>) {
        self.0 = groups;
    }

    /// Move this group to the front, removing all its previous occurrences.
    /// Other groups retain their order and duplicates, as in native `AddFirst`.
    pub fn add_first(&mut self, group: KeyExchangeGroup) {
        self.0.retain(|existing| *existing != group);
        self.0.insert(0, group);
    }

    /// Apply native field-trial defaults and remove disabled groups. The
    /// environment is borrowed only during this synchronous value operation.
    /// The result is owned and can be moved into actor configuration.
    pub fn update(&mut self, environment: &Environment, disabled: &[KeyExchangeGroup]) {
        let options = CryptoOptions {
            key_exchange_groups: self.clone(),
            ..Default::default()
        };
        let disabled: Vec<_> = disabled.iter().map(|group| group.id()).collect();
        *self = CryptoOptions::from_native(ffi::crypto_update_key_exchange_groups(
            environment.native(),
            &options.to_native(),
            &disabled,
        ))
        .key_exchange_groups;
    }
}

/// RFC 9335 RTP-header encryption negotiation policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum CryptexPolicy {
    #[default]
    Disabled,
    Negotiate,
    Require,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SrtpCryptoOptions {
    pub enable_gcm_crypto_suites: bool,
    pub prefer_gcm_crypto_suites: bool,
    /// Potentially insecure short authentication tag; disabled by default.
    pub enable_aes128_sha1_32_crypto_cipher: bool,
    pub enable_aes128_sha1_80_crypto_cipher: bool,
    /// Native RFC 6904 support also depends on its relevant field trial.
    pub enable_encrypted_rtp_header_extensions: bool,
    pub cryptex_policy: CryptexPolicy,
}

impl Default for SrtpCryptoOptions {
    fn default() -> Self {
        CryptoOptions::default().srtp
    }
}

/// Complete owned native peer crypto configuration. This configures native
/// negotiation and extension-provider policy, not a Rust cryptographic engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CryptoOptions {
    pub srtp: SrtpCryptoOptions,
    /// Require a frame encryptor on senders and a frame decryptor on receivers
    /// before they may send/receive. This does not install either provider.
    pub require_frame_encryption: bool,
    pub key_exchange_groups: KeyExchangeGroups,
}

impl Default for CryptoOptions {
    fn default() -> Self {
        Self::from_native(ffi::crypto_default_options())
    }
}

impl CryptoOptions {
    pub fn no_gcm() -> Self {
        let mut options = Self::default();
        options.srtp.enable_gcm_crypto_suites = false;
        options
    }

    pub fn prefer_gcm() -> Self {
        let mut options = Self::default();
        options.srtp.enable_gcm_crypto_suites = true;
        options.srtp.prefer_gcm_crypto_suites = true;
        options
    }

    /// Native DTLS-SRTP profile preference order. Native code aborts if all
    /// profiles are disabled; this safe query instead returns `InvalidParameter`.
    /// A nonempty result describes options, not successful peer negotiation.
    pub fn supported_dtls_srtp_cipher_suites(&self) -> Result<Vec<DtlsSrtpCipherSuite>, PeerError> {
        let mut suites = Vec::new();
        if !ffi::crypto_supported_srtp_suites(&self.to_native(), &mut suites) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "native DTLS-SRTP requires at least one enabled protection profile".into(),
            });
        }
        Ok(suites.into_iter().map(DtlsSrtpCipherSuite).collect())
    }

    pub(crate) fn from_native(options: ffi::FfiCryptoOptions) -> Self {
        Self {
            srtp: SrtpCryptoOptions {
                enable_gcm_crypto_suites: options.enable_gcm_crypto_suites,
                prefer_gcm_crypto_suites: options.prefer_gcm_crypto_suites,
                enable_aes128_sha1_32_crypto_cipher: options.enable_aes128_sha1_32_crypto_cipher,
                enable_aes128_sha1_80_crypto_cipher: options.enable_aes128_sha1_80_crypto_cipher,
                enable_encrypted_rtp_header_extensions: options
                    .enable_encrypted_rtp_header_extensions,
                cryptex_policy: match options.cryptex_policy {
                    0 => CryptexPolicy::Disabled,
                    1 => CryptexPolicy::Negotiate,
                    2 => CryptexPolicy::Require,
                    _ => unreachable!("invalid native cryptex policy"),
                },
            },
            require_frame_encryption: options.require_frame_encryption,
            key_exchange_groups: KeyExchangeGroups::new(
                options
                    .key_exchange_groups
                    .into_iter()
                    .map(KeyExchangeGroup)
                    .collect(),
            ),
        }
    }

    pub(crate) fn to_native(&self) -> ffi::FfiCryptoOptions {
        ffi::FfiCryptoOptions {
            enable_gcm_crypto_suites: self.srtp.enable_gcm_crypto_suites,
            prefer_gcm_crypto_suites: self.srtp.prefer_gcm_crypto_suites,
            enable_aes128_sha1_32_crypto_cipher: self.srtp.enable_aes128_sha1_32_crypto_cipher,
            enable_aes128_sha1_80_crypto_cipher: self.srtp.enable_aes128_sha1_80_crypto_cipher,
            enable_encrypted_rtp_header_extensions: self
                .srtp
                .enable_encrypted_rtp_header_extensions,
            cryptex_policy: self.srtp.cryptex_policy as u8,
            require_frame_encryption: self.require_frame_encryption,
            key_exchange_groups: self
                .key_exchange_groups
                .0
                .iter()
                .map(|group| group.id())
                .collect(),
        }
    }
}
