#!/usr/bin/env bash
# fetch-artifacts.sh — downloads the five pinned Blend v2 wasm files into
# target/sandbox/wasm/, verifying each against scripts/sandbox/versions.env,
# and prints their sizes.
#
# Idempotent and offline-safe once every file is present and verified: a
# second run finds each already_verified() and downloads nothing. A file
# that is missing, or present but corrupt (its SHA-256 no longer matches),
# is (re)fetched and re-verified.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"
# shellcheck source=scripts/sandbox/versions.env
source "${VERSIONS_ENV:-${script_dir}/versions.env}"

wasm_dir="$(sandbox_dir)/wasm"
mkdir -p "${wasm_dir}"

# already_verified FILE EXPECTED — true if FILE exists and its SHA-256
# already equals EXPECTED. Never fatal, unlike lib.sh's sha256_check: this
# is the idempotency test run *before* deciding whether to fetch at all,
# so "no" must be an ordinary false, not a die().
already_verified() {
	local file=$1 expected=$2 actual
	[ -f "${file}" ] || return 1
	actual=$(sha256sum "${file}" | cut -d' ' -f1)
	[ "${actual}" = "${expected}" ]
}

# process URL EXPECTED_SHA — fetches URL into wasm_dir under its own
# basename unless already_verified, then prints its size.
process() {
	local url=$1 expected=$2 dest
	dest="${wasm_dir}/$(basename "${url}")"
	if already_verified "${dest}" "${expected}"; then
		log "$(basename "${dest}") already present and verified, skipping"
	else
		fetch "${url}" "${dest}" "${expected}"
	fi
	printf '%s: %d bytes\n' "$(basename "${dest}")" "$(wc -c <"${dest}")"
}

process "${POOL_WASM_URL}" "${POOL_WASM_SHA256}"
process "${BACKSTOP_WASM_URL}" "${BACKSTOP_WASM_SHA256}"
process "${POOL_FACTORY_WASM_URL}" "${POOL_FACTORY_WASM_SHA256}"
process "${COMET_WASM_URL}" "${COMET_WASM_SHA256}"
process "${MOCK_ORACLE_WASM_URL}" "${MOCK_ORACLE_WASM_SHA256}"
