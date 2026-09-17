#!/usr/bin/env bash
# up.sh — starts the pinned stellar/quickstart local network, waits for
# its RPC to be healthy and closing ledgers, confirms it is in fact the
# standalone network (never a public one), and points the stellar CLI's
# `local` network definition at it.
#
# Refuses to run if a container named SANDBOX_CONTAINER already exists —
# stopped or running — rather than reusing or replacing it: `down.sh`
# first is the only way to a clean network, and guessing at "already
# fine" here is exactly how a stale ledger state gets mistaken for a
# fresh one.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"
# shellcheck source=scripts/sandbox/versions.env
source "${VERSIONS_ENV:-${script_dir}/versions.env}"

: "${SANDBOX_CONTAINER:=blend-sandbox}"
: "${SANDBOX_PORT:=8000}"

[ -n "${QUICKSTART_DIGEST}" ] || die "QUICKSTART_DIGEST is empty in versions.env — resolve the pin first (see that file's comment)"

if [ -n "$(docker ps -a --filter "name=^/${SANDBOX_CONTAINER}\$" --format '{{.Names}}')" ]; then
	die "a container named '${SANDBOX_CONTAINER}' already exists — run scripts/sandbox/down.sh first"
fi

image="${QUICKSTART_IMAGE}@${QUICKSTART_DIGEST}"
rpc_url="http://localhost:${SANDBOX_PORT}/rpc"

log "starting ${SANDBOX_CONTAINER} from ${image} (pull may take a few minutes on a cold cache)"
docker run -d \
	--name "${SANDBOX_CONTAINER}" \
	-p "${SANDBOX_PORT}:8000" \
	"${image}" \
	--local --enable core,rpc,horizon >/dev/null \
	|| die "docker run failed for ${image}"

if ! wait_for_rpc "${rpc_url}"; then
	log "last 50 lines of ${SANDBOX_CONTAINER}'s log:"
	docker logs --tail 50 "${SANDBOX_CONTAINER}" >&2 || true
	die "${rpc_url} never became healthy within the budget — container '${SANDBOX_CONTAINER}' is still running; run scripts/sandbox/down.sh to clean up"
fi

require_standalone_network "${rpc_url}"

# The stellar CLI ships a built-in `local` network already pointed at
# http://localhost:8000/rpc with this exact passphrase, so on the default
# port a fresh machine already matches and nothing needs writing. Pin it
# explicitly only when it doesn't — a non-default SANDBOX_PORT, or a
# leftover definition pointed elsewhere — so the add stays idempotent
# rather than rewriting an already-correct file on every run.
existing_rpc_url=$(
	stellar network ls -l 2>/dev/null | awk '
		/^Name: local$/ { want = 1; next }
		want && /^RPC url:/ { sub(/^RPC url: /, ""); print; exit }
		/^Name:/ { want = 0 }
	'
) || existing_rpc_url=""

if [ "${existing_rpc_url}" = "${rpc_url}" ]; then
	log "stellar CLI's 'local' network already points at ${rpc_url}, leaving it"
else
	stellar network add local --rpc-url "${rpc_url}" --network-passphrase "${SANDBOX_PASSPHRASE}" \
		|| die "stellar network add local failed"
	log "pinned the stellar CLI's 'local' network to ${rpc_url}"
fi

log "sandbox is up"
printf '%s\n' "${rpc_url}"
