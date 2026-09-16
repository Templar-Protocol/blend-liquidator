# Phase 6a: unwind — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After a fill lands, the bot repays the debt it took over from its wallet and withdraws the collateral it received — everything but the primary asset, and the primary down to its floor — while its own health factor stays at or above the pool's minimum; it repeats until a pass has nothing left to move, and says so once per pool when debt is left that the wallet cannot repay.

**Architecture:** `math::unwind` is pure: given the filler's positions, its wallet and the pool's accrued reserves and prices, it builds the request list spec §5's "Unwind" subsection describes, in that order, projecting every withdrawal exactly. `unwind.rs` is the I/O around it, run inside the filler task after the tick's fill walk: one snapshot of the filler's position, a wallet read, the plan, and a submission through the same executor and queue the fills use — so an unwind is behind the fills the tick submitted by the queue's own ordering. `notifier.rs` lands the `NotificationChannel` trait, a log-only channel and the deduplicating `Notifier` shell, which is what "one notification per pool" needs; Telegram, the semaphore and `drain()` are Phase 6b's.

**Tech Stack:** Rust 1.97.0, tokio, sqlx 0.9, stellar-xdr 28, clap 4, tracing, thiserror, ethnum.

**Spec:** `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md` — section 5 ("Unwind"), section 6 (`FAILURE_NOTIFICATION_COOLDOWN_HOURS`), section 7 ("Notifications"), section 8 ("Queues": unwinds retry 2; "Executor"), section 1 (the capital model: "Unwind to the wallet and hold"), section 12.

**Decided with the user on 2026-09-16:** Phase 6 is two pull requests. This is 6a, unwind alone, wired to a log-only notification channel. 6b lands Telegram, metrics and the HTTP endpoints.

## Global Constraints

- `clippy::pedantic` under `cargo clippy --all-targets -- -D warnings`; `unwrap`/`expect` only in `#[cfg(test)]` code.
- No `as` numeric casts. 3-digit digit grouping on numeric literals. Doc comments state constraints and invariants, never a narration of what changed.
- Structured `tracing` in the crate, never `println!`.
- **Money is never `f64`.** Config decimals go through `Decimal7`.
- **Secrets arrive through the environment, never as command-line arguments**; **a secret never reaches a `Debug` rendering or a log line**; `Submission`'s hand-written `Debug` never prints the operation.
- **`DRY_RUN` defaults to `true`**, strict parser. **Dry-run never signs or sends**: `Submitter::simulate_only` is the only simulation a dry run may make; `prepare` signs and may restore. The executor's mode guards — a queue offered to a dry-run or signer-less executor is refused before anything is simulated, recorded or enqueued — bind every new submission path exactly as they bind fills.
- **Nothing is sent for a key while an earlier transaction's outcome on it is unknown**; the queue owns that, and a new submission kind changes nothing about it.
- **A reservation is settled by value exactly once**, consumed when the transaction landed or may have (`Succeeded`, `Unknown`), released otherwise.
- The tracker owns every column of an `auctions` row but `fill_ledger`/`percent`; unwind touches no store table at all.
- **The three-way Rust version pin**; **`CI Summary` treats a skipped job as a failure**; **the mainnet fixture is immutable**.
- Compile-time checked queries only; no query changes are expected in this phase, so `.sqlx/` should not change — `cargo sqlx prepare --check` in CI says if it must.
- **Arithmetic on chain-sourced values is checked, or proven safe in a comment — never silently saturated** (the inventory ledger is the one sanctioned exception).
- Build with `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0` and a running database (`make db-up`; `DATABASE_URL=postgres://liquidator:liquidator@127.0.0.1:55432/liquidator` for bare `cargo`).
- **Counting tests:** `#[test]` + `#[tokio::test]` + `#[sqlx::test]`. The suite on `main` at `233c33b` is 423 (204 + 77 + 142).

---

## Facts the code is written against

Verified against the tree at `233c33b` and `blend-contracts-v2` at `v2.0.0`.

### The contract, for the requests an unwind sends

- `Repay`: `d_tokens_burnt = to_d_token_down(amount)`; when that exceeds the debt the whole debt is burnt and `amount − to_asset_from_d_token(debt)` is refunded in the same call; the spender still transfers the whole `amount` first, so the wallet must hold all of it.
- `WithdrawCollateral`: `to_burn = to_b_token_up(amount)`, capped at the position; an amount above the position withdraws exactly the position (`WITHDRAW_ALL = i64::MAX` is the executor's "all", safe against the contract's arithmetic). Sets `check_health`. Requires the reserve's utilisation to stay under 100% (`InvalidUtilRate`, 1207) — the simulation is what catches that.
- `validate_submit`, after every request: when `check_health && from.has_liabilities()`, `InvalidHf` (1205) if the health factor is under `1_0000100`, else `MinCollateralNotMet` (1224) if `collateral_base < min_collateral`. **A position left with no liabilities is not health-checked at all**, so an unwind that repays everything may withdraw everything.
- Withdrawing never raises the position count, so `MaxPositionsExceeded` is unreachable from an unwind.

### Interfaces this phase consumes, verbatim

