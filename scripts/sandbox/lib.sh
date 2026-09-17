#!/usr/bin/env bash
# lib.sh — shared helpers for scripts/sandbox/*.sh.
#
# Meant to be **sourced**, never executed: every script under
# scripts/sandbox/ runs with `set -euo pipefail`, and every function below
# is written to behave under that — the only function that ends a script
# on purpose is die(); everything else either returns a status for the
# caller to test or is itself fatal by design (sha256_check, fetch), never
# by accident (a stray failing command left unguarded inside a function
# that a caller expects to just return).
#
# shellcheck shell=bash

# The stellar CLI's own network and signing variables, unset at source time
# — before a single function is defined, so no script here can make a CLI
# call ahead of this.
#
# They are not ours to inherit. The CLI resolves an ad-hoc network from
# STELLAR_RPC_URL and STELLAR_NETWORK_PASSPHRASE *ahead of* an explicit
# `--network`, so an operator with the pair exported (they are the CLI's
# documented variables, advertised on every network-taking subcommand)
# would have this tier generate keys, fund them, deploy and invoke on
# whatever those name, while require_standalone_network went on confirming
# localhost. STELLAR_SIGN_WITH_KEY and its two siblings are worse still:
# nothing here passes a --sign-with-* flag, so the environment form is
# unopposed and the sandbox would sign with a key that is not its own.
#
# The flags every call passes (sandbox_network_args, below) already beat
# all of them; this is the second half of the answer, so that a variable
# nobody thought to override cannot decide anything either.
unset STELLAR_RPC_URL STELLAR_NETWORK_PASSPHRASE STELLAR_NETWORK \
	STELLAR_ACCOUNT STELLAR_SIGN_WITH_KEY \
	STELLAR_SIGN_WITH_LAB STELLAR_SIGN_WITH_LEDGER

# sandbox_lib_dir is lib.sh's own directory, resolved once at source time
# from BASH_SOURCE — a sourced file's $0 is the *caller's* path, not its
# own, so BASH_SOURCE is the only way sandbox_dir() below is correct
# however a script here is invoked (by relative path, by absolute path, or
# via PATH).
sandbox_lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# _sandbox_emit MESSAGE… — the one place a sandbox script's prose is
# written: a timestamped line to stderr, and, when SANDBOX_LOG names a
# file, appended to it as well. Always stderr, never stdout: a caller
# that captures a helper's stdout (e.g. `x=$(fetch …)` or `x=$(invoke …)`)
# must never pick up log noise mixed into its result.
#
# Everything reaching SANDBOX_LOG passes through here, which is what makes
# "sandbox.log holds no secret" a property of two functions rather than of
# every call site: nothing else appends to that file, and neither log()
# nor die() is ever handed a secret — invoke() below logs a function name
# and a contract, never an argument.
_sandbox_emit() {
	local line
	line="$(printf '[%s] %s' "$(date -u +%H:%M:%S)" "$*")"
	printf '%s\n' "${line}" >&2
	if [ -n "${SANDBOX_LOG:-}" ]; then
		printf '%s\n' "${line}" >>"${SANDBOX_LOG}" || true
	fi
	return 0
}

# log MESSAGE… — writes MESSAGE through _sandbox_emit. Returns 0 always,
# so a `log …` as the last statement of a function can never fail the
# caller under `set -e`.
log() {
	_sandbox_emit "$*"
}

# die MESSAGE… — logs MESSAGE as an error and exits 1. The one function in
# this file whose whole job is to end the calling script; every other
# function here returns a status and leaves the decision to its caller.
# Every caller's message names the step that failed, because this line is
# all an operator gets: the sandbox scripts have no stack trace.
die() {
	_sandbox_emit "ERROR: $*"
	exit 1
}

# sandbox_dir — prints <repo>/target/sandbox, the one scratch directory
# every sandbox script reads and writes under (git-ignored via /target).
# Does not create it or any subdirectory — a caller that needs one
# `mkdir -p`s it explicitly, since only the caller knows which
# subdirectory (wasm/, and later the deploy artefacts) it is about to use.
sandbox_dir() {
	printf '%s/target/sandbox\n' "$(cd "${sandbox_lib_dir}/../.." && pwd)"
}

# sha256_check FILE EXPECTED — dies, printing both hashes, unless FILE
# exists and its SHA-256 equals EXPECTED. Always fatal on a mismatch or a
# missing file: call it only where a failure really is an error (right
# after a download, inside fetch() below) — never as a "does this already
# match" test, since that path must not die on "no". A caller that wants
# the non-fatal question computes and compares the hash itself.
sha256_check() {
	local file=$1 expected=$2 actual
	[ -f "${file}" ] || die "sha256_check: ${file} does not exist"
	actual=$(sha256sum "${file}" | cut -d' ' -f1)
	if [ "${actual}" != "${expected}" ]; then
		die "SHA-256 mismatch for ${file}: expected ${expected}, got ${actual}"
	fi
}

