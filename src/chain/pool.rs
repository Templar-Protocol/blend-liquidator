//! Pool reads and the pool's three operations.
//!
//! A `PoolSnapshot` describes exactly one ledger: the reader takes the
//! instance and reserve list, then every reserve and requested user in one
//! batched read, then the oracle's decimals and prices by simulation, and
//! refuses the result if any of those reported a different `latestLedger`.
//! The snapshot holds reserves as stored; `position_data` accrues a copy to
//! a close time before valuing, the way the contract does inside a call.

use std::collections::BTreeMap;

use stellar_xdr::{Operation, ScVal};

use crate::chain::rpc::{RpcClient, SimulationOutcome};
use crate::chain::xdr::decode::{self, PoolInstance};
use crate::chain::xdr::encode::{
    address, invoke_contract_op, request, simulation_envelope, stellar_asset, vec as sc_vec,
    Request,
};
use crate::chain::xdr::{keys, AuctionType, XdrError};
use crate::chain::ChainError;
use crate::math::{
    calculate_position_data, AuctionData, OraclePrices, PositionData, Positions, Reserve,
};

/// `submit(from, spender, to, requests)`.
pub fn submit_op(
    pool: &str,
    from: &str,
    spender: &str,
    to: &str,
    requests: &[Request],
) -> Result<Operation, XdrError> {
    let requests = requests
        .iter()
        .map(request)
        .collect::<Result<Vec<_>, _>>()?;
    invoke_contract_op(
        pool,
        "submit",
        vec![
            address(from)?,
            address(spender)?,
            address(to)?,
            sc_vec(requests)?,
        ],
    )
}

/// `new_auction(auction_type, user, bid, lot, percent)`.
pub fn new_auction_op(
    pool: &str,
    auction_type: AuctionType,
    user: &str,
    bid: &[&str],
    lot: &[&str],
    percent: u32,
) -> Result<Operation, XdrError> {
    let addresses = |assets: &[&str]| {
        assets
            .iter()
            .map(|asset| address(asset))
            .collect::<Result<Vec<_>, _>>()
            .and_then(sc_vec)
    };
    invoke_contract_op(
        pool,
        "new_auction",
        vec![
            ScVal::U32(auction_type.code()),
            address(user)?,
            addresses(bid)?,
            addresses(lot)?,
            ScVal::U32(percent),
        ],
    )
}

/// `bad_debt(user)`.
pub fn bad_debt_op(pool: &str, user: &str) -> Result<Operation, XdrError> {
    invoke_contract_op(pool, "bad_debt", vec![address(user)?])
}

/// One ledger's view of a pool.
#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    /// The ledger every field describes.
    pub ledger: u32,
    /// The pool contract.
    pub pool: String,
    /// Instance storage: admin, backstop, config.
    pub instance: PoolInstance,
    /// Reserves keyed by `config.index`, the key `Positions` uses, as
    /// stored — not yet accrued. Indexes are unique by construction in the
    /// contract's reserve list; `PoolReader::snapshot` fails with
    /// `ChainError::Shape` rather than silently overwrite one if two ever
    /// collide.
    pub reserves: BTreeMap<u32, Reserve>,
    /// Asset address to reserve index.
    pub asset_index: BTreeMap<String, u32>,
    /// The oracle's prices for every reserve that had one.
    pub prices: OraclePrices,
    /// When the oracle last updated each price, unix seconds, so a caller
    /// can judge staleness against the tick.
    pub price_timestamps: BTreeMap<String, u64>,
    /// Each requested user's positions; empty when the ledger holds none.
    pub positions: BTreeMap<String, Positions>,
}

impl PoolSnapshot {
    /// Values `user`'s positions at `close_time`: accrues a copy of the
    /// reserves to it with the pool's backstop rate, then computes the
    /// effective and raw totals. `None` when the user was not requested or
    /// holds no positions.
    pub fn position_data(
        &self,
        user: &str,
        close_time: u64,
    ) -> Result<Option<PositionData>, ChainError> {
        let Some(positions) = self.positions.get(user) else {
            return Ok(None);
        };
        if positions.is_empty() {
            return Ok(None);
        }
        let mut reserves = self.reserves.clone();
        for reserve in reserves.values_mut() {
            reserve.accrue(self.instance.config.bstop_rate, close_time)?;
        }
        Ok(Some(calculate_position_data(
            &reserves,
            &self.prices,
            positions,
        )?))
    }
}

