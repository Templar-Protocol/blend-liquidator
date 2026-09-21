//! The end-to-end sandbox test: the real `liquidator` binary, **armed**,
//! against the local network `scripts/sandbox/up.sh` starts and
//! `scripts/sandbox/deploy.sh` populates.
//!
//! This is the one place in the repository the bot runs with `DRY_RUN=false`
//! and a signing key, so every guard here is about making that safe rather
//! than convenient:
//!
//! - it refuses to run at all unless `target/sandbox/sandbox.env` exists
//!   (the message names the two scripts that write it),
//! - it refuses unless that file's `SANDBOX_PASSPHRASE` is the standalone
//!   network's, and
//! - it refuses unless the RPC at that file's `SANDBOX_RPC_URL` answers
//!   `getNetwork` with the same passphrase. The file is a claim; the node
//!   is the fact, and only the second of those decides which network a
//!   signing key is handed to. All three refusals fire before a database
//!   is created or anything is spawned, so a misconfigured run can never
//!   point an armed bot at a real network.
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
//! **Nothing in this file may panic through `unwrap`/`expect`.** Every
//! failure goes through [`fail`], which prints the tail of the bot's own log
//! before it panics: a run that dies without that tail is a run nobody can
//! diagnose. `panic!` directly is for the three refusals above, which happen
//! before there is a bot or a log at all.
//!
//! One thing the assertions do not prove, so that nobody reads more into
//! them than is there: in this scenario the fill's own request list repays
//! the bid out of the filler's wallet, so the position it takes over
//! arrives with no liabilities and the unwind that follows runs the
//! withdraw step only — `"unwind planned", "actions":1,
//! "remaining_liabilities":"[]"`. The `liabilities == 0` half of
//! [`FillerPosition::settled`] is therefore satisfied by the fill, and the
//! unwind's repay branch has no coverage here. The scenario that would
//! give it some is a filler whose wallet cannot cover the bid, which is
//! the testnet soak — Phase 9's — rather than this tier's one run.
//!
//! The database is this test's own: it creates `sandbox_<unix seconds>` on
//! the `DATABASE_URL` server (the role has `CREATEDB`, which is what
//! `#[sqlx::test]` already relies on) and points the bot at it, so a rerun
//! against a fresh network never reads a previous run's rows. A run that
//! passes drops it again; a run that fails keeps it, because it is then the
//! only durable record of what the bot decided, and every failure says so.
//! `make sandbox-down` drops whatever has been kept, and [`RUN_DATABASES`]
//! is how it knows the names: this test writes them there, because the
//! sweep has nothing to enumerate them with.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blend_liquidator::chain::{PoolReader, RpcClient};
use blend_liquidator::config::ChainConfig;
use blend_liquidator::store::Store;
use sqlx::postgres::PgPool;

/// The only network this test will talk to. `scripts/sandbox/versions.env`
/// holds the same literal, and every sandbox script checks the RPC's own
/// `getNetwork` against it; this check is the same gate on the Rust side, so
/// an armed bot cannot be spawned against anything else however
/// `sandbox.env` was produced.
const STANDALONE_PASSPHRASE: &str = "Standalone Network ; February 2017";

/// How long the bot has to bind its HTTP port and report ready. Readiness
/// needs a store ping and one processed ledger per pool, so it covers
/// migration, the seed and the first poll.
const HEALTHY_TIMEOUT: Duration = Duration::from_mins(1);

/// How long the auctioneer has to create the auction after the crash: one
/// oracle scan (`ORACLE_SCAN_LEDGERS=5`) or full scan (`FULL_SCAN_LEDGERS=10`)
/// to flag the borrower, then the percent walk, then the submission.
const CREATION_TIMEOUT: Duration = Duration::from_secs(90);

/// How many ledgers the filler has to wait for before it can take the
/// auction at a profit.
///
/// This is the one budget the auction's own arithmetic sets rather than the
/// bot's, and it is counted in *ledgers* because that is what the contract
/// counts: the lot ramps linearly to full over the auction's first 200
/// ledgers while the bid stays whole, so the earliest ledger at which the
/// lot covers the bid plus the configured margin is `200 × bid_value /
/// lot_value_at_full`. The figures are a real run's, not an illustration:
/// the auctioneer created this scenario's auction at 69%, a bid of 207 USDC
/// against a full lot of ~3,139 XLM worth ~$235 at the crashed price, and
/// with the pool's 100 bps margin the fill landed at block 178. Both sides
/// scale with the percent, so the break-even block is near-invariant across
/// the band the percent walk lands in. Nothing shortens it: `force_fill`
/// caps the target at 350 ledgers, which is later, not sooner, and this is
/// the earliest-profitable break-even specifically — `pools_toml` below
/// pins `fill_objective` to `earliest-profitable` for exactly that reason,
/// since the crate's own default, `free-fill`, aims at `start + 400`
/// instead, well outside this budget. 190 is 178 with room for a re-plan.
const FILL_LEDGERS: u32 = 190;

/// What [`FILL_LEDGERS`] worth of measured close time is padded by, for the
/// bot's own cadences either side of the fill itself.
const FILL_SLACK: Duration = Duration::from_mins(1);

