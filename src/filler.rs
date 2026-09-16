//! The filler's evaluation loop: the I/O around everything
//! [`crate::math::fill`] decides.
//!
//! Once a tick, per configured pool, [`Filler::tick`] walks the store's
//! open auctions in six steps:
//!
//! 1. **Which rows are worth a chain read.** A row is kept only when it is
//!    a user liquidation, its account is none of the bot's own (spec §1),
//!    the pool [`PoolConfig::supports`] every asset it names, it is not an
//!    auction this process has already recorded a dry-run fill for
//!    (ruling 8), and it is *due* (spec §5; ruling 7) — the `due`
//!    function below is where that word is defined. A row that is not
//!    kept costs no chain read at all.
//! 2. **Re-read the chain's auction entry** for every row that is kept
//!    (spec §5: "Planning always re-reads the on-chain auction entry
//!    first"). A missing entry closes the row. What is planned against is
//!    the chain's entry, never the row: the row is the tracker's record of
//!    the auction's *events*, and a fill someone else made between two
//!    ticks reaches the entry first.
//! 3. **One snapshot per pool**, read only when something is live, of the
//!    filler's own account when there is one, valued at
//!    [`PoolSnapshot::valued_at`] — the clamp the auctioneer and the
//!    tracker apply, so the three cannot disagree about which instant a
//!    position is worth what. The wallet inventory is refreshed from that
//!    same read when its balances are stale or a confirmed fill has moved
//!    them.
//! 4. **Plan each live auction** with [`plan_fill`]. A skip clears the
//!    row's plan and says why at debug; a draft is written onto the row
//!    and logged at info. The filler writes `fill_ledger` and `percent`
//!    and **nothing else**: a row's `bid`, `lot` and `start_ledger` are
//!    the tracker's, from the pool's own events, and a filler that wrote
//!    them would be asserting an auction state no event ever reported.
//! 5. **Execute the ones whose ledger has come** — `fill_ledger` at or
//!    before the first ledger a transaction sent now could land in — when
//!    `execute` is true. It is false until the startup delay has passed
//!    (ruling 9): plans are still made and written, so an operator sees
//!    what the bot would do before it may do it. [`ExecOutcome::Replan`]
//!    is planned once more at half the percent and executed if it drafts
//!    (ruling 12); a second refusal is a skip. [`ExecOutcome::Stale`]
//!    clears the plan and forgets when it was made, so the next tick
//!    plans from fresh state rather than resending a plan the chain has
//!    moved past (spec §8). A submission that landed — or may have
//!    (ruling 14) — ends this pool's walk: every auction still to come in
//!    it was projected against the positions and the wallet as they stood
//!    *before* that fill, which is an optimistic view of both, and the
//!    next tick reads a snapshot that holds it.
//! 6. **One auction's failure is one auction's.** Every error but
//!    [`StoreError`] is logged with the pool and the account and the pass
//!    carries on; a store failure is fatal, as it is for the tracker and
//!    the auctioneer, because it means the bot cannot trust what it read
//!    about what is open. A set shutdown flag ends the tick between
//!    auctions — never inside a submission, which is waited for: a
//!    transaction whose outcome this bot never saw has still spent its
//!    key's sequence number.
//!
//! With no filler key configured there is no account to read positions or
//! balances for and nothing to simulate as, so a dry run plans against an
//! empty position and an empty wallet and the executor records what it
//! would have done, unsimulated (ruling 4).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::chain::pool::{PoolReader, PoolSnapshot};
use crate::chain::rpc::RpcClient;
use crate::chain::tx::Priority;
use crate::chain::xdr::{AuctionType, FillPercent, PoolStatus};
use crate::chain::TxOutcome;
use crate::config::PoolConfig;
use crate::executor::{ExecOutcome, Executor, ExecutorError, FillPlan, FillRecorded};
use crate::inventory::{read_balances, Inventory, Settlement};
use crate::ledger::LedgerTick;
use crate::math::fill::{
    health_floor, plan_fill, to_oracle_units, FillDraft, FillInputs, FillTerms, PlannedFill,
};
use crate::math::{AuctionData, MathError, Positions, Reserve};
use crate::queue::SubmissionQueue;
use crate::store::{Store, StoreError, TrackedAuction};

/// The whole of the auction, and the largest percent any plan may name.
/// The executor's one re-plan is the only thing that lowers it (ruling
/// 12), and it lowers it from whatever the contract refused.
const WHOLE_AUCTION: u32 = 100;

/// What the filler is configured with, beyond the pools themselves.
#[derive(Debug, Clone)]
pub struct FillerConfig {
    /// The bot's configured `DRY_RUN` mode: what every `fills` row it
    /// writes records, and what decides whether a plan takes a wallet
    /// reservation at all.
    pub dry_run: bool,
    /// Every account this bot holds a key for. The filler never fills its
    /// own liquidation (spec §1): the contract would refuse it, and in
    /// dry-run there is no contract to refuse.
    pub own_addresses: BTreeSet<String>,
    /// The pool's `min_health_factor` is multiplied by this for the floor
    /// a fill keeps the filler's own position at or above, 7 decimals.
    pub hf_safety_multiplier: i128,
    /// How many rounds of supply → percent → delay one plan may take.
    pub plan_iterations: u32,
    /// How often, in ledgers, an auction this process has already planned
    /// is planned again.
    pub replan_ledgers: u32,
    /// Within this many ledgers of its planned fill ledger an auction is
    /// planned again on every ledger.
    pub replan_near_ledgers: u32,
    /// The estimated profit, 7 decimals, at or above which a fill pays the
    /// high fee tier.
    pub high_fee_profit_threshold: i128,
    /// The longest the wallet balances go unread.
    pub inventory_refresh: Duration,
    /// The network's native asset contract: read alongside every reserve,
    /// because it is what pays the fees the fee reserve is held back for.
    pub native_asset: String,
}

/// One version of an auction a dry run has recorded a fill for: the pool,
/// the account, the start ledger, and the amounts the chain held.
///
/// The amounts are the version. A partial fill by someone else keeps the
/// start ledger and leaves a remainder — a different fill, and the one an
/// armed filler would now make — so the remainder is recorded afresh; the
/// same remainder is one version whether the filler saw it from the chain
/// before the tracker applied that fill or from the store after it, so it
/// is recorded once. Keying on a ledger instead — the row's
/// `updated_ledger`, or the ledger an entry was read at — would make those
/// two sightings two versions and record the same fill twice.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RecordedFill {
    pool: String,
    account: String,
    start_ledger: u32,
    bid: BTreeMap<String, i128>,
    lot: BTreeMap<String, i128>,
}

impl RecordedFill {
    /// The version the store's row describes — what the tracker last
    /// wrote, which may lag the chain by the events it has not applied.
    fn of_row(row: &TrackedAuction) -> Self {
        Self {
            pool: row.pool.clone(),
            account: row.account.clone(),
            start_ledger: row.start_ledger,
            bid: row.bid.clone(),
            lot: row.lot.clone(),
        }
    }

    /// The version the chain holds now, which is what a plan is made
    /// against and what a recorded fill is keyed by.
    fn of_entry(row: &TrackedAuction, auction: &AuctionData) -> Self {
        Self {
            pool: row.pool.clone(),
            account: row.account.clone(),
            start_ledger: auction.block,
            bid: auction.bid.clone(),
            lot: auction.lot.clone(),
        }
    }
}

/// What one run of the filler remembers between ticks.
///
/// All of it is in memory on purpose. Losing them on a restart costs one
/// extra plan per auction, one extra wallet read, and — for
/// `recorded_dry_run` — one extra dry-run `fills` row per open auction:
/// cheap, and none of it is a chain effect. The store holds everything
/// that must survive.
#[derive(Debug, Default)]
pub struct FillerState {
    /// Pool and account to the ledger this process last planned it at.
    last_planned: BTreeMap<(String, String), u32>,
    /// Every version of an auction this process has recorded a dry-run
    /// fill for (ruling 8), by content: see [`RecordedFill`].
    recorded_dry_run: BTreeSet<RecordedFill>,
    /// Set by a submission that landed or may have landed, so the next
    /// pool pass re-reads the wallet however fresh its balances look.
    inventory_stale: bool,
    /// Every asset any pool this run has read a snapshot of names, plus
    /// the native asset. A wallet read covers **all** of them, never just
    /// the pool that triggered it: [`Inventory::record_balances`] replaces
    /// the whole map, so a read of one pool's reserves would leave every
    /// other pool's assets showing zero — and a zero wallet plans no
    /// repay, caps a supply at nothing, and skips fills the funds were
    /// there for.
    known_assets: BTreeSet<String>,
    /// What the last successful wallet read actually covered. A pool
    /// naming an asset outside it is read for, however fresh the balances
    /// look.
    covered_assets: BTreeSet<String>,
}

impl FillerState {
    /// Forgets when this auction was planned, so the next tick plans it
    /// from fresh state rather than waiting out the replan period.
    fn forget(&mut self, pool: &str, account: &str) {
        self.last_planned
            .remove(&(pool.to_string(), account.to_string()));
    }

    /// Forgets an auction that is no longer there: when it was planned,
    /// and that a dry run already recorded a fill for it — every version
    /// of it, since the account has no row left to version.
    fn closed(&mut self, pool: &str, account: &str) {
        self.forget(pool, account);
        self.recorded_dry_run
            .retain(|recorded| recorded.pool != pool || recorded.account != account);
    }

    /// Drops every `recorded_dry_run` key of `pool` that `rows` — the
    /// pool's open auctions, read at the start of this pool's walk — no
    /// longer names.
    ///
    /// Ruling 8's set suppresses an auction by its content, and that
    /// suppression is what keeps the filler from ever re-reading its entry:
    /// nothing else would notice the row going away. So the walk that
    /// already holds the open rows is where every version of an auction
    /// that has been filled, or replaced by a new one at a later start
    /// ledger, is dropped — otherwise the set only ever grows, for as long
    /// as the process runs.
    ///
    /// A version is kept while its start ledger is *at or past* the row's,
    /// not only when it equals it. The chain can hold a new auction for an
    /// account — a later start ledger — before the tracker has applied the
    /// events that close the old one and open it, and the filler, which
    /// re-reads the chain's entry, records that new auction against the old
    /// row. Pruning it for not matching the row would record it again on
    /// every tick until the tracker caught up. Older versions go once the
    /// row has advanced past them; every version goes when the row does.
    fn prune_recorded(&mut self, pool: &str, rows: &[TrackedAuction]) {
        let open: BTreeMap<&str, u32> = rows
            .iter()
            .filter(|row| row.auction_type == AuctionType::UserLiquidation)
            .map(|row| (row.account.as_str(), row.start_ledger))
            .collect();
        self.recorded_dry_run.retain(|recorded| {
            recorded.pool != pool
                || open
                    .get(recorded.account.as_str())
                    .is_some_and(|start| recorded.start_ledger >= *start)
        });
    }
}

