# Phase 1: Fixed-Point Math and XDR Codecs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port Blend v2's reserve accrual, position valuation and auction scaling to checked `i128` arithmetic, and add the XDR encoders and decoders for every pool ledger entry, view-call result and pool event the bot reads, all pinned by tests against a committed mainnet fixture.

**Architecture:** Two new module trees. `math` is pure and chain-agnostic: `fixed` (rounding helpers with 256-bit widening), `reserve` (interest accrual and token conversions), `position` (effective values and health factor), `auction` (block-based scaling). `chain::xdr` turns Soroban `ScVal`s and ledger entries into those `math` types and builds the ledger keys and view-call envelopes the later RPC layer will send. A committed fixture captured from the mainnet "Fixed" pool at one ledger supplies ledger entries, the contract's own `get_reserve` and `get_positions` answers at that ledger, oracle prices, and real pool events, so every codec and every accrual is checked against what the contract computed.

**Tech Stack:** Rust 1.97 (pinned three ways), `stellar-xdr` 28.0.0 (features `base64`), `ethnum` 1.5 for 256-bit widening, `thiserror`, `serde_json` (dev and example only), `curl` as the capture tool's transport.

**Spec:** `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`, sections 1 (invariants), 3 (chain access and mathematics), 9 (testing). This plan is Phase 1 of the eight in section 12.

## Global Constraints

- The three-way Rust pin stays at 1.97.0 (`Cargo.toml` `rust-version`, `rust-toolchain.toml`, Dockerfile `FROM rust:1.97.0-bookworm`); do not touch any of the three.
- `clippy::pedantic` is warn-level and CI runs `cargo clippy --all-targets -- -D warnings`, so every pedantic finding is an error in every target including tests and examples. `unwrap_used` is denied outside tests; `expect_used` warns, which is also an error in CI outside tests (`clippy.toml` exempts tests only).
- Numeric literals must use 3-digit `_` grouping (`10_000_000`, never `1_000_0000`); `inconsistent_digit_grouping` and `unreadable_literal` are errors.
- Money is `i128` in each asset's own decimals; b-rates and d-rates are 12 decimals; factors and utilisation are 7 decimals; prices are in the oracle's decimals. No `f64` anywhere in these modules.
- No `as` numeric casts: use `i128::from`, `i64::from`, `u64::try_from`; the pedantic `cast_*` lints are errors.
- Doc comments state constraints and invariants, not narration.
- `make check` (fmt, clippy, tests, docs with `-D warnings`, invariants script, shellcheck) must be green at the end of every task before committing.
- Commit messages follow the repository's conventional style (`feat:`, `test:`, `docs:`, `chore:`) and end with the trailer `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`. Commits are signed by the container's git configuration; nothing to add.
- Work on branch `spec/bot-design` (where the spec, this plan and the fixture already live) or a branch off it; open one pull request for the whole phase.

---

## File Structure

| Path | Responsibility |
|---|---|
| `Cargo.toml` (modify) | add `stellar-xdr`, `ethnum`; dev-dependency `serde_json` |
| `src/liquidator.rs` (modify) | declare `pub mod chain; pub mod math;` and the test-only `fixture` module |
| `src/fixture.rs` (create, `cfg(test)`) | loads `tests/fixtures/mainnet-fixed-v2.json` for unit tests |
| `src/math/mod.rs` (create) | module root, re-exports |
| `src/math/fixed.rs` (create) | `MathError`, scalars, `mul_floor`/`mul_ceil`/`div_floor`/`div_ceil`/`pow10` |
| `src/math/reserve.rs` (create) | `ReserveConfig`, `ReserveData`, `Reserve`, token conversions, `calc_accrual`, `Reserve::accrue` |
| `src/math/position.rs` (create) | `Positions`, `OraclePrices`, `PositionData`, `calculate_position_data`, health-factor tests |
| `src/math/auction.rs` (create) | `AuctionData`, `ScaledAuction`, `scale_auction` |
| `src/chain/mod.rs` (create) | module root |
| `src/chain/xdr/mod.rs` (create) | `XdrError`, base64 helpers, re-exports |
| `src/chain/xdr/encode.rs` (create) | `symbol`, `address`, `stellar_asset`, `invoke_contract_op`, `simulation_envelope`, `to_base64` |
| `src/chain/xdr/keys.rs` (create) | ledger keys: instance, reserve list, reserve config/data, positions, auction |
| `src/chain/xdr/decode.rs` (create) | ledger entries and view-call results into `math` types |
| `src/chain/xdr/events.rs` (create) | `PoolEvent` and `decode_pool_event` |
| `examples/capture_fixture.rs` (create) | refreshes the fixture from a live RPC through `curl` |
| `tests/fixtures/mainnet-fixed-v2.json` (already committed) | the mainnet snapshot every test reads |
| `tests/fixtures/README.md` (create) | what the fixture holds and how to refresh it |
| `CLAUDE.md`, `CHANGELOG.md` (modify) | module map and changelog entry |

The fixture is a JSON object with these members, all XDR as base64 strings:

```text
rpc_url, pool, ledger, ledger_close_time, oracle,
instance_entry_xdr            LedgerEntryData of the pool's contract instance
res_list_entry_xdr            LedgerEntryData of the ResList entry
oracle_decimals_return_xdr    ScVal returned by oracle.decimals()
reserves[]                    { asset, config_entry_xdr, data_entry_xdr,
                                get_reserve_return_xdr, lastprice_return_xdr }
users[]                       { account, positions_entry_xdr, get_positions_return_xdr }
events[]                      raw getEvents objects { type, ledger, ledgerClosedAt,
                                contractId, id, topic[] (base64 ScVal), value (base64 ScVal),
                                txHash, ... }
```

Invariant of the fixture: every entry and every simulation was taken at the same ledger (`ledger` = 64271347, `ledger_close_time` = 1788534414), verified by re-fetching the entries after the simulations and requiring byte equality. The `get_reserve` results therefore equal the stored entries accrued to `ledger_close_time` with the pool's backstop take rate, which is what the accrual tests assert.

Values the tests assert, decoded from the committed fixture (pool `CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD`, "Fixed"):

| Item | Value |
|---|---|
| instance `Admin` | `GAX2VVWVHU5YQY5J3NJBXKHI3FFKZN54BE6GRJCWSIKSBZTQWJJNJMPC` |
| instance `Backstop` | `CAQQR5SWBXKIGZKPBZDH3KM5GQ5GUTPKB7JAFCINLZBC5WXPJKRG3IM7` |
| instance `BLNDTkn` | `CD25MNVTZDL4Y3XBCPCJXGXATV5WUHHOWMYFF4YBEGU5FCPGMYTVG5JY` |
| instance `Name` | `Fixed` |
| config | oracle `CCVTVW2CVA7JLH4ROQGP3CU4T3EXVCK66AZGSM4MUQPXAI4QHCZPOATS`, bstop_rate 2_000_000, status 1, max_positions 6, min_collateral 50_000_000 |
| reserve list | `[CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA (XLM), CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75 (USDC), CDTKPWPLOURQA2SGTKTUQOWRCBZEORB4BWBOMJ3D3ZTQQSGE5F6JBQLV (EURC)]` |
| oracle decimals | 7 |
| prices (timestamp 1_788_534_300) | XLM 1_778_617, USDC 9_999_165, EURC 11_613_052 |
| XLM config | index 0, decimals 7, c_factor 7_500_000, l_factor 7_500_000, util 4_000_000, max_util 7_000_000, r_base 100_000, r_one 300_000, r_two 3_000_000, r_three 50_000_000, reactivity 50, supply_cap 100_000_000_000_000_000, enabled true |
| XLM data (entry) | d_rate 1_001_568_283_884, b_rate 1_000_022_303_241, ir_mod 1_000_000, b_supply 7_654_654_078_715_796, d_supply 13_201_825_877_188, backstop_credit 31_426_481, last_time 1_788_533_688 |
| XLM data (get_reserve) | d_rate 1_001_568_307_242, b_rate 1_000_022_303_273, ir_mod 1_000_000, backstop_credit 31_488_154, last_time 1_788_534_414, scalar 10_000_000 |
| USDC data (entry) | d_rate 1_228_743_585_510, b_rate 1_143_597_595_724, ir_mod 14_810_061, b_supply 472_075_267_288_691, d_supply 354_986_019_743_103, backstop_credit 65_446_630_777, last_time 1_788_534_379 |
| USDC data (get_reserve) | d_rate 1_228_743_739_744, b_rate 1_143_597_688_507, ir_mod 14_810_066, backstop_credit 65_457_580_959 |
| EURC data (entry) | d_rate 1_228_440_145_896, b_rate 1_142_044_033_025, ir_mod 3_590_134, b_supply 3_087_825_484_723, d_supply 596_521_813_932, backstop_credit 171_404_770, last_time 1_788_533_750 |
| EURC data (get_reserve) | d_rate 1_228_440_520_958, b_rate 1_142_044_090_990, ir_mod 3_582_270, backstop_credit 171_449_516 |
| user 0 `GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE` | collateral {1: 125_043_746}, liabilities {1: 104_293_813}, supply {} |
| user 1 `GCIH7OYR6LX6364PGGLAKGMZYLV37EAH6YXZAFK7RY7U4K7625XBH5EL` | collateral {2: 7_618_671_504}, liabilities {2: 6_328_764_911}, supply {} |
| user 0 position data at the fixture ledger | collateral_base 135_838_407, collateral_raw 142_987_797, liability_base 134_883_864, liability_raw 128_139_670, health factor 10_070_767 |
| user 1 position data | collateral_base 9_599_134_909, collateral_raw 10_104_352_536, liability_base 9_503_768_801, liability_raw 9_028_580_360, health factor 10_100_345 |
| events | 15 real events: indices 0–2 `supply`, 3–5 `supply_collateral`, 6–8 `borrow`, 9–11 `repay`, 12–14 `withdraw_collateral` |

---

### Task 1: Dependencies, module skeleton, fixture loader

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/liquidator.rs`
- Create: `src/math/mod.rs`, `src/chain/mod.rs`, `src/chain/xdr/mod.rs`, `src/fixture.rs`, `tests/fixtures/README.md`

**Interfaces:**
- Produces: `crate::math`, `crate::chain::xdr` module paths; `crate::chain::xdr::XdrError`; `crate::fixture::mainnet_fixed_v2() -> serde_json::Value` (tests only).

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml`, replace the `[dependencies]` block's comment and add the two crates and the dev-dependency, keeping the existing entries:

```toml
# Deliberately minimal. Every dependency here is used; the Stellar/Soroban
# stack arrives with the code that needs it, not before, so that
# `cargo deny`'s license and advisory surface stays honest about what this
# crate actually builds.
[dependencies]
clap = { version = "4.6.0", features = ["derive", "env"] }
# 256-bit integers for the fixed-point widening path. Already in the tree
# through stellar-xdr, so this adds no new code to audit.
ethnum = "1.5"
# XDR types plus base64 for RPC payloads. No `soroban-sdk`: the handful of
# pool types the bot touches are encoded and decoded by hand in
# `chain::xdr`, which keeps every ScVal shape visible in one place.
stellar-xdr = { version = "28.0.0", features = ["base64"] }
thiserror = "2.0.18"
tokio = { version = "1.51.0", features = ["full"] }
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter", "json"] }

[dev-dependencies]
# Reads the JSON fixture in unit tests and drives the capture example.
serde_json = "1"
```

- [ ] **Step 2: Declare the modules**

In `src/liquidator.rs`, after the crate docs and before `pub mod config;`, add:

```rust
pub mod chain;
pub mod config;
pub mod math;

/// The committed mainnet snapshot every codec and math test reads.
#[cfg(test)]
pub(crate) mod fixture;
```

(Remove the existing `pub mod config;` line so it is declared once, in alphabetical order as above.) Update the crate docs' `# Status` paragraph: replace "there is no pool client, no scanner and no executor yet" with "the fixed-point math and XDR codecs exist (`math`, `chain::xdr`); there is no RPC client, scanner or executor yet".

- [ ] **Step 3: Create the module roots**

`src/math/mod.rs`:

```rust
//! Pure, chain-agnostic mathematics ported from the Blend v2 pool contract.
//!
//! Every function here mirrors a contract function with the same rounding
//! direction, because the bot's decisions are only as good as their
//! agreement with what the contract will compute at execution. Nothing in
//! this module performs I/O or panics: overflow and division by zero are
//! `MathError`s.

pub mod auction;
pub mod fixed;
pub mod position;
pub mod reserve;

pub use auction::{scale_auction, AuctionData, ScaledAuction};
pub use fixed::{div_ceil, div_floor, mul_ceil, mul_floor, pow10, MathError, SCALAR_12, SCALAR_7, SECONDS_PER_YEAR};
pub use position::{calculate_position_data, OraclePrices, PositionData, Positions};
pub use reserve::{calc_accrual, Reserve, ReserveConfig, ReserveData};
```

`src/chain/mod.rs`:

```rust
//! Everything that touches Soroban: XDR codecs now, the RPC client and pool
//! reads in later phases.

pub mod xdr;
```

`src/chain/xdr/mod.rs`:

```rust
//! ScVal and ledger-entry codecs for the Blend v2 pool contract.
//!
//! Hand-written rather than generated from the contract spec so that every
//! shape the bot depends on is visible here and pinned by a fixture test.
//! A shape mismatch after a contract upgrade fails a test, not a fill.

use crate::math::MathError;

pub mod decode;
pub mod encode;
pub mod events;
pub mod keys;

/// Failures turning chain data into bot types, or bot types into chain data.
///
/// `PartialEq` (but not `Eq`: `stellar_xdr::Error` carries an I/O variant)
/// so tests can compare whole `Result`s.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum XdrError {
    /// The XDR library rejected the bytes or the base64.
    #[error("xdr: {0}")]
    Xdr(#[from] stellar_xdr::Error),
    /// A string that is not a valid Stellar strkey for the expected kind.
    #[error("invalid address: {0}")]
    Address(String),
    /// A symbol longer than 32 bytes or with characters Soroban rejects.
    #[error("invalid symbol: {0}")]
    Symbol(String),
    /// The value decoded, but is not the shape this contract type has.
    #[error("unexpected value: expected {expected}, got {got}")]
    Shape {
        /// What the decoder was looking for.
        expected: &'static str,
        /// A debug rendering of what it found.
        got: String,
    },
    /// A struct map lacks a field the contract type always has.
    #[error("missing field {0}")]
    MissingField(&'static str),
    /// Decoded numbers could not be combined (e.g. `10^decimals` overflow).
    #[error("math: {0}")]
    Math(#[from] MathError),
}
```

- [ ] **Step 4: Create the fixture loader**

`src/fixture.rs`:

```rust
//! Access to the committed mainnet snapshot for unit tests.
//!
//! The file is embedded at compile time so tests need no working-directory
//! assumptions. See `tests/fixtures/README.md` for the capture procedure.

use serde_json::Value;

/// The raw fixture text.
pub(crate) const MAINNET_FIXED_V2: &str = include_str!("../tests/fixtures/mainnet-fixed-v2.json");

/// Parses the fixture; tests may `expect` because a malformed fixture is a
/// test-suite bug, not a runtime condition.
pub(crate) fn mainnet_fixed_v2() -> Value {
    serde_json::from_str(MAINNET_FIXED_V2).expect("fixture JSON parses")
}

/// A string member at `path` (e.g. `&["reserves", "0", "asset"]`), where a
/// numeric segment indexes an array.
pub(crate) fn text<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut current = value;
    for segment in path {
        current = match segment.parse::<usize>() {
            Ok(index) => &current[index],
            Err(_) => &current[*segment],
        };
    }
    current.as_str().unwrap_or_else(|| panic!("fixture path {path:?} is not a string"))
}
```

`tests/fixtures/README.md`:

```markdown
# Test fixtures

`mainnet-fixed-v2.json` is a snapshot of the Blend v2 mainnet pool
`CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD` ("Fixed") taken
at ledger 64271347 (close time 1788534414). It holds, all as base64 XDR:

- the pool's contract instance and `ResList` ledger entries;
- each reserve's `ResConfig` and `ResData` entries, the contract's own
  `get_reserve` answer at the same ledger, and the oracle's `lastprice`;
- the oracle's `decimals`;
- two borrowers' `Positions` entries and `get_positions` answers;
- fifteen real pool events (`supply`, `supply_collateral`, `borrow`,
  `repay`, `withdraw_collateral`) from the retained history.

Every entry and simulation was taken at one ledger: the capture re-fetches
the entries after the simulations and requires byte equality, retrying
otherwise. That is what lets tests assert that accruing the stored entries
to the close time reproduces `get_reserve` exactly.

## Refreshing

```bash
cargo run --example capture_fixture -- \
  https://mainnet.sorobanrpc.com \
  CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD \
  tests/fixtures/mainnet-fixed-v2.json \
  GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE \
  GCIH7OYR6LX6364PGGLAKGMZYLV37EAH6YXZAFK7RY7U4K7625XBH5EL
```

The tool needs `curl` on the PATH. A refresh changes every literal the
tests assert (rates, prices, position values), so refresh only when a
contract upgrade changes a shape, and update the literals in the same
change. Borrower addresses can be found with the public Blend analytics
API: `GET https://api.blend.templarfi.org/v1/analytics/state/positions?healthFactorMax=100&poolId=<pool>&limit=5`.
```

- [ ] **Step 5: Build and verify**

Run: `cargo build && cargo test --lib --bins`
Expected: builds; the existing config tests still pass (`4 passed`). `cargo clippy --all-targets -- -D warnings` may report `dead_code` for `fixture::text` until Task 5 uses it: silence nothing; proceed, the next tasks use it. If clippy fails on anything else, fix it now.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/liquidator.rs src/math/mod.rs src/chain/mod.rs src/chain/xdr/mod.rs src/fixture.rs tests/fixtures/README.md
git commit -m "chore: add stellar-xdr and ethnum, module skeleton, fixture loader

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

(The build will not link until Tasks 2–8 create the declared submodules. To keep every commit green, create each declared file in this task as an empty module with only a `//!` doc line, and let later tasks fill them: `src/math/{fixed,reserve,position,auction}.rs`, `src/chain/xdr/{decode,encode,events,keys}.rs`, each containing one line such as `//! Fixed-point helpers (Task 2).` Remove the re-export lines from `src/math/mod.rs` for items that do not exist yet and add them back in the task that creates them.)

---

### Task 2: `math::fixed`

**Files:**
- Create: `src/math/fixed.rs`
- Modify: `src/math/mod.rs` (re-exports)

**Interfaces:**
- Produces:
  - `pub const SCALAR_7: i128`, `SCALAR_12: i128`, `SECONDS_PER_YEAR: i128`
  - `pub enum MathError { DivisionByZero, Overflow, InvalidInput(&'static str), MissingReserve(u32), MissingPrice(String) }`
  - `pub fn mul_floor(x: i128, y: i128, denominator: i128) -> Result<i128, MathError>` = floor(x·y/denominator)
  - `pub fn mul_ceil(...)` = ceil(x·y/denominator)
  - `pub fn div_floor(x: i128, y: i128, denominator: i128) -> Result<i128, MathError>` = floor(x·denominator/y)
  - `pub fn div_ceil(...)` = ceil(x·denominator/y)
  - `pub fn pow10(decimals: u32) -> Result<i128, MathError>`

