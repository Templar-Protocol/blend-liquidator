# Phase 3: Store, Ledger Poller and Tracker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the bot durable state and a clock: a Postgres store with embedded migrations and typed queries, a ledger poller that streams a pool's events by cursor, and a tracker that applies those events, refreshes borrowers from chain, and seeds the user set from the public analytics API, a static file, or replayed events — so the binary can follow a pool and report the borrowers it is tracking.

**Architecture:** Four new modules plus a lifecycle. `store.rs` owns the schema, the migrations and every query, returning bot types (`i128` amounts, `u32` ledgers) rather than SQL types. `ledger.rs` is the clock: it polls chain head, reads each ledger's pool events through `chain::rpc`, persists its cursor, and emits events followed by a tick over a channel. `tracker.rs` consumes that channel: it applies each event to the store, refreshes the users an event names from chain through `chain::pool`, and seeds or replays when the cursor is missing or has fallen out of the RPC's retained window. `service.rs` wires them: migrate, validate, seed, spawn, shut down. Nothing trades; the phase's output is a followed pool and a list of tracked borrowers.

**Tech Stack:** Rust 1.97 (pinned three ways), `sqlx` 0.9 (`postgres`, `runtime-tokio`, `tls-rustls-ring-native-roots`, `macros`, `migrate`, `json`) with compile-time checked queries and committed offline metadata, `toml` 1.1 for the pools file, `rand` 0.10 for cadence phase offsets, plus Phase 2's `reqwest`, `serde`, `serde_json`, `tokio`, `tracing`. Postgres 17 in development, CI and tests.

**Spec:** `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`, sections 2 (process shape, module map, data flow), 4 (store, discovery and refresh, cadences), 6 (configuration, pools file, startup validation), 8 (error handling: poller, tracker, store), 9 (testing), 12 (delivery, phase 3). The spec is the authority; this plan argues from it.

## Global Constraints

- The three-way Rust pin stays at 1.97.0 (`Cargo.toml` `rust-version`, `rust-toolchain.toml`, Dockerfile `FROM rust:1.97.0-bookworm`); do not touch any of the three.
- `clippy::pedantic` is warn-level and CI runs `cargo clippy --all-targets -- -D warnings`, so every pedantic finding is an error in every target including tests and examples. `unwrap_used` is denied outside tests; `expect_used` warns, which is also an error in CI outside tests (`clippy.toml` exempts tests only; `examples/` gets no exemption).
- Numeric literals use 3-digit `_` grouping. No `as` numeric casts: use `u32::try_from`, `i64::from`, `i16::try_from`. No `f64` for money.
- Arithmetic on chain-sourced values is checked or carries a proof comment; never a silent saturation.
- Doc comments state constraints and invariants, not narration. `tracing` in the crate, `println!` only in `examples/`.
- Secrets never touch argv, a `Debug` rendering or a log line. `DATABASE_URL` may carry a password: it is read from the environment only, wrapped in `config::Secret`, and never logged.
- `cargo deny check` must pass. The dependency set and feature flags in Task 1 are the ones verified to pass it; do not widen them.
- `make check` (fmt, clippy, tests, docs with `-D warnings`, invariants script, shellcheck) must be green at the end of every task before committing. From this phase on it needs a running Postgres: `make db-up` first.
- Commit messages follow the repository's conventional style and end with the trailer `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`.
- Work on branch `phase-3/store-and-tracker`; open one pull request for the whole phase at the end. Never push or open the pull request without the user.
- The dev container is memory-constrained: if cargo is killed with `signal: 9`, rerun with `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.

## Rulings

Decisions this plan makes where the spec is silent or where following its letter needs a mechanism it does not name. Each was verified against a live Postgres 17 and the live analytics API on 2026-09-09. An implementer follows them; a reviewer checks them against the spec.

1. **Compile-time checked queries, with the offline metadata committed.** The spec asks for compile-time checked queries; the Dockerfile builds the binary with no database in reach. Both hold together through sqlx's offline mode: `cargo sqlx prepare` writes `.sqlx/`, which is committed, and the Dockerfile sets `SQLX_OFFLINE=true`. Verified: with the database stopped and `DATABASE_URL` unset, `SQLX_OFFLINE=true cargo build` succeeds against `.sqlx/`. CI's `lint-test` job gets a Postgres service so the macros are checked against a live schema, and a `cargo sqlx prepare --check` step proves the committed metadata is current.
2. **`i128` travels through `numeric` as text.** Amounts exceed `bigint` and health factors exceed it by far. Rather than add `bigdecimal` or `rust_decimal`, every write binds a decimal string and casts (`$3::text::numeric`) and every read casts back (`health_factor::text`). Verified round-tripping 10_070_767 and larger. This is the same choice the spec already makes for jsonb amounts ("stored as decimal text").
3. **Migration 0001 creates `cursors`, `users` and `auctions` only.** The spec's table list also has `fills` and `creations`; nothing in this phase writes them. They arrive in the migration that lands with the phase that writes them (creations in Phase 4, fills in Phase 5), so no table exists without a writer.
4. **Store tests use `#[sqlx::test]`.** It creates a fresh database per test, runs `migrations/` into it, hands the test a `PgPool`, and drops it afterwards. Verified, including that no test databases are left behind. Tests are never skipped when the database is absent: they fail, because a store test that silently passes without a database asserts nothing.
5. **`SeedSource` is an enum, not a trait object.** The spec names it a seam. An async trait needs boxing (`async-trait` or hand-rolled `Pin<Box<dyn Future>>`) to be dyn-compatible, and the implementations are known at compile time; an enum with an inherent `async fn accounts` is the same seam without the dependency. Adding a third source is adding a variant.
6. **Phase 3's startup validation is what a bot with no signer can check.** The spec's list also covers the filler account's XLM balance and per-pool collateral, which need `FILLER_SECRET_KEY` and belong with Phase 5's executor. This phase validates: the pools file parses, every pool loads from chain, all pools share one backstop, each pool's primary asset is an enabled reserve with a positive collateral factor, and every explicitly listed supported asset is a reserve. `check-config` runs exactly that and exits 0 or 2.
7. **A gap or a failed seed logs and counts; it does not notify.** The spec routes both to Telegram, but `notifier.rs` is Phase 6. Every place the spec says "notify", this phase emits a `tracing::warn!` carrying the same fields, and the Phase 6 plan replaces the call site. No placeholder trait ships.
8. **The poller owns one pool per task.** The spec's cursor is per task; running one poller task per pool keeps a slow or failing pool from stalling the others and makes the cursor name (`events:{pool}`) obvious. The tracker is a single task consuming one merged channel, because it owns the store's write ordering.
9. **`LedgerTick` carries the close time, and every valuation uses it.** A tick's close time is the timestamp the tracker accrues reserves to when it recomputes a health factor, so a user refreshed on ledger *N* is valued exactly as the contract would value it in ledger *N*. Refreshes triggered between ticks use the last tick's close time, never the wall clock.
10. **Replay is the poller, not a second code path.** The spec lists replaying retained events as the third seeding mechanism. It needs no separate implementation: a `Gap` restarts the poller at `oldestLedger` and it walks forward to head through the same loop that follows the chain normally, while the tracker reseeds alongside it. A second replay path would be the same code with a different name and its own bugs.
11. **Phase 3 carries the refresh and full-scan cadences; the oracle scan is Phase 4's.** The refresh pass and the full scan need only what this phase builds. The oracle scan is defined by a price move since the last significant price, which needs the price history the auctioneer keeps, so `ORACLE_SCAN_LEDGERS`, `PRICE_DELTA_BPS`, `LIQ_HF_THRESHOLD` and `TARGET_HF` arrive with it.
12. **Health factors are stored normalised to 7 decimals**, as `hf × 10^7 / oracle_scalar` computed with `math::mul_floor`. The oracle's own scalar varies per pool; the store's ordering and thresholds must not.

---

## File Structure

| Path | Responsibility |
|---|---|
| `Cargo.toml` (modify) | add `sqlx`, `toml`, `rand` |
| `migrations/0001_initial.sql` (create) | `cursors`, `users`, `auctions` |
| `.sqlx/` (create, committed) | offline query metadata, generated by `cargo sqlx prepare` |
| `src/store.rs` (create) | `Store`, `StoreError`, migrations, cursor/user/auction queries, jsonb and numeric conversions |
| `src/ledger.rs` (create) | `LedgerTick`, `PollerMessage`, `LedgerPoller`, cursor persistence, backoff, gap detection |
| `src/tracker.rs` (create) | `Tracker`, event application, user refresh, `SeedSource` (analytics and file), replay |
| `src/service.rs` (create) | lifecycle: migrate, validate, seed, spawn, shut down; `check-config` |
| `src/config.rs` (modify) | pools file, store and cadence knobs, `ServiceConfig`, `RunMode` |
| `src/liquidator.rs` (modify) | declare the modules; `LiquidatorError` gains `Store`, `Ledger`, `Tracker` |
| `src/main.rs` (modify) | build the configuration, dispatch on run mode, exit codes |
| `docker-compose.yml` (modify) | a `postgres` service the bot depends on |
| `Makefile` (modify) | `db-up`, `db-down`, `db-migrate`, `sqlx-prepare`; `check` documents the database it needs |
| `.github/workflows/ci.yml` (modify) | Postgres service and `DATABASE_URL` for `lint-test`, plus `cargo sqlx prepare --check` |
| `Dockerfile` (modify) | `SQLX_OFFLINE=true`, copy `.sqlx` and `migrations` |
| `.env.example`, `CLAUDE.md`, `CHANGELOG.md` (modify) | the new knobs, the module map, the database gotchas, the entries |

## Facts the code is written against

Verified on 2026-09-09 in this dev container against Postgres 17.11 and `https://api.blend.templarfi.org`.

**sqlx 0.9 workflow.** `sqlx::migrate!("./migrations")` embeds the directory in the binary and takes a Postgres advisory lock by default (`Migrator::locking` defaults to `true`, so the spec's advisory-lock requirement needs no extra code). `cargo sqlx prepare -- --lib --bins` writes one JSON per query into `.sqlx/`; `cargo sqlx prepare --check -- --lib --bins` fails when the committed metadata is stale. `SQLX_OFFLINE=true` makes the macros read `.sqlx/` and never touch a database. `sqlx-cli` 0.9.0 is already installed in this dev container (`sqlx --version`).

**`#[sqlx::test]`.** An `async fn` taking `pool: sqlx::PgPool` and returning `sqlx::Result<()>`. The macro creates a fresh database, applies `migrations/`, passes the pool, and drops the database when the test ends. It needs `DATABASE_URL` pointing at a server whose role may create databases; the Postgres container's default role may.

**`numeric` and `jsonb` round trips.** `INSERT ... VALUES ($1::text::numeric)` bound with `i128::to_string()`, read back with `SELECT health_factor::text AS hf` and `str::parse::<i128>()`. `jsonb` binds and reads as `serde_json::Value`.

**The seed source.** `GET https://api.blend.templarfi.org/v1/analytics/state/positions?healthFactorMax=10&poolId=<C…>&limit=500` answers 200 with

```json
{
  "firstScanAt": "2025-04-14T00:00:00Z",
  "generatedAt": "2026-09-09T01:07:33.693649577Z",
  "snapshotAt": "2026-09-09T00:11:41.921135Z",
  "nextCursor": "MS4wMTQwNDYyNDkzNzI3Mzg3OkNB…",
  "positions": [
    {
      "accountId": "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE",
      "healthFactor": 1.0065787026700665,
      "isLiquidatable": false,
      "poolId": "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD",
      "scanTs": "2026-09-09T00:11:41.921135Z",
      "totalBorrowUsd": 12.8331472,
      "totalCollateralUsd": 14.3130999
    }
  ]
}
```

The next page is the same URL plus `&cursor=<nextCursor>`. **`nextCursor` is `null` on the last page and absent for a pool the API does not know**, which the same `Option<String>` handles; an unknown pool answers 200 with an empty `positions` array. The mainnet pool `CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD` returned 84 positions under `healthFactorMax=10`. Only `accountId` is read: `healthFactor` there is a float from a third party, and every seeded account is re-valued from chain before the bot trusts it.

**Phase 2 interfaces this phase consumes.** `chain::rpc::RpcClient::{health, latest_ledger, events, ledger_entries, simulate}` with `Health { latest_ledger, oldest_ledger, ledger_retention_window, .. }`, `LatestLedger { sequence, close_time, .. }`, `EventQuery { start_ledger, cursor, contract_ids, limit }`, `Events { latest_ledger, cursor, events: Vec<Event> }`, `Event { ledger, id, tx_hash, contract_id, in_successful_contract_call, topics, value }`. `chain::xdr::decode_pool_event(&[ScVal], &ScVal) -> Result<Option<PoolEvent>, XdrError>` and `PoolEvent::affected_accounts`. `chain::pool::{PoolReader, PoolSnapshot}` with `PoolReader::new(&rpc, pool)`, `snapshot(&[&str])`, and `PoolSnapshot { ledger, instance, reserves, asset_index, prices, positions, .. }` plus `position_data(user, close_time) -> Result<Option<PositionData>, ChainError>`. `math::{PositionData, Positions, SCALAR_7, mul_floor}`. `config::{ChainConfig, Secret, NetworkName}`.

---

### Task 1: Postgres, the schema, and the store's connection

**Files:**
- Modify: `Cargo.toml`
- Create: `migrations/0001_initial.sql`
- Create: `src/store.rs`
- Modify: `src/liquidator.rs` (declare `pub mod store;`, extend `LiquidatorError`)
- Modify: `docker-compose.yml`, `Makefile`, `.github/workflows/ci.yml`, `Dockerfile`, `.env.example`
- Create (generated): `.sqlx/`

**Interfaces:**
- Produces:
  - `pub struct store::Store` with `Store::connect(url: &str, max_connections: u32) -> Result<Self, StoreError>`, `Store::from_pool(pool: PgPool) -> Self`, `pub fn pool(&self) -> &PgPool`, `pub async fn migrate(&self) -> Result<(), StoreError>`, `pub async fn ping(&self) -> Result<(), StoreError>`.
  - `pub enum store::StoreError { Connect(sqlx::Error), Query(sqlx::Error), Migrate(sqlx::migrate::MigrateError), Decimal { column: &'static str, value: String }, Json { column: &'static str, detail: String } }`.
  - `LiquidatorError::Store(#[from] store::StoreError)`.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml`'s `[dependencies]`, keeping the table alphabetical, add:

```toml
# Cadence phase offsets, so several bots on one pool do not fire on the
# same ledger.
rand = "0.10"
# Postgres with compile-time checked queries. rustls with the system root
# store and the ring backend: the same TLS posture as `reqwest` above, and
# the feature set whose licence tree `cargo deny` accepts — the aws-lc-rs
# backends are OpenSSL-licensed and `deny.toml` does not allow that.
sqlx = { version = "0.9", default-features = false, features = [
    "postgres",
    "runtime-tokio",
    "tls-rustls-ring-native-roots",
    "macros",
    "migrate",
    "json",
] }
# The pools file.
toml = "1.1"
```

Run: `cargo fetch && cargo deny check`
Expected: `advisories ok, bans ok, licenses ok, sources ok`. If licences fail, the feature set was changed; restore it.

- [ ] **Step 2: Write the migration**

Create `migrations/0001_initial.sql`:

```sql
-- Phase 3's schema: the three tables this phase writes. `creations` and
-- `fills` arrive in the migration that lands with the phase that writes
-- them, so no table exists without a writer.

-- How far a named task has applied. One row per task, e.g. `events:C…`.
CREATE TABLE cursors (
    name          text        PRIMARY KEY,
    ledger        bigint      NOT NULL,
    paging_token  text,
    updated_at    timestamptz NOT NULL DEFAULT now()
);

-- Borrowers the bot tracks. A row exists only while the account has
-- liabilities: an account that repays everything is deleted, not kept with
-- an empty map, so `count(*)` is the number of positions that can be
-- liquidated.
--
-- `health_factor` is numeric because the ratio of collateral to a dust
-- liability exceeds bigint, and it is normalised to 7 decimals
-- (`hf * 10^7 / oracle_scalar`) so pools whose oracles differ in decimals
-- order and compare alike. `collateral` and `liabilities` map a reserve
-- index to a b-token or d-token amount, both as decimal strings, because
-- those amounts exceed what JSON numbers hold exactly.
CREATE TABLE users (
    pool            text    NOT NULL,
    account         text    NOT NULL,
    health_factor   numeric NOT NULL,
    collateral      jsonb   NOT NULL,
    liabilities     jsonb   NOT NULL,
    updated_ledger  bigint  NOT NULL,
    PRIMARY KEY (pool, account)
);

-- The scan that matters: the least healthy borrowers in a pool, first.
CREATE INDEX users_by_health ON users (pool, health_factor);

-- Open auctions and, once the filler plans one, the ledger it intends to
-- fill at. `auction_type` is the contract's discriminant (0 user
-- liquidation, 1 bad debt, 2 interest) and `percent` its 1-to-100 fill
-- percent, both small enough for smallint.
CREATE TABLE auctions (
    pool            text     NOT NULL,
    account         text     NOT NULL,
    auction_type    smallint NOT NULL CHECK (auction_type BETWEEN 0 AND 2),
    start_ledger    bigint   NOT NULL,
    fill_ledger     bigint,
    percent         smallint NOT NULL CHECK (percent BETWEEN 1 AND 100),
    bid             jsonb    NOT NULL,
    lot             jsonb    NOT NULL,
    updated_ledger  bigint   NOT NULL,
    PRIMARY KEY (pool, account, auction_type)
);

