#!/usr/bin/env bash
# mint.sh AMOUNT — mints AMOUNT (a positive, unpadded integer of USDC
# stroops — USDC has 7 decimals, so 10000000 is 1 USDC) of the sandbox's
# USDC to the filler, signing as the issuer — the same account deploy.sh's
# own MINT_USDC_FILLER mint uses.
#
# Exists for the one scenario that deploys with none: unwind_repay's
# filler reaches its fill with no USDC of its own, on purpose, so the
# fill's repay leaves debt behind for the unwind pass to notice. This is
# how that scenario funds the wallet afterwards, once whatever it means to
# prove with the debt still outstanding has already happened.
#
# Safe to run repeatedly, and on a running bot: it only ever adds to the
# filler's balance.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"

amount=${1:-}
# Only a positive integer: the SAC reports i128 balances in fixed point
# (USDC's own 7 decimals), and a decimal here ("1.0") would silently mint
# a different amount, not an error.
case "${amount}" in
"" | *[!0-9]*) die "mint: AMOUNT must be a positive integer of USDC stroops, got '${amount}'" ;;
0*) die "mint: AMOUNT must be positive and unpadded, got '${amount}'" ;;
esac

env_file="$(sandbox_dir)/sandbox.env"
[ -f "${env_file}" ] || die "mint: ${env_file} does not exist — run scripts/sandbox/up.sh and scripts/sandbox/deploy.sh first"

# sandbox.env first, and the gate on the URL it names rather than one
# reconstructed from SANDBOX_PORT: SANDBOX_RPC_URL is what deploy.sh
# recorded and what the bot is running against, so it is the URL that has
# to answer the standalone passphrase. The two agree today; checking the
# other one would be checking a network nothing here uses.
pinned_passphrase="${SANDBOX_PASSPHRASE}"
# shellcheck source=/dev/null
source "${env_file}"
for key in SANDBOX_RPC_URL SANDBOX_USDC SANDBOX_FILLER; do
	[ -n "${!key:-}" ] || die "mint: ${env_file} does not define ${key}"
done
# The comparison passphrase stays versions.env's. sandbox.env sets
# SANDBOX_PASSPHRASE too and has just overwritten it; an env file naming a
# network the pins do not is exactly what this gate exists to refuse, so
# it is a failure rather than something to quietly adopt.
[ "${SANDBOX_PASSPHRASE}" = "${pinned_passphrase}" ] \
	|| die "mint: ${env_file} names the passphrase '${SANDBOX_PASSPHRASE}', not the sandbox's pinned '${pinned_passphrase}' — refusing to touch a network that is not this sandbox's own"

require_standalone_network "${SANDBOX_RPC_URL}"

sandbox_register_role "${SANDBOX_USDC}" usdc

# balance(id: Address) -> i128 — SEP-41's own token interface, which the
# Stellar Asset Contract implements; confirmed against this exact USDC SAC
# with `stellar contract info interface --id <SANDBOX_USDC> --rpc-url …
# --network-passphrase …`. The CLI prints an i128 return the same
# quoted-JSON-string way it prints an Address (see deploy.sh's step 6), so
# jq's bare `.` unwraps either.
before_answer=$(invoke_view "${SANDBOX_KEY_ISSUER}" "${SANDBOX_USDC}" balance --id "${SANDBOX_FILLER}")
before=$(printf '%s' "${before_answer}" | jq -r '.') \
	|| die "mint: could not parse the filler's balance before minting: ${before_answer}"

# mint(to: Address, amount: i128) — the same call deploy.sh's own
# MINT_USDC_FILLER makes, signed by the issuer: the only account able to
# mint a classic asset's SAC.
invoke "${SANDBOX_KEY_ISSUER}" "${SANDBOX_USDC}" mint --to "${SANDBOX_FILLER}" --amount "${amount}" >/dev/null

after_answer=$(invoke_view "${SANDBOX_KEY_ISSUER}" "${SANDBOX_USDC}" balance --id "${SANDBOX_FILLER}")
after=$(printf '%s' "${after_answer}" | jq -r '.') \
	|| die "mint: could not parse the filler's balance after minting: ${after_answer}"

expected=$((before + amount))
[ "${after}" = "${expected}" ] \
	|| die "mint: the filler's USDC balance is ${after}, expected ${before} + ${amount} = ${expected} (before: ${before_answer}, after: ${after_answer})"

log "the filler's USDC balance is now ${after} (was ${before})"
printf '%s\n' "${after}"
