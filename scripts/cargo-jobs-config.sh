#!/usr/bin/env bash
# cargo-jobs-config.sh FILE N — ensures FILE's [build] table sets
# `jobs = N`, in exactly one of three ways, and never declares [build]
# twice — which cargo refuses to parse ("Cannot declare ('build',)
# twice"), so getting this wrong is worse than the OOM the cap prevents:
#
#   - no [build] table in FILE at all (including a missing or empty
#     FILE): append a fresh [build] table, with jobs = N and a comment
#     naming this script. Prints "created".
#   - a [build] table exists but sets no `jobs` key anywhere inside it:
#     insert the same comment and `jobs = N` as the first lines under
#     that existing header, leaving every other line in the file —
#     including any other key already in the table — exactly where it
#     was. Prints "inserted".
#   - a [build] table already sets `jobs` (any value, anywhere in the
#     table): FILE is left byte-for-byte untouched — an operator's own
#     choice, or an earlier run of this script, wins. Prints "unchanged".
#
# Idempotent by the third rule: a second run, with the same or a
# different N, changes nothing once a `jobs` key exists in [build].
#
# Used by .devcontainer/post-create.sh; exercised directly by
# scripts/sandbox/test-cargo-config.sh so the file-editing logic is
# testable on temp files without running post-create.
set -euo pipefail

file=$1
jobs_n=$2

comment_lines=(
	"# Written by scripts/cargo-jobs.sh via .devcontainer/post-create.sh: caps"
	"# rustc's parallelism to this container's cgroup memory limit, so a cold"
	"# build does not OOM against nproc's host core count. CARGO_BUILD_JOBS in"
	"# the environment still overrides this at build time."
)

touch "${file}"

# has_build/has_jobs — whether a [build] table exists at all, and whether
# it (specifically it, not some other table) already sets `jobs`. One awk
# pass; has_jobs can only be set while in_build, so a `jobs` key in some
# other table never counts.
read -r has_build has_jobs < <(awk '
	/^\[build\]/ { in_build = 1; has_build = 1; next }
	/^\[/ { in_build = 0 }
	in_build && /^[[:space:]]*jobs[[:space:]]*=/ { has_jobs = 1 }
	END { printf "%d %d\n", has_build, has_jobs }
' "${file}")

if [ "${has_jobs}" = "1" ]; then
	echo "unchanged"
	exit 0
fi

if [ "${has_build}" = "1" ]; then
	# Insert right after the first (only, if the file is well-formed)
	# [build] header — never a second header of our own.
	build_lineno=$(awk '/^\[build\]/ { print NR; exit }' "${file}")
	tmp="$(mktemp)"
	trap 'rm -f "${tmp}"' EXIT
	{
		head -n "${build_lineno}" "${file}"
		printf '%s\n' "${comment_lines[@]}"
		printf 'jobs = %s\n' "${jobs_n}"
		tail -n "+$((build_lineno + 1))" "${file}"
	} >"${tmp}"
	# Written back into the existing file (not `mv`ed over it), so FILE's
	# own inode — and its permissions — survive the edit.
	cat "${tmp}" >"${file}"
	echo "inserted"
else
	{
		echo ""
		printf '%s\n' "${comment_lines[@]}"
		echo "[build]"
		printf 'jobs = %s\n' "${jobs_n}"
	} >>"${file}"
	echo "created"
fi
