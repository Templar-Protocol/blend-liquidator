//! The bot's durable state: cursors, tracked borrowers and open auctions.
//!
//! Every query is checked at compile time against the schema in
//! `migrations/`, and the metadata that makes that work without a database
//! lives in `.sqlx/` — regenerate it with `make sqlx-prepare` after
//! changing any query, or the Docker build fails on stale metadata.
//!
//! The store speaks the bot's types, not SQL's: amounts and health factors
//! are `i128`, ledgers are `u32`. Postgres has no 128-bit integer, so
//! amounts cross the boundary as decimal text — bound as `$n::text::numeric`
//! and read back through `::text` — which is exact for every value an
//! `i128` holds. Nothing here rounds.

use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::chain::xdr::{AuctionType, FillPercent};

/// A failure talking to the store, or reading a value it returned.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The connection pool could not be opened. The message is fixed on
    /// purpose: sqlx builds its own connection errors from the DSN, this
    /// one reaches a log line through `main`, and this repository's
    /// standing invariant is that a password never does. The cause is kept
    /// as a source for a caller that deliberately walks the chain, and is
    /// never rendered by `Display`.
    #[error("connecting to the database failed")]
    Connect(#[source] sqlx::Error),
    /// A query failed.
    #[error("database query: {0}")]
    Query(#[from] sqlx::Error),
    /// The embedded migrations could not be applied.
    #[error("applying migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// A `numeric` column did not read back as an `i128`. The column is
    /// named, never the row, so a message cannot leak an account.
    #[error("column {column} is not an integer: {value}")]
    Decimal {
        /// The column that failed.
        column: &'static str,
        /// What it held.
        value: String,
    },
    /// A `jsonb` column was not the shape this crate writes.
    #[error("column {column} is not an amount map: {detail}")]
    Json {
        /// The column that failed.
        column: &'static str,
        /// What was wrong with it.
        detail: String,
    },
}

/// The embedded migrations. `sqlx` takes a Postgres advisory lock while it
/// applies them, so two instances starting together cannot race.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A handle to the store. Cheap to clone: `PgPool` is a handle.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Opens a pool against `url`. Does not migrate: call [`Store::migrate`].
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await
            .map_err(StoreError::Connect)?;
        Ok(Self { pool })
    }

    /// Wraps an existing pool, which is how tests hand one in.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying pool, for queries this module does not wrap.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Applies every migration the binary embeds. Idempotent, and safe to
    /// run from two instances at once.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// One round trip, for the readiness probe.
    pub async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query!("SELECT 1 AS one")
            .fetch_one(&self.pool)
            .await?;
        Ok(())
    }
}

/// Reads a `numeric` column that was selected as text back into an `i128`.
/// A `NULL`, a fractional value or anything else is an error, never a zero:
/// a health factor that silently became zero would make a healthy account
/// look liquidatable.
fn decimal(text: Option<&str>, column: &'static str) -> Result<i128, StoreError> {
    let text = text.ok_or_else(|| StoreError::Decimal {
        column,
        value: "NULL".to_string(),
    })?;
    text.parse().map_err(|_| StoreError::Decimal {
        column,
        value: text.to_string(),
    })
}

/// A reserve-index-to-amount map as `jsonb`: keys are the index in decimal,
/// values the amount as a decimal string, because an `i128` amount exceeds
/// what a JSON number holds exactly.
fn index_amounts_to_json(amounts: &BTreeMap<u32, i128>) -> Value {
    Value::Object(
        amounts
            .iter()
            .map(|(index, amount)| (index.to_string(), Value::String(amount.to_string())))
            .collect(),
    )
}

/// The inverse. Anything that is not an object of decimal strings keyed by
/// decimal indexes is a `Json` error.
fn index_amounts_from_json(
    value: &Value,
    column: &'static str,
) -> Result<BTreeMap<u32, i128>, StoreError> {
    let object = value.as_object().ok_or_else(|| StoreError::Json {
        column,
        detail: "not an object".to_string(),
    })?;
    object
        .iter()
        .map(|(key, amount)| {
            let index: u32 = key.parse().map_err(|_| StoreError::Json {
                column,
                detail: format!("key {key:?} is not a reserve index"),
            })?;
            let amount = amount
                .as_str()
                .ok_or_else(|| StoreError::Json {
                    column,
                    detail: format!("amount for {key} is not a string"),
                })?
                .parse()
                .map_err(|_| StoreError::Json {
                    column,
                    detail: format!("amount for {key} is not an integer"),
                })?;
            Ok((index, amount))
        })
        .collect()
}

/// The same pair for a map keyed by asset address, as the auction sides are.
fn asset_amounts_to_json(amounts: &BTreeMap<String, i128>) -> Value {
    Value::Object(
        amounts
            .iter()
            .map(|(asset, amount)| (asset.clone(), Value::String(amount.to_string())))
            .collect(),
    )
}

/// The inverse of `asset_amounts_to_json`.
fn asset_amounts_from_json(
    value: &Value,
    column: &'static str,
) -> Result<BTreeMap<String, i128>, StoreError> {
    let object = value.as_object().ok_or_else(|| StoreError::Json {
        column,
        detail: "not an object".to_string(),
    })?;
    object
        .iter()
        .map(|(asset, amount)| {
            let amount = amount
                .as_str()
                .ok_or_else(|| StoreError::Json {
                    column,
                    detail: format!("amount for {asset} is not a string"),
                })?
                .parse()
                .map_err(|_| StoreError::Json {
                    column,
                    detail: format!("amount for {asset} is not an integer"),
                })?;
            Ok((asset.clone(), amount))
        })
        .collect()
}

