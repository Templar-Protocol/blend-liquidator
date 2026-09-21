//! The run's counters and gauges, rendered as Prometheus text exposition
//! format.
//!
//! [`Metrics`] is a pure, in-process recorder: the poller, the tracker, the
//! auctioneer, the filler and the notifier each call its methods as the
//! corresponding event happens (a ledger read, a creation, a fill, a skip,
//! a notification…) and the HTTP server's `/metrics` endpoint renders the
//! accumulated state on demand through [`Metrics::render`]. Nothing here
//! does I/O and nothing panics.
//!
//! One `Mutex<Inner>` backs every recorder and [`Metrics::render`] alike.
//! Every method that touches it is synchronous — there is no `.await`
//! anywhere in this module — so the lock is held for a few instructions
//! (a map lookup, an increment) and released before the call returns; it
//! is never held across an `.await` because there is none to hold it
//! across. A poisoned lock is recovered rather than propagated, the same
//! call [`crate::notifier`]'s `lock` makes: a metrics recorder that stops
//! for good over one panic is worse than one that occasionally serves a
//! slightly stale render.

use crate::notifier::NotificationKind;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Every metric name this module emits carries this prefix.
pub const METRIC_PREFIX: &str = "blend_liquidator_";

/// Whether an attempted creation or fill went through. A closed set:
/// [`Metrics::render`] emits all three `creations_total`/`fills_total`
/// series every time, zero included, so a dashboard never has to guess
/// whether a missing series means zero or means the bot has not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attempt {
    /// The bot decided to act and tried.
    Attempted,
    /// The attempt's transaction landed.
    Succeeded,
    /// The attempt was refused, or provably expired. Never an attempt
    /// whose outcome could not be confirmed: a
    /// [`crate::chain::TxOutcome::Unknown`] may still land, and is
    /// counted neither `succeeded` nor `failed` rather than guessed at
    /// here — a counter that guessed would have to be un-counted.
    Failed,
}

impl Attempt {
    /// Every variant, in declaration order — the order [`Metrics::render`]
    /// emits `creations_total`/`fills_total` series in.
    const ALL: [Self; 3] = [Self::Attempted, Self::Succeeded, Self::Failed];

    /// The label value spec's metric names use for this result. Stable:
    /// changing one changes a dashboard.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attempted => "attempted",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// Why the filler skipped an auction without taking it. A closed set,
/// rendered the same way [`Attempt`] is: all six `skips_total` series
/// every time, zero included. The auctioneer's own `SkipReason` is not
/// counted here — [`Metrics::skip`] has no auctioneer call site.
///
/// Counted once per auction *per reason*, never once per pass over one:
/// the filler re-makes every one of these decisions on every tick an
/// auction stays open, so it remembers what it has already counted (see
/// `FillerState::counted_skips`) and one auction the planner refuses
/// forever cannot bury the other five. A different reason for the same
/// auction counts again; the same reason does not until the auction
/// closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipLabel {
    /// The auction or fill needs an asset this pool's configuration does
    /// not support.
    UnsupportedAssets,
    /// The filler's wallet does not hold enough of what the fill needs.
    Unfunded,
    /// More of the primary asset would have closed the shortfall and the
    /// pool's `supply_cap` has no room for it.
    SupplyCapped,
    /// No plan closes the position at a profit worth taking.
    Unprofitable,
    /// Acting would leave the bot's own position below its health floor.
    Health,
    /// The chain refused the simulation for a reason nothing here
    /// classifies more specifically.
    ContractError,
}

impl SkipLabel {
    /// Every variant, in declaration order — the order [`Metrics::render`]
    /// emits `skips_total` series in.
    const ALL: [Self; 6] = [
        Self::UnsupportedAssets,
        Self::Unfunded,
        Self::SupplyCapped,
        Self::Unprofitable,
        Self::Health,
        Self::ContractError,
    ];

    /// The label value spec's metric names use for this reason.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedAssets => "unsupported_assets",
            Self::Unfunded => "unfunded",
            Self::SupplyCapped => "supply_capped",
            Self::Unprofitable => "unprofitable",
            Self::Health => "health",
            Self::ContractError => "contract_error",
        }
    }
}

