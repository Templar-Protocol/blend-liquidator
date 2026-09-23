#!/usr/bin/env bash
# lib.sh — shared helpers for scripts/testnet/*.sh, the testnet sibling of
# scripts/sandbox/*.sh. It reuses that tier's helpers rather than
# re-implementing them: see scripts/sandbox/lib.sh's own comments for the
# fuller reasoning behind invoke()/invoke_view()/env_write() and the
# network-pinning discipline this file only adds a second network to.
#
# Meant to be **sourced**, never executed, under the same `set -euo
# pipefail` contract sandbox/lib.sh documents there: die() is the only
# function that ends a script on purpose, and every other function here
# either returns a status or is fatal by design, never by accident.
#
# shellcheck shell=bash

TESTNET_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# scripts/sandbox/lib.sh, sourced from this directory and nowhere else — no
# environment variable selects the path, for the same reason its own header
# gives for versions.env: this file's whole job is a network gate, so what
# it is built from cannot be redirected by an operator's shell. Sourcing it
# also unsets the stellar CLI's own network/signing variables before this
# file defines anything (see its comment on that), which is what makes
# calling a script under scripts/testnet/ from a shell that has
# STELLAR_RPC_URL exported safe rather than silently wrong.
#
# What it gives us: log()/die(), require_network_passphrase() (the gate
# both tiers share, which `require_testnet_network` below is one line on
# top of), sandbox_set_network()/sandbox_require_network()/the
# sandbox_network_args array — one flag array and one gate shared by both
# tiers, whichever last called require_*_network — sandbox_register_role(),
# invoke()/invoke_view() and env_write(). versions.env's pins (the wasm
# URLs and hashes) come along too, since deploy.sh reuses the sandbox's
# already-fetched, already-verified wasm rather than fetching a second
# testnet-only copy.
# shellcheck source=scripts/sandbox/lib.sh
source "${TESTNET_SCRIPT_DIR}/../sandbox/lib.sh"

# TESTNET_PASSPHRASE and TESTNET_HORIZON_URL are literals, never
# overridable: TESTNET_PASSPHRASE is the value require_testnet_network below
# compares every getNetwork answer against, and moving it would defeat the
# gate exactly as redirecting versions.env would defeat the sandbox's.
# TESTNET_FRIENDBOT_URL is the same kind of fact, not a gate input — see
# require_funded_testnet below.
TESTNET_PASSPHRASE="Test SDF Network ; September 2015"
TESTNET_HORIZON_URL="https://horizon-testnet.stellar.org"
TESTNET_FRIENDBOT_URL="https://friendbot.stellar.org"

# TESTNET_RPC_URL is the one pin in this file an operator's environment may
# set ahead of time — every other value above is a literal precisely so it
# cannot be. That is not a hole in the gate below: require_testnet_network
# still calls whatever URL this names and dies unless *that node's own*
# getNetwork answers TESTNET_PASSPHRASE, so an override pointing anywhere
# else — the local sandbox, mainnet, nothing at all — simply dies there,
# naming the network it actually found. deploy.sh and run-bot.sh's dry-run
# mode gate and use this value; crash.sh and run-bot.sh --armed source
# testnet.env first, whose TESTNET_RPC_URL — the one deploy.sh verified and
# recorded — replaces it, and gate that instead. Either way the URL a
# script goes on to use is the one its gate verified. Nothing here weakens
# what require_testnet_network itself decides.
: "${TESTNET_RPC_URL:=https://soroban-testnet.stellar.org}"

# require_testnet_network — dies unless TESTNET_RPC_URL's own getNetwork
# answers TESTNET_PASSPHRASE (refusing the public passphrase by name first,
# whatever TESTNET_PASSPHRASE is — require_network_passphrase always does
# that before its own comparison), and on success makes TESTNET_RPC_URL the
# network every later `stellar` call in this tier is pinned to
# (sandbox_network_args). Every script under scripts/testnet/ calls this
# before its first `stellar` call or chain read — see each script's own
# header for exactly where.
require_testnet_network() {
	require_network_passphrase "${TESTNET_RPC_URL}" "${TESTNET_PASSPHRASE}" testnet
}

# testnet_dir — prints <repo>/target/testnet, this tier's own scratch
# directory (git-ignored via the repo's root /target, the same rule
# sandbox_dir's own comment gives). Does not create it — a caller that
# needs it `mkdir -p`s it explicitly.
testnet_dir() {
	printf '%s/target/testnet\n' "$(cd "${TESTNET_SCRIPT_DIR}/../.." && pwd)"
}

# The stellar CLI identity names deploy.sh creates and crash.sh signs
# with — see sandbox/lib.sh's own SANDBOX_KEY_* comment for why these live
# beside the network helpers rather than in deploy.sh alone: crash.sh has
# to sign the oracle's set_price_stable as the pool's admin, and only
# deploy.sh knows the name it generated it under. The `testnet-soak-` prefix
# keeps them apart from the sandbox's own `sandbox-` identities in the same
# ~/.config/stellar/identity/ directory. These are throwaway keys too, but
# for a network that is not this repo's to tear down — see deploy.sh's own
# header for why there is no down.sh here.
TESTNET_KEY_ISSUER=testnet-soak-issuer
TESTNET_KEY_ADMIN=testnet-soak-admin
TESTNET_KEY_BORROWER=testnet-soak-borrower
TESTNET_KEY_FILLER=testnet-soak-filler

# require_funded_testnet NAME ADDRESS — dies unless testnet Horizon reports
# ADDRESS as an existing account, polling and re-requesting friendbot while
# it waits. scripts/sandbox/deploy.sh's own require_funded is the template
# and the reasoning is identical: `stellar keys generate --fund` is
# best-effort, so a dropped or rate-limited friendbot request leaves a
# perfectly valid key with no funded account behind it, and the first
# symptom is an unrelated step failing two calls later under the wrong
# step's name. Horizon is the proof because friendbot funds through it; a
# request that races one already in flight is answered harmlessly (or
# refused, since testnet's friendbot 400s an already-funded address) and
# either way its result is ignored here.
#
# 120s, not the sandbox's 90: this friendbot is a shared public service, not
# a container on localhost a moment behind its own health check, and is the
# one budget in this tier that is *not* simply the sandbox's scaled for a
# ~5s ledger — funding has nothing to do with ledger close time, only with
# how slow a public service can be to answer.
require_funded_testnet() {
	local name=$1 address=$2 deadline body now last_request=0
	deadline=$(($(date +%s) + 120))
	while :; do
		now=$(date +%s)
		body=$(curl -fsS --max-time 5 "${TESTNET_HORIZON_URL}/accounts/${address}" 2>/dev/null) || body=""
		if [ "$(printf '%s' "${body}" | jq -r '.id // empty' 2>/dev/null)" = "${address}" ]; then
			log "${name} is funded"
			return 0
		fi
		[ "${now}" -lt "${deadline}" ] || break
		if [ "${now}" -ge "$((last_request + 5))" ]; then
			curl -fsS --max-time 10 "${TESTNET_FRIENDBOT_URL}/?addr=${address}" >/dev/null 2>&1 || true
			last_request=${now}
		fi
		sleep 2
	done
	die "require_funded_testnet: friendbot did not fund ${name} (${address}) — ${TESTNET_HORIZON_URL}/accounts/${address} holds no such account after 120s"
}
