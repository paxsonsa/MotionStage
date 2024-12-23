use anyhow::{Context, Result};
use clap::Args;

/// Serve Subcommand Parser and Runner.
#[derive(Args)]
pub struct ServerCmd {
    server_bind_address: Option<std::net::SocketAddr>,
}

impl ServerCmd {
    pub async fn run(&self) -> Result<i32> {
        let addr = self.server_bind_address.unwrap_or_else(|| {
            std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
                7788,
            )
        });

        let runtime = cinemotion::rt::Runtime::new();
        let server = cinemotion::server::Server::builder()
            .with_bind_address(addr)
            .build();

        server
            .run(runtime)
            .await
            .context("Server failed while running")?;

        Result::Ok(0)
    }
}
