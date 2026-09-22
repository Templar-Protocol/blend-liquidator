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
//! - [`dry_run`]: the tier's most important safety test. A dry-run bot
//!   holding the real filler key never signs or sends anything — not to
//!   create a borrower's auction, and not to fill one an armed run of the
//!   same bot created for it — proven on the filler's own sequence number
//!   as well as on the audit tables and the chain.
//! - [`unwind_repay`]: deployed with the filler holding no USDC, so its
//!   fill leaves debt behind for the unwind's repay branch — the one thing
//!   `liquidation`'s own run never exercises (see below) — to repay once
//!   the wallet is funded and a second bot restarts.
//! - [`restart_adopt`]: a bot is `SIGKILL`ed right after creating a
//!   borrower's liquidation auction, and a second instance, on a database
//!   that never recorded it, finds the auction still open on chain, adopts
//!   it (`AuctionInProgress`, 1212) rather than trying to create a second
//!   one, and fills it — proving `Auctioneer::adopt`, otherwise
//!   unreachable from this tier's own continuous runs.
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
//! repay branch has no coverage there. [`unwind_repay`] is what covers it:
//! deployed with a filler holding no USDC at all, its first bot's fill
//! leaves the bid unrepaid, the unwind's step-3 withdrawal narrows the
//! collateral to whatever the outstanding debt still allows and then goes
//! idle with debt still owed — raising `UnwindLeftovers` — and its second
//! bot, once `mint.sh` has funded the wallet, repays that debt on its very
//! first tick and withdraws the rest down to the primary floor.

mod sandbox_harness;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blend_liquidator::chain::xdr::AuctionType;
use blend_liquidator::chain::{PoolReader, RpcClient};
use blend_liquidator::config::ChainConfig;
use blend_liquidator::math::AuctionData;
use blend_liquidator::store::Store;
use sqlx::postgres::PgPool;

use sandbox_harness::{
    assert_all_dry_run, assert_auction_unchanged, assert_counter, assert_counter_at_least,
    assert_metrics, assert_no_auction, assert_no_tx_hash_row, crash, create_run_database,
    create_run_database_unmigrated, drop_run_database, fail, fail_check, fill_budget,
    log_lines_containing, mint, note_run_database, pools_toml, print_nonzero_counters,
    read_metrics, repo_root, require_standalone_rpc, required, run_check_config, sandbox_env,
    spawn_bot, terminate, wait_for_adopted_auction, wait_for_auction, wait_for_dry_run_row,
    wait_for_ledgers_past, wait_for_liability, wait_for_ready, wait_for_tx_hash, wait_for_unwind,
    wait_for_unwind_leftovers, with_database, Bot, BotConfig, CREATIONS_VIOLATING_DRY_RUN,
    CREATION_TIMEOUT, CREATION_TX_HASH, DRY_RUN_CREATION_ROW, DRY_RUN_FILL_ROW,
    FILLS_VIOLATING_DRY_RUN, FILL_TX_HASH,
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

/// How long the auction entry has to appear on chain once phase B's
/// creation has a transaction hash. It is the same transaction: the entry
/// exists the moment that hash is confirmed, so this is slack for the RPC
/// to catch up, not a real wait.
const AUCTION_ENTRY_TIMEOUT: Duration = Duration::from_mins(1);

/// How long the chain has to close the handful of ledgers `dry_run`'s
/// phases A and C each wait out. A fixed budget, not a measured one, like
/// [`sandbox_harness::CREATION_TIMEOUT`]: unlike the fill wait, these
/// waits do not depend on the auction's own ramp, only on the sandbox
/// closing ledgers at all, which [`fill_budget`] has already proven it
/// does by the time phase C reaches its own wait.
const LEDGER_ADVANCE_TIMEOUT: Duration = Duration::from_mins(3);

/// What [`unwind_repay`] mints the filler between its two runs, in USDC
/// stroops (7 decimals): 1,000 USDC, far more than the auction's own bid,
/// so run 2's repay is never itself short.
const MINT_AMOUNT: i128 = 10_000_000_000;

/// What every phase of [`dry_run`] shares: the pool's identity, the chain
/// and store handles, and the pools config a dry-run bot spawns with.
/// Grouped into one struct rather than passed field by field, so each
/// phase function takes one argument instead of the dozen a flat parameter
/// list would need.
struct DryRunCtx<'a> {
    root: &'a Path,
    env: &'a BTreeMap<String, String>,
    rpc: &'a RpcClient,
    store: &'a Store,
    http: &'a reqwest::Client,
    pool: &'a str,
    xlm: &'a str,
    borrower: &'a str,
    filler: &'a str,
    database_url: &'a str,
    seed_path: &'a Path,
    /// `supported_bid` includes USDC, the auction's actual bid asset — the
    /// pools config every phase but B spawns with.
    standard_pools: &'a str,
}

/// The filler's sequence number, before any bot in `phase` has been
/// spawned. Pre-spawn, so a failure here `panic!`s directly rather than
/// going through [`fail`] — there is no bot yet for it to tail.
async fn read_sequence(rpc: &RpcClient, filler: &str, phase: &str) -> i64 {
    match rpc.account(filler).await {
        Ok(account) => account.sequence,
        Err(error) => {
            panic!("{phase}: could not read the filler's sequence number before spawning: {error}")
        }
    }
}

