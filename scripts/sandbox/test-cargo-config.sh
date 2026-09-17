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
#
# And every spelling of that table an operator's own config may already
# use, because each one appends a second declaration if it is not
# recognised: an indented header, a header with a trailing comment, the
# root-level dotted form (`build.incremental = true`, and `build .
# incremental = true`, where a [build] header afterwards is the second
# declaration), a dotted key whose value spans several lines, which an
# insert must never be placed inside, and a file whose last line has no
# terminating newline, which glues whatever is written next onto it.
#
# CARGO_CONFIG_TEST_NO_TOMLLIB=1 runs every case through
# assert_toml_valid's dependency-free fallback instead of python3's
# tomllib, which is how that branch is exercised on a host that has
# tomllib. Both modes must end with a tally and "0 failed".
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

# count_lines PATTERN FILE — how many of FILE's lines match the extended
# regular expression PATTERN, printed, and 0 rather than a failure when
# none do.
#
# `grep -c` exits 1 on "no matches", and a bare `x=$(grep -c …)` takes that
# status — which under this file's `set -euo pipefail` ends the whole suite
# there and then, with no FAIL line and no tally, so a correct
# cargo-jobs-config.sh would read as an unexplained regression. Nothing in
# this suite may abort: every judgement goes through ok/bad.
count_lines() {
	local count
	count=$(grep -cE "$1" "$2" 2>/dev/null || true)
	# Empty only if grep failed outright (an unreadable file), which is a
	# count of nothing either way.
	printf '%s\n' "${count:-0}"
}

