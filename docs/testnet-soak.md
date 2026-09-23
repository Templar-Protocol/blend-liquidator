# Testnet soak

The design spec's last step before mainnet (`docs/specs/2026-09-04-blend-liquidator-bot-design.md`
§9): running the real binary against public Stellar testnet before it ever
touches money. `docs/deploy.md` is the operator's path from an image to a
running deployment and `docs/deployment-contract.md` is what this repository
guarantees about that image; this document is narrower — the two testnet
stages, `scripts/testnet/*.sh` and the `testnet-*` Make targets that drive
them, the two read-only `examples/` that read the evidence back, and what an
operator watching either stage should expect to see.

## The two stages

**Stage 1, observe**, runs the bot in dry run against Blend's own testnet
pool — a pool this repository did not deploy and does not control, with
borrowers who are other people's testnet accounts. No signing key reaches
the process: `run-bot.sh` unsets both key variables, and `DRY_RUN` is always
`true` here, so nothing is ever submitted. Its purpose is watching the bot
decide against a pool it has never seen before, for as long as it takes to
see it track borrowers, read their positions, and either leave them alone
or plan something — over real testnet conditions, not a scripted scenario.

**Stage 2, armed**, stands up a throwaway Blend v2 deployment of this
repository's own — our own tokens, oracle, backstop and pool, on the public
network rather than a local container — with one borrower a single oracle
price move away from being liquidatable, and runs the bot with
`DRY_RUN=false` against it. This is the only place outside the sandbox tier
that this bot ever signs and sends a transaction with real consequences
(testnet ones), and the only place that proves the whole pipeline — create,
fill, unwind — end to end against public infrastructure rather than a
container's own local network.

Both stages use the same binary, the same store schema, and the same two
read-only examples to read their results back; they differ only in which
pool they follow and whether a key is ever handed to the process.

## Safety

- **Every script under `scripts/testnet/` refuses any network but testnet,
  and names mainnet explicitly if that is what it finds.**
  `scripts/testnet/lib.sh` sources `scripts/sandbox/lib.sh` and adds
  `require_testnet_network`, which calls `TESTNET_RPC_URL`'s own
  `getNetwork` and dies unless it answers the literal passphrase
  `Test SDF Network ; September 2015` — refusing the public mainnet
  passphrase by name first, whatever else it checks (`require_network_passphrase`,
  shared with the sandbox gate). This runs before the first `stellar` call
  or chain read in `deploy.sh`, `crash.sh` and `run-bot.sh` alike, and on
  the URL each then uses: `crash.sh` and `run-bot.sh --armed` source
  `testnet.env` first and gate the `TESTNET_RPC_URL` `deploy.sh` recorded
  there, and `run-bot.sh` hands the binary, as `RPC_URL`, the very URL its
  gate verified.