/// Asserts the filler's sequence number still matches `before`, once
/// `bot` has drained and exited — the proof that whatever `phase` decided,
/// it never signed or sent a transaction with the real key it held.
async fn assert_sequence_unchanged(
    bot: &Bot,
    rpc: &RpcClient,
    filler: &str,
    before: i64,
    phase: &str,
) {
    let after = match rpc.account(filler).await {
        Ok(account) => account.sequence,
        Err(error) => fail(
            bot,
            &format!("{phase}: could not read the filler's sequence number after SIGTERM: {error}"),
        ),
    };
    println!("{phase}: the filler's sequence number after SIGTERM: {after}");
    if before != after {
        fail(
            bot,
            &format!(
                "{phase}: the filler's sequence number moved from {before} to {after} — a \
                 dry-run bot signed and sent something"
            ),
        );
    }
}

/// Fails if a dry-run row carries a transaction hash: a dry run only ever
/// writes an audit row, never submits it, so any hash at all is the
/// finding this whole scenario exists to catch.
fn assert_no_tx_hash(bot: &Bot, tx_hash: Option<String>, ledger: i64, table: &str, phase: &str) {
    if let Some(hash) = tx_hash {
        fail(
            bot,
            &format!(
                "{phase}: the dry-run {table} row (ledger {ledger}) carries a transaction hash \
                 ({hash}) — a dry run must never submit"
            ),
        );
    }
}

/// Phase A of [`dry_run`]: a dry-run bot holding the real filler key must
/// decide and record a liquidation without ever signing or sending
/// anything. The filler's own sequence number is the proof a `creations`
/// row with no `tx_hash` cannot fake on its own — a row is only ever
/// written, never submitted, but the sequence number is chain state this
/// test does not control at all.
async fn dry_run_phase_a(ctx: &DryRunCtx<'_>) {
    let sequence_before = read_sequence(ctx.rpc, ctx.filler, "phase A").await;
    println!("phase A: the filler's sequence number before spawning: {sequence_before}");

    let mut bot = spawn_bot(
        ctx.root,
        ctx.env,
        BotConfig {
            dry_run: true,
            filler_secret: Some(required(ctx.env, "SANDBOX_FILLER_SECRET_KEY")),
            pools: ctx.standard_pools.to_string(),
            database_url: ctx.database_url.to_string(),
            log_name: "bot-dry-run-a.log",
            extra_env: vec![("SEED_FILE", ctx.seed_path.display().to_string())],
        },
    );

    wait_for_ready(&mut bot, ctx.http).await;

    println!("phase A: crashing XLM's price at {:.1} s", bot.elapsed());
    crash(&bot, ctx.root);

    let baseline_ledger = match ctx.rpc.latest_ledger().await {
        Ok(ledger) => ledger.sequence,
        Err(error) => fail(&bot, &format!("could not read the latest ledger: {error}")),
    };

    let (tx_hash, decided_at_ledger) = wait_for_dry_run_row(
        &mut bot,
        ctx.store,
        DRY_RUN_CREATION_ROW,
        "a dry-run creations row for the borrower",
        CREATION_TIMEOUT,
        ctx.pool,
        ctx.borrower,
    )
    .await;
    assert_no_tx_hash(&bot, tx_hash, decided_at_ledger, "creations", "phase A");

    assert_all_dry_run(
        &bot,
        ctx.store,
        CREATIONS_VIOLATING_DRY_RUN,
        "creations",
        ctx.pool,
        ctx.borrower,
    )
    .await;

    assert_no_auction(
        &bot,
        ctx.rpc,
        ctx.pool,
        ctx.borrower,
        AuctionType::UserLiquidation,
        "phase A, immediately after the dry-run creation",
    )
    .await;

    wait_for_ledgers_past(
        &mut bot,
        ctx.rpc,
        baseline_ledger,
        10,
        "the chain to advance 10 ledgers past the crash",
        LEDGER_ADVANCE_TIMEOUT,
    )
    .await;

    assert_no_auction(
        &bot,
        ctx.rpc,
        ctx.pool,
        ctx.borrower,
        AuctionType::UserLiquidation,
        "phase A, 10 ledgers later",
    )
    .await;

    let metrics = read_metrics(&bot, ctx.http).await;
    assert_counter(
        &bot,
        &metrics,
        "blend_liquidator_creations_total{result=\"succeeded\"}",
        0,
    );

    terminate(&mut bot).await;
    assert_sequence_unchanged(&bot, ctx.rpc, ctx.filler, sequence_before, "phase A").await;

    println!("phase A done in {:.1} s", bot.elapsed());
}

/// Phase B of [`dry_run`]: an armed creator whose filler cannot fill the
/// auction it creates — `supported_bid` excludes USDC, the auction's own
/// bid asset — proving the bot creates a real, on-chain auction when
/// armed, for phase C's dry-run filler to sit in front of without ever
/// touching it. Answers the auction entry it created.
async fn armed_creation_phase_b(ctx: &DryRunCtx<'_>) -> AuctionData {
    let unfillable_pools = pools_toml(ctx.pool, ctx.xlm, &[ctx.xlm]);
    let mut bot = spawn_bot(
        ctx.root,
        ctx.env,
        BotConfig {
            dry_run: false,
            filler_secret: Some(required(ctx.env, "SANDBOX_FILLER_SECRET_KEY")),
            pools: unfillable_pools,
            database_url: ctx.database_url.to_string(),
            log_name: "bot-creator-b.log",
            extra_env: vec![("SEED_FILE", ctx.seed_path.display().to_string())],
        },
    );

    wait_for_ready(&mut bot, ctx.http).await;

    let creation_tx = wait_for_tx_hash(
        &mut bot,
        ctx.store,
        CREATION_TX_HASH,
        "the armed auctioneer to create the borrower's liquidation auction",
        CREATION_TIMEOUT,
        ctx.pool,
        ctx.borrower,
    )
    .await;

    let (ledger, entry) = wait_for_auction(
        &mut bot,
        ctx.rpc,
        ctx.pool,
        ctx.borrower,
        AuctionType::UserLiquidation,
        "the auction entry to exist on chain",
        AUCTION_ENTRY_TIMEOUT,
    )
    .await;
    println!(
        "phase B: auction created (tx {creation_tx}) at ledger {ledger}, start block {}, bid \
         {:?}, lot {:?}",
        entry.block, entry.bid, entry.lot
    );

    terminate(&mut bot).await;
    println!("phase B done in {:.1} s", bot.elapsed());
    entry
}

