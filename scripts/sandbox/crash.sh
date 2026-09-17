#!/usr/bin/env bash
# crash.sh [PRICE] — moves the sandbox oracle's XLM price, which is the
# one lever that turns deploy.sh's healthy borrower into a liquidatable
# one. PRICE is in the oracle's own units — 7 decimals, so the default
# 750000 is $0.075 against deploy.sh's $0.10, a 25% drop that takes the
# borrower's health factor from ~1.19 to ~0.89.
#
# USDC's price is re-sent unchanged alongside it because set_price_stable
# takes the whole price vector positionally, in the `assets` order
# deploy.sh gave set_data ([XLM, USDC]) — there is no way to set one
# price. A caller that passed only XLM's would silently unprice USDC.
#
# Safe to run repeatedly, and on a running bot: the oracle is the only
# thing it touches.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"

price=${1:-750000}
# Only a positive integer: the oracle reports i128 prices in fixed point,
# and a decimal here ("0.075") would be a silently different number, not
# an error.
case "${price}" in
"" | *[!0-9]*) die "crash: PRICE must be a positive integer in the oracle's 7-decimal units (750000 is \$0.075), got '${price}'" ;;
0*) die "crash: PRICE must be positive and unpadded, got '${price}'" ;;
esac

env_file="$(sandbox_dir)/sandbox.env"
[ -f "${env_file}" ] || die "crash: ${env_file} does not exist — run scripts/sandbox/up.sh and scripts/sandbox/deploy.sh first"

# sandbox.env first, and the gate on the URL it names rather than on one
# reconstructed from SANDBOX_PORT: SANDBOX_RPC_URL is what deploy.sh
# recorded and what the bot is running against, so it is the URL that has
# to answer the standalone passphrase. The two agree today; checking the
# other one would be checking a network nothing here uses.
pinned_passphrase="${SANDBOX_PASSPHRASE}"
# shellcheck source=/dev/null
source "${env_file}"
for key in SANDBOX_RPC_URL SANDBOX_ORACLE SANDBOX_XLM SANDBOX_USDC; do
	[ -n "${!key:-}" ] || die "crash: ${env_file} does not define ${key}"
done
# The comparison passphrase stays versions.env's. sandbox.env sets
# SANDBOX_PASSPHRASE too and has just overwritten it; an env file naming a
# network the pins do not is exactly what this gate exists to refuse, so
# it is a failure rather than something to quietly adopt.
[ "${SANDBOX_PASSPHRASE}" = "${pinned_passphrase}" ] \
	|| die "crash: ${env_file} names the passphrase '${SANDBOX_PASSPHRASE}', not the sandbox's pinned '${pinned_passphrase}' — refusing to touch a network that is not this sandbox's own"

require_standalone_network "${SANDBOX_RPC_URL}"

sandbox_register_role "${SANDBOX_ORACLE}" oracle

# set_price_stable(prices: Vec<i128>) — positional, in the `assets` order
# set_data was given. lastprice(asset: Asset) -> Option<PriceData>, with
# Asset the enum {"Stellar":"C…"}.
invoke "${SANDBOX_KEY_ADMIN}" "${SANDBOX_ORACLE}" set_price_stable \
	--prices "[\"${price}\",\"10000000\"]" >/dev/null

answer=$(invoke_view "${SANDBOX_KEY_ADMIN}" "${SANDBOX_ORACLE}" lastprice \
	--asset "{\"Stellar\":\"${SANDBOX_XLM}\"}")
reported=$(printf '%s' "${answer}" | jq -r '.price // empty') \
	|| die "crash: could not parse the oracle's lastprice answer: ${answer}"
[ "${reported}" = "${price}" ] \
	|| die "crash: the oracle reports XLM at ${reported:-nothing}, not the ${price} just written: ${answer}"

log "XLM is now ${price} (${answer})"
printf '%s\n' "${reported}"
