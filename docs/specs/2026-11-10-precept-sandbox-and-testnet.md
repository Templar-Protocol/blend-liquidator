# The sandbox and testnet tiers on Precept wasm

**Date:** 2026-11-10
**Status:** design approved. Closes the deferral in
[2026-09-20 ADR-0008 fork semantics](2026-09-20-adr-0008-fork-semantics.md)
§6 ("the sandbox stays on stock pins for now") and the "next change" that
[2026-11-09 Precept v0.1.1](2026-11-09-precept-v0.1.1-reconciliation.md) §5
names. Everything either document says and this one does not contradict
still stands.

## Source of truth

Both test tiers deploy stock Blend v2 today, so none of the fork's
behaviour, and none of what `v0.1.1` changed, has ever met real contract
code. This document moves both tiers onto Precept's released wasm, makes the
deployed pool look like a live Precept market rather than a made-up one, and
adds the one scenario that exercises the bot's own default fill timing.

| Tree | Ref |
|---|---|
| Wasm pinned | `Templar-Protocol/precept` release `v0.1.1` (tag commit `19cb928`) |
| Source read | the same repository at `v0.1.2`, commit `d819beb` |
| Market shape | `registry/configs/xlm-usdc.json` at `v0.1.2` |

`v0.1.2`'s release notes state, and its `SHA256SUMS` confirms, that
`pool.wasm`, `backstop.wasm`, `pool_factory.wasm` and
`pool_oracle_adapter.wasm` are byte-identical to `v0.1.1`'s; `v0.1.2` only
adds two unaudited incident-response contracts this design deploys neither
of. `v0.1.1` is pinned because it is the release every live market was
deployed from and the one this repository's docs and specs name. Line
references below are Precept's at `v0.1.2` unless they name a file in this
repository.

## 1. What Precept changes for a deploy

### 1.1 The backstop take rate must be zero

`pool_factory::deploy` refuses any `backstop_take_rate` other than `0` with
`1300 InvalidPoolInitArgs` (`pool-factory/src/pool_factory.rs:74`), before
it ever deploys the pool; the pool's own `execute_initialize` would refuse
it with `1201 InvalidPoolConfigArgs` (`pool/src/pool/config.rs:27`), but the
factory's check runs first. Both `scripts/sandbox/deploy.sh` and
`scripts/testnet/deploy.sh` pass `--backstop_take_rate 1000000`. CLAUDE.md's
fork gotcha names `1201` as what they would hit; it is `1300`.

### 1.2 `ReserveConfig` has a required `min_position`

`min_position: i128` sits between `supply_cap` and `enabled`
(`pool/src/storage.rs:71`) and must be non-negative (`config.rs:205`). The
CLI builds a `ReserveConfig` argument from a JSON object keyed by field
name, so `deploy.sh`'s `reserve_metadata` must emit the field or the
`queue_set_reserve` call fails to encode.

### 1.3 Leaving Setup reads the oracle's decimals

`execute_set_pool_status` refuses to move a pool out of Setup to status 0,
2 or 3 unless the oracle reports exactly 7 decimals (`InvalidPrice`,
`pool/src/pool/status.rs:96-101`); every later priced read checks the same
(`pool/src/pool/pool.rs:100-113`). The mock oracle is already configured
with 7. Nothing else about leaving Setup moved: `set_status(0)` still needs
the backstop threshold met and queued withdrawals under 50%, and
`update_status` still refuses from Setup.

### 1.4 What did not change

- The backstop's constructor, the factory's `deploy` and constructor
  signatures, `queue_set_reserve`/`set_reserve` in Setup, the backstop
  `deposit` and Comet's `init`/`join_pool` are all as `deploy.sh` already
  calls them.
- A reserve's supply is checked against its own configured `supply_cap`
  only (`pool/src/pool/actions.rs:385`). ADR-0008's $25k stress-priced cap
  is Precept's deployment policy, applied when a market's config is
  written, not a rule the contract enforces, so the tier's large caps stand.