/// The most the fill may ever be given, however slowly the sandbox closes
/// ledgers. A sandbox that would need longer is reported as such, up front,
/// rather than waited out.
const FILL_TIMEOUT_CAP: Duration = Duration::from_mins(10);

/// How long the close-rate sample runs for before the fill wait.
const CLOSE_RATE_SAMPLE: Duration = Duration::from_secs(5);

/// The longest the sample waits for a single ledger to close before giving
/// up on measuring at all. A sandbox that closes nothing in this long is not
/// one the fill could ever happen on.
const CLOSE_RATE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the unwind has to leave the filler with no liabilities and the
/// primary asset down to its floor, measured from the fill row appearing.
const UNWIND_TIMEOUT: Duration = Duration::from_mins(2);

/// How long the bot has to drain and exit after `SIGTERM`.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Every wait polls at this cadence.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often a wait reprints what it is still waiting for, so a hung run is
/// diagnosable from `--nocapture` without waiting for the timeout.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// How much of the bot's log a failure prints.
const LOG_TAIL_LINES: usize = 100;

/// The port the bot serves `/healthz` and `/metrics` on. Not 8080: the
/// devcontainer forwards that, and a collision would look like a bot that
/// never became ready.
const HTTP_PORT: u16 = 18080;

/// `min_primary_collateral` for the run, in stroops: 100 XLM.
const MIN_PRIMARY_COLLATERAL: i128 = 1_000_000_000;

/// The most XLM collateral the filler may still hold once the unwind has
/// finished, in stroops: `MIN_PRIMARY_COLLATERAL` plus one percent.
///
/// The floor is exact in *underlying*, but a withdrawal is sized in b-tokens
/// and the planner rounds the burn up so the position never drops under the
/// floor; the one percent is that rounding, not slack in the assertion.
const MAX_PRIMARY_COLLATERAL: i128 = 1_010_000_000;

/// The bot, and everything a failure needs to say what it was doing.
///
/// `Drop` kills the child, so a panic anywhere below cannot leave an armed
/// bot running against the sandbox — the ordinary path takes the child out
/// with [`Bot::terminate`] first and leaves `Drop` nothing to do.
struct Bot {
    child: Option<Child>,
    log: PathBuf,
    started: Instant,
}

impl Drop for Bot {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            println!("killing the bot (pid {}) after a failure", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Bot {
    /// Seconds since the bot was spawned, for the run's timeline.
    fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// The last [`LOG_TAIL_LINES`] lines of the bot's log, or a line saying
    /// why there are none.
    fn tail(&self) -> String {
        match std::fs::read_to_string(&self.log) {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                let from = lines.len().saturating_sub(LOG_TAIL_LINES);
                lines[from..].join("\n")
            }
            Err(error) => format!("(could not read {}: {error})", self.log.display()),
        }
    }

    /// `Some(status)` once the child has exited, `None` while it runs.
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(status) => status,
                Err(error) => fail(self, &format!("could not poll the bot: {error}")),
            },
            None => None,
        }
    }
}

/// This run's database, once it exists, so [`fail`] can say it was kept.
///
/// A `static` rather than a field of [`Bot`] because it outlives the bot:
/// the failures worth inspecting a database for include the ones that
/// happen after the process is gone.
static RUN_DATABASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Prints the tail of the bot's log and panics.
///
/// Every failure after the bot is spawned goes through here. The tail comes
/// first and on the same stream as the rest of the test's output, so a CI
/// log reads in the order things happened.
fn fail(bot: &Bot, message: &str) -> ! {
    println!(
        "\n--- last {LOG_TAIL_LINES} lines of {} ---",
        bot.log.display()
    );
    println!("{}", bot.tail());
    println!("--- end of {} ---\n", bot.log.display());
    if let Some(database) = RUN_DATABASE.get() {
        println!(
            "the database {database} is kept for inspection — it is listed in {RUN_DATABASES}, \
             and `make sandbox-down` drops what that file names\n"
        );
    }
    panic!("{message}");
}

/// One named wait: a budget, a 500 ms poll, and a line saying what it is for.
///
/// Every wait in this test is one of these, so a hung run names what it was
/// waiting for rather than timing out anonymously.
struct Wait {
    what: &'static str,
    deadline: Instant,
    budget: Duration,
    last_progress: Instant,
}

impl Wait {
    fn new(what: &'static str, budget: Duration) -> Self {
        println!("waiting for {what} ({} s budget)", budget.as_secs());
        let now = Instant::now();
        Self {
            what,
            deadline: now + budget,
            budget,
            last_progress: now,
        }
    }

    /// Sleeps one poll interval. `false` once the budget has run out.
    ///
    /// Takes the bot because a child that has already exited can never
    /// satisfy any of these conditions: waiting out the whole budget on a
    /// dead process turns a startup failure into a timeout, and the log tail
    /// that explains it arrives minutes later.
    async fn tick(&mut self, bot: &mut Bot) -> bool {
        if let Some(status) = bot.exited() {
            fail(
                bot,
                &format!("the bot exited ({status}) while waiting for {}", self.what),
            );
        }
        if Instant::now() >= self.deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        if self.last_progress.elapsed() >= PROGRESS_INTERVAL {
            self.last_progress = Instant::now();
            let left = self.deadline.saturating_duration_since(Instant::now());
            println!(
                "  still waiting for {} ({} s left of {} s)",
                self.what,
                left.as_secs(),
                self.budget.as_secs()
            );
        }
        true
    }

