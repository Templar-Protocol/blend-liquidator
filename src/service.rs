//! Wiring: validate the configuration, seed the store, follow the
//! configured pools, decide and act on what the tracker finds, and shut
//! down cleanly.
//!
//! `Service` has two entry points, matching [`crate::config::RunMode`]:
//! [`Service::check_config`] reads the chain and pings the store and
//! reports, writing nothing and following nothing, and [`Service::run`]
//! additionally migrates the store, seeds it, and follows every configured
//! pool until a shutdown signal arrives. Both share `validate`, because a
//! bot that never checked its own configuration would happily submit
//! against a pool it misread.
//!
//! # The tracker and the auctioneer are separate tasks, joined by a tick
//!
//! `tracker_loop` is the one consumer of the poller channel every pool's
//! events and ticks arrive on, and its cursor rests on a strict rule:
//! `handle_message`'s `Tick` arm acknowledges a ledger — which is what
//! lets the poller commit its cursor — only once that ledger's accounts
//! are refreshed and flagged in the store. A creation decision is not one
//! of those effects, so the auctioneer never sits inside that path: it is
//! `auctioneer_loop`, a second task fed by a `tokio::sync::watch` that
//! `handle_message` publishes to *after* it acknowledges, never a second
//! reader of the poller channel itself (which would break the per-sender
//! ordering the cursor rests on). The watch carries a value forward, never
//! an acknowledgement backward, so nothing the auctioneer does can reach
//! back to delay or fail a tick already committed — a slow pass simply
//! falls behind the newest ledger, and a failed one logs and moves on to
//! the next pool or borrower (see `recheck_batch`'s and
//! `auctioneer_tick`'s own docs for exactly which failures are isolated
//! that way). The one failure that ends the auctioneer's *own* task is a
//! [`crate::store::StoreError`], for the same reason it ends the
//! tracker's: the bot cannot trust what it reads.
//!
//! The durable link between the two tasks is
//! [`crate::store::Store::flag_recheck`]: `apply_tick` flags every account
//! its events named and every stale row its refresh pass touched, and the
//! auctioneer's own oracle-scan and full-scan cadences flag more on their
//! own schedules. The auctioneer reads that flagged set, decides, acts,
//! and clears each flag with the ledger it was read at — never the tick's
//! own ledger — so a flag raised again mid-decision survives the decision
//! that never saw it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rand::RngExt as _;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::auctioneer::{Auctioneer, AuctioneerConfig, AuctioneerError, PriceWatch};
use crate::chain::pool::{PoolReader, PoolSnapshot};
use crate::chain::rpc::RpcClient;
use crate::chain::xdr::PoolStatus;
use crate::chain::{Network, Signer, Submitter, TxConfig};
use crate::config::{PoolConfig, SeedConfig, ServiceConfig, SigningKeys};
use crate::ledger::{LedgerPoller, LedgerTick, PollerConfig, PollerMessage};
use crate::queue::{run_queue, SubmissionQueue};
use crate::store::{events_cursor, Cursor, Store, StoreError, TrackedUser};
use crate::tracker::{AnalyticsSeed, FileSeed, SeedSource, Tracker, TrackerError};
use crate::LiquidatorError;

/// What [`Service::check_config`] prints for one pool: how many reserves it
/// has and which backstop it reports, once `validate` has confirmed every
/// pool agrees on the backstop and can price and collateralise its primary
/// asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolValidation {
    /// The pool contract.
    pub pool: String,
    /// How many reserves the pool lists.
    pub reserves: usize,
    /// The backstop contract every pool must agree on.
    pub backstop: String,
}

/// Checks the things a bot with no signer can check: that every pool
/// answers, that they share one backstop (a filler's position is shared
/// across every pool it follows, so it cannot serve two), and that the
/// assets each pool config names are reserves it can actually use. A pool
/// that is not active, or an asset the oracle does not price, is a warning:
/// neither stops the bot from following the pool, only from acting on it.
async fn validate(
    rpc: &RpcClient,
    pools: &[PoolConfig],
) -> Result<(Vec<PoolValidation>, Vec<String>), LiquidatorError> {
    let mut validations = Vec::with_capacity(pools.len());
    let mut warnings = Vec::new();
    let mut shared_backstop: Option<(String, String)> = None;

    for pool in pools {
        let snapshot = PoolReader::new(rpc, &pool.address).snapshot(&[]).await?;
        let backstop = snapshot.instance.backstop.clone();

        match &shared_backstop {
            None => shared_backstop = Some((pool.address.clone(), backstop.clone())),
            Some((first_pool, first_backstop)) if *first_backstop != backstop => {
                return Err(LiquidatorError::Config(format!(
                    "pool {first_pool} reports backstop {first_backstop}, but pool {} reports \
                     backstop {backstop}: a filler's position is shared across every pool it \
                     follows, so they cannot use two backstops",
                    pool.address
                )));
            }
            Some(_) => {}
        }

        validate_primary_asset(&pool.address, &pool.primary_asset, &snapshot)?;
        validate_supported_assets(&pool.address, &pool.supported_bid, &snapshot)?;
        validate_supported_assets(&pool.address, &pool.supported_lot, &snapshot)?;

        if snapshot.instance.config.status != PoolStatus::Active {
            warnings.push(format!(
                "pool {}: status is {:?}, not active",
                pool.address, snapshot.instance.config.status
            ));
        }
        for reserve in snapshot.reserves.values() {
            if snapshot.prices.price(&reserve.asset).is_err() {
                warnings.push(format!(
                    "pool {}: the oracle has no price for {}",
                    pool.address, reserve.asset
                ));
            }
        }

        validations.push(PoolValidation {
            pool: pool.address.clone(),
            reserves: snapshot.reserves.len(),
            backstop,
        });
    }
    Ok((validations, warnings))
}

/// The primary asset must be a reserve the pool can actually hold as
/// collateral: listed, enabled, and with a positive collateral factor.
/// Every failure names the asset, since that is what an operator fixes.
fn validate_primary_asset(
    pool: &str,
    asset: &str,
    snapshot: &PoolSnapshot,
) -> Result<(), LiquidatorError> {
    let reserve = snapshot
        .asset_index
        .get(asset)
        .and_then(|index| snapshot.reserves.get(index));
    let Some(reserve) = reserve else {
        return Err(LiquidatorError::Config(format!(
            "pool {pool}: primary asset {asset} is not a reserve"
        )));
    };
    if !reserve.config.enabled {
        return Err(LiquidatorError::Config(format!(
            "pool {pool}: primary asset {asset} is disabled"
        )));
    }
    if reserve.config.c_factor == 0 {
        return Err(LiquidatorError::Config(format!(
            "pool {pool}: primary asset {asset} has no collateral factor"
        )));
    }
    Ok(())
}

/// Every explicitly named asset (anything but the `"*"` wildcard) must be
/// one of the pool's reserves.
fn validate_supported_assets(
    pool: &str,
    assets: &[String],
    snapshot: &PoolSnapshot,
) -> Result<(), LiquidatorError> {
    for asset in assets {
        if asset != "*" && !snapshot.asset_index.contains_key(asset) {
            return Err(LiquidatorError::Config(format!(
                "pool {pool}: supported asset {asset} is not a reserve"
            )));
        }
    }
    Ok(())
}

/// Logs the redacted configuration, the per-pool validation and every
/// warning, at the posture this crate's logs use throughout: `tracing`,
/// never a print.
fn log_validation(config: &ServiceConfig, validations: &[PoolValidation], warnings: &[String]) {
    tracing::info!(?config, "resolved configuration");
    for validation in validations {
        tracing::info!(
            pool = validation.pool,
            reserves = validation.reserves,
            backstop = validation.backstop,
            "pool validated"
        );
    }
    for warning in warnings {
        tracing::warn!(%warning, "configuration warning");
    }
}

/// Builds one [`SeedSource`] per seed configured, failing loudly on a source
/// that cannot even be built (a malformed seed file, say) rather than
/// silently following no accounts. A source that merely fails to *answer*
/// later is [`Tracker::seed`]'s concern, not this one: that failure is a
/// coverage loss the tracker already treats as a warning, never a startup
/// failure.
fn build_seed_sources(seed: &SeedConfig) -> Result<Vec<SeedSource>, LiquidatorError> {
    let mut sources = Vec::new();
    if let Some(url) = &seed.url {
        let analytics = AnalyticsSeed::new(url, seed.health_factor_max)
            .map_err(|error| LiquidatorError::Config(format!("seed url: {error}")))?;
        sources.push(SeedSource::Analytics(analytics));
    }
    if let Some(path) = &seed.file {
        let file = FileSeed::load(path)
            .map_err(|error| LiquidatorError::Config(format!("seed file: {error}")))?;
        sources.push(SeedSource::File(file));
    }
    if sources.is_empty() {
        tracing::warn!(
            "no seed source is configured; a pool with no stored users starts \
             empty and the tracker follows only accounts that later appear in \
             events"
        );
    }
    Ok(sources)
}

/// Seeds every pool whose tracked-user count is zero or whose events cursor
/// is missing: either means this pool has never been followed by this
/// store, so there is nothing yet for the poller to refresh incrementally.
///
/// Returns the pools whose seed could not reach every source, for the
/// tracker loop to retry on its full-scan cadence as spec §4 requires.
async fn seed_pools_needing_it(
    rpc: &RpcClient,
    store: &Store,
    pools: &[PoolConfig],
    sources: &[SeedSource],
    batch: u32,
    shutdown: &watch::Receiver<bool>,
) -> Result<BTreeSet<String>, LiquidatorError> {
    let tracker = Tracker::new(rpc, store);
    let mut incomplete = BTreeSet::new();
    for pool in pools {
        if *shutdown.borrow() {
            tracing::warn!("shutdown requested; stopping before every pool was seeded");
            break;
        }
        let user_count = store.count_users(&pool.address).await?;
        let cursor = store.cursor(&events_cursor(&pool.address)).await?;
        if user_count != 0 && cursor.is_some() {
            continue;
        }
        let head = rpc.latest_ledger().await?;
        let tick = LedgerTick {
            sequence: head.sequence,
            close_time: head.close_time,
        };
        let outcome = match tracker
            .seed(&pool.address, sources, tick, batch, shutdown)
            .await
        {
            Ok(outcome) => outcome,
            // The same split the tracker loop makes: a store failure is
            // fatal because the bot would be trading on state it cannot
            // write, while a chain or math failure costs coverage of this
            // pool and is retried. Propagating the latter would let a
            // single unpriced reserve — which `validate` deliberately
            // records as a warning that does not stop the bot — keep the
            // bot from ever starting.
            Err(error @ TrackerError::Store(_)) => return Err(error.into()),
            Err(error) => {
                tracing::warn!(pool = pool.address, %error, "seeding this pool failed; it will be retried");
                incomplete.insert(pool.address.clone());
                continue;
            }
        };
        if !outcome.is_complete() {
            incomplete.insert(pool.address.clone());
        }
        // The events cursor starts where the seed's own ledger ends, so the
        // poller resumes there rather than from the head *it* reads: every
        // event between the two — the whole time seeding takes, pool by
        // pool — would otherwise be read by nothing, permanently, because a
        // pool with users and a cursor is never seeded again.
        //
        // Which is exactly why only a *complete* seed may write it: a
        // partial seed that recorded its position would be claiming to have
        // read a ledger it did not finish reading.
        //
        // Withholding it does not, on its own, make the reseed durable. The
        // poller starts from the head it reads when no cursor is stored and
        // writes one on its first acknowledged tick, so within about a poll
        // interval this pool has a cursor and users again, and a restart
        // after that skips it just the same. What the gap buys is the
        // current run's retry — `needs_reseed` carries it to the next full
        // scan — and a restart inside that first interval. A reseed that
        // survives any restart needs a durable marker, which is Phase 4's:
        // the skip test cannot ask "was this seed complete?" of a store
        // that does not record the answer.
        if outcome.is_complete() {
            store
                .set_cursor(
                    &events_cursor(&pool.address),
                    &Cursor {
                        ledger: head.sequence,
                        paging_token: None,
                    },
                )
                .await?;
        }
        tracing::info!(
            pool = pool.address,
            tracked = outcome.refresh.tracked,
            failed_sources = outcome.failed_sources,
            stopped_early = outcome.stopped_early,
            complete = outcome.is_complete(),
            "seeded pool"
        );
    }
    Ok(incomplete)
}

/// Opens the store, reporting a failure to open it as a *configuration*
/// problem rather than a store failure. An unreachable instance, a wrong
/// password or a malformed `DATABASE_URL` is something an operator fixes in
/// configuration, and spec §10's exit-code contract calls that a 2; a store
/// that answered and then failed is a 1. The message never carries the DSN:
/// [`crate::store::StoreError::Connect`] renders fixed text.
async fn connect_store(config: &ServiceConfig) -> Result<Store, LiquidatorError> {
    Store::connect(
        config.database_url.expose(),
        config.database_max_connections,
    )
    .await
    .map_err(|error| LiquidatorError::Config(format!("database: {error}")))
}

/// Timings the tracker loop reads every message, bundled so its functions
/// do not grow a parameter per knob. Copy: every field is a plain number.
#[derive(Debug, Clone, Copy)]
struct Cadence {
    /// A user's row older than this many ledgers is refreshed.
    user_refresh_ledgers: u32,
    /// How many stale users to refresh per tick.
    refresh_batch: u32,
    /// How often the full scan reports the least healthy borrowers.
    full_scan_ledgers: u32,
    /// The health factor the full scan reports below, 7 decimals.
    scan_health_factor: i128,
    /// This instance's phase in the full-scan cadence: drawn once at
    /// startup so several bots following the same pool do not all fire the
    /// full scan on the same ledger.
    phase: u32,
}

/// How many of the least healthy borrowers the full scan reports.
const FULL_SCAN_REPORT_LIMIT: i64 = 20;

/// This instance's phase in a cadence of `period` ledgers, drawn once at
/// startup. `0` when `period` is zero, which never fires anyway.
fn scan_phase(period: u32) -> u32 {
    if period == 0 {
        0
    } else {
        rand::rng().random_range(0..period)
    }
}

/// Whether the full scan fires at `ledger`: once every `period` ledgers, at
/// this instance's `phase`, so several bots following the same pool do not
/// all scan on the same ledger.
///
/// `last` is the ledger this pool's scan last fired at. The test is whether
/// `ledger` has entered a new phased period since then, never whether it
/// lands exactly on a multiple: ticks are not consecutive — a pass slower
/// than a ledger close skips sequences, and a declined tick skips them too —
/// so an exact test silently drops a whole period, taking that period's
/// reseed retry with it.
fn scan_due(ledger: u32, last: Option<u32>, phase: u32, period: u32) -> bool {
    if period == 0 {
        return false;
    }
    let bucket = |ledger: u32| ledger.wrapping_add(phase) / period;
    match last {
        None => true,
        Some(last) => bucket(ledger) > bucket(last),
    }
}

/// What the tracker loop carries between messages, all of it keyed by
/// pool: two pools share one channel and must not see each other's state.
#[derive(Debug, Default)]
struct LoopState {
    /// The accounts each pool's events have named since that pool's last
    /// tick, refreshed in one batched read when it arrives.
    pending: BTreeMap<String, BTreeSet<String>>,
    /// Pools an event failed to apply for since their last tick. Their
    /// next tick declines its acknowledgement, so the poller leaves the
    /// cursor where it is and the range — events and tick alike — is read
    /// again rather than committed over a gap in the store.
    unapplied: BTreeSet<String>,
    /// Pools whose last seed could not reach every source. Spec §4 retries
    /// such a seed on the next full scan, which is slow on purpose: the
    /// alternative is walking a third-party API once per poll interval.
    needs_reseed: BTreeSet<String>,
    /// The ledger each pool's full scan last fired at, so the cadence fires
    /// once per period rather than only on a tick that lands exactly on a
    /// multiple of it.
    last_scan: BTreeMap<String, u32>,
}