/// How far a named task has applied. The poller keeps one per pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// The last ledger whose events were applied in full.
    pub ledger: u32,
    /// The RPC's paging token this task stopped at, `None` when it stopped
    /// on a range boundary. The event poller only ever commits a pass it
    /// proved drained a whole range, so it always writes `None`; the
    /// column stays because it is the `cursors` table's contract for any
    /// task that does resume mid-range, and the schema is the spec's.
    pub paging_token: Option<String>,
}

/// The cursor name a pool's event stream uses.
#[must_use]
pub fn events_cursor(pool: &str) -> String {
    format!("events:{pool}")
}

/// A `bigint` ledger back into the `u32` the chain uses. Out of range means
/// the row did not come from this crate.
fn ledger(value: i64, column: &'static str) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::Decimal {
        column,
        value: value.to_string(),
    })
}

impl Store {
    /// The named cursor, or `None` when the task has never run.
    pub async fn cursor(&self, name: &str) -> Result<Option<Cursor>, StoreError> {
        let row = sqlx::query!(
            "SELECT ledger, paging_token FROM cursors WHERE name = $1",
            name
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(Cursor {
                ledger: ledger(row.ledger, "ledger")?,
                paging_token: row.paging_token,
            })
        })
        .transpose()
    }

    /// Writes the named cursor, replacing any previous value.
    pub async fn set_cursor(&self, name: &str, cursor: &Cursor) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO cursors (name, ledger, paging_token, updated_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (name) DO UPDATE
               SET ledger = EXCLUDED.ledger,
                   paging_token = EXCLUDED.paging_token,
                   updated_at = EXCLUDED.updated_at",
            name,
            i64::from(cursor.ledger),
            cursor.paging_token.as_deref(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// A borrower the bot tracks. A row exists only while the account owes
/// something: the tracker deletes it the moment its liabilities empty, so
/// the table's size is the number of positions that could be liquidated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedUser {
    /// The pool contract.
    pub pool: String,
    /// The borrower.
    pub account: String,
    /// Health factor normalised to 7 decimals (`hf * 10^7 / oracle_scalar`),
    /// so pools whose oracles differ in decimals order alike.
    pub health_factor: i128,
    /// Reserve index to b-token amount.
    pub collateral: BTreeMap<u32, i128>,
    /// Reserve index to d-token amount.
    pub liabilities: BTreeMap<u32, i128>,
    /// The ledger this row was computed at.
    pub updated_ledger: u32,
    /// The ledger this row was flagged for an auctioneer decision at, or
    /// `None` when there is nothing to decide. Only [`Store::user`],
    /// [`Store::users_below_health`] and [`Store::users_stale`] leave this
    /// `None` unconditionally — they do not select the column, and nothing
    /// they are used for needs it. [`Store::users_needing_recheck`] is the
    /// one caller that reads the real value, because
    /// [`Store::clear_recheck`] needs it back to make its clear conditional.
    pub recheck_ledger: Option<u32>,
}

impl Store {
    /// Writes a borrower, replacing any previous row for the same pool and
    /// account.
    pub async fn upsert_user(&self, user: &TrackedUser) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO users (pool, account, health_factor, collateral, liabilities, updated_ledger)
             VALUES ($1, $2, $3::text::numeric, $4, $5, $6)
             ON CONFLICT (pool, account) DO UPDATE
               SET health_factor = EXCLUDED.health_factor,
                   collateral = EXCLUDED.collateral,
                   liabilities = EXCLUDED.liabilities,
                   updated_ledger = EXCLUDED.updated_ledger",
            user.pool,
            user.account,
            user.health_factor.to_string(),
            index_amounts_to_json(&user.collateral),
            index_amounts_to_json(&user.liabilities),
            i64::from(user.updated_ledger),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes a borrower. `true` when a row went away, which is what the
    /// tracker reports when a position closes.
    pub async fn delete_user(&self, pool: &str, account: &str) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "DELETE FROM users WHERE pool = $1 AND account = $2",
            pool,
            account
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// One borrower.
    pub async fn user(&self, pool: &str, account: &str) -> Result<Option<TrackedUser>, StoreError> {
        let row = sqlx::query!(
            "SELECT pool, account, health_factor::text AS health_factor, collateral,
                    liabilities, updated_ledger
             FROM users WHERE pool = $1 AND account = $2",
            pool,
            account
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(TrackedUser {
                pool: row.pool,
                account: row.account,
                health_factor: decimal(row.health_factor.as_deref(), "health_factor")?,
                collateral: index_amounts_from_json(&row.collateral, "collateral")?,
                liabilities: index_amounts_from_json(&row.liabilities, "liabilities")?,
                updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
                recheck_ledger: None,
            })
        })
        .transpose()
    }

    /// The pool's least healthy borrowers below `threshold`, in
    /// `(health_factor, account)` order — the order the index backing this
    /// query stores. `after` continues a walk after the last row of the
    /// previous page; `None` starts at the beginning.
    ///
    /// The threshold is exclusive, and paging is by keyset rather than by
    /// offset because rows move between pages as the tracker refreshes
    /// them: an offset would skip a borrower whose health factor fell while
    /// the scan was walking, which is the borrower the scan exists to find.
    pub async fn users_below_health(
        &self,
        pool: &str,
        threshold: i128,
        limit: i64,
        after: Option<(i128, &str)>,
    ) -> Result<Vec<TrackedUser>, StoreError> {
        let (after_health, after_account) = match after {
            Some((health, account)) => (Some(health.to_string()), Some(account)),
            None => (None, None),
        };
        let rows = sqlx::query!(
            "SELECT pool, account, health_factor::text AS health_factor, collateral,
                    liabilities, updated_ledger
             FROM users
             WHERE pool = $1
               AND health_factor < $2::text::numeric
               AND ($4::text IS NULL
                    OR (health_factor, account) > ($4::text::numeric, $5))
             ORDER BY health_factor ASC, account ASC
             LIMIT $3",
            pool,
            threshold.to_string(),
            limit,
            after_health,
            after_account,
        )
        .fetch_all(&self.pool)
        .await?;
        // The four row-to-`TrackedUser` conversions in this file cannot
        // share a helper: each `sqlx::query!` call produces its own
        // anonymous row type, so there is no common type a `fn` or trait
        // could take. The duplication is the honest cost of
        // compile-time-checked queries.
        rows.into_iter()
            .map(|row| {
                Ok(TrackedUser {
                    pool: row.pool,
                    account: row.account,
                    health_factor: decimal(row.health_factor.as_deref(), "health_factor")?,
                    collateral: index_amounts_from_json(&row.collateral, "collateral")?,
                    liabilities: index_amounts_from_json(&row.liabilities, "liabilities")?,
                    updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
                    recheck_ledger: None,
                })
            })
            .collect()
    }

    /// The pool's borrowers whose row was written before `updated_before`,
    /// oldest first: the refresh pass walks these so a long-idle
    /// borrower's accrued interest is not missed.
    ///
    /// `updated_before` is an absolute ledger, compared against the row's
    /// own `updated_ledger`. A caller holding a span of ledgers subtracts
    /// it from the current one first.
    pub async fn users_stale(
        &self,
        pool: &str,
        updated_before: u32,
        limit: i64,
    ) -> Result<Vec<TrackedUser>, StoreError> {
        let rows = sqlx::query!(
            "SELECT pool, account, health_factor::text AS health_factor, collateral,
                    liabilities, updated_ledger
             FROM users
             WHERE pool = $1 AND updated_ledger < $2
             ORDER BY updated_ledger ASC
             LIMIT $3",
            pool,
            i64::from(updated_before),
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(TrackedUser {
                    pool: row.pool,
                    account: row.account,
                    health_factor: decimal(row.health_factor.as_deref(), "health_factor")?,
                    collateral: index_amounts_from_json(&row.collateral, "collateral")?,
                    liabilities: index_amounts_from_json(&row.liabilities, "liabilities")?,
                    updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
                    recheck_ledger: None,
                })
            })
            .collect()
    }

    /// How many borrowers the bot tracks in a pool.
    pub async fn count_users(&self, pool: &str) -> Result<i64, StoreError> {
        let row = sqlx::query!(
            "SELECT count(*) AS \"count!\" FROM users WHERE pool = $1",
            pool
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.count)
    }

    /// Flags `account` for an auctioneer decision, recording the ledger the
    /// flag was raised at. Raising a flag that is already up moves it
    /// forward: the newest reason to look is the one worth recording.
    pub async fn flag_recheck(
        &self,
        pool: &str,
        account: &str,
        ledger: u32,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE users SET recheck_ledger = $3 WHERE pool = $1 AND account = $2",
            pool,
            account,
            i64::from(ledger),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The pool's flagged rows, oldest flag first, at most `limit`. The
    /// order is what keeps a flag from starving behind a busier one. Each
    /// row's `recheck_ledger` is the flag it was read under, which the
    /// caller passes back to [`Store::clear_recheck`].
    pub async fn users_needing_recheck(
        &self,
        pool: &str,
        limit: i64,
    ) -> Result<Vec<TrackedUser>, StoreError> {
        let rows = sqlx::query!(
            "SELECT pool, account, health_factor::text AS health_factor, collateral,
                    liabilities, updated_ledger, recheck_ledger
             FROM users
             WHERE pool = $1 AND recheck_ledger IS NOT NULL
             ORDER BY recheck_ledger ASC, account ASC
             LIMIT $2",
            pool,
            limit,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(TrackedUser {
                    pool: row.pool,
                    account: row.account,
                    health_factor: decimal(row.health_factor.as_deref(), "health_factor")?,
                    collateral: index_amounts_from_json(&row.collateral, "collateral")?,
                    liabilities: index_amounts_from_json(&row.liabilities, "liabilities")?,
                    updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
                    recheck_ledger: row
                        .recheck_ledger
                        .map(|value| ledger(value, "recheck_ledger"))
                        .transpose()?,
                })
            })
            .collect()
    }

    /// Clears the flag this decision saw, and only that one: `flagged_at`
    /// makes the clear conditional, so a flag raised again while the
    /// auctioneer was deciding survives the decision that did not account
    /// for it. `false` means exactly that happened.
    pub async fn clear_recheck(
        &self,
        pool: &str,
        account: &str,
        flagged_at: u32,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE users SET recheck_ledger = NULL
             WHERE pool = $1 AND account = $2 AND recheck_ledger = $3",
            pool,
            account,
            i64::from(flagged_at),
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
}

