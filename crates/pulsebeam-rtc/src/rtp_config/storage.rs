use std::mem::size_of;

use super::{CurrentBinding, CurrentEncodingLocation, RtpEncoding, RtpExtensionMapping};

pub(super) fn identity_bytes(count: usize) -> Option<usize> {
    count.checked_mul(size_of::<(u32, bool)>())
}

pub(super) fn live_bytes(record: &CurrentBinding) -> Option<usize> {
    let binding = &record.binding;
    let mut size = PayloadSize(size_of::<(u64, CurrentBinding)>());
    size.add(size_of::<((bool, String), u64)>())?;
    size.add(binding.mid.capacity())?;
    size.add(binding.rtcp.cname.capacity())?;
    // The MID index owns a separate copy of the string contents.
    size.add(binding.mid.len())?;
    size.array::<RtpExtensionMapping>(binding.extensions.capacity())?;
    size.array::<RtpEncoding>(binding.encodings.capacity())?;
    size.array::<(u32, CurrentEncodingLocation)>(binding.encodings.len())?;
    for extension in &binding.extensions {
        size.add(extension.uri.capacity())?;
    }
    for encoding in &binding.encodings {
        if let Some(rid) = &encoding.rid {
            size.add(rid.capacity())?;
        }
    }
    let codec = &binding.codec.parameters;
    size.add(codec.name.capacity())?;
    size.array::<(String, String)>(codec.fmtp.len())?;
    for (key, value) in &codec.fmtp {
        size.add(key.capacity())?;
        size.add(value.capacity())?;
    }
    size.array::<String>(codec.feedback.len())?;
    for feedback in &codec.feedback {
        size.add(feedback.capacity())?;
    }
    Some(size.0)
}

struct PayloadSize(usize);

impl PayloadSize {
    fn add(&mut self, bytes: usize) -> Option<()> {
        self.0 = self.0.checked_add(bytes)?;
        Some(())
    }

    fn array<T>(&mut self, count: usize) -> Option<()> {
        self.add(count.checked_mul(size_of::<T>())?)
    }
}
