//! Liquidation bot for [Blend Protocol](https://blend.capital) lending pools
//! on Stellar.
//!
//! **This bot is not non-custodial.** Like the venue it trades against, it is
//! expected to run unsupervised — but unlike the venue it holds a signing key
//! and submits transactions itself. That is the point of a liquidation bot,
//! and it is why dry-run is the default: see [`config::Args::dry_run`].
//!
//! # Status
//!
//! Skeleton. This crate currently parses configuration, sets up logging and
//! exits; the fixed-point math and XDR codecs exist (`math`, `chain::xdr`);
//! there is no RPC client, scanner or executor yet. The repository
//! scaffolding around it — CI gates, lint posture, dev container, release
//! preflight — is complete and enforced from the first commit, so the
//! liquidation logic lands into a repo that already fails loudly.
//!
//! The module layout is deliberately *not* pre-declared. A prior NEAR bot in
//! this organisation is a reasonable prior for what the seams will be, but
//! presuming it fits Blend before reading Blend's contracts would be a guess
//! dressed as a decision.

pub mod chain;
pub mod config;
pub mod math;

/// The committed mainnet snapshot every codec and math test reads.
#[cfg(test)]
pub(crate) mod fixture;

/// Errors this crate reports to its caller.
///
/// One variant today, and it exists so the error taxonomy has a home before
/// there is a pipeline to attribute failures to — a bot that liquidates real
/// positions needs to say *which phase* failed, and bolting that on later
/// means revisiting every call site.
#[derive(Debug, thiserror::Error)]
pub enum LiquidatorError {
    /// Configuration was rejected at startup, before anything could act on it.
    #[error("invalid configuration: {0}")]
    Config(String),
}
