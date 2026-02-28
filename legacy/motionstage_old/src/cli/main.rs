use anyhow::{Context, Result};
use clap::{ArgAction, Parser};

mod server;

/// A tool for running, debugging and testing a motionstage server.
#[derive(Parser)]
#[clap(about, author, version)]
struct Opt {
    /// Make the output more verbose.
    #[clap(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,

    /// The subcommand to run.
    #[clap(subcommand)]
    subcommand: Command,
}

/// Subcommands for the CLI
#[derive(clap::Subcommand)]
enum Command {
    /// Print the version information.
    Version,
    /// Start a standalone motionstage server.
    Server(server::ServerCmd),
}

impl Command {
    pub fn run(&self, opts: &Opt) -> Result<i32> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to create tokio runtime")?;

        actix::System::with_tokio_rt(|| rt).block_on(async { self.run_async(opts).await })
    }

    async fn run_async(&self, opts: &Opt) -> Result<i32> {
        match self {
            Command::Version => {
                println!("motionstage {}", env!("CARGO_PKG_VERSION"));
                Ok(0)
            }
            Command::Server(cmd) => {
                configure_logging(i32::from(opts.verbose));
                cmd.run().await
            }
        }
    }
}

fn main() -> Result<()> {
    let opts = Opt::parse();
    let result = opts.subcommand.run(&opts)?;
    std::process::exit(result);
}

pub fn configure_logging(verbosity: i32) {
    let base_config = match verbosity {
        n if n <= -3 => String::new(),
        -2 => "motionstage=error".to_string(),
        -1 => "motionstage=warn".to_string(),
        0 => std::env::var("MOTIONSTAGE_LOG").unwrap_or_else(|_| "motionstage=info".to_string()),
        1 => "motionstage=debug".to_string(),
        2 => "motionstage=trace".to_string(),
        _ => "trace".to_string(),
    };

    // the RUST_LOG variable will always override the current settings
    let config = match std::env::var("RUST_LOG") {
        Ok(tail) => format!("{},{}", base_config, tail),
        Err(_) => base_config,
    };

    println!("Logging config: {}", config);

    std::env::set_var("MOTIONSTAGE_LOG", &config);
    tracing::subscriber::set_global_default(build_logging_subscriber(config)).unwrap();
}

pub fn build_logging_subscriber(
    config: String,
) -> Box<dyn tracing::Subscriber + Send + Sync + 'static> {
    use tracing_subscriber::layer::SubscriberExt;
    let env_filter = tracing_subscriber::filter::EnvFilter::from(config);
    let registry = tracing_subscriber::Registry::default().with(env_filter);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);
    Box::new(registry.with(fmt_layer.without_time()))
}