/// Phase C of [`dry_run`]: a second dry-run bot, on the same database,
/// must plan a fill for the auction phase B created and never submit it —
/// the auction entry outlives 20 more ledgers unchanged, and the filler's
/// sequence number again does not move.
async fn dry_run_phase_c(ctx: &DryRunCtx<'_>, expected_entry: &AuctionData) {
    let sequence_before = read_sequence(ctx.rpc, ctx.filler, "phase C").await;
    println!("phase C: the filler's sequence number before spawning: {sequence_before}");

    // Read fresh rather than trust phase B's own read: this is this
    // phase's own baseline, and the 20-ledger check below must compare
    // against what phase C itself observed, not an assumption that
    // nothing moved between the two phases.
    let entry_at_start = match PoolReader::new(ctx.rpc, ctx.pool)
        .auction(ctx.borrower, AuctionType::UserLiquidation)
        .await
    {
        Ok(Some((_, entry))) => entry,
        Ok(None) => panic!(
            "phase C: no auction entry exists for the borrower at the start of phase C — \
             phase B's auction is gone"
        ),
        Err(error) => panic!("phase C: could not read the auction entry: {error}"),
    };
    assert!(
        entry_at_start.bid == expected_entry.bid && entry_at_start.lot == expected_entry.lot,
        "phase C: the auction entry changed between phase B and phase C — expected bid {:?} \
         lot {:?}, found bid {:?} lot {:?}",
        expected_entry.bid,
        expected_entry.lot,
        entry_at_start.bid,
        entry_at_start.lot
    );

    let mut bot = spawn_bot(
        ctx.root,
        ctx.env,
        BotConfig {
            dry_run: true,
            filler_secret: Some(required(ctx.env, "SANDBOX_FILLER_SECRET_KEY")),
            pools: ctx.standard_pools.to_string(),
            database_url: ctx.database_url.to_string(),
            log_name: "bot-dry-run-c.log",
            extra_env: vec![("SEED_FILE", ctx.seed_path.display().to_string())],
        },
    );

    wait_for_ready(&mut bot, ctx.http).await;

    let budget = fill_budget(&mut bot, ctx.rpc).await;
    let (tx_hash, fill_ledger) = wait_for_dry_run_row(
        &mut bot,
        ctx.store,
        DRY_RUN_FILL_ROW,
        "a dry-run fills row for the borrower",
        budget,
        ctx.pool,
        ctx.borrower,
    )
    .await;
    assert_no_tx_hash(&bot, tx_hash, fill_ledger, "fills", "phase C");

    assert_all_dry_run(
        &bot,
        ctx.store,
        FILLS_VIOLATING_DRY_RUN,
        "fills",
        ctx.pool,
        ctx.borrower,
    )
    .await;

    let observed_at = match ctx.rpc.latest_ledger().await {
        Ok(ledger) => ledger.sequence,
        Err(error) => fail(&bot, &format!("could not read the latest ledger: {error}")),
    };

    wait_for_ledgers_past(
        &mut bot,
        ctx.rpc,
        observed_at,
        20,
        "the chain to advance 20 ledgers past the dry-run fill row",
        LEDGER_ADVANCE_TIMEOUT,
    )
    .await;

    assert_auction_unchanged(
        &bot,
        ctx.rpc,
        ctx.pool,
        ctx.borrower,
        AuctionType::UserLiquidation,
        &entry_at_start,
        "phase C, 20 ledgers after the dry-run fill",
    )
    .await;

    let metrics = read_metrics(&bot, ctx.http).await;
    assert_counter(
        &bot,
        &metrics,
        "blend_liquidator_fills_total{result=\"succeeded\"}",
        0,
    );

    terminate(&mut bot).await;
    assert_sequence_unchanged(&bot, ctx.rpc, ctx.filler, sequence_before, "phase C").await;

    println!("phase C done in {:.1} s", bot.elapsed());
}

