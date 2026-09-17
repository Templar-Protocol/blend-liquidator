# blend-liquidator

[![CI](https://github.com/Templar-Protocol/blend-liquidator/actions/workflows/ci.yml/badge.svg)](https://github.com/Templar-Protocol/blend-liquidator/actions/workflows/ci.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A liquidation bot for [Blend Protocol](https://blend.capital) lending pools on
[Stellar](https://stellar.org).

> **Status: Phase 6 complete.** The bot validates its configuration, seeds its
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
> `FILLER_SECRET_KEY`, submits it on chain. It unwinds: after a
> fill lands, and once at startup, it repays the debt it holds from its
> wallet and withdraws collateral to the wallet — everything but the
> primary asset, and the primary down to `min_primary_collateral` — while
> keeping its health factor at or above the pool's `min_health_factor`
> (see Safety below). And it now reports itself: dependency-free
> Prometheus metrics at `/metrics`, `/healthz`/`/livez` for a deployment's
> readiness and liveness probes, and Telegram notifications alongside the
> log — all optional, and none of it load-bearing for trading (see Running
> it below). What remains is Phase 7 (a local sandbox integration tier
> against deployed pool contracts) and Phase 8 (docs and the first
> release). What *is* complete is the scaffolding around all of it — CI
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
continues regardless of whether the report was delivered. Delivery itself
is fire-and-forget — a decision, a fill or an unwind never waits on
Telegram, or on the log — so a channel that is down, or a burst past the
bounded number of deliveries in flight, costs the operator a delayed or
dropped report and nothing else.

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

## Running it

Setting `PORT` (or `HTTP_PORT`, for a deployment that does not inject
`PORT` — `PORT` wins when both are set) turns on a small HTTP server,
bound to `HTTP_BIND_ADDR` (`127.0.0.1` by default; Cloud Run needs
`0.0.0.0`, since it cannot route to a loopback listener) with three
endpoints:

- `/healthz` — readiness. `200` once every configured pool's processed
  ledger is within `HEALTH_MAX_LAG_LEDGERS` of the chain head this
  process has observed, that head was read recently — within the same
  window `/livez` uses — and the store answers a ping within five
  seconds; otherwise `503` with the reason as plain text. The head's age
  is what makes an RPC outage visible here: both ledger numbers are this
  process's own, an outage stops them together, and a lag that compared
  only the two would sit frozen at zero while the bot followed nothing.
- `/livez` — liveness. `200` while every pool's poller has heartbeated
  recently, independent of whether the RPC is currently answering: the
  window absorbs one worst-case backoff, so an RPC outage the poller is
  already riding out does not fail it. A pool whose poller has not run
  *at all* yet is measured from the process's start instead, and the
  initial seed heartbeats for the pool it is working on, so a first start
  against a busy pool is not read as a poller that has stopped. Only a
  poller that has genuinely stopped making progress fails this.
- `/metrics` — Prometheus text exposition format, prefixed
  `blend_liquidator_`: ledger head and processed per pool, events
  processed, users tracked and auctions open per pool, creation and fill
  attempts by result, skips by reason, estimated profit, reserved
  inventory per asset, unwind passes, and notification delivery counts.
  Always `200`.

**A deployment's restart probe must target `/livez`, never `/healthz`.**
Restarting on every readiness blip would kill and respawn the bot on
exactly the RPC outages its own backoff exists to ride out; a wedged
poller — which only `/livez` catches — is what a restart can actually
fix. The server binds before the initial seed, so `/livez` is reachable
for the whole of a first start. None of this is load-bearing: an unset
`PORT`/`HTTP_PORT` leaves the server off entirely, and a bind failure is
logged and never stops the bot from trading.

One readiness case is expected rather than wrong: when a pool's events
cursor has fallen out of the RPC's retained window, the bot reseeds that
pool, and for the duration of the reseed its processed ledger stands
still while the chain head advances — so `/healthz` answers `503` until
the reseed finishes. That is honest readiness (the bot is not caught up),
and the `EventGap` notification names the cause. Alert on it, but expect
it when a bot has been stopped for longer than the RPC's retention.

Setting both `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` — both or
neither, either alone is a startup error — sends every notification to
that chat as well as the log; leave them unset and every notification is
logged only. The token is read from the environment only, never a
command-line argument, exactly like the signing keys: `TELEGRAM_BOT_TOKEN`
never appears in `--help`, argv, or a rendered config. `check-config`
verifies a configured token with one `getMe` call before anything else
trusts it.

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
