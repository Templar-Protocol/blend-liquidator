# blend-liquidator

## What this is

A liquidation bot for [Blend Protocol](https://blend.capital) lending pools on
Stellar. It is intended to repay the debt of underwater positions and receive
their collateral at a discount.

**Status: Phase 5.** Phase 1 landed the pure fixed-point math (`math`) and
the ScVal/ledger-entry codecs (`chain::xdr`); Phase 2 landed the chain layer
(`chain::rpc`, `chain::pool`, `chain::signer`, `chain::tx`); Phase 3 landed
the Postgres store, a per-pool ledger poller and a tracker (`store`,
`ledger`, `tracker`, `service`), so the binary validates its configuration,
seeds its tracked-user set from the analytics API or a static file, and
follows every configured pool — applying events and refreshing borrowers'
health factors from chain — until it is shut down. Phase 4 landed the
auctioneer (`auctioneer`, `queue`, `math::liquidation`): once a tick, it
decides which tracked borrowers are liquidatable or owe bad debt, builds
the auction the contract should accept, lets the contract judge the percent
through simulation, records every creation it decides to make — dry-run
or not — and, only
when a signing key is configured and `DRY_RUN=false`, submits it through a
per-key queue. Phase 5 landed the filler (`filler`, `executor`,
`inventory`, `math::fill`): once a tick, it plans a fill for every open
liquidation auction whose assets its pool configuration supports, holds its
own position at or above `min_health_factor × HF_SAFETY_MULTIPLIER` while
taking one over, records every fill it executes — dry-run or not — and,
only with `DRY_RUN=false` *and* `FILLER_SECRET_KEY`, submits it on the
filler key's queue. **Nothing unwinds a fill yet:** a live fill leaves the
taken position — the lot as collateral, the bid as debt — sitting in the
pool, and nothing sells, repays or withdraws it. That is Phase 6. The
repository scaffolding is complete and enforced.

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
  `clap` arguments. The two signing keys — `AUCTIONEER_SECRET_KEY`, and
  `FILLER_SECRET_KEY`, which it falls back to — are read from the
  environment by `main.rs` and handed to `Args::signing_keys`; neither is
  ever a clap field, like every other secret, because argv is
  world-readable. Both are parsed at startup, so a malformed value in
  either is a startup error, and so are the two rules that pair them:
  `DRY_RUN=false` without `FILLER_SECRET_KEY` (the filler signs with its
  own key only, so an armed bot without it would create auctions and never
  fill one), and the two keys being the *same* key (leave
  `AUCTIONEER_SECRET_KEY` unset to share one). `SigningKeys::into_signers`
  answers which key signs which role, and `Signers::shared` tells by
  pointer whether both roles hold the one `Arc`.
- `src/main.rs` — binary entry point: tracing setup, argument parsing, exit.
- `src/math/` — the pure port of the pool contract's arithmetic: `fixed`
  (checked rounding), `reserve` (accrual and token conversions), `position`
  (effective values and health factor), `auction` (Dutch-auction scaling),
  `liquidation` (which auction to create — `plan_liquidation` selects the
  bid and lot assets and the percent that closes a borrower's excess down
  to `TARGET_HF`, walking in more assets when the selection cannot),
  `fill` (which auction to *take* — `fill_delay` answers, in closed form
  proved against the contract's own modifiers by `meets_margin`, the fewest
  ledgers after an auction's start at which its lot covers its bid plus the
  pool's profit margin; `health_floor` is `min_health_factor ×
  HF_SAFETY_MULTIPLIER`, rounded up; `plan_fill` builds the request list —
  the fill, a repay of each bid asset the wallet holds, a withdrawal of
  each zero-collateral-factor lot asset, a supply of the primary asset —
  by projecting the filler's own post-fill position exactly, and escalates
  supply → lower percent → later ledger when the projection is short,
  searching candidates exactly rather than estimating one a later round
  would only have to correct).
  Nothing here does I/O and nothing panics.
- `src/chain/xdr/` — ScVal codecs for the pool: `encode` (values, operations,
  simulation envelopes), `keys` (ledger keys, durability included), `decode`
  (entries and view-call returns), `events` (pool events).
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
  the cursor untouched.
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
  3, `FILL_RETRIES` 10 — backing off from one second, doubling, to thirty;
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
  outcome.
- `src/service.rs` — wiring: `Service::check_config` validates the
  configuration against the chain *and* the database (connect and ping, per
  the spec's deployment contract) and reports without following anything;
  `Service::run` connects and migrates the store, seeds every pool whose
  tracked-user count or events cursor is missing, then runs five kinds of
  task until a shutdown signal arrives and every one has returned: one
  `LedgerPoller` per pool, one tracker task consuming their shared channel,
  one auctioneer task, one filler task, and — only when armed — one
  submission-queue worker per *distinct* signing key, which is what
  `spawn_queues` is for. The tracker loop treats a `TrackerError::Store`
  as fatal and a `Chain` or `Math` one as transient — it declines the tick,
  and the same range is read again. Both entry points share `validate` and
  `validate_filler`: the filler's account must exist on the network and
  hold at least `XLM_FEE_RESERVE` of the native asset — armed, either
  failure is a startup *error*; in dry-run each is a warning — and, armed
  only, holding less than a pool's `min_primary_collateral` is a warning.
  With no `FILLER_SECRET_KEY` at all there is nothing to check and the
  warning says so: the filler plans against an empty inventory and
  simulates nothing.

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
  derives from the same contract-attested inputs.
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

The module layout beyond this follows
`docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`; the
phases still to land are unwind — which is what makes a filled position
into realised profit — and the rest of the operational surface.

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
  kept out of that gate.
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
- Commit signing in the dev container: `user.signingkey` copied from the host
  is a **host path** that does not resolve inside the container. The durable
  fix is a literal `key::ssh-ed25519 ...` value in the host's `~/.gitconfig` —
  it copies in verbatim on every rebuild and needs no script. See
  `.devcontainer/git-signing.sh`.
- The `stellar` CLI is deliberately **not** in the dev container yet: it is a
  multi-minute source build on every rebuild, and nothing invokes it. It stays
  out until Phase 7's sandbox integration tier (the spec's section 9) needs it
  to deploy the pool contracts locally; add it — and a cgroup-aware build-job
  cap alongside it — in that phase.
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
  at or above `1_1500000` is `InvalidLiqTooLarge` (error code `1213`), and
  below `1_0300000` is `InvalidLiqTooSmall` (`1214`, raised only for a
  partial liquidation). `TARGET_HF`'s default of `1.06` sits between them
  with room for a ledger or two of drift before the auction is filled.
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
- An auction's 400th ledger is the end of its ramp: from there the bid
  modifier is zero and the lot is complete, so there is nothing left to
  wait for — but a fill is still a position takeover that must pass the
  health check, so `plan_fill` answers `PastAuctionEnd` unless the pool
  sets `force_fill`. `force_fill` means two things at once: fill past the
  400th ledger at all, *and* cap both the profit delay and the health
  escalation at 350 ledgers (`FORCE_FILL_MAX_DELAY`), so the fill happens
  no later than that however little the lot then covers. What it does not
  mean is "fill regardless of profit": the margin still decides *when*,
  and the health floor still decides *whether*.
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
- `scripts/` — repo-invariant and release preflight checks, review tooling.
- `docs/` — design specs.
- `.github/workflows/` — CI and release automation.
