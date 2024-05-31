use anyhow::Result;
use clap::Args;
use tokio::net::{TcpListener, TcpStream};

// use crate::net;

/// Start the cinemotion broker services.
#[derive(Args)]
pub struct ServerCmd {
    #[clap(long = "address")]
    server_bind_address: Option<std::net::SocketAddr>,
}

impl ServerCmd {
    pub async fn run(&self) -> Result<i32> {
        // TODO: Setup PeerSevice - Every connnect is a spawned tokio stored in the service.
        // - connection spawns with a channel for the engine to send and receive messages
        // - engine service should receive a fan in channel and the connects get broadcast channel
        // - How do we id a the connect so it can filter non-relevant messages? The client needs to
        // filter out based on its own id which is wont have? Unless we init with one when we
        // establish.
        // -
        // TODO: Setup Protobuf JSON Layer
        // TODO: Setup Engine Server
        tracing::info!("starting cinemotion service");
        let listener = TcpListener::bind(self.server_bind_address.unwrap()).await?;
        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(net::handle_connection(stream));
        }

        tracing::info!("configure runtime services");
        Ok(0)
    }
}

mod net {
    use crate::Result;
    use futures::{Future, SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio_tungstenite::{accept_async, tungstenite};

    pub fn handle_connection(stream: TcpStream) -> impl Future<Output = Result<()>> {
        async move {
            let ws_stream = accept_async(stream).await?;
            let (mut write, mut read) = ws_stream.split();
            loop {
                tokio::select! {
                    msg = read.next() => {
                        match msg {
                            Some(Ok(tungstenite::Message::Text(msg))) => {
                                tracing::info!("received message: {}", msg);
                            }
                            Some(Ok(tungstenite::Message::Close(_))) => {
                                tracing::info!("closing connection");
                                break;
                            }
                            Some(Ok(_)) => {
                                tracing::info!("received binary message");
                            }
                            Some(Err(e)) => {
                                tracing::error!("error reading message: {}", e);
                                break;
                            }
                            None => break,
                        }
                    }
                    msg = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                        write.send(tungstenite::Message::Text("ping".into())).await?;
                    }
                }
            }
            Ok(())
        }
    }
}
