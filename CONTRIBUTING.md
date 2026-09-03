# Contributing

## Toolchain

`rust-toolchain.toml` pins the channel; `rustup` picks it up automatically.
The dev container installs it, plus `shellcheck` and `cargo-deny`, so CI's
gates are reproducible locally.

## Before opening a PR

```bash
make check
```

That runs exactly what CI runs: `cargo fmt --all --check`, `cargo clippy
--all-targets -- -D warnings`, `cargo test --lib --bins`, `cargo doc --no-deps`
with `RUSTDOCFLAGS=-D warnings`, `./scripts/check-repo-invariants.sh`, and
`shellcheck --severity=error`. `cargo-deny` runs in CI's `deny` job.

## Submitting a PR

Branch, then PR against `main`. Squash merge only. `main` requires signed
commits, a green `CI Summary`, and every review thread resolved — note that
**unresolved threads block the merge**, not review verdicts: there are no
required approvals.

If commit signing fails inside the dev container, read
`.devcontainer/git-signing.sh` — the durable fix is a literal `key::` public
key in the **host's** `~/.gitconfig`, which copies into every container on
every rebuild.

## Releases

Releases are tags (`vX.Y.Z`). Pushing one triggers
[`.github/workflows/release.yml`](.github/workflows/release.yml), which builds
and pushes `ghcr.io/templar-protocol/blend-liquidator:<tag>` and cuts a GitHub
Release.

```bash
# 1. Bump the version and refresh the lock
#    (edit Cargo.toml's `version`, then:)
cargo update -p blend-liquidator

# 2. Move CHANGELOG.md's [Unreleased] content into a `## [X.Y.Z] - YYYY-MM-DD`
#    section and update the README image pin to the new version

# 3. Check the working tree agrees before going further
./scripts/check-release.sh vX.Y.Z

# 4. COMMIT the bump and land it on main through a PR — the tag must point at
#    a merged commit, not at an uncommitted working tree
git switch -c release/vX.Y.Z && git commit -am "chore(release): vX.Y.Z"
#    ...open the PR, get it green, merge it...

# 5. Tag the MERGED commit and push
git switch main && git pull
git tag vX.Y.Z && git push origin vX.Y.Z
```

Step 4 is not optional bookkeeping. `check-release.sh` reads the **working
tree** while the `preflight` job reads the **tagged commit**, so bumping the
files without committing them lets step 3 pass locally and `preflight` fail
afterwards — once `vX.Y.Z` is already on origin and has to be deleted remotely
before you can retry.

`:latest` only moves for non-prerelease tags, so `vX.Y.Z-rc.1` publishes
without repointing `:latest` at a release candidate.

## Licence

By contributing, you agree your contribution is licensed under this repo's
[GPL-3.0-only licence](LICENSE).