    /// The message a timeout panics with.
    fn timed_out(&self) -> String {
        format!(
            "timed out after {} s waiting for {}",
            self.budget.as_secs(),
            self.what
        )
    }
}

/// The repository root, from the manifest directory cargo sets for a test.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `KEY="value"` lines from `sandbox.env`, quotes stripped.
///
/// Deliberately not a shell: the file is written by `deploy.sh`'s one
/// heredoc, every value is a plain double-quoted literal, and anything this
/// does not understand would be a change to that heredoc rather than
/// something to interpret generously.
fn parse_env_file(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').to_string();
            values.insert(key.trim().to_string(), value);
        }
    }
    values
}

/// One key of `sandbox.env`, or a panic naming it. Pre-spawn, so this is one
/// of the few places that panics without a log tail — there is no bot yet.
fn required<'a>(env: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    match env.get(key) {
        Some(value) if !value.is_empty() => value,
        _ => panic!(
            "target/sandbox/sandbox.env does not define {key} — re-run scripts/sandbox/deploy.sh"
        ),
    }
}

/// `url` with its database replaced by `name`, keeping user, host, port and
/// any query string.
fn with_database(url: &str, name: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let Some((prefix, _)) = base.rsplit_once('/') else {
        panic!("DATABASE_URL does not name a database: {base}")
    };
    // `scheme://host` has exactly two slashes; a third is the one before the
    // database name. Without it the split above would have eaten the host.
    assert!(
        prefix.matches('/').count() >= 2,
        "DATABASE_URL does not name a database: {base}"
    );
    match query {
        Some(query) => format!("{prefix}/{name}?{query}"),
        None => format!("{prefix}/{name}"),
    }
}

/// Where `make sandbox-down` reads the databases it is to drop, relative to
/// the repository root: one name per line.
///
/// The sweep cannot enumerate them itself — `sqlx database drop` only drops a
/// name it is handed, nothing in sqlx-cli lists databases, and `psql` is in
/// neither CI nor the dev container — so the only process that knows a name
/// is the one that created it, and this is where it leaves it. Appended the
/// moment the database exists and the line removed again when this run drops
/// it, so what the file holds is what the server still holds.
const RUN_DATABASES: &str = "target/sandbox/run-databases";

/// Adds `name` to [`RUN_DATABASES`], creating the file if it is not there.
///
/// Only warns on failure: an unrecorded database is one an operator drops by
/// hand, which is not worth failing a run that has otherwise done everything
/// asked of it.
fn record_run_database(root: &Path, name: &str) {
    let path = root.join(RUN_DATABASES);
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            println!("could not create {}: {error}", parent.display());
            return;
        }
    }
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{name}"));
    match appended {
        Ok(()) => println!("recorded {name} in {}", path.display()),
        Err(error) => println!(
            "could not record {name} in {}: {error} — `make sandbox-down` will not know to drop it",
            path.display()
        ),
    }
}

/// Removes `name` from [`RUN_DATABASES`], leaving every other line.
///
/// Called only where the drop itself succeeded, so the file never claims a
/// database that is gone. Warns rather than failing, for the reason
/// [`record_run_database`] gives.
fn forget_run_database(root: &Path, name: &str) {
    let path = root.join(RUN_DATABASES);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let kept: String =
        text.lines()
            .filter(|line| line.trim() != name)
            .fold(String::new(), |mut all, line| {
                all.push_str(line);
                all.push('\n');
                all
            });
    if let Err(error) = std::fs::write(&path, kept) {
        println!("could not rewrite {}: {error}", path.display());
    }
}

/// Creates this run's database and migrates it, answering its URL.
///
/// Migrating here rather than leaving it to the bot is what lets the row
/// polls below treat a query error as a real failure instead of "the table
/// may not exist yet"; `Store::migrate` is idempotent, so the bot's own run
/// of the same migrator finds nothing to do.
async fn create_run_database(root: &Path, maintenance_url: &str, name: &str) -> String {
    let maintenance = match PgPool::connect(maintenance_url).await {
        Ok(pool) => pool,
        Err(error) => panic!(
            "could not connect to DATABASE_URL to create this run's database: {error} — is \
             `make db-up` running?"
        ),
    };
    // The one statement here that cannot take a bind parameter: Postgres
    // has no placeholder for an identifier. `AssertSqlSafe` is the audit
    // sqlx asks for, and the audit is that `name` is the literal `sandbox_`
    // followed by `SystemTime`'s seconds — digits the caller builds, never
    // anything this process was given.
    let statement = format!("CREATE DATABASE \"{name}\"");
    if let Err(error) = sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
        .execute(&maintenance)
        .await
    {
        panic!("could not create the database {name}: {error}");
    }
    maintenance.close().await;
    // Before the migration below, not after: from the `CREATE` onwards there
    // is a database on the server, and every failure from here keeps it.
    record_run_database(root, name);

    let url = with_database(maintenance_url, name);
    let store = match Store::connect(&url, 2).await {
        Ok(store) => store,
        Err(error) => panic!("could not connect to the run's database {name}: {error}"),
    };
    if let Err(error) = store.migrate().await {
        panic!("could not migrate the run's database {name}: {error}");
    }
    // Closed, never merely dropped: `Drop` cannot do the I/O that sends
    // Postgres a termination, so a dropped pool's backends stay attached
    // until a keepalive timeout notices. `drop_run_database` runs long
    // before that, and `DROP DATABASE` refuses while anything is still
    // connected — so without this close the success path leaks exactly the
    // database it was written to reclaim.
    store.pool().close().await;
    url
}