/// Reads one pool through an `RpcClient`.
#[derive(Debug, Clone, Copy)]
pub struct PoolReader<'a> {
    rpc: &'a RpcClient,
    pool: &'a str,
}

fn same_ledger(expected: u32, actual: u32) -> Result<(), ChainError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ChainError::LedgerMoved {
            first: expected,
            second: actual,
        })
    }
}

impl<'a> PoolReader<'a> {
    /// A reader for `pool`.
    #[must_use]
    pub fn new(rpc: &'a RpcClient, pool: &'a str) -> Self {
        Self { rpc, pool }
    }

    /// Simulates a view call and returns `(latest ledger, return value)`.
    async fn view(
        &self,
        contract: &str,
        function: &str,
        args: Vec<ScVal>,
    ) -> Result<(u32, ScVal), ChainError> {
        let envelope = simulation_envelope(invoke_contract_op(contract, function, args)?)?;
        let simulation = self.rpc.simulate(&envelope).await?;
        match simulation.outcome {
            SimulationOutcome::Success(call) => Ok((simulation.latest_ledger, call.return_value)),
            SimulationOutcome::Failure {
                message,
                contract_error,
            } => Err(ChainError::Simulation {
                message,
                contract_error,
            }),
        }
    }

    /// The instance and reserve list, and the ledger they came from.
    async fn shape(&self) -> Result<(u32, PoolInstance, Vec<String>), ChainError> {
        let instance_key = keys::instance(self.pool)?;
        let list_key = keys::reserve_list(self.pool)?;
        let entries = self
            .rpc
            .ledger_entries(&[instance_key.clone(), list_key.clone()])
            .await?;
        let missing =
            |what: &str| ChainError::Shape(format!("pool {} has no {what} entry", self.pool));
        let instance = decode::pool_instance(
            &entries
                .get(&instance_key)?
                .ok_or_else(|| missing("instance"))?
                .data,
        )?;
        let assets = decode::reserve_list(
            &entries
                .get(&list_key)?
                .ok_or_else(|| missing("reserve list"))?
                .data,
        )?;
        Ok((entries.latest_ledger, instance, assets))
    }

    /// The oracle's decimals and one price per asset, all at `ledger`.
    async fn prices(
        &self,
        oracle: &str,
        assets: &[String],
        ledger: u32,
    ) -> Result<(OraclePrices, BTreeMap<String, u64>), ChainError> {
        let (at, decimals) = self.view(oracle, "decimals", Vec::new()).await?;
        same_ledger(ledger, at)?;
        let decimals = decode::decimals(&decimals)?;
        let mut prices = BTreeMap::new();
        let mut timestamps = BTreeMap::new();
        for asset in assets {
            let (at, value) = self
                .view(oracle, "lastprice", vec![stellar_asset(asset)?])
                .await?;
            same_ledger(ledger, at)?;
            match decode::price_data(&value)? {
                Some(price) => {
                    prices.insert(asset.clone(), price.price);
                    timestamps.insert(asset.clone(), price.timestamp);
                }
                None => {
                    tracing::warn!(pool = self.pool, asset = %asset, "the oracle has no price; the asset is unpriced");
                }
            }
        }
        Ok((OraclePrices::new(decimals, prices)?, timestamps))
    }

