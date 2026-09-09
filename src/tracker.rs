//! Applying what the chain says to what the bot stores.
//!
//! The tracker is the only writer of the `users` and `auctions` tables, so
//! ordering is its own: it applies a ledger's events, then refreshes the
//! accounts those events named, in one batched read per pool per tick.
//!
//! Two rules make a replayed ledger harmless. Applying an event is
//! idempotent — every write is an upsert or a delete keyed by what the
//! event names — and a user's row is recomputed from chain rather than
//! adjusted, so an event applied twice cannot drift a balance. The chain,
//! not the event, is the source of every number the store holds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use tokio::sync::watch;

use crate::chain::pool::PoolReader;
use crate::chain::rpc::RpcClient;
use crate::chain::xdr::PoolEvent;
use crate::chain::ChainError;
use crate::ledger::LedgerTick;
use crate::math::{mul_floor, MathError, SCALAR_7};
use crate::store::{Store, StoreError, TrackedAuction, TrackedUser};

/// A failure applying chain state to the store.
#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    /// Reading the chain failed.
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
    /// Writing the store failed.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// Normalising a health factor overflowed.
    #[error("math: {0}")]
    Math(#[from] MathError),
}

/// What one refresh did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshOutcome {
    /// Accounts written or updated.
    pub tracked: usize,
    /// Accounts removed because they no longer owe anything.
    pub removed: usize,
}

/// What one seed did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeedOutcome {
    /// The refresh that valued every account the sources named.
    pub refresh: RefreshOutcome,
    /// How many sources failed to answer. Non-zero means this seed's
    /// coverage is incomplete, which spec §4 makes a warning to be retried
    /// on the next full scan rather than a failure.
    pub failed_sources: usize,
    /// Whether the seed stopped before valuing every account it collected,
    /// which today means a shutdown arrived mid-seed. It is not an error —
    /// the accounts already written are written — but the seed is not
    /// finished, and a caller that records progress must not record it as
    /// though it were.
    pub stopped_early: bool,
}

impl SeedOutcome {
    /// Whether this seed reached every source and valued every account it
    /// collected. Only a complete seed may be recorded as one: a caller
    /// that commits progress for an incomplete seed makes the gap durable.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed_sources == 0 && !self.stopped_early
    }
}

/// Applies events and refreshes users for one store.
#[derive(Debug, Clone, Copy)]
pub struct Tracker<'a> {
    rpc: &'a RpcClient,
    store: &'a Store,
}

impl<'a> Tracker<'a> {
    /// A tracker writing to `store` and reading through `rpc`.
    #[must_use]
    pub fn new(rpc: &'a RpcClient, store: &'a Store) -> Self {
        Self { rpc, store }
    }

