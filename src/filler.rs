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
//!
//! Then, once the whole walk is done, the tick's seventh step: one
//! **unwind pass** per pool a fill landed in — and, on the first tick,
//! per configured pool. It repays the debt the filler took over and
//! withdraws the collateral it received, and it repeats every tick until
//! a pass has nothing left to move. The second `impl Filler` block below
//! is where it lives and what its rulings are; the requests it sends are
//! built by [`crate::math::unwind`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::chain::pool::{PoolReader, PoolSnapshot};
use crate::chain::rpc::RpcClient;
use crate::chain::tx::Priority;
use crate::chain::xdr::{AuctionType, FillPercent, PoolStatus};
use crate::chain::TxOutcome;
use crate::config::PoolConfig;
use crate::executor::{
    ExecOutcome, Executor, ExecutorError, FillPlan, FillRecorded, UnwindOutcome,
};
use crate::inventory::{read_balances, Inventory, Settlement};
use crate::ledger::LedgerTick;
use crate::math::fill::{
    health_floor, plan_fill, to_oracle_units, FillDraft, FillInputs, FillSkip, FillTerms,
    PlannedFill,
};
use crate::math::unwind::{plan_unwind, UnwindInputs, UnwindPlan, UnwindTerms};
use crate::math::{AuctionData, MathError, Positions, Reserve};
use crate::metrics::{Attempt, Metrics, SkipLabel};
use crate::notifier::{Delivery, Notification, NotificationKind, Notifier, Severity};
use crate::queue::{QueueError, SubmissionQueue};
use crate::store::{Store, StoreError, TrackedAuction};

/// The whole of the auction, and the largest percent any plan may name.
/// The executor's one re-plan is the only thing that lowers it (ruling
/// 12), and it lowers it from whatever the contract refused.
const WHOLE_AUCTION: u32 = 100;

/// The auction age, in ledgers past its start, at which
/// `delete_stale_auction` stops refusing and becomes callable by anyone.
/// Not a fill bound — the contract has none — but a plan aimed at or past
/// it is racing a deletion, not just another filler, so it is worth a
/// warning.
const STALE_AUCTION_BLOCKS: u32 = 500;

/// The longest an unwind pass that keeps making no progress is held off
/// for, in ledgers. The backoff doubles from two, so this is reached on
/// the sixth consecutive setback and never exceeded.
///
/// It is a bound on cost, never on attempts: the pool stays pending
/// however long the backoff grows, because nothing else schedules the
/// position's unwind and a cause that is structural today (a reserve
/// pinned at `max_util`, a `min_collateral` a price move put the position
/// under) stops being structural the moment the chain moves. Sixty-four
/// ledgers is roughly five minutes — often enough that a transient cause
/// is retried while it still matters, rare enough that a permanent one
/// costs a snapshot and a simulation every five minutes rather than every
/// five seconds.
pub const UNWIND_BACKOFF_MAX_LEDGERS: u32 = 64;

/// How many consecutive non-progressing unwind passes one pool takes
/// before the operator is told.
///
/// Exactly this many, not at least: the notification fires on the pass
/// whose count *reaches* it, so one episode raises one alert however long
/// it then goes on. [`Notifier`]'s cooldown is the second guard; the count
/// is the first. Three is the smallest number that is not a coincidence —
/// one refusal is ordinary (the state a plan was built against moved), two
/// can be the same ledger's bad luck seen twice, and three means the cause
/// has outlived six ledgers of backoff.
pub const UNWIND_SETBACK_ALERT: u32 = 3;

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
    /// Every `(auction, reason)` this process has already counted a
    /// `skips_total` for: the auction by pool, account and start ledger,
    /// and the reason by its label.
    ///
    /// The start ledger is the *chain entry's* for every skip decided
    /// after the entry was read, and the row's only for the one decided
    /// before it ([`SkipLabel::UnsupportedAssets`], where no entry has
    /// been read). The chain can hold a new auction for an account before
    /// the tracker has applied the events that opened it, and `kept` in
    /// [`FillerState::prune_recorded`] keeps only keys at or past the
    /// row's start ledger: a post-read skip keyed by the older row would
    /// be pruned the moment the tracker caught up — while the auction is
    /// still open — and the very same decision would count again.
    ///
    /// A skip is counted once per auction per reason. The filler re-makes
    /// every one of these decisions on every tick an auction stays open —
    /// the assets test runs before the dry-run and `due` filters, and a
    /// planner skip clears the row's plan, which makes `due` true again —
    /// so a reason counted per attempt would count per ledger instead,
    /// for as long as the auction stays open, and one auction the planner
    /// refuses forever would bury every other reason in the metric. A
    /// *different* reason for the same auction counts again; the same one
    /// does not until the auction closes. Pruned beside
    /// `recorded_dry_run`, by the same rule and for the same reason: the
    /// row going away is the only thing that ends the count.
    counted_skips: BTreeSet<(String, String, u32, SkipLabel)>,
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
    /// The pools an unwind pass has still to find idle (rulings 3 and 4):
    /// a fill in one landed, or the run has not yet made its startup pass
    /// over it.
    unwind_pending: BTreeSet<String>,
    /// Whether the startup seed has been made. The seed is once per run,
    /// not once per tick: a pool an idle pass has cleared must stay
    /// cleared until a fill in it lands.
    unwind_seeded: bool,
    /// The pools an `UnwindLeftovers` notification is outstanding for
    /// (ruling 11). Cleared by the first later pass that finds the pool
    /// clean, which is what makes the next episode notify again.
    leftovers_notified: BTreeSet<String>,
    /// Per pool, the run of unwind passes that moved nothing and the
    /// ledger it is not planned again before. An entry exists only while
    /// such a run is open: a pass that landed, a pass that found the pool
    /// idle, and a pool that stops being pending all drop it, so the next
    /// episode starts at full cadence rather than inheriting the last
    /// one's backoff.
    unwind_setbacks: BTreeMap<String, Setback>,
    /// Per pool, the least ledger every later snapshot must reach before
    /// this pool's unwind is planned against it — `None` for a
    /// [`TxOutcome::Unknown`], whose ledger is not known and which
    /// therefore holds the pool indefinitely.
    ///
    /// A high-water mark, not a one-shot. Proving one snapshot holds the
    /// submission does not clear it: `latestLedger` is not monotonic
    /// across calls, and most of what a pass does after the gate can
    /// return having sent nothing while the pool stays pending, so the
    /// pass after that one can be served an older ledger by a lagging
    /// node and see the position as it stood before the submission. The
    /// entry is raised by the next submission that lands and dropped only
    /// where the pool itself is cleared — a pass that finds nothing left
    /// to unwind, and a run with no filler key to hold a position at all.
    ///
    /// Both a fill and an unwind write it, because both move the
    /// filler's own position, and a pass reads its own snapshot, which
    /// need not hold either: an `Unknown` has not been applied at all,
    /// and a `Succeeded` one has not if the RPC serving the snapshot is a
    /// ledger or two behind the one that confirmed it. Planning against a
    /// snapshot without it is wrong in both directions — it clears a pool
    /// whose position has not arrived yet, stranding the lot and the debt
    /// with nothing to schedule a pass again but a restart or another
    /// landed fill; and, where the filler already held a position, it
    /// re-plans the withdrawal or the repay the outstanding submission is
    /// about to make, which the contract's own caps then apply to what is
    /// left rather than to what was read — taking the primary collateral
    /// under the floor [`crate::math::unwind::plan_unwind`] promises never
    /// to cross.
    ///
    /// So the pass is held on the evidence rather than on the shape of
    /// what it read: a pool whose entry is `None`, or whose snapshot is
    /// older than the ledger the entry names, is left pending and
    /// untouched. This lives here rather than on [`Pass`] because a
    /// submission that landed at one tick is not a fact about that tick:
    /// the snapshot two ticks later can still be behind it. An `Unknown`
    /// reaches the filler only at shutdown — [`crate::queue`] resolves
    /// every other one to a terminal outcome before it takes the next —
    /// so a pool held on one is a pool held until the next run.
    unwind_after: BTreeMap<String, Option<u32>>,
}

