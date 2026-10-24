use std::{fmt, sync::Arc, time::Duration};

use crate::{PeerError, PeerErrorKind, ffi};

/// The pinned native identity generator supports P-256.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertificateCurve {
    P256,
    /// Preserves native curve values, including representable invalid inputs.
    Other(i32),
}

/// Native key parameters. Native validity is necessary, not a guarantee that
/// backend key generation will succeed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CertificateKeyParams {
    Rsa {
        modulus_bits: u32,
        public_exponent: u32,
    },
    Ecdsa {
        curve: CertificateCurve,
    },
}

impl Default for CertificateKeyParams {
    fn default() -> Self {
        Self::from_native(ffi::certificate_default_key_params(false))
    }
}

impl CertificateKeyParams {
    pub fn rsa_default() -> Self {
        Self::from_native(ffi::certificate_default_key_params(true))
    }

    pub fn is_valid(self) -> bool {
        ffi::certificate_key_params_valid(&self.to_native())
    }

    pub(crate) fn to_native(self) -> ffi::FfiCertificateKeyParams {
        match self {
            Self::Rsa {
                modulus_bits,
                public_exponent,
            } => ffi::FfiCertificateKeyParams {
                kind: 0,
                modulus_bits,
                public_exponent,
                curve: 0,
            },
            Self::Ecdsa { curve } => ffi::FfiCertificateKeyParams {
                kind: 1,
                modulus_bits: 0,
                public_exponent: 0,
                curve: match curve {
                    CertificateCurve::P256 => 0,
                    CertificateCurve::Other(value) => value,
                },
            },
        }
    }

    pub(crate) fn from_native(value: ffi::FfiCertificateKeyParams) -> Self {
        match value.kind {
            0 => Self::Rsa {
                modulus_bits: value.modulus_bits,
                public_exponent: value.public_exponent,
            },
            1 => Self::Ecdsa {
                curve: if value.curve == 0 {
                    CertificateCurve::P256
                } else {
                    CertificateCurve::Other(value.curve)
                },
            },
            _ => unreachable!("pinned native generator returned an unknown default key parameter"),
        }
    }
}

/// Exported private key and leaf certificate. Contains sensitive key material.
/// Debug output deliberately omits the private key.
#[derive(Clone, Eq, PartialEq)]
pub struct CertificatePem {
    pub private_key: String,
    pub certificate: String,
}

impl fmt::Debug for CertificatePem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertificatePem")
            .field("private_key", &"[redacted]")
            .field("certificate", &self.certificate)
            .finish()
    }
}

/// A native certificate-chain member, copied without borrowing the resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CertificateData {
    pub pem: String,
    pub der: Vec<u8>,
    /// Unix epoch seconds. Only native's unavailable sentinel becomes None.
    pub expiration_unix_seconds: Option<i64>,
    pub signature_digest_algorithm: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CertificateFingerprint {
    pub algorithm: String,
    /// Native RFC 4572 colon-separated hexadecimal representation.
    pub value: String,
    pub digest: Vec<u8>,
}

struct CertificateInner {
    native: cxx::SharedPtr<ffi::NativeCertificate>,
}

// SAFETY: NativeCertificate retains an atomic native RTCCertificate reference.
// The adapter is compile-time restricted to the qualified pinned BoringSSL
// backend: immutable CRYPTO_BUFFER certificate bytes and concurrently readable
// EVP_PKEY objects. All exported operations are reads or create new objects;
// no SSLIdentity, EVP_PKEY, SSL_CTX, or SSL mutation is exposed. Last-owner
// destruction releases reference-counted backend objects, never borrowed ones.
// Authority: BoringSSL pool.h and evp.h thread-safety contracts, and pinned
// boringssl_certificate.cc / boringssl_identity.cc / openssl_key_pair.cc.
unsafe impl Send for CertificateInner {}
unsafe impl Sync for CertificateInner {}

/// Owned immutable native RTCCertificate resource. Clone retains the same
/// resource, including its native identity for peer configuration updates.
/// Generation is synchronous and may be expensive; it uses native crypto
/// randomness and system epoch time, not a synthesized binding clock.
#[derive(Clone)]
pub struct Certificate {
    inner: Arc<CertificateInner>,
}

