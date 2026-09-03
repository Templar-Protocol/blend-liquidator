# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- The repository itself: a Rust service scaffold for a Blend Protocol liquidation bot, green on its first commit. CI gates `cargo fmt`/`clippy -D warnings`/`test`/`doc -D warnings`, `cargo-deny`, the Docker build, `shellcheck` and the three-way Rust version pin behind one aggregate `CI Summary` check — the single required status check, so adding or renaming a job never needs a ruleset edit. `clippy::pedantic` is warn-level with `unwrap_used = "deny"` from the first commit, which is the cheap moment: retrofitting that onto an existing codebase is not. The dev container pins its base image by digest and its features by exact version, with a lock file CI verifies.
- `DRY_RUN` / `--dry-run`, defaulting to `true`, before there is anything to trade. The parser accepts only the literal strings `true` and `false`; `1`, `yes` and `on` are refused at startup. Every extra spelling is another way into live trading, and the dangerous direction is silent — a `DRY_RUN=yes` read as false would arm the bot while reading, to the operator, like it had been disarmed. This is the invariant most expensive to retrofit: a bot that defaults to live and is made safe-by-default later leaves every existing deployment silently changing behaviour on upgrade.
