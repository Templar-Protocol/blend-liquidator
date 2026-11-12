#!/usr/bin/env bash
# unprice.sh — leaves the sandbox oracle with no price for USDC, what
# Precept's oracle adapter answers for a tripped or stale feed. It is the
# unpriced_reserve scenario's one lever; scripts/sandbox/crash.sh restores
# both prices (set_price_stable).
#
# Safe to run on a running bot: the oracle is the only thing it touches.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"

env_file="$(sandbox_dir)/sandbox.env"
[ -f "${env_file}" ] || die "unprice: ${env_file} does not exist — run scripts/sandbox/up.sh and scripts/sandbox/deploy.sh first"

# The gate runs on the URL sandbox.env names, against versions.env's
# passphrase, exactly as crash.sh does.
pinned_passphrase="${SANDBOX_PASSPHRASE}"
# shellcheck source=/dev/null
source "${env_file}"
for key in SANDBOX_RPC_URL SANDBOX_ORACLE SANDBOX_XLM SANDBOX_USDC; do
	[ -n "${!key:-}" ] || die "unprice: ${env_file} does not define ${key}"
done
[ "${SANDBOX_PASSPHRASE}" = "${pinned_passphrase}" ] \
	|| die "unprice: ${env_file} names the passphrase '${SANDBOX_PASSPHRASE}', not the sandbox's pinned '${pinned_passphrase}' — refusing to touch a network that is not this sandbox's own"

require_standalone_network "${SANDBOX_RPC_URL}"

sandbox_register_role "${SANDBOX_ORACLE}" oracle

# set_price(prices: Vec<i128>, timestamp: u64) stores `prices`
# positionally, in the `assets` order deploy.sh gave set_data ([XLM, USDC]),
# under `timestamp`, and makes `timestamp` the one lastprice reads. A
# vector holding XLM's price alone therefore leaves USDC, index 1, with no
# price at the latest timestamp: lastprice(USDC) answers None — what
# Precept's oracle adapter answers for a tripped or stale feed. The
# timestamp is the latest ledger's close time, so XLM's price is current.
# scripts/sandbox/crash.sh restores both (set_price_stable).
close_time=$(curl -fsS --max-time 10 -X POST "${SANDBOX_RPC_URL}" \
	-H 'Content-Type: application/json' \
	-d '{"jsonrpc":"2.0","id":1,"method":"getLatestLedger"}' | jq -er '.result.closeTime') \
	|| die "unprice: could not read the latest ledger's close time from ${SANDBOX_RPC_URL}"
invoke "${SANDBOX_KEY_ADMIN}" "${SANDBOX_ORACLE}" set_price \
	--prices '["1000000"]' --timestamp "${close_time}" >/dev/null

answer=$(invoke_view "${SANDBOX_KEY_ADMIN}" "${SANDBOX_ORACLE}" lastprice \
	--asset "{\"Stellar\":\"${SANDBOX_USDC}\"}")
[ "${answer}" = "null" ] || [ -z "${answer}" ] \
	|| die "unprice: the oracle still prices USDC: ${answer}"
log "USDC now has no price (XLM at 1000000, stamped ${close_time})"
