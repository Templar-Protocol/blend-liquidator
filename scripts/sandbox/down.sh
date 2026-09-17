#!/usr/bin/env bash
# down.sh — tears down the sandbox: force-removes the quickstart
# container and target/sandbox/sandbox.env (its keys are dead the moment
# the network under them is gone — keeping the file would be a live path
# to a signing key for a network that no longer exists). Keeps
# target/sandbox/wasm/: the fetched artefacts are keyed by content hash,
# not by network, so there is nothing to invalidate. Keeps
# target/sandbox/run-databases too, and must: it names the Postgres
# databases failed runs left behind, which outlive every network this
# script tears down, and deleting the list is how they become
# unreclaimable. The `sandbox-down` make target is what drops them.
#
# Safe to run whether or not the sandbox is up — a missing container or
# env file is not an error, since "already down" is exactly what this
# script is for.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"

: "${SANDBOX_CONTAINER:=blend-sandbox}"

if [ -n "$(docker ps -a --filter "name=^/${SANDBOX_CONTAINER}\$" --format '{{.Names}}')" ]; then
	log "removing container ${SANDBOX_CONTAINER}"
	docker rm -f "${SANDBOX_CONTAINER}" >/dev/null
else
	log "no container named ${SANDBOX_CONTAINER}, nothing to remove"
fi

env_file="$(sandbox_dir)/sandbox.env"
if [ -f "${env_file}" ]; then
	log "removing ${env_file}"
	rm -f "${env_file}"
else
	log "no ${env_file}, nothing to remove"
fi

log "sandbox is down"