```rust
// src/math (re-exports) — see Phase 5's plan for the full list; the unwind builder uses:
pub struct Positions { pub collateral: BTreeMap<u32, i128>, pub liabilities: BTreeMap<u32, i128>, pub supply: BTreeMap<u32, i128> }
pub struct PositionData { pub collateral_base: i128, pub collateral_raw: i128, pub liability_base: i128, pub liability_raw: i128, pub scalar: i128 }
impl PositionData { pub fn health_factor(&self) -> Result<Option<i128>, MathError>; pub fn is_hf_under(&self, min: i128) -> Result<bool, MathError>; }
pub fn calculate_position_data(reserves: &BTreeMap<u32, Reserve>, prices: &OraclePrices, positions: &Positions) -> Result<PositionData, MathError>;
impl Reserve {
    pub fn to_asset_from_d_token(&self, d_tokens: i128) -> Result<i128, MathError>; // rounds up
    pub fn to_asset_from_b_token(&self, b_tokens: i128) -> Result<i128, MathError>; // rounds down
    pub fn to_effective_asset_from_b_token(&self, b_tokens: i128) -> Result<i128, MathError>; // ×c_factor, down
    pub fn to_d_token_down(&self, amount: i128) -> Result<i128, MathError>;
    pub fn to_b_token_up(&self, amount: i128) -> Result<i128, MathError>;
}
pub fn mul_floor / mul_ceil / div_floor / div_ceil(x: i128, y: i128, denominator: i128) -> Result<i128, MathError>;
pub const SCALAR_7: i128 = 10_000_000;
// src/math/fill.rs: private `reserve_for`, `add_to`, `REPAY_ALLOWANCE_BPS = 1`, `BPS = 10_000` — the unwind builder needs the same; Task 1 says how they are shared.

// src/executor.rs
pub const WITHDRAW_ALL: i128 = 9_223_372_036_854_775_807;
pub struct Executor<'a> { /* store, submitter: Option<Submitter<'a>>, dry_run */ }
impl<'a> Executor<'a> { pub fn new(store: &'a Store, submitter: Option<Submitter<'a>>, dry_run: bool) -> Self; pub fn filler(&self) -> Option<&str>; }
enum Settle { Consume, Release }               // private
enum Judged { Accepted(Operation), Refused(ExecOutcome), NeedsRestore(Operation), Unsimulated } // private
fn refusal(contract_error: Option<u32>) -> ExecOutcome; // private: 1205/1224 → Replan, else Refused

// src/inventory.rs
impl Inventory { pub fn available(&self) -> BTreeMap<String, i128>; pub fn reserve(&self, amounts: &BTreeMap<String, i128>) -> Result<Reservation, InventoryError>; pub fn stale(..); pub fn record_balances(..); }
impl Reservation { pub fn consume(self); pub fn release(self); }
pub enum Settlement { Live(Reservation), DryRun }
pub async fn read_balances(reader: &PoolReader<'_>, account: &str, assets: &BTreeSet<String>) -> Result<BTreeMap<String, i128>, ChainError>;

// src/queue.rs
pub struct Submission { pub operation: Operation, pub priority: Priority, pub label: String, pub retries: u32 }
pub const CREATION_RETRIES: u32 = 3; pub const FILL_RETRIES: u32 = 10;
impl SubmissionQueue { pub async fn enqueue(&self, submission: Submission) -> Result<TxOutcome, QueueError>; }

// src/filler.rs — as landed in Phase 5
pub struct Filler<'a> { rpc, store, pools: &'a [PoolConfig], config: FillerConfig, executor: Executor<'a>, inventory: Inventory } // fields private
impl<'a> Filler<'a> {
    pub fn new(rpc: &'a RpcClient, store: &'a Store, pools: &'a [PoolConfig], config: FillerConfig, executor: Executor<'a>, inventory: Inventory) -> Self;
    pub async fn tick(&self, state: &mut FillerState, tick: LedgerTick, execute: bool, queue: Option<&SubmissionQueue>, shutdown: &watch::Receiver<bool>) -> Result<TickSummary, FillerError>;
}
pub struct FillerState { /* last_planned, recorded_dry_run, inventory_stale, known_assets, covered_assets */ } // Default
pub struct TickSummary { pub planned: u32, pub executed: u32, pub skipped: u32, pub closed: u32 }
// private: `refresh_inventory(&self, reader, snapshot, state)`, `note_recorded(..)` sets `inventory_stale` on Succeeded|Unknown and returns `landed`;
// `tick_pool` ends the pool's walk when a fill landed.

// src/config.rs
pub struct PoolConfig { pub address, pub primary_asset: String, pub min_primary_collateral: i128 /* underlying */, pub min_health_factor: i128 /* 7dp */, .. }
pub struct ServiceConfig { .., pub dry_run: bool, pub inventory_refresh: Duration, pub xlm_fee_reserve: u64, pub startup_delay_ledgers: u32, .. }

// src/service.rs — private
fn spawn_filler(tasks, rpc, store, signing: &SigningContext, config: FillerConfig, pools: Vec<PoolConfig>, xlm_fee_reserve: u64, startup_delay_ledgers: u32, queue: Option<SubmissionQueue>, tick_rx, shutdown);
async fn filler_loop(filler: &Filler<'_>, startup_delay_ledgers: u32, queue: Option<&SubmissionQueue>, tick_rx, shutdown) -> Result<(), LiquidatorError>;
```

### Test scaffolding that exists

`src/harness.rs` (`pub(crate)`): `POOL`, `USER_ONE`, `USER_TWO`, `script_snapshot(rpc, accounts)`, `script_auction_entry[_in]`, `script_no_auction`, `fixture_tick`. `src/chain/script.rs` (`pub(crate)`): `ScriptedRpc`, `script_simulate_prelude`, `script_prepare_prelude`, `script_simulate_accepted`, `script_simulate_refused`, `script_simulate_needs_restore`, `script_send`, `script_transaction_success`, `script_transaction_not_found`, `scval_b64`, `transaction_data_b64`, `result_b64`, `meta_v4_b64`. Private to `src/filler.rs`'s tests, and promoted by Task 4: `positions_entry_xdr`, `script_snapshot_positions(rpc, &[(account, positions_xdr)])`, `instance_entry_xdr(pool)`, `contract_entry_xdr`, `entry`, `script_empty_wallet(rpc, ledger)`, `simulation(return_xdr, ledger)`, `filler_signer()`, `tx_config()`. `src/math/fill.rs`'s `plan_tests` module builds a three-reserve pool by hand (XLM at $0.10, factors 0.75; USDC at $1, factors 0.95; a $1 reserve with no collateral factor; every rate exactly one) — Task 1 reuses that shape.

---

## File structure

| File | Responsibility |
|---|---|
| `src/math/unwind.rs` (create) | Pure. The repay-and-withdraw request builder, spec §5's three steps, every withdrawal projected exactly. |
| `src/math/fill.rs` (modify) | `reserve_for`, `add_to`, `REPAY_ALLOWANCE_BPS`, `BPS` become `pub(crate)` so both builders share one repay rule. |
| `src/math/mod.rs` (modify) | `pub mod unwind;`. |
| `src/notifier.rs` (create) | `NotificationKind`, `Severity`, `Notification`, the `NotificationChannel` trait, `LogChannel`, and `Notifier` (dedup by `(pool, account, kind)` with a cooldown). |
| `src/unwind.rs` (create) | The pass: snapshot, wallet, plan, execute, notify leftovers; `UnwindState`. |
| `src/executor.rs` (modify) | `Executor::unwind`; the judge and submit internals shared with `execute`. |
| `src/queue.rs` (modify) | `UNWIND_RETRIES = 2`. |
| `src/filler.rs` (modify) | Runs the unwind passes after the fill walk; marks a pool pending when a fill lands; seeds every pool pending on the first tick. |
| `src/config.rs` (modify) | `FAILURE_NOTIFICATION_COOLDOWN_HOURS`. |
| `src/service.rs` (modify) | Builds the `Notifier` and hands it to the filler task. |
| `src/harness.rs` (modify) | The promoted scripting helpers. |
| `src/liquidator.rs` (modify) | Module declarations; `LiquidatorError::Unwind` if `unwind.rs` needs its own error (it does not: see Task 4). |
| `CLAUDE.md`, `README.md`, `CHANGELOG.md`, `.env.example` (modify) | Task 6. |

## Rulings taken before execution

