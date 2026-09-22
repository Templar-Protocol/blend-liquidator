//! Shared machinery for the sandbox tier's scenario tests: spawning the real
//! `liquidator` binary, **armed**, against the local network
//! `scripts/sandbox/up.sh` starts and `scripts/sandbox/deploy.sh` populates.
//!
//! This is the one place in the repository the bot runs with `DRY_RUN=false`
//! and a signing key, so almost everything here is about making that safe
//! rather than convenient:
//!
//! - [`sandbox_env`] refuses to run at all unless `target/sandbox/sandbox.env`
//!   exists (the message names the two scripts that write it), and refuses
//!   unless that file's `SANDBOX_PASSPHRASE` is the standalone network's;
//! - [`require_standalone_rpc`] refuses unless the RPC at that file's
//!   `SANDBOX_RPC_URL` answers `getNetwork` with the same passphrase. The
//!   file is a claim; the node is the fact, and only the second of those
//!   decides which network a signing key is handed to. Both refusals fire
//!   before a database is created or anything is spawned, so a
//!   misconfigured run can never point an armed bot at a real network.
//!
//! **Nothing in this module may panic through `unwrap`/`expect`.** Every
//! failure after a bot is spawned goes through [`fail`], which prints the
//! tail of its log before it panics: a run that dies without that tail is a
//! run nobody can diagnose. `panic!` directly is for refusals that happen
//! before there is a bot or a log at all — [`sandbox_env`],
//! [`require_standalone_rpc`] and the other pre-spawn helpers below.
//!
//! A scenario creates its own database, or several — a restart spans two
//! bot instances and needs a fresh one for each, which is why
//! [`RUN_DATABASE`] holds a list rather than one name.
//! [`create_run_database`] creates and migrates it, [`note_run_database`]
//! tells [`fail`] to mention it if the run dies, and
//! [`record_run_database`]/[`forget_run_database`] keep
//! `target/sandbox/run-databases` in sync so `make sandbox-down` can sweep
//! whatever a failed run left behind. A run that passes drops every
//! database it created; a run that fails keeps them all, because they are
//! then the only durable record of what the bot decided.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blend_liquidator::chain::xdr::AuctionType;
use blend_liquidator::chain::{PoolReader, RpcClient};
use blend_liquidator::math::AuctionData;
use blend_liquidator::store::{Store, TrackedAuction};
use sqlx::postgres::PgPool;

/// The only network this tier will talk to. `scripts/sandbox/versions.env`
/// holds the same literal, and every sandbox script checks the RPC's own
/// `getNetwork` against it; this check is the same gate on the Rust side, so
/// an armed bot cannot be spawned against anything else however
/// `sandbox.env` was produced.
const STANDALONE_PASSPHRASE: &str = "Standalone Network ; February 2017";

/// The tier's five scenarios, in the order `scripts/sandbox/deploy.sh`
/// refuses anything outside of and the Makefile's `sandbox` target loops
/// over. `deploy.sh` holds this same list for its own `SANDBOX_SCENARIO`
/// refusal; the two are not derived from one another — a shell script and a
/// Rust test share no build step that could — so a sixth scenario is added
/// to both by hand.
pub(crate) const SCENARIOS: [&str; 5] = [
    "liquidation",
    "check_config",
    "dry_run",
    "unwind_repay",
    "restart_adopt",
];

/// How long a bot has to bind its HTTP port and report ready. Readiness
/// needs a store ping and one processed ledger per pool, so it covers
/// migration, the seed and the first poll.
const HEALTHY_TIMEOUT: Duration = Duration::from_mins(1);

/// How long the auctioneer has to create an auction after a crash: one
/// oracle scan (`ORACLE_SCAN_LEDGERS=5`) or full scan (`FULL_SCAN_LEDGERS=10`)
/// to flag the borrower, then the percent walk, then the submission.
pub(crate) const CREATION_TIMEOUT: Duration = Duration::from_secs(90);

/// How many ledgers the filler has to wait for before it can take the
/// auction at a profit, in the `liquidation` scenario's own economics.
///
/// This is the one budget the auction's own arithmetic sets rather than the
/// bot's, and it is counted in *ledgers* because that is what the contract
/// counts: the lot ramps linearly to full over the auction's first 200
/// ledgers while the bid stays whole, so the earliest ledger at which the
/// lot covers the bid plus the configured margin is `200 × bid_value /
/// lot_value_at_full`. The figures are a real run's, not an illustration:
/// the auctioneer created that scenario's auction at 69%, a bid of 207 USDC
/// against a full lot of ~3,139 XLM worth ~$235 at the crashed price, and
/// with the pool's 100 bps margin the fill landed at block 178. Both sides
/// scale with the percent, so the break-even block is near-invariant across
/// the band the percent walk lands in. Nothing shortens it: `force_fill`
/// caps the target at 350 ledgers, which is later, not sooner, and this is
/// the earliest-profitable break-even specifically — [`pools_toml`] pins
/// `fill_objective` to `earliest-profitable` for exactly that reason, since
/// the crate's own default, `free-fill`, aims at `start + 400` instead,
/// well outside this budget. 190 is 178 with room for a re-plan.
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

/// How long the filler's position has to show at least one liability once
/// the fill's transaction hash is on the audit row. The position update is
/// part of the same transaction, so — like the fill row itself — this is
/// slack for a chain read to catch up, not a real wait.
const LIABILITY_TIMEOUT: Duration = Duration::from_mins(1);

