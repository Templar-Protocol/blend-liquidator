#!/usr/bin/env bash
# check-repo-invariants.sh — enforce the cross-file pins that CI cannot
# otherwise catch.
#
# THE THREE-WAY RUST PIN. Cargo.toml's `rust-version` (the declared MSRV),
# rust-toolchain.toml's `channel` (what CI and local dev actually build with),
# and the Dockerfile builder's `FROM rust:X-bookworm` must agree.
#
# It gets its own gate because the failure does not name itself: a `channel`
# newer than `rust-version` makes CI and Docker cheerfully compile syntax the
# declared MSRV does not support, so everything is green until someone on the
# declared minimum tries to build.
#
# ONE STELLAR-STRKEY. Cargo.toml depends on stellar-strkey directly, to decode
# an S… secret, at the version stellar-xdr itself depends on, so Cargo.lock
# holds one copy. Nothing else notices a second: cargo-deny allows multiple
# versions, Dependabot ignores stellar-strkey (see .github/dependabot.yml), and
# a stellar-xdr bump that moves its own copy still compiles against the old
# one. This check is what says to bump Cargo.toml's in that same PR.
#
# ONE STELLAR CLI VERSION. The dev container and the nightly sandbox workflow
# both install the stellar CLI, and both must take it from
# scripts/sandbox/versions.env. Two versions is two different sandboxes, one
# of which nobody can reproduce, and nothing fails to say so.
#
# ONE SQLX-CLI VERSION, for the same reason and with a sharper failure: the
# dev container and CI both install sqlx-cli, and the committed offline query
# metadata in .sqlx/ is what they have to agree about. A container on another
# sqlx-cli regenerates a file CI then rejects, and the diff blames the query
# rather than the tool.
#
# ONE LIST OF SANDBOX SCENARIOS. deploy.sh, the Makefile, the Rust harness,
# the nightly matrix and the test fns each name the tier's scenarios, and a
# scenario missing from any one of them is a scenario that silently never
# deploys, never runs nightly, or runs under plain `cargo test`.
#
# Run it locally the same way CI does: ./scripts/check-repo-invariants.sh
set -euo pipefail

cd "$(dirname "$0")/.."

fail=0
note() { printf '  %s\n' "$*"; }
bad() { printf '::error::%s\n' "$*"; fail=1; }

echo "Rust version pins"
cargo_rv=$(grep -oE '^rust-version\s*=\s*"[^"]+"' Cargo.toml | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' || true)
toolchain=$(grep -oE '^channel\s*=\s*"[^"]+"' rust-toolchain.toml | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' || true)
docker_rv=$(grep -oE '^FROM rust:[0-9]+\.[0-9]+(\.[0-9]+)?' Dockerfile | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' || true)

for pair in "Cargo.toml rust-version:${cargo_rv}" "rust-toolchain.toml channel:${toolchain}" "Dockerfile FROM rust:${docker_rv}"; do
	if [ -z "${pair#*:}" ]; then
		bad "could not parse ${pair%%:*}"
	else
		note "${pair%%:*} = ${pair#*:}"
	fi
done

if [ -n "${cargo_rv}" ] && [ -n "${toolchain}" ] && [ -n "${docker_rv}" ]; then
	if [ "${cargo_rv}" != "${toolchain}" ] || [ "${cargo_rv}" != "${docker_rv}" ]; then
		bad "the three Rust pins disagree — bump all three together (see rust-toolchain.toml's comment)"
	else
		note "all three agree"
	fi
fi

echo
echo "One stellar-strkey"
# Cargo.lock writes each package's `version` on the line after its `name`.
strkey=$(grep -A1 -xF 'name = "stellar-strkey"' Cargo.lock | grep -oE '^version = "[^"]+"' | grep -oE '[0-9][^"]*' | paste -sd' ' - || true)
case "${strkey}" in
	"") bad "could not find stellar-strkey in Cargo.lock" ;;
	*" "*) bad "Cargo.lock holds stellar-strkey ${strkey}, a second copy — pin Cargo.toml's to the version stellar-xdr depends on (see its comment)" ;;
	*) note "Cargo.lock = ${strkey}, one copy" ;;
esac