1. **The builder is pure and lives in `math::unwind`; the pass is I/O in `unwind.rs`** — Phase 4's and 5's ruling 1, for the same reason: the money arithmetic needs golden tests.
2. **Unwind runs inside the filler task, after the tick's fill walk, not as a queued operation.** Spec §5 says an `Unwind { pool }` "is queued behind pending fills". An operation built and queued behind a fill would be planned from state the fill it waits behind is about to change, and the executor's own rule for fills — never send a stale plan — applies. Planning at the moment of submission, after the tick's fills, and submitting on the same per-key queue puts the unwind behind those fills by the queue's ordering, which is what the spec wants from "behind". It also lets the unwind share the filler's inventory and reservations.
3. **A pool becomes unwind-pending when a fill in it lands (`Succeeded` or `Unknown`), and every pool is pending once at startup.** A restart after a fill must not strand the position; an idle pass costs one snapshot and one wallet read and sends nothing. Because spec §5 step 2 withdraws the primary down to `min_primary_collateral`, the startup pass trims any primary collateral above the floor to the wallet — the capital model spec §1 states ("unwind to the wallet and hold"); Task 6 documents it loudly.
4. **A pass repeats every tick while it moves something, and stops at the first pass that builds no requests.** A refused or failed submission keeps the pool pending; the next tick re-plans from fresh state.
5. **Repay amounts are what the fill planner uses:** the debt in underlying plus a 1 bp allowance plus one unit, capped at what the wallet has available — net of the fee reserve and open reservations — never the raw balance. The contract refunds excess. Spec §5's "with the wallet balance" is read as "with what the wallet can spend".
6. **The unwind floor is the pool's `min_health_factor` alone**, as spec §5 step 3 says; `HF_SAFETY_MULTIPLIER` is the *fill's* margin. *Corrected during review:* "alone" was wrong about the contract. `validate_submit` checks `collateral_base < pool.config.min_collateral` after every health-checked request of a position that keeps liabilities, whichever way that request moved it — the check `math::fill` already honours in `healthy` and `supply_for` — so `UnwindTerms` carries `min_collateral` and every step-3 acceptance holds it too. The mainnet pools set it to $5, which is *above* the health target in exactly the leftover-debt case this phase exists for, and the plan the contract could only refuse would then have been retried every tick with nothing notified. Step 2 is unaffected: with no liabilities the contract checks neither bound. `HF_SAFETY_MULTIPLIER` is still the fill's alone. `CLAUDE.md` states the current rule; this ruling is kept as written for the record.
7. **The 1%-of-floor dust rule applies to every partial withdrawal of the primary, in step 2 as well as step 3**, and `WithdrawAll` of a non-primary asset is never dust. The 0.5% rule ends step 3's search: once the projected health factor is within 0.5% of the minimum, nothing further is withdrawn. *Corrected during review:* that is where the margin stops *starting* a candidate, and it was all the implementation did with it — within one candidate the withdrawal targeted the bare `min_health_factor`, so an unwind that left debt rested the filler's own position exactly on the operator's minimum. The next pass then found it inside the margin, went idle, cleared the pool from pending, and the interest on that residual debt carried it under the minimum with nothing scheduled to look again. The margin is now the resting point everywhere in step 3 — the pre-candidate stop, the partial's target, the projection's verification and the whole-position branch. `CLAUDE.md` states the current rule; this ruling is kept as written for the record.
8. **Withdrawal amounts are found by formula and verified by exact projection**, backing off in bounded steps when the projection disagrees — the shape the fill planner's supply uses.
9. **There is no unwind audit table.** Spec §4 lists none. The structured log events `unwind planned` and `unwind submitted` are the record, and a dry run logs its plan and sends nothing.
10. **An unwind's repays reserve wallet amounts exactly as a fill's do**, settled by the outcome: consumed on `Succeeded`/`Unknown`, released otherwise, with `Settlement::DryRun` in dry-run.
11. **Leftover liabilities notify once per pool**: after an idle pass that leaves debt the wallet cannot repay, one `UnwindLeftovers` notification per pool, high severity, and not again until a later pass finds the pool clean and a subsequent one finds leftovers anew. The `Notifier`'s cooldown dedup is the second guard.
12. **`notifier.rs` in this phase is the trait, `LogChannel`, and dedup with `FAILURE_NOTIFICATION_COOLDOWN_HOURS`.** Telegram, the bounded in-flight semaphore and `drain()` are 6b's, together with the other notification kinds' call sites; the enum lists spec §7's kinds now so 6b adds no variant.
13. **`UNWIND_RETRIES = 2`** (spec §8), `Priority::Normal`.
14. **The unwind pass reads its own snapshot**, even for a pool the fill walk just read: a fill landed in between, and the pass exists to act on the position that fill changed.

---
15. **Files:** the pure builder is `src/math/unwind.rs` (`math/` is where this repo keeps pure planners, as Phases 4 and 5 ruled for `liquidation` and `fill`); the pass is an `impl Filler` block in `src/filler.rs`, because it shares the filler's inventory, executor, wallet-refresh logic and per-tick state; there is no top-level `unwind.rs`. The spec's map names one; `CLAUDE.md` says where the two halves live.

---

## Task 1: the repay-and-withdraw builder

**Files:**
- Create: `src/math/unwind.rs`
- Modify: `src/math/fill.rs` (`reserve_for`, `add_to`, `REPAY_ALLOWANCE_BPS`, `BPS` become `pub(crate)`, unchanged otherwise; `FillInputs` stays as it is)
- Modify: `src/math/mod.rs` (`pub mod unwind;`)
- Test: inline in `src/math/unwind.rs`

**Interfaces:**
- Consumes: `Positions`, `PositionData`, `calculate_position_data`, `Reserve`, `OraclePrices`, `mul_floor`/`mul_ceil`/`div_ceil`, `SCALAR_7`, `MathError`; the four `pub(crate)` items from `fill.rs`.
- Produces:
  ```rust
  pub struct UnwindTerms { pub primary_asset: String, pub min_primary_collateral: i128, pub min_health_factor: i128 } // Debug, Clone, PartialEq, Eq
  pub struct UnwindInputs<'a> { pub reserves: &'a BTreeMap<u32, Reserve>, pub asset_index: &'a BTreeMap<String, u32>,
      pub prices: &'a OraclePrices, pub positions: &'a Positions, pub wallet: &'a BTreeMap<String, i128> } // Debug, Clone, Copy
  pub enum UnwindAction { Repay { asset: String, amount: i128 }, Withdraw { asset: String, amount: i128 }, WithdrawAll { asset: String } } // Debug, Clone, PartialEq, Eq
  pub struct UnwindPlan { pub actions: Vec<UnwindAction>, pub spend: BTreeMap<String, i128>,
      pub remaining_liabilities: Vec<String>, pub projected_health: Option<i128> } // Debug, Clone, PartialEq, Eq
  impl UnwindPlan { pub fn is_idle(&self) -> bool; }
  pub const HEALTH_MARGIN_BPS: i128 = 50;   // step 3 stops within 0.5% of the minimum
  pub const DUST_FLOOR_BPS: i128 = 100;     // a primary withdrawal under 1% of the floor is not made
  pub fn plan_unwind(terms: &UnwindTerms, inputs: &UnwindInputs<'_>) -> Result<UnwindPlan, MathError>;
  ```

