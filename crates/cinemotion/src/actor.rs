use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq)]
pub enum Error {
    // Send Error
    #[error("failed to send message")]
    SendError,

    // Response Failed
    #[error("responder channel was close while awaiting response")]
    ResponseFailed,
}

/// `Response` is a struct that represents a response that can be awaited.
/// It contains a `tokio::sync::mpsc::Receiver` that receives a value of type `T`.
#[derive(Debug)]
pub struct Response<T> {
    receiver: tokio::sync::mpsc::Receiver<T>,
}

impl<T> Response<T> {
    /// Creates a new `Response` and a corresponding `Responder`.
    pub fn new() -> (Responder<T>, Self) {
        let (tx, rx) = tokio::sync::mpsc::channel::<T>(1);
        (Responder { sender: tx }, Self { receiver: rx })
    }
}

impl<T> Future for Response<T> {
    type Output = Result<T, Error>;

    /// Polls the `Response`.
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        Pin::new(&mut this.receiver).poll_recv(cx).map(|p| match p {
            Some(res) => Ok(res),
            None => Err(Error::ResponseFailed),
        })
    }
}

/// `Responder` is a struct that represents a sender that can dispatch a value of type `T`.
#[derive(Clone, Debug)]
pub struct Responder<T> {
    sender: tokio::sync::mpsc::Sender<T>,
}

impl<T: Send + 'static> Responder<T> {
    /// Sends a value.
    pub async fn dispatch(self, value: T) -> Result<(), Error> {
        self.sender.try_send(value).map_err(|_| Error::SendError)
    }
}

/// `Sender` is a type alias for the sender channel used by the actor system.
/// It contains a `tokio::sync::mpsc::UnboundedSender<Event<M>>`.
#[derive(Clone, Debug)]
pub struct Sender<M>
where
    M: Send + Sync + 'static,
{
    sender: tokio::sync::mpsc::UnboundedSender<Event<M>>,
}

impl<M> Sender<M>
where
    M: Send + Sync + 'static,
{
    /// Sends a message.
    pub fn send(&self, message: M) -> Result<(), self::Error> {
        self.send_event(Event::Message(message))
    }

    /// Sends an event to the actor.
    pub fn send_event(&self, event: Event<M>) -> Result<(), self::Error> {
        self.sender.send(event).map_err(|_| self::Error::SendError)
    }
}

impl<M> From<tokio::sync::mpsc::UnboundedSender<Event<M>>> for Sender<M>
where
    M: Send + Sync + 'static,
{
    /// Converts a `tokio::sync::mpsc::UnboundedSender<Event<M>>` into a `Sender<M>`.
    fn from(sender: tokio::sync::mpsc::UnboundedSender<Event<M>>) -> Self {
        Self { sender }
    }
}

/// `Signal` represents control signals that can be sent to an actor.
#[derive(Debug)]
pub enum Signal {
    /// Signal to stop the actor.
    Stop,
}

/// `Event` represents messages sent to actors, including user-defined messages and control signals.
pub enum Event<M>
where
    M: Send + Sync + 'static,
{
    /// Custom message type used by the actor model for its specific behavior.
    Message(M),
    /// Control signal to stop the actor, with a channel to send the response.
    Stop {
        respond_to: tokio::sync::oneshot::Sender<()>,
    },
}

/// `Actor` represents the behavior of an actor in the system.
/// It defines a `Handle` type, a `Message` type, and a `handle_message` function.
#[async_trait]
pub trait Actor: Send + Sync
where
    Self::Handle: Handle<Message = Self::Message>,
    Self::Message: Send + Sync + 'static,
{
    type Handle;
    type Message;

    async fn handle_message(&mut self, message: Self::Message) -> Option<Signal>;

    async fn tick(&mut self) -> Option<Signal> {
        // Default implementation is a future that never resolves
        futures::future::pending().await
    }
}

/// `Handle` represents the handle used to interact with an actor.
/// It defines a `Message` type, a `new` function, a `sender` function, and a `stop` function.
#[async_trait]
pub trait Handle: Clone
where
    Self::Message: Send + Sync + 'static,
{
    type Message;

    /// Creates a new handle with the given sender channel.
    fn new(sender: Sender<Self::Message>) -> Self;

    /// Returns the sender channel associated with the handle.
    fn sender(&self) -> Sender<Self::Message>;

    /// Sends a stop signal to the actor and waits for acknowledgment.
    async fn stop(&mut self);
}

/// Spawns a new actor instance and returns its handle.
pub fn spawn<M: Actor + 'static>(mut model: M) -> M::Handle {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Event<M::Message>>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(event) = receiver.recv() => {
                    match event {
                        Event::Stop { respond_to } => {
                            respond_to.send(()).unwrap();
                            break;
                        }
                        Event::Message(message) => {
                            if let Some(Signal::Stop) = model.handle_message(message).await {
                                break;
                            }
                        }
                    }
                }
                signal = model.tick() => {
                    match signal {
                        Some(Signal::Stop) => break,
                        _ => {}
                    }
                }
            }
        }
    });

    M::Handle::new(sender.into())
}
