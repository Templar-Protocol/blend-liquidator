//! Prints the evidence a soak run has produced for one pool: the tracked-user
//! count, the open auctions, every `creations` and `fills` row with its
//! `dry_run` and `tx_hash` columns, and — for each account given — its
//! on-chain position, the way `pool_snapshot` prints one. Read-only: it
//! opens the store without migrating it, makes no write, and takes no key.
//! This is what the soak runbook's evidence sections are filled from.
//!
//! ```text
//! DATABASE_URL=postgres://liquidator:liquidator@127.0.0.1:55432/testnet_armed \
//! RPC_URL=https://soroban-testnet.stellar.org \
//!   cargo run --example soak_report -- <pool> [account...]
//! ```
//!
//! `RPC_URL` is needed only when an account is given: the store sections
//! read nothing from chain. `RPC_API_KEY_HEADER` and `RPC_API_KEY` are
//! honoured together, as `pool_snapshot` does.
//!
//! There is no network gate here, unlike `scripts/testnet/`: nothing asks
//! the node which network it is, so the positions printed are whatever the
//! node `RPC_URL` names answers for. That is acceptable for a tool that
//! holds no key and sends nothing, and it is why the command above names
//! the RPC explicitly.
//!
//! `creations` and `fills` have no read method on [`blend_liquidator::store::Store`]
//! — the bot only ever writes them — so this example reads both with
//! runtime `sqlx::query_as`, never the compile-time `sqlx::query!`/
//! `query_as!` macros: those are checked against the committed `.sqlx/`
//! offline metadata at compile time, which covers only `--lib --bins`
//! (`make sqlx-prepare`'s own scope), never `examples/`. A `numeric` or
//! `timestamptz` column is cast to `::text` in the query itself, exactly as
//! `src/store.rs` does for every `i128` amount, so decoding needs no
//! `rust_decimal`/`chrono` feature this crate does not otherwise take on.

use std::error::Error;

use blend_liquidator::chain::pool::PoolReader;
use blend_liquidator::chain::rpc::RpcClient;
use blend_liquidator::store::Store;

/// One `creations` row, read at runtime — see the module doc.
#[derive(sqlx::FromRow)]
struct CreationRow {
    id: i64,
    tx_hash: Option<String>,
    kind: String,
    account: String,
    percent: Option<i16>,
    bid: serde_json::Value,
    lot: serde_json::Value,
    ledger: i64,
    dry_run: bool,
    created_at: String,
}

/// One `fills` row, read at runtime — see the module doc.
#[derive(sqlx::FromRow)]
struct FillRow {
    id: i64,
    tx_hash: Option<String>,
    account: String,
    auction_type: i16,
    fill_ledger: i64,
    percent: i16,
    bid: serde_json::Value,
    lot: serde_json::Value,
    bid_value: String,
    lot_value: String,
    est_profit: String,
    dry_run: bool,
    created_at: String,
}

/// The contract's auction-type discriminant, human-readable. The `fills`
/// table's own `CHECK` bounds it to 0..=2; anything else prints as itself
/// rather than failing a report over one unexpected row.
fn auction_type_name(code: i16) -> &'static str {
    match code {
        0 => "user_liquidation",
        1 => "bad_debt",
        2 => "interest",
        _ => "unknown",
    }
}

/// The tracked-user count and every open auction.
async fn print_store_state(store: &Store, pool: &str) -> Result<(), Box<dyn Error>> {
    let tracked = store.count_users(pool).await?;
    println!("tracked users: {tracked}");

    let open = store.open_auctions(pool).await?;
    println!("open auctions: {}", open.len());
    for auction in &open {
        println!(
            "  account {} type {:?} start_ledger {} fill_ledger {:?} percent {:?} bid {:?} lot {:?}",
            auction.account,
            auction.auction_type,
            auction.start_ledger,
            auction.fill_ledger,
            auction.percent.map(|percent| percent.get()),
            auction.bid,
            auction.lot,
        );
    }
    Ok(())
}