impl Certificate {
    /// Matches native synchronous generation. A supplied lifetime is truncated
    /// to seconds and capped by native at one year; None uses native's default.
    pub fn generate(
        params: CertificateKeyParams,
        lifetime: Option<Duration>,
    ) -> Result<Self, PeerError> {
        if !params.is_valid() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "native certificate key parameters are invalid".into(),
            });
        }
        let millis = lifetime.map(|value| value.as_millis().min(u64::MAX as u128) as u64);
        Self::from_native(ffi::certificate_generate(
            &params.to_native(),
            millis.is_some(),
            millis.unwrap_or(0),
        ))
        .ok_or_else(|| PeerError {
            kind: PeerErrorKind::Internal,
            message: "native certificate generation failed".into(),
        })
    }

    /// Matches RTCCertificate::FromPEM. Native import is not a new binding
    /// policy for certificate trust, expiration, or peer negotiation.
    pub fn from_pem(pem: &CertificatePem) -> Result<Self, PeerError> {
        Self::import(pem, false)
    }

    /// Uses native SSLIdentity chain import and retains the entire chain.
    /// ToPEM still exports only the leaf, matching RTCCertificate::ToPEM.
    pub fn from_pem_chain(pem: &CertificatePem) -> Result<Self, PeerError> {
        Self::import(pem, true)
    }

    fn import(pem: &CertificatePem, chain: bool) -> Result<Self, PeerError> {
        Self::from_native(ffi::certificate_from_pem(
            &pem.private_key,
            &pem.certificate,
            chain,
        ))
        .ok_or_else(|| PeerError {
            kind: PeerErrorKind::InvalidParameter,
            message: "native certificate PEM import failed".into(),
        })
    }

    pub fn to_pem(&self) -> CertificatePem {
        let value = ffi::certificate_to_pem(self.native());
        CertificatePem {
            private_key: value.private_key,
            certificate: value.certificate,
        }
    }

    /// Native unsigned Unix epoch milliseconds, not environment-clock time.
    pub fn expires_at_unix_millis(&self) -> u64 {
        ffi::certificate_expires(self.native())
    }

    pub fn has_expired_at_unix_millis(&self, now: u64) -> bool {
        ffi::certificate_has_expired(self.native(), now)
    }

    pub fn chain(&self) -> Vec<CertificateData> {
        ffi::certificate_chain(self.native())
            .into_iter()
            .map(|value| CertificateData {
                pem: value.pem,
                der: value.der,
                expiration_unix_seconds: (value.expiration_unix_seconds != -1)
                    .then_some(value.expiration_unix_seconds),
                signature_digest_algorithm: value
                    .has_signature_digest_algorithm
                    .then_some(value.signature_digest_algorithm),
            })
            .collect()
    }

    /// Native digest lookup, without a binding algorithm allowlist.
    pub fn fingerprint(&self, algorithm: &str) -> Option<CertificateFingerprint> {
        let value = ffi::certificate_fingerprint(self.native(), algorithm);
        value.valid.then_some(CertificateFingerprint {
            algorithm: value.algorithm,
            value: value.value,
            digest: value.digest,
        })
    }

    /// Native scoped-reference identity, which is distinct from certificate
    /// value equality. RTCConfiguration certificate comparisons use this.
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::certificate_same_resource(self.native(), other.native())
    }

    pub(crate) fn native(&self) -> &ffi::NativeCertificate {
        self.inner
            .native
            .as_ref()
            .expect("certificate resource is non-null")
    }

    pub(crate) fn from_native(native: cxx::SharedPtr<ffi::NativeCertificate>) -> Option<Self> {
        (!native.is_null()).then(|| Self {
            inner: Arc::new(CertificateInner { native }),
        })
    }
}

impl PartialEq for Certificate {
    /// Matches native RTCCertificate value equality, not scoped-ref identity.
    fn eq(&self, other: &Self) -> bool {
        ffi::certificate_equal(self.native(), other.native())
    }
}
impl Eq for Certificate {}

impl fmt::Debug for Certificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Certificate")
            .field("expires_at_unix_millis", &self.expires_at_unix_millis())
            .finish_non_exhaustive()
    }
}