    /// The chain client this tracker reads through, so a caller driving it
    /// reads the same chain rather than opening a second client.
    #[must_use]
    pub fn rpc(&self) -> &'a RpcClient {
        self.rpc
    }

    /// The store this tracker writes, so a caller reading back what it
    /// wrote cannot read a different one.
    #[must_use]
    pub fn store(&self) -> &'a Store {
        self.store
    }

    /// Applies one event's auction bookkeeping and returns the accounts it
    /// names, for the caller to refresh at the tick. Writes no row in the
    /// `users` table: refreshing is the tick's job, not the event's. The
    /// tracker only ever records what the chain says an auction *is* — its
    /// bid, lot and start ledger; what to fill of it is the filler's own
    /// decision, planned later and written into the same row by Phase 5.
    pub async fn apply(
        &self,
        pool: &str,
        ledger: u32,
        event: &PoolEvent,
    ) -> Result<Vec<String>, TrackerError> {
        match event {
            PoolEvent::NewAuction {
                auction_type,
                user,
                auction,
                ..
            } => {
                self.store
                    .upsert_auction(&TrackedAuction {
                        pool: pool.to_string(),
                        account: user.clone(),
                        auction_type: *auction_type,
                        start_ledger: auction.block,
                        fill_ledger: None,
                        percent: None,
                        bid: auction.bid.clone(),
                        lot: auction.lot.clone(),
                        updated_ledger: ledger,
                    })
                    .await?;
            }
            PoolEvent::FillAuction {
                auction_type,
                user,
                fill_percent,
                ..
            } => {
                if *fill_percent >= 100 {
                    self.store.delete_auction(pool, user, *auction_type).await?;
                } else {
                    // A partial fill leaves a remainder the contract
                    // computed; read it rather than subtracting the filled
                    // side ourselves — the contract owns that arithmetic
                    // and the entry is authoritative. The remainder carries
                    // no fill plan of its own, so `percent` starts absent
                    // again.
                    let reader = PoolReader::new(self.rpc, pool);
                    match reader.auction(user, *auction_type).await? {
                        Some((at, remaining)) => {
                            self.store
                                .upsert_auction(&TrackedAuction {
                                    pool: pool.to_string(),
                                    account: user.clone(),
                                    auction_type: *auction_type,
                                    start_ledger: remaining.block,
                                    fill_ledger: None,
                                    percent: None,
                                    bid: remaining.bid,
                                    lot: remaining.lot,
                                    updated_ledger: at,
                                })
                                .await?;
                        }
                        None => {
                            self.store.delete_auction(pool, user, *auction_type).await?;
                        }
                    }
                }
            }
            PoolEvent::DeleteAuction {
                auction_type, user, ..
            } => {
                self.store.delete_auction(pool, user, *auction_type).await?;
            }
            _ => {}
        }
        Ok(event
            .affected_accounts()
            .into_iter()
            .map(str::to_string)
            .collect())
    }

    /// Re-reads `accounts` from chain in one snapshot and writes each row,
    /// deleting the ones that no longer owe anything.
    ///
    /// Reserves are accrued to `tick.close_time`, so an account refreshed
    /// at ledger *N* is valued as the contract would value it in ledger
    /// *N* — **unless the chain has already moved past that tick**, which
    /// is the ordinary case rather than a rare race: `getEvents` has no
    /// end ledger, so a pass delivers events from ledgers newer than the
    /// head it read, and the snapshot taken to refresh them is newer
    /// still. The accrual target is therefore the later of the tick's
    /// close time and the newest `last_time` the snapshot holds. It never
    /// runs backwards, because a reserve entry is a stored state the
    /// contract only ever accrues *forward* from — `Reserve::accrue`
    /// rightly refuses the other direction, and clamping here keeps that
    /// refusal from turning "the chain moved" into a failed refresh.
    ///
    /// The row records `snapshot.ledger`, the ledger the positions were
    /// actually read at, not the tick: that is what the stale-refresh pass
    /// then measures staleness against.
    pub async fn refresh(
        &self,
        pool: &str,
        accounts: &[String],
        tick: LedgerTick,
    ) -> Result<RefreshOutcome, TrackerError> {
        if accounts.is_empty() {
            return Ok(RefreshOutcome::default());
        }
        let borrowed: Vec<&str> = accounts.iter().map(String::as_str).collect();
        let snapshot = PoolReader::new(self.rpc, pool).snapshot(&borrowed).await?;
        // One timestamp for every reserve, so the position is valued at a
        // single instant: the earliest one at which every entry read is
        // valid. Per-reserve clamping would value one asset later than
        // another and quietly mix two ledgers into one health factor.
        let valued_at = snapshot
            .reserves
            .values()
            .map(|reserve| reserve.data.last_time)
            .max()
            .unwrap_or(0)
            .max(tick.close_time);
        let mut outcome = RefreshOutcome::default();
        for account in accounts {
            let positions = snapshot.positions.get(account);
            let owes = positions.is_some_and(|positions| !positions.liabilities.is_empty());
            let health = if owes {
                snapshot
                    .position_data(account, valued_at)?
                    .and_then(|data| data.health_factor().transpose())
                    .transpose()?
            } else {
                None
            };
            match (positions, health) {
                (Some(positions), Some(health)) => {
                    self.store
                        .upsert_user(&TrackedUser {
                            pool: pool.to_string(),
                            account: account.clone(),
                            health_factor: mul_floor(health, SCALAR_7, snapshot.prices.scalar())?,
                            collateral: positions.collateral.clone(),
                            liabilities: positions.liabilities.clone(),
                            updated_ledger: snapshot.ledger,
                        })
                        .await?;
                    outcome.tracked += 1;
                }
                _ => {
                    if self.store.delete_user(pool, account).await? {
                        outcome.removed += 1;
                    }
                }
            }
        }
        Ok(outcome)
    }

    /// Refreshes up to `batch` users whose row was written before
    /// `updated_before`, oldest first, so a long-idle borrower's accrued
    /// interest is never missed.
    ///
    /// `updated_before` is an **absolute ledger**, not a span: a caller
    /// working from a knob like `USER_REFRESH_LEDGERS` subtracts it from
    /// the tick first. Handing the span over instead compares a count of
    /// ledgers against a ledger sequence, which selects every row on a
    /// fresh network and no row at all on a live one.
    pub async fn refresh_stale(
        &self,
        pool: &str,
        tick: LedgerTick,
        updated_before: u32,
        batch: u32,
    ) -> Result<RefreshOutcome, TrackerError> {
        let limit = i64::from(batch);
        let stale = self.store.users_stale(pool, updated_before, limit).await?;
        let accounts: Vec<String> = stale.into_iter().map(|user| user.account).collect();
        self.refresh(pool, &accounts, tick).await
    }

    /// Seeds `pool`'s tracked-user set: collects accounts from every
    /// source, deduplicates them, and refreshes them from chain in batches
    /// of `batch`. A source that fails is logged and skipped rather than
    /// failing the whole seed — the spec makes a failed seed a warning, not
    /// a startup failure, because every submission still acts only on an
    /// account [`Tracker::refresh`] has itself verified from chain: an
    /// incomplete seed costs coverage, never correctness.
    pub async fn seed(
        &self,
        pool: &str,
        sources: &[SeedSource],
        tick: LedgerTick,
        batch: u32,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<SeedOutcome, TrackerError> {
        let mut accounts: BTreeSet<String> = BTreeSet::new();
        let mut outcome = SeedOutcome::default();
        for source in sources {
            match source.accounts(pool).await {
                Ok(found) => accounts.extend(found),
                Err(error) => {
                    outcome.failed_sources += 1;
                    tracing::warn!(pool, %error, "a seed source failed; skipping it");
                }
            }
        }
        let accounts: Vec<String> = accounts.into_iter().collect();
        // `chunks` panics on a zero size; a batch this small is nonsensical
        // but must not crash the seed over it.
        let batch = usize::try_from(batch).unwrap_or(usize::MAX).max(1);
        for chunk in accounts.chunks(batch) {
            // A seed of a busy pool is many sequential round trips and
            // runs before any poller does, so it is the longest stretch in
            // which a shutdown request would otherwise go unheard.
            if *shutdown.borrow() {
                tracing::warn!(pool, "shutdown requested during a seed; stopping early");
                outcome.stopped_early = true;
                break;
            }
            let result = self.refresh(pool, chunk, tick).await?;
            outcome.refresh.tracked += result.tracked;
            outcome.refresh.removed += result.removed;
        }
        Ok(outcome)
    }
}

/// A failure seeding the tracker's initial user set.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    /// The request never produced a response, or the client could not be
    /// built.
    #[error("seed request: {0}")]
    Http(#[from] reqwest::Error),
    /// The seed source answered with a non-2xx status.
    #[error("seed source status {status}")]
    Status {
        /// The HTTP status.
        status: u16,
    },
    /// A 200 whose body was not the documented shape.
    #[error("seed response shape: {0}")]
    Shape(String),
    /// The seed file could not be read or parsed.
    #[error("seed file: {0}")]
    File(String),
}