/// What kind of submission the auctioneer made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreationKind {
    /// `new_auction` for a user liquidation.
    Auction,
    /// `bad_debt`, which the contract sizes itself.
    BadDebt,
}

impl CreationKind {
    /// The `kind` column's value. The column is text with a `CHECK`, so a
    /// reader can see what happened without this crate.
    fn as_str(self) -> &'static str {
        match self {
            Self::Auction => "auction",
            Self::BadDebt => "bad_debt",
        }
    }
}

/// One auctioneer submission, recorded whether or not it was sent: in
/// dry-run the decision is the whole artefact, and an audit that only kept
/// live submissions could not answer what the bot would have done.
#[derive(Debug, Clone)]
pub struct CreationRecord {
    pub kind: CreationKind,
    pub pool: String,
    pub account: String,
    /// The percent the auction named; `None` for bad debt, which has none.
    pub percent: Option<FillPercent>,
    pub bid: Vec<String>,
    pub lot: Vec<String>,
    /// The ledger the decision was taken at.
    pub ledger: u32,
    pub dry_run: bool,
    /// The transaction, once there is one.
    pub tx_hash: Option<String>,
}

impl Store {
    /// Records a creation and returns its id.
    pub async fn record_creation(&self, creation: &CreationRecord) -> Result<i64, StoreError> {
        let percent = creation
            .percent
            .map(|percent| {
                i16::try_from(percent.get()).map_err(|_| StoreError::Decimal {
                    column: "percent",
                    value: percent.get().to_string(),
                })
            })
            .transpose()?;
        let row = sqlx::query!(
            "INSERT INTO creations (tx_hash, kind, pool, account, percent, bid, lot, ledger, dry_run)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             RETURNING id",
            creation.tx_hash,
            creation.kind.as_str(),
            creation.pool,
            creation.account,
            percent,
            serde_json::to_value(&creation.bid).map_err(|error| StoreError::Json {
                column: "bid",
                detail: error.to_string(),
            })?,
            serde_json::to_value(&creation.lot).map_err(|error| StoreError::Json {
                column: "lot",
                detail: error.to_string(),
            })?,
            i64::from(creation.ledger),
            creation.dry_run,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.id)
    }
}

