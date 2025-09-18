use crate::actor::{self, ActorHandle, MessageSet};
use crate::mailbox;
use std::fmt::Debug;

/// A message enum to distinguish between high and low priority messages
/// received by the mock actor.
#[derive(Debug, Clone)]
pub enum MockMessage<H, L> {
    High(H),
    Low(L),
}

/// A container for the "receiving" side of a mock actor handle.
/// This is an internal detail, hidden by the `MockActor` struct.
struct MockReceivers<M: MessageSet> {
    high: mailbox::Receiver<M::HighPriorityMsg>,
    low: mailbox::Receiver<M::LowPriorityMsg>,
    // The system channel is kept in case you need to test system messages,
    // but it's not exposed in the ergonomic helpers for now.
    _sys: mailbox::Receiver<actor::SystemMsg<M::ObservableState>>,
}

/// A test double for any actor that implements `MessageSet`.
///
/// This struct is the main entry point for tests. It provides a simple, fluent
/// API for injecting a mock handle and asserting on the messages it receives.
pub struct FakeActor<A: MessageSet> {
    handle: ActorHandle<A>,
    receivers: MockReceivers<A>,
}

impl<A: MessageSet> FakeActor<A>
where
    A::HighPriorityMsg: Debug,
    A::LowPriorityMsg: Debug,
{
    /// Creates a test double for any actor conforming to the `MessageSet` `A`.
    ///
    /// This is the only function you need to call to create a mock.
    pub fn new(meta: A::Meta) -> Self {
        let (lo_tx, lo_rx) = mailbox::new(128);
        let (hi_tx, hi_rx) = mailbox::new(128);
        let (sys_tx, sys_rx) = mailbox::new(128);

        let handle = ActorHandle {
            hi_tx,
            lo_tx,
            sys_tx,
            meta,
        };

        let receivers = MockReceivers {
            high: hi_rx,
            low: lo_rx,
            _sys: sys_rx,
        };

        Self { handle, receivers }
    }

    /// Returns a clone of the handle to inject into the code under test.
    /// The returned handle has the correct type `ActorHandle<A>`.
    pub fn handle(&self) -> ActorHandle<A> {
        self.handle.clone()
    }

    /// Asserts that the next message is a Low-priority message and returns its content.
    /// Panics with a helpful message if the channel is closed or the message is High-priority.
    pub async fn expect_low(&mut self) -> A::LowPriorityMsg {
        match self.receivers.low.recv().await {
            Some(msg) => msg,
            None => panic!("Expected a Low-priority message, but the channel was closed."),
        }
    }

    /// Asserts that the next message is a High-priority message and returns its content.
    /// Panics with a helpful message if the channel is closed or the message is Low-priority.
    pub async fn expect_high(&mut self) -> A::HighPriorityMsg {
        match self.receivers.high.recv().await {
            Some(msg) => msg,
            None => panic!("Expected a High-priority message, but the channel was closed."),
        }
    }

    /// Asserts that NO messages of any kind have been sent to the actor.
    ///
    /// This is useful for testing paths where a component should *not* interact
    /// with a dependency.
    pub async fn expect_no_message(&mut self) {
        tokio::select! {
            res = self.receivers.low.recv() => {
                assert!(res.is_some(), "Expected no messages, but received a Low-priority message: {res:?}");
            }
            res = self.receivers.high.recv() => {
                assert!(res.is_some(), "Expected no messages, but received a High-priority message: {res:?}");
            }
            else => {},
        }
    }
}