- **Every `stellar` call in this tier passes `sandbox_network_args`, never
  `--network`.** Those are the exact `--rpc-url`/`--network-passphrase`
  flags built from the URL `require_testnet_network` just verified.
  `lib.sh` unsets `STELLAR_RPC_URL` and `STELLAR_NETWORK_PASSPHRASE` at
  source time (inherited from the sandbox's own `lib.sh`) for the same
  reason the sandbox tier does: the CLI resolves those two ahead of an
  explicit `--network`, so an operator with them exported could otherwise
  have the gate confirm testnet while every later call went somewhere else.
- **The only capital is friendbot's testnet XLM.** `require_funded_testnet`
  polls testnet Horizon (`https://horizon-testnet.stellar.org`) and
  re-requests `https://friendbot.stellar.org` for every identity
  `deploy.sh` generates; nothing in this tier ever holds a mainnet key or
  anything of real value.
- **The filler's secret leaves the keystore exactly once, into one file.**
  `deploy.sh`'s last step reads it straight into `target/testnet/testnet.env`
  (`env_write`, mode `0600`, under the git-ignored `target/`) — never
  echoed, never logged, never passed as an argument. `run-bot.sh --armed`
  reads `TESTNET_FILLER_SECRET_KEY` from that file into the child process's
  environment only.
- **`run-bot.sh --armed` is the only path by which `DRY_RUN=false` ever
  reaches the binary on testnet.** Plain `run-bot.sh` (and `make
  testnet-run`) always sets `DRY_RUN=true` and hands the binary no key at
  all. Only `run-bot.sh --armed` (and `make testnet-run-armed`) sets
  `DRY_RUN=false`, and only after confirming `target/testnet/testnet.env`
  exists.
- **The bot's environment is `run-bot.sh`'s, not your shell's.** Before it
  exports anything, `run-bot.sh` unsets every other setting the bot reads:
  both signing keys (`AUCTIONEER_SECRET_KEY`, and `FILLER_SECRET_KEY`,
  which only `--armed` sets again, from `testnet.env`), `RPC_API_KEY` and
  `RPC_API_KEY_HEADER`, the Telegram pair, `NETWORK_PASSPHRASE`,
  `POOLS_TOML`, `SEED_FILE`, `RUN_MODE`, the HTTP bind settings and every
  tuning knob. A shell exported for another deployment therefore cannot
  hand a testnet run its key, its RPC credential or its alert channel, and
  the soak runs on the defaults this document describes. An armed run also
  ignores an inherited `RUST_LOG`: it holds a key, and `docs/deploy.md` §6
  forbids running a key-holding bot at `trace`.
- **The two `examples/` have no network gate.** `scan_borrowers` and
  `soak_report` never ask the node which network it is: what they print is
  whatever the node `RPC_URL` names answers for. They hold no key and send
  nothing, so that is acceptable, but name the RPC explicitly, as every
  command below does.

## Stage 1: observe

### The pools file

`run-bot.sh` (no `--armed`) reads `target/testnet/pools.toml` and refuses to
start if it does not exist — this file is not generated by any script, and
is this document's job to specify. It names Blend's own testnet pool, taken
from Blend's `blend-utils` repository's `testnet.contracts.json` and
verified live against `soroban-testnet.stellar.org`:

```toml
# Blend's own Blend v2 pool on Stellar testnet, for the soak's observe stage.
# Read-only: the bot runs with DRY_RUN=true and no signing key, so nothing here
# can be submitted. Addresses are an example (see "Testnet resets" below) —
# re-derive current ones from blend-utils/testnet.contracts.json.
[[pools]]
address = "CCEBVDYM32YNYCVNRXQKDFFPISJJCV557CDZEIRBEE4NCV4KHPQ44HGF"
primary_asset = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC" # XLM
min_primary_collateral = "1000000000"
min_health_factor = 1.5
default_profit_bps = 100
fill_objective = "earliest-profitable"
supported_bid = ["*"]
supported_lot = ["*"]
```

See `pools.example.toml` for what each field means; nothing about this
table is testnet-specific beyond the addresses.

### The database

Postgres must already be running (`make db-up`) and the two soak databases
must already exist — `Store::connect` opens a pool against a database that
is already there; it creates no database, only the schema inside one.
Create them once with `sqlx-cli` (already required by `make sqlx-prepare`),
handing it the URL through `DATABASE_URL` in its environment rather than
`--database-url` on its command line — the URL carries a password, and
`/proc/<pid>/cmdline` is world-readable (the Makefile's own database sweep
does the same):

```bash
DATABASE_URL=postgres://liquidator:liquidator@127.0.0.1:55432/testnet_soak sqlx database create
DATABASE_URL=postgres://liquidator:liquidator@127.0.0.1:55432/testnet_armed sqlx database create
```

The binary itself migrates each one at startup (`RUN_MODE=loop`, this
tier's default, migrates under an advisory lock — see
`docs/deployment-contract.md`); nothing else applies the schema.

### Starting it

```bash
cargo build
make testnet-run
```

`make testnet-run` runs `./scripts/testnet/run-bot.sh` with no argument:
`DRY_RUN=true`, no key, port `18081`, database `testnet_soak`, logging to
`target/testnet/dry-run.log`. The script execs the binary, so the pid it
started with is the bot's own; note it (`$!` if you start it in the
background) — see "Stopping the bots" below.

