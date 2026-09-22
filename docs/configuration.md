# Configuration reference

`.env.example` is the copy-paste starting point: every variable this bot
reads, in the order a fresh deployment is likely to touch them, each with a
one-line comment. This document is the reference underneath it — every
variable's exact default, the bound its parser enforces, and what a startup
error naming it means. Read `.env.example` to get running; read this when a
value it suggests is not the one you want, or when the bot refuses to start
and you need to know why.

Some of these settings change what the bot does with funds it controls, not
just how it logs or where it listens. `DRY_RUN` and the two signing keys
(§1, "Safety first"), the health-factor thresholds (each pool's
`min_health_factor` in §4, `LIQ_HF_THRESHOLD` and `SCAN_HF_THRESHOLD` in
§5, `HF_SAFETY_MULTIPLIER` in §7) and `TARGET_HF` (§6) are that group —
read their sections before changing any of them on a deployment that
holds a key.

Every variable here is read once, at startup, by `Args` (`src/config.rs`)
or, for the five secrets and `RUST_LOG`, straight from the process
environment. There is no reload: changing a value means restarting the
process. An empty string counts as unset everywhere a variable is
described below as optional — except `RUST_LOG`, where an empty value is
a filter with no directives and silences every log line.

## 1. Safety first

`DRY_RUN` defaults to `true`, and there is no other way to opt into live
trading than setting it to exactly `false`. The parser accepts only the
literal strings `true` and `false` — not `1`, `yes`, `on`, `TRUE`, or an
empty string — because every extra spelling is another way into live
trading, and the dangerous direction (a value silently read as `false`) is
the one that must be loud instead. The flag takes an optional value so it
also works as a bare command-line switch: `--dry-run` alone means `true`.

| Variable | Default | Constraint |
|---|---|---|
| `DRY_RUN` | `true` | Only `true` or `false` (exact match) parses. `false` requires `FILLER_SECRET_KEY` to be set. |
| `FILLER_SECRET_KEY` | unset | Secret, read from the environment only — never a `clap` argument, so it never appears in argv. Parsed at startup whether or not it signs anything. |
| `AUCTIONEER_SECRET_KEY` | unset | Secret, same as above. Unset, the auctioneer signs with `FILLER_SECRET_KEY` instead, through the one submission queue that key needs. |
| `STARTUP_DELAY_LEDGERS` | `0` | Ledgers the chain must advance past the first tick a task sees before that task submits anything. The auctioneer and the filler each count their own. |

The two signing keys are read straight from the environment in
`src/main.rs`, never through `clap`, for the same reason `DATABASE_URL` and
`RPC_API_KEY` are: argv is world-readable (`/proc/<pid>/cmdline`, `ps`,
`docker inspect`), and a signing key is the one value in this bot's
configuration that must never appear there. Both are parsed whether or not
they end up signing anything — `SigningKeys::own_addresses` needs both
addresses regardless, because the auctioneer must refuse to build a
liquidation auction against either one, and an address the bot does not
recognize as its own is not a safety property at all.

Two rules bind the pair, both checked once both keys are parsed
(`Args::signing_keys`):

- **`DRY_RUN=false` requires `FILLER_SECRET_KEY`.** The filler signs fills
  with its own key only, never the auctioneer's, so an armed bot without
  it would create auctions and never fill one. The startup error text:
  `DRY_RUN=false needs FILLER_SECRET_KEY: live trading fills auctions, and
  the filler signs with its own key only`.
- **The two keys must not be the same key.** Two roles submitting through
  the one key would need two submission queues on it, which is exactly the
  sequence-number race `src/queue.rs` exists to make unreachable. Sharing
  one key across both roles is spelled by leaving `AUCTIONEER_SECRET_KEY`
  unset, not by setting both variables to the same value. The startup
  error text: `AUCTIONEER_SECRET_KEY and FILLER_SECRET_KEY are the same
  key: leave AUCTIONEER_SECRET_KEY unset and the auctioneer signs with the
  filler's key, through the one queue that key needs`.

