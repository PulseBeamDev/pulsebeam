use flume;
use std::error::Error;
use std::fmt;

/// An error returned when sending on a closed mailbox.
///
/// This error is returned by the asynchronous `send` method. It contains
/// the message that could not be sent.
#[derive(Debug, PartialEq, Eq)]
pub struct SendError<T>(pub T);

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sending on a closed mailbox")
    }
}

impl<T: fmt::Debug> Error for SendError<T> {}

/// An error returned from the `try_send_sync` method on a `Sender`.
#[derive(Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    /// The mailbox was full and could not accept the message.
    Full(T),
    /// The receiving actor has terminated and the mailbox is closed.
    Closed(T),
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(_) => write!(f, "sending on a full mailbox"),
            TrySendError::Closed(_) => write!(f, "sending on a closed mailbox"),
        }
    }
}

impl<T: fmt::Debug> Error for TrySendError<T> {}

impl<T> From<SendError<T>> for TrySendError<T> {
    fn from(err: SendError<T>) -> Self {
        TrySendError::Closed(err.0)
    }
}

/// A handle to send messages to an actor's mailbox.
pub struct Sender<T> {
    sender: flume::Sender<T>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<T> Sender<T> {
    /// Sends a message asynchronously, waiting if the mailbox is full.
    ///
    /// This is the default, asynchronous way to send a message.
    /// Returns a `SendError` only if the receiving actor has terminated.
    pub async fn send(&self, message: T) -> Result<(), SendError<T>> {
        self.sender
            .send_async(message)
            .await
            .map_err(|e| SendError(e.0))
    }

    /// Attempts to immediately send a message synchronously.
    ///
    /// This method does not block. It is useful for cases where sending
    /// must not wait, such as in timers or non-async contexts.
    ///
    /// Returns a `TrySendError` if the mailbox is full or if the
    /// receiving actor has terminated.
    pub fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        use flume::TrySendError as FlumeTrySendError;
        self.sender.try_send(message).map_err(|e| match e {
            FlumeTrySendError::Full(m) => {
                tracing::debug!("try_send_sync dropped a packet due to full queue");
                TrySendError::Full(m)
            }
            FlumeTrySendError::Disconnected(m) => TrySendError::Closed(m),
        })
    }
}

/// An actor's mailbox for receiving messages.
pub struct Receiver<T> {
    receiver: flume::Receiver<T>,
}

impl<T> Receiver<T> {
    /// Receives the next message from the mailbox.
    ///
    /// This is the default, asynchronous way to receive a message.
    /// It returns `None` if all sender handles have been dropped and
    /// the mailbox is empty.
    pub async fn recv(&mut self) -> Option<T> {
        self.receiver.recv_async().await.ok()
    }
}

/// Creates a new mailbox and a corresponding sender handle.
pub fn new<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = flume::bounded(buffer);
    (Sender { sender }, Receiver { receiver })
}

#[cfg(test)]
mod tests {
    use crate::mailbox;
    use std::time::Duration;

    #[tokio::test]
    async fn send_and_recv_single_message() {
        let (sender, mut mailbox) = mailbox::new::<String>(10);
        let message = "hello".to_string();

        sender.send(message.clone()).await.unwrap();

        let received = mailbox.recv().await;
        assert_eq!(received, Some(message));
    }

    #[tokio::test]
    async fn recv_returns_none_when_all_senders_are_dropped() {
        let (sender, mut mailbox) = mailbox::new::<i32>(10);
        sender.send(1).await.unwrap();
        sender.send(2).await.unwrap();

        drop(sender);

        assert_eq!(mailbox.recv().await, Some(1));
        assert_eq!(mailbox.recv().await, Some(2));
        assert_eq!(mailbox.recv().await, None);
    }

    #[tokio::test]
    async fn async_send_fails_when_receiver_is_dropped() {
        let (sender, mailbox) = mailbox::new::<String>(10);

        drop(mailbox);

        let result = sender.send("should fail".to_string()).await;
        assert!(result.is_err());

        if let Err(mailbox::SendError(msg)) = result {
            assert_eq!(msg, "should fail");
        } else {
            panic!("Expected a SendError");
        }
    }

    #[test]
    fn try_send_sync_success_on_capacity() {
        let (sender, _mailbox) = mailbox::new::<i32>(1);
        // Use the renamed method
        let result = sender.try_send(123);
        assert!(result.is_ok());
    }

    #[test]
    fn try_send_sync_fails_when_full() {
        let (sender, _mailbox) = mailbox::new::<i32>(1);

        // The first send succeeds and fills the buffer
        sender.try_send(1).unwrap();

        // The second send should fail because the buffer is full
        let result = sender.try_send(2);
        assert!(result.is_err());

        match result {
            Err(mailbox::TrySendError::Full(msg)) => assert_eq!(msg, 2),
            _ => panic!("Expected a TrySendError::Full"),
        }
    }

    #[test]
    fn try_send_sync_fails_when_receiver_is_dropped() {
        let (sender, mailbox) = mailbox::new::<i32>(1);

        drop(mailbox);

        // Use the renamed method
        let result = sender.try_send(42);
        assert!(result.is_err());

        match result {
            Err(mailbox::TrySendError::Closed(msg)) => assert_eq!(msg, 42),
            _ => panic!("Expected a TrySendError::Closed"),
        }
    }

    #[tokio::test]
    async fn async_send_waits_when_full() {
        let (sender, mut mailbox) = mailbox::new::<i32>(1);

        sender.send(1).await.unwrap();

        let send_task = tokio::spawn({
            let sender = sender.clone();
            async move {
                sender.send(2).await.unwrap();
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(mailbox.recv().await, Some(1));

        tokio::time::timeout(Duration::from_secs(1), send_task)
            .await
            .expect("send_task timed out")
            .unwrap();

        assert_eq!(mailbox.recv().await, Some(2));
    }

    #[tokio::test]
    async fn cloned_sender_works_and_channel_stays_open() {
        let (sender1, mut mailbox) = mailbox::new::<i32>(10);
        let sender2 = sender1.clone();

        sender1.send(1).await.unwrap();
        sender2.send(2).await.unwrap();

        drop(sender1);

        assert_eq!(mailbox.recv().await, Some(1));
        assert_eq!(mailbox.recv().await, Some(2));

        sender2.send(3).await.unwrap();
        assert_eq!(mailbox.recv().await, Some(3));

        drop(sender2);

        assert_eq!(mailbox.recv().await, None);
    }
}