/// Applies one message from the shared poller channel: an event accumulates
/// its accounts for the pool's next tick, a tick refreshes what accumulated
/// plus a stale-refresh pass and, on the scan cadence, logs the least
/// healthy borrowers and the tracked count, and a gap reseeds the pool.
///
/// A tick is acknowledged only once its whole effect is in the store,
/// because that acknowledgement is what commits the poller's cursor.
async fn handle_message(
    tracker: &Tracker<'_>,
    seed_sources: &[SeedSource],
    cadence: Cadence,
    state: &mut LoopState,
    shutdown: &watch::Receiver<bool>,
    tick_tx: &watch::Sender<LedgerTick>,
    message: PollerMessage,
) -> Result<(), TrackerError> {
    match message {
        PollerMessage::Event {
            pool,
            ledger,
            event,
        } => match tracker.apply(&pool, ledger, &event).await {
            Ok(accounts) => state.pending.entry(pool).or_default().extend(accounts),
            Err(error) => {
                // This event is not in the store, so the range it came
                // from must be read again: poison the pool's next tick so
                // the cursor is not committed over it.
                state.unapplied.insert(pool);
                return Err(error);
            }
        },
        PollerMessage::Tick { pool, tick, ack } => {
            let accounts: Vec<String> = state.pending.remove(&pool).into_iter().flatten().collect();
            let unapplied = state.unapplied.remove(&pool);
            if let Err(error) = apply_tick(
                tracker,
                seed_sources,
                cadence,
                state,
                shutdown,
                (&pool, &accounts, tick),
            )
            .await
            {
                // Put back what this tick took, so §8's "the next event or
                // refresh pass retries" is true rather than aspirational.
                // Dropping `ack` unanswered is how the poller learns this
                // ledger was not applied and must be delivered again.
                state.pending.entry(pool).or_default().extend(accounts);
                return Err(error);
            }
            if unapplied {
                tracing::warn!(
                    pool,
                    ledger = tick.sequence,
                    "an event in this range did not apply; leaving the range to be re-read"
                );
            } else {
                // Answering is the only thing that lets the cursor move.
                let _ = ack.send(());
                // Published only now: the auctioneer's whole input is
                // downstream of "this ledger's effects are in the store",
                // never upstream of it. A dropped receiver — the
                // auctioneer task has exited, or this is a test driving
                // `handle_message` with no receiver held — is not an
                // error: the tracker never waits on anyone listening.
                // "No auctioneer configured" is not one of the cases:
                // `Service::run` spawns it unconditionally.
                let _ = tick_tx.send(tick);
            }
        }
        PollerMessage::Gap { pool, from, oldest } => {
            tracing::warn!(pool, from, oldest, "reseeding after a gap");
            // Recorded before the attempt and cleared only by a seed that
            // reached every source, so a reseed that fails outright is
            // retried by the full scan rather than forgotten.
            state.needs_reseed.insert(pool.clone());
            let head = tracker.rpc().latest_ledger().await?;
            let tick = LedgerTick {
                sequence: head.sequence,
                close_time: head.close_time,
            };
            let outcome = tracker
                .seed(&pool, seed_sources, tick, cadence.refresh_batch, shutdown)
                .await?;
            if outcome.failed_sources == 0 {
                state.needs_reseed.remove(&pool);
            }
        }
    }
    Ok(())
}

/// A tick's whole effect: refresh the accounts this ledger's events named,
/// then the stale-refresh pass, then the full scan when it is due.
///
/// The first two must succeed for the tick to be acknowledged: they are
/// what "this ledger's effects are in the store" means, and the poller must
/// not commit a cursor past a ledger whose accounts were never re-valued.
/// The full scan is not one of those effects — it reports the least healthy
/// borrowers and retries an owed reseed — so it may fail without failing the
/// tick, and only a store error, which means the bot cannot trust what it
/// reads at all, still propagates.
///
/// `subject` is the pool, the accounts its events named, and the tick.
async fn apply_tick(
    tracker: &Tracker<'_>,
    seed_sources: &[SeedSource],
    cadence: Cadence,
    state: &mut LoopState,
    shutdown: &watch::Receiver<bool>,
    subject: (&str, &[String], LedgerTick),
) -> Result<(), TrackerError> {
    let (pool, accounts, tick) = subject;
    tracker.refresh(pool, accounts, tick).await?;
    // Flags every account this ledger's events named, for the auctioneer
    // task to pick up on its own cadence. This is the durable replacement
    // for `pending` as the auctioneer's input: `pending` itself stays
    // exactly what it was above — this tick's own batch, consumed by the
    // refresh it just fed — but a flag outlives this tick, which is the
    // whole point. Flagging an account the refresh just deleted (a closed
    // position) is harmless: `flag_recheck` is an `UPDATE` keyed by
    // `(pool, account)`, so it silently matches no row.
    for account in accounts {
        tracker
            .store()
            .flag_recheck(pool, account, tick.sequence)
            .await?;
    }
    // `USER_REFRESH_LEDGERS` is a *span*; `Store::users_stale` selects on
    // an absolute ledger. Subtracting here is what turns one into the
    // other: handing the span over as-is compares a ledger count against a
    // ledger sequence, which on any real network is false for every row.
    let updated_before = tick.sequence.saturating_sub(cadence.user_refresh_ledgers);
    // Inlines what `Tracker::refresh_stale` does — read the stale rows,
    // then refresh them — because this call site also needs the account
    // list to flag, and `RefreshOutcome` reports only counts.
    let stale_rows = tracker
        .store()
        .users_stale(pool, updated_before, i64::from(cadence.refresh_batch))
        .await?;
    let stale_accounts: Vec<String> = stale_rows.into_iter().map(|user| user.account).collect();
    tracker.refresh(pool, &stale_accounts, tick).await?;
    for account in &stale_accounts {
        tracker
            .store()
            .flag_recheck(pool, account, tick.sequence)
            .await?;
    }
    if scan_due(
        tick.sequence,
        state.last_scan.get(pool).copied(),
        cadence.phase,
        cadence.full_scan_ledgers,
    ) {
        // A failed scan does not fail the tick. The acknowledgement means
        // "this ledger's effects are in the store", and neither the
        // least-healthy report nor a best-effort reseed retry is one of
        // them. Declining the tick over one would stall the pool's cursor
        // for good when the failure is deterministic rather than transient
        // — a borrower the seed names who holds a reserve the oracle does
        // not price is exactly that, and `validate` deliberately lets the
        // bot follow such a pool. A store failure still propagates: it is
        // the one class that means the bot cannot trust what it reads.
        if let Err(error) = full_scan(
            tracker,
            seed_sources,
            cadence,
            state,
            shutdown,
            (pool, tick),
        )
        .await
        {
            if matches!(error, TrackerError::Store(_)) {
                return Err(error);
            }
            tracing::warn!(pool, ledger = tick.sequence, %error, "the full scan failed; it runs again next period");
        }
        // The period is recorded either way: by a scan that ran, or by one
        // that tried and failed. `needs_reseed` still holds the pool, so
        // the retry recurs next period rather than being lost — but the
        // cadence cannot spin on the same failure.
        state.last_scan.insert(pool.to_owned(), tick.sequence);
    }
    Ok(())
}

/// The full scan: report the least healthy borrowers and the tracked count,
/// and retry a seed that could not reach every source.
///
/// `subject` is the pool and the tick the scan fired on.
async fn full_scan(
    tracker: &Tracker<'_>,
    seed_sources: &[SeedSource],
    cadence: Cadence,
    state: &mut LoopState,
    shutdown: &watch::Receiver<bool>,
    subject: (&str, LedgerTick),
) -> Result<(), TrackerError> {
    let (pool, tick) = subject;
    let store = tracker.store();
    let least_healthy = store
        .users_below_health(
            pool,
            cadence.scan_health_factor,
            FULL_SCAN_REPORT_LIMIT,
            None,
        )
        .await?;
    let user_count = store.count_users(pool).await?;
    tracing::info!(
        pool,
        user_count,
        least_healthy = least_healthy.len(),
        "full scan"
    );
    for user in &least_healthy {
        tracing::info!(
            pool,
            account = user.account,
            health_factor = user.health_factor,
            "tracked borrower"
        );
    }
    // Spec §4: a failed seed "is retried on the next full scan". Without
    // this a source that was down at startup costs coverage for the life
    // of the process.
    if state.needs_reseed.contains(pool) {
        let outcome = tracker
            .seed(pool, seed_sources, tick, cadence.refresh_batch, shutdown)
            .await?;
        tracing::info!(
            pool,
            tracked = outcome.refresh.tracked,
            failed_sources = outcome.failed_sources,
            "retried an incomplete seed"
        );
        if outcome.failed_sources == 0 {
            state.needs_reseed.remove(pool);
        }
    }
    Ok(())
}

/// Consumes the shared poller channel until every poller's sender clone has
/// dropped, applying each message in order. Draining rather than watching
/// the shutdown flag itself is deliberate: a poller only stops after its
/// own `run` returns, and by then everything it sent is already in the
/// channel, so letting `recv` return `None` naturally applies a ledger that
/// was already read before this task returns.
///
/// `tick_tx` is owned, not borrowed: this is the one place that publishes
/// on it, always after `handle_message` has acknowledged the ledger it
/// carries (see that function's `Tick` arm), and dropping it when this
/// function returns is what lets the auctioneer task's own `changed()`
/// end rather than wait forever once this loop has nothing further to
/// send.
async fn tracker_loop(
    tracker: &Tracker<'_>,
    seed_sources: &[SeedSource],
    cadence: Cadence,
    mut state: LoopState,
    shutdown: &watch::Receiver<bool>,
    tick_tx: watch::Sender<LedgerTick>,
    mut receiver: mpsc::Receiver<PollerMessage>,
) -> Result<(), TrackerError> {
    while let Some(message) = receiver.recv().await {
        // `handle_message` consumes `message`, so the pool (and, for the
        // variants that name a single one, the ledger) must be read out
        // before the call — otherwise a warn logged after it returns has
        // no way to say which pool or ledger failed. `Gap` is about a
        // range rather than one ledger, so it names no `ledger` here.
        let (pool, ledger) = match &message {
            PollerMessage::Event { pool, ledger, .. } => (pool.clone(), Some(*ledger)),
            PollerMessage::Tick { pool, tick, .. } => (pool.clone(), Some(tick.sequence)),
            PollerMessage::Gap { pool, .. } => (pool.clone(), None),
        };
        match handle_message(
            tracker,
            seed_sources,
            cadence,
            &mut state,
            shutdown,
            &tick_tx,
            message,
        )
        .await
        {
            Ok(()) => {}
            // The two classes are treated differently on purpose. A store
            // failure is fatal: §8 wants a store outage to pause rather
            // than trade on stale state, and every retry path here writes
            // through the store that just failed, so carrying on would
            // build a user set and a cursor nobody can trust. A chain or
            // math failure is the transient case §8 describes — an RPC
            // blip, a reserve that moved under the read — where "a failed
            // user refresh logs and leaves the row untouched; the next
            // event or refresh pass retries" is the whole remedy: the tick
            // went unacknowledged, so the poller re-reads the same range,
            // and applying it again converges because every write is an
            // upsert or a delete keyed by what the chain says.
            Err(error @ TrackerError::Store(_)) => return Err(error),
            Err(error) => tracing::warn!(
                pool,
                ledger,
                %error,
                "applying a poller message failed; the range will be read again"
            ),
        }
    }
    Ok(())
}

/// How long a price reference may stand with no significant move before
/// the oracle scan refreshes it anyway (spec §4's cadence table: "refresh
/// the reference price after a day without a significant move").
const PRICE_REFERENCE_STALE_AFTER_SECS: u64 = 86_400;

/// The submission queue's backlog bound. It bounds how many decided
/// creations may wait for their turn before `enqueue` applies
/// backpressure to the auctioneer loop that calls it. An ordinary pass
/// never reaches the bound whatever `REFRESH_BATCH` is set to — not
/// because 64 is larger than a batch (it need not be), but because
/// `SubmissionQueue::enqueue` awaits its own outcome before
/// [`recheck_batch`] moves to the next borrower, so this bot never has
/// more than one creation in flight. The slack is for a future caller
/// that enqueues without awaiting, not for a batch.
const SUBMISSION_QUEUE_CAPACITY: usize = 64;

/// Timings and thresholds the auctioneer task reads every tick, bundled
/// the same way [`Cadence`] bundles the tracker's. `oracle_phase` and
/// `full_phase` are each drawn once at startup via [`scan_phase`],
/// independently of the tracker's own full-scan phase: the two full-scan
/// cadences serve different jobs on the same knobs — the tracker's reports
/// the least healthy borrowers and retries an owed reseed, this one flags
/// every borrower below the threshold for a decision — so each reuses
/// [`scan_phase`]'s randomisation rather than sharing one draw.
#[derive(Debug, Clone, Copy)]
struct AuctioneerCadence {
    /// How many flagged users to decide and act on per pool per tick, and
    /// the page size the full scan flags with. Never the oracle scan's
    /// bound: that scan flags every exposed borrower, per
    /// [`Auctioneer::scan_oracle`]'s own doc.
    refresh_batch: u32,
    /// How often, in ledgers, prices are re-read for a significant move.
    oracle_scan_ledgers: u32,
    /// This instance's phase in the oracle-scan cadence.
    oracle_phase: u32,
    /// How often every user below `scan_health_factor` is (re)flagged.
    full_scan_ledgers: u32,
    /// This instance's phase in the full-scan cadence.
    full_phase: u32,
    /// The health factor the full scan flags below, 7 decimals.
    scan_health_factor: i128,
    /// Basis points a price must move before the oracle scan reports it.
    price_delta_bps: u32,
    /// Ledgers of chain advance, measured from the first tick this task
    /// observes, before any submission is attempted even when armed.
    startup_delay_ledgers: u32,
}

/// Reads [`AuctioneerCadence`] out of the resolved configuration, drawing
/// this instance's oracle-scan and full-scan phases once, independently of
/// each other and of the tracker's own full-scan phase — see the struct's
/// own doc for why the two full-scan cadences on the same knobs never
/// share a draw.
fn auctioneer_cadence_from(config: &ServiceConfig) -> AuctioneerCadence {
    AuctioneerCadence {
        refresh_batch: config.refresh_batch,
        oracle_scan_ledgers: config.oracle_scan_ledgers,
        oracle_phase: scan_phase(config.oracle_scan_ledgers),
        full_scan_ledgers: config.full_scan_ledgers,
        full_phase: scan_phase(config.full_scan_ledgers),
        scan_health_factor: config.scan_health_factor,
        price_delta_bps: config.price_delta_bps,
        startup_delay_ledgers: config.startup_delay_ledgers,
    }
}

/// Pages every one of `pool`'s borrowers below `threshold` and flags each
/// for an auctioneer decision. This is the full scan's whole job — making
/// sure nothing below the threshold stays un-flagged — never to decide
/// inline: [`Auctioneer::decide`] still does that, on the ordinary recheck
/// path this feeds.
///
/// Pages by the keyset [`Store::users_below_health`] returns rather than
/// reading one page: a borrower who only shows up on the second page is
/// exactly the one a single-page scan would miss, and it is the one this
/// scan exists to catch.
///
/// `shutdown` is observed between pages, never inside one: a pool with
/// thousands of borrowers below the threshold would otherwise page and
/// flag every one of them before the next shutdown check, and a flag is
/// worth nothing to a process that is exiting. Stopping early is safe
/// because the scan is a safety net, not a ledger effect — the next
/// process's own full scan flags whatever this one did not reach.
async fn full_scan_and_flag(
    store: &Store,
    pool: &str,
    threshold: i128,
    ledger: u32,
    page_limit: i64,
    shutdown: &watch::Receiver<bool>,
) -> Result<usize, StoreError> {
    let mut flagged = 0_usize;
    let mut after: Option<(i128, String)> = None;
    loop {
        if *shutdown.borrow() {
            return Ok(flagged);
        }
        let page = store
            .users_below_health(
                pool,
                threshold,
                page_limit,
                after
                    .as_ref()
                    .map(|(health, account)| (*health, account.as_str())),
            )
            .await?;
        let Some(last) = page.last() else {
            break;
        };
        after = Some((last.health_factor, last.account.clone()));
        for user in &page {
            store.flag_recheck(pool, &user.account, ledger).await?;
        }
        flagged += page.len();
    }
    Ok(flagged)
}

