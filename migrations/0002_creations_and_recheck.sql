-- Phase 4. A new migration rather than an amendment of 0001: 0001 is
-- merged and may already be applied, and sqlx checksums an applied
-- migration.

-- Every auctioneer submission, the ones dry-run only simulated included.
-- `percent` is NULL for a bad-debt creation: the contract sizes that one
-- itself and there is no percent to record — and an auction creation always
-- names one, which the last CHECK enforces so the two kinds cannot be
-- confused by a row that carries the wrong shape. Amounts are not stored
-- here — `bid` and `lot` are the asset lists the auction named, which is
-- what a later reader needs to understand the decision. `dry_run` is the
-- bot's configured mode when the row was written, not whether this row was
-- sent: a row with `dry_run = false` and no `tx_hash` is an armed attempt
-- whose transaction was never named.
CREATE TABLE creations (
    id          bigserial PRIMARY KEY,
    tx_hash     text,
    kind        text     NOT NULL CHECK (kind IN ('auction', 'bad_debt')),
    pool        text     NOT NULL,
    account     text     NOT NULL,
    percent     smallint CHECK (percent BETWEEN 1 AND 100),
    bid         jsonb    NOT NULL,
    lot         jsonb    NOT NULL,
    ledger      bigint   NOT NULL,
    dry_run     boolean  NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (
        (kind = 'auction' AND percent IS NOT NULL)
        OR (kind = 'bad_debt' AND percent IS NULL)
    )
);

-- The audit read an operator makes: this pool's recent creations.
CREATE INDEX creations_by_pool ON creations (pool, created_at DESC);

-- The ledger at which this row was flagged for an auctioneer decision, or
-- NULL when there is nothing to decide. It lives on the row the tracker
-- already writes, so flagging costs no second statement — and it is
-- durable, so a restart does not drop the work an event created.
ALTER TABLE users ADD COLUMN recheck_ledger bigint;

-- The auctioneer's queue: this pool's flagged rows, oldest flag first.
-- Partial, because the flagged set is a small fraction of the table.
CREATE INDEX users_needing_recheck ON users (pool, recheck_ledger)
    WHERE recheck_ledger IS NOT NULL;