/// Drops this run's database, on the success path only.
///
/// Called after the bot has exited and this test's own pool is closed:
/// Postgres refuses to drop a database anything is still connected to.
/// A failure to drop only warns — the run itself has already passed, and
/// turning a leaked database name into a red test would say something false
/// about the bot; `make sandbox-down` sweeps whatever is left.
async fn drop_run_database(root: &Path, maintenance_url: &str, name: &str) {
    let maintenance = match PgPool::connect(maintenance_url).await {
        Ok(pool) => pool,
        Err(error) => {
            println!("could not connect to drop {name}: {error} — it is left behind");
            return;
        }
    };
    // Identifiers take no bind parameter; `name` is `sandbox_` and
    // `SystemTime`'s seconds, the same audit `create_run_database` makes.
    let statement = format!("DROP DATABASE \"{name}\"");
    match sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
        .execute(&maintenance)
        .await
    {
        Ok(_) => {
            // Only here: a line left in the file for a database that is gone
            // is a `make sandbox-down` that reports a failure every time.
            forget_run_database(root, name);
            println!("dropped the run's database {name}");
        }
        Err(error) => println!("could not drop {name}: {error} — it is left behind"),
    }
    maintenance.close().await;
}

/// The `POOLS_TOML` the bot follows: one pool, USDC bid, any lot, the
/// primary asset floor the unwind is asserted against.
fn pools_toml(pool: &str, xlm: &str, usdc: &str) -> String {
    format!(
        "[[pools]]\n\
         address = \"{pool}\"\n\
         primary_asset = \"{xlm}\"\n\
         min_primary_collateral = \"{MIN_PRIMARY_COLLATERAL}\"\n\
         min_health_factor = 1.5\n\
         default_profit_bps = 100\n\
         # earliest-profitable, not the crate's free-fill default: FILL_LEDGERS\n\
         # is measured against the lot ramp's break-even ledger, and free-fill\n\
         # would move the fill out to start + 400, past that measured budget.\n\
         fill_objective = \"earliest-profitable\"\n\
         supported_bid = [\"{usdc}\"]\n\
         supported_lot = [\"*\"]\n"
    )
}

/// Spawns the binary with `env_clear` and exactly the environment below.
///
/// `PATH` is the only variable carried over from this process, and the bot
/// does not need even that: it is exec'd by absolute path, reaches the RPC
/// over plain HTTP and Postgres over TCP, and runs no subprocess. It is
/// passed anyway, because a binary that one day shells out and finds no
/// `PATH` is a puzzling thing to debug. `HOME` is deliberately *not*
/// passed: nothing the bot reads lives there, and a test that handed it one
/// would be hiding a dependency on the developer's machine.
fn spawn_bot(
    log_path: &Path,
    database_url: &str,
    env: &BTreeMap<String, String>,
    pools: &str,
    seed_file: &Path,
) -> Bot {
    let log = match std::fs::File::create(log_path) {
        Ok(file) => file,
        Err(error) => panic!("could not create {}: {error}", log_path.display()),
    };
    let errors = match log.try_clone() {
        Ok(file) => file,
        Err(error) => panic!("could not duplicate {}: {error}", log_path.display()),
    };

    let mut command = Command::new(env!("CARGO_BIN_EXE_liquidator"));
    command
        .env_clear()
        .env("DATABASE_URL", database_url)
        .env("NETWORK_PASSPHRASE", required(env, "SANDBOX_PASSPHRASE"))
        .env("RPC_URL", required(env, "SANDBOX_RPC_URL"))
        .env("DRY_RUN", "false")
        .env(
            "FILLER_SECRET_KEY",
            required(env, "SANDBOX_FILLER_SECRET_KEY"),
        )
        .env("POOLS_TOML", pools)
        .env("SEED_FILE", seed_file)
        // Empty, which `Args::service_with_secrets` reads as "no analytics
        // source". The public API must never be reached from a test.
        .env("SEED_URL", "")
        .env("POLL_INTERVAL_MS", "500")
        .env("STARTUP_DELAY_LEDGERS", "0")
        .env("FULL_SCAN_LEDGERS", "10")
        .env("ORACLE_SCAN_LEDGERS", "5")
        .env("XLM_FEE_RESERVE", "50")
        .env("PORT", HTTP_PORT.to_string())
        .env("HTTP_BIND_ADDR", "127.0.0.1")
        .env("LOG_FORMAT", "json")
        .env("RUST_LOG", "info,blend_liquidator=debug")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errors));

    match command.spawn() {
        Ok(child) => {
            println!(
                "spawned the bot (pid {}), logging to {}",
                child.id(),
                log_path.display()
            );
            Bot {
                child: Some(child),
                log: log_path.to_path_buf(),
                started: Instant::now(),
            }
        }
        Err(error) => panic!("could not spawn the liquidator binary: {error}"),
    }
}