- [ ] **Step 1: Write the failing tests**

Create `src/math/fixed.rs` with the module doc, the items below as stubs that `todo!()`, and this test module. Stubs are acceptable for one commit-free step; they never get committed.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_and_ceil_agree_with_mathematics_for_positive_values() {
        assert_eq!(mul_floor(3, 2, 4), Ok(1));
        assert_eq!(mul_ceil(3, 2, 4), Ok(2));
        assert_eq!(mul_floor(4, 2, 4), Ok(2));
        assert_eq!(mul_ceil(4, 2, 4), Ok(2));
        assert_eq!(div_floor(1, 3, SCALAR_7), Ok(3_333_333));
        assert_eq!(div_ceil(1, 3, SCALAR_7), Ok(3_333_334));
    }

    #[test]
    fn floor_and_ceil_agree_with_mathematics_for_negative_values() {
        // -1.5 floors to -2 and ceils to -1, as the contract's library does.
        assert_eq!(mul_floor(-3, 1, 2), Ok(-2));
        assert_eq!(mul_ceil(-3, 1, 2), Ok(-1));
        // A negative divisor is normalised first: 7 / -2 = -3.5.
        assert_eq!(mul_floor(7, 1, -2), Ok(-4));
        assert_eq!(mul_ceil(7, 1, -2), Ok(-3));
        assert_eq!(div_floor(-7, 2, 1), Ok(-4));
        assert_eq!(div_ceil(-7, 2, 1), Ok(-3));
    }

    #[test]
    fn zero_divisor_is_an_error_not_a_panic() {
        assert_eq!(mul_floor(1, 1, 0), Err(MathError::DivisionByZero));
        assert_eq!(mul_ceil(1, 1, 0), Err(MathError::DivisionByZero));
        assert_eq!(div_floor(1, 0, SCALAR_7), Err(MathError::DivisionByZero));
        assert_eq!(div_ceil(1, 0, SCALAR_7), Err(MathError::DivisionByZero));
    }

    #[test]
    fn product_overflow_widens_to_256_bits() {
        assert_eq!(mul_floor(i128::MAX, 3, 3), Ok(i128::MAX));
        assert_eq!(mul_ceil(i128::MAX, 3, 3), Ok(i128::MAX));
        assert_eq!(mul_floor(i128::MIN, 2, 2), Ok(i128::MIN));
        // 2^126 * 2^126 / 2^125 = 2^127, which does not fit.
        let big = 1_i128 << 126;
        assert_eq!(mul_floor(big, big, 1_i128 << 125), Err(MathError::Overflow));
        assert_eq!(mul_floor(i128::MAX, 2, 1), Err(MathError::Overflow));
    }

    #[test]
    fn i128_min_as_divisor_or_dividend_does_not_spuriously_overflow() {
        // Negating either operand to normalise the divisor's sign would
        // report Overflow for results that fit; only a true 2^127 result
        // may.
        assert_eq!(mul_floor(1, 1, i128::MIN), Ok(-1));
        assert_eq!(mul_ceil(1, 1, i128::MIN), Ok(0));
        assert_eq!(mul_floor(i128::MIN, 1, -2), Ok(1_i128 << 126));
        assert_eq!(mul_ceil(i128::MIN, 1, -2), Ok(1_i128 << 126));
        assert_eq!(mul_floor(i128::MIN, 1, -1), Err(MathError::Overflow));
    }

    #[test]
    fn pow10_covers_token_decimals_and_rejects_overflow() {
        assert_eq!(pow10(0), Ok(1));
        assert_eq!(pow10(7), Ok(SCALAR_7));
        assert_eq!(pow10(12), Ok(SCALAR_12));
        assert_eq!(pow10(38), Ok(100_000_000_000_000_000_000_000_000_000_000_000_000));
        assert_eq!(pow10(39), Err(MathError::Overflow));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib math::fixed`
Expected: the six tests panic on `todo!()`.

- [ ] **Step 3: Implement**

Replace the stubs with:

```rust
//! Checked fixed-point arithmetic with the contract's rounding.
//!
//! Every helper computes `x * y / denominator` (or `x * denominator / y`)
//! with mathematical floor or ceiling, widening to 256 bits when the
//! 128-bit product overflows, exactly as the pool's `soroban-fixed-point-math`
//! does. Nothing here panics: a zero divisor or a result outside `i128` is
//! an error, and callers decide what that means for a decision.

use ethnum::I256;

/// Scalar for 7-decimal values: factors, utilisation, XLM-style amounts.
pub const SCALAR_7: i128 = 10_000_000;
/// Scalar for 12-decimal values: v2 `b_rate` and `d_rate`.
pub const SCALAR_12: i128 = 1_000_000_000_000;
/// Seconds in a year, the accrual time base.
pub const SECONDS_PER_YEAR: i128 = 31_536_000;

/// Arithmetic that could not produce a valid `i128`, or inputs a formula
/// cannot accept. Carried through every `math` and `chain::xdr` result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MathError {
    /// A zero denominator or divisor.
    #[error("division by zero")]
    DivisionByZero,
    /// The result does not fit in `i128`, or `10^decimals` does not.
    #[error("arithmetic overflow")]
    Overflow,
    /// An argument outside the formula's domain, named.
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    /// A position references a reserve index the pool does not have.
    #[error("no reserve at index {0}")]
    MissingReserve(u32),
    /// A position references an asset the oracle snapshot has no price for.
    #[error("no oracle price for {0}")]
    MissingPrice(String),
}

#[derive(Clone, Copy)]
enum Rounding {
    Floor,
    Ceil,
}

/// floor(x * y / denominator).
pub fn mul_floor(x: i128, y: i128, denominator: i128) -> Result<i128, MathError> {
    mul_div(x, y, denominator, Rounding::Floor)
}

/// ceil(x * y / denominator).
pub fn mul_ceil(x: i128, y: i128, denominator: i128) -> Result<i128, MathError> {
    mul_div(x, y, denominator, Rounding::Ceil)
}

/// floor(x * denominator / y).
pub fn div_floor(x: i128, y: i128, denominator: i128) -> Result<i128, MathError> {
    mul_div(x, denominator, y, Rounding::Floor)
}

/// ceil(x * denominator / y).
pub fn div_ceil(x: i128, y: i128, denominator: i128) -> Result<i128, MathError> {
    mul_div(x, denominator, y, Rounding::Ceil)
}

/// `10^decimals`, the scalar of a token with that many decimals.
pub fn pow10(decimals: u32) -> Result<i128, MathError> {
    10_i128.checked_pow(decimals).ok_or(MathError::Overflow)
}

fn mul_div(x: i128, y: i128, z: i128, rounding: Rounding) -> Result<i128, MathError> {
    if z == 0 {
        return Err(MathError::DivisionByZero);
    }
    match x.checked_mul(y) {
        Some(product) => divide_narrow(product, z, rounding),
        None => divide_wide(I256::new(x) * I256::new(y), I256::new(z), rounding),
    }
}

/// Division with a sign-normalised divisor so `div_euclid` is a true floor.
fn divide_narrow(r: i128, z: i128, rounding: Rounding) -> Result<i128, MathError> {
    // `i128::MIN` has no positive `i128` counterpart, so negating it below
    // would report `Overflow` even when the true quotient fits comfortably
    // in `i128` (e.g. `i128::MIN / -2`). The 256-bit path has the headroom
    // to negate exactly and is already proven correct for every sign
    // combination, so route these two cases through it instead.
    if r == i128::MIN || z == i128::MIN {
        return divide_wide(I256::new(r), I256::new(z), rounding);
    }
    let (r, z) = if z < 0 {
        (r.checked_neg().ok_or(MathError::Overflow)?, z.checked_neg().ok_or(MathError::Overflow)?)
    } else {
        (r, z)
    };
    let quotient = r.checked_div_euclid(z).ok_or(MathError::Overflow)?;
    match rounding {
        Rounding::Floor => Ok(quotient),
        Rounding::Ceil if r.rem_euclid(z) == 0 => Ok(quotient),
        Rounding::Ceil => quotient.checked_add(1).ok_or(MathError::Overflow),
    }
}

