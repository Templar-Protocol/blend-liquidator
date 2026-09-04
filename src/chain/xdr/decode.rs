//! Turning ledger entries and view-call results into the bot's types.
//!
//! Every decoder is written against the contract's own struct definitions.
//! A shape that does not match is an `XdrError`, never a default value: a
//! silently-zero rate or balance would produce a confident wrong decision,
//! and a loud failure at startup is the cheaper outcome.

use std::collections::BTreeMap;

use stellar_xdr::{Int128Parts, LedgerEntryData, ScVal};

use super::encode::from_base64;
use super::XdrError;
use crate::math::{AuctionData, Positions, Reserve, ReserveConfig, ReserveData};

/// The pool's `PoolConfig` instance-storage entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// The SEP-40 oracle the pool prices against.
    pub oracle: String,
    /// The backstop's share of accrued interest, 7 decimals.
    pub bstop_rate: u32,
    /// 0 admin-active, 1 active, 2/3 on-ice, 4/5 frozen, 6 setup.
    pub status: u32,
    /// The most collateral-plus-liability positions one account may hold,
    /// and the most assets one auction may name.
    pub max_positions: u32,
    /// The least collateral, in the oracle's decimals, a borrowing position
    /// must hold.
    pub min_collateral: i128,
}

/// Everything the pool keeps in instance storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInstance {
    /// The pool's admin account.
    pub admin: String,
    /// The backstop contract, and the auction counterparty for bad debt and
    /// interest auctions.
    pub backstop: String,
    /// The BLND token the pool emits.
    pub blnd_token: String,
    /// The pool's display name.
    pub name: String,
    /// The pool's configuration.
    pub config: PoolConfig,
}

/// A SEP-40 `PriceData`: a price in the oracle's decimals and the second it
/// was published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceData {
    /// The price, in the oracle's decimals.
    pub price: i128,
    /// Publication time, seconds since the epoch.
    pub timestamp: u64,
}

fn shape(expected: &'static str, got: &impl std::fmt::Debug) -> XdrError {
    XdrError::Shape {
        expected,
        got: format!("{got:?}"),
    }
}

fn contract_value(entry: &LedgerEntryData) -> Result<&ScVal, XdrError> {
    match entry {
        LedgerEntryData::ContractData(data) => Ok(&data.val),
        other => Err(shape("contract data", other)),
    }
}

fn as_i128(parts: &Int128Parts) -> i128 {
    (i128::from(parts.hi) << 64) | i128::from(parts.lo)
}

/// The fields of a contract struct, keyed by their symbol names.
fn fields(value: &ScVal) -> Result<BTreeMap<String, &ScVal>, XdrError> {
    match value {
        ScVal::Map(Some(entries)) => entries
            .iter()
            .map(|entry| match &entry.key {
                ScVal::Symbol(name) => Ok((name.to_utf8_string_lossy(), &entry.val)),
                other => Err(shape("symbol key", other)),
            })
            .collect(),
        other => Err(shape("struct map", other)),
    }
}

fn field<'a>(
    fields: &BTreeMap<String, &'a ScVal>,
    name: &'static str,
) -> Result<&'a ScVal, XdrError> {
    fields
        .get(name)
        .copied()
        .ok_or(XdrError::MissingField(name))
}

fn u32_field(fields: &BTreeMap<String, &ScVal>, name: &'static str) -> Result<u32, XdrError> {
    match field(fields, name)? {
        ScVal::U32(value) => Ok(*value),
        other => Err(shape("u32", other)),
    }
}

fn u64_field(fields: &BTreeMap<String, &ScVal>, name: &'static str) -> Result<u64, XdrError> {
    match field(fields, name)? {
        ScVal::U64(value) => Ok(*value),
        other => Err(shape("u64", other)),
    }
}

fn i128_field(fields: &BTreeMap<String, &ScVal>, name: &'static str) -> Result<i128, XdrError> {
    match field(fields, name)? {
        ScVal::I128(parts) => Ok(as_i128(parts)),
        other => Err(shape("i128", other)),
    }
}

