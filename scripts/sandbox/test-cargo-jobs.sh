#!/usr/bin/env bash
# test-cargo-jobs.sh — shell tests for scripts/cargo-jobs.sh's job-cap
# formula: min(nproc, max(1, mem_bytes / 2 GiB)). Run by hand and by the
# sandbox CI workflow. Exercises the formula only through
# CARGO_JOBS_NPROC/CARGO_JOBS_MEM_BYTES, never the real /proc or /sys
# paths, so it is deterministic wherever it runs.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cargo_jobs="${script_dir}/../cargo-jobs.sh"

pass=0
fail=0

# check DESCRIPTION NPROC MEM_BYTES EXPECTED — runs cargo-jobs.sh with the
# two inputs pinned and compares its one line of output to EXPECTED.
check() {
	local desc=$1 nproc_val=$2 mem_bytes=$3 expected=$4 actual
	actual=$(CARGO_JOBS_NPROC="${nproc_val}" CARGO_JOBS_MEM_BYTES="${mem_bytes}" "${cargo_jobs}")
	if [ "${actual}" = "${expected}" ]; then
		printf 'ok - %s\n' "${desc}"
		pass=$((pass + 1))
	else
		printf 'FAIL - %s: expected %s, got %s\n' "${desc}" "${expected}" "${actual}"
		fail=$((fail + 1))
	fi
}

# The three cases the task brief fixes: memory-bound, memory-bound down to
# the floor of 1, and core-bound.
check "16 cores, 8 GiB memory -> 4" 16 "$((8 * 1024 ** 3))" 4
check "16 cores, 1 GiB memory -> 1 (floor)" 16 "$((1024 ** 3))" 1
check "2 cores, 64 GiB memory -> 2 (core-bound)" 2 "$((64 * 1024 ** 3))" 2

printf '%d passed, %d failed\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
