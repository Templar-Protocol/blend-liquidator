//! Refreshes a pool fixture from a live Soroban RPC.
//!
//! ```text
//! cargo run --example capture_fixture -- \
//!   https://mainnet.sorobanrpc.com <pool> tests/fixtures/mainnet-fixed-v2.json [user...]
//! ```
//!
//! `curl` is the transport, so refreshing a fixture by hand costs the crate
//! no HTTP dependency.
//!
//! The fixture is only useful if everything in it describes one ledger: the
//! tests accrue the stored entries to that ledger's close time and compare
//! against the contract's own `get_reserve`. So each attempt reads the
//! entries, runs every simulation, reads the entries again, and keeps the
//! result only if the two reads are byte-identical and every simulation
//! reported the same ledger. Otherwise it retries.
//!
//! The accrual target itself is read from those `get_reserve` returns, not
//! from a separate RPC call: the pool contract's `Reserve::load` always
//! stamps `data.last_time` with the timestamp of the ledger it ran in (it
//! short-circuits when they already agree, and sets it in every other
//! branch), so each captured reserve already carries the exact second the
//! fixture's tests must accrue to. A later, independent call — `getHealth`,
//! say — has no such guarantee: mainnet closes a ledger roughly every five
//! seconds, and the round trip to ask again almost always lands on a newer
//! one than the simulations just agreed on.

use std::error::Error;
use std::process::Command;

use blend_liquidator::chain::xdr::encode::{
    address, invoke_contract_op, simulation_envelope, stellar_asset, symbol, to_base64,
};
use blend_liquidator::chain::xdr::{decode, from_base64, keys};
use serde_json::{json, Value};

type Fallible<T> = Result<T, Box<dyn Error>>;
/// Ledger entries as `(key base64, entry base64)` pairs.
type Entries = Vec<(String, String)>;

const ATTEMPTS: usize = 10;
/// Topics whose recent events go into the fixture, with their topic arity.
const EVENT_TOPICS: [(&str, usize); 8] = [
    ("supply", 3),
    ("supply_collateral", 3),
    ("borrow", 3),
    ("repay", 3),
    ("withdraw_collateral", 3),
    ("new_auction", 3),
    ("fill_auction", 3),
    ("delete_auction", 3),
];
/// At most this many of each topic, to keep the fixture readable.
const EVENTS_PER_TOPIC: usize = 3;