`STARTUP_DELAY_LEDGERS` defaults to `0`, so each task may submit from the
first tick it sees. Inside the delay the auctioneer and the filler still
decide and plan, and send nothing. A tick is published only once a
poller has drained its events up to the chain head it read, so the count
starts at chain head. Its use is a rolling deploy that runs two revisions
at once: set above the old revision's shutdown drain, it keeps the new
revision from submitting while the old one still may. The drain is
measured in seconds and this in ledgers — see `docs/deploy.md`, "5. Arm
it", for the conversion.

## 2. Network and RPC

| Variable | Default | Constraint |
|---|---|---|
| `NETWORK` | unset | One of `mainnet` or `testnet`. Give this or `NETWORK_PASSPHRASE`, never both — `clap` refuses both being set at once. |
| `NETWORK_PASSPHRASE` | unset | The network passphrase directly, for any network `NETWORK` does not name. Exactly one of `NETWORK`/`NETWORK_PASSPHRASE` is required; neither set is a startup error (`NETWORK_PASSPHRASE or NETWORK is required`). |
| `RPC_URL` | unset | Required — `RPC_URL is required` if missing. Must carry no credential: a provider's key goes in `RPC_API_KEY` (see below). |
| `RPC_API_KEY` | unset | Secret, read from the environment only. Both this and `RPC_API_KEY_HEADER` or neither: one alone is a startup error. |
| `RPC_API_KEY_HEADER` | unset | The header name the key is sent under. Validated as a well-formed HTTP header name. |
| `BASE_FEE` | `5000` | Inclusion-fee floor for normal-priority transactions, in stroops. |
| `HIGH_FEE` | `10000` | Inclusion-fee floor for high-priority transactions, in stroops (see `HIGH_FEE_PROFIT_THRESHOLD` below). |
| `TX_POLL_LEDGERS` | `3` | How many ledgers a submitted transaction stays valid and is polled for. At least 1, at most 100000. |

`NETWORK=mainnet` resolves to `Public Global Stellar Network ; September
2015`; `NETWORK=testnet` resolves to `Test SDF Network ; September 2015`.
Anything else — a private or future network — needs its passphrase given
directly through `NETWORK_PASSPHRASE`.

`RPC_API_KEY` and `RPC_API_KEY_HEADER` pair the same way the two signing
keys do: `RPC_API_KEY_HEADER` set without `RPC_API_KEY` fails with
`RPC_API_KEY_HEADER is set but RPC_API_KEY is not`, and the reverse fails
with `RPC_API_KEY is set but RPC_API_KEY_HEADER is not`. `RPC_API_KEY`
itself is a secret and, like `DATABASE_URL` and `TELEGRAM_BOT_TOKEN`, is
read from the environment only — it is never a `clap` argument and never
appears in this bot's own argv.

`RPC_URL` is not a secret, and must not carry one: put no credential in
it, and give a keyed provider's key through `RPC_API_KEY` with
`RPC_API_KEY_HEADER` instead. The resolved-configuration line shows only
the URL's origin, but an RPC call that fails at the transport level (DNS,
TLS, a timeout) logs the full URL, and the poller's `RpcFailing`
notification sends it to the notification channel. A provider that only
takes a key in its URL cannot be used safely with this release.

## 3. Store

| Variable | Default | Constraint |
|---|---|---|
| `DATABASE_URL` | unset | Secret, read from the environment only. Required — `DATABASE_URL is required` if missing. |
| `DATABASE_MAX_CONNECTIONS` | `10` | At least 1, at most 100. |

`DATABASE_MAX_CONNECTIONS` must cover every task that queries Postgres
concurrently: one ledger poller per pool, the tracker, the auctioneer and
the filler — roughly `pools + 3`, and the default of `10` covers up to
seven pools. Sizing it below that does not deadlock; it times out
acquiring a connection, and every store error in this bot is fatal, so a
load spike becomes a process exit rather than a slowdown.

## 4. Pools

| Variable | Default | Constraint |
|---|---|---|
| `POOLS_FILE` | unset | Path to the pools file. Give this or `POOLS_TOML`, never both — `clap` refuses both being set. |
| `POOLS_TOML` | unset | The pools file's contents inline, for an environment with no volume to mount. Exactly one of `POOLS_FILE`/`POOLS_TOML` is required — neither set is `one of POOLS_FILE or POOLS_TOML is required`. |