`TESTNET_RUN_PORT` and `TESTNET_RUN_DATABASE`, set together or not at all,
are for exercising the script beside a live run of the same mode: the run
takes that port and that (already created) database, and logs to
`target/testnet/<database>.log` rather than the mode's own transcript, so it
touches nothing the live run owns. They are not a way to start a second
instance of a mode on that mode's own database — two bots on one store,
and armed on one key, is the deployment contract's overlapping-instance
case, which a soak should not be measuring — and `TESTNET_RUN_DATABASE`
refuses to name `testnet_soak` or `testnet_armed`.

### What to watch

Some signals move as the bot works and some only when its full scan runs,
and on testnet the difference can be as much as 100 minutes. Watch the
live ones to see whether it is tracking.

**Live:**

- **`/metrics`** on port `18081`: `blend_liquidator_ledger_processed{pool=...}`
  (the last ledger the tracker fully applied — the cursor the poller
  commits) and `blend_liquidator_ledger_head{pool=...}` (the chain head,
  recorded each pass the moment `getLatestLedger` answers), which should
  stay within a ledger or two of each other;
  `blend_liquidator_poller_heartbeat_timestamp_seconds{pool=...}`, stamped
  every pass and every `POLL_INTERVAL_MS` (5 s here) within one, so it lags
  wall time by seconds while the poller is turning;
  `creations_total`/`fills_total` (an observe-stage pool can still produce
  `attempted` rows if the bot ever decides a tracked borrower is
  liquidatable — nothing is ever `succeeded`, since `DRY_RUN=true`
  submits nothing), and `skips_total` for the filler's own reasons.
- **The log** (`target/testnet/dry-run.log`, `LOG_FORMAT=json`): the first
  lines name whether it is armed (here, always "dry-run: no transaction
  will be submitted") and the resolved configuration; "seeded pool" gives
  the tracked count after the startup seed pass; "skip: no action taken"
  (debug, with a `reason` such as `Healthy`) is the auctioneer's decision
  about one borrower, logged the moment it is made — each time that
  borrower is rechecked: an event naming it, an oracle move past
  `PRICE_DELTA_BPS` in an asset it holds, or the auctioneer's own
  scan-and-flag; "filler tick" is the filler's walk of the pool's open
  auctions, once a ledger (debug when it did nothing, info when it planned,
  executed, closed or unwound something).
- **`soak_report`'s tracked-user count** (see "Reading the result" below),
  which reads the store directly: the number of accounts the store holds a
  position for right now.

**Full scan only:** `blend_liquidator_users_tracked{pool=...}` and the
"full scan" and "tracked borrower" log lines are written by the tracker's
full scan and nothing else (`full_scan` in `src/service.rs`). It runs once
when the process starts and then once in every `FULL_SCAN_LEDGERS` period
(1,200 ledgers, about 100 minutes at testnet's ~5 s ledgers), at a random
phase. Between scans the gauge holds the last scan's count, and "tracked
borrower" lists only accounts below `SCAN_HF_THRESHOLD` (1.2), at most 20
per scan. This run's own log shows the gap: the watched position below was
first decided ("skip … Healthy") at 01:54:52, after the full scan at
01:52:31 had reported `user_count` 0, and it was the next one, at 02:52:56,
that first reported 1 and logged it as a tracked borrower.

On a stock pool — which is what testnet runs; see `docs/deploy.md`'s "On a
stock pool, expect `StockWasmDetected`" — every `bad_debt` event the poller
reads, from its cursor forward, raises that notification, once per pool and
account within `FAILURE_NOTIFICATION_COOLDOWN_HOURS`. A pool's first run
follows from the chain head, so an event from before it raises nothing. It
is expected here, not a sign of anything wrong.

### Seeding it with `scan_borrowers`

Blend's testnet pool has no analytics API to seed from (`SEED_URL`'s
default answers for mainnet only, and `run-bot.sh` always sets it empty for
exactly that reason), and with no seed file the tracker only follows
accounts that act while it is watching. `examples/scan_borrowers.rs` finds
accounts worth tracking from the pool's own recent event history instead:

```bash
RPC_URL=https://soroban-testnet.stellar.org \
  cargo run --example scan_borrowers -- CCEBVDYM32YNYCVNRXQKDFFPISJJCV557CDZEIRBEE4NCV4KHPQ44HGF
```

`ledgers` (the optional second argument) defaults to `17280`, about 24
hours at testnet's ~5 s ledger close; `... 120960` scans 7 days instead.
It prints the accounts found, then a ready-to-paste `[accounts]` block in
`SEED_FILE`'s shape — paste that into `target/testnet/seed.toml` (see
`seed.example.toml`), which `run-bot.sh` reads automatically if present,
before the next start.

