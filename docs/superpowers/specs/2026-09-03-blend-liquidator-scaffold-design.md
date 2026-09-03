# blend-liquidator: repository scaffold

**Date:** 2026-09-03
**Status:** approved in outline, pending spec review

## What this is

A new private repository, `Templar-Protocol/blend-liquidator`, scaffolded to the
same standard as `Templar-Protocol/templar-liquidator` but carrying none of its
business logic. The deliverable is a repository that is green on its first
commit and ready to receive a liquidation bot for [Blend
Capital](https://blend.capital)'s lending pools on Stellar/Soroban.

This is deliberately **not** a fork. templar-liquidator's value here is its
*operational* scaffolding — the CI gate shape, the dev container pins, the
release preflight, the lint posture — not its NEAR integration, which does not
transfer to Soroban at all.

## Decisions

| Question | Decision |
|---|---|
| Owner | `Templar-Protocol` org, **private** |
| Target venue | Blend Protocol lending pools on Stellar (Soroban) |
| Skeleton depth | Minimal and CI-green: lib root, `main.rs`, `config.rs`, one test |
| Workflows | `ci`, `claude-review`, `release`, `devcontainer`, `dependabot`; **no** `sandbox` |
| License | GPL-3.0-only |
| Rust edition / MSRV | 2021 / 1.97.0 |

## Principle: carry the reasoning, not the facts

Every file adapted from templar-liquidator is rewritten by hand rather than
copied and pruned. The distinction that matters is between comments that record
*why a decision was made* and comments that assert *facts about a codebase*.

The first kind transfers and is worth keeping — it was paid for once already:

- The dev container stays on bookworm to match the production builder's glibc,
  and the cost of that parity is that Ubuntu-24.04-built release binaries need
  `GLIBC_2.39` and will not run.
- `docker-in-docker` must stay on 2.x or newer, because 1.x installs Compose v1
  through the system Python and bookworm refuses that under PEP 668 — which
  broke every arm64 rebuild while amd64 stayed green.
- `CI Summary` treats a *skipped* job as a failure, because a green summary once
  hid a pipeline that ran no tests at all.

The second kind does not transfer and is deleted outright, not reworded: THE
SINGLE-REV RULE, `near-cli-rs`, RedStone, Pyth, the oracle freshness model. A
scaffold whose comments confidently describe absent code is worse than one with
no comments, and this organisation has already paid to remove exactly that
(`d00a1df`, "dead code and dead comments out").

## Repository settings

Mirrors templar-liquidator: squash-merge only, delete-branch-on-merge, issues
enabled. Description: *"Liquidation bot for Blend Protocol lending pools on
Stellar"*.

A `Merge` ruleset on `main` replicating templar-liquidator's exactly:

- `deletion` — the branch cannot be deleted
- `pull_request` — 0 required approvals, `required_review_thread_resolution`,
  `require_extra_approval_for_unattributed_changes`, squash + merge allowed
- `required_signatures`
- `required_status_checks` — the single context `CI Summary`
- `copilot_code_review` with `review_on_push`

Requiring only `CI Summary` rather than each job by name is the point of that
aggregate job: adding or renaming a CI job never needs a matching ruleset edit,
only a `needs:` entry.

Labels: templar-liquidator's custom set minus `pre-existing`, which encodes a
migration from the contracts monorepo that never happened here — `security`,
`chore`, `claude-review`, `dependencies`, `feature`, `fix`, `infrastructure`,
`skip-changelog`.

Secret `CLAUDE_CODE_OAUTH_TOKEN` is created with a placeholder value for the
operator to overwrite. `claude-review.yml` will fail until it is replaced; that
failure is loud and self-explanatory, which is preferable to the workflow
silently not running.

### Known risk: Copilot review on a private repository

`copilot_code_review` is a rule in templar-liquidator's ruleset, and that
repository is public. Copilot code review on a **private** repository depends on
the org's Copilot plan, so applying the ruleset may be rejected or the rule may
sit inert. If it is rejected, the ruleset is applied without that rule and the
gap is reported rather than silently worked around — the remaining rules
(`required_signatures`, `CI Summary`, thread resolution) are the ones that
actually gate correctness, and `claude-review.yml` covers review independently.

### Order of operations

The ruleset requires pull requests and signed commits, so it cannot be in force
when the repository has no `main` to open a pull request against. Therefore:

1. Create the repository, private, with no auto-initialised content.
2. Push the scaffold directly to `main`.
3. Apply settings, labels, ruleset, and the placeholder secret.

Every commit is signed with the key established on 2026-09-03 (see
`.devcontainer/git-signing.sh` and the note below).

## Dev container

Same shape as templar-liquidator, with the NEAR-specific weight removed.

- Base image pinned by **digest**, not just tag, so a rebuild cannot silently
  pull a different image without a reviewed PR. Same bookworm base, for the same
  glibc-parity reason.
- Features pinned to exact versions for the same reason: `github-cli`,
  `docker-in-docker` (≥2.x), `node` 22.
- Named volumes `blend-liquidator-claude-code` and `blend-liquidator-gh-config`,
  distinct from templar-liquidator's so the two repositories do not share
  credential state. These hold credentials and live in Docker's volume store,
  outside anything `.gitignore` covers.
- `node` is retained but its rationale is rewritten. In templar-liquidator it is
  a *build* requirement — a transitive dependency's `build.rs` shells out to
  npm. Here nothing in the crate needs it; it is present for the Claude Code CLI
  that `post-create.sh` installs.

### Deliberate omissions

`near-cli-rs` is dropped, and the `available_kb()` cgroup-aware memory sizing
helper goes with it — its only caller was that build. Nothing else in the
scaffold compiles a large dependency tree during container creation.

The `stellar` CLI is **not** added yet. It is a multi-minute source build on
every container rebuild, and the crate has no Soroban dependency to use it with.
It and `available_kb()` return together when the first one lands; the helper is
not kept as dead code in the meantime.

### Commit signing

`git-signing.sh` is carried over including its `key::` early-exit (PR #75 in
templar-liquidator), so this repository never reproduces the false "commit
signing is on but … `git commit` will fail" warning on every container start.
Its header documents the literal-key form in the host's `~/.gitconfig` as the
durable fix and the script itself as a fallback for hosts still configured with
a path.

## CI

`ci.yml` keeps templar-liquidator's job set and its SHA-pinned actions:

| Job | Contents |
|---|---|
| `lint-test` | `cargo fmt --check`, `clippy --all-targets -D warnings`, `test --lib --bins`, `doc --no-deps` with `RUSTDOCFLAGS=-D warnings` |
| `deny` | `cargo-deny-action` |
| `docker` | image build, no push |
| `shellcheck` | gated at `error`, matching templar-liquidator's threshold |
| `invariants` | `scripts/check-repo-invariants.sh` |
| `CI Summary` | aggregate gate; a skipped job counts as a failure |

`check-repo-invariants.sh` survives but halves. THE SINGLE-REV RULE has nothing
to check — there are no `templar-*` git dependencies. The **three-way Rust
version pin** does apply and stays: `Cargo.toml`'s `rust-version`,
`rust-toolchain.toml`'s `channel`, and the Dockerfile builder's `FROM` must
agree, because a drift there fails by having CI and Docker cheerfully compile
syntax the declared MSRV does not support.

`claude-review.yml` is carried over near-verbatim — the concurrency grouping,
the fork/Dependabot exclusions, the janitor sweep for cancelled runs' tracking
comments — with the review prompt's repo-specific hazards rewritten for this
codebase.

`devcontainer.yml` becomes **amd64-only**. Free arm64 runners are a public-repo
benefit and this repository is private, so the dual-arch matrix would bill.
This is a real loss, recorded here so it is a decision rather than an oversight:
the arm64 job exists precisely to catch Apple-Silicon-only container breakage,
which is the exact failure that motivated writing it. Revisit if the repository
goes public or the cost proves acceptable.

`release.yml` is carried over with its `check-release.sh` preflight, publishing
to GHCR. The image is private, so pulls need a token.

`sandbox.yml` is dropped: it builds NEAR contract wasms at a pinned contracts
rev for an `#[ignore]`d sandbox test that does not exist here.

`dependabot.yml` keeps the cargo / github-actions / docker-digest /
devcontainer-feature ecosystems and their grouping, drops the `templar-*` ignore
rules, and keeps the docker version-bump ignore that protects the three-way Rust
pin.

## The crate

```
Cargo.toml          name = "blend-liquidator", version 0.1.0, GPL-3.0-only
                    empty [workspace] so no enclosing workspace is adopted
src/liquidator.rs   lib root, crate-level docs
src/main.rs         bin "liquidator"
src/config.rs       Args (clap, derive + env)
```

Dependencies are trimmed to what the skeleton actually uses: `clap`, `tokio`,
`tracing`, `tracing-subscriber`, `thiserror`, `serde`. No NEAR stack, no HTTP
client, no oracle clients — those arrive with the code that needs them.

The `[lints.clippy]` block is copied intact, including `pedantic` at warn and
`unwrap_used = "deny"`, with `clippy.toml`'s `allow-unwrap-in-tests` /
`allow-expect-in-tests`. Establishing the lint posture before there is code to
lint is the cheap moment to do it; retrofitting `unwrap_used = "deny"` onto an
existing codebase is not.

`main.rs` sets up tracing, parses `Args`, logs the resolved configuration, and
exits. One unit test asserts the dry-run default so `cargo test --lib --bins` is
green from the first commit.

### Dry-run defaults to true from commit one

`--dry-run` / `DRY_RUN` defaults to `true` before there is anything to trade.
This costs nothing now and is the safety invariant that is most expensive to
retrofit: a bot that defaults to live and is switched to safe-by-default later
has a window in which every existing deployment silently changes behaviour. The
flag takes an optional value so it works from argv-only surfaces — bare
`--dry-run` means true, `--dry-run=false` opts out — and the environment
variable parses only the literal strings `true` and `false`.

## Also shipped

`Dockerfile` (multi-stage, digest-pinned, no npm in the builder), `Makefile`,
`docker-compose.yml`, `.dockerignore`, `.gitignore`, `.env.example`,
`README.md`, `CLAUDE.md`, `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`,
`deny.toml`, `rust-toolchain.toml`, `.claude/commands/{fix-pr,fix-pr-auto}.md`,
and `scripts/{check-repo-invariants,check-release,claude-review-api,sweep-claude-review-comments}.sh`.

templar-liquidator's `deploy.sh`, `init-server.sh`, `run-mainnet.sh`,
`run-testnet.sh` and `setup-loki-grafana.sh` are specific to that bot's
deployment and are not carried over.

## Out of scope

Any Blend or Soroban integration: the pool client, position scanning, oracle
pricing, liquidation sizing, transaction submission. This spec covers the
repository only. The architecture of the bot itself is a separate design, and
deliberately so — templar-liquidator's module layout is a reasonable prior but
presuming it fits Blend before reading Blend's contracts would be a guess
dressed as a decision.
