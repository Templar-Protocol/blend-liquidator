# blend-liquidator: bot design

**Date:** 2026-09-04
**Status:** design approved section by section in review; written spec pending
review
**Predecessor:** [2026-09-03 repository scaffold](2026-09-03-blend-liquidator-scaffold-design.md)

## What this is

The design of the liquidation bot this repository exists to hold: a Rust
service that watches a configured set of [Blend Protocol](https://blend.capital)
v2 lending pools on Stellar, creates user liquidation auctions for underwater
borrowers, fills those auctions once they are profitable at the pool's own
oracle prices and safe for the bot's own health factor, and unwinds what it
takes over. It is a public, generic bot that any operator can run; Templar's
own deployment lives in a separate private infrastructure repository and is
covered by its own specification.

Two reference implementations shaped this design and are the right things to
read next to it:

- [Templar-Protocol/templar-liquidator](https://github.com/Templar-Protocol/templar-liquidator),
  this organisation's NEAR bot, supplies the engineering posture: module per
  stage, dry-run as the only default, typed reservations and settlement,
  graceful shutdown, readiness-only health, redacted configuration, tests
  that pin contract behaviour.
- [script3/auctioneer-bot](https://github.com/script3/auctioneer-bot),
  Blend's own maintained v2 auctioneer and filler, supplies the venue-specific
  heuristics: what makes a position liquidatable, how to size an auction, when
  an auction is worth filling, how to keep the filler healthy, and how to
  unwind afterwards. The 2024 v1 bot
  ([blend-capital/liquidation-bot](https://github.com/blend-capital/liquidation-bot))
  targets a contract API that no longer exists and is used only as a second
  opinion on the auction mathematics.

The venue facts this design depends on were verified against
[blend-capital/blend-contracts-v2](https://github.com/blend-capital/blend-contracts-v2)
at tag `v2.0.0`; the implementation must re-verify them against the deployed
pool WASM before mainnet.

## Decisions

| Question | Decision |
|---|---|
| Roles | Auctioneer and filler for **user liquidation auctions**; `bad_debt` calls for users with debt and no collateral. No bad-debt or interest auctions. |
| Capital model | Inventory: a primary asset supplied as collateral in each pool plus liability assets held in the wallet. No flash loans, no receiver contract. |
| After a fill | Unwind to the wallet and hold. No swaps; a swap seam is kept, no venue shipped. |
| Borrower discovery | Pool events with cursor replay, seeded from the public Blend analytics API and an optional static file. |
| Pricing | The pool's own SEP-40 oracle, with a configurable profit margin per pool and per asset set. No external feeds. |
| Pools | An explicit configured list with per-pool settings. No pool discovery. |
| Keys | Filler key required; auctioneer key optional, defaulting to the filler key. |
| Notifications | Telegram behind a channel trait, as in Templar. |
| Testing | Unit tests against contract-derived fixtures plus an ignored local Stellar sandbox integration test. |
| Architecture | One process, a ledger poller as the clock, stage-per-module, two tasks sharing a store, one submission queue per key. |
| Store | Postgres only, through `sqlx`; Cloud SQL in production, a `postgres:16` container in the generic compose file. |
| Deployment | Two repositories: this one is public and generic; a private `blend-liquidator-infra` deploys it to its own GCP project as a single always-on Cloud Run service with Cloud SQL. This spec fixes only the contract between them. |
| Implementer | The implementation plan will be executed by Claude Opus; phases are sized so each lands as one reviewable pull request. |

## 1. Scope and invariants

### What v1 does

For each configured pool:

1. **Tracks borrowers.** Learns users from pool events, refreshes their
   positions from chain, keeps every user that has liabilities, and prioritises
   the ones closest to liquidation.
2. **Creates liquidation auctions.** When a tracked user's effective collateral
   falls below its effective liabilities by the configured margin, computes
   the percent and asset lists that bring the position back to a healthy
   target, simulates the creation, and submits it.
3. **Reports bad debt.** When a tracked user has liabilities and no collateral
   left, submits the pool's `bad_debt` call. It costs one transaction, needs no
   capital, and is the pool-safety courtesy Script3's bot performs.
4. **Fills liquidation auctions**, created by anyone, once the auction's lot is
   worth at least the bid plus the configured margin at oracle prices and the
   fill fits inside the filler's health-factor headroom.
5. **Unwinds** after a fill: repays the debt it took over from wallet balances
   and withdraws the collateral it received, down to a configured floor of its
   primary asset.

### What v1 does not do

No bad-debt or interest auctions in either role. No swaps. No flash loans. No
external price feeds. No pool discovery. Each has a named seam or configuration
extension point (section 11) so it can be added without restructuring.

### Safety invariants carried from Templar

- **Dry-run is the default** and fails closed at three layers: the strict
  `DRY_RUN` parser, the executor (a live submission cannot exist without a
  live reservation), and the submission queue (refuses to sign in dry-run).
- **Secrets arrive only through the environment.** They never appear in argv,
  logs, panics, or `Debug` output; both configuration structs implement
  `Debug` by hand and tests assert the redaction.
- **Every transaction is simulated before it is signed.** A simulation that
  returns a contract error is a skip with a decoded reason, never a blind send.
- **SIGTERM is graceful:** finish in-flight submissions, start nothing new,
  drain notifications, exit 0. A second signal exits 130.
- **`/healthz` is readiness, not liveness.** It reports healthy while the
  ledger poller is close to chain head and the store answers. Liveness is a
  separate `/livez` that only proves the loop is not wedged, so an RPC outage
  can never restart-loop the bot.
- **The bot never builds a request set that leaves its own health factor
  below the configured minimum**, so it cannot liquidate itself.
- **Matches at funds-safety decision points are exhaustive**, with no wildcard
  arm, so a new error or outcome variant forces a decision at compile time.

### Invariants specific to Blend

- **Money is integer.** Token amounts are `i128` in each asset's own decimals;
  b-rates and d-rates are 12 decimals; collateral and liability factors are 7
  decimals; prices are in the oracle's decimals; health factors are computed
  in the oracle's decimal scale exactly as the contract does. Floats appear
  only in log formatting and in configuration values expressed as decimals,
  which are converted to fixed point at parse time.
- **The bot never fills its own auctions and never creates an auction for an
  address it controls.** Both are contract errors anyway; refusing locally
  saves a simulation.
- **An auction past ledger 400 is filled only under `force_fill`.** From that
  point the bid is zero and the lot is complete; there is nothing left to wait
  for, but the fill is still a position takeover that must pass the health
  check.
- **State is rebuildable.** Everything in the store can be reconstructed from
  the seed source, the RPC's retained events, and chain reads. Durable state
  is a convenience for restarts and an audit trail, not a correctness
  requirement.

## 2. Architecture

### Process shape

One binary, one crate, one tokio runtime. Seven long-lived tasks:

| Task | Owns |
|---|---|
| ledger poller | chain head, event stream, cursor |
| tracker | applying events and refreshes to the store, flagging users to recheck |
| auctioneer | scheduled scans, auction creation and bad-debt submissions |
| filler | evaluating open auctions, planning and executing fills, unwinding |
| submission queues | one per signing key; ordered, retried, bounded |
| http server | `/healthz` and `/metrics`, only when a port is configured |
| signal watcher | shutdown flag and wake channel |

Tasks communicate over tokio channels. The Postgres pool is shared; every
store call is short and never held across an await on the network.

### Module map

`src/`, one purpose per file, following Templar's layout:

- `main.rs` — tracing setup, argument parsing, run-mode dispatch, exit code.
- `liquidator.rs` — lib root. Crate docs, `LiquidatorError` with a phase per
  stage (`Ledger`, `Store`, `Chain`, `Plan`, `Execution`), `FillOutcome`,
  `CreationOutcome`, `NotificationKind`.
- `config.rs` — clap `Args` for global knobs, the pools TOML schema,
  translation into `ServiceConfig`, startup validation, redacted `Debug`.
- `service.rs` — lifecycle: migrations, validation, seeding, task wiring,
  shutdown, `check-config` mode.
- `ledger.rs` — the clock. Polls the latest ledger, streams pool events by
  cursor, persists the cursor, emits `LedgerTick { sequence, close_time }` and
  decoded `PoolEvent`s.
- `chain/rpc.rs` — Soroban JSON-RPC client: health, latest ledger, ledger
  entries, simulate, send, get transaction, fee stats, events, account.
- `chain/pool.rs` — pool reads (instance config, reserve list, reserves,
  positions, auction entry, oracle prices and decimals, token balances) and
  operation builders (`submit`, `new_auction`, `bad_debt`).
- `chain/xdr/` — ScVal codecs, one file per concern: `encode` (values,
  operations, simulation envelopes, `Request`), `keys` (storage keys,
  durability included), `decode` (`PoolConfig`, `ReserveConfig`,
  `ReserveData`, `Positions`, `AuctionData`, view-call returns), `events`
  (the pool event catalogue).
- `chain/tx.rs` — build, simulate, restore footprint, assemble, fee, sign,
  send, poll, classify.
- `math/` — pure: `fixed` (checked fixed-point helpers), `reserve`
  (interest accrual to a timestamp, b-token and d-token conversions),
  `position` (effective and raw values, health factor), `auction`
  (Dutch-auction scaling).
- `store.rs` — Postgres schema, embedded migrations, typed queries.
- `tracker.rs` — event application, user refresh, recheck flags, seed and
  replay on start.
- `auctioneer.rs` — liquidatable and bad-debt tests, auction percent and asset
  lists, creation submissions.
- `filler.rs` — auction valuation, fill ledger, health-bounded request
  builder, `FillPlan`.
- `inventory.rs` — filler wallet balances and pool positions, `Reservation`.
- `executor.rs` — simulate, submit, settle, record, schedule unwind.
- `unwind.rs` — post-fill repay-and-withdraw request builder.
- `queue.rs` — per-key ordered submission queue.
- `swap.rs` — the `SwapProvider` trait only; no venue ships in v1.
- `notifier.rs`, `metrics.rs`, `http.rs`, `format.rs` — as in Templar, with
  `http.rs` serving `/healthz`, `/livez`, and `/metrics`.

### Data flow per ledger

1. The poller sees a new ledger, fetches that ledger's pool events, and sends
   the events followed by the tick to the tracker.
2. The tracker applies the events to the store (users refreshed from chain,
   auctions opened, reduced, or closed), flags users touched by the events,
   then forwards the tick to the auctioneer and the filler. Ordering matters:
   evaluation never runs on a ledger whose events have not been applied.
3. The auctioneer, when its cadence fires, rechecks the flagged or scheduled
   users, builds creation and bad-debt submissions, and enqueues them on the
   creator queue.
4. The filler evaluates every open auction against the next ledger, plans a
   fill when its ledger has arrived, reserves inventory, and hands the plan to
   the executor, which submits on the filler queue.
5. A confirmed fill records the result, consumes the reservation, refreshes
   inventory, and enqueues unwind steps behind any pending fills.

## 3. Chain access and mathematics

### Crates

The official `stellar-rpc-client` and `stellar-xdr` crates for RPC and XDR,
`ed25519-dalek` and `stellar-strkey` for keys. Versions are pinned to the
protocol version the target RPC speaks and are resolved in the implementation
plan; the three-way Rust pin from the scaffold stays. No `soroban-sdk` in the
bot: the handful of pool types it touches are encoded and decoded by hand in
`chain/xdr.rs` against fixtures captured from a live pool, which keeps the
dependency tree small and every ScVal shape visible in one file.

### Reads

Everything about a pool comes from batched `getLedgerEntries`:

| Entry | Key | Durability |
|---|---|---|
| pool instance | contract instance | persistent (instance storage: `Config`, `Backstop`, `Admin`) |
| reserve list | `ResList` | persistent |
| reserve config and data | `ResConfig(asset)`, `ResData(asset)` | persistent |
| user positions | `Positions(user)` | persistent |
| auction | `Auction(AuctionKey { user, auct_type })` | temporary |

Oracle prices and decimals (`lastprice`, `decimals`), token balances
(`balance`), and the filler's account sequence come from `simulateTransaction`
and the account entry, because oracle and token implementations vary and
simulation is the portable path. Fee percentiles come from `getFeeStats`. The
RPC's retained event window comes from `getHealth` (`oldestLedger`,
`ledgerRetentionWindow`). Every read carries the ledger it was taken at, and a
snapshot used for a decision is never older than the tick that triggered it.

### Projecting reserve state

Ledger entries hold reserve rates as of their last update, but the contract
accrues interest to the current ledger inside every call. `math.rs` ports the
contract's accrual exactly: utilisation, the four-parameter rate curve
(`r_base`, `r_one`, `r_two`, `r_three` with the 95% knee), the
reactivity-driven interest-rate modifier with its floor and ceiling, the
12-decimal rate update, and the backstop credit. Effective and raw collateral
and liabilities use the contract's rounding directions (collateral floors,
liabilities ceil). Health factor is `collateral_base / liability_base` in the
oracle's decimal scale, and thresholds expressed with 7 decimals are scaled
the way `PositionData::is_hf_under` and `is_hf_over` scale them.

Unit tests pin these functions against values obtained by simulating
`get_reserve` and `get_positions` on the same ledger the fixtures were
captured at, so drift from a contract upgrade fails a test rather than a fill.

### Writes

One path in `chain/tx.rs`:

1. Fetch the signing account's sequence.
2. Build a transaction with a single invoke-host-function operation, a
   five-minute upper time bound, and a placeholder fee.
3. Simulate. If the response carries a restore preamble, build, sign, and
   send a restore-footprint transaction first, wait for it, then simulate
   again.
4. Assemble resources and authorisation from the simulation.
5. Set the inclusion fee: the fee-stats p70 percentile floored at `BASE_FEE`,
   or the p90 percentile floored at `HIGH_FEE` when the caller marks the
   transaction high priority.
6. Sign, send. `TRY_AGAIN_LATER` is retried once after a short pause.
7. Poll `getTransaction` for up to `TX_POLL_LEDGERS` ledgers.
8. Classify: succeeded (with ledger, hash, return value), failed with a decoded
   pool error code (the 1200-series), or unknown after timeout.

The filler is `from`, `spender`, and `to` of every `submit`, so source-account
authorisation covers it and no additional signatures are needed. Each queue
enforces one in-flight transaction per key, which is what Stellar sequence
numbers require.

### Math discipline

All amounts are `i128`. Multiply-divide helpers (`mul_floor`, `mul_ceil`,
`div_floor`, `div_ceil`) are checked and return an error on overflow or a zero
divisor; nothing in `math.rs` panics. Auction scaling reproduces the
contract's per-block modifiers (0.5% per block, lot ramps 0 to 100% over
blocks 0 to 200, bid ramps 100% to 0 over blocks 200 to 400) with bid
rounding up and lot rounding down, and returns both the scaled and the
remaining auction as the contract does.

## 4. Store, tracking, and the auctioneer

### Store

Postgres through `sqlx` with compile-time checked queries, migrations
embedded in the binary and applied at startup under an advisory lock (the
production deployment runs one instance, but the lock makes a rolling
deployment safe). Amounts and values are stored as decimal text because
`i128` exceeds `bigint`; health factors and ledgers fit `bigint`.

| Table | Columns | Purpose |
|---|---|---|
| `cursors` | `name` pk, `ledger`, `paging_token` | per-task progress |
| `users` | `pool`, `account`, `health_factor` (7-dec, normalised as `hf × 10^7 / oracle_scalar`, so pools with different oracle decimals order and compare alike), `collateral` jsonb, `liabilities` jsonb, `updated_ledger`; pk `(pool, account)`; index `(pool, health_factor)` | tracked borrowers with liabilities |
| `auctions` | `pool`, `account`, `auction_type`, `start_ledger`, `fill_ledger`, `percent`, `bid` jsonb, `lot` jsonb, `updated_ledger`; pk `(pool, account, auction_type)` | open auctions and the filler's current plan |
| `fills` | `id`, `tx_hash` nullable unique, `pool`, `account`, `auction_type`, `fill_ledger`, `percent`, `bid` jsonb, `lot` jsonb, `bid_value`, `lot_value`, `est_profit`, `dry_run`, `created_at` | audit of fills, simulated ones included |
| `creations` | `id`, `tx_hash` nullable, `kind` (`auction` or `bad_debt`), `pool`, `account`, `percent`, `bid` jsonb, `lot` jsonb, `ledger`, `dry_run`, `created_at` | audit of auctioneer submissions |

Fills and creations are also emitted as structured log events, so the audit
trail survives a database loss in the log system.

### Discovery and refresh

Any pool event that names a user (supply, withdraw, supply and withdraw
collateral, borrow, repay, flash loan, new auction, fill auction for both the
liquidated user and the filler, delete auction, bad debt) refreshes that user
from chain: positions entry plus reserves accrued to now, health factor
recomputed, row kept only while liabilities exist. New, fill and delete
auction events also open, update or close the matching `auctions` row.

On first start, and whenever the stored cursor is older than the RPC's
retained window, the tracker seeds the user set:

1. From the **seed source**, an HTTP endpoint with the shape of the public
   Blend analytics API: `GET {SEED_URL}/v1/analytics/state/positions` with
   `healthFactorMax={SEED_HF_MAX}`, `poolId={pool}`, `limit=500`, following
   `nextCursor` until absent and reading `positions[].accountId`. The default
   `SEED_URL` is `https://api.blend.templarfi.org`, which is keyless and free,
   so a third-party operator gets the same coverage. Pages are fetched with a
   small pause to stay under the anonymous rate limit. A failed seed is a
   warning and a notification, not a startup failure, and it is retried on
   the next full scan. It does not gate submissions: every submission acts
   only on a user the bot has already verified from chain, so an incomplete
   seed costs coverage, never correctness. `SEED_HF_MAX` bounds that
   coverage; a position above it needs a price move of that magnitude before
   it matters, and operators who want more raise it.
2. From an optional static `SEED_FILE` mapping pool addresses to account
   lists.
3. By replaying retained events from `oldestLedger` to chain head.

Seeded accounts are refreshed from chain before they are trusted. Users not
updated for `USER_REFRESH_LEDGERS` are refreshed in small batches every tick
so a long-idle borrower's interest accrual is not missed.

### Cadences

Expressed in ledgers and offset by a per-instance random phase so several bots
on one pool do not fire on the same ledger:

| Cadence | Default | Action |
|---|---|---|
| oracle scan | 60 | load prices; on a move of at least `PRICE_DELTA_BPS` since the last significant price, recheck users holding that asset as a liability (price up) or as collateral (price down); refresh the reference price after a day without a significant move |
| full scan | 1200 | recheck every user stored below `SCAN_HF_THRESHOLD` |
| refresh | every tick | refresh up to `REFRESH_BATCH` users whose row is older than `USER_REFRESH_LEDGERS` |

### Auctioneer decision

For each rechecked user, with reserves accrued and prices from the same tick:

- **Liquidatable** when effective liabilities and effective collateral are
  both positive and `collateral_base / liability_base < LIQ_HF_THRESHOLD`
  (default 0.998). The margin under the contract's strict
  `liability_base > collateral_base` test absorbs rounding and the accrual
  between planning and execution.
- **Bad debt** when liabilities are positive and collateral is zero: submit
  `bad_debt(user)`.
- If an auction entry already exists for the user, skip.
- Never for the filler or auctioneer addresses.

**Percent and asset lists**, ported from Script3 to fixed point:

1. `excess = effective_liabilities × TARGET_HF − effective_collateral`, with
   `TARGET_HF` defaulting to 1.06. If `excess ≤ 0`, skip.
2. Sort the user's collateral and liability positions by effective value.
   Start with the largest of each.
3. For the selected sets, with average collateral factor
   `cf = eff_coll / raw_coll` and average inverse liability factor
   `lf = eff_liab / raw_liab`, estimate the auction incentive as
   `1 + (1 − cf / lf) / 2`, the borrow-limit recovered per raw liability as
   `lf × TARGET_HF − incentive × cf`, and
   `percent = round(excess / (recovered × raw_liab) × 100)`. If the collateral
   the auction would withdraw, `percent × raw_liab × incentive`, exceeds the
   selected raw collateral, the percent is 0.
4. A percent above 100 adds the next largest liability, or the next largest
   collateral when liabilities are exhausted; a percent of 0 adds the next
   largest collateral. When both are exhausted, the answer is 100% of every
   position.
5. The combined asset count may not exceed the pool's `max_positions`; when
   it would, the largest assets are kept and the rest dropped.

The creation is then **simulated**. `InvalidLiqTooSmall` raises the percent by
one point and `InvalidLiqTooLarge` lowers it, up to five times, before anything
is signed. Any other contract error skips the user until the next recheck and
is logged with the decoded code. Dry-run records the creation it would have
made. Live creations go through the creator queue; a dropped creation is a
high-severity notification.

## 5. Filler, inventory, executor, unwind

### Evaluation loop

Every tick the filler walks the open liquidation auctions in the store whose
pool is configured and whose bid and lot assets are all within that pool's
`supported_bid` and `supported_lot` (a `*` entry matches any reserve). An
auction is re-planned when it has no fill ledger yet, every
`REPLAN_LEDGERS` (default 10), and every ledger within `REPLAN_NEAR_LEDGERS`
(default 5) of its target. Planning always re-reads the on-chain auction entry
first; a missing entry closes the row. Fill events from competitors reduce the
stored base auction by the filled amounts, and a fill event at 100% closes it.

### Valuation

Lot bTokens and bid dTokens convert to underlying through the accrued rates,
then to base value at oracle prices, both raw and effective. The profit margin
`p` is the `profit_bps` of the first `profits` entry whose `supported_bid` and
`supported_lot` cover every auction asset, else the pool's `default_profit_bps`.

### Fill ledger

The fill delay `d` is the smallest value in `0..=400` such that
`scaled_lot(d) ≥ scaled_bid(d) × (1 + p)`, where the scaling is the
contract's. It is solved in closed form with ceiling division: on the lot ramp
`d = ceil(200 × bid × (1 + p) / lot)` when the full lot covers the bid plus
margin, otherwise on the bid ramp
`d = 400 − floor(200 × lot / (bid × (1 + p)))`. Tests verify the closed form
by evaluating the contract's scaling at `d` and `d − 1`. The result is capped
at 350 under `force_fill`, and moved to the next ledger when it has already
passed.

### Health-bounded plan

Inputs: the filler's accrued pool positions, wallet balances with
`XLM_FEE_RESERVE` withheld from XLM, and the auction scaled to the candidate
ledger and percent. A loop of at most `PLAN_ITERATIONS` (default 5) builds the
`submit` requests:

1. `Repay` for each bid asset the filler holds in the wallet, converting the
   scaled dTokens to underlying with a small dust allowance, capped at the
   balance.
2. `WithdrawCollateral` of the maximum for each lot asset whose collateral
   factor is zero, so it does not consume a position slot for nothing.
3. Project the post-fill health factor: filler effective collateral plus the
   lot's effective value, over filler effective liabilities plus the bid's
   effective value minus what is repaid. Require it to be at least
   `min_health_factor × HF_SAFETY_MULTIPLIER` (default 1.1) and the projected
   collateral base to be at least the pool's `min_collateral`.
4. If short: `SupplyCollateral` of the primary asset from the wallet for the
   shortfall, only when the pool status permits supplying; then lower the
   percent to what the headroom allows, never below 1; then delay the fill past
   ledger 200 to the ledger at which received collateral outweighs taken debt.

The request list starts with the fill request itself
(`FillUserLiquidationAuction`, address the liquidated user, amount the
percent). The output is a `FillPlan`: pool, user, fill ledger, percent,
requests, valued bid and lot, estimated profit, and the wallet amounts to
reserve.

### Inventory and settlement

`inventory.rs` keeps the filler's wallet balances per asset and pool positions
per pool, refreshed after every confirmed transaction and at most every
`INVENTORY_REFRESH_SECS` otherwise. A plan takes a must-use `Reservation` for
the wallet amounts it will spend; the token is consumed or released by value
exactly once, carries the manager it was issued by, and saturates rather than
errors so the ledger only protects callers that honour it.
`Settlement::Live(Reservation)` or `Settlement::DryRun` travels with the plan;
a dry-run executor handed a live token releases it and fails, and a live
executor handed `DryRun` fails, so the two cannot be mixed up.

### Executor

1. Simulate the exact `submit`. The contract's own health check inside the
   simulation is authoritative. `InvalidHf` or `MinCollateralNotMet` triggers
   one re-plan at a lower percent; any other contract error skips the auction
   until the next tick, with the decoded code logged and counted.
2. Choose the fee tier: high when `est_profit ≥ HIGH_FEE_PROFIT_THRESHOLD`.
3. Sign and send on the filler queue; await the classified result.
4. On success: record the fill, consume the reservation, refresh inventory,
   enqueue unwind, notify.
5. On a send failure or timeout: the queue retries with backoff. On a
   contract failure: release the reservation, record the outcome, and let the
   next tick re-evaluate with fresh chain state.

Dry-run stops after logging and recording the plan; it takes no reservation
and sends nothing.

### Unwind

After a live fill, an `Unwind { pool }` submission is queued behind pending
fills and repeated until a pass produces no requests:

1. `Repay` each liability asset with the wallet balance (the contract refunds
   any excess), noting which liabilities remain.
2. If no liabilities remain, `WithdrawCollateral` everything except the
   primary asset, then the primary asset down to `min_primary_collateral`.
3. Otherwise withdraw collateral while the projected health factor stays at
   or above `min_health_factor`, taking assets that are also liabilities
   first, then the smallest positions, the primary asset last and never below
   its floor, and stopping when the health factor is within 0.5% of the
   minimum or a withdrawal would be under 1% of the primary floor.

Leftover liabilities after an idle pass raise one deduplicated high-severity
notification per pool.

## 6. Configuration

### Global knobs

Environment variable or flag, flag winning, with clap's `env` derive.

| Variable | Type | Default | Notes |
|---|---|---|---|
| `NETWORK_PASSPHRASE` | string | required | or `NETWORK=mainnet\|testnet` alias |
| `RPC_URL` | URL | required | |
| `RPC_API_KEY_HEADER` / `RPC_API_KEY` | string | unset | header name and value for keyed RPC providers; the key is a secret |
| `DATABASE_URL` | URL | required | secret: may carry a password |
| `POOLS_FILE` / `POOLS_TOML` | path / string | one required | per-pool configuration, section below |
| `FILLER_SECRET_KEY` | S-key | required for live | env only; secret |
| `AUCTIONEER_SECRET_KEY` | S-key | unset | env only; secret; defaults to the filler key |
| `DRY_RUN` | `true`/`false` only | `true` | strict parser, bare flag means true |
| `RUN_MODE` | `loop`/`check-config` | `loop` | |
| `LOG_FORMAT` | `text`/`json` | `text` | |
| `PORT` / `HTTP_PORT` | u16 | unset | `PORT` honoured for Cloud Run; unset disables the server |
| `HTTP_BIND_ADDR` | IP | `127.0.0.1` | Cloud Run cannot route to loopback: that deployment sets `0.0.0.0` |
| `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` | string | unset | both or neither; the token is a secret |
| `POLL_INTERVAL_MS` | u64 | 1000 | ledger polling |
| `ORACLE_SCAN_LEDGERS` | u32 | 60 | |
| `FULL_SCAN_LEDGERS` | u32 | 1200 | |
| `USER_REFRESH_LEDGERS` | u32 | 241920 (14 days) | |
| `REFRESH_BATCH` | u32 | 20 | |
| `PRICE_DELTA_BPS` | u32 | 250 | |
| `SCAN_HF_THRESHOLD` | decimal | 1.2 | |
| `LIQ_HF_THRESHOLD` | decimal | 0.998 | |
| `TARGET_HF` | decimal | 1.06 | |
| `HF_SAFETY_MULTIPLIER` | decimal | 1.1 | |
| `REPLAN_LEDGERS` / `REPLAN_NEAR_LEDGERS` | u32 | 10 / 5 | |
| `PLAN_ITERATIONS` | u32 | 5 | |
| `XLM_FEE_RESERVE` | decimal XLM | 50 | |
| `BASE_FEE` / `HIGH_FEE` | stroops | 5000 / 10000 | inclusion-fee floors |
| `HIGH_FEE_PROFIT_THRESHOLD` | decimal, oracle units | 10 | |
| `TX_POLL_LEDGERS` | u32 | 3 | |
| `INVENTORY_REFRESH_SECS` | u64 | 30 | |
| `SEED_URL` | URL | `https://api.blend.templarfi.org` | empty disables |
| `SEED_HF_MAX` | decimal | 10 | |
| `SEED_FILE` | path | unset | |
| `STARTUP_DELAY_LEDGERS` | u32 | 0 | no submissions before this many ticks |
| `HEALTH_MAX_LAG_LEDGERS` | u32 | 10 | readiness bound |
| `FAILURE_NOTIFICATION_COOLDOWN_HOURS` | u64 | 24 | |

Decimal-typed knobs are parsed into 7-decimal fixed point at startup; a value
that does not fit is a startup error.

### Pools file

TOML, one `[[pools]]` table per pool:

```toml
[[pools]]
address = "C..."                  # pool contract
primary_asset = "C..."            # asset kept as collateral in this pool
min_primary_collateral = "1000000000000"  # underlying units, decimal string
min_health_factor = 1.5
default_profit_bps = 1000
force_fill = false
supported_bid = ["C...", "C..."]  # or ["*"]
supported_lot = ["*"]

[[pools.profits]]                 # optional, ordered, first match wins
profit_bps = 500
supported_bid = ["C..."]
supported_lot = ["*"]
```

### Startup validation

Keys parse and differ when both are given. Every pool loads from chain; its
backstop is the same across pools; the primary asset is a reserve with a
positive collateral factor and is enabled; every explicitly listed supported
asset is a reserve. The filler account exists and holds XLM above the fee
reserve. In live mode the filler holds at least `min_primary_collateral` in
every pool or a warning names the shortfall. `check-config` runs exactly this
validation, prints the resolved (redacted) configuration, and exits 0 or 2.

## 7. Observability

**Logs** are structured tracing events, JSON when `LOG_FORMAT=json`, with the
pool, user, auction type, ledger, and decoded error code as fields. Fills and
creations are logged as dedicated events with every column the store records.

**Metrics** are Prometheus text at `/metrics`, prefixed `blend_liquidator_`:
ledger head and processed ledger gauges (lag is their difference), events
processed, users tracked and auctions open per pool, creations and fills
attempted, succeeded, and failed, skips by reason (`unsupported_assets`,
`unfunded`, `unprofitable`, `health`, `contract_error`), estimated profit
total in oracle units, reserved inventory per asset, unwind passes, last
successful scan timestamp, seed accounts loaded. Label sets are closed enums.

**`/healthz`** is readiness: 200 when the processed ledger is within
`HEALTH_MAX_LAG_LEDGERS` of chain head and the store answers a ping, else 503
with the reason. It is what an alert watches. **`/livez`** is liveness: 200
while the poller loop has recorded a heartbeat within five poll intervals,
regardless of whether the RPC is answering, else 503. It is what an
orchestrator's restart probe watches. Keeping them apart means an RPC outage
makes the bot not-ready without restart-looping it, while a wedged loop is
restarted; this is the `/livez` Templar's backlog asked for.

**Notifications** go to Telegram behind the `NotificationChannel` trait with
Templar's shell: deduplication by `(pool, account, kind)` with a cooldown, a
bounded in-flight semaphore, and `drain()` on every exit path. Kinds: auction
created, bad debt reported, fill confirmed, fill failed on chain, submission
dropped, unwind leftovers, poller stalled, persistent RPC failure, event gap
after a cursor fell out of the retained window, unfunded fill skipped.

## 8. Error handling

- **Poller.** RPC errors back off from one to thirty seconds and never advance
  the cursor past what was applied. A cursor older than `oldestLedger`
  notifies a gap, reseeds users, and restarts from the window's edge.
- **Queues.** A send that fails outright retries with exponential backoff up
  to a per-submission retry budget (creations 3, fills 10, unwinds 2); a
  sequence error refetches the sequence and retries once; a decoded contract
  error is returned to the caller without retry; exhausted retries drop the
  submission and notify. A timeout is `unknown`, never a failure: the
  transaction may still land. Every transaction the bot signs carries a
  ledger bound, and the queue records the hash, sequence and bound before
  sending; on a timeout it polls `getTransaction` for that hash until the
  outcome is terminal, or until the chain has passed the bound with the
  transaction still not found, which proves it can never be included. Only
  then does it retry, with a fresh sequence number. Inventory reservations
  stay held until the outcome is known, so a retry never sizes against
  inventory an unknown transaction may have spent.
- **Tracker.** A failed user refresh logs and leaves the row untouched; the
  next event or refresh pass retries.
- **Executor.** Reservation settlement happens on every non-panicking path,
  including early returns and task cancellation during shutdown, through a
  drop guard that releases an unsettled token and logs a warning. A panic
  aborts the process under the release profile, and the in-memory ledger dies
  with it; the next start rebuilds inventory from chain.
- **Notifier and metrics** failures never affect trading.
- **Store** errors are `Store`-phase errors; a store outage makes `/healthz`
  fail and pauses submissions rather than trading on stale state.

## 9. Testing

**Unit tests**, inline in every module:

- `math.rs`: accrual, conversions, effective values, health factor, auction
  scaling, fill delay, auction percent, plan and unwind builders, each pinned
  against fixtures captured from mainnet ledger entries with expected values
  taken from simulating `get_reserve`, `get_positions`, and `get_auction` on
  the same ledger. Fixtures live in `tests/fixtures/` and are refreshed by a
  documented script that uses plain JSON-RPC over `curl`.
- `chain/xdr.rs`: round trips on captured XDR for every entry and event type.
- `config.rs`: strict dry-run parsing, redaction, pools file validation,
  decimal-to-fixed-point conversion bounds.
- `queue.rs`, `inventory.rs`, `executor.rs`: ordering, retry budgets,
  reservation typestates, the four settlement guards.
- `chain/tx.rs`: a scripted localhost JSON-RPC server driving the real client
  through restore, `TRY_AGAIN_LATER`, timeout, and decoded-error paths.

**Integration**, `#[ignore]`, run nightly in CI: start a Stellar quickstart
container on a local network and a Postgres container, deploy the Blend v2
pool, backstop, and a mock SEP-40 oracle built from `blend-contracts-v2` at a
pinned revision, create a pool with two reserves, fund a borrower, crash the
collateral price, run the bot live against it, and assert that it created the
auction, filled it at the expected ledger, unwound to the wallet, and recorded
both in the store. The dev container gains the `stellar` CLI and the
cgroup-aware build-job cap at this phase, as the scaffold spec anticipated.

**Testnet soak**: a documented dry-run against Blend testnet pools, then a
live run with small capital, is the last step before mainnet configuration.

## 10. Deployment contract

What this repository guarantees to `blend-liquidator-infra` and to any other
operator:

- A multi-stage, digest-pinned, non-root image published to GHCR on tags,
  tagged `X.Y.Z` (the `v` is stripped by the metadata action, as with Templar).
- The process reads only environment variables and the pools file or inline
  string; it writes no filesystem state.
- `PORT` is honoured when set; `/healthz` (readiness), `/livez` (liveness),
  and `/metrics` are served on it. A restart probe must target `/livez`, never
  `/healthz`.
- Migrations run at startup under an advisory lock; the database user needs
  DDL on its own database.
- Secret-shaped variables, to be listed in the infra validation:
  `FILLER_SECRET_KEY`, `AUCTIONEER_SECRET_KEY`, `DATABASE_URL`,
  `RPC_API_KEY`, `TELEGRAM_BOT_TOKEN`.
- `RUN_MODE=check-config` validates configuration against chain and the
  database and exits, for use as a deploy smoke test before dry-run is turned
  off.
- Exit codes: 0 on graceful shutdown or a passing check, 2 on configuration
  error, 1 on any other fatal error, 130 on a second signal.
- Logs are JSON lines on stdout when `LOG_FORMAT=json`.

The infra repository's own specification covers the GCP project, the Cloud
Run service (one always-on instance, `max_instances = 1`, CPU always
allocated), Cloud SQL, Secret Manager, Workload Identity, deployer IAM, and
alerting. The service sets `HTTP_BIND_ADDR=0.0.0.0` and honours the injected
`PORT`, since Cloud Run cannot route to a loopback listener.

A rolling revision briefly runs two instances, and the signer account is the
lease that serialises them: Stellar accepts one transaction per account
sequence number, so when both submit, one lands and the other fails with a
bad sequence, refetches the sequence and retries once. That retry reaches the
contract after the winner, where a second `new_auction` for the same user
fails because the auction already exists, a second fill of a filled auction
fails because there is nothing left to fill, and a repeated unwind moves
nothing — decoded contract errors the queues return without retry. Each
instance's inventory reservations are process-local and rebuilt from chain on
start, so the one exposure is a fill the loser sized against inventory the
winner has since spent, which the contract rejects on the token transfer.
Rolling overlap therefore costs failed transaction fees, never a duplicate
position, and `STARTUP_DELAY_LEDGERS` must exceed the old revision's shutdown
drain so the window is normally empty.

## 11. Seams and extension points

- `notifier::NotificationChannel` — Slack, Discord, or a webhook channel.
- `swap::SwapProvider` — a Stellar DEX or Soroswap venue for selling received
  collateral; the trait ships, no implementation does.
- `PriceSource` in `filler.rs` — an external valuation for lot or bid assets;
  the oracle source is the only implementation.
- `SeedSource` in `tracker.rs` — the analytics API and the static file are
  the two implementations.
- Auction types 1 and 2 are reachable by extending `auctioneer.rs` and
  `filler.rs` behind the existing `auction_type` field; the store already
  keys on it.
- A flash-loan funding mode is a second executor strategy plus a receiver
  contract; the `FillPlan` and `Settlement` types are venue-neutral.

## 12. Delivery

**Dependencies**: `tokio`, `clap`, `tracing`, `tracing-subscriber`,
`thiserror`, `serde`, `toml`, `sqlx` (postgres, runtime-tokio, tls),
`reqwest`, `stellar-rpc-client`, `stellar-xdr`, `ed25519-dalek`,
`stellar-strkey`, `axum`, `rand`. Exact versions are chosen in the plan and
recorded in `Cargo.lock`; `cargo deny` gates licences and advisories.

**Phases**, each one pull request that leaves CI green and is demonstrable in
dry-run against testnet:

1. `math.rs`, `chain/xdr.rs`, fixtures and the capture script.
2. `chain/rpc.rs`, `chain/pool.rs`, `chain/tx.rs`, the scripted RPC server.
3. `store.rs`, `ledger.rs`, `tracker.rs`, seeding and replay; the bot can
   follow pools and print tracked users.
4. `auctioneer.rs` and `queue.rs`; auctions are created in dry-run.
5. `filler.rs`, `inventory.rs`, `executor.rs`; fills are planned and, live,
   submitted.
6. `unwind.rs`, `notifier.rs`, `metrics.rs`, `http.rs`, `check-config`.
7. The sandbox integration tier and the dev-container additions.
8. Documentation (`README`, `docs/configuration.md`, `docs/deploy.md`,
   `docs/architecture.md`), `CHANGELOG`, the deployment contract, first
   release tag.

**Working rules for the implementer**: tests before code for every pure
function; no `unwrap` outside tests; every doc comment states a constraint;
each phase ends with `make check` green and a dry-run log excerpt in the pull
request.