/// The `dry_run` scenario: three bots in a row on one network and one
/// database — a dry-run bot holding the real filler key (phase A), an
/// armed creator whose filler cannot fill what it creates (phase B), and a
/// second dry-run bot facing that live auction (phase C) — proving that
/// `DRY_RUN=true` never signs or sends anything, whether the decision is
/// to create an auction or to fill one, however real the key it holds.
///
/// Three phases, each its own bot, log and set of assertions: one function
/// per phase, with the sequence-number and no-tx-hash checks they share
/// factored out above, keeps this and each of them under clippy's line
/// count without an `#[allow]`.
#[tokio::test]
#[ignore = "needs the local sandbox network: scripts/sandbox/up.sh && scripts/sandbox/deploy.sh"]
async fn dry_run() {
    let root = repo_root();
    let env = sandbox_env(&root.join("target/sandbox/sandbox.env"), "dry_run");

    let passphrase = required(&env, "SANDBOX_PASSPHRASE").to_string();
    let pool = required(&env, "SANDBOX_POOL").to_string();
    let xlm = required(&env, "SANDBOX_XLM").to_string();
    let usdc = required(&env, "SANDBOX_USDC").to_string();
    let borrower = required(&env, "SANDBOX_BORROWER").to_string();
    let filler = required(&env, "SANDBOX_FILLER").to_string();
    let rpc_url = required(&env, "SANDBOX_RPC_URL").to_string();

    // Ordered with the two refusals `sandbox_env` just made, and for the
    // same reason every other scenario keeps it first: nothing below this
    // line may run against an endpoint whose own answer has not been
    // checked — and this scenario, of all of them, is the one where that
    // matters most, since every phase hands the real filler key to a
    // spawned bot.
    require_standalone_rpc(&rpc_url).await;

    let Ok(maintenance_url) = std::env::var("DATABASE_URL") else {
        panic!("DATABASE_URL is not set — the store tests need it too; see `make db-up`")
    };
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
    let seed_path = sandbox_dir.join("seed-dry-run.toml");
    let seed = format!("[accounts]\n\"{pool}\" = [\"{borrower}\"]\n");
    if let Err(error) = std::fs::write(&seed_path, seed) {
        panic!("could not write {}: {error}", seed_path.display());
    }

    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => panic!("could not build an HTTP client: {error}"),
    };
    let store = match Store::connect(&database_url, 4).await {
        Ok(store) => store,
        Err(error) => panic!("could not connect to {database}: {error}"),
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
        Err(error) => panic!("could not build an RPC client: {error}"),
    };

    let standard_pools = pools_toml(&pool, &xlm, &[usdc.as_str()]);

    let ctx = DryRunCtx {
        root: root.as_path(),
        env: &env,
        rpc: &rpc,
        store: &store,
        http: &http,
        pool: &pool,
        xlm: &xlm,
        borrower: &borrower,
        filler: &filler,
        database_url: &database_url,
        seed_path: seed_path.as_path(),
        standard_pools: &standard_pools,
    };

    println!("--- phase A: a dry-run bot holding the real filler key ---");
    dry_run_phase_a(&ctx).await;

    println!("--- phase B: an armed creator that cannot fill ---");
    let entry = armed_creation_phase_b(&ctx).await;

    println!("--- phase C: a dry-run bot facing that live auction ---");
    dry_run_phase_c(&ctx, &entry).await;

    println!("dry_run passed: no creation and no fill ever landed for {borrower}");

    // Last, and only here: every phase either passed or panicked, so
    // reaching this line is what "the run succeeded" means, and a database
    // nobody will read is a database worth not keeping.
    store.pool().close().await;
    drop_run_database(&root, &maintenance_url, &database).await;
}

/// What both of [`unwind_repay`]'s runs share: the pool's identity, the
/// chain and store handles, and the one pools config both bots spawn with
/// — this scenario needs no `unfillable_pools` variant the way [`dry_run`]
/// does, since both of its bots are meant to fill and unwind.
struct UnwindRepayCtx<'a> {
    root: &'a Path,
    env: &'a BTreeMap<String, String>,
    rpc: &'a RpcClient,
    store: &'a Store,
    http: &'a reqwest::Client,
    pool: &'a str,
    xlm: &'a str,
    borrower: &'a str,
    filler: &'a str,
    database_url: &'a str,
    seed_path: &'a Path,
    pools: &'a str,
}

/// Run 1 of [`unwind_repay`]: an armed bot whose filler wallet holds no
/// USDC creates the borrower's liquidation auction and fills it. The
/// fill's own request list would ordinarily repay the bid out of the
/// wallet — `liquidation`'s own run, and this file's module doc — but with
/// nothing to repay it from, the position the filler takes over keeps the
/// bid asset as a liability, and the unwind pass that follows narrows the
/// collateral to whatever that outstanding debt still allows before it can
/// move nothing further, raising `UnwindLeftovers`. Answers the bot,
/// already sent `SIGTERM` and exited 0, so the caller can hand it to
/// [`sandbox_harness::mint`] for the log context a mint failure would
/// need.
async fn unwind_repay_run_one(ctx: &UnwindRepayCtx<'_>) -> Bot {
    let mut bot = spawn_bot(
        ctx.root,
        ctx.env,
        BotConfig {
            dry_run: false,
            filler_secret: Some(required(ctx.env, "SANDBOX_FILLER_SECRET_KEY")),
            pools: ctx.pools.to_string(),
            database_url: ctx.database_url.to_string(),
            log_name: "bot-unwind-1.log",
            extra_env: vec![("SEED_FILE", ctx.seed_path.display().to_string())],
        },
    );

    wait_for_ready(&mut bot, ctx.http).await;

    println!("run 1: crashing XLM's price at {:.1} s", bot.elapsed());
    crash(&bot, ctx.root);

    let creation = wait_for_tx_hash(
        &mut bot,
        ctx.store,
        CREATION_TX_HASH,
        "the auctioneer to create the borrower's liquidation auction",
        CREATION_TIMEOUT,
        ctx.pool,
        ctx.borrower,
    )
    .await;
    // Measured, not assumed, the same reason `liquidation` measures it: the
    // fill waits on the auction's own ledger ramp.
    let budget = fill_budget(&mut bot, ctx.rpc).await;
    let fill = wait_for_tx_hash(
        &mut bot,
        ctx.store,
        FILL_TX_HASH,
        "the filler to take that auction",
        budget,
        ctx.pool,
        ctx.borrower,
    )
    .await;
    println!(
        "run 1: creation {creation}, fill {fill} at {:.1} s",
        bot.elapsed()
    );
    // For the report only — never an assertion. `"fill planned"` names the
    // ledger, percent and the values `plan_fill` aimed at; `"fill
    // recorded"` names the bid and lot the audit row was written with
    // before anything was sent.
    for line in log_lines_containing(&bot, "\"fill planned\"") {
        println!("run 1, the plan: {line}");
    }
    for line in log_lines_containing(&bot, "\"fill recorded\"") {
        println!("run 1, the record: {line}");
    }

    let liabilities = wait_for_liability(&mut bot, ctx.rpc, ctx.pool, ctx.filler, ctx.xlm).await;
    let plural = if liabilities == 1 { "y" } else { "ies" };
    println!(
        "run 1: the filler holds {liabilities} liabilit{plural} at {:.1} s",
        bot.elapsed()
    );

    wait_for_unwind_leftovers(&mut bot, ctx.http).await;
    for line in log_lines_containing(&bot, "debt the wallet cannot repay remains") {
        println!("run 1, the alert: {line}");
    }

    let metrics = read_metrics(&bot, ctx.http).await;
    print_nonzero_counters(&metrics);

    terminate(&mut bot).await;
    println!("run 1 done in {:.1} s", bot.elapsed());
    bot
}