fn bool_field(fields: &BTreeMap<String, &ScVal>, name: &'static str) -> Result<bool, XdrError> {
    match field(fields, name)? {
        ScVal::Bool(value) => Ok(*value),
        other => Err(shape("bool", other)),
    }
}

fn address_field(
    fields: &BTreeMap<String, &ScVal>,
    name: &'static str,
) -> Result<String, XdrError> {
    match field(fields, name)? {
        ScVal::Address(address) => Ok(address.to_string()),
        other => Err(shape("address", other)),
    }
}

/// A map of reserve index to token amount, as `Positions` stores each side.
fn index_map(value: &ScVal) -> Result<BTreeMap<u32, i128>, XdrError> {
    match value {
        ScVal::Map(Some(entries)) => entries
            .iter()
            .map(|entry| match (&entry.key, &entry.val) {
                (ScVal::U32(index), ScVal::I128(amount)) => Ok((*index, as_i128(amount))),
                (key, _) => Err(shape("u32 to i128", key)),
            })
            .collect(),
        other => Err(shape("index map", other)),
    }
}

/// A map of asset address to amount, as `AuctionData` stores each side.
fn address_map(value: &ScVal) -> Result<BTreeMap<String, i128>, XdrError> {
    match value {
        ScVal::Map(Some(entries)) => entries
            .iter()
            .map(|entry| match (&entry.key, &entry.val) {
                (ScVal::Address(asset), ScVal::I128(amount)) => {
                    Ok((asset.to_string(), as_i128(amount)))
                }
                (key, _) => Err(shape("address to i128", key)),
            })
            .collect(),
        other => Err(shape("address map", other)),
    }
}

/// Decodes the pool's contract-instance entry.
pub fn pool_instance(entry: &LedgerEntryData) -> Result<PoolInstance, XdrError> {
    let ScVal::ContractInstance(instance) = contract_value(entry)? else {
        return Err(shape("contract instance", entry));
    };
    let storage: BTreeMap<String, &ScVal> = instance
        .storage
        .iter()
        .flat_map(|map| map.iter())
        .filter_map(|entry| match &entry.key {
            ScVal::Symbol(name) => Some((name.to_utf8_string_lossy(), &entry.val)),
            _ => None,
        })
        .collect();

    let config_fields = fields(field(&storage, "Config")?)?;
    let name = match field(&storage, "Name")? {
        ScVal::String(text) => text.to_utf8_string_lossy(),
        other => return Err(shape("string", other)),
    };
    Ok(PoolInstance {
        admin: address_field(&storage, "Admin")?,
        backstop: address_field(&storage, "Backstop")?,
        blnd_token: address_field(&storage, "BLNDTkn")?,
        name,
        config: PoolConfig {
            oracle: address_field(&config_fields, "oracle")?,
            bstop_rate: u32_field(&config_fields, "bstop_rate")?,
            status: u32_field(&config_fields, "status")?,
            max_positions: u32_field(&config_fields, "max_positions")?,
            min_collateral: i128_field(&config_fields, "min_collateral")?,
        },
    })
}

/// Decodes `ResList`. The position in this vector is the reserve index that
/// `Positions` keys on.
pub fn reserve_list(entry: &LedgerEntryData) -> Result<Vec<String>, XdrError> {
    match contract_value(entry)? {
        ScVal::Vec(Some(items)) => items
            .iter()
            .map(|item| match item {
                ScVal::Address(address) => Ok(address.to_string()),
                other => Err(shape("address", other)),
            })
            .collect(),
        other => Err(shape("vector of addresses", other)),
    }
}

/// Decodes a `ResConfig` entry.
pub fn reserve_config(entry: &LedgerEntryData) -> Result<ReserveConfig, XdrError> {
    reserve_config_value(contract_value(entry)?)
}

