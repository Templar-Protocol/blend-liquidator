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
//! Phase 1 landed the pure fixed-point math and the ScVal/ledger-entry
//! codecs (`math`, `chain::xdr`); Phase 2 landed the chain layer
//! (`chain::rpc`, `chain::pool`, `chain::signer`, `chain::tx`); Phase 3
//! landed the store, the ledger poller, the tracker and `service`, which
//! wires them into a bot that validates its configuration, seeds its
//! tracked-user set and follows every configured pool until shut down.
//! Phase 4 lands the auctioneer (`auctioneer`, `queue`,
//! `math::liquidation`): the bot now decides which tracked borrowers are
//! liquidatable, builds the auction the contract should accept, lets the
//! contract judge the percent through simulation, records every creation
//! it decides to make, and — only when `DRY_RUN=false` **and** a signing
//! key is configured — signs and submits it through a per-key queue.
//! Phase 5 landed the filler (`filler`, `executor`, `inventory`,
//! `math::fill`): once a tick it plans a fill for every open liquidation
//! auction its pool configuration supports, holds its own position at or
//! above the pool's minimum health factor times `HF_SAFETY_MULTIPLIER`
//! while taking one over, and — armed, and only on the filler key's own
//! queue — submits it. Phase 6a lands the unwind (`math::unwind`, the
//! filler's unwind pass) and the notifier (`notifier`): a pool a fill
//! landed in is repaid from the wallet and withdrawn down to its floors,
//! pass after pass until one moves nothing, and what an unwind cannot
//! finish reaches the operator through a deduplicating
//! [`notifier::NotificationChannel`]. What is left is Phase 6b's
//! operational surface: the Telegram channel, metrics, the HTTP endpoints
//! and the sandbox integration tier.
//!
//! The repository scaffolding around it — CI gates, lint
//! posture, dev container, release preflight — is complete and enforced
//! from the first commit, so the liquidation logic lands into a repo that
//! already fails loudly.
//!
//! The module layout follows the design spec at
//! `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`.

pub mod auctioneer;
pub mod chain;
pub mod config;
pub mod executor;
pub mod filler;
pub mod http;
pub mod inventory;
pub mod ledger;
pub mod math;
pub mod metrics;
pub mod notifier;
pub mod queue;
pub mod service;
pub mod store;
pub mod tracker;

/// The committed mainnet snapshot the `chain::xdr` codec tests read.
#[cfg(test)]
pub(crate) mod fixture;

/// Scripted-RPC, store and notification-channel scaffolding shared by the
/// store, ledger, tracker, notifier and service tests.
#[cfg(test)]
pub(crate) mod harness;

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
    /// The store failed: connection, query, migration or a value it held.
    #[error("store: {0}")]
    Store(#[from] store::StoreError),
    /// The ledger poller failed.
    #[error("ledger: {0}")]
    Ledger(#[from] ledger::LedgerError),
    /// The tracker failed to apply an event or refresh a borrower.
    #[error("tracker: {0}")]
    Tracker(#[from] tracker::TrackerError),
    /// The auctioneer failed to decide who is liquidatable, or to act on it.
    #[error("auctioneer: {0}")]
    Auctioneer(#[from] auctioneer::AuctioneerError),
    /// The filler failed to plan or execute an auction's fill.
    #[error("filler: {0}")]
    Filler(#[from] filler::FillerError),
}
