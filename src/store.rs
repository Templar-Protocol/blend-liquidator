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

/// A failure talking to the store, or reading a value it returned.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The connection pool could not be opened.
    #[error("connecting to the database: {0}")]
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
///
/// No caller until the tracker (Task 3) reads `users.health_factor` back;
/// `#[allow(dead_code)]` until then, removed when that task lands.
#[allow(dead_code)]
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
///
/// No caller until the tracker (Task 3) writes `users.collateral` and
/// `users.liabilities`; `#[allow(dead_code)]` until then, removed when that
/// task lands.
#[allow(dead_code)]
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
///
/// No caller until the tracker (Task 3) reads `users.collateral` and
/// `users.liabilities` back; `#[allow(dead_code)]` until then, removed when
/// that task lands.
#[allow(dead_code)]
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
///
/// No caller until Task 4 wires the auction queries; `#[allow(dead_code)]`
/// until then, removed when that task lands.
#[allow(dead_code)]
fn asset_amounts_to_json(amounts: &BTreeMap<String, i128>) -> Value {
    Value::Object(
        amounts
            .iter()
            .map(|(asset, amount)| (asset.clone(), Value::String(amount.to_string())))
            .collect(),
    )
}

/// The inverse of `asset_amounts_to_json`.
///
/// No caller until Task 4 wires the auction queries; `#[allow(dead_code)]`
/// until then, removed when that task lands.
#[allow(dead_code)]
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
    /// The RPC's paging token for the next `getEvents` page, when the
    /// poller stopped mid-ledger. `None` means the ledger is complete.
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

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    /// No caller until Task 3's tests exercise `Store::user`/`upsert_user`/
    /// `delete_user`; `#[allow(dead_code)]` until then, removed when that
    /// task lands.
    #[allow(dead_code)]
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
    /// No caller until Task 3's tests exercise the same methods for a
    /// second account; `#[allow(dead_code)]` until then, removed when that
    /// task lands.
    #[allow(dead_code)]
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
}