Spec §5's algorithm, which `plan_unwind`'s doc carries:

1. **Repay** each liability asset the wallet holds: the debt in underlying (`to_asset_from_d_token`, rounded up) plus a 1 bp allowance plus one unit, capped at what the wallet has for that asset; project the burn (`to_d_token_down(amount)`, capped at the debt) and note which liabilities remain. Every asset the wallet cannot fully repay goes in `remaining_liabilities`, in reserve-index order.
2. **No liabilities remain:** `WithdrawAll` every collateral asset but the primary, in reserve-index order; then the primary down to `min_primary_collateral` — a `Withdraw` of the excess underlying (`to_asset_from_b_token(b_tokens) − floor`), only when that excess is at least `DUST_FLOOR_BPS` of the floor (a floor of zero makes any excess enough, and a position entirely above a zero floor is a `WithdrawAll`). The projection is not consulted: with no liabilities the contract checks nothing.
3. **Liabilities remain:** withdraw while the projected health factor stays at or above `min_health_factor`. Candidates in this order: collateral assets that are also liabilities (reserve-index order), then the remaining non-primary collateral by ascending effective value (ties by index), then the primary. For each: try `WithdrawAll` (the primary's "all" is the excess above its floor) and take it if the projection holds; otherwise the largest partial amount the projection allows, found by formula — the collateral base the floor requires is `mul_ceil(liability_base, min_health_factor, SCALAR_7)`, what may go is the current base less that, converted back through the asset's price and collateral factor rounding *down*, never below the primary's floor — verified by projection and backed off by 1 bp of itself at most eight times when the projection disagrees, then given up on. Stop before any candidate when the projected health factor is already under `min_health_factor × (1 + HEALTH_MARGIN_BPS)`, and skip a primary withdrawal under `DUST_FLOOR_BPS` of the floor. *Corrected during review* (rulings 6 and 7): every step-3 acceptance is the pair `!is_hf_under(margin) && collateral_base >= min_collateral`, with `margin = mul_ceil(min_health_factor, BPS + HEALTH_MARGIN_BPS, BPS)` derived once per walk, and the partial's target is `mul_ceil(liability_base, margin, SCALAR_7).max(min_collateral)` — not the bare `min_health_factor` this paragraph names in three places. The kept text stands as the record of what was planned.

`spend` is the wallet amounts the repays take, per asset; `projected_health` is the projection after every action, `None` when no liabilities remain.

- [ ] **Step 1: Write the failing tests**

The pool `math::fill`'s `plan_tests` builds, copied (not shared — the two modules' fixtures may diverge): XLM at index 0, 7 decimals, price `1_000_000` (a 7-decimal oracle: $0.10), factors `7_500_000`; USDC at index 1, price `10_000_000`, factors `9_500_000`; `NOCF` at index 2, price `10_000_000`, `c_factor` 0, `l_factor` `10_000_000`; every rate exactly one, so tokens and underlying are the same number. Terms unless a test says otherwise: primary XLM, `min_primary_collateral` `100_000_000_000` (10,000 XLM), `min_health_factor` `15_000_000`.

Derivations to carry in each test's comment (an XLM b-token balance `B` is worth `⌊⌊B × 0.75⌋ / 10⌋` of collateral base; a USDC balance is worth `⌊B × 0.95⌋`; a USDC debt `D` costs `⌈D / 0.95⌉` of liability base):

```rust
/// Everything repaid, everything but the floor withdrawn. Debt of 1,000 USDC
/// (1e10) against a wallet of 2,000: the repay is 1e10 + ⌊1e10 / 10_000⌋ + 1
/// = 10_001_000_001, which burns the whole debt; with no liabilities left
/// the USDC collateral goes entirely and the XLM's excess over the floor,
/// 2e11 − 1e11 = 1e11 (≥ 1% of the floor), goes as a plain withdrawal.
#[test]
fn a_wallet_that_covers_the_debt_unwinds_to_the_floor() {
    // positions: collateral {0: 200_000_000_000, 1: 5_000_000_000}, liabilities {1: 10_000_000_000}
    // wallet: {USDC: 20_000_000_000}
    // expect actions == [Repay{USDC, 10_001_000_001}, WithdrawAll{USDC}, Withdraw{XLM, 100_000_000_000}]
    // spend == {USDC: 10_001_000_001}; remaining_liabilities empty; projected_health None
}

/// The wallet is short: 400 USDC repays 4e9 of the 1e10 debt, leaving 6e9
/// d-tokens = 6_315_789_474 of liability base. The floor 1.5 needs
/// 9_473_684_211 of collateral base. The USDC collateral is also a
/// liability, so it goes first and entirely (1.5e10 of XLM base remains,
/// enough); then the primary, last: keeping 9_473_684_211 of base means
/// keeping ⌈94_736_842_110 / 0.75⌉ = 126_315_789_480 b-tokens, so
/// 73_684_210_520 may go — above the floor, above the dust rule — and the
/// projection lands on exactly 15_000_000.
#[test]
fn a_wallet_short_of_the_debt_withdraws_within_the_floor() {
    // wallet: {USDC: 4_000_000_000}
    // expect actions == [Repay{USDC, 4_000_000_000}, WithdrawAll{USDC}, Withdraw{XLM, 73_684_210_520}]
    // remaining_liabilities == [USDC]; projected_health == Some(15_000_000)
}

/// The primary never goes below its floor: with a floor of 15,000 XLM the
/// health floor would allow 73_684_210_520 out but the primary floor allows
/// only 2e11 − 1.5e11 = 5e10.
#[test]
fn the_primary_never_goes_below_its_floor() {
    // positions: collateral {0: 200_000_000_000}, liabilities {1: 6_000_000_000}; wallet empty
    // terms.min_primary_collateral = 150_000_000_000
    // expect actions == [Withdraw{XLM, 50_000_000_000}]; remaining == [USDC]
}

/// Also-liability assets first, then the smallest, the primary last. A
/// no-collateral-factor lot (worth nothing) is the smallest position and
/// goes second; the primary goes third, down to what the floor allows.
#[test]
fn withdrawals_take_also_liabilities_then_the_smallest_then_the_primary() {
    // positions: collateral {0: 200_000_000_000, 1: 5_000_000_000, 2: 1_000_000_000}, liabilities {1: 6_000_000_000}; wallet empty
    // expect actions == [WithdrawAll{USDC}, WithdrawAll{NOCF}, Withdraw{XLM, 73_684_210_520}]
}

/// Within 0.5% of the minimum nothing more is withdrawn: 2e11 of XLM
/// against 9_481_000_000 of USDC debt (liability base 9_980_000_000) is a
/// health factor of ⌊1.5e10 × 1e7 / 9.98e9⌋ = 15_030_060, under the
/// 15_075_000 the margin sets — an idle pass with the debt remaining.
#[test]
fn a_position_within_the_margin_of_the_floor_is_left_alone() {
    // positions: collateral {0: 200_000_000_000}, liabilities {1: 9_481_000_000}; wallet empty
    // expect is_idle(); remaining == [USDC]; projected_health == Some(15_030_060)
}

/// A primary excess under 1% of the floor is not worth a transaction:
/// 100_500_000_000 against a floor of 1e11 is 5e8 of excess, under 1e9.
#[test]
fn a_dust_excess_over_the_floor_is_not_withdrawn() {
    // positions: collateral {0: 100_500_000_000}; no liabilities; wallet empty
    // expect is_idle()
}

/// A zero floor is "withdraw everything": no liabilities, the primary
/// entirely, as a `WithdrawAll`.
#[test]
fn a_zero_floor_withdraws_the_primary_entirely() {
    // positions: collateral {0: 200_000_000_000}; terms.min_primary_collateral = 0
    // expect actions == [WithdrawAll{XLM}]
}

/// Nothing to do: no positions, or only the primary at its floor.
#[test]
fn a_clean_position_is_an_idle_pass() {
    // Positions::default() → idle; collateral {0: 100_000_000_000} alone → idle
}

/// The repay never spends more than the wallet holds, and never repays an
/// asset the wallet does not hold.
#[test]
fn a_repay_is_capped_at_the_wallet() {
    // liabilities {1: 10_000_000_000}, wallet {USDC: 1}: Repay{USDC, 1}, remaining [USDC]
    // wallet empty: no Repay action at all
}

/// Whatever the inputs, a plan holds the floors: re-projecting the plan's
/// end state gives a health factor at or above the minimum (or no
/// liabilities), and the primary at or above its floor. A grid over
/// positions and wallets, including rates that are not one.
#[test]
fn every_plan_holds_the_floors() {
    // For b_rate/d_rate in {1.0, 1.0000223, 1.2287} (12 decimals), XLM
    // collateral in {1e11, 1.5e11, 2e11, 5e11}, USDC debt in {0, 6e9, 1e10,
    // 2e10}, USDC collateral in {0, 5e9}, wallet USDC in {0, 4e9, 2e10}:
    // plan; apply the actions to a copy of the positions (repays burn
    // to_d_token_down(amount) capped at the debt; withdrawals burn
    // to_b_token_up(amount) capped at the position; WithdrawAll clears the
    // entry); calculate_position_data; assert !is_hf_under(min) or no
    // liabilities, and the primary's underlying ≥ floor (or the primary
    // was never above it). Also assert the plan's projected_health equals
    // this re-projection's.
}

/// An asset the pool does not list is a bug upstream, not a plan.
#[test]
fn an_unknown_asset_is_refused() { /* positions with index 9 → Err(MathError::MissingReserve(9)) */ }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib math::unwind`
Expected: compile errors — the module does not exist.

