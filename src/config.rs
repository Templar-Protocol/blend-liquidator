//! CLI and environment configuration.

use std::sync::Arc;

use clap::Parser;

use crate::math::fill::FillObjective;
use crate::LiquidatorError;

/// Parse only the literal strings `true` and `false`.
///
/// Deliberately stricter than clap's boolish parser, which also accepts `1`,
/// `y`, `yes` and `on`. Every extra spelling is another way into live trading,
/// and the dangerous direction is silent: a `DRY_RUN=yes` that parsed as
/// *false* would arm the bot while reading, to the operator, like it had been
/// disarmed. A value this refuses is a startup error they see immediately.
fn strict_bool(value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!(
            "expected `true` or `false` (exactly), got `{other}`"
        )),
    }
}

/// How to render logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// Human-readable, for a terminal.
    Text,
    /// One JSON object per line, for a log shipper.
    Json,
}

/// A named network, standing in for its passphrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum NetworkName {
    /// Public Global Stellar Network.
    Mainnet,
    /// Test SDF Network.
    Testnet,
}

impl NetworkName {
    /// The passphrase the network signs with.
    #[must_use]
    pub fn passphrase(self) -> &'static str {
        match self {
            Self::Mainnet => "Public Global Stellar Network ; September 2015",
            Self::Testnet => "Test SDF Network ; September 2015",
        }
    }
}

/// A secret configuration value. Renders as `Secret(<redacted>)` so it can
/// never reach a log line through `Debug`; the text is available only
/// through `expose`, which every caller must name deliberately.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps the secret text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The secret text, for the one place that puts it on the wire.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A decimal knob in 7-decimal fixed point, the scale the pool contract
/// uses for factors and the scale the store normalises health factors to.
///
/// Parsed from decimal text, never from float arithmetic: a TOML float
/// reaches this through its own shortest round-tripping rendering, so
/// `1.5` is exactly `15_000_000` and a value needing more than seven
/// fractional digits is a startup error rather than a quietly rounded
/// threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Decimal7(i128);

impl Decimal7 {
    /// The value in 7-decimal fixed point.
    #[must_use]
    pub fn get(self) -> i128 {
        self.0
    }
}

impl std::str::FromStr for Decimal7 {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (whole, fraction) = match text.split_once('.') {
            Some((whole, fraction)) => (whole, fraction),
            None => (text, ""),
        };
        if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!("`{text}` is not a non-negative decimal number"));
        }
        if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!("`{text}` is not a non-negative decimal number"));
        }
        if fraction.len() > 7 {
            return Err(format!("`{text}` has more than 7 decimal places"));
        }
        let scaled = format!("{whole}{fraction:0<7}");
        scaled
            .parse::<i128>()
            .map(Self)
            .map_err(|_| format!("`{text}` does not fit a 128-bit fixed-point value"))
    }
}

impl<'de> serde::Deserialize<'de> for Decimal7 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A TOML value reaches us as a string, an integer or a float; each
        // is converted through its decimal text, so no float arithmetic
        // ever touches a threshold.
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Integer(i64),
            Float(f64),
        }
        let text = match Raw::deserialize(deserializer)? {
            Raw::Text(text) => text,
            Raw::Integer(value) => value.to_string(),
            Raw::Float(value) => value.to_string(),
        };
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// The bottom of the pool contract's post-liquidation band, 7 decimals:
/// **below** `1.03` it answers `InvalidLiqTooSmall` (1214). The
/// comparison is strict (`is_hf_under` is `<`), so exactly `1.03` is
/// accepted and this bound is inclusive.
const TARGET_HF_MIN: i128 = 10_300_000;

/// One notch inside the top of that band, 7 decimals, and **this bot's
/// own margin rather than the contract's rule**. The contract refuses
/// only *above* `1.15` (`is_hf_over` is `>`, so exactly `1.15` is
/// accepted), but a target of exactly `1.15` aims every liquidation at
/// the ceiling with nothing left for the drift between planning and
/// fill — one ledger of interest on the borrower's debt puts the
/// outcome over, and the contract answers `InvalidLiqTooLarge` (1213).
const TARGET_HF_MAX: i128 = 11_500_000;

/// `TARGET_HF`, refused outside the band this bot plans within.
///
/// The knob names the health factor a liquidation aims to leave the
/// borrower at, and the plan's percent is computed straight from it:
/// `excess = liability_base × TARGET_HF − collateral_base`. At `0` that
/// excess is `−collateral_base`, never positive, so `plan_liquidation`
/// returns `None` for every borrower and each liquidatable one is recorded
/// `Skip(NoPlan)` — for ever, with no warning at all. Above the band it
/// fails the other way: the walk starts far too high and burns
/// `PLAN_ITERATIONS` simulations per borrower before skipping it, one warn
/// line each.
///
/// The lower bound is the contract's own: below `1.03` it answers
/// `InvalidLiqTooSmall`. The upper bound is not — the contract's own
/// check is strict (`is_hf_over` uses `>`), so it accepts exactly `1.15`
/// — but aiming a liquidation at the ceiling leaves no room for the
/// drift between planning and fill that `TARGET_HF`'s default of `1.06`
/// exists to absorb: one ledger of interest on the borrower's debt after
/// planning and the outcome lands over `1.15`, answered
/// `InvalidLiqTooLarge`. So `[1.03, 1.15)` is this bot's own band, one
/// notch narrower at the top than what the contract would accept, and
/// the ±1 percent walk is what absorbs whatever drift is left within it.
/// Refused at parse rather than clamped, for the same reason
/// `PLAN_ITERATIONS` and `PRICE_DELTA_BPS` are: a knob value that makes
/// the bot look busy and do nothing is a startup error, not a default.
fn target_health_factor(text: &str) -> Result<Decimal7, String> {
    let value: Decimal7 = text.parse()?;
    if value.get() < TARGET_HF_MIN || value.get() >= TARGET_HF_MAX {
        return Err(format!(
            "`{text}` is outside the band this bot plans within: TARGET_HF must be at least \
             1.03 and below 1.15. The contract itself accepts exactly 1.15 — its own check is \
             strict — but aiming there leaves no room for the drift between planning and fill, \
             so the upper bound is this bot's margin and the lower one is the contract's"
        ));
    }
    Ok(value)
}

/// `HF_SAFETY_MULTIPLIER`'s parser: a `Decimal7` of at least one.
fn health_multiplier(text: &str) -> Result<Decimal7, String> {
    let value: Decimal7 = text.parse()?;
    if value.get() < crate::math::SCALAR_7 {
        return Err(format!(
            "`{text}` is under 1: HF_SAFETY_MULTIPLIER scales the pool's own \
             min_health_factor, and under one the filler's floor would sit below it"
        ));
    }
    Ok(value)
}

/// An amount in an asset's own decimals, written as a decimal string
/// because it exceeds what TOML integers and JSON numbers hold.
fn amount_from_str(text: &str, field: &'static str) -> Result<i128, String> {
    text.parse()
        .map_err(|_| format!("{field}: `{text}` is not an integer amount"))
}

/// One profit rule: the first whose asset lists match a candidate wins.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfitRule {
    /// Required profit in basis points.
    pub profit_bps: u32,
    /// Bid assets this rule covers, or `["*"]`.
    pub supported_bid: Vec<String>,
    /// Lot assets this rule covers, or `["*"]`.
    pub supported_lot: Vec<String>,
}

/// One pool the bot follows. Phase 3 uses `address`, `primary_asset` and
/// the supported-asset lists; the profit and collateral fields are the
/// filler's, parsed here so the file's schema is settled once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// The pool contract.
    pub address: String,
    /// The asset the bot keeps as collateral in this pool.
    pub primary_asset: String,
    /// The least of it to hold, in the asset's own decimals.
    pub min_primary_collateral: i128,
    /// The health factor the filler keeps its own position above, 7 decimals.
    pub min_health_factor: i128,
    /// Profit required when no rule matches, in basis points.
    pub default_profit_bps: u32,
    /// Fill regardless of profit. For testing a pool, not for production.
    pub force_fill: bool,
    /// Which ledger a fill aims at: the free-fill point at block 400, or
    /// the earliest ledger the lot covers the bid plus the margin. See
    /// `crate::math::fill::FillObjective`.
    pub fill_objective: FillObjective,
    /// Bid assets the bot will pay, or `["*"]`.
    pub supported_bid: Vec<String>,
    /// Lot assets the bot will take, or `["*"]`.
    pub supported_lot: Vec<String>,
    /// Ordered profit rules; the first match wins.
    pub profits: Vec<ProfitRule>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPool {
    address: String,
    primary_asset: String,
    min_primary_collateral: String,
    min_health_factor: Decimal7,
    default_profit_bps: u32,
    #[serde(default)]
    force_fill: bool,
    #[serde(default)]
    fill_objective: Option<String>,
    supported_bid: Vec<String>,
    supported_lot: Vec<String>,
    #[serde(default)]
    profits: Vec<ProfitRule>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPools {
    #[serde(default)]
    pools: Vec<RawPool>,
}

/// The contract's own post-submit health minimum, `1.0000100` in 7
/// decimals (`validate_submit`'s `is_hf_under(e, 1_0000100)`). A filler
/// floor at or under it plans fills the contract refuses as `InvalidHf`.
const CONTRACT_MIN_HEALTH: i128 = 10_000_100;

/// Parses the pools file. Every failure names the field that caused it,
/// because this runs at startup where the operator is watching.
pub fn parse_pools(text: &str) -> Result<Vec<PoolConfig>, LiquidatorError> {
    let raw: RawPools = toml::from_str(text)
        .map_err(|error| LiquidatorError::Config(format!("pools file: {error}")))?;
    if raw.pools.is_empty() {
        return Err(LiquidatorError::Config(
            "pools file: at least one pool (a [[pools]] table) is required".to_string(),
        ));
    }
    let mut pools = Vec::with_capacity(raw.pools.len());
    let mut seen = std::collections::BTreeSet::new();
    for pool in raw.pools {
        if !seen.insert(pool.address.clone()) {
            return Err(LiquidatorError::Config(format!(
                "pools file: duplicate pool {}",
                pool.address
            )));
        }
        let min_primary_collateral =
            amount_from_str(&pool.min_primary_collateral, "min_primary_collateral")
                .map_err(LiquidatorError::Config)?;
        // The floor a position must clear to be worth acting on. A negative
        // floor is not a permissive one, it is a nonsense the filler would
        // read as "any position qualifies".
        if min_primary_collateral < 0 {
            return Err(LiquidatorError::Config(
                "pools file: min_primary_collateral must not be negative".to_owned(),
            ));
        }
        // A floor at or under the contract's own post-submit minimum lets
        // the filler plan fills the contract refuses as `InvalidHf` — every
        // one of them a wasted simulation and re-plan.
        if pool.min_health_factor.get() <= CONTRACT_MIN_HEALTH {
            return Err(LiquidatorError::Config(format!(
                "pools file: pool {}: min_health_factor is at or under the contract's own \
                 post-submit minimum (1.00001), so the filler would plan fills the contract \
                 refuses as InvalidHf",
                pool.address
            )));
        }
        let fill_objective = match pool.fill_objective.as_deref() {
            None | Some("free-fill") => FillObjective::FreeFill,
            Some("earliest-profitable") => FillObjective::EarliestProfitable,
            Some(other) => {
                return Err(LiquidatorError::Config(format!(
                    "pools file: pool {}: `{other}` is not a fill_objective: it is \
                     `free-fill` (the default, which waits for the ledger the bid is gone) \
                     or `earliest-profitable` (which fills as soon as the lot covers the bid \
                     plus the pool's margin)",
                    pool.address
                )));
            }
        };
        pools.push(PoolConfig {
            address: pool.address,
            primary_asset: pool.primary_asset,
            min_primary_collateral,
            min_health_factor: pool.min_health_factor.get(),
            default_profit_bps: pool.default_profit_bps,
            force_fill: pool.force_fill,
            fill_objective,
            supported_bid: pool.supported_bid,
            supported_lot: pool.supported_lot,
            profits: pool.profits,
        });
    }
    Ok(pools)
}

impl PoolConfig {
    /// Whether the filler takes this auction at all (spec §5): every bid
    /// asset is in `supported_bid` and every lot asset in `supported_lot`,
    /// `*` matching any reserve. One unsupported asset on either side
    /// refuses the whole auction — a fill takes every asset it names.
    #[must_use]
    pub fn supports(&self, bid: &[&str], lot: &[&str]) -> bool {
        covers(&self.supported_bid, bid) && covers(&self.supported_lot, lot)
    }

    /// The margin a fill waits for, in basis points: the first `profits`
    /// rule whose lists cover every bid and lot asset, else
    /// `default_profit_bps`. Order matters and is the operator's.
    #[must_use]
    pub fn profit_bps(&self, bid: &[&str], lot: &[&str]) -> u32 {
        self.profits
            .iter()
            .find(|rule| covers(&rule.supported_bid, bid) && covers(&rule.supported_lot, lot))
            .map_or(self.default_profit_bps, |rule| rule.profit_bps)
    }
}

/// Whether `list` names every one of `assets`, `*` naming them all.
fn covers(list: &[String], assets: &[&str]) -> bool {
    list.iter().any(|entry| entry == "*")
        || assets
            .iter()
            .all(|asset| list.iter().any(|entry| entry == asset))
}

/// What the binary does when it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RunMode {
    /// Follow the configured pools until shut down.
    Loop,
    /// Validate the configuration, print it redacted, and exit.
    CheckConfig,
}

