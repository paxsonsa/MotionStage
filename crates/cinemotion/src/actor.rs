use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ActorError {
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
    type Output = Result<T, ActorError>;

    /// Polls the `Response`.
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        Pin::new(&mut this.receiver).poll_recv(cx).map(|p| match p {
            Some(res) => Ok(res),
            None => Err(ActorError::ResponseFailed),
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
    pub async fn dispatch(self, value: T) -> Result<(), ActorError> {
        self.sender
            .try_send(value)
            .map_err(|_| ActorError::SendError)
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
    pub fn send(&self, message: M) -> Result<(), self::ActorError> {
        self.send_event(Event::Message(message))
    }

    /// Sends an event to the actor.
    pub fn send_event(&self, event: Event<M>) -> Result<(), self::ActorError> {
        self.sender
            .send(event)
            .map_err(|_| self::ActorError::SendError)
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

impl<M> Event<M>
where
    M: Send + Sync + 'static,
{
    pub fn stop() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Self::Stop { respond_to: tx }, rx)
    }
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

    /// Returns the sender channel associated with the handle.
    fn sender(&self) -> Sender<Self::Message>;

    /// Snds a stop signal to the actor and waits for acknowledgment.
    async fn stop(&mut self) {
        let (stop, rx) = Event::stop();
        if let Err(err) = self.sender().send_event(stop) {
            tracing::error!(?err, "Failed to send stop event to actor");
            return;
        }
        if let Err(err) = rx.await {
            tracing::error!(?err, "Failed to receive acknowledgment from actor");
        }
    }
}

pub trait HandleExt: Handle {
    /// Sends a message to the actor.
    fn send(&mut self, message: Self::Message) -> Result<(), ActorError> {
        self.sender().send(message)
    }
    /// Sends a message to the actor and awaits a response.
    ///
    /// This function takes a closure that returns a tuple of a message and a response future.
    /// The message is sent to the actor, and then the function awaits the response.
    ///
    /// # Type Parameters
    ///
    /// * `T`: The type of the value that the response future resolves to. Must be `Send` and `Sync`.
    /// * `F`: The type of the closure that produces the message and response. The closure must be `Send` and `Sync`.
    ///
    /// # Parameters
    ///
    /// * `message_fn`: The closure that produces the message and response.
    ///
    /// # Returns
    ///
    /// The value that the response future resolves to.
    ///
    /// # Panics
    ///
    /// This function will panic if sending the message fails or if the response future is erroneous.
    async fn perform_send<
        T: Send + Sync,
        F: (FnOnce() -> (Self::Message, Response<T>)) + Send + Sync,
    >(
        &self,
        message_fn: F,
    ) -> T {
        let (msg, response) = message_fn();
        self.sender().send(msg).expect("send should work");
        response.await.expect("response should not fail")
    }
}

/// Spawns a new actor instance and returns its handle.
pub fn spawn<M: Actor + 'static, F: FnOnce(Sender<M::Message>) -> M::Handle>(
    mut model: M,
    new_handle: F,
) -> M::Handle {
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
    new_handle(sender.into())
}
