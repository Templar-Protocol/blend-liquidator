#!/usr/bin/env bash
# test-cargo-config.sh — shell tests for scripts/cargo-jobs-config.sh's
# file-editing logic, run on temp files so nothing here ever touches a
# real ~/.cargo/config.toml. Run by hand and by
# .github/workflows/sandbox.yml, before it starts a network.
#
# Exercises exactly the case that was missed the first time around: a
# [build] table that exists but has no `jobs` key must get `jobs`
# inserted into *that* table, never a second [build] header — cargo
# refuses to parse a config with [build] declared twice.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cargo_jobs_config="${script_dir}/../cargo-jobs-config.sh"

pass=0
fail=0

ok() {
	printf 'ok - %s\n' "$1"
	pass=$((pass + 1))
}

bad() {
	printf 'FAIL - %s\n' "$1"
	fail=$((fail + 1))
}

# assert_toml_valid DESC FILE — parses FILE as TOML with python3's
# tomllib (3.11+, present in this container) if available; otherwise
# falls back to the weaker but dependency-free check that FILE names the
# [build] table exactly once, which is the specific defect this test
# suite exists to catch.
assert_toml_valid() {
	local desc=$1 file=$2
	if command -v python3 >/dev/null 2>&1 &&
		python3 -c 'import tomllib' >/dev/null 2>&1; then
		if python3 -c 'import tomllib,sys; tomllib.load(open(sys.argv[1], "rb"))' "${file}" 2>/dev/null; then
			ok "${desc}: parses as TOML"
		else
			bad "${desc}: does not parse as TOML"
		fi
	else
		local build_headers
		build_headers=$(grep -c '^\[build\]' "${file}")
		if [ "${build_headers}" = "1" ]; then
			ok "${desc}: exactly one [build] header (python3/tomllib unavailable, used the regex fallback)"
		else
			bad "${desc}: expected exactly one [build] header, found ${build_headers} (python3/tomllib unavailable, used the regex fallback)"
		fi
	fi
}

tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT

# Case (a): no file at all — the table is created.
file_a="${tmp_dir}/a.toml"
status_a="$("${cargo_jobs_config}" "${file_a}" 7)"
if [ "${status_a}" = "created" ]; then
	ok "no file: reports created"
else
	bad "no file: expected status 'created', got '${status_a}'"
fi
if grep -qx 'jobs = 7' "${file_a}"; then
	ok "no file: jobs = 7 present"
else
	bad "no file: jobs = 7 missing"
fi
assert_toml_valid "no file" "${file_a}"

# Case (b): a [build] table with an unrelated key and no jobs — jobs is
# inserted into the existing table (never a second [build] header), and
# the pre-existing key survives.
file_b="${tmp_dir}/b.toml"
printf '[build]\nincremental = true\n' >"${file_b}"
status_b="$("${cargo_jobs_config}" "${file_b}" 7)"
if [ "${status_b}" = "inserted" ]; then
	ok "existing [build], no jobs: reports inserted"
else
	bad "existing [build], no jobs: expected status 'inserted', got '${status_b}'"
fi
build_headers_b=$(grep -c '^\[build\]' "${file_b}")
if [ "${build_headers_b}" = "1" ]; then
	ok "existing [build], no jobs: exactly one [build] header"
else
	bad "existing [build], no jobs: expected exactly one [build] header, found ${build_headers_b}"
fi
if grep -qx 'jobs = 7' "${file_b}" && grep -qx 'incremental = true' "${file_b}"; then
	ok "existing [build], no jobs: jobs = 7 inserted and incremental = true kept"
else
	bad "existing [build], no jobs: jobs = 7 and/or incremental = true missing"
fi
assert_toml_valid "existing [build], no jobs" "${file_b}"

# Case (c): a [build] table that already sets jobs — the file is left
# byte-for-byte untouched, even with a different N.
file_c="${tmp_dir}/c.toml"
printf '[build]\njobs = 3\n' >"${file_c}"
before_c="$(cat "${file_c}")"
status_c="$("${cargo_jobs_config}" "${file_c}" 7)"
after_c="$(cat "${file_c}")"
if [ "${status_c}" = "unchanged" ]; then
	ok "existing jobs: reports unchanged"
else
	bad "existing jobs: expected status 'unchanged', got '${status_c}'"
fi
if [ "${before_c}" = "${after_c}" ]; then
	ok "existing jobs: file byte-for-byte unchanged"
else
	bad "existing jobs: file was modified"
fi
assert_toml_valid "existing jobs" "${file_c}"

# Idempotency: rerunning case (a)'s and case (b)'s now-jobs-bearing files
# reports unchanged and does not grow a second [build] header.
status_a2="$("${cargo_jobs_config}" "${file_a}" 9)"
build_headers_a2=$(grep -c '^\[build\]' "${file_a}")
if [ "${status_a2}" = "unchanged" ] && [ "${build_headers_a2}" = "1" ]; then
	ok "rerun after create: unchanged, still exactly one [build] header"
else
	bad "rerun after create: expected unchanged/1 header, got status '${status_a2}', ${build_headers_a2} header(s)"
fi

status_b2="$("${cargo_jobs_config}" "${file_b}" 9)"
build_headers_b2=$(grep -c '^\[build\]' "${file_b}")
if [ "${status_b2}" = "unchanged" ] && [ "${build_headers_b2}" = "1" ]; then
	ok "rerun after insert: unchanged, still exactly one [build] header"
else
	bad "rerun after insert: expected unchanged/1 header, got status '${status_b2}', ${build_headers_b2} header(s)"
fi

printf '%d passed, %d failed\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