/// What one tick did, across every pool.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickSummary {
    /// Drafts written onto a row. The one re-plan after a refused health
    /// check writes a second draft for the same auction, and counts again.
    pub planned: u32,
    /// Executions the executor answered [`ExecOutcome::Recorded`] to,
    /// dry-run or not.
    pub executed: u32,
    /// Auctions nothing was done about *by decision*: a planner skip, a
    /// refusal, a stale plan, a re-plan that could not be drafted, or a
    /// wallet that could not fund the spend. A chain read that simply
    /// failed is not counted here — it is not a decision, and the next
    /// tick reads it again.
    pub skipped: u32,
    /// Rows closed because the chain no longer holds their auction.
    pub closed: u32,
}

/// The one failure that ends a tick.
#[derive(Debug, thiserror::Error)]
pub enum FillerError {
    /// Reading or writing the store failed. Fatal to the tick, as it is
    /// for the tracker and the auctioneer: it means the bot cannot trust
    /// what it read about which auctions are open.
    #[error("store: {0}")]
    Store(#[from] StoreError),
}

/// Whether `row` is planned this tick (spec §5): it has no plan yet, this
/// process has not planned it, it is within `replan_near_ledgers` of its
/// fill ledger, or `replan_ledgers` have passed since it was planned. The
/// subtractions saturate on purpose — a fill ledger already passed is
/// "within reach", and a planned ledger ahead of this tick (a watch that
/// coalesced) is "just planned" — both being the safe reading.
fn due(
    row: &TrackedAuction,
    planned_at: Option<u32>,
    tick: LedgerTick,
    config: &FillerConfig,
) -> bool {
    let (Some(fill_ledger), Some(planned_at)) = (row.fill_ledger, planned_at) else {
        return true;
    };
    fill_ledger.saturating_sub(tick.sequence) <= config.replan_near_ledgers
        || tick.sequence.saturating_sub(planned_at) >= config.replan_ledgers
}

/// What one tick carries across the pools it walks.
struct Pass<'p> {
    /// The ledger this tick is for.
    tick: LedgerTick,
    /// What the run remembers between ticks.
    state: &'p mut FillerState,
    /// What to report at the end.
    summary: TickSummary,
}

/// One pool's tick, after its snapshot: everything every auction in it is
/// planned against, read once.
struct PoolPass<'p> {
    /// The pool's own configuration.
    pool: &'p PoolConfig,
    /// The first ledger a transaction sent now could land in.
    earliest_ledger: u32,
    /// The one snapshot this pool's auctions share.
    snapshot: PoolSnapshot,
    /// Its reserves, accrued to the instant the snapshot is valued at.
    reserves: BTreeMap<u32, Reserve>,
    /// The filler's own positions in this pool; empty without a key.
    filler: Positions,
    /// Whether a plan may supply the primary asset as collateral.
    supply_allowed: bool,
    /// The health factor a fill keeps the filler at or above.
    health_floor: i128,
}

/// Plans and executes the fills of every configured pool's open auctions,
/// against one store, one chain client, one wallet and one executor.
#[derive(Debug)]
pub struct Filler<'a> {
    rpc: &'a RpcClient,
    store: &'a Store,
    pools: &'a [PoolConfig],
    config: FillerConfig,
    executor: Executor<'a>,
    inventory: Inventory,
}

impl<'a> Filler<'a> {
    /// A filler reading `pools` through `rpc`, planning against `store`
    /// and `inventory`, and executing through `executor`.
    #[must_use]
    pub fn new(
        rpc: &'a RpcClient,
        store: &'a Store,
        pools: &'a [PoolConfig],
        config: FillerConfig,
        executor: Executor<'a>,
        inventory: Inventory,
    ) -> Self {
        Self {
            rpc,
            store,
            pools,
            config,
            executor,
            inventory,
        }
    }