- [ ] **Step 3: Write it**

The module doc states spec §5's three steps, rulings 5–8, and that every number is checked arithmetic. `plan_unwind` is the three steps over a cloned `Positions` and a cloned wallet; the private helpers, each with a doc stating its constraint:

- `repay(terms, inputs, positions, wallet, actions, spend) -> Result<(), MathError>` — step 1 over `positions.liabilities` in index order, the amount rule from `fill.rs` (`REPAY_ALLOWANCE_BPS`, `BPS`, `add_to`, `reserve_for`), removing an entry burnt to zero.
- `withdraw_free(terms, inputs, positions, actions)` — step 2.
- `withdraw_within_floor(terms, inputs, positions, actions) -> Result<Option<i128>, MathError>` — step 3, returning the final health factor. The candidate order is built once, before any withdrawal, from the post-repay positions. Each candidate: `project(inputs, positions)` for the current data; if `is_hf_under(min × (1 + margin))` → stop; try all (or the excess, for the primary); else the formula amount, verified, backed off.
- `primary_excess(terms, inputs, positions) -> Result<i128, MathError>` — underlying above the floor, or zero.
- `dust(terms, amount) -> bool` — `amount < mul_floor(min_primary_collateral, DUST_FLOOR_BPS, BPS)`.
- `project(inputs, positions) -> Result<PositionData, MathError>` — `calculate_position_data` on the working positions.

