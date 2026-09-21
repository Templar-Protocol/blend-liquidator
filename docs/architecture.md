# Architecture overview

This is the one-sitting tour: what the bot does, how its tasks fit
together, and where to look for the arithmetic and the contract facts
behind a decision. `CLAUDE.md` is the long form, written for an agent
working in this tree; this document is for a developer or a reviewer
reading once. `docs/configuration.md` is every setting, and
`docs/deployment-contract.md` and `docs/deploy.md` cover the image and the
operator's path to running it.

## 1. What the bot does

`blend-liquidator` follows a configured set of Blend v2 lending pools on
Stellar, decides which tracked borrowers are underwater, creates the
liquidation auction the pool's contract should accept, fills auctions —
its own and anyone else's — that its configured assets support, and
unwinds the position a fill leaves it holding back toward its wallet. It
is not non-custodial: it holds a signing key and submits transactions
itself, which is the point of a liquidation bot (`src/liquidator.rs:4-7`).
Dry-run is the default for exactly that reason — see §4.

## 2. The process

One binary (`src/main.rs`) runs one `Service`. `Service::run`
(`src/service.rs:2689`) connects and migrates the store, seeds any pool
that needs it, then spawns seven kinds of task and runs until a shutdown
signal arrives and every one has returned:

1. one `LedgerPoller` per configured pool (`src/ledger.rs:202`,
   `spawn_pollers` at `src/service.rs:2200`);
