#!/usr/bin/env bash
# test-network-pinning.sh — the guard on the one thing the whole tier rests
# on: every `stellar` call a sandbox script makes must take its network
# from lib.sh's own flags, never from the environment.
#
# The CLI resolves an ad-hoc network from STELLAR_RPC_URL and
# STELLAR_NETWORK_PASSPHRASE *ahead of* an explicit `--network`, so a
# sandbox that named a network would generate keys, fund them, deploy and
# sign on whatever an operator happened to have exported — while
# require_standalone_network went on confirming localhost, which is not
# the thing those calls were using. lib.sh answers that twice: it unsets
# the CLI's network and signing variables at source time, and every call
# passes "${sandbox_network_args[@]}" — the URL that was verified and the
# passphrase it was verified by. This is what proves the second half.
#
# Offline, and it needs no node: `stellar contract id wasm` derives a
# contract id from the source account, the salt and the **network
# passphrase** alone, which makes the id a cheap deterministic fingerprint
# of the network the CLI thinks it is on. The only requirement is the CLI
# itself. Run by hand and by .github/workflows/sandbox.yml, before it
# starts a network.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/sandbox/lib.sh
source "${script_dir}/lib.sh"
# shellcheck source=scripts/sandbox/versions.env
source "${VERSIONS_ENV:-${script_dir}/versions.env}"

command -v stellar >/dev/null 2>&1 \
	|| die "test-network-pinning: the stellar CLI is not on PATH — .devcontainer/post-create.sh and .github/workflows/sandbox.yml install it"

# Never contacted: the derivation below is arithmetic over the account, the
# salt and the passphrase. Port 1 rather than the sandbox's own, so a
# future call that did reach for a node fails here instead of quietly
# borrowing a sandbox somebody left running.
sandbox_set_network "http://127.0.0.1:1/rpc"

# Both are inputs to the derived id, so all three derivations below must
# use the same pair for the comparison to mean anything. The account is a
# literal G… address (the same one the crate's unit tests use), never an
# identity name, so this test needs no keystore.
SALT=0000000000000000000000000000000000000000000000000000000000000001
ACCOUNT=GDAWX4KV5EQLP5W44HE5AA5QN5QRBJOVQIAI5OXOH5FW2ENT5PXN33DE

# The public network a Soroban developer's shell profile plausibly points
# at — the CLI's own documented variables — and the one no sandbox call may
# follow.
POLLUTED_RPC_URL=https://soroban-testnet.stellar.org
POLLUTED_PASSPHRASE="Test SDF Network ; September 2015"

pass=0
fail=0

ok() {
	printf 'ok - %s\n' "$1"
	pass=$((pass + 1))
}

no() {
	printf 'FAIL - %s\n' "$1"
	fail=$((fail + 1))
}

# derive — the contract id the CLI derives through the sandbox's own flag
# array. Whichever network it resolved is in the answer.
derive() {
	stellar contract id wasm "${sandbox_network_args[@]}" \
		--salt "${SALT}" --source-account "${ACCOUNT}"
}

pinned=$(derive) || die "test-network-pinning: the derivation in a clean environment failed"
[ -n "${pinned}" ] || die "test-network-pinning: the derivation in a clean environment printed nothing"
printf 'the sandbox flags derive %s\n' "${pinned}"

# The same call with the CLI's network variables exported. They are
# exported inside the command substitution's subshell, so nothing leaks
# into the derivations either side — and they are set here rather than
# inherited, because sourcing lib.sh unset them.
polluted=$(
	export STELLAR_RPC_URL="${POLLUTED_RPC_URL}"
	export STELLAR_NETWORK_PASSPHRASE="${POLLUTED_PASSPHRASE}"
	export STELLAR_NETWORK=testnet
	derive
) || die "test-network-pinning: the derivation under the polluted environment failed"

if [ "${polluted}" = "${pinned}" ]; then
	ok "the flags beat STELLAR_RPC_URL, STELLAR_NETWORK_PASSPHRASE and STELLAR_NETWORK"
else
	no "the environment moved the network: the flags alone derive ${pinned}, with those variables exported ${polluted}"
fi

# The negative control, in a clean environment: a different network must
# derive a different id, or the assertion above would hold however
# thoroughly the flags were ignored.
named=$(stellar contract id wasm --network testnet --salt "${SALT}" --source-account "${ACCOUNT}") \
	|| die "test-network-pinning: deriving against testnet failed"
if [ "${named}" != "${pinned}" ]; then
	ok "a different network derives a different id (testnet: ${named}), so this test can fail"
else
	no "testnet derived the same id ${named} — the comparison above proves nothing"
fi

printf '%d passed, %d failed\n' "${pass}" "${fail}"
[ "${fail}" -eq 0 ]