/// Renders a 7-decimal fixed-point value as its shortest decimal text,
/// without ever going through a float: `whole = value / SCALAR_7`,
/// `fraction = value % SCALAR_7`, trailing zeros trimmed. Assumes `value`
/// is non-negative, which every parsed `Decimal7` knob is.
fn render_decimal7(value: i128) -> String {
    let whole = value / SCALAR_7;
    let fraction = value % SCALAR_7;
    if fraction == 0 {
        return whole.to_string();
    }
    let digits = format!("{fraction:07}");
    let trimmed = digits.trim_end_matches('0');
    format!("{whole}.{trimmed}")
}

/// Page size the analytics API is asked for.
const SEED_PAGE_LIMIT: u32 = 500;
/// Stop paging after this many pages even if the cursor keeps coming back,
/// so a misbehaving endpoint stalls visibly instead of paging forever.
const SEED_PAGE_CAP: usize = 200;
/// Pause between pages, to stay under the endpoint's anonymous rate limit.
const SEED_PAGE_PAUSE: Duration = Duration::from_millis(200);

/// The public Blend analytics API's positions endpoint, walked by cursor.
///
/// Only `accountId` is read from each position: the API's own health
/// factor is a third party's float, and every seeded account is re-valued
/// from chain — by [`Tracker::refresh`] — before the bot trusts it.
#[derive(Debug, Clone)]
pub struct AnalyticsSeed {
    http: reqwest::Client,
    base_url: String,
    health_factor_max: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsPage {
    positions: Vec<AnalyticsPosition>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsPosition {
    account_id: String,
}

/// The most of an unparseable response that reaches an error message, and so
/// a log line: [`Tracker::seed`] logs a source's failure with `%error`. The
/// body belongs to a third party and is bounded by nothing, so a diagnostic
/// takes its shape and its size, never all of it.
const BODY_SNIPPET: usize = 200;

/// Truncates `text` to [`BODY_SNIPPET`] bytes on a character boundary,
/// naming the full length so a truncated diagnostic still says how much was
/// left out.
///
/// This bounds the *whole* composed message rather than the body alone:
/// `serde_json` renders the offending value into its own error, so a body
/// truncated on its way into `{text}` would still arrive in full through
/// `{error}`.
fn snippet(text: &str) -> String {
    if text.len() <= BODY_SNIPPET {
        return text.to_owned();
    }
    let mut end = BODY_SNIPPET;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes total)", &text[..end], text.len())
}

impl AnalyticsSeed {
    /// A client for the analytics API at `base_url`, sending
    /// `healthFactorMax` rendered from `health_factor_max`'s 7-decimal
    /// fixed point. Built the way [`crate::chain::rpc::RpcClient::new`]
    /// builds its client — a bounded timeout and connect timeout, and the
    /// crate's own user agent — rather than `reqwest::Client::new()`.
    pub fn new(base_url: &str, health_factor_max: i128) -> Result<Self, SeedError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("blend-liquidator/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            health_factor_max: render_decimal7(health_factor_max),
        })
    }

    /// Walks every page of `pool`'s positions, returning every `accountId`
    /// in the order the API gave them. A null `nextCursor`, or one the API
    /// omits entirely (an unknown pool answers this way, with no
    /// positions), both end the walk without error. Caps at `SEED_PAGE_CAP`
    /// pages, logging a warning rather than looping forever when a cursor
    /// never comes back null.
    ///
    /// Private: [`SeedSource::accounts`] is the uniform entry point across
    /// sources, matching [`FileSeed::accounts`]'s visibility.
    async fn accounts(&self, pool: &str) -> Result<Vec<String>, SeedError> {
        let url = format!("{}/v1/analytics/state/positions", self.base_url);
        let mut accounts = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages: usize = 0;
        loop {
            pages += 1;
            let mut query = vec![
                ("healthFactorMax", self.health_factor_max.clone()),
                ("poolId", pool.to_string()),
                ("limit", SEED_PAGE_LIMIT.to_string()),
            ];
            if let Some(cursor) = &cursor {
                query.push(("cursor", cursor.clone()));
            }
            let response = self.http.get(&url).query(&query).send().await?;
            let status = response.status();
            if !status.is_success() {
                return Err(SeedError::Status {
                    status: status.as_u16(),
                });
            }
            let text = response.text().await?;
            let body: AnalyticsPage = serde_json::from_str(&text)
                .map_err(|error| SeedError::Shape(snippet(&format!("{error}: {text}"))))?;
            accounts.extend(
                body.positions
                    .into_iter()
                    .map(|position| position.account_id),
            );
            let Some(next) = body.next_cursor else {
                break;
            };
            if pages >= SEED_PAGE_CAP {
                tracing::warn!(
                    pool,
                    pages,
                    "seed pagination hit the page cap without a null cursor; stopping"
                );
                break;
            }
            cursor = Some(next);
            tokio::time::sleep(SEED_PAGE_PAUSE).await;
        }
        Ok(accounts)
    }
}

/// A static file of pool address to tracked-account list, for a network the
/// analytics API does not cover, or as a supplement to it.
#[derive(Debug, Clone)]
pub struct FileSeed {
    accounts: BTreeMap<String, Vec<String>>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSeedFile {
    accounts: BTreeMap<String, Vec<String>>,
}

impl FileSeed {
    /// Reads and parses the seed file: one `[accounts]` table mapping pool
    /// address to account list.
    pub fn load(path: &Path) -> Result<Self, SeedError> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| SeedError::File(format!("{}: {error}", path.display())))?;
        let raw: RawSeedFile = toml::from_str(&text)
            .map_err(|error| SeedError::File(format!("{}: {error}", path.display())))?;
        Ok(Self {
            accounts: raw.accounts,
        })
    }

    /// The accounts the file lists for `pool`, or empty when it does not
    /// mention that pool at all.
    fn accounts(&self, pool: &str) -> Vec<String> {
        self.accounts.get(pool).cloned().unwrap_or_default()
    }
}