/// Run 2 of [`unwind_repay`]: a second bot on the same database, now that
/// [`sandbox_harness::mint`] has funded the filler's wallet. Its startup
/// unwind pass runs on the very first tick for every configured pool
/// (`src/service.rs`'s own doc), repays what run 1 left owing, and
/// withdraws the rest of the primary asset down to its floor —
/// [`wait_for_unwind`]'s own `settled()` condition, unchanged from
/// [`liquidation`]'s.
async fn unwind_repay_run_two(ctx: &UnwindRepayCtx<'_>) {
    let mut bot = spawn_bot(
        ctx.root,
        ctx.env,
        BotConfig {
            dry_run: false,
            filler_secret: Some(required(ctx.env, "SANDBOX_FILLER_SECRET_KEY")),
            pools: ctx.pools.to_string(),
            database_url: ctx.database_url.to_string(),
            log_name: "bot-unwind-2.log",
            extra_env: vec![("SEED_FILE", ctx.seed_path.display().to_string())],
        },
    );

    wait_for_ready(&mut bot, ctx.http).await;

    let collateral = wait_for_unwind(&mut bot, ctx.rpc, ctx.pool, ctx.filler, ctx.xlm).await;

    // For the report only, the same rule run 1's own log excerpts keep.
    for line in log_lines_containing(&bot, "\"unwind planned\"") {
        println!("run 2, planned: {line}");
    }
    for line in log_lines_containing(&bot, "this unwind landed") {
        println!("run 2, landed: {line}");
    }

    let metrics = read_metrics(&bot, ctx.http).await;
    print_nonzero_counters(&metrics);
    // No new fill in this run — the auction run 1 took is gone — and at
    // least one unwind pass, the same two assertions [`assert_metrics`]
    // makes for `liquidation`, without its `creations_total`/`fills_total`
    // "exactly one" pair, which is run 1's to prove, not this run's.
    assert_counter(
        &bot,
        &metrics,
        "blend_liquidator_fills_total{result=\"succeeded\"}",
        0,
    );
    let passes = assert_counter_at_least(&bot, &metrics, "blend_liquidator_unwind_passes_total", 1);

    terminate(&mut bot).await;
    println!(
        "run 2 done in {:.1} s: {passes} unwind pass(es), {collateral} stroops of XLM collateral \
         left",
        bot.elapsed()
    );
}

/// The scenario's own precondition, load-bearing per this task's brief: a
/// deploy that minted the filler any USDC at all would let its fill repay
/// the bid the same way `liquidation`'s own does, leaving nothing for this
/// run to prove. Pre-spawn, so a failure here `panic!`s directly — there
/// is no bot yet for [`fail`] to tail.
async fn assert_filler_holds_no_usdc(rpc: &RpcClient, pool: &str, usdc: &str, filler: &str) {
    let balance = match PoolReader::new(rpc, pool).balance(usdc, filler).await {
        Ok((_, balance)) => balance,
        Err(error) => panic!("could not read the filler's USDC balance before spawning: {error}"),
    };
    assert_eq!(
        balance, 0,
        "the filler already holds {balance} stroops of USDC — unwind_repay's deploy must mint \
         it none, or its fill can cover the bid and there is no debt to leave behind"
    );
    println!("confirmed: the filler holds no USDC before run 1");
}

/// The proof [`sandbox_harness::mint`] actually funded the wallet that run
/// 2 depends on: a balance under `minimum` fails through `bot` — run 1's,
/// already terminated, kept only for the log tail and the kept-database
/// message [`fail`] prints.
async fn assert_filler_holds_usdc(
    bot: &Bot,
    rpc: &RpcClient,
    pool: &str,
    usdc: &str,
    filler: &str,
    minimum: i128,
) {
    let balance = match PoolReader::new(rpc, pool).balance(usdc, filler).await {
        Ok((_, balance)) => balance,
        Err(error) => fail(
            bot,
            &format!("could not read the filler's USDC balance after minting: {error}"),
        ),
    };
    if balance < minimum {
        fail(
            bot,
            &format!(
                "the filler's USDC balance is {balance} after minting, expected at least {minimum}"
            ),
        );
    }
    println!("confirmed: the filler holds {balance} stroops of USDC before run 2");
}