/// Waits for `/healthz` to answer 200.
async fn wait_for_ready(bot: &mut Bot, http: &reqwest::Client) {
    let url = format!("http://127.0.0.1:{HTTP_PORT}/healthz");
    let mut wait = Wait::new("the bot's /healthz to answer 200", HEALTHY_TIMEOUT);
    loop {
        if let Ok(response) = http.get(&url).send().await {
            if response.status().is_success() {
                println!("/healthz is 200 at {:.1} s", bot.elapsed());
                return;
            }
        }
        if !wait.tick(bot).await {
            let message = wait.timed_out();
            fail(bot, &message);
        }
    }
}

/// Measures the sandbox's ledger close rate and derives the fill's budget
/// from it.
///
/// The fill waits on the chain's clock, not the bot's: the lot ramp needs
/// [`FILL_LEDGERS`] ledgers whatever they cost in seconds. The quickstart
/// image closes one a second today, and a constant written around that
/// becomes a flake the day it does not — a slower runner would fail here
/// with "the filler never filled", which is a true statement about the
/// wrong thing. So the rate is measured, the budget derived, and a sandbox
/// too slow to finish inside [`FILL_TIMEOUT_CAP`] is reported now rather
/// than in ten minutes' time.
async fn fill_budget(bot: &mut Bot, rpc: &RpcClient) -> Duration {
    let first = match rpc.latest_ledger().await {
        Ok(ledger) => ledger.sequence,
        Err(error) => fail(bot, &format!("could not read the latest ledger: {error}")),
    };
    println!("sampling the sandbox's ledger close rate from ledger {first}");

    let started = Instant::now();
    let last = loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let latest = match rpc.latest_ledger().await {
            Ok(ledger) => ledger.sequence,
            Err(error) => fail(bot, &format!("could not read the latest ledger: {error}")),
        };
        // Both conditions, because one ledger in five seconds measures a
        // rate as badly as five seconds measures a ledger that takes ten.
        if started.elapsed() >= CLOSE_RATE_SAMPLE && latest > first {
            break latest;
        }
        if started.elapsed() >= CLOSE_RATE_TIMEOUT {
            if latest <= first {
                let message = format!(
                    "the sandbox closed no ledger in {} s (still at {latest}) — nothing the \
                     filler waits for can happen on it",
                    CLOSE_RATE_TIMEOUT.as_secs()
                );
                fail(bot, &message);
            }
            break latest;
        }
    };

    let elapsed = started.elapsed().as_secs_f64();
    let closed = f64::from(last.saturating_sub(first));
    let per_ledger = elapsed / closed;
    let ramp = match Duration::try_from_secs_f64(f64::from(FILL_LEDGERS) * per_ledger) {
        Ok(ramp) => ramp,
        Err(error) => fail(
            bot,
            &format!("a close rate of {per_ledger} s per ledger is not a duration: {error}"),
        ),
    };
    let budget = ramp.saturating_add(FILL_SLACK);
    println!(
        "the sandbox closed {closed} ledgers in {elapsed:.1} s — one every {per_ledger:.2} s; the \
         fill needs about {FILL_LEDGERS}, so its budget is {} s",
        budget.as_secs()
    );
    if budget > FILL_TIMEOUT_CAP {
        let message = format!(
            "the sandbox closes a ledger every {per_ledger:.2} s; this scenario needs ~\
             {FILL_LEDGERS} ledgers, which is {} s — past the {} s this test will wait",
            budget.as_secs(),
            FILL_TIMEOUT_CAP.as_secs()
        );
        fail(bot, &message);
    }
    budget
}

/// The auctioneer's audit row for one account, once a transaction has been
/// named for it. Postgres has no placeholder for a table name, so each
/// audit table gets its own literal rather than one interpolated statement.
///
/// `dry_run = false` is redundant against a hash — a dry run simulates and
/// submits nothing, so it can never attach one — and it is here anyway,
/// because the mode is the whole point of this tier and a row that states
/// it is a better witness than one that merely implies it.
const CREATION_TX_HASH: &str = "SELECT tx_hash FROM creations \
     WHERE pool = $1 AND account = $2 AND dry_run = false AND tx_hash IS NOT NULL LIMIT 1";

/// The filler's, the same shape.
const FILL_TX_HASH: &str = "SELECT tx_hash FROM fills \
     WHERE pool = $1 AND account = $2 AND dry_run = false AND tx_hash IS NOT NULL LIMIT 1";