/// One pool's open run of unwind passes that moved nothing.
#[derive(Debug, Default, Clone, Copy)]
struct Setback {
    /// Consecutive passes that neither landed nor found the pool idle.
    count: u32,
    /// The first tick this pool's pass may be planned at again. The pool
    /// stays pending throughout: this delays the pass, it never abandons
    /// it.
    retry_at: u32,
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
        let kept = |recorded_pool: &str, account: &str, start_ledger: u32| {
            recorded_pool != pool
                || open
                    .get(account)
                    .is_some_and(|start| start_ledger >= *start)
        };
        self.recorded_dry_run
            .retain(|recorded| kept(&recorded.pool, &recorded.account, recorded.start_ledger));
        // The same rule, because it answers the same question: an auction
        // the pool's open rows no longer name is one nothing will decide
        // about again, so neither set may keep it.
        self.counted_skips
            .retain(|(recorded_pool, account, start_ledger, _reason)| {
                kept(recorded_pool, account, *start_ledger)
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
    /// An auction nothing was done about *by decision*: a planner skip, a
    /// refusal, a stale plan, a re-plan that could not be drafted, or a
    /// wallet that could not fund the spend. A chain read that simply
    /// failed is not counted here — it is not a decision, and the next
    /// tick reads it again.
    ///
    /// The unwind pass reaches this for one of those alone: a wallet that
    /// could not fund its repays. A pass the contract refused, whose
    /// submission did not land, or whose plan went stale is counted
    /// nowhere — it is tracked per pool instead, in [`FillerState`]'s
    /// `unwind_setbacks`, because what matters about those is how many in
    /// a row a single pool has had rather than how many happened this
    /// tick.
    pub skipped: u32,
    /// Rows closed because the chain no longer holds their auction.
    pub closed: u32,
    /// Unwind passes that submitted a plan that moves something or, in
    /// dry-run, planned one. A pass that found nothing to move, was held
    /// back by the startup delay, was refused, or could not be read or
    /// planned is not counted: nothing of it reached the chain.
    pub unwound: u32,
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

/// The `skips_total` label one planner refusal is counted under.
///
/// Exhaustive on purpose — a new [`FillSkip`] must be given a label here
/// rather than silently joining whichever one a catch-all arm named.
/// [`FillSkip::TooManyPositions`] is `Health` because what it refuses is
/// the filler's own position, exactly as the floor does.
fn skip_label(reason: FillSkip) -> SkipLabel {
    match reason {
        FillSkip::Unprofitable => SkipLabel::Unprofitable,
        FillSkip::TooManyPositions | FillSkip::Health => SkipLabel::Health,
        FillSkip::Unfunded => SkipLabel::Unfunded,
        FillSkip::SupplyCapped => SkipLabel::SupplyCapped,
    }
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
///
/// Not `Debug`: [`Metrics`] is not, the same reason
/// [`crate::ledger::LedgerPoller`] stopped being once it took one.
pub struct Filler<'a> {
    rpc: &'a RpcClient,
    store: &'a Store,
    pools: &'a [PoolConfig],
    config: FillerConfig,
    executor: Executor<'a>,
    inventory: Inventory,
    notifier: Arc<Notifier>,
    /// The run's counters and gauges. Instrumenting only: every call
    /// through it is a lock and an integer, and nothing it answers is
    /// read back by anything that plans, simulates or sends (spec §8).
    metrics: Arc<Metrics>,
}

impl<'a> Filler<'a> {
    /// A filler reading `pools` through `rpc`, planning against `store`
    /// and `inventory`, executing through `executor`, reporting through
    /// `notifier` and counting into `metrics`.
    #[must_use]
    // Every one of these is a distinct collaborator with no sensible
    // default, and the two instruments are the run's single instances
    // rather than anything this type could build for itself.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc: &'a RpcClient,
        store: &'a Store,
        pools: &'a [PoolConfig],
        config: FillerConfig,
        executor: Executor<'a>,
        inventory: Inventory,
        notifier: Arc<Notifier>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            rpc,
            store,
            pools,
            config,
            executor,
            inventory,
            notifier,
            metrics,
        }
    }

    /// One tick, per configured pool, in the six steps the module doc sets
    /// out: keep the rows worth reading, re-read each one's auction entry,
    /// read one snapshot for the pool, plan every live entry against it,
    /// execute the ones whose ledger has come when `execute` says it may,
    /// and carry on past anything but a store failure. Then, after the
    /// whole walk, one unwind pass per pending pool — the seventh step the
    /// second `impl` block below documents. The startup seed is taken here
    /// rather than in [`Filler::new`] so a [`FillerState`] the caller built
    /// itself carries it too.
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
        if !state.unwind_seeded {
            state.unwind_seeded = true;
            state
                .unwind_pending
                .extend(self.pools.iter().map(|pool| pool.address.clone()));
        }
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
        self.unwind_passes(&mut pass, execute, queue, shutdown)
            .await?;
        // Once per tick, after the unwind passes: a gauge, not an
        // accumulator, so what it reports is what this whole tick left
        // held rather than what any one plan took mid-walk.
        self.metrics.reserved_inventory(&self.inventory.reserved());
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
        self.metrics.auctions_open(&pool.address, rows.len());
        pass.state.prune_recorded(&pool.address, &rows);
        let candidates: Vec<TrackedAuction> = rows
            .into_iter()
            .filter(|row| self.considered(pool, row, &mut *pass))
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
        // The filler's own account, and every live auction's borrower: a
        // full fill runs the contract's default path over the borrower
        // inside the filler's own transaction, and `plan_fill` projects
        // what that does to the reserves it then values the filler
        // against. `PoolReader::snapshot` answers for every account it is
        // handed, so this widens the `getLedgerEntries` it was already
        // making rather than adding a round trip.
        let mut accounts: Vec<&str> = self.executor.filler().into_iter().collect();
        accounts.extend(live.iter().map(|(row, _)| row.account.as_str()));
        accounts.sort_unstable();
        accounts.dedup();
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

    /// Counts one `skips_total{reason}` for this auction, once.
    ///
    /// Every skip the filler records goes through here, because every one
    /// of them is a decision it re-makes on every tick the auction stays
    /// open: counting per attempt would make each reason's rate a
    /// function of how long an auction lived rather than of how often the
    /// bot declined one, and the five reasons would stop being comparable
    /// with each other. See `FillerState::counted_skips` for what the
    /// key is and when it is forgotten.
    ///
    /// `start_ledger` is the auction this decision was about, and the
    /// caller says which: the chain entry's `block` for every skip decided
    /// after the entry was read, and `row.start_ledger` only for one
    /// decided before it. Passing the row's for a post-read skip is the
    /// bug — the row can lag the chain by an auction, and the key would be
    /// pruned out from under an auction that is still open.
    fn count_skip(
        &self,
        pass: &mut Pass<'_>,
        row: &TrackedAuction,
        start_ledger: u32,
        reason: SkipLabel,
    ) {
        if pass.state.counted_skips.insert((
            row.pool.clone(),
            row.account.clone(),
            start_ledger,
            reason,
        )) {
            self.metrics.skip(reason);
        }
    }

    /// Step 1: whether this row is worth a chain read at all.
    ///
    /// Silent by design — it runs for every open auction of every pool on
    /// every tick, and a line per row per ledger would bury the decisions
    /// that matter. What it filtered out is the difference between the
    /// store's open auctions and the tick's summary.
    fn considered(&self, pool: &PoolConfig, row: &TrackedAuction, pass: &mut Pass<'_>) -> bool {
        if row.auction_type != AuctionType::UserLiquidation {
            return false;
        }
        if self.config.own_addresses.contains(&row.account) {
            return false;
        }
        let bid: Vec<&str> = row.bid.keys().map(String::as_str).collect();
        let lot: Vec<&str> = row.lot.keys().map(String::as_str).collect();
        if !pool.supports(&bid, &lot) {
            // The one pre-read skip: nothing has been read, so the row
            // is all there is to key it by.
            self.count_skip(pass, row, row.start_ledger, SkipLabel::UnsupportedAssets);
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
            objective: context.pool.fill_objective,
            plan_iterations: self.config.plan_iterations,
        }
    }

    /// One plan against this pool's snapshot and the wallet as it stands
    /// now — which a reservation taken earlier in this same tick has
    /// already reduced.
    fn plan(
        &self,
        context: &PoolPass<'_>,
        account: &str,
        auction: &AuctionData,
        max_percent: FillPercent,
    ) -> Result<PlannedFill, MathError> {
        let wallet = self.inventory.available();
        // A borrower the snapshot holds no entry for has no position to
        // default, so an empty one is the truthful reading rather than a
        // refusal: the snapshot was asked for this account, and "no
        // entry" is how the ledger spells a position that holds nothing.
        let empty = Positions::default();
        let borrower = context.snapshot.positions.get(account).unwrap_or(&empty);
        let inputs = FillInputs {
            reserves: &context.reserves,
            asset_index: &context.snapshot.asset_index,
            prices: &context.snapshot.prices,
            filler: &context.filler,
            borrower,
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
        match self.plan(context, &row.account, auction, max_percent) {
            Ok(PlannedFill::Fill(draft)) => Ok(Some(draft)),
            Ok(PlannedFill::Skip(reason)) => {
                tracing::debug!(
                    pool = %row.pool,
                    account = %row.account,
                    reason = ?reason,
                    max_percent = max_percent.get(),
                    "no fill planned for this auction"
                );
                self.count_skip(pass, row, auction.block, skip_label(reason));
                // The one planner refusal an operator can do something
                // about: every other one is the auction's own shape, and
                // this one is the wallet's.
                if matches!(reason, FillSkip::Unfunded) {
                    self.notifier.notify(Notification {
                        kind: NotificationKind::UnfundedFill,
                        severity: Severity::Medium,
                        pool: row.pool.clone(),
                        account: Some(row.account.clone()),
                        message: "the wallet cannot fund the primary asset a fill of this \
                                  auction needs"
                            .to_string(),
                    });
                }
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
        let Some(outcome) = self
            .execute_once(context, row, auction.block, draft, queue, pass)
            .await?
        else {
            return Ok(false);
        };
        match outcome {
            ExecOutcome::Recorded(recorded) => {
                Ok(self.note_recorded(row, auction, &recorded, draft.est_profit, pass))
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
                self.count_skip(pass, row, auction.block, SkipLabel::ContractError);
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
        let Some(outcome) = self
            .execute_once(context, row, auction.block, &draft, queue, pass)
            .await?
        else {
            return Ok(false);
        };
        Ok(match outcome {
            ExecOutcome::Recorded(recorded) => {
                self.note_recorded(row, auction, &recorded, draft.est_profit, pass)
            }
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
                self.count_skip(pass, row, auction.block, SkipLabel::ContractError);
                pass.summary.skipped += 1;
                false
            }
        })
    }

    /// Hands one draft to the executor with the settlement its mode
    /// demands. `None` means nothing was executed and the reason is
    /// already counted or logged.
    ///
    /// `start_ledger` is the chain entry's `block` — the auction this
    /// draft was planned against — because both skips counted here are
    /// decided after the entry was read. See [`Filler::count_skip`].
    async fn execute_once(
        &self,
        context: &PoolPass<'_>,
        row: &TrackedAuction,
        start_ledger: u32,
        draft: &FillDraft,
        queue: Option<&SubmissionQueue>,
        pass: &mut Pass<'_>,
    ) -> Result<Option<ExecOutcome>, FillerError> {
        // From 500 on, `delete_stale_auction` stops refusing. It is
        // permissionless and deletes nothing by itself, so this is not a
        // refusal — but a plan aimed past it is racing anyone willing to
        // spend a transaction removing the auction, not just another
        // filler.
        if draft.fill_ledger.saturating_sub(start_ledger) >= STALE_AUCTION_BLOCKS {
            tracing::warn!(
                pool = %row.pool,
                account = %row.account,
                fill_ledger = draft.fill_ledger,
                start_ledger,
                "this auction is old enough for anyone to delete; the fill races a deletion"
            );
        }
        let priority = match self.priority(context, draft) {
            Ok(priority) => priority,
            Err(error) => {
                tracing::warn!(pool = %row.pool, account = %row.account, %error, "this fill's fee tier does not compute");
                // Nothing chain-specific refused this fill — it never
                // reached the chain — but `ContractError` is the label for
                // exactly this: a refusal none of the other four reasons
                // classifies more specifically.
                self.count_skip(pass, row, start_ledger, SkipLabel::ContractError);
                pass.summary.skipped += 1;
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
                    // A wallet shortfall, which is what `unfunded` means:
                    // the reservation is refused because what this plan
                    // means to spend is more than the inventory has left
                    // unreserved, never because the chain refused
                    // anything.
                    self.count_skip(pass, row, start_ledger, SkipLabel::Unfunded);
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
            // The `fills` row was written before the submission reached
            // the queue, so this attempt happened and it is gone rather
            // than pending: [`QueueError::Chain`] is narrowed to a failure
            // that provably sent nothing and has spent its retry budget.
            // Attempted *and* failed, which is what tells it from a
            // `TxOutcome::Unknown` — the one answer that is neither.
            Err(ExecutorError::Queue(QueueError::Chain(error))) => {
                tracing::warn!(
                    pool = %row.pool,
                    account = %row.account,
                    %error,
                    "the queue could not carry this fill; leaving it for the next tick"
                );
                self.metrics.fill(Attempt::Attempted);
                self.metrics.fill(Attempt::Failed);
                self.notifier.notify(Notification {
                    kind: NotificationKind::SubmissionDropped,
                    severity: Severity::High,
                    pool: row.pool.clone(),
                    account: Some(row.account.clone()),
                    message: format!("fill dropped by the queue: {error}"),
                });
                Ok(None)
            }
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

    /// What a recorded fill leaves behind: ruling 8's "recorded once" for
    /// a dry run, a wallet to re-read when the chain may have spent it
    /// (ruling 14 — an `Unknown` may still land), and this fill's place in
    /// the counters.
    ///
    /// Answers whether the chain applied this fill or may yet, which is
    /// both why the wallet is re-read and why the rest of this pool's
    /// auctions are left for the next tick: they were projected against
    /// the borrower's positions and the filler's own as this fill has just
    /// changed them.
    ///
    /// Every recorded fill is `attempted`, settled or not: a dry run's,
    /// one no key could sign, one the contract failed. What tells them
    /// apart is the chain's own answer — a [`TxOutcome::Failed`] (a fee
    /// charged and no fill) and a [`TxOutcome::Expired`] (provably never
    /// applied) are `failed`, while a [`TxOutcome::Unknown`] is neither:
    /// it may still land, and a counter that guessed would have to be
    /// un-counted. `est_profit` is the draft's own estimate, in the pool
    /// oracle's units, added to the display-only running total when — and
    /// only when — the chain says the fill landed.
    ///
    /// Only an [`ExecOutcome::Recorded`] reaches here, which is why
    /// `count(*) FROM fills` can exceed `fills_total{attempted}`: the
    /// executor writes the audit row before it enqueues anything, so a
    /// fill the *queue's* prepare then refused ([`ExecOutcome::Refused`],
    /// counted `skips_total{contract_error}`) or found stale
    /// ([`ExecOutcome::Stale`], counted under no label at all) leaves a
    /// row this never counts. The row is the record that the bot meant to
    /// fill; the counter is the record that it handed one to the chain.
    ///
    /// Instrumenting only: [`Notifier::notify`] spawns its delivery rather
    /// than awaiting a channel, so nothing here can delay or fail the pass
    /// that called it (spec §8).
    fn note_recorded(
        &self,
        row: &TrackedAuction,
        auction: &AuctionData,
        recorded: &FillRecorded,
        est_profit: i128,
        pass: &mut Pass<'_>,
    ) -> bool {
        pass.summary.executed += 1;
        self.metrics.fill(Attempt::Attempted);
        if recorded.dry_run {
            pass.state
                .recorded_dry_run
                .insert(RecordedFill::of_entry(row, auction));
        }
        match &recorded.submission {
            Some(TxOutcome::Succeeded { ledger, .. }) => {
                self.metrics.fill(Attempt::Succeeded);
                self.metrics.profit(est_profit);
                self.notifier.notify(Notification {
                    kind: NotificationKind::FillConfirmed,
                    severity: Severity::Low,
                    pool: row.pool.clone(),
                    account: Some(row.account.clone()),
                    message: format!("fill landed in ledger {ledger}"),
                });
            }
            Some(TxOutcome::Failed { ledger, .. }) => {
                self.metrics.fill(Attempt::Failed);
                self.notifier.notify(Notification {
                    kind: NotificationKind::FillFailed,
                    severity: Severity::High,
                    pool: row.pool.clone(),
                    account: Some(row.account.clone()),
                    message: format!("fill failed on chain in ledger {ledger}"),
                });
            }
            // Expired is the fee-less half of the same fact: it provably
            // never applied, so it is failed and there is no ledger to
            // name it in.
            Some(TxOutcome::Expired { .. }) => self.metrics.fill(Attempt::Failed),
            Some(TxOutcome::Unknown { .. }) | None => {}
        }
        // The ledger an `Unknown` will land in is not known, which is
        // exactly why it carries `None` rather than the window's own
        // bounds: a pass may not act on a snapshot until the fill is
        // provably in it.
        let landed = match recorded.submission {
            Some(TxOutcome::Succeeded { ledger, .. }) => Some(Some(ledger)),
            Some(TxOutcome::Unknown { .. }) => Some(None),
            _ => None,
        };
        if let Some(at) = landed {
            pass.state.inventory_stale = true;
            // Ruling 3: the fill handed this pool's position to the filler,
            // so an unwind pass is owed one — even for an `Unknown`
            // outcome, which may yet land. `unwind_after` is what keeps
            // every pass until one of them, this tick's included, from
            // planning against a snapshot that does not hold the fill.
            pass.state.unwind_pending.insert(row.pool.clone());
            pass.state.unwind_after.insert(row.pool.clone(), at);
            // And whatever the last passes could not do, they were refused
            // against a position this fill has changed — new collateral,
            // new debt. A backoff measured against the old one would hold
            // the new one unwound for as long as
            // `UNWIND_BACKOFF_MAX_LEDGERS`, which is exactly what a fill
            // must never be able to buy.
            clear_setback(&row.pool, pass);
        }
        landed.is_some()
    }
}

/// The unwind pass (spec §5, "Unwind"): the tick's seventh step, run in
/// this same task once the fill walk has finished.
///
/// **Ruling 2 — an unwind is not a queued operation.** Spec §8 says an
/// `Unwind { pool }` "is queued behind pending fills". An operation built
/// and queued behind a fill would be planned from the very state that fill
/// is about to change, which is the one thing the executor's own rule for
/// fills forbids. Planning here, after the tick's fills, and submitting on
/// the same per-key [`SubmissionQueue`] puts the unwind behind them by the
/// queue's own ordering — which is what "behind" is for — and lets it share
/// the filler's wallet, its reservations and its snapshot machinery.
///
/// **Ruling 3 — what makes a pool pending.** A fill in it landed, or may
/// have ([`Filler::note_recorded`]); or this is the run's first tick,
/// which seeds every configured pool once. A restart between a fill and
/// its unwind must not strand the position, and an idle pass costs one
/// snapshot and one wallet read and sends nothing. Because spec §5's step 2 withdraws the
/// primary down to `min_primary_collateral`, that startup pass also trims
/// any primary collateral above the floor to the wallet — the capital model
/// of spec §1, "unwind to the wallet and hold".
///
/// **Ruling 4 — it repeats while it moves something.** A pass that
/// submitted leaves its pool pending and the next tick plans it again from
/// a fresh snapshot; the first pass that builds no requests clears it. A
/// refusal, a failed or expired submission, a stale plan, a read that could
/// not be made and a plan that would not compute all leave it pending too:
/// none of them is a reason to believe the position is unwound.
///
/// **Ruling 9 — the startup delay gates the submission, not the plan.**
/// Inside it the pass still reads, still plans and still says at debug what
/// it would do, and sends nothing.
///
/// **Ruling 10 — the repays reserve as a fill's do.** A live pass takes a
/// [`Settlement::Live`] reservation for the plan's `spend` before anything
/// is simulated, and the executor settles it by what the chain did;
/// dry-run carries [`Settlement::DryRun`] and reserves nothing.
///
/// **Ruling 11 — leftovers notify once per pool.** An idle pass that leaves
/// debt the wallet cannot repay sends one
/// [`NotificationKind::UnwindLeftovers`] at [`Severity::High`], and not
/// again until a later pass finds the pool clean and a subsequent one finds
/// leftovers anew. [`Notifier`]'s own cooldown is the second guard, not
/// this one.
///
/// **Ruling 14 — a submission that may have landed counts as landed.** The
/// wallet is marked stale and the pool stays pending, because planning the
/// next pass against a position the chain is about to change is exactly
/// what must not happen. The same uncertainty runs the other way, which is
/// what [`FillerState`]'s `unwind_after` is for: a pool a fill — or a pass
/// of this very kind — made pending is passed over entirely until a
/// snapshot provably holds that submission, its ledger at or past the one
/// the submission landed in, and never for an `Unknown`, which has landed
/// in no ledger yet. What the position then looks like is not evidence
/// either way, and neither is how many ticks have gone by.
///
/// **Ruling 15 — this lives here.** The pass shares the filler's inventory,
/// executor, wallet refresh and per-tick state, so it is an `impl Filler`
/// block rather than a module of its own; the pure builder it calls is
/// [`crate::math::unwind`]. Nothing here writes any store table: spec §4
/// lists no unwind row, and the executor's structured events are the record
/// (ruling 9 of the executor's own set).
impl Filler<'_> {
    /// Every pending pool's pass, in configured order, checking the
    /// shutdown flag between them.
    ///
    /// With no filler key there is no account to hold a position or a
    /// balance, so there is nothing to unwind at all: that is said once and
    /// the pending set is dropped rather than re-read every tick.
    ///
    /// # Errors
    ///
    /// [`FillerError::Store`] only, as the fill walk: one pool's chain
    /// read, plan, simulation or submission failing is that pool's, and it
    /// stays pending for the next tick.
    async fn unwind_passes(
        &self,
        pass: &mut Pass<'_>,
        execute: bool,
        queue: Option<&SubmissionQueue>,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<(), FillerError> {
        let Some(filler) = self.executor.filler() else {
            if !pass.state.unwind_pending.is_empty() {
                pass.state.unwind_pending.clear();
                pass.state.unwind_after.clear();
                tracing::debug!("no filler key: nothing to unwind");
            }
            return Ok(());
        };
        for pool in self.pools {
            // Between pools, never inside a submission: the fill walk's
            // rule, and for its reason — a transaction whose outcome this
            // bot never saw has still spent its key's sequence number.
            if *shutdown.borrow() {
                return Ok(());
            }
            if !pass.state.unwind_pending.contains(&pool.address) {
                continue;
            }
            // Backing off keeps the pool pending: a pass that made no
            // progress three times running is very likely to make none on
            // the next ledger either, and a snapshot and a simulation per
            // tick is what that costs.
            if let Some(setback) = pass.state.unwind_setbacks.get(&pool.address) {
                if setback.retry_at > pass.tick.sequence {
                    tracing::debug!(
                        pool = %pool.address,
                        setbacks = setback.count,
                        retry_at = setback.retry_at,
                        "this pool's unwind is backing off; it stays pending and is planned \
                         again at that ledger"
                    );
                    continue;
                }
            }
            self.unwind_pool(pool, filler, pass, execute, queue).await?;
        }
        Ok(())
    }

    /// One pool's pass: its own snapshot (ruling 14 — the fill walk's was
    /// read before the fill that changed the position), the filler's
    /// wallet, the plan, and what the plan's answer means for the pending
    /// set.
    async fn unwind_pool(
        &self,
        pool: &PoolConfig,
        filler: &str,
        pass: &mut Pass<'_>,
        execute: bool,
        queue: Option<&SubmissionQueue>,
    ) -> Result<(), FillerError> {
        let reader = PoolReader::new(self.rpc, &pool.address);
        let snapshot = match reader.snapshot(&[filler]).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    pool = %pool.address,
                    %error,
                    "could not read this pool for its unwind; it stays pending for the next tick"
                );
                return Ok(());
            }
        };
        // The pass happened the moment it had a pool to look at: a read
        // that failed is not a pass, and everything below this — the
        // submission gate, an idle position, a plan that would not build —
        // is a pass that ran and found nothing to do.
        self.metrics.unwind_pass();
        // Before the position is looked at at all: what this snapshot
        // shows of a pool a fill or an earlier pass has just changed means
        // nothing until the snapshot is known to hold that submission.
        //
        // The entry survives the snapshot that proves it. Most of what
        // follows can return having sent nothing while the pool stays
        // pending — reserves that will not accrue, a plan that cannot be
        // built, a wallet that no longer covers the repays, an executor
        // failure, a contract refusal — and `latestLedger` is not
        // monotonic across calls: `same_ledger` makes one snapshot's own
        // parts agree, nothing makes the next snapshot newer than the
        // last. A gate that disarmed on proof would let the pass after
        // any of those plan against an older ledger from a lagging node,
        // which is the pre-withdrawal position again. So it is a
        // high-water mark, raised by the next submission that lands and
        // dropped only when the pool is cleared.
        if let Some(landed) = pass.state.unwind_after.get(&pool.address).copied() {
            let holds_it = landed.is_some_and(|ledger| snapshot.ledger >= ledger);
            if !holds_it {
                tracing::debug!(
                    pool = %pool.address,
                    snapshot = snapshot.ledger,
                    landed,
                    "this snapshot does not hold the submission this pool's pass is owed; it \
                     stays pending for the next tick"
                );
                return Ok(());
            }
        }
        let positions = snapshot.positions.get(filler).cloned().unwrap_or_default();
        if nothing_to_unwind(&positions) {
            tracing::debug!(
                pool = %pool.address,
                "the filler holds no position in this pool; its unwind is done"
            );
            pass.state.unwind_pending.remove(&pool.address);
            pass.state.leftovers_notified.remove(&pool.address);
            // The pool is done, so there is no submission left for a
            // later snapshot to have to hold.
            pass.state.unwind_after.remove(&pool.address);
            clear_setback(&pool.address, pass);
            return Ok(());
        }
        // A landed fill has already set the stale flag, so this reads; an
        // ordinary startup pass reads only when the balances have aged out.
        self.refresh_inventory(&reader, &snapshot, pass.state).await;
        // The same clamp `position_data` applies, so the reserves a plan is
        // built against and the ones the position is valued with describe
        // one instant.
        let reserves = match snapshot.accrued_reserves(snapshot.valued_at(pass.tick.close_time)) {
            Ok(reserves) => reserves,
            Err(error) => {
                tracing::warn!(
                    pool = %pool.address,
                    %error,
                    "could not accrue this pool's reserves for its unwind; it stays pending"
                );
                return Ok(());
            }
        };
        let terms = UnwindTerms {
            primary_asset: pool.primary_asset.clone(),
            min_primary_collateral: pool.min_primary_collateral,
            min_health_factor: pool.min_health_factor,
            min_collateral: snapshot.instance.config.min_collateral,
        };
        let wallet = self.inventory.available();
        let inputs = UnwindInputs {
            reserves: &reserves,
            asset_index: &snapshot.asset_index,
            prices: &snapshot.prices,
            positions: &positions,
            wallet: &wallet,
        };
        let plan = match plan_unwind(&terms, &inputs) {
            Ok(plan) => plan,
            Err(error) => {
                tracing::warn!(
                    pool = %pool.address,
                    %error,
                    "this pool's unwind could not be planned; it stays pending"
                );
                return Ok(());
            }
        };
        if plan.is_idle() {
            self.note_idle(pool, &plan, pass);
            return Ok(());
        }
        if !execute {
            tracing::debug!(
                pool = %pool.address,
                actions = plan.actions.len(),
                spend = ?plan.spend,
                "the startup delay holds this unwind back; it stays pending and is planned \
                 again once the delay has passed"
            );
            return Ok(());
        }
        self.execute_unwind(pool, filler, &plan, queue, pass).await
    }

    /// An idle pass (ruling 4): the pool is unwound as far as it can be, so
    /// it stops being pending. Ruling 11's notification is here, because an
    /// idle pass is the only one that can tell leftover debt from debt the
    /// next pass will repay.
    fn note_idle(&self, pool: &PoolConfig, plan: &UnwindPlan, pass: &mut Pass<'_>) {
        pass.state.unwind_pending.remove(&pool.address);
        clear_setback(&pool.address, pass);
        if plan.remaining_liabilities.is_empty() {
            // Clean: the next episode of leftovers in this pool notifies
            // again rather than being suppressed by the last one's entry.
            pass.state.leftovers_notified.remove(&pool.address);
            tracing::debug!(
                pool = %pool.address,
                "this pool's unwind has nothing left to move"
            );
            return;
        }
        let assets = plan.remaining_liabilities.join(", ");
        if !pass.state.leftovers_notified.insert(pool.address.clone()) {
            tracing::debug!(
                pool = %pool.address,
                remaining_liabilities = %assets,
                "debt the wallet cannot repay still remains here; already notified"
            );
            return;
        }
        let delivery = self.notifier.notify(Notification {
            kind: NotificationKind::UnwindLeftovers,
            severity: Severity::High,
            pool: pool.address.clone(),
            account: None,
            message: format!(
                "debt the wallet cannot repay remains in {}: {assets}",
                pool.address
            ),
        });
        // A notification the notifier never handed to its channel rolls its
        // own dedup entry back, and this one must roll back with it:
        // suppressing the next pass on the strength of a send that never
        // happened would lose a high-severity alert until some later pass
        // found the pool clean.
        //
        // A delivery that fails *inside* the notifier's task is not this:
        // `notify` has already answered `Queued`, the notifier logs the
        // failure, writes the notification to the log itself and rolls its
        // own entry back, and this pool stays marked as notified until a
        // later pass finds it clean. The filler does not retry it — a
        // notification must never affect trading (spec §8), and an alert
        // the operator can read in the log is not worth a second pass's
        // worth of bookkeeping.
        if delivery == Delivery::Dropped {
            pass.state.leftovers_notified.remove(&pool.address);
        }
    }

    /// The settlement this mode demands (ruling 10) and one call to the
    /// executor, and what its answer means for the pending set and the
    /// pool's setback run.
    ///
    /// Four answers are setbacks — a refusal, a stale plan, a submission
    /// that did not land, and an executor failure that is not the store's
    /// — because each leaves the position exactly as it was; the pool
    /// stays pending and its next pass is backed off. A pass that landed,
    /// or that was planned and held, resets the run: the position moved,
    /// or nothing about the pass failed.
    ///
    /// # Errors
    ///
    /// [`FillerError::Store`] only. Every other executor failure is this
    /// pool's: it is logged and the pool stays pending.
    async fn execute_unwind(
        &self,
        pool: &PoolConfig,
        filler: &str,
        plan: &UnwindPlan,
        queue: Option<&SubmissionQueue>,
        pass: &mut Pass<'_>,
    ) -> Result<(), FillerError> {
        let settlement = if self.config.dry_run {
            Settlement::DryRun
        } else {
            match self.inventory.reserve(&plan.spend) {
                Ok(reservation) => Settlement::Live(reservation),
                Err(error) => {
                    tracing::warn!(
                        pool = %pool.address,
                        %error,
                        "the wallet cannot fund this unwind's repays; skipping it this tick"
                    );
                    pass.summary.skipped += 1;
                    return Ok(());
                }
            }
        };
        let outcome = match self
            .executor
            .unwind(&pool.address, plan, settlement, queue)
            .await
        {
            Ok(outcome) => outcome,
            Err(ExecutorError::Store(error)) => return Err(FillerError::Store(error)),
            Err(error) => {
                tracing::warn!(
                    pool = %pool.address,
                    %error,
                    "this unwind could not be executed; it stays pending"
                );
                self.note_setback(pool, filler, &format!("the executor failed: {error}"), pass);
                return Ok(());
            }
        };
        self.note_unwound(pool, filler, &outcome, pass);
        Ok(())
    }

    /// What one [`UnwindOutcome`] means for the pending set, the tick's
    /// counts and the pool's setback run — the four arms that are
    /// setbacks, and the two that end one.
    fn note_unwound(
        &self,
        pool: &PoolConfig,
        filler: &str,
        outcome: &UnwindOutcome,
        pass: &mut Pass<'_>,
    ) {
        match outcome {
            // Nothing was sent, so nothing moved: in dry-run, planning it
            // again on every tick would say the same thing at the same
            // cost, and the next landed fill makes the pool pending again.
            // An *armed* pass reaching here was handed no queue, which
            // `Service::run` never does — clearing the pool on that would
            // silently drop a position nothing else schedules a pass for.
            UnwindOutcome::Planned { simulated } if self.config.dry_run => {
                pass.state.unwind_pending.remove(&pool.address);
                clear_setback(&pool.address, pass);
                pass.summary.unwound += 1;
                tracing::debug!(
                    pool = %pool.address,
                    simulated,
                    "unwind planned and not sent"
                );
            }
            UnwindOutcome::Planned { simulated } => {
                clear_setback(&pool.address, pass);
                tracing::warn!(
                    pool = %pool.address,
                    simulated,
                    "this armed pass was given no queue to send its unwind on; it stays \
                     pending, and nothing will send it until one is"
                );
            }
            UnwindOutcome::Submitted(submitted) if outcome.landed() => {
                pass.state.inventory_stale = true;
                // The requests this pass sent moved the very position the
                // next one is planned from, and `landed()` is exactly the
                // two outcomes that may have applied them. An `Unknown`
                // carries no ledger, so it holds the pool until the next
                // run rather than until a ledger that can be named.
                let landed = match submitted {
                    TxOutcome::Succeeded { ledger, .. } => Some(*ledger),
                    _ => None,
                };
                pass.state.unwind_after.insert(pool.address.clone(), landed);
                clear_setback(&pool.address, pass);
                pass.summary.unwound += 1;
                tracing::info!(
                    pool = %pool.address,
                    landed,
                    "this unwind landed, or may have; the pool stays pending and is not \
                     planned again until a snapshot holds it"
                );
            }
            UnwindOutcome::Submitted(outcome) => {
                tracing::warn!(
                    pool = %pool.address,
                    outcome = ?outcome,
                    "this unwind did not land; it stays pending and is planned again from \
                     fresh state rather than resent"
                );
                self.note_setback(
                    pool,
                    filler,
                    &format!("the submission did not land: {}", outcome.status()),
                    pass,
                );
            }
            UnwindOutcome::Refused { contract_error } => {
                tracing::info!(
                    pool = %pool.address,
                    contract_error,
                    "the contract refused this unwind; it stays pending and is planned again \
                     from fresh state"
                );
                let cause = contract_error.map_or_else(
                    || "the contract refused it without a code".to_string(),
                    |code| format!("the contract refused it with error {code}"),
                );
                self.note_setback(pool, filler, &cause, pass);
            }
            UnwindOutcome::Stale => {
                tracing::warn!(
                    pool = %pool.address,
                    "another transaction took this key's sequence first; this unwind stays \
                     pending and is planned again from fresh state"
                );
                self.note_setback(
                    pool,
                    filler,
                    "stale: another transaction took this key's sequence first",
                    pass,
                );
            }
        }
    }

    /// One unwind pass that left the position exactly as it was: the
    /// pool's run of them grows, its next pass is held off for twice as
    /// long as the last (from two ledgers, capped at
    /// [`UNWIND_BACKOFF_MAX_LEDGERS`]), and the pass whose count *reaches*
    /// [`UNWIND_SETBACK_ALERT`] tells the operator once.
    ///
    /// Once, and only on that pass: an episode that goes on raises one
    /// alert, not one per pass. The filler keeps no second set for it —
    /// the count is the guard, and [`Notifier`]'s cooldown is the other —
    /// and a delivery that fails is the notifier's to log, because a
    /// notification must never affect trading (spec §8).
    fn note_setback(&self, pool: &PoolConfig, filler: &str, cause: &str, pass: &mut Pass<'_>) {
        let setback = pass
            .state
            .unwind_setbacks
            .entry(pool.address.clone())
            .or_default();
        setback.count = setback.count.saturating_add(1);
        // Two ledgers, then four, then eight: `saturating_pow` is what
        // makes a count no realistic run reaches arithmetically harmless.
        let backoff = 2_u32
            .saturating_pow(setback.count)
            .min(UNWIND_BACKOFF_MAX_LEDGERS);
        setback.retry_at = pass.tick.sequence.saturating_add(backoff);
        let Setback { count, retry_at } = *setback;
        tracing::warn!(
            pool = %pool.address,
            setbacks = count,
            retry_at,
            cause,
            "this unwind pass moved nothing; it stays pending and is backed off"
        );
        if count != UNWIND_SETBACK_ALERT {
            return;
        }
        self.notifier.notify(Notification {
            kind: NotificationKind::SubmissionDropped,
            severity: Severity::High,
            pool: pool.address.clone(),
            account: Some(filler.to_string()),
            message: format!(
                "{count} unwind passes in {} have moved nothing ({cause}); the position stays \
                 pending and the pass is backing off to every {backoff} ledgers",
                pool.address
            ),
        });
    }
}

/// Ends a pool's run of setbacks: its pass landed, found the pool idle, or
/// found nothing to unwind at all. The entry is dropped rather than zeroed
/// so a pool that is not backing off costs nothing to check, and so the
/// next episode of trouble starts at full cadence instead of inheriting
/// the last one's.
fn clear_setback(pool: &str, pass: &mut Pass<'_>) {
    pass.state.unwind_setbacks.remove(pool);
}

/// Whether an unwind could move anything at all here: something to
/// withdraw, or something to repay. `supply` is not collateral and no
/// request an unwind sends touches it, so a position of supply alone is
/// nothing for this pass to do.
fn nothing_to_unwind(positions: &Positions) -> bool {
    positions.collateral.is_empty() && positions.liabilities.is_empty()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::watch;

    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::{
        script_simulate_accepted, script_simulate_prelude, script_simulate_refused, scval_b64,
        ScriptedRpc,
    };
    use crate::chain::signer::Network;
    use crate::chain::tx::Submitter;
    use crate::chain::xdr::encode::{
        address, from_base64, i128_val, map, sc_address, symbol, vec as sc_vec,
    };
    use crate::chain::xdr::keys;
    use crate::chain::{ChainError, LedgerWindow, TxHash, TxOutcome};
    use crate::harness::{
        self, contract_entry_xdr, entry, filler_signer, instance_entry_xdr, positions_entry_xdr,
        script_empty_wallet, script_snapshot_positions, simulation, tx_config, BLND, POOL_TWO,
    };
    use crate::math::fill::{FillAction, FillObjective};
    use crate::math::unwind::UnwindAction;
    use crate::notifier::{NotificationChannel, NotifyError, NOTIFY_IN_FLIGHT};
    use stellar_xdr::{
        ScVal, TransactionResult, TransactionResultExt, TransactionResultResult, VecM,
    };

    /// A log-only notifier with a cooldown longer than any test's run:
    /// every test but `leftover_debt_notifies_once_per_pool` is about what
    /// the filler decides to notify about, not about what the [`Notifier`]
    /// then deduplicates.
    fn notifier() -> Arc<Notifier> {
        Arc::new(Notifier::log_only(Duration::from_hours(1)))
    }

    /// A fresh recorder. Most tests here build one and never read it:
    /// what they are about is what the filler decides, and the counters
    /// must cost that nothing — a filler given a recorder nobody reads
    /// behaves exactly as one whose recorder is asserted on.
    fn metrics() -> Arc<Metrics> {
        Arc::new(Metrics::new())
    }

    /// One `fills_total`/`skips_total` series' current value, read out of
    /// the rendered text so a test asserts on what an operator's
    /// dashboard would actually scrape.
    fn series(metrics: &Metrics, name: &str, label: &str, value: &str) -> u64 {
        let needle = format!("blend_liquidator_{name}{{{label}=\"{value}\"}} ");
        metrics
            .render()
            .lines()
            .find_map(|line| line.strip_prefix(&needle)?.parse().ok())
            .unwrap_or_else(|| panic!("no {name} series for {label}={value}"))
    }

    /// The `fills_total` count for one result.
    fn fill_count(metrics: &Metrics, result: Attempt) -> u64 {
        series(metrics, "fills_total", "result", result.as_str())
    }

    /// The `skips_total` count for one reason.
    fn skip_count(metrics: &Metrics, reason: SkipLabel) -> u64 {
        series(metrics, "skips_total", "reason", reason.as_str())
    }

    /// The rendered running estimated-profit total, in the pool oracle's
    /// own units — an integer, which is what the counter carries: display
    /// only, never a number anything here decides on.
    fn profit_total(metrics: &Metrics) -> i128 {
        metrics
            .render()
            .lines()
            .find_map(|line| {
                line.strip_prefix("blend_liquidator_estimated_profit_total ")?
                    .parse()
                    .ok()
            })
            .expect("the profit total is always rendered")
    }

    /// Every [`FillSkip`] has its own label, and the one pair that shares
    /// one shares it on purpose: a fill that would take the filler past
    /// `max_positions` is refused by the filler's own position exactly as
    /// the health floor refuses it.
    #[test]
    fn every_planner_skip_has_a_label() {
        assert_eq!(skip_label(FillSkip::Unprofitable), SkipLabel::Unprofitable);
        assert_eq!(skip_label(FillSkip::TooManyPositions), SkipLabel::Health);
        assert_eq!(skip_label(FillSkip::Health), SkipLabel::Health);
        assert_eq!(skip_label(FillSkip::Unfunded), SkipLabel::Unfunded);
        assert_eq!(skip_label(FillSkip::SupplyCapped), SkipLabel::SupplyCapped);
    }

    /// A skip is one per auction *per reason*. The filler re-makes every
    /// skip decision on every tick an auction stays open, so a reason
    /// counted per attempt would count per ledger instead and one auction
    /// the planner refuses forever would bury the other four. A
    /// *different* reason for the same auction is a different decision and
    /// counts again, and the auction leaving the pool's open rows is what
    /// ends the count.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_skip_counts_once_per_auction_per_reason(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let metrics = metrics();
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
            notifier(),
            Arc::clone(&metrics),
        );
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        let row = tracked(harness::USER_ONE, &auction);
        let mut state = FillerState::default();
        let mut pass = Pass {
            tick,
            state: &mut state,
            summary: TickSummary::default(),
        };

        filler.count_skip(&mut pass, &row, row.start_ledger, SkipLabel::Unprofitable);
        filler.count_skip(&mut pass, &row, row.start_ledger, SkipLabel::Unprofitable);
        filler.count_skip(&mut pass, &row, row.start_ledger, SkipLabel::Unprofitable);
        assert_eq!(
            skip_count(&metrics, SkipLabel::Unprofitable),
            1,
            "the same auction refused for the same reason on three passes is one skip"
        );

        filler.count_skip(&mut pass, &row, row.start_ledger, SkipLabel::Health);
        assert_eq!(
            skip_count(&metrics, SkipLabel::Health),
            1,
            "a different reason for the same auction is a different decision"
        );
        assert_eq!(
            skip_count(&metrics, SkipLabel::Unprofitable),
            1,
            "and counting it does not re-open the reason already counted"
        );

        // The row leaving the pool's open auctions is the only thing that
        // ends the count: a later auction for the same account is a
        // decision worth counting afresh.
        pass.state.prune_recorded(&row.pool, &[]);
        filler.count_skip(&mut pass, &row, row.start_ledger, SkipLabel::Unprofitable);
        assert_eq!(
            skip_count(&metrics, SkipLabel::Unprofitable),
            2,
            "an auction the pool no longer holds open is forgotten, reasons and all"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A skip decided *after* the chain read is keyed by the entry's own
    /// start ledger, never the row's.
    ///
    /// The chain can hold a new auction for an account before the tracker
    /// has applied the events that closed the old one and opened it, so
    /// the row is the older ledger while the entry the filler actually
    /// decided about is the newer. Keyed by the row's, the tracker's
    /// catch-up prunes the key — `prune_recorded` keeps only keys at or
    /// past the row's start ledger — while the auction is still open, and
    /// the very same decision counts a second `skips_total`.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_post_read_skip_is_keyed_by_the_chain_auction(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        // A lot worth nothing at the oracle's prices, so every plan of
        // either is `Unprofitable` — a planner skip, which is the first of
        // the post-read sites — independent of when each is read.
        let worthless = |block: u32| AuctionData {
            bid: BTreeMap::from([(USDC.to_string(), BID)]),
            lot: BTreeMap::from([(XLM.to_string(), 1)]),
            block,
        };
        let old = worthless(tick.sequence - 600);
        let new = worthless(tick.sequence - 500);
        store
            .upsert_auction(&tracked(harness::USER_ONE, &old))
            .await
            .expect("the store still holds the old auction");
        let rpc = ScriptedRpc::start().await;
        harness::script_auction_entry(&rpc, harness::USER_ONE, &new, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let metrics = metrics();
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
            notifier(),
            Arc::clone(&metrics),
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
                skipped: 1,
                ..TickSummary::default()
            },
            "the auction the chain holds has a worthless lot, and the planner says so"
        );
        assert_eq!(skip_count(&metrics, SkipLabel::Unprofitable), 1);

        // The tracker catches up: the row now names the auction the chain
        // has held all along, and the pool still holds it open.
        let second = later(tick, 1);
        store
            .upsert_auction(&TrackedAuction {
                updated_ledger: second.sequence,
                ..tracked(harness::USER_ONE, &new)
            })
            .await
            .expect("the tracker opens the new auction");
        harness::script_auction_entry(&rpc, harness::USER_ONE, &new, second.sequence);
        harness::script_snapshot(&rpc, &[]);
        let summary = filler
            .tick(&mut state, second, true, None, &shutdown)
            .await
            .expect("the second tick");
        assert_eq!(
            summary,
            TickSummary {
                skipped: 1,
                ..TickSummary::default()
            },
            "the decision is re-made, as it is on every tick the auction stays open"
        );
        assert_eq!(
            skip_count(&metrics, SkipLabel::Unprofitable),
            1,
            "and it is the same auction, so it is still one skip: the key survived the \
             tracker's catch-up because it was the entry's start ledger, not the row's"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The filler's position after a fill of the fixture's pool: b-tokens
    /// of the primary XLM reserve, and the USDC debt the fill took on.
    /// 5e12 b-tokens is 500,011 XLM of underlying at the fixture's own
    /// `b_rate`; 1e9 d-tokens of USDC is 100.16 USDC.
    const UNWIND_COLLATERAL: i128 = 5_000_000_000_000;
    /// `plan_unwind` repays it with 1_228_866_615 — the debt in underlying
    /// at the fixture's own `d_rate`, plus the planner's 1 bp allowance and
    /// one unit.
    const UNWIND_DEBT: i128 = 1_000_000_000;
    /// A wallet that covers `UNWIND_REPAY` many times over.
    const WALLET_USDC: i128 = 1_000_000_000_000;
    /// A primary-collateral floor for the pools that keep one: 100,000 XLM,
    /// which `UNWIND_COLLATERAL` is far above and 5e11 b-tokens (500,011
    /// XLM-stroops short of it) is under.
    const UNWIND_FLOOR: i128 = 1_000_000_000_000;

    /// A position the fixture pool's own `min_collateral` traps: 33.7 XLM
    /// of b-tokens against 1.84 USDC of d-tokens, which at the fixture's
    /// accrued rates and prices is `44_955_547` of effective collateral
    /// against `19_399_598` of effective liability — under the pool's
    /// `50_000_000` ($5.00) minimum while comfortably over the health
    /// margin's `29_244_894`. See
    /// `the_pools_min_collateral_binds_the_filler_too`.
    const MIN_COLLATERAL_TRAPPED: i128 = 337_000_000;
    const MIN_COLLATERAL_DEBT: i128 = 15_000_000;

    /// One inventory refresh answering `balances` for the pool's three
    /// reserves in the order the filler asks for them — XLM, USDC, EURC,
    /// which is their strkey order and also the whole asset set, the
    /// configured native asset being the fixture's own XLM.
    fn script_wallet(rpc: &ScriptedRpc, ledger: u32, balances: [i128; 3]) {
        for balance in balances {
            rpc.expect(
                "simulateTransaction",
                simulation(&scval_b64(&i128_val(balance)), ledger),
            );
        }
    }

    /// One snapshot of the fixture's pool in which `account` holds exactly
    /// these positions: what an unwind pass reads.
    fn script_unwind_position(
        rpc: &ScriptedRpc,
        account: &str,
        collateral: &[(u32, i128)],
        liabilities: &[(u32, i128)],
    ) {
        script_snapshot_positions(
            rpc,
            &[(
                account,
                positions_entry_xdr(account, collateral, liabilities),
            )],
        );
    }

    /// The same, reported at `ledger` rather than at the fixture's own:
    /// a snapshot the chain has already moved past, or one taken late
    /// enough to hold a submission an earlier tick sent.
    fn script_unwind_position_at(
        rpc: &ScriptedRpc,
        ledger: u32,
        account: &str,
        collateral: &[(u32, i128)],
        liabilities: &[(u32, i128)],
    ) {
        harness::script_snapshot_positions_at(
            rpc,
            ledger,
            &[(
                account,
                positions_entry_xdr(account, collateral, liabilities),
            )],
        );
    }

    /// A [`NotificationChannel`] that keeps what it was handed, so a test
    /// can count what the filler actually decided to send — and that can
    /// be made to hold every send (which is how a test takes all of the
    /// notifier's in-flight permits) or to fail the next one (which is how
    /// a transient channel failure is put in front of the filler's own
    /// dedup). Both are off by default.
    #[derive(Debug, Default)]
    struct Recorded {
        sent: std::sync::Mutex<Vec<Notification>>,
        fail_once: std::sync::atomic::AtomicBool,
        held: std::sync::atomic::AtomicBool,
        gate: tokio::sync::Notify,
    }

    impl Recorded {
        /// Holds every send from here on, so each one keeps its notifier
        /// permit until [`Recorded::release`].
        fn hold(&self) {
            self.held.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        /// Lets every held send finish, and every later one through.
        fn release(&self) {
            self.held.store(false, std::sync::atomic::Ordering::SeqCst);
            self.gate.notify_waiters();
        }

        /// Fails the next send, once.
        fn fail_next(&self) {
            self.fail_once
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn sent(&self) -> Vec<Notification> {
            self.sent.lock().expect("the recorder mutex").clone()
        }

        /// What it was handed of one kind, which is how the leftovers a
        /// test is counting are told from the sends holding its permits.
        fn sent_of(&self, kind: NotificationKind) -> Vec<Notification> {
            self.sent()
                .into_iter()
                .filter(|notification| notification.kind == kind)
                .collect()
        }
    }

    impl NotificationChannel for Arc<Recorded> {
        fn name(&self) -> &'static str {
            "recorded"
        }

        fn send<'a>(
            &'a self,
            notification: &'a Notification,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), NotifyError>> + Send + 'a>>
        {
            Box::pin(async move {
                loop {
                    // Registered *before* the flag is re-read, so a release
                    // that lands between the two is never missed and this
                    // send cannot hang a test.
                    let notified = self.gate.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if !self.held.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    notified.await;
                }
                if self
                    .fail_once
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    return Err(NotifyError::Channel("the channel is down".to_string()));
                }
                self.sent
                    .lock()
                    .expect("the recorder mutex")
                    .push(notification.clone());
                Ok(())
            })
        }
    }

    /// A stand-in queue worker that answers every submission `Succeeded`
    /// and records its label, so a test can say what reached the queue and
    /// in what order. What this proves is the filler's ordering, not the
    /// queue's sending.
    fn recording_worker(
        capacity: usize,
    ) -> (
        SubmissionQueue,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let labels = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&labels);
        let (queue, mut receiver) = SubmissionQueue::new(
            NonZeroUsize::new(capacity).expect("a test capacity is never zero"),
        );
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                recorder
                    .lock()
                    .expect("the recorder mutex")
                    .push(queued.submission.label.clone());
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash: TxHash([3_u8; 32]),
                    ledger: 1,
                    return_value: None,
                }));
            }
        });
        (queue, labels, worker)
    }

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
            fill_objective: FillObjective::EarliestProfitable,
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

    /// The filler's own starting position: 15.9 billion b-tokens of the
    /// fixture's third reserve (about $20,031 of effective collateral at
    /// its 0.95 factor) against 38.7 billion d-tokens of USDC (about
    /// $50,050 of effective liability). Chosen so the *whole* auction
    /// lifts it over the 1.65 floor and half of it does not — see
    /// `a_re_plan_for_a_later_ledger_is_written_and_left`.
    const FILLER_COLLATERAL: i128 = 15_900_000_000;
    const FILLER_LIABILITIES: i128 = 38_700_000_000;

    /// A filler so far under its own floor that nothing the auction pays
    /// can lift it: `FILLER_COLLATERAL` against ten times
    /// `FILLER_LIABILITIES`. Even the whole lot at the end of the ramp,
    /// where the bid has scaled to nothing, leaves it at 0.07 against a
    /// 1.65 floor — so no percent and no ledger drafts, and what is left
    /// to say about it is that more of the primary asset would have been
    /// the difference.
    const DROWNED_LIABILITIES: i128 = 387_000_000_000;

    /// The fixture's third reserve, which `POOL_TWO` does not list.
    const EURC: &str = "CDTKPWPLOURQA2SGTKTUQOWRCBZEORB4BWBOMJ3D3ZTQQSGE5F6JBQLV";

    /// One reserve of the synthetic second pool. Rates are 1.0, so no
    /// accrual moves them and every amount below is also its underlying.
    #[derive(Debug, Clone, Copy)]
    struct SyntheticReserve {
        asset: &'static str,
        c_factor: u32,
        l_factor: u32,
        price: i128,
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
        // Ruling 3 makes every configured pool unwind-pending on the first
        // tick, so the pass reads its own snapshot after the walk. This key
        // holds no position in the fixture, so it is idle and sends nothing.
        harness::script_snapshot(&rpc, &[]);
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
            notifier(),
            metrics(),
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
            6,
            "the auction entry, the snapshot's two reads, one source-account read, and the \
             unwind pass's own two: a second account read would mean the re-plan was \
             simulated, which is the first step of sending it"
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
        // Ruling 3 makes every configured pool unwind-pending on the first
        // tick, so the pass reads its own snapshot after the walk. This key
        // holds no position in the fixture, so it is idle and sends nothing.
        harness::script_snapshot(&rpc, &[]);
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
            notifier(),
            metrics(),
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
            notifier(),
            metrics(),
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
            notifier(),
            metrics(),
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
            notifier(),
            metrics(),
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
            notifier(),
            metrics(),
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
            notifier(),
            metrics(),
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
            notifier(),
            metrics(),
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
            notifier(),
            metrics(),
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
        let metrics = metrics();
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            config,
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
            notifier(),
            Arc::clone(&metrics),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("tick");
        // A second tick over the same two rows: nothing about either has
        // changed, and neither has what the bot decided about them.
        let next = LedgerTick {
            sequence: tick.sequence + 1,
            ..tick
        };
        let again = filler
            .tick(&mut state, next, true, None, &shutdown)
            .await
            .expect("a second tick");

        assert_eq!(summary, TickSummary::default());
        assert_eq!(again, TickSummary::default());
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
        assert!(
            metrics.render().contains(&format!(
                "blend_liquidator_auctions_open{{pool=\"{}\"}} 2\n",
                harness::POOL
            )),
            "the gauge is what the store holds open, not what survived the filter:\n{}",
            metrics.render()
        );
        assert_eq!(
            skip_count(&metrics, SkipLabel::UnsupportedAssets),
            1,
            "the unsupported auction is the one counted, once for the auction and not once \
             per tick it stays open for — every reason is counted that way, and one \
             counted at the tick rate would bury the rest. The bot's own account is \
             filtered before the assets are ever looked at"
        );
        assert_eq!(
            skip_count(&metrics, SkipLabel::Unfunded)
                + skip_count(&metrics, SkipLabel::ContractError),
            0
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
            notifier(),
            metrics(),
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
            notifier(),
            metrics(),
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
        // Ruling 3 makes every configured pool unwind-pending on the first
        // tick, so the pass reads its own snapshot after the walk. This key
        // holds no position in the fixture, so it is idle and sends nothing.
        harness::script_snapshot(&rpc, &[]);
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
            notifier(),
            metrics(),
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
        // Ruling 3 makes every configured pool unwind-pending on the first
        // tick, so the pass reads its own snapshot after the walk. This key
        // holds no position in the fixture, so it is idle and sends nothing.
        harness::script_snapshot(&rpc, &[]);
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
            notifier(),
            metrics(),
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
        // The fill landed this time, so the pool is pending again.
        harness::script_snapshot(&rpc, &[]);

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
            notifier(),
            metrics(),
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
        // Ruling 3 makes every configured pool unwind-pending on the first
        // tick, so the pass reads its own snapshot after the walk. This key
        // holds no position in the fixture, so it is idle and sends nothing.
        harness::script_snapshot(&rpc, &[]);
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
            notifier(),
            metrics(),
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
        let metrics = metrics();
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
            notifier(),
            Arc::clone(&metrics),
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
                auction.block,
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
        assert_eq!(
            skip_count(&metrics, SkipLabel::Unfunded),
            1,
            "a wallet that moved under the plan is a shortfall, and the label says so"
        );
        assert_eq!(
            skip_count(&metrics, SkipLabel::ContractError),
            0,
            "nothing was asked of the chain"
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

    /// The fee tier does not compute: `to_oracle_units` overflows on a
    /// `HIGH_FEE_PROFIT_THRESHOLD` this large, `priority` answers `Err`,
    /// and `execute_once` refuses before it ever asks the wallet or the
    /// chain anything.
    ///
    /// `to_oracle_units` is `mul_floor(value, oracle_scalar, SCALAR_7)`,
    /// and the fixture's own oracle is fixed at 7 decimals (`harness`'s
    /// module doc), which makes `oracle_scalar == SCALAR_7` and the
    /// conversion an identity no threshold can overflow — so this test
    /// builds `PoolPass` directly, with a 30-decimal oracle, rather than
    /// through `pool_context`/`PoolReader`: the only field `priority`
    /// reads off it is `snapshot.prices`, and nothing else here needs a
    /// chain read at all.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_fee_tier_that_overflows_skips_the_fill(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let auction = auction(tick.sequence - 300);
        // Never dialed: `execute_once` touches neither `self.rpc` nor the
        // network on this path.
        let client = RpcClient::new("http://127.0.0.1:1", None).expect("client");
        let inventory = Inventory::new(XLM.to_string(), 0);
        inventory.record_balances(
            BTreeMap::from([(USDC.to_string(), 10_000_000_000)]),
            Instant::now(),
        );
        let metrics = metrics();
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            FillerConfig {
                high_fee_profit_threshold: i128::MAX,
                ..filler_config()
            },
            Executor::new(&store, None, true),
            inventory,
            notifier(),
            Arc::clone(&metrics),
        );
        let snapshot = PoolSnapshot {
            ledger: tick.sequence,
            pool: harness::POOL.to_string(),
            instance: crate::chain::xdr::decode::PoolInstance {
                admin: String::new(),
                backstop: String::new(),
                blnd_token: String::new(),
                name: String::new(),
                config: crate::chain::xdr::decode::PoolConfig {
                    oracle: String::new(),
                    bstop_rate: 0,
                    status: PoolStatus::Active,
                    max_positions: 4,
                    min_collateral: 0,
                },
            },
            reserves: BTreeMap::new(),
            asset_index: BTreeMap::new(),
            prices: crate::math::OraclePrices::new(30, BTreeMap::new()).expect("30 decimals fit"),
            price_timestamps: BTreeMap::new(),
            positions: BTreeMap::new(),
        };
        let context = PoolPass {
            pool: &pools[0],
            earliest_ledger: tick.sequence + 1,
            snapshot,
            reserves: BTreeMap::new(),
            filler: Positions::default(),
            supply_allowed: false,
            health_floor: 0,
        };
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
                auction.block,
                &draft,
                None,
                &mut pass,
            )
            .await
            .expect("a fee tier that does not compute is not the tick's failure");

        assert!(outcome.is_none(), "nothing was executed");
        assert_eq!(
            pass.summary,
            TickSummary {
                skipped: 1,
                ..TickSummary::default()
            },
            "a refused fee tier is a decision, and it is counted as one"
        );
        assert_eq!(
            skip_count(&metrics, SkipLabel::ContractError),
            1,
            "nothing here classifies a math overflow more specifically"
        );
        assert_eq!(
            skip_count(&metrics, SkipLabel::Unfunded),
            0,
            "the wallet was never asked"
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            fills.n,
            Some(0),
            "nothing whose fee tier is unknown is recorded"
        );
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
            notifier(),
            metrics(),
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
            notifier(),
            metrics(),
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
        // auction and the entry is re-read — and found already recorded,
        // after the pool's snapshot, which the walk reads before it tests
        // any entry against the set. Scripted, so the pass reaches that
        // test rather than bailing on an unscripted read.
        let second = later(tick, 1);
        harness::script_auction_entry(&rpc, harness::USER_ONE, &new, second.sequence);
        harness::script_snapshot(&rpc, &[]);
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

    /// Ruling 3: a fill that landed makes its pool unwind-pending, and the
    /// pass that follows in the same tick repays the debt the fill took on
    /// and takes the collateral out. Ruling 2 is what the labels show: the
    /// unwind reaches the same per-key queue, behind the fill it follows.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_landed_fill_queues_an_unwind_of_its_pool(db: sqlx::PgPool) -> sqlx::Result<()> {
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
        // The fill walk: the entry, the pool as it stands before the fill —
        // this key holds nothing in it yet — and an empty wallet.
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_empty_wallet(&rpc, tick.sequence);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        // The unwind pass, on its own snapshot (ruling 14): the fill has
        // left this key the lot as collateral and the bid as debt, and the
        // wallet it is re-read from now holds the USDC to repay with.
        script_unwind_position(
            &rpc,
            signer.address(),
            &[(0, UNWIND_COLLATERAL)],
            &[(1, UNWIND_DEBT)],
        );
        script_wallet(&rpc, tick.sequence, [0, WALLET_USDC, 0]);
        script_simulate_prelude(&rpc, &signer, 11, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let recorder = Arc::new(Recorded::default());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recorder)),
            Duration::from_hours(1),
        ));
        let metrics = metrics();
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
            Arc::clone(&notifier),
            Arc::clone(&metrics),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let (queue, labels, worker) = recording_worker(4);

        let summary = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");
        assert!(notifier.drain(Duration::from_secs(5)).await);

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                executed: 1,
                unwound: 1,
                ..TickSummary::default()
            }
        );
        let labels = labels.lock().expect("the recorder mutex").clone();
        assert_eq!(labels.len(), 2, "the fill and then the unwind: {labels:?}");
        assert!(
            labels[0].starts_with("fill") && labels[1].starts_with("unwind"),
            "the unwind is queued behind the fill it follows: {labels:?}"
        );
        assert!(
            state.unwind_pending.contains(harness::POOL),
            "a submission that landed leaves the pool pending: the next tick plans it again \
             from a snapshot that holds it"
        );
        assert!(
            state.inventory_stale,
            "and the wallet it repaid from is re-read before anything else plans against it"
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(1), "an unwind writes no row of its own");
        assert_eq!(
            (
                fill_count(&metrics, Attempt::Attempted),
                fill_count(&metrics, Attempt::Succeeded),
                fill_count(&metrics, Attempt::Failed),
            ),
            (1, 1, 0),
            "the chain landed it, so it is attempted and succeeded and nothing else"
        );
        assert!(
            profit_total(&metrics) > 0,
            "and the draft's own estimate reached the running total"
        );
        assert!(
            metrics
                .render()
                .contains("blend_liquidator_unwind_passes_total 1\n"),
            "the unwind pass that followed it is counted once"
        );
        let confirmed = recorder.sent_of(NotificationKind::FillConfirmed);
        assert_eq!(confirmed.len(), 1, "{confirmed:?}");
        assert_eq!(confirmed[0].severity, Severity::Low);
        assert_eq!(confirmed[0].pool, harness::POOL);
        assert_eq!(
            confirmed[0].account.as_deref(),
            Some(harness::USER_ONE),
            "the borrower whose auction was filled, not the filler's own key"
        );
        assert!(
            confirmed[0].message.contains("ledger 1"),
            "the message names the ledger it landed in: {}",
            confirmed[0].message
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The chain applied the fill and it failed: a fee was charged and no
    /// position changed hands. That is `attempted` and `failed`, never
    /// `succeeded`, nothing reaches the profit total, and the operator is
    /// told at `High`.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_fill_the_chain_failed_is_counted_and_notified(db: sqlx::PgPool) -> sqlx::Result<()> {
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
        // Nothing landed, so nothing handed this key a position: the
        // startup pass reads the pool, finds none, and clears it.
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let recorder = Arc::new(Recorded::default());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recorder)),
            Duration::from_hours(1),
        ));
        let metrics = metrics();
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
            Arc::clone(&notifier),
            Arc::clone(&metrics),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        // `recording_worker`'s twin, and the one difference is the answer:
        // the chain applied this submission and it failed.
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Failed {
                    hash: TxHash([5_u8; 32]),
                    ledger: 9,
                    contract_error: Some(1_207),
                    result: TransactionResult {
                        fee_charged: 100,
                        result: TransactionResultResult::TxFailed(VecM::default()),
                        ext: TransactionResultExt::V0,
                    },
                }));
            }
        });

        let summary = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");
        assert!(notifier.drain(Duration::from_secs(5)).await);

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            },
            "a fill the chain refused is still a fill this bot recorded"
        );
        assert_eq!(
            (
                fill_count(&metrics, Attempt::Attempted),
                fill_count(&metrics, Attempt::Succeeded),
                fill_count(&metrics, Attempt::Failed),
            ),
            (1, 0, 1)
        );
        assert!(
            profit_total(&metrics) == 0,
            "a fill that took nothing over earned nothing"
        );
        assert!(
            !state.unwind_pending.contains(harness::POOL),
            "and it handed this key no position to unwind"
        );
        let failed = recorder.sent_of(NotificationKind::FillFailed);
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].severity, Severity::High);
        assert_eq!(failed[0].pool, harness::POOL);
        assert_eq!(failed[0].account.as_deref(), Some(harness::USER_ONE));
        assert!(
            failed[0].message.contains("ledger 9"),
            "the message names the ledger it failed in: {}",
            failed[0].message
        );
        assert!(
            recorder.sent_of(NotificationKind::FillConfirmed).is_empty(),
            "and nothing was confirmed"
        );
        let fill = sqlx::query!("SELECT tx_hash FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            fill.tx_hash,
            Some(TxHash([5_u8; 32]).to_hex()),
            "a transaction that consumed a sequence number is named whatever became of it"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The fee-less half of the same fact: the chain passed the ledger
    /// bound without applying the fill, which is `attempted` and `failed`
    /// exactly as a charged failure is — and notifies nobody. There is no
    /// ledger to name it in and nothing was spent, so spec §7's closed set
    /// of kinds has nothing to say about it; the counter is where an
    /// expired fill shows up.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_fill_that_expired_is_counted_and_notifies_nobody(
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
        // It provably never applied, so this key holds no position: the
        // startup pass reads the pool, finds none, and clears it.
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let recorder = Arc::new(Recorded::default());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recorder)),
            Duration::from_hours(1),
        ));
        let metrics = metrics();
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
            Arc::clone(&notifier),
            Arc::clone(&metrics),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Expired {
                    hash: TxHash([5_u8; 32]),
                    window: LedgerWindow::try_new(100, 120).expect("a window ends after it opens"),
                    latest_ledger: 121,
                }));
            }
        });

        let summary = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");
        assert!(notifier.drain(Duration::from_secs(5)).await);

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            },
            "a fill that never applied is still a fill this bot recorded"
        );
        assert_eq!(
            (
                fill_count(&metrics, Attempt::Attempted),
                fill_count(&metrics, Attempt::Succeeded),
                fill_count(&metrics, Attempt::Failed),
            ),
            (1, 0, 1)
        );
        assert!(
            profit_total(&metrics) == 0,
            "a fill that never applied earned nothing"
        );
        assert!(
            !state.unwind_pending.contains(harness::POOL),
            "and it handed this key no position to unwind"
        );
        assert!(
            recorder.sent().is_empty(),
            "an expired fill is counted, not announced: {:?}",
            recorder.sent()
        );
        let fill = sqlx::query!("SELECT tx_hash FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            fill.tx_hash,
            Some(TxHash([5_u8; 32]).to_hex()),
            "a transaction that consumed a sequence number is named whatever became of it"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A submission the queue could not carry at all: the `fills` row was
    /// written before it was handed over, so the attempt happened and it
    /// is gone rather than pending — `attempted` and `failed`, and an
    /// alert naming the account.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_fill_the_queue_dropped_is_counted_and_notified(
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
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let recorder = Arc::new(Recorded::default());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recorder)),
            Duration::from_hours(1),
        ));
        let metrics = metrics();
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
            Arc::clone(&notifier),
            Arc::clone(&metrics),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        // The one failure `QueueError::Chain` is narrowed to: it provably
        // sent nothing, and its retry budget is spent.
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued
                    .respond
                    .send(Err(QueueError::Chain(ChainError::Http(503))));
            }
        });

        let summary = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("a queue that could not carry one fill is not the tick's failure");
        drop(queue);
        worker.await.expect("the worker ends with the queue");
        assert!(notifier.drain(Duration::from_secs(5)).await);

        assert_eq!(
            summary,
            TickSummary {
                planned: 1,
                ..TickSummary::default()
            },
            "nothing was executed: the executor never got an outcome to record"
        );
        assert_eq!(
            (
                fill_count(&metrics, Attempt::Attempted),
                fill_count(&metrics, Attempt::Succeeded),
                fill_count(&metrics, Attempt::Failed),
            ),
            (1, 0, 1),
            "the row was written before the queue was asked, so the attempt happened"
        );
        let dropped = recorder.sent_of(NotificationKind::SubmissionDropped);
        assert_eq!(dropped.len(), 1, "{dropped:?}");
        assert_eq!(dropped[0].severity, Severity::High);
        assert_eq!(dropped[0].pool, harness::POOL);
        assert_eq!(dropped[0].account.as_deref(), Some(harness::USER_ONE));
        assert!(
            dropped[0].message.contains("dropped by the queue"),
            "{}",
            dropped[0].message
        );
        let fill = sqlx::query!("SELECT tx_hash FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(
            fill.tx_hash, None,
            "nothing was ever named: the envelope never left"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A planner skip the wallet is the cause of. The filler is so far
    /// under its own floor that no percent and no ledger of the ramp
    /// lifts it, and more of the primary asset is what would have — so
    /// the skip is `Unfunded`, it is counted as one, and the operator is
    /// told at `Medium`.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unfunded_plan_is_counted_and_notified(db: sqlx::PgPool) -> sqlx::Result<()> {
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
        script_snapshot_positions(
            &rpc,
            &[(
                signer.address(),
                positions_entry_xdr(
                    signer.address(),
                    &[(2, FILLER_COLLATERAL)],
                    &[(1, DROWNED_LIABILITIES)],
                ),
            )],
        );
        // The wallet holds nothing, which is exactly what the skip is
        // about: the supply the projection wanted was capped at zero.
        script_empty_wallet(&rpc, tick.sequence);
        // Ruling 3's startup pass, on its own snapshot: no position there,
        // so it is idle and sends nothing.
        harness::script_snapshot(&rpc, &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let recorder = Arc::new(Recorded::default());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recorder)),
            Duration::from_hours(1),
        ));
        let metrics = metrics();
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
            Arc::clone(&notifier),
            Arc::clone(&metrics),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("tick");
        assert!(notifier.drain(Duration::from_secs(5)).await);

        assert_eq!(
            summary,
            TickSummary {
                skipped: 1,
                ..TickSummary::default()
            },
            "nothing drafted, so nothing was planned onto the row and nothing simulated"
        );
        assert_eq!(skip_count(&metrics, SkipLabel::Unfunded), 1);
        assert_eq!(
            skip_count(&metrics, SkipLabel::Health) + skip_count(&metrics, SkipLabel::Unprofitable),
            0,
            "the wallet is the cause, and the label says so"
        );
        assert_eq!(
            row(&store, harness::USER_ONE)
                .await
                .expect("the row stays")
                .fill_ledger,
            None,
            "a skip clears the plan off the row"
        );
        let unfunded = recorder.sent_of(NotificationKind::UnfundedFill);
        assert_eq!(unfunded.len(), 1, "{unfunded:?}");
        assert_eq!(unfunded[0].severity, Severity::Medium);
        assert_eq!(unfunded[0].pool, harness::POOL);
        assert_eq!(unfunded[0].account.as_deref(), Some(harness::USER_ONE));
        assert!(
            unfunded[0].message.contains("cannot fund"),
            "{}",
            unfunded[0].message
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Both refusal arms are `contract_error`: the first draft's, which
    /// ends the auction's tick, and the re-plan's, which the contract has
    /// now disagreed with twice. Counted once between them, because a
    /// skip is one per auction per reason however many ticks re-make the
    /// same decision.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_refused_fill_and_a_refused_re_plan_are_contract_errors(
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
        // The first tick: a refusal no re-plan addresses, so it is the
        // whole of that auction's tick.
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_empty_wallet(&rpc, tick.sequence);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_refused(&rpc, 1_207, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        // The second: the health check refuses the whole auction, and the
        // contract refuses the half as well.
        let second = later(tick, 1);
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, second.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_simulate_prelude(&rpc, &signer, 10, second.sequence);
        script_simulate_refused(&rpc, 1_205, second.sequence);
        script_simulate_prelude(&rpc, &signer, 10, second.sequence);
        script_simulate_refused(&rpc, 1_207, second.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let metrics = metrics();
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
            notifier(),
            Arc::clone(&metrics),
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
                skipped: 1,
                ..TickSummary::default()
            }
        );
        assert_eq!(
            skip_count(&metrics, SkipLabel::ContractError),
            1,
            "a refusal the contract gave a code for is a contract error"
        );

        let second_summary = filler
            .tick(&mut state, second, true, None, &shutdown)
            .await
            .expect("the second tick");

        assert_eq!(
            second_summary,
            TickSummary {
                planned: 2,
                skipped: 1,
                ..TickSummary::default()
            },
            "the refused draft and the half, and one skip for the second refusal"
        );
        assert_eq!(
            skip_count(&metrics, SkipLabel::ContractError),
            1,
            "the same auction refused for the same reason is one skip, not one per tick it \
             stays open for: the re-plan's refusal is the first refusal's reason again"
        );
        assert_eq!(
            fill_count(&metrics, Attempt::Attempted),
            0,
            "nothing was ever recorded, so nothing was ever attempted"
        );
        let fills = sqlx::query!("SELECT count(*) AS n FROM fills")
            .fetch_one(store.pool())
            .await?;
        assert_eq!(fills.n, Some(0));
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The reserved-inventory gauge is recorded once a tick, after its
    /// unwind passes, and it is a snapshot rather than an accumulator: a
    /// reservation still open when the tick ends is what it shows, and the
    /// tick after it is settled shows what the wallet then holds instead.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_ticks_open_reservations_are_gauged(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        // No auctions and no filler key: the tick reads nothing at all, so
        // the gauge is the only thing it leaves behind.
        let inventory = Inventory::new(XLM.to_string(), 0);
        inventory.record_balances(
            BTreeMap::from([(USDC.to_string(), 10_000_000_000)]),
            Instant::now(),
        );
        let held = inventory
            .reserve(&BTreeMap::from([(USDC.to_string(), 4_000_000_000)]))
            .expect("the wallet covers it");
        let metrics = metrics();
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            inventory,
            notifier(),
            Arc::clone(&metrics),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("the first tick");

        assert!(
            metrics.render().contains(&format!(
                "blend_liquidator_reserved_inventory{{asset=\"{USDC}\"}} 4000000000\n"
            )),
            "a reservation the tick ended with is what the gauge reports:\n{}",
            metrics.render()
        );

        held.release();
        filler
            .tick(&mut state, later(tick, 1), true, None, &shutdown)
            .await
            .expect("the second tick");

        assert!(
            metrics.render().contains(&format!(
                "blend_liquidator_reserved_inventory{{asset=\"{USDC}\"}} 0\n"
            )),
            "and a settled one is replaced rather than left at its last value:\n{}",
            metrics.render()
        );
        assert!(
            rpc.received().await.is_empty(),
            "no key and no auctions: neither tick cost a chain read"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A fill the chain never confirmed makes its pool pending, and every
    /// pass it is owed is passed over entirely — the one in its own tick
    /// and the ones after it: an `Unknown` has landed in no ledger, so no
    /// snapshot can ever be shown to hold it. What a pass would have read
    /// — nothing at all here, the handed-over position a tick later — is
    /// not what decides that; the fill's own outcome is, and it does not
    /// improve with time.
    ///
    /// Clearing the pool would strand the lot and the debt the fill hands
    /// over two ledgers later, with nothing to schedule a pass again but a
    /// restart or another landed fill; planning against a snapshot that
    /// cannot be shown to hold it would size a withdrawal against
    /// liabilities the fill is about to raise. So the pool is held until
    /// the next run — which costs this one nothing, because the queue
    /// resolves every outcome it can and hands an `Unknown` back only at
    /// shutdown.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unresolved_fills_pool_is_held_until_the_next_run(
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
        // Tick one: the fill walk, then a pass whose snapshot still shows
        // this key holding nothing at all.
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_empty_wallet(&rpc, tick.sequence);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        // Tick two: the fill has been applied, and the snapshot shows the
        // position it handed over — which is still not evidence that it
        // landed. Nothing past that snapshot is scripted: an unscripted
        // read answers HTTP 500, so a pass that plans on the shape of what
        // it read shows up in the counts below.
        let second = later(tick, 1);
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
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
            notifier(),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        // A stand-in worker whose every answer is the one the queue gives
        // back only when it has run out of chances to resolve it.
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Unknown {
                    hash: TxHash([5_u8; 32]),
                    sequence: 11,
                    window: LedgerWindow::try_new(100, 120).expect("a window ends after it opens"),
                }));
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
                executed: 1,
                ..TickSummary::default()
            },
            "the fill was sent and its outcome is unresolved; the pass found nothing to move"
        );
        assert!(
            state.unwind_pending.contains(harness::POOL),
            "a pool whose fill landed in no ledger anyone can name is passed over, not \
             cleared by a snapshot that cannot be holding it"
        );

        // The tracker, applying the fill that closed the auction: the row
        // goes, so the second tick's walk reads nothing and the pass is all
        // that is left of it.
        store
            .delete_auction(
                harness::POOL,
                harness::USER_ONE,
                AuctionType::UserLiquidation,
            )
            .await
            .expect("close the auction");
        let reads = rpc.calls("getLedgerEntries").len();
        let simulations = rpc.calls("simulateTransaction").len();

        let summary = filler
            .tick(&mut state, second, true, Some(&queue), &shutdown)
            .await
            .expect("the second tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert_eq!(
            summary,
            TickSummary::default(),
            "a later tick is not evidence either: the fill is still in no ledger, so the \
             pass is still owed one that holds it"
        );
        assert!(
            state.unwind_pending.contains(harness::POOL),
            "and the pool stays pending, for the run that can read the outcome of it"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            reads + 2,
            "the snapshot's two entry reads, and nothing past the gate"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            simulations + 4,
            "its four oracle reads: no wallet read, and no judgment"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// What one [`fill_then_unwind`] run leaves behind.
    struct Unwound {
        summary: TickSummary,
        pending: bool,
        /// The run's whole `FillerState::unwind_after`, so a test can say
        /// both which ledger the pool is still owed a snapshot at and
        /// that it is owed one at all.
        unwind_after: BTreeMap<String, Option<u32>>,
        entry_reads: usize,
        simulations: usize,
    }

    /// One tick of a pool whose auction the filler fills and then owes an
    /// unwind pass: the fill lands `ahead` ledgers past the ledger every
    /// snapshot here is read at, and the filler *already holds a position*
    /// in the pool, so nothing about the position's shape can tell the
    /// pass whether the fill is in the snapshot it reads.
    ///
    /// Only the pass that can see the fill is scripted for anything past
    /// its own snapshot: an unscripted read answers HTTP 500, so a pass
    /// that plans when it should have waited is visible in the counts this
    /// returns.
    async fn fill_then_unwind(db: sqlx::PgPool, ahead: u32) -> Unwound {
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
        // The fill walk: the entry, the pool before the fill, an empty
        // wallet, and the judgment.
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_empty_wallet(&rpc, tick.sequence);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        // The unwind pass's own snapshot, in which this key already holds
        // a position of its own.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        if ahead == 0 {
            script_wallet(&rpc, tick.sequence, [0, 0, 0]);
            script_simulate_prelude(&rpc, &signer, 11, tick.sequence);
            script_simulate_accepted(&rpc, tick.sequence);
        }
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
            notifier(),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let landed = tick.sequence + ahead;
        // Both runs share one database, and `fills.tx_hash` is unique: the
        // offset is what keeps the second run's row from colliding with
        // the first's.
        let hash = TxHash([7 + u8::try_from(ahead).expect("a small offset"); 32]);
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash,
                    ledger: landed,
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

        assert_eq!(rpc.remaining(), 0);
        Unwound {
            summary,
            pending: state.unwind_pending.contains(harness::POOL),
            unwind_after: state.unwind_after.clone(),
            entry_reads: rpc.calls("getLedgerEntries").len(),
            simulations: rpc.calls("simulateTransaction").len(),
        }
    }

    /// The ledger gate, in the case the position's shape cannot decide:
    /// the filler already holds a position in the pool, so an unwind pass
    /// finds one whether or not the fill it is owed a pass for has been
    /// applied yet. Planning against the pre-fill position would size the
    /// withdrawal against liabilities the fill is about to raise, and the
    /// queue applies it *after* that fill — leaving the filler's own
    /// position under the very floor the plan was built to hold.
    ///
    /// So the pass compares the snapshot's ledger against the one the fill
    /// landed in, and waits for a snapshot that holds it.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_pass_waits_for_a_snapshot_that_holds_the_fill(db: sqlx::PgPool) -> sqlx::Result<()> {
        let behind = fill_then_unwind(db.clone(), 1).await;
        assert_eq!(
            behind.summary,
            TickSummary {
                planned: 1,
                executed: 1,
                ..TickSummary::default()
            },
            "the fill landed a ledger past the snapshot, so the pass plans nothing"
        );
        assert!(
            behind.pending,
            "and the pool stays pending for the tick that can see it"
        );
        assert_eq!(
            behind.unwind_after.get(harness::POOL),
            Some(&Some(harness::fixture_tick().sequence + 1)),
            "the evidence is the run's, not the tick's: it is what holds the pool at every \
             later tick whose snapshot is still behind the fill"
        );
        assert_eq!(
            behind.entry_reads, 6,
            "the auction entry, the fill's snapshot, its source account, and the pass's own \
             snapshot — no source account for a judgment that never happened"
        );
        assert_eq!(
            behind.simulations, 12,
            "four oracle reads and three balances for the fill, its judgment, and the pass's \
             four oracle reads: no wallet read and no judgment past the gate"
        );

        let holds = fill_then_unwind(db, 0).await;
        assert_eq!(
            holds.summary,
            TickSummary {
                planned: 1,
                executed: 1,
                unwound: 1,
                ..TickSummary::default()
            },
            "a snapshot at the ledger the fill landed in holds it, so the pass plans"
        );
        assert!(holds.pending, "a submission that landed leaves it pending");
        assert_eq!(
            holds.unwind_after.get(harness::POOL),
            Some(&Some(harness::fixture_tick().sequence)),
            "and it stays a high-water mark past the snapshot that proved it — raised \
             again, to the same ledger, by the unwind this pass then landed: this run's \
             worker answers every submission with it"
        );
        assert_eq!(
            holds.entry_reads, 7,
            "and one more source account, to judge"
        );
        assert_eq!(
            holds.simulations, 16,
            "and the pass's three balances and its own judgment"
        );
        Ok(())
    }

    /// The gate's other half: an *unwind* that landed moved the position
    /// as surely as a fill did, and the pass that follows must not be
    /// planned against a snapshot taken before it.
    ///
    /// Nothing about the position's shape says so. Step 2 of an unwind
    /// withdraws an absolute amount of the primary asset, and a position
    /// with no liabilities left gives the contract nothing to check: the
    /// same withdrawal planned twice against the same pre-withdrawal
    /// snapshot takes out what the first one already did, and
    /// `WithdrawCollateral` caps that at the position — the whole of it,
    /// leaving the primary collateral at zero, under the floor
    /// `plan_unwind` promises never to cross.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_landed_unwind_holds_its_pool_until_a_snapshot_holds_it(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // The ledger the unwind lands in: one past every snapshot but the
        // last, so only the last can be holding it.
        let landed = tick.sequence + 1;
        // Tick one: the startup pass over a collateral-only position, its
        // judgment, and the submission that lands.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        // Tick two: one snapshot, a ledger behind the withdrawal and so
        // still showing the collateral it took out. Nothing past it is
        // scripted — an unscripted read answers HTTP 500 — so a pass that
        // plans when it should have waited shows up in the counts below.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        // Tick three: a snapshot at the ledger the withdrawal landed in,
        // which is what it left behind.
        harness::script_snapshot_positions_at(&rpc, landed, &[]);
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
            notifier(),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash: TxHash([9_u8; 32]),
                    ledger: landed,
                    return_value: None,
                }));
            }
        });

        let first = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("the first tick");

        assert_eq!(
            first,
            TickSummary {
                unwound: 1,
                ..TickSummary::default()
            },
            "the startup pass planned the withdrawal and the chain took it"
        );
        assert!(
            state.unwind_pending.contains(harness::POOL),
            "a submission that landed leaves the pool pending"
        );
        let reads = rpc.calls("getLedgerEntries").len();
        let simulations = rpc.calls("simulateTransaction").len();

        let second = filler
            .tick(&mut state, later(tick, 1), true, Some(&queue), &shutdown)
            .await
            .expect("the second tick");

        assert_eq!(
            second,
            TickSummary::default(),
            "the snapshot predates the withdrawal, so this tick plans nothing"
        );
        assert!(
            state.unwind_pending.contains(harness::POOL),
            "and the pool stays pending for the tick that can see it"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            reads + 2,
            "the snapshot's two entry reads, and nothing past the gate"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            simulations + 4,
            "its four oracle reads: no wallet read, and no second judgment"
        );

        let third = filler
            .tick(&mut state, later(tick, 2), true, Some(&queue), &shutdown)
            .await
            .expect("the third tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert_eq!(
            third,
            TickSummary::default(),
            "this snapshot holds the withdrawal, and what it left is nothing to unwind"
        );
        assert!(
            !state.unwind_pending.contains(harness::POOL),
            "so the pool is cleared rather than withdrawn from a second time"
        );
        assert!(
            !state.unwind_after.contains_key(harness::POOL),
            "and the evidence goes with the pool: a pool with no position left has no \
             submission outstanding for a later snapshot to have to hold"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The gate is a high-water mark, not a one-shot: proving a snapshot
    /// holds the submission is not the same as having no submission
    /// outstanding.
    ///
    /// Four of `unwind_pool`'s later paths — reserves that will not
    /// accrue, a plan that cannot be built, a wallet that no longer
    /// covers the repays, an executor that failed — and a contract
    /// refusal all return having sent nothing while the pool stays
    /// pending. `latestLedger` is not monotonic across calls, and
    /// `same_ledger` only makes one snapshot's own parts agree, so the
    /// next pass can read an older ledger from a node behind a load
    /// balancer and see the pre-withdrawal position again. A gate that
    /// disarmed on proof would re-plan step 2's absolute primary
    /// `Withdraw` against it, which the contract caps at the position —
    /// taking the primary collateral to zero.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_proven_gate_still_holds_a_snapshot_that_lags(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // The ledger the withdrawal lands in: every snapshot below is at
        // this or at the one before it.
        let landed = tick.sequence + 1;
        // Tick one: the startup pass, judged and submitted.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        // Tick two: a snapshot a ledger behind it. Nothing past the
        // snapshot is scripted — an unscripted read answers HTTP 500 — so
        // a pass that plans when it should have waited shows up in the
        // counts below.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        // Tick three: a snapshot that holds the withdrawal, and a pass
        // the contract then refuses — `InvalidUtilRate`, which a
        // withdrawal draws when the reserve is fully lent out. It sent
        // nothing, so the withdrawal of tick one is still the newest
        // thing any snapshot must hold.
        script_unwind_position_at(
            &rpc,
            landed,
            signer.address(),
            &[(0, UNWIND_COLLATERAL)],
            &[],
        );
        script_wallet(&rpc, landed, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 11, landed);
        script_simulate_refused(&rpc, 1_207, landed);
        // Tick four, past the setback's backoff: a lagging node answers
        // with the ledger before the withdrawal, and the position it
        // shows is the one the withdrawal already took out.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        // Tick five: a snapshot that holds it again, and what it left.
        harness::script_snapshot_positions_at(&rpc, landed, &[]);
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
            notifier(),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash: TxHash([4_u8; 32]),
                    ledger: landed,
                    return_value: None,
                }));
            }
        });

        let first = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("the first tick");

        assert_eq!(
            first,
            TickSummary {
                unwound: 1,
                ..TickSummary::default()
            },
            "the startup pass planned the withdrawal and the chain took it"
        );

        let second = filler
            .tick(&mut state, later(tick, 1), true, Some(&queue), &shutdown)
            .await
            .expect("the second tick");

        assert_eq!(second, TickSummary::default(), "this snapshot is behind it");

        let third = filler
            .tick(&mut state, later(tick, 2), true, Some(&queue), &shutdown)
            .await
            .expect("the third tick");

        assert_eq!(
            third,
            TickSummary::default(),
            "the snapshot held it, so the pass planned — and the contract refused it"
        );
        assert_eq!(
            state.unwind_after.get(harness::POOL),
            Some(&Some(landed)),
            "a refusal sent nothing, so the withdrawal that did is still the least ledger \
             any later snapshot has to reach: proving one snapshot is not the same as \
             having nothing outstanding"
        );
        assert!(
            state.unwind_pending.contains(harness::POOL),
            "and the pool stays pending"
        );
        let reads = rpc.calls("getLedgerEntries").len();
        let simulations = rpc.calls("simulateTransaction").len();

        // The setback backed the next pass off by two ledgers.
        let fourth = filler
            .tick(&mut state, later(tick, 4), true, Some(&queue), &shutdown)
            .await
            .expect("the fourth tick");

        assert_eq!(
            fourth,
            TickSummary::default(),
            "the node that answered this one is behind the withdrawal again"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            reads + 2,
            "so it is gated: the snapshot's two entry reads, and nothing past them"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            simulations + 4,
            "its four oracle reads: no wallet read, and no judgment"
        );

        let fifth = filler
            .tick(&mut state, later(tick, 5), true, Some(&queue), &shutdown)
            .await
            .expect("the fifth tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert_eq!(
            fifth,
            TickSummary::default(),
            "a snapshot that holds the withdrawal again shows what it left: nothing"
        );
        assert!(
            !state.unwind_pending.contains(harness::POOL),
            "so the pool is cleared rather than withdrawn from a second time"
        );
        assert!(
            !state.unwind_after.contains_key(harness::POOL),
            "and the gate goes with it: a pool with no position left has no submission \
             outstanding to hold a later snapshot against"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The same evidence, for a fill, one tick later: a fill that landed
    /// at tick T is not a fact about tick T. An RPC a ledger or two behind
    /// still serves a pre-fill snapshot at T+1, and a gate that lived on
    /// the tick would have nothing left to hold it with.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_landed_fill_holds_its_pool_past_the_tick_it_landed_in(
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
        let landed = tick.sequence + 1;
        // Tick one: the fill walk, then a pass whose snapshot predates the
        // ledger the fill landed in — and in which this key already holds
        // a position, so nothing about the position's shape can decide.
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_empty_wallet(&rpc, tick.sequence);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        // Tick two: one snapshot, still a ledger behind the fill. Nothing
        // past it is scripted.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        // Tick three: a snapshot at the ledger the fill landed in, and the
        // pass it finally allows.
        script_unwind_position_at(
            &rpc,
            landed,
            signer.address(),
            &[(0, UNWIND_COLLATERAL)],
            &[],
        );
        script_wallet(&rpc, landed, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 11, landed);
        script_simulate_accepted(&rpc, landed);
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
            notifier(),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let (queue, mut receiver) =
            SubmissionQueue::new(NonZeroUsize::new(4).expect("a test capacity is never zero"));
        let worker = tokio::spawn(async move {
            while let Some(queued) = receiver.recv().await {
                let _ = queued.respond.send(Ok(TxOutcome::Succeeded {
                    hash: TxHash([8_u8; 32]),
                    ledger: landed,
                    return_value: None,
                }));
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
                executed: 1,
                ..TickSummary::default()
            },
            "the fill landed a ledger past the pass's own snapshot, so the pass waited"
        );
        // The tracker, applying the fill that closed the auction: the row
        // goes, so the ticks that follow are the pass alone.
        store
            .delete_auction(
                harness::POOL,
                harness::USER_ONE,
                AuctionType::UserLiquidation,
            )
            .await
            .expect("close the auction");
        let reads = rpc.calls("getLedgerEntries").len();
        let simulations = rpc.calls("simulateTransaction").len();

        let second = filler
            .tick(&mut state, later(tick, 1), true, Some(&queue), &shutdown)
            .await
            .expect("the second tick");

        assert_eq!(
            second,
            TickSummary::default(),
            "a tick later the snapshot is still behind the fill, and the fill is still \
             what the pass is owed"
        );
        assert!(
            state.unwind_pending.contains(harness::POOL),
            "so the pool stays pending"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            reads + 2,
            "the snapshot's two entry reads, and nothing past the gate"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            simulations + 4,
            "its four oracle reads: no wallet read, and no judgment"
        );

        let third = filler
            .tick(&mut state, later(tick, 2), true, Some(&queue), &shutdown)
            .await
            .expect("the third tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert_eq!(
            third,
            TickSummary {
                unwound: 1,
                ..TickSummary::default()
            },
            "and a snapshot at the ledger the fill landed in is planned against"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Ruling 3's other half: every configured pool is unwind-pending once
    /// at startup, so a position a previous run left behind is trimmed to
    /// the wallet without waiting for a fill. Once, not once a tick — a
    /// dry-run pass that has said what it would do is not repeated.
    #[sqlx::test(migrations = "./migrations")]
    async fn every_pool_is_unwound_once_at_startup(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // No auctions at all, so the fill walk reads nothing and what
        // follows is the startup pass alone.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let metrics = metrics();
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
            notifier(),
            Arc::clone(&metrics),
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
                unwound: 1,
                ..TickSummary::default()
            },
            "the whole XLM position is above a floor of nothing, so the startup pass plans \
             it out to the wallet"
        );
        assert!(
            metrics
                .render()
                .contains("blend_liquidator_unwind_passes_total 1\n"),
            "one pass, counted once"
        );
        assert!(
            !state.unwind_pending.contains(harness::POOL),
            "a dry run plans it once, not on every tick until a fill lands"
        );
        let reads = rpc.calls("getLedgerEntries").len();

        // Nothing is scripted for the second tick: an unscripted read
        // answers HTTP 500, and this pool must not make one.
        let second = filler
            .tick(&mut state, later(tick, 1), true, None, &shutdown)
            .await
            .expect("the second tick");

        assert_eq!(second, TickSummary::default());
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            reads,
            "the seed is once per run, so a pool no longer pending costs no chain read"
        );
        assert!(
            metrics
                .render()
                .contains("blend_liquidator_unwind_passes_total 1\n"),
            "and a tick that made no pass counts none"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// With no filler key there is no account to hold a position or a
    /// balance in, so the startup seed is dropped rather than read for.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_keyless_dry_run_makes_no_unwind_read(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, None, true),
            Inventory::new(XLM.to_string(), 0),
            notifier(),
            metrics(),
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
            "no key, no position, no wallet: the pass costs no chain read at all"
        );
        assert!(
            state.unwind_pending.is_empty(),
            "and the seed is dropped rather than retried every tick"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Ruling 4: a pass that moved something leaves its pool pending and the
    /// next tick plans it again from a fresh snapshot; the first pass that
    /// builds no requests is what stops it. Here the floor is what ends it —
    /// the third snapshot's position is under `min_primary_collateral`, so
    /// there is no excess to withdraw.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unwind_repeats_until_a_pass_is_idle(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // Three passes over a position the withdrawals shrink, and the
        // wallet re-read after each landed submission.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        script_unwind_position(&rpc, signer.address(), &[(0, 3_000_000_000_000)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 11, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        // Under the floor: nothing left to take out.
        script_unwind_position(&rpc, signer.address(), &[(0, 500_000_000_000)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let pools = vec![PoolConfig {
            min_primary_collateral: UNWIND_FLOOR,
            ..pool_config()
        }];
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
            notifier(),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let (queue, labels, worker) = recording_worker(4);

        let first = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("the first tick");
        assert_eq!(first.unwound, 1);
        assert!(state.unwind_pending.contains(harness::POOL));

        let second = filler
            .tick(&mut state, later(tick, 1), true, Some(&queue), &shutdown)
            .await
            .expect("the second tick");
        assert_eq!(second.unwound, 1, "it moved something again");
        assert!(state.unwind_pending.contains(harness::POOL));

        let third = filler
            .tick(&mut state, later(tick, 2), true, Some(&queue), &shutdown)
            .await
            .expect("the third tick");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert_eq!(
            third,
            TickSummary::default(),
            "the position is at its floor: this pass builds nothing"
        );
        assert!(
            !state.unwind_pending.contains(harness::POOL),
            "and an idle pass is what stops the repetition"
        );
        assert_eq!(
            labels.lock().expect("the recorder mutex").len(),
            2,
            "two submissions, one per pass that moved something"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Ruling 11: debt the wallet cannot repay notifies once per pool, not
    /// once per tick, and again only after a later pass has found the pool
    /// clean. The [`Notifier`]'s cooldown is zero here so that what is being
    /// counted is the filler's own set and not the notifier's dedup.
    ///
    /// Two notifications that never reach the channel are in the middle of
    /// it, and the filler answers them differently on purpose. The first
    /// pass finds every in-flight permit taken, so its notification is
    /// *dropped*: the notifier rolls its own dedup entry back and the
    /// filler's set has to roll back with it, or a high-severity alert is
    /// lost until some later pass happens to find the pool clean. The
    /// second pass's delivery *fails*, inside the notifier's task and after
    /// `notify` has already answered `Queued`: the filler keeps the pool
    /// marked and does not retry it, which is what the channel's own count
    /// — taken after a `drain`, since nothing else says when the task has
    /// run — proves.
    #[sqlx::test(migrations = "./migrations")]
    async fn leftover_debt_notifies_once_per_pool(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // A debt and no collateral, against an empty wallet: nothing to
        // repay it with and nothing to withdraw, which is an idle pass with
        // the debt named. Five of them, the fourth finding the position
        // gone — which is the pool clean.
        script_unwind_position(&rpc, signer.address(), &[], &[(1, UNWIND_DEBT)]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_unwind_position(&rpc, signer.address(), &[], &[(1, UNWIND_DEBT)]);
        script_unwind_position(&rpc, signer.address(), &[], &[(1, UNWIND_DEBT)]);
        harness::script_snapshot(&rpc, &[]);
        script_unwind_position(&rpc, signer.address(), &[], &[(1, UNWIND_DEBT)]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let recorder = Arc::new(Recorded::default());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recorder)),
            Duration::ZERO,
        ));
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
            Arc::clone(&notifier),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        // Every in-flight permit, taken by a send the channel is holding:
        // the first pass's notification is the one that finds none left.
        recorder.hold();
        for i in 0..NOTIFY_IN_FLIGHT {
            assert_eq!(
                notifier.notify(Notification {
                    kind: NotificationKind::FillConfirmed,
                    severity: Severity::Low,
                    pool: "pool-holding-a-permit".to_string(),
                    account: Some(format!("acct-{i}")),
                    message: "held until this test releases it".to_string(),
                }),
                Delivery::Queued
            );
        }
        tokio::task::yield_now().await;
        assert_eq!(notifier.in_flight(), NOTIFY_IN_FLIGHT);

        // An idle pass clears the pool, so each tick after the first is
        // made pending again the way a landed fill would.
        for ledgers in 0..5 {
            if ledgers > 0 {
                state.unwind_pending.insert(harness::POOL.to_string());
            }
            if ledgers == 1 {
                recorder.fail_next();
            }
            let summary = filler
                .tick(&mut state, later(tick, ledgers), true, None, &shutdown)
                .await
                .expect("tick");
            assert_eq!(
                summary,
                TickSummary::default(),
                "tick {ledgers} moved nothing"
            );
            match ledgers {
                0 => {
                    assert!(
                        !state.leftovers_notified.contains(harness::POOL),
                        "the notification was dropped, so nothing was notified and nothing is \
                         suppressed"
                    );
                    // Let the held sends finish, so the next pass has a
                    // permit to be queued on and the channel's count can be
                    // read for what it did with each pass.
                    recorder.release();
                    assert!(notifier.drain(Duration::from_secs(5)).await);
                    assert!(
                        recorder
                            .sent_of(NotificationKind::UnwindLeftovers)
                            .is_empty(),
                        "a dropped notification never reached the channel"
                    );
                }
                1 => {
                    assert!(
                        state.leftovers_notified.contains(harness::POOL),
                        "`notify` answered `Queued`, so this pass counts as having notified"
                    );
                    assert!(notifier.drain(Duration::from_secs(5)).await);
                    assert!(
                        recorder
                            .sent_of(NotificationKind::UnwindLeftovers)
                            .is_empty(),
                        "the channel failed this one; the notifier logged it and the filler \
                         does not retry it"
                    );
                }
                2 => {
                    assert!(state.leftovers_notified.contains(harness::POOL));
                    assert!(
                        recorder
                            .sent_of(NotificationKind::UnwindLeftovers)
                            .is_empty(),
                        "the pool is still marked, so this pass notified nothing at all"
                    );
                }
                3 => assert!(
                    !state.leftovers_notified.contains(harness::POOL),
                    "a clean pass ends the episode"
                ),
                _ => assert!(state.leftovers_notified.contains(harness::POOL)),
            }
        }
        assert!(notifier.drain(Duration::from_secs(5)).await);

        let sent = recorder.sent_of(NotificationKind::UnwindLeftovers);
        assert_eq!(
            sent.len(),
            1,
            "the episode after the clean pass is the only one the channel ever saw: {sent:?}"
        );
        assert_eq!(
            recorder.sent().len(),
            NOTIFY_IN_FLIGHT + 1,
            "the sends holding the permits were delivered too"
        );
        for notification in &sent {
            assert_eq!(notification.kind, NotificationKind::UnwindLeftovers);
            assert_eq!(notification.severity, Severity::High);
            assert_eq!(notification.pool, harness::POOL);
            assert_eq!(notification.account, None);
            assert!(
                notification.message.contains(harness::POOL) && notification.message.contains(USDC),
                "the message names the pool and the debt left in it: {}",
                notification.message
            );
        }
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// Ruling 9: inside the startup delay the pass still reads and still
    /// plans, and nothing is simulated or sent. The pool stays pending, so
    /// the first tick past the delay acts on it.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unwind_inside_the_startup_delay_waits(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
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
            notifier(),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, false, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary::default(),
            "nothing was submitted, so nothing is counted"
        );
        assert!(
            state.unwind_pending.contains(harness::POOL),
            "and the pool waits for the first tick past the delay"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            7,
            "the snapshot's four oracle reads and the wallet's three balances, and no \
             judgment: a judgment is the first step of sending"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            2,
            "the snapshot's own two reads, and no source account read"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The contract refused the pass — here `InvalidUtilRate`, which a
    /// withdrawal draws when the reserve is fully lent out. There is no
    /// percent to lower, so the pool simply stays pending and the next tick
    /// plans it again from fresh state.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_refused_unwind_stays_pending(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_refused(&rpc, 1_207, tick.sequence);
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
            notifier(),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("a refusal is the contract's answer, not this tick's failure");

        assert_eq!(
            summary,
            TickSummary::default(),
            "a refusal moved nothing, so it is not counted as an unwind"
        );
        assert!(
            state.unwind_pending.contains(harness::POOL),
            "and the pool is planned again from fresh state next tick"
        );
        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "nothing was sent: the refusal came from the simulation"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A pass that keeps being refused is backed off and, at the third
    /// consecutive one, said out loud once. `InvalidUtilRate` here, whose
    /// cause is the reserve's rather than the plan's: re-planning from
    /// fresh state answers it no differently on the next ledger, and the
    /// snapshot and the simulation it costs are what the backoff exists
    /// to bound.
    ///
    /// The schedule the ticks below are chosen from is exactly the
    /// doubling: a refusal at tick `t` plans the pool again at
    /// `t + 2^setbacks`. So passes happen at 0, 2 and 6, and the four
    /// ticks in between script *nothing* — an unscripted read answers HTTP
    /// 500 and `remaining()` catches an answer nobody used, so a pass that
    /// failed to back off cannot pass this test quietly. The landed pass
    /// at 14 is the run's end, and the pool's tracking goes with it.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_repeatedly_refused_unwind_backs_off_and_notifies_once(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // Ticks 0, 2 and 6: the pass reads, plans and is refused. Only the
        // first reads the wallet — nothing lands, so nothing makes it
        // stale.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_refused(&rpc, 1_207, tick.sequence);
        for sequence in [11, 12] {
            script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
            script_simulate_prelude(&rpc, &signer, sequence, tick.sequence);
            script_simulate_refused(&rpc, 1_207, tick.sequence);
        }
        // Tick 14, where the third setback's eight ledgers are up: this
        // one is accepted and lands.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_simulate_prelude(&rpc, &signer, 13, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let recorder = Arc::new(Recorded::default());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recorder)),
            Duration::from_hours(1),
        ));
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
            Arc::clone(&notifier),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let (queue, labels, worker) = recording_worker(4);

        for ledgers in 0..=6 {
            let summary = filler
                .tick(
                    &mut state,
                    later(tick, ledgers),
                    true,
                    Some(&queue),
                    &shutdown,
                )
                .await
                .expect("a refusal is the contract's answer, not this tick's failure");
            assert_eq!(
                summary,
                TickSummary::default(),
                "tick {ledgers} moved nothing"
            );
            assert!(
                state.unwind_pending.contains(harness::POOL),
                "tick {ledgers}: backing off keeps the pool pending"
            );
        }

        assert!(
            notifier.drain(Duration::from_secs(5)).await,
            "the deliveries are spawned, so counting what the channel saw means nothing until \
             they have finished"
        );
        let sent = recorder.sent();
        assert_eq!(
            sent.len(),
            1,
            "three refusals in a row are one alert, not three: {sent:?}"
        );
        assert_eq!(sent[0].kind, NotificationKind::SubmissionDropped);
        assert_eq!(sent[0].severity, Severity::High);
        assert_eq!(sent[0].pool, harness::POOL);
        assert_eq!(sent[0].account.as_deref(), Some(signer.address()));
        assert!(
            sent[0].message.contains("1207") && sent[0].message.contains("backing off"),
            "the message names the cause and that the pass is backing off: {}",
            sent[0].message
        );
        assert!(
            labels.lock().expect("the recorder mutex").is_empty(),
            "a refusal comes from the simulation: nothing reached the queue"
        );

        let landed = filler
            .tick(&mut state, later(tick, 14), true, Some(&queue), &shutdown)
            .await
            .expect("the pass the backoff was waiting for");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert_eq!(landed.unwound, 1, "the pass at 14 sent something");
        assert!(
            state.unwind_setbacks.is_empty(),
            "and a pass that landed ends the run: the next episode starts at full cadence"
        );
        assert_eq!(recorder.sent().len(), 1, "and notifies nothing new");
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A landed fill ends a backoff early. The pass that was refused was
    /// refused against a position the fill has now materially changed —
    /// new collateral, new debt — so waiting out a delay that was measured
    /// against the old one would hold the *new* position unwound for up to
    /// `UNWIND_BACKOFF_MAX_LEDGERS`, which is the one thing a fill must
    /// never be able to buy.
    ///
    /// Tick 1 is inside the first refusal's two-ledger backoff, so with
    /// the run left standing nothing of its pass would be read at all and
    /// the scripting below would go unused.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_landed_fill_ends_a_backoff_early(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // Tick 0: the startup pass, refused.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_refused(&rpc, 1_207, tick.sequence);
        // Tick 1, inside that backoff: a fill lands, and the pass it owes
        // runs in the same tick rather than waiting the delay out.
        let auction = auction(tick.sequence - 300);
        harness::script_auction_entry(&rpc, harness::USER_ONE, &auction, tick.sequence);
        harness::script_snapshot(&rpc, &[]);
        script_simulate_prelude(&rpc, &signer, 11, tick.sequence);
        script_simulate_accepted(&rpc, tick.sequence);
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 12, tick.sequence);
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
            notifier(),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();
        let (queue, labels, worker) = recording_worker(4);

        let first = filler
            .tick(&mut state, tick, true, Some(&queue), &shutdown)
            .await
            .expect("a refusal is the contract's answer, not this tick's failure");
        assert_eq!(first, TickSummary::default(), "the pass at 0 was refused");

        // The tracker, opening the auction the fill then takes.
        store
            .upsert_auction(&tracked(harness::USER_ONE, &auction))
            .await
            .expect("seed the auction");
        let second = filler
            .tick(&mut state, later(tick, 1), true, Some(&queue), &shutdown)
            .await
            .expect("the tick the fill lands in");
        drop(queue);
        worker.await.expect("the worker ends with the queue");

        assert_eq!(
            second,
            TickSummary {
                planned: 1,
                executed: 1,
                unwound: 1,
                ..TickSummary::default()
            },
            "the fill landed and the pass it owes ran in the same tick, backoff or not"
        );
        assert!(
            state.unwind_setbacks.is_empty(),
            "the run the fill interrupted is forgotten, not merely overridden once"
        );
        let labels = labels.lock().expect("the recorder mutex").clone();
        assert_eq!(labels.len(), 2, "the fill and then the unwind: {labels:?}");
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The pool's own `min_collateral`, as the filler wires it from the
    /// snapshot's instance — the fixture's is `50_000_000`, $5.00 in the
    /// oracle's seven decimals.
    ///
    /// The position is 337,000,000 XLM b-tokens against 15,000,000 USDC
    /// d-tokens, and the wallet is empty, so the debt stays. At the
    /// fixture's own accrued rates and prices that values at
    /// `collateral_base = 44_955_547` and `liability_base = 19_399_598`,
    /// a health factor of 2.317 — far clear of the 1.5075 margin, which
    /// asks for only `29_244_894` of base and would therefore allow
    /// `117_774_301` XLM stroops out. The pool's $5 floor allows none: the
    /// position is already under it, so every projection a withdrawal
    /// could reach is further under, and the pass is idle with the debt
    /// named.
    ///
    /// Which makes this the test of the *wiring*: with
    /// `min_collateral` read as zero the pass would plan that withdrawal
    /// and judge it, and nothing past the snapshot and the wallet is
    /// scripted here.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_pools_min_collateral_binds_the_filler_too(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        script_unwind_position(
            &rpc,
            signer.address(),
            &[(0, MIN_COLLATERAL_TRAPPED)],
            &[(1, MIN_COLLATERAL_DEBT)],
        );
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let recorder = Arc::new(Recorded::default());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recorder)),
            Duration::from_hours(1),
        ));
        let pools = vec![pool_config()];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
            Arc::clone(&notifier),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        let summary = filler
            .tick(&mut state, tick, true, None, &shutdown)
            .await
            .expect("tick");

        assert_eq!(
            summary,
            TickSummary::default(),
            "the $5 floor allows no withdrawal, so the pass moves nothing"
        );
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            2,
            "the snapshot's own two reads and no source account: an idle plan is judged by \
             nobody"
        );
        assert_eq!(
            rpc.calls("simulateTransaction").len(),
            7,
            "the snapshot's four oracle reads and the wallet's three balances, and nothing \
             else — a withdrawal the health margin alone would allow was never planned"
        );
        assert!(
            notifier.drain(Duration::from_secs(5)).await,
            "the deliveries are spawned, so counting what the channel saw means nothing until \
             they have finished"
        );
        let sent = recorder.sent();
        assert_eq!(
            sent.len(),
            1,
            "an idle pass with debt left names it: {sent:?}"
        );
        assert_eq!(sent[0].kind, NotificationKind::UnwindLeftovers);
        assert!(
            sent[0].message.contains(USDC),
            "the debt the wallet cannot repay: {}",
            sent[0].message
        );
        assert!(
            !state.unwind_pending.contains(harness::POOL),
            "and an idle pass leaves the pending set, however it got there"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The other way a run ends: an idle pass. Nothing is notified, since
    /// the count never reaches the alert, and the pool's tracking goes
    /// with the pool itself — the position is as unwound as it can be.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_refusal_an_idle_pass_ends_notifies_nothing(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
        // Tick 0: refused. Tick 1: nothing at all, because two ledgers is
        // the first backoff. Tick 2: a position already under the primary
        // floor, which is an idle pass.
        script_unwind_position(&rpc, signer.address(), &[(0, UNWIND_COLLATERAL)], &[]);
        script_wallet(&rpc, tick.sequence, [0, 0, 0]);
        script_simulate_prelude(&rpc, &signer, 10, tick.sequence);
        script_simulate_refused(&rpc, 1_207, tick.sequence);
        script_unwind_position(&rpc, signer.address(), &[(0, 500_000_000_000)], &[]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let submitter = Submitter::new(&client, &network, &signer, tx_config());
        let recorder = Arc::new(Recorded::default());
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recorder)),
            Duration::from_hours(1),
        ));
        let pools = vec![PoolConfig {
            min_primary_collateral: UNWIND_FLOOR,
            ..pool_config()
        }];
        let filler = Filler::new(
            &client,
            &store,
            &pools,
            filler_config(),
            Executor::new(&store, Some(submitter), true),
            Inventory::new(XLM.to_string(), 0),
            Arc::clone(&notifier),
            metrics(),
        );
        let (_flag, shutdown) = watch::channel(false);
        let mut state = FillerState::default();

        for ledgers in 0..=2 {
            filler
                .tick(&mut state, later(tick, ledgers), true, None, &shutdown)
                .await
                .expect("tick");
        }

        assert!(
            !state.unwind_pending.contains(harness::POOL),
            "an idle pass clears the pool however it got there"
        );
        assert!(
            state.unwind_setbacks.is_empty(),
            "and the refusal before it is forgotten with it"
        );
        assert!(
            notifier.drain(Duration::from_secs(5)).await,
            "the deliveries are spawned, so counting what the channel saw means nothing until \
             they have finished"
        );
        assert!(
            recorder.sent().is_empty(),
            "one refusal is not an alert: {:?}",
            recorder.sent()
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// An armed pass handed no queue planned its unwind and could not send
    /// it. The pool stays pending: a dry run that plans has said everything
    /// it is going to say, but an armed bot that planned and sent nothing
    /// still holds the position, and clearing it would leave nothing to
    /// schedule another pass but a restart or another landed fill.
    ///
    /// `Service::run` never composes this — a live filler with a signer is
    /// given its key's queue — so the assertion is about the shape being
    /// unreachable by construction rather than about a path the bot walks.
    /// Driven through `execute_unwind` for that reason.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_armed_pass_with_no_queue_stays_pending(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let signer = filler_signer();
        let network = Network::testnet();
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
            notifier(),
            metrics(),
        );
        // Nothing to spend, so the reservation is not what this is about.
        let plan = UnwindPlan {
            actions: vec![UnwindAction::WithdrawAll {
                asset: XLM.to_string(),
            }],
            spend: BTreeMap::new(),
            remaining_liabilities: Vec::new(),
            projected_health: None,
        };
        let mut state = FillerState::default();
        state.unwind_pending.insert(harness::POOL.to_string());
        let mut pass = Pass {
            tick,
            state: &mut state,
            summary: TickSummary::default(),
        };
        let filler_address = signer.address().to_string();

        filler
            .execute_unwind(&pools[0], &filler_address, &plan, None, &mut pass)
            .await
            .expect("a pass with nowhere to send is not the tick's failure");

        assert_eq!(
            pass.summary,
            TickSummary::default(),
            "nothing was sent, so nothing is counted as unwound"
        );
        assert!(
            pass.state.unwind_pending.contains(harness::POOL),
            "and the position is still there: the pool stays pending"
        );
        assert!(
            rpc.calls("sendTransaction").is_empty(),
            "the contract judged it and nothing sent it"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The wallet cannot fund this pass's repays: it is skipped with a
    /// warning and stays pending, and the executor is never asked anything.
    ///
    /// Driven through `execute_unwind` rather than `Filler::tick`, for the
    /// reason `a_wallet_that_cannot_fund_a_fill_skips_it` gives: `plan_unwind`
    /// caps every repay at the same `Inventory::available` the reservation
    /// is then taken out of, and nothing inside a tick moves the wallet
    /// between the two, so a tick cannot reach this refusal by itself. It is
    /// the guard for a wallet that moved under a plan, and a test of it has
    /// to hand the pass a spend the wallet no longer covers.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unwind_the_wallet_cannot_fund_is_skipped_this_tick(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let tick = harness::fixture_tick();
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        // A tenth of what the plan below repays.
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
            notifier(),
            metrics(),
        );
        let plan = UnwindPlan {
            actions: vec![UnwindAction::Repay {
                asset: USDC.to_string(),
                amount: 10_000_000_000,
            }],
            spend: BTreeMap::from([(USDC.to_string(), 10_000_000_000)]),
            remaining_liabilities: Vec::new(),
            projected_health: None,
        };
        let mut state = FillerState::default();
        state.unwind_pending.insert(harness::POOL.to_string());
        let mut pass = Pass {
            tick,
            state: &mut state,
            summary: TickSummary::default(),
        };
        // The executor holds no key, so the address a notification would
        // name is supplied here rather than read back off it.
        let filler_address = filler_signer().address().to_string();

        filler
            .execute_unwind(&pools[0], &filler_address, &plan, None, &mut pass)
            .await
            .expect("a wallet that cannot fund one unwind is not the tick's failure");

        assert_eq!(
            pass.summary,
            TickSummary {
                skipped: 1,
                ..TickSummary::default()
            },
            "a refused reservation is a decision, and it is counted as one"
        );
        assert!(
            pass.state.unwind_pending.contains(harness::POOL),
            "and the pool stays pending: the next tick plans it against the wallet as it \
             then stands"
        );
        assert!(
            rpc.received().await.is_empty(),
            "nothing was simulated and nothing was sent"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }
}
