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
//! The module layout follows the design spec at
//! `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`.
//! `math` and `chain::xdr` are the two modules it declares that exist
//! today; the rest — the RPC client and pool reads, the store and ledger
//! poller, the auctioneer, the filler and executor, unwind, and the
//! operational surface — do not yet.

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
