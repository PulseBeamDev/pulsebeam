use std::{collections::VecDeque, num::NonZeroU64};

use pulsebeam_link::BindError;
use pulsebeam_runtime::sync::{CancellationToken, oneshot, watch};

use super::*;
use crate::route::fabric::network_repair::MAX_REPAIR_LIFETIME;

const CACHE_PACKETS: usize = 64;

#[derive(Clone, Copy, Default)]
enum Status {
    #[default]
    Staging,
    Active {
        last_media_at: Option<Instant>,
    },
}

#[derive(Clone)]
pub(crate) struct Fence {
    peer: NetworkId,
    pub(super) window: Window,
    pub(super) authority: NonZeroU64,
    deadline: wan::LiveDeadline,
    revoked: CancellationToken,
}

impl Fence {
    pub(crate) fn peer(&self) -> NetworkId {
        self.peer
    }

    pub(super) fn is_live(&self) -> bool {
        !self.revoked.is_cancelled()
    }

    pub(super) fn expires(&self) -> std::time::Instant {
        self.deadline.expires()
    }

    pub(super) fn matches(&self, other: &Self) -> bool {
        self.peer == other.peer && self.window == other.window && self.authority == other.authority
    }
}

/// Receive-only observation of an exact, controller-authorized window. This
/// handle owns the observation; dropping it revokes the socket-side window.
/// An install receipt is not proof of a media packet, or of a decodable frame.
pub(crate) struct WarmReceive {
    fence: Fence,
    status: watch::Receiver<Status>,
}

impl WarmReceive {
    pub(crate) fn peer(&self) -> NetworkId {
        self.fence.peer
    }

    pub(crate) fn fence(&self) -> Fence {
        self.fence.clone()
    }

    fn status(&self, now: Instant) -> Option<Status> {
        if !self.fence.is_live()
            || now.into_std() >= self.fence.expires()
            || self.status.has_changed().is_err()
        {
            return None;
        }
        Some(*self.status.borrow())
    }

    pub(crate) fn available(&self, now: Instant) -> bool {
        self.status(now).is_some()
    }

    pub(crate) fn installed(&self, now: Instant) -> Option<Window> {
        matches!(self.status(now), Some(Status::Active { .. })).then_some(self.fence.window)
    }

    pub(crate) fn last_media_at(&self, now: Instant) -> Option<Instant> {
        match self.status(now)? {
            Status::Active { last_media_at } => last_media_at,
            Status::Staging => None,
        }
    }

    pub(crate) fn renew(&self, now: Instant, expires: Instant) -> bool {
        self.installed(now).is_some()
            && self
                .fence
                .deadline
                .renew(now.into_std(), expires.into_std())
    }
}

impl Drop for WarmReceive {
    fn drop(&mut self) {
        self.fence.revoked.cancel();
    }
}

pub(crate) struct Install {
    fence: Fence,
    status: watch::Sender<Status>,
}

pub(crate) struct Forward {
    fence: Fence,
    output: WanTx,
}

pub(crate) struct Lookup {
    fence: Fence,
    position: u64,
    reply: oneshot::Sender<Option<CachedPacket>>,
}

#[derive(Clone)]
pub(crate) struct CachedPacket {
    fence: Fence,
    pub(crate) position: u64,
    pub(crate) packet: OwnedPacket,
    pub(crate) expires: Instant,
}

impl CachedPacket {
    pub(crate) fn is_live(&self, now: Instant) -> bool {
        self.fence.is_live() && now < self.expires && now.into_std() < self.fence.expires()
    }
}

pub(super) struct Active {
    pub(super) window: Staged<Window>,
    fence: Fence,
    status: watch::Sender<Status>,
    output: Option<WanTx>,
    cache: VecDeque<CachedPacket>,
}

impl Active {
    pub(super) fn expires(&self) -> std::time::Instant {
        self.fence.expires()
    }

    pub(super) fn live(&self, now: Instant) -> bool {
        self.fence.is_live() && now.into_std() < self.expires()
    }