/// The 256-bit path: the product of two `i128`s always fits, only the
/// quotient may not.
fn divide_wide(r: I256, z: I256, rounding: Rounding) -> Result<i128, MathError> {
    let (r, z) = if z < I256::ZERO { (-r, -z) } else { (r, z) };
    let quotient = r.div_euclid(z);
    let quotient = match rounding {
        Rounding::Floor => quotient,
        Rounding::Ceil if r.rem_euclid(z) == I256::ZERO => quotient,
        Rounding::Ceil => quotient + I256::ONE,
    };
    i128::try_from(quotient).map_err(|_| MathError::Overflow)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib math::fixed`
Expected: `6 passed`.

- [ ] **Step 5: Lint and commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --all`
Expected: clean. Then:

```bash
git add src/math/fixed.rs src/math/mod.rs
git commit -m "feat(math): checked fixed-point helpers with 256-bit widening

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `math::reserve` — types, conversions, accrual

**Files:**
- Create: `src/math/reserve.rs`
- Modify: `src/math/mod.rs` (re-exports)

**Interfaces:**
- Consumes: `fixed::{mul_floor, mul_ceil, div_floor, div_ceil, pow10, MathError, SCALAR_7, SCALAR_12, SECONDS_PER_YEAR}`
- Produces:
  - `pub struct ReserveConfig { pub index: u32, pub decimals: u32, pub c_factor: u32, pub l_factor: u32, pub util: u32, pub max_util: u32, pub r_base: u32, pub r_one: u32, pub r_two: u32, pub r_three: u32, pub reactivity: u32, pub supply_cap: i128, pub enabled: bool }`
  - `pub struct ReserveData { pub d_rate: i128, pub b_rate: i128, pub ir_mod: i128, pub b_supply: i128, pub d_supply: i128, pub backstop_credit: i128, pub last_time: u64 }`
  - `pub struct Reserve { pub asset: String, pub config: ReserveConfig, pub data: ReserveData, pub scalar: i128 }`
  - `Reserve::new(asset: String, config: ReserveConfig, data: ReserveData) -> Result<Reserve, MathError>`
  - `Reserve::{total_liabilities, total_supply, utilization}(&self) -> Result<i128, MathError>`
  - `Reserve::{to_asset_from_d_token, to_asset_from_b_token, to_effective_asset_from_d_token, to_effective_asset_from_b_token, to_d_token_up, to_d_token_down, to_b_token_up, to_b_token_down}(&self, amount: i128) -> Result<i128, MathError>`
  - `pub fn calc_accrual(config: &ReserveConfig, cur_util: i128, ir_mod: i128, last_time: u64, now: u64) -> Result<(i128, i128), MathError>` returning `(accrual_12dec, new_ir_mod)`
  - `Reserve::accrue(&mut self, bstop_rate: u32, now: u64) -> Result<(), MathError>`

- [ ] **Step 1: Write the failing tests**

Create `src/math/reserve.rs` with stubs and this test module. The accrual expectation is the contract's own `get_reserve` answer, captured from mainnet at close time 1788533824 for the XLM reserve whose entry is in the fixture table above.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn xlm_config() -> ReserveConfig {
        ReserveConfig {
            index: 0,
            decimals: 7,
            c_factor: 7_500_000,
            l_factor: 7_500_000,
            util: 4_000_000,
            max_util: 7_000_000,
            r_base: 100_000,
            r_one: 300_000,
            r_two: 3_000_000,
            r_three: 50_000_000,
            reactivity: 50,
            supply_cap: 100_000_000_000_000_000,
            enabled: true,
        }
    }

    fn xlm_data() -> ReserveData {
        ReserveData {
            d_rate: 1_001_568_283_884,
            b_rate: 1_000_022_303_241,
            ir_mod: 1_000_000,
            b_supply: 7_654_654_078_715_796,
            d_supply: 13_201_825_877_188,
            backstop_credit: 31_426_481,
            last_time: 1_788_533_688,
        }
    }

    fn simple_reserve() -> Reserve {
        let config = ReserveConfig { c_factor: 7_500_000, l_factor: 7_500_000, ..xlm_config() };
        let data = ReserveData {
            d_rate: 1_100_000_000_000,
            b_rate: 1_100_000_000_000,
            ir_mod: SCALAR_7,
            b_supply: 1_000_000,
            d_supply: 500_000,
            backstop_credit: 0,
            last_time: 0,
        };
        Reserve::new("XLM".to_string(), config, data).expect("7 decimals fit")
    }

    #[test]
    fn new_derives_the_scalar_from_decimals() {
        let reserve = simple_reserve();
        assert_eq!(reserve.scalar, SCALAR_7);
        let config = ReserveConfig { decimals: 39, ..xlm_config() };
        assert_eq!(Reserve::new("X".to_string(), config, xlm_data()).err(), Some(MathError::Overflow));
    }

    #[test]
    fn token_conversions_round_the_way_the_contract_does() {
        let reserve = simple_reserve();
        // d-tokens to underlying round up; b-tokens to underlying round down.
        assert_eq!(reserve.to_asset_from_d_token(1_000), Ok(1_100));
        assert_eq!(reserve.to_asset_from_d_token(1), Ok(2));
        assert_eq!(reserve.to_asset_from_b_token(1_000), Ok(1_100));
        assert_eq!(reserve.to_asset_from_b_token(1), Ok(1));
        // Effective liability divides by the liability factor and rounds up;
        // effective collateral multiplies by the collateral factor and rounds down.
        assert_eq!(reserve.to_effective_asset_from_d_token(1_000), Ok(1_467));
        assert_eq!(reserve.to_effective_asset_from_b_token(1_000), Ok(825));
        // Underlying to tokens, both directions of rounding.
        assert_eq!(reserve.to_d_token_up(1_101), Ok(1_001));
        assert_eq!(reserve.to_d_token_down(1_101), Ok(1_000));
        assert_eq!(reserve.to_b_token_up(1_101), Ok(1_001));
        assert_eq!(reserve.to_b_token_down(1_101), Ok(1_000));
    }

    #[test]
    fn utilization_is_liabilities_over_supply_capped_at_one() {
        let reserve = simple_reserve();
        // 550_000 liabilities over 1_100_000 supply, rounded up at 7 decimals.
        assert_eq!(reserve.utilization(), Ok(5_000_000));
        let mut full = simple_reserve();
        full.data.d_supply = full.data.b_supply * 2;
        assert_eq!(full.utilization(), Ok(SCALAR_7));
        let mut empty = simple_reserve();
        empty.data.d_supply = 0;
        assert_eq!(empty.utilization(), Ok(0));
    }

    #[test]
    fn accrue_reproduces_the_contracts_get_reserve() {
        let mut reserve = Reserve::new("XLM".to_string(), xlm_config(), xlm_data()).expect("scalar");
        reserve.accrue(2_000_000, 1_788_533_824).expect("accrues");
        assert_eq!(reserve.data.d_rate, 1_001_568_288_260);
        assert_eq!(reserve.data.b_rate, 1_000_022_303_247);
        assert_eq!(reserve.data.ir_mod, 1_000_000);
        assert_eq!(reserve.data.backstop_credit, 31_438_035);
        assert_eq!(reserve.data.last_time, 1_788_533_824);
        assert_eq!(reserve.data.b_supply, 7_654_654_078_715_796);
        assert_eq!(reserve.data.d_supply, 13_201_825_877_188);
    }

    #[test]
    fn accrue_is_a_no_op_within_the_same_second() {
        let mut reserve = Reserve::new("XLM".to_string(), xlm_config(), xlm_data()).expect("scalar");
        let before = reserve.data.clone();
        reserve.accrue(2_000_000, before.last_time).expect("accrues");
        assert_eq!(reserve.data, before);
    }

    #[test]
    fn accrue_only_stamps_time_when_nothing_is_borrowed() {
        let mut reserve = simple_reserve();
        reserve.data.d_supply = 0;
        reserve.accrue(2_000_000, 100).expect("accrues");
        assert_eq!(reserve.data.d_rate, 1_100_000_000_000);
        assert_eq!(reserve.data.last_time, 100);
        let mut empty = simple_reserve();
        empty.data.b_supply = 0;
        empty.accrue(2_000_000, 100).expect("accrues");
        assert_eq!(empty.data.last_time, 100);
    }

    #[test]
    fn calc_accrual_reports_overflow_instead_of_panicking_on_ir_mod_add() {
        // `ir_mod` is decoded straight off the chain, so `calc_accrual`
        // cannot assume it leaves room for `+ rate_dif`. Above target
        // utilisation takes the increasing branch, where the next `ir_mod`
        // is `ir_mod + rate_dif`; with `ir_mod` already at `i128::MAX` and
        // utilisation above target (a positive `rate_dif`), that addition
        // must report `Overflow` rather than panic -- `overflow-checks`
        // is on even in the release profile, so an unchecked `+` here
        // would abort the process, not just a debug build.
        let config = xlm_config();
        assert_eq!(
            calc_accrual(&config, 5_000_000, i128::MAX, 0, 1),
            Err(MathError::Overflow)
        );
    }

    #[test]
    fn accrue_rejects_time_running_backwards() {
        let mut reserve = Reserve::new("XLM".to_string(), xlm_config(), xlm_data()).expect("scalar");
        assert_eq!(reserve.accrue(2_000_000, 1_788_533_000), Err(MathError::InvalidInput("now is before last_time")));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib math::reserve`
Expected: panics on `todo!()`.

- [ ] **Step 3: Implement**

```rust
//! Reserve state and the interest accrual the contract performs on every
//! load.
//!
//! Ledger entries hold a reserve as of its last update; the contract accrues
//! to the current ledger inside every call. `Reserve::accrue` ports
//! `Reserve::load` plus `interest::calc_accrual` from the v2 pool so the bot
//! values positions on the same numbers the contract will use. Rates are 12
//! decimals, factors and utilisation 7 decimals, `ir_mod` 7 decimals.

use super::fixed::{div_ceil, div_floor, mul_ceil, mul_floor, pow10, MathError, SCALAR_12, SCALAR_7, SECONDS_PER_YEAR};

/// The `ResConfig` ledger entry. Factors and rates are 7 decimals;
/// `supply_cap` is in the underlying's decimals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveConfig {
    pub index: u32,
    pub decimals: u32,
    pub c_factor: u32,
    pub l_factor: u32,
    pub util: u32,
    pub max_util: u32,
    pub r_base: u32,
    pub r_one: u32,
    pub r_two: u32,
    pub r_three: u32,
    pub reactivity: u32,
    pub supply_cap: i128,
    pub enabled: bool,
}

/// The `ResData` ledger entry. `d_rate` and `b_rate` are 12 decimals,
/// `ir_mod` is 7 decimals, supplies are token amounts in the underlying's
/// decimals, `last_time` is a ledger close time in seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveData {
    pub d_rate: i128,
    pub b_rate: i128,
    pub ir_mod: i128,
    pub b_supply: i128,
    pub d_supply: i128,
    pub backstop_credit: i128,
    pub last_time: u64,
}

/// A reserve as the contract's `Reserve` struct: config, data and the
/// underlying's scalar `10^decimals`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserve {
    /// Strkey contract address of the underlying asset.
    pub asset: String,
    pub config: ReserveConfig,
    pub data: ReserveData,
    pub scalar: i128,
}

const UTIL_95: i128 = 9_500_000;
const UTIL_5: i128 = 500_000;
const IR_MOD_MAX: i128 = 10 * SCALAR_7;
const IR_MOD_MIN: i128 = SCALAR_7 / 10;

impl Reserve {
    /// Builds a reserve, deriving `scalar` from `config.decimals`.
    pub fn new(asset: String, config: ReserveConfig, data: ReserveData) -> Result<Self, MathError> {
        let scalar = pow10(config.decimals)?;
        Ok(Self { asset, config, data, scalar })
    }

    /// Total borrowed underlying: `d_supply` at `d_rate`, rounded up.
    pub fn total_liabilities(&self) -> Result<i128, MathError> {
        self.to_asset_from_d_token(self.data.d_supply)
    }

    /// Total supplied underlying: `b_supply` at `b_rate`, rounded down.
    pub fn total_supply(&self) -> Result<i128, MathError> {
        self.to_asset_from_b_token(self.data.b_supply)
    }

    /// Utilisation at 7 decimals, capped at 100% so the rate curve stays fair.
    pub fn utilization(&self) -> Result<i128, MathError> {
        let liabilities = self.total_liabilities()?;
        let supply = self.total_supply()?;
        if liabilities == 0 {
            Ok(0)
        } else if liabilities >= supply {
            Ok(SCALAR_7)
        } else {
            div_ceil(liabilities, supply, SCALAR_7)
        }
    }

    /// d-tokens to underlying, rounded up (the borrower owes the ceiling).
    pub fn to_asset_from_d_token(&self, d_tokens: i128) -> Result<i128, MathError> {
        mul_ceil(d_tokens, self.data.d_rate, SCALAR_12)
    }

    /// b-tokens to underlying, rounded down (the supplier gets the floor).
    pub fn to_asset_from_b_token(&self, b_tokens: i128) -> Result<i128, MathError> {
        mul_floor(b_tokens, self.data.b_rate, SCALAR_12)
    }

    /// d-tokens to effective liability: underlying divided by `l_factor`, up.
    pub fn to_effective_asset_from_d_token(&self, d_tokens: i128) -> Result<i128, MathError> {
        let assets = self.to_asset_from_d_token(d_tokens)?;
        div_ceil(assets, i128::from(self.config.l_factor), SCALAR_7)
    }

    /// b-tokens to effective collateral: underlying times `c_factor`, down.
    pub fn to_effective_asset_from_b_token(&self, b_tokens: i128) -> Result<i128, MathError> {
        let assets = self.to_asset_from_b_token(b_tokens)?;
        mul_floor(assets, i128::from(self.config.c_factor), SCALAR_7)
    }

    /// Underlying to d-tokens, rounded up.
    pub fn to_d_token_up(&self, amount: i128) -> Result<i128, MathError> {
        div_ceil(amount, self.data.d_rate, SCALAR_12)
    }

    /// Underlying to d-tokens, rounded down.
    pub fn to_d_token_down(&self, amount: i128) -> Result<i128, MathError> {
        div_floor(amount, self.data.d_rate, SCALAR_12)
    }

    /// Underlying to b-tokens, rounded up.
    pub fn to_b_token_up(&self, amount: i128) -> Result<i128, MathError> {
        div_ceil(amount, self.data.b_rate, SCALAR_12)
    }

    /// Underlying to b-tokens, rounded down.
    pub fn to_b_token_down(&self, amount: i128) -> Result<i128, MathError> {
        div_floor(amount, self.data.b_rate, SCALAR_12)
    }

    /// Accrues interest to `now` exactly as the contract's `Reserve::load`:
    /// no-op within the same second, time-stamp only when nothing is
    /// supplied or borrowed, otherwise update `ir_mod`, `d_rate`, the
    /// backstop credit and `b_rate`. `now` before `last_time` is invalid.
    pub fn accrue(&mut self, bstop_rate: u32, now: u64) -> Result<(), MathError> {
        if now == self.data.last_time {
            return Ok(());
        }
        if now < self.data.last_time {
            return Err(MathError::InvalidInput("now is before last_time"));
        }
        if self.data.b_supply == 0 {
            self.data.last_time = now;
            return Ok(());
        }
        let cur_util = self.utilization()?;
        if cur_util == 0 {
            self.data.last_time = now;
            return Ok(());
        }
        let (loan_accrual, new_ir_mod) = calc_accrual(&self.config, cur_util, self.data.ir_mod, self.data.last_time, now)?;
        self.data.ir_mod = new_ir_mod;

        let pre_update_supply = self.total_supply()?;
        let pre_update_liabilities = self.total_liabilities()?;
        self.data.d_rate = mul_ceil(loan_accrual, self.data.d_rate, SCALAR_12)?;
        let accrued = self.total_liabilities()?.checked_sub(pre_update_liabilities).ok_or(MathError::Overflow)?;
        if accrued > 0 {
            let mut new_backstop_credit = 0;
            if bstop_rate > 0 {
                new_backstop_credit = mul_floor(accrued, i128::from(bstop_rate), SCALAR_7)?;
                self.data.backstop_credit = self.data.backstop_credit.checked_add(new_backstop_credit).ok_or(MathError::Overflow)?;
            }
            let supply_after = pre_update_supply
                .checked_add(accrued)
                .and_then(|s| s.checked_sub(new_backstop_credit))
                .ok_or(MathError::Overflow)?;
            self.data.b_rate = div_floor(supply_after, self.data.b_supply, SCALAR_12)?;
        }
        self.data.last_time = now;
        Ok(())
    }
}

/// The contract's `interest::calc_accrual`: returns the loan accrual factor
/// at 12 decimals and the next interest-rate modifier at 7 decimals.
/// `cur_util` is 7 decimals; `now` must be at least one second after
/// `last_time`.
pub fn calc_accrual(
    config: &ReserveConfig,
    cur_util: i128,
    ir_mod: i128,
    last_time: u64,
    now: u64,
) -> Result<(i128, i128), MathError> {
    let target_util = i128::from(config.util);
    let cur_ir = if cur_util <= target_util {
        let util_scalar = div_ceil(cur_util, target_util, SCALAR_7)?;
        let base_rate = mul_ceil(util_scalar, i128::from(config.r_one), SCALAR_7)?
            .checked_add(i128::from(config.r_base))
            .ok_or(MathError::Overflow)?;
        mul_ceil(base_rate, ir_mod, SCALAR_7)?
    } else if cur_util <= UTIL_95 {
        let util_dif = cur_util
            .checked_sub(target_util)
            .ok_or(MathError::Overflow)?;
        let util_range = UTIL_95
            .checked_sub(target_util)
            .ok_or(MathError::Overflow)?;
        let util_scalar = div_ceil(util_dif, util_range, SCALAR_7)?;
        let base_rate = mul_ceil(util_scalar, i128::from(config.r_two), SCALAR_7)?
            .checked_add(i128::from(config.r_one))
            .and_then(|sum| sum.checked_add(i128::from(config.r_base)))
            .ok_or(MathError::Overflow)?;
        mul_ceil(base_rate, ir_mod, SCALAR_7)?
    } else {
        let util_dif = cur_util.checked_sub(UTIL_95).ok_or(MathError::Overflow)?;
        let util_scalar = div_ceil(util_dif, UTIL_5, SCALAR_7)?;
        let extra_rate = mul_ceil(util_scalar, i128::from(config.r_three), SCALAR_7)?;
        let rate_sum = i128::from(config.r_two)
            .checked_add(i128::from(config.r_one))
            .and_then(|sum| sum.checked_add(i128::from(config.r_base)))
            .ok_or(MathError::Overflow)?;
        let intersection = mul_ceil(ir_mod, rate_sum, SCALAR_7)?;
        extra_rate
            .checked_add(intersection)
            .ok_or(MathError::Overflow)?
    };

    let delta_time = i128::from(
        now.checked_sub(last_time)
            .ok_or(MathError::InvalidInput("now is before last_time"))?,
    );
    if delta_time < 1 {
        return Err(MathError::InvalidInput("no time elapsed"));
    }
    let util_dif = cur_util
        .checked_sub(target_util)
        .ok_or(MathError::Overflow)?;
    let util_error = delta_time
        .checked_mul(util_dif)
        .ok_or(MathError::Overflow)?;
    let new_ir_mod = if util_dif >= 0 {
        let rate_dif = mul_floor(util_error, i128::from(config.reactivity), SCALAR_7)?;
        ir_mod
            .checked_add(rate_dif)
            .ok_or(MathError::Overflow)?
            .min(IR_MOD_MAX)
    } else {
        let rate_dif = mul_ceil(util_error, i128::from(config.reactivity), SCALAR_7)?;
        ir_mod
            .checked_add(rate_dif)
            .ok_or(MathError::Overflow)?
            .max(IR_MOD_MIN)
    };

    let time_weight = delta_time
        .checked_mul(SCALAR_12)
        .ok_or(MathError::Overflow)?
        / SECONDS_PER_YEAR;
    let accrual = SCALAR_12
        .checked_add(mul_ceil(time_weight, cur_ir, SCALAR_7)?)
        .ok_or(MathError::Overflow)?;
    Ok((accrual, new_ir_mod))
}
```

If clippy reports `similar_names` for `b_rate`/`d_rate` or `b_supply`/`d_supply`, add `#[allow(clippy::similar_names)]` on the offending function with the comment `// the contract's own field names`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib math::reserve`
Expected: `8 passed`.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt --all
git add src/math/reserve.rs src/math/mod.rs
git commit -m "feat(math): reserve types, token conversions and interest accrual

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: `math::position`

**Files:**
- Create: `src/math/position.rs`
- Modify: `src/math/mod.rs` (re-exports)

**Interfaces:**
- Consumes: `fixed::{mul_floor, mul_ceil, div_floor, pow10, MathError, SCALAR_7}`, `reserve::Reserve`
- Produces:
  - `pub struct Positions { pub collateral: BTreeMap<u32, i128>, pub liabilities: BTreeMap<u32, i128>, pub supply: BTreeMap<u32, i128> }` with `Positions::effective_count(&self) -> usize` and `Positions::is_empty(&self) -> bool`
  - `pub struct OraclePrices { pub decimals: u32, pub scalar: i128, pub prices: BTreeMap<String, i128> }` with `OraclePrices::new(decimals: u32, prices: BTreeMap<String, i128>) -> Result<Self, MathError>` and `OraclePrices::price(&self, asset: &str) -> Result<i128, MathError>`
  - `pub struct PositionData { pub collateral_base: i128, pub collateral_raw: i128, pub liability_base: i128, pub liability_raw: i128, pub scalar: i128 }` with `health_factor(&self) -> Result<Option<i128>, MathError>`, `is_hf_over(&self, max: i128) -> Result<bool, MathError>`, `is_hf_under(&self, min: i128) -> Result<bool, MathError>`
  - `pub fn calculate_position_data(reserves: &BTreeMap<u32, Reserve>, prices: &OraclePrices, positions: &Positions) -> Result<PositionData, MathError>`

- [ ] **Step 1: Write the failing tests**

Create `src/math/position.rs` with stubs and this test module. Every expected value below was computed by hand from the contract's formula; the fixture cross-check that proves the port against real chain data lives in Task 7.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::reserve::{ReserveConfig, ReserveData};

    const ASSET_A: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const ASSET_B: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    /// 7 decimals, both rates 1.1, both factors 0.75.
    fn reserve(index: u32, asset: &str) -> Reserve {
        let config = ReserveConfig {
            index,
            decimals: 7,
            c_factor: 7_500_000,
            l_factor: 7_500_000,
            util: 4_000_000,
            max_util: 7_000_000,
            r_base: 100_000,
            r_one: 300_000,
            r_two: 3_000_000,
            r_three: 50_000_000,
            reactivity: 50,
            supply_cap: 100_000_000_000_000_000,
            enabled: true,
        };
        let data = ReserveData {
            d_rate: 1_100_000_000_000,
            b_rate: 1_100_000_000_000,
            ir_mod: SCALAR_7,
            b_supply: 1_000_000_000,
            d_supply: 500_000_000,
            backstop_credit: 0,
            last_time: 0,
        };
        Reserve::new(asset.to_string(), config, data).expect("7 decimals fit")
    }

    /// Oracle at 7 decimals with both assets priced at 2.0.
    fn prices() -> OraclePrices {
        let mut map = BTreeMap::new();
        map.insert(ASSET_A.to_string(), 20_000_000);
        map.insert(ASSET_B.to_string(), 20_000_000);
        OraclePrices::new(7, map).expect("7 decimals fit")
    }

    fn one_reserve() -> BTreeMap<u32, Reserve> {
        BTreeMap::from([(0, reserve(0, ASSET_A))])
    }

    fn two_reserves() -> BTreeMap<u32, Reserve> {
        BTreeMap::from([(0, reserve(0, ASSET_A)), (1, reserve(1, ASSET_B))])
    }

    fn position(entries: &[(u32, i128, i128)]) -> Positions {
        let mut positions = Positions::default();
        for (index, collateral, liability) in entries {
            if *collateral > 0 {
                positions.collateral.insert(*index, *collateral);
            }
            if *liability > 0 {
                positions.liabilities.insert(*index, *liability);
            }
        }
        positions
    }

    #[test]
    fn values_one_position_the_way_the_contract_does() {
        // 2_000_000 b-tokens at rate 1.1 is 2_200_000 underlying, 1_650_000
        // effective, 3_300_000 base at price 2.0. 1_000_000 d-tokens is
        // 1_100_000 underlying, 1_466_667 effective, 2_933_334 base.
        let data = calculate_position_data(&one_reserve(), &prices(), &position(&[(0, 2_000_000, 1_000_000)])).expect("values");
        assert_eq!(data.collateral_base, 3_300_000);
        assert_eq!(data.collateral_raw, 4_400_000);
        assert_eq!(data.liability_base, 2_933_334);
        assert_eq!(data.liability_raw, 2_200_000);
        assert_eq!(data.scalar, SCALAR_7);
        assert_eq!(data.health_factor(), Ok(Some(11_249_997)));
    }

    #[test]
    fn sums_across_reserves() {
        let positions = position(&[(0, 2_000_000, 1_000_000), (1, 2_000_000, 1_000_000)]);
        let data = calculate_position_data(&two_reserves(), &prices(), &positions).expect("values");
        assert_eq!(data.collateral_base, 6_600_000);
        assert_eq!(data.collateral_raw, 8_800_000);
        assert_eq!(data.liability_base, 5_866_668);
        assert_eq!(data.liability_raw, 4_400_000);
        // Doubling both sides leaves the ratio, and so the floor, unchanged.
        assert_eq!(data.health_factor(), Ok(Some(11_249_997)));
    }

    #[test]
    fn a_reserve_with_no_position_contributes_nothing() {
        let positions = position(&[(0, 2_000_000, 1_000_000)]);
        let one = calculate_position_data(&one_reserve(), &prices(), &positions).expect("values");
        let two = calculate_position_data(&two_reserves(), &prices(), &positions).expect("values");
        assert_eq!(one, two);
    }

    #[test]
    fn health_factor_is_absent_without_liabilities() {
        let data = calculate_position_data(&one_reserve(), &prices(), &position(&[(0, 2_000_000, 0)])).expect("values");
        assert_eq!(data.liability_base, 0);
        assert_eq!(data.health_factor(), Ok(None));
        // The contract treats no liabilities as over any maximum and under no
        // minimum, and so does this port.
        assert_eq!(data.is_hf_over(11_500_000), Ok(true));
        assert_eq!(data.is_hf_under(10_300_000), Ok(false));
    }

    #[test]
    fn hf_bounds_scale_a_7_decimal_threshold_to_the_oracle_scalar() {
        // Health factor 1.1249997 against the contract's own bounds.
        let data = calculate_position_data(&one_reserve(), &prices(), &position(&[(0, 2_000_000, 1_000_000)])).expect("values");
        assert_eq!(data.is_hf_over(11_500_000), Ok(false));
        assert_eq!(data.is_hf_over(11_000_000), Ok(true));
        assert_eq!(data.is_hf_under(11_500_000), Ok(true));
        assert_eq!(data.is_hf_under(10_300_000), Ok(false));
    }

    #[test]
    fn a_position_in_an_unknown_reserve_is_an_error() {
        let positions = position(&[(7, 2_000_000, 0)]);
        assert_eq!(calculate_position_data(&one_reserve(), &prices(), &positions), Err(MathError::MissingReserve(7)));
    }

    #[test]
    fn a_reserve_without_a_price_is_an_error() {
        let mut map = BTreeMap::new();
        map.insert(ASSET_B.to_string(), 20_000_000);
        let prices = OraclePrices::new(7, map).expect("scalar");
        let positions = position(&[(0, 2_000_000, 0)]);
        assert_eq!(
            calculate_position_data(&one_reserve(), &prices, &positions),
            Err(MathError::MissingPrice(ASSET_A.to_string()))
        );
    }

    #[test]
    fn effective_count_ignores_uncollateralised_supply() {
        let mut positions = position(&[(0, 2_000_000, 1_000_000)]);
        positions.supply.insert(1, 5_000_000);
        assert_eq!(positions.effective_count(), 2);
        assert!(!positions.is_empty());
        assert!(Positions::default().is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib math::position`
Expected: panics on `todo!()`.

- [ ] **Step 3: Implement**

```rust
//! Position valuation and the health factor, ported from the contract's
//! `PositionData`.
//!
//! Collateral rounds down and liabilities round up at every step, because
//! that is the direction the contract rounds and the bot must never believe
//! a position is healthier than the contract will find it. All base values
//! are in the oracle's decimals; the health factor is a ratio in that same
//! scale, so `1.0` is `10^oracle_decimals`.

use std::collections::BTreeMap;

use super::fixed::{div_floor, mul_ceil, mul_floor, pow10, MathError, SCALAR_7};
use super::reserve::Reserve;

/// A user's pool positions, keyed by reserve index, in b-tokens (collateral
/// and supply) and d-tokens (liabilities).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Positions {
    /// Collateralised supply, in b-tokens.
    pub collateral: BTreeMap<u32, i128>,
    /// Borrowings, in d-tokens.
    pub liabilities: BTreeMap<u32, i128>,
    /// Supply that is not collateral and does not affect the health factor.
    pub supply: BTreeMap<u32, i128>,
}

impl Positions {
    /// Positions that count against the pool's `max_positions`: collateral
    /// and liabilities, never plain supply.
    pub fn effective_count(&self) -> usize {
        self.collateral.len() + self.liabilities.len()
    }

    /// True when the user holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.collateral.is_empty() && self.liabilities.is_empty() && self.supply.is_empty()
    }
}

/// A snapshot of one pool oracle: its decimals and a price per asset, both
/// as the contract sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OraclePrices {
    /// The oracle's `decimals()`.
    pub decimals: u32,
    /// `10^decimals`, the scale every base value is expressed in.
    pub scalar: i128,
    /// Asset contract address to price.
    pub prices: BTreeMap<String, i128>,
}

impl OraclePrices {
    /// Builds a snapshot, deriving the scalar from the oracle's decimals.
    pub fn new(decimals: u32, prices: BTreeMap<String, i128>) -> Result<Self, MathError> {
        let scalar = pow10(decimals)?;
        Ok(Self { decimals, scalar, prices })
    }

    /// The price of `asset`, or `MissingPrice` when the snapshot has none.
    /// A missing price is never a zero: valuing a position at zero would
    /// make it look liquidatable.
    pub fn price(&self, asset: &str) -> Result<i128, MathError> {
        self.prices.get(asset).copied().ok_or_else(|| MathError::MissingPrice(asset.to_string()))
    }
}

/// A position valued in the oracle's base asset, effective (factor-adjusted)
/// and raw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionData {
    /// Collateral after collateral factors.
    pub collateral_base: i128,
    /// Collateral before collateral factors.
    pub collateral_raw: i128,
    /// Liabilities after liability factors.
    pub liability_base: i128,
    /// Liabilities before liability factors.
    pub liability_raw: i128,
    /// The oracle scalar these values are expressed in.
    pub scalar: i128,
}

impl PositionData {
    /// `collateral_base / liability_base` in the oracle's scale, or `None`
    /// when there are no liabilities — the contract divides only after
    /// guarding that case, and an `Option` makes the guard unforgettable.
    pub fn health_factor(&self) -> Result<Option<i128>, MathError> {
        if self.liability_base == 0 {
            return Ok(None);
        }
        div_floor(self.collateral_base, self.liability_base, self.scalar).map(Some)
    }

    /// Whether the health factor exceeds `max` (7 decimals). No liabilities
    /// counts as over any bound, matching `PositionData::is_hf_over`.
    pub fn is_hf_over(&self, max: i128) -> Result<bool, MathError> {
        match self.health_factor()? {
            None => Ok(true),
            Some(health_factor) => Ok(health_factor > mul_ceil(self.scalar, max, SCALAR_7)?),
        }
    }

    /// Whether the health factor is below `min` (7 decimals). No liabilities
    /// counts as under nothing, matching `PositionData::is_hf_under`.
    pub fn is_hf_under(&self, min: i128) -> Result<bool, MathError> {
        match self.health_factor()? {
            None => Ok(false),
            Some(health_factor) => Ok(health_factor < mul_floor(self.scalar, min, SCALAR_7)?),
        }
    }
}

/// Values `positions` against `reserves` (keyed by reserve index, already
/// accrued to the decision's ledger) and `prices`.
///
/// A position in an index the pool does not have, or in an asset the oracle
/// snapshot does not price, is an error rather than a zero: both mean the
/// caller's view of the pool is incomplete, and a zero would silently
/// understate a position.
pub fn calculate_position_data(
    reserves: &BTreeMap<u32, Reserve>,
    prices: &OraclePrices,
    positions: &Positions,
) -> Result<PositionData, MathError> {
    let mut data = PositionData {
        collateral_base: 0,
        collateral_raw: 0,
        liability_base: 0,
        liability_raw: 0,
        scalar: prices.scalar,
    };

    for (index, b_tokens) in &positions.collateral {
        if *b_tokens == 0 {
            continue;
        }
        let reserve = reserves.get(index).ok_or(MathError::MissingReserve(*index))?;
        let price = prices.price(&reserve.asset)?;
        let effective = reserve.to_effective_asset_from_b_token(*b_tokens)?;
        let raw = reserve.to_asset_from_b_token(*b_tokens)?;
        data.collateral_base = data
            .collateral_base
            .checked_add(mul_floor(price, effective, reserve.scalar)?)
            .ok_or(MathError::Overflow)?;
        data.collateral_raw = data
            .collateral_raw
            .checked_add(mul_floor(price, raw, reserve.scalar)?)
            .ok_or(MathError::Overflow)?;
    }

    for (index, d_tokens) in &positions.liabilities {
        if *d_tokens == 0 {
            continue;
        }
        let reserve = reserves.get(index).ok_or(MathError::MissingReserve(*index))?;
        let price = prices.price(&reserve.asset)?;
        let effective = reserve.to_effective_asset_from_d_token(*d_tokens)?;
        let raw = reserve.to_asset_from_d_token(*d_tokens)?;
        data.liability_base = data
            .liability_base
            .checked_add(mul_ceil(price, effective, reserve.scalar)?)
            .ok_or(MathError::Overflow)?;
        data.liability_raw = data
            .liability_raw
            .checked_add(mul_ceil(price, raw, reserve.scalar)?)
            .ok_or(MathError::Overflow)?;
    }

    Ok(data)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib math::position`
Expected: `8 passed`.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt --all
git add src/math/position.rs src/math/mod.rs
git commit -m "feat(math): position valuation and health factor

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: `math::auction`

**Files:**
- Create: `src/math/auction.rs`
- Modify: `src/math/mod.rs` (re-exports)

**Interfaces:**
- Consumes: `fixed::{mul_floor, mul_ceil, MathError, SCALAR_7}`
- Produces:
  - `pub struct AuctionData { pub bid: BTreeMap<String, i128>, pub lot: BTreeMap<String, i128>, pub block: u32 }` with `is_empty(&self) -> bool`
  - `pub struct ScaledAuction { pub to_fill: AuctionData, pub remaining: Option<AuctionData> }`
  - `pub fn scale_auction(auction: &AuctionData, fill_block: u32, percent_filled: u32) -> Result<ScaledAuction, MathError>`
  - `pub fn bid_modifier(block_delta: u32) -> i128`, `pub fn lot_modifier(block_delta: u32) -> i128` (7 decimals, public because the filler's fill-block search in Phase 5 uses them directly)

- [ ] **Step 1: Write the failing tests**

Create `src/math/auction.rs` with stubs and this test module. The vectors are the contract's own `scale_auction` unit-test vectors, which the v1 bot also ships, so a port that passes agrees with both.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const ASSET_1: &str = "asset_1";
    const ASSET_2: &str = "asset_2";
    const ASSET_3: &str = "asset_3";

    fn auction() -> AuctionData {
        AuctionData {
            bid: BTreeMap::from([
                (ASSET_1.to_string(), 1_000_000_000),
                (ASSET_2.to_string(), 2_000_000_001),
            ]),
            lot: BTreeMap::from([
                (ASSET_2.to_string(), 10_000_000),
                (ASSET_3.to_string(), 50_000_001),
            ]),
            block: 100,
        }
    }

    fn one_stroop_auction() -> AuctionData {
        AuctionData {
            bid: BTreeMap::from([(ASSET_1.to_string(), 1)]),
            lot: BTreeMap::from([(ASSET_2.to_string(), 1)]),
            block: 100,
        }
    }

    #[test]
    fn modifiers_ramp_the_lot_then_the_bid() {
        assert_eq!((lot_modifier(0), bid_modifier(0)), (0, SCALAR_7));
        assert_eq!((lot_modifier(100), bid_modifier(100)), (5_000_000, SCALAR_7));
        assert_eq!((lot_modifier(200), bid_modifier(200)), (SCALAR_7, SCALAR_7));
        assert_eq!((lot_modifier(300), bid_modifier(300)), (SCALAR_7, 5_000_000));
        assert_eq!((lot_modifier(399), bid_modifier(399)), (SCALAR_7, 50_000));
        assert_eq!((lot_modifier(400), bid_modifier(400)), (SCALAR_7, 0));
        assert_eq!((lot_modifier(4_000), bid_modifier(4_000)), (SCALAR_7, 0));
    }

    #[test]
    fn at_the_start_block_the_filler_pays_everything_and_receives_nothing() {
        let scaled = scale_auction(&auction(), 100, 100).expect("scales");
        assert_eq!(scaled.to_fill.block, 100);
        assert_eq!(scaled.to_fill.bid[ASSET_1], 1_000_000_000);
        assert_eq!(scaled.to_fill.bid[ASSET_2], 2_000_000_001);
        assert!(scaled.to_fill.lot.is_empty());
        assert_eq!(scaled.remaining, None);
    }

    #[test]
    fn halfway_through_the_lot_ramp_the_lot_rounds_down() {
        let scaled = scale_auction(&auction(), 200, 100).expect("scales");
        assert_eq!(scaled.to_fill.bid[ASSET_1], 1_000_000_000);
        assert_eq!(scaled.to_fill.bid[ASSET_2], 2_000_000_001);
        assert_eq!(scaled.to_fill.lot[ASSET_2], 5_000_000);
        assert_eq!(scaled.to_fill.lot[ASSET_3], 25_000_000);
        assert_eq!(scaled.remaining, None);
    }

    #[test]
    fn a_partial_fill_rounds_the_bid_up_and_leaves_the_rest() {
        let scaled = scale_auction(&auction(), 200, 50).expect("scales");
        assert_eq!(scaled.to_fill.bid[ASSET_1], 500_000_000);
        assert_eq!(scaled.to_fill.bid[ASSET_2], 1_000_000_001);
        assert_eq!(scaled.to_fill.lot[ASSET_2], 2_500_000);
        assert_eq!(scaled.to_fill.lot[ASSET_3], 12_500_000);
        let remaining = scaled.remaining.expect("half remains");
        assert_eq!(remaining.block, 100);
        assert_eq!(remaining.bid[ASSET_1], 500_000_000);
        assert_eq!(remaining.bid[ASSET_2], 1_000_000_000);
        assert_eq!(remaining.lot[ASSET_2], 5_000_000);
        assert_eq!(remaining.lot[ASSET_3], 25_000_001);
    }

    #[test]
    fn at_block_200_the_whole_auction_changes_hands() {
        let scaled = scale_auction(&auction(), 300, 100).expect("scales");
        assert_eq!(scaled.to_fill.bid[ASSET_1], 1_000_000_000);
        assert_eq!(scaled.to_fill.bid[ASSET_2], 2_000_000_001);
        assert_eq!(scaled.to_fill.lot[ASSET_2], 10_000_000);
        assert_eq!(scaled.to_fill.lot[ASSET_3], 50_000_001);
    }

    #[test]
    fn past_block_200_the_bid_decays_and_then_vanishes() {
        let half = scale_auction(&auction(), 400, 100).expect("scales");
        assert_eq!(half.to_fill.bid[ASSET_1], 500_000_000);
        assert_eq!(half.to_fill.bid[ASSET_2], 1_000_000_001);
        assert_eq!(half.to_fill.lot[ASSET_3], 50_000_001);

        let free = scale_auction(&auction(), 500, 100).expect("scales");
        assert!(free.to_fill.bid.is_empty());
        assert_eq!(free.to_fill.lot[ASSET_2], 10_000_000);
        assert_eq!(free.to_fill.lot[ASSET_3], 50_000_001);

        let still_free = scale_auction(&auction(), 600, 100).expect("scales");
        assert_eq!(still_free.to_fill, free.to_fill);
    }

    #[test]
    fn one_stroop_rounds_the_bid_up_and_the_lot_away() {
        let early = scale_auction(&one_stroop_auction(), 101, 10).expect("scales");
        assert_eq!(early.to_fill.bid[ASSET_1], 1);
        assert!(early.to_fill.lot.is_empty());
        // The whole stroop of bid was taken, so only the lot remains.
        let remaining = early.remaining.expect("lot remains");
        assert!(remaining.bid.is_empty());
        assert_eq!(remaining.lot[ASSET_2], 1);

        let late = scale_auction(&one_stroop_auction(), 499, 100).expect("scales");
        assert_eq!(late.to_fill.bid[ASSET_1], 1);
        assert_eq!(late.to_fill.lot[ASSET_2], 1);
        assert_eq!(late.remaining, None);
    }

    #[test]
    fn a_percent_outside_one_to_a_hundred_is_an_error() {
        assert_eq!(scale_auction(&auction(), 200, 0), Err(MathError::InvalidInput("percent_filled must be 1..=100")));
        assert_eq!(scale_auction(&auction(), 200, 101), Err(MathError::InvalidInput("percent_filled must be 1..=100")));
    }

    #[test]
    fn a_fill_block_before_the_auction_started_is_an_error() {
        assert_eq!(scale_auction(&auction(), 99, 100), Err(MathError::InvalidInput("fill_block precedes the auction block")));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib math::auction`
Expected: panics on `todo!()`.

- [ ] **Step 3: Implement**

```rust
//! Dutch-auction scaling, ported from the contract's `scale_auction`.
//!
//! An auction runs for 400 blocks from its start. Over the first 200 the
//! filler receives a growing share of the lot for the whole bid; over the
//! next 200 it receives the whole lot for a shrinking bid; after 400 the bid
//! is nothing. The modifier moves 0.5% per block. Bids round up and lots
//! round down, so a rounding error can only cost the filler, never the pool.

use std::collections::BTreeMap;

use super::fixed::{mul_ceil, mul_floor, MathError, SCALAR_7};

/// Half a percent at 7 decimals: the per-block step of both ramps.
const PER_BLOCK_SCALAR: i128 = 50_000;
/// The block at which the lot ramp ends and the bid ramp begins.
const RAMP_BLOCKS: u32 = 200;

/// An auction as the contract stores it: what the filler pays (`bid`), what
/// it receives (`lot`), and the block the auction began on. Amounts are
/// d-tokens, b-tokens or underlying depending on the auction type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuctionData {
    /// Asset address to amount the filler pays.
    pub bid: BTreeMap<String, i128>,
    /// Asset address to amount the filler receives.
    pub lot: BTreeMap<String, i128>,
    /// The block the auction started on.
    pub block: u32,
}

impl AuctionData {
    /// True when neither side has anything left.
    pub fn is_empty(&self) -> bool {
        self.bid.is_empty() && self.lot.is_empty()
    }
}

/// The result of scaling: what a fill at this block and percent exchanges,
/// and what would be left on the ledger afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaledAuction {
    /// The amounts this fill exchanges.
    pub to_fill: AuctionData,
    /// What remains after a partial fill; `None` means the fill closes it.
    pub remaining: Option<AuctionData>,
}

/// The bid modifier at `block_delta` blocks after the auction started,
/// at 7 decimals: 100% for the first 200 blocks, then down to 0% at 400.
pub fn bid_modifier(block_delta: u32) -> i128 {
    if block_delta <= RAMP_BLOCKS {
        SCALAR_7
    } else if block_delta < 2 * RAMP_BLOCKS {
        SCALAR_7 - i128::from(block_delta - RAMP_BLOCKS) * PER_BLOCK_SCALAR
    } else {
        0
    }
}

/// The lot modifier at `block_delta` blocks after the auction started, at 7
/// decimals: 0% rising to 100% over the first 200 blocks, then 100%.
pub fn lot_modifier(block_delta: u32) -> i128 {
    if block_delta <= RAMP_BLOCKS {
        i128::from(block_delta) * PER_BLOCK_SCALAR
    } else {
        SCALAR_7
    }
}

/// Scales `auction` for a fill of `percent_filled` percent at `fill_block`.
///
/// `percent_filled` is a whole percentage in `1..=100`, as the contract's
/// fill request takes it. `fill_block` must be at or after the auction's
/// start block.
pub fn scale_auction(auction: &AuctionData, fill_block: u32, percent_filled: u32) -> Result<ScaledAuction, MathError> {
    if percent_filled == 0 || percent_filled > 100 {
        return Err(MathError::InvalidInput("percent_filled must be 1..=100"));
    }
    let block_delta = fill_block
        .checked_sub(auction.block)
        .ok_or(MathError::InvalidInput("fill_block precedes the auction block"))?;

    let bid_scale = bid_modifier(block_delta);
    let lot_scale = lot_modifier(block_delta);
    // 100 percent is one whole, so a percentage scales to 7 decimals by 10^5.
    let percent_scaled = i128::from(percent_filled) * 100_000;

    let mut to_fill = AuctionData { block: auction.block, ..AuctionData::default() };
    let mut remaining = AuctionData { block: auction.block, ..AuctionData::default() };

    for (asset, amount) in &auction.bid {
        let to_fill_base = mul_ceil(*amount, percent_scaled, SCALAR_7)?;
        let remaining_base = amount.checked_sub(to_fill_base).ok_or(MathError::Overflow)?;
        if remaining_base > 0 {
            remaining.bid.insert(asset.clone(), remaining_base);
        }
        let scaled = mul_ceil(to_fill_base, bid_scale, SCALAR_7)?;
        if scaled > 0 {
            to_fill.bid.insert(asset.clone(), scaled);
        }
    }

    for (asset, amount) in &auction.lot {
        let to_fill_base = mul_floor(*amount, percent_scaled, SCALAR_7)?;
        let remaining_base = amount.checked_sub(to_fill_base).ok_or(MathError::Overflow)?;
        if remaining_base > 0 {
            remaining.lot.insert(asset.clone(), remaining_base);
        }
        let scaled = mul_floor(to_fill_base, lot_scale, SCALAR_7)?;
        if scaled > 0 {
            to_fill.lot.insert(asset.clone(), scaled);
        }
    }

    let remaining = if remaining.is_empty() { None } else { Some(remaining) };
    Ok(ScaledAuction { to_fill, remaining })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib math::auction`
Expected: `9 passed`.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt --all
git add src/math/auction.rs src/math/mod.rs
git commit -m "feat(math): Dutch-auction scaling

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: `chain::xdr::encode` and `chain::xdr::keys`

**Files:**
- Create: `src/chain/xdr/encode.rs`, `src/chain/xdr/keys.rs`
- Modify: `src/chain/xdr/mod.rs` (re-exports)

**Interfaces:**
- Consumes: `XdrError`
- Produces, in `encode`:
  - `pub fn symbol(text: &str) -> Result<ScVal, XdrError>`
  - `pub fn sc_address(strkey: &str) -> Result<ScAddress, XdrError>`
  - `pub fn address(strkey: &str) -> Result<ScVal, XdrError>`
  - `pub fn vec(items: Vec<ScVal>) -> Result<ScVal, XdrError>`
  - `pub fn map(entries: Vec<(ScVal, ScVal)>) -> Result<ScVal, XdrError>` (sorted, as the host requires)
  - `pub fn i128_val(value: i128) -> ScVal`
  - `pub fn stellar_asset(asset: &str) -> Result<ScVal, XdrError>` — the SEP-40 `Asset::Stellar(address)`
  - `pub fn invoke_contract_op(contract: &str, function: &str, args: Vec<ScVal>) -> Result<Operation, XdrError>`
  - `pub fn simulation_envelope(operation: Operation) -> Result<TransactionEnvelope, XdrError>`
  - `pub fn to_base64<T: WriteXdr>(value: &T) -> Result<String, XdrError>`
  - `pub fn from_base64<T: ReadXdr>(text: &str) -> Result<T, XdrError>`
- Produces, in `keys`:
  - `pub fn instance(pool: &str) -> Result<LedgerKey, XdrError>`
  - `pub fn reserve_list(pool: &str) -> Result<LedgerKey, XdrError>`
  - `pub fn reserve_config(pool: &str, asset: &str) -> Result<LedgerKey, XdrError>`
  - `pub fn reserve_data(pool: &str, asset: &str) -> Result<LedgerKey, XdrError>`
  - `pub fn positions(pool: &str, user: &str) -> Result<LedgerKey, XdrError>`
  - `pub fn auction(pool: &str, user: &str, auction_type: u32) -> Result<LedgerKey, XdrError>`

- [ ] **Step 1: Write the failing tests**

Create both files with stubs, and put this test module in `keys.rs`. Every base64 string was produced by these key shapes against the fixture's pool and verified to return the expected entries from mainnet, so a change in any of them is a wire-format regression.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::xdr::encode::to_base64;
    use stellar_xdr::{ContractDataDurability, LedgerKey, Limits, ReadXdr};

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";

    #[test]
    fn instance_key_matches_the_wire_format() {
        let key = to_base64(&instance(POOL).expect("key")).expect("base64");
        assert_eq!(key, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABQAAAAB");
    }

    #[test]
    fn reserve_list_key_matches_the_wire_format() {
        let key = to_base64(&reserve_list(POOL).expect("key")).expect("base64");
        assert_eq!(key, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAAA8AAAAHUmVzTGlzdAAAAAAB");
    }

    #[test]
    fn reserve_keys_match_the_wire_format() {
        let config = to_base64(&reserve_config(POOL, XLM).expect("key")).expect("base64");
        assert_eq!(config, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABAAAAABAAAAAgAAAA8AAAAJUmVzQ29uZmlnAAAAAAAAEgAAAAEltPzYWa7C+mNIQ4xImzw8EMmLbSG+T9PLMMtolT75dwAAAAE=");
        let data = to_base64(&reserve_data(POOL, XLM).expect("key")).expect("base64");
        assert_eq!(data, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABAAAAABAAAAAgAAAA8AAAAHUmVzRGF0YQAAAAASAAAAASW0/NhZrsL6Y0hDjEibPDwQyYttIb5P08swy2iVPvl3AAAAAQ==");
    }

    #[test]
    fn positions_key_matches_the_wire_format() {
        let key = to_base64(&positions(POOL, USER).expect("key")).expect("base64");
        assert_eq!(key, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABAAAAABAAAAAgAAAA8AAAAJUG9zaXRpb25zAAAAAAAAEgAAAAAAAAAAwWvxVekgt/bc4cnQA7BvYRCl1YIAjrruP0ttEbPr7t0AAAAB");
    }

    #[test]
    fn auction_key_matches_the_wire_format() {
        let key = to_base64(&auction(POOL, USER, 0).expect("key")).expect("base64");
        assert_eq!(key, "AAAABgAAAAESnMjMYzbx/bvcwPOYNDTDzbhf2eqFaXo3gtMY2HSlgAAAABAAAAABAAAAAgAAAA8AAAAHQXVjdGlvbgAAAAARAAAAAQAAAAIAAAAPAAAACWF1Y3RfdHlwZQAAAAAAAAMAAAAAAAAADwAAAAR1c2VyAAAAEgAAAAAAAAAAwWvxVekgt/bc4cnQA7BvYRCl1YIAjrruP0ttEbPr7t0AAAAA");
    }

    #[test]
    fn state_is_persistent_and_auctions_are_temporary() {
        // The durability is part of the key: asking for an auction in
        // persistent storage silently finds nothing.
        for key in [instance(POOL), reserve_list(POOL), reserve_config(POOL, XLM), reserve_data(POOL, XLM), positions(POOL, USER)] {
            match key.expect("key") {
                LedgerKey::ContractData(data) => assert_eq!(data.durability, ContractDataDurability::Persistent),
                other => panic!("expected contract data, got {other:?}"),
            }
        }
        match auction(POOL, USER, 0).expect("key") {
            LedgerKey::ContractData(data) => assert_eq!(data.durability, ContractDataDurability::Temporary),
            other => panic!("expected contract data, got {other:?}"),
        }
    }

    #[test]
    fn a_key_round_trips_through_base64() {
        let key = positions(POOL, USER).expect("key");
        let text = to_base64(&key).expect("base64");
        let parsed = LedgerKey::from_xdr_base64(&text, Limits::none()).expect("parses");
        assert_eq!(parsed, key);
    }

    #[test]
    fn a_bad_pool_address_is_an_error_not_a_panic() {
        assert!(matches!(instance("not-an-address"), Err(crate::chain::xdr::XdrError::Xdr(_))));
    }
}
```

Put this test module in `encode.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";

    #[test]
    fn symbols_match_the_wire_format() {
        assert_eq!(to_base64(&symbol("new_auction").expect("symbol")).expect("base64"), "AAAADwAAAAtuZXdfYXVjdGlvbgA=");
    }

    #[test]
    fn a_symbol_over_32_bytes_is_an_error() {
        let long = "a".repeat(33);
        assert!(matches!(symbol(&long), Err(XdrError::Symbol(_))));
        assert!(symbol(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn a_stellar_asset_is_a_two_element_enum_vector() {
        let encoded = to_base64(&stellar_asset(XLM).expect("asset")).expect("base64");
        assert_eq!(encoded, "AAAAEAAAAAEAAAACAAAADwAAAAdTdGVsbGFyAAAAABIAAAABJbT82FmuwvpjSEOMSJs8PBDJi20hvk/TyzDLaJU++Xc=");
    }

    #[test]
    fn i128_values_round_trip_through_their_high_and_low_halves() {
        for value in [0_i128, 1, -1, i128::MAX, i128::MIN, 1_228_743_739_744] {
            let encoded = to_base64(&i128_val(value)).expect("base64");
            let decoded: ScVal = from_base64(&encoded).expect("parses");
            match decoded {
                ScVal::I128(parts) => assert_eq!((i128::from(parts.hi) << 64) | i128::from(parts.lo), value),
                other => panic!("expected i128, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_map_is_sorted_by_key_as_the_host_requires() {
        let unsorted = map(vec![
            (symbol("user").expect("symbol"), ScVal::U32(1)),
            (symbol("auct_type").expect("symbol"), ScVal::U32(0)),
        ])
        .expect("map");
        match unsorted {
            ScVal::Map(Some(entries)) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].key, symbol("auct_type").expect("symbol"));
                assert_eq!(entries[1].key, symbol("user").expect("symbol"));
            }
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn a_simulation_envelope_carries_one_invoke_operation() {
        let operation = invoke_contract_op(POOL, "get_reserve", vec![address(XLM).expect("address")]).expect("op");
        let envelope = simulation_envelope(operation).expect("envelope");
        let text = to_base64(&envelope).expect("base64");
        let parsed: TransactionEnvelope = from_base64(&text).expect("parses");
        let TransactionEnvelope::Tx(v1) = parsed else { panic!("expected a v1 envelope") };
        assert_eq!(v1.tx.operations.len(), 1);
        assert!(v1.signatures.is_empty(), "a simulation is never signed");
        let OperationBody::InvokeHostFunction(invoke) = &v1.tx.operations[0].body else {
            panic!("expected an invoke-host-function operation")
        };
        let HostFunction::InvokeContract(args) = &invoke.host_function else {
            panic!("expected a contract invocation")
        };
        assert_eq!(args.function_name, symbol_name("get_reserve"));
        assert_eq!(args.args.len(), 1);
    }

    /// The bare `ScSymbol` for a name, for comparing against a decoded call.
    fn symbol_name(text: &str) -> ScSymbol {
        match symbol(text).expect("symbol") {
            ScVal::Symbol(name) => name,
            other => panic!("expected a symbol, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib chain::xdr`
Expected: panics on `todo!()`.

- [ ] **Step 3: Implement `encode.rs`**

```rust
//! Building the ScVals, operations and envelopes the bot sends.
//!
//! Two rules the host enforces and this module keeps: a contract map must be
//! sorted by key, and a `Symbol` is at most 32 bytes. Both are errors here
//! rather than surprises at simulation time.

use stellar_xdr::{
    HostFunction, InvokeContractArgs, InvokeHostFunctionOp, Limits, Memo,
    MuxedAccount, Operation, OperationBody, Preconditions, ReadXdr, ScAddress, ScMap, ScSymbol,
    ScVal, ScVec, SequenceNumber, Transaction, TransactionEnvelope, TransactionExt,
    TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

use super::XdrError;

/// A contract `Symbol`, at most 32 bytes.
pub fn symbol(text: &str) -> Result<ScVal, XdrError> {
    ScSymbol::try_from(text)
        .map(ScVal::Symbol)
        .map_err(|()| XdrError::Symbol(text.to_string()))
}

/// A strkey (`C…` contract or `G…` account) as an `ScAddress`.
pub fn sc_address(strkey: &str) -> Result<ScAddress, XdrError> {
    strkey.parse().map_err(XdrError::Xdr)
}

/// A strkey as an `ScVal::Address`.
pub fn address(strkey: &str) -> Result<ScVal, XdrError> {
    Ok(ScVal::Address(sc_address(strkey)?))
}

/// A contract vector.
pub fn vec(items: Vec<ScVal>) -> Result<ScVal, XdrError> {
    Ok(ScVal::Vec(Some(ScVec::try_from(items)?)))
}

/// A contract map, sorted by key as the host requires.
pub fn map(entries: Vec<(ScVal, ScVal)>) -> Result<ScVal, XdrError> {
    Ok(ScVal::Map(Some(ScMap::sorted_from(entries)?)))
}

/// An `i128` as an `ScVal`. The XDR carries it as a signed high half and an
/// unsigned low half; `stellar-xdr` owns that split, so this is a named
/// wrapper rather than hand-rolled bit twiddling.
pub fn i128_val(value: i128) -> ScVal {
    ScVal::from(value)
}

/// The SEP-40 `Asset::Stellar(address)` an oracle's `lastprice` takes.
pub fn stellar_asset(asset: &str) -> Result<ScVal, XdrError> {
    vec(vec![symbol("Stellar")?, address(asset)?])
}

/// One `InvokeHostFunction` operation calling `function` on `contract`.
/// Authorisation is empty: simulation fills it in, and the bot's own calls
/// are covered by the source account's signature.
pub fn invoke_contract_op(contract: &str, function: &str, args: Vec<ScVal>) -> Result<Operation, XdrError> {
    let ScVal::Symbol(function_name) = symbol(function)? else {
        return Err(XdrError::Symbol(function.to_string()));
    };
    Ok(Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: sc_address(contract)?,
                function_name,
                args: VecM::try_from(args)?,
            }),
            auth: VecM::default(),
        }),
    })
}

/// Wraps an operation in an unsigned envelope for `simulateTransaction`.
///
/// The source account is all zeroes and the sequence number is zero: the RPC
/// does not validate either when simulating, and using a real account would
/// make every read depend on that account existing and being funded.
pub fn simulation_envelope(operation: Operation) -> Result<TransactionEnvelope, XdrError> {
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256([0_u8; 32])),
        fee: 100,
        seq_num: SequenceNumber(0),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: VecM::try_from(vec![operation])?,
        ext: TransactionExt::V0,
    };
    Ok(TransactionEnvelope::Tx(TransactionV1Envelope { tx, signatures: VecM::default() }))
}

/// Base64 for the wire, with no depth or length limits imposed by us.
pub fn to_base64<T: WriteXdr>(value: &T) -> Result<String, XdrError> {
    value.to_xdr_base64(Limits::none()).map_err(XdrError::Xdr)
}

/// The inverse of `to_base64`.
pub fn from_base64<T: ReadXdr>(text: &str) -> Result<T, XdrError> {
    T::from_xdr_base64(text, Limits::none()).map_err(XdrError::Xdr)
}
```

- [ ] **Step 4: Implement `keys.rs`**

```rust
//! Ledger keys for the Blend v2 pool's storage.
//!
//! Durability is part of the key: pool configuration, reserves and positions
//! live in persistent storage, auctions in temporary storage. Reading an
//! auction with the persistent durability returns nothing rather than an
//! error, which is why these are constructed in one place and pinned by
//! wire-format tests.

use stellar_xdr::{ContractDataDurability, LedgerKey, LedgerKeyContractData, ScVal};

use super::encode::{address, map, sc_address, symbol, vec};
use super::XdrError;

fn contract_data(pool: &str, key: ScVal, durability: ContractDataDurability) -> Result<LedgerKey, XdrError> {
    Ok(LedgerKey::ContractData(LedgerKeyContractData {
        contract: sc_address(pool)?,
        key,
        durability,
    }))
}

/// The pool's contract instance: admin, backstop, BLND token, name, config.
pub fn instance(pool: &str) -> Result<LedgerKey, XdrError> {
    contract_data(pool, ScVal::LedgerKeyContractInstance, ContractDataDurability::Persistent)
}

/// `ResList`: the reserve addresses, in the index order positions use.
pub fn reserve_list(pool: &str) -> Result<LedgerKey, XdrError> {
    contract_data(pool, symbol("ResList")?, ContractDataDurability::Persistent)
}

/// `ResConfig(asset)`: the reserve's factors and rate curve.
pub fn reserve_config(pool: &str, asset: &str) -> Result<LedgerKey, XdrError> {
    let key = vec(vec![symbol("ResConfig")?, address(asset)?])?;
    contract_data(pool, key, ContractDataDurability::Persistent)
}

/// `ResData(asset)`: the reserve's rates and supplies as of its last update.
pub fn reserve_data(pool: &str, asset: &str) -> Result<LedgerKey, XdrError> {
    let key = vec(vec![symbol("ResData")?, address(asset)?])?;
    contract_data(pool, key, ContractDataDurability::Persistent)
}

/// `Positions(user)`: one user's collateral, liabilities and supply.
pub fn positions(pool: &str, user: &str) -> Result<LedgerKey, XdrError> {
    let key = vec(vec![symbol("Positions")?, address(user)?])?;
    contract_data(pool, key, ContractDataDurability::Persistent)
}

/// `Auction(AuctionKey { user, auct_type })`, in temporary storage.
/// `auction_type` is 0 for a user liquidation, 1 for bad debt, 2 for interest.
pub fn auction(pool: &str, user: &str, auction_type: u32) -> Result<LedgerKey, XdrError> {
    let auction_key = map(vec![
        (symbol("auct_type")?, ScVal::U32(auction_type)),
        (symbol("user")?, address(user)?),
    ])?;
    let key = vec(vec![symbol("Auction")?, auction_key])?;
    contract_data(pool, key, ContractDataDurability::Temporary)
}
```

Add `pub use encode::{address, from_base64, i128_val, invoke_contract_op, map, sc_address, simulation_envelope, stellar_asset, symbol, to_base64, vec};` to `src/chain/xdr/mod.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib chain::xdr`
Expected: `14 passed` (8 key tests, 6 encode tests).

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt --all
git add src/chain/xdr/encode.rs src/chain/xdr/keys.rs src/chain/xdr/mod.rs
git commit -m "feat(chain): ScVal encoders and pool ledger keys

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: `chain::xdr::decode` and the fixture cross-check

**Files:**
- Create: `src/chain/xdr/decode.rs`
- Modify: `src/chain/xdr/mod.rs` (re-exports)

**Interfaces:**
- Consumes: `encode::from_base64`, `math::{AuctionData, OraclePrices, Positions, Reserve, ReserveConfig, ReserveData}`, `crate::fixture` (tests)
- Produces:
  - `pub struct PoolConfig { pub oracle: String, pub bstop_rate: u32, pub status: u32, pub max_positions: u32, pub min_collateral: i128 }`
  - `pub struct PoolInstance { pub admin: String, pub backstop: String, pub blnd_token: String, pub name: String, pub config: PoolConfig }`
  - `pub struct PriceData { pub price: i128, pub timestamp: u64 }`
  - `pub fn pool_instance(entry: &LedgerEntryData) -> Result<PoolInstance, XdrError>`
  - `pub fn reserve_list(entry: &LedgerEntryData) -> Result<Vec<String>, XdrError>`
  - `pub fn reserve_config(entry: &LedgerEntryData) -> Result<ReserveConfig, XdrError>`
  - `pub fn reserve_data(entry: &LedgerEntryData) -> Result<ReserveData, XdrError>`
  - `pub fn positions(entry: &LedgerEntryData) -> Result<Positions, XdrError>`
  - `pub fn auction(entry: &LedgerEntryData) -> Result<AuctionData, XdrError>`
  - `pub fn positions_value(value: &ScVal) -> Result<Positions, XdrError>` and `pub fn auction_value(value: &ScVal) -> Result<AuctionData, XdrError>` (the same shapes as returned by `get_positions` and `get_auction`)
  - `pub fn reserve_value(value: &ScVal) -> Result<Reserve, XdrError>` (the `get_reserve` return)
  - `pub fn price_data(value: &ScVal) -> Result<PriceData, XdrError>`
  - `pub fn decimals(value: &ScVal) -> Result<u32, XdrError>`

- [ ] **Step 1: Write the failing tests**

Create `src/chain/xdr/decode.rs` with stubs and this test module. The first tests pin each shape; the last two are the ones that matter — they prove the whole `math` port against what the contract itself computed at the fixture's ledger.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{mainnet_fixed_v2, text};
    use crate::math::{calculate_position_data, OraclePrices, Reserve};
    use std::collections::BTreeMap;

    fn entry(base64: &str) -> LedgerEntryData {
        from_base64(base64).expect("entry parses")
    }

    fn value(base64: &str) -> ScVal {
        from_base64(base64).expect("value parses")
    }

    #[test]
    fn decodes_the_pool_instance() {
        let fixture = mainnet_fixed_v2();
        let instance = pool_instance(&entry(text(&fixture, &["instance_entry_xdr"]))).expect("instance");
        assert_eq!(instance.admin, "GAX2VVWVHU5YQY5J3NJBXKHI3FFKZN54BE6GRJCWSIKSBZTQWJJNJMPC");
        assert_eq!(instance.backstop, "CAQQR5SWBXKIGZKPBZDH3KM5GQ5GUTPKB7JAFCINLZBC5WXPJKRG3IM7");
        assert_eq!(instance.blnd_token, "CD25MNVTZDL4Y3XBCPCJXGXATV5WUHHOWMYFF4YBEGU5FCPGMYTVG5JY");
        assert_eq!(instance.name, "Fixed");
        assert_eq!(instance.config.oracle, "CCVTVW2CVA7JLH4ROQGP3CU4T3EXVCK66AZGSM4MUQPXAI4QHCZPOATS");
        assert_eq!(instance.config.bstop_rate, 2_000_000);
        assert_eq!(instance.config.status, 1);
        assert_eq!(instance.config.max_positions, 6);
        assert_eq!(instance.config.min_collateral, 50_000_000);
    }

    #[test]
    fn decodes_the_reserve_list_in_index_order() {
        let fixture = mainnet_fixed_v2();
        let assets = reserve_list(&entry(text(&fixture, &["res_list_entry_xdr"]))).expect("list");
        assert_eq!(
            assets,
            vec![
                "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA".to_string(),
                "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75".to_string(),
                "CDTKPWPLOURQA2SGTKTUQOWRCBZEORB4BWBOMJ3D3ZTQQSGE5F6JBQLV".to_string(),
            ]
        );
    }

    #[test]
    fn decodes_a_reserve_config_and_data() {
        let fixture = mainnet_fixed_v2();
        let config = reserve_config(&entry(text(&fixture, &["reserves", "0", "config_entry_xdr"]))).expect("config");
        assert_eq!(config.index, 0);
        assert_eq!(config.decimals, 7);
        assert_eq!(config.c_factor, 7_500_000);
        assert_eq!(config.l_factor, 7_500_000);
        assert_eq!(config.util, 4_000_000);
        assert_eq!(config.max_util, 7_000_000);
        assert_eq!(config.r_base, 100_000);
        assert_eq!(config.r_one, 300_000);
        assert_eq!(config.r_two, 3_000_000);
        assert_eq!(config.r_three, 50_000_000);
        assert_eq!(config.reactivity, 50);
        assert_eq!(config.supply_cap, 100_000_000_000_000_000);
        assert!(config.enabled);

        let data = reserve_data(&entry(text(&fixture, &["reserves", "0", "data_entry_xdr"]))).expect("data");
        assert_eq!(data.d_rate, 1_001_568_283_884);
        assert_eq!(data.b_rate, 1_000_022_303_241);
        assert_eq!(data.ir_mod, 1_000_000);
        assert_eq!(data.b_supply, 7_654_654_078_715_796);
        assert_eq!(data.d_supply, 13_201_825_877_188);
        assert_eq!(data.backstop_credit, 31_426_481);
        assert_eq!(data.last_time, 1_788_533_688);
    }

    #[test]
    fn decodes_the_oracle_decimals_and_prices() {
        let fixture = mainnet_fixed_v2();
        assert_eq!(decimals(&value(text(&fixture, &["oracle_decimals_return_xdr"]))), Ok(7));
        let price = price_data(&value(text(&fixture, &["reserves", "0", "lastprice_return_xdr"]))).expect("price");
        assert_eq!(price.price, 1_778_617);
        assert_eq!(price.timestamp, 1_788_534_300);
    }

    #[test]
    fn decodes_positions_identically_from_the_entry_and_the_view_call() {
        let fixture = mainnet_fixed_v2();
        let from_entry = positions(&entry(text(&fixture, &["users", "0", "positions_entry_xdr"]))).expect("entry");
        let from_view = positions_value(&value(text(&fixture, &["users", "0", "get_positions_return_xdr"]))).expect("view");
        assert_eq!(from_entry.collateral, BTreeMap::from([(1, 125_043_746)]));
        assert_eq!(from_entry.liabilities, BTreeMap::from([(1, 104_293_813)]));
        assert!(from_entry.supply.is_empty());
        assert_eq!(from_entry, from_view);
    }

    #[test]
    fn an_auction_round_trips_through_its_ledger_value() {
        // The retained event window held no liquidations when the fixture was
        // captured, so this shape is pinned by construction rather than by a
        // captured entry. The field names and types come from the contract's
        // `AuctionData`.
        use crate::chain::xdr::encode::{address, i128_val, map, symbol, to_base64};
        let usdc = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
        let xlm = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
        let encoded = map(vec![
            (symbol("bid").expect("symbol"), map(vec![(address(usdc).expect("address"), i128_val(1_234))]).expect("bid")),
            (symbol("block").expect("symbol"), ScVal::U32(64_271_347)),
            (symbol("lot").expect("symbol"), map(vec![(address(xlm).expect("address"), i128_val(5_678))]).expect("lot")),
        ])
        .expect("auction");
        let text = to_base64(&encoded).expect("base64");
        let decoded = auction_value(&value(&text)).expect("auction");
        assert_eq!(decoded.block, 64_271_347);
        assert_eq!(decoded.bid, BTreeMap::from([(usdc.to_string(), 1_234)]));
        assert_eq!(decoded.lot, BTreeMap::from([(xlm.to_string(), 5_678)]));
    }

    #[test]
    fn accruing_the_stored_entries_reproduces_the_contracts_get_reserve() {
        // The payoff test for `math::reserve`: the fixture's entries and the
        // contract's own `get_reserve` were read at one ledger, so accruing
        // the former to that ledger's close time must produce the latter,
        // to the stroop, for every reserve.
        let fixture = mainnet_fixed_v2();
        let instance = pool_instance(&entry(text(&fixture, &["instance_entry_xdr"]))).expect("instance");
        let now = fixture["ledger_close_time"].as_u64().expect("close time");
        let reserves = fixture["reserves"].as_array().expect("reserves");
        assert_eq!(reserves.len(), 3);
        for (index, _) in reserves.iter().enumerate() {
            let position = index.to_string();
            let asset = text(&fixture, &["reserves", &position, "asset"]).to_string();
            let config = reserve_config(&entry(text(&fixture, &["reserves", &position, "config_entry_xdr"]))).expect("config");
            let data = reserve_data(&entry(text(&fixture, &["reserves", &position, "data_entry_xdr"]))).expect("data");
            let mut reserve = Reserve::new(asset.clone(), config, data).expect("scalar");
            reserve.accrue(instance.config.bstop_rate, now).expect("accrues");

            let expected = reserve_value(&value(text(&fixture, &["reserves", &position, "get_reserve_return_xdr"]))).expect("get_reserve");
            assert_eq!(reserve.asset, expected.asset, "asset for reserve {index}");
            assert_eq!(reserve.scalar, expected.scalar, "scalar for reserve {index}");
            assert_eq!(reserve.config, expected.config, "config for reserve {index}");
            assert_eq!(reserve.data, expected.data, "accrued data for reserve {index}");
        }
    }

    #[test]
    fn values_the_fixtures_users_the_way_the_contract_would() {
        // The payoff test for `math::position`: real positions, real accrued
        // reserves, real oracle prices, at one ledger.
        let fixture = mainnet_fixed_v2();
        let instance = pool_instance(&entry(text(&fixture, &["instance_entry_xdr"]))).expect("instance");
        let now = fixture["ledger_close_time"].as_u64().expect("close time");

        let mut reserves = BTreeMap::new();
        let mut prices = BTreeMap::new();
        for index in 0..fixture["reserves"].as_array().expect("reserves").len() {
            let position = index.to_string();
            let asset = text(&fixture, &["reserves", &position, "asset"]).to_string();
            let mut reserve = reserve_value(&value(text(&fixture, &["reserves", &position, "get_reserve_return_xdr"]))).expect("reserve");
            reserve.accrue(instance.config.bstop_rate, now).expect("already accrued");
            prices.insert(asset, price_data(&value(text(&fixture, &["reserves", &position, "lastprice_return_xdr"]))).expect("price").price);
            reserves.insert(reserve.config.index, reserve);
        }
        let oracle_decimals = decimals(&value(text(&fixture, &["oracle_decimals_return_xdr"]))).expect("decimals");
        let prices = OraclePrices::new(oracle_decimals, prices).expect("scalar");

        let first = positions(&entry(text(&fixture, &["users", "0", "positions_entry_xdr"]))).expect("positions");
        let data = calculate_position_data(&reserves, &prices, &first).expect("values");
        assert_eq!(data.collateral_base, 135_838_407);
        assert_eq!(data.collateral_raw, 142_987_797);
        assert_eq!(data.liability_base, 134_883_864);
        assert_eq!(data.liability_raw, 128_139_670);
        assert_eq!(data.health_factor(), Ok(Some(10_070_767)));

        let second = positions(&entry(text(&fixture, &["users", "1", "positions_entry_xdr"]))).expect("positions");
        let data = calculate_position_data(&reserves, &prices, &second).expect("values");
        assert_eq!(data.collateral_base, 9_599_134_909);
        assert_eq!(data.collateral_raw, 10_104_352_536);
        assert_eq!(data.liability_base, 9_503_768_801);
        assert_eq!(data.liability_raw, 9_028_580_360);
        assert_eq!(data.health_factor(), Ok(Some(10_100_345)));
        // Both users are above water, so neither would be liquidatable.
        assert_eq!(data.is_hf_under(9_980_000), Ok(false));
    }

    #[test]
    fn a_value_of_the_wrong_shape_is_an_error_not_a_panic() {
        assert!(matches!(decimals(&ScVal::Void), Err(XdrError::Shape { .. })));
        assert!(matches!(positions_value(&ScVal::U32(1)), Err(XdrError::Shape { .. })));
        let empty = crate::chain::xdr::encode::map(vec![]).expect("map");
        assert!(matches!(price_data(&empty), Err(XdrError::MissingField("price"))));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib chain::xdr::decode`
Expected: panics on `todo!()`.

- [ ] **Step 3: Implement**

```rust
//! Turning ledger entries and view-call results into the bot's types.
//!
//! Every decoder is written against the contract's own struct definitions.
//! A shape that does not match is an `XdrError`, never a default value: a
//! silently-zero rate or balance would produce a confident wrong decision,
//! and a loud failure at startup is the cheaper outcome.

use std::collections::BTreeMap;

use stellar_xdr::{Int128Parts, LedgerEntryData, ScVal};

use super::encode::from_base64;
use super::XdrError;
use crate::math::{AuctionData, Positions, Reserve, ReserveConfig, ReserveData};

/// The pool's `PoolConfig` instance-storage entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// The SEP-40 oracle the pool prices against.
    pub oracle: String,
    /// The backstop's share of accrued interest, 7 decimals.
    pub bstop_rate: u32,
    /// 0 admin-active, 1 active, 2/3 on-ice, 4/5 frozen, 6 setup.
    pub status: u32,
    /// The most collateral-plus-liability positions one account may hold,
    /// and the most assets one auction may name.
    pub max_positions: u32,
    /// The least collateral, in the oracle's decimals, a borrowing position
    /// must hold.
    pub min_collateral: i128,
}

/// Everything the pool keeps in instance storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInstance {
    /// The pool's admin account.
    pub admin: String,
    /// The backstop contract, and the auction counterparty for bad debt and
    /// interest auctions.
    pub backstop: String,
    /// The BLND token the pool emits.
    pub blnd_token: String,
    /// The pool's display name.
    pub name: String,
    /// The pool's configuration.
    pub config: PoolConfig,
}

/// A SEP-40 `PriceData`: a price in the oracle's decimals and the second it
/// was published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceData {
    /// The price, in the oracle's decimals.
    pub price: i128,
    /// Publication time, seconds since the epoch.
    pub timestamp: u64,
}