/// Where the tracker gets its initial user set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedConfig {
    /// The analytics API's base URL; `None` when `SEED_URL` is empty.
    pub url: Option<String>,
    /// Only accounts at or below this health factor are seeded, 7 decimals.
    pub health_factor_max: i128,
    /// An optional static file of pool-to-account lists.
    pub file: Option<std::path::PathBuf>,
}

/// Everything the chain layer needs, validated. Built by [`Args::chain`].
///
/// `Debug` renders `rpc_url` as its origin alone. Some Soroban providers key
/// access by a path segment (`https://host/v1/<key>`), and this type is
/// rendered into a log line at startup, so the path and query are dropped
/// the way [`Secret`] drops its value: the endpoint is the diagnostic, the
/// rest can be a credential.
#[derive(Clone, PartialEq, Eq)]
pub struct ChainConfig {
    /// The network passphrase transactions are hashed with.
    pub network_passphrase: String,
    /// The Soroban RPC endpoint.
    pub rpc_url: String,
    /// Header name and secret value for a keyed RPC provider, both or
    /// neither. `Args::chain` validates the header name is one `reqwest`
    /// accepts before this is built.
    pub rpc_api_key: Option<(String, Secret)>,
    /// Inclusion-fee floor for a normal-priority transaction, in stroops.
    pub base_fee: u32,
    /// Inclusion-fee floor for a high-priority transaction, in stroops.
    pub high_fee: u32,
    /// How many ledgers a submitted transaction stays valid and is polled for.
    pub tx_poll_ledgers: u32,
}

impl std::fmt::Debug for ChainConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainConfig")
            .field("network_passphrase", &self.network_passphrase)
            .field("rpc_url", &endpoint_origin(&self.rpc_url))
            .field("rpc_api_key", &self.rpc_api_key)
            .field("base_fee", &self.base_fee)
            .field("high_fee", &self.high_fee)
            .field("tx_poll_ledgers", &self.tx_poll_ledgers)
            .finish()
    }
}

/// A URL's scheme, host and port, without the userinfo, path, query or
/// fragment that could carry a key. Anything that does not parse renders as
/// `<unparsed url>` rather than falling back to the whole string, because
/// the fallback is exactly the case where the shape is unexpected.
///
/// The parse is `reqwest`'s own, which is the parser the client itself uses,
/// so this cannot disagree with it about where the authority ends. Splitting
/// the text by hand did: it read everything before the first `/?#` as an
/// authority, so a URL whose path is separated by backslashes — which the
/// URL standard normalises to `/` for http and https, and which `reqwest`
/// therefore accepts — rendered its path, and any key in it.
fn endpoint_origin(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return "<unparsed url>".to_owned();
    };
    let Some(host) = parsed.host_str() else {
        return "<unparsed url>".to_owned();
    };
    match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    }
}

/// The `/healthz`, `/livez` and `/metrics` server. Built by
/// [`Args::service_with_secrets`] only when `PORT` or `HTTP_PORT` is set;
/// unset leaves the server off, which is every deployment that configures
/// neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpConfig {
    /// Address and port the server listens on.
    pub bind: std::net::SocketAddr,
    /// `/healthz` answers ready only while the processed ledger is within
    /// this many ledgers of chain head. Always at least 1:
    /// `HEALTH_MAX_LAG_LEDGERS=0` is refused at parse, because a bot
    /// exactly at head would still report not-ready on every ledger
    /// boundary its poll interval crosses — not a bound at all.
    pub max_lag_ledgers: u32,
}

/// The Telegram notification channel. Built by
/// [`Args::service_with_secrets`] only when both `TELEGRAM_BOT_TOKEN` and
/// `TELEGRAM_CHAT_ID` are set — either alone is a startup error, since a
/// token with nowhere to send is as useless as a destination with nothing
/// to send it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramConfig {
    /// The bot token. A secret: read from the environment only, never an
    /// argument, and never rendered by `Debug` — `Secret`'s own `Debug`
    /// redacts it, so this struct's derived `Debug` stays safe to log.
    pub token: Secret,
    /// The chat (or channel) id notifications are sent to.
    pub chat_id: String,
    /// The Bot API host to call instead of
    /// [`crate::notifier::telegram::TELEGRAM_API`]. Always `None` from
    /// configuration — no argument and no environment variable sets it —
    /// and it exists so a test can aim the channel at a local mock server
    /// through the same `ServiceConfig` the service is handed, rather than
    /// through a hook the service would have to consult at runtime.
    pub base_url: Option<String>,
}

/// Everything the service needs, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Network, RPC and fee configuration.
    pub chain: ChainConfig,
    /// Postgres. May carry a password, so it never renders.
    pub database_url: Secret,
    /// Connections in the pool. Sized against the tasks that query
    /// concurrently, not against one; see [`Args::database_max_connections`].
    pub database_max_connections: u32,
    /// The pools to follow.
    pub pools: Vec<PoolConfig>,
    /// Loop or validate.
    pub run_mode: RunMode,
    /// Whether submissions are suppressed. Still true by default.
    pub dry_run: bool,
    /// How often the poller asks for chain head.
    pub poll_interval: std::time::Duration,
    /// A user's row older than this many ledgers is refreshed.
    pub user_refresh_ledgers: u32,
    /// How many stale users to refresh per tick.
    pub refresh_batch: u32,
    /// How often the full scan reports the least healthy borrowers.
    pub full_scan_ledgers: u32,
    /// The health factor the full scan reports below, 7 decimals.
    pub scan_health_factor: i128,
    /// The health factor at or below which a borrower is liquidatable, 7
    /// decimals.
    pub liquidation_health_factor: i128,
    /// The health factor a liquidation aims to leave the borrower at, 7
    /// decimals.
    pub target_health_factor: i128,
    /// How often, in ledgers, prices are re-read for a significant move.
    pub oracle_scan_ledgers: u32,
    /// How far a price must move, in basis points, to be worth rechecking
    /// the borrowers exposed to it.
    ///
    /// Never zero: at zero, an unchanged price computes a delta of `0`,
    /// which fails the "moved at least this much" test and falls through to
    /// the direction branch, where `price > reference` is false on
    /// equality — reporting a spurious `Down` and flagging every borrower
    /// on every scan. `PRICE_DELTA_BPS=0` is refused at parse (see `Args`),
    /// so this field is never constructed with it.
    pub price_delta_bps: u32,
    /// How many times a rejected percent is adjusted against the contract's
    /// own answer before the borrower is left until the next recheck.
    ///
    /// Never zero: the walk runs `0..plan_iterations`, so a zero would
    /// simulate nothing at all and skip every liquidation while reporting
    /// that it had exhausted its iterations — a bot that looks busy and
    /// does nothing. `PLAN_ITERATIONS=0` is refused at parse (see `Args`),
    /// so this field is never constructed with it.
    pub plan_iterations: u32,
    /// A count of ledgers, measured from the first ledger the auctioneer
    /// sees, before any submission is attempted.
    pub startup_delay_ledgers: u32,
    /// Seeding.
    pub seed: SeedConfig,
    /// The pool's `min_health_factor` is multiplied by this for the floor
    /// the filler keeps its own position at or above after a fill, 7
    /// decimals. Always at least `SCALAR_7` (1.0).
    pub hf_safety_multiplier: i128,
    /// How often, in ledgers, an auction the filler has already planned is
    /// planned again. Always at least 1.
    pub replan_ledgers: u32,
    /// Within this many ledgers of its planned fill ledger an auction is
    /// planned again on every ledger. Zero means only at the fill ledger.
    pub replan_near_ledgers: u32,
    /// XLM the filler never spends, kept back for transaction fees, in
    /// stroops. Unsigned by type: `XLM_FEE_RESERVE` is refused negative at
    /// parse, and the inventory a negative reserve would *widen* takes a
    /// `u64` so nothing can hand it one.
    pub xlm_fee_reserve: u64,
    /// The estimated profit, in the pool oracle's units (7 decimals), at or
    /// above which a fill pays the high fee tier rather than the base one.
    pub high_fee_profit_threshold: i128,
    /// The longest the filler's wallet balances go unread. Always at least
    /// one second.
    pub inventory_refresh: std::time::Duration,
    /// How long a repeated notification of the same `(pool, account, kind)`
    /// is suppressed for (spec §7's dedup cooldown). Always at least one
    /// hour: `FAILURE_NOTIFICATION_COOLDOWN_HOURS` refuses zero at parse.
    pub notification_cooldown: std::time::Duration,
    /// The `/healthz`, `/livez` and `/metrics` server. `None` when neither
    /// `PORT` nor `HTTP_PORT` is set, which disables it entirely.
    pub http: Option<HttpConfig>,
    /// The Telegram notification channel. `None` when neither
    /// `TELEGRAM_BOT_TOKEN` nor `TELEGRAM_CHAT_ID` is set; either alone is
    /// a startup error.
    pub telegram: Option<TelegramConfig>,
}