    /// One tick, per configured pool, in the six steps the module doc sets
    /// out: keep the rows worth reading, re-read each one's auction entry,
    /// read one snapshot for the pool, plan every live entry against it,
    /// execute the ones whose ledger has come when `execute` says it may,
    /// and carry on past anything but a store failure.
    ///
    /// `execute` is the startup delay's gate (ruling 9) and `queue` the
    /// arming: a dry run is given no queue, and neither is an armed pass
    /// with no signer — the executor refuses both combinations outright.
    ///
    /// # Errors
    ///
    /// [`FillerError::Store`] only. Everything else — a chain read, a
    /// simulation, a refused reservation, a queue that could not carry a
    /// submission — is one auction's, logged with the pool and the
    /// account, and left for the next tick.
    pub async fn tick(
        &self,
        state: &mut FillerState,
        tick: LedgerTick,
        execute: bool,
        queue: Option<&SubmissionQueue>,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<TickSummary, FillerError> {
        let mut pass = Pass {
            tick,
            state,
            summary: TickSummary::default(),
        };
        for pool in self.pools {
            if *shutdown.borrow() {
                break;
            }
            self.tick_pool(pool, &mut pass, execute, queue, shutdown)
                .await?;
        }
        Ok(pass.summary)
    }

    /// One pool's steps 1 to 5.
    async fn tick_pool(
        &self,
        pool: &PoolConfig,
        pass: &mut Pass<'_>,
        execute: bool,
        queue: Option<&SubmissionQueue>,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<(), FillerError> {
        let rows = self.store.open_auctions(&pool.address).await?;
        pass.state.prune_recorded(&pool.address, &rows);
        let candidates: Vec<TrackedAuction> = rows
            .into_iter()
            .filter(|row| self.considered(pool, row, pass))
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let reader = PoolReader::new(self.rpc, &pool.address);
        let live = self
            .read_entries(&reader, candidates, pass, shutdown)
            .await?;
        // A shutdown between the entry reads ends the tick here rather
        // than one step later: there is nothing to plan against a
        // snapshot nobody will use.
        if live.is_empty() || *shutdown.borrow() {
            return Ok(());
        }
        let accounts: Vec<&str> = self.executor.filler().into_iter().collect();
        let snapshot = match reader.snapshot(&accounts).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    pool = %pool.address,
                    %error,
                    "could not read this pool; its auctions are planned on the next tick"
                );
                return Ok(());
            }
        };
        self.refresh_inventory(&reader, &snapshot, pass.state).await;
        let Some(context) = self.pool_context(pool, snapshot, pass.tick) else {
            return Ok(());
        };
        for (row, auction) in live {
            // Checked between auctions, never inside a submission: a
            // submission already in flight is waited for, because
            // abandoning it would leave a signing key's sequence number
            // consumed by something this bot never saw the outcome of.
            if *shutdown.borrow() {
                return Ok(());
            }
            // The definitive dry-run test, on the version the chain holds:
            // the row's own may lag it by a fill the tracker has not yet
            // applied, and this is the version any record would be keyed by.
            if pass
                .state
                .recorded_dry_run
                .contains(&RecordedFill::of_entry(&row, &auction))
            {
                tracing::debug!(
                    pool = %pool.address,
                    account = %row.account,
                    "a dry-run fill of this version of the auction is already recorded"
                );
                continue;
            }
            if self
                .plan_and_act(&context, &row, &auction, execute, queue, pass)
                .await?
            {
                tracing::debug!(
                    pool = %pool.address,
                    account = %row.account,
                    "a fill of this pool landed, or may have; its remaining auctions wait for \
                     the next tick rather than being planned against positions and a wallet \
                     this fill has moved"
                );
                return Ok(());
            }
        }
        Ok(())
    }

    /// Step 1: whether this row is worth a chain read at all.
    ///
    /// Silent by design — it runs for every open auction of every pool on
    /// every tick, and a line per row per ledger would bury the decisions
    /// that matter. What it filtered out is the difference between the
    /// store's open auctions and the tick's summary.
    fn considered(&self, pool: &PoolConfig, row: &TrackedAuction, pass: &Pass<'_>) -> bool {
        if row.auction_type != AuctionType::UserLiquidation {
            return false;
        }
        if self.config.own_addresses.contains(&row.account) {
            return false;
        }
        let bid: Vec<&str> = row.bid.keys().map(String::as_str).collect();
        let lot: Vec<&str> = row.lot.keys().map(String::as_str).collect();
        if !pool.supports(&bid, &lot) {
            return false;
        }
        // On the row's own version: the cheap test, before any chain read.
        // A row the tracker has not yet rewritten can pass it once; the
        // definitive test is against the chain's entry, in `tick_pool`.
        if pass
            .state
            .recorded_dry_run
            .contains(&RecordedFill::of_row(row))
        {
            return false;
        }
        let key = (row.pool.clone(), row.account.clone());
        due(
            row,
            pass.state.last_planned.get(&key).copied(),
            pass.tick,
            &self.config,
        )
    }

    /// Step 2: the chain's own entry for each kept row. A row the chain no
    /// longer holds an auction for is closed, and never reaches a plan.
    async fn read_entries(
        &self,
        reader: &PoolReader<'_>,
        candidates: Vec<TrackedAuction>,
        pass: &mut Pass<'_>,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<Vec<(TrackedAuction, AuctionData)>, FillerError> {
        let mut live = Vec::with_capacity(candidates.len());
        for row in candidates {
            if *shutdown.borrow() {
                break;
            }
            match reader
                .auction(&row.account, AuctionType::UserLiquidation)
                .await
            {
                Ok(Some((_ledger, auction))) => live.push((row, auction)),
                Ok(None) => {
                    self.store
                        .delete_auction(&row.pool, &row.account, row.auction_type)
                        .await?;
                    pass.state.closed(&row.pool, &row.account);
                    pass.summary.closed += 1;
                    tracing::info!(
                        pool = %row.pool,
                        account = %row.account,
                        "the chain no longer holds this auction; closing its row"
                    );
                }
                Err(error) => tracing::warn!(
                    pool = %row.pool,
                    account = %row.account,
                    %error,
                    "could not re-read this auction's entry; leaving it for the next tick"
                ),
            }
        }
        Ok(live)
    }

    /// Step 3's second half: the filler's wallet, when it has one and the
    /// balances are stale or a confirmed fill has moved them. A read that
    /// fails keeps the previous balances — they are the best claim there
    /// is until the next read — and says so.
    async fn refresh_inventory(
        &self,
        reader: &PoolReader<'_>,
        snapshot: &PoolSnapshot,
        state: &mut FillerState,
    ) {
        let Some(filler) = self.executor.filler() else {
            return;
        };
        state
            .known_assets
            .extend(snapshot.asset_index.keys().cloned());
        state.known_assets.insert(self.config.native_asset.clone());
        // This pool naming an asset the last read did not cover is a
        // refresh on its own: the balances may be seconds old and still
        // show nothing at all for what this pool's auctions are paid in.
        let uncovered = snapshot
            .asset_index
            .keys()
            .any(|asset| !state.covered_assets.contains(asset));
        if !state.inventory_stale
            && !uncovered
            && !self
                .inventory
                .stale(Instant::now(), self.config.inventory_refresh)
        {
            return;
        }
        let assets = state.known_assets.clone();
        match read_balances(reader, filler, &assets).await {
            Ok(balances) => {
                self.inventory.record_balances(balances, Instant::now());
                state.inventory_stale = false;
                tracing::debug!(
                    pool = %snapshot.pool,
                    assets = assets.len(),
                    "the filler's wallet balances were re-read"
                );
                state.covered_assets = assets;
            }
            Err(error) => tracing::warn!(
                pool = %snapshot.pool,
                %error,
                "could not read the filler's wallet; planning against the balances from the \
                 last read"
            ),
        }
    }

    /// Step 3's first half: everything this pool's auctions are planned
    /// against, or `None` when the pool cannot be planned at all this
    /// tick.
    fn pool_context<'p>(
        &self,
        pool: &'p PoolConfig,
        snapshot: PoolSnapshot,
        tick: LedgerTick,
    ) -> Option<PoolPass<'p>> {
        let Some(index) = snapshot.asset_index.get(&pool.primary_asset).copied() else {
            tracing::warn!(
                pool = %pool.address,
                primary_asset = %pool.primary_asset,
                "this pool no longer lists its primary asset as a reserve; skipping it this \
                 tick rather than asking the planner about every auction in it"
            );
            return None;
        };
        let Some(earliest_ledger) = tick.sequence.checked_add(1) else {
            tracing::warn!(pool = %pool.address, sequence = tick.sequence, "the ledger sequence overflows");
            return None;
        };
        let health_floor = match health_floor(
            pool.min_health_factor,
            self.config.hf_safety_multiplier,
        ) {
            Ok(floor) => floor,
            Err(error) => {
                tracing::warn!(pool = %pool.address, %error, "this pool's health floor does not compute");
                return None;
            }
        };
        let reserves = match snapshot.accrued_reserves(snapshot.valued_at(tick.close_time)) {
            Ok(reserves) => reserves,
            Err(error) => {
                tracing::warn!(
                    pool = %pool.address,
                    %error,
                    "could not accrue this pool's reserves; skipping it this tick"
                );
                return None;
            }
        };
        // A pool that is not lending, or whose primary reserve is
        // disabled, refuses a `SupplyCollateral` of it — so a plan that
        // needs one has to find the health elsewhere.
        let supply_allowed = snapshot.instance.config.status.code() <= PoolStatus::OnIce.code()
            && snapshot
                .reserves
                .get(&index)
                .is_some_and(|reserve| reserve.config.enabled);
        let filler = self
            .executor
            .filler()
            .and_then(|address| snapshot.positions.get(address))
            .cloned()
            .unwrap_or_default();
        Some(PoolPass {
            pool,
            earliest_ledger,
            snapshot,
            reserves,
            filler,
            supply_allowed,
            health_floor,
        })
    }

    /// What this pool holds one fill of `auction` to.
    fn terms(&self, context: &PoolPass<'_>, auction: &AuctionData) -> FillTerms {
        let bid: Vec<&str> = auction.bid.keys().map(String::as_str).collect();
        let lot: Vec<&str> = auction.lot.keys().map(String::as_str).collect();
        FillTerms {
            min_collateral: context.snapshot.instance.config.min_collateral,
            max_positions: context.snapshot.instance.config.max_positions,
            supply_allowed: context.supply_allowed,
            primary_asset: context.pool.primary_asset.clone(),
            health_floor: context.health_floor,
            profit_bps: context.pool.profit_bps(&bid, &lot),
            force_fill: context.pool.force_fill,
            plan_iterations: self.config.plan_iterations,
        }
    }

    /// One plan against this pool's snapshot and the wallet as it stands
    /// now — which a reservation taken earlier in this same tick has
    /// already reduced.
    fn plan(
        &self,
        context: &PoolPass<'_>,
        auction: &AuctionData,
        max_percent: FillPercent,
    ) -> Result<PlannedFill, MathError> {
        let wallet = self.inventory.available();
        let inputs = FillInputs {
            reserves: &context.reserves,
            asset_index: &context.snapshot.asset_index,
            prices: &context.snapshot.prices,
            filler: &context.filler,
            wallet: &wallet,
            auction,
            earliest_ledger: context.earliest_ledger,
            max_percent,
        };
        plan_fill(&self.terms(context, auction), &inputs)
    }

    /// Steps 4 and 5 for one auction. `true` when a submission landed or
    /// may have, which ends this pool's walk: everything left in it was
    /// projected against the positions and the wallet as they stood
    /// *before* this fill, an optimistic view of both, and the next tick
    /// reads a snapshot that holds it.
    async fn plan_and_act(
        &self,
        context: &PoolPass<'_>,
        row: &TrackedAuction,
        auction: &AuctionData,
        execute: bool,
        queue: Option<&SubmissionQueue>,
        pass: &mut Pass<'_>,
    ) -> Result<bool, FillerError> {
        let whole = match FillPercent::try_from(WHOLE_AUCTION) {
            Ok(percent) => percent,
            Err(error) => {
                tracing::warn!(
                    pool = %row.pool,
                    account = %row.account,
                    %error,
                    "the whole-auction percent does not construct; skipping this auction"
                );
                return Ok(false);
            }
        };
        let Some(draft) = self.drafted(context, row, auction, whole, pass).await? else {
            return Ok(false);
        };
        if !self.write_plan(row, &draft, pass).await? {
            return Ok(false);
        }
        if !execute || draft.fill_ledger > context.earliest_ledger {
            return Ok(false);
        }
        self.execute_draft(context, row, auction, &draft, queue, pass)
            .await
    }

    /// One planning attempt: the draft, or `None` with the row's plan
    /// cleared and the reason said once.
    async fn drafted(
        &self,
        context: &PoolPass<'_>,
        row: &TrackedAuction,
        auction: &AuctionData,
        max_percent: FillPercent,
        pass: &mut Pass<'_>,
    ) -> Result<Option<FillDraft>, FillerError> {
        match self.plan(context, auction, max_percent) {
            Ok(PlannedFill::Fill(draft)) => Ok(Some(draft)),
            Ok(PlannedFill::Skip(reason)) => {
                tracing::debug!(
                    pool = %row.pool,
                    account = %row.account,
                    reason = ?reason,
                    max_percent = max_percent.get(),
                    "no fill planned for this auction"
                );
                self.clear_plan(row).await?;
                pass.summary.skipped += 1;
                Ok(None)
            }
            Err(error) => {
                tracing::warn!(
                    pool = %row.pool,
                    account = %row.account,
                    %error,
                    "this auction could not be planned; leaving it for the next tick"
                );
                Ok(None)
            }
        }
    }

    /// Writes a draft onto its row: the filler's plan, and nothing else on
    /// it. `false` when the row has gone — the auction closed while it was
    /// being planned, which is not an error and is not executed either.
    async fn write_plan(
        &self,
        row: &TrackedAuction,
        draft: &FillDraft,
        pass: &mut Pass<'_>,
    ) -> Result<bool, FillerError> {
        let written = self
            .store
            .set_fill_plan(
                &row.pool,
                &row.account,
                row.auction_type,
                Some((draft.fill_ledger, draft.percent)),
            )
            .await?;
        if !written {
            tracing::debug!(
                pool = %row.pool,
                account = %row.account,
                "this auction closed while it was being planned"
            );
            return Ok(false);
        }
        pass.state
            .last_planned
            .insert((row.pool.clone(), row.account.clone()), pass.tick.sequence);
        pass.summary.planned += 1;
        tracing::info!(
            pool = %row.pool,
            account = %row.account,
            fill_ledger = draft.fill_ledger,
            percent = draft.percent.get(),
            lot_value = draft.lot_value,
            bid_value = draft.bid_value,
            est_profit = draft.est_profit,
            projected_health = draft.projected_health,
            "fill planned"
        );
        Ok(true)
    }

    /// Clears the plan off a row. A row that has gone is not an error:
    /// there is nothing left to clear.
    async fn clear_plan(&self, row: &TrackedAuction) -> Result<(), FillerError> {
        self.store
            .set_fill_plan(&row.pool, &row.account, row.auction_type, None)
            .await?;
        Ok(())
    }

    /// Step 5: one execution and what its answer means. `true` when the
    /// submission landed or may have, which is what ends this pool's walk.
    async fn execute_draft(
        &self,
        context: &PoolPass<'_>,
        row: &TrackedAuction,
        auction: &AuctionData,
        draft: &FillDraft,
        queue: Option<&SubmissionQueue>,
        pass: &mut Pass<'_>,
    ) -> Result<bool, FillerError> {
        let Some(outcome) = self.execute_once(context, row, draft, queue, pass).await? else {
            return Ok(false);
        };
        match outcome {
            ExecOutcome::Recorded(recorded) => Ok(note_recorded(row, auction, &recorded, pass)),
            ExecOutcome::Replan { contract_error } => {
                tracing::info!(
                    pool = %row.pool,
                    account = %row.account,
                    contract_error,
                    percent = draft.percent.get(),
                    "the contract refused this fill's health; re-planning it once, lower"
                );
                self.replan(context, row, auction, draft, queue, pass).await
            }
            ExecOutcome::Refused { contract_error } => {
                tracing::debug!(
                    pool = %row.pool,
                    account = %row.account,
                    contract_error,
                    "this fill was refused; leaving it for the next tick"
                );
                pass.summary.skipped += 1;
                Ok(false)
            }
            ExecOutcome::Stale => {
                self.clear_plan(row).await?;
                pass.state.forget(&row.pool, &row.account);
                pass.summary.skipped += 1;
                Ok(false)
            }
        }
    }

    /// Ruling 12's one re-plan, at half the refused percent and never
    /// below 1. A second refusal is a skip: the contract has now disagreed
    /// twice, and a third guess costs another simulation for the same
    /// answer. `true` means the same thing it does for the first draft: a
    /// submission landed, or may have, and this pool's walk ends here.
    async fn replan(
        &self,
        context: &PoolPass<'_>,
        row: &TrackedAuction,
        auction: &AuctionData,
        refused: &FillDraft,
        queue: Option<&SubmissionQueue>,
        pass: &mut Pass<'_>,
    ) -> Result<bool, FillerError> {
        let half = match FillPercent::try_from((refused.percent.get() / 2).max(1)) {
            Ok(percent) => percent,
            Err(error) => {
                tracing::warn!(pool = %row.pool, account = %row.account, %error, "half a percent is not one");
                pass.summary.skipped += 1;
                return Ok(false);
            }
        };
        let Some(draft) = self.drafted(context, row, auction, half, pass).await? else {
            return Ok(false);
        };
        if !self.write_plan(row, &draft, pass).await? {
            return Ok(false);
        }
        // The same gate the first draft passed, and for the same reason:
        // `plan_fill` answers a *later* ledger when the lower percent no
        // longer holds the filler's floor where the refused one did, and
        // a draft sent before its own ledger meets neither the health
        // margin nor the profit margin it was chosen for.
        if draft.fill_ledger > context.earliest_ledger {
            tracing::debug!(
                pool = %row.pool,
                account = %row.account,
                fill_ledger = draft.fill_ledger,
                percent = draft.percent.get(),
                "the re-plan is for a later ledger; it is on the row and waits for it"
            );
            return Ok(false);
        }
        let Some(outcome) = self.execute_once(context, row, &draft, queue, pass).await? else {
            return Ok(false);
        };
        Ok(match outcome {
            ExecOutcome::Recorded(recorded) => note_recorded(row, auction, &recorded, pass),
            ExecOutcome::Stale => {
                self.clear_plan(row).await?;
                pass.state.forget(&row.pool, &row.account);
                pass.summary.skipped += 1;
                false
            }
            other => {
                tracing::info!(
                    pool = %row.pool,
                    account = %row.account,
                    outcome = ?other,
                    "the contract refused the re-plan too; leaving this auction for the next tick"
                );
                pass.summary.skipped += 1;
                false
            }
        })
    }

    /// Hands one draft to the executor with the settlement its mode
    /// demands. `None` means nothing was executed and the reason is
    /// already counted or logged.
    async fn execute_once(
        &self,
        context: &PoolPass<'_>,
        row: &TrackedAuction,
        draft: &FillDraft,
        queue: Option<&SubmissionQueue>,
        pass: &mut Pass<'_>,
    ) -> Result<Option<ExecOutcome>, FillerError> {
        let priority = match self.priority(context, draft) {
            Ok(priority) => priority,
            Err(error) => {
                tracing::warn!(pool = %row.pool, account = %row.account, %error, "this fill's fee tier does not compute");
                return Ok(None);
            }
        };
        let settlement = if self.config.dry_run {
            Settlement::DryRun
        } else {
            match self.inventory.reserve(&draft.spend) {
                Ok(reservation) => Settlement::Live(reservation),
                Err(error) => {
                    tracing::warn!(
                        pool = %row.pool,
                        account = %row.account,
                        %error,
                        "the wallet cannot fund this fill; skipping it this tick"
                    );
                    pass.summary.skipped += 1;
                    return Ok(None);
                }
            }
        };
        let plan = FillPlan {
            pool: row.pool.clone(),
            user: row.account.clone(),
            draft: draft.clone(),
            priority,
        };
        match self.executor.execute(&plan, settlement, queue).await {
            Ok(outcome) => Ok(Some(outcome)),
            Err(ExecutorError::Store(error)) => Err(FillerError::Store(error)),
            Err(error) => {
                tracing::warn!(
                    pool = %row.pool,
                    account = %row.account,
                    %error,
                    "this fill could not be executed; leaving it for the next tick"
                );
                Ok(None)
            }
        }
    }

    /// The fee tier: high for a fill worth paying to land first.
    fn priority(&self, context: &PoolPass<'_>, draft: &FillDraft) -> Result<Priority, MathError> {
        let threshold = to_oracle_units(
            self.config.high_fee_profit_threshold,
            context.snapshot.prices.scalar(),
        )?;
        Ok(if draft.est_profit >= threshold {
            Priority::High
        } else {
            Priority::Normal
        })
    }
}