/// Decides and acts on one already-read batch of flagged users, clearing
/// each flag with the ledger the batch read it at — **never** `tick`'s own
/// ledger, which is not the same thing: [`Store::clear_recheck`] is
/// conditional on the exact value [`Store::users_needing_recheck`] read,
/// so a flag raised again while this batch was deciding is left standing
/// for the next pass rather than cleared by a decision that never saw it.
///
/// A per-borrower failure — a borrower [`Auctioneer::decide`] left out of
/// its result, one whose [`Auctioneer::act`] returned anything but a
/// store error, and one whose `act` came back
/// [`crate::auctioneer::ActOutcome::Refused`] — is logged with the account
/// and keeps its flag, and that flag is **moved forward**: re-raised at
/// this tick's ledger, or left at the batch's own if that is the newer of
/// the two (the exact rule, and why, are in [`move_flag_forward`]).
/// Leaving it where it was is what starves a pool:
/// [`Store::users_needing_recheck`] orders `recheck_ledger ASC, account
/// ASC`, so an untouched flag is the oldest in its pool and comes back at
/// the head of every following batch. A borrower nothing can decide is
/// not hypothetical — an oracle that stops pricing one reserve makes
/// `position_data` fail for every borrower holding it at once, and
/// `validate` deliberately keeps the bot following a pool with an
/// unpriced reserve — so `REFRESH_BATCH` such borrowers would fill every
/// batch for ever and no other borrower in that pool would be decided
/// again, with nothing but a repeated per-user warning to show for it.
///
/// Moving the flag forward cannot itself livelock. The row is written at
/// the later of `tick`'s ledger and the ledger the batch read it at, so
/// every borrower flagged before this tick sorts strictly ahead of it:
/// each pass serves `REFRESH_BATCH` borrowers none of which it has
/// already tried this cycle, and a queue of `n` is fully served in
/// `n / REFRESH_BATCH` passes however many of them keep failing. The flag
/// is never dropped, so the failing borrower is retried too, just behind
/// everyone else rather than in front of them.
///
/// A *refusal* is the case that is easy to mistake for success. `act`
/// answers `Refused` when it wanted to act and could not — the contract
/// said no, the percent walk exhausted its iterations, or the footprint
/// needs a restore — and `Skipped` when nothing was owed at all. Only the
/// second clears the flag: a refused borrower is one this bot believes is
/// liquidatable, so dropping it from the queue would hide it until an
/// event, a price move or the full scan's own period named it again. The
/// retry is bounded for the same reason an undecidable borrower's is: the
/// flag moves to this tick's ledger and everything flagged earlier sorts
/// ahead of it.
///
/// A whole-batch failure is different and is left alone: when `decide`
/// itself returns `Err` — the one snapshot it reads for the batch — no
/// borrower was served ahead of any other, there is no queue position to
/// correct, and the next pass reads the same batch again.
///
/// Isolating a per-borrower failure is this function's job, per `act`'s
/// own doc. Only [`AuctioneerError::Store`] ends the pass early and
/// propagates, for the reason it is fatal everywhere else in this module:
/// the bot cannot trust what it reads.
async fn recheck_batch(
    auctioneer: &Auctioneer<'_>,
    store: &Store,
    pool: &str,
    batch: &[TrackedUser],
    tick: LedgerTick,
    submit: Option<&SubmissionQueue>,
    shutdown: &watch::Receiver<bool>,
) -> Result<(), LiquidatorError> {
    if batch.is_empty() {
        return Ok(());
    }
    let flagged_at: BTreeMap<&str, u32> = batch
        .iter()
        .filter_map(|user| {
            user.recheck_ledger
                .map(|ledger| (user.account.as_str(), ledger))
        })
        .collect();
    let decisions = match auctioneer.decide(pool, batch, tick).await {
        Ok(decisions) => decisions,
        Err(AuctioneerError::Store(error)) => return Err(LiquidatorError::Store(error)),
        Err(error) => {
            tracing::warn!(
                pool,
                %error,
                "deciding this pool's recheck batch failed; every flag in it stays set for the next pass"
            );
            return Ok(());
        }
    };
    // The borrowers `decide` skipped: it logs each one's own failure and
    // leaves it out of the result. Moved forward before anything is acted
    // on, so the shutdown check below cannot leave them holding the
    // oldest flags in the pool either.
    let mut undecided: BTreeSet<&str> = batch.iter().map(|user| user.account.as_str()).collect();
    for (account, _) in &decisions {
        undecided.remove(account.as_str());
    }
    for account in undecided {
        tracing::debug!(
            pool,
            account,
            ledger = tick.sequence,
            "no decision for this borrower; re-flagged at this ledger so the queue \
             behind it is not held up"
        );
        move_flag_forward(store, pool, account, flagged_at.get(account).copied(), tick).await?;
    }
    for (account, decision) in decisions {
        // Checked between users, never inside a submission: a submission
        // already in flight is waited for, because abandoning it would
        // leave a signing key's sequence number consumed by something
        // this bot never saw the outcome of.
        if *shutdown.borrow() {
            return Ok(());
        }
        match auctioneer
            .act(pool, &account, &decision, tick, submit)
            .await
        {
            // A refusal is not a skip: something was owed and could not be
            // done, so the flag moves forward instead of being cleared —
            // exactly what an undecidable borrower's does, and bounded the
            // same way. Clearing it would drop a borrower this bot believes
            // is liquidatable out of the recheck queue until an event, a
            // price move or the full scan's ~1200-ledger period named it
            // again. `ActOutcome::settled` is where that rule lives.
            Ok(outcome) if !outcome.settled() => {
                tracing::debug!(
                    pool,
                    account,
                    ledger = tick.sequence,
                    "this borrower was owed an action that could not be made; re-flagged \
                     at this ledger for a later pass"
                );
                move_flag_forward(
                    store,
                    pool,
                    &account,
                    flagged_at.get(account.as_str()).copied(),
                    tick,
                )
                .await?;
            }
            Ok(_settled) => {
                if let Some(&flagged_at) = flagged_at.get(account.as_str()) {
                    match store.clear_recheck(pool, &account, flagged_at).await {
                        Ok(true) => {}
                        Ok(false) => tracing::debug!(
                            pool,
                            account,
                            "the flag was raised again while this was deciding; leaving it \
                             for the next pass"
                        ),
                        Err(error) => return Err(LiquidatorError::Store(error)),
                    }
                }
            }
            Err(AuctioneerError::Store(error)) => return Err(LiquidatorError::Store(error)),
            Err(error) => {
                tracing::warn!(
                    pool,
                    account,
                    %error,
                    ledger = tick.sequence,
                    "acting on this borrower failed; re-flagged at this ledger for a \
                     later pass"
                );
                move_flag_forward(
                    store,
                    pool,
                    &account,
                    flagged_at.get(account.as_str()).copied(),
                    tick,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Re-raises one borrower's recheck flag at `tick`'s ledger, so a
/// borrower this pass could not decide or act on is retried on a later
/// pass instead of holding the oldest flag in its pool — see
/// [`recheck_batch`]'s doc for why that ordering is the whole point.
///
/// `flagged_at` is the ledger the batch read the flag at, fixed before
/// `decide` and `act` ran. When it is newer than `tick` — the auctioneer
/// runs behind the tracker, so a pool can have flagged a borrower at a
/// ledger this pass has not been told about yet — it is kept, because
/// writing `tick`'s older ledger would move the row *towards* the head of
/// the queue, which is the direction this exists to prevent. Taking the
/// later of the two here is belt-and-braces, not the guarantee: `act` can
/// span several real ledger closes, so by the time this runs the
/// independently scheduled tracker may already have flagged the same
/// account at a ledger newer than both `flagged_at` and `tick`. It is
/// [`Store::flag_recheck`] — the only place that writes the column — that
/// actually makes the flag monotonic, by taking the `GREATEST` of the
/// stored value and what is written here.
async fn move_flag_forward(
    store: &Store,
    pool: &str,
    account: &str,
    flagged_at: Option<u32>,
    tick: LedgerTick,
) -> Result<(), LiquidatorError> {
    let ledger = flagged_at.map_or(tick.sequence, |flagged| flagged.max(tick.sequence));
    store
        .flag_recheck(pool, account, ledger)
        .await
        .map_err(LiquidatorError::Store)
}

/// Mutable state the auctioneer task carries from one tick to the next:
/// where the startup delay is measured from and whether it has elapsed,
/// and each pool's oracle-scan and full-scan cadence state. Bundled into
/// one struct, rather than five `&mut` parameters on [`auctioneer_tick`],
/// for the same reason [`LoopState`] exists for the tracker.
#[derive(Debug, Default)]
struct AuctioneerState {
    /// Each pool's oracle-scan reference prices.
    price_watches: BTreeMap<String, PriceWatch>,
    /// The ledger each pool's oracle scan last fired at.
    last_oracle_scan: BTreeMap<String, u32>,
    /// The ledger each pool's full scan last fired at.
    last_full_scan: BTreeMap<String, u32>,
    /// The ledger of the first tick this task observed, which the startup
    /// delay is measured from. `None` until that first tick: there is no
    /// meaningful "how far has the chain moved" before one has arrived.
    ///
    /// The ledger, deliberately, and not a count of ticks: every pool's
    /// poller publishes to the one watch this task reads, so with `n`
    /// pools a count would reach `STARTUP_DELAY_LEDGERS` in roughly
    /// `STARTUP_DELAY_LEDGERS / n` ledgers — a safety knob quietly
    /// under-delivering by the pool count, in the direction of arming
    /// sooner — while a pass slow enough to coalesce two wakeups into one
    /// would push the same count the other way.
    first_tick_ledger: Option<u32>,
    /// Whether the chain has moved `startup_delay_ledgers` past
    /// `first_tick_ledger`. Latches `true` and stays there — the delay is
    /// measured from startup, never re-armed — so the "submissions are
    /// now possible" log line fires at most once.
    submissions_unlocked: bool,
}

/// What one auctioneer task holds for its whole life: the pieces
/// `auctioneer_tick` needs but never mutates, bundled so that function
/// takes a context and the two things that actually change tick to tick
/// (`tick` itself and `&mut AuctioneerState`) rather than seven-plus
/// parameters.
struct AuctioneerContext<'a> {
    store: &'a Store,
    pools: &'a [String],
    auctioneer: &'a Auctioneer<'a>,
    cadence: AuctioneerCadence,
    submission_queue: Option<&'a SubmissionQueue>,
    shutdown: &'a watch::Receiver<bool>,
}

/// One tick's whole effect for every configured pool: decide and act on
/// each pool's currently flagged users, and fire the oracle-scan and
/// full-scan-and-flag cadences when due.
///
/// `state.submissions_unlocked` gates whether `ctx.submission_queue` is
/// actually handed to [`recheck_batch`] (`None` until the chain has moved
/// `startup_delay_ledgers` past the first tick this task saw, regardless
/// of whether the queue itself exists — i.e. regardless of dry-run or
/// armed) or passed through unchanged after it has. The elapsed distance
/// is a `saturating_sub` because ticks are not monotonic across pools: a
/// pool whose poller is a ledger or two behind publishes a tick older
/// than the first one seen, and `u32 - u32` going negative is a panic in
/// a debug build. Saturating reads that as "no ledgers have elapsed yet",
/// which keeps submissions locked — the safe direction.
///
/// A [`StoreError`] is fatal, exactly as it is in [`tracker_loop`]:
/// without a trustworthy store there is no way to know who is flagged or
/// to record a decision, so nothing downstream is safe to act on. Every
/// other failure — deciding or acting on one borrower, a failed oracle or
/// full scan — is logged with the pool or account and this pass carries
/// on to the next pool; see [`recheck_batch`] and this function's own
/// match arms for where each is isolated.
async fn auctioneer_tick(
    ctx: &AuctioneerContext<'_>,
    tick: LedgerTick,
    state: &mut AuctioneerState,
) -> Result<(), LiquidatorError> {
    let first_ledger = *state.first_tick_ledger.get_or_insert(tick.sequence);
    let elapsed = tick.sequence.saturating_sub(first_ledger);
    if !state.submissions_unlocked && elapsed >= ctx.cadence.startup_delay_ledgers {
        state.submissions_unlocked = true;
        tracing::info!(
            ledger = tick.sequence,
            first_ledger,
            elapsed,
            "the startup delay has elapsed; submissions are now possible"
        );
    }
    let submit = if state.submissions_unlocked {
        ctx.submission_queue
    } else {
        None
    };

    for pool in ctx.pools {
        if *ctx.shutdown.borrow() {
            return Ok(());
        }

        if scan_due(
            tick.sequence,
            state.last_oracle_scan.get(pool).copied(),
            ctx.cadence.oracle_phase,
            ctx.cadence.oracle_scan_ledgers,
        ) {
            let watch = state.price_watches.entry(pool.clone()).or_insert_with(|| {
                PriceWatch::new(
                    ctx.cadence.price_delta_bps,
                    PRICE_REFERENCE_STALE_AFTER_SECS,
                )
            });
            match ctx.auctioneer.scan_oracle(pool, watch, tick).await {
                Ok(flagged) => tracing::info!(pool, flagged, "oracle scan"),
                Err(AuctioneerError::Store(error)) => return Err(LiquidatorError::Store(error)),
                Err(error) => tracing::warn!(
                    pool,
                    %error,
                    "the oracle scan failed; it runs again next period"
                ),
            }
            state.last_oracle_scan.insert(pool.clone(), tick.sequence);
        }

        if scan_due(
            tick.sequence,
            state.last_full_scan.get(pool).copied(),
            ctx.cadence.full_phase,
            ctx.cadence.full_scan_ledgers,
        ) {
            match full_scan_and_flag(
                ctx.store,
                pool,
                ctx.cadence.scan_health_factor,
                tick.sequence,
                i64::from(ctx.cadence.refresh_batch),
                ctx.shutdown,
            )
            .await
            {
                Ok(flagged) => tracing::info!(
                    pool,
                    flagged,
                    "full scan flagged every user below the threshold"
                ),
                Err(error) => return Err(LiquidatorError::Store(error)),
            }
            state.last_full_scan.insert(pool.clone(), tick.sequence);
        }

        let batch = match ctx
            .store
            .users_needing_recheck(pool, i64::from(ctx.cadence.refresh_batch))
            .await
        {
            Ok(batch) => batch,
            Err(error) => return Err(LiquidatorError::Store(error)),
        };
        recheck_batch(
            ctx.auctioneer,
            ctx.store,
            pool,
            &batch,
            tick,
            submit,
            ctx.shutdown,
        )
        .await?;
    }
    Ok(())
}

/// Runs [`auctioneer_tick`] off the tick the tracker publishes after it
/// has acknowledged one.
///
/// Fed by a `watch`, never the poller channel the cursor rests on (see the
/// module doc's wiring note): nothing in this function is upstream of a
/// tick's acknowledgement, so a slow or failing pass here falls behind the
/// newest ledger and never blocks, delays or fails it. `changed()`
/// returning an error means every sender has dropped — the tracker task is
/// gone — and this loop then has nothing further to do. A decision is not
/// a stored effect of a ledger, so nothing this loop does ever reaches
/// back to poison a tick already acknowledged.
async fn auctioneer_loop(
    store: &Store,
    pools: &[String],
    auctioneer: &Auctioneer<'_>,
    cadence: AuctioneerCadence,
    submission_queue: Option<&SubmissionQueue>,
    mut tick_rx: watch::Receiver<LedgerTick>,
    shutdown: &watch::Receiver<bool>,
) -> Result<(), LiquidatorError> {
    let ctx = AuctioneerContext {
        store,
        pools,
        auctioneer,
        cadence,
        submission_queue,
        shutdown,
    };
    let mut state = AuctioneerState::default();
    while tick_rx.changed().await.is_ok() {
        if *shutdown.borrow() {
            return Ok(());
        }
        let tick = *tick_rx.borrow_and_update();
        auctioneer_tick(&ctx, tick, &mut state).await?;
    }
    Ok(())
}

/// Waits for one shutdown request: `ctrl_c`, or on Unix `SIGTERM` too.
/// Neither failing to install `SIGTERM`'s handler nor `ctrl_c` itself
/// erroring is treated as a shutdown: both are exceedingly rare, and the
/// safe fallback is to keep the bot running rather than exit on a spurious
/// signal-handling failure.
#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(error) => {
            tracing::warn!(%error, "could not install a SIGTERM handler; watching ctrl_c only");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// The non-Unix fallback: `ctrl_c` only, since `SIGTERM` does not exist.
#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Propagates a spawned task's panic into this task rather than wedging it
/// into [`LiquidatorError`]'s taxonomy: a panic is an internal bug, not one
/// of the phases that enum distinguishes, and swallowing it into, say,
/// `Config` would misreport a bug as a bad configuration.
fn resume_on_panic(error: tokio::task::JoinError) -> ! {
    match error.try_into_panic() {
        Ok(payload) => std::panic::resume_unwind(payload),
        Err(cancelled) => {
            // Nothing in this service ever calls `.abort()`, so a task
            // ending cancelled rather than panicked never happens in
            // practice; if it ever does, say so loudly rather than return
            // successfully as if the task had finished its work.
            panic!("a service task was cancelled unexpectedly: {cancelled}")
        }
    }
}

/// Waits for the first shutdown signal, flips `shutdown` so every task
/// still running finishes and returns, then waits for a second signal and
/// exits immediately with code 130 — the shell's convention for "killed by
/// `SIGINT`" — rather than wait any further for a graceful shutdown that is
/// apparently not coming.
///
/// The sender is shared rather than owned: [`Service::run`]'s join loop
/// raises the same flag when a task returns an error, so that a failure and
/// a signal drain the remaining tasks by exactly the same route.
fn spawn_shutdown_listener(shutdown: Arc<watch::Sender<bool>>) {
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::warn!("shutdown requested; finishing in-flight work");
        let _ = shutdown.send(true);
        wait_for_signal().await;
        tracing::warn!("second shutdown request; exiting immediately");
        std::process::exit(130);
    });
}

/// What signs a transaction, shared by the submission queue's own task and
/// the auctioneer's — whenever a key is configured at all, dry-run
/// included, since `Auctioneer` still simulates with no intent to submit
/// (see its own doc). `Signer` is deliberately not `Clone` — it holds key
/// material — so an `Arc` is what lets both tasks borrow the one key
/// without duplicating it in memory.
///
/// `own_addresses` is computed from **every** key the process was given,
/// not from the one that signs: with `AUCTIONEER_SECRET_KEY` and
/// `FILLER_SECRET_KEY` set to different keys only the auctioneer's signs,
/// and deriving the set from `signer` alone would leave the filler's own
/// position a borrower the auctioneer is willing to liquidate. It is
/// therefore taken from [`SigningKeys::own_addresses`] before the choice of
/// signer collapses the two.
struct SigningContext {
    network: Network,
    tx_config: TxConfig,
    signer: Option<Arc<Signer>>,
    own_addresses: BTreeSet<String>,
}

impl SigningContext {
    fn from_config(config: &ServiceConfig, keys: SigningKeys) -> Self {
        let own_addresses = keys.own_addresses();
        Self {
            network: Network::from_config(&config.chain),
            tx_config: TxConfig::from_config(&config.chain),
            signer: keys.into_auctioneer_signer().map(Arc::new),
            own_addresses,
        }
    }

    /// Every account this bot holds a key for, the filler's included —
    /// empty when no key is configured at all, which excludes nothing
    /// rather than everything.
    fn own_addresses(&self) -> BTreeSet<String> {
        self.own_addresses.clone()
    }
}

/// Spawns one [`LedgerPoller`] per pool, all sending into `sender`, and
/// drops the caller's own clone once every one holds its own — which is
/// what lets the tracker task's channel close, and its `recv` return
/// `None`, once (and only once) every poller has stopped.
fn spawn_pollers(
    tasks: &mut JoinSet<Result<(), LiquidatorError>>,
    rpc: &RpcClient,
    store: &Store,
    pools: &[PoolConfig],
    poller_config: PollerConfig,
    sender: &mpsc::Sender<PollerMessage>,
    shutdown: &watch::Receiver<bool>,
) {
    for pool in pools {
        let rpc = rpc.clone();
        let store = store.clone();
        let pool = pool.address.clone();
        let sender = sender.clone();
        let shutdown = shutdown.clone();
        tasks.spawn(async move {
            LedgerPoller::new(&rpc, &store, &pool, poller_config)
                .run(sender, shutdown)
                .await
                .map_err(LiquidatorError::from)
        });
    }
}

/// Builds and spawns the submission queue's worker when armed — `!dry_run`
/// and a signer configured, the one gate into live trading `DRY_RUN`'s
/// default makes safe — and returns the queue handle for the auctioneer to
/// submit through. `None` otherwise: the ordinary dry-run deployment, or a
/// live one with no key to sign with.
fn spawn_submission_queue(
    tasks: &mut JoinSet<Result<(), LiquidatorError>>,
    rpc: &RpcClient,
    signing: &SigningContext,
    dry_run: bool,
    shutdown: &watch::Receiver<bool>,
) -> Option<SubmissionQueue> {
    let (false, Some(signer)) = (dry_run, signing.signer.as_ref()) else {
        return None;
    };
    let signer = Arc::clone(signer);
    let rpc = rpc.clone();
    let network = signing.network.clone();
    let tx_config = signing.tx_config;
    let shutdown = shutdown.clone();
    let (queue, queue_rx) = SubmissionQueue::new(SUBMISSION_QUEUE_CAPACITY);
    tasks.spawn(async move {
        let submitter = Submitter::new(&rpc, &network, &signer, tx_config);
        run_queue(&submitter, queue_rx, &shutdown).await;
        Ok(())
    });
    Some(queue)
}

/// Spawns the auctioneer task: builds its own [`Auctioneer`] — with a
/// [`Submitter`] whenever a key is configured at all, dry-run included —
/// and runs [`auctioneer_loop`] off `tick_rx` until the tracker task's
/// sender drops.
#[allow(clippy::too_many_arguments)]
fn spawn_auctioneer(
    tasks: &mut JoinSet<Result<(), LiquidatorError>>,
    rpc: &RpcClient,
    store: &Store,
    signing: &SigningContext,
    config: AuctioneerConfig,
    cadence: AuctioneerCadence,
    pools: Vec<String>,
    submission_queue: Option<SubmissionQueue>,
    tick_rx: watch::Receiver<LedgerTick>,
    shutdown: &watch::Receiver<bool>,
) {
    let rpc = rpc.clone();
    let store = store.clone();
    let network = signing.network.clone();
    let tx_config = signing.tx_config;
    let signer = signing.signer.clone();
    let shutdown = shutdown.clone();
    tasks.spawn(async move {
        let submitter = signer
            .as_deref()
            .map(|signer| Submitter::new(&rpc, &network, signer, tx_config));
        let auctioneer = Auctioneer::new(&rpc, &store, config, submitter);
        auctioneer_loop(
            &store,
            &pools,
            &auctioneer,
            cadence,
            submission_queue.as_ref(),
            tick_rx,
            &shutdown,
        )
        .await
    });
}

/// Joins every service task, draining the set rather than short-circuiting
/// out of it, and reports the first error once they have all returned.
///
/// Returning on the first `Err` would drop the [`JoinSet`], which aborts
/// every task still running — including `run_queue`, possibly between
/// `sendTransaction` and the `getTransaction` poll that learns the
/// outcome. That is precisely what [`crate::queue::run_queue`]'s own doc
/// says must never happen: it would leave a signing key's sequence number
/// consumed by a transaction the bot never saw the end of, and a
/// `creations` row with `dry_run = false` and no hash.
/// `Store::attach_creation_tx` failing with a store error immediately after
/// a successful submission is a concrete way into exactly that.
///
/// So the first error raises `shutdown` — which every loop in this service
/// observes between units of work — and this keeps joining until every task
/// has returned on its own. The error path and the graceful-shutdown path
/// are then the same path, which is what the queue's doc already assumes.
/// Only the *first* error is reported: the ones after it are usually this
/// shutdown's own consequences, and a panic still propagates as a panic.
async fn drain_tasks(
    mut tasks: JoinSet<Result<(), LiquidatorError>>,
    shutdown: &watch::Sender<bool>,
) -> Result<(), LiquidatorError> {
    let mut failure: Option<LiquidatorError> = None;
    while let Some(outcome) = tasks.join_next().await {
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if failure.is_none() {
                    tracing::error!(
                        %error,
                        "a service task failed; shutting down and waiting for the rest"
                    );
                    let _ = shutdown.send(true);
                    failure = Some(error);
                } else {
                    tracing::warn!(%error, "another task failed while shutting down");
                }
            }
            Err(error) => resume_on_panic(error),
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The bot's two entry points: [`run`](Service::run) follows the configured
/// pools until shut down, and [`check_config`](Service::check_config)
/// validates and reports without following anything. Both are associated
/// functions rather than methods: there is no state to hold between a
/// process's one call to either.
pub struct Service;

impl Service {
    /// Validates the configuration, reporting its own resolved (redacted)
    /// form, the per-pool validation and every warning as it goes. Returns
    /// the warnings for a caller that wants them without re-reading logs;
    /// an `Err` is a failed validation, never a warning.
    pub async fn check_config(config: &ServiceConfig) -> Result<Vec<String>, LiquidatorError> {
        // Spec §10 makes this a deploy smoke test "against chain **and the
        // database**": a check that passes against an unreachable instance
        // or a wrong password is precisely the failure it exists to catch,
        // and it is run before dry-run is turned off. It connects and
        // pings rather than migrating, because a check must not have DDL
        // as a side effect — `run` migrates, under the advisory lock.
        let store = connect_store(config).await?;
        store.ping().await?;
        tracing::info!(
            max_connections = config.database_max_connections,
            "database reachable"
        );

        let rpc = RpcClient::from_config(&config.chain)?;
        let (validations, warnings) = validate(&rpc, &config.pools).await?;
        log_validation(config, &validations, &warnings);
        Ok(warnings)
    }

    /// Connects and migrates the store, validates the configuration, seeds
    /// every pool that needs it, then follows every configured pool — one
    /// [`LedgerPoller`] per pool, one tracker task consuming their shared
    /// channel, one auctioneer task fed by the tick the tracker publishes
    /// after it acknowledges, and — only when armed and a signer is given —
    /// one submission-queue worker for that signer's key — until a
    /// shutdown signal arrives and every task has returned.
    ///
    /// `keys` holds both of `AUCTIONEER_SECRET_KEY` and
    /// `FILLER_SECRET_KEY`, either or both of which may be absent — neither
    /// configured is the ordinary dry-run deployment; see
    /// [`crate::config::Args::signing_keys`]. At most one of them signs
    /// (the auctioneer's, else the filler's), but **both** addresses go
    /// into the set the auctioneer refuses to act on. Whether a submission
    /// is ever actually sent is `!config.dry_run && signer.is_some()` — the
    /// one gate this crate has into live trading, per the safety invariant
    /// that `DRY_RUN` defaults `true`.
    pub async fn run(config: ServiceConfig, keys: SigningKeys) -> Result<(), LiquidatorError> {
        // Installed before anything that takes time. Seeding a busy pool
        // is tens of seconds of sequential round trips, and until this is
        // in place a `SIGTERM` in that window reaches the default handler
        // and kills the process outright rather than draining it.
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // Shared rather than moved: the join loop at the bottom of this
        // function needs to raise the same flag when a task fails, so that
        // an error path and a signal path shut the bot down the same way.
        let shutdown_tx = Arc::new(shutdown_tx);
        spawn_shutdown_listener(Arc::clone(&shutdown_tx));

        let store = connect_store(&config).await?;
        store.migrate().await?;

        let rpc = RpcClient::from_config(&config.chain)?;
        let (validations, warnings) = validate(&rpc, &config.pools).await?;
        log_validation(&config, &validations, &warnings);

        let seed_sources = build_seed_sources(&config.seed)?;
        let needs_reseed = seed_pools_needing_it(
            &rpc,
            &store,
            &config.pools,
            &seed_sources,
            config.refresh_batch,
            &shutdown_rx,
        )
        .await?;

        let (message_tx, message_rx) = mpsc::channel(1_024);
        let poller_config = PollerConfig::new(config.poll_interval);
        let mut tasks = JoinSet::new();
        spawn_pollers(
            &mut tasks,
            &rpc,
            &store,
            &config.pools,
            poller_config,
            &message_tx,
            &shutdown_rx,
        );
        // Every poller now holds its own sender clone; dropping this one
        // lets the tracker task's channel close, and its `recv` return
        // `None`, once (and only once) every poller has stopped.
        drop(message_tx);

        let signing = SigningContext::from_config(&config, keys);
        let submission_queue =
            spawn_submission_queue(&mut tasks, &rpc, &signing, config.dry_run, &shutdown_rx);

        let auctioneer_config = AuctioneerConfig {
            liquidation_health_factor: config.liquidation_health_factor,
            target_health_factor: config.target_health_factor,
            plan_iterations: config.plan_iterations,
            own_addresses: signing.own_addresses(),
        };
        let auctioneer_cadence = auctioneer_cadence_from(&config);
        let pool_addresses: Vec<String> = config
            .pools
            .iter()
            .map(|pool| pool.address.clone())
            .collect();

        // The auctioneer's own view of the tick, published by the tracker
        // task only after it has acknowledged one (see `handle_message`'s
        // `Tick` arm). The initial value is never observed as real: a
        // `watch::Receiver` only wakes a waiter on a *change*, and this
        // loop's first `changed()` is what it actually reads.
        let (tick_tx, tick_rx) = watch::channel(LedgerTick {
            sequence: 0,
            close_time: 0,
        });
        spawn_auctioneer(
            &mut tasks,
            &rpc,
            &store,
            &signing,
            auctioneer_config,
            auctioneer_cadence,
            pool_addresses,
            submission_queue,
            tick_rx,
            &shutdown_rx,
        );

        let cadence = Cadence {
            user_refresh_ledgers: config.user_refresh_ledgers,
            refresh_batch: config.refresh_batch,
            full_scan_ledgers: config.full_scan_ledgers,
            scan_health_factor: config.scan_health_factor,
            phase: scan_phase(config.full_scan_ledgers),
        };
        let state = LoopState {
            needs_reseed,
            ..LoopState::default()
        };
        let shutdown = shutdown_rx.clone();
        tasks.spawn(async move {
            let tracker = Tracker::new(&rpc, &store);
            tracker_loop(
                &tracker,
                &seed_sources,
                cadence,
                state,
                &shutdown,
                tick_tx,
                message_rx,
            )
            .await
            .map_err(LiquidatorError::from)
        });

        drain_tasks(tasks, &shutdown_tx).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use stellar_xdr::{
        ContractDataDurability, ContractDataEntry, ContractExecutable, ExtensionPoint,
        LedgerEntryData, ScContractInstance, ScMap, ScString, ScVal,
    };

    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{scval_b64, transaction_data_b64, ScriptedRpc};
    use crate::chain::xdr::encode::{
        address, i128_val, map, sc_address, symbol, to_base64, vec as sc_vec,
    };
    use crate::chain::xdr::keys;
    use crate::chain::xdr::{AuctionType, PoolEvent};
    use crate::chain::{TxHash, TxOutcome};
    use crate::fixture::{mainnet_fixed_v2, text};
    use crate::harness;
    use crate::math::AuctionData;
    use std::collections::BTreeMap;
    use tokio::sync::oneshot;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const POOL_A: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const POOL_B: &str = "CA4I5TPQAEF6C62B4UI7IDNDAPT5RUNSYGB6WSYNIQXHLG4JOFX2NSMH";
    const BACKSTOP_A: &str = "CAQQR5SWBXKIGZKPBZDH3KM5GQ5GUTPKB7JAFCINLZBC5WXPJKRG3IM7";
    const BACKSTOP_B: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const ORACLE: &str = "CCVTVW2CVA7JLH4ROQGP3CU4T3EXVCK66AZGSM4MUQPXAI4QHCZPOATS";
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
    const BLND: &str = "CD25MNVTZDL4Y3XBCPCJXGXATV5WUHHOWMYFF4YBEGU5FCPGMYTVG5JY";
    const UNKNOWN_ASSET: &str = "CDTKPWPLOURQA2SGTKTUQOWRCBZEORB4BWBOMJ3D3ZTQQSGE5F6JBQLV";
    const ADMIN: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";

    /// One reserve as the synthetic pool builder describes it: the shape
    /// `validate` inspects, not a full contract-accurate reserve — no test
    /// here ever accrues one.
    #[derive(Debug, Clone, Copy)]
    struct SyntheticReserve {
        asset: &'static str,
        enabled: bool,
        c_factor: u32,
        price: Option<i128>,
    }

    fn entry(key: &stellar_xdr::LedgerKey, xdr: &str) -> Value {
        json!({"key": to_base64(key).unwrap(), "xdr": xdr, "lastModifiedLedgerSeq": 1, "liveUntilLedgerSeq": 99_999_999})
    }

    fn simulation(return_xdr: &str, ledger: u32) -> Value {
        json!({"transactionData": transaction_data_b64(1), "events": [], "minResourceFee": "1",
               "results": [{"auth": [], "xdr": return_xdr}], "latestLedger": ledger})
    }

    fn instance_entry_xdr(pool: &str, backstop: &str, oracle: &str, status: u32) -> String {
        let config = map(vec![
            (symbol("bstop_rate").unwrap(), ScVal::U32(1_000_000)),
            (symbol("max_positions").unwrap(), ScVal::U32(4)),
            (symbol("min_collateral").unwrap(), i128_val(0)),
            (symbol("oracle").unwrap(), address(oracle).unwrap()),
            (symbol("status").unwrap(), ScVal::U32(status)),
        ])
        .unwrap();
        let ScVal::Map(Some(config)) = config else {
            panic!("map returns a map")
        };
        let storage = ScMap::sorted_from(vec![
            (symbol("Admin").unwrap(), address(ADMIN).unwrap()),
            (symbol("BLNDTkn").unwrap(), address(BLND).unwrap()),
            (symbol("Backstop").unwrap(), address(backstop).unwrap()),
            (symbol("Config").unwrap(), ScVal::Map(Some(config))),
            (
                symbol("Name").unwrap(),
                ScVal::String(ScString::try_from(b"Test Pool".to_vec()).unwrap()),
            ),
        ])
        .unwrap();
        let instance = ScVal::ContractInstance(ScContractInstance {
            executable: ContractExecutable::StellarAsset,
            storage: Some(storage),
        });
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(pool).unwrap(),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
            val: instance,
        });
        to_base64(&entry).unwrap()
    }

    fn reserve_list_entry_xdr(pool: &str, assets: &[&str]) -> String {
        let list = sc_vec(assets.iter().map(|asset| address(asset).unwrap()).collect()).unwrap();
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(pool).unwrap(),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
            val: list,
        });
        to_base64(&entry).unwrap()
    }

    fn reserve_config_entry_xdr(pool: &str, index: u32, reserve: SyntheticReserve) -> String {
        let config = map(vec![
            (symbol("c_factor").unwrap(), ScVal::U32(reserve.c_factor)),
            (symbol("decimals").unwrap(), ScVal::U32(7)),
            (symbol("enabled").unwrap(), ScVal::Bool(reserve.enabled)),
            (symbol("index").unwrap(), ScVal::U32(index)),
            (symbol("l_factor").unwrap(), ScVal::U32(0)),
            (symbol("max_util").unwrap(), ScVal::U32(9_500_000)),
            (symbol("r_base").unwrap(), ScVal::U32(0)),
            (symbol("r_one").unwrap(), ScVal::U32(0)),
            (symbol("r_three").unwrap(), ScVal::U32(0)),
            (symbol("r_two").unwrap(), ScVal::U32(0)),
            (symbol("reactivity").unwrap(), ScVal::U32(0)),
            (symbol("supply_cap").unwrap(), i128_val(0)),
            (symbol("util").unwrap(), ScVal::U32(0)),
        ])
        .unwrap();
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(pool).unwrap(),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
            val: config,
        });
        to_base64(&entry).unwrap()
    }

    fn reserve_data_entry_xdr(pool: &str) -> String {
        let data = map(vec![
            (symbol("b_rate").unwrap(), i128_val(1_000_000_000_000)),
            (symbol("b_supply").unwrap(), i128_val(0)),
            (symbol("backstop_credit").unwrap(), i128_val(0)),
            (symbol("d_rate").unwrap(), i128_val(1_000_000_000_000)),
            (symbol("d_supply").unwrap(), i128_val(0)),
            (symbol("ir_mod").unwrap(), i128_val(10_000_000)),
            (symbol("last_time").unwrap(), ScVal::U64(0)),
        ])
        .unwrap();
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(pool).unwrap(),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
            val: data,
        });
        to_base64(&entry).unwrap()
    }

    /// Scripts one complete `PoolReader::snapshot(&[])` of a synthetic pool:
    /// the shape read, the batched reserve read, then the oracle's decimals
    /// and one `lastprice` per reserve, in reserve-list order — the same
    /// call sequence `harness::script_snapshot` scripts for the real
    /// fixture, but built from parameters instead of a captured ledger, so
    /// each test can choose exactly the field it means to violate.
    fn script_pool(
        rpc: &ScriptedRpc,
        pool: &str,
        backstop: &str,
        status: u32,
        reserves: &[SyntheticReserve],
        ledger: u32,
    ) {
        let assets: Vec<&str> = reserves.iter().map(|reserve| reserve.asset).collect();
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(pool).unwrap(), &instance_entry_xdr(pool, backstop, ORACLE, status)),
                entry(&keys::reserve_list(pool).unwrap(), &reserve_list_entry_xdr(pool, &assets)),
            ]}),
        );
        let mut entries = Vec::new();
        for (index, reserve) in reserves.iter().enumerate() {
            let index = u32::try_from(index).unwrap();
            entries.push(entry(
                &keys::reserve_config(pool, reserve.asset).unwrap(),
                &reserve_config_entry_xdr(pool, index, *reserve),
            ));
            entries.push(entry(
                &keys::reserve_data(pool, reserve.asset).unwrap(),
                &reserve_data_entry_xdr(pool),
            ));
        }
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": entries}),
        );
        rpc.expect(
            "simulateTransaction",
            simulation(&scval_b64(&ScVal::U32(7)), ledger),
        );
        for reserve in reserves {
            let price_value = match reserve.price {
                Some(price) => map(vec![
                    (symbol("price").unwrap(), i128_val(price)),
                    (symbol("timestamp").unwrap(), ScVal::U64(0)),
                ])
                .unwrap(),
                None => ScVal::Void,
            };
            rpc.expect(
                "simulateTransaction",
                simulation(&scval_b64(&price_value), ledger),
            );
        }
    }

    fn pool_config(address: &str, primary_asset: &str, bid: &[&str], lot: &[&str]) -> PoolConfig {
        PoolConfig {
            address: address.to_string(),
            primary_asset: primary_asset.to_string(),
            min_primary_collateral: 0,
            min_health_factor: 15_000_000,
            default_profit_bps: 0,
            force_fill: false,
            supported_bid: bid.iter().map(|asset| (*asset).to_string()).collect(),
            supported_lot: lot.iter().map(|asset| (*asset).to_string()).collect(),
            profits: Vec::new(),
        }
    }

    const LEDGER: u32 = 1_000;

    fn usable_reserve() -> SyntheticReserve {
        SyntheticReserve {
            asset: USDC,
            enabled: true,
            c_factor: 7_500_000,
            price: Some(10_000_000),
        }
    }

    /// A second, usable-but-unpriced reserve: neither the primary asset nor
    /// a listed supported asset in any test that uses it, so it exercises
    /// only the oracle-warning branch, not `validate_primary_asset`'s or
    /// `validate_supported_assets`'s already-covered `Err` paths.
    fn unpriced_reserve() -> SyntheticReserve {
        SyntheticReserve {
            asset: BLND,
            price: None,
            ..usable_reserve()
        }
    }

    /// Validation accepts the fixture pool and reports its reserves.
    #[sqlx::test(migrations = "./migrations")]
    async fn validation_accepts_a_pool_that_loads_from_chain(db: sqlx::PgPool) -> sqlx::Result<()> {
        // `db` is unused: `validate` never touches the store. `sqlx::test`
        // is kept anyway, as the async-test harness every other integration
        // test in this crate uses, and as a standing check that the
        // migrations this test's neighbours depend on still apply cleanly.
        let _store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Active.code(),
            &[usable_reserve()],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, USDC, &[USDC], &["*"]);

        let (validations, warnings) = validate(&client, &[pool]).await.expect("validate");
        assert_eq!(validations.len(), 1);
        assert_eq!(validations[0].pool, POOL_A);
        assert_eq!(validations[0].reserves, 1);
        assert_eq!(validations[0].backstop, BACKSTOP_A);
        assert!(
            warnings.is_empty(),
            "an active, fully priced pool has no warnings: {warnings:?}"
        );
        Ok(())
    }

    /// A non-active pool status is a warning that still lets validation
    /// succeed, not an error: a frozen or on-ice pool must let the bot
    /// start and warn, not refuse to start.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_non_active_pool_status_is_a_warning_not_an_error(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let _store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Frozen.code(),
            &[usable_reserve()],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, USDC, &[USDC], &["*"]);

        let (validations, warnings) = validate(&client, &[pool])
            .await
            .expect("a non-active pool status is a warning, not an error");
        assert_eq!(
            validations.len(),
            1,
            "validation still succeeded and reported the pool"
        );
        assert_eq!(validations[0].pool, POOL_A);
        assert_eq!(
            warnings,
            vec![format!(
                "pool {POOL_A}: status is {:?}, not active",
                PoolStatus::Frozen
            )],
            "the warning must name the pool and its actual status"
        );
        Ok(())
    }

    /// An asset the oracle does not price is a warning that still lets
    /// validation succeed, not an error: an oracle that has stopped
    /// pricing one reserve must let the bot start and warn, not refuse to
    /// start.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unpriced_reserve_is_a_warning_not_an_error(db: sqlx::PgPool) -> sqlx::Result<()> {
        let _store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Active.code(),
            &[usable_reserve(), unpriced_reserve()],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, USDC, &[USDC], &["*"]);

        let (validations, warnings) = validate(&client, &[pool])
            .await
            .expect("an unpriced reserve is a warning, not an error");
        assert_eq!(validations.len(), 1);
        assert_eq!(
            validations[0].reserves, 2,
            "both reserves are reported, priced or not"
        );
        assert_eq!(
            warnings,
            vec![format!("pool {POOL_A}: the oracle has no price for {BLND}")],
            "the warning must name the unpriced asset"
        );
        Ok(())
    }

    /// Two pools with different backstops is a configuration error naming
    /// both, because the filler's positions are shared across them.
    #[sqlx::test(migrations = "./migrations")]
    async fn pools_with_different_backstops_are_refused(db: sqlx::PgPool) -> sqlx::Result<()> {
        let _store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Active.code(),
            &[usable_reserve()],
            LEDGER,
        );
        script_pool(
            &rpc,
            POOL_B,
            BACKSTOP_B,
            PoolStatus::Active.code(),
            &[usable_reserve()],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![
            pool_config(POOL_A, USDC, &[USDC], &["*"]),
            pool_config(POOL_B, USDC, &[USDC], &["*"]),
        ];

        let error = validate(&client, &pools)
            .await
            .expect_err("different backstops");
        assert!(matches!(error, LiquidatorError::Config(_)));
        let message = error.to_string();
        assert!(
            message.contains(BACKSTOP_A) && message.contains(BACKSTOP_B),
            "the message should name both backstops: {message}"
        );
        Ok(())
    }

    /// A primary asset that is not a reserve, or has no collateral factor,
    /// is refused with a message naming the asset.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_primary_asset_that_is_not_usable_collateral_is_refused(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let _store = Store::from_pool(db);

        // Not a reserve at all.
        let rpc = ScriptedRpc::start().await;
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Active.code(),
            &[usable_reserve()],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, UNKNOWN_ASSET, &["*"], &["*"]);
        let error = validate(&client, &[pool])
            .await
            .expect_err("not a reserve")
            .to_string();
        assert!(error.contains(UNKNOWN_ASSET), "{error}");

        // A reserve, but with no collateral factor.
        let rpc = ScriptedRpc::start().await;
        let no_collateral = SyntheticReserve {
            c_factor: 0,
            ..usable_reserve()
        };
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Active.code(),
            &[no_collateral],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, USDC, &["*"], &["*"]);
        let error = validate(&client, &[pool])
            .await
            .expect_err("no collateral factor")
            .to_string();
        assert!(error.contains(USDC), "{error}");

        // A reserve with a collateral factor, but disabled.
        let rpc = ScriptedRpc::start().await;
        let disabled = SyntheticReserve {
            enabled: false,
            ..usable_reserve()
        };
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Active.code(),
            &[disabled],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, USDC, &["*"], &["*"]);
        let error = validate(&client, &[pool])
            .await
            .expect_err("disabled")
            .to_string();
        assert!(error.contains(USDC), "{error}");
        Ok(())
    }

    /// A supported asset that is not a reserve is refused; `"*"` is not
    /// checked against anything.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_supported_asset_is_refused_and_a_wildcard_is_not(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let _store = Store::from_pool(db);

        // The wildcard is accepted, and checked against nothing.
        let rpc = ScriptedRpc::start().await;
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Active.code(),
            &[usable_reserve()],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, USDC, &["*"], &["*"]);
        let (_, warnings) = validate(&client, &[pool])
            .await
            .expect("a wildcard is always accepted");
        assert!(warnings.is_empty());

        // An explicit, unknown asset is refused, naming it.
        let rpc = ScriptedRpc::start().await;
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Active.code(),
            &[usable_reserve()],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, USDC, &["*"], &[UNKNOWN_ASSET]);
        let error = validate(&client, &[pool])
            .await
            .expect_err("an unlisted reserve")
            .to_string();
        assert!(error.contains(UNKNOWN_ASSET), "{error}");
        Ok(())
    }

    /// Writes a seed file and returns its path: a real file for
    /// `FileSeed::load` to read, as the tracker's own seed tests use.
    /// Every caller removes it.
    fn write_temp_seed_file(contents: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "blend-liquidator-service-seed-{}-{id}.toml",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("write temp seed file");
        path
    }

    /// A cadence whose passes stay out of the way unless a test asks for
    /// one: the stale cutoff lands at ledger 0, so no row is ever stale,
    /// and a zero full-scan period never fires.
    fn quiet_cadence() -> Cadence {
        Cadence {
            user_refresh_ledgers: u32::MAX,
            refresh_batch: 20,
            full_scan_ledgers: 0,
            scan_health_factor: 15_000_000,
            phase: 0,
        }
    }

    /// A `borrow` event for `account`, which names it and nothing else.
    fn borrow(pool: &str, account: &str) -> PollerMessage {
        PollerMessage::Event {
            pool: pool.to_string(),
            ledger: 64_271_340,
            event: PoolEvent::Borrow {
                asset: USDC.to_string(),
                from: account.to_string(),
                amount: 1_000,
                d_tokens: 900,
            },
        }
    }

    /// A tick for `pool`, with the acknowledgement channel the poller uses
    /// to learn whether it may commit its cursor.
    fn tick_message(pool: &str, tick: LedgerTick) -> (PollerMessage, oneshot::Receiver<()>) {
        let (ack, applied) = oneshot::channel();
        (
            PollerMessage::Tick {
                pool: pool.to_string(),
                tick,
                ack,
            },
            applied,
        )
    }

    /// A throwaway tick watch for a test that does not care what the
    /// auctioneer sees, only that `handle_message`/`tracker_loop` have
    /// somewhere to publish to.
    fn tick_watch() -> (watch::Sender<LedgerTick>, watch::Receiver<LedgerTick>) {
        watch::channel(LedgerTick {
            sequence: 0,
            close_time: 0,
        })
    }

    /// A stored borrower row, for the passes that select on `updated_ledger`.
    fn tracked_user(account: &str, updated_ledger: u32) -> TrackedUser {
        let mut collateral = BTreeMap::new();
        collateral.insert(0_u32, 1_000_000_i128);
        let mut liabilities = BTreeMap::new();
        liabilities.insert(1_u32, 500_000_i128);
        TrackedUser {
            pool: harness::POOL.to_string(),
            account: account.to_string(),
            health_factor: 20_000_000,
            collateral,
            liabilities,
            updated_ledger,
            recheck_ledger: None,
        }
    }

    /// Whether the last batched `getLedgerEntries` asked for `account`'s
    /// positions: which accounts a pass actually read from chain.
    fn read_from_chain(rpc: &ScriptedRpc, account: &str) -> bool {
        let calls = rpc.calls("getLedgerEntries");
        let Some(batched) = calls.last() else {
            return false;
        };
        batched["keys"]
            .as_array()
            .expect("keys array")
            .contains(&json!(to_base64(
                &keys::positions(harness::POOL, account).unwrap()
            )
            .unwrap()))
    }

    /// An event accumulates its accounts and writes nothing; the pool's
    /// tick refreshes them from chain and acknowledges, which is what lets
    /// the poller commit its cursor.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_event_accumulates_and_is_flushed_at_its_pools_tick(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, tick_rx) = tick_watch();
        let mut state = LoopState::default();

        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            borrow(harness::POOL, harness::USER_ONE),
        )
        .await
        .expect("apply the event");
        assert_eq!(
            state.pending[harness::POOL],
            BTreeSet::from([harness::USER_ONE.to_string()])
        );
        assert_eq!(
            store.count_users(harness::POOL).await.expect("count"),
            0,
            "an event writes no user row: valuing is the tick's job"
        );

        let tick = harness::fixture_tick();
        let (message, applied) = tick_message(harness::POOL, tick);
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            message,
        )
        .await
        .expect("apply the tick");
        assert!(
            applied.await.is_ok(),
            "a tick whose whole effect landed is acknowledged"
        );
        assert!(
            !state.pending.contains_key(harness::POOL),
            "the tick consumed what accumulated"
        );
        assert!(store
            .user(harness::POOL, harness::USER_ONE)
            .await
            .expect("read")
            .is_some());
        assert_eq!(
            *tick_rx.borrow(),
            tick,
            "the auctioneer's watch carries the acknowledged tick"
        );
        assert_eq!(
            store
                .user(harness::POOL, harness::USER_ONE)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence),
            "the account this tick's event named is flagged for an auctioneer decision"
        );
        Ok(())
    }

    /// Two pools share one channel. One pool's tick must refresh only its
    /// own accounts and leave the other pool's accumulating.
    #[sqlx::test(migrations = "./migrations")]
    async fn two_pools_on_one_channel_do_not_cross_contaminate(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, _tick_rx) = tick_watch();
        let mut state = LoopState::default();

        for message in [
            borrow(harness::POOL, harness::USER_ONE),
            borrow(POOL_B, harness::USER_TWO),
        ] {
            handle_message(
                &tracker,
                &[],
                quiet_cadence(),
                &mut state,
                &shutdown,
                &tick_tx,
                message,
            )
            .await
            .expect("each pool's event");
        }

        let (message, applied) = tick_message(harness::POOL, harness::fixture_tick());
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            message,
        )
        .await
        .expect("one pool's tick");
        assert!(applied.await.is_ok());

        assert!(
            read_from_chain(&rpc, harness::USER_ONE),
            "the ticking pool's account was refreshed"
        );
        assert!(
            !read_from_chain(&rpc, harness::USER_TWO),
            "the other pool's account was not"
        );
        assert_eq!(
            state.pending[POOL_B],
            BTreeSet::from([harness::USER_TWO.to_string()]),
            "and is still waiting for its own pool's tick"
        );
        Ok(())
    }

    /// The stale-refresh pass compares a row's ledger against the tick
    /// minus `USER_REFRESH_LEDGERS`. Handing the span over as an absolute
    /// cutoff — a ledger count against a ledger sequence — selects nothing
    /// on any live network, which is what this test is here to catch.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_stale_refresh_pass_takes_an_old_row_and_leaves_a_fresh_one(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        store
            .upsert_user(&tracked_user(harness::USER_ONE, tick.sequence - 300_000))
            .await
            .expect("a row older than the refresh span");
        store
            .upsert_user(&tracked_user(harness::USER_TWO, tick.sequence - 10))
            .await
            .expect("a row well inside it");

        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, _tick_rx) = tick_watch();
        let mut state = LoopState::default();
        let cadence = Cadence {
            user_refresh_ledgers: 241_920,
            ..quiet_cadence()
        };

        let (message, applied) = tick_message(harness::POOL, tick);
        handle_message(
            &tracker,
            &[],
            cadence,
            &mut state,
            &shutdown,
            &tick_tx,
            message,
        )
        .await
        .expect("the tick");
        assert!(applied.await.is_ok());

        assert!(
            read_from_chain(&rpc, harness::USER_ONE),
            "the row older than the refresh span was re-read from chain"
        );
        assert!(
            !read_from_chain(&rpc, harness::USER_TWO),
            "the fresh row was left alone"
        );
        assert_eq!(
            store
                .user(harness::POOL, harness::USER_ONE)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence),
            "the stale row the refresh pass touched is flagged for an auctioneer decision"
        );
        assert_eq!(
            store
                .user(harness::POOL, harness::USER_TWO)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            None,
            "the fresh row the refresh pass left alone is not flagged"
        );
        Ok(())
    }

    /// A gap reseeds the pool from its sources; a source that could not
    /// answer leaves the pool marked, and the next full scan retries it.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_gap_reseeds_and_an_incomplete_seed_is_retried_by_the_full_scan(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let tick = harness::fixture_tick();
        // The gap's own reseed, then the full scan's retry.
        for _ in 0..2 {
            rpc.expect(
                "getLatestLedger",
                json!({"id": "aa", "protocolVersion": 27, "sequence": tick.sequence,
                       "closeTime": tick.close_time.to_string()}),
            );
            harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        }
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, _tick_rx) = tick_watch();
        let mut state = LoopState::default();

        let file = write_temp_seed_file(&format!(
            "[accounts]\n\"{}\" = [\"{}\"]\n",
            harness::POOL,
            harness::USER_ONE
        ));
        let unreachable = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&unreachable)
            .await;
        let sources = vec![
            SeedSource::File(FileSeed::load(&file).expect("loads")),
            SeedSource::Analytics(AnalyticsSeed::new(&unreachable.uri(), 100_000_000).unwrap()),
        ];

        handle_message(
            &tracker,
            &sources,
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            PollerMessage::Gap {
                pool: harness::POOL.to_string(),
                from: 10,
                oldest: 400_000,
            },
        )
        .await
        .expect("the gap");
        assert!(
            store
                .user(harness::POOL, harness::USER_ONE)
                .await
                .expect("read")
                .is_some(),
            "the source that answered was seeded and valued from chain"
        );
        assert!(
            state.needs_reseed.contains(harness::POOL),
            "a source that could not answer leaves the seed incomplete"
        );

        // The full scan retries it. This one reaches every source, because
        // the gap arm reseeds from the same list and the file still reads.
        let sources = vec![SeedSource::File(FileSeed::load(&file).expect("loads"))];
        let cadence = Cadence {
            full_scan_ledgers: 1,
            ..quiet_cadence()
        };
        let (message, applied) = tick_message(harness::POOL, tick);
        handle_message(
            &tracker, &sources, cadence, &mut state, &shutdown, &tick_tx, message,
        )
        .await
        .expect("the scanning tick");
        assert!(applied.await.is_ok());
        assert!(
            !state.needs_reseed.contains(harness::POOL),
            "a seed that reached every source clears the mark"
        );
        let _ = std::fs::remove_file(&file);
        Ok(())
    }

    /// A tick that fails part way declines its acknowledgement — so the
    /// poller leaves the cursor where it is — and puts back the accounts it
    /// took, so the next pass retries them rather than dropping them.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_failed_tick_declines_its_acknowledgement_and_keeps_its_accounts(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        rpc.expect_http("getLedgerEntries", 503);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, tick_rx) = tick_watch();
        let mut state = LoopState::default();
        state.pending.insert(
            harness::POOL.to_string(),
            BTreeSet::from([harness::USER_ONE.to_string()]),
        );

        let (message, applied) = tick_message(harness::POOL, harness::fixture_tick());
        let error = handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            message,
        )
        .await
        .expect_err("the refresh could not read chain");
        assert!(matches!(error, TrackerError::Chain(_)));
        assert!(
            applied.await.is_err(),
            "an unapplied tick is never acknowledged, so the cursor stays put"
        );
        assert_eq!(
            state.pending[harness::POOL],
            BTreeSet::from([harness::USER_ONE.to_string()]),
            "the accounts this tick took are back for the next pass"
        );
        assert_eq!(
            *tick_rx.borrow(),
            LedgerTick {
                sequence: 0,
                close_time: 0
            },
            "a tick that never acknowledged is never published to the auctioneer either"
        );
        Ok(())
    }

    /// `state.unapplied` is the poison `handle_message`'s `Event` arm sets
    /// when `Tracker::apply` fails, and the `Tick` arm's whole reason to
    /// exist: a failed event's pool declines its very next tick's
    /// acknowledgement so the poller does not commit a cursor past a
    /// ledger the store never recorded. Three things must all be true, or
    /// the pool's cursor either commits over a gap or stalls forever:
    /// setting the mark, clearing it once spent, and never touching a
    /// pool that did not fail.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_failed_event_poisons_only_its_own_pools_next_tick(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        // The only non-fatal failure inside `Tracker::apply`: a partial
        // fill re-reads the auction's remainder from chain, and that read
        // fails here.
        rpc.expect_http("getLedgerEntries", 503);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, _tick_rx) = tick_watch();
        let mut state = LoopState::default();

        let failing_fill = PollerMessage::Event {
            pool: harness::POOL.to_string(),
            ledger: harness::fixture_tick().sequence,
            event: PoolEvent::FillAuction {
                auction_type: AuctionType::UserLiquidation,
                user: harness::USER_ONE.to_string(),
                filler: harness::USER_TWO.to_string(),
                fill_percent: 60,
                filled: AuctionData::default(),
            },
        };
        let error = handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            failing_fill,
        )
        .await
        .expect_err("the partial-fill re-read could not reach chain");
        assert!(matches!(error, TrackerError::Chain(_)));
        assert!(
            state.unapplied.contains(harness::POOL),
            "the pool whose event failed is marked"
        );

        // A second pool's tick is not poisoned by the first pool's failed
        // event: they share one channel and must not see each other's
        // state.
        let (other_message, other_applied) = tick_message(POOL_B, harness::fixture_tick());
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            other_message,
        )
        .await
        .expect("the other pool's tick applies");
        assert!(
            other_applied.await.is_ok(),
            "a failure on one pool must not poison another pool's tick"
        );

        // This pool's own next tick declines its acknowledgement, so the
        // poller leaves the cursor where it is and re-reads the range —
        // and the mark is spent by that decline, not left latched.
        let (message, applied) = tick_message(harness::POOL, harness::fixture_tick());
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            message,
        )
        .await
        .expect("the tick itself has nothing to refresh and applies cleanly");
        assert!(
            applied.await.is_err(),
            "the tick right after a failed event is not acknowledged"
        );
        assert!(
            !state.unapplied.contains(harness::POOL),
            "the mark is cleared once it has declined a tick"
        );

        // A second tick, with no failure in between, is acknowledged
        // normally. A flag that latched instead of clearing would stall
        // this pool's cursor forever, and only this assertion catches it.
        let (message, applied) = tick_message(harness::POOL, harness::fixture_tick());
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            message,
        )
        .await
        .expect("a clean tick applies");
        assert!(
            applied.await.is_ok(),
            "with the mark cleared, a tick with no failure ahead of it is acknowledged"
        );
        Ok(())
    }

    /// A transient chain failure logs and the loop carries on: the poller
    /// re-reads the range it never acknowledged, and the second delivery
    /// applies. A store failure is the one that ends the loop.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_transient_failure_does_not_end_the_tracker_loop(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        rpc.expect_http("getLedgerEntries", 503);
        harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, tick_rx) = tick_watch();
        let (sender, receiver) = mpsc::channel(8);

        sender
            .send(borrow(harness::POOL, harness::USER_ONE))
            .await
            .expect("event");
        let (first, declined) = tick_message(harness::POOL, harness::fixture_tick());
        sender.send(first).await.expect("failing tick");
        let (second, acknowledged) = tick_message(harness::POOL, harness::fixture_tick());
        sender.send(second).await.expect("retried tick");
        drop(sender);

        tracker_loop(
            &tracker,
            &[],
            quiet_cadence(),
            LoopState::default(),
            &shutdown,
            tick_tx,
            receiver,
        )
        .await
        .expect("a chain failure is logged, not fatal");

        assert!(
            declined.await.is_err(),
            "the tick that failed was not acknowledged"
        );
        assert!(
            acknowledged.await.is_ok(),
            "the replayed tick was, once it applied"
        );
        assert!(
            store
                .user(harness::POOL, harness::USER_ONE)
                .await
                .expect("read")
                .is_some(),
            "the account the failed tick held on to was refreshed by the retry"
        );
        assert_eq!(
            *tick_rx.borrow(),
            harness::fixture_tick(),
            "only the acknowledged retry was published to the auctioneer's watch"
        );
        Ok(())
    }

    /// Seeding commits the events cursor at the ledger it seeded from.
    /// Without it the poller starts from the head *it* reads, and every
    /// event between the two — the whole time seeding every pool takes — is
    /// read by nothing. The loss is permanent: a pool that has users and a
    /// cursor is never seeded again, so a borrower who opens their first
    /// position in that window is tracked only if a later event names them.
    #[sqlx::test(migrations = "./migrations")]
    async fn seeding_commits_the_cursor_at_the_ledger_it_seeded_from(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let tick = harness::fixture_tick();
        rpc.expect(
            "getLatestLedger",
            json!({"id": "aa", "protocolVersion": 27, "sequence": tick.sequence,
                   "closeTime": tick.close_time.to_string()}),
        );
        harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let (_flag, shutdown) = watch::channel(false);
        let file = write_temp_seed_file(&format!(
            "[accounts]\n\"{}\" = [\"{}\"]\n",
            harness::POOL,
            harness::USER_ONE
        ));
        let sources = vec![SeedSource::File(FileSeed::load(&file).expect("loads"))];

        let incomplete = seed_pools_needing_it(
            &client,
            &store,
            &[pool_config(harness::POOL, USDC, &["*"], &["*"])],
            &sources,
            20,
            &shutdown,
        )
        .await
        .expect("seeding succeeds");

        assert!(incomplete.is_empty(), "the one source answered");
        let cursor = store
            .cursor(&events_cursor(harness::POOL))
            .await
            .expect("cursor read")
            .expect("seeding commits a cursor");
        assert_eq!(
            cursor.ledger, tick.sequence,
            "the cursor is the ledger the seed valued positions at, so the poller resumes there"
        );
        assert_eq!(cursor.paging_token, None);
        assert_eq!(store.count_users(harness::POOL).await.expect("count"), 1);
        Ok(())
    }

    /// A full scan that fails does not fail the tick, and the period is
    /// recorded anyway. The acknowledgement means this ledger's effects are
    /// in the store, and a best-effort reseed retry is not one of them —
    /// so a scan that fails the same way every time (a borrower the seed
    /// names holding a reserve the oracle does not price, which `validate`
    /// deliberately permits) would otherwise stall this pool's cursor for
    /// good, because the redelivered tick would find the scan still due.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_failing_full_scan_neither_fails_the_tick_nor_repeats_forever(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let tick = harness::fixture_tick();
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, _tick_rx) = tick_watch();

        // The pool is due a reseed, and the reseed will fail against the
        // chain every time: no snapshot is ever scripted.
        let mut state = LoopState::default();
        state.needs_reseed.insert(harness::POOL.to_string());
        let file = write_temp_seed_file(&format!(
            "[accounts]\n\"{}\" = [\"{}\"]\n",
            harness::POOL,
            harness::USER_ONE
        ));
        let sources = vec![SeedSource::File(FileSeed::load(&file).expect("loads"))];
        let cadence = Cadence {
            full_scan_ledgers: 100,
            ..quiet_cadence()
        };

        let (message, applied) = tick_message(harness::POOL, tick);
        handle_message(
            &tracker, &sources, cadence, &mut state, &shutdown, &tick_tx, message,
        )
        .await
        .expect("a failing scan is not a failing tick");
        assert!(
            applied.await.is_ok(),
            "the tick is acknowledged, so the poller commits the cursor"
        );
        assert!(
            state.needs_reseed.contains(harness::POOL),
            "the reseed is still owed, to be retried next period"
        );
        assert_eq!(
            state.last_scan.get(harness::POOL),
            Some(&tick.sequence),
            "the period is recorded by a scan that tried, so the cadence cannot spin on it"
        );

        // The next tick inside the same period does not scan again, so a
        // deterministic failure costs one warning per period, not a stall.
        let next = LedgerTick {
            sequence: tick.sequence + 1,
            close_time: tick.close_time,
        };
        let (message, applied) = tick_message(harness::POOL, next);
        handle_message(
            &tracker, &sources, cadence, &mut state, &shutdown, &tick_tx, message,
        )
        .await
        .expect("the following tick");
        assert!(applied.await.is_ok());
        // The acknowledgement alone cannot show the scan was skipped — a
        // scan that ran and failed is acknowledged too, by this very fix.
        // The recorded period is what distinguishes them: it would have
        // moved to this tick had the scan run again.
        assert_eq!(
            state.last_scan.get(harness::POOL),
            Some(&tick.sequence),
            "the second tick in the period did not scan"
        );
        Ok(())
    }

    /// A seed that ran only partly commits no cursor. Committing one would
    /// make the gap durable: the pool would have users *and* a cursor, so
    /// the next start's skip test would pass it by for good, and the
    /// accounts the seed never valued would be tracked only if some later
    /// event happened to name them.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_incomplete_seed_commits_no_cursor_so_the_next_start_finishes_it(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let tick = harness::fixture_tick();
        rpc.expect(
            "getLatestLedger",
            json!({"id": "aa", "protocolVersion": 27, "sequence": tick.sequence,
                   "closeTime": tick.close_time.to_string()}),
        );
        harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let (_flag, shutdown) = watch::channel(false);

        // One source answers and one does not: the accounts the first named
        // are written, so the pool ends up with users but an incomplete set.
        let file = write_temp_seed_file(&format!(
            "[accounts]\n\"{}\" = [\"{}\"]\n",
            harness::POOL,
            harness::USER_ONE
        ));
        let unreachable = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&unreachable)
            .await;
        let sources = vec![
            SeedSource::File(FileSeed::load(&file).expect("loads")),
            SeedSource::Analytics(AnalyticsSeed::new(&unreachable.uri(), 100_000_000).unwrap()),
        ];

        let incomplete = seed_pools_needing_it(
            &client,
            &store,
            &[pool_config(harness::POOL, USDC, &["*"], &["*"])],
            &sources,
            20,
            &shutdown,
        )
        .await
        .expect("a source that does not answer is not fatal");

        assert!(incomplete.contains(harness::POOL), "marked for retry");
        assert_eq!(
            store.count_users(harness::POOL).await.expect("count"),
            1,
            "what the answering source named is written"
        );
        assert!(
            store
                .cursor(&events_cursor(harness::POOL))
                .await
                .expect("cursor read")
                .is_none(),
            "an incomplete seed records no position, so the next start seeds this pool again"
        );
        Ok(())
    }

    /// A seed that fails against the chain costs this pool's coverage and is
    /// retried; it is not fatal. The tracker loop treats the same error
    /// class as transient, and `validate` deliberately records an unpriced
    /// reserve as a warning — but any seeded borrower holding that asset
    /// makes the seed's refresh fail, so propagating it would keep the bot
    /// from ever starting on a pool it is allowed to follow.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_chain_failure_while_seeding_is_not_fatal(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let tick = harness::fixture_tick();
        // The head is answered; the snapshot the refresh needs is not.
        rpc.expect(
            "getLatestLedger",
            json!({"id": "aa", "protocolVersion": 27, "sequence": tick.sequence,
                   "closeTime": tick.close_time.to_string()}),
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let (_flag, shutdown) = watch::channel(false);
        let file = write_temp_seed_file(&format!(
            "[accounts]\n\"{}\" = [\"{}\"]\n",
            harness::POOL,
            harness::USER_ONE
        ));
        let sources = vec![SeedSource::File(FileSeed::load(&file).expect("loads"))];

        let incomplete = seed_pools_needing_it(
            &client,
            &store,
            &[pool_config(harness::POOL, USDC, &["*"], &["*"])],
            &sources,
            20,
            &shutdown,
        )
        .await
        .expect("a chain failure while seeding is not fatal");

        assert!(
            incomplete.contains(harness::POOL),
            "the pool is marked for retry"
        );
        assert!(
            store
                .cursor(&events_cursor(harness::POOL))
                .await
                .expect("cursor read")
                .is_none(),
            "no cursor is committed for a pool the seed could not read, so the next start seeds it again"
        );
        Ok(())
    }

    /// The design spec's default thresholds (`LIQ_HF_THRESHOLD=0.998`,
    /// `TARGET_HF=1.06`, `PLAN_ITERATIONS=5`), with no own addresses: none
    /// of these tests needs to exclude an account.
    fn auctioneer_config() -> AuctioneerConfig {
        AuctioneerConfig {
            liquidation_health_factor: 9_980_000,
            target_health_factor: 10_600_000,
            plan_iterations: 5,
            own_addresses: BTreeSet::new(),
        }
    }

    /// A valid, distinct account strkey with no real key behind it — every
    /// test that uses one only ever reads chain state through the
    /// scripted RPC, never signs anything.
    fn synthetic_debtor() -> String {
        stellar_strkey::ed25519::PublicKey([42_u8; 32]).to_string()
    }

    /// Another one, distinct from [`synthetic_debtor`] and from every
    /// other `seed` these tests pass.
    fn synthetic_account(seed: u8) -> String {
        stellar_strkey::ed25519::PublicKey([seed; 32]).to_string()
    }

    /// A key for the one test here that needs a [`Submitter`] at all. It
    /// signs nothing: that test's simulation never gets past the source
    /// account's own read.
    fn test_signer() -> Signer {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7_u8; 32]);
        let secret = stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string();
        Signer::from_secret(&secret).expect("signer")
    }

    /// Fee and polling policy short enough that nothing in a test waits on
    /// a real interval.
    fn test_tx_config() -> TxConfig {
        TxConfig {
            poll_interval: std::time::Duration::from_millis(1),
            send_retry_pause: std::time::Duration::from_millis(1),
            wait_cap: std::time::Duration::from_millis(200),
            ..TxConfig::new(100, 200, 3)
        }
    }

    /// The source-account read `Submitter::simulate_only` makes before it
    /// builds anything: one `getLedgerEntries` for the signer's own
    /// account entry, and nothing else. A simulate-only call needs no fee
    /// stats, so a test that scripts those is scripting the signing path
    /// by mistake.
    fn script_account_entry(rpc: &ScriptedRpc, signer: &Signer) {
        let key = stellar_xdr::LedgerKey::Account(stellar_xdr::LedgerKeyAccount {
            account_id: signer.account_id(),
        });
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 1_u32, "entries": [
                {"key": to_base64(&key).expect("key"),
                 "xdr": crate::chain::script::account_entry_b64(signer.address(), 1),
                 "lastModifiedLedgerSeq": 1}
            ]}),
        );
    }

    /// The `simulateTransaction` answer for an operation the contract
    /// refuses with `code`, in both the diagnostic events and the message.
    fn script_simulate_refused(rpc: &ScriptedRpc, code: u32) {
        rpc.expect(
            "simulateTransaction",
            json!({"error": format!("HostError: Error(Contract, #{code})"),
                   "events": [crate::chain::script::diagnostic_error_b64(code)],
                   "latestLedger": 1_u32}),
        );
    }

    /// A `Positions` ledger entry naming only a liability, on reserve
    /// index 1, and no collateral at all — the one shape `decide_one`
    /// calls bad debt outright, with no health-factor arithmetic and no
    /// percent walk to script around: `liability_base > 0 &&
    /// collateral_base == 0`.
    fn bad_debt_positions_entry_xdr(account: &str) -> String {
        positions_entry_xdr(account, &[(1, 10_000_000_000)])
    }

    /// A `Positions` ledger entry with `liabilities` and nothing else.
    fn positions_entry_xdr(account: &str, liabilities: &[(u32, i128)]) -> String {
        let side = |amounts: &[(u32, i128)]| {
            map(amounts
                .iter()
                .map(|(index, amount)| (ScVal::U32(*index), i128_val(*amount)))
                .collect())
            .unwrap()
        };
        let value = map(vec![
            (symbol("collateral").unwrap(), side(&[])),
            (symbol("liabilities").unwrap(), side(liabilities)),
            (symbol("supply").unwrap(), side(&[])),
        ])
        .unwrap();
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(harness::POOL).unwrap(),
            key: sc_vec(vec![
                symbol("Positions").unwrap(),
                address(account).unwrap(),
            ])
            .unwrap(),
            durability: ContractDataDurability::Persistent,
            val: value,
        });
        to_base64(&entry).unwrap()
    }

    /// Scripts one snapshot exactly as `harness::script_snapshot` does —
    /// same reserves, same oracle prices, all from the fixture — except
    /// `account` gets a hand-built, liability-only positions entry instead
    /// of whatever (if anything) the fixture holds for it. Stands in for
    /// `harness::script_snapshot` in the tests here that need a decision
    /// other than `Skip`.
    fn script_snapshot_bad_debt(rpc: &ScriptedRpc, account: &str) {
        script_snapshot_positions(rpc, &[(account, bad_debt_positions_entry_xdr(account))]);
    }

    /// The same, for any number of accounts and any hand-built positions
    /// entry: `positions` pairs an account with the entry the snapshot's
    /// batched read answers for it. An account the batch asks about but
    /// this list does not name is simply absent from the answer, which is
    /// what the RPC does for a key with no entry.
    fn script_snapshot_positions(rpc: &ScriptedRpc, positions: &[(&str, String)]) {
        let fixture = mainnet_fixed_v2();
        let ledger = fixture["ledger"].as_u64().unwrap();
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(harness::POOL).unwrap(), text(&fixture, &["instance_entry_xdr"])),
                entry(&keys::reserve_list(harness::POOL).unwrap(), text(&fixture, &["res_list_entry_xdr"])),
            ]}),
        );
        let mut entries = Vec::new();
        for reserve in fixture["reserves"].as_array().unwrap() {
            let asset = reserve["asset"].as_str().unwrap();
            entries.push(entry(
                &keys::reserve_config(harness::POOL, asset).unwrap(),
                reserve["config_entry_xdr"].as_str().unwrap(),
            ));
            entries.push(entry(
                &keys::reserve_data(harness::POOL, asset).unwrap(),
                reserve["data_entry_xdr"].as_str().unwrap(),
            ));
        }
        for (account, positions_xdr) in positions {
            entries.push(entry(
                &keys::positions(harness::POOL, account).unwrap(),
                positions_xdr,
            ));
        }
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": entries}),
        );
        let ledger = u32::try_from(ledger).unwrap();
        rpc.expect(
            "simulateTransaction",
            simulation(text(&fixture, &["oracle_decimals_return_xdr"]), ledger),
        );
        for reserve in fixture["reserves"].as_array().unwrap() {
            rpc.expect(
                "simulateTransaction",
                simulation(reserve["lastprice_return_xdr"].as_str().unwrap(), ledger),
            );
        }
    }

    /// A tick flags the accounts its events named, durably: the
    /// auctioneer's input survives a restart, which the in-memory
    /// `pending` set it replaces did not.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_tick_flags_the_accounts_its_events_named(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, _tick_rx) = tick_watch();
        let mut state = LoopState::default();

        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            borrow(harness::POOL, harness::USER_ONE),
        )
        .await
        .expect("apply the event");
        assert_eq!(
            store
                .user(harness::POOL, harness::USER_ONE)
                .await
                .expect("read"),
            None,
            "an event writes no row yet: there is nothing to flag until the tick values it"
        );

        let tick = harness::fixture_tick();
        let (message, applied) = tick_message(harness::POOL, tick);
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            message,
        )
        .await
        .expect("apply the tick");
        assert!(applied.await.is_ok());

        // Read back from the store, not from anything the tick left in
        // memory: this is the durability the in-memory `pending` set this
        // flag replaces as the auctioneer's input never had.
        let flagged = store
            .users_needing_recheck(harness::POOL, 10)
            .await
            .expect("read the recheck queue");
        assert_eq!(
            flagged
                .iter()
                .map(|user| user.account.clone())
                .collect::<Vec<_>>(),
            vec![harness::USER_ONE.to_string()],
            "the account this tick's event named is flagged for an auctioneer decision"
        );
        assert_eq!(flagged[0].recheck_ledger, Some(tick.sequence));
        Ok(())
    }

    /// The auctioneer's pass clears only the flag it saw, so a flag raised
    /// while it was deciding survives the decision that did not account
    /// for it. `Store::clear_recheck`'s own conditional-clear semantics
    /// are `store.rs`'s to prove; this test pins the service layer's half:
    /// that it always passes the ledger the *batch read* saw, never
    /// `tick`'s own ledger — the two are deliberately different values
    /// here, so a service that clears with the wrong one fails this test.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_pass_clears_only_the_flag_it_decided_on(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        // One `decide` call per `recheck_batch` call below.
        harness::script_snapshot(&rpc, &[harness::USER_TWO]);
        harness::script_snapshot(&rpc, &[harness::USER_TWO]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), None);
        let (_flag, shutdown) = watch::channel(false);
        // Deliberately not the ledger any batch was flagged at, so a
        // service that clears with `tick.sequence` instead of the batch's
        // own value would clear a flag this test never raised at that
        // ledger — and the assertions below would catch it either way.
        let tick = harness::fixture_tick();

        // `flag_recheck` is an `UPDATE`, not an upsert: the row must
        // already exist, exactly as the tracker's own refresh would have
        // written it.
        store
            .upsert_user(&tracked_user(harness::USER_TWO, tick.sequence))
            .await
            .expect("seed the row");
        store
            .flag_recheck(harness::POOL, harness::USER_TWO, 100)
            .await
            .expect("flag");
        let stale_batch = store
            .users_needing_recheck(harness::POOL, 10)
            .await
            .expect("read");
        assert_eq!(stale_batch[0].recheck_ledger, Some(100));

        // A newer flag arrives — a fresh event, say — while this batch is
        // still the one being decided on.
        store
            .flag_recheck(harness::POOL, harness::USER_TWO, 200)
            .await
            .expect("reflag");

        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &stale_batch,
            tick,
            None,
            &shutdown,
        )
        .await
        .expect("a healthy borrower's pass never fails");
        assert_eq!(
            store
                .users_needing_recheck(harness::POOL, 10)
                .await
                .expect("read")[0]
                .recheck_ledger,
            Some(200),
            "clearing the flag this pass saw (100) must not clear the newer one (200)"
        );

        // A pass that reads the *current* flag clears it normally: the
        // conditional clear is about a stale value, not about refusing to
        // clear at all.
        let fresh_batch = store
            .users_needing_recheck(harness::POOL, 10)
            .await
            .expect("read");
        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &fresh_batch,
            tick,
            None,
            &shutdown,
        )
        .await
        .expect("a second pass");
        assert!(
            store
                .users_needing_recheck(harness::POOL, 10)
                .await
                .expect("read")
                .is_empty(),
            "a pass that decided on the current flag clears it"
        );
        Ok(())
    }

    /// A borrower `decide` could not decide keeps its flag, but the flag
    /// **moves forward** to this pass's ledger: `users_needing_recheck`
    /// orders `recheck_ledger ASC`, so a flag left where it was is the
    /// oldest in the pool and comes back at the head of every following
    /// batch for ever. The ordering, not the mere presence of a flag, is
    /// what this asserts — a flag that is still set proves nothing about
    /// the queue behind it.
    ///
    /// The undecidable borrower here holds a liability in reserve index
    /// 99, which the fixture's pool does not have, so `position_data`
    /// fails for it and for nothing else in the batch. That is the shape
    /// an oracle which stops pricing a reserve produces for every
    /// borrower holding it at once.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_undecidable_borrower_moves_to_the_back_of_the_queue(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let stuck = synthetic_account(70);
        let decidable = synthetic_account(71);
        let behind = synthetic_account(72);
        script_snapshot_positions(
            &rpc,
            &[
                (
                    stuck.as_str(),
                    positions_entry_xdr(&stuck, &[(99, 10_000_000_000)]),
                ),
                (decidable.as_str(), positions_entry_xdr(&decidable, &[])),
            ],
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), None);
        let (_flag, shutdown) = watch::channel(false);

        for (account, ledger) in [(&stuck, 100_u32), (&decidable, 110), (&behind, 120)] {
            store
                .upsert_user(&tracked_user(account, ledger))
                .await
                .expect("seed the row");
            store
                .flag_recheck(harness::POOL, account, ledger)
                .await
                .expect("flag");
        }

        // A batch of two, as `REFRESH_BATCH` bounds it: the oldest two
        // flags. `behind` is flagged after both and waits its turn.
        let batch = store
            .users_needing_recheck(harness::POOL, 2)
            .await
            .expect("read the recheck queue");
        assert_eq!(
            batch.iter().map(|user| &user.account).collect::<Vec<_>>(),
            vec![&stuck, &decidable],
            "the two oldest flags, oldest first"
        );

        let tick = harness::fixture_tick();
        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &batch,
            tick,
            None,
            &shutdown,
        )
        .await
        .expect("one undecidable borrower does not fail the pass");

        assert_eq!(
            store
                .user(harness::POOL, &decidable)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            None,
            "the borrower that was decided had its own flag cleared"
        );
        assert_eq!(
            store
                .user(harness::POOL, &stuck)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence),
            "the borrower nothing could decide is still flagged, at this pass's ledger"
        );
        let queue: Vec<String> = store
            .users_needing_recheck(harness::POOL, 10)
            .await
            .expect("read the recheck queue")
            .into_iter()
            .map(|user| user.account)
            .collect();
        assert_eq!(
            queue,
            vec![behind.clone(), stuck.clone()],
            "the undecidable borrower now sorts behind one flagged after it; left \
             where it was it would still be first, and first in every batch after this"
        );
        Ok(())
    }

    /// The starvation itself: with a batch of one, a borrower queued
    /// behind an undecidable one **is reached** on the next pass. This is
    /// the assertion that fails if a flag is left where it was — the pass
    /// would read the same undecidable borrower for ever and no other
    /// borrower in the pool would be decided again.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_borrower_behind_an_undecidable_one_is_reached(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let stuck = synthetic_account(73);
        let next = synthetic_account(74);
        // One snapshot per pass: the first batch holds `stuck`, the
        // second — if the queue moved at all — holds `next`.
        script_snapshot_positions(
            &rpc,
            &[(
                stuck.as_str(),
                positions_entry_xdr(&stuck, &[(99, 10_000_000_000)]),
            )],
        );
        script_snapshot_positions(&rpc, &[(next.as_str(), positions_entry_xdr(&next, &[]))]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), None);
        let (_flag, shutdown) = watch::channel(false);

        for (account, ledger) in [(&stuck, 100_u32), (&next, 110)] {
            store
                .upsert_user(&tracked_user(account, ledger))
                .await
                .expect("seed the row");
            store
                .flag_recheck(harness::POOL, account, ledger)
                .await
                .expect("flag");
        }

        let tick = harness::fixture_tick();
        let first = store
            .users_needing_recheck(harness::POOL, 1)
            .await
            .expect("read the recheck queue");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].account, stuck, "the oldest flag comes first");
        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &first,
            tick,
            None,
            &shutdown,
        )
        .await
        .expect("pass one");

        let next_tick = LedgerTick {
            sequence: tick.sequence + 1,
            close_time: tick.close_time,
        };
        let second = store
            .users_needing_recheck(harness::POOL, 1)
            .await
            .expect("read the recheck queue");
        assert_eq!(second.len(), 1);
        assert_eq!(
            second[0].account, next,
            "the second pass reaches the borrower queued behind the undecidable one"
        );
        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &second,
            next_tick,
            None,
            &shutdown,
        )
        .await
        .expect("pass two");

        assert_eq!(
            store
                .user(harness::POOL, &next)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            None,
            "and decides it: its flag is cleared"
        );
        assert_eq!(
            store
                .user(harness::POOL, &stuck)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence),
            "the undecidable borrower is retried later, not dropped"
        );
        Ok(())
    }

    /// The other half of the same rule: a borrower whose `act` failed —
    /// a chain error under the simulation, not a store error — is
    /// re-flagged at this pass's ledger too, for the same reason.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_borrower_whose_action_failed_moves_forward_too(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_debtor();
        let behind = synthetic_account(75);
        // Decidable — bad debt outright — and then `act`'s own
        // `simulate_only` reads the signer's account entry, which is the
        // third `getLedgerEntries` of the pass and gets a 503.
        script_snapshot_bad_debt(&rpc, &account);
        rpc.expect_http("getLedgerEntries", 503);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let signer = test_signer();
        let network = Network::testnet();
        let submitter = Submitter::new(&client, &network, &signer, test_tx_config());
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), Some(submitter));
        let (_flag, shutdown) = watch::channel(false);

        for (account, ledger) in [(&account, 100_u32), (&behind, 120)] {
            store
                .upsert_user(&tracked_user(account, ledger))
                .await
                .expect("seed the row");
            store
                .flag_recheck(harness::POOL, account, ledger)
                .await
                .expect("flag");
        }

        let batch = store
            .users_needing_recheck(harness::POOL, 1)
            .await
            .expect("read the recheck queue");
        assert_eq!(batch.len(), 1);
        let tick = harness::fixture_tick();
        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &batch,
            tick,
            None,
            &shutdown,
        )
        .await
        .expect("a failed action is this borrower's failure, not the pass's");

        assert_eq!(
            store
                .user(harness::POOL, &account)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence),
            "still flagged, and at this pass's ledger"
        );
        let queue: Vec<String> = store
            .users_needing_recheck(harness::POOL, 10)
            .await
            .expect("read the recheck queue")
            .into_iter()
            .map(|user| user.account)
            .collect();
        assert_eq!(
            queue,
            vec![behind.clone(), account.clone()],
            "and sorts behind the borrower flagged after it, rather than ahead of it"
        );
        Ok(())
    }

    /// A borrower the contract *refused* keeps its flag, and moves forward
    /// exactly as an undecidable one does.
    ///
    /// This is the case that used to look like success. `act` answered
    /// `Ok(None)` both for "there was nothing to do" and for "I wanted to
    /// act and could not", and the caller cleared the flag either way — so
    /// a borrower this bot believes is liquidatable dropped out of the
    /// recheck queue until an event, a price move or the full scan's
    /// ~1200-ledger period named it again.
    ///
    /// The refusal scripted here is the pool's own `AuctionInProgress`
    /// (1212), the benign one; the one that is not benign — a percent walk
    /// that ran out of iterations a point short of the contract's band —
    /// reaches the same arm, because both are `ActOutcome::Refused`.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_borrower_the_contract_refused_keeps_its_flag(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_debtor();
        let behind = synthetic_account(76);
        let signer = test_signer();
        // Decidable — bad debt outright — then `simulate_only`'s own
        // source-account read, then a contract refusal.
        script_snapshot_bad_debt(&rpc, &account);
        script_account_entry(&rpc, &signer);
        script_simulate_refused(&rpc, 1_212);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let network = Network::testnet();
        let submitter = Submitter::new(&client, &network, &signer, test_tx_config());
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), Some(submitter));
        let (_flag, shutdown) = watch::channel(false);

        for (account, ledger) in [(&account, 100_u32), (&behind, 120)] {
            store
                .upsert_user(&tracked_user(account, ledger))
                .await
                .expect("seed the row");
            store
                .flag_recheck(harness::POOL, account, ledger)
                .await
                .expect("flag");
        }

        let batch = store
            .users_needing_recheck(harness::POOL, 1)
            .await
            .expect("read the recheck queue");
        assert_eq!(batch.len(), 1);
        let tick = harness::fixture_tick();
        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &batch,
            tick,
            None,
            &shutdown,
        )
        .await
        .expect("a refusal is this borrower's answer, not the pass's failure");

        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "nothing was sent: the contract refused at simulation"
        );
        assert_eq!(
            store
                .user(harness::POOL, &account)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence),
            "a refused borrower keeps its flag, raised to this pass's ledger — not \
             cleared as though nothing had been owed"
        );
        let queue: Vec<String> = store
            .users_needing_recheck(harness::POOL, 10)
            .await
            .expect("read the recheck queue")
            .into_iter()
            .map(|user| user.account)
            .collect();
        assert_eq!(
            queue,
            vec![behind.clone(), account.clone()],
            "and sorts behind the borrower flagged after it, so the retry costs one \
             batch slot per pass rather than the whole batch"
        );
        Ok(())
    }

    /// A healthy borrower is a `Skipped`, not a `Refused`, so its flag is
    /// cleared: the distinction must not have turned every answer into a
    /// reason to keep rechecking, which would starve the queue just as
    /// surely as clearing every answer hid a liquidatable borrower.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_skipped_borrower_still_has_its_flag_cleared(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        // The fixture's own borrower, whose health factor is above the
        // threshold: `Decision::Skip(SkipReason::Healthy)`.
        harness::script_snapshot(&rpc, &[harness::USER_TWO]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), None);
        let (_flag, shutdown) = watch::channel(false);
        let tick = harness::fixture_tick();

        store
            .upsert_user(&tracked_user(harness::USER_TWO, tick.sequence))
            .await
            .expect("seed the row");
        store
            .flag_recheck(harness::POOL, harness::USER_TWO, 100)
            .await
            .expect("flag");
        let batch = store
            .users_needing_recheck(harness::POOL, 10)
            .await
            .expect("read");

        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &batch,
            tick,
            None,
            &shutdown,
        )
        .await
        .expect("a healthy borrower's pass never fails");
        assert!(
            store
                .users_needing_recheck(harness::POOL, 10)
                .await
                .expect("read")
                .is_empty(),
            "nothing was owed, so the flag is cleared"
        );
        Ok(())
    }

    /// One task's error must not abort another task's in-flight work.
    ///
    /// Returning on the first `Err` drops the `JoinSet`, and dropping a
    /// `JoinSet` aborts every task still in it — including `run_queue`,
    /// possibly between `sendTransaction` and the `getTransaction` poll
    /// that learns the outcome, which is the one thing that module's doc
    /// says must never happen. The stand-in below returns only once the
    /// shutdown flag is raised and records that it got to return at all;
    /// an aborted task never reaches that store, so the assertion
    /// distinguishes "drained" from "cancelled" rather than merely
    /// observing an error came back.
    #[tokio::test]
    async fn a_failing_task_shuts_the_rest_down_instead_of_aborting_them() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let finished = Arc::new(AtomicBool::new(false));
        let mut tasks: JoinSet<Result<(), LiquidatorError>> = JoinSet::new();

        let mut watcher = shutdown_rx.clone();
        let flag = Arc::clone(&finished);
        tasks.spawn(async move {
            while !*watcher.borrow_and_update() {
                if watcher.changed().await.is_err() {
                    break;
                }
            }
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });
        tasks.spawn(async { Err(LiquidatorError::Config("a task failed".to_string())) });

        let error = drain_tasks(tasks, &shutdown_tx)
            .await
            .expect_err("the failure is reported");
        assert!(
            matches!(error, LiquidatorError::Config(_)),
            "and it is the first error, not a shutdown artefact"
        );
        assert!(
            finished.load(Ordering::SeqCst),
            "the other task ran to its own end rather than being aborted mid-flight"
        );
        assert!(
            *shutdown_rx.borrow(),
            "the failure raised the shutdown flag, so the error path and the signal \
             path drain by the same route"
        );
    }

    /// The full scan flags every user below the threshold, across pages: a
    /// borrower on the second page is exactly the one a single-page scan
    /// would have missed. And it observes shutdown as it pages, so a pool
    /// with thousands of borrowers below the threshold does not flag all
    /// of them before shutdown is next looked at.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_full_scan_flags_every_user_below_the_threshold(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        for (account, health) in [
            ("A", 1_000_000_i128),
            ("B", 2_000_000),
            ("C", 2_000_000),
            ("D", 3_000_000),
        ] {
            store
                .upsert_user(&TrackedUser {
                    pool: harness::POOL.to_string(),
                    account: account.to_string(),
                    health_factor: health,
                    collateral: BTreeMap::new(),
                    liabilities: BTreeMap::from([(0, 1)]),
                    updated_ledger: 10,
                    recheck_ledger: None,
                })
                .await
                .expect("upsert");
        }
        // At or above the threshold: never flagged.
        store
            .upsert_user(&TrackedUser {
                pool: harness::POOL.to_string(),
                account: "HEALTHY".to_string(),
                health_factor: 20_000_000,
                collateral: BTreeMap::new(),
                liabilities: BTreeMap::from([(0, 1)]),
                updated_ledger: 10,
                recheck_ledger: None,
            })
            .await
            .expect("upsert");

        // A page size of 1 forces every one of the four qualifying users
        // onto its own page: a scan that flagged only the first page would
        // miss three of the four.
        let (flag, shutdown) = watch::channel(false);
        let flagged = full_scan_and_flag(&store, harness::POOL, 5_000_000, 999, 1, &shutdown)
            .await
            .expect("full scan");
        assert_eq!(flagged, 4);

        for account in ["A", "B", "C", "D"] {
            assert_eq!(
                store
                    .user(harness::POOL, account)
                    .await
                    .expect("read")
                    .and_then(|user| user.recheck_ledger),
                Some(999),
                "{account} is below the threshold and must be flagged, second page included"
            );
        }
        assert_eq!(
            store
                .user(harness::POOL, "HEALTHY")
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            None,
            "a user at or above the threshold is never flagged"
        );

        // Shutdown is observed between pages, so a pool with thousands of
        // borrowers below the threshold stops where it is rather than
        // paging through all of them first. With the flag already up the
        // scan does nothing at all: the check is at the head of the loop,
        // which is the same check every later page passes through.
        for account in ["A", "B", "C", "D"] {
            assert!(
                store
                    .clear_recheck(harness::POOL, account, 999)
                    .await
                    .expect("clear"),
                "the flag this scan raised is cleared before the next one"
            );
        }
        flag.send(true).expect("shut down");
        let flagged = full_scan_and_flag(&store, harness::POOL, 5_000_000, 1_000, 1, &shutdown)
            .await
            .expect("full scan under shutdown");
        assert_eq!(flagged, 0, "a scan under shutdown flags nothing");
        for account in ["A", "B", "C", "D"] {
            assert_eq!(
                store
                    .user(harness::POOL, account)
                    .await
                    .expect("read")
                    .and_then(|user| user.recheck_ledger),
                None,
                "{account} was not flagged by a scan that stopped for shutdown"
            );
        }
        Ok(())
    }

    /// Before `STARTUP_DELAY_LEDGERS` has elapsed nothing is submitted,
    /// even armed: a bot that has just started has the least state and the
    /// most reason to be wrong. Proven against a real `SubmissionQueue`
    /// and a real bad-debt decision, with only the deepest leaf — an
    /// actual submitted transaction — stood in for, exactly as
    /// `queue.rs`'s own tests stand in for `Submitter::submit`.
    #[sqlx::test(migrations = "./migrations")]
    async fn no_submission_before_the_startup_delay(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_debtor();
        // One `decide` call per `auctioneer_tick` call below.
        script_snapshot_bad_debt(&rpc, &account);
        script_snapshot_bad_debt(&rpc, &account);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), None);
        let (_flag, shutdown) = watch::channel(false);

        let (queue, mut queue_rx) = SubmissionQueue::new(8);
        let worker = tokio::spawn(async move {
            while let Some(queued) = queue_rx.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash: TxHash([9_u8; 32]),
                    ledger: 1,
                    return_value: None,
                }));
            }
        });

        let cadence = AuctioneerCadence {
            refresh_batch: 10,
            oracle_scan_ledgers: 0,
            oracle_phase: 0,
            full_scan_ledgers: 0,
            full_phase: 0,
            scan_health_factor: 0,
            price_delta_bps: 0,
            startup_delay_ledgers: 1,
        };
        let pools = vec![harness::POOL.to_string()];
        let ctx = AuctioneerContext {
            store: &store,
            pools: &pools,
            auctioneer: &auctioneer,
            cadence,
            submission_queue: Some(&queue),
            shutdown: &shutdown,
        };
        let mut state = AuctioneerState::default();
        let tick = harness::fixture_tick();

        // `flag_recheck` is an `UPDATE`, not an upsert: the row must
        // already exist, exactly as the tracker's own refresh would have
        // written it.
        store
            .upsert_user(&tracked_user(&account, tick.sequence))
            .await
            .expect("seed the row");
        store
            .flag_recheck(harness::POOL, &account, tick.sequence)
            .await
            .expect("flag");

        // Tick one is the ledger the delay is measured from: nothing has
        // elapsed yet (`0 >= startup_delay_ledgers (1)` is false), so
        // submissions stay locked even though the queue is right there
        // and the decision is a real bad debt.
        auctioneer_tick(&ctx, tick, &mut state)
            .await
            .expect("tick one");
        assert!(
            !state.submissions_unlocked,
            "the first tick is still inside the delay"
        );

        let recorded = sqlx::query!(
            "SELECT dry_run FROM creations WHERE pool = $1 AND account = $2 ORDER BY id",
            harness::POOL,
            account,
        )
        .fetch_all(store.pool())
        .await
        .expect("read the creation");
        assert_eq!(recorded.len(), 1, "the decision was still recorded");
        assert!(
            recorded[0].dry_run,
            "recorded as dry-run even though a live queue is configured: the startup \
             delay has not elapsed"
        );

        // Re-flag: this is the same unresolved bad debt, and in a real bot
        // some later event, oracle move or full scan would raise this
        // again anyway.
        store
            .flag_recheck(harness::POOL, &account, tick.sequence)
            .await
            .expect("reflag");
        // One ledger on: the chain, not the wakeup count, is what the
        // delay measures, so this is exactly `startup_delay_ledgers`.
        let tick_two = LedgerTick {
            sequence: tick.sequence + 1,
            close_time: tick.close_time,
        };
        auctioneer_tick(&ctx, tick_two, &mut state)
            .await
            .expect("tick two");
        assert!(
            state.submissions_unlocked,
            "the second tick is past the delay"
        );

        let recorded = sqlx::query!(
            "SELECT dry_run FROM creations WHERE pool = $1 AND account = $2 ORDER BY id",
            harness::POOL,
            account,
        )
        .fetch_all(store.pool())
        .await
        .expect("read the creations");
        assert_eq!(recorded.len(), 2);
        assert!(
            !recorded[1].dry_run,
            "once the delay has elapsed, the same decision is actually submitted"
        );

        drop(queue);
        worker.await.expect("worker");
        Ok(())
    }

    /// The auctioneer failing does not stall the cursor: a decision is not
    /// a stored effect of a ledger, so the tick that named the
    /// auctioneer's own input is acknowledged and published *before* the
    /// auctioneer is ever given a chance to run — and a later tick is
    /// acknowledged too, proving the earlier failure poisoned nothing
    /// downstream either.
    ///
    /// What pins "the auctioneer is never on the acknowledgement path" is
    /// the call count, not the order of the assertions: the `Tick` arm
    /// makes exactly the two `getLedgerEntries` calls its own refresh
    /// scripts, and `ScriptedRpc` records every request before it decides
    /// how to answer it. An auctioneer called from inside `apply_tick`
    /// would read a snapshot of its own for the account this tick just
    /// flagged, so it would show up here as a third call — whether its
    /// error propagated, was logged and swallowed, or never happened at
    /// all.
    ///
    /// The 503 below proves something narrower, and only that: it is
    /// scripted *after* the tick was applied, so it says nothing about an
    /// inline auctioneer (an unscripted method answers HTTP 500, which an
    /// inline port keeping this module's log-and-continue split would
    /// swallow). What it proves is that a chain failure inside
    /// `recheck_batch` is logged rather than propagated, and that the
    /// borrower it failed on keeps a flag.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_auctioneer_failure_does_not_stall_the_cursor(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, tick_rx) = tick_watch();
        let mut state = LoopState::default();

        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            borrow(harness::POOL, harness::USER_ONE),
        )
        .await
        .expect("apply the event");

        let tick = harness::fixture_tick();
        let (message, applied) = tick_message(harness::POOL, tick);
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            message,
        )
        .await
        .expect("apply the tick");

        // The cursor's own proof, entirely independent of whatever the
        // auctioneer does next: the tracker has already acknowledged and
        // published.
        assert!(
            applied.await.is_ok(),
            "the tick is acknowledged before the auctioneer ever runs"
        );
        assert_eq!(
            *tick_rx.borrow(),
            tick,
            "and published to the auctioneer's watch"
        );
        assert_eq!(
            store
                .user(harness::POOL, harness::USER_ONE)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence),
            "the tick flagged the account: this is the auctioneer's own input"
        );

        // And the pin on the constraint itself: `harness::script_snapshot`
        // scripts one snapshot — the pool's shape, then the batched
        // entries — so two is every chain read the refresh makes and a
        // third would be the auctioneer's own.
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            2,
            "the tick made its refresh's own two reads and no other chain call: \
             an auctioneer called from inside `apply_tick` would read a snapshot here"
        );

        // Now the auctioneer runs against that exact tick, and fails: its
        // own snapshot read gets a 503.
        rpc.expect_http("getLedgerEntries", 503);
        let batch = store
            .users_needing_recheck(harness::POOL, 10)
            .await
            .expect("read the recheck queue");
        assert_eq!(batch.len(), 1);
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), None);
        let result = recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &batch,
            tick,
            None,
            &shutdown,
        )
        .await;
        assert!(
            result.is_ok(),
            "a chain failure deciding this pool's batch is logged, not propagated: {result:?}"
        );
        assert_eq!(
            store
                .user(harness::POOL, harness::USER_ONE)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence),
            "the flag a failed decision never accounted for stays up, at this tick's \
             ledger, for a later pass"
        );

        // And the tracker's cursor keeps moving: a later tick, with
        // nothing new to refresh, is acknowledged exactly as any other
        // clean tick would be. The auctioneer's failure a moment ago
        // poisoned nothing in this path.
        let next = LedgerTick {
            sequence: tick.sequence + 1,
            close_time: tick.close_time,
        };
        let (message, applied) = tick_message(harness::POOL, next);
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &tick_tx,
            message,
        )
        .await
        .expect("a later tick applies cleanly");
        assert!(
            applied.await.is_ok(),
            "the cursor keeps moving after the auctioneer's failure"
        );
        Ok(())
    }

    /// The scan cadence fires once per period, cannot skip one, and its
    /// phase depends on the instance so two bots do not fire on the same
    /// ledger.
    #[test]
    fn the_scan_cadence_fires_once_per_period_at_an_instance_specific_phase() {
        let period = 10;

        // The ledgers a walk fires at, given what it last fired at.
        let fired = |phase: u32, from: u32, count: u32, mut last: Option<u32>| -> Vec<u32> {
            let mut out = Vec::new();
            for ledger in from..from + count {
                if scan_due(ledger, last, phase, period) {
                    last = Some(ledger);
                    out.push(ledger);
                }
            }
            out
        };

        // The first tick for a pool scans: the tracked set is worth logging
        // at startup rather than a period later.
        assert_eq!(fired(0, 2_000, 1, None), vec![2_000]);

        // Whatever the phase, two periods after that fire exactly twice.
        for phase in 0..period {
            let ledgers = fired(phase, 2_001, 2 * period, Some(2_000));
            assert_eq!(
                ledgers.len(),
                2,
                "phase {phase} should fire twice over two periods, fired at {ledgers:?}"
            );
        }

        // A tick that never lands on an exact multiple still fires. Ticks
        // are not consecutive — a pass slower than a ledger close skips
        // sequences — and the exact test this replaced dropped the whole
        // period, and that period's reseed retry with it.
        for phase in 0..period {
            assert!(
                scan_due(2_000 + period + 3, Some(2_000), phase, period),
                "phase {phase} must fire after a period even on a skipped sequence"
            );
        }

        // Two instances with different phases fire on different ledgers:
        // the phase is what keeps them from scanning in lockstep.
        assert_ne!(
            fired(2, 1, 3 * period, Some(0)),
            fired(7, 1, 3 * period, Some(0)),
            "different phases must fire on different ledgers"
        );

        // A zero period never fires: `full_scan_ledgers` is validated to be
        // at least 1, but `scan_due` must not divide by zero if it is ever
        // built by hand, e.g. in a test.
        assert!(!scan_due(0, None, 0, 0));
        assert!(!scan_due(100, Some(50), 3, 0));

        // The phase generator stays within the period it was asked for.
        for _ in 0..50 {
            assert!(scan_phase(period) < period);
        }
        assert_eq!(
            scan_phase(0),
            0,
            "a zero period has only one possible phase"
        );
    }
}
