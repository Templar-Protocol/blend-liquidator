# blend-liquidator

## What this is

A liquidation bot for [Blend Protocol](https://blend.capital) lending pools on
Stellar. It is intended to repay the debt of underwater positions and receive
their collateral at a discount.

**The source of truth for pool behaviour is
`Templar-Protocol/blend-contracts-v2`** — the ADR-0008 / ADR-0011 security
fork — not `blend-capital/blend-contracts-v2`, which Phases 1 through 7 were
built against. Most of the fork is byte-identical to stock, this crate's whole
arithmetic port included, so the change is narrower than it sounds; what it
does change, and what the bot still has to do about it, is
`docs/specs/2026-09-20-adr-0008-fork-semantics.md`. Read that before touching
anything contract-derived. The fork is **not deployed anywhere yet**: its pull
request is a draft whose first line reads "SECURITY GATE OPEN: not ready for
publication, release, deployment, activation, or handling funds", and it
publishes no release, which is why the sandbox tier below still pins stock
wasm.

**Status: Phase 9 complete.** Phase 1 landed the pure fixed-point math
(`math`) and the ScVal/ledger-entry codecs (`chain::xdr`); Phase 2 landed
the chain layer (`chain::rpc`, `chain::pool`, `chain::signer`, `chain::tx`);
Phase 3 landed the Postgres store, a per-pool ledger poller and a tracker
(`store`, `ledger`, `tracker`, `service`), so the binary validates its
configuration, seeds its tracked-user set from the analytics API or a
static file, and follows every configured pool — applying events and
refreshing borrowers' health factors from chain — until it is shut down.
Phase 4 landed the auctioneer (`auctioneer`, `queue`, `math::liquidation`):
once a tick, it decides which tracked borrowers are liquidatable or owe
bad debt, builds the auction the contract should accept, lets the contract
judge the percent through simulation, records every creation it decides to
make — dry-run or not — and, only when a signing key is configured and
`DRY_RUN=false`, submits it through a per-key queue. Phase 5 landed the
filler (`filler`, `executor`, `inventory`, `math::fill`): once a tick, it
plans a fill for every open liquidation auction whose assets its pool
configuration supports, holds its own position at or above
`min_health_factor × HF_SAFETY_MULTIPLIER` while taking one over, records
every fill it executes — dry-run or not — and, only with `DRY_RUN=false`
*and* `FILLER_SECRET_KEY`, submits it on the filler key's queue. Phase 6a
landed unwind (`math::unwind`, `notifier`, and the filler's unwind pass in
`filler`/`executor`): after a fill lands, and once at startup, the filler
repays the debt it holds from its wallet and withdraws collateral to the
wallet — everything but the primary, and the primary down to
`min_primary_collateral` — keeping its own health factor at or above the
pool's `min_health_factor`; debt the wallet cannot repay notifies once per
pool through the `Notifier`. Phase 6b landed the operational surface
(`metrics`, `http`, `notifier::telegram`, and the rest of `notifier`,
`ledger` and `service`): dependency-free Prometheus counters and gauges
rendered at `/metrics`, an axum server for `/healthz` (readiness) and
`/livez` (liveness) that runs only when `PORT` or `HTTP_PORT` is set, and
a Telegram `NotificationChannel` behind the trait `notifier` already had.
`Notifier::notify` is fire-and-forget now — it takes the dedup entry
synchronously and spawns the send behind a bounded semaphore
(`NOTIFY_IN_FLIGHT`), answering `Delivery::Queued`/`Deduplicated`/`Dropped`
rather than waiting on the channel — and `Notifier::drain` gives whatever
is still in flight a bounded `DRAIN_BUDGET` on every exit but the second
shutdown signal and a release build's panic abort. The poller records a heartbeat every iteration and the
chain head every pass, and reports `NotificationKind::RpcFailing` after
`RPC_FAILING_AFTER` consecutive failures; a watchdog task the run spawns
beside the pollers reports a pool whose heartbeat has gone past
`PollerConfig::liveness_deadline` as `PollerStalled`. `Service::run` spawns
seven kinds of task — one `LedgerPoller` per pool, one tracker, one
auctioneer, one filler, one watchdog, one HTTP server when a port is
configured, and one submission-queue worker per distinct signing key when
armed — and every exit but the second shutdown signal and a release
build's panic abort drains the notifier before it returns. Phase 7 landed the sandbox integration tier
(`scripts/sandbox/`, `tests/liquidation_sandbox.rs`,
`.github/workflows/sandbox.yml`) and the dev-container additions it needs
(`scripts/cargo-jobs.sh`, the `stellar` CLI): a throwaway Stellar network
in Docker, Blend v2 deployed on it from pinned wasm with one borrower a
price move from liquidation, and the real binary run against it **armed**
— the only place in this repository, besides the testnet tier Phase 9
added, that anything signs and sends a transaction — asserting on chain
and in the store that it created the
auction, filled it, unwound the position it took, and counted all three.
Phase 8 reconciled the bot with the ADR-0008 fork
(`docs/specs/2026-09-20-adr-0008-fork-semantics.md` §4, A through J,
landed on `phase-8/fork-reconciliation`): it decodes the fork's three new
events — `debt_setoff`, `collateral_orphaned`, `orphan_settled`
(`chain::xdr::events`) — and raises `NotificationKind::StockWasmDetected`
the moment a `bad_debt` event is seen, since the fork's own contract can
never emit one; decides bad debt on raw collateral rather than
c-factor-weighted, and only ever builds a user-liquidation auction, since
the fork panics on any other type (`new_auction_op` now takes no
auction-type parameter); ports the fork's set-off and default path as
`math::setoff`, and `plan_fill` projects the `b_rate` haircut a full fill
can cause on the reserves it values the filler's own health against,
rather than trusting the pre-fill snapshot; makes which ledger a fill
aims at a per-pool choice (`PoolConfig::fill_objective`,
`math::fill::FillObjective`: `FreeFill` at the ramp's end by default, or
`EarliestProfitable`), with no invented 400-ledger cutoff — the contract
never had one; sizes a fill's own supply under the reserve's `supply_cap`
by exact search rather than skipping only once the chain refuses it
(`FillSkip`/`SkipLabel::SupplyCapped`); and never treats the pool's own
contract address — a `Positions` holder on the fork, since confiscated
collateral lands there as ordinary supply — as a borrower to liquidate.
Phase 9 landed the documentation set — `docs/configuration.md`,
`docs/deploy.md`, `docs/deployment-contract.md` and `docs/architecture.md`
— the deployment contract, and a `check-release.sh`-green `0.1.0`
changelog section. The `v0.1.0` tag itself is the maintainer's to push:
pushing it is what publishes the GHCR image and cuts the GitHub Release,
not anything landed on this branch. Phase 9 also ran the testnet soak the
design spec's §9 ends with (`scripts/testnet/`, `docs/testnet-soak.md`),
against stock wasm on public Stellar testnet — the fork still is not
deployed anywhere, so the soak targets what actually is: Blend's own
testnet pool for stage 1 (observe) and this repository's own throwaway
deployment for stage 2 (armed). Stage 1 has run the bot in dry run
against a pool this repository does not control, seeded with the 15
accounts a scan of the pool's own recent events found and given one
position of the soak's own to follow. It proved the bot validates its
configuration and tracks and values whatever borrowers it can see —
bounded by the RPC's own event-retention window, not an index of every
position, since testnet has no analytics API — without ever submitting a
transaction. Stage 2 stood up its own pool, crashed the
oracle's price, and ran the bot armed against public infrastructure end
to end — the only place outside the sandbox tier this bot ever signs and
sends there: a recorded run created the borrower's liquidation auction,
filled it near the ledger `fill_objective = "earliest-profitable"`
targets, and unwound the filler's position back to zero liabilities
(`docs/testnet-soak.md`'s own Stage 2 results). Everything Phases 1
through 7 built still stands: the fork's differences are additions to
this crate's arithmetic port, not corrections to it. The repository
scaffolding is complete and enforced.

**This bot is NOT non-custodial.** It is designed to hold a signing key and
submit transactions itself — that is the point of a liquidation bot. Treat
that key with the weight it implies. Dry-run is the default for exactly this
reason (see Safety invariants below).

## Orientation commands

```bash
make db-up                          # start Postgres; make check needs it running
make check                          # everything CI runs
cargo test --lib --bins             # unit tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all
make sqlx-prepare                   # after changing a query in src/store.rs
make sandbox                        # all five scenarios, ~25 min (Docker + stellar CLI); SANDBOX_SCENARIO=x for one
make testnet-deploy                 # stand Blend v2 up on public Stellar testnet for the soak's armed stage
make testnet-crash                  # move the testnet soak's oracle price (default $0.075)
make testnet-run                    # run the bot against testnet in dry run (no key; nothing is ever submitted)
make testnet-run-armed              # run the bot against testnet ARMED — the only target that runs the bot armed on testnet
make help                           # Docker Compose lifecycle
```

## Module map

- `src/liquidator.rs` — library root: crate-level docs and the error taxonomy
  (`LiquidatorError`).
- `src/config.rs` — CLI and environment configuration (`Args`, `clap`),
  including the strict boolean parser behind `DRY_RUN`. The auctioneer's
  and the filler's thresholds and cadences are here too: `LIQ_HF_THRESHOLD`,
  `TARGET_HF`, `ORACLE_SCAN_LEDGERS`, `PRICE_DELTA_BPS`, `PLAN_ITERATIONS`,
  `STARTUP_DELAY_LEDGERS`, `HF_SAFETY_MULTIPLIER` (at least 1, refused
  under — the filler's floor is the pool's own `min_health_factor` times
  this, and under one it would sit *below* the operator's stated minimum),
  `REPLAN_LEDGERS` and `INVENTORY_REFRESH_SECS` (both refused at zero, which
  would re-plan and re-read on every ledger), `REPLAN_NEAR_LEDGERS` (zero is
  meaningful: re-plan only at the fill ledger), `XLM_FEE_RESERVE` (decimal
  XLM, and XLM has 7 decimals, so the parsed `Decimal7` *is* stroops) and
  `HIGH_FEE_PROFIT_THRESHOLD` (in the pool oracle's units) are ordinary
  `clap` arguments. So is the operational surface's own set: `PORT` and
  `HTTP_PORT` (`PORT` wins when both are set, since it is the one a
  deployment platform injects; either turns the HTTP server on and
  neither leaves it off), `HTTP_BIND_ADDR` (loopback by default; a
  `0.0.0.0` bind belongs behind an ingress that admits only the platform's
  probes and scraper, since the endpoints carry no authentication and
  `/healthz` costs a store ping per request),
  `HEALTH_MAX_LAG_LEDGERS` (default 10, refused at zero — a bot exactly
  at head would report not-ready on every poll-interval boundary),
  `FAILURE_NOTIFICATION_COOLDOWN_HOURS` (refused at zero: there is no
  "no cooldown" spelling, only shorter ones) and `TELEGRAM_CHAT_ID`,
  which is not a secret and is an argument like the rest. `HttpConfig`
  and `TelegramConfig` are what carry them into `ServiceConfig`.
  The secrets are the exception, and none of them is ever a clap field,
  because argv is world-readable — `DATABASE_URL` and `RPC_API_KEY` as
  much as the rest: the two signing keys —
  `AUCTIONEER_SECRET_KEY`, and `FILLER_SECRET_KEY`, which it falls back
  to — are read from the environment by `main.rs` and handed to
  `Args::signing_keys`, while `TELEGRAM_BOT_TOKEN` is read by
  `Args::service` itself (it pairs with `TELEGRAM_CHAT_ID`: both or
  neither, either alone a startup error). Both signing keys are parsed at
  startup, so a malformed value in
  either is a startup error, and so are the two rules that pair them:
  `DRY_RUN=false` without `FILLER_SECRET_KEY` (the filler signs with its
  own key only, so an armed bot without it would create auctions and never
  fill one), and the two keys being the *same* key (leave
  `AUCTIONEER_SECRET_KEY` unset to share one). `SigningKeys::into_signers`
  answers which key signs which role, and `Signers::shared` tells by
  pointer whether both roles hold the one `Arc`. `parse_pools` reads one
  more per-pool knob than the fields above: `fill_objective`
  (`free-fill`, the default, or `earliest-profitable`, anything else a
  named error) into `PoolConfig::fill_objective`, `math::fill`'s
  `FillObjective`. A test in this file,
  `the_configuration_documents_cover_exactly_the_real_settings`, fails
  unless the set of settings the code reads — every `clap` argument's
  `env` name plus a hard-coded list of the ones read directly — equals
  both the set of `NAME=` lines in `.env.example` (commented out or not)
  and the set of first-column names in `docs/configuration.md`'s settings
  tables (`` | `NAME` | ``, uppercase only, so the pools-file key table
  never counts), printing each set difference on failure; and unless
  `pools.example.toml` parses. A new variable read directly through
  `std::env::var` must be added to that test's hard-coded list by hand.
- `src/main.rs` — binary entry point: tracing setup, argument parsing, exit.
- `src/math/` — the pure port of the pool contract's arithmetic: `fixed`
  (checked rounding), `reserve` (accrual and token conversions), `position`
  (effective values and health factor), `auction` (Dutch-auction scaling),
  `liquidation` (which auction to create — `plan_liquidation` selects the
  bid and lot assets and the percent that closes a borrower's excess down
  to `TARGET_HF`, walking in more assets when the selection cannot),
  `fill` (which auction to *take* — `FillObjective`
  (`config::PoolConfig::fill_objective`) picks which ledger a plan aims
  at: `FreeFill`, the default, aims at `RAMP_END_BLOCKS` (400), where the
  bid has ramped away to nothing and the lot is whole; `EarliestProfitable`
  aims at `fill_delay`'s answer instead — in closed form, proved against
  the contract's own modifiers by `meets_margin`, the fewest ledgers after
  an auction's start at which its lot covers its bid plus the pool's
  profit margin. There is no fill cutoff past 400 either way; `force_fill`
  means one thing only, capping whichever ledger the objective picked at
  `FORCE_FILL_MAX_DELAY` (350). `health_floor` is `min_health_factor ×
  HF_SAFETY_MULTIPLIER`, rounded up; `plan_fill` builds the request list —
  the fill, a repay of each bid asset the wallet holds, a withdrawal of
  each zero-collateral-factor lot asset, a supply of the primary asset —
  by projecting the filler's own post-fill position exactly: a **full**
  fill (the scaling leaves no remainder) is valued against the reserves
  `setoff::project_default` leaves behind rather than the pre-fill
  snapshot's own, since the contract runs the borrower's own default path
  — and any `b_rate` haircut it causes — inside that same transaction
  before it checks the filler's health; every other conversion stays on
  the reserves as read. `supply_headroom` bounds the escalation's own
  supply at the reserve's `supply_cap` by exact search — `cap −
  total_supply()` cannot breach the cap at any rate, but understates the
  room by up to a stroop — and `plan_fill` escalates supply → lower
  percent → later ledger when the projection is short, searching
  candidates exactly rather than estimating one a later round would only
  have to correct, answering `FillSkip::SupplyCapped` when the cap rather
  than the wallet is what stopped it), `setoff` (the fork's default path,
  ported: `project_default` answers what the contract's
  `check_and_handle_user_bad_debt` does to the pool's reserves — the
  borrower's own supply in the debt reserve sets off what it can first,
  then whatever debt remains is destroyed and every b-token holder in
  that reserve is charged for it through a `b_rate` cut, floored at zero
  — so `fill` can project a full fill's own haircut rather than trust the
  pre-fill snapshot), `unwind` (which of the filler's own debts
  to repay from its wallet and which of its collateral to withdraw once a
  fill has left it holding a position — `plan_unwind`'s three steps: repay
  each liability the wallet holds, then with none left withdraw every
  collateral but the primary and the primary down to
  `min_primary_collateral`, else withdraw only while the projection holds
  the two bounds `validate_submit` applies to a position that keeps
  liabilities: the health factor at or above `min_health_factor` plus
  `HEALTH_MARGIN_BPS` — the margin is where an unwind *rests*, not merely
  where it stops starting candidates — and the effective collateral at or
  above the pool's own `min_collateral`, which binds step 3 wherever it is
  the larger of the two. `DUST_FLOOR_BPS` (100) is the smallest partial
  withdrawal of the primary worth sending, and every withdrawal is
  verified by exact projection, backing off when it disagrees).
  Nothing here does I/O and nothing panics.
- `src/chain/xdr/` — ScVal codecs for the pool: `encode` (values, operations,
  simulation envelopes), `keys` (ledger keys, durability included), `decode`
  (entries and view-call returns), `events` (pool events, including the
  fork's `debt_setoff`, `collateral_orphaned` and `orphan_settled`
  alongside the stock `bad_debt` and `defaulted_debt`).
- `src/chain/rpc.rs` — the Soroban JSON-RPC client: the eight methods the
  bot uses, their wire shapes, base64 XDR decoded at the boundary. Every
  result carries the ledger it was taken at.
- `src/chain/pool.rs` — pool reads: a single-ledger `PoolSnapshot` (instance,
  reserves, prices, positions) that `position_data` values with `math`,
  auction and balance reads, and the `submit`, `new_auction` and `bad_debt`
  operation builders. `PoolSnapshot::valued_at` is the one clamp every task
  that values a position shares — the later of the tick's close time and
  the newest reserve entry the snapshot holds — and
  `PoolSnapshot::accrued_reserves` is the clone of the reserves accrued to
  it; they live here, rather than in either task, so the tracker, the
  auctioneer and the filler cannot disagree about which instant a position
  is worth what.
- `src/chain/signer.rs` — the network id and the Ed25519 key. `Signer`
  renders as its address only.
- `src/chain/tx.rs` — the one write path: build with time and ledger bounds,
  simulate, restore archived entries, assemble, fee, sign, send, poll,
  classify into `TxOutcome`.
- `src/chain/script.rs` (`cfg(test)`) — a scripted JSON-RPC server the chain
  tests drive the real client through.
- `src/store.rs` — the bot's durable state: cursors per polling task,
  tracked borrowers (`users` — a row exists only while the account owes
  something) and open auctions (`auctions`), migrated by the embedded
  `migrations/` and read and written through compile-time-checked
  `sqlx::query!`. `i128` amounts and health factors cross the Postgres
  boundary as decimal text, never as a bound number.
- `src/ledger.rs` — the clock: one `LedgerPoller` per pool, reading events
  since a stored cursor, sending each decoded event and then the ledger's
  tick, and advancing the cursor only once the tracker has answered that
  tick's acknowledgement — see the cursor invariant below. A cursor fallen
  out of the RPC's retained window is reported as a `Gap` rather than
  silently caught up on, and at most once per stale cursor, since each one
  costs a full reseed; a pass that cannot prove it drained the range leaves
  the cursor untouched. An optional `Metrics` and `Notifier` (`with_metrics`,
  `with_notifier`; both `None` from `new`) instrument the loop without
  touching what it does: a heartbeat is recorded every `poll_interval`
  for as long as the loop is turning, the pass included — the RPC calls,
  the `getEvents` paging and the wait for the tracker's acknowledgement
  alike, since working is being alive — so the backoff sleep after a
  failed pass is the only stretch that stamps nothing, which is why
  `max_backoff` is what the deadline budgets for and a pass's own
  duration is not; the chain head is recorded the moment
  `getLatestLedger` answers, and a run of `RPC_FAILING_AFTER` (5)
  consecutive failed passes notifies `NotificationKind::RpcFailing` once,
  at the threshold and never past it, with the first successful pass
  logging the recovery and resetting the count. `PollerConfig::liveness_deadline`
  (`LIVENESS_INTERVALS` poll intervals plus `max_backoff`) is what
  `crate::http::liveness` and `crate::service::watchdog_loop` both check a
  heartbeat's age against.
- `src/tracker.rs` — applies chain state to the store: `Tracker::apply`
  writes an event's auction bookkeeping and returns the accounts it named;
  `Tracker::refresh` re-reads named accounts from chain in one snapshot,
  values them at the later of the tick's close time and the newest reserve
  entry the snapshot holds, and upserts or deletes their `users` row at the
  ledger the snapshot was read at; `Tracker::refresh_stale` takes an
  *absolute* ledger cutoff, not a span; `Tracker::seed` collects accounts
  from every configured `SeedSource` — the public analytics API
  (`AnalyticsSeed`) or a static file (`FileSeed`) — deduplicates them,
  refreshes them in batches, and reports how many sources failed so an
  incomplete seed is retried on the next full scan.
- `src/queue.rs` — `SubmissionQueue`: one ordered queue per **distinct**
  signing key — the auctioneer shares the filler's whenever it falls back
  to the filler's key, because two queues on one key is the very race this
  module exists to make unreachable. A Soroban transaction is built against
  its source account's sequence number at prepare time, so two tasks
  preparing for the same key concurrently would race to consume it. The
  queue owns ordering and one rule beyond it: nothing is prepared for a key
  while an earlier transaction's outcome on it is still unknown (see the
  invariant below). Only a failure that provably sent nothing is retried
  here, within the budget its `Submission` carries — `CREATION_RETRIES` is
  3, `FILL_RETRIES` 10, `UNWIND_RETRIES` 2 — backing off from one second,
  doubling, to thirty;
  that is exactly what `QueueError::Chain`'s doc comment narrows the
  variant to, and what lets `Executor::execute` release its wallet
  reservation on it without asking anything else. What to submit, at what
  fee priority, and what a failure means stay the caller's
  (`Auctioneer::act`, `Executor::execute`).
- `src/auctioneer.rs` — the auctioneer. `Auctioneer::decide` reads one
  snapshot per batch of tracked users, values each at the later of the
  tick's close time and the newest reserve entry the snapshot holds — the
  same clamp the tracker applies, so the two cannot disagree, and a snapshot
  the chain has moved past since the tick is valued rather than refused —
  and answers with a
  `Decision` per user: liquidate (a `LiquidationPlan`), move to bad debt, or
  skip with a `SkipReason` (healthy, an auction already open, no plan
  closes the excess, the bot's own account, or no liabilities left).
  `decide` needs no signer at all. `Auctioneer::act` turns a decision into
  an operation, lets the contract judge it by simulating through
  `Submitter::simulate_only` and adjusting the percent against
  `InvalidLiqTooLarge`/`InvalidLiqTooSmall` up to `PLAN_ITERATIONS` times,
  records the creation — dry-run or not — before it submits anything, and
  submits through a `SubmissionQueue` only when one is given — and refuses
  one in dry-run, or without a signer, before anything is simulated.
  `Auctioneer::scan_oracle` is a third, narrower path that decides nothing:
  it compares a pool's current prices against a remembered reference and
  flags **every** borrower exposed to whichever asset moved past
  `PRICE_DELTA_BPS`, for the ordinary recheck path to decide about them.
  That sweep is unbounded on purpose — `Store::flag_exposed_to` is one
  statement per move, with no `LIMIT` — because the reference re-anchors on
  the move it reports, so a borrower one scan skipped would not be reached
  by the next one either. `REFRESH_BATCH` is a *rate* for the decide path
  and must never become a cap here. `Auctioneer::adopt` is the one path
  that writes an auction row the tracker never saw: an auction opened
  before this bot's events cursor produced no `NewAuction`, so when a
  simulation answers `AuctionInProgress` (1212) the entry is read from
  chain and upserted — otherwise the filler, which walks the store, could
  never find it.
- `src/inventory.rs` — the filler's wallet: the balance last read per
  asset, the `XLM_FEE_RESERVE` withheld from the native asset, and the
  must-use `Reservation` a plan takes for what it will spend, consumed or
  released by value exactly once and warning if it is ever dropped
  unsettled. It holds **wallet balances only** — a plan's positions come
  from its own snapshot, because positions valued against a different
  ledger's reserves are exactly the disagreement the accrual gotcha below
  warns about. Its arithmetic saturates, the one sanctioned exception to
  this crate's checked-arithmetic rule: this ledger is a claim against a
  wallet rather than money on chain, so a drift it cannot lose a stroop to
  is repaired by the next read, whereas a checked subtraction would error a
  filler that has nothing wrong with the chain state it is about to act on.
- `src/notifier.rs` — `Notifier` deduplicates a `Notification` by `(pool,
  account, kind)` with a cooldown (`FAILURE_NOTIFICATION_COOLDOWN_HOURS`,
  at least 1 hour and refused at zero), then hands what survives to a
  delivery task rather than to the channel directly: `Notifier::notify`
  takes the dedup entry synchronously and tries a permit from a semaphore
  of `NOTIFY_IN_FLIGHT` (10), spawning the send and answering
  `Delivery::Queued`, `Delivery::Deduplicated` or — no permit free —
  `Delivery::Dropped` without ever awaiting the channel; nothing upstream
  can make a decision depend on whether a notification was delivered, or
  wait to find out. A send that fails, or is dropped for want of a permit,
  rolls back the dedup entry it optimistically inserted and writes the
  notification through `LogChannel` instead, so the operator still sees
  it. `LogChannel` is therefore not a second channel but the fallback
  every configured channel — and an unconfigured deployment — falls back
  to; it logs at `WARN` for `Severity::High` and `INFO` otherwise.
  `Notifier::drain(DRAIN_BUDGET)` acquires every permit with a timeout and
  is what an exit path calls to give in-flight sends a bounded chance to
  leave before the process does. `pub mod telegram` is `TelegramChannel`,
  the second `NotificationChannel`. `NotificationKind` already lists every
  kind spec §7 names, so the semaphore, `drain()` and the Telegram channel
  add no new variant; Phase 8 added the one this crate's own fork
  reconciliation needed, `StockWasmDetected` — raised, `Severity::High`,
  the moment `src/service.rs`'s event handler sees a `bad_debt` event,
  since the fork's contract can never emit one and one on chain means the
  wrong wasm is deployed. Must be used from inside a tokio runtime:
  `notify` spawns.
- `src/notifier/telegram.rs` — `TelegramChannel`: `sendMessage` for
  delivery, `getMe` (`verify`) to prove the configured credentials work
  before `Service::check_config` reports success. The bot token sits in
  the request *path* (`/bot<TOKEN>/sendMessage`), never a header or the
  body, so it is the one secret this module keeps out of everything it
  hands back: every `reqwest::Error` is passed through
  `reqwest::Error::without_url()` before it becomes `NotifyError` text,
  and a refusal's text comes only from Telegram's own `description`
  field, never the raw response body (which echoes the request URL, token
  included, on some of Telegram's own error pages). `with_base_url` is a
  test seam only — no argument or environment variable sets it — for
  aiming the channel at a mock server through `TelegramConfig::base_url`.
- `src/metrics.rs` — `Metrics`: the run's counters and gauges, one
  `Mutex<Inner>` behind synchronous methods (no `.await` anywhere in this
  module, so the lock is never held across one) that the poller, the
  tracker, the auctioneer, the filler and the notifier call as the
  corresponding event happens, and `Metrics::render` to
  Prometheus text exposition format on demand for `/metrics`. Label sets
  are closed enums (`Attempt`, `SkipLabel`, `DeliveryLabel`) rendered with
  every member present, zero included, so a dashboard never has to guess
  whether a missing series means zero or means the bot has not run yet.
  `SkipLabel` grew a sixth member in Phase 8, `SupplyCapped` — a new
  `skips_total` series — for a fill whose own primary-asset supply the
  reserve's `supply_cap` capped rather than the wallet; a new member here
  always means a new series, which is why `ALL`'s own length moves with
  the variant list rather than being derived from it.
  Money is rendered as an integer in the pool oracle's own units —
  `estimated_profit_total` and `estimated_loss_total` — never scaled by
  an assumed number of decimals, and a landed fill's negative estimate
  (a `force_fill` pool's) adds its magnitude to the *loss* counter rather
  than lowering the profit one, since a Prometheus counter that decreases
  is read as a reset. No I/O, no float and nothing panics; a poisoned
  lock is recovered rather than propagated, the same call `notifier`'s
  makes.
- `src/http.rs` — the `/healthz`, `/livez` and `/metrics` server, built
  from `HttpState` and served only when `crate::config::HttpConfig` is
  configured (`PORT` or `HTTP_PORT`). `readiness` (`/healthz`) needs every
  configured pool's processed ledger within `HttpState::max_lag_ledgers`
  of the chain head this process has observed — in **either** direction,
  since a head further than that *behind* the processed ledger is an RPC
  node sitting behind this bot's own committed cursor and a
  `saturating_sub` would read it as no lag at all — that head to have been
  read within `HttpState::liveness_deadline` (`PoolStatus::head_at`,
  stamped by `Metrics::ledger_head`), and the store to answer a ping
  inside `PING_TIMEOUT` (5s). The head's *age* is not a nicety: both
  ledger gauges are this process's own and an RPC outage stops both at
  once, so a readiness that compared only the two would answer `200`
  throughout the one failure it exists to catch. `liveness` (`/livez`)
  needs only that every pool's poller has heartbeated within
  `HttpState::liveness_deadline` (`PollerConfig::liveness_deadline`),
  which already absorbs one worst-case backoff — an RPC outage the
  poller's own backoff is riding out must not fail it — and measures a
  pool that has *never* heartbeated from `HttpState::started` instead,
  the same rule `watchdog_loop` applies: a poller whose first iteration
  has not run yet is starting, not stopped. `/metrics` never fails: it renders whatever
  `Metrics` holds, empty or not, and always answers `200`. `serve` never
  propagates a bind failure to its caller — it logs and returns — because
  a diagnostics port that cannot open must not stop the bot from trading
  (spec §8).
- `src/executor.rs` — one planned fill, from the contract's judgment to the
  audit row, the submission and the settled reservation. `Executor::execute`
  runs the mode guards first — a dry-run executor handed a live
  reservation, a live one handed a dry-run settlement, or *any* queue
  offered to an executor that is dry-run or has no signer to judge with,
  all fail before anything is simulated, recorded or enqueued — then
  simulates the exact `submit` through `Submitter::simulate_only`, writes
  the `fills` row before it sends anything, submits on the filler's queue
  with `FILL_RETRIES`, and attaches the transaction's hash for every
  outcome, the failed, expired and unresolved included, each having
  consumed a sequence number worth naming. `InvalidHf` (1205) and
  `MinCollateralNotMet` (1224) answer `ExecOutcome::Replan`, which the
  filler re-plans once at half the percent; `ChainError::BadSequence`
  answers `Stale`, which clears the plan rather than resending it; every
  other refusal is `Refused` with the code logged. The reservation is
  settled by value on every non-panicking path. No arithmetic on money
  happens here — every amount is `math::fill`'s, passed through unchanged.
  `Executor::unwind` is the same path for `plan_unwind`'s requests — the
  mode guards, `Submitter::simulate_only`'s judgment, submission on the
  filler's queue with `UNWIND_RETRIES` — but it writes no audit row (there
  is no unwind table; the `unwind planned`/`unwind submitted` log lines
  are the record) and never re-plans, since an unwind has no percent to
  lower: a refusal or a stale sequence is left for the next pass.
  `UnwindOutcome::landed` answers whether the chain applied it or may yet
  (`Succeeded` or `Unknown`), which is what keeps a pool unwind-pending.
- `src/filler.rs` — the filler: the I/O around everything `math::fill`
  decides. Once a tick, per pool, `Filler::tick` keeps only the rows worth
  a chain read — a user liquidation, not one of the bot's own accounts,
  every asset accepted by `PoolConfig::supports`, not one this process has
  already recorded a dry-run fill for, and *due*, which `due` defines as
  having no plan, no plan this process made, being within
  `REPLAN_NEAR_LEDGERS` of its planned fill ledger, or `REPLAN_LEDGERS`
  having passed since it was planned — re-reads each kept row's on-chain
  auction entry (a missing entry closes the row, and the **entry**, never
  the row, is what is planned against: someone else's fill reaches the
  entry first), takes one snapshot per pool valued at
  `PoolSnapshot::valued_at`, plans each with `plan_fill`, and executes the
  ones whose fill ledger has come. A fill that landed — or may have — ends
  that pool's walk for the tick: everything still queued behind it was
  projected against the positions and the wallet as they stood *before*
  it, and the next tick reads a snapshot that holds it. Before
  `STARTUP_DELAY_LEDGERS` has
  elapsed it plans and writes but executes nothing, so an operator sees
  what the bot would do before it may do it. One auction's failure is one
  auction's: everything but a `StoreError` is logged with its pool and
  account and the pass carries on, and a raised shutdown flag ends a tick
  *between* auctions, never inside a submission already waiting for its
  outcome. After the walk, one unwind pass runs per pool this run's
  `FillerState` marks pending: a fill that landed or may have (`Succeeded`
  or `Unknown`) makes its pool pending, and so does the run's very first
  tick, for every configured pool — a restart between a fill and its
  unwind must not strand the position, and an idle pass costs one snapshot
  and one wallet read. The pass reads its own snapshot even for a pool the
  fill walk just read in this same tick, since a fill may have changed the
  position in between — and a pool whose fill *or whose own earlier
  unwind* landed, or may have, is passed over entirely until a snapshot
  provably holds that submission (its ledger at or past the one the
  submission landed in, and never for a `TxOutcome::Unknown`, which landed
  in no ledger anyone can name). The evidence is the run's, not the tick's:
  `FillerState::unwind_after` holds it, so a pool is held across every
  later tick whose snapshot is still behind, and the entry is a high-water
  mark — the snapshot that proves it does not clear it, because
  `latestLedger` is not monotonic across calls and most of what follows the
  gate can return having sent nothing while the pool stays pending. It is
  raised by the next submission that lands and dropped only where the pool
  itself is cleared. What the position then looks like is not evidence
  either way, and planning against a snapshot taken before the submission
  sizes a withdrawal against liabilities the fill is about to raise, or
  re-plans a withdrawal the contract then caps at what is left. It plans through
  `math::unwind::plan_unwind` and executes through `Executor::unwind`
  behind the same startup gate and queue a fill uses. A pass that moves
  something leaves its pool pending for the next tick's fresh plan; the
  first pass that builds no requests (`UnwindPlan::is_idle`) clears it.
  Debt the wallet cannot repay notifies
  `NotificationKind::UnwindLeftovers` at `Severity::High` once per pool,
  not again until a later pass finds the pool clean. A pass that *moves
  nothing* — refused, stale, a submission that did not land, or a non-store
  executor failure — keeps the pool pending and backs it off by `2^n`
  ledgers, capped at `UNWIND_BACKOFF_MAX_LEDGERS` (64); the pass whose run
  reaches `UNWIND_SETBACK_ALERT` (3) raises one
  `NotificationKind::SubmissionDropped` at `Severity::High` naming the
  cause, and a pass that lands or finds the pool idle ends the run. There
  is no
  top-level `unwind.rs`: the pass shares the filler's inventory, executor,
  wallet refresh and per-tick state closely enough that it lives here as a
  second `impl Filler` block, with the pure builder in `math::unwind`.
  `Metrics` and `Notifier` are wired through every step above rather than
  through a step of their own: `fills_total{result}` counts every recorded fill
  (`attempted`, and `succeeded`/`failed` once the chain answers),
  `skips_total{reason}` counts every planner and executor skip a
  `SkipLabel` names — once per auction *per reason*, never once per tick it
  stays open for, which is what `FillerState::counted_skips` and
  `Filler::count_skip` are for: the filler re-makes every one of those
  decisions every tick, so one auction the planner refuses forever would
  otherwise bury the other five reasons. A skip decided *after* the chain
  read is keyed by the **entry's** `block`, never the row's: the chain can
  hold a new auction for an account before the tracker has opened it, and
  a key on the older row is pruned the moment the tracker catches up —
  while the auction is still open — so the same decision would count
  twice. Only the pre-read `UnsupportedAssets` keys on the row, because
  no entry has been read there —
  `estimated_profit_total` adds a landed fill's `est_profit` (and
  `estimated_loss_total` its magnitude when that estimate is negative),
  `reserved_inventory{asset}` is re-gauged from
  `Inventory::reserved()` after every tick, and `unwind_pass()` counts
  every unwind attempt. `NotificationKind::FillConfirmed` (Low) and
  `FillFailed` (High) answer a fill's `Succeeded`/`Failed`;
  `UnfundedFill` (Medium) answers `FillSkip::Unfunded`; the queue's
  `SubmissionDropped` (High) is this module's own, alongside the unwind
  pass's setback alert.
- `src/service.rs` — wiring: `Service::check_config` validates the
  configuration against the chain *and* the database (connect and ping, per
  the spec's deployment contract) and reports without following anything —
  and, when Telegram is configured, verifies it with one `getMe` call
  (`verify_telegram`), because a refused token must fail the deploy smoke
  test spec §10 makes this, not the first notification the operator needed
  to see; `Service::run` connects and migrates the store, seeds every pool
  whose tracked-user count or events cursor is missing, then runs seven
  kinds of task until a shutdown signal arrives and every one has returned:
  one `LedgerPoller` per pool, one tracker task consuming their shared
  channel, one auctioneer task, one filler task, one watchdog task
  (`spawn_watchdog`/`watchdog_loop`, reporting `NotificationKind::PollerStalled`
  for a pool whose heartbeat has gone past `PollerConfig::liveness_deadline`
  — it cannot be the poller's own report, because a wedged loop cannot
  report itself), one HTTP server (`crate::http::serve`) when `PORT` or
  `HTTP_PORT` gave the run an address — spawned *before* the seed pass,
  alone among the tasks, because a seed of a busy pool is tens of seconds
  during which a restart probe must still be able to reach `/livez`; the
  seed itself heartbeats through `ledger::heartbeat_while`, for *every*
  configured pool and not only the one it is seeding — no pool has a
  poller yet, so one the pass has not reached would otherwise be the
  same restart loop — and — only
  when armed — one
  submission-queue worker per *distinct* signing key, which is what
  `spawn_queues` is for. The run's one `Metrics` and one `Notifier`
  (`build_notifier`: the Telegram channel when both credentials are
  configured, `LogChannel` otherwise) are built once, before the seed pass,
  and carried together as one `Instruments` to every loop that needs both.
  Once its tasks are running, however `run` ends it leaves through
  `finish_run`, which drains the notifier (`Notifier::drain(DRAIN_BUDGET)`)
  on both the `Ok` and the `Err` path; an earlier `?` (validation, the
  seed pass) has nothing in flight, since nothing before the tasks
  notifies. The two exits that skip it are the second `SIGINT`/`SIGTERM`
  (`spawn_shutdown_listener`'s `exit(130)`, deliberately: a second signal
  means now) and a task panic. A release build — the image's — sets
  `panic = "abort"` (`Cargo.toml`'s `[profile.release]`), so there a
  panic aborts the process where it happens, with no unwind at all; in a
  debug or test build it unwinds, `resume_on_panic` carrying it straight
  out of `drain_tasks`, past `finish_run` entirely. The tracker loop treats
  a `TrackerError::Store` as fatal and a `Chain` or `Math` one as transient
  — it declines the tick, and the same range is read again. Both entry
  points share `validate` and `validate_filler`: the filler's account must
  exist on the network and hold at least `XLM_FEE_RESERVE` of the native
  asset — armed, either failure is a startup *error*; in dry-run each is a
  warning — and, armed only, holding less than a pool's
  `min_primary_collateral` is a warning. With no `FILLER_SECRET_KEY` at all
  there is nothing to check and the warning says so: the filler plans
  against an empty inventory and simulates nothing.

  **The auctioneer and the filler are separate tasks, and must stay two.**
  Neither is inside the tracker's tick: the tracker's acknowledgement is
  what commits the poller's cursor, and neither a decision nor a fill is a
  ledger effect, so either in that path could stall the cursor. Both are
  fed by a `tokio::sync::watch<LedgerTick>` the tracker publishes *after*
  it acknowledges — never a second reader of the poller channel, which
  would break the per-sender ordering the cursor rests on. Per tick the
  auctioneer fires the oracle-scan and full-scan-and-flag cadences when
  due, then decides and acts on every pool's currently flagged users; the
  filler walks every pool's open auctions. Each holds its own
  `StartupGate`, because each measures `STARTUP_DELAY_LEDGERS` from the
  first ledger *it* saw and each answers for its own key — the gate decides
  whether the submission queue is offered to `Auctioneer::act` at all, and
  whether `Filler::tick` may execute rather than only plan.
  `src/service.rs`'s module doc has the long form.
- `src/harness.rs` (`cfg(test)`) — scripted-RPC and store scaffolding shared
  by the store, ledger and tracker tests: the fixture's pool, its two
  borrowers, and the golden health factors `chain::xdr::decode`'s test
  derives from the same contract-attested inputs. `RecordingChannel` is
  the `NotificationChannel` the notifier, ledger and service tests assert
  against: it keeps every notification it accepted rather than only
  logging it, and its `fail` flag puts a channel failure in front of
  `Notifier`'s own rollback. Every delivery is spawned, so a test reads
  `sent`/`sent_count` only after `Notifier::drain` has answered.
- `migrations/` — the store's schema, embedded in the binary and applied by
  `Store::migrate`: `0001` is the initial schema (cursors, `users`,
  `auctions`); `0002` adds the `creations` audit table (every auctioneer
  submission, the ones dry-run only simulated included) and the
  `users.recheck_ledger` flag with its partial index, the queue
  `Store::users_needing_recheck` reads oldest-flag-first; `0003` adds the
  `fills` audit table, one row per fill attempt the executor recorded,
  dry-run or not, written before anything is submitted with the
  transaction's hash attached — once, never replaced — when there is one:
  `dry_run` is the mode the bot was configured in, `tx_hash` is the
  evidence that a transaction was named, and a row with `dry_run = false`
  and no attached hash is an armed attempt that was never submitted, or
  was submitted with its outcome unrecorded — the signing account's
  sequence number tells the two apart, not the row. A new migration, never
  an amendment, once one is applied anywhere. The
  `sqlx::query!` macros in `src/store.rs` are checked against it at compile
  time; see the query-macro gotcha below.
- `examples/pool_snapshot.rs` — prints a live pool's reserves and users'
  health factors.
- `examples/capture_fixture.rs` — refreshes `tests/fixtures/` from a live
  RPC through `curl`. See that directory's README.
- `tests/liquidation_sandbox.rs` and `tests/sandbox_harness/mod.rs` — the
  sandbox tier's five scenario tests, and the one place outside the
  testnet soak's armed stage (`scripts/testnet/run-bot.sh --armed`) where
  the bot is run with `DRY_RUN=false` and a signing key. `tests/sandbox_harness/mod.rs`
  is the machinery every scenario shares — the standalone-network gate,
  the spawned bot, the per-run database(s) and every named wait — and
  holds `SCENARIOS`, one of five places the scenario names are written:
  `deploy.sh`'s accepted list, the Makefile's `SANDBOX_SCENARIOS`, the
  nightly matrix and `tests/liquidation_sandbox.rs`'s `#[ignore]`d test
  fns are the others, and `scripts/check-repo-invariants.sh` fails unless
  all five name the same set. `tests/liquidation_sandbox.rs` is one
  `#[tokio::test]` per scenario, and `make sandbox-test SANDBOX_SCENARIO=x`
  runs one against this one binary: it runs `--list` first and refuses
  unless `--exact x` names exactly one test fn — libtest exits `0` having
  run nothing for a name that matches none — then runs it with
  `--include-ignored`, which a fn that lost its `#[ignore]` cannot slip
  past either. `liquidation` is the standard run: an armed bot
  creates a borrower's liquidation auction after `crash.sh` moves the
  oracle's price, fills it, and unwinds the position it took, proved
  through the `creations`/`fills` audit rows' transaction hashes, the
  filler's on-chain position and `/metrics`. `check_config` runs
  `RUN_MODE=check-config`'s eight table-driven exit-code and warning cases
  against a live network — (g) and (h) pinning what a wrong
  `NETWORK_PASSPHRASE` does (see the gotcha below) — proving none of them
  sends a transaction or migrates the database. `dry_run` is the tier's
  most important safety test: across three phases — deciding to create an
  auction, an armed creator whose filler cannot fill what it creates, and
  deciding to fill that live auction — a dry-run bot holding the real
  filler key never sends a transaction, proved by the key's sequence
  number, the chain and the audit rows. That evidence is what a
  transaction leaves behind, not what the process does locally, and the
  one historical way a dry run sent something — `Submitter::prepare`
  signing and sending a `RestoreFootprint` for an archived footprint — is
  unreachable on a network minutes old; the unit tests
  `a_dry_run_never_restores_an_archived_footprint` (`src/auctioneer.rs`)
  and `a_dry_run_that_needs_a_restore_is_refused` (`src/executor.rs`)
  cover it, not this scenario. `unwind_repay` is deployed with
  the filler holding no USDC, so its fill leaves debt behind for the
  unwind's repay branch to clear once `scripts/sandbox/mint.sh` funds the
  wallet and a second bot restarts — the one branch `liquidation`'s own
  run never exercises, since its fill repays the bid outright.
  `restart_adopt` `SIGKILL`s a bot right after it creates an auction, then
  runs a second instance on a database that never recorded it: that bot
  adopts the auction on chain (`AuctionInProgress`, 1212) rather than
  trying to create a second one, and fills it — the only path in this
  tier that reaches `Auctioneer::adopt`. Every scenario is `#[ignore]`d,
  so `cargo test` never starts a container, and each refuses to run at
  all unless `target/sandbox/sandbox.env` exists, names the standalone
  network and was deployed for that scenario. `spawn_bot` and
  `run_check_config` ask the node for its network again immediately before
  every spawn and hand the binary that URL and the passphrase it answered
  with; only `run_check_config` can be given another passphrase, and a
  long-running bot never is. Nothing in either file
  panics through `unwrap`/`expect`: every failure after a bot is spawned
  goes through `fail`, or — for `check_config`'s short-lived runs, which
  have no bot to tail — `fail_check`, either of which prints what it has
  before it panics.
- `scripts/sandbox/` — the tier's scripts, all `set -euo pipefail`. Every
  one that drives the sandbox itself — `fetch-artifacts.sh`, `up.sh`,
  `deploy.sh`, `crash.sh`, `mint.sh`, `down.sh` and
  `test-network-pinning.sh` — sources `lib.sh`, and through it
  `versions.env`; `test-cargo-jobs.sh` and `test-cargo-config.sh` source
  neither, because what they test is `scripts/cargo-jobs*.sh`, which
  reaches no network and no pin. `lib.sh` holds log/die, `sandbox_dir`, `sha256_check`/`fetch`, `wait_for_rpc`,
  the shared network gate — `require_network_passphrase URL EXPECTED
  LABEL` refuses the public mainnet passphrase by name before it compares
  anything else, dies unless `URL`'s own `getNetwork` answers exactly
  `EXPECTED`, and only then builds `sandbox_network_args` from *that* URL
  and *that* passphrase, never a default — and the `sandbox_network_args`
  flags it pins, with `require_standalone_network URL` now one line on
  top of it (`require_network_passphrase(url, SANDBOX_PASSPHRASE,
  "sandbox")`); the `invoke`/`invoke_view` wrappers that log a contract's
  *role* and never an argument; and `env_write`, which truncates and
  `chmod 600`s before it writes. `scripts/testnet/lib.sh`'s
  `require_testnet_network` is the same shape pinned to testnet's own
  passphrase instead — one gate shared by both tiers, each pinning every
  later `stellar` call to the passphrase its own node actually answered
  with, never a tier's assumed one. `versions.env` holds every pin: the five wasm
  URLs with their SHA-256s, the `stellar` CLI release and both tarball
  hashes, the quickstart image by digest, `SANDBOX_PASSPHRASE`, and
  `SQLX_CLI_VERSION`, which is not the sandbox's but has the same
  two-installers problem — see the gotcha on the database sweep below.
  `lib.sh` sources `versions.env` from its own directory and no
  environment variable selects it: that one file names the passphrase the
  standalone gate compares against *and* the hash every artefact is
  verified by, so a redirect would defeat both at once.
  `fetch-artifacts.sh` downloads and verifies the wasm; `up.sh` starts the
  pinned quickstart container (bound to `127.0.0.1`), waits for its RPC to
  be healthy *and* closing ledgers, and removes the container again if
  either that wait or the standalone gate fails — a container it created
  and could not bring up is its to clean up, since `make sandbox` never
  reaches `down.sh` on that path and the next run refuses on it;
  `deploy.sh` stands Blend v2 up in ten steps and writes
  `target/sandbox/sandbox.env`, keyed by `SANDBOX_SCENARIO` (one of the
  tier's five names, `liquidation` by default): the only thing it changes
  is step 2's mint, which `unwind_repay` alone skips, leaving the filler's
  wallet with no USDC to repay a fill's bid; `deploy.sh`'s own
  `require_funded` re-requests friendbot funding every few seconds, on a
  90 s budget, while it polls Horizon for an account to exist, because
  `up.sh`'s health gate can go green before friendbot behind it is ready
  and `stellar keys generate --fund` exits `0` either way; `crash.sh`
  moves the oracle's XLM price; `mint.sh AMOUNT` mints the filler more
  USDC, signed by the issuer — `unwind_repay`'s way of funding the wallet
  its own deploy left empty, once the debt it means to prove is already
  outstanding; `down.sh` removes the container and `sandbox.env`.
  `test-cargo-jobs.sh`, `test-cargo-config.sh` and
  `test-network-pinning.sh` are shell tests — the first two for the two
  scripts below, the third for the network pinning the gotcha below
  describes — run by hand and by `sandbox.yml`, which runs all three
  before it starts a network.
- `scripts/cargo-jobs.sh` and `scripts/cargo-jobs-config.sh` — the
  cgroup-aware build-job cap and the one thing that writes it down.
  `cargo-jobs.sh` prints `min(nproc, max(1, memory_limit / 2 GiB))`, the
  limit read from cgroup v2's `memory.max`, then v1's
  `memory.limit_in_bytes`, then `/proc/meminfo` (`CARGO_JOBS_NPROC` and
  `CARGO_JOBS_MEM_BYTES` override both inputs, which is what makes the
  formula testable); `cargo-jobs-config.sh` puts `jobs = N` into
  `~/.cargo/config.toml`'s `build` table — creating the table, or
  inserting into the one already there, or leaving a file that already
  sets `jobs` byte-for-byte alone — in whichever of TOML's two spellings
  the file already uses: a `[build]` header (indented or with a trailing
  comment counts) or root-level dotted keys (`build.incremental = true`,
  and `build . incremental = true`, since TOML ignores the whitespace
  around a dot and a spelling that goes unrecognised gets a second `build`
  declaration appended). The key lands directly under a header, and
  directly *above* the first dotted line — above, because a dotted value
  may span several lines, of which only the opening one matches anything,
  so a position after the last match can fall inside a value and a
  position before a line never can. See the OOM gotcha below.
- `.github/workflows/sandbox.yml` — the nightly run of the tier, on
  `schedule` and `workflow_dispatch` only, never `push` or
  `pull_request`, and deliberately outside `ci.yml`'s `ci-summary` needs
  list: that gate reads a skipped job as a failure, which is right for a
  workflow where nothing is conditional and wrong for one that has no
  pull-request run to skip. `strategy.matrix.scenario` runs all five
  scenarios, `fail-fast: false`, each its own job on its own runner,
  because each result stands on its own (`make sandbox` locally stops at
  the first failure instead, for a developer who wants that failure's
  state). Both the deploy step and the test step take the scenario
  through `env:` — `SANDBOX_SCENARIO: ${{ matrix.scenario }}` — and the
  test step's script reads it as `"${SANDBOX_SCENARIO}"` (`make
  sandbox-test SANDBOX_SCENARIO="${SANDBOX_SCENARIO}"`), never a `${{ }}`
  spliced into the shell; the uploaded log artifact is named
  `sandbox-logs-${{ matrix.scenario }}` so five jobs' logs do not collide.
  The workflow-level `concurrency` group of `sandbox`, with
  `cancel-in-progress: false`, serialises whole *workflow runs* rather
  than the matrix jobs inside one. On hosted runners every job of every
  run gets a fresh VM, so overlapping runs would not collide and the
  group buys ordering and runner minutes; a real collision — two jobs on
  one machine fighting over host port 8000 and the `blend-sandbox`
  container name `up.sh` refuses on — needs self-hosted runners sharing a
  machine, and even there the group keeps runs apart, not the matrix jobs
  inside one. It queues rather than cancels because a cancelled run never
  reaches its teardown. Every job masks the filler's key
  (`::add-mask::`) immediately after its own deploy writes it and before
  anything runs the bot, and uploads `target/sandbox/*.log` — `sandbox.log`
  and the scenario's `bot*.log` files — a path, not a mask, because
  uploaded artifacts are not masked.
- `scripts/testnet/` — the soak's own tier (`docs/testnet-soak.md`), a
  sibling of `scripts/sandbox/` rather than a mode of it: the same
  protocol stood up on public Stellar testnet, with friendbot's testnet
  XLM as the only capital. `lib.sh` sources `scripts/sandbox/lib.sh` and
  adds `require_testnet_network` — one line over the shared
  `require_network_passphrase`, pinned to testnet's own passphrase
  (`Test SDF Network ; September 2015`) — `testnet_dir` (`target/testnet`,
  this tier's own scratch directory), the `testnet-soak-` key names, and
  `require_funded_testnet`, which polls testnet Horizon and re-requests
  testnet's own public friendbot on a 120 s budget, wider than the
  sandbox's 90 s since this friendbot is a shared public service rather
  than a container a moment behind its own health check. `deploy.sh` is
  `scripts/sandbox/deploy.sh`'s own ten steps run against testnet instead:
  it derives the native asset's id with `stellar contract id asset
  --asset native` rather than deploying one (testnet already carries that
  SAC — see the Gotchas below), uploads the pool wasm as its own
  preflight step and stops, naming the reason, if testnet's protocol
  rejects bytes pinned against an older `soroban-sdk`, computes the Comet
  approval's live-until ledger from testnet's own current ledger rather
  than a fixed literal, and ends by writing `target/testnet/testnet.env`
  (mode 0600) with the deployed addresses and the filler's secret key —
  read once, never echoed, logged or passed as an argument. There is no
  `down.sh`: testnet is not this repository's network to reset, only to
  rebuild on top of. `crash.sh [PRICE]` moves that deployment's own
  oracle's XLM price, the one lever that turns `deploy.sh`'s healthy
  borrower liquidatable. `run-bot.sh [--armed]` is the only path by which
  `DRY_RUN=false` ever reaches the binary on testnet: plain (`make
  testnet-run`) reads `target/testnet/pools.toml` — not generated by any
  script, and `docs/testnet-soak.md`'s own job to specify — and always
  sets `DRY_RUN=true` with no key read; `--armed` (`make
  testnet-run-armed`) regenerates `pools.armed.toml` and
  `seed.armed.toml` from `testnet.env` on every run and exports
  `DRY_RUN=false` and `FILLER_SECRET_KEY` into the child process's own
  environment only, after confirming `testnet.env` exists. Its gate runs
  on exactly the URL the binary is handed — armed, `testnet.env`'s
  `TESTNET_RPC_URL`, sourced and its passphrase compared with the pin
  first, as `crash.sh` does — and the binary's `RPC_URL` is the
  `SANDBOX_RPC_URL` that gate exported. Before its exports it unsets every
  other setting the bot reads, so the operator's shell hands the binary
  no signing key (the dry run's "no key" is literal: both key variables
  are unset), no RPC credential, no Telegram pair and no tuning knob; an
  armed run also ignores an inherited `RUST_LOG`, and **either** mode
  refuses one naming `trace`, since both hold `DATABASE_URL` and the
  armed one the filler's key besides, and `docs/deploy.md` §6 forbids
  `trace` on a bot holding a secret. That list is by hand: a setting
  added to `src/config.rs` belongs in it too. Its last step before the
  `exec` takes `flock -n` on `target/testnet/<database>.lock`, held on a
  descriptor the `exec` hands the bot, so it is released only when the
  bot exits: one run per database at a time, which is what stops a
  second `make testnet-run-armed` signing with the live run's key.
  `TESTNET_RUN_PORT` and `TESTNET_RUN_DATABASE`, together or not at all,
  move a dry run to its own port, database and `run-<database>.log` so the
  script can be exercised beside a live run — never with `--armed`, which
  refuses them because a second armed bot would sign with the live run's
  key, and never on a mode's own database, which `TESTNET_RUN_DATABASE`
  refuses to name.
  Every script here calls `require_testnet_network` before its first
  `stellar` call or chain read, exactly the sandbox's own discipline with
  testnet's passphrase pinned in place of the standalone network's.
- `examples/scan_borrowers.rs` — finds accounts worth tracking in a
  pool's recent event history: pages `getEvents` from a start ledger to
  chain head the way `LedgerPoller` does and collects every account any
  decoded event names, printing them and a ready-to-paste `[accounts]`
  block in `SEED_FILE`'s shape. Read-only: no key, no store, nothing
  signed. This is the soak's answer to a pool with no analytics API to
  seed from, and it is explicit about its own limit — a start ledger
  older than the RPC's retained window is narrowed rather than failed,
  and a scan that stops at its own page cap says so, since either reads
  exactly like "this pool has no other borrowers" unless it says
  otherwise.
- `examples/soak_report.rs` — prints the evidence either soak stage has
  produced for one pool: the tracked-user count and open auctions, every
  `creations` and `fills` row with its `dry_run` and `tx_hash`, and, for
  accounts named on the command line, their on-chain position read
  fresh. Read-only — it opens the store without migrating it and takes no
  key — and reads `creations`/`fills` with runtime `sqlx::query_as`
  rather than the compile-time macros, since `.sqlx/`'s offline metadata
  covers only `--lib --bins`, never `examples/`.

The module layout beyond this follows
`docs/specs/2026-09-04-blend-liquidator-bot-design.md`; see
Status above for what remains.

## Conventions

- `clippy::pedantic` is warn-level with `unwrap_used = "deny"` (see
  `Cargo.toml`'s `[lints.clippy]`); tests are exempted via
  `allow-unwrap-in-tests` / `allow-expect-in-tests` in `clippy.toml`, **not**
  by relaxing the lint.
- Structured `tracing` logs in the crate, never `println!` or ad hoc
  formatting. `examples/` are terminal tools an operator runs by hand, and
  they print to stdout.
- Doc comments state constraints and invariants, not a narration of what
  changed.
- Money is never `f64`. Balances, debt, collateral and prices are exact
  integer or fixed-point quantities on-chain; floats are for display and
  USD-denominated config knobs only.
- Secrets arrive through the environment, never as command-line arguments —
  `/proc/<pid>/cmdline` is world-readable, and the value shows up in `ps`,
  `docker inspect` and `docker compose config`.

## Safety invariants a change must not break

- **The events cursor means "applied", never "sent".** A `PollerMessage::Tick`
  carries a `oneshot` sender; the tracker answers it only once that ledger's
  whole effect is in the store, and `LedgerPoller::poll_once` writes the
  cursor only on that answer. Committing earlier is not a narrow race to be
  shrunk with a smaller channel: everything still queued would be a ledger
  the store claims to have applied, a kill would drop it, and nothing would
  ever re-read it — `seed_pools_needing_it` reseeds only an empty store or a
  missing cursor. The failure is loud and the loss is silent. A dropped
  acknowledgement therefore means "not applied": do not commit, and let the
  poller's backoff slow the re-read.

- **Nothing is sent for a key while an earlier transaction's outcome on it
  is unknown.** `run_queue` resolves every submission to a terminal outcome
  before it takes the next: a `TxOutcome::Unknown` is polled by the hash
  the queue already holds until it is terminal, and a send whose *answer*
  was lost is resolved the same way rather than resent, because the RPC may
  already have forwarded the envelope. Moving on instead would prepare the
  next submission against a sequence number an in-flight transaction may
  still consume, and the `BadSequence` that follows has no recovery but
  re-planning from fresh state. Only a failure that provably sent nothing —
  a `prepare` that failed before any envelope left, or a send the RPC
  refused outright — may be retried, within the budget its `Submission`
  carries.
- **Dry-run is the default.** `DRY_RUN` (env) / `--dry-run` (flag) defaults to
  `true`. Live trading requires explicitly setting it to `false` — there is no
  other way to opt in. The flag accepts an optional value so it works from
  argv-only surfaces (bare `--dry-run` means true; `--dry-run=false` opts out),
  and the parser accepts **only** the literal strings `true` and `false`.
  Widening that to `1`/`yes`/`on` is not a convenience: each spelling is
  another way into live trading, and the dangerous direction is silent.
- **The three-way Rust version pin.** `Cargo.toml`'s `rust-version`,
  `rust-toolchain.toml`'s `channel`, and the Dockerfile builder's `FROM
  rust:X-bookworm` must move together. `scripts/check-repo-invariants.sh`
  gates it because the failure does not name itself: CI and Docker cheerfully
  compile syntax the declared MSRV does not support.
- **`CI Summary` treats a skipped job as a failure.** Nothing in `ci.yml` is
  path-filtered or conditionally gated, so a skipped job means a condition
  regressed. `devcontainer.yml` is path-filtered, which is exactly why it is
  kept out of that gate, and `sandbox.yml` is outside it for the same kind
  of reason: it runs on `schedule` and `workflow_dispatch` only, so on a
  pull request there is no run of it for the gate to wait on at all.
- **The maths agrees with the contract, and a fixture proves it — for what
  the contract actually attests.** `tests/fixtures/mainnet-fixed-v2.json`
  holds one mainnet ledger's entries *and* the contract's own answers at
  that ledger. Two things are contract-attested, and are never edited to
  match the code: accruing the stored entries must reproduce the contract's
  `get_reserve` to the stroop, and decoding the stored positions must
  reproduce the contract's `get_positions`. The pool exposes no
  health-factor view, so the health factors in `src/chain/xdr/decode.rs` and
  the synthetic-timestamp accrual in `src/math/reserve.rs` are golden values
  this port derived from those attested inputs, not contract answers.
  Changing one of those requires re-deriving it from the contract source,
  never from the code under test.

## Gotchas

- The dev container is memory-constrained by whatever Docker Desktop is given,
  while `nproc` reports the host's full core count — so cargo can fan out more
  parallel jobs than there is RAM for, dying with `signal: 9` or `collect2:
  fatal error: ld terminated with signal 9`. Since Phase 2 the tree includes
  `reqwest`, `rustls` and `hyper`, and a cold build in a small container does
  hit this. Cap it:

  ```bash
  CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo test --lib --bins
  ```

  The debug-info knob is **per profile**, so `cargo install` (release profile)
  needs `CARGO_PROFILE_RELEASE_DEBUG=0` instead.

  The dev container caps it without being asked:
  `post-create.sh` runs `scripts/cargo-jobs.sh` — `min(nproc, max(1,
  memory_limit / 2 GiB))`, the limit taken from the cgroup where there is
  one rather than from `/proc/meminfo`, which reports the host's — and
  `scripts/cargo-jobs-config.sh` writes it to `~/.cargo/config.toml` as
  `[build] jobs`. **Once**: a config that already sets `jobs` in `[build]`
  is left byte-for-byte alone, an operator's own choice winning over this,
  and a `build` table that exists without one gets `jobs` inserted into
  *that* table rather than a second declaration appended — cargo refuses
  to parse a config that declares `build` twice, which is a worse failure
  than the OOM the cap prevents. "That table" is whichever spelling the
  file uses, an indented `[build]`, one with a trailing comment, and the
  root-level dotted `build.<key>` form — whitespace around the dot
  included — among them; the dotted insert goes above the first such line,
  never after the last, so a value spanning several lines cannot be split
  in two; and an unterminated last line is terminated before anything is
  written after it. An environment
  `CARGO_BUILD_JOBS` still overrides the file at build time, which is what
  the one-liner above is.
- Commit signing in the dev container: `user.signingkey` copied from the host
  is a **host path** that does not resolve inside the container. The durable
  fix is a literal `key::ssh-ed25519 ...` value in the host's `~/.gitconfig` —
  it copies in verbatim on every rebuild and needs no script. See
  `.devcontainer/git-signing.sh`.
- The `stellar` CLI in the dev container is a **verified release binary,
  never a source build**: building it from source is minutes on every
  rebuild, for a tool only `scripts/sandbox/*.sh` invokes.
  `.devcontainer/post-create.sh` fetches the tarball for the machine's
  architecture from `scripts/sandbox/versions.env`, checks its SHA-256
  through `lib.sh`'s `fetch` (fatal on a mismatch, in a subshell so the
  step stays non-fatal like every other one below the toolchain) and only
  then installs `~/.local/bin/stellar`, skipping the whole thing once
  `stellar --version` already reports the pinned version. The version
  lives in exactly one file: `versions.env`. Both places that install the
  CLI — `post-create.sh` and `.github/workflows/sandbox.yml` — read it
  from there, and `scripts/check-repo-invariants.sh` fails unless each of
  them does, on a line that is not a comment (a stale `# shellcheck
  source=` directive is not a reference) and without naming a
  `stellar-cli-<version>` release literally. Two CLI versions is two
  different sandboxes, one of which nobody can reproduce, and nothing
  about that disagreement names itself: both sides install *a* CLI and
  both are green.
- The `stellar` binary links `libdbus` at runtime — its OS-keychain
  identity backend, which nothing here uses and which is a dynamic
  dependency regardless — so on a Debian-family image without
  `libdbus-1-3` even `stellar --version` fails with "cannot open shared
  object file". `post-create.sh` and `sandbox.yml` both install it behind
  the same `ldconfig -p | grep -qF libdbus-1.so.3` test, a no-op wherever
  the image already carries it (the GitHub runner does, so far).
- Money is `i128` in each asset's own decimals, but the scales differ by
  field: v2 rates (`b_rate`, `d_rate`) are 12 decimals, factors and
  utilisation are 7, prices are in the oracle's own decimals (7 on the
  mainnet pools, but read it, don't assume it). Mixing two of those silently
  produces a number that looks plausible.
- Storage durability is part of a ledger key. Auctions live in *temporary*
  storage; everything else the bot reads is persistent. Asking for an auction
  with the persistent durability returns no entry rather than an error, which
  reads exactly like "no auction exists".
- `reqwest` is pinned to 0.12 with `rustls-tls-native-roots` and no default
  features on purpose: that feature set is the one whose licence tree
  `cargo deny` accepts. `rustls-tls` pulls `webpki-roots` (CDLA-Permissive)
  and 0.13's `rustls` feature goes through `aws-lc-rs` (OpenSSL licence);
  neither is in `deny.toml`, and adding them there is a licence decision,
  not a build fix.
- `stellar-strkey` moves only with `stellar-xdr`. The bot depends on it
  directly just to decode an `S…` secret, at the version `stellar-xdr`
  itself depends on — a `0.0.x` requirement, which cargo reads as that
  exact patch — so any other version is a second copy, not an upgrade.
  Dependabot ignores it for that reason, and `scripts/check-repo-invariants.sh`
  fails unless `Cargo.lock` holds exactly one. Two means a `stellar-xdr` bump
  moved its copy: bump this one in the same PR.
- `getLedgerEntries` omits absent keys rather than returning nulls, so a
  lookup must go by key, never by position, and "the RPC returned fewer
  entries than keys" is the normal shape of "some of these do not exist".
  `RpcClient::ledger_entries` (`src/chain/rpc.rs:376`) already chunks a
  request into `ENTRY_BATCH`-sized (200-key) batches, so there is no
  key-count ceiling and nothing skips a pool for holding too many
  auctions. What a longer key list costs instead: every batch must report
  the same `latestLedger`, or the read is refused whole as
  `ChainError::LedgerMoved` (`src/chain/pool.rs:304-334`) and
  `PoolReader::snapshot` retries up to `SNAPSHOT_ATTEMPTS` (3) times. This
  is pre-existing and applies to any snapshot, not only a wide one — but
  `Filler::tick`'s snapshot key list now spans the filler's own account
  *and* every live auction's borrower (`math::setoff::project_default`
  needs the borrower's positions to project a full fill's own haircut),
  so it spans more batches on a pool with many open auctions and is
  correspondingly more likely to straddle a ledger close and pay for a
  retry. It still fails closed — a `LedgerMoved` that survives every
  retry is returned to the caller, never averaged across two ledgers.
- The `sqlx::query!` macros in `src/store.rs` are checked at compile time,
  so a build needs either a live database (`make db-up && sqlx migrate run`)
  or the committed offline metadata in `.sqlx/` (`SQLX_OFFLINE=true`, which
  the Dockerfile sets). Change a query and run `make sqlx-prepare`, or the
  Docker build fails on stale metadata while the local build — which still
  has a database to check against — passes.
- `i128` fits no Postgres integer type. Amounts and health factors cross the
  boundary as decimal text: bound as `$n::text::numeric` going in, read back
  through `::text` coming out. A query that binds one as a number instead is
  a rounding bug waiting to happen.
- Store tests need a live Postgres and are not skipped without one:
  `#[sqlx::test]` creates a database per test. `make db-up` first.
- **The local test suite needs capped concurrency.** At the default
  `cargo test` thread count this container's Postgres refuses connections
  (`PoolTimedOut`, `UnexpectedEof`) — every `#[sqlx::test]` opens a
  database of its own — and a timing-sensitive queue test fails with
  `Elapsed`. Three separate runs produced three different sets of
  spurious failures, all of them resource exhaustion rather than a real
  regression. `cargo test --lib --bins -- --test-threads=2` is green and
  takes about 25 seconds where the default thread count took 213 and
  failed. CI does not hit this — it runs on a clean machine at default
  concurrency — so it will be rediscovered by the next person who runs
  the suite locally rather than through `make check`. Separately, the
  lib **test** target needs a live `DATABASE_URL` even for a run that
  touches no database: `make sqlx-prepare` prepares `--lib --bins`, never
  `--tests`, so every `sqlx::query!`/`query_scalar!` reachable only from
  `#[cfg(test)]` code is absent from the committed `.sqlx/` offline
  cache. `SQLX_OFFLINE=true cargo test --lib --no-run` fails on 56 such
  queries across five files (`src/filler.rs` 19, `src/executor.rs` 14,
  `src/auctioneer.rs` 9, `src/store.rs` 8, `src/service.rs` 6) — not a
  handful in `store.rs` alone.
- A `users` row exists only while the account owes something — the tracker
  deletes it the moment its liabilities empty — so `count(*)` on `users` is
  the number of positions that could be liquidated, not the number of
  accounts ever seen.
- The migration in `migrations/` was amended twice during Phase 3, so a
  stale local database (one migrated before those amendments) fails loudly
  on a checksum mismatch rather than applying quietly. `make db-reset &&
  make db-up && sqlx migrate run` clears it.
- `docker-compose.yml`'s `DATABASE_URL` — set in the `liquidator` service's
  `environment:` block, which overrides `env_file:` on purpose, so the
  bot reaches Postgres by its compose service name rather than the
  host-side `127.0.0.1` in `.env` — is a local development credential, not
  a deployment one: `docker compose config` renders `environment:` in
  full, which is exactly what this file's own secrets convention above
  means by naming that command. A real deployment passes `DATABASE_URL`
  through the environment, never through a committed file. Nothing there
  percent-encodes `POSTGRES_PASSWORD` either, so a password containing
  `@`, `:` or `/` produces a malformed URL.
- `Submitter::prepare` **signs unconditionally**, and when a simulation
  reports an archived footprint it signs and sends a `RestoreFootprint`
  transaction of its own before it ever returns — simulating *through*
  `prepare` is a submission. `Submitter::simulate_only` is the only path
  that builds unsigned, never restores and never calls `sendTransaction`,
  which is what makes it the one a dry-run — or anything judging a
  candidate before committing to it — may call. This was a Critical in
  Phase 4: `Auctioneer::act`'s percent-adjustment loop must simulate
  through `simulate_only`, never `prepare`, or a dry-run signs and spends a
  restore's sequence number while claiming to have done nothing. It binds
  `Executor::execute` identically, which is why an archived footprint is a
  refusal in dry-run and only proceeds when armed, behind the queue.
- The contract's own auction bounds, which `Auctioneer::act` adjusts the
  percent against rather than predicting: a post-liquidation health factor
  **above** `1_1500000` is `InvalidLiqTooLarge` (error code `1213`), and
  below `1_0300000` is `InvalidLiqTooSmall` (`1214`, raised only for a
  partial liquidation). Both comparisons are strict (`is_hf_over` is `>`,
  `is_hf_under` is `<`), so the contract's own accepted window is the
  *closed* interval `[1.03, 1.15]`, and the adjustment loop reads it that
  way — raising the percent on `1214`, lowering it on `1213`, treating
  neither endpoint as a rejection. `TARGET_HF` itself is narrower at the
  top on purpose, refusing exactly `1.15`: aiming a liquidation at the
  contract's own ceiling leaves no room for the drift between planning and
  fill, and `target_health_factor` in `src/config.rs` says plainly that the
  upper bound is this bot's own margin, never the contract's rule. `TARGET_HF`'s
  default of `1.06` sits inside both bands with room for a ledger or two of
  drift before the auction is filled.
- `PoolSnapshot::position_data` accrues a **clone** of `self.reserves`
  before valuing a position, so `snapshot.reserves` itself is never
  accrued and stays exactly as read. Anything that values positions
  alongside a snapshot — the auctioneer included — must accrue to the same
  close time `position_data` used, or the two describe different ledgers
  and their numbers will not agree.
- A borrower's `recheck_ledger` flag must be **moved forward**, never left
  alone, when a pass cannot decide or act on it — or when its submission
  failed, expired or was lost on chain, or an armed pass recorded it but
  held it back (the startup delay, or no key) — re-raised one ledger *past* the
  current tick's (or at the flag's own, if that is already newer). One past,
  not at: the tracker raises flags at the very tick the pass runs on, so
  re-raising at the tick would leave the row exactly where it was.
  `Store::users_needing_recheck` orders `recheck_ledger ASC, account ASC`,
  so an untouched flag stays the oldest in its pool and comes back at the
  head of every following batch: one borrower nothing can decide (an
  oracle that stops pricing a reserve breaks `position_data` for everyone
  holding it) would otherwise starve every other borrower behind it,
  forever. This was Phase 4's second Critical; see `recheck_batch` and
  `move_flag_forward` in `src/service.rs`.
- The filler never writes an `auctions` row's `bid`, `lot` or
  `start_ledger`. Those three are the tracker's, from the pool's own events
  — a partial fill re-reads the remainder from chain rather than
  subtracting a side — and a filler that wrote them would be asserting an
  auction state no event ever reported. `Store::set_fill_plan` is the only
  write the filler makes to that table, and it writes `fill_ledger` and
  `percent` and nothing else. What a plan is made *against* is the chain's
  auction entry, re-read every time, never the row.
- An auction's 400th ledger is the end of its ramp: from there the bid is
  not merely zero, it is **absent** — the contract never stores a zero
  amount — so a fill from there on takes the whole lot and assumes no
  liability at all. There is no fill cutoff anywhere: `fill_auction`
  guards only the auction type and `user == filler`, so a fill at 400, 500
  or 5,000 is equally valid for as long as the entry exists, and
  `plan_fill` has no refusal for it — `FillSkip::PastAuctionEnd` does not
  exist. Which ledger a fill aims at *before* 400 is
  `PoolConfig::fill_objective` (`math::fill::FillObjective`): `FreeFill`,
  the default, aims at `RAMP_END_BLOCKS` (400) itself, the most the
  auction can pay and the last ledger to get it; `EarliestProfitable` aims
  at `fill_delay`'s answer instead, the earliest ledger the lot covers the
  bid plus the pool's margin, trading profit for actually landing where
  competition for the auction is real. `force_fill` means one thing only
  now — there is no "fill past the end" left to mean, since there is no
  end — capping whichever ledger the objective picked at
  `FORCE_FILL_MAX_DELAY` (350) however little the lot covers by then; the
  margin still decides *when* apart from that cap, and the health floor
  still decides *whether*. Past the 500th ledger `delete_stale_auction`
  becomes callable by anyone — permissionless, and it deletes nothing by
  itself — so a plan aimed past it is racing a deletion rather than only
  another filler: `Filler::execute_once` (`STALE_AUCTION_BLOCKS` in
  `src/filler.rs`) warns rather than refuses.
- `WITHDRAW_ALL` is `i64::MAX`, and that is the safe spelling of "all",
  not a saturation. `WithdrawCollateral` burns `min(to_b_token_up(amount),
  position)` and recomputes `tokens_out` from the cap, so any amount above
  the position withdraws exactly the position; and
  `to_b_token_up(i64::MAX)` is `9.22e18 × 1e12 / b_rate`, far inside
  `i128`, so the contract's own arithmetic cannot overflow on it either.
- A dry-run fill is recorded once per version of an auction the chain
  held, keyed by pool, account, start ledger **and the amounts the chain
  held** (`RecordedFill` in `FillerState::recorded_dry_run`) — not once
  per tick for as long as nobody else fills it, which is what a dry run
  would otherwise do to the `fills` table. The start ledger is in the key
  so a *new* auction for the same account is a new fill; the amounts are
  in it so a *changed* one is too: a partial fill by someone else keeps the
  start ledger and leaves a remainder, which is what an armed filler would
  now fill. The amounts, not a ledger: the filler re-reads the chain's
  entry, which can hold that remainder a tick before the tracker rewrites
  the row with it, and keying on the row's `updated_ledger` or on the
  ledger the entry was read at would record the same remainder twice. The
  row's own amounts are the cheap test before any chain read; the entry's
  are the definitive one before any plan. `prune_recorded` drops every
  version of an auction the pool's open rows no longer name, so the set
  cannot grow without bound. A restart may record one more row, and that
  is the whole cost of keeping it in memory.
- `DRY_RUN=false` with no `FILLER_SECRET_KEY` is a startup error, and so is
  `AUCTIONEER_SECRET_KEY` equal to `FILLER_SECRET_KEY`. The filler signs
  with its own key only — never the auctioneer's — so an armed bot without
  it would create auctions and never fill one; and two roles on one key
  would want two queues on one key, the sequence race `queue.rs` exists to
  make unreachable. Sharing a key is spelled by leaving
  `AUCTIONEER_SECRET_KEY` unset: the auctioneer then falls back to the
  filler's key and both roles submit through the one queue that key needs.
- The startup unwind pass trims any primary collateral above
  `min_primary_collateral` to the wallet, even when no fill triggered it —
  spec §5 step 2's floor applies on the run's first pass over every pool,
  by design (spec §1's capital model is "unwind to the wallet and hold").
  Set `min_primary_collateral` to exactly what you mean the bot to keep
  supplied in the pool: anything above it goes to the wallet on the first
  pass allowed to submit — armed, and past `STARTUP_DELAY_LEDGERS`.
- An unwind's repay is capped at what the wallet can spend, never the raw
  balance: `Inventory::available`'s figure, net of `XLM_FEE_RESERVE` and
  every open `Reservation` — the same rule a fill's repay uses. The
  contract refunds whatever a repay overshoots the debt by, but the wallet
  must hold all of it up front.
- `min_health_factor` plus `HEALTH_MARGIN_BPS` is where an unwind *rests*,
  and the pool's `min_collateral` binds it as well
  (`UnwindTerms::{min_health_factor, min_collateral}` in `math::unwind`).
  The margin is not just a stop condition for starting another candidate:
  a plan resting exactly on the operator's minimum is carried under it by
  the next ledger's interest on the debt it left, and the pass that would
  look again has gone idle and cleared the pool. `min_collateral` is the
  contract's, checked after every health-checked request of a position
  that keeps liabilities — the committed mainnet pool sets it to $5, which
  is above the health target in exactly the leftover-debt case this phase
  exists for. `HF_SAFETY_MULTIPLIER` only widens the *fill's* floor
  (`health_floor` in `math::fill`) and plays no part in an unwind's
  projection.
- A notification failure never affects trading: `Notifier::notify` answers
  a `Delivery`, never a `Result`, so a channel outage cannot hold up or
  fail a liquidation, a fill or an unwind — it can only mean the operator
  hears about one later than intended.
- The Telegram bot token sits in the request *path*
  (`/bot<TOKEN>/sendMessage`), not a header or the body, so every
  `reqwest::Error` this crate displays anywhere near the Telegram client
  goes through `reqwest::Error::without_url()` first. A new log line that
  prints a raw `reqwest::Error` from `notifier::telegram` or
  `service::telegram_channel`/`verify_telegram` leaks the token into the
  log. The one leak this crate cannot rule out is `RUST_LOG=trace`:
  logging there, dependencies' included, is not audited for secrets, and
  a dependency's request-level logging could print the request line —
  `/bot<TOKEN>/sendMessage` — which `tracing_subscriber`'s `log` bridge
  would capture. The default filter and `debug` are both clear; **a
  Telegram-configured bot is never run at TRACE.**
- `PORT` wins over `HTTP_PORT` when both are set, because `PORT` is the
  one a deployment platform controls (Cloud Run injects it); either alone
  turns the HTTP server on, neither leaves it off. A bind failure never
  stops trading: `http::serve` logs it and returns `()` — it is the task
  `Service::run` wraps it in that answers `Ok(())` — because a
  diagnostics port that cannot open must not stop the poller, the
  auctioneer or the filler from running. That task is also the one thing
  spawned *before* the initial seed: a seed of a busy pool is tens of
  seconds, and a restart probe aimed at `/livez` must be able to reach
  it.
- `/livez` includes the poller's own backoff in its window
  (`PollerConfig::liveness_deadline` is `LIVENESS_INTERVALS` poll
  intervals plus `max_backoff`), on purpose — an RPC outage the poller's
  backoff is already riding out must not also fail liveness. A restart
  probe must target `/livez`, never `/healthz`: restarting on every
  readiness blip would kill and respawn the process on exactly the
  outages its backoff exists to ride out, while a wedged poller — which
  `/livez` alone catches — is precisely what a restart can fix. The
  Docker `HEALTHCHECK` stays `pgrep` first because the HTTP server is off
  unless `PORT`/`HTTP_PORT` is set, so a check against `/livez` would mark
  every default container unhealthy — and is never wired to a readiness
  endpoint either, for the reason above; see the Dockerfile's own comment. A
  pool that has never heartbeated at all is measured from the run's start
  rather than reported dead, so the initial seed is not a restart loop;
  `/healthz` carries the mirror-image rule, failing once no chain head has
  been read for that same window, because an RPC outage freezes the lag it
  would otherwise be judged by — and its lag bound is symmetric for the same
  kind of reason, a head more than `max_lag_ledgers` *behind* the processed
  ledger being a lagging node rather than a bot at chain head.
- A `Notifier` must be used from inside a tokio runtime: `notify` spawns
  the delivery task, and calling it outside one panics.
- `notifications_total{kind,delivery}` is the one metric whose label set
  is `NotificationKind::as_str` rather than a `metrics`-local enum, which
  is why `NotificationKind` is closed (see `src/notifier.rs`'s doc): a new
  variant there is also a new metric label, never added quietly.
- The sandbox is the only thing in this repository that ever generates a
  signing key, and every one of them is for a network that exists for the
  length of a run. `deploy.sh` generates and friendbot-funds four
  `stellar keys` identities (`sandbox-issuer`, `sandbox-admin`,
  `sandbox-borrower`, `sandbox-filler` — the names live in `lib.sh`
  because `crash.sh` has to sign as the admin and only `deploy.sh` knows
  what it generated), then asserts through Horizon that each account
  exists: `stellar keys generate --fund` exits `0` whether or not
  friendbot answered, so without that check an unfunded account first
  surfaces two steps later as "account not found" under the wrong step's
  name. Only the filler's secret leaves the keystore — read once, in the
  last step, straight into `target/sandbox/sandbox.env` (mode 0600, under
  the git-ignored `target/`), never echoed, never logged, never an
  argument — and `down.sh` deletes that file, because a signing key for a
  network that no longer exists is a live-looking path to nothing. The
  identities themselves stay in the operator's
  `~/.config/stellar/identity/` under the `sandbox-` prefix, and the next
  `deploy.sh` overwrites them.
- **No sandbox script talks to a node it has not proved is the standalone
  network.** `require_standalone_network` — the RPC's own `getNetwork`
  answering `versions.env`'s `SANDBOX_PASSPHRASE`, `Standalone Network ;
  February 2017` — is the first thing `up.sh`, `deploy.sh`, `crash.sh`
  and `mint.sh` say to one, and `tests/liquidation_sandbox.rs` makes the
  same check on the Rust side, against `sandbox.env`, before it creates a
  database — and again, through `spawn_bot` and `run_check_config`,
  immediately before every binary it spawns, since a scenario's later
  bots start minutes after its first check. The answer has to come from
  the node, never from configuration: this is the one place the bot runs
  armed, and the only thing that makes that safe is what network it is
  pointed at.

  **And the URL it verified is the URL every `stellar` call is handed.**
  A named network is not: the CLI resolves an ad-hoc network from
  `STELLAR_RPC_URL` and `STELLAR_NETWORK_PASSPHRASE` *ahead of* an
  explicit `--network`, so an operator with that pair exported — they are
  the CLI's own documented variables — would have had the gate confirm
  localhost while `keys generate --fund`, every deploy and every invoke
  went to a public network, and `STELLAR_SIGN_WITH_KEY` was unopposed
  altogether. So `lib.sh` unsets those variables at source time, and
  `require_standalone_network` ends by exporting `SANDBOX_RPC_URL` and
  building `sandbox_network_args` (`--rpc-url … --network-passphrase …`),
  which every call passes and which beats the environment.
  `scripts/sandbox/test-network-pinning.sh` is the guard: it derives a
  contract id — the network passphrase is mixed into it — through the
  same array in a clean environment and in a polluted one, and requires
  the two to be equal. Nothing here may go back to `--network`.
- The backstop and the pool factory each name the other — the backstop's
  constructor takes the factory (it asks `is_pool` before accepting a
  deposit) and the factory's takes the backstop — so one address must be
  known before it exists. `deploy.sh` predicts the factory's with
  `stellar contract id wasm --salt … --source-account …`, which derives
  the id from the account, the salt and the network passphrase and *not*
  from the wasm; deploys the backstop against the prediction; deploys the
  factory with that same salt; and dies unless the deployed id is the
  predicted one. That last check is what makes the prediction safe — a
  mismatch means a backstop trusting a factory that does not exist, which
  would refuse every deposit. The salts are fixed literals rather than
  random on purpose, and are still unique per run, because the id takes
  the deploying account too and every run generates fresh keys.
- The emitter is not deployed at all; the admin's address stands in for
  it. It matters only to BLND emissions, which this sandbox never starts,
  and the backstop never calls the address it is given unless
  `drop()`/`distribute()` is invoked. A scenario that starts emissions
  would need the real one.
- **The pool leaves Setup through `set_status(0)`, not `update_status()`.**
  The contract's `execute_update_pool_status` panics with
  `StatusNotAllowed` (1204) whenever the current status is 6 (Setup) —
  verified against this exact wasm — so `update_status` cannot be the way
  out of Setup at all. `execute_set_pool_status(0)` reaches 0 (Admin
  Active) in one call and enforces the identical condition, panicking with
  the same 1204 unless the backstop threshold is met and queued
  withdrawals are under 50%; the `get_config` read straight after it is
  what proves the threshold *was* met rather than the call having been a
  no-op. Reserves are configured before it for a related reason:
  `queue_set_reserve`/`set_reserve` impose a timelock outside Setup. The
  pool it leaves behind reports status 0, and the bot treats that as
  active: `Service::validate` warns only past `AdminActive`, because the
  contract's `Pool::require_action_allowed` refuses borrow and cancel only
  above status 1 and supply only above 3, so 0 and 1 are the same thing to
  everything the bot does — and 0 is where any admin-activated pool sits,
  a new mainnet pool included. `deploy.sh` accordingly accepts 0 or 1.
- A fresh standalone network has no contracts at all, the native asset's
  included, and none of the plumbing a real network's accounts already
  carry. `deploy.sh` deploys the XLM SAC like any other
  (`stellar contract asset deploy --asset native`) or every XLM read
  answers "Contract not found"; it opens `change-trust` trustlines for the
  admin, the filler and the borrower, because a classic asset's SAC mints
  into a trustline and refuses an account without one (the borrower is
  never minted to — the pool pays it the USDC it borrows, which needs the
  trustline just the same); and it has the admin `approve` Comet for both
  tokens before `join_pool`, because Comet moves the caller's tokens with
  `transfer_from`, which checks a real allowance even where the
  transaction's own source account already satisfies the auth.
- The scenario is exactly one price move wide, and the numbers are load
  bearing. The borrower supplies 5,000 XLM of collateral at $0.10 and
  borrows 300 USDC: XLM's `c_factor` of 0.75 values the collateral at
  $375, USDC's `l_factor` of 0.95 values the debt at ~$315.8, and the
  health factor is ~1.19 — nothing to liquidate, `LIQ_HF_THRESHOLD` being
  0.998. `crash.sh`'s default price of `750000` (the oracle reports 7
  decimals, so $0.075) takes the collateral to $281.25 and the health
  factor to ~0.89. It re-sends USDC's unchanged price alongside it because
  `set_price_stable` takes the whole vector positionally, in the `assets`
  order `set_data` was given (`[XLM, USDC]`): there is no way to set one
  price, and a caller that passed only XLM's would silently unprice USDC.
- The sandbox test's fill budget is **measured**, never assumed. The fill
  waits on the auction's own ledger ramp — the lot ramps to full over the
  first 200 ledgers while the bid stays whole, so for this scenario — the
  numbers are a real run's: an auction created at 69%, a 207 USDC bid
  against a full lot of ~3,139 XLM worth ~$235 at the crashed price, plus
  the pool's 100 bps margin — the earliest profitable ledger is 178 in,
  and `FILL_LEDGERS` is 190, that with room for a re-plan. So
  `fill_budget` samples the chain head (`RpcClient::latest_ledger`, the
  RPC's `getLatestLedger`) for five seconds immediately before the wait,
  derives `FILL_LEDGERS × seconds per ledger` plus a minute, and fails *up
  front*, naming the rate it measured, when that exceeds the ten-minute
  cap. A constant written around "quickstart closes a ledger a second"
  becomes a flake the day it does not, and it fails as "the filler never
  filled" — a true statement about the wrong thing.
- Each sandbox scenario creates its own databases per run on the
  `DATABASE_URL` server — `sandbox_<unix seconds>`, or `sandbox_<unix
  seconds>_1` and `_2` for `restart_adopt`'s two bots, the suffixes
  literals the test writes — and migrates them before any bot starts, so
  a rerun never reads a previous run's rows, and a query error in an
  assertion is a real failure rather than "the table may not exist yet".
  `check_config` is the exception: its one database is created and **not**
  migrated, because what it asserts is that `check-config` never migrates
  it, and its case (f) points at a `sandbox_<unix seconds>_absent` that is
  never created at all. A run that passes drops what it created. A run
  that **fails keeps it**, because it is then the only durable record of
  what the bot decided, and every failure says so. Each name is appended
  to `target/sandbox/run-databases` the moment its `CREATE` lands and the
  line removed when the run's own drop succeeds, which is how `make
  sandbox-down` knows what to sweep:
  `sqlx database drop` drops only a name it is handed, nothing in sqlx-cli
  lists databases, and `psql` is in neither CI nor the dev container. That
  file is the one thing under `target/sandbox/` besides the wasm that
  `down.sh` must not delete. The sweep hands the URL to `sqlx` through
  `DATABASE_URL` in the environment rather than `-D` on argv — it carries
  a password, and `/proc/<pid>/cmdline` is world-readable — and checks for
  `sqlx` once before it starts, so a missing tool is named once instead of
  reported as "could not drop" per entry. `sqlx-cli` is installed by
  `ci.yml` and by `post-create.sh`, and the two must be the same version —
  they share the committed `.sqlx/` metadata, so a mismatch surfaces as a
  rejected query rather than as a tool disagreement.
  `post-create.sh` reads `versions.env`'s `SQLX_CLI_VERSION`; `ci.yml`, the
  PR gate, names its version as a literal on its own install line; and
  `check-repo-invariants.sh` fails unless that literal equals
  `SQLX_CLI_VERSION` — cross-file equality, the shape the three-way Rust
  pin already uses. Bump both together.
- **Nothing checks `NETWORK_PASSPHRASE` against the network it is pointed
  at.** There is no `getNetwork` call anywhere in `src/`: the passphrase
  is taken as given. What a wrong one does depends on whether a filler key
  is configured. With none — a keyless dry-run configuration —
  `check-config` passes (exit `0`), because nothing it reads depends on
  the passphrase. With one, in either mode, it fails, but not by name:
  `SigningContext::from_config` derives the native asset's contract id
  from the passphrase, so `validate_filler`'s native-balance read
  simulates a call on a contract the node does not hold, and that read
  propagates with `?` as a `LiquidatorError::Chain` —
  `chain: simulation failed: …`, never mentioning the passphrase.
  `check-config` exits `2` on it, as it does on any error (`src/main.rs`);
  `run` shares `validate_filler`, so any bot with a filler key — every
  armed one, since `DRY_RUN=false` requires it — fails at startup instead,
  exiting `1` because a chain error is not a `Config` one, before it seeds
  a pool, starts a poller or submits anything. The sandbox's
  `check_config` scenario pins both halves: case (g), the real key with
  the testnet passphrase in dry run, exits `2` naming `chain: simulation
  failed`; case (h), the same passphrase keyless, exits `0`. Nothing here
  closes the gap.
- Testnet is wiped 2 to 4 times a year, contracts and accounts alike —
  Stellar's own policy, not something this repository can prevent or
  detect. Anything `scripts/testnet/deploy.sh` stood up is disposable,
  and every address in `docs/testnet-soak.md` is an example of the shape
  an address takes, never a fact that outlives a reset: after one,
  `deploy.sh` rebuilds stage 2 from scratch (remove
  `target/testnet/testnet.env` first — it still names the now-gone pool)
  and stage 1's `pools.toml` needs its addresses re-derived from Blend's
  own current `blend-utils/testnet.contracts.json`.
- The native asset's Stellar Asset Contract already exists on testnet —
  every network but a brand-new standalone one carries it — so
  `scripts/sandbox/deploy.sh`'s own `stellar contract asset deploy
  --asset native` would fail there. `scripts/testnet/deploy.sh`'s
  `native_asset_id` (`scripts/testnet/deploy.sh:190-198`) takes the id
  instead from `stellar contract id asset --asset native`, which derives
  it purely from the network passphrase, no source account or
  transaction involved.
- Discovery on a pool this repository does not control is bounded by
  what the RPC's `getEvents` still retains, not an index of every
  position the pool has ever held — testnet has no analytics API for
  `SEED_URL` to enumerate positions from the way mainnet's does.
  On 2026-09-23 `examples/scan_borrowers.rs` found 9 accounts in Blend's
  testnet pool's last 24 hours and 15 in its last 7 days, and none of
  either set held debt: those positions were taken long before the window
  and their owners had not acted since (`docs/testnet-soak.md`, "Seeding
  it with `scan_borrowers`"). So a dry run's tracked set on a pool like this
  is only what acts while it watches, plus whatever a scan like that
  found — which is the argument for `SEED_URL`'s mainnet-analytics-API
  default existing at all.
- The armed runner seeds its own borrower. `deploy.sh`'s borrower takes
  its position inside its own step 9, before the bot or its events
  cursor exists, so no event in the range the poller ever reads names
  it. `run-bot.sh --armed` writes `target/testnet/seed.armed.toml`
  naming it for exactly that reason (its `seed_file=` heredoc), and both
  modes export `SEED_URL=""` (next to the `POOLS_FILE` export)
  — never the default, which answers for mainnet's own analytics API and
  would seed mainnet accounts into a testnet pool.
- `users_tracked` is a full-scan gauge, not a live count. `full_scan`
  (`src/service.rs:1089`) is its only writer
  (`instruments.metrics.users_tracked(pool, user_count)` at
  `src/service.rs:1109`), run on `FULL_SCAN_LEDGERS`'s cadence — 1,200
  ledgers by default, about 100 minutes at testnet's ~5 s ledgers.
  Between scans the gauge can trail what `Store::count_users` would
  answer right now; `examples/soak_report.rs` reads the store directly
  for that reason rather than trusting `/metrics`.
- Testnet's ledgers close about every 5 seconds, not the sandbox's ~1
  second, so every wait budget written around the sandbox is roughly
  five times longer here: an auction's 400-ledger ramp (`RAMP_END_BLOCKS`,
  `src/math/auction.rs:18`) takes about 33 minutes rather than the
  sandbox's few, and `fill_objective = "earliest-profitable"` — this
  tier's own pool setting — lands around 15 minutes in rather than under
  one. `docs/testnet-soak.md`'s own "What to expect, and when" budgets a
  full hour for an armed pass end to end for this reason.

## The fork's gotchas

Verified against `Templar-Protocol/blend-contracts-v2` PR #3 at head
`54afdae`, side by side with stock at `v2.0.0`. The long form, with the
reasoning and the work each one implies, is
`docs/specs/2026-09-20-adr-0008-fork-semantics.md`; these are the traps.

- **A 100% fill can cut the filler's own health factor, and the plan now
  accounts for it.** On the fork a full fill runs
  `check_and_handle_user_bad_debt` over the borrower before
  `validate_submit` checks the filler's own health, and that path can
  destroy debt by *reducing the reserve's `b_rate`* — inside the filler's
  own transaction. `plan_fill` values a **full** fill (one the scaling
  leaves no remainder of) against the reserves
  `math::setoff::project_default` leaves behind rather than the pre-fill
  snapshot's own; every other conversion still runs on the reserves as
  read, since the contract performs those first. The projection is still
  an **upper bound**, not an equality — `FillInputs::borrower`'s doc has
  the reasoning — so a miss can still surface as `1205 InvalidHf`, and the
  half-percent re-plan is still the backstop for it. This is the single
  most important difference on this list; nothing else here can lose
  money, and it is no longer flying blind into it.
- **Only three events are new**: `debt_setoff`, `collateral_orphaned` and
  `orphan_settled`. `defaulted_debt` is **stock**, and this crate already
  decodes it correctly — do not "add" it. The topic shapes differ and the
  borrower is not in a fixed position: `collateral_orphaned` carries the
  user in topic 1 and the asset in topic 2, while the other two carry no
  user at all, so a decoder that assumes "asset is always topic 1" mis-reads
  it silently, both being addresses.
- **A `bad_debt` event on a fork pool means stock wasm is deployed.** The
  event is still declared and has zero call sites, so a fork pool can
  never emit one. `handle_message` (`src/service.rs`) raises
  `NotificationKind::StockWasmDetected` at `Severity::High` the moment one
  is seen, naming the pool, the account and the asset — a deployment
  alarm, never something to act on: the tracker still applies the event
  exactly as any other and the tick proceeds.
- **The bad-debt *auction* is dead; the `bad_debt` *call* is not.** Creating
  or filling any auction type other than `UserLiquidation` raises `1200`, so
  `AuctionType::BadDebt` and `Interest` can never be built. But
  `bad_debt(user)` is kept and rewired, and is now the only way to clear a
  defaulted borrower — `CreationKind::BadDebt` names that call and stays.
- **`del_auction` and the 500-block staleness rule are stock**, byte-identical
  and permissionless, refusing with `1200` until `block_dif` reaches 500.
  Nothing here is a fork invention, and the bot could always have used them.
- **The oracle must report exactly 7 decimals** or every priced call panics
  `1210`, and a future-dated price is now rejected as well as one over 24
  hours old. `PositionData.scalar` is therefore always `10^7` on a fork pool,
  which makes this crate's normalisation a no-op — but read the decimals
  anyway, because the codec is shared with stock-pinned fixtures.
- **`RequestType::Withdraw` (1) now health-checks** whenever the same user
  owes anything in that reserve, where stock had no such rule. **This bot
  never sends that request type** — it appears once in the crate, in
  `encode`'s discriminant-ordering test — so nothing it does changes. Both
  unwind actions and every fill request build `WithdrawCollateral` (3),
  `Repay`, `SupplyCollateral` or the fill itself, and `WithdrawCollateral`
  already forced the check on stock. Worth knowing so an unwind's 1205 is not
  misdiagnosed as this.
- **`1220 ExceededSupplyCap` is now sized against by the planner.**
  ADR-0008 seals each reserve's stress-priced supply cap at $25k and a
  pool's sum at $50k. `plan_fill`'s supply-escalation step bounds itself
  at `supply_headroom`'s answer — the exact largest amount the reserve's
  own `supply_cap` still has room for, found by binary search rather than
  `cap − total_supply()` (which cannot breach the cap at any rate, but
  understates the room by up to a stroop) — and answers
  `FillSkip::SupplyCapped`/`SkipLabel::SupplyCapped` when the cap, not the
  wallet, is what stopped it. The executor's own handling was already
  right and stays untouched: `refusal` maps every code but 1205 and 1224
  to `Refused`, which counts `SkipLabel::ContractError` and leaves the
  auction for the next tick without re-planning — a `1220` reaching the
  executor at all is now the unexpected case, not the routine one.
- **`flash_loan`, `update_pool` and `set_emissions_config` all panic `1200`**,
  as do six backstop emissions exports (ADR-0011), with their ABIs preserved.
  The bot calls none of them; `flash_loan` matters only as a closed door.
- **The pool contract's own address is a `Positions` holder now** — confiscated
  collateral lands there as `supply`, never collateral or liabilities. It
  cannot be liquidated (`1211`, stock), and both the auctioneer and the
  filler now treat it the same as one of the bot's own signing accounts:
  `Auctioneer::decide` filters it out (`is_own_account`) before the
  batch's snapshot is even read, and the filler's row filter drops it
  before any chain read of the auction entry — so a seed source, or an
  auction row, that names the pool costs nothing rather than a wasted
  read.
- **`gulp` is repurposed and effectively unreachable**: permissionless, always
  returns zero, and raises `1200` while the reserve has any outstanding debt —
  which is always, for a reserve anybody borrows from. Orphaned collateral is
  dead capital, and the bot deliberately does not chase it.
- **`bstop_rate` must be zero at initialize** (`1201` otherwise), which
  both `scripts/sandbox/deploy.sh` and `scripts/testnet/deploy.sh` will have
  to satisfy before either can stand a fork pool up: both pass
  `--backstop_take_rate 1000000` today, which is right for the stock wasm
  they pin and fails the fork's initialize at step 6.

## Workflow

1. Branch → PR against `main`.
2. CI must be green: fmt, clippy, unit tests, docs, `cargo-deny`, Docker build,
   invariants, shellcheck.
3. Unresolved review threads block the merge; there are no required approvals.
4. Releases are tags `vX.Y.Z`, which publish a GHCR image and a GitHub Release.

## Where things live

- `src/` — the crate (binary `liquidator`, lib root `src/liquidator.rs`).
- `pools.example.toml`, `seed.example.toml` — annotated examples of the
  `POOLS_FILE`/`POOLS_TOML` and `SEED_FILE` formats, referenced from
  `.env.example`.
- `scripts/` — repo-invariant and release preflight checks, review tooling,
  the build-job cap (`cargo-jobs.sh`, `cargo-jobs-config.sh`) and the
  sandbox tier (`sandbox/`).
- `tests/` — the fixtures the math is pinned against;
  `liquidation_sandbox.rs`, the sandbox tier's five `#[ignore]`d scenario
  tests; and `sandbox_harness/mod.rs`, the harness they share.
- `docs/` — `configuration.md` (every setting, its default and bound),
  `deploy.md` (the operator's guide from pulling the image to running it
  armed), `deployment-contract.md` (what the image guarantees and what a
  deployment must provide), `architecture.md` (the one-sitting overview)
  and `testnet-soak.md` (the design spec's §9 soak: observe against
  Blend's own public-testnet pool, then armed against this repository's
  own throwaway deployment on the same network). Committed.
- `docs/specs/` — the design specs, the durable half of the documentation
  and the authority every plan argues from. Committed.
- `docs/plans/` — per-phase implementation plans. Working documents that go
  stale the moment their phase merges, so they are **gitignored**: kept on
  disk for the phase that is running, never committed.
- `docs/tmp/` — scratch: briefings and notes being worked through. Also
  **gitignored**, and never a source of truth for anything.
- `.github/workflows/` — CI, release automation and the nightly sandbox run.
