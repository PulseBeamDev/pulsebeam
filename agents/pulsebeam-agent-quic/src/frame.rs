//! Ordered application framing above the QUIC stream.

pub const MAX_ADMIT_BODY: usize = 9_274;
pub const MAX_ADMIT_RESULT_BODY: usize = 4_197;
pub const MAX_TOKEN: usize = 8_192;
const MAX_LEN: usize = MAX_ADMIT_BODY + 1;
const MAX_STATE: usize = pulsebeam_proto::state::codec::MAX_FRAME_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Client,
    Server,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Bootstrap,
    Admitted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Admit = 1,
    AdmitResult = 2,
    ClientState = 3,
    ServerState = 4,
    Renew = 5,
    RenewResult = 6,
    Close = 7,
    DataControl = 8,
}

impl Kind {
    fn from_byte(byte: u8) -> Result<Self, Error> {
        Ok(match byte {
            1 => Self::Admit,
            2 => Self::AdmitResult,
            3 => Self::ClientState,
            4 => Self::ServerState,
            5 => Self::Renew,
            6 => Self::RenewResult,
            7 => Self::Close,
            8 => Self::DataControl,
            _ => return Err(Error::Kind),
        })
    }

    fn validate(self, body_len: usize, direction: Direction, phase: Phase) -> Result<(), Error> {
        use Direction::{Client, Server};
        use Kind::*;
        use Phase::{Admitted, Bootstrap};
        let limits = match (self, direction, phase) {
            (Admit, Client, Bootstrap) => 24..=MAX_ADMIT_BODY,
            (AdmitResult, Server, Bootstrap) => 1..=MAX_ADMIT_RESULT_BODY,
            (ClientState, Client, Admitted) | (ServerState, Server, Admitted) => 1..=MAX_STATE,
            (Renew, Client, Admitted) => 11..=MAX_TOKEN.saturating_add(10),
            (RenewResult, Server, Admitted) => 25..=25,
            (Close, _, Admitted) => 0..=0,
            (DataControl, _, Admitted) => 1..=pulsebeam_proto::state::codec::MAX_DATA_CONTROL_FRAME,
            _ => return Err(Error::PhaseOrDirection),
        };
        limits
            .contains(&body_len)
            .then_some(())
            .ok_or(Error::Length)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Length,
    Kind,
    PhaseOrDirection,
    Truncated,
    Body,
    DecoderFailed,
}

// Do not derive Debug: bootstrap and renewal frames carry credentials.
pub struct Frame {
    pub kind: Kind,
    pub body: Vec<u8>,
}

pub fn encode(
    kind: Kind,
    body: &[u8],
    direction: Direction,
    phase: Phase,
) -> Result<Vec<u8>, Error> {
    kind.validate(body.len(), direction, phase)?;
    let length = u32::try_from(body.len().saturating_add(1)).map_err(|_| Error::Length)?;
    let mut bytes = Vec::with_capacity(body.len().saturating_add(5));
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.push(kind as u8);
    bytes.extend_from_slice(body);
    Ok(bytes)
}

#[derive(Default)]
pub struct Decoder {
    prefix: [u8; 5],
    prefix_read: usize,
    body: Vec<u8>,
    failed: bool,
}

impl Decoder {
    /// Byte count to the next parsing boundary, so stream adapters need not
    /// hold bytes from a subsequent frame while the owner changes phase.
    pub(crate) fn read_limit(&self) -> Result<usize, Error> {
        if self.failed {
            return Err(Error::DecoderFailed);
        }
        if self.prefix_read < 5 {
            let boundary = if self.prefix_read < 4 { 4usize } else { 5 };
            return Ok(boundary.saturating_sub(self.prefix_read));
        }
        let [a, b, c, d, _] = self.prefix;
        let length =
            usize::try_from(u32::from_be_bytes([a, b, c, d])).map_err(|_| Error::Length)?;
        Ok(length.saturating_sub(1).saturating_sub(self.body.len()))
    }

