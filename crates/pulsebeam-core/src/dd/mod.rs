pub mod bits;
pub mod model;
pub mod read;
pub mod temporal;
pub mod write;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
#[cfg(test)]
mod tests;

pub use model::*;
pub use read::{DdReadError, DependencyDescriptorReader, read_mandatory};
pub use write::{DdWriteError, DependencyDescriptorWriter};

use arrayvec::ArrayVec;

pub const URI: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";

/// The descriptor exactly as it arrived. Parsing needs the sending stream's
/// template structure, which a stateless serializer has no way to reach, so the
/// bytes are carried through to per-stream ingress handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDependencyDescriptor(pub ArrayVec<u8, MAX_DD_LEN>);

#[cfg(feature = "str0m")]
pub use serializer::Serializer;

#[cfg(feature = "str0m")]
mod serializer {
    use super::*;
    use str0m::rtp::{ExtensionSerializer, ExtensionValues};

    #[derive(Debug)]
    pub struct Serializer;

    impl ExtensionSerializer for Serializer {
        fn write_to(&self, buf: &mut [u8], ev: &ExtensionValues) -> usize {
            let Some(raw) = ev.user_values.get::<RawDependencyDescriptor>() else {
                return 0;
            };
            debug_assert!(raw.0.len() >= MANDATORY_LEN);
            let Some(dst) = buf.get_mut(..raw.0.len()) else {
                return 0;
            };
            dst.copy_from_slice(&raw.0);
            raw.0.len()
        }

        fn parse_value(&self, buf: &[u8], ev: &mut ExtensionValues) -> bool {
            if buf.len() < MANDATORY_LEN || buf.len() > MAX_DD_LEN {
                return false;
            }
            ev.user_values
                .set(RawDependencyDescriptor(buf.iter().copied().collect()));
            true
        }

        fn is_video(&self) -> bool {
            true
        }

        fn is_audio(&self) -> bool {
            false
        }

        /// str0m evaluates this for every registered extension on every packet, so
        /// answering unconditionally would push all traffic into the two-byte form.
        /// Descriptors longer than the one-byte form's 16-byte limit only exist when
        /// the value is present.
        fn requires_two_byte_form(&self, ev: &ExtensionValues) -> bool {
            ev.user_values.get::<RawDependencyDescriptor>().is_some()
        }
    }
}
