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

if [ "${fail}" -ne 0 ]; then
	echo
	echo "Repository invariants violated — see CLAUDE.md."
	exit 1
fi
echo
echo "All repository invariants hold."