#[derive(Debug, Parser)]
#[command(name = "liquidator", version, about, long_about = None)]
pub struct Args {
    /// Report what would be done without submitting any transaction.
    ///
    /// **Defaults to `true`, and there is no other way to opt into live
    /// trading.** The flag takes an optional value so it works from
    /// argv-only surfaces: bare `--dry-run` means true, and `--dry-run=false`
    /// or `--dry-run false` opts out.
    #[arg(
        long,
        env = "DRY_RUN",
        num_args = 0..=1,
        default_value = "true",
        default_missing_value = "true",
        value_parser = strict_bool,
    )]
    pub dry_run: bool,

    /// Log output format.
    #[arg(long, env = "LOG_FORMAT", value_enum, default_value = "text")]
    pub log_format: LogFormat,

    /// Network passphrase. Give this or `--network`, not both.
    #[arg(long, env = "NETWORK_PASSPHRASE", conflicts_with = "network")]
    pub network_passphrase: Option<String>,

    /// Named network, an alias for its passphrase.
    #[arg(long, env = "NETWORK", value_enum)]
    pub network: Option<NetworkName>,

    /// Soroban RPC URL.
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: Option<String>,

    /// Header that carries the RPC API key. The key itself is `RPC_API_KEY`
    /// in the environment only — never an argument. An empty value counts
    /// as unset, the same as the variable not being set at all.
    #[arg(long, env = "RPC_API_KEY_HEADER")]
    pub rpc_api_key_header: Option<String>,

    /// Inclusion-fee floor for normal-priority transactions, in stroops.
    #[arg(long, env = "BASE_FEE", default_value_t = 5_000)]
    pub base_fee: u32,

    /// Inclusion-fee floor for high-priority transactions, in stroops.
    #[arg(long, env = "HIGH_FEE", default_value_t = 10_000)]
    pub high_fee: u32,

    /// Ledgers a submitted transaction stays valid and is polled for: at
    /// least 1 (the upper bound is exclusive), at most 100 000 (about six
    /// days), so the derived wait cap always fits a clock deadline.
    #[arg(
        long,
        env = "TX_POLL_LEDGERS",
        default_value_t = 3,
        value_parser = clap::value_parser!(u32).range(1..=100_000),
    )]
    pub tx_poll_ledgers: u32,

    /// Path to the pools file. Give this or `--pools-toml`, not both.
    #[arg(long, env = "POOLS_FILE", conflicts_with = "pools_toml")]
    pub pools_file: Option<std::path::PathBuf>,

    /// The pools file's contents inline, for environments with no volume.
    #[arg(long, env = "POOLS_TOML")]
    pub pools_toml: Option<String>,

    /// What to do at startup.
    #[arg(long, env = "RUN_MODE", value_enum, default_value = "loop")]
    pub run_mode: RunMode,

    /// Connections in the database pool.
    ///
    /// Must cover every task that queries concurrently: one ledger poller
    /// per pool, the tracker, the auctioneer and the filler — roughly
    /// `pools + 3`, and the default covers up to seven pools. Sizing it
    /// below that does not
    /// deadlock; it times out acquiring a connection, and every
    /// [`crate::store::StoreError`] in this bot is fatal, so a load spike
    /// becomes a process exit.
    #[arg(
        long,
        env = "DATABASE_MAX_CONNECTIONS",
        default_value_t = 10,
        value_parser = clap::value_parser!(u32).range(1..=100),
    )]
    pub database_max_connections: u32,

    /// How often to ask the RPC for chain head, in milliseconds.
    #[arg(
        long,
        env = "POLL_INTERVAL_MS",
        default_value_t = 1_000,
        value_parser = clap::value_parser!(u64).range(100..=60_000),
    )]
    pub poll_interval_ms: u64,

    /// A tracked user whose row is older than this many ledgers is
    /// refreshed, so accrued interest is never missed.
    #[arg(long, env = "USER_REFRESH_LEDGERS", default_value_t = 241_920)]
    pub user_refresh_ledgers: u32,

    /// A rate, not a cap: how many stale users the tracker refreshes per
    /// tick, how many flagged borrowers the auctioneer decides and acts on
    /// per pool per tick, and the page size the full scan flags with.
    ///
    /// It deliberately does not bound the oracle scan. That scan flags
    /// every borrower a price move went against, because `PriceWatch`
    /// re-anchors its reference on the move it reports — so a borrower a
    /// bounded scan skipped would not be picked up by the next scan either.
    #[arg(
        long,
        env = "REFRESH_BATCH",
        default_value_t = 20,
        value_parser = clap::value_parser!(u32).range(1..=1_000),
    )]
    pub refresh_batch: u32,

    /// How often to report the least healthy borrowers, in ledgers.
    #[arg(
        long,
        env = "FULL_SCAN_LEDGERS",
        default_value_t = 1_200,
        value_parser = clap::value_parser!(u32).range(1..),
    )]
    pub full_scan_ledgers: u32,

    /// The health factor that scan reports below.
    #[arg(long, env = "SCAN_HF_THRESHOLD", default_value = "1.2")]
    pub scan_hf_threshold: Decimal7,

    /// The health factor at or below which a borrower is liquidatable.
    ///
    /// Below the contract's own strict test (`liability_base >
    /// collateral_base`, i.e. 1.0) on purpose: the margin absorbs rounding
    /// and the interest accrued between planning and execution, so an
    /// auction the bot creates is one the contract still accepts when it
    /// lands.
    #[arg(long, env = "LIQ_HF_THRESHOLD", default_value = "0.998")]
    pub liq_hf_threshold: Decimal7,

    /// The health factor a liquidation aims to leave the borrower at.
    ///
    /// The contract refuses a post-liquidation health factor above `1.15`
    /// (`InvalidLiqTooLarge`) or below `1.03` (`InvalidLiqTooSmall`), both
    /// comparisons strict, so it would accept exactly `1.15` — but aiming
    /// there leaves no room for the drift between planning and fill, so
    /// this bot refuses one notch inside that ceiling instead, with room
    /// for the auction to be filled a ledger or two later than planned.
    /// Anything outside `[1.03, 1.15)` is refused at parse rather than
    /// clamped: `TARGET_HF=0` would make the planned excess non-positive
    /// for every borrower, so every liquidatable one is recorded as "no
    /// plan" for ever, silently, and a value above the band burns
    /// `PLAN_ITERATIONS` simulations per borrower before skipping it. The
    /// lower bound is the contract's own rule; the upper one is this bot's
    /// margin, and the ±1 percent walk is what absorbs whatever drift is
    /// left within it.
    #[arg(
        long,
        env = "TARGET_HF",
        default_value = "1.06",
        value_parser = target_health_factor,
    )]
    pub target_hf: Decimal7,

    /// How often, in ledgers, prices are re-read and a significant move
    /// flags the borrowers it moved against.
    ///
    /// Zero is refused at parse. `scan_due` never fires for a zero period,
    /// so `0` would not mean "every ledger": it would silently switch the
    /// oracle scan off for the life of the process, leaving a price crash
    /// to be noticed by the full scan's cadence alone. A startup error is
    /// the loud direction, the same posture as `FULL_SCAN_LEDGERS`.
    #[arg(
        long,
        env = "ORACLE_SCAN_LEDGERS",
        default_value_t = 60,
        value_parser = clap::value_parser!(u32).range(1..),
    )]
    pub oracle_scan_ledgers: u32,

    /// How far a price must move from its reference, in basis points, to be
    /// worth rechecking the borrowers exposed to it.
    ///
    /// At least 1: at zero, an unchanged price computes a delta of `0`,
    /// which fails the "moved at least this much" test and falls through to
    /// the direction branch, where `price > reference` is false on
    /// equality — reporting a spurious `Down` and flagging every borrower
    /// on every scan, forever. Refusing it at parse time means that is a
    /// startup error, not a bot that runs and never stops rechecking
    /// everyone.
    #[arg(
        long,
        env = "PRICE_DELTA_BPS",
        default_value_t = 250,
        value_parser = clap::value_parser!(u32).range(1..),
    )]
    pub price_delta_bps: u32,

    /// How many times a rejected percent is adjusted against the contract's
    /// own answer before the borrower is left until the next recheck.
    ///
    /// At least 1: zero would make the walk never simulate at all, so every
    /// liquidation would be skipped as though the contract had refused it,
    /// silently. Refusing it at parse time means that is a startup error,
    /// not a bot that runs and never lands a fill.
    #[arg(
        long,
        env = "PLAN_ITERATIONS",
        default_value_t = 5,
        value_parser = clap::value_parser!(u32).range(1..),
    )]
    pub plan_iterations: u32,

    /// A count of ledgers, measured from the first ledger the auctioneer
    /// sees, before any submission is attempted.
    ///
    /// Zero by default, so a fresh deployment submits as soon as it is
    /// ready. A nonzero value gives a poller that is catching up on a
    /// backlog room to reach current chain state before the bot starts
    /// acting on health factors it has not yet re-verified against it.
    #[arg(long, env = "STARTUP_DELAY_LEDGERS", default_value_t = 0)]
    pub startup_delay_ledgers: u32,

    /// The pool's `min_health_factor` is multiplied by this for the floor the
    /// filler keeps its own position at or above after a fill (spec §5).
    ///
    /// At least 1, refused at parse rather than clamped: under one, the floor
    /// would sit under the pool's own `min_health_factor`, the operator's
    /// stated minimum, and a fill could leave the filler below it by design.
    #[arg(
        long,
        env = "HF_SAFETY_MULTIPLIER",
        default_value = "1.1",
        value_parser = health_multiplier,
    )]
    pub hf_safety_multiplier: Decimal7,

    /// How often, in ledgers, an auction the filler has already planned is
    /// planned again. Zero is refused: it would re-plan every auction on every
    /// ledger, which is `REPLAN_NEAR_LEDGERS`'s job and only near the target.
    #[arg(
        long,
        env = "REPLAN_LEDGERS",
        default_value_t = 10,
        value_parser = clap::value_parser!(u32).range(1..),
    )]
    pub replan_ledgers: u32,

    /// Within this many ledgers of its planned fill ledger an auction is
    /// planned again on every ledger. Zero means only at the fill ledger.
    #[arg(long, env = "REPLAN_NEAR_LEDGERS", default_value_t = 5)]
    pub replan_near_ledgers: u32,

    /// XLM the filler never spends, kept back for transaction fees. Decimal
    /// XLM; XLM has 7 decimals, so the parsed value is in stroops.
    #[arg(long, env = "XLM_FEE_RESERVE", default_value = "50")]
    pub xlm_fee_reserve: Decimal7,

    /// The estimated profit, in the pool oracle's units, at or above which a
    /// fill pays the high fee tier (`HIGH_FEE`) rather than the base one.
    #[arg(long, env = "HIGH_FEE_PROFIT_THRESHOLD", default_value = "10")]
    pub high_fee_profit_threshold: Decimal7,

    /// The longest the filler's wallet balances go unread, in seconds; they
    /// are also re-read after every confirmed transaction. Zero is refused:
    /// it would read every balance on every tick.
    #[arg(
        long,
        env = "INVENTORY_REFRESH_SECS",
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..),
    )]
    pub inventory_refresh_secs: u64,

    /// How long a repeated notification of the same `(pool, account, kind)`
    /// is suppressed for, in hours (spec §7's dedup cooldown). Zero is
    /// refused: it would dedup nothing, notifying on every tick — there is
    /// no "no cooldown" spelling, only shorter ones. The upper bound is
    /// [`std::time::Duration`]'s and not a policy: `Duration::from_hours`
    /// panics above `u64::MAX / 3_600`, so a value that would panic is
    /// refused at parse instead.
    #[arg(
        long,
        env = "FAILURE_NOTIFICATION_COOLDOWN_HOURS",
        default_value_t = 24,
        value_parser = clap::value_parser!(u64).range(1..=u64::MAX / 3_600),
    )]
    pub failure_notification_cooldown_hours: u64,

    /// The port injected by the deployment platform (spec §6: Cloud Run
    /// sets this). Wins over `HTTP_PORT` when both are set, because this
    /// is the one the platform controls — a deployment that also sets
    /// `HTTP_PORT` for some other reason must not silently lose the
    /// platform's binding. Either alone turns the `/healthz`, `/livez` and
    /// `/metrics` server on; neither leaves it off.
    #[arg(long, env = "PORT")]
    pub port: Option<u16>,

    /// The HTTP server's port for a deployment that does not inject
    /// `PORT`. See `port`: `PORT` wins when both are set. Unset, and
    /// `PORT` also unset, leaves the server off.
    #[arg(long, env = "HTTP_PORT")]
    pub http_port: Option<u16>,

    /// The address the HTTP server binds. Loopback by default, so nothing
    /// outside this host can reach it unless asked to. Cloud Run cannot
    /// route to a loopback listener, which is why that deployment sets
    /// this to `0.0.0.0`.
    #[arg(long, env = "HTTP_BIND_ADDR", default_value = "127.0.0.1")]
    pub http_bind_addr: std::net::IpAddr,

    /// `/healthz` answers ready only while the processed ledger is within
    /// this many ledgers of chain head. Zero is refused: a bot exactly at
    /// head would then report not-ready on every ledger boundary its own
    /// poll interval crosses, which is not a readiness bound at all.
    #[arg(
        long,
        env = "HEALTH_MAX_LAG_LEDGERS",
        default_value_t = 10,
        value_parser = clap::value_parser!(u32).range(1..),
    )]
    pub health_max_lag_ledgers: u32,

    /// The chat (or channel) Telegram notifications are sent to. Required
    /// together with `TELEGRAM_BOT_TOKEN` — read from the environment only,
    /// never an argument, by `Args::service`; either set without the other
    /// is a startup error, because a token with nowhere to send is as
    /// useless as a destination with nothing to send it with.
    #[arg(long, env = "TELEGRAM_CHAT_ID")]
    pub telegram_chat_id: Option<String>,

    /// The analytics API the tracker seeds from. Empty disables it.
    #[arg(
        long,
        env = "SEED_URL",
        default_value = "https://api.blend.templarfi.org"
    )]
    pub seed_url: String,

    /// Only accounts at or below this health factor are seeded.
    #[arg(long, env = "SEED_HF_MAX", default_value = "10")]
    pub seed_hf_max: Decimal7,

    /// An optional static file of pool-to-account lists.
    #[arg(long, env = "SEED_FILE")]
    pub seed_file: Option<std::path::PathBuf>,
}

impl Args {
    /// The chain configuration, reading `RPC_API_KEY` from the environment.
    /// An empty value counts as absent, the same as the variable not being
    /// set at all — a shell that exports `RPC_API_KEY=` should not silently
    /// behave differently from one that never set it.
    pub fn chain(&self) -> Result<ChainConfig, LiquidatorError> {
        self.chain_with_secret(
            std::env::var("RPC_API_KEY")
                .ok()
                .filter(|key| !key.is_empty()),
        )
    }

