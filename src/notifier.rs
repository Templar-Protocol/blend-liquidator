//! Notifications: a swappable delivery channel, and deduplication in front
//! of it so a repeating failure notifies once rather than every tick.
//!
//! Spec §7: "Notifications go to Telegram behind the `NotificationChannel`
//! trait with Templar's shell: deduplication by `(pool, account, kind)` with
//! a cooldown, a bounded in-flight semaphore, and `drain()` on every exit
//! path." This module is the trait, the dedup and a log-only channel;
//! [`NotificationKind`] already lists every kind that section names, so the
//! semaphore, `drain()` and the Telegram channel Phase 6b adds need no new
//! variant. Spec §8: "Notifier and metrics failures never affect trading" —
//! [`Notifier::notify`] therefore returns no `Result`. A channel failure is
//! logged and answered as [`Delivery::Failed`]; nothing upstream of a
//! notification ever has to decide whether its failure should hold up a
//! liquidation, a fill or an unwind.
//!
//! [`NotificationChannel::send`] returns a boxed future rather than being an
//! `async fn` so the trait stays object-safe without an `async_trait`
//! dependency: `Notifier` holds one behind `Box<dyn NotificationChannel>`.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

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

/// Every kind of notification spec §7 names. A closed set: Phase 6b uses
/// [`NotificationKind::as_str`] as a metric label, so a new variant is
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
    /// The snake_case label spec §7's kind names this. Stable: Phase 6b
    /// reuses it as a metric label, so changing one changes a dashboard.
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
    /// report: it never affects trading (spec §8), so the caller only logs
    /// it and rolls back the dedup entry it optimistically inserted.
    fn send<'a>(
        &'a self,
        notification: &'a Notification,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>>;
}

/// Logs every notification through `tracing` rather than delivering it
/// anywhere. The default channel, and the only one before Phase 6b's
/// Telegram channel lands.
#[derive(Debug, Clone, Copy)]
pub struct LogChannel;

impl NotificationChannel for LogChannel {
    fn name(&self) -> &'static str {
        "log"
    }

    fn send<'a>(
        &'a self,
        notification: &'a Notification,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
        Box::pin(async move {
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
            Ok(())
        })
    }
}

/// What became of a [`Notifier::notify`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Handed to the channel, which answered `Ok`.
    Sent,
    /// Suppressed: the same `(pool, account, kind)` was sent within the
    /// cooldown.
    Deduplicated,
    /// Handed to the channel, which answered `Err`; logged, and the dedup
    /// entry this call inserted was rolled back so the next attempt is not
    /// suppressed by a send that never actually happened.
    Failed,
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

/// Deduplicates by `(pool, account, kind)` with a cooldown (spec §7) and
/// delivers what survives through one [`NotificationChannel`].
///
/// Not `Clone`: every caller is meant to share one instance behind an
/// `Arc`, since the dedup state means nothing split across copies.
pub struct Notifier {
    channel: Box<dyn NotificationChannel>,
    cooldown: Duration,
    recent: Mutex<Recent>,
}

impl std::fmt::Debug for Notifier {
    /// Prints the channel's name and the cooldown only: `recent` is
    /// internal bookkeeping, not something a caller should come to depend
    /// on seeing in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field("channel", &self.channel.name())
            .field("cooldown", &self.cooldown)
            .finish_non_exhaustive()
    }
}

impl Notifier {
    #[must_use]
    pub fn new(channel: Box<dyn NotificationChannel>, cooldown: Duration) -> Self {
        Self {
            channel,
            cooldown,
            recent: Mutex::new(BTreeMap::new()),
        }
    }

    /// A [`Notifier`] over [`LogChannel`] — what every deployment gets
    /// before Phase 6b wires in Telegram.
    #[must_use]
    pub fn log_only(cooldown: Duration) -> Self {
        Self::new(Box::new(LogChannel), cooldown)
    }

