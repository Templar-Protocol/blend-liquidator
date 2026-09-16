-- The filler's audit trail: one row per fill the filler executed, dry-run
-- or not (spec section 4's `fills`). A row is written before anything is
-- submitted and the transaction's hash is attached once there is one, so a
-- row with no hash and `dry_run = false` is an armed attempt whose
-- transaction was never named. Values and profit are in the pool oracle's
-- units, as decimal text through `numeric` like every other i128 here.
CREATE TABLE fills (
    id            bigserial   PRIMARY KEY,
    tx_hash       text        UNIQUE,
    pool          text        NOT NULL,
    account       text        NOT NULL,
    auction_type  smallint    NOT NULL CHECK (auction_type BETWEEN 0 AND 2),
    fill_ledger   bigint      NOT NULL,
    percent       smallint    NOT NULL CHECK (percent BETWEEN 1 AND 100),
    bid           jsonb       NOT NULL,
    lot           jsonb       NOT NULL,
    bid_value     numeric     NOT NULL,
    lot_value     numeric     NOT NULL,
    est_profit    numeric     NOT NULL,
    dry_run       boolean     NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX fills_by_pool ON fills (pool, created_at DESC);
