//! Binary entry point: set up logging, parse configuration, report it, exit.

use blend_liquidator::config::{Args, LogFormat};
use clap::Parser;
use tracing_subscriber::EnvFilter;

fn main() {
    let args = Args::parse();

    // RUST_LOG wins when set; otherwise info for everything, debug for this
    // crate — the same posture the container image ships with.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,blend_liquidator=debug"));

    match args.log_format {
        LogFormat::Text => tracing_subscriber::fmt().with_env_filter(filter).init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init(),
    }

    // Logged at startup on purpose. When this bot can move inventory, the
    // single most important thing an operator needs from the first line of a
    // log is whether it is armed.
    tracing::info!(
        dry_run = args.dry_run,
        version = env!("CARGO_PKG_VERSION"),
        "blend-liquidator starting"
    );

    if args.dry_run {
        tracing::info!("dry-run: no transaction will be submitted");
    } else {
        tracing::warn!("LIVE: transactions will be submitted");
    }

    tracing::info!("nothing to do yet — this is a skeleton");
}
