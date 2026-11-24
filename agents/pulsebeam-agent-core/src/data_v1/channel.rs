use super::{
    AdmittedUnit, AssembledUnit, Error, LIFETIME, OutgoingUnit, Publisher, ReliablePublisher,
    Subscriber,
};
use crate::{
    ChannelId, DataChannelEffect, DataChannelEvent, DataChannelReliability, Generation,
    OperationId, PreparedDataChannel,
    ffi::{CaptureClock, DataSendOptions},
};
use alloc::{vec, vec::Vec};
use core::{num::NonZeroU64, time::Duration};
use pulsebeam_proto::{
    data_endpoint::{Binding, Delivery, Endpoint},
    data_record::MAX_RECORD_BYTES,
    state::{DataV1Binding, TrackBinding},
};

mod control;
pub use control::{PublisherControl, ReceiverControl, ReceiverReceipt};

/// The resource owner supplies both the physical binding and the logical host
/// callback ID. A reused physical stream must have a new callback ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channel {
    id: ChannelId,
    binding: Binding<Generation>,
}

impl Channel {
    /// The caller resolves the endpoint from the current authenticated resource
    /// offer. Its transport acknowledgment must match the reserved physical lease.
    pub fn from_accepted(
        id: ChannelId,
        binding: Binding<Generation>,
        prepared: &PreparedDataChannel,
        accepted: &TrackBinding,
    ) -> Result<Self, Error> {
        Self::from_confirmed(id, binding, Some(prepared), accepted)
    }

    pub(crate) fn from_confirmed(
        id: ChannelId,
        binding: Binding<Generation>,
        prepared: Option<&PreparedDataChannel>,
        accepted: &TrackBinding,
    ) -> Result<Self, Error> {
        let accepted = DataV1Binding::from_track(accepted).ok_or(Error::Binding)?;
        match (accepted, prepared) {
            (DataV1Binding::Sctp(_), Some(prepared))
                if prepared.validate().is_ok()
                    && prepared.negotiated
                    && !prepared.ordered
                    && prepared.reliability == DataChannelReliability::MaxRetransmits(0)
                    && prepared.stream_id == binding.channel.stream.get() => {}
            (DataV1Binding::Quic(_), None) => {}
            _ => return Err(Error::Binding),
        }
        if accepted.format() != Some(binding.format)
            || accepted.payload_channel(binding.channel.association) != Some(binding.channel)
        {
            return Err(Error::Binding);
        }
        Ok(Self { id, binding })
    }

    fn matches(self, generation: Generation, id: ChannelId) -> bool {
        self.binding.channel.association == generation && self.id == id
    }
}

enum PublisherMode {
    BestEffort(Publisher<Generation>),
    Reliable {
        publisher: Box<ReliablePublisher<Generation>>,
        control: ControlLease,
    },
}

enum TransmissionUnit {
    BestEffort(OutgoingUnit),
    Reliable(AdmittedUnit),
}

impl TransmissionUnit {
    fn count(&self) -> u16 {
        match self {
            Self::BestEffort(unit) => unit.count(),
            Self::Reliable(unit) => unit.fragment_count,
        }
    }
}

struct Transmission {
    unit: TransmissionUnit,
    index: u16,
    expires: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SendKind {
    Initial,
    RetryFirst,
    Original,
}

struct QueuedOriginal {
    expires: Duration,
    payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Ignored,
    Ready,
    Settled,
    Rejected,
    Closed,
}

/// One unsettled host send per publication channel. The Reliable publisher
/// retains originals and handles source admission independently of host sends.
pub struct SendChannel {
    channel: Channel,
    publisher: PublisherMode,
    transmission: Option<Transmission>,
    original: Option<QueuedOriginal>,
    pending: Option<(OperationId, SendKind)>,
    closed: bool,
    now: Duration,
}

impl SendChannel {
    pub fn new(channel: Channel, clock: Option<CaptureClock>, first: u64) -> Result<Self, Error> {
        let publisher = PublisherMode::BestEffort(Publisher::new(channel.binding, clock, first)?);
        Ok(Self::for_publisher(channel, publisher))
    }

    pub fn new_reliable(
        channel: Channel,
        clock: Option<CaptureClock>,
        first: u64,
        owned: NonZeroU64,
        signaling: ChannelId,
    ) -> Result<Self, Error> {
        if signaling == channel.id {
            return Err(Error::Binding);
        }
        let publisher = PublisherMode::Reliable {
            publisher: Box::new(ReliablePublisher::new(channel.binding, clock, first)?),
            control: ControlLease { owned, signaling },
        };
        Ok(Self::for_publisher(channel, publisher))
    }

    fn for_publisher(channel: Channel, publisher: PublisherMode) -> Self {
        Self {
            channel,
            publisher,
            transmission: None,
            original: None,
            pending: None,
            closed: false,
            now: Duration::ZERO,
        }
    }

