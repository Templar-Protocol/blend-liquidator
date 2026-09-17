#!/usr/bin/env bash
# post-create.sh — dev container one-time setup (postCreateCommand).
#
# Everything after the toolchain step is non-fatal: a crates.io hiccup should
# leave you inside a usable container to retry from, not fail the create and
# drop you back to the host with nothing.

set -euo pipefail

warn() { echo "!!  $*" >&2; }

# 1. Mark the workspace safe for git.
#
#    The workspace is a bind mount from the host, and the uid it reports
#    through that mount is not stable: `stat` and `git` can disagree seconds
#    apart, and `git pull` fails with "detected dubious ownership" while
#    `git status` succeeds, because pull re-discovers the repository in
#    subprocesses that get their own attribute read. The files really are the
#    user's on the host, so this is a uid-mapping artefact rather than a
#    permissions problem worth respecting.
#
#    VS Code usually adds this itself, but ~/.gitconfig is regenerated on every
#    rebuild and is not on a volume, so it cannot be relied on. Idempotent:
#    plain `--add` would append a duplicate line on each re-run.
echo "==> Marking the workspace safe for git"
workspace="$(cd "$(dirname "$0")/.." && pwd)"
if ! git config --global --get-all safe.directory 2>/dev/null | grep -qxF "${workspace}"; then
	git config --global --add safe.directory "${workspace}"
fi

# 2. Commit signing. Extracted to its own script because it also runs from
#    postStartCommand — SSH_AUTH_SOCK is not reliably present during creation.
#    See .devcontainer/git-signing.sh for the whole story.
echo "==> Configuring git commit signing"
"$(dirname "$0")/git-signing.sh"

# 3. Toolchain — `rustup show` reads rust-toolchain.toml and installs the
#    pinned toolchain plus rustfmt/clippy. Fatal if it fails; there is no
#    usable container without it.
echo "==> Installing the pinned Rust toolchain"
rustup show

# 4. Claude Code — the CLI itself is disposable and reinstalled on each
#    rebuild, but its state is not: ~/.claude is a named volume (see
#    devcontainer.json) holding session transcripts, config and credentials.
#
#    Docker creates a fresh volume root owned by root, so it has to be chowned
#    before the CLI can write to it. ~/.claude.json sits *outside* that
#    directory, so it is moved in and symlinked back — otherwise it is the one
#    piece of state that would still be lost on rebuild. ~/.config/gh is a
#    named volume for the same reason: gh's OAuth token lives there, the
#    container cannot inherit the host's login (it is in the macOS keychain),
#    and without persistence every rebuild needs another `gh auth login`.
echo "==> Setting up Claude Code and gh credential persistence"
sudo chown -R "$(id -u):$(id -g)" "${HOME}/.claude" || warn "could not chown ${HOME}/.claude"
if [ -d "${HOME}/.config/gh" ]; then
	sudo chown -R "$(id -u):$(id -g)" "${HOME}/.config/gh" ||
		warn "could not chown ${HOME}/.config/gh; gh auth may not persist across rebuilds"
fi

#    Each step below warns rather than aborting. The chown above is the one
#    most likely to fail (a volume the daemon hands over root-owned), and it
#    only warns — so leaving these unguarded under `set -e` would turn that
#    warning into a failed create two lines later, which is exactly the
#    outcome the non-fatal contract at the top of this file promises not to.
if [ -f "${HOME}/.claude.json" ] && [ ! -L "${HOME}/.claude.json" ]; then
	mv "${HOME}/.claude.json" "${HOME}/.claude/.claude.json" ||
		warn "could not migrate ${HOME}/.claude.json into the volume; it will not persist across rebuilds"
fi
if [ ! -e "${HOME}/.claude/.claude.json" ]; then
	echo '{}' > "${HOME}/.claude/.claude.json" ||
		warn "could not seed ${HOME}/.claude/.claude.json"
