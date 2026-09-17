//! `/healthz`, `/livez` and `/metrics` over HTTP — the operator-facing
//! surface spec §7 describes, served only when [`crate::config::HttpConfig`]
//! is configured (a `PORT` or `HTTP_PORT`).
//!
//! Spec §8: "notifier and metrics failures never affect trading." Nothing
//! in this module decides, submits, or otherwise touches a pool: it only
//! reads [`crate::metrics::Metrics`] and pings [`crate::store::Store`], so a
//! bind failure, a stuck connection or a slow client can cost this server
//! and nothing else. [`serve`] never propagates a bind failure to its
//! caller — it logs the address and the error and returns — because a
//! failure to open a diagnostics port must not stop the poller, the
//! auctioneer or the filler from running.
//!
//! **`/healthz` is readiness, `/livez` is liveness, and they answer
//! different questions.** [`readiness`] checks that every configured pool
//! has processed at least one ledger, that a chain head has been read for
//! it within [`HttpState::liveness_deadline`], that its processed ledger
//! is within [`HttpState::max_lag_ledgers`] of that head, and that the
//! store answers a ping inside [`PING_TIMEOUT`] — it fails during an
//! ordinary RPC hiccup or a lagging store, conditions the poller's own
//! backoff already recovers from without help. The head's *age* is what
//! makes the first of those true: both ledger gauges are written by this
//! process, an RPC outage stops both at once, and a readiness that
//! compared only the two would stay green throughout it. [`liveness`]
//! checks only that every pool's poller has heartbeated within
//! [`HttpState::liveness_deadline`] (which itself absorbs one worst-case
//! backoff — see [`crate::ledger::PollerConfig::liveness_deadline`]),
//! measuring a pool that has never heartbeated from
//! [`HttpState::started`] — it fails only once a poller has genuinely
//! stopped making progress.
//! **A deployment's restart probe must target `/livez`, never `/healthz`**:
//! restarting on every readiness blip would kill and respawn the process on
//! exactly the RPC outages its backoff is designed to ride out, while a
//! wedged poller is precisely what a restart can fix.
//!
//! `/metrics` never fails: it renders whatever [`crate::metrics::Metrics`]
//! holds, empty or not, and answers `200` unconditionally.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, HeaderName, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use tokio::sync::watch;

use crate::metrics::Metrics;
use crate::store::Store;

/// Everything a handler needs to answer a request. Built once by the
/// caller (`crate::service::Service::run`) and shared behind an `Arc` for
/// the life of the server.
pub struct HttpState {
    /// The run's counters and gauges.
    pub metrics: Arc<Metrics>,
    /// The store, pinged by [`readiness`] only.
    pub store: Store,
    /// Every configured pool's address, in configuration order.
    /// [`readiness`] and [`liveness`] check them in this order and report
    /// the first failure, so this order decides which pool's problem an
    /// operator sees first when more than one is behind or silent.
    pub pools: Vec<String>,
    /// [`readiness`]'s lag bound: see
    /// [`crate::config::HttpConfig::max_lag_ledgers`].
    pub max_lag_ledgers: u32,
    /// [`liveness`]'s bound, and [`readiness`]'s bound on the age of the
    /// chain head: see [`crate::ledger::PollerConfig::liveness_deadline`].
    pub liveness_deadline: Duration,
    /// When the run started, which is [`liveness`]'s baseline for a pool
    /// that has never heartbeated: until [`HttpState::liveness_deadline`]
    /// has passed since this, silence is a poller still starting — the
    /// initial seed of a busy pool is tens of seconds — rather than one
    /// that has stopped. The same rule `crate::service::watchdog_loop`
    /// applies, and a restart probe that read the two differently would
    /// kill the process in the middle of the seed it then restarts.
    pub started: Instant,
}

/// The router: `/healthz`, `/livez`, `/metrics`. Anything else falls
/// through to axum's own `404`.
pub fn router(state: Arc<HttpState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(livez))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}

/// Binds `bind` and serves [`router`] until `shutdown` flips. A bind
/// failure is logged and never propagated — see the module doc: a
/// diagnostics port that cannot open must not stop the bot from trading.
pub async fn serve(bind: SocketAddr, state: Arc<HttpState>, shutdown: watch::Receiver<bool>) {
    match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => serve_on(listener, state, shutdown).await,
        Err(error) => {
            tracing::error!(%bind, %error, "http server failed to bind; healthz/livez/metrics are unavailable for this run");
        }
    }
}

/// Serves [`router`] on an already-bound `listener` until `shutdown`
/// flips. Split from [`serve`] so a test can bind an ephemeral port
/// itself, read the bound address back, and drive the server directly.
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    state: Arc<HttpState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let app = router(state);
    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
    {
        tracing::error!(%error, "http server exited with an error");
    }
}

