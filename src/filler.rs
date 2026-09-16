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
//!    moved past (spec §8).
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

/// What one run of the filler remembers between ticks.
///
/// All three are in memory on purpose. Losing them on a restart costs one
/// extra plan per auction, one extra wallet read, and — for
/// `recorded_dry_run` — one extra dry-run `fills` row per open auction:
/// cheap, and none of it is a chain effect. The store holds everything
/// that must survive.
#[derive(Debug, Default)]
pub struct FillerState {
    /// Pool and account to the ledger this process last planned it at.
    last_planned: BTreeMap<(String, String), u32>,
    /// Pool, account and start ledger of every auction this process has
    /// recorded a dry-run fill for (ruling 8). Keyed by the start ledger
    /// so that a *new* auction for the same account — a different
    /// liquidation — is recorded again.
    recorded_dry_run: BTreeSet<(String, String, u32)>,
    /// Set by a submission that landed or may have landed, so the next
    /// pool pass re-reads the wallet however fresh its balances look.
    inventory_stale: bool,
}

impl FillerState {
    /// Forgets when this auction was planned, so the next tick plans it
    /// from fresh state rather than waiting out the replan period.
    fn forget(&mut self, pool: &str, account: &str) {
        self.last_planned
            .remove(&(pool.to_string(), account.to_string()));
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
            self.plan_and_act(&context, &row, &auction, execute, queue, pass)
                .await?;
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
        let key = (row.pool.clone(), row.account.clone());
        if pass
            .state
            .recorded_dry_run
            .contains(&(key.0.clone(), key.1.clone(), row.start_ledger))
        {
            return false;
        }
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
                    pass.state.forget(&row.pool, &row.account);
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
        if !state.inventory_stale
            && !self
                .inventory
                .stale(Instant::now(), self.config.inventory_refresh)
        {
            return;
        }
        let mut assets: BTreeSet<String> = snapshot.asset_index.keys().cloned().collect();
        assets.insert(self.config.native_asset.clone());
        match read_balances(reader, filler, &assets).await {
            Ok(balances) => {
                self.inventory.record_balances(balances, Instant::now());
                state.inventory_stale = false;
                tracing::debug!(
                    pool = %snapshot.pool,
                    assets = assets.len(),
                    "the filler's wallet balances were re-read"
                );
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

    /// Steps 4 and 5 for one auction.
    async fn plan_and_act(
        &self,
        context: &PoolPass<'_>,
        row: &TrackedAuction,
        auction: &AuctionData,
        execute: bool,
        queue: Option<&SubmissionQueue>,
        pass: &mut Pass<'_>,
    ) -> Result<(), FillerError> {
        let whole = match FillPercent::try_from(WHOLE_AUCTION) {
            Ok(percent) => percent,
            Err(error) => {
                tracing::warn!(
                    pool = %row.pool,
                    account = %row.account,
                    %error,
                    "the whole-auction percent does not construct; skipping this auction"
                );
                return Ok(());
            }
        };
        let Some(draft) = self.drafted(context, row, auction, whole, pass).await? else {
            return Ok(());
        };
        if !self.write_plan(row, &draft, pass).await? {
            return Ok(());
        }
        if !execute || draft.fill_ledger > context.earliest_ledger {
            return Ok(());
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

    /// Step 5: one execution and what its answer means.
    async fn execute_draft(
        &self,
        context: &PoolPass<'_>,
        row: &TrackedAuction,
        auction: &AuctionData,
        draft: &FillDraft,
        queue: Option<&SubmissionQueue>,
        pass: &mut Pass<'_>,
    ) -> Result<(), FillerError> {
        let Some(outcome) = self.execute_once(context, row, draft, queue, pass).await? else {
            return Ok(());
        };
        match outcome {
            ExecOutcome::Recorded(recorded) => {
                note_recorded(row, &recorded, pass);
                Ok(())
            }
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
                Ok(())
            }
            ExecOutcome::Stale => {
                self.clear_plan(row).await?;
                pass.state.forget(&row.pool, &row.account);
                pass.summary.skipped += 1;
                Ok(())
            }
        }
    }

    /// Ruling 12's one re-plan, at half the refused percent and never
    /// below 1. A second refusal is a skip: the contract has now disagreed
    /// twice, and a third guess costs another simulation for the same
    /// answer.
    async fn replan(
        &self,
        context: &PoolPass<'_>,
        row: &TrackedAuction,
        auction: &AuctionData,
        refused: &FillDraft,
        queue: Option<&SubmissionQueue>,
        pass: &mut Pass<'_>,
    ) -> Result<(), FillerError> {
        let half = match FillPercent::try_from((refused.percent.get() / 2).max(1)) {
            Ok(percent) => percent,
            Err(error) => {
                tracing::warn!(pool = %row.pool, account = %row.account, %error, "half a percent is not one");
                pass.summary.skipped += 1;
                return Ok(());
            }
        };
        let Some(draft) = self.drafted(context, row, auction, half, pass).await? else {
            return Ok(());
        };
        if !self.write_plan(row, &draft, pass).await? {
            return Ok(());
        }
        let Some(outcome) = self.execute_once(context, row, &draft, queue, pass).await? else {
            return Ok(());
        };
        match outcome {
            ExecOutcome::Recorded(recorded) => note_recorded(row, &recorded, pass),
            ExecOutcome::Stale => {
                self.clear_plan(row).await?;
                pass.state.forget(&row.pool, &row.account);
                pass.summary.skipped += 1;
            }
            other => {
                tracing::info!(
                    pool = %row.pool,
                    account = %row.account,
                    outcome = ?other,
                    "the contract refused the re-plan too; leaving this auction for the next tick"
                );
                pass.summary.skipped += 1;
            }
        }
        Ok(())
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
fn note_recorded(row: &TrackedAuction, recorded: &FillRecorded, pass: &mut Pass<'_>) {
    pass.summary.executed += 1;
    if recorded.dry_run {
        pass.state.recorded_dry_run.insert((
            row.pool.clone(),
            row.account.clone(),
            row.start_ledger,
        ));
    }
    if matches!(
        recorded.submission,
        Some(TxOutcome::Succeeded { .. } | TxOutcome::Unknown { .. })
    ) {
        pass.state.inventory_stale = true;
    }
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
    use crate::chain::xdr::encode::i128_val;
    use crate::chain::{ChainError, TxHash, TxOutcome};
    use crate::harness;
    use crate::queue::QueueError;

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

    /// The row as the store holds it now.
    async fn row(store: &Store, account: &str) -> Option<TrackedAuction> {
        store
            .auction(harness::POOL, account, AuctionType::UserLiquidation)
            .await
            .expect("read the auction row")
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
}
