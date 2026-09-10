//! Binary entry point: set up logging, build the configuration, dispatch on
//! run mode, and exit with a code that says what happened.
//!
//! Exit codes: `0` success, `1` a fatal failure once running (chain, store,
//! ledger or tracker), `2` a configuration problem — including a failed
//! `check-config` — caught before any of that ran.

use blend_liquidator::config::{Args, LogFormat, RunMode};
use blend_liquidator::service::Service;
use blend_liquidator::LiquidatorError;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// A configuration problem, caught before the bot did anything with it.
const EXIT_CONFIG: i32 = 2;
/// A fatal failure after the bot started running.
const EXIT_FATAL: i32 = 1;

#[tokio::main]
async fn main() {
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

    let config = match args.service() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "configuration error");
            std::process::exit(EXIT_CONFIG);
        }
    };

    // Both arrive from the environment, never argv: a signing key on the
    // command line is readable from `/proc/<pid>/cmdline` and shows up in
    // `ps` and `docker inspect`. Neither is a clap argument for exactly
    // that reason — see `Args::auctioneer_signer`'s own doc. An empty
    // value counts as absent, the same treatment `Args::chain` and
    // `Args::service` give `RPC_API_KEY` and `DATABASE_URL`.
    let signer = match args.auctioneer_signer(
        std::env::var("FILLER_SECRET_KEY")
            .ok()
            .filter(|key| !key.is_empty()),
        std::env::var("AUCTIONEER_SECRET_KEY")
            .ok()
            .filter(|key| !key.is_empty()),
    ) {
        Ok(signer) => signer,
        Err(error) => {
            tracing::error!(%error, "configuration error");
            std::process::exit(EXIT_CONFIG);
        }
    };

    let exit_code = match config.run_mode {
        RunMode::CheckConfig => match Service::check_config(&config).await {
            Ok(_warnings) => 0,
            Err(error) => {
                tracing::error!(%error, "configuration check failed");
                EXIT_CONFIG
            }
        },
        RunMode::Loop => match Service::run(config, signer).await {
            Ok(()) => 0,
            Err(error @ LiquidatorError::Config(_)) => {
                tracing::error!(%error, "configuration error");
                EXIT_CONFIG
            }
            Err(error) => {
                tracing::error!(%error, "fatal error");
                EXIT_FATAL
            }
        },
    };
    std::process::exit(exit_code);
}