    /// One ledger's view of the pool for `users`.
    pub async fn snapshot(&self, users: &[&str]) -> Result<PoolSnapshot, ChainError> {
        let (ledger, instance, assets) = self.shape().await?;
        let mut wanted = Vec::with_capacity(assets.len() * 2 + users.len());
        for asset in &assets {
            wanted.push(keys::reserve_config(self.pool, asset)?);
            wanted.push(keys::reserve_data(self.pool, asset)?);
        }
        for user in users {
            wanted.push(keys::positions(self.pool, user)?);
        }
        let entries = self.rpc.ledger_entries(&wanted).await?;
        same_ledger(ledger, entries.latest_ledger)?;

        let mut reserves = BTreeMap::new();
        let mut asset_index = BTreeMap::new();
        for asset in &assets {
            let missing =
                |what: &str| ChainError::Shape(format!("reserve {asset} has no {what} entry"));
            let config = decode::reserve_config(
                &entries
                    .get(&keys::reserve_config(self.pool, asset)?)?
                    .ok_or_else(|| missing("config"))?
                    .data,
            )?;
            let data = decode::reserve_data(
                &entries
                    .get(&keys::reserve_data(self.pool, asset)?)?
                    .ok_or_else(|| missing("data"))?
                    .data,
            )?;
            let reserve = Reserve::new(asset.clone(), config, data)?;
            let index = reserve.config.index;
            asset_index.insert(asset.clone(), index);
            if reserves.insert(index, reserve).is_some() {
                return Err(ChainError::Shape(format!(
                    "pool {} lists two reserves with index {index}",
                    self.pool
                )));
            }
        }

        let mut positions = BTreeMap::new();
        for user in users {
            let entry = entries.get(&keys::positions(self.pool, user)?)?;
            let user_positions = match entry {
                Some(entry) => decode::positions(&entry.data)?,
                None => Positions::default(),
            };
            positions.insert((*user).to_string(), user_positions);
        }

        let (prices, price_timestamps) = self
            .prices(&instance.config.oracle, &assets, ledger)
            .await?;
        Ok(PoolSnapshot {
            ledger,
            pool: self.pool.to_string(),
            instance,
            reserves,
            asset_index,
            prices,
            price_timestamps,
            positions,
        })
    }

    /// The user's open auction of that type, from temporary storage, with
    /// the ledger it was read at. `None` is "no auction", which is also what
    /// an expired temporary entry looks like.
    pub async fn auction(
        &self,
        user: &str,
        auction_type: AuctionType,
    ) -> Result<Option<(u32, AuctionData)>, ChainError> {
        let key = keys::auction(self.pool, user, auction_type)?;
        let entries = self.rpc.ledger_entries(std::slice::from_ref(&key)).await?;
        match entries.get(&key)? {
            Some(entry) => Ok(Some((entries.latest_ledger, decode::auction(&entry.data)?))),
            None => Ok(None),
        }
    }