/// The `unwind_repay` scenario: a fill whose bid the filler's wallet
/// cannot cover leaves it holding debt that raises `UnwindLeftovers`, and
/// — once the wallet is funded and a second bot restarts — the unwind's
/// repay branch clears it. Deployed with `SANDBOX_SCENARIO=unwind_repay`,
/// which mints the filler no USDC at all; see this file's module doc for
/// what this covers that `liquidation`'s own run does not.
///
/// Two runs sharing one database — [`unwind_repay_run_one`] and
/// [`unwind_repay_run_two`], with [`sandbox_harness::mint`] funding the
/// wallet between them — keeps this function and each of them well inside
/// clippy's line count without an `#[allow]`, the same reason [`dry_run`]
/// is split into phase functions above.
///
/// Ignored on purpose — it needs `scripts/sandbox/up.sh` and
/// `SANDBOX_SCENARIO=unwind_repay scripts/sandbox/deploy.sh` to have run,
/// and a Postgres at `DATABASE_URL`.
#[tokio::test]
#[ignore = "needs the local sandbox network: scripts/sandbox/up.sh && scripts/sandbox/deploy.sh"]
async fn unwind_repay() {
    let root = repo_root();
    let env = sandbox_env(&root.join("target/sandbox/sandbox.env"), "unwind_repay");

    let passphrase = required(&env, "SANDBOX_PASSPHRASE").to_string();
    let pool = required(&env, "SANDBOX_POOL").to_string();
    let xlm = required(&env, "SANDBOX_XLM").to_string();
    let usdc = required(&env, "SANDBOX_USDC").to_string();
    let borrower = required(&env, "SANDBOX_BORROWER").to_string();
    let filler = required(&env, "SANDBOX_FILLER").to_string();
    let rpc_url = required(&env, "SANDBOX_RPC_URL").to_string();

    // Ordered with the two refusals `sandbox_env` just made, and for the
    // same reason every other scenario keeps it first: nothing below this
    // line may run against an endpoint whose own answer has not been
    // checked — and this scenario, of all of them, is the one that arms
    // two bots with the real filler key rather than one.
    require_standalone_rpc(&rpc_url).await;

    let Ok(maintenance_url) = std::env::var("DATABASE_URL") else {
        panic!("DATABASE_URL is not set — the store tests need it too; see `make db-up`")
    };
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
    let seed_path = sandbox_dir.join("seed-unwind-repay.toml");
    let seed = format!("[accounts]\n\"{pool}\" = [\"{borrower}\"]\n");
    if let Err(error) = std::fs::write(&seed_path, seed) {
        panic!("could not write {}: {error}", seed_path.display());
    }

    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => panic!("could not build an HTTP client: {error}"),
    };
    let store = match Store::connect(&database_url, 4).await {
        Ok(store) => store,
        Err(error) => panic!("could not connect to {database}: {error}"),
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
        Err(error) => panic!("could not build an RPC client: {error}"),
    };

    assert_filler_holds_no_usdc(&rpc, &pool, &usdc, &filler).await;

    let pools = pools_toml(&pool, &xlm, &[usdc.as_str()]);
    let ctx = UnwindRepayCtx {
        root: root.as_path(),
        env: &env,
        rpc: &rpc,
        store: &store,
        http: &http,
        pool: &pool,
        xlm: &xlm,
        borrower: &borrower,
        filler: &filler,
        database_url: &database_url,
        seed_path: seed_path.as_path(),
        pools: &pools,
    };

    println!("--- run 1: a fill the wallet cannot cover ---");
    let bot_one = unwind_repay_run_one(&ctx).await;

    println!("--- funding the wallet ---");
    mint(&bot_one, &root, MINT_AMOUNT);
    assert_filler_holds_usdc(&bot_one, &rpc, &pool, &usdc, &filler, MINT_AMOUNT).await;

    println!("--- run 2: the startup unwind pass repays it ---");
    unwind_repay_run_two(&ctx).await;

    println!("unwind_repay passed for {borrower}");

    // Last, and only here: both runs either passed or panicked, so
    // reaching this line is what "the run succeeded" means, and a database
    // nobody will read is a database worth not keeping.
    store.pool().close().await;
    drop_run_database(&root, &maintenance_url, &database).await;
}

/// What both of [`restart_adopt`]'s bots share: the pool's identity, the
/// chain and HTTP handles, and the one seed file both bots read the
/// borrower from. Unlike [`UnwindRepayCtx`], there is no single shared
/// `Store` here — each bot gets its own database, and each phase function
/// below owns that database's own connection's lifecycle, closing it
/// before it returns.
struct RestartAdoptCtx<'a> {
    root: &'a Path,
    env: &'a BTreeMap<String, String>,
    rpc: &'a RpcClient,
    http: &'a reqwest::Client,
    pool: &'a str,
    xlm: &'a str,
    usdc: &'a str,
    borrower: &'a str,
    filler: &'a str,
    seed_path: &'a Path,
}

