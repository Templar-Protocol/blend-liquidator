# Security

## Reporting a vulnerability

Report privately through [GitHub Security
Advisories](https://github.com/Templar-Protocol/blend-liquidator/security/advisories/new).
Do not open a public issue for a vulnerability.

## What this software is

A liquidation bot. It is **not** non-custodial: it is designed to hold a
signing key and submit transactions on its own, unsupervised. The threat model
follows from that.

**In scope**

- Anything that could exfiltrate, log, serialise or transport the signing key
  or an API token.
- Anything that could cause funds to move differently than the configured
  strategy directs — including a path that reaches live trading without
  `DRY_RUN=false` being set explicitly.
- Dependency vulnerabilities reachable from the release binary. `cargo-deny`
  gates these in CI; an advisory ignored in `deny.toml` must carry the
  reasoning and the command proving the crate is absent from the release build.

**Out of scope**

- The dev container and CI tooling, except where a weakness there could reach
  a released artefact.
- Anything requiring an attacker to already control the host the bot runs on.

## Operator guidance

- Never pass a secret as a command-line argument. Process arguments are not
  secret: `/proc/<pid>/cmdline` is world-readable and the value appears in
  `ps`, `docker inspect` and `docker compose config`. Use the environment.
- Keep `DRY_RUN=true` until you have watched a full cycle and agree with what
  it reports.
- Pin a release tag rather than `:latest` for anything left running.
