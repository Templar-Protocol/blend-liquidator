# blend-liquidator

[![CI](https://github.com/Templar-Protocol/blend-liquidator/actions/workflows/ci.yml/badge.svg)](https://github.com/Templar-Protocol/blend-liquidator/actions/workflows/ci.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A liquidation bot for [Blend Protocol](https://blend.capital) lending pools on
[Stellar](https://stellar.org).

> **Status: Phase 4.** The bot validates its configuration, seeds its
> tracked-user set from the [Blend analytics API](https://api.blend.templarfi.org)
> or a static file, and follows every configured pool — applying pool events
> and refreshing borrowers' health factors from chain into a Postgres store.
> Once a tick it now also decides which tracked borrowers are liquidatable
> or owe bad debt, builds the auction the contract should accept, lets the
> contract judge the percent through simulation, and records every
> decision — and, only with a signing key configured and `DRY_RUN=false`,
> creates it on chain. It still fills no auction: nothing pays a bid or
> takes a lot yet. What *is* complete is the scaffolding around it — CI
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
