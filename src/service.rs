//! Wiring: validate the configuration, seed the store, follow the
//! configured pools and shut down cleanly.
//!
//! `Service` has two entry points, matching [`crate::config::RunMode`]:
//! [`Service::check_config`] reads the chain and pings the store and
//! reports, writing nothing and following nothing, and [`Service::run`]
//! additionally migrates the store, seeds it, and follows every configured
//! pool until a shutdown signal arrives. Both share `validate`, because a
//! bot that never checked its own configuration would happily submit
//! against a pool it misread.

use std::collections::{BTreeMap, BTreeSet};

use rand::RngExt as _;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::chain::pool::{PoolReader, PoolSnapshot};
use crate::chain::rpc::RpcClient;
use crate::chain::xdr::PoolStatus;
use crate::config::{PoolConfig, SeedConfig, ServiceConfig};
use crate::ledger::{LedgerPoller, LedgerTick, PollerConfig, PollerMessage};
use crate::store::{events_cursor, Store};
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
        let outcome = tracker
            .seed(&pool.address, sources, tick, batch, shutdown)
            .await?;
        if outcome.failed_sources != 0 {
            incomplete.insert(pool.address.clone());
        }
        tracing::info!(
            pool = pool.address,
            tracked = outcome.refresh.tracked,
            failed_sources = outcome.failed_sources,
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
fn scan_due(ledger: u32, phase: u32, period: u32) -> bool {
    period != 0 && ledger.wrapping_add(phase).is_multiple_of(period)
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
/// then the stale-refresh pass, then the full scan when it is due. Every
/// step must succeed before the tick is acknowledged, so this is one
/// fallible unit rather than three.
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
    // `USER_REFRESH_LEDGERS` is a *span*; `Store::users_stale` selects on
    // an absolute ledger. Subtracting here is what turns one into the
    // other: handing the span over as-is compares a ledger count against a
    // ledger sequence, which on any real network is false for every row.
    let updated_before = tick.sequence.saturating_sub(cadence.user_refresh_ledgers);
    tracker
        .refresh_stale(pool, tick, updated_before, cadence.refresh_batch)
        .await?;
    if scan_due(tick.sequence, cadence.phase, cadence.full_scan_ledgers) {
        full_scan(
            tracker,
            seed_sources,
            cadence,
            state,
            shutdown,
            (pool, tick),
        )
        .await?;
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
        .users_below_health(pool, cadence.scan_health_factor, FULL_SCAN_REPORT_LIMIT)
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
async fn tracker_loop(
    tracker: &Tracker<'_>,
    seed_sources: &[SeedSource],
    cadence: Cadence,
    mut state: LoopState,
    shutdown: &watch::Receiver<bool>,
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
fn spawn_shutdown_listener(shutdown: watch::Sender<bool>) {
    tokio::spawn(async move {
        wait_for_signal().await;
        tracing::warn!("shutdown requested; finishing in-flight work");
        let _ = shutdown.send(true);
        wait_for_signal().await;
        tracing::warn!("second shutdown request; exiting immediately");
        std::process::exit(130);
    });
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
    /// channel — until a shutdown signal arrives and every task has
    /// returned.
    pub async fn run(config: ServiceConfig) -> Result<(), LiquidatorError> {
        // Installed before anything that takes time. Seeding a busy pool
        // is tens of seconds of sequential round trips, and until this is
        // in place a `SIGTERM` in that window reaches the default handler
        // and kills the process outright rather than draining it.
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        spawn_shutdown_listener(shutdown_tx);

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
        for pool in &config.pools {
            let rpc = rpc.clone();
            let store = store.clone();
            let pool = pool.address.clone();
            let sender = message_tx.clone();
            let shutdown = shutdown_rx.clone();
            tasks.spawn(async move {
                LedgerPoller::new(&rpc, &store, &pool, poller_config)
                    .run(sender, shutdown)
                    .await
                    .map_err(LiquidatorError::from)
            });
        }
        // Every poller now holds its own sender clone; dropping this one
        // lets the tracker task's channel close, and its `recv` return
        // `None`, once (and only once) every poller has stopped.
        drop(message_tx);

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
                message_rx,
            )
            .await
            .map_err(LiquidatorError::from)
        });

        while let Some(outcome) = tasks.join_next().await {
            match outcome {
                Ok(result) => result?,
                Err(error) => resume_on_panic(error),
            }
        }
        Ok(())
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
    use crate::chain::xdr::PoolEvent;
    use crate::harness;
    use crate::store::TrackedUser;
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
        let mut state = LoopState::default();

        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
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

        let (message, applied) = tick_message(harness::POOL, harness::fixture_tick());
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
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
        let mut state = LoopState::default();
        let cadence = Cadence {
            user_refresh_ledgers: 241_920,
            ..quiet_cadence()
        };

        let (message, applied) = tick_message(harness::POOL, tick);
        handle_message(&tracker, &[], cadence, &mut state, &shutdown, message)
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
        handle_message(&tracker, &sources, cadence, &mut state, &shutdown, message)
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
        Ok(())
    }

    /// The scan cadence fires once per period and its phase depends on the
    /// instance, so two bots do not fire on the same ledger.
    #[test]
    fn the_scan_cadence_fires_once_per_period_at_an_instance_specific_phase() {
        let period = 10;

        // Whatever the phase, exactly one ledger in each period fires.
        for phase in 0..period {
            let fires = (2_000..2_000 + period)
                .filter(|&ledger| scan_due(ledger, phase, period))
                .count();
            assert_eq!(
                fires, 1,
                "phase {phase} should fire exactly once per period"
            );
        }

        // Two instances with different phases fire on different ledgers:
        // the phase is what keeps them from scanning in lockstep.
        let ledgers_at = |phase: u32| -> Vec<u32> {
            (0..period)
                .filter(|&ledger| scan_due(ledger, phase, period))
                .collect()
        };
        assert_ne!(
            ledgers_at(2),
            ledgers_at(7),
            "different phases must fire on different ledgers"
        );

        // A zero period never fires: `full_scan_ledgers` is validated to be
        // at least 1, but `scan_due` must not divide by zero if it is ever
        // built by hand, e.g. in a test.
        assert!(!scan_due(0, 0, 0));
        assert!(!scan_due(100, 3, 0));

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
