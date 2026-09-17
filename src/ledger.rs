//! The clock. One poller per pool: it asks the RPC for chain head, reads
//! every pool event since its cursor, sends each decoded event followed by
//! the ledger's tick, and only then writes the cursor forward.
//!
//! That order is the invariant: a crash between sending and storing
//! re-reads a ledger, which the tracker handles because applying an event
//! twice is idempotent, while storing first would skip one silently.
//!
//! The cursor never moves past what was **applied**. A tick carries an
//! acknowledgement channel the tracker answers once it has written that
//! ledger's effects, and only that answer commits the cursor: a kill
//! between the send and the write — an aborted task, an eviction, a
//! `SIGKILL` past the grace period — drops whatever the tracker had not
//! yet consumed, and a cursor that had already moved past it would never
//! read those ledgers again. A dropped acknowledgement means "not
//! applied", so an RPC failure and a tracker that declines both leave the
//! cursor exactly where it was.
//!
//! A cursor older than the RPC's retained window cannot be caught up: the
//! events between are gone. The poller reports that as a `Gap` and restarts
//! at the window's edge, leaving the tracker to reseed rather than pretend
//! the missing ledgers held nothing.
//!
//! A pass that cannot prove it drained the range — paging stalled,
//! repeated, or ran past a hard cap — leaves the cursor untouched, for the
//! same reason: the bot re-reads rather than skips, and a persistently
//! broken RPC stalls visibly instead of silently losing ledgers.
//!
//! What the loop reports about itself is instrumentation and nothing more:
//! a heartbeat every iteration — and every `poll_interval` of a wait for a
//! tracker that has not answered yet, because waiting for one is being
//! alive — the chain head every pass that read one, and one
//! [`NotificationKind::RpcFailing`] per run of [`RPC_FAILING_AFTER`]
//! failed passes. None of it is awaited, none of it can fail a pass, and a
//! poller given neither recorder behaves exactly as one given both
//! (spec §8).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::chain::rpc::{EventQuery, RpcClient};
use crate::chain::xdr::{decode_pool_event, PoolEvent};
use crate::chain::ChainError;
use crate::metrics::Metrics;
use crate::notifier::{Notification, NotificationKind, Notifier, Severity};
use crate::store::{events_cursor, Cursor, Store, StoreError};

/// A ledger the poller has caught up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerTick {
    /// The ledger sequence.
    pub sequence: u32,
    /// Its close time, the timestamp reserves are accrued to when a user is
    /// valued at this ledger.
    pub close_time: u64,
}

/// What a poller sends downstream, in order. Not `Clone` or `Eq`: a
/// [`PollerMessage::Tick`] carries the one acknowledgement channel that
/// commits its ledger, and a copy of that is a second answer to a
/// question only the tracker may answer.
#[derive(Debug)]
pub enum PollerMessage {
    /// One decoded pool event.
    Event {
        /// The pool that emitted it.
        pool: String,
        /// The ledger it was emitted in.
        ledger: u32,
        /// The event.
        event: PoolEvent,
    },
    /// Every event up to and including this ledger has been sent.
    Tick {
        /// The pool.
        pool: String,
        /// The ledger.
        tick: LedgerTick,
        /// Answered once this ledger's effects are in the store. The
        /// poller commits its cursor on this and nothing else, so
        /// dropping the sender unanswered means "not applied" and leaves
        /// the range to be read again.
        ack: oneshot::Sender<()>,
    },
    /// The cursor fell out of the RPC's retained window: the events between
    /// `from` and `oldest` are gone and the user set must be reseeded.
    Gap {
        /// The pool.
        pool: String,
        /// The last ledger that was applied.
        from: u32,
        /// The oldest ledger the RPC still holds.
        oldest: u32,
    },
}

/// Polling and backoff timings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollerConfig {
    /// How often to ask for chain head.
    pub poll_interval: Duration,
    /// Events per `getEvents` page.
    pub page_limit: u32,
    /// First backoff after an RPC failure.
    pub min_backoff: Duration,
    /// Longest backoff after repeated failures.
    pub max_backoff: Duration,
}

impl PollerConfig {
    /// The spec's timings: pages of 200, backoff from one to thirty seconds.
    #[must_use]
    pub fn new(poll_interval: Duration) -> Self {
        Self {
            poll_interval,
            page_limit: 200,
            min_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }

    /// How long `/livez` (see [`crate::http`]) waits without a heartbeat
    /// before declaring this pool's poller dead.
    ///
    /// A healthy poller heartbeats roughly every `poll_interval`, but an
    /// RPC outage backs its loop off up to `max_backoff` between attempts
    /// (see the module doc), and a gap or a transient store error can cost
    /// another interval or two before the next heartbeat lands. Spec §7:
    /// an RPC outage must not by itself fail `/livez` — only a poller that
    /// has stopped making progress entirely should. `LIVENESS_INTERVALS`
    /// intervals plus one worst-case backoff is generous enough to absorb
    /// that without also absorbing a genuinely stuck poller.
    #[must_use]
    pub fn liveness_deadline(&self) -> Duration {
        self.poll_interval * LIVENESS_INTERVALS + self.max_backoff
    }
}

/// How many `poll_interval`s of silence [`PollerConfig::liveness_deadline`]
/// tolerates before a missing heartbeat fails `/livez`.
pub const LIVENESS_INTERVALS: u32 = 5;

/// How many consecutive failed passes [`LedgerPoller::run`] reports
/// [`NotificationKind::RpcFailing`] at.
///
/// Notified at exactly this many, never at more: one streak is one
/// notification, and the streak's own count — not the notifier's cooldown
/// — is what makes it so. High enough that a single flaky round trip is
/// merely a backoff, low enough that an outage is reported within a minute
/// of the default poll interval.
pub const RPC_FAILING_AFTER: u32 = 5;

/// A failure in the poller.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The RPC could not be read.
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
    /// The cursor could not be read or written.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// The receiving end went away, which happens during shutdown.
    #[error("the tracker channel is closed")]
    Closed,
    /// The tracker did not acknowledge a tick, so the ledger was not
    /// applied and the cursor stays where it is. Reported as a failure so
    /// the poller's own backoff slows the re-read rather than spinning on
    /// a tracker that keeps declining.
    #[error("the tracker did not apply ledger {ledger}")]
    NotApplied {
        /// The ledger that was sent and not applied.
        ledger: u32,
    },
}