/// What a recorded fill leaves behind: ruling 8's "recorded once" for a
/// dry run, and a wallet to re-read when the chain may have spent it
/// (ruling 14 — an `Unknown` may still land).
///
/// Answers whether the chain applied this fill or may yet, which is both
/// why the wallet is re-read and why the rest of this pool's auctions are
/// left for the next tick: they were projected against the borrower's
/// positions and the filler's own as this fill has just changed them.
fn note_recorded(
    row: &TrackedAuction,
    auction: &AuctionData,
    recorded: &FillRecorded,
    pass: &mut Pass<'_>,
) -> bool {
    pass.summary.executed += 1;
    if recorded.dry_run {
        pass.state
            .recorded_dry_run
            .insert(RecordedFill::of_entry(row, auction));
    }
    let landed = matches!(
        recorded.submission,
        Some(TxOutcome::Succeeded { .. } | TxOutcome::Unknown { .. })
    );
    if landed {
        pass.state.inventory_stale = true;
    }
    landed
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::sync::watch;

    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{
        script_simulate_accepted, script_simulate_prelude, script_simulate_refused, scval_b64,
        transaction_data_b64, ScriptedRpc,
    };
    use crate::chain::signer::{Network, Signer};
    use crate::chain::tx::{Submitter, TxConfig};
    use crate::chain::xdr::encode::{
        address, from_base64, i128_val, map, sc_address, symbol, to_base64, vec as sc_vec,
    };
    use crate::chain::xdr::keys;
    use crate::chain::{ChainError, TxHash, TxOutcome};
    use crate::fixture::{mainnet_fixed_v2, text};
    use crate::harness;
    use crate::math::fill::FillAction;
    use crate::queue::QueueError;
    use stellar_xdr::{
        ContractDataDurability, ContractDataEntry, ExtensionPoint, LedgerEntryData, ScVal,
    };

    /// The fixture's XLM and USDC reserves. Real strkeys: every address
    /// here round-trips through the encoder on its way into a scripted
    /// entry.
    const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    /// The auction every test here plans: 100,000 XLM of b-tokens against
    /// 20,000,000,000 USDC d-tokens. At the fixture's own rates and oracle
    /// prices that is a lot worth $17,786.57 against a bid worth $2,457.28
    /// — the lot seven times over, which is what makes every skip in these
    /// tests a decision about the filler rather than about the auction.
    const LOT: i128 = 1_000_000_000_000;
    const BID: i128 = 20_000_000_000;

    /// Ledgers after its start at which this auction is planned when the
    /// whole ramp is still ahead of it.
    ///
    /// Not the margin ledger — that is 31, where `lot × d / 200` first
    /// reaches `bid × 1.1` — because the filler's own health floor, not
    /// the margin, is what binds here. At the margin ledger the fill hands
    /// the filler a position whose health is `1.1 × c_factor(XLM) ×
    /// l_factor(USDC)` = `1.1 × 0.75 × 0.95` = 0.78, far under the 1.65
    /// floor (`min_health_factor` 1.5 × `hf_safety_multiplier` 1.1). The
    /// lot ramp reaches the floor at the first `d` with `(d / 200) × 17,786.57
    /// × 0.75 ≥ (2,457.28 / 0.95) × 1.65`, i.e. `d ≥ 63.99`: 64, where the
    /// projection is 1.6503, against 1.6246 at 63.
    const HEALTHY_DELTA: u32 = 64;

    /// A pool that takes every asset, keeps XLM as its primary collateral,
    /// and wants 10% profit.
    fn pool_config() -> PoolConfig {
        PoolConfig {
            address: harness::POOL.to_string(),
            primary_asset: XLM.to_string(),
            min_primary_collateral: 0,
            min_health_factor: 15_000_000,
            default_profit_bps: 1_000,
            force_fill: false,
            supported_bid: vec!["*".to_string()],
            supported_lot: vec!["*".to_string()],
            profits: Vec::new(),
        }
    }

    /// A dry-run filler: the replan cadence the brief names, a profit
    /// threshold no test's fill reaches — so every fill here is
    /// `Priority::Normal` unless a test says otherwise — and the fixture's
    /// own native asset.
    fn filler_config() -> FillerConfig {
        FillerConfig {
            dry_run: true,
            own_addresses: BTreeSet::new(),
            hf_safety_multiplier: 11_000_000,
            plan_iterations: 5,
            replan_ledgers: 10,
            replan_near_ledgers: 5,
            high_fee_profit_threshold: 1_000_000_000_000_000,
            inventory_refresh: Duration::from_secs(30),
            native_asset: XLM.to_string(),
        }
    }

    /// The filler's key, for the two tests that need an account to
    /// simulate and sign as. Copied from `executor.rs`'s test module: a
    /// test signer is scaffolding, not an interface.
    fn filler_signer() -> Signer {
        let key = ed25519_dalek::SigningKey::from_bytes(&[11_u8; 32]);
        let secret = stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string();
        Signer::from_secret(&secret).expect("signer")
    }

    /// Fee and polling policy short enough that nothing here waits on a
    /// real interval. Copied from `executor.rs`'s test module.
    fn tx_config() -> TxConfig {
        TxConfig {
            poll_interval: Duration::from_millis(1),
            send_retry_pause: Duration::from_millis(1),
            wait_cap: Duration::from_millis(200),
            ..TxConfig::new(100, 200, 3)
        }
    }

    /// A bare `simulateTransaction` answer carrying one return value, as
    /// `chain::pool`'s and `inventory`'s own test helpers build it.
    fn simulation(return_xdr: &str, ledger: u32) -> Value {
        json!({"transactionData": transaction_data_b64(1), "events": [],
               "minResourceFee": "1", "results": [{"auth": [], "xdr": return_xdr}],
               "latestLedger": ledger})
    }

    /// One inventory refresh: a zero balance for each of the pool's three
    /// reserves, which is also the whole asset set here because the
    /// configured native asset is the fixture's own XLM.
    fn script_empty_wallet(rpc: &ScriptedRpc, ledger: u32) {
        for _ in 0..3 {
            rpc.expect(
                "simulateTransaction",
                simulation(&scval_b64(&i128_val(0)), ledger),
            );
        }
    }

    /// The auction as the chain holds it, starting at `block`.
    fn auction(block: u32) -> AuctionData {
        AuctionData {
            bid: BTreeMap::from([(USDC.to_string(), BID)]),
            lot: BTreeMap::from([(XLM.to_string(), LOT)]),
            block,
        }
    }

    /// The row the tracker would have written for `auction`: no plan yet,
    /// and the amounts the chain holds.
    fn tracked(account: &str, auction: &AuctionData) -> TrackedAuction {
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

    /// `tick` moved on by `ledgers`, at five seconds a ledger.
    fn later(tick: LedgerTick, ledgers: u32) -> LedgerTick {
        LedgerTick {
            sequence: tick.sequence + ledgers,
            close_time: tick.close_time + u64::from(ledgers) * 5,
        }
    }

    /// The fixture pool's row as the store holds it now.
    async fn row(store: &Store, account: &str) -> Option<TrackedAuction> {
        row_in(store, harness::POOL, account).await
    }

    /// The same, for any pool.
    async fn row_in(store: &Store, pool: &str, account: &str) -> Option<TrackedAuction> {
        store
            .auction(pool, account, AuctionType::UserLiquidation)
            .await
            .expect("read the auction row")
    }

    /// A `Positions` ledger entry for `account`, by reserve index.
    /// Copied from `service.rs`'s test module and widened to carry
    /// collateral as well as liabilities: the filler's own position is
    /// what makes a lower percent *worse* than a higher one, and the
    /// fixture holds no position for the filler's key.
    fn positions_entry_xdr(
        account: &str,
        collateral: &[(u32, i128)],
        liabilities: &[(u32, i128)],
    ) -> String {
        let side = |amounts: &[(u32, i128)]| {
            map(amounts
                .iter()
                .map(|(index, amount)| (ScVal::U32(*index), i128_val(*amount)))
                .collect())
            .expect("positions side")
        };
        let value = map(vec![
            (symbol("collateral").expect("symbol"), side(collateral)),
            (symbol("liabilities").expect("symbol"), side(liabilities)),
            (symbol("supply").expect("symbol"), side(&[])),
        ])
        .expect("positions map");
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(harness::POOL).expect("pool"),
            key: sc_vec(vec![
                symbol("Positions").expect("symbol"),
                address(account).expect("account"),
            ])
            .expect("positions key"),
            durability: ContractDataDurability::Persistent,
            val: value,
        });
        to_base64(&entry).expect("positions entry")
    }

    /// `harness::script_snapshot`'s reserves and oracle reads, with
    /// hand-built positions entries instead of the fixture's. Copied from
    /// `service.rs`'s test module.
    fn script_snapshot_positions(rpc: &ScriptedRpc, positions: &[(&str, String)]) {
        let fixture = mainnet_fixed_v2();
        let ledger = fixture["ledger"].as_u64().expect("ledger");
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(harness::POOL).expect("key"), text(&fixture, &["instance_entry_xdr"])),
                entry(&keys::reserve_list(harness::POOL).expect("key"), text(&fixture, &["res_list_entry_xdr"])),
            ]}),
        );
        let mut entries = Vec::new();
        for reserve in fixture["reserves"].as_array().expect("reserves") {
            let asset = reserve["asset"].as_str().expect("asset");
            entries.push(entry(
                &keys::reserve_config(harness::POOL, asset).expect("key"),
                reserve["config_entry_xdr"].as_str().expect("config"),
            ));
            entries.push(entry(
                &keys::reserve_data(harness::POOL, asset).expect("key"),
                reserve["data_entry_xdr"].as_str().expect("data"),
            ));
        }
        for (account, positions_xdr) in positions {
            entries.push(entry(
                &keys::positions(harness::POOL, account).expect("key"),
                positions_xdr,
            ));
        }
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": entries}),
        );
        let ledger = u32::try_from(ledger).expect("ledger fits");
        rpc.expect(
            "simulateTransaction",
            simulation(text(&fixture, &["oracle_decimals_return_xdr"]), ledger),
        );
        for reserve in fixture["reserves"].as_array().expect("reserves") {
            rpc.expect(
                "simulateTransaction",
                simulation(
                    reserve["lastprice_return_xdr"].as_str().expect("price"),
                    ledger,
                ),
            );
        }
    }

    /// An entry answer, as `harness`'s own private helper builds it.
    fn entry(key: &stellar_xdr::LedgerKey, xdr: &str) -> Value {
        json!({"key": to_base64(key).expect("key"), "xdr": xdr,
               "lastModifiedLedgerSeq": 1, "liveUntilLedgerSeq": 99_999_999_u32})
    }

    /// The filler's own starting position: 15.9 billion b-tokens of the
    /// fixture's third reserve (about $20,031 of effective collateral at
    /// its 0.95 factor) against 38.7 billion d-tokens of USDC (about
    /// $50,050 of effective liability). Chosen so the *whole* auction
    /// lifts it over the 1.65 floor and half of it does not — see
    /// `a_re_plan_for_a_later_ledger_is_written_and_left`.
    const FILLER_COLLATERAL: i128 = 15_900_000_000;
    const FILLER_LIABILITIES: i128 = 38_700_000_000;

    /// A second pool, built here rather than captured: the fixture holds
    /// one pool, and the wallet a multi-pool tick plans against is exactly
    /// what a second one is needed to pin.
    const POOL_TWO: &str = "CAQQR5SWBXKIGZKPBZDH3KM5GQ5GUTPKB7JAFCINLZBC5WXPJKRG3IM7";
    /// An asset of `POOL_TWO` that the fixture's pool does not list.
    const BLND: &str = "CD25MNVTZDL4Y3XBCPCJXGXATV5WUHHOWMYFF4YBEGU5FCPGMYTVG5JY";
    /// The fixture's third reserve, which `POOL_TWO` does not list.
    const EURC: &str = "CDTKPWPLOURQA2SGTKTUQOWRCBZEORB4BWBOMJ3D3ZTQQSGE5F6JBQLV";
    /// The fixture's oracle and admin, reused so `POOL_TWO`'s entries
    /// decode against real strkeys.
    const ORACLE: &str = "CCVTVW2CVA7JLH4ROQGP3CU4T3EXVCK66AZGSM4MUQPXAI4QHCZPOATS";
    const ADMIN: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";

    /// One reserve of the synthetic second pool. Rates are 1.0, so no
    /// accrual moves them and every amount below is also its underlying.
    #[derive(Debug, Clone, Copy)]
    struct SyntheticReserve {
        asset: &'static str,
        c_factor: u32,
        l_factor: u32,
        price: i128,
    }

    /// The instance entry of a synthetic pool: the five config fields
    /// `decode::pool_instance` reads, and the four storage keys around
    /// them. Modelled on `service.rs`'s test module.
    fn instance_entry_xdr(pool: &str) -> String {
        let config = map(vec![
            (symbol("bstop_rate").expect("symbol"), ScVal::U32(2_000_000)),
            (symbol("max_positions").expect("symbol"), ScVal::U32(6)),
            (symbol("min_collateral").expect("symbol"), i128_val(0)),
            (
                symbol("oracle").expect("symbol"),
                address(ORACLE).expect("oracle"),
            ),
            (symbol("status").expect("symbol"), ScVal::U32(1)),
        ])
        .expect("config map");
        let ScVal::Map(Some(config)) = config else {
            panic!("map returns a map")
        };
        let storage = stellar_xdr::ScMap::sorted_from(vec![
            (
                symbol("Admin").expect("symbol"),
                address(ADMIN).expect("admin"),
            ),
            (
                symbol("BLNDTkn").expect("symbol"),
                address(BLND).expect("blnd"),
            ),
            (
                symbol("Backstop").expect("symbol"),
                address(POOL_TWO).expect("backstop"),
            ),
            (symbol("Config").expect("symbol"), ScVal::Map(Some(config))),
            (
                symbol("Name").expect("symbol"),
                ScVal::String(
                    stellar_xdr::ScString::try_from(b"Second Pool".to_vec()).expect("name"),
                ),
            ),
        ])
        .expect("storage map");
        let instance = ScVal::ContractInstance(stellar_xdr::ScContractInstance {
            executable: stellar_xdr::ContractExecutable::StellarAsset,
            storage: Some(storage),
        });
        contract_entry_xdr(pool, ScVal::LedgerKeyContractInstance, instance)
    }

    /// A `ContractData` entry of `pool` holding `value`. The scripted RPC
    /// answers by the key it is asked for, so the entry's own key field
    /// only has to decode.
    fn contract_entry_xdr(pool: &str, key: ScVal, value: ScVal) -> String {
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(pool).expect("pool"),
            key,
            durability: ContractDataDurability::Persistent,
            val: value,
        });
        to_base64(&entry).expect("entry")
    }

    /// Scripts one complete `PoolReader::snapshot` of a synthetic pool:
    /// the shape read, the batched reserve read, then the oracle's
    /// decimals and one `lastprice` per reserve, in reserve-list order —
    /// the call sequence `harness::script_snapshot` scripts for the
    /// fixture, built from parameters instead of a captured ledger.
    fn script_second_pool(
        rpc: &ScriptedRpc,
        reserves: &[SyntheticReserve],
        close_time: u64,
        ledger: u32,
    ) {
        let list = sc_vec(
            reserves
                .iter()
                .map(|reserve| address(reserve.asset).expect("asset"))
                .collect(),
        )
        .expect("reserve list");
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(POOL_TWO).expect("key"), &instance_entry_xdr(POOL_TWO)),
                entry(&keys::reserve_list(POOL_TWO).expect("key"),
                      &contract_entry_xdr(POOL_TWO, ScVal::Void, list)),
            ]}),
        );
        let mut entries = Vec::new();
        for (index, reserve) in reserves.iter().enumerate() {
            let index = u32::try_from(index).expect("index fits");
            let config = map(vec![
                (
                    symbol("c_factor").expect("symbol"),
                    ScVal::U32(reserve.c_factor),
                ),
                (symbol("decimals").expect("symbol"), ScVal::U32(7)),
                (symbol("enabled").expect("symbol"), ScVal::Bool(true)),
                (symbol("index").expect("symbol"), ScVal::U32(index)),
                (
                    symbol("l_factor").expect("symbol"),
                    ScVal::U32(reserve.l_factor),
                ),
                (symbol("max_util").expect("symbol"), ScVal::U32(9_500_000)),
                (symbol("r_base").expect("symbol"), ScVal::U32(0)),
                (symbol("r_one").expect("symbol"), ScVal::U32(0)),
                (symbol("r_three").expect("symbol"), ScVal::U32(0)),
                (symbol("r_two").expect("symbol"), ScVal::U32(0)),
                (symbol("reactivity").expect("symbol"), ScVal::U32(0)),
                (symbol("supply_cap").expect("symbol"), i128_val(0)),
                (symbol("util").expect("symbol"), ScVal::U32(0)),
            ])
            .expect("reserve config");
            let data = map(vec![
                (
                    symbol("b_rate").expect("symbol"),
                    i128_val(1_000_000_000_000),
                ),
                (symbol("b_supply").expect("symbol"), i128_val(0)),
                (symbol("backstop_credit").expect("symbol"), i128_val(0)),
                (
                    symbol("d_rate").expect("symbol"),
                    i128_val(1_000_000_000_000),
                ),
                (symbol("d_supply").expect("symbol"), i128_val(0)),
                (symbol("ir_mod").expect("symbol"), i128_val(10_000_000)),
                (symbol("last_time").expect("symbol"), ScVal::U64(close_time)),
            ])
            .expect("reserve data");
            entries.push(entry(
                &keys::reserve_config(POOL_TWO, reserve.asset).expect("key"),
                &contract_entry_xdr(POOL_TWO, ScVal::Void, config),
            ));
            entries.push(entry(
                &keys::reserve_data(POOL_TWO, reserve.asset).expect("key"),
                &contract_entry_xdr(POOL_TWO, ScVal::Void, data),
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
            let price = map(vec![
                (symbol("price").expect("symbol"), i128_val(reserve.price)),
                (symbol("timestamp").expect("symbol"), ScVal::U64(close_time)),
            ])
            .expect("price");
            rpc.expect(
                "simulateTransaction",
                simulation(&scval_b64(&price), ledger),
            );
        }
    }

    /// The token every `balance` simulation asked about, in the order the
    /// filler asked. Decoded from the envelopes the client actually sent,
    /// so this is what reached the chain and not what a test hoped for.
    fn balance_reads(rpc: &ScriptedRpc) -> Vec<stellar_xdr::ScAddress> {
        rpc.calls("simulateTransaction")
            .iter()
            .filter_map(|params| {
                let encoded = params["transaction"].as_str()?;
                let envelope: stellar_xdr::TransactionEnvelope = from_base64(encoded).ok()?;
                let stellar_xdr::TransactionEnvelope::Tx(v1) = envelope else {
                    return None;
                };
                let stellar_xdr::OperationBody::InvokeHostFunction(op) =
                    &v1.tx.operations.first()?.body
                else {
                    return None;
                };
                let stellar_xdr::HostFunction::InvokeContract(args) = &op.host_function else {
                    return None;
                };
                (args.function_name.to_utf8_string_lossy() == "balance")
                    .then(|| args.contract_address.clone())
            })
            .collect()
    }

    /// The contract's health check refused the whole auction, and the
    /// halved plan no longer holds the filler's floor where the whole one
    /// did — so the planner answers a *later* ledger for it. That plan
    /// goes onto the row and waits, exactly as a first plan for a later
    /// ledger does; nothing is sent before the ledger it was chosen for.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_re_plan_for_a_later_ledger_is_written_and_left(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        // The filler already owes more than it holds, so half the auction
        // lifts it less than the whole does.
        script_snapshot_positions(
            &rpc,
            &[(
                signer.address(),
                positions_entry_xdr(
                    signer.address(),
                    &[(2, FILLER_COLLATERAL)],
                    &[(1, FILLER_LIABILITIES)],
                ),
            )],
        );
        script_empty_wallet(&rpc, tick.sequence);
        // One simulation only: the refusal. A second would mean the
        // halved plan was sent before its ledger.
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_refused(&rpc, 1_205, tick.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary {
                planned: 2,
                ..TickSummary::default()
            },
            "both drafts were written; neither was executed — the first was refused and \
             the second is for a ledger that has not come"
        );
        let row = row(&store, harness::USER_ONE).await.expect("the row stays");
        assert_eq!(
            (row.fill_ledger, row.percent.map(FillPercent::get)),
            (Some(tick.sequence + 62), Some(50)),
            "the halved plan needs 61 more ledgers of the bid ramp to hold the floor: it \
             projects 1.6518 there against the 1.65 the whole auction already cleared"
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            fills.n,
            Some(0),
            "a refusal records nothing, and a plan whose ledger has not come sends nothing"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            4,
            "the auction entry, the snapshot's two reads, and one source-account read: a              second account read would mean the re-plan was simulated, which is the first              step of sending it"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// One wallet read covers every pool's assets, not just the pool whose
    /// snapshot triggered it: `record_balances` replaces the whole map, so
    /// a read of one pool's reserves would leave the next pool planning
    /// against a wallet of zero.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_wallet_read_covers_every_pools_assets(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let first = auction(tick.sequence - 300);
        // The second pool pays in BLND, which the fixture's pool does not
        // list at all.
        let second = AuctionData {
            bid: BTreeMap::from([(BLND.to_string(), 1_000_000_000)]),
            lot: BTreeMap::from([(XLM.to_string(), 100_000_000_000)]),
            block: tick.sequence - 300,
        };
        store
            .upsert_auction(&tracked(harness::USER_ONE, &first))
            .await
            .expect("seed the first pool's auction");
        store
            .upsert_auction(&TrackedAuction {
                pool: POOL_TWO.to_string(),
                ..tracked(harness::USER_TWO, &second)
            })
            .await
            .expect("seed the second pool's auction");
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        harness::script_auction_entry(&rpc, harness::USER_ONE, &first, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_empty_wallet(&rpc, tick.sequence);
        harness::script_auction_entry_in(&rpc, POOL_TWO, harness::USER_TWO, &second, tick.sequence);
        script_second_pool(
            &rpc,
            &[
                SyntheticReserve {
                    asset: XLM,
                    c_factor: 7_500_000,
                    l_factor: 7_500_000,
                    price: 1_778_617,
                },
                SyntheticReserve {
                    asset: BLND,
                    c_factor: 9_500_000,
                    l_factor: 9_500_000,
                    price: 10_000_000,
                },
            ],
            tick.close_time,
            tick.sequence,
        );
        // Four balances this time: the union, which BLND has joined.
        for _ in 0..4 {
            rpc.expect(
                "simulateTransaction",
                simulation(&scval_b64(&i128_val(0)), tick.sequence),
            );
        }
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let pools = vec![
            pool_config(),
            PoolConfig {
                address: POOL_TWO.to_string(),
                ..pool_config()
            },
        ];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, false, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary {
                planned: 2,
                ..TickSummary::default()
            },
            "both pools' auctions were planned against the one wallet"
        );
        let expected: Vec<stellar_xdr::ScAddress> = [XLM, USDC, EURC, XLM, USDC, BLND, EURC]
            .iter()
            .map(|asset| sc_address(asset).expect("asset"))
            .collect();
        assert_eq!(
            balance_reads(&rpc),
            expected,
            "the first pool's read covers its own three reserves; the second's covers the \
             union, so the second pool's BLND is not invisible to it"
        );
        assert_eq!(
            state.covered_assets,
            BTreeSet::from([
                XLM.to_string(),
                USDC.to_string(),
                BLND.to_string(),
                EURC.to_string()
            ])
        );
        assert_eq!(
            row_in(&store, POOL_TWO, harness::USER_TWO)
                .await
                .expect("the row stays")
                .fill_ledger,
            Some(tick.sequence + 1)
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Ruling 8's set is not a leak: an auction the store no longer holds
    /// a row for is dropped from it, so a later auction for the same
    /// account is recorded again — and the set does not grow for as long
    /// as the process runs.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_closed_auctions_dry_run_record_is_forgotten(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let opened = auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &opened))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_auction_entry(&rpc, harness::USER_ONE, &opened, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("the first tick");
        assert_eq!(
            state.recorded_dry_run.len(),
            1,
            "the dry run recorded this auction once"
        );

        // The tracker, applying the fill that closed it: the row goes.
        store
            .delete_auction(
                harness::POOL,
                harness::USER_ONE,
                AuctionType::UserLiquidation,
            )
            .await
            .expect("close the auction");
        let empty = filler
            .tick(&mut state, later(tick, 1), true, None, &shutdown)
            .await
            .expect("the second tick");

        assert_eq!(empty, TickSummary::default());
        assert!(
            state.recorded_dry_run.is_empty(),
            "an auction the store no longer holds is dropped from the set that suppresses it"
        );

        // A new liquidation of the same account, at a later start ledger.
        let again = auction(tick.sequence - 250);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &again))
            .await
            .expect("seed the new auction");
        let third = later(tick, 2);
        harness::script_auction_entry(&rpc, harness::USER_ONE, &again, third.sequence);
        harness::script_snapshot(&rpc, &[]);

        let summary = filler
            .tick(&mut state, third, true, None, &shutdown)
            .await
            .expect("the third tick");

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            }
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(2), "a new auction is a new record");
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A partial fill by someone else keeps the auction's start ledger and
    /// leaves a remainder — which is what an armed filler would now fill,
    /// so a dry run records the remainder afresh. The remainder's own
    /// amounts, not the start ledger and not any ledger the row carries,
    /// are what tell the two versions apart.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_remainder_someone_else_left_is_recorded_again(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let opened = auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &opened))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_auction_entry(&rpc, harness::USER_ONE, &opened, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("the first tick");
        assert_eq!(state.recorded_dry_run.len(), 1);

        // The tracker, applying a competitor's 40% fill: the contract
        // stores the remainder under the same block, and the tracker
        // re-reads it and rewrites the row at the ledger it read it at.
        let second = later(tick, 1);
        let remainder = AuctionData {
            bid: BTreeMap::from([(USDC.to_string(), BID * 6 / 10)]),
            lot: BTreeMap::from([(XLM.to_string(), LOT * 6 / 10)]),
            block: opened.block,
        };
        store
            .upsert_auction(&TrackedAuction {
                updated_ledger: second.sequence,
                ..tracked(harness::USER_ONE, &remainder)
            })
            .await
            .expect("the tracker rewrites the row");
        harness::script_auction_entry(&rpc, harness::USER_ONE, &remainder, second.sequence);
        harness::script_snapshot(&rpc, &[]);

        let summary = filler
            .tick(&mut state, second, true, None, &shutdown)
            .await
            .expect("the second tick");

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            },
            "the remainder is a new fill to record"
        );
        assert_eq!(
            state.recorded_dry_run.len(),
            2,
            "both versions stay recorded while the auction is open: a version is pruned by \
             the row's content only when the auction closes, because the row can lag the \
             chain by a fill the tracker has not applied yet"
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            fills.n,
            Some(2),
            "one per version of the auction the chain held"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The version is the content, not the ledger: a remainder the filler
    /// sees on chain one tick before the tracker rewrites the row is
    /// recorded once, under the amounts the chain held — and when the
    /// tracker's rewrite arrives with those same amounts, the row is
    /// recognised as recorded and costs no chain read.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_remainder_seen_before_the_tracker_rewrote_the_row_is_recorded_once(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let opened = auction(tick.sequence - 300);
        // The store still holds the auction as opened: the tracker has not
        // applied the competitor's 40% fill yet.
        store
            .upsert_auction(&tracked(harness::USER_ONE, &opened))
            .await
            .expect("seed the auction");
        let remainder = AuctionData {
            bid: BTreeMap::from([(USDC.to_string(), BID * 6 / 10)]),
            lot: BTreeMap::from([(XLM.to_string(), LOT * 6 / 10)]),
            block: opened.block,
        };
        let rpc = ScriptedRpc::start().await;
        // But the chain already holds the remainder.
        harness::script_auction_entry(&rpc, harness::USER_ONE, &remainder, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let first = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("the first tick");
        assert_eq!(
            first,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            },
            "the remainder the chain holds is what was planned and recorded"
        );
        let reads = rpc.calls("getLedgerEntries").len();

        // The tracker catches up and rewrites the row with the same
        // remainder. Nothing is scripted for this tick: the row is now
        // recognised as recorded without a chain read.
        let second = later(tick, 1);
        store
            .upsert_auction(&TrackedAuction {
                updated_ledger: second.sequence,
                ..tracked(harness::USER_ONE, &remainder)
            })
            .await
            .expect("the tracker rewrites the row");
        let summary = filler
            .tick(&mut state, second, true, None, &shutdown)
            .await
            .expect("the second tick");

        assert_eq!(
            summary,
            TickSummary::default(),
            "the same version is not recorded twice"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            reads,
            "and costs no chain read"
        );
        assert_eq!(state.recorded_dry_run.len(), 1);
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(1));
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Planned, but its ledger has not come: the plan goes onto the row and
    /// nothing is executed.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_fill_whose_ledger_has_not_come_is_planned_and_left(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                ..TickSummary::default()
            }
        );
        let row = row(&store, harness::USER_ONE).await.expect("the row stays");
        assert_eq!(row.fill_ledger, Some(tick.sequence + HEALTHY_DELTA));
        assert!(
            row.fill_ledger.expect("planned") > tick.sequence + 1,
            "the ledger a transaction sent now would land in has not reached the plan"
        );
        assert_eq!(row.percent.map(FillPercent::get), Some(100));
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(0), "nothing was executed");
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            4,
            "the oracle's decimals and one price per reserve, and no fill simulation"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Its ledger has come: executed, and — a keyless dry run — recorded
    /// unsimulated.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_fill_whose_ledger_has_come_is_recorded(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            }
        );
        let planned = row(&store, harness::USER_ONE)
            .await
            .expect("the row stays")
            .fill_ledger;
        assert_eq!(planned, Some(tick.sequence + 1));
        let fill = sqlx::query!(
            r#"SELECT tx_hash, dry_run, pool, account, auction_type, fill_ledger, percent,
                      bid, lot FROM fills"#
        )
        .fetch_one(store.pool())
        .await?;
        assert_eq!(
            fill.tx_hash, None,
            "a dry run sends nothing to name it with"
        );
        assert!(fill.dry_run);
        assert_eq!(
            (fill.pool.as_str(), fill.account.as_str(), fill.auction_type),
            (harness::POOL, harness::USER_ONE, 0)
        );
        assert_eq!(
            (fill.fill_ledger, fill.percent),
            (i64::from(tick.sequence + 1), 100)
        );
        assert_eq!(
            fill.bid[USDC],
            json!("9900000000"),
            "the bid scaled to the 301st ledger of the ramp, where it is 49.5% of the whole"
        );
        assert_eq!(fill.lot[XLM], json!("1000000000000"), "the whole lot");
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Ruling 8: a dry run records an auction once, not on every tick until
    /// someone else fills it — and makes no chain read for it afterwards.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_dry_run_fill_is_recorded_once(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        // Scripted for the first tick only: the second must reach the
        // chain not at all, and an unscripted call answers HTTP 500.
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("the first tick");
        let reads = rpc.calls("getLedgerEntries").len();
        let summary = filler
            .tick(&mut state, later(tick, 1), true, None, &shutdown)
            .await
            .expect("the second tick");

        assert_eq!(
            summary,
            TickSummary::default(),
            "an auction this process has already recorded a dry-run fill for is not \
             considered again"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            reads,
            "and costs no chain read at all"
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(1), "recorded once, not once a ledger");
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Spec §5: an entry the chain no longer holds closes the row, and no
    /// snapshot is read for it.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_auction_the_chain_no_longer_holds_is_closed(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_no_auction(&rpc, tick.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary {
                closed: 1,
                ..TickSummary::default()
            }
        );
        assert!(
            row(&store, harness::USER_ONE).await.is_none(),
            "the auction the chain no longer holds is closed"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            1,
            "the entry read, and no snapshot: there was nothing live to plan against"
        );
        assert!(rpc.calls("simulateTransaction").is_empty());
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Unsupported assets and the bot's own account cost no chain read.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_auction_the_filler_does_not_take_costs_no_chain_read(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        // The first pays USDC, which this pool's `supported_bid` does not
        // name; the second is supported outright and is the bot's own.
        let unsupported = auction(tick.sequence - 300);
        let mut own = auction(tick.sequence - 300);
        own.bid = BTreeMap::from([(XLM.to_string(), BID)]);
        own.lot = BTreeMap::from([(USDC.to_string(), LOT)]);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &unsupported))
            .await
            .expect("seed the unsupported auction");
        store
            .upsert_auction(&tracked(harness::USER_TWO, &own))
            .await
            .expect("seed the bot's own auction");
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![PoolConfig {
            supported_bid: vec![XLM.to_string()],
            ..pool_config()
        }];
        let config = FillerConfig {
            own_addresses: BTreeSet::from([harness::USER_TWO.to_string()]),
            ..filler_config()
        };
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            config,
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(summary, TickSummary::default());
        assert!(
            rpc.received().await.is_empty(),
            "an auction the filler would never take is filtered out of the store's own rows, \
             before anything is read from chain"
        );
        assert_eq!(
            row(&store, harness::USER_ONE)
                .await
                .expect("the row stays")
                .fill_ledger,
            None,
            "and nothing was planned onto it"
        );
        Ok(())
    }

    /// The replan cadence: planned once, then not again until the period
    /// passes — except near the fill ledger, where every ledger re-plans.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_planned_auction_waits_for_its_replan_period(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        // The four ticks that plan; the fifth read below is the one that
        // must not happen.
        for _ in 0..4 {
            harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
            harness::script_snapshot(&rpc, &[]);
        }
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let plans = |summary: &TickSummary| summary.planned;

        // At its own tick: never planned before, so planned now.
        let first = filler
            .tick(&mut state, tick, false, None, &shutdown)
            .await
            .expect("tick");
        assert_eq!(plans(&first), 1);
        let reads = rpc.calls("getLedgerEntries").len();

        // One ledger later: inside the replan period, and 63 ledgers from
        // the fill ledger — no chain read at all.
        let quiet = filler
            .tick(&mut state, later(tick, 1), false, None, &shutdown)
            .await
            .expect("tick");
        assert_eq!(quiet, TickSummary::default());
        assert_eq!(rpc.calls("getLedgerEntries").len(), reads);

        // `replan_ledgers` after the plan: planned again.
        let period = filler
            .tick(&mut state, later(tick, 10), false, None, &shutdown)
            .await
            .expect("tick");
        assert_eq!(plans(&period), 1);

        // `replan_near_ledgers` from the fill ledger: planned again.
        let near = filler
            .tick(
                &mut state,
                later(tick, HEALTHY_DELTA - 5),
                false,
                None,
                &shutdown,
            )
            .await
            .expect("tick");
        assert_eq!(plans(&near), 1);

        // And the ledger after that, where only the nearness clause can
        // make it due: the period since the plan above is one ledger.
        let nearer = filler
            .tick(
                &mut state,
                later(tick, HEALTHY_DELTA - 4),
                false,
                None,
                &shutdown,
            )
            .await
            .expect("tick");
        assert_eq!(plans(&nearer), 1);
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Ruling 9: before the startup delay has passed, plans are made and
    /// nothing is executed.
    #[sqlx::test(migrations = "./migrations")]
    async fn nothing_is_executed_before_the_startup_delay(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, false, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                ..TickSummary::default()
            },
            "the plan is made; the execution is what the startup delay holds back"
        );
        assert_eq!(
            row(&store, harness::USER_ONE)
                .await
                .expect("the row stays")
                .fill_ledger,
            Some(tick.sequence + 1)
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(0));
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The contract's health check refused the plan: planned once more at
    /// half the percent, and that is what is recorded.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_health_refusal_is_planned_again_at_half_the_percent(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_empty_wallet(&rpc, tick.sequence);
        // The contract's own health check refuses the first plan, and
        // accepts the one at half the percent.
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_refused(&rpc, 1_205, tick.sequence);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary {
                planned: 2,
                executed: 1,
                ..TickSummary::default()
            },
            "two drafts written — the refused one and the half — and one execution"
        );
        let fill = sqlx::query!("SELECT percent, dry_run, fill_ledger FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            fill.percent, 50,
            "the contract judged the percent; nothing here predicted it"
        );
        assert!(fill.dry_run);
        assert_eq!(fill.fill_ledger, i64::from(tick.sequence + 1));
        assert_eq!(
            row(&store, harness::USER_ONE)
                .await
                .expect("the row stays")
                .percent
                .map(FillPercent::get),
            Some(50),
            "and the row carries what was actually filled, not the refused plan"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Spec §10's overlap case: the loser of a sequence race re-plans
    /// against the remainder the winner's partial fill left, never resends
    /// its own plan.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_stale_fill_is_planned_again_against_the_remainder(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_empty_wallet(&rpc, tick.sequence);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            FillerConfig {
                dry_run: false,
                ..filler_config()
            },
            Executor::new(&store, Some(submitter), false),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        // A stand-in queue worker: the first submission loses the race for
        // this key's sequence number, the second lands.
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let worker = tokio::spawn(async move {
            let mut first = true;
            while let Some(queued) = receiver.recv().await {
                let answer = if std::mem::take(&mut first) {
                    Err(QueueError::Chain(ChainError::BadSequence))
                } else {
                    Ok(TxOutcome::Succeeded {
                        hash: TxHash([7_u8; 32]),
                        ledger: 1,
                        return_value: None,
                    })
                };
                let _ = queued.respond.send(answer);
            }
        });

        let first = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("the first tick");

        assert_eq!(
            first,
            TickSummary {
                planned: 1,
                skipped: 1,
                ..TickSummary::default()
            },
            "a stale plan is not an execution: it is re-planned from fresh state"
        );
        let stale = row(&store, harness::USER_ONE).await.expect("the row stays");
        assert_eq!(
            (stale.fill_ledger, stale.percent),
            (None, None),
            "the plan the race invalidated is cleared off the row"
        );

        // The tracker, applying the winner's 40% fill: the remainder is
        // what the chain now holds, and it has no plan.
        let mut remainder = auction;
        remainder.bid = BTreeMap::from([(USDC.to_string(), 12_000_000_000)]);
        remainder.lot = BTreeMap::from([(XLM.to_string(), 600_000_000_000)]);
        store
            .upsert_auction(&TrackedAuction {
                updated_ledger: tick.sequence + 1,
                ..tracked(harness::USER_ONE, &remainder)
            })
            .await
            .expect("apply the winner's fill");

        let second_tick = later(tick, 1);
        harness::script_auction_entry(&rpc, harness::USER_ONE, &remainder, second_tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_simulate_prelude(&rpc, &signer, 11, second_tick.sequence);
        script_simulate_accepted(&rpc, second_tick.sequence);

        let second = filler
            .tick(&mut state, second_tick, true, Some(&queue), &shutdown)
            .await
            .expect("the second tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert_eq!(
            second,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            }
        );
        let fills = sqlx::query!("SELECT tx_hash, bid, lot, percent FROM fills ORDER BY id")
            .fetch_all(store.pool())
            .await?;
        assert_eq!(
            fills.len(),
            2,
            "the stale attempt was recorded before it was sent"
        );
        assert_eq!(fills[0].tx_hash, None, "and never became a transaction");
        assert!(fills[1].tx_hash.is_some());
        assert_eq!(
            fills[1].bid[USDC],
            json!("5880000000"),
            "60% of the bid, scaled to the 302nd ledger of the ramp"
        );
        assert_eq!(
            fills[1].lot[XLM],
            json!("600000000000"),
            "the remainder's whole lot, not the original's"
        );
        assert_eq!(fills[1].percent, 100);
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// One auction's chain failure is that auction's; the next is still
    /// planned.
    #[sqlx::test(migrations = "./migrations")]
    async fn one_failing_auction_does_not_stop_the_pass(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        // Distinct start ledgers, so `open_auctions`'s order is the one
        // this test scripts: the failing read first.
        let first = auction(tick.sequence - 300);
        let second = auction(tick.sequence - 299);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &first))
            .await
            .expect("seed the first auction");
        store
            .upsert_auction(&tracked(harness::USER_TWO, &second))
            .await
            .expect("seed the second auction");
        let rpc = ScriptedRpc::start().await;
        rpc.expect_http("getLedgerEntries", 500);
        harness::script_auction_entry(&rpc, harness::USER_TWO, &second, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, false, None, &shutdown)
            .await
            .expect("one auction's chain failure is not the tick's");

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                ..TickSummary::default()
            }
        );
        assert_eq!(
            row(&store, harness::USER_ONE)
                .await
                .expect("the row stays")
                .fill_ledger,
            None,
            "the auction whose entry could not be read was left exactly as it was"
        );
        assert_eq!(
            row(&store, harness::USER_TWO)
                .await
                .expect("the row stays")
                .fill_ledger,
            Some(tick.sequence + 1),
            "and the one behind it was still planned"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A fill that landed, or may have, ends this pool's walk: every
    /// auction still to come in it was projected against the positions
    /// and the wallet as they stood before that fill, so the rest of the
    /// pool waits for the next tick's snapshot.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_landed_fill_ends_this_pools_pass(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        // Distinct start ledgers, so `open_auctions`'s order is the one
        // this test scripts: the one that is filled comes first.
        let first = auction(tick.sequence - 300);
        let second = auction(tick.sequence - 299);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &first))
            .await
            .expect("seed the first auction");
        store
            .upsert_auction(&tracked(harness::USER_TWO, &second))
            .await
            .expect("seed the second auction");
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // Both entries are re-read before the snapshot is taken, so both
        // are scripted: what the landed fill stops is the planning that
        // comes after it.
        harness::script_auction_entry(&rpc, harness::USER_ONE, &first, tick.sequence);
        harness::script_auction_entry(&rpc, harness::USER_TWO, &second, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_empty_wallet(&rpc, tick.sequence);
        // One judgment only. A second would mean the second auction was
        // planned and executed against a view this fill has moved past,
        // and it would answer HTTP 500 here.
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            FillerConfig {
                dry_run: false,
                ..filler_config()
            },
            Executor::new(&store, Some(submitter), false),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        // A stand-in queue worker: this test is about what the filler does
        // once a fill has landed, not about how the queue landed it.
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash: TxHash([2_u8; 32]),
                    ledger: 1,
                    return_value: None,
                }));
            }
        });

        let summary = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            },
            "the first auction was filled and the second was not reached at all"
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(1), "one execution, not one per open auction");
        let left = row(&store, harness::USER_TWO)
            .await
            .expect("the second auction's row stays open");
        assert_eq!(
            (left.fill_ledger, left.percent),
            (None, None),
            "and unplanned: it is planned next tick, against a snapshot that holds the fill"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The wallet cannot fund this fill: the reservation is refused, the
    /// auction is skipped, and nothing is recorded or sent.
    ///
    /// Driven through `execute_once` rather than `Filler::tick` on
    /// purpose. `plan_fill` sizes every spend against the same
    /// `Inventory::available` the reservation is then taken out of — its
    /// own `a_plan_never_spends_more_than_the_wallet_holds` proves it —
    /// and nothing inside a tick moves the wallet between the two, so a
    /// tick cannot reach this refusal by itself. It is the guard for a
    /// wallet that moved under a plan, which is what a second holder of
    /// the same ledger would do, so a test of it has to hand the plan a
    /// spend the wallet no longer covers.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_wallet_that_cannot_fund_a_fill_skips_it(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let snapshot = PoolReader::new(&client, harness::POOL)
            .snapshot(&[])
            .await
            .expect("snapshot");
        // A tenth of what the draft below repays.
        let inventory = Inventory::new(XLM.to_string(), 0);
        inventory.record_balances(
            BTreeMap::from([(USDC.to_string(), 1_000_000_000)]),
            Instant::now(),
        );
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            FillerConfig {
                dry_run: false,
                ..filler_config()
            },
            // Live and keyless: the refusal comes before the executor is
            // asked anything at all, which is the point.
            Executor::new(&store, None, false),
            inventory,
        );
        let context = filler
            .pool_context(&pools[0], snapshot, tick)
            .expect("the fixture pool plans");
        let draft = FillDraft {
            fill_ledger: tick.sequence + 1,
            percent: FillPercent::try_from(WHOLE_AUCTION).expect("100 is in range"),
            actions: vec![FillAction::Repay {
                asset: USDC.to_string(),
                amount: 10_000_000_000,
            }],
            to_fill: auction.clone(),
            lot_value: 2,
            bid_value: 1,
            est_profit: 1,
            spend: BTreeMap::from([(USDC.to_string(), 10_000_000_000)]),
            projected_health: Some(20_000_000),
        };
        let mut state = FillerState::default();
        let mut pass = Pass {
            tick,
            state: &mut state,
            summary: TickSummary::default(),
        };

        let outcome = filler
            .execute_once(
                &context,
                &tracked(harness::USER_ONE, &auction),
                &draft,
                None,
                &mut pass,
            )
            .await
            .expect("a wallet that cannot fund one fill is not the tick's failure");

        assert!(outcome.is_none(), "nothing was executed");
        assert_eq!(
            pass.summary,
            TickSummary {
                skipped: 1,
                ..TickSummary::default()
            },
            "a refused reservation is a decision, and it is counted as one"
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            fills.n,
            Some(0),
            "nothing that was never funded is recorded"
        );
        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "and nothing was sent"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Several due auctions in one pool share one snapshot.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_pools_auctions_share_one_snapshot(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let first = auction(tick.sequence - 300);
        let second = auction(tick.sequence - 299);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &first))
            .await
            .expect("seed the first auction");
        store
            .upsert_auction(&tracked(harness::USER_TWO, &second))
            .await
            .expect("seed the second auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_auction_entry(&rpc, harness::USER_ONE, &first, tick.sequence);
        harness::script_auction_entry(&rpc, harness::USER_TWO, &second, tick.sequence);
        // One snapshot between them, and an unscripted second would be an
        // HTTP 500 this test fails on.
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, false, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary {
                planned: 2,
                ..TickSummary::default()
            }
        );
        for account in [harness::USER_ONE, harness::USER_TWO] {
            assert_eq!(
                row(&store, account)
                    .await
                    .expect("the row stays")
                    .fill_ledger,
                Some(tick.sequence + 1),
                "{account} was planned against the shared snapshot"
            );
        }
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            4,
            "two auction entries, then the one snapshot's shape and full reads"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            4,
            "the one snapshot's oracle reads"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A new auction the chain already holds, while the store still holds
    /// the previous one: the tracker has not applied the events that closed
    /// the old auction and opened the new. The filler re-reads the chain,
    /// so it plans and records the new one — once. A version at or past
    /// the row's start ledger survives `prune_recorded` until the row
    /// advances, so the next tick neither records it again nor, once the
    /// tracker has caught up, reads the chain for it.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_new_auction_seen_before_the_tracker_opened_it_is_recorded_once(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let old = auction(tick.sequence - 300);
        let new = auction(tick.sequence - 200);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &old))
            .await
            .expect("the store still holds the old auction");
        let rpc = ScriptedRpc::start().await;
        // Tick one: the chain holds the new auction.
        harness::script_auction_entry(&rpc, harness::USER_ONE, &new, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let first = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("the first tick");
        assert_eq!(
            first,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            }
        );

        // Tick two: the tracker still lags, so the row still says the old
        // auction and the entry is re-read — and found already recorded.
        let second = later(tick, 1);
        harness::script_auction_entry(&rpc, harness::USER_ONE, &new, second.sequence);
        let summary = filler
            .tick(&mut state, second, true, None, &shutdown)
            .await
            .expect("the second tick");
        assert_eq!(
            summary,
            TickSummary::default(),
            "the new auction's record survived the prune and suppresses a second record"
        );
        assert_eq!(state.recorded_dry_run.len(), 1);
        let reads = rpc.calls("getLedgerEntries").len();

        // Tick three: the tracker has caught up and the row is the new
        // auction. Nothing is scripted: it is recognised without a read.
        let third = later(tick, 2);
        store
            .upsert_auction(&TrackedAuction {
                updated_ledger: third.sequence,
                ..tracked(harness::USER_ONE, &new)
            })
            .await
            .expect("the tracker opens the new auction");
        let summary = filler
            .tick(&mut state, third, true, None, &shutdown)
            .await
            .expect("the third tick");
        assert_eq!(summary, TickSummary::default());
        assert_eq!(rpc.calls("getLedgerEntries").len(), reads, "no chain read");
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(1), "recorded once across the tracker's lag");
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }
}