    /// `account`'s balance of `token`, by simulating the token's `balance`,
    /// with the ledger it was read at.
    pub async fn balance(&self, token: &str, account: &str) -> Result<(u32, i128), ChainError> {
        let (ledger, value) = self.view(token, "balance", vec![address(account)?]).await?;
        match value {
            ScVal::I128(parts) => Ok((ledger, i128::from(&parts))),
            other => Err(ChainError::Xdr(XdrError::Shape {
                expected: "i128 balance",
                got: format!("{other:?}"),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::script::{scval_b64, transaction_data_b64, ScriptedRpc};
    use crate::chain::xdr::encode::{
        i128_val, map, sc_address, symbol, to_base64, vec as sc_vec, RequestType,
    };
    use crate::chain::xdr::keys;
    use crate::fixture::{mainnet_fixed_v2, text};
    use serde_json::{json, Value};
    use stellar_xdr::{
        ContractDataDurability, ContractDataEntry, ExtensionPoint, HostFunction, LedgerEntryData,
        OperationBody, ScVal,
    };

    const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
    const USER: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
    const FILLER: &str = "GCIH7OYRDHJ3IOPFEM7DMUX3SXTVHOO2XSWLGBMSVQ3EIHPHYUTNJID3";
    const USDC: &str = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";

    fn invoke(operation: &Operation) -> (String, Vec<ScVal>) {
        let OperationBody::InvokeHostFunction(op) = &operation.body else {
            panic!("invoke")
        };
        let HostFunction::InvokeContract(args) = &op.host_function else {
            panic!("invoke contract")
        };
        (
            args.function_name.to_utf8_string_lossy(),
            args.args.to_vec(),
        )
    }

    #[test]
    fn submit_op_calls_submit_with_from_spender_to_and_the_requests() {
        let requests = [Request {
            request_type: RequestType::FillUserLiquidationAuction,
            address: USER.to_string(),
            amount: 60,
        }];
        let op = submit_op(POOL, FILLER, FILLER, FILLER, &requests).unwrap();
        let (function, args) = invoke(&op);
        assert_eq!(function, "submit");
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], address(FILLER).unwrap());
        assert_eq!(
            args[3],
            sc_vec(vec![request(&requests[0]).unwrap()]).unwrap()
        );
    }

    #[test]
    fn new_auction_op_and_bad_debt_op_match_the_contract_signatures() {
        let op =
            new_auction_op(POOL, AuctionType::UserLiquidation, USER, &[USDC], &[], 50).unwrap();
        let (function, args) = invoke(&op);
        assert_eq!(function, "new_auction");
        assert_eq!(args[0], ScVal::U32(0));
        assert_eq!(args[1], address(USER).unwrap());
        assert_eq!(args[2], sc_vec(vec![address(USDC).unwrap()]).unwrap());
        assert_eq!(args[3], sc_vec(vec![]).unwrap());
        assert_eq!(args[4], ScVal::U32(50));
        let (function, args) = invoke(&bad_debt_op(POOL, USER).unwrap());
        assert_eq!((function.as_str(), args.len()), ("bad_debt", 1));
        assert!(new_auction_op(
            POOL,
            AuctionType::UserLiquidation,
            USER,
            &["not-an-address"],
            &[],
            50
        )
        .is_err());
    }

    fn entry(key: &stellar_xdr::LedgerKey, xdr: &str) -> Value {
        json!({"key": to_base64(key).unwrap(), "xdr": xdr, "lastModifiedLedgerSeq": 1, "liveUntilLedgerSeq": 99_999_999})
    }

    fn simulation(return_xdr: &str, ledger: u32) -> Value {
        json!({"transactionData": transaction_data_b64(1), "events": [], "minResourceFee": "1",
               "results": [{"auth": [], "xdr": return_xdr}], "latestLedger": ledger})
    }

    /// Scripts the fixture's ledger: the shape read, the full read, then the
    /// oracle's decimals and one lastprice per reserve in list order.
    fn script_fixture(rpc: &ScriptedRpc, fixture: &Value, second_ledger: u32) {
        let ledger = fixture["ledger"].as_u64().unwrap();
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(POOL).unwrap(), text(fixture, &["instance_entry_xdr"])),
                entry(&keys::reserve_list(POOL).unwrap(), text(fixture, &["res_list_entry_xdr"])),
            ]}),
        );
        let mut entries = Vec::new();
        for reserve in fixture["reserves"].as_array().unwrap() {
            let asset = reserve["asset"].as_str().unwrap();
            entries.push(entry(
                &keys::reserve_config(POOL, asset).unwrap(),
                reserve["config_entry_xdr"].as_str().unwrap(),
            ));
            entries.push(entry(
                &keys::reserve_data(POOL, asset).unwrap(),
                reserve["data_entry_xdr"].as_str().unwrap(),
            ));
        }
        for user in fixture["users"].as_array().unwrap() {
            let account = user["account"].as_str().unwrap();
            entries.push(entry(
                &keys::positions(POOL, account).unwrap(),
                user["positions_entry_xdr"].as_str().unwrap(),
            ));
        }
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": second_ledger, "entries": entries}),
        );
        rpc.expect(
            "simulateTransaction",
            simulation(
                text(fixture, &["oracle_decimals_return_xdr"]),
                second_ledger,
            ),
        );
        for reserve in fixture["reserves"].as_array().unwrap() {
            rpc.expect(
                "simulateTransaction",
                simulation(
                    reserve["lastprice_return_xdr"].as_str().unwrap(),
                    second_ledger,
                ),
            );
        }
    }

    /// Like `script_fixture`, but the second reserve's `ResConfig` entry is
    /// swapped for the first reserve's, so both decode to `config.index` 0
    /// — the collision `snapshot` must refuse. The refusal happens before
    /// any oracle read, so no simulation is scripted.
    fn script_fixture_with_duplicate_index(rpc: &ScriptedRpc, fixture: &Value, ledger: u32) {
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": [
                entry(&keys::instance(POOL).unwrap(), text(fixture, &["instance_entry_xdr"])),
                entry(&keys::reserve_list(POOL).unwrap(), text(fixture, &["res_list_entry_xdr"])),
            ]}),
        );
        let reserves = fixture["reserves"].as_array().unwrap();
        let duplicate_config = reserves[0]["config_entry_xdr"].as_str().unwrap();
        let mut entries = Vec::new();
        for (index, reserve) in reserves.iter().enumerate() {
            let asset = reserve["asset"].as_str().unwrap();
            let config_xdr = if index == 1 {
                duplicate_config
            } else {
                reserve["config_entry_xdr"].as_str().unwrap()
            };
            entries.push(entry(
                &keys::reserve_config(POOL, asset).unwrap(),
                config_xdr,
            ));
            entries.push(entry(
                &keys::reserve_data(POOL, asset).unwrap(),
                reserve["data_entry_xdr"].as_str().unwrap(),
            ));
        }
        for user in fixture["users"].as_array().unwrap() {
            let account = user["account"].as_str().unwrap();
            entries.push(entry(
                &keys::positions(POOL, account).unwrap(),
                user["positions_entry_xdr"].as_str().unwrap(),
            ));
        }
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": ledger, "entries": entries}),
        );
    }

    /// The payoff: read through the client, value through `math`, and land
    /// on the same golden health factors `chain::xdr::decode`'s test derives
    /// from the same attested inputs.
    #[tokio::test]
    async fn a_snapshot_of_the_fixture_ledger_values_its_users_to_the_golden_health_factors() {
        let fixture = mainnet_fixed_v2();
        let ledger = u32::try_from(fixture["ledger"].as_u64().unwrap()).unwrap();
        let close_time = fixture["ledger_close_time"].as_u64().unwrap();
        let rpc = ScriptedRpc::start().await;
        script_fixture(&rpc, &fixture, ledger);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let users: Vec<&str> = fixture["users"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["account"].as_str().unwrap())
            .collect();

        let snapshot = PoolReader::new(&client, POOL)
            .snapshot(&users)
            .await
            .unwrap();
        assert_eq!(snapshot.ledger, ledger);
        assert_eq!(
            snapshot.instance.config.oracle,
            fixture["oracle"].as_str().unwrap()
        );
        assert_eq!(snapshot.reserves.len(), 3);
        assert_eq!(snapshot.asset_index.len(), 3);
        assert_eq!(snapshot.prices.decimals(), 7);
        assert_eq!(snapshot.price_timestamps.len(), 3);
        for reserve in snapshot.reserves.values() {
            assert!(snapshot.prices.price(&reserve.asset).unwrap() > 0);
            assert_eq!(snapshot.asset_index[&reserve.asset], reserve.config.index);
        }
        let first = snapshot
            .position_data(users[0], close_time)
            .unwrap()
            .unwrap();
        assert_eq!(first.health_factor(), Ok(Some(10_070_767)));
        let second = snapshot
            .position_data(users[1], close_time)
            .unwrap()
            .unwrap();
        assert_eq!(second.health_factor(), Ok(Some(10_100_345)));
        assert!(snapshot
            .position_data("GA…unknown", close_time)
            .unwrap()
            .is_none());
        // The oracle was asked for decimals, then one lastprice per reserve, in list order.
        let simulations = rpc.calls("simulateTransaction");
        assert_eq!(simulations.len(), 4);
        assert_eq!(rpc.remaining(), 0);
    }

    #[tokio::test]
    async fn a_snapshot_refuses_a_ledger_that_moved_between_reads() {
        let fixture = mainnet_fixed_v2();
        let ledger = u32::try_from(fixture["ledger"].as_u64().unwrap()).unwrap();
        let rpc = ScriptedRpc::start().await;
        script_fixture(&rpc, &fixture, ledger + 1);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let error = PoolReader::new(&client, POOL)
            .snapshot(&[USER])
            .await
            .unwrap_err();
        assert!(matches!(error, ChainError::LedgerMoved { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_snapshot_refuses_two_reserves_with_one_index() {
        let fixture = mainnet_fixed_v2();
        let ledger = u32::try_from(fixture["ledger"].as_u64().unwrap()).unwrap();
        let rpc = ScriptedRpc::start().await;
        script_fixture_with_duplicate_index(&rpc, &fixture, ledger);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let error = PoolReader::new(&client, POOL)
            .snapshot(&[USER])
            .await
            .unwrap_err();
        let ChainError::Shape(message) = error else {
            panic!("expected a shape error, got {error:?}")
        };
        assert!(message.contains("two reserves with index"), "{message}");
        // The collision is caught before any oracle read.
        assert!(rpc.calls("simulateTransaction").is_empty());
    }

    #[tokio::test]
    async fn a_user_without_a_positions_entry_has_empty_positions() {
        let fixture = mainnet_fixed_v2();
        let ledger = u32::try_from(fixture["ledger"].as_u64().unwrap()).unwrap();
        let rpc = ScriptedRpc::start().await;
        // Script the fixture's users, then ask for a third the ledger does not hold.
        script_fixture(&rpc, &fixture, ledger);
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let mut users: Vec<&str> = fixture["users"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["account"].as_str().unwrap())
            .collect();
        users.push(FILLER);
        let snapshot = PoolReader::new(&client, POOL)
            .snapshot(&users)
            .await
            .unwrap();
        assert!(snapshot.positions[FILLER].is_empty());
        assert!(snapshot.position_data(FILLER, 1).unwrap().is_none());
    }

    fn auction_entry_xdr() -> String {
        let side = |amount: i128| map(vec![(address(USDC).unwrap(), i128_val(amount))]).unwrap();
        let auction = map(vec![
            (symbol("bid").unwrap(), side(1_000)),
            (symbol("block").unwrap(), ScVal::U32(64_271_300)),
            (symbol("lot").unwrap(), side(2_000)),
        ])
        .unwrap();
        let auction_key = map(vec![
            (symbol("auct_type").unwrap(), ScVal::U32(0)),
            (symbol("user").unwrap(), address(USER).unwrap()),
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

    #[tokio::test]
    async fn an_auction_is_read_from_temporary_storage_and_absent_is_none() {
        let key = keys::auction(POOL, USER, AuctionType::UserLiquidation).unwrap();
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 5, "entries": [entry(&key, &auction_entry_xdr())]}),
        );
        rpc.expect(
            "getLedgerEntries",
            json!({"latestLedger": 6, "entries": []}),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        let reader = PoolReader::new(&client, POOL);
        let (ledger, auction) = reader
            .auction(USER, AuctionType::UserLiquidation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ledger, 5);
        assert_eq!(auction.block, 64_271_300);
        assert_eq!(auction.bid[USDC], 1_000);
        assert_eq!(auction.lot[USDC], 2_000);
        assert!(reader
            .auction(USER, AuctionType::UserLiquidation)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            rpc.calls("getLedgerEntries")[0]["keys"][0],
            to_base64(&key).unwrap()
        );
    }

    #[tokio::test]
    async fn a_balance_is_simulated_on_the_token() {
        let rpc = ScriptedRpc::start().await;
        rpc.expect(
            "simulateTransaction",
            simulation(&scval_b64(&i128_val(123_456_789)), 9),
        );
        let client = RpcClient::new(&rpc.url(), None).unwrap();
        assert_eq!(
            PoolReader::new(&client, POOL)
                .balance(USDC, FILLER)
                .await
                .unwrap(),
            (9, 123_456_789)
        );
        let envelope: stellar_xdr::TransactionEnvelope = crate::chain::xdr::encode::from_base64(
            rpc.calls("simulateTransaction")[0]["transaction"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let stellar_xdr::TransactionEnvelope::Tx(v1) = envelope else {
            panic!("v1")
        };
        let (function, args) = invoke(&v1.tx.operations[0]);
        assert_eq!((function.as_str(), args.len()), ("balance", 1));
    }
}
