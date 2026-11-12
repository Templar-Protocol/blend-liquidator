-- A borrower whose position could not be valued: a reserve it holds had no
-- oracle price at `updated_ledger`. Its `collateral` and `liabilities` are
-- still exact — they are read from chain, not priced — and its
-- `recheck_ledger` is raised, so the auctioneer decides it from chain once
-- the price returns, and the tracker's stale pass, oldest first within its
-- batch, re-reads it until then. NULL means "unvalued", never zero: the full scan's
-- `health_factor < threshold` never selects it.
ALTER TABLE users ALTER COLUMN health_factor DROP NOT NULL;
