# blend-liquidator

## What this is

A liquidation bot for [Blend Protocol](https://blend.capital) lending pools on
Stellar. It is intended to repay the debt of underwater positions and receive
their collateral at a discount.

**Status: skeleton.** Phase 1 landed the pure fixed-point math (`math`) and
the ScVal/ledger-entry codecs (`chain::xdr`); Phase 2 landed the chain layer
(`chain::rpc`, `chain::pool`, `chain::signer`, `chain::tx`), which can read a
pool and sign and submit a transaction but is not yet driven by anything.
The binary itself still just parses configuration, sets up logging and
exits: there is no bot loop or executor yet. The repository scaffolding is
complete and enforced.

**This bot is NOT non-custodial.** It is designed to hold a signing key and
submit transactions itself — that is the point of a liquidation bot. Treat
that key with the weight it implies. Dry-run is the default for exactly this
reason (see Safety invariants below).

## Orientation commands

```bash
make check                          # everything CI runs
cargo test --lib --bins             # unit tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all
make help                           # Docker Compose lifecycle
```

## Module map

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
- `src/chain/rpc.rs` — the Soroban JSON-RPC client: the eight methods the
  bot uses, their wire shapes, base64 XDR decoded at the boundary. Every
  result carries the ledger it was taken at.
- `src/chain/pool.rs` — pool reads: a single-ledger `PoolSnapshot` (instance,
  reserves, prices, positions) that `position_data` values with `math`,
  auction and balance reads, and the `submit`, `new_auction` and `bad_debt`
  operation builders.
- `src/chain/signer.rs` — the network id and the Ed25519 key. `Signer`
  renders as its address only.
- `src/chain/tx.rs` — the one write path: build with time and ledger bounds,
  simulate, restore archived entries, assemble, fee, sign, send, poll,
  classify into `TxOutcome`.
- `src/chain/script.rs` (`cfg(test)`) — a scripted JSON-RPC server the chain
  tests drive the real client through.
- `examples/pool_snapshot.rs` — prints a live pool's reserves and users'
  health factors.
- `examples/capture_fixture.rs` — refreshes `tests/fixtures/` from a live
  RPC through `curl`. See that directory's README.

The module layout beyond this follows
`docs/superpowers/specs/2026-09-04-blend-liquidator-bot-design.md`; the
phases still to land are the store and ledger poller, the auctioneer, the
filler and executor, unwind, and the operational surface.

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
- `getLedgerEntries` omits absent keys rather than returning nulls, so a
  lookup must go by key, never by position, and "the RPC returned fewer
  entries than keys" is the normal shape of "some of these do not exist".

## Workflow

1. Branch → PR against `main`.
2. CI must be green: fmt, clippy, unit tests, docs, `cargo-deny`, Docker build,
   invariants, shellcheck.
3. Unresolved review threads block the merge; there are no required approvals.
4. Releases are tags `vX.Y.Z`, which publish a GHCR image and a GitHub Release.

## Where things live

- `src/` — the crate (binary `liquidator`, lib root `src/liquidator.rs`).
- `scripts/` — repo-invariant and release preflight checks, review tooling.
- `docs/` — design specs.
- `.github/workflows/` — CI and release automation.
