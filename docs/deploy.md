# Deploy guide

This is the operator's path from pulling the image to running it armed.
It is a procedure, not a reference: `docs/configuration.md` is every
setting's default and bound, and `docs/deployment-contract.md` is what
this repository guarantees about the image and what a deployment must
provide around it. Read those alongside this — this document links to
them rather than repeating their tables.

**This bot holds a signing key and submits transactions that move money.
It is not non-custodial.** `DRY_RUN` defaults to `true`, and the only way
to opt into live trading is setting it to the exact literal string
`false` together with `FILLER_SECRET_KEY` — see "5. Arm it" below before
you do either.

## 1. Getting the image

The image is published to `ghcr.io/templar-protocol/blend-liquidator`
only on a `v*` tag push (`.github/workflows/release.yml`), tagged with
the version the tag names — its leading `v` stripped, so the git tag
`v0.1.0` publishes as image tag `0.1.0`, never `v0.1.0`
(`docs/deployment-contract.md`, "What this repository guarantees"). This
repository is private, so the package is private too: pulling it needs a
token with the `read:packages` scope.

```bash
echo "$GHCR_TOKEN" | docker login ghcr.io -u <github-username> --password-stdin
docker pull ghcr.io/templar-protocol/blend-liquidator:<version>
```

Pin a specific version tag — never `:latest`. `:latest` moves to
whichever non-prerelease release most recently pushed it, so a plain
`docker pull`/restart of a container running `:latest` can silently
change what is deployed underneath an unrelated restart. A version tag
is what stays put. Check `CHANGELOG.md` — or the repository's GitHub
Releases page — for the version you want and what changed since the one
you are running.

## 2. Configure it