/// Decodes a `ResData` entry.
pub fn reserve_data(entry: &LedgerEntryData) -> Result<ReserveData, XdrError> {
    reserve_data_value(contract_value(entry)?)
}

/// Decodes a `Positions` entry.
pub fn positions(entry: &LedgerEntryData) -> Result<Positions, XdrError> {
    positions_value(contract_value(entry)?)
}

/// Decodes an `Auction` entry.
pub fn auction(entry: &LedgerEntryData) -> Result<AuctionData, XdrError> {
    auction_value(contract_value(entry)?)
}

/// Decodes a `ReserveConfig` value, wherever it came from.
pub fn reserve_config_value(value: &ScVal) -> Result<ReserveConfig, XdrError> {
    let fields = fields(value)?;
    Ok(ReserveConfig {
        index: u32_field(&fields, "index")?,
        decimals: u32_field(&fields, "decimals")?,
        c_factor: u32_field(&fields, "c_factor")?,
        l_factor: u32_field(&fields, "l_factor")?,
        util: u32_field(&fields, "util")?,
        max_util: u32_field(&fields, "max_util")?,
        r_base: u32_field(&fields, "r_base")?,
        r_one: u32_field(&fields, "r_one")?,
        r_two: u32_field(&fields, "r_two")?,
        r_three: u32_field(&fields, "r_three")?,
        reactivity: u32_field(&fields, "reactivity")?,
        supply_cap: i128_field(&fields, "supply_cap")?,
        enabled: bool_field(&fields, "enabled")?,
    })
}

/// Decodes a `ReserveData` value, wherever it came from.
pub fn reserve_data_value(value: &ScVal) -> Result<ReserveData, XdrError> {
    let fields = fields(value)?;
    Ok(ReserveData {
        d_rate: i128_field(&fields, "d_rate")?,
        b_rate: i128_field(&fields, "b_rate")?,
        ir_mod: i128_field(&fields, "ir_mod")?,
        b_supply: i128_field(&fields, "b_supply")?,
        d_supply: i128_field(&fields, "d_supply")?,
        backstop_credit: i128_field(&fields, "backstop_credit")?,
        last_time: u64_field(&fields, "last_time")?,
    })
}

/// Decodes the pool's `get_reserve` return: config, data, asset and scalar
/// already accrued to the simulated ledger.
pub fn reserve_value(value: &ScVal) -> Result<Reserve, XdrError> {
    let fields = fields(value)?;
    Ok(Reserve {
        asset: address_field(&fields, "asset")?,
        config: reserve_config_value(field(&fields, "config")?)?,
        data: reserve_data_value(field(&fields, "data")?)?,
        scalar: i128_field(&fields, "scalar")?,
    })
}

/// Decodes a `Positions` value, as stored or as `get_positions` returns it.
pub fn positions_value(value: &ScVal) -> Result<Positions, XdrError> {
    let fields = fields(value)?;
    Ok(Positions {
        collateral: index_map(field(&fields, "collateral")?)?,
        liabilities: index_map(field(&fields, "liabilities")?)?,
        supply: index_map(field(&fields, "supply")?)?,
    })
}

/// Decodes an `AuctionData` value, as stored or as an event carries it.
pub fn auction_value(value: &ScVal) -> Result<AuctionData, XdrError> {
    let fields = fields(value)?;
    Ok(AuctionData {
        bid: address_map(field(&fields, "bid")?)?,
        lot: address_map(field(&fields, "lot")?)?,
        block: u32_field(&fields, "block")?,
    })
}

/// Decodes a SEP-40 `lastprice` return. The oracle returns an `Option`, and
/// a `None` price is a missing price, not a zero.
pub fn price_data(value: &ScVal) -> Result<PriceData, XdrError> {
    let fields = fields(value)?;
    Ok(PriceData {
        price: i128_field(&fields, "price")?,
        timestamp: u64_field(&fields, "timestamp")?,
    })
}

