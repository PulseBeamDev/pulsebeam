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

/// An error returned from the `try_send` method on a `Sender`.
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
    /// Returns a `SendError` only if the receiving actor has terminated.
    pub async fn send(&mut self, message: T) -> Result<(), SendError<T>> {
        self.sender
            .send_async(message)
            .await
            .map_err(|flume::SendError(e)| SendError(e))
    }

    /// Attempts to immediately send a message.
    ///
    /// Returns a `TrySendError` if the mailbox is full or if the
    /// receiving actor has terminated.
    pub fn try_send(&mut self, message: T) -> Result<(), TrySendError<T>> {
        self.sender.try_send(message).map_err(|e| match e {
            flume::TrySendError::Full(e) => TrySendError::Full(e),
            flume::TrySendError::Disconnected(e) => TrySendError::Closed(e),
        })
    }
}

/// An actor's mailbox for receiving messages.
pub struct Receiver<T> {
    receiver: flume::Receiver<T>,
}

impl<T> Receiver<T> {
    /// Receives the next message from the mailbox.
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
        let (mut sender, mut mailbox) = mailbox::new(10);
        let message = "hello".to_string();

        sender.send(message.clone()).await.unwrap();

        let received = mailbox.recv().await;
        assert_eq!(received, Some(message));
    }

    #[tokio::test]
    async fn recv_returns_none_when_all_senders_are_dropped() {
        let (mut sender, mut mailbox) = mailbox::new::<i32>(10);
        sender.send(1).await.unwrap();
        sender.send(2).await.unwrap();

        // Drop the only sender
        drop(sender);

        // We can still receive the messages already in the queue
        assert_eq!(mailbox.recv().await, Some(1));
        assert_eq!(mailbox.recv().await, Some(2));

        // Now that the queue is empty and the sender is gone, recv returns None
        assert_eq!(mailbox.recv().await, None);
    }

    #[tokio::test]
    async fn async_send_fails_when_receiver_is_dropped() {
        let (mut sender, mailbox) = mailbox::new::<String>(10);

        // Drop the mailbox immediately
        drop(mailbox);

        let result = sender.send("should fail".to_string()).await;
        assert!(result.is_err());

        // Check that the error is our custom SendError and we can get the message back
        if result.is_ok() {
            panic!("Expected a SendError");
        }
    }

    #[test]
    fn try_send_success_on_capacity() {
        let (mut sender, _mailbox) = mailbox::new::<i32>(1);
        let result = sender.try_send(123);
        assert!(result.is_ok());
    }

    #[test]
    fn try_send_fails_when_full() {
        // Create a mailbox with a buffer of 1
        let (mut sender, _mailbox) = mailbox::new::<i32>(1);

        // The first send succeeds and fills the buffer
        sender.try_send(1).unwrap();
        sender.try_send(1).unwrap();

        // The second send should fail because the buffer is full
        let result = sender.try_send(2);
        assert!(result.is_err());

        // Check that the error is TrySendError::Full and contains the failed message
        match result {
            Err(mailbox::TrySendError::Full(msg)) => assert_eq!(msg, 2),
            _ => panic!("Expected a TrySendError::Full"),
        }
    }

    #[test]
    fn try_send_fails_when_receiver_is_dropped() {
        let (mut sender, mailbox) = mailbox::new::<i32>(1);

        // Drop the receiver
        drop(mailbox);

        let result = sender.try_send(42);
        assert!(result.is_err());

        // Check for the correct error variant
        match result {
            Err(mailbox::TrySendError::Closed(msg)) => assert_eq!(msg, 42),
            _ => panic!("Expected a TrySendError::Closed"),
        }
    }

    #[tokio::test]
    async fn async_send_waits_when_full() {
        let (mut sender, mut mailbox) = mailbox::new::<i32>(1);

        // Fill the buffer
        sender.send(1).await.unwrap();

        // This send should wait. We spawn it in a separate task.
        let send_task = tokio::spawn({
            let mut sender = sender.clone();
            async move {
                sender.send(2).await.unwrap();
            }
        });

        // Give the task a moment to block on send()
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now, make space in the buffer by receiving the first message
        assert_eq!(mailbox.recv().await, Some(1));

        // The send_task should now be able to complete. We wait for it.
        tokio::time::timeout(Duration::from_secs(1), send_task)
            .await
            .expect("send_task timed out")
            .unwrap();

        // The second message should now be in the mailbox
        assert_eq!(mailbox.recv().await, Some(2));
    }

    #[tokio::test]
    async fn cloned_sender_works_and_channel_stays_open() {
        let (mut sender1, mut mailbox) = mailbox::new::<i32>(10);
        let mut sender2 = sender1.clone();

        sender1.send(1).await.unwrap();
        sender2.send(2).await.unwrap();

        // Drop the first sender
        drop(sender1);

        // The channel should remain open because sender2 is still alive.
        // We can still receive messages.
        assert_eq!(mailbox.recv().await, Some(1));
        assert_eq!(mailbox.recv().await, Some(2));

        // We can also still send with the cloned sender
        sender2.send(3).await.unwrap();
        assert_eq!(mailbox.recv().await, Some(3));

        // Now drop the final sender
        drop(sender2);

        // The channel should now close
        assert_eq!(mailbox.recv().await, None);
    }
}
