use std::{mem, time::Instant};

use noq_proto::Connection;
use pulsebeam_core::fabric::Profile;

use crate::{
    data_bindings::FabricOffer,
    frame::{self, Kind, Phase},
    records::{Admit, Denial, decode_admit_result},
    transport::{
        Readiness,
        control::{self, ControlStream, Read, Write},
        data_readiness,
    },
};

#[derive(Debug, PartialEq, Eq)]
pub struct Admission {
    pub connection: [u8; 16],
    pub participant: String,
    pub room: Option<String>,
    pub capabilities: Vec<u8>,
    pub fabric: FabricOffer,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Expired,
    Transport(Readiness),
    Control(control::Error),
    Record(frame::Error),
    Denied(Denial),
    UnexpectedResult,
    Finished,
}

enum Stage {
    Handshake(Vec<u8>),
    Writing(ControlStream),
    Reading(ControlStream),
    Finished,
}

/// One admission attempt bound to one association. The deadline includes TLS and
/// the full application response; application I/O is unavailable before Accept.
pub struct ClientAdmission {
    stage: Stage,
    deadline: Instant,
    room: Option<String>,
    fabric: FabricOffer,
}

impl ClientAdmission {
    pub fn new(admit: &Admit<'_>, deadline: Instant) -> Result<Self, AdmissionError> {
        Ok(Self {
            stage: Stage::Handshake(admit.encode().map_err(AdmissionError::Record)?),
            deadline,
            room: admit.room.map(str::to_owned),
            fabric: admit.fabric,
        })
    }

    pub fn deadline(&self) -> Instant {
        match &self.stage {
            Stage::Reading(control) => control
                .deadline()
                .map_or(self.deadline, |at| at.min(self.deadline)),
            _ => self.deadline,
        }
    }

    pub fn poll(
        &mut self,
        connection: &mut Connection,
        now: Instant,
    ) -> Result<Option<(Admission, ControlStream)>, AdmissionError> {
        let mut stage = mem::replace(&mut self.stage, Stage::Finished);
        if now >= self.deadline {
            return Err(AdmissionError::Expired);
        }
        if connection.is_closed() {
            return Err(AdmissionError::Transport(Readiness::Closed));
        }
        if let Stage::Handshake(body) = &stage {
            match data_readiness(connection) {
                Readiness::Ready => {
                    if let Some(mut control) =
                        ControlStream::open(connection).map_err(AdmissionError::Control)?
                    {
                        control
                            .offer(Kind::Admit, body, Phase::Bootstrap)
                            .map_err(AdmissionError::Control)?;
                        stage = Stage::Writing(control);
                    }
                }
                Readiness::Handshaking => {}
                other => return Err(AdmissionError::Transport(other)),
            }
        }
        if let Stage::Writing(mut control) = stage {
            stage = match control
                .write(connection, now)
                .map_err(AdmissionError::Control)?
            {
                Write::Admitted {
                    kind: Kind::Admit, ..
                } => Stage::Reading(control),
                Write::Pending => Stage::Writing(control),
                _ => return Err(AdmissionError::UnexpectedResult),
            };
        }
        if let Stage::Reading(mut control) = stage {
            match control
                .read(connection, Phase::Bootstrap, now)
                .map_err(AdmissionError::Control)?
            {
                Read::Pending => stage = Stage::Reading(control),
                Read::Frame(frame) if frame.kind == Kind::AdmitResult => {
                    let accepted = decode_admit_result(&frame.body)
                        .map_err(AdmissionError::Record)?
                        .map_err(AdmissionError::Denied)?;
                    if accepted.room != self.room.as_deref()
                        || self.fabric.bind(accepted.fabric, Profile::Data, 0).is_err()
                    {
                        return Err(AdmissionError::UnexpectedResult);
                    }
                    return Ok(Some((
                        Admission {
                            connection: accepted.connection,
                            participant: accepted.participant.to_owned(),
                            room: accepted.room.map(str::to_owned),
                            capabilities: accepted.capabilities.to_vec(),
                            fabric: accepted.fabric,
                        },
                        control,
                    )));
                }
                _ => return Err(AdmissionError::UnexpectedResult),
            }
        }
        if matches!(stage, Stage::Finished) {
            return Err(AdmissionError::Finished);
        }
        self.stage = stage;
        Ok(None)
    }
}