# assert_toml_valid DESC FILE — parses FILE as TOML with python3's tomllib
# (3.11+, present in this container) when it is available; otherwise falls
# back to the weaker but dependency-free check below.
#
# CARGO_CONFIG_TEST_NO_TOMLLIB=1 forces that fallback. It is what exercises
# the fallback on a host that does have tomllib — sandbox.yml's runner
# always does — so the branch is not left to run only where nobody is
# looking.
#
# The fallback counts `build` declarations in both of TOML's spellings,
# with the same whitespace tolerance cargo-jobs-config.sh matches them by,
# and asserts the two things it can: at most one `[build]` header, since
# two is the "Cannot declare ('build',) twice" parse error this suite
# exists to catch, and at least one declaration overall, since the key has
# to have landed somewhere. Zero headers is correct by design for the
# dotted cases. What it cannot see is a value split across an insert —
# only a real parser can — which is why tomllib is the preferred path
# rather than a nicety.
assert_toml_valid() {
	local desc=$1 file=$2
	if [ -z "${CARGO_CONFIG_TEST_NO_TOMLLIB:-}" ] &&
		command -v python3 >/dev/null 2>&1 &&
		python3 -c 'import tomllib' >/dev/null 2>&1; then
		if python3 -c 'import tomllib,sys; tomllib.load(open(sys.argv[1], "rb"))' "${file}" 2>/dev/null; then
			ok "${desc}: parses as TOML"
		else
			bad "${desc}: does not parse as TOML"
		fi
		return
	fi
	local headers dotted
	headers=$(count_lines '^[[:space:]]*\[[[:space:]]*build[[:space:]]*\][[:space:]]*(#.*)?$' "${file}")
	dotted=$(count_lines '^[[:space:]]*build[[:space:]]*\.[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=' "${file}")
	if [ "${headers}" -gt 1 ]; then
		bad "${desc}: ${headers} [build] headers, which declares build twice (regex fallback)"
	elif [ "$((headers + dotted))" -lt 1 ]; then
		bad "${desc}: neither a [build] header nor a root-level build.* key (regex fallback)"
	else
		ok "${desc}: ${headers} [build] header(s), ${dotted} dotted build.* line(s) (regex fallback)"
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

# Case (d): an *indented* [build] header with a trailing comment. TOML
# allows whitespace before a table header, so this is the same table by
# every parser's reading — and a detection anchored at column 0 would miss
# it and append a second [build], which cargo refuses to parse.
file_d="${tmp_dir}/d.toml"
printf '  [build]  # the operator put it here\nincremental = true\n' >"${file_d}"
status_d="$("${cargo_jobs_config}" "${file_d}" 7)"
build_headers_d=$(grep -c '\[build\]' "${file_d}")
if [ "${status_d}" = "inserted" ] && [ "${build_headers_d}" = "1" ]; then
	ok "indented [build] with a trailing comment: inserted into it, still exactly one [build] header"
else
	bad "indented [build] with a trailing comment: expected inserted/1 header, got status '${status_d}', ${build_headers_d} header(s)"
fi
if grep -qx 'jobs = 7' "${file_d}"; then
	ok "indented [build]: jobs = 7 present"
else
	bad "indented [build]: jobs = 7 missing"
fi
assert_toml_valid "indented [build]" "${file_d}"

# Case (e): the root-level dotted spelling of the same key. `build.jobs`
# at the root *is* [build]'s jobs, so the operator's choice must win here
# exactly as it does in case (c).
file_e="${tmp_dir}/e.toml"
printf 'build.jobs = 3\n' >"${file_e}"
before_e="$(cat "${file_e}")"
status_e="$("${cargo_jobs_config}" "${file_e}" 7)"
after_e="$(cat "${file_e}")"
if [ "${status_e}" = "unchanged" ] && [ "${before_e}" = "${after_e}" ]; then
	ok "root-level build.jobs: reports unchanged and the file is byte-for-byte untouched"
else
	bad "root-level build.jobs: expected unchanged and no edit, got status '${status_e}' (modified: $([ "${before_e}" = "${after_e}" ] && echo no || echo yes))"
fi
assert_toml_valid "root-level build.jobs" "${file_e}"

# Case (f): the dotted table with some other key and no jobs. A [build]
# header appended here would declare `build` twice — TOML rejects it — so
# the key has to be added in the dotted form the file already uses.
file_f="${tmp_dir}/f.toml"
printf 'build.incremental = true\n' >"${file_f}"
status_f="$("${cargo_jobs_config}" "${file_f}" 7)"
if [ "${status_f}" = "inserted" ]; then
	ok "root-level build.incremental, no jobs: reports inserted"
else
	bad "root-level build.incremental, no jobs: expected status 'inserted', got '${status_f}'"
fi
if grep -qx 'build.jobs = 7' "${file_f}"; then
	ok "root-level build.incremental: build.jobs = 7 added in dotted form"
else
	bad "root-level build.incremental: build.jobs = 7 missing"
fi
if grep -q '\[build\]' "${file_f}"; then
	bad "root-level build.incremental: a [build] header was appended, which declares build twice"
else
	ok "root-level build.incremental: no [build] header appended"
fi
assert_toml_valid "root-level build.incremental" "${file_f}"

status_f2="$("${cargo_jobs_config}" "${file_f}" 9)"
if [ "${status_f2}" = "unchanged" ]; then
	ok "rerun after a dotted insert: unchanged"
else
	bad "rerun after a dotted insert: expected unchanged, got '${status_f2}'"
fi

# Case (g): a file whose final byte is not a newline. `head -n` emits the
# bytes verbatim, so an unterminated last line would have the comment
# glued straight onto it — `[build]# Written by …` — which is the same
# broken parse by the other path.
file_g="${tmp_dir}/g.toml"
printf '[build]' >"${file_g}"
status_g="$("${cargo_jobs_config}" "${file_g}" 7)"
if [ "${status_g}" = "inserted" ]; then
	ok "unterminated last line: reports inserted"
else
	bad "unterminated last line: expected status 'inserted', got '${status_g}'"
fi
if grep -qx '\[build\]' "${file_g}"; then
	ok "unterminated last line: [build] is still on a line of its own"
else
	bad "unterminated last line: [build] was glued to the inserted text — $(head -n 1 "${file_g}")"
fi
if grep -qx 'jobs = 7' "${file_g}"; then
	ok "unterminated last line: jobs = 7 present"
else
	bad "unterminated last line: jobs = 7 missing"
fi
assert_toml_valid "unterminated last line" "${file_g}"

# Case (h): the same unterminated final byte on the *append* path — no
# [build] table at all, so the fresh one must start on its own line.
file_h="${tmp_dir}/h.toml"
printf '[net]\nretry = 2' >"${file_h}"
status_h="$("${cargo_jobs_config}" "${file_h}" 7)"
if [ "${status_h}" = "created" ]; then
	ok "unterminated last line, no [build]: reports created"
else
	bad "unterminated last line, no [build]: expected status 'created', got '${status_h}'"
fi
if grep -qx 'retry = 2' "${file_h}"; then
	ok "unterminated last line, no [build]: the pre-existing key survived on its own line"
else
	bad "unterminated last line, no [build]: retry = 2 was run together with what follows"
fi
assert_toml_valid "unterminated last line, no [build]" "${file_h}"

# Case (i): the dotted spelling with whitespace around the dot. TOML
# ignores whitespace either side of a dot-separated key's parts, so
# `build . incremental` declares exactly the same table as
# `build.incremental` — and a detection that misses it appends a [build]
# header, which is the second declaration cargo refuses to parse.
file_i="${tmp_dir}/i.toml"
printf 'build . incremental = true\n' >"${file_i}"
status_i="$("${cargo_jobs_config}" "${file_i}" 7)"
if [ "${status_i}" = "inserted" ]; then
	ok "spaced dotted key, no jobs: reports inserted"
else
	bad "spaced dotted key, no jobs: expected status 'inserted', got '${status_i}'"
fi
if grep -qx 'build.jobs = 7' "${file_i}"; then
	ok "spaced dotted key: build.jobs = 7 added in dotted form"
else
	bad "spaced dotted key: build.jobs = 7 missing"
fi
if grep -q '\[build\]' "${file_i}"; then
	bad "spaced dotted key: a [build] header was appended, which declares build twice"
else
	ok "spaced dotted key: no [build] header appended"
fi
assert_toml_valid "spaced dotted key" "${file_i}"

# Case (j): the same whitespace on the key this script would otherwise
# set. `build . jobs` is `build.jobs`, so the operator's value wins here
# exactly as it does in cases (c) and (e).
file_j="${tmp_dir}/j.toml"
printf 'build . jobs = 3\n' >"${file_j}"
before_j="$(cat "${file_j}")"
status_j="$("${cargo_jobs_config}" "${file_j}" 7)"
after_j="$(cat "${file_j}")"
if [ "${status_j}" = "unchanged" ] && [ "${before_j}" = "${after_j}" ]; then
	ok "spaced build . jobs: reports unchanged and the file is byte-for-byte untouched"
else
	bad "spaced build . jobs: expected unchanged and no edit, got status '${status_j}' (modified: $([ "${before_j}" = "${after_j}" ] && echo no || echo yes))"
fi
assert_toml_valid "spaced build . jobs" "${file_j}"

# Case (k): a dotted key whose value spans several lines. Only the opening
# line matches a `build.<key> =` pattern, so an insert placed *after* the
# last matching line lands between the `[` and the array's elements and
# splits the value in half — a file no TOML parser accepts, which is the
# same unusable ~/.cargo/config.toml as a doubled declaration by another
# route. Inserting *before* the first dotted line cannot land inside any
# value, whatever shape it has.
file_k="${tmp_dir}/k.toml"
printf 'build.rustflags = [\n  "-C", "target-cpu=native",\n]\n' >"${file_k}"
status_k="$("${cargo_jobs_config}" "${file_k}" 7)"
if [ "${status_k}" = "inserted" ]; then
	ok "multi-line dotted value: reports inserted"
else
	bad "multi-line dotted value: expected status 'inserted', got '${status_k}'"
fi
if grep -qx 'build.jobs = 7' "${file_k}"; then
	ok "multi-line dotted value: build.jobs = 7 added in dotted form"
else
	bad "multi-line dotted value: build.jobs = 7 missing"
fi
# The array, verbatim: the line after the opening bracket must still be
# its first element, not anything this script wrote.
after_open_k="$(awk '/^build\.rustflags = \[$/ { getline; print; exit }' "${file_k}")"
if [ "${after_open_k}" = '  "-C", "target-cpu=native",' ]; then
	ok "multi-line dotted value: the array's own lines are untouched"
else
	bad "multi-line dotted value: the insert split the array — the line after '[' is '${after_open_k}'"
fi
jobs_line_k="$(grep -n -x 'build.jobs = 7' "${file_k}" | cut -d: -f1)"
rustflags_line_k="$(grep -n -x 'build.rustflags = \[' "${file_k}" | cut -d: -f1)"
if [ -n "${jobs_line_k}" ] && [ -n "${rustflags_line_k}" ] &&
	[ "${jobs_line_k}" -lt "${rustflags_line_k}" ]; then
	ok "multi-line dotted value: build.jobs was inserted above the first dotted line"
else
	bad "multi-line dotted value: expected build.jobs above build.rustflags, got lines '${jobs_line_k}' and '${rustflags_line_k}'"
fi
assert_toml_valid "multi-line dotted value" "${file_k}"

status_k2="$("${cargo_jobs_config}" "${file_k}" 9)"
if [ "${status_k2}" = "unchanged" ]; then
	ok "rerun after a multi-line dotted insert: unchanged"
else
	bad "rerun after a multi-line dotted insert: expected unchanged, got '${status_k2}'"
fi

printf '%d passed, %d failed\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
