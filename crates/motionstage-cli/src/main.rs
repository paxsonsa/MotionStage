use anyhow::Result;
use clap::{Parser, Subcommand};
use motionstage_server::{ServerConfig, ServerHandle};
mod simulate;
use simulate::SimulateArgs;

#[derive(Parser)]
#[command(author, version, about = "MotionStage command line interface")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Simulate(SimulateArgs),
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
                "motionstage server started"
            );

            tokio::signal::ctrl_c().await?;
            server.stop().await?;
        }
        Command::Simulate(args) => {
            simulate::run(args).await?;
        }
        Command::Version => {
            println!("motionstage-cli {}", env!("CARGO_PKG_VERSION"));
        }
    }

    Ok(())
}