    /// The chain configuration with the API key supplied by the caller —
    /// what `chain` does after reading the environment, separated so tests
    /// never touch process-global state. `Some(String::new())` is treated as
    /// `None`, the same as `chain` does for an empty environment variable.
    pub fn chain_with_secret(
        &self,
        rpc_api_key: Option<String>,
    ) -> Result<ChainConfig, LiquidatorError> {
        let rpc_api_key = rpc_api_key.filter(|key| !key.is_empty());
        let network_passphrase = match (&self.network_passphrase, self.network) {
            (Some(passphrase), _) => passphrase.clone(),
            (None, Some(name)) => name.passphrase().to_string(),
            (None, None) => {
                return Err(LiquidatorError::Config(
                    "NETWORK_PASSPHRASE or NETWORK is required".to_string(),
                ))
            }
        };
        let rpc_url = self
            .rpc_url
            .clone()
            .ok_or_else(|| LiquidatorError::Config("RPC_URL is required".to_string()))?;
        // An empty header name counts as absent, the same as an empty
        // RPC_API_KEY above: a shell that exports RPC_API_KEY_HEADER= should
        // not behave differently from one that never set it.
        let header = self
            .rpc_api_key_header
            .as_deref()
            .filter(|header| !header.is_empty());
        if let Some(header) = header {
            reqwest::header::HeaderName::from_bytes(header.as_bytes()).map_err(|_| {
                LiquidatorError::Config("RPC_API_KEY_HEADER is not a valid header name".to_string())
            })?;
        }
        let rpc_api_key = match (header, rpc_api_key) {
            (Some(header), Some(key)) => Some((header.to_string(), Secret::new(key))),
            (None, None) => None,
            (Some(_), None) => {
                return Err(LiquidatorError::Config(
                    "RPC_API_KEY_HEADER is set but RPC_API_KEY is not".to_string(),
                ))
            }
            (None, Some(_)) => {
                return Err(LiquidatorError::Config(
                    "RPC_API_KEY is set but RPC_API_KEY_HEADER is not".to_string(),
                ))
            }
        };
        Ok(ChainConfig {
            network_passphrase,
            rpc_url,
            rpc_api_key,
            base_fee: self.base_fee,
            high_fee: self.high_fee,
            tx_poll_ledgers: self.tx_poll_ledgers,
        })
    }

    /// The service configuration, reading all three secrets from the
    /// environment: `DATABASE_URL`, `RPC_API_KEY` and `TELEGRAM_BOT_TOKEN`.
    /// An empty value counts as absent for each, the same as the variable
    /// not being set at all.
    pub fn service(&self) -> Result<ServiceConfig, LiquidatorError> {
        self.service_with_secrets(
            std::env::var("DATABASE_URL")
                .ok()
                .filter(|url| !url.is_empty()),
            std::env::var("RPC_API_KEY")
                .ok()
                .filter(|key| !key.is_empty()),
            std::env::var("TELEGRAM_BOT_TOKEN")
                .ok()
                .filter(|token| !token.is_empty()),
        )
    }

    /// What `service` does after reading the environment, separated so
    /// tests never touch process-global state. `telegram_bot_token` is
    /// filtered for emptiness the same way `rpc_api_key` is by
    /// `chain_with_secret`: `Some(String::new())` counts as `None`.
    ///
    /// The Telegram pair is validated before anything that needs a chain
    /// or a database, so a startup error about `TELEGRAM_CHAT_ID` or
    /// `TELEGRAM_BOT_TOKEN` is never masked by an unrelated one about
    /// `NETWORK` or `RPC_URL`.
    pub fn service_with_secrets(
        &self,
        database_url: Option<String>,
        rpc_api_key: Option<String>,
        telegram_bot_token: Option<String>,
    ) -> Result<ServiceConfig, LiquidatorError> {
        let telegram_bot_token = telegram_bot_token.filter(|token| !token.is_empty());
        let telegram_chat_id = self
            .telegram_chat_id
            .clone()
            .filter(|chat_id| !chat_id.is_empty());
        let telegram = match (telegram_chat_id, telegram_bot_token) {
            (Some(chat_id), Some(token)) => Some(TelegramConfig {
                token: Secret::new(token),
                chat_id,
                // Telegram's own host: see `TelegramConfig::base_url`.
                base_url: None,
            }),
            (None, None) => None,
            (Some(_), None) => {
                return Err(LiquidatorError::Config(
                    "TELEGRAM_CHAT_ID is set but TELEGRAM_BOT_TOKEN is not".to_string(),
                ))
            }
            (None, Some(_)) => {
                return Err(LiquidatorError::Config(
                    "TELEGRAM_BOT_TOKEN is set but TELEGRAM_CHAT_ID is not".to_string(),
                ))
            }
        };
        let chain = self.chain_with_secret(rpc_api_key)?;
        let database_url = database_url
            .ok_or_else(|| LiquidatorError::Config("DATABASE_URL is required".to_string()))?;
        let pools = match (&self.pools_file, &self.pools_toml) {
            (Some(path), None) => {
                let text = std::fs::read_to_string(path).map_err(|error| {
                    LiquidatorError::Config(format!("pools file {}: {error}", path.display()))
                })?;
                parse_pools(&text)?
            }
            (None, Some(text)) => parse_pools(text)?,
            _ => {
                return Err(LiquidatorError::Config(
                    "one of POOLS_FILE or POOLS_TOML is required".to_string(),
                ))
            }
        };
        // `TARGET_HF` is bounded by its own value parser; this one is a
        // relation between two knobs, which no single parser can see. The
        // full scan flags borrowers *strictly* below `SCAN_HF_THRESHOLD`
        // and the auctioneer judges borrowers *at or* below
        // `LIQ_HF_THRESHOLD`, so the liquidation threshold must sit
        // strictly below the scan threshold: at equality, a borrower
        // exactly on it is liquidatable but is never flagged by the scan,
        // and above it a whole band is — reachable only through an event
        // or a price move, and silently.
        if self.liq_hf_threshold >= self.scan_hf_threshold {
            return Err(LiquidatorError::Config(
                "LIQ_HF_THRESHOLD is at or above SCAN_HF_THRESHOLD: the full scan only \
                 flags borrowers strictly below SCAN_HF_THRESHOLD, so a liquidation \
                 threshold at or above it names borrowers nothing ever flags for a \
                 decision"
                    .to_string(),
            ));
        }
        // `PORT` wins over `HTTP_PORT` when both are set (spec §6: `PORT`
        // is what Cloud Run injects), and either turns the server on.
        let http = self.port.or(self.http_port).map(|port| HttpConfig {
            bind: std::net::SocketAddr::new(self.http_bind_addr, port),
            max_lag_ledgers: self.health_max_lag_ledgers,
        });
        Ok(ServiceConfig {
            chain,
            database_url: Secret::new(database_url),
            database_max_connections: self.database_max_connections,
            pools,
            run_mode: self.run_mode,
            dry_run: self.dry_run,
            poll_interval: std::time::Duration::from_millis(self.poll_interval_ms),
            user_refresh_ledgers: self.user_refresh_ledgers,
            refresh_batch: self.refresh_batch,
            full_scan_ledgers: self.full_scan_ledgers,
            scan_health_factor: self.scan_hf_threshold.get(),
            liquidation_health_factor: self.liq_hf_threshold.get(),
            target_health_factor: self.target_hf.get(),
            oracle_scan_ledgers: self.oracle_scan_ledgers,
            price_delta_bps: self.price_delta_bps,
            // Never zero: `--plan-iterations`/`PLAN_ITERATIONS` refuses 0 at
            // parse, so this is always at least 1. See
            // `ServiceConfig::plan_iterations`.
            plan_iterations: self.plan_iterations,
            startup_delay_ledgers: self.startup_delay_ledgers,
            seed: SeedConfig {
                url: Some(self.seed_url.clone()).filter(|url| !url.is_empty()),
                health_factor_max: self.seed_hf_max.get(),
                file: self.seed_file.clone(),
            },
            hf_safety_multiplier: self.hf_safety_multiplier.get(),
            replan_ledgers: self.replan_ledgers,
            replan_near_ledgers: self.replan_near_ledgers,
            xlm_fee_reserve: u64::try_from(self.xlm_fee_reserve.get()).map_err(|_| {
                LiquidatorError::Config(
                    "XLM_FEE_RESERVE is larger than any wallet can hold".to_string(),
                )
            })?,
            high_fee_profit_threshold: self.high_fee_profit_threshold.get(),
            inventory_refresh: std::time::Duration::from_secs(self.inventory_refresh_secs),
            notification_cooldown: std::time::Duration::from_hours(
                self.failure_notification_cooldown_hours,
            ),
            http,
            telegram,
        })
    }

    /// Every signing key this process was given, each parsed whether or
    /// not it is the one that signs: `AUCTIONEER_SECRET_KEY` and
    /// `FILLER_SECRET_KEY`, both `None` in the ordinary dry-run deployment
    /// and that is not an error.
    ///
    /// The filler's key is parsed even when the auctioneer's is what signs,
    /// because its *address* is needed either way — see
    /// [`SigningKeys::own_addresses`]. A malformed key is therefore a
    /// startup error whichever of the two it is, rather than a key that
    /// quietly does not exist until the phase that signs with it.
    ///
    /// Both arrive from the environment, never from argv: a signing key on
    /// the command line is readable from `/proc/<pid>/cmdline`, `ps` and
    /// `docker inspect`.
    ///
    /// # Errors
    ///
    /// [`LiquidatorError::Config`] naming which variable was malformed. The
    /// message never carries the secret itself.
    pub fn signing_keys(
        &self,
        filler: Option<String>,
        auctioneer: Option<String>,
    ) -> Result<SigningKeys, LiquidatorError> {
        let keys = SigningKeys {
            auctioneer: parse_signing_key("AUCTIONEER_SECRET_KEY", auctioneer)?,
            filler: parse_signing_key("FILLER_SECRET_KEY", filler)?,
        };
        // Two roles on one key would need two queues on one key — the
        // sequence race `queue.rs` exists to make unreachable — so the same
        // key twice is refused, and leaving `AUCTIONEER_SECRET_KEY` unset is
        // how one key signs both.
        if let (Some(auctioneer), Some(filler)) = (&keys.auctioneer, &keys.filler) {
            if auctioneer.address() == filler.address() {
                return Err(LiquidatorError::Config(
                    "AUCTIONEER_SECRET_KEY and FILLER_SECRET_KEY are the same key: leave \
                     AUCTIONEER_SECRET_KEY unset and the auctioneer signs with the filler's key, \
                     through the one queue that key needs"
                        .to_string(),
                ));
            }
        }
        // Live trading fills auctions, and the filler signs with its own
        // key only — an armed bot with just the auctioneer's key would
        // create auctions and never fill one.
        if !self.dry_run && keys.filler.is_none() {
            return Err(LiquidatorError::Config(
                "DRY_RUN=false needs FILLER_SECRET_KEY: live trading fills auctions, and the \
                 filler signs with its own key only"
                    .to_string(),
            ));
        }
        Ok(keys)
    }
}

/// One optional secret key, parsed with `name` on the error rather than the
/// value: the message an operator sees says which variable is wrong and
/// never echoes what was in it.
fn parse_signing_key(
    name: &str,
    secret: Option<String>,
) -> Result<Option<crate::chain::Signer>, LiquidatorError> {
    secret
        .map(|secret| {
            crate::chain::Signer::from_secret(&secret)
                .map_err(|error| LiquidatorError::Config(format!("{name}: {error}")))
        })
        .transpose()
}

/// Every signing key this process holds.
///
/// **Both** addresses are the bot's own regardless of which one signs: the
/// auctioneer must never create a liquidation auction against either — the
/// contract has no reason to refuse the bot liquidating its own filler
/// position, and in dry-run there is no contract to refuse at all — so
/// [`SigningKeys::own_addresses`] reports both, and
/// [`SigningKeys::into_signers`] answers the separate question of which key
/// signs which role: the filler always signs with its own, and the
/// auctioneer signs with its own when configured, else the filler's.
#[derive(Debug)]
pub struct SigningKeys {
    /// `AUCTIONEER_SECRET_KEY`, when set.
    pub auctioneer: Option<crate::chain::Signer>,
    /// `FILLER_SECRET_KEY`, when set. Held for its address even while
    /// nothing signs with it: Phase 5 is where the filler's position first
    /// exists, and it is the position this bot must not liquidate.
    pub filler: Option<crate::chain::Signer>,
}

impl SigningKeys {
    /// Every address this bot holds a key for — both when the two keys
    /// differ, one when only one is configured or they are the same key,
    /// and empty when none is, which excludes nothing rather than
    /// everything.
    #[must_use]
    pub fn own_addresses(&self) -> std::collections::BTreeSet<String> {
        [self.auctioneer.as_ref(), self.filler.as_ref()]
            .into_iter()
            .flatten()
            .map(|signer| signer.address().to_string())
            .collect()
    }