/// An auction the bot knows about: opened by a `new_auction` event, reduced
/// by a partial `fill_auction`, removed by a full fill or a
/// `delete_auction`. `fill_ledger` is the filler's current plan, not
/// anything the chain says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedAuction {
    /// The pool contract.
    pub pool: String,
    /// The account being auctioned.
    pub account: String,
    /// Which auction: user liquidation, bad debt or interest.
    pub auction_type: AuctionType,
    /// The ledger the auction was created in, from which its Dutch ramp is
    /// measured.
    pub start_ledger: u32,
    /// The ledger the filler intends to fill at, once it has planned one.
    pub fill_ledger: Option<u32>,
    /// The filler's planned fill percent, `None` until it plans one. This
    /// is not a record of the auction's creation — the event's own percent
    /// at creation time is already embodied in `bid` and `lot` — and what
    /// was actually filled is recorded separately in `fills`.
    pub percent: Option<FillPercent>,
    /// Asset address to amount the filler pays.
    pub bid: BTreeMap<String, i128>,
    /// Asset address to amount the filler receives.
    pub lot: BTreeMap<String, i128>,
    /// The ledger this row was last written at.
    pub updated_ledger: u32,
}

/// The `smallint` an auction type is stored as.
fn auction_type_code(auction_type: AuctionType) -> i16 {
    match auction_type {
        AuctionType::UserLiquidation => 0,
        AuctionType::BadDebt => 1,
        AuctionType::Interest => 2,
    }
}

/// The inverse, rejecting a discriminant the contract never emits.
fn auction_type_from_code(code: i16) -> Result<AuctionType, StoreError> {
    let value = u32::try_from(code).map_err(|_| StoreError::Decimal {
        column: "auction_type",
        value: code.to_string(),
    })?;
    AuctionType::try_from(value).map_err(|_| StoreError::Decimal {
        column: "auction_type",
        value: code.to_string(),
    })
}

/// A `smallint` percent back into the 1-to-100 range the contract uses,
/// rejecting anything `FillPercent` would reject.
fn percent_from_code(code: i16) -> Result<FillPercent, StoreError> {
    let value = u32::try_from(code).map_err(|_| StoreError::Decimal {
        column: "percent",
        value: code.to_string(),
    })?;
    FillPercent::try_from(value).map_err(|_| StoreError::Decimal {
        column: "percent",
        value: code.to_string(),
    })
}

