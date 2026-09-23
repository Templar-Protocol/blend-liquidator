//! Finds accounts worth tracking in a pool's recent history, straight from
//! its event log. Read-only: no key, no store, nothing signed.
//!
//! ```text
//! RPC_URL=https://soroban-testnet.stellar.org \
//!   cargo run --example scan_borrowers -- <pool> [ledgers]
//! ```
//!
//! `ledgers` defaults to 17280, about 24 hours at testnet's ~5 s ledger
//! close. Pages `getEvents` from `latest - ledgers` to chain head the way
//! [`blend_liquidator::ledger::LedgerPoller`] does — a start ledger for the
//! first page, the returned cursor for every page after, stopping on a
//! short page — and decodes every event through `chain::xdr::events`,
//! collecting every account any variant names
//! (`PoolEvent::affected_accounts`). A start ledger older than what the RPC
//! retains is narrowed to the retained window's edge rather than failing:
//! this is a lookup, not a poller with a cursor to resume, so there is
//! nothing to lose by asking for less.
//!
//! `RPC_API_KEY_HEADER` and `RPC_API_KEY` are honoured together, as
//! `pool_snapshot` does.
//!
//! There is no network gate here, unlike `scripts/testnet/`: nothing asks
//! the node which network it is, so the accounts found are whatever the
//! node `RPC_URL` names answers for. That is acceptable for a tool that
//! holds no key and sends nothing, and it is why the command above names
//! the RPC explicitly.
//!
//! Prints the accounts one per line, then a ready-to-paste `[accounts]`
//! block in `SEED_FILE`'s TOML shape (see `seed.example.toml`), keyed by the
//! pool address.

use std::collections::BTreeSet;
use std::error::Error;

use blend_liquidator::chain::rpc::{EventQuery, RpcClient};
use blend_liquidator::chain::xdr::decode_pool_event;
use blend_liquidator::chain::ChainError;

/// About 24 hours at testnet's ~5 s ledger close.
const DEFAULT_LEDGERS: u32 = 17_280;
/// Events per `getEvents` page — the same figure `PollerConfig::new` uses.
const PAGE_LIMIT: u32 = 200;
/// A page count beyond which paging is presumed broken rather than merely
/// long, the same reasoning as `ledger::MAX_PAGES`.
const MAX_PAGES: usize = 2_000;

/// Pages `getEvents` for `pool` from `start` to chain head, decoding every
/// event and collecting every account any variant names.
async fn scan(client: &RpcClient, pool: &str, start: u32) -> Result<BTreeSet<String>, ChainError> {
    let mut accounts = BTreeSet::new();
    let mut cursor: Option<String> = None;
    let mut pages: usize = 0;
    loop {
        pages += 1;
        let page = client
            .events(&EventQuery {
                start_ledger: cursor.is_none().then_some(start),
                cursor: cursor.as_deref(),
                contract_ids: &[pool],
                limit: PAGE_LIMIT,
            })
            .await?;
        let count = page.events.len();
        for event in &page.events {
            if event.contract_id != pool || !event.in_successful_contract_call {
                continue;
            }
            match decode_pool_event(&event.topics, &event.value) {
                Ok(Some(decoded)) => {
                    for account in decoded.affected_accounts() {
                        accounts.insert(account.to_string());
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    println!(
                        "  (skipping an event at ledger {} that did not decode: {error})",
                        event.ledger
                    );
                }
            }
        }
        // The two ways this loop ends are not the same answer, so they do not
        // share an exit. A short page is the range drained: every event in it
        // was read. The page cap is a scan that stopped early, and a list
        // printed from it names fewer accounts than the range holds — which
        // reads exactly like "this pool has no other borrowers" unless it
        // says otherwise. `src/ledger.rs`'s own paging draws the same
        // distinction, and for the same reason.
        if count < PAGE_LIMIT as usize {
            return Ok(accounts);
        }
        if pages >= MAX_PAGES {
            println!(
                "  (stopped at the {MAX_PAGES}-page cap with {} accounts so far — the range was \
                 NOT fully scanned, and this list is incomplete; re-run with a smaller `ledgers` \
                 and combine the results)",
                accounts.len()
            );
            return Ok(accounts);
        }
        match page.cursor {
            Some(next) if Some(next.as_str()) != cursor.as_deref() => cursor = Some(next),
            _ => {
                println!("  (paging stalled after {pages} pages; stopping)");
                return Ok(accounts);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    let pool = arguments
        .get(1)
        .ok_or("usage: scan_borrowers <pool> [ledgers]")?
        .clone();
    let ledgers: u32 = match arguments.get(2) {
        Some(value) => value
            .parse()
            .map_err(|_| "ledgers must be a non-negative integer")?,
        None => DEFAULT_LEDGERS,
    };
    let url = std::env::var("RPC_URL").map_err(|_| "RPC_URL is required")?;
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

    let client = RpcClient::new(&url, api_key)?;
    let health = client.health().await?;
    let mut start = health.latest_ledger.saturating_sub(ledgers).max(1);
    if start < health.oldest_ledger {
        println!(
            "requested start ledger {start} is older than the RPC's retained window \
             (oldest {}); narrowing the range",
            health.oldest_ledger
        );
        start = health.oldest_ledger;
    }
    println!(
        "scanning pool {pool} from ledger {start} to {} (oldest retained {})",
        health.latest_ledger, health.oldest_ledger
    );

    let accounts = match scan(&client, &pool, start).await {
        Ok(accounts) => accounts,
        Err(ChainError::Rpc { code, message }) => {
            // The chain may have moved past `health`'s answer between that
            // call and the first page, or the RPC may simply refuse a start
            // ledger `health` itself called retained. Either way: narrow to
            // a freshly read retained edge and try once more rather than
            // failing outright.
            // Never earlier than the start already chosen: a refusal of any
            // kind (a later page, a rate limit) must not turn a narrow scan
            // into one over the whole retained window, which is both far
            // longer and a list of accounts outside the requested range.
            let health = client.health().await?;
            let retry_start = start.max(health.oldest_ledger);
            println!(
                "getEvents refused the scan ({code}: {message}); retrying once from ledger \
                 {retry_start}, the later of the requested start and the retained window's edge"
            );
            scan(&client, &pool, retry_start).await?
        }
        Err(error) => return Err(error.into()),
    };

    println!();
    println!("{} accounts found", accounts.len());
    for account in &accounts {
        println!("{account}");
    }

    println!();
    println!("[accounts]");
    println!("\"{pool}\" = [");
    for account in &accounts {
        println!("  \"{account}\",");
    }
    println!("]");

    Ok(())
}
