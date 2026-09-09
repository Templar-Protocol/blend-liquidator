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
    /// deleting the ones that no longer owe anything. Reserves are accrued
    /// to `tick.close_time`, so an account refreshed at ledger *N* is valued
    /// as the contract would value it in ledger *N*.
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
        let mut outcome = RefreshOutcome::default();
        for account in accounts {
            let positions = snapshot.positions.get(account);
            let owes = positions.is_some_and(|positions| !positions.liabilities.is_empty());
            let health = if owes {
                snapshot
                    .position_data(account, tick.close_time)?
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
                            updated_ledger: tick.sequence,
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

    /// Refreshes up to `batch` users whose row predates `older_than`, oldest
    /// first, so a long-idle borrower's accrued interest is never missed.
    pub async fn refresh_stale(
        &self,
        pool: &str,
        tick: LedgerTick,
        older_than: u32,
        batch: u32,
    ) -> Result<RefreshOutcome, TrackerError> {
        let limit = i64::from(batch);
        let stale = self.store.users_stale(pool, older_than, limit).await?;
        let accounts: Vec<String> = stale.into_iter().map(|user| user.account).collect();
        self.refresh(pool, &accounts, tick).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use stellar_xdr::{ContractDataDurability, ContractDataEntry, ExtensionPoint, LedgerEntryData};

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
            assert_eq!(row.updated_ledger, tick.sequence);
        }
        assert_eq!(store.count_users(POOL).await.expect("count"), 2);
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
}