impl Store {
    /// Writes an auction, replacing any previous row for the same pool,
    /// account and type.
    pub async fn upsert_auction(&self, auction: &TrackedAuction) -> Result<(), StoreError> {
        let percent = auction
            .percent
            .map(|percent| {
                i16::try_from(percent.get()).map_err(|_| StoreError::Decimal {
                    column: "percent",
                    value: percent.get().to_string(),
                })
            })
            .transpose()?;
        sqlx::query!(
            "INSERT INTO auctions (pool, account, auction_type, start_ledger, fill_ledger,
                                   percent, bid, lot, updated_ledger)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (pool, account, auction_type) DO UPDATE
               SET start_ledger = EXCLUDED.start_ledger,
                   fill_ledger = EXCLUDED.fill_ledger,
                   percent = EXCLUDED.percent,
                   bid = EXCLUDED.bid,
                   lot = EXCLUDED.lot,
                   updated_ledger = EXCLUDED.updated_ledger",
            auction.pool,
            auction.account,
            auction_type_code(auction.auction_type),
            i64::from(auction.start_ledger),
            auction.fill_ledger.map(i64::from),
            percent,
            asset_amounts_to_json(&auction.bid),
            asset_amounts_to_json(&auction.lot),
            i64::from(auction.updated_ledger),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes an auction. `true` when a row went away.
    pub async fn delete_auction(
        &self,
        pool: &str,
        account: &str,
        auction_type: AuctionType,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "DELETE FROM auctions WHERE pool = $1 AND account = $2 AND auction_type = $3",
            pool,
            account,
            auction_type_code(auction_type),
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// One auction.
    ///
    /// A discriminant this crate never writes cannot match a typed lookup,
    /// so a corrupted row is reported by [`Store::open_auctions`], which
    /// reads every row, rather than here.
    pub async fn auction(
        &self,
        pool: &str,
        account: &str,
        auction_type: AuctionType,
    ) -> Result<Option<TrackedAuction>, StoreError> {
        let row = sqlx::query!(
            "SELECT pool, account, auction_type, start_ledger, fill_ledger, percent,
                    bid, lot, updated_ledger
             FROM auctions WHERE pool = $1 AND account = $2 AND auction_type = $3",
            pool,
            account,
            auction_type_code(auction_type),
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(TrackedAuction {
                pool: row.pool,
                account: row.account,
                auction_type: auction_type_from_code(row.auction_type)?,
                start_ledger: ledger(row.start_ledger, "start_ledger")?,
                fill_ledger: row
                    .fill_ledger
                    .map(|value| ledger(value, "fill_ledger"))
                    .transpose()?,
                percent: row.percent.map(percent_from_code).transpose()?,
                bid: asset_amounts_from_json(&row.bid, "bid")?,
                lot: asset_amounts_from_json(&row.lot, "lot")?,
                updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
            })
        })
        .transpose()
    }

    /// Every auction the bot has open in a pool, oldest first: the filler
    /// walks them in the order they became fillable. The order is total —
    /// the three sort keys are the primary key plus the start ledger — so
    /// two auction types opened for one account in one ledger do not tie,
    /// and a caller that pages or compares runs sees one stable order.
    pub async fn open_auctions(&self, pool: &str) -> Result<Vec<TrackedAuction>, StoreError> {
        let rows = sqlx::query!(
            "SELECT pool, account, auction_type, start_ledger, fill_ledger, percent,
                    bid, lot, updated_ledger
             FROM auctions WHERE pool = $1
             ORDER BY start_ledger ASC, account ASC, auction_type ASC",
            pool
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(TrackedAuction {
                    pool: row.pool,
                    account: row.account,
                    auction_type: auction_type_from_code(row.auction_type)?,
                    start_ledger: ledger(row.start_ledger, "start_ledger")?,
                    fill_ledger: row
                        .fill_ledger
                        .map(|value| ledger(value, "fill_ledger"))
                        .transpose()?,
                    percent: row.percent.map(percent_from_code).transpose()?,
                    bid: asset_amounts_from_json(&row.bid, "bid")?,
                    lot: asset_amounts_from_json(&row.lot, "lot")?,
                    updated_ledger: ledger(row.updated_ledger, "updated_ledger")?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
    const FILLER: &str = "GCIH7OYRDHJ3IOPFEM7DMUX3SXTVHOO2XSWLGBMSVQ3EIHPHYUTNJID3";

    #[sqlx::test(migrations = "./migrations")]
    async fn a_cursor_is_absent_until_it_is_set_and_then_it_is_the_last_write(
        pool: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        let name = events_cursor(POOL);
        assert_eq!(name, format!("events:{POOL}"));
        assert_eq!(store.cursor(&name).await.expect("read"), None);

        let first = Cursor {
            ledger: 64_291_297,
            paging_token: Some("0276-0000".to_string()),
        };
        store.set_cursor(&name, &first).await.expect("write");
        assert_eq!(store.cursor(&name).await.expect("read"), Some(first));

        // Setting it again overwrites rather than failing on the primary key.
        let second = Cursor {
            ledger: 64_291_400,
            paging_token: None,
        };
        store.set_cursor(&name, &second).await.expect("overwrite");
        assert_eq!(store.cursor(&name).await.expect("read"), Some(second));

        // Cursors are per name: another pool's is untouched.
        assert_eq!(
            store.cursor(&events_cursor("COTHER")).await.expect("read"),
            None
        );
        Ok(())
    }

    /// A ledger that does not fit `u32` cannot have come from this crate;
    /// reading it is an error, not a truncation.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_ledger_outside_u32_is_an_error(pool: sqlx::PgPool) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO cursors (name, ledger) VALUES ($1, $2)",
            "events:bad",
            i64::from(u32::MAX) + 1,
        )
        .execute(&pool)
        .await?;
        let store = Store::from_pool(pool);
        assert!(matches!(
            store.cursor("events:bad").await,
            Err(StoreError::Decimal {
                column: "ledger",
                ..
            })
        ));
        Ok(())
    }

    /// Migrations apply to an empty database and the three tables exist.
    #[sqlx::test(migrations = "./migrations")]
    async fn migrations_create_the_schema(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store.ping().await.expect("ping");
        for table in ["cursors", "users", "auctions"] {
            let found: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables
                 WHERE table_schema = current_schema() AND table_name = $1)",
            )
            .bind(table)
            .fetch_one(store.pool())
            .await?;
            assert!(found, "{table} is missing");
        }
        Ok(())
    }

    /// Running them twice is a no-op, which is what every restart does.
    #[sqlx::test(migrations = "./migrations")]
    async fn migrating_an_already_migrated_database_is_a_no_op(
        pool: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store.migrate().await.expect("second migrate");
        store.migrate().await.expect("third migrate");
        Ok(())
    }

    /// `numeric` holds a health factor no `bigint` could, and it comes back
    /// as the same `i128`.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_health_factor_beyond_bigint_round_trips(pool: sqlx::PgPool) -> sqlx::Result<()> {
        // A stroop of debt against real collateral: far past i64::MAX.
        let enormous: i128 = 123_456_789_012_345_678_901_234_567;
        sqlx::query!(
            "INSERT INTO users (pool, account, health_factor, collateral, liabilities, updated_ledger)
             VALUES ($1, $2, $3::text::numeric, '{}'::jsonb, '{}'::jsonb, $4)",
            "CPOOL",
            "GUSER",
            enormous.to_string(),
            7_i64,
        )
        .execute(&pool)
        .await?;
        let row = sqlx::query!("SELECT health_factor::text AS hf FROM users")
            .fetch_one(&pool)
            .await?;
        let read = decimal(row.hf.as_deref(), "health_factor").expect("parses");
        assert_eq!(read, enormous);
        Ok(())
    }