# fetch URL DEST EXPECTED_SHA — downloads URL to DEST with curl (-fsSL,
# --retry 3) and verifies DEST against EXPECTED_SHA via sha256_check,
# which dies on any mismatch. DEST's parent directory must already exist.
# Unconditional: fetch() always downloads. The decision to skip an
# already-verified file is the caller's (fetch-artifacts.sh's
# already_verified()), because only the caller knows what "already have
# it" means for what it is fetching.
fetch() {
	local url=$1 dest=$2 expected=$3
	log "fetching ${url}"
	curl -fsSL --retry 3 -o "${dest}" "${url}" || die "download failed: ${url}"
	sha256_check "${dest}" "${expected}"
}

# _sandbox_rpc_call URL METHOD — POSTs a no-params JSON-RPC 2.0 request for
# METHOD to URL and prints the raw response body, or nothing if the
# request could not even be made (connection refused, timeout, non-2xx).
# Private to this file: both callers below are polling loops for which
# "not answering yet" must be an ordinary retry, never a script-ending
# failure, so this never dies and never propagates curl's exit status.
_sandbox_rpc_call() {
	local url=$1 method=$2
	curl -fsS --max-time 5 -H 'Content-Type: application/json' \
		-d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"${method}\"}" \
		"${url}" 2>/dev/null || true
}

# wait_for_rpc URL — blocks until URL's JSON-RPC getHealth reports
# status "healthy" and getLatestLedger's sequence has then advanced twice
# — proof the node is actually closing ledgers, not just answering once
# mid-catch-up. One 180 s budget covers both phases together. Returns 1
# on timeout rather than dying itself, logging what it was waiting for as
# it goes: the caller (up.sh) knows the container name and can attach its
# log tail to a more useful failure than this generic helper could write.
wait_for_rpc() {
	local url=$1 deadline status body seq base_seq=0 advances=0
	deadline=$(($(date +%s) + 180))

	log "waiting for ${url} to report healthy (180s budget)"
	status=""
	while [ "$(date +%s)" -lt "${deadline}" ]; do
		body=$(_sandbox_rpc_call "${url}" getHealth)
		status=$(printf '%s' "${body}" | jq -r '.result.status // empty' 2>/dev/null) || status=""
		[ "${status}" = "healthy" ] && break
		sleep 2
	done
	if [ "${status}" != "healthy" ]; then
		log "timed out waiting for ${url} to report healthy"
		return 1
	fi

	log "waiting for ${url}'s ledger to advance twice"
	while [ "${advances}" -lt 2 ]; do
		if [ "$(date +%s)" -ge "${deadline}" ]; then
			log "timed out waiting for ${url}'s ledger to advance"
			return 1
		fi
		body=$(_sandbox_rpc_call "${url}" getLatestLedger)
		seq=$(printf '%s' "${body}" | jq -r '.result.sequence // empty' 2>/dev/null) || seq=""
		if [ -n "${seq}" ]; then
			if [ "${base_seq}" -gt 0 ] && [ "${seq}" -gt "${base_seq}" ]; then
				advances=$((advances + 1))
			fi
			base_seq=${seq}
		fi
		[ "${advances}" -lt 2 ] && sleep 2
	done
	log "${url} is healthy and its ledger has advanced twice"
}

# sandbox_network_args holds the two flags every `stellar` call below
# passes: the RPC URL that has been proved standalone, and the passphrase
# it was proved by. Explicit flags rather than a named network for two
# reasons — they are the form that beats the environment (a `--network`
# does not, see the unset above and test-network-pinning.sh), and they
# make the URL that was checked and the URL that is used literally the
# same string rather than two reconstructions of it.
#
# Empty until sandbox_set_network has run, so a call made before the gate
# fails on a missing network rather than falling back to some default.
sandbox_network_args=()

# sandbox_set_network URL — records URL as this sandbox's RPC, exports it
# as SANDBOX_RPC_URL (the one name the rest of the tier reads it under)
# and builds the flag array from it.
#
# require_standalone_network calls this with the URL it has just verified,
# which is the only way a script should reach it; test-network-pinning.sh
# calls it directly, deriving a contract id offline against a URL it never
# contacts.
sandbox_set_network() {
	[ -n "${SANDBOX_PASSPHRASE:-}" ] \
		|| die "sandbox_set_network: SANDBOX_PASSPHRASE is unset — source versions.env before this"
	SANDBOX_RPC_URL=$1
	export SANDBOX_RPC_URL
	sandbox_network_args=(--rpc-url "${SANDBOX_RPC_URL}" --network-passphrase "${SANDBOX_PASSPHRASE}")
}

