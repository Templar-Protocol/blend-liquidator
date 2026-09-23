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

# SANDBOX_RPC_URL is ours rather than the CLI's, and unset for the same
# reason: sandbox_set_network exports it, so it is a record of a gate this
# process passed, never an input. Inherited from an operator's shell it
# would be a claim about a network nothing here verified — and until the
# gate runs there are no flags, so a CLI call made on the strength of that
# claim would carry none and resolve the CLI's own default, a public
# network. sandbox_require_network below tests the flags themselves for
# the same reason; this unset is the other half.
unset SANDBOX_RPC_URL

# SANDBOX_SCRIPT_DIR is lib.sh's own directory — which is scripts/sandbox,
# the directory every script in this tier and versions.env share —
# resolved once at source time from BASH_SOURCE. A sourced file's $0 is
# the *caller's* path, not its own, so BASH_SOURCE is the only way
# sandbox_dir() and the versions.env source below are correct however a
# script here is invoked (by relative path, by absolute path, or via
# PATH).
SANDBOX_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# The pins, sourced here and from nowhere else, at a path no environment
# variable takes part in.
#
# versions.env supplies SANDBOX_PASSPHRASE — the single literal
# require_standalone_network compares getNetwork's answer against — as
# well as every wasm URL together with the SHA-256 it is verified by. An
# override of which file that is would therefore hand over both halves at
# once: a standalone gate that logs success on a public network, after
# which this tier generates keys, funds them, deploys and signs there, and
# a supply-chain check that verifies each artefact against a hash from the
# same file that chose it. That is the same shell-inherited redirection
# the unset above closes, so it is closed the same way: the file is this
# directory's, and nothing reads VERSIONS_ENV.
# shellcheck source=scripts/sandbox/versions.env
source "${SANDBOX_SCRIPT_DIR}/versions.env"

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
	printf '%s/target/sandbox\n' "$(cd "${SANDBOX_SCRIPT_DIR}/../.." && pwd)"
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
# It is empty until sandbox_set_network has run, and an empty one is
# **not** self-enforcing: with no flags at all the CLI does not refuse,
# it resolves its own default, which is a public network — `stellar
# contract id wasm` with a cleared environment and no flags derives
# testnet's id. Nor does `set -u` help: bash expands an unset array to
# nothing without complaint. So the ordering is enforced by
# sandbox_require_network below, which every expansion of this array
# calls first.
sandbox_network_args=()

# sandbox_require_network — dies unless the gate has run, i.e. unless
# sandbox_set_network has built the flag array.
#
# It tests the array, not SANDBOX_RPC_URL: the array is what the call
# about to be made actually uses, and it is local to this process, whereas
# any variable is something an operator's shell can export. A guard keyed
# on the variable would pass on an inherited value while the array was
# still empty — the exact case it exists to refuse — so it is keyed on the
# thing it is guarding. Four elements because sandbox_set_network builds
# exactly --rpc-url URL --network-passphrase PASSPHRASE; a partially built
# array is not a state this file can reach, and >= 4 says so without
# asserting a length a later flag would break.
#
# Called immediately before **every** expansion of sandbox_network_args:
# invoke() and invoke_view() here, and each direct `stellar` call in
# deploy.sh. That is the rule a new call site has to follow, and it is
# load-bearing rather than tidy: a call made before
# require_standalone_network would carry no flags, and a flagless CLI
# call goes to a public network rather than failing. The one place the
# check itself is proved is test-network-pinning.sh.
sandbox_require_network() {
	[ "${#sandbox_network_args[@]}" -ge 4 ] \
		|| die "sandbox network not verified: call require_standalone_network first"
}

# sandbox_set_network URL [PASSPHRASE] — records URL as this sandbox's RPC,
# exports it as SANDBOX_RPC_URL (the one name the rest of the tier reads it
# under) and builds the flag array from URL and PASSPHRASE.
#
# PASSPHRASE defaults to SANDBOX_PASSPHRASE, never to a passphrase this
# process merely verified elsewhere: require_network_passphrase calls this
# with the URL *and the EXPECTED passphrase it just matched* — testnet's,
# when the caller is require_testnet_network — precisely so the flags this
# builds are pinned to the network that was actually checked, not
# whichever network this file happens to be the sandbox's own. Without the
# explicit second argument, a tier gating on a passphrase other than
# SANDBOX_PASSPHRASE would verify the right node and then hand the CLI the
# wrong one's passphrase anyway — every call would fail with "provided
# network passphrase does not match the server" despite the gate itself
# having passed. require_standalone_network's own call (through
# require_network_passphrase) supplies SANDBOX_PASSPHRASE as EXPECTED, so
# its behaviour is unchanged; test-network-pinning.sh's direct,
# single-argument call is unchanged too, for the same reason.
sandbox_set_network() {
	local url=$1 passphrase=${2:-${SANDBOX_PASSPHRASE:-}}
	[ -n "${passphrase}" ] \
		|| die "sandbox_set_network: no passphrase given and SANDBOX_PASSPHRASE is unset — ${SANDBOX_SCRIPT_DIR}/versions.env did not define it"
	SANDBOX_RPC_URL=$url
	export SANDBOX_RPC_URL
	sandbox_network_args=(--rpc-url "${SANDBOX_RPC_URL}" --network-passphrase "${passphrase}")
}