    #[test]
    fn amount_maps_round_trip_through_json_as_decimal_strings() {
        let mut amounts = BTreeMap::new();
        amounts.insert(0_u32, i128::MAX);
        amounts.insert(2_u32, -1_i128);
        let json = index_amounts_to_json(&amounts);
        assert_eq!(json["0"], serde_json::json!(i128::MAX.to_string()));
        assert_eq!(
            index_amounts_from_json(&json, "collateral").expect("parses"),
            amounts
        );
    }

    #[test]
    fn a_json_amount_that_is_not_a_decimal_string_is_an_error_not_a_zero() {
        for bad in [
            serde_json::json!({"0": 12}),
            serde_json::json!({"0": "twelve"}),
            serde_json::json!({"x": "12"}),
            serde_json::json!([]),
        ] {
            assert!(
                matches!(
                    index_amounts_from_json(&bad, "collateral"),
                    Err(StoreError::Json { .. })
                ),
                "{bad} should not decode"
            );
        }
    }

    #[test]
    fn a_missing_or_unparseable_decimal_is_an_error() {
        assert!(matches!(
            decimal(None, "health_factor"),
            Err(StoreError::Decimal { .. })
        ));
        assert!(matches!(
            decimal(Some("1.5"), "health_factor"),
            Err(StoreError::Decimal { .. })
        ));
    }