/// Waits for `statement` to answer a transaction hash, and answers it.
///
/// A row with a hash is the audit trail's evidence that a transaction was
/// named on chain; a row without one is an attempt that never reached the
/// network, which is exactly what this must not accept.
async fn wait_for_tx_hash(
    bot: &mut Bot,
    store: &Store,
    statement: &'static str,
    what: &'static str,
    budget: Duration,
    pool: &str,
    account: &str,
) -> String {
    let mut wait = Wait::new(what, budget);
    loop {
        let found = sqlx::query_scalar::<_, String>(statement)
            .bind(pool)
            .bind(account)
            .fetch_optional(store.pool())
            .await;
        match found {
            Ok(Some(hash)) => {
                println!("{what}: {hash} at {:.1} s", bot.elapsed());
                return hash;
            }
            Ok(None) => {}
            Err(error) => fail(bot, &format!("could not read the audit table: {error}")),
        }
        if !wait.tick(bot).await {
            let message = wait.timed_out();
            fail(bot, &message);
        }
    }
}

/// What the filler holds in the pool: its primary-asset collateral in
/// underlying, how many liabilities are left, and whether it has a position
/// at all.
///
/// `present` is what keeps "the filler has no position" from reading as a
/// finished unwind. An absent position satisfies every upper bound this test
/// has, and it is exactly the shape a filler that withdrew past its own
/// floor would leave behind — the failure most worth catching here, since
/// the floor is the operator's stated minimum rather than a preference.
#[derive(Debug, Clone, Copy)]
struct FillerPosition {
    collateral: i128,
    liabilities: usize,
    present: bool,
}

impl std::fmt::Display for FillerPosition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.present {
            write!(
                formatter,
                "{} liabilities and {} stroops of XLM collateral",
                self.liabilities, self.collateral
            )
        } else {
            write!(formatter, "no position in the pool at all")
        }
    }
}

impl FillerPosition {
    /// The unwind has finished: the debt it took on is repaid and the
    /// primary collateral is back at its floor — *at* it, not merely under
    /// the ceiling. [`MIN_PRIMARY_COLLATERAL`] is the pool's own
    /// `min_primary_collateral` and the planner rounds every withdrawal so
    /// the position never drops below it, so anything under that is a bug in
    /// the unwind rather than slack to be tolerated.
    fn settled(self) -> bool {
        self.present
            && self.liabilities == 0
            && self.collateral >= MIN_PRIMARY_COLLATERAL
            && self.collateral <= MAX_PRIMARY_COLLATERAL
    }
}

/// The filler's position, read from one pool snapshot.
///
/// The b-token amount is converted through the snapshot's own reserves,
/// accrued to now exactly as every task in the bot values a position: a
/// b-token count compared against an underlying floor would be the accrual
/// gotcha this crate warns about, in a test.
async fn filler_position(
    rpc: &RpcClient,
    pool: &str,
    filler: &str,
    xlm: &str,
) -> Result<FillerPosition, String> {
    let snapshot = PoolReader::new(rpc, pool)
        .snapshot(&[filler])
        .await
        .map_err(|error| format!("could not read the pool: {error}"))?;
    let Some(positions) = snapshot.positions.get(filler) else {
        return Ok(FillerPosition {
            collateral: 0,
            liabilities: 0,
            present: false,
        });
    };
    let liabilities = positions.liabilities.len();
    let Some(index) = snapshot.asset_index.get(xlm).copied() else {
        return Err(format!("{xlm} is not a reserve of {pool}"));
    };
    let Some(b_tokens) = positions.collateral.get(&index).copied() else {
        return Ok(FillerPosition {
            collateral: 0,
            liabilities,
            present: true,
        });
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let reserves = snapshot
        .accrued_reserves(now)
        .map_err(|error| format!("could not accrue the pool's reserves: {error}"))?;
    let Some(reserve) = reserves.get(&index) else {
        return Err(format!("the snapshot holds no reserve at index {index}"));
    };
    let collateral = reserve
        .to_asset_from_b_token(b_tokens)
        .map_err(|error| format!("could not convert the filler's b-tokens: {error}"))?;
    Ok(FillerPosition {
        collateral,
        liabilities,
        present: true,
    })
}

/// Waits until the unwind has left the filler with no liabilities and the
/// primary asset back inside [`MIN_PRIMARY_COLLATERAL`]..=[`MAX_PRIMARY_COLLATERAL`],
/// and answers what it holds.
async fn wait_for_unwind(
    bot: &mut Bot,
    rpc: &RpcClient,
    pool: &str,
    filler: &str,
    xlm: &str,
) -> i128 {
    let mut wait = Wait::new(
        "the unwind to repay the filler's debt and withdraw to the primary floor",
        UNWIND_TIMEOUT,
    );
    loop {
        // What this pass saw, carried only as far as the timeout message
        // below: a run that times out here has to say what the filler was
        // actually holding, or "the unwind never finished" is unfalsifiable.
        let seen = match filler_position(rpc, pool, filler, xlm).await {
            Ok(position) => {
                if position.settled() {
                    println!("the filler holds {position} at {:.1} s", bot.elapsed());
                    return position.collateral;
                }
                position.to_string()
            }
            Err(error) => error,
        };
        if !wait.tick(bot).await {
            let message = format!(
                "{} (last read: {seen}; expected 0 liabilities and {MIN_PRIMARY_COLLATERAL}..=\
                 {MAX_PRIMARY_COLLATERAL} stroops of XLM collateral)",
                wait.timed_out()
            );
            fail(bot, &message);
        }
    }
}

/// `/metrics`, once, before the bot is asked to stop.
async fn read_metrics(bot: &Bot, http: &reqwest::Client) -> String {
    let url = format!("http://127.0.0.1:{HTTP_PORT}/metrics");
    match http.get(&url).send().await {
        Ok(response) => match response.text().await {
            Ok(body) => body,
            Err(error) => fail(bot, &format!("could not read /metrics: {error}")),
        },
        Err(error) => fail(bot, &format!("could not reach /metrics: {error}")),
    }
}

/// One `NAME{labels} value` line's value, or `None` when the series is
/// absent.
fn series(metrics: &str, name: &str) -> Option<i64> {
    metrics.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?;
        let rest = rest.strip_prefix(' ')?;
        rest.trim().parse().ok()
    })
}