/// What became of one notification. Distinct from
/// [`crate::notifier::Delivery`]: that type is `Notifier::notify`'s
/// immediate answer (queued, deduplicated, or dropped), while this one
/// also covers what became of a delivery *after* that answer — a channel
/// that refused it — which is why it carries a fourth member `Delivery`
/// does not, recorded from inside the delivery task rather than by the
/// caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeliveryLabel {
    /// Handed to a delivery task for the notifier's channel.
    Queued,
    /// Suppressed: the same notification was sent within its cooldown.
    Deduplicated,
    /// Never handed to the channel: every in-flight permit was taken.
    Dropped,
    /// Handed to the channel, which failed to deliver it.
    Failed,
}

impl DeliveryLabel {
    /// The label value spec's metric names use for this outcome.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Deduplicated => "deduplicated",
            Self::Dropped => "dropped",
            Self::Failed => "failed",
        }
    }
}

/// What [`Metrics::pool_status`] answers for one pool: whatever of its
/// per-pool gauges has been recorded, each `None` until its own recorder
/// is called at least once for that pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStatus {
    /// The last ledger sequence [`Metrics::ledger_head`] recorded.
    pub head: Option<u32>,
    /// When that head was recorded, as the [`Instant`]
    /// [`Metrics::ledger_head`] or [`Metrics::ledger_head_at`] was given.
    ///
    /// The head alone cannot tell a bot at chain head from one that has
    /// not read a head since an RPC outage began: both gauges stop moving
    /// together, and the lag between them stays where it was. This is what
    /// [`crate::http::readiness`] measures that against.
    pub head_at: Option<Instant>,
    /// The last ledger sequence [`Metrics::ledger_processed`] recorded.
    pub processed: Option<u32>,
    /// When this pool's poller last reported itself alive, as the
    /// [`Instant`] [`Metrics::heartbeat`] or [`Metrics::heartbeat_at`] was
    /// given.
    pub heartbeat: Option<Instant>,
}

/// One pool's own bookkeeping — everything [`Metrics`] can record by pool
/// name. Every field starts `None` and is set only by the recorder it
/// belongs to, so [`Metrics::pool_status`] can tell "never recorded" apart
/// from "recorded as zero".
#[derive(Debug, Default)]
struct PoolRecord {
    head: Option<u32>,
    head_at: Option<Instant>,
    processed: Option<u32>,
    heartbeat: Option<(Instant, SystemTime)>,
    events_processed: Option<u64>,
    users_tracked: Option<i64>,
    auctions_open: Option<usize>,
    seed_accounts_loaded: Option<usize>,
    last_successful_scan: Option<SystemTime>,
}

/// The state behind [`Metrics`]'s one mutex.
struct Inner {
    pools: BTreeMap<String, PoolRecord>,
    creations: [u64; 3],
    fills: [u64; 3],
    skips: [u64; 6],
    /// Saturating running total of the positive estimates, in the pool
    /// oracle's units. Display-only: see [`Metrics::profit`].
    profit_total: i128,
    /// Saturating running total of the negative estimates, by magnitude —
    /// never a subtraction from `profit_total`: both render as counters,
    /// and a counter that decreases is read as a reset. See
    /// [`Metrics::profit`].
    loss_total: i128,
    reserved_inventory: BTreeMap<String, i128>,
    unwind_passes: u64,
    notifications: BTreeMap<(NotificationKind, DeliveryLabel), u64>,
}

impl Inner {
    fn new() -> Self {
        Self {
            pools: BTreeMap::new(),
            creations: [0; 3],
            fills: [0; 3],
            skips: [0; 6],
            profit_total: 0,
            loss_total: 0,
            reserved_inventory: BTreeMap::new(),
            unwind_passes: 0,
            notifications: BTreeMap::new(),
        }
    }
}

/// Locks `inner`. A poisoned lock is recovered rather than propagated —
/// see the module doc.
fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Escapes a label value per the Prometheus text exposition format:
/// backslash, double quote, and newline are the only characters that
/// need it. Every free-form label value [`Metrics::render`] emits — a
/// pool address, an asset's contract id — is routed through this before
/// it is written, even though none of those ever actually contains one of
/// these characters: the escaper exists so that stays true by
/// construction rather than by convention.
#[must_use]
pub(crate) fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Emits one metric's `# HELP` and `# TYPE` header lines.
fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {METRIC_PREFIX}{name} {help}");
    let _ = writeln!(out, "# TYPE {METRIC_PREFIX}{name} {kind}");
}

