# Phase 5: the filler, inventory and executor — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The bot plans a fill for every open liquidation auction it supports, keeps its own position above a floor while doing it, records every fill it decides to make — dry-run or not — and, only when `DRY_RUN=false` and `FILLER_SECRET_KEY` is set, submits it through the filler key's queue.

**Architecture:** `math::fill` is pure: it values an auction, finds the ledger at which its lot first covers its bid plus the pool's margin (closed form, proved against the contract's own modifiers), and builds the health-bounded request list — repay, withdraw zero-factor collateral, supply the primary asset, lower the percent, delay — by projecting the filler's position exactly. `filler.rs` does the I/O around it once a tick: which auctions are due, re-read the entry, one snapshot per pool, plan, write the plan onto the auction row, execute when the fill ledger arrives. `executor.rs` simulates the exact `submit`, records before it sends, submits on the filler's queue, and settles the `inventory.rs` reservation on every path. The queue learns to resolve every submission to a terminal outcome and to retry only what provably never landed.

**Tech Stack:** Rust 1.97.0, tokio, sqlx 0.9 (Postgres, compile-time checked queries, offline `.sqlx`), stellar-xdr 28, ed25519-dalek 3.0, reqwest 0.12, clap 4, tracing, thiserror, ethnum.

**Spec:** `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md` — section 5 (filler, inventory, executor; its "Unwind" subsection is Phase 6), section 6 (knobs, pools file, startup validation), section 8 (queues and executor), section 1 ("Invariants specific to Blend": the 400-ledger rule, never filling the bot's own auctions), section 12 (delivery). Read it alongside this plan.

**Decided with the user on 2026-09-15, before planning:** unwind stays in Phase 6, so a live fill under Phase 5 leaves the taken position in the pool until Phase 6 lands and the docs say so loudly; the demonstration is a dry-run against mainnet, as Phase 4's was.

## Global Constraints

- `clippy::pedantic` under `cargo clippy --all-targets -- -D warnings`; `unwrap`/`expect` only in `#[cfg(test)]` code (`clippy.toml` exempts tests; never relax the lint).
- No `as` numeric casts. 3-digit digit grouping on numeric literals. Doc comments state constraints and invariants, never a narration of what changed.
- Structured `tracing` in the crate, never `println!`. `examples/` are the exception.
- **Money is never `f64`.** Balances, debt, collateral and prices are exact integer or fixed-point quantities. Config decimals go through `Decimal7`, never float arithmetic.
- **Secrets arrive through the environment, never as command-line arguments** — `/proc/<pid>/cmdline` is world-readable and the value appears in `ps`, `docker inspect` and `docker compose config`. This binds `FILLER_SECRET_KEY` and `AUCTIONEER_SECRET_KEY` exactly as it binds `DATABASE_URL` and `RPC_API_KEY`.
- **A secret never reaches a `Debug` rendering or a log line.** `Signer` renders as its address only; `Submission`/`QueuedSubmission` have hand-written `Debug`s that never print the operation, and anything new that carries an operation keeps that.
- **`DRY_RUN` defaults to `true`** and the parser accepts only the literal strings `true` and `false`. There is no other way into live trading.
- **Dry-run never signs or sends.** The only simulation a dry-run may run is `Submitter::simulate_only`. `Submitter::prepare` signs unconditionally and, on an archived footprint, signs and sends a `RestoreFootprint` of its own — simulating through it is a submission (CLAUDE.md's gotcha; Phase 4's first Critical).
- **The three-way Rust version pin** moves together or not at all; `scripts/check-repo-invariants.sh` gates it.
- **`CI Summary` treats a skipped job as a failure.** Nothing in `ci.yml` is path-filtered.
- **`tests/fixtures/mainnet-fixed-v2.json` is immutable.** Its `get_reserve`/`get_positions` answers are the only contract-attested expectations; health factors derived from them are golden values.
- Compile-time checked queries only (`sqlx::query!`); after changing one, `make sqlx-prepare` and commit `.sqlx/`. CI runs `cargo sqlx prepare --check -- --lib --bins`.
- `i128` crosses the Postgres boundary as decimal text (`$n::text::numeric` in, `::text` out). Nothing may round.
- **A new migration, never an amendment.** `0001` and `0002` are merged and applied in places; this phase's schema is `0003`.
- **Arithmetic on chain-sourced values is checked, or proven safe in a comment — never silently saturated.** The one sanctioned exception is the inventory ledger, whose saturation spec §5 mandates; its doc says why.
- Build in the dev container with `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0` and a running database (`make db-up`; `DATABASE_URL` is exported by the Makefile — set it yourself for bare `cargo`, `postgres://liquidator:liquidator@127.0.0.1:55432/liquidator`).
- **Counting tests:** `#[test]` + `#[tokio::test]` + `#[sqlx::test]`. All three. The suite on `main` at `50fc03f` is 313 (153 + 61 + 99).

---

## Facts the code is written against

Verified on 2026-09-15 against the tree at `50fc03f` and the `blend-contracts-v2` source at tag `v2.0.0`. An implementer may rely on these without re-deriving them.

### The contract's fill path (`pool/src`)

- `submit(from, spender, to, requests)`; `Request { request_type, address, amount }`; `RequestType` 0 Supply, 1 Withdraw, 2 SupplyCollateral, 3 WithdrawCollateral, 4 Borrow, 5 Repay, 6 FillUserLiquidationAuction (`address` = the liquidated user, `amount` = the percent, 1–100), 7 FillBadDebtAuction, 8 FillInterestAuction, 9 DeleteLiquidationAuction. Requests are applied in order; **the health check runs once, after all of them**.
- `auctions::fill` refuses `user == filler` (`InvalidLiquidation`, 1211). It scales the stored auction by `percent_filled` and the ledger delay; the filler **takes over** the scaled lot's b-tokens as collateral and the scaled bid's d-tokens as liabilities (`add_positions`). The remainder is stored back with the **same `block`**; a full fill deletes the entry.
- `scale_auction`: `block_dif = sequence − auction.block`. For `0..=200`, the lot modifier is `block_dif × 0_0050000` and the bid's is `1`; for `200 < block_dif < 400`, the lot's is `1` and the bid's `1 − (block_dif − 200) × 0_0050000`; at `≥ 400` the bid's is `0`. The bid rounds up and the lot down, both for the percent share and for the modifier. `math::auction::scale_auction` is the port and is already tested against this.
- `validate_submit`, after every request: `require_under_max(positions, prev_count)` — `MaxPositionsExceeded` (1208) when the count of collateral plus liability entries **rose** and now exceeds `max_positions`; `AuctionInProgress` (1212) when `from` itself has a liquidation auction open; and, when `check_health && from.has_liabilities()`, `InvalidHf` (1205) if the health factor is under `1_0000100`, else `MinCollateralNotMet` (1224) if `collateral_base < pool.config.min_collateral`. `check_health` is set by a fill and by `WithdrawCollateral`, never by `Repay` or `SupplyCollateral`. **A position left with no liabilities is not checked at all.**
- `Repay`: `d_tokens_burnt = to_d_token_down(amount)`; if that exceeds the debt, the whole debt is burnt and `amount − to_asset_from_d_token(debt)` is **refunded in the same call**. The spender still transfers the whole `amount` first, so the wallet must hold all of it.
- `WithdrawCollateral`: `to_burn = to_b_token_up(amount)`, capped at the position's b-tokens, with `tokens_out` recomputed from the cap — an amount above the position withdraws all of it. `to_b_token_up(i64::MAX)` is `9.22e18 × 1e12 / b_rate`, far inside `i128`, so `i128::from(i64::MAX)` is a safe "all". The withdrawal also requires the reserve's utilisation to stay under 100% (`InvalidUtilRate`, 1207); the executor's simulation is what catches that.
- `SupplyCollateral`: refused when the reserve is disabled (`ReserveDisabled`, 1223), above the reserve's `supply_cap` (`ExceededSupplyCap`, 1220), or when the pool's status is above 3. Pool status gating (`Pool::require_action_allowed`): status > 1 refuses Borrow and DeleteLiquidationAuction; **status > 3 refuses Supply and SupplyCollateral; fills are always allowed.** `PoolStatus` is `AdminActive=0, Active=1, AdminOnIce=2, OnIce=3, AdminFrozen=4, Frozen=5, Setup=6`.
- `PositionData::is_hf_under(min)`: `false` with no liabilities; otherwise `collateral_base × scalar / liability_base` (floor) `< scalar × min / 1e7` (floor). `math::position::PositionData::is_hf_under` is the port.
- Pool error codes: `InvalidHf` 1205, `InvalidPoolStatus` 1206, `InvalidUtilRate` 1207, `MaxPositionsExceeded` 1208, `InvalidLiquidation` 1211, `AuctionInProgress` 1212, `ExceededSupplyCap` 1220, `ReserveDisabled` 1223, `MinCollateralNotMet` 1224.

### Network facts

- The native asset's Stellar Asset Contract address is `sha256(XDR(HashIdPreimage::ContractId(HashIdPreimageContractId { network_id, contract_id_preimage: ContractIdPreimage::Asset(Asset::Native) })))`, rendered as a `C…` strkey (`stellar_strkey::Contract`). Mainnet: `CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA` — the first reserve in `tests/fixtures/mainnet-fixed-v2.json`, so the fixture attests it. Testnet: `CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC`.
- XLM has 7 decimals, so `XLM_FEE_RESERVE` parsed as a `Decimal7` **is** its value in stroops.
- A SAC's `balance(address)` answers that account's balance of the asset (an account's XLM through the native SAC); `PoolReader::balance(token, account)` already simulates it.

### Interfaces this phase consumes, verbatim

```rust
// src/math — re-exported from src/math/mod.rs
pub struct AuctionData { pub bid: BTreeMap<String, i128>, pub lot: BTreeMap<String, i128>, pub block: u32 } // Clone, Default, PartialEq
pub struct ScaledAuction { pub to_fill: AuctionData, pub remaining: Option<AuctionData> }
pub fn scale_auction(auction: &AuctionData, fill_block: u32, percent_filled: u32) -> Result<ScaledAuction, MathError>;
pub fn bid_modifier(block_delta: u32) -> i128;   // math::auction, 0..=SCALAR_7
pub fn lot_modifier(block_delta: u32) -> i128;   // math::auction, 0..=SCALAR_7
pub struct Positions { pub collateral: BTreeMap<u32, i128>, pub liabilities: BTreeMap<u32, i128>, pub supply: BTreeMap<u32, i128> } // Clone, Default
impl Positions { pub fn effective_count(&self) -> usize; /* collateral.len() + liabilities.len() */ }
pub struct OraclePrices; impl OraclePrices { pub fn new(decimals: u32, prices: BTreeMap<String, i128>) -> Result<Self, MathError>; pub fn scalar(&self) -> i128; pub fn price(&self, asset: &str) -> Result<i128, MathError>; }
pub struct PositionData { pub collateral_base: i128, pub collateral_raw: i128, pub liability_base: i128, pub liability_raw: i128, pub scalar: i128 }
impl PositionData { pub fn health_factor(&self) -> Result<Option<i128>, MathError>; pub fn is_hf_under(&self, min: i128) -> Result<bool, MathError>; }
pub fn calculate_position_data(reserves: &BTreeMap<u32, Reserve>, prices: &OraclePrices, positions: &Positions) -> Result<PositionData, MathError>;
pub struct Reserve { pub asset: String, pub config: ReserveConfig, pub data: ReserveData, pub scalar: i128 }
impl Reserve {
    pub fn new(asset: String, config: ReserveConfig, data: ReserveData) -> Result<Self, MathError>;
    pub fn to_asset_from_d_token(&self, d_tokens: i128) -> Result<i128, MathError>; // rounds up
    pub fn to_asset_from_b_token(&self, b_tokens: i128) -> Result<i128, MathError>; // rounds down
    pub fn to_d_token_down(&self, amount: i128) -> Result<i128, MathError>;
    pub fn to_b_token_down(&self, amount: i128) -> Result<i128, MathError>;
    pub fn accrue(&mut self, bstop_rate: u32, now: u64) -> Result<(), MathError>;
}
// ReserveConfig { index, decimals, c_factor: u32, l_factor: u32, util, max_util, r_base, r_one, r_two, r_three, reactivity, supply_cap: i128, enabled: bool }
// ReserveData { d_rate, b_rate, ir_mod, b_supply, d_supply, backstop_credit: i128, last_time: u64 }
pub fn mul_floor(x: i128, y: i128, denominator: i128) -> Result<i128, MathError>; // floor(x*y/d)
pub fn mul_ceil(x: i128, y: i128, denominator: i128) -> Result<i128, MathError>;  // ceil(x*y/d)
pub fn div_floor(x: i128, y: i128, denominator: i128) -> Result<i128, MathError>; // floor(x*d/y)
pub fn div_ceil(x: i128, y: i128, denominator: i128) -> Result<i128, MathError>;  // ceil(x*d/y)
pub const SCALAR_7: i128 = 10_000_000;
pub enum MathError { DivisionByZero, Overflow, InvalidInput(&'static str), MissingReserve(u32), MissingPrice(String) }

// src/chain/xdr/encode.rs (re-exported by crate::chain::xdr)
pub enum RequestType { Supply, Withdraw, SupplyCollateral, WithdrawCollateral, Borrow, Repay, FillUserLiquidationAuction, FillBadDebtAuction, FillInterestAuction, DeleteLiquidationAuction }
pub struct Request { pub request_type: RequestType, pub address: String, pub amount: i128 }
impl Request { pub fn fill(request_type: RequestType, user: &str, percent: FillPercent) -> Result<Self, XdrError>; }
pub struct FillPercent(u32); impl FillPercent { pub fn get(self) -> u32; } impl TryFrom<u32> for FillPercent { type Error = XdrError; } // 1..=100

// src/chain/pool.rs
pub fn submit_op(pool: &str, from: &str, spender: &str, to: &str, requests: &[Request]) -> Result<Operation, XdrError>;
pub struct PoolSnapshot { pub ledger: u32, pub pool: String, pub instance: PoolInstance, pub reserves: BTreeMap<u32, Reserve>, pub asset_index: BTreeMap<String, u32>, pub prices: OraclePrices, pub price_timestamps: BTreeMap<String, u64>, pub positions: BTreeMap<String, Positions> }
impl PoolSnapshot { pub fn position_data(&self, user: &str, close_time: u64) -> Result<Option<PositionData>, ChainError>; }
impl<'a> PoolReader<'a> {
    pub fn new(rpc: &'a RpcClient, pool: &'a str) -> Self;
    pub async fn snapshot(&self, users: &[&str]) -> Result<PoolSnapshot, ChainError>;
    pub async fn auction(&self, user: &str, auction_type: AuctionType) -> Result<Option<(u32, AuctionData)>, ChainError>;
    pub async fn balance(&self, token: &str, account: &str) -> Result<(u32, i128), ChainError>;
}
// PoolInstance { admin, backstop, blnd_token, name, config: PoolConfig { oracle, bstop_rate: u32, status: PoolStatus, max_positions: u32, min_collateral: i128 } }

// src/chain/tx.rs
pub enum Priority { Normal, High }
pub enum TxOutcome { Succeeded { hash, ledger, return_value }, Failed { hash, ledger, contract_error: Option<u32>, result }, Expired { hash, window, latest_ledger }, Unknown { hash, sequence: i64, window: LedgerWindow } }
pub enum Judgment { Accepted, Refused { contract_error: Option<u32>, message: String }, NeedsRestore }
impl<'a> Submitter<'a> {
    pub fn new(rpc: &'a RpcClient, network: &'a Network, signer: &'a Signer, config: TxConfig) -> Self;
    pub async fn prepare(&self, operation: Operation, priority: Priority) -> Result<Prepared, ChainError>;
    pub async fn send(&self, prepared: &Prepared) -> Result<(), ChainError>;
    pub async fn wait_for(&self, hash: TxHash, sequence: i64, window: LedgerWindow) -> Result<TxOutcome, ChainError>;
    pub async fn simulate_only(&self, operation: &Operation) -> Result<Judgment, ChainError>;
}
pub struct Prepared { pub envelope: TransactionEnvelope, pub hash: TxHash, pub sequence: i64, pub window: LedgerWindow, pub fee: u32, pub resource_fee: i64 }
// TxConfig's fields are all pub: base_fee, high_fee, poll_ledgers, poll_interval, send_retry_pause, wait_cap.

// src/chain/mod.rs — ChainError variants: Transport, Http(u16), Rpc { .. }, Shape(String), LedgerMoved { .. }, NoAccount(String),
//   Xdr, Math, Config(&'static str), SecretKey, Simulation { message, contract_error: Option<u32> }, Restore(String),
//   RestoreUnknown { .. }, Rejected(String), BadSequence
// src/chain/rpc.rs: impl RpcClient { pub async fn account(&self, account: &str) -> Result<Account, ChainError> } // NoAccount when unfunded

// src/queue.rs
pub struct Submission { pub operation: Operation, pub priority: Priority, pub label: String }   // Task 5 adds `retries`
pub struct QueuedSubmission { pub submission: Submission, pub respond: oneshot::Sender<Result<TxOutcome, QueueError>> }
pub enum QueueError { Closed, ShuttingDown, Chain(ChainError) }
impl SubmissionQueue { pub fn new(capacity: NonZeroUsize) -> (Self, mpsc::Receiver<QueuedSubmission>); pub async fn enqueue(&self, submission: Submission) -> Result<TxOutcome, QueueError>; } // Clone
pub async fn run_queue(submitter: &Submitter<'_>, receiver: mpsc::Receiver<QueuedSubmission>, shutdown: &watch::Receiver<bool>);

// src/store.rs
pub struct TrackedAuction { pub pool: String, pub account: String, pub auction_type: AuctionType, pub start_ledger: u32, pub fill_ledger: Option<u32>, pub percent: Option<FillPercent>, pub bid: BTreeMap<String, i128>, pub lot: BTreeMap<String, i128>, pub updated_ledger: u32 }
impl Store {
    pub async fn upsert_auction(&self, auction: &TrackedAuction) -> Result<(), StoreError>;
    pub async fn delete_auction(&self, pool: &str, account: &str, auction_type: AuctionType) -> Result<bool, StoreError>;
    pub async fn auction(&self, pool: &str, account: &str, auction_type: AuctionType) -> Result<Option<TrackedAuction>, StoreError>;
    pub async fn open_auctions(&self, pool: &str) -> Result<Vec<TrackedAuction>, StoreError>; // ORDER BY start_ledger, account, auction_type
}
// private helpers reused: asset_amounts_to_json(&BTreeMap<String, i128>) -> Value (i128 as decimal strings), auction_type_code(AuctionType) -> i16

// src/config.rs
pub struct PoolConfig { pub address: String, pub primary_asset: String, pub min_primary_collateral: i128, pub min_health_factor: i128, pub default_profit_bps: u32, pub force_fill: bool, pub supported_bid: Vec<String>, pub supported_lot: Vec<String>, pub profits: Vec<ProfitRule> }
pub struct ProfitRule { pub profit_bps: u32, pub supported_bid: Vec<String>, pub supported_lot: Vec<String> }
pub struct Decimal7(i128); impl Decimal7 { pub fn get(self) -> i128; }
pub struct SigningKeys { pub auctioneer: Option<Signer>, pub filler: Option<Signer> }
impl SigningKeys { pub fn own_addresses(&self) -> BTreeSet<String>; pub fn into_auctioneer_signer(self) -> Option<Signer>; } // Task 1 replaces the latter
impl Args { pub fn signing_keys(&self, filler: Option<String>, auctioneer: Option<String>) -> Result<SigningKeys, LiquidatorError>; }

// src/ledger.rs
pub struct LedgerTick { pub sequence: u32, pub close_time: u64 } // Copy
```

### What already exists and is reused, not rebuilt

- `config::PoolConfig` already parses and validates every pool-level field this phase reads: `primary_asset`, `min_primary_collateral`, `min_health_factor`, `default_profit_bps`, `force_fill`, `supported_bid`, `supported_lot`, `profits`. Nothing consumes them yet.
- `Tracker::apply` already keeps `auctions` rows current: a `NewAuction` upserts with `fill_ledger`/`percent` `None`; a partial `FillAuction` re-reads the remainder from chain and resets the plan; a full fill or a `DeleteAuction` deletes the row. **The filler never writes `bid`, `lot` or `start_ledger`.**
- `TrackedAuction.fill_ledger` and `.percent` are documented as "the filler's current plan". Phase 5 is their first writer.
- `FILLER_SECRET_KEY` is already read by `main.rs` and parsed by `Args::signing_keys`; both keys' addresses already reach the auctioneer's `own_addresses`.
- `ActOutcome::settled` is the rule the executor mirrors: only a chain success, or a dry-run's deliberate no-send, is done.
- `Service::validate` already checks, per pool: one shared backstop, the primary asset is an enabled reserve with a positive collateral factor, every listed supported asset is a reserve, `max_positions ≥ 2`.

---

## File structure

| File | Responsibility |
|---|---|
| `migrations/0003_fills.sql` (create) | The `fills` audit table, spec §4's columns. |
| `src/math/fill.rs` (create) | Pure. Auction valuation, the fill delay and its proof, the health floor, and `plan_fill`. No I/O, no panics. |
| `src/math/auction.rs` (modify) | `RAMP_BLOCKS` and `RAMP_END_BLOCKS` become `pub`. |
| `src/math/mod.rs` (modify) | `pub mod fill;`. |
| `src/inventory.rs` (create) | The filler's wallet balances, reservations, and `Settlement`. |
| `src/executor.rs` (create) | Simulate the exact `submit`, record, submit on the filler queue, settle. |
| `src/filler.rs` (create) | The evaluation loop's I/O: due auctions, re-read, snapshot, plan, execute. |
| `src/queue.rs` (modify) | Terminal outcomes before the next submission; a per-submission retry budget. |
| `src/chain/signer.rs` (modify) | `Network::native_asset_contract`. |
| `src/chain/pool.rs` (modify) | `PoolSnapshot::valued_at` and `PoolSnapshot::accrued_reserves`, moved from `auctioneer.rs` so both tasks share one clamp. |
| `src/chain/tx.rs` (modify) | `Submitter::source`, the signing account's address. |
| `src/config.rs` (modify) | Six knobs, the key rules, `Signers`, `PoolConfig::supports`/`profit_bps`, the pool floor's lower bound. |
| `src/store.rs` (modify) | `FillRecord`, `record_fill`, `attach_fill_tx`, `set_fill_plan`. |
| `src/auctioneer.rs` (modify) | A retry budget on creations; adopt an auction the chain already holds; the moved valuation helpers. |
| `src/service.rs` (modify) | A queue per distinct key, the filler task, filler validation in `run` and `check-config`. |
| `src/liquidator.rs` (modify) | Module declarations; `LiquidatorError::Filler`. |
| `CLAUDE.md`, `README.md`, `CHANGELOG.md`, `.env.example`, `pools.example.toml` (modify) | Task 11. |

## Rulings taken before execution

Each is a decision an implementer would otherwise have to guess at. They are settled; do not relitigate them, and if one turns out to be wrong, say so in the report rather than quietly doing something else.

1. **Pure arithmetic lives in `math::fill`.** `filler.rs`, `executor.rs` and `inventory.rs` do no fixed-point arithmetic on money — Phase 4's ruling 1, for the same reason: the golden tests need it pure.
2. **One submission queue per distinct signing key, shared when two roles share one key.** With no `AUCTIONEER_SECRET_KEY` the auctioneer signs with the filler's key (spec §6: "auctioneer key optional, defaulting to the filler key"), and a queue per role would put two queues on one key — the sequence race `queue.rs` exists to make unreachable. Both keys given and equal is refused at startup (spec §6: "Keys parse and differ when both are given"), so sharing happens only by fallback, and `Arc::ptr_eq` detects it.
3. **The filler signs with `FILLER_SECRET_KEY` only**, never the auctioneer's. `DRY_RUN=false` without `FILLER_SECRET_KEY` is a startup error (spec §6: "required for live").
4. **A dry run with no filler key still plans**, against an empty inventory (no positions, nothing in the wallet), and records its fills unsimulated — Phase 4's ruling 5 as corrected, for the same reason: simulation needs a source account and the plan does not. What an operator sees is what the bot would do with no capital, which is the honest answer for a keyless dry run.
5. **The inventory tracks wallet balances only; positions come from each plan's own snapshot.** Spec §5 has the inventory keep pool positions too. A plan reads a snapshot anyway, for reserves and prices, and the filler's positions valued against another ledger's reserves would be exactly the disagreement CLAUDE.md's accrual gotcha warns about. Cost if wrong: Phase 6's unwind adds positions to the inventory.
6. **The queue resolves every submission to a terminal outcome before it takes the next, and retries only what provably never landed.** A send that fails indefinitely (transport, HTTP status, JSON-RPC error, hash mismatch) is resolved by hash through `wait_for`, never resent; an `Unknown` is polled until it is terminal. `Rejected` (the RPC refused the envelope: nothing will land) and a failed `prepare` that sent nothing are retried with a fresh `prepare`, backing off from 1 s doubling to 30 s, up to the submission's budget — creations 3, fills 10 (spec §8). `BadSequence`, a simulation's contract error, and `Expired` go back to the caller, who re-plans (spec §8: "a stale plan is never resent"). Today `run_queue` moves on after an `Unknown`, so the next submission for the same key can be prepared against a sequence number an in-flight transaction may still consume; closing that is part of making fills safe.
7. **A plan is written onto its auction row (`fill_ledger`, `percent`); the ledger it was made at is kept in memory.** The replan cadence needs "ledgers since this was last planned", and losing that on restart only makes every row due at once. A `fills` row is written only when a fill is executed.
8. **A dry-run fill is recorded once per auction**, keyed `(pool, account, start_ledger)` in memory, not on every tick until someone else fills it. A restart may record one more; nothing else does. *Corrected during review:* a partial fill by someone else keeps the start ledger and leaves a remainder — a different fill, and the one an armed filler would now make — so the key also carries the amounts the chain held — not a ledger: the filler can see a remainder from the chain a tick before the tracker rewrites the row, and a ledger-keyed version would record it twice — and a remainder is recorded afresh, once. `CLAUDE.md` states the current rule; this ruling is kept as written for the record.
9. **Before `STARTUP_DELAY_LEDGERS` has elapsed the filler plans but executes nothing**, dry-run or armed: no `fills` row, no submission. The auctioneer records during its delay because it re-decides each pass anyway; a held-back fill would otherwise be recorded on every tick of the delay.
10. **The delay escalation targets the filler's floor, not a ratio of 1.** Spec §5 step 4 says "delay the fill past ledger 200 to the ledger at which received collateral outweighs taken debt." A delay candidate has to pass the same exact projection as every other candidate, or the next round rejects it anyway, so "outweighs" is read as "outweighs enough to hold the floor".
11. **Candidates are searched exactly, not guessed.** Each escalation step picks the largest lower percent (at most 99 candidates) or the first later ledger (at most 400) whose exact projection holds the floor; `PLAN_ITERATIONS` bounds the rounds of supply → percent → delay. It is pure arithmetic at microseconds per projection, and exactness is worth more than a linear model's guess that the next round would have to correct.
12. **The executor's one re-plan after `InvalidHf` or `MinCollateralNotMet` halves the percent** — `max(1, percent / 2)` becomes the planner's ceiling. Our projection disagreed with the contract's, so a price or an accrual moved; halving buys margin in one step, and spec §5's "one re-plan" leaves no room for a second guess.
13. **An auction past its 400th ledger is filled only under `force_fill`, and `force_fill` caps both the profit delay and the health delay at 350** (spec §1 and §5). `pools.example.toml` says `force_fill` means "fill regardless of profit"; Task 11 corrects it to the spec's meaning.
14. **An `Unknown` outcome consumes its reservation.** The transaction may have spent the wallet, so the ledger assumes it did until the next refresh reads the truth. Under ruling 6 that only happens when shutdown interrupts a resolution, and the inventory dies with the process then anyway.
15. **The auctioneer adopts an auction the chain already holds.** An auction opened before the bot's events cursor never produced a `NewAuction` the tracker saw, so the filler could never find it. When the auctioneer's simulation answers `AuctionInProgress` (1212), it reads the entry and upserts the row.

---

## Task 1: the filler's configuration, the key rules and the native asset

**Files:**
- Modify: `src/config.rs` (six `Args` knobs and their `ServiceConfig` fields; the pool floor's lower bound in `parse_pools`; `PoolConfig::supports`/`profit_bps`; `Signers`; the two new rules in `Args::signing_keys`)
- Modify: `src/chain/signer.rs` (`Network::native_asset_contract`)
- Modify: `src/service.rs` (`SigningContext::from_config` takes the auctioneer's key from `into_signers()` — no behaviour change)
- Test: inline in both modules

**Interfaces:**
- Consumes: `Decimal7`, `parse_pools`, `SigningKeys`, `Network` as they exist.
- Produces:
  ```rust
  // src/config.rs
  pub struct Signers { pub auctioneer: Option<Arc<Signer>>, pub filler: Option<Arc<Signer>> } // Debug, Default
  impl Signers { pub fn shared(&self) -> bool; }
  impl SigningKeys { pub fn into_signers(self) -> Signers; } // replaces into_auctioneer_signer, which is removed
  impl PoolConfig {
      pub fn supports(&self, bid: &[&str], lot: &[&str]) -> bool;
      pub fn profit_bps(&self, bid: &[&str], lot: &[&str]) -> u32;
  }
  pub struct ServiceConfig { /* existing fields, plus: */
      pub hf_safety_multiplier: i128,      // 7 decimals, >= SCALAR_7
      pub replan_ledgers: u32,             // >= 1
      pub replan_near_ledgers: u32,
      pub xlm_fee_reserve: i128,           // stroops
      pub high_fee_profit_threshold: i128, // 7 decimals, oracle units
      pub inventory_refresh: std::time::Duration, // >= 1 s
  }
  // src/chain/signer.rs
  impl Network { pub fn native_asset_contract(&self) -> Result<String, ChainError>; }
  ```

- [ ] **Step 1: Write the failing tests**

In `src/config.rs`'s test module (it already has `parse`, `assert_clean_environment`, `POOLS`, `FILLER_SEED` and `AUCTIONEER_SEED`):

```rust
/// The first pool of the sample file, for struct-update syntax in the
/// rule tests below.
fn sample_pool() -> PoolConfig {
    parse_pools(POOLS).expect("the sample parses").remove(0)
}

/// Spec §6's defaults for the filler's knobs.
#[test]
fn the_filler_knobs_default_to_the_spec() {
    assert_clean_environment();
    let args = Args::try_parse_from(["liquidator"]).unwrap();
    assert_eq!(args.hf_safety_multiplier.get(), 11_000_000, "1.1");
    assert_eq!(args.replan_ledgers, 10);
    assert_eq!(args.replan_near_ledgers, 5);
    assert_eq!(
        args.xlm_fee_reserve.get(),
        500_000_000,
        "50 XLM: XLM has 7 decimals, so a Decimal7 is its value in stroops"
    );
    assert_eq!(args.high_fee_profit_threshold.get(), 100_000_000, "10");
    assert_eq!(args.inventory_refresh_secs, 30);
}

/// Under one, the filler's floor would sit under the pool's own
/// `min_health_factor` — the operator's stated minimum — and a fill could
/// leave the filler below it by design.
#[test]
fn a_health_multiplier_under_one_is_refused_at_parse() {
    assert!(Args::try_parse_from(["liquidator", "--hf-safety-multiplier", "0.9999999"]).is_err());
    assert!(Args::try_parse_from(["liquidator", "--hf-safety-multiplier", "1"]).is_ok());
}

/// `REPLAN_LEDGERS=0` would re-plan every auction on every ledger — not a
/// cadence at all — and `INVENTORY_REFRESH_SECS=0` would read every wallet
/// balance on every tick. Both refused like every other cadence knob.
#[test]
fn zero_filler_cadences_are_refused_at_parse() {
    assert!(Args::try_parse_from(["liquidator", "--replan-ledgers", "0"]).is_err());
    assert!(Args::try_parse_from(["liquidator", "--inventory-refresh-secs", "0"]).is_err());
    assert!(
        Args::try_parse_from(["liquidator", "--replan-near-ledgers", "0"]).is_ok(),
        "zero is meaningful here: re-plan only at the fill ledger itself"
    );
}

/// Spec §6: "Keys parse and differ when both are given." Two roles on one
/// key would need two queues on one key — the sequence race `queue.rs`
/// exists to make unreachable — so the same key twice is refused, and
/// leaving `AUCTIONEER_SECRET_KEY` unset is how one key signs both.
#[test]
fn the_same_key_twice_is_refused() {
    assert_clean_environment();
    let args = parse(&["liquidator", "--network", "testnet", "--rpc-url", "http://rpc"]);
    let error = args
        .signing_keys(Some(FILLER_SEED.to_string()), Some(FILLER_SEED.to_string()))
        .expect_err("the same key twice");
    let message = error.to_string();
    assert!(
        message.contains("AUCTIONEER_SECRET_KEY") && message.contains("FILLER_SECRET_KEY"),
        "{message}"
    );
    assert!(!message.contains(FILLER_SEED), "the message never echoes the key");
}

/// Spec §6: `FILLER_SECRET_KEY` is "required for live". An armed bot with
/// only an auctioneer key would create auctions and never fill one.
#[test]
fn live_trading_needs_the_fillers_key() {
    assert_clean_environment();
    let live = parse(&[
        "liquidator", "--network", "testnet", "--rpc-url", "http://rpc", "--dry-run=false",
    ]);
    assert!(live.signing_keys(None, None).is_err(), "no key at all");
    assert!(
        live.signing_keys(None, Some(AUCTIONEER_SEED.to_string())).is_err(),
        "an auctioneer key alone"
    );
    assert!(live.signing_keys(Some(FILLER_SEED.to_string()), None).is_ok());

    let dry = parse(&["liquidator", "--network", "testnet", "--rpc-url", "http://rpc"]);
    assert!(dry.signing_keys(None, None).is_ok(), "a dry run needs no key");
}

/// With no auctioneer key both roles hold the filler's one key — the same
/// `Arc`, so `shared` can tell by pointer — and that is what gives them one
/// queue. Two configured keys are two keys and two queues.
#[test]
fn a_fallback_auctioneer_shares_the_fillers_key() {
    assert_clean_environment();
    let args = parse(&["liquidator", "--network", "testnet", "--rpc-url", "http://rpc"]);

    let one = args
        .signing_keys(Some(FILLER_SEED.to_string()), None)
        .unwrap()
        .into_signers();
    assert!(one.shared());
    assert_eq!(
        one.auctioneer.as_ref().unwrap().address(),
        one.filler.as_ref().unwrap().address()
    );

    let two = args
        .signing_keys(Some(FILLER_SEED.to_string()), Some(AUCTIONEER_SEED.to_string()))
        .unwrap()
        .into_signers();
    assert!(!two.shared());
    assert_ne!(
        two.auctioneer.as_ref().unwrap().address(),
        two.filler.as_ref().unwrap().address()
    );

    let auctioneer_only = args
        .signing_keys(None, Some(AUCTIONEER_SEED.to_string()))
        .unwrap()
        .into_signers();
    assert!(auctioneer_only.filler.is_none(), "the filler never borrows the auctioneer's key");
    assert!(auctioneer_only.auctioneer.is_some() && !auctioneer_only.shared());

    let none = args.signing_keys(None, None).unwrap().into_signers();
    assert!(none.auctioneer.is_none() && none.filler.is_none() && !none.shared());
}

/// A pool floor at or under the contract's own post-submit minimum,
/// `1.0000100`, lets the filler plan fills the contract refuses as
/// `InvalidHf` — every one of them a wasted simulation and re-plan.
#[test]
fn a_pool_floor_at_the_contracts_own_minimum_is_refused() {
    let at = parse_pools(&POOLS.replace("min_health_factor = 1.5", "min_health_factor = 1.00001"));
    assert!(matches!(&at, Err(error) if error.to_string().contains("min_health_factor")), "{at:?}");
    assert!(parse_pools(&POOLS.replace("min_health_factor = 1.5", "min_health_factor = 1.0000101")).is_ok());
}

/// Spec §5: an auction is a candidate only when every bid asset is in
/// `supported_bid` and every lot asset in `supported_lot`; `*` matches
/// any reserve.
#[test]
fn supported_assets_cover_every_asset_on_each_side() {
    let pool = PoolConfig {
        supported_bid: vec!["A".to_string(), "B".to_string()],
        supported_lot: vec!["*".to_string()],
        ..sample_pool()
    };
    assert!(pool.supports(&["A"], &["X", "Y"]));
    assert!(pool.supports(&["A", "B"], &["X"]));
    assert!(!pool.supports(&["A", "C"], &["X"]), "one unsupported bid asset refuses the auction");
    let strict = PoolConfig { supported_lot: vec!["X".to_string()], ..pool };
    assert!(!strict.supports(&["A"], &["X", "Y"]), "one unsupported lot asset refuses it too");
}

/// Spec §5: the margin is the first `profits` rule whose lists cover every
/// auction asset, else `default_profit_bps`.
#[test]
fn the_first_matching_profit_rule_wins() {
    let pool = PoolConfig {
        default_profit_bps: 1_000,
        profits: vec![
            ProfitRule {
                profit_bps: 500,
                supported_bid: vec!["USDC".to_string()],
                supported_lot: vec!["*".to_string()],
            },
            ProfitRule {
                profit_bps: 200,
                supported_bid: vec!["*".to_string()],
                supported_lot: vec!["*".to_string()],
            },
        ],
        ..sample_pool()
    };
    assert_eq!(pool.profit_bps(&["USDC"], &["XLM"]), 500);
    assert_eq!(pool.profit_bps(&["XLM"], &["USDC"]), 200, "the first rule does not cover an XLM bid");
    let no_rules = PoolConfig { profits: Vec::new(), ..pool };
    assert_eq!(no_rules.profit_bps(&["XLM"], &["USDC"]), 1_000);
}
```

Update the existing `the_auctioneer_falls_back_to_the_filler_key` test to read `into_signers().auctioneer` where it read `into_auctioneer_signer()`; its assertions stay as they are.

In `src/chain/signer.rs`'s test module:

```rust
/// Derived from the network id, never configured, so it cannot be typed
/// in wrong. The two published addresses pin the derivation.
#[test]
fn the_native_asset_contract_is_derived_from_the_network() {
    assert_eq!(
        Network::mainnet().native_asset_contract().unwrap(),
        "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA"
    );
    assert_eq!(
        Network::testnet().native_asset_contract().unwrap(),
        "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC"
    );
}

/// The contract-attested half of the check above: the mainnet fixture's
/// first reserve is the native asset.
#[test]
fn the_fixtures_first_reserve_is_mainnets_native_asset() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/mainnet-fixed-v2.json")).unwrap();
    assert_eq!(
        fixture["reserves"][0]["asset"],
        Network::mainnet().native_asset_contract().unwrap()
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib config:: chain::signer::`
Expected: compile errors — the fields, `into_signers`, `supports`, `profit_bps` and `native_asset_contract` do not exist.

- [ ] **Step 3: Write it**

The knobs, beside the auctioneer's in `Args`. Each doc comment is the operator's documentation and states why a bad value is refused:

```rust
/// The pool's `min_health_factor` is multiplied by this for the floor the
/// filler keeps its own position at or above after a fill (spec §5).
///
/// At least 1, refused at parse rather than clamped: under one, the floor
/// would sit under the pool's own `min_health_factor`, the operator's
/// stated minimum, and a fill could leave the filler below it by design.
#[arg(long, env = "HF_SAFETY_MULTIPLIER", default_value = "1.1", value_parser = health_multiplier)]
pub hf_safety_multiplier: Decimal7,

/// How often, in ledgers, an auction the filler has already planned is
/// planned again. Zero is refused: it would re-plan every auction on every
/// ledger, which is `REPLAN_NEAR_LEDGERS`'s job and only near the target.
#[arg(long, env = "REPLAN_LEDGERS", default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..))]
pub replan_ledgers: u32,

/// Within this many ledgers of its planned fill ledger an auction is
/// planned again on every ledger. Zero means only at the fill ledger.
#[arg(long, env = "REPLAN_NEAR_LEDGERS", default_value_t = 5)]
pub replan_near_ledgers: u32,

/// XLM the filler never spends, kept back for transaction fees. Decimal
/// XLM; XLM has 7 decimals, so the parsed value is in stroops.
#[arg(long, env = "XLM_FEE_RESERVE", default_value = "50")]
pub xlm_fee_reserve: Decimal7,

/// The estimated profit, in the pool oracle's units, at or above which a
/// fill pays the high fee tier (`HIGH_FEE`) rather than the base one.
#[arg(long, env = "HIGH_FEE_PROFIT_THRESHOLD", default_value = "10")]
pub high_fee_profit_threshold: Decimal7,

/// The longest the filler's wallet balances go unread, in seconds; they
/// are also re-read after every confirmed transaction. Zero is refused:
/// it would read every balance on every tick.
#[arg(long, env = "INVENTORY_REFRESH_SECS", default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
pub inventory_refresh_secs: u64,
```

`health_multiplier` is shaped like `target_health_factor`:

```rust
/// `HF_SAFETY_MULTIPLIER`'s parser: a `Decimal7` of at least one.
fn health_multiplier(text: &str) -> Result<Decimal7, String> {
    let value: Decimal7 = text.parse()?;
    if value.get() < crate::math::SCALAR_7 {
        return Err(format!(
            "`{text}` is under 1: HF_SAFETY_MULTIPLIER scales the pool's own \
             min_health_factor, and under one the filler's floor would sit below it"
        ));
    }
    Ok(value)
}
```

Add the six `ServiceConfig` fields with the doc comments in the Interfaces block and set them in `service_with_secrets` (`hf_safety_multiplier: self.hf_safety_multiplier.get()`, `xlm_fee_reserve: self.xlm_fee_reserve.get()`, `high_fee_profit_threshold: self.high_fee_profit_threshold.get()`, `inventory_refresh: std::time::Duration::from_secs(self.inventory_refresh_secs)`, the two `u32`s as they are). Fix every other `ServiceConfig` literal the compiler names.

The pool floor, in `parse_pools` beside the existing per-pool checks:

```rust
/// The contract's own post-submit health minimum, `1.0000100` in 7
/// decimals (`validate_submit`'s `is_hf_under(e, 1_0000100)`). A filler
/// floor at or under it plans fills the contract refuses as `InvalidHf`.
const CONTRACT_MIN_HEALTH: i128 = 10_000_100;
```

and refuse `pool.min_health_factor.get() <= CONTRACT_MIN_HEALTH` with a `LiquidatorError::Config` naming the pool and `min_health_factor`.

The asset rules:

```rust
impl PoolConfig {
    /// Whether the filler takes this auction at all (spec §5): every bid
    /// asset is in `supported_bid` and every lot asset in `supported_lot`,
    /// `*` matching any reserve. One unsupported asset on either side
    /// refuses the whole auction — a fill takes every asset it names.
    #[must_use]
    pub fn supports(&self, bid: &[&str], lot: &[&str]) -> bool {
        covers(&self.supported_bid, bid) && covers(&self.supported_lot, lot)
    }

    /// The margin a fill waits for, in basis points: the first `profits`
    /// rule whose lists cover every bid and lot asset, else
    /// `default_profit_bps`. Order matters and is the operator's.
    #[must_use]
    pub fn profit_bps(&self, bid: &[&str], lot: &[&str]) -> u32 {
        self.profits
            .iter()
            .find(|rule| covers(&rule.supported_bid, bid) && covers(&rule.supported_lot, lot))
            .map_or(self.default_profit_bps, |rule| rule.profit_bps)
    }
}

/// Whether `list` names every one of `assets`, `*` naming them all.
fn covers(list: &[String], assets: &[&str]) -> bool {
    list.iter().any(|entry| entry == "*")
        || assets.iter().all(|asset| list.iter().any(|entry| entry == asset))
}
```

The signers, replacing `into_auctioneer_signer`:

```rust
/// The two signing roles' keys. Each is an `Arc` — `Signer` is
/// deliberately not `Clone`, since it holds key material — so a role that
/// falls back to the other's key holds *the same* key, and
/// [`Signers::shared`] tells by pointer. Whether the roles share a key is
/// what decides whether they share a submission queue: one queue per key,
/// because two queues on one key race each other for its sequence number.
#[derive(Debug, Default)]
pub struct Signers {
    /// Signs auction creations: `AUCTIONEER_SECRET_KEY`, else the filler's.
    pub auctioneer: Option<Arc<crate::chain::Signer>>,
    /// Signs fills: `FILLER_SECRET_KEY`, and never the auctioneer's.
    pub filler: Option<Arc<crate::chain::Signer>>,
}

impl Signers {
    /// Whether both roles hold the one key.
    #[must_use]
    pub fn shared(&self) -> bool {
        matches!(
            (&self.auctioneer, &self.filler),
            (Some(auctioneer), Some(filler)) if Arc::ptr_eq(auctioneer, filler)
        )
    }
}

impl SigningKeys {
    /// The two roles' keys: the filler's own, and the auctioneer's own or
    /// else the filler's — the spec's "auctioneer key optional, defaulting
    /// to the filler key". Never the reverse: the filler does not sign with
    /// the auctioneer's key.
    #[must_use]
    pub fn into_signers(self) -> Signers {
        let filler = self.filler.map(Arc::new);
        let auctioneer = self.auctioneer.map(Arc::new).or_else(|| filler.clone());
        Signers { auctioneer, filler }
    }
}
```

`Args::signing_keys` gains the two rules after both keys parse, each message naming variables and never a value:

```rust
if let (Some(auctioneer), Some(filler)) = (&keys.auctioneer, &keys.filler) {
    if auctioneer.address() == filler.address() {
        return Err(LiquidatorError::Config(
            "AUCTIONEER_SECRET_KEY and FILLER_SECRET_KEY are the same key: leave \
             AUCTIONEER_SECRET_KEY unset and the auctioneer signs with the filler's key, \
             through the one queue that key needs"
                .to_string(),
        ));
    }
}
if !self.dry_run && keys.filler.is_none() {
    return Err(LiquidatorError::Config(
        "DRY_RUN=false needs FILLER_SECRET_KEY: live trading fills auctions, and the \
         filler signs with its own key only"
            .to_string(),
    ));
}
```

Update `SigningKeys`' doc to describe `into_signers`. In `src/service.rs`, `SigningContext::from_config` becomes `signer: keys.into_signers().auctioneer` (computing `own_addresses` first, exactly as now) — Task 10 rewires the rest.

`Network::native_asset_contract` in `src/chain/signer.rs`:

```rust
/// The native asset's (XLM's) Stellar Asset Contract on this network:
/// `sha256` of the network id and the native asset, as the protocol
/// derives it. Derived rather than configured, so a wrong address cannot
/// be typed in — it is where the filler's fee reserve is held back from.
///
/// # Errors
///
/// Only if the fixed preimage fails to encode, which is a bug.
pub fn native_asset_contract(&self) -> Result<String, ChainError> {
    let preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
        network_id: Hash(self.id),
        contract_id_preimage: ContractIdPreimage::Asset(Asset::Native),
    });
    let bytes = preimage.to_xdr(Limits::none())?; // map through the crate's XdrError, as encode.rs does
    Ok(stellar_strkey::Contract(Sha256::digest(&bytes).into()).to_string())
}
```

- [ ] **Step 4: Run the tests and the suite**

Run: `cargo test --lib config:: chain::signer::` then `make check`.
Expected: the new tests pass; nothing else changed behaviour. If an existing test called `signing_keys` armed with no filler key, it now fails for the new rule: make it dry-run or give it a filler key, and say so in the report.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/chain/signer.rs src/service.rs
git commit -m "feat(config): the filler's knobs, the key rules and the native asset"
```

---

## Task 2: the fills table and the filler's writes

**Files:**
- Create: `migrations/0003_fills.sql`
- Modify: `src/store.rs` (`FillRecord`, `record_fill`, `attach_fill_tx`, `set_fill_plan`; module doc)
- Modify: `.sqlx/` (regenerated by `make sqlx-prepare`)
- Test: inline in `src/store.rs`

**Interfaces:**
- Consumes: `asset_amounts_to_json`, `auction_type_code`, `StoreError` as they exist in `src/store.rs`.
- Produces:
  ```rust
  pub struct FillRecord { pub pool: String, pub account: String, pub auction_type: AuctionType,
      pub fill_ledger: u32, pub percent: FillPercent, pub bid: BTreeMap<String, i128>,
      pub lot: BTreeMap<String, i128>, pub bid_value: i128, pub lot_value: i128,
      pub est_profit: i128, pub dry_run: bool } // Debug, Clone, PartialEq, Eq
  impl Store {
      pub async fn record_fill(&self, fill: &FillRecord) -> Result<i64, StoreError>;
      pub async fn attach_fill_tx(&self, id: i64, tx_hash: &str) -> Result<bool, StoreError>;
      pub async fn set_fill_plan(&self, pool: &str, account: &str, auction_type: AuctionType,
                                 plan: Option<(u32, FillPercent)>) -> Result<bool, StoreError>;
  }
  ```

- [ ] **Step 1: Write the migration**

`migrations/0003_fills.sql`:

```sql
-- The filler's audit trail: one row per fill the filler executed, dry-run
-- or not (spec section 4's `fills`). A row is written before anything is
-- submitted and the transaction's hash is attached once there is one, so a
-- row with no hash and `dry_run = false` is an armed attempt whose
-- transaction was never named. Values and profit are in the pool oracle's
-- units, as decimal text through `numeric` like every other i128 here.
CREATE TABLE fills (
    id            bigserial   PRIMARY KEY,
    tx_hash       text        UNIQUE,
    pool          text        NOT NULL,
    account       text        NOT NULL,
    auction_type  smallint    NOT NULL CHECK (auction_type BETWEEN 0 AND 2),
    fill_ledger   bigint      NOT NULL,
    percent       smallint    NOT NULL CHECK (percent BETWEEN 1 AND 100),
    bid           jsonb       NOT NULL,
    lot           jsonb       NOT NULL,
    bid_value     numeric     NOT NULL,
    lot_value     numeric     NOT NULL,
    est_profit    numeric     NOT NULL,
    dry_run       boolean     NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX fills_by_pool ON fills (pool, created_at DESC);
```

- [ ] **Step 2: Write the failing tests**

```rust
fn sample_fill() -> FillRecord {
    FillRecord {
        pool: POOL.to_string(),
        account: USER.to_string(),
        auction_type: AuctionType::UserLiquidation,
        fill_ledger: 64_271_400,
        percent: FillPercent::try_from(80).unwrap(),
        bid: BTreeMap::from([("CBID".to_string(), i128::MAX)]),
        lot: BTreeMap::from([("CLOT".to_string(), 12)]),
        // Beyond bigint, which is why the column is numeric.
        bid_value: 10_i128.pow(30),
        lot_value: 10_i128.pow(30) + 7,
        // A force_fill pool can fill at a loss; the column must hold it.
        est_profit: -5,
        dry_run: true,
    }
}

/// Every column round-trips exactly, i128s included.
#[sqlx::test(migrations = "./migrations")]
async fn a_fill_is_recorded_with_every_column(db: sqlx::PgPool) -> sqlx::Result<()> {
    let store = Store::from_pool(db);
    let id = store.record_fill(&sample_fill()).await.expect("record");
    let row = sqlx::query!(
        r#"SELECT tx_hash, auction_type, fill_ledger, percent, bid, lot,
                  bid_value::text AS "bid_value!", lot_value::text AS "lot_value!",
                  est_profit::text AS "est_profit!", dry_run
           FROM fills WHERE id = $1"#,
        id
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(row.tx_hash, None, "recorded before anything is sent");
    assert_eq!((row.auction_type, row.fill_ledger, row.percent), (0, 64_271_400, 80));
    assert_eq!(row.bid["CBID"], serde_json::json!(i128::MAX.to_string()));
    assert_eq!(row.bid_value, 10_i128.pow(30).to_string());
    assert_eq!(row.lot_value, (10_i128.pow(30) + 7).to_string());
    assert_eq!(row.est_profit, "-5");
    assert!(row.dry_run);
    Ok(())
}

/// The hash is a second write; a missing row is `false`, not an error.
#[sqlx::test(migrations = "./migrations")]
async fn a_fills_transaction_is_attached_once_it_has_one(db: sqlx::PgPool) -> sqlx::Result<()> {
    let store = Store::from_pool(db);
    let id = store.record_fill(&sample_fill()).await.expect("record");
    assert!(store.attach_fill_tx(id, &"ab".repeat(32)).await.expect("attach"));
    assert!(!store.attach_fill_tx(id + 1, &"cd".repeat(32)).await.expect("attach"));
    let hash = sqlx::query_scalar!("SELECT tx_hash FROM fills WHERE id = $1", id)
        .fetch_one(store.pool())
        .await?;
    assert_eq!(hash, Some("ab".repeat(32)));
    Ok(())
}

/// One transaction is one fill: the column is unique.
#[sqlx::test(migrations = "./migrations")]
async fn two_fills_cannot_share_a_transaction(db: sqlx::PgPool) -> sqlx::Result<()> {
    let store = Store::from_pool(db);
    let first = store.record_fill(&sample_fill()).await.expect("record");
    let second = store.record_fill(&sample_fill()).await.expect("record");
    assert!(store.attach_fill_tx(first, &"ab".repeat(32)).await.expect("attach"));
    assert!(store.attach_fill_tx(second, &"ab".repeat(32)).await.is_err());
    Ok(())
}

/// The plan is the filler's two columns and nothing else: the tracker's
/// `bid`, `lot` and `start_ledger` are untouched, `None` clears the plan,
/// and a row that has gone is `false`.
#[sqlx::test(migrations = "./migrations")]
async fn a_fill_plan_is_written_onto_its_auction_and_cleared(db: sqlx::PgPool) -> sqlx::Result<()> {
    let store = Store::from_pool(db);
    let auction = TrackedAuction {
        pool: POOL.to_string(),
        account: USER.to_string(),
        auction_type: AuctionType::UserLiquidation,
        start_ledger: 100,
        fill_ledger: None,
        percent: None,
        bid: BTreeMap::from([("CBID".to_string(), 5)]),
        lot: BTreeMap::from([("CLOT".to_string(), 9)]),
        updated_ledger: 101,
    };
    store.upsert_auction(&auction).await.expect("upsert");
    let percent = FillPercent::try_from(80).unwrap();
    assert!(store
        .set_fill_plan(POOL, USER, AuctionType::UserLiquidation, Some((310, percent)))
        .await
        .expect("plan"));
    let planned = store.auction(POOL, USER, AuctionType::UserLiquidation).await.expect("read").expect("row");
    assert_eq!((planned.fill_ledger, planned.percent), (Some(310), Some(percent)));
    assert_eq!(
        TrackedAuction { fill_ledger: None, percent: None, ..planned },
        auction,
        "only the plan columns moved"
    );
    assert!(store.set_fill_plan(POOL, USER, AuctionType::UserLiquidation, None).await.expect("clear"));
    let cleared = store.auction(POOL, USER, AuctionType::UserLiquidation).await.expect("read").expect("row");
    assert_eq!((cleared.fill_ledger, cleared.percent), (None, None));
    store.delete_auction(POOL, USER, AuctionType::UserLiquidation).await.expect("delete");
    assert!(!store.set_fill_plan(POOL, USER, AuctionType::UserLiquidation, None).await.expect("gone"));
    Ok(())
}
```

Use the store test module's existing pool and account constants if it has them (name them in the report); otherwise define `POOL` and `USER` as the harness's `harness::POOL` and `harness::USER_ONE`.

- [ ] **Step 3: Run them and watch them fail**

Run: `make db-reset && make db-up && sqlx migrate run && cargo test --lib store::`
Expected: compile errors — `FillRecord` and the three methods do not exist.

- [ ] **Step 4: Write it**

```rust
/// One fill the filler executed, as the `fills` table records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillRecord {
    /// The pool contract.
    pub pool: String,
    /// The liquidated account.
    pub account: String,
    /// Which auction; the filler fills user liquidations only.
    pub auction_type: AuctionType,
    /// The ledger the fill was planned for.
    pub fill_ledger: u32,
    /// The percent of the auction filled.
    pub percent: FillPercent,
    /// The d-tokens the filler takes on, per asset, scaled to `fill_ledger`
    /// and `percent`.
    pub bid: BTreeMap<String, i128>,
    /// The b-tokens the filler receives, per asset, scaled the same way.
    pub lot: BTreeMap<String, i128>,
    /// `bid`'s raw value in the pool oracle's units.
    pub bid_value: i128,
    /// `lot`'s raw value in the pool oracle's units.
    pub lot_value: i128,
    /// `lot_value − bid_value`. Negative only under `force_fill`.
    pub est_profit: i128,
    /// The bot's configured `DRY_RUN` mode when the row was written — the
    /// column's meaning, exactly as [`CreationRecord::dry_run`]'s, and never
    /// "whether this was sent": that is `tx_hash`.
    pub dry_run: bool,
}
```

`record_fill` inserts every column but `tx_hash` and `created_at`, binding the three values as `$n::text::numeric` from `to_string()` and the two maps through `asset_amounts_to_json`, and returns the `id` — the shape of `record_creation`. `attach_fill_tx` is `UPDATE fills SET tx_hash = $2 WHERE id = $1 AND tx_hash IS NULL`, `Ok(rows_affected == 1)` — append-only, so a repeated completion never replaces the hash the audit already names (*corrected during review*; `attach_creation_tx` gained the same guard) — with the doc `attach_creation_tx` has, adapted. `set_fill_plan` is `UPDATE auctions SET fill_ledger = $4, percent = $5 WHERE pool = $1 AND account = $2 AND auction_type = $3`, binding `None` as SQL `NULL`, `Ok(rows_affected == 1)`, documented: "the filler's plan and nothing else — the tracker owns every other column. `false` when the row has gone: the auction closed while it was being planned, which is not an error." Update the module doc's line about `fills` arriving with the phase that writes them.

- [ ] **Step 5: Regenerate the offline metadata, run, commit**

Run: `make sqlx-prepare && cargo test --lib store:: && make check`
Expected: PASS, and `.sqlx/` gains the new queries' files.

```bash
git add migrations/0003_fills.sql src/store.rs .sqlx
git commit -m "feat(store): the fills audit table and the filler's plan"
```

---

## Task 3: what an auction is worth, and when to fill it

**Files:**
- Create: `src/math/fill.rs` (the valuation half; Task 4 adds `plan_fill` to the same file)
- Modify: `src/math/auction.rs` (`RAMP_BLOCKS` and `RAMP_END_BLOCKS` become `pub`, with their existing docs)
- Modify: `src/math/mod.rs` (`pub mod fill;`)
- Test: inline in `src/math/fill.rs`

**Interfaces:**
- Consumes: `AuctionData`, `Positions`, `bid_modifier`, `lot_modifier`, `mul_ceil`, `mul_floor`, `MathError`, `SCALAR_7`.
- Produces:
  ```rust
  pub const FORCE_FILL_MAX_DELAY: u32 = 350;
  pub fn auction_positions(auction: &AuctionData, asset_index: &BTreeMap<String, u32>) -> Result<Positions, MathError>;
  pub fn fill_delay(lot_raw: i128, bid_raw: i128, profit_bps: u32, force_fill: bool) -> Result<u32, MathError>;
  pub fn meets_margin(delay: u32, lot_raw: i128, bid_raw: i128, profit_bps: u32) -> bool;
  pub fn health_floor(min_health_factor: i128, multiplier: i128) -> Result<i128, MathError>;
  pub fn to_oracle_units(value: i128, oracle_scalar: i128) -> Result<i128, MathError>;
  ```

Spec §5's closed form, and why it is exact: the contract's modifiers move in steps of `0_0050000`, exactly 1/200, so on the lot ramp the scaled lot is `lot × d / 200` and on the bid ramp the scaled bid is `bid × (400 − d) / 200`. Cross-multiplying by `200 × 10_000` puts every comparison in integers; `meets_margin` does that in `I256` and is what the closed form is proved against.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The closed form is the smallest delay that meets the margin: it
    /// meets it, and the ledger before does not. `meets_margin` is
    /// monotonic in the delay — the lot's modifier never falls and the
    /// bid's never rises — so checking `d − 1` is enough.
    #[test]
    fn the_fill_delay_is_the_first_ledger_that_meets_the_margin() {
        let values = [1_i128, 7, 999, 1_000, 1_001, 1_000_000_007, 10_i128.pow(20)];
        for lot in values {
            for bid in values {
                for profit_bps in [0_u32, 1, 500, 1_000, 10_000, 20_000] {
                    let delay = fill_delay(lot, bid, profit_bps, false).unwrap();
                    assert!(delay <= 400, "{lot} {bid} {profit_bps}: {delay}");
                    assert!(meets_margin(delay, lot, bid, profit_bps), "{lot} {bid} {profit_bps}: {delay} does not meet it");
                    if delay > 0 {
                        assert!(
                            !meets_margin(delay - 1, lot, bid, profit_bps),
                            "{lot} {bid} {profit_bps}: {} already met it", delay - 1
                        );
                    }
                }
            }
        }
    }

    /// On the lot ramp: $1,200 of lot against $1,000 of bid at 10% needs
    /// the lot at $1,100, which the ramp passes at ⌈200 × 1100 / 1200⌉ =
    /// 184 ledgers ($1,104); at 183 it is $1,098.
    #[test]
    fn a_lot_worth_more_than_the_margin_is_filled_on_the_lot_ramp() {
        assert_eq!(fill_delay(1_200, 1_000, 1_000, false).unwrap(), 184);
    }

    /// On the bid ramp: $900 of lot against $1,000 of bid at 10% needs the
    /// bid down to $818.18; 400 − ⌊200 × 900 / 1100⌋ = 400 − 163 = 237,
    /// where the bid is $815 and $815 × 1.1 = $896.50 ≤ $900. At 236 the
    /// bid is $820, and $902 is too much.
    #[test]
    fn a_lot_short_of_the_margin_waits_for_the_bid_ramp() {
        assert_eq!(fill_delay(900, 1_000, 1_000, false).unwrap(), 237);
    }

    /// Exactly the margin is met at 200: the whole lot against the whole
    /// bid.
    #[test]
    fn a_lot_exactly_at_the_margin_is_filled_at_200() {
        assert_eq!(fill_delay(1_100, 1_000, 1_000, false).unwrap(), 200);
    }

    /// The edges: nothing to pay is filled at once; nothing to receive is
    /// only "covered" once the bid has fallen to zero.
    #[test]
    fn a_zero_side_is_an_edge_not_an_error() {
        assert_eq!(fill_delay(1_000, 0, 1_000, false).unwrap(), 0);
        assert_eq!(fill_delay(0, 1_000, 1_000, false).unwrap(), 400);
    }

    /// `force_fill` never waits past 350, however little the lot covers.
    #[test]
    fn force_fill_caps_the_delay_at_350() {
        // ⌊200 × 100 / 1100⌋ = 18, so 382 without the cap.
        assert_eq!(fill_delay(100, 1_000, 1_000, false).unwrap(), 382);
        assert_eq!(fill_delay(100, 1_000, 1_000, true).unwrap(), 350);
        assert_eq!(fill_delay(1_200, 1_000, 1_000, true).unwrap(), 184, "under the cap it changes nothing");
    }

    /// A negative value is a bug upstream, not an auction.
    #[test]
    fn a_negative_value_is_refused() {
        assert!(fill_delay(-1, 1_000, 0, false).is_err());
        assert!(fill_delay(1_000, -1, 0, false).is_err());
    }

    /// The lot is collateral and the bid is liabilities, each by reserve
    /// index — the shape `calculate_position_data` values.
    #[test]
    fn an_auction_is_valued_as_a_position() {
        let auction = AuctionData {
            lot: BTreeMap::from([("L".to_string(), 5)]),
            bid: BTreeMap::from([("B".to_string(), 9)]),
            block: 1,
        };
        let index = BTreeMap::from([("L".to_string(), 3), ("B".to_string(), 0)]);
        let positions = auction_positions(&auction, &index).unwrap();
        assert_eq!(positions.collateral, BTreeMap::from([(3, 5)]));
        assert_eq!(positions.liabilities, BTreeMap::from([(0, 9)]));
        assert!(positions.supply.is_empty());
        let unknown = BTreeMap::from([("L".to_string(), 3)]);
        assert!(auction_positions(&auction, &unknown).is_err(), "an asset the pool does not list");
    }

    /// 1.5 × 1.1 = 1.65, rounded up — the floor errs toward safety.
    #[test]
    fn the_health_floor_is_the_pools_minimum_times_the_multiplier() {
        assert_eq!(health_floor(15_000_000, 11_000_000).unwrap(), 16_500_000);
        assert_eq!(health_floor(10_000_001, 11_000_000).unwrap(), 11_000_002, "⌈11_000_001.1⌉");
    }

    /// A 7-decimal config value in a 6-decimal oracle's units.
    #[test]
    fn a_config_value_is_rescaled_to_the_oracle() {
        assert_eq!(to_oracle_units(100_000_000, 10_000_000).unwrap(), 100_000_000);
        assert_eq!(to_oracle_units(100_000_000, 1_000_000).unwrap(), 10_000_000);
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib math::fill`
Expected: compile errors — the module does not exist.

- [ ] **Step 3: Write it**

```rust
//! The filler's arithmetic: what an auction is worth, the ledger to fill
//! it at, and — in [`plan_fill`] — the requests that keep the filler's own
//! position at or above its floor while it takes the auction over. Pure:
//! no I/O, and nothing panics.
//!
//! Values are in the pool oracle's units (`OraclePrices::scalar`), the
//! units `PositionData` reports. *Raw* values are what the filler is paid
//! and pays; *effective* values, after collateral and liability factors,
//! are what the contract's health check reads.

use std::collections::BTreeMap;

use ethnum::I256;

use super::auction::{bid_modifier, lot_modifier, AuctionData, RAMP_BLOCKS, RAMP_END_BLOCKS};
use super::fixed::{mul_ceil, mul_floor, MathError, SCALAR_7};
use super::position::Positions;

/// The latest delay, in ledgers from an auction's start, a `force_fill`
/// pool waits before filling, however little the lot then covers (spec
/// §5). Also the latest the health escalation may delay such a fill to.
pub const FORCE_FILL_MAX_DELAY: u32 = 350;

/// Basis points in one.
const BPS: i128 = 10_000;

/// An auction's two sides as a position: the lot's b-tokens as collateral
/// and the bid's d-tokens as liabilities, keyed by reserve index. That is
/// the shape `calculate_position_data` values, so an auction and a
/// borrower are valued by one function and cannot disagree about rounding.
///
/// # Errors
///
/// `MathError::InvalidInput` when the auction names an asset that is not
/// one of this pool's reserves.
pub fn auction_positions(
    auction: &AuctionData,
    asset_index: &BTreeMap<String, u32>,
) -> Result<Positions, MathError> {
    let index = |asset: &String| {
        asset_index.get(asset).copied().ok_or(MathError::InvalidInput(
            "an auction names an asset that is not a reserve of this pool",
        ))
    };
    let mut positions = Positions::default();
    for (asset, amount) in &auction.lot {
        positions.collateral.insert(index(asset)?, *amount);
    }
    for (asset, amount) in &auction.bid {
        positions.liabilities.insert(index(asset)?, *amount);
    }
    Ok(positions)
}

/// The fewest ledgers after an auction's start at which its lot covers its
/// bid plus `profit_bps`: the smallest `d` in `0..=400` with
/// `lot × lot_modifier(d) ≥ bid × bid_modifier(d) × (1 + p)`.
///
/// Spec §5's closed form. When the whole lot covers the bid plus margin,
/// the answer is on the lot ramp, `d = ⌈200 · bid · (1 + p) / lot⌉`;
/// otherwise it is on the bid ramp, `d = 400 − ⌊200 · lot / (bid · (1 +
/// p))⌋`. Both are exact in integers, because the contract's modifiers
/// move in steps of exactly 1/200 — [`meets_margin`] is the check it is
/// proved against. `force_fill` caps the answer at
/// [`FORCE_FILL_MAX_DELAY`].
///
/// # Errors
///
/// `MathError::InvalidInput` for a negative value; `Overflow` only for
/// values no auction holds.
pub fn fill_delay(
    lot_raw: i128,
    bid_raw: i128,
    profit_bps: u32,
    force_fill: bool,
) -> Result<u32, MathError> {
    if lot_raw < 0 || bid_raw < 0 {
        return Err(MathError::InvalidInput("an auction's value is never negative"));
    }
    let ramp = i128::from(RAMP_BLOCKS);
    let margin = BPS
        .checked_add(i128::from(profit_bps))
        .ok_or(MathError::Overflow)?;
    let delay = if bid_raw == 0 {
        0
    } else if lot_raw == 0 {
        RAMP_END_BLOCKS
    } else if I256::from(lot_raw) * I256::from(BPS) >= I256::from(bid_raw) * I256::from(margin) {
        // Lot ramp: the smallest d with lot · d / 200 ≥ bid · margin / BPS.
        let denominator = lot_raw.checked_mul(BPS).ok_or(MathError::Overflow)?;
        let factor = ramp.checked_mul(margin).ok_or(MathError::Overflow)?;
        let delay = mul_ceil(bid_raw, factor, denominator)?;
        u32::try_from(delay).map_err(|_| MathError::Overflow)?
    } else {
        // Bid ramp: the largest k = 400 − d with bid · k / 200 · margin ≤ lot · BPS.
        let denominator = bid_raw.checked_mul(margin).ok_or(MathError::Overflow)?;
        let factor = ramp.checked_mul(BPS).ok_or(MathError::Overflow)?;
        let covered = mul_floor(lot_raw, factor, denominator)?;
        // covered < 200 on this branch: lot · BPS < bid · margin.
        let covered = u32::try_from(covered).map_err(|_| MathError::Overflow)?;
        RAMP_END_BLOCKS
            .checked_sub(covered)
            .ok_or(MathError::Overflow)?
    };
    Ok(if force_fill {
        delay.min(FORCE_FILL_MAX_DELAY)
    } else {
        delay
    })
}

/// Whether an auction worth `lot_raw` against `bid_raw` meets `profit_bps`
/// when filled `delay` ledgers after its start, by the contract's own
/// modifiers and with no rounding anywhere: both sides are cross-multiplied
/// in 256 bits. What [`fill_delay`]'s closed form is proved against.
#[must_use]
pub fn meets_margin(delay: u32, lot_raw: i128, bid_raw: i128, profit_bps: u32) -> bool {
    let margin = I256::from(BPS) + I256::from(profit_bps);
    I256::from(lot_raw) * I256::from(lot_modifier(delay)) * I256::from(BPS)
        >= I256::from(bid_raw) * I256::from(bid_modifier(delay)) * margin
}

/// The health factor the filler keeps itself at or above after a fill:
/// the pool's `min_health_factor` times `HF_SAFETY_MULTIPLIER`, both 7
/// decimals, rounded up so the floor errs toward safety.
///
/// # Errors
///
/// `Overflow` only for inputs no configuration holds.
pub fn health_floor(min_health_factor: i128, multiplier: i128) -> Result<i128, MathError> {
    mul_ceil(min_health_factor, multiplier, SCALAR_7)
}

/// A 7-decimal configuration value in the pool oracle's own units, rounded
/// down.
///
/// # Errors
///
/// `Overflow` only for inputs no configuration holds.
pub fn to_oracle_units(value: i128, oracle_scalar: i128) -> Result<i128, MathError> {
    mul_floor(value, oracle_scalar, SCALAR_7)
}
```

Make `RAMP_BLOCKS` and `RAMP_END_BLOCKS` in `src/math/auction.rs` `pub` (their docs already say what they are), and add `pub mod fill;` to `src/math/mod.rs`. Do not add re-exports for the new module: callers name `math::fill::…`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib math::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/math/fill.rs src/math/auction.rs src/math/mod.rs
git commit -m "feat(math): value an auction and find the ledger to fill it at"
```

---

## Task 4: the health-bounded plan

**Files:**
- Modify: `src/math/fill.rs` (add `plan_fill` and its types below Task 3's code)
- Test: inline in `src/math/fill.rs`

**Interfaces:**
- Consumes: Task 3's `auction_positions`, `fill_delay`, `FORCE_FILL_MAX_DELAY`; `scale_auction`, `calculate_position_data`, `Reserve`, `OraclePrices`, `PositionData`, `Positions`, `FillPercent` (`crate::chain::xdr::encode::FillPercent`, as `math::liquidation` imports it), `div_ceil`, `mul_ceil`.
- Produces:
  ```rust
  pub struct FillTerms { pub min_collateral: i128, pub max_positions: u32, pub supply_allowed: bool,
      pub primary_asset: String, pub health_floor: i128, pub profit_bps: u32, pub force_fill: bool,
      pub plan_iterations: u32 }                                      // Debug, Clone, PartialEq, Eq
  pub struct FillInputs<'a> { pub reserves: &'a BTreeMap<u32, Reserve>, pub asset_index: &'a BTreeMap<String, u32>,
      pub prices: &'a OraclePrices, pub filler: &'a Positions, pub wallet: &'a BTreeMap<String, i128>,
      pub auction: &'a AuctionData, pub earliest_ledger: u32, pub max_percent: FillPercent } // Debug, Clone, Copy
  pub enum FillAction { Repay { asset: String, amount: i128 }, WithdrawAll { asset: String },
      SupplyCollateral { asset: String, amount: i128 } }            // Debug, Clone, PartialEq, Eq
  pub struct FillDraft { pub fill_ledger: u32, pub percent: FillPercent, pub actions: Vec<FillAction>,
      pub to_fill: AuctionData, pub lot_value: i128, pub bid_value: i128, pub est_profit: i128,
      pub spend: BTreeMap<String, i128>, pub projected_health: Option<i128> } // Debug, Clone, PartialEq, Eq
  pub enum FillSkip { Unprofitable, PastAuctionEnd, TooManyPositions, Unfunded, Health } // Debug, Clone, Copy, PartialEq, Eq
  pub enum PlannedFill { Fill(FillDraft), Skip(FillSkip) }            // Debug, Clone, PartialEq, Eq
  pub fn plan_fill(terms: &FillTerms, inputs: &FillInputs<'_>) -> Result<PlannedFill, MathError>;
  ```

The algorithm, which the doc comments must carry:

1. Value the whole auction. A lot worth nothing is `Unprofitable`. An auction whose earliest ledger is more than 400 past its start is `PastAuctionEnd` unless the pool is `force_fill` (spec §1).
2. The first candidate is `start + fill_delay(...)`, moved to the earliest ledger if it has already passed, at `max_percent`. The latest a candidate may be is `start + 350` under `force_fill`, else `start + 400` — or the earliest ledger, when that is already later.
3. Each round (at most `plan_iterations`) projects the candidate exactly: the fill's scaled lot and bid added to the filler's positions; `Repay` of each bid asset the wallet holds, the scaled d-tokens in underlying plus a 1 bp allowance, capped at the balance; `WithdrawAll` of each lot asset whose collateral factor is zero; `SupplyCollateral` of the primary asset sized so far, capped at what the wallet holds after the repays. A projection that raises the position count past `max_positions` is `TooManyPositions`. One with no liabilities, or at or above the floor with at least `min_collateral`, is the plan.
4. Short, in the spec's order: supply more of the primary for the shortfall, when the pool permits it (remember when the wallet capped it); else the largest lower percent that projects healthy; else the first later ledger at which `max_percent` projects healthy (ruling 10). Candidates are searched exactly (ruling 11).
5. Out of rounds or candidates: `Unfunded` if the wallet capped a supply along the way, else `Health`.

The request order a plan produces — fill, repays, withdrawals, supply — is the order the executor sends them in; the contract checks health once, after all of them, so order changes nothing but readability.

- [ ] **Step 1: Write the failing tests**

A three-reserve pool whose rates are exactly one, so b-tokens, d-tokens and underlying are the same number and every expected value below can be derived by hand. Every figure in a comment is in 7-decimal units: `1e10` is $1,000 of oracle value, and 1 XLM is `1e7` stroops.

```rust
#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::math::{ReserveConfig, ReserveData, SCALAR_12};

    const XLM: &str = "XLM";
    const USDC: &str = "USDC";
    const NO_CF: &str = "NOCF";
    const START: u32 = 1_000;

    fn reserve(asset: &str, index: u32, c_factor: u32, l_factor: u32) -> Reserve {
        Reserve::new(
            asset.to_string(),
            ReserveConfig {
                index,
                decimals: 7,
                c_factor,
                l_factor,
                util: 0,
                max_util: 9_500_000,
                r_base: 0,
                r_one: 0,
                r_two: 0,
                r_three: 0,
                reactivity: 0,
                supply_cap: i128::MAX,
                enabled: true,
            },
            ReserveData {
                d_rate: SCALAR_12,
                b_rate: SCALAR_12,
                ir_mod: SCALAR_7,
                b_supply: 0,
                d_supply: 0,
                backstop_credit: 0,
                last_time: 0,
            },
        )
        .expect("a test reserve")
    }

    struct Pool {
        reserves: BTreeMap<u32, Reserve>,
        asset_index: BTreeMap<String, u32>,
        prices: OraclePrices,
    }

    /// XLM at $0.10 with factors 0.75; USDC at $1 with factors 0.95; and a
    /// $1 reserve with no collateral factor at all.
    fn pool() -> Pool {
        Pool {
            reserves: BTreeMap::from([
                (0, reserve(XLM, 0, 7_500_000, 7_500_000)),
                (1, reserve(USDC, 1, 9_500_000, 9_500_000)),
                (2, reserve(NO_CF, 2, 0, 10_000_000)),
            ]),
            asset_index: BTreeMap::from([
                (XLM.to_string(), 0),
                (USDC.to_string(), 1),
                (NO_CF.to_string(), 2),
            ]),
            prices: OraclePrices::new(
                7,
                BTreeMap::from([
                    (XLM.to_string(), 1_000_000),
                    (USDC.to_string(), 10_000_000),
                    (NO_CF.to_string(), 10_000_000),
                ]),
            )
            .expect("prices"),
        }
    }

    /// Floor 1.1, 10% margin, XLM as the primary asset, $100 of minimum
    /// collateral.
    fn terms() -> FillTerms {
        FillTerms {
            min_collateral: 1_000_000_000,
            max_positions: 6,
            supply_allowed: true,
            primary_asset: XLM.to_string(),
            health_floor: 11_000_000,
            profit_bps: 1_000,
            force_fill: false,
            plan_iterations: 5,
        }
    }

    /// 20,000 XLM of lot — $2,000 raw, $1,500 effective — against 1,000
    /// USDC of bid — $1,000 raw, $1,052.63 effective (⌈1e10 / 0.95⌉ =
    /// 10_526_315_790). At 10% the lot covers the bid on the lot ramp at
    /// ⌈200 × 1100 / 2000⌉ = 110 ledgers.
    fn auction() -> AuctionData {
        AuctionData {
            lot: BTreeMap::from([(XLM.to_string(), 200_000_000_000)]),
            bid: BTreeMap::from([(USDC.to_string(), 10_000_000_000)]),
            block: START,
        }
    }

    fn percent(value: u32) -> FillPercent {
        FillPercent::try_from(value).expect("1..=100")
    }

    fn plan(
        terms: &FillTerms,
        pool: &Pool,
        filler: &Positions,
        wallet: &BTreeMap<String, i128>,
        auction: &AuctionData,
        earliest_ledger: u32,
        max_percent: FillPercent,
    ) -> PlannedFill {
        plan_fill(
            terms,
            &FillInputs {
                reserves: &pool.reserves,
                asset_index: &pool.asset_index,
                prices: &pool.prices,
                filler,
                wallet,
                auction,
                earliest_ledger,
                max_percent,
            },
        )
        .expect("no arithmetic failure")
    }

    fn draft(planned: PlannedFill) -> FillDraft {
        match planned {
            PlannedFill::Fill(draft) => draft,
            PlannedFill::Skip(skip) => panic!("expected a fill, got {skip:?}"),
        }
    }

    /// A filler holding $10,000 of USDC collateral ($9,500 effective) and
    /// nothing else: headroom for anything these tests auction.
    fn well_collateralised() -> Positions {
        Positions {
            collateral: BTreeMap::from([(1, 100_000_000_000)]),
            ..Positions::default()
        }
    }

    /// At a 200% margin the fill waits for the bid ramp: 400 − ⌊200 ×
    /// 2000 / 3000⌋ = 267. The bid there is 1e10 × 0.665 = 6.65e9 d-tokens,
    /// $665 raw and 7e9 effective (6.65e9 / 0.95, exactly), against $1,500
    /// effective lot: a health factor of ⌊1.5e10 × 1e7 / 7e9⌋ = 21_428_571
    /// with nothing from the wallet at all.
    #[test]
    fn a_fill_the_lot_carries_alone_needs_nothing_from_the_wallet() {
        let pool = pool();
        let terms = FillTerms { profit_bps: 20_000, ..terms() };
        let draft = draft(plan(&terms, &pool, &Positions::default(), &BTreeMap::new(), &auction(), START + 1, percent(100)));
        assert_eq!(draft.fill_ledger, START + 267);
        assert_eq!(draft.percent, percent(100));
        assert!(draft.actions.is_empty() && draft.spend.is_empty());
        assert_eq!(draft.to_fill.lot, BTreeMap::from([(XLM.to_string(), 200_000_000_000)]));
        assert_eq!(draft.to_fill.bid, BTreeMap::from([(USDC.to_string(), 6_650_000_000)]));
        assert_eq!((draft.lot_value, draft.bid_value), (20_000_000_000, 6_650_000_000));
        assert_eq!(draft.est_profit, 13_350_000_000);
        assert_eq!(draft.projected_health, Some(21_428_571));
    }

    /// At 110 ledgers the fill hands over 1.1e11 XLM ($825 effective)
    /// against the whole bid ($1,052.63 effective): 0.78, under the 1.1
    /// floor. The wallet's XLM closes it by supplying the primary asset.
    #[test]
    fn the_primary_asset_is_supplied_to_close_a_shortfall() {
        let pool = pool();
        let wallet = BTreeMap::from([(XLM.to_string(), 1_000_000_000_000)]);
        let draft = draft(plan(&terms(), &pool, &Positions::default(), &wallet, &auction(), START + 1, percent(100)));
        assert_eq!(draft.fill_ledger, START + 110);
        assert_eq!(draft.percent, percent(100));
        let [FillAction::SupplyCollateral { asset, amount }] = draft.actions.as_slice() else {
            panic!("expected one supply, got {:?}", draft.actions);
        };
        assert_eq!(asset.as_str(), XLM);
        assert!(*amount > 0 && *amount <= 1_000_000_000_000, "{amount}");
        assert_eq!(draft.spend, BTreeMap::from([(XLM.to_string(), *amount)]));
        let health = draft.projected_health.expect("the fill leaves liabilities");
        assert!(health >= 11_000_000, "{health}");
    }

    /// 1,000 XLM ($75 effective) cannot close the gap at 100%, so the whole
    /// of it is supplied and the percent comes down. With C(P) the
    /// collateral and L(P) the liabilities at P percent: C(22) = (2.42e10 +
    /// 1e10) × 0.75 / 10 = 2.565e9 and 1.1 × L(22) = 1.1 × ⌈2.2e9 / 0.95⌉
    /// = 2_547_368_422, so 22 holds; C(23) = 2.6475e9 against 2_663_157_896
    /// does not.
    #[test]
    fn a_wallet_short_of_the_primary_lowers_the_percent() {
        let pool = pool();
        let wallet = BTreeMap::from([(XLM.to_string(), 10_000_000_000)]);
        let draft = draft(plan(&terms(), &pool, &Positions::default(), &wallet, &auction(), START + 1, percent(100)));
        assert_eq!(draft.fill_ledger, START + 110);
        assert_eq!(draft.percent, percent(22));
        assert_eq!(
            draft.actions,
            vec![FillAction::SupplyCollateral { asset: XLM.to_string(), amount: 10_000_000_000 }]
        );
    }

    /// No wallet and no position: no supply, and no percent helps — the
    /// fill's own ratio is the filler's whole ratio at any size. So it
    /// waits until the lot ramp has the lot at 1.1 × 10_526_315_790 =
    /// 11_578_947_369 effective: at 155 ledgers it is 1.1625e10, at 154
    /// 1.155e10.
    #[test]
    fn with_no_inventory_and_no_headroom_the_fill_waits() {
        let pool = pool();
        let draft = draft(plan(&terms(), &pool, &Positions::default(), &BTreeMap::new(), &auction(), START + 1, percent(100)));
        assert_eq!(draft.fill_ledger, START + 155);
        assert_eq!(draft.percent, percent(100));
        assert!(draft.actions.is_empty());
    }

    /// A pool that does not permit supplying falls through to the same
    /// wait, whatever the wallet holds.
    #[test]
    fn nothing_is_supplied_where_the_pool_forbids_it() {
        let pool = pool();
        let terms = FillTerms { supply_allowed: false, ..terms() };
        let wallet = BTreeMap::from([(XLM.to_string(), 1_000_000_000_000)]);
        let draft = draft(plan(&terms, &pool, &Positions::default(), &wallet, &auction(), START + 1, percent(100)));
        assert_eq!(draft.fill_ledger, START + 155);
        assert!(draft.actions.is_empty());
    }

    /// USDC in the wallet repays the bid it names: the scaled d-tokens in
    /// underlying (1e10) plus a 1 bp allowance (1e6 + 1), and the contract
    /// refunds what is not owed. Repaid in full, the fill leaves no
    /// liabilities, so the contract checks nothing.
    #[test]
    fn a_bid_asset_in_the_wallet_is_repaid() {
        let pool = pool();
        let wallet = BTreeMap::from([(USDC.to_string(), 20_000_000_000)]);
        let draft = draft(plan(&terms(), &pool, &Positions::default(), &wallet, &auction(), START + 1, percent(100)));
        assert_eq!(draft.fill_ledger, START + 110);
        assert_eq!(
            draft.actions,
            vec![FillAction::Repay { asset: USDC.to_string(), amount: 10_001_000_001 }]
        );
        assert_eq!(draft.spend, BTreeMap::from([(USDC.to_string(), 10_001_000_001)]));
        assert_eq!(draft.projected_health, None);
        assert_eq!(draft.est_profit, 11_000_000_000 - 10_000_000_000);
    }

    /// A repay is capped at the balance; what it leaves (6e9 d-tokens,
    /// 6_315_789_474 effective, needing 6_947_368_422) the $825 of lot
    /// covers.
    #[test]
    fn a_repay_is_capped_at_what_the_wallet_holds() {
        let pool = pool();
        let wallet = BTreeMap::from([(USDC.to_string(), 4_000_000_000)]);
        let draft = draft(plan(&terms(), &pool, &Positions::default(), &wallet, &auction(), START + 1, percent(100)));
        assert_eq!(
            draft.actions,
            vec![FillAction::Repay { asset: USDC.to_string(), amount: 4_000_000_000 }]
        );
        assert!(draft.projected_health.expect("liabilities remain") >= 11_000_000);
    }

    /// Lot in a reserve with no collateral factor adds nothing to health
    /// and costs a position slot, so it is withdrawn in the same call.
    #[test]
    fn a_zero_collateral_factor_lot_is_withdrawn() {
        let pool = pool();
        let auction = AuctionData {
            lot: BTreeMap::from([
                (XLM.to_string(), 200_000_000_000),
                (NO_CF.to_string(), 10_000_000_000),
            ]),
            ..auction()
        };
        let draft = draft(plan(&terms(), &pool, &well_collateralised(), &BTreeMap::new(), &auction, START + 1, percent(100)));
        assert_eq!(draft.actions, vec![FillAction::WithdrawAll { asset: NO_CF.to_string() }]);
    }

    /// Spec §5's position cap: one position before, three after, and a cap
    /// of two.
    #[test]
    fn a_fill_past_the_pools_position_cap_is_skipped() {
        let pool = pool();
        let terms = FillTerms { max_positions: 2, ..terms() };
        let planned = plan(&terms, &pool, &well_collateralised(), &BTreeMap::new(), &auction(), START + 1, percent(100));
        assert_eq!(planned, PlannedFill::Skip(FillSkip::TooManyPositions));
    }

    /// Spec §1: past its 400th ledger an auction is filled only under
    /// `force_fill` — and then at once, with the bid at zero.
    #[test]
    fn past_its_end_only_a_force_fill_pool_fills() {
        let pool = pool();
        let late = START + 401;
        let planned = plan(&terms(), &pool, &well_collateralised(), &BTreeMap::new(), &auction(), late, percent(100));
        assert_eq!(planned, PlannedFill::Skip(FillSkip::PastAuctionEnd));
        let forced = FillTerms { force_fill: true, ..terms() };
        let draft = draft(plan(&forced, &pool, &well_collateralised(), &BTreeMap::new(), &auction(), late, percent(100)));
        assert_eq!(draft.fill_ledger, late);
        assert!(draft.to_fill.bid.is_empty(), "the bid has ramped to nothing");
    }

    /// $100 of lot against $1,000 of bid would wait 382 ledgers for its
    /// margin; `force_fill` fills at 350.
    #[test]
    fn force_fill_never_waits_past_350() {
        let pool = pool();
        let auction = AuctionData {
            lot: BTreeMap::from([(XLM.to_string(), 10_000_000_000)]),
            ..auction()
        };
        let patient = draft(plan(&terms(), &pool, &well_collateralised(), &BTreeMap::new(), &auction, START + 1, percent(100)));
        assert_eq!(patient.fill_ledger, START + 382);
        let forced = FillTerms { force_fill: true, ..terms() };
        let draft = draft(plan(&forced, &pool, &well_collateralised(), &BTreeMap::new(), &auction, START + 1, percent(100)));
        assert_eq!(draft.fill_ledger, START + 350);
    }

    /// One stroop of XLM is worth ⌊1e6 × 1 / 1e7⌋ = 0.
    #[test]
    fn a_lot_worth_nothing_is_unprofitable() {
        let pool = pool();
        let auction = AuctionData { lot: BTreeMap::from([(XLM.to_string(), 1)]), ..auction() };
        let planned = plan(&terms(), &pool, &well_collateralised(), &BTreeMap::new(), &auction, START + 1, percent(100));
        assert_eq!(planned, PlannedFill::Skip(FillSkip::Unprofitable));
    }

    /// A filler already under water — $7,500 effective against $10,526
    /// effective of its own debt — and nothing to supply: even at 400,
    /// with the bid gone, $9,000 against $11,579 needed.
    #[test]
    fn nothing_that_closes_the_gap_is_a_health_skip() {
        let pool = pool();
        let terms = FillTerms { supply_allowed: false, ..terms() };
        let under_water = Positions {
            collateral: BTreeMap::from([(0, 1_000_000_000_000)]),
            liabilities: BTreeMap::from([(1, 100_000_000_000)]),
            ..Positions::default()
        };
        let planned = plan(&terms, &pool, &under_water, &BTreeMap::new(), &auction(), START + 1, percent(100));
        assert_eq!(planned, PlannedFill::Skip(FillSkip::Health));
    }

    /// The same filler with a little XLM it may supply: more XLM would have
    /// closed it, so the skip says the wallet was short, not the plan.
    #[test]
    fn a_shortfall_only_more_primary_would_close_is_unfunded() {
        let pool = pool();
        let under_water = Positions {
            collateral: BTreeMap::from([(0, 1_000_000_000_000)]),
            liabilities: BTreeMap::from([(1, 100_000_000_000)]),
            ..Positions::default()
        };
        let wallet = BTreeMap::from([(XLM.to_string(), 1_000_000_000)]);
        let planned = plan(&terms(), &pool, &under_water, &wallet, &auction(), START + 1, percent(100));
        assert_eq!(planned, PlannedFill::Skip(FillSkip::Unfunded));
    }

    /// The executor's re-plan passes a lower ceiling, and the plan honours
    /// it.
    #[test]
    fn the_percent_never_exceeds_the_ceiling() {
        let pool = pool();
        let terms = FillTerms { profit_bps: 20_000, ..terms() };
        let draft = draft(plan(&terms, &pool, &Positions::default(), &BTreeMap::new(), &auction(), START + 1, percent(50)));
        assert_eq!(draft.percent, percent(50));
    }

    /// Nothing a plan spends exceeds what the wallet holds, whichever way
    /// it closes the gap.
    #[test]
    fn a_plan_never_spends_more_than_the_wallet_holds() {
        let pool = pool();
        for wallet in [
            BTreeMap::new(),
            BTreeMap::from([(XLM.to_string(), 10_000_000_000)]),
            BTreeMap::from([(USDC.to_string(), 4_000_000_000), (XLM.to_string(), 7)]),
            BTreeMap::from([(USDC.to_string(), 20_000_000_000), (XLM.to_string(), 1_000_000_000_000)]),
        ] {
            if let PlannedFill::Fill(draft) = plan(&terms(), &pool, &Positions::default(), &wallet, &auction(), START + 1, percent(100)) {
                for (asset, spent) in &draft.spend {
                    assert!(*spent <= wallet.get(asset).copied().unwrap_or(0), "{asset}: {spent} of {wallet:?}");
                }
            }
        }
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib math::fill`
Expected: compile errors — `plan_fill` and its types do not exist.

- [ ] **Step 3: Write it**

Extend the file's imports with `scale_auction`, `calculate_position_data`, `OraclePrices`, `PositionData`, `Reserve`, `div_ceil`, and `crate::chain::xdr::encode::FillPercent`.

```rust
/// What the pool and the operator hold one fill to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillTerms {
    /// The pool's `min_collateral`, in oracle units: the least effective
    /// collateral the contract lets a position with liabilities keep.
    pub min_collateral: i128,
    /// The pool's `max_positions`.
    pub max_positions: u32,
    /// Whether `SupplyCollateral` of the primary asset is allowed now: the
    /// pool's status is 3 or below and the primary reserve is enabled.
    pub supply_allowed: bool,
    /// The asset the filler keeps as collateral, and supplies to cover a
    /// shortfall.
    pub primary_asset: String,
    /// [`health_floor`] of the pool's `min_health_factor` and
    /// `HF_SAFETY_MULTIPLIER`, 7 decimals.
    pub health_floor: i128,
    /// The margin [`fill_delay`] waits for, in basis points.
    pub profit_bps: u32,
    /// Fill by [`FORCE_FILL_MAX_DELAY`], and past the auction's end at all.
    pub force_fill: bool,
    /// How many rounds of supply → percent → delay the plan may take.
    pub plan_iterations: u32,
}

/// The chain state one plan is made against, all read at one ledger.
#[derive(Debug, Clone, Copy)]
pub struct FillInputs<'a> {
    /// The pool's reserves, accrued to the valuation time.
    pub reserves: &'a BTreeMap<u32, Reserve>,
    /// Asset address to reserve index.
    pub asset_index: &'a BTreeMap<String, u32>,
    /// The pool oracle's prices.
    pub prices: &'a OraclePrices,
    /// The filler's own positions in this pool before the fill.
    pub filler: &'a Positions,
    /// What the filler's wallet may spend, per asset: its balance less the
    /// fee reserve and every live reservation.
    pub wallet: &'a BTreeMap<String, i128>,
    /// The auction as the chain holds it now.
    pub auction: &'a AuctionData,
    /// The first ledger a transaction sent now could land in.
    pub earliest_ledger: u32,
    /// The largest percent the plan may name: 100, except on the
    /// executor's one re-plan after the contract refused a health check.
    pub max_percent: FillPercent,
}

/// One request a plan adds after the fill itself, in the order sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillAction {
    /// Repay `amount` of `asset` from the wallet. The contract refunds
    /// whatever exceeds the debt, so the allowance above the scaled bid
    /// costs nothing but must be held.
    Repay {
        /// The bid asset.
        asset: String,
        /// Underlying, in the asset's decimals.
        amount: i128,
    },
    /// Withdraw every b-token of `asset`, a reserve with no collateral
    /// factor: it adds nothing to health and costs a position slot.
    WithdrawAll {
        /// The lot asset.
        asset: String,
    },
    /// Supply `amount` of the primary asset as collateral.
    SupplyCollateral {
        /// The primary asset.
        asset: String,
        /// Underlying, in the asset's decimals.
        amount: i128,
    },
}

/// A fill the filler's own position can carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillDraft {
    /// The ledger to fill at.
    pub fill_ledger: u32,
    /// The percent of the auction to fill.
    pub percent: FillPercent,
    /// The requests after the fill, in the order sent.
    pub actions: Vec<FillAction>,
    /// What the fill hands over at `fill_ledger` and `percent`.
    pub to_fill: AuctionData,
    /// `to_fill.lot`'s raw value, oracle units.
    pub lot_value: i128,
    /// `to_fill.bid`'s raw value, oracle units.
    pub bid_value: i128,
    /// `lot_value − bid_value`.
    pub est_profit: i128,
    /// The wallet amounts `actions` spend, per asset: what a live plan
    /// reserves.
    pub spend: BTreeMap<String, i128>,
    /// The filler's projected health factor, in the oracle's scale; `None`
    /// when the fill leaves it no liabilities and the contract checks
    /// nothing.
    pub projected_health: Option<i128>,
}

/// Why no fill was planned. A closed set: each is a metric label in Phase 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillSkip {
    /// The lot is worth nothing at the oracle's prices.
    Unprofitable,
    /// Past its 400th ledger, in a pool that is not `force_fill`.
    PastAuctionEnd,
    /// The fill would take the filler past the pool's `max_positions`.
    TooManyPositions,
    /// More of the primary asset would have closed the shortfall, and the
    /// wallet does not hold it.
    Unfunded,
    /// Nothing within `plan_iterations` holds the filler's floor.
    Health,
}

/// What [`plan_fill`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedFill {
    /// Fill as drafted.
    Fill(FillDraft),
    /// Not this auction, for this reason.
    Skip(FillSkip),
}

/// The allowance a repay adds above the scaled bid, as a fraction of it:
/// one basis point, which covers a few hours of interest at any rate the
/// pool allows. The contract refunds the excess.
const REPAY_ALLOWANCE_BPS: i128 = 1;

/// Units added to a supply for the b-token round trip's two floors; the
/// next projection verifies the result either way.
const SUPPLY_ROUNDING_ALLOWANCE: i128 = 2;

/// Plans one fill (spec §5, "Health-bounded plan"). [the algorithm, as
/// numbered above this task's code block, goes here as the doc comment]
///
/// # Errors
///
/// `MathError` for an auction naming an asset the pool does not list, a
/// reserve or price missing, or arithmetic no real auction reaches.
pub fn plan_fill(terms: &FillTerms, inputs: &FillInputs<'_>) -> Result<PlannedFill, MathError> {
    let whole = calculate_position_data(
        inputs.reserves,
        inputs.prices,
        &auction_positions(inputs.auction, inputs.asset_index)?,
    )?;
    if whole.collateral_raw == 0 {
        return Ok(PlannedFill::Skip(FillSkip::Unprofitable));
    }
    let start = inputs.auction.block;
    let earliest = inputs.earliest_ledger.max(start);
    // `earliest >= start` by the line above.
    if earliest - start > RAMP_END_BLOCKS && !terms.force_fill {
        return Ok(PlannedFill::Skip(FillSkip::PastAuctionEnd));
    }
    let delay = fill_delay(whole.collateral_raw, whole.liability_raw, terms.profit_bps, terms.force_fill)?;
    let mut ledger = start.checked_add(delay).ok_or(MathError::Overflow)?.max(earliest);
    let reach = if terms.force_fill { FORCE_FILL_MAX_DELAY } else { RAMP_END_BLOCKS };
    let last = start.checked_add(reach).ok_or(MathError::Overflow)?.max(ledger);
    let mut percent = inputs.max_percent;
    let mut supply = 0_i128;
    let mut unfunded = false;

    for _ in 0..terms.plan_iterations {
        let projection = project(terms, inputs, ledger, percent, supply)?;
        if over_positions(terms, inputs, &projection) {
            return Ok(PlannedFill::Skip(FillSkip::TooManyPositions));
        }
        if healthy(terms, &projection)? {
            return Ok(PlannedFill::Fill(draft(inputs, ledger, percent, projection)?));
        }
        if terms.supply_allowed {
            let wanted = supply
                .checked_add(supply_for(terms, inputs, &projection.data)?)
                .ok_or(MathError::Overflow)?;
            let next = wanted.min(projection.primary_available);
            unfunded |= wanted > projection.primary_available;
            if next > supply {
                supply = next;
                continue;
            }
        }
        if let Some(lower) = largest_healthy_percent(terms, inputs, ledger, percent, supply)? {
            percent = lower;
            continue;
        }
        if let Some(later) = first_healthy_ledger(terms, inputs, ledger, last, supply)? {
            ledger = later;
            percent = inputs.max_percent;
            continue;
        }
        break;
    }
    Ok(PlannedFill::Skip(if unfunded { FillSkip::Unfunded } else { FillSkip::Health }))
}
```

The private helpers, each with a doc comment stating its constraint:

- `struct Projection { to_fill: AuctionData, positions: Positions, actions: Vec<FillAction>, spend: BTreeMap<String, i128>, primary_available: i128, data: PositionData }` — `primary_available` is the primary asset the wallet holds **after the repays and before the supply**, which is what caps the supply.
- `fn reserve_for<'a>(inputs: &FillInputs<'a>, asset: &str) -> Result<(u32, &'a Reserve), MathError>` — index via `asset_index` (`InvalidInput` when absent), reserve via `reserves` (`MissingReserve` when absent).
- `fn project(terms, inputs, ledger: u32, percent: FillPercent, supply: i128) -> Result<Projection, MathError>` — in this order: `scale_auction(inputs.auction, ledger, percent.get())?.to_fill`; clone `inputs.filler` and add the scaled lot to `collateral` and the scaled bid to `liabilities` by index (checked adds); clone `inputs.wallet`; for each bid asset with a positive balance, `owed = to_asset_from_d_token(d)`, `amount = min(owed + owed * REPAY_ALLOWANCE_BPS / 10_000 + 1, held)`, `burnt = min(to_d_token_down(amount), owing)`, reduce the liability (removing the entry at zero), debit the wallet copy, add to `spend`, push `Repay`; for each lot asset whose reserve's `c_factor == 0`, remove that collateral entry and push `WithdrawAll`; read `primary_available` from the wallet copy (`max(0)`); clamp `supply` to it, and when positive add `to_b_token_down(supply)` to the primary's collateral, add to `spend`, push `SupplyCollateral`; finally `calculate_position_data(inputs.reserves, inputs.prices, &positions)`.
- `fn healthy(terms, projection) -> Result<bool, MathError>` — `true` when `projection.positions.liabilities.is_empty()` (the contract checks nothing then), else `!data.is_hf_under(terms.health_floor)? && data.collateral_base >= terms.min_collateral`.
- `fn over_positions(terms, inputs, projection) -> bool` — `after > before && after > max_positions`, the contract's `require_under_max`, with `before = inputs.filler.effective_count()`; a count that does not fit `u32` is over.
- `fn supply_for(terms, inputs, data) -> Result<i128, MathError>` — the shortfall `max(mul_ceil(data.liability_base, terms.health_floor, SCALAR_7), terms.min_collateral) − data.collateral_base`, floored at zero; zero when the primary reserve's `c_factor` is zero; else `div_ceil(mul_ceil(shortfall, reserve.scalar, price)?, i128::from(c_factor), SCALAR_7)? + SUPPLY_ROUNDING_ALLOWANCE`.
- `fn largest_healthy_percent(terms, inputs, ledger, below: FillPercent, supply) -> Result<Option<FillPercent>, MathError>` — `(1..below.get()).rev()`, the first whose projection is `healthy`.
- `fn first_healthy_ledger(terms, inputs, after: u32, last: u32, supply) -> Result<Option<u32>, MathError>` — `after + 1 ..= last` (checked: `after == u32::MAX` is `None`), the first whose projection at `inputs.max_percent` is `healthy`.
- `fn draft(inputs, ledger, percent, projection) -> Result<FillDraft, MathError>` — values `projection.to_fill` with `auction_positions` + `calculate_position_data` for `lot_value` (`collateral_raw`) and `bid_value` (`liability_raw`), `est_profit` by checked subtraction, and `projected_health` `None` when the liabilities are empty, else `data.health_factor()?`.

`FillPercent::try_from` returns an `XdrError`; map it to `MathError::InvalidInput("a fill percent is 1 to 100")` where the search builds one.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib math::fill`
Expected: PASS. **If a hand-derived figure in a test disagrees with the code, re-derive it from the formulas in that test's comment before touching either** — the figures were derived from the contract's own rounding, and a figure changed to match the code proves nothing. Report any you changed, with the derivation.

- [ ] **Step 5: Commit**

```bash
git add src/math/fill.rs
git commit -m "feat(math): the health-bounded fill plan"
```

---

## Task 5: a queue that never sends past an unknown outcome

**Files:**
- Modify: `src/queue.rs` (`Submission::retries`, `RetryPolicy`, `CREATION_RETRIES`, `FILL_RETRIES`, `run_queue_with`; `run_queue` delegates to it)
- Modify: `src/chain/script.rs` (shared scripting helpers, moved from `auctioneer.rs`'s tests)
- Modify: `src/auctioneer.rs` (its `Submission` carries `retries: CREATION_RETRIES`; its tests call the moved helpers)
- Test: inline in `src/queue.rs`

**Interfaces:**
- Consumes: `Submitter::{prepare, send, wait_for}`, `ChainError`, `TxOutcome`.
- Produces:
  ```rust
  pub struct Submission { pub operation: Operation, pub priority: Priority, pub label: String,
                          pub retries: u32 }        // Debug still prints label and priority only
  pub struct RetryPolicy { pub initial: Duration, pub max: Duration, pub resolve_pause: Duration }
  impl RetryPolicy { pub const DEFAULT: Self; }     // 1 s, 30 s, 1 s
  pub const CREATION_RETRIES: u32 = 3;
  pub const FILL_RETRIES: u32 = 10;
  pub async fn run_queue(submitter: &Submitter<'_>, receiver: mpsc::Receiver<QueuedSubmission>,
                         shutdown: &watch::Receiver<bool>);             // unchanged signature
  pub async fn run_queue_with(submitter: &Submitter<'_>, receiver: mpsc::Receiver<QueuedSubmission>,
                              shutdown: &watch::Receiver<bool>, policy: RetryPolicy);
  // src/chain/script.rs, cfg(test), pub(crate):
  pub(crate) fn script_simulate_prelude(rpc: &ScriptedRpc, signer: &Signer, sequence: i64, ledger: u32);
  pub(crate) fn script_prepare_prelude(rpc: &ScriptedRpc, signer: &Signer, sequence: i64, ledger: u32);
  pub(crate) fn script_simulate_accepted(rpc: &ScriptedRpc, ledger: u32);
  pub(crate) fn script_simulate_refused(rpc: &ScriptedRpc, code: u32, ledger: u32);
  pub(crate) fn script_simulate_needs_restore(rpc: &ScriptedRpc, ledger: u32);
  pub(crate) fn script_send(rpc: &ScriptedRpc, status: &str, error: Option<TransactionResultResult>, ledger: u32);
  pub(crate) fn script_transaction_success(rpc: &ScriptedRpc, ledger: u32);
  pub(crate) fn script_transaction_not_found(rpc: &ScriptedRpc, latest: u32, oldest: u32);
  ```

Ruling 6 is this task. What each failure proves, and so what the queue does with it:

| Failure | Proves | The queue |
|---|---|---|
| `prepare` fails: `Transport`, `Http`, `Rpc`, `LedgerMoved` | nothing of this operation was sent | retries with a fresh `prepare`, within the budget |
| `send` answers `Rejected` (an `ERROR` status, or `TRY_AGAIN_LATER` twice) | the RPC refused the envelope; nothing will land | retries with a fresh `prepare`, within the budget |
| `send` fails any other way (transport, HTTP, JSON-RPC error, hash mismatch) | nothing: the RPC may have forwarded it | resolves by the hash it holds, **never resends** |
| `wait_for` answers `Unknown` | nothing yet | polls again until terminal; answers `Unknown` only once shutdown is requested |
| `send` answers `BadSequence` | another signer of this account got in first; the plan is stale | answers the caller, who re-plans |
| `prepare` fails with `Simulation` (a contract refusal), `NoAccount`, `Restore`, `RestoreUnknown`, `Shape`, `Xdr`, `Math`, `Config` | retrying cannot change it | answers the caller |
| `Expired` | it never landed | answers the caller, who re-plans (spec §8) |

- [ ] **Step 1: Move the shared scripting helpers**

Move `script_simulate_prelude`, `script_prepare_prelude`, `script_simulate_accepted`, `script_simulate_refused` and `script_simulate_needs_restore` from `src/auctioneer.rs`'s test module into `src/chain/script.rs` as `pub(crate)` free functions with their bodies and docs unchanged, and make `auctioneer.rs`'s tests import them. Add three more beside them, built from the wire shapes `src/chain/tx.rs`'s own tests use:

- `script_send(rpc, status, error, ledger)` — `sendTransaction` answering `{"status": status, "hash": "$ENVELOPE_HASH", "latestLedger": ledger, "latestLedgerCloseTime": "1"}`, plus `"errorResultXdr": result_b64(error)` when `error` is `Some`.
- `script_transaction_success(rpc, ledger)` — the `getTransaction` `SUCCESS` answer `auctioneer.rs`'s `an_armed_creation_is_queued_and_recorded_with_its_hash` scripts, with `"txHash": "$ENVELOPE_HASH"`.
- `script_transaction_not_found(rpc, latest, oldest)` — `{"status": "NOT_FOUND", "latestLedger": latest, "oldestLedger": oldest, "ledger": 0}`.

Run `cargo test --lib auctioneer::` — every auctioneer test still passes, unchanged but for its imports.

- [ ] **Step 2: Write the failing tests**

In `src/queue.rs`'s test module. A real `Submitter` over a `ScriptedRpc`, as `run_queue_answers_rather_than_attempts_after_shutdown` builds one, with a `TxConfig` whose `poll_interval` and `wait_cap` are a few milliseconds and `send_retry_pause` zero, and `run_queue_with` driven by a policy of millisecond pauses:

```rust
/// A policy fast enough for a test; the shape of `RetryPolicy::DEFAULT`.
fn quick() -> RetryPolicy {
    RetryPolicy {
        initial: Duration::from_millis(1),
        max: Duration::from_millis(4),
        resolve_pause: Duration::from_millis(1),
    }
}

/// A send whose answer was lost may still have been forwarded, so the
/// queue asks the chain about the hash it holds — and finds it landed.
/// Resending would have been a second transaction for one plan.
#[tokio::test]
async fn a_send_whose_answer_was_lost_is_resolved_by_hash_never_resent() {
    // script_prepare_prelude + script_simulate_accepted; then
    // rpc.expect_http("sendTransaction", 500); then script_transaction_success.
    // Enqueue with retries: 3 through run_queue_with(.., quick()).
    // Assert: Ok(TxOutcome::Succeeded { .. }), and exactly one
    // sendTransaction and one simulateTransaction call.
}

/// An outcome the RPC cannot yet name is polled until it can be named.
/// Moving on would let the next submission for this key be prepared
/// against a sequence number this transaction may still consume.
#[tokio::test]
async fn an_unknown_outcome_is_polled_until_it_is_terminal() {
    // Prepare prelude + accepted simulation; script_send("PENDING", None, 100);
    // three script_transaction_not_found(rpc, 100, 1) — latest far under the
    // window's max ledger, so none is provably expired and each wait_for
    // gives up as Unknown at the tiny wait cap — then
    // script_transaction_success. Assert Succeeded and four getTransaction
    // calls.
}

/// A second submission is not prepared while the first is unresolved.
#[tokio::test]
async fn nothing_else_is_sent_for_the_key_until_the_first_is_settled() {
    // Two submissions enqueued back to back. The first: prelude, accepted,
    // PENDING, two NOT_FOUNDs, SUCCESS. The second: prelude, accepted,
    // PENDING, SUCCESS. Assert both succeed, and that the second
    // submission's account read — the fourth getLedgerEntries-or-later
    // call in `rpc.received()` order — comes after the first's last
    // getTransaction.
}

/// The RPC refusing the envelope proves nothing landed, so the queue
/// prepares again from fresh state — a new sequence read and a new
/// simulation — within the submission's budget.
#[tokio::test]
async fn a_refused_send_is_retried_with_a_fresh_prepare() {
    // Prelude, accepted, script_send("ERROR", Some(TxInsufficientFee), 100);
    // then prelude, accepted, PENDING, SUCCESS. retries: 3.
    // Assert Succeeded, two sendTransaction and two simulateTransaction calls.
}

/// The budget is a bound: `retries: 2` is three attempts in all.
#[tokio::test]
async fn retries_stop_at_the_submissions_budget() {
    // Three rounds of prelude, accepted, ERROR(TxInsufficientFee). retries: 2.
    // Assert Err(QueueError::Chain(ChainError::Rejected(_))) and three sends.
}

/// A bad sequence means the plan is stale: it goes back to the caller to
/// be rebuilt, however much budget is left.
#[tokio::test]
async fn a_bad_sequence_goes_back_to_the_caller() {
    // Prelude, accepted, ERROR(TxBadSeq). retries: 5.
    // Assert Err(QueueError::Chain(ChainError::BadSequence)) and one send.
}

/// A contract refusal at prepare cannot change on a retry.
#[tokio::test]
async fn a_contract_refusal_goes_back_to_the_caller() {
    // script_simulate_prelude + script_simulate_refused(rpc, 1205, 100). retries: 5.
    // Assert Err(QueueError::Chain(ChainError::Simulation { contract_error: Some(1205), .. })),
    // one simulateTransaction call and no sendTransaction call.
}

/// An expired transaction never landed; whether to try again is the
/// caller's decision, made against fresh state (spec §8).
#[tokio::test]
async fn an_expired_transaction_goes_back_to_the_caller() {
    // Prelude, accepted, PENDING; then script_transaction_not_found with
    // latest past the window's max ledger and oldest at 1 — provably
    // expired. retries: 5. Assert Ok(TxOutcome::Expired { .. }) and one send.
}

/// Shutdown cuts a backoff short: the caller hears the failure at once.
#[tokio::test]
async fn shutdown_cuts_a_backoff_short() {
    // Prelude, accepted, ERROR(TxInsufficientFee). retries: 5, a policy
    // whose initial pause is an hour. Raise the shutdown flag from a task
    // that waits for the first sendTransaction call to appear. Assert the
    // enqueue answers within a second with Err(Chain(Rejected)), one send.
}
```

Write every body out; the comments say what each scripts and asserts. Existing tests that build a `Submission` gain `retries: 0`.

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test --lib queue::`
Expected: compile errors — `retries`, `RetryPolicy` and `run_queue_with` do not exist.

- [ ] **Step 4: Write it**

```rust
/// Retries an auction creation gets after a failure that sent nothing
/// (spec §8).
pub const CREATION_RETRIES: u32 = 3;
/// Retries a fill gets (spec §8): more than a creation, because a fill
/// that lapses is money another bot takes.
pub const FILL_RETRIES: u32 = 10;

/// How the queue paces what a submission's budget allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The pause before the first retry; each later one doubles it.
    pub initial: Duration,
    /// The longest pause.
    pub max: Duration,
    /// The pause between two polls of an outcome still unknown.
    pub resolve_pause: Duration,
}

impl RetryPolicy {
    /// Spec §8's backoff: one second, doubling, to thirty.
    pub const DEFAULT: Self = Self {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(30),
        resolve_pause: Duration::from_secs(1),
    };
}
```

`run_queue` becomes `run_queue_with(submitter, receiver, shutdown, RetryPolicy::DEFAULT)`, and its doc moves to `run_queue_with` with this added: every submission is answered only once its outcome is terminal (`Succeeded`, `Failed`, `Expired`), or with `Unknown` once shutdown interrupts the resolution — never while the next submission for this key could be prepared against a sequence number it may still consume.

```rust
/// One submission, to an answer the caller can act on. Retries only what
/// provably never reached the network, pausing between attempts.
async fn submit_until_settled(
    submitter: &Submitter<'_>,
    submission: &Submission,
    shutdown: &watch::Receiver<bool>,
    policy: RetryPolicy,
) -> Result<TxOutcome, QueueError> {
    let mut retries_left = submission.retries;
    let mut pause = policy.initial;
    loop {
        match attempt(submitter, submission, shutdown, policy).await {
            Err(error) if sent_nothing(&error) && retries_left > 0 => {
                retries_left -= 1;
                tracing::warn!(
                    label = submission.label,
                    %error,
                    retries_left,
                    "nothing was sent; preparing again from fresh state"
                );
                if !pause_unless_shutdown(pause, shutdown).await {
                    return Err(QueueError::Chain(error));
                }
                pause = pause.saturating_mul(2).min(policy.max);
            }
            outcome => return outcome.map_err(QueueError::Chain),
        }
    }
}

/// Prepare, send, and resolve. A send whose answer is lost is resolved by
/// hash like any other — the RPC may have forwarded it.
async fn attempt(
    submitter: &Submitter<'_>,
    submission: &Submission,
    shutdown: &watch::Receiver<bool>,
    policy: RetryPolicy,
) -> Result<TxOutcome, ChainError> {
    let prepared = submitter
        .prepare(submission.operation.clone(), submission.priority)
        .await?;
    tracing::info!(
        label = submission.label,
        hash = %prepared.hash,
        sequence = prepared.sequence,
        max_ledger = prepared.window.max_ledger(),
        fee = prepared.fee,
        "sending transaction"
    );
    match submitter.send(&prepared).await {
        Ok(()) => {}
        Err(error @ (ChainError::BadSequence | ChainError::Rejected(_))) => return Err(error),
        Err(error) => tracing::warn!(
            label = submission.label,
            hash = %prepared.hash,
            %error,
            "the send's answer was lost; resolving by hash rather than resending"
        ),
    }
    resolve(submitter, &prepared, shutdown, policy).await
}

/// `wait_for` until the outcome is terminal. `Unknown` comes back only
/// when shutdown has been requested.
async fn resolve(
    submitter: &Submitter<'_>,
    prepared: &Prepared,
    shutdown: &watch::Receiver<bool>,
    policy: RetryPolicy,
) -> Result<TxOutcome, ChainError> {
    loop {
        let outcome = submitter
            .wait_for(prepared.hash, prepared.sequence, prepared.window)
            .await?;
        if !matches!(outcome, TxOutcome::Unknown { .. }) || *shutdown.borrow() {
            return Ok(outcome);
        }
        tracing::warn!(
            hash = %prepared.hash,
            "the outcome is still unknown; nothing else is sent for this key until it is known"
        );
        if !pause_unless_shutdown(policy.resolve_pause, shutdown).await {
            return Ok(outcome);
        }
    }
}

/// Whether `error` proves nothing of this submission reached the network:
/// a `prepare` that failed before sending, or a send the RPC refused.
/// Everything a send can fail with *after* the envelope left is handled in
/// `attempt`, by hash, and never reaches this.
fn sent_nothing(error: &ChainError) -> bool {
    matches!(
        error,
        ChainError::Rejected(_)
            | ChainError::Transport(_)
            | ChainError::Http(_)
            | ChainError::Rpc { .. }
            | ChainError::LedgerMoved { .. }
    )
}

/// Sleeps `pause`, cut short by a shutdown request. `false` when it was.
async fn pause_unless_shutdown(pause: Duration, shutdown: &watch::Receiver<bool>) -> bool {
    let mut shutdown = shutdown.clone();
    tokio::select! {
        () = tokio::time::sleep(pause) => !*shutdown.borrow(),
        _ = shutdown.wait_for(|stopping| *stopping) => false,
    }
}
```

`Submission` gains `/// Further attempts after a failure that provably sent nothing; zero for none. What each role gets is spec §8's: CREATION_RETRIES, FILL_RETRIES.` `pub retries: u32`. The hand-written `Debug`s stay as they are. `Auctioneer::submit_recorded` builds its `Submission` with `retries: CREATION_RETRIES`. Update the module doc: the queue owns ordering *and* the rule that nothing is sent for a key while an earlier transaction's outcome is unknown; what to submit, and what a failure means, stay the caller's.

- [ ] **Step 5: Run the tests and the suite**

Run: `cargo test --lib queue:: auctioneer::` then `make check`.
Expected: PASS. The auctioneer's armed tests pass unchanged: each scripts a terminal outcome.

- [ ] **Step 6: Commit**

```bash
git add src/queue.rs src/chain/script.rs src/auctioneer.rs
git commit -m "feat(queue): resolve every submission before the next, retry only what never landed"
```

---

## Task 6: the filler's inventory

**Files:**
- Create: `src/inventory.rs`
- Modify: `src/liquidator.rs` (`pub mod inventory;`)
- Test: inline in `src/inventory.rs`

**Interfaces:**
- Consumes: `PoolReader::balance`, `ChainError`.
- Produces:
  ```rust
  pub struct Inventory { /* Arc<Mutex<Ledger>> */ }            // Debug
  impl Inventory {
      pub fn new(fee_asset: String, fee_reserve: i128) -> Self;
      pub fn record_balances(&self, balances: BTreeMap<String, i128>, at: Instant);
      pub fn stale(&self, now: Instant, max_age: Duration) -> bool;
      pub fn available(&self) -> BTreeMap<String, i128>;
      pub fn reserved(&self) -> BTreeMap<String, i128>;
      pub fn reserve(&self, amounts: &BTreeMap<String, i128>) -> Result<Reservation, InventoryError>;
  }
  #[must_use] pub struct Reservation { /* amounts, Arc<Mutex<Ledger>>, settled */ } // Debug
  impl Reservation { pub fn amounts(&self) -> &BTreeMap<String, i128>; pub fn consume(self); pub fn release(self); }
  impl Drop for Reservation { /* an unsettled token is released, with a warning */ }
  pub enum Settlement { Live(Reservation), DryRun }             // Debug
  pub enum InventoryError { Insufficient { asset: String, needed: i128, available: i128 } }
  pub async fn read_balances(reader: &PoolReader<'_>, account: &str, assets: &BTreeSet<String>)
      -> Result<BTreeMap<String, i128>, ChainError>;
  ```

Spec §5: "A plan takes a must-use `Reservation` for the wallet amounts it will spend; the token is consumed or released by value exactly once, carries the manager it was issued by, and saturates rather than errors so the ledger only protects callers that honour it." Spec §8: settlement "happens on every non-panicking path, including early returns and task cancellation during shutdown, through a drop guard that releases an unsettled token and logs a warning."

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const XLM: &str = "CXLM";
    const USDC: &str = "CUSDC";

    fn inventory() -> Inventory {
        let inventory = Inventory::new(XLM.to_string(), 500_000_000);
        inventory.record_balances(
            BTreeMap::from([(XLM.to_string(), 2_000_000_000), (USDC.to_string(), 700)]),
            Instant::now(),
        );
        inventory
    }

    /// The fee reserve is held back from XLM and from nothing else, and
    /// nothing is ever less than zero available.
    #[test]
    fn the_fee_reserve_is_withheld_from_the_native_asset_only() {
        let inventory = inventory();
        assert_eq!(inventory.available()[XLM], 1_500_000_000);
        assert_eq!(inventory.available()[USDC], 700);
        let poor = Inventory::new(XLM.to_string(), 500_000_000);
        poor.record_balances(BTreeMap::from([(XLM.to_string(), 100)]), Instant::now());
        assert_eq!(poor.available()[XLM], 0);
    }

    /// A reservation is out of what later plans may spend until it is
    /// settled; consumed, it has been spent; released, it is back.
    #[test]
    fn a_reservation_holds_until_it_is_settled() {
        let inventory = inventory();
        let held = inventory.reserve(&BTreeMap::from([(USDC.to_string(), 300)])).unwrap();
        assert_eq!(inventory.available()[USDC], 400);
        held.release();
        assert_eq!(inventory.available()[USDC], 700);
        let spent = inventory.reserve(&BTreeMap::from([(USDC.to_string(), 300)])).unwrap();
        spent.consume();
        assert_eq!(inventory.available()[USDC], 400, "debited until the next read");
        assert!(inventory.reserved().values().all(|amount| *amount == 0));
    }

    /// A plan sized against a view that has since changed is refused, and
    /// the refusal names the asset.
    #[test]
    fn more_than_is_available_is_refused() {
        let inventory = inventory();
        let _first = inventory.reserve(&BTreeMap::from([(USDC.to_string(), 600)])).unwrap();
        let error = inventory
            .reserve(&BTreeMap::from([(USDC.to_string(), 200)]))
            .expect_err("only 100 is left");
        assert!(matches!(&error, InventoryError::Insufficient { asset, needed: 200, available: 100 } if asset == USDC));
    }

    /// The drop guard: a reservation nobody settled — an early return, a
    /// cancelled task — is released, not leaked.
    #[test]
    fn an_unsettled_reservation_is_released_when_dropped() {
        let inventory = inventory();
        {
            let _forgotten = inventory.reserve(&BTreeMap::from([(USDC.to_string(), 300)])).unwrap();
        }
        assert_eq!(inventory.available()[USDC], 700);
    }

    /// A reservation settles against the inventory that issued it, by
    /// construction.
    #[test]
    fn a_reservation_never_touches_another_inventory() {
        let first = inventory();
        let second = inventory();
        first.reserve(&BTreeMap::from([(USDC.to_string(), 300)])).unwrap().consume();
        assert_eq!(second.available()[USDC], 700);
    }

    /// A fresh read replaces the balances and keeps live reservations: a
    /// plan in flight still owns what it reserved.
    #[test]
    fn a_fresh_read_keeps_live_reservations() {
        let inventory = inventory();
        let held = inventory.reserve(&BTreeMap::from([(USDC.to_string(), 300)])).unwrap();
        inventory.record_balances(BTreeMap::from([(USDC.to_string(), 1_000)]), Instant::now());
        assert_eq!(inventory.available()[USDC], 700);
        held.release();
    }

    /// Saturation: consuming after a read that already shows the spend
    /// leaves zero, never a negative balance.
    #[test]
    fn settling_saturates_rather_than_going_negative() {
        let inventory = inventory();
        let held = inventory.reserve(&BTreeMap::from([(USDC.to_string(), 700)])).unwrap();
        inventory.record_balances(BTreeMap::from([(USDC.to_string(), 0)]), Instant::now());
        held.consume();
        assert_eq!(inventory.available()[USDC], 0);
    }

    /// Never read is stale; read now is not; read long enough ago is.
    #[test]
    fn balances_go_stale() {
        let never = Inventory::new(XLM.to_string(), 0);
        assert!(never.stale(Instant::now(), Duration::from_secs(30)));
        let read = Instant::now();
        let fresh = Inventory::new(XLM.to_string(), 0);
        fresh.record_balances(BTreeMap::new(), read);
        assert!(!fresh.stale(read, Duration::from_secs(30)));
        assert!(fresh.stale(read + Duration::from_secs(31), Duration::from_secs(30)));
    }

    /// One `balance` simulation per asset, read into one map.
    #[tokio::test]
    async fn balances_are_read_per_asset() {
        // Script two simulateTransaction answers returning i128 balances,
        // as `chain::pool`'s `a_balance_is_simulated_on_the_token` does;
        // read_balances for {XLM, USDC}; assert the map and two calls.
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib inventory::`
Expected: compile errors — the module does not exist.

- [ ] **Step 3: Write it**

The module doc states spec §5 and §8's contract (quoted above this step), that the inventory holds wallet balances only (ruling 5), and that its arithmetic saturates by design: it is a ledger of claims, not money — the chain is the truth, and the next read corrects any drift.

```rust
struct Ledger {
    balances: BTreeMap<String, i128>,
    reserved: BTreeMap<String, i128>,
    read_at: Option<Instant>,
    fee_asset: String,
    fee_reserve: i128,
}

impl Ledger {
    fn available(&self, asset: &str) -> i128 {
        let balance = self.balances.get(asset).copied().unwrap_or(0);
        let reserved = self.reserved.get(asset).copied().unwrap_or(0);
        let withheld = if asset == self.fee_asset { self.fee_reserve } else { 0 };
        balance.saturating_sub(reserved).saturating_sub(withheld).max(0)
    }
}

/// Locks the ledger. A poisoned lock is recovered rather than propagated:
/// the ledger holds no invariant a panic mid-update could break that the
/// next read does not repair, and a filler that stops for good over one is
/// worse than one that re-reads its wallet.
fn lock(ledger: &Mutex<Ledger>) -> MutexGuard<'_, Ledger> {
    ledger.lock().unwrap_or_else(PoisonError::into_inner)
}
```

`Inventory` wraps `Arc<Mutex<Ledger>>` (`std::sync::Mutex`: never held across an `.await`). `reserve` checks every asset against `available` before adding any, so a refusal claims nothing; the returned `Reservation` clones the `Arc`. `consume` and `release` take `self`, set `settled`, and subtract from `reserved` (and, for `consume`, from `balances`) with `saturating_sub(..).max(0)`. `Drop` does `release`'s work when `settled` is false and logs `tracing::warn!(amounts = ?self.amounts, "a reservation was dropped unsettled; releasing it")`. `Settlement` is:

```rust
/// What travels with a plan to the executor: a live reservation, or the
/// statement that this is a dry run and nothing was reserved. The
/// executor refuses the one that does not match its mode, so the two
/// cannot be mixed up (spec §5).
#[derive(Debug)]
pub enum Settlement {
    /// A live plan's claim on the wallet.
    Live(Reservation),
    /// A dry run: nothing was reserved, and nothing may be spent.
    DryRun,
}
```

`read_balances` calls `reader.balance(asset, account)` for each asset in order and collects the amounts; any failure is returned, and the caller keeps its previous balances.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib inventory::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/inventory.rs src/liquidator.rs
git commit -m "feat(inventory): wallet balances, reservations and settlement"
```

---

## Task 7: the executor

**Files:**
- Create: `src/executor.rs`
- Modify: `src/chain/tx.rs` (`Submitter::source`; `TxOutcome::hash` and `TxOutcome::status`, moved from `auctioneer.rs`'s private `outcome_hash`/`outcome_status`)
- Modify: `src/auctioneer.rs` (call the moved methods)
- Modify: `src/liquidator.rs` (`pub mod executor;`)
- Test: inline in `src/executor.rs`

**Interfaces:**
- Consumes: Task 2's `FillRecord`, `record_fill`, `attach_fill_tx`; Task 4's `FillDraft`, `FillAction`; Task 5's `Submission { retries }`, `FILL_RETRIES`, `QueueError`; Task 6's `Settlement`, `Reservation`; `submit_op`, `Request`, `RequestType`, `Submitter::simulate_only`, `Judgment`.
- Produces:
  ```rust
  pub const WITHDRAW_ALL: i128 = 9_223_372_036_854_775_807; // i64::MAX
  pub struct FillPlan { pub pool: String, pub user: String, pub draft: FillDraft, pub priority: Priority } // Debug, Clone
  pub struct FillRecorded { pub fill_id: i64, pub simulated: bool, pub dry_run: bool,
                            pub submission: Option<TxOutcome> }                      // Debug
  impl FillRecorded { pub fn succeeded(&self) -> bool; }
  pub enum ExecOutcome { Recorded(FillRecorded), Replan { contract_error: u32 },
                         Refused { contract_error: Option<u32> }, Stale }            // Debug
  pub enum ExecutorError { Store(StoreError), Chain(ChainError), Xdr(XdrError), Queue(QueueError),
                           Mode(&'static str) }                                      // thiserror
  pub fn fill_requests(user: &str, draft: &FillDraft) -> Result<Vec<Request>, XdrError>;
  impl<'a> Executor<'a> {
      pub fn new(store: &'a Store, submitter: Option<Submitter<'a>>, dry_run: bool) -> Self;
      pub fn filler(&self) -> Option<&str>;
      pub async fn execute(&self, plan: &FillPlan, settlement: Settlement,
                           queue: Option<&SubmissionQueue>) -> Result<ExecOutcome, ExecutorError>;
  }
  // src/chain/tx.rs
  impl Submitter<'_> { pub fn source(&self) -> &str; }   // the signer's G… address
  impl TxOutcome { pub fn hash(&self) -> TxHash; pub fn status(&self) -> &'static str; }
  ```

Spec §5's executor, in order — the doc comment on `execute` carries it:

1. **The mode guards**, before anything touches the chain. A dry-run executor handed `Settlement::Live` releases the reservation and fails; a live one handed `Settlement::DryRun` fails; a queue offered to an executor that is dry-run or has no signer fails — Phase 4's CodeRabbit finding, for the same reason: the public pieces composed by hand must not be able to make a dry run that sends.
2. **Simulate the exact `submit`** — `from`, `spender` and `to` all the filler's address, the requests `fill_requests` builds — through `Submitter::simulate_only`, when there is a signer to simulate as. `InvalidHf` (1205) or `MinCollateralNotMet` (1224) answers `Replan`; any other refusal answers `Refused` with the code on a warn line; nothing is recorded for either, and the reservation is released. A footprint that needs restoring answers `Refused` in dry-run (only an armed submission restores) and proceeds when armed (`prepare` restores it). With no signer there is nothing to simulate as: the plan is recorded unsimulated (ruling 4).
3. **Record before sending.** `record_fill` with `dry_run` the configured mode, and one structured `tracing::info!` event carrying every column plus `simulated` and `armed` (spec §7).
4. **Submit** on the queue, when one is given, with `retries: FILL_RETRIES` and the plan's priority. Attach the hash to the row for every `TxOutcome`. `Succeeded` and `Unknown` consume the reservation (ruling 14); `Failed` and `Expired` release it. `QueueError::Chain(BadSequence)` answers `Stale`; a `Simulation` refusal at `prepare` answers `Replan` or `Refused` as in step 2; everything answering early releases the reservation.
5. **Dry-run stops after step 3**: no reservation was taken, nothing is sent.

- [ ] **Step 1: Write the failing tests**

Each builds a `Store` (`#[sqlx::test]`), a `ScriptedRpc`, a test signer, and a `FillPlan` whose draft is a literal — this task does not need `plan_fill`. Use the fixture's own addresses so every request encodes: `harness::POOL`, `harness::USER_ONE` as the liquidated user, the fixture's XLM (`CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA`) and USDC (`CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75`) reserves.

```rust
/// The fill comes first, then the plan's actions in order; "withdraw
/// all" is `WITHDRAW_ALL`.
#[test]
fn fill_requests_put_the_fill_first() {
    // A draft with actions [Repay{USDC, 10}, WithdrawAll{XLM}, SupplyCollateral{XLM, 7}]
    // and percent 80. Assert the four requests: FillUserLiquidationAuction
    // (address USER_ONE, amount 80), Repay (USDC, 10), WithdrawCollateral
    // (XLM, WITHDRAW_ALL), SupplyCollateral (XLM, 7).
}

/// Dry-run simulates through `simulate_only` and records, and never
/// reaches the signing path: no fee stats, no send.
#[sqlx::test(migrations = "./migrations")]
async fn a_dry_run_records_the_fill_and_sends_nothing(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Dry-run executor with a Submitter. script_simulate_prelude + script_simulate_accepted.
    // execute(plan, Settlement::DryRun, None) → Recorded { simulated: true, dry_run: true, submission: None }.
    // The fills row: dry_run true, tx_hash NULL, every value the plan carried.
    // rpc.calls("sendTransaction") and rpc.calls("getFeeStats") both empty.
}

/// With no key there is no account to simulate as; the plan is still
/// recorded, and says it was not simulated.
#[sqlx::test(migrations = "./migrations")]
async fn a_keyless_dry_run_records_the_plan_unsimulated(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Executor::new(&store, None, true). Recorded { simulated: false }. No RPC call at all.
}

/// Armed: the fill is recorded before it is sent, sent on the queue with
/// the plan's priority and the fill budget, its hash attached, and its
/// reservation consumed.
#[sqlx::test(migrations = "./migrations")]
async fn a_live_fill_is_sent_recorded_and_its_reservation_consumed(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Live executor; an Inventory holding the plan's spend; reserve it.
    // Script: simulate prelude + accepted (the executor's judgment), then
    // prepare prelude + accepted + script_send("PENDING") +
    // script_transaction_success (the queue's own path). Run
    // run_queue_with alongside, joined as auctioneer.rs's armed test does.
    // Assert Recorded with Succeeded; the row's tx_hash is the envelope's;
    // inventory.reserved() is empty and available() is down by the spend.
}

/// A fill the chain failed frees what it reserved; the attempt stays on
/// the audit, hash and all.
#[sqlx::test(migrations = "./migrations")]
async fn a_fill_the_chain_failed_releases_its_reservation(db: sqlx::PgPool) -> sqlx::Result<()> {
    // A stand-in queue worker (a task draining the receiver, as
    // service.rs's tests do) answers Ok(TxOutcome::Failed { .. }).
    // Assert Recorded with Failed, the row's hash attached, and the
    // inventory's available() back where it started.
}

/// The contract's health check disagreed with the plan's projection: one
/// re-plan, nothing recorded, the reservation released.
#[sqlx::test(migrations = "./migrations")]
async fn a_health_refusal_asks_for_a_replan(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Once with script_simulate_refused(1205), once with 1224: each is
    // Replan { contract_error } with that code, no fills row, reservation released.
}

/// Any other refusal skips this auction until the next tick, code on the log.
#[sqlx::test(migrations = "./migrations")]
async fn another_refusal_skips_without_recording(db: sqlx::PgPool) -> sqlx::Result<()> {
    // script_simulate_refused(1212). Refused { contract_error: Some(1212) }, no row.
}

/// A bad sequence means someone else spent this key's sequence first: the
/// plan is stale. The attempt was recorded before the send, so its row
/// stays — with no hash, an armed attempt that was never named.
#[sqlx::test(migrations = "./migrations")]
async fn a_bad_sequence_is_stale(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Stand-in worker answers Err(QueueError::Chain(ChainError::BadSequence)).
    // Stale; reservation released; one fills row, tx_hash NULL, dry_run false.
}

/// Spec §5: the two settlements cannot be mixed up.
#[sqlx::test(migrations = "./migrations")]
async fn the_settlement_must_match_the_mode(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Dry-run executor handed Settlement::Live(reservation): Err(Mode), the
    // reservation released (available() restored), no RPC call.
    // Live executor handed Settlement::DryRun: Err(Mode), no RPC call.
}

/// A queue offered to a dry-run executor is refused before anything is
/// simulated, recorded or enqueued.
#[sqlx::test(migrations = "./migrations")]
async fn a_queue_offered_to_a_dry_run_executor_is_refused(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Err(Mode); no simulateTransaction; the receiver's try_recv is Empty; no row.
}

/// The priority and the retry budget reach the queue as the plan set them.
#[sqlx::test(migrations = "./migrations")]
async fn a_fill_carries_its_priority_and_the_fill_budget(db: sqlx::PgPool) -> sqlx::Result<()> {
    // A stand-in worker records queued.submission.priority and .retries,
    // then answers Succeeded. Plan with Priority::High → High and FILL_RETRIES.
}

/// An archived footprint cannot be judged in dry-run: only an armed
/// submission restores.
#[sqlx::test(migrations = "./migrations")]
async fn a_dry_run_that_needs_a_restore_is_refused(db: sqlx::PgPool) -> sqlx::Result<()> {
    // script_simulate_needs_restore. Refused { contract_error: None }, no row, no send.
}
```

Write every body out.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib executor::`
Expected: compile errors — the module does not exist.

- [ ] **Step 3: Write it**

```rust
/// `WithdrawCollateral`'s "all": the contract caps what it burns at the
/// position (`to_burn = min(to_b_token_up(amount), balance)`), and
/// `to_b_token_up(i64::MAX)` is `9.22e18 × 1e12 / b_rate`, far inside
/// `i128` — so this withdraws everything and cannot overflow the
/// contract's own arithmetic.
pub const WITHDRAW_ALL: i128 = 9_223_372_036_854_775_807;

/// The pool's `InvalidHf`: the post-submit health check failed.
const INVALID_HF: u32 = 1_205;
/// The pool's `MinCollateralNotMet`.
const MIN_COLLATERAL_NOT_MET: u32 = 1_224;

/// A planned fill, ready to execute.
#[derive(Debug, Clone)]
pub struct FillPlan {
    /// The pool contract.
    pub pool: String,
    /// The liquidated account.
    pub user: String,
    /// What `plan_fill` drafted.
    pub draft: FillDraft,
    /// The fee tier: high when the estimated profit reaches
    /// `HIGH_FEE_PROFIT_THRESHOLD`.
    pub priority: Priority,
}
```

`fill_requests` maps `Repay` → `Request { request_type: RequestType::Repay, address: asset, amount }`, `WithdrawAll` → `WithdrawCollateral` with `WITHDRAW_ALL`, `SupplyCollateral` → `SupplyCollateral`, after `Request::fill(RequestType::FillUserLiquidationAuction, user, draft.percent)?`.

`Executor::execute` is steps 1–5 above, in that order, with a doc comment that states them. Build the operation once, before simulating, and reuse it for the submission. Release the reservation explicitly on every early answer — the drop guard is the net, and it warns, so a path that relies on it is a path that logs a warning on every ordinary refusal. `FillRecord` is built from the plan: `account` is `plan.user`, `auction_type` `UserLiquidation`, `bid`/`lot` `draft.to_fill`'s, the three values `draft`'s.

`Submitter::source` returns `self.signer.address()`. Move `outcome_hash` and `outcome_status` from `auctioneer.rs` onto `TxOutcome` as `hash()` and `status()`, unchanged, and have the auctioneer call them.

- [ ] **Step 4: Run the tests and the suite**

Run: `cargo test --lib executor:: auctioneer::` then `make check`.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/executor.rs src/chain/tx.rs src/auctioneer.rs src/liquidator.rs
git commit -m "feat(executor): simulate, record, submit and settle one fill"
```

---

## Task 8: the filler's evaluation loop

**Files:**
- Create: `src/filler.rs`
- Modify: `src/chain/pool.rs` (`PoolSnapshot::valued_at`, `PoolSnapshot::accrued_reserves`, moved from `auctioneer.rs`)
- Modify: `src/auctioneer.rs` (`decide` calls the moved methods; its private `valued_at`/`accrue_reserves` go)
- Modify: `src/harness.rs` (`script_auction_entry`, `script_no_auction`)
- Modify: `src/liquidator.rs` (`pub mod filler;`, `LiquidatorError::Filler`)
- Test: inline in `src/filler.rs`

**Interfaces:**
- Consumes: Tasks 1–7.
- Produces:
  ```rust
  // src/chain/pool.rs
  impl PoolSnapshot {
      pub fn valued_at(&self, close_time: u64) -> u64;
      pub fn accrued_reserves(&self, at: u64) -> Result<BTreeMap<u32, Reserve>, MathError>;
  }
  // src/filler.rs
  pub struct FillerConfig { pub dry_run: bool, pub own_addresses: BTreeSet<String>,
      pub hf_safety_multiplier: i128, pub plan_iterations: u32, pub replan_ledgers: u32,
      pub replan_near_ledgers: u32, pub high_fee_profit_threshold: i128,
      pub inventory_refresh: Duration, pub native_asset: String }       // Debug, Clone
  #[derive(Debug, Default)] pub struct FillerState { /* last_planned, recorded_dry_run, inventory_stale */ }
  pub struct TickSummary { pub planned: u32, pub executed: u32, pub skipped: u32, pub closed: u32 } // Debug, Default, PartialEq
  pub enum FillerError { Store(StoreError) }                               // thiserror; the fatal kind only
  impl<'a> Filler<'a> {
      pub fn new(rpc: &'a RpcClient, store: &'a Store, pools: &'a [PoolConfig], config: FillerConfig,
                 executor: Executor<'a>, inventory: Inventory) -> Self;
      pub async fn tick(&self, state: &mut FillerState, tick: LedgerTick, execute: bool,
                        queue: Option<&SubmissionQueue>, shutdown: &watch::Receiver<bool>)
          -> Result<TickSummary, FillerError>;
  }
  // src/harness.rs, cfg(test)
  pub(crate) fn script_auction_entry(rpc: &ScriptedRpc, user: &str, auction: &AuctionData, ledger: u32);
  pub(crate) fn script_no_auction(rpc: &ScriptedRpc, ledger: u32);
  ```

One tick, per configured pool, in this order (the doc comment on `tick` carries it):

1. `open_auctions(pool)`. Keep a row only when it is a user liquidation, its account is not one of `own_addresses` (spec §1), the pool `supports` every asset it names, it is not an auction this process already recorded a dry-run fill for (ruling 8), and it is **due**: no `fill_ledger` yet, never planned by this process, within `replan_near_ledgers` of its `fill_ledger`, or `replan_ledgers` since it was last planned (spec §5; ruling 7). A row that is not kept costs no chain read at all.
2. Re-read each kept row's auction entry (spec §5: "Planning always re-reads the on-chain auction entry first"). A missing entry deletes the row. The chain's entry, not the row, is what is planned against.
3. If any entry is live, read **one** snapshot for the pool — of the filler's account when there is one — and value its reserves at `snapshot.valued_at(tick.close_time)`. Refresh the inventory from it when the balances are stale or a confirmed fill has flagged them: every reserve asset of the pool plus the native asset, through `read_balances`; a failed read keeps the old balances and warns.
4. For each live entry: `plan_fill` with `FillTerms` from the pool config and the snapshot (`supply_allowed` is status 3 or below and the primary reserve enabled; `health_floor(pool.min_health_factor, hf_safety_multiplier)`; `pool.profit_bps(bid, lot)`), and `FillInputs` with `earliest_ledger = tick.sequence + 1`, the filler's positions from the snapshot (empty without a key), the inventory's `available()` (empty without a key), and `max_percent` 100. A skip clears the row's plan and logs the reason at debug. A draft is written onto the row (`set_fill_plan`) and logged at info.
5. When `execute` is true and the draft's `fill_ledger ≤ tick.sequence + 1`, execute it: the priority is `High` when `est_profit ≥ to_oracle_units(high_fee_profit_threshold, prices.scalar())`; the settlement is `DryRun`, or `Live(inventory.reserve(&draft.spend))` — a refused reservation skips the auction this tick with a warning. `Replan` plans once more with `max_percent = max(1, percent / 2)` and executes that if it drafts (ruling 12) — a second `Replan` is a skip. `Stale` clears the row's plan and forgets when it was planned, so the next tick plans from fresh state. A dry run's `Recorded` marks the auction recorded; a `Succeeded` or `Unknown` submission flags the inventory stale.
6. Between auctions, a set shutdown flag ends the tick. Every error but `StoreError` is one auction's: logged with the pool and account, and the pass carries on. `StoreError` is fatal, as it is for the tracker and the auctioneer.

- [ ] **Step 1: Move the valuation clamp**

Move `valued_at` and `accrue_reserves` from `src/auctioneer.rs` into `impl PoolSnapshot` in `src/chain/pool.rs` as `valued_at(&self, close_time: u64) -> u64` and `accrued_reserves(&self, at: u64) -> Result<BTreeMap<u32, Reserve>, MathError>`, with their doc comments (reworded from "the auctioneer" to "whoever values positions alongside this snapshot — the auctioneer and the filler"), and make `Auctioneer::decide` call them. Run `cargo test --lib auctioneer:: tracker::` — every test passes unchanged. Add to `src/harness.rs` the two scripting helpers named above, built as `tracker.rs`'s tests build an auction entry (temporary durability, `keys::auction(POOL, user, AuctionType::UserLiquidation)`).

- [ ] **Step 2: Write the failing tests**

Each seeds `auctions` rows directly with `Store::upsert_auction`, scripts the chain, and calls `Filler::tick`. The pool config is `harness::POOL` with `primary_asset` the fixture's XLM, `min_health_factor` 1.5, `default_profit_bps` 1000, and both supported lists `["*"]` unless a test says otherwise. Choose each auction's amounts in the fixture's own XLM and USDC so the lot is worth several times the bid; with a start of `tick.sequence − 300` the margin's ledger has passed and the fill is due now, and with a start of `tick.sequence` it is 100-odd ledgers away.

```rust
/// Planned, but its ledger has not come: the plan goes onto the row and
/// nothing is executed.
#[sqlx::test(migrations = "./migrations")]
async fn a_fill_whose_ledger_has_not_come_is_planned_and_left(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Start = tick.sequence. After tick: the row's fill_ledger is Some(l)
    // with l > tick.sequence + 1 and percent is Some; no fills row; no
    // simulateTransaction for the fill.
}

/// Its ledger has come: executed, and — a keyless dry run — recorded
/// unsimulated.
#[sqlx::test(migrations = "./migrations")]
async fn a_fill_whose_ledger_has_come_is_recorded(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Start = tick.sequence − 300. One fills row, dry_run true, tx_hash NULL,
    // percent and fill_ledger matching the row's plan. TickSummary { planned: 1, executed: 1, .. }.
}

/// Ruling 8: a dry run records an auction once, not on every tick until
/// someone else fills it — and makes no chain read for it afterwards.
#[sqlx::test(migrations = "./migrations")]
async fn a_dry_run_fill_is_recorded_once(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Two ticks, the second one ledger later, with chain reads scripted
    // for the first only (the scripted server fails loudly on an
    // unscripted call). Still one fills row.
}

/// Spec §5: an entry the chain no longer holds closes the row, and no
/// snapshot is read for it.
#[sqlx::test(migrations = "./migrations")]
async fn an_auction_the_chain_no_longer_holds_is_closed(db: sqlx::PgPool) -> sqlx::Result<()> {
    // script_no_auction. The row is gone; TickSummary { closed: 1, .. };
    // no snapshot was read (only the one entry read reached the RPC).
}

/// Unsupported assets and the bot's own account cost no chain read.
#[sqlx::test(migrations = "./migrations")]
async fn an_auction_the_filler_does_not_take_costs_no_chain_read(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Two rows: one whose bid asset is outside supported_bid, one whose
    // account is in own_addresses. Nothing scripted; the tick answers Ok
    // and rpc.received() is empty.
}

/// The replan cadence: planned once, then not again until the period
/// passes — except near the fill ledger, where every ledger re-plans.
#[sqlx::test(migrations = "./migrations")]
async fn a_planned_auction_waits_for_its_replan_period(db: sqlx::PgPool) -> sqlx::Result<()> {
    // replan_ledgers 10, replan_near_ledgers 5, a fill ledger ~110 away.
    // Tick at s plans (scripted). Tick at s+1: no chain read. Tick at
    // s+10: plans again (scripted). Tick at fill_ledger−5: plans again.
}

/// Ruling 9: before the startup delay has passed, plans are made and
/// nothing is executed.
#[sqlx::test(migrations = "./migrations")]
async fn nothing_is_executed_before_the_startup_delay(db: sqlx::PgPool) -> sqlx::Result<()> {
    // A fill due now, tick(.., execute: false, ..). The row has its plan; no fills row.
}

/// The contract's health check refused the plan: planned once more at half
/// the percent, and that is what is recorded.
#[sqlx::test(migrations = "./migrations")]
async fn a_health_refusal_is_planned_again_at_half_the_percent(db: sqlx::PgPool) -> sqlx::Result<()> {
    // A dry run with a key: an Executor over a Submitter, the inventory's
    // balance reads scripted as zero, and a lot worth enough that the fill
    // alone holds the floor at any percent, so the first plan is 100%.
    // Simulations: refused 1205, then accepted. One fills row, percent 50.
}

/// Spec §10's overlap case, which this suite must cover: the loser of a
/// sequence race re-plans against the remainder the winner's partial fill
/// left, never resends its own plan.
#[sqlx::test(migrations = "./migrations")]
async fn a_stale_fill_is_planned_again_against_the_remainder(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Armed, with a stand-in queue worker that answers BadSequence to the
    // first submission and Succeeded to the second. Tick 1: executes,
    // Stale → the row's plan is cleared. Between ticks, upsert the row as
    // the tracker would after the winner's 40% fill (the remainder,
    // fill_ledger None). Tick 2: the entry read is scripted as that
    // remainder; the second fills row's bid and lot are the remainder's
    // scaled amounts, not the original's.
}

/// One auction's chain failure is that auction's; the next is still planned.
#[sqlx::test(migrations = "./migrations")]
async fn one_failing_auction_does_not_stop_the_pass(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Two due rows. The first entry read answers HTTP 500; the second is
    // live. The second's plan is on its row; the tick answers Ok.
}

/// Several due auctions in one pool share one snapshot.
#[sqlx::test(migrations = "./migrations")]
async fn a_pools_auctions_share_one_snapshot(db: sqlx::PgPool) -> sqlx::Result<()> {
    // Two due rows, scripted with one snapshot between them; both planned.
    // (An unscripted second snapshot would fail the test loudly.)
}
```

Write every body out.

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test --lib filler::`
Expected: compile errors — the module does not exist.

- [ ] **Step 4: Write it**

The module doc states steps 1–6, rulings 4, 7, 8, 9 and 12, and that the filler never writes a row's `bid`, `lot` or `start_ledger` — the tracker owns them, from the pool's own events. The due test is its own function, documented, because it is where a quiet mistake would re-plan nothing or everything:

```rust
/// Whether `row` is planned this tick (spec §5): it has no plan yet, this
/// process has not planned it, it is within `replan_near_ledgers` of its
/// fill ledger, or `replan_ledgers` have passed since it was planned. The
/// subtractions saturate on purpose — a fill ledger already passed is
/// "within reach", and a planned ledger ahead of this tick (a watch that
/// coalesced) is "just planned" — both being the safe reading.
fn due(row: &TrackedAuction, planned_at: Option<u32>, tick: LedgerTick, config: &FillerConfig) -> bool {
    let (Some(fill_ledger), Some(planned_at)) = (row.fill_ledger, planned_at) else {
        return true;
    };
    fill_ledger.saturating_sub(tick.sequence) <= config.replan_near_ledgers
        || tick.sequence.saturating_sub(planned_at) >= config.replan_ledgers
}
```

Everything else follows the six steps. The filler's own address is `executor.filler()`. `FillerError` holds only `Store`; per-auction failures are logged inside `tick`, never returned. Add `LiquidatorError::Filler(#[from] filler::FillerError)` in `src/liquidator.rs`.

- [ ] **Step 5: Run the tests and the suite**

Run: `cargo test --lib filler:: auctioneer::` then `make check`.
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/filler.rs src/chain/pool.rs src/auctioneer.rs src/harness.rs src/liquidator.rs
git commit -m "feat(filler): plan every supported auction and execute the ones whose ledger has come"
```

---

## Task 9: adopting an auction the events never showed

**Files:**
- Modify: `src/auctioneer.rs`
- Test: inline in `src/auctioneer.rs`

**Interfaces:**
- Consumes: `PoolReader::auction`, `Store::upsert_auction`, Task 8's `harness::script_auction_entry`.
- Produces: no new public surface. `Auctioneer::act` still answers `ActOutcome::Refused` for `AuctionInProgress`; it now also writes the auction's row.

Ruling 15. An auction opened before the bot's events cursor produced no `NewAuction` the tracker applied, so there is no row, the auctioneer decides to liquidate, the contract answers `AuctionInProgress` (1212) — and nothing ever tells the filler the auction exists.

- [ ] **Step 1: Write the failing tests**

```rust
/// An auction already open on chain is adopted: the contract's refusal
/// names it, the auctioneer reads it, and the row the filler walks exists.
#[sqlx::test(migrations = "./migrations")]
async fn an_auction_already_open_on_chain_is_adopted(db: sqlx::PgPool) -> sqlx::Result<()> {
    // A liquidatable borrower with no auctions row; act's simulation is
    // script_simulate_refused(1212); then script_auction_entry for that
    // borrower. act answers Refused, and store.auction(..) holds the
    // entry's bid, lot and block (as start_ledger), with no fill plan.
}

/// The next pass sees it and skips, which is what clears the flag.
#[sqlx::test(migrations = "./migrations")]
async fn an_adopted_auction_is_skipped_on_the_next_pass(db: sqlx::PgPool) -> sqlx::Result<()> {
    // After the adoption above, decide answers Skip(SkipReason::AuctionOpen).
}

/// A read that fails is a warning, never a failed pass.
#[sqlx::test(migrations = "./migrations")]
async fn a_failed_adoption_is_only_a_warning(db: sqlx::PgPool) -> sqlx::Result<()> {
    // The entry read answers HTTP 500. act answers Refused, not Err; no row.
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib auctioneer::`
Expected: the first two fail — no row is written.

- [ ] **Step 3: Write it**

A private `const AUCTION_IN_PROGRESS: u32 = 1_212;` beside the two constants the percent walk uses. Where the walk meets a refusal it does not adjust for, a refusal carrying `AUCTION_IN_PROGRESS` first calls:

```rust
/// Writes the row for an auction the chain holds and the store does not —
/// one opened before this bot's events cursor, which no `NewAuction` ever
/// reached the tracker for. Without it the filler, which walks the store,
/// would never see the auction at all. The chain's entry is the row's
/// truth: bid, lot, and its block as the start ledger; no fill plan.
///
/// A failed read is a warning: the borrower stays flagged and the next
/// pass tries again. A failed write is the store's, and fatal as always.
async fn adopt(&self, pool: &str, account: &str) -> Result<(), AuctioneerError>
```

and then answers `Refused` exactly as before.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib auctioneer::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/auctioneer.rs
git commit -m "feat(auctioneer): adopt an auction the chain holds and the store does not"
```

---

## Task 10: wiring the filler into the service

**Files:**
- Modify: `src/service.rs` (`SigningContext` keeps both roles; `spawn_queues` replaces `spawn_submission_queue`; `StartupGate` shared by both loops; `spawn_filler` and `filler_loop`; `validate_filler` in `run` and `check_config`; the module doc)
- Modify: `src/main.rs` only if a signature it calls changed
- Test: inline in `src/service.rs`

**Interfaces:**
- Consumes: everything above.
- Produces (all private to `service.rs`):
  ```rust
  struct SigningContext { network: Network, tx_config: TxConfig, signers: Signers,
                          own_addresses: BTreeSet<String>, native_asset: String }
  struct Queues { auctioneer: Option<SubmissionQueue>, filler: Option<SubmissionQueue> }
  fn spawn_queues(tasks: &mut JoinSet<Result<(), LiquidatorError>>, rpc: &RpcClient,
                  signing: &SigningContext, dry_run: bool, shutdown: &watch::Receiver<bool>) -> Queues;
  struct StartupGate { first_tick_ledger: Option<u32>, unlocked: bool }   // Debug, Default
  impl StartupGate { fn observe(&mut self, tick: LedgerTick, delay_ledgers: u32) -> bool; }
  async fn validate_filler(rpc: &RpcClient, config: &ServiceConfig, signing: &SigningContext)
      -> Result<Vec<String>, LiquidatorError>;
  ```

What the service does, and the doc comments must say:

- **Queues** (ruling 2). Dry-run: none at all. Armed: one worker for the filler's key; the auctioneer's queue is a clone of the filler's when `signers.shared()`, and its own worker when the auctioneer has a key of its own. Two workers never serve one key. `spawn_submission_queue`'s warning for "armed with no key" stays, for the auctioneer, as defence: Task 1's key rule makes it unreachable from `main`.
- **The startup gate** moves out of `AuctioneerState` (`first_tick_ledger`, `submissions_unlocked`) into `StartupGate`, unchanged in behaviour and docs, and each loop holds its own: the filler's `execute` flag is `gate.observe(tick, startup_delay_ledgers)` (ruling 9). The auctioneer's tests for the delay pass unchanged.
- **The filler task** is spawned beside the auctioneer, off `tick_rx.clone()` — the same watch the tracker publishes after it acknowledges, never the poller channel (CLAUDE.md: the auctioneer "is a separate task, and must stay one"; the filler is one for the same reason). It builds an `Executor` over a `Submitter` for `signers.filler` when there is one, an `Inventory` over the native asset and `XLM_FEE_RESERVE`, and a `Filler`, then runs `filler.tick` on each change, returning on shutdown or when the watch's sender drops. A `FillerError::Store` ends it with an error, which `drain_tasks` turns into a shutdown like any other.
- **Filler validation** (spec §6: "The filler account exists and holds XLM above the fee reserve. In live mode the filler holds at least `min_primary_collateral` in every pool or a warning names the shortfall.") With no filler key: a warning that the filler plans against an empty inventory and simulates nothing (reachable only in dry-run). With one: the account must exist (`RpcClient::account`; `NoAccount`) and its native balance must reach `XLM_FEE_RESERVE` — each a `LiquidatorError::Config` when armed and a warning in dry-run. Armed, each pool's filler collateral in its primary asset (one snapshot of the filler's account per pool, b-tokens through the reserve's rate) below `min_primary_collateral` is a warning naming the pool and the shortfall. `run` and `check_config` both call it after `validate`, and `log_validation` prints its warnings with the rest.

- [ ] **Step 1: Write the failing tests**

```rust
/// Ruling 2: one key, one queue. The fallback auctioneer shares the
/// filler's; two keys are two workers; a dry run starts none.
#[tokio::test]
async fn one_key_is_one_queue() {
    // Three SigningContexts (shared, distinct, none) and spawn_queues
    // with dry_run false, false, true. Assert tasks.len() is 1, 2, 0, and
    // which of Queues' two fields are Some in each.
}

/// Spec §6: an armed filler whose account does not exist cannot start.
#[tokio::test]
async fn an_armed_filler_with_no_account_is_refused() {
    // The account read answers no entry. validate_filler armed → Err(Config)
    // naming the account; in dry-run → Ok with one warning.
}

/// Spec §6: nor can one without its fee reserve.
#[tokio::test]
async fn an_armed_filler_short_of_its_fee_reserve_is_refused() {
    // Account present; the native balance simulation answers less than
    // XLM_FEE_RESERVE. Armed → Err(Config) naming XLM_FEE_RESERVE; dry-run → a warning.
}

/// Spec §6: short of a pool's primary floor is a warning, not a refusal.
#[tokio::test]
async fn an_armed_filler_under_its_primary_floor_is_warned() {
    // Account present, fee reserve met, the pool snapshot showing the
    // filler with less primary collateral than min_primary_collateral.
    // Ok, with a warning naming the pool and the shortfall.
}

/// The filler runs off the tracker's published tick, like the auctioneer:
/// a due auction in the store becomes a fills row.
#[sqlx::test(migrations = "./migrations")]
async fn the_filler_runs_off_the_published_tick(db: sqlx::PgPool) -> sqlx::Result<()> {
    // A keyless dry-run filler loop over a watch channel; one due auction
    // row; the entry read and snapshot scripted. Publish one tick; wait
    // for the fills row (bounded poll); raise shutdown; the loop returns Ok.
}

/// Ruling 9 through the loop: inside the startup delay the plan lands on
/// the row and nothing is recorded.
#[sqlx::test(migrations = "./migrations")]
async fn the_filler_waits_out_the_startup_delay(db: sqlx::PgPool) -> sqlx::Result<()> {
    // startup_delay_ledgers 5; one tick; the row has its plan; no fills row.
}
```

Write every body out.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib service::`
Expected: compile errors — `spawn_queues`, `StartupGate` and `validate_filler` do not exist.

- [ ] **Step 3: Write it**

In `Service::run`, after the pollers: build `SigningContext` (computing `own_addresses` before `into_signers` consumes the keys, and `native_asset` from `network.native_asset_contract()?`), `validate_filler`, `spawn_queues`, then `spawn_auctioneer` with `queues.auctioneer` and `spawn_filler` with `queues.filler` and `tick_rx.clone()`, all before the tracker loop. `FillerConfig` comes from `ServiceConfig`'s Task 1 fields, `dry_run`, `plan_iterations`, `own_addresses` and `native_asset`. The module doc's task list becomes five kinds of task — pollers, the tracker, the auctioneer, the filler, and a submission-queue worker per distinct key when armed — and says why the filler is its own task.

- [ ] **Step 4: Run the tests and the suite**

Run: `cargo test --lib service::` then `make check`.
Expected: PASS, the auctioneer's service tests unchanged but for construction sites the compiler names.

- [ ] **Step 5: Commit**

```bash
git add src/service.rs src/main.rs
git commit -m "feat(service): run the filler, one queue per key, and validate its account"
```

---

## Task 11: documentation, and the dry-run against mainnet

**Files:**
- Modify: `CLAUDE.md`, `README.md`, `CHANGELOG.md`, `.env.example`, `pools.example.toml`
- Create (scratchpad, not committed): the demonstration's log excerpt, for the pull request body

**Interfaces:** none.

- [ ] **Step 1: CLAUDE.md**

- **Status** becomes Phase 5: the filler (`filler`, `executor`, `inventory`, `math::fill`) plans a fill for every open liquidation auction it supports, holds its own position at `min_health_factor × HF_SAFETY_MULTIPLIER`, records every fill it executes — dry-run or not — and, only with `DRY_RUN=false` and `FILLER_SECRET_KEY`, submits it on the filler key's queue. **Nothing unwinds a fill yet:** a live fill leaves the taken position in the pool until Phase 6. Keep the paragraph's existing shape.
- **Module map**: entries for `src/math/fill.rs`, `src/inventory.rs`, `src/executor.rs`, `src/filler.rs`, each one paragraph in the existing style; `src/queue.rs` gains the terminal-outcome rule and the retry budget (ruling 6) and "one queue per distinct key — the auctioneer shares the filler's when it falls back to it"; `src/config.rs` names the six knobs and the two key rules; `src/service.rs` has five kinds of task and says the filler is a separate task for the auctioneer's reason; `migrations/` names `0003`; `src/chain/pool.rs` names `valued_at`/`accrued_reserves` as the one clamp both tasks share.
- **Safety invariants**: add one — "**Nothing is sent for a key while an earlier transaction's outcome on it is unknown.**" — with ruling 6's reasoning in two or three sentences.
- **Gotchas**: the filler never writes an `auctions` row's `bid`, `lot` or `start_ledger`, the tracker owns them; an auction past its 400th ledger is filled only under `force_fill`, which also caps every delay at 350; `WITHDRAW_ALL` is `i64::MAX` and why that is safe; a dry-run fill is recorded once per auction per process; live without `FILLER_SECRET_KEY`, or with the same key twice, is refused at startup.

- [ ] **Step 2: README, CHANGELOG, `.env.example`, `pools.example.toml`**

- `README.md`: the status and "what it does" sections gain the filler; the live-trading caution gains "Phase 5 does not unwind: a live fill's position stays in the pool".
- `CHANGELOG.md`: a Phase 5 entry under Unreleased in the existing style — the filler, the executor, the inventory, the queue's terminal-outcome rule and retry budgets, the `fills` table (migration `0003`), adoption of auctions opened before the cursor, the six knobs, the two key rules, the pool floor's lower bound.
- `.env.example`: the six knobs, each with the operator-facing reason its bad values are refused, beside the auctioneer's; `FILLER_SECRET_KEY`'s comment says it is required when `DRY_RUN=false` and must differ from `AUCTIONEER_SECRET_KEY`.
- `pools.example.toml`: `force_fill` is rewritten to the spec's meaning — fill no later than 350 ledgers into an auction, however little the lot then covers, and past its 400th ledger at all — replacing "Fill regardless of profit"; `min_health_factor` says it must be above `1.00001` and that the filler's floor is it times `HF_SAFETY_MULTIPLIER`.

Run `make check` (the invariants script reads some of these files).

- [ ] **Step 3: Commit the documentation**

```bash
git add CLAUDE.md README.md CHANGELOG.md .env.example pools.example.toml
git commit -m "docs(phase-5): document the filler, the executor and the inventory"
```

- [ ] **Step 4: The dry run against mainnet**

Run the bot with `DRY_RUN=true` against the pool `pools.example.toml` names, from a fresh database (`make db-reset && make db-up`), with the mainnet `RPC_URL` Phase 4's demonstration used, for at least ten minutes. **Use a `FILLER_SECRET_KEY` only if the user has put one in the environment; never generate, fund or ask for one here** — the keyless run is the demonstration. Capture:

- the startup lines: the resolved configuration (redacted), the filler validation's warnings, "no FILLER_SECRET_KEY: the filler plans against an empty inventory";
- every filler line: auctions walked, plans made (fill ledger, percent, estimated profit), skips with their reason, fills recorded;
- `SELECT * FROM fills` and `SELECT pool, account, start_ledger, fill_ledger, percent FROM auctions` at the end.

Write the excerpt to the scratchpad as `phase-5-live-demo.md`. If no liquidation auction was open on the pool during the window, say exactly that — the demonstration then shows the filler running and idle, and the scripted tests are the evidence for the fill path.

---

## Self-review

**Spec coverage** — each section 5 requirement, and where it lands:

| Spec §5 / §6 / §8 requirement | Task |
|---|---|
| Walk open auctions of configured pools whose assets are all supported (`*` wildcard) | 1 (`supports`), 8 |
| Re-plan with no fill ledger, every `REPLAN_LEDGERS`, every ledger within `REPLAN_NEAR_LEDGERS` | 1, 8 |
| Re-read the entry before planning; a missing entry closes the row | 8 |
| Competitors' fills reduce or close the stored auction | already the tracker's (Phase 3); 8's overlap test relies on it |
| Lot and bid valued raw and effective at oracle prices through accrued rates | 3 (`auction_positions`), 4 (`draft`), 8 (`accrued_reserves`) |
| Profit margin: first matching `profits` rule, else `default_profit_bps` | 1 (`profit_bps`) |
| Fill delay in closed form, verified at `d` and `d − 1`; capped at 350 under `force_fill`; moved to the next ledger when passed | 3, 4 |
| Repay held bid assets with a dust allowance, capped at the balance | 4 |
| Withdraw zero-collateral-factor lot assets | 4 |
| Project the post-fill health factor; require `min_health_factor × HF_SAFETY_MULTIPLIER` and `min_collateral` | 3 (`health_floor`), 4 |
| Short: supply the primary when the status permits; then lower the percent, never below 1; then delay past 200 | 4 |
| The fill request first; a `FillPlan` with pool, user, ledger, percent, requests, values, profit and wallet amounts | 4 (`FillDraft`), 7 (`FillPlan`, `fill_requests`) |
| Inventory: balances refreshed after confirmed transactions and at most every `INVENTORY_REFRESH_SECS` | 6, 8 |
| Must-use `Reservation`: consumed or released by value once, carries its manager, saturates | 6 |
| `Settlement::Live`/`DryRun`, each executor refusing the other | 6, 7 |
| Executor: simulate the exact `submit`; `InvalidHf`/`MinCollateralNotMet` re-plan once lower; other errors skip with the code logged | 7, 8 |
| High fee tier when `est_profit ≥ HIGH_FEE_PROFIT_THRESHOLD` | 1, 8 |
| Sign and send on the filler queue; success records, consumes and refreshes inventory | 5, 7, 8 |
| Send failures retry with backoff; a sequence error or timeout re-plans, never resends | 5, 7, 8 |
| Dry-run logs and records, takes no reservation, sends nothing | 7, 8 |
| Unwind; notify; metrics | **Phase 6** (decided with the user) |
| §6 knobs `HF_SAFETY_MULTIPLIER`, `REPLAN_LEDGERS`, `REPLAN_NEAR_LEDGERS`, `XLM_FEE_RESERVE`, `HIGH_FEE_PROFIT_THRESHOLD`, `INVENTORY_REFRESH_SECS` | 1 |
| §6 keys parse and differ; filler key required for live | 1 |
| §6 filler account exists and holds XLM above the fee reserve; live `min_primary_collateral` warning; `check-config` runs the same | 10 |
| §8 retry budgets: creations 3, fills 10 (unwinds 2 is Phase 6's) | 5 |
| §8 reservations held until the outcome is known — and, per ruling 14, an outcome still `Unknown` when shutdown interrupts its resolution *consumes* the reservation, never releases it: the wallet may have paid, and only the next balance read may say otherwise; a drop guard settles every non-panicking path | 5, 6, 7 |
| §1 never fill the bot's own auctions; past 400 only under `force_fill` | 4, 8 |
| §7 fills logged as a dedicated event with every column | 7 |
| §9 tests: queue retry budgets, reservation typestates, the settlement guards, the sequence-error re-plan and the overlap case | 5, 6, 7, 8 |
| §10 the overlap case where a partial fill leaves the auction open | 8 |

**Deviations from the spec, each a ruling:** the inventory keeps balances only (ruling 5); the delay escalation aims at the floor rather than a ratio of 1 (ruling 10); the auctioneer adopts pre-cursor auctions, which the spec does not mention but its discovery section leaves no other way to reach (ruling 15).

**Type consistency:** `FillPercent` is `crate::chain::xdr::encode::FillPercent` throughout; `FillDraft` (Task 4) is what `FillPlan` (Task 7) carries and `Filler` (Task 8) builds; `Settlement` (Task 6) is what `Executor::execute` (Task 7) takes; `Signers` (Task 1) is what `SigningContext` (Task 10) holds; `FILL_RETRIES` and `CREATION_RETRIES` (Task 5) are what the executor and the auctioneer set.
