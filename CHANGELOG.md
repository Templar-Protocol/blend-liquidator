# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Four more sandbox scenarios alongside the original `liquidation` run —
  `check_config`, `dry_run`, `unwind_repay` and `restart_adopt` (see
  `tests/liquidation_sandbox.rs`'s module doc for what each proves) —
  sharing a new `tests/sandbox_harness/mod.rs` harness that factors out
  the standalone-network gate, the spawned bot, the per-run database(s)
  and every named wait. `deploy.sh` takes `SANDBOX_SCENARIO` to pick
  which of the five it deploys for (`liquidation` by default), and a new
  `scripts/sandbox/mint.sh` mints the filler more USDC on a running
  network — `unwind_repay`'s way of funding a wallet its own deploy left
  empty. `.github/workflows/sandbox.yml`'s nightly run now matrices over
  all five scenarios, one job per scenario on its own runner,
  `fail-fast: false`. `make sandbox-test` refuses a `SANDBOX_SCENARIO`
  that does not name exactly one test fn, rather than passing having run
  none, and `scripts/check-repo-invariants.sh` fails unless the five
  places that name the scenarios agree.
- The testnet soak tier: `scripts/testnet/{lib.sh,deploy.sh,crash.sh,
  run-bot.sh}` and the Makefile's `testnet-deploy`, `testnet-crash`,
  `testnet-run` and `testnet-run-armed` targets stand the same Blend v2
  protocol up on public Stellar testnet, funded by friendbot's XLM, and
  run the bot against it dry (`testnet-run`) or armed
  (`testnet-run-armed`) — the only place outside the sandbox tier this
  bot ever signs and sends a transaction with real, if testnet-only,
  consequences. `deploy.sh` is `scripts/sandbox/deploy.sh`'s own ten
  steps against testnet instead of a local container, taking the native
  asset's id from the Stellar Asset Contract testnet already carries
  rather than deploying one and ending by writing
  `target/testnet/testnet.env` (mode 0600) with the filler's secret key;
  `crash.sh` moves the deployment's own oracle price; `run-bot.sh
  [--armed]` is the one path `DRY_RUN=false` ever reaches the binary on
  testnet, regenerating its pools and seed files from `testnet.env` every
  armed run, handing the binary exactly the RPC URL its gate verified, and
  unsetting every other setting the bot reads first, so an operator's
  shell cannot give a testnet run a signing key, an RPC credential or a
  Telegram channel. Both tiers share one network gate:
  `scripts/sandbox/lib.sh`'s `require_network_passphrase URL EXPECTED
  LABEL`, with `require_standalone_network` one line on top of it and
  `scripts/testnet/lib.sh`'s `require_testnet_network` a second, pinned to
  testnet's passphrase. It refuses the public mainnet passphrase by name
  before any other comparison, and builds the flags every later `stellar`
  call carries from the URL it checked and the passphrase that node
  answered with. Two new read-only tools read the evidence back:
  `examples/scan_borrowers.rs` finds accounts worth tracking from a
  pool's own recent event history — the answer to a pool with no
  analytics API to seed from — and `examples/soak_report.rs` prints a
  pool's tracked-user count, open auctions, and every `creations`/`fills`
  row with its `dry_run` and `tx_hash`. `docs/testnet-soak.md` is the
  runbook for both stages.

### Fixed

- An oracle scan refused because the ledger moved between its reads
  (`ChainError::LedgerMoved`, after the snapshot's own three attempts)
  runs again on the next tick instead of waiting a whole
  `ORACLE_SCAN_LEDGERS` period — about 5 minutes at the default of 60,
  during which a price move the scan would have caught went unflagged.
  The testnet soak saw three such refusals in about three hours on the
  public RPC. Every other scan failure still waits a period, so an
  oracle or RPC that has stopped answering is not re-read every ledger.
- `scripts/sandbox/deploy.sh`'s `require_funded` re-requests friendbot
  funding every few seconds, on a widened 90s budget, while it polls
  Horizon for an account to exist. `up.sh`'s own health gate proves the
  RPC is answering and closing ledgers, not that friendbot behind it is
  ready to fund an account, and `stellar keys generate --fund` exits `0`
  whether or not its own funding request landed — so that race used to
  fail a deploy at step 1 after 30s with "friendbot did not fund …", on a
  network with nothing wrong with it but timing.

### Security

- An RPC call that fails at the transport level (DNS, TLS, a timeout, a
  refused connection) no longer renders the RPC URL: `ChainError`'s
  `From<reqwest::Error>` strips it with `reqwest::Error::without_url()`
  before the error is logged or, as `RpcFailing`, sent to the
  notification channel. `RPC_URL` must still carry no credential — it is
  an ordinary argument, and a debug print of the RPC client renders it.

## [0.1.0] - 2026-09-22

The first release: a Blend v2 liquidation bot targeting the ADR-0008
security fork of the Blend contracts (`Templar-Protocol/blend-contracts-v2`)
and also running against stock pools, dry-run by default. This being the
first release, the `Changed` entries below record how behaviour settled
during development rather than a difference from an earlier release.

### Added

- The ADR-0008 fork reconciliation (`docs/specs/2026-09-20-adr-0008-fork-semantics.md`
  §4). The bot now targets `Templar-Protocol/blend-contracts-v2`, whose pools
  destroy a defaulted borrower's debt by cutting the reserve's `b_rate`
  rather than handing it to the backstop.
- `math::setoff` (`src/math/setoff.rs`): the fork's default path, ported —
  the borrower's own supply in the debt reserve sets off what it can, then
  the rest is destroyed and every b-token holder in that reserve pays for it
  through a `b_rate` cut rounded up and floored at zero.
- Three fork events decoded (`src/chain/xdr/events.rs`): `debt_setoff`,
  `collateral_orphaned` and `orphan_settled`.
- `NotificationKind::StockWasmDetected`, raised at `Severity::High` whenever a
  `bad_debt` event is seen. The fork declares that event with no call sites,
  so one means the pool is running stock wasm. New series
  `notifications_total{kind="stock_wasm_detected"}`.
- A per-pool `fill_objective` key in the pools file: `free-fill` (the
  default) or `earliest-profitable`. Any other value is a startup error that
  names the two legal spellings.
- `FillSkip::SupplyCapped` and a new series `skips_total{reason="supply_capped"}`,
  for a fill whose own primary-asset supply the reserve's `supply_cap`
  stopped rather than the wallet.

- The sandbox integration tier (`scripts/sandbox/`,
  `tests/liquidation_sandbox.rs`, and the `sandbox*` make targets): a
  throwaway Stellar network from a digest-pinned `stellar/quickstart`
  image, Blend v2 deployed on it from release wasm pinned by SHA-256
  (pool, backstop, pool factory, Comet and a mock SEP-40 oracle, with
  `versions.env` holding every pin), two reserves, and a borrower left at
  a health factor of ~1.19 that `crash.sh` takes to ~0.89 with a 25% move
  in XLM's price. `make sandbox` is up → fetch → deploy → test → down in
  about five minutes; the test spawns the real binary **armed**
  (`DRY_RUN=false`, a signing key) against that network — the only place
  in this repository anything signs and sends a transaction — and asserts
  a `creations` row with a transaction hash, a `fills` row with one, the
  filler's own position left with no liabilities and its primary
  collateral back at `min_primary_collateral`, `/metrics` holding exactly
  one succeeded creation, exactly one succeeded fill and at least one
  unwind pass, and exit `0` on `SIGTERM`. It is `#[ignore]`d and refuses
  to start unless `target/sandbox/sandbox.env` exists and names the
  standalone network, the same check every script makes against the RPC's
  own `getNetwork` before it speaks to a node; the keys are generated per
  run, funded by friendbot, and the filler's secret reaches the bot
  through a mode-`0600` file under `target/` that the teardown deletes.
  The fill's budget is measured from the sandbox's own ledger close rate
  rather than assumed, and each run gets its own `sandbox_<unix seconds>`
  database — dropped when it passes, kept when it fails and listed in
  `target/sandbox/run-databases` for `make sandbox-down` to reclaim.
- The dev container gains the `stellar` CLI and a cgroup-aware build-job
  cap. The CLI is a checksum-verified release binary taken from the
  pins in `scripts/sandbox/versions.env` — the one file its version lives
  in, which `scripts/check-repo-invariants.sh` now enforces against both
  places that install it — with `libdbus-1-3` installed alongside it,
  since the binary links it at runtime and will not even report its own
  version without it. The cap is `scripts/cargo-jobs.sh`'s `min(nproc,
  max(1, memory_limit / 2 GiB))`, read from the cgroup rather than from
  `/proc/meminfo`, written once into `~/.cargo/config.toml`'s `[build]
  jobs` by `scripts/cargo-jobs-config.sh` — into an existing `[build]`
  table rather than a second one, since cargo refuses to parse a config
  that declares it twice, and never over a `jobs` key that is already
  there. An environment `CARGO_BUILD_JOBS` still wins at build time.
- `.github/workflows/sandbox.yml`: the tier run nightly and on
  `workflow_dispatch`, never on `push` or `pull_request`, and deliberately
  outside `CI Summary`'s needs list — that gate reads a skipped job as a
  failure, which is right for a workflow where nothing is conditional and
  wrong for one with no pull-request run to skip. One sandbox at a time
  (`concurrency`, queued rather than cancelled, since a cancelled run
  never reaches its teardown), the filler's key masked immediately after
  the deploy that writes it, `target/sandbox/*.log` uploaded by a path
  that cannot name the key rather than by a mask that artifacts do not
  honour, and the teardown on `always()`.
- Five knobs for the operational surface (`src/config.rs`): `PORT` and
  `HTTP_PORT` (`PORT` wins when both are set, since it is the one a
  deployment platform like Cloud Run controls), `HTTP_BIND_ADDR` (loopback
  by default — Cloud Run sets it to `0.0.0.0`, since it cannot route to a
  loopback listener), and `HEALTH_MAX_LAG_LEDGERS` (default 10, refused at
  zero: a bot exactly at head would report not-ready on every poll-interval
  boundary otherwise) — either port turns the HTTP server on; and
  `TELEGRAM_BOT_TOKEN` (read from the environment only, like the signing
  keys, never an argument) with `TELEGRAM_CHAT_ID`, both or neither, either
  alone a startup error. `HttpConfig` and `TelegramConfig` carry them into
  `ServiceConfig`.
- `src/metrics.rs` (`Metrics`): the run's counters and gauges — ledger head
  and processed, poller heartbeats, events processed, users tracked and
  auctions open per pool, creation and fill attempts by result (`Attempt`),
  skips by reason (`SkipLabel`, a closed five, each counted once per
  auction *per reason* rather than once per pass over one, so an auction
  the filler refuses on every tick cannot bury the other four reasons —
  keyed, for every skip decided after the chain read, by the auction
  entry's own start ledger rather than the store row's, which can lag it
  by a whole auction),
  estimated profit and estimated loss — two counters, as integers
  in the pool oracle's own units, because a landed fill's estimate can be
  negative on a `force_fill` pool and a Prometheus counter that decreases
  is read as a reset — reserved
  inventory per asset, unwind passes, the last successful scan per pool,
  and notification deliveries by kind and outcome (`DeliveryLabel`) — behind
  one `Mutex<Inner>`, synchronous and never held across an `.await`,
  rendered to Prometheus text exposition format (`Metrics::render`, prefix
  `blend_liquidator_`) with every closed label's series present, zero
  included, so a dashboard never has to guess whether a missing series
  means zero or means the bot has not run.
- `src/http.rs`: an axum server for `/healthz` (readiness — every
  configured pool's processed ledger within `HEALTH_MAX_LAG_LEDGERS` of the
  observed chain head *in either direction*, so a head that far behind the
  processed ledger — an RPC node sitting behind the cursor this bot has
  already committed — fails rather than reading as no lag at all, that head itself read within
  `PollerConfig::liveness_deadline`, and the store answering a ping
  inside `PING_TIMEOUT` (5s); the head's age is the rule that makes an
  RPC outage visible, since both ledger gauges are the process's own and
  an outage stops them together), `/livez`
  (liveness — every pool's poller heartbeated within
  `PollerConfig::liveness_deadline`, which absorbs one worst-case backoff
  so an RPC outage alone cannot fail it, with a pool that has never
  heartbeated measured from the run's start instead — the same rule the
  watchdog applies) and `/metrics`, served only when
  `HttpConfig` is set and never propagating a bind failure to its caller:
  a diagnostics port that cannot open must not stop the bot from trading.
  Its task is spawned before the initial seed, so `/livez` is reachable
  while a first start seeds, and the seed heartbeats every configured
  pool through `ledger::heartbeat_while` (which `LedgerPoller::await_ack`
  is built on) rather than leaving that window silent for the pools it
  has not reached.
- `Notifier` gains a bounded number of deliveries in flight
  (`NOTIFY_IN_FLIGHT`, 10) and `Notifier::drain(budget)`, which acquires
  every permit with a timeout and is what an exit path calls, with
  `DRAIN_BUDGET` (10s), to give sends still in flight a bounded chance to
  leave before the process does.
- `src/notifier/telegram.rs` (`TelegramChannel`): the second
  `NotificationChannel`, over `sendMessage`, with `verify` (`getMe`) for
  `check-config` to prove a configured token works before the bot starts
  trusting it. The bot token sits in the request path, so every
  `reqwest::Error` this channel returns is passed through
  `reqwest::Error::without_url()` first, and a refusal's text is built only
  from Telegram's own `description` field, never the raw response body,
  which echoes the request URL — token included — on some of Telegram's
  own error pages.
- `LedgerPoller` records a heartbeat every `poll_interval` for as long as
  its loop is turning, the pass included — the RPC calls, the `getEvents`
  paging and the wait for the tracker's acknowledgement alike — so the
  backoff sleep after a failed pass is the only unstamped stretch, and the
  chain head every pass that reads one; a run of `RPC_FAILING_AFTER` (5) consecutive failed passes
  notifies `NotificationKind::RpcFailing` once, at the threshold, with the
  first successful pass logging the recovery and resetting the count.
  `Service::run` spawns a watchdog task beside the pollers
  (`watchdog_loop`) that reports a pool whose heartbeat has gone past
  `PollerConfig::liveness_deadline` as `NotificationKind::PollerStalled`,
  since a poller that has stopped is exactly the thing that cannot report
  itself.
- `Service::run` now spawns seven kinds of task — one `LedgerPoller` per
  pool, one tracker, one auctioneer, one filler, one watchdog, one HTTP
  server when a port is configured, and one submission-queue worker per
  distinct signing key when armed, the HTTP server spawned before the
  seed pass and every other task after it — sharing one `Metrics` and one
  `Notifier` built before the seed pass (`build_notifier`: the Telegram
  channel when both credentials are configured, `LogChannel` otherwise);
  once the tasks are running, every exit but the second shutdown signal
  and a task panic drains the notifier (`finish_run`) before returning;
  nothing before them notifies, so an earlier startup failure has nothing
  in flight. A release build, the image's,
  sets `panic = "abort"`, so a panic there aborts the process on the spot;
  a debug build unwinds out of `drain_tasks` past `finish_run`. Either way
  notifications still in flight are lost with it. `Service::check_config`
  verifies a configured Telegram token with one `getMe` call, a refusal a
  configuration error, since spec §10 makes it the deploy smoke test.
- The filler and its executor now report themselves: `fills_total{result}`,
  `skips_total{reason}`, `estimated_profit_total`/`estimated_loss_total`,
  `reserved_inventory{asset}` and `unwind_passes_total` on every `Metrics`
  this run holds, and `NotificationKind::FillConfirmed`/`FillFailed`/
  `UnfundedFill` alongside the queue's existing `SubmissionDropped`.
- The unwind builder (`src/math/unwind.rs`, `plan_unwind`): the
  repay-and-withdraw request list for a position a fill has left the
  filler holding, in three steps. First, repay each liability asset the
  wallet holds — the debt in underlying plus a one-basis-point allowance
  plus one unit, capped at what the wallet can spend, the same rule
  `math::fill`'s repay uses. With no liability left, withdraw every
  collateral but the primary entirely and the primary down to
  `min_primary_collateral`; the contract health-checks nothing once no
  debt remains, so no projection is consulted. With liabilities left,
  withdraw candidate collateral — the ones that are also liabilities
  first, then the rest by ascending value, the primary last — only while
  the projection holds the two bounds the contract's `validate_submit`
  applies to a position that keeps liabilities. The health factor stays at
  or above `min_health_factor` plus `HEALTH_MARGIN_BPS` (50, 0.5%): the
  margin is where the withdrawal *rests*, not only where the walk stops
  starting candidates, because a plan resting exactly on the operator's
  minimum is carried under it by the next ledger's interest on the debt it
  left. And the effective collateral stays at or above the pool's own
  `min_collateral` (`UnwindTerms::min_collateral`), which binds wherever
  it is the larger of the two — the mainnet pools set it to $5, above the
  health target in exactly the leftover-debt case. `DUST_FLOOR_BPS` (100,
  1% of `min_primary_collateral`) is the smallest partial withdrawal of
  the primary worth sending, in the no-debt step as well as this one. Every
  withdrawal amount is found by formula and then verified by exact
  projection, backing off in bounded steps when the two disagree, so
  nothing reaches the plan unprojected. Pure: no I/O, nothing panics.
- The notifier (`src/notifier.rs`): `NotificationKind`, `Severity`,
  `Notification`, the `NotificationChannel` trait, `LogChannel`, and
  `Notifier`, which deduplicates by `(pool, account, kind)` with a
  cooldown — `FAILURE_NOTIFICATION_COOLDOWN_HOURS`, default 24 hours,
  refused at zero since there is no "no cooldown" spelling, only shorter
  ones — before handing what survives to one channel. `LogChannel`, the
  fallback channel and the only one when no Telegram credentials are
  configured, logs at `WARN` for `Severity::High` and `INFO` otherwise.
  A notification that survives the cooldown answers `Delivery::Queued`
  the moment its delivery task is spawned; a channel that then fails
  rolls the dedup entry back inside that task and writes the
  notification through `LogChannel` instead. None of it affects trading:
  `Notifier::notify` returns no `Result`, only a `Delivery`, and never
  awaits a channel.
- The filler now unwinds. `Executor::unwind` (`src/executor.rs`) is
  `Executor::execute`'s path for the requests `plan_unwind` builds: the
  same mode guards, judged through `Submitter::simulate_only`, submitted
  on the filler's queue with `UNWIND_RETRIES` (2, `src/queue.rs`) — but it
  writes no audit row (there is no unwind table; the `unwind
  planned`/`unwind submitted` log lines are the record) and never
  re-plans, since an unwind has no percent to lower. `Filler::tick`
  (`src/filler.rs`) runs one unwind pass per pool after its fill walk, as
  a seventh step: a fill that landed or may have (`Succeeded` or
  `Unknown`) makes its pool pending, and so does the run's very first
  tick, for every configured pool — a restart between a fill and its
  unwind must not strand the position, and that startup pass is also what
  trims any primary collateral above `min_primary_collateral` back to the
  wallet. The pass reads its own snapshot even for a pool the fill walk
  just read this same tick, plans through `plan_unwind`, and repeats every
  tick while it moves something; the first pass that builds no requests
  (`UnwindPlan::is_idle`) clears the pool. A pool whose fill — or whose own
  earlier unwind — landed, or may have, is passed over until a snapshot
  provably holds that submission: its ledger at or past the one the
  submission landed in, and never for an unresolved `Unknown`, which
  landed in no ledger anyone can name. The evidence is the run's, not the
  tick's (`FillerState::unwind_after`), since a snapshot two ticks later
  can still be behind it, and it is a high-water mark rather than a
  one-shot: the snapshot that proves it does not clear it, because
  `latestLedger` is not monotonic across calls and nearly everything past
  the gate can return having sent nothing while the pool stays pending.
  The next submission that lands raises it; only clearing the pool drops
  it. What the position happens to look like is not evidence either way. Leftover
  debt the wallet cannot repay notifies
  `NotificationKind::UnwindLeftovers` at `Severity::High` once per pool,
  not again until a later pass finds it clean. A pass that moves nothing —
  refused, stale, a submission that did not land, or a non-store executor
  failure — keeps its pool pending and backs the next one off by `2^n`
  ledgers up to `UNWIND_BACKOFF_MAX_LEDGERS` (64), and the pass whose run
  reaches `UNWIND_SETBACK_ALERT` (3) raises one
  `NotificationKind::SubmissionDropped` at `Severity::High` naming the
  cause; a pass that lands or finds the pool idle ends the run.
- The filler (`src/filler.rs`, `Filler`): once a tick, per configured pool,
  `tick` keeps only the open-auction rows worth a chain read — a user
  liquidation, none of the bot's own accounts, every asset accepted by the
  pool's `supported_bid`/`supported_lot` lists (`PoolConfig::supports`), not
  one this process has already recorded a dry-run fill for, and *due*: no
  plan yet, no plan this process made, within `REPLAN_NEAR_LEDGERS` of its
  planned fill ledger, or `REPLAN_LEDGERS` since it was last planned. Each
  kept row's on-chain auction entry is re-read before anything is planned,
  and the entry — never the stored row — is what is planned against, since
  a competitor's fill reaches the entry first; an entry that is gone closes
  the row. One snapshot per pool serves every auction in it, valued at the
  clamp the tracker and the auctioneer already share. The filler writes
  `fill_ledger` and `percent` onto a row and **nothing else**: its `bid`,
  `lot` and `start_ledger` are the tracker's, from the pool's own events,
  and a filler that wrote them would be asserting an auction state no event
  ever reported. One auction's failure is one auction's — everything but a
  `StoreError` is logged with its pool and account and the pass carries on
  — and a raised shutdown flag ends a tick between auctions, never inside a
  submission already waiting for its outcome.
- The filler's arithmetic (`src/math/fill.rs`), pure and panic-free like
  the rest of `math`: `fill_delay` answers in closed form — proved against
  the contract's own modifiers by `meets_margin`, at `d` and at `d − 1` —
  the fewest ledgers after an auction's start at which its lot covers its
  bid plus the pool's profit margin; `health_floor` is the pool's
  `min_health_factor` times `HF_SAFETY_MULTIPLIER`, rounded up; and
  `plan_fill` builds the health-bounded request list — the fill, a repay of
  each bid asset the wallet holds (with a one-basis-point allowance the
  contract refunds), a withdrawal of each zero-collateral-factor lot asset,
  a supply of the primary asset — by projecting the filler's own post-fill
  position exactly. When a projection is short it escalates in the spec's
  order: supply more of the primary where the pool's status permits it,
  else the largest lower percent that projects healthy, else the first
  later ledger that does. Candidates are searched exactly rather than
  estimated, and a skip is named (`Unprofitable`, `PastAuctionEnd`,
  `TooManyPositions`, `Unfunded`, `Health`) rather than silent.
- The executor (`src/executor.rs`, `Executor`): one planned fill, from the
  contract's judgment to the audit row, the submission and the settled
  reservation. The mode guards run before anything touches the chain — a
  dry-run executor handed a live reservation, a live one handed a dry-run
  settlement, or *any* queue offered to an executor that is dry-run or has
  no signer to judge with, all fail before anything is simulated, recorded
  or enqueued. Then the exact `submit` is simulated unsigned through
  `Submitter::simulate_only` (never `prepare`, which signs unconditionally
  and restores an archived footprint by sending a transaction of its own),
  the `fills` row is written *before* anything is submitted, the operation
  goes onto the filler's queue, and the transaction's hash is attached for
  every outcome — the failed, expired and unresolved included, each having
  consumed a sequence number worth naming. `InvalidHf` (1205) and
  `MinCollateralNotMet` (1224) answer a re-plan, which the filler makes
  exactly once at half the percent; `BadSequence` clears the plan rather
  than resending it; every other refusal is logged with its contract error
  code and left for the next tick. The wallet reservation is settled by
  value on every non-panicking path, including an error raised after the
  chain has already answered.
- The filler's inventory (`src/inventory.rs`): the balance last read per
  asset, the `XLM_FEE_RESERVE` withheld from the native asset so a plan
  never spends the wallet below its own fees, and a must-use `Reservation`
  a plan takes for what it will spend, consumed or released by value
  exactly once, carrying the manager that issued it and warning if it is
  ever dropped unsettled. Balances are re-read after a transaction that
  landed or may have landed, and at most every `INVENTORY_REFRESH_SECS`
  otherwise. It holds wallet balances only — a plan's positions come from
  its own snapshot, because positions valued against a different ledger's
  reserves are exactly the disagreement this bot's accrual clamp exists to
  prevent. Its arithmetic saturates, the one sanctioned exception to the
  crate's checked-arithmetic rule, and its doc says why.
- `src/queue.rs` now resolves every submission to a terminal outcome before
  it takes the next one for that key: an `Unknown` is polled by the hash
  the queue already holds until it is terminal, and a send whose *answer*
  was lost is resolved the same way rather than resent, because the RPC may
  already have forwarded the envelope. Preparing the next submission first
  would build it against a sequence number an in-flight transaction may
  still consume. Only a failure that provably sent nothing — a `prepare`
  that failed before any envelope left, or a send the RPC refused outright
  — is retried, within the budget its `Submission` carries: 3 for an
  auction creation, 10 for a fill, backing off from one second, doubling,
  to thirty, with shutdown cutting a backoff short. `QueueError::Chain` is
  narrowed to exactly those failures, which is what lets a caller release
  its wallet reservation on it.
- `RUN_MODE=loop` now runs the filler as a fifth kind of task, fed by the
  same `watch` the auctioneer is — never inside the tracker's
  acknowledgement path, whose cursor a fill must not be able to stall, and
  never a second reader of the poller channel. Each of the two holds its
  own startup gate, because each measures `STARTUP_DELAY_LEDGERS` from the
  first ledger it saw and each answers for its own key; before it elapses
  the filler plans and writes its plans but executes nothing, so an
  operator sees what the bot would do before it may do it. Submission
  queues are now spawned one per **distinct** signing key: the auctioneer
  shares the filler's whenever it falls back to the filler's key, since two
  queues on one key is the sequence race `queue.rs` exists to make
  unreachable.
- Migration `0003`: a `fills` table auditing every fill attempt the
  executor recorded, dry-run or not: a row is written before anything is
  submitted and the transaction's hash is attached — once, never replaced —
  when there is one. So `dry_run` records the mode the bot was configured
  in, `tx_hash` is the evidence that a transaction was named, and a row
  with `dry_run = false` and no attached hash is an armed attempt that was
  never submitted, or was submitted with its outcome unrecorded — the
  signing account's sequence number tells the two apart, not the row. A
  dry-run fill is recorded once per version of an auction the chain held —
  keyed by pool, account, start ledger and the amounts the chain held —
  rather than once per tick for as long as nobody else fills it; a partial
  fill by someone else leaves a remainder, and that remainder is recorded
  afresh, once, whether the filler first saw it from the chain or from the
  store.
- The auctioneer now adopts an auction the chain already holds and the
  store does not. An auction opened before this bot's events cursor
  produced no `NewAuction` for the tracker to apply, so the filler — which
  walks the store — could never have found it; when a creation's simulation
  answers `AuctionInProgress` (1212), the entry is read from chain and
  upserted as a row with no fill plan.
- New knobs: `HF_SAFETY_MULTIPLIER` (1.1; the pool's own
  `min_health_factor` times this is the floor a fill keeps the filler's
  position at or above — at least 1, refused under rather than clamped,
  since under one the floor would sit *below* the operator's stated
  minimum), `REPLAN_LEDGERS` (10) and `REPLAN_NEAR_LEDGERS` (5; zero is
  meaningful here — re-plan only at the fill ledger itself — while
  `REPLAN_LEDGERS=0` is refused, since it would re-plan every auction on
  every ledger), `XLM_FEE_RESERVE` (50, in decimal XLM: XLM has 7 decimals,
  so the parsed value is its value in stroops), `HIGH_FEE_PROFIT_THRESHOLD`
  (10, in the pool oracle's units, at or above which a fill pays the high
  fee tier) and `INVENTORY_REFRESH_SECS` (30, refused at zero, which would
  read every wallet balance on every tick). `FILLER_SECRET_KEY` is now
  **required** when `DRY_RUN=false` — the filler signs with its own key
  only, never the auctioneer's, so an armed bot without it would create
  auctions and never fill one — and `AUCTIONEER_SECRET_KEY` equal to it is
  refused at startup; leaving `AUCTIONEER_SECRET_KEY` unset is how one key
  signs both roles, through the one queue that key needs. A pool's
  `min_health_factor` must now be strictly above the contract's own
  post-submit minimum of 1.00001, or the filler would plan fills the
  contract refuses as `InvalidHf`.
- Startup validation, in `loop` and `check-config` alike, now also checks
  the filler's account: it must exist on the network and hold more of the
  native asset than `XLM_FEE_RESERVE`, and armed, holding less than a
  pool's `min_primary_collateral` is a warning. Armed, each of the first
  two is a startup error; in dry-run each is a warning, and no
  `FILLER_SECRET_KEY` at all warns that the filler plans against an empty
  inventory and simulates nothing.
- The auctioneer (`src/auctioneer.rs`, `Auctioneer`): `decide` reads one
  snapshot per batch of tracked users, values each at the later of the
  tick's close time and the newest reserve entry the snapshot holds — the
  clamp the tracker already applies, so the decision and the stored health
  factor cannot disagree and a snapshot the chain has moved past is valued
  rather than refused — and answers with a
  `Decision` per user: liquidate (via the new `math::liquidation`'s
  `plan_liquidation`, which selects the auction's bid and lot assets and
  the percent that closes the borrower's excess down to `TARGET_HF`,
  walking in more assets when the current selection cannot), move to bad
  debt, or skip with a named `SkipReason` (healthy, an auction already
  open, no plan closes the excess, the bot's own account, or no
  liabilities left). `decide` needs no signer at all. `act` turns a
  decision into an operation, lets the contract judge it by simulating
  unsigned through `Submitter::simulate_only` — never `prepare`, which
  signs unconditionally and, on an archived footprint, signs and sends a
  restore transaction of its own — adjusting the percent against the
  contract's own `InvalidLiqTooLarge`/`InvalidLiqTooSmall` refusals up to
  `PLAN_ITERATIONS` times, records every creation (the ones dry-run only
  simulated included) before it submits anything, and submits through a
  `SubmissionQueue` only when one is configured. `act` answers an
  `ActOutcome`, which distinguishes a borrower it *skipped* (nothing was
  owed) from one it was *refused* (something was owed and could not be
  done, whose submission failed, expired or was lost on chain, or that an
  armed bot recorded but held back — the startup delay still running, or
  no key to send with) — the caller clears the recheck flag only for the
  first, so a borrower the contract refused is retried on a later pass
  instead of being forgotten until the next full scan. A retry is re-flagged one ledger *past* the
  tick that could not act on it, so it sorts behind everything flagged on
  that tick rather than returning to the head of the next batch. `scan_oracle` is a third path
  that decides nothing itself: it compares a pool's current prices against
  a remembered reference and, via `Store::flag_exposed_to`, flags **every**
  borrower exposed to whichever asset moved past `PRICE_DELTA_BPS` for the
  ordinary recheck path to decide about — one unbounded statement per
  move, deliberately not sized by `REFRESH_BATCH`, because the reference
  re-anchors on the move it reports and a borrower one scan skipped would
  not be reached by the next one either.
- `src/queue.rs`'s `SubmissionQueue`: one ordered queue per signing key, so
  two tasks preparing a transaction for the same key cannot race to spend
  its sequence number and produce an unrecoverable `BadSequence` — the
  filler holds a second one for its own key whenever that key is not also
  the auctioneer's. An error from any
  service task now raises the shutdown flag and lets every other task
  return on its own rather than dropping the `JoinSet` and aborting them:
  aborting the queue between `sendTransaction` and the poll that learns
  the outcome would leave a key's sequence number consumed by a
  transaction the bot never saw the end of.
- `RUN_MODE=loop` now runs the auctioneer as its own task, fed by a
  `watch` the tracker publishes after it acknowledges a tick — never
  inside the acknowledgement path, whose cursor a decision must not be
  able to stall. Per tick it fires the oracle-scan
  (`ORACLE_SCAN_LEDGERS`) and full-scan-and-flag (`FULL_SCAN_LEDGERS`,
  reusing `SCAN_HF_THRESHOLD`) cadences when due, then decides and acts on
  every pool's currently flagged users. A borrower a pass cannot decide or
  act on has its recheck flag moved forward — re-raised one ledger past
  the current tick's, never left where it was — so one borrower nothing can
  decide (an unpriced reserve breaks every position holding it at once)
  cannot starve the rest of the queue behind it, which
  `Store::users_needing_recheck` orders oldest-flag-first. No submission
  is attempted until `STARTUP_DELAY_LEDGERS` ledgers have elapsed since the
  first ledger the auctioneer observed, dry-run or armed alike.
- New knobs: `LIQ_HF_THRESHOLD` (the health factor at or below which a
  borrower is liquidatable — below the contract's own strict `1.0` test,
  so the margin absorbs rounding and the interest accrued between planning
  and execution), `TARGET_HF` (the health factor a liquidation aims to
  leave the borrower at — refused at parse outside `[1.03, 1.15)`, since
  `TARGET_HF=0` would make every liquidatable borrower a silent "no plan"
  for ever; the floor is the contract's own `InvalidLiqTooSmall` bound,
  and the ceiling is this bot's own margin, one notch inside the
  contract's `InvalidLiqTooLarge` check, which accepts exactly `1.15`;
  `LIQ_HF_THRESHOLD` at or above `SCAN_HF_THRESHOLD` is refused for the
  mirror reason),
  `ORACLE_SCAN_LEDGERS`, `PRICE_DELTA_BPS` and `PLAN_ITERATIONS` (both
  refused at zero rather than clamped — a zero price delta flags every
  borrower on every scan forever, and zero plan iterations would simulate
  nothing and skip every liquidation silently), and
  `STARTUP_DELAY_LEDGERS` (a count of ledgers from the first the
  auctioneer sees, before which no submission is attempted).
  `AUCTIONEER_SECRET_KEY` — falling back to `FILLER_SECRET_KEY` — is the
  auctioneer's signing key, read from the environment only like every
  other secret; with neither set the auctioneer still decides and records,
  it just never signs, which is the ordinary dry-run deployment and not an
  error. Both keys are parsed when both are set, and **both** addresses go
  into the set the auctioneer refuses to act on — at most one of them
  signs, but a filler position this bot could liquidate is its own.
  `DATABASE_MAX_CONNECTIONS` now defaults to 10 rather than 5: the pool
  must cover every task that queries concurrently — one poller per pool,
  the tracker, the auctioneer and the filler — roughly `pools + 3`, and an
  acquire timeout surfaces as a fatal `StoreError`.
- Migration `0002`: a `creations` table auditing every auctioneer
  submission (the ones dry-run only simulated included, with a `tx_hash`
  only once one was actually sent), and a durable `users.recheck_ledger`
  flag — with a partial index — that the auctioneer's recheck queue reads
  oldest-flag-first.
- The Postgres store (`src/store.rs`): cursors per polling task, tracked
  borrowers (`users`, one row while an account owes something, deleted the
  moment it does not) and open auctions (`auctions`), migrated by embedded,
  compile-time-checked `migrations/` and queried through `sqlx::query!` so a
  schema drift is a build failure, not a runtime surprise. `i128` amounts and
  health factors cross the Postgres boundary as decimal text — bound
  `$n::text::numeric`, read back `::text` — since no Postgres integer holds
  one exactly; a `numeric` column that does not parse back to an `i128` is a
  `StoreError`, never a silently truncated zero.
- The per-pool ledger poller (`src/ledger.rs`, `LedgerPoller`): asks the RPC
  for chain head, pages every pool event since its stored cursor, sends each
  decoded event followed by the ledger's tick, and advances the cursor only
  once the tracker acknowledges that tick — the cursor means "applied", so a
  kill anywhere before that replays a ledger rather than skipping one, which
  the tracker tolerates because applying an event twice is idempotent. A
  cursor that has fallen out of the RPC's retained window is reported as a
  `Gap` and restarted at the window's edge, for the tracker to reseed, and
  at most once per stale cursor since each `Gap` costs a full reseed; a pass
  that cannot prove it drained its range — paging
  stalled, repeated, or hit a hard cap — leaves the cursor untouched instead
  of guessing.
- The tracker (`src/tracker.rs`, `Tracker`): `apply` writes an event's
  auction bookkeeping (a partial fill re-reads the remainder from chain
  rather than subtracting the filled side, since the contract owns that
  arithmetic) and returns the accounts it named; `refresh` re-reads named
  accounts from chain in one batched snapshot per tick and upserts or
  deletes their `users` row at the ledger it read them at, valuing them at
  the tick's own close time — or, when the chain has already moved past it,
  at the newest reserve entry the snapshot holds, since the contract only
  ever accrues forward from a stored entry; `refresh_stale` walks the
  oldest-updated rows first, below an absolute ledger cutoff the caller
  derives from `USER_REFRESH_LEDGERS`, so a long-idle borrower's accrued
  interest is never missed; `seed` collects accounts from
  every configured source, deduplicates them, and refreshes them in
  batches — a source that fails is a warning, not a startup failure, because
  every account is re-verified from chain before the bot ever acts on it,
  and an incomplete seed is retried on the next full scan.
  Seeding draws from the public Blend analytics API (`AnalyticsSeed`,
  walking its cursor-paginated positions endpoint) and/or a static TOML file
  of pool-to-account lists (`FileSeed`), both behind the uniform
  `SeedSource`.
- The pools file (`POOLS_FILE`/`POOLS_TOML`, parsed in `src/config.rs`): one
  `[[pools]]` table per pool naming its primary asset, supported bid/lot
  assets and profit rules, validated at startup — every pool must agree on
  one backstop (a filler's position is shared across every pool it follows),
  and every named asset must be one of the pool's own reserves. New knobs:
  `USER_REFRESH_LEDGERS`, `REFRESH_BATCH`, `FULL_SCAN_LEDGERS`,
  `SCAN_HF_THRESHOLD`, `SEED_URL` (empty disables the analytics source),
  `SEED_HF_MAX`, `SEED_FILE`, `DATABASE_URL`, `DATABASE_MAX_CONNECTIONS`,
  `POLL_INTERVAL_MS`. `pools.example.toml` and `seed.example.toml` document
  both file formats and are referenced from `.env.example`.
- `RUN_MODE`/`--run-mode` (`src/service.rs`, `Service`): `check-config`
  validates the configuration against the chain and the database — it
  connects and pings, never migrates — and reports the resolved (redacted)
  form, the per-pool validation and every warning, without following
  anything; `loop` (the default) additionally connects and
  migrates the store, seeds every pool that needs it, then runs one
  `LedgerPoller` per pool and one tracker task consuming their shared
  channel until `SIGTERM`/`SIGINT` and every task has returned — a second
  signal exits immediately with code 130 rather than wait further. Once a
  tick, the full scan reports the pool's tracked-user count and its least
  healthy borrowers, staggered across instances by a per-process random
  phase so several bots following the same pool do not scan in lockstep.
- A `postgres` service in `docker-compose.yml` and `Makefile` targets
  (`db-up`, `db-down`, `db-reset`, `db-migrate`, `sqlx-prepare`) for the
  local database `make check` and the store's tests now need.
- The pool contract's arithmetic, ported to checked `i128` (`src/math/`):
  reserve interest accrual, b/d-token conversions, effective position values
  and the health factor, and Dutch-auction scaling. Every rounding direction
  matches the contract's, because the bot's decisions are only as good as
  their agreement with what the contract computes at execution; nothing in
  the module panics, so an overflow is a skipped decision rather than a dead
  process.
- XDR codecs for everything the bot reads from a pool (`src/chain/xdr/`):
  ledger keys with their durability, the contract instance, reserve config
  and data, positions, auctions, SEP-40 prices, and the pool event
  catalogue. Written by hand rather than generated, so every shape the bot
  depends on is visible in one place — and pinned by
  `tests/fixtures/mainnet-fixed-v2.json`, a single-ledger mainnet snapshot
  that carries both the raw entries and the contract's own `get_reserve` and
  `get_positions` answers. Accruing the former must reproduce the latter to
  the stroop, so a contract upgrade that changes the maths fails a test
  instead of a fill.
- `PoolStatus` and `AuctionType` enums in `src/chain/xdr`, decoded through
  `TryFrom<u32>`, so a pool status or auction discriminant the bot does not
  recognise is a decode error rather than an opaque `u32` a caller could
  mishandle silently. `keys::auction` and every event carrying an auction
  type now take `AuctionType` directly.
- Strict topic-count checking in the pool event decoder
  (`src/chain/xdr/events.rs`): a modelled event with a surplus topic is now
  a shape error instead of silently ignoring the extra one, and
  `delete_auction` requires its data to be the `()` the contract actually
  publishes.
- A finite XDR read/write limit, `chain::xdr::encode::XDR_LIMITS`, in place
  of the previous unbounded `Limits::none()`: generous enough that no value
  the chain can produce is ever rejected, but no longer letting a hostile or
  broken RPC response make the decoder recurse or allocate without bound.
- `OraclePrices::new` now rejects any non-positive price. A zero or negative
  price would value collateral at nothing, which would make a healthy
  account look liquidatable; the fields are private so the check cannot be
  bypassed by a struct literal and `scalar` always equals `10^decimals`.
- `cargo run --example capture_fixture` refreshes that snapshot from a live
  RPC over `curl`, retrying until every entry and simulation describes one
  ledger.
- The chain layer (`src/chain/`): a hand-written Soroban JSON-RPC client
  (`rpc`) for the eight methods the bot uses, with every base64 XDR field
  decoded at the boundary and every result carrying its ledger — `events`'s
  `limit` is validated against the RPC's 1 to 10 000 range before a request
  is ever sent; pool reads (`pool`) that assemble one ledger's instance,
  reserves, oracle prices and positions into a `PoolSnapshot`, refuse a
  ledger that moved between reads, and retry a moved ledger across up to
  three attempts before giving up, since the pool is read in several round
  trips and a ledger closing mid-read is expected, not exceptional; a
  `FillPercent` newtype validates the contract's 1 to 100 fill-percent range
  once, for both `new_auction_op`'s `percent` and the `amount` the three
  fill requests carry, built through `Request::fill`, so an out-of-range
  value is refused before a request is even built; the network id and an
  Ed25519 `Signer` that renders as its address only (`signer`); and the one
  write path (`tx`): build with a five-minute time bound and a `LedgerWindow`
  of `[latest_ledger, latest_ledger + TX_POLL_LEDGERS + 1)` — a `try_new`-only
  newtype that can never be empty or inverted, which `Prepared` carries since
  its lower bound is what lets `Expired` be trusted — simulate, restore
  archived entries, assemble, fee from the p70/p90 inclusion percentiles
  floored at `BASE_FEE`/`HIGH_FEE`, sign, send with one `TRY_AGAIN_LATER`
  retry, poll, and classify into succeeded, failed with the pool's error
  code, expired, or unknown. A `NOT_FOUND` is only `Expired` when the RPC's
  retention still reaches back to the transaction's lower ledger bound; once
  retention has moved past it, a `NOT_FOUND` proves nothing, so the outcome
  stays `Unknown` for reconciliation by other means (the account's sequence
  number) instead of being called `Expired` on a guess. A `TxBadSeq` at send
  is its own error, `BadSequence`, because the plan behind such a
  transaction is stale and must be rebuilt. `send` also checks the hash a
  `Pending`/`Duplicate` `sendTransaction` response carries against the
  envelope it just sent, refusing a mismatch as `Shape` rather than trusting
  the RPC's echo blindly. `wait_for` polls to that same outcome from a bare
  hash, sequence and window — the fields an `Unknown` outcome carries — so a
  submission queue can resume a transaction after a restart or a send that
  timed out without resending it; `wait` is `wait_for` on the fields a
  `Prepared` already holds. Only a transient failure (a transport error, or
  an HTTP 429/5xx) is retried while polling; a permanent one — a JSON-RPC
  error object, a malformed response, or any other HTTP status — stops
  polling and is reported as `Unknown` rather than an `Err`, since the
  transaction may already have landed and only the chain can say what
  happened: `wait_for` never loses the handle while the transaction could
  still be in flight. A restore transaction whose own outcome comes back
  `Unknown` keeps its hash, sequence and window too, in
  `ChainError::RestoreUnknown`, instead of losing them inside `Restore`'s
  formatted string.
- Configuration for the chain: `NETWORK` or `NETWORK_PASSPHRASE`,
  `RPC_URL`, `RPC_API_KEY_HEADER` (an empty value counts as absent, the same
  as an unset one) with `RPC_API_KEY` read from the environment only (an
  empty value counts as absent here too), `BASE_FEE`, `HIGH_FEE`,
  `TX_POLL_LEDGERS` (minimum 1: the ledger bound is exclusive, so a zero
  window would make every transaction unlandable before it starts).
- `cargo run --example pool_snapshot` prints a live pool's reserves and its
  users' projected health factors through the real client.
- Every chain test drives the real client through a scripted localhost
  JSON-RPC server (`src/chain/script.rs`), covering the restore,
  `TRY_AGAIN_LATER`, timeout and decoded-error paths without a network.
- The runtime Docker image no longer installs `libssl3`: the binary is
  built against `rustls-tls-native-roots` and links no OpenSSL, so the
  package bought nothing.
- The repository itself: a Rust service scaffold for a Blend Protocol liquidation bot, green on its first commit. CI gates `cargo fmt`/`clippy -D warnings`/`test`/`doc -D warnings`, `cargo-deny`, the Docker build, `shellcheck` and the three-way Rust version pin behind one aggregate `CI Summary` check — the single required status check, so adding or renaming a job never needs a ruleset edit. `clippy::pedantic` is warn-level with `unwrap_used = "deny"` from the first commit, which is the cheap moment: retrofitting that onto an existing codebase is not. The dev container pins its base image by digest and its features by exact version, with a lock file CI verifies.
- `DRY_RUN` / `--dry-run`, defaulting to `true`, before there is anything to trade. The parser accepts only the literal strings `true` and `false`; `1`, `yes` and `on` are refused at startup. Every extra spelling is another way into live trading, and the dangerous direction is silent — a `DRY_RUN=yes` read as false would arm the bot while reading, to the operator, like it had been disarmed. This is the invariant most expensive to retrofit: a bot that defaults to live and is made safe-by-default later leaves every existing deployment silently changing behaviour on upgrade.
- `scripts/check-repo-invariants.sh` also gates a single `stellar-strkey`.
  `Cargo.toml` pins it to the version `stellar-xdr` depends on and
  Dependabot ignores it, since a bump of it alone only adds a second copy;
  the check fails unless `Cargo.lock` holds exactly one — two is the signal
  to bump it in the same PR as a `stellar-xdr` that moves its own.
- The documentation set: `docs/configuration.md` (every setting, its
  default and bound), `docs/deploy.md` (the operator's guide from
  pulling the image to running it armed), `docs/deployment-contract.md`
  (what the image guarantees and what a deployment must provide) and
  `docs/architecture.md` (the one-sitting overview), plus
  `the_configuration_documents_cover_exactly_the_real_settings`
  (`src/config.rs`), a test that fails unless the settings the code reads
  — every `clap` argument's `env` name and the six read directly — are
  exactly the set of `NAME=` lines in `.env.example` and exactly the set
  of first-column names in the reference's settings tables, no more and
  no fewer, printing the differences when they are not, and unless
  `pools.example.toml` parses.

### Changed

- **Fills now wait for the free-fill point by default, which moves an
  existing deployment's fills later on upgrade.** With no `fill_objective`
  set, a pool now aims at `start + 400`, where the scaled bid has decayed to
  nothing and the filler takes the whole lot without assuming any debt.
  Previously it aimed at the earliest ledger the lot covered the bid plus the
  pool's margin. Waiting pays more per fill and forfeits the auction to
  anyone who fills earlier; set `fill_objective = "earliest-profitable"` to
  keep the old timing. `force_fill` still caps the target at 350 ledgers
  into the auction; it is not a deadline, so an auction first seen later
  fills at the first ledger the bot can act in.
- There is no longer a 400-ledger fill cutoff. `FillSkip::PastAuctionEnd` is
  gone: the contract never refused a late fill, so an auction first seen past
  its 400th ledger is now planned rather than skipped. From its 500th ledger
  anyone may delete it, which the filler logs as a warning.
- `plan_fill` projects the `b_rate` cut a full fill causes. On the fork a
  100% fill runs the borrower's default path inside the filler's own
  transaction, before the contract checks the filler's health, so the
  filler's collateral in that reserve is worth less at check time than the
  pre-fill snapshot said. A full fill is now valued against the post-default
  reserves. The fill walk reads each auction's borrower to do this, which
  widens an existing snapshot rather than adding a round trip.
- A fill's primary-asset supply is sized under the reserve's `supply_cap` by
  exact search, instead of rebuilding an over-cap supply the chain refuses
  with error 1220 on every re-plan.
- Bad debt is decided on raw collateral rather than c-factor-weighted
  collateral, matching the contract's own gate. A borrower holding a
  low-factor reserve no longer draws a `bad_debt` proposal the contract
  refuses.
- `new_auction_op` takes no auction-type parameter. Only a user-liquidation
  auction is legal on the fork; every other type raises error 1200.
- A pool's own contract address is never treated as a borrower, and is
  filtered out before the auctioneer's batch snapshot is read. On the fork
  confiscated collateral lands there as ordinary supply.
- `TARGET_HF`'s refusal of exactly `1.15` is unchanged, but its message and
  documentation now say that bound is this bot's own margin. The contract
  itself accepts `1.15`.

- `Notifier::notify` no longer waits for delivery: it takes the dedup
  entry synchronously and spawns the send behind `NOTIFY_IN_FLIGHT`'s
  bounded semaphore, answering immediately rather than after the channel
  does — so a channel that takes seconds to answer, or never answers,
  delays no tick, no liquidation and no fill. `Delivery::Sent` and
  `Delivery::Failed` are gone; `Delivery::Queued` (handed to a delivery
  task) and `Delivery::Dropped` (no permit was free) take their place
  alongside the unchanged `Delivery::Deduplicated`. A delivery that fails,
  or is dropped for want of a permit, rolls back the dedup entry it
  optimistically inserted and writes the notification through `LogChannel`
  instead, so the operator still sees it even though nothing upstream —
  which never waited to be told — learns whether it arrived.

### Fixed

- SIGPIPE-safe `grep` pipelines in `scripts/check-repo-invariants.sh` and
  `scripts/check-release.sh`: a `grep -q` consumer under `pipefail` could
  read its upstream's `SIGPIPE` exit as a failed match even though the
  pattern was found.
- The Dockerfile's `HEALTHCHECK` comment, corrected: it predated `/livez`
  and never explained why the check isn't wired to it.

[Unreleased]: https://github.com/Templar-Protocol/blend-liquidator/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Templar-Protocol/blend-liquidator/releases/tag/v0.1.0