fn rpc(url: &str, method: &str, params: &Value) -> Fallible<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let output = Command::new("curl")
        .args([
            "-s",
            "-m",
            "30",
            "-X",
            "POST",
            url,
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "curl failed for {method}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let response: Value = serde_json::from_slice(&output.stdout)?;
    if let Some(error) = response.get("error") {
        return Err(format!("{method}: {error}").into());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| format!("{method}: no result").into())
}

/// Reads ledger entries, returning `(latestLedger, [(key, xdr)])`.
fn ledger_entries(url: &str, key_base64: &[String]) -> Fallible<(u64, Entries)> {
    let result = rpc(url, "getLedgerEntries", &json!({ "keys": key_base64 }))?;
    let ledger = result["latestLedger"]
        .as_u64()
        .ok_or("latestLedger missing")?;
    let mut entries = Vec::new();
    for entry in result["entries"].as_array().unwrap_or(&Vec::new()) {
        let key = entry["key"]
            .as_str()
            .ok_or("entry key missing")?
            .to_string();
        let xdr = entry["xdr"]
            .as_str()
            .ok_or("entry xdr missing")?
            .to_string();
        entries.push((key, xdr));
    }
    Ok((ledger, entries))
}

/// Simulates a read-only call, returning `(latestLedger, return value base64)`.
fn simulate(
    url: &str,
    contract: &str,
    function: &str,
    args: Vec<stellar_xdr::ScVal>,
) -> Fallible<(u64, String)> {
    let envelope = simulation_envelope(invoke_contract_op(contract, function, args)?)?;
    let result = rpc(
        url,
        "simulateTransaction",
        &json!({ "transaction": to_base64(&envelope)? }),
    )?;
    if let Some(error) = result.get("error") {
        return Err(format!("simulating {function}: {error}").into());
    }
    let ledger = result["latestLedger"]
        .as_u64()
        .ok_or("latestLedger missing")?;
    let value = result["results"][0]["xdr"]
        .as_str()
        .ok_or("simulation returned no value")?
        .to_string();
    Ok((ledger, value))
}

/// The pool's oracle and reserve list, from its instance and `ResList`.
fn pool_shape(url: &str, pool: &str) -> Fallible<(Entries, String, Vec<String>)> {
    let instance_key = to_base64(&keys::instance(pool)?)?;
    let reserve_list_key = to_base64(&keys::reserve_list(pool)?)?;
    let (_, entries) = ledger_entries(url, &[instance_key.clone(), reserve_list_key.clone()])?;
    // The RPC does not promise to return entries in the order they were
    // asked for, so every lookup goes through the key.
    let instance = decode::pool_instance(&decode::entry_from_base64(entry_for(
        &entries,
        &instance_key,
    )?)?)?;
    let assets = decode::reserve_list(&decode::entry_from_base64(entry_for(
        &entries,
        &reserve_list_key,
    )?)?)?;
    Ok((entries, instance.config.oracle, assets))
}

/// Every ledger entry the fixture holds, in a stable order.
fn all_entry_keys(pool: &str, assets: &[String], users: &[String]) -> Fallible<Vec<String>> {
    let mut key_base64 = vec![
        to_base64(&keys::instance(pool)?)?,
        to_base64(&keys::reserve_list(pool)?)?,
    ];
    for asset in assets {
        key_base64.push(to_base64(&keys::reserve_config(pool, asset)?)?);
        key_base64.push(to_base64(&keys::reserve_data(pool, asset)?)?);
    }
    for user in users {
        key_base64.push(to_base64(&keys::positions(pool, user)?)?);
    }
    Ok(key_base64)
}

/// Finds an entry by its key, since the RPC may reorder or omit entries.
fn entry_for<'a>(entries: &'a [(String, String)], key: &str) -> Fallible<&'a str> {
    entries
        .iter()
        .find(|(entry_key, _)| entry_key == key)
        .map(|(_, xdr)| xdr.as_str())
        .ok_or_else(|| format!("the ledger has no entry for key {key}").into())
}

fn recent_events(url: &str, pool: &str, oldest: u64, ledger: u64) -> Fallible<Vec<Value>> {
    let start = oldest.max(ledger.saturating_sub(120_000));
    let mut events = Vec::new();
    for (name, arity) in EVENT_TOPICS {
        let mut topics = vec![to_base64(&symbol(name)?)?];
        topics.extend(std::iter::repeat_n("*".to_string(), arity - 1));
        let result = rpc(
            url,
            "getEvents",
            &json!({
                "startLedger": start,
                "filters": [{"type": "contract", "contractIds": [pool], "topics": [topics]}],
                "pagination": {"limit": 20}
            }),
        )?;
        let found = result["events"].as_array().cloned().unwrap_or_default();
        println!("  {name}: {} event(s) since ledger {start}", found.len());
        events.extend(found.into_iter().take(EVENTS_PER_TOPIC));
    }
    Ok(events)
}