    /// The two roles' keys: the filler's own, and the auctioneer's own or
    /// else the filler's — the spec's "auctioneer key optional, defaulting
    /// to the filler key". Never the reverse: the filler does not sign with
    /// the auctioneer's key.
    #[must_use]
    pub fn into_signers(self) -> Signers {
        let filler = self.filler.map(Arc::new);
        let auctioneer = self.auctioneer.map(Arc::new).or_else(|| filler.clone());
        Signers { auctioneer, filler }
    }
}

/// The two signing roles' keys. Each is an `Arc` — `Signer` is
/// deliberately not `Clone`, since it holds key material — so a role that
/// falls back to the other's key holds *the same* key, and
/// [`Signers::shared`] tells by pointer. Whether the roles share a key is
/// what decides whether they share a submission queue: one queue per key,
/// because two queues on one key race each other for its sequence number.
#[derive(Debug, Default)]
pub struct Signers {
    /// Signs auction creations: `AUCTIONEER_SECRET_KEY`, else the filler's.
    pub auctioneer: Option<Arc<crate::chain::Signer>>,
    /// Signs fills: `FILLER_SECRET_KEY`, and never the auctioneer's.
    pub filler: Option<Arc<crate::chain::Signer>>,
}

impl Signers {
    /// Whether both roles hold the one key.
    #[must_use]
    pub fn shared(&self) -> bool {
        matches!(
            (&self.auctioneer, &self.filler),
            (Some(auctioneer), Some(filler)) if Arc::ptr_eq(auctioneer, filler)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The safety invariant, asserted from the first commit: absent any
    /// argument or environment, the bot is disarmed.
    #[test]
    fn dry_run_defaults_to_true() {
        assert_clean_environment();
        let args = Args::try_parse_from(["liquidator"]).unwrap();
        assert!(args.dry_run, "dry-run must default to true");
    }

    #[test]
    fn bare_flag_means_true() {
        let args = Args::try_parse_from(["liquidator", "--dry-run"]).unwrap();
        assert!(args.dry_run);
    }

    #[test]
    fn explicit_false_opts_out() {
        for argv in [
            vec!["liquidator", "--dry-run=false"],
            vec!["liquidator", "--dry-run", "false"],
        ] {
            let args = Args::try_parse_from(&argv).unwrap();
            assert!(!args.dry_run, "{argv:?} should disable dry-run");
        }
    }

    /// A near-miss must fail loudly rather than resolve to `false`.
    #[test]
    fn near_miss_spellings_are_refused() {
        for value in ["1", "0", "yes", "no", "on", "off", "True", "FALSE", ""] {
            assert!(
                Args::try_parse_from(["liquidator", "--dry-run", value]).is_err(),
                "`{value}` must be refused, never silently treated as a boolean"
            );
        }
    }

    /// `Args::try_parse_from` honours `#[arg(env = ..)]`, so a developer's
    /// shell exporting any of these would silently change what the config
    /// tests exercise. Asserted, never mutated: fail loudly instead of
    /// passing for the wrong reason.
    ///
    /// `DATABASE_URL` is deliberately not in this list, for the same reason
    /// `RPC_API_KEY` is not: neither is a clap argument (both are secrets,
    /// read straight from the environment by `service`/`chain`, never by
    /// parsing), so a value set in the shell cannot change what
    /// `Args::try_parse_from` produces here — and both `make check` and CI's
    /// `lint-test` job export `DATABASE_URL` for the whole run so the sqlx
    /// query macros can check themselves, which would make this assertion
    /// fail on every sanctioned way of running these tests.
    ///
    /// `AUCTIONEER_SECRET_KEY` and `FILLER_SECRET_KEY` are excluded for the
    /// same reason: both are read straight from the environment by
    /// `main.rs` and handed to [`Args::signing_keys`], and neither is ever
    /// declared as a clap argument. `TELEGRAM_BOT_TOKEN` is excluded for
    /// the same reason again: it is a secret read straight from the
    /// environment by `service`, never a clap argument, so the tests pass
    /// it explicitly to `service_with_secrets` instead.
    fn assert_clean_environment() {
        for name in [
            "RPC_URL",
            "NETWORK",
            "NETWORK_PASSPHRASE",
            "RPC_API_KEY_HEADER",
            "BASE_FEE",
            "HIGH_FEE",
            "TX_POLL_LEDGERS",
            "DRY_RUN",
            "LOG_FORMAT",
            "DATABASE_MAX_CONNECTIONS",
            "POOLS_FILE",
            "POOLS_TOML",
            "RUN_MODE",
            "POLL_INTERVAL_MS",
            "USER_REFRESH_LEDGERS",
            "REFRESH_BATCH",
            "FULL_SCAN_LEDGERS",
            "SCAN_HF_THRESHOLD",
            "LIQ_HF_THRESHOLD",
            "TARGET_HF",
            "ORACLE_SCAN_LEDGERS",
            "PRICE_DELTA_BPS",
            "PLAN_ITERATIONS",
            "STARTUP_DELAY_LEDGERS",
            "HF_SAFETY_MULTIPLIER",
            "REPLAN_LEDGERS",
            "REPLAN_NEAR_LEDGERS",
            "XLM_FEE_RESERVE",
            "HIGH_FEE_PROFIT_THRESHOLD",
            "INVENTORY_REFRESH_SECS",
            "FAILURE_NOTIFICATION_COOLDOWN_HOURS",
            "SEED_URL",
            "SEED_HF_MAX",
            "SEED_FILE",
            "PORT",
            "HTTP_PORT",
            "HTTP_BIND_ADDR",
            "HEALTH_MAX_LAG_LEDGERS",
            "TELEGRAM_CHAT_ID",
        ] {
            assert!(
                std::env::var_os(name).is_none(),
                "{name} is set in the environment; the config tests must run in a clean \
                 environment or they exercise the shell's values instead of the argv given"
            );
        }
    }

    fn parse(argv: &[&str]) -> Args {
        assert_clean_environment();
        Args::try_parse_from(argv).unwrap()
    }

    /// The fallible sibling of `parse`, for the tests that want to assert
    /// *how* parsing fails rather than unwrap a success.
    fn try_parse(argv: &[&str]) -> clap::error::Result<Args> {
        assert_clean_environment();
        Args::try_parse_from(argv)
    }

    #[test]
    fn a_network_name_resolves_to_its_passphrase() {
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
        ]);
        let chain = args.chain_with_secret(None).unwrap();
        assert_eq!(
            chain.network_passphrase,
            "Test SDF Network ; September 2015"
        );
        assert_eq!(chain.rpc_url, "http://rpc");
        assert_eq!(chain.rpc_api_key, None);
        assert_eq!(
            (chain.base_fee, chain.high_fee, chain.tx_poll_ledgers),
            (5_000, 10_000, 3)
        );
    }

    #[test]
    fn an_explicit_passphrase_wins_over_nothing_and_conflicts_with_a_name() {
        let args = parse(&[
            "liquidator",
            "--network-passphrase",
            "Custom ; 2026",
            "--rpc-url",
            "http://rpc",
        ]);
        assert_eq!(
            args.chain_with_secret(None).unwrap().network_passphrase,
            "Custom ; 2026"
        );
        assert!(Args::try_parse_from([
            "liquidator",
            "--network",
            "mainnet",
            "--network-passphrase",
            "x",
            "--rpc-url",
            "http://rpc",
        ])
        .is_err());
    }

    #[test]
    fn the_network_and_rpc_url_are_required_by_chain_not_by_parsing() {
        let args = parse(&["liquidator"]);
        assert!(matches!(
            args.chain_with_secret(None),
            Err(LiquidatorError::Config(_))
        ));
        let args = parse(&["liquidator", "--network", "mainnet"]);
        assert!(matches!(
            args.chain_with_secret(None),
            Err(LiquidatorError::Config(_))
        ));
    }

    #[test]
    fn the_api_key_header_and_secret_come_together_or_not_at_all() {
        let base = [
            "liquidator",
            "--network",
            "mainnet",
            "--rpc-url",
            "http://rpc",
        ];
        let with_header = parse(&[&base[..], &["--rpc-api-key-header", "X-Api-Key"]].concat());
        assert!(matches!(
            with_header.chain_with_secret(None),
            Err(LiquidatorError::Config(_))
        ));
        assert_eq!(
            with_header
                .chain_with_secret(Some("k".to_string()))
                .unwrap()
                .rpc_api_key,
            Some(("X-Api-Key".to_string(), Secret::new("k")))
        );
        let without = parse(&base);
        assert!(matches!(
            without.chain_with_secret(Some("k".to_string())),
            Err(LiquidatorError::Config(_))
        ));
    }

    /// The key is a secret: it must never be a clap argument, or it would
    /// be readable from `/proc/<pid>/cmdline`.
    #[test]
    fn the_api_key_is_not_a_command_line_argument() {
        assert!(Args::try_parse_from(["liquidator", "--rpc-api-key", "k"]).is_err());
    }

    /// `RPC_API_KEY=` (set but empty) must behave exactly like the variable
    /// being unset, in both `chain` (via an empty environment read) and
    /// `chain_with_secret` (via a bare `Some(String::new())`); the same goes
    /// for an empty `RPC_API_KEY_HEADER`, which `chain_with_secret` also
    /// treats as absent.
    #[test]
    fn an_empty_api_key_counts_as_absent() {
        let with_header = parse(&[
            "liquidator",
            "--network",
            "mainnet",
            "--rpc-url",
            "http://rpc",
            "--rpc-api-key-header",
            "X-Api-Key",
        ]);
        assert!(matches!(
            with_header.chain_with_secret(Some(String::new())),
            Err(LiquidatorError::Config(_))
        ));
        let without_header = parse(&[
            "liquidator",
            "--network",
            "mainnet",
            "--rpc-url",
            "http://rpc",
        ]);
        let chain = without_header
            .chain_with_secret(Some(String::new()))
            .unwrap();
        assert_eq!(chain.rpc_api_key, None);

        // An empty header behaves like an absent one, both alone and paired
        // with an empty key, but not paired with a real key: that is still
        // "the key is set but the header is not".
        let empty_header = parse(&[
            "liquidator",
            "--network",
            "mainnet",
            "--rpc-url",
            "http://rpc",
            "--rpc-api-key-header",
            "",
        ]);
        let chain = empty_header.chain_with_secret(Some(String::new())).unwrap();
        assert_eq!(chain.rpc_api_key, None);
        assert!(matches!(
            empty_header.chain_with_secret(Some("k".to_string())),
            Err(LiquidatorError::Config(_))
        ));
    }

    /// `ChainConfig`'s doc claims the header name is validated; this is
    /// where that validation happens, since `ChainConfig` itself has no
    /// constructor of its own that could enforce it.
    #[test]
    fn an_invalid_api_key_header_name_is_a_config_error() {
        let args = parse(&[
            "liquidator",
            "--network",
            "mainnet",
            "--rpc-url",
            "http://rpc",
            "--rpc-api-key-header",
            "bad header",
        ]);
        assert!(matches!(
            args.chain_with_secret(Some("k".to_string())),
            Err(LiquidatorError::Config(_))
        ));
    }

    /// The transaction's ledger bound is exclusive, so `TX_POLL_LEDGERS=0`
    /// would make every transaction unlandable before it starts. Refusing
    /// it at parse time means the failure is a startup error, not a bot that
    /// runs and never lands a fill.
    #[test]
    fn a_zero_poll_window_is_refused_at_parse() {
        assert!(Args::try_parse_from(["liquidator", "--tx-poll-ledgers", "0"]).is_err());
    }

    /// The global safety invariant applied to configuration: a secret must
    /// never appear in a `Debug` rendering, because that is exactly the path
    /// a stray `tracing::debug!("{config:?}")` or panic message would take.
    #[test]
    fn the_api_key_never_appears_in_a_debug_rendering() {
        let args = parse(&[
            "liquidator",
            "--network",
            "mainnet",
            "--rpc-url",
            "http://rpc",
            "--rpc-api-key-header",
            "X-Api-Key",
        ]);
        let config = args
            .chain_with_secret(Some("secret-123".to_string()))
            .unwrap();
        let rendered = format!("{config:?}");
        assert!(rendered.contains("X-Api-Key"));
        assert!(rendered.contains("Secret(<redacted>)"));
        assert!(!rendered.contains("secret-123"));
        assert_eq!(Secret::new("secret-123").expose(), "secret-123");
    }

