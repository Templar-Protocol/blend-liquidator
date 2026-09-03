//! CLI and environment configuration.

use clap::Parser;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The safety invariant, asserted from the first commit: absent any
    /// argument or environment, the bot is disarmed.
    #[test]
    fn dry_run_defaults_to_true() {
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
}
