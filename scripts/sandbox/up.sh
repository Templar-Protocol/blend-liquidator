#!/usr/bin/env bash
# up.sh — starts the pinned stellar/quickstart local network, waits for
# its RPC to be healthy and closing ledgers, and confirms it is in fact
# the standalone network (never a public one).
#
# It writes nothing to the stellar CLI's configuration, and deliberately:
# nothing in this tier names a CLI network, because an explicit
# `--network` loses to STELLAR_RPC_URL/STELLAR_NETWORK_PASSPHRASE in the
# environment. Every call passes lib.sh's own --rpc-url and
# --network-passphrase instead, which is also what makes the URL this
# script verified and the URL those calls use the same string.
#
# Refuses to run if a container named SANDBOX_CONTAINER already exists —
# stopped or running — rather than reusing or replacing it: `down.sh`
# first is the only way to a clean network, and guessing at "already
# fine" here is exactly how a stale ledger state gets mistaken for a
# fresh one.
#
# A container this invocation created and then could not bring up is
# removed before the script dies, which is the other half of that refusal:
# `make sandbox` exits the moment `sandbox-up` fails and so never reaches
# `down.sh`, so without this the next run would refuse on a container
# nothing ever used. Only a container created *here* is ever removed — a
# pre-existing one is still the refusal above.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"

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
	-p "127.0.0.1:${SANDBOX_PORT}:8000" \
	"${image}" \
	--local --enable core,rpc,horizon >/dev/null \
	|| die "docker run failed for ${image}"

# Armed only once `docker run` has succeeded, and disarmed only once the
# network is up and verified, so the window it covers is exactly "this
# script created the container and did not finish" — `wait_for_rpc`
# returning non-zero, `require_standalone_network`'s own `die`, and any
# other failure `set -e` ends the script on. `docker rm -f` swallows its
# own failure: the script is already dying with a message that names the
# real reason, and a removal that could not run is not a better one.
remove_created_container() {
	log "removing '${SANDBOX_CONTAINER}', which this run created and could not bring up"
	docker rm -f "${SANDBOX_CONTAINER}" >/dev/null 2>&1 || true
}
trap remove_created_container EXIT

if ! wait_for_rpc "${rpc_url}"; then
	log "last 50 lines of ${SANDBOX_CONTAINER}'s log:"
	docker logs --tail 50 "${SANDBOX_CONTAINER}" >&2 || true
	die "${rpc_url} never became healthy within the budget — removing the container this run created; see the log lines above, then run scripts/sandbox/up.sh again"
fi

require_standalone_network "${rpc_url}"

trap - EXIT

log "sandbox is up"
printf '%s\n' "${rpc_url}"
