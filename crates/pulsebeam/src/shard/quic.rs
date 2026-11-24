use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    num::NonZeroU64,
    pin::Pin,
    task::{Context, Poll},
};

use pulsebeam_agent_quic::{
    frame::{Kind, Phase},
    records::{Denial, Lease, Renew, RenewResult, encode_admit_result},
    transport::{
        close,
        control::{Read, Write},
        ingress::Ingress,
        listener::{Association, ConnectionHandle},
    },
};
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use super::worker::{RenewalToken, ShardEvent};
use crate::{
    control::{
        api::creation::Delivery,
        controller::{AdmissionCommand, ControllerCommand, quic_redeem},
        native::handoff::{EndpointHandoff, Opened},
    },
    entity::{ConnectionId, ParticipantId},
    id::ShardId,
    media::ChannelId,
    participant::signaling::{SignalingOutput, SignalingProgress},
    route::NodeTransportAddress,
};
use pulsebeam_queue as mailbox;

const CONTROL_CHANNEL: ChannelId = ChannelId::from_value(0);

pub(super) struct QuicShard {
    ingress: Ingress,
    controller: mailbox::Sender<ControllerCommand>,
    sessions: HashMap<ConnectionHandle, Session>,
    next: u64,
}

struct Session {
    serial: NonZeroU64,
    reply: Option<oneshot::Receiver<Result<Delivery, Denial>>>,
    response: Option<Result<Delivery, Denial>>,
    installed: Option<Installed>,
    admitted: bool,
    denying: bool,
}

pub(super) struct Installed {
    pub connection: ConnectionId,
    pub participant: ParticipantId,
    pub transport: NodeTransportAddress,
    pub handoff: Box<EndpointHandoff>,
    output: Option<SignalingOutput>,
    renewal: Option<Renewal>,
    last_renewal: u64,
}

struct Renewal {
    request: NonZeroU64,
    result: Option<RenewResult>,
    staged: bool,
}

impl Installed {
    pub fn new(
        connection: ConnectionId,
        participant: ParticipantId,
        transport: NodeTransportAddress,
        handoff: Box<EndpointHandoff>,
    ) -> Self {
        Self {
            connection,
            participant,
            transport,
            handoff,
            output: None,
            renewal: None,
            last_renewal: 0,
        }
    }
}

impl QuicShard {
    pub fn new(ingress: Ingress, controller: mailbox::Sender<ControllerCommand>) -> Self {
        Self {
            ingress,
            controller,
            sessions: HashMap::new(),
            next: 0,
        }
    }

    pub fn install(&mut self, serial: NonZeroU64, installed: Installed) -> bool {
        let Some((&handle, session)) = self
            .sessions
            .iter_mut()
            .find(|(_, session)| session.serial == serial)
        else {
            return false;
        };
        if session.installed.is_some()
            || session.reply.is_none()
            || self
                .ingress
                .get(handle)
                .is_none_or(|peer| peer.connection.is_closed())
        {
            return false;
        }
        session.installed = Some(installed);
        true
    }

    pub fn retire(
        &mut self,
        connection: ConnectionId,
        transport: NodeTransportAddress,
        now: Instant,
    ) {
        let handle = self.sessions.iter().find_map(|(&handle, session)| {
            session
                .installed
                .as_ref()
                .filter(|installed| {
                    installed.connection == connection && installed.transport == transport
                })
                .map(|_| handle)
        });
        if let Some(handle) = handle {
            self.sessions.remove(&handle);
            if let Some(peer) = self.ingress.get(handle) {
                close(&mut peer.connection, now.into_std());
            }
            self.ingress.schedule();
        }
    }

