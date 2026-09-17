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