/// Every `creations` row for `pool`, newest first.
async fn print_creations(store: &Store, pool: &str) -> Result<(), Box<dyn Error>> {
    let creations: Vec<CreationRow> = sqlx::query_as(
        "SELECT id, tx_hash, kind, account, percent, bid, lot, ledger, dry_run, \
         created_at::text AS created_at \
         FROM creations WHERE pool = $1 ORDER BY created_at DESC",
    )
    .bind(pool)
    .fetch_all(store.pool())
    .await?;
    println!("creations: {}", creations.len());
    for row in &creations {
        println!(
            "  #{} {} account {} percent {:?} bid {} lot {} ledger {} dry_run {} tx_hash {:?} at {}",
            row.id,
            row.kind,
            row.account,
            row.percent,
            row.bid,
            row.lot,
            row.ledger,
            row.dry_run,
            row.tx_hash,
            row.created_at,
        );
    }
    Ok(())
}

/// Every `fills` row for `pool`, newest first.
async fn print_fills(store: &Store, pool: &str) -> Result<(), Box<dyn Error>> {
    let fills: Vec<FillRow> = sqlx::query_as(
        "SELECT id, tx_hash, account, auction_type, fill_ledger, percent, bid, lot, \
         bid_value::text AS bid_value, lot_value::text AS lot_value, \
         est_profit::text AS est_profit, dry_run, created_at::text AS created_at \
         FROM fills WHERE pool = $1 ORDER BY created_at DESC",
    )
    .bind(pool)
    .fetch_all(store.pool())
    .await?;
    println!("fills: {}", fills.len());
    for row in &fills {
        println!(
            "  #{} account {} auction_type {} fill_ledger {} percent {} bid {} lot {} \
             bid_value {} lot_value {} est_profit {} dry_run {} tx_hash {:?} at {}",
            row.id,
            row.account,
            auction_type_name(row.auction_type),
            row.fill_ledger,
            row.percent,
            row.bid,
            row.lot,
            row.bid_value,
            row.lot_value,
            row.est_profit,
            row.dry_run,
            row.tx_hash,
            row.created_at,
        );
    }
    Ok(())
}

/// Each given account's on-chain position, projected the way `pool_snapshot`
/// projects one.
async fn print_positions(
    rpc_url: &str,
    api_key: Option<(&str, &str)>,
    pool: &str,
    accounts: &[&str],
) -> Result<(), Box<dyn Error>> {
    let client = RpcClient::new(rpc_url, api_key)?;
    let snapshot = PoolReader::new(&client, pool).snapshot(accounts).await?;
    let latest = client.latest_ledger().await?;
    println!(
        "on-chain position at ledger {} (latest {} closed at {})",
        snapshot.ledger, latest.sequence, latest.close_time
    );
    for account in accounts {
        match snapshot.position_data(account, latest.close_time)? {
            Some(data) => println!(
                "  {account}: collateral {} liabilities {} health factor {:?}",
                data.collateral_base,
                data.liability_base,
                data.health_factor()?
            ),
            None => println!("  {account}: no positions"),
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    let pool = arguments
        .get(1)
        .ok_or("usage: soak_report <pool> [account...]")?
        .clone();
    let accounts: Vec<&str> = arguments.iter().skip(2).map(String::as_str).collect();

    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is required")?;
    // Read up front, before the store is opened, so a report that names an
    // account fails on a missing RPC_URL before printing anything, as it
    // always has; one that names none never needs it.
    let rpc_url = if accounts.is_empty() {
        None
    } else {
        Some(std::env::var("RPC_URL").map_err(|_| "RPC_URL is required when an account is given")?)
    };
    let header = std::env::var("RPC_API_KEY_HEADER")
        .ok()
        .filter(|value| !value.is_empty());
    let key = std::env::var("RPC_API_KEY")
        .ok()
        .filter(|value| !value.is_empty());
    let api_key = match (&header, &key) {
        (Some(header), Some(key)) => Some((header.as_str(), key.as_str())),
        (None, None) => None,
        _ => return Err("RPC_API_KEY_HEADER and RPC_API_KEY come together".into()),
    };

    // No `Store::migrate`: a report is read-only, and a soak database is
    // migrated by the bot that owns it.
    let store = Store::connect(&database_url, 2).await?;

    println!("pool {pool}");
    print_store_state(&store, &pool).await?;
    print_creations(&store, &pool).await?;
    print_fills(&store, &pool).await?;

    match rpc_url {
        Some(rpc_url) => print_positions(&rpc_url, api_key, &pool, &accounts).await,
        None => Ok(()),
    }
}
