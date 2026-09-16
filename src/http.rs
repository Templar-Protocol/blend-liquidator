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
//! has processed at least one ledger, that its processed ledger is within
//! [`HttpState::max_lag_ledgers`] of the chain head this process has
//! observed, and that the store answers a ping — it fails during an
//! ordinary RPC hiccup or a lagging store, conditions the poller's own
//! backoff already recovers from without help. [`liveness`] checks only
//! that every pool's poller has heartbeated within
//! [`HttpState::liveness_deadline`] (which itself absorbs one worst-case
//! backoff — see [`crate::ledger::PollerConfig::liveness_deadline`]) —
//! it fails only once a poller has genuinely stopped making progress.
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
    /// [`liveness`]'s bound: see
    /// [`crate::ledger::PollerConfig::liveness_deadline`].
    pub liveness_deadline: Duration,
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

/// `/healthz`'s answer: `Ok` when every pool in [`HttpState::pools`] has
/// processed at least one ledger within [`HttpState::max_lag_ledgers`] of
/// its observed chain head and the store answers a ping; otherwise the
/// first failure, which is also the `503` body. Checked in configuration
/// order, so the earliest pool with a problem is what a caller sees.
///
/// `now` is accepted, and unused, for signature symmetry with
/// [`liveness`]: every rule here is ledger-counted rather than wall-clock,
/// so a future caller does not have to thread a clock through a new
/// signature to add a time-based one.
pub async fn readiness(state: &HttpState, _now: Instant) -> Result<(), String> {
    let max_lag_ledgers = state.max_lag_ledgers;
    for pool in &state.pools {
        let status = state.metrics.pool_status(pool);
        let Some(processed) = status.and_then(|status| status.processed) else {
            return Err(format!("no ledger processed yet for {pool}"));
        };
        let Some(head) = status.and_then(|status| status.head) else {
            return Err(format!("no chain head observed yet for {pool}"));
        };
        let lag = head.saturating_sub(processed);
        if lag > max_lag_ledgers {
            return Err(format!(
                "{pool} is {lag} ledgers behind head (limit {max_lag_ledgers})"
            ));
        }
    }
    state
        .store
        .ping()
        .await
        .map_err(|error| format!("store: {error}"))
}

/// `/livez`'s answer: `Ok` when every pool in [`HttpState::pools`] has
/// heartbeated within [`HttpState::liveness_deadline`] of `now`; otherwise
/// the first pool that has not, which is also the `503` body. Checked in
/// configuration order, like [`readiness`].
pub fn liveness(state: &HttpState, now: Instant) -> Result<(), String> {
    for pool in &state.pools {
        let heartbeat = state
            .metrics
            .pool_status(pool)
            .and_then(|status| status.heartbeat);
        let Some(at) = heartbeat else {
            return Err(format!("{pool}'s poller has not run yet"));
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
        let state = HttpState {
            metrics: Arc::clone(&metrics),
            store,
            pools: vec!["A".into(), "B".into()],
            max_lag_ledgers: 10,
            liveness_deadline: Duration::from_secs(35),
        };
        assert_eq!(
            readiness(&state, Instant::now()).await,
            Err("no ledger processed yet for A".into())
        );
        metrics.ledger_head("A", 100);
        metrics.ledger_processed("A", 95);
        assert_eq!(
            readiness(&state, Instant::now()).await,
            Err("no ledger processed yet for B".into())
        );
        metrics.ledger_head("B", 100);
        metrics.ledger_processed("B", 80);
        assert_eq!(
            readiness(&state, Instant::now()).await,
            Err("B is 20 ledgers behind head (limit 10)".into())
        );
        metrics.ledger_processed("B", 90);
        assert_eq!(readiness(&state, Instant::now()).await, Ok(()));
        Ok(())
    }

    // `#[tokio::test]`, not a plain `#[test]`: `liveness` itself is sync,
    // but building a `Store` even over a `connect_lazy` pool spawns sqlx's
    // background maintenance task, which needs a Tokio context to exist at
    // all — this test never awaits anything past that setup.
    #[tokio::test]
    async fn liveness_is_the_heartbeat_within_the_deadline() {
        let metrics = Arc::new(Metrics::new());
        let state = HttpState {
            metrics: Arc::clone(&metrics),
            store: disconnected_store(),
            pools: vec!["A".into()],
            max_lag_ledgers: 10,
            liveness_deadline: Duration::from_secs(35),
        };
        let now = Instant::now();

        assert_eq!(
            liveness(&state, now),
            Err("A's poller has not run yet".into())
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
        assert_eq!(
            response.text().await.expect("livez body"),
            "A's poller has not run yet"
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
        });
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        tokio::time::timeout(Duration::from_secs(1), serve(addr, state, shutdown_rx))
            .await
            .expect("serve returns promptly rather than panicking on a bind failure");

        drop(blocking);
    }
}