/// A page count beyond which paging is presumed broken rather than merely
/// long: at the default 200-event page this is two million events in one
/// pass, far beyond any legitimate backlog. It exists to bound a
/// misbehaving RPC, not a busy one.
pub const MAX_PAGES: usize = 10_000;

/// The result of paging through one `getEvents` range.
struct DrainOutcome {
    /// Whether a short page proved the range was fully read.
    drained: bool,
    /// How many pages were fetched.
    pages: usize,
    /// Why paging stopped without draining; `None` when `drained` is `true`.
    stall_reason: Option<&'static str>,
}

/// Follows one pool's events.
pub struct LedgerPoller<'a> {
    rpc: &'a RpcClient,
    store: &'a Store,
    pool: &'a str,
    config: PollerConfig,
    /// Where the heartbeat and the chain-head gauge go, when this run
    /// records them at all. `None` records nothing: instrumentation is
    /// never load-bearing (spec §8), so every call through it is behind
    /// this option rather than behind a no-op recorder a caller must
    /// remember to build.
    metrics: Option<Arc<Metrics>>,
    /// Where a run of failed passes is reported, when this run notifies at
    /// all. Never awaited: [`Notifier::notify`] spawns.
    notifier: Option<Arc<Notifier>>,
    /// The cursor a `Gap` was last reported for. A gap makes the tracker
    /// walk every seed source, and a pass that then fails leaves the same
    /// stale cursor behind, so without this the next poll would compute
    /// the very same gap and reseed again — once per poll interval, which
    /// earns a rate limit rather than a recovery.
    gap_reported_at: Option<u32>,
}

impl std::fmt::Debug for LedgerPoller<'_> {
    /// Where this poller is, and nothing about what it records through:
    /// [`Metrics`] is not `Debug`, and neither recorder is state a log
    /// line should come to depend on.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerPoller")
            .field("pool", &self.pool)
            .field("config", &self.config)
            .field("gap_reported_at", &self.gap_reported_at)
            .finish_non_exhaustive()
    }
}

impl<'a> LedgerPoller<'a> {
    /// A poller for `pool`.
    #[must_use]
    pub fn new(rpc: &'a RpcClient, store: &'a Store, pool: &'a str, config: PollerConfig) -> Self {
        Self {
            rpc,
            store,
            pool,
            config,
            metrics: None,
            notifier: None,
            gap_reported_at: None,
        }
    }