- A price older than 24 hours, or dated in the future, is refused
  (`pool.rs:133-140`). The 24-hour rule is stock; it matters to a testnet
  deployment that outlives a day (§2.F).

### 1.5 What the release does not ship

Precept's release carries the pool, backstop, factory and oracle adapter,
but no Comet and no mock oracle. Comet stays on its current pin:
`COMET_WASM_SHA256` (`5d721fa5…`) is exactly the Comet hash Precept's own
testnet deployments record in `registry/testnet.jsonl`. The script3
`mock_sep_40_oracle` stays as the pool's oracle: it reports 7 decimals and
needs no keeper. Precept's live markets price through
`pool_oracle_adapter` over a proxy-oracle runtime the release does not
ship; neither tier deploys that stack (§5).

### 1.6 Protocol

Precept builds against `soroban-sdk =25.3.0`, so its wasm needs a network
on protocol 25 or later. Public testnet answers protocol 30. The pinned
quickstart image's standalone network has not been checked, and is the
first thing the implementation verifies (§2.A).

## 2. Design

### A. Pins

`scripts/sandbox/versions.env`:

- `POOL_WASM_URL`, `BACKSTOP_WASM_URL` and `POOL_FACTORY_WASM_URL` point at
  `https://github.com/Templar-Protocol/precept/releases/download/v0.1.1/`
  `pool.wasm`, `backstop.wasm` and `pool_factory.wasm`; their `_SHA256`s are
  the release's `SHA256SUMS` lines, each confirmed against GitHub's own
  asset digest — two independent records that must agree:

  | File | Bytes | SHA-256 |
  |---|---:|---|
  | `pool.wasm` | 49520 | `acf7a128074d4d9c34b4ac4e8ab46cc1148eb353eb3908ad0faf922851728086` |
  | `backstop.wasm` | 18080 | `41608bcb5176fdfdc74d4594be7ea538e93bc32bda24bc537cc1cd9e44e650d1` |
  | `pool_factory.wasm` | 3433 | `47956acea8b2ca37ea83e4153fd592609f430ddc4899eca7d7878c6bf1cfbe65` |

- `BLEND_CONTRACTS_TAG`/`BLEND_CONTRACTS_COMMIT` become the record of where
  Comet alone comes from, and a `PRECEPT_RELEASE_TAG` names the release the
  other three come from, with a comment that `v0.1.2` ships the same bytes.
- Every key name the workflows, `post-create.sh` and
  `scripts/check-repo-invariants.sh` read is unchanged.
- **First implementation step:** start the pinned quickstart image and read
  `protocolVersion` from its `getLatestLedger`. At 25 or later, nothing
  moves. Below 25, bump `QUICKSTART_TAG`/`QUICKSTART_DIGEST` by the
  procedure `versions.env` already documents (the manifest-list digest),
  in the same PR.

`fetch-artifacts.sh` and both `deploy.sh` scripts name the files by their
new basenames. The fetch-and-verify mechanism itself does not change.

### B. The market

Both reserves copy the live XLM/USDC market's `config` from
`registry/configs/xlm-usdc.json`; the pool keeps `max_positions = 4` and
`min_collateral = 0`, which the live market also uses.

| Field | XLM (index 0) | USDC (index 1) |
|---|---:|---:|
| `decimals` | 7 | 7 |
| `c_factor` | 8000000 (0.8) | 0 |
| `l_factor` | 0 | 10000000 (1.0) |
| `util` | 8000000 | 9000000 |
| `max_util` | 9000000 | 9900000 |
| `r_base` | 1000 | 1000 |
| `r_one` | 0 | 799000 |
| `r_two` | 0 | 1200000 |
| `r_three` | 0 | 1200000 |
| `reactivity` | 0 | 0 |
| `supply_cap` | i128::MAX | i128::MAX |
| `min_position` | 500000000 (50 XLM) | 10000000 (1 USDC) |
| `enabled` | true | true |