fn shape(expected: &'static str, got: &impl std::fmt::Debug) -> XdrError {
    XdrError::Shape { expected, got: format!("{got:?}") }
}

fn contract_value(entry: &LedgerEntryData) -> Result<&ScVal, XdrError> {
    match entry {
        LedgerEntryData::ContractData(data) => Ok(&data.val),
        other => Err(shape("contract data", other)),
    }
}

fn as_i128(parts: &Int128Parts) -> i128 {
    (i128::from(parts.hi) << 64) | i128::from(parts.lo)
}

/// The fields of a contract struct, keyed by their symbol names.
fn fields(value: &ScVal) -> Result<BTreeMap<String, &ScVal>, XdrError> {
    match value {
        ScVal::Map(Some(entries)) => entries
            .iter()
            .map(|entry| match &entry.key {
                ScVal::Symbol(name) => Ok((name.to_utf8_string_lossy(), &entry.val)),
                other => Err(shape("symbol key", other)),
            })
            .collect(),
        other => Err(shape("struct map", other)),
    }
}

fn field<'a>(fields: &BTreeMap<String, &'a ScVal>, name: &'static str) -> Result<&'a ScVal, XdrError> {
    fields.get(name).copied().ok_or(XdrError::MissingField(name))
}

fn u32_field(fields: &BTreeMap<String, &ScVal>, name: &'static str) -> Result<u32, XdrError> {
    match field(fields, name)? {
        ScVal::U32(value) => Ok(*value),
        other => Err(shape("u32", other)),
    }
}