The file (or inline string) is TOML: one `[[pools]]` table per pool the
bot follows, at least one required (`at least one pool (a [[pools]] table)
is required`), and every pool's `address` must be unique (`duplicate pool
<address>`). `pools.example.toml` is an annotated, parseable example.
Every table below rejects a key it does not recognize.

**`[[pools]]` fields:**

| Key | Type | Default | Constraint |
|---|---|---|---|
| `address` | string | required | The pool contract address. |
| `primary_asset` | string | required | The asset the bot keeps as collateral in this pool. |
| `min_primary_collateral` | decimal string | required | Primary-asset collateral to keep supplied to this pool — the filler's position in the pool, not its wallet balance — in the asset's own decimals. The unwind pass withdraws the primary down to it and never supplies to reach it. Parsed as an integer amount (`min_primary_collateral: '<text>' is not an integer amount` on failure) and refused negative (`min_primary_collateral must not be negative`). |
| `min_health_factor` | decimal (7 places) | required | The health factor the filler keeps its own position above after a fill. Must be strictly above `1.00001` (the contract's own post-submit minimum) — at or under it, every failure names the pool: `min_health_factor is at or under the contract's own post-submit minimum (1.00001), so the filler would plan fills the contract refuses as InvalidHf`. |
| `default_profit_bps` | integer | required | Profit required, in basis points, when no `[[pools.profits]]` rule matches. |
| `force_fill` | boolean | `false` | Caps whichever ledger `fill_objective` picks at the 350-ledger mark, however little the lot covers by then. Does not waive the health check. |
| `fill_objective` | string | `"free-fill"` | `"free-fill"` or `"earliest-profitable"`; anything else names the pool and the bad value (see below). |
| `supported_bid` | list of strings | required | Bid assets the bot will pay, or `["*"]` for any reserve. |
| `supported_lot` | list of strings | required | Lot assets the bot will take, or `["*"]` for any reserve. |
| `profits` | list of `[[pools.profits]]` tables | `[]` | Ordered profit rules; the first whose lists cover a candidate auction wins over `default_profit_bps`. |

**`[[pools.profits]]` fields** (all required): `profit_bps` (integer),
`supported_bid` (list of strings, or `["*"]`), `supported_lot` (list of
strings, or `["*"]`).

