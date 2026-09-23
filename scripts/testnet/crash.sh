#!/usr/bin/env bash
# crash.sh [PRICE] — moves the testnet soak's oracle's XLM price, which is
# the one lever that turns deploy.sh's healthy borrower into a liquidatable
# one. PRICE is in the oracle's own units — 7 decimals, so the default
# 750000 is $0.075 against deploy.sh's $0.10, a 25% drop that takes the
# borrower's health factor from ~1.19 to ~0.89. Exactly
# scripts/sandbox/crash.sh's own scenario numbers, against our own mock
# oracle on testnet rather than the sandbox's.
#
# USDC's price is re-sent unchanged alongside it because set_price_stable
# takes the whole price vector positionally, in the `assets` order
# deploy.sh gave set_data ([XLM, USDC]) — there is no way to set one price.
# A caller that passed only XLM's would silently unprice USDC.
#
# Safe to run repeatedly, and on a running bot: the oracle is the only
# thing it touches. Signs as the pool's admin (testnet-soak-admin), the
# identity deploy.sh generated and left in this container's stellar CLI
# keystore.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/testnet/lib.sh
source "${script_dir}/lib.sh"

price=${1:-750000}
# Only a positive integer: the oracle reports i128 prices in fixed point,
# and a decimal here ("0.075") would be a silently different number, not
# an error.
case "${price}" in
"" | *[!0-9]*) die "crash: PRICE must be a positive integer in the oracle's 7-decimal units (750000 is \$0.075), got '${price}'" ;;
0*) die "crash: PRICE must be positive and unpadded, got '${price}'" ;;
esac

env_file="$(testnet_dir)/testnet.env"
[ -f "${env_file}" ] || die "crash: ${env_file} does not exist — run scripts/testnet/deploy.sh first"

# testnet.env first, and the gate on the URL it names rather than one
# reconstructed here: TESTNET_RPC_URL is what deploy.sh recorded and what
# run-bot.sh is pointed at, so it is the URL that has to answer testnet's
# passphrase.
pinned_passphrase="${TESTNET_PASSPHRASE}"
# Unset before the source, so the check below reads testnet.env's own
# values rather than the ones lib.sh already set.
unset TESTNET_RPC_URL TESTNET_PASSPHRASE
# shellcheck source=/dev/null
source "${env_file}"
for key in TESTNET_RPC_URL TESTNET_PASSPHRASE TESTNET_ORACLE TESTNET_XLM TESTNET_USDC; do
	[ -n "${!key:-}" ] || die "crash: ${env_file} does not define ${key}"
done
# The comparison passphrase stays lib.sh's own. testnet.env sets
# TESTNET_PASSPHRASE too and has just overwritten it; an env file naming a
# network the pins do not is exactly what this gate exists to refuse, so it
# is a failure rather than something to quietly adopt.
[ "${TESTNET_PASSPHRASE}" = "${pinned_passphrase}" ] \
	|| die "crash: ${env_file} names the passphrase '${TESTNET_PASSPHRASE}', not testnet's pinned '${pinned_passphrase}' — refusing to touch a network that is not testnet's own"

require_testnet_network

sandbox_register_role "${TESTNET_ORACLE}" oracle

# set_price_stable(prices: Vec<i128>) — positional, in the `assets` order
# set_data was given. lastprice(asset: Asset) -> Option<PriceData>, with
# Asset the enum {"Stellar":"C…"}.
invoke "${TESTNET_KEY_ADMIN}" "${TESTNET_ORACLE}" set_price_stable \
	--prices "[\"${price}\",\"10000000\"]" >/dev/null

answer=$(invoke_view "${TESTNET_KEY_ADMIN}" "${TESTNET_ORACLE}" lastprice \
	--asset "{\"Stellar\":\"${TESTNET_XLM}\"}")
reported=$(printf '%s' "${answer}" | jq -r '.price // empty') \
	|| die "crash: could not parse the oracle's lastprice answer: ${answer}"
[ "${reported}" = "${price}" ] \
	|| die "crash: the oracle reports XLM at ${reported:-nothing}, not the ${price} just written: ${answer}"

log "XLM is now ${price} (${answer})"
printf '%s\n' "${reported}"