/// How long `unwind_repay`'s first bot has to reach an unwind pass that is
/// idle with debt still outstanding. `plan_unwind`'s step-3 withdrawal is
/// sized by exact projection in one call, so an empty wallet converges in a
/// handful of landed passes rather than many, but each landed pass is still
/// a real chain submission — a repay or a withdrawal, prepared, signed and
/// sent — so this stays in the same range as [`UNWIND_TIMEOUT`]'s own
/// budget rather than the near-instant [`LIABILITY_TIMEOUT`] above.
const LEFTOVERS_TIMEOUT: Duration = Duration::from_mins(3);

/// How long the bot has to drain and exit after `SIGTERM`.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a `RUN_MODE=check-config` run has to exit on its own — a read
/// of the chain and a store ping, never a follow — before
/// [`run_check_config`] kills it and fails the case rather than waiting
/// out the ordinary bot timeouts above, which this mode never approaches.
const CHECK_CONFIG_TIMEOUT: Duration = Duration::from_mins(1);

/// Every wait polls at this cadence.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often a wait reprints what it is still waiting for, so a hung run is
/// diagnosable from `--nocapture` without waiting for the timeout.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// How much of the bot's log a failure prints.
const LOG_TAIL_LINES: usize = 100;

/// The port a bot serves `/healthz` and `/metrics` on. Not 8080: the
/// devcontainer forwards that, and a collision would look like a bot that
/// never became ready. Bots run one at a time — this and every scenario
/// exits one before it spawns the next — so every scenario shares it.
const HTTP_PORT: u16 = 18080;

/// `min_primary_collateral` for [`pools_toml`], in stroops: 100 XLM.
const MIN_PRIMARY_COLLATERAL: i128 = 1_000_000_000;

/// The most XLM collateral a filler may still hold once an unwind has
/// finished, in stroops: [`MIN_PRIMARY_COLLATERAL`] plus one percent.
///
/// The floor is exact in *underlying*, but a withdrawal is sized in b-tokens
/// and the planner rounds the burn up so the position never drops under the
/// floor; the one percent is that rounding, not slack in the assertion.
const MAX_PRIMARY_COLLATERAL: i128 = 1_010_000_000;

/// The bot, and everything a failure needs to say what it was doing.
///
/// `Drop` kills the child, so a panic anywhere in a scenario cannot leave an
/// armed bot running against the sandbox — the ordinary path takes the
/// child out with [`terminate`] (or [`Bot::kill`], for a scenario that
/// means to crash it) first and leaves `Drop` nothing to do.
pub(crate) struct Bot {
    child: Option<Child>,
    log: PathBuf,
    started: Instant,
}

impl Drop for Bot {
    fn drop(&mut self) {
        self.kill();
    }
}