fi
ln -sfn "${HOME}/.claude/.claude.json" "${HOME}/.claude.json" ||
	warn "could not link ${HOME}/.claude.json into the volume; config will not persist across rebuilds"

#    Installed WITHOUT sudo, deliberately. The node feature puts node/npm under
#    /usr/local/share/nvm, which is writable by this user but is not on root's
#    sudo secure_path — `sudo npm install -g` fails with "npm: command not
#    found".
if command -v claude >/dev/null 2>&1; then
	echo "==> Claude Code already installed ($(claude --version 2>/dev/null || echo unknown))"
else
	npm install -g --no-fund --loglevel=error @anthropic-ai/claude-code || warn \
		"Claude Code install failed. Nothing in the build depends on it. Retry with: npm install -g @anthropic-ai/claude-code"
fi

# 5. CI-parity tooling.
#
#    CI gates on all three of these (.github/workflows/ci.yml's `shellcheck`
#    and `deny` jobs, and its `test` job's sqlx steps), so without them
#    locally the first sign of a violation is a red PR. None is needed to
#    build the crate.
#
#    cargo-deny is installed from its static musl release binary rather than
#    `cargo install`, which would compile a large dependency tree — and a musl
#    build carries no libc dependency, so the GLIBC_2.39 trap described in
#    devcontainer.json does not apply to it. Pinned and checksum-verified,
#    downloaded to a temp dir and moved into place only after the checksum
#    passes, so a failed or truncated download never leaves a partial binary on
#    PATH. To bump: change the version and both sha256 (the .sha256 sidecars on
#    the GitHub release).
CARGO_DENY_VERSION="0.20.2"
echo "==> Installing CI-parity tooling (shellcheck, cargo-deny, sqlx-cli)"

if ! command -v shellcheck >/dev/null 2>&1; then
	sudo apt-get update -qq && sudo apt-get install -y -qq --no-install-recommends shellcheck ||
		warn "shellcheck install failed; CI's shellcheck job cannot be reproduced locally."
fi

if [ "$(cargo deny --version 2>/dev/null | awk '{ print $2 }')" = "${CARGO_DENY_VERSION}" ]; then
	echo "    cargo-deny ${CARGO_DENY_VERSION} already installed"