-- The filler walks open auctions in the order they became fillable.
CREATE INDEX auctions_by_start ON auctions (pool, start_ledger);
```

- [ ] **Step 3: Give the repository a database**

`docker-compose.yml`, a new service before `liquidator`:

```yaml
  postgres:
    image: postgres:17-alpine
    container_name: blend-liquidator-db

    # The bot's durable state: cursors, tracked borrowers, open auctions.
    # Losing it costs a reseed and a replay, not correctness, but a restart
    # that finds an empty database re-reads every borrower from chain.
    environment:
      - POSTGRES_USER=${POSTGRES_USER:-liquidator}
      - POSTGRES_PASSWORD=${POSTGRES_PASSWORD:-liquidator}
      - POSTGRES_DB=${POSTGRES_DB:-liquidator}

    ports:
      - "${POSTGRES_PORT:-55432}:5432"

    volumes:
      - postgres-data:/var/lib/postgresql/data

    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U ${POSTGRES_USER:-liquidator}"]
      interval: 10s
      timeout: 5s
      retries: 5
      start_period: 10s

    restart: unless-stopped
```

and, on the `liquidator` service, so it starts only once the database answers:

```yaml
    depends_on:
      postgres:
        condition: service_healthy
```

and at the end of the file:

```yaml
volumes:
  postgres-data:
```

The published port is 55432, not 5432, so the container does not collide with a Postgres the developer already runs.

`Makefile`, new targets before `check`, and `check`'s doc line updated:

```makefile
DATABASE_URL ?= postgres://liquidator:liquidator@127.0.0.1:55432/liquidator
export DATABASE_URL

db-up: ## Start Postgres and wait for it
	$(COMPOSE) up -d postgres
	@until $(COMPOSE) exec -T postgres pg_isready -U liquidator >/dev/null 2>&1; do sleep 1; done
	@echo "postgres ready on 127.0.0.1:55432"

db-down: ## Stop Postgres, keeping its data
	$(COMPOSE) stop postgres

db-reset: ## Delete the database and its volume
	$(COMPOSE) rm -sf postgres
	docker volume rm -f blend-liquidator_postgres-data

db-migrate: ## Apply migrations to the local database
	sqlx migrate run

sqlx-prepare: ## Regenerate the committed offline query metadata (.sqlx)
	cargo sqlx prepare -- --lib --bins

check: ## Run everything CI runs (needs `make db-up` first)
```

Add `db-up db-down db-reset db-migrate sqlx-prepare` to the `.PHONY` line.

`.env.example`, a new section:

```bash
# ============================================
# STORE
# ============================================

# Postgres. May carry a password, so it is a secret: it is read from the
# environment only and never logged. `make db-up` starts the compose
# service this default points at.
DATABASE_URL=postgres://liquidator:liquidator@127.0.0.1:55432/liquidator

# Connections in the pool. Every store call is short and none is held
# across a network await, so a handful is plenty.
DATABASE_MAX_CONNECTIONS=5
```

`.github/workflows/ci.yml`, in the `lint-test` job only, between `runs-on` and `steps`:

```yaml
    services:
      postgres:
        image: postgres:17-alpine
        env:
          POSTGRES_USER: liquidator
          POSTGRES_PASSWORD: liquidator
          POSTGRES_DB: liquidator
        ports:
          - 5432:5432
        options: >-
          --health-cmd "pg_isready -U liquidator"
          --health-interval 10s
          --health-timeout 5s
          --health-retries 5
    env:
      DATABASE_URL: postgres://liquidator:liquidator@127.0.0.1:5432/liquidator
```

and, after the checkout and toolchain steps and before `cargo fmt`:

```yaml
      - name: Install sqlx-cli
        run: cargo install sqlx-cli --version 0.9.0 --no-default-features --features postgres,rustls --locked
      - name: Apply migrations
        run: sqlx migrate run
      - name: Offline query metadata is current
        run: cargo sqlx prepare --check -- --lib --bins
```

The database service makes the query macros check against a live schema and lets the store tests run; the `--check` step proves `.sqlx/` matches, because the Docker build has nothing else to go on.

`Dockerfile`, in the builder stage, extending the `COPY` list and setting the offline flag before `cargo build`:

```dockerfile
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The query metadata `cargo sqlx prepare` wrote, and the migrations the
# binary embeds. With SQLX_OFFLINE the compile-time query checks read the
# metadata instead of a database, which is the only way this stage can
# build: there is no Postgres in an image build.
COPY .sqlx ./.sqlx
COPY migrations ./migrations
ENV SQLX_OFFLINE=true
```

- [ ] **Step 4: Write the failing store tests**

Create `src/store.rs` with the module doc, the items below as stubs, and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Migrations apply to an empty database and the three tables exist.
    #[sqlx::test(migrations = "./migrations")]
    async fn migrations_create_the_schema(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store.ping().await.expect("ping");
        for table in ["cursors", "users", "auctions"] {
            let found: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables
                 WHERE table_schema = current_schema() AND table_name = $1)",
            )
            .bind(table)
            .fetch_one(store.pool())
            .await?;
            assert!(found, "{table} is missing");
        }
        Ok(())
    }

    /// Running them twice is a no-op, which is what every restart does.
    #[sqlx::test(migrations = "./migrations")]
    async fn migrating_an_already_migrated_database_is_a_no_op(
        pool: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store.migrate().await.expect("second migrate");
        store.migrate().await.expect("third migrate");
        Ok(())
    }

    /// `numeric` holds a health factor no `bigint` could, and it comes back
    /// as the same `i128`.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_health_factor_beyond_bigint_round_trips(pool: sqlx::PgPool) -> sqlx::Result<()> {
        // A stroop of debt against real collateral: far past i64::MAX.
        let enormous: i128 = 123_456_789_012_345_678_901_234_567;
        sqlx::query!(
            "INSERT INTO users (pool, account, health_factor, collateral, liabilities, updated_ledger)
             VALUES ($1, $2, $3::text::numeric, '{}'::jsonb, '{}'::jsonb, $4)",
            "CPOOL",
            "GUSER",
            enormous.to_string(),
            7_i64,
        )
        .execute(&pool)
        .await?;
        let row = sqlx::query!("SELECT health_factor::text AS hf FROM users")
            .fetch_one(&pool)
            .await?;
        let read = decimal(row.hf.as_deref(), "health_factor").expect("parses");
        assert_eq!(read, enormous);
        Ok(())
    }

    #[test]
    fn amount_maps_round_trip_through_json_as_decimal_strings() {
        let mut amounts = BTreeMap::new();
        amounts.insert(0_u32, i128::MAX);
        amounts.insert(2_u32, -1_i128);
        let json = index_amounts_to_json(&amounts);
        assert_eq!(json["0"], serde_json::json!(i128::MAX.to_string()));
        assert_eq!(index_amounts_from_json(&json, "collateral").expect("parses"), amounts);
    }

    #[test]
    fn a_json_amount_that_is_not_a_decimal_string_is_an_error_not_a_zero() {
        for bad in [
            serde_json::json!({"0": 12}),
            serde_json::json!({"0": "twelve"}),
            serde_json::json!({"x": "12"}),
            serde_json::json!([]),
        ] {
            assert!(
                matches!(
                    index_amounts_from_json(&bad, "collateral"),
                    Err(StoreError::Json { .. })
                ),
                "{bad} should not decode"
            );
        }
    }

    #[test]
    fn a_missing_or_unparseable_decimal_is_an_error() {
        assert!(matches!(decimal(None, "health_factor"), Err(StoreError::Decimal { .. })));
        assert!(matches!(decimal(Some("1.5"), "health_factor"), Err(StoreError::Decimal { .. })));
    }
}
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `make db-up && sqlx migrate run && cargo test --lib store`
Expected: compile errors — the items do not exist yet.

- [ ] **Step 6: Implement the store's foundation**

`src/store.rs`, above the tests:

```rust
//! The bot's durable state: cursors, tracked borrowers and open auctions.
//!
//! Every query is checked at compile time against the schema in
//! `migrations/`, and the metadata that makes that work without a database
//! lives in `.sqlx/` — regenerate it with `make sqlx-prepare` after
//! changing any query, or the Docker build fails on stale metadata.
//!
//! The store speaks the bot's types, not SQL's: amounts and health factors
//! are `i128`, ledgers are `u32`. Postgres has no 128-bit integer, so
//! amounts cross the boundary as decimal text — bound as `$n::text::numeric`
//! and read back through `::text` — which is exact for every value an
//! `i128` holds. Nothing here rounds.

use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// A failure talking to the store, or reading a value it returned.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The connection pool could not be opened.
    #[error("connecting to the database: {0}")]
    Connect(#[source] sqlx::Error),
    /// A query failed.
    #[error("database query: {0}")]
    Query(#[from] sqlx::Error),
    /// The embedded migrations could not be applied.
    #[error("applying migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// A `numeric` column did not read back as an `i128`. The column is
    /// named, never the row, so a message cannot leak an account.
    #[error("column {column} is not an integer: {value}")]
    Decimal {
        /// The column that failed.
        column: &'static str,
        /// What it held.
        value: String,
    },
    /// A `jsonb` column was not the shape this crate writes.
    #[error("column {column} is not an amount map: {detail}")]
    Json {
        /// The column that failed.
        column: &'static str,
        /// What was wrong with it.
        detail: String,
    },
}

/// The embedded migrations. `sqlx` takes a Postgres advisory lock while it
/// applies them, so two instances starting together cannot race.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A handle to the store. Cheap to clone: `PgPool` is a handle.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Opens a pool against `url`. Does not migrate: call [`Store::migrate`].
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await
            .map_err(StoreError::Connect)?;
        Ok(Self { pool })
    }

    /// Wraps an existing pool, which is how tests hand one in.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying pool, for queries this module does not wrap.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Applies every migration the binary embeds. Idempotent, and safe to
    /// run from two instances at once.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// One round trip, for the readiness probe.
    pub async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query!("SELECT 1 AS one").fetch_one(&self.pool).await?;
        Ok(())
    }
}

/// Reads a `numeric` column that was selected as text back into an `i128`.
/// A `NULL`, a fractional value or anything else is an error, never a zero:
/// a health factor that silently became zero would make a healthy account
/// look liquidatable.
fn decimal(text: Option<&str>, column: &'static str) -> Result<i128, StoreError> {
    let text = text.ok_or_else(|| StoreError::Decimal {
        column,
        value: "NULL".to_string(),
    })?;
    text.parse().map_err(|_| StoreError::Decimal {
        column,
        value: text.to_string(),
    })
}

/// A reserve-index-to-amount map as `jsonb`: keys are the index in decimal,
/// values the amount as a decimal string, because an `i128` amount exceeds
/// what a JSON number holds exactly.
fn index_amounts_to_json(amounts: &BTreeMap<u32, i128>) -> Value {
    Value::Object(
        amounts
            .iter()
            .map(|(index, amount)| (index.to_string(), Value::String(amount.to_string())))
            .collect(),
    )
}

/// The inverse. Anything that is not an object of decimal strings keyed by
/// decimal indexes is a `Json` error.
fn index_amounts_from_json(
    value: &Value,
    column: &'static str,
) -> Result<BTreeMap<u32, i128>, StoreError> {
    let object = value.as_object().ok_or_else(|| StoreError::Json {
        column,
        detail: "not an object".to_string(),
    })?;
    object
        .iter()
        .map(|(key, amount)| {
            let index: u32 = key.parse().map_err(|_| StoreError::Json {
                column,
                detail: format!("key {key:?} is not a reserve index"),
            })?;
            let amount = amount
                .as_str()
                .ok_or_else(|| StoreError::Json {
                    column,
                    detail: format!("amount for {key} is not a string"),
                })?
                .parse()
                .map_err(|_| StoreError::Json {
                    column,
                    detail: format!("amount for {key} is not an integer"),
                })?;
            Ok((index, amount))
        })
        .collect()
}

/// The same pair for a map keyed by asset address, as the auction sides are.
fn asset_amounts_to_json(amounts: &BTreeMap<String, i128>) -> Value {
    Value::Object(
        amounts
            .iter()
            .map(|(asset, amount)| (asset.clone(), Value::String(amount.to_string())))
            .collect(),
    )
}

