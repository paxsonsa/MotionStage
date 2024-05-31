use anyhow::{Context, Result};
use clap::{ArgAction, Parser};

mod server;

/// A server for receiving and processing streamed motion data.
#[derive(Parser)]
#[clap(about, author)]
struct Opt {
    /// Make output more verbose
    #[clap(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,

    /// Make output less verbose
    ///
    /// This flag takes precedence over the --verbose flag
    #[clap(short, long, global = true, action = ArgAction::Count)]
    quiet: u8,

    /// The subcommand to run
    #[clap(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Print the version information.
    Version,
    // Start the cinemotion broker service
    Server(server::ServerCmd),
}

impl Command {
    pub fn run(&self) -> Result<i32> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to establish async runtime")?;
        rt.block_on(self.run_async())
    }

    pub async fn run_async(&self) -> Result<i32> {
        match self {
            Self::Version => {
                println!("cinemotion: {}", cinemotion::VERSION);
                Ok(0)
            }
            Self::Server(cmd) => cmd.run().await,
        }
    }
}

fn main() -> Result<()> {
    let opts = Opt::parse();
    configure_logging(i32::from(opts.verbose) - i32::from(opts.quiet));
    let code = opts.command.run()?;
    std::process::exit(code);
}

pub fn configure_logging(verbosity: i32) {
    let base_config = match verbosity {
        n if n <= -3 => String::new(),
        -2 => "cinemotion=error".to_string(),
        -1 => "cinemotion=warn".to_string(),
        0 => std::env::var("CINEMOTION_LOG").unwrap_or_else(|_| "cinemotion=info".to_string()),
        1 => "cinemotion=debug".to_string(),
        2 => "cinemotion=trace".to_string(),
        _ => "trace".to_string(),
    };

    // the RUST_LOG variable will always override the current settings
    let config = match std::env::var("RUST_LOG") {
        Ok(tail) => format!("{},{}", base_config, tail),
        Err(_) => base_config,
    };

    println!("Logging config: {}", config);

    std::env::set_var("CINEMOTION_LOG", &config);
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
