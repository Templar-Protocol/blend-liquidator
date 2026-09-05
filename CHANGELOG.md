# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
- The repository itself: a Rust service scaffold for a Blend Protocol liquidation bot, green on its first commit. CI gates `cargo fmt`/`clippy -D warnings`/`test`/`doc -D warnings`, `cargo-deny`, the Docker build, `shellcheck` and the three-way Rust version pin behind one aggregate `CI Summary` check — the single required status check, so adding or renaming a job never needs a ruleset edit. `clippy::pedantic` is warn-level with `unwrap_used = "deny"` from the first commit, which is the cheap moment: retrofitting that onto an existing codebase is not. The dev container pins its base image by digest and its features by exact version, with a lock file CI verifies.
- `DRY_RUN` / `--dry-run`, defaulting to `true`, before there is anything to trade. The parser accepts only the literal strings `true` and `false`; `1`, `yes` and `on` are refused at startup. Every extra spelling is another way into live trading, and the dangerous direction is silent — a `DRY_RUN=yes` read as false would arm the bot while reading, to the operator, like it had been disarmed. This is the invariant most expensive to retrofit: a bot that defaults to live and is made safe-by-default later leaves every existing deployment silently changing behaviour on upgrade.
