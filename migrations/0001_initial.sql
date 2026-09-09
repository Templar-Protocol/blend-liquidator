-- Phase 3's schema: the three tables this phase writes. `creations` and
-- `fills` arrive in the migration that lands with the phase that writes
-- them, so no table exists without a writer.

-- How far a named task has applied. One row per task, e.g. `events:C…`.
CREATE TABLE cursors (
    name          text        PRIMARY KEY,
    ledger        bigint      NOT NULL,
    paging_token  text,
    updated_at    timestamptz NOT NULL DEFAULT now()
);

-- Borrowers the bot tracks. A row exists only while the account has
-- liabilities: an account that repays everything is deleted, not kept with
-- an empty map, so `count(*)` is the number of positions that can be
-- liquidated.
--
-- `health_factor` is numeric because the ratio of collateral to a dust
-- liability exceeds bigint, and it is normalised to 7 decimals
-- (`hf * 10^7 / oracle_scalar`) so pools whose oracles differ in decimals
-- order and compare alike. `collateral` and `liabilities` map a reserve
-- index to a b-token or d-token amount, both as decimal strings, because
-- those amounts exceed what JSON numbers hold exactly.
CREATE TABLE users (
    pool            text    NOT NULL,
    account         text    NOT NULL,
    health_factor   numeric NOT NULL,
    collateral      jsonb   NOT NULL,
    liabilities     jsonb   NOT NULL,
    updated_ledger  bigint  NOT NULL,
    PRIMARY KEY (pool, account)
);

-- The scan that matters: the least healthy borrowers in a pool, first.
CREATE INDEX users_by_health ON users (pool, health_factor);

-- The refresh pass, which runs on every tick for every pool: the rows this
-- pool has not re-valued since a given ledger, oldest first. Without this the
-- pass scans the pool's rows and sorts them, once per ledger.
CREATE INDEX users_by_staleness ON users (pool, updated_ledger);

-- Open auctions and the filler's current plan for each: the ledger it
-- intends to fill at and the percent it intends to fill. `auction_type` is
-- the contract's discriminant (0 user liquidation, 1 bad debt, 2 interest),
-- small enough for smallint alongside it.
--
-- `percent` is the filler's *planned* fill percent, not a record of the
-- auction's creation: it is NULL until the filler plans a fill, just like
-- `fill_ledger` beside it. What was actually filled is recorded separately
-- in `fills`, once that table exists.
--
-- Both ranges are the contract's, and the Rust `AuctionType`/`FillPercent`
-- types already enforce the same thing on every value this crate writes;
-- these `CHECK`s are defence in depth, not the primary guard, so a row that
-- violates one did not come from this crate. A `CHECK` passes on `NULL`,
-- which is exactly what an unplanned percent is.
CREATE TABLE auctions (
    pool            text     NOT NULL,
    account         text     NOT NULL,
    auction_type    smallint NOT NULL CHECK (auction_type BETWEEN 0 AND 2),
    start_ledger    bigint   NOT NULL,
    fill_ledger     bigint,
    percent         smallint CHECK (percent BETWEEN 1 AND 100),
    bid             jsonb    NOT NULL,
    lot             jsonb    NOT NULL,
    updated_ledger  bigint   NOT NULL,
    PRIMARY KEY (pool, account, auction_type)
);

-- The filler walks open auctions in the order they became fillable.
CREATE INDEX auctions_by_start ON auctions (pool, start_ledger);