The unwind actions' amounts are underlying in the asset's own decimals; `WithdrawAll` carries no amount (the executor sends `WITHDRAW_ALL`). Where the primary's partial withdrawal is computed, comment the conversion chain: collateral base → effective underlying (`mul_ceil(base, scalar, price)`) → b-tokens (`div_ceil(effective, c_factor, SCALAR_7)`) is what must *stay*; what goes is the position less that, in b-tokens, then `to_asset_from_b_token` for the request's underlying.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib math::`
Expected: PASS. A hand-derived figure that disagrees with the code is re-derived from its comment before either is touched; report any you changed, with the derivation.

- [ ] **Step 5: Commit**

```bash
git add src/math/unwind.rs src/math/fill.rs src/math/mod.rs
git commit -m "feat(math): the repay-and-withdraw builder for unwinding a fill"
```

---

## Task 2: the notifier's trait, log channel and dedup

**Files:**
- Create: `src/notifier.rs`
- Modify: `src/config.rs` (`FAILURE_NOTIFICATION_COOLDOWN_HOURS`; `ServiceConfig::notification_cooldown: Duration`)
- Modify: `src/liquidator.rs` (`pub mod notifier;`)
- Test: inline

**Interfaces:**
- Produces:
  ```rust
  pub enum Severity { Low, Medium, High }                                  // Debug, Clone, Copy, PartialEq, Eq
  pub enum NotificationKind { AuctionCreated, BadDebtReported, FillConfirmed, FillFailed, SubmissionDropped,
      UnwindLeftovers, PollerStalled, RpcFailing, EventGap, UnfundedFill }  // Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord
  impl NotificationKind { pub fn as_str(self) -> &'static str; }           // snake_case labels, closed set (a metric label in 6b)
  pub struct Notification { pub kind: NotificationKind, pub severity: Severity, pub pool: String,
      pub account: Option<String>, pub message: String }                     // Debug, Clone
  pub enum NotifyError { Channel(String) }                                  // thiserror
  pub trait NotificationChannel: Send + Sync {
      fn name(&self) -> &'static str;
      fn send<'a>(&'a self, notification: &'a Notification) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>>;
  }
  pub struct LogChannel;                                                    // High → tracing::warn!, else tracing::info!, fields kind/pool/account/severity + message
  pub enum Delivery { Sent, Deduplicated, Failed }                          // Debug, Clone, Copy, PartialEq, Eq
  pub struct Notifier { /* channel: Box<dyn NotificationChannel>, cooldown: Duration, recent: Mutex<BTreeMap<(String, Option<String>, NotificationKind), Instant>> */ }
  impl Notifier {
      pub fn new(channel: Box<dyn NotificationChannel>, cooldown: Duration) -> Self;
      pub fn log_only(cooldown: Duration) -> Self;
      pub async fn notify(&self, notification: Notification) -> Delivery;              // = notify_at(.., Instant::now())
      pub async fn notify_at(&self, notification: Notification, now: Instant) -> Delivery;
  }
  ```
  The knob: `FAILURE_NOTIFICATION_COOLDOWN_HOURS`, `u64`, default `24`, `range(1..)` (zero would dedup nothing and is not "no cooldown" but "notify every tick"); `ServiceConfig::notification_cooldown: Duration`.

Spec §7 and §8: dedup by `(pool, account, kind)` with a cooldown; a channel failure never affects trading — `notify` never returns an error, it logs one and answers `Delivery::Failed`; the dedup entry is rolled back on failure so the next attempt is not suppressed by a send that never happened.

- [ ] **Step 1: Write the failing tests**

```rust
/// A recording channel for the tests: what it was asked to send.
struct Recording { sent: Mutex<Vec<Notification>>, fail: bool }

#[tokio::test] async fn the_first_of_a_kind_is_sent_and_the_next_within_the_cooldown_is_not() { /* Sent, then Deduplicated; one send recorded */ }
#[tokio::test] async fn a_different_pool_account_or_kind_is_its_own_key() { /* three variations, each Sent */ }
#[tokio::test] async fn the_cooldown_expires() { /* notify_at(now), notify_at(now + cooldown + 1s) → Sent, Sent */ }
#[tokio::test] async fn a_failed_send_is_reported_and_does_not_start_a_cooldown() { /* fail: true → Failed; then fail: false … the same key → Sent */ }
#[test] fn the_log_channel_names_the_kind() { /* LogChannel.name() == "log"; NotificationKind::UnwindLeftovers.as_str() == "unwind_leftovers" */ }
#[test] fn a_zero_cooldown_is_refused_at_parse() { /* Args --failure-notification-cooldown-hours 0 → Err; 1 → Ok; default 24 */ }
```

The "does not affect trading" property is the `notify` signature (no `Result`) plus `a_failed_send_is_reported_and_does_not_start_a_cooldown`.

- [ ] **Step 2: Run them and watch them fail** — `cargo test --lib notifier:: config::`
- [ ] **Step 3: Write it** — the module doc quotes spec §7's dedup and §8's "never affect trading"; `Notifier::notify_at` takes the lock, checks `recent`, inserts `now` before sending, sends, and on `Err` removes the entry and logs `warn!` with the channel's name and the error. The `Mutex` is `std::sync::Mutex`, never held across the `send` await (insert, drop the guard, send, re-lock to roll back).
- [ ] **Step 4: Run the tests** — `cargo test --lib notifier:: config::`
- [ ] **Step 5: Commit** — `git commit -m "feat(notifier): the channel trait, a log channel, and deduplication"`

---

## Task 3: the executor submits an unwind

**Files:**
- Modify: `src/executor.rs` (`unwind_requests`, `UnwindOutcome`, `Executor::unwind`; `judge` and `submit_recorded`'s chain-facing halves extracted into helpers both paths call)
- Modify: `src/queue.rs` (`UNWIND_RETRIES`)
- Test: inline in `src/executor.rs`

**Interfaces:**
- Consumes: Task 1's `UnwindAction`, `UnwindPlan`; `Settlement`, `Reservation`; `Submitter::simulate_only`; `submit_op`, `Request`, `RequestType`; `WITHDRAW_ALL`.
- Produces:
  ```rust
  // src/queue.rs
  pub const UNWIND_RETRIES: u32 = 2;
  // src/executor.rs
  pub fn unwind_requests(actions: &[UnwindAction]) -> Vec<Request>;
  pub enum UnwindOutcome { Planned { simulated: bool }, Submitted(TxOutcome), Refused { contract_error: Option<u32> }, Stale } // Debug
  impl UnwindOutcome { pub fn landed(&self) -> bool; }   // Submitted(Succeeded | Unknown)
  impl<'a> Executor<'a> {
      pub async fn unwind(&self, pool: &str, plan: &UnwindPlan, settlement: Settlement, queue: Option<&SubmissionQueue>) -> Result<UnwindOutcome, ExecutorError>;
  }
  ```

`Executor::unwind` is `execute` without the audit row: the same mode guards first (a dry-run executor given `Live` releases and fails; a live one given `DryRun` fails; a queue offered to a dry-run or signer-less executor fails), then the exact `submit` — `from`, `spender`, `to` all the filler — judged through `simulate_only` (any refusal is `Refused` with the code on a warn line: there is no lower percent to re-plan at; `NeedsRestore` is `Refused` in dry-run and proceeds armed), then a structured `tracing::info!` event `unwind planned` carrying the pool, the action count, the spend, `remaining_liabilities`, `projected_health`, `simulated` and `armed`; then, when a queue is given, the submission with `Priority::Normal` and `UNWIND_RETRIES`, label `unwind <pool>`, an `unwind submitted` info event with the hash and status, and the reservation settled by the outcome (consume on `Succeeded`/`Unknown`, release otherwise); `BadSequence` answers `Stale` and releases; a `Simulation` error at `prepare` answers `Refused` and releases. A dry run stops after the `unwind planned` event with `Planned { simulated }`.

The extraction: a private `judge_operation(&self, pool, account: Option<&str>, operation: &Operation, armed: bool, what: &'static str) -> Result<Judged, ExecutorError>` that both `judge` and `unwind` call (the fill's Replan/Refused split stays in `judge`, which maps the generic refusal through `refusal()`), and a private `enqueue(&self, queue, submission: Submission, describe: &dyn Fn(&TxOutcome)) -> (Result<TxOutcome, ExecutorError>, Settle)` … or whatever smaller shape keeps `execute`'s behaviour and all its tests unchanged. The constraint is that no fill test changes.

- [ ] **Step 1: Write the failing tests**

Each builds a `Store` (the executor holds one, though unwind never writes it), a `ScriptedRpc`, the test signer, and a literal `UnwindPlan` over the fixture's XLM and USDC addresses (`Repay USDC 10`, `WithdrawAll USDC`, `Withdraw XLM 7`), as `executor.rs`'s fill tests build their draft.

```rust
#[test] fn unwind_requests_map_each_action() { /* Repay→Repay(asset, amount); Withdraw→WithdrawCollateral(asset, amount); WithdrawAll→WithdrawCollateral(asset, WITHDRAW_ALL); order kept */ }
#[sqlx::test] async fn a_dry_run_unwind_is_planned_and_sends_nothing() { /* simulate prelude + accepted; Planned { simulated: true }; no getFeeStats, no sendTransaction; remaining()==0 */ }
#[sqlx::test] async fn a_keyless_dry_run_unwind_is_planned_unsimulated() { /* Executor::new(&store, None, true); Planned { simulated: false }; no RPC call */ }
#[sqlx::test] async fn a_live_unwind_is_submitted_at_normal_priority_with_its_budget() { /* stand-in worker records priority/retries/label, answers Succeeded; Submitted; reservation consumed (available down by the spend); label == "unwind <POOL>" */ }
#[sqlx::test] async fn an_unwind_the_chain_failed_releases_its_reservation() { /* Failed → released */ }
#[sqlx::test] async fn an_unknown_unwind_outcome_consumes_its_reservation() { /* Unknown → consumed */ }
#[sqlx::test] async fn a_refused_unwind_sends_nothing_and_releases() { /* script_simulate_refused(1207) → Refused { Some(1207) }; released; no send */ }
#[sqlx::test] async fn a_stale_unwind_is_replanned_not_resent() { /* worker answers BadSequence → Stale; released */ }
#[sqlx::test] async fn the_unwind_settlement_must_match_the_mode() { /* both mismatches → Err(Mode), reservation released, no RPC call */ }
#[sqlx::test] async fn a_queue_offered_to_a_dry_run_unwind_is_refused() { /* Err(Mode); nothing simulated/enqueued */ }
#[sqlx::test] async fn a_dry_run_unwind_that_needs_a_restore_is_refused() { /* NeedsRestore → Refused { None }; armed variant proceeds and is Submitted */ }
```

Every scripting test ends with `assert_eq!(rpc.remaining(), 0)`.

- [ ] **Step 2: Run them and watch them fail** — compile errors.
- [ ] **Step 3: Write it** — as above; keep every existing fill test green without edits.
- [ ] **Step 4: Run the tests** — `cargo test --lib executor:: queue::`, then `make check`.
- [ ] **Step 5: Commit** — `git commit -m "feat(executor): submit an unwind through the fill's guards and queue"`

---

## Task 4: the unwind pass in the filler task

**Files:**
- Modify: `src/filler.rs` (`FillerState` gains the unwind state; `Filler::new` takes the notifier; `tick` runs the passes after the fill walk; `note_recorded` marks the pool pending; the new `impl Filler` block with `unwind_pool`)
- Modify: `src/harness.rs` (the promoted helpers, `pub(crate)`, docs kept)
- Test: inline in `src/filler.rs`

**Interfaces:**
- Consumes: Task 1's `plan_unwind`/`UnwindTerms`/`UnwindInputs`/`UnwindPlan`; Task 2's `Notifier`, `Notification`, `NotificationKind::UnwindLeftovers`, `Severity::High`; Task 3's `Executor::unwind`, `UnwindOutcome`; the filler's own `refresh_inventory`, `PoolPass`-style context (a snapshot, accrued reserves, the filler's positions), `Inventory::{available, reserve}`, `Settlement`.
- Produces:
  ```rust
  impl<'a> Filler<'a> {
      pub fn new(rpc: &'a RpcClient, store: &'a Store, pools: &'a [PoolConfig], config: FillerConfig,
                 executor: Executor<'a>, inventory: Inventory, notifier: Arc<Notifier>) -> Self;   // one new parameter
  }
  pub struct TickSummary { /* existing four, plus: */ pub unwound: u32 }  // passes that submitted or, dry-run, planned a non-idle unwind
  // FillerState, private: unwind_pending: BTreeSet<String>, unwind_seeded: bool, leftovers_notified: BTreeSet<String>
  ```

The pass, per pending pool, after the tick's fill walk and only when the filler has an address to act as (a keyless dry run plans against an empty position, which is always idle — say so once at debug and skip the read):

1. Read a snapshot of the filler's account; its positions in the pool, empty → the pass is idle; clear the pool from `pending` and, if `leftovers_notified` held it, clear that too.
2. Refresh the inventory through the existing `refresh_inventory` (a landed fill has already flagged it stale).
3. `plan_unwind` with `UnwindTerms` from the pool config and `UnwindInputs` from the snapshot (reserves accrued at `snapshot.valued_at(tick.close_time)`, the filler's positions, `inventory.available()`).
4. An idle plan: remove the pool from `pending`. If `remaining_liabilities` is non-empty and the pool is not in `leftovers_notified`, notify `UnwindLeftovers` (high severity, pool, no account, a message naming the assets) and add it; if it is empty, remove the pool from `leftovers_notified`.
5. A non-idle plan: `execute` false (inside the startup delay) → leave it pending, log at debug; else `Settlement::DryRun` or `Live(inventory.reserve(&plan.spend))` — a refused reservation skips the pool this tick with a warning — then `Executor::unwind`. `Planned` (dry-run) → remove from `pending` (a dry run plans once per landing, not every tick) and count `unwound`. `Submitted` landed → keep pending, flag the inventory stale, count `unwound`; `Submitted` failed/expired, `Refused`, `Stale` → keep pending (the next tick re-plans from fresh state), log the reason. A non-`Store` error is one pool's: log and carry on.

`note_recorded` (a fill landed) inserts the pool into `pending`. The first tick inserts every configured pool once (`unwind_seeded`, ruling 3). The passes run in `tick` after the pool loop, in pool order, each checking the shutdown flag first.

- [ ] **Step 1: Promote the helpers**

Move `positions_entry_xdr`, `script_snapshot_positions`, `instance_entry_xdr`, `contract_entry_xdr`, `entry`, `script_empty_wallet`, `simulation`, `filler_signer` and `tx_config` from `src/filler.rs`'s test module to `src/harness.rs` as `pub(crate)`, bodies and docs unchanged; `filler.rs`'s tests import them. `cargo test --lib filler::` stays green.

- [ ] **Step 2: Write the failing tests**

The scripted snapshot puts the filler's positions on chain (`script_snapshot_positions`), the wallet reads answer balances, the executor is a keyed dry run or armed with a stand-in queue worker.

```rust
#[sqlx::test] async fn a_landed_fill_queues_an_unwind_of_its_pool() { /* armed; one due auction; worker answers Succeeded to the fill, then the unwind's submission; after the tick: summary.executed == 1, summary.unwound == 1, the second submission's label starts with "unwind"; remaining() == 0 */ }
#[sqlx::test] async fn every_pool_is_unwound_once_at_startup() { /* dry-run with a key; no auctions; the filler holds a position above the floor in the pool; first tick: unwound == 1 (planned); second tick: nothing read for it (not pending); remaining() == 0 */ }
#[sqlx::test] async fn a_keyless_dry_run_makes_no_unwind_read() { /* Executor::new(&store, None, true); first tick; no getLedgerEntries at all */ }
#[sqlx::test] async fn an_unwind_repeats_until_a_pass_is_idle() { /* armed; pending pool; tick one submits (Succeeded), tick two re-reads and submits again, tick three's plan is idle → pool no longer pending; unwound == 1, 1, 0 */ }
#[sqlx::test] async fn leftover_debt_notifies_once_per_pool() { /* a recording channel in the Notifier; idle plan with remaining liabilities on ticks one and two → one notification; a later tick whose pass finds no leftovers, then one that does again → a second notification */ }
#[sqlx::test] async fn an_unwind_inside_the_startup_delay_waits() { /* execute: false; pending pool; the plan is made (unwind planned event) but nothing submitted; still pending; unwound == 0 */ }
#[sqlx::test] async fn a_refused_unwind_stays_pending() { /* script_simulate_refused(1207); pool still pending after the tick; nothing sent */ }
#[sqlx::test] async fn an_unwind_the_wallet_cannot_fund_is_skipped_this_tick() { /* inventory below the plan's spend; skipped with a warning; still pending */ }
```

Assert `rpc.remaining() == 0` in every scripting test.

- [ ] **Step 3: Run them and watch them fail** — compile errors on `Filler::new`'s arity and the state fields.
- [ ] **Step 4: Write it** — the `impl Filler` block's doc states the pass and rulings 2–4, 9–11 and 14; `TickSummary::unwound`'s doc says what it counts.
- [ ] **Step 5: Run the tests and the suite** — `cargo test --lib filler::`, then `make check`.
- [ ] **Step 6: Commit** — `git commit -m "feat(filler): unwind a pool after a fill lands, and once at startup"`

---

## Task 5: wiring the notifier through the service

**Files:**
- Modify: `src/service.rs` (`Service::run` builds a `Notifier::log_only(config.notification_cooldown)` in an `Arc` and hands it to `spawn_filler`; `spawn_filler`/`filler_loop` thread it to `Filler::new`; the module doc's task list mentions the notifier)
- Test: inline in `src/service.rs` (the existing filler-loop tests gain the parameter; one new test)

**Interfaces:** none new. `spawn_filler` gains `notifier: Arc<Notifier>`.

- [ ] **Step 1: Write the failing test**

```rust
/// The filler loop unwinds a pool it holds a position in on its first
/// tick, dry-run: the `unwind planned` path runs off the published tick
/// like everything else in this task.
#[sqlx::test] async fn the_filler_loop_plans_a_startup_unwind(db: sqlx::PgPool) -> sqlx::Result<()> { /* keyed dry run; a position above the floor; one tick; a bounded poll for the loop's debug/info line is not observable — assert instead on rpc.calls("getLedgerEntries") having read the account's positions and on remaining() == 0 after shutdown */ }
```

- [ ] **Step 2: Write it**, run `cargo test --lib service::` and `make check`.
- [ ] **Step 3: Commit** — `git commit -m "feat(service): a log-only notifier for the filler's unwind"`

---

## Task 6: documentation, and the dry run against mainnet

**Files:**
- Modify: `CLAUDE.md`, `README.md`, `CHANGELOG.md`, `.env.example`

- [ ] **Step 1: CLAUDE.md**
  - **Status** becomes Phase 6a: unwind lands — after a fill lands, and once at startup, the filler repays the debt it holds from its wallet and withdraws collateral to the wallet, everything but the primary and the primary down to `min_primary_collateral`, keeping its health factor at or above the pool's `min_health_factor`. The "does not unwind" warning goes; what remains for Phase 6b — Telegram, metrics, `/healthz`, `/livez`, `/metrics` — is named.
  - **Module map**: `src/math/unwind.rs` (pure builder, the three steps, rulings 5–8); `src/notifier.rs` (trait, log channel, dedup; Telegram is 6b's); `src/filler.rs` gains the unwind pass paragraph (rulings 2–4, 9–11, 14) and why there is no `unwind.rs`; `src/executor.rs` gains `unwind`; `src/queue.rs` names `UNWIND_RETRIES`.
  - **Gotchas**: the startup unwind trims primary collateral above the floor to the wallet — by design, and the reason to set `min_primary_collateral` to what you mean to keep in the pool; an unwind's repay is capped at what the wallet can spend net of the fee reserve; `min_health_factor` is the unwind floor and `× HF_SAFETY_MULTIPLIER` the fill's; a notification failure never affects trading.
- [ ] **Step 2: README, CHANGELOG, `.env.example`** — README's Safety section: the Phase 5 "does not unwind" paragraph becomes "unwinds to the wallet and holds; nothing is sold" with the startup-trim note; CHANGELOG: a Phase 6a entry in the existing style; `.env.example`: `FAILURE_NOTIFICATION_COOLDOWN_HOURS` beside the filler knobs.
- [ ] **Step 3: `make check`; commit** — `git commit -m "docs(phase-6a): document unwind and the notifier"`
- [ ] **Step 4: The dry run against mainnet** — as Phase 5's: fresh database, `DRY_RUN=true`, `NETWORK=mainnet`, `RPC_URL=https://mainnet.sorobanrpc.com`, `POOLS_FILE=pools.example.toml`, `SEED_FILE=./seed.example.toml`, no keys, ten minutes; capture startup, the filler's ticks (a keyless run makes no unwind read and says so at debug once), and the shutdown; write `phase-6a-live-demo.md` to the scratchpad. Never generate, fund or ask for a key.

---

## Self-review

**Spec coverage** — §5 "Unwind": queued behind pending fills (ruling 2; Task 4); repeated until a pass produces no requests (Task 4); step 1 repay with the wallet balance noting remaining liabilities (Task 1); step 2 withdraw all but the primary, the primary to its floor (Task 1); step 3 withdraw within the floor, also-liabilities first, smallest next, primary last never below its floor, the 0.5% and 1% stops (Task 1); leftover liabilities → one deduplicated high-severity notification per pool (Tasks 2, 4). §8 "Queues": unwinds retry 2 (Task 3). §7 "Notifications": the trait, dedup by `(pool, account, kind)` with a cooldown (Task 2); the semaphore, `drain()` and Telegram are 6b's. §6: `FAILURE_NOTIFICATION_COOLDOWN_HOURS` (Task 2). §1: unwind to the wallet and hold, no swaps (Task 1 sells nothing).

**Deviations, each a ruling:** unwind runs in the filler task rather than as a queued operation (2); a startup pass per pool (3); repays capped at the spendable wallet (5); the dust rule in step 2 too (7); no `unwind.rs` file (15).

**Type consistency:** `UnwindAction`/`UnwindPlan` (Task 1) are what `unwind_requests`/`Executor::unwind` (Task 3) take and `Filler` (Task 4) builds; `Notifier` (Task 2) is what `Filler::new` (Task 4) takes and `spawn_filler` (Task 5) passes; `UnwindOutcome::landed` (Task 3) is what the pass (Task 4) reads to keep a pool pending.