/// The inverse of `asset_amounts_to_json`.
fn asset_amounts_from_json(
    value: &Value,
    column: &'static str,
) -> Result<BTreeMap<String, i128>, StoreError> {
    let object = value.as_object().ok_or_else(|| StoreError::Json {
        column,
        detail: "not an object".to_string(),
    })?;
    object
        .iter()
        .map(|(asset, amount)| {
            let amount = amount
                .as_str()
                .ok_or_else(|| StoreError::Json {
                    column,
                    detail: format!("amount for {asset} is not a string"),
                })?
                .parse()
                .map_err(|_| StoreError::Json {
                    column,
                    detail: format!("amount for {asset} is not an integer"),
                })?;
            Ok((asset.clone(), amount))
        })
        .collect()
}
```

`asset_amounts_to_json` and `asset_amounts_from_json` have no caller until Task 4; add `#[cfg_attr(not(test), allow(dead_code))]`-free code by writing Task 4's queries in the same phase — if `dead_code` fires at the end of this task, mark the two functions `#[allow(dead_code)]` with a comment naming Task 4 as their caller and remove it there.

In `src/liquidator.rs` add `pub mod store;` and extend the error:

```rust
    /// The store failed: connection, query, migration or a value it held.
    #[error("store: {0}")]
    Store(#[from] store::StoreError),
```

- [ ] **Step 7: Generate the offline metadata and run everything**

Run:

```bash
make db-up
sqlx migrate run
cargo test --lib store
make sqlx-prepare
git status --short .sqlx
```

Expected: 6 tests pass; `.sqlx/` holds one JSON per macro query.

Then prove the Docker path: `docker build -t blend-liquidator:phase3 .` succeeds with no database in reach.

- [ ] **Step 8: `make check`, then commit**

```bash
git add Cargo.toml Cargo.lock migrations .sqlx src/store.rs src/liquidator.rs \
        docker-compose.yml Makefile .github/workflows/ci.yml Dockerfile .env.example
git commit -m "feat(store): postgres schema, migrations and the store handle" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Cursors

**Files:**
- Modify: `src/store.rs`

**Interfaces:**
- Consumes: `Store`, `StoreError`, `decimal` (Task 1).
- Produces:
  - `pub struct store::Cursor { pub ledger: u32, pub paging_token: Option<String> }`.
  - `pub fn store::events_cursor(pool: &str) -> String` — the cursor name a pool's event stream uses, `events:{pool}`.
  - `Store::cursor(&self, name: &str) -> Result<Option<Cursor>, StoreError>`.
  - `Store::set_cursor(&self, name: &str, cursor: &Cursor) -> Result<(), StoreError>`.

- [ ] **Step 1: Write the failing tests**

Append to `src/store.rs`'s test module:

```rust
    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
    const FILLER: &str = "GCIH7OYRDHJ3IOPFEM7DMUX3SXTVHOO2XSWLGBMSVQ3EIHPHYUTNJID3";

    #[sqlx::test(migrations = "./migrations")]
    async fn a_cursor_is_absent_until_it_is_set_and_then_it_is_the_last_write(
        pool: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        let name = events_cursor(POOL);
        assert_eq!(name, format!("events:{POOL}"));
        assert_eq!(store.cursor(&name).await.expect("read"), None);

        let first = Cursor { ledger: 64_291_297, paging_token: Some("0276-0000".to_string()) };
        store.set_cursor(&name, &first).await.expect("write");
        assert_eq!(store.cursor(&name).await.expect("read"), Some(first));

        // Setting it again overwrites rather than failing on the primary key.
        let second = Cursor { ledger: 64_291_400, paging_token: None };
        store.set_cursor(&name, &second).await.expect("overwrite");
        assert_eq!(store.cursor(&name).await.expect("read"), Some(second));

        // Cursors are per name: another pool's is untouched.
        assert_eq!(store.cursor(&events_cursor("COTHER")).await.expect("read"), None);
        Ok(())
    }

    /// A ledger that does not fit `u32` cannot have come from this crate;
    /// reading it is an error, not a truncation.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_ledger_outside_u32_is_an_error(pool: sqlx::PgPool) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO cursors (name, ledger) VALUES ($1, $2)",
            "events:bad",
            i64::from(u32::MAX) + 1,
        )
        .execute(&pool)
        .await?;
        let store = Store::from_pool(pool);
        assert!(matches!(
            store.cursor("events:bad").await,
            Err(StoreError::Decimal { column: "ledger", .. })
        ));
        Ok(())
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib store`
Expected: compile errors — `Cursor`, `events_cursor`, `cursor`, `set_cursor` do not exist.

- [ ] **Step 3: Implement**

Add to `src/store.rs`:

```rust
/// How far a named task has applied. The poller keeps one per pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// The last ledger whose events were applied in full.
    pub ledger: u32,
    /// The RPC's paging token for the next `getEvents` page, when the
    /// poller stopped mid-ledger. `None` means the ledger is complete.
    pub paging_token: Option<String>,
}

/// The cursor name a pool's event stream uses.
#[must_use]
pub fn events_cursor(pool: &str) -> String {
    format!("events:{pool}")
}

/// A `bigint` ledger back into the `u32` the chain uses. Out of range means
/// the row did not come from this crate.
fn ledger(value: i64, column: &'static str) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::Decimal {
        column,
        value: value.to_string(),
    })
}

