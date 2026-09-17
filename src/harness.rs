//! Test scaffolding: a scripted RPC that answers from the committed mainnet
//! fixture, the store to write what it says into, and the recording
//! notification channel the notifier, ledger and service tests assert
//! against.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use stellar_xdr::{
    ContractDataDurability, ContractDataEntry, ExtensionPoint, LedgerEntryData, ScVal,
};

use crate::chain::script::{scval_b64, transaction_data_b64, ScriptedRpc};
use crate::chain::signer::Signer;
use crate::chain::tx::TxConfig;
use crate::chain::xdr::encode::{
    address, i128_val, map, sc_address, symbol, to_base64, vec as sc_vec,
};
use crate::chain::xdr::keys;
use crate::fixture::{mainnet_fixed_v2, text};
use crate::notifier::{Notification, NotificationChannel, NotifyError};

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

/// One `getLedgerEntries` answer: the key asked for, the entry's XDR, and
/// an archival window nothing in these tests is ever outside of.
pub(crate) fn entry(key: &stellar_xdr::LedgerKey, xdr: &str) -> Value {
    json!({
        "key": to_base64(key).expect("key"),
        "xdr": xdr,
        "lastModifiedLedgerSeq": 1,
        "liveUntilLedgerSeq": 99_999_999_u32,
    })
}

