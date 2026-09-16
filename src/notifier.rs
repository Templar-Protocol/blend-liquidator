//! Notifications: a swappable delivery channel, deduplication in front of
//! it so a repeating failure notifies once rather than every tick, and a
//! bounded number of deliveries in flight behind it.
//!
//! Spec §7: "Notifications go to Telegram behind the `NotificationChannel`
//! trait with Templar's shell: deduplication by `(pool, account, kind)` with
//! a cooldown, a bounded in-flight semaphore, and `drain()` on every exit
//! path." This module is the trait, the dedup, the semaphore, `drain()` and
//! a log-only channel; [`NotificationKind`] already lists every kind that
//! section names, so the Telegram channel needs no new variant.
//!
//! Spec §8: "Notifier and metrics failures never affect trading" —
//! [`Notifier::notify`] therefore returns no `Result`, **and does not
//! await the channel at all**. It answers what it did with the
//! notification ([`Delivery`]) and leaves the delivery itself to a spawned
//! task holding one of [`NOTIFY_IN_FLIGHT`] permits, so a channel that
//! takes seconds to answer — or never answers — delays no tick, no
//! liquidation and no fill. Two consequences a caller must know:
//!
//! - A delivery that *fails* fails after `notify` has already answered
//!   [`Delivery::Queued`]. The task logs it, rolls back the dedup entry so
//!   the next attempt is not suppressed by a send that never happened, and
//!   writes the notification through [`LogChannel`] so the operator still
//!   sees it. Nothing upstream is told, and nothing retries it.
//! - A notification that finds every permit taken is [`Delivery::Dropped`]:
//!   logged, written through [`LogChannel`], its dedup entry rolled back,
//!   and never handed to the channel. Dropping is the deliberate answer to
//!   a stuck channel — the alternative is a queue that grows for as long as
//!   the channel is down.
//!
//! Because `notify` spawns, a [`Notifier`] must be used from inside a tokio
//! runtime. [`Notifier::drain`] is what an exit path calls to give the
//! in-flight sends a bounded chance to finish before the process leaves.
//!
//! [`NotificationChannel::send`] returns a boxed future rather than being an
//! `async fn` so the trait stays object-safe without an `async_trait`
//! dependency: `Notifier` holds one behind `Box<dyn NotificationChannel>`.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use crate::metrics::{DeliveryLabel, Metrics};

/// The Telegram [`NotificationChannel`]: `sendMessage` for delivery, `getMe`
/// to verify credentials at startup.
pub mod telegram;

/// How many deliveries may be in flight at once. Past this a notification
/// is dropped rather than queued: a channel that has stopped answering must
/// cost a bounded amount of memory and a bounded number of tasks, and a
/// notification that has waited behind ten others is stale anyway.
pub const NOTIFY_IN_FLIGHT: usize = 10;

/// How long an exit path gives [`Notifier::drain`] before it leaves the
/// in-flight sends behind. Long enough for a slow HTTP round trip, short
/// enough that a wedged channel cannot hold up a shutdown.
pub const DRAIN_BUDGET: Duration = Duration::from_secs(10);

/// How urgently a notification should be surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Worth recording, not worth waking anyone: the bot did what it
    /// meant to.
    Low,
    /// Something did not go as planned and the bot handled it. Worth
    /// looking at, not worth interrupting anything for.
    Medium,
    /// Money or the bot's own position is at stake, or the bot has
    /// stopped making progress at something it is meant to finish.
    High,
}

/// Every kind of notification spec §7 names. A closed set:
/// [`NotificationKind::as_str`] is a metric label, so a new variant is
/// never added quietly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationKind {
    AuctionCreated,
    BadDebtReported,
    FillConfirmed,
    FillFailed,
    SubmissionDropped,
    UnwindLeftovers,
    PollerStalled,
    RpcFailing,
    EventGap,
    UnfundedFill,
}

