//! Prints a pool's reserves and, for each user given, the user's health
//! factor, read from a live RPC through the real client. The phase's
//! dry-run demonstration: nothing is signed or sent.
//!
//! ```text
//! RPC_URL=https://mainnet.sorobanrpc.com \
//!   cargo run --example pool_snapshot -- <pool> [user...]
//! ```
//!
//! `RPC_API_KEY_HEADER` and `RPC_API_KEY` are honoured together. The health
//! factor is computed at the latest ledger's close time, read after the
//! snapshot, so it is at or after the snapshot's ledger: that is the
//! accrual the contract would apply to a call landing now.

use std::error::Error;

use blend_liquidator::chain::pool::PoolReader;
use blend_liquidator::chain::rpc::RpcClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    let pool = arguments
        .get(1)
        .ok_or("usage: pool_snapshot <pool> [user...]")?;
    let users: Vec<&str> = arguments.iter().skip(2).map(String::as_str).collect();
    let url = std::env::var("RPC_URL").map_err(|_| "RPC_URL is required")?;
    let header = std::env::var("RPC_API_KEY_HEADER").ok();
    let key = std::env::var("RPC_API_KEY").ok();
    let api_key = match (&header, &key) {
        (Some(header), Some(key)) => Some((header.as_str(), key.as_str())),
        (None, None) => None,
        _ => return Err("RPC_API_KEY_HEADER and RPC_API_KEY come together".into()),
    };

    let client = RpcClient::new(&url, api_key)?;
    let snapshot = PoolReader::new(&client, pool).snapshot(&users).await?;
    let latest = client.latest_ledger().await?;

    println!(
        "pool {pool} at ledger {} (latest {} closed at {})",
        snapshot.ledger, latest.sequence, latest.close_time
    );
    println!(
        "  status {:?}, backstop rate {}, oracle {} with {} decimals",
        snapshot.instance.config.status,
        snapshot.instance.config.bstop_rate,
        snapshot.instance.config.oracle,
        snapshot.prices.decimals()
    );
    for reserve in snapshot.reserves.values() {
        let price = snapshot
            .prices
            .price(&reserve.asset)
            .map_or_else(|_| "none".to_string(), |price| price.to_string());
        println!(
            "  reserve {} index {} utilisation {} b_rate {} d_rate {} price {price}",
            reserve.asset,
            reserve.config.index,
            reserve.utilization()?,
            reserve.data.b_rate,
            reserve.data.d_rate
        );
    }
    for user in users {
        match snapshot.position_data(user, latest.close_time)? {
            Some(data) => println!(
                "  user {user}: collateral {} liabilities {} health factor {:?}",
                data.collateral_base,
                data.liability_base,
                data.health_factor()?
            ),
            None => println!("  user {user}: no positions"),
        }
    }
    Ok(())
}