/// A bare `simulateTransaction` answer carrying one return value, as
/// `chain::pool`'s and `inventory`'s own test helpers build it.
pub(crate) fn simulation(return_xdr: &str, ledger: u32) -> Value {
    json!({
        "transactionData": transaction_data_b64(1),
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
    script_auction_entry_in(rpc, POOL, user, auction, ledger);
}

/// The same, for a pool other than the fixture's — a second configured
/// pool whose entries a test builds itself.
pub(crate) fn script_auction_entry_in(
    rpc: &ScriptedRpc,
    pool: &str,
    user: &str,
    auction: &crate::math::AuctionData,
    ledger: u32,
) {
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
        contract: sc_address(pool).expect("pool address"),
        key: sc_vec(vec![symbol("Auction").expect("symbol"), auction_key]).expect("key vec"),
        durability: ContractDataDurability::Temporary,
        val: value,
    });
    let key = keys::auction(pool, user, crate::chain::xdr::AuctionType::UserLiquidation)
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

/// A second pool, built here rather than captured: the fixture holds
/// one pool, and the wallet a multi-pool tick plans against is exactly
/// what a second one is needed to pin.
pub(crate) const POOL_TWO: &str = "CAQQR5SWBXKIGZKPBZDH3KM5GQ5GUTPKB7JAFCINLZBC5WXPJKRG3IM7";
/// An asset of `POOL_TWO` that the fixture's pool does not list.
pub(crate) const BLND: &str = "CD25MNVTZDL4Y3XBCPCJXGXATV5WUHHOWMYFF4YBEGU5FCPGMYTVG5JY";
/// The fixture's oracle and admin, reused so `POOL_TWO`'s entries
/// decode against real strkeys.
const ORACLE: &str = "CCVTVW2CVA7JLH4ROQGP3CU4T3EXVCK66AZGSM4MUQPXAI4QHCZPOATS";
const ADMIN: &str = "GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE";

/// The filler's key: the account every test that simulates, signs or
/// holds a position of its own does so as. Copied from `executor.rs`'s
/// test module rather than shared with it, because a test signer is
/// scaffolding and not an interface. Deterministic, so the address a
/// scripted ledger entry is keyed by is the address the code derives.
pub(crate) fn filler_signer() -> Signer {
    let key = ed25519_dalek::SigningKey::from_bytes(&[11_u8; 32]);
    let secret = stellar_strkey::ed25519::PrivateKey(key.to_bytes()).to_string();
    Signer::from_secret(&secret).expect("signer")
}

/// Fee and polling policy short enough that nothing here waits on a
/// real interval. Copied from `executor.rs`'s test module.
pub(crate) fn tx_config() -> TxConfig {
    TxConfig {
        poll_interval: Duration::from_millis(1),
        send_retry_pause: Duration::from_millis(1),
        wait_cap: Duration::from_millis(200),
        ..TxConfig::new(100, 200, 3)
    }
}

/// One inventory refresh: a zero balance for each of the pool's three
/// reserves, which is also the whole asset set here because the
/// configured native asset is the fixture's own XLM.
pub(crate) fn script_empty_wallet(rpc: &ScriptedRpc, ledger: u32) {
    for _ in 0..3 {
        rpc.expect(
            "simulateTransaction",
            simulation(&scval_b64(&i128_val(0)), ledger),
        );
    }
}

/// A `Positions` ledger entry for `account`, by reserve index: the
/// three sides the contract's own map carries, with `supply` always
/// empty.
///
/// Copied from `service.rs`'s test module and widened to carry
/// collateral as well as liabilities. The fixture holds no position
/// for [`filler_signer`]'s key at all, so any test about what the
/// filler *itself* holds — a fill's projection of its own health, an
/// unwind's whole subject — has to build one.
pub(crate) fn positions_entry_xdr(
    account: &str,
    collateral: &[(u32, i128)],
    liabilities: &[(u32, i128)],
) -> String {
    let side = |amounts: &[(u32, i128)]| {
        map(amounts
            .iter()
            .map(|(index, amount)| (ScVal::U32(*index), i128_val(*amount)))
            .collect())
        .expect("positions side")
    };
    let value = map(vec![
        (symbol("collateral").expect("symbol"), side(collateral)),
        (symbol("liabilities").expect("symbol"), side(liabilities)),
        (symbol("supply").expect("symbol"), side(&[])),
    ])
    .expect("positions map");
    let entry = LedgerEntryData::ContractData(ContractDataEntry {
        ext: ExtensionPoint::V0,
        contract: sc_address(POOL).expect("pool"),
        key: sc_vec(vec![
            symbol("Positions").expect("symbol"),
            address(account).expect("account"),
        ])
        .expect("positions key"),
        durability: ContractDataDurability::Persistent,
        val: value,
    });
    to_base64(&entry).expect("positions entry")
}

/// `harness::script_snapshot`'s reserves and oracle reads, with
/// hand-built positions entries instead of the fixture's. Copied from
/// `service.rs`'s test module.
pub(crate) fn script_snapshot_positions(rpc: &ScriptedRpc, positions: &[(&str, String)]) {
    let ledger =
        u32::try_from(mainnet_fixed_v2()["ledger"].as_u64().expect("ledger")).expect("ledger fits");
    script_snapshot_positions_at(rpc, ledger, positions);
}

/// The same, reported at `ledger` rather than at the fixture's own.
///
/// `PoolReader::snapshot` refuses a read whose parts disagree about the
/// ledger, so every answer here carries the one `ledger` — which is what
/// the snapshot's own `ledger` then is. Only that field moves: the
/// entries are the fixture's, so what the reserves accrue to and what the
/// oracle prices are do not depend on it. A test that needs a snapshot
/// the chain has moved past — or one past a ledger something else landed
/// in — says so here.
pub(crate) fn script_snapshot_positions_at(
    rpc: &ScriptedRpc,
    ledger: u32,
    positions: &[(&str, String)],
) {
    let fixture = mainnet_fixed_v2();
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
    for (account, positions_xdr) in positions {
        entries.push(entry(
            &keys::positions(POOL, account).expect("key"),
            positions_xdr,
        ));
    }
    rpc.expect(
        "getLedgerEntries",
        json!({"latestLedger": ledger, "entries": entries}),
    );
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

/// The instance entry of a synthetic pool: the five config fields
/// `decode::pool_instance` reads, and the four storage keys around
/// them. Modelled on `service.rs`'s test module.
pub(crate) fn instance_entry_xdr(pool: &str) -> String {
    let config = map(vec![
        (symbol("bstop_rate").expect("symbol"), ScVal::U32(2_000_000)),
        (symbol("max_positions").expect("symbol"), ScVal::U32(6)),
        (symbol("min_collateral").expect("symbol"), i128_val(0)),
        (
            symbol("oracle").expect("symbol"),
            address(ORACLE).expect("oracle"),
        ),
        (symbol("status").expect("symbol"), ScVal::U32(1)),
    ])
    .expect("config map");
    let ScVal::Map(Some(config)) = config else {
        panic!("map returns a map")
    };
    let storage = stellar_xdr::ScMap::sorted_from(vec![
        (
            symbol("Admin").expect("symbol"),
            address(ADMIN).expect("admin"),
        ),
        (
            symbol("BLNDTkn").expect("symbol"),
            address(BLND).expect("blnd"),
        ),
        (
            symbol("Backstop").expect("symbol"),
            address(POOL_TWO).expect("backstop"),
        ),
        (symbol("Config").expect("symbol"), ScVal::Map(Some(config))),
        (
            symbol("Name").expect("symbol"),
            ScVal::String(stellar_xdr::ScString::try_from(b"Second Pool".to_vec()).expect("name")),
        ),
    ])
    .expect("storage map");
    let instance = ScVal::ContractInstance(stellar_xdr::ScContractInstance {
        executable: stellar_xdr::ContractExecutable::StellarAsset,
        storage: Some(storage),
    });
    contract_entry_xdr(pool, ScVal::LedgerKeyContractInstance, instance)
}

/// A `ContractData` entry of `pool` holding `value`. The scripted RPC
/// answers by the key it is asked for, so the entry's own key field
/// only has to decode.
pub(crate) fn contract_entry_xdr(pool: &str, key: ScVal, value: ScVal) -> String {
    let entry = LedgerEntryData::ContractData(ContractDataEntry {
        ext: ExtensionPoint::V0,
        contract: sc_address(pool).expect("pool"),
        key,
        durability: ContractDataDurability::Persistent,
        val: value,
    });
    to_base64(&entry).expect("entry")
}

/// A [`NotificationChannel`] that keeps what it was handed, so a test can
/// count and read back what a task actually decided to send rather than
/// inspecting a log line.
///
/// `fail` makes every send fail instead of recording it, which is how a
/// test puts a channel failure in front of [`Notifier`]'s own rollback; it
/// is public and atomic so a test can flip it between sends. Every
/// delivery is spawned, so a test asserts on `sent` only after
/// [`Notifier::drain`] has answered.
///
/// [`Notifier`]: crate::notifier::Notifier
/// [`Notifier::drain`]: crate::notifier::Notifier::drain
#[derive(Debug)]
pub(crate) struct RecordingChannel {
    sent: std::sync::Mutex<Vec<Notification>>,
    /// Whether the next send — and every one after it, until a test says
    /// otherwise — fails rather than recording.
    pub(crate) fail: AtomicBool,
}

impl RecordingChannel {
    /// A channel that records every send, or — with `fail` — refuses every
    /// one.
    pub(crate) fn new(fail: bool) -> Self {
        Self {
            sent: std::sync::Mutex::new(Vec::new()),
            fail: AtomicBool::new(fail),
        }
    }

    /// Every notification this channel accepted, in the order it took
    /// them.
    pub(crate) fn sent(&self) -> Vec<Notification> {
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many it accepted.
    pub(crate) fn sent_count(&self) -> usize {
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl NotificationChannel for Arc<RecordingChannel> {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn send<'a>(
        &'a self,
        notification: &'a Notification,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err(NotifyError::Channel("recording channel failed".to_string()));
            }
            self.sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(notification.clone());
            Ok(())
        })
    }
}