impl Store {
    /// The named cursor, or `None` when the task has never run.
    pub async fn cursor(&self, name: &str) -> Result<Option<Cursor>, StoreError> {
        let row = sqlx::query!(
            "SELECT ledger, paging_token FROM cursors WHERE name = $1",
            name
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(Cursor {
                ledger: ledger(row.ledger, "ledger")?,
                paging_token: row.paging_token,
            })
        })
        .transpose()
    }

    /// Writes the named cursor, replacing any previous value.
    pub async fn set_cursor(&self, name: &str, cursor: &Cursor) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO cursors (name, ledger, paging_token, updated_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (name) DO UPDATE
               SET ledger = EXCLUDED.ledger,
                   paging_token = EXCLUDED.paging_token,
                   updated_at = EXCLUDED.updated_at",
            name,
            i64::from(cursor.ledger),
            cursor.paging_token.as_deref(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib store`
Expected: 8 pass.

- [ ] **Step 5: `make sqlx-prepare`, `make check`, then commit**

```bash
git add src/store.rs .sqlx
git commit -m "feat(store): per-task cursors" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Tracked users

**Files:**
- Modify: `src/store.rs`

**Interfaces:**
- Consumes: `Store`, `StoreError`, `decimal`, `ledger`, `index_amounts_to_json`, `index_amounts_from_json`.
- Produces:
  - `pub struct store::TrackedUser { pub pool: String, pub account: String, pub health_factor: i128, pub collateral: BTreeMap<u32, i128>, pub liabilities: BTreeMap<u32, i128>, pub updated_ledger: u32 }`.
  - `Store::upsert_user(&self, user: &TrackedUser) -> Result<(), StoreError>`.
  - `Store::delete_user(&self, pool: &str, account: &str) -> Result<bool, StoreError>` — `true` when a row went away.
  - `Store::user(&self, pool: &str, account: &str) -> Result<Option<TrackedUser>, StoreError>`.
  - `Store::users_below_health(&self, pool: &str, threshold: i128, limit: i64) -> Result<Vec<TrackedUser>, StoreError>` — ascending by health factor.
  - `Store::users_stale(&self, pool: &str, older_than: u32, limit: i64) -> Result<Vec<TrackedUser>, StoreError>` — oldest first.
  - `Store::count_users(&self, pool: &str) -> Result<i64, StoreError>`.

- [ ] **Step 1: Write the failing tests**

Append to `src/store.rs`'s test module:

```rust
    fn user(account: &str, health_factor: i128, updated_ledger: u32) -> TrackedUser {
        let mut collateral = BTreeMap::new();
        collateral.insert(0_u32, 1_000_000_i128);
        let mut liabilities = BTreeMap::new();
        liabilities.insert(1_u32, 500_000_i128);
        TrackedUser {
            pool: POOL.to_string(),
            account: account.to_string(),
            health_factor,
            collateral,
            liabilities,
            updated_ledger,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_user_round_trips_and_upsert_replaces(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        assert_eq!(store.user(POOL, USER).await.expect("read"), None);

        let mut tracked = user(USER, 10_070_767, 64_291_297);
        store.upsert_user(&tracked).await.expect("insert");
        assert_eq!(store.user(POOL, USER).await.expect("read"), Some(tracked.clone()));
        assert_eq!(store.count_users(POOL).await.expect("count"), 1);

        tracked.health_factor = 9_500_000;
        tracked.updated_ledger = 64_291_400;
        tracked.liabilities.insert(2_u32, 7_i128);
        store.upsert_user(&tracked).await.expect("update");
        assert_eq!(store.user(POOL, USER).await.expect("read"), Some(tracked));
        assert_eq!(store.count_users(POOL).await.expect("count"), 1);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_reports_whether_a_row_went_away(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store.upsert_user(&user(USER, 10_070_767, 1)).await.expect("insert");
        assert!(store.delete_user(POOL, USER).await.expect("delete"));
        assert!(!store.delete_user(POOL, USER).await.expect("delete again"));
        assert_eq!(store.count_users(POOL).await.expect("count"), 0);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_unhealthiest_users_come_back_first_and_the_threshold_excludes(
        pool: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store.upsert_user(&user(USER, 9_000_000, 1)).await.expect("a");
        store.upsert_user(&user(FILLER, 8_000_000, 1)).await.expect("b");
        store.upsert_user(&user("GHEALTHY", 30_000_000, 1)).await.expect("c");

        let scanned = store.users_below_health(POOL, 12_000_000, 10).await.expect("scan");
        let accounts: Vec<&str> = scanned.iter().map(|u| u.account.as_str()).collect();
        assert_eq!(accounts, [FILLER, USER], "ascending by health factor");

        assert_eq!(store.users_below_health(POOL, 12_000_000, 1).await.expect("limit").len(), 1);
        assert!(store.users_below_health("COTHER", 12_000_000, 10).await.expect("other").is_empty());
        // The threshold is exclusive: a user exactly at it is healthy enough.
        assert!(store.users_below_health(POOL, 8_000_000, 10).await.expect("exact").is_empty());
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn stale_users_come_back_oldest_first(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store.upsert_user(&user(USER, 10_000_000, 100)).await.expect("a");
        store.upsert_user(&user(FILLER, 10_000_000, 50)).await.expect("b");
        store.upsert_user(&user("GFRESH", 10_000_000, 900)).await.expect("c");

        let stale = store.users_stale(POOL, 500, 10).await.expect("stale");
        let accounts: Vec<&str> = stale.iter().map(|u| u.account.as_str()).collect();
        assert_eq!(accounts, [FILLER, USER]);
        assert_eq!(store.users_stale(POOL, 500, 1).await.expect("limit").len(), 1);
        Ok(())
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib store`
Expected: compile errors.

- [ ] **Step 3: Implement**

Add to `src/store.rs`:

```rust
/// A borrower the bot tracks. A row exists only while the account owes
/// something: the tracker deletes it the moment its liabilities empty, so
/// the table's size is the number of positions that could be liquidated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedUser {
    /// The pool contract.
    pub pool: String,
    /// The borrower.
    pub account: String,
    /// Health factor normalised to 7 decimals (`hf * 10^7 / oracle_scalar`),
    /// so pools whose oracles differ in decimals order alike.
    pub health_factor: i128,
    /// Reserve index to b-token amount.
    pub collateral: BTreeMap<u32, i128>,
    /// Reserve index to d-token amount.
    pub liabilities: BTreeMap<u32, i128>,
    /// The ledger this row was computed at.
    pub updated_ledger: u32,
}

impl Store {
    /// Writes a borrower, replacing any previous row for the same pool and
    /// account.
    pub async fn upsert_user(&self, user: &TrackedUser) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO users (pool, account, health_factor, collateral, liabilities, updated_ledger)
             VALUES ($1, $2, $3::text::numeric, $4, $5, $6)
             ON CONFLICT (pool, account) DO UPDATE
               SET health_factor = EXCLUDED.health_factor,
                   collateral = EXCLUDED.collateral,
                   liabilities = EXCLUDED.liabilities,
                   updated_ledger = EXCLUDED.updated_ledger",
            user.pool,
            user.account,
            user.health_factor.to_string(),
            index_amounts_to_json(&user.collateral),
            index_amounts_to_json(&user.liabilities),
            i64::from(user.updated_ledger),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes a borrower. `true` when a row went away, which is what the
    /// tracker reports when a position closes.
    pub async fn delete_user(&self, pool: &str, account: &str) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "DELETE FROM users WHERE pool = $1 AND account = $2",
            pool,
            account
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// One borrower.
    pub async fn user(&self, pool: &str, account: &str) -> Result<Option<TrackedUser>, StoreError> {
        let row = sqlx::query!(
            "SELECT pool, account, health_factor::text AS health_factor, collateral,
                    liabilities, updated_ledger
             FROM users WHERE pool = $1 AND account = $2",
            pool,
            account
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(TrackedUser {
                pool: row.pool,
                account: row.account,
                health_factor: decimal(row.health_factor.as_deref(), "health_factor")?,
                collateral: index_amounts_from_json(&row.collateral, "collateral")?,
                liabilities: index_amounts_from_json(&row.liabilities, "liabilities")?,
                updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
            })
        })
        .transpose()
    }

    /// The pool's borrowers below `threshold`, least healthy first. The
    /// threshold is exclusive, so a user exactly at a scan threshold is not
    /// rechecked by it.
    pub async fn users_below_health(
        &self,
        pool: &str,
        threshold: i128,
        limit: i64,
    ) -> Result<Vec<TrackedUser>, StoreError> {
        let rows = sqlx::query!(
            "SELECT pool, account, health_factor::text AS health_factor, collateral,
                    liabilities, updated_ledger
             FROM users
             WHERE pool = $1 AND health_factor < $2::text::numeric
             ORDER BY health_factor ASC
             LIMIT $3",
            pool,
            threshold.to_string(),
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(TrackedUser {
                    pool: row.pool,
                    account: row.account,
                    health_factor: decimal(row.health_factor.as_deref(), "health_factor")?,
                    collateral: index_amounts_from_json(&row.collateral, "collateral")?,
                    liabilities: index_amounts_from_json(&row.liabilities, "liabilities")?,
                    updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
                })
            })
            .collect()
    }

    /// The pool's borrowers whose row predates `older_than`, oldest first:
    /// the refresh pass walks these so a long-idle borrower's accrued
    /// interest is not missed.
    pub async fn users_stale(
        &self,
        pool: &str,
        older_than: u32,
        limit: i64,
    ) -> Result<Vec<TrackedUser>, StoreError> {
        let rows = sqlx::query!(
            "SELECT pool, account, health_factor::text AS health_factor, collateral,
                    liabilities, updated_ledger
             FROM users
             WHERE pool = $1 AND updated_ledger < $2
             ORDER BY updated_ledger ASC
             LIMIT $3",
            pool,
            i64::from(older_than),
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(TrackedUser {
                    pool: row.pool,
                    account: row.account,
                    health_factor: decimal(row.health_factor.as_deref(), "health_factor")?,
                    collateral: index_amounts_from_json(&row.collateral, "collateral")?,
                    liabilities: index_amounts_from_json(&row.liabilities, "liabilities")?,
                    updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
                })
            })
            .collect()
    }

    /// How many borrowers the bot tracks in a pool.
    pub async fn count_users(&self, pool: &str) -> Result<i64, StoreError> {
        let row = sqlx::query!(
            "SELECT count(*) AS \"count!\" FROM users WHERE pool = $1",
            pool
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.count)
    }
}
```

The three row-to-`TrackedUser` conversions are the same shape; if clippy or the reviewer objects to the repetition, extract a `fn tracked_user(row: …)` only if the macro's anonymous row types allow it — they do not share a type, so a small macro-free duplication here is the honest cost of compile-time checking. Say so in a comment rather than inventing a trait.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib store`
Expected: 12 pass.

- [ ] **Step 5: `make sqlx-prepare`, `make check`, then commit**

```bash
git add src/store.rs .sqlx
git commit -m "feat(store): tracked borrowers, health scans and refresh batches" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Tracked auctions

**Files:**
- Modify: `src/store.rs`

**Interfaces:**
- Consumes: `Store`, `StoreError`, `ledger`, `asset_amounts_to_json`, `asset_amounts_from_json`; `chain::xdr::AuctionType`.
- Produces:
  - `pub struct store::TrackedAuction { pub pool: String, pub account: String, pub auction_type: AuctionType, pub start_ledger: u32, pub fill_ledger: Option<u32>, pub percent: FillPercent, pub bid: BTreeMap<String, i128>, pub lot: BTreeMap<String, i128>, pub updated_ledger: u32 }`.
  - `Store::upsert_auction(&self, auction: &TrackedAuction) -> Result<(), StoreError>`.
  - `Store::delete_auction(&self, pool: &str, account: &str, auction_type: AuctionType) -> Result<bool, StoreError>`.
  - `Store::auction(&self, pool: &str, account: &str, auction_type: AuctionType) -> Result<Option<TrackedAuction>, StoreError>`.
  - `Store::open_auctions(&self, pool: &str) -> Result<Vec<TrackedAuction>, StoreError>` — ascending by start ledger.

- [ ] **Step 1: Write the failing tests**

Append to `src/store.rs`'s test module:

```rust
    fn auction(account: &str, start_ledger: u32) -> TrackedAuction {
        let mut bid = BTreeMap::new();
        bid.insert("CUSDC".to_string(), 1_000_i128);
        let mut lot = BTreeMap::new();
        lot.insert("CXLM".to_string(), 2_000_i128);
        TrackedAuction {
            pool: POOL.to_string(),
            account: account.to_string(),
            auction_type: AuctionType::UserLiquidation,
            start_ledger,
            fill_ledger: None,
            percent: 100,
            bid,
            lot,
            updated_ledger: start_ledger,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_auction_round_trips_and_upsert_replaces(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        let kind = AuctionType::UserLiquidation;
        assert_eq!(store.auction(POOL, USER, kind).await.expect("read"), None);

        let mut open = auction(USER, 64_291_297);
        store.upsert_auction(&open).await.expect("insert");
        assert_eq!(store.auction(POOL, USER, kind).await.expect("read"), Some(open.clone()));

        // The filler plans a fill ledger and a partial percent.
        open.fill_ledger = Some(64_291_400);
        open.percent = 60;
        open.updated_ledger = 64_291_350;
        store.upsert_auction(&open).await.expect("update");
        assert_eq!(store.auction(POOL, USER, kind).await.expect("read"), Some(open));
        Ok(())
    }

    /// The three auction types are separate rows for one account.
    #[sqlx::test(migrations = "./migrations")]
    async fn auction_types_do_not_collide(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        for kind in [AuctionType::UserLiquidation, AuctionType::BadDebt, AuctionType::Interest] {
            let mut row = auction(USER, 10);
            row.auction_type = kind;
            store.upsert_auction(&row).await.expect("insert");
        }
        assert_eq!(store.open_auctions(POOL).await.expect("list").len(), 3);
        assert!(store.delete_auction(POOL, USER, AuctionType::BadDebt).await.expect("delete"));
        assert_eq!(store.open_auctions(POOL).await.expect("list").len(), 2);
        assert!(!store.delete_auction(POOL, USER, AuctionType::BadDebt).await.expect("again"));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn open_auctions_come_back_in_start_order(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store.upsert_auction(&auction(USER, 300)).await.expect("a");
        store.upsert_auction(&auction(FILLER, 100)).await.expect("b");
        let open = store.open_auctions(POOL).await.expect("list");
        let accounts: Vec<&str> = open.iter().map(|a| a.account.as_str()).collect();
        assert_eq!(accounts, [FILLER, USER]);
        assert!(store.open_auctions("COTHER").await.expect("other").is_empty());
        Ok(())
    }

    /// A discriminant the contract never emits cannot be read back as an
    /// auction type. It surfaces through `open_auctions`, which reads every
    /// row of a pool: a typed lookup cannot match a code this crate never
    /// writes, so that is where corruption has to be caught.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_auction_type_in_the_row_is_an_error(pool: sqlx::PgPool) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO auctions (pool, account, auction_type, start_ledger, percent, bid, lot, updated_ledger)
             VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, '{}'::jsonb, $4)",
            POOL, USER, 7_i16, 10_i64, 100_i16,
        )
        .execute(&pool)
        .await?;
        let store = Store::from_pool(pool);
        assert!(matches!(
            store.open_auctions(POOL).await,
            Err(StoreError::Decimal { column: "auction_type", .. })
        ));
        // A typed read is not the place this shows up: there is no
        // user-liquidation auction for this account, and that is the answer.
        assert_eq!(
            store.auction(POOL, USER, AuctionType::UserLiquidation).await.expect("typed read"),
            None
        );
        Ok(())
    }
```

Add `use crate::chain::xdr::AuctionType;` to the test module's imports if `use super::*` does not already bring it in.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib store`
Expected: compile errors.

- [ ] **Step 3: Implement**

Add to `src/store.rs`, with `use crate::chain::xdr::AuctionType;` at the top of the file:

```rust
/// An auction the bot knows about: opened by a `new_auction` event, reduced
/// by a partial `fill_auction`, removed by a full fill or a
/// `delete_auction`. `fill_ledger` is the filler's current plan, not
/// anything the chain says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedAuction {
    /// The pool contract.
    pub pool: String,
    /// The account being auctioned.
    pub account: String,
    /// Which auction: user liquidation, bad debt or interest.
    pub auction_type: AuctionType,
    /// The ledger the auction was created in, from which its Dutch ramp is
    /// measured.
    pub start_ledger: u32,
    /// The ledger the filler intends to fill at, once it has planned one.
    pub fill_ledger: Option<u32>,
    /// The share of the position auctioned, validated 1 to 100 by the
    /// type Phase 2 built for the contract's own range.
    pub percent: FillPercent,
    /// Asset address to amount the filler pays.
    pub bid: BTreeMap<String, i128>,
    /// Asset address to amount the filler receives.
    pub lot: BTreeMap<String, i128>,
    /// The ledger this row was last written at.
    pub updated_ledger: u32,
}

/// The `smallint` an auction type is stored as.
fn auction_type_code(auction_type: AuctionType) -> i16 {
    match auction_type {
        AuctionType::UserLiquidation => 0,
        AuctionType::BadDebt => 1,
        AuctionType::Interest => 2,
    }
}

/// The inverse, rejecting a discriminant the contract never emits.
fn auction_type_from_code(code: i16) -> Result<AuctionType, StoreError> {
    let value = u32::try_from(code).map_err(|_| StoreError::Decimal {
        column: "auction_type",
        value: code.to_string(),
    })?;
    AuctionType::try_from(value).map_err(|_| StoreError::Decimal {
        column: "auction_type",
        value: code.to_string(),
    })
}

/// A `smallint` percent back into the 1-to-100 range the contract uses.
fn percent_from_code(code: i16) -> Result<u32, StoreError> {
    u32::try_from(code).map_err(|_| StoreError::Decimal {
        column: "percent",
        value: code.to_string(),
    })
}

impl Store {
    /// Writes an auction, replacing any previous row for the same pool,
    /// account and type.
    pub async fn upsert_auction(&self, auction: &TrackedAuction) -> Result<(), StoreError> {
        let percent = i16::try_from(auction.percent).map_err(|_| StoreError::Decimal {
            column: "percent",
            value: auction.percent.to_string(),
        })?;
        sqlx::query!(
            "INSERT INTO auctions (pool, account, auction_type, start_ledger, fill_ledger,
                                   percent, bid, lot, updated_ledger)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (pool, account, auction_type) DO UPDATE
               SET start_ledger = EXCLUDED.start_ledger,
                   fill_ledger = EXCLUDED.fill_ledger,
                   percent = EXCLUDED.percent,
                   bid = EXCLUDED.bid,
                   lot = EXCLUDED.lot,
                   updated_ledger = EXCLUDED.updated_ledger",
            auction.pool,
            auction.account,
            auction_type_code(auction.auction_type),
            i64::from(auction.start_ledger),
            auction.fill_ledger.map(i64::from),
            percent,
            asset_amounts_to_json(&auction.bid),
            asset_amounts_to_json(&auction.lot),
            i64::from(auction.updated_ledger),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes an auction. `true` when a row went away.
    pub async fn delete_auction(
        &self,
        pool: &str,
        account: &str,
        auction_type: AuctionType,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "DELETE FROM auctions WHERE pool = $1 AND account = $2 AND auction_type = $3",
            pool,
            account,
            auction_type_code(auction_type),
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// One auction, by its primary key. A discriminant this crate never
    /// writes cannot match a typed lookup, so a corrupted row is reported
    /// by `open_auctions`, which reads every row, rather than here.
    pub async fn auction(
        &self,
        pool: &str,
        account: &str,
        auction_type: AuctionType,
    ) -> Result<Option<TrackedAuction>, StoreError> {
        let row = sqlx::query!(
            "SELECT pool, account, auction_type, start_ledger, fill_ledger, percent,
                    bid, lot, updated_ledger
             FROM auctions WHERE pool = $1 AND account = $2 AND auction_type = $3",
            pool,
            account,
            auction_type_code(auction_type),
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(TrackedAuction {
                pool: row.pool,
                account: row.account,
                auction_type: auction_type_from_code(row.auction_type)?,
                start_ledger: ledger(row.start_ledger, "start_ledger")?,
                fill_ledger: row
                    .fill_ledger
                    .map(|value| ledger(value, "fill_ledger"))
                    .transpose()?,
                percent: percent_from_code(row.percent)?,
                bid: asset_amounts_from_json(&row.bid, "bid")?,
                lot: asset_amounts_from_json(&row.lot, "lot")?,
                updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
            })
        })
        .transpose()
    }

    /// Every auction the bot has open in a pool, oldest first: the filler
    /// walks them in the order they became fillable.
    pub async fn open_auctions(&self, pool: &str) -> Result<Vec<TrackedAuction>, StoreError> {
        let rows = sqlx::query!(
            "SELECT pool, account, auction_type, start_ledger, fill_ledger, percent,
                    bid, lot, updated_ledger
             FROM auctions WHERE pool = $1 ORDER BY start_ledger ASC, account ASC",
            pool
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(TrackedAuction {
                    pool: row.pool,
                    account: row.account,
                    auction_type: auction_type_from_code(row.auction_type)?,
                    start_ledger: ledger(row.start_ledger, "start_ledger")?,
                    fill_ledger: row
                        .fill_ledger
                        .map(|value| ledger(value, "fill_ledger"))
                        .transpose()?,
                    percent: percent_from_code(row.percent)?,
                    bid: asset_amounts_from_json(&row.bid, "bid")?,
                    lot: asset_amounts_from_json(&row.lot, "lot")?,
                    updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
                })
            })
            .collect()
    }
}
```

If Task 1 left `#[allow(dead_code)]` on `asset_amounts_to_json` and `asset_amounts_from_json`, remove it here: this task is their caller.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib store`
Expected: 16 pass.

- [ ] **Step 5: `make sqlx-prepare`, `make check`, then commit**

```bash
git add src/store.rs .sqlx
git commit -m "feat(store): open auctions keyed by pool, account and type" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: The pools file and the service configuration

**Files:**
- Modify: `src/config.rs`, `.env.example`

**Interfaces:**
- Consumes: `ChainConfig`, `Secret`, `Args` (Phase 2).
- Produces:
  - `pub struct config::Decimal7(i128)` with `FromStr`, `Deserialize` (TOML string, integer or float, never float arithmetic), `pub fn get(self) -> i128`.
  - `pub struct config::PoolConfig { pub address: String, pub primary_asset: String, pub min_primary_collateral: i128, pub min_health_factor: i128, pub default_profit_bps: u32, pub force_fill: bool, pub supported_bid: Vec<String>, pub supported_lot: Vec<String>, pub profits: Vec<ProfitRule> }` and `pub struct config::ProfitRule { pub profit_bps: u32, pub supported_bid: Vec<String>, pub supported_lot: Vec<String> }`.
  - `pub enum config::RunMode { Loop, CheckConfig }` (clap `ValueEnum`, `loop`/`check-config`).
  - `pub struct config::SeedConfig { pub url: Option<String>, pub health_factor_max: i128, pub file: Option<PathBuf> }`.
  - `pub struct config::ServiceConfig { pub chain: ChainConfig, pub database_url: Secret, pub database_max_connections: u32, pub pools: Vec<PoolConfig>, pub run_mode: RunMode, pub dry_run: bool, pub poll_interval: Duration, pub user_refresh_ledgers: u32, pub refresh_batch: u32, pub full_scan_ledgers: u32, pub scan_health_factor: i128, pub seed: SeedConfig }`.
  - `Args::service(&self) -> Result<ServiceConfig, LiquidatorError>` and `Args::service_with_secrets(&self, database_url: Option<String>, rpc_api_key: Option<String>) -> Result<ServiceConfig, LiquidatorError>`.
  - New `Args` fields: `pools_file: Option<PathBuf>`, `pools_toml: Option<String>`, `run_mode: RunMode`, `database_max_connections: u32`, `poll_interval_ms: u64`, `user_refresh_ledgers: u32`, `refresh_batch: u32`, `full_scan_ledgers: u32`, `scan_hf_threshold: Decimal7`, `seed_url: String`, `seed_hf_max: Decimal7`, `seed_file: Option<PathBuf>`. `DATABASE_URL` is **not** a clap argument: like `RPC_API_KEY` it is read from the environment only, because it may carry a password.

- [ ] **Step 1: Write the failing tests**

Append to `src/config.rs`'s test module (every test that parses arguments calls `assert_clean_environment()` first, as the existing ones do):

```rust
    const POOLS: &str = r#"
[[pools]]
address = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD"
primary_asset = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75"
min_primary_collateral = "1000000000000"
min_health_factor = 1.5
default_profit_bps = 1000
force_fill = false
supported_bid = ["CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75"]
supported_lot = ["*"]

[[pools.profits]]
profit_bps = 500
supported_bid = ["CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75"]
supported_lot = ["*"]
"#;

    #[test]
    fn a_decimal_knob_becomes_seven_decimal_fixed_point() {
        for (text, expected) in [
            ("1.5", 15_000_000_i128),
            ("1", 10_000_000),
            ("0.998", 9_980_000),
            ("1.0000001", 10_000_001),
            ("10", 100_000_000),
            ("0", 0),
        ] {
            assert_eq!(text.parse::<Decimal7>().expect(text).get(), expected, "{text}");
        }
    }

    /// More precision than the fixed point holds is a startup error, not a
    /// silent rounding of a threshold that decides whether to liquidate.
    #[test]
    fn a_decimal_knob_refuses_what_it_cannot_hold() {
        for text in ["1.00000001", "", "1.2.3", "abc", "-1", "1e9", "170141183460469231731687303715884105728"] {
            assert!(text.parse::<Decimal7>().is_err(), "{text} should not parse");
        }
    }

    #[test]
    fn the_pools_file_parses_with_its_profit_rules() {
        let pools = parse_pools(POOLS).expect("parses");
        assert_eq!(pools.len(), 1);
        let pool = &pools[0];
        assert_eq!(pool.address, "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD");
        assert_eq!(pool.min_primary_collateral, 1_000_000_000_000);
        assert_eq!(pool.min_health_factor, 15_000_000, "1.5 in 7 decimals");
        assert_eq!(pool.default_profit_bps, 1_000);
        assert!(!pool.force_fill);
        assert_eq!(pool.supported_lot, ["*"]);
        assert_eq!(pool.profits.len(), 1);
        assert_eq!(pool.profits[0].profit_bps, 500);
    }

    #[test]
    fn a_pools_file_that_is_wrong_is_a_config_error_naming_the_problem() {
        for (bad, expected) in [
            ("", "at least one pool"),
            ("[[pools]]\naddress = \"C\"\n", "missing"),
            (&POOLS.replace("1000000000000", "not a number"), "min_primary_collateral"),
            (&POOLS.replace("min_health_factor = 1.5", "min_health_factor = 1.00000001"), "min_health_factor"),
            (&POOLS.replace("[[pools]]", "[[pools]]\naddress = \"CDUP\""), "duplicate"),
        ] {
            let error = parse_pools(bad).expect_err(bad).to_string();
            assert!(error.contains(expected), "{error} should mention {expected}");
        }
    }

    /// Two pools naming the same address is a configuration mistake the bot
    /// must refuse: it would track one pool twice and race itself.
    #[test]
    fn duplicate_pool_addresses_are_refused() {
        let doubled = format!("{POOLS}{POOLS}");
        assert!(parse_pools(&doubled).expect_err("duplicate").to_string().contains("duplicate"));
    }

    #[test]
    fn the_service_configuration_needs_a_database_url_and_one_pools_source() {
        assert_clean_environment();
        let base = ["liquidator", "--network", "testnet", "--rpc-url", "http://rpc"];
        let args = parse(&[&base[..], &["--pools-toml", POOLS]].concat());
        assert!(matches!(args.service_with_secrets(None, None), Err(LiquidatorError::Config(_))));
        let config = args
            .service_with_secrets(Some("postgres://u:p@localhost/db".to_string()), None)
            .expect("configuration");
        assert_eq!(config.pools.len(), 1);
        assert_eq!(config.database_url.expose(), "postgres://u:p@localhost/db");
        assert_eq!(config.run_mode, RunMode::Loop);
        assert!(config.dry_run, "dry-run is still the default");
        assert_eq!(config.poll_interval, std::time::Duration::from_millis(1_000));
        assert_eq!(config.scan_health_factor, 12_000_000);
        assert_eq!(config.seed.health_factor_max, 100_000_000);
        assert_eq!(config.seed.url.as_deref(), Some("https://api.blend.templarfi.org"));

        // Neither pools source, and both at once, are both errors.
        let neither = parse(&base);
        assert!(matches!(neither.service_with_secrets(Some("postgres://x".to_string()), None), Err(LiquidatorError::Config(_))));
        assert!(Args::try_parse_from([&base[..], &["--pools-toml", POOLS, "--pools-file", "/x"]].concat()).is_err());
    }

    /// The database URL may carry a password, so it must never be an
    /// argument and never render.
    #[test]
    fn the_database_url_is_not_an_argument_and_never_renders() {
        assert_clean_environment();
        assert!(Args::try_parse_from(["liquidator", "--database-url", "postgres://x"]).is_err());
        let args = parse(&["liquidator", "--network", "testnet", "--rpc-url", "http://rpc", "--pools-toml", POOLS]);
        let config = args
            .service_with_secrets(Some("postgres://user:hunter2@localhost/db".to_string()), None)
            .expect("configuration");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("Secret(<redacted>)"));
    }

    /// An empty SEED_URL disables the analytics source, as the spec says.
    #[test]
    fn an_empty_seed_url_disables_the_analytics_source() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator", "--network", "testnet", "--rpc-url", "http://rpc",
            "--pools-toml", POOLS, "--seed-url", "",
        ]);
        let config = args.service_with_secrets(Some("postgres://x".to_string()), None).expect("configuration");
        assert_eq!(config.seed.url, None);
    }
```

Extend `assert_clean_environment`'s list with the new variables — but **not** `DATABASE_URL`: like `RPC_API_KEY` it is never a clap argument, and `make check` and CI both export it job-wide so the query macros can reach the schema, so asserting it absent would fail every run. The list gains `DATABASE_MAX_CONNECTIONS`, `POOLS_FILE`, `POOLS_TOML`, `RUN_MODE`, `POLL_INTERVAL_MS`, `USER_REFRESH_LEDGERS`, `REFRESH_BATCH`, `FULL_SCAN_LEDGERS`, `SCAN_HF_THRESHOLD`, `SEED_URL`, `SEED_HF_MAX`, `SEED_FILE`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config`
Expected: compile errors.

- [ ] **Step 3: Implement**

In `src/config.rs`:

```rust
/// A decimal knob in 7-decimal fixed point, the scale the pool contract
/// uses for factors and the scale the store normalises health factors to.
///
/// Parsed from decimal text, never from float arithmetic: a TOML float
/// reaches this through its own shortest round-tripping rendering, so
/// `1.5` is exactly `15_000_000` and a value needing more than seven
/// fractional digits is a startup error rather than a quietly rounded
/// threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Decimal7(i128);

impl Decimal7 {
    /// The value in 7-decimal fixed point.
    #[must_use]
    pub fn get(self) -> i128 {
        self.0
    }
}

impl std::str::FromStr for Decimal7 {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (whole, fraction) = match text.split_once('.') {
            Some((whole, fraction)) => (whole, fraction),
            None => (text, ""),
        };
        if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!("`{text}` is not a non-negative decimal number"));
        }
        if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!("`{text}` is not a non-negative decimal number"));
        }
        if fraction.len() > 7 {
            return Err(format!("`{text}` has more than 7 decimal places"));
        }
        let scaled = format!("{whole}{fraction:0<7}");
        scaled
            .parse::<i128>()
            .map(Self)
            .map_err(|_| format!("`{text}` does not fit a 128-bit fixed-point value"))
    }
}

impl<'de> serde::Deserialize<'de> for Decimal7 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A TOML value reaches us as a string, an integer or a float; each
        // is converted through its decimal text, so no float arithmetic
        // ever touches a threshold.
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Integer(i64),
            Float(f64),
        }
        let text = match Raw::deserialize(deserializer)? {
            Raw::Text(text) => text,
            Raw::Integer(value) => value.to_string(),
            Raw::Float(value) => value.to_string(),
        };
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// An amount in an asset's own decimals, written as a decimal string
/// because it exceeds what TOML integers and JSON numbers hold.
fn amount_from_str(text: &str, field: &'static str) -> Result<i128, String> {
    text.parse()
        .map_err(|_| format!("{field}: `{text}` is not an integer amount"))
}

/// One profit rule: the first whose asset lists match a candidate wins.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfitRule {
    /// Required profit in basis points.
    pub profit_bps: u32,
    /// Bid assets this rule covers, or `["*"]`.
    pub supported_bid: Vec<String>,
    /// Lot assets this rule covers, or `["*"]`.
    pub supported_lot: Vec<String>,
}

/// One pool the bot follows. Phase 3 uses `address`, `primary_asset` and
/// the supported-asset lists; the profit and collateral fields are the
/// filler's, parsed here so the file's schema is settled once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// The pool contract.
    pub address: String,
    /// The asset the bot keeps as collateral in this pool.
    pub primary_asset: String,
    /// The least of it to hold, in the asset's own decimals.
    pub min_primary_collateral: i128,
    /// The health factor the filler keeps its own position above, 7 decimals.
    pub min_health_factor: i128,
    /// Profit required when no rule matches, in basis points.
    pub default_profit_bps: u32,
    /// Fill regardless of profit. For testing a pool, not for production.
    pub force_fill: bool,
    /// Bid assets the bot will pay, or `["*"]`.
    pub supported_bid: Vec<String>,
    /// Lot assets the bot will take, or `["*"]`.
    pub supported_lot: Vec<String>,
    /// Ordered profit rules; the first match wins.
    pub profits: Vec<ProfitRule>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPool {
    address: String,
    primary_asset: String,
    min_primary_collateral: String,
    min_health_factor: Decimal7,
    default_profit_bps: u32,
    #[serde(default)]
    force_fill: bool,
    supported_bid: Vec<String>,
    supported_lot: Vec<String>,
    #[serde(default)]
    profits: Vec<ProfitRule>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPools {
    #[serde(default)]
    pools: Vec<RawPool>,
}

/// Parses the pools file. Every failure names the field that caused it,
/// because this runs at startup where the operator is watching.
pub fn parse_pools(text: &str) -> Result<Vec<PoolConfig>, LiquidatorError> {
    let raw: RawPools = toml::from_str(text)
        .map_err(|error| LiquidatorError::Config(format!("pools file: {error}")))?;
    if raw.pools.is_empty() {
        return Err(LiquidatorError::Config(
            "pools file: at least one pool (a [[pools]] table) is required".to_string(),
        ));
    }
    let mut pools = Vec::with_capacity(raw.pools.len());
    let mut seen = std::collections::BTreeSet::new();
    for pool in raw.pools {
        if !seen.insert(pool.address.clone()) {
            return Err(LiquidatorError::Config(format!(
                "pools file: duplicate pool {}",
                pool.address
            )));
        }
        let min_primary_collateral =
            amount_from_str(&pool.min_primary_collateral, "min_primary_collateral")
                .map_err(LiquidatorError::Config)?;
        pools.push(PoolConfig {
            address: pool.address,
            primary_asset: pool.primary_asset,
            min_primary_collateral,
            min_health_factor: pool.min_health_factor.get(),
            default_profit_bps: pool.default_profit_bps,
            force_fill: pool.force_fill,
            supported_bid: pool.supported_bid,
            supported_lot: pool.supported_lot,
            profits: pool.profits,
        });
    }
    Ok(pools)
}

/// What the binary does when it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RunMode {
    /// Follow the configured pools until shut down.
    Loop,
    /// Validate the configuration, print it redacted, and exit.
    CheckConfig,
}

/// Where the tracker gets its initial user set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedConfig {
    /// The analytics API's base URL; `None` when `SEED_URL` is empty.
    pub url: Option<String>,
    /// Only accounts at or below this health factor are seeded, 7 decimals.
    pub health_factor_max: i128,
    /// An optional static file of pool-to-account lists.
    pub file: Option<std::path::PathBuf>,
}

/// Everything the service needs, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Network, RPC and fee configuration.
    pub chain: ChainConfig,
    /// Postgres. May carry a password, so it never renders.
    pub database_url: Secret,
    /// Connections in the pool.
    pub database_max_connections: u32,
    /// The pools to follow.
    pub pools: Vec<PoolConfig>,
    /// Loop or validate.
    pub run_mode: RunMode,
    /// Whether submissions are suppressed. Still true by default.
    pub dry_run: bool,
    /// How often the poller asks for chain head.
    pub poll_interval: std::time::Duration,
    /// A user's row older than this many ledgers is refreshed.
    pub user_refresh_ledgers: u32,
    /// How many stale users to refresh per tick.
    pub refresh_batch: u32,
    /// How often the full scan reports the least healthy borrowers.
    pub full_scan_ledgers: u32,
    /// The health factor the full scan reports below, 7 decimals.
    pub scan_health_factor: i128,
    /// Seeding.
    pub seed: SeedConfig,
}
```

New `Args` fields, after Phase 2's:

```rust
    /// Path to the pools file. Give this or `--pools-toml`, not both.
    #[arg(long, env = "POOLS_FILE", conflicts_with = "pools_toml")]
    pub pools_file: Option<std::path::PathBuf>,

    /// The pools file's contents inline, for environments with no volume.
    #[arg(long, env = "POOLS_TOML")]
    pub pools_toml: Option<String>,

    /// What to do at startup.
    #[arg(long, env = "RUN_MODE", value_enum, default_value = "loop")]
    pub run_mode: RunMode,

    /// Connections in the database pool.
    #[arg(long, env = "DATABASE_MAX_CONNECTIONS", default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub database_max_connections: u32,

    /// How often to ask the RPC for chain head, in milliseconds.
    #[arg(long, env = "POLL_INTERVAL_MS", default_value_t = 1_000, value_parser = clap::value_parser!(u64).range(100..=60_000))]
    pub poll_interval_ms: u64,

    /// A tracked user whose row is older than this many ledgers is
    /// refreshed, so accrued interest is never missed.
    #[arg(long, env = "USER_REFRESH_LEDGERS", default_value_t = 241_920)]
    pub user_refresh_ledgers: u32,

    /// How many stale users to refresh per tick.
    #[arg(long, env = "REFRESH_BATCH", default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1_000))]
    pub refresh_batch: u32,

    /// How often to report the least healthy borrowers, in ledgers.
    #[arg(long, env = "FULL_SCAN_LEDGERS", default_value_t = 1_200, value_parser = clap::value_parser!(u32).range(1..))]
    pub full_scan_ledgers: u32,

    /// The health factor that scan reports below.
    #[arg(long, env = "SCAN_HF_THRESHOLD", default_value = "1.2")]
    pub scan_hf_threshold: Decimal7,

    /// The analytics API the tracker seeds from. Empty disables it.
    #[arg(long, env = "SEED_URL", default_value = "https://api.blend.templarfi.org")]
    pub seed_url: String,

    /// Only accounts at or below this health factor are seeded.
    #[arg(long, env = "SEED_HF_MAX", default_value = "10")]
    pub seed_hf_max: Decimal7,

    /// An optional static file of pool-to-account lists.
    #[arg(long, env = "SEED_FILE")]
    pub seed_file: Option<std::path::PathBuf>,
```

and the builders:

```rust
impl Args {
    /// The service configuration, reading both secrets from the environment.
    pub fn service(&self) -> Result<ServiceConfig, LiquidatorError> {
        self.service_with_secrets(
            std::env::var("DATABASE_URL").ok().filter(|url| !url.is_empty()),
            std::env::var("RPC_API_KEY").ok().filter(|key| !key.is_empty()),
        )
    }

    /// What `service` does after reading the environment, separated so
    /// tests never touch process-global state.
    pub fn service_with_secrets(
        &self,
        database_url: Option<String>,
        rpc_api_key: Option<String>,
    ) -> Result<ServiceConfig, LiquidatorError> {
        let chain = self.chain_with_secret(rpc_api_key)?;
        let database_url = database_url
            .ok_or_else(|| LiquidatorError::Config("DATABASE_URL is required".to_string()))?;
        let pools = match (&self.pools_file, &self.pools_toml) {
            (Some(path), None) => {
                let text = std::fs::read_to_string(path).map_err(|error| {
                    LiquidatorError::Config(format!("pools file {}: {error}", path.display()))
                })?;
                parse_pools(&text)?
            }
            (None, Some(text)) => parse_pools(text)?,
            _ => {
                return Err(LiquidatorError::Config(
                    "one of POOLS_FILE or POOLS_TOML is required".to_string(),
                ))
            }
        };
        Ok(ServiceConfig {
            chain,
            database_url: Secret::new(database_url),
            database_max_connections: self.database_max_connections,
            pools,
            run_mode: self.run_mode,
            dry_run: self.dry_run,
            poll_interval: std::time::Duration::from_millis(self.poll_interval_ms),
            user_refresh_ledgers: self.user_refresh_ledgers,
            refresh_batch: self.refresh_batch,
            full_scan_ledgers: self.full_scan_ledgers,
            scan_health_factor: self.scan_hf_threshold.get(),
            seed: SeedConfig {
                url: Some(self.seed_url.clone()).filter(|url| !url.is_empty()),
                health_factor_max: self.seed_hf_max.get(),
                file: self.seed_file.clone(),
            },
        })
    }
}
```

`.env.example` gains the pools, run-mode, cadence and seed knobs with the same explanatory comments, and a `pools.example.toml` reference.

- [ ] **Step 4: Run the tests, then `make check` and commit**

Run: `cargo test --lib config`
Expected: the new tests pass alongside Phase 2's.

```bash
git add src/config.rs .env.example
git commit -m "feat(config): pools file, run mode and the service configuration" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: The ledger poller

**Files:**
- Create: `src/ledger.rs`
- Modify: `src/liquidator.rs` (declare `pub mod ledger;`, add `LiquidatorError::Ledger`)

**Interfaces:**
- Consumes: `chain::rpc::{RpcClient, EventQuery, Events, Event}`, `chain::xdr::decode_pool_event`, `chain::ChainError`, `store::{Store, Cursor, events_cursor, StoreError}`.
- Produces:
  - `pub struct ledger::LedgerTick { pub sequence: u32, pub close_time: u64 }`.
  - `pub enum ledger::PollerMessage { Event { pool: String, ledger: u32, event: PoolEvent }, Tick { pool: String, tick: LedgerTick }, Gap { pool: String, from: u32, oldest: u32 } }`.
  - `pub struct ledger::PollerConfig { pub poll_interval: Duration, pub page_limit: u32, pub min_backoff: Duration, pub max_backoff: Duration }` with `PollerConfig::new(poll_interval: Duration)` (page limit 200, backoff one to thirty seconds, the spec's numbers).
  - `pub enum ledger::LedgerError { Chain(ChainError), Store(StoreError), Closed }`.
  - `pub struct ledger::LedgerPoller<'a>` with `LedgerPoller::new(rpc: &'a RpcClient, store: &'a Store, pool: &'a str, config: PollerConfig)`, `pub async fn run(&self, sender: mpsc::Sender<PollerMessage>, shutdown: watch::Receiver<bool>) -> Result<(), LedgerError>`, and `pub async fn poll_once(&self, sender: &mpsc::Sender<PollerMessage>) -> Result<Option<LedgerTick>, LedgerError>`.

**Behaviour the tests pin:**
- The first poll with no cursor starts at chain head: the bot follows from now, and the user set comes from seeding, not from replaying all of history.
- A cursor whose next ledger is older than the RPC's `oldestLedger` emits `Gap` and restarts at `oldestLedger`, never silently skipping.
- Events are sent in order, then the `Tick`; the cursor is written only after the tick is sent, so a crash re-reads a ledger rather than skipping one.
- An event the decoder does not model is dropped without stopping the stream; an event from a contract other than this pool is ignored.
- An RPC failure leaves the cursor where it was and backs off.

- [ ] **Step 1: Write the failing tests**

Create `src/ledger.rs` with the module doc, stubs, and:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::script::ScriptedRpc;
    use crate::chain::xdr::encode::{address, i128_val, symbol, to_base64, vec as sc_vec};
    use serde_json::json;
    use std::time::Duration;

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

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
        let topics = [symbol("borrow").unwrap(), address(USDC).unwrap(), address(USER).unwrap()];
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
        rpc.expect("getEvents", json!({"latestLedger": 64_291_297, "cursor": null, "events": []}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let poller = LedgerPoller::new(&client, &store, POOL, config());

        let tick = poller.poll_once(&sender).await.expect("poll").expect("a tick");
        assert_eq!((tick.sequence, tick.close_time), (64_291_297, 1_788_645_403));
        // The stream starts at head, so the first getEvents asks for it.
        assert_eq!(rpc.calls("getEvents")[0]["startLedger"], 64_291_297);
        assert!(matches!(receiver.try_recv(), Ok(PollerMessage::Tick { .. })));
        assert_eq!(
            store.cursor(&events_cursor(POOL)).await.expect("cursor"),
            Some(Cursor { ledger: 64_291_297, paging_token: None })
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn events_arrive_before_the_tick_and_the_cursor_follows(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        store
            .set_cursor(&events_cursor(POOL), &Cursor { ledger: 100, paging_token: None })
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
        let poller = LedgerPoller::new(&client, &store, POOL, config());

        poller.poll_once(&sender).await.expect("poll");
        // It asked for the ledger after the cursor, not the cursor itself.
        assert_eq!(rpc.calls("getEvents")[0]["startLedger"], 101);
        for expected_ledger in [101, 103] {
            match receiver.try_recv().expect("an event") {
                PollerMessage::Event { pool, ledger, event } => {
                    assert_eq!(pool, POOL);
                    assert_eq!(ledger, expected_ledger);
                    assert!(matches!(event, PoolEvent::Borrow { .. }));
                }
                other => panic!("expected an event, got {other:?}"),
            }
        }
        assert!(matches!(receiver.try_recv(), Ok(PollerMessage::Tick { .. })));
        assert_eq!(
            store.cursor(&events_cursor(POOL)).await.expect("cursor").expect("set").ledger,
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
            .set_cursor(&events_cursor(POOL), &Cursor { ledger: 10, paging_token: None })
            .await
            .expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(500_000, 400_000));
        rpc.expect("getLatestLedger", latest(500_000, 1_788_645_403));
        rpc.expect("getEvents", json!({"latestLedger": 500_000, "cursor": null, "events": []}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let poller = LedgerPoller::new(&client, &store, POOL, config());

        poller.poll_once(&sender).await.expect("poll");
        match receiver.try_recv().expect("a gap") {
            PollerMessage::Gap { pool, from, oldest } => {
                assert_eq!((pool.as_str(), from, oldest), (POOL, 10, 400_000));
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
            .set_cursor(&events_cursor(POOL), &Cursor { ledger: 100, paging_token: None })
            .await
            .expect("cursor");
        let mut unmodelled = borrow_event(101, 1);
        unmodelled["topic"] = json!([to_base64(&symbol("gulp").unwrap()).unwrap()]);
        let mut foreign = borrow_event(101, 2);
        foreign["contractId"] = json!(USDC);
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(101, 1));
        rpc.expect("getLatestLedger", latest(101, 1_788_645_403));
        rpc.expect("getEvents", json!({"latestLedger": 101, "cursor": null, "events": [unmodelled, foreign]}));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let poller = LedgerPoller::new(&client, &store, POOL, config());

        poller.poll_once(&sender).await.expect("poll");
        assert!(matches!(receiver.try_recv(), Ok(PollerMessage::Tick { .. })), "only the tick");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_rpc_failure_leaves_the_cursor_alone(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let before = Cursor { ledger: 100, paging_token: None };
        store.set_cursor(&events_cursor(POOL), &before).await.expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect_http("getHealth", 503);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, _receiver) = tokio::sync::mpsc::channel(16);
        let poller = LedgerPoller::new(&client, &store, POOL, config());

        assert!(matches!(poller.poll_once(&sender).await, Err(LedgerError::Chain(_))));
        assert_eq!(store.cursor(&events_cursor(POOL)).await.expect("cursor"), Some(before));
        Ok(())
    }

    /// Nothing new closed, so there is nothing to do and no cursor write.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_ledger_that_has_not_moved_is_a_no_op(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let at_head = Cursor { ledger: 103, paging_token: None };
        store.set_cursor(&events_cursor(POOL), &at_head).await.expect("cursor");
        let rpc = ScriptedRpc::start().await;
        rpc.expect("getHealth", health(103, 1));
        rpc.expect("getLatestLedger", latest(103, 1_788_645_403));
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let poller = LedgerPoller::new(&client, &store, POOL, config());

        assert_eq!(poller.poll_once(&sender).await.expect("poll"), None);
        assert!(receiver.try_recv().is_err(), "no messages");
        assert!(rpc.calls("getEvents").is_empty());
        assert_eq!(store.cursor(&events_cursor(POOL)).await.expect("cursor"), Some(at_head));
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
        let poller = LedgerPoller::new(&client, &store, POOL, config());
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            flag.send(true).expect("flag");
        });
        poller.run(sender, watch).await.expect("run");
        stopper.await.expect("stopper");
        Ok(())
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `make db-up && cargo test --lib ledger`
Expected: compile errors.

- [ ] **Step 3: Implement**

`src/ledger.rs`:

```rust
//! The clock. One poller per pool: it asks the RPC for chain head, reads
//! every pool event since its cursor, sends each decoded event followed by
//! the ledger's tick, and only then writes the cursor forward.
//!
//! That order is the invariant: a crash between sending and storing
//! re-reads a ledger, which the tracker handles because applying an event
//! twice is idempotent, while storing first would skip one silently. The
//! cursor never moves past what was sent, and an RPC failure moves it not
//! at all.
//!
//! A cursor older than the RPC's retained window cannot be caught up: the
//! events between are gone. The poller reports that as a `Gap` and restarts
//! at the window's edge, leaving the tracker to reseed rather than pretend
//! the missing ledgers held nothing.

use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::chain::rpc::{EventQuery, RpcClient};
use crate::chain::xdr::{decode_pool_event, PoolEvent};
use crate::chain::ChainError;
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

/// What a poller sends downstream, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

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
}