/// One capture attempt. Returns `Ok(None)` when the ledger moved under it.
fn attempt(url: &str, pool: &str, users: &[String]) -> Fallible<Option<Value>> {
    let (_, oracle, assets) = pool_shape(url, pool)?;
    let key_base64 = all_entry_keys(pool, &assets, users)?;
    let (_, first_pass) = ledger_entries(url, &key_base64)?;

    let mut ledgers = Vec::new();
    let (ledger, oracle_decimals) = simulate(url, &oracle, "decimals", Vec::new())?;
    ledgers.push(ledger);

    let mut reserves = Vec::new();
    // Each `get_reserve` return already carries the ledger's own timestamp
    // in `data.last_time` — see the module doc comment for why that, and
    // not a separate `getHealth` call, is the fixture's accrual target.
    let mut accrual_times = Vec::new();
    for asset in &assets {
        let (get_reserve_ledger, get_reserve) =
            simulate(url, pool, "get_reserve", vec![address(asset)?])?;
        let (price_ledger, lastprice) =
            simulate(url, &oracle, "lastprice", vec![stellar_asset(asset)?])?;
        ledgers.push(get_reserve_ledger);
        ledgers.push(price_ledger);
        let reserve = decode::reserve_value(&from_base64(&get_reserve)?)?;
        accrual_times.push(reserve.data.last_time);
        reserves.push(json!({
            "asset": asset,
            "config_entry_xdr": entry_for(&first_pass, &to_base64(&keys::reserve_config(pool, asset)?)?)?,
            "data_entry_xdr": entry_for(&first_pass, &to_base64(&keys::reserve_data(pool, asset)?)?)?,
            "get_reserve_return_xdr": get_reserve,
            "lastprice_return_xdr": lastprice,
        }));
    }

    let mut user_entries = Vec::new();
    for user in users {
        let (positions_ledger, get_positions) =
            simulate(url, pool, "get_positions", vec![address(user)?])?;
        ledgers.push(positions_ledger);
        user_entries.push(json!({
            "account": user,
            "positions_entry_xdr": entry_for(&first_pass, &to_base64(&keys::positions(pool, user)?)?)?,
            "get_positions_return_xdr": get_positions,
        }));
    }

    if ledgers.iter().any(|other| *other != ledger) {
        println!("  the ledger moved during the simulations ({ledgers:?})");
        return Ok(None);
    }
    let (_, second_pass) = ledger_entries(url, &key_base64)?;
    if second_pass != first_pass {
        println!("  a ledger entry changed between passes");
        return Ok(None);
    }

    // The fixture's accrual target: every reserve's own `last_time`, which
    // must agree since they all came from simulations against one ledger.
    let close_time = *accrual_times
        .first()
        .ok_or("the pool has no reserves; there is no accrual target")?;
    if accrual_times.iter().any(|&other| other != close_time) {
        println!("  reserves disagree on their accrual time ({accrual_times:?})");
        return Ok(None);
    }

    let health = rpc(url, "getHealth", &json!({}))?;
    let oldest = health["oldestLedger"]
        .as_u64()
        .ok_or("oldestLedger missing")?;
    let events = recent_events(url, pool, oldest, ledger)?;

    Ok(Some(json!({
        "rpc_url": url,
        "pool": pool,
        "ledger": ledger,
        "ledger_close_time": close_time,
        "instance_entry_xdr": entry_for(&first_pass, &to_base64(&keys::instance(pool)?)?)?,
        "res_list_entry_xdr": entry_for(&first_pass, &to_base64(&keys::reserve_list(pool)?)?)?,
        "oracle": oracle,
        "oracle_decimals_return_xdr": oracle_decimals,
        "reserves": reserves,
        "users": user_entries,
        "events": events,
    })))
}

fn main() -> Fallible<()> {
    let arguments: Vec<String> = std::env::args().collect();
    let usage = "usage: capture_fixture <rpc-url> <pool> <out.json> [user...]";
    let url = arguments.get(1).ok_or(usage)?;
    let pool = arguments.get(2).ok_or(usage)?;
    let out = arguments.get(3).ok_or(usage)?;
    let users: Vec<String> = arguments.iter().skip(4).cloned().collect();

    for number in 1..=ATTEMPTS {
        println!("attempt {number}:");
        if let Some(fixture) = attempt(url, pool, &users)? {
            std::fs::write(out, serde_json::to_string_pretty(&fixture)?)?;
            println!(
                "wrote {out} at ledger {} (close time {})",
                fixture["ledger"], fixture["ledger_close_time"]
            );
            return Ok(());
        }
    }
    Err(
        format!("no consistent snapshot after {ATTEMPTS} attempts; the pool may be too busy")
            .into(),
    )
}
