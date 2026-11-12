# An unpriced reserve does not hold a pool's events cursor

**Date:** 2026-11-12
**Status:** design approved. Closes the first item of
[2026-11-09 Precept v0.1.1](2026-11-09-precept-v0.1.1-reconciliation.md) §5
("A pool whose reserve the oracle cannot price holds its events cursor").

## The problem

`Tracker::refresh` (`src/tracker.rs:209-264`) values every account in its
batch that owes anything with `?`. One holder of a reserve the oracle cannot
price fails it with `MathError::MissingPrice`, and the batch fails with it.
The tracker loop reads a `TrackerError::Math` as transient: it declines the
tick, and the poller re-reads the same range until the price returns. So the
pool's events cursor stops. No new borrower, no position change and no
auction is applied for that pool, for any borrower, not only the ones
holding the unpriced asset. If the outage outlasts the RPC's event retention
window, the cursor falls out of it and the pool needs a full reseed.

Precept makes this an ordinary operating state, not an exotic one.
`pool_oracle_adapter`'s `lastprice` (Templar-Protocol/precept `v0.1.2`,
`pool-oracle-adapter/src/lib.rs:189-200`) answers `None`, never a trap, when
its parent proxy oracle fails or has no aggregated price, which includes a
feed past its `max_age_secs` and a feed tripped by hand. It also answers
`None` when a price is dated after the current ledger. `v0.1.2`'s
`oracle_tripper` exists to trip one asset's feed in a single call. A tripped
feed is an operator's deliberate incident response, and the bot must keep
following the pool through it.

Today the stall is the only signal an operator gets: `/healthz` reports the
pool's lag. Remove the stall and that signal goes with it, so this change
adds one.

## 1. Design

### A. The tracker defers a valuation it cannot compute

In `Tracker::refresh`, per account:

- **Owes nothing:** delete the row, unchanged. No price is involved.
- **Valued:** upsert the row with its health factor, unchanged.
- **`MathError::MissingPrice(asset)`, and only that error:** upsert the row
  with the positions as read from chain (exact), a health factor of
  **NULL**, and `updated_ledger` at the snapshot's ledger. Flag the account
  for recheck at the tick's ledger (`Store::flag_recheck`). Record `asset`
  in the outcome and carry on with the batch.
- **Any other error** still fails the batch exactly as now, whether a
  `ChainError`, an overflow or a missing reserve. Those mean an incomplete
  view or a bug, not an operating state.

`RefreshOutcome` gains `unvalued: usize` and `unpriced: BTreeSet<String>`,
the distinct assets that stopped a valuation.

The seed path (`Tracker::seed`) refreshes through the same function, so one
unpriced holder no longer fails a seed batch either. It counts unvalued
accounts as tracked.

### B. Why this keeps the cursor invariant

CLAUDE.md's first safety invariant: the poller commits a ledger only once
the tracker has answered that its whole effect is in the store.

A ledger's effect is what its events change:
- the auction bookkeeping `Tracker::apply` writes;
- the on-chain positions of every account the events name.

Both are stored exactly in every case above. A health factor is not a ledger
effect. It is a valuation derived from those positions and the oracle's
prices, and the oracle is not part of the ledger range the poller reads.

When the valuation cannot be computed, the store records that fact durably:
- a NULL health factor;
- a recheck flag.

A kill therefore loses nothing. The next run's stale pass (§1.D) re-values
the row, and the auctioneer re-decides the account from chain. The rejected
alternative is a retry set held in memory: a kill would drop it and nothing
would ever re-read those accounts. That is the silent loss the invariant
exists to prevent.

The other rejected alternative is valuing at the last price seen. The
contract itself refuses priced actions without a current price, so a
decision made on a stale one is a decision the chain would not honour.

### C. The store

- **Migration `0004`:** `ALTER TABLE users ALTER COLUMN health_factor DROP
  NOT NULL;`. It is a new migration, never an amendment. NULL means "could
  not be valued at `updated_ledger`: a reserve this account holds had no
  price".