**This is bounded by what the RPC still retains, and that bound is the main
thing to know about this stage.** On 2026-09-23 a 24-hour scan of Blend's
testnet pool found 9 accounts and a 7-day scan found 15 — and of either set,
**none held debt**: those positions were taken long before the window and
their owners had not acted since, so there was nothing for the bot to
liquidate among them. A later scan will find a different set; the shape of
the result, not its numbers, is what carries over. On a
pool this repository does not control, "what the RPC's event window still
holds, plus whatever acts while the bot is watching" is the entire universe
of borrowers the bot can ever see — there is no fallback to an index of
every position the pool has ever held. (Mainnet does not have this
limitation: the public analytics API `SEED_URL` defaults to enumerates
positions directly, which is the reason that API exists.)

### Giving it something real to follow: a watched position

To see the observe stage track and value an actual borrower rather than an
empty pool, supply collateral and borrow against it yourself, in the same
reserve, using a fresh identity funded by friendbot. Read the reserve's own
factors first rather than assuming them — they differ per asset and per
pool.

These commands are run by hand, so they do not source
`scripts/testnet/lib.sh` and nothing unsets the stellar CLI's own network
and signing variables for them. Each is prefixed with that unset instead:
an exported `STELLAR_SIGN_WITH_KEY` would otherwise sign the `submit` with
a key that is not this identity's. The explicit `--rpc-url` and
`--network-passphrase` pin the network, but nothing here asks the node
which network it is first, as the scripts' gate does — check the URL.

```bash
env -u STELLAR_SIGN_WITH_KEY -u STELLAR_SIGN_WITH_LAB -u STELLAR_SIGN_WITH_LEDGER \
  -u STELLAR_ACCOUNT -u STELLAR_RPC_URL -u STELLAR_NETWORK_PASSPHRASE -u STELLAR_NETWORK \
  stellar keys generate testnet-soak-watched --fund \
  --rpc-url https://soroban-testnet.stellar.org \
  --network-passphrase "Test SDF Network ; September 2015"
WATCHED=$(env -u STELLAR_SIGN_WITH_KEY -u STELLAR_SIGN_WITH_LAB -u STELLAR_SIGN_WITH_LEDGER \
  -u STELLAR_ACCOUNT -u STELLAR_RPC_URL -u STELLAR_NETWORK_PASSPHRASE -u STELLAR_NETWORK \
  stellar keys address testnet-soak-watched)

env -u STELLAR_SIGN_WITH_KEY -u STELLAR_SIGN_WITH_LAB -u STELLAR_SIGN_WITH_LEDGER \
  -u STELLAR_ACCOUNT -u STELLAR_RPC_URL -u STELLAR_NETWORK_PASSPHRASE -u STELLAR_NETWORK \
  stellar contract invoke --id CCEBVDYM32YNYCVNRXQKDFFPISJJCV557CDZEIRBEE4NCV4KHPQ44HGF \
  --source-account testnet-soak-watched \
  --rpc-url https://soroban-testnet.stellar.org \
  --network-passphrase "Test SDF Network ; September 2015" \
  --send=no -- get_reserve --asset CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC
# read .config.c_factor and .config.l_factor from the reply (7-decimal
# fixed point — chain::xdr::decode::reserve_config_value decodes the same
# fields this crate's own way)

env -u STELLAR_SIGN_WITH_KEY -u STELLAR_SIGN_WITH_LAB -u STELLAR_SIGN_WITH_LEDGER \
  -u STELLAR_ACCOUNT -u STELLAR_RPC_URL -u STELLAR_NETWORK_PASSPHRASE -u STELLAR_NETWORK \
  stellar contract invoke --id CCEBVDYM32YNYCVNRXQKDFFPISJJCV557CDZEIRBEE4NCV4KHPQ44HGF \
  --source-account testnet-soak-watched \
  --rpc-url https://soroban-testnet.stellar.org \
  --network-passphrase "Test SDF Network ; September 2015" \
  --send=yes -- submit --from "$WATCHED" --spender "$WATCHED" --to "$WATCHED" \
  --requests "[{\"request_type\":2,\"address\":\"CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC\",\"amount\":\"10000000000\"},{\"request_type\":4,\"address\":\"CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC\",\"amount\":\"7710000000\"}]"
```