/// The run's counters and gauges. Cheap to call from anywhere: every
/// method takes `&self`, so one [`Metrics`] is meant to be shared behind
/// an `Arc` by every task that records into it.
///
/// Not `Clone`: the state means nothing split across copies, the same
/// reason [`crate::notifier::Notifier`] is not `Clone` either.
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// An empty recorder: every counter zero, every gauge and series
    /// absent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::new()),
        }
    }

    /// [`Metrics::ledger_head_at`] at [`Instant::now`].
    pub fn ledger_head(&self, pool: &str, sequence: u32) {
        self.ledger_head_at(pool, sequence, Instant::now());
    }

    /// Records the last ledger sequence this pool's poller has seen, and
    /// `at` as when it saw it. The test seam [`Metrics::ledger_head`]
    /// calls with the current clock.
    ///
    /// The two are recorded together and never apart: a head with no
    /// reading time would leave [`crate::http::readiness`] unable to tell
    /// a bot at chain head from one whose RPC stopped answering, since
    /// nothing about either gauge moves in that case.
    pub fn ledger_head_at(&self, pool: &str, sequence: u32, at: Instant) {
        let mut inner = lock(&self.inner);
        let record = inner.pools.entry(pool.to_string()).or_default();
        record.head = Some(sequence);
        record.head_at = Some(at);
    }

    /// Records the last ledger sequence this pool's tracker has fully
    /// applied — the cursor [`crate::ledger::LedgerPoller`] commits.
    pub fn ledger_processed(&self, pool: &str, sequence: u32) {
        lock(&self.inner)
            .pools
            .entry(pool.to_string())
            .or_default()
            .processed = Some(sequence);
    }

    /// [`Metrics::heartbeat_at`] at [`Instant::now`] and
    /// [`SystemTime::now`].
    pub fn heartbeat(&self, pool: &str) {
        self.heartbeat_at(pool, Instant::now(), SystemTime::now());
    }

    /// Records that this pool's poller was alive at `at` (monotonic, for
    /// [`Metrics::pool_status`]) and `wall` (wall-clock, for the rendered
    /// `poller_heartbeat_timestamp_seconds`). The test seam
    /// [`Metrics::heartbeat`] calls with the two current clocks.
    pub fn heartbeat_at(&self, pool: &str, at: Instant, wall: SystemTime) {
        lock(&self.inner)
            .pools
            .entry(pool.to_string())
            .or_default()
            .heartbeat = Some((at, wall));
    }

    /// Adds `count` to this pool's processed-event counter, saturating:
    /// this is a display counter, not money, and a caller that has just
    /// applied `count` real events has nothing wrong with the chain state
    /// it is reporting on.
    pub fn events_processed(&self, pool: &str, count: u64) {
        let mut inner = lock(&self.inner);
        let record = inner.pools.entry(pool.to_string()).or_default();
        record.events_processed = Some(record.events_processed.unwrap_or(0).saturating_add(count));
    }

    /// Records this pool's current tracked-user count.
    pub fn users_tracked(&self, pool: &str, count: i64) {
        lock(&self.inner)
            .pools
            .entry(pool.to_string())
            .or_default()
            .users_tracked = Some(count);
    }

    /// Records this pool's current open-auction count.
    pub fn auctions_open(&self, pool: &str, count: usize) {
        lock(&self.inner)
            .pools
            .entry(pool.to_string())
            .or_default()
            .auctions_open = Some(count);
    }

    /// Records how many accounts this pool's seed pass loaded.
    pub fn seed_accounts_loaded(&self, pool: &str, count: usize) {
        lock(&self.inner)
            .pools
            .entry(pool.to_string())
            .or_default()
            .seed_accounts_loaded = Some(count);
    }

    /// Counts one auction-creation attempt with the given result.
    pub fn creation(&self, result: Attempt) {
        let mut inner = lock(&self.inner);
        let slot = &mut inner.creations[result as usize];
        *slot = slot.saturating_add(1);
    }

    /// Counts one fill attempt with the given result.
    pub fn fill(&self, result: Attempt) {
        let mut inner = lock(&self.inner);
        let slot = &mut inner.fills[result as usize];
        *slot = slot.saturating_add(1);
    }

    /// Counts one skipped auction with the given reason. Its caller owes
    /// the deduplication [`SkipLabel`] documents: one per auction per
    /// reason, not one per pass over it.
    pub fn skip(&self, reason: SkipLabel) {
        let mut inner = lock(&self.inner);
        let slot = &mut inner.skips[reason as usize];
        *slot = slot.saturating_add(1);
    }

    /// Adds one fill's estimate, in the pool oracle's own units, to the
    /// running total it belongs in: a positive value to
    /// `estimated_profit_total`, a negative one's magnitude to
    /// `estimated_loss_total`.
    ///
    /// Two counters rather than one signed running total, because both
    /// render as Prometheus counters and a counter that decreases is read
    /// as a counter reset — `rate()` over that window would then report a
    /// spurious jump. A negative estimate is reachable: `plan_fill`
    /// refuses one only when the pool does not set `force_fill`.
    ///
    /// Saturating, both of them, including the negation of [`i128::MIN`]:
    /// these totals are rendered and nothing else (see
    /// [`Metrics::render`]), never used in any decision, so a caller that
    /// has just realised a real fill has nothing wrong with the chain
    /// state it is reporting on.
    pub fn profit(&self, oracle_units: i128) {
        let mut inner = lock(&self.inner);
        if oracle_units < 0 {
            inner.loss_total = inner
                .loss_total
                .saturating_add(oracle_units.saturating_neg());
        } else {
            inner.profit_total = inner.profit_total.saturating_add(oracle_units);
        }
    }

    /// Replaces the reserved-inventory gauge map wholesale with `reserved`
    /// — this is a snapshot of the filler's current reservations, not an
    /// accumulator, so a stale asset that is no longer reserved must stop
    /// being rendered rather than linger at its last value.
    pub fn reserved_inventory(&self, reserved: &BTreeMap<String, i128>) {
        lock(&self.inner).reserved_inventory = reserved.clone();
    }

    /// Counts one completed unwind pass.
    pub fn unwind_pass(&self) {
        let mut inner = lock(&self.inner);
        inner.unwind_passes = inner.unwind_passes.saturating_add(1);
    }

    /// Records that `pool`'s full scan succeeded at wall-clock time `at`.
    ///
    /// Per pool, because the scan is: one unlabelled series would stay
    /// fresh on one pool's successes while another's scans failed every
    /// cadence, and "scans have stopped finishing" is exactly what an
    /// operator alerts on this for. A pool is absent from the series
    /// until this is called for it at least once: see
    /// `last_successful_scan_timestamp_seconds` in [`Metrics::render`].
    pub fn scan_succeeded(&self, pool: &str, at: SystemTime) {
        lock(&self.inner)
            .pools
            .entry(pool.to_string())
            .or_default()
            .last_successful_scan = Some(at);
    }

    /// Counts one notification's delivery outcome, by kind and by how it
    /// was delivered.
    pub fn notification(&self, kind: NotificationKind, delivery: DeliveryLabel) {
        let mut inner = lock(&self.inner);
        let slot = inner.notifications.entry((kind, delivery)).or_insert(0);
        *slot = slot.saturating_add(1);
    }

    /// What has been recorded for `pool`, or `None` if nothing has —
    /// [`Metrics::pool_status`] never fabricates a zero for a pool no
    /// recorder has named yet.
    #[must_use]
    pub fn pool_status(&self, pool: &str) -> Option<PoolStatus> {
        lock(&self.inner).pools.get(pool).map(|record| PoolStatus {
            head: record.head,
            head_at: record.head_at,
            processed: record.processed,
            heartbeat: record.heartbeat.map(|(at, _wall)| at),
        })
    }

    /// Renders every recorded metric as Prometheus text exposition
    /// format, in a fixed series order, ending with a trailing newline.
    /// See the module doc for the order and which series are closed
    /// (every label member, zero included) versus open (only what was
    /// actually recorded).
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn render(&self) -> String {
        let inner = lock(&self.inner);
        let mut out = String::new();

        header(
            &mut out,
            "ledger_head",
            "The last ledger sequence a pool's poller has seen.",
            "gauge",
        );
        for (pool, record) in &inner.pools {
            if let Some(head) = record.head {
                let _ = writeln!(
                    out,
                    "{METRIC_PREFIX}ledger_head{{pool=\"{}\"}} {head}",
                    escape_label(pool)
                );
            }
        }

        header(
            &mut out,
            "ledger_processed",
            "The last ledger sequence a pool's tracker has fully applied.",
            "gauge",
        );
        for (pool, record) in &inner.pools {
            if let Some(processed) = record.processed {
                let _ = writeln!(
                    out,
                    "{METRIC_PREFIX}ledger_processed{{pool=\"{}\"}} {processed}",
                    escape_label(pool)
                );
            }
        }

        header(
            &mut out,
            "poller_heartbeat_timestamp_seconds",
            "Wall-clock time a pool's poller last reported itself alive.",
            "gauge",
        );
        for (pool, record) in &inner.pools {
            if let Some((_at, wall)) = record.heartbeat {
                let seconds = wall
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let _ = writeln!(
                    out,
                    "{METRIC_PREFIX}poller_heartbeat_timestamp_seconds{{pool=\"{}\"}} {seconds}",
                    escape_label(pool)
                );
            }
        }

        header(
            &mut out,
            "events_processed_total",
            "Pool events applied by the tracker.",
            "counter",
        );
        for (pool, record) in &inner.pools {
            if let Some(count) = record.events_processed {
                let _ = writeln!(
                    out,
                    "{METRIC_PREFIX}events_processed_total{{pool=\"{}\"}} {count}",
                    escape_label(pool)
                );
            }
        }

        header(
            &mut out,
            "users_tracked",
            "Tracked borrowers currently owing something in a pool.",
            "gauge",
        );
        for (pool, record) in &inner.pools {
            if let Some(count) = record.users_tracked {
                let _ = writeln!(
                    out,
                    "{METRIC_PREFIX}users_tracked{{pool=\"{}\"}} {count}",
                    escape_label(pool)
                );
            }
        }

        header(
            &mut out,
            "auctions_open",
            "Open auctions currently tracked in a pool.",
            "gauge",
        );
        for (pool, record) in &inner.pools {
            if let Some(count) = record.auctions_open {
                let _ = writeln!(
                    out,
                    "{METRIC_PREFIX}auctions_open{{pool=\"{}\"}} {count}",
                    escape_label(pool)
                );
            }
        }

        header(
            &mut out,
            "seed_accounts_loaded",
            "Accounts a pool's seed pass loaded.",
            "gauge",
        );
        for (pool, record) in &inner.pools {
            if let Some(count) = record.seed_accounts_loaded {
                let _ = writeln!(
                    out,
                    "{METRIC_PREFIX}seed_accounts_loaded{{pool=\"{}\"}} {count}",
                    escape_label(pool)
                );
            }
        }

        header(
            &mut out,
            "creations_total",
            "Auction-creation attempts, by result.",
            "counter",
        );
        for attempt in Attempt::ALL {
            let _ = writeln!(
                out,
                "{METRIC_PREFIX}creations_total{{result=\"{}\"}} {}",
                attempt.as_str(),
                inner.creations[attempt as usize]
            );
        }

        header(
            &mut out,
            "fills_total",
            // The executor writes a fill's audit row before it enqueues
            // anything, so a fill the queue's prepare refused or found
            // stale leaves a row that was never counted here: the fills
            // table can hold more rows than this counts attempts. See
            // `Filler::note_recorded`.
            "Fill attempts handed to the chain, by result. A fill refused or found stale when \
             it was prepared leaves a fills row this does not count.",
            "counter",
        );
        for attempt in Attempt::ALL {
            let _ = writeln!(
                out,
                "{METRIC_PREFIX}fills_total{{result=\"{}\"}} {}",
                attempt.as_str(),
                inner.fills[attempt as usize]
            );
        }

        header(
            &mut out,
            "skips_total",
            "Auctions the filler skipped without taking, by reason. One per auction the \
             filler decided not to take, per reason, never one per pass over it, so the \
             reasons are comparable with each other.",
            "counter",
        );
        for reason in SkipLabel::ALL {
            let _ = writeln!(
                out,
                "{METRIC_PREFIX}skips_total{{reason=\"{}\"}} {}",
                reason.as_str(),
                inner.skips[reason as usize]
            );
        }

        header(
            &mut out,
            "estimated_profit_total",
            "Running estimated profit of the fills that landed, as an integer in the pool \
             oracle's own units — its decimals are the pool's to read, not this bot's to \
             assume. Display only: never used in any decision.",
            "counter",
        );
        let _ = writeln!(
            out,
            "{METRIC_PREFIX}estimated_profit_total {}",
            inner.profit_total
        );

        header(
            &mut out,
            "estimated_loss_total",
            "Running estimated loss of the fills that landed, by magnitude, as an integer in \
             the pool oracle's own units — its decimals are the pool's to read, not this \
             bot's to assume. Its own counter rather than a subtraction from \
             estimated_profit_total, which must never decrease. Display only.",
            "counter",
        );
        let _ = writeln!(
            out,
            "{METRIC_PREFIX}estimated_loss_total {}",
            inner.loss_total
        );

        header(
            &mut out,
            "reserved_inventory",
            "The filler wallet's current reservations, by asset.",
            "gauge",
        );
        for (asset, amount) in &inner.reserved_inventory {
            let _ = writeln!(
                out,
                "{METRIC_PREFIX}reserved_inventory{{asset=\"{}\"}} {amount}",
                escape_label(asset)
            );
        }

        header(
            &mut out,
            "unwind_passes_total",
            "Completed unwind passes.",
            "counter",
        );
        let _ = writeln!(
            out,
            "{METRIC_PREFIX}unwind_passes_total {}",
            inner.unwind_passes
        );

        // The one per-pool series whose header is conditional: a pool
        // that has never finished a scan carries no sample, and until one
        // pool has, the metric is not there at all.
        if inner
            .pools
            .values()
            .any(|record| record.last_successful_scan.is_some())
        {
            header(
                &mut out,
                "last_successful_scan_timestamp_seconds",
                "Wall-clock time a pool's last full scan succeeded.",
                "gauge",
            );
            for (pool, record) in &inner.pools {
                if let Some(at) = record.last_successful_scan {
                    let seconds = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                    let _ = writeln!(
                        out,
                        "{METRIC_PREFIX}last_successful_scan_timestamp_seconds{{pool=\"{}\"}} \
                         {seconds}",
                        escape_label(pool)
                    );
                }
            }
        }

        header(
            &mut out,
            "notifications_total",
            "Notifications, by kind and by delivery outcome.",
            "counter",
        );
        for ((kind, delivery), count) in &inner.notifications {
            let _ = writeln!(
                out,
                "{METRIC_PREFIX}notifications_total{{kind=\"{}\",delivery=\"{}\"}} {count}",
                kind.as_str(),
                delivery.as_str()
            );
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn render_carries_the_prefix_the_types_and_every_closed_label() {
        let m = Metrics::new();
        m.ledger_head("POOL", 105);
        m.ledger_processed("POOL", 100);
        m.creation(Attempt::Attempted);
        m.creation(Attempt::Succeeded);
        m.skip(SkipLabel::Unfunded);
        m.profit(13_350_000_000);
        m.reserved_inventory(&BTreeMap::from([("XLM".to_string(), 50_000_000_i128)]));
        let text = m.render();
        assert!(text.contains("# TYPE blend_liquidator_ledger_head gauge\n"));
        assert!(text.contains("blend_liquidator_ledger_head{pool=\"POOL\"} 105\n"));
        assert!(text.contains("blend_liquidator_ledger_processed{pool=\"POOL\"} 100\n"));
        assert!(text.contains("blend_liquidator_creations_total{result=\"attempted\"} 1\n"));
        assert!(
            text.contains("blend_liquidator_creations_total{result=\"failed\"} 0\n"),
            "closed sets render every member"
        );
        assert!(text.contains("blend_liquidator_skips_total{reason=\"unsupported_assets\"} 0\n"));
        assert!(text.contains("blend_liquidator_skips_total{reason=\"unfunded\"} 1\n"));
        assert!(
            text.contains("blend_liquidator_estimated_profit_total 13350000000\n"),
            "the oracle's own units, whatever its decimals are: {text}"
        );
        assert!(text.contains("blend_liquidator_estimated_loss_total 0\n"));
        assert!(text.contains("blend_liquidator_reserved_inventory{asset=\"XLM\"} 50000000\n"));
        assert!(
            !text.contains("last_successful_scan"),
            "absent until a scan succeeded"
        );
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn pool_status_reports_what_was_recorded() {
        let m = Metrics::new();
        assert_eq!(m.pool_status("POOL"), None);
        let at = Instant::now();
        m.heartbeat_at(
            "POOL",
            at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        );
        m.ledger_head_at("POOL", 7, at);
        let status = m.pool_status("POOL").expect("recorded");
        assert_eq!(status.head, Some(7));
        assert_eq!(
            status.head_at,
            Some(at),
            "a head is worth nothing to a readiness probe without when it was read"
        );
        assert_eq!(status.processed, None);
        assert_eq!(status.heartbeat, Some(at));
        assert!(m.render().contains(
            "blend_liquidator_poller_heartbeat_timestamp_seconds{pool=\"POOL\"} 1700000000\n"
        ));
    }

    /// The full scan is per pool, and so is the gauge that says one
    /// finished. One unlabelled series would stay fresh on the pool whose
    /// scans succeed while the pool beside it failed every cadence, and
    /// an operator alerting on "scans have stopped finishing" would never
    /// hear about the second one.
    #[test]
    fn a_scan_that_finished_is_stamped_for_its_own_pool() {
        let m = Metrics::new();
        assert!(
            !m.render().contains("last_successful_scan"),
            "absent until some pool's scan succeeded"
        );
        m.scan_succeeded(
            "A",
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        );
        // Recorded for B by something else, so B exists in the map and is
        // still missing from this series.
        m.users_tracked("B", 3);
        let text = m.render();
        assert!(
            text.contains(
                "blend_liquidator_last_successful_scan_timestamp_seconds{pool=\"A\"} \
                 1700000000\n"
            ),
            "{text}"
        );
        assert!(
            !text.contains("last_successful_scan_timestamp_seconds{pool=\"B\"}"),
            "a pool whose scans have never finished carries no sample at all: {text}"
        );
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    /// A loss is its own counter, never a decrement of the profit one: a
    /// Prometheus counter that went down would be read as a reset, and
    /// `rate()` over that window would report a spurious jump an operator
    /// might act on. A `force_fill` pool can land a fill whose estimate is
    /// negative, so this is a reachable state, not a hypothetical.
    #[test]
    fn a_negative_estimate_counts_as_a_loss_rather_than_lowering_the_profit() {
        let m = Metrics::new();
        m.profit(1_000);
        m.profit(-250);
        let text = m.render();
        assert!(
            text.contains("blend_liquidator_estimated_profit_total 1000\n"),
            "the profit counter only ever rises: {text}"
        );
        assert!(
            text.contains("blend_liquidator_estimated_loss_total 250\n"),
            "the loss is counted by magnitude, in its own counter: {text}"
        );
    }

    #[test]
    fn counters_saturate_rather_than_wrap() {
        let m = Metrics::new();
        m.profit(i128::MAX);
        m.profit(1);
        // Saturated, not wrapped: adding 1 past `i128::MAX` stays at
        // `i128::MAX`, so the rendered value is that total exactly.
        assert!(m.render().contains(&format!(
            "blend_liquidator_estimated_profit_total {}\n",
            i128::MAX
        )));

        let loss = Metrics::new();
        // `i128::MIN` has no positive counterpart, so negating it
        // saturates too rather than overflowing.
        loss.profit(i128::MIN);
        loss.profit(-1);
        assert!(loss.render().contains(&format!(
            "blend_liquidator_estimated_loss_total {}\n",
            i128::MAX
        )));
    }

    #[test]
    fn default_is_new() {
        let m = Metrics::default();
        assert_eq!(m.pool_status("POOL"), None);
    }
}
