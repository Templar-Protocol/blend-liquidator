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

/// Everything the chain layer needs, validated. Built by [`Args::chain`].
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// Ledgers a submitted transaction stays valid and is polled for. Must
    /// be at least 1: the ledger bound is exclusive, so a zero window would
    /// make every transaction unlandable before it starts.
    #[arg(
        long,
        env = "TX_POLL_LEDGERS",
        default_value_t = 3,
        value_parser = clap::value_parser!(u32).range(1..),
    )]
    pub tx_poll_ledgers: u32,
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
}