`request_type` 2 is `SupplyCollateral`, 4 is `Borrow` (the same encoding
`scripts/sandbox/deploy.sh` and `scripts/testnet/deploy.sh` use); both in
one `submit` call, atomically. This supplies 1000 XLM as collateral and
borrows 771 XLM against it, in the same reserve — no second trustline or
approval needed, since both legs are the native asset. On Blend's testnet
pool the XLM reserve's collateral and liability factors are both 0.9, and
the health factor is `(collateral × c_factor × l_factor) / debt` when
collateral and debt price identically: `(1000 × 0.9 × 0.9) / 771`, which
is 810/771, about 1.05058 — borrowing roughly 77% of what you supply lands
there.

What this is for: a real borrower of the soak's own, on a pool this
repository does not control, for the observe stage to find, track and
value. It is not a way to get a liquidation. With collateral and debt in
the same reserve, the health factor moves only by how much faster the
debt's rate accrues than the collateral's. On 2026-09-23 the bot valued
the position at 1.0505836 about an hour after it opened and again nine
minutes later, the opening ratio to 7 decimals both times (810/771 is
1.05058365…). At that pace a crossing of `LIQ_HF_THRESHOLD` (0.998) is far
off; a liquidation on testnet is what stage 2 is for.

Nothing further is required — the bot follows this account the moment its
`supply_collateral`/`borrow` events reach the poller, with no seed entry
needed, exactly the "acts while it watches" path above.

## Stage 2: armed

### Deploy

```bash
make testnet-deploy
```

Runs `scripts/testnet/deploy.sh`: `scripts/sandbox/deploy.sh`'s own ten
steps against testnet rather than a local container — four funded
identities, the mock oracle, Comet, the backstop and pool factory (address
predicted before either exists, since each names the other), the pool
itself with XLM and USDC reserves, backstop funding past the activation
threshold, and one borrower supplying 5,000 XLM of collateral and borrowing
300 USDC — worth $375 and ~$315.8 respectively at the deploy-time price,
a health factor around 1.19, nothing to liquidate yet. Refuses to start if
`target/testnet/testnet.env` already exists (see "Testnet resets" below);
otherwise writes it at the end, `mode 0600`, holding the deployed
addresses and the filler's secret key. A transcript of every step lands in
`target/testnet/deploy.log`.

### Crash

```bash
make testnet-crash
```