- **`TrackedUser.health_factor` becomes `Option<i128>`.** The store's
  decimal decode reads NULL as `None` and binds `None` as NULL.
- **`users_below_health` is unchanged in effect.** In SQL `NULL < x` is not
  true, so the full scan never selects an unvalued row; the recheck flag
  already covers it. Its keyset pagination reads `health_factor` only from
  the rows it returned, which are never NULL.
- **`users_stale` also selects `health_factor IS NULL` rows**, whatever
  their `updated_ledger`, oldest first, under the same batch limit. That is
  what re-values a row within a ledger of its price returning, rather than
  a whole `USER_REFRESH_LEDGERS` span later. The stale pass runs inside the
  tick, so on a pool with unvalued rows it costs one more snapshot read per
  tick, bounded by the refresh batch.
- **A new `Store::count_unvalued(pool)`.**
- `.sqlx/` is regenerated (`make sqlx-prepare`).

### D. The tick

In `apply_tick` (`src/service.rs:1033`), the refresh of the accounts the
ledger's events named and the stale pass both return outcomes. The tick
then:
- raises one `ReserveUnpriced` per distinct unpriced asset across both
  (§1.E);
- re-gauges `unvalued_users` (§1.F).

It is acknowledged as before, now including the case where a reserve was
unpriced.

`apply_tick` already flags for recheck every account the ledger's events
name. An unvalued account is flagged by the refresh itself as well, so one
the stale pass re-reads is flagged too. The auctioneer cannot decide a
borrower it cannot value, and moves its flag one past the tick (CLAUDE.md,
"A borrower's `recheck_ledger` flag must be moved forward"). So an unvalued
borrower never starves the queue, and is decided on the first pass after
its price returns.

The auctioneer and the filler are unchanged. Both already degrade per
borrower and per auction.

### E. The notification

`NotificationKind::ReserveUnpriced`, at `Severity::High`.

- **Raised by** `apply_tick`, once per distinct unpriced asset its
  refreshes reported.
- **Fields:** `pool` is the pool. `account` is `Some(asset address)`: the
  dedup key is `(pool, account, kind)`, and keying on the asset makes a
  second asset that loses its price in the same pool page too, rather than
  being suppressed by the first. `Notification::account`'s doc changes from
  "the borrower" to "the borrower, or for `ReserveUnpriced` the asset".
- **Message:** names the pool, the asset, and how many tracked borrowers in
  that pool are unvalued. It says the contract refuses priced actions on
  those borrowers until the price returns, and that the bot keeps following
  the pool meanwhile.
- **Cooldown:** the existing one (`FAILURE_NOTIFICATION_COOLDOWN_HOURS`), so
  a deliberate trip pages once per cooldown, not once per ledger.
- **Not from the startup seed.** Nothing before the run's tasks may notify:
  `finish_run` drains the notifier only after they start. The seed logs a
  WARN line per unpriced asset instead, and the first tick's stale pass
  re-reads those rows and notifies.
- **No recovery notification.** The gauge falling to zero is the recovery
  signal.
- **A new metric series.** `NotificationKind` is closed and its `as_str` is
  a metric label, so `notifications_total{kind="reserve_unpriced", …}` is a
  new series. That is documented, never added quietly (CLAUDE.md).

### F. The gauge

`precept_liquidator_unvalued_users{pool}` comes from `count_unvalued`.
`regauge_users` sets it beside `users_tracked`, at every place that already
re-gauges, so it trails the store by at most a ledger. Rendered with every
pool present, zero included, as `users_tracked` is.

## 2. Testing

Test-first, each new branch's test confirmed to fail without it.

- **Store** (`#[sqlx::test]`):
  - a NULL health factor survives an upsert and a read;
  - `users_below_health` skips a NULL row;
  - `users_stale` returns a NULL row whatever its `updated_ledger`, and
    respects the limit;
  - `count_unvalued` counts only NULL rows of the pool asked.
