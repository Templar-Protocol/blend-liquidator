# blend-liquidator

[![CI](https://github.com/Templar-Protocol/blend-liquidator/actions/workflows/ci.yml/badge.svg)](https://github.com/Templar-Protocol/blend-liquidator/actions/workflows/ci.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

A liquidation bot for [Blend Protocol](https://blend.capital) lending pools on
[Stellar](https://stellar.org).

> **Status: 0.1.0.** The bot follows every configured Blend v2 pool,
> creates the liquidation auction a tracked borrower's position calls
> for, fills auctions — its own and anyone else's — that its pool
> configuration supports, and unwinds the position a fill leaves it
> holding back to its own wallet. It targets the ADR-0008 security fork
> of the Blend contracts (`Templar-Protocol/blend-contracts-v2`) and
> also runs against stock pools. Dry-run is the default: nothing is
> created or filled on chain until `DRY_RUN=false` is set explicitly.
> Armed, it is custodial — it holds a signing key and submits
> transactions itself, which is the point of a liquidation bot (see
> Safety below). `docs/architecture.md` is the one-sitting tour of how
> the pieces fit together and where the arithmetic and contract facts
> behind a decision live.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — the one-sitting overview:
  what the bot does, how its tasks fit together, and where the arithmetic
  and contract facts behind a decision live.
- [`docs/configuration.md`](docs/configuration.md) — every setting: its
  default, the bound its parser enforces, and what a startup error naming
  it means.
- [`docs/deploy.md`](docs/deploy.md) — the operator's guide, from pulling
  the image to running it armed.
- [`docs/deployment-contract.md`](docs/deployment-contract.md) — what this
  repository guarantees about the image and the binary, and what a
  deployment must provide around it.
- [`docs/testnet-soak.md`](docs/testnet-soak.md) — the design spec's §9
  soak: running the bot dry against Blend's own public-testnet pool, then
  armed against this repository's own throwaway deployment on the same
  network.

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
is an operator decision this bot does not make for you.
`min_primary_collateral` is primary collateral supplied to the pool — the
filler's position there, not its wallet balance — and the startup pass
makes it the most primary collateral you should expect the bot to leave
supplied: once armed, and once `STARTUP_DELAY_LEDGERS` has passed, the
run's first unwind pass trims anything above it back to the wallet. Where
debt is left behind, the withdrawal stops half a percent *above*
`min_health_factor` rather than on it, so a ledger of interest on that
debt does not carry the position under the minimum you set with nothing
scheduled to look again. It also stops at the pool's own
`min_collateral` — the least collateral the contract lets a position with
debt keep, $5 on the mainnet pools — which can be the binding one when
the debt left behind is small. An unwind that keeps being refused is
backed off rather than retried every ledger, and the third refusal in a
row is reported. A notification failure never blocks or delays any of
this: debt the wallet cannot repay is reported once per pool and trading
continues regardless of whether the report was delivered. Delivery itself
is fire-and-forget — a decision, a fill or an unwind never waits on
Telegram, or on the log — and the log is the fallback under it: a
delivery that fails, and one that finds every in-flight permit taken, is
written there instead. So a channel that is down costs the operator a
report read in the log rather than in the chat, and nothing else.

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

This README tracks `ghcr.io/templar-protocol/blend-liquidator:0.1.0`. Each
image is published by pushing its `v<version>` tag (`v0.1.0` for this one),
so a version is pullable only once its tag has been pushed and the release
workflow has run. Whether the package is public is a GitHub package setting, not something
this repository controls; while it is private, pulling it needs a token
with `read:packages`.

## Running it

Setting `PORT` (or `HTTP_PORT`, for a deployment that does not inject
`PORT` — `PORT` wins when both are set) turns on a small HTTP server,
bound to `HTTP_BIND_ADDR` (`127.0.0.1` by default; Cloud Run needs
`0.0.0.0`, since it cannot route to a loopback listener) with three
endpoints. The endpoints carry no authentication and `/healthz` costs a
store ping per request, so a `0.0.0.0` bind is for a platform whose
ingress admits only its own probes and scraper — never for a public
address.

- `/healthz` — readiness. `200` once every configured pool's processed
  ledger is within `HEALTH_MAX_LAG_LEDGERS` of the chain head this
  process has observed — in either direction, so a head *more than* that
  far behind the processed ledger, which is an RPC node sitting behind this bot's own
  committed cursor, fails too — that head was read recently — within the
  same window `/livez` uses — and the store answers a ping within five
  seconds; otherwise `503` with the reason as plain text. The head's age
  is what makes an RPC outage visible here: both ledger numbers are this
  process's own, an outage stops them together, and a lag that compared
  only the two would sit frozen at zero while the bot followed nothing.
- `/livez` — liveness. `200` while every pool's poller has heartbeated
  recently, independent of whether the RPC is currently answering: the
  window absorbs one worst-case backoff, so an RPC outage the poller is
  already riding out does not fail it. A pool whose poller has not run
  *at all* yet is measured from the process's start instead, and the
  initial seed heartbeats for every configured pool while it runs, so a
  first start against a busy pool is not read as a poller that has
  stopped — including the pools the seed has not reached yet. Only a
  poller that has genuinely stopped making progress fails this.
- `/metrics` — Prometheus text exposition format, prefixed
  `blend_liquidator_`: ledger head and processed per pool, events
  processed, users tracked and auctions open per pool, creation and fill
  attempts by result, skips by reason, estimated profit and estimated
  loss, reserved
  inventory per asset, unwind passes, and notification delivery counts.
  Always `200`.

**A deployment's restart probe must target `/livez`, never `/healthz`.**
Restarting on every readiness blip would kill and respawn the bot on
exactly the RPC outages its own backoff exists to ride out; a wedged
poller — which only `/livez` catches — is what a restart can actually
fix. The server binds before the initial seed — after the store
connects, migrates and the configuration is validated, which is the only
part of a start it is not up for. None of this is load-bearing: an unset
`PORT`/`HTTP_PORT` leaves the server off entirely, and a bind failure is
logged and never stops the bot from trading.

One readiness case is expected rather than wrong: when a pool's events
cursor has fallen out of the RPC's retained window, the bot reseeds that
pool, and its poller waits for that reseed to be applied before it polls
again — so for the reseed's duration it reads no chain head at all and
processes no ledger. `/healthz` answers `503` once the wait passes the
window, with `no chain head read for Ns` as the body rather than a lag.
That is honest readiness (the bot is not following the chain while it
rebuilds its user set), the poller keeps heartbeating throughout so
`/livez` stays `200`, and the `EventGap` notification names the cause.
Alert on it, but expect it when a bot has been stopped for longer than
the RPC's retention.

Setting both `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` — both or
neither, either alone is a startup error — makes that chat the delivery
channel: a notification goes there, and to the log only when the delivery
fails or a burst has taken every in-flight permit. Leave them unset and
the log *is* the channel, and every notification goes there. The token is read from the environment only, never a
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
and sets up `shellcheck`, `cargo-deny` and the `stellar` CLI so CI's gates
are reproducible locally. It also caps cargo's build parallelism against the
container's own memory limit (`scripts/cargo-jobs.sh`), since `nproc` inside
a container reports the host's cores and a cold build fanned out that wide
is how you meet the OOM killer.

## Testing

Three tiers, and only the first two run on a pull request.

**Unit tests**, inline in every module and run by `make check`. The pure
arithmetic is pinned against `tests/fixtures/mainnet-fixed-v2.json`, one
mainnet ledger's entries together with the contract's own answers at that
ledger: accruing the stored reserves must reproduce `get_reserve` to the
stroop, and decoding the stored positions must reproduce `get_positions`.
The store's tests need a live Postgres and are not skipped without one
(`make db-up` first) — `#[sqlx::test]` creates a database per test.

**The scripted RPC server** (`src/chain/script.rs`), which the chain tests
drive the *real* client through: the restore path, `TRY_AGAIN_LATER`, a
timeout and every decoded contract error, with no network and nothing
mocked below the wire format.

**The sandbox**, a throwaway Stellar network in Docker with Blend v2
deployed on it and this binary run against it **armed** — one of the two
places anything in this repository signs and sends a transaction; the
other is the testnet soak's armed stage (`docs/testnet-soak.md`), on
public testnet with friendbot's XLM. Five scenarios
(`liquidation`, `check_config`, `dry_run`, `unwind_repay`,
`restart_adopt` — see `tests/liquidation_sandbox.rs`'s module doc for what
each proves):

```bash
make sandbox                                # all five, about 25 minutes
SANDBOX_SCENARIO=liquidation make sandbox   # just one
make sandbox-down                           # tear it down by hand (SANDBOX_KEEP=1 left it up)
```

`make sandbox` stops at the first scenario that fails, leaving its
database and logs to inspect. `SANDBOX_KEEP=1` leaves the network up
too, and so takes exactly one scenario (`SANDBOX_SCENARIO=x`).

It needs Docker, the `stellar` CLI (the dev container installs it; it is
pinned and checksum-verified from `scripts/sandbox/versions.env`), and a
Postgres at `DATABASE_URL` — each scenario creates its own databases per
run (`restart_adopt` one per bot) and migrates them, except
`check_config`, which leaves its one database unmigrated because it
asserts that `check-config` never migrates it. Everything it deploys
comes from wasm pinned by SHA-256, and every script refuses to proceed
unless the RPC's own `getNetwork` answers the standalone network's
passphrase, so none of it can be pointed at a public network. The keys it generates are funded by friendbot — with
a retry while `deploy.sh` waits, since the network's own health gate can
go green before friendbot behind it is ready to fund an account — and
belong to a network that is gone the moment it is torn down; the bot's
copy of the filler's key lives in `target/sandbox/sandbox.env` at mode
`0600`, and the teardown deletes it.

The same five runs are the nightly `Sandbox` workflow (`schedule` and
`workflow_dispatch` only — never a pull request, so it is deliberately
outside the `CI Summary` gate), one matrix job per scenario on its own
runner. `liquidation`'s deploy leaves a borrower at a health factor of
~1.19 and `crash.sh` takes it to ~0.89 with a 25% move in XLM's price;
the bot then creates the borrower's liquidation auction, fills it and
unwinds the position it took, asserted through the audit tables' own
transaction hashes, the filler's on-chain position and `/metrics`. The
other four prove `check-config`'s exit codes and warnings, a wrong
`NETWORK_PASSPHRASE`'s included, without ever sending a transaction or
migrating the database; that a dry-run bot holding a real signing key
never sends a transaction, whether deciding to create an auction or to
fill one — shown by the key's sequence number, the chain and the audit
rows; that the unwind's repay branch clears debt a
fill's own repay could not cover, once the wallet is funded and a second
bot restarts; and that a bot `SIGKILL`ed right after creating an auction
is followed by a fresh instance that adopts and fills that same auction
rather than trying to create a second one. What it catches is the world
moving — a quickstart image, a pinned wasm, a contract that changed its
mind — rather than a diff being wrong, which is what the pull-request
gate is for.

## Layout

| Path | What it is |
|---|---|
| `src/liquidator.rs` | Library root and error taxonomy |
| `src/config.rs` | CLI and environment configuration |
| `src/main.rs` | Binary entry point |
| `scripts/` | Repo-invariant and release preflight checks, review tooling, the sandbox tier |
| `tests/` | Fixtures, and the sandbox tier's five scenario tests |
| `docs/` | Guides and reference (see Documentation above) and design specs |

## Licence

[GPL-3.0-only](LICENSE).