fn u64_field(fields: &BTreeMap<String, &ScVal>, name: &'static str) -> Result<u64, XdrError> {
    match field(fields, name)? {
        ScVal::U64(value) => Ok(*value),
        other => Err(shape("u64", other)),
    }
}

fn i128_field(fields: &BTreeMap<String, &ScVal>, name: &'static str) -> Result<i128, XdrError> {
    match field(fields, name)? {
        ScVal::I128(parts) => Ok(as_i128(parts)),
        other => Err(shape("i128", other)),
    }
}

fn bool_field(fields: &BTreeMap<String, &ScVal>, name: &'static str) -> Result<bool, XdrError> {
    match field(fields, name)? {
        ScVal::Bool(value) => Ok(*value),
        other => Err(shape("bool", other)),
    }
}

fn address_field(fields: &BTreeMap<String, &ScVal>, name: &'static str) -> Result<String, XdrError> {
    match field(fields, name)? {
        ScVal::Address(address) => Ok(address.to_string()),
        other => Err(shape("address", other)),
    }
}

/// A map of reserve index to token amount, as `Positions` stores each side.
fn index_map(value: &ScVal) -> Result<BTreeMap<u32, i128>, XdrError> {
    match value {
        ScVal::Map(Some(entries)) => entries
            .iter()
            .map(|entry| match (&entry.key, &entry.val) {
                (ScVal::U32(index), ScVal::I128(amount)) => Ok((*index, as_i128(amount))),
                (key, _) => Err(shape("u32 to i128", key)),
            })
            .collect(),
        other => Err(shape("index map", other)),
    }
}