impl NotificationKind {
    /// The snake_case label spec §7's kind names this. Stable: it is also
    /// a metric label, so changing one changes a dashboard.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuctionCreated => "auction_created",
            Self::BadDebtReported => "bad_debt_reported",
            Self::FillConfirmed => "fill_confirmed",
            Self::FillFailed => "fill_failed",
            Self::SubmissionDropped => "submission_dropped",
            Self::UnwindLeftovers => "unwind_leftovers",
            Self::PollerStalled => "poller_stalled",
            Self::RpcFailing => "rpc_failing",
            Self::EventGap => "event_gap",
            Self::UnfundedFill => "unfunded_fill",
        }
    }
}

/// One notification to deliver, and the key [`Notifier`] deduplicates it
/// by: `(pool, account, kind)`.
#[derive(Debug, Clone)]
pub struct Notification {
    /// What happened. Part of the dedup key, so two different kinds about
    /// one account in one pool never suppress each other.
    pub kind: NotificationKind,
    /// How urgently to surface it. Not part of the dedup key: the same
    /// event is always sent at the same severity, and a channel that
    /// routes by severity would otherwise see one kind arrive by two
    /// routes.
    pub severity: Severity,
    /// The pool it happened in. Always set — every kind this bot sends is
    /// about one pool's chain state — and part of the dedup key, so one
    /// pool's repeating failure never silences another's.
    pub pool: String,
    /// The borrower this notification is about, when it is about one —
    /// `PollerStalled`, `RpcFailing` and `EventGap` are pool-wide and carry
    /// `None`.
    pub account: Option<String>,
    /// Human-readable detail. Never a secret: every [`NotificationChannel`]
    /// this bot ever hands one to may log or forward it verbatim.
    pub message: String,
}

/// Why a [`NotificationChannel`] failed to deliver a [`Notification`].
///
/// The text is logged verbatim by [`Notifier`], so a channel must keep
/// secrets out of it: a bot token in a URL an HTTP client put in its own
/// error message would end up in the operator's logs.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("{0}")]
    Channel(String),
}

/// Where a [`Notification`] is delivered.
///
/// `send` returns a boxed future rather than being declared `async fn` so
/// this trait stays object-safe: an `async fn` in a trait desugars to an
/// anonymous, unnameable `Future` type, which cannot appear behind `dyn`
/// without either `async_trait` or this same boxing done by hand.
pub trait NotificationChannel: Send + Sync {
    /// The channel's name, for logs and [`Notifier`]'s `Debug`.
    fn name(&self) -> &'static str;

    /// Deliver one notification. A failure is this channel's alone to
    /// report: it never affects trading (spec §8), so the notifier only
    /// logs it, rolls back the dedup entry it optimistically inserted, and
    /// writes the notification through [`LogChannel`] instead.
    fn send<'a>(
        &'a self,
        notification: &'a Notification,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>>;
}

/// Logs every notification through `tracing` rather than delivering it
/// anywhere. The default channel, and [`Notifier`]'s fallback whatever its
/// configured channel is: a notification that was dropped or that the
/// channel refused is still written here, so nothing the bot decided to
/// say is lost just because the delivery path is down.
#[derive(Debug, Clone, Copy)]
pub struct LogChannel;

impl LogChannel {
    /// Writes `notification` to the log. This — and not any I/O — is the
    /// whole of [`LogChannel::send`], which is what lets [`Notifier`] call
    /// it from a synchronous path as well as from a delivery task.
    fn emit(notification: &Notification) {
        if notification.severity == Severity::High {
            tracing::warn!(
                kind = notification.kind.as_str(),
                severity = ?notification.severity,
                pool = %notification.pool,
                account = notification.account.as_deref(),
                "{}",
                notification.message
            );
        } else {
            tracing::info!(
                kind = notification.kind.as_str(),
                severity = ?notification.severity,
                pool = %notification.pool,
                account = notification.account.as_deref(),
                "{}",
                notification.message
            );
        }
    }
}

