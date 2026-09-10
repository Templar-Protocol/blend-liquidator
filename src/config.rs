//! CLI and environment configuration.

use clap::Parser;

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
        pools.push(PoolConfig {
            address: pool.address,
            primary_asset: pool.primary_asset,
            min_primary_collateral,
            min_health_factor: pool.min_health_factor.get(),
            default_profit_bps: pool.default_profit_bps,
            force_fill: pool.force_fill,
            supported_bid: pool.supported_bid,
            supported_lot: pool.supported_lot,
            profits: pool.profits,
        });
    }
    Ok(pools)
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

/// Everything the service needs, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Network, RPC and fee configuration.
    pub chain: ChainConfig,
    /// Postgres. May carry a password, so it never renders.
    pub database_url: Secret,
    /// Connections in the pool.
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
    pub liq_hf_threshold: i128,
    /// The health factor a liquidation aims to leave the borrower at, 7
    /// decimals.
    pub target_hf: i128,
    /// How often, in ledgers, prices are re-read for a significant move.
    pub oracle_scan_ledgers: u32,
    /// How far a price must move, in basis points, to be worth rechecking
    /// the borrowers exposed to it.
    pub price_delta_bps: u32,
    /// How many times a rejected percent is adjusted against the contract's
    /// own answer before the borrower is left until the next recheck.
    pub plan_iterations: u32,
    /// Ledger ticks to wait after startup before any submission is
    /// attempted.
    pub startup_delay_ledgers: u32,
    /// Seeding.
    pub seed: SeedConfig,
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
    #[arg(
        long,
        env = "DATABASE_MAX_CONNECTIONS",
        default_value_t = 5,
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

    /// How many stale users to refresh per tick.
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
    /// The contract refuses a post-liquidation health factor at or above
    /// `1.15` (`InvalidLiqTooLarge`) or below `1.03` (`InvalidLiqTooSmall`),
    /// so this sits between them with room for the auction to be filled a
    /// ledger or two later than planned.
    #[arg(long, env = "TARGET_HF", default_value = "1.06")]
    pub target_hf: Decimal7,

    /// How often, in ledgers, prices are re-read and a significant move
    /// flags the borrowers it moved against.
    #[arg(long, env = "ORACLE_SCAN_LEDGERS", default_value_t = 60)]
    pub oracle_scan_ledgers: u32,

    /// How far a price must move from its reference, in basis points, to be
    /// worth rechecking the borrowers exposed to it.
    #[arg(long, env = "PRICE_DELTA_BPS", default_value_t = 250)]
    pub price_delta_bps: u32,

    /// How many times a rejected percent is adjusted against the contract's
    /// own answer before the borrower is left until the next recheck.
    #[arg(long, env = "PLAN_ITERATIONS", default_value_t = 5)]
    pub plan_iterations: u32,

    /// Ledger ticks to wait after startup before any submission is
    /// attempted.
    ///
    /// Zero by default, so a fresh deployment submits as soon as it is
    /// ready. A nonzero value gives a poller that is catching up on a
    /// backlog room to reach current chain state before the bot starts
    /// acting on health factors it has not yet re-verified against it.
    #[arg(long, env = "STARTUP_DELAY_LEDGERS", default_value_t = 0)]
    pub startup_delay_ledgers: u32,

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

    /// The service configuration, reading both secrets from the environment.
    pub fn service(&self) -> Result<ServiceConfig, LiquidatorError> {
        self.service_with_secrets(
            std::env::var("DATABASE_URL")
                .ok()
                .filter(|url| !url.is_empty()),
            std::env::var("RPC_API_KEY")
                .ok()
                .filter(|key| !key.is_empty()),
        )
    }

    /// What `service` does after reading the environment, separated so
    /// tests never touch process-global state.
    pub fn service_with_secrets(
        &self,
        database_url: Option<String>,
        rpc_api_key: Option<String>,
    ) -> Result<ServiceConfig, LiquidatorError> {
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
            liq_hf_threshold: self.liq_hf_threshold.get(),
            target_hf: self.target_hf.get(),
            oracle_scan_ledgers: self.oracle_scan_ledgers,
            price_delta_bps: self.price_delta_bps,
            plan_iterations: self.plan_iterations,
            startup_delay_ledgers: self.startup_delay_ledgers,
            seed: SeedConfig {
                url: Some(self.seed_url.clone()).filter(|url| !url.is_empty()),
                health_factor_max: self.seed_hf_max.get(),
                file: self.seed_file.clone(),
            },
        })
    }

    /// The key the auctioneer signs with: `AUCTIONEER_SECRET_KEY` when set,
    /// otherwise `FILLER_SECRET_KEY`, and `None` when neither is — which is
    /// the ordinary dry-run deployment and not an error.
    ///
    /// Both arrive from the environment, never from argv: a signing key on
    /// the command line is readable from `/proc/<pid>/cmdline`, `ps` and
    /// `docker inspect`.
    pub fn auctioneer_signer(
        &self,
        filler: Option<String>,
        auctioneer: Option<String>,
    ) -> Result<Option<crate::chain::Signer>, LiquidatorError> {
        let Some(secret) = auctioneer.or(filler) else {
            return Ok(None);
        };
        crate::chain::Signer::from_secret(&secret)
            .map(Some)
            .map_err(|error| LiquidatorError::Config(format!("auctioneer key: {error}")))
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
            "SEED_URL",
            "SEED_HF_MAX",
            "SEED_FILE",
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
            args.service_with_secrets(None, None),
            Err(LiquidatorError::Config(_))
        ));
        let config = args
            .service_with_secrets(Some("postgres://u:p@localhost/db".to_string()), None)
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
            neither.service_with_secrets(Some("postgres://x".to_string()), None),
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
            .service_with_secrets(Some("postgres://x".to_string()), None)
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
        // Two valid test seeds; any S… strkey this crate can decode will do.
        let filler = "SAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC5MY";
        let auctioneer = "SABAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAFNE7";

        let only_filler = args
            .auctioneer_signer(Some(filler.to_string()), None)
            .expect("signer")
            .expect("a key is configured");
        let both = args
            .auctioneer_signer(Some(filler.to_string()), Some(auctioneer.to_string()))
            .expect("signer")
            .expect("a key is configured");
        assert_ne!(
            only_filler.address(),
            both.address(),
            "the auctioneer key wins when set"
        );
        assert!(
            args.auctioneer_signer(None, None)
                .expect("no key")
                .is_none(),
            "no key configured is not an error: dry-run needs none"
        );
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
            rendered.starts_with('G') || rendered.contains('G'),
            "the address, not the seed"
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
            .auctioneer_signer(None, Some("not-a-valid-key".to_string()))
            .expect_err("a malformed key is refused");
        let rendered = error.to_string();
        assert!(!rendered.contains("not-a-valid-key"), "{rendered}");
        assert!(matches!(error, LiquidatorError::Config(_)));
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
            .service_with_secrets(Some("postgres://x".to_string()), None)
            .expect("configuration");
        assert_eq!(config.liq_hf_threshold, 9_900_000);
        assert_eq!(config.target_hf, 11_000_000);
        assert_eq!(config.oracle_scan_ledgers, 30);
        assert_eq!(config.price_delta_bps, 100);
        assert_eq!(config.plan_iterations, 3);
        assert_eq!(config.startup_delay_ledgers, 12);
    }
}