2. one tracker task consuming every poller's shared channel
   (`tracker_loop`, fed by `spawn_pollers`' `mpsc::Sender`);
3. one auctioneer task (`spawn_auctioneer`, `src/service.rs:2431`);
4. one filler task (`spawn_filler`, `src/service.rs:2483`);
5. one watchdog task (`spawn_watchdog`/`watchdog_loop`,
   `src/service.rs:2238`/`2274`);
6. one HTTP server, only when `PORT` or `HTTP_PORT` is set
   (`http::serve`, wired in `Service::run` before the seed pass);
7. only when armed (`DRY_RUN=false`), one submission-queue worker per
   *distinct* signing key (`spawn_queues`, `src/service.rs:2356`) — the
   auctioneer and the filler share one worker whenever the auctioneer has
   fallen back to the filler's key, because two queues on one key is the
   race `src/queue.rs` exists to make unreachable.

**The auctioneer and the filler are separate tasks, off a published tick,
never inside the tracker's own.** The tracker's acknowledgement is what
lets a poller commit its cursor (§3), and neither a liquidation decision
nor a fill is a ledger effect — either sitting inside that path could stall
the cursor on a slow decision or a failed submission. Instead, once the
tracker acknowledges a tick it publishes the ledger on a
`tokio::sync::watch<LedgerTick>` (`src/service.rs`, `handle_message`'s
`Tick` arm), and the auctioneer and the filler are two independent readers
of that watch — never a second reader of the poller channel itself, which
would break the per-sender ordering the cursor rests on. Each holds its
own `StartupGate` (`src/service.rs:1737`), because each measures
`STARTUP_DELAY_LEDGERS` from the first tick *it* saw and each answers for
its own signing key.

## 3. One ledger's journey

```
poller: getEvents ──Event──► tracker.apply (stage accounts)
                │
                ▼ Tick { ack }
     tracker refreshes + flags staged accounts, answers ack
                │
     cursor commits only now ◄──┘
                │
     tick published on watch<LedgerTick>
           │                │
           ▼                ▼
      Auctioneer          Filler
     decide → act       tick → execute
           │                │
           └──────┬─────────┘
                  ▼
    SubmissionQueue (one per signing key)
```

A poller drains a range of events, sends each as `PollerMessage::Event`
(the tracker applies it and stages the accounts it named), then sends
`PollerMessage::Tick` carrying a `oneshot` acknowledgement
(`src/ledger.rs:584-591`). Only once the tracker has refreshed and flagged
those accounts in the store does it answer that channel — and only that
answer lets `LedgerPoller::poll_once` write the events cursor
(`src/ledger.rs:600-614`). The tracker then publishes the ledger on the
watch every deciding task reads. The auctioneer decides and acts on
whichever pools' users are flagged; the filler walks every pool's open
auctions. Both submit, when armed, through the one `SubmissionQueue` their
signing key owns.

## 4. The invariants that matter

- **The events cursor means "applied", never "sent".** A poller only
  writes its cursor after the tracker's `oneshot` acknowledgement, which
  it sends only once a tick's whole effect is in the store
  (`src/ledger.rs:584-614`). Committing on send instead would let a kill
  drop whatever was still queued while the store claimed those ledgers
  done — and nothing re-reads a cursor that already exists, so the loss
  would be silent.
- **Nothing is sent for a key while an earlier transaction's outcome on it
  is unknown.** A Soroban transaction is built against its source
  account's sequence number at prepare time, so two in-flight
  transactions on one key race to consume it. `SubmissionQueue` resolves
  every submission to a terminal outcome before it takes the next
  (`src/queue.rs:1-23`); only a failure that provably sent nothing is
  retried.
- **Dry-run is the default, with exactly one opt-out.** `DRY_RUN` parses
  only the literal strings `true` or `false` (`strict_bool`,
  `src/config.rs:17`), wired to `Args::dry_run` with a default of `true`
  (`src/config.rs:602-619`); live trading requires setting it to exactly
  `false`. Every other spelling is another way into live trading, and the
  dangerous direction must be the loud one.
- **The maths agrees with the contract, and a fixture proves it.**
  `tests/fixtures/mainnet-fixed-v2.json` holds one mainnet ledger's
  entries and the contract's own attested answers at that ledger.
  Accruing the stored entries must reproduce the contract's `get_reserve`
  to the stroop, and decoding the stored positions must reproduce
  `get_positions`; both are checked in code, never hand-adjusted to match
  it.

## 5. Where the maths lives

`src/math/` is a pure port of the pool contract's arithmetic: no I/O, no
panics, every operation checked. Each module owns one piece of the
contract's own logic:

| Module | Decides |
|---|---|
| `fixed` | Checked fixed-point rounding, the contract's own (`div_ceil`/`mul_floor`/…). |
| `reserve` | Interest accrual and token conversions (`Reserve::accrue`). |
| `position` | Effective collateral/liability values and the health factor. |
| `auction` | The Dutch-auction ramp (`scale_auction`). |
| `liquidation` | Which bid and lot assets, and what percent, closes a borrower's excess down to `TARGET_HF` (`plan_liquidation`). |
| `fill` | Which ledger a fill aims at and the request list that takes an auction over while keeping the filler's own position at its floor (`plan_fill`). |
| `setoff` | The fork's default path: what set-off and default do to a reserve's balances and `b_rate` (`project_default`). |
| `unwind` | Which debts to repay and which collateral to withdraw once a fill has left the filler holding a position (`plan_unwind`). |

## 6. The contract it targets

The source of truth for pool behaviour is
`Templar-Protocol/blend-contracts-v2`, the ADR-0008/ADR-0011 security
fork — not the `blend-capital/blend-contracts-v2` stock contract Phases 1
through 7 were built against. Most of the fork is byte-identical to stock,
including all of this crate's arithmetic port. What differs, and what the
bot does about each difference, is written down in
`docs/specs/2026-09-20-adr-0008-fork-semantics.md`; in short:

- the fork destroys a defaulted borrower's debt in place — set-off, then a
  `b_rate` cut on the reserve's own suppliers, then confiscation of any
  remaining collateral to the pool's own address as plain supply — where
  stock instead moved the liabilities into the backstop's `Positions` and
  left every reserve's `b_rate` untouched;
- because that destruction runs inside a 100% fill's own transaction,
  before the contract checks the filler's health, `math::fill::plan_fill`
  projects a full fill's own `b_rate` haircut through
  `math::setoff::project_default` rather than trusting the pre-fill
  snapshot;
- only `UserLiquidation` auctions can be created or filled on the fork —
  the bad-debt and interest auction types panic — so the bot only ever
  builds that kind, and clears a defaulted borrower through the
  still-live `bad_debt(user)` call instead;
- three new events (`debt_setoff`, `collateral_orphaned`,
  `orphan_settled`) are decoded in `chain::xdr::events`, and a `bad_debt`
  event — which the fork's own contract can never emit — raises
  `NotificationKind::StockWasmDetected` as a deployment alarm, not a
  decision input;
- the pool contract's own address is now a `Positions` holder, since
  confiscated collateral lands there as ordinary supply, and both the
  auctioneer and the filler treat it as one of the bot's own accounts
  rather than a borrower.

**The fork is not deployed anywhere yet**: its pull request is a draft
whose first line says it is not ready for deployment, and it publishes no
release to pin. The bot still runs against stock Blend v2 pools as well,
and two things follow from that:

- `plan_fill`'s `b_rate`-haircut projection runs unconditionally, on stock
  pools too. Stock's own default path moves debt to the backstop and cuts
  no `b_rate` at all, so on a stock pool the projection overstates the
  damage a full fill causes — pessimistic, and safe, rather than wrong in
  the dangerous direction.
- `NotificationKind::StockWasmDetected` fires on every `bad_debt` event
  regardless of which wasm is actually deployed, and a stock pool emits
  `bad_debt` in ordinary operation — so on a stock pool this alert is
  expected noise, not evidence of a misconfiguration.
- The sandbox integration tier (`scripts/sandbox/`) pins stock wasm by
  release URL and SHA-256 (`scripts/sandbox/versions.env`) for exactly
  this reason: there is nothing from the fork yet to pin.

## 7. The store

Postgres holds two kinds of state, and they are not equally disposable.

**Tracking state** — the per-pool events cursor (`cursors`), tracked
borrowers (`users`), and open auctions (`auctions`), all from
`migrations/0001_initial.sql` — is a cache the bot rebuilds from chain.
`seed_pools_needing_it` (`src/service.rs:546`) reseeds a pool only when
its `users` table is empty *or* its events cursor is missing
(`user_count != 0 && cursor.is_some()` is the one condition that skips
it); `Tracker::seed` (`src/tracker.rs:301`) then pulls the account list
from every configured seed source and refreshes each from chain. Losing
this half costs one reseed pass per affected pool, not correctness.

**The audit tables** — `creations` (`migrations/0002_creations_and_recheck.sql`)
and `fills` (`migrations/0003_fills.sql`) — are the bot's own record of
what it decided and attempted, dry-run rows included: every auctioneer
submission (whether only simulated or actually sent) and every fill
attempt, written before anything is submitted and updated with a
transaction hash once there is one. Nothing on chain reproduces a
dry-run row, and a chain read cannot recover *why* the bot acted, only
*that* something landed. Dropping the database loses this record
permanently — it is not rebuildable, and it is the operational history an
operator or an incident review would reach for.
