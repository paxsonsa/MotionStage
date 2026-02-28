use std::net::IpAddr;

use crate::client::ClientObserver;

use super::*;

struct TestEngine;

impl Engine for TestEngine {}

/// Test that the runtime's cancellation token works as expected.
#[actix::test]
async fn test_runtime_actor_cancelltion() {
    let engine = Box::new(TestEngine {});
    let cancellation = CancellationToken::new();

    let inner_cancellation = cancellation.clone();
    // Start a new arbiter to run the actor.
    let arbiter = actix::Arbiter::new();
    let _ = arbiter.spawn(async move {
        let actor = RuntimeActor::new(engine, inner_cancellation.clone());
        let _addr = actor.start();
        System::current().stop();
    });

    tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        cancellation.cancelled(),
    )
    .await
    .unwrap();
    assert!(cancellation.is_cancelled());
}

struct TestClientSpy {
    registered: tokio::sync::mpsc::Receiver<()>,
}

impl TestClientSpy {
    async fn expect_registered(&mut self) {
        tokio::time::timeout(tokio::time::Duration::from_secs(1), self.registered.recv())
            .await
            .expect("registered() observer timed out.")
            .expect("registered() observer channel closed before message received.");
    }
}

/// A test observer for the actor client.
struct TestClientDelegate {
    registered: tokio::sync::mpsc::Sender<()>,
}

impl TestClientDelegate {
    fn create() -> (Box<dyn ClientObserver>, TestClientSpy) {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let delegate = Box::new(Self { registered: tx });
        (delegate, TestClientSpy { registered: rx })
    }
}

impl ClientObserver for TestClientDelegate {
    fn registered(&self) {
        println!("Client registered");
        let _ = self
            .registered
            .try_send(())
            .expect("Failed to send registered");
    }

    fn close(&self, _reason: &str) {}
}

#[actix::test]
async fn test_runtime_actor_register() {
    use crate::client::ClientActor;
    use crate::net;

    let rt_addr = net::get_new_localhost_address();
    let runtime_addr = RemoteAddr::new_from_id(rt_addr.clone(), RuntimeActor::ACTOR_ID);
    let cancellation = CancellationToken::new();

    let rt_cancellation = cancellation.clone();
    let arbiter = actix::Arbiter::new();
    let _ = arbiter.spawn(async move {
        let _cluster = Cluster::new(rt_addr, vec![]);
        let engine = Box::new(TestEngine {});
        let cancellation = CancellationToken::new();
        let actor = RuntimeActor::new(engine, cancellation.clone());
        let _addr = actor.start();
        rt_cancellation.cancelled().await;
        println!("Runtime cancelled");
    });
    let (delegate, mut delegate_spy) = TestClientDelegate::create();

    // TODO: Need to look into why this is failing, it appears to because the network interface is
    // not being started. and so the client actor is not being registered.

    let client_cancellation = cancellation.clone();
    let arbiter = actix::Arbiter::new();
    let _ = arbiter.spawn(async move {
        let client_addr = net::get_new_localhost_address();
        let _cluster = Cluster::new(client_addr, vec![rt_addr]);
        let addr = RemoteAddr::new_from_id(client_addr, ClientActor::ACTOR_ID);
        let handle = RuntimeHandle { addr: runtime_addr };
        let client = ClientActor::new(delegate);
        let _client = client.start();
        // handle.register_client(addr);
        client_cancellation.cancelled().await;
        println!("Client cancelled");
    });

    delegate_spy.expect_registered().await;
    cancellation.cancel();
}
