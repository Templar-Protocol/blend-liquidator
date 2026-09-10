//! The auctioneer: which borrowers are liquidatable, and what auction to
//! create for them.
//!
//! The arithmetic is [`crate::math::liquidation`]'s; this module is the I/O
//! around it. It reads one snapshot per batch, values each borrower at the
//! tick's close time — the same instant the tracker valued them at, so the
//! decision and the stored health factor cannot disagree — and answers with
//! a [`Decision`] per user.
//!
//! Simulation and submission are not here: a [`Decision`] is an answer, not
//! an action, which is what lets this module's tests drive real logic
//! through a scripted RPC with no signer at all.

use std::collections::{BTreeMap, BTreeSet};

use crate::chain::pool::{PoolReader, PoolSnapshot};
use crate::chain::rpc::RpcClient;
use crate::chain::xdr::AuctionType;
use crate::chain::ChainError;
use crate::ledger::LedgerTick;
use crate::math::liquidation::{plan_liquidation, position_values, LiquidationPlan};
use crate::math::{mul_floor, MathError, Reserve, SCALAR_7};
use crate::store::{Store, StoreError, TrackedUser};

/// Why a borrower was not acted on. Every skip is a decision, and a decision
/// worth naming: an operator asking "why did nothing happen" is asking about
/// exactly this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Above `LIQ_HF_THRESHOLD`.
    Healthy,
    /// An auction for this user already exists.
    AuctionOpen,
    /// Liquidatable, but no selection of positions closes the excess.
    NoPlan,
    /// The filler's or the auctioneer's own account.
    OwnAccount,
    /// No liabilities: nothing to liquidate.
    NoLiabilities,
}

/// What to do about one borrower.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Create this auction.
    Liquidate(LiquidationPlan),
    /// Move this user's debt to the backstop.
    BadDebt,
    /// Nothing, for this reason.
    Skip(SkipReason),
}

/// The batch-level policy [`Auctioneer::decide`] reads: the thresholds a
/// health factor is judged against, and the accounts it must never name.
#[derive(Debug, Clone)]
pub struct AuctioneerConfig {
    /// The health factor at or below which a borrower is liquidatable, 7
    /// decimals (`LIQ_HF_THRESHOLD`).
    pub liquidation_health_factor: i128,
    /// The health factor a liquidation aims to leave the borrower at, 7
    /// decimals (`TARGET_HF`).
    pub target_health_factor: i128,
    /// How many times a rejected percent is adjusted against the contract's
    /// own answer before the borrower is left until the next recheck.
    /// Carried here for Task 6's simulation retry; `decide` does not read
    /// it, because a plan's own percent is never resimulated before it is
    /// returned.
    pub plan_iterations: u32,
    /// The bot's own accounts, filler included. The contract refuses to let
    /// the bot liquidate itself, but in dry-run there is no contract to
    /// refuse, and a bot that would have tried is one that will try when
    /// armed.
    pub own_addresses: BTreeSet<String>,
}

/// A failure deciding who is liquidatable.
#[derive(Debug, thiserror::Error)]
pub enum AuctioneerError {
    /// Reading the chain failed.
    #[error("chain: {0}")]
    Chain(#[from] ChainError),
    /// Reading the store failed.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// Checked arithmetic on chain values failed.
    #[error("math: {0}")]
    Math(#[from] MathError),
}

/// Decides who is liquidatable, against one store and one chain client.
#[derive(Debug, Clone)]
pub struct Auctioneer<'a> {
    rpc: &'a RpcClient,
    store: &'a Store,
    config: AuctioneerConfig,
}

/// Accrues a clone of the snapshot's reserves to `close_time`, once for the
/// whole batch.
///
/// `PoolSnapshot::reserves` is stored, not accrued. [`position_values`]
/// needs the accrued numbers to price each position individually, the same
/// instant [`PoolSnapshot::position_data`] accrues its own clone to for the
/// health-factor gate — accruing a second time here, to the same
/// `close_time`, is what keeps the two agreeing. Doing that accrual inside
/// the per-user loop would accrue the whole reserve set again for every
/// borrower the batch finds liquidatable; a full scan of a thousand
/// borrowers is exactly the cadence a repeated accrual like that turns into
/// a stall, so this runs once, before the loop, and every user's plan
/// borrows from the one result.
fn accrue_reserves(
    snapshot: &PoolSnapshot,
    close_time: u64,
) -> Result<BTreeMap<u32, Reserve>, MathError> {
    let mut reserves = snapshot.reserves.clone();
    for reserve in reserves.values_mut() {
        reserve.accrue(snapshot.instance.config.bstop_rate, close_time)?;
    }
    Ok(reserves)
}

impl<'a> Auctioneer<'a> {
    /// An auctioneer reading through `rpc`, checking open auctions against
    /// `store`, and judging against `config`.
    #[must_use]
    pub fn new(rpc: &'a RpcClient, store: &'a Store, config: AuctioneerConfig) -> Self {
        Self { rpc, store, config }
    }

