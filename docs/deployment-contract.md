# Deployment contract

This is the contract between this repository and whoever deploys it, per
`docs/specs/2026-09-04-blend-liquidator-bot-design.md` §10: guarantees
this repository makes about the image and the binary it runs, and
requirements it places on the deployment around it. Nothing here is
specific to any platform. See `docs/configuration.md` for every setting's
default and bound beyond what is stated below.

## What this repository guarantees

- **Image.** Published to `ghcr.io/templar-protocol/blend-liquidator`
  only on a `v*` tag push. A release tags the image `{{version}}` (the
  leading `v` stripped); a non-prerelease also moves `{{major}}.{{minor}}`
  and `latest` to it, a prerelease (a tag with a `-` suffix) moves
  neither. Whether the package is public is a GitHub package setting,
  not something this repository controls; while it is private, pulling
  it needs a token with `read:packages`.
- **Both build stages pin their base image by digest**, not only by tag,
  so what a tag later resolves to cannot change either base image. The
  runtime stage's apt packages (`ca-certificates`, `procps`) are not
  pinned.
- **The image runs as a non-root user** (`liquidator`, uid 1000).
- **Configuration arrives only through the environment, the pools file
  or inline string** (`POOLS_FILE`/`POOLS_TOML`), **and the optional
  seed file** (`SEED_FILE`). The process writes no filesystem state of
  its own — nothing to persist or back up beyond the database.
- **The HTTP surface is off unless `PORT` or `HTTP_PORT` is set.** Either
  turns it on; `PORT` wins when both are set. It serves exactly three
  routes — `/healthz`, `/livez`, `/metrics` — and binds `127.0.0.1`
  unless `HTTP_BIND_ADDR` says otherwise.
- **Migrations run at startup, only in `loop` mode, under a Postgres
  advisory lock**, so two instances starting together cannot race each
  other applying them. `RUN_MODE=check-config` never migrates — it only
  connects and pings.
- **`RUN_MODE=check-config` is a deploy smoke test.** In order, it:
  connects to and pings the database; validates every configured pool
  against chain; when `FILLER_SECRET_KEY` is set, validates the filler
  account (existence and holding at least `XLM_FEE_RESERVE`) and, with
  `DRY_RUN=false` only, warns for each pool whose supplied primary
  collateral is below its `min_primary_collateral`; and, when Telegram
  is configured, calls `getMe` to prove the bot token works. The
  existence and balance checks fail it only with `DRY_RUN=false`; with
  `DRY_RUN=true` each is a warning. It exits `0` on success and `2` on
  any failure, and changes nothing in either mode — no migration runs
  and no transaction is sent.
- **Exactly five environment variables are ever secret**:
  `FILLER_SECRET_KEY`, `AUCTIONEER_SECRET_KEY`, `DATABASE_URL`,
  `RPC_API_KEY`, `TELEGRAM_BOT_TOKEN`. None of the five is ever read
  from the command line, and at the image's default log filter and at
  `RUST_LOG=debug` none is rendered in a log line: the two signing keys
  render as their public address only, and the other three render as
  `Secret(<redacted>)`. This holds only while `RPC_URL` carries no
  credential. Put a provider's key in `RPC_API_KEY` (with
  `RPC_API_KEY_HEADER`), never in the URL: an RPC call that fails at the
  transport level logs the full URL, and a run of such failures sends it
  to the notification channel, so a provider that only takes a key in its
  URL cannot be used safely with this release.
- **Exit codes**: `0` on graceful shutdown or a passing `check-config`;
  `2` on a configuration error — including a failed `check-config`, a
  command-line parse error, and a database that cannot be reached when
  `loop` starts; `1` on any other fatal error while running; `130` on a
  second `SIGINT`/`SIGTERM`, deliberately immediate.
- **A panic in any task aborts the process** (the image is a release
  build, and the release profile sets `panic = "abort"`): no unwind and
  no graceful shutdown. The process is killed by a signal rather than
  exiting with a code — `SIGABRT` (`134`) ordinarily; as a container's
  PID 1, where the kernel discards a self-sent `SIGABRT`, glibc's
  `abort()` ends in a fault signal instead, whose number depends on the
  architecture (`SIGTRAP`, `133`, on arm64). Treat any death by signal as
  a crash.
- **Two exits skip draining notifications still in flight**: `130` and a
  panic's abort.
- **Logs go to stdout**, one line per event; `LOG_FORMAT=json` renders
  each as one JSON object per line instead of text. A command-line parse
  error and a panic's message go to stderr as plain text, whatever
  `LOG_FORMAT` says.
- **Two overlapping instances cannot both act on one liquidation.**
  Stellar accepts one transaction per account sequence number: of two
  submissions built from the same signing account, one lands and the
  other fails with a bad sequence and is never resent. The loser clears
  its own state and re-decides from a fresh chain read on its next
  pass — the filler drops the stale plan outright, the auctioneer
  re-flags the borrower for its next tick — the same thing either would
  do a tick later regardless.

## What a deployment must provide

- **A Postgres database the process can run DDL on.** `loop` mode
  migrates it at every startup; nothing else creates the schema.
- **`HTTP_BIND_ADDR=0.0.0.0`**, only where the platform itself routes to
  the container, and only behind an ingress that admits exclusively the
  platform's own health probes and metrics scraper — the three routes
  carry no authentication.
- **A restart probe on `/livez`, never `/healthz`.** `/healthz` answers
  readiness and is expected to fail during an ordinary RPC outage;
  restarting on it turns that outage into a restart loop. `/livez`
  already absorbs the poller's own backoff window.
- **`STARTUP_DELAY_LEDGERS` set above the previous revision's shutdown
  drain, converted from seconds to ledgers, for any deployment that runs
  overlapping revisions.** It defaults to `0`: the image does not keep
  that overlap window empty on its own, and a rolling deploy that never
  sets it can have two revisions submitting at once until the older one
  finishes draining.
- **The image run with `DRY_RUN=true` — its own default — until
  `RUN_MODE=check-config` passes against the real configuration with
  `DRY_RUN=false` and `FILLER_SECRET_KEY` set.** Only that run fails on
  a missing or underfunded filler account, and `check-config` sends no
  transaction in either mode. Only then is turning `DRY_RUN` to `false`
  for that deployment a decision the operator has evidence for.
- **No `RUST_LOG=trace` on a deployment that holds a secret.** Logging at
  TRACE is not audited for secrets.

## Versioning

This is the `0.1.0` contract. A change to any guarantee above is a
breaking change to this contract and must be called out in
`CHANGELOG.md`.