    pub(crate) fn matches_binding(&self, binding: Binding<Generation>) -> bool {
        !self.closed && self.channel.binding == binding
    }

    pub(crate) fn reliable_source(&self) -> Option<NonZeroU64> {
        match (&self.publisher, self.channel.binding.endpoint) {
            (PublisherMode::Reliable { .. }, Endpoint::Publisher(endpoint)) => {
                Some(endpoint.source)
            }
            _ => None,
        }
    }

    pub fn admit(
        &mut self,
        bytes: Vec<u8>,
        options: DataSendOptions,
        now: Duration,
    ) -> Result<u64, Error> {
        self.expire(now);
        if self.closed {
            return Err(Error::Binding);
        }
        if self.transmission.is_some() || self.pending.is_some() {
            return Err(Error::Capacity);
        }
        let expires = self.now.checked_add(LIFETIME).ok_or(Error::Capacity)?;
        let (sequence, unit) = match &mut self.publisher {
            PublisherMode::BestEffort(publisher) => {
                let unit = publisher.admit(self.channel.binding, bytes, options)?;
                (unit.unit_seq(), TransmissionUnit::BestEffort(unit))
            }
            PublisherMode::Reliable { publisher, .. } => {
                let unit = publisher.admit(self.channel.binding, bytes, options, self.now)?;
                (unit.unit_seq, TransmissionUnit::Reliable(unit))
            }
        };
        self.transmission = Some(Transmission {
            unit,
            index: 0,
            expires,
        });
        Ok(sequence)
    }

    pub fn next_effect(
        &mut self,
        operation: OperationId,
        now: Duration,
    ) -> Result<Option<DataChannelEffect>, Error> {
        self.expire(now);
        if self.closed || self.pending.is_some() {
            return Ok(None);
        }
        let (payload, kind) = if let Some(transmission) = &self.transmission {
            let mut payload = vec![0; MAX_RECORD_BYTES];
            let size = match (&transmission.unit, &mut self.publisher) {
                (TransmissionUnit::BestEffort(unit), _) => {
                    unit.write(transmission.index, &mut payload)?
                }
                (TransmissionUnit::Reliable(unit), PublisherMode::Reliable { publisher, .. }) => {
                    publisher.write_initial(*unit, transmission.index, &mut payload, self.now)?
                }
                _ => return Err(Error::Binding),
            };
            payload.truncate(size);
            (payload, SendKind::Initial)
        } else if let Some(original) = self.original.take() {
            (original.payload, SendKind::Original)
        } else if let PublisherMode::Reliable { publisher, .. } = &mut self.publisher {
            let Some((_, payload)) = publisher.first_fragment_retry(self.now)? else {
                return Ok(None);
            };
            (payload, SendKind::RetryFirst)
        } else {
            return Ok(None);
        };
        self.pending = Some((operation, kind));
        Ok(Some(DataChannelEffect::Send {
            operation,
            generation: self.channel.binding.channel.association,
            channel: self.channel.id,
            binary: true,
            payload,
        }))
    }

    pub fn deadline(&self) -> Option<Duration> {
        let initial = self.transmission.as_ref().map(|unit| unit.expires);
        let retained = match &self.publisher {
            PublisherMode::BestEffort(_) => None,
            PublisherMode::Reliable { publisher, .. } => {
                if self.transmission.is_some() || self.pending.is_some() {
                    publisher.expiry()
                } else {
                    publisher.deadline()
                }
            }
        };
        initial.into_iter().chain(retained).min()
    }

    pub(crate) fn wake_deadline(&self, now: Duration) -> Option<Duration> {
        let deadline = self.deadline()?;
        if deadline <= now
            && let PublisherMode::Reliable { publisher, .. } = &self.publisher
        {
            // The effect drain sends a ready retry; do not keep arming a zero-delay timer.
            return publisher.expiry();
        }
        Some(deadline)
    }

    pub(crate) fn take_unconfirmed(&mut self, now: Duration, retiring: bool) -> Option<u64> {
        let PublisherMode::Reliable { publisher, .. } = &mut self.publisher else {
            return None;
        };
        publisher.expire_admission(now, retiring)
    }

    pub(crate) fn discard_unit(&mut self) {
        self.transmission = None;
        self.original = None;
        if let PublisherMode::Reliable { publisher, .. } = &mut self.publisher {
            publisher.discard();
        }
    }

    pub fn expire(&mut self, now: Duration) -> bool {
        self.now = self.now.max(now);
        if self
            .original
            .as_ref()
            .is_some_and(|original| self.now >= original.expires)
        {
            self.original = None;
        }
        if self
            .transmission
            .as_ref()
            .is_some_and(|unit| self.now >= unit.expires)
        {
            self.transmission = None;
            return true;
        }
        false
    }