Runs `scripts/testnet/crash.sh` with its default price (`750000`, $0.075 —
`PRICE=<n>` in the oracle's own 7-decimal units overrides it). Moves the
XLM price down 25% from deploy's $0.10, taking the borrower's collateral
value to $281.25 and its health factor to about 0.89 — now liquidatable.
Safe to run before or after starting the bot; it touches nothing but the
oracle and can be re-run at any time.

### Run armed

```bash
make testnet-run-armed
```

Runs `./scripts/testnet/run-bot.sh --armed`: regenerates
`target/testnet/pools.armed.toml` from `testnet.env` (one pool, XLM as
`primary_asset`, `fill_objective = "earliest-profitable"`,
`supported_bid` limited to the USDC this deployment minted), and
`target/testnet/seed.armed.toml` naming the borrower — the borrower took
its position inside `deploy.sh`, before the bot or its events cursor
existed, so nothing in the range the poller reads would ever name it
without this seed. `DRY_RUN=false` and `FILLER_SECRET_KEY` are exported
into the binary's own environment only. Port `18082`, database
`testnet_armed`, logging to `target/testnet/armed.log` (the
`TESTNET_RUN_PORT`/`TESTNET_RUN_DATABASE` pair under "Starting it" above
applies here too, with the same limit).

### What to expect, and when

At testnet's ~5 s ledger close, an auction's 400-ledger ramp takes about 33
minutes end to end. `fill_objective = "earliest-profitable"` (this pool's
own setting) aims the fill at the earliest ledger the lot covers the bid
plus the pool's profit margin — about 15 minutes after the auction opens
for this scenario's numbers, not the ramp's end. Order of events to expect:

1. **Startup and seed** — under a minute: keys and pools validated, the
   borrower's position read and tracked from the seed file.
2. **The auction** — as soon as the auctioneer's next tick sees the crashed
   price and the borrower's health factor below `LIQ_HF_THRESHOLD`: a
   `creations` row, `dry_run = false`, a `tx_hash`, and a
   `NotificationKind::AuctionCreated` log line ("liquidation auction
   created at N% in ledger L") naming the percent.
3. **The fill** — around 15 minutes later: a `fills` row, `dry_run =
   false`, its own `tx_hash`.
4. **The unwind** — moments after the fill lands: one or more unwind
   passes; the filler repays what its wallet can cover and withdraws
   collateral down toward `min_primary_collateral`, then a pass finds
   nothing left to move and the pool goes idle.

Budget an hour for a full armed pass from a clean deploy — comfortably past
the ~33-minute ramp, with room for the startup seed, a re-plan or two, and
the unwind passes that follow.

## Reading the result

`examples/soak_report.rs` is read-only — it opens the store without
migrating it and takes no key — and prints exactly the evidence either
stage produces: the tracked-user count and open auctions, every
`creations` and `fills` row (with `dry_run` and `tx_hash`), and, for any
accounts named on the command line, their on-chain position read fresh.

```bash
DATABASE_URL=postgres://liquidator:liquidator@127.0.0.1:55432/testnet_armed \
RPC_URL=https://soroban-testnet.stellar.org \
  cargo run --example soak_report -- <pool> [account...]
```

Point `DATABASE_URL` at `testnet_soak` for stage 1 or `testnet_armed` for
stage 2, and `<pool>` at whichever pool that stage follows (`RPC_API_KEY_HEADER`/
`RPC_API_KEY` are honoured together, as `pool_snapshot` does, for a keyed
RPC provider). `RPC_URL` is needed only when an account is named: the
tracked-user count, the open auctions and the audit rows come from the
store directly, not from `/metrics`, so the count is the store's right now
rather than the last full scan's. The positions come from whatever node
`RPC_URL` names, with no network gate (see "Safety" above).

Beyond the report and the log lines named under each stage above, `/metrics`
(`18081` for stage 1, `18082` for stage 2) is the fastest live check:
`creations_total`/`fills_total` by `result` (`attempted`/`succeeded`/`failed`),
`estimated_profit_total`/`estimated_loss_total` in the pool oracle's own
units, and `unwind_passes_total`. `/healthz` and `/livez` on the same port
answer readiness and liveness the same way any deployment's do — see
`docs/deploy.md` §6.

## Testnet resets

Stellar wipes every contract and every account on testnet 2 to 4 times a
year. Nothing deployed here survives one: **every address in this
document is an example of the shape an address takes, never a fact that
outlives a reset.** After one, `scripts/testnet/deploy.sh` rebuilds stage
2's deployment from scratch (delete `target/testnet/testnet.env` first if
it still names the old, now-gone pool), and stage 1's `pools.toml` needs
its addresses re-derived from Blend's own current `blend-utils/testnet.contracts.json`
the same way this document's own example was.

## Rebuilds, cleanup and stopping

### What a dev-container rebuild loses, and what survives

The stellar CLI keeps its identities in `~/.config/stellar/identity/`,
inside the container, and a rebuild deletes them: every `testnet-soak-`
identity `deploy.sh` generated (issuer, admin, borrower, filler) and the
`testnet-soak-watched` one above. That means `crash.sh` can no longer sign
as the admin, so the deployment's oracle can never be moved again, and the
watched position's key is gone. This container lost them in the rebuild of
2026-09-23.

What survives is everything on the workspace mount and the Docker volume:
`target/testnet/` — the pools and seed files, the logs, and
`testnet.env` — and both databases, which live on the `postgres-data`
volume. After a rebuild `testnet.env` is the only copy of the filler's
secret key, and `cargo clean` deletes `target/`, so it takes that key
with it.

### Recovering

- **Stage 1** needs nothing but a restart (`make testnet-run`): its
  store holds its users and its events cursor, so it resumes from the
  cursor without reseeding and catches up to the chain head. That holds
  while the cursor is inside the RPC's retained event window (about 7 days
  on 2026-09-23); a cursor older than that is reported as a gap and the
  pool is reseeded.