    fn user(account: &str, health_factor: i128, updated_ledger: u32) -> TrackedUser {
        let mut collateral = BTreeMap::new();
        collateral.insert(0_u32, 1_000_000_i128);
        let mut liabilities = BTreeMap::new();
        liabilities.insert(1_u32, 500_000_i128);
        TrackedUser {
            pool: POOL.to_string(),
            account: account.to_string(),
            health_factor,
            collateral,
            liabilities,
            updated_ledger,
            recheck_ledger: None,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_user_round_trips_and_upsert_replaces(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        assert_eq!(store.user(POOL, USER).await.expect("read"), None);

        let mut tracked = user(USER, 10_070_767, 64_291_297);
        store.upsert_user(&tracked).await.expect("insert");
        assert_eq!(
            store.user(POOL, USER).await.expect("read"),
            Some(tracked.clone())
        );
        assert_eq!(store.count_users(POOL).await.expect("count"), 1);

        tracked.health_factor = 9_500_000;
        tracked.updated_ledger = 64_291_400;
        tracked.liabilities.insert(2_u32, 7_i128);
        store.upsert_user(&tracked).await.expect("update");
        assert_eq!(store.user(POOL, USER).await.expect("read"), Some(tracked));
        assert_eq!(store.count_users(POOL).await.expect("count"), 1);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_reports_whether_a_row_went_away(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store
            .upsert_user(&user(USER, 10_070_767, 1))
            .await
            .expect("insert");
        assert!(store.delete_user(POOL, USER).await.expect("delete"));
        assert!(!store.delete_user(POOL, USER).await.expect("delete again"));
        assert_eq!(store.count_users(POOL).await.expect("count"), 0);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_unhealthiest_users_come_back_first_and_the_threshold_excludes(
        pool: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store
            .upsert_user(&user(USER, 9_000_000, 1))
            .await
            .expect("a");
        store
            .upsert_user(&user(FILLER, 8_000_000, 1))
            .await
            .expect("b");
        store
            .upsert_user(&user("GHEALTHY", 30_000_000, 1))
            .await
            .expect("c");

        let scanned = store
            .users_below_health(POOL, 12_000_000, 10, None)
            .await
            .expect("scan");
        let accounts: Vec<&str> = scanned.iter().map(|u| u.account.as_str()).collect();
        assert_eq!(accounts, [FILLER, USER], "ascending by health factor");

        assert_eq!(
            store
                .users_below_health(POOL, 12_000_000, 1, None)
                .await
                .expect("limit")
                .len(),
            1
        );
        assert!(store
            .users_below_health("COTHER", 12_000_000, 10, None)
            .await
            .expect("other")
            .is_empty());
        // The threshold is exclusive: a user exactly at it is healthy enough.
        assert!(store
            .users_below_health(POOL, 8_000_000, 10, None)
            .await
            .expect("exact")
            .is_empty());
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn stale_users_come_back_oldest_first(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store
            .upsert_user(&user(USER, 10_000_000, 100))
            .await
            .expect("a");
        store
            .upsert_user(&user(FILLER, 10_000_000, 50))
            .await
            .expect("b");
        store
            .upsert_user(&user("GFRESH", 10_000_000, 900))
            .await
            .expect("c");

        let stale = store.users_stale(POOL, 500, 10).await.expect("stale");
        let accounts: Vec<&str> = stale.iter().map(|u| u.account.as_str()).collect();
        assert_eq!(accounts, [FILLER, USER]);
        assert_eq!(
            store.users_stale(POOL, 500, 1).await.expect("limit").len(),
            1
        );
        Ok(())
    }

    fn auction(account: &str, start_ledger: u32) -> TrackedAuction {
        let mut bid = BTreeMap::new();
        bid.insert("CUSDC".to_string(), 1_000_i128);
        let mut lot = BTreeMap::new();
        lot.insert("CXLM".to_string(), 2_000_i128);
        TrackedAuction {
            pool: POOL.to_string(),
            account: account.to_string(),
            auction_type: AuctionType::UserLiquidation,
            start_ledger,
            fill_ledger: None,
            percent: None,
            bid,
            lot,
            updated_ledger: start_ledger,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_auction_round_trips_and_upsert_replaces(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        let kind = AuctionType::UserLiquidation;
        assert_eq!(store.auction(POOL, USER, kind).await.expect("read"), None);

        // Freshly opened: no fill plan yet, so `percent` is absent.
        let mut open = auction(USER, 64_291_297);
        store.upsert_auction(&open).await.expect("insert");
        assert_eq!(
            store.auction(POOL, USER, kind).await.expect("read"),
            Some(open.clone())
        );

        // The filler plans a fill ledger and a percent to fill.
        open.fill_ledger = Some(64_291_400);
        open.percent = Some(FillPercent::try_from(60).expect("60 is in range"));
        open.updated_ledger = 64_291_350;
        store.upsert_auction(&open).await.expect("update");
        assert_eq!(
            store.auction(POOL, USER, kind).await.expect("read"),
            Some(open)
        );
        Ok(())
    }

    /// The three auction types are separate rows for one account.
    #[sqlx::test(migrations = "./migrations")]
    async fn auction_types_do_not_collide(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        for kind in [
            AuctionType::UserLiquidation,
            AuctionType::BadDebt,
            AuctionType::Interest,
        ] {
            let mut row = auction(USER, 10);
            row.auction_type = kind;
            store.upsert_auction(&row).await.expect("insert");
        }
        assert_eq!(store.open_auctions(POOL).await.expect("list").len(), 3);
        assert!(store
            .delete_auction(POOL, USER, AuctionType::BadDebt)
            .await
            .expect("delete"));
        assert_eq!(store.open_auctions(POOL).await.expect("list").len(), 2);
        assert!(!store
            .delete_auction(POOL, USER, AuctionType::BadDebt)
            .await
            .expect("again"));
        Ok(())
    }

    /// Two auction types for one account in one ledger tie on the first
    /// two sort keys; the type breaks the tie, so the order is total.
    #[sqlx::test(migrations = "./migrations")]
    async fn two_auction_types_in_one_ledger_come_back_in_type_order(
        pool: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        for kind in [AuctionType::Interest, AuctionType::UserLiquidation] {
            let mut row = auction(USER, 500);
            row.auction_type = kind;
            store.upsert_auction(&row).await.expect("insert");
        }
        let open = store.open_auctions(POOL).await.expect("list");
        let types: Vec<AuctionType> = open.iter().map(|a| a.auction_type).collect();
        assert_eq!(
            types,
            [AuctionType::UserLiquidation, AuctionType::Interest],
            "the type breaks a tie on start ledger and account"
        );

        // A typed read answers for the type asked for and `None` for the
        // one that is absent, even while another type's row exists.
        assert!(store
            .auction(POOL, USER, AuctionType::Interest)
            .await
            .expect("read")
            .is_some());
        assert_eq!(
            store
                .auction(POOL, USER, AuctionType::BadDebt)
                .await
                .expect("read"),
            None
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn open_auctions_come_back_in_start_order(pool: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(pool);
        store.upsert_auction(&auction(USER, 300)).await.expect("a");
        store
            .upsert_auction(&auction(FILLER, 100))
            .await
            .expect("b");
        let open = store.open_auctions(POOL).await.expect("list");
        let accounts: Vec<&str> = open.iter().map(|a| a.account.as_str()).collect();
        assert_eq!(accounts, [FILLER, USER]);
        assert!(store
            .open_auctions("COTHER")
            .await
            .expect("other")
            .is_empty());
        Ok(())
    }

    // The `auctions` table now has `CHECK` constraints mirroring both
    // guards below, so neither a raw `INSERT` nor `Store::upsert_auction`
    // (which only ever writes a valid `AuctionType`/`FillPercent`) can put a
    // corrupted row in the database for an integration test to read back.
    // These two tests exercise the guards directly instead.

    /// A discriminant the contract never emits cannot decode. `auction_type`
    /// is also the store's lookup key, so a row this corrupted is filtered
    /// out of a typed `Store::auction` lookup by SQL before decoding ever
    /// runs: only `Store::open_auctions`, which decodes every row of a
    /// pool, ever calls this on such a row.
    #[test]
    fn auction_type_from_code_rejects_a_discriminant_the_contract_never_emits() {
        assert!(matches!(
            auction_type_from_code(7),
            Err(StoreError::Decimal {
                column: "auction_type",
                ..
            })
        ));
    }

    /// A percent outside 1 to 100 cannot decode. Unlike `auction_type`,
    /// `percent` is not part of the lookup key, so a corrupted percent
    /// would be decoded by both `Store::auction` and `Store::open_auctions`
    /// alike.
    #[test]
    fn percent_from_code_rejects_a_value_outside_one_to_a_hundred() {
        assert!(matches!(
            percent_from_code(200),
            Err(StoreError::Decimal {
                column: "percent",
                ..
            })
        ));
    }

    /// The scan walks every page: the keyset continues after the last row of
    /// the previous page, so a user below the threshold cannot be hidden by a
    /// limit. Ordering is `(health_factor, account)` — the index's own order —
    /// and two users sharing a health factor are separated by the account.
    #[sqlx::test(migrations = "./migrations")]
    async fn users_below_health_pages_through_every_user(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        for (account, health) in [
            ("A", 1_000_000),
            ("B", 2_000_000),
            ("C", 2_000_000),
            ("D", 3_000_000),
        ] {
            store
                .upsert_user(&TrackedUser {
                    pool: POOL.to_string(),
                    account: account.to_string(),
                    health_factor: health,
                    collateral: BTreeMap::new(),
                    liabilities: BTreeMap::from([(0, 1)]),
                    updated_ledger: 10,
                    recheck_ledger: None,
                })
                .await
                .expect("upsert");
        }

        let mut seen = Vec::new();
        let mut after: Option<(i128, String)> = None;
        loop {
            let page = store
                .users_below_health(
                    POOL,
                    5_000_000,
                    2,
                    after.as_ref().map(|(hf, account)| (*hf, account.as_str())),
                )
                .await
                .expect("page");
            if page.is_empty() {
                break;
            }
            if let Some(last) = page.last() {
                after = Some((last.health_factor, last.account.clone()));
            }
            seen.extend(page.into_iter().map(|user| user.account));
        }
        assert_eq!(
            seen,
            vec!["A", "B", "C", "D"],
            "every user, in health then account order"
        );
        Ok(())
    }

    /// A flag survives the process: it is a column on the row, and clearing it
    /// is conditional on the ledger it was set at, so a flag raised again while
    /// the auctioneer was deciding is not cleared by that decision.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_recheck_flag_is_durable_and_clears_only_the_flag_it_saw(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        store
            .upsert_user(&TrackedUser {
                pool: POOL.to_string(),
                account: USER.to_string(),
                health_factor: 9_000_000,
                collateral: BTreeMap::new(),
                liabilities: BTreeMap::from([(0, 1)]),
                updated_ledger: 10,
                recheck_ledger: None,
            })
            .await
            .expect("upsert");

        assert!(
            store
                .users_needing_recheck(POOL, 10)
                .await
                .expect("read")
                .is_empty(),
            "a fresh row is not flagged"
        );

        store.flag_recheck(POOL, USER, 100).await.expect("flag");
        let flagged = store.users_needing_recheck(POOL, 10).await.expect("read");
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].account, USER);
        assert_eq!(flagged[0].recheck_ledger, Some(100));

        // A newer flag arrives while the auctioneer is deciding on the old one.
        store.flag_recheck(POOL, USER, 200).await.expect("reflag");
        assert!(
            !store
                .clear_recheck(POOL, USER, 100)
                .await
                .expect("stale clear"),
            "clearing the flag it saw must not clear a newer one"
        );
        assert_eq!(
            store
                .users_needing_recheck(POOL, 10)
                .await
                .expect("read")
                .len(),
            1,
            "the newer flag stands"
        );

        assert!(store.clear_recheck(POOL, USER, 200).await.expect("clear"));
        assert!(store
            .users_needing_recheck(POOL, 10)
            .await
            .expect("read")
            .is_empty());
        Ok(())
    }

    /// An upsert must not clear a pending flag: the tracker rewrites the row
    /// whenever it refreshes a user, and the auctioneer's work is not that
    /// refresh's to discard.
    #[sqlx::test(migrations = "./migrations")]
    async fn refreshing_a_user_leaves_its_recheck_flag_alone(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let user = TrackedUser {
            pool: POOL.to_string(),
            account: USER.to_string(),
            health_factor: 9_000_000,
            collateral: BTreeMap::new(),
            liabilities: BTreeMap::from([(0, 1)]),
            updated_ledger: 10,
            recheck_ledger: None,
        };
        store.upsert_user(&user).await.expect("upsert");
        store.flag_recheck(POOL, USER, 100).await.expect("flag");
        store
            .upsert_user(&TrackedUser {
                updated_ledger: 20,
                ..user
            })
            .await
            .expect("re-upsert");
        assert_eq!(
            store
                .users_needing_recheck(POOL, 10)
                .await
                .expect("read")
                .len(),
            1,
            "the refresh rewrote the row without dropping the flag"
        );
        Ok(())
    }

    /// A creation is recorded whether or not it was sent, because the audit
    /// exists to say what the bot decided, and in dry-run the decision is all
    /// there is. A bad-debt creation carries no percent.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_dry_run_creation_is_recorded_with_no_hash(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let id = store
            .record_creation(&CreationRecord {
                kind: CreationKind::Auction,
                pool: POOL.to_string(),
                account: USER.to_string(),
                percent: Some(FillPercent::try_from(42).expect("percent")),
                bid: vec!["USDC".to_string()],
                lot: vec!["XLM".to_string()],
                ledger: 64_271_340,
                dry_run: true,
                tx_hash: None,
            })
            .await
            .expect("record");
        assert!(id > 0, "the row is identified by its serial");

        let bad_debt = store
            .record_creation(&CreationRecord {
                kind: CreationKind::BadDebt,
                pool: POOL.to_string(),
                account: USER.to_string(),
                percent: None,
                bid: Vec::new(),
                lot: Vec::new(),
                ledger: 64_271_341,
                dry_run: false,
                tx_hash: Some("ab".repeat(32)),
            })
            .await
            .expect("record bad debt");
        assert!(bad_debt > id, "the serial advances");
        Ok(())
    }
}