/// Decodes a SEP-40 `decimals` return.
pub fn decimals(value: &ScVal) -> Result<u32, XdrError> {
    match value {
        ScVal::U32(decimals) => Ok(*decimals),
        other => Err(shape("u32", other)),
    }
}

/// Decodes a base64 ledger entry straight to its `LedgerEntryData`.
pub fn entry_from_base64(text: &str) -> Result<LedgerEntryData, XdrError> {
    from_base64(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{mainnet_fixed_v2, text};
    use crate::math::{calculate_position_data, OraclePrices, Reserve};
    use std::collections::BTreeMap;

    fn entry(base64: &str) -> LedgerEntryData {
        from_base64(base64).expect("entry parses")
    }

    fn value(base64: &str) -> ScVal {
        from_base64(base64).expect("value parses")
    }

    #[test]
    fn decodes_the_pool_instance() {
        let fixture = mainnet_fixed_v2();
        let instance =
            pool_instance(&entry(text(&fixture, &["instance_entry_xdr"]))).expect("instance");
        assert_eq!(
            instance.admin,
            "GAX2VVWVHU5YQY5J3NJBXKHI3FFKZN54BE6GRJCWSIKSBZTQWJJNJMPC"
        );
        assert_eq!(
            instance.backstop,
            "CAQQR5SWBXKIGZKPBZDH3KM5GQ5GUTPKB7JAFCINLZBC5WXPJKRG3IM7"
        );
        assert_eq!(
            instance.blnd_token,
            "CD25MNVTZDL4Y3XBCPCJXGXATV5WUHHOWMYFF4YBEGU5FCPGMYTVG5JY"
        );
        assert_eq!(instance.name, "Fixed");
        assert_eq!(
            instance.config.oracle,
            "CCVTVW2CVA7JLH4ROQGP3CU4T3EXVCK66AZGSM4MUQPXAI4QHCZPOATS"
        );
        assert_eq!(instance.config.bstop_rate, 2_000_000);
        assert_eq!(instance.config.status, 1);
        assert_eq!(instance.config.max_positions, 6);
        assert_eq!(instance.config.min_collateral, 50_000_000);
    }

    #[test]
    fn decodes_the_reserve_list_in_index_order() {
        let fixture = mainnet_fixed_v2();
        let assets = reserve_list(&entry(text(&fixture, &["res_list_entry_xdr"]))).expect("list");
        assert_eq!(
            assets,
            vec![
                "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA".to_string(),
                "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75".to_string(),
                "CDTKPWPLOURQA2SGTKTUQOWRCBZEORB4BWBOMJ3D3ZTQQSGE5F6JBQLV".to_string(),
            ]
        );
    }

    #[test]
    fn decodes_a_reserve_config_and_data() {
        let fixture = mainnet_fixed_v2();
        let config = reserve_config(&entry(text(
            &fixture,
            &["reserves", "0", "config_entry_xdr"],
        )))
        .expect("config");
        assert_eq!(config.index, 0);
        assert_eq!(config.decimals, 7);
        assert_eq!(config.c_factor, 7_500_000);
        assert_eq!(config.l_factor, 7_500_000);
        assert_eq!(config.util, 4_000_000);
        assert_eq!(config.max_util, 7_000_000);
        assert_eq!(config.r_base, 100_000);
        assert_eq!(config.r_one, 300_000);
        assert_eq!(config.r_two, 3_000_000);
        assert_eq!(config.r_three, 50_000_000);
        assert_eq!(config.reactivity, 50);
        assert_eq!(config.supply_cap, 100_000_000_000_000_000);
        assert!(config.enabled);

        let data = reserve_data(&entry(text(&fixture, &["reserves", "0", "data_entry_xdr"])))
            .expect("data");
        assert_eq!(data.d_rate, 1_001_568_283_884);
        assert_eq!(data.b_rate, 1_000_022_303_241);
        assert_eq!(data.ir_mod, 1_000_000);
        assert_eq!(data.b_supply, 7_654_654_078_715_796);
        assert_eq!(data.d_supply, 13_201_825_877_188);
        assert_eq!(data.backstop_credit, 31_426_481);
        assert_eq!(data.last_time, 1_788_533_688);
    }

    #[test]
    fn decodes_the_oracle_decimals_and_prices() {
        let fixture = mainnet_fixed_v2();
        assert_eq!(
            decimals(&value(text(&fixture, &["oracle_decimals_return_xdr"]))),
            Ok(7)
        );
        let price = price_data(&value(text(
            &fixture,
            &["reserves", "0", "lastprice_return_xdr"],
        )))
        .expect("price");
        assert_eq!(price.price, 1_778_617);
        assert_eq!(price.timestamp, 1_788_534_300);
    }

    #[test]
    fn decodes_positions_identically_from_the_entry_and_the_view_call() {
        let fixture = mainnet_fixed_v2();
        let from_entry = positions(&entry(text(
            &fixture,
            &["users", "0", "positions_entry_xdr"],
        )))
        .expect("entry");
        let from_view = positions_value(&value(text(
            &fixture,
            &["users", "0", "get_positions_return_xdr"],
        )))
        .expect("view");
        assert_eq!(from_entry.collateral, BTreeMap::from([(1, 125_043_746)]));
        assert_eq!(from_entry.liabilities, BTreeMap::from([(1, 104_293_813)]));
        assert!(from_entry.supply.is_empty());
        assert_eq!(from_entry, from_view);
    }

    #[test]
    fn an_auction_round_trips_through_its_ledger_value() {
        // The retained event window held no liquidations when the fixture was
        // captured, so this shape is pinned by construction rather than by a
        // captured entry. The field names and types come from the contract's
        // `AuctionData`.
        use crate::chain::xdr::encode::{address, i128_val, map, symbol, to_base64};
        let usdc = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75";
        let xlm = "CAS3J7GYLGXMF6TDJBBYYSE3HQ6BBSMLNUQ34T6TZMYMW2EVH34XOWMA";
        let encoded = map(vec![
            (
                symbol("bid").expect("symbol"),
                map(vec![(address(usdc).expect("address"), i128_val(1_234))]).expect("bid"),
            ),
            (symbol("block").expect("symbol"), ScVal::U32(64_271_347)),
            (
                symbol("lot").expect("symbol"),
                map(vec![(address(xlm).expect("address"), i128_val(5_678))]).expect("lot"),
            ),
        ])
        .expect("auction");
        let text = to_base64(&encoded).expect("base64");
        let decoded = auction_value(&value(&text)).expect("auction");
        assert_eq!(decoded.block, 64_271_347);
        assert_eq!(decoded.bid, BTreeMap::from([(usdc.to_string(), 1_234)]));
        assert_eq!(decoded.lot, BTreeMap::from([(xlm.to_string(), 5_678)]));
    }

    #[test]
    fn accruing_the_stored_entries_reproduces_the_contracts_get_reserve() {
        // The payoff test for `math::reserve`: the fixture's entries and the
        // contract's own `get_reserve` were read at one ledger, so accruing
        // the former to that ledger's close time must produce the latter,
        // to the stroop, for every reserve.
        let fixture = mainnet_fixed_v2();
        let instance =
            pool_instance(&entry(text(&fixture, &["instance_entry_xdr"]))).expect("instance");
        let now = fixture["ledger_close_time"].as_u64().expect("close time");
        let reserves = fixture["reserves"].as_array().expect("reserves");
        assert_eq!(reserves.len(), 3);
        for (index, _) in reserves.iter().enumerate() {
            let position = index.to_string();
            let asset = text(&fixture, &["reserves", &position, "asset"]).to_string();
            let config = reserve_config(&entry(text(
                &fixture,
                &["reserves", &position, "config_entry_xdr"],
            )))
            .expect("config");
            let data = reserve_data(&entry(text(
                &fixture,
                &["reserves", &position, "data_entry_xdr"],
            )))
            .expect("data");
            let mut reserve = Reserve::new(asset.clone(), config, data).expect("scalar");
            reserve
                .accrue(instance.config.bstop_rate, now)
                .expect("accrues");

            let expected = reserve_value(&value(text(
                &fixture,
                &["reserves", &position, "get_reserve_return_xdr"],
            )))
            .expect("get_reserve");
            assert_eq!(reserve.asset, expected.asset, "asset for reserve {index}");
            assert_eq!(
                reserve.scalar, expected.scalar,
                "scalar for reserve {index}"
            );
            assert_eq!(
                reserve.config, expected.config,
                "config for reserve {index}"
            );
            assert_eq!(
                reserve.data, expected.data,
                "accrued data for reserve {index}"
            );
        }
    }

    #[test]
    fn values_the_fixtures_users_the_way_the_contract_would() {
        // The payoff test for `math::position`: real positions, real accrued
        // reserves, real oracle prices, at one ledger.
        let fixture = mainnet_fixed_v2();
        let instance =
            pool_instance(&entry(text(&fixture, &["instance_entry_xdr"]))).expect("instance");
        let now = fixture["ledger_close_time"].as_u64().expect("close time");

        let mut reserves = BTreeMap::new();
        let mut prices = BTreeMap::new();
        for index in 0..fixture["reserves"].as_array().expect("reserves").len() {
            let position = index.to_string();
            let asset = text(&fixture, &["reserves", &position, "asset"]).to_string();
            let mut reserve = reserve_value(&value(text(
                &fixture,
                &["reserves", &position, "get_reserve_return_xdr"],
            )))
            .expect("reserve");
            reserve
                .accrue(instance.config.bstop_rate, now)
                .expect("already accrued");
            prices.insert(
                asset,
                price_data(&value(text(
                    &fixture,
                    &["reserves", &position, "lastprice_return_xdr"],
                )))
                .expect("price")
                .price,
            );
            reserves.insert(reserve.config.index, reserve);
        }
        let oracle_decimals =
            decimals(&value(text(&fixture, &["oracle_decimals_return_xdr"]))).expect("decimals");
        let prices = OraclePrices::new(oracle_decimals, prices).expect("scalar");

        let first = positions(&entry(text(
            &fixture,
            &["users", "0", "positions_entry_xdr"],
        )))
        .expect("positions");
        let data = calculate_position_data(&reserves, &prices, &first).expect("values");
        assert_eq!(data.collateral_base, 135_838_407);
        assert_eq!(data.collateral_raw, 142_987_797);
        assert_eq!(data.liability_base, 134_883_864);
        assert_eq!(data.liability_raw, 128_139_670);
        assert_eq!(data.health_factor(), Ok(Some(10_070_767)));

        let second = positions(&entry(text(
            &fixture,
            &["users", "1", "positions_entry_xdr"],
        )))
        .expect("positions");
        let data = calculate_position_data(&reserves, &prices, &second).expect("values");
        assert_eq!(data.collateral_base, 9_599_134_909);
        assert_eq!(data.collateral_raw, 10_104_352_536);
        assert_eq!(data.liability_base, 9_503_768_801);
        assert_eq!(data.liability_raw, 9_028_580_360);
        assert_eq!(data.health_factor(), Ok(Some(10_100_345)));
        // Both users are above water, so neither would be liquidatable.
        assert_eq!(data.is_hf_under(9_980_000), Ok(false));
    }

    #[test]
    fn a_value_of_the_wrong_shape_is_an_error_not_a_panic() {
        assert!(matches!(
            decimals(&ScVal::Void),
            Err(XdrError::Shape { .. })
        ));
        assert!(matches!(
            positions_value(&ScVal::U32(1)),
            Err(XdrError::Shape { .. })
        ));
        let empty = crate::chain::xdr::encode::map(vec![]).expect("map");
        assert!(matches!(
            price_data(&empty),
            Err(XdrError::MissingField("price"))
        ));
    }
}