impl Bot {
    /// Sends `SIGKILL`, waits for the child to exit, and takes it, so
    /// nothing is left for `Drop` to do. `Drop` itself calls this; a
    /// scenario that means to simulate a crash (a restart, adopting an
    /// auction the killed bot never finished) may call it directly too.
    pub(crate) fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            println!("killing the bot (pid {}) after a failure", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Whether this struct still holds a child handle. `kill`'s own
    /// `Option::take` clears it unconditionally, and `kill` blocks on
    /// `Child::wait` before returning — which does not answer until the
    /// process has actually exited — so by the time `kill` returns this is
    /// always `false`; the `restart_adopt` scenario still checks it
    /// explicitly, as its own proof that the first bot it spawns cannot
    /// still be running by the time the second one starts against the same
    /// auction.
    pub(crate) fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// Seconds since the bot was spawned, for the run's timeline.
    pub(crate) fn elapsed(&self) -> f64 {
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

/// This run's databases, once they exist, so [`fail`] can say they were
/// kept.
///
/// A `Mutex<Vec<String>>` rather than one name: a scenario may create more
/// than one database across the bots it spawns (a restart, onto a fresh
/// database, is the reason). A `static` rather than a field of [`Bot`]
/// because it outlives any one bot — the failures worth inspecting a
/// database for include the ones that happen after every bot in the run is
/// gone.
static RUN_DATABASE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Adds `name` to [`RUN_DATABASE`], so [`fail`] mentions it if the run dies.
///
/// A poisoned lock is recovered rather than propagated: nothing in this
/// crate may panic through anything but [`fail`] itself, and a poisoned
/// mutex is not a reason to skip recording a database that exists.
pub(crate) fn note_run_database(name: &str) {
    match RUN_DATABASE.lock() {
        Ok(mut guard) => guard.push(name.to_string()),
        Err(poisoned) => poisoned.into_inner().push(name.to_string()),
    }
}

/// Prints the tail of the bot's log and panics.
///
/// Every failure after a bot is spawned goes through here. The tail comes
/// first and on the same stream as the rest of the test's output, so a CI
/// log reads in the order things happened.
pub(crate) fn fail(bot: &Bot, message: &str) -> ! {
    println!(
        "\n--- last {LOG_TAIL_LINES} lines of {} ---",
        bot.log.display()
    );
    println!("{}", bot.tail());
    println!("--- end of {} ---\n", bot.log.display());
    let databases = match RUN_DATABASE.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    for database in &databases {
        println!(
            "the database {database} is kept for inspection — it is listed in {RUN_DATABASES}, \
             and `make sandbox-down` drops what that file names\n"
        );
    }
    panic!("{message}");
}

/// Prints `output` and panics, for a `check-config` case.
///
/// [`fail`] tails a running bot's log file; a `check-config` case has no
/// [`Bot`] at all — it is a short-lived process that has already exited by
/// the time a case can fail its assertion — so this prints the captured
/// stdout/stderr [`run_check_config`] returned instead, and otherwise
/// mirrors `fail`'s "say what is kept before panicking" shape, including
/// this run's databases, since a failing case is exactly the kind of
/// failure that database was created to be inspected for.
pub(crate) fn fail_check(output: &str, message: &str) -> ! {
    println!("\n--- check-config output ---");
    println!("{output}");
    println!("--- end of check-config output ---\n");
    let databases = match RUN_DATABASE.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    for database in &databases {
        println!(
            "the database {database} is kept for inspection — it is listed in {RUN_DATABASES}, \
             and `make sandbox-down` drops what that file names\n"
        );
    }
    panic!("{message}");
}

/// One named wait: a budget, a 500 ms poll, and a line saying what it is for.
///
/// Every wait in this tier is one of these, so a hung run names what it was
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
pub(crate) fn repo_root() -> PathBuf {
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
pub(crate) fn required<'a>(env: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    match env.get(key) {
        Some(value) if !value.is_empty() => value,
        _ => panic!(
            "target/sandbox/sandbox.env does not define {key} — re-run scripts/sandbox/deploy.sh"
        ),
    }
}

/// `url` with its database replaced by `name`, keeping user, host, port and
/// any query string.
pub(crate) fn with_database(url: &str, name: &str) -> String {
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
pub(crate) async fn create_run_database(root: &Path, maintenance_url: &str, name: &str) -> String {
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

/// Creates this run's database **without** migrating it, answering its URL.
///
/// [`create_run_database`] migrates so the row polls it feeds treat a query
/// error as real; `check_config` asserts the opposite — that `check-config`
/// itself never migrates — so its database must be left exactly as a plain
/// `CREATE DATABASE` leaves it: no `_sqlx_migrations` table, nothing else
/// either. Otherwise identical: the same identifier audit
/// ([`AssertSqlSafe`](sqlx::AssertSqlSafe) on a name this process built, a
/// digit string it was never handed), and [`record_run_database`] before
/// anything past the `CREATE` could fail and leave an unrecorded database
/// behind.
pub(crate) async fn create_run_database_unmigrated(
    root: &Path,
    maintenance_url: &str,
    name: &str,
) -> String {
    let maintenance = match PgPool::connect(maintenance_url).await {
        Ok(pool) => pool,
        Err(error) => panic!(
            "could not connect to DATABASE_URL to create this run's database: {error} — is \
             `make db-up` running?"
        ),
    };
    let statement = format!("CREATE DATABASE \"{name}\"");
    if let Err(error) = sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
        .execute(&maintenance)
        .await
    {
        panic!("could not create the database {name}: {error}");
    }
    maintenance.close().await;
    record_run_database(root, name);
    with_database(maintenance_url, name)
}

/// Drops this run's database, on the success path only.
///
/// Called after the bot has exited and this test's own pool is closed:
/// Postgres refuses to drop a database anything is still connected to.
/// A failure to drop only warns — the run itself has already passed, and
/// turning a leaked database name into a red test would say something false
/// about the bot; `make sandbox-down` sweeps whatever is left.
pub(crate) async fn drop_run_database(root: &Path, maintenance_url: &str, name: &str) {
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

/// The `POOLS_TOML` a bot follows: one pool, `bid` as `supported_bid`, any
/// lot, the primary asset floor an unwind is asserted against.
pub(crate) fn pools_toml(pool: &str, xlm: &str, bid: &[&str]) -> String {
    let supported_bid = bid
        .iter()
        .map(|asset| format!("\"{asset}\""))
        .collect::<Vec<_>>()
        .join(", ");
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
         supported_bid = [{supported_bid}]\n\
         supported_lot = [\"*\"]\n"
    )
}

/// What varies from one bot spawn to the next. What is common to every bot
/// in this tier — the network's passphrase and RPC URL (read from
/// `sandbox.env`), the HTTP port, the poll cadence — stays a [`spawn_bot`]
/// default, since every scenario reads the first from the same file and
/// wants the rest unchanged unless it says otherwise.
pub(crate) struct BotConfig<'a> {
    pub(crate) dry_run: bool,
    pub(crate) filler_secret: Option<&'a str>,
    pub(crate) pools: String,
    pub(crate) database_url: String,
    /// A file name under `target/sandbox/`, not a full path.
    pub(crate) log_name: &'a str,
    /// Environment variables applied after every default above, so a
    /// scenario can override one (a non-default `XLM_FEE_RESERVE`) or add
    /// one the defaults do not set at all (`SEED_FILE`, which is not every
    /// scenario's to set the same way).
    pub(crate) extra_env: Vec<(&'a str, String)>,
}

/// Spawns the binary with `env_clear` and exactly the environment below,
/// `config.extra_env` layered on top.
///
/// `PATH` is the only variable carried over from this process, and the bot
/// does not need even that: it is exec'd by absolute path, reaches the RPC
/// over plain HTTP and Postgres over TCP, and runs no subprocess. It is
/// passed anyway, because a binary that one day shells out and finds no
/// `PATH` is a puzzling thing to debug. `HOME` is deliberately *not*
/// passed: nothing the bot reads lives there, and a test that handed it one
/// would be hiding a dependency on the developer's machine.
pub(crate) fn spawn_bot(root: &Path, env: &BTreeMap<String, String>, config: BotConfig<'_>) -> Bot {
    // Destructured, not borrowed field-by-field: every field below is
    // consumed once, so taking `config` by value (a plain struct, not a
    // reference) has somewhere to go.
    let BotConfig {
        dry_run,
        filler_secret,
        pools,
        database_url,
        log_name,
        extra_env,
    } = config;

    let log_path = root.join("target/sandbox").join(log_name);
    let log = match std::fs::File::create(&log_path) {
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
        // Set explicitly either way, rather than left to the default when
        // true: a scenario that means to prove a dry run never signs must
        // not depend on `DRY_RUN`'s own default staying `true`.
        .env("DRY_RUN", if dry_run { "true" } else { "false" })
        .env("POOLS_TOML", pools)
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
    if let Some(secret) = filler_secret {
        command.env("FILLER_SECRET_KEY", secret);
    }
    for (key, value) in &extra_env {
        command.env(key, value);
    }

    match command.spawn() {
        Ok(child) => {
            println!(
                "spawned the bot (pid {}), logging to {}",
                child.id(),
                log_path.display()
            );
            Bot {
                child: Some(child),
                log: log_path,
                started: Instant::now(),
            }
        }
        Err(error) => panic!("could not spawn the liquidator binary: {error}"),
    }
}

/// Spawns the binary with `RUN_MODE=check-config`, waits up to
/// [`CHECK_CONFIG_TIMEOUT`] for it to exit — killing it if that budget
/// runs out — and answers its exit status and its combined stdout and
/// stderr.
///
/// There is no [`Bot`] here: `check-config` reads the chain, pings the
/// database and exits — it serves no HTTP port and outlives no wait this
/// module already has a shape for — so each call is its own short-lived
/// process rather than the one long-running bot the rest of this module
/// spawns, and [`fail_check`] is its own failure path over the captured
/// output rather than a log file.
///
/// `env_clear`, exactly the chain variables [`spawn_bot`] sets
/// (`NETWORK_PASSPHRASE`, `RPC_URL`, `PATH`) plus `RUN_MODE=check-config`
/// and no `PORT` — a `check-config` run has nothing to serve. The signing
/// key goes through [`Command::env`] only, never argv, the same rule
/// [`spawn_bot`] keeps.
pub(crate) fn run_check_config(
    env: &BTreeMap<String, String>,
    database_url: &str,
    pools: &str,
    dry_run: bool,
    filler_secret: Option<&str>,
    extra_env: &[(&str, String)],
) -> (std::process::ExitStatus, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_liquidator"));
    command
        .env_clear()
        .env("DATABASE_URL", database_url)
        .env("NETWORK_PASSPHRASE", required(env, "SANDBOX_PASSPHRASE"))
        .env("RPC_URL", required(env, "SANDBOX_RPC_URL"))
        .env("DRY_RUN", if dry_run { "true" } else { "false" })
        .env("POOLS_TOML", pools)
        .env("SEED_URL", "")
        .env("RUN_MODE", "check-config")
        .env("LOG_FORMAT", "json")
        .env("RUST_LOG", "info,blend_liquidator=debug")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(secret) = filler_secret {
        command.env("FILLER_SECRET_KEY", secret);
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => panic!("could not spawn the liquidator binary for check-config: {error}"),
    };

    // Read stdout and stderr on their own threads from the moment the
    // process starts: `check-config` runs under `LOG_FORMAT=json`, which
    // can print more than a pipe's buffer holds, and a `wait` performed
    // without draining both pipes concurrently can deadlock against a
    // child still writing to the one nobody is reading.
    let Some(stdout) = child.stdout.take() else {
        panic!("check-config's child had no stdout pipe");
    };
    let Some(stderr) = child.stderr.take() else {
        panic!("check-config's child had no stderr pipe");
    };
    let stdout_reader = std::thread::spawn(move || std::io::read_to_string(stdout));
    let stderr_reader = std::thread::spawn(move || std::io::read_to_string(stderr));

    let deadline = Instant::now() + CHECK_CONFIG_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => panic!("could not poll the check-config run: {error}"),
        }
        if Instant::now() >= deadline {
            println!(
                "check-config did not exit within {} s — killing it",
                CHECK_CONFIG_TIMEOUT.as_secs()
            );
            timed_out = true;
            let _ = child.kill();
            break match child.wait() {
                Ok(status) => status,
                Err(error) => panic!("could not reap the killed check-config run: {error}"),
            };
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let stdout_text = match stdout_reader.join() {
        Ok(Ok(text)) => text,
        Ok(Err(error)) => format!("(could not read stdout: {error})"),
        Err(_) => "(the stdout reader thread panicked)".to_string(),
    };
    let stderr_text = match stderr_reader.join() {
        Ok(Ok(text)) => text,
        Ok(Err(error)) => format!("(could not read stderr: {error})"),
        Err(_) => "(the stderr reader thread panicked)".to_string(),
    };
    let combined = format!("{stdout_text}{stderr_text}");
    if timed_out {
        // A budget failure, not one of the six cases' own assertions, so
        // it goes through `fail_check` here rather than handing the
        // caller a status no case expects.
        fail_check(
            &combined,
            &format!(
                "check-config did not exit within {} s",
                CHECK_CONFIG_TIMEOUT.as_secs()
            ),
        );
    }
    (status, combined)
}

/// Waits for `/healthz` to answer 200.
pub(crate) async fn wait_for_ready(bot: &mut Bot, http: &reqwest::Client) {
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
pub(crate) async fn fill_budget(bot: &mut Bot, rpc: &RpcClient) -> Duration {
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
pub(crate) const CREATION_TX_HASH: &str = "SELECT tx_hash FROM creations \
     WHERE pool = $1 AND account = $2 AND dry_run = false AND tx_hash IS NOT NULL LIMIT 1";

/// The filler's, the same shape.
pub(crate) const FILL_TX_HASH: &str = "SELECT tx_hash FROM fills \
     WHERE pool = $1 AND account = $2 AND dry_run = false AND tx_hash IS NOT NULL LIMIT 1";

/// Waits for `statement` to answer a transaction hash, and answers it.
///
/// A row with a hash is the audit trail's evidence that a transaction was
/// named on chain; a row without one is an attempt that never reached the
/// network, which is exactly what this must not accept.
pub(crate) async fn wait_for_tx_hash(
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

/// Waits for `pool`'s store to hold an `auctions` row for `account`, and
/// answers it.
///
/// `restart_adopt`'s own wait: adoption is a read-then-upsert of the
/// chain's own entry (`Auctioneer::adopt`/`Store::upsert_auction`), not a
/// submission, so there is no transaction hash to wait for the way
/// [`wait_for_tx_hash`] does — the row itself, appearing in a database that
/// started with none, is the evidence.
pub(crate) async fn wait_for_adopted_auction(
    bot: &mut Bot,
    store: &Store,
    what: &'static str,
    budget: Duration,
    pool: &str,
    account: &str,
) -> TrackedAuction {
    let mut wait = Wait::new(what, budget);
    loop {
        match store
            .auction(pool, account, AuctionType::UserLiquidation)
            .await
        {
            Ok(Some(auction)) => {
                println!(
                    "{what} at {:.1} s (start ledger {}, bid {:?}, lot {:?})",
                    bot.elapsed(),
                    auction.start_ledger,
                    auction.bid,
                    auction.lot
                );
                return auction;
            }
            Ok(None) => {}
            Err(error) => fail(
                bot,
                &format!("could not read the store's auction row: {error}"),
            ),
        }
        if !wait.tick(bot).await {
            let message = wait.timed_out();
            fail(bot, &message);
        }
    }
}

/// Asserts `statement` (one of the `*_TX_HASH` queries above) finds no row
/// for `(pool, account)` at all.
///
/// Stronger than the `*_VIOLATING_DRY_RUN` counts above, which only rule
/// out an armed or hashed row alongside others that carry neither:
/// `restart_adopt` needs no row whatsoever, because `Auctioneer::act`
/// answers `ActOutcome::Refused` — see `refuse_percent` in
/// `src/auctioneer.rs` — before `record_creation` is ever called, so a
/// refusal that adopts an auction never writes a `creations` row for the
/// attempt at all.
pub(crate) async fn assert_no_tx_hash_row(
    bot: &Bot,
    store: &Store,
    statement: &'static str,
    what: &'static str,
    pool: &str,
    account: &str,
) {
    match sqlx::query_scalar::<_, String>(statement)
        .bind(pool)
        .bind(account)
        .fetch_optional(store.pool())
        .await
    {
        Ok(None) => println!("asserted: no {what} row carries a transaction hash"),
        Ok(Some(hash)) => fail(
            bot,
            &format!("a {what} row carries a transaction hash ({hash}) — none should exist"),
        ),
        Err(error) => fail(bot, &format!("could not check {what} rows: {error}")),
    }
}

/// The first dry-run row this scenario's borrower has in `creations`, with
/// the ledger the decision was made at. Ordered oldest first: a dry run
/// that keeps deciding (the borrower stays flagged for as long as no
/// auction ever lands) writes a new row every pass, and the first one is
/// the evidence the `dry_run` scenario's phase A waits for.
pub(crate) const DRY_RUN_CREATION_ROW: &str = "SELECT tx_hash, ledger FROM creations \
     WHERE pool = $1 AND account = $2 AND dry_run = true ORDER BY id ASC LIMIT 1";

/// The same shape for `fills`.
pub(crate) const DRY_RUN_FILL_ROW: &str = "SELECT tx_hash, fill_ledger FROM fills \
     WHERE pool = $1 AND account = $2 AND dry_run = true ORDER BY id ASC LIMIT 1";

/// Counts every `creations` row for this account that is *not* what a dry
/// run may ever write: armed (`dry_run = false`) or carrying a transaction
/// hash. Zero is the only value the `dry_run` scenario's phase A accepts.
pub(crate) const CREATIONS_VIOLATING_DRY_RUN: &str =
    "SELECT count(*) FROM creations WHERE pool = $1 AND account = $2 \
     AND (dry_run = false OR tx_hash IS NOT NULL)";

/// The same shape for `fills`.
pub(crate) const FILLS_VIOLATING_DRY_RUN: &str =
    "SELECT count(*) FROM fills WHERE pool = $1 AND account = $2 \
     AND (dry_run = false OR tx_hash IS NOT NULL)";

/// Waits for `statement`'s row to exist and answers its transaction hash
/// (`None` for a dry run that never submitted) and the ledger the row
/// names.
///
/// Shared by the `dry_run` scenario's two audit tables (`creations`'
/// `ledger`, `fills`' `fill_ledger`): both are one bigint alongside the
/// hash, and the whole assertion there is "a row exists and its hash is
/// null", which needs nothing table-specific beyond the query text.
pub(crate) async fn wait_for_dry_run_row(
    bot: &mut Bot,
    store: &Store,
    statement: &'static str,
    what: &'static str,
    budget: Duration,
    pool: &str,
    account: &str,
) -> (Option<String>, i64) {
    let mut wait = Wait::new(what, budget);
    loop {
        let found = sqlx::query_as::<_, (Option<String>, i64)>(statement)
            .bind(pool)
            .bind(account)
            .fetch_optional(store.pool())
            .await;
        match found {
            Ok(Some((tx_hash, ledger))) => {
                println!(
                    "{what} at {:.1} s (ledger {ledger}, tx_hash {tx_hash:?})",
                    bot.elapsed()
                );
                return (tx_hash, ledger);
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

/// Asserts `statement` — one of the `*_VIOLATING_DRY_RUN` counts above —
/// answers zero: every row this account's table holds was written in dry
/// run and never carried a transaction hash, however many times a pass
/// decided.
pub(crate) async fn assert_all_dry_run(
    bot: &Bot,
    store: &Store,
    statement: &'static str,
    what: &'static str,
    pool: &str,
    account: &str,
) {
    let violations = sqlx::query_scalar::<_, i64>(statement)
        .bind(pool)
        .bind(account)
        .fetch_one(store.pool())
        .await;
    match violations {
        Ok(0) => println!("asserted: every {what} row is dry_run = true and tx_hash IS NULL"),
        Ok(count) => fail(
            bot,
            &format!(
                "{count} {what} row(s) are armed or carry a transaction hash — a dry run must \
                 never submit"
            ),
        ),
        Err(error) => fail(bot, &format!("could not check {what} rows: {error}")),
    }
}

/// Asserts no open auction entry exists on chain for `user`, right now.
///
/// Not a wait: an entry existing at all is the failure this is for, so
/// there is nothing to wait out — the `dry_run` scenario calls this once
/// immediately after a dry-run creation and once after the chain has
/// moved, never in a loop.
pub(crate) async fn assert_no_auction(
    bot: &Bot,
    rpc: &RpcClient,
    pool: &str,
    user: &str,
    auction_type: AuctionType,
    context: &str,
) {
    let reader = PoolReader::new(rpc, pool);
    match reader.auction(user, auction_type).await {
        Ok(None) => println!("asserted: no auction entry on chain for {user} ({context})"),
        Ok(Some((ledger, data))) => fail(
            bot,
            &format!(
                "{context}: an auction entry exists on chain for {user} at ledger {ledger} \
                 (bid {:?}, lot {:?}) — a dry run must never create one",
                data.bid, data.lot
            ),
        ),
        Err(error) => fail(
            bot,
            &format!("{context}: could not read the auction entry: {error}"),
        ),
    }
}

/// Waits for `PoolReader::auction` to answer `Some` for `(pool, user,
/// auction_type)`, and answers it with the ledger it was read at.
pub(crate) async fn wait_for_auction(
    bot: &mut Bot,
    rpc: &RpcClient,
    pool: &str,
    user: &str,
    auction_type: AuctionType,
    what: &'static str,
    budget: Duration,
) -> (u32, AuctionData) {
    let reader = PoolReader::new(rpc, pool);
    let mut wait = Wait::new(what, budget);
    loop {
        match reader.auction(user, auction_type).await {
            Ok(Some((ledger, data))) => {
                println!(
                    "{what} at {:.1} s (ledger {ledger}, start block {}, bid {:?}, lot {:?})",
                    bot.elapsed(),
                    data.block,
                    data.bid,
                    data.lot
                );
                return (ledger, data);
            }
            Ok(None) => {}
            Err(error) => fail(bot, &format!("could not read the auction entry: {error}")),
        }
        if !wait.tick(bot).await {
            let message = wait.timed_out();
            fail(bot, &message);
        }
    }
}

/// Asserts the auction still on chain for `user` carries the same bid and
/// lot as `expected` — the evidence a dry-run filler never touched it.
pub(crate) async fn assert_auction_unchanged(
    bot: &Bot,
    rpc: &RpcClient,
    pool: &str,
    user: &str,
    auction_type: AuctionType,
    expected: &AuctionData,
    context: &str,
) {
    let reader = PoolReader::new(rpc, pool);
    match reader.auction(user, auction_type).await {
        Ok(Some((ledger, data))) if data.bid == expected.bid && data.lot == expected.lot => {
            println!(
                "asserted: the auction at ledger {ledger} still has the same bid and lot \
                 ({context})"
            );
        }
        Ok(Some((ledger, data))) => fail(
            bot,
            &format!(
                "{context}: the auction's bid or lot changed at ledger {ledger} — expected bid \
                 {:?} lot {:?}, found bid {:?} lot {:?}",
                expected.bid, expected.lot, data.bid, data.lot
            ),
        ),
        Ok(None) => fail(
            bot,
            &format!("{context}: the auction entry is gone — a dry run must never fill one"),
        ),
        Err(error) => fail(
            bot,
            &format!("{context}: could not read the auction entry: {error}"),
        ),
    }
}

/// Waits until the chain has closed at least `count` ledgers past `since`.
pub(crate) async fn wait_for_ledgers_past(
    bot: &mut Bot,
    rpc: &RpcClient,
    since: u32,
    count: u32,
    what: &'static str,
    budget: Duration,
) -> u32 {
    let target = since.saturating_add(count);
    let mut wait = Wait::new(what, budget);
    loop {
        match rpc.latest_ledger().await {
            Ok(latest) if latest.sequence >= target => {
                println!(
                    "{what}: now at ledger {} (target {target}) at {:.1} s",
                    latest.sequence,
                    bot.elapsed()
                );
                return latest.sequence;
            }
            Ok(_) => {}
            Err(error) => fail(bot, &format!("could not read the latest ledger: {error}")),
        }
        if !wait.tick(bot).await {
            let message = wait.timed_out();
            fail(bot, &message);
        }
    }
}

/// What a filler holds in the pool: its primary-asset collateral in
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
pub(crate) async fn wait_for_unwind(
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

/// Waits until the filler holds at least one liability in `pool`, and
/// answers how many.
///
/// `unwind_repay`'s counterpart to [`wait_for_unwind`]'s opposite
/// condition: its first run's fill leaves the bid asset unrepaid because
/// the wallet holds none of it, and this is the proof that debt actually
/// landed on the filler's own position before the scenario goes looking
/// for the `UnwindLeftovers` alert that follows.
pub(crate) async fn wait_for_liability(
    bot: &mut Bot,
    rpc: &RpcClient,
    pool: &str,
    filler: &str,
    xlm: &str,
) -> usize {
    let mut wait = Wait::new(
        "the filler's position to hold at least one liability",
        LIABILITY_TIMEOUT,
    );
    loop {
        let seen = match filler_position(rpc, pool, filler, xlm).await {
            Ok(position) => {
                if position.liabilities >= 1 {
                    println!("the filler holds {position} at {:.1} s", bot.elapsed());
                    return position.liabilities;
                }
                position.to_string()
            }
            Err(error) => error,
        };
        if !wait.tick(bot).await {
            let message = format!(
                "{} (last read: {seen}; expected at least one liability)",
                wait.timed_out()
            );
            fail(bot, &message);
        }
    }
}

/// The metric series [`NotificationKind::UnwindLeftovers`] renders as, at
/// the `LogChannel` delivery every unconfigured or failed-send channel
/// falls back to — this sandbox tier configures no Telegram credentials,
/// so `queued` here means exactly what `src/notifier.rs` says it means:
/// the notifier accepted the entry and spawned its delivery, not that any
/// particular channel confirmed it.
///
/// [`NotificationKind::UnwindLeftovers`]: blend_liquidator::notifier::NotificationKind::UnwindLeftovers
const UNWIND_LEFTOVERS_METRIC: &str =
    "blend_liquidator_notifications_total{kind=\"unwind_leftovers\",delivery=\"queued\"}";

/// Waits until [`UNWIND_LEFTOVERS_METRIC`] reaches exactly `1` on
/// `/metrics` — the one signal in this scenario that nothing on chain or
/// in the store proves ahead of the notifier's own counter: a
/// notification leaves no audit row of its own, so `/metrics` is the only
/// place this evidence exists at all.
pub(crate) async fn wait_for_unwind_leftovers(bot: &mut Bot, http: &reqwest::Client) -> i64 {
    let mut wait = Wait::new(
        "the UnwindLeftovers notification to be queued once",
        LEFTOVERS_TIMEOUT,
    );
    loop {
        let metrics = read_metrics(bot, http).await;
        if let Some(value) = series(&metrics, UNWIND_LEFTOVERS_METRIC) {
            if value == 1 {
                println!("{UNWIND_LEFTOVERS_METRIC} is 1 at {:.1} s", bot.elapsed());
                return value;
            }
        }
        if !wait.tick(bot).await {
            let message = wait.timed_out();
            fail(bot, &message);
        }
    }
}

/// `/metrics`, once, before a bot is asked to stop.
pub(crate) async fn read_metrics(bot: &Bot, http: &reqwest::Client) -> String {
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
pub(crate) fn assert_counter(bot: &Bot, metrics: &str, name: &str, expected: i64) {
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

/// Asserts a counter is at least `minimum`, and answers what it held.
///
/// [`assert_counter`]'s counterpart for a series this tier only ever bounds
/// from below — `unwind_repay`'s own `unwind_passes_total`, which a
/// backed-off pass can move more than once before the pool goes idle.
pub(crate) fn assert_counter_at_least(bot: &Bot, metrics: &str, name: &str, minimum: i64) -> i64 {
    match series(metrics, name) {
        Some(value) if value >= minimum => value,
        Some(value) => {
            let message = format!("{name} is {value}, expected at least {minimum}");
            fail(bot, &message);
        }
        None => {
            let message = format!("/metrics has no {name} series");
            fail(bot, &message);
        }
    }
}

/// Prints every counter series `/metrics` reports as nonzero.
///
/// Factored out of [`assert_metrics`] so a scenario that means to print
/// `/metrics` without that function's own three hard assertions —
/// `unwind_repay`'s first run, whose fill lands but deliberately leaves
/// debt behind rather than the one landed fill and nothing else
/// [`assert_metrics`] expects — can still show the same excerpt a passing
/// run's report keeps.
pub(crate) fn print_nonzero_counters(metrics: &str) {
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
}

/// The three series the `liquidation` scenario is judged by: exactly one
/// creation and one fill that landed, and at least one completed unwind
/// pass.
///
/// Exactly one of each, not "at least": a second creation for the same
/// borrower would mean the first was lost, and a second fill would mean the
/// first took only part of the auction. Either is worth failing on.
pub(crate) fn assert_metrics(bot: &Bot, metrics: &str) {
    // Every counter the run actually moved, printed before anything is
    // asserted: a failure below is far easier to read next to the rest of
    // what the bot counted, and this is the excerpt a CI artefact keeps.
    print_nonzero_counters(metrics);
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

/// `SIGTERM`, then the exit status, which must be `0`: the bot's graceful
/// drain is what the deployment's own stop is, and a non-zero status here
/// would mean a task failed on the way out.
pub(crate) async fn terminate(bot: &mut Bot) {
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

/// Reads `sandbox.env` and refuses the run unless it was deployed for
/// `scenario` and names the standalone network.
///
/// The scenario check comes first — before anything else, including the
/// passphrase assertion below it — because a mismatch here means this
/// test is about to run its own assertions against a network some *other*
/// scenario's deploy set up: the fixture is wrong, not merely the network,
/// and the file is checked before any of the file's other claims are worth
/// reading at all. `scenario` is asserted against [`SCENARIOS`] too, since
/// a typo in the test's own argument to this function deserves the same
/// answer as a typo in `SANDBOX_SCENARIO`.
///
/// Both of the file's refusals — this one and the passphrase's — live
/// here, before the caller has created a database or spawned anything:
/// this tier arms a bot with a real signing key, and the only thing that
/// makes that safe is the network it points at. What the file *claims* is
/// only half of that, so [`require_standalone_rpc`] asks the node itself
/// before the caller goes any further.
pub(crate) fn sandbox_env(env_path: &Path, scenario: &str) -> BTreeMap<String, String> {
    assert!(
        SCENARIOS.contains(&scenario),
        "sandbox_env: {scenario:?} is not one of {SCENARIOS:?} — this test named its own \
         scenario wrong"
    );
    let Ok(text) = std::fs::read_to_string(env_path) else {
        panic!(
            "{} does not exist — run scripts/sandbox/up.sh and scripts/sandbox/deploy.sh first",
            env_path.display()
        )
    };
    let env = parse_env_file(&text);

    let deployed = env
        .get("SANDBOX_SCENARIO")
        .map_or("(not set)", String::as_str);
    assert!(
        deployed == scenario,
        "{} was deployed for the '{deployed}' scenario, but this test is '{scenario}' — run \
         `SANDBOX_SCENARIO={scenario} make sandbox-deploy` first",
        env_path.display()
    );

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
/// say anything; this checks the node that an armed bot — `DRY_RUN=false`,
/// with a real signing key — is about to submit to. They are the same gate
/// `scripts/sandbox/lib.sh`'s `require_standalone_network` is on the shell
/// side, and this is the Rust side of it: whatever wrote `sandbox.env`, the
/// endpoint itself has to be this sandbox's own.
///
/// Every failure is a refusal, an unreachable RPC included: "could not ask"
/// is not "it is the sandbox". Called before a run's database is created
/// and long before anything is spawned, so a refusal here leaves nothing
/// behind.
pub(crate) async fn require_standalone_rpc(url: &str) {
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
pub(crate) fn crash(bot: &Bot, root: &Path) {
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

/// Runs `scripts/sandbox/mint.sh amount`, which mints `amount` of USDC
/// stroops to the sandbox filler — the `unwind_repay` scenario's way of
/// funding a wallet its own deploy left empty, once whatever it means to
/// prove with the debt still outstanding has already happened.
pub(crate) fn mint(bot: &Bot, root: &Path, amount: i128) {
    let script = root.join("scripts/sandbox/mint.sh");
    match Command::new(&script)
        .arg(amount.to_string())
        .current_dir(root)
        .output()
    {
        Ok(output) if output.status.success() => {
            println!(
                "mint.sh: the filler's USDC balance is now {}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        Ok(output) => {
            let message = format!(
                "mint.sh failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            fail(bot, &message);
        }
        Err(error) => fail(bot, &format!("could not run {}: {error}", script.display())),
    }
}

/// Every line of the bot's log containing `needle`, for a scenario's own
/// report — never for an assertion.
///
/// `unwind_repay` uses this to pull the fill's own request out of the log
/// (`"fill planned"`, `"fill recorded"`) and the leftover notification's
/// own line, rather than re-deriving either from a chain or store read the
/// assertions above have already made. A read failure is one line saying
/// so rather than a panic: this is reporting, and a report that could not
/// be built is not this run's own failure.
pub(crate) fn log_lines_containing(bot: &Bot, needle: &str) -> Vec<String> {
    match std::fs::read_to_string(&bot.log) {
        Ok(text) => text
            .lines()
            .filter(|line| line.contains(needle))
            .map(str::to_string)
            .collect(),
        Err(error) => vec![format!("(could not read {}: {error})", bot.log.display())],
    }
}
