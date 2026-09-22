//! The sandbox tier's scenario tests, one `#[tokio::test]` per scenario,
//! run against the local network `scripts/sandbox/up.sh` starts and
//! `scripts/sandbox/deploy.sh` populates. `make sandbox-test` always names
//! this one binary (`--test liquidation_sandbox`) and picks the scenario
//! with `--exact $(SANDBOX_SCENARIO)`, which is why a new scenario is a new
//! function here rather than a new test target.
//!
//! - [`liquidation`]: the standard end-to-end run — an armed bot creates a
//!   borrower's liquidation auction after a price crash, fills it, and
//!   unwinds the position it took.
//! - [`check_config`]: `RUN_MODE=check-config` against the same deploy,
//!   proving its six cases' exit codes and messages and that none of them
//!   sends a transaction or migrates the database.
//!
//! `mod sandbox_harness` (`tests/sandbox_harness/mod.rs`) is the machinery
//! every scenario in this tier shares: the standalone-network gate, the
//! spawned bot, the per-run database and every named wait. Its own module
//! doc carries the safety reasoning — the three refusals that keep an armed
//! bot off any network but this sandbox's own, and the "nothing panics
//! through `unwrap`/`expect`" rule every failure below honours through
//! [`sandbox_harness::fail`] (or, for `check_config`'s short-lived runs,
//! [`sandbox_harness::fail_check`]).
//!
//! Every scenario here is `#[ignore]`d: `cargo test` must never start
//! Docker containers, and nothing here runs without the sandbox already up.
//! Run one by hand, or from the sandbox workflow:
//!
//! ```text
//! scripts/sandbox/up.sh && SANDBOX_SCENARIO=liquidation scripts/sandbox/deploy.sh
//! cargo test --test liquidation_sandbox -- --ignored --exact --nocapture liquidation
//! ```
//!
//! One thing `liquidation`'s assertions do not prove, so that nobody reads
//! more into them than is there: in that scenario the fill's own request
//! list repays the bid out of the filler's wallet, so the position it takes
//! over arrives with no liabilities and the unwind that follows runs the
//! withdraw step only — `"unwind planned", "actions":1,
//! "remaining_liabilities":"[]"`. The "no liabilities left" half of a
//! settled position is therefore satisfied by the fill, and the unwind's
//! repay branch has no coverage here. The scenario that gives it some is a
//! filler whose wallet cannot cover the bid — `unwind_repay`, the testnet
//! soak's now that this tier has more than one scenario.

mod sandbox_harness;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blend_liquidator::chain::RpcClient;
use blend_liquidator::config::ChainConfig;
use blend_liquidator::store::Store;
use sqlx::postgres::PgPool;