    pub fn authorization_result(
        &mut self,
        connection: ConnectionId,
        revision: u64,
        expiry: Option<i64>,
    ) -> bool {
        let Some(installed) = self
            .sessions
            .values_mut()
            .filter_map(|s| s.installed.as_mut())
            .find(|i| i.connection == connection)
        else {
            return false;
        };
        if let Some(renewal) = &mut installed.renewal
            && !renewal.staged
        {
            let accepted = NonZeroU64::new(revision)
                .zip(
                    expiry
                        .and_then(|expiry| u64::try_from(expiry).ok())
                        .and_then(NonZeroU64::new),
                )
                .map(|(revision, expires_unix_seconds)| Lease {
                    revision,
                    expires_unix_seconds,
                });
            renewal.result = Some(RenewResult {
                request: renewal.request,
                accepted,
            });
            self.ingress.schedule();
        }
        true
    }

    pub fn wake(&mut self) {
        self.ingress.schedule();
    }

    pub fn deadline(&self) -> Option<Instant> {
        (!self.ingress.wants_write())
            .then(|| self.ingress.deadline().map(Instant::from_std))
            .flatten()
    }

    pub fn wants_write(&self) -> bool {
        self.ingress.wants_write()
    }

    pub async fn ready(&mut self) {
        std::future::poll_fn(|cx| {
            if Self::poll_replies(&mut self.sessions, cx).is_ready() {
                self.ingress.schedule();
            }
            if self.ingress.runnable() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    fn poll_replies(
        sessions: &mut HashMap<ConnectionHandle, Session>,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        let mut ready = false;
        for session in sessions.values_mut() {
            if let Some(reply) = session.reply.as_mut()
                && let Poll::Ready(result) = Pin::new(reply).poll(cx)
            {
                session.reply = None;
                session.response = Some(result.unwrap_or(Err(Denial::AttemptUnavailable)));
                ready = true;
            }
        }
        if ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    pub fn turn(
        &mut self,
        shard: ShardId,
        socket: &mut pulsebeam_udp::UdpSocket,
        now: Instant,
        events: &mut VecDeque<ShardEvent>,
    ) -> std::io::Result<()> {
        let now_std = now.into_std();
        self.ingress.turn(socket, now_std, |handle, peer| {
            if peer.connection.is_closed() {
                Self::closed(&mut self.sessions, handle, events);
                return;
            }
            if let Some(session) = self.sessions.get_mut(&handle) {
                if !session.poll(peer, now, events) {
                    close(&mut peer.connection, now_std);
                    Self::closed(&mut self.sessions, handle, events);
                }
                return;
            }
            let body = match peer.take_admit(now_std) {
                Ok(Some(body)) => body,
                Ok(None) => return,
                Err(_) => {
                    close(&mut peer.connection, now_std);
                    return;
                }
            };
            let Some(serial) = self.next.checked_add(1).and_then(NonZeroU64::new) else {
                close(&mut peer.connection, now_std);
                return;
            };
            self.next = serial.get();
            let (reply, receive) = oneshot::channel();
            let command = AdmissionCommand::redeem_quic(quic_redeem::Command {
                shard,
                association: serial,
                body,
                reply,
            });
            if self.controller.try_send(command.into()).is_err() {
                close(&mut peer.connection, now_std);
                return;
            }
            self.sessions.insert(
                handle,
                Session {
                    serial,
                    reply: Some(receive),
                    response: None,
                    installed: None,
                    admitted: false,
                    denying: false,
                },
            );
        })
    }

    fn closed(
        sessions: &mut HashMap<ConnectionHandle, Session>,
        handle: ConnectionHandle,
        events: &mut VecDeque<ShardEvent>,
    ) {
        if let Some(installed) = sessions
            .remove(&handle)
            .and_then(|session| session.installed)
        {
            events.push_back(ShardEvent::ParticipantClosed {
                participant: installed.participant,
                connection_id: installed.connection,
            });
        }
    }
}

impl Session {
    fn poll(
        &mut self,
        peer: &mut Association,
        now: Instant,
        events: &mut VecDeque<ShardEvent>,
    ) -> bool {
        let Some(control) = peer.control.as_mut() else {
            return false;
        };
        if let Some(result) = self.response.take() {
            match result {
                Ok(reply) if self.installed.is_some() => {
                    if control
                        .offer(Kind::AdmitResult, reply.as_ref(), Phase::Bootstrap)
                        .is_err()
                    {
                        return false;
                    }
                }
                Ok(_) => return false,
                Err(denial) => {
                    let Ok(bytes) = encode_admit_result(Err(denial)) else {
                        return false;
                    };
                    if control
                        .offer(Kind::AdmitResult, &bytes, Phase::Bootstrap)
                        .is_err()
                    {
                        return false;
                    }
                    self.denying = true;
                }
            }
        }
        if self.admitted {
            let Some(installed) = self.installed.as_mut() else {
                return false;
            };
            if installed.handoff.failed() {
                return false;
            }
            match control.read(&mut peer.connection, Phase::Admitted, now.into_std()) {
                Ok(Read::Pending) => {}
                Ok(Read::Frame(frame)) if frame.kind == Kind::ClientState => {
                    if !installed.handoff.input(frame.body) {
                        return false;
                    }
                }
                Ok(Read::Frame(frame)) if frame.kind == Kind::Renew => {
                    let Ok(renew) = Renew::decode(&frame.body) else {
                        return false;
                    };
                    if installed.renewal.is_some() || installed.last_renewal >= renew.revision.get()
                    {
                        return false;
                    }
                    installed.last_renewal = renew.revision.get();
                    installed.renewal = Some(Renewal {
                        request: renew.revision,
                        result: None,
                        staged: false,
                    });
                    events.push_back(ShardEvent::RenewAuthorization {
                        participant: installed.participant,
                        connection_id: installed.connection,
                        token: RenewalToken(renew.token.to_owned()),
                    });
                }
                Ok(Read::Frame(frame)) if frame.kind == Kind::Close => return false,
                _ => return false,
            }
            let mut renewal_staged = installed
                .renewal
                .as_ref()
                .is_some_and(|renewal| renewal.staged);
            if installed.output.is_none()
                && let Some(renewal) = installed.renewal.as_mut()
                && let Some(result) = renewal.result.take()
            {
                if control
                    .offer(Kind::RenewResult, &result.encode(), Phase::Admitted)
                    .is_err()
                {
                    return false;
                }
                renewal.staged = true;
                renewal_staged = true;
            }
            if !renewal_staged
                && installed.output.is_none()
                && let Some(output) = installed.handoff.output()
            {
                if control
                    .offer(Kind::ServerState, &output.bytes, Phase::Admitted)
                    .is_err()
                {
                    return false;
                }
                installed.output = Some(output);
            }
            if installed
                .output
                .as_ref()
                .is_some_and(|output| now >= output.deadline)
            {
                return false;
            }
        }
        match control.write(&mut peer.connection, now.into_std()) {
            Ok(Write::Admitted {
                kind: Kind::AdmitResult,
                ..
            }) if !self.denying => {
                if !peer.finish_admission() {
                    return false;
                }
                self.admitted = true;
                self.installed.as_mut().is_some_and(|installed| {
                    installed.handoff.opened(Opened {
                        channel: CONTROL_CHANNEL,
                        at: now,
                    })
                })
            }
            Ok(Write::Admitted {
                kind: Kind::ServerState,
                ..
            }) => {
                let Some(installed) = self.installed.as_mut() else {
                    return false;
                };
                installed.output.take().is_some_and(|output| {
                    installed.handoff.progress(SignalingProgress {
                        sequence: output.sequence,
                        cid: output.cid,
                        at: now,
                    })
                })
            }
            Ok(Write::Admitted {
                kind: Kind::RenewResult,
                ..
            }) => self
                .installed
                .as_mut()
                .is_some_and(|installed| installed.renewal.take().is_some()),
            // Let Noq serialize a denial before the admission timeout.
            Ok(
                Write::Pending
                | Write::Empty
                | Write::Admitted {
                    kind: Kind::AdmitResult,
                    ..
                },
            ) => true,
            _ => false,
        }
    }
}