/// How long [`readiness`]'s store ping may take before it is a failure
/// rather than an answer.
///
/// A saturated pool makes `ping` wait out sqlx's own acquire timeout —
/// tens of seconds — and a probe that hangs is a probe that times out:
/// the prober calls it a failure either way, so this makes the endpoint
/// *say* so, in the body, rather than leave the operator reading a
/// timeout from their load balancer's logs.
pub const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// `/healthz`'s answer: `Ok` when every pool in [`HttpState::pools`] has
/// processed at least one ledger, has had a chain head read for it within
/// [`HttpState::liveness_deadline`], and is within
/// [`HttpState::max_lag_ledgers`] of that head — and the store answers a
/// ping inside [`PING_TIMEOUT`]. Otherwise the first failure, which is
/// also the `503` body. Checked in configuration order, so the earliest
/// pool with a problem is what a caller sees.
///
/// **A failed ping says only that it failed.** `StoreError::Query`
/// renders sqlx's own text — the role, the database name, a TLS error
/// naming an internal hostname — and this body reaches anyone who can
/// reach the port, which on a hosted runtime is whatever `0.0.0.0` means
/// there. So the cause is logged and the body is fixed text, the same
/// treatment `StoreError::Connect` already gives its own cause. The
/// timeout body stays as it is: it names a budget, not a server.
///
/// **The head's age is a rule, not a nicety.** Both ledger gauges are
/// this process's own: `ledger_head` moves only when a pass actually read
/// a head, and `ledger_processed` only when a tick was acknowledged. An
/// RPC outage stops both, leaving a lag frozen wherever it was — so a
/// readiness that compared only the two would answer `200` for the whole
/// of the one failure it exists to catch. `now` is what tells that apart
/// from a bot genuinely at chain head, which is why this signature takes
/// a clock.
pub async fn readiness(state: &HttpState, now: Instant) -> Result<(), String> {
    let max_lag_ledgers = state.max_lag_ledgers;
    let limit = state.liveness_deadline.as_secs();
    for pool in &state.pools {
        let status = state.metrics.pool_status(pool);
        let Some(processed) = status.and_then(|status| status.processed) else {
            return Err(format!("no ledger processed yet for {pool}"));
        };
        let Some(head) = status.and_then(|status| status.head) else {
            return Err(format!("no chain head observed yet for {pool}"));
        };
        // Recorded with the head and never apart from it, so this is
        // `Some` wherever `head` is; a `None` is read as "just now"
        // rather than invented as a failure of its own.
        if let Some(head_at) = status.and_then(|status| status.head_at) {
            let age = now.saturating_duration_since(head_at);
            if age > state.liveness_deadline {
                let secs = age.as_secs();
                return Err(format!(
                    "{pool}: no chain head read for {secs}s (limit {limit}s)"
                ));
            }
        }
        let lag = head.saturating_sub(processed);
        if lag > max_lag_ledgers {
            return Err(format!(
                "{pool} is {lag} ledgers behind head (limit {max_lag_ledgers})"
            ));
        }
    }
    match tokio::time::timeout(PING_TIMEOUT, state.store.ping()).await {
        // The cause goes to the log, never into the body: see the doc
        // above.
        Ok(result) => result.map_err(|error| {
            tracing::warn!(%error, "readiness ping failed");
            "store: ping failed".to_string()
        }),
        Err(_) => Err(format!(
            "store: ping timed out after {}s",
            PING_TIMEOUT.as_secs()
        )),
    }
}

/// `/livez`'s answer: `Ok` when every pool in [`HttpState::pools`] has
/// heartbeated within [`HttpState::liveness_deadline`] of `now`; otherwise
/// the first pool that has not, which is also the `503` body. Checked in
/// configuration order, like [`readiness`].
///
/// A pool that has *never* heartbeated is measured from
/// [`HttpState::started`] instead, and is alive until the same deadline
/// has passed since then: a poller whose first iteration has not run yet
/// — the run seeds every pool that needs it before the first pass, which
/// on a busy pool is tens of seconds — has not stopped, and a restart
/// probe that read it as dead would kill the process in the middle of the
/// seed it would then start over. `crate::service::watchdog_loop` reads a
/// missing heartbeat the same way.
pub fn liveness(state: &HttpState, now: Instant) -> Result<(), String> {
    let limit = state.liveness_deadline.as_secs();
    for pool in &state.pools {
        let heartbeat = state
            .metrics
            .pool_status(pool)
            .and_then(|status| status.heartbeat);
        let Some(at) = heartbeat else {
            let age = now.saturating_duration_since(state.started);
            if age > state.liveness_deadline {
                let secs = age.as_secs();
                return Err(format!(
                    "{pool}'s poller has not run yet, {secs}s after start (limit {limit}s)"
                ));
            }
            continue;
        };
        let age = now.saturating_duration_since(at);
        if age > state.liveness_deadline {
            let secs = age.as_secs();
            return Err(format!("{pool}'s poller last ran {secs}s ago"));
        }
    }
    Ok(())
}