/// Bot #1 of [`restart_adopt`]: an armed creator whose filler cannot fill
/// its own auction — `supported_bid = [xlm]`, while the auction's bid is
/// USDC, the same trick [`armed_creation_phase_b`] uses — creates the
/// borrower's liquidation auction and is killed with `SIGKILL`, never
/// `SIGTERM`, before it does anything else with it. The kill is the whole
/// point: nothing about a graceful exit runs, and bot #2's database never
/// hears from this bot at all, so whatever bot #2 later does with this
/// auction has to be the adoption path, not a resumed session on the same
/// database. Answers the auction entry it created, read from chain.
async fn restart_adopt_bot_one(ctx: &RestartAdoptCtx<'_>, database_url: &str) -> AuctionData {
    let store = match Store::connect(database_url, 2).await {
        Ok(store) => store,
        Err(error) => panic!("bot #1: could not connect to its database: {error}"),
    };

    let unfillable_pools = pools_toml(ctx.pool, ctx.xlm, &[ctx.xlm]);
    let mut bot = spawn_bot(
        ctx.root,
        ctx.env,
        BotConfig {
            dry_run: false,
            filler_secret: Some(required(ctx.env, "SANDBOX_FILLER_SECRET_KEY")),
            pools: unfillable_pools,
            database_url: database_url.to_string(),
            log_name: "bot-restart-1.log",
            extra_env: vec![("SEED_FILE", ctx.seed_path.display().to_string())],
        },
    );

    wait_for_ready(&mut bot, ctx.http).await;

    println!("bot #1: crashing XLM's price at {:.1} s", bot.elapsed());
    crash(&bot, ctx.root);

    let creation = wait_for_tx_hash(
        &mut bot,
        &store,
        CREATION_TX_HASH,
        "the auctioneer to create the borrower's liquidation auction",
        CREATION_TIMEOUT,
        ctx.pool,
        ctx.borrower,
    )
    .await;

    let (ledger, entry) = wait_for_auction(
        &mut bot,
        ctx.rpc,
        ctx.pool,
        ctx.borrower,
        AuctionType::UserLiquidation,
        "the auction entry to exist on chain",
        AUCTION_ENTRY_TIMEOUT,
    )
    .await;
    println!(
        "bot #1: auction created (tx {creation}) at ledger {ledger}, start block {}, bid {:?}, \
         lot {:?}",
        entry.block, entry.bid, entry.lot
    );

    bot.kill();
    assert!(
        !bot.is_running(),
        "bot #1's process is still recorded as running after kill()"
    );
    println!("bot #1: confirmed dead at {:.1} s", bot.elapsed());

    // Closed before this function returns: bot #2 is not spawned yet, but
    // this connection has nothing left to do, and the caller's database is
    // this bot's own to release.
    store.pool().close().await;
    entry
}

/// Bot #2 of [`restart_adopt`]: an armed bot with the standard, fillable
/// pools config, on a fresh database that has never recorded bot #1's
/// auction at all. Its own `new_auction` simulation is refused with
/// `AuctionInProgress` (1212) — the contract already holds one for this
/// borrower — which is exactly the path `Auctioneer::adopt` exists for
/// (`src/auctioneer.rs`): it re-reads the chain's own entry and writes the
/// store's `auctions` row from it, and `refuse_percent` returns `None`
/// without ever recording a `creations` row for the attempt at all — see
/// `assert_no_tx_hash_row`'s own doc. The filler then walks that adopted
/// row like any other and fills it.
async fn restart_adopt_bot_two(ctx: &RestartAdoptCtx<'_>, database_url: &str) {
    let store = match Store::connect(database_url, 4).await {
        Ok(store) => store,
        Err(error) => panic!("bot #2: could not connect to its database: {error}"),
    };

    let standard_pools = pools_toml(ctx.pool, ctx.xlm, &[ctx.usdc]);
    let mut bot = spawn_bot(
        ctx.root,
        ctx.env,
        BotConfig {
            dry_run: false,
            filler_secret: Some(required(ctx.env, "SANDBOX_FILLER_SECRET_KEY")),
            pools: standard_pools,
            database_url: database_url.to_string(),
            log_name: "bot-restart-2.log",
            extra_env: vec![("SEED_FILE", ctx.seed_path.display().to_string())],
        },
    );

    wait_for_ready(&mut bot, ctx.http).await;

    let adopted = wait_for_adopted_auction(
        &mut bot,
        &store,
        "the auctioneer to adopt the auction bot #1 left open",
        CREATION_TIMEOUT,
        ctx.pool,
        ctx.borrower,
    )
    .await;
    println!(
        "bot #2: adopted the auction at {:.1} s (start ledger {}, bid {:?}, lot {:?})",
        bot.elapsed(),
        adopted.start_ledger,
        adopted.bid,
        adopted.lot
    );

    // The line `refuse_percent` (`src/auctioneer.rs`) logs unconditionally
    // for every refused simulation, filtered here to the one refusal this
    // scenario means to prove: `contract_error` 1212, `AuctionInProgress`,
    // is what sends this borrower down the adoption path rather than a
    // percent-adjustment retry. For the report only — the assertions below
    // and the adopted row waited for above are what this test actually
    // stands or falls on.
    let adoption_lines: Vec<String> =
        log_lines_containing(&bot, "liquidation refused by simulation; skipping")
            .into_iter()
            .filter(|line| line.contains("1212"))
            .collect();
    if adoption_lines.is_empty() {
        fail(
            &bot,
            "bot #2's log names no refusal with contract_error 1212 — the adoption path this \
             scenario means to prove was never taken",
        );
    }
    for line in &adoption_lines {
        println!("bot #2, the adoption path: {line}");
    }

    let budget = fill_budget(&mut bot, ctx.rpc).await;
    let fill = wait_for_tx_hash(
        &mut bot,
        &store,
        FILL_TX_HASH,
        "the filler to take the adopted auction",
        budget,
        ctx.pool,
        ctx.borrower,
    )
    .await;
    let collateral = wait_for_unwind(&mut bot, ctx.rpc, ctx.pool, ctx.filler, ctx.xlm).await;

    assert_no_tx_hash_row(
        &bot,
        &store,
        CREATION_TX_HASH,
        "creations",
        ctx.pool,
        ctx.borrower,
    )
    .await;

    let metrics = read_metrics(&bot, ctx.http).await;
    print_nonzero_counters(&metrics);
    assert_counter(
        &bot,
        &metrics,
        "blend_liquidator_creations_total{result=\"succeeded\"}",
        0,
    );
    assert_counter(
        &bot,
        &metrics,
        "blend_liquidator_fills_total{result=\"succeeded\"}",
        1,
    );

    terminate(&mut bot).await;
    println!(
        "bot #2 done in {:.1} s: fill {fill}, {collateral} stroops of XLM collateral left",
        bot.elapsed()
    );

    store.pool().close().await;
}

