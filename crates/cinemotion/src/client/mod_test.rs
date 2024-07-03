use derive_more::{Display, Error};

use super::*;

use crate::actor::{self, Handle};

#[derive(Debug, Display, Error)]
struct TimeoutElapsed {}

struct TestActor<M>
where
    M: actor::Actor + 'static,
{
    pub model: M,
    receiver: tokio::sync::mpsc::UnboundedReceiver<actor::Event<M::Message>>,
}

impl<M> TestActor<M>
where
    M: actor::Actor + 'static,
{
    pub fn new(model: M) -> (Self, M::Handle) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let actor = Self { model, receiver };
        (actor, M::Handle::new(sender.into()))
    }

    /// Execute the actor loop and wait for the given future to complete or timeout.
    pub async fn wait_for<F, T>(
        &mut self,
        future: F,
        timeout_secs: Option<u64>,
    ) -> Result<T, TimeoutElapsed>
    where
        F: futures::Future<Output = T>,
    {
        let timeout_secs = timeout_secs.unwrap_or(3);
        let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs));
        tokio::pin!(timeout);
        tokio::pin!(future);
        loop {
            tokio::select! {
                Some(event) = self.receiver.recv() => {
                    match event {
                        actor::Event::Stop { respond_to } => {
                            respond_to.send(()).unwrap();
                            break;
                        }
                        actor::Event::Message(message) => {
                            self.model.handle_message(message).await;
                        }
                    }
                }
                _ = &mut timeout => {
                    panic!("Timeout");
                }
                result = &mut future => { return Ok(result) },
            }
        }
        // Handle case where the loop breaks but we are still waiting on the future.
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, future).await {
            Ok(result) => Ok(result),
            Err(_) => Err(TimeoutElapsed {}),
        }
    }
}

// Test the propogation of calling Actor.stop() on the ClientCoordinator
#[tokio::test]
async fn test_stop_propogation() {
    let (mut test_actor, mut handle) = TestActor::new(coordinator::ClientCoordinator::new());
    test_actor
        .wait_for(handle.stop(), None)
        .await
        .expect("stop call should return successfully.");
}