/// A `text/plain` response with the given status and body.
fn text_response(
    status: StatusCode,
    body: String,
) -> (StatusCode, [(HeaderName, &'static str); 1], String) {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
}

async fn healthz(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    match readiness(&state, Instant::now()).await {
        Ok(()) => text_response(StatusCode::OK, "ok".to_string()),
        Err(reason) => text_response(StatusCode::SERVICE_UNAVAILABLE, reason),
    }
}

async fn livez(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    match liveness(&state, Instant::now()) {
        Ok(()) => text_response(StatusCode::OK, "ok".to_string()),
        Err(reason) => text_response(StatusCode::SERVICE_UNAVAILABLE, reason),
    }
}

async fn metrics_handler(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`Store`] that never connects: valid for every test in this
    /// module except the readiness ones, which need a real ping.
    /// `connect_lazy` builds the pool synchronously and dials nothing
    /// until a query actually runs.
    fn disconnected_store() -> Store {
        Store::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://unused/unused")
                .expect("a lazy pool never dials, so this cannot fail on a bad host"),
        )
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn readiness_needs_every_pool_within_the_lag_and_a_store_that_answers(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let metrics = Arc::new(Metrics::new());
        let now = Instant::now();
        let state = HttpState {
            metrics: Arc::clone(&metrics),
            store,
            pools: vec!["A".into(), "B".into()],
            max_lag_ledgers: 10,
            liveness_deadline: Duration::from_secs(35),
            started: now,
        };
        assert_eq!(
            readiness(&state, now).await,
            Err("no ledger processed yet for A".into())
        );
        metrics.ledger_head_at("A", 100, now);
        metrics.ledger_processed("A", 95);
        assert_eq!(
            readiness(&state, now).await,
            Err("no ledger processed yet for B".into())
        );
        metrics.ledger_head_at("B", 100, now);
        metrics.ledger_processed("B", 80);
        assert_eq!(
            readiness(&state, now).await,
            Err("B is 20 ledgers behind head (limit 10)".into())
        );
        metrics.ledger_processed("B", 90);
        assert_eq!(readiness(&state, now).await, Ok(()));

        // The ping is a rule of its own, and a store that refuses is the
        // one failure no gauge can report: without this the ping could be
        // deleted and every assertion above would still hold.
        state.store.pool().close().await;
        let refused = readiness(&state, now)
            .await
            .expect_err("a closed pool cannot answer a ping");
        assert!(
            refused.starts_with("store: "),
            "the store's failure is named as the store's: {refused}"
        );
        assert!(
            !refused.contains("database"),
            "and says only that, never sqlx's own text: the body reaches anyone who can \
             reach the port, and this one is bound to 0.0.0.0 on a hosted runtime, so the \
             role, the database name and any hostname in the cause stay in the log: \
             {refused}"
        );
        Ok(())
    }

    /// Readiness compares two *internal* gauges, and an RPC outage stops
    /// both: `ledger_head` is written only by a pass that read a head and
    /// `ledger_processed` only by an acknowledged tick, so a frozen lag is
    /// exactly what the bot following nothing looks like. The head's age
    /// is what tells the two apart.
    #[sqlx::test(migrations = "./migrations")]
    async fn readiness_fails_once_no_chain_head_has_been_read_for_the_deadline(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let metrics = Arc::new(Metrics::new());
        let now = Instant::now();
        let state = HttpState {
            metrics: Arc::clone(&metrics),
            store,
            pools: vec!["A".into()],
            max_lag_ledgers: 10,
            liveness_deadline: Duration::from_secs(35),
            started: now,
        };
        metrics.ledger_head_at("A", 100, now);
        metrics.ledger_processed("A", 100);
        assert_eq!(readiness(&state, now).await, Ok(()));

        let stale = now + Duration::from_secs(36);
        assert_eq!(
            readiness(&state, stale).await,
            Err("A: no chain head read for 36s (limit 35s)".into()),
            "a head nobody has read since the deadline is not a ready bot, however \
             small the lag between two gauges that both stopped moving"
        );
        Ok(())
    }

    /// A store that accepts the connection and then never speaks is what
    /// a saturated pool looks like to a prober: `ping` would wait out
    /// sqlx's own acquire timeout, tens of seconds, and the endpoint
    /// would hang rather than answer. This test spends
    /// [`PING_TIMEOUT`] of real time on purpose — the bound is wall-clock,
    /// and `tokio::time::pause` is not available to this crate — but it
    /// spends it concurrently with the rest of the suite.
    #[tokio::test]
    async fn readiness_answers_rather_than_hanging_on_a_store_that_never_replies() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a listener that answers nothing");
        let port = listener.local_addr().expect("read the port back").port();
        let accepting = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });

        let store = Store::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy(&format!("postgres://unused:unused@127.0.0.1:{port}/unused"))
                .expect("a lazy pool never dials, so this cannot fail here"),
        );
        let state = HttpState {
            metrics: Arc::new(Metrics::new()),
            store,
            pools: Vec::new(),
            max_lag_ledgers: 10,
            liveness_deadline: Duration::from_secs(35),
            started: Instant::now(),
        };

        assert_eq!(
            readiness(&state, Instant::now()).await,
            Err("store: ping timed out after 5s".into()),
            "the probe says what is wrong instead of waiting for the store to"
        );
        accepting.abort();
    }

    // `#[tokio::test]`, not a plain `#[test]`: `liveness` itself is sync,
    // but building a `Store` even over a `connect_lazy` pool spawns sqlx's
    // background maintenance task, which needs a Tokio context to exist at
    // all — this test never awaits anything past that setup.
    #[tokio::test]
    async fn liveness_is_the_heartbeat_within_the_deadline() {
        let metrics = Arc::new(Metrics::new());
        let now = Instant::now();
        let state = HttpState {
            metrics: Arc::clone(&metrics),
            store: disconnected_store(),
            pools: vec!["A".into()],
            max_lag_ledgers: 10,
            liveness_deadline: Duration::from_secs(35),
            started: now,
        };

        // Not yet run is not dead — `watchdog_loop` reads it the same way:
        // a poller seeding a busy pool has not had its first iteration.
        assert_eq!(liveness(&state, now), Ok(()));
        assert_eq!(
            liveness(&state, now + Duration::from_secs(34)),
            Ok(()),
            "still inside the startup baseline"
        );
        assert_eq!(
            liveness(&state, now + Duration::from_secs(36)),
            Err("A's poller has not run yet, 36s after start (limit 35s)".into()),
            "past it, silence is a poller that never started"
        );

        metrics.heartbeat_at("A", now, std::time::SystemTime::now());
        assert_eq!(liveness(&state, now), Ok(()));

        let after_deadline = now + Duration::from_secs(36);
        assert_eq!(
            liveness(&state, after_deadline),
            Err("A's poller last ran 36s ago".into())
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_three_routes_answer_over_a_real_socket_and_stop_on_shutdown(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let metrics = Arc::new(Metrics::new());
        let state = Arc::new(HttpState {
            metrics: Arc::clone(&metrics),
            store,
            pools: vec!["A".into()],
            max_lag_ledgers: 10,
            liveness_deadline: Duration::from_secs(35),
            // Long enough ago that the startup baseline has passed, so a
            // pool with no heartbeat is the "never started" failure this
            // test reads over the socket rather than a bot still starting.
            started: Instant::now()
                .checked_sub(Duration::from_mins(1))
                .expect("a minute ago"),
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("read the bound port back");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let server = tokio::spawn(serve_on(listener, Arc::clone(&state), shutdown_rx));

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        let response = client
            .get(format!("{base}/livez"))
            .send()
            .await
            .expect("livez request");
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let body = response.text().await.expect("livez body");
        assert!(
            body.starts_with("A's poller has not run yet,"),
            "the body is the reason, and the reason names the pool: {body}"
        );

        metrics.heartbeat("A");
        let response = client
            .get(format!("{base}/livez"))
            .send()
            .await
            .expect("livez request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.expect("livez body"), "ok");

        let response = client
            .get(format!("{base}/metrics"))
            .send()
            .await
            .expect("metrics request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .expect("content-type header")
            .to_str()
            .expect("ascii content-type");
        assert!(content_type.starts_with("text/plain; version=0.0.4"));
        let body = response.text().await.expect("metrics body");
        assert!(body.contains("# TYPE blend_liquidator_ledger_head gauge"));

        let response = client
            .get(format!("{base}/nope"))
            .send()
            .await
            .expect("404 request");
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

        shutdown_tx.send(true).expect("flip the shutdown watch");
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("serve_on stops within 5s of the shutdown flip")
            .expect("serve_on task did not panic");

        Ok(())
    }

    #[tokio::test]
    async fn a_bind_failure_is_logged_and_returns() {
        let blocking = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the first listener");
        let addr = blocking.local_addr().expect("read the bound address back");

        let metrics = Arc::new(Metrics::new());
        let state = Arc::new(HttpState {
            metrics,
            store: disconnected_store(),
            pools: Vec::new(),
            max_lag_ledgers: 10,
            liveness_deadline: Duration::from_secs(35),
            started: Instant::now(),
        });
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        tokio::time::timeout(Duration::from_secs(1), serve(addr, state, shutdown_rx))
            .await
            .expect("serve returns promptly rather than panicking on a bind failure");

        drop(blocking);
    }
}