use sandbox_harness::{
    assert_metrics, crash, create_run_database, create_run_database_unmigrated, drop_run_database,
    fail, fail_check, fill_budget, note_run_database, pools_toml, read_metrics, repo_root,
    require_standalone_rpc, required, run_check_config, sandbox_env, spawn_bot, terminate,
    wait_for_ready, wait_for_tx_hash, wait_for_unwind, with_database, BotConfig, CREATION_TIMEOUT,
    CREATION_TX_HASH, FILL_TX_HASH,
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
    let env = sandbox_env(&root.join("target/sandbox/sandbox.env"), "liquidation");

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

/// Asserts one `check-config` case's exit code and that its combined
/// stdout/stderr contains `expected_text`, then prints both for the
/// report.
///
/// `expected_text` is always a stable substring, never a whole line — the
/// same rule the module doc above draws from `src/service.rs` and
/// `src/config.rs`'s exact wording, which is where every one of this
/// scenario's six cases gets the text it matches.
fn expect_case(
    name: &str,
    status: std::process::ExitStatus,
    output: &str,
    expected_code: i32,
    expected_text: &str,
) {
    if status.code() != Some(expected_code) {
        fail_check(
            output,
            &format!("case {name}: exited {status}, expected code {expected_code}"),
        );
    }
    let Some(line) = output.lines().find(|line| line.contains(expected_text)) else {
        fail_check(
            output,
            &format!(
                "case {name}: expected the output to contain {expected_text:?}, but it did not"
            ),
        );
    };
    println!("case {name}: exit {expected_code}, matched: {line}");
}

/// The `check_config` scenario: `RUN_MODE=check-config` against the
/// standard deploy, proving the deploy-smoke-test contract spec §10
/// promises — the right exit code and the right warning or error text for
/// each of six cases — and that none of them sends a transaction or
/// migrates the database it is pointed at.
///
/// Six short-lived binary runs rather than one long-running [`Bot`]:
/// `check-config` validates and exits on its own, so
/// [`sandbox_harness::run_check_config`] is its own spawn-and-wait-for-exit
/// rather than [`spawn_bot`]'s spawn-and-wait-for-`/healthz`, and every
/// failure below goes through [`fail_check`] over a case's captured output
/// rather than [`fail`] over a log file — there is no [`Bot`] here for
/// `fail` to tail.
///
/// The database this run creates is deliberately **not** migrated
/// ([`create_run_database_unmigrated`]): the whole point of cases (a)
/// through (e) is that `check-config` leaves it exactly that way, which a
/// migrated fixture could never prove. Case (f) points at a sixth name on
/// the same server that this test never creates at all, for the one case
/// that needs a `DATABASE_URL` naming nothing.
///
/// Ignored on purpose — it needs `scripts/sandbox/up.sh` and
/// `SANDBOX_SCENARIO=check_config scripts/sandbox/deploy.sh` to have run,
/// and a Postgres at `DATABASE_URL`.
// Six cases, each its own `run_check_config` call and `expect_case`
// assertion, in the one order the brief lists them: wiring, not logic —
// same reason `Service::run` carries the same allow.
#[allow(clippy::too_many_lines)]
#[tokio::test]
#[ignore = "needs the local sandbox network: scripts/sandbox/up.sh && scripts/sandbox/deploy.sh"]
async fn check_config() {
    let root = repo_root();
    let env = sandbox_env(&root.join("target/sandbox/sandbox.env"), "check_config");

    let passphrase = required(&env, "SANDBOX_PASSPHRASE").to_string();
    let pool = required(&env, "SANDBOX_POOL").to_string();
    let xlm = required(&env, "SANDBOX_XLM").to_string();
    let usdc = required(&env, "SANDBOX_USDC").to_string();
    let blnd = required(&env, "SANDBOX_BLND").to_string();
    let filler = required(&env, "SANDBOX_FILLER").to_string();
    let filler_secret = required(&env, "SANDBOX_FILLER_SECRET_KEY").to_string();
    let rpc_url = required(&env, "SANDBOX_RPC_URL").to_string();

    // Ordered with the two refusals `sandbox_env` just made, and for the
    // same reason `liquidation` keeps it first: nothing below this line
    // may run against an endpoint whose own answer has not been checked.
    require_standalone_rpc(&rpc_url).await;

    let Ok(maintenance_url) = std::env::var("DATABASE_URL") else {
        panic!("DATABASE_URL is not set — the store tests need it too; see `make db-up`")
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let database = format!("sandbox_{stamp}");
    println!("creating the run's database {database}, created but not migrated");
    let database_url = create_run_database_unmigrated(&root, &maintenance_url, &database).await;
    // Noted once the database exists, so a failure from here on says it
    // was kept; the success path at the bottom drops it and it is never
    // read again.
    note_run_database(&database);
    // Never created: case (f) is the one case that needs a `DATABASE_URL`
    // naming nothing, on the same server as every other case's own.
    let absent_url = with_database(&maintenance_url, &format!("sandbox_{stamp}_absent"));

    let chain = ChainConfig {
        network_passphrase: passphrase,
        rpc_url: rpc_url.clone(),
        rpc_api_key: None,
        base_fee: 5_000,
        high_fee: 10_000,
        tx_poll_ledgers: 30,
    };
    let rpc = match RpcClient::from_config(&chain) {
        Ok(rpc) => rpc,
        Err(error) => fail_check("", &format!("could not build an RPC client: {error}")),
    };

    // Read before the first case and again after the last: every one of
    // the six below is `check-config`, which reads and pings but never
    // signs or sends, so nothing between these two reads may move it.
    let sequence_before = match rpc.account(&filler).await {
        Ok(account) => account.sequence,
        Err(error) => fail_check(
            "",
            &format!("could not read the filler's sequence number before the cases: {error}"),
        ),
    };
    println!("the filler's sequence number before the cases: {sequence_before}");

    let default_pools = pools_toml(&pool, &xlm, &[usdc.as_str()]);
    let blnd_pools = pools_toml(&pool, &xlm, &[blnd.as_str()]);
    let started = Instant::now();

    // (a) armed, real key: the account exists and clears the default
    // XLM_FEE_RESERVE, but has supplied nothing as collateral, so
    // `validate_filler` warns about `min_primary_collateral` rather than
    // refusing to start — a warning is exactly what an armed but
    // under-collateralised filler deserves, not a refusal to run at all.
    let (status, output) = run_check_config(
        &env,
        &database_url,
        &default_pools,
        false,
        Some(filler_secret.as_str()),
        &[],
    );
    expect_case("a", status, &output, 0, "short of min_primary_collateral");

    // (b) dry run, real key, an XLM_FEE_RESERVE no sandbox wallet holds:
    // a warning, not a refusal, because a dry run submits nothing either
    // way.
    let (status, output) = run_check_config(
        &env,
        &database_url,
        &default_pools,
        true,
        Some(filler_secret.as_str()),
        &[("XLM_FEE_RESERVE", "1000000".to_string())],
    );
    expect_case("b", status, &output, 0, "XLM_FEE_RESERVE asks for");

    // (c) the same shortfall, armed: now a refusal, exit 2 — the same
    // message `validate_filler` raises as an error rather than a warning.
    let (status, output) = run_check_config(
        &env,
        &database_url,
        &default_pools,
        false,
        Some(filler_secret.as_str()),
        &[("XLM_FEE_RESERVE", "1000000".to_string())],
    );
    expect_case("c", status, &output, 2, "XLM_FEE_RESERVE asks for");

    // (d) DRY_RUN=false with no FILLER_SECRET_KEY: `Args::signing_keys`'
    // own refusal, raised before `check_config` reads chain or database
    // at all.
    let (status, output) = run_check_config(&env, &database_url, &default_pools, false, None, &[]);
    expect_case(
        "d",
        status,
        &output,
        2,
        "DRY_RUN=false needs FILLER_SECRET_KEY",
    );

    // (e) a pools config naming BLND as a supported bid asset, which is
    // not one of this pool's reserves.
    let (status, output) = run_check_config(&env, &database_url, &blnd_pools, true, None, &[]);
    expect_case("e", status, &output, 2, "is not a reserve");

    // (f) a DATABASE_URL naming a database this test never created.
    let (status, output) = run_check_config(&env, &absent_url, &default_pools, true, None, &[]);
    expect_case("f", status, &output, 2, "connecting to the database failed");

    println!(
        "all six cases ran in {:.1} s",
        started.elapsed().as_secs_f64()
    );

    let sequence_after = match rpc.account(&filler).await {
        Ok(account) => account.sequence,
        Err(error) => fail_check(
            "",
            &format!("could not read the filler's sequence number after the cases: {error}"),
        ),
    };
    println!("the filler's sequence number after the cases: {sequence_after}");
    assert_eq!(
        sequence_before, sequence_after,
        "the filler's sequence number moved from {sequence_before} to {sequence_after} — a \
         check-config run sent something"
    );

    let migration_check = match PgPool::connect(&database_url).await {
        Ok(pool) => pool,
        Err(error) => fail_check(
            "",
            &format!("could not connect to {database} to check for a migrations table: {error}"),
        ),
    };
    let migrations = match sqlx::query_scalar::<_, Option<String>>(
        "SELECT to_regclass('_sqlx_migrations')::text",
    )
    .fetch_one(&migration_check)
    .await
    {
        Ok(value) => value,
        Err(error) => fail_check(
            "",
            &format!("could not check {database} for a migrations table: {error}"),
        ),
    };
    // Closed before the drop below, for the same reason
    // `create_run_database`'s own doc gives: `DROP DATABASE` refuses while
    // anything is still connected.
    migration_check.close().await;
    println!("to_regclass('_sqlx_migrations') on {database}: {migrations:?}");
    assert_eq!(
        migrations, None,
        "check-config migrated {database} — it should only connect and ping"
    );

    // Last, and only here: every case passed and the two assertions above
    // held, so reaching this line is what "the run succeeded" means.
    drop_run_database(&root, &maintenance_url, &database).await;
}