    /// Decides every one of `users`, against one snapshot read for the
    /// whole batch: the decision for a thousand borrowers is one
    /// `getLedgerEntries`, not a thousand. Returns one `(account,
    /// Decision)` per user, in the order `users` gave them.
    pub async fn decide(
        &self,
        pool: &str,
        users: &[TrackedUser],
        tick: LedgerTick,
    ) -> Result<Vec<(String, Decision)>, AuctioneerError> {
        if users.is_empty() {
            return Ok(Vec::new());
        }
        let accounts: Vec<&str> = users.iter().map(|user| user.account.as_str()).collect();
        let snapshot = PoolReader::new(self.rpc, pool).snapshot(&accounts).await?;
        let reserves = accrue_reserves(&snapshot, tick.close_time)?;

        let mut decisions = Vec::with_capacity(users.len());
        for user in users {
            let decision = self
                .decide_one(pool, &user.account, &snapshot, &reserves, tick)
                .await?;
            decisions.push((user.account.clone(), decision));
        }
        Ok(decisions)
    }

    /// One borrower's decision against an already-read `snapshot` and its
    /// already-accrued `reserves`. The six steps are the module's whole
    /// policy; see the module doc for why each exists.
    async fn decide_one(
        &self,
        pool: &str,
        account: &str,
        snapshot: &PoolSnapshot,
        reserves: &BTreeMap<u32, Reserve>,
        tick: LedgerTick,
    ) -> Result<Decision, AuctioneerError> {
        if self.config.own_addresses.contains(account) {
            return Ok(Decision::Skip(SkipReason::OwnAccount));
        }
        // The store, not the chain: the tracker maintains this table from
        // the pool's own events, and a chain read per user is exactly the
        // round trip batching this decision exists to avoid.
        if self
            .store
            .auction(pool, account, AuctionType::UserLiquidation)
            .await?
            .is_some()
        {
            return Ok(Decision::Skip(SkipReason::AuctionOpen));
        }
        let Some(data) = snapshot.position_data(account, tick.close_time)? else {
            return Ok(Decision::Skip(SkipReason::NoLiabilities));
        };
        if data.liability_base > 0 && data.collateral_base == 0 {
            return Ok(Decision::BadDebt);
        }
        // `health_factor` is `None` only when there are no liabilities. A
        // positions entry can be non-empty on collateral or plain supply
        // alone, which step 3 above does not catch, so this is a second,
        // real path to the same answer rather than an unreachable guard.
        let Some(health) = data.health_factor()? else {
            return Ok(Decision::Skip(SkipReason::NoLiabilities));
        };
        // Normalised to 7 decimals exactly as the tracker normalises the
        // value it stores, so the two are comparable against one threshold
        // regardless of the oracle's own decimals.
        let normalized = mul_floor(health, SCALAR_7, data.scalar)?;
        if normalized > self.config.liquidation_health_factor {
            return Ok(Decision::Skip(SkipReason::Healthy));
        }
        // `position_data` already proved this account holds a positions
        // entry; this is the same lookup, kept fallible rather than
        // unwrapped.
        let Some(positions) = snapshot.positions.get(account) else {
            return Ok(Decision::Skip(SkipReason::NoLiabilities));
        };
        let (collateral, liabilities) = position_values(reserves, &snapshot.prices, positions)?;
        let plan = plan_liquidation(
            &data,
            &collateral,
            &liabilities,
            self.config.target_health_factor,
            snapshot.instance.config.max_positions,
        )?;
        Ok(match plan {
            Some(plan) => Decision::Liquidate(plan),
            None => Decision::Skip(SkipReason::NoPlan),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::{json, Value};
    use stellar_xdr::{ContractDataDurability, ContractDataEntry, ExtensionPoint, LedgerEntryData};

    use super::*;
    use crate::chain::rpc::RpcClient;
    use crate::chain::script::ScriptedRpc;
    use crate::chain::xdr::encode::{
        address, i128_val, map, sc_address, symbol, to_base64, vec as sc_vec,
    };
    use crate::chain::xdr::keys;
    use crate::fixture::{mainnet_fixed_v2, text};
    use crate::harness::{self, GOLDEN_HEALTH, POOL, USER_ONE, USER_TWO};
    use crate::store::TrackedAuction;

    /// A bid or lot asset for the open-auction test: any reserve address
    /// works, since that test never reaches the math that would care which
    /// one.
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    /// A valid, distinct account strkey from `byte` alone — no real key
    /// behind it, and none needed: every test here only ever reads chain
    /// state through the scripted RPC, never signs anything.
    fn synthetic_account(byte: u8) -> String {
        stellar_strkey::ed25519::PublicKey([byte; 32]).to_string()
    }

    /// The brief's own default thresholds
    /// (`LIQ_HF_THRESHOLD=0.998`, `TARGET_HF=1.06`, `PLAN_ITERATIONS=5`),
    /// with `own_addresses` supplied per test.
    fn config(own_addresses: BTreeSet<String>) -> AuctioneerConfig {
        AuctioneerConfig {
            liquidation_health_factor: 9_980_000,
            target_health_factor: 10_600_000,
            plan_iterations: 5,
            own_addresses,
        }
    }

    /// A `TrackedUser` naming only `account`: `decide` re-derives
    /// everything else from the fresh snapshot, so no other field is read.
    fn tracked_user(account: &str) -> TrackedUser {
        TrackedUser {
            pool: POOL.to_string(),
            account: account.to_string(),
            health_factor: 0,
            collateral: BTreeMap::new(),
            liabilities: BTreeMap::new(),
            updated_ledger: 0,
            recheck_ledger: None,
        }
    }

    fn entry(key: &stellar_xdr::LedgerKey, xdr: &str) -> Value {
        json!({
            "key": to_base64(key).expect("key"),
            "xdr": xdr,
            "lastModifiedLedgerSeq": 1,
            "liveUntilLedgerSeq": 99_999_999_u32,
        })
    }

    fn simulation(return_xdr: &str, ledger: u32) -> Value {
        json!({
            "transactionData": crate::chain::script::transaction_data_b64(1),
            "events": [],
            "minResourceFee": "1",
            "results": [{"auth": [], "xdr": return_xdr}],
            "latestLedger": ledger,
        })
    }

    /// A `Positions` ledger entry built by hand: `collateral` and
    /// `liabilities` are `(reserve index, token amount)` pairs, `supply`
    /// always empty — nothing here reads plain supply.
    fn positions_entry_xdr(
        account: &str,
        collateral: &[(u32, i128)],
        liabilities: &[(u32, i128)],
    ) -> String {
        let side = |amounts: &[(u32, i128)]| {
            map(amounts
                .iter()
                .map(|(index, amount)| (stellar_xdr::ScVal::U32(*index), i128_val(*amount)))
                .collect())
            .expect("side map")
        };
        let value = map(vec![
            (symbol("collateral").expect("symbol"), side(collateral)),
            (symbol("liabilities").expect("symbol"), side(liabilities)),
            (symbol("supply").expect("symbol"), side(&[])),
        ])
        .expect("positions map");
        let entry = LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: sc_address(POOL).expect("pool address"),
            key: sc_vec(vec![
                symbol("Positions").expect("symbol"),
                address(account).expect("address"),
            ])
            .expect("key"),
            durability: ContractDataDurability::Persistent,
            val: value,
        });
        to_base64(&entry).expect("encodes")
    }

    /// Scripts one snapshot for one account exactly as
    /// `harness::script_snapshot` does — same shape read, same reserve
    /// configs and data, same oracle prices, all straight from the fixture
    /// — except that the account gets a hand-built positions entry rather
    /// than whatever (if anything) the fixture itself holds for it.
    ///
    /// This is what stands in for `harness::script_snapshot` in the
    /// liquidatable and bad-debt tests below. The fixture's own two
    /// borrowers each hold their collateral and liability on the very same
    /// reserve (see `chain::xdr::decode`'s golden-health test), so their
    /// health factor is invariant to any single reserve's price — neither
    /// can be pushed underwater by moving a price, only by a positions
    /// entry that spans two reserves, which the fixture does not have. The
    /// prices and reserve configuration stay the fixture's real, attested
    /// ones throughout; only the position is fabricated.
    fn script_snapshot_with_positions(
        rpc: &ScriptedRpc,
        account: &str,
        collateral: &[(u32, i128)],
        liabilities: &[(u32, i128)],
    ) {
        let fixture = mainnet_fixed_v2();
        let ledger = fixture["ledger"].as_u64().expect("ledger");
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(POOL).expect("key"), text(&fixture, &["instance_entry_xdr"])),
                entry(&keys::reserve_list(POOL).expect("key"), text(&fixture, &["res_list_entry_xdr"])),
            ]}),
        );
        let mut entries = Vec::new();
        for reserve in fixture["reserves"].as_array().expect("reserves") {
            let asset = reserve["asset"].as_str().expect("asset");
            entries.push(entry(
                &keys::reserve_config(POOL, asset).expect("key"),
                reserve["config_entry_xdr"].as_str().expect("config"),
            ));
            entries.push(entry(
                &keys::reserve_data(POOL, asset).expect("key"),
                reserve["data_entry_xdr"].as_str().expect("data"),
            ));
        }
        entries.push(entry(
            &keys::positions(POOL, account).expect("key"),
            &positions_entry_xdr(account, collateral, liabilities),
        ));
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": entries}),
        );
        let ledger = u32::try_from(ledger).expect("ledger fits");
        rpc.expect(
            "simulateTransaction",
            simulation(text(&fixture, &["oracle_decimals_return_xdr"]), ledger),
        );
        for reserve in fixture["reserves"].as_array().expect("reserves") {
            rpc.expect(
                "simulateTransaction",
                simulation(
                    reserve["lastprice_return_xdr"].as_str().expect("price"),
                    ledger,
                ),
            );
        }
    }

    /// A borrower above the threshold is left alone. The threshold sits
    /// below the contract's own strict test, so "not liquidatable yet" is
    /// the common answer and must be cheap and silent.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_healthy_borrower_is_skipped(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[USER_TWO]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()));
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(USER_TWO)], tick)
            .await
            .expect("decide");
        assert_eq!(
            decisions,
            vec![(USER_TWO.to_string(), Decision::Skip(SkipReason::Healthy))]
        );
        // Not a vacuous pass: the golden health factor really is above the
        // threshold this config used.
        let golden = GOLDEN_HEALTH
            .iter()
            .find(|(account, _)| *account == USER_TWO)
            .expect("golden entry")
            .1;
        assert!(golden > 9_980_000);
        Ok(())
    }

    /// A borrower under the threshold with collateral and liabilities gets
    /// a plan naming both sides and a percent in 1..=100.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_underwater_borrower_gets_a_plan(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_account(1);
        // 10 units of reserve 0 as collateral against 5,000 units of
        // reserve 2 as a liability: at the fixture's real, unmodified
        // prices this is wildly underwater (health factor a small fraction
        // of a percent), so the plan is not a borderline case.
        script_snapshot_with_positions(&rpc, &account, &[(0, 100_000_000)], &[(2, 50_000_000_000)]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()));
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(&account)], tick)
            .await
            .expect("decide");
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].0, account);
        let Decision::Liquidate(plan) = &decisions[0].1 else {
            panic!("expected a plan, got {:?}", decisions[0].1);
        };
        // A single collateral position against a single liability position
        // leaves the selection nothing to add on either side, so this
        // lands on `plan_liquidation`'s exhaustion fallback deterministically:
        // percent 100, still within the 1..=100 the brief asks for.
        assert_eq!(plan.percent.get(), 100);
        let fixture = mainnet_fixed_v2();
        let asset_of = |index: usize| {
            fixture["reserves"][index]["asset"]
                .as_str()
                .expect("asset")
                .to_string()
        };
        assert_eq!(
            plan.lot,
            vec![asset_of(0)],
            "the lot is the collateral paid out"
        );
        assert_eq!(
            plan.bid,
            vec![asset_of(2)],
            "the bid is the liability taken over"
        );
        Ok(())
    }

    /// Liabilities with no collateral is bad debt, which is a different
    /// submission entirely: `bad_debt(user)` takes no percent and no asset
    /// lists, because the contract sizes it.
    #[sqlx::test(migrations = "./migrations")]
    async fn liabilities_without_collateral_are_bad_debt(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let account = synthetic_account(2);
        script_snapshot_with_positions(&rpc, &account, &[], &[(1, 10_000_000_000)]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()));
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(&account)], tick)
            .await
            .expect("decide");
        assert_eq!(decisions, vec![(account, Decision::BadDebt)]);
        Ok(())
    }

    /// A borrower with an auction already open is skipped: the contract
    /// would answer `AuctionInProgress`, and a simulation spent finding
    /// that out is a round trip for nothing.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_user_with_an_open_auction_is_skipped(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        store
            .upsert_auction(&TrackedAuction {
                pool: POOL.to_string(),
                account: USER_ONE.to_string(),
                auction_type: AuctionType::UserLiquidation,
                start_ledger: 1,
                fill_ledger: None,
                percent: None,
                bid: BTreeMap::from([(USDC.to_string(), 1_000)]),
                lot: BTreeMap::from([(USDC.to_string(), 2_000)]),
                updated_ledger: 1,
            })
            .await
            .expect("seed an open auction");
        // The auction is judged from the store, before the position is
        // ever read from chain, but the snapshot is still one per batch
        // regardless of who is skipped, so it is still scripted here.
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()));
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(USER_ONE)], tick)
            .await
            .expect("decide");
        assert_eq!(
            decisions,
            vec![(
                USER_ONE.to_string(),
                Decision::Skip(SkipReason::AuctionOpen)
            )]
        );
        Ok(())
    }

    /// The bot never liquidates itself. The contract refuses it, but in
    /// dry-run there is no contract to refuse, and a bot that would have
    /// tried is a bot that will try when armed.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_bots_own_accounts_are_never_liquidated(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        harness::script_snapshot(&rpc, &[USER_ONE]);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let own_addresses = BTreeSet::from([USER_ONE.to_string()]);
        let auctioneer = Auctioneer::new(&client, &store, config(own_addresses));
        let tick = harness::fixture_tick();

        let decisions = auctioneer
            .decide(POOL, &[tracked_user(USER_ONE)], tick)
            .await
            .expect("decide");
        assert_eq!(
            decisions,
            vec![(USER_ONE.to_string(), Decision::Skip(SkipReason::OwnAccount))],
            "USER_ONE is healthy anyway, so a wrong check here would still \
             pass by accident unless it runs before the health factor is read"
        );
        Ok(())
    }

    /// One snapshot serves every user in the batch: the decision for
    /// twenty borrowers is one `getLedgerEntries`, not twenty.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_batch_of_users_takes_one_snapshot(db: sqlx::PgPool) -> sqlx::Result<()> {
        let store = Store::from_pool(db);
        let rpc = ScriptedRpc::start().await;
        let extra: Vec<String> = (0..18u8).map(synthetic_account).collect();
        let mut accounts: Vec<&str> = vec![USER_ONE, USER_TWO];
        accounts.extend(extra.iter().map(String::as_str));
        assert_eq!(accounts.len(), 20, "twenty users, not two");
        harness::script_snapshot(&rpc, &accounts);
        let client = RpcClient::new(&rpc.url(), None).expect("client");
        let auctioneer = Auctioneer::new(&client, &store, config(BTreeSet::new()));
        let tick = harness::fixture_tick();
        let users: Vec<TrackedUser> = accounts
            .iter()
            .map(|account| tracked_user(account))
            .collect();

        let decisions = auctioneer.decide(POOL, &users, tick).await.expect("decide");
        assert_eq!(decisions.len(), 20);
        assert_eq!(
            decisions[0],
            (USER_ONE.to_string(), Decision::Skip(SkipReason::Healthy))
        );
        assert_eq!(
            decisions[1],
            (USER_TWO.to_string(), Decision::Skip(SkipReason::Healthy))
        );
        for (account, decision) in &decisions[2..] {
            assert_eq!(
                *decision,
                Decision::Skip(SkipReason::NoLiabilities),
                "{account} holds no positions entry in the fixture ledger"
            );
        }

        // The assertion that actually pins it: a snapshot per user would
        // be twenty `getLedgerEntries` calls (or more), not two.
        assert_eq!(
            rpc.calls("getLedgerEntries").len(),
            2,
            "one snapshot (a shape read and a batched full read) for all \
             twenty users, not one per user"
        );
        assert_eq!(rpc.remaining(), 0, "every scripted answer was consumed");
        Ok(())
    }
}
