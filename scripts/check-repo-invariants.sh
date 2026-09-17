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
	elif ! grep -vE '^[[:space:]]*#' "${file}" | grep -qF 'versions.env'; then
		bad "${file} does not read scripts/sandbox/versions.env — the stellar CLI version (${cli_version}) lives in that one file, and ${file} must source or grep it rather than pin its own"
	elif grep -qE 'stellar-cli-[0-9]' "${file}"; then
		bad "${file} names a stellar CLI release literally (stellar-cli-…) — take the URL and its checksum from scripts/sandbox/versions.env instead, which is the only place the version belongs"
	else
		note "${file} reads scripts/sandbox/versions.env"
	fi
done

if [ "${fail}" -ne 0 ]; then
	echo
	echo "Repository invariants violated — see CLAUDE.md."
	exit 1
fi
echo
echo "All repository invariants hold."