    pub fn handle(&mut self, event: &DataChannelEvent, now: Duration) -> Progress {
        self.expire(now);
        let (operation, generation, channel, accepted) = match event {
            DataChannelEvent::Sent {
                operation,
                generation,
                channel,
            } => (*operation, *generation, *channel, true),
            DataChannelEvent::SendFailed {
                operation,
                generation,
                channel,
                ..
            } => (*operation, *generation, *channel, false),
            DataChannelEvent::Closed {
                generation,
                channel,
                ..
            } if self.channel.matches(*generation, *channel) => {
                self.closed = true;
                self.discard_unit();
                self.pending = None;
                return Progress::Closed;
            }
            _ => return Progress::Ignored,
        };
        let Some((expected, kind)) = self.pending else {
            return Progress::Ignored;
        };
        if !self.channel.matches(generation, channel) || expected != operation {
            return Progress::Ignored;
        }
        self.pending = None;
        if kind != SendKind::Initial {
            return if accepted {
                Progress::Ready
            } else {
                Progress::Rejected
            };
        }
        let Some(unit) = &mut self.transmission else {
            return Progress::Ignored;
        };
        if !accepted {
            self.transmission = None;
            return Progress::Rejected;
        }
        unit.index = unit.index.saturating_add(1);
        if unit.index == unit.unit.count() {
            self.transmission = None;
            return Progress::Settled;
        }
        Progress::Ready
    }
}

/// Dispatches authenticated binary callbacks only for the installed source and
/// branch. Malformed bytes have no alternate decoder on this lease.
pub struct ReceiveChannel {
    channel: Channel,
    subscriber: Subscriber<Generation>,
    control: Option<ControlLease>,
    closed: bool,
}

struct ControlLease {
    owned: NonZeroU64,
    signaling: ChannelId,
}

impl ReceiveChannel {
    pub fn new(channel: Channel, clock: Option<CaptureClock>) -> Result<Self, Error> {
        if channel.binding.format.delivery() == Delivery::Reliable {
            return Err(Error::Binding);
        }
        Self::for_binding(channel, clock, None)
    }

    /// The session supplies the current authenticated control channel and owned
    /// resource after a Reliable receiver is installed, never from the frame.
    pub fn new_reliable(
        channel: Channel,
        clock: Option<CaptureClock>,
        owned: NonZeroU64,
        signaling: ChannelId,
    ) -> Result<Self, Error> {
        if channel.binding.format.delivery() != Delivery::Reliable || signaling == channel.id {
            return Err(Error::Binding);
        }
        Self::for_binding(channel, clock, Some(ControlLease { owned, signaling }))
    }

    fn for_binding(
        channel: Channel,
        clock: Option<CaptureClock>,
        control: Option<ControlLease>,
    ) -> Result<Self, Error> {
        Ok(Self {
            channel,
            subscriber: Subscriber::new(channel.binding, clock)?,
            control,
            closed: false,
        })
    }

    pub fn handle(
        &mut self,
        event: &DataChannelEvent,
        now: Duration,
    ) -> Result<Option<AssembledUnit>, Error> {
        match event {
            DataChannelEvent::Message {
                generation,
                channel,
                binary: true,
                payload,
            } => self.offer_binary(*generation, *channel, payload, now),
            DataChannelEvent::Closed {
                generation,
                channel,
                ..
            } => {
                self.close(*generation, *channel);
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn offer_binary(
        &mut self,
        generation: Generation,
        channel: ChannelId,
        bytes: &[u8],
        now: Duration,
    ) -> Result<Option<AssembledUnit>, Error> {
        if self.closed || !self.channel.matches(generation, channel) {
            return Err(Error::Binding);
        }
        self.subscriber.offer(self.channel.binding, bytes, now)
    }

    pub fn deadline(&self) -> Option<Duration> {
        self.assembly_deadline()
            .into_iter()
            .chain(self.receipt_deadline())
            .min()
    }

    pub(crate) fn assembly_deadline(&self) -> Option<Duration> {
        (!self.closed)
            .then(|| self.subscriber.next_expiry())
            .flatten()
    }

    pub(crate) fn receipt_deadline(&self) -> Option<Duration> {
        (!self.closed)
            .then(|| self.subscriber.receipt_deadline())
            .flatten()
    }

    pub fn expire(&mut self, now: Duration) {
        self.subscriber.expire(now);
    }

    pub fn close(&mut self, generation: Generation, channel: ChannelId) -> bool {
        if !self.channel.matches(generation, channel) || self.closed {
            return false;
        }
        self.closed = true;
        self.subscriber.expire(Duration::MAX);
        self.subscriber.receipts.clear();
        true
    }
}

#[cfg(test)]
mod tests;
