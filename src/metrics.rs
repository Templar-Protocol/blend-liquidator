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

/// Why the auctioneer or the filler skipped a tracked borrower without
/// acting. A closed set, rendered the same way [`Attempt`] is: all five
/// `skips_total` series every time, zero included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipLabel {
    /// The auction or fill needs an asset this pool's configuration does
    /// not support.
    UnsupportedAssets,
    /// The filler's wallet does not hold enough of what the fill needs.
    Unfunded,
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
    const ALL: [Self; 5] = [
        Self::UnsupportedAssets,
        Self::Unfunded,
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
    processed: Option<u32>,
    heartbeat: Option<(Instant, SystemTime)>,
    events_processed: Option<u64>,
    users_tracked: Option<i64>,
    auctions_open: Option<usize>,
    seed_accounts_loaded: Option<usize>,
}

/// The state behind [`Metrics`]'s one mutex.
struct Inner {
    pools: BTreeMap<String, PoolRecord>,
    creations: [u64; 3],
    fills: [u64; 3],
    skips: [u64; 5],
    /// Saturating running total, in the pool oracle's units. Display-only:
    /// see [`Metrics::profit`].
    profit_total: i128,
    reserved_inventory: BTreeMap<String, i128>,
    unwind_passes: u64,
    last_successful_scan: Option<SystemTime>,
    notifications: BTreeMap<(NotificationKind, DeliveryLabel), u64>,
}

impl Inner {
    fn new() -> Self {
        Self {
            pools: BTreeMap::new(),
            creations: [0; 3],
            fills: [0; 3],
            skips: [0; 5],
            profit_total: 0,
            reserved_inventory: BTreeMap::new(),
            unwind_passes: 0,
            last_successful_scan: None,
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

    /// Records the last ledger sequence this pool's poller has seen.
    pub fn ledger_head(&self, pool: &str, sequence: u32) {
        lock(&self.inner)
            .pools
            .entry(pool.to_string())
            .or_default()
            .head = Some(sequence);
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

    /// Counts one skipped borrower with the given reason.
    pub fn skip(&self, reason: SkipLabel) {
        let mut inner = lock(&self.inner);
        let slot = &mut inner.skips[reason as usize];
        *slot = slot.saturating_add(1);
    }

    /// Adds `oracle_units` to the running estimated-profit total,
    /// saturating: this total is rendered as a display-only float (see
    /// [`Metrics::render`]), never used in any decision, so a caller that
    /// has just realised a real profit has nothing wrong with the chain
    /// state it is reporting on.
    pub fn profit(&self, oracle_units: i128) {
        let mut inner = lock(&self.inner);
        inner.profit_total = inner.profit_total.saturating_add(oracle_units);
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

    /// Records that a full scan succeeded at wall-clock time `at`. Absent
    /// until this is called at least once: see
    /// `last_successful_scan_timestamp_seconds` in [`Metrics::render`].
    pub fn scan_succeeded(&self, at: SystemTime) {
        lock(&self.inner).last_successful_scan = Some(at);
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
            "Tracked borrowers skipped without acting, by reason.",
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
            "Running estimated profit, in the pool oracle's units scaled to whole tokens. Display only: never used in any decision.",
            "counter",
        );
        // Display-only: `profit_total` is never compared or branched on,
        // only rendered, so the precision `as f64` loses here is exactly
        // the precision a human reading a dashboard does not need.
        #[allow(clippy::cast_precision_loss)]
        let profit = inner.profit_total as f64 / 1e7;
        let _ = writeln!(out, "{METRIC_PREFIX}estimated_profit_total {profit:.7}");

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

        if let Some(at) = inner.last_successful_scan {
            header(
                &mut out,
                "last_successful_scan_timestamp_seconds",
                "Wall-clock time the last full scan succeeded.",
                "gauge",
            );
            let seconds = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let _ = writeln!(
                out,
                "{METRIC_PREFIX}last_successful_scan_timestamp_seconds {seconds}"
            );
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
        assert!(text.contains("blend_liquidator_estimated_profit_total 1335.0000000\n"));
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
        m.ledger_head("POOL", 7);
        let status = m.pool_status("POOL").expect("recorded");
        assert_eq!(status.head, Some(7));
        assert_eq!(status.processed, None);
        assert_eq!(status.heartbeat, Some(at));
        assert!(m.render().contains(
            "blend_liquidator_poller_heartbeat_timestamp_seconds{pool=\"POOL\"} 1700000000\n"
        ));
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn counters_saturate_rather_than_wrap() {
        let m = Metrics::new();
        m.profit(i128::MAX);
        m.profit(1);
        // Saturated, not wrapped: adding 1 past `i128::MAX` stays at
        // `i128::MAX`, so the rendered value is that total scaled exactly
        // the way `render` scales it, not some wrapped-around figure.
        #[allow(clippy::cast_precision_loss)]
        let expected = i128::MAX as f64 / 1e7;
        assert!(m.render().contains(&format!(
            "blend_liquidator_estimated_profit_total {expected:.7}\n"
        )));
    }

    #[test]
    fn default_is_new() {
        let m = Metrics::default();
        assert_eq!(m.pool_status("POOL"), None);
    }
}