    /// Some Soroban providers key access by a path segment rather than a
    /// header, and this config is rendered into a log line on every start,
    /// so `Debug` keeps the endpoint and drops everything after it.
    #[test]
    fn the_rpc_url_renders_as_its_origin_only() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "mainnet",
            "--rpc-url",
            "https://soroban.example.org/v1/super-secret-key",
        ]);
        let rendered = format!("{:?}", args.chain().expect("chain config"));
        assert!(
            !rendered.contains("super-secret-key"),
            "a path-embedded key must not reach a log line: {rendered}"
        );
        assert!(
            rendered.contains("https://soroban.example.org"),
            "the endpoint itself is the diagnostic: {rendered}"
        );

        // The shapes the origin helper has to get right: a port is part of
        // the endpoint, userinfo is a credential, and anything that is not a
        // URL renders as nothing rather than as itself.
        assert_eq!(
            endpoint_origin("https://host:8000/v1/key?token=t"),
            "https://host:8000"
        );
        assert_eq!(endpoint_origin("https://user:pw@host/v1"), "https://host");
        assert_eq!(endpoint_origin("not a url"), "<unparsed url>");
        assert_eq!(endpoint_origin("https://"), "<unparsed url>");
        // A port that is not a port makes the whole URL unparseable, so
        // none of it renders rather than the text before the first `/`.
        assert_eq!(
            endpoint_origin("https://rpc.example:token"),
            "<unparsed url>"
        );
        // http and https normalise a backslash to a path separator, so a
        // URL written this way still reaches the RPC — and must not carry
        // its path into a log line.
        let backslashed = endpoint_origin("https://rpc.example\\v1\\super-secret-key");
        assert!(
            !backslashed.contains("super-secret-key"),
            "a backslash-separated path must not render: {backslashed}"
        );
    }

    const POOLS: &str = r#"
[[pools]]
address = "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD"
primary_asset = "CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75"
min_primary_collateral = "1000000000000"
min_health_factor = 1.5
default_profit_bps = 1000
force_fill = false
supported_bid = ["CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75"]
supported_lot = ["*"]