XLM is collateral-only and USDC borrow-only, exactly as live. The live
market lists USDC at index 0; this tier keeps XLM at 0 and USDC at 1, the
order every existing assertion and `sandbox.env` key already assumes. Index
order carries no contract semantics, and the bot reads indices from chain.

`reserve_metadata`'s four positional arguments give way to two literal
`ReserveConfig` JSON objects, one per reserve, every field written out, so
neither reserve's row can silently inherit the other's.

### C. Scenario numbers

The borrower keeps 5,000 XLM of collateral and borrows **340 USDC**, where
today it borrows 300:

| | XLM price | Effective collateral | Effective debt | Health factor |
|---|---:|---:|---:|---:|
| deployed | $0.10 | 5,000 × 0.10 × 0.8 = $400 | 340 / 1.0 = $340 | ≈ 1.18 |
| after `crash.sh` (default) | $0.075 | $300 | $340 | ≈ 0.88 |

So the scenario stays one 25% price move wide and `crash.sh`'s default of
`750000` stands. At 300 USDC the same crash would reach exactly 1.0, which
is not under `LIQ_HF_THRESHOLD`'s 0.998.

Everything downstream of those numbers is **measured, never assumed**,
exactly as the existing fill-budget gotcha requires: the partial percent
the auctioneer creates and `FILL_LEDGERS` are read from the first green
sandbox run under the new market and written into the harness and
CLAUDE.md with that run as their source. As an estimate only: the contract's
incentive for this market is `(1 − 0.8 / 1) / 2 + 1 = 1.1`, so the lot
covers the bid plus the pool's 100 bps margin around ledger
`200 × 1.01 / 1.1 ≈ 184`, and `FILL_LEDGERS` is expected near 200.

`MIN_PRIMARY_COLLATERAL` (100 XLM) is above XLM's `min_position`, so
`validate_primary_asset` accepts it and every unwind can rest on it.

The five existing scenarios keep their purpose:

- `liquidation`: crash, partial auction, `earliest-profitable` fill, unwind.
- `unwind_repay`: the filler holds no USDC, so the fill leaves it a USDC
  liability, and on this market its health then needs a supply of its
  primary, XLM — the first time `plan_fill`'s `min_position` top-up meets
  real contract code. The repay branch after `mint.sh` is unchanged.
- `dry_run`, `restart_adopt`: logic unchanged; numbers follow §2.C.
- `check_config`: gains case (i), §2.E.

### D. New scenario: `free_fill_default`

**Why.** `free-fill` is `PoolConfig::fill_objective`'s default, and no
scenario uses it: all five set `earliest-profitable` to fit their time
budget. A free fill of a 100% auction is also the ordinary way a Precept
borrower's leftover debt is defaulted, which makes this the scenario that
puts `defaulted_debt` and `debt_setoff`, with their `v0.1.1` topics, in
front of the bot from a real contract.

**Deploy.** As `liquidation`, plus: the borrower's step-9 submit adds a
**plain supply** (request type 0, `Supply`) of **40 USDC** after its
borrow. Plain supply, never `SupplyCollateral`: on this market USDC
collateral would carry zero weight but non-zero raw value, and
`check_and_handle_user_bad_debt` returns early for any position with raw
collateral (`pool/src/pool/bad_debt.rs:64-74`), so the default would never
run. Plain supply leaves the health factor unchanged. `deploy.sh` gates the
extra request on `SANDBOX_SCENARIO`, the way `unwind_repay` already gates
its skipped mint.

