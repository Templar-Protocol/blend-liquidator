//! The `liquidation` scenario: the standard end-to-end run against the
//! local network `scripts/sandbox/up.sh` starts and `scripts/sandbox/deploy.sh`
//! populates — an armed bot creates a borrower's liquidation auction after a
//! price crash, fills it, and unwinds the position it took.
//!
//! `mod sandbox_harness` (`tests/sandbox_harness/mod.rs`) is the machinery
//! every scenario in this tier shares: the standalone-network gate, the
//! spawned bot, the per-run database and every named wait. Its own module
//! doc carries the safety reasoning — the three refusals that keep an armed
//! bot off any network but this sandbox's own, and the "nothing panics
//! through `unwrap`/`expect`" rule every failure below honours through
//! [`sandbox_harness::fail`].
//!
//! It is `#[ignore]`d: `cargo test` must never start Docker containers, and
//! nothing here runs without the sandbox already up. Run it by hand, or from
//! the sandbox workflow:
//!
//! ```text
//! scripts/sandbox/up.sh && scripts/sandbox/deploy.sh
//! cargo test --test liquidation_sandbox -- --ignored --nocapture
//! ```
//!
//! One thing the assertions do not prove, so that nobody reads more into
//! them than is there: in this scenario the fill's own request list repays
//! the bid out of the filler's wallet, so the position it takes over
//! arrives with no liabilities and the unwind that follows runs the
//! withdraw step only — `"unwind planned", "actions":1,
//! "remaining_liabilities":"[]"`. The "no liabilities left" half of a
//! settled position is therefore satisfied by the fill, and the unwind's
//! repay branch has no coverage here. The scenario that gives it some is a
//! filler whose wallet cannot cover the bid — `unwind_repay`, the testnet
//! soak's now that this tier has more than one scenario.

mod sandbox_harness;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blend_liquidator::chain::RpcClient;
use blend_liquidator::config::ChainConfig;
use blend_liquidator::store::Store;

use sandbox_harness::{
    assert_metrics, crash, create_run_database, drop_run_database, fail, fill_budget,
    note_run_database, pools_toml, read_metrics, repo_root, require_standalone_rpc, required,
    sandbox_env, spawn_bot, terminate, wait_for_ready, wait_for_tx_hash, wait_for_unwind,
    BotConfig, CREATION_TIMEOUT, CREATION_TX_HASH, FILL_TX_HASH,
};

/// The whole tier's original scenario: an armed bot creates the auction,
/// fills it and unwinds the position it took, against a network that exists
/// only for this run.
///
/// Ignored on purpose — it needs `scripts/sandbox/up.sh` and
/// `scripts/sandbox/deploy.sh` to have run, and a Postgres at `DATABASE_URL`.
#[tokio::test]
#[ignore = "needs the local sandbox network: scripts/sandbox/up.sh && scripts/sandbox/deploy.sh"]
async fn liquidation() {
    let root = repo_root();
    let env = sandbox_env(&root.join("target/sandbox/sandbox.env"));

    let passphrase = required(&env, "SANDBOX_PASSPHRASE").to_string();
    let pool = required(&env, "SANDBOX_POOL").to_string();
    let xlm = required(&env, "SANDBOX_XLM").to_string();
    let usdc = required(&env, "SANDBOX_USDC").to_string();
    let borrower = required(&env, "SANDBOX_BORROWER").to_string();
    let filler = required(&env, "SANDBOX_FILLER").to_string();
    let rpc_url = required(&env, "SANDBOX_RPC_URL").to_string();

    // Ordered with the two refusals `sandbox_env` just made, and for the
    // same reason: nothing below this line may run against an endpoint
    // whose own answer has not been checked.
    require_standalone_rpc(&rpc_url).await;

    let Ok(maintenance_url) = std::env::var("DATABASE_URL") else {
        panic!("DATABASE_URL is not set — the store tests need it too; see `make db-up`")
    };
    // `sandbox_<unix seconds>`: unique enough for one database, on one
    // server, created by one test run at a time.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let database = format!("sandbox_{stamp}");
    println!("creating the run's database {database}");
    let database_url = create_run_database(&root, &maintenance_url, &database).await;
    // Noted once the database exists, so every failure from here on says it
    // was kept; the success path at the bottom drops it and it is never
    // read again.
    note_run_database(&database);

    let sandbox_dir = root.join("target/sandbox");
    let seed_path = sandbox_dir.join("seed.toml");
    let seed = format!("[accounts]\n\"{pool}\" = [\"{borrower}\"]\n");
    if let Err(error) = std::fs::write(&seed_path, seed) {
        panic!("could not write {}: {error}", seed_path.display());
    }

    let pools = pools_toml(&pool, &xlm, &[usdc.as_str()]);
    let mut bot = spawn_bot(
        &root,
        &env,
        BotConfig {
            dry_run: false,
            filler_secret: Some(required(&env, "SANDBOX_FILLER_SECRET_KEY")),
            pools,
            database_url: database_url.clone(),
            log_name: "bot.log",
            extra_env: vec![("SEED_FILE", seed_path.display().to_string())],
        },
    );

    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => fail(&bot, &format!("could not build an HTTP client: {error}")),
    };
    let store = match Store::connect(&database_url, 4).await {
        Ok(store) => store,
        Err(error) => fail(&bot, &format!("could not connect to {database}: {error}")),
    };
    let chain = ChainConfig {
        network_passphrase: passphrase,
        rpc_url,
        rpc_api_key: None,
        base_fee: 5_000,
        high_fee: 10_000,
        tx_poll_ledgers: 30,
    };
    let rpc = match RpcClient::from_config(&chain) {
        Ok(rpc) => rpc,
        Err(error) => fail(&bot, &format!("could not build an RPC client: {error}")),
    };

    wait_for_ready(&mut bot, &http).await;

    // Only now: the crash is what makes the borrower liquidatable, and a
    // bot that was not yet following the pool would miss the price move
    // that its oracle scan is there to notice.
    println!("crashing XLM's price at {:.1} s", bot.elapsed());
    crash(&bot, &root);

    let creation = wait_for_tx_hash(
        &mut bot,
        &store,
        CREATION_TX_HASH,
        "the auctioneer to create the borrower's liquidation auction",
        CREATION_TIMEOUT,
        &pool,
        &borrower,
    )
    .await;
    // Measured, not assumed: the fill waits on the auction's ledger ramp,
    // so its budget is however long this sandbox takes to close that many
    // ledgers.
    let budget = fill_budget(&mut bot, &rpc).await;
    let fill = wait_for_tx_hash(
        &mut bot,
        &store,
        FILL_TX_HASH,
        "the filler to take that auction",
        budget,
        &pool,
        &borrower,
    )
    .await;
    let collateral = wait_for_unwind(&mut bot, &rpc, &pool, &filler, &xlm).await;

    let metrics = read_metrics(&bot, &http).await;
    assert_metrics(&bot, &metrics);

    terminate(&mut bot).await;

    println!(
        "done in {:.1} s: creation {creation}, fill {fill}, {collateral} stroops of XLM \
         collateral left",
        bot.elapsed()
    );

    // Last, and only here: everything above either passed or panicked, so
    // reaching this line is what "the run succeeded" means, and a database
    // nobody will read is a database worth not keeping.
    store.pool().close().await;
    drop_run_database(&root, &maintenance_url, &database).await;
}