    /// Records this poller's heartbeat and chain head on `metrics`.
    ///
    /// A builder step taken before [`LedgerPoller::run`]: without it the
    /// poller records nothing, which is what every caller that has no
    /// recorder — a test, an example — gets.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Reports a run of [`RPC_FAILING_AFTER`] failed passes through
    /// `notifier`.
    ///
    /// A builder step taken before [`LedgerPoller::run`]: without it a
    /// failing RPC is logged and nothing else.
    #[must_use]
    pub fn with_notifier(mut self, notifier: Arc<Notifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Reports `failures` consecutive failed passes, if this run notifies
    /// at all.
    ///
    /// Never awaited: [`Notifier::notify`] spawns its delivery, so a
    /// channel that has stopped answering costs the poll loop nothing
    /// (spec §8). A [`LedgerError`]'s `Display` is safe to forward — it
    /// carries an RPC's status or a store's message, never the API key,
    /// which [`RpcClient`] keeps out of its errors.
    fn report_rpc_failing(&self, failures: u32, error: &LedgerError) {
        let Some(notifier) = &self.notifier else {
            return;
        };
        notifier.notify(Notification {
            kind: NotificationKind::RpcFailing,
            severity: Severity::High,
            pool: self.pool.to_string(),
            account: None,
            message: format!("{failures} consecutive poll failures; last: {error}"),
        });
    }

    /// Waits for a tick's acknowledgement, heartbeating every
    /// `poll_interval` for as long as it takes.
    ///
    /// **Waiting for the tracker is being alive.** The tracker answers a
    /// tick only once that ledger's whole effect is in the store, and that
    /// can legitimately take longer than
    /// [`PollerConfig::liveness_deadline`]: a gap reseed walks every seed
    /// source, and a full scan refreshes every tracked borrower. A poller
    /// that recorded nothing while it waited would fail `/livez` (see
    /// [`crate::http::liveness`]) and have a restart probe kill the bot in
    /// the middle of the very seed it was waiting on — which, on restart,
    /// it would begin again.
    ///
    /// Answers exactly what awaiting the receiver answers, and nothing
    /// here touches what that means: a dropped sender is still "not
    /// applied", and the cursor still moves only on an answer.
    async fn await_ack(
        &self,
        applied: oneshot::Receiver<()>,
    ) -> Result<(), oneshot::error::RecvError> {
        heartbeat_while(
            self.metrics.as_deref(),
            self.pool,
            self.config.poll_interval,
            applied,
        )
        .await
    }

    /// Polls until `shutdown` flips, backing off on RPC failures. An RPC
    /// outage never advances the cursor, so nothing is skipped.
    ///
    /// Every iteration records a heartbeat first, before the shutdown
    /// check and before the pass: the heartbeat is this loop's own
    /// liveness, not the pass's, which is what lets `/livez` (see
    /// [`crate::http::liveness`]) tell an RPC outage — where the loop
    /// keeps turning and backing off — from a poller that has stopped
    /// turning at all. A run of [`RPC_FAILING_AFTER`] failed passes is
    /// notified once, at the threshold, and the counter is reset by the
    /// first pass that succeeds.
    pub async fn run(
        &mut self,
        sender: mpsc::Sender<PollerMessage>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), LedgerError> {
        let mut backoff = self.config.min_backoff;
        let mut failures: u32 = 0;
        loop {
            if let Some(metrics) = &self.metrics {
                metrics.heartbeat(self.pool);
            }
            if *shutdown.borrow_and_update() {
                return Ok(());
            }
            let wait = match self.poll_once(&sender).await {
                Ok(_) => {
                    if failures >= RPC_FAILING_AFTER {
                        tracing::info!(pool = self.pool, failures, "rpc recovered");
                    }
                    failures = 0;
                    backoff = self.config.min_backoff;
                    self.config.poll_interval
                }
                Err(LedgerError::Closed) => return Ok(()),
                Err(error) => {
                    failures = failures.saturating_add(1);
                    tracing::warn!(pool = self.pool, %error, failures, "poll failed; backing off");
                    // At the threshold, not past it: the count passes
                    // through this value once per streak, so one outage is
                    // one notification whatever the notifier's cooldown is.
                    if failures == RPC_FAILING_AFTER {
                        self.report_rpc_failing(failures, &error);
                    }
                    let wait = backoff;
                    backoff = (backoff * 2).min(self.config.max_backoff);
                    wait
                }
            };
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Pages `getEvents` from `start`, decoding and sending every pool
    /// event along the way, until a short page proves the range is
    /// drained. Stops early — without claiming the range drained — when
    /// paging looks broken: the page count reaches [`MAX_PAGES`], the
    /// returned cursor equals the one just used, or a full page carries no
    /// cursor at all.
    async fn drain_events(
        &self,
        start: u32,
        sender: &mpsc::Sender<PollerMessage>,
    ) -> Result<DrainOutcome, LedgerError> {
        // The cursor used for the request just sent, or `None` on the first
        // page when `start` drives it instead; it doubles as "the cursor
        // just used" for the repeat check below.
        let mut request_cursor: Option<String> = None;
        let mut pages: usize = 0;
        loop {
            pages += 1;
            let page = self
                .rpc
                .events(&EventQuery {
                    start_ledger: request_cursor.is_none().then_some(start),
                    cursor: request_cursor.as_deref(),
                    contract_ids: &[self.pool],
                    limit: self.config.page_limit,
                })
                .await?;
            let count = page.events.len();
            for event in &page.events {
                if event.contract_id != self.pool || !event.in_successful_contract_call {
                    continue;
                }
                match decode_pool_event(&event.topics, &event.value) {
                    Ok(Some(decoded)) => {
                        send(
                            sender,
                            PollerMessage::Event {
                                pool: self.pool.to_string(),
                                ledger: event.ledger,
                                event: decoded,
                            },
                        )
                        .await?;
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(
                        pool = self.pool,
                        ledger = event.ledger,
                        id = %event.id,
                        %error,
                        "an event this bot models did not decode; skipping it"
                    ),
                }
            }
            // A short page is the last one; the range is drained.
            if count < usize::try_from(self.config.page_limit).unwrap_or(usize::MAX) {
                return Ok(DrainOutcome {
                    drained: true,
                    pages,
                    stall_reason: None,
                });
            }
            // A full page means there is more to read. Bound how long that
            // can go on, and refuse to trust a cursor that did not move or
            // never came back — either would page forever or skip on the
            // next poll, and both are worse than pausing here.
            if pages >= MAX_PAGES {
                return Ok(DrainOutcome {
                    drained: false,
                    pages,
                    stall_reason: Some("page cap reached"),
                });
            }
            match page.cursor {
                Some(next) if Some(next.as_str()) == request_cursor.as_deref() => {
                    return Ok(DrainOutcome {
                        drained: false,
                        pages,
                        stall_reason: Some("cursor did not advance"),
                    });
                }
                Some(next) => request_cursor = Some(next),
                None => {
                    return Ok(DrainOutcome {
                        drained: false,
                        pages,
                        stall_reason: Some("full page returned no cursor"),
                    });
                }
            }
        }
    }

    /// One pass: head, events since the cursor, then the tick and the
    /// cursor. `None` when the chain has not moved, or when the pass could
    /// not prove it drained the range and left the cursor untouched.
    pub async fn poll_once(
        &mut self,
        sender: &mpsc::Sender<PollerMessage>,
    ) -> Result<Option<LedgerTick>, LedgerError> {
        let health = self.rpc.health().await?;
        let head = self.rpc.latest_ledger().await?;
        // The head this pool's poller has seen, whatever the pass does
        // with it: `/healthz` compares it against what the tracker has
        // applied, so it is recorded before anything here can decline.
        if let Some(metrics) = &self.metrics {
            metrics.ledger_head(self.pool, head.sequence);
        }
        let stored = self.store.cursor(&events_cursor(self.pool)).await?;

        // With no cursor the bot follows from now: history comes from
        // seeding, not from replaying every ledger the RPC still holds.
        let mut start = match &stored {
            None => head.sequence,
            Some(cursor) => cursor.ledger.saturating_add(1),
        };
        if let Some(cursor) = &stored {
            if start < health.oldest_ledger {
                // At most one `Gap` per stale cursor: each one costs the
                // tracker a walk of every seed source, and until the
                // cursor actually moves every later pass computes exactly
                // this gap again. Whether the reseed itself succeeded is
                // the tracker's business — it retries an incomplete one on
                // its own full-scan cadence.
                if self.gap_reported_at == Some(cursor.ledger) {
                    tracing::debug!(
                        pool = self.pool,
                        from = cursor.ledger,
                        oldest = health.oldest_ledger,
                        "the cursor is still outside the retained window; gap already reported"
                    );
                } else {
                    tracing::warn!(
                        pool = self.pool,
                        from = cursor.ledger,
                        oldest = health.oldest_ledger,
                        "the cursor fell out of the RPC's retained window; reseeding"
                    );
                    send(
                        sender,
                        PollerMessage::Gap {
                            pool: self.pool.to_string(),
                            from: cursor.ledger,
                            oldest: health.oldest_ledger,
                        },
                    )
                    .await?;
                    self.gap_reported_at = Some(cursor.ledger);
                }
                start = health.oldest_ledger;
            }
        }
        if start > head.sequence {
            return Ok(None);
        }

        let outcome = self.drain_events(start, sender).await?;

        // Only a drained pass may advance the cursor: events already sent
        // stay sent (applying one twice is idempotent), but a pass that
        // cannot prove it read everything must not tell the store it did.
        if !outcome.drained {
            tracing::warn!(
                pool = self.pool,
                reason = outcome.stall_reason.unwrap_or("unknown"),
                pages = outcome.pages,
                "the event stream did not drain; not advancing the cursor"
            );
            return Ok(None);
        }

        let tick = LedgerTick {
            sequence: head.sequence,
            close_time: head.close_time,
        };
        let (ack, applied) = oneshot::channel();
        send(
            sender,
            PollerMessage::Tick {
                pool: self.pool.to_string(),
                tick,
                ack,
            },
        )
        .await?;
        // The cursor says "applied", not "sent". Writing it before the
        // tracker has answered would let a kill drop everything still
        // queued while the store claims those ledgers are done, and
        // nothing re-reads them: `seed_pools_needing_it` reseeds only a
        // store that is empty or has no cursor, so a populated one never
        // recovers them.
        if self.await_ack(applied).await.is_err() {
            return Err(LedgerError::NotApplied {
                ledger: head.sequence,
            });
        }
        self.store
            .set_cursor(
                &events_cursor(self.pool),
                &Cursor {
                    ledger: head.sequence,
                    paging_token: None,
                },
            )
            .await?;
        Ok(Some(tick))
    }
}

/// Runs `fut` to completion while recording `pool`'s heartbeat on
/// `metrics` — once when the wait starts and again every `interval` until
/// it finishes — and answers exactly what `fut` answered.
///
/// **Working is being alive.** A heartbeat says the process is still
/// turning on this pool's behalf, and the two places that take longer
/// than [`PollerConfig::liveness_deadline`] without turning the poll loop
/// are the wait for a tick's acknowledgement (a gap reseed, a full scan)
/// and the initial seed, which runs before the pollers are spawned at
/// all. Neither is a stall, and a `/livez` (see [`crate::http::liveness`])
/// that read either as one would have a restart probe kill the bot in the
/// middle of the very work it was waiting on — which, on restart, it
/// would begin again.
///
/// Nothing here can change what `fut` answers or when: the output is
/// returned unchanged, and a caller whose `metrics` is `None` records
/// nothing at all (spec §8).
pub async fn heartbeat_while<F: std::future::Future>(
    metrics: Option<&Metrics>,
    pool: &str,
    interval: Duration,
    fut: F,
) -> F::Output {
    // `tokio::time::interval` panics on a zero period, and a heartbeat
    // helper is no place for a run to die: every production caller is far
    // above this floor (`POLL_INTERVAL_MS` is at least 100).
    let period = interval.max(Duration::from_millis(1));
    let mut ticker = tokio::time::interval(period);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            output = &mut fut => return output,
            // The first tick completes immediately, so a wait that starts
            // is itself recorded rather than only one that lasts a whole
            // interval.
            _ = ticker.tick() => {
                if let Some(metrics) = metrics {
                    metrics.heartbeat(pool);
                }
            }
        }
    }
}

/// Sends downstream, turning a closed channel into `Closed` rather than an
/// error the caller would retry.
async fn send(
    sender: &mpsc::Sender<PollerMessage>,
    message: PollerMessage,
) -> Result<(), LedgerError> {
    sender.send(message).await.map_err(|_| LedgerError::Closed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::script::ScriptedRpc;
    use crate::chain::xdr::encode::{address, i128_val, symbol, to_base64, vec as sc_vec};
    use crate::harness::RecordingChannel;
    use serde_json::json;
    use std::time::Duration;

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    /// One message a poll sent, without the acknowledgement channel a
    /// `Tick` carries: enough to assert what was sent, and in what order.
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Event {
            pool: String,
            ledger: u32,
            event: PoolEvent,
        },
        Tick(LedgerTick),
        Gap {
            pool: String,
            from: u32,
            oldest: u32,
        },
    }

    /// Records one message, answering a tick's acknowledgement when
    /// `acknowledge`, and dropping it unanswered otherwise — which is how
    /// a tracker says "not applied".
    fn record(message: PollerMessage, acknowledge: bool) -> Seen {
        match message {
            PollerMessage::Event {
                pool,
                ledger,
                event,
            } => Seen::Event {
                pool,
                ledger,
                event,
            },
            PollerMessage::Tick { tick, ack, .. } => {
                if acknowledge {
                    let _ = ack.send(());
                }
                Seen::Tick(tick)
            }
            PollerMessage::Gap { pool, from, oldest } => Seen::Gap { pool, from, oldest },
        }
    }

    /// Runs one pass while draining everything it sends. The poller waits
    /// for a tick's acknowledgement before committing its cursor, so a
    /// test that let the message sit in the channel would hang: every
    /// caller drains concurrently, and chooses whether to answer.
    async fn poll_draining(
        poller: &mut LedgerPoller<'_>,
        sender: &mpsc::Sender<PollerMessage>,
        receiver: &mut mpsc::Receiver<PollerMessage>,
        acknowledge: bool,
    ) -> (Result<Option<LedgerTick>, LedgerError>, Vec<Seen>) {
        let mut seen = Vec::new();
        let poll = poller.poll_once(sender);
        tokio::pin!(poll);
        let result = loop {
            tokio::select! {
                result = &mut poll => break result,
                Some(message) = receiver.recv() => seen.push(record(message, acknowledge)),
            }
        };
        // Whatever the pass sent after the last time this loop looked.
        while let Ok(message) = receiver.try_recv() {
            seen.push(record(message, acknowledge));
        }
        (result, seen)
    }

    fn config() -> PollerConfig {
        PollerConfig {
            poll_interval: Duration::from_millis(5),
            page_limit: 200,
            min_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
        }
    }

    fn health(latest: u32, oldest: u32) -> serde_json::Value {
        json!({
            "status": "healthy", "latestLedger": latest,
            "latestLedgerCloseTime": "1788635204", "oldestLedger": oldest,
            "oldestLedgerCloseTime": "1787949229", "ledgerRetentionWindow": 120_960
        })
    }

    fn latest(sequence: u32, close_time: u64) -> serde_json::Value {
        json!({"id": "aa", "protocolVersion": 27, "sequence": sequence, "closeTime": close_time.to_string()})
    }

    /// A `borrow` event as the pool emits it, at `ledger`.
    fn borrow_event(ledger: u32, index: u32) -> serde_json::Value {
        let topics = [
            symbol("borrow").unwrap(),
            address(USDC).unwrap(),
            address(USER).unwrap(),
        ];
        let value = sc_vec(vec![i128_val(1_000), i128_val(900)]).unwrap();
        json!({
            "type": "contract", "ledger": ledger, "ledgerClosedAt": "2026-09-05T14:28:07Z",
            "contractId": POOL, "id": format!("{ledger}-{index}"), "operationIndex": 0,
            "transactionIndex": 1, "txHash": "ab".repeat(32), "inSuccessfulContractCall": true,
            "topic": topics.iter().map(|t| to_base64(t).unwrap()).collect::<Vec<_>>(),
            "value": to_base64(&value).unwrap()
        })
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_first_poll_with_no_cursor_starts_at_chain_head(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(64_291_297, 64_150_000));
        rpc.expect("getLatestLedger", latest(64_291_297, 1_788_645_403));
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 64_291_297, "cursor": "64291297-4294967295", "events": []}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let mut poller = LedgerPoller::new(&client, &store, POOL, config());

        let (result, seen) = poll_draining(&mut poller, &sender, &mut receiver, true).await;
        let tick = result.expect("poll").expect("a tick");
        assert_eq!(
            (tick.sequence, tick.close_time),
            (64_291_297, 1_788_645_403)
        );
        // The stream starts at head, so the first getEvents asks for it.
        assert_eq!(rpc.calls("getEvents")[0]["startLedger"], 64_291_297);
        assert_eq!(seen, vec![Seen::Tick(tick)]);
        assert_eq!(
            store.cursor(&events_cursor(POOL)).await.expect("cursor"),
            Some(Cursor {
                ledger: 64_291_297,
                paging_token: None
            })
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn events_arrive_before_the_tick_and_the_cursor_follows(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        store
            .set_cursor(
                &events_cursor(POOL),
                &Cursor {
                    ledger: 100,
                    paging_token: None,
                },
            )
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(103, 1));
        rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 103, "cursor": "0103-2", "events": [borrow_event(101, 1), borrow_event(103, 2)]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let mut poller = LedgerPoller::new(&client, &store, POOL, config());

        let (result, seen) = poll_draining(&mut poller, &sender, &mut receiver, true).await;
        result.expect("poll");
        // It asked for the ledger after the cursor, not the cursor itself.
        assert_eq!(rpc.calls("getEvents")[0]["startLedger"], 101);
        assert_eq!(seen.len(), 3, "two events, then the tick");
        for (position, expected_ledger) in [101, 103].into_iter().enumerate() {
            match &seen[position] {
                Seen::Event {
                    pool,
                    ledger,
                    event,
                } => {
                    assert_eq!(pool, POOL);
                    assert_eq!(*ledger, expected_ledger);
                    assert!(matches!(event, PoolEvent::Borrow { .. }));
                }
                other => panic!("expected an event, got {other:?}"),
            }
        }
        assert!(
            matches!(seen[2], Seen::Tick(_)),
            "the tick comes after every event it covers"
        );
        assert_eq!(
            store
                .cursor(&events_cursor(POOL))
                .await
                .expect("cursor")
                .expect("set")
                .ledger,
            103
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_cursor_older_than_the_retained_window_is_a_gap(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        store
            .set_cursor(
                &events_cursor(POOL),
                &Cursor {
                    ledger: 10,
                    paging_token: None,
                },
            )
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(500_000, 400_000));
        rpc.expect("getLatestLedger", latest(500_000, 1_788_645_403));
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 500_000, "cursor": "500000-4294967295", "events": []}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let mut poller = LedgerPoller::new(&client, &store, POOL, config());

        let (result, seen) = poll_draining(&mut poller, &sender, &mut receiver, true).await;
        result.expect("poll");
        match &seen[0] {
            Seen::Gap { pool, from, oldest } => {
                assert_eq!((pool.as_str(), *from, *oldest), (POOL, 10, 400_000));
            }
            other => panic!("expected a gap, got {other:?}"),
        }
        // It restarts at the window's edge rather than skipping to head.
        assert_eq!(rpc.calls("getEvents")[0]["startLedger"], 400_000);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unmodelled_event_and_a_foreign_contract_are_both_ignored(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        store
            .set_cursor(
                &events_cursor(POOL),
                &Cursor {
                    ledger: 100,
                    paging_token: None,
                },
            )
            .await
            .expect("cursor");
        let mut unmodelled = borrow_event(101, 1);
        unmodelled["topic"] = json!([to_base64(&symbol("gulp").unwrap()).unwrap()]);
        let mut foreign = borrow_event(101, 2);
        foreign["contractId"] = json!(USDC);
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(101, 1));
        rpc.expect("getLatestLedger", latest(101, 1_788_645_403));
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 101, "cursor": "101-4294967295", "events": [unmodelled, foreign]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let mut poller = LedgerPoller::new(&client, &store, POOL, config());

        let (result, seen) = poll_draining(&mut poller, &sender, &mut receiver, true).await;
        result.expect("poll");
        assert!(
            matches!(seen.as_slice(), [Seen::Tick(_)]),
            "only the tick: {seen:?}"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_rpc_failure_leaves_the_cursor_alone(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let before = Cursor {
            ledger: 100,
            paging_token: None,
        };
        store
            .set_cursor(&events_cursor(POOL), &before)
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect_http("getHealth", 503);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, _receiver) = tokio::sync::mpsc::channel(16);
        let mut poller = LedgerPoller::new(&client, &store, POOL, config());

        assert!(matches!(
            poller.poll_once(&sender).await,
            Err(LedgerError::Chain(_))
        ));
        assert_eq!(
            store.cursor(&events_cursor(POOL)).await.expect("cursor"),
            Some(before)
        );
        Ok(())
    }

    /// Nothing new closed, so there is nothing to do and no cursor write.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_ledger_that_has_not_moved_is_a_no_op(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let at_head = Cursor {
            ledger: 103,
            paging_token: None,
        };
        store
            .set_cursor(&events_cursor(POOL), &at_head)
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(103, 1));
        rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let mut poller = LedgerPoller::new(&client, &store, POOL, config());

        assert_eq!(poller.poll_once(&sender).await.expect("poll"), None);
        assert!(receiver.try_recv().is_err(), "no messages");
        assert!(rpc.calls("getEvents").is_empty());
        assert_eq!(
            store.cursor(&events_cursor(POOL)).await.expect("cursor"),
            Some(at_head)
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_pass_that_does_not_drain_leaves_the_cursor_alone(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let before = Cursor {
            ledger: 100,
            paging_token: None,
        };
        store
            .set_cursor(&events_cursor(POOL), &before)
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(103, 1));
        rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        // A full page (exactly `page_limit` events) whose cursor comes back
        // unchanged on the next request: the range never advances.
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 103, "cursor": "101-1", "events": [borrow_event(101, 1), borrow_event(101, 2)]}),
        );
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 103, "cursor": "101-1", "events": [borrow_event(101, 3), borrow_event(101, 4)]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let config = PollerConfig {
            page_limit: 2,
            ..config()
        };
        let mut poller = LedgerPoller::new(&client, &store, POOL, config);

        assert_eq!(poller.poll_once(&sender).await.expect("poll"), None);
        while let Ok(message) = receiver.try_recv() {
            assert!(
                !matches!(message, PollerMessage::Tick { .. }),
                "a pass that did not drain must not tick"
            );
        }
        assert_eq!(
            store.cursor(&events_cursor(POOL)).await.expect("cursor"),
            Some(before)
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_full_page_without_a_cursor_does_not_advance_the_cursor(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let before = Cursor {
            ledger: 100,
            paging_token: None,
        };
        store
            .set_cursor(&events_cursor(POOL), &before)
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(103, 1));
        rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        // A full page with no cursor at all: real RPCs never do this, but
        // if one ever did the bot must not skip past it.
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 103, "cursor": null, "events": [borrow_event(101, 1), borrow_event(101, 2)]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let config = PollerConfig {
            page_limit: 2,
            ..config()
        };
        let mut poller = LedgerPoller::new(&client, &store, POOL, config);

        assert_eq!(poller.poll_once(&sender).await.expect("poll"), None);
        while let Ok(message) = receiver.try_recv() {
            assert!(
                !matches!(message, PollerMessage::Tick { .. }),
                "a pass that did not drain must not tick"
            );
        }
        assert_eq!(
            store.cursor(&events_cursor(POOL)).await.expect("cursor"),
            Some(before)
        );
        Ok(())
    }

    /// `run` polls until the shutdown flag flips, then returns.
    #[sqlx::test(migrations = "./migrations")]
    async fn run_stops_on_the_shutdown_flag(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        for _ in 0..40 {
            rpc.expect("getHealth", health(103, 1));
            rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        }
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, _receiver) = tokio::sync::mpsc::channel(16);
        let (flag, watch) = tokio::sync::watch::channel(false);
        let mut poller = LedgerPoller::new(&client, &store, POOL, config());
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            flag.send(true).expect("flag");
        });
        poller.run(sender, watch).await.expect("run");
        stopper.await.expect("stopper");
        Ok(())
    }

    /// The cursor means "applied": a tick the tracker never acknowledges
    /// leaves it exactly where it was, so the next pass re-reads the same
    /// range rather than resuming past ledgers nobody stored.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_tick_the_tracker_does_not_acknowledge_leaves_the_cursor_alone(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let before = Cursor {
            ledger: 100,
            paging_token: None,
        };
        store
            .set_cursor(&events_cursor(POOL), &before)
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(103, 1));
        rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 103, "cursor": "103-1", "events": [borrow_event(101, 1)]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let mut poller = LedgerPoller::new(&client, &store, POOL, config());

        // The tick is drained and dropped unanswered, which is what the
        // tracker does when it could not apply the ledger.
        let (result, seen) = poll_draining(&mut poller, &sender, &mut receiver, false).await;
        assert!(
            matches!(result, Err(LedgerError::NotApplied { ledger: 103 })),
            "an unacknowledged tick is a failure the poller backs off from"
        );
        assert!(
            seen.iter().any(|message| matches!(message, Seen::Tick(_))),
            "the tick was sent; only its acknowledgement was withheld"
        );
        assert_eq!(
            store.cursor(&events_cursor(POOL)).await.expect("cursor"),
            Some(before),
            "an unapplied ledger never moves the cursor"
        );
        Ok(())
    }

    /// A gap is reported once per stale cursor. Each one costs the tracker
    /// a walk of every seed source, and a pass that cannot drain leaves the
    /// same cursor behind, so a repeat would reseed once per poll.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_gap_is_reported_once_until_the_cursor_moves(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        store
            .set_cursor(
                &events_cursor(POOL),
                &Cursor {
                    ledger: 10,
                    paging_token: None,
                },
            )
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        // Two passes, each of which fails to drain (a full page whose
        // cursor never advances), so the stale cursor survives both.
        for _ in 0..2 {
            rpc.expect("getHealth", health(500_000, 400_000));
            rpc.expect("getLatestLedger", latest(500_000, 1_788_645_403));
            rpc.expect(
                "getEvents",
                json!({"latestLedger": 500_000, "cursor": "400000-1",
                       "events": [borrow_event(400_001, 1), borrow_event(400_001, 2)]}),
            );
            rpc.expect(
                "getEvents",
                json!({"latestLedger": 500_000, "cursor": "400000-1",
                       "events": [borrow_event(400_001, 3), borrow_event(400_001, 4)]}),
            );
        }
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        let config = PollerConfig {
            page_limit: 2,
            ..config()
        };
        let mut poller = LedgerPoller::new(&client, &store, POOL, config);

        let (first, seen_first) = poll_draining(&mut poller, &sender, &mut receiver, true).await;
        assert_eq!(first.expect("first pass"), None, "the pass did not drain");
        assert_eq!(
            seen_first
                .iter()
                .filter(|message| matches!(message, Seen::Gap { .. }))
                .count(),
            1,
            "the first pass reports the gap"
        );

        let (second, seen_second) = poll_draining(&mut poller, &sender, &mut receiver, true).await;
        assert_eq!(second.expect("second pass"), None);
        assert!(
            !seen_second
                .iter()
                .any(|message| matches!(message, Seen::Gap { .. })),
            "the same stale cursor does not report the gap twice: {seen_second:?}"
        );
        // It still restarts at the window edge, gap message or not.
        assert_eq!(rpc.calls("getEvents")[2]["startLedger"], 400_000);

        // The cursor then moves — a recovery, or an operator's fix — to a
        // ledger that is still outside the retained window. This is a
        // genuine second gap, not a repeat of the first, and must be
        // reported: `gap_reported_at` is keyed on the cursor it last fired
        // for, not a bare "has a gap fired yet" flag, so a *different*
        // stale cursor fires again rather than being swallowed for the
        // rest of the process.
        store
            .set_cursor(
                &events_cursor(POOL),
                &Cursor {
                    ledger: 250_000,
                    paging_token: None,
                },
            )
            .await
            .expect("the cursor moves");
        rpc.expect("getHealth", health(500_000, 400_000));
        rpc.expect("getLatestLedger", latest(500_000, 1_788_645_403));
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 500_000, "cursor": "400000-1",
                   "events": [borrow_event(400_001, 1), borrow_event(400_001, 2)]}),
        );
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 500_000, "cursor": "400000-1",
                   "events": [borrow_event(400_001, 3), borrow_event(400_001, 4)]}),
        );

        let (third, seen_third) = poll_draining(&mut poller, &sender, &mut receiver, true).await;
        assert_eq!(third.expect("third pass"), None, "the pass did not drain");
        assert_eq!(
            seen_third
                .iter()
                .filter(|message| matches!(message, Seen::Gap { .. }))
                .count(),
            1,
            "a cursor that moved and then fell stale again reports a fresh \
             gap rather than staying silent for the rest of the process: \
             {seen_third:?}"
        );
        Ok(())
    }

    /// Waiting for a tracker that is busy — a gap reseed, a full scan —
    /// is the poller being alive, not stuck: the wait heartbeats, so
    /// `/livez` cannot answer 503 and have a restart probe kill the bot
    /// in the middle of the very seed it is waiting on. The
    /// acknowledgement itself is untouched: the cursor still moves only
    /// once the tracker has answered.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_wait_for_an_acknowledgement_keeps_heartbeating(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let before_cursor = Cursor {
            ledger: 100,
            paging_token: None,
        };
        store
            .set_cursor(&events_cursor(POOL), &before_cursor)
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(103, 1));
        rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 103, "cursor": "103-1", "events": [borrow_event(101, 1)]}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let metrics = Arc::new(Metrics::new());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let config = PollerConfig {
            poll_interval: Duration::from_millis(5),
            ..config()
        };
        // `poll_once` rather than `run`, so the only heartbeat this test
        // can observe is one the wait itself recorded.
        let mut poller =
            LedgerPoller::new(&client, &store, POOL, config).with_metrics(Arc::clone(&metrics));

        let before = std::time::Instant::now();
        let poll = poller.poll_once(&sender);
        let driver = async {
            let mut held = None;
            while let Some(message) = receiver.recv().await {
                if let PollerMessage::Tick { ack, .. } = message {
                    held = Some(ack);
                    break;
                }
            }
            let ack = held.expect("the pass sent a tick");
            // Five poll intervals of a tracker that has not answered yet.
            tokio::time::sleep(Duration::from_millis(25)).await;
            let at = metrics
                .pool_status(POOL)
                .expect("a status")
                .heartbeat
                .expect("the wait recorded a heartbeat");
            assert!(
                at > before,
                "the heartbeat advanced while the acknowledgement was held"
            );
            assert_eq!(
                store.cursor(&events_cursor(POOL)).await.expect("cursor"),
                Some(before_cursor),
                "the cursor waits for the acknowledgement, heartbeat or not"
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
            let again = metrics
                .pool_status(POOL)
                .expect("a status")
                .heartbeat
                .expect("the wait is still recording");
            assert!(
                again > at,
                "the heartbeat keeps advancing for as long as the wait lasts, rather \
                 than being stamped once when it began"
            );
            ack.send(()).expect("acknowledge the tick");
        };
        let (result, ()) = tokio::join!(poll, driver);
        result.expect("poll").expect("a tick");

        assert_eq!(
            store
                .cursor(&events_cursor(POOL))
                .await
                .expect("cursor")
                .expect("set")
                .ledger,
            103,
            "the acknowledgement, and only it, moved the cursor"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// A streak of failures notifies exactly once — at the threshold, not
    /// at every failure past it — and the pass that succeeds records the
    /// head it read. The notifier's cooldown is zero here so that dedup
    /// cannot be what makes the count one: only the counter's `==` may.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_fifth_consecutive_failure_notifies_rpc_failing_once_and_a_success_resets(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        // Six failed passes, so a counter that notified at every failure
        // past the threshold would notify twice.
        for _ in 0..=RPC_FAILING_AFTER {
            rpc.expect_http("getHealth", 500);
        }
        rpc.expect("getHealth", health(103, 1));
        rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 103, "cursor": "103-1", "events": []}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let recording = Arc::new(RecordingChannel::new(false));
        let notifier = Arc::new(Notifier::new(
            Box::new(Arc::clone(&recording)),
            Duration::ZERO,
        ));
        let metrics = Arc::new(Metrics::new());
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let (flag, watch) = tokio::sync::watch::channel(false);
        let config = PollerConfig {
            // Long enough that the pass after the healthy one never starts
            // before the flag below stops the loop.
            poll_interval: Duration::from_millis(500),
            min_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            ..config()
        };
        let mut poller = LedgerPoller::new(&client, &store, POOL, config)
            .with_metrics(Arc::clone(&metrics))
            .with_notifier(Arc::clone(&notifier));

        let run = poller.run(sender, watch);
        let drain = async {
            while let Some(message) = receiver.recv().await {
                let _ = record(message, true);
            }
        };
        // The healthy pass is the only one that reaches `getEvents`, so
        // its call is what says the run has done what the test scripted.
        let stop = async {
            for _ in 0..2_000 {
                if !rpc.calls("getEvents").is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            flag.send(true).expect("flag");
        };
        let (result, (), ()) = tokio::join!(run, drain, stop);
        result.expect("run");

        assert!(notifier.drain(Duration::from_secs(5)).await);
        let sent = recording.sent();
        assert_eq!(
            sent.len(),
            1,
            "the threshold is crossed once per streak: {sent:?}"
        );
        assert_eq!(sent[0].kind, NotificationKind::RpcFailing);
        assert_eq!(sent[0].severity, Severity::High);
        assert_eq!(sent[0].pool, POOL);
        assert_eq!(sent[0].account, None);
        assert!(
            sent[0].message.starts_with(&format!(
                "{RPC_FAILING_AFTER} consecutive poll failures; last: "
            )),
            "the message names the streak and the failure: {}",
            sent[0].message
        );
        assert_eq!(
            metrics.pool_status(POOL).expect("a status").head,
            Some(103),
            "the pass that read a head recorded it"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    /// The heartbeat is the loop's own liveness, not the pass's: it is
    /// recorded before anything can fail, which is what lets `/livez` tell
    /// an RPC outage from a poller that has stopped.
    #[sqlx::test(migrations = "./migrations")]
    async fn every_iteration_records_a_heartbeat(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(103, 1));
        rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        rpc.expect(
            "getEvents",
            json!({"latestLedger": 103, "cursor": "103-1", "events": []}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let metrics = Arc::new(Metrics::new());
        assert!(
            metrics.pool_status(POOL).is_none(),
            "nothing has been recorded for this pool yet"
        );
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let (flag, watch) = tokio::sync::watch::channel(false);
        let config = PollerConfig {
            poll_interval: Duration::from_millis(500),
            ..config()
        };
        let mut poller =
            LedgerPoller::new(&client, &store, POOL, config).with_metrics(Arc::clone(&metrics));

        let run = poller.run(sender, watch);
        let drain = async {
            while let Some(message) = receiver.recv().await {
                let _ = record(message, true);
            }
        };
        let stop = async {
            for _ in 0..2_000 {
                if !rpc.calls("getEvents").is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            flag.send(true).expect("flag");
        };
        let (result, (), ()) = tokio::join!(run, drain, stop);
        result.expect("run");

        assert!(
            metrics
                .pool_status(POOL)
                .expect("a status")
                .heartbeat
                .is_some(),
            "the iteration that polled recorded a heartbeat"
        );
        assert_eq!(rpc.remaining(), 0);
        Ok(())
    }

    #[test]
    fn liveness_deadline_is_five_intervals_plus_the_max_backoff() {
        let config = PollerConfig::new(Duration::from_secs(1));
        assert_eq!(config.liveness_deadline(), Duration::from_secs(35));
    }
}