/// A map of asset address to amount, as `AuctionData` stores each side.
fn address_map(value: &ScVal) -> Result<BTreeMap<String, i128>, XdrError> {
    match value {
        ScVal::Map(Some(entries)) => entries
            .iter()
            .map(|entry| match (&entry.key, &entry.val) {
                (ScVal::Address(asset), ScVal::I128(amount)) => Ok((asset.to_string(), as_i128(amount))),
                (key, _) => Err(shape("address to i128", key)),
            })
            .collect(),
        other => Err(shape("address map", other)),
    }
}

/// Decodes the pool's contract-instance entry.
pub fn pool_instance(entry: &LedgerEntryData) -> Result<PoolInstance, XdrError> {
    let ScVal::ContractInstance(instance) = contract_value(entry)? else {
        return Err(shape("contract instance", entry));
    };
    let storage: BTreeMap<String, &ScVal> = instance
        .storage
        .iter()
        .flat_map(|map| map.iter())
        .filter_map(|entry| match &entry.key {
            ScVal::Symbol(name) => Some((name.to_utf8_string_lossy(), &entry.val)),
            _ => None,
        })
        .collect();

    let config_fields = fields(field(&storage, "Config")?)?;
    let name = match field(&storage, "Name")? {
        ScVal::String(text) => text.to_utf8_string_lossy(),
        other => return Err(shape("string", other)),
    };
    Ok(PoolInstance {
        admin: address_field(&storage, "Admin")?,
        backstop: address_field(&storage, "Backstop")?,
        blnd_token: address_field(&storage, "BLNDTkn")?,
        name,
        config: PoolConfig {
            oracle: address_field(&config_fields, "oracle")?,
            bstop_rate: u32_field(&config_fields, "bstop_rate")?,
            status: u32_field(&config_fields, "status")?,
            max_positions: u32_field(&config_fields, "max_positions")?,
            min_collateral: i128_field(&config_fields, "min_collateral")?,
        },
    })
}

/// Decodes `ResList`. The position in this vector is the reserve index that
/// `Positions` keys on.
pub fn reserve_list(entry: &LedgerEntryData) -> Result<Vec<String>, XdrError> {
    match contract_value(entry)? {
        ScVal::Vec(Some(items)) => items
            .iter()
            .map(|item| match item {
                ScVal::Address(address) => Ok(address.to_string()),
                other => Err(shape("address", other)),
            })
            .collect(),
        other => Err(shape("vector of addresses", other)),
    }
}

/// Decodes a `ResConfig` entry.
pub fn reserve_config(entry: &LedgerEntryData) -> Result<ReserveConfig, XdrError> {
    reserve_config_value(contract_value(entry)?)
}

/// Decodes a `ResData` entry.
pub fn reserve_data(entry: &LedgerEntryData) -> Result<ReserveData, XdrError> {
    reserve_data_value(contract_value(entry)?)
}

/// Decodes a `Positions` entry.
pub fn positions(entry: &LedgerEntryData) -> Result<Positions, XdrError> {
    positions_value(contract_value(entry)?)
}

/// Decodes an `Auction` entry.
pub fn auction(entry: &LedgerEntryData) -> Result<AuctionData, XdrError> {
    auction_value(contract_value(entry)?)
}

/// Decodes a `ReserveConfig` value, wherever it came from.
pub fn reserve_config_value(value: &ScVal) -> Result<ReserveConfig, XdrError> {
    let fields = fields(value)?;
    Ok(ReserveConfig {
        index: u32_field(&fields, "index")?,
        decimals: u32_field(&fields, "decimals")?,
        c_factor: u32_field(&fields, "c_factor")?,
        l_factor: u32_field(&fields, "l_factor")?,
        util: u32_field(&fields, "util")?,
        max_util: u32_field(&fields, "max_util")?,
        r_base: u32_field(&fields, "r_base")?,
        r_one: u32_field(&fields, "r_one")?,
        r_two: u32_field(&fields, "r_two")?,
        r_three: u32_field(&fields, "r_three")?,
        reactivity: u32_field(&fields, "reactivity")?,
        supply_cap: i128_field(&fields, "supply_cap")?,
        enabled: bool_field(&fields, "enabled")?,
    })
}

/// Decodes a `ReserveData` value, wherever it came from.
pub fn reserve_data_value(value: &ScVal) -> Result<ReserveData, XdrError> {
    let fields = fields(value)?;
    Ok(ReserveData {
        d_rate: i128_field(&fields, "d_rate")?,
        b_rate: i128_field(&fields, "b_rate")?,
        ir_mod: i128_field(&fields, "ir_mod")?,
        b_supply: i128_field(&fields, "b_supply")?,
        d_supply: i128_field(&fields, "d_supply")?,
        backstop_credit: i128_field(&fields, "backstop_credit")?,
        last_time: u64_field(&fields, "last_time")?,
    })
}

/// Decodes the pool's `get_reserve` return: config, data, asset and scalar
/// already accrued to the simulated ledger.
pub fn reserve_value(value: &ScVal) -> Result<Reserve, XdrError> {
    let fields = fields(value)?;
    Ok(Reserve {
        asset: address_field(&fields, "asset")?,
        config: reserve_config_value(field(&fields, "config")?)?,
        data: reserve_data_value(field(&fields, "data")?)?,
        scalar: i128_field(&fields, "scalar")?,
    })
}

/// Decodes a `Positions` value, as stored or as `get_positions` returns it.
pub fn positions_value(value: &ScVal) -> Result<Positions, XdrError> {
    let fields = fields(value)?;
    Ok(Positions {
        collateral: index_map(field(&fields, "collateral")?)?,
        liabilities: index_map(field(&fields, "liabilities")?)?,
        supply: index_map(field(&fields, "supply")?)?,
    })
}

/// Decodes an `AuctionData` value, as stored or as an event carries it.
pub fn auction_value(value: &ScVal) -> Result<AuctionData, XdrError> {
    let fields = fields(value)?;
    Ok(AuctionData {
        bid: address_map(field(&fields, "bid")?)?,
        lot: address_map(field(&fields, "lot")?)?,
        block: u32_field(&fields, "block")?,
    })
}

/// Decodes a SEP-40 `lastprice` return. The oracle returns an `Option`, and
/// a `None` price is a missing price, not a zero.
pub fn price_data(value: &ScVal) -> Result<PriceData, XdrError> {
    let fields = fields(value)?;
    Ok(PriceData {
        price: i128_field(&fields, "price")?,
        timestamp: u64_field(&fields, "timestamp")?,
    })
}

/// Decodes a SEP-40 `decimals` return.
pub fn decimals(value: &ScVal) -> Result<u32, XdrError> {
    match value {
        ScVal::U32(decimals) => Ok(*decimals),
        other => Err(shape("u32", other)),
    }
}

