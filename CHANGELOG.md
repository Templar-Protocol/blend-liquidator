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
  write path (`tx`): build with a five-minute time bound and a ledger bound
  of `[latest_ledger, latest_ledger + TX_POLL_LEDGERS + 1)` — `Prepared`
  carries both, since the lower bound is what lets `Expired` be trusted —
  simulate, restore archived entries, assemble, fee from the p70/p90
  inclusion percentiles floored at `BASE_FEE`/`HIGH_FEE`, sign, send with
  one `TRY_AGAIN_LATER` retry, poll, and classify into succeeded, failed
  with the pool's error code, expired, or unknown. A `NOT_FOUND` is only
  `Expired` when the RPC's retention still reaches back to the transaction's
  lower ledger bound; once retention has moved past it, a `NOT_FOUND`
  proves nothing, so the outcome stays `Unknown` for reconciliation by other
  means (the account's sequence number) instead of being called `Expired` on
  a guess. A `TxBadSeq` at send is its own error, `BadSequence`, because the
  plan behind such a transaction is stale and must be rebuilt. `wait_for`
  polls to that same outcome from a bare hash, sequence and the two ledger
  bounds — the fields an `Unknown` outcome carries — so a submission queue
  can resume a transaction after a restart or a send that timed out without
  resending it; `wait` is `wait_for` on the fields a `Prepared` already
  holds. Only a transient failure (a transport error, or an HTTP 429/5xx) is
  retried while polling; any other error propagates immediately, since
  polling again cannot change what the RPC already said.
- Configuration for the chain: `NETWORK` or `NETWORK_PASSPHRASE`,
  `RPC_URL`, `RPC_API_KEY_HEADER` with `RPC_API_KEY` read from the
  environment only (an empty value counts as absent), `BASE_FEE`,
  `HIGH_FEE`, `TX_POLL_LEDGERS` (minimum 1: the ledger bound is exclusive,
  so a zero window would make every transaction unlandable before it
  starts).
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