else
	case "$(uname -m)" in
	aarch64 | arm64) deny_arch="aarch64"; deny_sha="995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d" ;;
	x86_64 | amd64) deny_arch="x86_64"; deny_sha="9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f" ;;
	*) deny_arch="" ;;
	esac

	if [ -z "${deny_arch}" ]; then
		warn "cargo-deny: no prebuilt binary for $(uname -m); install it manually if you need to reproduce CI's deny job."
	elif ! deny_tmp="$(mktemp -d 2>/dev/null)"; then
		warn "cargo-deny: mktemp failed; skipping."
	else
		deny_tar="cargo-deny-${CARGO_DENY_VERSION}-${deny_arch}-unknown-linux-musl.tar.gz"
		if curl -fsSL -o "${deny_tmp}/${deny_tar}" \
			"https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}/${deny_tar}" &&
			echo "${deny_sha}  ${deny_tmp}/${deny_tar}" | sha256sum -c - >/dev/null 2>&1 &&
			tar -xzf "${deny_tmp}/${deny_tar}" -C "${deny_tmp}" &&
			sudo install -m 0755 "${deny_tmp}"/*/cargo-deny /usr/local/bin/cargo-deny; then
			echo "    cargo-deny ${CARGO_DENY_VERSION} installed"
		else
			warn "cargo-deny install failed (download/checksum/extract); CI's deny job cannot be reproduced locally."
		fi
		rm -rf "${deny_tmp}"
	fi
fi

#    sqlx-cli is the one tool here that has to be compiled — it publishes no
#    release binary — so it is installed exactly as CI installs it, with the
#    version read from scripts/sandbox/versions.env rather than spelled
#    twice: the crate's `sqlx::query!` macros are checked against the
#    committed offline metadata in .sqlx/, and a container regenerating that
#    metadata on a different sqlx-cli produces a file CI rejects.
#    `make sqlx-prepare` and `make sandbox-down`'s database sweep both need
#    it locally. Non-fatal like the rest of this section.
#
#    CARGO_BUILD_JOBS is passed explicitly because this compile happens
#    before step 8 writes the cap into ~/.cargo/config.toml, and it is
#    exactly the kind of cold dependency tree that OOMs against nproc's host
#    core count (see scripts/cargo-jobs.sh). An operator's own
#    CARGO_BUILD_JOBS still wins.
sqlx_version="$(grep -oE '^SQLX_CLI_VERSION=.+' "${workspace}/scripts/sandbox/versions.env" | cut -d= -f2- || true)"
if [ -z "${sqlx_version}" ]; then
	warn "scripts/sandbox/versions.env does not pin SQLX_CLI_VERSION; skipping sqlx-cli."
elif [ "$(sqlx --version 2>/dev/null | awk '{ print $2 }')" = "${sqlx_version}" ]; then
	echo "    sqlx-cli ${sqlx_version} already installed"
else
	echo "    compiling sqlx-cli ${sqlx_version} (a few minutes)"
	CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$("${workspace}/scripts/cargo-jobs.sh" 2>/dev/null || echo 1)}" \
		cargo install sqlx-cli --version "${sqlx_version}" \
		--no-default-features --features postgres,rustls --locked ||
		warn "sqlx-cli install failed; make sqlx-prepare and make sandbox-down's sweep cannot run locally."
fi

# 6. Warm the dependency cache, so the first build/test/clippy run does not
#    also pay for the download.
#
#    The OOM gotcha this used to warn about (cargo defaulting to one rustc
#    job per core against `nproc`'s HOST count, in a container with far
#    less memory) is handled by step 8 below, which caps `[build] jobs`
#    against the cgroup memory limit — this step itself is a download, not
#    a build, so it doesn't need the cap.
echo "==> Warming the cargo dependency cache"
cargo fetch || warn "cargo fetch failed; it will run again on your first build."

# 7. The `stellar` CLI, pinned and checksum-verified from
#    scripts/sandbox/versions.env (the pins Task 1 of the sandbox tier
#    fetches wasm from). Needed only for scripts/sandbox/*.sh; nothing in
#    the crate build depends on it, so — like cargo-deny above — a failure
#    here only warns.
#
#    Downloaded to a temp dir and extracted there too, so a failed or
#    truncated download, or a mismatched checksum, never leaves a partial
#    binary on PATH: the move to ~/.local/bin/stellar (already on PATH in
#    this image) is the last step, after sha256_check has passed.
#    Idempotent: skipped outright once `stellar version --only-version`
#    prints exactly the pinned version. Exact equality, not a substring
#    match: a prerelease such as 28.0.0-rc.1 contains the pinned 28.0.0 and
#    would otherwise be kept. An installed CLI too old to know
#    `--only-version` prints nothing to stdout, the comparison fails, and
#    the install proceeds — which is the wanted answer for it too.
#    lib.sh's sha256_check/fetch are fatal by design (they call die(),
#    which exits) — exactly right inside a script that must not go on
#    using an unverified or half-downloaded file. Run in a `(…)` subshell
#    so that exit only ends the subshell: the parent tests its status and
#    warns, keeping this step non-fatal like every other one below the
#    toolchain.
#
#    The binary itself links against libdbus at runtime (its OS-keychain
#    identity backend, unused by this bot but still dynamically linked),
#    and the base image does not ship it: without this, even
#    `stellar --version` fails with "cannot open shared object file".
echo "==> Installing the stellar CLI"
if ! ldconfig -p | grep -qF 'libdbus-1.so.3'; then
	sudo apt-get update -qq && sudo apt-get install -y -qq --no-install-recommends libdbus-1-3 ||
		warn "libdbus-1-3 install failed; the stellar CLI will not run without it."
fi
if (
	set -euo pipefail
	sandbox_scripts_dir="$(cd "$(dirname "$0")/../scripts/sandbox" && pwd)"
	# shellcheck source=scripts/sandbox/lib.sh
	source "${sandbox_scripts_dir}/lib.sh"
	# shellcheck source=scripts/sandbox/versions.env
	source "${sandbox_scripts_dir}/versions.env"

	if command -v stellar >/dev/null 2>&1 &&
		[ "$(stellar version --only-version 2>/dev/null)" = "${STELLAR_CLI_VERSION}" ]; then
		echo "    stellar CLI ${STELLAR_CLI_VERSION} already installed"
		exit 0
	fi

	case "$(uname -m)" in
	x86_64 | amd64)
		url="${STELLAR_CLI_URL_X86_64}"
		sha="${STELLAR_CLI_SHA256_X86_64}"
		;;
	aarch64 | arm64)
		url="${STELLAR_CLI_URL_AARCH64}"
		sha="${STELLAR_CLI_SHA256_AARCH64}"
		;;
	*)
		warn "stellar CLI: no prebuilt binary for $(uname -m); install it manually."
		exit 1
		;;
	esac

	tmp="$(mktemp -d)"
	trap 'rm -rf "${tmp}"' EXIT

	fetch "${url}" "${tmp}/stellar-cli.tar.gz" "${sha}"
	tar -xzf "${tmp}/stellar-cli.tar.gz" -C "${tmp}" stellar
	mkdir -p "${HOME}/.local/bin"
	install -m 0755 "${tmp}/stellar" "${HOME}/.local/bin/stellar"
	echo "    stellar CLI ${STELLAR_CLI_VERSION} installed to ${HOME}/.local/bin/stellar"
); then
	:
else
	warn "stellar CLI install failed; scripts/sandbox/*.sh will not run without it."
fi

# 8. Cap cargo's build parallelism to this container's cgroup memory limit
#    (scripts/cargo-jobs.sh — see its header for the formula and the OOM
#    gotcha it exists for). Written once: an environment CARGO_BUILD_JOBS
#    still overrides this at build time, and a config already carrying a
#    `[build]` `jobs` key (an operator's own choice) is left untouched
#    rather than getting a second, conflicting one appended.
#
#    The file-editing itself lives in scripts/cargo-jobs-config.sh, not
#    inline here: cargo refuses to parse a *second* `[build]` header
#    ("Cannot declare ('build',) twice"), so a config that already has a
#    `[build]` table with some other key but no `jobs` must get `jobs`
#    inserted into that same table, never a fresh one appended — a case
#    worth its own tests (scripts/sandbox/test-cargo-config.sh) on temp
#    files, not just a hand-verification in this one container.
echo "==> Capping cargo build parallelism"
cargo_jobs_n="$("$(dirname "$0")/../scripts/cargo-jobs.sh")" || cargo_jobs_n=""
if [ -z "${cargo_jobs_n}" ]; then
	warn "scripts/cargo-jobs.sh failed; leaving ~/.cargo/config.toml untouched."
else
	cargo_config="${HOME}/.cargo/config.toml"
	mkdir -p "${HOME}/.cargo"
	status="$("$(dirname "$0")/../scripts/cargo-jobs-config.sh" "${cargo_config}" "${cargo_jobs_n}")" || status=""
	case "${status}" in
	created) echo "    ~/.cargo/config.toml: [build] jobs = ${cargo_jobs_n} (new [build] table)" ;;
	inserted) echo "    ~/.cargo/config.toml: [build] jobs = ${cargo_jobs_n} (added to the existing [build] table)" ;;
	unchanged) echo "    ~/.cargo/config.toml already sets [build] jobs; leaving it as configured" ;;
	*) warn "scripts/cargo-jobs-config.sh failed; ~/.cargo/config.toml was not updated." ;;
	esac
fi
