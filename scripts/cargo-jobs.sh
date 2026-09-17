#!/usr/bin/env bash
# cargo-jobs.sh — prints the cargo build job cap for this container:
#
#   min(nproc, max(1, mem_bytes / 2 GiB))
#
# `cargo` defaults to one rustc job per core, and `nproc` reports the HOST's
# core count even inside a container holding far less memory than that
# implies — a large dependency tree then dies with `signal: 9` from the OOM
# killer (see CLAUDE.md's gotcha on CARGO_BUILD_JOBS). This prints the safe
# cap; post-create.sh writes it once into `~/.cargo/config.toml`'s
# `[build] jobs`, which an environment `CARGO_BUILD_JOBS` still overrides at
# build time.
#
# CARGO_JOBS_NPROC and CARGO_JOBS_MEM_BYTES override the two inputs — for
# scripts/sandbox/test-cargo-jobs.sh, so the formula is testable without
# faking /proc or /sys. Pure shell arithmetic: `$(( ))` on byte counts fits
# in 64 bits, no bc.
set -euo pipefail

readonly TWO_GIB=$((2 * 1024 * 1024 * 1024))
# Above this, a cgroup v1 limit is a "no limit" sentinel (near i64::MAX)
# rather than a real cap — see mem_bytes() below.
readonly TWO_POW_62=4611686018427387904

# mem_bytes — the container's memory limit in bytes, read in this order:
# cgroup v2's memory.max (the literal "max" means unlimited, so it is
# skipped rather than parsed as a number), then cgroup v1's
# memory.limit_in_bytes (skipped the same way once it is at or above
# 2^62, which is how an unlimited v1 cgroup reports itself), then
# /proc/meminfo's MemTotal, converted from kB to bytes.
mem_bytes() {
	local v
	if [ -r /sys/fs/cgroup/memory.max ] && v=$(cat /sys/fs/cgroup/memory.max) &&
		[[ "${v}" =~ ^[0-9]+$ ]]; then
		printf '%s\n' "${v}"
		return
	fi
	if [ -r /sys/fs/cgroup/memory/memory.limit_in_bytes ] &&
		v=$(cat /sys/fs/cgroup/memory/memory.limit_in_bytes) &&
		[[ "${v}" =~ ^[0-9]+$ ]] && [ "${v}" -lt "${TWO_POW_62}" ]; then
		printf '%s\n' "${v}"
		return
	fi
	awk '/^MemTotal:/ { printf "%.0f\n", $2 * 1024 }' /proc/meminfo
}

nproc_val="${CARGO_JOBS_NPROC:-$(nproc)}"
mem="${CARGO_JOBS_MEM_BYTES:-$(mem_bytes)}"

by_mem=$((mem / TWO_GIB))
if [ "${by_mem}" -lt 1 ]; then
	by_mem=1
fi

jobs=${by_mem}
if [ "${jobs}" -gt "${nproc_val}" ]; then
	jobs=${nproc_val}
fi

printf '%d\n' "${jobs}"
