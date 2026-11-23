//! PRODUCTION WIRE CONTRACT — EDIT WITH EXTREME CARE. Changes can break deployed clients, mixed-version nodes, and production traffic. Require compatibility review and wire-vector tests.
//!
//! FabricPacket [RFC #275 §5.4 and Appendix B][rfc]: one DATA_V1 binary record
//! per SCTP message; the WAN projection moves optional DD out of the payload.
//! Big-endian, no padding or trailing bytes.
//! The 22-byte base is `50 44`, version `01`, flags, unit sequence (64 bits),
//! index, count and fragment length (16 bits each), and unit length (32 bits).
//! Flags `01` add signed capture nanoseconds (64 bits); `03` also adds unsigned
//! uncertainty (32 bits). Both refer to the installed clock and epoch, which do
//! not appear in the record. Flag `04` adds one length byte and 3–96 opaque
//! DD bytes after the clock fields. The fragment length excludes this field.
//! The maximum is 1,155 bytes with DD and 1,058 bytes without DD. The WAN
//! payload always uses the descriptor-free form.
//!
//! [rfc]: https://github.com/PulseBeamDev/pulsebeam/issues/275
//! Literal golden vectors are in `data_record/tests.rs`.

pub const BASE_BYTES: usize = 22;
pub const FRAGMENT_BYTES: usize = 1_024;
pub const MAX_FRAGMENTS: u16 = 64;
pub const MAX_UNIT_BYTES: usize = 65_536;
pub const MAX_RECORD_BYTES: usize = 1_058;
pub const MAX_DESCRIPTOR_BYTES: usize = 96;
pub const MAX_ENDPOINT_RECORD_BYTES: usize = 1_155;

const MAGIC: [u8; 2] = [0x50, 0x44];
const CAPTURE: u8 = 1;
const CAPTURE_UNCERTAINTY: u8 = 3;
const DESCRIPTOR: u8 = 4;
const CAPTURE_END: usize = 30;
const UNCERTAINTY_END: usize = 34;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock<'a> {
    pub id: &'a str,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capture<'a> {
    pub clock: Clock<'a>,
    pub nanos: i64,
    pub uncertainty_ns: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
    pub unit_seq: u64,
    pub index: u16,
    pub count: u16,
    pub unit_len: u32,
    pub capture: Option<Capture<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Length,
    Magic,
    Version,
    Flags,
    DescriptorLength,
    Clock,
    UnitLength,
    FragmentCount,
    FragmentIndex,
    FragmentLength,
}

/// An exact, validated fragment. The codec neither allocates nor stores a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record<'data, 'clock> {
    header: Header<'clock>,
    bytes: &'data [u8],
    descriptor: Option<&'data [u8]>,
}

impl<'data, 'clock> Record<'data, 'clock> {
    pub fn new(
        header: Header<'clock>,
        bytes: &'data [u8],
        clock: Option<Clock<'clock>>,
    ) -> Result<Self, Error> {
        if header
            .capture
            .is_some_and(|capture| Some(capture.clock) != clock)
        {
            return Err(Error::Clock);
        }
        check_fragment(header.unit_len, header.count, header.index, bytes.len())?;
        Ok(Self {
            header,
            bytes,
            descriptor: None,
        })
    }

    /// Decode a descriptor-free payload, as required on the WAN.
    pub fn decode_without_descriptor(
        record: &'data [u8],
        clock: Option<Clock<'clock>>,
    ) -> Result<Self, Error> {
        Self::from_parsed(parse(record, false)?, clock)
    }

    /// Decode a DATA v1 endpoint record with or without a descriptor.
    pub fn decode(record: &'data [u8], clock: Option<Clock<'clock>>) -> Result<Self, Error> {
        Self::from_parsed(parse(record, true)?, clock)
    }

    fn from_parsed(parsed: Parsed<'data>, clock: Option<Clock<'clock>>) -> Result<Self, Error> {
        let capture = parsed
            .capture
            .map(|(nanos, uncertainty_ns)| {
                Ok(Capture {
                    clock: clock.ok_or(Error::Clock)?,
                    nanos,
                    uncertainty_ns,
                })
            })
            .transpose()?;
        Ok(Self {
            header: Header {
                unit_seq: parsed.identity.unit_seq,
                index: parsed.identity.index,
                count: parsed.identity.count,
                unit_len: parsed.unit_len,
                capture,
            },
            bytes: parsed.bytes,
            descriptor: parsed.descriptor,
        })
    }

