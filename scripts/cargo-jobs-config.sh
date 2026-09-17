#!/usr/bin/env bash
# cargo-jobs-config.sh FILE N — ensures FILE's `build` table sets
# `jobs = N`, in exactly one of three ways, and never declares `build`
# twice — which cargo refuses to parse ("Cannot declare ('build',)
# twice"), so getting this wrong is worse than the OOM the cap prevents:
#
#   - no `build` table in FILE at all (including a missing or empty
#     FILE): append a fresh [build] table, with jobs = N and a comment
#     naming this script. Prints "created".
#   - a `build` table exists but sets no `jobs` key anywhere inside it:
#     insert the same comment and the key against the line that declared
#     the table — where exactly depends on the spelling, see below —
#     leaving every other line in the file, including any other key
#     already in the table, exactly where it was. Prints "inserted".
#   - a `build` table already sets `jobs` (any value, anywhere in the
#     table): FILE is left byte-for-byte untouched — an operator's own
#     choice, or an earlier run of this script, wins. Prints "unchanged".
#
# "A `build` table" is whichever of TOML's two spellings FILE uses, and
# both are recognised because appending a second declaration of either is
# the parse error above:
#
#   - a `[build]` header, which TOML allows to be indented and to carry a
#     trailing comment (`  [build]  # …`), so the match is not anchored at
#     column 0; the key is inserted under the header as plain `jobs = N`.
#   - root-level dotted keys — `build.incremental = true` — where `build`
#     is declared by the dotted key itself and a `[build]` header
#     afterwards would be the second declaration. TOML ignores whitespace
#     around the dot, so `build . incremental = true` is that same
#     declaration and is matched too. The key is inserted immediately
#     *before* the first such line, in the same dotted form, as
#     `build.jobs = N`. Before, not after: a dotted key's value may span
#     several lines (`build.rustflags = [` … `]`), only the opening line
#     of which matches anything here, so inserting after the last match
#     can land between a `[` and its elements and split a value in half —
#     a file no parser accepts, the same unusable config as a doubled
#     declaration. A position before a line is never inside a value.
#     Only at the root: `build.jobs` under some other table header is that
#     table's key, not this one.
#
# A FILE whose last line is unterminated gets a newline first, before
# anything is inserted after it or appended to it — otherwise the comment
# would be glued onto that line (`[build]# Written by …`), which is the
# same broken parse by a different route. Only when FILE is about to be
# edited: the "unchanged" case really is byte-for-byte.
#
# Idempotent by the third rule: a second run, with the same or a
# different N, changes nothing once a `jobs` key exists in `build`.
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

# One awk pass answering four things: whether a `build` table exists at
# all, whether it (specifically it, not some other table) already sets
# `jobs`, which spelling declared it, and the line the insert goes after.
#
# style 1 is a [build] header and 2 the root-level dotted form; a file
# holding both already declares `build` twice, so the header wins and the
# insert goes inside it rather than adding a third declaration.
read -r has_build has_jobs style lineno < <(awk '
	BEGIN { at_root = 1 }
	# A [build] header: indentable, and a trailing comment is still the
	# same header. Only the first one is an insertion point.
	/^[ \t]*\[[ \t]*build[ \t]*\][ \t]*(#.*)?$/ {
		in_build = 1
		at_root = 0
		if (!build_lineno) { build_lineno = NR }
		next
	}
	# Any other table header ends both the build table and the root.
	/^[ \t]*\[/ { in_build = 0; at_root = 0; next }
	in_build && /^[ \t]*jobs[ \t]*=/ { has_jobs = 1; next }
	# The dotted spelling, at the root only: under another header these
	# belong to that table instead. Whitespace is allowed either side of
	# the dot, because TOML allows it and `build . incremental = true`
	# declares exactly the same table.
	at_root && /^[ \t]*build[ \t]*\.[ \t]*[A-Za-z0-9_-]+[ \t]*=/ {
		if (!dotted_lineno) { dotted_lineno = NR }
		if ($0 ~ /^[ \t]*build[ \t]*\.[ \t]*jobs[ \t]*=/) { has_jobs = 1 }
	}
	# lineno is "insert after this line", so the dotted style reports the
	# line *before* its first match — 0 when that match is line 1, which
	# the splice below reads as "insert at the top".
	END {
		if (build_lineno) { style = 1; lineno = build_lineno }
		else if (dotted_lineno) { style = 2; lineno = dotted_lineno - 1 }
		else { style = 0; lineno = 0 }
		printf "%d %d %d %d\n", (style ? 1 : 0), has_jobs, style, lineno
	}
' "${file}")

if [ "${has_jobs}" = "1" ]; then
	echo "unchanged"
	exit 0
fi

# From here FILE is being edited, so terminate a dangling last line before
# anything is written after it. A command substitution strips trailing
# newlines, so a non-empty result is exactly "the last byte is not a
# newline".
if [ -s "${file}" ] && [ -n "$(tail -c 1 "${file}")" ]; then
	printf '\n' >>"${file}"
fi

if [ "${has_build}" = "1" ]; then
	# Insert into the table already declared — never a second declaration
	# of our own — in that declaration's own spelling: directly under a
	# `[build]` header, and directly above the first root-level dotted
	# `build.` line, which is the one position that cannot fall inside a
	# multi-line value.
	if [ "${style}" = "2" ]; then
		key="build.jobs"
	else
		key="jobs"
	fi
	tmp="$(mktemp)"
	trap 'rm -f "${tmp}"' EXIT
	{
		head -n "${lineno}" "${file}"
		printf '%s\n' "${comment_lines[@]}"
		printf '%s = %s\n' "${key}" "${jobs_n}"
		tail -n "+$((lineno + 1))" "${file}"
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