`fill_objective` decides which ledger a fill aims at, before `force_fill`'s
cap (if any) is applied. `"free-fill"`, the default, waits for the ledger
the auction's bid has ramped away to nothing — the most the auction can
pay, and the last ledger to get it, since anyone willing to pay a real bid
can fill it first. `"earliest-profitable"` instead fills at the first
ledger the lot covers the bid plus the matching profit rule (or
`default_profit_bps`): less profit per fill, but landing where competition
for the auction is real rather than betting nothing else is watching this
pool. A value that is neither fails naming the pool and the value:
`` `<value>` is not a fill_objective: it is `free-fill` (the default,
which waits for the ledger the bid is gone) or `earliest-profitable`
(which fills as soon as the lot covers the bid plus the pool's margin) ``.

## 5. Tracking and seeding

| Variable | Default | Constraint |
|---|---|---|
| `POLL_INTERVAL_MS` | `1000` | How often the poller asks the RPC for chain head, in milliseconds. At least 100, at most 60000. |
| `USER_REFRESH_LEDGERS` | `241920` | A tracked user whose row is older than this many ledgers is refreshed, so accrued interest is never missed. |
| `REFRESH_BATCH` | `20` | A rate, not a cap: how many stale users the tracker refreshes per tick, how many flagged borrowers the auctioneer decides and acts on per pool per tick, and the full scan's page size. At least 1, at most 1000. Never bounds the oracle scan, which flags every borrower a price move went against regardless of batch size. |
| `FULL_SCAN_LEDGERS` | `1200` | How often the full scan reports the least healthy borrowers, in ledgers. At least 1. |
| `SCAN_HF_THRESHOLD` | `1.2` | The health factor the full scan reports below (strictly). Must sit strictly above `LIQ_HF_THRESHOLD` (see below). |
| `LIQ_HF_THRESHOLD` | `0.998` | The health factor at or below which a borrower is liquidatable. Below the contract's own strict threshold of `1.0` on purpose, to absorb rounding and the interest accrued between planning and execution. |
| `SEED_URL` | `https://api.blend.templarfi.org` | The analytics API the tracker seeds its initial tracked-user set from. An empty value disables it, leaving `SEED_FILE` (if set) as the only seed source. |
| `SEED_HF_MAX` | `10` | Only accounts at or below this health factor are seeded. |
| `SEED_FILE` | unset | Path to a static file of pool-to-account lists; supplements or substitutes `SEED_URL`. |

`LIQ_HF_THRESHOLD` must sit strictly below `SCAN_HF_THRESHOLD`, checked
after parsing both (`Args::service_with_secrets`): the full scan only
flags borrowers strictly below `SCAN_HF_THRESHOLD`, so a liquidation
threshold at or above it would name borrowers nothing ever flags for a
decision — reachable only through an event or a price move, and silently.
The two equal is refused with `LIQ_HF_THRESHOLD is at or above
SCAN_HF_THRESHOLD: the full scan only flags borrowers strictly below
SCAN_HF_THRESHOLD, so a liquidation threshold at or above it names
borrowers nothing ever flags for a decision`.

`SEED_FILE`, when given, is TOML: one `[accounts]` table mapping pool
address to a list of account addresses to seed for that pool
(`seed.example.toml` has the shape). The table rejects an unrecognized
top-level key. Every account it lists is re-read and re-valued from chain
before the bot trusts it, so a stale or wrong entry in the file costs one
extra chain read and nothing more — it can never cause an incorrect
submission.

## 6. Auctioneer

| Variable | Default | Constraint |
|---|---|---|
| `TARGET_HF` | `1.06` | The health factor a liquidation aims to leave the borrower at. At least `1.03`, and strictly below `1.15`. |
| `ORACLE_SCAN_LEDGERS` | `60` | How often, in ledgers, prices are re-read for a significant move. At least 1. |
| `PRICE_DELTA_BPS` | `250` | How far a price must move, in basis points, to be worth rechecking the borrowers exposed to it. At least 1. |
| `PLAN_ITERATIONS` | `5` | How many times a rejected liquidation percent is adjusted against the contract's own answer before the borrower is left until the next recheck. At least 1. |

`TARGET_HF`'s band is `[1.03, 1.15)`. The lower bound, `1.03`, is the
contract's own: below it the contract answers `InvalidLiqTooSmall`
(`1214`) for a partial liquidation. The upper bound is this bot's own
margin, one notch inside the contract's actual ceiling — the contract's
own check is strict (`is_hf_over` is `>`), so it accepts exactly `1.15`,
but aiming a liquidation at that ceiling leaves no room for the drift
between planning and fill: one ledger of interest on the borrower's debt
after planning can put the outcome over `1.15`, which the contract answers
`InvalidLiqTooLarge` (`1213`). A value outside the band is refused at
parse rather than clamped, because a silently clamped `TARGET_HF=0` would
make the planned excess non-positive for every borrower — every
liquidatable one recorded as "no plan", forever, with no warning at all —
and a value above the band would burn `PLAN_ITERATIONS` simulations per
borrower before skipping it.

`ORACLE_SCAN_LEDGERS` and `PRICE_DELTA_BPS` are both refused at `0`: a
zero scan period would not mean "every ledger", it would switch the
oracle scan off for the life of the process, and a zero delta computes to
`0` for an unchanged price, which flags every borrower on every scan as a
spurious price move. `PLAN_ITERATIONS` is refused at `0` for the same
reason — a walk that never simulates would skip every liquidation as
though the contract had refused it, silently.

## 7. Filler

| Variable | Default | Constraint |
|---|---|---|
| `HF_SAFETY_MULTIPLIER` | `1.1` | The pool's own `min_health_factor` is multiplied by this for the floor the filler keeps its own position at or above after a fill. At least `1`. |
| `REPLAN_LEDGERS` | `10` | How often, in ledgers, an auction the filler has already planned is planned again. At least 1. |
| `REPLAN_NEAR_LEDGERS` | `5` | Within this many ledgers of its planned fill ledger, an auction is planned again on every ledger. `0` is meaningful: it means only at the fill ledger itself. |
| `XLM_FEE_RESERVE` | `50` | XLM the filler never spends, kept back for transaction fees. Decimal XLM; since XLM has 7 decimals, the parsed value is exactly this many stroops. |
| `HIGH_FEE_PROFIT_THRESHOLD` | `10` | The estimated profit, in the pool oracle's own units, at or above which a fill pays `HIGH_FEE` rather than `BASE_FEE`. |
| `INVENTORY_REFRESH_SECS` | `30` | The longest the filler's wallet balances go unread, in seconds; also re-read after every transaction that landed or may have. At least 1. |

`HF_SAFETY_MULTIPLIER` under `1` is refused at parse rather than clamped:
under one, the filler's floor would sit below the pool's own
`min_health_factor` — the operator's stated minimum — letting a fill leave
the filler below it by design. `REPLAN_LEDGERS` at `0` is refused because
it would re-plan every open auction on every ledger, which is
`REPLAN_NEAR_LEDGERS`'s job and is meant to apply only near the fill
target. `INVENTORY_REFRESH_SECS` at `0` is refused because it would read
every wallet balance on every tick.

## 8. Operations

| Variable | Default | Constraint |
|---|---|---|
| `RUN_MODE` | `loop` | `loop` (follow the configured pools until shut down) or `check-config` (validate everything, print it redacted, and exit). |
| `LOG_FORMAT` | `text` | `text` for a terminal, `json` for a log shipper (one JSON object per line). |
| `RUST_LOG` | `info,blend_liquidator=debug` in the image; unset outside it | Not a `clap` argument — read by `tracing_subscriber`'s `EnvFilter` in `src/main.rs`. The image sets it with `ENV` (and `docker-compose.yml` sets the same value); unset or invalid, the bot falls back to that same filter. Set but empty, it is a filter with no directives and the bot logs nothing at all, a startup error included. |
| `PORT` | unset | Turns the `/healthz`, `/livez` and `/metrics` server on. Wins over `HTTP_PORT` when both are set — this is the variable a platform such as Cloud Run injects. |
| `HTTP_PORT` | unset | Also turns the HTTP server on, for a deployment that does not inject `PORT`. Neither set leaves the server off entirely. |
| `HTTP_BIND_ADDR` | `127.0.0.1` | The address the HTTP server binds. Loopback by default; a `0.0.0.0` bind belongs behind an ingress that admits only the platform's probes and scraper, since the endpoints carry no authentication. |
| `HEALTH_MAX_LAG_LEDGERS` | `10` | `/healthz` answers ready only while the processed ledger is within this many ledgers of the chain head this process has observed, in either direction. At least 1. |
| `TELEGRAM_BOT_TOKEN` | unset | Secret, read from the environment only. Both this and `TELEGRAM_CHAT_ID` or neither: one alone is a startup error. |
| `TELEGRAM_CHAT_ID` | unset | Not a secret — an ordinary `clap` argument. The chat (or channel) notifications are sent to. |
| `FAILURE_NOTIFICATION_COOLDOWN_HOURS` | `24` | How long a repeated notification of the same `(pool, account, kind)` is suppressed for, in hours. At least 1 — there is no "no cooldown" spelling, only shorter ones. |

`TELEGRAM_CHAT_ID` set without `TELEGRAM_BOT_TOKEN` fails with
`TELEGRAM_CHAT_ID is set but TELEGRAM_BOT_TOKEN is not`, and the reverse
fails with `TELEGRAM_BOT_TOKEN is set but TELEGRAM_CHAT_ID is not` — a
token with nowhere to send is as useless as a destination with nothing to
send it with. The token's value is never logged: it is `Secret`-wrapped
and renders as `Secret(<redacted>)`. `TELEGRAM_CHAT_ID` is not a secret,
and the resolved-configuration line prints it beside the redacted token.

`HEALTH_MAX_LAG_LEDGERS` at `0` is refused: a bot exactly at chain head
would still report not-ready on every ledger boundary its own poll
interval crosses, which is not a readiness bound at all.
`FAILURE_NOTIFICATION_COOLDOWN_HOURS` at `0` is refused for the matching
reason on the notification side: it would suppress nothing, notifying on
every tick.
