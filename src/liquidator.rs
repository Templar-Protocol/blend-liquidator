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
//! Skeleton. Phase 1 landed the pure fixed-point math and the ScVal/ledger-
//! entry codecs (`math`, `chain::xdr`); Phase 2 landed the chain layer
//! (`chain::rpc`, `chain::pool`, `chain::signer`, `chain::tx`), which can
//! read a pool and sign and submit a transaction, but is driven by nothing
//! yet. The binary itself still just parses configuration, sets up logging
//! and exits. The repository scaffolding around it — CI gates, lint
//! posture, dev container, release preflight — is complete and enforced
//! from the first commit, so the liquidation logic lands into a repo that
//! already fails loudly.
//!
//! The module layout follows the design spec at
//! `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`. The
//! phases still to land are the store and ledger poller, the auctioneer,
//! the filler and executor, unwind, and the operational surface.

pub mod chain;
pub mod config;
pub mod math;

/// The committed mainnet snapshot the `chain::xdr` codec tests read.
#[cfg(test)]
pub(crate) mod fixture;

/// Errors this crate reports to its caller.
///
/// One variant per phase that exists, so the error taxonomy has a home
/// before there is a full pipeline to attribute failures to — a bot that
/// liquidates real positions needs to say *which phase* failed, and bolting
/// that on later means revisiting every call site.
#[derive(Debug, thiserror::Error)]
pub enum LiquidatorError {
    /// Configuration was rejected at startup, before anything could act on it.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The chain layer failed: transport, RPC, decoding, signing or submission.
    #[error("chain: {0}")]
    Chain(#[from] chain::ChainError),
}