/// Decodes a base64 ledger entry straight to its `LedgerEntryData`.
pub fn entry_from_base64(text: &str) -> Result<LedgerEntryData, XdrError> {
    from_base64(text)
}
```

`fields` returning a `BTreeMap<String, &ScVal>` costs a small allocation per decode and buys named lookup with a `MissingField` error; the alternative, positional access, breaks silently when the contract reorders a struct.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib chain::xdr::decode`
Expected: `9 passed`. If `accruing_the_stored_entries_reproduces_the_contracts_get_reserve` fails, the accrual port is wrong — do not adjust the expectation, fix `math::reserve`; the fixture is the contract's own answer.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt --all
git add src/chain/xdr/decode.rs src/chain/xdr/mod.rs
git commit -m "feat(chain): ledger entry and view-call decoders, pinned to a mainnet fixture

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: `chain::xdr::events`

**Files:**
- Create: `src/chain/xdr/events.rs`
- Modify: `src/chain/xdr/mod.rs` (re-exports)

**Interfaces:**
- Consumes: `decode::{auction_value, PriceData}`, `encode`, `math::AuctionData`
- Produces:
  - `pub enum PoolEvent { Supply {..}, Withdraw {..}, SupplyCollateral {..}, WithdrawCollateral {..}, Borrow {..}, Repay {..}, FlashLoan {..}, NewAuction {..}, FillAuction {..}, DeleteAuction {..}, BadDebt {..}, DefaultedDebt {..}, SetReserve {..}, SetStatus {..} }` with the field sets in the implementation below
  - `pub fn decode_pool_event(topics: &[ScVal], value: &ScVal) -> Result<Option<PoolEvent>, XdrError>` — `Ok(None)` for an event the bot does not model
  - `PoolEvent::affected_accounts(&self) -> Vec<&str>`

- [ ] **Step 1: Write the failing tests**

Create `src/chain/xdr/events.rs` with stubs and this test module. Five of the six shapes are pinned by real events from the fixture; the auction events are pinned by construction, because the pool had no liquidation in the RPC's retained window when the fixture was captured. That is stated in the test so a future reader does not mistake it for laziness.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::xdr::encode::{address, i128_val, map, symbol, to_base64};
    use crate::chain::xdr::from_base64;
    use crate::fixture::{mainnet_fixed_v2, text};
    use std::collections::BTreeMap;

    const XLM: &str = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
    const EURC: &str = "CDTKPWPLOURQA2SGTKTUQOWRCBZEORB4BWBOMJ3D3ZTQQSGE5F6JBQLV";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
    const FILLER: &str = "GCIH7OYR6LX6364PGGLAKGMZYLV37EAH6YXZAFK7RY7U4K7625XBH5EL";

    /// Decodes fixture event `index`.
    fn fixture_event(index: usize) -> Option<PoolEvent> {
        let fixture = mainnet_fixed_v2();
        let position = index.to_string();
        let raw = &fixture["events"][index];
        let topics: Vec<ScVal> = raw["topic"]
            .as_array()
            .expect("topics")
            .iter()
            .map(|topic| from_base64(topic.as_str().expect("base64")).expect("topic parses"))
            .collect();
        let value: ScVal = from_base64(text(&fixture, &["events", &position, "value"])).expect("value parses");
        decode_pool_event(&topics, &value).expect("decodes")
    }

    #[test]
    fn decodes_a_real_supply() {
        assert_eq!(
            fixture_event(0),
            Some(PoolEvent::Supply {
                asset: USDC.to_string(),
                from: "CDB2WMKQQNVZMEBY7Q7GZ5C7E7IAFSNMZ7GGVD6WKTCEWK7XOIAVZSAP".to_string(),
                amount: 102_008_939,
                b_tokens: 89_367_096,
            })
        );
    }

    #[test]
    fn decodes_a_real_supply_collateral() {
        assert_eq!(
            fixture_event(3),
            Some(PoolEvent::SupplyCollateral {
                asset: USDC.to_string(),
                from: "GCQFPCCKQ6SLE4Z56L2YZQVFMJAG3NY3VPGNGT477CNPQZ2LNVJQOG54".to_string(),
                amount: 19_762_500_000,
                b_tokens: 17_312_878_935,
            })
        );
    }

    #[test]
    fn decodes_a_real_borrow() {
        assert_eq!(
            fixture_event(6),
            Some(PoolEvent::Borrow {
                asset: EURC.to_string(),
                from: "GDU4ICJ4W23A4TPQZ6ORIAL2O7Y6ZG6BWWT4PNCJZEU3JZCFR37HTB4H".to_string(),
                amount: 150_000_000,
                d_tokens: 122_190_129,
            })
        );
    }

    #[test]
    fn decodes_a_real_repay() {
        assert_eq!(
            fixture_event(9),
            Some(PoolEvent::Repay {
                asset: XLM.to_string(),
                from: "CA4I5TPQAEF6C62B4UI7IDNDAPT5RUNSYGB6WSYNIQXHLG4JOFX2NSMH".to_string(),
                amount: 98_707_774,
                d_tokens: 98_555_365,
            })
        );
    }

    #[test]
    fn decodes_a_real_withdraw_collateral() {
        assert_eq!(
            fixture_event(12),
            Some(PoolEvent::WithdrawCollateral {
                asset: USDC.to_string(),
                from: "GCC4A2FN5BIXW6I57LKMP4XK7WVNZJWDCD5JZGGQKAI45PNTPC5NU6U4".to_string(),
                amount: 102_008_942,
                b_tokens: 89_367_100,
            })
        );
    }

    /// The auction data the synthetic auction events carry.
    fn auction_scval() -> ScVal {
        map(vec![
            (symbol("bid").expect("symbol"), map(vec![(address(USDC).expect("address"), i128_val(1_234))]).expect("bid")),
            (symbol("block").expect("symbol"), ScVal::U32(64_271_348)),
            (symbol("lot").expect("symbol"), map(vec![(address(XLM).expect("address"), i128_val(5_678))]).expect("lot")),
        ])
        .expect("auction")
    }

    fn expected_auction() -> AuctionData {
        AuctionData {
            bid: BTreeMap::from([(USDC.to_string(), 1_234)]),
            lot: BTreeMap::from([(XLM.to_string(), 5_678)]),
            block: 64_271_348,
        }
    }

    #[test]
    fn decodes_a_constructed_new_auction() {
        // Constructed, not captured: the pool had no liquidation in the RPC's
        // retained window when the fixture was taken. The shape is the
        // contract's `PoolEvents::new_auction`.
        let topics = vec![symbol("new_auction").expect("symbol"), ScVal::U32(0), address(USER).expect("address")];
        let value = crate::chain::xdr::encode::vec(vec![ScVal::U32(42), auction_scval()]).expect("data");
        assert_eq!(
            decode_pool_event(&topics, &value).expect("decodes"),
            Some(PoolEvent::NewAuction { auction_type: 0, user: USER.to_string(), percent: 42, auction: expected_auction() })
        );
    }

    #[test]
    fn decodes_a_constructed_fill_auction() {
        let topics = vec![symbol("fill_auction").expect("symbol"), ScVal::U32(0), address(USER).expect("address")];
        let value = crate::chain::xdr::encode::vec(vec![address(FILLER).expect("address"), i128_val(75), auction_scval()]).expect("data");
        let event = decode_pool_event(&topics, &value).expect("decodes").expect("modelled");
        assert_eq!(
            event,
            PoolEvent::FillAuction {
                auction_type: 0,
                user: USER.to_string(),
                filler: FILLER.to_string(),
                fill_percent: 75,
                filled: expected_auction(),
            }
        );
        // Both sides of a fill change positions, so both must be refreshed.
        assert_eq!(event.affected_accounts(), vec![USER, FILLER]);
    }

    #[test]
    fn decodes_a_constructed_delete_auction_and_bad_debt() {
        let topics = vec![symbol("delete_auction").expect("symbol"), ScVal::U32(0), address(USER).expect("address")];
        assert_eq!(
            decode_pool_event(&topics, &ScVal::Void).expect("decodes"),
            Some(PoolEvent::DeleteAuction { auction_type: 0, user: USER.to_string() })
        );

        let topics = vec![symbol("bad_debt").expect("symbol"), address(USER).expect("address"), address(USDC).expect("address")];
        assert_eq!(
            decode_pool_event(&topics, &i128_val(9_999)).expect("decodes"),
            Some(PoolEvent::BadDebt { user: USER.to_string(), asset: USDC.to_string(), d_tokens: 9_999 })
        );
    }

    #[test]
    fn an_unmodelled_event_is_none_rather_than_an_error() {
        // Pools emit events this bot has no use for, and new ones arrive with
        // contract upgrades. Neither may stop the poller.
        let topics = vec![symbol("gulp_emissions").expect("symbol")];
        assert_eq!(decode_pool_event(&topics, &i128_val(1)).expect("decodes"), None);
    }

    #[test]
    fn a_modelled_event_with_the_wrong_shape_is_an_error() {
        let topics = vec![symbol("supply").expect("symbol"), address(USDC).expect("address")];
        assert!(matches!(decode_pool_event(&topics, &ScVal::Void), Err(XdrError::Shape { .. })));
    }

    #[test]
    fn affected_accounts_names_every_account_a_position_moved_for() {
        let supply = fixture_event(0).expect("modelled");
        assert_eq!(supply.affected_accounts(), vec!["CDB2WMKQQNVZMEBY7Q7GZ5C7E7IAFSNMZ7GGVD6WKTCEWK7XOIAVZSAP"]);
        let auction = PoolEvent::DeleteAuction { auction_type: 0, user: USER.to_string() };
        assert_eq!(auction.affected_accounts(), vec![USER]);
        let reserve = PoolEvent::SetReserve { asset: XLM.to_string(), index: 0 };
        assert!(reserve.affected_accounts().is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib chain::xdr::events`
Expected: panics on `todo!()`.

- [ ] **Step 3: Implement**

```rust
//! Pool event decoding.
//!
//! Topic and data shapes come from the v2 pool's `PoolEvents`. An event this
//! bot does not model decodes to `None` rather than an error: pools emit
//! events for emissions and administration that no liquidator needs, and a
//! contract upgrade may add more, neither of which may stall the poller. A
//! *modelled* event with an unexpected shape is an error, because that means
//! a shape this bot depends on has changed.

use stellar_xdr::ScVal;

use super::decode::auction_value;
use super::XdrError;
use crate::math::AuctionData;

/// A Blend v2 pool event the bot acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolEvent {
    /// Uncollateralised supply.
    Supply { asset: String, from: String, amount: i128, b_tokens: i128 },
    /// Withdrawal of uncollateralised supply.
    Withdraw { asset: String, from: String, amount: i128, b_tokens: i128 },
    /// Supply posted as collateral.
    SupplyCollateral { asset: String, from: String, amount: i128, b_tokens: i128 },
    /// Collateral withdrawn.
    WithdrawCollateral { asset: String, from: String, amount: i128, b_tokens: i128 },
    /// New borrowing.
    Borrow { asset: String, from: String, amount: i128, d_tokens: i128 },
    /// Debt repaid.
    Repay { asset: String, from: String, amount: i128, d_tokens: i128 },
    /// A flash loan, which also mints debt for `from` within the transaction.
    FlashLoan { asset: String, from: String, contract: String, amount: i128, d_tokens: i128 },
    /// An auction was created. `auction_type` is 0 liquidation, 1 bad debt,
    /// 2 interest; `percent` is the share of the user's positions auctioned.
    NewAuction { auction_type: u32, user: String, percent: u32, auction: AuctionData },
    /// An auction was filled, wholly or in part, by `filler`.
    FillAuction { auction_type: u32, user: String, filler: String, fill_percent: i128, filled: AuctionData },
    /// An auction was deleted before being filled.
    DeleteAuction { auction_type: u32, user: String },
    /// A user's debt moved to the backstop.
    BadDebt { user: String, asset: String, d_tokens: i128 },
    /// The backstop defaulted debt; suppliers took the loss.
    DefaultedDebt { asset: String, d_tokens: i128 },
    /// A reserve was added or reconfigured.
    SetReserve { asset: String, index: u32 },
    /// The pool's status changed, which gates what requests it accepts.
    SetStatus { status: u32 },
}

impl PoolEvent {
    /// The accounts whose pool positions this event may have changed, and
    /// which the tracker must therefore re-read. Empty for events that
    /// change pool-wide state only.
    pub fn affected_accounts(&self) -> Vec<&str> {
        match self {
            Self::Supply { from, .. }
            | Self::Withdraw { from, .. }
            | Self::SupplyCollateral { from, .. }
            | Self::WithdrawCollateral { from, .. }
            | Self::Borrow { from, .. }
            | Self::Repay { from, .. }
            | Self::FlashLoan { from, .. } => vec![from],
            Self::NewAuction { user, .. } | Self::DeleteAuction { user, .. } | Self::BadDebt { user, .. } => vec![user],
            Self::FillAuction { user, filler, .. } => vec![user, filler],
            Self::DefaultedDebt { .. } | Self::SetReserve { .. } | Self::SetStatus { .. } => Vec::new(),
        }
    }
}

fn shape(expected: &'static str, got: &impl std::fmt::Debug) -> XdrError {
    XdrError::Shape { expected, got: format!("{got:?}") }
}

fn as_address(value: &ScVal) -> Result<String, XdrError> {
    match value {
        ScVal::Address(address) => Ok(address.to_string()),
        other => Err(shape("address", other)),
    }
}

fn as_u32(value: &ScVal) -> Result<u32, XdrError> {
    match value {
        ScVal::U32(number) => Ok(*number),
        other => Err(shape("u32", other)),
    }
}

fn as_i128(value: &ScVal) -> Result<i128, XdrError> {
    match value {
        ScVal::I128(parts) => Ok((i128::from(parts.hi) << 64) | i128::from(parts.lo)),
        other => Err(shape("i128", other)),
    }
}

/// The data vector of an event, required to hold exactly `expected` items.
fn data<'a>(value: &'a ScVal, expected: usize) -> Result<&'a [ScVal], XdrError> {
    match value {
        ScVal::Vec(Some(items)) if items.len() == expected => Ok(items.as_slice()),
        other => Err(shape("event data vector", other)),
    }
}

/// A topic at `index`, or a shape error naming what was expected.
fn topic(topics: &[ScVal], index: usize) -> Result<&ScVal, XdrError> {
    topics.get(index).ok_or_else(|| XdrError::Shape { expected: "another topic", got: format!("{} topics", topics.len()) })
}

/// Decodes one contract event into a `PoolEvent`.
///
/// `topics` are the event's topics in order, the first being its name;
/// `value` is its data. Returns `Ok(None)` when the event is not one the bot
/// models.
pub fn decode_pool_event(topics: &[ScVal], value: &ScVal) -> Result<Option<PoolEvent>, XdrError> {
    let ScVal::Symbol(name) = topic(topics, 0)? else {
        return Ok(None);
    };
    let name = name.to_utf8_string_lossy();

    // asset, from | amount, reserve tokens
    let two_sided = |topics: &[ScVal], value: &ScVal| -> Result<(String, String, i128, i128), XdrError> {
        let asset = as_address(topic(topics, 1)?)?;
        let from = as_address(topic(topics, 2)?)?;
        let items = data(value, 2)?;
        Ok((asset, from, as_i128(&items[0])?, as_i128(&items[1])?))
    };

    let event = match name.as_str() {
        "supply" => {
            let (asset, from, amount, b_tokens) = two_sided(topics, value)?;
            PoolEvent::Supply { asset, from, amount, b_tokens }
        }
        "withdraw" => {
            let (asset, from, amount, b_tokens) = two_sided(topics, value)?;
            PoolEvent::Withdraw { asset, from, amount, b_tokens }
        }
        "supply_collateral" => {
            let (asset, from, amount, b_tokens) = two_sided(topics, value)?;
            PoolEvent::SupplyCollateral { asset, from, amount, b_tokens }
        }
        "withdraw_collateral" => {
            let (asset, from, amount, b_tokens) = two_sided(topics, value)?;
            PoolEvent::WithdrawCollateral { asset, from, amount, b_tokens }
        }
        "borrow" => {
            let (asset, from, amount, d_tokens) = two_sided(topics, value)?;
            PoolEvent::Borrow { asset, from, amount, d_tokens }
        }
        "repay" => {
            let (asset, from, amount, d_tokens) = two_sided(topics, value)?;
            PoolEvent::Repay { asset, from, amount, d_tokens }
        }
        "flash_loan" => {
            let items = data(value, 2)?;
            PoolEvent::FlashLoan {
                asset: as_address(topic(topics, 1)?)?,
                from: as_address(topic(topics, 2)?)?,
                contract: as_address(topic(topics, 3)?)?,
                amount: as_i128(&items[0])?,
                d_tokens: as_i128(&items[1])?,
            }
        }
        "new_auction" => {
            let items = data(value, 2)?;
            PoolEvent::NewAuction {
                auction_type: as_u32(topic(topics, 1)?)?,
                user: as_address(topic(topics, 2)?)?,
                percent: as_u32(&items[0])?,
                auction: auction_value(&items[1])?,
            }
        }
        "fill_auction" => {
            let items = data(value, 3)?;
            PoolEvent::FillAuction {
                auction_type: as_u32(topic(topics, 1)?)?,
                user: as_address(topic(topics, 2)?)?,
                filler: as_address(&items[0])?,
                fill_percent: as_i128(&items[1])?,
                filled: auction_value(&items[2])?,
            }
        }
        "delete_auction" => PoolEvent::DeleteAuction {
            auction_type: as_u32(topic(topics, 1)?)?,
            user: as_address(topic(topics, 2)?)?,
        },
        "bad_debt" => PoolEvent::BadDebt {
            user: as_address(topic(topics, 1)?)?,
            asset: as_address(topic(topics, 2)?)?,
            d_tokens: as_i128(value)?,
        },
        "defaulted_debt" => PoolEvent::DefaultedDebt {
            asset: as_address(topic(topics, 1)?)?,
            d_tokens: as_i128(value)?,
        },
        "set_reserve" => {
            let items = data(value, 2)?;
            PoolEvent::SetReserve { asset: as_address(&items[0])?, index: as_u32(&items[1])? }
        }
        // Emitted with one topic by `update_status` and two by the admin's
        // `set_status`; the status itself is the data either way.
        "set_status" => PoolEvent::SetStatus { status: as_u32(value)? },
        _ => return Ok(None),
    };
    Ok(Some(event))
}
```

Add `pub use events::{decode_pool_event, PoolEvent};` to `src/chain/xdr/mod.rs`. If clippy's `enum_variant_names` or `large_enum_variant` fires on `PoolEvent`, box the two auction-carrying variants' `AuctionData` rather than silencing the lint, and update the tests to match.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib chain::xdr::events`
Expected: `11 passed`.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets -- -D warnings && cargo fmt --all
git add src/chain/xdr/events.rs src/chain/xdr/mod.rs
git commit -m "feat(chain): pool event decoding

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: The fixture capture tool, documentation and changelog

**Files:**
- Create: `examples/capture_fixture.rs`
- Modify: `CLAUDE.md`, `CHANGELOG.md`, `.dockerignore`