    pub(super) fn deliver(
        &mut self,
        now: Instant,
        image: wan::Received<(OwnedPacket, Option<OmissionRun>), Window>,
    ) {
        if self.live(now) {
            self.status.send_replace(Status::Active {
                last_media_at: Some(now),
            });
            let (packet, omitted) = image.packet;
            tracing::trace!(target: "pulsebeam_protection", position = image.position, timestamp = packet.header().timestamp, ?omitted, "protected relay authenticated");
            if let Some(output) = &self.output
                && packet.remaining_hops() != 0
            {
                if let Some(expires) = now.checked_add(MAX_REPAIR_LIFETIME) {
                    while self
                        .cache
                        .front()
                        .is_some_and(|packet| now >= packet.expires)
                    {
                        self.cache.pop_front();
                    }
                    if self.cache.len() == CACHE_PACKETS {
                        self.cache.pop_front();
                    }
                    self.cache.push_back(CachedPacket {
                        fence: self.fence.clone(),
                        position: image.position,
                        packet: packet.clone(),
                        expires,
                    });
                }
                let expiry = Instant::from_std(image.expires);
                let deadline = now
                    .checked_add(crate::route::fabric::local::MAX_AGE)
                    .map_or(expiry, |fresh| fresh.min(expiry));
                let omitted = omitted
                    .filter(|&run| output.accommodates_omission(image.position, &packet, run));
                output.try_send_projected(now, deadline, image.position, packet, omitted);
            }
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.fence.revoked.cancel();
    }
}

impl Commands {
    pub(crate) fn lookup_warm(
        &self,
        fence: Fence,
        position: u64,
    ) -> Option<oneshot::Receiver<Option<CachedPacket>>> {
        if self.peer() != fence.peer() || !fence.window.contains(position) || !fence.is_live() {
            return None;
        }
        let (reply, result) = oneshot::channel();
        self.try_send(Command::LookupWarm(Lookup {
            fence,
            position,
            reply,
        }))
        .ok()?;
        Some(result)
    }

    /// The caller must validate the signed source permit and exact path before
    /// staging. The host binds the result to its own current association lease.
    pub(crate) fn stage_warm_receive(
        &self,
        window: Window,
        authority: NonZeroU64,
        expires: Instant,
    ) -> Result<WarmReceive, BindError> {
        let fence = Fence {
            peer: self.peer(),
            window,
            authority,
            deadline: wan::LiveDeadline::new(expires.into_std()),
            revoked: CancellationToken::new(),
        };
        let (status, updates) = watch::channel(Status::Staging);
        self.try_send(Command::Warm(Install {
            fence: fence.clone(),
            status,
        }))?;
        Ok(WarmReceive {
            fence,
            status: updates,
        })
    }

    pub(crate) fn forward_warm(&self, input: &WarmReceive, output: WanTx) -> Result<(), BindError> {
        if input.peer() != self.peer() || !output.has_warm_parent(&input.fence) {
            return Err(BindError::Closed);
        }
        self.try_send(Command::ForwardWarm(Box::new(Forward {
            fence: input.fence(),
            output,
        })))
    }
}

impl WanHost {
    pub(super) fn install_warm(&mut self, now: Instant, install: Install) -> Result<(), Error> {
        let Install { fence, status } = install;
        let window = fence.window;
        let ssrc = window.ssrc();
        if !fence.is_live()
            || fence.peer != self.id
            || !window.profile().is_video()
            || self.pending.contains_key(&ssrc)
            || self.receives.contains_key(&ssrc)
            || self.repairs.contains_key(&ssrc)
            || self.warm.contains_key(&ssrc)
        {
            return Err(Error::Binding);
        }
        let spec = WindowSpec {
            authority: fence.authority,
            direction: Direction::Receive,
            start: window.start(),
            ssrc,
            purpose: Purpose::Live,
            expires: fence.expires(),
        };
        let ticket = self.driver.stage_with_deadline(
            now.into_std(),
            self.binding(),
            spec,
            window,
            Some(fence.deadline.clone()),
        )?;
        if let Err(error) = self.driver.activate(now.into_std(), ticket, window, spec) {
            self.driver.retire(ticket);
            return Err(error.into());
        }
        status.send_replace(Status::Active {
            last_media_at: None,
        });
        self.warm.insert(
            ssrc,
            Active {
                window: ticket,
                fence,
                status,
                output: None,
                cache: VecDeque::new(),
            },
        );
        Ok(())
    }

    pub(super) fn lookup_warm(&self, now: Instant, lookup: Lookup) {
        let packet = self
            .warm
            .get(&lookup.fence.window.ssrc())
            .filter(|active| active.live(now) && active.fence.matches(&lookup.fence))
            .and_then(|active| {
                active
                    .cache
                    .iter()
                    .find(|packet| packet.position == lookup.position && packet.is_live(now))
            })
            .cloned();
        let _ = lookup.reply.send(packet);
    }

    pub(super) fn forward_warm(&mut self, now: Instant, command: Forward) {
        let Forward { fence, output } = command;
        if let Some(active) = self.warm.get_mut(&fence.window.ssrc())
            && active.live(now)
            && active.fence.matches(&fence)
            && output.has_warm_parent(&active.fence)
        {
            active.output = Some(output);
        }
    }
}
