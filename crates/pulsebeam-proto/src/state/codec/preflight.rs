use super::DecodeError;
use crate::state::*;

const MAX_DEPTH: usize = 16;
const MAX_ALLOCATION: usize = 4 * 1024 * 1024;

// Only known message fields recurse. Unknown length-delimited fields are opaque
// bytes to prost, not messages; unknown groups still consume the depth budget.
macro_rules! shapes {
    ($($name:ident { $($field:literal => $child:ident),* $(,)? }),* $(,)?) => {
        #[derive(Clone, Copy)]
        enum Shape { $($name),* }
        impl Shape {
            #[cfg(test)]
            const ALL: &'static [Self] = &[$(Self::$name),*];
            #[cfg(test)]
            fn name(self) -> &'static str {
                match self { $(Self::$name => stringify!($name)),* }
            }
            fn child(self, field: u32) -> Option<Self> {
                match (self, field) {
                    $($( (Self::$name, $field) => Some(Self::$child), )*)*
                    _ => None,
                }
            }
            fn storage(self) -> usize {
                match self { $(Self::$name => core::mem::size_of::<$name>().saturating_mul(2)),* }
            }
        }
    };
}

shapes! {
    ClientStateBatch { 1 => ClientStateRecord },
    ClientStateRecord { 2 => DesiredState, 3 => ExecutionReport },
    ExecutionReport { 3 => Failure },
    ServerStateBatch { 1 => ServerStateRecord },
    ServerStateRecord { 1 => ResourceKey, 2 => ConfirmedState },
    ResourceKey {},
    DesiredState { 2 => ResourceSpec },
    ConfirmedState { 2 => Failure, 3 => LiveResource, 4 => AbsentState, 5 => SessionState, 6 => Participant, 7 => Track },
    AbsentState {},
    SessionState { 1 => Authorization },
    Authorization {},
    Participant {},
    Track { 4 => AudioTrack, 5 => VideoTrack, 6 => DataTrack },
    AudioTrack { 1 => CodecFormat },
    VideoTrack { 1 => CodecFormat },
    DataTrack {},
    ResourceSpec { 1 => ResourceSettings, 2 => PublicationSpec, 3 => SubscriptionSpec },
    ResourceSettings { 2 => AudioReceiveOptions, 3 => VideoReceiveOptions },
    PublicationSpec { 2 => AudioPublication, 3 => VideoPublication, 4 => DataPublication },
    AudioPublication { 1 => TrackBinding },
    VideoPublication { 1 => TrackBinding },
    DataPublication { 2 => TrackBinding },
    SubscriptionSpec { 3 => TrackBinding },
    LiveResource { 1 => ResourceSpec, 2 => Participant, 3 => Track, 5 => Failure },
    AudioReceiveOptions { 1 => PlayoutDelay },
    VideoReceiveOptions { 5 => PlayoutDelay },
    PlayoutDelay {},
    TrackBinding { 1 => RtpParameters, 2 => SctpParameters },
    SctpParameters {},
    RtpParameters { 2 => RtpCodecParameters, 3 => RtpHeaderExtensionParameters, 4 => RtpEncodingParameters, 5 => RtcpParameters },
    RtpCodecParameters { 4 => CodecFormat },
    CodecFormat { 4 => CodecParameter },
    CodecParameter {},
    RtpHeaderExtensionParameters {},
    RtpEncodingParameters {},
    RtcpParameters {},
    Failure {},
}

pub(super) fn check_client(bytes: &[u8]) -> Result<(), DecodeError> {
    message(bytes, Some(Shape::ClientStateBatch), 1, &mut 0)
}

pub(super) fn check_server(bytes: &[u8]) -> Result<(), DecodeError> {
    message(bytes, Some(Shape::ServerStateBatch), 1, &mut 0)
}