- **Stage 2** needs a fresh deployment, since its admin key is gone:
  remove `target/testnet/testnet.env` (`deploy.sh` refuses while it
  exists) and run `make testnet-deploy` again, then crash and run as
  before. The old deployment is abandoned where it stands. `testnet_armed`
  can stay: the store keys its rows by pool, so the new pool starts with
  no cursor and seeds from the new `seed.armed.toml`.

### What the soak leaves behind on testnet

Nothing here tears anything down: testnet is not this repository's to
reset, and the next reset wipes it all. Until then, these remain:

- stage 2's deployment — the pool, the USDC and BLND asset contracts, the
  mock oracle, Comet, the backstop and the pool factory — with the
  borrower's position what the liquidation left of it and the filler's
  `min_primary_collateral` still supplied;
- every friendbot-funded account the soak created: the four
  `testnet-soak-` identities `deploy.sh` generated and
  `testnet-soak-watched`;
- the watched position on Blend's own pool (1,000 XLM supplied, 771 XLM
  borrowed). Closing it would mean repaying the debt and then withdrawing
  the collateral, signed as `testnet-soak-watched`; with that key lost, it
  is abandoned until the reset.

### Stopping the bots

By pid, with `SIGTERM`, never by pattern. Both stages run the same binary
(`target/debug/liquidator`), so a `pkill -f liquidator` stops whichever
bots match, the other stage's included. `run-bot.sh` execs the binary, so
the pid it started with is the bot's own. If you did not note it, find it
by the port it serves:

```bash
ss -ltnp 'sport = :18081'   # stage 1; 18082 for stage 2
kill -TERM <pid>
```

The first `SIGTERM` shuts the bot down cleanly. A second one exits at
once (status 130).

## Stage 1 results

Run of 2026-09-23, against Blend's testnet pool
`CCEBVDYM32YNYCVNRXQKDFFPISJJCV557CDZEIRBEE4NCV4KHPQ44HGF` (an example;
see "Testnet resets" above). Dry run, no key configured.

- **Span:** 00:50 to 03:03 UTC, about 2 hours 12 minutes, ledgers
  4,820,214 to 4,821,800 (about 1,590) — the cut-off these results are
  drawn from; the process itself was left running past it. Five starts:
  the first, three operator restarts while its seeding was wired up, and
  one resume after the process was killed without warning when this dev
  container was rebuilt at 02:53.
- **Discovery:** `scan_borrowers` found 9 accounts in the last 24 hours
  and 15 in the last 7 days (the oldest ledger the RPC still held was
  4,699,988); seeding with all 15 tracked none, since none held debt.
- **The watched position:** `testnet-soak-watched`
  (`GB2CGACJLELPQVREA4P2T3EVNWZPZTSKBJDLQRWPOLSTNJN4UQPZLU25`) supplied
  1,000 XLM and borrowed 771 XLM, a designed health factor of 810/771,
  about 1.05058. The bot picked it up from the pool's own events and first
  decided it "Healthy" at 01:54:52; the full scans at 02:52:56 and after
  the resume (03:02:14) valued it at 1.0505836, the opening ratio to 7
  decimals, and the auctioneer decided "Healthy" again at 03:02:16 —
  correct, since the liquidation threshold is 0.998. Collateral and debt
  are the same asset, so the health factor moves only by the spread
  between the borrow and supply rates; it did not move at 7 decimals in
  over an hour.