impl NotificationChannel for LogChannel {
    fn name(&self) -> &'static str {
        "log"
    }

    fn send<'a>(
        &'a self,
        notification: &'a Notification,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
        Box::pin(async move {
            Self::emit(notification);
            Ok(())
        })
    }
}

/// What [`Notifier::notify`] did with a notification. Never whether it
/// arrived: `notify` answers before the channel does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Handed to a delivery task holding one of [`NOTIFY_IN_FLIGHT`]
    /// permits. Whether the channel accepted it is not known yet, and is
    /// never reported back: a failure is logged, written through
    /// [`LogChannel`] and rolled back out of the dedup map by that task.
    Queued,
    /// Suppressed: the same `(pool, account, kind)` was sent within the
    /// cooldown.
    Deduplicated,
    /// Never handed to the channel: [`NOTIFY_IN_FLIGHT`] deliveries were
    /// already in flight. Logged, written through [`LogChannel`], and the
    /// dedup entry this call inserted was rolled back so the next attempt
    /// is not suppressed by a send that never happened.
    Dropped,
}

/// The key [`Notifier`] deduplicates by, and when it was last sent.
type Recent = BTreeMap<(String, Option<String>, NotificationKind), Instant>;

/// Locks `recent`. A poisoned lock is recovered rather than propagated, the
/// same call [`crate::inventory`]'s `lock` makes: this map holds no
/// invariant a panic mid-update could break that the next read does not
/// repair, and a notifier that stops for good over one is worse than one
/// that occasionally re-sends too early.
fn lock(recent: &Mutex<Recent>) -> MutexGuard<'_, Recent> {
    recent.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Everything a delivery task needs, which is everything the [`Notifier`]
/// has: the task outlives the call that spawned it, so this is shared
/// behind an `Arc` rather than borrowed.
struct Inner {
    channel: Box<dyn NotificationChannel>,
    /// Written through when the configured channel refuses a notification,
    /// or when there was no permit to hand it one. Held rather than
    /// constructed on the spot so that "there is always a fallback" is a
    /// property of the type and not of two call sites remembering.
    fallback: LogChannel,
    cooldown: Duration,
    recent: Mutex<Recent>,
    permits: Arc<Semaphore>,
    metrics: Option<Arc<Metrics>>,
}

impl Inner {
    /// Counts one outcome, if this run records metrics at all.
    fn record(&self, kind: NotificationKind, delivery: DeliveryLabel) {
        if let Some(metrics) = &self.metrics {
            metrics.notification(kind, delivery);
        }
    }

    /// Removes `key`'s dedup entry, but only if it still holds the `now`
    /// the call being rolled back inserted: a concurrent call that has
    /// since sent its own notification for the same key must not be undone
    /// by this one's rollback.
    fn roll_back(&self, key: &(String, Option<String>, NotificationKind), now: Instant) {
        let mut recent = lock(&self.recent);
        if recent.get(key) == Some(&now) {
            recent.remove(key);
        }
    }
}

/// Deduplicates by `(pool, account, kind)` with a cooldown (spec §7),
/// hands what survives to a delivery task holding one of
/// [`NOTIFY_IN_FLIGHT`] permits, and never makes a caller wait for a
/// channel (spec §8).
///
/// Must be used from inside a tokio runtime: [`Notifier::notify`] spawns.
///
/// Not `Clone`: every caller is meant to share one instance behind an
/// `Arc`, since the dedup state and the in-flight count mean nothing split
/// across copies.
pub struct Notifier {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Notifier {
    /// Prints the channel's name, the cooldown and what is in flight:
    /// `recent` is internal bookkeeping, not something a caller should
    /// come to depend on seeing in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field("channel", &self.inner.channel.name())
            .field("cooldown", &self.inner.cooldown)
            .field("in_flight", &self.in_flight())
            .finish_non_exhaustive()
    }
}

