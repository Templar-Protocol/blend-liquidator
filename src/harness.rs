//! Test scaffolding: a scripted RPC that answers from the committed mainnet
//! fixture, and the store to write what it says into.

use serde_json::{json, Value};

use crate::chain::script::ScriptedRpc;
use crate::chain::xdr::encode::to_base64;
use crate::chain::xdr::keys;
use crate::fixture::{mainnet_fixed_v2, text};

/// The fixture's pool, its two borrowers and its USDC reserve.
pub(crate) const POOL: &str = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD";
pub(crate) const USER_ONE: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";
pub(crate) const USER_TWO: &str = "GCIH7OYR6LX6364PGGLAKGMZYLV37EAH6YXZAFK7RY7U4K7625XBH5EL";

/// The golden health factors `chain::xdr::decode`'s test derives from the
/// same attested inputs. The fixture's oracle has 7 decimals, so
/// normalising to 7 decimals is the identity and these are also what the
/// store must hold.
pub(crate) const GOLDEN_HEALTH: [(&str, i128); 2] =
    [(USER_ONE, 10_070_767), (USER_TWO, 10_100_345)];

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

/// Scripts one complete `PoolReader::snapshot` for `accounts` at the
/// fixture's ledger: the shape read, the batched entry read, then the
/// oracle's `decimals` and one `lastprice` per reserve, in reserve-list
/// order. Accounts the fixture does not hold are simply absent from the
/// entry read, which is what the RPC does for a key with no entry.
pub(crate) fn script_snapshot(rpc: &ScriptedRpc, accounts: &[&str]) {
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
    for user in fixture["users"].as_array().expect("users") {
        let account = user["account"].as_str().expect("account");
        if accounts.contains(&account) {
            entries.push(entry(
                &keys::positions(POOL, account).expect("key"),
                user["positions_entry_xdr"].as_str().expect("positions"),
            ));
        }
    }
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

/// The fixture's ledger and close time, as a tick.
pub(crate) fn fixture_tick() -> crate::ledger::LedgerTick {
    let fixture = mainnet_fixed_v2();
    crate::ledger::LedgerTick {
        sequence: u32::try_from(fixture["ledger"].as_u64().expect("ledger")).expect("fits"),
        close_time: fixture["ledger_close_time"].as_u64().expect("close time"),
    }
}

/// Scripts one `PoolReader::auction` read for `user`'s user-liquidation
/// auction: a temporary-durability `ContractData` entry holding `auction`,
/// reported at `ledger`. The entry's shape is the contract's own — a map
/// of `bid`, `block` and `lot`, under the `("Auction", {auct_type, user})`
/// key — so the read goes through the real decoder.
pub(crate) fn script_auction_entry(
    rpc: &ScriptedRpc,
    user: &str,
    auction: &crate::math::AuctionData,
    ledger: u32,
) {
    use crate::chain::xdr::encode::{address, i128_val, map, sc_address, symbol, vec as sc_vec};
    use stellar_xdr::{
        ContractDataDurability, ContractDataEntry, ExtensionPoint, LedgerEntryData, ScVal,
    };

    let side = |amounts: &std::collections::BTreeMap<String, i128>| {
        map(amounts
            .iter()
            .map(|(asset, amount)| (address(asset).expect("asset address"), i128_val(*amount)))
            .collect())
        .expect("side map")
    };
    let value = map(vec![
        (symbol("bid").expect("symbol"), side(&auction.bid)),
        (symbol("block").expect("symbol"), ScVal::U32(auction.block)),
        (symbol("lot").expect("symbol"), side(&auction.lot)),
    ])
    .expect("auction map");
    let auction_key = map(vec![
        (symbol("auct_type").expect("symbol"), ScVal::U32(0)),
        (
            symbol("user").expect("symbol"),
            address(user).expect("user"),
        ),
    ])
    .expect("auction key");
    let data = LedgerEntryData::ContractData(ContractDataEntry {
        ext: ExtensionPoint::V0,
        contract: sc_address(POOL).expect("pool address"),
        key: sc_vec(vec![symbol("Auction").expect("symbol"), auction_key]).expect("key vec"),
        durability: ContractDataDurability::Temporary,
        val: value,
    });
    let key = keys::auction(POOL, user, crate::chain::xdr::AuctionType::UserLiquidation)
        .expect("auction key");
    rpc.expect(
        "getLedgerEntries",
        json!({"latestLedger": ledger, "entries": [entry(&key, &to_base64(&data).expect("entry"))]}),
    );
}

/// Scripts one `PoolReader::auction` read that finds nothing: the answer
/// the RPC gives for a key with no entry, which is also what an expired
/// temporary auction entry looks like.
pub(crate) fn script_no_auction(rpc: &ScriptedRpc, ledger: u32) {
    rpc.expect(
        "getLedgerEntries",
        json!({"latestLedger": ledger, "entries": []}),
    );
}