- **Tracker** (scripted RPC, `src/harness.rs`'s fixture pool and its two
  borrowers), with the oracle scripted to answer no price for one reserve
  that one borrower holds:
  - `refresh` returns `Ok`;
  - the priced borrower's row carries its golden health factor;
  - the other's row has a NULL health factor, its exact positions and a
    recheck flag;
  - the outcome reports `unvalued == 1` and that one asset.
  - Separately, a non-price `MathError` still fails the batch.
- **Service:** `apply_tick` with that unpriced reserve:
  - returns `Ok`, so the tick is acknowledged;
  - a `RecordingChannel` receives exactly one `ReserveUnpriced`, at High,
    naming the pool and the asset;
  - a second tick inside the cooldown is deduplicated;
  - `unvalued_users{pool}` reads 1;
  - with the price scripted back, the next tick's stale pass values the row
    and the gauge reads 0.
- **Poller and cursor:** a test that the cursor advances across ledgers in
  which a reserve was unpriced, if `src/ledger.rs`'s existing tracker-ack
  tests can drive it. Otherwise the service test's `Ok` is the contract,
  since the poller's commit-on-ack is already tested.
- **A seventh sandbox scenario, `unpriced_reserve`.** The script3 mock
  SEP-40 oracle can answer `None` for an asset it was configured with
  (`script3/sep-40-oracle` `v1.2.0`, `mock-sep-40/src/mock_sep_40.rs`):
  `set_price(prices, timestamp)` stores `prices` positionally under
  `timestamp` and makes it the latest, so `set_price([xlm], now)` leaves
  USDC, index 1, with no price at the latest timestamp, and
  `lastprice(USDC)` answers `None` — the real path, against the real pool
  and RPC. `set_price_stable([xlm, usdc])` (`crash.sh`) restores both.

  A new `scripts/sandbox/unprice.sh` makes that call. It is gated and
  pinned like `crash.sh`, signed by the admin, and stamped with the latest
  ledger's close time. The scenario runs a **dry-run bot with no key**
  (nothing here needs to sign), with `USER_REFRESH_LEDGERS=3` so the stale
  pass re-reads the seeded borrower within a few ledgers, with no chain
  action. It asserts, in order:
  1. the borrower is tracked with a health factor;
  2. after `unprice.sh`, `unvalued_users{pool}` reads 1 and the row's health
     factor is NULL;
  3. `ledger_processed{pool}` keeps advancing while USDC is unpriced —
     the cursor is not held, which is the whole fix;
  4. `notifications_total{kind="reserve_unpriced",…}` counts exactly one
     queued delivery;
  5. after `crash.sh 1000000` restores both prices at $0.10, the gauge
     reads 0 and the row's health factor is back.

  The name joins the five places scenario names are written, which
  `scripts/check-repo-invariants.sh` holds equal.

## 3. Documentation

- **CLAUDE.md:**
  - the cursor-invariant bullet gains §1.B's sentence that a valuation is not
    a ledger effect and is deferred durably when it cannot be computed;
  - the module map: `src/tracker.rs`, `src/store.rs`, `migrations/` (0004),
    `src/notifier.rs` (the new kind), `src/metrics.rs` (the new gauge);
  - a gotcha: a NULL `users.health_factor` means unvalued, never zero, and
    the full scan skips it by design.
- **`docs/architecture.md`**, and wherever the notification kinds or metrics
  are listed for operators: `docs/deploy.md` and
  `docs/deployment-contract.md`. The implementation greps for them.
- **`CHANGELOG.md` `[Unreleased]`:**
  - **Fixed:** the stall.
  - **Added:** `ReserveUnpriced` and `unvalued_users`.
  - **Note:** migration `0004` applies itself at startup. A binary from
    before it cannot read a store holding a NULL health factor. Nothing is
    released yet, so no earlier binary is a rollback target.
- A dated "resolved" note on the 2026-11-09 spec's §5 item.

## 4. Out of scope

- A recovery notification.
- The adapter's own `max_age_secs`, and anything about how a feed is tripped
  or re-armed.
- Any change to the auctioneer or the filler.
- The `v0.2.0` release.