Everything the process reads arrives through the environment, and the
pools it follows through `POOLS_FILE` or `POOLS_TOML` — nothing else on
disk, and nothing it writes back (`docs/deployment-contract.md`, "What
this repository guarantees"). The required minimum to start it in any
mode:

- One of `NETWORK` (`mainnet` or `testnet`) or `NETWORK_PASSPHRASE`.
- `RPC_URL`, a Soroban RPC endpoint.
- `DATABASE_URL`, a Postgres instance the process can run DDL on — `loop`
  mode migrates it at every startup, under an advisory lock, so nothing
  else needs to create the schema first.
- A pools file (`POOLS_FILE`, mounted into the container) or its inline
  contents (`POOLS_TOML`), naming at least one pool. `pools.example.toml`
  is an annotated, parseable starting point.

Everything else — the health-factor thresholds, the filler's profit and
safety margins, the HTTP and Telegram surfaces — has a default and is
documented in `docs/configuration.md`. `.env.example` is the same set as
a copy-paste starting point, in the order a fresh deployment is likely to
touch them.

Secrets never go on the command line or into a `command:`/`args:` array:
`DATABASE_URL`, `RPC_API_KEY`, `TELEGRAM_BOT_TOKEN` and the two signing
keys are read from the process environment only, because argv is
world-readable (`/proc/<pid>/cmdline`, `ps`, `docker inspect`). Set them
through your platform's own secret-injection mechanism (a Kubernetes
`Secret`, Cloud Run's `--set-secrets`, and so on), not a plain
environment block a config dump would echo back.

## 3. Smoke-test it

Before trusting a configuration with real traffic, run the image once
with `RUN_MODE=check-config`. It connects to and pings the database,
validates every configured pool against chain (each pool answers, they
share one backstop, the configured assets are reserves, `max_positions`
is at least 2), validates the filler account when a key is configured,
and — when Telegram is configured — calls `getMe` to prove the bot token
works. It changes nothing on either path: no migration runs and no
transaction is sent.

```bash
docker run --rm --env-file .env \
  -e RUN_MODE=check-config \
  ghcr.io/templar-protocol/blend-liquidator:<version>
```

A pass logs the resolved configuration (redacted — no secret value is
ever printed) and every warning it found, then exits `0`. A failure logs
the reason at `ERROR` and exits `2` — the same code a bad signing key or
any other startup configuration problem uses. Run this again after any
change to the environment or the pools file, and always before the first
time a deployment sets `DRY_RUN=false`.

## 4. Run it dry

`DRY_RUN` defaults to `true`, so a plain `RUN_MODE=loop` start is already
the safe path: the bot follows the configured pools, decides which
borrowers are liquidatable, plans the auctions it would create and the
fills it would take, and records every one of those decisions — but
submits nothing.

Run it dry first, against the real configuration, for long enough to see
it make a decision — a busy pool costs a real seed pass, which the log
shows starting and finishing. What to watch:

- **Logs.** The first line names whether it is armed
  (`dry_run = <bool>` and either "dry-run: no transaction will be
  submitted" or "LIVE: transactions will be submitted"). After that,
  watch for a decision naming a borrower, a planned liquidation percent,
  and a planned fill — the same log lines an armed run would produce, one
  step earlier.
- **`/metrics`**, if `PORT` or `HTTP_PORT` is set: `creations_total` and
  `fills_total` count dry-run attempts exactly like armed ones (the
  `result` label is `attempted` either way — dry-run never reaches
  `succeeded`/`failed`, since nothing was sent), so a nonzero
  `creations_total{result="attempted"}` on a pool with an unhealthy
  borrower is the dry run doing its job. `skips_total{reason=...}`
  explains every borrower it declined to act on.
- **`/healthz` and `/livez`**, if the HTTP server is on: both should
  settle to `200` once every configured pool's poller has caught up to
  chain head.

A dry run's decisions are still written to the store's `creations` and
`fills` audit tables (with `dry_run = true`), so what it would have done
is durable and reviewable, not just a log line that scrolled past.

## 5. Arm it

Arming means the bot signs and sends. Do this only once a dry run's
decisions look right and `check-config` passes against the real
configuration. `docs/deployment-contract.md` makes that ordering a
requirement of the deployment, not a guarantee the image enforces on its
own: nothing in `src/` stops `DRY_RUN=false` from being set before
`check-config` has ever run against that configuration — this is the
operator's own line to hold, and evidence to have in hand before crossing
it.

**Two things, together, and nothing else opts in:**

- `DRY_RUN=false` — the literal string `false`, exactly. Every other
  spelling (`1`, `yes`, `on`, an empty string) fails to parse rather than
  being read as true or false; there is no silent way into live trading.
- `FILLER_SECRET_KEY` — the filler signs fills with its own key only,
  never the auctioneer's, so `DRY_RUN=false` without it is refused at
  startup (`docs/configuration.md` §1).

`AUCTIONEER_SECRET_KEY` is optional. Leave it unset and the auctioneer
signs auction-creation transactions with the filler's key too, through
the one submission queue that key needs — the ordinary deployment. Set
it only when you want the two roles on separate keys, and note the two
keys are refused if they are the same value.

**Fund the filler account's wallet before arming:** at least
`XLM_FEE_RESERVE` (default 50 XLM) of native XLM, held back from
everything else the filler spends. `check-config` and the run's own
startup validation both check this wallet balance, and it is a startup
*error* once armed (a warning in dry-run).

**`min_primary_collateral` is a separate check, on a different quantity,
and funding the wallet does not satisfy it.** Once armed, `check-config`
and startup also warn — never an error, either way — when each
configured pool's primary asset, *already supplied to that pool as
collateral*, is below the pool's own `min_primary_collateral`. This reads
the filler's on-chain position (b-tokens converted to underlying), not
its wallet balance, so sending the wallet more of the primary asset does
nothing to clear this warning by itself. The bot never tops the position
up to reach the floor on its own: a fill's own supply step sizes itself
to whatever that fill's post-fill health floor needs, not to
`min_primary_collateral`, and the unwind pass only ever trims the
primary asset supplied to a pool *down* to `min_primary_collateral` —
never up. So an operator who wants a standing buffer supplies it to the
pool directly, outside the bot, and sets `min_primary_collateral` to
exactly what they mean the bot to keep supplied there: anything supplied
above that floor is trimmed back to the wallet on the run's first tick.

**`STARTUP_DELAY_LEDGERS`** is the window in which the bot plans and
records but does not submit. The auctioneer and the filler each hold
their own gate for it, each counted in ledgers from the first one that
task itself saw, so the two are not necessarily in lockstep at startup.
It defaults to `0` — an operator setting, not something the image chooses
for you (`docs/deployment-contract.md`, "What a deployment must
provide"). Set it deliberately in two situations:

- **A poller catching up on a backlog.** A bot pointed at a pool it has
  not followed recently re-seeds and replays a range of history before
  its view of chain state is current; a nonzero delay gives that catch-up
  room to finish before the bot acts on health factors it has not yet
  re-verified against current chain state.
- **A rolling deploy that runs overlapping revisions** (a new container
  starts before the old one has fully drained). Set it above the old
  revision's shutdown drain time (`stop_grace_period` in Compose, the
  platform's own termination grace period elsewhere), so the two
  revisions are never both submitting for the same pools at once. The
  image does not keep that window empty on its own at the default of `0`.

## 6. Observe it

**Probes**, when `PORT` or `HTTP_PORT` is set (either turns the HTTP
server on; `PORT` wins when both are set):

- **`/livez`** is what a restart probe targets. It answers `200` while
  every pool's poller has heartbeated recently, in a window that already
  absorbs the poller's own worst-case backoff, so an RPC outage the
  poller is already riding out does not fail it — only a poller that has
  genuinely stopped making progress does.
- **`/healthz`** is readiness, not a restart signal: it answers `503`
  during an ordinary RPC hiccup, which is expected and recovers on its
  own. Restarting on it turns that hiccup into a restart loop.
- **`/metrics`** always answers `200` and renders whatever the process
  holds, empty or not — safe to scrape before the bot has made a single
  decision.

**Metrics**, all prefixed `blend_liquidator_`: per-pool ledger head and
processed ledger and poller heartbeat, events processed, users tracked
and auctions open; `creations_total`/`fills_total` by result
(`attempted`/`succeeded`/`failed`); `skips_total` by reason;
`estimated_profit_total`/`estimated_loss_total` in the pool oracle's own
units; `reserved_inventory` per asset; unwind passes; and notification
delivery counts by kind. `docs/configuration.md` §8 has the HTTP
variables that turn this on; the metric families themselves are defined
in `src/metrics.rs`.

**Telegram**, when both `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` are
set (both or neither — either alone is a startup error): notifications go
to that chat, and to the log only when delivery fails or a burst has
taken every in-flight delivery permit. A notification failure never
blocks or delays trading — delivery is fire-and-forget.

**The one thing never to do: run a Telegram-configured bot at
`RUST_LOG=trace`.** The bot token sits in the Telegram Bot API request
*path* (`/bot<token>/sendMessage`), never a header or the body, and every
error this bot displays near the Telegram client is already scrubbed of
it (`reqwest::Error::without_url`) — but at `TRACE`, hyper's own
byte-level request logging prints the request line, token included,
before this bot's own code ever sees it, and `tracing_subscriber`'s `log`
bridge captures that line into your logs regardless. The default filter
and `RUST_LOG=debug` are both clear of this. If you need `TRACE`-level
diagnostics for something else, do it against a deployment with no
Telegram credentials configured.

## 7. Upgrade it

An upgrade is a new image tag through the same path as the first deploy:
pull the version you want, run `check-config` against it before rolling
it out if anything in the configuration changed, and read
`CHANGELOG.md` for what changed in the versions between the one you are
running and the one you are moving to — the deployment contract's own
"Versioning" section states that any change to a guarantee it lists is
called out there.

For a rolling deploy specifically, see `STARTUP_DELAY_LEDGERS` in
"5. Arm it" above: set it above the old revision's shutdown drain so the
new revision is not submitting for a pool the old one might still be
finishing.

**`fill_objective`'s default makes fills wait for the free-fill point.** A
pool with no `fill_objective` set in its `[[pools]]` table aims fills at
`"free-fill"` — the ledger the auction's bid has ramped away to nothing,
the most the auction can pay, and the last ledger to get it. If you are
used to an earlier deployment of this bot that filled sooner, set
`fill_objective = "earliest-profitable"` on a pool to keep that timing:
filling at the first ledger the lot covers the bid plus that pool's
profit margin, trading profit for landing where competition for the
auction is real rather than waiting out the whole ramp. See
`docs/configuration.md` §4 for the exact trade-off and error text.

## 8. Locally

`docker compose up` (after `cp .env.example .env`) runs the bot against a
local Postgres for development. It is **not a production reference**:
`docker-compose.yml`'s `DATABASE_URL` uses development-default credentials
(`liquidator`/`liquidator`) and Postgres is published to `127.0.0.1`
only — loopback, not a routable address, but still a plaintext local
credential rather than anything you'd carry into a real deployment. A
real deployment passes `DATABASE_URL` through the environment, never
through a file compose can render in full with `docker compose config`.