    /// Consume at most one frame. Pass the current application phase each time
    /// so it can be updated before parsing the next frame from the same read.
    pub fn push(
        &mut self,
        input: &mut &[u8],
        direction: Direction,
        phase: Phase,
    ) -> Result<Option<Frame>, Error> {
        if self.failed {
            return Err(Error::DecoderFailed);
        }
        let result = self.consume(input, direction, phase);
        if result.is_err() {
            self.failed = true;
            self.body.clear();
        }
        result
    }

    fn consume(
        &mut self,
        input: &mut &[u8],
        direction: Direction,
        phase: Phase,
    ) -> Result<Option<Frame>, Error> {
        self.fill_prefix(input, 4);
        if self.prefix_read < 4 {
            return Ok(None);
        }
        let [a, b, c, d, _] = self.prefix;
        let length =
            usize::try_from(u32::from_be_bytes([a, b, c, d])).map_err(|_| Error::Length)?;
        if !(1..=MAX_LEN).contains(&length) {
            return Err(Error::Length);
        }
        self.fill_prefix(input, 5);
        if self.prefix_read < 5 {
            return Ok(None);
        }
        let [_, _, _, _, tag] = self.prefix;
        let kind = Kind::from_byte(tag)?;
        let length = length.saturating_sub(1);
        kind.validate(length, direction, phase)?;
        if self.body.capacity() < length {
            self.body
                .reserve_exact(length.saturating_sub(self.body.len()));
        }
        let count = length.saturating_sub(self.body.len()).min(input.len());
        let (bytes, rest) = input.split_at(count);
        self.body.extend_from_slice(bytes);
        *input = rest;
        if self.body.len() != length {
            return Ok(None);
        }
        self.prefix_read = 0;
        Ok(Some(Frame {
            kind,
            body: std::mem::take(&mut self.body),
        }))
    }

    fn fill_prefix(&mut self, input: &mut &[u8], end: usize) {
        for byte in self.prefix.iter_mut().take(end).skip(self.prefix_read) {
            let Some((first, rest)) = input.split_first() else {
                break;
            };
            *byte = *first;
            *input = rest;
            self.prefix_read = self.prefix_read.saturating_add(1);
        }
    }

    pub fn finish(&self) -> Result<(), Error> {
        if self.failed {
            return Err(Error::DecoderFailed);
        }
        if self.prefix_read != 0 {
            return Err(Error::Truncated);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_coalesced_frames_preserve_boundaries() {
        let encoded = encode(
            Kind::ServerState,
            &[0x08, 0x01],
            Direction::Server,
            Phase::Admitted,
        )
        .unwrap();
        assert_eq!(encoded, [0, 0, 0, 3, 4, 8, 1]);
        let mut decoder = Decoder::default();
        for mut byte in encoded.get(..6).unwrap().chunks(1) {
            assert!(
                decoder
                    .push(&mut byte, Direction::Server, Phase::Admitted)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(decoder.finish(), Err(Error::Truncated));
        let mut tail = [1, 0, 0, 0, 1, 7].as_slice();
        let first = decoder
            .push(&mut tail, Direction::Server, Phase::Admitted)
            .unwrap()
            .unwrap();
        assert_eq!(first.kind, Kind::ServerState);
        assert_eq!(first.body, [8, 1]);
        assert_eq!(
            decoder
                .push(&mut tail, Direction::Server, Phase::Admitted)
                .unwrap()
                .unwrap()
                .kind,
            Kind::Close
        );
        assert!(tail.is_empty());
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn rejects_before_consuming_body_and_latches_failure() {
        for (prefix, error, consumed) in [
            ([0xff, 0xff, 0xff, 0xff, 3, 99], Error::Length, 4),
            ([0, 0, 0, 2, 0xff, 99], Error::Kind, 5),
            ([0, 0, 0, 2, 4, 99], Error::PhaseOrDirection, 5),
            ([0, 0, 0, 2, 7, 99], Error::Length, 5),
        ] {
            let mut input = prefix.as_slice();
            let mut decoder = Decoder::default();
            assert_eq!(
                decoder
                    .push(&mut input, Direction::Client, Phase::Admitted)
                    .err(),
                Some(error)
            );
            assert_eq!(Some(input), prefix.get(consumed..));
            assert_eq!(
                decoder
                    .push(&mut input, Direction::Client, Phase::Admitted)
                    .err(),
                Some(Error::DecoderFailed)
            );
        }
    }
}