/// Where the tracker's initial user set comes from.
#[derive(Debug, Clone)]
pub enum SeedSource {
    /// The public Blend analytics API.
    Analytics(AnalyticsSeed),
    /// A static file.
    File(FileSeed),
}

impl SeedSource {
    /// `pool`'s accounts from this source.
    pub async fn accounts(&self, pool: &str) -> Result<Vec<String>, SeedError> {
        match self {
            Self::Analytics(analytics) => analytics.accounts(pool).await,
            Self::File(file) => Ok(file.accounts(pool)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use stellar_xdr::{ContractDataDurability, ContractDataEntry, ExtensionPoint, LedgerEntryData};

    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::chain::script::ScriptedRpc;
    use crate::chain::xdr::encode::{
        address, i128_val, map, sc_address, symbol, to_base64, vec as sc_vec,
    };
    use crate::chain::xdr::keys;
    use crate::chain::xdr::AuctionType;
    use crate::harness::{self, GOLDEN_HEALTH, POOL, USER_ONE, USER_TWO};
    use crate::math::AuctionData;

    /// Two more addresses the fixture ledger holds no position for: real,
    /// valid strkeys pulled from elsewhere in this crate's fixtures rather
    /// than invented, so encoding them into a ledger key never fails.
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
    const REPAID: &str = "GCC4A2FN5BIXW6I57LKMP4XK7WVNZJWDCD5JZGGQKAI45PNTPC5NU6U4";
    const NEVER_TRACKED: &str = "GAX2VVWVHU5YQY5J3NJBXKHI3FFKZN54BE6GRJCWSIKSBZTQWJJNJMPC";

    /// Writes `contents` to a fresh temporary file and returns its path, for
    /// the seed-file tests: a real path `FileSeed::load` reads, rather than
    /// a string it never gets to parse from disk. Every caller removes the
    /// file itself once done with it.
    fn write_temp_seed_file(contents: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "blend-liquidator-seed-test-{}-{id}.toml",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("write temp seed file");
        path
    }

    fn stale_user(account: &str, updated_ledger: u32) -> TrackedUser {
        let mut collateral = BTreeMap::new();
        collateral.insert(0_u32, 1_000_000_i128);
        let mut liabilities = BTreeMap::new();
        liabilities.insert(1_u32, 500_000_i128);
        TrackedUser {
            pool: POOL.to_string(),
            account: account.to_string(),
            health_factor: 20_000_000,
            collateral,
            liabilities,
            updated_ledger,
        }
    }

    /// A `ContractData` auction entry at `block`, with the given bid and lot
    /// on a single asset each, matching the shape `chain::pool`'s auction
    /// test builds.
    fn auction_entry_xdr(bid_amount: i128, lot_amount: i128, block: u32) -> String {
        let side = |amount: i128| map(vec![(address(USDC).unwrap(), i128_val(amount))]).unwrap();
        let auction = map(vec![
            (symbol("bid").unwrap(), side(bid_amount)),
            (symbol("block").unwrap(), stellar_xdr::ScVal::U32(block)),
            (symbol("lot").unwrap(), side(lot_amount)),
        ])
        .unwrap();
        let auction_key = map(vec![
            (symbol("auct_type").unwrap(), stellar_xdr::ScVal::U32(0)),
            (symbol("user").unwrap(), address(USER_ONE).unwrap()),
        ])
        .unwrap();
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(POOL).unwrap(),
            key: sc_vec(vec![symbol("Auction").unwrap(), auction_key]).unwrap(),
            durability: ContractDataDurability::Temporary,
            val: auction,
        });
        to_base64(&entry).unwrap()
    }

    fn entry(key: &stellar_xdr::LedgerKey, xdr: &str) -> serde_json::Value {
        json!({"key": to_base64(key).unwrap(), "xdr": xdr, "lastModifiedLedgerSeq": 1, "liveUntilLedgerSeq": 99_999_999})
    }

    /// The fixture's two borrowers land in the store with the golden health
    /// factors, normalised to 7 decimals — the oracle's scalar is 10^7 here,
    /// so normalising is the identity and the stored values are the same
    /// numbers `chain::xdr::decode`'s test derives.
    #[sqlx::test(migrations = "./migrations")]
    async fn refreshing_the_fixtures_users_stores_their_golden_health_factors(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[USER_ONE, USER_TWO]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let tick = harness::fixture_tick();

        let outcome = tracker
            .refresh(POOL, &[USER_ONE.to_string(), USER_TWO.to_string()], tick)
            .await
            .expect("refresh");
        assert_eq!(
            outcome,
            RefreshOutcome {
                tracked: 2,
                removed: 0
            }
        );

        for (account, golden) in GOLDEN_HEALTH {
            let row = store
                .user(POOL, account)
                .await
                .expect("read")
                .unwrap_or_else(|| panic!("{account} should be tracked"));
            assert_eq!(row.health_factor, golden);
            assert_eq!(row.pool, POOL);
            // The snapshot is the fixture's own ledger, which is also this
            // tick's, so this pins both: the row records the ledger it was
            // read at, and here that is the ledger it was valued at.
            assert_eq!(row.updated_ledger, tick.sequence);
        }
        assert_eq!(store.count_users(POOL).await.expect("count"), 2);
        Ok(())
    }

    /// A snapshot newer than the tick is the ordinary case, not a race:
    /// `getEvents` carries no end ledger, so a pass delivers events from
    /// ledgers newer than the head it read, and the snapshot taken to
    /// value them is newer still. Accruing must clamp forward to the
    /// newest reserve entry rather than refuse to run backwards, and the
    /// row must record the ledger the positions were read at.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_snapshot_newer_than_the_tick_is_valued_at_its_newest_reserve(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        for _ in 0..3 {
            harness::script_snapshot(&rpc, &[USER_ONE]);
        }
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let fixture = harness::fixture_tick();

