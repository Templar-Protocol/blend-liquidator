# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
- **Nothing unwinds a fill yet.** A live fill pays the auction's bid and
  takes its lot, which leaves the position in the pool — the lot as
  collateral, the bid as debt on the filler's own account — and nothing in
  this phase sells, repays or withdraws it. That is Phase 6.
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
  leave the borrower at — refused at parse outside the contract's own
  `InvalidLiqTooSmall`/`InvalidLiqTooLarge` band of `[1.03, 1.15)`, since
  `TARGET_HF=0` would make every liquidatable borrower a silent
  "no plan" for ever; `LIQ_HF_THRESHOLD` at or above `SCAN_HF_THRESHOLD`
  is refused for the mirror reason),
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
