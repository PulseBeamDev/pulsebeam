use std::time::{Duration, Instant};

use noq_proto::{Connection, Dir, ReadError, Side, StreamId, WriteError};

use crate::frame::{self, Decoder, Direction, Frame, Kind, Phase};

const PARTIAL_TIMEOUT: Duration = Duration::from_secs(5);
const READ_STEPS: usize = 3;

/// One ordered stream. The association owner decides admission and keeps the
/// application deadline while an admission or renewal is pending in control.
pub struct ControlStream {
    id: StreamId,
    side: Side,
    input: Decoder,
    partial_since: Option<Instant>,
    output: Option<Pending>,
}

struct Pending {
    encoded: Vec<u8>,
    offset: usize,
    kind: Kind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Frame(frame::Error),
    UnexpectedStream,
    StreamRead,
    StreamWrite,
    PartialTimeout,
    Backpressure,
}

pub enum Read {
    Pending,
    Frame(Frame),
    Finished,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Write {
    Empty,
    Pending,
    /// The entire frame entered Noq's bounded send window, not the socket or peer.
    Admitted {
        kind: Kind,
        at: Instant,
    },
}

impl ControlStream {
    /// Only call after validating peer identity, ALPN and the DATAGRAM budget.
    pub fn open(connection: &mut Connection) -> Result<Option<Self>, Error> {
        if connection.side() != Side::Client {
            return Err(Error::UnexpectedStream);
        }
        connection
            .streams()
            .open(Dir::Bi)
            .map(|id| Self::new(id, Side::Client))
            .transpose()
    }

    /// Only call after validating the transport profile and before redemption.
    pub fn accept(connection: &mut Connection) -> Result<Option<Self>, Error> {
        if connection.side() != Side::Server || connection.streams().accept(Dir::Uni).is_some() {
            return Err(Error::UnexpectedStream);
        }
        connection
            .streams()
            .accept(Dir::Bi)
            .map(|id| Self::new(id, Side::Server))
            .transpose()
    }

    fn new(id: StreamId, side: Side) -> Result<Self, Error> {
        if id.initiator() != Side::Client || id.index() != 0 {
            return Err(Error::UnexpectedStream);
        }
        Ok(Self {
            id,
            side,
            input: Decoder::default(),
            partial_since: None,
            output: None,
        })
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.partial_since
            .and_then(|at| at.checked_add(PARTIAL_TIMEOUT))
    }

    /// Read no more than one frame and leave subsequent bytes in the flow-
    /// controlled stream until the application owner is ready for them.
    pub fn read(
        &mut self,
        connection: &mut Connection,
        phase: Phase,
        now: Instant,
    ) -> Result<Read, Error> {
        if self.deadline().is_some_and(|at| at <= now) {
            return Err(Error::PartialTimeout);
        }
        if connection.streams().accept(Dir::Bi).is_some()
            || connection.streams().accept(Dir::Uni).is_some()
        {
            return Err(Error::UnexpectedStream);
        }
        let mut stream = connection.recv_stream(self.id);
        let mut chunks = stream.read(true).map_err(|_| Error::StreamRead)?;
        let direction = if self.side == Side::Client {
            Direction::Server
        } else {
            Direction::Client
        };
        let result = (|| {
            for _ in 0..READ_STEPS {
                let limit = self.input.read_limit().map_err(Error::Frame)?;
                match chunks.next(limit) {
                    Ok(Some(chunk)) => {
                        self.partial_since.get_or_insert(now);
                        let mut input = chunk.bytes.as_ref();
                        if let Some(frame) = self
                            .input
                            .push(&mut input, direction, phase)
                            .map_err(Error::Frame)?
                        {
                            self.partial_since = None;
                            return Ok(Read::Frame(frame));
                        }
                    }
                    Ok(None) => {
                        self.input.finish().map_err(Error::Frame)?;
                        return Ok(Read::Finished);
                    }
                    Err(ReadError::Blocked) => break,
                    Err(ReadError::Reset(_)) => return Err(Error::StreamRead),
                }
            }
            Ok(Read::Pending)
        })();
        let _ = chunks.finalize();
        result
    }

    pub fn offer(&mut self, kind: Kind, body: &[u8], phase: Phase) -> Result<(), Error> {
        if self.output.is_some() {
            return Err(Error::Backpressure);
        }
        let direction = if self.side == Side::Client {
            Direction::Client
        } else {
            Direction::Server
        };
        let encoded = frame::encode(kind, body, direction, phase).map_err(Error::Frame)?;
        self.output = Some(Pending {
            encoded,
            offset: 0,
            kind,
        });
        Ok(())
    }

    pub fn write(&mut self, connection: &mut Connection, now: Instant) -> Result<Write, Error> {
        let Some(pending) = self.output.as_mut() else {
            return Ok(Write::Empty);
        };
        let remaining = pending
            .encoded
            .get(pending.offset..)
            .ok_or(Error::StreamWrite)?;
        match connection.send_stream(self.id).write(remaining) {
            Ok(count) if count == remaining.len() => {
                let kind = pending.kind;
                self.output = None;
                Ok(Write::Admitted { kind, at: now })
            }
            Ok(count) => {
                pending.offset = pending.offset.saturating_add(count);
                Ok(Write::Pending)
            }
            Err(WriteError::Blocked) => Ok(Write::Pending),
            Err(_) => Err(Error::StreamWrite),
        }
    }
}