        // What the fixture's own entries say: every reserve was last
        // touched before the ledger closed, so a tick between the two is
        // exactly the case this clamp exists for.
        let snapshot = PoolReader::new(&client, POOL)
            .snapshot(&[USER_ONE])
            .await
            .expect("snapshot");
        let newest = snapshot
            .reserves
            .values()
            .map(|reserve| reserve.data.last_time)
            .max()
            .expect("the fixture pool has reserves");
        assert!(
            newest < fixture.close_time,
            "the fixture's newest reserve entry ({newest}) precedes its close time"
        );

        // A tick four ledgers behind the snapshot, whose close time is
        // older than that newest entry: `Reserve::accrue` refuses to run
        // backwards, so without the clamp this refresh is a hard error.
        let stale = LedgerTick {
            sequence: fixture.sequence - 4,
            close_time: newest - 1,
        };
        tracker
            .refresh(POOL, &[USER_ONE.to_string()], stale)
            .await
            .expect("a tick behind the chain still values the position");
        let after_stale = store
            .user(POOL, USER_ONE)
            .await
            .expect("read")
            .expect("a row");
        assert_eq!(
            after_stale.updated_ledger, snapshot.ledger,
            "the row records the ledger it was read at, not the older tick"
        );

        // Valuing at that newest entry directly is the same number, which
        // is what clamping forward to it means.
        let at_newest = LedgerTick {
            sequence: stale.sequence,
            close_time: newest,
        };
        tracker
            .refresh(POOL, &[USER_ONE.to_string()], at_newest)
            .await
            .expect("refresh");
        let after_newest = store
            .user(POOL, USER_ONE)
            .await
            .expect("read")
            .expect("a row");
        assert_eq!(
            after_stale.health_factor, after_newest.health_factor,
            "a stale tick values at the newest reserve entry, not at itself"
        );
        Ok(())
    }

    /// An account the ledger has no positions entry for is not tracked, and
    /// one that repays everything is deleted rather than stored empty.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_account_without_liabilities_is_not_tracked(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        // REPAID already has a row from an earlier, healthier refresh; the
        // fixture ledger holds no positions entry for it now.
        store
            .upsert_user(&stale_user(REPAID, 1))
            .await
            .expect("seed a stale row");
        assert!(store
            .user(POOL, NEVER_TRACKED)
            .await
            .expect("read")
            .is_none());

        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[REPAID, NEVER_TRACKED]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);

        let outcome = tracker
            .refresh(
                POOL,
                &[REPAID.to_string(), NEVER_TRACKED.to_string()],
                harness::fixture_tick(),
            )
            .await
            .expect("refresh");
        assert_eq!(
            outcome,
            RefreshOutcome {
                tracked: 0,
                removed: 1
            },
            "only REPAID had a row to remove"
        );
        assert_eq!(store.user(POOL, REPAID).await.expect("read"), None);
        assert_eq!(store.user(POOL, NEVER_TRACKED).await.expect("read"), None);
        assert_eq!(store.count_users(POOL).await.expect("count"), 0);
        Ok(())
    }

    /// `apply` returns the accounts an event names and writes no user rows
    /// itself: refreshing is the tick's job.
    #[sqlx::test(migrations = "./migrations")]
    async fn apply_returns_the_accounts_an_event_names(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);

        let repay = PoolEvent::Repay {
            asset: USDC.to_string(),
            from: USER_ONE.to_string(),
            amount: 500,
            d_tokens: 450,
        };
        assert_eq!(
            tracker.apply(POOL, 10, &repay).await.expect("apply"),
            vec![USER_ONE.to_string()]
        );

        // A full fill names both the liquidated user and the filler, and
        // does not require reading chain (it only deletes).
        let fill = PoolEvent::FillAuction {
            auction_type: AuctionType::UserLiquidation,
            user: USER_ONE.to_string(),
            filler: USER_TWO.to_string(),
            fill_percent: 100,
            filled: AuctionData::default(),
        };
        assert_eq!(
            tracker.apply(POOL, 11, &fill).await.expect("apply"),
            vec![USER_ONE.to_string(), USER_TWO.to_string()]
        );

        // A pool-wide event names no account at all.
        let set_reserve = PoolEvent::SetReserve {
            asset: USDC.to_string(),
            index: 0,
        };
        assert!(tracker
            .apply(POOL, 12, &set_reserve)
            .await
            .expect("apply")
            .is_empty());

        // Neither event wrote a user row: refreshing is the tick's job.
        assert_eq!(store.count_users(POOL).await.expect("count"), 0);
        Ok(())
    }

    /// A new auction opens a row at the auction's own block, a partial fill
    /// re-reads the remainder from chain, a full fill deletes, and a delete
    /// event deletes.
    #[sqlx::test(migrations = "./migrations")]
    async fn auction_events_open_reduce_and_close_the_row(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let kind = AuctionType::UserLiquidation;

        let mut bid = BTreeMap::new();
        bid.insert(USDC.to_string(), 1_000_i128);
        let mut lot = BTreeMap::new();
        lot.insert(USDC.to_string(), 2_000_i128);
        let new_auction = PoolEvent::NewAuction {
            auction_type: kind,
            user: USER_ONE.to_string(),
            percent: 40,
            auction: AuctionData {
                bid: bid.clone(),
                lot: lot.clone(),
                block: 64_271_300,
            },
        };
        let accounts = tracker
            .apply(POOL, 64_271_301, &new_auction)
            .await
            .expect("apply new auction");
        assert_eq!(accounts, vec![USER_ONE.to_string()]);
        let opened = store
            .auction(POOL, USER_ONE, kind)
            .await
            .expect("read")
            .expect("a row");
        assert_eq!(opened.start_ledger, 64_271_300);
        assert_eq!(opened.percent, None, "no fill has been planned for it yet");
        assert_eq!(opened.bid, bid);
        assert_eq!(opened.lot, lot);
        assert_eq!(opened.updated_ledger, 64_271_301);

        // A partial fill re-reads the remainder from chain rather than
        // subtracting the filled side, and the remainder carries no fill
        // plan of its own either.
        let key = keys::auction(POOL, USER_ONE, kind).expect("key");
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 64_271_320, "entries": [
                entry(&key, &auction_entry_xdr(400, 800, 64_271_300)),
            ]}),
        );
        let partial_fill = PoolEvent::FillAuction {
            auction_type: kind,
            user: USER_ONE.to_string(),
            filler: USER_TWO.to_string(),
            fill_percent: 60,
            filled: AuctionData {
                bid: bid.clone(),
                lot: lot.clone(),
                block: 64_271_300,
            },
        };
        let accounts = tracker
            .apply(POOL, 64_271_320, &partial_fill)
            .await
            .expect("apply partial fill");
        assert_eq!(accounts, vec![USER_ONE.to_string(), USER_TWO.to_string()]);
        let reduced = store
            .auction(POOL, USER_ONE, kind)
            .await
            .expect("read")
            .expect("still a row");
        assert_eq!(reduced.bid[USDC], 400);
        assert_eq!(reduced.lot[USDC], 800);
        assert_eq!(
            reduced.percent, None,
            "the remainder starts with no fill planned either"
        );
        assert_eq!(reduced.start_ledger, 64_271_300, "the chain's own block");
        assert_eq!(
            reduced.updated_ledger, 64_271_320,
            "the ledger it was read at"
        );

        // A full fill deletes the row.
        let full_fill = PoolEvent::FillAuction {
            auction_type: kind,
            user: USER_ONE.to_string(),
            filler: USER_TWO.to_string(),
            fill_percent: 100,
            filled: AuctionData::default(),
        };
        tracker
            .apply(POOL, 64_271_340, &full_fill)
            .await
            .expect("apply full fill");
        assert_eq!(
            store.auction(POOL, USER_ONE, kind).await.expect("read"),
            None
        );

        // Re-open, then a delete event removes it too.
        tracker
            .apply(POOL, 64_271_350, &new_auction)
            .await
            .expect("reopen");
        assert!(store
            .auction(POOL, USER_ONE, kind)
            .await
            .expect("read")
            .is_some());
        let delete_event = PoolEvent::DeleteAuction {
            auction_type: kind,
            user: USER_ONE.to_string(),
        };
        let accounts = tracker
            .apply(POOL, 64_271_360, &delete_event)
            .await
            .expect("apply delete");
        assert_eq!(accounts, vec![USER_ONE.to_string()]);
        assert_eq!(
            store.auction(POOL, USER_ONE, kind).await.expect("read"),
            None
        );
        Ok(())
    }

    /// Applying the same events twice ends in the same state.
    #[sqlx::test(migrations = "./migrations")]
    async fn applying_an_event_twice_is_idempotent(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let kind = AuctionType::UserLiquidation;

        let mut bid = BTreeMap::new();
        bid.insert(USDC.to_string(), 1_000_i128);
        let mut lot = BTreeMap::new();
        lot.insert(USDC.to_string(), 2_000_i128);
        let new_auction = PoolEvent::NewAuction {
            auction_type: kind,
            user: USER_ONE.to_string(),
            percent: 75,
            auction: AuctionData {
                bid,
                lot,
                block: 64_271_300,
            },
        };
        // A crash between sending and storing the cursor replays a ledger,
        // so applying the same event a second time must land on the same
        // row rather than drift it.
        tracker
            .apply(POOL, 64_271_301, &new_auction)
            .await
            .expect("first apply");
        let first = store
            .auction(POOL, USER_ONE, kind)
            .await
            .expect("read")
            .expect("a row");
        tracker
            .apply(POOL, 64_271_301, &new_auction)
            .await
            .expect("replay");
        let second = store
            .auction(POOL, USER_ONE, kind)
            .await
            .expect("read")
            .expect("still a row");
        assert_eq!(first, second);

        // The same holds for a refresh: two identical snapshots of the
        // fixture land on the same stored row and the same outcome.
        harness::script_snapshot(&rpc, &[USER_ONE]);
        harness::script_snapshot(&rpc, &[USER_ONE]);
        let tick = harness::fixture_tick();
        let outcome_one = tracker
            .refresh(POOL, &[USER_ONE.to_string()], tick)
            .await
            .expect("first refresh");
        let user_after_first = store
            .user(POOL, USER_ONE)
            .await
            .expect("read")
            .expect("a row");
        let outcome_two = tracker
            .refresh(POOL, &[USER_ONE.to_string()], tick)
            .await
            .expect("replay refresh");
        let user_after_second = store
            .user(POOL, USER_ONE)
            .await
            .expect("read")
            .expect("still a row");
        assert_eq!(outcome_one, outcome_two);
        assert_eq!(user_after_first, user_after_second);
        Ok(())
    }

    /// The refresh pass takes the oldest rows first and no more than the
    /// batch size.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_refresh_pass_takes_the_oldest_rows_up_to_the_batch(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        store
            .upsert_user(&stale_user(USER_ONE, 10))
            .await
            .expect("seed oldest");
        store
            .upsert_user(&stale_user(REPAID, 20))
            .await
            .expect("seed middle");
        store
            .upsert_user(&stale_user(NEVER_TRACKED, 30))
            .await
            .expect("seed newest");

        let rpc = ScriptedRpc::start().await;
        // Only the two oldest rows are ever read from chain.
        harness::script_snapshot(&rpc, &[USER_ONE, REPAID]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let tick = harness::fixture_tick();

        let outcome = tracker
            .refresh_stale(POOL, tick, 100, 2)
            .await
            .expect("refresh_stale");
        assert_eq!(outcome.tracked + outcome.removed, 2);

        // The batch cap held: the third, newest row was never touched.
        let untouched = store
            .user(POOL, NEVER_TRACKED)
            .await
            .expect("read")
            .expect("still there, unrefreshed");
        assert_eq!(untouched.updated_ledger, 30);

        let calls = rpc.calls("getLedgerEntries");
        let batched = calls.last().expect("a batched entries call");
        let keys = batched["keys"].as_array().expect("keys array");
        let requested = |account: &str| {
            keys.contains(&json!(
                to_base64(&keys::positions(POOL, account).unwrap()).unwrap()
            ))
        };
        assert!(
            requested(USER_ONE) && requested(REPAID),
            "the two oldest rows were read"
        );
        assert!(
            !requested(NEVER_TRACKED),
            "the newest row was excluded by the batch"
        );
        Ok(())
    }

    /// Two pages, then a null cursor: every accountId, in order, once.
    #[tokio::test]
    async fn the_analytics_source_follows_the_cursor_to_the_end() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/analytics/state/positions"))
            .and(query_param("poolId", POOL))
            .and(query_param_is_missing("cursor"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "positions": [
                    {"accountId": USER_ONE, "healthFactor": 0.4},
                    {"accountId": USER_TWO, "healthFactor": 0.5},
                ],
                "nextCursor": "page-2",
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/analytics/state/positions"))
            .and(query_param("poolId", POOL))
            .and(query_param("cursor", "page-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "positions": [{"accountId": USDC, "healthFactor": 0.9}],
                "nextCursor": null,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let seed = AnalyticsSeed::new(&server.uri(), 100_000_000).expect("client");
        let accounts = seed.accounts(POOL).await.expect("accounts");
        assert_eq!(
            accounts,
            vec![USER_ONE.to_string(), USER_TWO.to_string(), USDC.to_string()],
            "every accountId, in the order the pages gave them, exactly once"
        );
    }

    /// A null `nextCursor` and an absent one both end the walk, and an
    /// unknown pool's empty `positions` is not an error.
    #[tokio::test]
    async fn an_unknown_pool_seeds_nothing_without_failing() {
        const UNKNOWN_POOL: &str = "unknown-pool";
        let server = MockServer::start().await;
        // A null `nextCursor` ends the walk after one page even though the
        // page carries data.
        Mock::given(method("GET"))
            .and(path("/v1/analytics/state/positions"))
            .and(query_param("poolId", POOL))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "positions": [{"accountId": USER_ONE, "healthFactor": 0.4}],
                "nextCursor": null,
            })))
            .expect(1)
            .mount(&server)
            .await;
        // An unknown pool's real response: no `nextCursor` field at all,
        // and an empty `positions` array — not an error, just nothing.
        Mock::given(method("GET"))
            .and(path("/v1/analytics/state/positions"))
            .and(query_param("poolId", UNKNOWN_POOL))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "positions": [],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let seed = AnalyticsSeed::new(&server.uri(), 100_000_000).expect("client");
        assert_eq!(
            seed.accounts(POOL)
                .await
                .expect("a null cursor ends the walk"),
            vec![USER_ONE.to_string()]
        );
        assert_eq!(
            seed.accounts(UNKNOWN_POOL)
                .await
                .expect("an absent cursor and empty positions are not an error"),
            Vec::<String>::new()
        );
    }

    /// A non-200, and a 200 whose body is not the documented shape, are
    /// errors that name what happened rather than seeding silently.
    #[tokio::test]
    async fn a_bad_status_or_shape_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/analytics/state/positions"))
            .and(query_param("poolId", "bad-status"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/analytics/state/positions"))
            .and(query_param("poolId", "bad-shape"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"positions": "not-an-array"}"#),
            )
            .mount(&server)
            .await;

        let seed = AnalyticsSeed::new(&server.uri(), 100_000_000).expect("client");
        assert!(
            matches!(
                seed.accounts("bad-status").await.unwrap_err(),
                SeedError::Status { status: 503 }
            ),
            "a non-200 status is a Status error"
        );
        assert!(
            matches!(
                seed.accounts("bad-shape").await.unwrap_err(),
                SeedError::Shape(_)
            ),
            "a 200 with an undocumented body shape is a Shape error"
        );
    }

    /// The body of an unparseable response reaches a log line through
    /// `SeedError::Shape`, so it is truncated: a third party's response is
    /// bounded by nothing, and a diagnostic needs the body's shape and its
    /// size, not all of it.
    #[tokio::test]
    async fn an_unparseable_body_is_truncated_in_the_error() {
        let server = MockServer::start().await;
        let huge = format!("{{\"positions\": \"{}\"}}", "x".repeat(50_000));
        Mock::given(method("GET"))
            .and(path("/v1/analytics/state/positions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(huge.clone()))
            .mount(&server)
            .await;

        let seed = AnalyticsSeed::new(&server.uri(), 100_000_000).expect("client");
        let SeedError::Shape(message) = seed.accounts("pool").await.unwrap_err() else {
            panic!("an unparseable body is a Shape error");
        };
        assert!(
            message.len() < 400,
            "the whole body must not reach the message: {} bytes",
            message.len()
        );
        assert!(
            message.ends_with(" bytes total)"),
            "the message names how much it left out: {message}"
        );
        assert!(
            !message.contains(&"x".repeat(300)),
            "no run of the body survives past the snippet"
        );
    }

    /// The query carries the pool, the limit and the health factor rendered
    /// from 7-decimal fixed point, and the cursor only on later pages.
    #[tokio::test]
    async fn the_query_carries_the_pool_and_the_rendered_health_factor() {
        let server = MockServer::start().await;
        // The first page, matched only while unconsumed: no cursor.
        Mock::given(method("GET"))
            .and(path("/v1/analytics/state/positions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "positions": [],
                "nextCursor": "next-page",
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Falls through to this one on the second request.
        Mock::given(method("GET"))
            .and(path("/v1/analytics/state/positions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "positions": [],
                "nextCursor": null,
            })))
            .mount(&server)
            .await;

        // 1.5 in 7-decimal fixed point, the same mapping config.rs's
        // Decimal7 test pins the other direction.
        let seed = AnalyticsSeed::new(&server.uri(), 15_000_000).expect("client");
        seed.accounts(POOL).await.expect("accounts");

        let requests = server.received_requests().await.expect("recorded");
        assert_eq!(requests.len(), 2, "one request per page");
        let query = |index: usize| -> std::collections::HashMap<String, String> {
            requests[index].url.query_pairs().into_owned().collect()
        };
        let first = query(0);
        assert_eq!(first.get("poolId").map(String::as_str), Some(POOL));
        assert_eq!(first.get("limit").map(String::as_str), Some("500"));
        assert_eq!(
            first.get("healthFactorMax").map(String::as_str),
            Some("1.5")
        );
        assert!(
            !first.contains_key("cursor"),
            "the first page carries no cursor"
        );
        let second = query(1);
        assert_eq!(
            second.get("cursor").map(String::as_str),
            Some("next-page"),
            "the second page carries the cursor the first page returned"
        );
    }

    #[test]
    fn the_seed_file_reads_a_pools_account_list() {
        let path = write_temp_seed_file(&format!("[accounts]\n\"{POOL}\" = [\"{USER_ONE}\"]\n"));
        let seed = FileSeed::load(&path).expect("loads");
        assert_eq!(seed.accounts(POOL), vec![USER_ONE.to_string()]);
        assert!(
            seed.accounts("some-other-pool").is_empty(),
            "a pool the file does not mention has no accounts"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_seed_file_that_is_not_the_documented_shape_is_an_error() {
        for contents in [
            "not valid toml {{{",
            "[wrong-table]\nkey = 1\n",
            "[accounts]\n\"C...\" = \"not-a-list\"\n",
        ] {
            let path = write_temp_seed_file(contents);
            assert!(
                matches!(FileSeed::load(&path).unwrap_err(), SeedError::File(_)),
                "{contents:?} should be a File error"
            );
            let _ = std::fs::remove_file(&path);
        }
        // A path that does not exist is also a File error, not a panic.
        let missing = std::env::temp_dir().join("blend-liquidator-seed-test-missing.toml");
        assert!(matches!(
            FileSeed::load(&missing).unwrap_err(),
            SeedError::File(_)
        ));
    }

    /// A shutdown arriving mid-seed is not an error and not a completed
    /// seed either: the accounts already valued stay written, and the
    /// outcome says it stopped early so a caller cannot record the seed's
    /// position as though it had finished.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_shutdown_mid_seed_reports_that_it_stopped_early(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        // No snapshot is scripted: the loop must break before it reads.
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);
        let file_path =
            write_temp_seed_file(&format!("[accounts]\n\"{POOL}\" = [\"{USER_ONE}\"]\n"));
        let file = SeedSource::File(FileSeed::load(&file_path).expect("loads"));
        let (_flag, shutdown) = watch::channel(true);

        let outcome = tracker
            .seed(POOL, &[file], harness::fixture_tick(), 20, &shutdown)
            .await
            .expect("a shutdown is not a seed failure");

        assert!(outcome.stopped_early, "the seed stopped before it finished");
        assert_eq!(outcome.failed_sources, 0, "the source itself answered");
        assert!(
            !outcome.is_complete(),
            "a seed that stopped early is not complete, whatever its sources did"
        );
        assert_eq!(outcome.refresh.tracked, 0);
        Ok(())
    }

    /// Every source's accounts are refreshed once, deduplicated, and a
    /// failing source is skipped rather than failing the seed.
    #[sqlx::test(migrations = "./migrations")]
    async fn seeding_refreshes_every_account_once_and_survives_a_failing_source(
        db: sqlx::PgPool,
    ) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        // Scripted exactly once: if the two sources' overlapping accounts
        // were not deduplicated first, a second refresh would need a
        // second scripted snapshot that is not here, and fail loudly.
        harness::script_snapshot(&rpc, &[USER_ONE, USER_TWO]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let tracker = Tracker::new(&client, &store);

        let analytics_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "positions": [
                    {"accountId": USER_ONE, "healthFactor": 0.4},
                    {"accountId": USER_TWO, "healthFactor": 0.5},
                ],
                "nextCursor": null,
            })))
            .mount(&analytics_server)
            .await;
        let analytics = SeedSource::Analytics(
            AnalyticsSeed::new(&analytics_server.uri(), 100_000_000).expect("client"),
        );

        let file_path =
            write_temp_seed_file(&format!("[accounts]\n\"{POOL}\" = [\"{USER_TWO}\"]\n"));
        let file = SeedSource::File(FileSeed::load(&file_path).expect("loads"));

        let failing_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&failing_server)
            .await;
        let failing = SeedSource::Analytics(
            AnalyticsSeed::new(&failing_server.uri(), 100_000_000).expect("client"),
        );

        let tick = harness::fixture_tick();
        let (_flag, shutdown) = tokio::sync::watch::channel(false);
        let outcome = tracker
            .seed(POOL, &[analytics, file, failing], tick, 10, &shutdown)
            .await
            .expect("seed");

        assert_eq!(
            outcome.refresh,
            RefreshOutcome {
                tracked: 2,
                removed: 0
            },
            "USER_ONE and USER_TWO are each refreshed exactly once, deduplicated \
             across the analytics source and the file source"
        );
        assert_eq!(
            outcome.failed_sources, 1,
            "the source that could not answer is reported, so the next full scan retries the seed"
        );
        assert_eq!(store.count_users(POOL).await.expect("count"), 2);
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            2,
            "one batched snapshot (two getLedgerEntries calls), not one per source"
        );
        let _ = std::fs::remove_file(&file_path);
        Ok(())
    }
}
