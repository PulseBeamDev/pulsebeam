//! Native SFrame values and key-management handles. The pinned stock creation
//! methods currently return errors, possibly after enabling negotiation state.
use crate::peer::PeerInner;
use crate::{RtcError, ffi};
use std::{fmt, rc::Rc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SframeMode {
    PerFrame,
    PerPacket,
    Other(i32),
}
impl SframeMode {
    pub const fn raw(self) -> i32 {
        match self {
            Self::PerFrame => 0,
            Self::PerPacket => 1,
            Self::Other(value) => value,
        }
    }
    pub const fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::PerFrame,
            1 => Self::PerPacket,
            value => Self::Other(value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SframeCipherSuite {
    Aes128CtrHmacSha256_80,
    Aes128CtrHmacSha256_64,
    Aes128CtrHmacSha256_32,
    Aes128GcmSha256_128,
    Aes256GcmSha512_128,
    Other(i32),
}
impl SframeCipherSuite {
    pub const fn raw(self) -> i32 {
        match self {
            Self::Aes128CtrHmacSha256_80 => 0,
            Self::Aes128CtrHmacSha256_64 => 1,
            Self::Aes128CtrHmacSha256_32 => 2,
            Self::Aes128GcmSha256_128 => 3,
            Self::Aes256GcmSha512_128 => 4,
            Self::Other(value) => value,
        }
    }
    pub const fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::Aes128CtrHmacSha256_80,
            1 => Self::Aes128CtrHmacSha256_64,
            2 => Self::Aes128CtrHmacSha256_32,
            3 => Self::Aes128GcmSha256_128,
            4 => Self::Aes256GcmSha512_128,
            value => Self::Other(value),
        }
    }
}
/// Complete owning SDK aggregate. Both fields are explicit; native default
/// initialization without braces leaves them indeterminate, so there is no
/// invented Default. Unknown enum metadata is preserved, not normalized.
pub struct SframeEncrypterInit {
    native: cxx::UniquePtr<ffi::NativeSframeEncrypterInit>,
}
impl SframeEncrypterInit {
    pub fn new(mode: SframeMode, suite: SframeCipherSuite) -> Self {
        Self {
            native: ffi::sframe_init_new(mode.raw(), suite.raw()),
        }
    }
    pub(crate) fn native(&self) -> &ffi::NativeSframeEncrypterInit {
        self.native.as_ref().expect("owned SFrame init")
    }
    pub fn mode(&self) -> SframeMode {
        SframeMode::from_raw(ffi::sframe_init_fields(self.native()).mode)
    }
    pub fn cipher_suite(&self) -> SframeCipherSuite {
        SframeCipherSuite::from_raw(ffi::sframe_init_fields(self.native()).suite)
    }
    pub fn set_mode(&mut self, value: SframeMode) {
        ffi::sframe_init_set_mode(self.native.pin_mut(), value.raw());
    }
    pub fn set_cipher_suite(&mut self, value: SframeCipherSuite) {
        ffi::sframe_init_set_suite(self.native.pin_mut(), value.raw());
    }
    pub fn assign(&mut self, source: &Self) {
        ffi::sframe_init_assign(self.native.pin_mut(), source.native());
    }
}
impl Clone for SframeEncrypterInit {
    fn clone(&self) -> Self {
        Self {
            native: ffi::sframe_init_clone(self.native()),
        }
    }
    fn clone_from(&mut self, source: &Self) {
        self.assign(source);
    }
}
impl fmt::Debug for SframeEncrypterInit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SframeEncrypterInit")
            .field("mode", &self.mode())
            .field("cipher_suite", &self.cipher_suite())
            .finish()
    }
}
// SAFETY: independently owned two-enum SDK value; shared access only reads and
// all mutation requires an exclusive loan. No retained peer/callback/origin.
unsafe impl Send for SframeEncrypterInit {}
unsafe impl Sync for SframeEncrypterInit {}