/// The `restart_adopt` scenario: a bot `SIGKILL`ed right after creating a
/// borrower's liquidation auction, and a second, fresh instance — on a
/// database that has never heard of that auction — finds it on chain,
/// adopts it, and fills it. What no other scenario in this tier proves:
/// every other bot here creates and fills its own auction inside one
/// continuous run, so `Auctioneer::adopt` (the `AuctionInProgress`/1212
/// path in `src/auctioneer.rs`) is otherwise unreachable from this tier at
/// all.
///
/// Two bots, two databases — [`restart_adopt_bot_one`] and
/// [`restart_adopt_bot_two`], sharing one [`RestartAdoptCtx`] — the same
/// shape [`unwind_repay`]'s own two runs use, except that a restart needs a
/// fresh database for its second bot rather than the one database
/// `unwind_repay`'s two runs share.
///
/// Ignored on purpose — it needs `scripts/sandbox/up.sh` and
/// `SANDBOX_SCENARIO=restart_adopt scripts/sandbox/deploy.sh` to have run,
/// and a Postgres at `DATABASE_URL`.
#[tokio::test]
#[ignore = "needs the local sandbox network: scripts/sandbox/up.sh && scripts/sandbox/deploy.sh"]
async fn restart_adopt() {
    let root = repo_root();
    let env = sandbox_env(&root.join("target/sandbox/sandbox.env"), "restart_adopt");

    let passphrase = required(&env, "SANDBOX_PASSPHRASE").to_string();
    let pool = required(&env, "SANDBOX_POOL").to_string();
    let xlm = required(&env, "SANDBOX_XLM").to_string();
    let usdc = required(&env, "SANDBOX_USDC").to_string();
    let borrower = required(&env, "SANDBOX_BORROWER").to_string();
    let filler = required(&env, "SANDBOX_FILLER").to_string();
    let rpc_url = required(&env, "SANDBOX_RPC_URL").to_string();

    // Ordered with the two refusals `sandbox_env` just made, and for the
    // same reason every other scenario keeps it first: nothing below this
    // line may run against an endpoint whose own answer has not been
    // checked — and this scenario, of all of them, is the one that arms
    // two bots on two different databases with the real filler key.
    require_standalone_rpc(&rpc_url).await;

    let Ok(maintenance_url) = std::env::var("DATABASE_URL") else {
        panic!("DATABASE_URL is not set — the store tests need it too; see `make db-up`")
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let database_one = format!("sandbox_{stamp}_1");
    let database_two = format!("sandbox_{stamp}_2");
    println!("creating bot #1's database {database_one}");
    let database_url_one = create_run_database(&root, &maintenance_url, &database_one).await;
    // Noted once each database exists, so a failure from here on says both
    // are kept; the success path at the bottom drops them and neither is
    // read again.
    note_run_database(&database_one);
    println!("creating bot #2's database {database_two}, fresh — it must never see bot #1's own");
    let database_url_two = create_run_database(&root, &maintenance_url, &database_two).await;
    note_run_database(&database_two);

    let sandbox_dir = root.join("target/sandbox");
    let seed_path = sandbox_dir.join("seed-restart-adopt.toml");
    let seed = format!("[accounts]\n\"{pool}\" = [\"{borrower}\"]\n");
    if let Err(error) = std::fs::write(&seed_path, seed) {
        panic!("could not write {}: {error}", seed_path.display());
    }

    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => panic!("could not build an HTTP client: {error}"),
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
        Err(error) => panic!("could not build an RPC client: {error}"),
    };

    let ctx = RestartAdoptCtx {
        root: root.as_path(),
        env: &env,
        rpc: &rpc,
        http: &http,
        pool: &pool,
        xlm: &xlm,
        usdc: &usdc,
        borrower: &borrower,
        filler: &filler,
        seed_path: seed_path.as_path(),
    };

    println!("--- bot #1: an armed creator that cannot fill ---");
    let entry = restart_adopt_bot_one(&ctx, &database_url_one).await;
    println!(
        "bot #1 created bid {:?} lot {:?} at start block {}",
        entry.bid, entry.lot, entry.block
    );

    println!("--- bot #2: adopts the auction on a fresh database ---");
    restart_adopt_bot_two(&ctx, &database_url_two).await;

    println!("restart_adopt passed for {borrower}");

    // Last, and only here: both bots either passed or panicked, so reaching
    // this line is what "the run succeeded" means, and databases nobody
    // will read are databases worth not keeping.
    drop_run_database(&root, &maintenance_url, &database_one).await;
    drop_run_database(&root, &maintenance_url, &database_two).await;
}