**Interfaces:**
- Consumes: `chain::xdr::{encode, keys}`
- Produces: the `capture_fixture` example, run by hand.

- [ ] **Step 1: Write the capture tool**

`examples/capture_fixture.rs`. Note the lint posture: `[lints.clippy]` applies to examples too, and `clippy.toml`'s test exemptions do not, so this file may use neither `unwrap` nor `expect`. Everything is `?` on `Box<dyn Error>`.

```rust
//! Refreshes a pool fixture from a live Soroban RPC.
//!
//! ```text
//! cargo run --example capture_fixture -- \
//!   https://mainnet.sorobanrpc.com <pool> tests/fixtures/mainnet-fixed-v2.json [user...]
//! ```
//!
//! `curl` is the transport, so refreshing a fixture by hand costs the crate
//! no HTTP dependency.
//!
//! The fixture is only useful if everything in it describes one ledger: the
//! tests accrue the stored entries to that ledger's close time and compare
//! against the contract's own `get_reserve`. So each attempt reads the
//! entries, runs every simulation, reads the entries again, and keeps the
//! result only if the two reads are byte-identical and every simulation
//! reported the same ledger. Otherwise it retries.
//!
//! The accrual target itself is read from those `get_reserve` returns, not
//! from a separate RPC call: the pool contract's `Reserve::load` always
//! stamps `data.last_time` with the timestamp of the ledger it ran in (it
//! short-circuits when they already agree, and sets it in every other
//! branch), so each captured reserve already carries the exact second the
//! fixture's tests must accrue to. A later, independent call — `getHealth`,
//! say — has no such guarantee: mainnet closes a ledger roughly every five
//! seconds, and the round trip to ask again almost always lands on a newer
//! one than the simulations just agreed on.

use std::error::Error;
use std::process::Command;

use blend_liquidator::chain::xdr::encode::{address, invoke_contract_op, simulation_envelope, stellar_asset, symbol, to_base64};
use blend_liquidator::chain::xdr::{decode, from_base64, keys};
use serde_json::{json, Value};

type Fallible<T> = Result<T, Box<dyn Error>>;

const ATTEMPTS: usize = 10;
/// Topics whose recent events go into the fixture, with their topic arity.
const EVENT_TOPICS: [(&str, usize); 8] = [
    ("supply", 3),
    ("supply_collateral", 3),
    ("borrow", 3),
    ("repay", 3),
    ("withdraw_collateral", 3),
    ("new_auction", 3),
    ("fill_auction", 3),
    ("delete_auction", 3),
];
/// At most this many of each topic, to keep the fixture readable.
const EVENTS_PER_TOPIC: usize = 3;

fn rpc(url: &str, method: &str, params: &Value) -> Fallible<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let output = Command::new("curl")
        .args(["-s", "-m", "30", "-X", "POST", url, "-H", "Content-Type: application/json", "-d", &body])
        .output()?;
    if !output.status.success() {
        return Err(format!("curl failed for {method}: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    let response: Value = serde_json::from_slice(&output.stdout)?;
    if let Some(error) = response.get("error") {
        return Err(format!("{method}: {error}").into());
    }
    response.get("result").cloned().ok_or_else(|| format!("{method}: no result").into())
}

/// Reads ledger entries, returning `(latestLedger, [(key, xdr)])`.
fn ledger_entries(url: &str, key_base64: &[String]) -> Fallible<(u64, Vec<(String, String)>)> {
    let result = rpc(url, "getLedgerEntries", &json!({ "keys": key_base64 }))?;
    let ledger = result["latestLedger"].as_u64().ok_or("latestLedger missing")?;
    let mut entries = Vec::new();
    for entry in result["entries"].as_array().unwrap_or(&Vec::new()) {
        let key = entry["key"].as_str().ok_or("entry key missing")?.to_string();
        let xdr = entry["xdr"].as_str().ok_or("entry xdr missing")?.to_string();
        entries.push((key, xdr));
    }
    Ok((ledger, entries))
}

/// Simulates a read-only call, returning `(latestLedger, return value base64)`.
fn simulate(url: &str, contract: &str, function: &str, args: Vec<stellar_xdr::ScVal>) -> Fallible<(u64, String)> {
    let envelope = simulation_envelope(invoke_contract_op(contract, function, args)?)?;
    let result = rpc(url, "simulateTransaction", &json!({ "transaction": to_base64(&envelope)? }))?;
    if let Some(error) = result.get("error") {
        return Err(format!("simulating {function}: {error}").into());
    }
    let ledger = result["latestLedger"].as_u64().ok_or("latestLedger missing")?;
    let value = result["results"][0]["xdr"].as_str().ok_or("simulation returned no value")?.to_string();
    Ok((ledger, value))
}

/// The pool's oracle and reserve list, from its instance and `ResList`.
fn pool_shape(url: &str, pool: &str) -> Fallible<(Vec<(String, String)>, String, Vec<String>)> {
    let instance_key = to_base64(&keys::instance(pool)?)?;
    let reserve_list_key = to_base64(&keys::reserve_list(pool)?)?;
    let (_, entries) = ledger_entries(url, &[instance_key.clone(), reserve_list_key.clone()])?;
    // The RPC does not promise to return entries in the order they were
    // asked for, so every lookup goes through the key.
    let instance = decode::pool_instance(&decode::entry_from_base64(entry_for(&entries, &instance_key)?)?)?;
    let assets = decode::reserve_list(&decode::entry_from_base64(entry_for(&entries, &reserve_list_key)?)?)?;
    Ok((entries, instance.config.oracle, assets))
}

/// Every ledger entry the fixture holds, in a stable order.
fn all_entry_keys(pool: &str, assets: &[String], users: &[String]) -> Fallible<Vec<String>> {
    let mut key_base64 = vec![to_base64(&keys::instance(pool)?)?, to_base64(&keys::reserve_list(pool)?)?];
    for asset in assets {
        key_base64.push(to_base64(&keys::reserve_config(pool, asset)?)?);
        key_base64.push(to_base64(&keys::reserve_data(pool, asset)?)?);
    }
    for user in users {
        key_base64.push(to_base64(&keys::positions(pool, user)?)?);
    }
    Ok(key_base64)
}

/// Finds an entry by its key, since the RPC may reorder or omit entries.
fn entry_for<'a>(entries: &'a [(String, String)], key: &str) -> Fallible<&'a str> {
    entries
        .iter()
        .find(|(entry_key, _)| entry_key == key)
        .map(|(_, xdr)| xdr.as_str())
        .ok_or_else(|| format!("the ledger has no entry for key {key}").into())
}

fn recent_events(url: &str, pool: &str, oldest: u64, ledger: u64) -> Fallible<Vec<Value>> {
    let start = oldest.max(ledger.saturating_sub(120_000));
    let mut events = Vec::new();
    for (name, arity) in EVENT_TOPICS {
        let mut topics = vec![to_base64(&symbol(name)?)?];
        topics.extend(std::iter::repeat_n("*".to_string(), arity - 1));
        let result = rpc(
            url,
            "getEvents",
            &json!({
                "startLedger": start,
                "filters": [{"type": "contract", "contractIds": [pool], "topics": [topics]}],
                "pagination": {"limit": 20}
            }),
        )?;
        let found = result["events"].as_array().cloned().unwrap_or_default();
        println!("  {name}: {} event(s) since ledger {start}", found.len());
        events.extend(found.into_iter().take(EVENTS_PER_TOPIC));
    }
    Ok(events)
}

/// One capture attempt. Returns `Ok(None)` when the ledger moved under it.
fn attempt(url: &str, pool: &str, users: &[String]) -> Fallible<Option<Value>> {
    let (_, oracle, assets) = pool_shape(url, pool)?;
    let key_base64 = all_entry_keys(pool, &assets, users)?;
    let (_, first_pass) = ledger_entries(url, &key_base64)?;

    let mut ledgers = Vec::new();
    let (ledger, oracle_decimals) = simulate(url, &oracle, "decimals", Vec::new())?;
    ledgers.push(ledger);

    let mut reserves = Vec::new();
    // Each `get_reserve` return already carries the ledger's own timestamp
    // in `data.last_time` — see the module doc comment for why that, and
    // not a separate `getHealth` call, is the fixture's accrual target.
    let mut accrual_times = Vec::new();
    for asset in &assets {
        let (get_reserve_ledger, get_reserve) = simulate(url, pool, "get_reserve", vec![address(asset)?])?;
        let (price_ledger, lastprice) = simulate(url, &oracle, "lastprice", vec![stellar_asset(asset)?])?;
        ledgers.push(get_reserve_ledger);
        ledgers.push(price_ledger);
        let reserve = decode::reserve_value(&from_base64(&get_reserve)?)?;
        accrual_times.push(reserve.data.last_time);
        reserves.push(json!({
            "asset": asset,
            "config_entry_xdr": entry_for(&first_pass, &to_base64(&keys::reserve_config(pool, asset)?)?)?,
            "data_entry_xdr": entry_for(&first_pass, &to_base64(&keys::reserve_data(pool, asset)?)?)?,
            "get_reserve_return_xdr": get_reserve,
            "lastprice_return_xdr": lastprice,
        }));
    }

    let mut user_entries = Vec::new();
    for user in users {
        let (positions_ledger, get_positions) = simulate(url, pool, "get_positions", vec![address(user)?])?;
        ledgers.push(positions_ledger);
        user_entries.push(json!({
            "account": user,
            "positions_entry_xdr": entry_for(&first_pass, &to_base64(&keys::positions(pool, user)?)?)?,
            "get_positions_return_xdr": get_positions,
        }));
    }

    if ledgers.iter().any(|other| *other != ledger) {
        println!("  the ledger moved during the simulations ({ledgers:?})");
        return Ok(None);
    }
    let (_, second_pass) = ledger_entries(url, &key_base64)?;
    if second_pass != first_pass {
        println!("  a ledger entry changed between passes");
        return Ok(None);
    }

    // The fixture's accrual target: every reserve's own `last_time`, which
    // must agree since they all came from simulations against one ledger.
    let close_time = *accrual_times
        .first()
        .ok_or("the pool has no reserves; there is no accrual target")?;
    if accrual_times.iter().any(|&other| other != close_time) {
        println!("  reserves disagree on their accrual time ({accrual_times:?})");
        return Ok(None);
    }

    let health = rpc(url, "getHealth", &json!({}))?;
    let oldest = health["oldestLedger"].as_u64().ok_or("oldestLedger missing")?;
    let events = recent_events(url, pool, oldest, ledger)?;

    Ok(Some(json!({
        "rpc_url": url,
        "pool": pool,
        "ledger": ledger,
        "ledger_close_time": close_time,
        "instance_entry_xdr": entry_for(&first_pass, &to_base64(&keys::instance(pool)?)?)?,
        "res_list_entry_xdr": entry_for(&first_pass, &to_base64(&keys::reserve_list(pool)?)?)?,
        "oracle": oracle,
        "oracle_decimals_return_xdr": oracle_decimals,
        "reserves": reserves,
        "users": user_entries,
        "events": events,
    })))
}

fn main() -> Fallible<()> {
    let arguments: Vec<String> = std::env::args().collect();
    let usage = "usage: capture_fixture <rpc-url> <pool> <out.json> [user...]";
    let url = arguments.get(1).ok_or(usage)?;
    let pool = arguments.get(2).ok_or(usage)?;
    let out = arguments.get(3).ok_or(usage)?;
    let users: Vec<String> = arguments.iter().skip(4).cloned().collect();

    for number in 1..=ATTEMPTS {
        println!("attempt {number}:");
        if let Some(fixture) = attempt(url, pool, &users)? {
            std::fs::write(out, serde_json::to_string_pretty(&fixture)?)?;
            println!("wrote {out} at ledger {} (close time {})", fixture["ledger"], fixture["ledger_close_time"]);
            return Ok(());
        }
    }
    Err(format!("no consistent snapshot after {ATTEMPTS} attempts; the pool may be too busy").into())
}
```

The `close_time` is each `get_reserve` return's `data.last_time`. The pool contract's `Reserve::load` always stamps it with the timestamp of the ledger the simulation ran against, so it is exactly the accrual target the Task 7 cross-check needs, and every reserve in one consistent attempt must report the same value — a disagreement is treated like a moved ledger and the attempt retries. Do not read the close time from a later `getHealth` call: it lands on whatever ledger is newest at that moment, which is usually one past the one the simulations agreed on, and the accrual cross-check then fails by a few seconds of interest. `getHealth` is used only for `oldestLedger`, the floor of the event window.

- [ ] **Step 2: Verify the tool reproduces a working fixture**

Run:

```bash
cargo run --example capture_fixture -- \
  https://mainnet.sorobanrpc.com \
  CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD \
  /tmp/refreshed.json \
  GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE \
  GCIH7OYR6LX6364PGGLAKGMZYLV37EAH6YXZAFK7RY7U4K7625XBH5EL
```

Expected: it prints one or more attempts and finally `wrote /tmp/refreshed.json at ledger …`. Then confirm the accrual cross-check passes against the *fresh* capture, which is the real proof the tool and the maths agree:

```bash
cp tests/fixtures/mainnet-fixed-v2.json /tmp/committed.json
cp /tmp/refreshed.json tests/fixtures/mainnet-fixed-v2.json
cargo test --lib chain::xdr::decode::tests::accruing_the_stored_entries_reproduces_the_contracts_get_reserve
cp /tmp/committed.json tests/fixtures/mainnet-fixed-v2.json
```

Expected: that one test passes against the fresh capture (the other fixture tests will fail on changed literals — that is expected and why the committed fixture is restored immediately). If the accrual test fails against a fresh capture, the close-time question in Step 1's note is the first thing to check; do not change the committed fixture to make it pass.

- [ ] **Step 3: Keep the example out of the image**

Add to `.dockerignore`, after the `tests/` line:

```text
# Example-only sources — not part of the release binary build
examples/
```

- [ ] **Step 4: Update `CLAUDE.md`**

Replace the "Module map" section's body with:

```markdown
- `src/liquidator.rs` — library root: crate-level docs and the error taxonomy
  (`LiquidatorError`).
- `src/config.rs` — CLI and environment configuration (`Args`, `clap`),
  including the strict boolean parser behind `DRY_RUN`.
- `src/main.rs` — binary entry point: tracing setup, argument parsing, exit.
- `src/math/` — the pure port of the pool contract's arithmetic: `fixed`
  (checked rounding), `reserve` (accrual and token conversions), `position`
  (effective values and health factor), `auction` (Dutch-auction scaling).
  Nothing here does I/O and nothing panics.
- `src/chain/xdr/` — ScVal codecs for the pool: `encode` (values, operations,
  simulation envelopes), `keys` (ledger keys, durability included), `decode`
  (entries and view-call returns), `events` (pool events).
- `examples/capture_fixture.rs` — refreshes `tests/fixtures/` from a live
  RPC through `curl`. See that directory's README.

The module layout beyond this follows
`docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`; the
phases still to land are the RPC client and pool reads, the store and ledger
poller, the auctioneer, the filler and executor, unwind, and the operational
surface.
```

Add to the "Safety invariants a change must not break" section:

```markdown
- **The maths agrees with the contract, and a fixture proves it.**
  `tests/fixtures/mainnet-fixed-v2.json` holds one mainnet ledger's entries
  *and* the contract's own answers at that ledger. Accruing the entries must
  reproduce `get_reserve` to the stroop, and valuing the positions must
  reproduce the health factors. If one of those tests fails, the port is
  wrong or the contract changed — never edit the expectation to match the
  code.
```

Add to "Gotchas":

```markdown
- Money is `i128` in each asset's own decimals, but the scales differ by
  field: v2 rates (`b_rate`, `d_rate`) are 12 decimals, factors and
  utilisation are 7, prices are in the oracle's own decimals (7 on the
  mainnet pools, but read it, don't assume it). Mixing two of those silently
  produces a number that looks plausible.
- Storage durability is part of a ledger key. Auctions live in *temporary*
  storage; everything else the bot reads is persistent. Asking for an auction
  with the persistent durability returns no entry rather than an error, which
  reads exactly like "no auction exists".
```

- [ ] **Step 5: Update `CHANGELOG.md`**

Under `## [Unreleased]` / `### Added`, add these entries above the existing ones:

```markdown
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
- `cargo run --example capture_fixture` refreshes that snapshot from a live
  RPC over `curl`, retrying until every entry and simulation describes one
  ledger.
```

- [ ] **Step 6: Run everything CI runs**

Run: `make check`
Expected: fmt clean, clippy clean, all tests pass, docs build with `-D warnings`, invariants and shellcheck pass. If the dev container runs short of memory while linking, use the documented cap:

```bash
CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo test --lib --bins
```

- [ ] **Step 7: Commit and open the pull request**

```bash
git add examples/capture_fixture.rs .dockerignore CLAUDE.md CHANGELOG.md tests/fixtures/
git commit -m "docs: fixture capture tool, module map and changelog for phase 1

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
git push -u origin spec/bot-design
gh pr create --title "Phase 1: fixed-point maths and XDR codecs" --body "$(cat <<'BODY'
Ports the Blend v2 pool's arithmetic to checked `i128` and adds the XDR
codecs for every ledger entry, view-call result and pool event the bot
reads. No I/O yet: the RPC client is Phase 2.

The load-bearing test is `accruing_the_stored_entries_reproduces_the_contracts_get_reserve`.
`tests/fixtures/mainnet-fixed-v2.json` holds one mainnet ledger's raw
entries *and* the contract's own `get_reserve` and `get_positions` answers
at that same ledger, so accruing the entries has exactly one right answer
and the test asserts it to the stroop. The position valuation is pinned the
same way, against two real borrowers' health factors.

Also here: the design spec and this plan, and
`cargo run --example capture_fixture` to refresh the snapshot.

Spec: `docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`
Plan: `docs/superpowers/plans/2026-09-04-phase-1-math-and-xdr.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---

## Phase 1 completion checklist

Before calling this phase done:

- [ ] `make check` is green.
- [ ] `cargo test --lib` reports every test in `math::*` and `chain::xdr::*` passing.
- [ ] The two fixture cross-checks pass, and neither was weakened to make it pass.
- [ ] `cargo run --example capture_fixture` produces a fresh, self-consistent snapshot.
- [ ] `CLAUDE.md`'s module map names `math/` and `chain/xdr/`, and the fixture invariant is written down.
- [ ] No `unwrap` or `expect` outside `#[cfg(test)]` code, and no `f64` anywhere in this phase.

## What Phase 2 needs from this phase

Phase 2 (the Soroban RPC client and pool reads) consumes, and must not have to change:

- `chain::xdr::keys::*` to build `getLedgerEntries` requests.
- `chain::xdr::encode::{invoke_contract_op, simulation_envelope, to_base64, from_base64, address, stellar_asset}` to build `simulateTransaction` requests.
- `chain::xdr::decode::*` to turn responses into `math` types, and `PoolInstance`/`PoolConfig`/`PriceData`, which Phase 2 may move to `chain::pool` if that reads better once the client exists.
- `chain::xdr::events::{decode_pool_event, PoolEvent}` for the `getEvents` stream.
- `math::Reserve::accrue`, which Phase 2 calls with the ledger close time from `getLatestLedger` after every read.