# _SANDBOX_PUBLIC_PASSPHRASE — mainnet's own, refused by name regardless of
# what a caller of require_network_passphrase expected, below. Not sourced
# from anywhere pinnable: it names the one network this whole tier must
# never touch, so it is a literal here rather than a value some file could
# be redirected away from — the same reasoning versions.env's own header
# gives for SANDBOX_PASSPHRASE.
_SANDBOX_PUBLIC_PASSPHRASE="Public Global Stellar Network ; September 2015"

# require_network_passphrase URL EXPECTED LABEL — dies unless URL's
# getNetwork answers exactly EXPECTED, and on success makes URL *and
# EXPECTED* the network every later CLI call names, through
# sandbox_set_network(url, expected) — the passphrase every later `stellar`
# call carries is the one this function just matched, never
# SANDBOX_PASSPHRASE by default, which is what makes this helper correct
# for a LABEL other than "sandbox" (require_testnet_network's "testnet"
# among them). LABEL is prose only — the network this call believes URL to
# be ("sandbox", "testnet") — used solely to make a failure legible; it
# decides nothing.
#
# Three ways to fail, in order:
#   1. no answer, or no passphrase in the answer — the node might not even
#      be the right kind of thing to ask;
#   2. the answer is mainnet's own passphrase, whatever EXPECTED is — the
#      one network no script here may ever act on, refused by name before
#      the ordinary comparison below so that a script pointed at mainnet
#      says so, rather than reporting a generic "expected testnet" that
#      buries the one failure that matters most. This also catches the
#      degenerate case of a caller whose own EXPECTED is mainnet's
#      passphrase: matching mainnet against mainnet must still die, not
#      quietly "pass", which is exactly why this check does not read
#      EXPECTED at all;
#   3. any other answer that is not EXPECTED.
#
# Verifying and pinning in the one function is the point: what the node
# answered for is then exactly what every `stellar` call is handed, so the
# gate cannot be passed about one network while the work happens on
# another.
require_network_passphrase() {
	local url=$1 expected=$2 label=$3 body passphrase
	body=$(_sandbox_rpc_call "${url}" getNetwork)
	passphrase=$(printf '%s' "${body}" | jq -r '.result.passphrase // empty' 2>/dev/null) || passphrase=""
	[ -n "${passphrase}" ] || die "require_network_passphrase: ${url} did not answer getNetwork (expected ${label}'s network)"
	[ "${passphrase}" != "${_SANDBOX_PUBLIC_PASSPHRASE}" ] \
		|| die "require_network_passphrase: ${url} reports the public network passphrase '${_SANDBOX_PUBLIC_PASSPHRASE}' — refusing to touch mainnet in place of ${label}'s own"
	[ "${passphrase}" = "${expected}" ] \
		|| die "require_network_passphrase: ${url} reports passphrase '${passphrase}', expected ${label}'s '${expected}' — refusing to touch a network that is not ${label}'s own"
	sandbox_set_network "${url}" "${expected}"
}

# require_standalone_network URL — dies unless URL's getNetwork answers
# exactly SANDBOX_PASSPHRASE — which this file sources from versions.env in
# its own directory, and which no caller supplies or can redirect — and on
# success makes URL the network every later CLI call names, through
# sandbox_set_network (see require_network_passphrase above, which this
# calls). This is the one gate every later sandbox script calls first: the
# sandbox exists to never touch a public network, so refusing on any other
# passphrase — including no answer at all — has to happen before that
# script does anything else, however its RPC URL got configured.
#
# SANDBOX_PASSPHRASE is "Standalone Network ; February 2017", so
# require_network_passphrase's own mismatch message already names the
# standalone network by quoting it as EXPECTED — the label "sandbox" below
# is prose only, and does not change that. Its no-answer message named no
# passphrase before this refactor either, and still does not: there is
# none to quote when the node gave none.
require_standalone_network() {
	require_network_passphrase "$1" "${SANDBOX_PASSPHRASE}" sandbox
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
	sandbox_require_network
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
	sandbox_require_network
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