echo
echo "One stellar CLI version"
# scripts/sandbox/versions.env holds every pin the sandbox tier builds on,
# the stellar CLI's among them. Two places install that CLI — the dev
# container's post-create and the nightly sandbox workflow — and both must
# read the version from that file rather than repeat it. A workflow pinned
# to a CLI the dev container does not have reproduces neither a failure nor
# a success, and nothing about the disagreement names itself: both sides
# install *a* CLI, both are green, and only the contract deployments differ.
#
# Two checks, because either alone passes something broken: that the file is
# read at all (on a line that is not a comment — a stale shellcheck
# directive is not a reference), and that neither file names a release
# literally, which is what reading versions.env and then ignoring it looks
# like.
cli_version=$(grep -oE '^STELLAR_CLI_VERSION=.+' scripts/sandbox/versions.env | cut -d= -f2 || true)
if [ -z "${cli_version}" ]; then
	bad "could not parse STELLAR_CLI_VERSION from scripts/sandbox/versions.env"
else
	note "scripts/sandbox/versions.env pins the stellar CLI at ${cli_version}"
fi

for file in .devcontainer/post-create.sh .github/workflows/sandbox.yml; do
	if [ ! -f "${file}" ]; then
		bad "${file} is missing — it is one of the two places that install the stellar CLI, and both must take the version from scripts/sandbox/versions.env"
	# No `-q` on the consumer: `grep -q` exits at its first match while the
	# producer is still writing, the producer takes SIGPIPE, and under
	# `pipefail` a match then reads as a failure. Discarding the output lets
	# the consumer drain its input, so the producer cannot be cut off.
	elif ! grep -vE '^[[:space:]]*#' "${file}" | grep -F 'versions.env' >/dev/null; then
		bad "${file} does not read scripts/sandbox/versions.env — the stellar CLI version (${cli_version}) lives in that one file, and ${file} must source or grep it rather than pin its own"
	elif grep -qE 'stellar-cli-[0-9]' "${file}"; then
		bad "${file} names a stellar CLI release literally (stellar-cli-…) — take the URL and its checksum from scripts/sandbox/versions.env instead, which is the only place the version belongs"
	else
		note "${file} reads scripts/sandbox/versions.env"
	fi
done

echo
echo "One sqlx-cli version"
# Two places install sqlx-cli — CI's test job and the dev container's
# post-create — and they must install the same one: the two sides share the
# committed .sqlx/ offline metadata, so a container on another sqlx-cli
# regenerates a file CI then rejects, and the diff blames the query rather
# than the tool.
#
# They are held together differently, because the workflow is the PR gate and
# is not this loop's to rewrite. post-create.sh *reads* SQLX_CLI_VERSION from
# scripts/sandbox/versions.env; ci.yml *names the literal* on its install
# line, and this check compares the two — the same shape as the three-way Rust
# pin above, where the guarantee is equality across files rather than a single
# reader.
sqlx_version=$(grep -oE '^SQLX_CLI_VERSION=.+' scripts/sandbox/versions.env | cut -d= -f2- || true)
if [ -z "${sqlx_version}" ]; then
	bad "could not parse SQLX_CLI_VERSION from scripts/sandbox/versions.env"
else
	note "scripts/sandbox/versions.env pins sqlx-cli at ${sqlx_version}"
fi

post_create=.devcontainer/post-create.sh
if [ ! -f "${post_create}" ]; then
	bad "${post_create} is missing — it is one of the two places that install sqlx-cli, and it must take the version from scripts/sandbox/versions.env"
elif ! grep -vE '^[[:space:]]*#' "${post_create}" | grep -F 'SQLX_CLI_VERSION' >/dev/null; then
	bad "${post_create} does not read SQLX_CLI_VERSION from scripts/sandbox/versions.env — it must grep or source it rather than pin its own (on a line that is not a comment)"
elif grep -qE 'sqlx-cli[[:space:]]+--version[[:space:]]+[0-9]' "${post_create}"; then
	bad "${post_create} names an sqlx-cli version literally — read SQLX_CLI_VERSION from scripts/sandbox/versions.env instead"
else
	note "${post_create} reads SQLX_CLI_VERSION from scripts/sandbox/versions.env"
fi

ci_workflow=.github/workflows/ci.yml
ci_sqlx=$(grep -oE 'sqlx-cli[[:space:]]+--version[[:space:]]+[0-9][^[:space:]]*' "${ci_workflow}" 2>/dev/null | grep -oE '[0-9][^[:space:]]*$' || true)
if [ ! -f "${ci_workflow}" ]; then
	bad "${ci_workflow} is missing — it is one of the two places that install sqlx-cli"
elif [ -z "${ci_sqlx}" ]; then
	bad "${ci_workflow} does not install sqlx-cli at a literal version (cargo install sqlx-cli --version <X>) — that literal is what scripts/sandbox/versions.env's SQLX_CLI_VERSION (${sqlx_version}) is compared against"