/// Asserts a counter is exactly `expected`.
///
/// Silent on success: [`assert_metrics`] has already printed every counter
/// the run moved, and repeating the ones it asserts would only make the
/// artefact harder to read. A failure names the series, what it held and
/// what was expected.
fn assert_counter(bot: &Bot, metrics: &str, name: &str, expected: i64) {
    match series(metrics, name) {
        Some(value) if value == expected => {}
        Some(value) => {
            let message = format!("{name} is {value}, expected {expected}");
            fail(bot, &message);
        }
        None => {
            let message = format!("/metrics has no {name} series");
            fail(bot, &message);
        }
    }
}

/// `SIGTERM`, then the exit status, which must be `0`: the bot's graceful
/// drain is what the deployment's own stop is, and a non-zero status here
/// would mean a task failed on the way out.
async fn terminate(bot: &mut Bot) {
    let Some(pid) = bot.child.as_ref().map(std::process::Child::id) else {
        fail(bot, "the bot was already reaped before SIGTERM");
    };
    println!("sending SIGTERM to pid {pid} at {:.1} s", bot.elapsed());
    match Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => fail(bot, &format!("kill -TERM answered {status}")),
        Err(error) => fail(bot, &format!("could not run kill -TERM: {error}")),
    }

    // `Wait::tick` cannot drive this loop: it fails on a child that has
    // already exited, which here is the success condition. So the budget
    // and the message come from `Wait` and the progress line is printed
    // here, on the same [`PROGRESS_INTERVAL`] — a bot that hangs on
    // SIGTERM must not give thirty seconds of silence and then a timeout.
    let wait = Wait::new("the bot to drain and exit", SHUTDOWN_TIMEOUT);
    let mut last_progress = Instant::now();
    loop {
        let exited = match bot.child.as_mut().map(std::process::Child::try_wait) {
            Some(Ok(status)) => status,
            Some(Err(error)) => fail(bot, &format!("could not poll the bot: {error}")),
            None => fail(bot, "the bot was reaped while waiting for it to exit"),
        };
        if let Some(status) = exited {
            // Taken so `Drop` has nothing to kill: the process is gone and
            // its status is what the assertion below is about.
            bot.child = None;
            println!("the bot exited {status} at {:.1} s", bot.elapsed());
            if status.code() != Some(0) {
                fail(bot, &format!("the bot exited {status}, expected 0"));
            }
            return;
        }
        if Instant::now() >= wait.deadline {
            let message = wait.timed_out();
            fail(bot, &message);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            let left = wait.deadline.saturating_duration_since(Instant::now());
            println!(
                "  still waiting for {} ({} s left of {} s)",
                wait.what,
                left.as_secs(),
                wait.budget.as_secs()
            );
        }
    }
}

/// Reads `sandbox.env` and refuses the run unless it names the standalone
/// network.
///
/// Both of the file's refusals live here, before the caller has created a
/// database or spawned anything: this test arms a bot with a real signing
/// key, and the only thing that makes that safe is the network it points
/// at. What the file *claims* is only half of that, so
/// [`require_standalone_rpc`] asks the node itself before the caller goes
/// any further.
fn sandbox_env(env_path: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(env_path) else {
        panic!(
            "{} does not exist — run scripts/sandbox/up.sh and scripts/sandbox/deploy.sh first",
            env_path.display()
        )
    };
    let env = parse_env_file(&text);
    let passphrase = required(&env, "SANDBOX_PASSPHRASE");
    assert_eq!(
        passphrase,
        STANDALONE_PASSPHRASE,
        "{} reports the network passphrase {passphrase:?}, not the sandbox's standalone one — \
         refusing to run an armed bot against a network that is not this sandbox's own",
        env_path.display()
    );
    env
}