/// Follows one pool's events.
#[derive(Debug)]
pub struct LedgerPoller<'a> {
    rpc: &'a RpcClient,
    store: &'a Store,
    pool: &'a str,
    config: PollerConfig,
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
        }
    }

    /// Polls until `shutdown` flips, backing off on RPC failures. An RPC
    /// outage never advances the cursor, so nothing is skipped.
    pub async fn run(
        &self,
        sender: mpsc::Sender<PollerMessage>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), LedgerError> {
        let mut backoff = self.config.min_backoff;
        loop {
            if *shutdown.borrow_and_update() {
                return Ok(());
            }
            let wait = match self.poll_once(&sender).await {
                Ok(_) => {
                    backoff = self.config.min_backoff;
                    self.config.poll_interval
                }
                Err(LedgerError::Closed) => return Ok(()),
                Err(error) => {
                    tracing::warn!(pool = self.pool, %error, "poll failed; backing off");
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

    /// One pass: head, events since the cursor, then the tick and the
    /// cursor. `None` when the chain has not moved.
    pub async fn poll_once(
        &self,
        sender: &mpsc::Sender<PollerMessage>,
    ) -> Result<Option<LedgerTick>, LedgerError> {
        let health = self.rpc.health().await?;
        let head = self.rpc.latest_ledger().await?;
        let stored = self.store.cursor(&events_cursor(self.pool)).await?;

        // With no cursor the bot follows from now: history comes from
        // seeding, not from replaying every ledger the RPC still holds.
        let mut start = match &stored {
            None => head.sequence,
            Some(cursor) => cursor.ledger.saturating_add(1),
        };
        if let Some(cursor) = &stored {
            if start < health.oldest_ledger {
                tracing::warn!(
                    pool = self.pool,
                    from = cursor.ledger,
                    oldest = health.oldest_ledger,
                    "the cursor fell out of the RPC's retained window; reseeding"
                );
                send(sender, PollerMessage::Gap {
                    pool: self.pool.to_string(),
                    from: cursor.ledger,
                    oldest: health.oldest_ledger,
                })
                .await?;
                start = health.oldest_ledger;
            }
        }
        if start > head.sequence {
            return Ok(None);
        }

        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .rpc
                .events(&EventQuery {
                    start_ledger: cursor.is_none().then_some(start),
                    cursor: cursor.as_deref(),
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
                        send(sender, PollerMessage::Event {
                            pool: self.pool.to_string(),
                            ledger: event.ledger,
                            event: decoded,
                        })
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
            // A short page is the last one; the next poll starts from the
            // ledger after the tick.
            if count < usize::try_from(self.config.page_limit).unwrap_or(usize::MAX) {
                break;
            }
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        let tick = LedgerTick {
            sequence: head.sequence,
            close_time: head.close_time,
        };
        send(sender, PollerMessage::Tick {
            pool: self.pool.to_string(),
            tick,
        })
        .await?;
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

/// Sends downstream, turning a closed channel into `Closed` rather than an
/// error the caller would retry.
async fn send(
    sender: &mpsc::Sender<PollerMessage>,
    message: PollerMessage,
) -> Result<(), LedgerError> {
    sender.send(message).await.map_err(|_| LedgerError::Closed)
}
```

In `src/liquidator.rs` add `pub mod ledger;` and:

```rust
    /// The ledger poller failed.
    #[error("ledger: {0}")]
    Ledger(#[from] ledger::LedgerError),
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ledger`
Expected: 7 pass.

- [ ] **Step 5: `make sqlx-prepare`, `make check`, then commit**

```bash
git add src/ledger.rs src/liquidator.rs .sqlx
git commit -m "feat(ledger): a per-pool poller with cursors, gaps and backoff" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```


---

## Test scaffolding for Tasks 7 to 9

Tasks 7, 8 and 9 all need a pool that answers from the committed fixture and
a store to write into. Write this module once, in Task 7, as
`#[cfg(test)] pub(crate) mod harness;` under `src/` (declared from
`src/liquidator.rs` beside `fixture`), and let Tasks 8 and 9 use it. It is
the only piece of those three tasks that is fiddly rather than obvious, so
it is given in full; each test then reads as its own three or four
assertions.

```rust
//! Test scaffolding: a scripted RPC that answers from the committed mainnet
//! fixture, and the store to write what it says into.

use serde_json::{json, Value};

use crate::chain::script::ScriptedRpc;
use crate::chain::xdr::encode::to_base64;
use crate::chain::xdr::keys;
use crate::fixture::{mainnet_fixed_v2, text};

/// The fixture's pool, its two borrowers and its USDC reserve.
pub(crate) const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
pub(crate) const USER_ONE: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
pub(crate) const USER_TWO: &str = "GCIH7OYRDHJ3IOPFEM7DMUX3SXTVHOO2XSWLGBMSVQ3EIHPHYUTNJID3";

/// The golden health factors `chain::xdr::decode`'s test derives from the
/// same attested inputs. The fixture's oracle has 7 decimals, so
/// normalising to 7 decimals is the identity and these are also what the
/// store must hold.
pub(crate) const GOLDEN_HEALTH: [(&str, i128); 2] =
    [(USER_ONE, 10_070_767), (USER_TWO, 10_100_345)];

fn entry(key: &stellar_xdr::LedgerKey, xdr: &str) -> Value {
    json!({
        "key": to_base64(key).expect("key"),
        "xdr": xdr,
        "lastModifiedLedgerSeq": 1,
        "liveUntilLedgerSeq": 99_999_999_u32,
    })
}

fn simulation(return_xdr: &str, ledger: u32) -> Value {
    json!({
        "transactionData": crate::chain::script::transaction_data_b64(1),
        "events": [],
        "minResourceFee": "1",
        "results": [{"auth": [], "xdr": return_xdr}],
        "latestLedger": ledger,
    })
}

/// Scripts one complete `PoolReader::snapshot` for `accounts` at the
/// fixture's ledger: the shape read, the batched entry read, then the
/// oracle's `decimals` and one `lastprice` per reserve, in reserve-list
/// order. Accounts the fixture does not hold are simply absent from the
/// entry read, which is what the RPC does for a key with no entry.
pub(crate) fn script_snapshot(rpc: &ScriptedRpc, accounts: &[&str]) {
    let fixture = mainnet_fixed_v2();
    let ledger = fixture["ledger"].as_u64().expect("ledger");
    rpc.expect(
        "getLedgerEntries",
        json!({"latestLedger": ledger, "entries": [
            entry(&keys::instance(POOL).expect("key"), text(&fixture, &["instance_entry_xdr"])),
            entry(&keys::reserve_list(POOL).expect("key"), text(&fixture, &["res_list_entry_xdr"])),
        ]}),
    );
    let mut entries = Vec::new();
    for reserve in fixture["reserves"].as_array().expect("reserves") {
        let asset = reserve["asset"].as_str().expect("asset");
        entries.push(entry(
            &keys::reserve_config(POOL, asset).expect("key"),
            reserve["config_entry_xdr"].as_str().expect("config"),
        ));
        entries.push(entry(
            &keys::reserve_data(POOL, asset).expect("key"),
            reserve["data_entry_xdr"].as_str().expect("data"),
        ));
    }
    for user in fixture["users"].as_array().expect("users") {
        let account = user["account"].as_str().expect("account");
        if accounts.contains(&account) {
            entries.push(entry(
                &keys::positions(POOL, account).expect("key"),
                user["positions_entry_xdr"].as_str().expect("positions"),
            ));
        }
    }
    rpc.expect("getLedgerEntries", json!({"latestLedger": ledger, "entries": entries}));
    let ledger = u32::try_from(ledger).expect("ledger fits");
    rpc.expect(
        "simulateTransaction",
        simulation(text(&fixture, &["oracle_decimals_return_xdr"]), ledger),
    );
    for reserve in fixture["reserves"].as_array().expect("reserves") {
        rpc.expect(
            "simulateTransaction",
            simulation(reserve["lastprice_return_xdr"].as_str().expect("price"), ledger),
        );
    }
}

/// The fixture's ledger and close time, as a tick.
pub(crate) fn fixture_tick() -> crate::ledger::LedgerTick {
    let fixture = mainnet_fixed_v2();
    crate::ledger::LedgerTick {
        sequence: u32::try_from(fixture["ledger"].as_u64().expect("ledger")).expect("fits"),
        close_time: fixture["ledger_close_time"].as_u64().expect("close time"),
    }
}
```

`script_snapshot` mirrors `chain::pool`'s own `script_fixture` helper; if that
one has since changed shape, follow it rather than this copy, and say so in
the report.

---

### Task 7: The tracker — applying events and refreshing users

**Files:**
- Create: `src/tracker.rs`
- Modify: `src/liquidator.rs` (declare `pub mod tracker;`, add `LiquidatorError::Tracker`)

**Interfaces:**
- Consumes: `chain::pool::{PoolReader, PoolSnapshot}`, `chain::rpc::RpcClient`, `chain::xdr::{PoolEvent, AuctionType}`, `math::{mul_floor, SCALAR_7, MathError}`, `store::{Store, TrackedUser, TrackedAuction, StoreError}`, `ledger::LedgerTick`.
- Produces:
  - `pub enum tracker::TrackerError { Chain(ChainError), Store(StoreError), Math(MathError) }`.
  - `pub struct tracker::RefreshOutcome { pub tracked: usize, pub removed: usize }`.
  - `pub struct tracker::Tracker<'a>` with `Tracker::new(rpc: &'a RpcClient, store: &'a Store)`.
  - `Tracker::apply(&self, pool: &str, ledger: u32, event: &PoolEvent) -> Result<Vec<String>, TrackerError>` — writes the auction rows an auction event implies and returns the accounts the event names, which the caller refreshes at the next tick.
  - `Tracker::refresh(&self, pool: &str, accounts: &[String], tick: LedgerTick) -> Result<RefreshOutcome, TrackerError>` — one batched snapshot, then a row per account.
  - `Tracker::refresh_stale(&self, pool: &str, tick: LedgerTick, older_than: u32, batch: u32) -> Result<RefreshOutcome, TrackerError>`.

**The rules the tests pin, from the spec's "Discovery and refresh":**
- Every event that names a user makes that user a refresh candidate; `PoolEvent::affected_accounts` already decides which, including both the liquidated user and the filler on a fill.
- A refreshed account with no liabilities is **deleted**, not stored with an empty map: the table's size is the number of positions that could be liquidated.
- The health factor stored is normalised: `mul_floor(hf, SCALAR_7, snapshot.prices.scalar())`.
- Reserves are accrued to the **tick's close time**, so a user refreshed at ledger *N* is valued as the contract would value it in ledger *N*.
- `NewAuction` opens a row with `auction.block` as the start ledger and the event's `percent`, bid and lot.
- `FillAuction` with `fill_percent` at or above 100 deletes the row. Below 100 the remainder is **re-read from chain** with `PoolReader::auction` and written as the chain reports it, or deleted when the chain no longer has it. The bot never subtracts the filled side from the stored side: the contract owns that arithmetic and the entry is authoritative.
- `DeleteAuction` deletes the row.
- Applying the same event twice leaves the same state, because a crash between sending and storing a cursor replays a ledger.

- [ ] **Step 1: Write the failing tests**

Create `src/tracker.rs` with the module doc, stubs, and a test module that scripts the fixture ledger through `ScriptedRpc` the way `chain::pool`'s tests do (reuse that shape: the shape read, the batched entry read, then the oracle's `decimals` and one `lastprice` per reserve) and asserts:

```rust
    /// The fixture's two borrowers land in the store with the golden health
    /// factors, normalised to 7 decimals — the oracle's scalar is 10^7 here,
    /// so normalising is the identity and the stored values are the same
    /// numbers `chain::xdr::decode`'s test derives.
    #[sqlx::test(migrations = "./migrations")]
    async fn refreshing_the_fixtures_users_stores_their_golden_health_factors(db: sqlx::PgPool) -> sqlx::Result<()>

    /// An account the ledger has no positions entry for is not tracked, and
    /// one that repays everything is deleted rather than stored empty.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_account_without_liabilities_is_not_tracked(db: sqlx::PgPool) -> sqlx::Result<()>

    /// `apply` returns the accounts an event names and writes no user rows
    /// itself: refreshing is the tick's job.
    #[sqlx::test(migrations = "./migrations")]
    async fn apply_returns_the_accounts_an_event_names(db: sqlx::PgPool) -> sqlx::Result<()>

    /// A new auction opens a row at the auction's own block, a partial fill
    /// re-reads the remainder from chain, a full fill deletes, and a delete
    /// event deletes.
    #[sqlx::test(migrations = "./migrations")]
    async fn auction_events_open_reduce_and_close_the_row(db: sqlx::PgPool) -> sqlx::Result<()>

    /// Applying the same events twice ends in the same state.
    #[sqlx::test(migrations = "./migrations")]
    async fn applying_an_event_twice_is_idempotent(db: sqlx::PgPool) -> sqlx::Result<()>

    /// The refresh pass takes the oldest rows first and no more than the
    /// batch size.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_refresh_pass_takes_the_oldest_rows_up_to_the_batch(db: sqlx::PgPool) -> sqlx::Result<()>
```

Write each body in full: script the RPC, run the tracker, assert against `store.user(..)`, `store.auction(..)` and `store.count_users(..)`. Take the fixture through `crate::fixture::{mainnet_fixed_v2, text}` and the golden health factors `10_070_767` and `10_100_345` from `src/chain/xdr/decode.rs`'s test, which derives them from the same attested inputs. For the auction tests, build `PoolEvent` values directly rather than decoding them, and script `getLedgerEntries` for the auction key the way `chain::pool`'s auction test does.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `make db-up && cargo test --lib tracker`
Expected: compile errors.

- [ ] **Step 3: Implement**

`src/tracker.rs`, following the rules above. The shape:

```rust
//! Applying what the chain says to what the bot stores.
//!
//! The tracker is the only writer of the `users` and `auctions` tables, so
//! ordering is its own: it applies a ledger's events, then refreshes the
//! accounts those events named, in one batched read per pool per tick.
//!
//! Two rules make a replayed ledger harmless. Applying an event is
//! idempotent — every write is an upsert or a delete keyed by what the
//! event names — and a user's row is recomputed from chain rather than
//! adjusted, so an event applied twice cannot drift a balance. The chain,
//! not the event, is the source of every number the store holds.

use std::collections::BTreeMap;

use crate::chain::pool::PoolReader;
use crate::chain::rpc::RpcClient;
use crate::chain::xdr::PoolEvent;
use crate::chain::ChainError;
use crate::ledger::LedgerTick;
use crate::math::{mul_floor, MathError, SCALAR_7};
use crate::store::{Store, StoreError, TrackedAuction, TrackedUser};

/// A failure applying chain state to the store.
#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    /// Reading the chain failed.
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
    /// Writing the store failed.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// Normalising a health factor overflowed.
    #[error("math: {0}")]
    Math(#[from] MathError),
}

/// What one refresh did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshOutcome {
    /// Accounts written or updated.
    pub tracked: usize,
    /// Accounts removed because they no longer owe anything.
    pub removed: usize,
}

/// Applies events and refreshes users for one store.
#[derive(Debug, Clone, Copy)]
pub struct Tracker<'a> {
    rpc: &'a RpcClient,
    store: &'a Store,
}

impl<'a> Tracker<'a> {
    /// A tracker writing to `store` and reading through `rpc`.
    #[must_use]
    pub fn new(rpc: &'a RpcClient, store: &'a Store) -> Self {
        Self { rpc, store }
    }

    /// Applies one event's auction bookkeeping and returns the accounts it
    /// names, for the caller to refresh at the tick.
    pub async fn apply(
        &self,
        pool: &str,
        ledger: u32,
        event: &PoolEvent,
    ) -> Result<Vec<String>, TrackerError> {
        match event {
            PoolEvent::NewAuction {
                auction_type,
                user,
                percent,
                auction,
            } => {
                self.store
                    .upsert_auction(&TrackedAuction {
                        pool: pool.to_string(),
                        account: user.clone(),
                        auction_type: *auction_type,
                        start_ledger: auction.block,
                        fill_ledger: None,
                        percent: *percent,
                        bid: auction.bid.clone(),
                        lot: auction.lot.clone(),
                        updated_ledger: ledger,
                    })
                    .await?;
            }
            PoolEvent::FillAuction {
                auction_type,
                user,
                fill_percent,
                ..
            } => {
                if *fill_percent >= 100 {
                    self.store.delete_auction(pool, user, *auction_type).await?;
                } else {
                    // A partial fill leaves a remainder the contract
                    // computed; read it rather than subtracting.
                    let reader = PoolReader::new(self.rpc, pool);
                    match reader.auction(user, *auction_type).await? {
                        Some((at, remaining)) => {
                            self.store
                                .upsert_auction(&TrackedAuction {
                                    pool: pool.to_string(),
                                    account: user.clone(),
                                    auction_type: *auction_type,
                                    start_ledger: remaining.block,
                                    fill_ledger: None,
                                    percent: 100,
                                    bid: remaining.bid,
                                    lot: remaining.lot,
                                    updated_ledger: at,
                                })
                                .await?;
                        }
                        None => {
                            self.store.delete_auction(pool, user, *auction_type).await?;
                        }
                    }
                }
            }
            PoolEvent::DeleteAuction {
                auction_type, user, ..
            } => {
                self.store.delete_auction(pool, user, *auction_type).await?;
            }
            _ => {}
        }
        Ok(event
            .affected_accounts()
            .into_iter()
            .map(str::to_string)
            .collect())
    }

    /// Re-reads `accounts` from chain in one snapshot and writes each row,
    /// deleting the ones that no longer owe anything.
    pub async fn refresh(
        &self,
        pool: &str,
        accounts: &[String],
        tick: LedgerTick,
    ) -> Result<RefreshOutcome, TrackerError> {
        if accounts.is_empty() {
            return Ok(RefreshOutcome::default());
        }
        let borrowed: Vec<&str> = accounts.iter().map(String::as_str).collect();
        let snapshot = PoolReader::new(self.rpc, pool).snapshot(&borrowed).await?;
        let mut outcome = RefreshOutcome::default();
        for account in accounts {
            let positions = snapshot.positions.get(account);
            let owes = positions.is_some_and(|positions| !positions.liabilities.is_empty());
            let health = if owes {
                snapshot
                    .position_data(account, tick.close_time)?
                    .and_then(|data| data.health_factor().transpose())
                    .transpose()?
            } else {
                None
            };
            match (positions, health) {
                (Some(positions), Some(health)) => {
                    self.store
                        .upsert_user(&TrackedUser {
                            pool: pool.to_string(),
                            account: account.clone(),
                            health_factor: mul_floor(health, SCALAR_7, snapshot.prices.scalar())?,
                            collateral: positions.collateral.clone(),
                            liabilities: positions.liabilities.clone(),
                            updated_ledger: tick.sequence,
                        })
                        .await?;
                    outcome.tracked += 1;
                }
                _ => {
                    if self.store.delete_user(pool, account).await? {
                        outcome.removed += 1;
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// Refreshes up to `batch` users whose row predates `older_than`, so a
    /// long-idle borrower's accrued interest is never missed.
    pub async fn refresh_stale(
        &self,
        pool: &str,
        tick: LedgerTick,
        older_than: u32,
        batch: u32,
    ) -> Result<RefreshOutcome, TrackerError> {
        let limit = i64::from(batch);
        let stale = self.store.users_stale(pool, older_than, limit).await?;
        let accounts: Vec<String> = stale.into_iter().map(|user| user.account).collect();
        self.refresh(pool, &accounts, tick).await
    }
}
```

`position_data` returns `Result<Option<PositionData>, ChainError>` and `health_factor` returns `Result<Option<i128>, MathError>`; the `and_then`/`transpose` dance above flattens both into `Option<i128>` while keeping both error types. If it does not compile as written, use a `match` — clarity beats combinators here, and the reviewer will read it.

In `src/liquidator.rs` add `pub mod tracker;` and a `Tracker` error variant.

- [ ] **Step 4: Run the tests, `make sqlx-prepare`, `make check`, then commit**

```bash
git add src/tracker.rs src/liquidator.rs .sqlx
git commit -m "feat(tracker): apply pool events and refresh borrowers from chain" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Seeding

**Files:**
- Modify: `src/tracker.rs`
- Create: `pools.example.toml`, `seed.example.toml`

**Interfaces:**
- Produces:
  - `pub enum tracker::SeedError { Http(reqwest::Error), Status { status: u16 }, Shape(String), File(String) }`.
  - `pub struct tracker::AnalyticsSeed` with `AnalyticsSeed::new(base_url: &str, health_factor_max: i128) -> Result<Self, SeedError>`; the health factor is rendered from 7-decimal fixed point into the query's `healthFactorMax`.
  - `pub struct tracker::FileSeed` with `FileSeed::load(path: &Path) -> Result<Self, SeedError>`.
  - `pub enum tracker::SeedSource { Analytics(AnalyticsSeed), File(FileSeed) }` with `pub async fn accounts(&self, pool: &str) -> Result<Vec<String>, SeedError>`.
  - `Tracker::seed(&self, pool: &str, sources: &[SeedSource], tick: LedgerTick, batch: u32) -> Result<RefreshOutcome, TrackerError>` — collects from every source, deduplicates, and refreshes in batches of `batch`; a source that fails is logged and skipped, because the spec makes a failed seed a warning, not a startup failure.

**Wire facts** (captured 2026-09-09, repeated here so the implementer does not have to re-derive them): `GET {base}/v1/analytics/state/positions?healthFactorMax={hf}&poolId={pool}&limit=500`, then `&cursor={nextCursor}` for each further page. The response is `{ "positions": [ { "accountId": "G…", … } ], "nextCursor": "…"|null, … }`; `nextCursor` is `null` on the last page and absent for an unknown pool, so `Option<String>` handles both. Only `accountId` is read: the API's `healthFactor` is a third party's float, and every seeded account is re-valued from chain before the bot trusts it. Pages are fetched with a short pause to stay under the anonymous rate limit, and a page cap stops a cursor that never terminates.

**The seed file** is TOML, a table of pool address to account list:

```toml
[accounts]
"CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD" = [
  "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE",
]
```

- [ ] **Step 1: Write the failing tests**

Append to `src/tracker.rs`'s test module. The analytics tests drive a `wiremock::MockServer` directly (the scripted RPC server answers JSON-RPC; this is a REST endpoint), asserting:

```rust
    /// Two pages, then a null cursor: every accountId, in order, once.
    #[tokio::test]
    async fn the_analytics_source_follows_the_cursor_to_the_end()

    /// A null `nextCursor` and an absent one both end the walk, and an
    /// unknown pool's empty `positions` is not an error.
    #[tokio::test]
    async fn an_unknown_pool_seeds_nothing_without_failing()

    /// A non-200, and a 200 whose body is not the documented shape, are
    /// errors that name what happened rather than seeding silently.
    #[tokio::test]
    async fn a_bad_status_or_shape_is_an_error()

    /// The query carries the pool, the limit and the health factor rendered
    /// from 7-decimal fixed point, and the cursor only on later pages.
    #[tokio::test]
    async fn the_query_carries_the_pool_and_the_rendered_health_factor()

    #[test]
    fn the_seed_file_reads_a_pools_account_list()

    #[test]
    fn a_seed_file_that_is_not_the_documented_shape_is_an_error()

    /// Every source's accounts are refreshed once, deduplicated, and a
    /// failing source is skipped rather than failing the seed.
    #[sqlx::test(migrations = "./migrations")]
    async fn seeding_refreshes_every_account_once_and_survives_a_failing_source(db: sqlx::PgPool) -> sqlx::Result<()>
```

- [ ] **Step 2: Run the tests to verify they fail; Step 3: implement**

Render the health factor with a small helper that divides the 7-decimal fixed point back into a decimal string without a float (`whole = value / SCALAR_7`, `fraction = value % SCALAR_7`, trailing zeros trimmed). Cap the walk at 200 pages and log a warning if the cap is hit. Between pages sleep 200 ms. Use the crate's existing `reqwest::Client` construction style (timeout, connect timeout, user agent) from `chain::rpc`.

- [ ] **Step 4: Write the example files**

`pools.example.toml` is the spec's pools file with the mainnet pool, XLM as the primary asset, and comments explaining each field. `seed.example.toml` is the seed file above. Both are referenced from `.env.example` and `CLAUDE.md`.

- [ ] **Step 5: `make check`, then commit**

```bash
git add src/tracker.rs pools.example.toml seed.example.toml .sqlx
git commit -m "feat(tracker): seed the user set from the analytics API and a file" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: The service and the binary

**Files:**
- Create: `src/service.rs`
- Modify: `src/main.rs`, `src/liquidator.rs`

**Interfaces:**
- Produces:
  - `pub struct service::Service` with `pub async fn run(config: ServiceConfig) -> Result<(), LiquidatorError>` and `pub async fn check_config(config: &ServiceConfig) -> Result<Vec<String>, LiquidatorError>` (returns warnings; an error is a failed validation).
  - `pub struct service::PoolValidation { pub pool: String, pub reserves: usize, pub backstop: String }` for what `check-config` prints.

**Behaviour:**
1. **Validate** (both modes): every pool loads from chain through `PoolReader::snapshot(&[])`; all pools report the same backstop, else an error naming both; each pool's `primary_asset` is a reserve, is enabled, and has a positive collateral factor; every explicitly listed `supported_bid`/`supported_lot` asset (anything but `"*"`) is a reserve. Warnings, not errors: a pool whose status is not active, and an asset the oracle has no price for.
2. **`check-config`** prints the resolved configuration with every secret redacted, prints the per-pool validation, and exits 0, or prints the failure and exits 2.
3. **`loop`** connects the store, migrates, validates, seeds each pool whose `users` count is zero or whose cursor is missing, then spawns one `LedgerPoller` per pool and one tracker task over a shared `mpsc` channel, and waits for a shutdown signal.
4. **The tracker task**: accumulates the accounts that a ledger's events named; on `Tick` it refreshes them, then runs the stale-refresh pass, then on the `full_scan_ledgers` cadence logs the least healthy borrowers (`users_below_health(pool, scan_health_factor, 20)`) and the tracked count — this is the phase's "print tracked users". On `Gap` it reseeds that pool.
5. **Cadences carry a per-instance random phase** (`rand`) so several bots on one pool do not fire on the same ledger: the full scan fires when `(ledger + phase) % full_scan_ledgers == 0`.
6. **Shutdown**: `tokio::signal::ctrl_c` and, on Unix, `SIGTERM`, flip a `watch` flag; every task returns; a second signal exits immediately with code 130. The channel is drained before returning so a ledger already read is applied.
7. `main.rs` builds the configuration, sets up logging, dispatches on `RunMode`, and exits 0, 1 (fatal) or 2 (configuration).

- [ ] **Step 1: Write the failing tests**

In `src/service.rs`'s test module, against `ScriptedRpc` and `#[sqlx::test]`:

```rust
    /// Validation accepts the fixture pool and reports its reserves.
    #[sqlx::test(migrations = "./migrations")]
    async fn validation_accepts_a_pool_that_loads_from_chain(db: sqlx::PgPool) -> sqlx::Result<()>

    /// Two pools with different backstops is a configuration error naming
    /// both, because the filler's positions are shared across them.
    #[sqlx::test(migrations = "./migrations")]
    async fn pools_with_different_backstops_are_refused(db: sqlx::PgPool) -> sqlx::Result<()>

    /// A primary asset that is not a reserve, or has no collateral factor,
    /// is refused with a message naming the asset.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_primary_asset_that_is_not_usable_collateral_is_refused(db: sqlx::PgPool) -> sqlx::Result<()>

    /// A supported asset that is not a reserve is refused; `"*"` is not
    /// checked against anything.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_supported_asset_is_refused_and_a_wildcard_is_not(db: sqlx::PgPool) -> sqlx::Result<()>

    /// The scan cadence fires once per period and its phase depends on the
    /// instance, so two bots do not fire on the same ledger.
    #[test]
    fn the_scan_cadence_fires_once_per_period_at_an_instance_specific_phase()
```

- [ ] **Step 2: Run to verify failure; Step 3: implement; Step 4: run**

Keep `run`'s task wiring small enough to read: a `spawn` per poller, one tracker loop, a `watch` flag, and a `JoinSet` awaited at the end. If clippy's `too_many_lines` fires, split the tracker loop's body into `handle_message`.

- [ ] **Step 5: `make check`, then commit**

```bash
git add src/service.rs src/main.rs src/liquidator.rs .sqlx
git commit -m "feat(service): validate, seed, follow pools and shut down cleanly" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 10: The demonstration and the documentation

**Files:**
- Modify: `CLAUDE.md`, `CHANGELOG.md`, `README.md`, `.env.example`

- [ ] **Step 1: Run the bot against mainnet and keep the output**

```bash
make db-up
export DATABASE_URL=postgres://liquidator:liquidator@127.0.0.1:55432/liquidator
export NETWORK=mainnet RPC_URL=https://mainnet.sorobanrpc.com
cargo run --bin liquidator -- --pools-file pools.example.toml --run-mode check-config
timeout 120 cargo run --bin liquidator -- --pools-file pools.example.toml --full-scan-ledgers 5
```

Expected: `check-config` prints the resolved configuration and the pool's reserves and exits 0; the loop seeds from the analytics API, follows the pool, and logs the tracked count and the least healthy borrowers. Paste both outputs into the task report; they go into the pull request as the phase's demonstration. `DRY_RUN` stays true throughout and nothing is signed.

- [ ] **Step 2: Update `CLAUDE.md`**

Status: Phase 3 landed the store, the ledger poller and the tracker, so the bot follows pools and tracks borrowers; it still creates no auctions and fills nothing. Module map: entries for `store.rs`, `ledger.rs`, `tracker.rs`, `service.rs`, and `migrations/`. Orientation commands: `make db-up` before `make check`, and `make sqlx-prepare` after changing a query. New gotchas:

- The query macros are checked at compile time, so a build needs either a database (`make db-up && sqlx migrate run`) or the committed `.sqlx` metadata (`SQLX_OFFLINE=true`, which the Dockerfile sets). Change a query and `make sqlx-prepare`, or the Docker build fails on stale metadata while the local build passes.
- `i128` does not fit any Postgres integer. Amounts and health factors cross the boundary as decimal text (`$n::text::numeric` out, `::text` back). A query that binds one as a number is a rounding bug waiting to happen.
- Store tests need a live Postgres and are not skipped without one: `#[sqlx::test]` creates a database per test. `make db-up` first.
- A user row exists only while the account owes something, so `count(*)` on `users` is the number of positions that could be liquidated, not the number of accounts ever seen.

- [ ] **Step 3: Update `CHANGELOG.md` and `README.md`**

Changelog entries under Unreleased for: the Postgres store with embedded migrations and compile-time checked queries; the per-pool ledger poller with cursors, gap detection and backoff; the tracker with event application, chain-authoritative refresh and seeding; the pools file and the new knobs; `check-config`; and the compose Postgres service. README's status line gains the phase.

- [ ] **Step 4: `make check`, then commit**

```bash
git add CLAUDE.md CHANGELOG.md README.md .env.example
git commit -m "docs: the store, the poller and the tracker in the module map and changelog" \
           -m "Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Phase checklist

- [ ] Every task's `make check` green with Postgres up; `cargo deny check` green.
- [ ] `.sqlx` current (`cargo sqlx prepare --check`) and the Docker build green with no database in reach.
- [ ] The tracker test lands on the same golden health factors Phase 1 and Phase 2 derive from the fixture.
- [ ] The poller's cursor never advances past what was sent, proven by a test.
- [ ] `check-config` and a live loop run against mainnet are in the pull request.
- [ ] The pull request is opened against `main` with a summary of the rulings above.

### Correction after the Task 3+4 review

`TrackedAuction::percent` is `chain::xdr::encode::FillPercent`, not a bare
`u32`: the plan's own doc comment claimed the contract's 1-to-100 range while
nothing enforced it, and Phase 2 already built the validated newtype for
exactly that range. `percent_from_code` returns a `FillPercent`, and
`migrations/0001_initial.sql` carries `CHECK` constraints on `percent` and
`auction_type` as an independent second line against a row this crate did not
write. Because those constraints make a corrupt row unconstructible through
SQL, the two corruption tests are direct unit tests of `auction_type_from_code`
and `percent_from_code` rather than round trips. Amending `0001` rather than
adding a migration is deliberate: nothing is deployed, and sqlx's per-migration
checksum makes an already-migrated local database fail loudly — run
`make db-reset && make db-up && sqlx migrate run` if you hold one (landed in
`d3a8e37`).