    pub const fn header(self) -> Header<'clock> {
        self.header
    }

    pub const fn bytes(self) -> &'data [u8] {
        self.bytes
    }

    pub const fn final_fragment(self) -> bool {
        self.header.index.saturating_add(1) == self.header.count
    }

    pub const fn dependency_descriptor(self) -> Option<&'data [u8]> {
        self.descriptor
    }

    /// Set a descriptor from SDK packetization or from the bound WAN extension.
    /// This validates the field envelope, not DD syntax or template context.
    pub fn with_dependency_descriptor(mut self, value: &'data [u8]) -> Result<Self, Error> {
        check_descriptor_len(value.len())?;
        self.descriptor = Some(value);
        Ok(self)
    }

    pub const fn without_dependency_descriptor(mut self) -> Self {
        self.descriptor = None;
        self
    }

    const fn format(self) -> (u8, usize) {
        match self.header.capture {
            None => (0, BASE_BYTES),
            Some(Capture {
                uncertainty_ns: None,
                ..
            }) => (CAPTURE, CAPTURE_END),
            Some(Capture {
                uncertainty_ns: Some(_),
                ..
            }) => (CAPTURE_UNCERTAINTY, UNCERTAINTY_END),
        }
    }

    pub fn encoded_len(self) -> usize {
        self.format()
            .1
            .saturating_add(
                self.descriptor
                    .map_or(0, |value| value.len().saturating_add(1)),
            )
            .saturating_add(self.bytes.len())
    }

    /// Write one record, leaving unused capacity in `output` unchanged.
    pub fn write(self, output: &mut [u8]) -> Result<usize, Error> {
        let (mut flags, capture_end) = self.format();
        let len = self.encoded_len();
        let offset = len.saturating_sub(self.bytes.len());
        if self.descriptor.is_some() {
            flags |= DESCRIPTOR;
        }
        let (head, bytes) = output
            .get_mut(..len)
            .and_then(|out| out.split_at_mut_checked(offset))
            .ok_or(Error::Length)?;
        let fragment_len = u16::try_from(self.bytes.len()).map_err(|_| Error::FragmentLength)?;
        put(head, 0, MAGIC)?;
        put(head, 2, [crate::data_endpoint::DATA_V1, flags])?;
        put(head, 4, self.header.unit_seq.to_be_bytes())?;
        put(head, 12, self.header.index.to_be_bytes())?;
        put(head, 14, self.header.count.to_be_bytes())?;
        put(head, 16, fragment_len.to_be_bytes())?;
        put(head, 18, self.header.unit_len.to_be_bytes())?;
        if let Some(capture) = self.header.capture {
            put(head, BASE_BYTES, capture.nanos.to_be_bytes())?;
            if let Some(uncertainty_ns) = capture.uncertainty_ns {
                put(head, CAPTURE_END, uncertainty_ns.to_be_bytes())?;
            }
        }
        if let Some(value) = self.descriptor {
            let field = head.get_mut(capture_end..).ok_or(Error::Length)?;
            let (length, descriptor) = field.split_first_mut().ok_or(Error::Length)?;
            *length = u8::try_from(value.len()).map_err(|_| Error::DescriptorLength)?;
            descriptor.copy_from_slice(value);
        }
        bytes.copy_from_slice(self.bytes);
        Ok(len)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragmentIdentity {
    pub unit_seq: u64,
    pub index: u16,
    pub count: u16,
}

impl FragmentIdentity {
    pub const fn final_fragment(self) -> bool {
        self.index.saturating_add(1) == self.count
    }
}

/// Validate the record's wire shape for opaque transit. A receiving endpoint
/// still has to decode it with the publication's bound capture clock.
pub fn wire_identity(record: &[u8]) -> Result<FragmentIdentity, Error> {
    Ok(parse(record, false)?.identity)
}

struct Parsed<'a> {
    identity: FragmentIdentity,
    unit_len: u32,
    capture: Option<(i64, Option<u32>)>,
    bytes: &'a [u8],
    descriptor: Option<&'a [u8]>,
}

