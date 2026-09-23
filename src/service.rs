//! Wiring: validate the configuration, seed the store, follow the
//! configured pools, decide and act on what the tracker finds, and shut
//! down cleanly.
//!
//! `Service` has two entry points, matching [`crate::config::RunMode`]:
//! [`Service::check_config`] reads the chain and pings the store and
//! reports, writing nothing and following nothing, and [`Service::run`]
//! additionally migrates the store, seeds it, and follows every configured
//! pool until a shutdown signal arrives. Both share `validate` and
//! `validate_filler`, because a bot that never checked its own
//! configuration would happily submit against a pool it misread, or start
//! armed with a filler account that does not exist.
//!
//! # Seven kinds of task
//!
//! [`Service::run`] spawns one [`LedgerPoller`] per pool, one tracker task
//! consuming their shared channel, one auctioneer task, one filler task,
//! one watchdog task, one HTTP server when a port is set, and — only when
//! armed — one submission-queue worker per distinct signing key. The
//! queues are the subject of `spawn_queues`: one worker per key and never
//! two, because a Soroban transaction is built against its source
//! account's sequence number at prepare time. The watchdog is
//! `watchdog_loop`: it reads the pollers' heartbeats off the run's one
//! [`crate::metrics::Metrics`] and reports a pool that has stopped
//! heartbeating, and it is a task of its own because a poller that has
//! stopped is exactly the thing that cannot report itself. The HTTP
//! server is [`crate::http::serve`] over that same recorder and the
//! store, and it is spawned only when `PORT` or `HTTP_PORT` gave the run
//! an address — and, alone among the tasks, *before* the initial seed,
//! since a seed of a busy pool is tens of seconds during which a restart
//! probe must be able to reach `/livez` at all. Its task answers `Ok(())`
//! however it ends, a bind failure included, because a diagnostics port
//! that cannot open must not stop the bot from trading (spec §8).
//!
//! The run's one [`crate::metrics::Metrics`] and its one
//! [`crate::notifier::Notifier`] — the latter over the Telegram channel
//! when both credentials are configured and over
//! [`crate::notifier::LogChannel`] otherwise, at
//! `config.notification_cooldown` — are both constructed before the seed
//! pass, which is the first thing that records through them and runs
//! before every task but the HTTP server; every loop that needs both is
//! handed them together as one `Instruments`, and the pollers, the
//! watchdog, the HTTP server and the filler are handed those same two
//! instances rather than instances of their own, since dedup state and
//! gauges mean nothing split across copies. `build_notifier` is also the
//! only place the notifier's recorder can be installed at all:
//! [`crate::notifier::Notifier::with_metrics`] works through
//! `Arc::get_mut`, so a notifier that has already spawned a delivery can
//! no longer take one. Delivery is fire-and-forget: `notify` spawns
//! behind a bounded semaphore and never awaits a channel, so no
//! notification can delay a tick, a decision or a fill, and a channel
//! that has stopped answering costs a bounded number of tasks and drops
//! what does not fit.
//!
//! # Every exit of `run` once its tasks run drains
//!
//! `run`'s earlier `?`s — validation and the seed pass — return before
//! `finish_run`, with nothing to drain: nothing before the task set
//! notifies.
//!
//! What a shutdown owes the sends still in flight is
//! [`crate::notifier::Notifier::drain`], and `finish_run` is where both
//! of [`Service::run`]'s exits pay it: a task failure and a shutdown
//! signal join the same way (see `drain_tasks`), so they leave the same
//! way too — every in-flight delivery gets
//! [`crate::notifier::DRAIN_BUDGET`] to finish, a timeout is a warning
//! and nothing more, and the result `drain_tasks` answered is returned
//! unchanged. A task's panic leaves the same way too, resumed only after
//! the drain (see `drain_tasks`). One exit does not drain, deliberately:
//! the *second* `SIGINT`/`SIGTERM` is answered by
//! `spawn_shutdown_listener` with `exit(130)`, because a second signal
//! means now and a drain is exactly the delay it is refusing.
//!
//! # The deciding tasks are joined to the tracker by a tick
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
//! `filler_loop` is a third task on the same watch, and is one for exactly
//! the same reason: a fill is not a stored effect of a ledger either, so
//! planning and executing one must never sit between a tick and the
//! acknowledgement that commits the poller's cursor — and it must never be
//! a second reader of the poller channel, which would break the
//! per-sender ordering that cursor rests on. Each of the two holds its own
//! `StartupGate`, because each measures the delay from the first tick
//! *it* saw and each answers for its own key.
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
use std::num::NonZeroUsize;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::auctioneer::{
    ActOutcome, Auctioneer, AuctioneerConfig, AuctioneerError, CreationOutcome, PriceWatch,
};
use crate::chain::pool::{PoolReader, PoolSnapshot};
use crate::chain::rpc::RpcClient;
use crate::chain::xdr::{PoolEvent, PoolStatus};
use crate::chain::{ChainError, Network, Signer, Submitter, TxConfig, TxOutcome};
use crate::config::{
    HttpConfig, PoolConfig, SeedConfig, ServiceConfig, Signers, SigningKeys, TelegramConfig,
};
use crate::executor::Executor;
use crate::filler::{Filler, FillerConfig, FillerState};
use crate::http::{self, HttpState};
use crate::inventory::Inventory;
use crate::ledger::{heartbeat_while, LedgerPoller, LedgerTick, PollerConfig, PollerMessage};
use crate::metrics::{Attempt, Metrics};
use crate::notifier::telegram::TelegramChannel;
use crate::notifier::{
    LogChannel, Notification, NotificationChannel, NotificationKind, Notifier, Severity,
    DRAIN_BUDGET,
};
use crate::queue::{run_queue, QueueError, SubmissionQueue};
use crate::store::{events_cursor, CreationKind, Cursor, Store, StoreError, TrackedUser};
use crate::tracker::{AnalyticsSeed, FileSeed, SeedSource, Tracker, TrackerError};
use crate::LiquidatorError;

/// The run's instruments: the one [`Metrics`] every loop records into and
/// the one [`Notifier`] every loop reports through, carried together so a
/// loop that needs both takes one parameter rather than two.
///
/// Cloning clones the two handles and never the state behind them: a
/// gauge means nothing split across recorders, and the notifier's dedup
/// means nothing split across notifiers, which is why both are `Arc`s of
/// the instances [`Service::run`] builds exactly once.
///
/// Nothing reached through here may affect trading (spec §8): recording
/// is a lock and an integer, and [`Notifier::notify`] spawns its delivery
/// rather than awaiting a channel, so no tick, decision or fill can be
/// delayed or failed by an instrument.
#[derive(Clone)]
pub(crate) struct Instruments {
    /// The run's counters and gauges.
    pub metrics: Arc<Metrics>,
    /// The run's notification channel.
    pub notifier: Arc<Notifier>,
}