# require_standalone_network URL — dies unless URL's getNetwork answers
# exactly SANDBOX_PASSPHRASE (from versions.env — every caller sources it
# before this), and on success makes URL the network every later CLI call
# names, through sandbox_set_network. This is the one gate every later
# sandbox script calls first: the sandbox exists to never touch a public
# network, so refusing on any other passphrase — including no answer at
# all — has to happen before that script does anything else, however its
# RPC URL got configured.
#
# Verifying and pinning in the one function is the point: what the node
# answered for is then exactly what every `stellar` call is handed, so the
# gate cannot be passed about one network while the work happens on
# another.
require_standalone_network() {
	local url=$1 body passphrase
	body=$(_sandbox_rpc_call "${url}" getNetwork)
	passphrase=$(printf '%s' "${body}" | jq -r '.result.passphrase // empty' 2>/dev/null) || passphrase=""
	[ -n "${passphrase}" ] || die "require_standalone_network: ${url} did not answer getNetwork"
	[ "${passphrase}" = "${SANDBOX_PASSPHRASE}" ] || die "require_standalone_network: ${url} reports passphrase '${passphrase}', expected the sandbox's standalone passphrase '${SANDBOX_PASSPHRASE}' — refusing to touch a network that is not this sandbox's own"
	sandbox_set_network "${url}"
}

# The stellar CLI identity names deploy.sh creates and crash.sh signs
# with. They live here rather than in either script because crash.sh has
# to sign the oracle's set_price_stable as the pool's admin and only
# deploy.sh knows the name it generated it under — sandbox.env carries the
# admin's G-address, which cannot sign anything. The `sandbox-` prefix is
# what makes them recognisable among whatever else is in the operator's
# ~/.config/stellar/identity/; they are throwaway keys for a throwaway
# network, so down.sh leaves them alone.
SANDBOX_KEY_ISSUER=sandbox-issuer
SANDBOX_KEY_ADMIN=sandbox-admin
SANDBOX_KEY_BORROWER=sandbox-borrower
SANDBOX_KEY_FILLER=sandbox-filler

# SANDBOX_ROLES maps a deployed contract id to the role deploy.sh gave it
# ("pool", "oracle", "comet", …). invoke() reads it so a log line can name
# what it is talking to without ever printing an argument — the arguments
# are where an amount, an address or, in principle, a secret would be, and
# sandbox.log is a file an operator pastes into an issue.
declare -A SANDBOX_ROLES

# sandbox_register_role CONTRACT ROLE — records ROLE for CONTRACT and logs
# the pair. This is the line that puts every deployed address into
# sandbox.log, so call it for every contract the moment its id is known.
sandbox_register_role() {
	local contract=$1 role=$2
	SANDBOX_ROLES["${contract}"]="${role}"
	log "${role} is ${contract}"
}

# invoke KEY CONTRACT FN ARGS… — `stellar contract invoke … --send=yes`,
# logging the function name and CONTRACT's registered role but **never**
# the arguments, and dying (naming the function, the role and the key) on
# any failure. Prints the CLI's stdout — the contract's return value —
# unmixed, so `POOL=$(invoke admin "${FACTORY}" deploy …)` is the way to
# capture a returned address.
#
# --send=yes and not the default: the default only sends when simulation
# says the call writes, which turns a call this script means as a
# state change into a silent no-op the moment a contract's behaviour
# shifts. Every invoke() here is meant to land on chain; use
# invoke_view() for the ones that are not.
invoke() {
	local key=$1 contract=$2 fn=$3 role
	shift 3
	role="${SANDBOX_ROLES[${contract}]:-unregistered contract}"
	log "invoke ${fn} on ${role} as ${key}"
	stellar contract invoke "${sandbox_network_args[@]}" --source-account "${key}" \
		--id "${contract}" --send=yes -- "${fn}" "$@" \
		|| die "invoke ${fn} on ${role} (${contract}) as ${key} failed"
}

# invoke_view KEY CONTRACT FN ARGS… — invoke()'s read-only twin:
# `--send=no`, so the CLI simulates and prints the result without building,
# signing or sending anything. Same logging and same fatal-on-failure
# contract. KEY is still required — a simulation needs a source account —
# but nothing is signed with it.
invoke_view() {
	local key=$1 contract=$2 fn=$3 role
	shift 3
	role="${SANDBOX_ROLES[${contract}]:-unregistered contract}"
	log "view ${fn} on ${role} as ${key}"
	stellar contract invoke "${sandbox_network_args[@]}" --source-account "${key}" \
		--id "${contract}" --send=no -- "${fn}" "$@" \
		|| die "view ${fn} on ${role} (${contract}) as ${key} failed"
}

# env_write FILE — creates FILE empty, restricts it to mode 0600, and only
# then appends this function's stdin to it. The order is the point:
# sandbox.env carries the filler's secret key, and creating it under the
# operator's umask and chmod-ing afterwards would leave a world-readable
# file holding a signing key for however long the write took. Nothing is
# ever appended to an existing FILE — a truncate is what makes the mode
# guarantee hold on a second call.
env_write() {
	local file=$1
	: >"${file}" || die "env_write: could not create ${file}"
	chmod 600 "${file}" || die "env_write: could not restrict ${file} to mode 0600"
	cat >>"${file}" || die "env_write: could not write ${file}"
}