fn parse(record: &[u8], allow_descriptor: bool) -> Result<Parsed<'_>, Error> {
    let limit = if allow_descriptor {
        MAX_ENDPOINT_RECORD_BYTES
    } else {
        MAX_RECORD_BYTES
    };
    if !(BASE_BYTES..=limit).contains(&record.len()) {
        return Err(Error::Length);
    }
    if read(record, 0)? != MAGIC {
        return Err(Error::Magic);
    }
    if read(record, 2)? != [crate::data_endpoint::DATA_V1] {
        return Err(Error::Version);
    }
    let [flags] = read(record, 3)?;
    let has_descriptor = allow_descriptor && flags & DESCRIPTOR != 0;
    let clock_flags = if has_descriptor {
        flags & !DESCRIPTOR
    } else {
        flags
    };
    let (offset, capture) = match clock_flags {
        0 => (BASE_BYTES, None),
        CAPTURE => (
            CAPTURE_END,
            Some((i64::from_be_bytes(read(record, BASE_BYTES)?), None)),
        ),
        CAPTURE_UNCERTAINTY => (
            UNCERTAINTY_END,
            Some((
                i64::from_be_bytes(read(record, BASE_BYTES)?),
                Some(u32::from_be_bytes(read(record, CAPTURE_END)?)),
            )),
        ),
        _ => return Err(Error::Flags),
    };
    let tail = record.get(offset..).ok_or(Error::Length)?;
    let (descriptor, bytes) = if has_descriptor {
        let (length, tail) = tail.split_first().ok_or(Error::Length)?;
        let length = usize::from(*length);
        check_descriptor_len(length)?;
        let (value, bytes) = tail.split_at_checked(length).ok_or(Error::Length)?;
        (Some(value), bytes)
    } else {
        (None, tail)
    };
    if bytes.len() != usize::from(u16::from_be_bytes(read(record, 16)?)) {
        return Err(Error::Length);
    }
    let identity = FragmentIdentity {
        unit_seq: u64::from_be_bytes(read(record, 4)?),
        index: u16::from_be_bytes(read(record, 12)?),
        count: u16::from_be_bytes(read(record, 14)?),
    };
    let unit_len = u32::from_be_bytes(read(record, 18)?);
    check_fragment(unit_len, identity.count, identity.index, bytes.len())?;
    Ok(Parsed {
        identity,
        unit_len,
        capture,
        bytes,
        descriptor,
    })
}

fn check_descriptor_len(length: usize) -> Result<(), Error> {
    (3..=MAX_DESCRIPTOR_BYTES)
        .contains(&length)
        .then_some(())
        .ok_or(Error::DescriptorLength)
}

fn check_fragment(unit_len: u32, count: u16, index: u16, length: usize) -> Result<(), Error> {
    let unit_len = usize::try_from(unit_len).map_err(|_| Error::UnitLength)?;
    if count != fragment_count(unit_len)? {
        return Err(Error::FragmentCount);
    }
    if index >= count {
        return Err(Error::FragmentIndex);
    }
    let offset = usize::from(index)
        .checked_mul(FRAGMENT_BYTES)
        .ok_or(Error::FragmentIndex)?;
    let expected = unit_len
        .checked_sub(offset)
        .ok_or(Error::FragmentIndex)?
        .min(FRAGMENT_BYTES);
    if length != expected {
        return Err(Error::FragmentLength);
    }
    Ok(())
}

pub fn fragment_count(unit_len: usize) -> Result<u16, Error> {
    if unit_len > MAX_UNIT_BYTES {
        return Err(Error::UnitLength);
    }
    u16::try_from(unit_len.div_ceil(FRAGMENT_BYTES).max(1)).map_err(|_| Error::FragmentCount)
}

fn read<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], Error> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk)
        .copied()
        .ok_or(Error::Length)
}

fn put<const N: usize>(bytes: &mut [u8], offset: usize, value: [u8; N]) -> Result<(), Error> {
    let target = bytes
        .get_mut(offset..)
        .and_then(<[u8]>::first_chunk_mut)
        .ok_or(Error::Length)?;
    *target = value;
    Ok(())
}

#[cfg(test)]
mod tests;