    /// [`Notifier::notify_at`] at [`Instant::now`].
    pub async fn notify(&self, notification: Notification) -> Delivery {
        self.notify_at(notification, Instant::now()).await
    }

    /// Delivers `notification` unless the same `(pool, account, kind)` was
    /// sent within `cooldown` of `now`.
    ///
    /// The dedup entry is inserted *before* the send and the lock is
    /// dropped before awaiting it, so a slow channel never holds the lock
    /// across an await point; on failure the entry is removed again —
    /// only if it still holds the `now` this call inserted, so a
    /// concurrent call that has since sent its own notification for the
    /// same key is never undone by this one's rollback.
    pub async fn notify_at(&self, notification: Notification, now: Instant) -> Delivery {
        let key = (
            notification.pool.clone(),
            notification.account.clone(),
            notification.kind,
        );
        {
            let mut recent = lock(&self.recent);
            if let Some(&last) = recent.get(&key) {
                if now.saturating_duration_since(last) < self.cooldown {
                    return Delivery::Deduplicated;
                }
            }
            recent.insert(key.clone(), now);
        }

        match self.channel.send(&notification).await {
            Ok(()) => Delivery::Sent,
            Err(error) => {
                let mut recent = lock(&self.recent);
                if recent.get(&key) == Some(&now) {
                    recent.remove(&key);
                }
                drop(recent);
                tracing::warn!(
                    channel = self.channel.name(),
                    %error,
                    kind = notification.kind.as_str(),
                    pool = %notification.pool,
                    account = notification.account.as_deref(),
                    "notification delivery failed"
                );
                Delivery::Failed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

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

        assert_eq!(notifier.notify(note.clone()).await, Delivery::Sent);
        assert_eq!(notifier.notify(note).await, Delivery::Deduplicated);
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

        assert_eq!(notifier.notify(base.clone()).await, Delivery::Sent);
        assert_eq!(
            notifier
                .notify(Notification {
                    pool: "pool-b".to_string(),
                    ..base.clone()
                })
                .await,
            Delivery::Sent,
            "a different pool is its own key"
        );
        assert_eq!(
            notifier
                .notify(Notification {
                    account: Some("acct-2".to_string()),
                    ..base.clone()
                })
                .await,
            Delivery::Sent,
            "a different account is its own key"
        );
        assert_eq!(
            notifier
                .notify(Notification {
                    kind: NotificationKind::BadDebtReported,
                    ..base
                })
                .await,
            Delivery::Sent,
            "a different kind is its own key"
        );
        assert_eq!(recording.sent_count(), 4);
    }

    #[tokio::test]
    async fn the_cooldown_expires() {
        let recording = Arc::new(Recording::new(false));
        let notifier = Notifier::new(Box::new(Arc::clone(&recording)), cooldown());
        let note = notification(NotificationKind::PollerStalled, "pool-a", None);
        let now = Instant::now();

        assert_eq!(notifier.notify_at(note.clone(), now).await, Delivery::Sent);
        assert_eq!(
            notifier
                .notify_at(note, now + cooldown() + Duration::from_secs(1))
                .await,
            Delivery::Sent,
            "the cooldown has fully elapsed"
        );
        assert_eq!(recording.sent_count(), 2);
    }

    #[tokio::test]
    async fn a_failed_send_is_reported_and_does_not_start_a_cooldown() {
        let recording = Arc::new(Recording::new(true));
        let notifier = Notifier::new(Box::new(Arc::clone(&recording)), cooldown());
        let note = notification(NotificationKind::RpcFailing, "pool-a", None);

        assert_eq!(notifier.notify(note.clone()).await, Delivery::Failed);
        recording.fail.store(false, Ordering::SeqCst);
        assert_eq!(
            notifier.notify(note).await,
            Delivery::Sent,
            "the failed attempt must not have started a cooldown"
        );
        assert_eq!(
            recording.sent_count(),
            1,
            "only the successful attempt was recorded"
        );
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
