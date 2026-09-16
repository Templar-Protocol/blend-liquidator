# blend-liquidator

[![CI](https://github.com/Templar-Protocol/blend-liquidator/actions/workflows/ci.yml/badge.svg)](https://github.com/Templar-Protocol/blend-liquidator/actions/workflows/ci.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A liquidation bot for [Blend Protocol](https://blend.capital) lending pools on
[Stellar](https://stellar.org).

> **Status: Phase 6a.** The bot validates its configuration, seeds its
> tracked-user set from the [Blend analytics API](https://api.blend.templarfi.org)
> or a static file, and follows every configured pool — applying pool events
> and refreshing borrowers' health factors from chain into a Postgres store.
> Once a tick it decides which tracked borrowers are liquidatable
> or owe bad debt, builds the auction the contract should accept, lets the
> contract judge the percent through simulation, and records every
> creation it decides to make — and, only with a signing key configured
> and `DRY_RUN=false`,
> creates it on chain. It also fills: once a tick it plans a fill for
> every open liquidation auction whose assets its pool configuration
> supports, works out the ledger at which the auction's lot first covers
> its bid plus the pool's profit margin, keeps its own position at or above
> `min_health_factor × HF_SAFETY_MULTIPLIER` while taking one over, records
> every fill it executes — and, only with `DRY_RUN=false` *and*
> `FILLER_SECRET_KEY`, submits it on chain. And it now unwinds: after a
> fill lands, and once at startup, it repays the debt it holds from its
> wallet and withdraws collateral to the wallet — everything but the
> primary asset, and the primary down to `min_primary_collateral` — while
> keeping its health factor at or above the pool's `min_health_factor`
> (see Safety below). What remains is Phase 6b: Telegram notifications,
> metrics, and the `/healthz`/`/livez`/`/metrics` operational surface.
> What *is* complete is the scaffolding around all of it — CI
> gates, lint posture, dev container, release preflight — so the
> liquidation logic lands into a repository that already fails loudly.

## Safety

**This bot is not non-custodial.** It is designed to hold a signing key and
submit transactions itself; that is what a liquidation bot is. Dry-run is the
default for exactly that reason:

- `DRY_RUN` / `--dry-run` defaults to `true`.
- Live trading requires explicitly setting it to `false`. There is no other
  opt-in.
- The value is parsed strictly — exactly `true` or `false`. `1`, `yes` and
  `on` are refused at startup rather than guessed at, because the dangerous
  direction is silent: a value quietly read as false would arm the bot while
  looking, to the operator, like it had been disarmed.
- `DRY_RUN=false` additionally requires `FILLER_SECRET_KEY`. Separately, and
  in every mode, `AUCTIONEER_SECRET_KEY` equal to `FILLER_SECRET_KEY` is
  refused at startup — share one key by leaving the auctioneer's unset. Both
  are read from the environment only: a signing key passed on the command
  line is readable from `/proc/<pid>/cmdline`, `ps` and `docker inspect`.

**A fill unwinds to the wallet and holds; nothing is sold.** After a live
fill lands — and once at startup, for whatever position is already
there — the filler repays the debt it took from its own wallet and
withdraws its collateral to that wallet: everything but the primary
asset, and the primary down to `min_primary_collateral`. It never trades
one asset for another, so profit sits in the wallet as whatever assets the
position happened to hold, not as a single settled currency; converting it
is an operator decision this bot does not make for you. The startup pass
also means the wallet's stated floor, `min_primary_collateral`, doubles as
the most primary collateral you should expect the bot to leave supplied to
a pool — anything above it is trimmed back to the wallet on the very first
tick of a run. Where debt is left behind, the withdrawal stops half a
percent *above* `min_health_factor` rather than on it, so a ledger of
interest on that debt does not carry the position under the minimum you
set with nothing scheduled to look again. It also stops at the pool's own
`min_collateral` — the least collateral the contract lets a position with
debt keep, $5 on the mainnet pools — which can be the binding one when
the debt left behind is small. An unwind that keeps being refused is
backed off rather than retried every ledger, and the third refusal in a
row is reported. A notification failure never blocks or delays any of
this: debt the wallet cannot repay is reported once per pool and trading
continues regardless of whether the report was delivered.

## Quickstart

```bash
cargo run -- --help          # see every flag
cargo run                    # dry-run (the default)
cargo run -- --dry-run=false # LIVE — refuses to be set any other way
```

Or with Docker:

```bash
cp .env.example .env
docker compose up
```

The published image is `ghcr.io/templar-protocol/blend-liquidator:0.1.0`. This
repository is private, so the package is too — pulling it needs a token with
`read:packages`.

## Development

```bash
make db-up     # start Postgres; make check needs it running
make check     # everything CI runs: fmt, clippy, test, doc, invariants, shellcheck
make help      # Docker Compose lifecycle
```

The dev container (`.devcontainer/`) pins its base image by digest and its
features by exact version, installs the toolchain from `rust-toolchain.toml`,
and sets up `shellcheck` and `cargo-deny` so CI's gates are reproducible
locally.

## Layout

| Path | What it is |
|---|---|
| `src/liquidator.rs` | Library root and error taxonomy |
| `src/config.rs` | CLI and environment configuration |
| `src/main.rs` | Binary entry point |
| `scripts/` | Repo-invariant and release preflight checks, review tooling |
| `docs/` | Design specs |

## Licence

[GPL-3.0-only](LICENSE).