fn message(
    mut bytes: &[u8],
    shape: Option<Shape>,
    depth: usize,
    allocation: &mut usize,
) -> Result<(), DecodeError> {
    if depth > MAX_DEPTH {
        return Err(DecodeError::Structure);
    }
    charge(allocation, shape.map_or(0, Shape::storage))?;
    let mut records = 0usize;
    while !bytes.is_empty() {
        let (field, wire) = tag(&mut bytes)?;
        if matches!(
            shape,
            Some(Shape::ClientStateBatch | Shape::ServerStateBatch)
        ) && field == 1
        {
            records = records.saturating_add(1);
            if records > super::MAX_RECORDS {
                return Err(DecodeError::Structure);
            }
        }
        match wire {
            0 => {
                varint(&mut bytes)?;
            }
            1 => {
                take(&mut bytes, 8)?;
            }
            2 => {
                let len = usize::try_from(varint(&mut bytes)?)
                    .map_err(|_| DecodeError::InvalidProtobuf)?;
                let child = take(&mut bytes, len)?;
                // Packed enums can expand from one byte to four, with
                // geometric Vec growth and temporary copies. Nested-message
                // bytes deliberately overcount that maximum amplification.
                charge(allocation, len.saturating_mul(16))?;
                if let Some(child_shape) = shape.and_then(|s| s.child(field)) {
                    message(
                        child,
                        Some(child_shape),
                        depth.saturating_add(1),
                        allocation,
                    )?;
                }
            }
            3 => group(&mut bytes, field, depth.saturating_add(1), allocation)?,
            5 => {
                take(&mut bytes, 4)?;
            }
            _ => return Err(DecodeError::InvalidProtobuf),
        }
    }
    Ok(())
}

fn group(
    bytes: &mut &[u8],
    field: u32,
    depth: usize,
    allocation: &mut usize,
) -> Result<(), DecodeError> {
    if depth > MAX_DEPTH {
        return Err(DecodeError::Structure);
    }
    loop {
        let (next, wire) = tag(bytes)?;
        match wire {
            0 => {
                varint(bytes)?;
            }
            1 => {
                take(bytes, 8)?;
            }
            2 => {
                let len =
                    usize::try_from(varint(bytes)?).map_err(|_| DecodeError::InvalidProtobuf)?;
                take(bytes, len)?;
                charge(allocation, len.saturating_mul(16))?;
            }
            3 => group(bytes, next, depth.saturating_add(1), allocation)?,
            4 if next == field => return Ok(()),
            5 => {
                take(bytes, 4)?;
            }
            _ => return Err(DecodeError::InvalidProtobuf),
        }
    }
}

fn charge(allocation: &mut usize, bytes: usize) -> Result<(), DecodeError> {
    *allocation = allocation
        .checked_add(bytes)
        .ok_or(DecodeError::Structure)?;
    if *allocation > MAX_ALLOCATION {
        return Err(DecodeError::Structure);
    }
    Ok(())
}

fn tag(bytes: &mut &[u8]) -> Result<(u32, u8), DecodeError> {
    let tag = u32::try_from(varint(bytes)?).map_err(|_| DecodeError::InvalidProtobuf)?;
    let field = tag >> 3;
    if field == 0 || field > 0x1fff_ffff {
        return Err(DecodeError::InvalidProtobuf);
    }
    Ok((
        field,
        u8::try_from(tag & 7).map_err(|_| DecodeError::InvalidProtobuf)?,
    ))
}

fn varint(bytes: &mut &[u8]) -> Result<u64, DecodeError> {
    let mut result = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or(DecodeError::InvalidProtobuf)?;
        *bytes = rest;
        if shift == 63 && byte > 1 {
            return Err(DecodeError::InvalidProtobuf);
        }
        result |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(result);
        }
    }
    Err(DecodeError::InvalidProtobuf)
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], DecodeError> {
    let (head, tail) = bytes
        .split_at_checked(len)
        .ok_or(DecodeError::InvalidProtobuf)?;
    *bytes = tail;
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    #[allow(clippy::expect_used)]
    fn every_generated_message_edge_is_preflighted() {
        let descriptors = prost_types::FileDescriptorSet::decode(
            &include_bytes!(concat!(env!("OUT_DIR"), "/state-descriptors.bin"))[..],
        )
        .expect("generated descriptors");
        let schema = descriptors
            .file
            .iter()
            .find(|f| f.package.as_deref() == Some("pulsebeam.media.v1"))
            .expect("state schema");
        for shape in Shape::ALL {
            let message = schema
                .message_type
                .iter()
                .find(|m| m.name.as_deref() == Some(shape.name()))
                .expect("preflight shape exists in schema");
            for field in &message.field {
                let number = u32::try_from(field.number()).expect("positive field number");
                let child = shape.child(number).map(Shape::name);
                if field.r#type() == prost_types::field_descriptor_proto::Type::Message {
                    assert_eq!(
                        child,
                        field.type_name().rsplit('.').next(),
                        "{}.{}",
                        shape.name(),
                        field.name()
                    );
                } else {
                    assert_eq!(child, None, "scalar {}.{}", shape.name(), field.name());
                }
            }
        }
    }
}
