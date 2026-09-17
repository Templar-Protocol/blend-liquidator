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

# sandbox_lib_dir is lib.sh's own directory, resolved once at source time
# from BASH_SOURCE — a sourced file's $0 is the *caller's* path, not its
# own, so BASH_SOURCE is the only way sandbox_dir() below is correct
# however a script here is invoked (by relative path, by absolute path, or
# via PATH).
sandbox_lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# log MESSAGE… — writes a timestamped line to stderr. Always stderr, never
# stdout: a caller that captures a helper's stdout (e.g. `x=$(fetch …)`)
# must never pick up log noise mixed into its result.
log() {
	printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*" >&2
}

# die MESSAGE… — logs MESSAGE as an error and exits 1. The one function in
# this file whose whole job is to end the calling script; every other
# function here returns a status and leaves the decision to its caller.
die() {
	printf '[%s] ERROR: %s\n' "$(date -u +%H:%M:%S)" "$*" >&2
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

# require_standalone_network URL — dies unless URL's getNetwork answers
# exactly SANDBOX_PASSPHRASE (from versions.env — every caller sources it
# before this). This is the one gate every later sandbox script calls
# first: the sandbox exists to never touch a public network, so refusing
# on any other passphrase — including no answer at all — has to happen
# before that script does anything else, however its RPC URL got
# configured.
require_standalone_network() {
	local url=$1 body passphrase
	body=$(_sandbox_rpc_call "${url}" getNetwork)
	passphrase=$(printf '%s' "${body}" | jq -r '.result.passphrase // empty' 2>/dev/null) || passphrase=""
	[ -n "${passphrase}" ] || die "require_standalone_network: ${url} did not answer getNetwork"
	[ "${passphrase}" = "${SANDBOX_PASSPHRASE}" ] || die "require_standalone_network: ${url} reports passphrase '${passphrase}', expected the sandbox's standalone passphrase '${SANDBOX_PASSPHRASE}' — refusing to touch a network that is not this sandbox's own"
}