#[derive(Debug)]
pub enum SframeCreateError {
    Binding(crate::PeerError),
    Native(RtcError),
    DetachedOwner,
    MissingNativeProduct,
}
impl From<crate::PeerError> for SframeCreateError {
    fn from(value: crate::PeerError) -> Self {
        Self::Binding(value)
    }
}
impl fmt::Display for SframeCreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binding(value) => value.fmt(f),
            Self::Native(value) => value.fmt(f),
            Self::DetachedOwner => {
                f.write_str("cannot establish live SDK transceiver owner for SFrame creation")
            }
            Self::MissingNativeProduct => {
                f.write_str("SDK SFrame creation returned success without a key-management handle")
            }
        }
    }
}
impl std::error::Error for SframeCreateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Binding(value) => Some(value),
            Self::Native(value) => Some(value),
            Self::DetachedOwner | Self::MissingNativeProduct => None,
        }
    }
}
/// Owning native encryption key-management handle. This SDK pin has no stock
/// successful producer. Local handles retain their peer and are not Send/Sync.
///
/// ```compile_fail
/// fn send<T: Send>() {} send::<pulsebeam_webrtc_sys::SframeEncrypter>();
/// ```
pub struct SframeEncrypter {
    native: cxx::UniquePtr<ffi::NativeSframeEncrypter>,
    peer: Rc<PeerInner>,
}
impl SframeEncrypter {
    pub(crate) fn from_creation(
        value: ffi::FfiSframeEncrypterCreation,
        peer: Rc<PeerInner>,
    ) -> Result<Self, SframeCreateError> {
        if value.detached_owner {
            return Err(SframeCreateError::DetachedOwner);
        }
        if !value.error.is_null() {
            return Err(SframeCreateError::Native(RtcError::from_native(
                value.error,
            )));
        }
        if value.product.is_null() {
            return Err(SframeCreateError::MissingNativeProduct);
        }
        Ok(Self {
            native: value.product,
            peer,
        })
    }
    fn native(&self) -> &ffi::NativeSframeEncrypter {
        self.native.as_ref().expect("owned SFrame encrypter")
    }
    pub fn set_encryption_key(&self, id: u64, key_material: &[u8]) -> RtcError {
        RtcError::from_native(ffi::sframe_set_encryption_key(
            self.native(),
            id,
            key_material,
        ))
    }
}
impl Clone for SframeEncrypter {
    fn clone(&self) -> Self {
        Self {
            native: ffi::sframe_encrypter_clone(self.native()),
            peer: self.peer.clone(),
        }
    }
}
impl fmt::Debug for SframeEncrypter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SframeEncrypter").finish_non_exhaustive()
    }
}
/// Owning native decryption key-management handle, distinct from the legacy
/// FrameDecryptor. No stock successful producer exists at the selected pin.
///
/// ```compile_fail
/// fn send<T: Send>() {} send::<pulsebeam_webrtc_sys::SframeDecrypter>();
/// ```
pub struct SframeDecrypter {
    native: cxx::UniquePtr<ffi::NativeSframeDecrypter>,
    peer: Rc<PeerInner>,
}
impl SframeDecrypter {
    pub(crate) fn from_creation(
        value: ffi::FfiSframeDecrypterCreation,
        peer: Rc<PeerInner>,
    ) -> Result<Self, SframeCreateError> {
        if value.detached_owner {
            return Err(SframeCreateError::DetachedOwner);
        }
        if !value.error.is_null() {
            return Err(SframeCreateError::Native(RtcError::from_native(
                value.error,
            )));
        }
        if value.product.is_null() {
            return Err(SframeCreateError::MissingNativeProduct);
        }
        Ok(Self {
            native: value.product,
            peer,
        })
    }
    fn native(&self) -> &ffi::NativeSframeDecrypter {
        self.native.as_ref().expect("owned SFrame decrypter")
    }
    pub fn add_decryption_key(&self, id: u64, key_material: &[u8]) -> RtcError {
        RtcError::from_native(ffi::sframe_add_decryption_key(
            self.native(),
            id,
            key_material,
        ))
    }
    pub fn remove_decryption_key(&self, id: u64) -> RtcError {
        RtcError::from_native(ffi::sframe_remove_decryption_key(self.native(), id))
    }
}
impl Clone for SframeDecrypter {
    fn clone(&self) -> Self {
        Self {
            native: ffi::sframe_decrypter_clone(self.native()),
            peer: self.peer.clone(),
        }
    }
}
impl fmt::Debug for SframeDecrypter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SframeDecrypter").finish_non_exhaustive()
    }
}