**Crash.** `crash.sh 500000` ($0.05): raw collateral is $250 against $340
of debt (health ≈ 0.59). No partial percent closes the excess, so
`plan_liquidation` exhausts both sides and answers the whole position at
100% (`src/math/liquidation.rs`, "exhausting both sides takes
everything"). Precept treats it as a full liquidation — every position, at
over 95% (`pool/src/auctions/user_liquidation_auction.rs:97`) — and since
the estimated withdrawal at 95% (340 × 0.95 × 1.1 ≈ $355) exceeds the $250
of collateral, its too-large check is skipped (`:186-190`).

**The bot.** The pool's `fill_objective` is left at the default,
`free-fill`, so the filler plans for ledger 400 after the auction's start,
where the bid modifier is zero (`pool/src/auctions/auction.rs:296-311`) and
the scaled bid is absent. The fill takes all 5,000 XLM and no liability.

**What the chain does in that one transaction.** `fill_user_liq_auction`
removes the lot from the borrower, leaving all 340 USDC of debt and no
collateral, and — the fill being full — runs
`check_and_handle_user_bad_debt` (`user_liquidation_auction.rs:206-222`):

1. the borrower's 40 USDC of supply sets off what it can, emitting
   `["debt_setoff", borrower, USDC]` with `(b_tokens_burned,
   d_tokens_repaid)`;
2. the ≈ 300 USDC residual is defaulted, emitting
   `["defaulted_debt", borrower, USDC]` with `d_tokens_burnt`, and cutting
   USDC's `b_rate`, which charges the admin's USDC liquidity for it
   (`bad_debt.rs:86-102`; `pool/src/events.rs:179-197`).

No collateral is left to orphan, so no `collateral_orphaned` is emitted.

**Assertions.**

- one `creations` row and one `fills` row, `dry_run = false`, each with a
  transaction hash; the auction's percent is 100;
- the fill landed at least 400 ledgers after the auction's start;
- the harness reads the fill's ledger through `getEvents` and finds both
  `debt_setoff` and `defaulted_debt`, each with the borrower in topic 1;
- the bot's log, across the whole run, holds no warning for an event it
  could not decode;
- the borrower's on-chain positions are empty and its `users` row is gone;
- USDC's `b_rate` after the fill is lower than before it;
- no `StockWasmDetected` was raised (`notifications_total` for that kind is
  zero), since no `bad_debt` event exists to raise it;
- the unwind left the filler's XLM inside
  `MIN_PRIMARY_COLLATERAL..=MAX_PRIMARY_COLLATERAL`, the band the other
  scenarios already assert, with no liabilities;
- `fills_total{result="succeeded"}` is 1.

**Budget.** About 400 ledgers between creation and fill. `FILL_TIMEOUT_CAP`
(10 minutes) becomes a per-scenario value: this scenario's is 15 minutes,
still compared up front against the rate `fill_budget` measures, so a slow
runner fails naming that rate rather than as "the filler never filled".
The workflow's 45-minute job timeout already covers a cold build, the
deploy and that budget; it moves only if a measured run says otherwise.

**The name.** `free_fill_default` is added in all five places the
scenario names are written — `deploy.sh`'s `sandbox_scenarios`, the
Makefile's `SANDBOX_SCENARIOS`, the harness's `SCENARIOS`, the nightly
matrix and an `#[ignore]`d test fn — which
`scripts/check-repo-invariants.sh` already holds equal.

### E. `check_config` case (i)

A pools config whose `min_primary_collateral` is 10 XLM — above zero and
under XLM's 50 XLM `min_position` — exits `2`, naming `min_position`, and
sends nothing. The unit tests already pin `validate_primary_asset`'s rule;
this case pins that the decoder reads `min_position` from a real `v0.1.1`
`ReserveConfig` at the right field position, which a field-order bug would
otherwise get past silently, decoding `enabled`'s slot or `supply_cap`'s.

### F. The testnet tier

`scripts/testnet/deploy.sh` makes the same changes as §2.A–C: Precept
pins, take rate 0, the §2.B reserves, the 340 USDC borrower. Its upload
preflight message names `soroban-sdk 25` and testnet's protocol 30.
`crash.sh` keeps `750000` as its default. `run-bot.sh --armed` keeps
`fill_objective = "earliest-profitable"`, so an armed pass fits its hour.

**Stage 2 is re-run** on the new deployment: any `target/testnet/testnet.env`
from the stock soak is moved aside, never deleted; then deploy, run armed,
crash, and wait for the creation, the fill and the unwind back to zero
liabilities; then `examples/soak_report.rs` captures the evidence.
`docs/testnet-soak.md` records it as a new dated Stage 2 result beside the
existing one, which is relabelled as run against stock Blend v2.

**Stage 1 is not re-run.** Its recorded results are relabelled as stock
too, with a note that Precept's testnet rehearsal pool (`CDPHII5C…`) is the
candidate for re-running it once it is priced and has borrowers — on
2026-11-09 it had neither.

The soak document gains one line on §1.4's 24-hour rule: a deployment
older than a day needs `crash.sh` re-run at the price it should hold
before anything in the pool is priced again.

This is the one step of the change that signs transactions on a public
network, as Phase 9's stage 2 did.

## 3. Verification

- `make check`, the unit tests at `--test-threads=2` per CLAUDE.md, and the
  three shell tests under `scripts/sandbox/`.
- `make sandbox`: all six scenarios green locally, the run that also
  measures §2.C's numbers.
- `gh workflow run sandbox.yml --ref precept/sandbox-testnet`: the nightly
  matrix on GitHub's amd64 runners before merge.
- The recorded testnet Stage 2 run, §2.F.

## 4. Documentation

- **CLAUDE.md**: the source-of-truth paragraph's "the sandbox tier below
  still pins stock wasm"; five scenarios become six wherever counted; the
  "scenario is exactly one price move wide" and "fill budget is measured"
  gotchas take §2.C's numbers; the fork gotcha on `bstop_rate` becomes
  resolved, with `1300` named; the module-map entries for
  `scripts/sandbox/`, `scripts/testnet/`, `tests/liquidation_sandbox.rs`
  and `tests/sandbox_harness/mod.rs`.
- `docs/architecture.md`'s description of the tiers; `docs/testnet-soak.md`
  (§2.F); the Makefile's help text and `sandbox.yml`'s step names, which say
  "Blend v2".
- `CHANGELOG.md` `[Unreleased]`: under **Changed**, both tiers deploy
  Precept `v0.1.1` wasm on the live XLM/USDC market's shape; under
  **Added**, the `free_fill_default` scenario and `check_config` case (i).
- A dated note at the head of the 2026-09-20 spec's §6 and on the
  2026-11-09 spec's "next change" line, pointing here.

## 5. Risks, and the rule for each

1. **The quickstart image is under protocol 25.** Bump it (§2.A), in this
   change.
2. **The new market moves a scenario's numbers unexpectedly.** The first
   sandbox run is the measurement; numbers are derived from it, and an
   assertion is never loosened to fit one.
3. **A slow runner for `free_fill_default`.** The up-front budget check
   fails naming the measured rate.
4. **The sandbox exposes a bot bug against real Precept code.** A small,
   clearly scoped fix lands in this change, test-first, with a unit test
   that fails without it. Anything larger stops the change and goes back to
   the maintainer rather than being folded in.
5. **Testnet's friendbot or a testnet reset.** The scripts already retry
   funding and name the failure; a reset means rebuilding stage 2 from
   scratch, which `docs/testnet-soak.md` already describes.

## 6. Out of scope

- `Tracker::refresh` holding a pool's events cursor while one of its
  reserves is unpriced (the 2026-11-09 spec's §5 follow-up). It is the next
  change, not this one; `v0.1.2`'s `oracle_tripper` makes an unpriced
  reserve an operator's deliberate act, which is why it comes next.
- `pool_oracle_adapter`, the proxy-oracle runtime and feed tripping: no
  tier deploys them, so neither the adapter's staleness rules nor a tripped
  feed is exercised here.
- Observing Precept's own testnet pools (stage 1).
- The `v0.2.0` release.