/// Refuses the run unless the RPC at `url` answers `getNetwork` with
/// [`STANDALONE_PASSPHRASE`].
///
/// [`sandbox_env`] checks a *file*, which an edit or a stale deploy can make
/// say anything; this checks the node that the armed bot — `DRY_RUN=false`,
/// with a real signing key — is about to submit to. They are the same gate
/// `scripts/sandbox/lib.sh`'s `require_standalone_network` is on the shell
/// side, and this is the Rust side of it: whatever wrote `sandbox.env`, the
/// endpoint itself has to be this sandbox's own.
///
/// Every failure is a refusal, an unreachable RPC included: "could not ask"
/// is not "it is the sandbox". Called before the run's database is created
/// and long before anything is spawned, so a refusal here leaves nothing
/// behind.
async fn require_standalone_rpc(url: &str) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => panic!("could not build an HTTP client to check {url}: {error}"),
    };
    let request = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "getNetwork" });
    let response = match client.post(url).json(&request).send().await {
        Ok(response) => response,
        Err(error) => panic!(
            "{url} did not answer getNetwork ({error}) — refusing to arm the bot against an RPC \
             this test could not verify; is scripts/sandbox/up.sh's container running?"
        ),
    };
    let body = match response.json::<serde_json::Value>().await {
        Ok(body) => body,
        Err(error) => panic!("{url} answered getNetwork with something that is not JSON: {error}"),
    };
    let passphrase = body
        .get("result")
        .and_then(|result| result.get("passphrase"))
        .and_then(serde_json::Value::as_str);
    let Some(passphrase) = passphrase else {
        panic!("{url} answered getNetwork without a result.passphrase: {body}")
    };
    assert_eq!(
        passphrase, STANDALONE_PASSPHRASE,
        "{url} reports the network passphrase {passphrase:?}, not the sandbox's standalone one \
         — refusing to run an armed bot against a network that is not this sandbox's own"
    );
    println!("{url} answered getNetwork with the standalone passphrase");
}

/// Runs `scripts/sandbox/crash.sh`, which moves the oracle's XLM price and
/// is the one thing that makes the borrower liquidatable.
fn crash(bot: &Bot, root: &Path) {
    let script = root.join("scripts/sandbox/crash.sh");
    match Command::new(&script).current_dir(root).output() {
        Ok(output) if output.status.success() => {
            println!(
                "crash.sh: XLM is now {}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        Ok(output) => {
            let message = format!(
                "crash.sh failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            fail(bot, &message);
        }
        Err(error) => fail(bot, &format!("could not run {}: {error}", script.display())),
    }
}

/// The three series the run is judged by: exactly one creation and one fill
/// that landed, and at least one completed unwind pass.
///
/// Exactly one of each, not "at least": a second creation for the same
/// borrower would mean the first was lost, and a second fill would mean the
/// first took only part of the auction. Either is worth failing on.
fn assert_metrics(bot: &Bot, metrics: &str) {
    // Every counter the run actually moved, printed before anything is
    // asserted: a failure below is far easier to read next to the rest of
    // what the bot counted, and this is the excerpt a CI artefact keeps.
    println!("/metrics, the counters this run moved:");
    for line in metrics.lines() {
        let Some(series) = line.strip_prefix("blend_liquidator_") else {
            continue;
        };
        if !series.contains("_total") {
            continue;
        }
        match series.rsplit_once(' ') {
            Some((_, value)) if value != "0" => println!("  blend_liquidator_{series}"),
            _ => {}
        }
    }
    assert_counter(
        bot,
        metrics,
        "blend_liquidator_creations_total{result=\"succeeded\"}",
        1,
    );
    assert_counter(
        bot,
        metrics,
        "blend_liquidator_fills_total{result=\"succeeded\"}",
        1,
    );
    let passes = match series(metrics, "blend_liquidator_unwind_passes_total") {
        Some(passes) if passes >= 1 => passes,
        Some(passes) => {
            let message = format!("unwind_passes_total is {passes}, expected at least 1");
            fail(bot, &message);
        }
        None => fail(bot, "/metrics has no unwind_passes_total series"),
    };
    println!("asserted: one creation and one fill that landed, {passes} unwind passes");
}

/// The whole tier: an armed bot creates the auction, fills it and unwinds
/// the position it took, against a network that exists only for this run.
///
/// Ignored on purpose — it needs `scripts/sandbox/up.sh` and
/// `scripts/sandbox/deploy.sh` to have run, and a Postgres at `DATABASE_URL`.
#[tokio::test]
#[ignore = "needs the local sandbox network: scripts/sandbox/up.sh && scripts/sandbox/deploy.sh"]
async fn liquidation_end_to_end() {
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
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let database = format!("sandbox_{stamp}");
    println!("creating the run's database {database}");
    let database_url = create_run_database(&root, &maintenance_url, &database).await;
    // Set once the database exists, so every failure from here on says it
    // was kept; the success path at the bottom drops it and it is never
    // read again.
    let _ = RUN_DATABASE.set(database.clone());

    let sandbox_dir = root.join("target/sandbox");
    let seed_path = sandbox_dir.join("seed.toml");
    let seed = format!("[accounts]\n\"{pool}\" = [\"{borrower}\"]\n");
    if let Err(error) = std::fs::write(&seed_path, seed) {
        panic!("could not write {}: {error}", seed_path.display());
    }

    let log_path = sandbox_dir.join("bot.log");
    let pools = pools_toml(&pool, &xlm, &usdc);
    let mut bot = spawn_bot(&log_path, &database_url, &env, &pools, &seed_path);

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