impl Instruments {
    /// A fresh recorder and a log-only notifier whose cooldown no test
    /// runs past: what the tests that call an instrumented loop but
    /// assert on neither instrument pass.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            metrics: Arc::new(Metrics::new()),
            notifier: Arc::new(Notifier::log_only(std::time::Duration::from_hours(1))),
        }
    }
}

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
/// that is neither active nor admin-active, or an asset the oracle does not
/// price, is a warning: neither stops the bot from following the pool, only
/// from acting on it.
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

        // `AdminActive` (0) and `Active` (1) are one thing to this bot.
        // The contract's `Pool::require_action_allowed` refuses borrow and
        // auction-cancel only at a status *above* 1 and supply only above
        // 3, so nothing the bot does — creating an auction, filling one,
        // repaying, withdrawing — can tell 0 from 1. An admin-activated
        // pool is also the ordinary state of a newly deployed one
        // (`set_status(0)` is how a pool leaves Setup), so warning about
        // it teaches an operator to read these warnings as noise, which is
        // the opposite of what they are for.
        let status = snapshot.instance.config.status;
        if status != PoolStatus::Active && status != PoolStatus::AdminActive {
            warnings.push(format!(
                "pool {}: status is {status:?}, not active",
                pool.address
            ));
        }
        // The contract refuses an auction naming more assets than
        // `max_positions`, and every auction names at least one bid and one
        // lot. A pool below two can never have an auction created for it,
        // so following it is pointless — and `plan_liquidation` would
        // otherwise raise its own cap to two and hand the contract a plan
        // it rejects, every tick, silently.
        if snapshot.instance.config.max_positions < 2 {
            return Err(LiquidatorError::Config(format!(
                "pool {}: max_positions is {}, but an auction names at least one bid and \
                 one lot, so no auction could ever be created for it",
                pool.address, snapshot.instance.config.max_positions
            )));
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

/// Checks the things only a bot with a filler key can check (spec §6):
/// that the filler account exists, that it holds `XLM_FEE_RESERVE` of the
/// network's native asset, and — armed — that it holds at least
/// `min_primary_collateral` of each pool's primary asset.
///
/// The first two are failures when armed and warnings in dry-run: an
/// account that does not exist can sign nothing, and a wallet below the
/// fee reserve has nothing left to spend once fees are held back, so an
/// armed bot in either state would fill nothing while reporting itself
/// live. A dry run is allowed both, because it submits nothing either way
/// and refusing to start would make `DRY_RUN=true` harder to run than
/// live trading.
///
/// The third is a warning whether armed or not: too little primary
/// collateral caps how much the filler can take, it does not stop it from
/// taking anything, and the floor is an operator's own target rather than
/// a chain constraint.
///
/// With no filler key there is nothing to check and one warning to make:
/// the filler still plans, against an empty wallet and without simulating
/// (ruling 4). `Args::signing_keys` refuses an armed bot with no filler
/// key, so that path is a dry run's.
///
/// # Errors
///
/// [`LiquidatorError::Config`] for a refusal an operator fixes, and
/// [`LiquidatorError::Chain`] for any other failure of the reads
/// themselves — a validation that cannot read the chain has not passed.
async fn validate_filler(
    rpc: &RpcClient,
    config: &ServiceConfig,
    signing: &SigningContext,
) -> Result<Vec<String>, LiquidatorError> {
    let mut warnings = Vec::new();
    let Some(signer) = signing.signers.filler.as_ref() else {
        warnings.push(
            "no FILLER_SECRET_KEY: the filler plans against an empty inventory and simulates \
             nothing"
                .to_owned(),
        );
        return Ok(warnings);
    };
    let address = signer.address();
    let armed = !config.dry_run;
    let refuse_or_warn = |warnings: &mut Vec<String>, message: String| {
        if armed {
            return Err(LiquidatorError::Config(message));
        }
        warnings.push(message);
        Ok(())
    };

    match rpc.account(address).await {
        Ok(_) => {}
        Err(ChainError::NoAccount(_)) => {
            refuse_or_warn(
                &mut warnings,
                format!(
                    "the filler account {address} does not exist on this network: it must be \
                     funded before it can sign, hold collateral or pay a fee"
                ),
            )?;
            // Nothing further is true of an account that is not there, and
            // every read below would only report the same absence again.
            return Ok(warnings);
        }
        Err(error) => return Err(LiquidatorError::Chain(error)),
    }

    // Any configured pool serves: `balance` is a view call on the *token*
    // contract, and the pool is only what the reader happens to be
    // addressed with. `parse_pools` refuses a configuration with no pool
    // at all, so the `else` is only ever reached by a hand-built one — and
    // a bot with nothing to follow has no wallet to check.
    let Some(any_pool) = config.pools.first() else {
        return Ok(warnings);
    };
    let (_, native_balance) = PoolReader::new(rpc, &any_pool.address)
        .balance(&signing.native_asset, address)
        .await?;
    if native_balance < i128::from(config.xlm_fee_reserve) {
        refuse_or_warn(
            &mut warnings,
            format!(
                "the filler account {address} holds {native_balance} stroops of the native \
                 asset, below the {} stroops XLM_FEE_RESERVE asks for (XLM_FEE_RESERVE is set \
                 in decimal XLM, i.e. XLM_FEE_RESERVE × 10^7 stroops): it could not pay for \
                 the fills it plans",
                config.xlm_fee_reserve
            ),
        )?;
    }

    // Only armed: a dry run plans against whatever the wallet holds and
    // fills nothing, so a pool-by-pool snapshot read per startup would buy
    // nothing but round trips.
    if !armed {
        return Ok(warnings);
    }
    for pool in &config.pools {
        let snapshot = PoolReader::new(rpc, &pool.address)
            .snapshot(std::slice::from_ref(&address))
            .await?;
        match filler_primary_collateral(&snapshot, address, &pool.primary_asset) {
            // The subtraction cannot go negative or overflow: the guard
            // gives `held < min_primary_collateral`, `held` is
            // non-negative because it is a b-token balance through
            // `to_asset_from_b_token`, and `parse_pools` refuses a
            // negative `min_primary_collateral` — so the difference lies
            // in `1..=min_primary_collateral`.
            Ok(held) if held < pool.min_primary_collateral => warnings.push(format!(
                "pool {}: the filler holds {held} of its primary asset {}, {} short of \
                 min_primary_collateral ({})",
                pool.address,
                pool.primary_asset,
                pool.min_primary_collateral - held,
                pool.min_primary_collateral
            )),
            Ok(_) => {}
            Err(error) => warnings.push(format!(
                "pool {}: the filler's primary collateral could not be valued: {error}",
                pool.address
            )),
        }
    }
    Ok(warnings)
}

/// How much of `asset` the filler holds as collateral in `snapshot`, in
/// the asset's own units.
///
/// The b-tokens are converted at the rate the snapshot was **read** with
/// rather than one accrued to a close time, because this answers a
/// startup question — is the wallet within an order of magnitude of the
/// floor an operator set — and not a question any fill is planned against.
/// Zero when the pool holds no position for this account, or does not list
/// the asset at all: `validate` has already refused a primary asset that
/// is not a reserve.
fn filler_primary_collateral(
    snapshot: &PoolSnapshot,
    filler: &str,
    asset: &str,
) -> Result<i128, crate::math::MathError> {
    let Some(index) = snapshot.asset_index.get(asset) else {
        return Ok(0);
    };
    let Some(reserve) = snapshot.reserves.get(index) else {
        return Ok(0);
    };
    let b_tokens = snapshot
        .positions
        .get(filler)
        .and_then(|positions| positions.collateral.get(index))
        .copied()
        .unwrap_or(0);
    reserve.to_asset_from_b_token(b_tokens)
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
///
/// Each pool's seed is wrapped in [`heartbeat_while`] at
/// `heartbeat_interval`, which stamps **every** configured pool — not
/// only the one being seeded. This pass runs before any poller exists to
/// heartbeat for itself, so without it `/livez` would have nothing but
/// [`crate::http::HttpState::started`]'s baseline to go on for the whole
/// of a seed that can outlast it, and a restart probe would kill the bot
/// in the middle of the seed it would then start again — for the pool
/// being seeded if nothing stamped, and for every pool the pass has not
/// reached, or skipped as already followed, if only that one did.
// One collaborator per parameter — two clients, the pools, the seed
// sources, the batch rate, the heartbeat cadence, the shutdown flag and
// the run's instruments — and no two of them belong together in a type of
// their own.
#[allow(clippy::too_many_arguments)]
async fn seed_pools_needing_it(
    rpc: &RpcClient,
    store: &Store,
    pools: &[PoolConfig],
    sources: &[SeedSource],
    batch: u32,
    heartbeat_interval: std::time::Duration,
    shutdown: &watch::Receiver<bool>,
    instruments: &Instruments,
) -> Result<BTreeSet<String>, LiquidatorError> {
    let tracker = Tracker::new(rpc, store);
    let mut incomplete = BTreeSet::new();
    // Every configured pool, stamped by whichever pool's seed is running:
    // no poller exists yet for any of them, so a heartbeat only for the
    // one being seeded would leave the rest measured from
    // `HttpState::started` alone and fail `/livez` for the pool this pass
    // has not reached — the same restart loop, one pool wider.
    let addresses: Vec<&str> = pools.iter().map(|pool| pool.address.as_str()).collect();
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
        let outcome = match heartbeat_while(
            Some(&instruments.metrics),
            &addresses,
            heartbeat_interval,
            tracker.seed(&pool.address, sources, tick, batch, shutdown),
        )
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
        // This pool's seed gauge, and only it: the tracked-user count at
        // the top of this loop was read *before* the seed, so it is stale
        // by the time the seed finishes — a pool with users but no cursor
        // is seeded on a count that was never zero, and one seeded from
        // empty has stopped being zero by here. `users_tracked` is set
        // from a count taken after it.
        instruments
            .metrics
            .seed_accounts_loaded(&pool.address, outcome.refresh.tracked);
        regauge_users(store, instruments, &pool.address).await?;
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

/// Builds the configured Telegram channel: the token goes to
/// [`TelegramChannel::new`] and nowhere else, and a channel that cannot be
/// built at all is a configuration failure.
///
/// The error text is safe to log: every [`crate::notifier::NotifyError`]
/// this channel produces has already been through
/// [`reqwest::Error::without_url`] or is built from Telegram's own
/// `description` field, because the bot token sits in the request *path*
/// (see [`crate::notifier::telegram`]).
fn telegram_channel(config: &TelegramConfig) -> Result<TelegramChannel, LiquidatorError> {
    let channel = TelegramChannel::new(config.token.clone(), config.chat_id.clone())
        .map_err(|error| LiquidatorError::Config(format!("telegram: {error}")))?;
    Ok(match &config.base_url {
        Some(base) => channel.with_base_url(base.clone()),
        None => channel,
    })
}

/// The run's one [`Notifier`]: the Telegram channel when both credentials
/// are configured, [`LogChannel`] otherwise, counting every delivery on
/// `metrics`.
///
/// Called exactly once per run, and **before anything notifies**:
/// [`Notifier::with_metrics`] installs through `Arc::get_mut`, so a
/// notifier that has already spawned a delivery can no longer be given a
/// recorder — it says so and counts nothing.
///
/// Logs which channel is in use and nothing else about it: the token is a
/// secret, and the chat id is one more identifier a log line has no reason
/// to carry.
fn build_notifier(
    config: &ServiceConfig,
    metrics: Arc<Metrics>,
) -> Result<Notifier, LiquidatorError> {
    let channel: Box<dyn NotificationChannel> = match &config.telegram {
        Some(telegram) => Box::new(telegram_channel(telegram)?),
        None => Box::new(LogChannel),
    };
    let name = channel.name();
    let notifier = Notifier::new(channel, config.notification_cooldown).with_metrics(metrics);
    tracing::info!(
        channel = name,
        cooldown_secs = config.notification_cooldown.as_secs(),
        "notification channel"
    );
    Ok(notifier)
}

/// Proves the configured Telegram credentials work, through the one Bot
/// API call that reads nothing and changes nothing: `getMe`.
///
/// A refusal is a [`LiquidatorError::Config`] — exit 2, the code spec §10
/// gives a configuration the operator must fix — and never carries the
/// token. The only side effect [`Service::check_config`] has beyond its
/// reads, and it is a read too.
async fn verify_telegram(config: &ServiceConfig) -> Result<(), LiquidatorError> {
    let Some(telegram) = &config.telegram else {
        tracing::info!(
            "no TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID: notifications are written to the log only"
        );
        return Ok(());
    };
    let username = telegram_channel(telegram)?
        .verify()
        .await
        .map_err(|error| LiquidatorError::Config(format!("telegram: {error}")))?;
    tracing::info!(username, "telegram reachable");
    Ok(())
}

/// Reports whether the diagnostics server is configured, and where. Shared
/// by both entry points so `check-config` says exactly what `run` would.
fn log_http(http: Option<HttpConfig>) {
    if let Some(http) = http {
        tracing::info!(
            bind = %http.bind,
            max_lag_ledgers = http.max_lag_ledgers,
            "serving /healthz, /livez and /metrics"
        );
    } else {
        tracing::info!("neither PORT nor HTTP_PORT is set: /healthz, /livez and /metrics are off");
    }
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
#[allow(clippy::too_many_arguments)]
async fn handle_message(
    tracker: &Tracker<'_>,
    seed_sources: &[SeedSource],
    cadence: Cadence,
    state: &mut LoopState,
    shutdown: &watch::Receiver<bool>,
    instruments: &Instruments,
    tick_tx: &watch::Sender<LedgerTick>,
    message: PollerMessage,
) -> Result<(), TrackerError> {
    match message {
        PollerMessage::Event {
            pool,
            ledger,
            event,
        } => match tracker.apply(&pool, ledger, &event).await {
            Ok(accounts) => {
                instruments.metrics.events_processed(&pool, 1);
                // The fork's `bad_debt` event has zero call sites, so the
                // contract this bot is written against can never emit it:
                // seeing one means the pool is running stock wasm, and
                // every assumption this bot makes about default handling
                // is void. An alarm, never a decision input — the tracker
                // applies the event exactly as it would any other and the
                // tick proceeds.
                if let PoolEvent::BadDebt { user, asset, .. } = &event {
                    instruments.notifier.notify(Notification {
                        kind: NotificationKind::StockWasmDetected,
                        severity: Severity::High,
                        pool: pool.clone(),
                        account: Some(user.clone()),
                        message: format!(
                            "a bad_debt event named {asset}: this pool is not running the \
                             ADR-0008 fork, whose bad_debt event has no call sites — the \
                             bot's default-handling assumptions do not hold here"
                        ),
                    });
                }
                state.pending.entry(pool).or_default().extend(accounts);
            }
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
                instruments,
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
                // The acknowledged path and nowhere else: this gauge is
                // the cursor the poller commits, and a tick whose
                // acknowledgement is declined leaves that cursor where it
                // was, for the whole range to be read again.
                instruments.metrics.ledger_processed(&pool, tick.sequence);
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
                //
                // Monotonic across pools: every pool's poller publishes to
                // this one watch, and a poller that fell behind would
                // otherwise overwrite a newer ledger with an older one —
                // moving the auctioneer's clock backwards, and with it the
                // instant every pool's flagged borrowers are valued at. A
                // ledger sequence is the network's, not the pool's, so
                // "newer" is well-defined across pools. `false` here means
                // nothing changed and no receiver is woken.
                tick_tx.send_if_modified(|current| {
                    if tick.sequence > current.sequence {
                        *current = tick;
                        true
                    } else {
                        false
                    }
                });
            }
        }
        PollerMessage::Gap { pool, from, oldest } => {
            tracing::warn!(pool, from, oldest, "reseeding after a gap");
            // Reported before the reseed, not after it: reseeding a busy
            // pool is tens of seconds of sequential round trips, and the
            // operator should learn that a range of events was lost while
            // it is being made good rather than once it has been.
            instruments.notifier.notify(Notification {
                kind: NotificationKind::EventGap,
                severity: Severity::High,
                pool: pool.clone(),
                account: None,
                message: format!("events from ledger {from} to {oldest} are gone; reseeding"),
            });
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
            regauge_users(tracker.store(), instruments, &pool).await?;
            if outcome.failed_sources == 0 {
                state.needs_reseed.remove(&pool);
            }
        }
    }
    Ok(())
}

/// Sets `users_tracked{pool}` from the store's count as it stands now.
///
/// Called wherever a `users` row can have been inserted or deleted — after
/// a tick's refresh and after every seed, the startup, gap and retried ones
/// alike — as well as by the full scan. With the full scan as its only
/// writer the gauge trailed the store by up to a whole
/// `FULL_SCAN_LEDGERS` period.
async fn regauge_users(
    store: &Store,
    instruments: &Instruments,
    pool: &str,
) -> Result<(), StoreError> {
    let user_count = store.count_users(pool).await?;
    instruments.metrics.users_tracked(pool, user_count);
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
    instruments: &Instruments,
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
    // Only a refresh or a seed inserts or deletes a `users` row, so a tick
    // that refreshed nothing cannot have moved the count and pays for no
    // read.
    if !accounts.is_empty() || !stale_accounts.is_empty() {
        regauge_users(tracker.store(), instruments, pool).await?;
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
            instruments,
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
    instruments: &Instruments,
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
    instruments.metrics.users_tracked(pool, user_count);
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
        // The count this scan gauged above was read before the seed.
        regauge_users(store, instruments, pool).await?;
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
    // Only a scan that reached the end of the function: an owed reseed
    // that failed above propagates instead, and
    // `last_successful_scan_timestamp_seconds{pool}` is what an operator
    // alerts on when scans stop finishing. Stamped for this pool alone —
    // the scan is per pool, and a pool whose scans keep failing must not
    // be covered by another's successes.
    instruments
        .metrics
        .scan_succeeded(pool, std::time::SystemTime::now());
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
#[allow(clippy::too_many_arguments)]
async fn tracker_loop(
    tracker: &Tracker<'_>,
    seed_sources: &[SeedSource],
    cadence: Cadence,
    mut state: LoopState,
    shutdown: &watch::Receiver<bool>,
    instruments: &Instruments,
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
            instruments,
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
const SUBMISSION_QUEUE_CAPACITY: NonZeroUsize = match NonZeroUsize::new(64) {
    Some(capacity) => capacity,
    // Evaluated at compile time, and 64 is not zero: this arm is unreachable
    // and exists only because `Option::expect` is not `const`.
    None => panic!("the submission queue capacity is a non-zero literal"),
};

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

/// Reads [`AuctioneerConfig`] out of the resolved configuration and the
/// keys this process holds. `own_addresses` is every one of them, never
/// just the signing role's: see [`SigningContext`].
fn auctioneer_config_from(config: &ServiceConfig, signing: &SigningContext) -> AuctioneerConfig {
    AuctioneerConfig {
        liquidation_health_factor: config.liquidation_health_factor,
        target_health_factor: config.target_health_factor,
        plan_iterations: config.plan_iterations,
        dry_run: config.dry_run,
        own_addresses: signing.own_addresses(),
    }
}

/// The same for [`FillerConfig`], whose `native_asset` comes from the
/// network rather than the configuration: it is what pays the fees
/// `XLM_FEE_RESERVE` holds back for.
fn filler_config_from(config: &ServiceConfig, signing: &SigningContext) -> FillerConfig {
    FillerConfig {
        dry_run: config.dry_run,
        own_addresses: signing.own_addresses(),
        hf_safety_multiplier: config.hf_safety_multiplier,
        plan_iterations: config.plan_iterations,
        replan_ledgers: config.replan_ledgers,
        replan_near_ledgers: config.replan_near_ledgers,
        high_fee_profit_threshold: config.high_fee_profit_threshold,
        inventory_refresh: config.inventory_refresh,
        native_asset: signing.native_asset.clone(),
    }
}

/// The tracker's own cadence, whose full-scan phase is drawn separately
/// from the auctioneer's on the same knob — see [`AuctioneerCadence`].
fn tracker_cadence_from(config: &ServiceConfig) -> Cadence {
    Cadence {
        user_refresh_ledgers: config.user_refresh_ledgers,
        refresh_batch: config.refresh_batch,
        full_scan_ledgers: config.full_scan_ledgers,
        scan_health_factor: config.scan_health_factor,
        phase: scan_phase(config.full_scan_ledgers),
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
///
/// `instruments` is read-only to all of that: what `act` answered is
/// counted and, for a creation the chain landed or one the queue dropped,
/// reported — ahead of the match that owns this borrower's flag, so that
/// no arm of it moves, clears or keeps a flag differently for having been
/// instrumented.
#[allow(clippy::too_many_arguments)]
async fn recheck_batch(
    auctioneer: &Auctioneer<'_>,
    store: &Store,
    pool: &str,
    batch: &[TrackedUser],
    tick: LedgerTick,
    submit: Option<&SubmissionQueue>,
    shutdown: &watch::Receiver<bool>,
    instruments: &Instruments,
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
        let retry_at =
            move_flag_forward(store, pool, account, flagged_at.get(account).copied(), tick).await?;
        tracing::debug!(
            pool,
            account,
            retry_at,
            "no decision for this borrower; re-flagged to be retried at this ledger so the \
             queue behind it is not held up"
        );
    }
    for (account, decision) in decisions {
        // Checked between users, never inside a submission: a submission
        // already in flight is waited for, because abandoning it would
        // leave a signing key's sequence number consumed by something
        // this bot never saw the outcome of.
        if *shutdown.borrow() {
            return Ok(());
        }
        let acted = auctioneer
            .act(pool, &account, &decision, tick, submit)
            .await;
        note_act(instruments, pool, &account, &acted);
        match acted {
            // A refusal is not a skip, and neither is a submission the chain
            // failed, expired or lost: something was owed and was not done,
            // so the flag moves forward instead of being cleared — exactly
            // what an undecidable borrower's does, and bounded the same way.
            // Clearing it would drop a borrower this bot believes is
            // liquidatable out of the recheck queue until an event, a price
            // move or the full scan's ~1200-ledger period named it again.
            // `ActOutcome::settled` is where that rule lives.
            Ok(outcome) if !outcome.settled() => {
                let retry_at = move_flag_forward(
                    store,
                    pool,
                    &account,
                    flagged_at.get(account.as_str()).copied(),
                    tick,
                )
                .await?;
                tracing::debug!(
                    pool,
                    account,
                    retry_at,
                    "this borrower was owed an action that was not made; re-flagged to be \
                     retried at this ledger"
                );
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
                let retry_at = move_flag_forward(
                    store,
                    pool,
                    &account,
                    flagged_at.get(account.as_str()).copied(),
                    tick,
                )
                .await?;
                tracing::warn!(
                    pool,
                    account,
                    %error,
                    retry_at,
                    "acting on this borrower failed; re-flagged to be retried at this ledger"
                );
            }
        }
    }
    Ok(())
}

/// Instruments what [`Auctioneer::act`] answered for one borrower, and
/// nothing else: it is called before the match that owns that borrower's
/// recheck flag, so no arm of that match moves, clears or keeps a flag
/// differently for having been instrumented.
///
/// Two answers are worth recording. A recorded creation is
/// [`note_creation`]'s. A [`QueueError::Chain`] is the one failure the
/// queue narrows to a submission that provably sent nothing and has spent
/// its retry budget — the creation is gone rather than pending, which is
/// worth saying out loud even though the flag the caller moves forward
/// brings the borrower back on a later pass. Every other answer — a skip,
/// a refusal, a store, chain or math failure — is the caller's to log as
/// it already does.
///
/// A dropped creation counts *both* `attempted` and `failed`, for the same
/// reason [`note_creation`] counts `attempted` on a creation the chain
/// failed: [`Auctioneer::act`] writes the `creations` row before it hands
/// anything to the queue, so by the time the queue answers, the bot has
/// decided to act and recorded the decision. Counting only `failed` would
/// leave `creations_total{result="failed"}` above the `attempted` it is
/// meant to be a fraction of.
fn note_act(
    instruments: &Instruments,
    pool: &str,
    account: &str,
    acted: &Result<ActOutcome, AuctioneerError>,
) {
    match acted {
        Ok(ActOutcome::Recorded(outcome)) => note_creation(instruments, pool, outcome),
        Err(AuctioneerError::Queue(QueueError::Chain(error))) => {
            instruments.metrics.creation(Attempt::Attempted);
            instruments.metrics.creation(Attempt::Failed);
            instruments.notifier.notify(Notification {
                kind: NotificationKind::SubmissionDropped,
                severity: Severity::High,
                pool: pool.to_string(),
                account: Some(account.to_string()),
                message: format!("creation dropped by the queue: {error}"),
            });
        }
        Ok(ActOutcome::Skipped(_) | ActOutcome::Refused) | Err(_) => {}
    }
}

/// Counts one recorded creation and, when the chain landed it, reports it.
///
/// Called for every [`ActOutcome::Recorded`], settled or not: `attempted`
/// means the bot decided to act and recorded the decision, which a dry
/// run's creation, one the startup delay held back and one the chain
/// failed all are. What tells them apart is the second count, which is
/// the chain's answer and not the row's existence — so a
/// [`TxOutcome::Failed`] (a fee charged and no auction) and a
/// [`TxOutcome::Expired`] (provably never applied) are `failed`, while a
/// [`TxOutcome::Unknown`] is neither: it may still land, and a counter
/// that guessed would have to be un-counted.
///
/// Instrumenting only. [`Notifier::notify`] spawns its delivery rather
/// than awaiting a channel, so nothing here can delay or fail the pass
/// that called it (spec §8).
fn note_creation(instruments: &Instruments, pool: &str, outcome: &CreationOutcome) {
    instruments.metrics.creation(Attempt::Attempted);
    match &outcome.submission {
        Some(TxOutcome::Succeeded { ledger, .. }) => {
            instruments.metrics.creation(Attempt::Succeeded);
            let (kind, severity, message) = match outcome.kind {
                CreationKind::Auction => {
                    // Every auction creation names a percent; the fallback
                    // is for the type, which allows `None` because bad
                    // debt has none.
                    let percent = outcome
                        .percent
                        .map_or_else(|| "?".to_string(), |percent| percent.get().to_string());
                    (
                        NotificationKind::AuctionCreated,
                        Severity::Low,
                        format!("liquidation auction created at {percent}% in ledger {ledger}"),
                    )
                }
                CreationKind::BadDebt => (
                    NotificationKind::BadDebtReported,
                    Severity::Medium,
                    format!("bad debt reported in ledger {ledger}"),
                ),
            };
            instruments.notifier.notify(Notification {
                kind,
                severity,
                pool: pool.to_string(),
                account: Some(outcome.account.clone()),
                message,
            });
        }
        Some(TxOutcome::Failed { .. } | TxOutcome::Expired { .. }) => {
            instruments.metrics.creation(Attempt::Failed);
        }
        Some(TxOutcome::Unknown { .. }) | None => {}
    }
}

/// Re-raises one borrower's recheck flag one ledger past `tick`'s, so a
/// borrower this pass could not decide or act on is retried on a later
/// pass instead of holding the oldest flag in its pool — see
/// [`recheck_batch`]'s doc for why that ordering is the whole point.
///
/// One *past* the tick, not the tick itself: the tracker raises flags at
/// `tick.sequence` in the very tick this pass runs on, so `flagged_at ==
/// tick.sequence` is the ordinary case, not an edge. Writing the tick back
/// would leave the row exactly where it was — oldest in the pool, ordered
/// by account among its peers — at the head of every following batch,
/// which is the starvation this function exists to end.
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
///
/// Answers the ledger it wrote, so a caller's log line names the number
/// an operator will find in `users.recheck_ledger` — or an older one, only
/// if the tracker has since flagged the account newer still.
async fn move_flag_forward(
    store: &Store,
    pool: &str,
    account: &str,
    flagged_at: Option<u32>,
    tick: LedgerTick,
) -> Result<u32, LiquidatorError> {
    let retry_at = tick.sequence.saturating_add(1);
    let ledger = flagged_at.map_or(retry_at, |flagged| flagged.max(retry_at));
    store
        .flag_recheck(pool, account, ledger)
        .await
        .map_err(LiquidatorError::Store)?;
    Ok(ledger)
}

/// The two role names a [`StartupGate`] logs under, so the one "startup
/// delay has elapsed" line each task prints says which of them unlocked.
const AUCTIONEER_ROLE: &str = "auctioneer";
/// The filler's, for the same line.
const FILLER_ROLE: &str = "filler";

/// One task's share of `STARTUP_DELAY_LEDGERS`: whether the chain has
/// moved far enough past the first tick this task saw for it to be
/// allowed to submit anything.
///
/// One per task, never shared: the auctioneer and the filler each start
/// at whatever tick they first observe, each answers for its own key, and
/// a gate shared between them would have one task's first tick decide
/// when the other may spend. Each therefore carries the role it logs
/// under, so the two unlock lines are told apart rather than read as one
/// line printed twice.
///
/// No `Default`: a gate with no role would log an unnamed one, and the
/// whole point of the field is that it is set deliberately at each of the
/// two construction sites.
#[derive(Debug)]
struct StartupGate {
    /// The ledger of the first tick this task observed, which the startup
    /// delay is measured from. `None` until that first tick: there is no
    /// meaningful "how far has the chain moved" before one has arrived.
    /// It is also the smallest sequence this task will ever see: the watch
    /// it reads only moves forward (`handle_message` publishes through
    /// `send_if_modified`, gated on a newer sequence), so a pool whose
    /// poller is a ledger or two behind never moves it backwards.
    ///
    /// The ledger, deliberately, and not a count of wakeups: the watch
    /// coalesces — a pass slower than a ledger close wakes once for
    /// several ledgers — and publishes only a tick newer than the last, so
    /// how many times this task wakes depends on the pass's speed and on
    /// the pollers' relative lag, not on how far the chain has moved. A
    /// count would have a safety knob deliver a distance it never
    /// measured; the ledger is the chain's own.
    first_tick_ledger: Option<u32>,
    /// Whether the chain has moved `delay_ledgers` past
    /// `first_tick_ledger`. Latches `true` and stays there — the delay is
    /// measured from startup, never re-armed — so the "submissions are
    /// now possible" log line fires at most once per task.
    unlocked: bool,
    /// Which task this gate belongs to, on that one log line. Two tasks
    /// unlock independently and a line with no role would read as a
    /// duplicate of the other's rather than as the second of two.
    role: &'static str,
}

impl StartupGate {
    /// A locked gate for `role`, which is what its one log line is named
    /// with: [`AUCTIONEER_ROLE`] or [`FILLER_ROLE`].
    fn new(role: &'static str) -> Self {
        Self {
            first_tick_ledger: None,
            unlocked: false,
            role,
        }
    }

    /// Observes `tick` and answers whether this task may submit.
    ///
    /// The elapsed distance cannot underflow: the watch these tasks read
    /// is monotonic — the tracker publishes through `send_if_modified`,
    /// gated on a newer sequence, so a pool whose poller is a ledger or
    /// two behind never moves it backwards — and the first tick seen is
    /// therefore the smallest. It is written as a `saturating_sub` all the
    /// same, so the arithmetic stays total and a later change to how ticks
    /// are published cannot turn this line into a debug-build panic: a
    /// saturated answer reads as "no ledgers have elapsed yet", which
    /// keeps submissions locked — the safe direction.
    fn observe(&mut self, tick: LedgerTick, delay_ledgers: u32) -> bool {
        let first_ledger = *self.first_tick_ledger.get_or_insert(tick.sequence);
        let elapsed = tick.sequence.saturating_sub(first_ledger);
        if !self.unlocked && elapsed >= delay_ledgers {
            self.unlocked = true;
            tracing::info!(
                role = self.role,
                ledger = tick.sequence,
                first_ledger,
                elapsed,
                "the startup delay has elapsed; submissions are now possible"
            );
        }
        self.unlocked
    }
}

/// Mutable state the auctioneer task carries from one tick to the next:
/// its startup gate, and each pool's oracle-scan and full-scan cadence
/// state. Bundled into one struct, rather than four `&mut` parameters on
/// [`auctioneer_tick`], for the same reason [`LoopState`] exists for the
/// tracker.
#[derive(Debug)]
struct AuctioneerState {
    /// Each pool's oracle-scan reference prices.
    price_watches: BTreeMap<String, PriceWatch>,
    /// The ledger each pool's oracle scan last fired at — except a scan
    /// refused as `ChainError::LedgerMoved` that has not yet had its one
    /// immediate retry, which is left unrecorded so the next tick is due
    /// again (see `auctioneer_tick`).
    last_oracle_scan: BTreeMap<String, u32>,
    /// Pools whose oracle scan has spent its one immediate retry since it
    /// last succeeded or failed for any other reason.
    oracle_scan_retried: BTreeSet<String>,
    /// The ledger each pool's full scan last fired at.
    last_full_scan: BTreeMap<String, u32>,
    /// Whether this task may submit yet.
    gate: StartupGate,
}

impl Default for AuctioneerState {
    /// Every cadence map empty and the gate the auctioneer's own: only
    /// this task ever builds one, and [`StartupGate`] has no `Default` of
    /// its own precisely so the role cannot be left unset.
    fn default() -> Self {
        Self {
            price_watches: BTreeMap::new(),
            last_oracle_scan: BTreeMap::new(),
            oracle_scan_retried: BTreeSet::new(),
            last_full_scan: BTreeMap::new(),
            gate: StartupGate::new(AUCTIONEER_ROLE),
        }
    }
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
    instruments: &'a Instruments,
}

/// One pool's oracle scan, run when [`scan_due`] says it is due, and the
/// bookkeeping that decides when it is due next.
///
/// A [`StoreError`] is fatal, as it is everywhere in [`auctioneer_tick`];
/// every other failure is logged with the pool and the tick carries on.
async fn oracle_scan(
    ctx: &AuctioneerContext<'_>,
    pool: &str,
    tick: LedgerTick,
    state: &mut AuctioneerState,
) -> Result<(), LiquidatorError> {
    let watch = state
        .price_watches
        .entry(pool.to_owned())
        .or_insert_with(|| {
            PriceWatch::new(
                ctx.cadence.price_delta_bps,
                PRICE_REFERENCE_STALE_AFTER_SECS,
            )
        });
    match ctx.auctioneer.scan_oracle(pool, watch, tick).await {
        Ok(flagged) => {
            tracing::info!(pool, flagged, "oracle scan");
            state
                .last_oracle_scan
                .insert(pool.to_owned(), tick.sequence);
            state.oracle_scan_retried.remove(pool);
        }
        Err(AuctioneerError::Store(error)) => return Err(LiquidatorError::Store(error)),
        // A race between reads, which the snapshot has already lost
        // `SNAPSHOT_ATTEMPTS` times, not a fault: left unrecorded
        // once, so the next tick is due and the scan does not wait a
        // whole period to see a move it would have caught. Once
        // only: an RPC whose answers keep straddling ledgers would
        // otherwise cost every tick up to `SNAPSHOT_ATTEMPTS` whole
        // snapshots, ahead of every recheck this task makes.
        Err(AuctioneerError::Chain(error @ ChainError::LedgerMoved { .. })) => {
            if state.oracle_scan_retried.insert(pool.to_owned()) {
                tracing::warn!(
                    pool,
                    %error,
                    "the oracle scan's ledger moved; it runs again next tick"
                );
            } else {
                tracing::warn!(
                    pool,
                    %error,
                    "the oracle scan's ledger moved again; it runs again next period"
                );
                state
                    .last_oracle_scan
                    .insert(pool.to_owned(), tick.sequence);
                state.oracle_scan_retried.remove(pool);
            }
        }
        // Anything else waits a period: retried every ledger, an
        // oracle or RPC that has stopped answering would cost a read
        // per ledger per pool for as long as it stays down.
        Err(error) => {
            tracing::warn!(
                pool,
                %error,
                "the oracle scan failed; it runs again next period"
            );
            state
                .last_oracle_scan
                .insert(pool.to_owned(), tick.sequence);
            state.oracle_scan_retried.remove(pool);
        }
    }
    Ok(())
}

/// One tick's whole effect for every configured pool: decide and act on
/// each pool's currently flagged users, and fire the oracle-scan and
/// full-scan-and-flag cadences when due.
///
/// `state.gate` decides whether `ctx.submission_queue` is actually handed
/// to [`recheck_batch`] (`None` until the chain has moved
/// `startup_delay_ledgers` past the first tick this task saw, regardless
/// of whether the queue itself exists — i.e. regardless of dry-run or
/// armed) or passed through unchanged after it has; see
/// [`StartupGate::observe`].
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
    let submit = if state.gate.observe(tick, ctx.cadence.startup_delay_ledgers) {
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
            oracle_scan(ctx, pool, tick, state).await?;
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
            ctx.instruments,
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
#[allow(clippy::too_many_arguments)]
async fn auctioneer_loop(
    store: &Store,
    pools: &[String],
    auctioneer: &Auctioneer<'_>,
    cadence: AuctioneerCadence,
    submission_queue: Option<&SubmissionQueue>,
    mut tick_rx: watch::Receiver<LedgerTick>,
    shutdown: &watch::Receiver<bool>,
    instruments: &Instruments,
) -> Result<(), LiquidatorError> {
    let ctx = AuctioneerContext {
        store,
        pools,
        auctioneer,
        cadence,
        submission_queue,
        shutdown,
        instruments,
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

/// Runs [`Filler::tick`] off the same tick the tracker publishes after it
/// has acknowledged one.
///
/// [`auctioneer_loop`]'s shape, and a separate task for the same reason
/// (see the module doc's wiring note): a fill is not a stored effect of a
/// ledger, so nothing here is upstream of a tick's acknowledgement and a
/// slow or failing pass falls behind the newest ledger rather than
/// blocking, delaying or failing it. It is fed by the watch, never by the
/// poller channel the cursor rests on. `changed()` returning an error
/// means every sender has dropped — the tracker task is gone — and this
/// loop then has nothing further to do.
///
/// Its own [`StartupGate`] decides `execute`: inside the delay the filler
/// still plans every auction, and records and submits nothing (ruling 9).
///
/// # Errors
///
/// [`crate::filler::FillerError::Store`] only, which ends this task — as
/// a store failure ends the tracker's and the auctioneer's, for the same
/// reason: the bot cannot trust what it read about which auctions are
/// open. [`drain_tasks`] turns that into a shutdown like any other.
async fn filler_loop(
    filler: &Filler<'_>,
    startup_delay_ledgers: u32,
    queue: Option<&SubmissionQueue>,
    mut tick_rx: watch::Receiver<LedgerTick>,
    shutdown: &watch::Receiver<bool>,
) -> Result<(), LiquidatorError> {
    let mut state = FillerState::default();
    let mut gate = StartupGate::new(FILLER_ROLE);
    while tick_rx.changed().await.is_ok() {
        if *shutdown.borrow() {
            return Ok(());
        }
        let tick = *tick_rx.borrow_and_update();
        let execute = gate.observe(tick, startup_delay_ledgers);
        let summary = filler
            .tick(&mut state, tick, execute, queue, shutdown)
            .await?;
        // A tick that did nothing is the ordinary one — most ledgers hold
        // no open auction worth planning — so it stays at debug and only
        // a tick that moved something is worth a line per ledger.
        if summary.planned == 0
            && summary.executed == 0
            && summary.closed == 0
            && summary.unwound == 0
        {
            tracing::debug!(
                ledger = tick.sequence,
                skipped = summary.skipped,
                "filler tick"
            );
        } else {
            tracing::info!(
                ledger = tick.sequence,
                planned = summary.planned,
                executed = summary.executed,
                skipped = summary.skipped,
                closed = summary.closed,
                unwound = summary.unwound,
                "filler tick"
            );
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

/// How [`drain_tasks`] ended, once every task has returned.
///
/// A panic is kept apart from [`LiquidatorError`] rather than wedged into
/// its taxonomy: it is an internal bug, not one of the phases that enum
/// distinguishes, and swallowing it into, say, `Config` would misreport a
/// bug as a bad configuration — and exit `2`, telling the operator to fix
/// something they did not break.
enum Drained {
    /// Every task returned: `Ok` when all of them did, else the first
    /// error, the ones after it being usually this shutdown's own doing.
    Returned(Result<(), LiquidatorError>),
    /// A task panicked: the first panic's payload, which [`finish_run`]
    /// resumes only once the notifier has drained, so the process still
    /// ends as a panic — Rust's exit code `101`, the message already on
    /// stderr from when it happened.
    Panicked(Box<dyn std::any::Any + Send + 'static>),
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

/// What signs a transaction, shared by the submission queues' own tasks
/// and by the auctioneer's and the filler's — each holding a key whenever
/// one is configured at all, dry-run included, since both still simulate
/// with no intent to submit (see their own docs). `Signer` is deliberately
/// not `Clone` — it holds key material — so an `Arc` is what lets several
/// tasks borrow the one key without duplicating it in memory, and what
/// lets [`Signers::shared`] tell "both roles fell back to one key" from
/// "two keys that happen to be configured" by pointer.
///
/// `own_addresses` is computed from **every** key the process was given,
/// not from the one that signs a given role: with `AUCTIONEER_SECRET_KEY`
/// and `FILLER_SECRET_KEY` set to different keys, deriving the set from
/// the auctioneer's alone would leave the filler's own position a borrower
/// the auctioneer is willing to liquidate. It is therefore taken from
/// [`SigningKeys::own_addresses`] before [`SigningKeys::into_signers`]
/// consumes the keys.
///
/// `native_asset` is the network's own — derived from the passphrase, not
/// configured — because it is what pays every fee, and so what
/// `XLM_FEE_RESERVE` is withheld from.
struct SigningContext {
    network: Network,
    tx_config: TxConfig,
    signers: Signers,
    own_addresses: BTreeSet<String>,
    native_asset: String,
}

impl SigningContext {
    /// # Errors
    ///
    /// [`LiquidatorError::Chain`] when the network's native asset contract
    /// cannot be derived, which is an XDR failure rather than anything an
    /// operator configured.
    fn from_config(config: &ServiceConfig, keys: SigningKeys) -> Result<Self, LiquidatorError> {
        // Before `into_signers`, which consumes the keys and collapses the
        // auctioneer's onto the filler's when only one is configured.
        let own_addresses = keys.own_addresses();
        let network = Network::from_config(&config.chain);
        let native_asset = network.native_asset_contract()?;
        Ok(Self {
            network,
            tx_config: TxConfig::from_config(&config.chain),
            signers: keys.into_signers(),
            own_addresses,
            native_asset,
        })
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
///
/// Every poller records its heartbeat and its pool's chain head on the
/// run's one `metrics`, and reports a run of failed passes through its one
/// `notifier`. Both are shared rather than per-pool: `/livez` and
/// [`watchdog_loop`] read one recorder for every pool, and the notifier's
/// dedup is keyed by pool, so one instance never lets one pool's failures
/// silence another's.
#[allow(clippy::too_many_arguments)]
fn spawn_pollers(
    tasks: &mut JoinSet<Result<(), LiquidatorError>>,
    rpc: &RpcClient,
    store: &Store,
    pools: &[PoolConfig],
    poller_config: PollerConfig,
    sender: &mpsc::Sender<PollerMessage>,
    metrics: &Arc<Metrics>,
    notifier: &Arc<Notifier>,
    shutdown: &watch::Receiver<bool>,
) {
    for pool in pools {
        let rpc = rpc.clone();
        let store = store.clone();
        let pool = pool.address.clone();
        let sender = sender.clone();
        let metrics = Arc::clone(metrics);
        let notifier = Arc::clone(notifier);
        let shutdown = shutdown.clone();
        tasks.spawn(async move {
            LedgerPoller::new(&rpc, &store, &pool, poller_config)
                .with_metrics(metrics)
                .with_notifier(notifier)
                .run(sender, shutdown)
                .await
                .map_err(LiquidatorError::from)
        });
    }
}

/// Spawns the watchdog: one task for the whole run, watching every
/// poller's heartbeat rather than any pool's chain state.
///
/// It is the counterpart of `/livez` for a deployment that nothing probes:
/// the same [`PollerConfig::liveness_deadline`], reported to the operator
/// instead of to a load balancer. Spawned beside the pollers rather than
/// inside one, because a poller that has stopped is exactly the thing that
/// cannot report itself.
fn spawn_watchdog(
    tasks: &mut JoinSet<Result<(), LiquidatorError>>,
    metrics: Arc<Metrics>,
    notifier: Arc<Notifier>,
    pools: Vec<String>,
    config: PollerConfig,
    shutdown: &watch::Receiver<bool>,
) {
    let shutdown = shutdown.clone();
    tasks.spawn(async move {
        watchdog_loop(&metrics, &notifier, &pools, config, &shutdown).await;
        Ok(())
    });
}

/// Reports every pool whose poller has not heartbeated within
/// [`PollerConfig::liveness_deadline`], once per `poll_interval`, until
/// `shutdown` flips.
///
/// Three things it deliberately does not do:
///
/// - **It does not read a missing heartbeat as a stall.** A pool with no
///   heartbeat at all has a poller that may not have run its first
///   iteration yet — seeding a busy pool is tens of seconds — and a
///   watchdog that notified on that would notify on every restart.
/// - **It does not deduplicate.** A pool that is still stalled is notified
///   again on every pass and the notifier's own cooldown suppresses it,
///   which is the one place in this crate that decides how often a
///   repeating condition is worth saying out loud.
/// - **It does not notify a recovery.** Spec §7's kinds are a closed set
///   and none of them means "the heartbeat is back", so a pool whose
///   poller returns is logged once. `reported` exists for exactly that
///   once: without it the log line would repeat every pass.
///
/// Returns rather than erroring, always: a watchdog that failed the run
/// would be instrumentation stopping trading (spec §8).
pub(crate) async fn watchdog_loop(
    metrics: &Metrics,
    notifier: &Notifier,
    pools: &[String],
    config: PollerConfig,
    shutdown: &watch::Receiver<bool>,
) {
    let deadline = config.liveness_deadline();
    let mut shutdown = shutdown.clone();
    let mut reported: BTreeSet<String> = BTreeSet::new();
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        let now = std::time::Instant::now();
        for pool in pools {
            let Some(last) = metrics
                .pool_status(pool)
                .and_then(|status| status.heartbeat)
            else {
                continue;
            };
            let silent = now.saturating_duration_since(last);
            if silent > deadline {
                reported.insert(pool.clone());
                notifier.notify(Notification {
                    kind: NotificationKind::PollerStalled,
                    severity: Severity::High,
                    pool: pool.clone(),
                    account: None,
                    message: format!(
                        "no poller heartbeat for {}s (limit {}s)",
                        silent.as_secs(),
                        deadline.as_secs()
                    ),
                });
            } else if reported.remove(pool) {
                tracing::info!(
                    pool = %pool,
                    silent_secs = silent.as_secs(),
                    "the poller is heartbeating again"
                );
            }
        }
        tokio::select! {
            () = tokio::time::sleep(config.poll_interval) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

/// The submission queues one run holds: the handle each role submits
/// through, or `None` when that role submits nothing.
///
/// The two fields are the *same* queue whenever the two roles hold the one
/// key — see [`spawn_queues`] — and that is the point of the type: a role
/// never learns which, and never needs to.
struct Queues {
    /// Where [`Auctioneer::act`] submits an auction creation.
    auctioneer: Option<SubmissionQueue>,
    /// Where the filler's [`Executor`] submits a fill.
    filler: Option<SubmissionQueue>,
}

/// Builds and spawns one queue worker per **distinct** signing key when
/// armed — `!dry_run`, the one gate into live trading `DRY_RUN`'s default
/// makes safe — and returns the handles each role submits through.
///
/// One worker per key, and never two: a Soroban transaction is built
/// against its source account's sequence number at prepare time, so two
/// workers on one key would race to consume it. So the filler's key gets a
/// worker, and the auctioneer either shares that worker's queue — when
/// [`SigningKeys::into_signers`]' fallback gave both roles the one `Arc`,
/// which [`Signers::shared`] tells by pointer — or gets a worker of its
/// own for its own key.
///
/// A dry run gets neither worker nor handle: nothing is submitted, so
/// there is nothing to order.
fn spawn_queues(
    tasks: &mut JoinSet<Result<(), LiquidatorError>>,
    rpc: &RpcClient,
    signing: &SigningContext,
    dry_run: bool,
    shutdown: &watch::Receiver<bool>,
) -> Queues {
    let none = Queues {
        auctioneer: None,
        filler: None,
    };
    if dry_run {
        return none;
    }
    if signing.signers.auctioneer.is_none() && signing.signers.filler.is_none() {
        // `main` has already warned `LIVE`, on `DRY_RUN` alone. Arming
        // needs both halves, and a bot that says LIVE and then only ever
        // simulates is the silent direction this repository logs loudly
        // against everywhere else. `Args::signing_keys` refuses
        // `DRY_RUN=false` with no `FILLER_SECRET_KEY`, so this is
        // unreachable from `main`; it stays because that refusal should
        // not be the only thing standing between an armed bot and
        // silence.
        tracing::warn!(
            "DRY_RUN=false, but no signing key is configured: nothing will be submitted \
             until AUCTIONEER_SECRET_KEY or FILLER_SECRET_KEY is set"
        );
        return none;
    }
    let filler = signing
        .signers
        .filler
        .as_ref()
        .map(|signer| spawn_queue_worker(tasks, rpc, signing, signer, shutdown));
    let auctioneer = if signing.signers.shared() {
        filler.clone()
    } else {
        signing
            .signers
            .auctioneer
            .as_ref()
            .map(|signer| spawn_queue_worker(tasks, rpc, signing, signer, shutdown))
    };
    Queues { auctioneer, filler }
}

/// One ordered worker for one key: its own [`Submitter`] over an
/// `Arc::clone` of `signer`, draining its queue until every handle has
/// dropped or shutdown is raised.
fn spawn_queue_worker(
    tasks: &mut JoinSet<Result<(), LiquidatorError>>,
    rpc: &RpcClient,
    signing: &SigningContext,
    signer: &Arc<Signer>,
    shutdown: &watch::Receiver<bool>,
) -> SubmissionQueue {
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
    queue
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
    instruments: Instruments,
    tick_rx: watch::Receiver<LedgerTick>,
    shutdown: &watch::Receiver<bool>,
) {
    let rpc = rpc.clone();
    let store = store.clone();
    let network = signing.network.clone();
    let tx_config = signing.tx_config;
    let signer = signing.signers.auctioneer.clone();
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
            &instruments,
        )
        .await
    });
}

/// Spawns the filler task, beside the auctioneer and on the same watch:
/// builds its own [`Submitter`] whenever a filler key is configured at all
/// — dry-run included, since a dry run with a key simulates every fill it
/// plans — its [`Executor`] over that, an [`Inventory`] withholding
/// `xlm_fee_reserve` of the network's native asset, and runs
/// [`filler_loop`] off `tick_rx` until the tracker task's sender drops.
///
/// `notifier` and `metrics` are [`Service::run`]'s one instance of each,
/// shared with the pollers and the watchdog: both are built there, before
/// the first task that reports or records through them, and handed in here
/// rather than constructed per task — the dedup the notifier's cooldown
/// rests on is keyed by `(pool, account, kind)` and a gauge means nothing
/// split across recorders.
#[allow(clippy::too_many_arguments)]
fn spawn_filler(
    tasks: &mut JoinSet<Result<(), LiquidatorError>>,
    rpc: &RpcClient,
    store: &Store,
    signing: &SigningContext,
    config: FillerConfig,
    pools: Vec<PoolConfig>,
    xlm_fee_reserve: u64,
    startup_delay_ledgers: u32,
    queue: Option<SubmissionQueue>,
    notifier: Arc<Notifier>,
    metrics: Arc<Metrics>,
    tick_rx: watch::Receiver<LedgerTick>,
    shutdown: &watch::Receiver<bool>,
) {
    let rpc = rpc.clone();
    let store = store.clone();
    let network = signing.network.clone();
    let tx_config = signing.tx_config;
    let signer = signing.signers.filler.clone();
    let native_asset = signing.native_asset.clone();
    let shutdown = shutdown.clone();
    tasks.spawn(async move {
        let dry_run = config.dry_run;
        let submitter = signer
            .as_deref()
            .map(|signer| Submitter::new(&rpc, &network, signer, tx_config));
        let executor = Executor::new(&store, submitter, dry_run);
        let inventory = Inventory::new(native_asset, xlm_fee_reserve);
        let filler = Filler::new(
            &rpc, &store, &pools, config, executor, inventory, notifier, metrics,
        );
        filler_loop(
            &filler,
            startup_delay_ledgers,
            queue.as_ref(),
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
/// shutdown's own consequences.
///
/// A task's panic takes the same path, for the same reason. Resuming it the
/// moment its `JoinHandle` answered would unwind out of here and drop the
/// [`JoinSet`] just the same, and the release profile unwinds for exactly
/// this (`Cargo.toml`): with `panic = "abort"` a bug in any task — an
/// overflow check tripping in the maths, say — ended the process between a
/// `sendTransaction` and its outcome. The first panic wins over any error,
/// since it is a bug and must end the process as one; a cancelled task,
/// which nothing here ever causes, is treated as a panic rather than as a
/// task that finished its work.
async fn drain_tasks(
    mut tasks: JoinSet<Result<(), LiquidatorError>>,
    shutdown: &watch::Sender<bool>,
) -> Drained {
    let mut failure: Option<LiquidatorError> = None;
    let mut panicked: Option<Box<dyn std::any::Any + Send + 'static>> = None;
    while let Some(outcome) = tasks.join_next().await {
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if failure.is_none() && panicked.is_none() {
                    tracing::error!(
                        %error,
                        "a service task failed; shutting down and waiting for the rest"
                    );
                    let _ = shutdown.send(true);
                } else {
                    tracing::warn!(%error, "another task failed while shutting down");
                }
                failure.get_or_insert(error);
            }
            Err(error) => {
                if panicked.is_none() {
                    tracing::error!(
                        %error,
                        "a service task panicked; shutting down and waiting for the rest"
                    );
                    let _ = shutdown.send(true);
                } else {
                    tracing::warn!(%error, "another task panicked while shutting down");
                }
                let payload = error.try_into_panic().unwrap_or_else(|cancelled| {
                    Box::new(format!(
                        "a service task was cancelled unexpectedly: {cancelled}"
                    ))
                });
                panicked.get_or_insert(payload);
            }
        }
    }
    match (panicked, failure) {
        (Some(payload), _) => Drained::Panicked(payload),
        (None, Some(error)) => Drained::Returned(Err(error)),
        (None, None) => Drained::Returned(Ok(())),
    }
}

/// The last thing every exit of [`Service::run`] does: gives the
/// notifications still in flight [`DRAIN_BUDGET`] to leave, then returns
/// what [`drain_tasks`] answered unchanged — or, for a task's panic,
/// resumes it, so the process ends as the panic it was.
///
/// Spec §7 asks for `drain()` on every exit path, and this is where both
/// of `run`'s are — a task failure and a shutdown signal join the same way
/// (see [`drain_tasks`]), so they leave the same way too. `run`'s earlier
/// `?`s need no drain of their own: everything before the [`JoinSet`] —
/// validation and the seed pass — records gauges and logs, and nothing
/// there notifies, so a startup failure has nothing in flight to wait on.
/// The one exit that deliberately does not drain is the second
/// `SIGINT`/`SIGTERM`, which `spawn_shutdown_listener` answers with
/// `exit(130)`: a second signal means *now*, and a drain would be exactly
/// the delay it is refusing.
///
/// A drain that times out is a warning and nothing else. An instrument
/// never affects trading (spec §8), and by here there is no trading left
/// to affect: what it costs is a notification the operator reads in the
/// log instead of in the channel, which [`Notifier`] has already written
/// there.
///
/// One exit skips this, deliberately, per the paragraph above: the second
/// `SIGINT`/`SIGTERM`. A task's panic does not: every build unwinds
/// (`Cargo.toml`'s release profile says so explicitly), [`drain_tasks`]
/// shuts the other tasks down behind it as it would behind an error, and
/// only here, after the drain, is it resumed.
async fn finish_run(drained: Drained, notifier: &Notifier) -> Result<(), LiquidatorError> {
    if notifier.drain(DRAIN_BUDGET).await {
        tracing::info!("notifications drained");
    } else {
        tracing::warn!(
            budget_secs = DRAIN_BUDGET.as_secs(),
            in_flight = notifier.in_flight(),
            "notifications still in flight at the drain budget; leaving them behind"
        );
    }
    match drained {
        Drained::Returned(result) => result,
        Drained::Panicked(payload) => std::panic::resume_unwind(payload),
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
    ///
    /// `keys` is taken because `validate_filler` is part of what this
    /// checks: a deploy smoke test that never looked at the filler account
    /// would pass on a configuration `run` refuses a moment later.
    pub async fn check_config(
        config: &ServiceConfig,
        keys: SigningKeys,
    ) -> Result<Vec<String>, LiquidatorError> {
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
        let (validations, mut warnings) = validate(&rpc, &config.pools).await?;
        let signing = SigningContext::from_config(config, keys)?;
        warnings.extend(validate_filler(&rpc, config, &signing).await?);
        log_http(config.http);
        log_validation(config, &validations, &warnings);
        // Last, and after everything is reported: the credentials the run
        // would trust, checked here rather than discovered at the first
        // notification the operator needed to see. One `getMe`, which
        // reads and changes nothing — and reporting first means a refused
        // token does not cost the operator the rest of the report.
        verify_telegram(config).await?;
        Ok(warnings)
    }

    /// Connects and migrates the store, validates the configuration, seeds
    /// every pool that needs it, then follows every configured pool — one
    /// [`LedgerPoller`] per pool, one tracker task consuming their shared
    /// channel, one auctioneer task and one filler task, each fed by the
    /// tick the tracker publishes after it acknowledges, one watchdog,
    /// one HTTP server when a port is set, and — only when armed — one
    /// submission-queue worker per distinct signing key — until a
    /// shutdown signal arrives and every task has returned. Once its
    /// tasks are running, every return goes through `finish_run`, which
    /// drains the notifier; an earlier `?` returns with nothing in flight,
    /// since nothing before the tasks notifies — a task's panic included,
    /// which is resumed after the drain rather than returned. The second
    /// shutdown signal's `exit(130)` ends the process without returning.
    ///
    /// `keys` holds both of `AUCTIONEER_SECRET_KEY` and
    /// `FILLER_SECRET_KEY`, either or both of which may be absent — neither
    /// configured is the ordinary dry-run deployment; see
    /// [`crate::config::Args::signing_keys`]. The filler signs with its own
    /// key and the auctioneer with its own or else the filler's, and
    /// **both** addresses go into the set the auctioneer refuses to act on.
    /// Whether a submission is ever actually sent is `!config.dry_run` and
    /// a key for that role — the one gate this crate has into live
    /// trading, per the safety invariant that `DRY_RUN` defaults `true`.
    // Wiring, not logic: this function is one `spawn_*` call per kind of
    // task, each of which is a named function with its own docs, so
    // splitting it further would hide the one place the run's shape — and
    // its order — can be read end to end.
    #[allow(clippy::too_many_lines)]
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
        let (validations, mut warnings) = validate(&rpc, &config.pools).await?;
        // Both validations run before anything is spawned or seeded: a
        // configuration this bot will refuse should cost one round trip,
        // not a full seed of every pool first.
        let signing = SigningContext::from_config(&config, keys)?;
        warnings.extend(validate_filler(&rpc, &config, &signing).await?);
        log_validation(&config, &validations, &warnings);

        // The run's one recorder and its one notifier, both built before
        // the first thing that records or reports through them — which is
        // the seed pass below, before any task is spawned at all — rather
        // than beside whichever task happens to be their last caller. The
        // notifier is built here and not a line later for a second reason:
        // `with_metrics` installs through `Arc::get_mut`, so it must be
        // called before any delivery has been spawned. It is log-only
        // unless both Telegram credentials are configured; every delivery
        // it makes is spawned behind its own semaphore rather than awaited
        // in a tick (spec §8).
        let metrics = Arc::new(Metrics::new());
        let notifier = Arc::new(build_notifier(&config, Arc::clone(&metrics))?);
        // The pair every loop that needs both is handed. The pollers and
        // the watchdog take the two `Arc`s themselves: each uses one of
        // them the way its own constructor already spells it.
        let instruments = Instruments {
            metrics: Arc::clone(&metrics),
            notifier: Arc::clone(&notifier),
        };

        let poller_config = PollerConfig::new(config.poll_interval);
        let pool_addresses: Vec<String> = config
            .pools
            .iter()
            .map(|pool| pool.address.clone())
            .collect();
        let mut tasks = JoinSet::new();
        // The diagnostics port, when one is configured, and a task of the
        // run like any other so that a shutdown joins it with the rest.
        //
        // Spawned **before** the seed pass, not after it: seeding a busy
        // pool is tens of seconds of sequential round trips, and a port
        // that is not bound for the whole of that window is a `/livez` a
        // restart probe cannot reach — a Kubernetes deployment without a
        // startup probe kills the process mid-seed and it starts the same
        // seed again, and a Cloud Run revision whose startup TCP probe
        // never connects fails to start at all. Everything `HttpState`
        // needs exists by here, and until the pollers run `liveness`
        // measures a pool's silence from `started` (see
        // [`crate::http::liveness`]), which is the same rule
        // `watchdog_loop` applies.
        log_http(config.http);
        if let Some(http) = config.http {
            let state = Arc::new(HttpState {
                metrics: Arc::clone(&metrics),
                store: store.clone(),
                pools: pool_addresses.clone(),
                max_lag_ledgers: http.max_lag_ledgers,
                liveness_deadline: poller_config.liveness_deadline(),
                started: std::time::Instant::now(),
            });
            let shutdown = shutdown_rx.clone();
            tasks.spawn(async move {
                http::serve(http.bind, state, shutdown).await;
                Ok(())
            });
        }

        let seed_sources = build_seed_sources(&config.seed)?;
        let needs_reseed = seed_pools_needing_it(
            &rpc,
            &store,
            &config.pools,
            &seed_sources,
            config.refresh_batch,
            config.poll_interval,
            &shutdown_rx,
            &instruments,
        )
        .await?;

        let (message_tx, message_rx) = mpsc::channel(1_024);
        spawn_pollers(
            &mut tasks,
            &rpc,
            &store,
            &config.pools,
            poller_config,
            &message_tx,
            &metrics,
            &notifier,
            &shutdown_rx,
        );
        // Every poller now holds its own sender clone; dropping this one
        // lets the tracker task's channel close, and its `recv` return
        // `None`, once (and only once) every poller has stopped.
        drop(message_tx);

        let queues = spawn_queues(&mut tasks, &rpc, &signing, config.dry_run, &shutdown_rx);

        let auctioneer_config = auctioneer_config_from(&config, &signing);
        let filler_config = filler_config_from(&config, &signing);
        let auctioneer_cadence = auctioneer_cadence_from(&config);

        // The deciding tasks' own view of the tick, published by the
        // tracker task only after it has acknowledged one (see
        // `handle_message`'s `Tick` arm). The initial value is never
        // observed as real: a `watch::Receiver` only wakes a waiter on a
        // *change*, and each loop's first `changed()` is what it actually
        // reads.
        let (tick_tx, tick_rx) = watch::channel(LedgerTick {
            sequence: 0,
            close_time: 0,
        });
        // Beside the pollers, never inside one: a poller that has stopped
        // is exactly what cannot report itself.
        spawn_watchdog(
            &mut tasks,
            Arc::clone(&metrics),
            Arc::clone(&notifier),
            pool_addresses.clone(),
            poller_config,
            &shutdown_rx,
        );
        spawn_auctioneer(
            &mut tasks,
            &rpc,
            &store,
            &signing,
            auctioneer_config,
            auctioneer_cadence,
            pool_addresses,
            queues.auctioneer,
            instruments.clone(),
            tick_rx.clone(),
            &shutdown_rx,
        );
        spawn_filler(
            &mut tasks,
            &rpc,
            &store,
            &signing,
            filler_config,
            config.pools.clone(),
            config.xlm_fee_reserve,
            config.startup_delay_ledgers,
            queues.filler,
            Arc::clone(&notifier),
            Arc::clone(&instruments.metrics),
            tick_rx,
            &shutdown_rx,
        );

        let cadence = tracker_cadence_from(&config);
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
                &instruments,
                tick_tx,
                message_rx,
            )
            .await
            .map_err(LiquidatorError::from)
        });

        // Both exits — every task returned, or one failed and the rest
        // were shut down behind it — leave through here, so the
        // notifications the way out produced have their bounded chance to
        // go before the process does.
        finish_run(drain_tasks(tasks, &shutdown_tx).await, &notifier).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use stellar_xdr::{
        ContractDataDurability, ContractDataEntry, ContractExecutable, ExtensionPoint,
        LedgerEntryData, ScContractInstance, ScMap, ScString, ScVal, TransactionResult,
        TransactionResultExt, TransactionResultResult, VecM,
    };

    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{scval_b64, transaction_data_b64, ScriptedRpc};
    use crate::chain::xdr::encode::{
        address, i128_val, map, sc_address, symbol, to_base64, vec as sc_vec, FillPercent,
    };
    use crate::chain::xdr::keys;
    use crate::chain::xdr::{AuctionType, PoolEvent};
    use crate::chain::{LedgerWindow, TxHash, TxOutcome};
    use crate::config::{ChainConfig, RunMode, Secret};
    use crate::fixture::{mainnet_fixed_v2, text};
    use crate::harness;
    use crate::math::fill::FillObjective;
    use crate::math::AuctionData;
    use crate::store::TrackedAuction;
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

    fn instance_entry_xdr(
        pool: &str,
        backstop: &str,
        oracle: &str,
        status: u32,
        max_positions: u32,
    ) -> String {
        let config = map(vec![
            (symbol("bstop_rate").unwrap(), ScVal::U32(1_000_000)),
            (symbol("max_positions").unwrap(), ScVal::U32(max_positions)),
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
    /// The synthetic pool with the `max_positions` every test but one wants: 4.
    fn script_pool(
        rpc: &ScriptedRpc,
        pool: &str,
        backstop: &str,
        status: u32,
        reserves: &[SyntheticReserve],
        ledger: u32,
    ) {
        script_pool_with_max_positions(rpc, pool, backstop, status, reserves, ledger, 4);
    }

    fn script_pool_with_max_positions(
        rpc: &ScriptedRpc,
        pool: &str,
        backstop: &str,
        status: u32,
        reserves: &[SyntheticReserve],
        ledger: u32,
        max_positions: u32,
    ) {
        let assets: Vec<&str> = reserves.iter().map(|reserve| reserve.asset).collect();
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(pool).unwrap(), &instance_entry_xdr(pool, backstop, ORACLE, status, max_positions)),
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
            // Deliberately not the crate's own default (`FreeFill`): none
            // of this fixture's callers are testing which ledger a fill
            // aims at, and `FreeFill` would move every one of their fill
            // ledgers out to `start + RAMP_END_BLOCKS`, which is a fact
            // about the objective, not about whatever each test's name
            // says it is checking.
            fill_objective: FillObjective::EarliestProfitable,
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

    /// A status the pool cannot borrow at is a warning that still lets
    /// validation succeed, not an error: an on-ice or frozen pool must let
    /// the bot start and warn, not refuse to start.
    ///
    /// `AdminOnIce` (2) rather than something deeper because it is the
    /// boundary: it is the first status past the two the contract treats
    /// as operational, so a warning that started one status early or one
    /// late would show up here.
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
            PoolStatus::AdminOnIce.code(),
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
                PoolStatus::AdminOnIce
            )],
            "the warning must name the pool and its actual status"
        );
        Ok(())
    }

    /// An admin-activated pool draws no warning at all.
    ///
    /// Status 0 and status 1 are operationally identical for everything
    /// this bot does — `Pool::require_action_allowed` refuses borrow and
    /// cancel only past 1 and supply only past 3 — and 0 is where a pool
    /// lands when its admin activates it, which is the ordinary state of a
    /// new mainnet pool and of the sandbox tier's own. Warning about it
    /// teaches an operator to read the startup warnings as noise.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_admin_active_pool_status_is_not_a_warning(db: sqlx::PgPool) -> sqlx::Result<()> {
        let _store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        script_pool(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::AdminActive.code(),
            &[usable_reserve()],
            LEDGER,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, USDC, &[USDC], &["*"]);

        let (validations, warnings) = validate(&client, &[pool]).await.expect("validate");
        assert_eq!(validations.len(), 1);
        assert_eq!(validations[0].pool, POOL_A);
        assert!(
            warnings.is_empty(),
            "an admin-activated pool is active for everything the bot does: {warnings:?}"
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

    /// A pool whose `max_positions` is below two can never have an auction
    /// created for it — every auction names at least one bid and one lot —
    /// so following it is refused at startup rather than handing the
    /// contract a plan it rejects on every tick.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_pool_with_max_positions_below_two_is_refused(db: sqlx::PgPool) -> sqlx::Result<()> {
        let _store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        script_pool_with_max_positions(
            &rpc,
            POOL_A,
            BACKSTOP_A,
            PoolStatus::Active.code(),
            &[usable_reserve()],
            LEDGER,
            1,
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pool = pool_config(POOL_A, USDC, &[USDC], &["*"]);
        let error = validate(&client, &[pool])
            .await
            .expect_err("a pool no auction fits")
            .to_string();
        assert!(error.contains("max_positions"), "{error}");
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

    /// Instruments whose notifier records every delivery instead of
    /// logging it, and the channel to read them back off once
    /// [`Notifier::drain`] has let the spawned deliveries finish. The
    /// cooldown is an hour, so a second notification of the same
    /// `(pool, account, kind)` inside one test is a deduplication a test
    /// can assert on rather than a race.
    fn recording_instruments() -> (Instruments, Arc<harness::RecordingChannel>) {
        let recording = Arc::new(harness::RecordingChannel::new(false));
        let instruments = Instruments {
            metrics: Arc::new(Metrics::new()),
            notifier: Arc::new(Notifier::new(
                Box::new(Arc::clone(&recording)),
                std::time::Duration::from_hours(1),
            )),
        };
        (instruments, recording)
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
        let instruments = Instruments::for_tests();

        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &instruments,
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
        assert!(
            instruments.metrics.render().contains(&format!(
                "events_processed_total{{pool=\"{}\"}} 1",
                harness::POOL
            )),
            "the event it applied is counted, once"
        );
        assert_eq!(
            instruments
                .metrics
                .pool_status(harness::POOL)
                .and_then(|status| status.processed),
            None,
            "and no ledger is processed yet: that gauge is the cursor the poller commits, \
             which only an acknowledged tick moves"
        );

        let tick = harness::fixture_tick();
        let (message, applied) = tick_message(harness::POOL, tick);
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &instruments,
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
        assert_eq!(
            instruments
                .metrics
                .pool_status(harness::POOL)
                .and_then(|status| status.processed),
            Some(tick.sequence),
            "the acknowledged tick is the ledger this pool has fully applied"
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
                &Instruments::for_tests(),
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
            &Instruments::for_tests(),
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
            &Instruments::for_tests(),
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
        let (instruments, recording) = recording_instruments();

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
            &instruments,
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
        let rendered = instruments.metrics.render();
        assert!(
            rendered.contains(&format!("users_tracked{{pool=\"{}\"}} 1\n", harness::POOL)),
            "the reseed gauges the count it left, before any full scan: {rendered}"
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
            &tracker,
            &sources,
            cadence,
            &mut state,
            &shutdown,
            &instruments,
            &tick_tx,
            message,
        )
        .await
        .expect("the scanning tick");
        assert!(applied.await.is_ok());
        assert!(
            !state.needs_reseed.contains(harness::POOL),
            "a seed that reached every source clears the mark"
        );

        assert!(
            instruments
                .notifier
                .drain(std::time::Duration::from_secs(5))
                .await,
            "every delivery this test's notifier spawned finished"
        );
        let sent = recording.sent();
        assert_eq!(
            sent.len(),
            1,
            "the gap is reported, and the reseed and the tick that followed report nothing: \
             {sent:?}"
        );
        assert_eq!(sent[0].kind, NotificationKind::EventGap);
        assert_eq!(sent[0].severity, Severity::High);
        assert_eq!(sent[0].pool, harness::POOL);
        assert_eq!(
            sent[0].account, None,
            "a gap is the pool's, never one borrower's"
        );
        assert!(
            sent[0].message.contains("10") && sent[0].message.contains("400000"),
            "the message names the range that is gone: {}",
            sent[0].message
        );
        assert!(
            instruments
                .metrics
                .render()
                .contains(&format!("users_tracked{{pool=\"{}\"}} 1\n", harness::POOL)),
            "the full scan published the count it read"
        );
        assert!(
            instruments
                .metrics
                .render()
                .lines()
                .any(|line| line.starts_with(&format!(
                    "blend_liquidator_last_successful_scan_timestamp_seconds{{pool=\"{}\"}} ",
                    harness::POOL
                ))),
            "and stamped itself as a scan that finished, for the pool it scanned: the scan \
             is per pool, so one pool's failing scans must not hide behind another's \
             successes"
        );
        let _ = std::fs::remove_file(&file);
        Ok(())
    }

    /// A `bad_debt` event cannot be emitted by the fork — the event is
    /// declared with zero call sites — so one means the pool is running
    /// stock wasm and every assumption the bot makes about default
    /// handling is void. It is an alarm, never something to act on.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_bad_debt_event_alarms_that_stock_wasm_is_deployed(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, _tick_rx) = tick_watch();
        let mut state = LoopState::default();
        let (instruments, recording) = recording_instruments();

        let event = PoolEvent::BadDebt {
            user: harness::USER_ONE.to_string(),
            asset: USDC.to_string(),
            d_tokens: 4_242,
        };
        handle_message(
            &tracker,
            &[],
            quiet_cadence(),
            &mut state,
            &shutdown,
            &instruments,
            &tick_tx,
            PollerMessage::Event {
                pool: harness::POOL.to_string(),
                ledger: 10,
                event: event.clone(),
            },
        )
        .await
        .expect("the event is applied");

        assert!(instruments.notifier.drain(DRAIN_BUDGET).await);
        let sent = recording.sent();
        assert_eq!(sent.len(), 1, "exactly one alarm: {sent:?}");
        assert_eq!(sent[0].kind, NotificationKind::StockWasmDetected);
        assert_eq!(
            sent[0].severity,
            Severity::High,
            "the wrong contract is deployed; this wakes someone"
        );
        assert_eq!(sent[0].pool, harness::POOL);
        assert_eq!(sent[0].account.as_deref(), Some(harness::USER_ONE));
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
        let instruments = Instruments::for_tests();
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
            &instruments,
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
        assert_eq!(
            instruments
                .metrics
                .pool_status(harness::POOL)
                .and_then(|status| status.processed),
            None,
            "nor recorded as processed: the range it covers will be read again"
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
        let instruments = Instruments::for_tests();

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
            &instruments,
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
            &instruments,
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
            &instruments,
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
        assert_eq!(
            instruments
                .metrics
                .pool_status(harness::POOL)
                .and_then(|status| status.processed),
            None,
            "a tick that declined its acknowledgement commits no cursor, so nothing records \
             it as this pool's last processed ledger"
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
            &instruments,
            &tick_tx,
            message,
        )
        .await
        .expect("a clean tick applies");
        assert!(
            applied.await.is_ok(),
            "with the mark cleared, a tick with no failure ahead of it is acknowledged"
        );
        assert_eq!(
            instruments
                .metrics
                .pool_status(harness::POOL)
                .and_then(|status| status.processed),
            Some(harness::fixture_tick().sequence),
            "and the acknowledged one is recorded"
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
            &Instruments::for_tests(),
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
        let instruments = Instruments::for_tests();

        let incomplete = seed_pools_needing_it(
            &client,
            &store,
            &[pool_config(harness::POOL, USDC, &["*"], &["*"])],
            &sources,
            20,
            std::time::Duration::from_millis(1),
            &shutdown,
            &instruments,
        )
        .await
        .expect("seeding succeeds");

        assert!(incomplete.is_empty(), "the one source answered");
        let rendered = instruments.metrics.render();
        assert!(
            rendered.contains(&format!(
                "seed_accounts_loaded{{pool=\"{}\"}} 1",
                harness::POOL
            )),
            "the seed publishes what it loaded, for this pool: {rendered}"
        );
        // The seed runs before this pool has a poller to report itself,
        // and can outlast `/livez`'s deadline on a busy pool: what it is
        // working on is alive, and says so.
        assert!(
            instruments
                .metrics
                .pool_status(harness::POOL)
                .and_then(|status| status.heartbeat)
                .is_some(),
            "seeding a pool heartbeats for it, so a restart probe cannot read the \
             seed as a poller that has stopped"
        );
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

    /// The seed heartbeats for **every** configured pool, not only the one
    /// it is working on.
    ///
    /// A pool the pass has not reached yet, or one it skipped because it
    /// needs no seed, has no poller either — nothing is spawned until the
    /// whole pass returns — so a heartbeat only for the pool being seeded
    /// leaves every other pool measured from `HttpState::started` alone.
    /// The moment that baseline passes, `/livez` fails for one of them and
    /// a restart probe kills the bot in the middle of the seed: the
    /// restart loop closed for the pool being seeded, still open for the
    /// rest.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_seed_heartbeats_for_every_configured_pool(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        // Followed already — a user and a cursor — so the pass skips it
        // without seeding, reaching the `continue` above the wrapper.
        store
            .upsert_user(&TrackedUser {
                pool: POOL_B.to_string(),
                account: harness::USER_TWO.to_string(),
                health_factor: 20_000_000,
                collateral: BTreeMap::new(),
                liabilities: BTreeMap::from([(0, 1)]),
                updated_ledger: tick.sequence,
                recheck_ledger: None,
            })
            .await
            .expect("seed a user for the followed pool");
        store
            .set_cursor(
                &events_cursor(POOL_B),
                &Cursor {
                    ledger: tick.sequence,
                    paging_token: None,
                },
            )
            .await
            .expect("a cursor for the followed pool");

        let rpc = ScriptedRpc::start().await;
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
        let instruments = Instruments::for_tests();

        // The followed pool first, so the pass has already `continue`d past
        // it by the time the second pool's seed starts stamping.
        let incomplete = seed_pools_needing_it(
            &client,
            &store,
            &[
                pool_config(POOL_B, USDC, &["*"], &["*"]),
                pool_config(harness::POOL, USDC, &["*"], &["*"]),
            ],
            &sources,
            20,
            std::time::Duration::from_millis(1),
            &shutdown,
            &instruments,
        )
        .await
        .expect("seeding succeeds");

        assert!(incomplete.is_empty(), "the one source answered");
        for pool in [harness::POOL, POOL_B] {
            assert!(
                instruments
                    .metrics
                    .pool_status(pool)
                    .and_then(|status| status.heartbeat)
                    .is_some(),
                "every configured pool is heartbeated while the seed runs, the ones it \
                 seeds and the ones it does not: {pool} has none"
            );
        }
        assert_eq!(rpc.remaining(), 0, "the followed pool cost no chain read");
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
            &tracker,
            &sources,
            cadence,
            &mut state,
            &shutdown,
            &Instruments::for_tests(),
            &tick_tx,
            message,
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
            &tracker,
            &sources,
            cadence,
            &mut state,
            &shutdown,
            &Instruments::for_tests(),
            &tick_tx,
            message,
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
            std::time::Duration::from_millis(1),
            &shutdown,
            &Instruments::for_tests(),
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
            std::time::Duration::from_millis(1),
            &shutdown,
            &Instruments::for_tests(),
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
            dry_run: true,
            own_addresses: BTreeSet::new(),
        }
    }

    /// The same for an armed bot — `DRY_RUN=false`, which is what its
    /// `creations` rows record whether or not a given one was sent.
    fn armed_config() -> AuctioneerConfig {
        AuctioneerConfig {
            dry_run: false,
            ..auctioneer_config()
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

    /// `PoolError::AuctionInProgress`: an auction for this user already
    /// exists — the benign refusal, and the cheapest one to script.
    const AUCTION_IN_PROGRESS: u32 = 1_212;

    /// The `simulateTransaction` answer for an operation the contract
    /// accepts.
    fn script_simulate_accepted(rpc: &ScriptedRpc) {
        rpc.expect(
            "simulateTransaction",
            json!({"transactionData": transaction_data_b64(10),
                   "events": [],
                   "minResourceFee": "10",
                   "results": [{"auth": [], "xdr": scval_b64(&ScVal::Void)}],
                   "latestLedger": 1_u32}),
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
            &Instruments::for_tests(),
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
            &Instruments::for_tests(),
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

    /// `users_tracked` follows the tick's own refresh, in both directions,
    /// with no full scan anywhere in this test (`quiet_cadence` disables
    /// it): a gauge only the full scan wrote trailed the store by up to a
    /// whole `FULL_SCAN_LEDGERS` period — about 100 minutes on testnet.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_tick_that_refreshes_accounts_regauges_users_tracked(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        // Tick one values USER_ONE's position; tick two's read no longer
        // holds it, so the refresh deletes the row.
        harness::script_snapshot(&rpc, &[harness::USER_ONE]);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, _tick_rx) = tick_watch();
        let instruments = Instruments::for_tests();
        let mut state = LoopState::default();
        let gauge = |count: u32| format!("users_tracked{{pool=\"{}\"}} {count}\n", harness::POOL);

        let tick = harness::fixture_tick();
        for (sequence, tracked) in [(tick.sequence, 1), (tick.sequence + 1, 0)] {
            handle_message(
                &tracker,
                &[],
                quiet_cadence(),
                &mut state,
                &shutdown,
                &instruments,
                &tick_tx,
                borrow(harness::POOL, harness::USER_ONE),
            )
            .await
            .expect("apply the event");
            let (message, applied) = tick_message(
                harness::POOL,
                LedgerTick {
                    sequence,
                    close_time: tick.close_time,
                },
            );
            handle_message(
                &tracker,
                &[],
                quiet_cadence(),
                &mut state,
                &shutdown,
                &instruments,
                &tick_tx,
                message,
            )
            .await
            .expect("apply the tick");
            assert!(applied.await.is_ok());
            assert_eq!(
                store.count_users(harness::POOL).await.expect("count"),
                i64::from(tracked)
            );
            let rendered = instruments.metrics.render();
            assert!(
                rendered.contains(&gauge(tracked)),
                "the gauge reads the store's count after tick {sequence}: {rendered}"
            );
        }
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
            &Instruments::for_tests(),
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
            &Instruments::for_tests(),
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
            &Instruments::for_tests(),
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
            Some(tick.sequence + 1),
            "the borrower nothing could decide is still flagged, one past this pass's ledger"
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
            &Instruments::for_tests(),
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
            &Instruments::for_tests(),
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
            Some(tick.sequence + 1),
            "the undecidable borrower is retried later, not dropped — one past this pass's ledger"
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
            &Instruments::for_tests(),
        )
        .await
        .expect("a failed action is this borrower's failure, not the pass's");

        assert_eq!(
            store
                .user(harness::POOL, &account)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence + 1),
            "still flagged, one past this pass's ledger"
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
    /// The refusal scripted here is the pool's own `AuctionInProgress`,
    /// the benign one; the one that is not benign — a percent walk
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
        script_simulate_refused(&rpc, AUCTION_IN_PROGRESS);
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
            &Instruments::for_tests(),
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
            Some(tick.sequence + 1),
            "a refused borrower keeps its flag, raised one past this pass's ledger — not \
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
            &Instruments::for_tests(),
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

        let Drained::Returned(result) = drain_tasks(tasks, &shutdown_tx).await else {
            panic!("no task panicked");
        };
        let error = result.expect_err("the failure is reported");
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

    /// A task's panic drains the rest exactly as an error does, and is what
    /// the run reports even when an error came too.
    ///
    /// Resumed the moment its `JoinHandle` answered, a panic would unwind
    /// out of `drain_tasks` and drop the `JoinSet` — aborting `run_queue`
    /// between a send and its outcome just as surely as returning on an
    /// error would. The watcher below returns only once shutdown is raised
    /// and records that it did, so an abort cannot pass for a drain. The
    /// error task returns only after the panic has raised shutdown, so
    /// both have happened by the time the run reports, and the panic, a
    /// bug, is what it reports.
    #[tokio::test]
    async fn a_panicking_task_shuts_the_rest_down_and_is_reported_over_an_error() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let finished = Arc::new(AtomicBool::new(false));
        let mut tasks: JoinSet<Result<(), LiquidatorError>> = JoinSet::new();

        let until_shutdown = |mut watcher: watch::Receiver<bool>| async move {
            while !*watcher.borrow_and_update() {
                if watcher.changed().await.is_err() {
                    break;
                }
            }
        };
        let flag = Arc::clone(&finished);
        let watcher = until_shutdown(shutdown_rx.clone());
        tasks.spawn(async move {
            watcher.await;
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });
        let watcher = until_shutdown(shutdown_rx.clone());
        tasks.spawn(async move {
            watcher.await;
            Err(LiquidatorError::Config(
                "failed after the panic".to_string(),
            ))
        });
        tasks.spawn(async { panic!("a task panicked") });

        let Drained::Panicked(payload) = drain_tasks(tasks, &shutdown_tx).await else {
            panic!("the panic is what the run reports");
        };
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"a task panicked"),
            "and it is the task's own panic, carried through unchanged"
        );
        assert!(
            finished.load(Ordering::SeqCst),
            "the other task ran to its own end rather than being aborted mid-flight"
        );
        assert!(
            *shutdown_rx.borrow(),
            "the panic raised the shutdown flag, as a failure does"
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

    /// An oracle scan the chain moved under is retried on the very next
    /// tick, not a whole `ORACLE_SCAN_LEDGERS` later: `LedgerMoved` is a
    /// race between reads, not a fault, and a price move the scan would
    /// have caught must not wait a period for it. Once only: a second move
    /// in a row waits the period, since an RPC whose answers keep
    /// straddling ledgers would otherwise cost every tick up to
    /// `SNAPSHOT_ATTEMPTS` snapshots. A success restores the retry, and any
    /// other failure waits a period from the start — retried every ledger,
    /// an oracle that has stopped answering would cost a read per ledger
    /// per pool for as long as it stays down.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_oracle_scan_the_ledger_moved_under_is_retried_once_next_tick(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let tick = harness::fixture_tick();
        // Every attempt `PoolReader::snapshot` makes is moved under, so the
        // refusal reaches the auctioneer.
        let moved_scan = |ledger: u32| {
            for _ in 0..crate::chain::pool::SNAPSHOT_ATTEMPTS {
                harness::script_moved_snapshot(&rpc, ledger);
            }
        };
        // `ScriptedRpc` answers each method first-in first-out, so these
        // are the five ticks' scans in order.
        moved_scan(tick.sequence);
        moved_scan(tick.sequence + 1);
        harness::script_snapshot(&rpc, &[]);
        moved_scan(tick.sequence + 21);
        rpc.expect_http("getLedgerEntries", 503);

        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), None);
        let (_flag, shutdown) = watch::channel(false);
        let (instruments, _recording) = recording_instruments();
        let cadence = AuctioneerCadence {
            refresh_batch: 10,
            oracle_scan_ledgers: 10,
            oracle_phase: 0,
            full_scan_ledgers: 0,
            full_phase: 0,
            scan_health_factor: 0,
            price_delta_bps: 100,
            startup_delay_ledgers: 0,
        };
        let pools = vec![harness::POOL.to_string()];
        let ctx = AuctioneerContext {
            store: &store,
            pools: &pools,
            auctioneer: &auctioneer,
            cadence,
            submission_queue: None,
            shutdown: &shutdown,
            instruments: &instruments,
        };
        let mut state = AuctioneerState::default();
        let at = |sequence: u32| LedgerTick {
            sequence,
            close_time: tick.close_time,
        };
        // Each step: the tick's offset from the fixture's ledger, the scan
        // ledger it must leave recorded, and why.
        let steps: [(u32, Option<u32>, &str); 5] = [
            (
                0,
                None,
                "a first move is left unrecorded, so the next tick is due",
            ),
            (
                1,
                Some(1),
                "a second move in a row is recorded, and waits a period",
            ),
            (
                11,
                Some(11),
                "the next period's scan succeeds and is recorded",
            ),
            (
                21,
                Some(11),
                "after a success, a move gets its one retry again",
            ),
            (
                22,
                Some(22),
                "any other failure is recorded, and waits a period",
            ),
        ];
        for (offset, recorded, why) in steps {
            auctioneer_tick(&ctx, at(tick.sequence + offset), &mut state)
                .await
                .expect("tick");
            assert_eq!(
                state.last_oracle_scan.get(harness::POOL).copied(),
                recorded.map(|offset| tick.sequence + offset),
                "tick +{offset}: {why}"
            );
        }
        assert_eq!(rpc.remaining(), 0, "every scripted read was made");
        Ok(())
    }

    /// Before `STARTUP_DELAY_LEDGERS` has elapsed nothing is submitted,
    /// even armed: a bot that has just started has the least state and the
    /// most reason to be wrong. The borrower it held fire on is not
    /// forgotten, either: the creation is recorded, the flag moves forward
    /// rather than being cleared, and the pass after the delay picks the
    /// same borrower up again on its own. Proven against a real
    /// `SubmissionQueue` and a real bad-debt decision, with only the
    /// deepest leaf — an actual submitted transaction — stood in for,
    /// exactly as `queue.rs`'s own tests stand in for `Submitter::submit`.
    #[sqlx::test(migrations = "./migrations")]
    async fn no_submission_before_the_startup_delay(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_debtor();
        let signer = test_signer();
        // One `decide` and one `act` per `auctioneer_tick` call below: the
        // snapshot the decision reads, then the signer's account entry and
        // the accepted simulation `act`'s bad-debt check makes — in that
        // order per method, because `ScriptedRpc` answers each method
        // first-in first-out.
        for _ in 0..2 {
            script_snapshot_bad_debt(&rpc, &account);
            script_account_entry(&rpc, &signer);
            script_simulate_accepted(&rpc);
        }
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let network = Network::testnet();
        let submitter = Submitter::new(&client, &network, &signer, test_tx_config());
        let auctioneer = Auctioneer::new(&client, &store, armed_config(), Some(submitter));
        let (_flag, shutdown) = watch::channel(false);

        let (queue, mut queue_rx) = SubmissionQueue::new(NonZeroUsize::new(8).expect("non-zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = queue_rx.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash: TxHash([9_u8; 32]),
                    ledger: 1,
                    return_value: None,
                }));
            }
        });

        let (instruments, recording) = recording_instruments();
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
            instruments: &instruments,
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
            !state.gate.unlocked,
            "the first tick is still inside the delay"
        );

        let recorded = sqlx::query!(
            "SELECT dry_run, tx_hash FROM creations WHERE pool = $1 AND account = $2 ORDER BY id",
            harness::POOL,
            account,
        )
        .fetch_all(store.pool())
        .await
        .expect("read the creation");
        assert_eq!(recorded.len(), 1, "the decision was still recorded");
        assert!(
            !recorded[0].dry_run,
            "the column is the configured mode, and this bot is armed"
        );
        assert!(
            recorded[0].tx_hash.is_none(),
            "but nothing was sent: the startup delay has not elapsed, so the row names no \
             transaction"
        );
        // Counted the same way the row reads: the bot decided and
        // recorded, which is an attempt, and nothing was sent, which is
        // no result at all — not a failure.
        let rendered = instruments.metrics.render();
        assert!(
            rendered.contains("creations_total{result=\"attempted\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"succeeded\"} 0"),
            "{rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"failed\"} 0"),
            "a creation the delay held back failed at nothing: {rendered}"
        );
        assert!(
            instruments
                .notifier
                .drain(std::time::Duration::from_secs(5))
                .await
        );
        assert_eq!(
            recording.sent_count(),
            0,
            "and nothing is reported for a creation that never reached the chain: {:?}",
            recording.sent()
        );

        // Held back, not settled: the bot still believes this is bad debt
        // and merely declined to send, so the flag moved forward instead
        // of being cleared, and the next pass reads it again without an
        // event, a price move or the full scan having to name it twice.
        assert_eq!(
            store
                .user(harness::POOL, &account)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence + 1),
            "a creation the delay held back is re-flagged one past the tick"
        );
        // One ledger on: the chain, not the wakeup count, is what the
        // delay measures, so this is exactly `startup_delay_ledgers`.
        let tick_two = LedgerTick {
            sequence: tick.sequence + 1,
            close_time: tick.close_time,
        };
        auctioneer_tick(&ctx, tick_two, &mut state)
            .await
            .expect("tick two");
        assert!(state.gate.unlocked, "the second tick is past the delay");

        let recorded = sqlx::query!(
            "SELECT dry_run, tx_hash FROM creations WHERE pool = $1 AND account = $2 ORDER BY id",
            harness::POOL,
            account,
        )
        .fetch_all(store.pool())
        .await
        .expect("read the creations");
        assert_eq!(recorded.len(), 2);
        assert!(
            recorded[1].tx_hash.is_some(),
            "once the delay has elapsed, the same decision is actually submitted"
        );
        assert_eq!(
            store
                .user(harness::POOL, &account)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            None,
            "sent and succeeded, the borrower is settled and its flag cleared"
        );
        let rendered = instruments.metrics.render();
        assert!(
            rendered.contains("creations_total{result=\"attempted\"} 2"),
            "{rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"succeeded\"} 1"),
            "the transaction the chain applied is the one that succeeded: {rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"failed\"} 0"),
            "and it is not also counted as a failure: {rendered}"
        );
        assert!(
            instruments
                .notifier
                .drain(std::time::Duration::from_secs(5))
                .await
        );
        let sent = recording.sent();
        assert_eq!(
            sent.len(),
            1,
            "the creation that landed is reported, and only it: {sent:?}"
        );
        assert_eq!(sent[0].kind, NotificationKind::BadDebtReported);
        assert_eq!(
            sent[0].severity,
            Severity::Medium,
            "bad debt is worth looking at, not worth waking anyone"
        );
        assert_eq!(sent[0].pool, harness::POOL);
        assert_eq!(sent[0].account.as_deref(), Some(account.as_str()));
        assert!(
            sent[0].message.contains("bad debt reported in ledger 1"),
            "the message names the ledger it landed in: {}",
            sent[0].message
        );

        drop(queue);
        worker.await.expect("worker");
        Ok(())
    }

    /// One watch serves every pool, so a poller that fell behind must not
    /// move it backwards: the auctioneer values every pool's flagged
    /// borrowers at the published tick, and a ledger sequence is the
    /// network's, so "newer" is well-defined across pools.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_lagging_pool_cannot_move_the_watch_backwards(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let (_flag, shutdown) = watch::channel(false);
        let (tick_tx, tick_rx) = tick_watch();
        let mut state = LoopState::default();

        let newer = LedgerTick {
            sequence: 100,
            close_time: 1_000,
        };
        let older = LedgerTick {
            sequence: 99,
            close_time: 990,
        };
        for (pool, tick) in [(harness::POOL, newer), (POOL_B, older)] {
            let (message, applied) = tick_message(pool, tick);
            handle_message(
                &tracker,
                &[],
                quiet_cadence(),
                &mut state,
                &shutdown,
                &Instruments::for_tests(),
                &tick_tx,
                message,
            )
            .await
            .expect("apply the tick");
            assert!(applied.await.is_ok(), "both ticks are acknowledged");
        }
        assert_eq!(
            *tick_rx.borrow(),
            newer,
            "the older tick from the lagging pool did not overwrite the newer one"
        );
        Ok(())
    }

    /// The tracker raises flags at the very tick the auctioneer's pass runs
    /// on, so `flagged_at == tick.sequence` is the ordinary case. A retry
    /// written back at the tick would leave the row exactly where it was —
    /// oldest, by account order, first in every following batch — which is
    /// the starvation the forward move exists to end.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_flag_raised_on_this_tick_still_moves_behind_it(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let stuck = synthetic_account(60);
        let behind = synthetic_account(61);
        script_snapshot_positions(
            &rpc,
            &[(
                stuck.as_str(),
                positions_entry_xdr(&stuck, &[(99, 10_000_000_000)]),
            )],
        );
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, auctioneer_config(), None);
        let (_flag, shutdown) = watch::channel(false);
        let tick = harness::fixture_tick();

        // Both flagged on this very tick, as the tracker would have.
        for account in [&stuck, &behind] {
            store
                .upsert_user(&tracked_user(account, tick.sequence))
                .await
                .expect("seed the row");
            store
                .flag_recheck(harness::POOL, account, tick.sequence)
                .await
                .expect("flag");
        }
        let batch = vec![store
            .user(harness::POOL, &stuck)
            .await
            .expect("read")
            .expect("a row")];
        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &batch,
            tick,
            None,
            &shutdown,
            &Instruments::for_tests(),
        )
        .await
        .expect("one undecidable borrower does not fail the pass");

        assert_eq!(
            store
                .user(harness::POOL, &stuck)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence + 1),
            "the retry sorts one past the tick it could not be decided on"
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
            "a borrower flagged on the same tick now sorts ahead of the retry"
        );
        Ok(())
    }

    /// A submission the chain failed leaves the borrower owed an auction:
    /// none exists, the borrower is still liquidatable, and clearing its
    /// flag would forget it until the next full scan. The flag moves
    /// forward instead, exactly as a refusal's does — and the attempt is
    /// still on the audit, hash and all.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_failed_submission_keeps_the_borrowers_flag(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_debtor();
        let signer = test_signer();
        script_snapshot_bad_debt(&rpc, &account);
        script_account_entry(&rpc, &signer);
        script_simulate_accepted(&rpc);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let network = Network::testnet();
        let submitter = Submitter::new(&client, &network, &signer, test_tx_config());
        let auctioneer = Auctioneer::new(&client, &store, armed_config(), Some(submitter));
        let (_flag, shutdown) = watch::channel(false);
        let tick = harness::fixture_tick();

        // The queue's worker answers with a transaction the chain applied
        // and failed: a fee was charged, no auction exists.
        let (queue, mut queue_rx) = SubmissionQueue::new(NonZeroUsize::new(8).expect("non-zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = queue_rx.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Failed {
                    hash: TxHash([9_u8; 32]),
                    ledger: 1,
                    contract_error: Some(1_205),
                    result: TransactionResult {
                        fee_charged: 100,
                        result: TransactionResultResult::TxFailed(VecM::default()),
                        ext: TransactionResultExt::V0,
                    },
                }));
            }
        });

        store
            .upsert_user(&tracked_user(&account, tick.sequence))
            .await
            .expect("seed the row");
        store
            .flag_recheck(harness::POOL, &account, tick.sequence)
            .await
            .expect("flag");
        let batch = store
            .users_needing_recheck(harness::POOL, 1)
            .await
            .expect("read the recheck queue");
        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &batch,
            tick,
            Some(&queue),
            &shutdown,
            &Instruments::for_tests(),
        )
        .await
        .expect("a failed submission is one borrower's, not the pass's");
        drop(queue);
        worker.await.expect("worker");

        assert_eq!(
            store
                .user(harness::POOL, &account)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence + 1),
            "a failed submission leaves the borrower owed, so the flag moves forward rather \
             than clearing"
        );
        let row = sqlx::query!(
            "SELECT tx_hash FROM creations WHERE account = $1",
            account.as_str(),
        )
        .fetch_one(store.pool())
        .await
        .expect("the attempt was recorded");
        assert!(
            row.tx_hash.is_some(),
            "with the hash of the transaction that failed"
        );
        Ok(())
    }

    /// A creation the queue could not carry through at all is counted as
    /// a failure and reported. [`QueueError::Chain`] is narrowed to a
    /// submission that provably sent nothing and has spent its retry
    /// budget, so this creation is gone rather than pending — while the
    /// borrower keeps its flag exactly as every other unsettled one does,
    /// because the report is instrumenting and never a decision.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_creation_the_queue_dropped_notifies_submission_dropped(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_debtor();
        let signer = test_signer();
        script_snapshot_bad_debt(&rpc, &account);
        script_account_entry(&rpc, &signer);
        script_simulate_accepted(&rpc);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let network = Network::testnet();
        let submitter = Submitter::new(&client, &network, &signer, test_tx_config());
        let auctioneer = Auctioneer::new(&client, &store, armed_config(), Some(submitter));
        let (_flag, shutdown) = watch::channel(false);
        let (instruments, recording) = recording_instruments();
        let tick = harness::fixture_tick();

        // The shape `run_queue` answers when the send was refused
        // outright: no envelope reached the chain, and the budget this
        // submission carried is spent.
        let (queue, mut queue_rx) = SubmissionQueue::new(NonZeroUsize::new(8).expect("non-zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = queue_rx.recv().await {
                let _ = queued
                    .respond
                    .send(Err(QueueError::Chain(ChainError::Rejected(
                        "tx_insufficient_fee".to_string(),
                    ))));
            }
        });

        store
            .upsert_user(&tracked_user(&account, tick.sequence))
            .await
            .expect("seed the row");
        store
            .flag_recheck(harness::POOL, &account, tick.sequence)
            .await
            .expect("flag");
        let batch = store
            .users_needing_recheck(harness::POOL, 1)
            .await
            .expect("read the recheck queue");
        recheck_batch(
            &auctioneer,
            &store,
            harness::POOL,
            &batch,
            tick,
            Some(&queue),
            &shutdown,
            &instruments,
        )
        .await
        .expect("a dropped creation is one borrower's, not the pass's");
        drop(queue);
        worker.await.expect("worker");

        assert_eq!(
            store
                .user(harness::POOL, &account)
                .await
                .expect("read")
                .and_then(|user| user.recheck_ledger),
            Some(tick.sequence + 1),
            "the borrower is still owed a creation, so its flag moves forward rather than \
             being cleared"
        );
        let rendered = instruments.metrics.render();
        assert!(
            rendered.contains("creations_total{result=\"attempted\"} 1"),
            "the `creations` row was written before the queue was asked, so the bot did \
             attempt this one: {rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"failed\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"succeeded\"} 0"),
            "{rendered}"
        );
        assert!(
            instruments
                .notifier
                .drain(std::time::Duration::from_secs(5))
                .await
        );
        let sent = recording.sent();
        assert_eq!(sent.len(), 1, "one dropped creation, one report: {sent:?}");
        assert_eq!(sent[0].kind, NotificationKind::SubmissionDropped);
        assert_eq!(
            sent[0].severity,
            Severity::High,
            "a creation this bot decided on and never made is money at stake"
        );
        assert_eq!(sent[0].pool, harness::POOL);
        assert_eq!(sent[0].account.as_deref(), Some(account.as_str()));
        assert!(
            sent[0].message.contains("rejected at send"),
            "the queue's own error says what happened: {}",
            sent[0].message
        );
        assert_eq!(
            rpc.remaining(),
            0,
            "every scripted answer was used: the decision and the simulation both ran"
        );
        Ok(())
    }

    /// A [`CreationOutcome`] shaped as [`Auctioneer::act`] answers one.
    /// `seed` gives each case its own account, because the notifier
    /// deduplicates by `(pool, account, kind)` and a shared account would
    /// silence the second case of a kind rather than test it.
    fn creation_outcome(
        kind: CreationKind,
        seed: u8,
        percent: Option<u32>,
        submission: Option<TxOutcome>,
    ) -> CreationOutcome {
        CreationOutcome {
            kind,
            account: synthetic_account(seed),
            percent: percent.map(|value| FillPercent::try_from(value).expect("1..=100")),
            simulated: true,
            creation_id: 1,
            dry_run: false,
            submission,
        }
    }

    /// The submission a queue answers for a transaction the chain applied
    /// in `ledger`.
    fn landed(ledger: u32) -> TxOutcome {
        TxOutcome::Succeeded {
            hash: TxHash([9_u8; 32]),
            ledger,
            return_value: None,
        }
    }

    /// A creation the chain landed is counted as an attempt *and* a
    /// success, and reported under the kind spec §7 names for it — an
    /// auction at its percent, bad debt at the severity its own kind
    /// carries.
    #[tokio::test]
    async fn a_landed_creation_counts_and_notifies() {
        let (instruments, recording) = recording_instruments();
        note_creation(
            &instruments,
            harness::POOL,
            &creation_outcome(CreationKind::Auction, 1, Some(42), Some(landed(7))),
        );
        note_creation(
            &instruments,
            harness::POOL,
            &creation_outcome(CreationKind::BadDebt, 2, None, Some(landed(9))),
        );
        note_creation(
            &instruments,
            harness::POOL,
            &creation_outcome(CreationKind::Auction, 3, None, Some(landed(11))),
        );

        let rendered = instruments.metrics.render();
        assert!(
            rendered.contains("creations_total{result=\"attempted\"} 3"),
            "every recorded creation is an attempt: {rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"succeeded\"} 3"),
            "{rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"failed\"} 0"),
            "a transaction the chain applied is not also a failure: {rendered}"
        );

        assert!(
            instruments
                .notifier
                .drain(std::time::Duration::from_secs(5))
                .await
        );
        let sent = recording.sent();
        assert_eq!(sent.len(), 3, "one report each: {sent:?}");
        let about = |seed: u8| {
            let account = synthetic_account(seed);
            sent.iter()
                .find(|notification| notification.account.as_deref() == Some(account.as_str()))
                .cloned()
                .unwrap_or_else(|| panic!("a notification about {account}: {sent:?}"))
        };
        let auction = about(1);
        assert_eq!(auction.kind, NotificationKind::AuctionCreated);
        assert_eq!(auction.severity, Severity::Low);
        assert_eq!(auction.pool, harness::POOL);
        assert_eq!(
            auction.message,
            "liquidation auction created at 42% in ledger 7"
        );
        let bad_debt = about(2);
        assert_eq!(bad_debt.kind, NotificationKind::BadDebtReported);
        assert_eq!(
            bad_debt.severity,
            Severity::Medium,
            "bad debt is worth looking at, not worth waking anyone"
        );
        assert_eq!(bad_debt.message, "bad debt reported in ledger 9");
        assert_eq!(
            about(3).message,
            "liquidation auction created at ?% in ledger 11",
            "the percent is optional on the type, and its absence is said rather than guessed"
        );
    }

    /// A creation the chain did not land is still an attempt, and is
    /// reported to nobody: only what landed is worth a notification.
    ///
    /// The split between the two counted results is the chain's answer,
    /// never the row's existence. `Failed` charged a fee and created no
    /// auction and `Expired` provably never applied, so both are
    /// failures; `Unknown` may still land, and a counter that guessed
    /// would have to be un-counted; `None` was never sent at all — a dry
    /// run's creation, or an armed bot holding fire.
    #[tokio::test]
    async fn a_creation_the_chain_did_not_land_counts_but_notifies_nobody() {
        let (instruments, recording) = recording_instruments();
        let window = LedgerWindow::try_new(1, 100).expect("a window");
        for submission in [
            Some(TxOutcome::Failed {
                hash: TxHash([1_u8; 32]),
                ledger: 12,
                contract_error: Some(1_205),
                result: TransactionResult {
                    fee_charged: 100,
                    result: TransactionResultResult::TxFailed(VecM::default()),
                    ext: TransactionResultExt::V0,
                },
            }),
            Some(TxOutcome::Expired {
                hash: TxHash([2_u8; 32]),
                window,
                latest_ledger: 13,
            }),
            Some(TxOutcome::Unknown {
                hash: TxHash([3_u8; 32]),
                sequence: 5,
                window,
            }),
            None,
        ] {
            note_creation(
                &instruments,
                harness::POOL,
                &creation_outcome(CreationKind::Auction, 4, Some(50), submission),
            );
        }

        let rendered = instruments.metrics.render();
        assert!(
            rendered.contains("creations_total{result=\"attempted\"} 4"),
            "{rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"succeeded\"} 0"),
            "{rendered}"
        );
        assert!(
            rendered.contains("creations_total{result=\"failed\"} 2"),
            "the failed and the expired one; the unresolved one and the one nothing sent are \
             neither: {rendered}"
        );
        assert!(
            instruments
                .notifier
                .drain(std::time::Duration::from_secs(5))
                .await
        );
        assert_eq!(
            recording.sent_count(),
            0,
            "nothing landed, so nothing is reported: {:?}",
            recording.sent()
        );
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
            &Instruments::for_tests(),
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
            &Instruments::for_tests(),
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
            &Instruments::for_tests(),
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
            &Instruments::for_tests(),
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

    /// The fixture pool's first reserve, which is also mainnet's native
    /// asset contract: what [`Inventory`] withholds the fee reserve from
    /// and what `validate_filler` reads the filler's balance of.
    const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";

    /// The auction the two loop tests plan: 100,000 XLM of b-tokens
    /// against 20,000,000,000 USDC d-tokens, the same one `filler.rs`'s
    /// own tests use — a lot worth several times its bid, so every
    /// decision here is about the loop rather than about the auction.
    const FILLER_LOT: i128 = 1_000_000_000_000;
    const FILLER_BID: i128 = 20_000_000_000;

    /// A second key, distinct from [`test_signer`]'s. Ruling 2's two-key
    /// case needs two addresses that are really different, and `Signers`
    /// tells the shared case from the distinct one by `Arc` pointer.
    fn filler_signer() -> Signer {
        let key = ed25519_dalek::SigningKey::from_bytes(&[11_u8; 32]);
        let secret = stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string();
        Signer::from_secret(&secret).expect("signer")
    }

    /// A [`SigningContext`] built by hand, since the real one consumes a
    /// [`SigningKeys`] whose fallback these tests need to vary
    /// independently: `own_addresses` is derived from both roles exactly
    /// as [`SigningKeys::own_addresses`] derives it.
    fn signing_context(
        auctioneer: Option<Arc<Signer>>,
        filler: Option<Arc<Signer>>,
    ) -> SigningContext {
        let own_addresses = [auctioneer.as_deref(), filler.as_deref()]
            .into_iter()
            .flatten()
            .map(|signer| signer.address().to_string())
            .collect();
        SigningContext {
            network: Network::testnet(),
            tx_config: test_tx_config(),
            signers: Signers { auctioneer, filler },
            own_addresses,
            native_asset: XLM.to_string(),
        }
    }

    /// The fixture pool, taking every asset, with XLM as the primary
    /// collateral `min_primary_collateral` is measured in.
    fn filler_pool_config(min_primary_collateral: i128) -> PoolConfig {
        PoolConfig {
            address: harness::POOL.to_string(),
            primary_asset: XLM.to_string(),
            min_primary_collateral,
            min_health_factor: 15_000_000,
            default_profit_bps: 1_000,
            force_fill: false,
            // Deliberately not the crate's own default (`FreeFill`): the
            // tests built on this fixture (startup delay, the published
            // tick, unwind timing) are about the loop's wiring, not about
            // which ledger a fill aims at, and their expected fill ledgers
            // are `EarliestProfitable`'s. `FreeFill` would move every one
            // of them out to `start + RAMP_END_BLOCKS` for no reason the
            // test's own name explains.
            fill_objective: FillObjective::EarliestProfitable,
            supported_bid: vec!["*".to_string()],
            supported_lot: vec!["*".to_string()],
            profits: Vec::new(),
        }
    }

    /// A whole [`ServiceConfig`], for the three `validate_filler` tests.
    /// Only `pools`, `dry_run` and `xlm_fee_reserve` are ever read by it;
    /// the rest is the design spec's defaults, and nothing here connects
    /// to the database or the RPC URL it names.
    fn filler_service_config(
        pools: Vec<PoolConfig>,
        dry_run: bool,
        xlm_fee_reserve: u64,
    ) -> ServiceConfig {
        ServiceConfig {
            chain: ChainConfig {
                network_passphrase: "Test SDF Network ; September 2015".to_string(),
                rpc_url: "http://127.0.0.1:1".to_string(),
                rpc_api_key: None,
                base_fee: 100,
                high_fee: 200,
                tx_poll_ledgers: 3,
            },
            database_url: Secret::new("postgres://unused"),
            database_max_connections: 1,
            pools,
            run_mode: RunMode::Loop,
            dry_run,
            poll_interval: std::time::Duration::from_millis(1),
            user_refresh_ledgers: 100,
            refresh_batch: 10,
            full_scan_ledgers: 100,
            scan_health_factor: 11_000_000,
            liquidation_health_factor: 9_980_000,
            target_health_factor: 10_600_000,
            oracle_scan_ledgers: 10,
            price_delta_bps: 100,
            plan_iterations: 5,
            startup_delay_ledgers: 0,
            seed: SeedConfig {
                url: None,
                health_factor_max: 20_000_000,
                file: None,
            },
            hf_safety_multiplier: 11_000_000,
            replan_ledgers: 10,
            replan_near_ledgers: 5,
            xlm_fee_reserve,
            high_fee_profit_threshold: 1_000_000_000_000_000,
            inventory_refresh: std::time::Duration::from_secs(30),
            notification_cooldown: std::time::Duration::from_hours(24),
            http: None,
            telegram: None,
        }
    }

    /// The keyless dry-run filler the two loop tests run: no signer, so
    /// nothing is simulated and every `fills` row it writes is a dry-run
    /// record.
    fn filler_tick_config() -> FillerConfig {
        FillerConfig {
            dry_run: true,
            own_addresses: BTreeSet::new(),
            hf_safety_multiplier: 11_000_000,
            plan_iterations: 5,
            replan_ledgers: 10,
            replan_near_ledgers: 5,
            high_fee_profit_threshold: 1_000_000_000_000_000,
            inventory_refresh: std::time::Duration::from_secs(30),
            native_asset: XLM.to_string(),
        }
    }

    /// The auction as the chain holds it, starting at `block`.
    fn filler_auction(block: u32) -> AuctionData {
        AuctionData {
            bid: BTreeMap::from([(USDC.to_string(), FILLER_BID)]),
            lot: BTreeMap::from([(XLM.to_string(), FILLER_LOT)]),
            block,
        }
    }

    /// The row the tracker would have written for it: no plan yet, and
    /// the amounts the chain holds.
    fn tracked_auction(account: &str, auction: &AuctionData) -> TrackedAuction {
        TrackedAuction {
            pool: harness::POOL.to_string(),
            account: account.to_string(),
            auction_type: AuctionType::UserLiquidation,
            start_ledger: auction.block,
            fill_ledger: None,
            percent: None,
            bid: auction.bid.clone(),
            lot: auction.lot.clone(),
            updated_ledger: auction.block,
        }
    }

    /// Joins every queue worker `spawn_queues` started, asserting each
    /// returned on its own rather than being aborted with the set.
    async fn drain_queue_workers(mut tasks: JoinSet<Result<(), LiquidatorError>>) {
        while let Some(outcome) = tasks.join_next().await {
            outcome.expect("join").expect("the queue worker returned");
        }
    }

    /// Ruling 2: one key, one queue. The fallback auctioneer shares the
    /// filler's; two keys are two workers; a dry run starts none.
    #[tokio::test]
    async fn one_key_is_one_queue() {
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let (_flag, shutdown) = watch::channel(false);

        // One key in both roles — what `SigningKeys::into_signers` hands
        // back when `AUCTIONEER_SECRET_KEY` is unset. One worker, and the
        // auctioneer submits through the filler's own queue: a second
        // queue on this key would prepare against the same sequence
        // number.
        let shared = Arc::new(test_signer());
        let mut tasks = JoinSet::new();
        let queues = spawn_queues(
            &mut tasks,
            &client,
            &signing_context(Some(Arc::clone(&shared)), Some(shared)),
            false,
            &shutdown,
        );
        assert_eq!(tasks.len(), 1, "one key is one worker, never two");
        assert!(
            queues.auctioneer.is_some() && queues.filler.is_some(),
            "both roles submit, through the one queue"
        );
        drop(queues);
        drain_queue_workers(tasks).await;

        // Two distinct keys: a worker each. Sharing one would serialise
        // two sequence numbers that never race.
        let mut tasks = JoinSet::new();
        let queues = spawn_queues(
            &mut tasks,
            &client,
            &signing_context(
                Some(Arc::new(test_signer())),
                Some(Arc::new(filler_signer())),
            ),
            false,
            &shutdown,
        );
        assert_eq!(tasks.len(), 2, "two keys are two workers");
        assert!(queues.auctioneer.is_some() && queues.filler.is_some());
        drop(queues);
        drain_queue_workers(tasks).await;

        // Dry run: no worker, and no queue to hand either task — the
        // safety invariant, asserted at the one place that could break it.
        let mut tasks = JoinSet::new();
        let queues = spawn_queues(
            &mut tasks,
            &client,
            &signing_context(
                Some(Arc::new(test_signer())),
                Some(Arc::new(filler_signer())),
            ),
            true,
            &shutdown,
        );
        assert_eq!(tasks.len(), 0, "a dry run spawns no submission worker");
        assert!(queues.auctioneer.is_none() && queues.filler.is_none());

        // Armed with no key at all: `Args::signing_keys` refuses this, so
        // it is reachable only by hand — and it still starts nothing.
        let mut tasks = JoinSet::new();
        let queues = spawn_queues(
            &mut tasks,
            &client,
            &signing_context(None, None),
            false,
            &shutdown,
        );
        assert_eq!(tasks.len(), 0, "no key is no worker");
        assert!(queues.auctioneer.is_none() && queues.filler.is_none());
    }

    /// Spec §6: an armed filler whose account does not exist cannot start.
    #[tokio::test]
    async fn an_armed_filler_with_no_account_is_refused() {
        let signer = Arc::new(filler_signer());
        let address = signer.address().to_string();
        let pools = vec![filler_pool_config(0)];
        let rpc = ScriptedRpc::start().await;
        // One account read per call below, both answering no entry —
        // which is exactly what an unfunded account looks like.
        for _ in 0..2 {
            rpc.expect(
                "getLedgerEntries",
                json!({"latestLedger": 1_u32, "entries": []}),
            );
        }
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let signing = signing_context(Some(Arc::clone(&signer)), Some(signer));

        let refused = validate_filler(
            &client,
            &filler_service_config(pools.clone(), false, 0),
            &signing,
        )
        .await
        .expect_err("an armed filler with no account cannot start");
        assert!(
            matches!(&refused, LiquidatorError::Config(message) if message.contains(&address)),
            "the refusal names the account: {refused}"
        );

        let warnings = validate_filler(&client, &filler_service_config(pools, true, 0), &signing)
            .await
            .expect("a dry run reports it and carries on");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains(&address), "{warnings:?}");
        assert_eq!(rpc.remaining(), 0);
    }

    /// Spec §6: nor can one without its fee reserve.
    #[tokio::test]
    async fn an_armed_filler_short_of_its_fee_reserve_is_refused() {
        let signer = Arc::new(filler_signer());
        let pools = vec![filler_pool_config(0)];
        let reserve: u64 = 100_000_000;
        let rpc = ScriptedRpc::start().await;
        // The account exists; its native balance is one stroop short.
        for _ in 0..2 {
            script_account_entry(&rpc, &signer);
            rpc.expect(
                "simulateTransaction",
                simulation(&scval_b64(&i128_val(i128::from(reserve) - 1)), 1),
            );
        }
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let signing = signing_context(Some(Arc::clone(&signer)), Some(signer));

        let refused = validate_filler(
            &client,
            &filler_service_config(pools.clone(), false, reserve),
            &signing,
        )
        .await
        .expect_err("an armed filler below its fee reserve cannot start");
        assert!(
            matches!(&refused, LiquidatorError::Config(message)
                if message.contains("XLM_FEE_RESERVE")),
            "the refusal names the knob an operator sets: {refused}"
        );

        let warnings = validate_filler(
            &client,
            &filler_service_config(pools, true, reserve),
            &signing,
        )
        .await
        .expect("a dry run reports it and carries on");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("XLM_FEE_RESERVE"), "{warnings:?}");
        assert_eq!(rpc.remaining(), 0);
    }

    /// Spec §6: short of a pool's primary floor is a warning, not a
    /// refusal — the filler can still fill, it just cannot fill as much.
    #[tokio::test]
    async fn an_armed_filler_under_its_primary_floor_is_warned() {
        let signer = Arc::new(filler_signer());
        let floor: i128 = 1_000_000_000;
        let pools = vec![filler_pool_config(floor)];
        let rpc = ScriptedRpc::start().await;
        script_account_entry(&rpc, &signer);
        rpc.expect(
            "simulateTransaction",
            simulation(&scval_b64(&i128_val(100_000_000)), 1),
        );
        // The fixture's own pool, holding no position for this key at
        // all: zero primary collateral, so the shortfall is the whole
        // floor.
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let signing = signing_context(Some(Arc::clone(&signer)), Some(signer));

        let warnings = validate_filler(
            &client,
            &filler_service_config(pools, false, 100_000_000),
            &signing,
        )
        .await
        .expect("a shortfall is a warning, not a refusal");

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains(harness::POOL) && warnings[0].contains(&floor.to_string()),
            "the warning names the pool and the shortfall: {warnings:?}"
        );
        assert_eq!(rpc.remaining(), 0);
    }

    /// The filler runs off the tracker's published tick, like the
    /// auctioneer: a due auction in the store becomes a `fills` row, and
    /// the loop returns when the watch's sender drops.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_filler_runs_off_the_published_tick(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = filler_auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked_auction(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![filler_pool_config(0)];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_tick_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
            Arc::new(Notifier::log_only(std::time::Duration::from_hours(1))),
            Arc::new(Metrics::new()),
        );
        let (flag_tx, flag_rx) = watch::channel(false);
        let (tick_tx, tick_rx) = watch::channel(LedgerTick {
            sequence: 0,
            close_time: 0,
        });

        let driver = async {
            tick_tx.send(tick).expect("publish the tick");
            let mut filled = false;
            for _ in 0..200 {
                let rows = sqlx::query!("SELECT count(*) AS n FROM fills")
                    .fetch_one(store.pool())
                    .await
                    .expect("count the fills");
                if rows.n == Some(1) {
                    filled = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(filled, "the published tick never reached the filler");
            flag_tx.send(true).expect("raise shutdown");
            drop(tick_tx);
        };
        let (outcome, ()) = tokio::join!(filler_loop(&filler, 0, None, tick_rx, &flag_rx), driver);
        outcome.expect("the loop returned when its sender dropped");

        let fill = sqlx::query!("SELECT dry_run, tx_hash, fill_ledger, percent FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert!(fill.dry_run, "no key, and dry-run: nothing was sent");
        assert_eq!(fill.tx_hash, None);
        assert_eq!(
            (fill.fill_ledger, fill.percent),
            (i64::from(tick.sequence + 1), 100)
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Ruling 9 through the loop: inside the startup delay the plan lands
    /// on the row and nothing is recorded.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_filler_waits_out_the_startup_delay(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = filler_auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked_auction(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![filler_pool_config(0)];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_tick_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
            Arc::new(Notifier::log_only(std::time::Duration::from_hours(1))),
            Arc::new(Metrics::new()),
        );
        let (flag_tx, flag_rx) = watch::channel(false);
        let (tick_tx, tick_rx) = watch::channel(LedgerTick {
            sequence: 0,
            close_time: 0,
        });

        let driver = async {
            tick_tx.send(tick).expect("publish the tick");
            let mut planned = None;
            for _ in 0..200 {
                planned = store
                    .auction(
                        harness::POOL,
                        harness::USER_ONE,
                        AuctionType::UserLiquidation,
                    )
                    .await
                    .expect("read the auction row")
                    .and_then(|row| row.fill_ledger);
                if planned.is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                planned,
                Some(tick.sequence + 1),
                "the plan is made; the execution is what the startup delay holds back"
            );
            flag_tx.send(true).expect("raise shutdown");
            drop(tick_tx);
        };
        // Five ledgers of delay against a single tick: `observe` can never
        // unlock, so the loop plans and executes nothing.
        let (outcome, ()) = tokio::join!(filler_loop(&filler, 5, None, tick_rx, &flag_rx), driver);
        outcome.expect("the loop returned when its sender dropped");

        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(0), "nothing is recorded inside the delay");
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The filler loop unwinds a pool it holds a position in on its first
    /// tick, dry-run: the `unwind planned` path runs off the published tick
    /// like everything else in this task.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_filler_loop_plans_a_startup_unwind(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // No auctions are seeded, so the fill walk reads nothing and the
        // only chain traffic is the startup unwind pass: a position above
        // the pool's (zero) primary floor, read for the filler's own
        // account.
        harness::script_snapshot_positions(
            &rpc,
            &[(
                signer.address(),
                harness::positions_entry_xdr(signer.address(), &[(0, 5_000_000_000_000)], &[]),
            )],
        );
        harness::script_empty_wallet(&rpc, tick.sequence);
        crate::chain::script::script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        crate::chain::script::script_simulate_accepted(&rpc, tick.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, test_tx_config());
        let pools = vec![filler_pool_config(0)];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_tick_config(),
            // Keyed and dry-run: the judgment still simulates through the
            // key, and nothing is queued.
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
            Arc::new(Notifier::log_only(std::time::Duration::from_hours(1))),
            Arc::new(Metrics::new()),
        );
        let (flag_tx, flag_rx) = watch::channel(false);
        let (tick_tx, tick_rx) = watch::channel(LedgerTick {
            sequence: 0,
            close_time: 0,
        });

        let positions_key =
            to_base64(&keys::positions(harness::POOL, signer.address()).expect("positions key"))
                .expect("key encodes");
        let driver = async {
            tick_tx.send(tick).expect("publish the tick");
            let mut read = false;
            for _ in 0..200 {
                read = rpc.calls("getLedgerEntries").iter().any(|params| {
                    params["keys"].as_array().is_some_and(|keys| {
                        keys.iter().any(|key| key.as_str() == Some(&positions_key))
                    })
                });
                if read {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(
                read,
                "the loop never read the filler's own account positions"
            );
            flag_tx.send(true).expect("raise shutdown");
            drop(tick_tx);
        };
        let (outcome, ()) = tokio::join!(filler_loop(&filler, 0, None, tick_rx, &flag_rx), driver);
        outcome.expect("the loop returned when its sender dropped");

        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The watchdog notifies about a poller whose heartbeat has aged past
    /// [`PollerConfig::liveness_deadline`], once per cooldown, and says
    /// nothing about one that keeps heartbeating — nor about a pool with
    /// no heartbeat at all, which is a poller that has not started rather
    /// than one that has stopped.
    #[tokio::test]
    async fn the_watchdog_notifies_a_stalled_poller_once_per_cooldown() {
        const STALLED: &str = "pool-stalled";
        const ALIVE: &str = "pool-alive";
        const SILENT: &str = "pool-never-started";

        let metrics = Arc::new(Metrics::new());
        // `max_backoff` zero makes the deadline exactly five intervals —
        // 100 ms — so the stale stamp below is four deadlines old and the
        // live pool's is never more than one interval old. The margins are
        // this wide on purpose: a loaded CI runner that starves the
        // stamping task below for a few tens of milliseconds would
        // otherwise turn the "alive" assertion into a flake.
        let config = PollerConfig {
            poll_interval: std::time::Duration::from_millis(20),
            page_limit: 200,
            min_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::ZERO,
        };
        assert_eq!(
            config.liveness_deadline(),
            std::time::Duration::from_millis(100)
        );
        metrics.heartbeat_at(
            STALLED,
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_millis(400))
                .expect("a recent instant"),
            std::time::SystemTime::now(),
        );
        // A pool whose poller is running: its own task keeps stamping,
        // which is what a live loop does every iteration.
        let alive = tokio::spawn({
            let metrics = Arc::clone(&metrics);
            async move {
                loop {
                    metrics.heartbeat(ALIVE);
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
            }
        });

        let recording = Arc::new(harness::RecordingChannel::new(false));
        let notifier = Notifier::new(
            Box::new(Arc::clone(&recording)),
            std::time::Duration::from_hours(1),
        );
        let pools = vec![STALLED.to_string(), ALIVE.to_string(), SILENT.to_string()];
        let (flag, shutdown) = watch::channel(false);
        let stop = async {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            flag.send(true).expect("raise shutdown");
        };
        let ((), ()) = tokio::join!(
            watchdog_loop(&metrics, &notifier, &pools, config, &shutdown),
            stop
        );
        alive.abort();

        assert!(notifier.drain(std::time::Duration::from_secs(5)).await);
        let sent = recording.sent();
        // Per pool, never by total count: a run of the alive pool's
        // stamping task that the runner starved would then read as this
        // test's own flake rather than as what it is.
        let stalls = |pool: &str| -> Vec<Notification> {
            sent.iter()
                .filter(|notification| {
                    notification.kind == NotificationKind::PollerStalled
                        && notification.pool == pool
                })
                .cloned()
                .collect()
        };
        let stalled = stalls(STALLED);
        assert_eq!(
            stalled.len(),
            1,
            "one stalled poller, and the cooldown suppresses every later pass: {sent:?}"
        );
        assert_eq!(stalled[0].severity, Severity::High);
        assert_eq!(stalled[0].account, None);
        assert!(
            stalled[0].message.contains("no poller heartbeat"),
            "the message says what is missing: {}",
            stalled[0].message
        );
        assert!(
            stalls(ALIVE).is_empty(),
            "a poller that keeps heartbeating is not stalled: {sent:?}"
        );
        assert!(
            stalls(SILENT).is_empty(),
            "a poller that has never heartbeated has not started, which is \
             not the same as having stopped: {sent:?}"
        );
    }

    /// A [`NotificationChannel`] that holds every send until the test hands
    /// it a permit: what proves a drain *waited* rather than merely
    /// returning. Simpler than the notifier tests' own gate because
    /// nothing here needs a send to be released and then re-held —
    /// `release` adds the permits once and every held send proceeds.
    struct GatedChannel {
        gate: tokio::sync::Semaphore,
        sent: std::sync::atomic::AtomicUsize,
    }

    impl GatedChannel {
        fn new() -> Self {
            Self {
                gate: tokio::sync::Semaphore::new(0),
                sent: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        /// Lets every held send — and every later one — through.
        fn release(&self) {
            self.gate.add_permits(crate::notifier::NOTIFY_IN_FLIGHT);
        }

        fn sent_count(&self) -> usize {
            self.sent.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl crate::notifier::NotificationChannel for Arc<GatedChannel> {
        fn name(&self) -> &'static str {
            "gated"
        }

        fn send<'a>(
            &'a self,
            _notification: &'a Notification,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), crate::notifier::NotifyError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                let permit = self.gate.acquire().await.expect("the gate is never closed");
                permit.forget();
                self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }
    }

    /// The URL of the database `#[sqlx::test]` built for this test:
    /// `DATABASE_URL`'s own host and credentials — the server the harness
    /// created it on — with the per-test database name the pool itself
    /// reports. What a test driving an entry point that takes a
    /// [`ServiceConfig`] needs, since those connect by URL rather than
    /// borrowing a pool.
    ///
    /// Read with `std::env::var`, the same source `#[sqlx::test]` itself
    /// reads: `dotenvy` is only a transitive dependency here (`sqlx-macros`
    /// and `sqlx-postgres` pull it), never declared in this crate's own
    /// `Cargo.toml`, so `.env` parsing is not available to this crate's own
    /// code and is out of scope for this helper.
    async fn test_database_url(store: &Store) -> String {
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(store.pool())
            .await
            .expect("the test database names itself");
        let base = std::env::var("DATABASE_URL").expect("sqlx::test needs DATABASE_URL too");
        // The query string (`?sslmode=...`, say) is not part of the
        // database path and must survive the rebuild, not just the split.
        let (path, query) = match base.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (base.as_str(), None),
        };
        let (server, _) = path
            .rsplit_once('/')
            .expect("DATABASE_URL carries a database path");
        match query {
            Some(query) => format!("{server}/{name}?{query}"),
            None => format!("{server}/{name}"),
        }
    }

    /// The configured credentials decide the channel, and nothing else
    /// does: both of `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` build the
    /// Telegram channel, and their absence leaves the run log-only. The
    /// token reaches the channel and no rendering of the notifier.
    #[tokio::test]
    async fn the_notifier_is_telegram_when_configured_and_log_otherwise() {
        let metrics = Arc::new(Metrics::new());
        let mut config = filler_service_config(vec![filler_pool_config(0)], true, 0);

        let notifier = build_notifier(&config, Arc::clone(&metrics)).expect("a log-only notifier");
        let rendered = format!("{notifier:?}");
        assert!(
            rendered.contains("channel: \"log\""),
            "no credentials is log-only: {rendered}"
        );

        config.telegram = Some(crate::config::TelegramConfig {
            token: Secret::new("123456:a-bot-token"),
            chat_id: "-1001".to_string(),
            base_url: None,
        });
        let notifier = build_notifier(&config, metrics).expect("a telegram notifier");
        let rendered = format!("{notifier:?}");
        assert!(
            rendered.contains("channel: \"telegram\""),
            "configured credentials pick the telegram channel: {rendered}"
        );
        assert!(
            !rendered.contains("a-bot-token"),
            "the token never renders: {rendered}"
        );
    }

    /// `check-config` proves the configured Telegram credentials work
    /// before the bot starts trusting them: a `getMe` that answers is an
    /// `info!` and a pass, and one that refuses is a configuration error —
    /// exit 2 through `main` — whose message never carries the token.
    #[sqlx::test(migrations = "./migrations")]
    async fn check_config_verifies_a_configured_telegram_token(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        const TOKEN: &str = "123456:a-bot-token";

        let store = Store::from_pool(db);
        let database_url = test_database_url(&store).await;
        let rpc = ScriptedRpc::start().await;
        // One script per `check_config` call: the scripted answers are
        // consumed in order, and this test makes two calls.
        for _ in 0..2 {
            script_pool(
                &rpc,
                POOL_A,
                BACKSTOP_A,
                PoolStatus::Active.code(),
                &[usable_reserve()],
                LEDGER,
            );
        }

        let telegram = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "liquidator_bot"}}),
            ))
            // What was scripted was consumed: `check_config` verifies the
            // token with exactly one `getMe`, and wiremock asserts it on
            // the server's drop.
            .expect(1)
            .mount(&telegram)
            .await;

        let mut config =
            filler_service_config(vec![pool_config(POOL_A, USDC, &[USDC], &["*"])], true, 0);
        config.chain.rpc_url = rpc.url();
        config.database_url = Secret::new(database_url);
        config.telegram = Some(crate::config::TelegramConfig {
            token: Secret::new(TOKEN),
            chat_id: "-1001".to_string(),
            base_url: Some(telegram.uri()),
        });

        let warnings = Service::check_config(
            &config,
            SigningKeys {
                auctioneer: None,
                filler: None,
            },
        )
        .await
        .expect("a reachable telegram passes the check");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("no FILLER_SECRET_KEY")),
            "the keyless run still reports its own warnings: {warnings:?}"
        );

        // The same configuration against a bot token Telegram refuses.
        let refusing = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_json(
                json!({"ok": false, "error_code": 401, "description": "Unauthorized"}),
            ))
            .expect(1)
            .mount(&refusing)
            .await;
        config.telegram = Some(crate::config::TelegramConfig {
            token: Secret::new(TOKEN),
            chat_id: "-1001".to_string(),
            base_url: Some(refusing.uri()),
        });

        let error = Service::check_config(
            &config,
            SigningKeys {
                auctioneer: None,
                filler: None,
            },
        )
        .await
        .expect_err("a refused token fails the check");
        assert!(
            matches!(error, LiquidatorError::Config(_)),
            "a credential the operator must fix is a configuration error: {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("telegram") && message.contains("401"),
            "the message says which channel refused and how: {message}"
        );
        assert!(
            !message.contains("a-bot-token"),
            "the token never reaches an error message: {message}"
        );
        Ok(())
    }

    /// Every exit of the run drains the notifier before it returns, and
    /// returns what it was given: `finish_run` waits for the sends still in
    /// flight — proved here by a channel that holds one until the test
    /// releases it — and passes both an `Ok` and an `Err` through
    /// untouched.
    #[tokio::test]
    async fn the_run_drains_the_notifier_on_shutdown() {
        let gated = Arc::new(GatedChannel::new());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&gated)),
            std::time::Duration::from_hours(1),
        ));
        assert_eq!(
            notifier.notify(Notification {
                kind: NotificationKind::PollerStalled,
                severity: Severity::High,
                pool: POOL_A.to_string(),
                account: None,
                message: "on its way out".to_string(),
            }),
            crate::notifier::Delivery::Queued
        );

        let finishing = tokio::spawn({
            let notifier = Arc::clone(&notifier);
            async move { finish_run(Drained::Returned(Ok(())), &notifier).await }
        });
        // The send is still held, so the drain cannot have finished: a
        // `finish_run` that returned here would be one that left a
        // notification the bot decided to send behind.
        tokio::task::yield_now().await;
        assert!(
            !finishing.is_finished(),
            "the drain waits for the send the channel is holding"
        );
        assert_eq!(gated.sent_count(), 0);

        gated.release();
        finishing
            .await
            .expect("joined")
            .expect("an Ok is returned unchanged");
        assert_eq!(gated.sent_count(), 1, "the held send left before the exit");

        // The error path drains the same way and reports the same error.
        let error = finish_run(
            Drained::Returned(Err(LiquidatorError::Config("a task failed".to_string()))),
            &notifier,
        )
        .await
        .expect_err("an Err is returned unchanged");
        assert!(matches!(error, LiquidatorError::Config(message) if message == "a task failed"));
    }

    /// A task's panic is resumed only once the notifier has drained: the
    /// send the gated channel holds must leave before the panic ends the
    /// run, and the panic that ends it is the task's own.
    #[tokio::test]
    async fn a_panic_is_resumed_only_after_the_notifier_drains() {
        let gated = Arc::new(GatedChannel::new());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&gated)),
            std::time::Duration::from_hours(1),
        ));
        assert_eq!(
            notifier.notify(Notification {
                kind: NotificationKind::SubmissionDropped,
                severity: Severity::High,
                pool: POOL_A.to_string(),
                account: None,
                message: "sent just before the panic".to_string(),
            }),
            crate::notifier::Delivery::Queued
        );

        let finishing = tokio::spawn({
            let notifier = Arc::clone(&notifier);
            async move { finish_run(Drained::Panicked(Box::new("a task panicked")), &notifier).await }
        });
        tokio::task::yield_now().await;
        assert!(
            !finishing.is_finished(),
            "the panic waits for the send the channel is holding"
        );
        assert_eq!(gated.sent_count(), 0);

        gated.release();
        let payload = finishing
            .await
            .expect_err("the run ends as a panic, not a return")
            .into_panic();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"a task panicked"));
        assert_eq!(
            gated.sent_count(),
            1,
            "the held send left before the panic did"
        );
    }
}
