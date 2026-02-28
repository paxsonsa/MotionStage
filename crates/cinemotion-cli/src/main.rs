use anyhow::Result;
use cinemotion_server::{ServerConfig, ServerHandle};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(author, version, about = "CineMotion command line interface")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Version,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();

    match cli.command {
        Command::Serve => {
            let server = ServerHandle::new(ServerConfig::default());
            let adv = server.start().await?;
            tracing::info!(
                name = %adv.service_name,
                host = %adv.bind_host,
                port = adv.bind_port,
                "cinemotion server started"
            );

            tokio::signal::ctrl_c().await?;
            server.stop().await?;
        }
        Command::Version => {
            println!("cinemotion-cli {}", env!("CARGO_PKG_VERSION"));
        }
    }

    Ok(())
}
