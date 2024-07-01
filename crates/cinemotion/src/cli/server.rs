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
        let address = self
            .server_bind_address
            .unwrap_or("0.0.0.0:7878".parse().unwrap());

        let (c_sender, mut c_receiver) = tokio::sync::mpsc::unbounded_channel::<usize>();
        let listener = TcpListener::bind(address).await?;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    break;
                }
                value = c_receiver.recv() => {
                    tracing::info!("received value: {:?}", value);
                }
                res = net::accept_connection(&listener, c_sender.clone()) => {
                    if let Err(e) = res {
                        tracing::error!("error accepting connections: {}", e);
                    }
                }
            }
        }
        Ok(0)
    }
}
mod net {
    use crate::Result;
    use futures::{Future, SinkExt, StreamExt};
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::mpsc::UnboundedSender,
    };
    use tokio_tungstenite::{accept_async, tungstenite};

    struct ConnectionAgent {
        uid: usize,
    }

    struct NetAgent {}

    struct NetConnectionGrp {}

    pub async fn accept_connection(listener: &TcpListener, net_agent: &NetAgent) -> Result<()> {
        let (stream, _) = listener.accept().await?;
        let connection = net_agent.register().await?;
        tokio::spawn(handle_connection(stream, connection));
        net_agent.send(42).unwrap();
        Ok(())
    }

    pub fn handle_connection(
        stream: TcpStream,
        connection: NetConnectionGroup,
    ) -> impl Future<Output = Result<()>> {
        async move {
            let ws_stream = accept_async(stream).await?;
            let (mut write, mut read) = ws_stream.split();
            let mut counter = 0;
            loop {
                tokio::select! {
                    msg = read.next() => {
                        match msg {
                            Some(Ok(tungstenite::Message::Text(msg))) => {
                                tracing::info!("received message: {}", msg);
                                connection.receive(msg, false).await;
                            }
                            Some(Ok(tungstenite::Message::Close(_))) => {
                                tracing::info!("closing connection");
                                connection.close().await;
                                break;
                            }
                            Some(Ok(msg)) => {
                                tracing::info!("received binary message");
                                connection.receive(msg, true);
                            }
                            Some(Err(e)) => {s
                                tracing::error!("error reading message: {}", e);
                                connection.error();
                                break;
                            }
                            None => break,
                        };
                        counter += 1;
                    }
                    msg = connection.outbound_message() => {
                        if let Some(msg) = msg {
                            write.send(tungstenite::Message::Text(msg)).await?;
                        }
                    },
                    _msg = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                        write.send(tungstenite::Message::Text("ping".into())).await?;
                    }
                }
            }
            Ok(())
        }
    }
}