- **Cadences:** 26 oracle scans and 6 full scans.
- **What the public network did, and how the bot handled it:**
  - 2 RPC transport errors (02:38:48 and 02:50:56), each retried after
    the poller's backoff. Neither began a streak long enough for
    `RpcFailing` (5 in a row).
  - 3 oracle scans failed with "the ledger moved between reads" (02:28,
    02:34 and 02:39). A snapshot is several separate reads — the pool's
    shape (2 keys), the reserves (8 keys for this pool's four, one
    `getLedgerEntries` well inside a single 200-key batch, so not a
    batching effect), then one oracle simulation per asset — and each must
    report the same ledger. In each of the three, the ledger moved between
    those reads on all three of the snapshot's attempts, and the attempts
    were long: 2.5 s to 70 s each, the ledger moving by 1 to 12 within
    one. With ~5 s ledgers a close inside an attempt that long is likely
    rather than unlucky; a load-balanced public RPC whose `latestLedger` is
    not monotonic across calls is a possible further cause, and the logs do
    not say which. The read fails closed rather than mixing two ledgers,
    and the scan runs again next period — about 5 minutes at the default
    `ORACLE_SCAN_LEDGERS` of 60. None of the sandbox runs whose logs this
    container still holds failed a snapshot this way.
  - 1 `PollerStalled` at 02:50:55 ("no poller heartbeat for 60s, limit
    55s"), with recovery logged five seconds later — the only stall in
    the run, in the same second as a transport error. The code's premise
    holds: the poller heartbeats its whole pass (the RPC calls, the event
    paging, the wait for the tracker, the cursor read and write), so the
    only stretch it stamps nothing is the end of one pass and the sleep
    before the next: up to one 5 s interval since the pass's last stamp,
    then 5 s after a good pass or a backoff capped at 30 s after a failed
    one — about 35 s at most, and the backoff was at its 1 s minimum then.
    But the log contradicts a frozen process: the poller's own cursor read
    (`SELECT ledger, paging_token FROM cursors`) completed at 02:50:17,
    after 3.95 s, inside the silent minute, so the pass was running while
    its heartbeat went unstamped;
    and the stage 2 bot, a separate process, logged nothing from 02:50:03
    to 02:50:56 either. That is contention across the whole machine — both
    bots slowed together, Postgres took 4 s over a one-row read, and the
    repository's test suite and builds shared the machine — and the cause
    of the missed stamps is not established.
  - 44 slow-statement warnings and 7 slow connection acquires from 02:16
    onward, when the repository's own test suite and builds were sharing
    the same Postgres. They slowed the bot and changed none of its
    decisions.
- **Resume after the abrupt kill:** no reseed, since both users and a
  cursor were present, and the processed ledger equalled the chain head
  (4,821,797) within 45 seconds: the poller resumed from its last
  committed ledger and caught up the gap.
- **Nothing was submitted:** no key was configured, and the store holds no
  `creations` or `fills` rows for the pool.

## Stage 2 results

Run of 2026-09-23 (addresses below are that run's; treat them as an
example — see "Testnet resets" above):

- Pool `CC4LXYC4X3FXPAVCAWV3NAOGGVBVY6K67MRLDMWREC4WQ6IC6OLXCLPC`.
- **Creation**: `creations` row `#1`, borrower
  `GDVQR7O4HOAVWKJ6ACOZZM2EW7RLBP7MEJW6T7NJ5QRUEWDMEMEHJZXM`, percent 69,
  `dry_run = false`, tx
  `3e3cdbe135c7d103099875b6eb6ca87ab01f4c6567a1dd27718e8afe4ebdb251`.
- **Fill**: `fills` row `#1`, `auction_type user_liquidation`, percent 100,
  `fill_ledger` 4820857, `est_profit` 25292272, `dry_run = false`, tx
  `ec3c26d176d4b5570e217eee848e684e962ac35e4422eb52179104f94448921f` —
  landed about 15 minutes after the auction was created, matching
  `fill_objective = "earliest-profitable"`'s own target for this pool.
- **Unwind**: submitted tx
  `3ebd3559350baa7961816019297da7d39e52719ae8c7fda5bbdfe26bad77ad49`;
  the pass after it reported the pool's unwind had nothing left to move.
  The filler's on-chain position afterward: 0 liabilities, and primary
  collateral valued at 56250000 in the oracle's own units — exactly 100 XLM
  at $0.075 and a 0.75 collateral factor, which is this pool's
  `min_primary_collateral`. The unwind withdrew everything above that
  floor to the wallet and kept the floor supplied, as it does in the
  sandbox.
- **`/metrics`**: `creations_total{result="succeeded"} 1`,
  `fills_total{result="succeeded"} 1`, `unwind_passes_total 3`,
  `estimated_profit_total 25292272`, `estimated_loss_total 0`.