elif [ -n "${sqlx_version}" ] && [ "${ci_sqlx}" != "${sqlx_version}" ]; then
	bad "sqlx-cli versions disagree: ${ci_workflow} installs ${ci_sqlx}, scripts/sandbox/versions.env pins SQLX_CLI_VERSION=${sqlx_version} (which .devcontainer/post-create.sh installs) — bump both together"
else
	note "${ci_workflow} installs sqlx-cli ${ci_sqlx}, matching versions.env"
fi

echo
echo "One list of sandbox scenarios"
# The sandbox tier's scenarios are named in five places that no build step
# derives from one another — a shell script, a Makefile, a Rust const, a
# workflow matrix and the test fns themselves — and a disagreement is
# silent in the worst direction: `make sandbox-test` refuses a name that
# matches no test fn, but a scenario missing from the matrix is never run
# nightly at all, one missing from deploy.sh can never be deployed for, and
# a test fn that has lost its #[ignore] runs under plain `cargo test`. So
# the five must name the same set, duplicates included, and each is parsed
# from the one shape it is written in, a shape this cannot read being a
# failure rather than an empty list that happens to agree with another.
#
# `|| true` on each parse, because under `set -euo pipefail` a grep that
# matches nothing fails the assignment, which would abort here instead of
# reaching the message that names the file. No consumer below exits before
# its input ends (no `grep -q`, no `head`), so no producer can be cut off by
# SIGPIPE.
scenario_set() { tr -s '[:space:],|' '\n' | sed '/^$/d' | sort | paste -sd' ' -; }

scenarios_deploy=$(grep -E '^sandbox_scenarios=\(' scripts/sandbox/deploy.sh | sed -E 's/^sandbox_scenarios=\(([^)]*)\).*/\1/' | scenario_set || true)
scenarios_make=$(grep -E '^SANDBOX_SCENARIOS[[:space:]]*\?=' Makefile | sed -E 's/^[^=]*\?=//' | scenario_set || true)
# The const may be rustfmt'd onto one line or several: everything from its
# declaration to the first `];` is the array, and its quoted strings are
# the names.
scenarios_harness=$(awk '/const SCENARIOS/ { on = 1 } on { print } on && /\];/ { exit }' tests/sandbox_harness/mod.rs | grep -oE '"[^"]*"' | tr -d '"' | scenario_set || true)
# A one-line flow sequence, `scenario: [a, b, …]`, which is how the matrix
# is written; a block sequence would parse as nothing and fail below.
scenarios_matrix=$(grep -E '^[[:space:]]+scenario:[[:space:]]*\[' .github/workflows/sandbox.yml | sed -E 's/^[^[]*\[([^]]*)\].*/\1/' | scenario_set || true)
# Every fn whose attributes include #[ignore] or #[ignore = "…"]: the
# attribute lines between the #[ignore] and the fn are skipped, and
# anything else in between ends the match.
scenarios_tests=$(awk '
	/^[ \t]*#\[ignore([ \t]*=|\])/ { pending = 1; next }
	pending && /(^|[ \t])fn[ \t]+[A-Za-z_]/ {
		name = $0
		sub(/^.*fn[ \t]+/, "", name)
		sub(/[^A-Za-z0-9_].*$/, "", name)
		print name
		pending = 0
		next
	}
	pending && !/^[ \t]*(#\[|\/\/)/ { pending = 0 }
' tests/liquidation_sandbox.rs | scenario_set || true)

scenario_sources=(
	"scripts/sandbox/deploy.sh sandbox_scenarios:${scenarios_deploy}"
	"Makefile SANDBOX_SCENARIOS:${scenarios_make}"
	"tests/sandbox_harness/mod.rs SCENARIOS:${scenarios_harness}"
	".github/workflows/sandbox.yml matrix.scenario:${scenarios_matrix}"
	"tests/liquidation_sandbox.rs #[ignore]d test fns:${scenarios_tests}"
)
scenarios_agree=1
for source in "${scenario_sources[@]}"; do
	if [ -z "${source#*:}" ]; then
		bad "could not parse the sandbox scenarios from ${source%%:*}"
		scenarios_agree=0
	elif [ "${source#*:}" != "${scenarios_deploy}" ]; then
		scenarios_agree=0
	fi
done
if [ "${scenarios_agree}" -eq 1 ]; then
	note "all five name: ${scenarios_deploy}"
else
	bad "the sandbox scenario lists disagree — add or remove a scenario in all five places together:"
	for source in "${scenario_sources[@]}"; do
		note "${source%%:*} = ${source#*:}"
	done
fi

if [ "${fail}" -ne 0 ]; then
	echo
	echo "Repository invariants violated — see CLAUDE.md."
	exit 1
fi
echo
echo "All repository invariants hold."
