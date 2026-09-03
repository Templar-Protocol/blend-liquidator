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
#    CI gates on both of these (.github/workflows/ci.yml's `shellcheck` and
#    `deny` jobs), so without them locally the first sign of a violation is a
#    red PR. Neither is needed to build the crate; both are cheap.
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
echo "==> Installing CI-parity tooling (shellcheck, cargo-deny)"

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

# 6. Warm the dependency cache, so the first build/test/clippy run does not
#    also pay for the download.
#
#    NOTE for when the Stellar/Soroban stack lands: `cargo` defaults to one
#    rustc job per core, and `nproc` reports the HOST's core count while this
#    container has far less memory — a large dependency tree then dies with
#    `signal: 9` from the OOM killer. The current tree is small enough not to
#    care. If you add the `stellar` CLI or a heavy source build here, cap the
#    job count against the cgroup memory limit at the same time.
echo "==> Warming the cargo dependency cache"
cargo fetch || warn "cargo fetch failed; it will run again on your first build."