impl Notifier {
    #[must_use]
    pub fn new(channel: Box<dyn NotificationChannel>, cooldown: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                channel,
                fallback: LogChannel,
                cooldown,
                recent: Mutex::new(BTreeMap::new()),
                permits: Arc::new(Semaphore::new(NOTIFY_IN_FLIGHT)),
                metrics: None,
            }),
        }
    }

    /// A [`Notifier`] over [`LogChannel`] — what a deployment that
    /// configures no Telegram credentials gets.
    #[must_use]
    pub fn log_only(cooldown: Duration) -> Self {
        Self::new(Box::new(LogChannel), cooldown)
    }

    /// Counts every notification's outcome on `metrics`.
    ///
    /// A builder step, and only that: it must be called on a freshly built
    /// [`Notifier`], before any [`Notifier::notify`], because a delivery
    /// task holds an `Arc` to the same state. A call after one has been
    /// spawned cannot install anything and says so rather than counting
    /// half a run.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.metrics = Some(metrics);
        } else {
            tracing::error!(
                "with_metrics after a notification was already spawned; notifications are not \
                 counted"
            );
        }
        self
    }

    /// [`Notifier::notify_at`] at [`Instant::now`].
    pub fn notify(&self, notification: Notification) -> Delivery {
        self.notify_at(notification, Instant::now())
    }

    /// Hands `notification` to a delivery task unless the same
    /// `(pool, account, kind)` was sent within `cooldown` of `now`, or
    /// [`NOTIFY_IN_FLIGHT`] deliveries are already in flight.
    ///
    /// Returns as soon as the task is spawned: the channel is never
    /// awaited here, so nothing a caller does is ever held up by a
    /// delivery (spec §8). The dedup entry is inserted *before* the send
    /// and the lock is dropped before the spawn, so no lock is ever held
    /// across an await; a send that never happens — dropped here, or
    /// refused by the channel in the task — removes it again.
    ///
    /// # Panics
    ///
    /// If it is called outside a tokio runtime, since it spawns. Every
    /// caller in this crate is a task of [`crate::service`]'s.
    pub fn notify_at(&self, notification: Notification, now: Instant) -> Delivery {
        let key = (
            notification.pool.clone(),
            notification.account.clone(),
            notification.kind,
        );
        let deduplicated = {
            let mut recent = lock(&self.inner.recent);
            match recent.get(&key) {
                Some(&last) if now.saturating_duration_since(last) < self.inner.cooldown => true,
                _ => {
                    recent.insert(key.clone(), now);
                    false
                }
            }
        };
        if deduplicated {
            self.inner
                .record(notification.kind, DeliveryLabel::Deduplicated);
            return Delivery::Deduplicated;
        }

        let Ok(permit) = Arc::clone(&self.inner.permits).try_acquire_owned() else {
            self.inner.roll_back(&key, now);
            tracing::warn!(
                channel = self.inner.channel.name(),
                in_flight = NOTIFY_IN_FLIGHT,
                kind = notification.kind.as_str(),
                pool = %notification.pool,
                account = notification.account.as_deref(),
                "notification dropped: too many in flight"
            );
            // The operator still sees what the bot decided to say, even
            // though the channel never will: `Inner::fallback` writes it.
            // Called as an associated function because emitting is the
            // whole of that channel's `send` and this path is not async.
            LogChannel::emit(&notification);
            self.inner.record(notification.kind, DeliveryLabel::Dropped);
            return Delivery::Dropped;
        };

        let inner = Arc::clone(&self.inner);
        // Read before the notification is moved into the task.
        let kind = notification.kind;
        tokio::spawn(async move {
            // Held for the whole send and released by this drop, which is
            // what `drain` waits on.
            let _permit = permit;
            if let Err(error) = inner.channel.send(&notification).await {
                inner.roll_back(&key, now);
                tracing::warn!(
                    channel = inner.channel.name(),
                    // The error's own text, which a channel must keep free
                    // of credentials: this line is the operator's log.
                    %error,
                    kind = notification.kind.as_str(),
                    pool = %notification.pool,
                    account = notification.account.as_deref(),
                    "notification delivery failed"
                );
                // The operator still sees it. Ignored rather than
                // handled: `LogChannel` writes a tracing event and
                // nothing else, so it has no failure to report.
                let _ = inner.fallback.send(&notification).await;
                inner.record(notification.kind, DeliveryLabel::Failed);
            }
        });
        self.inner.record(kind, DeliveryLabel::Queued);
        Delivery::Queued
    }

    /// How many deliveries are in flight: spawned and not yet finished.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        NOTIFY_IN_FLIGHT.saturating_sub(self.inner.permits.available_permits())
    }

    /// Waits up to `budget` for every in-flight delivery to finish, and
    /// answers whether they all did. What an exit path calls, with
    /// [`DRAIN_BUDGET`], so a notification the bot decided to send on its
    /// way out has a bounded chance to leave before the process does.
    ///
    /// Acquires every permit and releases them again, so a notification
    /// sent *during* a drain is not blocked by it — draining is a wait,
    /// not a close.
    pub async fn drain(&self, budget: Duration) -> bool {
        // `NOTIFY_IN_FLIGHT` is 10; the saturating fallback is unreachable
        // and asks for more permits than exist rather than panicking.
        let all = u32::try_from(NOTIFY_IN_FLIGHT).unwrap_or(u32::MAX);
        // A closed semaphore cannot happen — nothing closes this one — and
        // is treated as a timeout rather than a panic if it ever does.
        if let Ok(Ok(permits)) =
            tokio::time::timeout(budget, self.inner.permits.acquire_many(all)).await
        {
            drop(permits);
            tracing::debug!("notifications drained");
            true
        } else {
            tracing::warn!(in_flight = self.in_flight(), "notification drain timed out");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A recording channel for the tests: what it was asked to send.
    struct Recording {
        sent: Mutex<Vec<Notification>>,
        fail: AtomicBool,
    }

    impl Recording {
        fn new(fail: bool) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail: AtomicBool::new(fail),
            }
        }

        fn sent_count(&self) -> usize {
            self.sent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
        }
    }

    impl NotificationChannel for Arc<Recording> {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn send<'a>(
            &'a self,
            notification: &'a Notification,
        ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
            Box::pin(async move {
                if self.fail.load(Ordering::SeqCst) {
                    return Err(NotifyError::Channel("recording channel failed".to_string()));
                }
                self.sent
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(notification.clone());
                Ok(())
            })
        }
    }

    /// A channel that holds every send until the test releases it, which is
    /// how a test holds permits open and watches what the notifier does
    /// with the notification that finds none left.
    struct Gated {
        sent: Mutex<Vec<Notification>>,
        gate: Arc<tokio::sync::Notify>,
        released: AtomicBool,
    }

    impl Gated {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                gate: Arc::new(tokio::sync::Notify::new()),
                released: AtomicBool::new(false),
            }
        }

        /// Lets every held send finish, and every later one through
        /// immediately.
        fn release(&self) {
            self.released.store(true, Ordering::SeqCst);
            self.gate.notify_waiters();
        }

        fn sent_count(&self) -> usize {
            self.sent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
        }
    }

    impl NotificationChannel for Arc<Gated> {
        fn name(&self) -> &'static str {
            "gated"
        }

        fn send<'a>(
            &'a self,
            notification: &'a Notification,
        ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
            Box::pin(async move {
                loop {
                    // Registered *before* the flag is re-read, so a release
                    // that lands between the two is never missed and this
                    // send cannot hang a test.
                    let notified = self.gate.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.released.load(Ordering::SeqCst) {
                        break;
                    }
                    notified.await;
                }
                self.sent
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(notification.clone());
                Ok(())
            })
        }
    }

    fn cooldown() -> Duration {
        Duration::from_hours(1)
    }

    fn notification(kind: NotificationKind, pool: &str, account: Option<&str>) -> Notification {
        Notification {
            kind,
            severity: Severity::Medium,
            pool: pool.to_string(),
            account: account.map(str::to_string),
            message: "test notification".to_string(),
        }
    }

    #[tokio::test]
    async fn the_first_of_a_kind_is_sent_and_the_next_within_the_cooldown_is_not() {
        let recording = Arc::new(Recording::new(false));
        let notifier = Notifier::new(Box::new(Arc::clone(&recording)), cooldown());
        let note = notification(NotificationKind::AuctionCreated, "pool-a", Some("acct-1"));

        assert_eq!(notifier.notify(note.clone()), Delivery::Queued);
        assert_eq!(notifier.notify(note), Delivery::Deduplicated);
        assert!(notifier.drain(Duration::from_secs(5)).await);
        assert_eq!(
            recording.sent_count(),
            1,
            "only the first send was recorded"
        );
    }

    #[tokio::test]
    async fn a_different_pool_account_or_kind_is_its_own_key() {
        let recording = Arc::new(Recording::new(false));
        let notifier = Notifier::new(Box::new(Arc::clone(&recording)), cooldown());
        let base = notification(NotificationKind::AuctionCreated, "pool-a", Some("acct-1"));

        assert_eq!(notifier.notify(base.clone()), Delivery::Queued);
        assert_eq!(
            notifier.notify(Notification {
                pool: "pool-b".to_string(),
                ..base.clone()
            }),
            Delivery::Queued,
            "a different pool is its own key"
        );
        assert_eq!(
            notifier.notify(Notification {
                account: Some("acct-2".to_string()),
                ..base.clone()
            }),
            Delivery::Queued,
            "a different account is its own key"
        );
        assert_eq!(
            notifier.notify(Notification {
                kind: NotificationKind::BadDebtReported,
                ..base
            }),
            Delivery::Queued,
            "a different kind is its own key"
        );
        assert!(notifier.drain(Duration::from_secs(5)).await);
        assert_eq!(recording.sent_count(), 4);
    }

    #[tokio::test]
    async fn the_cooldown_expires() {
        let recording = Arc::new(Recording::new(false));
        let notifier = Notifier::new(Box::new(Arc::clone(&recording)), cooldown());
        let note = notification(NotificationKind::PollerStalled, "pool-a", None);
        let now = Instant::now();

        assert_eq!(notifier.notify_at(note.clone(), now), Delivery::Queued);
        assert_eq!(
            notifier.notify_at(note, now + cooldown() + Duration::from_secs(1)),
            Delivery::Queued,
            "the cooldown has fully elapsed"
        );
        assert!(notifier.drain(Duration::from_secs(5)).await);
        assert_eq!(recording.sent_count(), 2);
    }

    #[tokio::test]
    async fn the_eleventh_in_flight_notification_is_dropped_and_its_dedup_entry_rolled_back() {
        let gated = Arc::new(Gated::new());
        let notifier = Notifier::new(Box::new(Arc::clone(&gated)), cooldown());

        for i in 0..NOTIFY_IN_FLIGHT {
            assert_eq!(
                notifier.notify(notification(
                    NotificationKind::AuctionCreated,
                    "pool",
                    Some(&format!("acct-{i}"))
                )),
                Delivery::Queued
            );
        }
        tokio::task::yield_now().await;
        assert_eq!(notifier.in_flight(), NOTIFY_IN_FLIGHT);

        let extra = notification(NotificationKind::AuctionCreated, "pool", Some("acct-extra"));
        assert_eq!(notifier.notify(extra.clone()), Delivery::Dropped);

        gated.release();
        assert!(
            notifier.drain(Duration::from_secs(5)).await,
            "drain finishes once the gate opens"
        );
        assert_eq!(notifier.in_flight(), 0, "every permit is back");
        assert_eq!(
            notifier.notify(extra),
            Delivery::Queued,
            "a dropped notification did not start a cooldown"
        );
        assert!(notifier.drain(Duration::from_secs(5)).await);
        assert_eq!(gated.sent_count(), NOTIFY_IN_FLIGHT + 1);
    }

    #[tokio::test]
    async fn drain_times_out_on_a_stuck_channel_and_reports_it() {
        let gated = Arc::new(Gated::new());
        let notifier = Notifier::new(Box::new(Arc::clone(&gated)), cooldown());

        assert_eq!(
            notifier.notify(notification(NotificationKind::EventGap, "pool", None)),
            Delivery::Queued
        );
        assert!(
            !notifier.drain(Duration::from_millis(50)).await,
            "a send the channel is still holding is not drained"
        );
        gated.release();
        assert!(notifier.drain(Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn a_failed_send_rolls_back_the_cooldown_and_counts_as_failed() {
        let recording = Arc::new(Recording::new(true));
        let metrics = Arc::new(Metrics::new());
        let notifier = Notifier::new(Box::new(Arc::clone(&recording)), cooldown())
            .with_metrics(Arc::clone(&metrics));
        let note = notification(NotificationKind::RpcFailing, "pool-a", None);

        assert_eq!(notifier.notify(note.clone()), Delivery::Queued);
        assert!(notifier.drain(Duration::from_secs(5)).await);
        recording.fail.store(false, Ordering::SeqCst);
        assert_eq!(
            notifier.notify(note),
            Delivery::Queued,
            "the failure did not start a cooldown"
        );
        assert!(notifier.drain(Duration::from_secs(5)).await);
        assert_eq!(
            recording.sent_count(),
            1,
            "only the successful attempt was recorded"
        );

        let text = metrics.render();
        assert!(
            text.contains("notifications_total{kind=\"rpc_failing\",delivery=\"queued\"} 2"),
            "both attempts were queued: {text}"
        );
        assert!(
            text.contains("notifications_total{kind=\"rpc_failing\",delivery=\"failed\"} 1"),
            "the failure is counted from inside the task: {text}"
        );
    }

    #[tokio::test]
    async fn a_dropped_notification_is_counted_and_a_deduplicated_one_too() {
        let gated = Arc::new(Gated::new());
        let metrics = Arc::new(Metrics::new());
        let notifier = Notifier::new(Box::new(Arc::clone(&gated)), cooldown())
            .with_metrics(Arc::clone(&metrics));
        let note = notification(NotificationKind::UnfundedFill, "pool-a", Some("acct-1"));

        assert_eq!(notifier.notify(note.clone()), Delivery::Queued);
        assert_eq!(notifier.notify(note), Delivery::Deduplicated);
        for i in 1..NOTIFY_IN_FLIGHT {
            assert_eq!(
                notifier.notify(notification(
                    NotificationKind::UnfundedFill,
                    "pool-a",
                    Some(&format!("filler-{i}"))
                )),
                Delivery::Queued
            );
        }
        tokio::task::yield_now().await;
        assert_eq!(
            notifier.notify(notification(
                NotificationKind::UnfundedFill,
                "pool-a",
                Some("acct-last")
            )),
            Delivery::Dropped
        );

        let text = metrics.render();
        assert!(
            text.contains("notifications_total{kind=\"unfunded_fill\",delivery=\"queued\"} 10"),
            "{text}"
        );
        assert!(
            text.contains(
                "notifications_total{kind=\"unfunded_fill\",delivery=\"deduplicated\"} 1"
            ),
            "{text}"
        );
        assert!(
            text.contains("notifications_total{kind=\"unfunded_fill\",delivery=\"dropped\"} 1"),
            "{text}"
        );

        gated.release();
        assert!(notifier.drain(Duration::from_secs(5)).await);
    }

    #[test]
    fn the_log_channel_names_the_kind() {
        assert_eq!(LogChannel.name(), "log");
        assert_eq!(
            NotificationKind::UnwindLeftovers.as_str(),
            "unwind_leftovers"
        );
    }
}