[[pools.profits]]
profit_bps = 500
supported_bid = ["CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75"]
supported_lot = ["*"]
"#;

    /// A `DATABASE_URL` value for tests that need `service_with_secrets`
    /// to succeed but never connect: nothing here talks to Postgres.
    const DB: &str = "postgres://u:p@127.0.0.1/x";

    /// A `ServiceConfig` built from a fixed, valid chain-and-pools baseline
    /// with `args`'s HTTP and Telegram fields overlaid — for tests that
    /// exercise only those knobs and would otherwise have to spell out a
    /// whole network, RPC endpoint and pools file to get one.
    fn minimal_service(args: &Args) -> ServiceConfig {
        let mut base = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
            "--pools-toml",
            POOLS,
        ]);
        base.port = args.port;
        base.http_port = args.http_port;
        base.http_bind_addr = args.http_bind_addr;
        base.health_max_lag_ledgers = args.health_max_lag_ledgers;
        base.telegram_chat_id = args.telegram_chat_id.clone();
        base.service_with_secrets(Some(DB.into()), None, None)
            .expect("minimal service configuration")
    }

    #[test]
    fn a_decimal_knob_becomes_seven_decimal_fixed_point() {
        for (text, expected) in [
            ("1.5", 15_000_000_i128),
            ("1", 10_000_000),
            ("0.998", 9_980_000),
            ("1.0000001", 10_000_001),
            ("10", 100_000_000),
            ("0", 0),
        ] {
            assert_eq!(
                text.parse::<Decimal7>().expect(text).get(),
                expected,
                "{text}"
            );
        }
    }

    /// More precision than the fixed point holds is a startup error, not a
    /// silent rounding of a threshold that decides whether to liquidate.
    #[test]
    fn a_decimal_knob_refuses_what_it_cannot_hold() {
        for text in [
            "1.00000001",
            "",
            "1.2.3",
            "abc",
            "-1",
            "1e9",
            "170141183460469231731687303715884105728",
        ] {
            assert!(text.parse::<Decimal7>().is_err(), "{text} should not parse");
        }
    }

    #[test]
    fn the_pools_file_parses_with_its_profit_rules() {
        let pools = parse_pools(POOLS).expect("parses");
        assert_eq!(pools.len(), 1);
        let pool = &pools[0];
        assert_eq!(
            pool.address,
            "CAJJZSGMMM3PD7N33TAPHGBUGTB43OC73HVIK2L2G6BNGGGYOSSYBXBD"
        );
        assert_eq!(pool.min_primary_collateral, 1_000_000_000_000);
        assert_eq!(pool.min_health_factor, 15_000_000, "1.5 in 7 decimals");
        assert_eq!(pool.default_profit_bps, 1_000);
        assert!(!pool.force_fill);
        assert_eq!(pool.supported_lot, ["*"]);
        assert_eq!(pool.profits.len(), 1);
        assert_eq!(pool.profits[0].profit_bps, 500);
    }

    #[test]
    fn the_fill_objective_defaults_to_the_free_fill_and_rejects_nonsense() {
        let pools = parse_pools(POOLS).expect("the example parses");
        assert_eq!(pools[0].fill_objective, FillObjective::FreeFill);

        let named = POOLS.replace(
            "force_fill = false",
            "force_fill = false\nfill_objective = \"earliest-profitable\"",
        );
        assert_eq!(
            parse_pools(&named).expect("parses")[0].fill_objective,
            FillObjective::EarliestProfitable
        );

        let bad = POOLS.replace(
            "force_fill = false",
            "force_fill = false\nfill_objective = \"whenever\"",
        );
        assert!(parse_pools(&bad)
            .expect_err("nonsense is refused")
            .to_string()
            .contains("fill_objective"));
    }

    #[test]
    fn a_pools_file_that_is_wrong_is_a_config_error_naming_the_problem() {
        for (bad, expected) in [
            ("", "at least one pool"),
            ("[[pools]]\naddress = \"C\"\n", "missing"),
            (
                &POOLS.replace("1000000000000", "not a number"),
                "min_primary_collateral",
            ),
            (
                &POOLS.replace("min_health_factor = 1.5", "min_health_factor = 1.00000001"),
                "min_health_factor",
            ),
            (
                &POOLS.replace("\"1000000000000\"", "\"-1\""),
                "must not be negative",
            ),
            (
                &POOLS.replace("[[pools]]", "[[pools]]\naddress = \"CDUP\""),
                "duplicate",
            ),
        ] {
            let error = parse_pools(bad).expect_err(bad).to_string();
            assert!(
                error.contains(expected),
                "{error} should mention {expected}"
            );
        }
    }

    /// Two pools naming the same address is a configuration mistake the bot
    /// must refuse: it would track one pool twice and race itself.
    #[test]
    fn duplicate_pool_addresses_are_refused() {
        let doubled = format!("{POOLS}{POOLS}");
        assert!(parse_pools(&doubled)
            .expect_err("duplicate")
            .to_string()
            .contains("duplicate"));
    }

    #[test]
    fn the_service_configuration_needs_a_database_url_and_one_pools_source() {
        assert_clean_environment();
        let base = [
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
        ];
        let args = parse(&[&base[..], &["--pools-toml", POOLS]].concat());
        assert!(matches!(
            args.service_with_secrets(None, None, None),
            Err(LiquidatorError::Config(_))
        ));
        let config = args
            .service_with_secrets(Some("postgres://u:p@localhost/db".to_string()), None, None)
            .expect("configuration");
        assert_eq!(config.pools.len(), 1);
        assert_eq!(config.database_url.expose(), "postgres://u:p@localhost/db");
        assert_eq!(config.run_mode, RunMode::Loop);
        assert!(config.dry_run, "dry-run is still the default");
        assert_eq!(config.poll_interval, std::time::Duration::from_secs(1));
        assert_eq!(config.scan_health_factor, 12_000_000);
        assert_eq!(config.seed.health_factor_max, 100_000_000);
        assert_eq!(
            config.seed.url.as_deref(),
            Some("https://api.blend.templarfi.org")
        );

        // Neither pools source, and both at once, are both errors.
        let neither = parse(&base);
        assert!(matches!(
            neither.service_with_secrets(Some("postgres://x".to_string()), None, None),
            Err(LiquidatorError::Config(_))
        ));
        assert!(Args::try_parse_from(
            [&base[..], &["--pools-toml", POOLS, "--pools-file", "/x"]].concat()
        )
        .is_err());
    }

    /// The database URL may carry a password, so it must never be an
    /// argument and never render.
    #[test]
    fn the_database_url_is_not_an_argument_and_never_renders() {
        assert_clean_environment();
        assert!(Args::try_parse_from(["liquidator", "--database-url", "postgres://x"]).is_err());
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
            "--pools-toml",
            POOLS,
        ]);
        let config = args
            .service_with_secrets(
                Some("postgres://user:hunter2@localhost/db".to_string()),
                None,
                None,
            )
            .expect("configuration");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("Secret(<redacted>)"));
    }

    /// An empty SEED_URL disables the analytics source, as the spec says.
    #[test]
    fn an_empty_seed_url_disables_the_analytics_source() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
            "--pools-toml",
            POOLS,
            "--seed-url",
            "",
        ]);
        let config = args
            .service_with_secrets(Some("postgres://x".to_string()), None, None)
            .expect("configuration");
        assert_eq!(config.seed.url, None);
    }

    /// The auctioneer's thresholds parse into 7-decimal fixed point with the
    /// spec's defaults, and a value with more precision than the scale can
    /// hold is refused at startup rather than silently rounded — a
    /// threshold that decides whether to liquidate is not a place to lose a
    /// digit.
    #[test]
    fn the_auctioneer_thresholds_default_and_refuse_over_precision() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
        ]);
        assert_eq!(
            args.liq_hf_threshold.get(),
            9_980_000,
            "LIQ_HF_THRESHOLD defaults to 0.998"
        );
        assert_eq!(
            args.target_hf.get(),
            10_600_000,
            "TARGET_HF defaults to 1.06"
        );
        assert_eq!(args.oracle_scan_ledgers, 60);
        assert_eq!(args.price_delta_bps, 250);
        assert_eq!(args.plan_iterations, 5);
        assert_eq!(args.startup_delay_ledgers, 0);

        let refused = try_parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
            "--target-hf",
            "1.060000001",
        ]);
        assert!(
            refused.is_err(),
            "a value the 7-decimal scale cannot hold is a startup error"
        );
    }

    /// `TARGET_HF` outside the contract's post-liquidation band is refused
    /// at parse, not clamped — the same standard `PLAN_ITERATIONS=0` and
    /// `PRICE_DELTA_BPS=0` already hold.
    ///
    /// Zero is the one that matters most: `excess = liability_base ×
    /// TARGET_HF − collateral_base` is then never positive, so every
    /// liquidatable borrower is recorded `Skip(NoPlan)` for ever and
    /// nothing warns. The edges are asserted in both directions, because a
    /// bound written with the comparison the wrong way round would refuse
    /// exactly the values it must accept.
    #[test]
    fn a_target_hf_outside_the_contracts_band_is_refused_at_parse() {
        assert_clean_environment();
        let parse_target = |value: &str| {
            try_parse(&[
                "liquidator",
                "--network",
                "testnet",
                "--rpc-url",
                "http://rpc",
                "--target-hf",
                value,
            ])
        };

        for refused in ["0", "0.9", "1.0299999", "1.15", "1.2", "2"] {
            assert!(
                parse_target(refused).is_err(),
                "TARGET_HF={refused} is outside the band this bot plans within"
            );
        }
        for accepted in ["1.03", "1.06", "1.1499999"] {
            assert!(
                parse_target(accepted).is_ok(),
                "TARGET_HF={accepted} names an outcome the contract accepts"
            );
        }
    }

    /// The contract's own comparisons are strict — `is_hf_over(1_1500000)`
    /// uses `>` and `is_hf_under(1_0300000)` uses `<` — so exactly 1.15
    /// is accepted by the contract and refused here on purpose. The
    /// refusal must say that it is the bot's own margin, not the
    /// contract's rule.
    #[test]
    fn the_target_band_is_the_bots_own_margin_and_says_so() {
        let error = target_health_factor("1.15").expect_err("refused");
        assert!(
            !error.contains("the band the pool contract accepts"),
            "the contract accepts 1.15; the refusal must not claim otherwise: {error}"
        );
        assert!(
            error.contains("drift"),
            "the refusal must say why the bot is narrower: {error}"
        );
        // Exactly 1.03 is accepted by both.
        assert!(target_health_factor("1.03").is_ok());
    }

    /// A liquidation threshold above the scan threshold names borrowers
    /// the full scan never flags, so the auctioneer would only ever reach
    /// them through an event or a price move. That is a relation between
    /// two knobs, so it is checked where both are visible rather than in
    /// either one's value parser.
    #[test]
    fn a_liquidation_threshold_above_the_scan_threshold_is_refused() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
            "--pools-toml",
            POOLS,
            "--scan-hf-threshold",
            "1.2",
            "--liq-hf-threshold",
            "1.3",
        ]);
        let error = args
            .service_with_secrets(Some("postgres://x".to_string()), None, None)
            .expect_err("refused");
        assert!(error.to_string().contains("LIQ_HF_THRESHOLD"), "{error}");

        // Equality is refused too: the scan flags strictly below its
        // threshold while the auctioneer liquidates at or below its own, so
        // a borrower exactly on a shared value is liquidatable and never
        // flagged.
        let equal = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
            "--pools-toml",
            POOLS,
            "--scan-hf-threshold",
            "1.2",
            "--liq-hf-threshold",
            "1.2",
        ]);
        let error = equal
            .service_with_secrets(Some("postgres://x".to_string()), None, None)
            .expect_err("equality refused");
        assert!(error.to_string().contains("at or above"), "{error}");
    }

    /// The auctioneer's key is environment-only, like every other secret: a
    /// key on the command line is readable in `ps`, `docker inspect` and
    /// `/proc/<pid>/cmdline`.
    #[test]
    fn the_auctioneer_key_is_not_an_argument() {
        assert_clean_environment();
        assert!(
            try_parse(&[
                "liquidator",
                "--network",
                "testnet",
                "--rpc-url",
                "http://rpc",
                "--auctioneer-secret-key",
                "SB…",
            ])
            .is_err(),
            "there is no such flag, and there must not be"
        );
    }

    /// Two valid test seeds for the key tests; any S… strkey this crate
    /// can decode will do, and neither has ever held funds.
    const FILLER_SEED: &str = "SAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC5MY";
    const AUCTIONEER_SEED: &str = "SABAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAFNE7";

    /// Unset, the auctioneer signs with the filler's key: one key is the
    /// ordinary deployment, and the spec makes the separate key the option
    /// rather than the requirement. A configured auctioneer key wins.
    #[test]
    fn the_auctioneer_falls_back_to_the_filler_key() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
        ]);

        let only_filler = args
            .signing_keys(Some(FILLER_SEED.to_string()), None)
            .expect("keys")
            .into_signers()
            .auctioneer
            .expect("a key is configured");
        let both = args
            .signing_keys(
                Some(FILLER_SEED.to_string()),
                Some(AUCTIONEER_SEED.to_string()),
            )
            .expect("keys")
            .into_signers()
            .auctioneer
            .expect("a key is configured");
        assert_ne!(
            only_filler.address(),
            both.address(),
            "the auctioneer key wins when set"
        );
        assert!(
            args.signing_keys(None, None)
                .expect("no key")
                .into_signers()
                .auctioneer
                .is_none(),
            "no key configured is not an error: dry-run needs none"
        );
    }

    /// The spec's "never for the filler or auctioneer addresses", pinned
    /// for the case that used to breach it: two *different* keys.
    ///
    /// Only one of them signs — the auctioneer's — so a set derived from
    /// the signing key alone holds one address, and the filler's own
    /// position is then a borrower this bot would happily create a
    /// liquidation auction against. The contract has no reason to refuse
    /// that, and in dry-run there is no contract to refuse at all.
    #[test]
    fn both_configured_keys_are_addresses_the_bot_refuses_to_act_on() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
        ]);

        let keys = args
            .signing_keys(
                Some(FILLER_SEED.to_string()),
                Some(AUCTIONEER_SEED.to_string()),
            )
            .expect("keys");
        let filler = crate::chain::Signer::from_secret(FILLER_SEED).expect("filler");
        let auctioneer = crate::chain::Signer::from_secret(AUCTIONEER_SEED).expect("auctioneer");
        assert_ne!(
            filler.address(),
            auctioneer.address(),
            "the two seeds must differ, or this test proves nothing"
        );

        let own = keys.own_addresses();
        assert_eq!(own.len(), 2, "both keys, not just the one that signs");
        assert!(own.contains(filler.address()), "the filler's address");
        assert!(
            own.contains(auctioneer.address()),
            "the auctioneer's address"
        );
        assert_eq!(
            keys.into_signers()
                .auctioneer
                .expect("a key is configured")
                .address(),
            auctioneer.address(),
            "and the auctioneer's key is still the one that signs"
        );

        // One key configured is one address; none is none, which excludes
        // nothing rather than everything.
        assert_eq!(
            args.signing_keys(Some(FILLER_SEED.to_string()), None)
                .expect("keys")
                .own_addresses()
                .len(),
            1
        );
        assert!(args
            .signing_keys(None, None)
            .expect("keys")
            .own_addresses()
            .is_empty());
    }

    /// A secret never renders, whichever key it is.
    #[test]
    fn the_auctioneer_signer_renders_as_its_address() {
        let signer = crate::chain::signer::Signer::from_secret(
            "SAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC5MY",
        )
        .expect("signer");
        let rendered = format!("{signer:?}");
        assert!(
            rendered.contains(signer.address()),
            "the address, not the seed: {rendered}"
        );
        assert!(!rendered.contains("SAAQCAIBAEAQ"), "the seed never renders");
    }

    /// A malformed auctioneer key is a configuration error, not a panic,
    /// and the error text never carries the secret.
    #[test]
    fn a_bad_auctioneer_key_is_a_config_error_that_never_echoes_it() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
        ]);
        let error = args
            .signing_keys(None, Some("not-a-valid-key".to_string()))
            .expect_err("a malformed key is refused");
        let rendered = error.to_string();
        assert!(!rendered.contains("not-a-valid-key"), "{rendered}");
        assert!(
            rendered.contains("AUCTIONEER_SECRET_KEY"),
            "the message names the variable: {rendered}"
        );
        assert!(matches!(error, LiquidatorError::Config(_)));

        // The filler's key is parsed too, even when the auctioneer's is
        // what would sign: an address the bot must not act on is worth a
        // startup error rather than a key that quietly does not exist.
        let error = args
            .signing_keys(
                Some("also-not-a-key".to_string()),
                Some(AUCTIONEER_SEED.to_string()),
            )
            .expect_err("a malformed filler key is refused too");
        let rendered = error.to_string();
        assert!(!rendered.contains("also-not-a-key"), "{rendered}");
        assert!(
            rendered.contains("FILLER_SECRET_KEY"),
            "the message names the variable: {rendered}"
        );
    }

    /// The five thresholds and cadence knobs, plus the startup delay, all
    /// reach `ServiceConfig` unchanged.
    #[test]
    fn the_auctioneer_knobs_reach_the_service_configuration() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
            "--pools-toml",
            POOLS,
            "--liq-hf-threshold",
            "0.99",
            "--target-hf",
            "1.1",
            "--oracle-scan-ledgers",
            "30",
            "--price-delta-bps",
            "100",
            "--plan-iterations",
            "3",
            "--startup-delay-ledgers",
            "12",
        ]);
        let config = args
            .service_with_secrets(Some("postgres://x".to_string()), None, None)
            .expect("configuration");
        assert_eq!(config.liquidation_health_factor, 9_900_000);
        assert_eq!(config.target_health_factor, 11_000_000);
        assert_eq!(config.oracle_scan_ledgers, 30);
        assert_eq!(config.price_delta_bps, 100);
        assert_eq!(config.plan_iterations, 3);
        assert_eq!(config.startup_delay_ledgers, 12);
    }

    /// `PLAN_ITERATIONS=0` is not "adjust nothing", it is "attempt
    /// nothing": the auctioneer's walk runs `0..plan_iterations`, so zero
    /// would skip every liquidation while logging that it had exhausted its
    /// iterations — a bot that looks busy and does nothing. Refusing it at
    /// parse time means the failure is a startup error, not that silent
    /// bot.
    #[test]
    fn a_zero_plan_iterations_is_refused_at_parse() {
        assert!(Args::try_parse_from(["liquidator", "--plan-iterations", "0"]).is_err());
    }

    /// `scan_due` never fires for a zero period, so a zero here would not
    /// mean "every ledger" — it would switch the oracle scan off for the
    /// life of the process, silently. Refused at parse like every other
    /// cadence knob.
    #[test]
    fn a_zero_oracle_scan_ledgers_is_refused_at_parse() {
        assert!(Args::try_parse_from(["liquidator", "--oracle-scan-ledgers", "0"]).is_err());
    }

    /// `PRICE_DELTA_BPS=0` is not "recheck on any move", it is "recheck on
    /// no move at all": an unchanged price computes a delta of `0`, which
    /// fails the "moved at least this much" test and falls through to the
    /// direction branch, where `price > reference` is false on equality —
    /// reporting a spurious `Down` and flagging every borrower on every
    /// scan, forever. Refusing it at parse time means the failure is a
    /// startup error, not that silent every-scan flood.
    #[test]
    fn a_zero_price_delta_bps_is_refused_at_parse() {
        assert!(Args::try_parse_from(["liquidator", "--price-delta-bps", "0"]).is_err());
    }

    /// The first pool of the sample file, for struct-update syntax in the
    /// rule tests below.
    fn sample_pool() -> PoolConfig {
        parse_pools(POOLS).expect("the sample parses").remove(0)
    }

    /// Spec §6's defaults for the filler's knobs.
    #[test]
    fn the_filler_knobs_default_to_the_spec() {
        assert_clean_environment();
        let args = Args::try_parse_from(["liquidator"]).unwrap();
        assert_eq!(args.hf_safety_multiplier.get(), 11_000_000, "1.1");
        assert_eq!(args.replan_ledgers, 10);
        assert_eq!(args.replan_near_ledgers, 5);
        assert_eq!(
            args.xlm_fee_reserve.get(),
            500_000_000,
            "50 XLM: XLM has 7 decimals, so a Decimal7 is its value in stroops"
        );
        assert_eq!(args.high_fee_profit_threshold.get(), 100_000_000, "10");
        assert_eq!(args.inventory_refresh_secs, 30);
    }

    /// A negative fee reserve would widen what the filler may spend by its
    /// magnitude; `Decimal7` refuses the sign, so it never reaches the
    /// inventory.
    #[test]
    fn a_negative_fee_reserve_is_refused_at_parse() {
        assert!(Args::try_parse_from(["liquidator", "--xlm-fee-reserve", "-1"]).is_err());
        assert!(Args::try_parse_from(["liquidator", "--xlm-fee-reserve", "0"]).is_ok());
    }

    /// Under one, the filler's floor would sit under the pool's own
    /// `min_health_factor` — the operator's stated minimum — and a fill could
    /// leave the filler below it by design.
    #[test]
    fn a_health_multiplier_under_one_is_refused_at_parse() {
        assert!(
            Args::try_parse_from(["liquidator", "--hf-safety-multiplier", "0.9999999"]).is_err()
        );
        assert!(Args::try_parse_from(["liquidator", "--hf-safety-multiplier", "1"]).is_ok());
    }

    /// `REPLAN_LEDGERS=0` would re-plan every auction on every ledger — not a
    /// cadence at all — and `INVENTORY_REFRESH_SECS=0` would read every wallet
    /// balance on every tick. Both refused like every other cadence knob.
    #[test]
    fn zero_filler_cadences_are_refused_at_parse() {
        assert!(Args::try_parse_from(["liquidator", "--replan-ledgers", "0"]).is_err());
        assert!(Args::try_parse_from(["liquidator", "--inventory-refresh-secs", "0"]).is_err());
        assert!(
            Args::try_parse_from(["liquidator", "--replan-near-ledgers", "0"]).is_ok(),
            "zero is meaningful here: re-plan only at the fill ledger itself"
        );
    }

    /// `FAILURE_NOTIFICATION_COOLDOWN_HOURS=0` would dedup nothing — every
    /// tick renotifies — which is not "no cooldown", it is "notify on every
    /// tick"; refused at parse rather than a silent notification flood.
    ///
    /// The other end is `Duration`'s, not a policy: `Duration::from_hours`
    /// panics above `u64::MAX / 3_600`, so the last hour count that can be
    /// turned into a `Duration` at all is the last one this accepts.
    #[test]
    fn a_zero_cooldown_is_refused_at_parse() {
        assert!(
            Args::try_parse_from(["liquidator", "--failure-notification-cooldown-hours", "0"])
                .is_err()
        );
        assert!(
            Args::try_parse_from(["liquidator", "--failure-notification-cooldown-hours", "1"])
                .is_ok()
        );
        let highest = u64::MAX / 3_600;
        let args = Args::try_parse_from([
            "liquidator",
            "--failure-notification-cooldown-hours",
            &highest.to_string(),
        ])
        .expect("the highest hour count `Duration::from_hours` accepts parses");
        assert_eq!(args.failure_notification_cooldown_hours, highest);
        // And it is a `Duration`, rather than the panic the unbounded
        // range let through.
        let _ = std::time::Duration::from_hours(args.failure_notification_cooldown_hours);
        assert!(
            Args::try_parse_from([
                "liquidator",
                "--failure-notification-cooldown-hours",
                &(highest + 1).to_string(),
            ])
            .is_err(),
            "one hour past it is a panic, so it is refused at parse"
        );
        assert_clean_environment();
        let args = Args::try_parse_from(["liquidator"]).unwrap();
        assert_eq!(args.failure_notification_cooldown_hours, 24);
    }

    /// Spec §6: "Keys parse and differ when both are given." Two roles on one
    /// key would need two queues on one key — the sequence race `queue.rs`
    /// exists to make unreachable — so the same key twice is refused, and
    /// leaving `AUCTIONEER_SECRET_KEY` unset is how one key signs both.
    #[test]
    fn the_same_key_twice_is_refused() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
        ]);
        let error = args
            .signing_keys(Some(FILLER_SEED.to_string()), Some(FILLER_SEED.to_string()))
            .expect_err("the same key twice");
        let message = error.to_string();
        assert!(
            message.contains("AUCTIONEER_SECRET_KEY") && message.contains("FILLER_SECRET_KEY"),
            "{message}"
        );
        assert!(
            !message.contains(FILLER_SEED),
            "the message never echoes the key"
        );
    }

    /// Spec §6: `FILLER_SECRET_KEY` is "required for live". An armed bot with
    /// only an auctioneer key would create auctions and never fill one.
    #[test]
    fn live_trading_needs_the_fillers_key() {
        assert_clean_environment();
        let live = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
            "--dry-run=false",
        ]);
        assert!(live.signing_keys(None, None).is_err(), "no key at all");
        assert!(
            live.signing_keys(None, Some(AUCTIONEER_SEED.to_string()))
                .is_err(),
            "an auctioneer key alone"
        );
        assert!(live
            .signing_keys(Some(FILLER_SEED.to_string()), None)
            .is_ok());

        let dry = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
        ]);
        assert!(
            dry.signing_keys(None, None).is_ok(),
            "a dry run needs no key"
        );
    }

    /// With no auctioneer key both roles hold the filler's one key — the same
    /// `Arc`, so `shared` can tell by pointer — and that is what gives them one
    /// queue. Two configured keys are two keys and two queues.
    #[test]
    fn a_fallback_auctioneer_shares_the_fillers_key() {
        assert_clean_environment();
        let args = parse(&[
            "liquidator",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
        ]);

        let one = args
            .signing_keys(Some(FILLER_SEED.to_string()), None)
            .unwrap()
            .into_signers();
        assert!(one.shared());
        assert_eq!(
            one.auctioneer.as_ref().unwrap().address(),
            one.filler.as_ref().unwrap().address()
        );

        let two = args
            .signing_keys(
                Some(FILLER_SEED.to_string()),
                Some(AUCTIONEER_SEED.to_string()),
            )
            .unwrap()
            .into_signers();
        assert!(!two.shared());
        assert_ne!(
            two.auctioneer.as_ref().unwrap().address(),
            two.filler.as_ref().unwrap().address()
        );

        let auctioneer_only = args
            .signing_keys(None, Some(AUCTIONEER_SEED.to_string()))
            .unwrap()
            .into_signers();
        assert!(
            auctioneer_only.filler.is_none(),
            "the filler never borrows the auctioneer's key"
        );
        assert!(auctioneer_only.auctioneer.is_some() && !auctioneer_only.shared());

        let none = args.signing_keys(None, None).unwrap().into_signers();
        assert!(none.auctioneer.is_none() && none.filler.is_none() && !none.shared());
    }

    /// A pool floor at or under the contract's own post-submit minimum,
    /// `1.0000100`, lets the filler plan fills the contract refuses as
    /// `InvalidHf` — every one of them a wasted simulation and re-plan.
    #[test]
    fn a_pool_floor_at_the_contracts_own_minimum_is_refused() {
        let at =
            parse_pools(&POOLS.replace("min_health_factor = 1.5", "min_health_factor = 1.00001"));
        assert!(
            matches!(&at, Err(error) if error.to_string().contains("min_health_factor")),
            "{at:?}"
        );
        assert!(parse_pools(
            &POOLS.replace("min_health_factor = 1.5", "min_health_factor = 1.0000101")
        )
        .is_ok());
    }

    /// Spec §5: an auction is a candidate only when every bid asset is in
    /// `supported_bid` and every lot asset in `supported_lot`; `*` matches
    /// any reserve.
    #[test]
    fn supported_assets_cover_every_asset_on_each_side() {
        let pool = PoolConfig {
            supported_bid: vec!["A".to_string(), "B".to_string()],
            supported_lot: vec!["*".to_string()],
            ..sample_pool()
        };
        assert!(pool.supports(&["A"], &["X", "Y"]));
        assert!(pool.supports(&["A", "B"], &["X"]));
        assert!(
            !pool.supports(&["A", "C"], &["X"]),
            "one unsupported bid asset refuses the auction"
        );
        let strict = PoolConfig {
            supported_lot: vec!["X".to_string()],
            ..pool
        };
        assert!(
            !strict.supports(&["A"], &["X", "Y"]),
            "one unsupported lot asset refuses it too"
        );
    }

    /// Spec §5: the margin is the first `profits` rule whose lists cover every
    /// auction asset, else `default_profit_bps`.
    #[test]
    fn the_first_matching_profit_rule_wins() {
        let pool = PoolConfig {
            default_profit_bps: 1_000,
            profits: vec![
                ProfitRule {
                    profit_bps: 500,
                    supported_bid: vec!["USDC".to_string()],
                    supported_lot: vec!["*".to_string()],
                },
                ProfitRule {
                    profit_bps: 200,
                    supported_bid: vec!["*".to_string()],
                    supported_lot: vec!["*".to_string()],
                },
            ],
            ..sample_pool()
        };
        assert_eq!(pool.profit_bps(&["USDC"], &["XLM"]), 500);
        assert_eq!(
            pool.profit_bps(&["XLM"], &["USDC"]),
            200,
            "the first rule does not cover an XLM bid"
        );
        let no_rules = PoolConfig {
            profits: Vec::new(),
            ..pool
        };
        assert_eq!(no_rules.profit_bps(&["XLM"], &["USDC"]), 1_000);
    }

    /// Neither `PORT` nor `HTTP_PORT` leaves the server off; either turns
    /// it on, and `PORT` wins when both are set — spec §6: Cloud Run
    /// injects `PORT`, so a deployment that also sets `HTTP_PORT` for some
    /// other reason must not silently lose the platform's binding.
    #[test]
    fn the_http_server_is_off_without_a_port_and_port_wins_over_http_port() {
        let args = Args::try_parse_from(["liquidator"]).expect("parses");
        assert_eq!(args.port, None);
        let config = minimal_service(&args);
        assert!(config.http.is_none());
        let args = Args::try_parse_from(["liquidator", "--port", "8080", "--http-port", "9090"])
            .expect("parses");
        let config = minimal_service(&args);
        let http = config.http.expect("a port turns the server on");
        assert_eq!(http.bind.port(), 8080, "PORT wins: Cloud Run injects it");
        assert_eq!(http.bind.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
        assert_eq!(http.max_lag_ledgers, 10);
    }

    /// `HEALTH_MAX_LAG_LEDGERS=0` is not "always ready", it is "ready only
    /// exactly at head" — a bound no poll interval can hold on every
    /// ledger boundary, so `/healthz` would flap between ready and
    /// not-ready forever. Refused at parse like every other cadence knob.
    #[test]
    fn a_zero_health_lag_is_refused_at_parse() {
        assert!(Args::try_parse_from(["liquidator", "--health-max-lag-ledgers", "0"]).is_err());
    }

    /// `TELEGRAM_CHAT_ID` or `TELEGRAM_BOT_TOKEN` alone is a startup
    /// error, never a half-configured channel; only both together build a
    /// `TelegramConfig`, and the token never renders through `Debug`.
    #[test]
    fn telegram_needs_both_the_token_and_the_chat_id() {
        assert_clean_environment();
        let args =
            Args::try_parse_from(["liquidator", "--telegram-chat-id", "12345"]).expect("parses");
        let error = args
            .service_with_secrets(Some(DB.into()), None, None)
            .expect_err("chat id alone");
        assert!(error.to_string().contains("TELEGRAM_BOT_TOKEN is not"));

        // An empty TELEGRAM_BOT_TOKEN counts as absent, exactly as an
        // empty RPC_API_KEY does.
        let error = args
            .service_with_secrets(Some(DB.into()), None, Some(String::new()))
            .expect_err("an empty token counts as absent too");
        assert!(error.to_string().contains("TELEGRAM_BOT_TOKEN is not"));

        let args = Args::try_parse_from(["liquidator"]).expect("parses");
        let error = args
            .service_with_secrets(Some(DB.into()), None, Some("123:abc".into()))
            .expect_err("token alone");
        assert!(error.to_string().contains("TELEGRAM_CHAT_ID is not"));

        let args = Args::try_parse_from([
            "liquidator",
            "--telegram-chat-id",
            "12345",
            "--network",
            "testnet",
            "--rpc-url",
            "http://rpc",
            "--pools-toml",
            POOLS,
        ])
        .expect("parses");
        let config = args
            .service_with_secrets(Some(DB.into()), None, Some("123:abc".into()))
            .expect("both");
        let telegram = config.telegram.as_ref().expect("configured");
        assert_eq!(telegram.chat_id, "12345");
        assert_eq!(telegram.token.expose(), "123:abc");
        // No argument or environment variable sets this: the token sits in
        // the request *path* (`.../bot<TOKEN>/sendMessage`), so pointing a
        // test at a mock server is a call to `TelegramChannel::with_base_url`,
        // never a configuration field.
        assert!(telegram.base_url.is_none());
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("123:abc"),
            "the token never renders: {rendered}"
        );
        assert!(rendered.contains("Secret(<redacted>)"));
    }

    /// The configuration reference, `.env.example` and the real `clap`
    /// definition name the same variables, and the example pools file is
    /// one `parse_pools` accepts. Documentation that has drifted from the
    /// code is worse than none, and nothing else would notice: every one
    /// of these files can go stale without a single other test failing.
    #[test]
    fn the_configuration_documents_cover_exactly_the_real_settings() {
        use clap::CommandFactory;
        let root = env!("CARGO_MANIFEST_DIR");
        let read_file = |path: &str| {
            std::fs::read_to_string(format!("{root}/{path}"))
                .unwrap_or_else(|error| panic!("{path}: {error}"))
        };
        let reference = read_file("docs/configuration.md");
        let template = read_file(".env.example");

        // Read straight from the environment, never through clap, so
        // `Args::command()` cannot see them. A new one must be added here.
        let direct = [
            "RPC_API_KEY",
            "DATABASE_URL",
            "TELEGRAM_BOT_TOKEN",
            "FILLER_SECRET_KEY",
            "AUCTIONEER_SECRET_KEY",
            "RUST_LOG",
        ];
        let mut real: std::collections::BTreeSet<String> = Args::command()
            .get_arguments()
            .filter_map(|arg| arg.get_env())
            .map(|env| env.to_string_lossy().into_owned())
            .collect();
        real.extend(direct.iter().map(|name| (*name).to_string()));

        for name in &real {
            assert!(
                reference.contains(&format!("`{name}`")),
                "docs/configuration.md never names `{name}`"
            );
            assert!(
                template.contains(&format!("{name}=")),
                ".env.example never names {name}"
            );
        }

        // The reverse: every variable `.env.example` names is real.
        let named = template.lines().filter_map(|line| {
            let line = line.trim_start_matches(['#', ' ']);
            let (name, _) = line.split_once('=')?;
            (!name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                .then_some(name)
        });
        for name in named {
            assert!(
                real.contains(name),
                ".env.example names {name}, which nothing reads"
            );
        }

        // The annotated example is a file the bot accepts; with
        // `deny_unknown_fields` on every table, that also proves each of
        // its keys is real.
        parse_pools(&read_file("pools.example.toml")).expect("pools.example.toml parses");
    }
}
