use std::mem::size_of;

use super::{
    CurrentBinding, CurrentEncodingLocation, RtpCapabilities, RtpCodecCapability,
    RtpCodecParameters, RtpEncoding, RtpExtensionMapping,
};

pub(super) fn identity_bytes(count: usize) -> Option<usize> {
    count.checked_mul(size_of::<(u32, bool)>())
}

pub(super) fn capability_bytes(capabilities: &RtpCapabilities) -> Option<usize> {
    let mut size = PayloadSize(0);
    size.array::<RtpCodecCapability>(capabilities.codecs.capacity())?;
    for codec in &capabilities.codecs {
        size.codec(&codec.parameters)?;
    }
    size.array::<String>(capabilities.extensions.len())?;
    for extension in &capabilities.extensions {
        size.add(extension.capacity())?;
    }
    Some(size.0)
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
    size.array::<usize>(record.extension_order.len())?;
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
    size.codec(&binding.codec.parameters)?;
    Some(size.0)
}

struct PayloadSize(usize);

impl PayloadSize {
    fn codec(&mut self, codec: &RtpCodecParameters) -> Option<()> {
        self.add(codec.name.capacity())?;
        self.array::<(String, String)>(codec.fmtp.len())?;
        for (key, value) in &codec.fmtp {
            self.add(key.capacity())?;
            self.add(value.capacity())?;
        }
        self.array::<String>(codec.feedback.len())?;
        for feedback in &codec.feedback {
            self.add(feedback.capacity())?;
        }
        Some(())
    }

    fn add(&mut self, bytes: usize) -> Option<()> {
        self.0 = self.0.checked_add(bytes)?;
        Some(())
    }

    fn array<T>(&mut self, count: usize) -> Option<()> {
        self.add(count.checked_mul(size_of::<T>())?)
    }
}
